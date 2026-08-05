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
        let record_count = self.records.len();
        if self.paths.len() != record_count || self.modified_ns_deltas.len() != record_count {
            return Err(PithosError::InvalidMetadata("microfile metadata columns"));
        }
        if record_count > 0 && self.mode_dictionary.is_empty() {
            return Err(PithosError::InvalidMetadata("microfile mode dictionary"));
        }
        if self
            .mode_dictionary
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(PithosError::InvalidMetadata("microfile mode dictionary"));
        }

        let mut paths = Vec::new();
        try_reserve_exact(&mut paths, record_count)?;
        let mut previous = Vec::new();
        let mut entry_ids = Vec::new();
        try_reserve_exact(&mut entry_ids, record_count)?;
        let mut expected_offset = 0_u64;

        for ((path, delta), record) in self
            .paths
            .iter()
            .zip(&self.modified_ns_deltas)
            .zip(&self.records)
        {
            let prefix = usize::try_from(path.shared_prefix_len)
                .map_err(|_| PithosError::IntegerOverflow)?;
            if prefix > previous.len() {
                return Err(PithosError::InvalidMetadata("front-coded microfile path"));
            }
            let path_len = prefix
                .checked_add(path.suffix.len())
                .ok_or(PithosError::IntegerOverflow)?;
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
            previous = expanded.clone();
            paths.push(expanded);
        }

        entry_ids.sort_unstable();
        if entry_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(PithosError::InvalidMetadata("duplicate microfile entry id"));
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
        config.validate()?;
        if self.members.is_empty()
            || self.members.len() != self.metadata.records.len()
            || self.uncompressed_len > u64::from(config.micro_pack_target)
        {
            return Err(PithosError::InvalidMetadata("microfile pack layout"));
        }
        self.metadata.expanded_paths()?;

        let mut expected_offset = 0_u64;
        for (member, record) in self.members.iter().zip(&self.metadata.records) {
            if member.entry_id != record.entry_id
                || member.content_offset != record.content_offset
                || member.length != record.length
                || member.content_offset != expected_offset
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
        config.validate()?;
        let pack_count =
            u64::try_from(self.packs.len()).map_err(|_| PithosError::IntegerOverflow)?;
        if pack_count > config.max_chunks {
            return Err(PithosError::ResourceLimit("chunk count"));
        }

        let total_members = self.packs.iter().try_fold(0_u64, |total, pack| {
            total
                .checked_add(
                    u64::try_from(pack.members.len()).map_err(|_| PithosError::IntegerOverflow)?,
                )
                .ok_or(PithosError::IntegerOverflow)
        })?;
        if total_members > config.max_chunks {
            return Err(PithosError::ResourceLimit("chunk count"));
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
            if pack.pack_id != u64::try_from(index).map_err(|_| PithosError::IntegerOverflow)? {
                return Err(PithosError::InvalidMetadata("microfile pack ID"));
            }
            pack.validate(config)?;
            entry_ids.extend(pack.members.iter().map(|member| member.entry_id));
        }
        entry_ids.extend(self.excluded.iter().map(|entry| entry.entry_id));
        entry_ids.sort_unstable();
        if entry_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(PithosError::InvalidMetadata("duplicate microfile entry id"));
        }
        Ok(())
    }
}

