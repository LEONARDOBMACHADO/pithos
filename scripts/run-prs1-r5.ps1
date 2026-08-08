#requires -Version 5.1
param(
    [string]$Corpus = 'tst_compact',
    [string]$ExternalEvidenceRoot = '',
    [int]$PhaseMaxTotalMiB = 2048
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repo
$branch = (& git branch --show-current).Trim()
$sha = (& git rev-parse HEAD).Trim()
if ($branch -ne 'feat/31-representation-substrate') {
    throw "PRS1 R5 must run from feat/31-representation-substrate; current=$branch"
}

$allowedLocal = @(
    'docs/gates/GATE_A_EVIDENCE.md',
    'docs/gates/GATE_B_EVIDENCE.md'
)
function Get-UnexpectedStatus([switch]$AllowCargoLock) {
    $rows = @(& git status --porcelain)
    return @($rows | Where-Object {
        if ([string]::IsNullOrWhiteSpace($_)) { return $false }
        $path = if ($_.Length -gt 3) { $_.Substring(3).Replace('\','/') } else { $_ }
        if ($allowedLocal -contains $path) { return $false }
        if ($AllowCargoLock -and $path -eq 'Cargo.lock') { return $false }
        return $true
    })
}

function Stop-WithLog([string]$Message, [string]$Path) {
    Write-Host "`n$Message" -ForegroundColor Red
    if (Test-Path -LiteralPath $Path) {
        Write-Host "`n===== COMPLETE LOG =====" -ForegroundColor Yellow
        Get-Content -LiteralPath $Path
    }
    throw $Message
}

$unexpected = @(Get-UnexpectedStatus)
if ($unexpected.Count -gt 0) {
    $unexpected | ForEach-Object { Write-Host $_ -ForegroundColor Red }
    throw 'Unexpected local changes before PRS1 R5.'
}

# A new workspace crate must be represented by a Cargo-generated lockfile before
# any benchmark is accepted. Never benchmark an uncommitted dependency graph.
$lockHasSubstrate = Select-String -LiteralPath (Join-Path $repo 'Cargo.lock') `
    -SimpleMatch 'name = "pithos-representation-substrate"' -Quiet
if (-not $lockHasSubstrate) {
    Write-Host '=== PRS1 R5 LOCKFILE PREPARATION ===' -ForegroundColor Yellow
    & cargo generate-lockfile
    if ($LASTEXITCODE -ne 0) { throw 'cargo generate-lockfile failed' }

    $unexpectedAfterLock = @(Get-UnexpectedStatus -AllowCargoLock)
    if ($unexpectedAfterLock.Count -gt 0) {
        $unexpectedAfterLock | ForEach-Object { Write-Host $_ -ForegroundColor Red }
        throw 'Lockfile generation changed files other than Cargo.lock.'
    }
    $status = @(& git status --porcelain)
    $cargoLockChanged = @($status | Where-Object {
        $_.Length -gt 3 -and $_.Substring(3).Replace('\','/') -eq 'Cargo.lock'
    }).Count -eq 1
    if (-not $cargoLockChanged) {
        throw 'Cargo.lock did not change as expected after adding PRS1 workspace crate.'
    }

    Write-Host "`n===== CARGO.LOCK GENERATED DIFF =====" -ForegroundColor Cyan
    & git diff -- Cargo.lock
    Write-Host "`nSTOP: commit the Cargo-generated lockfile upstream before R5 benchmark." -ForegroundColor Yellow
    Write-Host 'Do not edit Cargo.lock manually. Return this diff and git status.'
    exit 23
}

# From this point onward the dependency graph must already be committed and
# reproducible. --locked prevents Cargo from silently repairing it during gates.
Write-Host '=== PRS1 R5 LOCKFILE REPRODUCIBILITY ===' -ForegroundColor Cyan
& cargo metadata --locked --format-version 1 --no-deps | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'cargo metadata --locked failed; Cargo.lock is stale or inconsistent.'
}

$gateRoot = if ([string]::IsNullOrWhiteSpace($ExternalEvidenceRoot)) {
    Join-Path $env:TEMP 'pithos-prs1-r5-gates'
} else {
    $ExternalEvidenceRoot
}
New-Item -ItemType Directory -Force -Path $gateRoot | Out-Null
$gateTimestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$clippyMapLog = Join-Path $gateRoot "prs1-r5-clippy-map-$gateTimestamp.log"
$strictLog = Join-Path $gateRoot "prs1-r5-clippy-strict-$gateTimestamp.log"

Write-Host "`n=== PRS1 R5 STATIC GATE 1/5: RUSTFMT ===" -ForegroundColor Cyan
& cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) {
    throw 'cargo fmt --all -- --check failed. Run cargo fmt --all in a dedicated source-fix commit; do not benchmark.'
}

