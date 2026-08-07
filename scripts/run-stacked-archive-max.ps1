#requires -Version 5.1
param(
    [Parameter(Mandatory=$true)][string]$Repo,
    [string]$Corpus = "tst_compact",
    [string]$ExternalEvidenceRoot = ""
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoPath = (Resolve-Path -LiteralPath $Repo).Path
Set-Location $repoPath
$corpusPath = if ([System.IO.Path]::IsPathRooted($Corpus)) { $Corpus } else { Join-Path $repoPath $Corpus }
if (-not (Test-Path -LiteralPath $corpusPath -PathType Container)) { throw "Corpus directory not found: $corpusPath" }
$resultsPath = Join-Path $corpusPath 'results'
New-Item -ItemType Directory -Force -Path $resultsPath | Out-Null
$workPath = Join-Path $resultsPath 'stack-work'
if (Test-Path -LiteralPath $workPath) { Remove-Item -LiteralPath $workPath -Recurse -Force }
New-Item -ItemType Directory -Force -Path $workPath | Out-Null
$inputPath = Join-Path $workPath 'input'
$pithosOut = Join-Path $workPath 'pithos-out'
$sevenOut = Join-Path $workPath '7zip-out'
New-Item -ItemType Directory -Force -Path $inputPath | Out-Null

$sourceFiles = @(Get-ChildItem -LiteralPath $corpusPath -File -Recurse |
    Where-Object { -not $_.FullName.StartsWith($resultsPath, [System.StringComparison]::OrdinalIgnoreCase) } |
    Sort-Object FullName)
if ($sourceFiles.Count -eq 0) { throw 'Frozen corpus is empty.' }
foreach ($file in $sourceFiles) {
    $relative = $file.FullName.Substring($corpusPath.Length).TrimStart([char[]]@([char]92,[char]47))
    $destination = Join-Path $inputPath $relative
    $parent = Split-Path -Parent $destination
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    try { New-Item -ItemType HardLink -Path $destination -Target $file.FullName -ErrorAction Stop | Out-Null }
    catch { Copy-Item -LiteralPath $file.FullName -Destination $destination -Force }
}

$sevenZip = $null
foreach ($candidate in @(
    (Get-Command 7z -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source),
    (Join-Path ([System.Environment]::GetFolderPath('ProgramFiles')) '7-Zip\7z.exe')
)) {
    if (-not [string]::IsNullOrWhiteSpace($candidate) -and (Test-Path -LiteralPath $candidate -PathType Leaf)) { $sevenZip = $candidate; break }
}
if ($null -eq $sevenZip) { throw '7-Zip CLI not found.' }

$pithosExe = Join-Path $repoPath 'target\release\pithos.exe'
if (-not (Test-Path -LiteralPath $pithosExe -PathType Leaf)) { throw "Pithos release binary not found: $pithosExe. Build it before running this benchmark." }

function Invoke-Timed {
    param([Parameter(Mandatory=$true)][scriptblock]$Action,[Parameter(Mandatory=$true)][string]$Label)
    $previousErrorActionPreference = $ErrorActionPreference
    $nativeOutput = @(); $nativeException = $null; $code = 0
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $ErrorActionPreference = 'Continue'; $global:LASTEXITCODE = 0
        $nativeOutput = @(& $Action 2>&1); $code = $LASTEXITCODE
        if ($null -eq $code) { $code = 0 }
    } catch { $nativeException = $_; $code = 1 }
    finally { $watch.Stop(); $ErrorActionPreference = $previousErrorActionPreference }
    foreach ($line in $nativeOutput) { Write-Host $line }
    if ($null -ne $nativeException) { Write-Host $nativeException; throw "$Label raised a PowerShell exception: $($nativeException.Exception.Message)" }
    if ($code -ne 0) { throw "$Label failed with exit code $code" }
    $elapsed = [double][math]::Round($watch.Elapsed.TotalMilliseconds, 3)
    Write-Output -NoEnumerate $elapsed
}

function Assert-ScalarFiniteNumber {
    param([Parameter(Mandatory=$true)][object]$Value,[Parameter(Mandatory=$true)][string]$Label)
    if ($Value -is [System.Array]) { throw "$Label is not scalar; captured $($Value.Count) values." }
    $text = [string]$Value; $parsed = 0.0
    $ok = [double]::TryParse($text,[System.Globalization.NumberStyles]::Float,[System.Globalization.CultureInfo]::InvariantCulture,[ref]$parsed)
    if (-not $ok -or [double]::IsNaN($parsed) -or [double]::IsInfinity($parsed) -or $parsed -lt 0) { throw "$Label is not a valid non-negative scalar number: '$text'" }
    return [double]$parsed
}

function Get-TreeDigest {
    param([Parameter(Mandatory=$true)][string]$Root)
    $rows = @(Get-ChildItem -LiteralPath $Root -File -Recurse | Sort-Object FullName | ForEach-Object {
        $relative = $_.FullName.Substring($Root.Length).TrimStart([char[]]@([char]92,[char]47)).Replace([char]92,[char]47)
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        "$relative|$($_.Length)|$hash"
    })
    $payload = [System.Text.Encoding]::UTF8.GetBytes(($rows -join "`n")); $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return (($sha.ComputeHash($payload) | ForEach-Object { $_.ToString('x2') }) -join '') } finally { $sha.Dispose() }
}

$expectedDigest = Get-TreeDigest -Root $inputPath
$originalBytes = [int64](($sourceFiles | Measure-Object -Property Length -Sum).Sum)
$pits = Join-Path $workPath 'combined.pits'; $seven = Join-Path $workPath 'combined.7z'

