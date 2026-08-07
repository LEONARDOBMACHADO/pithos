#requires -Version 5.1
param(
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$rootPath = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $rootPath

$utf8Encoding = New-Object System.Text.UTF8Encoding -ArgumentList $false
$runnerVersion = 'gate-c3-windows-v4'
$script:gateRows = @()

function Write-Utf8TextFile {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text
    )

    [System.IO.File]::WriteAllText($FilePath, $Text, $utf8Encoding)
}

function New-TextBuilder {
    return (New-Object System.Text.StringBuilder)
}

function Add-TextLine {
    param(
        [Parameter(Mandatory = $true)][System.Text.StringBuilder]$Builder,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text
    )

    [void]$Builder.AppendLine($Text)
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

    $nativeCapture = Invoke-NativeProcess -Executable $Executable -ArgumentString $ArgumentString
    $stdoutText = ''
    $stderrText = ''

    if (-not [string]::IsNullOrWhiteSpace($nativeCapture.StdOut)) {
        $stdoutText = $nativeCapture.StdOut.TrimEnd([char[]]"`r`n")
    }
    if (-not [string]::IsNullOrWhiteSpace($nativeCapture.StdErr)) {
        $stderrText = $nativeCapture.StdErr.TrimEnd([char[]]"`r`n")
    }

    if (($stdoutText.Length -gt 0) -and ($stderrText.Length -gt 0)) {
        return "$stdoutText | $stderrText"
    }
    if ($stdoutText.Length -gt 0) {
        return $stdoutText
    }
    if ($stderrText.Length -gt 0) {
        return $stderrText
    }
    return ''
}

function Get-EnvironmentText {
    $environmentBuilder = New-TextBuilder
    Add-TextLine $environmentBuilder "runner=$runnerVersion"
    Add-TextLine $environmentBuilder "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))"
    Add-TextLine $environmentBuilder "repo=$rootPath"
    Add-TextLine $environmentBuilder "powershell_version=$($PSVersionTable.PSVersion.ToString())"
    Add-TextLine $environmentBuilder "powershell_edition=$($PSVersionTable.PSEdition)"
    Add-TextLine $environmentBuilder "branch=$(Get-NativeProcessText -Executable 'git' -ArgumentString 'branch --show-current')"
    Add-TextLine $environmentBuilder "commit=$(Get-NativeProcessText -Executable 'git' -ArgumentString 'rev-parse HEAD')"
    Add-TextLine $environmentBuilder "os=$([System.Environment]::OSVersion.VersionString)"
    Add-TextLine $environmentBuilder "is_64bit_os=$([System.Environment]::Is64BitOperatingSystem)"
    Add-TextLine $environmentBuilder "is_64bit_process=$([System.Environment]::Is64BitProcess)"
    Add-TextLine $environmentBuilder 'gate_c3_windows_fuzz_sanitizer=none'
    Add-TextLine $environmentBuilder 'gate_c3_windows_fuzz_note=Functional libFuzzer run; MSVC AddressSanitizer is a separate hardening prerequisite.'
    Add-TextLine $environmentBuilder ''
    Add-TextLine $environmentBuilder 'rustc --version --verbose:'
    Add-TextLine $environmentBuilder (Get-NativeProcessText -Executable 'rustc' -ArgumentString '--version --verbose')
    Add-TextLine $environmentBuilder ''
    Add-TextLine $environmentBuilder 'cargo --version:'
    Add-TextLine $environmentBuilder (Get-NativeProcessText -Executable 'cargo' -ArgumentString '--version')
    Add-TextLine $environmentBuilder ''
    Add-TextLine $environmentBuilder 'cargo-fuzz --version:'
    Add-TextLine $environmentBuilder (Get-NativeProcessText -Executable 'cargo' -ArgumentString 'fuzz --version')
    Add-TextLine $environmentBuilder ''
    Add-TextLine $environmentBuilder 'cargo-llvm-cov --version:'
    Add-TextLine $environmentBuilder (Get-NativeProcessText -Executable 'cargo' -ArgumentString 'llvm-cov --version')
    Add-TextLine $environmentBuilder ''
    Add-TextLine $environmentBuilder 'rustup toolchain list:'
    Add-TextLine $environmentBuilder (Get-NativeProcessText -Executable 'rustup' -ArgumentString 'toolchain list')
    Add-TextLine $environmentBuilder ''
    Add-TextLine $environmentBuilder 'git status --short:'
    Add-TextLine $environmentBuilder (Get-NativeProcessText -Executable 'git' -ArgumentString 'status --short')
    return $environmentBuilder.ToString()
}

