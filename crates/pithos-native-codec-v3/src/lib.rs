//! Native codec v3: exact dedup plus a bounded multi-base reference graph.

use pithos_analysis::{ChunkOrigin, ChunkingConfig, chunk_fastcdc};
use pithos_core::{PithosError, Result};
use std::collections::HashMap;
use std::io::Cursor;
use xxhash_rust::xxh3::xxh3_64;

pub const NATIVE_CODEC_ID: u16 = pithos_native_v2::NATIVE_CODEC_ID;
pub const NATIVE_CODEC_VERSION: u16 = pithos_native_v2::NATIVE_CODEC_VERSION;
const MAGIC: &[u8; 4] = b"PNT3";
const HEADER_LEN: usize = 24;
const ANCHOR: usize = 12;
const MIN_COPY: usize = 24;
const MAX_ANCHOR_CANDIDATES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}

#[derive(Debug)]
enum Token {
    Literal(Vec<u8>),
    Copy { base: u32, offset: u32, len: u32 },
}

#[derive(Debug)]
struct CanonicalRecord {
    len: u32,
    tokens: Vec<Token>,
}

pub fn encode_exact_dedup(
    input: &[u8],
    member_lengths: &[u64],
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    validate_members(input, member_lengths)?;
    let (canonical, sequence, dup) = canonical_chunks(input, member_lengths)?;
    let mut anchor_index = HashMap::<u64, Vec<(u32, u32)>>::new();
    let mut records = Vec::with_capacity(canonical.len());
    for (id, bytes) in canonical.iter().enumerate() {
        let tokens = reference_tokens(bytes, &canonical[..id], &anchor_index)?;
        records.push(CanonicalRecord {
            len: bytes.len() as u32,
            tokens,
        });
        index_anchors(id as u32, bytes, &mut anchor_index);
    }

    let mut repr = Vec::new();
    repr.extend_from_slice(MAGIC);
    repr.extend_from_slice(&3_u16.to_le_bytes());
    repr.extend_from_slice(&0_u16.to_le_bytes());
    repr.extend_from_slice(&(input.len() as u64).to_le_bytes());
    repr.extend_from_slice(&(sequence.len() as u32).to_le_bytes());
    repr.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for id in &sequence {
        repr.extend_from_slice(&id.to_le_bytes());
    }
    for record in records {
        encode_record(record, &mut repr)?;
    }
    let encoded = zstd::stream::encode_all(Cursor::new(&repr), level)?;
    let encoded_bytes = encoded.len() as u64;
    Ok((
        encoded,
        NativeStats {
            chunk_count: sequence.len() as u32,
            canonical_chunks: canonical.len() as u32,
            gross_duplicate_bytes: dup,
            representation_bytes: repr.len() as u64,
            encoded_bytes,
        },
    ))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    let Ok(repr) = zstd::stream::decode_all(Cursor::new(payload)) else {
        return pithos_native_v2::decode_exact_dedup(payload, expected_len);
    };
    if repr.len() < HEADER_LEN || &repr[..4] != MAGIC {
        return pithos_native_v2::decode_exact_dedup(payload, expected_len);
    }
    if read_u16(&repr, 4)? != 3 || read_u64(&repr, 8)? != expected_len {
        return Err(PithosError::InvalidMetadata("native v3 header"));
    }
    let sequence_count = read_u32(&repr, 16)? as usize;
    let canonical_count = read_u32(&repr, 20)? as usize;
    let mut pos = HEADER_LEN;
    let mut sequence = Vec::with_capacity(sequence_count);
    for _ in 0..sequence_count {
        sequence.push(read_u32(&repr, pos)?);
        pos += 4;
    }
    let mut canonical = Vec::<Vec<u8>>::with_capacity(canonical_count);
    for _ in 0..canonical_count {
        let len = read_u32(&repr, pos)? as usize;
        pos += 4;
        let token_count = read_u32(&repr, pos)? as usize;
        pos += 4;
        let mut chunk = Vec::with_capacity(len);
        for _ in 0..token_count {
            let kind = *repr.get(pos).ok_or(PithosError::InvalidRange)?;
            pos += 1;
            match kind {
                0 => {
                    let literal_len = read_u32(&repr, pos)? as usize;
                    pos += 4;
                    let end = pos
                        .checked_add(literal_len)
                        .ok_or(PithosError::IntegerOverflow)?;
                    chunk.extend_from_slice(repr.get(pos..end).ok_or(PithosError::InvalidRange)?);
                    pos = end;
                }
                1 => {
                    let base = read_u32(&repr, pos)? as usize;
                    pos += 4;
                    let offset = read_u32(&repr, pos)? as usize;
                    pos += 4;
                    let copy_len = read_u32(&repr, pos)? as usize;
                    pos += 4;
                    let source = canonical
                        .get(base)
                        .ok_or(PithosError::InvalidMetadata("reference base"))?;
                    let end = offset
                        .checked_add(copy_len)
                        .ok_or(PithosError::IntegerOverflow)?;
                    chunk.extend_from_slice(
                        source.get(offset..end).ok_or(PithosError::InvalidRange)?,
                    );
                }
                _ => return Err(PithosError::InvalidMetadata("reference token")),
            }
            if chunk.len() > len {
                return Err(PithosError::ResourceLimit("reference chunk output"));
            }
        }
        if chunk.len() != len {
            return Err(PithosError::InvalidRange);
        }
        canonical.push(chunk);
    }
    if pos != repr.len() {
        return Err(PithosError::InvalidMetadata("native trailing bytes"));
    }
    let mut out = Vec::with_capacity(expected_len as usize);
    for id in sequence {
        out.extend_from_slice(
            canonical
                .get(id as usize)
                .ok_or(PithosError::InvalidMetadata("native reference"))?,
        );
        if out.len() > expected_len as usize {
            return Err(PithosError::ResourceLimit("native decoded output"));
        }
    }
    if out.len() as u64 != expected_len {
        return Err(PithosError::InvalidRange);
    }
    Ok(out)
}

