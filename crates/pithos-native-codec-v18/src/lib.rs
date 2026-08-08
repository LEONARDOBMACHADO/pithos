//! Native codec v18: historical floor selector plus PRS1 compatibility transport.
//!
//! Historical native encoding remains unchanged: v17 and the R2 v12 floor are
//! independent candidates. PRS1 is a third, representation-first candidate.
//! v18 acts only as the experimental compatibility selector/transport so the
//! current PAF registry can carry whichever complete payload is smallest.

use pithos_core::{PithosError, Result};
pub use pithos_representation_substrate::SubstrateStats;
use std::time::Instant;

pub const NATIVE_CODEC_ID: u16 = 4;
pub const NATIVE_CODEC_VERSION: u16 = 18;

// Three full representation candidates can each hold buffers comparable to the
// input. Keep the three-way race for moderate groups when the machine can
// actually execute three workers. Otherwise reduce the parallelism layer while
// preserving exactly the same candidate set and final size arbitration.
const PRS1_PARALLEL_MAX_INPUT_BYTES: usize = 128 * 1024 * 1024;
const PRS1_HEADER_LEN: usize = 24;
const PRS1_PLANE_RECORD_LEN: usize = 24;
const PRS1_PLANE_COUNT: usize = 8;
const PRS1_MAX_CELLS: u32 = 1_000_000;
const PRS1_DECODE_SLACK_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}

