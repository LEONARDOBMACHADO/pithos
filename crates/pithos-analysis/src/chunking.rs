use std::{
    cell::RefCell,
    cmp::Ordering,
    io::{self, Read},
    rc::Rc,
};

use fastcdc::v2020::{
    AVERAGE_MAX, AVERAGE_MIN, FastCDC, MAXIMUM_MAX, MAXIMUM_MIN, MINIMUM_MAX, MINIMUM_MIN,
    Normalization, StreamCDC,
};
use pithos_core::{DecodeLimits, PithosError, Result};

const KIB: u32 = 1024;
const MIB: u32 = 1024 * KIB;
const FASTCDC_SEED: u64 = 0;

/// Deterministic policy and resource limits for logical chunking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkingConfig {
    pub fastcdc_min: u32,
    pub fastcdc_avg: u32,
    pub fastcdc_max: u32,
    pub high_entropy_fixed: u32,
    pub micro_file_max: u32,
    pub micro_pack_target: u32,
    pub max_chunks: u64,
    pub max_logical_bytes: u64,
    pub max_metadata_bytes: u64,
    pub max_path_bytes: u64,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        let limits = DecodeLimits::default();
        Self {
            fastcdc_min: 64 * KIB,
            fastcdc_avg: 256 * KIB,
            fastcdc_max: MIB,
            high_entropy_fixed: MIB,
            micro_file_max: 64 * KIB,
            micro_pack_target: 4 * MIB,
            max_chunks: limits.max_chunks,
            max_logical_bytes: limits.max_original_bytes,
            max_metadata_bytes: limits.max_metadata_bytes,
            max_path_bytes: limits.max_path_bytes,
        }
    }
}

impl ChunkingConfig {
    /// Validates all values before they reach a third-party chunker or drive an
    /// allocation. The FastCDC crate only debug-asserts some of these limits.
    pub fn validate(&self) -> Result<()> {
        let min = usize::try_from(self.fastcdc_min).map_err(|_| PithosError::IntegerOverflow)?;
        let avg = usize::try_from(self.fastcdc_avg).map_err(|_| PithosError::IntegerOverflow)?;
        let max = usize::try_from(self.fastcdc_max).map_err(|_| PithosError::IntegerOverflow)?;

        if !((MINIMUM_MIN..=MINIMUM_MAX).contains(&min)
            && (AVERAGE_MIN..=AVERAGE_MAX).contains(&avg)
            && (MAXIMUM_MIN..=MAXIMUM_MAX).contains(&max)
            && min < avg
            && avg < max)
        {
            return Err(PithosError::InvalidMetadata("FastCDC sizes"));
        }
        if !(MIB..=4 * MIB).contains(&self.high_entropy_fixed) {
            return Err(PithosError::InvalidMetadata("high entropy block size"));
        }
        if self.micro_file_max == 0 || self.micro_file_max > 64 * KIB {
            return Err(PithosError::InvalidMetadata("microfile size limit"));
        }
        if !(MIB..=16 * MIB).contains(&self.micro_pack_target)
            || self.micro_pack_target < self.micro_file_max
        {
            return Err(PithosError::InvalidMetadata("microfile pack target"));
        }
        if self.max_chunks == 0
            || self.max_logical_bytes == 0
            || self.max_metadata_bytes == 0
            || self.max_path_bytes == 0
        {
            return Err(PithosError::InvalidMetadata("chunking resource limit"));
        }
        Ok(())
    }
}

/// Identity and offset of an independently chunked logical object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkOrigin {
    pub entry_id: u64,
    pub object_id: u64,
    pub base_offset: u64,
}

/// Algorithm that produced a logical chunk boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChunkingMethod {
    FastCdcV2020,
    FixedHighEntropy,
    Structural,
    MicroFile,
}

/// A chunk discovered by a worker before globally deterministic ID assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LogicalChunkDraft {
    pub entry_id: u64,
    pub object_id: u64,
    pub logical_offset: u64,
    pub length: u32,
    pub method: ChunkingMethod,
}

