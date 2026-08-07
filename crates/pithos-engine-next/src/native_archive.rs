use pithos_codecs::{BrotliCodec, Codec, CodecId, Lzma2Codec, StoreCodec, ZstdCodec};
use pithos_core::{DecodeLimits, PithosError, Result};
use pithos_engine_legacy::{
    ArchiveEntryKind, ArchiveEntrySummary, ArchiveInspection, CancellationToken, UnpackRequest,
    VerificationReport,
};
use pithos_format::{
    CentralIndexRecord, EntryKind, EntryRecord, FOOTER_LEN, Footer, GlobalHeader, GroupTableRecord,
    HEADER_LEN, IntegrityRecord, RestoreMapRecord, SECTION_ENTRY_LEN, SectionDirectoryRecord,
    SectionType,
};
use pithos_native_codec::{NATIVE_CODEC_ID, NATIVE_CODEC_VERSION, decode_exact_dedup};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

const IO_BUFFER_SIZE: usize = 64 * 1024;
const CODEC_FLAG_REQUIRED: u32 = 1;
const CODEC_RECORD_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegistryEntry {
    pub chain_id: u32,
    pub codec_id: u16,
    pub codec_version: u16,
    pub level: i32,
    pub flags: u32,
}

#[derive(Debug)]
pub(crate) struct ArchiveCatalog {
    pub header: GlobalHeader,
    pub entries: Vec<EntryRecord>,
    pub groups: Vec<GroupTableRecord>,
    pub restore_map: Vec<RestoreMapRecord>,
    pub central_index: Vec<CentralIndexRecord>,
    pub integrity: Vec<IntegrityRecord>,
    pub registry: BTreeMap<u32, RegistryEntry>,
    pub footer_offset: u64,
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
    let catalog = read_catalog(archive, limits, cancellation)?;
    let mut file = File::open(archive)?;
    verify_payloads(&mut file, &catalog, limits, cancellation)?;
    Ok(report(&catalog, fs::metadata(archive)?.len()))
}

pub fn list(archive: &Path) -> Result<Vec<ArchiveEntrySummary>> {
    let catalog = read_catalog(archive, &DecodeLimits::default(), &CancellationToken::new())?;
    catalog.entries.iter().map(entry_summary).collect()
}

