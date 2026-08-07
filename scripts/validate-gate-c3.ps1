#requires -Version 5.1

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repo

$timestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$evidenceDir = Join-Path $repo "docs/gates/evidence/gate-c3-$timestamp"
New-Item -ItemType Directory -Force -Path $evidenceDir | Out-Null

$utf8NoBom = New-Object System.Text.UTF8Encoding -ArgumentList $false
$script:summary = @()

function Write-Utf8Text {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text
    )

    [System.IO.File]::WriteAllText($Path, $Text, $utf8NoBom)
}

function Write-Utf8Lines {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$Lines
    )

    [System.IO.File]::WriteAllLines($Path, $Lines, $utf8NoBom)
}

function Invoke-NativeCapture {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Arguments
    )

    $command = Get-Command $FilePath -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $command) {
        return [pscustomobject]@{
            ExitCode = 127
            StdOut = ''
            StdErr = "COMMAND_NOT_FOUND: $FilePath"
        }
    }

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $command.Source
    $psi.Arguments = $Arguments
    $psi.WorkingDirectory = $repo
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $psi

    try {
        if (-not $process.Start()) {
            throw "Failed to start $FilePath"
        }

        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()

        return [pscustomobject]@{
            ExitCode = [int]$process.ExitCode
            StdOut = [string]$stdoutTask.Result
            StdErr = [string]$stderrTask.Result
        }
    }
    catch {
        return [pscustomobject]@{
            ExitCode = 126
            StdOut = ''
            StdErr = "PROCESS_START_FAILED: $($_.Exception.Message)"
        }
    }
    finally {
        $process.Dispose()
    }
}

function Get-NativeText {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Arguments
    )

    $result = Invoke-NativeCapture -FilePath $FilePath -Arguments $Arguments
    $combined = @()
    if (-not [string]::IsNullOrWhiteSpace($result.StdOut)) {
        $combined += $result.StdOut.TrimEnd("`r", "`n")
    }
    if (-not [string]::IsNullOrWhiteSpace($result.StdErr)) {
        $combined += $result.StdErr.TrimEnd("`r", "`n")
    }
    if ($combined.Count -eq 0) {
        return ''
    }
    return ($combined -join ' | ')
}

function Invoke-EvidenceCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Arguments
    )

    $safeName = ($Name -replace '[^A-Za-z0-9._-]', '_')
    $logPath = Join-Path $evidenceDir "$safeName.log"
    $metaPath = Join-Path $evidenceDir "$safeName.meta.txt"
    $displayCommand = if ([string]::IsNullOrWhiteSpace($Arguments)) { $FilePath } else { "$FilePath $Arguments" }

    Write-Host "`n=== $Name ===" -ForegroundColor Cyan
    Write-Host $displayCommand

    $started = (Get-Date).ToUniversalTime()
    $result = Invoke-NativeCapture -FilePath $FilePath -Arguments $Arguments
    $ended = (Get-Date).ToUniversalTime()

    $logParts = @()
    if (-not [string]::IsNullOrEmpty($result.StdOut)) {
        $logParts += "[stdout]`r`n$($result.StdOut.TrimEnd("`r", "`n"))"
    }
    if (-not [string]::IsNullOrEmpty($result.StdErr)) {
        $logParts += "[stderr]`r`n$($result.StdErr.TrimEnd("`r", "`n"))"
    }
    if ($logParts.Count -eq 0) {
        $logParts += '[no output]'
    }
    $logText = ($logParts -join "`r`n") + "`r`n"
    Write-Utf8Text -Path $logPath -Text $logText

    if (-not [string]::IsNullOrWhiteSpace($result.StdOut)) {
        Write-Host $result.StdOut.TrimEnd("`r", "`n")
    }
    if (-not [string]::IsNullOrWhiteSpace($result.StdErr)) {
        Write-Host $result.StdErr.TrimEnd("`r", "`n")
    }

    Write-Utf8Lines -Path $metaPath -Lines @(
        "name=$Name",
        "command=$displayCommand",
        "started_utc=$($started.ToString('o'))",
        "ended_utc=$($ended.ToString('o'))",
        "exit_code=$($result.ExitCode)"
    )

    $trimChars = [char[]]@([char]92, [char]47)
    $relativeLog = $logPath.Substring($repo.Length).TrimStart($trimChars).Replace('\', '/')
    $script:summary += [pscustomobject]@{
        Name = $Name
        ExitCode = [int]$result.ExitCode
        Log = $relativeLog
    }
}

$psEdition = 'Desktop'
if ($PSVersionTable.PSObject.Properties.Name -contains 'PSEdition') {
    $psEdition = [string]$PSVersionTable.PSEdition
}

