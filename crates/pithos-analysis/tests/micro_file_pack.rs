use pithos_analysis::{
    ChunkingConfig, MicroFileExclusionReason, MicroFileInput, MicroFilePackPlan,
    plan_micro_file_packs,
};
use pithos_core::PithosError;

const KIB: u32 = 1024;
const MIB: u32 = 1024 * KIB;

fn input(entry_id: u64, path: &str, size: u64) -> MicroFileInput {
    MicroFileInput {
        entry_id,
        path: path.as_bytes().to_vec(),
        size,
        modified_ns: 1_700_000_000_000_000_000,
        mode: 0o644,
        file_hash: hash(entry_id as u8),
        family_key: 7,
        path_prefix_key: b"root".to_vec(),
        extension_key: b"bin".to_vec(),
        similarity_key: 11,
        requires_isolated_access: false,
    }
}

fn hash(seed: u8) -> [u8; 32] {
    [seed; 32]
}

fn flattened_entry_ids(plan: &MicroFilePackPlan) -> Vec<u64> {
    plan.packs
        .iter()
        .flat_map(|pack| pack.members.iter().map(|member| member.entry_id))
        .collect()
}

#[test]
fn eligibility_is_inclusive_at_64_kib_and_exclusions_are_explicit() {
    let config = ChunkingConfig::default();
    assert_eq!(config.micro_file_max, 64 * KIB);

    let mut isolated = input(4, "isolated.db", u64::from(config.micro_file_max));
    isolated.requires_isolated_access = true;

    let inputs = vec![
        input(1, "empty.bin", 0),
        input(2, "below.bin", u64::from(config.micro_file_max - 1)),
        input(3, "at-limit.bin", u64::from(config.micro_file_max)),
        isolated,
        input(5, "too-large.bin", u64::from(config.micro_file_max) + 1),
        input(6, "unrepresentable.bin", u64::MAX),
    ];

    let plan = plan_micro_file_packs(&inputs, &config).unwrap();

    assert_eq!(flattened_entry_ids(&plan), vec![3, 2, 1]);
    assert_eq!(
        plan.excluded
            .iter()
            .map(|excluded| (excluded.entry_id, excluded.reason))
            .collect::<Vec<_>>(),
        vec![
            (4, MicroFileExclusionReason::RequiresIsolatedAccess),
            (5, MicroFileExclusionReason::TooLarge),
            (6, MicroFileExclusionReason::TooLarge),
        ]
    );
}

#[test]
fn planning_has_a_canonical_total_order_for_every_input_permutation() {
    let mut inputs = Vec::new();
    for entry_id in 0..9 {
        let mut candidate = input(entry_id, &format!("root/{entry_id:02}.bin"), 31);
        candidate.family_key = entry_id % 3;
        candidate.path_prefix_key = format!("prefix-{}", entry_id % 2).into_bytes();
        candidate.extension_key = format!("ext-{}", entry_id % 4).into_bytes();
        candidate.mode = if entry_id % 2 == 0 { 0o600 } else { 0o644 };
        candidate.similarity_key = entry_id % 5;
        inputs.push(candidate);
    }

    let expected = plan_micro_file_packs(&inputs, &ChunkingConfig::default()).unwrap();

    let mut reversed = inputs.clone();
    reversed.reverse();
    assert_eq!(
        plan_micro_file_packs(&reversed, &ChunkingConfig::default()).unwrap(),
        expected
    );

    for rotation in 1..inputs.len() {
        let mut rotated = inputs.clone();
        rotated.rotate_left(rotation);
        assert_eq!(
            plan_micro_file_packs(&rotated, &ChunkingConfig::default()).unwrap(),
            expected,
            "rotation {rotation} changed the canonical plan"
        );
    }
}

