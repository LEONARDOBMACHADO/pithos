# Pithos

Pithos é um mecanismo de arquivamento e compactação em Rust, orientado tanto a
uso humano por CLI quanto a automações e agentes por uma API local. O projeto
define o contêiner PAF (`.pithos`), preserva paths e links de forma portável e
prioriza determinismo, integridade verificável, limites de recursos e publicação
atômica dos resultados.

O objetivo é evoluir de um formato RAW/STORE auditável para um portfólio de
codecs e otimizações estruturais sem sacrificar restauração byte-exact ou tornar
o parser vulnerável a arquivos malformados.

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
| Zstandard, Brotli, LZMA2 e solid groups | Fase 2 em implementação |
| Deduplicação, similarity, transforms, recompression, viewer e mount | Fases posteriores |

O perfil público disponível neste momento é `raw`. Ele usa STORE, sem compressão,
e serve como baseline determinístico e verificável. Não confunda o estado atual
com a visão completa do produto: codecs comprimidos e otimizações avançadas só
serão anunciados quando seus respectivos gates estiverem verdes.

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
cargo run -p pithos-cli --bin pithos -- pack ./dados --output ./backup.pithos --profile raw
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
cargo run -p pithos-cli --bin pithos -- --mode daemon --daemon-state-dir ./.pithos-state pack ./dados --output ./backup.pithos --profile raw
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
- [`docs/agent-api-v1.md`](docs/agent-api-v1.md): transporte, sessões, métodos,
  jobs, quotas e erros públicos;
- [`docs/adrs/ADRS.md`](docs/adrs/ADRS.md): decisões arquiteturais;
- [`CONTRIBUTING.md`](CONTRIBUTING.md): fluxo de contribuição e gates locais;
- [`SECURITY.md`](SECURITY.md): canal e fronteira de segurança.

Planos internos, relatórios temporários e evidências geradas de gates ficam fora
do Git. Testes de regressão e corpora licenciados permanecem versionados porque
fazem parte do contrato verificável do projeto.

## Licença

Os manifests declaram licenciamento dual `MIT OR Apache-2.0`.