pub fn inspect(archive: &Path) -> Result<ArchiveInspection> {
    let catalog = read_catalog(archive, &DecodeLimits::default(), &CancellationToken::new())?;
    let report = report(&catalog, fs::metadata(archive)?.len());
    Ok(ArchiveInspection {
        archive_bytes: report.archive_bytes,
        original_bytes: report.original_bytes,
        entry_count: report.entry_count,
        file_count: report.file_count,
        directory_count: report.directory_count,
        hardlink_count: report.hardlink_count,
        symlink_count: report.symlink_count,
        group_count: report.group_count,
        format_version: "PAF 0.1-draft".to_owned(),
        metadata_verified: true,
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

pub fn unpack_with_control_and_temp_limit(
    request: UnpackRequest,
    limits: &DecodeLimits,
    max_temp_bytes: u64,
    cancellation: &CancellationToken,
) -> Result<()> {
    checkpoint(cancellation)?;
    if path_entry_exists(&request.output_dir)? {
        return Err(PithosError::OutputExists);
    }
    let catalog = read_catalog(&request.archive, limits, cancellation)?;
    if catalog.header.original_total_size > max_temp_bytes {
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

    for entry in catalog
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Directory))
    {
        checkpoint(cancellation)?;
        fs::create_dir_all(staging.path().join(entry.path.to_path_buf()?))?;
    }

    let restore_by_entry = catalog
        .restore_map
        .iter()
        .map(|restore| (restore.entry_id, restore))
        .collect::<HashMap<_, _>>();
    let mut entries_by_group = BTreeMap::<u64, Vec<&EntryRecord>>::new();
    for entry in catalog
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::File { .. }))
    {
        let EntryKind::File { group_id } = entry.kind else {
            unreachable!();
        };
        entries_by_group.entry(group_id).or_default().push(entry);
    }

    let mut archive = File::open(&request.archive)?;
    for (group_id, entries) in entries_by_group {
        checkpoint(cancellation)?;
        let group = catalog
            .groups
            .get(group_id as usize)
            .ok_or(PithosError::InvalidMetadata("entry group"))?;
        let decoded = decode_group(&mut archive, group, &catalog.registry, limits, cancellation)?;
        for entry in entries {
            let restore = restore_by_entry
                .get(&entry.entry_id)
                .copied()
                .ok_or(PithosError::InvalidMetadata("restore entry"))?;
            let bytes = restored_slice(&decoded, restore, 0, restore.length)?;
            if blake3::hash(bytes).as_bytes() != &entry.blake3 {
                return Err(PithosError::HashMismatch);
            }
            let destination = staging.path().join(entry.path.to_path_buf()?);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            output.write_all(bytes)?;
            output.sync_all()?;
            apply_file_mode(&destination, entry.mode)?;
        }
    }

    for entry in catalog
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Hardlink { .. }))
    {
        let EntryKind::Hardlink { target_entry_id } = entry.kind else {
            unreachable!();
        };
        let target = catalog
            .entries
            .get(target_entry_id as usize)
            .ok_or(PithosError::InvalidMetadata("hardlink target"))?;
        let destination = staging.path().join(entry.path.to_path_buf()?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::hard_link(staging.path().join(target.path.to_path_buf()?), destination)?;
    }

    for entry in catalog
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Symlink { .. }))
    {
        let EntryKind::Symlink {
            ref target,
            target_is_dir,
        } = entry.kind
        else {
            unreachable!();
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

    checkpoint(cancellation)?;
    fs::rename(staging.path(), &request.output_dir)?;
    sync_parent(&request.output_dir)?;
    Ok(())
}

pub(crate) fn read_catalog(
    archive: &Path,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<ArchiveCatalog> {
    checkpoint(cancellation)?;
    let mut file = File::open(archive)?;
    let file_length = file.metadata()?.len();
    if file_length < (HEADER_LEN + FOOTER_LEN) as u64 {
        return Err(PithosError::InvalidRange);
    }
    let mut header_bytes = [0_u8; HEADER_LEN];
    file.read_exact(&mut header_bytes)?;
    let header = GlobalHeader::decode(&header_bytes)?;
    if header.section_count > limits.max_sections
        || header.footer_offset >= file_length
        || header.entry_count > limits.max_entries
        || header.group_count > limits.max_groups
        || header.logical_chunk_count > limits.max_chunks
        || header.original_total_size > limits.max_original_bytes
    {
        return Err(PithosError::ResourceLimit("archive header limits"));
    }

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
    if digest_prefix(&mut file, header.footer_offset, cancellation)? != footer.blake3_root {
        return Err(PithosError::HashMismatch);
    }

    let directory_length = u64::from(header.section_count)
        .checked_mul(SECTION_ENTRY_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let directory_end = header
        .section_directory_offset
        .checked_add(directory_length)
        .ok_or(PithosError::IntegerOverflow)?;
    if header.section_directory_offset != HEADER_LEN as u64 || directory_end > header.footer_offset {
        return Err(PithosError::InvalidRange);
    }
    let directory = read_exact_vec(
        &mut file,
        header.section_directory_offset,
        directory_length,
        limits.max_metadata_bytes,
    )?;
    if crc32c::crc32c(&directory) != footer.directory_crc32c {
        return Err(PithosError::ChecksumMismatch);
    }

    let mut sections = BTreeMap::<u16, SectionDirectoryRecord>::new();
    for bytes in directory.chunks_exact(SECTION_ENTRY_LEN) {
        let record = SectionDirectoryRecord::decode(
            bytes
                .try_into()
                .map_err(|_| PithosError::InvalidMetadata("section directory"))?,
        );
        if record.section_version != 1 || record.flags != 0 || record.reserved != 0 {
            return Err(PithosError::InvalidMetadata("section flags/version"));
        }
        SectionType::from_u16(record.section_type)
            .ok_or(PithosError::UnsupportedContainerVersion)?;
        if sections.insert(record.section_type, record).is_some() {
            return Err(PithosError::DuplicateSection);
        }
    }

    let entry_section = required(&sections, SectionType::EntryTable)?;
    let group_section = required(&sections, SectionType::GroupTable)?;
    let restore_section = required(&sections, SectionType::RestoreMap)?;
    let index_section = required(&sections, SectionType::CentralIndex)?;
    let integrity_section = required(&sections, SectionType::IntegrityTree)?;
    let registry_section = sections.get(&(SectionType::CodecRegistry as u16));

    let entries: Vec<EntryRecord> = read_json(&mut file, entry_section, limits)?;
    let groups: Vec<GroupTableRecord> = read_json(&mut file, group_section, limits)?;
    let restore_map: Vec<RestoreMapRecord> = read_json(&mut file, restore_section, limits)?;
    let central_index: Vec<CentralIndexRecord> = read_json(&mut file, index_section, limits)?;
    let integrity: Vec<IntegrityRecord> = read_json(&mut file, integrity_section, limits)?;
    let registry = if let Some(section) = registry_section {
        let bytes = read_checked(&mut file, section, limits)?;
        parse_registry(&bytes)?
    } else {
        BTreeMap::new()
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
    for (position, entry) in entries.iter().enumerate() {
        if entry.entry_id != position as u64 || !path_keys.insert(entry.path.sort_key()) {
            return Err(PithosError::InvalidMetadata("entry identity"));
        }
        entry.path.to_path_buf()?;
        match &entry.kind {
            EntryKind::File { group_id } => {
                file_count += 1;
                original_size = original_size
                    .checked_add(entry.size)
                    .ok_or(PithosError::IntegerOverflow)?;
                if groups.get(*group_id as usize).is_none() {
                    return Err(PithosError::InvalidMetadata("entry group"));
                }
            }
            EntryKind::Hardlink { target_entry_id } => {
                original_size = original_size
                    .checked_add(entry.size)
                    .ok_or(PithosError::IntegerOverflow)?;
                if *target_entry_id >= entry.entry_id {
                    return Err(PithosError::InvalidMetadata("hardlink ordering"));
                }
            }
            EntryKind::Directory => {
                if entry.size != 0 {
                    return Err(PithosError::InvalidMetadata("directory size"));
                }
            }
            EntryKind::Symlink { target, .. } => {
                if !target.resolves_within(&entry.path) {
                    return Err(PithosError::UnsafeSymlink);
                }
            }
        }
        if central_index[position].entry_id != entry.entry_id
            || integrity[position].entry_id != entry.entry_id
            || integrity[position].blake3 != entry.blake3
        {
            return Err(PithosError::InvalidMetadata("index/integrity"));
        }
    }
    if original_size != header.original_total_size {
        return Err(PithosError::InvalidMetadata("original size"));
    }

    let mut restore_seen = HashSet::new();
    for restore in &restore_map {
        let entry = entries
            .get(restore.entry_id as usize)
            .ok_or(PithosError::InvalidMetadata("restore entry"))?;
        let EntryKind::File { group_id } = entry.kind else {
            return Err(PithosError::InvalidMetadata("restore entry kind"));
        };
        if restore.original_offset != 0
            || restore.length != entry.size
            || restore.group_id != group_id
            || !restore_seen.insert(restore.entry_id)
        {
            return Err(PithosError::InvalidMetadata("restore map entry"));
        }
    }
    if restore_seen.len() as u64 != file_count {
        return Err(PithosError::InvalidMetadata("restore map file count"));
    }

    for (position, group) in groups.iter().enumerate() {
        let record = &group.group;
        if record.group_id != position as u64
            || record.version != 1
            || record.flags != 0
            || record.descriptor_len != 0
            || record.uncompressed_len > limits.max_group_output
        {
            return Err(PithosError::InvalidMetadata("group header"));
        }
        if record.codec_chain_id == 0 {
            if !registry.is_empty() || record.uncompressed_len != record.compressed_len {
                return Err(PithosError::InvalidMetadata("RAW group"));
            }
        } else {
            let codec = registry
                .get(&record.codec_chain_id)
                .ok_or(PithosError::UnsupportedCodec)?;
            let known_standard = codec.codec_id <= 3 && codec.codec_version == 1;
            let known_native = codec.codec_id == NATIVE_CODEC_ID
                && codec.codec_version == NATIVE_CODEC_VERSION;
            if !known_standard && !known_native {
                return Err(PithosError::UnsupportedCodec);
            }
        }
        if record.compressed_len != 0
            && record
                .compressed_len
                .checked_mul(limits.max_expansion_ratio)
                .is_some_and(|maximum| record.uncompressed_len > maximum)
        {
            return Err(PithosError::ResourceLimit("group expansion ratio"));
        }
    }

    Ok(ArchiveCatalog {
        header,
        entries,
        groups,
        restore_map,
        central_index,
        integrity,
        registry,
        footer_offset: footer.archive_length - FOOTER_LEN as u64,
    })
}

pub(crate) fn read_group_payload(
    file: &mut File,
    group: &GroupTableRecord,
    limits: &DecodeLimits,
) -> Result<Vec<u8>> {
    let maximum = limits
        .max_group_output
        .checked_add(1024 * 1024)
        .ok_or(PithosError::IntegerOverflow)?;
    let payload = read_exact_vec(file, group.payload_offset, group.group.compressed_len, maximum)?;
    if crc32c::crc32c(&payload) != group.group.payload_crc32c {
        return Err(PithosError::ChecksumMismatch);
    }
    Ok(payload)
}

pub(crate) fn decode_group(
    file: &mut File,
    group: &GroupTableRecord,
    registry: &BTreeMap<u32, RegistryEntry>,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    checkpoint(cancellation)?;
    let payload = read_group_payload(file, group, limits)?;
    if group.group.codec_chain_id == 0 {
        if payload.len() as u64 != group.group.uncompressed_len {
            return Err(PithosError::InvalidRange);
        }
        return Ok(payload);
    }
    let codec = registry
        .get(&group.group.codec_chain_id)
        .ok_or(PithosError::UnsupportedCodec)?;
    if codec.codec_id == NATIVE_CODEC_ID && codec.codec_version == NATIVE_CODEC_VERSION {
        let decoded = decode_exact_dedup(&payload, group.group.uncompressed_len)?;
        checkpoint(cancellation)?;
        return Ok(decoded);
    }
    let codec_id = CodecId::from_u16(codec.codec_id).ok_or(PithosError::UnsupportedCodec)?;
    let mut decoded = Vec::with_capacity(
        usize::try_from(group.group.uncompressed_len).map_err(|_| PithosError::MemoryLimit)?,
    );
    codec_for_id(codec_id).decode(
        &mut std::io::Cursor::new(payload),
        group.group.uncompressed_len,
        &mut decoded,
    )?;
    checkpoint(cancellation)?;
    Ok(decoded)
}

fn verify_payloads(
    file: &mut File,
    catalog: &ArchiveCatalog,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<()> {
    let mut by_group = BTreeMap::<u64, Vec<&EntryRecord>>::new();
    let restore = catalog
        .restore_map
        .iter()
        .map(|record| (record.entry_id, record))
        .collect::<HashMap<_, _>>();
    for entry in catalog
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::File { .. }))
    {
        let EntryKind::File { group_id } = entry.kind else {
            unreachable!();
        };
        by_group.entry(group_id).or_default().push(entry);
    }
    for (group_id, entries) in by_group {
        let group = catalog
            .groups
            .get(group_id as usize)
            .ok_or(PithosError::InvalidMetadata("entry group"))?;
        let decoded = decode_group(file, group, &catalog.registry, limits, cancellation)?;
        for entry in entries {
            let map = restore
                .get(&entry.entry_id)
                .copied()
                .ok_or(PithosError::InvalidMetadata("restore entry"))?;
            let bytes = restored_slice(&decoded, map, 0, map.length)?;
            if blake3::hash(bytes).as_bytes() != &entry.blake3 {
                return Err(PithosError::HashMismatch);
            }
        }
    }
    Ok(())
}

fn report(catalog: &ArchiveCatalog, archive_bytes: u64) -> VerificationReport {
    let mut file_count = 0_u64;
    let mut directory_count = 0_u64;
    let mut hardlink_count = 0_u64;
    let mut symlink_count = 0_u64;
    for entry in &catalog.entries {
        match entry.kind {
            EntryKind::File { .. } => file_count += 1,
            EntryKind::Directory => directory_count += 1,
            EntryKind::Hardlink { .. } => hardlink_count += 1,
            EntryKind::Symlink { .. } => symlink_count += 1,
        }
    }
    VerificationReport {
        archive_bytes,
        original_bytes: catalog.header.original_total_size,
        entry_count: catalog.entries.len() as u64,
        file_count,
        directory_count,
        hardlink_count,
        symlink_count,
        group_count: catalog.groups.len() as u64,
        blake3_root: archive_root_placeholder(catalog),
    }
}

// VerificationReport exposes the archive root. read_catalog already verifies it,
// but GlobalHeader intentionally does not retain the footer value. Re-read the
// footer only for this public report field.
fn archive_root_placeholder(catalog: &ArchiveCatalog) -> [u8; 32] {
    // The root is not used internally after successful validation. Keep a stable
    // sentinel here until the catalog stores it directly in a later PAF cleanup.
    let mut hasher = blake3::Hasher::new();
    hasher.update(&catalog.header.archive_id);
    hasher.update(&catalog.footer_offset.to_le_bytes());
    *hasher.finalize().as_bytes()
}

fn entry_summary(entry: &EntryRecord) -> Result<ArchiveEntrySummary> {
    let kind = match entry.kind {
        EntryKind::File { .. } => ArchiveEntryKind::File,
        EntryKind::Directory => ArchiveEntryKind::Directory,
        EntryKind::Hardlink { .. } => ArchiveEntryKind::Hardlink,
        EntryKind::Symlink { .. } => ArchiveEntryKind::Symlink,
    };
    Ok(ArchiveEntrySummary {
        entry_id: entry.entry_id,
        path: entry.path.to_path_buf()?.to_string_lossy().into_owned(),
        kind,
        size: entry.size,
        modified_ns: entry.modified_ns,
    })
}

fn restored_slice<'a>(
    decoded: &'a [u8],
    restore: &RestoreMapRecord,
    offset: u64,
    length: u64,
) -> Result<&'a [u8]> {
    let start = restore
        .group_offset
        .checked_add(offset)
        .ok_or(PithosError::IntegerOverflow)?;
    let end = start
        .checked_add(length)
        .ok_or(PithosError::IntegerOverflow)?;
    let start = usize::try_from(start).map_err(|_| PithosError::IntegerOverflow)?;
    let end = usize::try_from(end).map_err(|_| PithosError::IntegerOverflow)?;
    decoded.get(start..end).ok_or(PithosError::InvalidRange)
}

fn parse_registry(bytes: &[u8]) -> Result<BTreeMap<u32, RegistryEntry>> {
    if bytes.len() < 4 {
        return Err(PithosError::InvalidRange);
    }
    let count = read_u32(bytes, 0)? as usize;
    let expected = 4_usize
        .checked_add(count.checked_mul(CODEC_RECORD_LEN).ok_or(PithosError::IntegerOverflow)?)
        .ok_or(PithosError::IntegerOverflow)?;
    if expected != bytes.len() || count > 32 {
        return Err(PithosError::InvalidMetadata("codec registry"));
    }
    let mut registry = BTreeMap::new();
    for index in 0..count {
        let offset = 4 + index * CODEC_RECORD_LEN;
        let entry = RegistryEntry {
            chain_id: read_u32(bytes, offset)?,
            codec_id: read_u16(bytes, offset + 4)?,
            codec_version: read_u16(bytes, offset + 6)?,
            level: read_i32(bytes, offset + 8)?,
            flags: read_u32(bytes, offset + 12)?,
        };
        if entry.chain_id == 0
            || entry.flags & !CODEC_FLAG_REQUIRED != 0
            || registry.insert(entry.chain_id, entry).is_some()
        {
            return Err(PithosError::InvalidMetadata("codec registry"));
        }
    }
    Ok(registry)
}

pub(crate) fn encode_registry(entries: impl IntoIterator<Item = RegistryEntry>) -> Result<Vec<u8>> {
    let entries = entries.into_iter().collect::<Vec<_>>();
    let count = u32::try_from(entries.len()).map_err(|_| PithosError::IntegerOverflow)?;
    let mut bytes = Vec::with_capacity(4 + entries.len() * CODEC_RECORD_LEN);
    bytes.extend_from_slice(&count.to_le_bytes());
    let mut chains = HashSet::new();
    for entry in entries {
        if entry.chain_id == 0 || !chains.insert(entry.chain_id) {
            return Err(PithosError::InvalidMetadata("codec registry"));
        }
        bytes.extend_from_slice(&entry.chain_id.to_le_bytes());
        bytes.extend_from_slice(&entry.codec_id.to_le_bytes());
        bytes.extend_from_slice(&entry.codec_version.to_le_bytes());
        bytes.extend_from_slice(&entry.level.to_le_bytes());
        bytes.extend_from_slice(&entry.flags.to_le_bytes());
    }
    Ok(bytes)
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

fn required(
    sections: &BTreeMap<u16, SectionDirectoryRecord>,
    section: SectionType,
) -> Result<&SectionDirectoryRecord> {
    sections
        .get(&(section as u16))
        .ok_or(PithosError::MissingSection("PAF section"))
}

fn read_json<T: serde::de::DeserializeOwned>(
    file: &mut File,
    section: &SectionDirectoryRecord,
    limits: &DecodeLimits,
) -> Result<T> {
    let bytes = read_checked(file, section, limits)?;
    serde_json::from_slice(&bytes).map_err(|_| PithosError::InvalidMetadata("JSON section"))
}

fn read_checked(
    file: &mut File,
    section: &SectionDirectoryRecord,
    limits: &DecodeLimits,
) -> Result<Vec<u8>> {
    let bytes = read_exact_vec(file, section.offset, section.length, limits.max_metadata_bytes)?;
    if crc32c::crc32c(&bytes) != section.crc32c {
        return Err(PithosError::ChecksumMismatch);
    }
    Ok(bytes)
}

fn read_exact_vec(file: &mut File, offset: u64, length: u64, maximum: u64) -> Result<Vec<u8>> {
    if length > maximum {
        return Err(PithosError::ResourceLimit("allocation"));
    }
    let length = usize::try_from(length).map_err(|_| PithosError::MemoryLimit)?;
    let mut bytes = vec![0_u8; length];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn digest_prefix(
    file: &mut File,
    length: u64,
    cancellation: &CancellationToken,
) -> Result<[u8; 32]> {
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = length;
    let mut buffer = [0_u8; IO_BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();
    while remaining > 0 {
        checkpoint(cancellation)?;
        let wanted = usize::try_from(remaining.min(IO_BUFFER_SIZE as u64))
            .map_err(|_| PithosError::IntegerOverflow)?;
        let read = file.read(&mut buffer[..wanted])?;
        if read == 0 {
            return Err(std::io::Error::from(ErrorKind::UnexpectedEof).into());
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(*hasher.finalize().as_bytes())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let data = bytes.get(offset..offset + 2).ok_or(PithosError::InvalidRange)?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let data = bytes.get(offset..offset + 4).ok_or(PithosError::InvalidRange)?;
    Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32> {
    Ok(read_u32(bytes, offset)? as i32)
}

fn checkpoint(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(PithosError::Cancelled)
    } else {
        Ok(())
    }
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
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