/// Produces a canonical metadata plan for microfile payload aggregation.
///
/// Grouping signals determine proximity only. A pack boundary is introduced
/// solely when adding the next eligible file would exceed the configured target.
pub fn plan_micro_file_packs(
    inputs: &[MicroFileInput],
    config: &ChunkingConfig,
) -> Result<MicroFilePackPlan> {
    config.validate()?;
    let input_count = u64::try_from(inputs.len()).map_err(|_| PithosError::IntegerOverflow)?;
    if input_count > config.max_chunks {
        return Err(PithosError::ResourceLimit("chunk count"));
    }
    validate_unique_inputs(inputs)?;

    let mut ordered = Vec::new();
    try_reserve_exact(&mut ordered, inputs.len())?;
    ordered.extend(inputs);
    ordered.sort_unstable_by(|left, right| canonical_key(left).cmp(&canonical_key(right)));

    let mut eligible = Vec::new();
    try_reserve_exact(&mut eligible, inputs.len())?;
    let mut excluded = Vec::new();
    try_reserve_exact(&mut excluded, inputs.len())?;

    for candidate in ordered {
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
            eligible.push(candidate);
        }
    }

    let target = u64::from(config.micro_pack_target);
    let required_packs = required_pack_count(&eligible, target)?;
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
        let next_len = pack_len
            .checked_add(candidate.size)
            .ok_or(PithosError::IntegerOverflow)?;

        if index > pack_start && next_len > target {
            push_pack(
                &mut packs,
                &eligible[pack_start..index],
                pack_len,
                config.max_chunks,
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
            config.max_chunks,
        )?;
    }

    let plan = MicroFilePackPlan { packs, excluded };
    plan.validate(config)?;
    Ok(plan)
}

fn required_pack_count(inputs: &[&MicroFileInput], target: u64) -> Result<usize> {
    let mut count = 0_usize;
    let mut current_len = 0_u64;
    let mut has_members = false;

    for input in inputs {
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

fn validate_unique_inputs(inputs: &[MicroFileInput]) -> Result<()> {
    if inputs.iter().any(|input| input.path.is_empty()) {
        return Err(PithosError::InvalidMetadata("empty microfile path"));
    }
    let mut entry_ids = Vec::new();
    try_reserve_exact(&mut entry_ids, inputs.len())?;
    entry_ids.extend(inputs.iter().map(|input| input.entry_id));
    entry_ids.sort_unstable();
    if entry_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(PithosError::InvalidMetadata("duplicate microfile entry id"));
    }

    let mut paths = Vec::new();
    try_reserve_exact(&mut paths, inputs.len())?;
    paths.extend(inputs.iter().map(|input| input.path.as_slice()));
    paths.sort_unstable();
    if paths.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(PithosError::InvalidMetadata("duplicate microfile path"));
    }
    Ok(())
}

fn push_pack(
    packs: &mut Vec<MicroFilePack>,
    inputs: &[&MicroFileInput],
    expected_len: u64,
    max_chunks: u64,
) -> Result<()> {
    let pack_id = u64::try_from(packs.len()).map_err(|_| PithosError::IntegerOverflow)?;
    let next_count = pack_id.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
    if next_count > max_chunks {
        return Err(PithosError::ResourceLimit("chunk count"));
    }

    let pack = build_pack(pack_id, inputs)?;
    if pack.uncompressed_len != expected_len {
        return Err(PithosError::InvalidMetadata("microfile pack length"));
    }
    packs.push(pack);
    Ok(())
}

fn build_pack(pack_id: u64, inputs: &[&MicroFileInput]) -> Result<MicroFilePack> {
    let base_modified_ns = inputs
        .iter()
        .map(|input| input.modified_ns)
        .min()
        .ok_or(PithosError::InvalidMetadata("empty microfile pack"))?;

    let mut mode_dictionary = Vec::new();
    try_reserve_exact(&mut mode_dictionary, inputs.len())?;
    mode_dictionary.extend(inputs.iter().map(|input| input.mode));
    mode_dictionary.sort_unstable();
    mode_dictionary.dedup();

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
        let length = u32::try_from(input.size).map_err(|_| PithosError::IntegerOverflow)?;
        let shared_prefix_len = common_prefix_len(previous_path, &input.path);
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

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left_byte, right_byte)| left_byte == right_byte)
        .count()
}

fn try_reserve_exact<T>(items: &mut Vec<T>, additional: usize) -> Result<()> {
    items
        .try_reserve_exact(additional)
        .map_err(|_| PithosError::MemoryLimit)
}
