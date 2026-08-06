use std::io::{self, Read};

use pithos_core::{DecodeLimits, PithosError, Result};
use rayon::prelude::*;
use smallvec::SmallVec;
use xxhash_rust::xxh3::Xxh3;

use crate::{LogicalChunk, chunking::try_sort_by_checkpoint};

const MIB: u64 = 1024 * 1024;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const ROLLING_BASE: u64 = 0x0000_0100_0000_01b3;
const FINGERPRINT_WORKER_SCRATCH_BYTES: u64 = 16 * 1024;
const FINGERPRINT_FIXED_WORKING_BYTES: u64 = 4 * 1024;
const FINGERPRINT_PER_CHUNK_MARGIN_BYTES: u64 = 64;

type CompactKey = (u64, u32, [u8; 16]);
type CollisionCandidate = (CompactKey, usize);

/// Controls when the full 256-bit BLAKE3 value is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FullHashPolicy {
    /// Retain full hashes only for compact-identity collision candidates.
    #[default]
    Standard,
    /// Retain a full hash for every logical chunk.
    Paranoid,
}

/// Normative fingerprint parameters and resource bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FingerprintConfig {
    pub full_hash_policy: FullHashPolicy,
    pub subchunk_count: u8,
    pub superfeature_count: u8,
    pub rolling_window: u16,
    pub max_chunk_bytes: u64,
    pub max_chunks: u64,
    pub max_total_bytes: u64,
    pub max_metadata_bytes: u64,
    /// Bounds heap working memory owned by a fingerprint operation.
    pub max_working_bytes: u64,
    pub parallelism: u16,
}

impl Default for FingerprintConfig {
    fn default() -> Self {
        let limits = DecodeLimits::default();
        Self {
            full_hash_policy: FullHashPolicy::Standard,
            subchunk_count: 12,
            superfeature_count: 4,
            rolling_window: 48,
            max_chunk_bytes: 4 * MIB,
            max_chunks: limits.max_chunks,
            max_total_bytes: limits.max_original_bytes,
            max_metadata_bytes: limits.max_metadata_bytes,
            max_working_bytes: limits.max_metadata_bytes,
            parallelism: 64,
        }
    }
}

impl FingerprintConfig {
    pub fn validate(&self) -> Result<()> {
        if !(4..=64).contains(&self.subchunk_count)
            || !matches!(self.superfeature_count, 3 | 4)
            || !self.subchunk_count.is_multiple_of(self.superfeature_count)
            || !(1..=4096).contains(&self.rolling_window)
            || self.max_chunk_bytes == 0
            || self.max_chunk_bytes > 4 * MIB
            || self.max_chunks == 0
            || self.max_total_bytes == 0
            || self.max_metadata_bytes == 0
            || self.max_working_bytes == 0
            || !(1..=64).contains(&self.parallelism)
        {
            return Err(PithosError::InvalidMetadata("fingerprint configuration"));
        }
        Ok(())
    }
}

/// All analysis values associated with one logical chunk.
///
/// Equality of this structure is never sufficient to authorize deduplication;
/// the exact-dedup phase must still confirm the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkFingerprint {
    pub chunk_id: u64,
    pub length: u32,
    pub xxh3: u64,
    pub blake3_128: [u8; 16],
    pub full_blake3: Option<[u8; 32]>,
    pub crc32c: u32,
    pub superfeatures: SmallVec<[u64; 8]>,
}

/// Binds a logical descriptor to the exact bytes fingerprinted for it.
#[derive(Debug, Clone, Copy)]
pub struct FingerprintInput<'a> {
    pub chunk: &'a LogicalChunk,
    pub data: &'a [u8],
}

impl ChunkFingerprint {
    /// Convenience API for one standard-policy in-memory chunk.
    pub fn compute(chunk_id: u64, data: &[u8]) -> Result<Self> {
        Self::compute_with_config(chunk_id, data, &FingerprintConfig::default())
    }

    pub fn compute_with_config(
        chunk_id: u64,
        data: &[u8],
        config: &FingerprintConfig,
    ) -> Result<Self> {
        config.validate()?;
        ensure_single_request_limits(data.len(), config)?;
        fingerprint_bytes(chunk_id, data, config, &|| Ok(()))
    }

