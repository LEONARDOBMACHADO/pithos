#requires -Version 5.1
param(
    [switch]$SelfTest
)

$runnerPath = Join-Path $PSScriptRoot 'validate-gate-c3-windows.ps1'
& $runnerPath -SelfTest:$SelfTest
