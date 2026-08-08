#requires -Version 5.1
param(
    [Parameter(Mandatory=$true)][string]$TracePath,
    [Parameter(Mandatory=$true)][string]$EvidencePath,
    [Parameter(Mandatory=$true)][string]$Branch,
    [Parameter(Mandatory=$true)][string]$SourceCommit
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not (Test-Path -LiteralPath $TracePath -PathType Leaf)) {
    throw "PRS1 trace not found: $TracePath"
}
if (-not (Test-Path -LiteralPath $EvidencePath -PathType Container)) {
    throw "PRS1 evidence directory not found: $EvidencePath"
}

function Parse-TraceLine([string]$Line) {
    $marker = 'PITHOS_REP_TRACE'
    $markerIndex = $Line.IndexOf($marker, [System.StringComparison]::Ordinal)
    if ($markerIndex -lt 0) { return $null }
    $trace = $Line.Substring($markerIndex)
    $map = [ordered]@{}
    foreach ($part in ($trace -split "`t")) {
        if ($part -eq $marker) { continue }
        $pair = $part -split '=', 2
        if ($pair.Count -eq 2) { $map[$pair[0]] = $pair[1] }
    }
    if ($map.Count -eq 0) { return $null }
    return [pscustomobject]$map
}

$traceRows = New-Object System.Collections.Generic.List[object]
$currentCase = $null
$currentProfile = $null
$currentArchiveCandidate = $null

foreach ($line in Get-Content -LiteralPath $TracePath) {
    $row = Parse-TraceLine $line
    if ($null -eq $row) { continue }

    if ($row.stage -eq 'benchmark_case') {
        $currentCase = $row.case
        $currentProfile = $row.profile
        $currentArchiveCandidate = $null
        continue
    }

    if ($row.stage -eq 'archive_scope') {
        if ($row.phase -eq 'begin') {
            if (-not [string]::IsNullOrWhiteSpace($currentArchiveCandidate)) {
                throw "Nested archive scope detected: current=$currentArchiveCandidate next=$($row.candidate)"
            }
            $currentArchiveCandidate = $row.candidate
        } elseif ($row.phase -eq 'end') {
            if ($currentArchiveCandidate -ne $row.candidate) {
                throw "Archive scope mismatch: current=$currentArchiveCandidate end=$($row.candidate)"
            }
            $currentArchiveCandidate = $null
        } else {
            throw "Unknown archive scope phase: $($row.phase)"
        }
        continue
    }

    if (-not [string]::IsNullOrWhiteSpace($currentCase)) {
        $row | Add-Member -NotePropertyName case -NotePropertyValue $currentCase -Force
        $row | Add-Member -NotePropertyName benchmark_profile -NotePropertyValue $currentProfile -Force
    }
    if (-not [string]::IsNullOrWhiteSpace($currentArchiveCandidate)) {
        $row | Add-Member -NotePropertyName planner_candidate -NotePropertyValue $currentArchiveCandidate -Force
    }
    $traceRows.Add($row)
}

if ($traceRows.Count -eq 0) {
    throw 'No PITHOS_REP_TRACE rows captured.'
}
if (-not [string]::IsNullOrWhiteSpace($currentArchiveCandidate)) {
    throw "Unclosed archive scope at end of trace: $currentArchiveCandidate"
}

$traceRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-trace.csv') -NoTypeInformation -Encoding UTF8

$archiveWinnerRows = @($traceRows | Where-Object { $_.stage -eq 'archive_winner' })
if ($archiveWinnerRows.Count -eq 0) {
    throw 'No archive_winner rows captured.'
}
if (@($archiveWinnerRows | Where-Object { [string]::IsNullOrWhiteSpace($_.case) }).Count -gt 0) {
    throw 'archive_winner row is missing benchmark case context.'
}

$winnerByCase = @{}
foreach ($row in $archiveWinnerRows) {
    if ($winnerByCase.ContainsKey($row.case)) {
        throw "Multiple archive_winner rows for benchmark case: $($row.case)"
    }
    if ($row.winner -notin @('class-aware','global')) {
        throw "Unknown archive winner '$($row.winner)' for case $($row.case)"
    }
    $winnerByCase[$row.case] = $row.winner
}

