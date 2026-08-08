#requires -Version 5.1
param(
    [string]$Corpus = "tst_compact",
    [int]$PhaseMaxTotalMiB = 2048,
    [string]$ExternalEvidenceRoot = ""
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repo
$corpusPath = if ([System.IO.Path]::IsPathRooted($Corpus)) { $Corpus } else { Join-Path $repo $Corpus }
if (-not (Test-Path -LiteralPath $corpusPath -PathType Container)) { throw "Corpus directory not found: $corpusPath" }

$baselinePath = Join-Path $repo 'docs\benchmarks\7zip-best-baseline.csv'
if (-not (Test-Path -LiteralPath $baselinePath -PathType Leaf)) { throw "Frozen 7-Zip baseline not found: $baselinePath" }
$baselineRows = @(Import-Csv -LiteralPath $baselinePath)
$baselineMap = @{}
foreach ($row in $baselineRows) { $baselineMap[$row.case] = $row }

$resultsPath = Join-Path $corpusPath 'results'
New-Item -ItemType Directory -Force -Path $resultsPath | Out-Null
$workPath = Join-Path $resultsPath 'work'
if (Test-Path -LiteralPath $workPath) { Remove-Item -LiteralPath $workPath -Recurse -Force }
foreach ($stale in @(
    'phase-analysis.jsonl','phase-analysis-summary.json','codec-benchmark.jsonl','codec-benchmark.csv',
    'benchmark.jsonl','benchmark.csv','benchmark-summary.md','pithos-telemetry.jsonl',
    'frozen-baseline-comparison.csv','frozen-baseline-summary.txt'
)) {
    Remove-Item -LiteralPath (Join-Path $resultsPath $stale) -Force -ErrorAction SilentlyContinue
}

$timestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$branch = (& git branch --show-current).Trim()
$sha = (& git rev-parse HEAD).Trim()
$safeBranch = $branch.Replace('/','-')
$evidencePath = Join-Path $repo "docs\benchmarks\evidence\frozen-$safeBranch-$timestamp"
New-Item -ItemType Directory -Force -Path $evidencePath | Out-Null

$corpusFiles = @(Get-ChildItem -LiteralPath $corpusPath -File -Recurse |
    Where-Object { -not $_.FullName.StartsWith($resultsPath, [System.StringComparison]::OrdinalIgnoreCase) })
if ($corpusFiles.Count -eq 0) { throw 'Frozen corpus is empty.' }

Write-Host '=== Corpus inventory ===' -ForegroundColor Cyan
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'inventory-tst-compact.ps1') -Corpus $corpusPath
if ($LASTEXITCODE -ne 0) { throw "inventory failed with exit code $LASTEXITCODE" }

Write-Host "`n=== Phase analysis benchmark (Pithos internal) ===" -ForegroundColor Cyan
& cargo run --release -p pithos-bench --bin pithos-phasebench -- --corpus $corpusPath --results $resultsPath --max-total-mib $PhaseMaxTotalMiB
if ($LASTEXITCODE -ne 0) { throw "phasebench failed with exit code $LASTEXITCODE" }

Write-Host "`n=== Codec contribution benchmark (Pithos internal) ===" -ForegroundColor Cyan
& cargo run --release -p pithos-bench --bin pithos-codecbench -- --corpus $corpusPath --results $resultsPath
if ($LASTEXITCODE -ne 0) { throw "codecbench failed with exit code $LASTEXITCODE" }

Write-Host "`n=== Compression benchmark: PITHOS ONLY ===" -ForegroundColor Cyan
Write-Host '7-Zip is NOT executed. Comparison uses docs/benchmarks/7zip-best-baseline.csv.' -ForegroundColor Yellow
& cargo run --release -p pithos-bench --bin pithos-bench -- --corpus $corpusPath --results $resultsPath --pithos-only
if ($LASTEXITCODE -ne 0) { throw "pithos-only benchmark failed with exit code $LASTEXITCODE" }

$benchmarkPath = Join-Path $resultsPath 'benchmark.csv'
if (-not (Test-Path -LiteralPath $benchmarkPath -PathType Leaf)) { throw 'benchmark.csv not produced.' }
$pithosRows = @(Import-Csv -LiteralPath $benchmarkPath | Where-Object { $_.compressor -eq 'pithos' -and $_.profile -eq 'archive-max' })
if ($pithosRows.Count -eq 0) { throw 'No Pithos archive-max rows found.' }

function Parse-OptionalDouble([string]$Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) { return $null }
    return [double]::Parse($Value, [System.Globalization.CultureInfo]::InvariantCulture)
}

