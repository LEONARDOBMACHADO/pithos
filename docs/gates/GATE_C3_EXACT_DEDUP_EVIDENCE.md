# Gate C3 — Exact Dedup Evidence

Status: implementation complete in `feat/phase3-exact-dedup`; final validation
must now be executed **locally**. Automatic GitHub Actions validation is disabled.

The local validation runners create a timestamped directory under
`docs/gates/evidence/` containing environment information, every command output,
exit codes and a generated `SUMMARY.md`. Failing logs must be preserved exactly
as generated.

## What the developer must run

First update the local checkout and switch to the implementation branch:

```text
git fetch origin
git switch feat/phase3-exact-dedup
git pull --ff-only origin feat/phase3-exact-dedup
git status --short
git rev-parse HEAD
```

The working tree should be clean before validation.

### Windows / PowerShell

The Windows implementation lives in `scripts/validate-gate-c3-windows.ps1`.
`scripts/validate-gate-c3.ps1` is only a compatibility wrapper.

The current Windows runner is `gate-c3-windows-v4` and is intentionally written
for Windows PowerShell 5.1 compatibility.

From the repository root, first execute the fast runner self-test:

```text
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\validate-gate-c3-windows.ps1 -SelfTest
```

It must print `SELF_TEST_OK`. The v4 self-test exercises the same serialization
paths used by the full run for `environment.txt`, command metadata and
`SUMMARY.md`, in addition to UTF-8 writing and native-process capture. This is
intended to catch runner/infrastructure faults before starting the expensive Rust
validation.

Only after `SELF_TEST_OK`, run the complete Gate C3 validation:

```text
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\validate-gate-c3-windows.ps1
```

PowerShell 5.1 or newer is sufficient. PowerShell 7 is not required.

The full runner executes its self-test again before creating the validation run.
It writes `BOOTSTRAP.txt` immediately after creating the evidence directory. If
an unexpected runner exception still occurs later, it also writes
`RUNNER_FATAL.txt` with the exception type, message and PowerShell script stack
trace before exiting with code 2. Therefore a runner failure must still leave
versionable diagnostics.

The Windows runner performs the 10,000-run libFuzzer campaign with
`--sanitizer none`. This keeps Gate C3 focused on deterministic functional fuzzing
without requiring the Visual Studio C++ AddressSanitizer runtime to be installed
or added to `PATH`. MSVC AddressSanitizer remains a separate hardening validation
and is not silently reported as having run.

### Linux / macOS / bash

From the repository root:

```text
chmod +x ./scripts/validate-gate-c3.sh
./scripts/validate-gate-c3.sh
```

Run **one** runner appropriate for the machine. Do not manually repeat individual
commands unless the runner itself cannot start.

## Commands executed by the Windows runner

The Windows runner records all of the following:

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

The evidence directory also records:

```text
runner version
branch
commit SHA
OS / architecture
PowerShell version and edition
rustc --version --verbose
cargo --version
cargo-fuzz --version
cargo-llvm-cov --version
rustup toolchain list
git status --short
start/end timestamps
exit code for every command
fuzz sanitizer mode
```

Each command gets a `.log` file even when it produces no output. Native stdout
and stderr are captured directly through `System.Diagnostics.Process`, avoiding
PowerShell error-record conversion in the evidence.

## If a required local tool is missing

Do not edit code merely to make the runner continue. Record the failure first.
For missing validation tooling, install only the missing tool and rerun the whole
runner so the committed evidence comes from one complete pass:

```text
rustup toolchain install nightly
cargo install cargo-fuzz --locked
cargo install cargo-llvm-cov --locked
```

`rustfmt` and `clippy` should be available through the repository Rust toolchain.
If they are missing:

```text
rustup component add rustfmt clippy
```

## What must be committed after the run

The runner creates a directory similar to:

```text
docs/gates/evidence/gate-c3-20260807T120000Z/
```

Commit the **entire generated directory**, including logs from failed commands.
Do not delete errors, warnings, panic output, `BOOTSTRAP.txt`, `RUNNER_FATAL.txt`
or environment metadata.

```text
git status --short
git add docs/gates/evidence/
git commit -m "test: record local Gate C3 validation evidence"
git push origin feat/phase3-exact-dedup
```

Do not add unrelated local files such as `docs/gates/GATE_A_EVIDENCE.md` or
`docs/gates/GATE_B_EVIDENCE.md` to the evidence commit.

If `git add` reports that a generated evidence file is ignored, stop and record
that exact message instead of using `git add -f`; the branch is configured so
Gate evidence should be trackable normally.

## Gate C3 acceptance

The next review closes Gate C3 only when the committed evidence demonstrates:

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

If any command fails, the Gate remains open. The committed failure evidence is
the input for the next correction cycle; the developer should not make unrelated
implementation changes before that evidence is reviewed.