$allRaceRows = @($traceRows | Where-Object { $_.stage -eq 'representation_race' })
$allSummaryRows = @($traceRows | Where-Object { $_.stage -eq 'prs1_summary' })
$allPlaneRows = @($traceRows | Where-Object { $_.stage -eq 'prs1_plane' })
$allCandidateRows = @($allRaceRows + $allSummaryRows + $allPlaneRows)

if (@($allCandidateRows | Where-Object { [string]::IsNullOrWhiteSpace($_.case) }).Count -gt 0) {
    throw 'PRS1 candidate trace row is missing benchmark case context.'
}
if (@($allCandidateRows | Where-Object { [string]::IsNullOrWhiteSpace($_.planner_candidate) }).Count -gt 0) {
    throw 'PRS1 candidate trace row is missing archive planner scope.'
}

$unexpectedLevels = @($allCandidateRows | Where-Object { $_.level -notin @('3','15') })
if ($unexpectedLevels.Count -gt 0) {
    $unexpectedLevels | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-unexpected-trace-levels.csv') -NoTypeInformation -Encoding UTF8
    throw "Unexpected PRS1 trace level(s) found: $($unexpectedLevels.Count)"
}

function Is-WinningArchiveRow([object]$Row) {
    return $winnerByCase.ContainsKey($Row.case) -and $winnerByCase[$Row.case] -eq $Row.planner_candidate
}

$winningRaceRows = @($allRaceRows | Where-Object { Is-WinningArchiveRow $_ })
$winningSummaryRows = @($allSummaryRows | Where-Object { Is-WinningArchiveRow $_ })
$winningPlaneRows = @($allPlaneRows | Where-Object { Is-WinningArchiveRow $_ })

$probeRaceRows = @($winningRaceRows | Where-Object { $_.level -eq '3' })
$probeSummaryRows = @($winningSummaryRows | Where-Object { $_.level -eq '3' })
$probePlaneRows = @($winningPlaneRows | Where-Object { $_.level -eq '3' })
$raceRows = @($winningRaceRows | Where-Object { $_.level -eq '15' })
$summaryRows = @($winningSummaryRows | Where-Object { $_.level -eq '15' })
$planeRows = @($winningPlaneRows | Where-Object { $_.level -eq '15' })

if ($raceRows.Count -eq 0) {
    throw 'No full ArchiveMax representation_race rows from winning archive candidates captured.'
}
if ($summaryRows.Count -eq 0) {
    throw 'No full ArchiveMax prs1_summary rows from winning archive candidates captured.'
}
if ($raceRows.Count -ne $summaryRows.Count) {
    throw "Full race/summary cardinality mismatch: races=$($raceRows.Count) summaries=$($summaryRows.Count)"
}

$expectedPlaneRows = $summaryRows.Count * 8
if ($planeRows.Count -ne $expectedPlaneRows) {
    throw "Physical PRS1 plane evidence incomplete: actual=$($planeRows.Count) expected=$expectedPlaneRows"
}

$raceRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-winning-races.csv') -NoTypeInformation -Encoding UTF8
$summaryRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-winning-summaries.csv') -NoTypeInformation -Encoding UTF8
$planeRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-plane-trace.csv') -NoTypeInformation -Encoding UTF8
$archiveWinnerRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-archive-winners.csv') -NoTypeInformation -Encoding UTF8

$planeSummary = @(
    $planeRows |
        Group-Object plane |
        Sort-Object { [int]$_.Name } |
        ForEach-Object {
            $group = @($_.Group)
            $rawBytes = [int64](($group | ForEach-Object { [int64]$_.raw_bytes } | Measure-Object -Sum).Sum)
            $encodedBytes = [int64](($group | ForEach-Object { [int64]$_.encoded_bytes } | Measure-Object -Sum).Sum)
            [pscustomobject]@{
                plane = [int]$_.Name
                records = $group.Count
                raw_bytes = $rawBytes
                encoded_bytes = $encodedBytes
                savings_bytes = $rawBytes - $encodedBytes
                store_records = @($group | Where-Object { $_.codec_id -eq '0' }).Count
                zstd_records = @($group | Where-Object { $_.codec_id -eq '1' }).Count
                lzma2_records = @($group | Where-Object { $_.codec_id -eq '3' }).Count
            }
        }
)
$planeSummary | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-plane-summary.csv') -NoTypeInformation -Encoding UTF8

