use std::io::{self, Cursor, Read};

use pithos_analysis::{
    ChunkOrigin, ChunkingConfig, ChunkingMethod, LogicalChunkDraft, assign_chunk_ids,
    chunk_fastcdc, chunk_fastcdc_reader, chunk_fastcdc_reader_with_checkpoint,
    chunk_fixed_high_entropy, chunk_structural, validate_chunk_coverage,
};
use pithos_core::{DecodeLimits, PithosError};
use proptest::prelude::*;

const KIB: u32 = 1024;
const MIB: u32 = 1024 * KIB;

fn deterministic_bytes(length: usize) -> Vec<u8> {
    let mut state = 0x6a09_e667_f3bc_c909_u64;
    (0..length)
        .map(|index| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state as u8) ^ (index as u8).wrapping_mul(31)
        })
        .collect()
}

struct ThrottledReader<R> {
    inner: R,
    max_read: usize,
}

impl<R: Read> Read for ThrottledReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let allowed = buffer.len().min(self.max_read);
        self.inner.read(&mut buffer[..allowed])
    }
}

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("synthetic reader failure"))
    }
}

fn assert_contiguous_coverage(
    chunks: &[LogicalChunkDraft],
    origin: ChunkOrigin,
    logical_size: u64,
    method: ChunkingMethod,
) {
    validate_chunk_coverage(chunks, origin, logical_size).unwrap();

    let mut cursor = origin.base_offset;
    for chunk in chunks {
        assert_eq!(chunk.entry_id, origin.entry_id);
        assert_eq!(chunk.object_id, origin.object_id);
        assert_eq!(chunk.logical_offset, cursor);
        assert!(chunk.length > 0);
        assert_eq!(chunk.method, method);
        cursor += u64::from(chunk.length);
    }
    assert_eq!(cursor, origin.base_offset + logical_size);
}

#[test]
fn default_config_is_normative_and_valid() {
    let config = ChunkingConfig::default();

    assert_eq!(config.fastcdc_min, 64 * KIB);
    assert_eq!(config.fastcdc_avg, 256 * KIB);
    assert_eq!(config.fastcdc_max, MIB);
    assert_eq!(config.high_entropy_fixed, MIB);
    assert_eq!(config.micro_file_max, 64 * KIB);
    assert_eq!(config.micro_pack_target, 4 * MIB);
    assert_eq!(config.max_chunks, DecodeLimits::default().max_chunks);
    config.validate().unwrap();
}

#[test]
fn config_rejects_invalid_size_relations_and_resource_limits() {
    let invalid_configs = [
        ChunkingConfig {
            fastcdc_min: 0,
            ..ChunkingConfig::default()
        },
        ChunkingConfig {
            fastcdc_min: 256 * KIB,
            fastcdc_avg: 64 * KIB,
            ..ChunkingConfig::default()
        },
        ChunkingConfig {
            fastcdc_avg: 2 * MIB,
            fastcdc_max: MIB,
            ..ChunkingConfig::default()
        },
        ChunkingConfig {
            high_entropy_fixed: MIB - 1,
            ..ChunkingConfig::default()
        },
        ChunkingConfig {
            high_entropy_fixed: 4 * MIB + 1,
            ..ChunkingConfig::default()
        },
        ChunkingConfig {
            micro_file_max: 64 * KIB + 1,
            ..ChunkingConfig::default()
        },
        ChunkingConfig {
            micro_pack_target: MIB - 1,
            ..ChunkingConfig::default()
        },
        ChunkingConfig {
            micro_pack_target: 16 * MIB + 1,
            ..ChunkingConfig::default()
        },
        ChunkingConfig {
            max_chunks: 0,
            ..ChunkingConfig::default()
        },
    ];

    for config in invalid_configs {
        assert!(matches!(
            config.validate(),
            Err(PithosError::InvalidMetadata(_))
        ));
    }
}

#[test]
fn fastcdc_is_deterministic_and_covers_the_complete_logical_range() {
    let data = deterministic_bytes(3 * MIB as usize + 137);
    let origin = ChunkOrigin {
        entry_id: 7,
        object_id: 11,
        base_offset: 4096,
    };
    let config = ChunkingConfig::default();

    let first = chunk_fastcdc(&data, origin, &config).unwrap();
    let second = chunk_fastcdc(&data, origin, &config).unwrap();

    assert_eq!(first, second);
    assert!(first.len() >= 3);
    assert_contiguous_coverage(
        &first,
        origin,
        data.len() as u64,
        ChunkingMethod::FastCdcV2020,
    );
    for (index, chunk) in first.iter().enumerate() {
        assert!(chunk.length <= config.fastcdc_max);
        if index + 1 != first.len() {
            assert!(chunk.length >= config.fastcdc_min);
        }
    }
}

