#requires -Version 5.1
param(
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$rootPath = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $rootPath

$utf8Encoding = New-Object System.Text.UTF8Encoding -ArgumentList $false
$runnerVersion = 'gate-c3-windows-v3'

function Write-Utf8TextFile {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text
    )

    [System.IO.File]::WriteAllText($FilePath, $Text, $utf8Encoding)
}

function Write-Utf8LineFile {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$Lines
    )

    [System.IO.File]::WriteAllLines($FilePath, $Lines, $utf8Encoding)
}

function Invoke-NativeProcess {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$ArgumentString
    )

    $resolvedCommand = Get-Command $Executable -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $resolvedCommand) {
        return [pscustomobject]@{
            ExitCode = 127
            StdOut = ''
            StdErr = "COMMAND_NOT_FOUND: $Executable"
        }
    }

    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $resolvedCommand.Source
    $startInfo.Arguments = $ArgumentString
    $startInfo.WorkingDirectory = $rootPath
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.CreateNoWindow = $true

    $nativeProcess = New-Object System.Diagnostics.Process
    $nativeProcess.StartInfo = $startInfo

    try {
        if (-not $nativeProcess.Start()) {
            throw "Failed to start executable: $Executable"
        }

        $stdoutRead = $nativeProcess.StandardOutput.ReadToEndAsync()
        $stderrRead = $nativeProcess.StandardError.ReadToEndAsync()
        $nativeProcess.WaitForExit()
        $stdoutRead.Wait()
        $stderrRead.Wait()

        return [pscustomobject]@{
            ExitCode = [int]$nativeProcess.ExitCode
            StdOut = [string]$stdoutRead.Result
            StdErr = [string]$stderrRead.Result
        }
    }
    catch {
        return [pscustomobject]@{
            ExitCode = 126
            StdOut = ''
            StdErr = "PROCESS_EXECUTION_FAILED: $($_.Exception.Message)"
        }
    }
    finally {
        $nativeProcess.Dispose()
    }
}

function Get-NativeProcessText {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$ArgumentString
    )

    $capture = Invoke-NativeProcess -Executable $Executable -ArgumentString $ArgumentString
    $parts = @()

    if (-not [string]::IsNullOrWhiteSpace($capture.StdOut)) {
        $parts += $capture.StdOut.TrimEnd([char[]]"`r`n")
    }
    if (-not [string]::IsNullOrWhiteSpace($capture.StdErr)) {
        $parts += $capture.StdErr.TrimEnd([char[]]"`r`n")
    }

    if ($parts.Count -eq 0) {
        return ''
    }

    return ($parts -join ' | ')
}

function Invoke-RunnerSelfTest {
    $selfTestPath = Join-Path ([System.IO.Path]::GetTempPath()) ("pithos-gate-c3-selftest-{0}.txt" -f [Guid]::NewGuid().ToString('N'))

    try {
        Write-Utf8TextFile -FilePath $selfTestPath -Text 'PITHOS_GATE_C3_SELF_TEST'
        $roundTripText = [System.IO.File]::ReadAllText($selfTestPath, [System.Text.Encoding]::UTF8)
        if ($roundTripText -ne 'PITHOS_GATE_C3_SELF_TEST') {
            throw 'UTF-8 file round-trip failed.'
        }

        $gitCheck = Invoke-NativeProcess -Executable 'git' -ArgumentString '--version'
        if ($gitCheck.ExitCode -ne 0) {
            throw "git invocation failed: $($gitCheck.StdErr)"
        }

        if ([string]::IsNullOrWhiteSpace($PSVersionTable.PSEdition)) {
            throw 'PowerShell edition metadata is unavailable.'
        }

        Write-Host "SELF_TEST_OK runner=$runnerVersion powershell=$($PSVersionTable.PSVersion) edition=$($PSVersionTable.PSEdition)" -ForegroundColor Green
    }
    finally {
        Remove-Item -LiteralPath $selfTestPath -Force -ErrorAction SilentlyContinue
    }
}

if ($SelfTest) {
    Invoke-RunnerSelfTest
    exit 0
}

# Always exercise the runner plumbing before starting expensive validation.
Invoke-RunnerSelfTest

$utcStamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$evidencePath = Join-Path $rootPath "docs/gates/evidence/gate-c3-$utcStamp"
New-Item -ItemType Directory -Force -Path $evidencePath | Out-Null

