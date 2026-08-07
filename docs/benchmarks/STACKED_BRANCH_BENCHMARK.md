# Pithos stacked branch benchmark protocol

This protocol freezes the development benchmark strategy after baseline commit
`95d4d0623861661e2ca2a71ee304bd5e8726c21f`.

The stack measures the incremental effect of each Pithos technique. Branches are
cumulative: every branch starts from the previous implementation checkpoint.
After the branch-04 Rust ownership failure discovered on 2026-08-07, branches
05-15 were intentionally rebuilt on top of the corrected predecessor so fixes
are inherited instead of copied into divergent snapshots.

## Frozen reference

Baseline corpus: 31 files, 476.19 MiB, 25 extensions.

| Compressor | Archive | Compress | Decompress |
|---|---:|---:|---:|
| 7-Zip LZMA2 mx9 | 82.42 MiB | 66.6 s | 5.3 s |
| Pithos archive-max | 84.41 MiB | 248.2 s | 275.2 s |

Intermediate development comparisons use only **Pithos `archive-max` vs 7-Zip
LZMA2 mx9 solid**. WinRAR/WinZip remain excluded until the final release benchmark.

## Mandatory branch order

| Step | Branch | Increment being measured |
|---:|---|---|
| 01 | `perf/01-group-decode-once` | Decode each solid group once |
| 02 | `perf/02-adaptive-pack` | Sample-based codec selection and real archive-max levels |
| 03 | `feat/03-native-exact-dedup` | Physical reversible FastCDC exact dedup |
| 04 | `feat/04-native-similarity-delta` | Sparse and splice similarity deltas |
| 05 | `feat/05-native-reference-graph` | Bounded multi-base range-copy reference graph |
| 06 | `feat/06-native-canonicalization` | Reversible text/JSON canonicalization |
| 07 | `feat/07-native-recompression` | Exact GZIP and PNG/IDAT recompression modelling |
| 08 | `feat/08-native-grammar-residual` | Grammar/RLE/copy residual recursion |
| 09 | `feat/09-native-synthetic-math` | Synthetic bases and arithmetic byte rules |
| 10 | `feat/10-native-nested-deflate` | ZIP/Office/PDF nested DEFLATE modelling |
| 11 | `perf/11-direct-native-pack` | Remove full compressed intermediate |
| 12 | `perf/12-fused-native-selector` | Fused native selector |
| 13 | `perf/13-prescreen-parallel-pack` | Prescreen plus parallel close-call evaluation |
| 14 | `feat/14-native-cluster-reorder` | Content-class clustering/reordering |
| 15 | `perf/15-parallel-clusters` | Parallel global-vs-clustered candidate evaluation |

Do not skip forward after a failure.

## Rebuilt-stack synchronization

The orchestrator fetches origin once, verifies a clean worktree, then uses:

```powershell
git switch -C <BRANCH> origin/<BRANCH>
```

This is deliberate. Branches 05-15 were rewritten as a true cumulative stack,
so stale local branch pointers must not be used as the test source. The remote
branch is authoritative. The command is run only by the orchestrator after the
worktree is confirmed clean. Local `GATE_A_EVIDENCE.md` and
`GATE_B_EVIDENCE.md` remain untracked and must never be staged or deleted.

## Blocking gate per branch

```powershell
cargo fmt --all
cargo fmt --all -- --check
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-targets --all-features -- --nocapture
cargo test --workspace --all-features --doc -- --nocapture
cargo build --release -p pithos-cli
```

Any non-zero exit code stops the sequence. The developer must not repair source.
Clippy, >=80% coverage and fuzzing are deferred until technique selection.

## Benchmark gate

Before every benchmark only generated work is removed:

```powershell
Remove-Item "$repo\tst_compact\results\stack-work" -Recurse -Force -ErrorAction SilentlyContinue
```

The runner then executes Pithos archive-max pack/verify/unpack and exact SHA-256
tree comparison, followed by 7-Zip mx9 solid pack/test/extract and the same tree
comparison. It writes versioned evidence under `docs/benchmarks/evidence/` and,
when requested, an external evidence copy. It never invokes WinRAR or WinZip.

## Failure protocol

On the first failure:

1. stop immediately;
2. do not test later branches;
3. do not edit Rust or PowerShell locally;
4. preserve existing evidence;
5. return `STACK_SEQUENCE_FAILURE.txt`, the failing branch log,
   `git rev-parse HEAD`, and `git status --short`.

## Resuming

Already validated branches do not need to be repeated. Use the orchestrator's
`-StartAtBranch` parameter, for example:

```powershell
-StartAtBranch 'feat/04-native-similarity-delta'
```

The summary records the requested start branch and number of branches actually
run.

## Final hardening after stack selection

After selecting the winning techniques, consolidate them and restore:

- strict workspace Clippy with `-D warnings`;
- workspace line coverage >=80%;
- fuzzing for changed native decoders/transforms;
- malformed/collision/resource-bound tests;
- full individual-file and combined benchmarks;
- final Pithos vs 7-Zip and WinRAR comparison.
