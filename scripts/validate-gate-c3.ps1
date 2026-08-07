#requires -Version 5.1

$ErrorActionPreference = 'Continue'
Set-StrictMode -Version Latest

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repo

$timestamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$evidenceDir = Join-Path $repo "docs/gates/evidence/gate-c3-$timestamp"
New-Item -ItemType Directory -Force -Path $evidenceDir | Out-Null

$summary = New-Object System.Collections.Generic.List[object]
$utf8NoBom = New-Object System.Text.UTF8Encoding -ArgumentList $false

function Write-TextFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$Lines
    )
    [System.IO.File]::WriteAllLines($Path, $Lines, $utf8NoBom)
}

function Capture-CommandText {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [string[]]$Arguments = @()
    )

    if ($null -eq (Get-Command $Command -ErrorAction SilentlyContinue)) {
        return [string[]]@("COMMAND_NOT_FOUND: $Command")
    }

    $captured = @(& $Command @Arguments 2>&1 | ForEach-Object { $_.ToString() })
    return [string[]]$captured
}

function Invoke-EvidenceCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Command,
        [string[]]$Arguments = @()
    )

    $safeName = ($Name -replace '[^A-Za-z0-9._-]', '_')
    $logPath = Join-Path $evidenceDir "$safeName.log"
    $metaPath = Join-Path $evidenceDir "$safeName.meta.txt"

    Write-Host "`n=== $Name ===" -ForegroundColor Cyan
    Write-Host "$Command $($Arguments -join ' ')"

    $started = (Get-Date).ToUniversalTime()
    $commandInfo = Get-Command $Command -ErrorAction SilentlyContinue

    if ($null -eq $commandInfo) {
        $output = [string[]]@("COMMAND_NOT_FOUND: $Command")
        $exitCode = 127
    }
    else {
        $global:LASTEXITCODE = 0
        $captured = @(& $Command @Arguments 2>&1 | ForEach-Object { $_.ToString() })
        $output = [string[]]$captured
        $exitCode = $LASTEXITCODE
        if ($null -eq $exitCode) {
            $exitCode = 0
        }
    }

    $ended = (Get-Date).ToUniversalTime()

    Write-TextFile -Path $logPath -Lines $output
    foreach ($line in $output) {
        Write-Host $line
    }

    Write-TextFile -Path $metaPath -Lines @(
        "name=$Name",
        "command=$Command $($Arguments -join ' ')",
        "started_utc=$($started.ToString('o'))",
        "ended_utc=$($ended.ToString('o'))",
        "exit_code=$exitCode"
    )

    $trimChars = [char[]]@([char]92, [char]47)
    $relativeLog = $logPath.Substring($repo.Length).TrimStart($trimChars) -replace '\\', '/'
    $summary.Add([pscustomobject]@{
        Name = $Name
        ExitCode = [int]$exitCode
        Log = $relativeLog
    })
}

$environmentPath = Join-Path $evidenceDir 'environment.txt'
$environmentLines = New-Object System.Collections.Generic.List[string]
$environmentLines.Add("timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))")
$environmentLines.Add("repo=$repo")
$environmentLines.Add("powershell_version=$($PSVersionTable.PSVersion.ToString())")
$environmentLines.Add("powershell_edition=$(if ($PSVersionTable.PSObject.Properties.Name -contains 'PSEdition') { $PSVersionTable.PSEdition } else { 'Desktop' })")
$environmentLines.Add("branch=$((Capture-CommandText 'git' @('branch', '--show-current')) -join ' ')")
$environmentLines.Add("commit=$((Capture-CommandText 'git' @('rev-parse', 'HEAD')) -join ' ')")
$environmentLines.Add("os=$([System.Environment]::OSVersion.VersionString)")
$environmentLines.Add("is_64bit_os=$([System.Environment]::Is64BitOperatingSystem)")
$environmentLines.Add("is_64bit_process=$([System.Environment]::Is64BitProcess)")
$environmentLines.Add('')
$environmentLines.Add('rustc --version --verbose:')
$environmentLines.AddRange([string[]](Capture-CommandText 'rustc' @('--version', '--verbose')))
$environmentLines.Add('')
$environmentLines.Add('cargo --version:')
$environmentLines.AddRange([string[]](Capture-CommandText 'cargo' @('--version')))
$environmentLines.Add('')
$environmentLines.Add('rustup toolchain list:')
$environmentLines.AddRange([string[]](Capture-CommandText 'rustup' @('toolchain', 'list')))
$environmentLines.Add('')
$environmentLines.Add('git status --short:')
$environmentLines.AddRange([string[]](Capture-CommandText 'git' @('status', '--short')))
Write-TextFile -Path $environmentPath -Lines $environmentLines.ToArray()

