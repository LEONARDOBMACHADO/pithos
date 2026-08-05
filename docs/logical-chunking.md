# Logical chunking do Pithos R1

Este documento congela o contrato implementado pela crate `pithos-analysis`
para a etapa de logical chunking. Logical chunks identificam intervalos lógicos
para fingerprints, deduplicação, similaridade, delta e restore maps futuros.
Eles não são compression groups: groups continuam sendo uma decisão física de
compactação solid do `pithos-engine`.

## Contrato determinístico

| Caminho | Política implementada |
|---|---|
| Conteúdo geral | FastCDC v2020, `fastcdc = 4.0.1`, Level1, seed 0 |
| Alta entropia | Blocos fixos explicitamente selecionados entre 1 e 4 MiB |
| Objeto estruturado | Ends exclusivos do scanner, com FastCDC reiniciado dentro de regiões maiores que 1 MiB |
| Microfiles | Arquivos de até 64 KiB, exceto os que exigem acesso isolado, agregados por plano determinístico |

Os defaults FastCDC são exatamente:

```text
min = 65.536 bytes
avg = 262.144 bytes
max = 1.048.576 bytes
normalization = Level1
seed = 0
```

A versão da dependência é fixada com `=4.0.1`. Alterar versão, gear table,
normalização, seed ou os três tamanhos é uma mudança do contrato de boundaries,
não um refactor. O gear hash interno do FastCDC não é identidade de conteúdo e
não deve ser usado para deduplicação; fingerprints criptográficos pertencem à
etapa seguinte.

O vetor Pithos usa 6 MiB + 12.345 bytes gerados por PRNG inteiro fixo. A
serialização little-endian dos pares `(logical_offset, length)` deve produzir:

```text
BLAKE3 0eae0ffb692f3f31a2a3d2616112fcd0bca04a66ea660083c3826751587b52bc
chunks 22
```

O teste versionado também fixa todos os 22 pares e exige igualdade entre os
caminhos slice e streaming independentemente do tamanho dos reads.

## Invariantes

Para cada `(entry_id, object_id)`:

- os offsets começam em `base_offset`;
- chunks são contíguos, sem gaps ou overlaps;
- nenhum chunk não vazio possui comprimento zero;
- a soma dos comprimentos é o tamanho lógico exato;
- todo cálculo de offset e tamanho usa aritmética checada;
- `chunk_id` só é atribuído depois da ordenação por
  `(entry_id, object_id, logical_offset)`;
- descoberta concorrente ou em ordem diferente gera os mesmos IDs finais.

Entrada vazia em FastCDC ou fixed produz zero chunks. Um microfile vazio continua
presente como membro de comprimento zero no MicroFilePack, com hash e metadados
próprios, sem criar loops ou bytes sintéticos.

No caminho `MicroFile`, cada membro do pack gera um `LogicalChunkDraft` próprio;
isso inclui o membro vazio com comprimento zero. Esses drafts entram na mesma
atribuição global e determinística de `chunk_id` usada pelos outros caminhos.

## Boundaries estruturais

Scanners fornecem ends exclusivos relativos ao objeto. A lista deve ser
estritamente crescente, começar acima de zero e terminar exatamente no tamanho
lógico. Duplicatas, ordem decrescente, ends fora do objeto ou cobertura
incompleta falham fechados.

Uma região de até 1 MiB permanece inteira. Uma região maior é subchunked com o
mesmo FastCDC normativo, reiniciado no início da região. Assim nenhum chunk pode
atravessar um boundary do scanner.

Há APIs equivalentes para slice e `Read`. O caminho estrutural streaming consome
uma região limitada por vez, rejeita EOF prematuro e também rejeita qualquer byte
depois do último boundary. O objeto estruturado completo não precisa ser
materializado em memória.

## Alta entropia

O caminho fixed existe e é configurável de 1 a 4 MiB; o default é 1 MiB. A
classificação de alta entropia é explícita. O Pithos não inventa um detector ou
threshold flutuante nesta fase, pois a especificação não define uma métrica
normativa e uma heurística implícita quebraria reprodutibilidade.

