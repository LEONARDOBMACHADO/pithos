# Gate C3 — Exact Dedup Evidence

**Status: CLOSED / PASS.**

Gate C3 is closed for the exact-dedup implementation on
`feat/phase3-exact-dedup`. Automatic GitHub Actions validation is disabled; all
acceptance evidence was produced locally and committed under
`docs/gates/evidence/`.

## Evidence set

The acceptance decision combines two local Windows evidence runs:

1. `docs/gates/evidence/gate-c3-20260807T135731Z/`
   - implementation commit: `384c2ca9e1e57fde6bff58b2d75f26674a717585`;
   - 10 of the 11 checks passed;
   - the only failure was the Windows `--sanitizer none` cargo-fuzz link step,
     which left SanitizerCoverage symbols unresolved under MSVC;
   - format, workspace build, exact-dedup tests, analysis tests, workspace tests,
     doc-tests, analysis/workspace Clippy, fuzz-target build and coverage all
     passed.
2. `docs/gates/evidence/gate-c3-fuzz-20260807T152429Z/`
   - implementation commit: `cf201827df7774455d610b754bba2eef0de1caf4`;
   - runner: `gate-c3-windows-asan-fuzz-v1`;
   - MSVC AddressSanitizer runtime was located and loaded;
   - `cargo +nightly fuzz run exact_dedup -- -runs=10000 -max_len=65536` ended
     with exit code `0`.

The evidence-only commit
`898cd785965abb7d21a528603fe0a45f27b51336` records the supplemental fuzz result
and the first, unsuccessful benchmark-corpus attempt.

A repository compare between `384c2ca...` and `cf201827...` contains no changes
under `crates/pithos-analysis/` or the exact-dedup fuzz target. The intervening
changes are benchmark/telemetry/CLI/docs/runners and committed evidence. The
supplemental ASan result therefore validates the same exact-dedup implementation
that passed the other ten checks.

## Acceptance checks

Gate C3 acceptance requires:

- every beneficial exact duplicate in the versioned corpus is detected;
- different bytes are never deduplicated;
- forced compact-hash collision remains safe;
- forced full-hash collision remains safe because exact bytes are compared;
- canonical selection is deterministic;
- parallelism does not alter the plan;
- resource limits and cancellation fail closed;
- workspace build/tests/doc-tests/Clippy/format pass for the validated
  implementation;
- exact-dedup fuzz completes 10,000 runs without crash or panic;
- line coverage remains at least 80%.

All of these requirements are satisfied by the combined evidence above. The
supplemental fuzz metadata explicitly records `exit_code=0` for the 10,000-run
ASan campaign.

## Windows validation implementation

The full Windows runner remains at:

```text
scripts/validate-gate-c3-windows.ps1
```

and the dedicated MSVC AddressSanitizer runner remains at:

```text
scripts/validate-gate-c3-fuzz-windows.ps1
```

The ASan runner searches installed Visual Studio toolsets for:

```text
clang_rt.asan_dynamic-x86_64.dll
```

and prepends the matching directory to the runner process `PATH`. If the runtime
is missing it records `ASAN_RUNTIME_NOT_FOUND` rather than misclassifying the
condition as a Pithos failure.

## Linux / macOS regression runner

The cross-platform regression runner remains available at:

```text
scripts/validate-gate-c3.sh
```

Gate C3 does not need to be rerun merely because tooling, benchmark code,
documentation or the public filename extension changes. It must be reopened if
`pithos-analysis` exact-dedup behavior, its collision/equality contract, the fuzz
target, or the persisted dedup representation changes in a way that can affect
C3 semantics.

## Next boundary

Closing C3 authorizes the next Phase 3 implementation boundary: persist the
validated exact-dedup plan physically in PAF through an explicit `ChunkTable`
while preserving versioning/backwards compatibility and keeping `LogicalChunk`,
`RestoreMap` and `GroupTable` as separate relations.

The benchmark/telemetry tooling introduced before that physical integration is
used only to freeze a baseline and quantify the before/after effect. The
format-neutral `net_saved_bytes` reported today remains a **potential** saving
until the `ChunkTable` makes it observable in the physical `.pits` archive size.

## Evidence policy

Generated Gate evidence remains immutable. Do not edit historical logs merely to
match later code or naming changes. Future regressions receive a new timestamped
evidence directory.

Do not add unrelated local files such as `docs/gates/GATE_A_EVIDENCE.md` or
`docs/gates/GATE_B_EVIDENCE.md` to Gate evidence commits.
