#requires -Version 5.1
param(
    [string]$Corpus = "tst_compact",
    [int]$PhaseMaxTotalMiB = 2048
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repo

$corpusPath = if ([System.IO.Path]::IsPathRooted($Corpus)) {
    $Corpus
} else {
    Join-Path $repo $Corpus
}
if (-not (Test-Path -LiteralPath $corpusPath -PathType Container)) {
    throw "Corpus directory not found: $corpusPath"
}

$resultsPath = Join-Path $corpusPath 'results'
New-Item -ItemType Directory -Force -Path $resultsPath | Out-Null

# Make common Windows installations visible to child benchmark processes even
# when installers did not add their CLI directory to PATH.
$programFiles = [System.Environment]::GetFolderPath('ProgramFiles')
$programFilesX86 = [System.Environment]::GetEnvironmentVariable('ProgramFiles(x86)')
$toolDirs = New-Object System.Collections.Generic.List[string]
foreach ($base in @($programFiles, $programFilesX86)) {
    if ([string]::IsNullOrWhiteSpace($base)) {
        continue
    }
    foreach ($leaf in @('7-Zip', 'WinRAR', 'WinZip')) {
        $candidate = Join-Path $base $leaf
        if ((Test-Path -LiteralPath $candidate -PathType Container) -and -not $toolDirs.Contains($candidate)) {
            $toolDirs.Add($candidate)
        }
    }
}
if ($toolDirs.Count -gt 0) {
    $env:PATH = (($toolDirs.ToArray() + @($env:PATH)) -join [System.IO.Path]::PathSeparator)
}

$timestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$evidencePath = Join-Path $repo "docs/benchmarks/evidence/tst-compact-$timestamp"
New-Item -ItemType Directory -Force -Path $evidencePath | Out-Null

$utf8 = New-Object System.Text.UTF8Encoding -ArgumentList $false
$toolsPath = Join-Path $resultsPath 'tools.txt'
$toolLines = New-Object System.Collections.Generic.List[string]
$toolLines.Add("timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))")
$toolLines.Add("repo=$repo")
$toolLines.Add("corpus=$corpusPath")
$toolLines.Add("powershell=$($PSVersionTable.PSVersion)")
$toolLines.Add("rustc=$(& rustc --version 2>&1)")
$toolLines.Add("cargo=$(& cargo --version 2>&1)")
foreach ($tool in @('7z', '7zz', 'WinRAR', 'wzzip', 'wzunzip')) {
    $command = Get-Command $tool -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $command) {
        $toolLines.Add("$tool=NOT_FOUND")
    } else {
        $toolLines.Add("$tool=$($command.Source)")
    }
}
[System.IO.File]::WriteAllLines($toolsPath, $toolLines, $utf8)

Write-Host "=== Corpus inventory ===" -ForegroundColor Cyan
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'inventory-tst-compact.ps1') -Corpus $corpusPath
$inventoryExit = $LASTEXITCODE

Write-Host "`n=== Phase analysis benchmark ===" -ForegroundColor Cyan
& cargo run --release -p pithos-bench --bin pithos-phasebench -- --corpus $corpusPath --results $resultsPath --max-total-mib $PhaseMaxTotalMiB
$phaseExit = $LASTEXITCODE

Write-Host "`n=== Comparative compression benchmark ===" -ForegroundColor Cyan
& cargo run --release -p pithos-bench --bin pithos-bench -- --corpus $corpusPath --results $resultsPath
$benchmarkExit = $LASTEXITCODE

$reportNames = @(
    'corpus-manifest.csv',
    'corpus-summary.txt',
    'tools.txt',
    'phase-analysis.jsonl',
    'phase-analysis-summary.json',
    'benchmark.jsonl',
    'benchmark.csv',
    'pithos-telemetry.jsonl'
)
foreach ($name in $reportNames) {
    $source = Join-Path $resultsPath $name
    if (Test-Path -LiteralPath $source -PathType Leaf) {
        Copy-Item -LiteralPath $source -Destination (Join-Path $evidencePath $name) -Force
    }
}

$summaryPath = Join-Path $evidencePath 'RUN_SUMMARY.txt'
$summaryLines = @(
    "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
    "commit=$(& git rev-parse HEAD)",
    "branch=$(& git branch --show-current)",
    "inventory_exit=$inventoryExit",
    "phasebench_exit=$phaseExit",
    "benchmark_exit=$benchmarkExit",
    "corpus=$corpusPath",
    "local_results=$resultsPath",
    "versioned_evidence=$evidencePath"
)
[System.IO.File]::WriteAllLines($summaryPath, [string[]]$summaryLines, $utf8)

Write-Host "`nBenchmark evidence: $evidencePath" -ForegroundColor Green
if (($inventoryExit -ne 0) -or ($phaseExit -ne 0) -or ($benchmarkExit -ne 0)) {
    exit 1
}
exit 0
