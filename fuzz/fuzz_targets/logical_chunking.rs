#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use pithos_analysis::{
    ChunkOrigin, ChunkingConfig, MicroFileInput, assign_chunk_ids,
    assign_chunk_ids_with_checkpoint, chunk_fastcdc, chunk_fastcdc_reader,
    chunk_fastcdc_reader_with_checkpoint, chunk_fixed_high_entropy, chunk_structural,
    chunk_structural_reader, micro_file_logical_chunks, micro_file_logical_chunks_with_checkpoint,
    plan_micro_file_packs, plan_micro_file_packs_with_checkpoint,
};
use pithos_core::PithosError;

fuzz_target!(|data: &[u8]| {
    let origin = ChunkOrigin {
        entry_id: 1,
        object_id: 2,
        base_offset: data.first().copied().map_or(0, u64::from),
    };
    let mut config = ChunkingConfig::default();
    config.max_chunks = data.get(1).copied().map_or(1, |value| u64::from(value) + 1);
    let payload = data.get(2..).unwrap_or_default();

    if let Ok(chunks) = chunk_fastcdc(payload, origin, &config) {
        let streamed = chunk_fastcdc_reader(Cursor::new(payload), origin, &config);
        if let Ok(streamed) = streamed {
            assert_eq!(chunks, streamed);
        }
        let _ = assign_chunk_ids(chunks, config.max_chunks);
    }
    let mut reader_checkpoints = 0_u8;
    let _ = chunk_fastcdc_reader_with_checkpoint(Cursor::new(payload), origin, &config, || {
        reader_checkpoints = reader_checkpoints.saturating_add(1);
        if reader_checkpoints > data.get(2).copied().unwrap_or_default() % 4 {
            Err(PithosError::Cancelled)
        } else {
            Ok(())
        }
    });

    let fixed_size = payload
        .get(..8)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_le_bytes)
        .unwrap_or(payload.len() as u64)
        % (64 * 1024 * 1024);
    let _ = chunk_fixed_high_entropy(fixed_size, origin, &config);

    let mut boundaries = payload
        .chunks_exact(4)
        .take(16)
        .map(|bytes| {
            u64::from(u32::from_le_bytes(bytes.try_into().unwrap())) % (payload.len() as u64 + 2)
        })
        .collect::<Vec<_>>();
    if data.first().is_some_and(|value| value & 1 == 0) && !payload.is_empty() {
        boundaries.push(payload.len() as u64);
    }
    let structural = chunk_structural(payload, origin, &boundaries, &config);
    let streamed_structural =
        chunk_structural_reader(Cursor::new(payload), origin, &boundaries, &config);
    if let (Ok(memory), Ok(streamed)) = (structural, streamed_structural) {
        assert_eq!(memory, streamed);
    }

    let microfiles = payload
        .chunks(8)
        .take(32)
        .enumerate()
        .map(|(index, bytes)| {
            let seed = bytes.first().copied().unwrap_or_default();
            MicroFileInput {
                entry_id: if seed & 1 == 0 {
                    index as u64
                } else {
                    u64::from(seed)
                },
                path: if bytes.is_empty() {
                    vec![b'x']
                } else {
                    bytes.to_vec()
                },
                size: u64::from(seed) * 1024,
                modified_ns: i64::from(seed) - 128,
                mode: u32::from(seed),
                file_hash: [seed; 32],
                family_key: u64::from(seed & 3),
                path_prefix_key: vec![seed & 7],
                extension_key: vec![seed & 15],
                similarity_key: u64::from(seed & 31),
                requires_isolated_access: seed & 0x80 != 0,
            }
        })
        .collect::<Vec<_>>();
    if let Ok(plan) = plan_micro_file_packs(&microfiles, &config) {
        let _ = plan.validate(&config);
        let _ = micro_file_logical_chunks(&plan, 0, &config)
            .and_then(|drafts| assign_chunk_ids(drafts, config.max_chunks));
        let _ = micro_file_logical_chunks_with_checkpoint(&plan, 0, &config, || {
            Err(PithosError::Cancelled)
        });

        let mut invalid_prefix = plan.clone();
        if let Some(pack) = invalid_prefix.packs.first_mut() {
            if let Some(path) = pack.metadata.paths.first_mut() {
                path.shared_prefix_len = u32::MAX;
            }
            let _ = pack.metadata.expanded_paths_with_config(&config);
            let _ = pack.validate(&config);
        }

        let mut missing_path_column = plan.clone();
        if let Some(pack) = missing_path_column.packs.first_mut() {
            pack.metadata.paths.pop();
            let _ = pack.validate(&config);
        }

        let mut missing_timestamp_column = plan.clone();
        if let Some(pack) = missing_timestamp_column.packs.first_mut() {
            pack.metadata.modified_ns_deltas.pop();
            let _ = pack.validate(&config);
        }

        let mut invalid_mode = plan.clone();
        if let Some(record) = invalid_mode
            .packs
            .first_mut()
            .and_then(|pack| pack.metadata.records.first_mut())
        {
            record.mode_index = u32::MAX;
            let _ = invalid_mode.validate(&config);
        }

        let mut invalid_offset = plan.clone();
        if let Some(record) = invalid_offset
            .packs
            .first_mut()
            .and_then(|pack| pack.metadata.records.first_mut())
        {
            record.content_offset = u64::MAX;
            let _ = invalid_offset.validate(&config);
        }

        let mut duplicate_path = plan.clone();
        if let Some(pack) = duplicate_path.packs.first_mut()
            && pack.metadata.paths.len() > 1
        {
            let first = pack.metadata.paths[0].suffix.clone();
            pack.metadata.paths[1].shared_prefix_len = 0;
            pack.metadata.paths[1].suffix = first;
            let _ = pack.validate(&config);
        }

        let mut amplified_paths = plan.clone();
        if let Some(pack) = amplified_paths.packs.first_mut()
            && pack.metadata.paths.len() > 1
        {
            let mut expanded_len = pack.metadata.paths[0].suffix.len();
            for (index, path) in pack.metadata.paths.iter_mut().enumerate().skip(1) {
                path.shared_prefix_len = u32::try_from(expanded_len).unwrap_or(u32::MAX);
                path.suffix = vec![u8::try_from(index).unwrap_or(u8::MAX)];
                expanded_len = expanded_len.saturating_add(1);
            }
            let tight_config = ChunkingConfig {
                max_metadata_bytes: u64::from(data.get(4).copied().unwrap_or_default()) + 1,
                ..config
            };
            let _ = pack.metadata.expanded_paths_with_config(&tight_config);
        }
    }

    let mut planner_checkpoints = 0_u8;
    let _ = plan_micro_file_packs_with_checkpoint(&microfiles, &config, || {
        planner_checkpoints = planner_checkpoints.saturating_add(1);
        if planner_checkpoints > data.get(3).copied().unwrap_or_default() % 8 {
            Err(PithosError::Cancelled)
        } else {
            Ok(())
        }
    });

    let _ = assign_chunk_ids_with_checkpoint(Vec::new(), config.max_chunks, || {
        Err(PithosError::Cancelled)
    });
});
