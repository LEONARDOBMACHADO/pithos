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

$baselinePath = Join-Path $repoPath 'docs\benchmarks\7zip-best-baseline.csv'
if (-not (Test-Path -LiteralPath $baselinePath -PathType Leaf)) { throw "Frozen 7-Zip baseline not found: $baselinePath" }
$combinedBaseline = Import-Csv -LiteralPath $baselinePath | Where-Object { $_.case -eq 'combined-all' } | Select-Object -First 1
if ($null -eq $combinedBaseline) { throw 'combined-all row missing from frozen 7-Zip baseline.' }

$pithosExe = Join-Path $repoPath 'target\release\pithos.exe'
if (-not (Test-Path -LiteralPath $pithosExe -PathType Leaf)) { throw "Pithos release binary not found: $pithosExe" }

$timestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$branch = (& git branch --show-current).Trim()
$sha = (& git rev-parse HEAD).Trim()
$safeBranch = $branch.Replace('/','-')
$evidenceDir = Join-Path $repoPath "docs\benchmarks\evidence\r4-$safeBranch-$timestamp"
New-Item -ItemType Directory -Force -Path $evidenceDir | Out-Null

$resultsPath = Join-Path $corpusPath 'results'
$workRoot = Join-Path ([System.IO.Path]::GetTempPath()) "pithos-r4-$timestamp"
$inputPath = Join-Path $workRoot 'input'
$pithosOut = Join-Path $workRoot 'pithos-out'
$pits = Join-Path $workRoot 'combined.pits'
New-Item -ItemType Directory -Force -Path $inputPath | Out-Null

