//! Native codec v11: parallel clustered evaluation.
//!
//! The payload syntax is intentionally the same PCL0 envelope introduced by
//! v10. This revision changes only execution: the global v9 candidate and the
//! clustered candidate are evaluated concurrently, and independent clusters
//! are encoded through Rayon. The smaller physical payload is retained.

use pithos_core::{PithosError, Result};
use rayon::prelude::*;
use std::collections::BTreeMap;

pub const NATIVE_CODEC_ID: u16 = pithos_native_v10::NATIVE_CODEC_ID;
pub const NATIVE_CODEC_VERSION: u16 = pithos_native_v10::NATIVE_CODEC_VERSION;
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

#[derive(Debug, Clone, Copy)]
struct Member<'a> {
    index: u32,
    bytes: &'a [u8],
}

#[derive(Debug)]
struct Cluster<'a> {
    class: Class,
    members: Vec<Member<'a>>,
}

#[derive(Debug)]
struct EncodedCluster {
    class: Class,
    members: Vec<(u32, u64)>,
    cluster_len: u64,
    payload: Vec<u8>,
    stats: NativeStats,
}

pub fn encode_exact_dedup(
    input: &[u8],
    member_lengths: &[u64],
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    let members = split_members(input, member_lengths)?;
    let clusters = make_clusters(members);
    if clusters.len() <= 1 || clusters.len() > MAX_CLUSTERS {
        let (payload, stats) = pithos_native_v9::encode_exact_dedup(input, member_lengths, level)?;
        return Ok((payload, from_v9(stats)));
    }

    let (baseline_result, clustered_result) = rayon::join(
        || pithos_native_v9::encode_exact_dedup(input, member_lengths, level),
        || encode_clustered(input.len() as u64, clusters, level),
    );
    let (baseline, baseline_stats) = baseline_result?;
    let (clustered, clustered_stats) = clustered_result?;
    if clustered.len() < baseline.len() {
        Ok((clustered, clustered_stats))
    } else {
        Ok((baseline, from_v9(baseline_stats)))
    }
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    pithos_native_v10::decode_exact_dedup(payload, expected_len)
}

fn encode_clustered(
    original_len: u64,
    clusters: Vec<Cluster<'_>>,
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    let encoded = clusters
        .par_iter()
        .map(|cluster| encode_cluster(cluster, level))
        .collect::<Result<Vec<_>>>()?;

    let member_count = encoded.iter().try_fold(0_usize, |total, cluster| {
        total
            .checked_add(cluster.members.len())
            .ok_or(PithosError::IntegerOverflow)
    })?;
    let mut output = Vec::new();
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&10_u16.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&original_len.to_le_bytes());
    output.extend_from_slice(
        &u32::try_from(member_count)
            .map_err(|_| PithosError::IntegerOverflow)?
            .to_le_bytes(),
    );
    output.extend_from_slice(
        &u32::try_from(encoded.len())
            .map_err(|_| PithosError::IntegerOverflow)?
            .to_le_bytes(),
    );

    let mut stats = NativeStats {
        chunk_count: 0,
        canonical_chunks: 0,
        gross_duplicate_bytes: 0,
        representation_bytes: 0,
        encoded_bytes: 0,
    };
    for cluster in encoded {
        output.push(cluster.class as u8);
        output.extend_from_slice(
            &u32::try_from(cluster.members.len())
                .map_err(|_| PithosError::IntegerOverflow)?
                .to_le_bytes(),
        );
        output.extend_from_slice(&cluster.cluster_len.to_le_bytes());
        output.extend_from_slice(&(cluster.payload.len() as u64).to_le_bytes());
        for (index, length) in cluster.members {
            output.extend_from_slice(&index.to_le_bytes());
            output.extend_from_slice(&length.to_le_bytes());
        }
        output.extend_from_slice(&cluster.payload);
        stats.chunk_count = stats.chunk_count.saturating_add(cluster.stats.chunk_count);
        stats.canonical_chunks = stats
            .canonical_chunks
            .saturating_add(cluster.stats.canonical_chunks);
        stats.gross_duplicate_bytes = stats
            .gross_duplicate_bytes
            .saturating_add(cluster.stats.gross_duplicate_bytes);
        stats.representation_bytes = stats
            .representation_bytes
            .saturating_add(cluster.stats.representation_bytes);
        stats.encoded_bytes = stats
            .encoded_bytes
            .saturating_add(cluster.stats.encoded_bytes);
    }
    let nested_encoded_bytes = stats.encoded_bytes;
    let envelope_overhead = (output.len() as u64).saturating_sub(nested_encoded_bytes);
    stats.representation_bytes = stats.representation_bytes.saturating_add(envelope_overhead);
    stats.encoded_bytes = output.len() as u64;
    Ok((output, stats))
}

fn encode_cluster(cluster: &Cluster<'_>, level: i32) -> Result<EncodedCluster> {
    let mut bytes = Vec::new();
    let mut lengths = Vec::with_capacity(cluster.members.len());
    let mut member_meta = Vec::with_capacity(cluster.members.len());
    for member in &cluster.members {
        bytes.extend_from_slice(member.bytes);
        let length = member.bytes.len() as u64;
        lengths.push(length);
        member_meta.push((member.index, length));
    }
    let (payload, stats) = pithos_native_v9::encode_exact_dedup(&bytes, &lengths, level)?;
    Ok(EncodedCluster {
        class: cluster.class,
        members: member_meta,
        cluster_len: bytes.len() as u64,
        payload,
        stats: from_v9(stats),
    })
}

fn make_clusters(members: Vec<Member<'_>>) -> Vec<Cluster<'_>> {
    let mut grouped = BTreeMap::<Class, Vec<Member<'_>>>::new();
    for member in members {
        grouped.entry(classify(member.bytes)).or_default().push(member);
    }
    grouped
        .into_iter()
        .map(|(class, members)| Cluster { class, members })
        .collect()
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
        let first = text
            .as_bytes()
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace());
        if matches!(first, Some(b'{') | Some(b'[') | Some(b'<')) {
            return Class::StructuredText;
        }
        let sample = &text.as_bytes()[..text.len().min(256 * 1024)];
        let printable = sample
            .iter()
            .filter(|byte| byte.is_ascii_graphic() || byte.is_ascii_whitespace())
            .count();
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parallel_cluster_payload_roundtrips_v10_decoder() {
        let text = b"row,value\r\n1,alpha\r\n2,beta\r\n".repeat(20_000);
        let imageish = [b"BM".as_slice(), &vec![0_u8; 800_000]].concat();
        let mut input = imageish.clone();
        input.extend_from_slice(&text);
        let lengths = [imageish.len() as u64, text.len() as u64];
        let (payload, _) = encode_exact_dedup(&input, &lengths, 5).unwrap();
        assert_eq!(decode_exact_dedup(&payload, input.len() as u64).unwrap(), input);
    }
}
