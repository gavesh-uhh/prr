#!/usr/bin/env bash
#
# prr eval harness -- measures how many planted bugs prr actually reports.
#
# Usage
#   1. Build the release binary:
#          cargo build --release
#   2. Fill in evals/cases.json. Every "owner/repo#1" value is a PLACEHOLDER:
#      replace the owner/repo part and the PR number with a real pull request
#      that contains the planted bug named in the "bug" key. This script
#      refuses to run while any placeholder is left.
#   3. Export credentials (prr needs both to talk to GitHub and the model):
#          export OPENAI_API_KEY=sk-...
#          export GITHUB_TOKEN=ghp_...
#      Optional: OPENAI_BASE_URL (default https://api.openai.com/v1),
#      OPENAI_MODEL (default gpt-4o).
#   4. Run it from anywhere -- paths are resolved from this script's location:
#          evals/run.sh
#
# Optional environment variables:
#   PRR_BIN    path to the prr binary  (default: <repo root>/target/release/prr)
#   PRR_CASES  path to the cases file  (default: <this dir>/cases.json)
#
# What it does per case
#   Runs:  prr <owner/repo#N> --json      from the repo root
#   prr prints a pretty-printed JSON *array* of findings on stdout and sends
#   all progress/log lines to stderr, so stderr is discarded here. Each finding
#   is { "path": ..., "line": ..., "severity": ..., "body": ... }.
#
# Scoring
#   * A case PASSES when some finding has the same .path as the expected file
#     and |finding.line - expected.line| <= 2.
#   * A finding that matches no case of its own PR is a false positive. A
#     finding that does match a case is attributed to that case, so it is never
#     charged as a false positive as well.
#
# PASS and FAIL are both normal outcomes: this is a measurement, not a gate.
# The exit status is non-zero only for setup problems (missing jq, missing or
# non-executable binary, unfilled placeholders, unreadable cases file).
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PRR_BIN="${PRR_BIN:-$ROOT/target/release/prr}"
CASES_FILE="${PRR_CASES:-$SCRIPT_DIR/cases.json}"

die() {
  printf '%s\n' "$*" >&2
  exit 1
}

# ---- setup checks ---------------------------------------------------------

if [ ! -x "$PRR_BIN" ]; then
  printf 'ERROR: prr binary is missing or not executable: %s\n' "$PRR_BIN" >&2
  printf 'hint: build it with:   (cd "%s" && cargo build --release)\n' "$ROOT" >&2
  printf 'hint: or point at an existing binary:  PRR_BIN=/path/to/prr\n' >&2
  exit 1
fi

command -v jq >/dev/null 2>&1 || die "ERROR: jq is required (https://jqlang.github.io/jq/download/)"

[ -f "$CASES_FILE" ] || die "ERROR: cases file not found: $CASES_FILE"

if ! jq -e 'type == "array"' "$CASES_FILE" >/dev/null 2>&1; then
  die "ERROR: $CASES_FILE is not valid JSON, or is not a JSON array"
fi

if ! jq -e 'all(.[]; ((.pr | type) == "string") and ((.file | type) == "string") and ((.line | type) == "number") and ((.bug | type) == "string"))' "$CASES_FILE" >/dev/null 2>&1; then
  die "ERROR: $CASES_FILE must contain objects with string .pr, .file and .bug plus a numeric .line"
fi

TOTAL_CASES="$(jq -r 'length' "$CASES_FILE")"

# ---- unfilled placeholder detection --------------------------------------

