//! Gate A integration tests and test-vector helpers.

pub fn assert_bytes_equal_and_blake3(original: &[u8], restored: &[u8]) {
    assert_eq!(original, restored, "conteúdo restaurado diverge");
    assert_eq!(blake3::hash(original), blake3::hash(restored));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pithos_core::{CompressionProfile, DecodeLimits, PithosError};
    use pithos_engine::{
        CancellationToken, ExtractRequest, PackLimits, PackRequest, ReadRangeRequest,
        UnpackRequest, extract, extract_to_writer, extract_with_control, inspect,
        inspect_with_control, list, list_with_control, pack, pack_with_control,
        pack_with_limits_and_control, read_range_to_writer_with_control, unpack, verify,
        verify_with_control, verify_with_limits,
    };
    use pithos_format::{
        FOOTER_LEN, Footer, GlobalHeader, HEADER_LEN, SECTION_ENTRY_LEN, SectionDirectoryRecord,
        SectionType,
    };
    use proptest::prelude::*;
    use std::fs::{self, File};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::Path;

    fn pack_directory(source: &Path, archive: &Path) {
        pack(PackRequest {
            inputs: vec![source.to_path_buf()],
            output: archive.to_path_buf(),
            profile: CompressionProfile::Raw,
        })
        .unwrap();
    }

    fn read_bytes(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        File::open(path).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    }

    fn write_bytes(path: &Path, bytes: &[u8]) {
        File::create(path).unwrap().write_all(bytes).unwrap();
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("payload marker ausente")
    }

    fn payload_offset(bytes: &[u8]) -> usize {
        let header_buf: &[u8; HEADER_LEN] = bytes[..HEADER_LEN].try_into().unwrap();
        let header = GlobalHeader::decode(header_buf).unwrap();
        for index in 0..header.section_count as usize {
            let start = header.section_directory_offset as usize + index * SECTION_ENTRY_LEN;
            let record = SectionDirectoryRecord::decode(
                bytes[start..start + SECTION_ENTRY_LEN].try_into().unwrap(),
            );
            if record.section_type == SectionType::PayloadArea as u16 {
                return record.offset as usize;
            }
        }
        panic!("PayloadArea ausente")
    }

    #[test]
    fn gate_a_roundtrip_covers_empty_one_byte_random_nested_unicode_and_empty_dir() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let nested = source.join("dados").join("unicódé");
        let empty_dir = source.join("diretório vazio");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&empty_dir).unwrap();
        write_bytes(&source.join("empty.bin"), b"");
        write_bytes(&source.join("one.bin"), b"A");
        let random: Vec<u8> = (0..131_071).map(|i| ((i * 73 + 19) % 256) as u8).collect();
        write_bytes(&nested.join("aleatório.bin"), &random);
        let mut sparse = File::create(source.join("sparse.bin")).unwrap();
        sparse.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        sparse.write_all(b"tail").unwrap();

        let archive = temp.path().join("archive.pithos");
        let restored = temp.path().join("restored");
        pack_directory(&source, &archive);
        let report = verify(&archive).unwrap();
        assert_eq!(report.file_count, 4);
        assert_eq!(report.directory_count, 3);

        unpack(UnpackRequest {
            archive,
            output_dir: restored.clone(),
        })
        .unwrap();

        assert_bytes_equal_and_blake3(
            &random,
            &read_bytes(&restored.join("dados/unicódé/aleatório.bin")),
        );
        assert!(restored.join("diretório vazio").is_dir());
        let sparse_restored = read_bytes(&restored.join("sparse.bin"));
        assert_eq!(sparse_restored.len(), 1024 * 1024 + 4);
        assert_eq!(&sparse_restored[1024 * 1024..], b"tail");
    }

    #[test]
    fn gate_a_output_is_byte_deterministic() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write_bytes(&source.join("b.txt"), b"Data 2");
        write_bytes(&source.join("a.txt"), b"Data 1");
        let first = temp.path().join("first.pithos");
        let second = temp.path().join("second.pithos");
        pack_directory(&source, &first);
        pack_directory(&source, &second);
        assert_eq!(read_bytes(&first), read_bytes(&second));
    }

    #[test]
    fn gate_a_corruption_is_detected_and_unpack_is_transactional() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write_bytes(&source.join("payload.bin"), b"integrity protected payload");
        let archive = temp.path().join("valid.pithos");
        pack_directory(&source, &archive);

        let mut bytes = read_bytes(&archive);
        let payload = payload_offset(&bytes);
        bytes[payload] ^= 0x80;
        let corrupt = temp.path().join("corrupt.pithos");
        write_bytes(&corrupt, &bytes);
        assert!(verify(&corrupt).is_err());

        let output = temp.path().join("must-not-exist");
        assert!(
            unpack(UnpackRequest {
                archive: corrupt,
                output_dir: output.clone()
            })
            .is_err()
        );
        assert!(!output.exists(), "falha não pode publicar saída parcial");
    }

    #[test]
    fn gate_a_every_sampled_archive_region_is_integrity_protected() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write_bytes(&source.join("payload"), b"protect every archive region");
        let archive = temp.path().join("valid.pithos");
        pack_directory(&source, &archive);
        let original = read_bytes(&archive);
        let mutated = temp.path().join("mutated.pithos");
        for position in (0..original.len()).step_by(7) {
            let mut bytes = original.clone();
            bytes[position] ^= 1;
            write_bytes(&mutated, &bytes);
            assert!(verify(&mutated).is_err(), "byte sem proteção: {position}");
        }
    }

    #[test]
    fn gate_a_rejects_overlapping_sections() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write_bytes(&source.join("file"), b"x");
        let archive = temp.path().join("valid.pithos");
        pack_directory(&source, &archive);
        let mut bytes = read_bytes(&archive);
        let header = GlobalHeader::decode(bytes[..HEADER_LEN].try_into().unwrap()).unwrap();
        let first_start = header.section_directory_offset as usize;
        let second_start = first_start + SECTION_ENTRY_LEN;
        let first = SectionDirectoryRecord::decode(
            bytes[first_start..first_start + SECTION_ENTRY_LEN]
                .try_into()
                .unwrap(),
        );
        let mut second = SectionDirectoryRecord::decode(
            bytes[second_start..second_start + SECTION_ENTRY_LEN]
                .try_into()
                .unwrap(),
        );
        second.offset = first.offset;
        bytes[second_start..second_start + SECTION_ENTRY_LEN].copy_from_slice(&second.encode());
        let directory_end = first_start + header.section_count as usize * SECTION_ENTRY_LEN;
        let footer_start = header.footer_offset as usize;
        let mut footer = Footer::decode(
            bytes[footer_start..footer_start + FOOTER_LEN]
                .try_into()
                .unwrap(),
        )
        .unwrap();
        footer.directory_crc32c = crc32c::crc32c(&bytes[first_start..directory_end]);
        bytes[footer_start..footer_start + FOOTER_LEN].copy_from_slice(&footer.encode());
        let corrupt = temp.path().join("overlap.pithos");
        write_bytes(&corrupt, &bytes);
        assert!(matches!(
            verify(&corrupt),
            Err(PithosError::OverlappingSections)
        ));
    }

    #[test]
    fn gate_a_rejects_bomb_declarations_before_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write_bytes(&source.join("file"), b"x");
        let archive = temp.path().join("valid.pithos");
        pack_directory(&source, &archive);
        let mut bytes = read_bytes(&archive);
        let mut header = GlobalHeader::decode(bytes[..HEADER_LEN].try_into().unwrap()).unwrap();
        header.entry_count = DecodeLimits::default().max_entries + 1;
        bytes[..HEADER_LEN].copy_from_slice(&header.encode());
        let bomb = temp.path().join("bomb.pithos");
        write_bytes(&bomb, &bytes);
        assert!(matches!(verify(&bomb), Err(PithosError::ResourceLimit(_))));
    }

    #[test]
    fn gate_a_pre_cancel_does_not_publish_archive() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write_bytes(&source.join("file"), b"cancel me");
        let archive = temp.path().join("cancelled.pithos");
        let token = CancellationToken::new();
        token.cancel();
        let result = pack_with_control(
            PackRequest {
                inputs: vec![source],
                output: archive.clone(),
                profile: CompressionProfile::Raw,
            },
            &token,
        );
        assert!(matches!(result, Err(PithosError::Cancelled)));
        assert!(!archive.exists());
    }

    #[test]
    fn gate_a_hardlinks_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let original = source.join("original.bin");
        write_bytes(&original, b"shared inode");
        fs::hard_link(&original, source.join("alias.bin")).unwrap();
        let archive = temp.path().join("hardlink.pithos");
        let restored = temp.path().join("restored");
        pack_directory(&source, &archive);
        let report = verify(&archive).unwrap();
        assert_eq!(report.group_count, 1);
        assert_eq!(report.hardlink_count, 1);
        unpack(UnpackRequest {
            archive,
            output_dir: restored.clone(),
        })
        .unwrap();
        assert_eq!(read_bytes(&restored.join("original.bin")), b"shared inode");
        assert_eq!(read_bytes(&restored.join("alias.bin")), b"shared inode");
        assert_eq!(
            same_file::Handle::from_path(restored.join("original.bin")).unwrap(),
            same_file::Handle::from_path(restored.join("alias.bin")).unwrap()
        );
    }

    #[test]
    fn gate_a_single_file_and_empty_directory_are_valid_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let single = temp.path().join("single.bin");
        write_bytes(&single, b"single");
        let single_archive = temp.path().join("single.pithos");
        pack(PackRequest {
            inputs: vec![single],
            output: single_archive.clone(),
            profile: CompressionProfile::Raw,
        })
        .unwrap();
        let single_output = temp.path().join("single-output");
        unpack(UnpackRequest {
            archive: single_archive,
            output_dir: single_output.clone(),
        })
        .unwrap();
        assert_eq!(read_bytes(&single_output.join("single.bin")), b"single");

        let empty = temp.path().join("empty-source");
        fs::create_dir(&empty).unwrap();
        let empty_archive = temp.path().join("empty.pithos");
        pack_directory(&empty, &empty_archive);
        let empty_output = temp.path().join("empty-output");
        unpack(UnpackRequest {
            archive: empty_archive,
            output_dir: empty_output.clone(),
        })
        .unwrap();
        assert!(empty_output.is_dir());
        assert_eq!(fs::read_dir(empty_output).unwrap().count(), 0);
    }

    #[test]
    fn gate_a_limits_and_no_clobber_are_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write_bytes(&source.join("file"), b"content");
        let archive = temp.path().join("archive.pithos");
        pack_directory(&source, &archive);

        let strict = DecodeLimits {
            max_entries: 0,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            verify_with_limits(&archive, &strict),
            Err(PithosError::ResourceLimit(_))
        ));

        let destination = temp.path().join("existing");
        fs::create_dir(&destination).unwrap();
        assert!(matches!(
            unpack(UnpackRequest {
                archive: archive.clone(),
                output_dir: destination,
            }),
            Err(PithosError::OutputExists)
        ));
        assert!(matches!(
            pack(PackRequest {
                inputs: vec![source],
                output: archive,
                profile: CompressionProfile::Raw,
            }),
            Err(PithosError::OutputExists)
        ));
    }

    #[test]
    fn gate_a_invalid_and_truncated_inputs_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let archive = temp.path().join("archive.pithos");
        assert!(
            pack(PackRequest {
                inputs: vec![missing],
                output: archive.clone(),
                profile: CompressionProfile::Raw,
            })
            .is_err()
        );
        assert!(!archive.exists());
        write_bytes(&archive, b"UDOC");
        assert!(verify(&archive).is_err());
    }

    #[test]
    fn phase_1_listing_and_inspection_do_not_read_payload() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write_bytes(&source.join("a.txt"), b"EXTRACT_THIS_ENTRY_ONLY");
        write_bytes(&source.join("b.txt"), b"CORRUPT_THIS_UNRELATED_PAYLOAD");
        let archive = temp.path().join("archive.pithos");
        pack_directory(&source, &archive);

        let mut bytes = read_bytes(&archive);
        let corrupt_at = find_bytes(&bytes, b"CORRUPT_THIS_UNRELATED_PAYLOAD");
        bytes[corrupt_at] ^= 1;
        write_bytes(&archive, &bytes);

        let entries = list(&archive).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "a.txt");
        let summary = inspect(&archive).unwrap();
        assert_eq!(summary.entry_count, 2);
        assert!(verify(&archive).is_err());
    }

    #[test]
    fn phase_1_extract_reads_only_the_requested_group_and_rejects_unsafe_selector() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write_bytes(&source.join("a.txt"), b"EXTRACT_THIS_ENTRY_ONLY");
        write_bytes(&source.join("b.txt"), b"CORRUPT_THIS_UNRELATED_PAYLOAD");
        let archive = temp.path().join("archive.pithos");
        pack_directory(&source, &archive);
        let mut bytes = read_bytes(&archive);
        let corrupt_at = find_bytes(&bytes, b"CORRUPT_THIS_UNRELATED_PAYLOAD");
        bytes[corrupt_at] ^= 1;
        write_bytes(&archive, &bytes);

        let destination = temp.path().join("extract");
        let report = extract(ExtractRequest {
            archive: archive.clone(),
            entry: Path::new("a.txt").to_path_buf(),
            output_dir: destination.clone(),
        })
        .unwrap();
        assert_eq!(report.path, "a.txt");
        assert_eq!(
            read_bytes(&destination.join("a.txt")),
            b"EXTRACT_THIS_ENTRY_ONLY"
        );
        assert!(
            extract(ExtractRequest {
                archive,
                entry: Path::new("../escape").to_path_buf(),
                output_dir: destination,
            })
            .is_err()
        );
    }

    #[test]
    fn phase_1_stream_extract_and_directory_extract_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        write_bytes(&source.join("nested/file.txt"), b"streamed payload");
        std::fs::create_dir_all(source.join("empty")).unwrap();
        let archive = temp.path().join("archive.pithos");
        pack(PackRequest {
            inputs: vec![source],
            output: archive.clone(),
            profile: CompressionProfile::Raw,
        })
        .unwrap();

        let mut streamed = Vec::new();
        let streamed_report =
            extract_to_writer(&archive, Path::new("nested/file.txt"), &mut streamed).unwrap();
        assert_eq!(streamed, b"streamed payload");
        assert_eq!(
            Path::new(&streamed_report.path),
            Path::new("nested/file.txt")
        );
        assert_eq!(streamed_report.bytes_written, 16);

        let destination = temp.path().join("destination");
        let directory_report = extract(ExtractRequest {
            archive,
            entry: Path::new("empty").to_path_buf(),
            output_dir: destination.clone(),
        })
        .unwrap();
        assert_eq!(directory_report.bytes_written, 0);
        assert!(destination.join("empty").is_dir());

        let obstructed = temp.path().join("obstructed");
        fs::create_dir_all(&obstructed).unwrap();
        File::create(obstructed.join("nested")).unwrap();
        assert!(
            extract(ExtractRequest {
                archive: temp.path().join("archive.pithos"),
                entry: Path::new("nested/file.txt").to_path_buf(),
                output_dir: obstructed,
            })
            .is_err()
        );
    }

    #[test]
    fn phase_1_daemon_operations_honor_external_cancellation_and_ranges() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write_bytes(&source.join("file.txt"), b"0123456789");
        let archive = temp.path().join("archive.pithos");
        pack_directory(&source, &archive);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let limits = DecodeLimits::default();
        assert!(matches!(
            list_with_control(&archive, &limits, &cancelled),
            Err(PithosError::Cancelled)
        ));
        assert!(matches!(
            inspect_with_control(&archive, &limits, &cancelled),
            Err(PithosError::Cancelled)
        ));
        assert!(matches!(
            verify_with_control(&archive, &limits, &cancelled),
            Err(PithosError::Cancelled)
        ));
        let cancelled_output = temp.path().join("cancelled-output");
        assert!(matches!(
            extract_with_control(
                ExtractRequest {
                    archive: archive.clone(),
                    entry: Path::new("file.txt").to_path_buf(),
                    output_dir: cancelled_output.clone(),
                },
                &limits,
                &cancelled,
            ),
            Err(PithosError::Cancelled)
        ));
        assert!(!cancelled_output.exists());

        let mut range = Vec::new();
        let report = read_range_to_writer_with_control(
            ReadRangeRequest {
                archive,
                entry: Path::new("file.txt").to_path_buf(),
                offset: 3,
                length: 4,
            },
            &mut range,
            &limits,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(range, b"3456");
        assert_eq!(report.entry_size, 10);
        assert_eq!(report.length, 4);
        assert_eq!(report.blake3, *blake3::hash(b"3456").as_bytes());
    }

    #[test]
    fn phase_1_pack_budget_stops_before_publishing_an_oversized_archive() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write_bytes(&source.join("payload.bin"), &[7_u8; 1024]);
        let archive = temp.path().join("bounded.pithos");

        let error = pack_with_limits_and_control(
            PackRequest {
                inputs: vec![source],
                output: archive.clone(),
                profile: CompressionProfile::Raw,
            },
            &PackLimits {
                max_input_bytes: 2048,
                max_memory_bytes: 1024 * 1024,
                max_temp_bytes: 512,
                max_output_bytes: 512,
                max_metadata_bytes: 1024 * 1024,
                max_entries: 100,
            },
            &CancellationToken::new(),
        )
        .unwrap_err();

        assert!(matches!(error, PithosError::ResourceLimit(_)));
        assert!(!archive.exists());
    }

    #[test]
    fn phase_1_decode_budget_counts_all_metadata_sections_together() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        for number in 0..8 {
            write_bytes(
                &source.join(format!("entry-{number}.txt")),
                format!("metadata-{number}").as_bytes(),
            );
        }
        let archive = temp.path().join("metadata.pithos");
        pack_directory(&source, &archive);
        let bytes = read_bytes(&archive);
        let header = GlobalHeader::decode(bytes[..HEADER_LEN].try_into().unwrap()).unwrap();
        let directory_len = u64::from(header.section_count) * SECTION_ENTRY_LEN as u64;
        let mut metadata_sum = directory_len;
        let mut largest_single = directory_len;
        for index in 0..header.section_count as usize {
            let start = header.section_directory_offset as usize + index * SECTION_ENTRY_LEN;
            let record = SectionDirectoryRecord::decode(
                bytes[start..start + SECTION_ENTRY_LEN].try_into().unwrap(),
            );
            if record.section_type != SectionType::PayloadArea as u16 {
                metadata_sum += record.length;
                largest_single = largest_single.max(record.length);
            }
        }
        assert!(metadata_sum > largest_single);

        let limits = DecodeLimits {
            max_metadata_bytes: largest_single,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            verify_with_limits(&archive, &limits),
            Err(PithosError::ResourceLimit(_))
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn gate_a_full_archive_parser_never_panics(data in prop::collection::vec(any::<u8>(), 0..16_384)) {
            let archive = tempfile::NamedTempFile::new().unwrap();
            write_bytes(archive.path(), &data);
            let _ = verify(archive.path());
        }
    }

    #[cfg(windows)]
    #[test]
    fn gate_a_non_utf16_windows_path_roundtrips_losslessly() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let raw_name = OsString::from_wide(&[b'n' as u16, 0xd800, b'x' as u16]);
        write_bytes(&source.join(&raw_name), b"WTF-16 name");
        let archive = temp.path().join("path.pithos");
        let restored = temp.path().join("restored");
        pack_directory(&source, &archive);
        unpack(UnpackRequest {
            archive,
            output_dir: restored.clone(),
        })
        .unwrap();
        assert_eq!(read_bytes(&restored.join(raw_name)), b"WTF-16 name");
    }

    #[cfg(unix)]
    #[test]
    fn gate_a_symlinks_and_non_utf8_paths_roundtrip_safely() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;
        use std::path::PathBuf;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        write_bytes(&source.join("target"), b"target");
        symlink("target", source.join("link")).unwrap();
        let raw_name = OsString::from_vec(vec![b'n', b'o', b'n', 0xff, b'u', b'8']);
        write_bytes(&source.join(&raw_name), b"raw name");
        let archive = temp.path().join("paths.pithos");
        let restored = temp.path().join("restored");
        pack_directory(&source, &archive);
        unpack(UnpackRequest {
            archive,
            output_dir: restored.clone(),
        })
        .unwrap();
        assert_eq!(
            fs::read_link(restored.join("link")).unwrap(),
            PathBuf::from("target")
        );
        assert_eq!(read_bytes(&restored.join(raw_name)), b"raw name");
    }
}
