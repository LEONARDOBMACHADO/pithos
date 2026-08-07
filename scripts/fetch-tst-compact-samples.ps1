#requires -Version 5.1
param(
    [string]$Corpus = "tst_compact",
    [switch]$Force,
    [switch]$SkipDuplicates
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repo
$corpusPath = if ([System.IO.Path]::IsPathRooted($Corpus)) { $Corpus } else { Join-Path $repo $Corpus }
New-Item -ItemType Directory -Force -Path $corpusPath | Out-Null
$resultsPath = Join-Path $corpusPath 'results'
New-Item -ItemType Directory -Force -Path $resultsPath | Out-Null

# The generic SampleFile API returns name, size_bytes, sha256 and download_url.
# Multiple targets for one format intentionally select different fixtures.
$targets = @(
    @{ Format='txt';   Folder='text';       MiB=10 },
    @{ Format='csv';   Folder='structured'; MiB=25 },
    @{ Format='json';  Folder='structured'; MiB=25 },
    @{ Format='xml';   Folder='structured'; MiB=25 },
    @{ Format='sql';   Folder='structured'; MiB=25 },
    @{ Format='log';   Folder='text';       MiB=10 },
    @{ Format='pdf';   Folder='documents';  MiB=10 },
    @{ Format='pdf';   Folder='documents';  MiB=25 },
    @{ Format='docx';  Folder='documents';  MiB=10 },
    @{ Format='xlsx';  Folder='documents';  MiB=10 },
    @{ Format='pptx';  Folder='documents';  MiB=25 },
    @{ Format='bmp';   Folder='images';     MiB=25 },
    @{ Format='tiff';  Folder='images';     MiB=25 },
    @{ Format='png';   Folder='images';     MiB=10 },
    @{ Format='png';   Folder='images';     MiB=25 },
    @{ Format='jpg';   Folder='images';     MiB=25 },
    @{ Format='webp';  Folder='images';     MiB=25 },
    @{ Format='wav';   Folder='audio';      MiB=50 },
    @{ Format='flac';  Folder='audio';      MiB=25 },
    @{ Format='mp3';   Folder='audio';      MiB=25 },
    @{ Format='mp4';   Folder='video';      MiB=100 },
    @{ Format='mkv';   Folder='video';      MiB=50 },
    @{ Format='sqlite';Folder='databases';  MiB=25 },
    @{ Format='tar';   Folder='archives';   MiB=50 },
    @{ Format='zip';   Folder='archives';   MiB=50 },
    @{ Format='7z';    Folder='archives';   MiB=50 },
    @{ Format='rar';   Folder='archives';   MiB=50 },
    @{ Format='gz';    Folder='archives';   MiB=25 },
    @{ Format='wasm';  Folder='binaries';   MiB=10 }
)

foreach ($folder in ($targets.Folder | Sort-Object -Unique)) {
    New-Item -ItemType Directory -Force -Path (Join-Path $corpusPath $folder) | Out-Null
}
New-Item -ItemType Directory -Force -Path (Join-Path $corpusPath 'duplicates') | Out-Null

$selectedByFormat = @{}
$sourceRows = New-Object System.Collections.Generic.List[object]
$missingRows = New-Object System.Collections.Generic.List[string]

function Get-ApiItems {
    param([Parameter(Mandatory=$true)][string]$Format)
    $uri = "https://samplefile.com/samples/api/files?format=$Format"
    try {
        $response = Invoke-RestMethod -Uri $uri -Method Get -UseBasicParsing
    }
    catch {
        Write-Warning "API failed for .$Format : $($_.Exception.Message)"
        return @()
    }

    if ($response -is [System.Array]) { return @($response) }
    if ($null -ne $response.PSObject.Properties['files']) { return @($response.files) }
    if ($null -ne $response.PSObject.Properties['results']) { return @($response.results) }
    if ($null -ne $response.PSObject.Properties['data']) { return @($response.data) }
    return @($response)
}

function Select-ClosestFixture {
    param(
        [Parameter(Mandatory=$true)][AllowEmptyCollection()][object[]]$Items,
        [Parameter(Mandatory=$true)][long]$TargetBytes,
        [Parameter(Mandatory=$true)][System.Collections.Generic.HashSet[string]]$AlreadyUsed
    )
    $eligible = @($Items | Where-Object {
        $null -ne $_.name -and $null -ne $_.size_bytes -and $null -ne $_.sha256 -and
        $null -ne $_.download_url -and -not $AlreadyUsed.Contains([string]$_.name)
    })
    if ($eligible.Count -eq 0) { return $null }
    return @($eligible | Sort-Object { [math]::Abs(([double]$_.size_bytes) - $TargetBytes) })[0]
}

foreach ($target in $targets) {
    $format = [string]$target.Format
    $targetBytes = [long]$target.MiB * 1MB
    if (-not $selectedByFormat.ContainsKey($format)) {
        $selectedByFormat[$format] = New-Object -TypeName 'System.Collections.Generic.HashSet[string]' -ArgumentList ([System.StringComparer]::OrdinalIgnoreCase)
    }
    $used = $selectedByFormat[$format]
    Write-Host "Selecting .$format near $($target.MiB) MiB..." -ForegroundColor Cyan
    $items = @(Get-ApiItems -Format $format)
    $fixture = Select-ClosestFixture -Items $items -TargetBytes $targetBytes -AlreadyUsed $used
    if ($null -eq $fixture) {
        $missingRows.Add("format=$format,target_mib=$($target.MiB),reason=no_fixture")
        Write-Warning "No usable fixture returned for .$format"
        continue
    }

    [void]$used.Add([string]$fixture.name)
    $folderPath = Join-Path $corpusPath ([string]$target.Folder)
    $destination = Join-Path $folderPath ([string]$fixture.name)
    $expectedHash = ([string]$fixture.sha256).ToLowerInvariant()

    $downloadRequired = $true
    if (Test-Path -LiteralPath $destination -PathType Leaf) {
        $currentHash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($currentHash -eq $expectedHash) {
            $downloadRequired = $false
            Write-Host "  existing SHA-256 matches: $([string]$fixture.name)"
        } elseif (-not $Force) {
            throw "Existing file has wrong SHA-256: $destination (use -Force to replace)"
        }
    }

    if ($downloadRequired) {
        Write-Host "  downloading $([string]$fixture.name) ($([math]::Round(([double]$fixture.size_bytes / 1MB), 3)) MiB)"
        $partial = "$destination.partial"
        Remove-Item -LiteralPath $partial -Force -ErrorAction SilentlyContinue
        try {
            Invoke-WebRequest -Uri ([string]$fixture.download_url) -OutFile $partial -UseBasicParsing
            $actualHash = (Get-FileHash -LiteralPath $partial -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actualHash -ne $expectedHash) {
                throw "SHA-256 mismatch for $([string]$fixture.name): expected $expectedHash got $actualHash"
            }
            Move-Item -LiteralPath $partial -Destination $destination -Force
        }
        finally {
            Remove-Item -LiteralPath $partial -Force -ErrorAction SilentlyContinue
        }
    }

    $actualBytes = (Get-Item -LiteralPath $destination).Length
    $relative = $destination.Substring($corpusPath.Length).TrimStart([char[]]@([char]92, [char]47)).Replace([char]92, [char]47)
    $sourceRows.Add([pscustomobject]@{
        relative_path = $relative
        format = $format
        target_mib = [int]$target.MiB
        actual_mib = [math]::Round($actualBytes / 1MB, 3)
        sha256 = $expectedHash
        source_url = [string]$fixture.download_url
        source_description = 'SampleFile.com verified fixture'
    })
}

if (-not $SkipDuplicates) {
    $duplicateRoot = Join-Path $corpusPath 'duplicates'
    $controlPatterns = @(
        @{ Label='structured'; Extensions=@('.txt','.csv','.json') },
        @{ Label='pdf'; Extensions=@('.pdf') },
        @{ Label='image'; Extensions=@('.png','.jpg','.jpeg') },
        @{ Label='archive'; Extensions=@('.zip','.7z','.rar') }
    )

    $downloadedFiles = @(Get-ChildItem -LiteralPath $corpusPath -File -Recurse |
        Where-Object { -not $_.FullName.StartsWith($resultsPath, [System.StringComparison]::OrdinalIgnoreCase) -and -not $_.FullName.StartsWith($duplicateRoot, [System.StringComparison]::OrdinalIgnoreCase) })

    foreach ($control in $controlPatterns) {
        $candidate = @($downloadedFiles | Where-Object { $control.Extensions -contains $_.Extension.ToLowerInvariant() } | Sort-Object Length -Descending | Select-Object -First 1)
        if ($candidate.Count -eq 0) {
            $missingRows.Add("duplicate_control=$($control.Label),reason=no_source")
            continue
        }
        $source = $candidate[0]
        for ($copyIndex = 1; $copyIndex -le 2; $copyIndex++) {
            $duplicateName = "{0}.dup{1}{2}" -f [System.IO.Path]::GetFileNameWithoutExtension($source.Name), $copyIndex, $source.Extension
            $destination = Join-Path $duplicateRoot $duplicateName
            Copy-Item -LiteralPath $source.FullName -Destination $destination -Force
            $hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
            $relative = $destination.Substring($corpusPath.Length).TrimStart([char[]]@([char]92, [char]47)).Replace([char]92, [char]47)
            $sourceRows.Add([pscustomobject]@{
                relative_path = $relative
                format = $source.Extension.TrimStart([char]'.').ToLowerInvariant()
                target_mib = [math]::Round($source.Length / 1MB, 3)
                actual_mib = [math]::Round($source.Length / 1MB, 3)
                sha256 = $hash
                source_url = 'local-byte-exact-copy'
                source_description = "exact-dedup control copied from $($source.Name)"
            })
        }
    }
}

$sourceRegister = Join-Path $resultsPath 'source-register.csv'
$sourceRows | Sort-Object relative_path | Export-Csv -LiteralPath $sourceRegister -NoTypeInformation -Encoding UTF8
$missingPath = Join-Path $resultsPath 'download-missing.txt'
[System.IO.File]::WriteAllLines($missingPath, $missingRows, (New-Object System.Text.UTF8Encoding -ArgumentList $false))

Write-Host "`nDownload pass complete." -ForegroundColor Green
Write-Host "Registered fixtures: $($sourceRows.Count)"
Write-Host "Missing selections/controls: $($missingRows.Count)"
Write-Host "Source register: $sourceRegister"
Write-Host "Missing report: $missingPath"
Write-Host "Run inventory-tst-compact.ps1 next to freeze the actual corpus manifest."
