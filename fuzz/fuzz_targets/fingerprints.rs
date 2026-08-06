#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use pithos_analysis::{
    ChunkFingerprint, ChunkingMethod, FingerprintConfig, FingerprintInput, FullHashPolicy,
    LogicalChunk, fingerprint_chunks, fingerprint_chunks_with_checkpoint, fingerprint_reader,
};
use pithos_core::PithosError;

fn chunk(chunk_id: u64, length: usize) -> LogicalChunk {
    LogicalChunk {
        chunk_id,
        entry_id: chunk_id.wrapping_add(1),
        object_id: 0,
        logical_offset: 0,
        length: u32::try_from(length).expect("fuzz payload is bounded"),
        method: ChunkingMethod::FastCdcV2020,
    }
}

fuzz_target!(|data: &[u8]| {
    const MAX_INPUT: usize = 256 * 1024;
    let payload = &data[..data.len().min(MAX_INPUT)];
    let seed = payload.first().copied().unwrap_or_default();
    let config = FingerprintConfig {
        full_hash_policy: if seed & 1 == 0 {
            FullHashPolicy::Standard
        } else {
            FullHashPolicy::Paranoid
        },
        parallelism: u16::from(seed % 4 + 1),
        ..FingerprintConfig::default()
    };

    let descriptor = chunk(11, payload.len());
    if let Ok(memory) = ChunkFingerprint::compute_with_config(11, payload, &config) {
        let streamed = fingerprint_reader(&descriptor, Cursor::new(payload), &config)
            .expect("bounded slice and exact reader must agree");
        assert_eq!(memory, streamed);

        let mut escalated = memory.clone();
        escalated
            .escalate_full_blake3(payload)
            .expect("the original bytes must revalidate");
        assert!(escalated.full_blake3.is_some());
    }

    let split = payload.len() / 2;
    let left = &payload[..split];
    let right = &payload[split..];
    let chunks = [
        chunk(9, left.len()),
        chunk(2, right.len()),
        chunk(7, left.len()),
    ];
    let inputs = [
        FingerprintInput {
            chunk: &chunks[0],
            data: left,
        },
        FingerprintInput {
            chunk: &chunks[1],
            data: right,
        },
        FingerprintInput {
            chunk: &chunks[2],
            data: left,
        },
    ];
    // Sample pool construction so campaigns spend most executions mutating
    // hash and rolling-feature state instead of creating worker threads.
    if seed & 0x0f == 0
        && let Ok(batch) = fingerprint_chunks(&inputs, &config)
    {
        assert!(
            batch
                .windows(2)
                .all(|pair| pair[0].chunk_id < pair[1].chunk_id)
        );
        let duplicate_a = batch.iter().find(|item| item.chunk_id == 7).unwrap();
        let duplicate_b = batch.iter().find(|item| item.chunk_id == 9).unwrap();
        assert_eq!(duplicate_a.xxh3, duplicate_b.xxh3);
        assert_eq!(duplicate_a.blake3_128, duplicate_b.blake3_128);
        assert!(duplicate_a.full_blake3.is_some());
        assert!(duplicate_b.full_blake3.is_some());
        let cancelled =
            fingerprint_chunks_with_checkpoint(&inputs, &config, &|| Err(PithosError::Cancelled));
        assert!(matches!(cancelled, Err(PithosError::Cancelled)));
    }

    let invalid = FingerprintConfig {
        subchunk_count: seed,
        superfeature_count: seed.rotate_left(1),
        rolling_window: u16::from(seed),
        parallelism: u16::from(seed),
        ..config
    };
    let _ = invalid.validate();
    let _ = ChunkFingerprint::compute_with_config(0, payload, &invalid);
});