/// Logical chunk with its final ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LogicalChunk {
    pub chunk_id: u64,
    pub entry_id: u64,
    pub object_id: u64,
    pub logical_offset: u64,
    pub length: u32,
    pub method: ChunkingMethod,
}

/// Runs canonical FastCDC v2020 over an in-memory logical object.
pub fn chunk_fastcdc(
    data: &[u8],
    origin: ChunkOrigin,
    config: &ChunkingConfig,
) -> Result<Vec<LogicalChunkDraft>> {
    chunk_fastcdc_with_checkpoint(data, origin, config, || Ok(()))
}

/// In-memory FastCDC with a cooperative checkpoint before every bounded chunk
/// scan.
pub fn chunk_fastcdc_with_checkpoint<F>(
    data: &[u8],
    origin: ChunkOrigin,
    config: &ChunkingConfig,
    mut checkpoint: F,
) -> Result<Vec<LogicalChunkDraft>>
where
    F: FnMut() -> Result<()>,
{
    config.validate()?;
    let logical_size = u64::try_from(data.len()).map_err(|_| PithosError::IntegerOverflow)?;
    ensure_logical_size(logical_size, config)?;
    origin
        .base_offset
        .checked_add(logical_size)
        .ok_or(PithosError::IntegerOverflow)?;

    let mut chunks = Vec::new();
    let chunker = FastCDC::with_level_and_seed(
        data,
        config.fastcdc_min as usize,
        config.fastcdc_avg as usize,
        config.fastcdc_max as usize,
        Normalization::Level1,
        FASTCDC_SEED,
    );
    for chunk in chunker {
        checkpoint()?;
        push_relative_chunk(
            &mut chunks,
            origin,
            u64::try_from(chunk.offset).map_err(|_| PithosError::IntegerOverflow)?,
            chunk.length,
            ChunkingMethod::FastCdcV2020,
            config.max_chunks,
        )?;
    }
    validate_chunk_coverage(&chunks, origin, logical_size)?;
    Ok(chunks)
}

/// Runs streaming FastCDC while retaining only chunk descriptors. The
/// third-party iterator owns at most one `fastcdc_max`-sized data buffer.
pub fn chunk_fastcdc_reader<R: Read>(
    reader: R,
    origin: ChunkOrigin,
    config: &ChunkingConfig,
) -> Result<Vec<LogicalChunkDraft>> {
    chunk_fastcdc_reader_with_checkpoint(reader, origin, config, || Ok(()))
}

/// Streaming FastCDC variant with a cooperative cancellation/resource
/// checkpoint evaluated before every chunk-sized read operation.
pub fn chunk_fastcdc_reader_with_checkpoint<R, F>(
    reader: R,
    origin: ChunkOrigin,
    config: &ChunkingConfig,
    mut checkpoint: F,
) -> Result<Vec<LogicalChunkDraft>>
where
    R: Read,
    F: FnMut() -> Result<()>,
{
    config.validate()?;
    let checkpoint_error = Rc::new(RefCell::new(None));
    let bounded_reader = BoundedCheckpointReader {
        inner: reader,
        checkpoint: &mut checkpoint,
        checkpoint_error: Rc::clone(&checkpoint_error),
        remaining: config.max_logical_bytes.saturating_add(1),
    };
    let chunker = StreamCDC::with_level_and_seed(
        bounded_reader,
        config.fastcdc_min as usize,
        config.fastcdc_avg as usize,
        config.fastcdc_max as usize,
        Normalization::Level1,
        FASTCDC_SEED,
    );
    let mut chunks = Vec::new();
    let mut logical_size = 0_u64;

    for next in chunker {
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(error) => {
                if let Some(error) = checkpoint_error.borrow_mut().take() {
                    return Err(error);
                }
                let error: io::Error = error.into();
                return Err(PithosError::Io(error));
            }
        };
        if chunk.offset != logical_size || chunk.data.len() != chunk.length {
            return Err(PithosError::InvalidMetadata("FastCDC stream layout"));
        }
        let next_size = logical_size
            .checked_add(u64::try_from(chunk.length).map_err(|_| PithosError::IntegerOverflow)?)
            .ok_or(PithosError::IntegerOverflow)?;
        ensure_logical_size(next_size, config)?;
        push_relative_chunk(
            &mut chunks,
            origin,
            chunk.offset,
            chunk.length,
            ChunkingMethod::FastCdcV2020,
            config.max_chunks,
        )?;
        logical_size = next_size;
    }
    if let Some(error) = checkpoint_error.borrow_mut().take() {
        return Err(error);
    }

    validate_chunk_coverage(&chunks, origin, logical_size)?;
    Ok(chunks)
}

