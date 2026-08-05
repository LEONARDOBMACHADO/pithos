//! RAW/STORE pack, verification and transactional unpack orchestration.

pub mod scheduler;

use pithos_codecs::{BrotliCodec, Codec, CodecConfig, CodecId, Lzma2Codec, StoreCodec, ZstdCodec};
use pithos_core::{CompressionProfile, DecodeLimits, PithosError, Result};
use pithos_format::{
    ArchivePath, CODEC_FLAG_REQUIRED, CentralIndexRecord, CodecRegistry, CodecRegistryRecord,
    EntryKind, EntryRecord, FOOTER_LEN, Footer, GlobalHeader, GroupRecord, GroupTableRecord,
    HEADER_LEN, IntegrityRecord, LinkComponent, LinkTarget, REQUIRED_COMPRESSED_SECTIONS,
    REQUIRED_RAW_SECTIONS, RestoreMapRecord, SECTION_ENTRY_LEN, SectionDirectoryRecord,
    SectionType,
};
use pithos_io::{atomic_commit, create_atomic_spool};
use pithos_planner::{CandidateCost, SolidGroupPlan, plan_solid_groups, solid_group_target};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tracing::info;

const IO_BUFFER_SIZE: usize = 64 * 1024;
const RAW_SECTION_TYPES: [SectionType; 6] = [
    SectionType::EntryTable,
    SectionType::GroupTable,
    SectionType::PayloadArea,
    SectionType::RestoreMap,
    SectionType::CentralIndex,
    SectionType::IntegrityTree,
];