N_PLACEHOLDERS="$(jq -r '[.[] | select(((.pr // "") | startswith("owner/repo")))] | length' "$CASES_FILE")"
if [ "$N_PLACEHOLDERS" -gt 0 ]; then
  printf 'ERROR: %s still contains placeholders -- fill in owner/repo#N for each case.\n' "$CASES_FILE" >&2
  printf '       Every ".pr" of the form "owner/repo#N" is a placeholder, not a real pull request.\n' >&2
  printf '       %s of %s case(s) are still unfilled:\n' "$N_PLACEHOLDERS" "$TOTAL_CASES" >&2
  jq -r 'to_entries[]
         | select((.value.pr // "") | startswith("owner/repo"))
         | "         case \(.key + 1)  \(.value.bug)  \(.value.pr)  (expected \(.value.file):\(.value.line))"' \
    "$CASES_FILE"
  exit 1
fi

# ---- scratch space for per-run records (never inside the project) ----------

TMP_RUNS="$(mktemp -d 2>/dev/null)" || TMP_RUNS=""
if [ -z "$TMP_RUNS" ] || [ ! -d "$TMP_RUNS" ]; then
  TMP_RUNS="${TMPDIR:-/tmp}/prr-evals-$$"
  mkdir -p "$TMP_RUNS" || die "ERROR: could not create a temporary directory"
fi
trap 'rm -rf "$TMP_RUNS"' EXIT INT TERM
RUNS_FILE="$TMP_RUNS/runs.jsonl"
: > "$RUNS_FILE"

# ---- run every case -------------------------------------------------------

TOTAL=0
CAUGHT=0
MISSED=""

printf 'prr evals: %s case(s)\n' "$TOTAL_CASES"
printf 'binary: %s\n\n' "$PRR_BIN"

CASES_TSV="$(jq -r '.[] | [.pr, .file, (.line | tostring), .bug] | @tsv' "$CASES_FILE")"

while IFS=$'\t' read -r pr file line bug; do
  [ -n "${pr:-}" ] || continue
  TOTAL=$((TOTAL + 1))
  IDX="$TOTAL"

  OUT="$(cd "$ROOT" && "$PRR_BIN" "$pr" --json 2>/dev/null)"
  RC=$?

  if ! printf '%s' "$OUT" | jq -e . >/dev/null 2>&1; then
    if [ "$RC" -ne 0 ]; then
      REASON="prr exited $RC with no JSON array on stdout"
    else
      REASON="prr stdout was not a valid JSON array"
    fi
    printf 'FAIL  case %-3s %-24s %-22s expected %s:%s -- %s\n' \
      "$IDX" "$bug" "$pr" "$file" "$line" "$REASON"
    MISSED="${MISSED}${bug}"$'\n'
    jq -c -n --arg pr "$pr" --arg file "$file" --argjson line "$line" --arg bug "$bug" \
      '{case:{pr:$pr,file:$file,line:$line,bug:$bug},findings:[]}' >> "$RUNS_FILE"
    continue
  fi

  MATCHES="$(printf '%s' "$OUT" | jq --arg p "$file" --argjson e "$line" '
    def absdelta: ((. - $e) | if . < 0 then (0 - .) else . end);
    [.[] | select((.path == $p) and ((.line | absdelta) <= 2))] | length')"
  case "$MATCHES" in
    '' | *[!0-9]*) MATCHES=0 ;;
  esac

  jq -c -n --arg pr "$pr" --arg file "$file" --argjson line "$line" --arg bug "$bug" \
    --argjson findings "$OUT" \
    '{case:{pr:$pr,file:$file,line:$line,bug:$bug},findings:$findings}' >> "$RUNS_FILE"

  if [ "$MATCHES" -gt 0 ]; then
    CAUGHT=$((CAUGHT + 1))
    printf 'PASS  case %-3s %-24s %-22s expected %s:%s\n' \
      "$IDX" "$bug" "$pr" "$file" "$line"
  else
    NEAREST="$(printf '%s' "$OUT" | jq -r --arg p "$file" --argjson e "$line" '
      def absdelta: ((. - $e) | if . < 0 then (0 - .) else . end);
      [.[] | {p: .path, l: .line, same: (.path == $p), d: (.line | absdelta)}] as $all
      | ([ $all[] | select(.same) ] | if length > 0 then . else $all end)
      | if length == 0 then "none"
        else (sort_by(.d) | .[0] | "\(.p):\(.l)")
        end')"
    [ -n "$NEAREST" ] || NEAREST="none"
    printf 'FAIL  case %-3s %-24s %-22s expected %s:%s -- nearest finding: %s\n' \
      "$IDX" "$bug" "$pr" "$file" "$line" "$NEAREST"
    MISSED="${MISSED}${bug}"$'\n'
  fi
done <<EOF
$CASES_TSV
EOF

# ---- false positives ------------------------------------------------------
# Group the recorded runs by PR, so a finding is only ever compared against the
# cases that belong to the same pull request. A finding matching no case of its
# own PR is a false positive; one that does match is attributed to that case.

FALSE_POSITIVES="$(jq -s '
  def near($a; $b): (($a - $b) | if . < 0 then (0 - .) else . end) <= 2;
  def matched($cases): . as $f | any($cases[]; ((.file == $f.path) and near($f.line; .line)));
  [ .[] ]
  | group_by(.case.pr)
  | map(
      . as $g
      | ($g | map(.case)) as $cs
      | ($g | map(.findings) | (add // []) | unique_by([.path, .line])) as $fs
      | ($fs | map(select(matched($cs) | not)) | length)
    )
  | (add) // 0
' "$RUNS_FILE")"
case "$FALSE_POSITIVES" in
  '' | *[!0-9]*) FALSE_POSITIVES=0 ;;
esac

# ---- summary --------------------------------------------------------------

printf '\ncaught %s/%s\n' "$CAUGHT" "$TOTAL"
printf 'false positives: %s\n' "$FALSE_POSITIVES"
if [ -z "$MISSED" ]; then
  printf '  missed: none\n'
else
  printf '%s' "$MISSED" | while IFS= read -r missed_bug; do
    [ -n "$missed_bug" ] && printf '  missed: %s\n' "$missed_bug"
  done
fi

exit 0