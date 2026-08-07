#requires -Version 5.1
param(
    [string]$Results = "tst_compact/results"
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repo
$resultsPath = if ([System.IO.Path]::IsPathRooted($Results)) { $Results } else { Join-Path $repo $Results }
$csvPath = Join-Path $resultsPath 'benchmark.csv'
$codecPath = Join-Path $resultsPath 'codec-benchmark.csv'
$phasePath = Join-Path $resultsPath 'phase-analysis-summary.json'
$outPath = Join-Path $resultsPath 'benchmark-summary.md'

if (-not (Test-Path -LiteralPath $csvPath -PathType Leaf)) {
    throw "Benchmark CSV not found: $csvPath"
}

$rows = @(Import-Csv -LiteralPath $csvPath)
$okRows = @($rows | Where-Object { $_.status -eq 'ok' -and -not [string]::IsNullOrWhiteSpace($_.archive_bytes) })

$lines = New-Object System.Collections.Generic.List[string]
$lines.Add('# Pithos compression benchmark summary')
$lines.Add('')
$lines.Add("Generated UTC: $((Get-Date).ToUniversalTime().ToString('o'))")
$lines.Add('')

$combined = @($okRows | Where-Object { $_.case -eq 'combined-all' } | Sort-Object { [int64]$_.archive_bytes })
$lines.Add('## Combined corpus')
$lines.Add('')
if ($combined.Count -eq 0) {
    $lines.Add('No successful combined-corpus result was recorded.')
} else {
    $lines.Add('| Rank | Compressor | Profile | Archive MiB | Savings % | Compress s | Decompress s |')
    $lines.Add('|---:|---|---|---:|---:|---:|---:|')
    $rank = 1
    foreach ($row in $combined) {
        $archiveMiB = [math]::Round(([double]$row.archive_bytes / 1MB), 3)
        $compressSec = if ([string]::IsNullOrWhiteSpace($row.compress_ms)) { '' } else { [math]::Round(([double]$row.compress_ms / 1000), 3) }
        $decompressSec = if ([string]::IsNullOrWhiteSpace($row.decompress_ms)) { '' } else { [math]::Round(([double]$row.decompress_ms / 1000), 3) }
        $lines.Add("| $rank | $($row.compressor) | $($row.profile) | $archiveMiB | $($row.savings_percent) | $compressSec | $decompressSec |")
        $rank++
    }
}
$lines.Add('')

$individual = @($okRows | Where-Object { $_.case -like 'single-*' })
$wins = @{}
foreach ($caseGroup in ($individual | Group-Object case)) {
    $winner = @($caseGroup.Group | Sort-Object { [int64]$_.archive_bytes })[0]
    $key = "$($winner.compressor) / $($winner.profile)"
    if (-not $wins.ContainsKey($key)) { $wins[$key] = 0 }
    $wins[$key]++
}
$lines.Add('## Individual-file size wins')
$lines.Add('')
if ($wins.Count -eq 0) {
    $lines.Add('No successful individual-file result was recorded.')
} else {
    $lines.Add('| Compressor / profile | Files won |')
    $lines.Add('|---|---:|')
    $sortedWins = @($wins.GetEnumerator() | Sort-Object -Property @{ Expression = 'Value'; Descending = $true }, @{ Expression = 'Name'; Descending = $false })
    foreach ($item in $sortedWins) {
        $lines.Add("| $($item.Name) | $($item.Value) |")
    }
}
$lines.Add('')

$lines.Add('## Pithos codec contribution probe')
$lines.Add('')
if (Test-Path -LiteralPath $codecPath -PathType Leaf) {
    $codecRows = @(Import-Csv -LiteralPath $codecPath | Where-Object { $_.roundtrip_ok -eq 'true' -or $_.roundtrip_ok -eq 'True' })
    $codecWins = @{}
    foreach ($fileGroup in ($codecRows | Group-Object relative_path)) {
        $winner = @($fileGroup.Group | Sort-Object { [int64]$_.output_bytes })[0]
        if (-not $codecWins.ContainsKey($winner.codec)) { $codecWins[$winner.codec] = 0 }
        $codecWins[$winner.codec]++
    }
    $lines.Add('| Codec | Files | Size wins | Aggregate savings % | Encode total s | Decode total s |')
    $lines.Add('|---|---:|---:|---:|---:|---:|')
    foreach ($codecGroup in ($codecRows | Group-Object codec | Sort-Object Name)) {
        $totalInput = [double](($codecGroup.Group | Measure-Object -Property input_bytes -Sum).Sum)
        $totalOutput = [double](($codecGroup.Group | Measure-Object -Property output_bytes -Sum).Sum)
        $totalEncodeMs = [double](($codecGroup.Group | Measure-Object -Property encode_ms -Sum).Sum)
        $totalDecodeMs = [double](($codecGroup.Group | Measure-Object -Property decode_ms -Sum).Sum)
        $saving = if ($totalInput -eq 0) { 0 } else { [math]::Round((1 - ($totalOutput / $totalInput)) * 100, 4) }
        $winCount = if ($codecWins.ContainsKey($codecGroup.Name)) { $codecWins[$codecGroup.Name] } else { 0 }
        $lines.Add("| $($codecGroup.Name) | $($codecGroup.Count) | $winCount | $saving | $([math]::Round($totalEncodeMs / 1000, 3)) | $([math]::Round($totalDecodeMs / 1000, 3)) |")
    }
    $lines.Add('')
    $lines.Add('This probe runs every mandatory codec directly on every eligible file and verifies byte-exact decode. It isolates codec cost/benefit from grouping and other Pithos stages.')
} else {
    $lines.Add('Codec benchmark was not produced.')
}
$lines.Add('')

$failed = @($rows | Where-Object { $_.status -ne 'ok' })
$lines.Add('## Failed benchmark records')
$lines.Add('')
if ($failed.Count -eq 0) {
    $lines.Add('None.')
} else {
    $lines.Add('| Case | Compressor | Profile | Detail |')
    $lines.Add('|---|---|---|---|')
    foreach ($row in $failed) {
        $detail = ([string]$row.detail).Replace('|', '\|').Replace("`r", ' ').Replace("`n", ' ')
        $lines.Add("| $($row.case) | $($row.compressor) | $($row.profile) | $detail |")
    }
}
$lines.Add('')

$lines.Add('## Current Phase 3 analysis probe')
$lines.Add('')
if (Test-Path -LiteralPath $phasePath -PathType Leaf) {
    $phase = Get-Content -LiteralPath $phasePath -Raw | ConvertFrom-Json
    $analysisMs = [double]$phase.scan_ms + [double]$phase.chunking_ms + [double]$phase.fingerprint_ms + [double]$phase.exact_dedup_ms
    $lines.Add("- Files: $($phase.file_count)")
    $lines.Add("- Input MiB: $([math]::Round(([double]$phase.original_bytes / 1MB), 3))")
    $lines.Add("- Logical chunks: $($phase.chunk_count)")
    $lines.Add("- Exact-dedup referenced chunks: $($phase.referenced_chunks)")
    $lines.Add("- Exact-dedup potential saved MiB: $([math]::Round(([double]$phase.net_saved_bytes / 1MB), 3))")
    $lines.Add("- Exact-dedup potential savings: $([math]::Round([double]$phase.dedup_savings_percent, 4))%")
    $lines.Add('')
    $lines.Add('| Analysis stage | ms | % of measured analysis |')
    $lines.Add('|---|---:|---:|')
    foreach ($stage in @(
        @{ Name = 'scan'; Value = [double]$phase.scan_ms },
        @{ Name = 'chunking'; Value = [double]$phase.chunking_ms },
        @{ Name = 'fingerprinting'; Value = [double]$phase.fingerprint_ms },
        @{ Name = 'exact_dedup'; Value = [double]$phase.exact_dedup_ms }
    )) {
        $percent = if ($analysisMs -eq 0) { 0 } else { [math]::Round(($stage.Value / $analysisMs) * 100, 3) }
        $lines.Add("| $($stage.Name) | $($stage.Value) | $percent |")
    }
    $lines.Add('')
    $lines.Add('Exact-dedup savings are format-neutral potential until ChunkTable persistence is integrated into PAF.')
} else {
    $lines.Add('Phase-analysis summary was not produced.')
}
$lines.Add('')

[System.IO.File]::WriteAllLines($outPath, $lines, (New-Object System.Text.UTF8Encoding -ArgumentList $false))
Write-Host "Benchmark summary: $outPath" -ForegroundColor Green