/// Splits explicitly classified high-entropy content into fixed 1--4 MiB
/// blocks. Entropy classification is deliberately left to the caller.
pub fn chunk_fixed_high_entropy(
    logical_size: u64,
    origin: ChunkOrigin,
    config: &ChunkingConfig,
) -> Result<Vec<LogicalChunkDraft>> {
    chunk_fixed_high_entropy_with_checkpoint(logical_size, origin, config, || Ok(()))
}

/// Fixed high-entropy chunking with cooperative cancellation.
pub fn chunk_fixed_high_entropy_with_checkpoint<F>(
    logical_size: u64,
    origin: ChunkOrigin,
    config: &ChunkingConfig,
    mut checkpoint: F,
) -> Result<Vec<LogicalChunkDraft>>
where
    F: FnMut() -> Result<()>,
{
    config.validate()?;
    ensure_logical_size(logical_size, config)?;
    origin
        .base_offset
        .checked_add(logical_size)
        .ok_or(PithosError::IntegerOverflow)?;
    if logical_size == 0 {
        return Ok(Vec::new());
    }

    let block_size = u64::from(config.high_entropy_fixed);
    let chunk_count = logical_size
        .checked_sub(1)
        .and_then(|value| value.checked_div(block_size))
        .and_then(|value| value.checked_add(1))
        .ok_or(PithosError::IntegerOverflow)?;
    ensure_chunk_count(chunk_count, config.max_chunks)?;
    let capacity = usize::try_from(chunk_count).map_err(|_| PithosError::IntegerOverflow)?;
    let mut chunks = Vec::new();
    chunks
        .try_reserve_exact(capacity)
        .map_err(|_| PithosError::MemoryLimit)?;

    let mut relative_offset = 0_u64;
    while relative_offset < logical_size {
        checkpoint()?;
        let remaining = logical_size - relative_offset;
        let length = remaining.min(block_size);
        push_relative_chunk(
            &mut chunks,
            origin,
            relative_offset,
            usize::try_from(length).map_err(|_| PithosError::IntegerOverflow)?,
            ChunkingMethod::FixedHighEntropy,
            config.max_chunks,
        )?;
        relative_offset = relative_offset
            .checked_add(length)
            .ok_or(PithosError::IntegerOverflow)?;
    }

    validate_chunk_coverage(&chunks, origin, logical_size)?;
    Ok(chunks)
}

/// Preserves scanner-provided exclusive region ends. Regions larger than the
/// FastCDC maximum are subchunked independently, so no chunk crosses a
/// structural boundary.
pub fn chunk_structural(
    data: &[u8],
    origin: ChunkOrigin,
    boundary_ends: &[u64],
    config: &ChunkingConfig,
) -> Result<Vec<LogicalChunkDraft>> {
    chunk_structural_with_checkpoint(data, origin, boundary_ends, config, || Ok(()))
}

