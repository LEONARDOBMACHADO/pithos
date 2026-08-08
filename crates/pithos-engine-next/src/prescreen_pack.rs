use crate::native_archive::{self, RegistryEntry, encode_registry, read_catalog};
use pithos_codecs::{BrotliCodec, Codec, CodecConfig, CodecId, Lzma2Codec, StoreCodec, ZstdCodec};
use pithos_core::{CompressionProfile, DecodeLimits, PithosError, Result};
use pithos_engine_legacy::{CancellationToken, PackLimits, PackRequest};
use pithos_format::{
    CODEC_FLAG_REQUIRED, CentralIndexRecord, EntryKind, FOOTER_LEN, Footer, GlobalHeader,
    GroupRecord, GroupTableRecord, HEADER_LEN, IntegrityRecord, REQUIRED_COMPRESSED_SECTIONS,
    RestoreMapRecord, SECTION_ENTRY_LEN, SectionDirectoryRecord, SectionType,
};
use pithos_io::{atomic_commit, create_atomic_spool};
use pithos_native_codec::{NATIVE_CODEC_ID, NATIVE_CODEC_VERSION, encode_exact_dedup};
use pithos_planner::{SolidGroupPlan, plan_solid_groups};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

const SAMPLE_BYTES: usize = 128 * 1024;
const NATIVE_SAMPLE_MEMBER_BYTES: usize = 96 * 1024;
const MAX_NATIVE_SAMPLE_BYTES: usize = 3 * 1024 * 1024;
const NATIVE_CHAIN_ID: u32 = 5;
const MIN_NATIVE_GROUP_BYTES: usize = 1024 * 1024;
const IO_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug)]
struct SourceData {
    entry_id: u64,
    bytes: Vec<u8>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    codec: CodecId,
    level: i32,
}
#[derive(Debug)]
struct EncodedChoice {
    chain_id: u32,
    codec_id: u16,
    codec_version: u16,
    level: i32,
    payload: Vec<u8>,
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
    let stage = tempfile::Builder::new()
        .prefix(".pithos-prescreen-pack-")
        .tempdir_in(parent)?;
    let raw_path = stage.path().join("source.raw.pits");
    pithos_engine_legacy::pack_with_limits_and_control(
        PackRequest {
            inputs,
            output: raw_path.clone(),
            profile: CompressionProfile::Raw,
        },
        limits,
        cancellation,
    )?;