    /// Atomically retains the full hash after revalidating the compact identity.
    pub fn escalate_full_blake3(&mut self, data: &[u8]) -> Result<()> {
        self.escalate_full_blake3_with_config(data, &FingerprintConfig::default())
    }

    /// Configured, bounded variant of [`Self::escalate_full_blake3`].
    pub fn escalate_full_blake3_with_config(
        &mut self,
        data: &[u8],
        config: &FingerprintConfig,
    ) -> Result<()> {
        self.escalate_full_blake3_with_checkpoint(data, config, &|| Ok(()))
    }

    /// Bounded promotion with cooperative checkpoints between 64 KiB blocks.
    pub fn escalate_full_blake3_with_checkpoint<F>(
        &mut self,
        data: &[u8],
        config: &FingerprintConfig,
        checkpoint: &F,
    ) -> Result<()>
    where
        F: Fn() -> Result<()> + Sync,
    {
        config.validate()?;
        let length = u32::try_from(data.len()).map_err(|_| PithosError::IntegerOverflow)?;
        if length != self.length {
            return Err(PithosError::InvalidMetadata("fingerprint chunk length"));
        }
        ensure_single_request_limits(data.len(), config)?;

        let mut xxh3 = Xxh3::new();
        let mut blake3 = blake3::Hasher::new();
        let mut crc32c = 0_u32;
        for block in data.chunks(READ_BUFFER_BYTES) {
            checkpoint()?;
            xxh3.update(block);
            blake3.update(block);
            crc32c = crc32c::crc32c_append(crc32c, block);
        }
        checkpoint()?;
        let full = blake3.finalize();
        let mut compact = [0_u8; 16];
        compact.copy_from_slice(&full.as_bytes()[..16]);
        if compact != self.blake3_128 || xxh3.digest() != self.xxh3 || crc32c != self.crc32c {
            return Err(PithosError::HashMismatch);
        }
        self.full_blake3 = Some(*full.as_bytes());
        Ok(())
    }

    fn compact_key(&self) -> CompactKey {
        (self.xxh3, self.length, self.blake3_128)
    }
}

/// Streaming fingerprinting of a reader positioned at one logical chunk.
pub fn fingerprint_reader<R: Read>(
    chunk: &LogicalChunk,
    reader: R,
    config: &FingerprintConfig,
) -> Result<ChunkFingerprint> {
    fingerprint_reader_with_checkpoint(chunk, reader, config, &|| Ok(()))
}

