# Fingerprints de logical chunks

Este documento congela o contrato de fingerprints da Fase 3. A implementação
fica em `pithos-analysis` e recebe os `LogicalChunk`s produzidos pela etapa de
chunking. Fingerprints não decidem deduplicação nem alteram o PAF; o exact dedup
é uma etapa separada descrita em `docs/exact-dedup.md`.

## Registro por chunk

Cada `ChunkFingerprint` contém:

- `chunk_id` e `length`, que vinculam o resultado ao descritor lógico;
- XXH3-64 para agrupamento rápido;
- os primeiros 128 bits de BLAKE3 para a identidade compacta;
- BLAKE3-256 opcional para escalonamento de colisões;
- CRC32C para detecção rápida de corrupção;
- três ou quatro superfeatures determinísticas para busca de similaridade.

Hashes são sinais de análise, não autorização para descartar bytes. O exact
dedup implementado em `pithos-analysis` sempre confirma os bytes completos antes
de produzir uma referência canônica.

## Política de BLAKE3 completo

Em `Standard`, chunks isolados retêm somente a identidade compacta. Todo grupo
com mais de um membro para a chave `(xxh3, length, blake3_128)` é recalculado e
retém BLAKE3-256. Em `Paranoid`, todos os chunks retêm BLAKE3-256.

`escalate_full_blake3` só publica o valor completo depois de recalcular e
confirmar comprimento, XXH3, BLAKE3-128 e CRC32C. Uma falha não modifica o
fingerprint. A variante configurável respeita todos os limites e a variante com
checkpoint pode ser cancelada entre blocos de 64 KiB.

## Superfeatures normativas

A configuração padrão usa 12 subchunks, quatro superfeatures e janela rolante
de 48 bytes. O algoritmo é:

1. dividir os `n` bytes em 12 regiões balanceadas pelos limites
   `floor(i*n/12)..floor((i+1)*n/12)`;
2. em cada região, usar uma janela de `min(48, tamanho_da_região)` e o hash
   polinomial `h = h * 0x100000001b3 + (byte + 1)`, com aritmética `u64`
   modular; no avanço, remover o byte antigo multiplicado pela base elevada ao
   tamanho da janela;
3. reter o maior valor `u64` sem sinal observado em cada região;
4. agrupar regiões consecutivas de modo balanceado, serializar seus máximos em
   little-endian e aplicar XXH3-64 para formar cada superfeature.

São aceitas 12 regiões/4 grupos por padrão e configurações válidas com três ou
quatro grupos. O chunk vazio produz uma lista vazia. Superfeatures servem apenas
à seleção de candidatos; nunca comprovam igualdade.

Vetor de conformidade para `pithos fingerprint conformance vector v1`, com os
parâmetros padrão:

```text
[4035240308330923769, 4879917261836866628,
 10844589855720895847, 12518628049364330038]
```

Para `abc`, os campos compactos congelados são:

```text
XXH3-64:   0x78af5f94892f3950
CRC32C:    0x364b3fb7
BLAKE3-128: 6437b3ac38465133ffb63b75273a8db5
```

## Streaming, limites e paralelismo

O caminho streaming lê exatamente `LogicalChunk.length` em buffers de 64 KiB,
rejeita EOF antecipado e qualquer byte excedente e oferece checkpoints de
cancelamento antes de cada leitura. O chamador deve fornecer um handle estável
ou snapshot do conteúdo; mudanças concorrentes no objeto de origem são uma
violação do contrato de entrada.

Por padrão, um chunk tem no máximo 4 MiB. Contagem de chunks, soma dos bytes,
metadata, working set do heap e paralelismo possuem limites explícitos. O
orçamento de working set inclui conservadoramente o resultado, referências
ordenadas, candidatos de colisão, bitmap, margem de allocator e scratch por
worker antes de qualquer alocação proporcional ao lote. O scratch é derivado do
maior chunk real, da quantidade de subchunks e da janela rolante, incluindo a
capacidade de todos os rings. Esses mesmos limites se aplicam às APIs unitárias
e streaming.

O processamento em lote não publica resultados parciais em erro e retorna
sempre em ordem crescente de `chunk_id`, independentemente da quantidade de
threads.

## Evidência reproduzível

```text
cargo test -p pithos-analysis --test fingerprints
cargo test -p pithos-analysis --tests
cargo clippy -p pithos-analysis --all-targets -- -D warnings
cargo fuzz run fingerprints -- -runs=10000
```
