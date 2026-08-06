use std::sync::atomic::{AtomicUsize, Ordering};

use pithos_analysis::{
    ChunkFingerprint, ChunkingMethod, FingerprintConfig, FingerprintInput, LogicalChunk,
    fingerprint_chunks, fingerprint_chunks_with_checkpoint,
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
    assert_eq!(config.max_total_bytes, DecodeLimits::default().max_original_bytes);
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
        fingerprint.superfeatures,
        ChunkFingerprint::compute(7, data).unwrap().superfeatures
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
        fingerprints.iter().map(|item| item.chunk_id).collect::<Vec<_>>(),
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