function Invoke-RunnerSelfTest {
    $selfTestDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("pithos-gate-c3-selftest-{0}" -f [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $selfTestDirectory | Out-Null

    try {
        $plainFile = Join-Path $selfTestDirectory 'plain.txt'
        Write-Utf8TextFile -FilePath $plainFile -Text 'PITHOS_GATE_C3_SELF_TEST'
        $roundTripText = [System.IO.File]::ReadAllText($plainFile, [System.Text.Encoding]::UTF8)
        if ($roundTripText -ne 'PITHOS_GATE_C3_SELF_TEST') {
            throw 'UTF-8 file round-trip failed.'
        }

        $gitCheck = Invoke-NativeProcess -Executable 'git' -ArgumentString '--version'
        if ($gitCheck.ExitCode -ne 0) {
            throw "git invocation failed: $($gitCheck.StdErr)"
        }

        if ([string]::IsNullOrWhiteSpace([string]$PSVersionTable.PSEdition)) {
            throw 'PowerShell edition metadata is unavailable.'
        }

        # Exercise the exact environment serialization path used by a full run.
        $environmentProbe = Get-EnvironmentText
        if ([string]::IsNullOrWhiteSpace($environmentProbe)) {
            throw 'Environment serialization returned empty text.'
        }
        if ($environmentProbe.IndexOf("runner=$runnerVersion", [System.StringComparison]::Ordinal) -lt 0) {
            throw 'Environment serialization is missing the runner marker.'
        }
        $environmentProbePath = Join-Path $selfTestDirectory 'environment-probe.txt'
        Write-Utf8TextFile -FilePath $environmentProbePath -Text $environmentProbe

        # Exercise the same StringBuilder path used for metadata and SUMMARY.md.
        $metadataProbe = New-TextBuilder
        Add-TextLine $metadataProbe 'name=self_test'
        Add-TextLine $metadataProbe 'command=git --version'
        Add-TextLine $metadataProbe 'exit_code=0'
        $metadataProbePath = Join-Path $selfTestDirectory 'metadata-probe.txt'
        Write-Utf8TextFile -FilePath $metadataProbePath -Text $metadataProbe.ToString()

        $summaryProbe = New-TextBuilder
        Add-TextLine $summaryProbe '# Gate C3 runner self-test'
        Add-TextLine $summaryProbe '| Check | Exit code |'
        Add-TextLine $summaryProbe '|---|---:|'
        Add-TextLine $summaryProbe '| self_test | 0 |'
        $summaryProbePath = Join-Path $selfTestDirectory 'summary-probe.md'
        Write-Utf8TextFile -FilePath $summaryProbePath -Text $summaryProbe.ToString()

        foreach ($probePath in @($environmentProbePath, $metadataProbePath, $summaryProbePath)) {
            if (-not (Test-Path -LiteralPath $probePath -PathType Leaf)) {
                throw "Self-test output was not created: $probePath"
            }
            if ((Get-Item -LiteralPath $probePath).Length -le 0) {
                throw "Self-test output is empty: $probePath"
            }
        }

        Write-Host "SELF_TEST_OK runner=$runnerVersion powershell=$($PSVersionTable.PSVersion) edition=$($PSVersionTable.PSEdition)" -ForegroundColor Green
    }
    finally {
        Remove-Item -LiteralPath $selfTestDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Invoke-GateCheck {
    param(
        [Parameter(Mandatory = $true)][string]$CheckName,
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$ArgumentString,
        [Parameter(Mandatory = $true)][string]$EvidencePath
    )

    $safeCheckName = ($CheckName -replace '[^A-Za-z0-9._-]', '_')
    $logFile = Join-Path $EvidencePath "$safeCheckName.log"
    $metaFile = Join-Path $EvidencePath "$safeCheckName.meta.txt"
    $shownCommand = if ([string]::IsNullOrWhiteSpace($ArgumentString)) { $Executable } else { "$Executable $ArgumentString" }

    Write-Host "`n=== $CheckName ===" -ForegroundColor Cyan
    Write-Host $shownCommand

    $startedUtc = (Get-Date).ToUniversalTime()
    $nativeCapture = Invoke-NativeProcess -Executable $Executable -ArgumentString $ArgumentString
    $endedUtc = (Get-Date).ToUniversalTime()

    $logBuilder = New-TextBuilder
    if (-not [string]::IsNullOrEmpty($nativeCapture.StdOut)) {
        Add-TextLine $logBuilder '[stdout]'
        Add-TextLine $logBuilder $nativeCapture.StdOut.TrimEnd([char[]]"`r`n")
    }
    if (-not [string]::IsNullOrEmpty($nativeCapture.StdErr)) {
        Add-TextLine $logBuilder '[stderr]'
        Add-TextLine $logBuilder $nativeCapture.StdErr.TrimEnd([char[]]"`r`n")
    }
    if ($logBuilder.Length -eq 0) {
        Add-TextLine $logBuilder '[no output]'
    }
    Write-Utf8TextFile -FilePath $logFile -Text $logBuilder.ToString()

    $metaBuilder = New-TextBuilder
    Add-TextLine $metaBuilder "name=$CheckName"
    Add-TextLine $metaBuilder "command=$shownCommand"
    Add-TextLine $metaBuilder "started_utc=$($startedUtc.ToString('o'))"
    Add-TextLine $metaBuilder "ended_utc=$($endedUtc.ToString('o'))"
    Add-TextLine $metaBuilder "exit_code=$($nativeCapture.ExitCode)"
    Write-Utf8TextFile -FilePath $metaFile -Text $metaBuilder.ToString()

    if (-not [string]::IsNullOrWhiteSpace($nativeCapture.StdOut)) {
        Write-Host $nativeCapture.StdOut.TrimEnd([char[]]"`r`n")
    }
    if (-not [string]::IsNullOrWhiteSpace($nativeCapture.StdErr)) {
        Write-Host $nativeCapture.StdErr.TrimEnd([char[]]"`r`n")
    }

    $separatorChars = [char[]]@([char]92, [char]47)
    $relativeLogPath = $logFile.Substring($rootPath.Length).TrimStart($separatorChars).Replace([char]92, [char]47)
    $script:gateRows += [pscustomobject]@{
        Name = $CheckName
        ExitCode = [int]$nativeCapture.ExitCode
        Log = $relativeLogPath
    }
}

function Write-GateSummary {
    param(
        [Parameter(Mandatory = $true)][string]$EvidencePath,
        [Parameter(Mandatory = $true)][string]$UtcStamp
    )

    $failedRows = @($script:gateRows | Where-Object { $_.ExitCode -ne 0 })
    $gateResult = if ($failedRows.Count -eq 0) { 'PASS' } else { 'FAIL' }
    $branchName = Get-NativeProcessText -Executable 'git' -ArgumentString 'branch --show-current'
    $commitSha = Get-NativeProcessText -Executable 'git' -ArgumentString 'rev-parse HEAD'

    $summaryBuilder = New-TextBuilder
    Add-TextLine $summaryBuilder '# Gate C3 local validation'
    Add-TextLine $summaryBuilder ''
    Add-TextLine $summaryBuilder "- Timestamp UTC: $UtcStamp"
    Add-TextLine $summaryBuilder "- Branch: $branchName"
    Add-TextLine $summaryBuilder "- Commit: $commitSha"
    Add-TextLine $summaryBuilder "- Runner: $runnerVersion"
    Add-TextLine $summaryBuilder "- PowerShell: $($PSVersionTable.PSVersion.ToString())"
    Add-TextLine $summaryBuilder "- PowerShell edition: $($PSVersionTable.PSEdition)"
    Add-TextLine $summaryBuilder '- Windows fuzz sanitizer: none (functional fuzz; MSVC ASan remains a separate hardening check)'
    Add-TextLine $summaryBuilder "- Result: **$gateResult**"
    Add-TextLine $summaryBuilder ''
    Add-TextLine $summaryBuilder '| Check | Exit code | Log |'
    Add-TextLine $summaryBuilder '|---|---:|---|'

    foreach ($row in $script:gateRows) {
        Add-TextLine $summaryBuilder "| $($row.Name) | $($row.ExitCode) | $($row.Log) |"
    }

    Add-TextLine $summaryBuilder ''
    if ($failedRows.Count -gt 0) {
        Add-TextLine $summaryBuilder '## Failures'
        Add-TextLine $summaryBuilder ''
        foreach ($row in $failedRows) {
            Add-TextLine $summaryBuilder "- **$($row.Name)** - exit code $($row.ExitCode); preserve the corresponding log unchanged."
        }
    }
    else {
        Add-TextLine $summaryBuilder 'No command failed.'
    }
    Add-TextLine $summaryBuilder ''
    Add-TextLine $summaryBuilder 'Do not delete failing logs. Commit this entire evidence directory so the next review can reproduce the failure context.'

    $summaryPath = Join-Path $EvidencePath 'SUMMARY.md'
    Write-Utf8TextFile -FilePath $summaryPath -Text $summaryBuilder.ToString()
    return $failedRows.Count
}

if ($SelfTest) {
    Invoke-RunnerSelfTest
    exit 0
}

Invoke-RunnerSelfTest

$utcStamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$evidencePath = Join-Path $rootPath "docs/gates/evidence/gate-c3-$utcStamp"
New-Item -ItemType Directory -Force -Path $evidencePath | Out-Null

try {
    $bootstrapBuilder = New-TextBuilder
    Add-TextLine $bootstrapBuilder "runner=$runnerVersion"
    Add-TextLine $bootstrapBuilder "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))"
    Add-TextLine $bootstrapBuilder "powershell_version=$($PSVersionTable.PSVersion.ToString())"
    Add-TextLine $bootstrapBuilder "powershell_edition=$($PSVersionTable.PSEdition)"
    Add-TextLine $bootstrapBuilder "repo=$rootPath"
    Write-Utf8TextFile -FilePath (Join-Path $evidencePath 'BOOTSTRAP.txt') -Text $bootstrapBuilder.ToString()

    Write-Utf8TextFile -FilePath (Join-Path $evidencePath 'environment.txt') -Text (Get-EnvironmentText)

    Invoke-GateCheck -CheckName '01_fmt' -Executable 'cargo' -ArgumentString 'fmt --all -- --check' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '02_build_workspace' -Executable 'cargo' -ArgumentString 'build --workspace --all-targets --all-features' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '03_exact_dedup_test' -Executable 'cargo' -ArgumentString 'test -p pithos-analysis --test exact_dedup -- --nocapture' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '04_analysis_tests' -Executable 'cargo' -ArgumentString 'test -p pithos-analysis --tests -- --nocapture' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '05_workspace_tests' -Executable 'cargo' -ArgumentString 'test --workspace --all-targets --all-features -- --nocapture' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '06_doc_tests' -Executable 'cargo' -ArgumentString 'test --workspace --all-features --doc -- --nocapture' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '07_clippy_analysis' -Executable 'cargo' -ArgumentString 'clippy -p pithos-analysis --all-targets -- -D warnings' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '08_clippy_workspace' -Executable 'cargo' -ArgumentString 'clippy --workspace --all-targets --all-features -- -D warnings' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '09_fuzz_target_build' -Executable 'cargo' -ArgumentString '+nightly check --manifest-path fuzz/Cargo.toml --bin exact_dedup' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '10_exact_dedup_fuzz_10k' -Executable 'cargo' -ArgumentString '+nightly fuzz run --sanitizer none exact_dedup -- -runs=10000 -max_len=65536' -EvidencePath $evidencePath
    Invoke-GateCheck -CheckName '11_coverage_80' -Executable 'cargo' -ArgumentString 'llvm-cov --workspace --all-targets --all-features --fail-under-lines 80' -EvidencePath $evidencePath

    $failureCount = Write-GateSummary -EvidencePath $evidencePath -UtcStamp $utcStamp

    Write-Host "`nEvidence written to: $evidencePath" -ForegroundColor Green
    Write-Host "Summary: $(Join-Path $evidencePath 'SUMMARY.md')"

    if ($failureCount -gt 0) {
        exit 1
    }
    exit 0
}
catch {
    $fatalBuilder = New-TextBuilder
    Add-TextLine $fatalBuilder "runner=$runnerVersion"
    Add-TextLine $fatalBuilder "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))"
    Add-TextLine $fatalBuilder "exception_type=$($_.Exception.GetType().FullName)"
    Add-TextLine $fatalBuilder "message=$($_.Exception.Message)"
    Add-TextLine $fatalBuilder "script_stack_trace=$($_.ScriptStackTrace)"
    Write-Utf8TextFile -FilePath (Join-Path $evidencePath 'RUNNER_FATAL.txt') -Text $fatalBuilder.ToString()
    Write-Error "Gate C3 runner failed before completion. Evidence: $evidencePath. $($_.Exception.Message)"
    exit 2
}
