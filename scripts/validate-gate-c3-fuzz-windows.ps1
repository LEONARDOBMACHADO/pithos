#requires -Version 5.1

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$rootPath = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $rootPath

$utf8Encoding = New-Object System.Text.UTF8Encoding -ArgumentList $false
$runnerVersion = 'gate-c3-windows-asan-fuzz-v1'
$dllName = 'clang_rt.asan_dynamic-x86_64.dll'

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

function Get-NativeText {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$ArgumentString
    )
    $capture = Invoke-NativeProcess -Executable $Executable -ArgumentString $ArgumentString
    $stdoutText = ''
    $stderrText = ''
    if (-not [string]::IsNullOrWhiteSpace($capture.StdOut)) {
        $stdoutText = $capture.StdOut.TrimEnd([char[]]"`r`n")
    }
    if (-not [string]::IsNullOrWhiteSpace($capture.StdErr)) {
        $stderrText = $capture.StdErr.TrimEnd([char[]]"`r`n")
    }
    if (($stdoutText.Length -gt 0) -and ($stderrText.Length -gt 0)) {
        return "$stdoutText | $stderrText"
    }
    if ($stdoutText.Length -gt 0) { return $stdoutText }
    if ($stderrText.Length -gt 0) { return $stderrText }
    return ''
}

function Find-AsanRuntimeDll {
    $linkCommand = Get-Command 'link.exe' -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $linkCommand) {
        $besideLink = Join-Path (Split-Path -Parent $linkCommand.Source) $dllName
        if (Test-Path -LiteralPath $besideLink -PathType Leaf) {
            return (Resolve-Path -LiteralPath $besideLink).Path
        }
    }

    $searchRoots = New-Object System.Collections.Generic.List[string]
    $programFilesX86 = [System.Environment]::GetFolderPath([System.Environment+SpecialFolder]::ProgramFilesX86)
    $programFiles = [System.Environment]::GetFolderPath([System.Environment+SpecialFolder]::ProgramFiles)
    foreach ($programRoot in @($programFilesX86, $programFiles)) {
        if (-not [string]::IsNullOrWhiteSpace($programRoot)) {
            foreach ($vsVersion in @('2022', '18')) {
                $candidateRoot = Join-Path $programRoot "Microsoft Visual Studio\$vsVersion"
                if ((Test-Path -LiteralPath $candidateRoot -PathType Container) -and (-not $searchRoots.Contains($candidateRoot))) {
                    $searchRoots.Add($candidateRoot)
                }
            }
        }
    }

    foreach ($searchRoot in $searchRoots) {
        $matches = @(Get-ChildItem -LiteralPath $searchRoot -Filter $dllName -File -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\bin\\Host[xX]64\\x64\\' } |
            Sort-Object FullName -Descending)
        if ($matches.Count -gt 0) {
            return $matches[0].FullName
        }
    }

    return ''
}

$utcStamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$evidencePath = Join-Path $rootPath "docs/gates/evidence/gate-c3-fuzz-$utcStamp"
New-Item -ItemType Directory -Force -Path $evidencePath | Out-Null