$raceByCase = @(
    $raceRows |
        Group-Object case |
        Sort-Object Name |
        ForEach-Object {
            $group = @($_.Group)
            [pscustomobject]@{
                case = $_.Name
                archive_winner = $winnerByCase[$_.Name]
                races = $group.Count
                prs1_wins = @($group | Where-Object { $_.winner -eq 'prs1' }).Count
                v12_wins = @($group | Where-Object { $_.winner -eq 'v12' }).Count
                v17_wins = @($group | Where-Object { $_.winner -eq 'v17' }).Count
                max_group_input_bytes = [int64](($group | ForEach-Object { [int64]$_.input_bytes } | Measure-Object -Maximum).Maximum)
            }
        }
)
$raceByCase | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-races-by-case.csv') -NoTypeInformation -Encoding UTF8

$familyByCase = @(
    $summaryRows |
        Group-Object case |
        Sort-Object Name |
        ForEach-Object {
            $group = @($_.Group)
            $familySum = {
                param([string]$Property)
                [int64](($group | ForEach-Object { if ($_.$Property) { [int64]$_.$Property } else { 0 } } | Measure-Object -Sum).Sum)
            }
            [pscustomobject]@{
                case = $_.Name
                archive_winner = $winnerByCase[$_.Name]
                candidates = $group.Count
                raw = & $familySum 'raw'
                exact_ref = & $familySum 'exact_ref'
                overlay = & $familySum 'overlay'
                overlay_xor = & $familySum 'overlay_xor'
                mixture = & $familySum 'mixture'
                mixture_combinadic = & $familySum 'mixture_combinadic'
                axial = & $familySum 'axial'
                axial_xor = & $familySum 'axial_xor'
                axial_even_odd = & $familySum 'axial_even_odd'
                defect = & $familySum 'defect'
                periodic_defect = & $familySum 'periodic_defect'
                transition = & $familySum 'transition'
                delta_transition = & $familySum 'delta_transition'
            }
        }
)
$familyByCase | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-families-by-case.csv') -NoTypeInformation -Encoding UTF8

$planeByCase = @(
    $planeRows |
        Group-Object case,plane |
        ForEach-Object {
            $group = @($_.Group)
            $rawBytes = [int64](($group | ForEach-Object { [int64]$_.raw_bytes } | Measure-Object -Sum).Sum)
            $encodedBytes = [int64](($group | ForEach-Object { [int64]$_.encoded_bytes } | Measure-Object -Sum).Sum)
            [pscustomobject]@{
                case = $group[0].case
                archive_winner = $winnerByCase[$group[0].case]
                plane = [int]$group[0].plane
                records = $group.Count
                raw_bytes = $rawBytes
                encoded_bytes = $encodedBytes
                savings_bytes = $rawBytes - $encodedBytes
                store_records = @($group | Where-Object { $_.codec_id -eq '0' }).Count
                zstd_records = @($group | Where-Object { $_.codec_id -eq '1' }).Count
                lzma2_records = @($group | Where-Object { $_.codec_id -eq '3' }).Count
            }
        } |
        Sort-Object case,plane
)
$planeByCase | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-planes-by-case.csv') -NoTypeInformation -Encoding UTF8

$prs1Wins = @($raceRows | Where-Object { $_.winner -eq 'prs1' }).Count
$v12Wins = @($raceRows | Where-Object { $_.winner -eq 'v12' }).Count
$v17Wins = @($raceRows | Where-Object { $_.winner -eq 'v17' }).Count
$probePrs1Wins = @($probeRaceRows | Where-Object { $_.winner -eq 'prs1' }).Count
$probeV12Wins = @($probeRaceRows | Where-Object { $_.winner -eq 'v12' }).Count
$probeV17Wins = @($probeRaceRows | Where-Object { $_.winner -eq 'v17' }).Count

