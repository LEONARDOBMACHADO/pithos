//! Native codec v10: content-class clustering and reversible member reordering.
//!
//! A PAF solid group may contain unrelated formats. v10 groups logical members
//! by a deterministic content class, encodes each cluster independently with
//! the fused v9 selector, and stores the original member indexes so decode
//! restores the exact original order. The clustered envelope is used only when
//! it is physically smaller than the unclustered v9 payload.

use pithos_core::{PithosError, Result};
use std::collections::BTreeMap;

pub const NATIVE_CODEC_ID: u16 = pithos_native_v9::NATIVE_CODEC_ID;
pub const NATIVE_CODEC_VERSION: u16 = pithos_native_v9::NATIVE_CODEC_VERSION;
const MAGIC: &[u8; 4] = b"PCL0";
const HEADER_LEN: usize = 24;
const MAX_CLUSTERS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum Class {
    StructuredText = 0,
    Text = 1,
    Archive = 2,
    Image = 3,
    Audio = 4,
    Video = 5,
    Database = 6,
    Binary = 7,
}

#[derive(Debug)]
struct Member<'a> {
    index: u32,
    bytes: &'a [u8],
}

pub fn encode_exact_dedup(
    input: &[u8],
    member_lengths: &[u64],
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    let (baseline, baseline_stats) =
        pithos_native_v9::encode_exact_dedup(input, member_lengths, level)?;
    let members = split_members(input, member_lengths)?;
    if members.len() < 2 {
        return Ok((baseline, from_v9(baseline_stats)));
    }

    let mut clusters = BTreeMap::<Class, Vec<Member<'_>>>::new();
    for member in members {
        clusters.entry(classify(member.bytes)).or_default().push(member);
    }
    if clusters.len() <= 1 || clusters.len() > MAX_CLUSTERS {
        return Ok((baseline, from_v9(baseline_stats)));
    }

    let mut encoded_clusters = Vec::with_capacity(clusters.len());
    let mut total_stats = NativeStats {
        chunk_count: 0,
        canonical_chunks: 0,
        gross_duplicate_bytes: 0,
        representation_bytes: 0,
        encoded_bytes: 0,
    };
    for (class, members) in clusters {
        let mut cluster_bytes = Vec::new();
        let mut lengths = Vec::with_capacity(members.len());
        for member in &members {
            cluster_bytes.extend_from_slice(member.bytes);
            lengths.push(member.bytes.len() as u64);
        }
        let (payload, stats) =
            pithos_native_v9::encode_exact_dedup(&cluster_bytes, &lengths, level)?;
        total_stats.chunk_count = total_stats.chunk_count.saturating_add(stats.chunk_count);
        total_stats.canonical_chunks = total_stats.canonical_chunks.saturating_add(stats.canonical_chunks);
        total_stats.gross_duplicate_bytes = total_stats.gross_duplicate_bytes.saturating_add(stats.gross_duplicate_bytes);
        total_stats.representation_bytes = total_stats.representation_bytes.saturating_add(stats.representation_bytes);
        total_stats.encoded_bytes = total_stats.encoded_bytes.saturating_add(stats.encoded_bytes);
        encoded_clusters.push((class, members, cluster_bytes.len() as u64, payload));
    }

    let mut candidate = Vec::new();
    candidate.extend_from_slice(MAGIC);
    candidate.extend_from_slice(&10_u16.to_le_bytes());
    candidate.extend_from_slice(&0_u16.to_le_bytes());
    candidate.extend_from_slice(&(input.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&(member_lengths.len() as u32).to_le_bytes());
    candidate.extend_from_slice(&(encoded_clusters.len() as u32).to_le_bytes());
    for (class, members, cluster_len, payload) in encoded_clusters {
        candidate.push(class as u8);
        candidate.extend_from_slice(&(members.len() as u32).to_le_bytes());
        candidate.extend_from_slice(&cluster_len.to_le_bytes());
        candidate.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        for member in members {
            candidate.extend_from_slice(&member.index.to_le_bytes());
            candidate.extend_from_slice(&(member.bytes.len() as u64).to_le_bytes());
        }
        candidate.extend_from_slice(&payload);
    }

    if candidate.len() >= baseline.len() {
        return Ok((baseline, from_v9(baseline_stats)));
    }
    let nested_encoded_bytes = total_stats.encoded_bytes;
    let envelope_overhead = (candidate.len() as u64).saturating_sub(nested_encoded_bytes);
    total_stats.representation_bytes = total_stats.representation_bytes.saturating_add(envelope_overhead);
    total_stats.encoded_bytes = candidate.len() as u64;
    Ok((candidate, total_stats))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    if payload.len() < HEADER_LEN || &payload[..4] != MAGIC {
        return pithos_native_v9::decode_exact_dedup(payload, expected_len);
    }
    if read_u16(payload, 4)? != 10 || read_u64(payload, 8)? != expected_len {
        return Err(PithosError::InvalidMetadata("cluster header"));
    }
    let member_count = read_u32(payload, 16)? as usize;
    let cluster_count = read_u32(payload, 20)? as usize;
    if cluster_count == 0 || cluster_count > MAX_CLUSTERS {
        return Err(PithosError::InvalidMetadata("cluster count"));
    }
    let mut restored = vec![None::<Vec<u8>>; member_count];
    let mut position = HEADER_LEN;
    for _ in 0..cluster_count {
        let class = *payload.get(position).ok_or(PithosError::InvalidRange)?;
        if class > Class::Binary as u8 {
            return Err(PithosError::InvalidMetadata("cluster class"));
        }
        position += 1;
        let count = read_u32(payload, position)? as usize;
        position += 4;
        let cluster_len = read_u64(payload, position)?;
        position += 8;
        let payload_len = read_u64(payload, position)? as usize;
        position += 8;
        if count == 0 || count > member_count {
            return Err(PithosError::InvalidMetadata("cluster member count"));
        }
        let mut members = Vec::with_capacity(count);
        let mut declared_cluster_len = 0_u64;
        for _ in 0..count {
            let index = read_u32(payload, position)? as usize;
            position += 4;
            let length = read_u64(payload, position)?;
            position += 8;
            declared_cluster_len = declared_cluster_len.checked_add(length).ok_or(PithosError::IntegerOverflow)?;
            members.push((index, length));
        }
        if declared_cluster_len != cluster_len {
            return Err(PithosError::InvalidMetadata("cluster length"));
        }
        let end = position.checked_add(payload_len).ok_or(PithosError::IntegerOverflow)?;
        let encoded = payload.get(position..end).ok_or(PithosError::InvalidRange)?;
        let decoded = pithos_native_v9::decode_exact_dedup(encoded, cluster_len)?;
        position = end;
        let mut cursor = 0_usize;
        for (index, length) in members {
            if index >= member_count || restored[index].is_some() {
                return Err(PithosError::InvalidMetadata("cluster member index"));
            }
            let length = usize::try_from(length).map_err(|_| PithosError::IntegerOverflow)?;
            let member_end = cursor.checked_add(length).ok_or(PithosError::IntegerOverflow)?;
            restored[index] = Some(decoded.get(cursor..member_end).ok_or(PithosError::InvalidRange)?.to_vec());
            cursor = member_end;
        }
        if cursor != decoded.len() {
            return Err(PithosError::InvalidRange);
        }
    }
    if position != payload.len() {
        return Err(PithosError::InvalidMetadata("cluster trailing bytes"));
    }
    let mut output = Vec::with_capacity(usize::try_from(expected_len).map_err(|_| PithosError::MemoryLimit)?);
    for member in restored {
        let member = member.ok_or(PithosError::InvalidMetadata("missing clustered member"))?;
        output.extend_from_slice(&member);
        if output.len() as u64 > expected_len {
            return Err(PithosError::ResourceLimit("cluster output"));
        }
    }
    if output.len() as u64 != expected_len {
        return Err(PithosError::InvalidRange);
    }
    Ok(output)
}

fn split_members<'a>(input: &'a [u8], lengths: &[u64]) -> Result<Vec<Member<'a>>> {
    let mut members = Vec::with_capacity(lengths.len());
    let mut offset = 0_usize;
    for (index, length) in lengths.iter().enumerate() {
        let length = usize::try_from(*length).map_err(|_| PithosError::IntegerOverflow)?;
        let end = offset.checked_add(length).ok_or(PithosError::IntegerOverflow)?;
        members.push(Member {
            index: u32::try_from(index).map_err(|_| PithosError::IntegerOverflow)?,
            bytes: input.get(offset..end).ok_or(PithosError::InvalidRange)?,
        });
        offset = end;
    }
    if offset != input.len() {
        return Err(PithosError::InvalidMetadata("cluster member boundaries"));
    }
    Ok(members)
}

