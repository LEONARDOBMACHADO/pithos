# Pithos Representation Substrate (PRS1)

This document defines the representation-first substrate used by the Pithos compression research branch. PRS1 is intentionally not another historical native wrapper version.

## Goal

Transform source bytes into the cheapest reversible representation before selecting an entropy backend. Store, Zstd, LZMA2 and the historical native stack are competitors or leaf engines, not the architecture itself.

## Implemented architecture

```text
source group
    |
    v
member-bounded multi-granular recursive packing
(cost-driven split at 1/4, 1/2 or 3/4)
    |
    +--> exact shared reference
    +--> template + sparse replacement/XOR overlay
    +--> low-cardinality bit-pack/combinadic mixture
    +--> adaptive multiaxial representation
    |      +--> nibble planes
    |      +--> XOR-delta nibble planes
    |      +--> even/odd positional planes
    +--> sparse periodic defect lattice (period 1/2/4/8)
    +--> transition/racetrack representation
    |      +--> absolute state
    |      +--> delta state
    +--> raw fallback
             |
             v
orthogonal shared planes
 descriptor | raw | overlay | mixture | axis-A | axis-B | defect | transition
             |
             v
per-plane Store / Zstd / bounded LZMA2 arbitration
             |
             v
complete PRS1 payload
             |
             v
experimental group race: PRS1 vs v17 vs v12
             |
             v
standard/native complete-archive planner
```

## How the research ideas cooperate

The design deliberately combines the storage analogies instead of applying them as serial wrappers.

### 1. Multi-Granular Recursive Packing — the original "box" idea

The partitioner respects member boundaries and operates from MiB scale toward KiB scale. A region is not split merely because its local statistics look different. PRS1 evaluates candidate cut points near 1/4, 1/2 and 3/4 and estimates the cheapest intrinsic representation on each side. It recurses only when the two smaller regions plus split overhead beat the whole region by a minimum margin.

This is the software form of filling the large box first and then looking for smaller spaces that expose structure hidden at the larger scale.

Current bounds:

- 4 KiB minimum cell;
- 1 MiB maximum initial cell;
- recursion depth 8;
- 1,000,000 cell hard decoder/encoder limit;
- split evaluation begins at 64 KiB.

### 2. Global Template + Sparse Overlay — epigenetic DNA analogy

Earlier decoded cells become internal side information. Exact reuse requires BLAKE3 identity plus byte equality. For same-length cells, PRS1 also searches bounded recent templates using both a coarse statistical fingerprint and a same-length window.

A sparse overlay stores gap-varint positions and one residual byte per changed position. Two residual semantics compete:

- replacement value;
- XOR against the template value.

The lower estimated-entropy residual wins. This lets near-identical cells share one template while materialising only their differences.

### 3. Set / Combinatorial Mixture

For cells containing 2–16 distinct symbols, PRS1 constructs an explicit alphabet and a compact index stream.

Implemented modes:

- 1–4 bit fixed symbol indexes for alphabets up to 16 symbols;
- binary combinadic encoding in independent blocks of at most 64 symbols.

The binary combinadic form stores the number of occurrences of symbol 1 plus the combinatorial rank of their positions. It competes against ordinary one-bit packing and is selected only when its materialized representation is smaller.

This is the first true enumerative/combinatorial representation in Pithos rather than a base conversion.

### 4. Adaptive Multiaxial Representation — 5D optical-storage analogy

A byte stream can be projected into different logical axes before entropy coding. PRS1 currently races three reversible decompositions:

- high-nibble / low-nibble planes;
- previous-byte XOR delta followed by high/low nibble planes;
- even-position / odd-position byte planes.

The mode with the lowest combined estimated entropy plus metadata cost wins for that cell. The axes from many cells are then multiplexed into the same global axis planes.

This is materially different from the old fixed quaternary transform: representation dimensionality is selected adaptively and competes against all other cell models.

### 5. Orthogonal Representation Multiplexing — holographic analogy

Selected cell material is not entropy-coded cell-by-cell. Similar logical material from all leaves is accumulated into eight homogeneous planes:

1. descriptor;
2. raw literals;
3. overlays;
4. mixture/combinatorial payloads;
5. axis A;
6. axis B;
7. defect payloads;
8. transition payloads.

Each plane receives one shared entropy context. This is the core synergy between the individual models: recursive packing discovers structure locally, while multiplexing lets many local winners share a long global statistical context.

### 6. Sparse Defect Lattice — atomic vacancy analogy