#[test]
fn grouping_signals_define_member_proximity_before_path_tie_breaking() {
    let keys = [
        (1, 1, b"a".as_slice(), b"txt".as_slice(), 0o600, 9),
        (2, 0, b"z".as_slice(), b"txt".as_slice(), 0o600, 9),
        (3, 0, b"a".as_slice(), b"zip".as_slice(), 0o600, 9),
        (4, 0, b"a".as_slice(), b"txt".as_slice(), 0o644, 9),
        (5, 0, b"a".as_slice(), b"txt".as_slice(), 0o600, 10),
        (6, 0, b"a".as_slice(), b"txt".as_slice(), 0o600, 9),
        (7, 0, b"a".as_slice(), b"txt".as_slice(), 0o600, 9),
    ];
    let paths = [
        "g.bin", "f.bin", "e.bin", "d.bin", "c.bin", "b.bin", "a.bin",
    ];

    let mut inputs = keys
        .into_iter()
        .zip(paths)
        .map(
            |((entry_id, family, prefix, extension, mode, similarity), path)| {
                let mut candidate = input(entry_id, path, 1);
                candidate.family_key = family;
                candidate.path_prefix_key = prefix.to_vec();
                candidate.extension_key = extension.to_vec();
                candidate.mode = mode;
                candidate.similarity_key = similarity;
                candidate
            },
        )
        .collect::<Vec<_>>();
    inputs.reverse();

    let plan = plan_micro_file_packs(&inputs, &ChunkingConfig::default()).unwrap();

    // Order: family, path prefix, extension, mode, similarity, path, entry_id.
    assert_eq!(flattened_entry_ids(&plan), vec![7, 6, 5, 4, 3, 2, 1]);
}

#[test]
fn target_size_splits_packs_and_offsets_restart_without_gaps() {
    let config = ChunkingConfig {
        micro_pack_target: MIB,
        ..ChunkingConfig::default()
    };
    let inputs = (0..33)
        .map(|entry_id| {
            input(
                entry_id,
                &format!("root/{entry_id:03}.bin"),
                u64::from(64 * KIB),
            )
        })
        .collect::<Vec<_>>();

    let plan = plan_micro_file_packs(&inputs, &config).unwrap();

    assert_eq!(plan.packs.len(), 3);
    assert_eq!(
        plan.packs
            .iter()
            .map(|pack| pack.pack_id)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        plan.packs
            .iter()
            .map(|pack| pack.uncompressed_len)
            .collect::<Vec<_>>(),
        vec![u64::from(MIB), u64::from(MIB), u64::from(64 * KIB)]
    );

    for pack in &plan.packs {
        let mut expected_offset = 0_u64;
        for member in &pack.members {
            assert_eq!(member.content_offset, expected_offset);
            expected_offset = expected_offset
                .checked_add(u64::from(member.length))
                .unwrap();
        }
        assert_eq!(expected_offset, pack.uncompressed_len);
        assert!(pack.uncompressed_len <= u64::from(config.micro_pack_target));
    }
}

#[test]
fn zero_byte_files_are_preserved_with_canonical_metadata() {
    let mut alpha = input(10, "src/a.txt", 0);
    alpha.modified_ns = 100;
    alpha.mode = 0o644;
    alpha.file_hash = hash(0xaa);
    alpha.extension_key = b"txt".to_vec();

    let mut beta = input(11, "src/ab.txt", 5);
    beta.modified_ns = 120;
    beta.mode = 0o600;
    beta.file_hash = hash(0xbb);
    beta.extension_key = b"txt".to_vec();

    let mut gamma = input(12, "src/abc.txt", 7);
    gamma.modified_ns = 90;
    gamma.mode = 0o644;
    gamma.file_hash = hash(0xcc);
    gamma.extension_key = b"txt".to_vec();

    let plan = plan_micro_file_packs(&[gamma, beta, alpha], &ChunkingConfig::default()).unwrap();
    assert_eq!(plan.packs.len(), 1);
    let pack = &plan.packs[0];

    assert_eq!(flattened_entry_ids(&plan), vec![11, 10, 12]);
    plan.validate(&ChunkingConfig::default()).unwrap();
    assert_eq!(
        pack.metadata.expanded_paths().unwrap(),
        vec![
            b"src/ab.txt".to_vec(),
            b"src/a.txt".to_vec(),
            b"src/abc.txt".to_vec(),
        ]
    );
    assert_eq!(pack.uncompressed_len, 12);
    assert_eq!(pack.members[0].length, 5);
    assert_eq!(pack.members[0].content_offset, 0);
    assert_eq!(pack.members[1].length, 0);
    assert_eq!(pack.members[1].content_offset, 5);
    assert_eq!(pack.members[2].content_offset, 5);

    assert_eq!(pack.metadata.base_modified_ns, 90);
    assert_eq!(pack.metadata.modified_ns_deltas, vec![30, 10, 0]);
    assert_eq!(pack.metadata.mode_dictionary, vec![0o600, 0o644]);
    assert_eq!(
        pack.metadata
            .records
            .iter()
            .map(|record| record.mode_index)
            .collect::<Vec<_>>(),
        vec![0, 1, 1]
    );
    assert_eq!(pack.metadata.records[0].content_offset, 0);
    assert_eq!(pack.metadata.records[0].length, 5);
    assert_eq!(pack.metadata.records[0].file_hash, hash(0xbb));
}