fn classify(bytes: &[u8]) -> Class {
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(&[0x1f, 0x8b])
        || bytes.starts_with(b"7z\xbc\xaf\x27\x1c")
        || bytes.starts_with(b"Rar!")
    {
        return Class::Archive;
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"BM")
        || bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP")
    {
        return Class::Image;
    }
    if bytes.starts_with(b"fLaC")
        || bytes.starts_with(b"ID3")
        || bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE")
    {
        return Class::Audio;
    }
    if bytes.get(4..8) == Some(b"ftyp") || bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Class::Video;
    }
    if bytes.starts_with(b"SQLite format 3\0") {
        return Class::Database;
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        let first = text.as_bytes().iter().copied().find(|byte| !byte.is_ascii_whitespace());
        if matches!(first, Some(b'{') | Some(b'[') | Some(b'<')) {
            return Class::StructuredText;
        }
        let sample = &text.as_bytes()[..text.len().min(256 * 1024)];
        let printable = sample.iter().filter(|byte| byte.is_ascii_graphic() || byte.is_ascii_whitespace()).count();
        if sample.is_empty() || printable.saturating_mul(100) >= sample.len().saturating_mul(90) {
            return Class::Text;
        }
    }
    Class::Binary
}

fn from_v9(stats: pithos_native_v9::NativeStats) -> NativeStats {
    NativeStats {
        chunk_count: stats.chunk_count,
        canonical_chunks: stats.canonical_chunks,
        gross_duplicate_bytes: stats.gross_duplicate_bytes,
        representation_bytes: stats.representation_bytes,
        encoded_bytes: stats.encoded_bytes,
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let data = bytes.get(offset..offset + 2).ok_or(PithosError::InvalidRange)?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let data = bytes.get(offset..offset + 4).ok_or(PithosError::InvalidRange)?;
    Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let data = bytes.get(offset..offset + 8).ok_or(PithosError::InvalidRange)?;
    Ok(u64::from_le_bytes([data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clustered_members_restore_original_order() {
        let text = b"{ \"a\": 1, \"b\": 2 }\n".repeat(20_000);
        let binary = (0..700_000).map(|index| ((index * 181 + 7) % 256) as u8).collect::<Vec<_>>();
        let mut input = binary.clone();
        input.extend_from_slice(&text);
        let lengths = [binary.len() as u64, text.len() as u64];
        let (encoded, _) = encode_exact_dedup(&input, &lengths, 5).unwrap();
        assert_eq!(decode_exact_dedup(&encoded, input.len() as u64).unwrap(), input);
    }
}
