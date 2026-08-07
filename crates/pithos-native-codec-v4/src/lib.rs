//! Native codec v4: v3 reference graph plus reversible text canonicalization.

use pithos_core::{PithosError, Result};
use std::io::Cursor;

pub const NATIVE_CODEC_ID: u16 = pithos_native_v3::NATIVE_CODEC_ID;
pub const NATIVE_CODEC_VERSION: u16 = pithos_native_v3::NATIVE_CODEC_VERSION;
const MAGIC: &[u8; 4] = b"PNC4";
const HEADER_LEN: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}

#[derive(Debug)]
struct Event { offset: u32, bytes: Vec<u8> }
#[derive(Debug)]
struct MemberMeta { original_len: u64, normalized_len: u64, events: Vec<Event> }

pub fn encode_exact_dedup(input: &[u8], member_lengths: &[u64], level: i32) -> Result<(Vec<u8>, NativeStats)> {
    let (baseline, base_stats) = pithos_native_v3::encode_exact_dedup(input, member_lengths, level)?;
    let baseline_len = baseline.len() as u64;
    let mut normalized = Vec::new();
    let mut normalized_lengths = Vec::with_capacity(member_lengths.len());
    let mut metas = Vec::with_capacity(member_lengths.len());
    let mut base = 0_usize;
    let mut changed = false;
    for len in member_lengths {
        let end = base.checked_add(*len as usize).ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(base..end).ok_or(PithosError::InvalidRange)?;
        let (bytes, events) = canonicalize_member(member)?;
        changed |= !events.is_empty();
        normalized_lengths.push(bytes.len() as u64);
        metas.push(MemberMeta { original_len: *len, normalized_len: bytes.len() as u64, events });
        normalized.extend_from_slice(&bytes);
        base = end;
    }
    if !changed { return Ok((baseline, convert_stats(base_stats, baseline_len))); }

    let (nested, nested_stats) = pithos_native_v3::encode_exact_dedup(&normalized, &normalized_lengths, level)?;
    let metadata = encode_metadata(&metas)?;
    let metadata = zstd::stream::encode_all(Cursor::new(metadata), 3)?;
    let mut candidate = Vec::with_capacity(HEADER_LEN + metadata.len() + nested.len());
    candidate.extend_from_slice(MAGIC);
    candidate.extend_from_slice(&4_u16.to_le_bytes());
    candidate.extend_from_slice(&0_u16.to_le_bytes());
    candidate.extend_from_slice(&(input.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&(normalized.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&(metas.len() as u32).to_le_bytes());
    candidate.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    candidate.extend_from_slice(&(nested.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&metadata);
    candidate.extend_from_slice(&nested);
    if candidate.len() >= baseline.len() {
        return Ok((baseline, convert_stats(base_stats, baseline_len)));
    }
    Ok((candidate, NativeStats {
        chunk_count: nested_stats.chunk_count,
        canonical_chunks: nested_stats.canonical_chunks,
        gross_duplicate_bytes: nested_stats.gross_duplicate_bytes,
        representation_bytes: nested_stats.representation_bytes + metadata.len() as u64,
        encoded_bytes: (HEADER_LEN + metadata.len() + nested.len()) as u64,
    }))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    if payload.len() < HEADER_LEN || &payload[..4] != MAGIC {
        return pithos_native_v3::decode_exact_dedup(payload, expected_len);
    }
    if read_u16(payload, 4)? != 4 || read_u64(payload, 8)? != expected_len {
        return Err(PithosError::InvalidMetadata("canonicalization header"));
    }
    let normalized_len = read_u64(payload, 16)?;
    let member_count = read_u32(payload, 24)? as usize;
    let meta_len = read_u32(payload, 28)? as usize;
    let nested_len = read_u64(payload, 32)? as usize;
    let meta_start = HEADER_LEN;
    let meta_end = meta_start.checked_add(meta_len).ok_or(PithosError::IntegerOverflow)?;
    let nested_end = meta_end.checked_add(nested_len).ok_or(PithosError::IntegerOverflow)?;
    if nested_end != payload.len() { return Err(PithosError::InvalidRange); }
    let metadata = zstd::stream::decode_all(Cursor::new(payload.get(meta_start..meta_end).ok_or(PithosError::InvalidRange)?))?;
    let metas = decode_metadata(&metadata, member_count)?;
    let normalized = pithos_native_v3::decode_exact_dedup(payload.get(meta_end..nested_end).ok_or(PithosError::InvalidRange)?, normalized_len)?;
    restore_members(&normalized, &metas, expected_len)
}

fn canonicalize_member(input: &[u8]) -> Result<(Vec<u8>, Vec<Event>)> {
    if std::str::from_utf8(input).is_err() { return Ok((input.to_vec(), Vec::new())); }
    let first = input.iter().copied().find(|b| !b.is_ascii_whitespace());
    let json = matches!(first, Some(b'{') | Some(b'[')) && serde_json::from_slice::<serde_json::Value>(input).is_ok();
    let mut out = Vec::with_capacity(input.len());
    let mut events = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0_usize;
    while index < input.len() {
        let byte = input[index];
        if json {
            if in_string {
                out.push(byte);
                if escaped { escaped = false; }
                else if byte == b'\\' { escaped = true; }
                else if byte == b'"' { in_string = false; }
                index += 1; continue;
            }
            if byte == b'"' { in_string = true; out.push(byte); index += 1; continue; }
            if byte.is_ascii_whitespace() {
                let start = index; while index < input.len() && input[index].is_ascii_whitespace() { index += 1; }
                events.push(Event { offset: out.len() as u32, bytes: input[start..index].to_vec() }); continue;
            }
            out.push(byte); index += 1;
        } else if byte == b'\r' && input.get(index + 1) == Some(&b'\n') {
            events.push(Event { offset: out.len() as u32, bytes: vec![b'\r'] }); index += 1;
        } else { out.push(byte); index += 1; }
    }
    Ok((out, events))
}

fn restore_members(normalized: &[u8], metas: &[MemberMeta], expected_len: u64) -> Result<Vec<u8>> {
    let mut result = Vec::with_capacity(expected_len as usize);
    let mut base = 0_usize;
    for meta in metas {
        let member_start = result.len();
        let end = base.checked_add(meta.normalized_len as usize).ok_or(PithosError::IntegerOverflow)?;
        let member = normalized.get(base..end).ok_or(PithosError::InvalidRange)?;
        let mut cursor = 0_usize;
        for event in &meta.events {
            let offset = event.offset as usize;
            if offset < cursor || offset > member.len() { return Err(PithosError::InvalidMetadata("canonical event order")); }
            result.extend_from_slice(&member[cursor..offset]); result.extend_from_slice(&event.bytes); cursor = offset;
        }
        result.extend_from_slice(&member[cursor..]);
        if result.len() - member_start != meta.original_len as usize { return Err(PithosError::InvalidMetadata("canonical original length")); }
        if result.len() as u64 > expected_len { return Err(PithosError::ResourceLimit("canonical restore output")); }
        base = end;
    }
    if base != normalized.len() || result.len() as u64 != expected_len { return Err(PithosError::InvalidRange); }
    Ok(result)
}

fn encode_metadata(metas: &[MemberMeta]) -> Result<Vec<u8>> {
    let mut out = Vec::new(); out.extend_from_slice(&(metas.len() as u32).to_le_bytes());
    for meta in metas {
        out.extend_from_slice(&meta.original_len.to_le_bytes()); out.extend_from_slice(&meta.normalized_len.to_le_bytes()); out.extend_from_slice(&(meta.events.len() as u32).to_le_bytes());
        for event in &meta.events { out.extend_from_slice(&event.offset.to_le_bytes()); out.extend_from_slice(&(event.bytes.len() as u32).to_le_bytes()); out.extend_from_slice(&event.bytes); }
    } Ok(out)
}
fn decode_metadata(data:&[u8], expected:usize)->Result<Vec<MemberMeta>> { let count=read_u32(data,0)? as usize; if count!=expected{return Err(PithosError::InvalidMetadata("canonical member count"));} let mut pos=4; let mut metas=Vec::with_capacity(count); for _ in 0..count { let original_len=read_u64(data,pos)?;pos+=8;let normalized_len=read_u64(data,pos)?;pos+=8;let ec=read_u32(data,pos)? as usize;pos+=4;let mut events=Vec::with_capacity(ec);for _ in 0..ec{let offset=read_u32(data,pos)?;pos+=4;let len=read_u32(data,pos)? as usize;pos+=4;let end=pos.checked_add(len).ok_or(PithosError::IntegerOverflow)?;events.push(Event{offset,bytes:data.get(pos..end).ok_or(PithosError::InvalidRange)?.to_vec()});pos=end;}metas.push(MemberMeta{original_len,normalized_len,events});}if pos!=data.len(){return Err(PithosError::InvalidMetadata("canonical metadata trailing"));}Ok(metas)}
fn convert_stats(s:pithos_native_v3::NativeStats,encoded:u64)->NativeStats{NativeStats{chunk_count:s.chunk_count,canonical_chunks:s.canonical_chunks,gross_duplicate_bytes:s.gross_duplicate_bytes,representation_bytes:s.representation_bytes,encoded_bytes:encoded}}
fn read_u16(b:&[u8],o:usize)->Result<u16>{let s=b.get(o..o+2).ok_or(PithosError::InvalidRange)?;Ok(u16::from_le_bytes([s[0],s[1]]))}
fn read_u32(b:&[u8],o:usize)->Result<u32>{let s=b.get(o..o+4).ok_or(PithosError::InvalidRange)?;Ok(u32::from_le_bytes([s[0],s[1],s[2],s[3]]))}
fn read_u64(b:&[u8],o:usize)->Result<u64>{let s=b.get(o..o+8).ok_or(PithosError::InvalidRange)?;Ok(u64::from_le_bytes([s[0],s[1],s[2],s[3],s[4],s[5],s[6],s[7]]))}

#[cfg(test)] mod tests { use super::*; #[test] fn json_whitespace_is_reversible(){let a=b"{ \r\n  \"a\" : 1, \"b\" : [1, 2] }";let input=[a.as_slice(),a.as_slice()].concat();let (e,_)=encode_exact_dedup(&input,&[a.len()as u64,a.len()as u64],5).unwrap();assert_eq!(decode_exact_dedup(&e,input.len()as u64).unwrap(),input);} }
