use std::{cmp::Ordering, mem::size_of, sync::OnceLock};

use pithos_core::{DecodeLimits, PithosError, Result};
use rayon::prelude::*;

use crate::{ChunkFingerprint, LogicalChunk, chunking::try_sort_by_checkpoint};

const READ_BLOCK_BYTES: usize = 64 * 1024;
const DEDUP_FIXED_WORKING_BYTES: u64 = 4 * 1024;
const DEDUP_PER_CHUNK_MARGIN_BYTES: u64 = 96;

/// Resource and cost bounds for exact deduplication.
///
/// `reference_cost_bytes` is deliberately explicit: a duplicate is only turned
/// into an exact reference when replacing its stored bytes by a future physical
/// reference is strictly beneficial after this metadata cost and the configured
/// minimum margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExactDedupConfig {
    pub max_chunks: u64,
    pub max_total_bytes: u64,
    pub max_working_bytes: u64,
    pub reference_cost_bytes: u32,
    pub min_net_savings_bytes: u32,
    pub parallelism: u16,
}

impl Default for ExactDedupConfig {
    fn default() -> Self {
        let limits = DecodeLimits::default();
        Self {
            max_chunks: limits.max_chunks,
            max_total_bytes: limits.max_original_bytes,
            max_working_bytes: limits.max_metadata_bytes,
            reference_cost_bytes: 16,
            min_net_savings_bytes: 1,
            parallelism: 64,
        }
    }
}

impl ExactDedupConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_chunks == 0
            || self.max_total_bytes == 0
            || self.max_working_bytes == 0
            || self.reference_cost_bytes == 0
            || !(1..=64).contains(&self.parallelism)
        {
            return Err(PithosError::InvalidMetadata("exact dedup configuration"));
        }
        Ok(())
    }
}

/// Binds one logical chunk to the fingerprint and exact bytes produced by the
/// preceding analysis stages.
#[derive(Debug, Clone, Copy)]
pub struct DedupInput<'a> {
    pub chunk: &'a LogicalChunk,
    pub fingerprint: &'a ChunkFingerprint,
    pub data: &'a [u8],
}

/// Storage decision for one logical chunk.
///
/// A record that references itself is canonical and stores bytes. A record that
/// references a different chunk may share physical storage only after the PAF
/// persistence layer writes the corresponding exact-reference metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExactDedupRecord {
    pub chunk_id: u64,
    pub canonical_chunk_id: u64,
    pub length: u32,
    pub reference_cost_bytes: u32,
    pub net_saved_bytes: u64,
}

impl ExactDedupRecord {
    pub const fn is_reference(self) -> bool {
        self.chunk_id != self.canonical_chunk_id
    }
}

/// Deterministic, format-neutral result of exact deduplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactDedupPlan {
    pub records: Vec<ExactDedupRecord>,
    pub canonical_chunks: u64,
    pub referenced_chunks: u64,
    pub gross_duplicate_bytes: u64,
    pub reference_bytes: u64,
    pub net_saved_bytes: u64,
}

impl ExactDedupPlan {
    pub fn record(&self, chunk_id: u64) -> Option<&ExactDedupRecord> {
        self.records
            .binary_search_by_key(&chunk_id, |record| record.chunk_id)
            .ok()
            .map(|index| &self.records[index])
    }
}

#[derive(Debug, Clone, Copy)]
struct ShardRange {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy)]
struct FullCandidate<'a> {
    input: DedupInput<'a>,
    full_hash: [u8; 32],
}