/// In-memory structural chunking with cooperative cancellation at every region
/// and FastCDC subchunk.
pub fn chunk_structural_with_checkpoint<F>(
    data: &[u8],
    origin: ChunkOrigin,
    boundary_ends: &[u64],
    config: &ChunkingConfig,
    mut checkpoint: F,
) -> Result<Vec<LogicalChunkDraft>>
where
    F: FnMut() -> Result<()>,
{
    config.validate()?;
    let logical_size = u64::try_from(data.len()).map_err(|_| PithosError::IntegerOverflow)?;
    ensure_logical_size(logical_size, config)?;
    origin
        .base_offset
        .checked_add(logical_size)
        .ok_or(PithosError::IntegerOverflow)?;

    if data.is_empty() {
        if boundary_ends.is_empty() {
            return Ok(Vec::new());
        }
        return Err(PithosError::InvalidMetadata("structural boundaries"));
    }
    ensure_chunk_count(
        u64::try_from(boundary_ends.len()).map_err(|_| PithosError::IntegerOverflow)?,
        config.max_chunks,
    )?;
    validate_boundary_ends(boundary_ends, logical_size)?;

    let mut chunks = Vec::new();
    let mut region_start = 0_u64;
    for &region_end in boundary_ends {
        checkpoint()?;
        let region_len = region_end
            .checked_sub(region_start)
            .ok_or(PithosError::IntegerOverflow)?;
        if region_len <= u64::from(config.fastcdc_max) {
            push_relative_chunk(
                &mut chunks,
                origin,
                region_start,
                usize::try_from(region_len).map_err(|_| PithosError::IntegerOverflow)?,
                ChunkingMethod::Structural,
                config.max_chunks,
            )?;
        } else {
            let start = usize::try_from(region_start).map_err(|_| PithosError::IntegerOverflow)?;
            let end = usize::try_from(region_end).map_err(|_| PithosError::IntegerOverflow)?;
            let remaining_limit = config
                .max_chunks
                .checked_sub(u64::try_from(chunks.len()).map_err(|_| PithosError::IntegerOverflow)?)
                .ok_or(PithosError::IntegerOverflow)?;
            if remaining_limit == 0 {
                return Err(PithosError::ResourceLimit("chunk count"));
            }
            let sub_origin = ChunkOrigin {
                entry_id: origin.entry_id,
                object_id: origin.object_id,
                base_offset: origin
                    .base_offset
                    .checked_add(region_start)
                    .ok_or(PithosError::IntegerOverflow)?,
            };
            let sub_config = ChunkingConfig {
                max_chunks: remaining_limit,
                max_logical_bytes: region_len,
                ..*config
            };
            let mut subchunks = chunk_fastcdc_with_checkpoint(
                &data[start..end],
                sub_origin,
                &sub_config,
                &mut checkpoint,
            )?;
            for chunk in &mut subchunks {
                chunk.method = ChunkingMethod::Structural;
            }
            chunks
                .try_reserve(subchunks.len())
                .map_err(|_| PithosError::MemoryLimit)?;
            chunks.extend(subchunks);
        }
        region_start = region_end;
    }

    validate_chunk_coverage(&chunks, origin, logical_size)?;
    Ok(chunks)
}

/// Streaming structural chunking. Scanner boundaries define the expected
/// logical size, so short and trailing input both fail closed.
pub fn chunk_structural_reader<R: Read>(
    reader: R,
    origin: ChunkOrigin,
    boundary_ends: &[u64],
    config: &ChunkingConfig,
) -> Result<Vec<LogicalChunkDraft>> {
    chunk_structural_reader_with_checkpoint(reader, origin, boundary_ends, config, || Ok(()))
}

