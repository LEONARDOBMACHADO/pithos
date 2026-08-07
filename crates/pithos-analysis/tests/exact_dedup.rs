use pithos_analysis::{
    ChunkFingerprint, ChunkingMethod, DedupInput, ExactDedupConfig, LogicalChunk, exact_dedup,
    exact_dedup_with_config,
};

fn chunk(chunk_id: u64, data: &[u8]) -> LogicalChunk {
    LogicalChunk {
        chunk_id,
        entry_id: chunk_id,
        object_id: 0,
        logical_offset: 0,
        length: u32::try_from(data.len()).unwrap(),
        method: ChunkingMethod::FastCdcV2020,
    }
}

#[test]
fn gate_c3_detects_every_beneficial_exact_duplicate() {
    let a = vec![b'a'; 8192];
    let b = vec![b'b'; 4096];
    let c = vec![b'c'; 2048];
    let blobs = [&a[..], &a[..], &b[..], &a[..], &c[..], &b[..]];
    let chunks = blobs
        .iter()
        .enumerate()
        .map(|(index, data)| chunk(index as u64, data))
        .collect::<Vec<_>>();
    let fingerprints = blobs
        .iter()
        .enumerate()
        .map(|(index, data)| ChunkFingerprint::compute(index as u64, data).unwrap())
        .collect::<Vec<_>>();
    let inputs = (0..blobs.len())
        .map(|index| DedupInput {
            chunk: &chunks[index],
            fingerprint: &fingerprints[index],
            data: blobs[index],
        })
        .collect::<Vec<_>>();

    let plan = exact_dedup(&inputs).unwrap();
    assert_eq!(plan.referenced_chunks, 3);
    assert_eq!(plan.canonical_chunks, 3);
    assert_eq!(plan.record(1).unwrap().canonical_chunk_id, 0);
    assert_eq!(plan.record(3).unwrap().canonical_chunk_id, 0);
    assert_eq!(plan.record(5).unwrap().canonical_chunk_id, 2);
    assert!(plan.net_saved_bytes > 0);
}

#[test]
fn public_pipeline_never_deduplicates_different_bytes_after_compact_collision() {
    let left = vec![0x11; 4096];
    let right = vec![0x22; 4096];
    let chunks = [chunk(0, &left), chunk(1, &right)];
    let first = ChunkFingerprint::compute(0, &left).unwrap();
    let mut second = ChunkFingerprint::compute(1, &right).unwrap();
    second.xxh3 = first.xxh3;
    second.blake3_128 = first.blake3_128;
    second.full_blake3 = None;
    let inputs = [
        DedupInput {
            chunk: &chunks[0],
            fingerprint: &first,
            data: &left,
        },
        DedupInput {
            chunk: &chunks[1],
            fingerprint: &second,
            data: &right,
        },
    ];

    let plan = exact_dedup(&inputs).unwrap();
    assert_eq!(plan.referenced_chunks, 0);
    assert!(plan.records.iter().all(|record| !record.is_reference()));
}

#[test]
fn cost_model_keeps_non_beneficial_duplicates_physical() {
    let data = vec![0x44; 128];
    let chunks = [chunk(0, &data), chunk(1, &data)];
    let fingerprints = [
        ChunkFingerprint::compute(0, &data).unwrap(),
        ChunkFingerprint::compute(1, &data).unwrap(),
    ];
    let inputs = [
        DedupInput {
            chunk: &chunks[0],
            fingerprint: &fingerprints[0],
            data: &data,
        },
        DedupInput {
            chunk: &chunks[1],
            fingerprint: &fingerprints[1],
            data: &data,
        },
    ];
    let config = ExactDedupConfig {
        reference_cost_bytes: 128,
        min_net_savings_bytes: 1,
        ..ExactDedupConfig::default()
    };

    let plan = exact_dedup_with_config(&inputs, &config).unwrap();
    assert_eq!(plan.referenced_chunks, 0);
    assert_eq!(plan.net_saved_bytes, 0);
}