/// Exact deduplication using the default configuration.
pub fn exact_dedup(inputs: &[DedupInput<'_>]) -> Result<ExactDedupPlan> {
    exact_dedup_with_config(inputs, &ExactDedupConfig::default())
}

/// Exact deduplication with explicit resource and cost bounds.
pub fn exact_dedup_with_config(
    inputs: &[DedupInput<'_>],
    config: &ExactDedupConfig,
) -> Result<ExactDedupPlan> {
    exact_dedup_with_checkpoint(inputs, config, &|| Ok(()))
}

/// Bounded exact deduplication with cooperative checkpoints. Shards are
/// processed in parallel, but final records are always returned in canonical
/// `chunk_id` order.
pub fn exact_dedup_with_checkpoint<F>(
    inputs: &[DedupInput<'_>],
    config: &ExactDedupConfig,
    checkpoint: &F,
) -> Result<ExactDedupPlan>
where
    F: Fn() -> Result<()> + Sync,
{
    exact_dedup_internal(inputs, config, checkpoint, None)
}

fn exact_dedup_internal<F>(
    inputs: &[DedupInput<'_>],
    config: &ExactDedupConfig,
    checkpoint: &F,
    forced_full_hash: Option<[u8; 32]>,
) -> Result<ExactDedupPlan>
where
    F: Fn() -> Result<()> + Sync,
{
    config.validate()?;
    checkpoint()?;
    let count = u64::try_from(inputs.len()).map_err(|_| PithosError::IntegerOverflow)?;
    if count > config.max_chunks {
        return Err(PithosError::ResourceLimit("exact dedup chunk count"));
    }
    ensure_working_memory(count, config)?;

    let mut total_bytes = 0_u64;
    let mut ordered = Vec::new();
    ordered
        .try_reserve_exact(inputs.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    for input in inputs {
        checkpoint()?;
        validate_input(input)?;
        total_bytes = total_bytes
            .checked_add(u64::from(input.chunk.length))
            .ok_or(PithosError::IntegerOverflow)?;
        if total_bytes > config.max_total_bytes {
            return Err(PithosError::ResourceLimit("exact dedup total bytes"));
        }
        ordered.push(*input);
    }

    if ordered.is_empty() {
        return Ok(ExactDedupPlan {
            records: Vec::new(),
            canonical_chunks: 0,
            referenced_chunks: 0,
            gross_duplicate_bytes: 0,
            reference_bytes: 0,
            net_saved_bytes: 0,
        });
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
            return Err(PithosError::InvalidMetadata("duplicate exact dedup chunk ID"));
        }
    }

    let mut grouped = Vec::new();
    grouped
        .try_reserve_exact(ordered.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    for input in &ordered {
        checkpoint()?;
        grouped.push(*input);
    }
    let mut group_sort_checkpoint = || checkpoint();
    try_sort_by_checkpoint(
        &mut grouped,
        |left, right| grouping_key(left).cmp(&grouping_key(right)),
        &mut group_sort_checkpoint,
    )?;

    let mut shards = Vec::new();
    shards
        .try_reserve_exact(grouped.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    let mut start = 0_usize;
    while start < grouped.len() {
        checkpoint()?;
        let xxh3 = grouped[start].fingerprint.xxh3;
        let mut end = start + 1;
        while end < grouped.len() && grouped[end].fingerprint.xxh3 == xxh3 {
            checkpoint()?;
            end += 1;
        }
        shards.push(ShardRange { start, end });
        start = end;
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(usize::from(config.parallelism).min(shards.len()).max(1))
        .build()
        .map_err(|_| PithosError::ResourceLimit("exact dedup parallelism"))?;
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(shards.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    for _ in 0..shards.len() {
        slots.push(OnceLock::<Result<Vec<ExactDedupRecord>>>::new());
    }

    pool.install(|| {
        slots
            .par_iter()
            .zip(shards.par_iter())
            .for_each(|(slot, shard)| {
                let result = checkpoint().and_then(|()| {
                    process_shard(
                        &grouped[shard.start..shard.end],
                        config,
                        checkpoint,
                        forced_full_hash,
                    )
                });
                let _ = slot.set(result);
            });
    });

    let mut records = Vec::new();
    records
        .try_reserve_exact(ordered.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    for slot in slots {
        checkpoint()?;
        let shard_records = slot.into_inner().ok_or(PithosError::InvalidMetadata(
            "missing exact dedup worker result",
        ))??;
        for record in shard_records {
            checkpoint()?;
            records.push(record);
        }
    }

    let mut record_sort_checkpoint = || checkpoint();
    try_sort_by_checkpoint(
        &mut records,
        |left, right| left.chunk_id.cmp(&right.chunk_id),
        &mut record_sort_checkpoint,
    )?;
    if records.len() != ordered.len() {
        return Err(PithosError::InvalidMetadata("exact dedup record coverage"));
    }

    let mut canonical_chunks = 0_u64;
    let mut referenced_chunks = 0_u64;
    let mut gross_duplicate_bytes = 0_u64;
    let mut reference_bytes = 0_u64;
    let mut net_saved_bytes = 0_u64;
    for record in &records {
        checkpoint()?;
        if record.is_reference() {
            referenced_chunks = referenced_chunks
                .checked_add(1)
                .ok_or(PithosError::IntegerOverflow)?;
            gross_duplicate_bytes = gross_duplicate_bytes
                .checked_add(u64::from(record.length))
                .ok_or(PithosError::IntegerOverflow)?;
            reference_bytes = reference_bytes
                .checked_add(u64::from(record.reference_cost_bytes))
                .ok_or(PithosError::IntegerOverflow)?;
            net_saved_bytes = net_saved_bytes
                .checked_add(record.net_saved_bytes)
                .ok_or(PithosError::IntegerOverflow)?;
        } else {
            canonical_chunks = canonical_chunks
                .checked_add(1)
                .ok_or(PithosError::IntegerOverflow)?;
        }
    }

    let plan = ExactDedupPlan {
        records,
        canonical_chunks,
        referenced_chunks,
        gross_duplicate_bytes,
        reference_bytes,
        net_saved_bytes,
    };
    validate_exact_dedup_plan(&ordered, &plan, config, checkpoint)?;
    Ok(plan)
}

fn process_shard<F>(
    shard: &[DedupInput<'_>],
    config: &ExactDedupConfig,
    checkpoint: &F,
    forced_full_hash: Option<[u8; 32]>,
) -> Result<Vec<ExactDedupRecord>>
where
    F: Fn() -> Result<()> + Sync,
{
    let mut records = Vec::new();
    records
        .try_reserve_exact(shard.len())
        .map_err(|_| PithosError::MemoryLimit)?;

    let mut start = 0_usize;
    while start < shard.len() {
        checkpoint()?;
        let compact = compact_group_key(&shard[start]);
        let mut end = start + 1;
        while end < shard.len() && compact_group_key(&shard[end]) == compact {
            checkpoint()?;
            end += 1;
        }

        if end - start == 1 {
            records.push(canonical_record(&shard[start]));
            start = end;
            continue;
        }

        let mut candidates = Vec::new();
        candidates
            .try_reserve_exact(end - start)
            .map_err(|_| PithosError::MemoryLimit)?;
        for input in &shard[start..end] {
            checkpoint()?;
            candidates.push(FullCandidate {
                input: *input,
                full_hash: compute_full_blake3(input.data, checkpoint, forced_full_hash)?,
            });
        }
        let mut candidate_sort_checkpoint = || checkpoint();
        try_sort_by_checkpoint(
            &mut candidates,
            compare_full_candidates,
            &mut candidate_sort_checkpoint,
        )?;

        let mut exact_start = 0_usize;
        while exact_start < candidates.len() {
            checkpoint()?;
            let mut exact_end = exact_start + 1;
            while exact_end < candidates.len()
                && candidates[exact_end].full_hash == candidates[exact_start].full_hash
                && candidates[exact_end].input.data == candidates[exact_start].input.data
            {
                checkpoint()?;
                exact_end += 1;
            }

            if exact_end - exact_start > 1 {
                let canonical = candidates[exact_start].input;
                if let Some(net_savings) = reference_savings(canonical.chunk.length, config)? {
                    records.push(canonical_record(&canonical));
                    for candidate in &candidates[exact_start + 1..exact_end] {
                        checkpoint()?;
                        records.push(reference_record(
                            &candidate.input,
                            canonical.chunk.chunk_id,
                            config.reference_cost_bytes,
                            net_savings,
                        ));
                    }
                } else {
                    for candidate in &candidates[exact_start..exact_end] {
                        checkpoint()?;
                        records.push(canonical_record(&candidate.input));
                    }
                }
            } else {
                records.push(canonical_record(&candidates[exact_start].input));
            }
            exact_start = exact_end;
        }
        start = end;
    }
    Ok(records)
}

fn compare_full_candidates(left: &FullCandidate<'_>, right: &FullCandidate<'_>) -> Ordering {
    left.full_hash
        .cmp(&right.full_hash)
        .then_with(|| left.input.data.cmp(right.input.data))
        .then_with(|| canonical_key(&left.input).cmp(&canonical_key(&right.input)))
}

fn validate_input(input: &DedupInput<'_>) -> Result<()> {
    let actual = u32::try_from(input.data.len()).map_err(|_| PithosError::IntegerOverflow)?;
    if actual != input.chunk.length
        || input.fingerprint.chunk_id != input.chunk.chunk_id
        || input.fingerprint.length != input.chunk.length
    {
        return Err(PithosError::InvalidMetadata("exact dedup input binding"));
    }
    if let Some(full) = input.fingerprint.full_blake3
        && full[..16] != input.fingerprint.blake3_128
    {
        return Err(PithosError::HashMismatch);
    }
    input
        .chunk
        .logical_offset
        .checked_add(u64::from(input.chunk.length))
        .ok_or(PithosError::IntegerOverflow)?;
    Ok(())
}

fn grouping_key(input: &DedupInput<'_>) -> (u64, u32, [u8; 16], u64, u64, u64, u64) {
    (
        input.fingerprint.xxh3,
        input.chunk.length,
        input.fingerprint.blake3_128,
        input.chunk.entry_id,
        input.chunk.object_id,
        input.chunk.logical_offset,
        input.chunk.chunk_id,
    )
}

fn compact_group_key(input: &DedupInput<'_>) -> (u32, [u8; 16]) {
    (input.chunk.length, input.fingerprint.blake3_128)
}

fn canonical_key(input: &DedupInput<'_>) -> (u64, u64, u64, u64) {
    (
        input.chunk.entry_id,
        input.chunk.object_id,
        input.chunk.logical_offset,
        input.chunk.chunk_id,
    )
}

fn canonical_record(input: &DedupInput<'_>) -> ExactDedupRecord {
    ExactDedupRecord {
        chunk_id: input.chunk.chunk_id,
        canonical_chunk_id: input.chunk.chunk_id,
        length: input.chunk.length,
        reference_cost_bytes: 0,
        net_saved_bytes: 0,
    }
}

fn reference_record(
    input: &DedupInput<'_>,
    canonical_chunk_id: u64,
    reference_cost_bytes: u32,
    net_saved_bytes: u64,
) -> ExactDedupRecord {
    ExactDedupRecord {
        chunk_id: input.chunk.chunk_id,
        canonical_chunk_id,
        length: input.chunk.length,
        reference_cost_bytes,
        net_saved_bytes,
    }
}

fn reference_savings(length: u32, config: &ExactDedupConfig) -> Result<Option<u64>> {
    let length = u64::from(length);
    let reference_cost = u64::from(config.reference_cost_bytes);
    let Some(net) = length.checked_sub(reference_cost) else {
        return Ok(None);
    };
    if net == 0 || net < u64::from(config.min_net_savings_bytes) {
        Ok(None)
    } else {
        Ok(Some(net))
    }
}

fn compute_full_blake3<F>(
    data: &[u8],
    checkpoint: &F,
    forced_full_hash: Option<[u8; 32]>,
) -> Result<[u8; 32]>
where
    F: Fn() -> Result<()> + Sync,
{
    if let Some(forced) = forced_full_hash {
        checkpoint()?;
        return Ok(forced);
    }
    let mut hasher = blake3::Hasher::new();
    for block in data.chunks(READ_BLOCK_BYTES) {
        checkpoint()?;
        hasher.update(block);
    }
    checkpoint()?;
    Ok(*hasher.finalize().as_bytes())
}

fn ensure_working_memory(count: u64, config: &ExactDedupConfig) -> Result<()> {
    let per_chunk = size_of::<DedupInput<'static>>()
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(size_of::<FullCandidate<'static>>()))
        .and_then(|bytes| bytes.checked_add(size_of::<ExactDedupRecord>() * 2))
        .and_then(|bytes| bytes.checked_add(size_of::<ShardRange>()))
        .and_then(|bytes| bytes.checked_add(DEDUP_PER_CHUNK_MARGIN_BYTES as usize))
        .ok_or(PithosError::IntegerOverflow)?;
    let per_chunk = u64::try_from(per_chunk).map_err(|_| PithosError::IntegerOverflow)?;
    let working = count
        .checked_mul(per_chunk)
        .and_then(|bytes| bytes.checked_add(DEDUP_FIXED_WORKING_BYTES))
        .ok_or(PithosError::IntegerOverflow)?;
    if working > config.max_working_bytes {
        Err(PithosError::ResourceLimit("exact dedup working bytes"))
    } else {
        Ok(())
    }
}

fn validate_exact_dedup_plan<F>(
    ordered_inputs: &[DedupInput<'_>],
    plan: &ExactDedupPlan,
    config: &ExactDedupConfig,
    checkpoint: &F,
) -> Result<()>
where
    F: Fn() -> Result<()> + Sync,
{
    if ordered_inputs.len() != plan.records.len() {
        return Err(PithosError::InvalidMetadata("exact dedup plan coverage"));
    }

    let mut canonical_chunks = 0_u64;
    let mut referenced_chunks = 0_u64;
    let mut gross_duplicate_bytes = 0_u64;
    let mut reference_bytes = 0_u64;
    let mut net_saved_bytes = 0_u64;

    for (input, record) in ordered_inputs.iter().zip(&plan.records) {
        checkpoint()?;
        if input.chunk.chunk_id != record.chunk_id || input.chunk.length != record.length {
            return Err(PithosError::InvalidMetadata("exact dedup plan binding"));
        }
        if record.is_reference() {
            let canonical_index = ordered_inputs
                .binary_search_by_key(&record.canonical_chunk_id, |candidate| candidate.chunk.chunk_id)
                .map_err(|_| PithosError::InvalidMetadata("exact dedup canonical target"))?;
            let canonical = &ordered_inputs[canonical_index];
            if canonical_key(canonical) >= canonical_key(input)
                || canonical.chunk.length != input.chunk.length
                || canonical.data != input.data
            {
                return Err(PithosError::HashMismatch);
            }
            let expected = reference_savings(input.chunk.length, config)?.ok_or(
                PithosError::InvalidMetadata("non-beneficial exact dedup reference"),
            )?;
            if record.reference_cost_bytes != config.reference_cost_bytes
                || record.net_saved_bytes != expected
            {
                return Err(PithosError::InvalidMetadata("exact dedup cost mismatch"));
            }
            referenced_chunks = referenced_chunks
                .checked_add(1)
                .ok_or(PithosError::IntegerOverflow)?;
            gross_duplicate_bytes = gross_duplicate_bytes
                .checked_add(u64::from(record.length))
                .ok_or(PithosError::IntegerOverflow)?;
            reference_bytes = reference_bytes
                .checked_add(u64::from(record.reference_cost_bytes))
                .ok_or(PithosError::IntegerOverflow)?;
            net_saved_bytes = net_saved_bytes
                .checked_add(record.net_saved_bytes)
                .ok_or(PithosError::IntegerOverflow)?;
        } else {
            if record.canonical_chunk_id != record.chunk_id
                || record.reference_cost_bytes != 0
                || record.net_saved_bytes != 0
            {
                return Err(PithosError::InvalidMetadata("invalid canonical dedup record"));
            }
            canonical_chunks = canonical_chunks
                .checked_add(1)
                .ok_or(PithosError::IntegerOverflow)?;
        }
    }

    if canonical_chunks != plan.canonical_chunks
        || referenced_chunks != plan.referenced_chunks
        || gross_duplicate_bytes != plan.gross_duplicate_bytes
        || reference_bytes != plan.reference_bytes
        || net_saved_bytes != plan.net_saved_bytes
    {
        return Err(PithosError::InvalidMetadata("exact dedup plan totals"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;
    use crate::ChunkingMethod;

    fn chunk(chunk_id: u64, entry_id: u64, data: &[u8]) -> LogicalChunk {
        LogicalChunk {
            chunk_id,
            entry_id,
            object_id: 0,
            logical_offset: 0,
            length: u32::try_from(data.len()).unwrap(),
            method: ChunkingMethod::FastCdcV2020,
        }
    }

    #[test]
    fn exact_duplicates_reference_the_canonical_chunk_when_beneficial() {
        let duplicate = vec![7_u8; 4096];
        let distinct = vec![9_u8; 4096];
        let chunks = [
            chunk(0, 0, &duplicate),
            chunk(1, 1, &duplicate),
            chunk(2, 2, &distinct),
        ];
        let fingerprints = [
            ChunkFingerprint::compute(0, &duplicate).unwrap(),
            ChunkFingerprint::compute(1, &duplicate).unwrap(),
            ChunkFingerprint::compute(2, &distinct).unwrap(),
        ];
        let inputs = [
            DedupInput {
                chunk: &chunks[0],
                fingerprint: &fingerprints[0],
                data: &duplicate,
            },
            DedupInput {
                chunk: &chunks[1],
                fingerprint: &fingerprints[1],
                data: &duplicate,
            },
            DedupInput {
                chunk: &chunks[2],
                fingerprint: &fingerprints[2],
                data: &distinct,
            },
        ];

        let plan = exact_dedup(&inputs).unwrap();
        assert_eq!(plan.canonical_chunks, 2);
        assert_eq!(plan.referenced_chunks, 1);
        assert_eq!(plan.gross_duplicate_bytes, 4096);
        assert_eq!(plan.reference_bytes, 16);
        assert_eq!(plan.net_saved_bytes, 4080);
        assert_eq!(plan.record(1).unwrap().canonical_chunk_id, 0);
        assert!(!plan.record(0).unwrap().is_reference());
        assert!(!plan.record(2).unwrap().is_reference());
    }

    #[test]
    fn input_order_and_parallelism_do_not_change_the_plan() {
        let data = vec![5_u8; 8192];
        let chunks = [chunk(0, 0, &data), chunk(1, 1, &data), chunk(2, 2, &data)];
        let fingerprints = [
            ChunkFingerprint::compute(0, &data).unwrap(),
            ChunkFingerprint::compute(1, &data).unwrap(),
            ChunkFingerprint::compute(2, &data).unwrap(),
        ];
        let reversed = [
            DedupInput {
                chunk: &chunks[2],
                fingerprint: &fingerprints[2],
                data: &data,
            },
            DedupInput {
                chunk: &chunks[1],
                fingerprint: &fingerprints[1],
                data: &data,
            },
            DedupInput {
                chunk: &chunks[0],
                fingerprint: &fingerprints[0],
                data: &data,
            },
        ];
        let serial = ExactDedupConfig {
            parallelism: 1,
            ..ExactDedupConfig::default()
        };
        let parallel = ExactDedupConfig {
            parallelism: 4,
            ..ExactDedupConfig::default()
        };
        assert_eq!(
            exact_dedup_with_config(&reversed, &serial).unwrap(),
            exact_dedup_with_config(&reversed, &parallel).unwrap()
        );
        assert_eq!(
            exact_dedup_with_config(&reversed, &parallel)
                .unwrap()
                .record(2)
                .unwrap()
                .canonical_chunk_id,
            0
        );
    }

    #[test]
    fn compact_hash_collision_does_not_authorize_dedup() {
        let left = vec![1_u8; 4096];
        let right = vec![2_u8; 4096];
        let chunks = [chunk(0, 0, &left), chunk(1, 1, &right)];
        let first = ChunkFingerprint::compute(0, &left).unwrap();
        let mut forged = ChunkFingerprint::compute(1, &right).unwrap();
        forged.xxh3 = first.xxh3;
        forged.blake3_128 = first.blake3_128;
        forged.full_blake3 = None;
        let inputs = [
            DedupInput {
                chunk: &chunks[0],
                fingerprint: &first,
                data: &left,
            },
            DedupInput {
                chunk: &chunks[1],
                fingerprint: &forged,
                data: &right,
            },
        ];
        let plan = exact_dedup(&inputs).unwrap();
        assert_eq!(plan.referenced_chunks, 0);
        assert_eq!(plan.canonical_chunks, 2);
    }

    #[test]
    fn even_a_forced_full_hash_collision_is_resolved_by_exact_bytes() {
        let left = vec![3_u8; 4096];
        let right = vec![4_u8; 4096];
        let chunks = [chunk(0, 0, &left), chunk(1, 1, &right)];
        let first = ChunkFingerprint::compute(0, &left).unwrap();
        let mut forged = ChunkFingerprint::compute(1, &right).unwrap();
        forged.xxh3 = first.xxh3;
        forged.blake3_128 = first.blake3_128;
        forged.full_blake3 = None;
        let inputs = [
            DedupInput {
                chunk: &chunks[0],
                fingerprint: &first,
                data: &left,
            },
            DedupInput {
                chunk: &chunks[1],
                fingerprint: &forged,
                data: &right,
            },
        ];
        let plan = exact_dedup_internal(
            &inputs,
            &ExactDedupConfig::default(),
            &|| Ok(()),
            Some([0xA5; 32]),
        )
        .unwrap();
        assert_eq!(plan.referenced_chunks, 0);
    }

    #[test]
    fn tiny_duplicates_are_not_referenced_when_metadata_would_lose() {
        let data = [8_u8; 8];
        let chunks = [chunk(0, 0, &data), chunk(1, 1, &data)];
        let fingerprints = [
            ChunkFingerprint::compute(0, &data).unwrap(),
            ChunkFingerprint::compute(1, &data).unwrap(),
        ];
        let inputs = [
            DedupInput {
                chunk: &chunks[0],
                fingerprint: &fingerprints[0],
                data: &data,
            },
            DedupInput {
                chunk: &chunks[1],
                fingerprint: &fingerprints[1],
                data: &data,
            },
        ];
        let plan = exact_dedup(&inputs).unwrap();
        assert_eq!(plan.referenced_chunks, 0);
        assert_eq!(plan.net_saved_bytes, 0);
    }

    #[test]
    fn duplicate_ids_and_resource_abuse_fail_closed() {
        let data = vec![1_u8; 1024];
        let chunks = [chunk(0, 0, &data), chunk(0, 1, &data)];
        let fingerprints = [
            ChunkFingerprint::compute(0, &data).unwrap(),
            ChunkFingerprint::compute(0, &data).unwrap(),
        ];
        let inputs = [
            DedupInput {
                chunk: &chunks[0],
                fingerprint: &fingerprints[0],
                data: &data,
            },
            DedupInput {
                chunk: &chunks[1],
                fingerprint: &fingerprints[1],
                data: &data,
            },
        ];
        assert!(matches!(
            exact_dedup(&inputs),
            Err(PithosError::InvalidMetadata("duplicate exact dedup chunk ID"))
        ));

        let one_chunk = [DedupInput {
            chunk: &chunks[0],
            fingerprint: &fingerprints[0],
            data: &data,
        }];
        let limited = ExactDedupConfig {
            max_total_bytes: 1,
            ..ExactDedupConfig::default()
        };
        assert!(matches!(
            exact_dedup_with_config(&one_chunk, &limited),
            Err(PithosError::ResourceLimit("exact dedup total bytes"))
        ));
        let memory_limited = ExactDedupConfig {
            max_working_bytes: 1,
            ..ExactDedupConfig::default()
        };
        assert!(matches!(
            exact_dedup_with_config(&one_chunk, &memory_limited),
            Err(PithosError::ResourceLimit("exact dedup working bytes"))
        ));
    }

    #[test]
    fn checkpoints_cancel_before_publishing_a_plan() {
        let data = vec![6_u8; 256 * 1024];
        let chunks = [chunk(0, 0, &data), chunk(1, 1, &data)];
        let fingerprints = [
            ChunkFingerprint::compute(0, &data).unwrap(),
            ChunkFingerprint::compute(1, &data).unwrap(),
        ];
        let inputs = [
            DedupInput {
                chunk: &chunks[0],
                fingerprint: &fingerprints[0],
                data: &data,
            },
            DedupInput {
                chunk: &chunks[1],
                fingerprint: &fingerprints[1],
                data: &data,
            },
        ];
        let calls = AtomicUsize::new(0);
        let checkpoint = || {
            if calls.fetch_add(1, AtomicOrdering::Relaxed) > 8 {
                Err(PithosError::Cancelled)
            } else {
                Ok(())
            }
        };
        assert!(matches!(
            exact_dedup_with_checkpoint(&inputs, &ExactDedupConfig::default(), &checkpoint),
            Err(PithosError::Cancelled)
        ));
    }
}
