#requires -Version 5.1

Set-StrictMode -Version Latest

$sourceMapValidator = Join-Path $PSScriptRoot 'validate-powershell-source-map.ps1'
if (-not (Test-Path -LiteralPath $sourceMapValidator -PathType Leaf)) {
    throw "PowerShell source-map validator not found: $sourceMapValidator"
}
& $sourceMapValidator

function Invoke-PithosNativeProcess {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory=$true)][string]$FilePath,
        [string[]]$Arguments = @(),
        [string]$LogPath = '',
        [switch]$AppendLog,
        [switch]$DiscardOutput
    )

    # Resolve before changing ErrorActionPreference so command-not-found is a
    # normal terminating harness error, never a stale LASTEXITCODE result.
    $command = Get-Command $FilePath -ErrorAction Stop
    $resolvedPath = if ($command.Path) { $command.Path } else { $FilePath }

    # Windows PowerShell 5.1 re-serializes native argv through a legacy command
    # line representation. Embedded quotes in an inline `-Command` payload can
    # therefore be lost before powershell.exe receives them. Never allow that
    # fragile path through the generic native helper. Inline PowerShell must go
    # through Invoke-PithosNativePowerShellEncoded below; `-File` remains valid.
    $nativeLeaf = [System.IO.Path]::GetFileName($resolvedPath)
    $isPowerShellHost = $nativeLeaf -match '^(?i:powershell|pwsh)(\.exe)?$'
    if ($isPowerShellHost -and ($Arguments -contains '-Command')) {
        throw 'Unsafe native PowerShell -Command invocation rejected. Use Invoke-PithosNativePowerShellEncoded so the payload is transported with -EncodedCommand.'
    }

    $previousErrorActionPreference = $ErrorActionPreference
    $exitCode = $null

    try {
        # Windows PowerShell 5.1 can promote native stderr redirected through
        # 2>&1 into PowerShell ErrorRecord objects. Pithos and Cargo both use
        # stderr for legitimate diagnostics/telemetry, so native process
        # success must be decided by the process exit code, never by stderr.
        $ErrorActionPreference = 'Continue'

        # LASTEXITCODE is process-global state and can otherwise retain the
        # result of an earlier command when process startup itself fails.
        $global:LASTEXITCODE = $null

        if ([string]::IsNullOrWhiteSpace($LogPath)) {
            & $resolvedPath @Arguments 2>&1 | ForEach-Object {
                if (-not $DiscardOutput) { Write-Host $_ }
            }
        } elseif ($AppendLog) {
            & $resolvedPath @Arguments 2>&1 |
                Tee-Object -FilePath $LogPath -Append |
                ForEach-Object {
                    if (-not $DiscardOutput) { Write-Host $_ }
                }
        } else {
            & $resolvedPath @Arguments 2>&1 |
                Tee-Object -FilePath $LogPath |
                ForEach-Object {
                    if (-not $DiscardOutput) { Write-Host $_ }
                }
        }

        # Capture immediately. Any later native command could overwrite it.
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }

    if ($null -eq $exitCode) {
        throw "Native process completed without an exit code: $resolvedPath"
    }

    return [int]$exitCode
}

function Invoke-PithosNativePowerShellEncoded {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory=$true)][string]$ScriptText,
        [string]$PowerShellPath = 'powershell',
        [string]$LogPath = '',
        [switch]$AppendLog,
        [switch]$DiscardOutput
    )

    # powershell.exe -EncodedCommand requires UTF-16LE (Encoding.Unicode).
    # The resulting Base64 argv contains no embedded quotes, whitespace or
    # shell metacharacters, avoiding the PowerShell 5.1 native re-quoting path.
    $encodedCommand = [Convert]::ToBase64String(
        [System.Text.Encoding]::Unicode.GetBytes($ScriptText)
    )

    return Invoke-PithosNativeProcess `
        -FilePath $PowerShellPath `
        -Arguments @('-NoProfile','-EncodedCommand',$encodedCommand) `
        -LogPath $LogPath `
        -AppendLog:$AppendLog `
        -DiscardOutput:$DiscardOutput
}
