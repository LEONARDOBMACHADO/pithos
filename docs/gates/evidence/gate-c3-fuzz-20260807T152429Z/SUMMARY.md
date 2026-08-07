# Gate C3 Windows ASan fuzz validation

- Timestamp UTC: 20260807T152429Z
- Branch: feat/phase3-exact-dedup
- Commit: cf201827df7774455d610b754bba2eef0de1caf4
- Runner: gate-c3-windows-asan-fuzz-v1
- Sanitizer: AddressSanitizer
- ASan runtime: C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\clang_rt.asan_dynamic-x86_64.dll
- Result: **PASS**

| Check | Exit code | Log |
|---|---:|---|
| exact_dedup_fuzz_10k_asan | 0 | docs/gates/evidence/gate-c3-fuzz-20260807T152429Z/exact_dedup_fuzz_10k_asan.log |

This is supplemental evidence for the full Gate C3 run. Rust source is unchanged from the previously validated Gate C3 commit.
