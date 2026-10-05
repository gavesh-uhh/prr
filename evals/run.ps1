<#
.SYNOPSIS
    Scores prr against the hand-planted bugs in cases.json. Native Windows
    equivalent of evals/run.sh, for people without a POSIX shell or jq.

.DESCRIPTION
    For each case runs:  prr <owner/repo#N> --json
    prr prints a findings array on stdout and all progress on stderr.

    A case PASSES when some finding has the same .path and is within +/-2 of
    the expected .line. A finding that matches no case of its own PR is a
    false positive; one that does match is attributed to that case, so it is
    never double counted.

    PASS and FAIL are both normal outcomes: this measures, it does not gate.
    Exits non-zero only for setup problems (missing binary, unfilled
    placeholders, unreadable cases file).

.PARAMETER PrrBin
    Path to the prr binary. Defaults to target\release\prr.exe under the repo.
.PARAMETER CasesFile
    Path to cases.json. Defaults to evals\cases.json next to this script.
#>
[CmdletBinding()]
param(
    [string]$PrrBin,
    [string]$CasesFile
)

$ErrorActionPreference = 'Stop'
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = (Resolve-Path (Join-Path $scriptDir '..')).Path

if (-not $PrrBin) { $PrrBin = $env:PRR_BIN }
if (-not $PrrBin) {
    foreach ($n in 'prr.exe', 'prr') {
        $c = Join-Path $root "target\release\$n"
        if (Test-Path $c) { $PrrBin = $c; break }
    }
}
if (-not $CasesFile) { $CasesFile = $env:PRR_CASES }
if (-not $CasesFile) { $CasesFile = Join-Path $scriptDir 'cases.json' }

# ---- setup checks ----------------------------------------------------------

if (-not $PrrBin -or -not (Test-Path $PrrBin)) {
    Write-Host "ERROR: prr binary not found: $PrrBin" -ForegroundColor Red
    Write-Host "hint: build it with:   (cd `"$root`"; cargo build --release)"
    Write-Host "hint: or pass -PrrBin / set PRR_BIN (an absolute path is safest)"
    exit 1
}
if (-not (Test-Path $CasesFile)) {
    Write-Host "ERROR: cases file not found: $CasesFile" -ForegroundColor Red
    exit 1
}

try {
    $parsed = Get-Content -Raw -LiteralPath $CasesFile | ConvertFrom-Json -ErrorAction Stop
    # Wrap the variable, not the command output: Windows PowerShell 5.1 emits a
    # JSON array from ConvertFrom-Json without enumerating it, so `@(...)`
    # around the call would capture a single element (the array itself).
    $cases = @($parsed)
} catch {
    Write-Host "ERROR: $CasesFile is not valid JSON: $_" -ForegroundColor Red
    exit 1
}
if ($cases.Count -eq 0) { Write-Host 'ERROR: cases file is empty' -ForegroundColor Red; exit 1 }
foreach ($c in $cases) {
    $lineIsNumber = $c.line -is [int] -or $c.line -is [long] -or $c.line -is [double]
    if (-not ($c.pr -is [string]) -or -not ($c.file -is [string]) -or
        -not ($c.bug -is [string]) -or -not $lineIsNumber) {
        Write-Host 'ERROR: each case needs string .pr, .file, .bug and numeric .line' -ForegroundColor Red
        exit 1
    }
}

$unfilled = @($cases | Where-Object { $_.pr -like 'owner/repo*' })
if ($unfilled.Count -gt 0) {
    Write-Host "ERROR: $CasesFile still contains placeholders -- fill in owner/repo#N for each case." -ForegroundColor Red
    Write-Host "       Every `"pr`" of the form `"owner/repo#N`" is a placeholder, not a real pull request."
    Write-Host "       $($unfilled.Count) of $($cases.Count) case(s) are still unfilled:"
    $i = 0
    foreach ($c in $unfilled) {
        $i++
        Write-Host "         case $i  $($c.bug)  $($c.pr)  (expected $($c.file):$($c.line))"
    }
    exit 1
}

# ---- helpers ---------------------------------------------------------------

function Invoke-Prr {
    # Runs prr and returns @{ Out; Code }. Both streams are drained
    # asynchronously so a chatty stderr can never deadlock the child.
    param([string]$Ref)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $PrrBin
    $psi.Arguments = '"' + $Ref + '" --json'
    $psi.WorkingDirectory = $root
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $so = $p.StandardOutput.ReadToEndAsync()
    $null = $p.StandardError.ReadToEndAsync()
    $p.WaitForExit()
    [pscustomobject]@{ Out = $so.Result; Code = $p.ExitCode }
}

