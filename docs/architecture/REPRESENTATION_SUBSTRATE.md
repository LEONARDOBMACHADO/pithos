# Pithos Representation Substrate (PRS1)

This document defines the first representation-first substrate for Pithos. It is intentionally not another historical native wrapper version.

## Goal

Transform source bytes into the cheapest reversible representation before selecting an entropy backend. Standard codecs (Store/Zstd/Brotli/LZMA2) and the historical native stack remain competitors, not architectural parents.

## Current architecture

```text
source group
    |
    v
member-bounded recursive partitioner
    |
    +--> exact shared reference
    +--> template + sparse overlay
    +--> low-cardinality mixture/index representation
    +--> multiaxial nibble planes
    +--> sparse defect lattice
    +--> transition/run representation
    +--> raw fallback
             |
             v
orthogonal shared planes
 descriptor | raw | overlay | mixture | axis-hi | axis-lo | defect | transition
             |
             v
per-plane Store / Zstd / bounded LZMA2 arbitration
             |
             v
complete PRS1 payload
             |
             v
experimental group-level race: PRS1 vs v17 vs v12, then native winner vs standard
```

## Integrated representation model

PRS1 combines eight research directions in one bounded deterministic planner:

1. **Multi-Granular Recursive Packing** — recursively partitions member-bounded regions from MiB scale toward KiB scale only when quarter-region feature vectors show statistical heterogeneity. A strong single model prevents unnecessary splitting. Current hard bounds: 4 KiB minimum cell, 1 MiB maximum initial cell, recursion depth 8 and 1,000,000 cells.
2. **Global Template + Sparse Overlay** — earlier cells are reusable templates. Full BLAKE3 plus byte equality provides exact references. A bounded coarse histogram index locates recent same-shape templates; sparse changes are stored as gap-varints plus replacement values only when cheaper.
3. **Set + Permutation / Combinatorial Mixture** — current implementation handles low-cardinality cells with a 2–16 byte alphabet plus a 1–4 bit packed symbol-index stream. This is a concrete mixture representation, not yet a full enumerative combinatorial-rank coder; that stronger variant remains a future experiment.
4. **Multiaxial Representation** — bytes can be transposed into high-nibble and low-nibble planes. Selection is based on the estimated entropy of the separated axes rather than an assumption that base conversion itself compresses data.
5. **Orthogonal Representation Multiplexing** — descriptor, literal, overlay, mixture, axis, defect and transition data from many cells live in shared planes. This is the primary holographic-style software analogy: independent logical channels share one substrate but retain separate statistical contexts.
6. **Sparse Defect Lattice** — when one value occupies at least 70% of a cell, that value becomes the implicit lattice default and only defect gaps plus values are materialized.
7. **Topological Transition / Racetrack Coding** — repeated states are represented as state plus run distance instead of repeated values. The representation is retained only when its materialized stream is cheaper.
8. **Shared Side Information** — standalone `.pits` uses only internal, previously decoded cells as side information. The architecture reserves externally resolved content-addressed references for Atlas/agent-native deployments, but PRS1 standalone payloads never depend on external state.

## Synergy rather than wrappers

The eight ideas are not applied serially. They cooperate in four layers:

1. **Where:** recursive packing chooses the decision granularity.
2. **How:** exact-ref, overlay, mixture, multiaxial, defect, transition and raw compete for each leaf.
3. **Across leaves:** selected material is multiplexed into long homogeneous planes, allowing many leaves to share one entropy context.
4. **Final cost:** every plane chooses an entropy backend, then the complete PRS1 payload competes against the complete historical representations.

This explicitly avoids a `v19 -> v20 -> v21` transform cascade.

## PRS1 wire format

Current experimental payload magic: `PRS1`.

The payload contains:

- fixed 24-byte PRS1 header;
- exactly eight fixed plane records;
- each record stores plane id, entropy codec id, decoded plane length, encoded plane length and CRC32C;
- encoded planes concatenated in deterministic id order.

The decoder validates magic, version, declared original length, cell count, plane identities, CRCs, raw/encoded size bounds and exact plane consumption before returning the group bytes.

## Entropy policy

- **Store** is always the baseline.
- **Zstd** is evaluated for every non-empty plane.
- **LZMA2** is evaluated only for transformed planes between 64 KiB and 64 MiB.
- The raw plane deliberately does not rerun LZMA2 because the standard full-group path already provides that expensive candidate; duplicating it inside PRS1 adds CPU without creating a new representation.

This makes entropy codecs leaf engines rather than product identity.

## Cost and memory policy

The current compatibility selector races complete `v17`, `v12` and PRS1 candidates.

- Groups up to 128 MiB may use a three-way parallel race.
- Above 128 MiB, `v17` and `v12` finish first and PRS1 runs separately, bounding peak memory rather than holding three large representation worksets concurrently.
- The PRS1 decoder hard-limits cell count, per-plane decoded length and aggregate plane decoded length before allocation.
- PRS1 candidate failure does not invalidate historical native compression; it is treated as an unavailable candidate and the older representations remain eligible.

## Observability

`PITHOS_REP_TRACE=1` records:

- race mode (`parallel` or `bounded-sequential`);
- v17, v12 and PRS1 complete payload bytes and elapsed time;
- winning representation;
- PRS1 cell count and counts selected as raw, exact reference, sparse overlay, mixture, axial, defect and transition.

This is required so representation techniques can be retained or removed based on evidence rather than intuition.

## PAF / compatibility strategy

PRS1 is an independent crate: `pithos-representation-substrate`.

During this experiment, PRS1 is transported through native codec v18 only as a compatibility dispatch:

- encoder v18 may select a complete PRS1 payload;
- decoder v18 detects `PRS1` magic and delegates to the independent substrate decoder;
- PAF tables, RestoreMap and the current codec registry do not change during the experiment.

`SUBSTRATE_CODEC_ID = 5` and substrate version 1 are reserved for a future explicit registry promotion **only if PRS1 proves itself empirically**. Promotion will require a deliberate format/compatibility migration instead of silently redefining existing native identity.

## Decoder contract

PRS1 reconstructs the exact concatenated group bytes. The existing RestoreMap restores individual files, so the representation experiment does not alter file restoration semantics.

## Hard rules

- Deterministic and byte-exact.
- Member boundaries are respected by recursive partitioning.
- Exact references require BLAKE3 identity plus byte equality.
- Candidate generation is bounded.
- External side information is never required to decode standalone `.pits`.
- No physical-density or quantum-compression claim is made.
- A representation is useful only if the final complete payload beats the competing representation after all metadata and entropy coding.

## Research lineage

The design is inspired by universal-template/epigenetic DNA storage, multidimensional optical storage, multiplexed holographic channels, atomic-vacancy lattices, multistate/racetrack memory and shared-side-information ideas from quantum communication. PRS1 uses only reversible classical algorithms; the physical systems are inspiration for representation structure, not evidence of impossible compression gains.
