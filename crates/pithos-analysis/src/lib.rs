//! Format-neutral logical chunking and analysis primitives for Pithos.

mod chunking;
mod micro_file;

pub use chunking::*;
pub use micro_file::*;

pub struct ChunkFingerprint {
    pub chunk_id: u64,
    pub length: u32,
    pub xxh3: u64,
    pub blake3_128: [u8; 16],
    pub crc32c: u32,
}

impl ChunkFingerprint {
    pub fn compute(chunk_id: u64, data: &[u8]) -> pithos_core::Result<Self> {
        let length =
            u32::try_from(data.len()).map_err(|_| pithos_core::PithosError::IntegerOverflow)?;
        let xxh3 = xxhash_rust::xxh3::xxh3_64(data);
        let hash = blake3::hash(data);
        let mut blake3_128 = [0u8; 16];
        blake3_128.copy_from_slice(&hash.as_bytes()[..16]);
        let crc32c = crc32c::crc32c(data);

        Ok(Self {
            chunk_id,
            length,
            xxh3,
            blake3_128,
            crc32c,
        })
    }
}