#[test]
fn timestamp_delta_handles_the_full_i64_domain_without_overflow() {
    let mut earliest = input(1, "a.bin", 1);
    earliest.modified_ns = i64::MIN;
    let mut latest = input(2, "b.bin", 1);
    latest.modified_ns = i64::MAX;

    let plan = plan_micro_file_packs(&[latest, earliest], &ChunkingConfig::default()).unwrap();
    let metadata = &plan.packs[0].metadata;

    assert_eq!(metadata.base_modified_ns, i64::MIN);
    assert_eq!(metadata.modified_ns_deltas, vec![0, u64::MAX]);
}

#[test]
fn duplicate_entry_ids_and_paths_fail_closed() {
    let duplicate_ids = [input(7, "a.bin", 1), input(7, "b.bin", 1)];
    assert!(matches!(
        plan_micro_file_packs(&duplicate_ids, &ChunkingConfig::default()),
        Err(PithosError::InvalidMetadata(_))
    ));

    let duplicate_paths = [input(7, "same.bin", 1), input(8, "same.bin", 1)];
    assert!(matches!(
        plan_micro_file_packs(&duplicate_paths, &ChunkingConfig::default()),
        Err(PithosError::InvalidMetadata(_))
    ));
}

#[test]
fn max_chunks_limits_pack_count_before_returning_a_partial_plan() {
    let config = ChunkingConfig {
        micro_pack_target: MIB,
        max_chunks: 1,
        ..ChunkingConfig::default()
    };
    let inputs = (0..17)
        .map(|entry_id| {
            input(
                entry_id,
                &format!("root/{entry_id:03}.bin"),
                u64::from(64 * KIB),
            )
        })
        .collect::<Vec<_>>();

    assert!(matches!(
        plan_micro_file_packs(&inputs, &config),
        Err(PithosError::ResourceLimit(_))
    ));
}

#[test]
fn empty_input_has_no_synthetic_pack_or_exclusion() {
    let plan = plan_micro_file_packs(&[], &ChunkingConfig::default()).unwrap();
    assert!(plan.packs.is_empty());
    assert!(plan.excluded.is_empty());
}

#[test]
fn compact_metadata_validation_rejects_corrupt_columns_and_offsets() {
    let inputs = [input(1, "root/a.bin", 3), input(2, "root/b.bin", 5)];
    let config = ChunkingConfig::default();
    let plan = plan_micro_file_packs(&inputs, &config).unwrap();

    let mut invalid_prefix = plan.clone();
    invalid_prefix.packs[0].metadata.paths[0].shared_prefix_len = 1;
    assert!(matches!(
        invalid_prefix.validate(&config),
        Err(PithosError::InvalidMetadata(_))
    ));

    let mut invalid_offset = plan.clone();
    invalid_offset.packs[0].metadata.records[1].content_offset += 1;
    assert!(matches!(
        invalid_offset.validate(&config),
        Err(PithosError::InvalidMetadata(_))
    ));

    let mut timestamp_overflow = plan;
    timestamp_overflow.packs[0].metadata.base_modified_ns = i64::MAX;
    timestamp_overflow.packs[0].metadata.modified_ns_deltas[0] = 1;
    assert!(matches!(
        timestamp_overflow.validate(&config),
        Err(PithosError::IntegerOverflow)
    ));
}
