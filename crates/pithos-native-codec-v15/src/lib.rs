//! Native codec v15: shortmer-inspired motif codebook.
//!
//! A small deterministic dictionary of recurring 8-byte motifs is mined from
//! the archive-wide input. The input is converted into literal blocks and
//! single-byte motif symbols, then handed to the v14 global canonical pool.
//! The complete v14 baseline is retained whenever the final motif envelope is
//! not strictly smaller.

use pithos_core::{PithosError, Result};
use std::cmp::Reverse;
use std::collections::HashMap;

pub const NATIVE_CODEC_ID: u16 = 4;
pub const NATIVE_CODEC_VERSION: u16 = 15;
const MAGIC: &[u8; 4] = b"PN15";
const HEADER_LEN: usize = 32;
const MOTIF_LEN: usize = 8;
const MAX_MOTIFS: usize = 32;
const SAMPLE_STRIDE: usize = 256;
const MAX_SAMPLE_KEYS: usize = 131_072;
const MIN_INPUT_BYTES: usize = 1024 * 1024;

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
        pithos_native_v14::encode_exact_dedup(input, member_lengths, level)?;
    let baseline_len = baseline.len() as u64;
    if input.len() < MIN_INPUT_BYTES {
        return Ok((baseline, convert_stats(baseline_stats, baseline_len)));
    }

    let dictionary = build_dictionary(input);
    if dictionary.is_empty() {
        return Ok((baseline, convert_stats(baseline_stats, baseline_len)));
    }
    let transformed = encode_motifs(input, &dictionary);
    if transformed.len() >= input.len() {
        return Ok((baseline, convert_stats(baseline_stats, baseline_len)));
    }

    let transformed_lengths = [transformed.len() as u64];
    let (inner, inner_stats) =
        pithos_native_v14::encode_exact_dedup(&transformed, &transformed_lengths, level)?;
    let dictionary_bytes = dictionary
        .len()
        .checked_mul(MOTIF_LEN)
        .ok_or(PithosError::IntegerOverflow)?;
    let capacity = HEADER_LEN
        .checked_add(dictionary_bytes)
        .and_then(|value| value.checked_add(inner.len()))
        .ok_or(PithosError::IntegerOverflow)?;
    let mut wrapped = Vec::with_capacity(capacity);
    wrapped.extend_from_slice(MAGIC);
    wrapped.extend_from_slice(&NATIVE_CODEC_VERSION.to_le_bytes());
    wrapped.push(u8::try_from(dictionary.len()).map_err(|_| PithosError::IntegerOverflow)?);
    wrapped.push(MOTIF_LEN as u8);
    wrapped.extend_from_slice(&(input.len() as u64).to_le_bytes());
    wrapped.extend_from_slice(&(transformed.len() as u64).to_le_bytes());
    wrapped.extend_from_slice(&(inner.len() as u64).to_le_bytes());
    for motif in &dictionary {
        wrapped.extend_from_slice(motif);
    }
    wrapped.extend_from_slice(&inner);

    if wrapped.len() as u64 >= baseline_len {
        return Ok((baseline, convert_stats(baseline_stats, baseline_len)));
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
        return pithos_native_v14::decode_exact_dedup(payload, expected_len);
    }
    let version = read_u16(payload, 4)?;
    let motif_count = usize::from(*payload.get(6).ok_or(PithosError::InvalidRange)?);
    let motif_len = usize::from(*payload.get(7).ok_or(PithosError::InvalidRange)?);
    let original_len = read_u64(payload, 8)?;
    let transformed_len = read_u64(payload, 16)?;
    let inner_len = read_u64(payload, 24)?;
    if version != NATIVE_CODEC_VERSION
        || motif_len != MOTIF_LEN
        || motif_count == 0
        || motif_count > MAX_MOTIFS
        || original_len != expected_len
    {
        return Err(PithosError::InvalidMetadata("native v15 header"));
    }
    let dictionary_bytes = motif_count
        .checked_mul(MOTIF_LEN)
        .ok_or(PithosError::IntegerOverflow)?;
    let dictionary_end = HEADER_LEN
        .checked_add(dictionary_bytes)
        .ok_or(PithosError::IntegerOverflow)?;
    let inner_len = usize::try_from(inner_len).map_err(|_| PithosError::MemoryLimit)?;
    let inner_end = dictionary_end
        .checked_add(inner_len)
        .ok_or(PithosError::IntegerOverflow)?;
    if inner_end != payload.len() {
        return Err(PithosError::InvalidRange);
    }

    let mut dictionary = Vec::<[u8; MOTIF_LEN]>::with_capacity(motif_count);
    let mut cursor = HEADER_LEN;
    for _ in 0..motif_count {
        let end = cursor
            .checked_add(MOTIF_LEN)
            .ok_or(PithosError::IntegerOverflow)?;
        let bytes = payload.get(cursor..end).ok_or(PithosError::InvalidRange)?;
        dictionary.push(bytes.try_into().map_err(|_| PithosError::InvalidRange)?);
        cursor = end;
    }
    let transformed = pithos_native_v14::decode_exact_dedup(
        payload
            .get(dictionary_end..inner_end)
            .ok_or(PithosError::InvalidRange)?,
        transformed_len,
    )?;
    decode_motifs(&transformed, &dictionary, expected_len)
}

