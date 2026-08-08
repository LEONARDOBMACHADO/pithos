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
fn binary_combinadic_survives_real_candidate_selection() {
    // 12.5% ones in a non-periodic deterministic distribution. A period-1
    // defect model is available but materially more expensive than enumerating
    // the binary set positions; ordinary 1-bit packing also competes.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut input = vec![0_u8; 32 * 1024];
    for byte in &mut input {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        if state & 7 == 0 {
            *byte = 1;
        }
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
