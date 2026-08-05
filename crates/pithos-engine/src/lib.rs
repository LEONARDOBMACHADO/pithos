//! RAW/STORE pack, verification and transactional unpack orchestration.

use pithos_core::{CompressionProfile, DecodeLimits, PithosError, Result};
use pithos_format::{
    ArchivePath, CentralIndexRecord, EntryKind, EntryRecord, FOOTER_LEN, Footer, GlobalHeader,
    GroupRecord, GroupTableRecord, HEADER_LEN, IntegrityRecord, LinkComponent, LinkTarget,
    REQUIRED_RAW_SECTIONS, RestoreMapRecord, SECTION_ENTRY_LEN, SectionDirectoryRecord,
    SectionType,
};
use pithos_io::{atomic_commit, create_atomic_spool};
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
            max_temp_bytes: u64::MAX,
            max_output_bytes: u64::MAX,
            max_metadata_bytes: decode.max_metadata_bytes,
            max_entries: decode.max_entries,
        }
    }
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
        return Err(PithosError::UnsupportedCodec);
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
    let mut hardlinks: HashMap<same_file::Handle, (u64, u64, [u8; 32])> = HashMap::new();
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
                let identity = file_identity(&scanned_entry.source);
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
            let (crc, hash) = copy_range(
                &mut archive,
                group.payload_offset,
                group.group.compressed_len,
                &mut spool,
                cancellation,
            )?;
            if crc != group.group.payload_crc32c || hash != source.blake3 {
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
    let (crc, hash) = copy_range(
        &mut source_archive,
        group.payload_offset,
        group.group.compressed_len,
        output,
        cancellation,
    )?;
    if crc != group.group.payload_crc32c || hash != source.blake3 {
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
    let (crc, entry_hash, range_hash) = copy_verified_subrange(
        &mut archive,
        group.payload_offset,
        group.group.compressed_len,
        request.offset,
        request.length,
        output,
        cancellation,
    )?;
    if crc != group.group.payload_crc32c || entry_hash != source.blake3 {
        return Err(PithosError::HashMismatch);
    }
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
        let destination = staging.path().join(entry.path.to_path_buf()?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        let (crc, hash) = copy_range(
            &mut archive,
            group.payload_offset,
            group.group.compressed_len,
            &mut output,
            cancellation,
        )?;
        if crc != group.group.payload_crc32c || hash != entry.blake3 {
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
        if !RAW_SECTION_TYPES.contains(&section_type) {
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

    let aggregate_metadata = [
        entry_section,
        group_section,
        restore_section,
        index_section,
        integrity_section,
    ]
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
    if entries.len() as u64 != header.entry_count
        || groups.len() as u64 != header.group_count
        || groups.len() as u64 != header.logical_chunk_count
        || restore_map.len() != groups.len()
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
    let mut group_owners = vec![None; groups.len()];
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
                if group.group.group_id != *group_id || group.group.uncompressed_len != entry.size {
                    return Err(PithosError::InvalidMetadata("entry group size"));
                }
                let owner = group_owners
                    .get_mut(*group_id as usize)
                    .ok_or(PithosError::InvalidMetadata("entry group"))?;
                if owner.replace(position).is_some() {
                    return Err(PithosError::InvalidMetadata("duplicate group owner"));
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
            || group.codec_chain_id != 0
            || group.chunk_count != 1
            || group.descriptor_len != 0
            || group.uncompressed_len != group.compressed_len
        {
            return Err(PithosError::InvalidMetadata("RAW group"));
        }
        if group.compressed_len > limits.max_group_output {
            return Err(PithosError::ResourceLimit("group output"));
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
        let owner = group_owners[position].ok_or(PithosError::InvalidMetadata("group owner"))?;
        let entry = &entries[owner];
        let restore = &restore_map[position];
        if restore.entry_id != entry.entry_id
            || restore.original_offset != 0
            || restore.length != group.uncompressed_len
            || restore.group_id != group.group_id
            || restore.group_offset != 0
        {
            return Err(PithosError::InvalidMetadata("restore map"));
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
        let entry = parsed
            .entries
            .iter()
            .find(|entry| matches!(entry.kind, EntryKind::File { group_id } if group_id == group.group.group_id))
            .ok_or(PithosError::InvalidMetadata("group owner"))?;
        let (crc, hash) = digest_range(
            file,
            group.payload_offset,
            group.group.compressed_len,
            cancellation,
        )?;
        if crc != group.group.payload_crc32c {
            return Err(PithosError::ChecksumMismatch);
        }
        if hash != entry.blake3 {
            return Err(PithosError::HashMismatch);
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

fn validate_header_limits(
    header: &GlobalHeader,
    file_length: u64,
    limits: &DecodeLimits,
) -> Result<()> {
    if header.flags != 0 {
        return Err(PithosError::UnsupportedContainerVersion);
    }
    if header.section_count != REQUIRED_RAW_SECTIONS || header.section_count > limits.max_sections {
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

fn copy_range<W: Write>(
    source: &mut File,
    offset: u64,
    length: u64,
    destination: &mut W,
    cancellation: &CancellationToken,
) -> Result<(u32, [u8; 32])> {
    source.seek(SeekFrom::Start(offset))?;
    let mut remaining = length;
    let mut buffer = [0_u8; IO_BUFFER_SIZE];
    let mut crc = 0_u32;
    let mut hasher = blake3::Hasher::new();
    while remaining > 0 {
        cancellation.checkpoint()?;
        let wanted = remaining.min(IO_BUFFER_SIZE as u64) as usize;
        let read = source.read(&mut buffer[..wanted])?;
        if read == 0 {
            return Err(std::io::Error::from(ErrorKind::UnexpectedEof).into());
        }
        destination.write_all(&buffer[..read])?;
        crc = crc32c::crc32c_append(crc, &buffer[..read]);
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok((crc, *hasher.finalize().as_bytes()))
}

fn copy_verified_subrange<W: Write>(
    source: &mut File,
    group_offset: u64,
    group_length: u64,
    range_offset: u64,
    range_length: u64,
    destination: &mut W,
    cancellation: &CancellationToken,
) -> Result<(u32, [u8; 32], [u8; 32])> {
    source.seek(SeekFrom::Start(group_offset))?;
    let range_end = range_offset
        .checked_add(range_length)
        .ok_or(PithosError::IntegerOverflow)?;
    let mut consumed = 0_u64;
    let mut remaining = group_length;
    let mut buffer = [0_u8; IO_BUFFER_SIZE];
    let mut crc = 0_u32;
    let mut entry_hasher = blake3::Hasher::new();
    let mut range_hasher = blake3::Hasher::new();
    while remaining > 0 {
        cancellation.checkpoint()?;
        let wanted = remaining.min(IO_BUFFER_SIZE as u64) as usize;
        let read = source.read(&mut buffer[..wanted])?;
        if read == 0 {
            return Err(std::io::Error::from(ErrorKind::UnexpectedEof).into());
        }
        let chunk_end = consumed
            .checked_add(read as u64)
            .ok_or(PithosError::IntegerOverflow)?;
        let write_start = consumed.max(range_offset);
        let write_end = chunk_end.min(range_end);
        if write_start < write_end {
            let start = usize::try_from(write_start - consumed)
                .map_err(|_| PithosError::IntegerOverflow)?;
            let end =
                usize::try_from(write_end - consumed).map_err(|_| PithosError::IntegerOverflow)?;
            let selected = &buffer[start..end];
            destination.write_all(selected)?;
            range_hasher.update(selected);
        }
        crc = crc32c::crc32c_append(crc, &buffer[..read]);
        entry_hasher.update(&buffer[..read]);
        consumed = chunk_end;
        remaining -= read as u64;
    }
    Ok((
        crc,
        *entry_hasher.finalize().as_bytes(),
        *range_hasher.finalize().as_bytes(),
    ))
}

fn copy_input_file(
    path: &Path,
    initial_metadata: &fs::Metadata,
    destination: &mut tempfile::NamedTempFile,
    payload_crc: &mut u32,
    cancellation: &CancellationToken,
) -> Result<(u64, [u8; 32], u32)> {
    let expected_length = initial_metadata.len();
    let expected_modified = initial_metadata.modified().ok();
    let expected_identity = file_identity(path);
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
        || final_metadata.len() != expected_length
        || final_metadata.modified().ok() != expected_modified
        || file_identity(path) != expected_identity
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
        budget.retain_entry(source, &archive_path, &sort_key, 0)?;
        entries.push(ScannedEntry {
            source: source.to_path_buf(),
            archive_path,
            sort_key,
            metadata,
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

fn file_identity(path: &Path) -> Option<same_file::Handle> {
    same_file::Handle::from_path(path).ok()
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
}