$environmentLines = @(
    "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
    "repo=$repo",
    "powershell_version=$($PSVersionTable.PSVersion.ToString())",
    "powershell_edition=$psEdition",
    "branch=$(Get-NativeText -FilePath 'git' -Arguments 'branch --show-current')",
    "commit=$(Get-NativeText -FilePath 'git' -Arguments 'rev-parse HEAD')",
    "os=$([System.Environment]::OSVersion.VersionString)",
    "is_64bit_os=$([System.Environment]::Is64BitOperatingSystem)",
    "is_64bit_process=$([System.Environment]::Is64BitProcess)",
    'gate_c3_windows_fuzz_sanitizer=none',
    'gate_c3_windows_fuzz_note=Functional libFuzzer run; MSVC AddressSanitizer is a separate hardening prerequisite.',
    '',
    'rustc --version --verbose:',
    (Get-NativeText -FilePath 'rustc' -Arguments '--version --verbose'),
    '',
    'cargo --version:',
    (Get-NativeText -FilePath 'cargo' -Arguments '--version'),
    '',
    'rustup toolchain list:',
    (Get-NativeText -FilePath 'rustup' -Arguments 'toolchain list'),
    '',
    'git status --short:',
    (Get-NativeText -FilePath 'git' -Arguments 'status --short')
)
$environmentPath = Join-Path $evidenceDir 'environment.txt'
Write-Utf8Lines -Path $environmentPath -Lines ([string[]]$environmentLines)

Invoke-EvidenceCommand -Name '01_fmt' -FilePath 'cargo' -Arguments 'fmt --all -- --check'
Invoke-EvidenceCommand -Name '02_build_workspace' -FilePath 'cargo' -Arguments 'build --workspace --all-targets --all-features'
Invoke-EvidenceCommand -Name '03_exact_dedup_test' -FilePath 'cargo' -Arguments 'test -p pithos-analysis --test exact_dedup -- --nocapture'
Invoke-EvidenceCommand -Name '04_analysis_tests' -FilePath 'cargo' -Arguments 'test -p pithos-analysis --tests -- --nocapture'
Invoke-EvidenceCommand -Name '05_workspace_tests' -FilePath 'cargo' -Arguments 'test --workspace --all-targets --all-features -- --nocapture'
Invoke-EvidenceCommand -Name '06_doc_tests' -FilePath 'cargo' -Arguments 'test --workspace --all-features --doc -- --nocapture'
Invoke-EvidenceCommand -Name '07_clippy_analysis' -FilePath 'cargo' -Arguments 'clippy -p pithos-analysis --all-targets -- -D warnings'
Invoke-EvidenceCommand -Name '08_clippy_workspace' -FilePath 'cargo' -Arguments 'clippy --workspace --all-targets --all-features -- -D warnings'
Invoke-EvidenceCommand -Name '09_fuzz_target_build' -FilePath 'cargo' -Arguments '+nightly check --manifest-path fuzz/Cargo.toml --bin exact_dedup'
Invoke-EvidenceCommand -Name '10_exact_dedup_fuzz_10k' -FilePath 'cargo' -Arguments '+nightly fuzz run --sanitizer none exact_dedup -- -runs=10000 -max_len=65536'
Invoke-EvidenceCommand -Name '11_coverage_80' -FilePath 'cargo' -Arguments 'llvm-cov --workspace --all-targets --all-features --fail-under-lines 80'

$failed = @($script:summary | Where-Object { $_.ExitCode -ne 0 })
$summaryPath = Join-Path $evidenceDir 'SUMMARY.md'
$branchText = Get-NativeText -FilePath 'git' -Arguments 'branch --show-current'
$commitText = Get-NativeText -FilePath 'git' -Arguments 'rev-parse HEAD'
$resultText = if ($failed.Count -eq 0) { 'PASS' } else { 'FAIL' }

$summaryLines = @(
    '# Gate C3 local validation',
    '',
    "- Timestamp UTC: $timestamp",
    "- Branch: $branchText",
    "- Commit: $commitText",
    "- PowerShell: $($PSVersionTable.PSVersion.ToString())",
    '- Windows fuzz sanitizer: none (functional fuzz; MSVC ASan remains a separate hardening check)',
    "- Result: **$resultText**",
    '',
    '| Check | Exit code | Log |',
    '|---|---:|---|'
)

foreach ($item in $script:summary) {
    $summaryLines += "| $($item.Name) | $($item.ExitCode) | $($item.Log) |"
}

$summaryLines += ''
if ($failed.Count -gt 0) {
    $summaryLines += '## Failures'
    $summaryLines += ''
    foreach ($item in $failed) {
        $summaryLines += "- **$($item.Name)** - exit code $($item.ExitCode); preserve the corresponding log unchanged."
    }
}
else {
    $summaryLines += 'No command failed.'
}
$summaryLines += ''
$summaryLines += 'Do not delete failing logs. Commit this entire evidence directory so the next review can reproduce the failure context.'
Write-Utf8Lines -Path $summaryPath -Lines ([string[]]$summaryLines)

Write-Host "`nEvidence written to: $evidenceDir" -ForegroundColor Green
Write-Host "Summary: $summaryPath"

if ($failed.Count -gt 0) {
    exit 1
}
exit 0
