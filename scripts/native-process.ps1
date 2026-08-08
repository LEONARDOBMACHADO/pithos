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

    # Resolve before changing ErrorActionPreference so command-not-found is a
    # normal terminating harness error, never a stale LASTEXITCODE result.
    $command = Get-Command $FilePath -ErrorAction Stop
    $resolvedPath = if ($command.Path) { $command.Path } else { $FilePath }

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
