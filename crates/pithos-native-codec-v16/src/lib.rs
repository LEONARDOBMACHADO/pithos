//! Native codec v16: implicit archive-wide references.
//!
//! Repeated regions are represented as distance/length references to bytes
//! already reconstructed. The proven v15 payload is the baseline. The new
//! reference representation is sent directly to the v14 representation core,
//! rather than recursively invoking the entire transform stack. This keeps the
//! experimental cost approximately linear while preserving exact final-size
//! arbitration.

use pithos_core::{PithosError, Result};
use std::collections::HashMap;
use xxhash_rust::xxh3::xxh3_64;

pub const NATIVE_CODEC_ID: u16 = 4;
pub const NATIVE_CODEC_VERSION: u16 = 16;
const MAGIC: &[u8; 4] = b"PN16";
const HEADER_LEN: usize = 32;
const ANCHOR_LEN: usize = 64;
const ANCHOR_STRIDE: usize = 64;
const MAX_MATCH_LEN: usize = 4 * 1024 * 1024;
const MAX_CANDIDATES_PER_ANCHOR: usize = 4;
const MIN_INPUT_BYTES: usize = 1024 * 1024;
const MIN_RAW_SAVING_PER_MILLE: usize = 5;

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
    let (baseline, baseline_stats) =
        pithos_native_v15::encode_exact_dedup(input, member_lengths, level)?;
    let baseline_len = baseline.len() as u64;
    if input.len() < MIN_INPUT_BYTES {
        return Ok((baseline, convert_baseline_stats(baseline_stats, baseline_len)));
    }

    let transformed = encode_global_refs(input);
    let required_max = input
        .len()
        .saturating_mul(1000usize.saturating_sub(MIN_RAW_SAVING_PER_MILLE))
        / 1000;
    if transformed.len() >= required_max {
        return Ok((baseline, convert_baseline_stats(baseline_stats, baseline_len)));
    }

    let transformed_lengths = [transformed.len() as u64];
    let (inner, inner_stats) =
        pithos_native_core::encode_exact_dedup(&transformed, &transformed_lengths, level)?;
    let capacity = HEADER_LEN
        .checked_add(inner.len())
        .ok_or(PithosError::IntegerOverflow)?;
    let mut wrapped = Vec::with_capacity(capacity);
    wrapped.extend_from_slice(MAGIC);
    wrapped.extend_from_slice(&NATIVE_CODEC_VERSION.to_le_bytes());
    wrapped.extend_from_slice(&0_u16.to_le_bytes());
    wrapped.extend_from_slice(&(input.len() as u64).to_le_bytes());
    wrapped.extend_from_slice(&(transformed.len() as u64).to_le_bytes());
    wrapped.extend_from_slice(&(inner.len() as u64).to_le_bytes());
    wrapped.extend_from_slice(&inner);

    if wrapped.len() as u64 >= baseline_len {
        return Ok((baseline, convert_baseline_stats(baseline_stats, baseline_len)));
    }
    let encoded_bytes = wrapped.len() as u64;
    Ok((
        wrapped,
        NativeStats {
            chunk_count: inner_stats.chunk_count,
            canonical_chunks: inner_stats.canonical_chunks,
            gross_duplicate_bytes: inner_stats.gross_duplicate_bytes,
            representation_bytes: transformed.len() as u64,
            encoded_bytes,
        },
    ))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    if payload.len() < HEADER_LEN || &payload[..4] != MAGIC {
        return pithos_native_v15::decode_exact_dedup(payload, expected_len);
    }
    let version = read_u16(payload, 4)?;
    let flags = read_u16(payload, 6)?;
    let original_len = read_u64(payload, 8)?;
    let transformed_len = read_u64(payload, 16)?;
    let inner_len = read_u64(payload, 24)?;
    if version != NATIVE_CODEC_VERSION || flags != 0 || original_len != expected_len {
        return Err(PithosError::InvalidMetadata("native v16 header"));
    }
    let inner_len = usize::try_from(inner_len).map_err(|_| PithosError::MemoryLimit)?;
    let inner_end = HEADER_LEN
        .checked_add(inner_len)
        .ok_or(PithosError::IntegerOverflow)?;
    if inner_end != payload.len() {
        return Err(PithosError::InvalidRange);
    }
    let transformed = pithos_native_core::decode_exact_dedup(
        payload
            .get(HEADER_LEN..inner_end)
            .ok_or(PithosError::InvalidRange)?,
        transformed_len,
    )?;
    decode_global_refs(&transformed, expected_len)
}