fn canonical_chunks(input: &[u8], member_lengths: &[u64]) -> Result<(Vec<Vec<u8>>, Vec<u32>, u64)> {
    let cfg = ChunkingConfig::default();
    let mut canonical = Vec::<Vec<u8>>::new();
    let mut by_hash = HashMap::<[u8; 32], Vec<u32>>::new();
    let mut sequence = Vec::new();
    let mut duplicate = 0_u64;
    let mut base = 0_u64;
    for (member_id, len) in member_lengths.iter().copied().enumerate() {
        let start = base as usize;
        let end = start
            .checked_add(len as usize)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(start..end).ok_or(PithosError::InvalidRange)?;
        for draft in chunk_fastcdc(
            member,
            ChunkOrigin {
                entry_id: member_id as u64,
                object_id: 0,
                base_offset: base,
            },
            &cfg,
        )? {
            let start = draft.logical_offset as usize;
            let end = start
                .checked_add(draft.length as usize)
                .ok_or(PithosError::IntegerOverflow)?;
            let bytes = input.get(start..end).ok_or(PithosError::InvalidRange)?;
            let hash = *blake3::hash(bytes).as_bytes();
            let found = by_hash.get(&hash).and_then(|ids| {
                ids.iter()
                    .copied()
                    .find(|id| canonical[*id as usize].as_slice() == bytes)
            });
            let id = match found {
                Some(id) => {
                    duplicate = duplicate
                        .checked_add(bytes.len() as u64)
                        .ok_or(PithosError::IntegerOverflow)?;
                    id
                }
                None => {
                    let id = canonical.len() as u32;
                    canonical.push(bytes.to_vec());
                    by_hash.entry(hash).or_default().push(id);
                    id
                }
            };
            sequence.push(id);
        }
        base = base.checked_add(len).ok_or(PithosError::IntegerOverflow)?;
    }
    Ok((canonical, sequence, duplicate))
}

