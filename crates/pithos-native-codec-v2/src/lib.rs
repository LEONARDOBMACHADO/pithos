//! Native codec v2: exact dedup plus bounded similarity deltas.

use pithos_analysis::{ChunkOrigin, ChunkingConfig, chunk_fastcdc};
use pithos_core::{PithosError, Result};
use std::collections::HashMap;
use std::io::Cursor;

pub const NATIVE_CODEC_ID: u16 = pithos_native_v1::NATIVE_CODEC_ID;
pub const NATIVE_CODEC_VERSION: u16 = pithos_native_v1::NATIVE_CODEC_VERSION;
const MAGIC: &[u8; 4] = b"PNT2";
const HEADER_LEN: usize = 24;
const SEARCH_BACK: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}

enum Record {
    Literal(Vec<u8>),
    Xor { base: u32, len: u32, patches: Vec<(u32, u8)> },
    Splice { base: u32, len: u32, prefix: u32, suffix: u32, middle: Vec<u8> },
}

pub fn encode_exact_dedup(input: &[u8], member_lengths: &[u64], level: i32) -> Result<(Vec<u8>, NativeStats)> {
    if member_lengths.iter().try_fold(0_u64, |a, b| a.checked_add(*b).ok_or(PithosError::IntegerOverflow))? != input.len() as u64 {
        return Err(PithosError::InvalidMetadata("native member boundaries"));
    }
    let cfg = ChunkingConfig::default();
    let mut canonical = Vec::<Vec<u8>>::new();
    let mut by_hash = HashMap::<[u8; 32], Vec<u32>>::new();
    let mut sequence = Vec::<u32>::new();
    let mut dup = 0_u64;
    let mut base = 0_u64;
    for (member_id, len) in member_lengths.iter().copied().enumerate() {
        let start = base as usize;
        let end = start.checked_add(len as usize).ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(start..end).ok_or(PithosError::InvalidRange)?;
        for draft in chunk_fastcdc(member, ChunkOrigin { entry_id: member_id as u64, object_id: 0, base_offset: base }, &cfg)? {
            let s = draft.logical_offset as usize;
            let e = s.checked_add(draft.length as usize).ok_or(PithosError::IntegerOverflow)?;
            let bytes = input.get(s..e).ok_or(PithosError::InvalidRange)?;
            let hash = *blake3::hash(bytes).as_bytes();
            let existing = by_hash.get(&hash).and_then(|ids| ids.iter().copied().find(|id| canonical[*id as usize].as_slice() == bytes));
            let id = if let Some(id) = existing {
                dup = dup.checked_add(bytes.len() as u64).ok_or(PithosError::IntegerOverflow)?;
                id
            } else {
                let id = u32::try_from(canonical.len()).map_err(|_| PithosError::ResourceLimit("native canonical chunks"))?;
                canonical.push(bytes.to_vec());
                by_hash.entry(hash).or_default().push(id);
                id
            };
            sequence.push(id);
        }
        base = base.checked_add(len).ok_or(PithosError::IntegerOverflow)?;
    }

    let mut records = Vec::with_capacity(canonical.len());
    for (index, bytes) in canonical.iter().enumerate() {
        records.push(best_record(index, bytes, &canonical)?);
    }
    let mut repr = Vec::new();
    repr.extend_from_slice(MAGIC);
    repr.extend_from_slice(&2_u16.to_le_bytes());
    repr.extend_from_slice(&0_u16.to_le_bytes());
    repr.extend_from_slice(&(input.len() as u64).to_le_bytes());
    repr.extend_from_slice(&(sequence.len() as u32).to_le_bytes());
    repr.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for id in &sequence { repr.extend_from_slice(&id.to_le_bytes()); }
    for record in records { encode_record(record, &mut repr)?; }
    let encoded = zstd::stream::encode_all(Cursor::new(&repr), level)?;
    Ok((encoded, NativeStats {
        chunk_count: sequence.len() as u32,
        canonical_chunks: canonical.len() as u32,
        gross_duplicate_bytes: dup,
        representation_bytes: repr.len() as u64,
        encoded_bytes: encoded.len() as u64,
    }))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    let Ok(repr) = zstd::stream::decode_all(Cursor::new(payload)) else {
        return pithos_native_v1::decode_exact_dedup(payload, expected_len);
    };
    if repr.len() < HEADER_LEN || &repr[..4] != MAGIC {
        return pithos_native_v1::decode_exact_dedup(payload, expected_len);
    }
    if read_u16(&repr, 4)? != 2 || read_u64(&repr, 8)? != expected_len {
        return Err(PithosError::InvalidMetadata("native v2 header"));
    }
    let seq_count = read_u32(&repr, 16)? as usize;
    let canon_count = read_u32(&repr, 20)? as usize;
    let mut pos = HEADER_LEN;
    let mut seq = Vec::with_capacity(seq_count);
    for _ in 0..seq_count { seq.push(read_u32(&repr, pos)?); pos += 4; }
    let mut canonical = Vec::<Vec<u8>>::with_capacity(canon_count);
    for _ in 0..canon_count { canonical.push(decode_record(&repr, &mut pos, &canonical)?); }
    if pos != repr.len() { return Err(PithosError::InvalidMetadata("native trailing bytes")); }
    let mut out = Vec::with_capacity(expected_len as usize);
    for id in seq {
        let bytes = canonical.get(id as usize).ok_or(PithosError::InvalidMetadata("native reference"))?;
        out.extend_from_slice(bytes);
        if out.len() > expected_len as usize { return Err(PithosError::ResourceLimit("native decoded output")); }
    }
    if out.len() as u64 != expected_len { return Err(PithosError::InvalidRange); }
    Ok(out)
}