# Run a complete non-strict workspace Clippy FIRST. This deliberately discovers
# the full warning surface in one pass so R5 cannot repeat the R4 cycle of fixing
# one compiler blocker only to reveal the next warning on the following run.
Write-Host "`n=== PRS1 R5 STATIC GATE 2/5: COMPLETE CLIPPY WARNING MAP ===" -ForegroundColor Cyan
& cargo clippy --workspace --all-targets 2>&1 | Tee-Object -FilePath $clippyMapLog
$clippyMapExit = $LASTEXITCODE
if ($clippyMapExit -ne 0) {
    Stop-WithLog 'Workspace Clippy map failed to compile. STOP before tests/benchmark.' $clippyMapLog
}

$warningLines = @(Select-String -LiteralPath $clippyMapLog -Pattern '(^|\s)warning:' -CaseSensitive:$false)
if ($warningLines.Count -gt 0) {
    Write-Host "`nDetected $($warningLines.Count) warning line(s)." -ForegroundColor Red
    Write-Host 'The complete map is printed so all warnings can be repaired in one source pass.' -ForegroundColor Yellow
    Stop-WithLog 'Workspace is not warning-clean. STOP before strict Clippy/tests/benchmark.' $clippyMapLog
}
Write-Host 'COMPLETE CLIPPY WARNING MAP: 0 warnings' -ForegroundColor Green

Write-Host "`n=== PRS1 R5 STATIC GATE 3/5: STRICT CLIPPY ===" -ForegroundColor Cyan
& cargo clippy --workspace --all-targets -- -D warnings 2>&1 | Tee-Object -FilePath $strictLog
if ($LASTEXITCODE -ne 0) {
    Stop-WithLog 'Workspace strict Clippy failed. STOP before tests/benchmark.' $strictLog
}
Write-Host 'STRICT CLIPPY PASS' -ForegroundColor Green

Write-Host "`n=== PRS1 R5 STATIC GATE 4/5: TARGETED ROUNDTRIP TESTS ===" -ForegroundColor Cyan
& cargo test -p pithos-representation-substrate -p pithos-native-codec-v18
if ($LASTEXITCODE -ne 0) {
    throw 'PRS1/native-v18 tests failed.'
}

Write-Host "`n=== PRS1 R5 STATIC GATE 5/5: WORKSPACE REGRESSION + RELEASE ===" -ForegroundColor Cyan
& cargo test --workspace
if ($LASTEXITCODE -ne 0) {
    throw 'workspace tests failed.'
}

& cargo build --release -p pithos-cli
if ($LASTEXITCODE -ne 0) {
    throw 'release CLI build failed.'
}

& git diff --check
if ($LASTEXITCODE -ne 0) {
    throw 'git diff --check failed after gates.'
}

$shaAfterGates = (& git rev-parse HEAD).Trim()
if ($shaAfterGates -ne $sha) {
    throw "HEAD changed while gates were running. before=$sha after=$shaAfterGates"
}

$unexpected = @(Get-UnexpectedStatus)
if ($unexpected.Count -gt 0) {
    Write-Host 'Build/gates produced tracked changes:' -ForegroundColor Red
    $unexpected | ForEach-Object { Write-Host $_ }
    throw 'STOP before benchmark. Return status; do not repair source manually.'
}

Write-Host "`n=== PRS1 R5 PRE-BENCHMARK CONTRACT PASS ===" -ForegroundColor Green
Write-Host "source_commit=$sha"
Write-Host 'fmt=PASS'
Write-Host 'clippy_warning_map=0'
Write-Host 'strict_clippy=PASS'
Write-Host 'targeted_tests=PASS'
Write-Host 'workspace_tests=PASS'
Write-Host 'release_build=PASS'
Write-Host 'git_diff_check=PASS'

$traceRoot = if ([string]::IsNullOrWhiteSpace($ExternalEvidenceRoot)) {
    Join-Path $env:TEMP 'pithos-prs1-r5'
} else {
    $ExternalEvidenceRoot
}
New-Item -ItemType Directory -Force -Path $traceRoot | Out-Null
$timestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$tracePath = Join-Path $traceRoot "prs1-r5-trace-$timestamp.log"

