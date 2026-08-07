# Pithos baseline local runbook

Este procedimento é a bateria local oficial para validar a migração definitiva
para `.pits` e congelar o primeiro baseline real de compactação em
`tst_compact/`.

**Gate C3 já está CLOSED / PASS.** Não repetir a campanha ASan de 10.000 runs
nesta execução. A evidência está preservada em
`docs/gates/evidence/gate-c3-fuzz-20260807T152429Z/`.

Não corrija lógica/código manualmente durante a execução. Se uma etapa falhar,
preserve o erro e a evidência gerada para revisão. O corpus bruto e os artefatos
grandes de benchmark são locais e não entram no Git.

## 1. Atualizar o branch

```powershell
git fetch origin
git switch feat/phase3-exact-dedup
git pull --ff-only origin feat/phase3-exact-dedup
git rev-parse HEAD
git status --short
```

Arquivos locais não relacionados `docs/gates/GATE_A_EVIDENCE.md` e
`docs/gates/GATE_B_EVIDENCE.md`, se existirem, devem permanecer fora dos commits.

## 2. Parse-check dos runners PowerShell

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; @('.\scripts\fetch-tst-compact-samples.ps1','.\scripts\inventory-tst-compact.ps1','.\scripts\run-tst-compact-bench.ps1','.\scripts\summarize-tst-compact.ps1') | ForEach-Object { [void][scriptblock]::Create([System.IO.File]::ReadAllText($_)); Write-Host ('PARSE_OK ' + $_) }"
```

## 3. Formatação determinística

A rodada anterior mostrou diferenças de `rustfmt` nos novos crates. Nesta rodada
está **explicitamente autorizado somente** executar o formatador oficial Rust;
não fazer nenhuma alteração manual de lógica:

```powershell
cargo fmt --all
cargo fmt --all -- --check
```

Preserve qualquer alteração produzida exclusivamente por `cargo fmt`. Todos os
demais fixes de código continuam sendo responsabilidade do branch, não do dev.

## 4. Qualidade e compilação do workspace

```powershell
cargo build --workspace --all-targets --all-features
cargo test -p pithos-telemetry --all-targets -- --nocapture
cargo test -p pithos-bench --all-targets -- --nocapture
cargo test -p pithos-cli --test default_pits -- --nocapture
cargo test --workspace --all-targets --all-features -- --nocapture
cargo test --workspace --all-features --doc -- --nocapture
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo llvm-cov --workspace --all-targets --all-features --fail-under-lines 80
```

A rodada anterior já atualizou `Cargo.lock` com `pithos-bench` e
`pithos-telemetry`. Preserve o lockfile; não o edite manualmente.

## 5. Verificar a extensão definitiva `.pits`

```powershell
cargo run -p pithos-cli --bin pithos -- capabilities
cargo run -p pithos-cli --bin pithos -- --output-format json capabilities
```

O output standalone deve anunciar `.pits` como extensão pública. Os testes
`default_pits` verificam:

- um input: `report.pdf` → `report.pdf.pits`;
- múltiplos inputs: `files.pits`;
- leitura dos aliases históricos `.phs`, `.pts` e `.pithos`.

## 6. Preparar comparadores externos

O ambiente anterior já detectou:

- 7-Zip em `C:\Program Files\7-Zip\7z.exe`;
- WinRAR em `C:\Program Files\WinRAR\WinRAR.exe`;
- WinZip CLI não encontrada.

Não é necessário instalar WinZip para esta rodada. O benchmark deve registrar
`NOT_FOUND` e continuar com Pithos, 7-Zip e WinRAR.

## 7. Baixar o corpus da internet

A rodada anterior não baixou nenhum arquivo devido a um bug de binding de
coleção no PowerShell 5.1. O downloader foi endurecido para aceitar arrays,
wrappers comuns da API, respostas vazias e fallback para o endpoint random.

Execute:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\fetch-tst-compact-samples.ps1
```

Depois confira:

```powershell
Get-Content .\tst_compact\results\source-register.csv
Get-Content .\tst_compact\results\download-missing.txt
```

Formatos indisponíveis devem ser registrados, não convertidos em falha do runner.
Não substitua lacunas por downloads manuais nesta execução.

## 8. Congelar o inventário

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\inventory-tst-compact.ps1
Get-Content .\tst_compact\results\corpus-summary.txt
```

O inventário agora é empty-safe, mas **não execute os benchmarks se
`file_count=0`**. Nesse caso preserve `source-register.csv`,
`download-missing.txt` e `corpus-summary.txt`, faça o commit de evidência e pare.

O alvo desejado continua aproximadamente 850–1050 MiB; se a fonte não oferecer
fixtures próximos dos targets, aceite o corpus menor e preserve o tamanho real no
manifesto. Não invente bytes para atingir a meta.

## 9. Executar a bateria de compactação

Se `file_count > 0`:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\run-tst-compact-bench.ps1
```

O runner executa:

1. inventário SHA-256;
2. Phase 3 probe: scan → FastCDC → fingerprints → exact dedup;
3. codec probe: STORE/Zstd/Brotli/LZMA2 com decode byte-exact;
4. Pithos `balanced` e `archive-max` para cada arquivo;
5. Pithos `balanced` e `archive-max` para `combined-all`;
6. 7-Zip e WinRAR quando detectados; WinZip somente se suas CLIs existirem;
7. geração de JSONL, CSV e `benchmark-summary.md`;
8. cópia apenas das evidências pequenas para `docs/benchmarks/evidence/`.

Os arquivos grandes ficam sob `tst_compact/results/work/` e não devem ser
commitados.

## 10. Revisar os resultados locais

```powershell
Get-Content .\tst_compact\results\benchmark-summary.md
Get-Content .\tst_compact\results\tools.txt
Get-Content .\tst_compact\results\corpus-summary.txt
```

Não tente otimizar os resultados. O objetivo é congelar o baseline exato do
código atual antes da persistência física de `ChunkTable`.

## 11. Commit de evidência

Independentemente de PASS/FAIL:

```powershell
git status --short
git add Cargo.lock
git add crates/pithos-bench/
git add crates/pithos-cli/
git add crates/pithos-telemetry/
git add docs/benchmarks/evidence/
git commit -m "test: record .pits compression baseline evidence"
git push origin feat/phase3-exact-dedup
git rev-parse HEAD
```

Os `git add crates/...` acima servem somente para preservar mudanças produzidas
por `cargo fmt --all`; não devem existir alterações manuais do dev nesses paths.
Se não houver diff de formatação, esses comandos são inofensivos.

Não adicione:

- `tst_compact/`;
- `docs/gates/GATE_A_EVIDENCE.md`;
- `docs/gates/GATE_B_EVIDENCE.md`;
- novos arquivos em `docs/gates/evidence/` nesta rodada, pois C3 já está fechado.

Retorne o SHA final para revisão.