/// Streaming variant with a thread-safe cooperative checkpoint before reads.
pub fn fingerprint_reader_with_checkpoint<R, F>(
    chunk: &LogicalChunk,
    mut reader: R,
    config: &FingerprintConfig,
    checkpoint: &F,
) -> Result<ChunkFingerprint>
where
    R: Read,
    F: Fn() -> Result<()> + Sync,
{
    config.validate()?;
    let expected = u64::from(chunk.length);
    ensure_single_request_limits_u64(expected, config)?;
    let mut accumulator = FingerprintAccumulator::new(expected, config)?;
    let mut remaining = expected;
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    while remaining > 0 {
        checkpoint()?;
        let request = usize::try_from(remaining.min(READ_BUFFER_BYTES as u64))
            .map_err(|_| PithosError::IntegerOverflow)?;
        match reader.read(&mut buffer[..request]) {
            Ok(0) => return Err(PithosError::InvalidMetadata("short fingerprint stream")),
            Ok(read) => {
                accumulator.update(&buffer[..read])?;
                remaining = remaining
                    .checked_sub(u64::try_from(read).map_err(|_| PithosError::IntegerOverflow)?)
                    .ok_or(PithosError::IntegerOverflow)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(PithosError::Io(error)),
        }
    }
    let mut trailing = [0_u8; 1];
    loop {
        checkpoint()?;
        match reader.read(&mut trailing) {
            Ok(0) => break,
            Ok(_) => return Err(PithosError::InvalidMetadata("trailing fingerprint data")),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(PithosError::Io(error)),
        }
    }
    accumulator.finish(chunk.chunk_id, config)
}

/// Parallel, bounded fingerprinting with canonical `chunk_id` ordering.
pub fn fingerprint_chunks(
    inputs: &[FingerprintInput<'_>],
    config: &FingerprintConfig,
) -> Result<Vec<ChunkFingerprint>> {
    fingerprint_chunks_with_checkpoint(inputs, config, &|| Ok(()))
}

pub fn fingerprint_chunks_with_checkpoint<F>(
    inputs: &[FingerprintInput<'_>],
    config: &FingerprintConfig,
    checkpoint: &F,
) -> Result<Vec<ChunkFingerprint>>
where
    F: Fn() -> Result<()> + Sync,
{
    config.validate()?;
    checkpoint()?;
    let count = u64::try_from(inputs.len()).map_err(|_| PithosError::IntegerOverflow)?;
    if count > config.max_chunks {
        return Err(PithosError::ResourceLimit("fingerprint chunk count"));
    }
    ensure_batch_memory_limits(count, config)?;

    let mut ordered = Vec::new();
    ordered
        .try_reserve_exact(inputs.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    let mut total_bytes = 0_u64;
    for input in inputs {
        checkpoint()?;
        let actual = u64::try_from(input.data.len()).map_err(|_| PithosError::IntegerOverflow)?;
        if actual != u64::from(input.chunk.length) {
            return Err(PithosError::InvalidMetadata("fingerprint chunk length"));
        }
        ensure_chunk_size_u64(actual, config)?;
        total_bytes = total_bytes
            .checked_add(actual)
            .ok_or(PithosError::IntegerOverflow)?;
        if total_bytes > config.max_total_bytes {
            return Err(PithosError::ResourceLimit("fingerprint total bytes"));
        }
        ordered.push(input);
    }
    let mut sort_checkpoint = || checkpoint();
    try_sort_by_checkpoint(
        &mut ordered,
        |left, right| left.chunk.chunk_id.cmp(&right.chunk.chunk_id),
        &mut sort_checkpoint,
    )?;
    for pair in ordered.windows(2) {
        checkpoint()?;
        if pair[0].chunk.chunk_id == pair[1].chunk.chunk_id {
            return Err(PithosError::InvalidMetadata(
                "duplicate fingerprint chunk ID",
            ));
        }
    }
    if ordered.is_empty() {
        return Ok(Vec::new());
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(usize::from(config.parallelism).min(ordered.len()).max(1))
        .build()
        .map_err(|_| PithosError::ResourceLimit("fingerprint parallelism"))?;
    let mut fingerprints = pool.install(|| {
        ordered
            .par_iter()
            .map(|input| {
                checkpoint().and_then(|()| {
                    fingerprint_bytes(input.chunk.chunk_id, input.data, config, checkpoint)
                })
            })
            .collect::<Result<Vec<_>>>()
    })?;
    checkpoint()?;
    if config.full_hash_policy == FullHashPolicy::Standard {
        escalate_compact_collisions(&mut fingerprints, &ordered, config, checkpoint)?;
    }
    Ok(fingerprints)
}

fn fingerprint_bytes<F>(
    chunk_id: u64,
    data: &[u8],
    config: &FingerprintConfig,
    checkpoint: &F,
) -> Result<ChunkFingerprint>
where
    F: Fn() -> Result<()> + Sync,
{
    ensure_chunk_size(data.len(), config)?;
    let expected = u64::try_from(data.len()).map_err(|_| PithosError::IntegerOverflow)?;
    let mut accumulator = FingerprintAccumulator::new(expected, config)?;
    for block in data.chunks(READ_BUFFER_BYTES) {
        checkpoint()?;
        accumulator.update(block)?;
    }
    accumulator.finish(chunk_id, config)
}

fn escalate_compact_collisions<F>(
    fingerprints: &mut [ChunkFingerprint],
    inputs: &[&FingerprintInput<'_>],
    config: &FingerprintConfig,
    checkpoint: &F,
) -> Result<()>
where
    F: Fn() -> Result<()> + Sync,
{
    let mut candidates = Vec::new();
    candidates
        .try_reserve_exact(fingerprints.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    for (index, fingerprint) in fingerprints.iter().enumerate() {
        checkpoint()?;
        candidates.push((fingerprint.compact_key(), index));
    }
    let mut sort_checkpoint = || checkpoint();
    try_sort_by_checkpoint(
        &mut candidates,
        |left, right| left.cmp(right),
        &mut sort_checkpoint,
    )?;
    let mut escalate = Vec::new();
    escalate
        .try_reserve_exact(fingerprints.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    escalate.resize(fingerprints.len(), false);
    let mut start = 0_usize;
    while start < candidates.len() {
        checkpoint()?;
        let mut end = start + 1;
        while end < candidates.len() && candidates[end].0 == candidates[start].0 {
            checkpoint()?;
            end += 1;
        }
        if end - start > 1 {
            for candidate in &candidates[start..end] {
                checkpoint()?;
                escalate[candidate.1] = true;
            }
        }
        start = end;
    }
    for ((fingerprint, input), should_escalate) in fingerprints.iter_mut().zip(inputs).zip(escalate)
    {
        checkpoint()?;
        if should_escalate {
            fingerprint.escalate_full_blake3_with_checkpoint(input.data, config, checkpoint)?;
        }
    }
    Ok(())
}

struct FingerprintAccumulator {
    xxh3: Xxh3,
    blake3: blake3::Hasher,
    crc32c: u32,
    superfeatures: SuperfeatureAccumulator,
    bytes_seen: u64,
}

impl FingerprintAccumulator {
    fn new(expected: u64, config: &FingerprintConfig) -> Result<Self> {
        Ok(Self {
            xxh3: Xxh3::new(),
            blake3: blake3::Hasher::new(),
            crc32c: 0,
            superfeatures: SuperfeatureAccumulator::new(expected, config)?,
            bytes_seen: 0,
        })
    }

    fn update(&mut self, bytes: &[u8]) -> Result<()> {
        self.xxh3.update(bytes);
        self.blake3.update(bytes);
        self.crc32c = crc32c::crc32c_append(self.crc32c, bytes);
        self.superfeatures.update(bytes)?;
        self.bytes_seen = self
            .bytes_seen
            .checked_add(u64::try_from(bytes.len()).map_err(|_| PithosError::IntegerOverflow)?)
            .ok_or(PithosError::IntegerOverflow)?;
        Ok(())
    }

    fn finish(self, chunk_id: u64, config: &FingerprintConfig) -> Result<ChunkFingerprint> {
        if self.bytes_seen != self.superfeatures.total_len {
            return Err(PithosError::InvalidMetadata("fingerprint byte count"));
        }
        let length = u32::try_from(self.bytes_seen).map_err(|_| PithosError::IntegerOverflow)?;
        let full = self.blake3.finalize();
        let mut blake3_128 = [0_u8; 16];
        blake3_128.copy_from_slice(&full.as_bytes()[..16]);
        Ok(ChunkFingerprint {
            chunk_id,
            length,
            xxh3: self.xxh3.digest(),
            blake3_128,
            full_blake3: (config.full_hash_policy == FullHashPolicy::Paranoid)
                .then_some(*full.as_bytes()),
            crc32c: self.crc32c,
            superfeatures: self.superfeatures.finish(config)?,
        })
    }
}

struct SuperfeatureAccumulator {
    total_len: u64,
    position: u64,
    region_ends: Vec<u64>,
    regions: Vec<RollingRegion>,
    current_region: usize,
}

impl SuperfeatureAccumulator {
    fn new(total_len: u64, config: &FingerprintConfig) -> Result<Self> {
        let count = usize::from(config.subchunk_count);
        let mut region_ends = Vec::new();
        let mut regions = Vec::new();
        region_ends
            .try_reserve_exact(count)
            .map_err(|_| PithosError::MemoryLimit)?;
        regions
            .try_reserve_exact(count)
            .map_err(|_| PithosError::MemoryLimit)?;
        for index in 0..count {
            let start = total_len
                .checked_mul(u64::try_from(index).map_err(|_| PithosError::IntegerOverflow)?)
                .and_then(|value| value.checked_div(u64::from(config.subchunk_count)))
                .ok_or(PithosError::IntegerOverflow)?;
            let end = total_len
                .checked_mul(u64::try_from(index + 1).map_err(|_| PithosError::IntegerOverflow)?)
                .and_then(|value| value.checked_div(u64::from(config.subchunk_count)))
                .ok_or(PithosError::IntegerOverflow)?;
            region_ends.push(end);
            regions.push(RollingRegion::new(end - start, config.rolling_window)?);
        }
        Ok(Self {
            total_len,
            position: 0,
            region_ends,
            regions,
            current_region: 0,
        })
    }

    fn update(&mut self, bytes: &[u8]) -> Result<()> {
        for &byte in bytes {
            while self.current_region < self.region_ends.len()
                && self.position >= self.region_ends[self.current_region]
            {
                self.current_region += 1;
            }
            if self.current_region >= self.regions.len() {
                return Err(PithosError::InvalidMetadata("superfeature byte count"));
            }
            self.regions[self.current_region].push(byte)?;
            self.position = self
                .position
                .checked_add(1)
                .ok_or(PithosError::IntegerOverflow)?;
        }
        Ok(())
    }

    fn finish(self, config: &FingerprintConfig) -> Result<SmallVec<[u64; 8]>> {
        if self.position != self.total_len {
            return Err(PithosError::InvalidMetadata("superfeature byte count"));
        }
        let mut output = SmallVec::new();
        if self.total_len == 0 {
            return Ok(output);
        }
        let group_count = usize::from(config.superfeature_count);
        for group in 0..group_count {
            let start = group * self.regions.len() / group_count;
            let end = (group + 1) * self.regions.len() / group_count;
            let mut hasher = Xxh3::new();
            for region in &self.regions[start..end] {
                hasher.update(&region.maximum.to_le_bytes());
            }
            output.push(hasher.digest());
        }
        Ok(output)
    }
}

struct RollingRegion {
    window: usize,
    ring: Vec<u8>,
    cursor: usize,
    hash: u64,
    base_power: u64,
    maximum: u64,
    bytes_seen: u64,
    expected: u64,
}

impl RollingRegion {
    fn new(expected: u64, configured_window: u16) -> Result<Self> {
        let window = usize::try_from(expected.min(u64::from(configured_window)))
            .map_err(|_| PithosError::IntegerOverflow)?;
        let mut ring = Vec::new();
        ring.try_reserve_exact(window)
            .map_err(|_| PithosError::MemoryLimit)?;
        let mut base_power = 1_u64;
        for _ in 0..window {
            base_power = base_power.wrapping_mul(ROLLING_BASE);
        }
        Ok(Self {
            window,
            ring,
            cursor: 0,
            hash: 0,
            base_power,
            maximum: 0,
            bytes_seen: 0,
            expected,
        })
    }

    fn push(&mut self, byte: u8) -> Result<()> {
        if self.bytes_seen >= self.expected || self.window == 0 {
            return Err(PithosError::InvalidMetadata("superfeature region length"));
        }
        let value = u64::from(byte) + 1;
        if self.ring.len() < self.window {
            self.ring.push(byte);
            self.hash = self.hash.wrapping_mul(ROLLING_BASE).wrapping_add(value);
            if self.ring.len() == self.window {
                self.maximum = self.hash;
            }
        } else {
            let removed = u64::from(self.ring[self.cursor]) + 1;
            self.ring[self.cursor] = byte;
            self.cursor = (self.cursor + 1) % self.window;
            self.hash = self
                .hash
                .wrapping_mul(ROLLING_BASE)
                .wrapping_add(value)
                .wrapping_sub(removed.wrapping_mul(self.base_power));
            self.maximum = self.maximum.max(self.hash);
        }
        self.bytes_seen = self
            .bytes_seen
            .checked_add(1)
            .ok_or(PithosError::IntegerOverflow)?;
        Ok(())
    }
}

fn ensure_chunk_size(length: usize, config: &FingerprintConfig) -> Result<()> {
    let length = u64::try_from(length).map_err(|_| PithosError::IntegerOverflow)?;
    ensure_chunk_size_u64(length, config)
}

fn ensure_single_request_limits(length: usize, config: &FingerprintConfig) -> Result<()> {
    let length = u64::try_from(length).map_err(|_| PithosError::IntegerOverflow)?;
    ensure_single_request_limits_u64(length, config)
}

fn ensure_single_request_limits_u64(length: u64, config: &FingerprintConfig) -> Result<()> {
    ensure_chunk_size_u64(length, config)?;
    if length > config.max_total_bytes {
        return Err(PithosError::ResourceLimit("fingerprint total bytes"));
    }
    ensure_batch_memory_limits(1, config)
}

fn ensure_batch_memory_limits(count: u64, config: &FingerprintConfig) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    let metadata_bytes = count
        .checked_mul(
            u64::try_from(std::mem::size_of::<ChunkFingerprint>())
                .map_err(|_| PithosError::IntegerOverflow)?,
        )
        .ok_or(PithosError::IntegerOverflow)?;
    if metadata_bytes > config.max_metadata_bytes {
        return Err(PithosError::ResourceLimit("fingerprint metadata bytes"));
    }

    // At collision analysis these vectors can coexist: canonical input refs,
    // output fingerprints, collision candidates, and the escalation bitmap.
    // The explicit margin covers allocator rounding and Vec bookkeeping.
    let per_chunk = std::mem::size_of::<&FingerprintInput<'static>>()
        .checked_add(std::mem::size_of::<ChunkFingerprint>())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<CollisionCandidate>()))
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<bool>()))
        .and_then(|bytes| {
            bytes.checked_add(usize::try_from(FINGERPRINT_PER_CHUNK_MARGIN_BYTES).ok()?)
        })
        .ok_or(PithosError::IntegerOverflow)?;
    let per_chunk = u64::try_from(per_chunk).map_err(|_| PithosError::IntegerOverflow)?;
    let workers = count.min(u64::from(config.parallelism));
    let working_bytes = count
        .checked_mul(per_chunk)
        .and_then(|bytes| {
            workers
                .checked_mul(FINGERPRINT_WORKER_SCRATCH_BYTES)
                .and_then(|scratch| bytes.checked_add(scratch))
        })
        .and_then(|bytes| bytes.checked_add(FINGERPRINT_FIXED_WORKING_BYTES))
        .ok_or(PithosError::IntegerOverflow)?;
    if working_bytes > config.max_working_bytes {
        return Err(PithosError::ResourceLimit("fingerprint working bytes"));
    }
    Ok(())
}

fn ensure_chunk_size_u64(length: u64, config: &FingerprintConfig) -> Result<()> {
    if length > u64::from(u32::MAX) {
        return Err(PithosError::IntegerOverflow);
    }
    if length > config.max_chunk_bytes {
        return Err(PithosError::ResourceLimit("fingerprint chunk bytes"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChunkingMethod;

    fn chunk(chunk_id: u64, length: usize) -> LogicalChunk {
        LogicalChunk {
            chunk_id,
            entry_id: chunk_id,
            object_id: 0,
            logical_offset: 0,
            length: u32::try_from(length).unwrap(),
            method: ChunkingMethod::FastCdcV2020,
        }
    }

    #[test]
    fn synthetic_compact_collision_fails_closed_before_publication() {
        let left = b"collision-a";
        let right = b"collision-b";
        assert_ne!(blake3::hash(left), blake3::hash(right));

        let chunks = [chunk(1, left.len()), chunk(2, right.len())];
        let inputs = [
            FingerprintInput {
                chunk: &chunks[0],
                data: left,
            },
            FingerprintInput {
                chunk: &chunks[1],
                data: right,
            },
        ];
        let config = FingerprintConfig::default();
        let mut fingerprints = vec![
            ChunkFingerprint::compute(1, left).unwrap(),
            ChunkFingerprint::compute(2, right).unwrap(),
        ];
        fingerprints[1].xxh3 = fingerprints[0].xxh3;
        fingerprints[1].blake3_128 = fingerprints[0].blake3_128;
        fingerprints[1].crc32c = fingerprints[0].crc32c;

        let error = escalate_compact_collisions(
            &mut fingerprints,
            &[&inputs[0], &inputs[1]],
            &config,
            &|| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(error, PithosError::HashMismatch));
        assert_eq!(
            fingerprints[0].full_blake3,
            Some(*blake3::hash(left).as_bytes())
        );
        assert!(fingerprints[1].full_blake3.is_none());
    }
}
