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
function Get-UnexpectedStatus {
    $rows = @(& git status --porcelain)
    return @($rows | Where-Object {
        $path = if ($_.Length -gt 3) { $_.Substring(3).Replace('\\','/') } else { $_ }
        $allowedLocal -notcontains $path
    })
}

$unexpected = @(Get-UnexpectedStatus)
if ($unexpected.Count -gt 0) {
    $unexpected | ForEach-Object { Write-Host $_ -ForegroundColor Red }
    throw 'Unexpected local changes before PRS1 R5.'
}

Write-Host '=== PRS1 R5 STATIC/ROUNDTRIP GATES ===' -ForegroundColor Cyan
& cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { throw 'cargo fmt --check failed' }

& cargo test -p pithos-representation-substrate -p pithos-native-codec-v18
if ($LASTEXITCODE -ne 0) { throw 'PRS1/native-v18 tests failed' }

& cargo clippy -p pithos-representation-substrate -p pithos-native-codec-v18 --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { throw 'PRS1/native-v18 strict clippy failed' }

Write-Host "`n=== WORKSPACE REGRESSION GATES ===" -ForegroundColor Cyan
& cargo test --workspace
if ($LASTEXITCODE -ne 0) { throw 'workspace tests failed' }

& cargo clippy --workspace --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { throw 'workspace strict clippy failed' }

& cargo build --release -p pithos-cli
if ($LASTEXITCODE -ne 0) { throw 'release CLI build failed' }

$unexpected = @(Get-UnexpectedStatus)
if ($unexpected.Count -gt 0) {
    Write-Host 'Build/gates produced tracked changes:' -ForegroundColor Red
    $unexpected | ForEach-Object { Write-Host $_ }
    throw 'STOP before benchmark. Return status; do not repair source manually.'
}

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
    "mixture_cells=$(& $sum 'mixture')",
    "axial_cells=$(& $sum 'axial')",
    "defect_cells=$(& $sum 'defect')",
    "transition_cells=$(& $sum 'transition')",
    '7zip_executed=False',
    'winrar_executed=False',
    'winzip_executed=False'
) | Set-Content -LiteralPath $summaryPath -Encoding UTF8

Write-Host "`n=== PRS1 R5 RESULT ===" -ForegroundColor Green
Get-Content -LiteralPath $summaryPath | ForEach-Object { Write-Host $_ }
Write-Host "Evidence: $($evidence.FullName)"
Write-Host "Trace: $tracePath"
exit 0
