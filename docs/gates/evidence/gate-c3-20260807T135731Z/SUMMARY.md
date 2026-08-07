# Gate C3 local validation

- Timestamp UTC: 20260807T135731Z
- Branch: feat/phase3-exact-dedup
- Commit: 384c2ca9e1e57fde6bff58b2d75f26674a717585
- Runner: gate-c3-windows-v4
- PowerShell: 5.1.26100.8875
- PowerShell edition: Desktop
- Windows fuzz sanitizer: none (functional fuzz; MSVC ASan remains a separate hardening check)
- Result: **FAIL**

| Check | Exit code | Log |
|---|---:|---|
| 01_fmt | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/01_fmt.log |
| 02_build_workspace | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/02_build_workspace.log |
| 03_exact_dedup_test | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/03_exact_dedup_test.log |
| 04_analysis_tests | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/04_analysis_tests.log |
| 05_workspace_tests | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/05_workspace_tests.log |
| 06_doc_tests | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/06_doc_tests.log |
| 07_clippy_analysis | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/07_clippy_analysis.log |
| 08_clippy_workspace | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/08_clippy_workspace.log |
| 09_fuzz_target_build | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/09_fuzz_target_build.log |
| 10_exact_dedup_fuzz_10k | 1 | docs/gates/evidence/gate-c3-20260807T135731Z/10_exact_dedup_fuzz_10k.log |
| 11_coverage_80 | 0 | docs/gates/evidence/gate-c3-20260807T135731Z/11_coverage_80.log |

## Failures

- **10_exact_dedup_fuzz_10k** - exit code 1; preserve the corresponding log unchanged.

Do not delete failing logs. Commit this entire evidence directory so the next review can reproduce the failure context.
