# Gate C3 — Exact Dedup Evidence

Status: implementation complete in `feat/phase3-exact-dedup`; final validation
must be executed **locally**. Automatic GitHub Actions validation is disabled.

The local validation runners create timestamped directories under
`docs/gates/evidence/` containing environment information, command output, exit
codes and a generated `SUMMARY.md`. Failing logs must be preserved exactly as
generated.

## Full Windows validation

The Windows implementation lives in `scripts/validate-gate-c3-windows.ps1`.
`scripts/validate-gate-c3.ps1` is only a compatibility wrapper. The full runner is
written for Windows PowerShell 5.1 compatibility and executes:

```text
cargo fmt --all -- --check
cargo build --workspace --all-targets --all-features
cargo test -p pithos-analysis --test exact_dedup -- --nocapture
cargo test -p pithos-analysis --tests -- --nocapture
cargo test --workspace --all-targets --all-features -- --nocapture
cargo test --workspace --all-features --doc -- --nocapture
cargo clippy -p pithos-analysis --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo +nightly check --manifest-path fuzz/Cargo.toml --bin exact_dedup
cargo +nightly fuzz run --sanitizer none exact_dedup -- -runs=10000 -max_len=65536
cargo llvm-cov --workspace --all-targets --all-features --fail-under-lines 80
```

The completed Windows run at commit
`384c2ca9e1e57fde6bff58b2d75f26674a717585` produced ten successful checks. The
only failed check was the 10,000-run cargo-fuzz campaign: with MSVC,
`--sanitizer none` left SanitizerCoverage symbols unresolved at link time. This
is a fuzzing-toolchain limitation, not a Pithos test failure. Build, all tests,
doc-tests, Clippy, fuzz-target build and coverage completed successfully.

The evidence commit `47f6e9a1121a3195b9234171295f662b33c0ba3c` adds only the
full-run evidence directory; it does not change Rust source. Therefore those ten
successful results remain evidence for the same Rust implementation.

## Supplemental Windows ASan fuzz validation

Windows cargo-fuzz requires the MSVC C++ AddressSanitizer runtime. The
supplemental runner `scripts/validate-gate-c3-fuzz-windows.ps1` uses the default
AddressSanitizer configuration and automatically searches the installed Visual
Studio toolsets for `clang_rt.asan_dynamic-x86_64.dll`. When found, its directory
is prepended to the runner process `PATH` before cargo-fuzz starts.

Run from the repository root:

```text
git fetch origin
git switch feat/phase3-exact-dedup
git pull --ff-only origin feat/phase3-exact-dedup
git status --short
git rev-parse HEAD

powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; [void][scriptblock]::Create([System.IO.File]::ReadAllText('.\scripts\validate-gate-c3-fuzz-windows.ps1')); Write-Host 'PARSE_OK'"
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\validate-gate-c3-fuzz-windows.ps1
```

The supplemental runner executes:

```text
cargo +nightly fuzz run exact_dedup -- -runs=10000 -max_len=65536
```

This is a stronger Windows fuzz validation than the previous `--sanitizer none`
attempt because AddressSanitizer remains enabled.

If the runner cannot find the MSVC ASan runtime, it records exit code `125` and
the exact marker `ASAN_RUNTIME_NOT_FOUND`. Install the Visual Studio component:

```text
Microsoft.VisualStudio.Component.VC.ASAN
```

and rerun the supplemental runner. Do not edit Pithos code merely to bypass the
missing runtime.

## Linux / macOS

From the repository root:

```text
chmod +x ./scripts/validate-gate-c3.sh
./scripts/validate-gate-c3.sh
```

## Evidence policy

Commit the **entire generated evidence directory**, including logs from failed
commands. Do not delete errors, warnings, panic output, `BOOTSTRAP.txt`,
`RUNNER_FATAL.txt` or environment metadata.

```text
git status --short
git add docs/gates/evidence/
git commit -m "test: record local Gate C3 validation evidence"
git push origin feat/phase3-exact-dedup
git rev-parse HEAD
```

Do not add unrelated local files such as `docs/gates/GATE_A_EVIDENCE.md` or
`docs/gates/GATE_B_EVIDENCE.md` to an evidence commit.

## Gate C3 acceptance

Gate C3 closes when the committed evidence demonstrates:

- every beneficial exact duplicate in the versioned corpus is detected;
- different bytes are never deduplicated;
- forced compact-hash collision remains safe;
- forced full-hash collision remains safe because exact bytes are compared;
- canonical selection is deterministic;
- parallelism does not alter the plan;
- resource limits and cancellation fail closed;
- workspace build/tests/doc-tests/Clippy/format pass;
- exact-dedup fuzz completes 10,000 runs without crash or panic;
- line coverage remains at least 80%.

For the current Windows validation cycle, the ten successful results in
`gate-c3-20260807T135731Z` may be combined with a successful supplemental ASan
fuzz result because no Rust source changed between the validated implementation
and the evidence-only commit. If the supplemental fuzz passes, no repetition of
the other ten checks is required to close Gate C3.
