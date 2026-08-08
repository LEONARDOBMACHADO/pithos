//! Native codec v7: mined synthetic bases and bounded mathematical byte rules.

use pithos_core::{PithosError, Result};
use std::collections::HashMap;
use std::io::Cursor;
use xxhash_rust::xxh3::xxh3_64;

pub const NATIVE_CODEC_ID: u16 = pithos_native_v6::NATIVE_CODEC_ID;
pub const NATIVE_CODEC_VERSION: u16 = pithos_native_v6::NATIVE_CODEC_VERSION;
const MAGIC: &[u8; 4] = b"PSM7";
const MEMBER_MAGIC: &[u8; 4] = b"SYN7";
const HEADER_LEN: usize = 40;
const BASE_LEN: usize = 32;
const MAX_BASES: usize = 256;
const MIN_BASE_USES: u32 = 3;
const MIN_MATH: usize = 16;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}
#[derive(Debug)]
struct Meta {
    original_len: u64,
    transformed_len: u64,
}

pub fn encode_exact_dedup(
    input: &[u8],
    members: &[u64],
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    let (baseline, bs) = pithos_native_v6::encode_exact_dedup(input, members, level)?;
    let baseline_len = baseline.len() as u64;
    let bases = mine_bases(input);
    if bases.is_empty() {
        return Ok((baseline, convert(bs, baseline_len)));
    }
    let mut transformed = Vec::new();
    let mut lengths = Vec::new();
    let mut metas = Vec::new();
    let mut p = 0usize;
    let mut any = false;
    for &len in members {
        let e = p
            .checked_add(len as usize)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(p..e).ok_or(PithosError::InvalidRange)?;
        let encoded = encode_member(member, &bases)?;
        any |= encoded.len() < member.len();
        let chosen = if encoded.len() < member.len() {
            encoded
        } else {
            member.to_vec()
        };
        lengths.push(chosen.len() as u64);
        metas.push(Meta {
            original_len: len,
            transformed_len: chosen.len() as u64,
        });
        transformed.extend_from_slice(&chosen);
        p = e;
    }
    if !any {
        return Ok((baseline, convert(bs, baseline_len)));
    }
    let (nested, ns) = pithos_native_v6::encode_exact_dedup(&transformed, &lengths, level)?;
    let meta = zstd::stream::encode_all(Cursor::new(encode_meta(&bases, &metas)), 3)?;
    let mut candidate = Vec::with_capacity(HEADER_LEN + meta.len() + nested.len());
    candidate.extend_from_slice(MAGIC);
    candidate.extend_from_slice(&7u16.to_le_bytes());
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
        return pithos_native_v6::decode_exact_dedup(payload, expected);
    }
    if read_u16(payload, 4)? != 7 || read_u64(payload, 8)? != expected {
        return Err(PithosError::InvalidMetadata("synthetic header"));
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
    let (bases, metas) = decode_meta(&meta, count)?;
    let transformed = pithos_native_v6::decode_exact_dedup(&payload[me..ne], transformed_len)?;
    let mut out = Vec::with_capacity(expected as usize);
    let mut p = 0usize;
    for m in metas {
        let e = p
            .checked_add(m.transformed_len as usize)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = transformed.get(p..e).ok_or(PithosError::InvalidRange)?;
        let decoded = if member.starts_with(MEMBER_MAGIC) {
            decode_member(member, &bases, m.original_len)?
        } else {
            if member.len() as u64 != m.original_len {
                return Err(PithosError::InvalidRange);
            }
            member.to_vec()
        };
        out.extend_from_slice(&decoded);
        p = e;
    }
    if p != transformed.len() || out.len() as u64 != expected {
        return Err(PithosError::InvalidRange);
    }
    Ok(out)
}