pub fn encode_exact_dedup(
    input: &[u8],
    member_lengths: &[u64],
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    let workers = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    // Preserve the historical v17/v12 level contract. PRS1 is representation-
    // first and owns its entropy leaves, so ArchiveMax (native level 15) uses
    // the strongest deterministic Zstd leaf level instead of inheriting 15.
    let substrate_entropy_level = if level >= 15 { 19 } else { level };

    let (current, current_ms, floor, floor_ms, substrate, substrate_ms, race_mode) =
        if workers >= 3 && input.len() <= PRS1_PARALLEL_MAX_INPUT_BYTES {
            let (current_result, floor_result, substrate_result) = std::thread::scope(|scope| {
                let current = scope.spawn(|| {
                    timed(|| pithos_native_current::encode_exact_dedup(input, member_lengths, level))
                });
                let floor = scope.spawn(|| {
                    timed(|| pithos_native_floor::encode_exact_dedup(input, member_lengths, level))
                });
                let substrate = scope.spawn(|| {
                    timed(|| {
                        pithos_representation_substrate::encode(
                            input,
                            member_lengths,
                            substrate_entropy_level,
                        )
                    })
                });
                (current.join(), floor.join(), substrate.join())
            });
            let (current, current_ms) = current_result
                .map_err(|_| PithosError::InvalidMetadata("native current worker panic"))?;
            let (floor, floor_ms) = floor_result
                .map_err(|_| PithosError::InvalidMetadata("native floor worker panic"))?;
            let (substrate, substrate_ms) = substrate_result
                .map_err(|_| PithosError::InvalidMetadata("PRS1 worker panic"))?;
            (
                current?,
                current_ms,
                floor?,
                floor_ms,
                substrate,
                substrate_ms,
                "three-way-parallel",
            )
        } else if workers >= 2 {
            let (current_result, floor_result) = std::thread::scope(|scope| {
                let current = scope.spawn(|| {
                    timed(|| pithos_native_current::encode_exact_dedup(input, member_lengths, level))
                });
                let floor = scope.spawn(|| {
                    timed(|| pithos_native_floor::encode_exact_dedup(input, member_lengths, level))
                });
                (current.join(), floor.join())
            });
            let (current, current_ms) = current_result
                .map_err(|_| PithosError::InvalidMetadata("native current worker panic"))?;
            let (floor, floor_ms) = floor_result
                .map_err(|_| PithosError::InvalidMetadata("native floor worker panic"))?;
            let (substrate, substrate_ms) = timed(|| {
                pithos_representation_substrate::encode(
                    input,
                    member_lengths,
                    substrate_entropy_level,
                )
            });
            (
                current?,
                current_ms,
                floor?,
                floor_ms,
                substrate,
                substrate_ms,
                if input.len() <= PRS1_PARALLEL_MAX_INPUT_BYTES {
                    "cpu-bounded-pair"
                } else {
                    "memory-bounded-pair"
                },
            )
        } else {
            let (current, current_ms) =
                timed(|| pithos_native_current::encode_exact_dedup(input, member_lengths, level));
            let (floor, floor_ms) =
                timed(|| pithos_native_floor::encode_exact_dedup(input, member_lengths, level));
            let (substrate, substrate_ms) = timed(|| {
                pithos_representation_substrate::encode(
                    input,
                    member_lengths,
                    substrate_entropy_level,
                )
            });
            (
                current?,
                current_ms,
                floor?,
                floor_ms,
                substrate,
                substrate_ms,
                "sequential",
            )
        };

    let current_len = current.0.len() as u64;
    let floor_len = floor.0.len() as u64;
    let substrate_len = substrate
        .as_ref()
        .map(|candidate| candidate.0.len() as u64)
        .unwrap_or(u64::MAX);

    let winner = if substrate_len < current_len.min(floor_len) {
        "prs1"
    } else if floor_len < current_len {
        "v12"
    } else {
        "v17"
    };

    if representation_trace_enabled() {
        eprintln!(
            "PITHOS_REP_TRACE\tstage=representation_race\tlevel={level}\tprs1_entropy_level={substrate_entropy_level}\tinput_bytes={}\tmembers={}\tracing_mode={}\tworker_budget={}\tv17_bytes={}\tv17_ms={:.3}\tv12_bytes={}\tv12_ms={:.3}\tprs1_bytes={}\tprs1_ms={:.3}\twinner={}",
            input.len(),
            member_lengths.len(),
            race_mode,
            workers,
            current_len,
            current_ms,
            floor_len,
            floor_ms,
            if substrate_len == u64::MAX { 0 } else { substrate_len },
            substrate_ms,
            winner
        );
        match &substrate {
            Ok((payload, stats)) => {
                trace_substrate_stats(input.len(), level, substrate_entropy_level, stats);
                trace_substrate_planes(input.len(), level, substrate_entropy_level, payload);
            }
            Err(error) => eprintln!(
                "PITHOS_REP_TRACE\tstage=prs1_candidate_error\tlevel={level}\tprs1_entropy_level={substrate_entropy_level}\tinput_bytes={}\terror={}",
                input.len(),
                error
            ),
        }
    }

    if winner == "prs1" {
        if let Ok((payload, stats)) = substrate {
            return Ok((payload, substrate_native_stats(stats)));
        }
        return Err(PithosError::InvalidMetadata("PRS1 winner unavailable"));
    }
    if winner == "v12" {
        return Ok((
            floor.0,
            NativeStats {
                chunk_count: floor.1.chunk_count,
                canonical_chunks: floor.1.canonical_chunks,
                gross_duplicate_bytes: floor.1.gross_duplicate_bytes,
                representation_bytes: floor.1.representation_bytes,
                encoded_bytes: floor_len,
            },
        ));
    }
    Ok((
        current.0,
        NativeStats {
            chunk_count: current.1.chunk_count,
            canonical_chunks: current.1.canonical_chunks,
            gross_duplicate_bytes: current.1.gross_duplicate_bytes,
            representation_bytes: current.1.representation_bytes,
            encoded_bytes: current_len,
        },
    ))
}

pub fn encode_substrate(
    input: &[u8],
    member_lengths: &[u64],
    level: i32,
) -> Result<(Vec<u8>, SubstrateStats)> {
    pithos_representation_substrate::encode(input, member_lengths, level)
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    if payload.starts_with(b"PRS1") {
        validate_prs1_bounds(payload, expected_len)?;
        pithos_representation_substrate::decode(payload, expected_len)
    } else {
        pithos_native_current::decode_exact_dedup(payload, expected_len)
    }
}

