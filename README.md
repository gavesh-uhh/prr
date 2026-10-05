# prr

Reviews a GitHub pull request with an OpenAI-compatible chat completions endpoint
and posts the findings you accept as one PR review.

## Install

```sh
cargo build --release      # -> target/release/prr
```

## Config

| Variable | Required | Default |
| --- | --- | --- |
| `OPENAI_API_KEY` | yes | - |
| `GITHUB_TOKEN` | yes | - |
| `OPENAI_BASE_URL` | no | `https://api.openai.com/v1` |
| `OPENAI_MODEL` | no | `gpt-4o` |

The token needs read access to the PR and permission to post a review.

## Usage

```sh
prr owner/repo#123            # TUI
prr owner/repo#123 --no-post  # TUI, analyse only: posting is disabled
prr owner/repo#123 --dry-run  # TUI, post prints the payload instead of sending
prr owner/repo#123 --json     # headless, findings as JSON on stdout
```

`--no-post` and `--json` can never reach GitHub's review endpoint. Use
`--no-post` to read findings in the TUI with the post path switched off, or
`--dry-run` to have `p` `y` print the exact payload it would have sent.

Findings look like this; progress goes to stderr.

```json
[{"path": "src/auth.rs", "line": 88, "severity": "high", "body": "Expiry is checked after the comparison."}]
```

## Keys

| Key | Action |
| --- | --- |
| `j` `k` | select file |
| `J` `K` | scroll diff |
| `r` | start a run |
| `n` `N` | next / previous finding |
| `space` | accept finding |
| `p` `y` | post accepted findings (disabled under `--no-post`) |
| `q` | quit |

## Architecture

```text
main --+--> tui --(r)--> [thread] agent --+--> llm     (chat/completions)
       |                mpsc::Receiver     +--> github  (api.github.com)
       +--json--------------------------> agent
```

Blocking `reqwest`, no async runtime. `agent` gives the model `read_file`,
`list_files`, `add_comment` and `finish`, and caps the run at 10 steps.

## Evals

Ten hand-planted bugs, one per case in `evals/cases.json`. Fill in the
`owner/repo#N` refs first; the runner refuses to start on placeholders.

```powershell
cargo build --release
.\evals\run.ps1              # Windows, no extra tooling
```

```sh
cargo build --release
evals/run.sh                 # POSIX, needs jq
```

A case passes when a finding matches on `path` and `line` within +/-2. Both
runners print `caught X/10` plus the false-positive count and are otherwise
identical in behaviour.

## Limitations

- Max 10 agent steps per run.
- The model sees the diff plus what it fetches; `read_file` is truncated to
  30000 chars, so large files arrive partial.
- Reviews are always `event: COMMENT` - never `REQUEST_CHANGES` or `APPROVE`.
- Skipped: `*.lock`, `package-lock.json`, `*.min.js`, `dist/`, `vendor/`, files
  with no patch, and patches over 20000 chars.
- Only OpenAI-style chat completions with tool calling.
- Line numbers are diff-validated, but severity and wording are the model's own
  judgement and are not checked.
- No pagination: at most 100 files per PR.
- `--json` runs the agent on the main thread, so it looks idle until it prints.
- No cost accounting, no caching between runs.