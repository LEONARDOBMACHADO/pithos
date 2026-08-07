#requires -Version 5.1
param(
    [string]$Corpus = "tst_compact"
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
$manifestPath = Join-Path $resultsPath 'corpus-manifest.csv'
$summaryPath = Join-Path $resultsPath 'corpus-summary.txt'

$files = Get-ChildItem -LiteralPath $corpusPath -File -Recurse |
    Where-Object { -not $_.FullName.StartsWith($resultsPath, [System.StringComparison]::OrdinalIgnoreCase) } |
    Sort-Object FullName

$rows = foreach ($file in $files) {
    $relative = $file.FullName.Substring($corpusPath.Length).TrimStart([char[]]@([char]92, [char]47))
    $hash = Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256
    [pscustomobject]@{
        relative_path = $relative.Replace([char]92, [char]47)
        extension = $file.Extension.ToLowerInvariant()
        bytes = [int64]$file.Length
        mib = [math]::Round($file.Length / 1MB, 3)
        sha256 = $hash.Hash.ToLowerInvariant()
        modified_utc = $file.LastWriteTimeUtc.ToString('o')
    }
}

$rows | Export-Csv -LiteralPath $manifestPath -NoTypeInformation -Encoding UTF8

$totalBytes = [int64](($rows | Measure-Object -Property bytes -Sum).Sum)
$count = @($rows).Count
$averageBytes = if ($count -gt 0) { [double]$totalBytes / $count } else { 0 }
$extensions = @($rows | Group-Object extension | Sort-Object Name)

$summary = New-Object System.Collections.Generic.List[string]
$summary.Add("corpus=$corpusPath")
$summary.Add("file_count=$count")
$summary.Add("total_bytes=$totalBytes")
$summary.Add("total_mib=$([math]::Round($totalBytes / 1MB, 3))")
$summary.Add("average_mib=$([math]::Round($averageBytes / 1MB, 3))")
$summary.Add("extension_count=$($extensions.Count)")
$summary.Add('')
$summary.Add('extensions:')
foreach ($group in $extensions) {
    $extensionBytes = [int64](($group.Group | Measure-Object -Property bytes -Sum).Sum)
    $summary.Add("$($group.Name),count=$($group.Count),mib=$([math]::Round($extensionBytes / 1MB, 3))")
}

[System.IO.File]::WriteAllLines($summaryPath, $summary, (New-Object System.Text.UTF8Encoding -ArgumentList $false))

Write-Host "Corpus inventory complete" -ForegroundColor Green
Write-Host "Files: $count"
Write-Host "Total MiB: $([math]::Round($totalBytes / 1MB, 3))"
Write-Host "Average MiB: $([math]::Round($averageBytes / 1MB, 3))"
Write-Host "Manifest: $manifestPath"
Write-Host "Summary: $summaryPath"
