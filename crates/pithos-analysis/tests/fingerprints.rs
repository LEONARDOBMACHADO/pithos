use std::{
    io::{self, Cursor, Read},
    sync::atomic::{AtomicUsize, Ordering},
};

use pithos_analysis::{
    ChunkFingerprint, ChunkingMethod, FingerprintConfig, FingerprintInput, FullHashPolicy,
    LogicalChunk, fingerprint_chunks, fingerprint_chunks_with_checkpoint, fingerprint_reader,
    fingerprint_reader_with_checkpoint,
};
use pithos_core::{DecodeLimits, PithosError};

fn chunk(chunk_id: u64, length: usize) -> LogicalChunk {
    LogicalChunk {
        chunk_id,
        entry_id: chunk_id + 10,
        object_id: 0,
        logical_offset: chunk_id * 4096,
        length: u32::try_from(length).unwrap(),
        method: ChunkingMethod::FastCdcV2020,
    }
}

#[test]
fn fingerprint_config_defaults_are_bounded_and_normative() {
    let config = FingerprintConfig::default();
    assert_eq!(config.subchunk_count, 12);
    assert_eq!(config.superfeature_count, 4);
    assert_eq!(config.max_chunk_bytes, 4 * 1024 * 1024);
    assert_eq!(config.max_chunks, DecodeLimits::default().max_chunks);
    assert_eq!(
        config.max_total_bytes,
        DecodeLimits::default().max_original_bytes
    );
    config.validate().unwrap();

    for invalid in [
        FingerprintConfig {
            subchunk_count: 0,
            ..config
        },
        FingerprintConfig {
            superfeature_count: 2,
            ..config
        },
        FingerprintConfig {
            superfeature_count: 5,
            ..config
        },
        FingerprintConfig {
            max_chunk_bytes: 0,
            ..config
        },
        FingerprintConfig {
            max_chunks: 0,
            ..config
        },
        FingerprintConfig {
            max_total_bytes: 0,
            ..config
        },
    ] {
        assert!(matches!(
            invalid.validate(),
            Err(PithosError::InvalidMetadata(_))
        ));
    }
}

#[test]
fn one_chunk_receives_every_compact_fingerprint_and_superfeature() {
    let data = b"pithos fingerprint conformance vector v1";
    let fingerprint = ChunkFingerprint::compute(7, data).unwrap();
    let full = blake3::hash(data);

    assert_eq!(fingerprint.chunk_id, 7);
    assert_eq!(fingerprint.length, data.len() as u32);
    assert_eq!(fingerprint.xxh3, xxhash_rust::xxh3::xxh3_64(data));
    assert_eq!(fingerprint.blake3_128, full.as_bytes()[..16]);
    assert_eq!(fingerprint.crc32c, crc32c::crc32c(data));
    assert_eq!(fingerprint.full_blake3, None);
    assert_eq!(fingerprint.superfeatures.len(), 4);
    assert_eq!(
        fingerprint.superfeatures.as_slice(),
        &[
            4_035_240_308_330_923_769,
            4_879_917_261_836_866_628,
            10_844_589_855_720_895_847,
            12_518_628_049_364_330_038,
        ]
    );
    assert_eq!(
        fingerprint.superfeatures,
        ChunkFingerprint::compute(7, data).unwrap().superfeatures
    );
}

#[test]
fn compact_hashes_match_the_frozen_abc_vector() {
    let fingerprint = ChunkFingerprint::compute(0, b"abc").unwrap();
    assert_eq!(fingerprint.xxh3, 0x78af_5f94_892f_3950);
    assert_eq!(fingerprint.crc32c, 0x364b_3fb7);
    assert_eq!(
        fingerprint.blake3_128,
        [
            0x64, 0x37, 0xb3, 0xac, 0x38, 0x46, 0x51, 0x33, 0xff, 0xb6, 0x3b, 0x75, 0x27, 0x3a,
            0x8d, 0xb5,
        ]
    );
}