fn build_dictionary(input: &[u8]) -> Vec<[u8; MOTIF_LEN]> {
    if input.len() < MOTIF_LEN {
        return Vec::new();
    }
    let mut counts = HashMap::<[u8; MOTIF_LEN], u32>::new();
    for start in (0..=input.len() - MOTIF_LEN).step_by(SAMPLE_STRIDE) {
        let motif: [u8; MOTIF_LEN] = input[start..start + MOTIF_LEN]
            .try_into()
            .expect("fixed motif length");
        if let Some(count) = counts.get_mut(&motif) {
            *count = count.saturating_add(1);
        } else if counts.len() < MAX_SAMPLE_KEYS {
            counts.insert(motif, 1);
        }
    }
    let mut ranked = counts
        .into_iter()
        .filter(|(_, count)| *count >= 3)
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(motif, count)| (Reverse(*count), *motif));
    ranked.truncate(MAX_MOTIFS);
    ranked.into_iter().map(|(motif, _)| motif).collect()
}

fn encode_motifs(input: &[u8], dictionary: &[[u8; MOTIF_LEN]]) -> Vec<u8> {
    let lookup = dictionary
        .iter()
        .enumerate()
        .map(|(index, motif)| (*motif, (index + 1) as u8))
        .collect::<HashMap<_, _>>();
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    let mut literal_start = 0usize;
    while cursor < input.len() {
        let tag = if cursor + MOTIF_LEN <= input.len() {
            let motif: [u8; MOTIF_LEN] = input[cursor..cursor + MOTIF_LEN]
                .try_into()
                .expect("fixed motif length");
            lookup.get(&motif).copied()
        } else {
            None
        };
        if let Some(tag) = tag {
            if literal_start < cursor {
                emit_literal(&input[literal_start..cursor], &mut output);
            }
            output.push(tag);
            cursor += MOTIF_LEN;
            literal_start = cursor;
        } else {
            cursor += 1;
        }
    }
    if literal_start < input.len() {
        emit_literal(&input[literal_start..], &mut output);
    }
    output
}

fn emit_literal(bytes: &[u8], output: &mut Vec<u8>) {
    output.push(0);
    write_varint(bytes.len() as u64, output);
    output.extend_from_slice(bytes);
}

fn decode_motifs(
    transformed: &[u8],
    dictionary: &[[u8; MOTIF_LEN]],
    expected_len: u64,
) -> Result<Vec<u8>> {
    let expected = usize::try_from(expected_len).map_err(|_| PithosError::MemoryLimit)?;
    let mut output = Vec::with_capacity(expected);
    let mut cursor = 0usize;
    while cursor < transformed.len() {
        let tag = *transformed.get(cursor).ok_or(PithosError::InvalidRange)?;
        cursor += 1;
        if tag == 0 {
            let length = usize::try_from(read_varint(transformed, &mut cursor)?)
                .map_err(|_| PithosError::MemoryLimit)?;
            let end = cursor
                .checked_add(length)
                .ok_or(PithosError::IntegerOverflow)?;
            let bytes = transformed
                .get(cursor..end)
                .ok_or(PithosError::InvalidRange)?;
            if output.len().saturating_add(bytes.len()) > expected {
                return Err(PithosError::ResourceLimit("native motif output"));
            }
            output.extend_from_slice(bytes);
            cursor = end;
        } else {
            let motif = dictionary
                .get(usize::from(tag - 1))
                .ok_or(PithosError::InvalidMetadata("native motif index"))?;
            if output.len().saturating_add(MOTIF_LEN) > expected {
                return Err(PithosError::ResourceLimit("native motif output"));
            }
            output.extend_from_slice(motif);
        }
    }
    if output.len() != expected {
        return Err(PithosError::InvalidRange);
    }
    Ok(output)
}

fn convert_stats(stats: pithos_native_v14::NativeStats, encoded_bytes: u64) -> NativeStats {
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
        *cursor = cursor.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(PithosError::InvalidMetadata("native motif varint"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motif_transform_roundtrips() {
        let dictionary = vec![*b"ABCDEFGH"];
        let input = b"xxABCDEFGHyyABCDEFGHzz";
        let encoded = encode_motifs(input, &dictionary);
        assert!(encoded.len() < input.len());
        assert_eq!(
            decode_motifs(&encoded, &dictionary, input.len() as u64).unwrap(),
            input
        );
    }

    #[test]
    fn codec_falls_back_or_roundtrips() {
        let input = b"ABCDEFGH".repeat(256 * 1024);
        let lengths = [input.len() as u64];
        let (payload, _) = encode_exact_dedup(&input, &lengths, 15).unwrap();
        assert_eq!(
            decode_exact_dedup(&payload, input.len() as u64).unwrap(),
            input
        );
    }
}
