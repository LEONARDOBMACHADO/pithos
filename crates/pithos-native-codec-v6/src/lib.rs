//! Native codec v6: local grammar, varint enumeration and bounded residual recursion.

use pithos_core::{PithosError, Result};
use std::collections::HashMap;
use std::io::Cursor;
use xxhash_rust::xxh3::xxh3_64;

pub const NATIVE_CODEC_ID: u16 = pithos_native_v5::NATIVE_CODEC_ID;
pub const NATIVE_CODEC_VERSION: u16 = pithos_native_v5::NATIVE_CODEC_VERSION;
const MAGIC: &[u8; 4] = b"PGR6";
const GRAMMAR_MAGIC: &[u8; 4] = b"GRM1";
const HEADER_LEN: usize = 40;
const MAX_DEPTH: u8 = 3;
const MIN_MATCH: usize = 12;
const MAX_DISTANCE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}
#[derive(Debug)]
struct MemberMeta {
    original_len: u64,
    transformed_len: u64,
    depth: u8,
}

pub fn encode_exact_dedup(
    input: &[u8],
    member_lengths: &[u64],
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    let (baseline, bs) = pithos_native_v5::encode_exact_dedup(input, member_lengths, level)?;
    let baseline_len = baseline.len() as u64;
    let mut transformed = Vec::new();
    let mut lengths = Vec::new();
    let mut metas = Vec::new();
    let mut p = 0usize;
    let mut any = false;
    for &len in member_lengths {
        let e = p
            .checked_add(len as usize)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(p..e).ok_or(PithosError::InvalidRange)?;
        let (mut current, mut depth) = (member.to_vec(), 0u8);
        while depth < MAX_DEPTH {
            let next = grammar_encode(&current)?;
            if next.len() >= current.len() {
                break;
            }
            current = next;
            depth += 1;
        }
        any |= depth > 0;
        lengths.push(current.len() as u64);
        metas.push(MemberMeta {
            original_len: len,
            transformed_len: current.len() as u64,
            depth,
        });
        transformed.extend_from_slice(&current);
        p = e;
    }
    if p != input.len() || !any {
        return Ok((baseline, convert(bs, baseline_len)));
    }
    let (nested, ns) = pithos_native_v5::encode_exact_dedup(&transformed, &lengths, level)?;
    let meta = zstd::stream::encode_all(Cursor::new(encode_meta(&metas)), 3)?;
    let mut candidate = Vec::with_capacity(HEADER_LEN + meta.len() + nested.len());
    candidate.extend_from_slice(MAGIC);
    candidate.extend_from_slice(&6u16.to_le_bytes());
    candidate.extend_from_slice(&0u16.to_le_bytes());
    candidate.extend_from_slice(&(input.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&(transformed.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&(metas.len() as u32).to_le_bytes());
    candidate.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    candidate.extend_from_slice(&(nested.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&meta);
    candidate.extend_from_slice(&nested);
    if candidate.len() >= baseline.len() {
        return Ok((baseline, convert(bs, baseline_len)));
    }
    Ok((
        candidate,
        NativeStats {
            chunk_count: ns.chunk_count,
            canonical_chunks: ns.canonical_chunks,
            gross_duplicate_bytes: ns.gross_duplicate_bytes,
            representation_bytes: ns.representation_bytes + meta.len() as u64,
            encoded_bytes: (HEADER_LEN + meta.len() + nested.len()) as u64,
        },
    ))
}

pub fn decode_exact_dedup(payload: &[u8], expected: u64) -> Result<Vec<u8>> {
    if payload.len() < HEADER_LEN || &payload[..4] != MAGIC {
        return pithos_native_v5::decode_exact_dedup(payload, expected);
    }
    if read_u16(payload, 4)? != 6 || read_u64(payload, 8)? != expected {
        return Err(PithosError::InvalidMetadata("grammar header"));
    }
    let transformed_len = read_u64(payload, 16)?;
    let count = read_u32(payload, 24)? as usize;
    let ml = read_u32(payload, 28)? as usize;
    let nl = read_u64(payload, 32)? as usize;
    let me = HEADER_LEN
        .checked_add(ml)
        .ok_or(PithosError::IntegerOverflow)?;
    let ne = me.checked_add(nl).ok_or(PithosError::IntegerOverflow)?;
    if ne != payload.len() {
        return Err(PithosError::InvalidRange);
    }
    let meta = zstd::stream::decode_all(Cursor::new(&payload[HEADER_LEN..me]))?;
    let metas = decode_meta(&meta, count)?;
    let transformed = pithos_native_v5::decode_exact_dedup(&payload[me..ne], transformed_len)?;
    let mut out = Vec::with_capacity(expected as usize);
    let mut p = 0usize;
    for m in metas {
        let e = p
            .checked_add(m.transformed_len as usize)
            .ok_or(PithosError::IntegerOverflow)?;
        let mut current = transformed
            .get(p..e)
            .ok_or(PithosError::InvalidRange)?
            .to_vec();
        for _ in 0..m.depth {
            current = grammar_decode(&current)?;
        }
        if current.len() as u64 != m.original_len {
            return Err(PithosError::InvalidRange);
        }
        out.extend_from_slice(&current);
        p = e;
    }
    if p != transformed.len() || out.len() as u64 != expected {
        return Err(PithosError::InvalidRange);
    }
    Ok(out)
}

fn grammar_encode(input: &[u8]) -> Result<Vec<u8>> {
    if input.len() < 32 {
        return Ok(input.to_vec());
    }
    let mut out = Vec::new();
    out.extend_from_slice(GRAMMAR_MAGIC);
    put_var(input.len() as u64, &mut out);
    let mut index = HashMap::<u64, usize>::new();
    let mut p = 0usize;
    let mut lit = Vec::new();
    while p < input.len() {
        let run = run_len(input, p);
        if run >= 4 {
            flush_literal(&mut lit, &mut out);
            out.push(1);
            out.push(input[p]);
            put_var(run as u64, &mut out);
            index_positions(input, p, run, &mut index);
            p += run;
            continue;
        }
        let best = best_match(input, p, &index);
        if let Some((distance, len)) = best {
            flush_literal(&mut lit, &mut out);
            out.push(2);
            put_var(distance as u64, &mut out);
            put_var(len as u64, &mut out);
            index_positions(input, p, len, &mut index);
            p += len;
        } else {
            lit.push(input[p]);
            index_positions(input, p, 1, &mut index);
            p += 1;
            if lit.len() >= 64 * 1024 {
                flush_literal(&mut lit, &mut out);
            }
        }
    }
    flush_literal(&mut lit, &mut out);
    if out.len() >= input.len() {
        Ok(input.to_vec())
    } else {
        Ok(out)
    }
}
fn grammar_decode(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 4 || &data[..4] != GRAMMAR_MAGIC {
        return Ok(data.to_vec());
    }
    let mut p = 4usize;
    let expected = get_var(data, &mut p)? as usize;
    let mut out = Vec::with_capacity(expected);
    while p < data.len() {
        let kind = *data.get(p).ok_or(PithosError::InvalidRange)?;
        p += 1;
        match kind {
            0 => {
                let len = get_var(data, &mut p)? as usize;
                let e = p.checked_add(len).ok_or(PithosError::IntegerOverflow)?;
                out.extend_from_slice(data.get(p..e).ok_or(PithosError::InvalidRange)?);
                p = e;
            }
            1 => {
                let value = *data.get(p).ok_or(PithosError::InvalidRange)?;
                p += 1;
                let len = get_var(data, &mut p)? as usize;
                if out.len().checked_add(len).is_none_or(|v| v > expected) {
                    return Err(PithosError::ResourceLimit("grammar run"));
                }
                out.resize(out.len() + len, value);
            }
            2 => {
                let distance = get_var(data, &mut p)? as usize;
                let len = get_var(data, &mut p)? as usize;
                if distance == 0 || distance > out.len() {
                    return Err(PithosError::InvalidMetadata("grammar copy"));
                }
                for _ in 0..len {
                    let b = out[out.len() - distance];
                    out.push(b);
                    if out.len() > expected {
                        return Err(PithosError::ResourceLimit("grammar copy"));
                    }
                }
            }
            _ => return Err(PithosError::InvalidMetadata("grammar token")),
        }
    }
    if out.len() != expected {
        return Err(PithosError::InvalidRange);
    }
    Ok(out)
}
fn best_match(data: &[u8], p: usize, index: &HashMap<u64, usize>) -> Option<(usize, usize)> {
    if p + 8 > data.len() {
        return None;
    }
    let key = xxh3_64(&data[p..p + 8]);
    let &prev = index.get(&key)?;
    let distance = p.checked_sub(prev)?;
    if distance == 0 || distance > MAX_DISTANCE {
        return None;
    }
    let mut len = 0usize;
    while p + len < data.len() && prev + len < p && data[p + len] == data[prev + len] {
        len += 1;
    }
    if len >= MIN_MATCH {
        Some((distance, len))
    } else {
        None
    }
}
fn index_positions(data: &[u8], start: usize, len: usize, index: &mut HashMap<u64, usize>) {
    let end = (start + len).min(data.len());
    for p in start..end {
        if p + 8 <= data.len() {
            index.insert(xxh3_64(&data[p..p + 8]), p);
        }
    }
}
fn run_len(data: &[u8], p: usize) -> usize {
    let b = data[p];
    let mut e = p + 1;
    while e < data.len() && data[e] == b {
        e += 1;
    }
    e - p
}
fn flush_literal(l: &mut Vec<u8>, o: &mut Vec<u8>) {
    if l.is_empty() {
        return;
    }
    o.push(0);
    put_var(l.len() as u64, o);
    o.extend_from_slice(l);
    l.clear();
}
fn put_var(mut v: u64, o: &mut Vec<u8>) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        o.push(b);
        if v == 0 {
            break;
        }
    }
}
fn get_var(d: &[u8], p: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *d.get(*p).ok_or(PithosError::InvalidRange)?;
        *p += 1;
        if shift >= 64 {
            return Err(PithosError::IntegerOverflow);
        }
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
}
fn encode_meta(m: &[MemberMeta]) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(&(m.len() as u32).to_le_bytes());
    for x in m {
        o.extend_from_slice(&x.original_len.to_le_bytes());
        o.extend_from_slice(&x.transformed_len.to_le_bytes());
        o.push(x.depth);
    }
    o
}
fn decode_meta(d: &[u8], expected: usize) -> Result<Vec<MemberMeta>> {
    let c = read_u32(d, 0)? as usize;
    if c != expected {
        return Err(PithosError::InvalidMetadata("grammar member count"));
    }
    let mut p = 4;
    let mut o = Vec::with_capacity(c);
    for _ in 0..c {
        let a = read_u64(d, p)?;
        p += 8;
        let b = read_u64(d, p)?;
        p += 8;
        let depth = *d.get(p).ok_or(PithosError::InvalidRange)?;
        p += 1;
        if depth > MAX_DEPTH {
            return Err(PithosError::InvalidMetadata("grammar depth"));
        }
        o.push(MemberMeta {
            original_len: a,
            transformed_len: b,
            depth,
        });
    }
    if p != d.len() {
        return Err(PithosError::InvalidMetadata("grammar metadata trailing"));
    }
    Ok(o)
}
fn convert(s: pithos_native_v5::NativeStats, e: u64) -> NativeStats {
    NativeStats {
        chunk_count: s.chunk_count,
        canonical_chunks: s.canonical_chunks,
        gross_duplicate_bytes: s.gross_duplicate_bytes,
        representation_bytes: s.representation_bytes,
        encoded_bytes: e,
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
    fn grammar_roundtrip() {
        let mut d = Vec::new();
        for _ in 0..5000 {
            d.extend_from_slice(b"alpha=123456;alpha=123456;AAAAAA\n");
        }
        let e = grammar_encode(&d).unwrap();
        assert!(e.len() < d.len());
        assert_eq!(grammar_decode(&e).unwrap(), d);
    }
}