try {
    $asanDll = Find-AsanRuntimeDll
    $asanDirectory = ''
    if (-not [string]::IsNullOrWhiteSpace($asanDll)) {
        $asanDirectory = Split-Path -Parent $asanDll
        $pathParts = @($env:PATH -split ';')
        if (-not ($pathParts -contains $asanDirectory)) {
            $env:PATH = "$asanDirectory;$env:PATH"
        }
    }

    $environmentBuilder = New-TextBuilder
    Add-TextLine $environmentBuilder "runner=$runnerVersion"
    Add-TextLine $environmentBuilder "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))"
    Add-TextLine $environmentBuilder "repo=$rootPath"
    Add-TextLine $environmentBuilder "branch=$(Get-NativeText -Executable 'git' -ArgumentString 'branch --show-current')"
    Add-TextLine $environmentBuilder "commit=$(Get-NativeText -Executable 'git' -ArgumentString 'rev-parse HEAD')"
    Add-TextLine $environmentBuilder "powershell_version=$($PSVersionTable.PSVersion.ToString())"
    Add-TextLine $environmentBuilder "powershell_edition=$($PSVersionTable.PSEdition)"
    Add-TextLine $environmentBuilder "rustc=$(Get-NativeText -Executable 'rustc' -ArgumentString '--version')"
    Add-TextLine $environmentBuilder "cargo=$(Get-NativeText -Executable 'cargo' -ArgumentString '--version')"
    Add-TextLine $environmentBuilder "cargo_fuzz=$(Get-NativeText -Executable 'cargo' -ArgumentString 'fuzz --version')"
    Add-TextLine $environmentBuilder "asan_runtime_dll=$asanDll"
    Add-TextLine $environmentBuilder "asan_runtime_directory=$asanDirectory"
    Add-TextLine $environmentBuilder 'fuzz_sanitizer=address'
    Write-Utf8TextFile -FilePath (Join-Path $evidencePath 'environment.txt') -Text $environmentBuilder.ToString()

    $logPath = Join-Path $evidencePath 'exact_dedup_fuzz_10k_asan.log'
    $metaPath = Join-Path $evidencePath 'exact_dedup_fuzz_10k_asan.meta.txt'
    $summaryPath = Join-Path $evidencePath 'SUMMARY.md'

    if ([string]::IsNullOrWhiteSpace($asanDll)) {
        $missingMessage = 'ASAN_RUNTIME_NOT_FOUND: install Visual Studio component Microsoft.VisualStudio.Component.VC.ASAN, then rerun this runner.'
        Write-Utf8TextFile -FilePath $logPath -Text ($missingMessage + "`r`n")
        $exitCode = 125
        $startedUtc = (Get-Date).ToUniversalTime()
        $endedUtc = $startedUtc
    }
    else {
        $startedUtc = (Get-Date).ToUniversalTime()
        $capture = Invoke-NativeProcess -Executable 'cargo' -ArgumentString '+nightly fuzz run exact_dedup -- -runs=10000 -max_len=65536'
        $endedUtc = (Get-Date).ToUniversalTime()
        $exitCode = [int]$capture.ExitCode

        $logBuilder = New-TextBuilder
        if (-not [string]::IsNullOrEmpty($capture.StdOut)) {
            Add-TextLine $logBuilder '[stdout]'
            Add-TextLine $logBuilder $capture.StdOut.TrimEnd([char[]]"`r`n")
        }
        if (-not [string]::IsNullOrEmpty($capture.StdErr)) {
            Add-TextLine $logBuilder '[stderr]'
            Add-TextLine $logBuilder $capture.StdErr.TrimEnd([char[]]"`r`n")
        }
        if ($logBuilder.Length -eq 0) {
            Add-TextLine $logBuilder '[no output]'
        }
        Write-Utf8TextFile -FilePath $logPath -Text $logBuilder.ToString()

        if (-not [string]::IsNullOrWhiteSpace($capture.StdOut)) {
            Write-Host $capture.StdOut.TrimEnd([char[]]"`r`n")
        }
        if (-not [string]::IsNullOrWhiteSpace($capture.StdErr)) {
            Write-Host $capture.StdErr.TrimEnd([char[]]"`r`n")
        }
    }

    $metaBuilder = New-TextBuilder
    Add-TextLine $metaBuilder 'name=exact_dedup_fuzz_10k_asan'
    Add-TextLine $metaBuilder 'command=cargo +nightly fuzz run exact_dedup -- -runs=10000 -max_len=65536'
    Add-TextLine $metaBuilder "started_utc=$($startedUtc.ToString('o'))"
    Add-TextLine $metaBuilder "ended_utc=$($endedUtc.ToString('o'))"
    Add-TextLine $metaBuilder "exit_code=$exitCode"
    Write-Utf8TextFile -FilePath $metaPath -Text $metaBuilder.ToString()

    $resultText = if ($exitCode -eq 0) { 'PASS' } else { 'FAIL' }
    $summaryBuilder = New-TextBuilder
    Add-TextLine $summaryBuilder '# Gate C3 Windows ASan fuzz validation'
    Add-TextLine $summaryBuilder ''
    Add-TextLine $summaryBuilder "- Timestamp UTC: $utcStamp"
    Add-TextLine $summaryBuilder "- Branch: $(Get-NativeText -Executable 'git' -ArgumentString 'branch --show-current')"
    Add-TextLine $summaryBuilder "- Commit: $(Get-NativeText -Executable 'git' -ArgumentString 'rev-parse HEAD')"
    Add-TextLine $summaryBuilder "- Runner: $runnerVersion"
    Add-TextLine $summaryBuilder '- Sanitizer: AddressSanitizer'
    Add-TextLine $summaryBuilder "- ASan runtime: $asanDll"
    Add-TextLine $summaryBuilder "- Result: **$resultText**"
    Add-TextLine $summaryBuilder ''
    Add-TextLine $summaryBuilder '| Check | Exit code | Log |'
    Add-TextLine $summaryBuilder '|---|---:|---|'
    Add-TextLine $summaryBuilder "| exact_dedup_fuzz_10k_asan | $exitCode | docs/gates/evidence/gate-c3-fuzz-$utcStamp/exact_dedup_fuzz_10k_asan.log |"
    Add-TextLine $summaryBuilder ''
    Add-TextLine $summaryBuilder 'This is supplemental evidence for the full Gate C3 run. Rust source is unchanged from the previously validated Gate C3 commit.'
    Write-Utf8TextFile -FilePath $summaryPath -Text $summaryBuilder.ToString()

    Write-Host "Evidence written to: $evidencePath" -ForegroundColor Green
    Write-Host "Summary: $summaryPath"
    exit $exitCode
}
catch {
    $fatalBuilder = New-TextBuilder
    Add-TextLine $fatalBuilder "runner=$runnerVersion"
    Add-TextLine $fatalBuilder "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))"
    Add-TextLine $fatalBuilder "exception_type=$($_.Exception.GetType().FullName)"
    Add-TextLine $fatalBuilder "message=$($_.Exception.Message)"
    Add-TextLine $fatalBuilder "script_stack_trace=$($_.ScriptStackTrace)"
    Write-Utf8TextFile -FilePath (Join-Path $evidencePath 'RUNNER_FATAL.txt') -Text $fatalBuilder.ToString()
    Write-Error "Gate C3 fuzz runner failed. Evidence: $evidencePath. $($_.Exception.Message)"
    exit 2
}
