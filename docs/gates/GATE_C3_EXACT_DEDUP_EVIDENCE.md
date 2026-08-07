# Gate C3 — Exact Dedup Evidence

> Este arquivo deve ser preenchido na máquina de validação e commitado sem
> remover falhas. Se algum comando falhar, preservar o comando, exit code e
> output relevante para correção no próximo ciclo.

## Identificação

- Branch: `feat/phase3-exact-dedup`
- Commit testado: `PREENCHER`
- Data/hora UTC: `PREENCHER`
- SO: `PREENCHER`
- Arquitetura: `PREENCHER`
- `rustc --version --verbose`: `PREENCHER`
- `cargo --version`: `PREENCHER`

## Resultado resumido

| Gate | Resultado | Evidência curta |
|---|---|---|
| build workspace | PREENCHER | PREENCHER |
| exact dedup tests | PREENCHER | PREENCHER |
| analysis tests | PREENCHER | PREENCHER |
| workspace tests | PREENCHER | PREENCHER |
| Clippy analysis | PREENCHER | PREENCHER |
| Clippy workspace | PREENCHER | PREENCHER |
| rustfmt | PREENCHER | PREENCHER |
| fuzz target build | PREENCHER | PREENCHER |
| exact_dedup fuzz 10k | PREENCHER | PREENCHER |
| coverage >= 80% | PREENCHER | PREENCHER |

## Comandos e outputs

### 1. Build

```text
PREENCHER output de: cargo build --workspace --all-targets
```

### 2. Exact dedup

```text
PREENCHER output de: cargo test -p pithos-analysis --test exact_dedup
```

### 3. Analysis completo

```text
PREENCHER output de: cargo test -p pithos-analysis --tests
```

### 4. Workspace completo

```text
PREENCHER output de: cargo test --workspace --all-targets
```

### 5. Clippy analysis

```text
PREENCHER output de: cargo clippy -p pithos-analysis --all-targets -- -D warnings
```

### 6. Clippy workspace

```text
PREENCHER output de: cargo clippy --workspace --all-targets -- -D warnings
```

### 7. Rustfmt

```text
PREENCHER output de: cargo fmt --all -- --check
```

### 8. Fuzz target build

```text
PREENCHER output de: cargo check --manifest-path fuzz/Cargo.toml --bin exact_dedup
```

### 9. Exact dedup fuzz

```text
PREENCHER output de: cargo +nightly fuzz run exact_dedup -- -runs=10000 -max_len=65536
```

### 10. Coverage

```text
PREENCHER output de: cargo llvm-cov --workspace --all-targets --fail-under-lines 80
```

## Erros encontrados

Registrar **todos** os erros, warnings bloqueantes, panics, timeouts ou diferenças
entre plataformas. Não corrigir silenciosamente este arquivo após uma falha; o
commit deve preservar a evidência que motivou a correção.

```text
PREENCHER ou escrever: nenhum
```

## Gate C3

Marcar somente após todos os itens obrigatórios acima passarem:

- [ ] 100% dos duplicates exatos benéficos detectados nos testes versionados;
- [ ] nenhum false dedup;
- [ ] simulação de colisão segura;
- [ ] determinismo entre paralelismo 1 e 4;
- [ ] limites e cancelamento passam;
- [ ] fuzz 10.000 runs sem crash/panic;
- [ ] workspace sem regressão;
- [ ] coverage >= 80%.

**Decisão:** `PREENCHER: PASS / FAIL`