fn reference_tokens(
    bytes: &[u8],
    previous: &[Vec<u8>],
    index: &HashMap<u64, Vec<(u32, u32)>>,
) -> Result<Vec<Token>> {
    if bytes.len() < MIN_COPY || previous.is_empty() {
        return Ok(vec![Token::Literal(bytes.to_vec())]);
    }
    let mut tokens = Vec::new();
    let mut literal = Vec::new();
    let mut pos = 0_usize;
    while pos < bytes.len() {
        let mut best: Option<(u32, u32, usize)> = None;
        if pos + ANCHOR <= bytes.len() {
            let key = xxh3_64(&bytes[pos..pos + ANCHOR]);
            if let Some(candidates) = index.get(&key) {
                for &(base_id, offset) in candidates.iter().rev().take(MAX_ANCHOR_CANDIDATES) {
                    let source = &previous[base_id as usize];
                    let mut len = 0_usize;
                    let source_offset = offset as usize;
                    while pos + len < bytes.len()
                        && source_offset + len < source.len()
                        && bytes[pos + len] == source[source_offset + len]
                    {
                        len += 1;
                    }
                    if len >= MIN_COPY && best.is_none_or(|(_, _, best_len)| len > best_len) {
                        best = Some((base_id, offset, len));
                    }
                }
            }
        }
        if let Some((base, offset, len)) = best {
            if !literal.is_empty() {
                tokens.push(Token::Literal(std::mem::take(&mut literal)));
            }
            tokens.push(Token::Copy {
                base,
                offset,
                len: len as u32,
            });
            pos += len;
        } else {
            literal.push(bytes[pos]);
            pos += 1;
        }
    }
    if !literal.is_empty() {
        tokens.push(Token::Literal(literal));
    }
    let encoded_cost = tokens
        .iter()
        .map(|t| match t {
            Token::Literal(v) => 5 + v.len(),
            Token::Copy { .. } => 13,
        })
        .sum::<usize>()
        + 8;
    if encoded_cost >= bytes.len() + 8 {
        Ok(vec![Token::Literal(bytes.to_vec())])
    } else {
        Ok(tokens)
    }
}

fn index_anchors(id: u32, bytes: &[u8], index: &mut HashMap<u64, Vec<(u32, u32)>>) {
    if bytes.len() < ANCHOR {
        return;
    }
    for offset in (0..=bytes.len() - ANCHOR).step_by(8) {
        let key = xxh3_64(&bytes[offset..offset + ANCHOR]);
        let bucket = index.entry(key).or_default();
        bucket.push((id, offset as u32));
        if bucket.len() > MAX_ANCHOR_CANDIDATES * 2 {
            bucket.remove(0);
        }
    }
}

fn encode_record(record: CanonicalRecord, out: &mut Vec<u8>) -> Result<()> {
    out.extend_from_slice(&record.len.to_le_bytes());
    out.extend_from_slice(&(record.tokens.len() as u32).to_le_bytes());
    for token in record.tokens {
        match token {
            Token::Literal(bytes) => {
                out.push(0);
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(&bytes);
            }
            Token::Copy { base, offset, len } => {
                out.push(1);
                out.extend_from_slice(&base.to_le_bytes());
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
            }
        }
    }
    Ok(())
}

fn validate_members(input: &[u8], members: &[u64]) -> Result<()> {
    let total = members.iter().try_fold(0u64, |a, b| {
        a.checked_add(*b).ok_or(PithosError::IntegerOverflow)
    })?;
    if total != input.len() as u64 {
        Err(PithosError::InvalidMetadata("native member boundaries"))
    } else {
        Ok(())
    }
}
fn read_u16(b: &[u8], o: usize) -> Result<u16> {
    let s = b.get(o..o + 2).ok_or(PithosError::InvalidRange)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}
fn read_u32(b: &[u8], o: usize) -> Result<u32> {
    let s = b.get(o..o + 4).ok_or(PithosError::InvalidRange)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn read_u64(b: &[u8], o: usize) -> Result<u64> {
    let s = b.get(o..o + 8).ok_or(PithosError::InvalidRange)?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn moved_regions_roundtrip() {
        let base = (0..200_000).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let mut changed = vec![7u8; 1000];
        changed.extend_from_slice(&base[20_000..180_000]);
        changed.extend_from_slice(&[8u8; 2000]);
        let mut input = base.clone();
        input.extend_from_slice(&changed);
        let (e, _) =
            encode_exact_dedup(&input, &[base.len() as u64, changed.len() as u64], 5).unwrap();
        assert_eq!(decode_exact_dedup(&e, input.len() as u64).unwrap(), input);
    }
}