# Write immediately so even an unexpected later runner failure leaves evidence.
$bootstrapPath = Join-Path $evidencePath 'BOOTSTRAP.txt'
Write-Utf8LineFile -FilePath $bootstrapPath -Lines @(
    "runner=$runnerVersion",
    "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
    "powershell_version=$($PSVersionTable.PSVersion.ToString())",
    "powershell_edition=$($PSVersionTable.PSEdition)",
    "repo=$rootPath"
)

$validationRows = @()

function Invoke-GateCheck {
    param(
        [Parameter(Mandatory = $true)][string]$CheckName,
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$ArgumentString
    )

    $safeCheckName = ($CheckName -replace '[^A-Za-z0-9._-]', '_')
    $logFile = Join-Path $evidencePath "$safeCheckName.log"
    $metaFile = Join-Path $evidencePath "$safeCheckName.meta.txt"
    $shownCommand = if ([string]::IsNullOrWhiteSpace($ArgumentString)) { $Executable } else { "$Executable $ArgumentString" }

    Write-Host "`n=== $CheckName ===" -ForegroundColor Cyan
    Write-Host $shownCommand

    $startedUtc = (Get-Date).ToUniversalTime()
    $capture = Invoke-NativeProcess -Executable $Executable -ArgumentString $ArgumentString
    $endedUtc = (Get-Date).ToUniversalTime()

    $logSections = @()
    if (-not [string]::IsNullOrEmpty($capture.StdOut)) {
        $logSections += "[stdout]`r`n$($capture.StdOut.TrimEnd([char[]]"`r`n"))"
    }
    if (-not [string]::IsNullOrEmpty($capture.StdErr)) {
        $logSections += "[stderr]`r`n$($capture.StdErr.TrimEnd([char[]]"`r`n"))"
    }
    if ($logSections.Count -eq 0) {
        $logSections += '[no output]'
    }

    Write-Utf8TextFile -FilePath $logFile -Text (($logSections -join "`r`n") + "`r`n")
    Write-Utf8LineFile -FilePath $metaFile -Lines @(
        "name=$CheckName",
        "command=$shownCommand",
        "started_utc=$($startedUtc.ToString('o'))",
        "ended_utc=$($endedUtc.ToString('o'))",
        "exit_code=$($capture.ExitCode)"
    )

    if (-not [string]::IsNullOrWhiteSpace($capture.StdOut)) {
        Write-Host $capture.StdOut.TrimEnd([char[]]"`r`n")
    }
    if (-not [string]::IsNullOrWhiteSpace($capture.StdErr)) {
        Write-Host $capture.StdErr.TrimEnd([char[]]"`r`n")
    }

    $separatorChars = [char[]]@([char]92, [char]47)
    $relativeLogPath = $logFile.Substring($rootPath.Length).TrimStart($separatorChars).Replace([char]92, [char]47)
    $script:validationRows += [pscustomobject]@{
        Name = $CheckName
        ExitCode = [int]$capture.ExitCode
        Log = $relativeLogPath
    }
}

$environmentPath = Join-Path $evidencePath 'environment.txt'
Write-Utf8LineFile -FilePath $environmentPath -Lines @(
    "runner=$runnerVersion",
    "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
    "repo=$rootPath",
    "powershell_version=$($PSVersionTable.PSVersion.ToString())",
    "powershell_edition=$($PSVersionTable.PSEdition)",
    "branch=$(Get-NativeProcessText -Executable 'git' -ArgumentString 'branch --show-current')",
    "commit=$(Get-NativeProcessText -Executable 'git' -ArgumentString 'rev-parse HEAD')",
    "os=$([System.Environment]::OSVersion.VersionString)",
    "is_64bit_os=$([System.Environment]::Is64BitOperatingSystem)",
    "is_64bit_process=$([System.Environment]::Is64BitProcess)",
    'gate_c3_windows_fuzz_sanitizer=none',
    'gate_c3_windows_fuzz_note=Functional libFuzzer run; MSVC AddressSanitizer is a separate hardening prerequisite.',
    '',
    'rustc --version --verbose:',
    (Get-NativeProcessText -Executable 'rustc' -ArgumentString '--version --verbose'),
    '',
    'cargo --version:',
    (Get-NativeProcessText -Executable 'cargo' -ArgumentString '--version'),
    '',
    'cargo-fuzz --version:',
    (Get-NativeProcessText -Executable 'cargo' -ArgumentString 'fuzz --version'),
    '',
    'cargo-llvm-cov --version:',
    (Get-NativeProcessText -Executable 'cargo' -ArgumentString 'llvm-cov --version'),
    '',
    'rustup toolchain list:',
    (Get-NativeProcessText -Executable 'rustup' -ArgumentString 'toolchain list'),
    '',
    'git status --short:',
    (Get-NativeProcessText -Executable 'git' -ArgumentString 'status --short')
)