$comparison = New-Object System.Collections.Generic.List[object]
$missingCases = New-Object System.Collections.Generic.List[string]
foreach ($p in $pithosRows) {
    if (-not $baselineMap.ContainsKey($p.case)) {
        $missingCases.Add($p.case)
        continue
    }
    $b = $baselineMap[$p.case]
    $pArchive = Parse-OptionalDouble $p.archive_bytes
    $pCompress = Parse-OptionalDouble $p.compress_ms
    $pVerify = Parse-OptionalDouble $p.verify_ms
    $pDecompress = Parse-OptionalDouble $p.decompress_ms
    $bArchive = Parse-OptionalDouble $b.archive_bytes
    $bCompress = Parse-OptionalDouble $b.compress_ms
    $bVerify = Parse-OptionalDouble $b.verify_ms
    $bDecompress = Parse-OptionalDouble $b.decompress_ms

    $comparison.Add([pscustomobject]@{
        case = $p.case
        status = $p.status
        original_bytes = $p.original_bytes
        pithos_archive_bytes = $pArchive
        sevenzip_best_archive_bytes = $bArchive
        delta_bytes_vs_7zip_best = if ($null -ne $pArchive -and $null -ne $bArchive) { [int64]($pArchive - $bArchive) } else { $null }
        pithos_savings_percent = $p.savings_percent
        sevenzip_best_savings_percent = $b.savings_percent
        pithos_compress_ms = $pCompress
        sevenzip_best_compress_ms = $bCompress
        compress_ratio_vs_7zip_best = if ($null -ne $pCompress -and $null -ne $bCompress -and $bCompress -gt 0) { [math]::Round($pCompress / $bCompress, 6) } else { $null }
        pithos_verify_ms = $pVerify
        sevenzip_best_verify_ms = $bVerify
        verify_ratio_vs_7zip_best = if ($null -ne $pVerify -and $null -ne $bVerify -and $bVerify -gt 0) { [math]::Round($pVerify / $bVerify, 6) } else { $null }
        pithos_decompress_ms = $pDecompress
        sevenzip_best_decompress_ms = $bDecompress
        decompress_ratio_vs_7zip_best = if ($null -ne $pDecompress -and $null -ne $bDecompress -and $bDecompress -gt 0) { [math]::Round($pDecompress / $bDecompress, 6) } else { $null }
        sevenzip_executed = $false
    })
}
if ($missingCases.Count -gt 0) {
    $missingCases | Set-Content -LiteralPath (Join-Path $evidencePath 'MISSING_BASELINE_CASES.txt') -Encoding UTF8
    throw "Frozen baseline missing $($missingCases.Count) benchmark cases."
}

$comparisonPath = Join-Path $resultsPath 'frozen-baseline-comparison.csv'
$comparison | Export-Csv -LiteralPath $comparisonPath -NoTypeInformation -Encoding UTF8

$ok = @($comparison | Where-Object { $_.status -eq 'ok' })
$sizeWins = @($ok | Where-Object { $null -ne $_.delta_bytes_vs_7zip_best -and $_.delta_bytes_vs_7zip_best -lt 0 }).Count
$compressWins = @($ok | Where-Object { $null -ne $_.compress_ratio_vs_7zip_best -and $_.compress_ratio_vs_7zip_best -lt 1 }).Count
$verifyComparable = @($ok | Where-Object { $null -ne $_.verify_ratio_vs_7zip_best }).Count
$verifyWins = @($ok | Where-Object { $null -ne $_.verify_ratio_vs_7zip_best -and $_.verify_ratio_vs_7zip_best -lt 1 }).Count
$decompressWins = @($ok | Where-Object { $null -ne $_.decompress_ratio_vs_7zip_best -and $_.decompress_ratio_vs_7zip_best -lt 1 }).Count
$combined = $comparison | Where-Object { $_.case -eq 'combined-all' } | Select-Object -First 1

$summaryPath = Join-Path $resultsPath 'frozen-baseline-summary.txt'
@(
    "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
    "branch=$branch",
    "commit=$sha",
    "cases=$($comparison.Count)",
    "successful_cases=$($ok.Count)",
    "size_wins_vs_7zip_best=$sizeWins",
    "compress_wins_vs_7zip_best=$compressWins",
    "verify_comparable_cases=$verifyComparable",
    "verify_wins_vs_7zip_best=$verifyWins",
    "decompress_wins_vs_7zip_best=$decompressWins",
    "combined_pithos_archive_bytes=$($combined.pithos_archive_bytes)",
    "combined_7zip_best_archive_bytes=$($combined.sevenzip_best_archive_bytes)",
    "combined_delta_bytes=$($combined.delta_bytes_vs_7zip_best)",
    "combined_compress_ratio=$($combined.compress_ratio_vs_7zip_best)",
    "combined_verify_ratio=$($combined.verify_ratio_vs_7zip_best)",
    "combined_decompress_ratio=$($combined.decompress_ratio_vs_7zip_best)",
    '7zip_executed=False',
    'winrar_executed=False',
    'winzip_executed=False'
) | Set-Content -LiteralPath $summaryPath -Encoding UTF8

foreach ($name in @(
    'corpus-manifest.csv','corpus-summary.txt','source-register.csv','download-missing.txt',
    'phase-analysis.jsonl','phase-analysis-summary.json','codec-benchmark.jsonl','codec-benchmark.csv',
    'benchmark.jsonl','benchmark.csv','pithos-telemetry.jsonl','frozen-baseline-comparison.csv','frozen-baseline-summary.txt'
)) {
    $source = Join-Path $resultsPath $name
    if (Test-Path -LiteralPath $source -PathType Leaf) { Copy-Item -LiteralPath $source -Destination (Join-Path $evidencePath $name) -Force }
}
Copy-Item -LiteralPath $baselinePath -Destination (Join-Path $evidencePath '7zip-best-baseline.csv') -Force

if (-not [string]::IsNullOrWhiteSpace($ExternalEvidenceRoot)) {
    New-Item -ItemType Directory -Force -Path $ExternalEvidenceRoot | Out-Null
    Copy-Item -LiteralPath $evidencePath -Destination (Join-Path $ExternalEvidenceRoot (Split-Path $evidencePath -Leaf)) -Recurse -Force
}

Write-Host "`n=== FROZEN 7-ZIP BASELINE RESULT ===" -ForegroundColor Green
Get-Content -LiteralPath $summaryPath | ForEach-Object { Write-Host $_ }
Write-Host ''
$comparison | Select-Object case,pithos_archive_bytes,sevenzip_best_archive_bytes,delta_bytes_vs_7zip_best,compress_ratio_vs_7zip_best,decompress_ratio_vs_7zip_best,status | Format-Table -AutoSize | Out-String | Write-Host
Write-Host "Evidence: $evidencePath"
exit 0
