use pithos_codecs::{BrotliCodec, Codec, CodecId, Lzma2Codec, StoreCodec, ZstdCodec};
use pithos_core::{DecodeLimits, PithosError, Result};
use pithos_engine_legacy::{CancellationToken, UnpackRequest};
use pithos_format::{
    CodecRegistry, EntryKind, EntryRecord, FOOTER_LEN, Footer, GlobalHeader, GroupTableRecord,
    HEADER_LEN, REQUIRED_COMPRESSED_SECTIONS, REQUIRED_RAW_SECTIONS, RestoreMapRecord,
    SECTION_ENTRY_LEN, SectionDirectoryRecord, SectionType,
};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

const IO_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug)]
struct FastCatalog {
    entries: Vec<EntryRecord>,
    groups: Vec<GroupTableRecord>,
    restore_map: Vec<RestoreMapRecord>,
    codec_registry: Option<CodecRegistry>,
}

/// Unpacks a PAF archive transactionally while decoding each solid group once.
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

    // Reuse the mature catalog validator first. It does not decode payloads.
    // The subsequent root verification protects the second parse against a
    // changed archive between validation and extraction.
    let _ = pithos_engine_legacy::list_with_control(&request.archive, limits, cancellation)?;
    let catalog = read_fast_catalog(&request.archive, limits, cancellation)?;

    let original_bytes = catalog
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::File { .. } | EntryKind::Hardlink { .. }))
        .try_fold(0_u64, |total, entry| {
            total
                .checked_add(entry.size)
                .ok_or(PithosError::IntegerOverflow)
        })?;
    if original_bytes > max_temp_bytes {
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
        let decoded = decode_group_once(
            &mut archive,
            group,
            catalog.codec_registry.as_ref(),
            limits,
            cancellation,
        )?;

        for entry in entries {
            checkpoint(cancellation)?;
            let restore = restore_by_entry
                .get(&entry.entry_id)
                .copied()
                .ok_or(PithosError::InvalidMetadata("restore entry"))?;
            if restore.group_id != group_id || restore.length != entry.size {
                return Err(PithosError::InvalidMetadata("restore map entry"));
            }
            let bytes = restored_slice(&decoded, restore)?;
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
        checkpoint(cancellation)?;
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
        checkpoint(cancellation)?;
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

fn read_fast_catalog(
    archive_path: &Path,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<FastCatalog> {
    let mut file = File::open(archive_path)?;
    let file_length = file.metadata()?.len();
    if file_length < (HEADER_LEN + FOOTER_LEN) as u64 {
        return Err(PithosError::InvalidRange);
    }

    let mut header_bytes = [0_u8; HEADER_LEN];
    file.read_exact(&mut header_bytes)?;
    let header = GlobalHeader::decode(&header_bytes)?;
    if header.section_count > limits.max_sections
        || ![REQUIRED_RAW_SECTIONS, REQUIRED_COMPRESSED_SECTIONS].contains(&header.section_count)
        || header.footer_offset >= file_length
    {
        return Err(PithosError::ResourceLimit("section count"));
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

    let root = digest_prefix(&mut file, header.footer_offset, cancellation)?;
    if root != footer.blake3_root {
        return Err(PithosError::HashMismatch);
    }

    let directory_length = u64::from(header.section_count)
        .checked_mul(SECTION_ENTRY_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    if directory_length > limits.max_metadata_bytes {
        return Err(PithosError::ResourceLimit("metadata allocation"));
    }
    let directory_bytes = read_exact_vec(
        &mut file,
        header.section_directory_offset,
        directory_length,
        limits.max_metadata_bytes,
    )?;
    if crc32c::crc32c(&directory_bytes) != footer.directory_crc32c {
        return Err(PithosError::ChecksumMismatch);
    }

    let mut sections = BTreeMap::<u16, SectionDirectoryRecord>::new();
    for chunk in directory_bytes.chunks_exact(SECTION_ENTRY_LEN) {
        let record = SectionDirectoryRecord::decode(
            chunk
                .try_into()
                .map_err(|_| PithosError::InvalidMetadata("section directory"))?,
        );
        if record.section_version != 1 || record.flags != 0 || record.reserved != 0 {
            return Err(PithosError::InvalidMetadata("section flags/version"));
        }
        if sections.insert(record.section_type, record).is_some() {
            return Err(PithosError::DuplicateSection);
        }
    }

    let entry_section = required_section(&sections, SectionType::EntryTable)?;
    let group_section = required_section(&sections, SectionType::GroupTable)?;
    let restore_section = required_section(&sections, SectionType::RestoreMap)?;
    let codec_section = sections.get(&(SectionType::CodecRegistry as u16));

    let entries: Vec<EntryRecord> = read_json_section(&mut file, entry_section, limits)?;
    let groups: Vec<GroupTableRecord> = read_json_section(&mut file, group_section, limits)?;
    let restore_map: Vec<RestoreMapRecord> = read_json_section(&mut file, restore_section, limits)?;
    let codec_registry = if let Some(section) = codec_section {
        let bytes = read_checked_section(&mut file, section, limits)?;
        Some(CodecRegistry::decode(&bytes, 32)?)
    } else {
        None
    };

    Ok(FastCatalog {
        entries,
        groups,
        restore_map,
        codec_registry,
    })
}

fn decode_group_once(
    archive: &mut File,
    record: &GroupTableRecord,
    registry: Option<&CodecRegistry>,
    limits: &DecodeLimits,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    checkpoint(cancellation)?;
    if record.group.uncompressed_len > limits.max_group_output {
        return Err(PithosError::ResourceLimit("group output"));
    }
    let maximum_compressed = limits
        .max_group_output
        .checked_add(1024 * 1024)
        .ok_or(PithosError::IntegerOverflow)?;
    let payload = read_exact_vec(
        archive,
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

    let capacity = usize::try_from(record.group.uncompressed_len)
        .map_err(|_| PithosError::MemoryLimit)?;
    let mut decoded = Vec::with_capacity(capacity);
    codec_for_id(codec_id).decode(
        &mut std::io::Cursor::new(payload),
        record.group.uncompressed_len,
        &mut decoded,
    )?;
    checkpoint(cancellation)?;
    Ok(decoded)
}

fn restored_slice<'a>(decoded: &'a [u8], restore: &RestoreMapRecord) -> Result<&'a [u8]> {
    let start = usize::try_from(restore.group_offset).map_err(|_| PithosError::IntegerOverflow)?;
    let length = usize::try_from(restore.length).map_err(|_| PithosError::IntegerOverflow)?;
    let end = start
        .checked_add(length)
        .ok_or(PithosError::IntegerOverflow)?;
    decoded.get(start..end).ok_or(PithosError::InvalidRange)
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

fn read_json_section<T: serde::de::DeserializeOwned>(
    file: &mut File,
    section: &SectionDirectoryRecord,
    limits: &DecodeLimits,
) -> Result<T> {
    let bytes = read_checked_section(file, section, limits)?;
    serde_json::from_slice(&bytes).map_err(|_| PithosError::InvalidMetadata("JSON section"))
}

fn read_checked_section(
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
        .ok_or_else(|| PithosError::MissingSection(section_name(section_type)))
}

fn section_name(section_type: SectionType) -> &'static str {
    match section_type {
        SectionType::CodecRegistry => "CodecRegistry",
        SectionType::EntryTable => "EntryTable",
        SectionType::GroupTable => "GroupTable",
        SectionType::RestoreMap => "RestoreMap",
        SectionType::CentralIndex => "CentralIndex",
        SectionType::IntegrityTree => "IntegrityTree",
        SectionType::PayloadArea => "PayloadArea",
        _ => "unsupported",
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use pithos_core::CompressionProfile;
    use pithos_engine_legacy::PackRequest;

    #[test]
    fn fast_unpack_roundtrips_a_multi_file_solid_group() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a.txt");
        let b = root.path().join("b.txt");
        fs::write(&a, b"alpha alpha alpha alpha").unwrap();
        fs::write(&b, b"beta beta beta beta").unwrap();
        let archive = root.path().join("solid.pits");
        pithos_engine_legacy::pack(PackRequest {
            inputs: vec![a.clone(), b.clone()],
            output: archive.clone(),
            profile: CompressionProfile::Balanced,
        })
        .unwrap();

        let output = root.path().join("out");
        unpack(UnpackRequest {
            archive,
            output_dir: output.clone(),
        })
        .unwrap();
        assert_eq!(fs::read(output.join("a.txt")).unwrap(), fs::read(a).unwrap());
        assert_eq!(fs::read(output.join("b.txt")).unwrap(), fs::read(b).unwrap());
    }
}
