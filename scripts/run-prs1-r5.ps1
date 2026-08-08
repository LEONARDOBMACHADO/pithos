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

$nativeHelper = Join-Path $PSScriptRoot 'native-process.ps1'
$analyzer = Join-Path $PSScriptRoot 'analyze-prs1-r5-trace.ps1'
$frozenRunner = Join-Path $PSScriptRoot 'run-tst-compact-frozen-baseline.ps1'
foreach ($required in @($nativeHelper, $analyzer, $frozenRunner)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Required R5 script not found: $required"
    }
}

function Assert-PowerShellSyntax([string]$Path) {
    $tokens = $null
    $parseErrors = $null
    [System.Management.Automation.Language.Parser]::ParseFile(
        $Path,
        [ref]$tokens,
        [ref]$parseErrors
    ) | Out-Null
    if ($parseErrors.Count -gt 0) {
        $parseErrors | Format-List * | Out-String | Write-Host
        throw "PowerShell syntax error(s): $Path"
    }
}

foreach ($script in @($PSCommandPath, $nativeHelper, $analyzer, $frozenRunner)) {
    Assert-PowerShellSyntax $script
}
Write-Host 'R5 POWERSHELL PARSE: PASS' -ForegroundColor Green

. $nativeHelper

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

Write-Host '=== PRS1 R5 NATIVE STDERR CONTRACT ===' -ForegroundColor Cyan
$stderrProbe = Join-Path $env:TEMP "pithos-r5-stderr-probe-$PID.log"
try {
    $stderrProbeExit = Invoke-PithosNativeProcess `
        -FilePath 'powershell' `
        -Arguments @(
            '-NoProfile',
            '-Command',
            '[Console]::Error.WriteLine("R5_STDERR_PROBE"); exit 0'
        ) `
        -LogPath $stderrProbe
    if ($stderrProbeExit -ne 0) {
        throw "R5 stderr probe returned exit code $stderrProbeExit"
    }
    if (-not (Select-String -LiteralPath $stderrProbe -SimpleMatch 'R5_STDERR_PROBE' -Quiet)) {
        throw 'R5 stderr probe output was not preserved in the native-process log.'
    }
    Write-Host 'R5 STDERR PROBE: PASS (stderr preserved; exit code controls failure)' -ForegroundColor Green
} finally {
    Remove-Item -LiteralPath $stderrProbe -Force -ErrorAction SilentlyContinue
}

# Materialize only the missing workspace-package relationship in the existing
# lockfile. `cargo generate-lockfile` is deliberately avoided because it can
# re-resolve unrelated semver dependencies. The targeted check lets Cargo update
# the current lock minimally; the run then stops for explicit diff review/commit.
$lockPath = Join-Path $repo 'Cargo.lock'
$lockHasSubstrate = (Test-Path -LiteralPath $lockPath -PathType Leaf) -and
    (Select-String -LiteralPath $lockPath -SimpleMatch 'name = "pithos-representation-substrate"' -Quiet)
if (-not $lockHasSubstrate) {
    Write-Host '=== PRS1 R5 MINIMAL LOCKFILE PREPARATION ===' -ForegroundColor Yellow
    $lockCheckExit = Invoke-PithosNativeProcess `
        -FilePath 'cargo' `
        -Arguments @(
            'check',
            '-p','pithos-representation-substrate',
            '-p','pithos-native-codec-v18'
        )
    if ($lockCheckExit -ne 0) {
        throw "targeted cargo check for lockfile preparation failed with exit code $lockCheckExit"
    }

    $unexpectedAfterLock = @(Get-UnexpectedStatus -AllowCargoLock)
    if ($unexpectedAfterLock.Count -gt 0) {
        $unexpectedAfterLock | ForEach-Object { Write-Host $_ -ForegroundColor Red }
        throw 'Targeted Cargo lock preparation changed files other than Cargo.lock.'
    }
    $status = @(& git status --porcelain)
    $cargoLockChanged = @($status | Where-Object {
        $_.Length -gt 3 -and $_.Substring(3).Replace('\','/') -eq 'Cargo.lock'
    }).Count -eq 1
    if (-not $cargoLockChanged) {
        throw 'Cargo.lock did not change as expected after adding the PRS1 workspace crate.'
    }
    if (-not (Select-String -LiteralPath $lockPath -SimpleMatch 'name = "pithos-representation-substrate"' -Quiet)) {
        throw 'Cargo-updated lockfile still does not contain pithos-representation-substrate.'
    }

    Write-Host "`n===== TARGETED CARGO.LOCK DIFF =====" -ForegroundColor Cyan
    & git diff -- Cargo.lock
    Write-Host "`nSTOP: review and commit ONLY the Cargo-generated Cargo.lock, then rerun R5." -ForegroundColor Yellow
    Write-Host 'Reject the lock diff if unrelated external package versions/checksums changed. Do not edit Cargo.lock manually.'
    exit 23
}

