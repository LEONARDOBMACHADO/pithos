//! Native codec v5: exact structural recompression for GZIP and PNG IDAT.

use flate2::{
    Compression,
    read::{GzDecoder, ZlibDecoder},
    write::{GzEncoder, ZlibEncoder},
};
use pithos_core::{PithosError, Result};
use std::io::{Cursor, Read, Write};

pub const NATIVE_CODEC_ID: u16 = pithos_native_v4::NATIVE_CODEC_ID;
pub const NATIVE_CODEC_VERSION: u16 = pithos_native_v4::NATIVE_CODEC_VERSION;
const MAGIC: &[u8; 4] = b"PNR5";
const HEADER_LEN: usize = 40;
const MAX_INFLATED_MEMBER: usize = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}

#[derive(Debug)]
struct Delta {
    prefix: u32,
    suffix: u32,
    original_len: u32,
    middle: Vec<u8>,
}
#[derive(Debug)]
enum Model {
    Raw {
        transformed_len: u64,
    },
    Gzip {
        transformed_len: u64,
        delta: Delta,
    },
    Png {
        transformed_len: u64,
        lengths: Vec<u32>,
        prefix: Vec<u8>,
        gaps: Vec<Vec<u8>>,
        delta: Delta,
    },
}

pub fn encode_exact_dedup(
    input: &[u8],
    member_lengths: &[u64],
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    let (baseline, base_stats) =
        pithos_native_v4::encode_exact_dedup(input, member_lengths, level)?;
    let baseline_len = baseline.len() as u64;
    let mut transformed = Vec::new();
    let mut transformed_lengths = Vec::new();
    let mut models = Vec::new();
    let mut pos = 0usize;
    let mut any = false;
    for &length in member_lengths {
        let end = pos
            .checked_add(length as usize)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(pos..end).ok_or(PithosError::InvalidRange)?;
        let (bytes, model) = model_member(member)?;
        any |= !matches!(model, Model::Raw { .. });
        transformed_lengths.push(bytes.len() as u64);
        transformed.extend_from_slice(&bytes);
        models.push(model);
        pos = end;
    }
    if pos != input.len() || !any {
        return Ok((baseline, convert_stats(base_stats, baseline_len)));
    }
    let (nested, nested_stats) =
        pithos_native_v4::encode_exact_dedup(&transformed, &transformed_lengths, level)?;
    let metadata = zstd::stream::encode_all(Cursor::new(encode_models(&models)?), 3)?;
    let mut candidate = Vec::with_capacity(HEADER_LEN + metadata.len() + nested.len());
    candidate.extend_from_slice(MAGIC);
    candidate.extend_from_slice(&5u16.to_le_bytes());
    candidate.extend_from_slice(&0u16.to_le_bytes());
    candidate.extend_from_slice(&(input.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&(transformed.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&(models.len() as u32).to_le_bytes());
    candidate.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    candidate.extend_from_slice(&(nested.len() as u64).to_le_bytes());
    candidate.extend_from_slice(&metadata);
    candidate.extend_from_slice(&nested);
    if candidate.len() >= baseline.len() {
        return Ok((baseline, convert_stats(base_stats, baseline_len)));
    }
    Ok((
        candidate,
        NativeStats {
            chunk_count: nested_stats.chunk_count,
            canonical_chunks: nested_stats.canonical_chunks,
            gross_duplicate_bytes: nested_stats.gross_duplicate_bytes,
            representation_bytes: nested_stats.representation_bytes + metadata.len() as u64,
            encoded_bytes: (HEADER_LEN + metadata.len() + nested.len()) as u64,
        },
    ))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    if payload.len() < HEADER_LEN || &payload[..4] != MAGIC {
        return pithos_native_v4::decode_exact_dedup(payload, expected_len);
    }
    if read_u16(payload, 4)? != 5 || read_u64(payload, 8)? != expected_len {
        return Err(PithosError::InvalidMetadata("recompression header"));
    }
    let transformed_len = read_u64(payload, 16)?;
    let count = read_u32(payload, 24)? as usize;
    let meta_len = read_u32(payload, 28)? as usize;
    let nested_len = read_u64(payload, 32)? as usize;
    let meta_end = HEADER_LEN
        .checked_add(meta_len)
        .ok_or(PithosError::IntegerOverflow)?;
    let nested_end = meta_end
        .checked_add(nested_len)
        .ok_or(PithosError::IntegerOverflow)?;
    if nested_end != payload.len() {
        return Err(PithosError::InvalidRange);
    }
    let meta = zstd::stream::decode_all(Cursor::new(
        payload
            .get(HEADER_LEN..meta_end)
            .ok_or(PithosError::InvalidRange)?,
    ))?;
    let models = decode_models(&meta, count)?;
    let transformed = pithos_native_v4::decode_exact_dedup(
        payload
            .get(meta_end..nested_end)
            .ok_or(PithosError::InvalidRange)?,
        transformed_len,
    )?;
    restore_models(&transformed, &models, expected_len)
}

fn model_member(member: &[u8]) -> Result<(Vec<u8>, Model)> {
    if member.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = GzDecoder::new(member);
        if let Ok(decoded) = read_limited(&mut decoder) {
            let canonical = gzip_encode(&decoded)?;
            let delta = make_delta(&canonical, member)?;
            let overhead = delta.middle.len() + 16;
            if overhead < member.len() {
                return Ok((
                    decoded.clone(),
                    Model::Gzip {
                        transformed_len: decoded.len() as u64,
                        delta,
                    },
                ));
            }
        }
    }
    if member.starts_with(b"\x89PNG\r\n\x1a\n") && let Some(parts) = parse_png(member)? {
        let mut decoder = ZlibDecoder::new(parts.idat.as_slice());
        if let Ok(decoded) = read_limited(&mut decoder) {
            let canonical = zlib_encode(&decoded)?;
            let delta = make_delta(&canonical, &parts.idat)?;
            let metadata_cost = parts.prefix.len()
                + parts.gaps.iter().map(Vec::len).sum::<usize>()
                + delta.middle.len()
                + parts.lengths.len() * 4;
            if metadata_cost < member.len() {
                return Ok((
                    decoded.clone(),
                    Model::Png {
                        transformed_len: decoded.len() as u64,
                        lengths: parts.lengths,
                        prefix: parts.prefix,
                        gaps: parts.gaps,
                        delta,
                    },
                ));
            }
        }
    }
    Ok((
        member.to_vec(),
        Model::Raw {
            transformed_len: member.len() as u64,
        },
    ))
}

struct PngParts {
    lengths: Vec<u32>,
    prefix: Vec<u8>,
    gaps: Vec<Vec<u8>>,
    idat: Vec<u8>,
}
fn parse_png(data: &[u8]) -> Result<Option<PngParts>> {
    let mut pos = 8usize;
    let mut ranges = Vec::<(usize, usize)>::new();
    while pos + 12 <= data.len() {
        let len = u32::from_be_bytes(
            data[pos..pos + 4]
                .try_into()
                .map_err(|_| PithosError::InvalidRange)?,
        ) as usize;
        let typ = &data[pos + 4..pos + 8];
        let ds = pos + 8;
        let de = ds.checked_add(len).ok_or(PithosError::IntegerOverflow)?;
        let end = de.checked_add(4).ok_or(PithosError::IntegerOverflow)?;
        if end > data.len() {
            return Ok(None);
        }
        if typ == b"IDAT" {
            ranges.push((ds, de));
        }
        pos = end;
        if typ == b"IEND" {
            break;
        }
    }
    if ranges.is_empty() {
        return Ok(None);
    }
    let prefix = data[..ranges[0].0].to_vec();
    let mut gaps = Vec::new();
    let mut lengths = Vec::new();
    let mut idat = Vec::new();
    for (i, (s, e)) in ranges.iter().copied().enumerate() {
        lengths.push((e - s) as u32);
        idat.extend_from_slice(&data[s..e]);
        let next = if i + 1 < ranges.len() {
            ranges[i + 1].0
        } else {
            data.len()
        };
        gaps.push(data[e..next].to_vec());
    }
    Ok(Some(PngParts {
        lengths,
        prefix,
        gaps,
        idat,
    }))
}

fn restore_models(transformed: &[u8], models: &[Model], expected: u64) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected as usize);
    let mut pos = 0usize;
    for model in models {
        let len = match model {
            Model::Raw { transformed_len }
            | Model::Gzip {
                transformed_len, ..
            }
            | Model::Png {
                transformed_len, ..
            } => *transformed_len,
        } as usize;
        let end = pos.checked_add(len).ok_or(PithosError::IntegerOverflow)?;
        let bytes = transformed.get(pos..end).ok_or(PithosError::InvalidRange)?;
        match model {
            Model::Raw { .. } => out.extend_from_slice(bytes),
            Model::Gzip { delta, .. } => {
                let canonical = gzip_encode(bytes)?;
                out.extend_from_slice(&apply_delta(&canonical, delta)?);
            }
            Model::Png {
                lengths,
                prefix,
                gaps,
                delta,
                ..
            } => {
                let canonical = zlib_encode(bytes)?;
                let original = apply_delta(&canonical, delta)?;
                if lengths.iter().map(|v| *v as usize).sum::<usize>() != original.len() {
                    return Err(PithosError::InvalidMetadata("PNG IDAT length"));
                }
                out.extend_from_slice(prefix);
                let mut p = 0usize;
                for (i, l) in lengths.iter().enumerate() {
                    let e = p + *l as usize;
                    out.extend_from_slice(&original[p..e]);
                    out.extend_from_slice(
                        gaps.get(i).ok_or(PithosError::InvalidMetadata("PNG gap"))?,
                    );
                    p = e;
                }
            }
        }
        pos = end;
    }
    if pos != transformed.len() || out.len() as u64 != expected {
        return Err(PithosError::InvalidRange);
    }
    Ok(out)
}

