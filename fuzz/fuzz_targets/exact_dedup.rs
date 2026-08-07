#![no_main]

use libfuzzer_sys::fuzz_target;
use pithos_analysis::{
    ChunkFingerprint, ChunkingMethod, DedupInput, ExactDedupConfig, LogicalChunk,
    exact_dedup_with_config,
};

fuzz_target!(|data: &[u8]| {
    let bounded = &data[..data.len().min(64 * 1024)];
    let split = bounded.len() / 3;
    let first = bounded[..split].to_vec();
    let second = bounded[split..split.saturating_mul(2)].to_vec();
    let third = bounded[split.saturating_mul(2)..].to_vec();

    let mut blobs = vec![first.clone(), second, third];
    if bounded.first().is_some_and(|byte| byte & 1 != 0) {
        blobs.push(first);
    }

    let chunks = blobs
        .iter()
        .enumerate()
        .map(|(index, blob)| LogicalChunk {
            chunk_id: index as u64,
            entry_id: index as u64,
            object_id: 0,
            logical_offset: 0,
            length: blob.len() as u32,
            method: if blob.is_empty() {
                ChunkingMethod::MicroFile
            } else {
                ChunkingMethod::FastCdcV2020
            },
        })
        .collect::<Vec<_>>();

    let mut fingerprints = blobs
        .iter()
        .enumerate()
        .map(|(index, blob)| ChunkFingerprint::compute(index as u64, blob))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    // Deliberately forge a compact collision sometimes. Exact dedup must still
    // require a full-hash/byte match before producing a reference.
    if bounded.get(1).is_some_and(|byte| byte & 1 != 0) && fingerprints.len() > 1 {
        fingerprints[1].xxh3 = fingerprints[0].xxh3;
        fingerprints[1].blake3_128 = fingerprints[0].blake3_128;
        fingerprints[1].full_blake3 = None;
    }

    let inputs = (0..chunks.len())
        .map(|index| DedupInput {
            chunk: &chunks[index],
            fingerprint: &fingerprints[index],
            data: &blobs[index],
        })
        .collect::<Vec<_>>();

    let config = ExactDedupConfig {
        parallelism: 1,
        ..ExactDedupConfig::default()
    };
    if let Ok(plan) = exact_dedup_with_config(&inputs, &config) {
        assert_eq!(plan.records.len(), inputs.len());
        for record in &plan.records {
            if record.is_reference() {
                let source = &blobs[record.chunk_id as usize];
                let canonical = &blobs[record.canonical_chunk_id as usize];
                assert_eq!(source, canonical);
                assert!(record.net_saved_bytes > 0);
            }
        }
    }
});