Invoke-GateCheck -CheckName '01_fmt' -Executable 'cargo' -ArgumentString 'fmt --all -- --check'
Invoke-GateCheck -CheckName '02_build_workspace' -Executable 'cargo' -ArgumentString 'build --workspace --all-targets --all-features'
Invoke-GateCheck -CheckName '03_exact_dedup_test' -Executable 'cargo' -ArgumentString 'test -p pithos-analysis --test exact_dedup -- --nocapture'
Invoke-GateCheck -CheckName '04_analysis_tests' -Executable 'cargo' -ArgumentString 'test -p pithos-analysis --tests -- --nocapture'
Invoke-GateCheck -CheckName '05_workspace_tests' -Executable 'cargo' -ArgumentString 'test --workspace --all-targets --all-features -- --nocapture'
Invoke-GateCheck -CheckName '06_doc_tests' -Executable 'cargo' -ArgumentString 'test --workspace --all-features --doc -- --nocapture'
Invoke-GateCheck -CheckName '07_clippy_analysis' -Executable 'cargo' -ArgumentString 'clippy -p pithos-analysis --all-targets -- -D warnings'
Invoke-GateCheck -CheckName '08_clippy_workspace' -Executable 'cargo' -ArgumentString 'clippy --workspace --all-targets --all-features -- -D warnings'
Invoke-GateCheck -CheckName '09_fuzz_target_build' -Executable 'cargo' -ArgumentString '+nightly check --manifest-path fuzz/Cargo.toml --bin exact_dedup'
Invoke-GateCheck -CheckName '10_exact_dedup_fuzz_10k' -Executable 'cargo' -ArgumentString '+nightly fuzz run --sanitizer none exact_dedup -- -runs=10000 -max_len=65536'
Invoke-GateCheck -CheckName '11_coverage_80' -Executable 'cargo' -ArgumentString 'llvm-cov --workspace --all-targets --all-features --fail-under-lines 80'

$failedRows = @($script:validationRows | Where-Object { $_.ExitCode -ne 0 })
$summaryPath = Join-Path $evidencePath 'SUMMARY.md'
$branchName = Get-NativeProcessText -Executable 'git' -ArgumentString 'branch --show-current'
$commitSha = Get-NativeProcessText -Executable 'git' -ArgumentString 'rev-parse HEAD'
$gateResult = if ($failedRows.Count -eq 0) { 'PASS' } else { 'FAIL' }

$summaryLines = @(
    '# Gate C3 local validation',
    '',
    "- Timestamp UTC: $utcStamp",
    "- Branch: $branchName",
    "- Commit: $commitSha",
    "- Runner: $runnerVersion",
    "- PowerShell: $($PSVersionTable.PSVersion.ToString())",
    "- PowerShell edition: $($PSVersionTable.PSEdition)",
    '- Windows fuzz sanitizer: none (functional fuzz; MSVC ASan remains a separate hardening check)',
    "- Result: **$gateResult**",
    '',
    '| Check | Exit code | Log |',
    '|---|---:|---|'
)

foreach ($row in $script:validationRows) {
    $summaryLines += "| $($row.Name) | $($row.ExitCode) | $($row.Log) |"
}

$summaryLines += ''
if ($failedRows.Count -gt 0) {
    $summaryLines += '## Failures'
    $summaryLines += ''
    foreach ($row in $failedRows) {
        $summaryLines += "- **$($row.Name)** - exit code $($row.ExitCode); preserve the corresponding log unchanged."
    }
}
else {
    $summaryLines += 'No command failed.'
}
$summaryLines += ''
$summaryLines += 'Do not delete failing logs. Commit this entire evidence directory so the next review can reproduce the failure context.'

Write-Utf8LineFile -FilePath $summaryPath -Lines ([string[]]$summaryLines)

Write-Host "`nEvidence written to: $evidencePath" -ForegroundColor Green
Write-Host "Summary: $summaryPath"

if ($failedRows.Count -gt 0) {
    exit 1
}
exit 0