    let decode_limits = pack_decode_limits(limits);
    let raw_catalog = read_catalog(&raw_path, &decode_limits, cancellation)?;
    let mut raw_file = File::open(&raw_path)?;
    let mut sources = Vec::<SourceData>::new();
    for entry in &raw_catalog.entries {
        let EntryKind::File { group_id } = entry.kind else {
            continue;
        };
        let group = raw_catalog
            .groups
            .get(group_id as usize)
            .ok_or(PithosError::InvalidMetadata("RAW entry group"))?;
        let bytes = native_archive::decode_group(
            &mut raw_file,
            group,
            &raw_catalog.registry,
            &decode_limits,
            cancellation,
        )?;
        if bytes.len() as u64 != entry.size || blake3::hash(&bytes).as_bytes() != &entry.blake3 {
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
    let mut entries = raw_catalog.entries;
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

    let directory_length = u64::from(REQUIRED_COMPRESSED_SECTIONS)
        .checked_mul(SECTION_ENTRY_LEN as u64)
        .ok_or(PithosError::IntegerOverflow)?;
    let payload_start = (HEADER_LEN as u64)
        .checked_add(directory_length)
        .ok_or(PithosError::IntegerOverflow)?;
    let mut spool = create_atomic_spool(parent)?;
    spool.seek(SeekFrom::Start(payload_start))?;
    let mut groups = Vec::with_capacity(plans.len());
    let mut registry = BTreeMap::<u32, RegistryEntry>::new();
    let mut payload_crc = 0_u32;
    for (group_id, plan) in plans.iter().enumerate() {
        checkpoint(cancellation)?;
        if plan.uncompressed_len > limits.max_memory_bytes {
            return Err(PithosError::MemoryLimit);
        }
        let members = group_members(&sources, plan)?;
        let capacity =
            usize::try_from(plan.uncompressed_len).map_err(|_| PithosError::MemoryLimit)?;
        let mut input = Vec::with_capacity(capacity);
        let mut member_lengths = Vec::with_capacity(members.len());
        for source in members {
            member_lengths.push(source.bytes.len() as u64);
            input.extend_from_slice(&source.bytes);
        }
        let allow_parallel = std::thread::available_parallelism()
            .map(|value| value.get() > 1)
            .unwrap_or(false)
            && (input.len() as u64)
                .checked_mul(5)
                .is_some_and(|peak| peak <= limits.max_memory_bytes);
        let choice = choose_with_prescreen(
            &input,
            &member_lengths,
            profile,
            cancellation,
            allow_parallel,
        )?;
        let payload_offset = spool.stream_position()?;
        spool.write_all(&choice.payload)?;
        let crc = crc32c::crc32c(&choice.payload);
        payload_crc = crc32c::crc32c_append(payload_crc, &choice.payload);
        groups.push(GroupTableRecord {
            group: GroupRecord {
                version: 1,
                flags: 0,
                group_id: group_id as u64,
                codec_chain_id: choice.chain_id,
                chunk_count: u32::try_from(plan.item_count)
                    .map_err(|_| PithosError::IntegerOverflow)?,
                uncompressed_len: plan.uncompressed_len,
                compressed_len: choice.payload.len() as u64,
                descriptor_len: 0,
                payload_crc32c: crc,
            },
            payload_offset,
        });
        registry.entry(choice.chain_id).or_insert(RegistryEntry {
            chain_id: choice.chain_id,
            codec_id: choice.codec_id,
            codec_version: choice.codec_version,
            level: choice.level,
            flags: CODEC_FLAG_REQUIRED,
        });
    }
    let payload_end = spool.stream_position()?;
    let payload_length = payload_end
        .checked_sub(payload_start)
        .ok_or(PithosError::IntegerOverflow)?;
    let registry_bytes = encode_registry(registry.into_values())?;
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
        total
            .checked_add(bytes.len() as u64)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    if metadata_total > limits.max_metadata_bytes {
        return Err(PithosError::ResourceLimit("aggregate metadata"));
    }
    let expected_archive_length = payload_end
        .checked_add(metadata_total)
        .and_then(|length| length.checked_add(FOOTER_LEN as u64))
        .ok_or(PithosError::IntegerOverflow)?;
    if expected_archive_length > limits.max_output_bytes {
        return Err(PithosError::ResourceLimit("archive output"));
    }
    let raw_bytes = fs::metadata(&raw_path)?.len();
    if raw_bytes
        .checked_add(expected_archive_length)
        .is_none_or(|peak| peak > limits.max_temp_bytes)
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
    let identity = identity_hasher.finalize();
    let mut archive_id = [0_u8; 16];
    archive_id.copy_from_slice(&identity.as_bytes()[..16]);
    let mut header = GlobalHeader::new(archive_id);
    header.original_total_size = raw_catalog.header.original_total_size;
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
    native_archive::verify_with_limits(spool.path(), &decode_limits)?;
    checkpoint(cancellation)?;
    atomic_commit(spool, &output)?;
    Ok(())
}

fn choose_with_prescreen(
    input: &[u8],
    member_lengths: &[u64],
    profile: CompressionProfile,
    cancellation: &CancellationToken,
    allow_parallel: bool,
) -> Result<EncodedChoice> {
    if input.len() < MIN_NATIVE_GROUP_BYTES {
        let (candidate, _) =
            select_standard_candidate(&deterministic_sample(input), profile, cancellation)?;
        return encode_standard_full(input, candidate);
    }
    let standard_sample = deterministic_sample(input);
    let (standard_candidate, standard_probe_bytes) =
        select_standard_candidate(&standard_sample, profile, cancellation)?;
    let (native_sample, native_sample_lengths) =
        deterministic_member_sample(input, member_lengths)?;
    let native_probe_bytes = if native_sample.is_empty() {
        usize::MAX
    } else {
        encode_exact_dedup(&native_sample, &native_sample_lengths, 3)?
            .0
            .len()
    };
    if native_probe_bytes.saturating_mul(100) <= standard_probe_bytes.saturating_mul(95) {
        return encode_native_full(input, member_lengths, profile);
    }
    if standard_probe_bytes.saturating_mul(100) <= native_probe_bytes.saturating_mul(88) {
        return encode_standard_full(input, standard_candidate);
    }

    if allow_parallel {
        let (standard_result, native_result) = std::thread::scope(|scope| {
            let standard_handle = scope.spawn(|| encode_standard_full(input, standard_candidate));
            let native_handle = scope.spawn(|| encode_native_full(input, member_lengths, profile));
            (standard_handle.join(), native_handle.join())
        });
        checkpoint(cancellation)?;
        let standard = standard_result
            .map_err(|_| PithosError::InvalidMetadata("standard candidate worker panic"))??;
        let native = native_result
            .map_err(|_| PithosError::InvalidMetadata("native candidate worker panic"))??;
        return Ok(smaller_choice(standard, native));
    }
    let standard = encode_standard_full(input, standard_candidate)?;
    checkpoint(cancellation)?;
    let native = encode_native_full(input, member_lengths, profile)?;
    Ok(smaller_choice(standard, native))
}

fn smaller_choice(left: EncodedChoice, right: EncodedChoice) -> EncodedChoice {
    if right.payload.len() < left.payload.len() {
        right
    } else {
        left
    }
}
fn encode_native_full(
    input: &[u8],
    member_lengths: &[u64],
    profile: CompressionProfile,
) -> Result<EncodedChoice> {
    let level = native_level(profile);
    let (payload, _) = encode_exact_dedup(input, member_lengths, level)?;
    Ok(EncodedChoice {
        chain_id: NATIVE_CHAIN_ID,
        codec_id: NATIVE_CODEC_ID,
        codec_version: NATIVE_CODEC_VERSION,
        level,
        payload,
    })
}
fn select_standard_candidate(
    sample: &[u8],
    profile: CompressionProfile,
    cancellation: &CancellationToken,
) -> Result<(Candidate, usize)> {
    let candidates = profile_candidates(profile);
    let mut probes = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        checkpoint(cancellation)?;
        let mut output = Vec::new();
        codec_for_id(candidate.codec).encode(
            sample,
            &CodecConfig {
                level: candidate.level,
            },
            &mut output,
        )?;
        probes.push((*candidate, output.len()));
    }
    probes.sort_by_key(|(candidate, bytes)| (*bytes, speed_rank(candidate.codec)));
    let best_bytes = probes[0].1;
    let tolerance = match profile {
        CompressionProfile::ArchiveMax => 1.003,
        CompressionProfile::Balanced => 1.01,
        _ => 1.02,
    };
    let mut eligible = probes
        .iter()
        .filter(|(_, bytes)| (*bytes as f64) <= best_bytes as f64 * tolerance)
        .map(|(candidate, bytes)| (*candidate, *bytes))
        .collect::<Vec<_>>();
    eligible.sort_by_key(|(candidate, _)| speed_rank(candidate.codec));
    let (mut selected, mut selected_bytes) = eligible[0];
    if selected.codec != CodecId::Store && selected_bytes >= sample.len() {
        selected = Candidate {
            codec: CodecId::Store,
            level: 0,
        };
        selected_bytes = sample.len();
    }
    Ok((selected, selected_bytes))
}
fn encode_standard_full(input: &[u8], candidate: Candidate) -> Result<EncodedChoice> {
    let mut payload = Vec::new();
    codec_for_id(candidate.codec).encode(
        input,
        &CodecConfig {
            level: candidate.level,
        },
        &mut payload,
    )?;
    Ok(EncodedChoice {
        chain_id: u32::from(candidate.codec as u16) + 1,
        codec_id: candidate.codec as u16,
        codec_version: codec_for_id(candidate.codec).version(),
        level: candidate.level,
        payload,
    })
}
fn deterministic_member_sample(
    input: &[u8],
    member_lengths: &[u64],
) -> Result<(Vec<u8>, Vec<u64>)> {
    let mut output = Vec::new();
    let mut lengths = Vec::new();
    let mut offset = 0usize;
    for length in member_lengths {
        let length = usize::try_from(*length).map_err(|_| PithosError::IntegerOverflow)?;
        let end = offset
            .checked_add(length)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(offset..end).ok_or(PithosError::InvalidRange)?;
        if output.len() >= MAX_NATIVE_SAMPLE_BYTES {
            offset = end;
            continue;
        }
        let wanted = member
            .len()
            .min(NATIVE_SAMPLE_MEMBER_BYTES)
            .min(MAX_NATIVE_SAMPLE_BYTES - output.len());
        if wanted == 0 {
            offset = end;
            continue;
        }
        if member.len() <= wanted {
            output.extend_from_slice(member);
        } else {
            let first = wanted / 3;
            let middle = wanted / 3;
            let last = wanted - first - middle;
            output.extend_from_slice(&member[..first]);
            let middle_start = member.len() / 2 - middle / 2;
            output.extend_from_slice(&member[middle_start..middle_start + middle]);
            output.extend_from_slice(&member[member.len() - last..]);
        }
        lengths.push(wanted as u64);
        offset = end;
    }
    Ok((output, lengths))
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
fn native_level(profile: CompressionProfile) -> i32 {
    match profile {
        CompressionProfile::ArchiveMax => 15,
        CompressionProfile::Balanced => 9,
        CompressionProfile::Stream | CompressionProfile::Random => 5,
        CompressionProfile::Raw => 3,
    }
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
fn group_members<'a>(sources: &'a [SourceData], plan: &SolidGroupPlan) -> Result<&'a [SourceData]> {
    let end = plan
        .first_item
        .checked_add(plan.item_count)
        .ok_or(PithosError::IntegerOverflow)?;
    sources
        .get(plan.first_item..end)
        .ok_or(PithosError::InvalidMetadata("solid group member range"))
}
fn pack_decode_limits(limits: &PackLimits) -> DecodeLimits {
    DecodeLimits {
        max_entries: limits.max_entries.min(DecodeLimits::default().max_entries),
        max_original_bytes: limits.max_input_bytes,
        max_metadata_bytes: limits
            .max_metadata_bytes
            .min(DecodeLimits::default().max_metadata_bytes),
        ..DecodeLimits::default()
    }
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
    let mut buffer = [0u8; IO_BUFFER_SIZE];
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
