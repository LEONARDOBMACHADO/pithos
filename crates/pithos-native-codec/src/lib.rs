//! Reversible native Pithos transform codec.
//!
//! Version 1 persists exact content-defined deduplication inside one solid
//! group. The outer PAF remains responsible for entries, integrity and restore
//! maps; this codec must always decode back to the exact original group bytes.

use pithos_analysis::{ChunkOrigin, ChunkingConfig, chunk_fastcdc};
use pithos_core::{PithosError, Result};
use std::collections::HashMap;
use std::io::Cursor;

pub const NATIVE_CODEC_ID: u16 = 4;
pub const NATIVE_CODEC_VERSION: u16 = 1;
const MAGIC: &[u8; 4] = b"PNT1";
const HEADER_LEN: usize = 24;
const MAX_NATIVE_CHUNKS: usize = 50_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}

/// Encodes one complete solid group. `member_lengths` describes the original
/// file boundaries inside the group so FastCDC is reset for each logical file.
pub fn encode_exact_dedup(
    input: &[u8],
    member_lengths: &[u64],
    zstd_level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    validate_members(input.len(), member_lengths)?;
    let config = ChunkingConfig::default();
    let mut canonical = Vec::<Vec<u8>>::new();
    let mut candidates = HashMap::<[u8; 32], Vec<u32>>::new();
    let mut sequence = Vec::<u32>::new();
    let mut gross_duplicate_bytes = 0_u64;
    let mut member_base = 0_u64;

    for (member_id, member_length) in member_lengths.iter().copied().enumerate() {
        let start = usize::try_from(member_base).map_err(|_| PithosError::IntegerOverflow)?;
        let member_length_usize =
            usize::try_from(member_length).map_err(|_| PithosError::IntegerOverflow)?;
        let end = start
            .checked_add(member_length_usize)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(start..end).ok_or(PithosError::InvalidRange)?;
        let drafts = chunk_fastcdc(
            member,
            ChunkOrigin {
                entry_id: member_id as u64,
                object_id: 0,
                base_offset: member_base,
            },
            &config,
        )?;
        for draft in drafts {
            if sequence.len() >= MAX_NATIVE_CHUNKS {
                return Err(PithosError::ResourceLimit("native chunk count"));
            }
            let chunk_start =
                usize::try_from(draft.logical_offset).map_err(|_| PithosError::IntegerOverflow)?;
            let chunk_end = chunk_start
                .checked_add(draft.length as usize)
                .ok_or(PithosError::IntegerOverflow)?;
            let bytes = input
                .get(chunk_start..chunk_end)
                .ok_or(PithosError::InvalidRange)?;
            let hash = *blake3::hash(bytes).as_bytes();

            let mut canonical_index = None;
            if let Some(indexes) = candidates.get(&hash) {
                for index in indexes {
                    let candidate = canonical
                        .get(*index as usize)
                        .ok_or(PithosError::InvalidMetadata("native canonical index"))?;
                    if candidate.as_slice() == bytes {
                        canonical_index = Some(*index);
                        gross_duplicate_bytes = gross_duplicate_bytes
                            .checked_add(bytes.len() as u64)
                            .ok_or(PithosError::IntegerOverflow)?;
                        break;
                    }
                }
            }
            let index = if let Some(index) = canonical_index {
                index
            } else {
                let index = u32::try_from(canonical.len())
                    .map_err(|_| PithosError::ResourceLimit("native canonical chunks"))?;
                canonical.push(bytes.to_vec());
                candidates.entry(hash).or_default().push(index);
                index
            };
            sequence.push(index);
        }
        member_base = member_base
            .checked_add(member_length)
            .ok_or(PithosError::IntegerOverflow)?;
    }

    let chunk_count = u32::try_from(sequence.len())
        .map_err(|_| PithosError::ResourceLimit("native chunk count"))?;
    let canonical_count = u32::try_from(canonical.len())
        .map_err(|_| PithosError::ResourceLimit("native canonical chunks"))?;
    let mut representation = Vec::new();
    representation.extend_from_slice(MAGIC);
    representation.extend_from_slice(&NATIVE_CODEC_VERSION.to_le_bytes());
    representation.extend_from_slice(&0_u16.to_le_bytes());
    representation.extend_from_slice(&(input.len() as u64).to_le_bytes());
    representation.extend_from_slice(&chunk_count.to_le_bytes());
    representation.extend_from_slice(&canonical_count.to_le_bytes());
    for index in &sequence {
        representation.extend_from_slice(&index.to_le_bytes());
    }
    for bytes in &canonical {
        let length = u32::try_from(bytes.len()).map_err(|_| PithosError::IntegerOverflow)?;
        representation.extend_from_slice(&length.to_le_bytes());
        representation.extend_from_slice(bytes);
    }

    let encoded = zstd::stream::encode_all(Cursor::new(&representation), zstd_level)?;
    let stats = NativeStats {
        chunk_count,
        canonical_chunks: canonical_count,
        gross_duplicate_bytes,
        representation_bytes: representation.len() as u64,
        encoded_bytes: encoded.len() as u64,
    };
    Ok((encoded, stats))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    let representation = zstd::stream::decode_all(Cursor::new(payload))?;
    if representation.len() < HEADER_LEN || &representation[..4] != MAGIC {
        return Err(PithosError::InvalidMagic);
    }
    let version = read_u16(&representation, 4)?;
    let flags = read_u16(&representation, 6)?;
    let original_len = read_u64(&representation, 8)?;
    let chunk_count = read_u32(&representation, 16)? as usize;
    let canonical_count = read_u32(&representation, 20)? as usize;
    if version != NATIVE_CODEC_VERSION || flags != 0 || original_len != expected_len {
        return Err(PithosError::InvalidMetadata("native codec header"));
    }
    if chunk_count > MAX_NATIVE_CHUNKS || canonical_count > chunk_count {
        return Err(PithosError::ResourceLimit("native chunk count"));
    }
    let expected_len_usize = usize::try_from(expected_len).map_err(|_| PithosError::MemoryLimit)?;
    let sequence_bytes = chunk_count
        .checked_mul(4)
        .ok_or(PithosError::IntegerOverflow)?;
    let mut cursor = HEADER_LEN;
    let sequence_end = cursor
        .checked_add(sequence_bytes)
        .ok_or(PithosError::IntegerOverflow)?;
    if sequence_end > representation.len() {
        return Err(PithosError::InvalidRange);
    }
    let mut sequence = Vec::with_capacity(chunk_count);
    while cursor < sequence_end {
        sequence.push(read_u32(&representation, cursor)?);
        cursor += 4;
    }

    let mut canonical = Vec::<&[u8]>::with_capacity(canonical_count);
    for _ in 0..canonical_count {
        let length = read_u32(&representation, cursor)? as usize;
        cursor = cursor.checked_add(4).ok_or(PithosError::IntegerOverflow)?;
        let end = cursor
            .checked_add(length)
            .ok_or(PithosError::IntegerOverflow)?;
        let bytes = representation
            .get(cursor..end)
            .ok_or(PithosError::InvalidRange)?;
        canonical.push(bytes);
        cursor = end;
    }
    if cursor != representation.len() {
        return Err(PithosError::InvalidMetadata("native trailing bytes"));
    }

    let mut output = Vec::with_capacity(expected_len_usize);
    for index in sequence {
        let bytes = canonical
            .get(index as usize)
            .ok_or(PithosError::InvalidMetadata("native reference"))?;
        if output
            .len()
            .checked_add(bytes.len())
            .is_none_or(|next| next > expected_len_usize)
        {
            return Err(PithosError::ResourceLimit("native decoded output"));
        }
        output.extend_from_slice(bytes);
    }
    if output.len() != expected_len_usize {
        return Err(PithosError::InvalidRange);
    }
    Ok(output)
}