/// Streaming structural chunking with cooperative cancellation.
pub fn chunk_structural_reader_with_checkpoint<R, F>(
    mut reader: R,
    origin: ChunkOrigin,
    boundary_ends: &[u64],
    config: &ChunkingConfig,
    mut checkpoint: F,
) -> Result<Vec<LogicalChunkDraft>>
where
    R: Read,
    F: FnMut() -> Result<()>,
{
    config.validate()?;
    let logical_size = boundary_ends.last().copied().unwrap_or(0);
    ensure_logical_size(logical_size, config)?;
    origin
        .base_offset
        .checked_add(logical_size)
        .ok_or(PithosError::IntegerOverflow)?;

    if logical_size == 0 {
        if !boundary_ends.is_empty() {
            return Err(PithosError::InvalidMetadata("structural boundaries"));
        }
        checkpoint()?;
        let mut trailing = [0_u8; 1];
        return match reader.read(&mut trailing)? {
            0 => Ok(Vec::new()),
            _ => Err(PithosError::InvalidMetadata("trailing structural data")),
        };
    }
    ensure_chunk_count(
        u64::try_from(boundary_ends.len()).map_err(|_| PithosError::IntegerOverflow)?,
        config.max_chunks,
    )?;
    validate_boundary_ends(boundary_ends, logical_size)?;

    let mut chunks = Vec::new();
    let mut region_start = 0_u64;
    for &region_end in boundary_ends {
        checkpoint()?;
        let region_len = region_end
            .checked_sub(region_start)
            .ok_or(PithosError::IntegerOverflow)?;
        if region_len <= u64::from(config.fastcdc_max) {
            consume_exact_region(&mut reader, region_len, &mut checkpoint)?;
            push_relative_chunk(
                &mut chunks,
                origin,
                region_start,
                usize::try_from(region_len).map_err(|_| PithosError::IntegerOverflow)?,
                ChunkingMethod::Structural,
                config.max_chunks,
            )?;
        } else {
            let remaining_limit = config
                .max_chunks
                .checked_sub(u64::try_from(chunks.len()).map_err(|_| PithosError::IntegerOverflow)?)
                .ok_or(PithosError::IntegerOverflow)?;
            if remaining_limit == 0 {
                return Err(PithosError::ResourceLimit("chunk count"));
            }
            let sub_origin = ChunkOrigin {
                entry_id: origin.entry_id,
                object_id: origin.object_id,
                base_offset: origin
                    .base_offset
                    .checked_add(region_start)
                    .ok_or(PithosError::IntegerOverflow)?,
            };
            let sub_config = ChunkingConfig {
                max_chunks: remaining_limit,
                max_logical_bytes: region_len,
                ..*config
            };
            let mut limited = (&mut reader).take(region_len);
            let mut subchunks = chunk_fastcdc_reader_with_checkpoint(
                &mut limited,
                sub_origin,
                &sub_config,
                &mut checkpoint,
            )?;
            if limited.limit() != 0 {
                return Err(PithosError::InvalidMetadata("short structural stream"));
            }
            validate_chunk_coverage(&subchunks, sub_origin, region_len)?;
            for chunk in &mut subchunks {
                chunk.method = ChunkingMethod::Structural;
            }
            chunks
                .try_reserve(subchunks.len())
                .map_err(|_| PithosError::MemoryLimit)?;
            chunks.extend(subchunks);
        }
        region_start = region_end;
    }

    checkpoint()?;
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(PithosError::InvalidMetadata("trailing structural data"));
    }
    validate_chunk_coverage(&chunks, origin, logical_size)?;
    Ok(chunks)
}

/// Verifies exact, ordered coverage of one logical object.
pub fn validate_chunk_coverage(
    chunks: &[LogicalChunkDraft],
    origin: ChunkOrigin,
    logical_size: u64,
) -> Result<()> {
    let expected_end = origin
        .base_offset
        .checked_add(logical_size)
        .ok_or(PithosError::IntegerOverflow)?;
    if logical_size == 0 {
        return if chunks.is_empty()
            || (chunks.len() == 1
                && chunks[0].entry_id == origin.entry_id
                && chunks[0].object_id == origin.object_id
                && chunks[0].logical_offset == origin.base_offset
                && chunks[0].length == 0
                && chunks[0].method == ChunkingMethod::MicroFile)
        {
            Ok(())
        } else {
            Err(PithosError::InvalidMetadata("empty chunk coverage"))
        };
    }
    if chunks.is_empty() {
        return Err(PithosError::InvalidMetadata("missing chunk coverage"));
    }

    let mut cursor = origin.base_offset;
    for chunk in chunks {
        if chunk.entry_id != origin.entry_id
            || chunk.object_id != origin.object_id
            || chunk.logical_offset != cursor
            || chunk.length == 0
        {
            return Err(PithosError::InvalidMetadata("chunk coverage"));
        }
        cursor = cursor
            .checked_add(u64::from(chunk.length))
            .ok_or(PithosError::IntegerOverflow)?;
        if cursor > expected_end {
            return Err(PithosError::InvalidMetadata("chunk coverage"));
        }
    }
    if cursor != expected_end {
        return Err(PithosError::InvalidMetadata("chunk coverage"));
    }
    Ok(())
}

