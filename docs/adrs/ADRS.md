# Pithos R1 — Architecture Decision Records (ADRs)

Este documento registra as decisões arquiteturais normativas do Pithos R1.

---

## ADR-001: Product and Format Versioning
- **Status:** Aceito
- **Contexto:** Necessidade de clareza no nome e versão do produto vs formato binário.
- **Decisão:** O produto chama-se **Pithos R1**. A expressão "Pithos R1 / R3" está proibida. O formato binário é **PAF 0.1-draft** durante o desenvolvimento e será congelado como **PAF 1.0** no lançamento. A extensão pública oficial passa a ser **`.pts`**, preservando a assinatura consonantal P-T-S de Pithos. `.pithos` permanece aceita como extensão legada durante a transição; a identificação e validação do formato são feitas pelo magic/layout PAF e nunca pela extensão do filename.

---

## ADR-002: Seekable Pack Output
- **Status:** Aceito
- **Contexto:** Geração do arquivo `.pts` exige escrita de offsets globais, CentralIndex e BLAKE3 root no fechamento.
- **Decisão:** A operação `pithos pack` exige estritamente um destino **seekable** (arquivo local ou handle seekable). O suporte a `pithos pack --stdout` NÃO faz parte do release R1. Extração para stdout (`pithos extract <entry> --stdout`) é permitida.

---

## ADR-003: Local JSON-RPC IPC
- **Status:** Aceito
- **Contexto:** O daemon `pithosd` precisa fornecer uma API agent-first estruturada para agentes locais do ecossistema Titans.
- **Decisão:** `pithosd` usará o protocolo **JSON-RPC 2.0** sobre mecanismos de IPC local (Named Pipes no Windows e Unix Domain Sockets no Linux/macOS). Não haverá escuta via TCP por padrão.

---

## ADR-004: Codec Portfolio
- **Status:** Aceito
- **Contexto:** Definição dos codecs genéricos suportados na release inicial.
- **Decisão:** Os codecs genéricos obrigatórios no R1 são **STORE**, **Zstandard**, **Brotli** e **LZMA2**. Codecs experimentais ou PPMd ficam fora do MVP inicial do R1.

---

## ADR-005: liblzma Adapter
- **Status:** Aceito
- **Contexto:** Integração do codec LZMA2 no ecossistema Rust.
- **Decisão:** Utilizar a crate Rust `liblzma` como wrapper de `liblzma`. O formato PAF especificará apenas a especificação do bitstream LZMA2 comprimido sem depender de detalhes de ABI da biblioteca.

---

## ADR-006: Workspace Boundaries & Progressive Extraction
- **Status:** Aceito, com esclarecimento de tooling no ADR-014
- **Contexto:** Evitar sobrecargas de gerenciamento iniciando com crates em excesso.
- **Decisão:** O runtime/engine iniciou com **15 crates** na pasta `crates/`. A separação de novos crates algorítmicos de runtime (ex: `pithos-delta`, `pithos-grammar`) só ocorrerá mediante critérios formais: mais de 10.000 linhas, bitstream isolado ou ganho mensurável no tempo de build. Crates de tooling explicitamente previstos no plano oficial (`pithos-telemetry` e `pithos-bench`) não são extrações algorítmicas do runtime e são tratados separadamente pelo ADR-014.

---

## ADR-007: PAF Compatibility & Version Registry
- **Status:** Aceito
- **Contexto:** Mudanças de versão no container e nos módulos de transformação.
- **Decisão:** O PAF usará um `CodecRegistry` e `SectionDirectory` para registrar IDs de codecs e versões. IDs de codecs/transforms nunca serão reutilizados. O decodificador rejeitará qualquer seção ou codec obrigatório (*required*) com ID desconhecido.

---

## ADR-008: Security Limits & Safe Parsers
- **Status:** Aceito
- **Contexto:** Proteção contra manipulação de arquivos maliciosos e estouro de recursos.
- **Decisão:** Todos os decodificadores e parsers utilizarão aritmética checada (`checked_add`, `checked_mul`), validações estritas de alocação prévia, limite de profundidade de dependências/regras (max 64) e prevenção total contra Path Traversal e symlink escape. Fuzzing é obrigatório desde a Fase 0.

---

## ADR-009: Reproducibility & Determinismo
- **Status:** Aceito
- **Contexto:** Invariância do arquivo comprimido gerado.
- **Decisão:** Sob as mesmas entradas, parâmetros de compressão e versão, o Pithos produzirá **exatamente os mesmos bytes e o mesmo hash BLAKE3**, independente do número de threads ou da ordem de conclusão dos jobs assíncronos. Tie-breaks são totalmente determinísticos.

---