Invoke-EvidenceCommand '01_fmt' 'cargo' @('fmt', '--all', '--', '--check')
Invoke-EvidenceCommand '02_build_workspace' 'cargo' @('build', '--workspace', '--all-targets')
Invoke-EvidenceCommand '03_exact_dedup_test' 'cargo' @('test', '-p', 'pithos-analysis', '--test', 'exact_dedup', '--', '--nocapture')
Invoke-EvidenceCommand '04_analysis_tests' 'cargo' @('test', '-p', 'pithos-analysis', '--tests', '--', '--nocapture')
Invoke-EvidenceCommand '05_workspace_tests' 'cargo' @('test', '--workspace', '--all-targets', '--', '--nocapture')
Invoke-EvidenceCommand '06_clippy_analysis' 'cargo' @('clippy', '-p', 'pithos-analysis', '--all-targets', '--', '-D', 'warnings')
Invoke-EvidenceCommand '07_clippy_workspace' 'cargo' @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
Invoke-EvidenceCommand '08_fuzz_target_build' 'cargo' @('+nightly', 'check', '--manifest-path', 'fuzz/Cargo.toml', '--bin', 'exact_dedup')
Invoke-EvidenceCommand '09_exact_dedup_fuzz_10k' 'cargo' @('+nightly', 'fuzz', 'run', 'exact_dedup', '--', '-runs=10000', '-max_len=65536')
Invoke-EvidenceCommand '10_coverage_80' 'cargo' @('llvm-cov', '--workspace', '--all-targets', '--fail-under-lines', '80')

$failed = @($summary | Where-Object { $_.ExitCode -ne 0 })
$summaryPath = Join-Path $evidenceDir 'SUMMARY.md'
$branchText = (Capture-CommandText 'git' @('branch', '--show-current')) -join ' '
$commitText = (Capture-CommandText 'git' @('rev-parse', 'HEAD')) -join ' '
$resultText = if ($failed.Count -eq 0) { 'PASS' } else { 'FAIL' }

$summaryLines = New-Object System.Collections.Generic.List[string]
$summaryLines.Add('# Gate C3 local validation')
$summaryLines.Add('')
$summaryLines.Add("- Timestamp UTC: $timestamp")
$summaryLines.Add("- Branch: $branchText")
$summaryLines.Add("- Commit: $commitText")
$summaryLines.Add("- PowerShell: $($PSVersionTable.PSVersion.ToString())")
$summaryLines.Add("- Result: **$resultText**")
$summaryLines.Add('')
$summaryLines.Add('| Check | Exit code | Log |')
$summaryLines.Add('|---|---:|---|')
foreach ($item in $summary) {
    $summaryLines.Add("| $($item.Name) | $($item.ExitCode) | $($item.Log) |")
}
$summaryLines.Add('')
if ($failed.Count -gt 0) {
    $summaryLines.Add('## Failures')
    $summaryLines.Add('')
    foreach ($item in $failed) {
        $summaryLines.Add("- **$($item.Name)** - exit code $($item.ExitCode); preserve the corresponding log unchanged.")
    }
}
else {
    $summaryLines.Add('No command failed.')
}
$summaryLines.Add('')
$summaryLines.Add('Do not delete failing logs. Commit this entire evidence directory so the next review can reproduce the failure context.')
Write-TextFile -Path $summaryPath -Lines $summaryLines.ToArray()

Write-Host "`nEvidence written to: $evidenceDir" -ForegroundColor Green
Write-Host "Summary: $summaryPath"

if ($failed.Count -gt 0) {
    exit 1
}
exit 0