fn validate_members(input_len: usize, member_lengths: &[u64]) -> Result<()> {
    if member_lengths.is_empty() && input_len != 0 {
        return Err(PithosError::InvalidMetadata("native member boundaries"));
    }
    let total = member_lengths.iter().try_fold(0_u64, |total, length| {
        total
            .checked_add(*length)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    if total != input_len as u64 {
        return Err(PithosError::InvalidMetadata("native member boundaries"));
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
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
    fn exact_duplicate_members_share_canonical_chunks_and_roundtrip() {
        let file = vec![b'A'; 2 * 1024 * 1024];
        let mut input = file.clone();
        input.extend_from_slice(&file);
        let (encoded, stats) =
            encode_exact_dedup(&input, &[file.len() as u64, file.len() as u64], 9).unwrap();
        assert!(stats.canonical_chunks < stats.chunk_count);
        assert!(stats.gross_duplicate_bytes >= file.len() as u64);
        assert_eq!(
            decode_exact_dedup(&encoded, input.len() as u64).unwrap(),
            input
        );
    }

    #[test]
    fn unrelated_bytes_roundtrip_without_false_dedup() {
        let input = (0..(3 * 1024 * 1024))
            .map(|index| ((index * 131 + index / 17) % 251) as u8)
            .collect::<Vec<_>>();
        let (encoded, _) = encode_exact_dedup(&input, &[input.len() as u64], 3).unwrap();
        assert_eq!(
            decode_exact_dedup(&encoded, input.len() as u64).unwrap(),
            input
        );
    }
}
