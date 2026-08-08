//! Native codec v17: quaternary context transposition.
//!
//! Each byte is four 2-bit symbols. v17 transposes those symbol positions into
//! four independently contiguous lanes and packs four symbols per output byte.
//! The v16 payload is the baseline, while the transformed candidate is sent
//! directly to the v14 representation core to avoid recursively re-running the
//! full transform stack. The envelope is retained only when strictly smaller.

use pithos_core::{PithosError, Result};

pub const NATIVE_CODEC_ID: u16 = 4;
pub const NATIVE_CODEC_VERSION: u16 = 17;
const MAGIC: &[u8; 4] = b"PN17";
const HEADER_LEN: usize = 32;
const LANES: usize = 4;
const MIN_INPUT_BYTES: usize = 256 * 1024;

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
    let (baseline, baseline_stats) =
        pithos_native_v16::encode_exact_dedup(input, member_lengths, level)?;
    let baseline_len = baseline.len() as u64;
    if input.len() < MIN_INPUT_BYTES {
        return Ok((baseline, convert_baseline_stats(baseline_stats, baseline_len)));
    }

    let transformed = transpose_quaternary(input);
    let transformed_lengths = [transformed.len() as u64];
    let (inner, inner_stats) =
        pithos_native_core::encode_exact_dedup(&transformed, &transformed_lengths, level)?;

    let capacity = HEADER_LEN
        .checked_add(inner.len())
        .ok_or(PithosError::IntegerOverflow)?;
    let mut wrapped = Vec::with_capacity(capacity);
    wrapped.extend_from_slice(MAGIC);
    wrapped.extend_from_slice(&NATIVE_CODEC_VERSION.to_le_bytes());
    wrapped.extend_from_slice(&(LANES as u16).to_le_bytes());
    wrapped.extend_from_slice(&(input.len() as u64).to_le_bytes());
    wrapped.extend_from_slice(&(transformed.len() as u64).to_le_bytes());
    wrapped.extend_from_slice(&(inner.len() as u64).to_le_bytes());
    wrapped.extend_from_slice(&inner);

    if wrapped.len() as u64 >= baseline_len {
        return Ok((baseline, convert_baseline_stats(baseline_stats, baseline_len)));
    }
    let encoded_bytes = wrapped.len() as u64;
    Ok((
        wrapped,
        NativeStats {
            chunk_count: inner_stats.chunk_count,
            canonical_chunks: inner_stats.canonical_chunks,
            gross_duplicate_bytes: inner_stats.gross_duplicate_bytes,
            representation_bytes: transformed.len() as u64,
            encoded_bytes,
        },
    ))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    if payload.len() < HEADER_LEN || &payload[..4] != MAGIC {
        return pithos_native_v16::decode_exact_dedup(payload, expected_len);
    }
    let version = read_u16(payload, 4)?;
    let lanes = read_u16(payload, 6)? as usize;
    let original_len = read_u64(payload, 8)?;
    let transformed_len = read_u64(payload, 16)?;
    let inner_len = read_u64(payload, 24)?;
    if version != NATIVE_CODEC_VERSION || lanes != LANES || original_len != expected_len {
        return Err(PithosError::InvalidMetadata("native v17 header"));
    }
    let inner_len = usize::try_from(inner_len).map_err(|_| PithosError::MemoryLimit)?;
    let end = HEADER_LEN
        .checked_add(inner_len)
        .ok_or(PithosError::IntegerOverflow)?;
    if end != payload.len() {
        return Err(PithosError::InvalidRange);
    }
    let transformed = pithos_native_core::decode_exact_dedup(
        payload.get(HEADER_LEN..end).ok_or(PithosError::InvalidRange)?,
        transformed_len,
    )?;
    inverse_quaternary(&transformed, expected_len)
}

fn transpose_quaternary(input: &[u8]) -> Vec<u8> {
    let bytes_per_lane = input.len().div_ceil(4);
    let mut output = vec![0_u8; bytes_per_lane * LANES];
    for (index, byte) in input.iter().copied().enumerate() {
        for lane in 0..LANES {
            let symbol = (byte >> (6 - lane * 2)) & 0x03;
            let packed_index = lane * bytes_per_lane + index / 4;
            output[packed_index] |= symbol << (6 - (index % 4) * 2);
        }
    }
    output
}

fn inverse_quaternary(transformed: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    let expected = usize::try_from(expected_len).map_err(|_| PithosError::MemoryLimit)?;
    let bytes_per_lane = expected.div_ceil(4);
    let required = bytes_per_lane
        .checked_mul(LANES)
        .ok_or(PithosError::IntegerOverflow)?;
    if transformed.len() != required {
        return Err(PithosError::InvalidRange);
    }
    let mut output = vec![0_u8; expected];
    for index in 0..expected {
        let mut byte = 0_u8;
        for lane in 0..LANES {
            let packed = transformed[lane * bytes_per_lane + index / 4];
            let symbol = (packed >> (6 - (index % 4) * 2)) & 0x03;
            byte |= symbol << (6 - lane * 2);
        }
        output[index] = byte;
    }
    Ok(output)
}

fn convert_baseline_stats(
    stats: pithos_native_v16::NativeStats,
    encoded_bytes: u64,
) -> NativeStats {
    NativeStats {
        chunk_count: stats.chunk_count,
        canonical_chunks: stats.canonical_chunks,
        gross_duplicate_bytes: stats.gross_duplicate_bytes,
        representation_bytes: stats.representation_bytes,
        encoded_bytes,
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes.get(offset..offset + 2).ok_or(PithosError::InvalidRange)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes.get(offset..offset + 8).ok_or(PithosError::InvalidRange)?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quaternary_transpose_roundtrips_arbitrary_length() {
        let input = (0..1003).map(|value| (value * 37) as u8).collect::<Vec<_>>();
        let transformed = transpose_quaternary(&input);
        assert_eq!(
            inverse_quaternary(&transformed, input.len() as u64).unwrap(),
            input
        );
    }

    #[test]
    fn codec_falls_back_or_roundtrips() {
        let input = b"{\"agent\":\"pithos\",\"value\":123}\n".repeat(64 * 1024);
        let lengths = [input.len() as u64];
        let (payload, _) = encode_exact_dedup(&input, &lengths, 15).unwrap();
        assert_eq!(decode_exact_dedup(&payload, input.len() as u64).unwrap(), input);
    }
}
