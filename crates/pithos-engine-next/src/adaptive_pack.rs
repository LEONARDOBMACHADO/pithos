use pithos_codecs::{BrotliCodec, Codec, CodecConfig, CodecId, Lzma2Codec, StoreCodec, ZstdCodec};
use pithos_core::{CompressionProfile, DecodeLimits, PithosError, Result};
use pithos_engine_legacy::{CancellationToken, PackLimits, PackRequest};
use pithos_format::{
    CODEC_FLAG_REQUIRED, CentralIndexRecord, CodecRegistry, CodecRegistryRecord, EntryKind,
    EntryRecord, FOOTER_LEN, Footer, GlobalHeader, GroupRecord, GroupTableRecord, HEADER_LEN,
    IntegrityRecord, REQUIRED_COMPRESSED_SECTIONS, RestoreMapRecord, SECTION_ENTRY_LEN,
    SectionDirectoryRecord, SectionType,
};
use pithos_io::{atomic_commit, create_atomic_spool};
use pithos_planner::{SolidGroupPlan, plan_solid_groups};
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SAMPLE_BYTES: usize = 128 * 1024;
const IO_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug)]
struct RawCatalog {
    header: GlobalHeader,
    entries: Vec<EntryRecord>,
    groups: Vec<GroupTableRecord>,
}

#[derive(Debug)]
struct SourceData {
    entry_id: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    codec: CodecId,
    level: i32,
}

#[derive(Debug)]
struct EncodedGroup {
    codec: CodecId,
    level: i32,
    payload: Vec<u8>,
}

