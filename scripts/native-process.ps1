#requires -Version 5.1

Set-StrictMode -Version Latest

function Invoke-PithosNativeProcess {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory=$true)][string]$FilePath,
        [string[]]$Arguments = @(),
        [string]$LogPath = '',
        [switch]$AppendLog,
        [switch]$DiscardOutput
    )

    $previousErrorActionPreference = $ErrorActionPreference
    $exitCode = $null

    try {
        # Windows PowerShell 5.1 can promote native stderr redirected through
        # 2>&1 into PowerShell ErrorRecord objects. Pithos and Cargo both use
        # stderr for legitimate diagnostics/telemetry, so native process
        # success must be decided by the process exit code, never by stderr.
        $ErrorActionPreference = 'Continue'

        if ([string]::IsNullOrWhiteSpace($LogPath)) {
            & $FilePath @Arguments 2>&1 | ForEach-Object {
                if (-not $DiscardOutput) { Write-Host $_ }
            }
        } elseif ($AppendLog) {
            & $FilePath @Arguments 2>&1 |
                Tee-Object -FilePath $LogPath -Append |
                ForEach-Object {
                    if (-not $DiscardOutput) { Write-Host $_ }
                }
        } else {
            & $FilePath @Arguments 2>&1 |
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
        throw "Native process completed without an exit code: $FilePath"
    }

    return [int]$exitCode
}