#[test]
fn empty_chunk_is_supported_without_synthetic_superfeatures() {
    let fingerprint = ChunkFingerprint::compute(0, b"").unwrap();
    assert_eq!(fingerprint.length, 0);
    assert!(fingerprint.superfeatures.is_empty());
    assert_eq!(fingerprint.blake3_128, blake3::hash(b"").as_bytes()[..16]);
}

#[test]
fn batch_is_sorted_deterministically_and_escalates_only_compact_collisions() {
    let duplicate = b"same logical bytes".to_vec();
    let unique = b"unique logical bytes".to_vec();
    let chunks = [
        chunk(9, duplicate.len()),
        chunk(2, unique.len()),
        chunk(5, duplicate.len()),
    ];
    let inputs = [
        FingerprintInput {
            chunk: &chunks[0],
            data: &duplicate,
        },
        FingerprintInput {
            chunk: &chunks[1],
            data: &unique,
        },
        FingerprintInput {
            chunk: &chunks[2],
            data: &duplicate,
        },
    ];

    let fingerprints = fingerprint_chunks(&inputs, &FingerprintConfig::default()).unwrap();
    assert_eq!(
        fingerprints
            .iter()
            .map(|item| item.chunk_id)
            .collect::<Vec<_>>(),
        [2, 5, 9]
    );
    assert!(fingerprints[0].full_blake3.is_none());
    assert_eq!(fingerprints[1].full_blake3, fingerprints[2].full_blake3);
    assert_eq!(
        fingerprints[1].full_blake3,
        Some(*blake3::hash(&duplicate).as_bytes())
    );

    let reversed = inputs.into_iter().rev().collect::<Vec<_>>();
    assert_eq!(
        fingerprint_chunks(&reversed, &FingerprintConfig::default()).unwrap(),
        fingerprints
    );
}

#[test]
fn full_hash_escalation_revalidates_the_compact_identity() {
    let data = b"identity";
    let mut fingerprint = ChunkFingerprint::compute(1, data).unwrap();
    fingerprint.escalate_full_blake3(data).unwrap();
    assert_eq!(
        fingerprint.full_blake3,
        Some(*blake3::hash(data).as_bytes())
    );

    let mut corrupted = ChunkFingerprint::compute(1, data).unwrap();
    assert!(matches!(
        corrupted.escalate_full_blake3(b"different"),
        Err(PithosError::HashMismatch) | Err(PithosError::InvalidMetadata(_))
    ));
    assert!(corrupted.full_blake3.is_none());
}

#[test]
fn paranoid_policy_retains_full_blake3_for_singletons() {
    let data = b"singleton";
    let descriptor = chunk(4, data.len());
    let config = FingerprintConfig {
        full_hash_policy: FullHashPolicy::Paranoid,
        ..FingerprintConfig::default()
    };
    let fingerprints = fingerprint_chunks(
        &[FingerprintInput {
            chunk: &descriptor,
            data,
        }],
        &config,
    )
    .unwrap();
    assert_eq!(
        fingerprints[0].full_blake3,
        Some(*blake3::hash(data).as_bytes())
    );
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
        Err(io::Error::other("synthetic fingerprint reader failure"))
    }
}

#[test]
fn streaming_matches_slice_and_rejects_short_or_trailing_input() {
    let data = vec![0x5a; 257 * 1024 + 19];
    let descriptor = chunk(11, data.len());
    let expected = ChunkFingerprint::compute(descriptor.chunk_id, &data).unwrap();
    for max_read in [1, 7, 64 * 1024] {
        let actual = fingerprint_reader(
            &descriptor,
            ThrottledReader {
                inner: Cursor::new(&data),
                max_read,
            },
            &FingerprintConfig::default(),
        )
        .unwrap();
        assert_eq!(actual, expected);
    }

    assert!(matches!(
        fingerprint_reader(
            &descriptor,
            Cursor::new(&data[..data.len() - 1]),
            &FingerprintConfig::default(),
        ),
        Err(PithosError::InvalidMetadata(_))
    ));
    let mut trailing = data.clone();
    trailing.push(0);
    assert!(matches!(
        fingerprint_reader(
            &descriptor,
            Cursor::new(trailing),
            &FingerprintConfig::default(),
        ),
        Err(PithosError::InvalidMetadata(_))
    ));
    assert!(matches!(
        fingerprint_reader(&descriptor, FailingReader, &FingerprintConfig::default(),),
        Err(PithosError::Io(_))
    ));
    assert!(matches!(
        fingerprint_reader_with_checkpoint(
            &descriptor,
            Cursor::new(&data),
            &FingerprintConfig::default(),
            &|| Err(PithosError::Cancelled),
        ),
        Err(PithosError::Cancelled)
    ));
}

