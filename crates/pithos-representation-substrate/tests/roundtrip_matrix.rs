use pithos_representation_substrate::{decode, encode};

fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.max(1);
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state as u8).wrapping_add((index as u8).wrapping_mul(31)));
    }
    out
}

fn roundtrip(input: &[u8], member_lengths: &[u64]) -> Vec<u8> {
    let (payload, stats) = encode(input, member_lengths, 9).expect("PRS1 encode must succeed");
    assert_eq!(stats.encoded_bytes, payload.len() as u64);
    let decoded = decode(&payload, input.len() as u64).expect("PRS1 decode must succeed");
    assert_eq!(decoded, input);
    payload
}

#[test]
fn public_roundtrip_covers_cell_boundaries() {
    for (index, size) in [
        1_usize,
        31,
        255,
        4095,
        4096,
        4097,
        65_535,
        65_536,
        65_537,
        1_048_576,
        1_048_577,
    ]
    .into_iter()
    .enumerate()
    {
        let input = deterministic_bytes(size, 0x9e37_79b9_u64 ^ index as u64);
        roundtrip(&input, &[size as u64]);
    }
}

#[test]
fn public_roundtrip_preserves_member_boundaries() {
    let members = [4095_usize, 4096, 4097, 32_768, 65_536, 131_073];
    let mut input = Vec::new();
    let mut lengths = Vec::new();
    for (index, len) in members.into_iter().enumerate() {
        let bytes = deterministic_bytes(len, 0x51ed_270b_u64.wrapping_add(index as u64));
        input.extend_from_slice(&bytes);
        lengths.push(len as u64);
    }
    roundtrip(&input, &lengths);
}

#[test]
fn public_roundtrip_covers_structural_families_together() {
    let template = b"PRS1-template-family-".repeat(8192);
    let duplicate = template.clone();

    let mut overlay = template.clone();
    for offset in [17_usize, 4093, 16_381, 65_521, 98_317] {
        if let Some(value) = overlay.get_mut(offset) {
            *value ^= 0x5a;
        }
    }

    let mixture = (0..256 * 1024)
        .map(|index| [b'A', b'B', b'C', b'D'][index % 4])
        .collect::<Vec<_>>();

    let pattern = [0x11_u8, 0x22, 0x33, 0x44];
    let mut defects = (0..256 * 1024)
        .map(|index| pattern[index % pattern.len()])
        .collect::<Vec<_>>();
    for value in defects.iter_mut().step_by(4093) {
        *value ^= 0xa5;
    }

    let mut transitions = Vec::new();
    for value in 0..128_u8 {
        transitions.extend(std::iter::repeat_n(value.wrapping_mul(3), 1024));
    }

    let members = [template, duplicate, overlay, mixture, defects, transitions];
    let mut input = Vec::new();
    let mut lengths = Vec::new();
    for member in members {
        lengths.push(member.len() as u64);
        input.extend_from_slice(&member);
    }

    roundtrip(&input, &lengths);
}

#[test]
fn public_encoding_is_deterministic() {
    let input = deterministic_bytes(192 * 1024 + 37, 0xd1b5_4a32_d192_ed03);
    let lengths = [65_537_u64, 65_539_u64, (input.len() - 131_076) as u64];

    let (first, first_stats) = encode(&input, &lengths, 9).expect("first encode");
    let (second, second_stats) = encode(&input, &lengths, 9).expect("second encode");

    assert_eq!(first, second);
    assert_eq!(first_stats, second_stats);
    assert_eq!(decode(&first, input.len() as u64).expect("decode"), input);
}

#[test]
fn decoder_rejects_truncated_payload() {
    let input = b"truncation-guard".repeat(8192);
    let mut payload = roundtrip(&input, &[input.len() as u64]);
    payload.pop().expect("encoded payload is non-empty");
    assert!(decode(&payload, input.len() as u64).is_err());
}

#[test]
fn decoder_rejects_corrupt_magic() {
    let input = b"magic-guard".repeat(8192);
    let mut payload = roundtrip(&input, &[input.len() as u64]);
    payload[0] ^= 0xff;
    assert!(decode(&payload, input.len() as u64).is_err());
}

#[test]
fn decoder_rejects_wrong_expected_length() {
    let input = deterministic_bytes(96 * 1024 + 11, 0x243f_6a88_85a3_08d3);
    let payload = roundtrip(&input, &[input.len() as u64]);
    assert!(decode(&payload, input.len() as u64 + 1).is_err());
}
