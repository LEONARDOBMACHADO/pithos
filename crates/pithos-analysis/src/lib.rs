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
    pub fn compute(chunk_id: u64, data: &[u8]) -> Self {
        let xxh3 = xxhash_rust::xxh3::xxh3_64(data);
        let hash = blake3::hash(data);
        let mut blake3_128 = [0u8; 16];
        blake3_128.copy_from_slice(&hash.as_bytes()[..16]);
        let crc32c = crc32c::crc32c(data);

        Self {
            chunk_id,
            length: data.len() as u32,
            xxh3,
            blake3_128,
            crc32c,
        }
    }
}