#[test]
fn streaming_fastcdc_matches_the_slice_implementation_exactly() {
    let data = deterministic_bytes(2 * MIB as usize + 73 * KIB as usize + 19);
    let origin = ChunkOrigin {
        entry_id: 3,
        object_id: 5,
        base_offset: 8192,
    };
    let config = ChunkingConfig::default();

    let from_slice = chunk_fastcdc(&data, origin, &config).unwrap();
    let from_reader = chunk_fastcdc_reader(Cursor::new(&data), origin, &config).unwrap();

    assert_eq!(from_reader, from_slice);
}

#[test]
fn streaming_boundaries_do_not_depend_on_reader_batch_size() {
    let data = deterministic_bytes(3 * MIB as usize + 7919);
    let origin = ChunkOrigin {
        entry_id: 89,
        object_id: 144,
        base_offset: 233,
    };
    let config = ChunkingConfig::default();
    let expected = chunk_fastcdc(&data, origin, &config).unwrap();

    for max_read in [1, 7, 64 * KIB as usize] {
        let reader = ThrottledReader {
            inner: Cursor::new(&data),
            max_read,
        };
        assert_eq!(
            chunk_fastcdc_reader(reader, origin, &config).unwrap(),
            expected,
            "boundaries diverged with reader batches of {max_read} bytes"
        );
    }
}

#[test]
fn streaming_fastcdc_propagates_io_errors() {
    let origin = ChunkOrigin {
        entry_id: 1,
        object_id: 1,
        base_offset: 0,
    };

    assert!(matches!(
        chunk_fastcdc_reader(FailingReader, origin, &ChunkingConfig::default()),
        Err(PithosError::Io(_))
    ));
}

#[test]
fn streaming_fastcdc_observes_cancellation_checkpoint() {
    let data = deterministic_bytes(MIB as usize + 1);
    let origin = ChunkOrigin {
        entry_id: 1,
        object_id: 1,
        base_offset: 0,
    };
    let mut checkpoint_calls = 0_u64;

    let result = chunk_fastcdc_reader_with_checkpoint(
        Cursor::new(data),
        origin,
        &ChunkingConfig::default(),
        || {
            checkpoint_calls += 1;
            Err(PithosError::Cancelled)
        },
    );

    assert!(matches!(result, Err(PithosError::Cancelled)));
    assert!(checkpoint_calls > 0);
}

#[test]
fn fastcdc_and_fixed_chunking_return_no_chunks_for_empty_inputs() {
    let origin = ChunkOrigin {
        entry_id: 1,
        object_id: 2,
        base_offset: 123,
    };
    let config = ChunkingConfig::default();

    assert!(chunk_fastcdc(&[], origin, &config).unwrap().is_empty());
    assert!(
        chunk_fastcdc_reader(Cursor::new(Vec::<u8>::new()), origin, &config)
            .unwrap()
            .is_empty()
    );
    assert!(
        chunk_fixed_high_entropy(0, origin, &config)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn fixed_high_entropy_chunking_handles_tail_and_exact_multiple() {
    let origin = ChunkOrigin {
        entry_id: 13,
        object_id: 21,
        base_offset: 65_537,
    };
    let config = ChunkingConfig::default();

    let with_tail = chunk_fixed_high_entropy(u64::from(2 * MIB + 17), origin, &config).unwrap();
    assert_eq!(
        with_tail
            .iter()
            .map(|chunk| chunk.length)
            .collect::<Vec<_>>(),
        [MIB, MIB, 17]
    );
    assert_contiguous_coverage(
        &with_tail,
        origin,
        u64::from(2 * MIB + 17),
        ChunkingMethod::FixedHighEntropy,
    );

    let exact = chunk_fixed_high_entropy(u64::from(2 * MIB), origin, &config).unwrap();
    assert_eq!(
        exact.iter().map(|chunk| chunk.length).collect::<Vec<_>>(),
        [MIB, MIB]
    );
}

#[test]
fn fixed_high_entropy_supports_the_normative_one_to_four_mib_edges() {
    for block_size in [MIB, 4 * MIB] {
        let config = ChunkingConfig {
            high_entropy_fixed: block_size,
            ..ChunkingConfig::default()
        };
        config.validate().unwrap();
        let origin = ChunkOrigin {
            entry_id: u64::from(block_size),
            object_id: 1,
            base_offset: 0,
        };
        let chunks = chunk_fixed_high_entropy(u64::from(block_size) + 1, origin, &config).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].length, block_size);
        assert_eq!(chunks[1].length, 1);
    }
}

