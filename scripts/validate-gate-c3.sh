#!/usr/bin/env bash
set -u

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
evidence_dir="$repo/docs/gates/evidence/gate-c3-$timestamp"
mkdir -p "$evidence_dir"

summary_file="$evidence_dir/SUMMARY.md"
environment_file="$evidence_dir/environment.txt"
failed=0

declare -a summary_rows=()

run_check() {
  local name="$1"
  shift
  local log="$evidence_dir/${name}.log"
  local meta="$evidence_dir/${name}.meta.txt"
  local started ended exit_code

  echo
  echo "=== $name ==="
  printf 'Command:'
  printf ' %q' "$@"
  echo

  started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  set +e
  "$@" > >(tee "$log") 2> >(tee -a "$log" >&2)
  exit_code=$?
  set -e
  ended="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

  {
    echo "name=$name"
    printf 'command='
    printf '%q ' "$@"
    echo
    echo "started_utc=$started"
    echo "ended_utc=$ended"
    echo "exit_code=$exit_code"
  } > "$meta"

  summary_rows+=("| $name | $exit_code | \`${log#$repo/}\` |")
  if [[ $exit_code -ne 0 ]]; then
    failed=1
  fi
}

{
  echo "timestamp_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "repo=$repo"
  echo "branch=$(git branch --show-current 2>&1)"
  echo "commit=$(git rev-parse HEAD 2>&1)"
  echo "uname=$(uname -a 2>&1)"
  echo "gate_c3_fuzz_sanitizer=address-default"
  echo
  echo 'rustc --version --verbose:'
  rustc --version --verbose 2>&1 || true
  echo
  echo 'cargo --version:'
  cargo --version 2>&1 || true
  echo
  echo 'rustup toolchain list:'
  rustup toolchain list 2>&1 || true
  echo
  echo 'git status --short:'
  git status --short 2>&1 || true
} > "$environment_file"

set -e
run_check 01_fmt cargo fmt --all -- --check
run_check 02_build_workspace cargo build --workspace --all-targets --all-features
run_check 03_exact_dedup_test cargo test -p pithos-analysis --test exact_dedup -- --nocapture
run_check 04_analysis_tests cargo test -p pithos-analysis --tests -- --nocapture
run_check 05_workspace_tests cargo test --workspace --all-targets --all-features -- --nocapture
run_check 06_doc_tests cargo test --workspace --all-features --doc -- --nocapture
run_check 07_clippy_analysis cargo clippy -p pithos-analysis --all-targets -- -D warnings
run_check 08_clippy_workspace cargo clippy --workspace --all-targets --all-features -- -D warnings
run_check 09_fuzz_target_build cargo +nightly check --manifest-path fuzz/Cargo.toml --bin exact_dedup
run_check 10_exact_dedup_fuzz_10k cargo +nightly fuzz run exact_dedup -- -runs=10000 -max_len=65536
run_check 11_coverage_80 cargo llvm-cov --workspace --all-targets --all-features --fail-under-lines 80

{
  echo '# Gate C3 local validation'
  echo
  echo "- Timestamp UTC: \`$timestamp\`"
  echo "- Branch: \`$(git branch --show-current)\`"
  echo "- Commit: \`$(git rev-parse HEAD)\`"
  echo '- Fuzz sanitizer: default cargo-fuzz AddressSanitizer'
  if [[ $failed -eq 0 ]]; then
    echo '- Result: **PASS**'
  else
    echo '- Result: **FAIL**'
  fi
  echo
  echo '| Check | Exit code | Log |'
  echo '|---|---:|---|'
  printf '%s\n' "${summary_rows[@]}"
  echo
  if [[ $failed -eq 0 ]]; then
    echo 'No command failed.'
  else
    echo 'One or more checks failed. Preserve every generated log unchanged.'
  fi
  echo
  echo 'Do not delete failing logs. Commit this entire evidence directory so the next review can reproduce the failure context.'
} > "$summary_file"

echo
echo "Evidence written to: $evidence_dir"
echo "Summary: $summary_file"

exit "$failed"
