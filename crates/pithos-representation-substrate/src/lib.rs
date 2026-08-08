//! PRS1: representation-first substrate.
//!
//! PRS1 is deliberately independent from the historical native codec chain.
//! It partitions member-bounded input into deterministic multi-granular cells,
//! chooses one reversible representation per cell, multiplexes like data into
//! orthogonal planes, and only then selects an entropy backend per plane.

use pithos_codecs::{Codec, CodecConfig, CodecId, Lzma2Codec, ZstdCodec};
use pithos_core::{PithosError, Result};
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;

pub const SUBSTRATE_CODEC_ID: u16 = 5;
pub const SUBSTRATE_CODEC_VERSION: u16 = 1;

const MAGIC: &[u8; 4] = b"PRS1";
const FORMAT_VERSION: u16 = 1;
const HEADER_LEN: usize = 24;
const PLANE_RECORD_LEN: usize = 24;
const PLANE_COUNT: u16 = 8;
const MIN_CELL_BYTES: usize = 4 * 1024;
const MAX_CELL_BYTES: usize = 1024 * 1024;
const MAX_RECURSION_DEPTH: usize = 8;
const MAX_CELL_COUNT: usize = 1_000_000;
const TEMPLATE_WINDOW: usize = 16;
const LZMA_MIN_PLANE_BYTES: usize = 64 * 1024;
const LZMA_MAX_PLANE_BYTES: usize = 64 * 1024 * 1024;
const DECODE_SLACK_BYTES: u64 = 16 * 1024 * 1024;