Write-Host "`n=== PRS1 R5 PITHOS-ONLY FROZEN-BASELINE BENCHMARK ===" -ForegroundColor Cyan
Write-Host '7-Zip/WinRAR/WinZip executables are not run.' -ForegroundColor Yellow
$env:PITHOS_REP_TRACE = '1'
try {
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'run-tst-compact-frozen-baseline.ps1') `
        -Corpus $Corpus `
        -PhaseMaxTotalMiB $PhaseMaxTotalMiB `
        -ExternalEvidenceRoot $ExternalEvidenceRoot 2>&1 | Tee-Object -FilePath $tracePath
    $benchmarkExit = $LASTEXITCODE
} finally {
    Remove-Item Env:PITHOS_REP_TRACE -ErrorAction SilentlyContinue
}
if ($benchmarkExit -ne 0) {
    throw "PRS1 R5 frozen-baseline benchmark failed with exit code $benchmarkExit. Trace: $tracePath"
}

$evidenceRoot = Join-Path $repo 'docs\benchmarks\evidence'
$evidence = Get-ChildItem -LiteralPath $evidenceRoot -Directory -Filter 'frozen-feat-31-representation-substrate-*' |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1
if ($null -eq $evidence) { throw 'Frozen-baseline evidence directory not found.' }
Copy-Item -LiteralPath $tracePath -Destination (Join-Path $evidence.FullName 'prs1-representation-trace.log') -Force

function Parse-TraceLine([string]$Line) {
    if ($Line -notlike 'PITHOS_REP_TRACE*') { return $null }
    $map = [ordered]@{}
    foreach ($part in ($Line -split "`t")) {
        if ($part -eq 'PITHOS_REP_TRACE') { continue }
        $pair = $part -split '=', 2
        if ($pair.Count -eq 2) { $map[$pair[0]] = $pair[1] }
    }
    if ($map.Count -eq 0) { return $null }
    return [pscustomobject]$map
}

$traceRows = New-Object System.Collections.Generic.List[object]
Get-Content -LiteralPath $tracePath | ForEach-Object {
    $row = Parse-TraceLine $_
    if ($null -ne $row) { $traceRows.Add($row) }
}
if ($traceRows.Count -eq 0) { throw 'No PITHOS_REP_TRACE rows captured.' }
$traceRows | Export-Csv -LiteralPath (Join-Path $evidence.FullName 'prs1-trace.csv') -NoTypeInformation -Encoding UTF8

$raceRows = @($traceRows | Where-Object { $_.stage -eq 'representation_race' })
$summaryRows = @($traceRows | Where-Object { $_.stage -eq 'prs1_summary' })
$prs1Wins = @($raceRows | Where-Object { $_.winner -eq 'prs1' }).Count
$v12Wins = @($raceRows | Where-Object { $_.winner -eq 'v12' }).Count
$v17Wins = @($raceRows | Where-Object { $_.winner -eq 'v17' }).Count

$sum = {
    param([string]$Property)
    [int64](($summaryRows | ForEach-Object { if ($_.$Property) { [int64]$_.$Property } else { 0 } } | Measure-Object -Sum).Sum)
}
$summaryPath = Join-Path $evidence.FullName 'PRS1_R5_SUMMARY.txt'
@(
    "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
    "branch=$branch",
    "source_commit=$sha",
    "representation_races=$($raceRows.Count)",
    "prs1_wins=$prs1Wins",
    "v12_wins=$v12Wins",
    "v17_wins=$v17Wins",
    "prs1_candidate_summaries=$($summaryRows.Count)",
    "raw_cells=$(& $sum 'raw')",
    "exact_ref_cells=$(& $sum 'exact_ref')",
    "overlay_cells=$(& $sum 'overlay')",
    "overlay_xor_cells=$(& $sum 'overlay_xor')",
    "mixture_cells=$(& $sum 'mixture')",
    "mixture_combinadic_cells=$(& $sum 'mixture_combinadic')",
    "axial_cells=$(& $sum 'axial')",
    "axial_xor_cells=$(& $sum 'axial_xor')",
    "axial_even_odd_cells=$(& $sum 'axial_even_odd')",
    "defect_cells=$(& $sum 'defect')",
    "periodic_defect_cells=$(& $sum 'periodic_defect')",
    "transition_cells=$(& $sum 'transition')",
    "delta_transition_cells=$(& $sum 'delta_transition')",
    '7zip_executed=False',
    'winrar_executed=False',
    'winzip_executed=False'
) | Set-Content -LiteralPath $summaryPath -Encoding UTF8

Write-Host "`n=== PRS1 R5 RESULT ===" -ForegroundColor Green
Get-Content -LiteralPath $summaryPath | ForEach-Object { Write-Host $_ }
Write-Host "Evidence: $($evidence.FullName)"
Write-Host "Trace: $tracePath"
exit 0