Write-Host '=== PRS1 R5 LOCKFILE REPRODUCIBILITY ===' -ForegroundColor Cyan
$metadataExit = Invoke-PithosNativeProcess `
    -FilePath 'cargo' `
    -Arguments @('metadata','--locked','--format-version','1','--no-deps') `
    -DiscardOutput
if ($metadataExit -ne 0) {
    throw "cargo metadata --locked failed with exit code $metadataExit; Cargo.lock is stale or inconsistent."
}

$gateRoot = if ([string]::IsNullOrWhiteSpace($ExternalEvidenceRoot)) {
    Join-Path $env:TEMP 'pithos-prs1-r5-gates'
} else {
    Join-Path $ExternalEvidenceRoot 'gates'
}
New-Item -ItemType Directory -Force -Path $gateRoot | Out-Null
$gateTimestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$clippyMapLog = Join-Path $gateRoot "prs1-r5-clippy-map-$gateTimestamp.log"
$strictLog = Join-Path $gateRoot "prs1-r5-clippy-strict-$gateTimestamp.log"

Write-Host "`n=== R5 STATIC GATE 1/5: RUSTFMT ===" -ForegroundColor Cyan
$fmtExit = Invoke-PithosNativeProcess `
    -FilePath 'cargo' `
    -Arguments @('fmt','--all','--','--check')
if ($fmtExit -ne 0) {
    throw "cargo fmt --all -- --check failed with exit code $fmtExit. STOP before benchmark."
}

Write-Host "`n=== R5 STATIC GATE 2/5: COMPLETE CLIPPY WARNING MAP ===" -ForegroundColor Cyan
$clippyMapExit = Invoke-PithosNativeProcess `
    -FilePath 'cargo' `
    -Arguments @('clippy','--workspace','--all-targets') `
    -LogPath $clippyMapLog
if ($clippyMapExit -ne 0) {
    Stop-WithLog "Workspace Clippy map failed with exit code $clippyMapExit. STOP before tests/benchmark." $clippyMapLog
}
$warningLines = @(Select-String -LiteralPath $clippyMapLog -Pattern '(^|\s)warning:' -CaseSensitive:$false)
if ($warningLines.Count -gt 0) {
    Stop-WithLog "Workspace is not warning-clean: $($warningLines.Count) warning line(s)." $clippyMapLog
}
Write-Host 'COMPLETE CLIPPY WARNING MAP: 0 warnings' -ForegroundColor Green

Write-Host "`n=== R5 STATIC GATE 3/5: STRICT CLIPPY ===" -ForegroundColor Cyan
$strictExit = Invoke-PithosNativeProcess `
    -FilePath 'cargo' `
    -Arguments @('clippy','--workspace','--all-targets','--','-D','warnings') `
    -LogPath $strictLog
if ($strictExit -ne 0) {
    Stop-WithLog "Workspace strict Clippy failed with exit code $strictExit." $strictLog
}
Write-Host 'STRICT CLIPPY: PASS' -ForegroundColor Green

Write-Host "`n=== R5 STATIC GATE 4/5: TARGETED ROUNDTRIP TESTS ===" -ForegroundColor Cyan
$targetedTestsExit = Invoke-PithosNativeProcess `
    -FilePath 'cargo' `
    -Arguments @('test','-p','pithos-representation-substrate','-p','pithos-native-codec-v18')
if ($targetedTestsExit -ne 0) {
    throw "PRS1/native-v18 tests failed with exit code $targetedTestsExit."
}

Write-Host "`n=== R5 STATIC GATE 5/5: WORKSPACE REGRESSION + RELEASE ===" -ForegroundColor Cyan
$workspaceTestsExit = Invoke-PithosNativeProcess `
    -FilePath 'cargo' `
    -Arguments @('test','--workspace')
if ($workspaceTestsExit -ne 0) {
    throw "workspace tests failed with exit code $workspaceTestsExit."
}
$releaseExit = Invoke-PithosNativeProcess `
    -FilePath 'cargo' `
    -Arguments @('build','--release','-p','pithos-cli')
if ($releaseExit -ne 0) {
    throw "release CLI build failed with exit code $releaseExit."
}
$diffCheckExit = Invoke-PithosNativeProcess `
    -FilePath 'git' `
    -Arguments @('diff','--check')
if ($diffCheckExit -ne 0) {
    throw "git diff --check failed with exit code $diffCheckExit."
}