const PLANE_DESCRIPTOR: u16 = 0;
const PLANE_RAW: u16 = 1;
const PLANE_OVERLAY: u16 = 2;
const PLANE_MIXTURE: u16 = 3;
const PLANE_AXIS_HI: u16 = 4;
const PLANE_AXIS_LO: u16 = 5;
const PLANE_DEFECT: u16 = 6;
const PLANE_TRANSITION: u16 = 7;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubstrateStats {
    pub cell_count: u32,
    pub raw_cells: u32,
    pub exact_ref_cells: u32,
    pub overlay_cells: u32,
    pub mixture_cells: u32,
    pub axial_cells: u32,
    pub defect_cells: u32,
    pub transition_cells: u32,
    pub encoded_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct CellRange {
    start: usize,
    len: usize,
}

#[derive(Debug)]
enum Candidate {
    Raw,
    ExactRef {
        base: usize,
    },
    Overlay {
        base: usize,
        changes: usize,
        payload: Vec<u8>,
    },
    Mixture {
        alphabet: Vec<u8>,
        bits: u8,
        payload: Vec<u8>,
    },
    Axial {
        hi: Vec<u8>,
        lo: Vec<u8>,
    },
    Defect {
        default: u8,
        defects: usize,
        payload: Vec<u8>,
    },
    Transition {
        runs: usize,
        payload: Vec<u8>,
    },
}

struct ScoredCandidate {
    score: usize,
    candidate: Candidate,
}

#[derive(Default)]
struct RawPlanes {
    descriptor: Vec<u8>,
    raw: Vec<u8>,
    overlay: Vec<u8>,
    mixture: Vec<u8>,
    axis_hi: Vec<u8>,
    axis_lo: Vec<u8>,
    defect: Vec<u8>,
    transition: Vec<u8>,
}

struct EncodedPlane {
    plane_id: u16,
    codec_id: u16,
    raw_len: u64,
    payload: Vec<u8>,
    crc32c: u32,
}

#[derive(Debug, Clone, Copy)]
struct PlaneRecord {
    plane_id: u16,
    codec_id: u16,
    raw_len: u64,
    encoded_len: u64,
    crc32c: u32,
}

#[derive(Debug, Clone, Copy)]
struct FeatureVector {
    unique: u16,
    dominant_pct: u8,
    transition_pct: u8,
    zero_pct: u8,
}

pub fn encode(
    input: &[u8],
    member_lengths: &[u64],
    entropy_level: i32,
) -> Result<(Vec<u8>, SubstrateStats)> {
    validate_member_lengths(input, member_lengths)?;
    let cells = partition_members(input, member_lengths)?;
    if cells.len() > MAX_CELL_COUNT {
        return Err(PithosError::ResourceLimit("PRS1 cell count"));
    }
    let mut planes = RawPlanes::default();
    let mut stats = SubstrateStats {
        cell_count: u32::try_from(cells.len())
            .map_err(|_| PithosError::ResourceLimit("PRS1 cell count"))?,
        ..SubstrateStats::default()
    };

    let mut exact = HashMap::<[u8; 32], usize>::new();
    let mut coarse = HashMap::<u64, VecDeque<usize>>::new();

    for (index, range) in cells.iter().copied().enumerate() {
        let end = range
            .start
            .checked_add(range.len)
            .ok_or(PithosError::IntegerOverflow)?;
        let bytes = input
            .get(range.start..end)
            .ok_or(PithosError::InvalidRange)?;
        let candidate = choose_candidate(bytes, input, &cells, index, &exact, &coarse)?;
        write_varint(range.len as u64, &mut planes.descriptor);
        apply_candidate(candidate, bytes, &mut planes, &mut stats);

        let hash = *blake3::hash(bytes).as_bytes();
        exact.entry(hash).or_insert(index);
        let key = coarse_fingerprint(bytes);
        let queue = coarse.entry(key).or_default();
        queue.push_back(index);
        while queue.len() > TEMPLATE_WINDOW {
            queue.pop_front();
        }
    }

    let raw_planes = [
        (PLANE_DESCRIPTOR, planes.descriptor),
        (PLANE_RAW, planes.raw),
        (PLANE_OVERLAY, planes.overlay),
        (PLANE_MIXTURE, planes.mixture),
        (PLANE_AXIS_HI, planes.axis_hi),
        (PLANE_AXIS_LO, planes.axis_lo),
        (PLANE_DEFECT, planes.defect),
        (PLANE_TRANSITION, planes.transition),
    ];
    let mut encoded_planes = Vec::with_capacity(raw_planes.len());
    for (plane_id, bytes) in raw_planes {
        encoded_planes.push(encode_plane(plane_id, &bytes, entropy_level)?);
    }

    let records_len = usize::from(PLANE_COUNT)
        .checked_mul(PLANE_RECORD_LEN)
        .ok_or(PithosError::IntegerOverflow)?;
    let payload_capacity = encoded_planes.iter().try_fold(
        HEADER_LEN
            .checked_add(records_len)
            .ok_or(PithosError::IntegerOverflow)?,
        |total, plane| {
            total
                .checked_add(plane.payload.len())
                .ok_or(PithosError::IntegerOverflow)
        },
    )?;
    let mut output = Vec::with_capacity(payload_capacity);
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&(input.len() as u64).to_le_bytes());
    output.extend_from_slice(&stats.cell_count.to_le_bytes());
    output.extend_from_slice(&PLANE_COUNT.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    for plane in &encoded_planes {
        output.extend_from_slice(&plane.plane_id.to_le_bytes());
        output.extend_from_slice(&plane.codec_id.to_le_bytes());
        output.extend_from_slice(&plane.raw_len.to_le_bytes());
        output.extend_from_slice(&(plane.payload.len() as u64).to_le_bytes());
        output.extend_from_slice(&plane.crc32c.to_le_bytes());
    }
    for plane in encoded_planes {
        output.extend_from_slice(&plane.payload);
    }
    stats.encoded_bytes = output.len() as u64;
    Ok((output, stats))
}

pub fn decode(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    if payload.len() < HEADER_LEN || &payload[..4] != MAGIC {
        return Err(PithosError::InvalidMetadata("PRS1 magic"));
    }
    if read_u16(payload, 4)? != FORMAT_VERSION
        || read_u16(payload, 6)? != 0
        || read_u64(payload, 8)? != expected_len
        || read_u16(payload, 20)? != PLANE_COUNT
        || read_u16(payload, 22)? != 0
    {
        return Err(PithosError::InvalidMetadata("PRS1 header"));
    }
    let cell_count = read_u32(payload, 16)? as usize;
    if cell_count > MAX_CELL_COUNT {
        return Err(PithosError::ResourceLimit("PRS1 cell count"));
    }
    let table_len = usize::from(PLANE_COUNT)
        .checked_mul(PLANE_RECORD_LEN)
        .ok_or(PithosError::IntegerOverflow)?;
    let data_start = HEADER_LEN
        .checked_add(table_len)
        .ok_or(PithosError::IntegerOverflow)?;
    if data_start > payload.len() {
        return Err(PithosError::InvalidRange);
    }

    let mut records = Vec::with_capacity(usize::from(PLANE_COUNT));
    for index in 0..usize::from(PLANE_COUNT) {
        let offset = HEADER_LEN
            .checked_add(index * PLANE_RECORD_LEN)
            .ok_or(PithosError::IntegerOverflow)?;
        records.push(PlaneRecord {
            plane_id: read_u16(payload, offset)?,
            codec_id: read_u16(payload, offset + 2)?,
            raw_len: read_u64(payload, offset + 4)?,
            encoded_len: read_u64(payload, offset + 12)?,
            crc32c: read_u32(payload, offset + 20)?,
        });
    }
    validate_plane_records(&records)?;
    validate_plane_bounds(&records, expected_len, payload.len(), data_start)?;

    let mut cursor = data_start;
    let mut decoded = HashMap::<u16, Vec<u8>>::new();
    for record in records {
        let encoded_len = usize::try_from(record.encoded_len)
            .map_err(|_| PithosError::MemoryLimit)?;
        let end = cursor
            .checked_add(encoded_len)
            .ok_or(PithosError::IntegerOverflow)?;
        let encoded = payload.get(cursor..end).ok_or(PithosError::InvalidRange)?;
        if crc32c::crc32c(encoded) != record.crc32c {
            return Err(PithosError::ChecksumMismatch);
        }
        let plane = decode_plane(record.codec_id, encoded, record.raw_len)?;
        if decoded.insert(record.plane_id, plane).is_some() {
            return Err(PithosError::DuplicateSection);
        }
        cursor = end;
    }
    if cursor != payload.len() {
        return Err(PithosError::InvalidMetadata("PRS1 trailing bytes"));
    }

    let descriptor = decoded
        .remove(&PLANE_DESCRIPTOR)
        .ok_or(PithosError::MissingSection("PRS1 descriptor"))?;
    let mut raw = PlaneCursor::new(
        decoded
            .remove(&PLANE_RAW)
            .ok_or(PithosError::MissingSection("PRS1 raw"))?,
    );
    let mut overlay = PlaneCursor::new(
        decoded
            .remove(&PLANE_OVERLAY)
            .ok_or(PithosError::MissingSection("PRS1 overlay"))?,
    );
    let mut mixture = PlaneCursor::new(
        decoded
            .remove(&PLANE_MIXTURE)
            .ok_or(PithosError::MissingSection("PRS1 mixture"))?,
    );
    let mut axis_hi = PlaneCursor::new(
        decoded
            .remove(&PLANE_AXIS_HI)
            .ok_or(PithosError::MissingSection("PRS1 axis-hi"))?,
    );
    let mut axis_lo = PlaneCursor::new(
        decoded
            .remove(&PLANE_AXIS_LO)
            .ok_or(PithosError::MissingSection("PRS1 axis-lo"))?,
    );
    let mut defect = PlaneCursor::new(
        decoded
            .remove(&PLANE_DEFECT)
            .ok_or(PithosError::MissingSection("PRS1 defect"))?,
    );
    let mut transition = PlaneCursor::new(
        decoded
            .remove(&PLANE_TRANSITION)
            .ok_or(PithosError::MissingSection("PRS1 transition"))?,
    );

    let capacity = usize::try_from(expected_len).map_err(|_| PithosError::MemoryLimit)?;
    let mut output = Vec::with_capacity(capacity);
    let mut ranges = Vec::<CellRange>::with_capacity(cell_count);
    let mut descriptor_pos = 0usize;

    for _ in 0..cell_count {
        let len = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
            .map_err(|_| PithosError::MemoryLimit)?;
        let kind = *descriptor
            .get(descriptor_pos)
            .ok_or(PithosError::InvalidRange)?;
        descriptor_pos += 1;
        let start = output.len();
        match kind {
            0 => {
                let payload_len = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::MemoryLimit)?;
                if payload_len != len {
                    return Err(PithosError::InvalidMetadata("PRS1 raw length"));
                }
                output.extend_from_slice(raw.take(payload_len)?);
            }
            1 => {
                let base = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::IntegerOverflow)?;
                append_reference(&mut output, &ranges, base, len)?;
            }
            2 => {
                let base = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::IntegerOverflow)?;
                let changes = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::IntegerOverflow)?;
                let payload_len = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::MemoryLimit)?;
                let bytes = decode_overlay(
                    &output,
                    &ranges,
                    base,
                    len,
                    changes,
                    overlay.take(payload_len)?,
                )?;
                output.extend_from_slice(&bytes);
            }
            3 => {
                let alphabet_len = *descriptor
                    .get(descriptor_pos)
                    .ok_or(PithosError::InvalidRange)? as usize;
                descriptor_pos += 1;
                let bits = *descriptor
                    .get(descriptor_pos)
                    .ok_or(PithosError::InvalidRange)?;
                descriptor_pos += 1;
                if !(2..=16).contains(&alphabet_len) || !(1..=4).contains(&bits) {
                    return Err(PithosError::InvalidMetadata("PRS1 mixture metadata"));
                }
                let alphabet_end = descriptor_pos
                    .checked_add(alphabet_len)
                    .ok_or(PithosError::IntegerOverflow)?;
                let alphabet = descriptor
                    .get(descriptor_pos..alphabet_end)
                    .ok_or(PithosError::InvalidRange)?;
                descriptor_pos = alphabet_end;
                let payload_len = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::MemoryLimit)?;
                let bytes =
                    unpack_symbol_indexes(mixture.take(payload_len)?, alphabet, bits, len)?;
                output.extend_from_slice(&bytes);
            }
            4 => {
                let hi_len = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::MemoryLimit)?;
                let lo_len = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::MemoryLimit)?;
                let bytes = decode_axes(axis_hi.take(hi_len)?, axis_lo.take(lo_len)?, len)?;
                output.extend_from_slice(&bytes);
            }
            5 => {
                let default = *descriptor
                    .get(descriptor_pos)
                    .ok_or(PithosError::InvalidRange)?;
                descriptor_pos += 1;
                let defects = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::IntegerOverflow)?;
                let payload_len = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::MemoryLimit)?;
                let bytes = decode_defects(default, len, defects, defect.take(payload_len)?)?;
                output.extend_from_slice(&bytes);
            }
            6 => {
                let runs = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::IntegerOverflow)?;
                let payload_len = usize::try_from(read_varint(&descriptor, &mut descriptor_pos)?)
                    .map_err(|_| PithosError::MemoryLimit)?;
                let bytes = decode_transitions(len, runs, transition.take(payload_len)?)?;
                output.extend_from_slice(&bytes);
            }
            _ => return Err(PithosError::InvalidMetadata("PRS1 cell kind")),
        }
        if output.len().checked_sub(start) != Some(len) {
            return Err(PithosError::InvalidMetadata("PRS1 cell output length"));
        }
        ranges.push(CellRange { start, len });
    }

    if descriptor_pos != descriptor.len()
        || !raw.finished()
        || !overlay.finished()
        || !mixture.finished()
        || !axis_hi.finished()
        || !axis_lo.finished()
        || !defect.finished()
        || !transition.finished()
        || output.len() as u64 != expected_len
    {
        return Err(PithosError::InvalidMetadata("PRS1 plane consumption"));
    }
    Ok(output)
}

