//! Native codec v9: fused selector that avoids recursively evaluating every
//! experimental transform on every group.

use pithos_core::{PithosError, Result};

pub const NATIVE_CODEC_ID: u16 = pithos_native_v8::NATIVE_CODEC_ID;
pub const NATIVE_CODEC_VERSION: u16 = pithos_native_v8::NATIVE_CODEC_VERSION;
const SAMPLE_PER_MEMBER: usize = 96 * 1024;
const MAX_SAMPLE_TOTAL: usize = 3 * 1024 * 1024;

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
    validate_members(input, member_lengths)?;

    if contains_nested_deflate_candidate(input, member_lengths)? {
        let (payload, stats) = pithos_native_v8::encode_exact_dedup(input, member_lengths, level)?;
        return Ok((payload, from_v8(stats)));
    }
    if contains_gzip_or_png(input, member_lengths)? {
        let (payload, stats) = pithos_native_v5::encode_exact_dedup(input, member_lengths, level)?;
        return Ok((payload, from_v5(stats)));
    }

    if is_text_like(input) {
        let (sample, sample_lengths) = deterministic_member_sample(input, member_lengths)?;
        if sample.len() >= 4096 {
            let (v3_probe, _) =
                pithos_native_v3::encode_exact_dedup(&sample, &sample_lengths, 3)?;
            let (v7_probe, _) =
                pithos_native_v7::encode_exact_dedup(&sample, &sample_lengths, 3)?;
            if v7_probe.len().saturating_mul(100) <= v3_probe.len().saturating_mul(98) {
                let (payload, stats) =
                    pithos_native_v7::encode_exact_dedup(input, member_lengths, level)?;
                return Ok((payload, from_v7(stats)));
            }
        }
    }

    let (payload, stats) = pithos_native_v3::encode_exact_dedup(input, member_lengths, level)?;
    Ok((payload, from_v3(stats)))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    pithos_native_v8::decode_exact_dedup(payload, expected_len)
}

fn validate_members(input: &[u8], member_lengths: &[u64]) -> Result<()> {
    let total = member_lengths.iter().try_fold(0_u64, |total, length| {
        total.checked_add(*length).ok_or(PithosError::IntegerOverflow)
    })?;
    if total != input.len() as u64 {
        return Err(PithosError::InvalidMetadata("native member boundaries"));
    }
    Ok(())
}

fn contains_nested_deflate_candidate(input: &[u8], members: &[u64]) -> Result<bool> {
    let mut offset = 0_usize;
    for length in members {
        let end = offset
            .checked_add(usize::try_from(*length).map_err(|_| PithosError::IntegerOverflow)?)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(offset..end).ok_or(PithosError::InvalidRange)?;
        if member.starts_with(b"PK\x03\x04") || member.starts_with(b"%PDF-") {
            return Ok(true);
        }
        offset = end;
    }
    Ok(false)
}

fn contains_gzip_or_png(input: &[u8], members: &[u64]) -> Result<bool> {
    let mut offset = 0_usize;
    for length in members {
        let end = offset
            .checked_add(usize::try_from(*length).map_err(|_| PithosError::IntegerOverflow)?)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(offset..end).ok_or(PithosError::InvalidRange)?;
        if member.starts_with(&[0x1f, 0x8b]) || member.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Ok(true);
        }
        offset = end;
    }
    Ok(false)
}

fn is_text_like(input: &[u8]) -> bool {
    if input.is_empty() || std::str::from_utf8(input).is_err() {
        return false;
    }
    let sample = if input.len() > 512 * 1024 {
        &input[..512 * 1024]
    } else {
        input
    };
    let printable = sample
        .iter()
        .filter(|byte| byte.is_ascii_graphic() || byte.is_ascii_whitespace())
        .count();
    printable.saturating_mul(100) >= sample.len().saturating_mul(92)
}

fn deterministic_member_sample(
    input: &[u8],
    member_lengths: &[u64],
) -> Result<(Vec<u8>, Vec<u64>)> {
    let mut output = Vec::new();
    let mut lengths = Vec::new();
    let mut offset = 0_usize;
    for length in member_lengths {
        if output.len() >= MAX_SAMPLE_TOTAL {
            break;
        }
        let length = usize::try_from(*length).map_err(|_| PithosError::IntegerOverflow)?;
        let end = offset.checked_add(length).ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(offset..end).ok_or(PithosError::InvalidRange)?;
        let remaining = MAX_SAMPLE_TOTAL - output.len();
        let wanted = member.len().min(SAMPLE_PER_MEMBER).min(remaining);
        if wanted == 0 {
            offset = end;
            continue;
        }
        if member.len() <= wanted {
            output.extend_from_slice(member);
        } else {
            let first = wanted / 3;
            let middle = wanted / 3;
            let last = wanted - first - middle;
            output.extend_from_slice(&member[..first]);
            let middle_start = member.len() / 2 - middle / 2;
            output.extend_from_slice(&member[middle_start..middle_start + middle]);
            output.extend_from_slice(&member[member.len() - last..]);
        }
        lengths.push(wanted as u64);
        offset = end;
    }
    Ok((output, lengths))
}

fn from_v3(stats: pithos_native_v3::NativeStats) -> NativeStats {
    NativeStats { chunk_count: stats.chunk_count, canonical_chunks: stats.canonical_chunks, gross_duplicate_bytes: stats.gross_duplicate_bytes, representation_bytes: stats.representation_bytes, encoded_bytes: stats.encoded_bytes }
}
fn from_v5(stats: pithos_native_v5::NativeStats) -> NativeStats {
    NativeStats { chunk_count: stats.chunk_count, canonical_chunks: stats.canonical_chunks, gross_duplicate_bytes: stats.gross_duplicate_bytes, representation_bytes: stats.representation_bytes, encoded_bytes: stats.encoded_bytes }
}
fn from_v7(stats: pithos_native_v7::NativeStats) -> NativeStats {
    NativeStats { chunk_count: stats.chunk_count, canonical_chunks: stats.canonical_chunks, gross_duplicate_bytes: stats.gross_duplicate_bytes, representation_bytes: stats.representation_bytes, encoded_bytes: stats.encoded_bytes }
}
fn from_v8(stats: pithos_native_v8::NativeStats) -> NativeStats {
    NativeStats { chunk_count: stats.chunk_count, canonical_chunks: stats.canonical_chunks, gross_duplicate_bytes: stats.gross_duplicate_bytes, representation_bytes: stats.representation_bytes, encoded_bytes: stats.encoded_bytes }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_generic_route_roundtrips() {
        let input = (0..2_000_000)
            .map(|index| ((index * 193 + index / 7) % 256) as u8)
            .collect::<Vec<_>>();
        let (encoded, _) = encode_exact_dedup(&input, &[input.len() as u64], 5).unwrap();
        assert_eq!(decode_exact_dedup(&encoded, input.len() as u64).unwrap(), input);
    }
}
