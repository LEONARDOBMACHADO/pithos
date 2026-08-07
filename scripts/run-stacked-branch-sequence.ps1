#requires -Version 5.1
param(
    [Parameter(Mandatory=$true)][string]$Repo,
    [Parameter(Mandatory=$true)][string]$Runner,
    [string]$Corpus = 'tst_compact',
    [string]$ExternalEvidenceRoot = 'C:\PithosStack\evidence',
    [string]$StartAtBranch = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$allBranches = @(
    'perf/01-group-decode-once',
    'perf/02-adaptive-pack',
    'feat/03-native-exact-dedup',
    'feat/04-native-similarity-delta',
    'feat/05-native-reference-graph',
    'feat/06-native-canonicalization',
    'feat/07-native-recompression',
    'feat/08-native-grammar-residual',
    'feat/09-native-synthetic-math',
    'feat/10-native-nested-deflate',
    'perf/11-direct-native-pack',
    'perf/12-fused-native-selector',
    'perf/13-prescreen-parallel-pack',
    'feat/14-native-cluster-reorder',
    'perf/15-parallel-clusters'
)

$branches = @($allBranches)
if (-not [string]::IsNullOrWhiteSpace($StartAtBranch)) {
    $startIndex = -1
    for ($index = 0; $index -lt $allBranches.Count; $index++) {
        if ($allBranches[$index] -eq $StartAtBranch) { $startIndex = $index; break }
    }
    if ($startIndex -lt 0) { throw "Unknown StartAtBranch '$StartAtBranch'." }
    $branches = @($allBranches[$startIndex..($allBranches.Count - 1)])
}
$expectedBranches = $branches.Count

$repoPath = (Resolve-Path -LiteralPath $Repo).Path
$runnerPath = (Resolve-Path -LiteralPath $Runner).Path
New-Item -ItemType Directory -Force -Path $ExternalEvidenceRoot | Out-Null
$sessionRoot = Join-Path $ExternalEvidenceRoot ("sequence-" + (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ'))
New-Item -ItemType Directory -Force -Path $sessionRoot | Out-Null
$sequenceRows = New-Object System.Collections.Generic.List[object]
$currentBranch = ''
$currentSha = ''
$currentLog = ''

function Invoke-NativeStep {
    param([Parameter(Mandatory=$true)][string]$Label,[Parameter(Mandatory=$true)][scriptblock]$Action,[Parameter(Mandatory=$true)][string]$Log)
    "`n===== $Label =====" | Tee-Object -FilePath $Log -Append | Write-Host
    $previousErrorActionPreference = $ErrorActionPreference
    $nativeOutput = @(); $nativeException = $null; $code = 0
    try {
        $ErrorActionPreference = 'Continue'; $global:LASTEXITCODE = 0
        $nativeOutput = @(& $Action 2>&1); $code = $LASTEXITCODE
        if ($null -eq $code) { $code = 0 }
    } catch { $nativeException = $_; $code = 1 }
    finally { $ErrorActionPreference = $previousErrorActionPreference }
    if ($nativeOutput.Count -gt 0) { $nativeOutput | Tee-Object -FilePath $Log -Append | Write-Host }
    if ($null -ne $nativeException) { $nativeException | Tee-Object -FilePath $Log -Append | Write-Host; throw "$Label raised a PowerShell exception: $($nativeException.Exception.Message)" }
    if ($code -ne 0) { throw "$Label failed with exit code $code" }
}

function Convert-BenchmarkDouble {
    param([Parameter(Mandatory=$true)][object]$Value,[Parameter(Mandatory=$true)][string]$Field)
    if ($Value -is [System.Array]) { throw "Benchmark field '$Field' is not scalar; received $($Value.Count) values." }
    $text = [string]$Value
    if ([string]::IsNullOrWhiteSpace($text) -or $text -eq 'System.Object[]') { throw "Benchmark field '$Field' is not a numeric scalar: '$text'" }
    $parsed = 0.0
    $ok = [double]::TryParse($text,[System.Globalization.NumberStyles]::Float,[System.Globalization.CultureInfo]::InvariantCulture,[ref]$parsed)
    if (-not $ok -or [double]::IsNaN($parsed) -or [double]::IsInfinity($parsed) -or $parsed -lt 0) { throw "Benchmark field '$Field' is not a valid non-negative number: '$text'" }
    return [double]$parsed
}

function Convert-BenchmarkInt64 {
    param([Parameter(Mandatory=$true)][object]$Value,[Parameter(Mandatory=$true)][string]$Field)
    if ($Value -is [System.Array]) { throw "Benchmark field '$Field' is not scalar; received $($Value.Count) values." }
    $text = [string]$Value; $parsed = [int64]0
    $ok = [int64]::TryParse($text,[System.Globalization.NumberStyles]::Integer,[System.Globalization.CultureInfo]::InvariantCulture,[ref]$parsed)
    if (-not $ok -or $parsed -lt 0) { throw "Benchmark field '$Field' is not a valid non-negative integer: '$text'" }
    return [int64]$parsed
}

function Assert-PreRunStatusClean {
    $lines = @(& git status --porcelain)
    $unexpected = @($lines | Where-Object { $_ -notmatch 'docs/gates/GATE_A_EVIDENCE\.md$' -and $_ -notmatch 'docs/gates/GATE_B_EVIDENCE\.md$' })
    if ($unexpected.Count -gt 0) { throw "Unexpected pre-run git status:`n$($unexpected -join [Environment]::NewLine)" }
}

function Assert-ProhibitedPathsNotStaged {
    $staged = @(& git diff --cached --name-only)
    $bad = @($staged | Where-Object { $_ -eq 'docs/gates/GATE_A_EVIDENCE.md' -or $_ -eq 'docs/gates/GATE_B_EVIDENCE.md' -or $_ -like 'tst_compact/*' })
    if ($bad.Count -gt 0) { throw "Prohibited paths staged:`n$($bad -join [Environment]::NewLine)" }
}

try {
    Set-Location $repoPath
    "requested_start_branch=$($branches[0])" | Set-Content -LiteralPath (Join-Path $sessionRoot 'SEQUENCE_SCOPE.txt') -Encoding UTF8
    "requested_branch_count=$expectedBranches" | Add-Content -LiteralPath (Join-Path $sessionRoot 'SEQUENCE_SCOPE.txt') -Encoding UTF8

    Invoke-NativeStep -Label 'git fetch origin' -Log (Join-Path $sessionRoot '00-git-fetch.log') -Action { & git fetch origin }
    Assert-PreRunStatusClean

    foreach ($branch in $branches) {
        $currentBranch = $branch; $currentSha = ''
        $safeBranch = $branch.Replace('/','-')
        $currentLog = Join-Path $sessionRoot ("$safeBranch.log")
        "Pithos stacked benchmark branch: $branch" | Set-Content -LiteralPath $currentLog -Encoding UTF8

        # Branches 05-15 were intentionally rebuilt as a true cumulative stack.
        # A developer machine may still have stale local branch refs from the
        # pre-rebuild history. With a clean worktree, -C safely makes the local
        # test branch exactly match the authoritative fetched remote branch.
        Invoke-NativeStep -Label "sync $branch to origin" -Log $currentLog -Action {
            & git switch -C $branch "origin/$branch"
        }
        $currentSha = (& git rev-parse HEAD).Trim()
        $remoteSha = (& git rev-parse "origin/$branch").Trim()
        if ($currentSha -ne $remoteSha) { throw "Local/remote SHA mismatch for ${branch}: local=$currentSha remote=$remoteSha" }
        "source_sha=$currentSha" | Tee-Object -FilePath $currentLog -Append | Write-Host
        Assert-PreRunStatusClean

        Invoke-NativeStep -Label 'cargo fmt --all' -Log $currentLog -Action { & cargo fmt --all }
        Invoke-NativeStep -Label 'cargo fmt check' -Log $currentLog -Action { & cargo fmt --all -- --check }
        Invoke-NativeStep -Label 'workspace build' -Log $currentLog -Action { & cargo build --workspace --all-targets --all-features }
        Invoke-NativeStep -Label 'workspace tests' -Log $currentLog -Action { & cargo test --workspace --all-targets --all-features -- --nocapture }
        Invoke-NativeStep -Label 'doc tests' -Log $currentLog -Action { & cargo test --workspace --all-features --doc -- --nocapture }
        Invoke-NativeStep -Label 'release CLI build' -Log $currentLog -Action { & cargo build --release -p pithos-cli }

        Remove-Item (Join-Path $repoPath 'tst_compact\results\stack-work') -Recurse -Force -ErrorAction SilentlyContinue
        Invoke-NativeStep -Label 'archive-max Pithos vs 7-Zip benchmark' -Log $currentLog -Action { & $runnerPath -Repo $repoPath -Corpus $Corpus -ExternalEvidenceRoot $ExternalEvidenceRoot }

        $latest = Get-ChildItem (Join-Path $repoPath 'docs\benchmarks\evidence') -Directory -Filter ("stack-$safeBranch-*") | Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
        if ($null -eq $latest) { throw "No versioned stack evidence directory found for $branch" }
        $resultCsv = Join-Path $latest.FullName 'stack-result.csv'; $records = @(Import-Csv -LiteralPath $resultCsv)
        $pithos = $records | Where-Object { $_.compressor -eq 'pithos' } | Select-Object -First 1
        $seven = $records | Where-Object { $_.compressor -eq '7zip' } | Select-Object -First 1
        if ($null -eq $pithos -or $null -eq $seven -or $pithos.status -ne 'ok' -or $seven.status -ne 'ok') { throw "Incomplete benchmark records for $branch" }

        $pithosArchiveBytes = Convert-BenchmarkInt64 -Value $pithos.archive_bytes -Field 'pithos.archive_bytes'
        $sevenArchiveBytes = Convert-BenchmarkInt64 -Value $seven.archive_bytes -Field '7zip.archive_bytes'
        $pithosCompressMs = Convert-BenchmarkDouble -Value $pithos.compress_ms -Field 'pithos.compress_ms'
        $sevenCompressMs = Convert-BenchmarkDouble -Value $seven.compress_ms -Field '7zip.compress_ms'
        $pithosVerifyMs = Convert-BenchmarkDouble -Value $pithos.verify_ms -Field 'pithos.verify_ms'
        $sevenVerifyMs = Convert-BenchmarkDouble -Value $seven.verify_ms -Field '7zip.verify_ms'
        $pithosDecompressMs = Convert-BenchmarkDouble -Value $pithos.decompress_ms -Field 'pithos.decompress_ms'
        $sevenDecompressMs = Convert-BenchmarkDouble -Value $seven.decompress_ms -Field '7zip.decompress_ms'
        $pithosSavings = Convert-BenchmarkDouble -Value $pithos.savings_percent -Field 'pithos.savings_percent'
        $sevenSavings = Convert-BenchmarkDouble -Value $seven.savings_percent -Field '7zip.savings_percent'

        if (Test-Path -LiteralPath (Join-Path $repoPath 'Cargo.lock')) { & git add Cargo.lock }
        & git add crates/
        & git add docs/benchmarks/evidence/
        Assert-ProhibitedPathsNotStaged
        $staged = @(& git diff --cached --name-only)
        if ($staged.Count -eq 0) { throw "No staged formatting/lock/evidence changes for $branch" }
        Invoke-NativeStep -Label 'commit branch evidence' -Log $currentLog -Action { & git commit -m "test: benchmark $branch against 7-Zip" }
        Invoke-NativeStep -Label 'push branch evidence' -Log $currentLog -Action { & git push origin $branch }
        $evidenceSha = (& git rev-parse HEAD).Trim()

        $sequenceRows.Add([pscustomobject]@{
            branch=$branch; source_sha=$currentSha; result_sha=$evidenceSha
            pithos_archive_bytes=$pithosArchiveBytes; sevenzip_archive_bytes=$sevenArchiveBytes
            pithos_compress_ms=$pithosCompressMs; sevenzip_compress_ms=$sevenCompressMs
            pithos_verify_ms=$pithosVerifyMs; sevenzip_verify_ms=$sevenVerifyMs
            pithos_decompress_ms=$pithosDecompressMs; sevenzip_decompress_ms=$sevenDecompressMs
            pithos_savings_percent=$pithosSavings; sevenzip_savings_percent=$sevenSavings; status='PASS'
        })

        $remaining = @(& git status --porcelain | Where-Object { $_ -notmatch 'docs/gates/GATE_A_EVIDENCE\.md$' -and $_ -notmatch 'docs/gates/GATE_B_EVIDENCE\.md$' })
        if ($remaining.Count -gt 0) { throw "Unexpected post-commit status on ${branch}:`n$($remaining -join [Environment]::NewLine)" }
    }

    $summaryCsv = Join-Path $sessionRoot 'STACK_SEQUENCE_SUMMARY.csv'; $sequenceRows | Export-Csv -LiteralPath $summaryCsv -NoTypeInformation -Encoding UTF8
    $summaryTxt = Join-Path $sessionRoot 'STACK_SEQUENCE_SUMMARY.txt'
    @("completed_utc=$((Get-Date).ToUniversalTime().ToString('o'))","start_branch=$($branches[0])","branches_passed=$($sequenceRows.Count)","expected_branches=$expectedBranches",'comparison_scope=pithos-archive-max-vs-7zip-mx9','winrar=INTENTIONALLY_NOT_USED','status=PASS','',($sequenceRows | Format-Table branch,pithos_archive_bytes,sevenzip_archive_bytes,pithos_compress_ms,sevenzip_compress_ms,pithos_decompress_ms,sevenzip_decompress_ms -AutoSize | Out-String)) | Set-Content -LiteralPath $summaryTxt -Encoding UTF8
    Write-Host "`nALL $expectedBranches REQUESTED BRANCHES PASSED" -ForegroundColor Green
    Write-Host "Summary: $summaryCsv"
    exit 0
}
catch {
    $failure = Join-Path $sessionRoot 'STACK_SEQUENCE_FAILURE.txt'
    @("failed_utc=$((Get-Date).ToUniversalTime().ToString('o'))","requested_start_branch=$(if ($branches.Count -gt 0) { $branches[0] } else { '' })","branch=$currentBranch","source_sha=$currentSha","log=$currentLog","error=$($_.Exception.Message)",'','STOP: later branches were NOT tested.','Do not modify source. Return this file and the branch log.') | Set-Content -LiteralPath $failure -Encoding UTF8
    Write-Error "STACK STOPPED on $currentBranch : $($_.Exception.Message). Later branches were NOT tested. Evidence: $failure"
    exit 1
}
