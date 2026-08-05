use pithos_codecs::{BrotliCodec, Codec, CodecConfig, CodecId, Lzma2Codec, StoreCodec, ZstdCodec};
use pithos_core::{CompressionProfile, PithosError};
use pithos_engine::{CancellationToken, PackLimits, pack_with_limits_and_control};
use pithos_engine::{PackRequest, UnpackRequest, pack, unpack, verify};
use pithos_format::{
    CodecRegistry, GlobalHeader, GroupTableRecord, HEADER_LEN, SECTION_ENTRY_LEN,
    SectionDirectoryRecord, SectionType,
};
use std::fs;
use std::io::{Read, Seek, SeekFrom};

fn write_fixture(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let paths = [
        root.join("alpha.txt"),
        root.join("beta.txt"),
        root.join("gamma.txt"),
    ];
    for (index, path) in paths.iter().enumerate() {
        let body = format!(
            "shared-prefix-{index}-{}",
            "compressible-pattern-".repeat(512)
        );
        fs::write(path, body).unwrap();
    }
    paths.into_iter().collect()
}

fn read_phase_two_sections(path: &std::path::Path) -> (CodecRegistry, Vec<GroupTableRecord>) {
    let mut file = fs::File::open(path).unwrap();
    let mut header_bytes = [0_u8; HEADER_LEN];
    file.read_exact(&mut header_bytes).unwrap();
    let header = GlobalHeader::decode(&header_bytes).unwrap();
    let mut directory = vec![0_u8; header.section_count as usize * SECTION_ENTRY_LEN];
    file.read_exact(&mut directory).unwrap();
    let records = directory
        .chunks_exact(SECTION_ENTRY_LEN)
        .map(|bytes| SectionDirectoryRecord::decode(bytes.try_into().unwrap()))
        .collect::<Vec<_>>();
    let read_section = |section_type: SectionType, file: &mut fs::File| {
        let record = records
            .iter()
            .find(|record| record.section_type == section_type as u16)
            .unwrap();
        file.seek(SeekFrom::Start(record.offset)).unwrap();
        let mut bytes = vec![0_u8; record.length as usize];
        file.read_exact(&mut bytes).unwrap();
        bytes
    };
    let registry =
        CodecRegistry::decode(&read_section(SectionType::CodecRegistry, &mut file), 32).unwrap();
    let groups = serde_json::from_slice(&read_section(SectionType::GroupTable, &mut file)).unwrap();
    (registry, groups)
}

#[test]
fn balanced_roundtrip_uses_registry_and_one_logical_solid_group() {
    let temp = tempfile::tempdir().unwrap();
    let inputs = write_fixture(temp.path());
    let expected = inputs
        .iter()
        .map(|path| {
            (
                path.file_name().unwrap().to_owned(),
                fs::read(path).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let archive = temp.path().join("balanced.pithos");
    pack(PackRequest {
        inputs,
        output: archive.clone(),
        profile: CompressionProfile::Balanced,
    })
    .unwrap();

    let report = verify(&archive).unwrap();
    assert_eq!(report.file_count, 3);
    assert_eq!(report.group_count, 1);
    let (registry, groups) = read_phase_two_sections(&archive);
    assert!(!registry.records.is_empty());
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].group.chunk_count, 3);
    assert_ne!(groups[0].group.codec_chain_id, 0);
    assert!(groups[0].group.compressed_len < groups[0].group.uncompressed_len);
    let selected = registry.chain(groups[0].group.codec_chain_id).unwrap();
    let codec: &dyn Codec = match CodecId::from_u16(selected.codec_id).unwrap() {
        CodecId::Store => &StoreCodec,
        CodecId::Zstd => &ZstdCodec,
        CodecId::Brotli => &BrotliCodec,
        CodecId::Lzma2 => &Lzma2Codec,
    };
    let independent_compressed_len = expected
        .iter()
        .map(|(_, bytes)| {
            let mut encoded = Vec::new();
            codec
                .encode(
                    bytes,
                    &CodecConfig {
                        level: selected.level,
                    },
                    &mut encoded,
                )
                .unwrap();
            encoded.len() as u64
        })
        .sum::<u64>();
    assert!(
        groups[0].group.compressed_len < independent_compressed_len,
        "solid={} independent={independent_compressed_len}",
        groups[0].group.compressed_len
    );

    let restored = temp.path().join("restored");
    unpack(UnpackRequest {
        archive,
        output_dir: restored.clone(),
    })
    .unwrap();
    for (name, bytes) in expected {
        assert_eq!(fs::read(restored.join(name)).unwrap(), bytes);
    }
}

#[test]
fn all_non_raw_profiles_pack_and_verify() {
    for profile in [
        CompressionProfile::Stream,
        CompressionProfile::Random,
        CompressionProfile::Balanced,
        CompressionProfile::ArchiveMax,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("payload.txt");
        fs::write(&input, "phase-two-vector-".repeat(256)).unwrap();
        let archive = temp.path().join("profile.pithos");
        pack(PackRequest {
            inputs: vec![input],
            output: archive.clone(),
            profile,
        })
        .unwrap();
        assert_eq!(verify(&archive).unwrap().file_count, 1);
    }
}

#[test]
fn compressed_output_is_byte_deterministic() {
    let temp = tempfile::tempdir().unwrap();
    let inputs = write_fixture(temp.path());
    let first = temp.path().join("first.pithos");
    let second = temp.path().join("second.pithos");
    for output in [&first, &second] {
        pack(PackRequest {
            inputs: inputs.clone(),
            output: output.clone(),
            profile: CompressionProfile::Stream,
        })
        .unwrap();
    }
    assert_eq!(fs::read(first).unwrap(), fs::read(second).unwrap());
}

#[test]
fn compressed_profiles_preserve_empty_files() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("empty.bin");
    fs::write(&input, []).unwrap();
    let archive = temp.path().join("empty.pithos");
    pack(PackRequest {
        inputs: vec![input],
        output: archive.clone(),
        profile: CompressionProfile::Balanced,
    })
    .unwrap();
    let restored = temp.path().join("empty-restored");
    unpack(UnpackRequest {
        archive,
        output_dir: restored.clone(),
    })
    .unwrap();
    assert_eq!(
        fs::read(restored.join("empty.bin")).unwrap(),
        Vec::<u8>::new()
    );
}

#[test]
fn compressed_pack_honors_memory_budget_independently_from_temp_budget() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("payload.txt");
    fs::write(&input, "bounded-memory-payload".repeat(256)).unwrap();
    let archive = temp.path().join("memory-limited.pithos");
    let limits = PackLimits {
        max_memory_bytes: 1024 * 1024,
        max_temp_bytes: u64::MAX,
        ..PackLimits::default()
    };

    let error = pack_with_limits_and_control(
        PackRequest {
            inputs: vec![input],
            output: archive.clone(),
            profile: CompressionProfile::Balanced,
        },
        &limits,
        &CancellationToken::new(),
    )
    .expect_err("the codec task must not exceed its explicit memory budget");

    assert!(matches!(error, PithosError::MemoryLimit));
    assert!(!archive.exists());
}