$sum = {
    param([string]$Property)
    [int64](($summaryRows | ForEach-Object { if ($_.$Property) { [int64]$_.$Property } else { 0 } } | Measure-Object -Sum).Sum)
}
$planeRawTotal = [int64](($planeRows | ForEach-Object { [int64]$_.raw_bytes } | Measure-Object -Sum).Sum)
$planeEncodedTotal = [int64](($planeRows | ForEach-Object { [int64]$_.encoded_bytes } | Measure-Object -Sum).Sum)

$summaryPath = Join-Path $EvidencePath 'PRS1_R5_SUMMARY.txt'
@(
    "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
    "branch=$Branch",
    "source_commit=$SourceCommit",
    'native_process_failure_policy=EXIT_CODE_ONLY',
    'benchmark_profiles=archive-max-only',
    'evidence_scope=winning_archive_candidate_and_full_level_15_only',
    "cases_with_archive_winner=$($winnerByCase.Count)",
    "cases_with_full_races=$($raceByCase.Count)",
    "representation_races=$($raceRows.Count)",
    "prs1_internal_wins=$prs1Wins",
    "v12_internal_wins=$v12Wins",
    "v17_internal_wins=$v17Wins",
    "prs1_candidate_summaries=$($summaryRows.Count)",
    "physical_plane_records=$($planeRows.Count)",
    "physical_plane_raw_bytes=$planeRawTotal",
    "physical_plane_encoded_bytes=$planeEncodedTotal",
    "physical_plane_store_records=$(@($planeRows | Where-Object { $_.codec_id -eq '0' }).Count)",
    "physical_plane_zstd_records=$(@($planeRows | Where-Object { $_.codec_id -eq '1' }).Count)",
    "physical_plane_lzma2_records=$(@($planeRows | Where-Object { $_.codec_id -eq '3' }).Count)",
    "probe_representation_races=$($probeRaceRows.Count)",
    "probe_prs1_internal_wins=$probePrs1Wins",
    "probe_v12_internal_wins=$probeV12Wins",
    "probe_v17_internal_wins=$probeV17Wins",
    "probe_candidate_summaries=$($probeSummaryRows.Count)",
    "probe_plane_records=$($probePlaneRows.Count)",
    "raw_cells=$(& $sum 'raw')",
    "exact_ref_cells=$(& $sum 'exact_ref')",
    "overlay_cells=$(& $sum 'overlay')",
    "overlay_xor_cells=$(& $sum 'overlay_xor')",
    "mixture_cells=$(& $sum 'mixture')",
    "mixture_combinadic_cells=$(& $sum 'mixture_combinadic')",
    "axial_cells=$(& $sum 'axial')",
    "axial_xor_cells=$(& $sum 'axial_xor')",
    "axial_even_odd_cells=$(& $sum 'axial_even_odd')",
    "defect_cells=$(& $sum 'defect')",
    "periodic_defect_cells=$(& $sum 'periodic_defect')",
    "transition_cells=$(& $sum 'transition')",
    "delta_transition_cells=$(& $sum 'delta_transition')",
    'NOTE=internal_wins_are_within_native_v18; final_group_choice_requires group-choice telemetry',
    '7zip_executed=False',
    'winrar_executed=False',
    'winzip_executed=False'
) | Set-Content -LiteralPath $summaryPath -Encoding UTF8

Write-Host 'PRS1 R5 trace analysis PASS' -ForegroundColor Green
Write-Host "Summary: $summaryPath"
Write-Host "Race-by-case: $(Join-Path $EvidencePath 'prs1-races-by-case.csv')"
Write-Host "Family-by-case: $(Join-Path $EvidencePath 'prs1-families-by-case.csv')"
Write-Host "Physical planes: $(Join-Path $EvidencePath 'prs1-plane-summary.csv')"
exit 0
