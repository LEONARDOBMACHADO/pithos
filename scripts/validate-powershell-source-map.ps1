#requires -Version 5.1

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$scriptRoot = (Resolve-Path -LiteralPath $PSScriptRoot).Path
$scripts = @(Get-ChildItem -LiteralPath $scriptRoot -File -Filter '*.ps1' | Sort-Object FullName)
if ($scripts.Count -eq 0) {
    throw "No PowerShell scripts found under $scriptRoot"
}

$parseFailures = New-Object System.Collections.Generic.List[object]
$ambiguousFailures = New-Object System.Collections.Generic.List[object]
$ambiguousPattern = '\$[A-Za-z_][A-Za-z0-9_]*:(?![A-Za-z_][A-Za-z0-9_]*)'

foreach ($script in $scripts) {
    $tokens = $null
    $parseErrors = $null
    [System.Management.Automation.Language.Parser]::ParseFile(
        $script.FullName,
        [ref]$tokens,
        [ref]$parseErrors
    ) | Out-Null

    foreach ($parseError in @($parseErrors)) {
        $parseFailures.Add([pscustomobject]@{
            path = $script.FullName
            line = $parseError.Extent.StartLineNumber
            column = $parseError.Extent.StartColumnNumber
            error_id = $parseError.ErrorId
            text = $parseError.Extent.Text
            message = $parseError.Message
        })
    }

    # Only inspect strings that PowerShell actually expands. This deliberately
    # ignores comments and single-quoted literals while still catching the
    # `$name:` ambiguity that PowerShell interprets as a drive-qualified
    # variable. Legitimate scoped references such as $env:TEMP and
    # $global:LASTEXITCODE do not match because the colon is followed by a
    # valid variable-name character.
    foreach ($token in @($tokens | Where-Object {
        $_.Kind -in @('StringExpandable', 'HereStringExpandable')
    })) {
        foreach ($match in [regex]::Matches($token.Text, $ambiguousPattern)) {
            $ambiguousFailures.Add([pscustomobject]@{
                path = $script.FullName
                line = $token.Extent.StartLineNumber
                token = $match.Value
                source = $token.Text
            })
        }
    }
}

if ($ambiguousFailures.Count -gt 0) {
    Write-Host "`n===== COMPLETE AMBIGUOUS VARIABLE-COLON MAP =====" -ForegroundColor Red
    $ambiguousFailures |
        Sort-Object path,line |
        Format-Table path,line,token,source -Wrap -AutoSize |
        Out-String |
        Write-Host
}

if ($parseFailures.Count -gt 0) {
    Write-Host "`n===== COMPLETE POWERSHELL PARSE ERROR MAP =====" -ForegroundColor Red
    $parseFailures |
        Sort-Object path,line,column |
        Format-Table path,line,column,error_id,text,message -Wrap -AutoSize |
        Out-String |
        Write-Host
}

if ($ambiguousFailures.Count -gt 0 -or $parseFailures.Count -gt 0) {
    throw "PowerShell source preflight failed: ambiguous=$($ambiguousFailures.Count) parse_errors=$($parseFailures.Count)"
}

Write-Host "POWERSHELL SOURCE MAP: PASS ($($scripts.Count) scripts; 0 ambiguous variable-colon references; 0 parse errors)" -ForegroundColor Green