/// Sorts worker drafts by the normative tuple and assigns stable IDs starting
/// at zero. Duplicate tuple keys are rejected because they have no stable
/// tie-break defined by the format contract.
pub fn assign_chunk_ids(
    drafts: Vec<LogicalChunkDraft>,
    max_chunks: u64,
) -> Result<Vec<LogicalChunk>> {
    assign_chunk_ids_with_checkpoint(drafts, max_chunks, || Ok(()))
}

/// Deterministic global ID assignment with cooperative checkpoints throughout
/// sorting and materialization.
pub fn assign_chunk_ids_with_checkpoint<F>(
    mut drafts: Vec<LogicalChunkDraft>,
    max_chunks: u64,
    mut checkpoint: F,
) -> Result<Vec<LogicalChunk>>
where
    F: FnMut() -> Result<()>,
{
    if max_chunks == 0 {
        return Err(PithosError::InvalidMetadata("chunk count limit"));
    }
    ensure_chunk_count(
        u64::try_from(drafts.len()).map_err(|_| PithosError::IntegerOverflow)?,
        max_chunks,
    )?;
    try_sort_by_checkpoint(
        &mut drafts,
        |left, right| {
            (left.entry_id, left.object_id, left.logical_offset).cmp(&(
                right.entry_id,
                right.object_id,
                right.logical_offset,
            ))
        },
        &mut checkpoint,
    )?;
    if drafts.windows(2).any(|pair| {
        (pair[0].entry_id, pair[0].object_id, pair[0].logical_offset)
            == (pair[1].entry_id, pair[1].object_id, pair[1].logical_offset)
    }) {
        return Err(PithosError::InvalidMetadata("duplicate chunk position"));
    }

    let mut chunks = Vec::new();
    chunks
        .try_reserve_exact(drafts.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    for (index, draft) in drafts.into_iter().enumerate() {
        checkpoint()?;
        if draft.length == 0 && draft.method != ChunkingMethod::MicroFile {
            return Err(PithosError::InvalidMetadata("zero-length chunk"));
        }
        draft
            .logical_offset
            .checked_add(u64::from(draft.length))
            .ok_or(PithosError::IntegerOverflow)?;
        chunks.push(LogicalChunk {
            chunk_id: u64::try_from(index).map_err(|_| PithosError::IntegerOverflow)?,
            entry_id: draft.entry_id,
            object_id: draft.object_id,
            logical_offset: draft.logical_offset,
            length: draft.length,
            method: draft.method,
        });
    }
    Ok(chunks)
}

struct BoundedCheckpointReader<'a, R, F> {
    inner: R,
    checkpoint: &'a mut F,
    checkpoint_error: Rc<RefCell<Option<PithosError>>>,
    remaining: u64,
}

impl<R, F> Read for BoundedCheckpointReader<'_, R, F>
where
    R: Read,
    F: FnMut() -> Result<()>,
{
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        if let Err(error) = (self.checkpoint)() {
            *self.checkpoint_error.borrow_mut() = Some(error);
            return Err(io::Error::other("logical chunking checkpoint"));
        }
        let buffer_len = u64::try_from(buffer.len())
            .map_err(|_| io::Error::other("reader buffer length overflow"))?;
        let allowed = usize::try_from(self.remaining.min(buffer_len))
            .map_err(|_| io::Error::other("reader limit overflow"))?;
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.remaining = self
            .remaining
            .checked_sub(u64::try_from(read).map_err(|_| io::Error::other("read overflow"))?)
            .ok_or_else(|| io::Error::other("reader exceeded requested buffer"))?;
        Ok(read)
    }
}