fn make_delta(canonical: &[u8], original: &[u8]) -> Result<Delta> {
    let prefix = canonical
        .iter()
        .zip(original)
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = canonical.len().min(original.len()).saturating_sub(prefix);
    let suffix = canonical
        .iter()
        .rev()
        .zip(original.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let middle = original[prefix..original.len() - suffix].to_vec();
    Ok(Delta {
        prefix: prefix as u32,
        suffix: suffix as u32,
        original_len: original.len() as u32,
        middle,
    })
}
fn apply_delta(canonical: &[u8], d: &Delta) -> Result<Vec<u8>> {
    let p = d.prefix as usize;
    let s = d.suffix as usize;
    if p + s > canonical.len() {
        return Err(PithosError::InvalidMetadata("recompression delta"));
    }
    let mut out = Vec::with_capacity(d.original_len as usize);
    out.extend_from_slice(&canonical[..p]);
    out.extend_from_slice(&d.middle);
    out.extend_from_slice(&canonical[canonical.len() - s..]);
    if out.len() != d.original_len as usize {
        return Err(PithosError::InvalidRange);
    }
    Ok(out)
}
fn gzip_encode(data: &[u8]) -> Result<Vec<u8>> {
    let mut e = GzEncoder::new(Vec::new(), Compression::best());
    e.write_all(data)?;
    Ok(e.finish()?)
}
fn zlib_encode(data: &[u8]) -> Result<Vec<u8>> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::best());
    e.write_all(data)?;
    Ok(e.finish()?)
}
fn read_limited<R: Read>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out
            .len()
            .checked_add(n)
            .is_none_or(|v| v > MAX_INFLATED_MEMBER)
        {
            return Err(std::io::Error::other("inflated member limit"));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

fn encode_models(models: &[Model]) -> Result<Vec<u8>> {
    let mut o = Vec::new();
    o.extend_from_slice(&(models.len() as u32).to_le_bytes());
    for m in models {
        match m {
            Model::Raw { transformed_len } => {
                o.push(0);
                o.extend_from_slice(&transformed_len.to_le_bytes());
            }
            Model::Gzip {
                transformed_len,
                delta,
            } => {
                o.push(1);
                o.extend_from_slice(&transformed_len.to_le_bytes());
                encode_delta(delta, &mut o);
            }
            Model::Png {
                transformed_len,
                lengths,
                prefix,
                gaps,
                delta,
            } => {
                o.push(2);
                o.extend_from_slice(&transformed_len.to_le_bytes());
                o.extend_from_slice(&(lengths.len() as u32).to_le_bytes());
                o.extend_from_slice(&(prefix.len() as u32).to_le_bytes());
                o.extend_from_slice(prefix);
                for l in lengths {
                    o.extend_from_slice(&l.to_le_bytes());
                }
                for g in gaps {
                    o.extend_from_slice(&(g.len() as u32).to_le_bytes());
                    o.extend_from_slice(g);
                }
                encode_delta(delta, &mut o);
            }
        }
    }
    Ok(o)
}
fn decode_models(d: &[u8], expected: usize) -> Result<Vec<Model>> {
    let count = read_u32(d, 0)? as usize;
    if count != expected {
        return Err(PithosError::InvalidMetadata("recompression model count"));
    }
    let mut p = 4;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let k = *d.get(p).ok_or(PithosError::InvalidRange)?;
        p += 1;
        let t = read_u64(d, p)?;
        p += 8;
        out.push(match k {
            0 => Model::Raw { transformed_len: t },
            1 => Model::Gzip {
                transformed_len: t,
                delta: decode_delta(d, &mut p)?,
            },
            2 => {
                let c = read_u32(d, p)? as usize;
                p += 4;
                let pl = read_u32(d, p)? as usize;
                p += 4;
                let pe = p + pl;
                let prefix = d.get(p..pe).ok_or(PithosError::InvalidRange)?.to_vec();
                p = pe;
                let mut lengths = Vec::with_capacity(c);
                for _ in 0..c {
                    lengths.push(read_u32(d, p)?);
                    p += 4;
                }
                let mut gaps = Vec::with_capacity(c);
                for _ in 0..c {
                    let l = read_u32(d, p)? as usize;
                    p += 4;
                    let e = p + l;
                    gaps.push(d.get(p..e).ok_or(PithosError::InvalidRange)?.to_vec());
                    p = e;
                }
                Model::Png {
                    transformed_len: t,
                    lengths,
                    prefix,
                    gaps,
                    delta: decode_delta(d, &mut p)?,
                }
            }
            _ => return Err(PithosError::InvalidMetadata("recompression model")),
        });
    }
    if p != d.len() {
        return Err(PithosError::InvalidMetadata(
            "recompression metadata trailing",
        ));
    }
    Ok(out)
}
fn encode_delta(d: &Delta, o: &mut Vec<u8>) {
    o.extend_from_slice(&d.prefix.to_le_bytes());
    o.extend_from_slice(&d.suffix.to_le_bytes());
    o.extend_from_slice(&d.original_len.to_le_bytes());
    o.extend_from_slice(&(d.middle.len() as u32).to_le_bytes());
    o.extend_from_slice(&d.middle);
}
fn decode_delta(d: &[u8], p: &mut usize) -> Result<Delta> {
    let prefix = read_u32(d, *p)?;
    *p += 4;
    let suffix = read_u32(d, *p)?;
    *p += 4;
    let original_len = read_u32(d, *p)?;
    *p += 4;
    let l = read_u32(d, *p)? as usize;
    *p += 4;
    let e = *p + l;
    let middle = d.get(*p..e).ok_or(PithosError::InvalidRange)?.to_vec();
    *p = e;
    Ok(Delta {
        prefix,
        suffix,
        original_len,
        middle,
    })
}
fn convert_stats(s: pithos_native_v4::NativeStats, e: u64) -> NativeStats {
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
    fn gzip_roundtrip() {
        let raw = vec![b'A'; 200_000];
        let gz = gzip_encode(&raw).unwrap();
        let (e, _) = encode_exact_dedup(&gz, &[gz.len() as u64], 5).unwrap();
        assert_eq!(decode_exact_dedup(&e, gz.len() as u64).unwrap(), gz);
    }
}
