# Exact dedup do Pithos R1

Status: implementação presente em `pithos-analysis`; Gate C3 depende da execução
externa dos testes, Clippy, fuzz e coverage antes de a persistência ser ligada ao
PAF.

Esta etapa consome `LogicalChunk`, `ChunkFingerprint` e os bytes exatos do chunk.
Ela produz um `ExactDedupPlan` determinístico e **format-neutral**. Nenhum hash,
superfeature ou score de similaridade é suficiente para compartilhar bytes.

## Pipeline normativo

A ordem de filtragem segue a Fase 3 do plano:

1. shard por XXH3-64;
2. comprimento;
3. BLAKE3-128;
4. BLAKE3-256 calculado somente para grupos compactos com mais de um candidato;
5. comparação exata dos bytes;
6. canonical tie-break determinístico;
7. decisão por custo líquido.

Mesmo uma colisão artificial de BLAKE3-256 não autoriza deduplicação: candidatos
com o mesmo full hash ainda são ordenados e agrupados pelos bytes completos.

## Canonical tie-break

Para bytes idênticos, o canonical é o menor valor da ordem total:

```text
(entry_id, object_id, logical_offset, chunk_id)
```

A atribuição normal de `chunk_id` já segue a ordem lógica, mas o deduplicador não
depende da ordem de chegada dos inputs nem da ordem em que shards paralelos
terminam. O resultado final é sempre publicado em ordem crescente de `chunk_id`.

## Custo

Um duplicate só vira referência quando:

```text
chunk_length - reference_cost_bytes >= min_net_savings_bytes
```

com ganho estritamente positivo. O default atual usa:

```text
reference_cost_bytes = 16
min_net_savings_bytes = 1
```

Esses valores representam a porta de custo da etapa format-neutral. A futura
`ChunkTable` deverá confirmar seu custo físico real; se o record definitivo for
maior, o planner deve usar o custo real antes de habilitar a referência no
archive.

O plano reporta separadamente:

- `canonical_chunks`;
- `referenced_chunks`;
- `gross_duplicate_bytes`;
- `reference_bytes`;
- `net_saved_bytes`.

## Limites e cancelamento

`ExactDedupConfig` limita:

- quantidade de chunks;
- soma dos bytes analisados;
- working set de metadata;
- custo de uma referência;
- margem mínima de ganho;
- paralelismo de shards.

O algoritmo não copia payloads para filas. Ele mantém referências aos bytes
fornecidos pelo caller, usa estruturas bounded por `max_chunks` e aplica
checkpoints durante validação, hashing completo, ordenação, comparação, merge de
resultados e validação final.

Uma falha ou cancelamento não publica plano parcial.

## Gate C3

A implementação e os testes versionados exigem:

- todos os duplicates exatos e economicamente benéficos são detectados;
- bytes diferentes nunca são deduplicados;
- colisão compacta forçada continua segura;
- colisão full-hash forçada em teste unitário continua segura porque os bytes são
  comparados;
- input order não altera o canonical;
- paralelismo 1 vs 4 produz o mesmo plano;
- referências sem ganho líquido são rejeitadas;
- IDs duplicados e limites de recursos falham fechados;
- cancelamento interrompe a operação sem resultado parcial.

## Fronteira com o PAF

Esta mudança **não altera os bytes de arquivos PAF existentes**. `RestoreMap` e
`GroupTable` continuam descrevendo o formato físico atual.

A etapa seguinte, somente após evidência do Gate C3, deverá introduzir uma
`ChunkTable` explícita e versionada que preserve três relações distintas:

```text
LogicalChunk -> identidade / canonical exact reference
RestoreMap   -> reconstrução da Entry
GroupTable   -> armazenamento físico / compression group
```

A integração não pode reutilizar `GroupTable` como tabela de chunks nem tornar
RAW/compressed incompatíveis sem atualização simultânea do byte spec, parser,
fuzz targets e vectors.

## Verificação externa do Gate

```text
cargo test -p pithos-analysis --tests
cargo clippy -p pithos-analysis --all-targets -- -D warnings
cargo check --manifest-path fuzz/Cargo.toml --bin exact_dedup
cargo +nightly fuzz run exact_dedup -- -runs=10000 -max_len=65536
```

O gate de regressão também deve executar `cargo fmt`, testes/Clippy do workspace
e coverage >= 80% antes de a próxima mudança do formato começar.