fn best_record(index: usize, bytes: &[u8], canonical: &[Vec<u8>]) -> Result<Record> {
    let mut best_size = 5 + bytes.len();
    let mut best = Record::Literal(bytes.to_vec());
    let start = index.saturating_sub(SEARCH_BACK);
    for base_index in start..index {
        let base = &canonical[base_index];
        if base.len() == bytes.len() {
            let mut patches = Vec::new();
            for (offset, (&a, &b)) in base.iter().zip(bytes).enumerate() {
                if a != b { patches.push((offset as u32, b)); }
            }
            let size = 1 + 4 + 4 + 4 + patches.len() * 5;
            if !patches.is_empty() && size < best_size {
                best_size = size;
                best = Record::Xor { base: base_index as u32, len: bytes.len() as u32, patches };
            }
        }
        let prefix = common_prefix(base, bytes);
        let suffix = common_suffix(&base[prefix..], &bytes[prefix..]);
        if prefix + suffix <= bytes.len() {
            let middle_end = bytes.len() - suffix;
            let middle = bytes[prefix..middle_end].to_vec();
            let size = 1 + 4 * 5 + middle.len();
            if size < best_size {
                best_size = size;
                best = Record::Splice {
                    base: base_index as u32, len: bytes.len() as u32,
                    prefix: prefix as u32, suffix: suffix as u32, middle,
                };
            }
        }
    }
    let _ = best_size;
    Ok(best)
}

fn encode_record(record: Record, out: &mut Vec<u8>) -> Result<()> {
    match record {
        Record::Literal(bytes) => { out.push(0); out.extend_from_slice(&(bytes.len() as u32).to_le_bytes()); out.extend_from_slice(&bytes); }
        Record::Xor { base, len, patches } => {
            out.push(1); out.extend_from_slice(&len.to_le_bytes()); out.extend_from_slice(&base.to_le_bytes()); out.extend_from_slice(&(patches.len() as u32).to_le_bytes());
            for (offset, value) in patches { out.extend_from_slice(&offset.to_le_bytes()); out.push(value); }
        }
        Record::Splice { base, len, prefix, suffix, middle } => {
            out.push(2); out.extend_from_slice(&len.to_le_bytes()); out.extend_from_slice(&base.to_le_bytes()); out.extend_from_slice(&prefix.to_le_bytes()); out.extend_from_slice(&suffix.to_le_bytes()); out.extend_from_slice(&(middle.len() as u32).to_le_bytes()); out.extend_from_slice(&middle);
        }
    }
    Ok(())
}

