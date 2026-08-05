# Contribuindo para o Pithos

## Fluxo obrigatório

1. Abra uma issue ou descreva claramente o objetivo da mudança.
2. Mantenha as fronteiras dos crates e a direção de dependências definida nos ADRs.
3. Adicione testes para comportamento novo e casos de erro.
4. Execute localmente:

```text
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

5. Não introduza secrets, binários gerados ou dados de corpus sem licença e origem
   documentadas.

Mudanças no formato PAF, nos IDs de codecs ou nas políticas de segurança exigem
um ADR antes da implementação.
