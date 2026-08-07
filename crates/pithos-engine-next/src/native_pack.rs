use crate::adaptive_pack;
use crate::native_archive::{
    self, ArchiveCatalog, RegistryEntry, decode_group, encode_registry, read_catalog,
    read_group_payload,
};
use pithos_core::{CompressionProfile, DecodeLimits, PithosError, Result};
use pithos_engine_legacy::{CancellationToken, PackLimits, PackRequest};
use pithos_format::{
    FOOTER_LEN, Footer, GlobalHeader, GroupRecord, GroupTableRecord, HEADER_LEN,
    REQUIRED_COMPRESSED_SECTIONS, SECTION_ENTRY_LEN, SectionDirectoryRecord, SectionType,
};
use pithos_io::{atomic_commit, create_atomic_spool};
use pithos_native_codec::{
    NATIVE_CODEC_ID, NATIVE_CODEC_VERSION, NativeStats, encode_exact_dedup,
};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

const NATIVE_CHAIN_ID: u32 = 5;
const CODEC_FLAG_REQUIRED: u32 = 1;
const MIN_NATIVE_GROUP_BYTES: u64 = 1024 * 1024;
const IO_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug)]
struct GroupChoice {
    chain_id: u32,
    payload: Vec<u8>,
    native_stats: Option<NativeStats>,
}

pub fn pack(request: PackRequest) -> Result<()> {
    pack_with_control(request, &CancellationToken::new())
}

pub fn pack_with_control(request: PackRequest, cancellation: &CancellationToken) -> Result<()> {
    pack_with_limits_and_control(request, &PackLimits::default(), cancellation)
}

pub fn pack_with_limits_and_control(
    request: PackRequest,
    limits: &PackLimits,
    cancellation: &CancellationToken,
) -> Result<()> {
    if request.profile == CompressionProfile::Raw {
        return adaptive_pack::pack_with_limits_and_control(request, limits, cancellation);
    }
    checkpoint(cancellation)?;
    if path_entry_exists(&request.output)? {
        return Err(PithosError::OutputExists);
    }

    let PackRequest {
        inputs,
        output,
        profile,
    } = request;
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".pithos-native-pack-")
        .tempdir_in(parent)?;
    let standard_path = stage.path().join("standard.pits");

    adaptive_pack::pack_with_limits_and_control(
        PackRequest {
            inputs,
            output: standard_path.clone(),
            profile,
        },
        limits,
        cancellation,
    )?;
    checkpoint(cancellation)?;

    let decode_limits = DecodeLimits {
        max_entries: limits.max_entries.min(DecodeLimits::default().max_entries),
        max_original_bytes: limits.max_input_bytes,
        max_metadata_bytes: limits
            .max_metadata_bytes
            .min(DecodeLimits::default().max_metadata_bytes),
        ..DecodeLimits::default()
    };
    let catalog = read_catalog(&standard_path, &decode_limits, cancellation)?;
    let mut archive = File::open(&standard_path)?;
    let mut choices = Vec::<GroupChoice>::with_capacity(catalog.groups.len());
    let native_level = match profile {
        CompressionProfile::ArchiveMax => 15,
        CompressionProfile::Balanced => 9,
        CompressionProfile::Stream | CompressionProfile::Random => 5,
        CompressionProfile::Raw => 3,
    };

    for group in &catalog.groups {
        checkpoint(cancellation)?;
        let standard_payload = read_group_payload(&mut archive, group, &decode_limits)?;
        let mut choice = GroupChoice {
            chain_id: group.group.codec_chain_id,
            payload: standard_payload,
            native_stats: None,
        };

        if group.group.uncompressed_len >= MIN_NATIVE_GROUP_BYTES {
            let decoded = decode_group(
                &mut archive,
                group,
                &catalog.registry,
                &decode_limits,
                cancellation,
            )?;
            let member_lengths = group_member_lengths(&catalog, group.group.group_id)?;
            let (native_payload, stats) =
                encode_exact_dedup(&decoded, &member_lengths, native_level)?;
            if stats.gross_duplicate_bytes > 0 && native_payload.len() < choice.payload.len() {
                choice = GroupChoice {
                    chain_id: NATIVE_CHAIN_ID,
                    payload: native_payload,
                    native_stats: Some(stats),
                };
            }
        }
        choices.push(choice);
    }

    write_repacked_archive(
        &output,
        &standard_path,
        catalog,
        choices,
        native_level,
        limits,
        cancellation,
    )?;
    Ok(())
}