/// Adaptive pack route for all compressed profiles. RAW remains delegated to
/// the compatibility engine so the stable baseline format/path scanner stays
/// the single source of truth.
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
        return pithos_engine_legacy::pack_with_limits_and_control(request, limits, cancellation);
    }
    checkpoint(cancellation)?;
    if request.inputs.is_empty() {
        return Err(PithosError::InvalidMetadata("nenhuma entrada"));
    }
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

    // Stage through RAW. This deliberately reuses the already hardened scanner,
    // path/link handling and input-change checks instead of duplicating them in
    // the experimental compression path.
    let staging_dir = tempfile::Builder::new()
        .prefix(".pithos-adaptive-pack-")
        .tempdir_in(parent)?;
    let raw_path = staging_dir.path().join("source.raw.pits");
    pithos_engine_legacy::pack_with_limits_and_control(
        PackRequest {
            inputs,
            output: raw_path.clone(),
            profile: CompressionProfile::Raw,
        },
        limits,
        cancellation,
    )?;
    checkpoint(cancellation)?;

    let catalog = read_raw_catalog(&raw_path, cancellation)?;
    let mut raw_file = File::open(&raw_path)?;
    let mut sources = Vec::<SourceData>::new();
    for entry in &catalog.entries {
        let EntryKind::File { group_id } = entry.kind else {
            continue;
        };
        let group = catalog
            .groups
            .get(group_id as usize)
            .ok_or(PithosError::InvalidMetadata("entry group"))?;
        if group.group.codec_chain_id != 0
            || group.group.uncompressed_len != entry.size
            || group.group.compressed_len != entry.size
        {
            return Err(PithosError::InvalidMetadata("RAW staging group"));
        }
        let bytes = read_exact_vec(
            &mut raw_file,
            group.payload_offset,
            group.group.compressed_len,
            limits.max_input_bytes,
        )?;
        if crc32c::crc32c(&bytes) != group.group.payload_crc32c
            || blake3::hash(&bytes).as_bytes() != &entry.blake3
        {
            return Err(PithosError::HashMismatch);
        }
        sources.push(SourceData {
            entry_id: entry.entry_id,
            bytes,
        });
    }

    let lengths = sources
        .iter()
        .map(|source| source.bytes.len() as u64)
        .collect::<Vec<_>>();
    let plans = plan_solid_groups(profile, &lengths)?;
    let mut entries = catalog.entries;
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
                length: source.bytes.len() as u64,
                group_id: group_id as u64,
                group_offset,
            });
            group_offset = group_offset
                .checked_add(source.bytes.len() as u64)
                .ok_or(PithosError::IntegerOverflow)?;
        }
        if group_offset != plan.uncompressed_len {
            return Err(PithosError::InvalidMetadata("solid group plan length"));
        }
    }

    let decode_defaults = DecodeLimits::default();
    let decode_limits = DecodeLimits {
        max_entries: limits.max_entries.min(decode_defaults.max_entries),
        max_groups: limits.max_entries.min(decode_defaults.max_groups),
        max_chunks: limits.max_entries.min(decode_defaults.max_chunks),
        max_original_bytes: limits.max_input_bytes,
        max_metadata_bytes: limits.max_metadata_bytes.min(decode_defaults.max_metadata_bytes),
        ..decode_defaults
    };

    let directory_length = u64::from(REQUIRED_COMPRESSED_SECTIONS)
        .checked_mul(SECTION_ENTRY_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let payload_start = (HEADER_LEN as u64)
        .checked_add(directory_length)
        .ok_or(PithosError::IntegerOverflow)?;
    let mut spool = create_atomic_spool(parent)?;
    spool.seek(SeekFrom::Start(payload_start))?;

    let mut groups = Vec::with_capacity(plans.len());
    let mut registry_levels = BTreeMap::<u16, i32>::new();
    let mut payload_crc = 0_u32;
    for (group_id, plan) in plans.iter().enumerate() {
        checkpoint(cancellation)?;
        if plan.uncompressed_len > limits.max_memory_bytes {
            return Err(PithosError::MemoryLimit);
        }
        let members = group_members(&sources, plan)?;
        let capacity = usize::try_from(plan.uncompressed_len).map_err(|_| PithosError::MemoryLimit)?;
        let mut input = Vec::with_capacity(capacity);
        for source in members {
            input.extend_from_slice(&source.bytes);
        }

        let encoded = encode_adaptively(&input, profile, cancellation)?;
        let payload_offset = spool.stream_position()?;
        spool.write_all(&encoded.payload)?;
        let group_crc = crc32c::crc32c(&encoded.payload);
        payload_crc = crc32c::crc32c_append(payload_crc, &encoded.payload);
        let compressed_len = encoded.payload.len() as u64;
        groups.push(GroupTableRecord {
            group: GroupRecord {
                version: 1,
                flags: 0,
                group_id: group_id as u64,
                codec_chain_id: u32::from(encoded.codec as u16) + 1,
                chunk_count: u32::try_from(plan.item_count)
                    .map_err(|_| PithosError::IntegerOverflow)?,
                uncompressed_len: plan.uncompressed_len,
                compressed_len,
                descriptor_len: 0,
                payload_crc32c: group_crc,
            },
            payload_offset,
        });
        let raw_id = encoded.codec as u16;
        if let Some(previous) = registry_levels.insert(raw_id, encoded.level) {
            if previous != encoded.level {
                return Err(PithosError::InvalidMetadata("codec level instability"));
            }
        }
    }
    let payload_end = spool.stream_position()?;
    let payload_length = payload_end
        .checked_sub(payload_start)
        .ok_or(PithosError::IntegerOverflow)?;

    let registry = CodecRegistry {
        records: registry_levels
            .into_iter()
            .map(|(codec_id, level)| {
                let id = CodecId::from_u16(codec_id).ok_or(PithosError::UnsupportedCodec)?;
                Ok(CodecRegistryRecord {
                    chain_id: u32::from(codec_id) + 1,
                    codec_id,
                    codec_version: codec_for_id(id).version(),
                    level,
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
        let length = bytes.len() as u64;
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
        .and_then(|value| value.checked_add(FOOTER_LEN as u64))
        .ok_or(PithosError::IntegerOverflow)?;
    if expected_archive_length > limits.max_output_bytes {
        return Err(PithosError::ResourceLimit("archive output"));
    }
    let raw_bytes = fs::metadata(&raw_path)?.len();
    if raw_bytes
        .checked_add(expected_archive_length)
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
    let identity_hash = identity_hasher.finalize();
    let mut archive_id = [0_u8; 16];
    archive_id.copy_from_slice(&identity_hash.as_bytes()[..16]);
    let mut header = GlobalHeader::new(archive_id);
    header.original_total_size = catalog.header.original_total_size;
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

    // Validate the not-yet-published temp archive with the mature reader.
    pithos_engine_legacy::verify_with_limits(spool.path(), &decode_limits)?;
    checkpoint(cancellation)?;
    atomic_commit(spool, &output)?;
    Ok(())
}

fn encode_adaptively(
    input: &[u8],
    profile: CompressionProfile,
    cancellation: &CancellationToken,
) -> Result<EncodedGroup> {
    let candidates = profile_candidates(profile);
    if candidates.len() == 1 {
        return encode_full(input, candidates[0]);
    }

    let sample = deterministic_sample(input);
    let mut probes = Vec::<(Candidate, usize)>::with_capacity(candidates.len());
    for candidate in &candidates {
        checkpoint(cancellation)?;
        let mut encoded = Vec::new();
        codec_for_id(candidate.codec).encode(
            &sample,
            &CodecConfig {
                level: candidate.level,
            },
            &mut encoded,
        )?;
        probes.push((*candidate, encoded.len()));
    }
    probes.sort_by_key(|(candidate, bytes)| (*bytes, speed_rank(candidate.codec)));
    let best_bytes = probes[0].1;

    // Avoid paying a large full-input latency cost for a statistically tiny
    // sample win. archive-max uses a tighter tolerance than balanced.
    let tolerance = match profile {
        CompressionProfile::ArchiveMax => 1.003,
        CompressionProfile::Balanced => 1.01,
        _ => 1.02,
    };
    let mut eligible = probes
        .iter()
        .filter(|(_, bytes)| (*bytes as f64) <= (best_bytes as f64 * tolerance))
        .map(|(candidate, _)| *candidate)
        .collect::<Vec<_>>();
    eligible.sort_by_key(|candidate| speed_rank(candidate.codec));
    let selected = eligible[0];

    // If every compressor expands an already-compressed/random sample, STORE
    // wins without processing the full group through a heavyweight codec.
    let selected_probe = probes
        .iter()
        .find(|(candidate, _)| candidate.codec == selected.codec)
        .map(|(_, bytes)| *bytes)
        .ok_or(PithosError::InvalidMetadata("adaptive codec selection"))?;
    if selected.codec != CodecId::Store && selected_probe >= sample.len() {
        return encode_full(
            input,
            Candidate {
                codec: CodecId::Store,
                level: 0,
            },
        );
    }
    encode_full(input, selected)
}

fn profile_candidates(profile: CompressionProfile) -> Vec<Candidate> {
    match profile {
        CompressionProfile::Raw => vec![Candidate {
            codec: CodecId::Store,
            level: 0,
        }],
        CompressionProfile::Stream | CompressionProfile::Random => vec![
            Candidate {
                codec: CodecId::Store,
                level: 0,
            },
            Candidate {
                codec: CodecId::Zstd,
                level: 5,
            },
        ],
        CompressionProfile::Balanced => vec![
            Candidate {
                codec: CodecId::Store,
                level: 0,
            },
            Candidate {
                codec: CodecId::Zstd,
                level: 9,
            },
            Candidate {
                codec: CodecId::Brotli,
                level: 8,
            },
            Candidate {
                codec: CodecId::Lzma2,
                level: 6,
            },
        ],
        CompressionProfile::ArchiveMax => vec![
            Candidate {
                codec: CodecId::Store,
                level: 0,
            },
            Candidate {
                codec: CodecId::Zstd,
                level: 19,
            },
            Candidate {
                codec: CodecId::Brotli,
                level: 11,
            },
            Candidate {
                codec: CodecId::Lzma2,
                level: 9,
            },
        ],
    }
}

fn encode_full(input: &[u8], candidate: Candidate) -> Result<EncodedGroup> {
    let mut payload = Vec::new();
    codec_for_id(candidate.codec).encode(
        input,
        &CodecConfig {
            level: candidate.level,
        },
        &mut payload,
    )?;
    Ok(EncodedGroup {
        codec: candidate.codec,
        level: candidate.level,
        payload,
    })
}

fn deterministic_sample(input: &[u8]) -> Vec<u8> {
    if input.len() <= SAMPLE_BYTES * 3 {
        return input.to_vec();
    }
    let mut sample = Vec::with_capacity(SAMPLE_BYTES * 3);
    sample.extend_from_slice(&input[..SAMPLE_BYTES]);
    let middle = input.len() / 2;
    let middle_start = middle.saturating_sub(SAMPLE_BYTES / 2);
    sample.extend_from_slice(&input[middle_start..middle_start + SAMPLE_BYTES]);
    sample.extend_from_slice(&input[input.len() - SAMPLE_BYTES..]);
    sample
}

fn speed_rank(codec: CodecId) -> u8 {
    match codec {
        CodecId::Store => 0,
        CodecId::Zstd => 1,
        CodecId::Brotli => 2,
        CodecId::Lzma2 => 3,
    }
}

fn group_members<'a>(sources: &'a [SourceData], plan: &SolidGroupPlan) -> Result<&'a [SourceData]> {
    let end = plan
        .first_item
        .checked_add(plan.item_count)
        .ok_or(PithosError::IntegerOverflow)?;
    sources
        .get(plan.first_item..end)
        .ok_or(PithosError::InvalidMetadata("solid group member range"))
}

fn read_raw_catalog(path: &Path, cancellation: &CancellationToken) -> Result<RawCatalog> {
    let mut file = File::open(path)?;
    let file_length = file.metadata()?.len();
    let mut header_bytes = [0_u8; HEADER_LEN];
    file.read_exact(&mut header_bytes)?;
    let header = GlobalHeader::decode(&header_bytes)?;
    if header.footer_offset >= file_length {
        return Err(PithosError::InvalidRange);
    }
    file.seek(SeekFrom::Start(header.footer_offset))?;
    let mut footer_bytes = [0_u8; FOOTER_LEN];
    file.read_exact(&mut footer_bytes)?;
    let footer = Footer::decode(&footer_bytes)?;
    if footer.archive_length != file_length || footer.version != 1 {
        return Err(PithosError::InvalidMetadata("footer"));
    }
    if digest_prefix(&mut file, header.footer_offset, cancellation)? != footer.blake3_root {
        return Err(PithosError::HashMismatch);
    }

    let directory_length = u64::from(header.section_count)
        .checked_mul(SECTION_ENTRY_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let directory = read_exact_vec(
        &mut file,
        header.section_directory_offset,
        directory_length,
        256 * 1024 * 1024,
    )?;
    if crc32c::crc32c(&directory) != footer.directory_crc32c {
        return Err(PithosError::ChecksumMismatch);
    }
    let mut sections = HashMap::<u16, SectionDirectoryRecord>::new();
    for bytes in directory.chunks_exact(SECTION_ENTRY_LEN) {
        let record = SectionDirectoryRecord::decode(
            bytes
                .try_into()
                .map_err(|_| PithosError::InvalidMetadata("section directory"))?,
        );
        sections.insert(record.section_type, record);
    }
    let entry_section = sections
        .get(&(SectionType::EntryTable as u16))
        .ok_or(PithosError::MissingSection("EntryTable"))?;
    let group_section = sections
        .get(&(SectionType::GroupTable as u16))
        .ok_or(PithosError::MissingSection("GroupTable"))?;
    let entries: Vec<EntryRecord> = read_json_section(&mut file, entry_section)?;
    let groups: Vec<GroupTableRecord> = read_json_section(&mut file, group_section)?;
    Ok(RawCatalog {
        header,
        entries,
        groups,
    })
}

fn serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|_| PithosError::InvalidMetadata("serialization"))
}

fn read_json_section<T: DeserializeOwned>(
    file: &mut File,
    section: &SectionDirectoryRecord,
) -> Result<T> {
    let bytes = read_exact_vec(file, section.offset, section.length, 256 * 1024 * 1024)?;
    if crc32c::crc32c(&bytes) != section.crc32c {
        return Err(PithosError::ChecksumMismatch);
    }
    serde_json::from_slice(&bytes).map_err(|_| PithosError::InvalidMetadata("JSON section"))
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

fn codec_for_id(codec: CodecId) -> &'static dyn Codec {
    static STORE: StoreCodec = StoreCodec;
    static ZSTD: ZstdCodec = ZstdCodec;
    static BROTLI: BrotliCodec = BrotliCodec;
    static LZMA2: Lzma2Codec = Lzma2Codec;
    match codec {
        CodecId::Store => &STORE,
        CodecId::Zstd => &ZSTD,
        CodecId::Brotli => &BROTLI,
        CodecId::Lzma2 => &LZMA2,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_sample_covers_start_middle_and_end() {
        let input = (0..(SAMPLE_BYTES * 4))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let sample = deterministic_sample(&input);
        assert_eq!(sample.len(), SAMPLE_BYTES * 3);
        assert_eq!(&sample[..SAMPLE_BYTES], &input[..SAMPLE_BYTES]);
        assert_eq!(
            &sample[sample.len() - SAMPLE_BYTES..],
            &input[input.len() - SAMPLE_BYTES..]
        );
    }

    #[test]
    fn archive_max_candidates_are_actual_max_configs() {
        let candidates = profile_candidates(CompressionProfile::ArchiveMax);
        assert!(candidates.iter().any(|c| c.codec == CodecId::Zstd && c.level == 19));
        assert!(candidates.iter().any(|c| c.codec == CodecId::Brotli && c.level == 11));
        assert!(candidates.iter().any(|c| c.codec == CodecId::Lzma2 && c.level == 9));
    }
}
