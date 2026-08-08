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

function Trace-Key([object]$Row) {
    return "$($Row.case)|$($Row.planner_candidate)|$($Row.group)"
}

$traceRows = New-Object System.Collections.Generic.List[object]
$currentCase = $null
$currentProfile = $null
$currentArchiveCandidate = $null
$currentGroup = $null
$currentGroupParallel = $null

foreach ($line in Get-Content -LiteralPath $TracePath) {
    $row = Parse-TraceLine $line
    if ($null -eq $row) { continue }

    if ($row.stage -eq 'benchmark_case') {
        if (-not [string]::IsNullOrWhiteSpace($currentArchiveCandidate) -or
            -not [string]::IsNullOrWhiteSpace($currentGroup)) {
            throw 'Benchmark case changed while a trace scope was still open.'
        }
        $currentCase = $row.case
        $currentProfile = $row.profile
        continue
    }

    if ($row.stage -eq 'archive_scope') {
        if ($row.phase -eq 'begin') {
            if (-not [string]::IsNullOrWhiteSpace($currentArchiveCandidate)) {
                throw "Nested archive scope detected: current=$currentArchiveCandidate next=$($row.candidate)"
            }
            $currentArchiveCandidate = $row.candidate
        } elseif ($row.phase -eq 'end') {
            if (-not [string]::IsNullOrWhiteSpace($currentGroup)) {
                throw "Archive scope ended with group scope still open: group=$currentGroup"
            }
            if ($currentArchiveCandidate -ne $row.candidate) {
                throw "Archive scope mismatch: current=$currentArchiveCandidate end=$($row.candidate)"
            }
            $currentArchiveCandidate = $null
        } else {
            throw "Unknown archive scope phase: $($row.phase)"
        }
        continue
    }

    if ($row.stage -eq 'group_scope') {
        if ([string]::IsNullOrWhiteSpace($currentArchiveCandidate)) {
            throw 'Group scope encountered outside archive scope.'
        }
        if ($row.phase -eq 'begin') {
            if (-not [string]::IsNullOrWhiteSpace($currentGroup)) {
                throw "Nested group scope detected: current=$currentGroup next=$($row.group)"
            }
            $currentGroup = $row.group
            $currentGroupParallel = $row.parallel
        } elseif ($row.phase -eq 'end') {
            if ($currentGroup -ne $row.group) {
                throw "Group scope mismatch: current=$currentGroup end=$($row.group)"
            }
            $currentGroup = $null
            $currentGroupParallel = $null
        } else {
            throw "Unknown group scope phase: $($row.phase)"
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
    if (-not [string]::IsNullOrWhiteSpace($currentGroup)) {
        $row | Add-Member -NotePropertyName group -NotePropertyValue $currentGroup -Force
        $row | Add-Member -NotePropertyName group_parallel -NotePropertyValue $currentGroupParallel -Force
    }
    $traceRows.Add($row)
}

if ($traceRows.Count -eq 0) {
    throw 'No PITHOS_REP_TRACE rows captured.'
}
if (-not [string]::IsNullOrWhiteSpace($currentArchiveCandidate)) {
    throw "Unclosed archive scope at end of trace: $currentArchiveCandidate"
}
if (-not [string]::IsNullOrWhiteSpace($currentGroup)) {
    throw "Unclosed group scope at end of trace: $currentGroup"
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
$archiveWinnerRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-archive-winners.csv') -NoTypeInformation -Encoding UTF8

function Is-WinningArchiveRow([object]$Row) {
    return $winnerByCase.ContainsKey($Row.case) -and $winnerByCase[$Row.case] -eq $Row.planner_candidate
}

$allRaceRows = @($traceRows | Where-Object { $_.stage -eq 'representation_race' })
$allSummaryRows = @($traceRows | Where-Object { $_.stage -eq 'prs1_summary' })
$allPlaneRows = @($traceRows | Where-Object { $_.stage -eq 'prs1_plane' })
$allCandidateErrorRows = @($traceRows | Where-Object { $_.stage -eq 'prs1_candidate_error' })
$allGroupChoiceRows = @($traceRows | Where-Object { $_.stage -eq 'group_choice' })
$allCandidateRows = @($allRaceRows + $allSummaryRows + $allPlaneRows + $allCandidateErrorRows)

foreach ($row in $allCandidateRows + $allGroupChoiceRows) {
    if ([string]::IsNullOrWhiteSpace($row.case)) {
        throw "Trace row '$($row.stage)' is missing benchmark case context."
    }
    if ([string]::IsNullOrWhiteSpace($row.planner_candidate)) {
        throw "Trace row '$($row.stage)' is missing archive planner scope."
    }
    if ([string]::IsNullOrWhiteSpace($row.group)) {
        throw "Trace row '$($row.stage)' is missing group scope."
    }
}

$unexpectedLevels = @($allCandidateRows | Where-Object { $_.level -notin @('3','15') })
if ($unexpectedLevels.Count -gt 0) {
    $unexpectedLevels | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-unexpected-trace-levels.csv') -NoTypeInformation -Encoding UTF8
    throw "Unexpected PRS1 trace level(s) found: $($unexpectedLevels.Count)"
}

$winningRaceRows = @($allRaceRows | Where-Object { Is-WinningArchiveRow $_ })
$winningSummaryRows = @($allSummaryRows | Where-Object { Is-WinningArchiveRow $_ })
$winningPlaneRows = @($allPlaneRows | Where-Object { Is-WinningArchiveRow $_ })
$winningCandidateErrorRows = @($allCandidateErrorRows | Where-Object { Is-WinningArchiveRow $_ })
$winningGroupChoiceRows = @($allGroupChoiceRows | Where-Object { Is-WinningArchiveRow $_ })

$probeRaceRows = @($winningRaceRows | Where-Object { $_.level -eq '3' })
$probeSummaryRows = @($winningSummaryRows | Where-Object { $_.level -eq '3' })
$probePlaneRows = @($winningPlaneRows | Where-Object { $_.level -eq '3' })
$raceRows = @($winningRaceRows | Where-Object { $_.level -eq '15' })
$summaryRows = @($winningSummaryRows | Where-Object { $_.level -eq '15' })
$planeRows = @($winningPlaneRows | Where-Object { $_.level -eq '15' })
$fullCandidateErrors = @($winningCandidateErrorRows | Where-Object { $_.level -eq '15' })

if ($fullCandidateErrors.Count -gt 0) {
    $fullCandidateErrors | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-full-candidate-errors.csv') -NoTypeInformation -Encoding UTF8
    throw "PRS1 failed on $($fullCandidateErrors.Count) valid full ArchiveMax group(s)."
}
if ($raceRows.Count -eq 0) {
    throw 'No full ArchiveMax representation_race rows from winning archive candidates captured.'
}
if ($summaryRows.Count -eq 0) {
    throw 'No full ArchiveMax prs1_summary rows from winning archive candidates captured.'
}
if ($raceRows.Count -ne $summaryRows.Count) {
    throw "Full race/summary cardinality mismatch: races=$($raceRows.Count) summaries=$($summaryRows.Count)"
}
if ($winningGroupChoiceRows.Count -ne $raceRows.Count) {
    throw "Full group/race cardinality mismatch: groups=$($winningGroupChoiceRows.Count) races=$($raceRows.Count)"
}

$expectedPlaneRows = $summaryRows.Count * 8
if ($planeRows.Count -ne $expectedPlaneRows) {
    throw "Physical PRS1 candidate plane evidence incomplete: actual=$($planeRows.Count) expected=$expectedPlaneRows"
}

$raceByKey = @{}
foreach ($row in $raceRows) {
    $key = Trace-Key $row
    if ($raceByKey.ContainsKey($key)) { throw "Duplicate full representation race for $key" }
    $raceByKey[$key] = $row
}
$summaryByKey = @{}
foreach ($row in $summaryRows) {
    $key = Trace-Key $row
    if ($summaryByKey.ContainsKey($key)) { throw "Duplicate full PRS1 summary for $key" }
    $summaryByKey[$key] = $row
}

$finalGroups = New-Object System.Collections.Generic.List[object]
$finalPrs1Keys = @{}
foreach ($choice in $winningGroupChoiceRows) {
    $key = Trace-Key $choice
    if (-not $raceByKey.ContainsKey($key)) {
        throw "Missing v18 full race for final group $key"
    }
    $race = $raceByKey[$key]
    $chainId = [int]$choice.chain_id
    $codecId = [int]$choice.codec_id
    $finalFamily = switch ($chainId) {
        1 { 'store' }
        2 { 'zstd' }
        3 { 'brotli' }
        4 { 'lzma2' }
        5 { $race.winner }
        default { throw "Unknown final group chain_id=$chainId for $key" }
    }
    if ($chainId -ne 5 -and $codecId -ne ($chainId - 1)) {
        throw "Standard chain/codec mismatch for $key: chain=$chainId codec=$codecId"
    }
    if ($chainId -eq 5 -and $codecId -ne 4) {
        throw "Native chain/codec mismatch for $key: chain=$chainId codec=$codecId"
    }
    if ($finalFamily -eq 'prs1') {
        $finalPrs1Keys[$key] = $true
    }
    $finalGroups.Add([pscustomobject]@{
        case = $choice.case
        archive_winner = $choice.planner_candidate
        group = [int]$choice.group
        group_parallel = $choice.group_parallel
        input_bytes = [int64]$choice.input_bytes
        members = [int]$choice.members
        payload_bytes = [int64]$choice.payload_bytes
        chain_id = $chainId
        codec_id = $codecId
        codec_version = [int]$choice.codec_version
        level = [int]$choice.level
        native_internal_winner = if ($chainId -eq 5) { $race.winner } else { '' }
        final_family = $finalFamily
    })
}
$finalGroups | Sort-Object case,group | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-final-groups.csv') -NoTypeInformation -Encoding UTF8

$finalPrs1SummaryRows = @($summaryRows | Where-Object { $finalPrs1Keys.ContainsKey((Trace-Key $_)) })
$finalPrs1PlaneRows = @($planeRows | Where-Object { $finalPrs1Keys.ContainsKey((Trace-Key $_)) })
if ($finalPrs1PlaneRows.Count -ne ($finalPrs1SummaryRows.Count * 8)) {
    throw "Final PRS1 plane cardinality mismatch: planes=$($finalPrs1PlaneRows.Count) summaries=$($finalPrs1SummaryRows.Count)"
}

$raceRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-native-races.csv') -NoTypeInformation -Encoding UTF8
$summaryRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-candidate-summaries.csv') -NoTypeInformation -Encoding UTF8
$planeRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-candidate-plane-trace.csv') -NoTypeInformation -Encoding UTF8
$finalPrs1SummaryRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-final-summaries.csv') -NoTypeInformation -Encoding UTF8
$finalPrs1PlaneRows | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-final-plane-trace.csv') -NoTypeInformation -Encoding UTF8

function Build-PlaneSummary([object[]]$Rows) {
    return @(
        $Rows |
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
}

$candidatePlaneSummary = Build-PlaneSummary $planeRows
$finalPlaneSummary = Build-PlaneSummary $finalPrs1PlaneRows
$candidatePlaneSummary | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-candidate-plane-summary.csv') -NoTypeInformation -Encoding UTF8
$finalPlaneSummary | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-final-plane-summary.csv') -NoTypeInformation -Encoding UTF8

$finalGroupsByCase = @(
    $finalGroups |
        Group-Object case |
        Sort-Object Name |
        ForEach-Object {
            $group = @($_.Group)
            [pscustomobject]@{
                case = $_.Name
                archive_winner = $winnerByCase[$_.Name]
                groups = $group.Count
                prs1 = @($group | Where-Object { $_.final_family -eq 'prs1' }).Count
                v17 = @($group | Where-Object { $_.final_family -eq 'v17' }).Count
                v12 = @($group | Where-Object { $_.final_family -eq 'v12' }).Count
                zstd = @($group | Where-Object { $_.final_family -eq 'zstd' }).Count
                brotli = @($group | Where-Object { $_.final_family -eq 'brotli' }).Count
                lzma2 = @($group | Where-Object { $_.final_family -eq 'lzma2' }).Count
                store = @($group | Where-Object { $_.final_family -eq 'store' }).Count
            }
        }
)
$finalGroupsByCase | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-final-groups-by-case.csv') -NoTypeInformation -Encoding UTF8

function Sum-Property([object[]]$Rows, [string]$Property) {
    return [int64](($Rows | ForEach-Object { if ($_.$Property) { [int64]$_.$Property } else { 0 } } | Measure-Object -Sum).Sum)
}

$familyByCase = @(
    $finalPrs1SummaryRows |
        Group-Object case |
        Sort-Object Name |
        ForEach-Object {
            $group = @($_.Group)
            [pscustomobject]@{
                case = $_.Name
                archive_winner = $winnerByCase[$_.Name]
                prs1_groups = $group.Count
                raw = Sum-Property $group 'raw'
                exact_ref = Sum-Property $group 'exact_ref'
                overlay = Sum-Property $group 'overlay'
                overlay_xor = Sum-Property $group 'overlay_xor'
                mixture = Sum-Property $group 'mixture'
                mixture_combinadic = Sum-Property $group 'mixture_combinadic'
                axial = Sum-Property $group 'axial'
                axial_xor = Sum-Property $group 'axial_xor'
                axial_even_odd = Sum-Property $group 'axial_even_odd'
                defect = Sum-Property $group 'defect'
                periodic_defect = Sum-Property $group 'periodic_defect'
                transition = Sum-Property $group 'transition'
                delta_transition = Sum-Property $group 'delta_transition'
            }
        }
)
$familyByCase | Export-Csv -LiteralPath (Join-Path $EvidencePath 'prs1-final-families-by-case.csv') -NoTypeInformation -Encoding UTF8

$prs1InternalWins = @($raceRows | Where-Object { $_.winner -eq 'prs1' }).Count
$v12InternalWins = @($raceRows | Where-Object { $_.winner -eq 'v12' }).Count
$v17InternalWins = @($raceRows | Where-Object { $_.winner -eq 'v17' }).Count
$finalPrs1Groups = @($finalGroups | Where-Object { $_.final_family -eq 'prs1' }).Count
$finalV12Groups = @($finalGroups | Where-Object { $_.final_family -eq 'v12' }).Count
$finalV17Groups = @($finalGroups | Where-Object { $_.final_family -eq 'v17' }).Count
$finalZstdGroups = @($finalGroups | Where-Object { $_.final_family -eq 'zstd' }).Count
$finalBrotliGroups = @($finalGroups | Where-Object { $_.final_family -eq 'brotli' }).Count
$finalLzma2Groups = @($finalGroups | Where-Object { $_.final_family -eq 'lzma2' }).Count
$finalStoreGroups = @($finalGroups | Where-Object { $_.final_family -eq 'store' }).Count

$candidatePlaneRaw = Sum-Property $planeRows 'raw_bytes'
$candidatePlaneEncoded = Sum-Property $planeRows 'encoded_bytes'
$finalPlaneRaw = Sum-Property $finalPrs1PlaneRows 'raw_bytes'
$finalPlaneEncoded = Sum-Property $finalPrs1PlaneRows 'encoded_bytes'

$summaryPath = Join-Path $EvidencePath 'PRS1_R5_SUMMARY.txt'
@(
    "timestamp_utc=$((Get-Date).ToUniversalTime().ToString('o'))",
    "branch=$Branch",
    "source_commit=$SourceCommit",
    'native_process_failure_policy=EXIT_CODE_ONLY',
    'benchmark_profiles=archive-max-only',
    'evidence_scope=final_physical_groups_from_winning_archive_candidates',
    "cases_with_archive_winner=$($winnerByCase.Count)",
    "final_groups=$($finalGroups.Count)",
    "final_prs1_groups=$finalPrs1Groups",
    "final_v17_groups=$finalV17Groups",
    "final_v12_groups=$finalV12Groups",
    "final_zstd_groups=$finalZstdGroups",
    "final_brotli_groups=$finalBrotliGroups",
    "final_lzma2_groups=$finalLzma2Groups",
    "final_store_groups=$finalStoreGroups",
    "native_full_races=$($raceRows.Count)",
    "prs1_internal_wins=$prs1InternalWins",
    "v12_internal_wins=$v12InternalWins",
    "v17_internal_wins=$v17InternalWins",
    "prs1_candidate_attempts=$($summaryRows.Count)",
    "prs1_final_summaries=$($finalPrs1SummaryRows.Count)",
    "candidate_plane_records=$($planeRows.Count)",
    "candidate_plane_raw_bytes=$candidatePlaneRaw",
    "candidate_plane_encoded_bytes=$candidatePlaneEncoded",
    "final_prs1_plane_records=$($finalPrs1PlaneRows.Count)",
    "final_prs1_plane_raw_bytes=$finalPlaneRaw",
    "final_prs1_plane_encoded_bytes=$finalPlaneEncoded",
    "final_prs1_plane_store_records=$(@($finalPrs1PlaneRows | Where-Object { $_.codec_id -eq '0' }).Count)",
    "final_prs1_plane_zstd_records=$(@($finalPrs1PlaneRows | Where-Object { $_.codec_id -eq '1' }).Count)",
    "final_prs1_plane_lzma2_records=$(@($finalPrs1PlaneRows | Where-Object { $_.codec_id -eq '3' }).Count)",
    "probe_representation_races=$($probeRaceRows.Count)",
    "probe_candidate_summaries=$($probeSummaryRows.Count)",
    "probe_plane_records=$($probePlaneRows.Count)",
    "final_raw_cells=$(Sum-Property $finalPrs1SummaryRows 'raw')",
    "final_exact_ref_cells=$(Sum-Property $finalPrs1SummaryRows 'exact_ref')",
    "final_overlay_cells=$(Sum-Property $finalPrs1SummaryRows 'overlay')",
    "final_overlay_xor_cells=$(Sum-Property $finalPrs1SummaryRows 'overlay_xor')",
    "final_mixture_cells=$(Sum-Property $finalPrs1SummaryRows 'mixture')",
    "final_mixture_combinadic_cells=$(Sum-Property $finalPrs1SummaryRows 'mixture_combinadic')",
    "final_axial_cells=$(Sum-Property $finalPrs1SummaryRows 'axial')",
    "final_axial_xor_cells=$(Sum-Property $finalPrs1SummaryRows 'axial_xor')",
    "final_axial_even_odd_cells=$(Sum-Property $finalPrs1SummaryRows 'axial_even_odd')",
    "final_defect_cells=$(Sum-Property $finalPrs1SummaryRows 'defect')",
    "final_periodic_defect_cells=$(Sum-Property $finalPrs1SummaryRows 'periodic_defect')",
    "final_transition_cells=$(Sum-Property $finalPrs1SummaryRows 'transition')",
    "final_delta_transition_cells=$(Sum-Property $finalPrs1SummaryRows 'delta_transition')",
    '7zip_executed=False',
    'winrar_executed=False',
    'winzip_executed=False'
) | Set-Content -LiteralPath $summaryPath -Encoding UTF8

Write-Host 'PRS1 R5 trace analysis PASS' -ForegroundColor Green
Write-Host "Summary: $summaryPath"
Write-Host "Final groups: $(Join-Path $EvidencePath 'prs1-final-groups.csv')"
Write-Host "Final groups by case: $(Join-Path $EvidencePath 'prs1-final-groups-by-case.csv')"
Write-Host "Final PRS1 families by case: $(Join-Path $EvidencePath 'prs1-final-families-by-case.csv')"
Write-Host "Final PRS1 physical planes: $(Join-Path $EvidencePath 'prs1-final-plane-summary.csv')"
exit 0