fn validate_prs1_bounds(payload: &[u8], expected_len: u64) -> Result<()> {
    let table_bytes = PRS1_PLANE_COUNT
        .checked_mul(PRS1_PLANE_RECORD_LEN)
        .ok_or(PithosError::IntegerOverflow)?;
    let data_start = PRS1_HEADER_LEN
        .checked_add(table_bytes)
        .ok_or(PithosError::IntegerOverflow)?;
    if payload.len() < data_start {
        return Err(PithosError::InvalidRange);
    }
    if read_u64(payload, 8)? != expected_len || read_u16(payload, 20)? as usize != PRS1_PLANE_COUNT {
        return Err(PithosError::InvalidMetadata("PRS1 transport header"));
    }
    let cell_count = read_u32(payload, 16)?;
    if cell_count > PRS1_MAX_CELLS {
        return Err(PithosError::ResourceLimit("PRS1 cell count"));
    }

    let per_plane_limit = expected_len
        .checked_add(PRS1_DECODE_SLACK_BYTES)
        .ok_or(PithosError::IntegerOverflow)?;
    let total_plane_limit = expected_len
        .checked_mul(2)
        .and_then(|value| value.checked_add(PRS1_DECODE_SLACK_BYTES))
        .ok_or(PithosError::IntegerOverflow)?;
    let mut total_raw = 0_u64;
    let mut total_encoded = 0_u64;
    for index in 0..PRS1_PLANE_COUNT {
        let offset = PRS1_HEADER_LEN
            .checked_add(index * PRS1_PLANE_RECORD_LEN)
            .ok_or(PithosError::IntegerOverflow)?;
        let raw_len = read_u64(payload, offset + 4)?;
        let encoded_len = read_u64(payload, offset + 12)?;
        if raw_len > per_plane_limit {
            return Err(PithosError::ResourceLimit("PRS1 plane raw bytes"));
        }
        total_raw = total_raw
            .checked_add(raw_len)
            .ok_or(PithosError::IntegerOverflow)?;
        total_encoded = total_encoded
            .checked_add(encoded_len)
            .ok_or(PithosError::IntegerOverflow)?;
    }
    if total_raw > total_plane_limit {
        return Err(PithosError::ResourceLimit("PRS1 aggregate plane bytes"));
    }
    let expected_payload_len = u64::try_from(data_start)
        .map_err(|_| PithosError::IntegerOverflow)?
        .checked_add(total_encoded)
        .ok_or(PithosError::IntegerOverflow)?;
    if expected_payload_len != payload.len() as u64 {
        return Err(PithosError::InvalidRange);
    }
    Ok(())
}

fn trace_substrate_stats(
    input_bytes: usize,
    level: i32,
    substrate_entropy_level: i32,
    stats: &SubstrateStats,
) {
    eprintln!(
        "PITHOS_REP_TRACE\tstage=prs1_summary\tlevel={level}\tprs1_entropy_level={substrate_entropy_level}\tinput_bytes={}\tencoded_bytes={}\tcells={}\traw={}\texact_ref={}\toverlay={}\toverlay_xor={}\tmixture={}\tmixture_combinadic={}\taxial={}\taxial_xor={}\taxial_even_odd={}\tdefect={}\tperiodic_defect={}\ttransition={}\tdelta_transition={}",
        input_bytes,
        stats.encoded_bytes,
        stats.cell_count,
        stats.raw_cells,
        stats.exact_ref_cells,
        stats.overlay_cells,
        stats.overlay_xor_cells,
        stats.mixture_cells,
        stats.mixture_combinadic_cells,
        stats.axial_cells,
        stats.axial_xor_cells,
        stats.axial_even_odd_cells,
        stats.defect_cells,
        stats.periodic_defect_cells,
        stats.transition_cells,
        stats.delta_transition_cells
    );
}