#[test]
fn parallelism_does_not_change_batch_output() {
    let payloads = (0_u8..32)
        .map(|seed| vec![seed; usize::from(seed) * 997 + 1])
        .collect::<Vec<_>>();
    let chunks = payloads
        .iter()
        .enumerate()
        .map(|(id, data)| chunk(id as u64, data.len()))
        .collect::<Vec<_>>();
    let inputs = chunks
        .iter()
        .zip(&payloads)
        .rev()
        .map(|(chunk, data)| FingerprintInput { chunk, data })
        .collect::<Vec<_>>();

    let expected = fingerprint_chunks(
        &inputs,
        &FingerprintConfig {
            parallelism: 1,
            ..FingerprintConfig::default()
        },
    )
    .unwrap();
    for parallelism in [2, 4, 8] {
        assert_eq!(
            fingerprint_chunks(
                &inputs,
                &FingerprintConfig {
                    parallelism,
                    ..FingerprintConfig::default()
                },
            )
            .unwrap(),
            expected
        );
    }
}

#[test]
fn batch_rejects_mismatched_lengths_duplicate_ids_and_resource_abuse() {
    let bytes = [1_u8; 8];
    let wrong = chunk(0, 7);
    assert!(matches!(
        fingerprint_chunks(
            &[FingerprintInput {
                chunk: &wrong,
                data: &bytes,
            }],
            &FingerprintConfig::default(),
        ),
        Err(PithosError::InvalidMetadata(_))
    ));

    let first = chunk(3, bytes.len());
    let duplicate_id = chunk(3, bytes.len());
    let duplicate_inputs = [
        FingerprintInput {
            chunk: &first,
            data: &bytes,
        },
        FingerprintInput {
            chunk: &duplicate_id,
            data: &bytes,
        },
    ];
    assert!(matches!(
        fingerprint_chunks(&duplicate_inputs, &FingerprintConfig::default()),
        Err(PithosError::InvalidMetadata(_))
    ));

    let limits = FingerprintConfig {
        max_chunks: 1,
        max_total_bytes: 7,
        max_chunk_bytes: 7,
        ..FingerprintConfig::default()
    };
    assert!(matches!(
        fingerprint_chunks(&duplicate_inputs, &limits),
        Err(PithosError::ResourceLimit(_))
    ));
    assert!(matches!(
        fingerprint_chunks(
            &[FingerprintInput {
                chunk: &first,
                data: &bytes,
            }],
            &limits,
        ),
        Err(PithosError::ResourceLimit(_))
    ));
}

#[test]
fn parallel_fingerprinting_observes_thread_safe_cancellation() {
    let data = vec![7_u8; 128 * 1024];
    let chunks = [chunk(0, data.len()), chunk(1, data.len())];
    let inputs = [
        FingerprintInput {
            chunk: &chunks[0],
            data: &data,
        },
        FingerprintInput {
            chunk: &chunks[1],
            data: &data,
        },
    ];
    let calls = AtomicUsize::new(0);

    assert!(matches!(
        fingerprint_chunks_with_checkpoint(&inputs, &FingerprintConfig::default(), &|| {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(PithosError::Cancelled)
        }),
        Err(PithosError::Cancelled)
    ));
    assert!(calls.load(Ordering::Relaxed) > 0);
}