function Get-Findings {
    # Returns the parsed array, or $null when stdout is not a findings array.
    # The comma in `,@(...)` is load bearing: a plain `return @()` unrolls an
    # empty array to $null and an empty result would read as "invalid JSON".
    param([string]$Text)
    $t = $Text.Trim()
    if (-not $t.StartsWith('[')) { return $null }
    try { $parsed = ConvertFrom-Json -InputObject $t -ErrorAction Stop } catch { return $null }
    , @($parsed)
}

function Test-Hit {
    param($Findings, [string]$Path, [int]$Line)
    foreach ($f in $Findings) {
        if ($f.path -eq $Path -and [math]::Abs($f.line - $Line) -le 2) { return $true }
    }
    return $false
}

function Get-Nearest {
    # Prefer the expected path when the model flagged the right file at the
    # wrong line; otherwise fall back to the closest line across all files.
    param($Findings, [string]$Path, [int]$Line)
    if ($Findings.Count -eq 0) { return 'none' }
    $same = @($Findings | Where-Object { $_.path -eq $Path })
    $pool = if ($same.Count -gt 0) { $same } else { $Findings }
    $best = $pool[0]
    $bestD = [math]::Abs($best.line - $Line)
    foreach ($f in $pool) {
        $d = [math]::Abs($f.line - $Line)
        if ($d -lt $bestD) { $best = $f; $bestD = $d }
    }
    "$($best.path):$($best.line)"
}

# ---- run every case --------------------------------------------------------

Write-Host "prr evals: $($cases.Count) case(s)"
Write-Host "binary: $PrrBin"
Write-Host ''

$runs = @()
$caught = 0
$missed = @()
$idx = 0

foreach ($c in $cases) {
    $idx++
    $r = Invoke-Prr -Ref $c.pr
    $findings = Get-Findings -Text $r.Out

    if ($null -eq $findings) {
        $reason = if ($r.Code -ne 0) { "prr exited $($r.Code) with no JSON array on stdout" }
                  else { 'prr stdout was not a valid JSON array' }
        Write-Host ("FAIL  case {0,-3} {1,-24} {2,-22} expected {3}:{4} -- {5}" -f $idx, $c.bug, $c.pr, $c.file, $c.line, $reason) -ForegroundColor Yellow
        $missed += $c.bug
        $runs += [pscustomobject]@{ Pr = $c.pr; Findings = @() }
        continue
    }

    if (Test-Hit -Findings $findings -Path $c.file -Line $c.line) {
        $caught++
        Write-Host ("PASS  case {0,-3} {1,-24} {2,-22} expected {3}:{4}" -f $idx, $c.bug, $c.pr, $c.file, $c.line) -ForegroundColor Green
    } else {
        Write-Host ("FAIL  case {0,-3} {1,-24} {2,-22} expected {3}:{4} -- nearest finding: {5}" -f $idx, $c.bug, $c.pr, $c.file, $c.line, (Get-Nearest -Findings $findings -Path $c.file -Line $c.line)) -ForegroundColor Yellow
        $missed += $c.bug
    }
    $runs += [pscustomobject]@{ Pr = $c.pr; Findings = $findings }
}

# ---- false positives -------------------------------------------------------
# Compared per PR so a finding is only ever judged against the cases that
# could plausibly contain it. Identical findings from repeated runs are
# counted once.

$falsePositives = 0
foreach ($grp in ($runs | Group-Object Pr)) {
    $groupCases = @($cases | Where-Object { $_.pr -eq $grp.Name })
    $seen = @{}
    foreach ($run in $grp.Group) {
        foreach ($f in $run.Findings) {
            $key = "$($f.path)|$($f.line)"
            if ($seen.ContainsKey($key)) { continue }
            $seen[$key] = $true
            $matched = $false
            foreach ($c in $groupCases) {
                if ($f.path -eq $c.file -and [math]::Abs($f.line - $c.line) -le 2) { $matched = $true; break }
            }
            if (-not $matched) { $falsePositives++ }
        }
    }
}

# ---- summary ---------------------------------------------------------------

Write-Host ''
Write-Host "caught $caught/$($cases.Count)"
Write-Host "false positives: $falsePositives"
if ($missed.Count -eq 0) { Write-Host '  missed: none' }
else { $missed | ForEach-Object { Write-Host "  missed: $_" } }

exit 0