fn validate_member_lengths(input: &[u8], member_lengths: &[u64]) -> Result<()> {
    let total = member_lengths.iter().try_fold(0_u64, |total, length| {
        total
            .checked_add(*length)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    if total != input.len() as u64 {
        return Err(PithosError::InvalidMetadata("PRS1 member boundaries"));
    }
    Ok(())
}

fn partition_members(input: &[u8], member_lengths: &[u64]) -> Result<Vec<CellRange>> {
    let mut cells = Vec::new();
    let mut offset = 0usize;
    for length in member_lengths {
        let length = usize::try_from(*length).map_err(|_| PithosError::MemoryLimit)?;
        let end = offset
            .checked_add(length)
            .ok_or(PithosError::IntegerOverflow)?;
        input.get(offset..end).ok_or(PithosError::InvalidRange)?;
        partition_range(input, offset, length, 0, &mut cells)?;
        if cells.len() > MAX_CELL_COUNT {
            return Err(PithosError::ResourceLimit("PRS1 cell count"));
        }
        offset = end;
    }
    if offset != input.len() {
        return Err(PithosError::InvalidRange);
    }
    Ok(cells)
}

fn partition_range(
    input: &[u8],
    start: usize,
    len: usize,
    depth: usize,
    output: &mut Vec<CellRange>,
) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let end = start
        .checked_add(len)
        .ok_or(PithosError::IntegerOverflow)?;
    let bytes = input.get(start..end).ok_or(PithosError::InvalidRange)?;
    if len <= MIN_CELL_BYTES || depth >= MAX_RECURSION_DEPTH {
        output.push(CellRange { start, len });
        return Ok(());
    }
    if len > MAX_CELL_BYTES {
        let mut local = start;
        let mut remaining = len;
        while remaining > 0 {
            let chunk = remaining.min(MAX_CELL_BYTES);
            partition_range(input, local, chunk, depth + 1, output)?;
            local = local
                .checked_add(chunk)
                .ok_or(PithosError::IntegerOverflow)?;
            remaining -= chunk;
        }
        return Ok(());
    }
    if !should_split(bytes) {
        output.push(CellRange { start, len });
        return Ok(());
    }
    let mut split = ((len / 2) / MIN_CELL_BYTES) * MIN_CELL_BYTES;
    if split < MIN_CELL_BYTES || len - split < MIN_CELL_BYTES {
        split = len / 2;
    }
    if split == 0 || split >= len {
        output.push(CellRange { start, len });
        return Ok(());
    }
    partition_range(input, start, split, depth + 1, output)?;
    partition_range(input, start + split, len - split, depth + 1, output)
}

fn should_split(bytes: &[u8]) -> bool {
    if bytes.len() <= 64 * 1024 || has_strong_single_model(bytes) {
        return false;
    }
    let quarter = bytes.len() / 4;
    if quarter == 0 {
        return false;
    }
    let mut features = Vec::with_capacity(4);
    for index in 0..4 {
        let start = index * quarter;
        let end = if index == 3 {
            bytes.len()
        } else {
            (index + 1) * quarter
        };
        features.push(feature_vector(&bytes[start..end]));
    }
    let unique_min = features.iter().map(|v| v.unique).min().unwrap_or(0);
    let unique_max = features.iter().map(|v| v.unique).max().unwrap_or(0);
    let dominant_min = features.iter().map(|v| v.dominant_pct).min().unwrap_or(0);
    let dominant_max = features.iter().map(|v| v.dominant_pct).max().unwrap_or(0);
    let transition_min = features
        .iter()
        .map(|v| v.transition_pct)
        .min()
        .unwrap_or(0);
    let transition_max = features
        .iter()
        .map(|v| v.transition_pct)
        .max()
        .unwrap_or(0);
    let zero_min = features.iter().map(|v| v.zero_pct).min().unwrap_or(0);
    let zero_max = features.iter().map(|v| v.zero_pct).max().unwrap_or(0);
    unique_max.saturating_sub(unique_min) >= 32
        || dominant_max.saturating_sub(dominant_min) >= 20
        || transition_max.saturating_sub(transition_min) >= 20
        || zero_max.saturating_sub(zero_min) >= 20
}

fn has_strong_single_model(bytes: &[u8]) -> bool {
    let feature = feature_vector(bytes);
    feature.unique <= 16 || feature.dominant_pct >= 80 || feature.transition_pct <= 20
}

fn feature_vector(bytes: &[u8]) -> FeatureVector {
    if bytes.is_empty() {
        return FeatureVector {
            unique: 0,
            dominant_pct: 0,
            transition_pct: 0,
            zero_pct: 0,
        };
    }
    let mut counts = [0_u32; 256];
    let mut transitions = 0usize;
    let mut previous = bytes[0];
    for (index, byte) in bytes.iter().copied().enumerate() {
        counts[byte as usize] += 1;
        if index > 0 && byte != previous {
            transitions += 1;
        }
        previous = byte;
    }
    let unique = counts.iter().filter(|count| **count != 0).count() as u16;
    let dominant = counts.iter().copied().max().unwrap_or(0) as usize;
    let zero = counts[0] as usize;
    FeatureVector {
        unique,
        dominant_pct: percentage(dominant, bytes.len()),
        transition_pct: percentage(transitions, bytes.len().saturating_sub(1).max(1)),
        zero_pct: percentage(zero, bytes.len()),
    }
}

fn percentage(numerator: usize, denominator: usize) -> u8 {
    ((numerator.saturating_mul(100) / denominator.max(1)).min(100)) as u8
}

fn choose_candidate(
    bytes: &[u8],
    input: &[u8],
    ranges: &[CellRange],
    index: usize,
    exact: &HashMap<[u8; 32], usize>,
    coarse: &HashMap<u64, VecDeque<usize>>,
) -> Result<Candidate> {
    let hash = *blake3::hash(bytes).as_bytes();
    if let Some(base) = exact.get(&hash).copied() {
        let range = ranges
            .get(base)
            .copied()
            .ok_or(PithosError::InvalidMetadata("PRS1 exact reference"))?;
        if range.len == bytes.len() {
            let end = range
                .start
                .checked_add(range.len)
                .ok_or(PithosError::IntegerOverflow)?;
            if input.get(range.start..end) == Some(bytes) {
                return Ok(Candidate::ExactRef { base });
            }
        }
    }

    let mut best = ScoredCandidate {
        score: entropy_estimate_bytes(bytes).saturating_add(2),
        candidate: Candidate::Raw,
    };

    if let Some(queue) = coarse.get(&coarse_fingerprint(bytes)) {
        for base in queue.iter().copied().rev().take(TEMPLATE_WINDOW) {
            if base >= index {
                continue;
            }
            let range = ranges
                .get(base)
                .copied()
                .ok_or(PithosError::InvalidMetadata("PRS1 overlay reference"))?;
            if range.len != bytes.len() {
                continue;
            }
            let end = range
                .start
                .checked_add(range.len)
                .ok_or(PithosError::IntegerOverflow)?;
            let template = input
                .get(range.start..end)
                .ok_or(PithosError::InvalidRange)?;
            if let Some((changes, payload)) = encode_overlay(template, bytes) {
                let score = payload.len().saturating_add(varint_len(base as u64) + 8);
                replace_if_smaller(
                    &mut best,
                    score,
                    Candidate::Overlay {
                        base,
                        changes,
                        payload,
                    },
                );
            }
        }
    }

    if let Some((alphabet, bits, payload)) = encode_mixture(bytes) {
        let score = payload.len().saturating_add(alphabet.len() + 8);
        replace_if_smaller(
            &mut best,
            score,
            Candidate::Mixture {
                alphabet,
                bits,
                payload,
            },
        );
    }

    if let Some((default, defects, payload)) = encode_defects(bytes) {
        let score = payload.len().saturating_add(8);
        replace_if_smaller(
            &mut best,
            score,
            Candidate::Defect {
                default,
                defects,
                payload,
            },
        );
    }

    if let Some((runs, payload)) = encode_transitions(bytes) {
        let score = payload.len().saturating_add(6);
        replace_if_smaller(
            &mut best,
            score,
            Candidate::Transition { runs, payload },
        );
    }

    let (hi, lo) = encode_axes(bytes);
    let axis_score = axial_entropy_estimate_bytes(bytes).saturating_add(6);
    replace_if_smaller(&mut best, axis_score, Candidate::Axial { hi, lo });
    Ok(best.candidate)
}

fn replace_if_smaller(best: &mut ScoredCandidate, score: usize, candidate: Candidate) {
    if score < best.score {
        best.score = score;
        best.candidate = candidate;
    }
}

fn apply_candidate(
    candidate: Candidate,
    bytes: &[u8],
    planes: &mut RawPlanes,
    stats: &mut SubstrateStats,
) {
    match candidate {
        Candidate::Raw => {
            planes.descriptor.push(0);
            write_varint(bytes.len() as u64, &mut planes.descriptor);
            planes.raw.extend_from_slice(bytes);
            stats.raw_cells += 1;
        }
        Candidate::ExactRef { base } => {
            planes.descriptor.push(1);
            write_varint(base as u64, &mut planes.descriptor);
            stats.exact_ref_cells += 1;
        }
        Candidate::Overlay {
            base,
            changes,
            payload,
        } => {
            planes.descriptor.push(2);
            write_varint(base as u64, &mut planes.descriptor);
            write_varint(changes as u64, &mut planes.descriptor);
            write_varint(payload.len() as u64, &mut planes.descriptor);
            planes.overlay.extend_from_slice(&payload);
            stats.overlay_cells += 1;
        }
        Candidate::Mixture {
            alphabet,
            bits,
            payload,
        } => {
            planes.descriptor.push(3);
            planes.descriptor.push(alphabet.len() as u8);
            planes.descriptor.push(bits);
            planes.descriptor.extend_from_slice(&alphabet);
            write_varint(payload.len() as u64, &mut planes.descriptor);
            planes.mixture.extend_from_slice(&payload);
            stats.mixture_cells += 1;
        }
        Candidate::Axial { hi, lo } => {
            planes.descriptor.push(4);
            write_varint(hi.len() as u64, &mut planes.descriptor);
            write_varint(lo.len() as u64, &mut planes.descriptor);
            planes.axis_hi.extend_from_slice(&hi);
            planes.axis_lo.extend_from_slice(&lo);
            stats.axial_cells += 1;
        }
        Candidate::Defect {
            default,
            defects,
            payload,
        } => {
            planes.descriptor.push(5);
            planes.descriptor.push(default);
            write_varint(defects as u64, &mut planes.descriptor);
            write_varint(payload.len() as u64, &mut planes.descriptor);
            planes.defect.extend_from_slice(&payload);
            stats.defect_cells += 1;
        }
        Candidate::Transition { runs, payload } => {
            planes.descriptor.push(6);
            write_varint(runs as u64, &mut planes.descriptor);
            write_varint(payload.len() as u64, &mut planes.descriptor);
            planes.transition.extend_from_slice(&payload);
            stats.transition_cells += 1;
        }
    }
}

fn encode_overlay(template: &[u8], bytes: &[u8]) -> Option<(usize, Vec<u8>)> {
    if template.len() != bytes.len() || bytes.is_empty() {
        return None;
    }
    let mut payload = Vec::new();
    let mut changes = 0usize;
    let mut previous = None::<usize>;
    for (position, (&left, &right)) in template.iter().zip(bytes).enumerate() {
        if left == right {
            continue;
        }
        changes += 1;
        if changes.saturating_mul(4) > bytes.len() {
            return None;
        }
        let gap = previous.map_or(position, |value| position - value - 1);
        write_varint(gap as u64, &mut payload);
        payload.push(right);
        previous = Some(position);
    }
    if changes == 0 || payload.len().saturating_add(8) >= bytes.len() {
        None
    } else {
        Some((changes, payload))
    }
}

fn decode_overlay(
    output: &[u8],
    ranges: &[CellRange],
    base: usize,
    len: usize,
    changes: usize,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let range = ranges
        .get(base)
        .copied()
        .ok_or(PithosError::InvalidMetadata("PRS1 overlay base"))?;
    if range.len != len {
        return Err(PithosError::InvalidMetadata("PRS1 overlay base length"));
    }
    let end = range
        .start
        .checked_add(range.len)
        .ok_or(PithosError::IntegerOverflow)?;
    let mut bytes = output
        .get(range.start..end)
        .ok_or(PithosError::InvalidRange)?
        .to_vec();
    let mut pos = 0usize;
    let mut previous = None::<usize>;
    for _ in 0..changes {
        let gap = usize::try_from(read_varint(payload, &mut pos)?)
            .map_err(|_| PithosError::IntegerOverflow)?;
        let index = previous.map_or(gap, |value| value + 1 + gap);
        let value = *payload.get(pos).ok_or(PithosError::InvalidRange)?;
        pos += 1;
        *bytes.get_mut(index).ok_or(PithosError::InvalidRange)? = value;
        previous = Some(index);
    }
    if pos != payload.len() {
        return Err(PithosError::InvalidMetadata("PRS1 overlay trailing"));
    }
    Ok(bytes)
}

fn encode_mixture(bytes: &[u8]) -> Option<(Vec<u8>, u8, Vec<u8>)> {
    if bytes.len() < 64 {
        return None;
    }
    let mut counts = [0_u32; 256];
    for byte in bytes {
        counts[*byte as usize] += 1;
    }
    let alphabet = counts
        .iter()
        .enumerate()
        .filter_map(|(value, count)| (*count != 0).then_some(value as u8))
        .collect::<Vec<_>>();
    if !(2..=16).contains(&alphabet.len()) {
        return None;
    }
    let bits = bits_for_symbols(alphabet.len());
    let mut index = [u8::MAX; 256];
    for (position, value) in alphabet.iter().copied().enumerate() {
        index[value as usize] = position as u8;
    }
    let payload = pack_symbol_indexes(bytes, &index, bits);
    if payload.len().saturating_add(alphabet.len() + 8) >= bytes.len() {
        None
    } else {
        Some((alphabet, bits, payload))
    }
}

fn bits_for_symbols(count: usize) -> u8 {
    match count {
        0 | 1 | 2 => 1,
        3 | 4 => 2,
        5..=8 => 3,
        _ => 4,
    }
}

fn pack_symbol_indexes(bytes: &[u8], index: &[u8; 256], bits: u8) -> Vec<u8> {
    let mut output = Vec::with_capacity((bytes.len() * bits as usize).div_ceil(8));
    let mut accumulator = 0_u64;
    let mut accumulator_bits = 0_u8;
    for byte in bytes {
        accumulator |= u64::from(index[*byte as usize]) << accumulator_bits;
        accumulator_bits += bits;
        while accumulator_bits >= 8 {
            output.push(accumulator as u8);
            accumulator >>= 8;
            accumulator_bits -= 8;
        }
    }
    if accumulator_bits != 0 {
        output.push(accumulator as u8);
    }
    output
}

fn unpack_symbol_indexes(
    payload: &[u8],
    alphabet: &[u8],
    bits: u8,
    expected: usize,
) -> Result<Vec<u8>> {
    let mask = (1_u64 << bits) - 1;
    let mut output = Vec::with_capacity(expected);
    let mut input_pos = 0usize;
    let mut accumulator = 0_u64;
    let mut accumulator_bits = 0_u8;
    while output.len() < expected {
        while accumulator_bits < bits {
            let byte = *payload.get(input_pos).ok_or(PithosError::InvalidRange)?;
            input_pos += 1;
            accumulator |= u64::from(byte) << accumulator_bits;
            accumulator_bits += 8;
        }
        let symbol = (accumulator & mask) as usize;
        output.push(
            *alphabet
                .get(symbol)
                .ok_or(PithosError::InvalidMetadata("PRS1 mixture symbol"))?,
        );
        accumulator >>= bits;
        accumulator_bits -= bits;
    }
    if input_pos != payload.len() {
        return Err(PithosError::InvalidMetadata("PRS1 mixture trailing"));
    }
    Ok(output)
}

fn encode_axes(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut high = Vec::with_capacity(bytes.len().div_ceil(2));
    let mut low = Vec::with_capacity(bytes.len().div_ceil(2));
    for pair in bytes.chunks(2) {
        let first = pair[0];
        let second = pair.get(1).copied().unwrap_or(0);
        high.push((first & 0xf0) | (second >> 4));
        low.push(((first & 0x0f) << 4) | (second & 0x0f));
    }
    (high, low)
}

fn decode_axes(high: &[u8], low: &[u8], expected: usize) -> Result<Vec<u8>> {
    let packed = expected.div_ceil(2);
    if high.len() != packed || low.len() != packed {
        return Err(PithosError::InvalidMetadata("PRS1 axial length"));
    }
    let mut output = Vec::with_capacity(expected);
    for (&hi, &lo) in high.iter().zip(low) {
        output.push((hi & 0xf0) | (lo >> 4));
        if output.len() < expected {
            output.push((hi << 4) | (lo & 0x0f));
        }
    }
    Ok(output)
}

fn encode_defects(bytes: &[u8]) -> Option<(u8, usize, Vec<u8>)> {
    if bytes.len() < 64 {
        return None;
    }
    let mut counts = [0_u32; 256];
    for byte in bytes {
        counts[*byte as usize] += 1;
    }
    let (default, count) = counts
        .iter()
        .copied()
        .enumerate()
        .max_by_key(|(_, count)| *count)?;
    if (count as usize).saturating_mul(100) < bytes.len().saturating_mul(70) {
        return None;
    }
    let mut payload = Vec::new();
    let mut defects = 0usize;
    let mut previous = None::<usize>;
    for (position, byte) in bytes.iter().copied().enumerate() {
        if byte == default as u8 {
            continue;
        }
        let gap = previous.map_or(position, |value| position - value - 1);
        write_varint(gap as u64, &mut payload);
        payload.push(byte);
        defects += 1;
        previous = Some(position);
    }
    if defects == 0 || payload.len().saturating_add(8) >= bytes.len() {
        None
    } else {
        Some((default as u8, defects, payload))
    }
}

fn decode_defects(default: u8, len: usize, defects: usize, payload: &[u8]) -> Result<Vec<u8>> {
    let mut output = vec![default; len];
    let mut pos = 0usize;
    let mut previous = None::<usize>;
    for _ in 0..defects {
        let gap = usize::try_from(read_varint(payload, &mut pos)?)
            .map_err(|_| PithosError::IntegerOverflow)?;
        let index = previous.map_or(gap, |value| value + 1 + gap);
        let value = *payload.get(pos).ok_or(PithosError::InvalidRange)?;
        pos += 1;
        *output.get_mut(index).ok_or(PithosError::InvalidRange)? = value;
        previous = Some(index);
    }
    if pos != payload.len() {
        return Err(PithosError::InvalidMetadata("PRS1 defect trailing"));
    }
    Ok(output)
}

fn encode_transitions(bytes: &[u8]) -> Option<(usize, Vec<u8>)> {
    if bytes.len() < 64 {
        return None;
    }
    let mut payload = Vec::new();
    let mut runs = 0usize;
    let mut start = 0usize;
    while start < bytes.len() {
        let value = bytes[start];
        let mut end = start + 1;
        while end < bytes.len() && bytes[end] == value {
            end += 1;
        }
        payload.push(value);
        write_varint((end - start) as u64, &mut payload);
        runs += 1;
        start = end;
    }
    if payload.len().saturating_add(6) >= bytes.len() {
        None
    } else {
        Some((runs, payload))
    }
}

fn decode_transitions(expected: usize, runs: usize, payload: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(expected);
    let mut pos = 0usize;
    for _ in 0..runs {
        let value = *payload.get(pos).ok_or(PithosError::InvalidRange)?;
        pos += 1;
        let run = usize::try_from(read_varint(payload, &mut pos)?)
            .map_err(|_| PithosError::MemoryLimit)?;
        let new_len = output
            .len()
            .checked_add(run)
            .ok_or(PithosError::IntegerOverflow)?;
        if new_len > expected {
            return Err(PithosError::ResourceLimit("PRS1 transition output"));
        }
        output.resize(new_len, value);
    }
    if pos != payload.len() || output.len() != expected {
        return Err(PithosError::InvalidMetadata("PRS1 transition trailing"));
    }
    Ok(output)
}

fn append_reference(
    output: &mut Vec<u8>,
    ranges: &[CellRange],
    base: usize,
    len: usize,
) -> Result<()> {
    let range = ranges
        .get(base)
        .copied()
        .ok_or(PithosError::InvalidMetadata("PRS1 reference base"))?;
    if range.len != len {
        return Err(PithosError::InvalidMetadata("PRS1 reference length"));
    }
    let end = range
        .start
        .checked_add(range.len)
        .ok_or(PithosError::IntegerOverflow)?;
    let bytes = output
        .get(range.start..end)
        .ok_or(PithosError::InvalidRange)?
        .to_vec();
    output.extend_from_slice(&bytes);
    Ok(())
}

fn encode_plane(plane_id: u16, bytes: &[u8], entropy_level: i32) -> Result<EncodedPlane> {
    let mut best_codec = CodecId::Store as u16;
    let mut best = bytes.to_vec();

    if !bytes.is_empty() {
        let mut zstd = Vec::new();
        ZstdCodec.encode(
            bytes,
            &CodecConfig {
                level: entropy_level.clamp(3, 19),
            },
            &mut zstd,
        )?;
        if zstd.len() < best.len() {
            best_codec = CodecId::Zstd as u16;
            best = zstd;
        }

        // The raw lane already competes against the standard full-group codecs.
        // Re-running LZMA2 over a very large raw plane only duplicates expensive
        // work. Reserve LZMA2 for transformed planes where multiplexing created
        // a genuinely different statistical context, and cap the candidate size.
        if plane_id != PLANE_RAW
            && (LZMA_MIN_PLANE_BYTES..=LZMA_MAX_PLANE_BYTES).contains(&bytes.len())
        {
            let mut lzma = Vec::new();
            Lzma2Codec.encode(bytes, &CodecConfig { level: 9 }, &mut lzma)?;
            if lzma.len() < best.len() {
                best_codec = CodecId::Lzma2 as u16;
                best = lzma;
            }
        }
    }

    Ok(EncodedPlane {
        plane_id,
        codec_id: best_codec,
        raw_len: bytes.len() as u64,
        crc32c: crc32c::crc32c(&best),
        payload: best,
    })
}

fn decode_plane(codec_id: u16, payload: &[u8], raw_len: u64) -> Result<Vec<u8>> {
    if codec_id == CodecId::Store as u16 {
        if payload.len() as u64 != raw_len {
            return Err(PithosError::InvalidRange);
        }
        return Ok(payload.to_vec());
    }
    let mut output = Vec::with_capacity(
        usize::try_from(raw_len).map_err(|_| PithosError::MemoryLimit)?,
    );
    match CodecId::from_u16(codec_id) {
        Some(CodecId::Zstd) => ZstdCodec.decode(&mut Cursor::new(payload), raw_len, &mut output)?,
        Some(CodecId::Lzma2) => {
            Lzma2Codec.decode(&mut Cursor::new(payload), raw_len, &mut output)?
        }
        _ => return Err(PithosError::UnsupportedCodec),
    }
    if output.len() as u64 != raw_len {
        return Err(PithosError::InvalidRange);
    }
    Ok(output)
}

fn validate_plane_records(records: &[PlaneRecord]) -> Result<()> {
    if records.len() != usize::from(PLANE_COUNT) {
        return Err(PithosError::InvalidMetadata("PRS1 plane count"));
    }
    let mut seen = [false; PLANE_COUNT as usize];
    for record in records {
        if usize::from(record.plane_id) >= seen.len() || seen[usize::from(record.plane_id)] {
            return Err(PithosError::InvalidMetadata("PRS1 plane identity"));
        }
        if !matches!(record.codec_id, 0 | 1 | 3) {
            return Err(PithosError::UnsupportedCodec);
        }
        seen[usize::from(record.plane_id)] = true;
    }
    if seen.iter().any(|value| !value) {
        return Err(PithosError::MissingSection("PRS1 plane"));
    }
    Ok(())
}

fn validate_plane_bounds(
    records: &[PlaneRecord],
    expected_len: u64,
    payload_len: usize,
    data_start: usize,
) -> Result<()> {
    let per_plane_limit = expected_len
        .checked_add(DECODE_SLACK_BYTES)
        .ok_or(PithosError::IntegerOverflow)?;
    let aggregate_limit = expected_len
        .checked_mul(2)
        .and_then(|value| value.checked_add(DECODE_SLACK_BYTES))
        .ok_or(PithosError::IntegerOverflow)?;
    let mut aggregate_raw = 0_u64;
    let mut aggregate_encoded = 0_u64;
    for record in records {
        if record.raw_len > per_plane_limit {
            return Err(PithosError::ResourceLimit("PRS1 plane raw bytes"));
        }
        aggregate_raw = aggregate_raw
            .checked_add(record.raw_len)
            .ok_or(PithosError::IntegerOverflow)?;
        aggregate_encoded = aggregate_encoded
            .checked_add(record.encoded_len)
            .ok_or(PithosError::IntegerOverflow)?;
    }
    if aggregate_raw > aggregate_limit {
        return Err(PithosError::ResourceLimit("PRS1 aggregate plane bytes"));
    }
    let expected_payload = u64::try_from(data_start)
        .map_err(|_| PithosError::IntegerOverflow)?
        .checked_add(aggregate_encoded)
        .ok_or(PithosError::IntegerOverflow)?;
    if expected_payload != payload_len as u64 {
        return Err(PithosError::InvalidRange);
    }
    Ok(())
}

fn entropy_estimate_bytes(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let mut counts = [0_u32; 256];
    for byte in bytes {
        counts[*byte as usize] += 1;
    }
    let total = bytes.len() as f64;
    let bits = counts
        .iter()
        .copied()
        .filter(|count| *count != 0)
        .map(|count| {
            let probability = f64::from(count) / total;
            -f64::from(count) * probability.log2()
        })
        .sum::<f64>();
    (bits / 8.0).ceil() as usize
}

fn axial_entropy_estimate_bytes(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let mut high = [0_u32; 16];
    let mut low = [0_u32; 16];
    for byte in bytes {
        high[(byte >> 4) as usize] += 1;
        low[(byte & 0x0f) as usize] += 1;
    }
    let total = bytes.len() as f64;
    let bits = high
        .iter()
        .chain(low.iter())
        .copied()
        .filter(|count| *count != 0)
        .map(|count| {
            let probability = f64::from(count) / total;
            -f64::from(count) * probability.log2()
        })
        .sum::<f64>();
    (bits / 8.0).ceil() as usize
}

fn coarse_fingerprint(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut histogram = [0_u32; 16];
    for byte in bytes {
        histogram[(byte >> 4) as usize] += 1;
    }
    let mut fingerprint = 0_u64;
    for (index, count) in histogram.iter().copied().enumerate() {
        let quantized = ((count as usize).saturating_mul(15) / bytes.len()).min(15) as u64;
        fingerprint |= quantized << (index * 4);
    }
    fingerprint
}

fn write_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    loop {
        if shift >= 64 {
            return Err(PithosError::IntegerOverflow);
        }
        let byte = *bytes.get(*pos).ok_or(PithosError::InvalidRange)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

fn varint_len(mut value: u64) -> usize {
    let mut len = 1usize;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
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

struct PlaneCursor {
    bytes: Vec<u8>,
    pos: usize,
}

impl PlaneCursor {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&[u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(PithosError::IntegerOverflow)?;
        let bytes = self
            .bytes
            .get(self.pos..end)
            .ok_or(PithosError::InvalidRange)?;
        self.pos = end;
        Ok(bytes)
    }

    fn finished(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(members: &[Vec<u8>]) -> (Vec<u8>, SubstrateStats) {
        let mut input = Vec::new();
        let mut lengths = Vec::new();
        for member in members {
            lengths.push(member.len() as u64);
            input.extend_from_slice(member);
        }
        let (payload, stats) = encode(&input, &lengths, 9).unwrap();
        assert_eq!(decode(&payload, input.len() as u64).unwrap(), input);
        (payload, stats)
    }

    #[test]
    fn exact_reference_and_overlay_roundtrip() {
        let base = b"representation-substrate-template".repeat(4096);
        let same = base.clone();
        let mut changed = base.clone();
        changed[12_345] ^= 0x55;
        changed[54_321] ^= 0x33;
        let (_, stats) = roundtrip(&[base, same, changed]);
        assert!(stats.exact_ref_cells > 0);
        assert!(stats.overlay_cells > 0 || stats.raw_cells > 0);
    }

    #[test]
    fn mixture_defect_transition_and_axes_are_reversible() {
        let mixture = (0..256 * 1024)
            .map(|index| [b'A', b'B', b'C', b'D'][index % 4])
            .collect::<Vec<_>>();
        let mut defect = vec![0_u8; 256 * 1024];
        for index in (0..defect.len()).step_by(997) {
            defect[index] = 7;
        }
        let mut transition = Vec::new();
        for index in 0..4096 {
            transition.extend(std::iter::repeat_n((index % 7) as u8, 128));
        }
        let axial = (0..256 * 1024)
            .map(|index| ((index & 0x0f) as u8) | 0xa0)
            .collect::<Vec<_>>();
        let (_, stats) = roundtrip(&[mixture, defect, transition, axial]);
        assert!(stats.cell_count > 0);
        assert!(
            stats.mixture_cells + stats.defect_cells + stats.transition_cells + stats.axial_cells > 0
        );
    }

    #[test]
    fn recursive_partition_respects_member_boundaries() {
        let left = vec![b'A'; MAX_CELL_BYTES + 12345];
        let right = (0..MAX_CELL_BYTES + 54321)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut input = left.clone();
        input.extend_from_slice(&right);
        let ranges = partition_members(&input, &[left.len() as u64, right.len() as u64]).unwrap();
        assert!(!ranges.is_empty());
        for range in ranges {
            let end = range.start + range.len;
            assert!(end <= left.len() || range.start >= left.len());
        }
    }

    #[test]
    fn decoder_rejects_impossible_plane_raw_length() {
        let member = b"prs1-bounds".repeat(8192);
        let lengths = [member.len() as u64];
        let (mut payload, _) = encode(&member, &lengths, 9).unwrap();
        let malicious = (member.len() as u64)
            .saturating_add(DECODE_SLACK_BYTES)
            .saturating_add(1);
        payload[HEADER_LEN + 4..HEADER_LEN + 12].copy_from_slice(&malicious.to_le_bytes());
        assert!(decode(&payload, member.len() as u64).is_err());
    }
}
