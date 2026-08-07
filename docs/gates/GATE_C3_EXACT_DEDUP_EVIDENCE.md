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

From the repository root:

```text
powershell -ExecutionPolicy Bypass -File .\scripts\validate-gate-c3.ps1
```

If using PowerShell 7:

```text
pwsh -File .\scripts\validate-gate-c3.ps1
```

### Linux / macOS / bash

From the repository root:

```text
chmod +x ./scripts/validate-gate-c3.sh
./scripts/validate-gate-c3.sh
```

Run **one** runner appropriate for the machine. Do not manually repeat individual
commands unless the runner itself cannot start.

## Commands executed by the runner

The runner records all of the following:

```text
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo test -p pithos-analysis --test exact_dedup -- --nocapture
cargo test -p pithos-analysis --tests -- --nocapture
cargo test --workspace --all-targets -- --nocapture
cargo clippy -p pithos-analysis --all-targets -- -D warnings
cargo clippy --workspace --all-targets -- -D warnings
cargo +nightly check --manifest-path fuzz/Cargo.toml --bin exact_dedup
cargo +nightly fuzz run exact_dedup -- -runs=10000 -max_len=65536
cargo llvm-cov --workspace --all-targets --fail-under-lines 80
```

The evidence directory also records:

```text
branch
commit SHA
OS / architecture
rustc --version --verbose
cargo --version
git status --short
start/end timestamps
exit code for every command
```

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
Do not delete errors, warnings, panic output or environment metadata.

```text
git status --short
git add docs/gates/evidence/
git commit -m "test: record local Gate C3 validation evidence"
git push origin feat/phase3-exact-dedup
```

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
- workspace build/tests/Clippy/format pass;
- exact-dedup fuzz completes 10,000 runs without crash or panic;
- line coverage remains at least 80%.

If any command fails, the Gate remains open. The committed failure evidence is
the input for the next correction cycle; the developer should not make unrelated
implementation changes before that evidence is reviewed.
