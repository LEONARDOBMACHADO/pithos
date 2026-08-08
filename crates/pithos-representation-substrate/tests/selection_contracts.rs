use pithos_representation_substrate::{SubstrateStats, decode, encode};

fn encode_one(input: &[u8]) -> SubstrateStats {
    let (payload, stats) = encode(input, &[input.len() as u64], 9).expect("PRS1 encode");
    assert_eq!(decode(&payload, input.len() as u64).expect("PRS1 decode"), input);
    stats
}

fn encode_members(members: &[Vec<u8>]) -> SubstrateStats {
    let mut input = Vec::new();
    let mut lengths = Vec::new();
    for member in members {
        lengths.push(member.len() as u64);
        input.extend_from_slice(member);
    }
    let (payload, stats) = encode(&input, &lengths, 9).expect("PRS1 encode");
    assert_eq!(decode(&payload, input.len() as u64).expect("PRS1 decode"), input);
    stats
}

fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.max(1);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(state as u8);
    }
    out
}

fn same_coarse_fingerprint_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.max(1);
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let high = ((index % 16) as u8) << 4;
        out.push(high | (state as u8 & 0x0f));
    }
    out
}

#[test]
fn exact_reference_survives_real_candidate_selection() {
    let base = deterministic_bytes(32 * 1024, 0x243f_6a88_85a3_08d3);
    let stats = encode_members(&[base.clone(), base]);
    assert!(stats.exact_ref_cells > 0, "exact-ref never survived selection: {stats:?}");
}

#[test]
fn sparse_xor_overlay_survives_real_candidate_selection() {
    let base = deterministic_bytes(32 * 1024, 0x1319_8a2e_0370_7344);
    let mut changed = base.clone();
    for position in (257..changed.len()).step_by(997) {
        changed[position] ^= 0x5a;
    }
    let stats = encode_members(&[base, changed]);
    assert!(stats.overlay_cells > 0, "overlay never survived selection: {stats:?}");
    assert!(
        stats.overlay_xor_cells > 0,
        "XOR overlay never survived replacement-vs-XOR selection: {stats:?}"
    );
}

#[test]
fn global_template_anchor_survives_beyond_recent_window() {
    let base = same_coarse_fingerprint_bytes(32 * 1024, 0x7f4a_7c15_9e37_79b9);
    let mut members = Vec::new();
    members.push(base.clone());

    // Every filler has the exact same high-nibble histogram and length as the
    // base while its low nibbles are unrelated. They therefore share the coarse
    // fingerprint and deterministically evict `base` from both 16-entry recent
    // windows without accidentally becoming sparse overlays themselves.
    for index in 0..20_u64 {
        members.push(same_coarse_fingerprint_bytes(
            32 * 1024,
            0xd1b5_4a32_d192_ed03_u64.wrapping_add(index * 0x9e37),
        ));
    }

    let mut near_copy = base;
    for position in (101..near_copy.len()).step_by(4093) {
        near_copy[position] ^= 0x05;
    }
    members.push(near_copy);

    let stats = encode_members(&members);
    assert!(
        stats.overlay_cells > 0,
        "global template was lost after the bounded recent windows: {stats:?}"
    );
}

#[test]
fn binary_combinadic_survives_real_candidate_selection() {
    // Alternate all-zero and all-one 64-byte blocks. The global byte
    // distribution is 50/50, but each enumerative block has cardinality 0 or
    // 64 and therefore rank 0. This is exactly the higher-order positional
    // structure combinadic coding is meant to expose; ordinary one-bit packing,
    // raw zero-order entropy and the short defect lattice all compete.
    let mut input = Vec::with_capacity(32 * 1024);
    for block in 0..(32 * 1024 / 64) {
        input.extend(std::iter::repeat_n(if block % 2 == 0 { 0_u8 } else { 1_u8 }, 64));
    }
    let stats = encode_one(&input);
    assert!(stats.mixture_cells > 0, "mixture never survived selection: {stats:?}");
    assert!(
        stats.mixture_combinadic_cells > 0,
        "combinadic never beat ordinary bit packing: {stats:?}"
    );
}

#[test]
fn multiaxial_representation_survives_real_candidate_selection() {
    // Even and odd positions come from disjoint two-symbol alphabets. The byte
    // marginal has four symbols, while each positional axis has only two. The
    // sequence is deliberately non-periodic so the defect lattice cannot steal
    // the case with a short period.
    let mut state = 0xd1b5_4a32_d192_ed03_u64;
    let mut input = Vec::with_capacity(32 * 1024);
    for _ in 0..(16 * 1024) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        input.push(if state & 1 == 0 { 0x10 } else { 0x20 });
        input.push(if state & 2 == 0 { 0xe1 } else { 0xf1 });
    }
    let stats = encode_one(&input);
    assert!(stats.axial_cells > 0, "axial representation never survived selection: {stats:?}");
}

#[test]
fn periodic_defect_lattice_survives_real_candidate_selection() {
    let pattern = [0x11_u8, 0x22, 0x33, 0x44];
    let mut input = (0..32 * 1024)
        .map(|index| pattern[index % pattern.len()])
        .collect::<Vec<_>>();
    for position in (503..input.len()).step_by(4093) {
        input[position] ^= 0xa5;
    }
    let stats = encode_one(&input);
    assert!(stats.defect_cells > 0, "defect lattice never survived selection: {stats:?}");
    assert!(
        stats.periodic_defect_cells > 0,
        "periodic defect mode never survived selection: {stats:?}"
    );
}

#[test]
fn delta_transition_survives_real_candidate_selection() {
    let mut input = Vec::new();
    for value in 0..128_u8 {
        input.extend(std::iter::repeat_n(value.wrapping_mul(3), 256));
    }
    let stats = encode_one(&input);
    assert!(stats.transition_cells > 0, "transition model never survived selection: {stats:?}");
    assert!(
        stats.delta_transition_cells > 0,
        "delta-state transition mode never survived selection: {stats:?}"
    );
}

#[test]
fn recursive_packing_must_split_mixed_low_cardinality_regions() {
    // Four symbols are globally "simple", but the two halves have radically
    // different structure: one constant half and one three-symbol half. A
    // global unique<=4 shortcut must not suppress the real 1/2 split, whose
    // intrinsic representation cost is materially lower.
    let half = 128 * 1024;
    let mut input = vec![b'A'; half];
    input.extend((0..half).map(|index| [b'B', b'C', b'D'][index % 3]));
    let stats = encode_one(&input);
    assert!(
        stats.cell_count > 1,
        "recursive packing was suppressed by a whole-block strong-model shortcut: {stats:?}"
    );
}