fn write_repacked_archive(
    output: &Path,
    standard_path: &Path,
    catalog: ArchiveCatalog,
    choices: Vec<GroupChoice>,
    native_level: i32,
    limits: &PackLimits,
    cancellation: &CancellationToken,
) -> Result<()> {
    if choices.len() != catalog.groups.len() {
        return Err(PithosError::InvalidMetadata("native group choices"));
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let directory_length = u64::from(REQUIRED_COMPRESSED_SECTIONS)
        .checked_mul(SECTION_ENTRY_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let payload_start = (HEADER_LEN as u64)
        .checked_add(directory_length)
        .ok_or(PithosError::IntegerOverflow)?;
    let mut spool = create_atomic_spool(parent)?;
    spool.seek(SeekFrom::Start(payload_start))?;

    let mut groups = Vec::<GroupTableRecord>::with_capacity(choices.len());
    let mut used_chains = HashSet::<u32>::new();
    let mut payload_crc = 0_u32;
    let mut native_selected = false;
    for (index, (old, choice)) in catalog.groups.iter().zip(choices.iter()).enumerate() {
        checkpoint(cancellation)?;
        let payload_offset = spool.stream_position()?;
        spool.write_all(&choice.payload)?;
        let crc = crc32c::crc32c(&choice.payload);
        payload_crc = crc32c::crc32c_append(payload_crc, &choice.payload);
        used_chains.insert(choice.chain_id);
        native_selected |= choice.native_stats.is_some();
        groups.push(GroupTableRecord {
            group: GroupRecord {
                version: 1,
                flags: 0,
                group_id: index as u64,
                codec_chain_id: choice.chain_id,
                chunk_count: old.group.chunk_count,
                uncompressed_len: old.group.uncompressed_len,
                compressed_len: choice.payload.len() as u64,
                descriptor_len: 0,
                payload_crc32c: crc,
            },
            payload_offset,
        });
    }
    let payload_end = spool.stream_position()?;
    let payload_length = payload_end
        .checked_sub(payload_start)
        .ok_or(PithosError::IntegerOverflow)?;

    let mut registry = BTreeMap::<u32, RegistryEntry>::new();
    for (chain_id, entry) in &catalog.registry {
        if used_chains.contains(chain_id) {
            registry.insert(*chain_id, *entry);
        }
    }
    if native_selected {
        registry.insert(
            NATIVE_CHAIN_ID,
            RegistryEntry {
                chain_id: NATIVE_CHAIN_ID,
                codec_id: NATIVE_CODEC_ID,
                codec_version: NATIVE_CODEC_VERSION,
                level: native_level,
                flags: CODEC_FLAG_REQUIRED,
            },
        );
    }
    let registry_bytes = encode_registry(registry.into_values())?;
    let entry_bytes = serialize(&catalog.entries)?;
    let group_bytes = serialize(&groups)?;
    let restore_bytes = serialize(&catalog.restore_map)?;
    let index_bytes = serialize(&catalog.central_index)?;
    let integrity_bytes = serialize(&catalog.integrity)?;
    let metadata_sections = [
        &registry_bytes,
        &entry_bytes,
        &group_bytes,
        &restore_bytes,
        &index_bytes,
        &integrity_bytes,
    ];
    let metadata_total = metadata_sections.iter().try_fold(0_u64, |total, bytes| {
        total
            .checked_add(bytes.len() as u64)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    if metadata_total > limits.max_metadata_bytes {
        return Err(PithosError::ResourceLimit("aggregate metadata"));
    }

    let expected_length = payload_end
        .checked_add(metadata_total)
        .and_then(|value| value.checked_add(FOOTER_LEN as u64))
        .ok_or(PithosError::IntegerOverflow)?;
    if expected_length > limits.max_output_bytes {
        return Err(PithosError::ResourceLimit("archive output"));
    }
    let standard_bytes = fs::metadata(standard_path)?.len();
    if standard_bytes
        .checked_add(expected_length)
        .is_none_or(|value| value > limits.max_temp_bytes)
    {
        return Err(PithosError::TemporarySpaceLimit);
    }

    let mut sections = Vec::with_capacity(REQUIRED_COMPRESSED_SECTIONS as usize);
    sections.push(write_section(&mut spool, SectionType::CodecRegistry, &registry_bytes)?);
    sections.push(write_section(&mut spool, SectionType::EntryTable, &entry_bytes)?);
    sections.push(write_section(&mut spool, SectionType::GroupTable, &group_bytes)?);
    sections.push(SectionDirectoryRecord {
        section_type: SectionType::PayloadArea as u16,
        section_version: 1,
        flags: 0,
        offset: payload_start,
        length: payload_length,
        crc32c: payload_crc,
        reserved: 0,
    });
    sections.push(write_section(&mut spool, SectionType::RestoreMap, &restore_bytes)?);
    sections.push(write_section(&mut spool, SectionType::CentralIndex, &index_bytes)?);
    sections.push(write_section(&mut spool, SectionType::IntegrityTree, &integrity_bytes)?);
    sections.sort_by_key(|section| section.section_type);

    let footer_offset = spool.stream_position()?;
    let mut identity_hasher = blake3::Hasher::new();
    for bytes in metadata_sections {
        identity_hasher.update(bytes);
    }
    let identity = identity_hasher.finalize();
    let mut archive_id = [0_u8; 16];
    archive_id.copy_from_slice(&identity.as_bytes()[..16]);
    let mut header = GlobalHeader::new(archive_id);
    header.original_total_size = catalog.header.original_total_size;
    header.entry_count = catalog.entries.len() as u64;
    header.logical_chunk_count = catalog.restore_map.len() as u64;
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
    let root = digest_prefix(spool.as_file_mut(), footer_offset, cancellation)?;
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

    native_archive::verify_with_limits(spool.path(), &DecodeLimits::default())?;
    checkpoint(cancellation)?;
    atomic_commit(spool, output)?;
    Ok(())
}

fn group_member_lengths(catalog: &ArchiveCatalog, group_id: u64) -> Result<Vec<u64>> {
    let mut records = catalog
        .restore_map
        .iter()
        .filter(|record| record.group_id == group_id)
        .collect::<Vec<_>>();
    records.sort_by_key(|record| record.group_offset);
    let mut expected_offset = 0_u64;
    let mut lengths = Vec::with_capacity(records.len());
    for record in records {
        if record.group_offset != expected_offset {
            return Err(PithosError::InvalidMetadata("group restore layout"));
        }
        lengths.push(record.length);
        expected_offset = expected_offset
            .checked_add(record.length)
            .ok_or(PithosError::IntegerOverflow)?;
    }
    let group = catalog
        .groups
        .get(group_id as usize)
        .ok_or(PithosError::InvalidMetadata("entry group"))?;
    if expected_offset != group.group.uncompressed_len {
        return Err(PithosError::InvalidMetadata("group restore length"));
    }
    Ok(lengths)
}

fn serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|_| PithosError::InvalidMetadata("serialization"))
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
