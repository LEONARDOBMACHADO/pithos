# Pithos

Pithos é um mecanismo de arquivamento e compactação em Rust, orientado tanto a
uso humano por CLI quanto a automações e agentes por uma API local. O projeto
define o contêiner PAF (`.pithos`), preserva paths e links de forma portável e
prioriza determinismo, integridade verificável, limites de recursos e publicação
atômica dos resultados.

O formato evolui a partir de um baseline RAW/STORE auditável para um portfólio
de codecs e otimizações estruturais sem sacrificar restauração byte-exact ou
tornar o parser vulnerável a arquivos malformados.

## Para que serve

- empacotar arquivos e árvores de diretórios em um contêiner determinístico;
- restaurar todo o conteúdo ou apenas uma entrada;
- listar e inspecionar metadados sem ler o payload;
- verificar CRC32C e BLAKE3 antes de publicar dados restaurados;
- oferecer os mesmos fluxos diretamente no processo ou por um daemon local;
- permitir integração com agentes por JSON-RPC estrito, jobs persistentes,
  eventos, cancelamento, quotas e retomada de sessão.

## Estado e escopo atual

| Área | Estado |
|---|---|
| PAF 0.1-draft RAW/STORE | Implementado e testado |
| CLI `pack`, `unpack`, `list`, `inspect`, `extract`, `verify`, `capabilities` | Implementado em `standalone` e `daemon` |
| Agent API local com 12 métodos | Implementada |
| Jobs persistentes, idempotência, eventos, prioridades, quotas e recovery | Implementados |
| Zstandard, Brotli, LZMA2, seleção determinística e solid groups | Implementados e testados |
| Logical chunking (FastCDC, fixed, structural e MicroFilePack) | Implementado e testado |
| Fingerprints (XXH3, BLAKE3, CRC32C e superfeatures) | Implementados e testados |
| Exact dedup format-neutral | Implementado; Gate C3 aguardando execução externa |
| Similarity, clustering, reordering, transforms, recompression, viewer e mount | Fases posteriores |

Os perfis públicos de empacotamento são:

| Perfil CLI | Objetivo | Alvo de solid group | Codecs candidatos |
|---|---|---:|---|
| `raw` | baseline sem compressão | arquivos independentes | STORE |
| `stream` | baixa latência e grupos pequenos | 4 MiB | STORE, Zstandard |
| `random` | acesso aleatório com grupos contidos | 8 MiB | STORE, Zstandard |
| `balanced` | equilíbrio entre razão, memória e acesso | 64 MiB | STORE, Zstandard, Brotli, LZMA2 |
| `archive-max` | máxima razão para arquivamento | 512 MiB | STORE, Zstandard, Brotli, LZMA2 |

O engine avalia os candidatos permitidos pelo perfil e escolhe pelo custo total
com tie-break determinístico. `raw` permanece o padrão por compatibilidade. Na
Agent API JSON, o último perfil é escrito `archive_max`; a CLI e a resposta de
`capabilities` usam `archive-max`.

## Garantias já implementadas

- inteiros e records do formato validados com limites e aritmética checada;
- CRC32C por estruturas/seções/grupos e BLAKE3 por entrada e arquivo;
- escrita por spool e publicação atômica sem sobrescrever destinos;
- unpack transacional: um destino parcial não é publicado;
- paths absolutos, `..`, escapes de symlink e nomes perigosos são rejeitados;
- cancelamento e deadlines possuem checkpoints durante I/O;
- daemon falha fechado quando não consegue persistir uma transição;
- IPC restrito ao usuário local, sem listener TCP;
- resultados de jobs e chaves de idempotência sobrevivem a restart.
- cada codec obrigatório possui vetor de bytes e BLAKE3 fixado por teste;
- perfis comprimidos validam identidade, timestamp, tamanho e hash da entrada
  entre scan, hashing e encoding;
- cotas de memória e espaço temporário são aplicadas separadamente pelo engine;
- boundaries lógicos são determinísticos, streaming e validados sem gaps ou
  overlaps, com vetor FastCDC fixado e MicroFilePack metadata-only.
- fingerprints compactos e completos possuem vetores congelados, limites de
  recursos, streaming exato e saída paralela determinística;
- exact dedup usa XXH3/length/BLAKE3 apenas para filtrar candidatos, recalcula
  BLAKE3 completo quando necessário, confirma bytes, usa canonical tie-break
  determinístico e rejeita referências sem ganho líquido;
- exact dedup permanece format-neutral até o Gate C3 ser validado; hashes nunca
  autorizam compartilhamento físico sem a comparação exata.

O usuário do sistema operacional é a fronteira de segurança do daemon. O
`path_scope` restringe clientes e automações a raízes canonicalizadas, mas não é
uma sandbox contra outro processo hostil executando simultaneamente com a mesma
identidade. Para esse cenário, execute `pithosd` sob uma conta dedicada com
permissões apenas nas raízes necessárias.

## Requisitos

