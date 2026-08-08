//! Native codec v18: historical floor selector.
//!
//! Representation experiments must not silently discard a native encoding that
//! already proved useful. v18 races the current R3 representation stack (v17)
//! against the R2 v12 native codec and returns the smaller complete payload.
//! The decoder delegates to v17, whose compatibility chain reaches v12.

use pithos_core::Result;

pub const NATIVE_CODEC_ID: u16 = 4;
pub const NATIVE_CODEC_VERSION: u16 = 18;

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
    let (current_result, floor_result) = std::thread::scope(|scope| {
        let current = scope.spawn(|| {
            pithos_native_current::encode_exact_dedup(input, member_lengths, level)
        });
        let floor = scope.spawn(|| {
            pithos_native_floor::encode_exact_dedup(input, member_lengths, level)
        });
        (current.join(), floor.join())
    });
    let current = current_result
        .map_err(|_| pithos_core::PithosError::InvalidMetadata("native current worker panic"))??;
    let floor = floor_result
        .map_err(|_| pithos_core::PithosError::InvalidMetadata("native floor worker panic"))??;

    if floor.0.len() < current.0.len() {
        let len = floor.0.len() as u64;
        Ok((
            floor.0,
            NativeStats {
                chunk_count: floor.1.chunk_count,
                canonical_chunks: floor.1.canonical_chunks,
                gross_duplicate_bytes: floor.1.gross_duplicate_bytes,
                representation_bytes: floor.1.representation_bytes,
                encoded_bytes: len,
            },
        ))
    } else {
        let len = current.0.len() as u64;
        Ok((
            current.0,
            NativeStats {
                chunk_count: current.1.chunk_count,
                canonical_chunks: current.1.canonical_chunks,
                gross_duplicate_bytes: current.1.gross_duplicate_bytes,
                representation_bytes: current.1.representation_bytes,
                encoded_bytes: len,
            },
        ))
    }
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    pithos_native_current::decode_exact_dedup(payload, expected_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_selector_roundtrips() {
        let member = b"floor-selector-payload".repeat(64 * 1024);
        let mut input = member.clone();
        input.extend_from_slice(&member);
        let lengths = [member.len() as u64, member.len() as u64];
        let (payload, _) = encode_exact_dedup(&input, &lengths, 15).unwrap();
        assert_eq!(decode_exact_dedup(&payload, input.len() as u64).unwrap(), input);
    }
}