fn encode_global_refs(input: &[u8]) -> Vec<u8> {
    if input.len() < ANCHOR_LEN {
        let mut output = Vec::new();
        emit_literal(input, &mut output);
        return output;
    }
    let mut anchors = HashMap::<u64, Vec<usize>>::new();
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    let mut literal_start = 0usize;

    while cursor + ANCHOR_LEN <= input.len() {
        let anchor = xxh3_64(&input[cursor..cursor + ANCHOR_LEN]);
        let mut best_start = None;
        let mut best_len = 0usize;
        if let Some(candidates) = anchors.get(&anchor) {
            for &candidate in candidates.iter().rev() {
                if candidate >= cursor
                    || input[candidate..candidate + ANCHOR_LEN]
                        != input[cursor..cursor + ANCHOR_LEN]
                {
                    continue;
                }
                let maximum = MAX_MATCH_LEN.min(input.len() - cursor);
                let length = extend_match(input, candidate, cursor, maximum);
                if length > best_len {
                    best_len = length;
                    best_start = Some(candidate);
                }
            }
        }

        if best_len >= ANCHOR_LEN {
            if literal_start < cursor {
                emit_literal(&input[literal_start..cursor], &mut output);
            }
            let source = best_start.expect("match has source");
            output.push(1);
            write_varint((cursor - source) as u64, &mut output);
            write_varint(best_len as u64, &mut output);
            cursor += best_len;
            literal_start = cursor;
            continue;
        }

        let candidates = anchors.entry(anchor).or_default();
        candidates.push(cursor);
        if candidates.len() > MAX_CANDIDATES_PER_ANCHOR {
            candidates.remove(0);
        }
        cursor = cursor.saturating_add(ANCHOR_STRIDE);
    }

    if literal_start < input.len() {
        emit_literal(&input[literal_start..], &mut output);
    }
    output
}

fn extend_match(input: &[u8], source: usize, target: usize, maximum: usize) -> usize {
    let mut length = ANCHOR_LEN;
    while length < maximum {
        let source_index = source + length;
        let target_index = target + length;
        if target_index >= input.len() || source_index >= input.len() {
            break;
        }
        if input[source_index] != input[target_index] {
            break;
        }
        length += 1;
    }
    length
}

fn emit_literal(bytes: &[u8], output: &mut Vec<u8>) {
    if bytes.is_empty() {
        return;
    }
    output.push(0);
    write_varint(bytes.len() as u64, output);
    output.extend_from_slice(bytes);
}

fn decode_global_refs(transformed: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    let expected = usize::try_from(expected_len).map_err(|_| PithosError::MemoryLimit)?;
    let mut output = Vec::with_capacity(expected);
    let mut cursor = 0usize;
    while cursor < transformed.len() {
        let tag = *transformed.get(cursor).ok_or(PithosError::InvalidRange)?;
        cursor += 1;
        match tag {
            0 => {
                let length = usize::try_from(read_varint(transformed, &mut cursor)?)
                    .map_err(|_| PithosError::MemoryLimit)?;
                let end = cursor
                    .checked_add(length)
                    .ok_or(PithosError::IntegerOverflow)?;
                let bytes = transformed.get(cursor..end).ok_or(PithosError::InvalidRange)?;
                if output.len().saturating_add(bytes.len()) > expected {
                    return Err(PithosError::ResourceLimit("native reference output"));
                }
                output.extend_from_slice(bytes);
                cursor = end;
            }
            1 => {
                let distance = usize::try_from(read_varint(transformed, &mut cursor)?)
                    .map_err(|_| PithosError::IntegerOverflow)?;
                let length = usize::try_from(read_varint(transformed, &mut cursor)?)
                    .map_err(|_| PithosError::MemoryLimit)?;
                if distance == 0
                    || distance > output.len()
                    || output.len().saturating_add(length) > expected
                {
                    return Err(PithosError::InvalidMetadata("native back reference"));
                }
                let source = output.len() - distance;
                for index in 0..length {
                    let byte = *output
                        .get(source + index)
                        .ok_or(PithosError::InvalidMetadata("native overlapping reference"))?;
                    output.push(byte);
                }
            }
            _ => return Err(PithosError::InvalidMetadata("native reference tag")),
        }
    }
    if output.len() != expected {
        return Err(PithosError::InvalidRange);
    }
    Ok(output)
}

fn convert_baseline_stats(
    stats: pithos_native_v15::NativeStats,
    encoded_bytes: u64,
) -> NativeStats {
    NativeStats {
        chunk_count: stats.chunk_count,
        canonical_chunks: stats.canonical_chunks,
        gross_duplicate_bytes: stats.gross_duplicate_bytes,
        representation_bytes: stats.representation_bytes,
        encoded_bytes,
    }
}

fn write_varint(mut value: u64, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    for _ in 0..10 {
        let byte = *bytes.get(*cursor).ok_or(PithosError::InvalidRange)?;
        *cursor = cursor
            .checked_add(1)
            .ok_or(PithosError::IntegerOverflow)?;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(PithosError::InvalidMetadata("native reference varint"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes.get(offset..offset + 2).ok_or(PithosError::InvalidRange)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes.get(offset..offset + 8).ok_or(PithosError::InvalidRange)?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_reference_transform_roundtrips() {
        let repeated = b"global-reference-window".repeat(4096);
        let mut input = repeated.clone();
        input.extend_from_slice(b"separator");
        input.extend_from_slice(&repeated);
        let transformed = encode_global_refs(&input);
        assert!(transformed.len() < input.len());
        assert_eq!(
            decode_global_refs(&transformed, input.len() as u64).unwrap(),
            input
        );
    }
}