PRS1 searches implicit periodic lattices with periods 1, 2, 4 and 8. For each residue position it derives the modal byte, then chooses the pattern producing the largest number of matches.

If at least 70% of the cell follows the periodic lattice, only gap-varint defect positions and replacement bytes are stored. A perfect lattice is valid and can be represented by its pattern with zero defect payload bytes.

This generalises the old single-default-byte model to short implicit structures.

### 7. Topological Transition / Racetrack Coding

Long runs are represented as state plus run distance. PRS1 constructs two state streams:

- absolute state value;
- first absolute state followed by wrapping deltas between successive states.

The lower estimated-entropy stream wins. This is useful when run values themselves move predictably even when the run lengths vary.

### 8. Shared Side Information — quantum communication analogy

Standalone `.pits` remains fully self-contained. Its side information is limited to cells already encoded inside the same PRS1 payload: exact references and template overlays.

The architecture deliberately leaves external content-addressed side information for Atlas/agent-native deployments. An external-reference mode must be explicit because it changes the decode contract: it cannot silently enter standalone PRS1 archives. The current branch therefore implements the reusable internal mechanism but does not make `.pits` dependent on Atlas.

## Cost planner

PRS1 has two planning levels.

### Cell-level planning

For each leaf, raw, exact-ref, overlay, mixture, defect, transition and multiaxial representations compete. Candidate scoring includes representation metadata and an entropy estimate of the material that will enter the shared plane.

### Final physical planning

Cell winners are multiplexed into planes. Every non-empty plane races Store and Zstd. Bounded transformed planes from 64 KiB through 64 MiB additionally race LZMA2. The raw plane deliberately does not rerun LZMA2 because the complete standard/native path already supplies that expensive candidate.

Finally the complete PRS1 payload competes against complete v17 and v12 payloads. A local representation is therefore not considered a product win until the complete physical payload wins.

## Cost and memory policy

The current compatibility selector races complete v17, v12 and PRS1 candidates.

- groups up to 128 MiB may use a three-way parallel race;
- above 128 MiB, v17 and v12 complete first and PRS1 runs separately;
- PRS1 caps recursive depth and cell count;
- decoder plane lengths are validated before allocation;
- aggregate decoded-plane bytes are bounded;
- malformed modes, periods, combinadic ranks, CRCs and trailing bytes fail closed;
- PRS1 failure makes that candidate unavailable without invalidating the historical candidates.

## Observability

`PITHOS_REP_TRACE=1` records complete representation races and PRS1 cell counts. The R5 runner is expected to retain both family counts and sub-mode counts:

- raw;
- exact reference;
- overlay total / XOR overlay;
- mixture total / combinadic mixture;
- axial total / XOR-nibble / even-odd;
- defect total / periodic defect;
- transition total / delta transition.

This allows each physical-storage inspiration to be kept or removed based on measured wins rather than intuition.

## Wire format and compatibility

Experimental payload magic remains `PRS1`. Internal format version is currently `2`.

The envelope remains deliberately stable in shape:

- fixed 24-byte PRS1 header;
- exactly eight fixed plane records;
- each record stores plane id, entropy codec id, decoded plane length, encoded plane length and CRC32C;
- encoded planes are concatenated in deterministic id order.

Version 2 adds representation mode bytes inside the descriptor plane for overlay, mixture, axial and transition models and stores the defect-lattice period/pattern in the descriptor.

During this experiment PRS1 is transported through native codec v18 only as a compatibility dispatch. v18 detects the `PRS1` magic on decode and delegates to the independent substrate decoder. PAF tables, RestoreMap and the current registry do not change during the experiment.

`SUBSTRATE_CODEC_ID = 5` is reserved for an explicit future registry promotion only if PRS1 proves itself empirically. Promotion requires a deliberate format/compatibility migration.

## Decoder contract

PRS1 reconstructs the exact concatenated group bytes. Existing RestoreMap handling restores individual files, so the representation experiment does not change path, hardlink, symlink or file restoration semantics.

## Hard rules

- deterministic and byte-exact;
- member boundaries respected;
- exact references require full BLAKE3 plus byte equality;
- candidate search is bounded;
- no required external state for standalone `.pits`;
- no claim that DNA, glass, holographic, atomic, racetrack or quantum physics themselves compress classical files;
- physical technologies are used only as inspiration for reversible representation structures;
- every representation must justify itself at final physical payload level.

## Architectural direction

The intended endpoint is not `v19 -> v20 -> v21`. If empirical results validate PRS1 concepts, the winning representations will be consolidated into one representation substrate and the historical native versions will become compatibility/floor implementations rather than the identity of the compressor.