## ADR-010: Temporary Storage & Atomic Commit
- **Status:** Aceito
- **Contexto:** Gerenciamento de spools e integridade durante falhas ou cancelamento.
- **Decisão:** Arquivos temporários de spool serão gravados via `tempfile` no mesmo volume de destino quando possível. A substituição do arquivo final só ocorrerá via **rename atômico** após a validação completa de integridade (`verify`). Erros ou cancelamentos limparão 100% dos spools sem deixar arquivos parciais com o nome final.

---

## ADR-011: Logical Chunking Contract & PAF Boundary
- **Status:** Aceito
- **Contexto:** Logical chunks precisam ser determinísticos e servir a fingerprints/dedup sem serem confundidos com compression groups. O PAF 0.1-draft atual ainda não possui `ChunkTable` independente.
- **Decisão:** `pithos-analysis` usa FastCDC v2020 da crate `fastcdc = 4.0.1`, Level1, seed 0 e 64/256/1024 KiB; high-entropy fixed de 1–4 MiB; boundaries estruturais validados; e MicroFilePack metadata-only de 1–16 MiB. Todos os caminhos possuem limites explícitos de chunks, bytes lógicos, metadata e paths, além de variantes cooperativamente canceláveis. Esta etapa permanece format-neutral. A persistência no PAF só será ligada com uma `ChunkTable` explícita durante fingerprints/exact dedup, mantendo separados LogicalChunk, RestoreMap e GroupTable.

---

## ADR-012: Fingerprint Contract & Collision Boundary
- **Status:** Aceito
- **Contexto:** O pipeline precisa agrupar chunks rapidamente e localizar similaridade sem transformar hashes probabilísticos em prova de igualdade.
- **Decisão:** `pithos-analysis` calcula XXH3-64, BLAKE3-128, CRC32C e superfeatures determinísticas em uma passagem. O modo padrão retém BLAKE3-256 para grupos que colidem em `(xxh3, length, blake3_128)`; o modo paranoico o retém sempre. Lotes são limitados, canceláveis, paralelos e ordenados por `chunk_id`. Nenhum hash ou superfeature autoriza deduplicação: exact dedup e comparação byte a byte permanecem uma fase separada, junto da futura persistência em `ChunkTable`.

---

## ADR-013: Exact Dedup Gate & ChunkTable Boundary
- **Status:** Aceito
- **Contexto:** A Fase 3 precisa eliminar duplicates exatos sem permitir falsos positivos por colisão e sem acoplar prematuramente logical chunks ao layout físico de compression groups.
- **Decisão:** `pithos-analysis` executa exact dedup em shards XXH3, filtra por comprimento e BLAKE3-128, calcula BLAKE3-256 apenas nos grupos candidatos e sempre confirma igualdade pelos bytes completos antes de emitir uma referência. O canonical é o menor `(entry_id, object_id, logical_offset, chunk_id)`. Uma referência só é emitida quando seu ganho líquido permanece positivo após `reference_cost_bytes` e a margem mínima configurada. O resultado é um `ExactDedupPlan` format-neutral e não altera o PAF. A persistência física por `ChunkTable` só pode começar após evidência do Gate C3 e deve manter `LogicalChunk`, `RestoreMap` e `GroupTable` como relações separadas; RAW e compressed atuais permanecem byte-idênticos até essa mudança de formato ser versionada, documentada, testada e fuzzada.

---

## ADR-014: Early Observability, Benchmarking & Ablation Baseline
- **Status:** Aceito
- **Contexto:** O plano oficial posiciona `pithos-telemetry`, `pithos-bench`, benchmarks contra ferramentas externas e ablation reports na Fase 11. Esperar até a Fase 11 impediria medir incrementalmente o custo e o benefício das Fases 3–10 e dificultaria identificar cedo técnicas que aumentam tempo/memória sem ganho líquido relevante.
- **Decisão:** As fronteiras já previstas `pithos-telemetry` e `pithos-bench` são ativadas antecipadamente como **tooling de desenvolvimento**, sem alterar o bitstream PAF nem o caminho obrigatório de runtime. O corpus local padrão é `tst_compact/`, mantido fora do Git; seu manifesto SHA-256 e relatórios pequenos são versionáveis. O baseline mede arquivos individualmente e em conjunto, Pithos `balanced`/`archive-max`, e comparadores 7-Zip/WinRAR/WinZip quando disponíveis. A sonda de Fase 3 mede separadamente scan, chunking, fingerprinting e exact dedup, incluindo tempo e `net_saved_bytes`. Ganhos format-neutral são rotulados como potenciais até aparecerem no tamanho físico do `.pts`. Cada futura técnica relevante deverá reutilizar o mesmo corpus e produzir comparação antes/depois ou ablation equivalente antes de ser considerada vantajosa por padrão.