Write-Host '=== Pithos archive-max ===' -ForegroundColor Cyan
$pithosCompressMs = Invoke-Timed -Label 'Pithos compress' -Action { & $pithosExe pack $inputPath --profile archive-max --output $pits }
$pithosVerifyMs = Invoke-Timed -Label 'Pithos verify' -Action { & $pithosExe verify $pits }
$pithosDecompressMs = Invoke-Timed -Label 'Pithos unpack' -Action { & $pithosExe unpack $pits --output $pithosOut }
$pithosCompressMs = Assert-ScalarFiniteNumber -Value $pithosCompressMs -Label 'pithos compress_ms'
$pithosVerifyMs = Assert-ScalarFiniteNumber -Value $pithosVerifyMs -Label 'pithos verify_ms'
$pithosDecompressMs = Assert-ScalarFiniteNumber -Value $pithosDecompressMs -Label 'pithos decompress_ms'
$pithosDigest = Get-TreeDigest -Root $pithosOut
if ($pithosDigest -ne $expectedDigest) { throw "Pithos byte-exact tree mismatch: expected=$expectedDigest actual=$pithosDigest" }

Write-Host '=== 7-Zip LZMA2 mx9 solid ===' -ForegroundColor Cyan
$sevenCompressMs = Invoke-Timed -Label '7-Zip compress' -Action { & $sevenZip a -t7z $seven (Join-Path $inputPath '*') -m0=lzma2 -mx=9 -ms=on -y }
$sevenVerifyMs = Invoke-Timed -Label '7-Zip test' -Action { & $sevenZip t $seven -y }
New-Item -ItemType Directory -Force -Path $sevenOut | Out-Null
$sevenDecompressMs = Invoke-Timed -Label '7-Zip extract' -Action { & $sevenZip x $seven ("-o$sevenOut") -y }
$sevenCompressMs = Assert-ScalarFiniteNumber -Value $sevenCompressMs -Label '7zip compress_ms'
$sevenVerifyMs = Assert-ScalarFiniteNumber -Value $sevenVerifyMs -Label '7zip verify_ms'
$sevenDecompressMs = Assert-ScalarFiniteNumber -Value $sevenDecompressMs -Label '7zip decompress_ms'
$sevenDigest = Get-TreeDigest -Root $sevenOut
if ($sevenDigest -ne $expectedDigest) { throw "7-Zip byte-exact tree mismatch: expected=$expectedDigest actual=$sevenDigest" }

$pithosBytes = (Get-Item -LiteralPath $pits).Length; $sevenBytes = (Get-Item -LiteralPath $seven).Length
$timestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ'); $branch = (& git branch --show-current).Trim(); $sha = (& git rev-parse HEAD).Trim(); $safeBranch = $branch.Replace('/','-')
$evidenceDir = Join-Path $repoPath "docs\benchmarks\evidence\stack-$safeBranch-$timestamp"
New-Item -ItemType Directory -Force -Path $evidenceDir | Out-Null
$records = @(
    [pscustomobject]@{branch=$branch;commit=$sha;compressor='pithos';profile='archive-max';original_bytes=$originalBytes;archive_bytes=$pithosBytes;savings_percent=[math]::Round((1.0-($pithosBytes/[double]$originalBytes))*100,6);compress_ms=[double]$pithosCompressMs;verify_ms=[double]$pithosVerifyMs;decompress_ms=[double]$pithosDecompressMs;tree_sha256=$pithosDigest;status='ok'},
    [pscustomobject]@{branch=$branch;commit=$sha;compressor='7zip';profile='lzma2-mx9-solid';original_bytes=$originalBytes;archive_bytes=$sevenBytes;savings_percent=[math]::Round((1.0-($sevenBytes/[double]$originalBytes))*100,6);compress_ms=[double]$sevenCompressMs;verify_ms=[double]$sevenVerifyMs;decompress_ms=[double]$sevenDecompressMs;tree_sha256=$sevenDigest;status='ok'}
)
$csv = Join-Path $evidenceDir 'stack-result.csv'; $records | Export-Csv -LiteralPath $csv -NoTypeInformation -Encoding UTF8
$json = Join-Path $evidenceDir 'stack-result.json'; $records | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $json -Encoding UTF8
$summary = Join-Path $evidenceDir 'STACK_RESULT.txt'
@("timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))","branch=$branch","commit=$sha","corpus_files=$($sourceFiles.Count)","original_bytes=$originalBytes","cleanup_generated_work_before_run=True","comparison_scope=pithos-archive-max-vs-7zip-mx9","winrar=INTENTIONALLY_NOT_USED","pithos_archive_bytes=$pithosBytes","pithos_compress_ms=$pithosCompressMs","pithos_verify_ms=$pithosVerifyMs","pithos_decompress_ms=$pithosDecompressMs","7zip_archive_bytes=$sevenBytes","7zip_compress_ms=$sevenCompressMs","7zip_verify_ms=$sevenVerifyMs","7zip_decompress_ms=$sevenDecompressMs","tree_sha256=$expectedDigest",'status=PASS') | Set-Content -LiteralPath $summary -Encoding UTF8
if (-not [string]::IsNullOrWhiteSpace($ExternalEvidenceRoot)) { New-Item -ItemType Directory -Force -Path $ExternalEvidenceRoot | Out-Null; Copy-Item -LiteralPath $evidenceDir -Destination (Join-Path $ExternalEvidenceRoot (Split-Path $evidenceDir -Leaf)) -Recurse -Force }
Write-Host "`nBranch benchmark PASS" -ForegroundColor Green
$records | Format-Table compressor,profile,archive_bytes,savings_percent,compress_ms,verify_ms,decompress_ms -AutoSize | Out-String | Write-Host
Write-Host "Evidence: $evidenceDir"
exit 0