fn decode_record(data: &[u8], pos: &mut usize, canonical: &[Vec<u8>]) -> Result<Vec<u8>> {
    let kind = *data.get(*pos).ok_or(PithosError::InvalidRange)?; *pos += 1;
    let len = read_u32(data, *pos)? as usize; *pos += 4;
    match kind {
        0 => { let end = pos.checked_add(len).ok_or(PithosError::IntegerOverflow)?; let out = data.get(*pos..end).ok_or(PithosError::InvalidRange)?.to_vec(); *pos = end; Ok(out) }
        1 => {
            let base_id = read_u32(data, *pos)? as usize; *pos += 4;
            let count = read_u32(data, *pos)? as usize; *pos += 4;
            let mut out = canonical.get(base_id).ok_or(PithosError::InvalidMetadata("delta base"))?.clone();
            if out.len() != len { return Err(PithosError::InvalidMetadata("delta length")); }
            for _ in 0..count { let off = read_u32(data, *pos)? as usize; *pos += 4; let value = *data.get(*pos).ok_or(PithosError::InvalidRange)?; *pos += 1; *out.get_mut(off).ok_or(PithosError::InvalidRange)? = value; }
            Ok(out)
        }
        2 => {
            let base_id = read_u32(data, *pos)? as usize; *pos += 4;
            let prefix = read_u32(data, *pos)? as usize; *pos += 4;
            let suffix = read_u32(data, *pos)? as usize; *pos += 4;
            let middle_len = read_u32(data, *pos)? as usize; *pos += 4;
            let end = pos.checked_add(middle_len).ok_or(PithosError::IntegerOverflow)?;
            let middle = data.get(*pos..end).ok_or(PithosError::InvalidRange)?; *pos = end;
            let base = canonical.get(base_id).ok_or(PithosError::InvalidMetadata("delta base"))?;
            if prefix + suffix > base.len() || prefix + middle_len + suffix != len { return Err(PithosError::InvalidMetadata("splice layout")); }
            let mut out = Vec::with_capacity(len); out.extend_from_slice(&base[..prefix]); out.extend_from_slice(middle); out.extend_from_slice(&base[base.len()-suffix..]); Ok(out)
        }
        _ => Err(PithosError::InvalidMetadata("native record kind")),
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize { a.iter().zip(b).take_while(|(x,y)| x == y).count() }
fn common_suffix(a: &[u8], b: &[u8]) -> usize { a.iter().rev().zip(b.iter().rev()).take_while(|(x,y)| x == y).count() }
fn read_u16(b:&[u8],o:usize)->Result<u16>{let s=b.get(o..o+2).ok_or(PithosError::InvalidRange)?;Ok(u16::from_le_bytes([s[0],s[1]]))}
fn read_u32(b:&[u8],o:usize)->Result<u32>{let s=b.get(o..o+4).ok_or(PithosError::InvalidRange)?;Ok(u32::from_le_bytes([s[0],s[1],s[2],s[3]]))}
fn read_u64(b:&[u8],o:usize)->Result<u64>{let s=b.get(o..o+8).ok_or(PithosError::InvalidRange)?;Ok(u64::from_le_bytes([s[0],s[1],s[2],s[3],s[4],s[5],s[6],s[7]]))}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_delta_roundtrips() {
        let mut a = vec![b'A'; 1024 * 1024];
        let mut b = a.clone(); b[500_000] = b'B'; b[700_000] = b'C';
        a.extend_from_slice(&b);
        let (encoded, _) = encode_exact_dedup(&a, &[1024*1024,1024*1024], 5).unwrap();
        assert_eq!(decode_exact_dedup(&encoded, a.len() as u64).unwrap(), a);
    }
}
