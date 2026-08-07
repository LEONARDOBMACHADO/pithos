# Pithos baseline local runbook

Este procedimento é a bateria local oficial para validar o estado atual do branch,
fechar a evidência pendente do Gate C3 e congelar o primeiro baseline de
compactação em `tst_compact/`.

Não corrija código durante a execução. Se qualquer etapa falhar, preserve o erro
e a evidência gerada para revisão. O corpus bruto e os artefatos grandes de
benchmark são locais e não entram no Git.

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
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; @('.\scripts\fetch-tst-compact-samples.ps1','.\scripts\inventory-tst-compact.ps1','.\scripts\run-tst-compact-bench.ps1','.\scripts\summarize-tst-compact.ps1','.\scripts\validate-gate-c3-windows.ps1','.\scripts\validate-gate-c3-fuzz-windows.ps1') | ForEach-Object { [void][scriptblock]::Create([System.IO.File]::ReadAllText($_)); Write-Host ('PARSE_OK ' + $_) }"
```

## 3. Qualidade e compilação do workspace

Rode antes de baixar o corpus:

```powershell
cargo fmt --all -- --check
cargo build --workspace --all-targets --all-features
cargo test -p pithos-telemetry --all-targets -- --nocapture
cargo test -p pithos-bench --all-targets -- --nocapture
cargo test -p pithos-cli --test default_phs -- --nocapture
cargo test --workspace --all-targets --all-features -- --nocapture
cargo test --workspace --all-features --doc -- --nocapture
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo llvm-cov --workspace --all-targets --all-features --fail-under-lines 80
```

A primeira chamada Cargo após a inclusão dos crates de tooling pode atualizar
`Cargo.lock`. Preserve a alteração gerada pelo Cargo; não edite o lockfile
manualmente.

## 4. Fechar o fuzz ASan pendente do Gate C3

Primeiro o self-test do runner principal:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\validate-gate-c3-windows.ps1 -SelfTest
```

Depois rode somente o fuzz suplementar com AddressSanitizer, porque os outros dez
checks do Gate C3 já possuem evidência anterior e os comandos de qualidade acima
revalidam o workspace atual:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\validate-gate-c3-fuzz-windows.ps1
```

Se o componente MSVC AddressSanitizer estiver ausente, não altere código. Preserve
a evidência produzida e reporte a mensagem exata.

## 5. Preparar comparadores externos

Para a matriz completa, instale as versões atuais a partir dos fornecedores
oficiais antes do benchmark:

- 7-Zip com CLI `7z` ou `7zz`;
- WinRAR com `WinRAR.exe`;
- WinZip com o Command Line Support Add-On (`WZZIP` e `WZUNZIP`).

O benchmark continua funcionando se algum deles não estiver disponível;
`tools.txt` registra `NOT_FOUND` explicitamente.

## 6. Baixar o corpus da internet

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\fetch-tst-compact-samples.ps1
```

O downloader usa a API pública de SampleFile.com, tenta 29 alvos de
formato/tamanho, valida SHA-256 e cria controles byte-exact para exact dedup.
Formatos indisponíveis ficam registrados em
`tst_compact/results/download-missing.txt`; não substitua lacunas manualmente por
arquivos aleatórios nesta execução.

## 7. Congelar o inventário

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\inventory-tst-compact.ps1
Get-Content .\tst_compact\results\corpus-summary.txt
Get-Content .\tst_compact\results\download-missing.txt
```

O alvo inicial é aproximadamente 850–1050 MiB no total, dependendo dos fixtures
realmente disponíveis.

## 8. Executar a bateria de compactação

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\run-tst-compact-bench.ps1
```

O runner executa:

1. inventário SHA-256;
2. Phase 3 probe: scan → FastCDC → fingerprints → exact dedup;
3. codec probe: STORE/Zstd/Brotli/LZMA2 com decode byte-exact;
4. Pithos `balanced` e `archive-max` para cada arquivo;
5. Pithos `balanced` e `archive-max` para `combined-all`;
6. 7-Zip/WinRAR/WinZip quando detectados;
7. geração de JSONL, CSV e `benchmark-summary.md`;
8. cópia apenas das evidências pequenas para `docs/benchmarks/evidence/`.

Os arquivos grandes ficam sob `tst_compact/results/work/` e não devem ser
commitados.

## 9. Revisar rapidamente os resultados locais

```powershell
Get-Content .\tst_compact\results\benchmark-summary.md
Get-Content .\tst_compact\results\tools.txt
Get-Content .\tst_compact\results\corpus-summary.txt
```

Não tente otimizar ou corrigir resultados ainda. O baseline precisa refletir o
estado exato do código testado.

## 10. Commit de evidência

Independentemente de PASS/FAIL:

```powershell
git status --short
git add Cargo.lock
git add docs/gates/evidence/
git add docs/benchmarks/evidence/
git commit -m "test: record Gate C3 and compression baseline evidence"
git push origin feat/phase3-exact-dedup
git rev-parse HEAD
```

Se `Cargo.lock` não tiver mudado, o `git add Cargo.lock` é inofensivo.
Não adicione `tst_compact/`, `GATE_A_EVIDENCE.md` ou `GATE_B_EVIDENCE.md`.
Retorne o SHA final para revisão.
