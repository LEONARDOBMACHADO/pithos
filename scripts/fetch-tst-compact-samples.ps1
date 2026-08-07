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

# SampleFile.com documents both /samples/api/files?format=<ext> and stable
# per-format manifest.json endpoints. The generic API is preferred; manifests
# are the deterministic fallback. A final random endpoint fallback keeps a
# format usable if its listing/manifest route changes temporarily.
$targets = @(
    @{ Format='txt';    Folder='text';       MiB=10 },
    @{ Format='csv';    Folder='structured'; MiB=25 },
    @{ Format='json';   Folder='structured'; MiB=25 },
    @{ Format='xml';    Folder='structured'; MiB=25 },
    @{ Format='sql';    Folder='structured'; MiB=25 },
    @{ Format='log';    Folder='text';       MiB=10 },
    @{ Format='pdf';    Folder='documents';  MiB=10 },
    @{ Format='pdf';    Folder='documents';  MiB=25 },
    @{ Format='docx';   Folder='documents';  MiB=10 },
    @{ Format='xlsx';   Folder='documents';  MiB=10 },
    @{ Format='pptx';   Folder='documents';  MiB=25 },
    @{ Format='bmp';    Folder='images';     MiB=25 },
    @{ Format='tiff';   Folder='images';     MiB=25 },
    @{ Format='png';    Folder='images';     MiB=10 },
    @{ Format='png';    Folder='images';     MiB=25 },
    @{ Format='jpeg';   Folder='images';     MiB=25 },
    @{ Format='webp';   Folder='images';     MiB=25 },
    @{ Format='wav';    Folder='audio';      MiB=50 },
    @{ Format='flac';   Folder='audio';      MiB=25 },
    @{ Format='mp3';    Folder='audio';      MiB=25 },
    @{ Format='mp4';    Folder='video';      MiB=100 },
    @{ Format='mkv';    Folder='video';      MiB=50 },
    @{ Format='sqlite'; Folder='databases';  MiB=25 },
    @{ Format='tar';    Folder='archives';   MiB=50 },
    @{ Format='zip';    Folder='archives';   MiB=50 },
    @{ Format='7z';     Folder='archives';   MiB=50 },
    @{ Format='rar';    Folder='archives';   MiB=50 },
    @{ Format='gz';     Folder='archives';   MiB=25 },
    @{ Format='wasm';   Folder='binaries';   MiB=10 }
)

$manifestCategories = @{
    txt    = @('document', 'data')
    csv    = @('data', 'document')
    json   = @('data', 'document', 'code', 'log')
    xml    = @('data', 'document', 'code')
    sql    = @('data', 'code')
    log    = @('log', 'data')
    pdf    = @('document')
    docx   = @('document')
    xlsx   = @('document')
    pptx   = @('document')
    bmp    = @('image')
    tiff   = @('image')
    png    = @('image')
    jpeg   = @('image')
    webp   = @('image')
    wav    = @('audio')
    flac   = @('audio')
    mp3    = @('audio')
    mp4    = @('video')
    mkv    = @('video')
    sqlite = @('data')
    tar    = @('archive')
    zip    = @('archive')
    '7z'   = @('archive')
    rar    = @('archive')
    gz     = @('archive')
    wasm   = @('code')
}

foreach ($folder in ($targets.Folder | Sort-Object -Unique)) {
    New-Item -ItemType Directory -Force -Path (Join-Path $corpusPath $folder) | Out-Null
}
New-Item -ItemType Directory -Force -Path (Join-Path $corpusPath 'duplicates') | Out-Null

$selectedByFormat = @{}
$sourceRows = New-Object System.Collections.Generic.List[object]
$missingRows = New-Object System.Collections.Generic.List[string]

function Test-FixtureShape {
    param([AllowNull()][object]$Value)
    if ($null -eq $Value) { return $false }
    if (($null -eq $Value.PSObject.Properties['name']) -or
        ($null -eq $Value.PSObject.Properties['size_bytes']) -or
        ($null -eq $Value.PSObject.Properties['sha256']) -or
        ($null -eq $Value.PSObject.Properties['download_url'])) {
        return $false
    }
    return (-not [string]::IsNullOrWhiteSpace([string]$Value.name)) -and
        ([int64]$Value.size_bytes -gt 0) -and
        (-not [string]::IsNullOrWhiteSpace([string]$Value.sha256)) -and
        (-not [string]::IsNullOrWhiteSpace([string]$Value.download_url))
}

function Expand-FixtureResponse {
    param([AllowNull()][object]$Response)

    if ($null -eq $Response) { return }
    if ($Response -is [System.Array]) {
        foreach ($item in $Response) {
            if (Test-FixtureShape $item) { Write-Output $item }
        }
        return
    }
    if (Test-FixtureShape $Response) {
        Write-Output $Response
        return
    }

    foreach ($propertyName in @('files', 'results', 'items', 'fixtures', 'data')) {
        $property = $Response.PSObject.Properties[$propertyName]
        if ($null -ne $property) {
            $expanded = @(Expand-FixtureResponse -Response $property.Value)
            if (@($expanded).Length -gt 0) {
                Write-Output $expanded
                return
            }
        }
    }
}