- Rust estável, com `rustfmt` e `clippy`;
- Windows ou Unix com suporte a Unix Domain Sockets;
- C toolchain compatível com os backends de codec usados pelo workspace.

O arquivo `rust-toolchain.toml` instala automaticamente os componentes Rust
necessários quando o projeto é usado por `rustup`.

## Compilar e testar

```text
cargo build --workspace
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

A cobertura do workspace é validada com `cargo-llvm-cov` e deve permanecer acima
de 80% de linhas:

```text
cargo llvm-cov --workspace --all-targets --fail-under-lines 80
```

## Uso standalone

Empacotar uma árvore:

```text
cargo run -p pithos-cli --bin pithos -- pack ./dados --output ./backup.pithos --profile balanced
```

Listar e verificar sem restaurar:

```text
cargo run -p pithos-cli --bin pithos -- list ./backup.pithos
cargo run -p pithos-cli --bin pithos -- verify ./backup.pithos
```

Restaurar todo o archive ou uma única entrada:

```text
cargo run -p pithos-cli --bin pithos -- unpack ./backup.pithos --output ./restaurado

cargo run -p pithos-cli --bin pithos -- extract ./backup.pithos caminho/no/archive.txt --output ./selecionado
```

Todos os comandos aceitam `--output-format human|json`. O modo padrão é
`standalone` e não depende do daemon.

## Uso com `pithosd`

Inicie o daemon em um terminal, declarando explicitamente as raízes máximas:

```text
cargo run -p pithos-daemon --bin pithosd -- --state-dir ./.pithos-state --allow-read-root . --allow-write-root .
```

Em outro terminal, use a mesma state directory e selecione o modo daemon:

```text
cargo run -p pithos-cli --bin pithos -- --mode daemon --daemon-state-dir ./.pithos-state pack ./dados --output ./backup.pithos --profile balanced
```

O transporte é Named Pipe no Windows e Unix Domain Socket no Unix. Não existe
endpoint TCP. A CLI negocia capabilities, acompanha o job até o estado terminal e
retoma a sessão após reconexões de transporte.

## Arquitetura do workspace

| Crate | Responsabilidade |
|---|---|
| `pithos-core` | tipos centrais, perfis, limites e erros |
| `pithos-format` | records e codificação do PAF |
| `pithos-io` | publicação atômica e primitivas de I/O |
| `pithos-codecs` | contrato e backends de codec |
| `pithos-analysis` | logical chunking, MicroFilePack, fingerprints, exact dedup e análises para similarity |
| `pithos-planner` | custos e decisões globais de encoding |
| `pithos-engine` | pack, catálogo, verify, extract e unpack |
| `pithos-agent-api` | contrato JSON-RPC público |
| `pithos-daemon` | IPC, sessões, scheduler, jobs e persistência |
| `pithos-cli` | interface de linha de comando e cliente do daemon |
| `pithos-testkit` | corpus determinístico e testes de integração |

Os demais crates do workspace reservam fronteiras para análise, transforms,
recompression, viewer e mount, que serão preenchidas nas fases correspondentes.

## Documentação versionada

- [`docs/paf-0.1-raw.md`](docs/paf-0.1-raw.md): layout RAW/STORE implementado;
- [`docs/paf-0.1-compressed.md`](docs/paf-0.1-compressed.md): extensão comprimida,
  registry de codecs, solid groups e perfis;
- [`docs/agent-api-v1.md`](docs/agent-api-v1.md): transporte, sessões, métodos,
  jobs, quotas e erros públicos;
- [`docs/logical-chunking.md`](docs/logical-chunking.md): algoritmos, vetores,
  limites e fronteira format-neutral do chunking;
- [`docs/fingerprints.md`](docs/fingerprints.md): hashes, superfeatures,
  escalonamento de colisões, limites e vetores de conformidade;
- [`docs/exact-dedup.md`](docs/exact-dedup.md): exact dedup, colisões, custo,
  determinismo e fronteira para a futura `ChunkTable`;
- [`docs/adrs/ADRS.md`](docs/adrs/ADRS.md): decisões arquiteturais;
- [`docs/gates/GATE_C3_EXACT_DEDUP_EVIDENCE.md`](docs/gates/GATE_C3_EXACT_DEDUP_EVIDENCE.md):
  registro reproduzível da validação externa do Gate C3;
- [`CONTRIBUTING.md`](CONTRIBUTING.md): fluxo de contribuição e gates locais;
- [`SECURITY.md`](SECURITY.md): canal e fronteira de segurança.

Relatórios temporários pesados e artefatos brutos de execução podem ficar fora
do Git. Resumos reproduzíveis de Gates, comandos, versões, resultados e erros que
fundamentam uma decisão de avanço devem ser versionados em `docs/gates/`. Testes
de regressão e corpora licenciados permanecem versionados porque fazem parte do
contrato verificável do projeto.

## Licença

Os manifests declaram licenciamento dual `MIT OR Apache-2.0`.