#[test]
fn structural_chunking_preserves_scanner_ends_and_subchunks_large_regions() {
    let data = deterministic_bytes(3 * MIB as usize + 101);
    let origin = ChunkOrigin {
        entry_id: 34,
        object_id: 55,
        base_offset: 16_384,
    };
    let config = ChunkingConfig::default();
    let boundary_ends = [128 * KIB as u64, 2 * MIB as u64, data.len() as u64];

    let chunks = chunk_structural(&data, origin, &boundary_ends, &config).unwrap();
    let produced_ends = chunks
        .iter()
        .map(|chunk| chunk.logical_offset + u64::from(chunk.length))
        .collect::<Vec<_>>();

    assert_contiguous_coverage(
        &chunks,
        origin,
        data.len() as u64,
        ChunkingMethod::Structural,
    );
    for boundary_end in boundary_ends {
        assert!(produced_ends.contains(&(origin.base_offset + boundary_end)));
    }
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.length <= config.fastcdc_max)
    );
    assert!(chunks.len() > boundary_ends.len());
}

#[test]
fn structural_chunking_rejects_noncanonical_or_incomplete_boundaries() {
    let data = deterministic_bytes(1024);
    let origin = ChunkOrigin {
        entry_id: 1,
        object_id: 1,
        base_offset: 0,
    };
    let config = ChunkingConfig::default();
    let invalid_boundaries: [&[u64]; 6] = [
        &[],
        &[0, 1024],
        &[512, 512, 1024],
        &[768, 512, 1024],
        &[512, 1025],
        &[512],
    ];

    for boundary_ends in invalid_boundaries {
        assert!(matches!(
            chunk_structural(&data, origin, boundary_ends, &config),
            Err(PithosError::InvalidMetadata(_))
        ));
    }
}

#[test]
fn coverage_validation_rejects_gaps_overlaps_zero_lengths_and_wrong_origin() {
    let origin = ChunkOrigin {
        entry_id: 8,
        object_id: 9,
        base_offset: 100,
    };
    let draft = |entry_id, object_id, logical_offset, length| LogicalChunkDraft {
        entry_id,
        object_id,
        logical_offset,
        length,
        method: ChunkingMethod::Structural,
    };
    let invalid = [
        vec![draft(8, 9, 100, 10), draft(8, 9, 111, 9)],
        vec![draft(8, 9, 100, 11), draft(8, 9, 110, 9)],
        vec![draft(8, 9, 100, 0), draft(8, 9, 100, 20)],
        vec![draft(7, 9, 100, 20)],
        vec![draft(8, 7, 100, 20)],
    ];

    for chunks in invalid {
        assert!(matches!(
            validate_chunk_coverage(&chunks, origin, 20),
            Err(PithosError::InvalidMetadata(_))
        ));
    }
}

#[test]
fn chunk_ids_are_assigned_after_deterministic_tuple_sorting() {
    let draft = |entry_id, object_id, logical_offset, length| LogicalChunkDraft {
        entry_id,
        object_id,
        logical_offset,
        length,
        method: ChunkingMethod::FastCdcV2020,
    };
    let shuffled = vec![
        draft(2, 0, 5, 2),
        draft(1, 4, 9, 3),
        draft(1, 3, 10, 4),
        draft(1, 3, 0, 10),
    ];
    let mut reversed = shuffled.clone();
    reversed.reverse();

    let assigned = assign_chunk_ids(shuffled, 4).unwrap();
    let assigned_from_reverse = assign_chunk_ids(reversed, 4).unwrap();

    assert_eq!(assigned, assigned_from_reverse);
    assert_eq!(
        assigned
            .iter()
            .map(|chunk| (
                chunk.chunk_id,
                chunk.entry_id,
                chunk.object_id,
                chunk.logical_offset,
            ))
            .collect::<Vec<_>>(),
        [(0, 1, 3, 0), (1, 1, 3, 10), (2, 1, 4, 9), (3, 2, 0, 5),]
    );
}