$shaAfterGates = (& git rev-parse HEAD).Trim()
if ($shaAfterGates -ne $sha) {
    throw "HEAD changed while gates were running. before=$sha after=$shaAfterGates"
}
$unexpected = @(Get-UnexpectedStatus)
if ($unexpected.Count -gt 0) {
    $unexpected | ForEach-Object { Write-Host $_ -ForegroundColor Red }
    throw 'Gates produced unexpected tracked/local changes. STOP before benchmark.'
}

Write-Host "`n=== PRS1 R5 PRE-BENCHMARK CONTRACT: PASS ===" -ForegroundColor Green
Write-Host "source_commit=$sha"
Write-Host 'native_process_failure_policy=EXIT_CODE_ONLY'
Write-Host 'benchmark_profiles=archive-max-only'
Write-Host 'clippy_warning_map=0'

$traceRoot = Join-Path $env:TEMP 'pithos-prs1-r5'
New-Item -ItemType Directory -Force -Path $traceRoot | Out-Null
$timestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$tracePath = Join-Path $traceRoot "prs1-r5-trace-$timestamp.log"
$benchmarkStartedUtc = [DateTime]::UtcNow

Write-Host "`n=== PRS1 R5 PITHOS-ONLY FROZEN-BASELINE BENCHMARK ===" -ForegroundColor Cyan
Write-Host '7-Zip/WinRAR/WinZip executables are not run.' -ForegroundColor Yellow
$env:PITHOS_REP_TRACE = '1'
try {
    # The child does not copy evidence externally. The parent enriches local
    # evidence first and performs one final atomic-ish directory refresh below.
    $benchmarkArguments = @(
        '-NoProfile',
        '-ExecutionPolicy','Bypass',
        '-File',$frozenRunner,
        '-Corpus',$Corpus,
        '-PhaseMaxTotalMiB',[string]$PhaseMaxTotalMiB
    )
    $benchmarkExit = Invoke-PithosNativeProcess `
        -FilePath 'powershell' `
        -Arguments $benchmarkArguments `
        -LogPath $tracePath
} finally {
    Remove-Item Env:PITHOS_REP_TRACE -ErrorAction SilentlyContinue
}
if ($benchmarkExit -ne 0) {
    throw "PRS1 R5 frozen-baseline benchmark failed with exit code $benchmarkExit. Trace: $tracePath"
}

$evidenceRoot = Join-Path $repo 'docs\benchmarks\evidence'
$evidence = Get-ChildItem -LiteralPath $evidenceRoot -Directory -Filter 'frozen-feat-31-representation-substrate-*' |
    Where-Object { $_.LastWriteTimeUtc -ge $benchmarkStartedUtc.AddSeconds(-5) } |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1
if ($null -eq $evidence) {
    throw 'R5 evidence directory from this benchmark run was not found.'
}

Copy-Item -LiteralPath $tracePath -Destination (Join-Path $evidence.FullName 'prs1-representation-trace.log') -Force

$analyzerExit = Invoke-PithosNativeProcess `
    -FilePath 'powershell' `
    -Arguments @(
        '-NoProfile',
        '-ExecutionPolicy','Bypass',
        '-File',$analyzer,
        '-TracePath',$tracePath,
        '-EvidencePath',$evidence.FullName,
        '-Branch',$branch,
        '-SourceCommit',$sha
    )
if ($analyzerExit -ne 0) {
    throw "PRS1 R5 trace analysis failed with exit code $analyzerExit. Evidence=$($evidence.FullName)"
}

$summaryPath = Join-Path $evidence.FullName 'PRS1_R5_SUMMARY.txt'
if (-not (Test-Path -LiteralPath $summaryPath -PathType Leaf)) {
    throw 'PRS1_R5_SUMMARY.txt was not produced by analyzer.'
}

if (-not [string]::IsNullOrWhiteSpace($ExternalEvidenceRoot)) {
    New-Item -ItemType Directory -Force -Path $ExternalEvidenceRoot | Out-Null
    $externalFinal = Join-Path $ExternalEvidenceRoot $evidence.Name
    if (Test-Path -LiteralPath $externalFinal) {
        Remove-Item -LiteralPath $externalFinal -Recurse -Force
    }
    Copy-Item -LiteralPath $evidence.FullName -Destination $externalFinal -Recurse -Force
    Write-Host "Final enriched external evidence: $externalFinal" -ForegroundColor Green
}

Write-Host "`n=== PRS1 R5 RESULT ===" -ForegroundColor Green
Get-Content -LiteralPath $summaryPath | ForEach-Object { Write-Host $_ }
Write-Host "Evidence: $($evidence.FullName)"
Write-Host "Trace: $tracePath"
exit 0