try {
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

    function Get-TreeDigest {
        param([Parameter(Mandatory=$true)][string]$Root)
        $rows = @(Get-ChildItem -LiteralPath $Root -File -Recurse | Sort-Object FullName | ForEach-Object {
            $relative = $_.FullName.Substring($Root.Length).TrimStart([char[]]@([char]92,[char]47)).Replace([char]92,[char]47)
            $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            "$relative|$($_.Length)|$hash"
        })
        $payload = [System.Text.Encoding]::UTF8.GetBytes(($rows -join "`n"))
        $sha256 = [System.Security.Cryptography.SHA256]::Create()
        try { return (($sha256.ComputeHash($payload) | ForEach-Object { $_.ToString('x2') }) -join '') }
        finally { $sha256.Dispose() }
    }

    function Invoke-TimedNative {
        param(
            [Parameter(Mandatory=$true)][string]$Exe,
            [Parameter(Mandatory=$true)][string[]]$Arguments,
            [Parameter(Mandatory=$true)][string]$Label,
            [Parameter(Mandatory=$true)][string]$LogPath
        )

        $code = $null
        $watch = [System.Diagnostics.Stopwatch]::StartNew()

        # Windows PowerShell 5.1 converts native stderr redirected with
        # 2>&1 into PowerShell error records. Pithos intentionally emits
        # PITHOS_REP_TRACE through stderr, so ErrorActionPreference=Stop
        # must not be active while executing the native process.
        #
        # Success/failure of a native executable is determined exclusively
        # by its process exit code, not by whether it produced stderr.
        $previousErrorActionPreference = $ErrorActionPreference

        try {
            $ErrorActionPreference = 'Continue'

            & $Exe @Arguments 2>&1 |
                Tee-Object -FilePath $LogPath -Append |
                ForEach-Object { Write-Host $_ }

            # Capture immediately, before executing another native command.
            $code = $LASTEXITCODE
        }
        finally {
            $ErrorActionPreference = $previousErrorActionPreference
            $watch.Stop()
        }

        if ($null -eq $code) {
            throw "$Label completed without a native exit code"
        }

        if ($code -ne 0) {
            throw "$Label failed with exit code $code; log=$LogPath"
        }

        return [double][math]::Round($watch.Elapsed.TotalMilliseconds, 3)
    }

    $expectedDigest = Get-TreeDigest -Root $inputPath
    $originalBytes = [int64](($sourceFiles | Measure-Object -Property Length -Sum).Sum)
    if ($originalBytes -ne [int64]$combinedBaseline.original_bytes) {
        throw "Corpus size differs from frozen baseline: actual=$originalBytes baseline=$($combinedBaseline.original_bytes)"
    }

    Copy-Item -LiteralPath $baselinePath -Destination (Join-Path $evidenceDir '7zip-best-baseline.csv') -Force

    Write-Host ''
    Write-Host '=== Full ArchiveMax representation trace: Pithos only ===' -ForegroundColor Cyan
    Write-Host 'Trace includes ClassAware vs Global and v17 vs historical v12 floor per native call.' -ForegroundColor Cyan
    $plannerLog = Join-Path $evidenceDir 'planner-trace.log'
    $previousTrace = $env:PITHOS_REP_TRACE
    $env:PITHOS_REP_TRACE = '1'
    try {
        $compressMs = Invoke-TimedNative -Exe $pithosExe -Arguments @('pack',$inputPath,'--profile','archive-max','--output',$pits) -Label 'Pithos compress' -LogPath $plannerLog
    } finally {
        $env:PITHOS_REP_TRACE = $previousTrace
    }

    $verifyLog = Join-Path $evidenceDir 'verify.log'
    $verifyMs = Invoke-TimedNative -Exe $pithosExe -Arguments @('verify',$pits) -Label 'Pithos verify' -LogPath $verifyLog

    $unpackLog = Join-Path $evidenceDir 'unpack.log'
    $decompressMs = Invoke-TimedNative -Exe $pithosExe -Arguments @('unpack',$pits,'--output',$pithosOut) -Label 'Pithos unpack' -LogPath $unpackLog

    $actualDigest = Get-TreeDigest -Root $pithosOut
    if ($actualDigest -ne $expectedDigest) {
        throw "Pithos byte-exact tree mismatch: expected=$expectedDigest actual=$actualDigest"
    }

    $archiveBytes = [int64](Get-Item -LiteralPath $pits).Length
    $baselineBytes = [int64]$combinedBaseline.archive_bytes
    $baselineCompressMs = [double]::Parse($combinedBaseline.compress_ms, [System.Globalization.CultureInfo]::InvariantCulture)
    $baselineVerifyMs = [double]::Parse($combinedBaseline.verify_ms, [System.Globalization.CultureInfo]::InvariantCulture)
    $baselineDecompressMs = [double]::Parse($combinedBaseline.decompress_ms, [System.Globalization.CultureInfo]::InvariantCulture)

    $traceLines = @(Get-Content -LiteralPath $plannerLog | Where-Object { $_ -like '*PITHOS_REP_TRACE*' })
    $nativeRaceLines = @($traceLines | Where-Object { $_ -like '*stage=native_floor_race*' })
    $archiveCandidateLines = @($traceLines | Where-Object { $_ -like '*stage=archive_candidate*' })
    $winnerLines = @($traceLines | Where-Object { $_ -like '*stage=archive_winner*' })
    $traceLines | Set-Content -LiteralPath (Join-Path $evidenceDir 'representation-trace-only.log') -Encoding UTF8

    $result = [pscustomobject]@{
        branch = $branch
        commit = $sha
        original_bytes = $originalBytes
        archive_bytes = $archiveBytes
        savings_percent = [math]::Round((1.0 - ($archiveBytes / [double]$originalBytes)) * 100, 6)
        compress_ms = $compressMs
        verify_ms = $verifyMs
        decompress_ms = $decompressMs
        tree_sha256 = $actualDigest
        native_floor_races = $nativeRaceLines.Count
        archive_candidate_records = $archiveCandidateLines.Count
        archive_winner_records = $winnerLines.Count
        sevenzip_best_archive_bytes = $baselineBytes
        delta_bytes_vs_7zip_best = $archiveBytes - $baselineBytes
        sevenzip_best_compress_ms = $baselineCompressMs
        compress_ratio_vs_7zip_best = [math]::Round($compressMs / $baselineCompressMs, 6)
        sevenzip_best_verify_ms = $baselineVerifyMs
        verify_ratio_vs_7zip_best = [math]::Round($verifyMs / $baselineVerifyMs, 6)
        sevenzip_best_decompress_ms = $baselineDecompressMs
        decompress_ratio_vs_7zip_best = [math]::Round($decompressMs / $baselineDecompressMs, 6)
        sevenzip_executed = $false
        status = 'ok'
    }

    $result | Export-Csv -LiteralPath (Join-Path $evidenceDir 'r4-result.csv') -NoTypeInformation -Encoding UTF8
    $result | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $evidenceDir 'r4-result.json') -Encoding UTF8

    @(
        "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
        "branch=$branch",
        "commit=$sha",
        "corpus_files=$($sourceFiles.Count)",
        "original_bytes=$originalBytes",
        "pithos_archive_bytes=$archiveBytes",
        "pithos_compress_ms=$compressMs",
        "pithos_verify_ms=$verifyMs",
        "pithos_decompress_ms=$decompressMs",
        "native_floor_races=$($nativeRaceLines.Count)",
        "archive_candidate_records=$($archiveCandidateLines.Count)",
        "archive_winner_records=$($winnerLines.Count)",
        "7zip_frozen_archive_bytes=$baselineBytes",
        "7zip_frozen_compress_ms=$baselineCompressMs",
        "7zip_frozen_verify_ms=$baselineVerifyMs",
        "7zip_frozen_decompress_ms=$baselineDecompressMs",
        "delta_bytes_vs_7zip_best=$($archiveBytes - $baselineBytes)",
        '7zip_executed=False',
        "tree_sha256=$actualDigest",
        'status=PASS'
    ) | Set-Content -LiteralPath (Join-Path $evidenceDir 'R4_RESULT.txt') -Encoding UTF8

    if (-not [string]::IsNullOrWhiteSpace($ExternalEvidenceRoot)) {
        New-Item -ItemType Directory -Force -Path $ExternalEvidenceRoot | Out-Null
        Copy-Item -LiteralPath $evidenceDir -Destination (Join-Path $ExternalEvidenceRoot (Split-Path $evidenceDir -Leaf)) -Recurse -Force
    }

    Write-Host ''
    Write-Host 'R4 representation trace PASS' -ForegroundColor Green
    $result | Format-List | Out-String | Write-Host
    Write-Host 'Representation trace:' -ForegroundColor Green
    $traceLines | ForEach-Object { Write-Host $_ }
    Write-Host "Evidence: $evidenceDir"
    exit 0
}
catch {
    $failure = Join-Path $evidenceDir 'R4_FAILURE.txt'
    @(
        "failed_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
        "branch=$branch",
        "commit=$sha",
        "error=$($_.Exception.Message)",
        '7zip_executed=False',
        'STOP: do not modify Rust manually; return this evidence directory.'
    ) | Set-Content -LiteralPath $failure -Encoding UTF8
    Write-Host $_ -ForegroundColor Red
    Write-Host "Failure evidence: $failure" -ForegroundColor Red
    exit 1
}
finally {
    if (Test-Path -LiteralPath $workRoot) {
        Remove-Item -LiteralPath $workRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
