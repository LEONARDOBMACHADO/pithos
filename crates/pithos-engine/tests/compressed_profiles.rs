use pithos_core::CompressionProfile;
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