fn mine_bases(input: &[u8]) -> Vec<[u8; BASE_LEN]> {
    let mut map = HashMap::<u64, ([u8; BASE_LEN], u32)>::new();
    if input.len() < BASE_LEN {
        return Vec::new();
    }
    for p in (0..=input.len() - BASE_LEN).step_by(16) {
        let slice = &input[p..p + BASE_LEN];
        let key = xxh3_64(slice);
        let mut arr = [0u8; BASE_LEN];
        arr.copy_from_slice(slice);
        match map.get_mut(&key) {
            Some((existing, count)) if existing.as_slice() == slice => {
                *count = count.saturating_add(1)
            }
            Some(_) => {}
            None => {
                map.insert(key, (arr, 1));
            }
        }
    }
    let mut items = map
        .into_values()
        .filter(|(_, c)| *c >= MIN_BASE_USES)
        .collect::<Vec<_>>();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    items.truncate(MAX_BASES);
    items.into_iter().map(|(b, _)| b).collect()
}
fn encode_member(input: &[u8], bases: &[[u8; BASE_LEN]]) -> Result<Vec<u8>> {
    let mut lookup = HashMap::<u64, Vec<usize>>::new();
    for (i, b) in bases.iter().enumerate() {
        lookup.entry(xxh3_64(b)).or_default().push(i);
    }
    let mut o = Vec::new();
    o.extend_from_slice(MEMBER_MAGIC);
    put_var(input.len() as u64, &mut o);
    let mut p = 0usize;
    let mut lit = Vec::new();
    while p < input.len() {
        let math = math_len(input, p);
        if math >= MIN_MATH {
            flush(&mut lit, &mut o);
            o.push(2);
            o.push(input[p]);
            o.push(input[p + 1].wrapping_sub(input[p]));
            put_var(math as u64, &mut o);
            p += math;
            continue;
        }
        if p + BASE_LEN <= input.len() {
            let slice = &input[p..p + BASE_LEN];
            if let Some(ids) = lookup.get(&xxh3_64(slice))
                && let Some(&id) = ids.iter().find(|&&id| bases[id].as_slice() == slice)
            {
                flush(&mut lit, &mut o);
                o.push(1);
                put_var(id as u64, &mut o);
                p += BASE_LEN;
                continue;
            }
        }
        lit.push(input[p]);
        p += 1;
        if lit.len() >= 64 * 1024 {
            flush(&mut lit, &mut o);
        }
    }
    flush(&mut lit, &mut o);
    Ok(o)
}
fn decode_member(data: &[u8], bases: &[[u8; BASE_LEN]], expected: u64) -> Result<Vec<u8>> {
    let mut p = 4usize;
    let declared = get_var(data, &mut p)?;
    if declared != expected {
        return Err(PithosError::InvalidMetadata("synthetic member length"));
    }
    let mut o = Vec::with_capacity(expected as usize);
    while p < data.len() {
        let k = *data.get(p).ok_or(PithosError::InvalidRange)?;
        p += 1;
        match k {
            0 => {
                let l = get_var(data, &mut p)? as usize;
                let e = p + l;
                o.extend_from_slice(data.get(p..e).ok_or(PithosError::InvalidRange)?);
                p = e;
            }
            1 => {
                let id = get_var(data, &mut p)? as usize;
                o.extend_from_slice(
                    bases
                        .get(id)
                        .ok_or(PithosError::InvalidMetadata("synthetic base"))?,
                );
            }
            2 => {
                let start = *data.get(p).ok_or(PithosError::InvalidRange)?;
                let delta = *data.get(p + 1).ok_or(PithosError::InvalidRange)?;
                p += 2;
                let l = get_var(data, &mut p)? as usize;
                for n in 0..l {
                    o.push(start.wrapping_add(delta.wrapping_mul(n as u8)));
                }
            }
            _ => return Err(PithosError::InvalidMetadata("synthetic token")),
        }
        if o.len() as u64 > expected {
            return Err(PithosError::ResourceLimit("synthetic output"));
        }
    }
    if o.len() as u64 != expected {
        return Err(PithosError::InvalidRange);
    }
    Ok(o)
}
fn math_len(d: &[u8], p: usize) -> usize {
    if p + 2 > d.len() {
        return 0;
    }
    let delta = d[p + 1].wrapping_sub(d[p]);
    let mut e = p + 2;
    while e < d.len() && d[e] == d[e - 1].wrapping_add(delta) {
        e += 1;
    }
    e - p
}
fn flush(l: &mut Vec<u8>, o: &mut Vec<u8>) {
    if l.is_empty() {
        return;
    }
    o.push(0);
    put_var(l.len() as u64, o);
    o.extend_from_slice(l);
    l.clear();
}
fn encode_meta(bases: &[[u8; BASE_LEN]], metas: &[Meta]) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(&(bases.len() as u32).to_le_bytes());
    for b in bases {
        o.extend_from_slice(b);
    }
    o.extend_from_slice(&(metas.len() as u32).to_le_bytes());
    for m in metas {
        o.extend_from_slice(&m.original_len.to_le_bytes());
        o.extend_from_slice(&m.transformed_len.to_le_bytes());
    }
    o
}
fn decode_meta(d: &[u8], expected: usize) -> Result<(Vec<[u8; BASE_LEN]>, Vec<Meta>)> {
    let bc = read_u32(d, 0)? as usize;
    if bc > MAX_BASES {
        return Err(PithosError::ResourceLimit("synthetic bases"));
    }
    let mut p = 4;
    let mut bases = Vec::with_capacity(bc);
    for _ in 0..bc {
        let e = p + BASE_LEN;
        let mut b = [0u8; BASE_LEN];
        b.copy_from_slice(d.get(p..e).ok_or(PithosError::InvalidRange)?);
        bases.push(b);
        p = e;
    }
    let mc = read_u32(d, p)? as usize;
    p += 4;
    if mc != expected {
        return Err(PithosError::InvalidMetadata("synthetic member count"));
    }
    let mut metas = Vec::with_capacity(mc);
    for _ in 0..mc {
        let a = read_u64(d, p)?;
        p += 8;
        let b = read_u64(d, p)?;
        p += 8;
        metas.push(Meta {
            original_len: a,
            transformed_len: b,
        });
    }
    if p != d.len() {
        return Err(PithosError::InvalidMetadata("synthetic metadata trailing"));
    }
    Ok((bases, metas))
}
fn put_var(mut v: u64, o: &mut Vec<u8>) {
    loop {
        let mut b = (v & 127) as u8;
        v >>= 7;
        if v != 0 {
            b |= 128;
        }
        o.push(b);
        if v == 0 {
            break;
        }
    }
}
fn get_var(d: &[u8], p: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut s = 0;
    loop {
        let b = *d.get(*p).ok_or(PithosError::InvalidRange)?;
        *p += 1;
        if s >= 64 {
            return Err(PithosError::IntegerOverflow);
        }
        v |= ((b & 127) as u64) << s;
        if b & 128 == 0 {
            return Ok(v);
        }
        s += 7;
    }
}
fn convert(s: pithos_native_v6::NativeStats, e: u64) -> NativeStats {
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
    fn math_rule_roundtrip() {
        let d = (0..200u16)
            .map(|n| (n as u8).wrapping_mul(3))
            .collect::<Vec<_>>();
        let bases = vec![];
        let e = encode_member(&d, &bases).unwrap();
        assert_eq!(decode_member(&e, &bases, d.len() as u64).unwrap(), d);
    }
}
