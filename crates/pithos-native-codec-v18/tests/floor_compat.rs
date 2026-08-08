#[test]
fn current_decoder_accepts_historical_v12_floor_payloads() {
    for size in [
        4 * 1024_usize,
        256 * 1024,
        1024 * 1024 + 37,
    ] {
        let input = (0..size)
            .map(|index| {
                let lane = (index / 257) as u8;
                (index as u8).wrapping_mul(37) ^ lane.rotate_left((index % 7) as u32)
            })
            .collect::<Vec<_>>();
        let lengths = [input.len() as u64];
        let (payload, _) = pithos_native_floor::encode_exact_dedup(&input, &lengths, 15)
            .expect("v12 floor encode");
        let decoded = pithos_native_current::decode_exact_dedup(&payload, input.len() as u64)
            .expect("v17/current decoder must preserve the historical fallback chain");
        assert_eq!(decoded, input, "v12 compatibility failed for {size} bytes");
    }
}
