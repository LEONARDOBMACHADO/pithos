use crate::ChunkingConfig;
use pithos_core::{PithosError, Result};

/// Metadata-only description of a candidate for a [`MicroFilePack`].
///
/// The planner deliberately receives the content length and its already-computed
/// hash instead of the content itself. This keeps planning bounded independently
/// of the total payload size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroFileInput {
    pub entry_id: u64,
    pub path: Vec<u8>,
    pub size: u64,
    pub modified_ns: i64,
    pub mode: u32,
    pub file_hash: [u8; 32],
    pub family_key: u64,
    pub path_prefix_key: Vec<u8>,
    pub extension_key: Vec<u8>,
    pub similarity_key: u64,
    pub requires_isolated_access: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicroFileExclusionReason {
    TooLarge,
    RequiresIsolatedAccess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroFileExclusion {
    pub entry_id: u64,
    pub reason: MicroFileExclusionReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontCodedPath {
    /// Number of bytes reused from the immediately preceding decoded path in
    /// this pack. It is zero for the first path.
    pub shared_prefix_len: u32,
    pub suffix: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroFileMetadataRecord {
    pub entry_id: u64,
    pub mode_index: u32,
    pub content_offset: u64,
    pub length: u32,
    pub file_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroFilePackMetadata {
    pub paths: Vec<FrontCodedPath>,
    pub base_modified_ns: i64,
    /// Non-negative deltas from `base_modified_ns`. A `u64` covers the full
    /// distance between `i64::MIN` and `i64::MAX` without overflow.
    pub modified_ns_deltas: Vec<u64>,
    pub mode_dictionary: Vec<u32>,
    pub records: Vec<MicroFileMetadataRecord>,
}

impl MicroFilePackMetadata {
    /// Reconstructs and validates all front-coded paths and parallel metadata
    /// columns. Malformed metadata fails closed without indexing unchecked data.
    pub fn expanded_paths(&self) -> Result<Vec<Vec<u8>>> {
        self.expanded_paths_with_checkpoint(&ChunkingConfig::default(), || Ok(()))
    }

    /// Bounded variant used when the caller supplies stricter archive limits.
    pub fn expanded_paths_with_config(&self, config: &ChunkingConfig) -> Result<Vec<Vec<u8>>> {
        self.expanded_paths_with_checkpoint(config, || Ok(()))
    }

    /// Bounded path reconstruction with checkpoints in every scan, comparison
    /// and allocation-driving pass.
    pub fn expanded_paths_with_checkpoint<F>(
        &self,
        config: &ChunkingConfig,
        mut checkpoint: F,
    ) -> Result<Vec<Vec<u8>>>
    where
        F: FnMut() -> Result<()>,
    {
        config.validate()?;
        checkpoint()?;
        let record_count = self.records.len();
        let record_count_u64 =
            u64::try_from(record_count).map_err(|_| PithosError::IntegerOverflow)?;
        if record_count_u64 > config.max_chunks {
            return Err(PithosError::ResourceLimit("chunk count"));
        }
        if self.paths.len() != record_count || self.modified_ns_deltas.len() != record_count {
            return Err(PithosError::InvalidMetadata("microfile metadata columns"));
        }
        if record_count > 0 && self.mode_dictionary.is_empty() {
            return Err(PithosError::InvalidMetadata("microfile mode dictionary"));
        }
        if self.mode_dictionary.len() > record_count {
            return Err(PithosError::InvalidMetadata("microfile mode dictionary"));
        }
        for pair in self.mode_dictionary.windows(2) {
            checkpoint()?;
            if pair[0] >= pair[1] {
                return Err(PithosError::InvalidMetadata("microfile mode dictionary"));
            }
        }

        let dictionary_bytes = u64::try_from(self.mode_dictionary.len())
            .map_err(|_| PithosError::IntegerOverflow)?
            .checked_mul(4)
            .ok_or(PithosError::IntegerOverflow)?;
        let base_column_bytes = record_count_u64
            .checked_mul(72)
            .and_then(|value| value.checked_add(dictionary_bytes))
            .ok_or(PithosError::IntegerOverflow)?;
        ensure_metadata_size(base_column_bytes, config)?;

        let mut suffix_bytes = 0_u64;
        for path in &self.paths {
            checkpoint()?;
            suffix_bytes = suffix_bytes
                .checked_add(
                    u64::try_from(path.suffix.len()).map_err(|_| PithosError::IntegerOverflow)?,
                )
                .ok_or(PithosError::IntegerOverflow)?;
            ensure_metadata_size(
                base_column_bytes
                    .checked_add(suffix_bytes)
                    .ok_or(PithosError::IntegerOverflow)?,
                config,
            )?;
        }
        let column_bytes = base_column_bytes
            .checked_add(suffix_bytes)
            .ok_or(PithosError::IntegerOverflow)?;

        let mut paths = Vec::new();
        try_reserve_exact(&mut paths, record_count)?;
        let mut previous = Vec::new();
        let mut entry_ids = Vec::new();
        try_reserve_exact(&mut entry_ids, record_count)?;
        let mut expected_offset = 0_u64;
        let mut expanded_path_bytes = 0_u64;

        for ((path, delta), record) in self
            .paths
            .iter()
            .zip(&self.modified_ns_deltas)
            .zip(&self.records)
        {
            checkpoint()?;
            let prefix = usize::try_from(path.shared_prefix_len)
                .map_err(|_| PithosError::IntegerOverflow)?;
            if prefix > previous.len() {
                return Err(PithosError::InvalidMetadata("front-coded microfile path"));
            }
            let path_len = prefix
                .checked_add(path.suffix.len())
                .ok_or(PithosError::IntegerOverflow)?;
            let path_len_u64 = u64::try_from(path_len).map_err(|_| PithosError::IntegerOverflow)?;
            if path_len_u64 > config.max_path_bytes {
                return Err(PithosError::ResourceLimit("path bytes"));
            }
            expanded_path_bytes = expanded_path_bytes
                .checked_add(path_len_u64)
                .ok_or(PithosError::IntegerOverflow)?;
            ensure_metadata_size(
                column_bytes
                    .checked_add(expanded_path_bytes)
                    .ok_or(PithosError::IntegerOverflow)?,
                config,
            )?;
            let mut expanded = Vec::new();
            expanded
                .try_reserve_exact(path_len)
                .map_err(|_| PithosError::MemoryLimit)?;
            expanded.extend_from_slice(&previous[..prefix]);
            expanded.extend_from_slice(&path.suffix);
            if expanded.is_empty() {
                return Err(PithosError::InvalidMetadata("empty microfile path"));
            }

            let modified = i128::from(self.base_modified_ns) + i128::from(*delta);
            i64::try_from(modified).map_err(|_| PithosError::IntegerOverflow)?;
            let mode_index =
                usize::try_from(record.mode_index).map_err(|_| PithosError::IntegerOverflow)?;
            if mode_index >= self.mode_dictionary.len() || record.content_offset != expected_offset
            {
                return Err(PithosError::InvalidMetadata("microfile metadata record"));
            }
            expected_offset = expected_offset
                .checked_add(u64::from(record.length))
                .ok_or(PithosError::IntegerOverflow)?;
            entry_ids.push(record.entry_id);
            previous.clear();
            previous
                .try_reserve_exact(expanded.len())
                .map_err(|_| PithosError::MemoryLimit)?;
            previous.extend_from_slice(&expanded);
            paths.push(expanded);
        }

        crate::chunking::try_sort_by_checkpoint(&mut entry_ids, Ord::cmp, &mut checkpoint)?;
        for pair in entry_ids.windows(2) {
            checkpoint()?;
            if pair[0] == pair[1] {
                return Err(PithosError::InvalidMetadata("duplicate microfile entry id"));
            }
        }
        let mut ordered_paths = Vec::new();
        try_reserve_exact(&mut ordered_paths, paths.len())?;
        for path in &paths {
            checkpoint()?;
            ordered_paths.push(path.as_slice());
        }
        crate::chunking::try_sort_by_checkpoint(&mut ordered_paths, Ord::cmp, &mut checkpoint)?;
        for pair in ordered_paths.windows(2) {
            checkpoint()?;
            if pair[0] == pair[1] {
                return Err(PithosError::InvalidMetadata("duplicate microfile path"));
            }
        }
        Ok(paths)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroFilePackMember {
    pub entry_id: u64,
    pub content_offset: u64,
    pub length: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroFilePack {
    pub pack_id: u64,
    pub members: Vec<MicroFilePackMember>,
    pub uncompressed_len: u64,
    pub metadata: MicroFilePackMetadata,
}

impl MicroFilePack {
    /// Validates member coverage and its compact metadata representation.
    pub fn validate(&self, config: &ChunkingConfig) -> Result<()> {
        self.validate_with_checkpoint(config, || Ok(()))
    }

    /// Validates a pack with cooperative checkpoints in all linear passes.
    pub fn validate_with_checkpoint<F>(
        &self,
        config: &ChunkingConfig,
        mut checkpoint: F,
    ) -> Result<()>
    where
        F: FnMut() -> Result<()>,
    {
        config.validate()?;
        checkpoint()?;
        let member_count =
            u64::try_from(self.members.len()).map_err(|_| PithosError::IntegerOverflow)?;
        if member_count > config.max_chunks {
            return Err(PithosError::ResourceLimit("chunk count"));
        }
        if self.members.is_empty()
            || self.members.len() != self.metadata.records.len()
            || self.uncompressed_len > u64::from(config.micro_pack_target)
            || self.uncompressed_len > config.max_logical_bytes
        {
            return Err(PithosError::InvalidMetadata("microfile pack layout"));
        }
        self.metadata
            .expanded_paths_with_checkpoint(config, &mut checkpoint)?;

        let mut expected_offset = 0_u64;
        for (member, record) in self.members.iter().zip(&self.metadata.records) {
            checkpoint()?;
            if member.entry_id != record.entry_id
                || member.content_offset != record.content_offset
                || member.length != record.length
                || member.content_offset != expected_offset
                || member.length > config.micro_file_max
            {
                return Err(PithosError::InvalidMetadata("microfile member mapping"));
            }
            expected_offset = expected_offset
                .checked_add(u64::from(member.length))
                .ok_or(PithosError::IntegerOverflow)?;
        }
        if expected_offset != self.uncompressed_len {
            return Err(PithosError::InvalidMetadata("microfile pack length"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MicroFilePackPlan {
    pub packs: Vec<MicroFilePack>,
    pub excluded: Vec<MicroFileExclusion>,
}

impl MicroFilePackPlan {
    /// Validates canonical pack IDs, global entry uniqueness and every pack.
    pub fn validate(&self, config: &ChunkingConfig) -> Result<()> {
        self.validate_with_checkpoint(config, || Ok(()))
    }

    /// Validates the complete plan with cooperative checkpoints during pack,
    /// member and uniqueness scans.
    pub fn validate_with_checkpoint<F>(
        &self,
        config: &ChunkingConfig,
        mut checkpoint: F,
    ) -> Result<()>
    where
        F: FnMut() -> Result<()>,
    {
        config.validate()?;
        checkpoint()?;
        let pack_count =
            u64::try_from(self.packs.len()).map_err(|_| PithosError::IntegerOverflow)?;
        if pack_count > config.max_chunks {
            return Err(PithosError::ResourceLimit("chunk count"));
        }

        let mut total_members = 0_u64;
        let mut total_content = 0_u64;
        for pack in &self.packs {
            checkpoint()?;
            total_members = total_members
                .checked_add(
                    u64::try_from(pack.members.len()).map_err(|_| PithosError::IntegerOverflow)?,
                )
                .ok_or(PithosError::IntegerOverflow)?;
            if total_members > config.max_chunks {
                return Err(PithosError::ResourceLimit("chunk count"));
            }
            total_content = total_content
                .checked_add(pack.uncompressed_len)
                .ok_or(PithosError::IntegerOverflow)?;
            if total_content > config.max_logical_bytes {
                return Err(PithosError::ResourceLimit("logical bytes"));
            }
        }

        let total_ids_u64 = total_members
            .checked_add(
                u64::try_from(self.excluded.len()).map_err(|_| PithosError::IntegerOverflow)?,
            )
            .ok_or(PithosError::IntegerOverflow)?;
        if total_ids_u64 > config.max_chunks {
            return Err(PithosError::ResourceLimit("chunk count"));
        }
        let total_ids = usize::try_from(total_ids_u64).map_err(|_| PithosError::IntegerOverflow)?;
        let mut entry_ids = Vec::new();
        try_reserve_exact(&mut entry_ids, total_ids)?;
        for (index, pack) in self.packs.iter().enumerate() {
            checkpoint()?;
            if pack.pack_id != u64::try_from(index).map_err(|_| PithosError::IntegerOverflow)? {
                return Err(PithosError::InvalidMetadata("microfile pack ID"));
            }
            pack.validate_with_checkpoint(config, &mut checkpoint)?;
            for member in &pack.members {
                checkpoint()?;
                entry_ids.push(member.entry_id);
            }
        }
        for entry in &self.excluded {
            checkpoint()?;
            entry_ids.push(entry.entry_id);
        }
        crate::chunking::try_sort_by_checkpoint(&mut entry_ids, Ord::cmp, &mut checkpoint)?;
        for pair in entry_ids.windows(2) {
            checkpoint()?;
            if pair[0] == pair[1] {
                return Err(PithosError::InvalidMetadata("duplicate microfile entry id"));
            }
        }
        Ok(())
    }
}

/// Converts every eligible MicroFilePack member into one logical chunk draft.
/// The caller then combines these drafts with other objects and invokes
/// [`crate::assign_chunk_ids`] for global deterministic IDs.
pub fn micro_file_logical_chunks(
    plan: &MicroFilePackPlan,
    object_id: u64,
    config: &ChunkingConfig,
) -> Result<Vec<crate::LogicalChunkDraft>> {
    micro_file_logical_chunks_with_checkpoint(plan, object_id, config, || Ok(()))
}

/// Converts pack members to logical chunks with cooperative validation and
/// materialization checkpoints.
pub fn micro_file_logical_chunks_with_checkpoint<F>(
    plan: &MicroFilePackPlan,
    object_id: u64,
    config: &ChunkingConfig,
    mut checkpoint: F,
) -> Result<Vec<crate::LogicalChunkDraft>>
where
    F: FnMut() -> Result<()>,
{
    plan.validate_with_checkpoint(config, &mut checkpoint)?;
    let mut member_count = 0_usize;
    for pack in &plan.packs {
        checkpoint()?;
        member_count = member_count
            .checked_add(pack.members.len())
            .ok_or(PithosError::IntegerOverflow)?;
    }
    let mut drafts = Vec::new();
    try_reserve_exact(&mut drafts, member_count)?;
    for pack in &plan.packs {
        for member in &pack.members {
            checkpoint()?;
            drafts.push(crate::LogicalChunkDraft {
                entry_id: member.entry_id,
                object_id,
                logical_offset: 0,
                length: member.length,
                method: crate::ChunkingMethod::MicroFile,
            });
        }
    }
    Ok(drafts)
}

/// Produces a canonical metadata plan for microfile payload aggregation.
///
/// Grouping signals determine proximity only. A pack boundary is introduced
/// solely when adding the next eligible file would exceed the configured target.
pub fn plan_micro_file_packs(
    inputs: &[MicroFileInput],
    config: &ChunkingConfig,
) -> Result<MicroFilePackPlan> {
    plan_micro_file_packs_with_checkpoint(inputs, config, || Ok(()))
}

/// MicroFilePack planning with cooperative checkpoints around sorting and in
/// every linear construction pass.
pub fn plan_micro_file_packs_with_checkpoint<F>(
    inputs: &[MicroFileInput],
    config: &ChunkingConfig,
    mut checkpoint: F,
) -> Result<MicroFilePackPlan>
where
    F: FnMut() -> Result<()>,
{
    config.validate()?;
    checkpoint()?;
    let input_count = u64::try_from(inputs.len()).map_err(|_| PithosError::IntegerOverflow)?;
    if input_count > config.max_chunks {
        return Err(PithosError::ResourceLimit("chunk count"));
    }
    let minimum_metadata_bytes = input_count
        .checked_mul(76)
        .ok_or(PithosError::IntegerOverflow)?;
    ensure_metadata_size(minimum_metadata_bytes, config)?;
    validate_unique_inputs(inputs, config, &mut checkpoint)?;

    let mut ordered = Vec::new();
    try_reserve_exact(&mut ordered, inputs.len())?;
    for input in inputs {
        checkpoint()?;
        ordered.push(input);
    }
    crate::chunking::try_sort_by_checkpoint(
        &mut ordered,
        |left, right| canonical_key(left).cmp(&canonical_key(right)),
        &mut checkpoint,
    )?;

    let mut eligible = Vec::new();
    try_reserve_exact(&mut eligible, inputs.len())?;
    let mut excluded = Vec::new();
    try_reserve_exact(&mut excluded, inputs.len())?;

    let mut eligible_bytes = 0_u64;
    for candidate in ordered {
        checkpoint()?;
        let reason = if candidate.requires_isolated_access {
            Some(MicroFileExclusionReason::RequiresIsolatedAccess)
        } else if candidate.size > u64::from(config.micro_file_max) {
            Some(MicroFileExclusionReason::TooLarge)
        } else {
            None
        };

        if let Some(reason) = reason {
            excluded.push(MicroFileExclusion {
                entry_id: candidate.entry_id,
                reason,
            });
        } else {
            eligible_bytes = eligible_bytes
                .checked_add(candidate.size)
                .ok_or(PithosError::IntegerOverflow)?;
            if eligible_bytes > config.max_logical_bytes {
                return Err(PithosError::ResourceLimit("logical bytes"));
            }
            eligible.push(candidate);
        }
    }

    let target = u64::from(config.micro_pack_target);
    let required_packs = required_pack_count(&eligible, target, &mut checkpoint)?;
    let required_packs_u64 =
        u64::try_from(required_packs).map_err(|_| PithosError::IntegerOverflow)?;
    if required_packs_u64 > config.max_chunks {
        return Err(PithosError::ResourceLimit("chunk count"));
    }

    let mut packs = Vec::new();
    try_reserve_exact(&mut packs, required_packs)?;
    let mut pack_start = 0_usize;
    let mut pack_len = 0_u64;

    for (index, candidate) in eligible.iter().enumerate() {
        checkpoint()?;
        let next_len = pack_len
            .checked_add(candidate.size)
            .ok_or(PithosError::IntegerOverflow)?;

        if index > pack_start && next_len > target {
            push_pack(
                &mut packs,
                &eligible[pack_start..index],
                pack_len,
                config,
                &mut checkpoint,
            )?;
            pack_start = index;
            pack_len = candidate.size;
        } else {
            pack_len = next_len;
        }
    }

    if pack_start < eligible.len() {
        push_pack(
            &mut packs,
            &eligible[pack_start..],
            pack_len,
            config,
            &mut checkpoint,
        )?;
    }

    let plan = MicroFilePackPlan { packs, excluded };
    checkpoint()?;
    plan.validate_with_checkpoint(config, &mut checkpoint)?;
    Ok(plan)
}

fn required_pack_count<F>(
    inputs: &[&MicroFileInput],
    target: u64,
    checkpoint: &mut F,
) -> Result<usize>
where
    F: FnMut() -> Result<()>,
{
    let mut count = 0_usize;
    let mut current_len = 0_u64;
    let mut has_members = false;

    for input in inputs {
        checkpoint()?;
        let next_len = current_len
            .checked_add(input.size)
            .ok_or(PithosError::IntegerOverflow)?;
        if has_members && next_len > target {
            count = count.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
            current_len = input.size;
        } else {
            current_len = next_len;
        }
        has_members = true;
    }

    if has_members {
        count = count.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
    }
    Ok(count)
}

type CanonicalKey<'a> = (u64, &'a [u8], &'a [u8], u32, u64, &'a [u8], u64);

fn canonical_key(input: &MicroFileInput) -> CanonicalKey<'_> {
    (
        input.family_key,
        &input.path_prefix_key,
        &input.extension_key,
        input.mode,
        input.similarity_key,
        &input.path,
        input.entry_id,
    )
}

fn validate_unique_inputs<F>(
    inputs: &[MicroFileInput],
    config: &ChunkingConfig,
    checkpoint: &mut F,
) -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    let input_count = u64::try_from(inputs.len()).map_err(|_| PithosError::IntegerOverflow)?;
    let base_metadata_bytes = input_count
        .checked_mul(76)
        .ok_or(PithosError::IntegerOverflow)?;
    ensure_metadata_size(base_metadata_bytes, config)?;

    let mut input_metadata_bytes = 0_u64;
    for input in inputs {
        checkpoint()?;
        if input.path.is_empty() {
            return Err(PithosError::InvalidMetadata("empty microfile path"));
        }
        let path_len = u64::try_from(input.path.len()).map_err(|_| PithosError::IntegerOverflow)?;
        let prefix_len =
            u64::try_from(input.path_prefix_key.len()).map_err(|_| PithosError::IntegerOverflow)?;
        let extension_len =
            u64::try_from(input.extension_key.len()).map_err(|_| PithosError::IntegerOverflow)?;
        if path_len > config.max_path_bytes
            || prefix_len > config.max_path_bytes
            || extension_len > config.max_path_bytes
        {
            return Err(PithosError::ResourceLimit("path bytes"));
        }
        input_metadata_bytes = input_metadata_bytes
            .checked_add(path_len)
            .and_then(|value| value.checked_add(prefix_len))
            .and_then(|value| value.checked_add(extension_len))
            .ok_or(PithosError::IntegerOverflow)?;
        ensure_metadata_size(
            base_metadata_bytes
                .checked_add(input_metadata_bytes)
                .ok_or(PithosError::IntegerOverflow)?,
            config,
        )?;
    }

    let mut entry_ids = Vec::new();
    try_reserve_exact(&mut entry_ids, inputs.len())?;
    for input in inputs {
        checkpoint()?;
        entry_ids.push(input.entry_id);
    }
    crate::chunking::try_sort_by_checkpoint(&mut entry_ids, Ord::cmp, checkpoint)?;
    for pair in entry_ids.windows(2) {
        checkpoint()?;
        if pair[0] == pair[1] {
            return Err(PithosError::InvalidMetadata("duplicate microfile entry id"));
        }
    }

    let mut paths = Vec::new();
    try_reserve_exact(&mut paths, inputs.len())?;
    for input in inputs {
        checkpoint()?;
        paths.push(input.path.as_slice());
    }
    crate::chunking::try_sort_by_checkpoint(&mut paths, Ord::cmp, checkpoint)?;
    for pair in paths.windows(2) {
        checkpoint()?;
        if pair[0] == pair[1] {
            return Err(PithosError::InvalidMetadata("duplicate microfile path"));
        }
    }
    Ok(())
}

fn push_pack<F>(
    packs: &mut Vec<MicroFilePack>,
    inputs: &[&MicroFileInput],
    expected_len: u64,
    config: &ChunkingConfig,
    checkpoint: &mut F,
) -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    checkpoint()?;
    let pack_id = u64::try_from(packs.len()).map_err(|_| PithosError::IntegerOverflow)?;
    let next_count = pack_id.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
    if next_count > config.max_chunks {
        return Err(PithosError::ResourceLimit("chunk count"));
    }

    let pack = build_pack(pack_id, inputs, checkpoint)?;
    if pack.uncompressed_len != expected_len {
        return Err(PithosError::InvalidMetadata("microfile pack length"));
    }
    packs.push(pack);
    Ok(())
}

fn build_pack<F>(
    pack_id: u64,
    inputs: &[&MicroFileInput],
    checkpoint: &mut F,
) -> Result<MicroFilePack>
where
    F: FnMut() -> Result<()>,
{
    checkpoint()?;
    let mut input_iter = inputs.iter();
    let mut base_modified_ns = input_iter
        .next()
        .ok_or(PithosError::InvalidMetadata("empty microfile pack"))?
        .modified_ns;
    for input in input_iter {
        checkpoint()?;
        base_modified_ns = base_modified_ns.min(input.modified_ns);
    }

    let mut mode_dictionary = Vec::new();
    try_reserve_exact(&mut mode_dictionary, inputs.len())?;
    for input in inputs {
        checkpoint()?;
        mode_dictionary.push(input.mode);
    }
    crate::chunking::try_sort_by_checkpoint(&mut mode_dictionary, Ord::cmp, checkpoint)?;
    dedup_sorted_with_checkpoint(&mut mode_dictionary, checkpoint)?;

    let mut members = Vec::new();
    let mut paths = Vec::new();
    let mut modified_ns_deltas = Vec::new();
    let mut records = Vec::new();
    try_reserve_exact(&mut members, inputs.len())?;
    try_reserve_exact(&mut paths, inputs.len())?;
    try_reserve_exact(&mut modified_ns_deltas, inputs.len())?;
    try_reserve_exact(&mut records, inputs.len())?;

    let mut content_offset = 0_u64;
    let mut previous_path: &[u8] = &[];
    for input in inputs {
        checkpoint()?;
        let length = u32::try_from(input.size).map_err(|_| PithosError::IntegerOverflow)?;
        let shared_prefix_len =
            common_prefix_len_with_checkpoint(previous_path, &input.path, checkpoint)?;
        let shared_prefix_len =
            u32::try_from(shared_prefix_len).map_err(|_| PithosError::IntegerOverflow)?;
        let suffix_start =
            usize::try_from(shared_prefix_len).map_err(|_| PithosError::IntegerOverflow)?;
        let mut suffix = Vec::new();
        try_reserve_exact(&mut suffix, input.path.len() - suffix_start)?;
        suffix.extend_from_slice(&input.path[suffix_start..]);
        paths.push(FrontCodedPath {
            shared_prefix_len,
            suffix,
        });

        let delta = i128::from(input.modified_ns) - i128::from(base_modified_ns);
        let delta = u64::try_from(delta).map_err(|_| PithosError::IntegerOverflow)?;
        modified_ns_deltas.push(delta);

        let mode_index = mode_dictionary
            .binary_search(&input.mode)
            .map_err(|_| PithosError::InvalidMetadata("microfile mode dictionary"))?;
        let mode_index = u32::try_from(mode_index).map_err(|_| PithosError::IntegerOverflow)?;

        members.push(MicroFilePackMember {
            entry_id: input.entry_id,
            content_offset,
            length,
        });
        records.push(MicroFileMetadataRecord {
            entry_id: input.entry_id,
            mode_index,
            content_offset,
            length,
            file_hash: input.file_hash,
        });

        content_offset = content_offset
            .checked_add(input.size)
            .ok_or(PithosError::IntegerOverflow)?;
        previous_path = &input.path;
    }

    Ok(MicroFilePack {
        pack_id,
        members,
        uncompressed_len: content_offset,
        metadata: MicroFilePackMetadata {
            paths,
            base_modified_ns,
            modified_ns_deltas,
            mode_dictionary,
            records,
        },
    })
}

fn common_prefix_len_with_checkpoint<F>(
    left: &[u8],
    right: &[u8],
    checkpoint: &mut F,
) -> Result<usize>
where
    F: FnMut() -> Result<()>,
{
    let mut length = 0_usize;
    for (left_byte, right_byte) in left.iter().zip(right) {
        checkpoint()?;
        if left_byte != right_byte {
            break;
        }
        length = length.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
    }
    Ok(length)
}

fn dedup_sorted_with_checkpoint<T, F>(items: &mut Vec<T>, checkpoint: &mut F) -> Result<()>
where
    T: Copy + Eq,
    F: FnMut() -> Result<()>,
{
    if items.len() < 2 {
        checkpoint()?;
        return Ok(());
    }
    let mut write = 1_usize;
    for read in 1..items.len() {
        checkpoint()?;
        if items[read] != items[write - 1] {
            items[write] = items[read];
            write += 1;
        }
    }
    items.truncate(write);
    Ok(())
}

fn try_reserve_exact<T>(items: &mut Vec<T>, additional: usize) -> Result<()> {
    items
        .try_reserve_exact(additional)
        .map_err(|_| PithosError::MemoryLimit)
}

fn ensure_metadata_size(size: u64, config: &ChunkingConfig) -> Result<()> {
    if size > config.max_metadata_bytes {
        Err(PithosError::ResourceLimit("metadata bytes"))
    } else {
        Ok(())
    }
}