#[test]
fn max_chunks_is_enforced_before_unbounded_chunk_metadata_growth() {
    let config = ChunkingConfig {
        max_chunks: 2,
        ..ChunkingConfig::default()
    };
    let origin = ChunkOrigin {
        entry_id: 1,
        object_id: 1,
        base_offset: 0,
    };

    assert!(matches!(
        chunk_fixed_high_entropy(u64::from(2 * MIB + 1), origin, &config),
        Err(PithosError::ResourceLimit(_))
    ));
    let fastcdc_input = deterministic_bytes(3 * MIB as usize + 1);
    assert!(matches!(
        chunk_fastcdc(&fastcdc_input, origin, &config),
        Err(PithosError::ResourceLimit(_))
    ));
    assert!(matches!(
        chunk_structural(
            &fastcdc_input,
            origin,
            &[MIB as u64, 2 * MIB as u64, fastcdc_input.len() as u64],
            &config,
        ),
        Err(PithosError::ResourceLimit(_))
    ));

    let drafts = vec![
        LogicalChunkDraft {
            entry_id: 1,
            object_id: 1,
            logical_offset: 0,
            length: 1,
            method: ChunkingMethod::FixedHighEntropy,
        },
        LogicalChunkDraft {
            entry_id: 1,
            object_id: 1,
            logical_offset: 1,
            length: 1,
            method: ChunkingMethod::FixedHighEntropy,
        },
        LogicalChunkDraft {
            entry_id: 1,
            object_id: 1,
            logical_offset: 2,
            length: 1,
            method: ChunkingMethod::FixedHighEntropy,
        },
    ];
    assert!(matches!(
        assign_chunk_ids(drafts, 2),
        Err(PithosError::ResourceLimit(_))
    ));
}

#[test]
fn chunk_offsets_fail_closed_on_u64_overflow() {
    let origin = ChunkOrigin {
        entry_id: 1,
        object_id: 1,
        base_offset: u64::MAX - 7,
    };

    assert!(matches!(
        chunk_fixed_high_entropy(8, origin, &ChunkingConfig::default()),
        Err(PithosError::IntegerOverflow)
    ));
    assert!(matches!(
        chunk_fastcdc(&[0; 8], origin, &ChunkingConfig::default()),
        Err(PithosError::IntegerOverflow)
    ));
    assert!(matches!(
        chunk_fastcdc_reader(Cursor::new([0; 8]), origin, &ChunkingConfig::default()),
        Err(PithosError::IntegerOverflow)
    ));
    assert!(matches!(
        chunk_structural(&[0; 8], origin, &[8], &ChunkingConfig::default()),
        Err(PithosError::IntegerOverflow)
    ));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn fixed_chunking_preserves_coverage_for_arbitrary_lengths(
        logical_size in 0_u64..(16 * u64::from(MIB)),
        fixed_mib in 1_u32..=4,
        base_offset in 0_u64..u64::from(MIB),
    ) {
        let config = ChunkingConfig {
            high_entropy_fixed: fixed_mib * MIB,
            ..ChunkingConfig::default()
        };
        let origin = ChunkOrigin {
            entry_id: 1,
            object_id: 2,
            base_offset,
        };

        let chunks = chunk_fixed_high_entropy(logical_size, origin, &config).unwrap();
        validate_chunk_coverage(&chunks, origin, logical_size).unwrap();
        prop_assert!(chunks.iter().all(|chunk| chunk.length <= config.high_entropy_fixed));
    }

    #[test]
    fn fastcdc_never_creates_a_gap_for_generated_content(
        length in 0_usize..(2 * MIB as usize),
        seed in any::<u64>(),
    ) {
        let mut state = seed;
        let data = (0..length)
            .map(|index| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                (state >> 32) as u8 ^ index as u8
            })
            .collect::<Vec<_>>();
        let origin = ChunkOrigin {
            entry_id: 3,
            object_id: 4,
            base_offset: 5,
        };

        let chunks = chunk_fastcdc(&data, origin, &ChunkingConfig::default()).unwrap();
        validate_chunk_coverage(&chunks, origin, data.len() as u64).unwrap();
    }
}
