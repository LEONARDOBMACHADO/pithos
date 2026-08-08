# Pithos R3 — Representation-first compression

R3 treats entropy codecs as terminal engines rather than the product. The archive is first reduced to a reversible symbolic representation, then compressed.

## Layers

1. **Representation** — canonical chunks, compact references, motifs/codebooks, deterministic transforms.
2. **Planning** — content-class routing and benefit/cost gates. No transform is accepted unless its final payload beats the fallback.
3. **Entropy** — Zstd/Brotli/LZMA2 and future coders compete only on already-reduced representations.
4. **Agent surface** — deterministic metadata, content addressing, partial access and explainable planning decisions.

## Hard invariants

- lossless byte-for-byte reconstruction;
- deterministic output for the same inputs/profile/toolchain;
- fail closed on malformed metadata;
- no global expensive transform without a cheap prescreen;
- every experimental transform has a smaller-payload fallback;
- `.pits` remains the public extension;
- no paid API or cloud dependency.

## R3 sequence

- 23: compact symbolic native references (varint + run tokens + inner entropy arbitration).
- 24: parallel verify/unpack across independent solid groups.
- 25: archive-level canonical pool experiment.
- 26: shortmer-inspired motif codebook with final-size arbitration.
- 27: implicit/seeded reference representation with explicit exception fallback.
- 28: quaternary context representation experiment.
- 29: global planner choosing a Pareto-optimal representation under size/CPU/decode budgets.