pub(crate) fn try_sort_by_checkpoint<T, Compare, Checkpoint>(
    items: &mut [T],
    mut compare: Compare,
    checkpoint: &mut Checkpoint,
) -> Result<()>
where
    T: Copy,
    Compare: FnMut(&T, &T) -> Ordering,
    Checkpoint: FnMut() -> Result<()>,
{
    if items.len() < 2 {
        checkpoint()?;
        return Ok(());
    }

    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(items.len())
        .map_err(|_| PithosError::MemoryLimit)?;
    scratch.extend_from_slice(items);

    let mut width = 1_usize;
    while width < items.len() {
        let mut start = 0_usize;
        while start < items.len() {
            checkpoint()?;
            let middle = start.saturating_add(width).min(items.len());
            let end = middle.saturating_add(width).min(items.len());
            let mut left = start;
            let mut right = middle;
            let mut output = start;

            while left < middle && right < end {
                checkpoint()?;
                if compare(&items[left], &items[right]) != Ordering::Greater {
                    scratch[output] = items[left];
                    left += 1;
                } else {
                    scratch[output] = items[right];
                    right += 1;
                }
                output += 1;
            }
            while left < middle {
                checkpoint()?;
                scratch[output] = items[left];
                left += 1;
                output += 1;
            }
            while right < end {
                checkpoint()?;
                scratch[output] = items[right];
                right += 1;
                output += 1;
            }
            start = end;
        }

        for (item, sorted) in items.iter_mut().zip(&scratch) {
            checkpoint()?;
            *item = *sorted;
        }
        width = width.saturating_mul(2);
    }
    Ok(())
}

fn validate_boundary_ends(boundary_ends: &[u64], logical_size: u64) -> Result<()> {
    if boundary_ends.is_empty() || boundary_ends.last().copied() != Some(logical_size) {
        return Err(PithosError::InvalidMetadata("structural boundaries"));
    }
    let mut previous = 0_u64;
    for &boundary in boundary_ends {
        if boundary <= previous || boundary > logical_size {
            return Err(PithosError::InvalidMetadata("structural boundaries"));
        }
        previous = boundary;
    }
    Ok(())
}

fn push_relative_chunk(
    chunks: &mut Vec<LogicalChunkDraft>,
    origin: ChunkOrigin,
    relative_offset: u64,
    length: usize,
    method: ChunkingMethod,
    max_chunks: u64,
) -> Result<()> {
    let next_count = u64::try_from(chunks.len())
        .map_err(|_| PithosError::IntegerOverflow)?
        .checked_add(1)
        .ok_or(PithosError::IntegerOverflow)?;
    ensure_chunk_count(next_count, max_chunks)?;
    let length = u32::try_from(length).map_err(|_| PithosError::IntegerOverflow)?;
    if length == 0 {
        return Err(PithosError::InvalidMetadata("zero-length chunk"));
    }
    let logical_offset = origin
        .base_offset
        .checked_add(relative_offset)
        .ok_or(PithosError::IntegerOverflow)?;
    logical_offset
        .checked_add(u64::from(length))
        .ok_or(PithosError::IntegerOverflow)?;
    chunks
        .try_reserve(1)
        .map_err(|_| PithosError::MemoryLimit)?;
    chunks.push(LogicalChunkDraft {
        entry_id: origin.entry_id,
        object_id: origin.object_id,
        logical_offset,
        length,
        method,
    });
    Ok(())
}

fn ensure_chunk_count(count: u64, max_chunks: u64) -> Result<()> {
    if count > max_chunks {
        Err(PithosError::ResourceLimit("chunk count"))
    } else {
        Ok(())
    }
}

fn ensure_logical_size(size: u64, config: &ChunkingConfig) -> Result<()> {
    if size > config.max_logical_bytes {
        Err(PithosError::ResourceLimit("logical bytes"))
    } else {
        Ok(())
    }
}

fn consume_exact_region<R, F>(reader: &mut R, length: u64, checkpoint: &mut F) -> Result<()>
where
    R: Read,
    F: FnMut() -> Result<()>,
{
    let mut remaining = length;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        checkpoint()?;
        let request = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| PithosError::IntegerOverflow)?;
        match reader.read(&mut buffer[..request]) {
            Ok(0) => return Err(PithosError::InvalidMetadata("short structural stream")),
            Ok(read) => {
                remaining = remaining
                    .checked_sub(u64::try_from(read).map_err(|_| PithosError::IntegerOverflow)?)
                    .ok_or(PithosError::IntegerOverflow)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(PithosError::Io(error)),
        }
    }
    Ok(())
}