pub struct PackRequest {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub profile: CompressionProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackLimits {
    pub max_input_bytes: u64,
    pub max_memory_bytes: u64,
    pub max_temp_bytes: u64,
    pub max_output_bytes: u64,
    pub max_metadata_bytes: u64,
    pub max_entries: u64,
}

impl Default for PackLimits {
    fn default() -> Self {
        let decode = DecodeLimits::default();
        Self {
            max_input_bytes: decode.max_original_bytes,
            max_memory_bytes: u64::MAX,
            max_temp_bytes: u64::MAX,
            max_output_bytes: u64::MAX,
            max_metadata_bytes: decode.max_metadata_bytes,
            max_entries: decode.max_entries,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackResourceEstimate {
    pub estimated_memory: u64,
    pub estimated_temp: u64,
    pub output_upper_bound: u64,
}

pub fn estimate_pack_resources(
    profile: CompressionProfile,
    input_bytes: u64,
    max_file_bytes: u64,
    entry_count: u64,
) -> Result<PackResourceEstimate> {
    const ENTRY_OUTPUT_OVERHEAD: u64 = 1024 * 1024;
    const FIXED_OUTPUT_OVERHEAD: u64 = 16 * 1024 * 1024;
    const SCAN_ENTRY_MEMORY: u64 = 512;

    let metadata_upper_bound = entry_count
        .checked_mul(ENTRY_OUTPUT_OVERHEAD)
        .and_then(|value| value.checked_add(FIXED_OUTPUT_OVERHEAD))
        .ok_or(PithosError::IntegerOverflow)?;
    let output_upper_bound = input_bytes
        .checked_add(metadata_upper_bound)
        .ok_or(PithosError::IntegerOverflow)?;
    let scan_memory = entry_count
        .checked_mul(SCAN_ENTRY_MEMORY)
        .and_then(|value| value.checked_add(IO_BUFFER_SIZE as u64))
        .ok_or(PithosError::IntegerOverflow)?;

    if profile == CompressionProfile::Raw {
        return Ok(PackResourceEstimate {
            estimated_memory: scan_memory,
            estimated_temp: output_upper_bound,
            output_upper_bound,
        });
    }

    let target = solid_group_target(profile);
    let largest_group = max_file_bytes.max(input_bytes.min(target));
    let codec_memory =
        profile_codecs(profile)
            .into_iter()
            .try_fold(0_u64, |maximum, codec_id| {
                let config = CodecConfig::deterministic_default(codec_id);
                Ok::<u64, PithosError>(
                    maximum.max(codec_for_id(codec_id).memory_bound(largest_group, &config)?),
                )
            })?;
    let selected_output_bound = largest_group
        .checked_add(ENTRY_OUTPUT_OVERHEAD + 2)
        .ok_or(PithosError::IntegerOverflow)?;
    let estimated_memory = codec_memory
        .checked_add(selected_output_bound)
        .ok_or(PithosError::IntegerOverflow)?
        .max(scan_memory);
    let estimated_temp = input_bytes
        .checked_add(output_upper_bound)
        .ok_or(PithosError::IntegerOverflow)?;

    Ok(PackResourceEstimate {
        estimated_memory,
        estimated_temp,
        output_upper_bound,
    })
}

pub struct UnpackRequest {
    pub archive: PathBuf,
    pub output_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerificationReport {
    pub archive_bytes: u64,
    pub original_bytes: u64,
    pub entry_count: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub hardlink_count: u64,
    pub symlink_count: u64,
    pub group_count: u64,
    pub blake3_root: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveEntryKind {
    File,
    Directory,
    Hardlink,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveEntrySummary {
    pub entry_id: u64,
    pub path: String,
    pub kind: ArchiveEntryKind,
    pub size: u64,
    pub modified_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveInspection {
    pub archive_bytes: u64,
    pub original_bytes: u64,
    pub entry_count: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub hardlink_count: u64,
    pub symlink_count: u64,
    pub group_count: u64,
    pub format_version: String,
    pub metadata_verified: bool,
}

pub struct ExtractRequest {
    pub archive: PathBuf,
    pub entry: PathBuf,
    pub output_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtractReport {
    pub path: String,
    pub bytes_written: u64,
    pub kind: ArchiveEntryKind,
}

pub struct ReadRangeRequest {
    pub archive: PathBuf,
    pub entry: PathBuf,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadRangeReport {
    pub path: String,
    pub offset: u64,
    pub length: u64,
    pub entry_size: u64,
    /// BLAKE3 of exactly the bytes written for the requested range.
    pub blake3: [u8; 32],
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn checkpoint(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(PithosError::Cancelled)
        } else {
            Ok(())
        }
    }
}

pub fn pack(request: PackRequest) -> Result<()> {
    pack_with_control(request, &CancellationToken::new())
}

pub fn pack_with_control(request: PackRequest, cancellation: &CancellationToken) -> Result<()> {
    pack_with_limits_and_control(request, &PackLimits::default(), cancellation)
}

pub fn pack_with_limits_and_control(
    request: PackRequest,
    pack_limits: &PackLimits,
    cancellation: &CancellationToken,
) -> Result<()> {
    cancellation.checkpoint()?;
    if request.profile != CompressionProfile::Raw {
        return pack_compressed_with_limits(request, pack_limits, cancellation);
    }
    if request.inputs.is_empty() {
        return Err(PithosError::InvalidMetadata("nenhuma entrada"));
    }
    if path_entry_exists(&request.output)? {
        return Err(PithosError::OutputExists);
    }

    if pack_limits.max_temp_bytes == 0
        || pack_limits.max_output_bytes == 0
        || pack_limits.max_metadata_bytes == 0
        || pack_limits.max_entries == 0
    {
        return Err(PithosError::ResourceLimit("pack budget"));
    }
    let defaults = DecodeLimits::default();
    let decode_limits = DecodeLimits {
        max_entries: pack_limits.max_entries.min(defaults.max_entries),
        max_groups: pack_limits.max_entries.min(defaults.max_groups),
        max_chunks: pack_limits.max_entries.min(defaults.max_chunks),
        max_original_bytes: pack_limits.max_input_bytes,
        max_group_output: pack_limits.max_input_bytes.min(defaults.max_group_output),
        max_metadata_bytes: pack_limits
            .max_metadata_bytes
            .min(defaults.max_metadata_bytes),
        ..defaults
    };
    let archive_budget = pack_limits.max_temp_bytes.min(pack_limits.max_output_bytes);
    let scanned = scan_inputs(&request.inputs, &decode_limits, cancellation)?;
    let parent = request.output.parent().unwrap_or_else(|| Path::new("."));
    let mut spool = create_atomic_spool(parent)?;
    let directory_length = u64::from(REQUIRED_RAW_SECTIONS)
        .checked_mul(SECTION_ENTRY_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let payload_start = (HEADER_LEN as u64)
        .checked_add(directory_length)
        .ok_or(PithosError::IntegerOverflow)?;
    if payload_start > archive_budget {
        return Err(PithosError::ResourceLimit("archive output"));
    }
    spool.seek(SeekFrom::Start(payload_start))?;

    let mut entries = Vec::with_capacity(scanned.len());
    let mut groups = Vec::new();
    let mut restore_map = Vec::new();
    let mut hardlinks: HashMap<Arc<same_file::Handle>, (u64, u64, [u8; 32])> = HashMap::new();
    let mut payload_crc = 0_u32;
    let mut original_total_size = 0_u64;

    for scanned_entry in scanned {
        cancellation.checkpoint()?;
        let entry_id = entries.len() as u64;
        let modified_ns = modified_ns(&scanned_entry.metadata);
        let mode = metadata_mode(&scanned_entry.metadata);
        let (size, hash, kind) = match scanned_entry.kind {
            ScannedKind::Directory => (
                0,
                *blake3::hash(b"directory").as_bytes(),
                EntryKind::Directory,
            ),
            ScannedKind::Symlink {
                target,
                target_is_dir,
            } => {
                let serialized = serialize(&target)?;
                (
                    serialized.len() as u64,
                    *blake3::hash(&serialized).as_bytes(),
                    EntryKind::Symlink {
                        target,
                        target_is_dir,
                    },
                )
            }
            ScannedKind::File => {
                let expected_size = scanned_entry.metadata.len();
                original_total_size = original_total_size
                    .checked_add(expected_size)
                    .ok_or(PithosError::IntegerOverflow)?;
                if original_total_size > decode_limits.max_original_bytes {
                    return Err(PithosError::ResourceLimit("tamanho original"));
                }
                let identity = scanned_entry.identity.clone();
                if let Some((target_entry_id, target_size, target_hash)) = identity
                    .as_ref()
                    .and_then(|key| hardlinks.get(key))
                    .copied()
                {
                    (
                        target_size,
                        target_hash,
                        EntryKind::Hardlink { target_entry_id },
                    )
                } else {
                    if expected_size > decode_limits.max_group_output {
                        return Err(PithosError::ResourceLimit("tamanho do grupo"));
                    }
                    let group_id = groups.len() as u64;
                    let payload_offset = spool.stream_position()?;
                    if payload_offset
                        .checked_add(expected_size)
                        .is_none_or(|end| end > archive_budget)
                    {
                        return Err(PithosError::ResourceLimit("archive output"));
                    }
                    let (actual_size, file_hash, file_crc) = copy_input_file(
                        &scanned_entry.source,
                        &scanned_entry.metadata,
                        identity.as_ref(),
                        &mut spool,
                        &mut payload_crc,
                        cancellation,
                    )?;
                    let group = GroupRecord {
                        version: 1,
                        flags: 0,
                        group_id,
                        codec_chain_id: 0,
                        chunk_count: 1,
                        uncompressed_len: actual_size,
                        compressed_len: actual_size,
                        descriptor_len: 0,
                        payload_crc32c: file_crc,
                    };
                    groups.push(GroupTableRecord {
                        group,
                        payload_offset,
                    });
                    restore_map.push(RestoreMapRecord {
                        entry_id,
                        original_offset: 0,
                        length: actual_size,
                        group_id,
                        group_offset: 0,
                    });
                    if let Some(identity) = identity {
                        hardlinks.insert(identity, (entry_id, actual_size, file_hash));
                    }
                    (actual_size, file_hash, EntryKind::File { group_id })
                }
            }
        };
        entries.push(EntryRecord {
            entry_id,
            path: scanned_entry.archive_path,
            size,
            modified_ns,
            mode,
            blake3: hash,
            kind,
        });
    }

    let payload_end = spool.stream_position()?;
    let payload_length = payload_end
        .checked_sub(payload_start)
        .ok_or(PithosError::IntegerOverflow)?;
    let central_index = entries
        .iter()
        .map(|entry| CentralIndexRecord {
            path: entry.path.clone(),
            entry_id: entry.entry_id,
            group_id: match entry.kind {
                EntryKind::File { group_id } => Some(group_id),
                _ => None,
            },
        })
        .collect::<Vec<_>>();
    let integrity = entries
        .iter()
        .map(|entry| IntegrityRecord {
            entry_id: entry.entry_id,
            blake3: entry.blake3,
        })
        .collect::<Vec<_>>();

    let entry_bytes = serialize(&entries)?;
    let group_bytes = serialize(&groups)?;
    let restore_bytes = serialize(&restore_map)?;
    let index_bytes = serialize(&central_index)?;
    let integrity_bytes = serialize(&integrity)?;
    for bytes in [
        &entry_bytes,
        &group_bytes,
        &restore_bytes,
        &index_bytes,
        &integrity_bytes,
    ] {
        if bytes.len() as u64 > decode_limits.max_metadata_bytes {
            return Err(PithosError::ResourceLimit("seção de metadados"));
        }
    }
    let metadata_total = [
        &entry_bytes,
        &group_bytes,
        &restore_bytes,
        &index_bytes,
        &integrity_bytes,
    ]
    .iter()
    .try_fold(0_u64, |total, bytes| {
        total
            .checked_add(bytes.len() as u64)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    if metadata_total > decode_limits.max_metadata_bytes {
        return Err(PithosError::ResourceLimit("aggregate metadata"));
    }
    let expected_archive_length = payload_end
        .checked_add(metadata_total)
        .and_then(|length| length.checked_add(FOOTER_LEN as u64))
        .ok_or(PithosError::IntegerOverflow)?;
    if expected_archive_length > archive_budget {
        return Err(PithosError::ResourceLimit("archive output"));
    }

    let mut sections = Vec::with_capacity(REQUIRED_RAW_SECTIONS as usize);
    sections.push(write_section(
        &mut spool,
        SectionType::EntryTable,
        &entry_bytes,
    )?);
    sections.push(write_section(
        &mut spool,
        SectionType::GroupTable,
        &group_bytes,
    )?);
    sections.push(SectionDirectoryRecord {
        section_type: SectionType::PayloadArea as u16,
        section_version: 1,
        flags: 0,
        offset: payload_start,
        length: payload_length,
        crc32c: payload_crc,
        reserved: 0,
    });
    sections.push(write_section(
        &mut spool,
        SectionType::RestoreMap,
        &restore_bytes,
    )?);
    sections.push(write_section(
        &mut spool,
        SectionType::CentralIndex,
        &index_bytes,
    )?);
    sections.push(write_section(
        &mut spool,
        SectionType::IntegrityTree,
        &integrity_bytes,
    )?);
    sections.sort_by_key(|section| section.section_type);

    let footer_offset = spool.stream_position()?;
    let mut identity_hasher = blake3::Hasher::new();
    for bytes in [
        &entry_bytes,
        &group_bytes,
        &restore_bytes,
        &index_bytes,
        &integrity_bytes,
    ] {
        identity_hasher.update(bytes);
    }
    let identity_hash = identity_hasher.finalize();
    let mut archive_id = [0_u8; 16];
    archive_id.copy_from_slice(&identity_hash.as_bytes()[..16]);
    let mut header = GlobalHeader::new(archive_id);
    header.original_total_size = original_total_size;
    header.entry_count = entries.len() as u64;
    header.logical_chunk_count = groups.len() as u64;
    header.group_count = groups.len() as u64;
    header.footer_offset = footer_offset;

    let directory_bytes = sections
        .iter()
        .flat_map(SectionDirectoryRecord::encode)
        .collect::<Vec<_>>();
    spool.seek(SeekFrom::Start(0))?;
    spool.write_all(&header.encode())?;
    spool.write_all(&directory_bytes)?;
    spool.flush()?;

    let (_, root) = digest_range(spool.as_file_mut(), 0, footer_offset, cancellation)?;
    let archive_length = footer_offset
        .checked_add(FOOTER_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let footer = Footer {
        archive_length,
        blake3_root: root,
        directory_crc32c: crc32c::crc32c(&directory_bytes),
        version: 1,
    };
    spool.seek(SeekFrom::Start(footer_offset))?;
    spool.write_all(&footer.encode())?;
    spool.flush()?;
    spool.as_file().sync_all()?;

    verify_open_file(spool.as_file_mut(), &decode_limits, cancellation)?;
    cancellation.checkpoint()?;
    atomic_commit(spool, &request.output)?;
    info!(?request.output, "RAW archive committed");
    Ok(())
}

#[derive(Debug, Clone)]
struct CompressedSource {
    entry_id: u64,
    path: PathBuf,
    size: u64,
    hash: [u8; 32],
    modified: Option<std::time::SystemTime>,
    identity: Option<Arc<same_file::Handle>>,
}

fn pack_compressed_with_limits(
    request: PackRequest,
    pack_limits: &PackLimits,
    cancellation: &CancellationToken,
) -> Result<()> {
    if request.inputs.is_empty() {
        return Err(PithosError::InvalidMetadata("nenhuma entrada"));
    }
    if path_entry_exists(&request.output)? {
        return Err(PithosError::OutputExists);
    }
    if pack_limits.max_temp_bytes == 0
        || pack_limits.max_output_bytes == 0
        || pack_limits.max_metadata_bytes == 0
        || pack_limits.max_entries == 0
    {
        return Err(PithosError::ResourceLimit("pack budget"));
    }

    let defaults = DecodeLimits::default();
    let decode_limits = DecodeLimits {
        max_entries: pack_limits.max_entries.min(defaults.max_entries),
        max_groups: pack_limits.max_entries.min(defaults.max_groups),
        max_chunks: pack_limits.max_entries.min(defaults.max_chunks),
        max_original_bytes: pack_limits.max_input_bytes,
        max_group_output: defaults.max_group_output,
        max_metadata_bytes: pack_limits
            .max_metadata_bytes
            .min(defaults.max_metadata_bytes),
        ..defaults
    };
    let scanned = scan_inputs(&request.inputs, &decode_limits, cancellation)?;
    let mut entries = Vec::with_capacity(scanned.len());
    let mut sources = Vec::<CompressedSource>::new();
    let mut hardlinks: HashMap<Arc<same_file::Handle>, (u64, u64, [u8; 32])> = HashMap::new();
    let mut original_total_size = 0_u64;

    for scanned_entry in scanned {
        cancellation.checkpoint()?;
        let entry_id = entries.len() as u64;
        let modified_ns = modified_ns(&scanned_entry.metadata);
        let mode = metadata_mode(&scanned_entry.metadata);
        let (size, hash, kind) = match scanned_entry.kind {
            ScannedKind::Directory => (
                0,
                *blake3::hash(b"directory").as_bytes(),
                EntryKind::Directory,
            ),
            ScannedKind::Symlink {
                target,
                target_is_dir,
            } => {
                let serialized = serialize(&target)?;
                (
                    serialized.len() as u64,
                    *blake3::hash(&serialized).as_bytes(),
                    EntryKind::Symlink {
                        target,
                        target_is_dir,
                    },
                )
            }
            ScannedKind::File => {
                let expected_size = scanned_entry.metadata.len();
                original_total_size = original_total_size
                    .checked_add(expected_size)
                    .ok_or(PithosError::IntegerOverflow)?;
                if original_total_size > decode_limits.max_original_bytes {
                    return Err(PithosError::ResourceLimit("tamanho original"));
                }
                let identity = scanned_entry.identity.clone();
                if let Some((target_entry_id, target_size, target_hash)) = identity
                    .as_ref()
                    .and_then(|key| hardlinks.get(key))
                    .copied()
                {
                    (
                        target_size,
                        target_hash,
                        EntryKind::Hardlink { target_entry_id },
                    )
                } else {
                    if expected_size > decode_limits.max_group_output {
                        return Err(PithosError::ResourceLimit("tamanho do grupo"));
                    }
                    let (actual_size, file_hash) = hash_input_file(
                        &scanned_entry.source,
                        &scanned_entry.metadata,
                        identity.clone(),
                        cancellation,
                    )?;
                    sources.push(CompressedSource {
                        entry_id,
                        path: scanned_entry.source.clone(),
                        size: actual_size,
                        hash: file_hash,
                        modified: scanned_entry.metadata.modified().ok(),
                        identity: identity.clone(),
                    });
                    if let Some(identity) = identity {
                        hardlinks.insert(identity, (entry_id, actual_size, file_hash));
                    }
                    (
                        actual_size,
                        file_hash,
                        EntryKind::File { group_id: u64::MAX },
                    )
                }
            }
        };
        entries.push(EntryRecord {
            entry_id,
            path: scanned_entry.archive_path,
            size,
            modified_ns,
            mode,
            blake3: hash,
            kind,
        });
    }

    let lengths = sources.iter().map(|source| source.size).collect::<Vec<_>>();
    let plans = plan_solid_groups(request.profile, &lengths)?;
    let mut restore_map = Vec::with_capacity(sources.len());
    for (group_id, plan) in plans.iter().enumerate() {
        let members = group_members(&sources, plan)?;
        let mut group_offset = 0_u64;
        for source in members {
            entries[source.entry_id as usize].kind = EntryKind::File {
                group_id: group_id as u64,
            };
            restore_map.push(RestoreMapRecord {
                entry_id: source.entry_id,
                original_offset: 0,
                length: source.size,
                group_id: group_id as u64,
                group_offset,
            });
            group_offset = group_offset
                .checked_add(source.size)
                .ok_or(PithosError::IntegerOverflow)?;
        }
        if group_offset != plan.uncompressed_len {
            return Err(PithosError::InvalidMetadata("solid group plan length"));
        }
    }

    let candidates = profile_codecs(request.profile);
    let parent = request.output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(8);
    let memory_budget = pack_limits
        .max_memory_bytes
        .clamp(1, 2 * 1024 * 1024 * 1024);
    let scheduler_config = scheduler::SchedulerConfig::new(workers, memory_budget, parent)?;
    let scheduler_cancellation = scheduler::CancellationToken::new();
    let mut tasks = Vec::with_capacity(plans.len());
    for (group_id, plan) in plans.iter().enumerate() {
        let members = group_members(&sources, plan)?.to_vec();
        let output_bound = plan
            .uncompressed_len
            .checked_add(1024 * 1024 + 2)
            .ok_or(PithosError::IntegerOverflow)?;
        let codec_memory = candidates.iter().try_fold(0_u64, |maximum, codec_id| {
            let config = CodecConfig::deterministic_default(*codec_id);
            Ok::<u64, PithosError>(
                maximum.max(codec_for_id(*codec_id).memory_bound(plan.uncompressed_len, &config)?),
            )
        })?;
        let outer_cancellation = cancellation.clone();
        let task_candidates = candidates.clone();
        tasks.push(scheduler::ScheduledTask::new(
            group_id as u64,
            scheduler::JobPriority::PackForeground,
            Vec::new(),
            scheduler::ResourceEstimate {
                input_bytes: 0,
                scratch_bytes: codec_memory,
                output_bound,
            },
            move |task_cancellation| {
                encode_solid_group(
                    &members,
                    &task_candidates,
                    &outer_cancellation,
                    task_cancellation,
                )
            },
        ));
    }
    let encoded_groups =
        scheduler::execute_scheduled(tasks, scheduler_config, scheduler_cancellation)?;
    cancellation.checkpoint()?;

    let directory_length = u64::from(REQUIRED_COMPRESSED_SECTIONS)
        .checked_mul(SECTION_ENTRY_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let payload_start = (HEADER_LEN as u64)
        .checked_add(directory_length)
        .ok_or(PithosError::IntegerOverflow)?;
    let mut spool = create_atomic_spool(parent)?;
    spool.seek(SeekFrom::Start(payload_start))?;
    let mut groups = Vec::with_capacity(encoded_groups.len());
    let mut used_codecs = std::collections::BTreeSet::new();
    let mut payload_crc = 0_u32;
    for encoded in &encoded_groups {
        cancellation.checkpoint()?;
        let envelope = encoded.read_all()?;
        if envelope.len() < 2 {
            return Err(PithosError::InvalidMetadata("encoded group envelope"));
        }
        let codec_id_raw = u16::from_le_bytes([envelope[0], envelope[1]]);
        let codec_id = CodecId::from_u16(codec_id_raw).ok_or(PithosError::UnsupportedCodec)?;
        let payload = &envelope[2..];
        let payload_offset = spool.stream_position()?;
        spool.write_all(payload)?;
        let group_crc = crc32c::crc32c(payload);
        payload_crc = crc32c::crc32c_append(payload_crc, payload);
        let plan = plans
            .get(encoded.task_id as usize)
            .ok_or(PithosError::InvalidMetadata("encoded group ID"))?;
        let compressed_len =
            u64::try_from(payload.len()).map_err(|_| PithosError::IntegerOverflow)?;
        groups.push(GroupTableRecord {
            group: GroupRecord {
                version: 1,
                flags: 0,
                group_id: encoded.task_id,
                codec_chain_id: u32::from(codec_id as u16) + 1,
                chunk_count: u32::try_from(plan.item_count)
                    .map_err(|_| PithosError::IntegerOverflow)?,
                uncompressed_len: plan.uncompressed_len,
                compressed_len,
                descriptor_len: 0,
                payload_crc32c: group_crc,
            },
            payload_offset,
        });
        used_codecs.insert(codec_id_raw);
    }
    let payload_end = spool.stream_position()?;
    let payload_length = payload_end
        .checked_sub(payload_start)
        .ok_or(PithosError::IntegerOverflow)?;

    let registry = CodecRegistry {
        records: used_codecs
            .into_iter()
            .map(|codec_id| {
                let id = CodecId::from_u16(codec_id).ok_or(PithosError::UnsupportedCodec)?;
                Ok(CodecRegistryRecord {
                    chain_id: u32::from(codec_id) + 1,
                    codec_id,
                    codec_version: codec_for_id(id).version(),
                    level: CodecConfig::deterministic_default(id).level,
                    flags: CODEC_FLAG_REQUIRED,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let registry_bytes = registry.encode()?;
    let central_index = entries
        .iter()
        .map(|entry| CentralIndexRecord {
            path: entry.path.clone(),
            entry_id: entry.entry_id,
            group_id: match entry.kind {
                EntryKind::File { group_id } => Some(group_id),
                _ => None,
            },
        })
        .collect::<Vec<_>>();
    let integrity = entries
        .iter()
        .map(|entry| IntegrityRecord {
            entry_id: entry.entry_id,
            blake3: entry.blake3,
        })
        .collect::<Vec<_>>();
    let entry_bytes = serialize(&entries)?;
    let group_bytes = serialize(&groups)?;
    let restore_bytes = serialize(&restore_map)?;
    let index_bytes = serialize(&central_index)?;
    let integrity_bytes = serialize(&integrity)?;
    let metadata_sections = [
        &registry_bytes,
        &entry_bytes,
        &group_bytes,
        &restore_bytes,
        &index_bytes,
        &integrity_bytes,
    ];
    let metadata_total = metadata_sections.iter().try_fold(0_u64, |total, bytes| {
        let length = u64::try_from(bytes.len()).map_err(|_| PithosError::IntegerOverflow)?;
        if length > decode_limits.max_metadata_bytes {
            return Err(PithosError::ResourceLimit("seção de metadados"));
        }
        total
            .checked_add(length)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    if metadata_total > decode_limits.max_metadata_bytes {
        return Err(PithosError::ResourceLimit("aggregate metadata"));
    }
    let expected_archive_length = payload_end
        .checked_add(metadata_total)
        .and_then(|length| length.checked_add(FOOTER_LEN as u64))
        .ok_or(PithosError::IntegerOverflow)?;
    if expected_archive_length > pack_limits.max_output_bytes {
        return Err(PithosError::ResourceLimit("archive output"));
    }
    let encoded_temp = encoded_groups.iter().try_fold(0_u64, |total, group| {
        total
            .checked_add(group.len)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    if encoded_temp
        .checked_add(expected_archive_length)
        .is_none_or(|peak| peak > pack_limits.max_temp_bytes)
    {
        return Err(PithosError::TemporarySpaceLimit);
    }

    let mut sections = Vec::with_capacity(REQUIRED_COMPRESSED_SECTIONS as usize);
    sections.push(write_section(
        &mut spool,
        SectionType::CodecRegistry,
        &registry_bytes,
    )?);
    sections.push(write_section(
        &mut spool,
        SectionType::EntryTable,
        &entry_bytes,
    )?);
    sections.push(write_section(
        &mut spool,
        SectionType::GroupTable,
        &group_bytes,
    )?);
    sections.push(SectionDirectoryRecord {
        section_type: SectionType::PayloadArea as u16,
        section_version: 1,
        flags: 0,
        offset: payload_start,
        length: payload_length,
        crc32c: payload_crc,
        reserved: 0,
    });
    sections.push(write_section(
        &mut spool,
        SectionType::RestoreMap,
        &restore_bytes,
    )?);
    sections.push(write_section(
        &mut spool,
        SectionType::CentralIndex,
        &index_bytes,
    )?);
    sections.push(write_section(
        &mut spool,
        SectionType::IntegrityTree,
        &integrity_bytes,
    )?);
    sections.sort_by_key(|section| section.section_type);

    let footer_offset = spool.stream_position()?;
    let mut identity_hasher = blake3::Hasher::new();
    for bytes in metadata_sections {
        identity_hasher.update(bytes);
    }
    let identity_hash = identity_hasher.finalize();
    let mut archive_id = [0_u8; 16];
    archive_id.copy_from_slice(&identity_hash.as_bytes()[..16]);
    let mut header = GlobalHeader::new(archive_id);
    header.original_total_size = original_total_size;
    header.entry_count = entries.len() as u64;
    header.logical_chunk_count = restore_map.len() as u64;
    header.group_count = groups.len() as u64;
    header.footer_offset = footer_offset;
    header.section_count = REQUIRED_COMPRESSED_SECTIONS;
    let directory_bytes = sections
        .iter()
        .flat_map(SectionDirectoryRecord::encode)
        .collect::<Vec<_>>();
    spool.seek(SeekFrom::Start(0))?;
    spool.write_all(&header.encode())?;
    spool.write_all(&directory_bytes)?;
    spool.flush()?;
    let (_, root) = digest_range(spool.as_file_mut(), 0, footer_offset, cancellation)?;
    let archive_length = footer_offset
        .checked_add(FOOTER_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let footer = Footer {
        archive_length,
        blake3_root: root,
        directory_crc32c: crc32c::crc32c(&directory_bytes),
        version: 1,
    };
    spool.seek(SeekFrom::Start(footer_offset))?;
    spool.write_all(&footer.encode())?;
    spool.flush()?;
    spool.as_file().sync_all()?;
    verify_open_file(spool.as_file_mut(), &decode_limits, cancellation)?;
    cancellation.checkpoint()?;
    atomic_commit(spool, &request.output)?;
    info!(?request.output, "compressed archive committed");
    Ok(())
}

fn group_members<'a>(
    sources: &'a [CompressedSource],
    plan: &SolidGroupPlan,
) -> Result<&'a [CompressedSource]> {
    let end = plan
        .first_item
        .checked_add(plan.item_count)
        .ok_or(PithosError::IntegerOverflow)?;
    sources
        .get(plan.first_item..end)
        .ok_or(PithosError::InvalidMetadata("solid group member range"))
}

fn profile_codecs(profile: CompressionProfile) -> Vec<CodecId> {
    match profile {
        CompressionProfile::Raw => vec![CodecId::Store],
        CompressionProfile::Stream | CompressionProfile::Random => {
            vec![CodecId::Store, CodecId::Zstd]
        }
        CompressionProfile::Balanced | CompressionProfile::ArchiveMax => vec![
            CodecId::Store,
            CodecId::Zstd,
            CodecId::Brotli,
            CodecId::Lzma2,
        ],
    }
}

fn codec_for_id(codec_id: CodecId) -> &'static dyn Codec {
    static STORE: StoreCodec = StoreCodec;
    static ZSTD: ZstdCodec = ZstdCodec;
    static BROTLI: BrotliCodec = BrotliCodec;
    static LZMA2: Lzma2Codec = Lzma2Codec;
    match codec_id {
        CodecId::Store => &STORE,
        CodecId::Zstd => &ZSTD,
        CodecId::Brotli => &BROTLI,
        CodecId::Lzma2 => &LZMA2,
    }
}

fn encode_solid_group(
    members: &[CompressedSource],
    candidates: &[CodecId],
    cancellation: &CancellationToken,
    task_cancellation: &scheduler::CancellationToken,
) -> Result<Vec<u8>> {
    let input_length = members.iter().try_fold(0_u64, |total, source| {
        total
            .checked_add(source.size)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    let capacity = usize::try_from(input_length).map_err(|_| PithosError::MemoryLimit)?;
    let mut input = Vec::with_capacity(capacity);
    for source in members {
        cancellation.checkpoint()?;
        task_cancellation.checkpoint()?;
        read_verified_source(source, &mut input, cancellation, task_cancellation)?;
    }

    let mut best: Option<(CodecId, CandidateCost, Vec<u8>)> = None;
    for codec_id in candidates {
        cancellation.checkpoint()?;
        task_cancellation.checkpoint()?;
        let codec = codec_for_id(*codec_id);
        let config = CodecConfig::deterministic_default(*codec_id);
        let mut encoded = Vec::new();
        let stats = codec.encode(&input, &config, &mut encoded)?;
        let cost = CandidateCost {
            payload: stats.output_bytes,
            codec_descriptor: 16,
            group_descriptor: 48,
            index_delta: 0,
            integrity: 4,
            padding: 0,
        };
        let replace = match &best {
            None => true,
            Some((best_id, best_cost, _)) => {
                let total = cost.total()?;
                let best_total = best_cost.total()?;
                total < best_total
                    || (total == best_total && (*codec_id as u16) < (*best_id as u16))
            }
        };
        if replace {
            best = Some((*codec_id, cost, encoded));
        }
    }
    let (codec_id, _, payload) = best.ok_or(PithosError::UnsupportedCodec)?;
    let mut envelope = Vec::with_capacity(payload.len() + 2);
    envelope.extend_from_slice(&(codec_id as u16).to_le_bytes());
    envelope.extend_from_slice(&payload);
    Ok(envelope)
}

fn hash_input_file(
    path: &Path,
    initial_metadata: &fs::Metadata,
    initial_identity: Option<Arc<same_file::Handle>>,
    cancellation: &CancellationToken,
) -> Result<(u64, [u8; 32])> {
    let current_metadata = fs::metadata(path)?;
    if !same_file_metadata(initial_metadata, &current_metadata)
        || file_identity(path).as_ref() != initial_identity.as_ref()
    {
        return Err(PithosError::InputChanged);
    }
    let source = CompressedSource {
        entry_id: 0,
        path: path.to_path_buf(),
        size: initial_metadata.len(),
        hash: [0; 32],
        modified: initial_metadata.modified().ok(),
        identity: initial_identity,
    };
    let task_cancellation = scheduler::CancellationToken::new();
    let mut bytes = Vec::new();
    read_source_bytes(&source, &mut bytes, cancellation, &task_cancellation, false)?;
    Ok((
        u64::try_from(bytes.len()).map_err(|_| PithosError::IntegerOverflow)?,
        *blake3::hash(&bytes).as_bytes(),
    ))
}

fn read_verified_source(
    source: &CompressedSource,
    output: &mut Vec<u8>,
    cancellation: &CancellationToken,
    task_cancellation: &scheduler::CancellationToken,
) -> Result<()> {
    let start = output.len();
    read_source_bytes(source, output, cancellation, task_cancellation, true)?;
    if blake3::hash(&output[start..]).as_bytes() != &source.hash {
        return Err(PithosError::InputChanged);
    }
    Ok(())
}

fn read_source_bytes(
    source: &CompressedSource,
    output: &mut Vec<u8>,
    cancellation: &CancellationToken,
    task_cancellation: &scheduler::CancellationToken,
    verify_hash: bool,
) -> Result<()> {
    let initial = fs::metadata(&source.path)?;
    if initial.len() != source.size
        || initial.modified().ok() != source.modified
        || file_identity(&source.path).as_ref() != source.identity.as_ref()
    {
        return Err(PithosError::InputChanged);
    }
    let start = output.len();
    let mut file = File::open(&source.path)?;
    let mut buffer = [0_u8; IO_BUFFER_SIZE];
    loop {
        cancellation.checkpoint()?;
        task_cancellation.checkpoint()?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
        let written = output
            .len()
            .checked_sub(start)
            .ok_or(PithosError::IntegerOverflow)?;
        if u64::try_from(written).map_err(|_| PithosError::IntegerOverflow)? > source.size {
            return Err(PithosError::InputChanged);
        }
    }
    let final_metadata = fs::metadata(&source.path)?;
    let written = output
        .len()
        .checked_sub(start)
        .ok_or(PithosError::IntegerOverflow)?;
    if u64::try_from(written).map_err(|_| PithosError::IntegerOverflow)? != source.size
        || final_metadata.len() != source.size
        || final_metadata.modified().ok() != source.modified
        || file_identity(&source.path).as_ref() != source.identity.as_ref()
        || (verify_hash && blake3::hash(&output[start..]).as_bytes() != &source.hash)
    {
        return Err(PithosError::InputChanged);
    }
    Ok(())
}

pub fn verify(archive: &Path) -> Result<VerificationReport> {
    verify_with_limits(archive, &DecodeLimits::default())
}

pub fn verify_with_limits(archive: &Path, limits: &DecodeLimits) -> Result<VerificationReport> {
    verify_with_control(archive, limits, &CancellationToken::new())
}

pub fn verify_with_control(
    archive: &Path,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<VerificationReport> {
    cancellation.checkpoint()?;
    let mut file = File::open(archive)?;
    let parsed = verify_open_file(&mut file, limits, cancellation)?;
    Ok(parsed.report)
}

/// Lists validated metadata without reading the archive payload area.
pub fn list(archive: &Path) -> Result<Vec<ArchiveEntrySummary>> {
    list_with_control(archive, &DecodeLimits::default(), &CancellationToken::new())
}

pub fn list_with_control(
    archive: &Path,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<Vec<ArchiveEntrySummary>> {
    cancellation.checkpoint()?;
    let mut file = File::open(archive)?;
    let parsed = read_catalog(&mut file, limits, cancellation)?;
    parsed.entries.iter().map(entry_summary).collect()
}

/// Inspects validated metadata without reading the archive payload area.
pub fn inspect(archive: &Path) -> Result<ArchiveInspection> {
    inspect_with_control(archive, &DecodeLimits::default(), &CancellationToken::new())
}

pub fn inspect_with_control(
    archive: &Path,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<ArchiveInspection> {
    cancellation.checkpoint()?;
    let mut file = File::open(archive)?;
    let parsed = read_catalog(&mut file, limits, cancellation)?;
    Ok(ArchiveInspection {
        archive_bytes: parsed.report.archive_bytes,
        original_bytes: parsed.report.original_bytes,
        entry_count: parsed.report.entry_count,
        file_count: parsed.report.file_count,
        directory_count: parsed.report.directory_count,
        hardlink_count: parsed.report.hardlink_count,
        symlink_count: parsed.report.symlink_count,
        group_count: parsed.report.group_count,
        format_version: "PAF 0.1-draft".to_owned(),
        metadata_verified: true,
    })
}

/// Extracts exactly one selected entry. Only the group owning that entry is read.
pub fn extract(request: ExtractRequest) -> Result<ExtractReport> {
    extract_with_control(request, &DecodeLimits::default(), &CancellationToken::new())
}

pub fn extract_with_control(
    request: ExtractRequest,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<ExtractReport> {
    extract_with_control_and_limits(
        request,
        limits,
        limits.max_group_output,
        limits.max_group_output,
        cancellation,
    )
}

/// Extracts one entry while enforcing the operation's actual output and
/// temporary-file budgets independently from the archive parser limits.
pub fn extract_with_control_and_limits(
    request: ExtractRequest,
    limits: &DecodeLimits,
    max_output_bytes: u64,
    max_temp_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<ExtractReport> {
    cancellation.checkpoint()?;
    let selector = ArchivePath::from_relative(&request.entry)?;
    let mut archive = File::open(&request.archive)?;
    let parsed = read_catalog(&mut archive, limits, cancellation)?;
    let entry = parsed
        .entries
        .iter()
        .find(|entry| entry.path == selector)
        .ok_or(PithosError::InvalidMetadata("entry not found"))?;
    let destination = request.output_dir.join(entry.path.to_path_buf()?);

    match &entry.kind {
        EntryKind::Directory => {
            cancellation.checkpoint()?;
            prepare_extract_parent(&request.output_dir, &entry.path)?;
            if path_entry_exists(&destination)? {
                return Err(PithosError::OutputExists);
            }
            fs::create_dir_all(&destination)?;
            Ok(extract_report(entry, 0))
        }
        EntryKind::Symlink {
            target,
            target_is_dir,
        } => {
            cancellation.checkpoint()?;
            prepare_extract_parent(&request.output_dir, &entry.path)?;
            if path_entry_exists(&destination)? {
                return Err(PithosError::OutputExists);
            }
            if !target.resolves_within(&entry.path) {
                return Err(PithosError::UnsafeSymlink);
            }
            create_symlink(&target.to_path_buf()?, &destination, *target_is_dir)?;
            Ok(extract_report(entry, entry.size))
        }
        EntryKind::File { .. } | EntryKind::Hardlink { .. } => {
            let source = file_source_entry(entry, &parsed.entries)?;
            let EntryKind::File { group_id } = source.kind else {
                return Err(PithosError::InvalidMetadata("file source"));
            };
            let group = parsed
                .groups
                .get(group_id as usize)
                .ok_or(PithosError::InvalidMetadata("entry group"))?;
            let restore = restore_for_entry(&parsed, source.entry_id)?;
            if source.size > max_output_bytes {
                return Err(PithosError::ResourceLimit("extract output bytes"));
            }
            if group.group.compressed_len > max_temp_bytes {
                return Err(PithosError::ResourceLimit("extract temporary bytes"));
            }
            prepare_extract_parent(&request.output_dir, &entry.path)?;
            if path_entry_exists(&destination)? {
                return Err(PithosError::OutputExists);
            }
            let parent = destination.parent().unwrap_or_else(|| Path::new("."));
            let mut spool = create_atomic_spool(parent)?;
            let decoded = decode_group(
                &mut archive,
                group,
                parsed.codec_registry.as_ref(),
                limits,
                cancellation,
            )?;
            let bytes = restored_slice(&decoded, restore, 0, restore.length)?;
            spool.write_all(bytes)?;
            if blake3::hash(bytes).as_bytes() != &source.blake3 {
                return Err(PithosError::HashMismatch);
            }
            cancellation.checkpoint()?;
            atomic_commit(spool, &destination)?;
            apply_file_mode(&destination, entry.mode)?;
            Ok(extract_report(entry, source.size))
        }
    }
}

/// Streams one file entry to a caller-provided writer without materializing it on disk.
pub fn extract_to_writer<W: Write>(
    archive: &Path,
    entry: &Path,
    output: &mut W,
) -> Result<ExtractReport> {
    extract_to_writer_with_control(
        archive,
        entry,
        output,
        &DecodeLimits::default(),
        &CancellationToken::new(),
    )
}

pub fn extract_to_writer_with_control<W: Write>(
    archive: &Path,
    entry: &Path,
    output: &mut W,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<ExtractReport> {
    cancellation.checkpoint()?;
    let selector = ArchivePath::from_relative(entry)?;
    let mut source_archive = File::open(archive)?;
    let parsed = read_catalog(&mut source_archive, limits, cancellation)?;
    let selected = parsed
        .entries
        .iter()
        .find(|candidate| candidate.path == selector)
        .ok_or(PithosError::InvalidMetadata("entry not found"))?;
    let source = file_source_entry(selected, &parsed.entries)?;
    let EntryKind::File { group_id } = source.kind else {
        return Err(PithosError::InvalidMetadata("stdout requires a file entry"));
    };
    let group = parsed
        .groups
        .get(group_id as usize)
        .ok_or(PithosError::InvalidMetadata("entry group"))?;
    let restore = restore_for_entry(&parsed, source.entry_id)?;
    let decoded = decode_group(
        &mut source_archive,
        group,
        parsed.codec_registry.as_ref(),
        limits,
        cancellation,
    )?;
    let bytes = restored_slice(&decoded, restore, 0, restore.length)?;
    output.write_all(bytes)?;
    if blake3::hash(bytes).as_bytes() != &source.blake3 {
        return Err(PithosError::HashMismatch);
    }
    Ok(extract_report(selected, source.size))
}

pub fn read_range_to_writer_with_control<W: Write>(
    request: ReadRangeRequest,
    output: &mut W,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<ReadRangeReport> {
    cancellation.checkpoint()?;
    if request.length > limits.max_group_output {
        return Err(PithosError::ResourceLimit("range output"));
    }
    let selector = ArchivePath::from_relative(&request.entry)?;
    let mut archive = File::open(&request.archive)?;
    let parsed = read_catalog(&mut archive, limits, cancellation)?;
    let selected = parsed
        .entries
        .iter()
        .find(|candidate| candidate.path == selector)
        .ok_or(PithosError::InvalidMetadata("entry not found"))?;
    let source = file_source_entry(selected, &parsed.entries)?;
    let end = request
        .offset
        .checked_add(request.length)
        .ok_or(PithosError::IntegerOverflow)?;
    if end > source.size {
        return Err(PithosError::InvalidRange);
    }
    let EntryKind::File { group_id } = source.kind else {
        return Err(PithosError::InvalidMetadata("range requires a file entry"));
    };
    let group = parsed
        .groups
        .get(group_id as usize)
        .ok_or(PithosError::InvalidMetadata("entry group"))?;
    let restore = restore_for_entry(&parsed, source.entry_id)?;
    let decoded = decode_group(
        &mut archive,
        group,
        parsed.codec_registry.as_ref(),
        limits,
        cancellation,
    )?;
    let entry_bytes = restored_slice(&decoded, restore, 0, restore.length)?;
    if blake3::hash(entry_bytes).as_bytes() != &source.blake3 {
        return Err(PithosError::HashMismatch);
    }
    let range_bytes = restored_slice(&decoded, restore, request.offset, request.length)?;
    output.write_all(range_bytes)?;
    let range_hash = *blake3::hash(range_bytes).as_bytes();
    Ok(ReadRangeReport {
        path: selected.path.to_path_buf()?.to_string_lossy().into_owned(),
        offset: request.offset,
        length: request.length,
        entry_size: source.size,
        blake3: range_hash,
    })
}

pub fn unpack(request: UnpackRequest) -> Result<()> {
    unpack_with_control(request, &DecodeLimits::default(), &CancellationToken::new())
}

pub fn unpack_with_control(
    request: UnpackRequest,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<()> {
    unpack_with_control_and_temp_limit(request, limits, limits.max_original_bytes, cancellation)
}

/// Transactionally unpacks an archive while bounding the staging tree.
pub fn unpack_with_control_and_temp_limit(
    request: UnpackRequest,
    limits: &DecodeLimits,
    max_temp_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<()> {
    cancellation.checkpoint()?;
    if path_entry_exists(&request.output_dir)? {
        return Err(PithosError::OutputExists);
    }
    let mut archive = File::open(&request.archive)?;
    let parsed = verify_open_file(&mut archive, limits, cancellation)?;
    if parsed.report.original_bytes > max_temp_bytes {
        return Err(PithosError::ResourceLimit("unpack temporary bytes"));
    }
    let output_parent = request
        .output_dir
        .parent()
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".pithos-unpack-")
        .tempdir_in(output_parent)?;

    for entry in parsed
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Directory))
    {
        cancellation.checkpoint()?;
        fs::create_dir_all(staging.path().join(entry.path.to_path_buf()?))?;
    }

    let groups = parsed
        .groups
        .iter()
        .map(|record| (record.group.group_id, record))
        .collect::<HashMap<_, _>>();
    for entry in parsed
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::File { .. }))
    {
        cancellation.checkpoint()?;
        let EntryKind::File { group_id } = entry.kind else {
            continue;
        };
        let group = groups
            .get(&group_id)
            .ok_or(PithosError::InvalidMetadata("grupo ausente"))?;
        let restore = restore_for_entry(&parsed, entry.entry_id)?;
        let destination = staging.path().join(entry.path.to_path_buf()?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        let decoded = decode_group(
            &mut archive,
            group,
            parsed.codec_registry.as_ref(),
            limits,
            cancellation,
        )?;
        let bytes = restored_slice(&decoded, restore, 0, restore.length)?;
        output.write_all(bytes)?;
        if blake3::hash(bytes).as_bytes() != &entry.blake3 {
            return Err(PithosError::HashMismatch);
        }
        output.sync_all()?;
        apply_file_mode(&destination, entry.mode)?;
    }

    for entry in parsed
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Hardlink { .. }))
    {
        cancellation.checkpoint()?;
        let EntryKind::Hardlink { target_entry_id } = entry.kind else {
            continue;
        };
        let target = parsed
            .entries
            .get(target_entry_id as usize)
            .ok_or(PithosError::InvalidMetadata("hardlink inválido"))?;
        let destination = staging.path().join(entry.path.to_path_buf()?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::hard_link(staging.path().join(target.path.to_path_buf()?), destination)?;
    }

    for entry in parsed
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Symlink { .. }))
    {
        cancellation.checkpoint()?;
        let EntryKind::Symlink {
            ref target,
            target_is_dir,
        } = entry.kind
        else {
            continue;
        };
        if !target.resolves_within(&entry.path) {
            return Err(PithosError::UnsafeSymlink);
        }
        let destination = staging.path().join(entry.path.to_path_buf()?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        create_symlink(&target.to_path_buf()?, &destination, target_is_dir)?;
    }

    cancellation.checkpoint()?;
    fs::rename(staging.path(), &request.output_dir)?;
    sync_parent(&request.output_dir)?;
    info!(?request.output_dir, "RAW archive unpacked transactionally");
    Ok(())
}

#[derive(Debug)]
struct ParsedArchive {
    entries: Vec<EntryRecord>,
    groups: Vec<GroupTableRecord>,
    restore_map: Vec<RestoreMapRecord>,
    codec_registry: Option<CodecRegistry>,
    report: VerificationReport,
    payload_section: SectionDirectoryRecord,
    footer_offset: u64,
    expected_root: [u8; 32],
}

fn read_catalog(
    file: &mut File,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<ParsedArchive> {
    cancellation.checkpoint()?;
    let file_length = file.metadata()?.len();
    if file_length < (HEADER_LEN + FOOTER_LEN) as u64 {
        return Err(PithosError::InvalidRange);
    }
    file.seek(SeekFrom::Start(0))?;
    let mut header_bytes = [0_u8; HEADER_LEN];
    file.read_exact(&mut header_bytes)?;
    let header = GlobalHeader::decode(&header_bytes)?;
    validate_header_limits(&header, file_length, limits)?;

    file.seek(SeekFrom::Start(header.footer_offset))?;
    let mut footer_bytes = [0_u8; FOOTER_LEN];
    file.read_exact(&mut footer_bytes)?;
    let footer = Footer::decode(&footer_bytes)?;
    if footer.archive_length != file_length
        || footer.version != 1
        || header.footer_offset.checked_add(FOOTER_LEN as u64) != Some(file_length)
    {
        return Err(PithosError::InvalidMetadata("footer"));
    }

    let directory_length = u64::from(header.section_count)
        .checked_mul(SECTION_ENTRY_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let directory_end = header
        .section_directory_offset
        .checked_add(directory_length)
        .ok_or(PithosError::IntegerOverflow)?;
    if header.section_directory_offset != HEADER_LEN as u64 || directory_end > header.footer_offset
    {
        return Err(PithosError::InvalidRange);
    }
    let directory_bytes = read_exact_vec(
        file,
        header.section_directory_offset,
        directory_length,
        limits.max_metadata_bytes,
    )?;
    if crc32c::crc32c(&directory_bytes) != footer.directory_crc32c {
        return Err(PithosError::ChecksumMismatch);
    }

    let mut section_map = BTreeMap::new();
    for chunk in directory_bytes.chunks_exact(SECTION_ENTRY_LEN) {
        let record = SectionDirectoryRecord::decode(
            chunk
                .try_into()
                .map_err(|_| PithosError::InvalidMetadata("section directory"))?,
        );
        if record.section_version != 1 || record.flags != 0 || record.reserved != 0 {
            return Err(PithosError::InvalidMetadata("section flags/version"));
        }
        let section_type = SectionType::from_u16(record.section_type)
            .ok_or(PithosError::UnsupportedContainerVersion)?;
        if !RAW_SECTION_TYPES.contains(&section_type) && section_type != SectionType::CodecRegistry
        {
            return Err(PithosError::UnsupportedContainerVersion);
        }
        if section_map.insert(record.section_type, record).is_some() {
            return Err(PithosError::DuplicateSection);
        }
    }
    for section_type in RAW_SECTION_TYPES {
        if !section_map.contains_key(&(section_type as u16)) {
            return Err(PithosError::MissingSection(section_name(section_type)));
        }
    }
    validate_section_ranges(section_map.values(), directory_end, header.footer_offset)?;

    let entry_section = required_section(&section_map, SectionType::EntryTable)?;
    let group_section = required_section(&section_map, SectionType::GroupTable)?;
    let restore_section = required_section(&section_map, SectionType::RestoreMap)?;
    let index_section = required_section(&section_map, SectionType::CentralIndex)?;
    let integrity_section = required_section(&section_map, SectionType::IntegrityTree)?;
    let payload_section = required_section(&section_map, SectionType::PayloadArea)?;
    let codec_section = section_map.get(&(SectionType::CodecRegistry as u16));

    if header.section_count == REQUIRED_RAW_SECTIONS && codec_section.is_some()
        || header.section_count == REQUIRED_COMPRESSED_SECTIONS && codec_section.is_none()
    {
        return Err(PithosError::InvalidMetadata("codec registry section count"));
    }

    let mut metadata_sections = vec![
        entry_section,
        group_section,
        restore_section,
        index_section,
        integrity_section,
    ];
    if let Some(section) = codec_section {
        metadata_sections.push(section);
    }
    let aggregate_metadata =
        metadata_sections
            .iter()
            .try_fold(directory_length, |total, section| {
                total
                    .checked_add(section.length)
                    .ok_or(PithosError::IntegerOverflow)
            })?;
    if aggregate_metadata > limits.max_metadata_bytes {
        return Err(PithosError::ResourceLimit("aggregate metadata"));
    }

    let entries: Vec<EntryRecord> = read_json_section(file, entry_section, limits)?;
    let groups: Vec<GroupTableRecord> = read_json_section(file, group_section, limits)?;
    let restore_map: Vec<RestoreMapRecord> = read_json_section(file, restore_section, limits)?;
    let central_index: Vec<CentralIndexRecord> = read_json_section(file, index_section, limits)?;
    let integrity: Vec<IntegrityRecord> = read_json_section(file, integrity_section, limits)?;
    let codec_registry = if let Some(section) = codec_section {
        let bytes = read_checked_section(file, section, limits)?;
        Some(CodecRegistry::decode(&bytes, 32)?)
    } else {
        None
    };
    if entries.len() as u64 != header.entry_count
        || groups.len() as u64 != header.group_count
        || restore_map.len() as u64 != header.logical_chunk_count
        || central_index.len() != entries.len()
        || integrity.len() != entries.len()
    {
        return Err(PithosError::InvalidMetadata("table counts"));
    }

    let mut path_keys = HashSet::new();
    let mut original_size = 0_u64;
    let mut file_count = 0_u64;
    let mut directory_count = 0_u64;
    let mut hardlink_count = 0_u64;
    let mut symlink_count = 0_u64;
    for (position, entry) in entries.iter().enumerate() {
        if entry.entry_id != position as u64 {
            return Err(PithosError::InvalidMetadata("entry id"));
        }
        validate_archive_path(&entry.path, limits)?;
        if !path_keys.insert(entry.path.sort_key()) {
            return Err(PithosError::InvalidMetadata("duplicate path"));
        }
        match &entry.kind {
            EntryKind::File { group_id } => {
                file_count += 1;
                original_size = checked_total(original_size, entry.size, limits)?;
                let group = groups
                    .get(*group_id as usize)
                    .ok_or(PithosError::InvalidMetadata("entry group"))?;
                if group.group.group_id != *group_id {
                    return Err(PithosError::InvalidMetadata("entry group ID"));
                }
            }
            EntryKind::Directory => {
                directory_count += 1;
                if entry.size != 0 {
                    return Err(PithosError::InvalidMetadata("directory size"));
                }
            }
            EntryKind::Hardlink { target_entry_id } => {
                hardlink_count += 1;
                original_size = checked_total(original_size, entry.size, limits)?;
                if *target_entry_id >= entry.entry_id {
                    return Err(PithosError::InvalidMetadata("hardlink ordering"));
                }
                let target = entries
                    .get(*target_entry_id as usize)
                    .ok_or(PithosError::InvalidMetadata("hardlink target"))?;
                if !matches!(target.kind, EntryKind::File { .. })
                    || target.size != entry.size
                    || target.blake3 != entry.blake3
                {
                    return Err(PithosError::InvalidMetadata("hardlink target"));
                }
            }
            EntryKind::Symlink { target, .. } => {
                symlink_count += 1;
                if !target.resolves_within(&entry.path) {
                    return Err(PithosError::UnsafeSymlink);
                }
                target.to_path_buf()?;
            }
        }
        if central_index[position]
            != (CentralIndexRecord {
                path: entry.path.clone(),
                entry_id: entry.entry_id,
                group_id: match entry.kind {
                    EntryKind::File { group_id } => Some(group_id),
                    _ => None,
                },
            })
            || integrity[position]
                != (IntegrityRecord {
                    entry_id: entry.entry_id,
                    blake3: entry.blake3,
                })
        {
            return Err(PithosError::InvalidMetadata("index/integrity"));
        }
    }
    if original_size != header.original_total_size {
        return Err(PithosError::InvalidMetadata("original size"));
    }

    let mut restore_by_entry = BTreeMap::new();
    for restore in &restore_map {
        let entry = entries
            .get(restore.entry_id as usize)
            .ok_or(PithosError::InvalidMetadata("restore entry"))?;
        let EntryKind::File { group_id } = entry.kind else {
            return Err(PithosError::InvalidMetadata("restore entry kind"));
        };
        if restore.entry_id != entry.entry_id
            || restore.original_offset != 0
            || restore.length != entry.size
            || restore.group_id != group_id
            || restore_by_entry.insert(restore.entry_id, restore).is_some()
        {
            return Err(PithosError::InvalidMetadata("restore map entry"));
        }
    }
    if restore_by_entry.len() as u64 != file_count {
        return Err(PithosError::InvalidMetadata("restore map file count"));
    }

    let payload_end = payload_section
        .offset
        .checked_add(payload_section.length)
        .ok_or(PithosError::IntegerOverflow)?;
    let mut previous_group_end = payload_section.offset;
    for (position, record) in groups.iter().enumerate() {
        cancellation.checkpoint()?;
        let group = &record.group;
        if group.group_id != position as u64
            || group.version != 1
            || group.flags != 0
            || group.descriptor_len != 0
        {
            return Err(PithosError::InvalidMetadata("group header"));
        }
        if group.uncompressed_len > limits.max_group_output
            || group.compressed_len
                > limits
                    .max_group_output
                    .checked_add(1024 * 1024)
                    .ok_or(PithosError::IntegerOverflow)?
        {
            return Err(PithosError::ResourceLimit("group output"));
        }
        if group.compressed_len != 0
            && group
                .compressed_len
                .checked_mul(limits.max_expansion_ratio)
                .is_some_and(|maximum| group.uncompressed_len > maximum)
        {
            return Err(PithosError::ResourceLimit("group expansion ratio"));
        }
        match (group.codec_chain_id, &codec_registry) {
            (0, None) => {
                if group.chunk_count != 1 || group.uncompressed_len != group.compressed_len {
                    return Err(PithosError::InvalidMetadata("RAW group"));
                }
            }
            (0, Some(_)) | (_, None) => return Err(PithosError::UnsupportedCodec),
            (chain_id, Some(registry)) => {
                let record = registry
                    .chain(chain_id)
                    .ok_or(PithosError::UnsupportedCodec)?;
                let codec_id =
                    CodecId::from_u16(record.codec_id).ok_or(PithosError::UnsupportedCodec)?;
                codec_for_id(codec_id).memory_bound(
                    group.uncompressed_len,
                    &CodecConfig {
                        level: record.level,
                    },
                )?;
            }
        }
        let group_end = record
            .payload_offset
            .checked_add(group.compressed_len)
            .ok_or(PithosError::IntegerOverflow)?;
        if record.payload_offset < payload_section.offset
            || record.payload_offset != previous_group_end
            || group_end > payload_end
        {
            return Err(PithosError::InvalidRange);
        }
        let mappings = restore_map
            .iter()
            .filter(|restore| restore.group_id == group.group_id)
            .collect::<Vec<_>>();
        if mappings.len() != group.chunk_count as usize || mappings.is_empty() {
            return Err(PithosError::InvalidMetadata("group chunk count"));
        }
        let mut expected_offset = 0_u64;
        for restore in mappings {
            if restore.group_offset != expected_offset {
                return Err(PithosError::InvalidMetadata("restore group order"));
            }
            expected_offset = expected_offset
                .checked_add(restore.length)
                .ok_or(PithosError::IntegerOverflow)?;
        }
        if expected_offset != group.uncompressed_len {
            return Err(PithosError::InvalidMetadata("restore group length"));
        }
        previous_group_end = group_end;
    }
    if previous_group_end != payload_end {
        return Err(PithosError::InvalidMetadata("unreferenced payload bytes"));
    }

    Ok(ParsedArchive {
        report: VerificationReport {
            archive_bytes: file_length,
            original_bytes: original_size,
            entry_count: entries.len() as u64,
            file_count,
            directory_count,
            hardlink_count,
            symlink_count,
            group_count: groups.len() as u64,
            blake3_root: [0; 32],
        },
        entries,
        groups,
        restore_map,
        codec_registry,
        payload_section: payload_section.clone(),
        footer_offset: header.footer_offset,
        expected_root: footer.blake3_root,
    })
}

fn verify_open_file(
    file: &mut File,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<ParsedArchive> {
    let mut parsed = read_catalog(file, limits, cancellation)?;
    let (payload_crc, _) = digest_range(
        file,
        parsed.payload_section.offset,
        parsed.payload_section.length,
        cancellation,
    )?;
    if payload_crc != parsed.payload_section.crc32c {
        return Err(PithosError::ChecksumMismatch);
    }
    for group in &parsed.groups {
        cancellation.checkpoint()?;
        let decoded = decode_group(
            file,
            group,
            parsed.codec_registry.as_ref(),
            limits,
            cancellation,
        )?;
        for restore in parsed
            .restore_map
            .iter()
            .filter(|restore| restore.group_id == group.group.group_id)
        {
            let entry = parsed
                .entries
                .get(restore.entry_id as usize)
                .ok_or(PithosError::InvalidMetadata("restore entry"))?;
            let bytes = restored_slice(&decoded, restore, 0, restore.length)?;
            if blake3::hash(bytes).as_bytes() != &entry.blake3 {
                return Err(PithosError::HashMismatch);
            }
        }
    }
    let (_, root) = digest_range(file, 0, parsed.footer_offset, cancellation)?;
    if root != parsed.expected_root {
        return Err(PithosError::HashMismatch);
    }
    parsed.report.blake3_root = root;
    Ok(parsed)
}

fn entry_summary(entry: &EntryRecord) -> Result<ArchiveEntrySummary> {
    Ok(ArchiveEntrySummary {
        entry_id: entry.entry_id,
        path: entry.path.to_path_buf()?.to_string_lossy().into_owned(),
        kind: archive_entry_kind(&entry.kind),
        size: entry.size,
        modified_ns: entry.modified_ns,
    })
}

fn extract_report(entry: &EntryRecord, bytes_written: u64) -> ExtractReport {
    ExtractReport {
        path: entry
            .path
            .to_path_buf()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        bytes_written,
        kind: archive_entry_kind(&entry.kind),
    }
}

fn archive_entry_kind(kind: &EntryKind) -> ArchiveEntryKind {
    match kind {
        EntryKind::File { .. } => ArchiveEntryKind::File,
        EntryKind::Directory => ArchiveEntryKind::Directory,
        EntryKind::Hardlink { .. } => ArchiveEntryKind::Hardlink,
        EntryKind::Symlink { .. } => ArchiveEntryKind::Symlink,
    }
}

fn file_source_entry<'a>(
    entry: &'a EntryRecord,
    entries: &'a [EntryRecord],
) -> Result<&'a EntryRecord> {
    match entry.kind {
        EntryKind::File { .. } => Ok(entry),
        EntryKind::Hardlink { target_entry_id } => entries
            .get(target_entry_id as usize)
            .ok_or(PithosError::InvalidMetadata("hardlink target")),
        _ => Err(PithosError::InvalidMetadata("file source")),
    }
}

fn restore_for_entry(parsed: &ParsedArchive, entry_id: u64) -> Result<&RestoreMapRecord> {
    parsed
        .restore_map
        .iter()
        .find(|restore| restore.entry_id == entry_id)
        .ok_or(PithosError::InvalidMetadata("restore entry missing"))
}

fn restored_slice<'a>(
    decoded_group: &'a [u8],
    restore: &RestoreMapRecord,
    offset: u64,
    length: u64,
) -> Result<&'a [u8]> {
    let entry_end = offset
        .checked_add(length)
        .ok_or(PithosError::IntegerOverflow)?;
    if entry_end > restore.length {
        return Err(PithosError::InvalidRange);
    }
    let start = restore
        .group_offset
        .checked_add(offset)
        .ok_or(PithosError::IntegerOverflow)?;
    let end = start
        .checked_add(length)
        .ok_or(PithosError::IntegerOverflow)?;
    let start = usize::try_from(start).map_err(|_| PithosError::IntegerOverflow)?;
    let end = usize::try_from(end).map_err(|_| PithosError::IntegerOverflow)?;
    decoded_group
        .get(start..end)
        .ok_or(PithosError::InvalidRange)
}

fn decode_group(
    file: &mut File,
    record: &GroupTableRecord,
    registry: Option<&CodecRegistry>,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    cancellation.checkpoint()?;
    let maximum_compressed = limits
        .max_group_output
        .checked_add(1024 * 1024)
        .ok_or(PithosError::IntegerOverflow)?;
    let payload = read_exact_vec(
        file,
        record.payload_offset,
        record.group.compressed_len,
        maximum_compressed,
    )?;
    if crc32c::crc32c(&payload) != record.group.payload_crc32c {
        return Err(PithosError::ChecksumMismatch);
    }
    let codec_id = if record.group.codec_chain_id == 0 {
        if registry.is_some() {
            return Err(PithosError::UnsupportedCodec);
        }
        CodecId::Store
    } else {
        let registry = registry.ok_or(PithosError::UnsupportedCodec)?;
        let codec = registry
            .chain(record.group.codec_chain_id)
            .ok_or(PithosError::UnsupportedCodec)?;
        CodecId::from_u16(codec.codec_id).ok_or(PithosError::UnsupportedCodec)?
    };
    let capacity =
        usize::try_from(record.group.uncompressed_len).map_err(|_| PithosError::MemoryLimit)?;
    let mut decoded = Vec::with_capacity(capacity);
    codec_for_id(codec_id).decode(
        &mut std::io::Cursor::new(payload),
        record.group.uncompressed_len,
        &mut decoded,
    )?;
    cancellation.checkpoint()?;
    Ok(decoded)
}

fn validate_header_limits(
    header: &GlobalHeader,
    file_length: u64,
    limits: &DecodeLimits,
) -> Result<()> {
    if header.flags != 0 {
        return Err(PithosError::UnsupportedContainerVersion);
    }
    if ![REQUIRED_RAW_SECTIONS, REQUIRED_COMPRESSED_SECTIONS].contains(&header.section_count)
        || header.section_count > limits.max_sections
    {
        return Err(PithosError::ResourceLimit("section count"));
    }
    if header.entry_count > limits.max_entries {
        return Err(PithosError::ResourceLimit("entry count"));
    }
    if header.group_count > limits.max_groups {
        return Err(PithosError::ResourceLimit("group count"));
    }
    if header.logical_chunk_count > limits.max_chunks {
        return Err(PithosError::ResourceLimit("chunk count"));
    }
    if header.original_total_size > limits.max_original_bytes {
        return Err(PithosError::ResourceLimit("original bytes"));
    }
    if header.footer_offset >= file_length {
        return Err(PithosError::InvalidRange);
    }
    Ok(())
}

fn validate_section_ranges<'a>(
    records: impl Iterator<Item = &'a SectionDirectoryRecord>,
    directory_end: u64,
    footer_offset: u64,
) -> Result<()> {
    let mut ranges = records
        .map(|record| {
            let end = record
                .offset
                .checked_add(record.length)
                .ok_or(PithosError::IntegerOverflow)?;
            if record.offset < directory_end || end > footer_offset {
                return Err(PithosError::InvalidRange);
            }
            Ok((record.offset, end))
        })
        .collect::<Result<Vec<_>>>()?;
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(PithosError::OverlappingSections);
        }
    }
    Ok(())
}

fn read_json_section<T: DeserializeOwned>(
    file: &mut File,
    section: &SectionDirectoryRecord,
    limits: &DecodeLimits,
) -> Result<T> {
    let bytes = read_exact_vec(
        file,
        section.offset,
        section.length,
        limits.max_metadata_bytes,
    )?;
    if crc32c::crc32c(&bytes) != section.crc32c {
        return Err(PithosError::ChecksumMismatch);
    }
    serde_json::from_slice(&bytes).map_err(|_| PithosError::InvalidMetadata("JSON section"))
}

fn read_checked_section(
    file: &mut File,
    section: &SectionDirectoryRecord,
    limits: &DecodeLimits,
) -> Result<Vec<u8>> {
    let bytes = read_exact_vec(
        file,
        section.offset,
        section.length,
        limits.max_metadata_bytes,
    )?;
    if crc32c::crc32c(&bytes) != section.crc32c {
        return Err(PithosError::ChecksumMismatch);
    }
    Ok(bytes)
}

fn read_exact_vec(file: &mut File, offset: u64, length: u64, maximum: u64) -> Result<Vec<u8>> {
    if length > maximum {
        return Err(PithosError::ResourceLimit("metadata allocation"));
    }
    let length = usize::try_from(length).map_err(|_| PithosError::ResourceLimit("usize"))?;
    let mut bytes = vec![0_u8; length];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn required_section(
    sections: &BTreeMap<u16, SectionDirectoryRecord>,
    section_type: SectionType,
) -> Result<&SectionDirectoryRecord> {
    sections
        .get(&(section_type as u16))
        .ok_or(PithosError::MissingSection(section_name(section_type)))
}

fn section_name(section_type: SectionType) -> &'static str {
    match section_type {
        SectionType::CodecRegistry => "CodecRegistry",
        SectionType::EntryTable => "EntryTable",
        SectionType::GroupTable => "GroupTable",
        SectionType::PayloadArea => "PayloadArea",
        SectionType::RestoreMap => "RestoreMap",
        SectionType::CentralIndex => "CentralIndex",
        SectionType::IntegrityTree => "IntegrityTree",
        _ => "unsupported",
    }
}

fn validate_archive_path(path: &ArchivePath, limits: &DecodeLimits) -> Result<()> {
    if path.components.len() as u64 > limits.max_path_components
        || path.encoded_len() > limits.max_path_bytes
    {
        return Err(PithosError::ResourceLimit("path"));
    }
    path.to_path_buf()?;
    Ok(())
}

fn checked_total(current: u64, value: u64, limits: &DecodeLimits) -> Result<u64> {
    let total = current
        .checked_add(value)
        .ok_or(PithosError::IntegerOverflow)?;
    if total > limits.max_original_bytes {
        return Err(PithosError::ResourceLimit("original bytes"));
    }
    Ok(total)
}

fn serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|_| PithosError::InvalidMetadata("serialization"))
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Creates the selected entry's parent directories without traversing a symlink
/// introduced below the caller-controlled output root.
fn prepare_extract_parent(output_root: &Path, archive_path: &ArchivePath) -> Result<()> {
    fs::create_dir_all(output_root)?;
    if fs::symlink_metadata(output_root)?.file_type().is_symlink() {
        return Err(PithosError::UnsafePath);
    }
    let relative = archive_path.to_path_buf()?;
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = output_root.to_path_buf();
    for component in parent.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(PithosError::UnsafePath);
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(PithosError::InvalidMetadata("extract parent")),
            Err(error) if error.kind() == ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn write_section(
    spool: &mut tempfile::NamedTempFile,
    section_type: SectionType,
    bytes: &[u8],
) -> Result<SectionDirectoryRecord> {
    let offset = spool.stream_position()?;
    spool.write_all(bytes)?;
    Ok(SectionDirectoryRecord {
        section_type: section_type as u16,
        section_version: 1,
        flags: 0,
        offset,
        length: bytes.len() as u64,
        crc32c: crc32c::crc32c(bytes),
        reserved: 0,
    })
}

fn digest_range(
    file: &mut File,
    offset: u64,
    length: u64,
    cancellation: &CancellationToken,
) -> Result<(u32, [u8; 32])> {
    file.seek(SeekFrom::Start(offset))?;
    let mut remaining = length;
    let mut buffer = [0_u8; IO_BUFFER_SIZE];
    let mut crc = 0_u32;
    let mut hasher = blake3::Hasher::new();
    while remaining > 0 {
        cancellation.checkpoint()?;
        let wanted = usize::try_from(remaining.min(IO_BUFFER_SIZE as u64))
            .map_err(|_| PithosError::IntegerOverflow)?;
        let read = file.read(&mut buffer[..wanted])?;
        if read == 0 {
            return Err(std::io::Error::from(ErrorKind::UnexpectedEof).into());
        }
        crc = crc32c::crc32c_append(crc, &buffer[..read]);
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok((crc, *hasher.finalize().as_bytes()))
}

fn copy_input_file(
    path: &Path,
    initial_metadata: &fs::Metadata,
    initial_identity: Option<&Arc<same_file::Handle>>,
    destination: &mut tempfile::NamedTempFile,
    payload_crc: &mut u32,
    cancellation: &CancellationToken,
) -> Result<(u64, [u8; 32], u32)> {
    let expected_length = initial_metadata.len();
    let current_metadata = fs::metadata(path)?;
    if !same_file_metadata(initial_metadata, &current_metadata)
        || file_identity(path).as_ref() != initial_identity
    {
        return Err(PithosError::InputChanged);
    }
    let mut source = File::open(path)?;
    let mut buffer = [0_u8; IO_BUFFER_SIZE];
    let mut written = 0_u64;
    let mut crc = 0_u32;
    let mut hasher = blake3::Hasher::new();
    loop {
        cancellation.checkpoint()?;
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        written = written
            .checked_add(read as u64)
            .ok_or(PithosError::IntegerOverflow)?;
        if written > expected_length {
            return Err(PithosError::InputChanged);
        }
        destination.write_all(&buffer[..read])?;
        crc = crc32c::crc32c_append(crc, &buffer[..read]);
        *payload_crc = crc32c::crc32c_append(*payload_crc, &buffer[..read]);
        hasher.update(&buffer[..read]);
    }
    let final_metadata = fs::metadata(path)?;
    if written != expected_length
        || !same_file_metadata(initial_metadata, &final_metadata)
        || file_identity(path).as_ref() != initial_identity
    {
        return Err(PithosError::InputChanged);
    }
    Ok((written, *hasher.finalize().as_bytes(), crc))
}

#[derive(Debug)]
struct ScannedEntry {
    source: PathBuf,
    archive_path: ArchivePath,
    sort_key: Vec<u8>,
    metadata: fs::Metadata,
    identity: Option<Arc<same_file::Handle>>,
    kind: ScannedKind,
}

#[derive(Debug)]
enum ScannedKind {
    File,
    Directory,
    Symlink {
        target: LinkTarget,
        target_is_dir: bool,
    },
}

const SCANNED_ENTRY_MEMORY_OVERHEAD: u64 = 512;
const PENDING_CHILD_MEMORY_OVERHEAD: u64 = 128;

#[derive(Debug)]
struct ScanBudget {
    max_entries: u64,
    max_bytes: u64,
    retained_entries: u64,
    retained_bytes: u64,
    transient_bytes: u64,
}

impl ScanBudget {
    fn new(limits: &DecodeLimits) -> Self {
        Self {
            max_entries: limits.max_entries,
            max_bytes: limits.max_metadata_bytes,
            retained_entries: 0,
            retained_bytes: 0,
            transient_bytes: 0,
        }
    }

    fn ensure_pending_slot(&self, pending: usize) -> Result<()> {
        let pending = u64::try_from(pending).map_err(|_| PithosError::IntegerOverflow)?;
        if self
            .retained_entries
            .checked_add(pending)
            .ok_or(PithosError::IntegerOverflow)?
            >= self.max_entries
        {
            return Err(PithosError::ResourceLimit("entry count"));
        }
        Ok(())
    }

    fn reserve_transient(&mut self, bytes: u64) -> Result<()> {
        let next = self
            .retained_bytes
            .checked_add(self.transient_bytes)
            .and_then(|total| total.checked_add(bytes))
            .ok_or(PithosError::IntegerOverflow)?;
        if next > self.max_bytes {
            return Err(PithosError::ResourceLimit("scan memory"));
        }
        self.transient_bytes = self
            .transient_bytes
            .checked_add(bytes)
            .ok_or(PithosError::IntegerOverflow)?;
        Ok(())
    }

    fn release_transient(&mut self, bytes: u64) {
        self.transient_bytes = self.transient_bytes.saturating_sub(bytes);
    }

    fn retain_entry(
        &mut self,
        source: &Path,
        archive_path: &ArchivePath,
        sort_key: &[u8],
        kind_bytes: u64,
    ) -> Result<()> {
        if self.retained_entries >= self.max_entries {
            return Err(PithosError::ResourceLimit("entry count"));
        }
        let bytes = SCANNED_ENTRY_MEMORY_OVERHEAD
            .checked_add(encoded_path_len(source)?)
            .and_then(|total| total.checked_add(archive_path.encoded_len()))
            .and_then(|total| total.checked_add(archive_path.components.len() as u64 * 32))
            .and_then(|total| total.checked_add(sort_key.len() as u64))
            .and_then(|total| total.checked_add(kind_bytes))
            .ok_or(PithosError::IntegerOverflow)?;
        let next = self
            .retained_bytes
            .checked_add(self.transient_bytes)
            .and_then(|total| total.checked_add(bytes))
            .ok_or(PithosError::IntegerOverflow)?;
        if next > self.max_bytes {
            return Err(PithosError::ResourceLimit("scan memory"));
        }
        self.retained_entries += 1;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(bytes)
            .ok_or(PithosError::IntegerOverflow)?;
        Ok(())
    }
}

#[derive(Debug)]
struct PendingChild {
    path: PathBuf,
    sort_key: Vec<u8>,
    memory_charge: u64,
}

fn scan_inputs(
    inputs: &[PathBuf],
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<Vec<ScannedEntry>> {
    let mut entries = Vec::new();
    let mut budget = ScanBudget::new(limits);
    if inputs.len() as u64 > limits.max_entries {
        return Err(PithosError::ResourceLimit("entry count"));
    }
    let single_directory = inputs.len() == 1 && fs::symlink_metadata(&inputs[0])?.is_dir();
    for input in inputs {
        cancellation.checkpoint()?;
        let metadata = fs::symlink_metadata(input)?;
        let root_scope = if metadata.is_dir() {
            fs::canonicalize(input)?
        } else {
            fs::canonicalize(input.parent().unwrap_or_else(|| Path::new(".")))?
        };
        if single_directory {
            scan_directory_children(
                input,
                Path::new(""),
                &root_scope,
                &mut entries,
                limits,
                &mut budget,
                cancellation,
            )?;
        } else {
            let name = input.file_name().ok_or(PithosError::UnsafePath)?;
            scan_node(
                input,
                Path::new(name),
                &root_scope,
                &mut entries,
                limits,
                &mut budget,
                cancellation,
            )?;
        }
    }
    entries.sort_unstable_by(|left, right| left.sort_key.cmp(&right.sort_key));
    if entries
        .windows(2)
        .any(|pair| pair[0].sort_key == pair[1].sort_key)
    {
        return Err(PithosError::InvalidMetadata("duplicate input path"));
    }
    Ok(entries)
}

fn scan_directory_children(
    directory: &Path,
    archive_prefix: &Path,
    root_scope: &Path,
    entries: &mut Vec<ScannedEntry>,
    limits: &DecodeLimits,
    budget: &mut ScanBudget,
    cancellation: &CancellationToken,
) -> Result<()> {
    let mut children = Vec::new();
    for entry in fs::read_dir(directory)? {
        cancellation.checkpoint()?;
        budget.ensure_pending_slot(children.len())?;
        let path = entry?.path();
        let name = path.file_name().ok_or(PithosError::UnsafePath)?;
        let sort_key = ArchivePath::from_relative(Path::new(name))?.sort_key();
        let memory_charge = PENDING_CHILD_MEMORY_OVERHEAD
            .checked_add(encoded_path_len(&path)?)
            .and_then(|total| total.checked_add(sort_key.len() as u64))
            .ok_or(PithosError::IntegerOverflow)?;
        budget.reserve_transient(memory_charge)?;
        children.push(PendingChild {
            path,
            sort_key,
            memory_charge,
        });
    }
    children.sort_unstable_by(|left, right| left.sort_key.cmp(&right.sort_key));
    for child in children {
        cancellation.checkpoint()?;
        let name = child.path.file_name().ok_or(PithosError::UnsafePath)?;
        let archive_path = archive_prefix.join(name);
        scan_node(
            &child.path,
            &archive_path,
            root_scope,
            entries,
            limits,
            budget,
            cancellation,
        )?;
        budget.release_transient(child.memory_charge);
    }
    Ok(())
}

fn scan_node(
    source: &Path,
    relative: &Path,
    root_scope: &Path,
    entries: &mut Vec<ScannedEntry>,
    limits: &DecodeLimits,
    budget: &mut ScanBudget,
    cancellation: &CancellationToken,
) -> Result<()> {
    cancellation.checkpoint()?;
    let metadata = fs::symlink_metadata(source)?;
    let archive_path = ArchivePath::from_relative(relative)?;
    validate_archive_path(&archive_path, limits)?;
    let sort_key = archive_path.sort_key();
    if metadata.file_type().is_symlink() {
        let target_path = fs::read_link(source)?;
        let target = LinkTarget::from_relative(&target_path)?;
        if !target.resolves_within(&archive_path) {
            return Err(PithosError::UnsafeSymlink);
        }
        let canonical_target = fs::canonicalize(source)?;
        if !canonical_target.starts_with(root_scope) {
            return Err(PithosError::UnsafeSymlink);
        }
        let target_is_dir = fs::metadata(source)?.is_dir();
        budget.retain_entry(
            source,
            &archive_path,
            &sort_key,
            link_target_memory_bytes(&target)?,
        )?;
        entries.push(ScannedEntry {
            source: source.to_path_buf(),
            archive_path,
            sort_key,
            metadata,
            identity: None,
            kind: ScannedKind::Symlink {
                target,
                target_is_dir,
            },
        });
    } else if metadata.is_dir() {
        budget.retain_entry(source, &archive_path, &sort_key, 0)?;
        entries.push(ScannedEntry {
            source: source.to_path_buf(),
            archive_path,
            sort_key,
            metadata,
            identity: None,
            kind: ScannedKind::Directory,
        });
        scan_directory_children(
            source,
            relative,
            root_scope,
            entries,
            limits,
            budget,
            cancellation,
        )?;
    } else if metadata.is_file() {
        let identity = file_identity(source);
        budget.retain_entry(source, &archive_path, &sort_key, 0)?;
        entries.push(ScannedEntry {
            source: source.to_path_buf(),
            archive_path,
            sort_key,
            metadata,
            identity,
            kind: ScannedKind::File,
        });
    } else {
        return Err(PithosError::UnsupportedFileType);
    }
    Ok(())
}

fn encoded_path_len(path: &Path) -> Result<u64> {
    u64::try_from(path.as_os_str().as_encoded_bytes().len())
        .map_err(|_| PithosError::IntegerOverflow)
}

fn link_target_memory_bytes(target: &LinkTarget) -> Result<u64> {
    target
        .components
        .iter()
        .try_fold(0_u64, |total, component| {
            let bytes = match component {
                LinkComponent::Parent => 1,
                LinkComponent::Normal(component) => u64::try_from(component.bytes.len())
                    .map_err(|_| PithosError::IntegerOverflow)?,
            };
            total
                .checked_add(32)
                .and_then(|value| value.checked_add(bytes))
                .ok_or(PithosError::IntegerOverflow)
        })
}

fn file_identity(path: &Path) -> Option<Arc<same_file::Handle>> {
    same_file::Handle::from_path(path).ok().map(Arc::new)
}

fn same_file_metadata(expected: &fs::Metadata, actual: &fs::Metadata) -> bool {
    expected.len() == actual.len() && expected.modified().ok() == actual.modified().ok()
}

fn modified_ns(metadata: &fs::Metadata) -> i64 {
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos().min(i64::MAX as u128) as i64,
        Err(error) => -(error.duration().as_nanos().min(i64::MAX as u128) as i64),
    }
}

#[cfg(unix)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode()
}

#[cfg(not(unix))]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(unix)]
fn apply_file_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_file_mode(path: &Path, mode: u32) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path, _target_is_dir: bool) -> Result<()> {
    std::os::unix::fs::symlink(target, destination)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path, target_is_dir: bool) -> Result<()> {
    if target_is_dir {
        std::os::windows::fs::symlink_dir(target, destination)?;
    } else {
        std::os::windows::fs::symlink_file(target, destination)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _destination: &Path, _target_is_dir: bool) -> Result<()> {
    Err(PithosError::UnsupportedFileType)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_inputs_enforces_memory_budget_while_collecting() {
        let temp = tempfile::tempdir().expect("temporary input directory");
        fs::write(temp.path().join("entry.bin"), b"payload").expect("write input");
        let limits = DecodeLimits {
            max_entries: 100,
            max_metadata_bytes: 1,
            ..DecodeLimits::default()
        };

        let error = scan_inputs(
            &[temp.path().to_path_buf()],
            &limits,
            &CancellationToken::new(),
        )
        .expect_err("the scanner must reject before retaining metadata over budget");
        assert!(matches!(error, PithosError::ResourceLimit("scan memory")));
    }

    #[test]
    fn compressed_hash_rejects_same_length_replacement_after_scan() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("changing.bin");
        fs::write(&input, b"before").unwrap();
        let initial_metadata = fs::metadata(&input).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::remove_file(&input).unwrap();
        fs::write(&input, b"after!").unwrap();

        let error = hash_input_file(&input, &initial_metadata, None, &CancellationToken::new())
            .expect_err("a same-length replacement after scan must be rejected");

        assert!(matches!(error, PithosError::InputChanged));
    }
}