fn trace_substrate_planes(
    input_bytes: usize,
    level: i32,
    substrate_entropy_level: i32,
    payload: &[u8],
) {
    let table_bytes = match PRS1_PLANE_COUNT.checked_mul(PRS1_PLANE_RECORD_LEN) {
        Some(value) => value,
        None => return,
    };
    if payload.len() < PRS1_HEADER_LEN.saturating_add(table_bytes) || !payload.starts_with(b"PRS1") {
        return;
    }
    for index in 0..PRS1_PLANE_COUNT {
        let offset = PRS1_HEADER_LEN + index * PRS1_PLANE_RECORD_LEN;
        let Some(record) = payload.get(offset..offset + PRS1_PLANE_RECORD_LEN) else {
            return;
        };
        let plane_id = u16::from_le_bytes([record[0], record[1]]);
        let codec_id = u16::from_le_bytes([record[2], record[3]]);
        let raw_len = u64::from_le_bytes([
            record[4], record[5], record[6], record[7], record[8], record[9], record[10],
            record[11],
        ]);
        let encoded_len = u64::from_le_bytes([
            record[12], record[13], record[14], record[15], record[16], record[17], record[18],
            record[19],
        ]);
        eprintln!(
            "PITHOS_REP_TRACE\tstage=prs1_plane\tlevel={level}\tprs1_entropy_level={substrate_entropy_level}\tinput_bytes={input_bytes}\tplane={plane_id}\tcodec_id={codec_id}\traw_bytes={raw_len}\tencoded_bytes={encoded_len}"
        );
    }
}

fn timed<T>(operation: impl FnOnce() -> T) -> (T, f64) {
    let started = Instant::now();
    let result = operation();
    (result, started.elapsed().as_secs_f64() * 1000.0)
}

fn substrate_native_stats(stats: SubstrateStats) -> NativeStats {
    NativeStats {
        chunk_count: stats.cell_count,
        canonical_chunks: stats.cell_count.saturating_sub(stats.exact_ref_cells),
        gross_duplicate_bytes: 0,
        representation_bytes: stats.encoded_bytes,
        encoded_bytes: stats.encoded_bytes,
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let data = bytes
        .get(offset..offset + 2)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let data = bytes
        .get(offset..offset + 4)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let data = bytes
        .get(offset..offset + 8)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]))
}

fn representation_trace_enabled() -> bool {
    std::env::var("PITHOS_REP_TRACE").ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_roundtrips() {
        let member = b"floor-selector-payload".repeat(64 * 1024);
        let mut input = member.clone();
        input.extend_from_slice(&member);
        let lengths = [member.len() as u64, member.len() as u64];
        let (payload, _) = encode_exact_dedup(&input, &lengths, 15).unwrap();
        assert_eq!(
            decode_exact_dedup(&payload, input.len() as u64).unwrap(),
            input
        );
    }

    #[test]
    fn substrate_transport_roundtrips() {
        let member = b"prs1-transport-template".repeat(32 * 1024);
        let mut input = member.clone();
        input.extend_from_slice(&member);
        let lengths = [member.len() as u64, member.len() as u64];
        let (payload, stats) = encode_substrate(&input, &lengths, 9).unwrap();
        assert!(payload.starts_with(b"PRS1"));
        assert!(stats.cell_count > 0);
        assert_eq!(
            decode_exact_dedup(&payload, input.len() as u64).unwrap(),
            input
        );
    }

    #[test]
    fn prs1_transport_rejects_impossible_plane_allocation() {
        let member = b"bounded-prs1".repeat(8192);
        let lengths = [member.len() as u64];
        let (mut payload, _) = encode_substrate(&member, &lengths, 9).unwrap();
        let malicious = (member.len() as u64)
            .saturating_add(PRS1_DECODE_SLACK_BYTES)
            .saturating_add(1);
        payload[PRS1_HEADER_LEN + 4..PRS1_HEADER_LEN + 12]
            .copy_from_slice(&malicious.to_le_bytes());
        assert!(decode_exact_dedup(&payload, member.len() as u64).is_err());
    }
}