function Get-ManifestItems {
    param([Parameter(Mandatory=$true)][string]$Format)

    if (-not $manifestCategories.ContainsKey($Format)) { return }
    foreach ($category in @($manifestCategories[$Format])) {
        $manifestUri = "https://samplefile.com/samples/$category/$Format/manifest.json"
        try {
            $manifest = Invoke-RestMethod -Uri $manifestUri -Method Get -UseBasicParsing
            $items = @(Expand-FixtureResponse -Response $manifest)
            if (@($items).Length -gt 0) {
                Write-Host "  manifest fallback: $manifestUri"
                Write-Output $items
                return
            }
        }
        catch {
            # Category candidates are intentionally speculative fallbacks. A 404
            # here is not itself a failed corpus acquisition.
        }
    }
}

function Get-ApiItems {
    param([Parameter(Mandatory=$true)][string]$Format)

    $filesUri = "https://samplefile.com/samples/api/files?format=$Format"
    try {
        $response = Invoke-RestMethod -Uri $filesUri -Method Get -UseBasicParsing
        $items = @(Expand-FixtureResponse -Response $response)
        if (@($items).Length -gt 0) {
            Write-Output $items
            return
        }
        Write-Warning "Files API returned no usable fixtures for .$Format; trying manifest"
    }
    catch {
        Write-Warning "Files API failed for .$Format : $($_.Exception.Message); trying manifest"
    }

    $manifestItems = @(Get-ManifestItems -Format $Format)
    if (@($manifestItems).Length -gt 0) {
        Write-Output $manifestItems
        return
    }

    $randomUri = "https://samplefile.com/samples/api/random?format=$Format"
    try {
        $randomResponse = Invoke-RestMethod -Uri $randomUri -Method Get -UseBasicParsing
        $randomItems = @(Expand-FixtureResponse -Response $randomResponse)
        if (@($randomItems).Length -gt 0) {
            Write-Warning "Using random fallback for .$Format because list/manifest were unavailable"
            Write-Output $randomItems
            return
        }
    }
    catch {
        Write-Warning "Random API failed for .$Format : $($_.Exception.Message)"
    }
}

foreach ($target in $targets) {
    $format = [string]$target.Format
    $targetBytes = [long]$target.MiB * 1MB
    if (-not $selectedByFormat.ContainsKey($format)) {
        $selectedByFormat[$format] = New-Object -TypeName 'System.Collections.Generic.HashSet[string]' -ArgumentList ([System.StringComparer]::OrdinalIgnoreCase)
    }
    $used = $selectedByFormat[$format]
    Write-Host "Selecting .$format near $($target.MiB) MiB..." -ForegroundColor Cyan

    # Keep the empty-collection branch in this scope. Windows PowerShell 5.1 can
    # reject an empty array while binding it to another advanced-function array
    # parameter even when AllowEmptyCollection is present.
    $items = @(Get-ApiItems -Format $format)
    if (@($items).Length -eq 0) {
        $missingRows.Add("format=$format,target_mib=$($target.MiB),reason=no_fixture")
        Write-Warning "No usable fixture returned for .$format"
        continue
    }

    $eligible = @($items | Where-Object {
        (Test-FixtureShape $_) -and -not $used.Contains([string]$_.name)
    })
    if (@($eligible).Length -eq 0) {
        $missingRows.Add("format=$format,target_mib=$($target.MiB),reason=no_unused_fixture")
        Write-Warning "No unused fixture remains for .$format"
        continue
    }
    $fixture = @($eligible | Sort-Object { [math]::Abs(([double]$_.size_bytes) - $targetBytes) })[0]

    [void]$used.Add([string]$fixture.name)
    $folderPath = Join-Path $corpusPath ([string]$target.Folder)
    $destination = Join-Path $folderPath ([string]$fixture.name)
    $expectedHash = ([string]$fixture.sha256).ToLowerInvariant()
    $downloadUrl = [string]$fixture.download_url
    if ($downloadUrl.StartsWith('/')) {
        $downloadUrl = "https://samplefile.com$downloadUrl"
    }

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
            Invoke-WebRequest -Uri $downloadUrl -OutFile $partial -UseBasicParsing
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
        source_url = $downloadUrl
        source_description = 'SampleFile.com SHA-256 verified fixture'
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
        if (@($candidate).Length -eq 0) {
            $missingRows.Add("duplicate_control=$($control.Label),reason=no_source")
            continue
        }
        $source = $candidate[0]
        $duplicateName = "{0}.dup1{1}" -f [System.IO.Path]::GetFileNameWithoutExtension($source.Name), $source.Extension
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

$sourceRegister = Join-Path $resultsPath 'source-register.csv'
if ($sourceRows.Count -gt 0) {
    $sourceRows | Sort-Object relative_path | Export-Csv -LiteralPath $sourceRegister -NoTypeInformation -Encoding UTF8
} else {
    [System.IO.File]::WriteAllText(
        $sourceRegister,
        '"relative_path","format","target_mib","actual_mib","sha256","source_url","source_description"' + [Environment]::NewLine,
        (New-Object System.Text.UTF8Encoding -ArgumentList $false)
    )
}

$missingPath = Join-Path $resultsPath 'download-missing.txt'
[System.IO.File]::WriteAllLines($missingPath, [string[]]$missingRows.ToArray(), (New-Object System.Text.UTF8Encoding -ArgumentList $false))

Write-Host "`nDownload pass complete." -ForegroundColor Green
Write-Host "Registered fixtures: $($sourceRows.Count)"
Write-Host "Missing selections/controls: $($missingRows.Count)"
Write-Host "Source register: $sourceRegister"
Write-Host "Missing report: $missingPath"
Write-Host "Run inventory-tst-compact.ps1 next to freeze the actual corpus manifest."