## MicroFilePack

O planner recebe somente metadados, tamanho e hash já calculado; ele não mantém
o conteúdo de todos os arquivos em memória. Arquivos acima de 64 KiB e arquivos
marcados `requires_isolated_access` aparecem em `excluded` com motivo tipado.

Entradas elegíveis são aproximadas pela ordem total:

```text
family_key
path_prefix_key
extension_key
mode
similarity_key
path
entry_id
```

`similarity_key` é fornecida pelo chamador; até a fase de similarity, zero é um
valor válido. Packs são divididos quando o próximo conteúdo excederia o target.
O target default é 4 MiB e o intervalo permitido é 1–16 MiB. O último pack pode
ser menor que 1 MiB.

Cada pack contém e valida:

- paths front-coded em relação ao path anterior;
- timestamp base mínimo e deltas `u64`, cobrindo todo o domínio `i64`;
- dicionário de modes ordenado e índices validados;
- offsets de conteúdo contíguos e comprimentos `u32`;
- hash de arquivo BLAKE3 de 32 bytes fornecido pelo estágio anterior;
- correspondência exata entre membros e records de metadados.

Os validadores públicos reaplicam os limites mesmo para estruturas montadas por
chamadores ou decodificadas de dados corrompidos: máximo de 64 KiB por membro,
contagem, bytes lógicos, bytes de metadata, tamanho individual de path, colunas
paralelas, IDs e paths duplicados. A expansão de paths front-coded contabiliza
tanto os sufixos armazenados quanto todos os bytes reconstruídos antes de cada
alocação.

## Streaming, limites e falhas

`chunk_fastcdc_reader` usa o `StreamCDC` e retém apenas o buffer interno de no
máximo `fastcdc_max` mais os descritores já aceitos. O reader é limitado a
`max_logical_bytes + 1`: o byte adicional distingue um objeto exatamente no
limite de um objeto excedente, sem continuar consumindo a origem. A variante com
checkpoint chama o callback antes de cada operação `Read`, portanto readers que
entregam um byte por vez também permanecem canceláveis. Erros reais de `Read`
são propagados como `PithosError::Io`; o erro do checkpoint preserva seu tipo.

`ChunkingConfig::validate` valida os limites antes de chamar a dependência, que
usa debug assertions para parte do contrato. Além dos tamanhos dos algoritmos, a
configuração limita `max_chunks`, `max_logical_bytes`, `max_metadata_bytes` e
`max_path_bytes`. `try_reserve`, conversões verificadas e
`checked_add`/`checked_sub` evitam panic, wraparound e alocação dirigida por
campos não validados.

As variantes `*_with_checkpoint` cobrem chunking, validação, planejamento,
ordenação global de IDs e materialização de MicroFilePacks. As ordenações usam
merge sort fallible com checkpoints durante comparações e cópias; não existe um
sort longo e não cancelável escondido entre dois callbacks.

## Fronteira com o PAF

Esta etapa é format-neutral e não altera arquivos PAF existentes. O PAF
0.1-draft atual ainda representa um único `RestoreMapRecord` por arquivo e não
possui uma `ChunkTable` independente. Reutilizar `GroupTable` como chunk table
violaria a separação obrigatória entre logical chunks e compression groups.

A persistência será ligada quando fingerprints e exact dedup definirem a
`ChunkTable` e as referências físicas. Essa evolução deve manter três relações
separadas:

```text
LogicalChunk -> identidade/referência
RestoreMap   -> reconstrução de Entry
GroupTable   -> armazenamento físico
```

Até lá, os vetores RAW e compressed permanecem byte-idênticos.

## Verificação local

```text
cargo test -p pithos-analysis --tests
cargo clippy -p pithos-analysis --all-targets -- -D warnings
cargo check --manifest-path fuzz/Cargo.toml --bin logical_chunking
cargo +nightly fuzz run logical_chunking -- -runs=10000 -max_len=65536
```

O gate final do projeto também executa testes e Clippy do workspace inteiro,
rustfmt, cobertura acima de 80% e o vetor em Windows e Linux/WSL.
