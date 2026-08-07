# `tst_compact` — corpus local de compactação

## Objetivo

`tst_compact/` é o ambiente local reproduzível para medir a evolução do Pithos.
Os arquivos brutos não são versionados no Git. O inventário SHA-256 e os
resultados resumidos são copiados para `docs/benchmarks/evidence/` pelo runner.

O primeiro baseline deve medir duas situações separadas:

1. cada arquivo individualmente;
2. todos os arquivos do corpus em conjunto.

O Pithos é medido nos perfis `balanced` e `archive-max`. Quando as respectivas
CLIs estão instaladas, o mesmo runner também mede 7-Zip/LZMA2, WinRAR/RAR5 solid
e WinZip.

## Tamanho inicial recomendado

Para a primeira rodada, mantenha o corpus entre **750 e 950 MiB** no total.
A média pretendida dos arquivos primários é **20–30 MiB**, com diversidade
intencional:

- pequenos: 1–10 MiB;
- médios: 10–25 MiB;
- grandes: 50–100 MiB.

Evite arquivos individuais acima de 150 MiB nesta primeira rodada. O
`pithos-phasebench` carrega o corpus da análise de Fase 3 em memória e possui um
limite explícito de segurança (`--max-total-mib`, 2048 MiB por padrão).

## Estrutura sugerida

```text
tst_compact/
  text/
  structured/
  documents/
  images/
  audio/
  video/
  databases/
  archives/
  binaries/
  duplicates/
  results/       # gerado; não entra no corpus nem no Git
```

Não coloque outputs `.pts`, `.zip`, `.rar` ou `.7z` gerados pelos benchmarks nas
pastas de entrada. O runner usa `results/` para todos os artefatos temporários.

## Matriz inicial de formatos

Baixe **arquivos reais e válidos**, não arquivos preenchidos artificialmente com
zeros, escolhendo aproximadamente os tamanhos abaixo:

| Família | Extensão | Alvo inicial |
|---|---|---:|
| Texto | `.txt` | 10 MiB |
| CSV | `.csv` | 25 MiB |
| JSON | `.json` | 25 MiB |
| XML | `.xml` | 10–25 MiB |
| SQL dump | `.sql` | 25–50 MiB |
| Log | `.log` | 10 MiB |
| PDF | `.pdf` | 10 MiB e 25 MiB |
| Word/OOXML | `.docx` | 5–10 MiB |
| Excel/OOXML | `.xlsx` | 5–10 MiB |
| PowerPoint/OOXML | `.pptx` | 10–25 MiB |
| Bitmap | `.bmp` | 25–50 MiB |
| TIFF | `.tif`/`.tiff` | 25–50 MiB |
| PNG | `.png` | 10 MiB e 25 MiB |
| JPEG | `.jpg`/`.jpeg` | 10–25 MiB |
| WebP | `.webp` | 10–25 MiB |
| WAV | `.wav` | 25–50 MiB |
| FLAC | `.flac` | 25–50 MiB |
| MP3 | `.mp3` | 10–25 MiB |
| MP4 | `.mp4` | 50–100 MiB |
| MKV | `.mkv` | 50–100 MiB |
| SQLite/database | `.sqlite`/`.db` | 25–50 MiB |
| TAR sem compressão | `.tar` | 25–50 MiB |
| ZIP | `.zip` | 25–50 MiB |
| 7-Zip | `.7z` | 25–50 MiB |
| RAR | `.rar` | 25–50 MiB |
| Gzip | `.gz` | 25–50 MiB |
| Binário/WebAssembly | `.bin`/`.wasm` | 10–25 MiB |

A matriz intencionalmente mistura conteúdo altamente compressível, estruturado,
já comprimido e alta entropia. Isso evita otimizar o Pithos para apenas um tipo
de dado.

## Download automatizado recomendado

A forma preferida de criar a primeira versão do corpus é executar:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\fetch-tst-compact-samples.ps1
```

O downloader:

- consulta `https://samplefile.com/samples/api/files?format=<extensão>`;
- tenta 29 alvos de formato/tamanho da matriz acima;
- escolhe o fixture disponível mais próximo do tamanho-alvo;
- baixa para a subpasta correta;
- valida o SHA-256 publicado antes de publicar o arquivo local;
- nunca substitui silenciosamente um arquivo local com hash divergente;
- cria, por padrão, duas cópias byte-exact de quatro famílias como controles de
  dedup;
- grava `source-register.csv` e `download-missing.txt` em `results/`.

Se um formato não existir ou a API não retornar fixture utilizável, ele é
registrado em `download-missing.txt`; não é criado conteúdo sintético para
fingir cobertura. O benchmark pode rodar com os formatos disponíveis e a lacuna
fica explícita para a próxima rodada.

Use `-Force` apenas quando for intencional substituir uma fixture local cujo hash
não coincide mais. `-SkipDuplicates` desliga somente a criação dos controles
byte-exact.

## Fontes de download

Preferir fontes que publiquem fixtures reais e checksum. Para o primeiro corpus,
`https://samplefile.com/` é a fonte-base: possui uma biblioteca grande de
formatos, diversos tamanhos e SHA-256 publicado por fixture.

Para código/binários adicionais, use somente projetos open-source/repositórios ou
releases oficiais. Não baixe executáveis aleatórios de sites de terceiros apenas
para aumentar variedade.

O downloader registra cada fixture em:

```text
tst_compact/results/source-register.csv
```

com path relativo, formato, target MiB, tamanho efetivo, SHA-256, URL e descrição
da origem. Esse registro não entra no corpus e é copiado para a evidência.

## Controles de deduplicação

O downloader cria em `duplicates/` **cópias byte a byte** de quatro famílias,
quando existem fontes adequadas:

- TXT/CSV/JSON;
- PDF;
- PNG/JPEG;
- ZIP/7z/RAR.

Cada fonte selecionada recebe duas cópias com nomes diferentes. Não há alteração
dos bytes. Essas cópias são controles positivos para a deduplicação exata e
permitem medir quanto trabalho C3 consegue eliminar independentemente do codec.

Near-duplicates serão adicionados em uma matriz separada quando similarity,
clustering e delta entrarem fisicamente no pipeline; não misturar essa avaliação
com o controle inicial de exact dedup.

## Inventário obrigatório

Depois de preencher `tst_compact/`:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\inventory-tst-compact.ps1
```

Isso produz em `tst_compact/results/`:

- `corpus-manifest.csv`: path, extensão, bytes, MiB, SHA-256 e timestamp;
- `corpus-summary.txt`: quantidade, total, média e distribuição por extensão.

O SHA-256 é a identidade do corpus. Uma futura comparação só é considerada
comparável ao baseline se o manifesto for o mesmo, ou se a mudança de corpus for
explicitamente registrada.

## Ferramentas concorrentes

O runner detecta as CLIs no `PATH` e também acrescenta as instalações Windows
mais comuns (`Program Files/7-Zip`, `Program Files/WinRAR` e
`Program Files/WinZip`) ao `PATH` do processo.

Para a matriz completa instale, a partir de suas fontes oficiais:

- 7-Zip atual com CLI `7z`/`7zz`;
- WinRAR atual com `WinRAR.exe`;
- WinZip e o **WinZip Command Line Support Add-On**, que fornece `WZZIP` e
  `WZUNZIP`.

Se alguma ferramenta não existir, o Pithos continua sendo testado; `tools.txt`
registra explicitamente `NOT_FOUND` para o comparador ausente.

## Execução

O runner completo é:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\run-tst-compact-bench.ps1
```

Ele executa, em sequência:

1. inventário e SHA-256 do corpus;
2. `pithos-phasebench` para Scan → FastCDC → Fingerprints → Exact Dedup;
3. benchmark de compactação/descompactação Pithos por arquivo e combinado;
4. 7-Zip, WinRAR e WinZip quando encontrados;
5. `benchmark-summary.md` com ranking combinado, vitórias individuais e
   distribuição percentual do tempo da análise de Fase 3;
6. cópia somente dos relatórios pequenos para uma pasta versionável em
   `docs/benchmarks/evidence/tst-compact-<timestamp>/`.

Os grandes arquivos compactados/descompactados ficam somente em
`tst_compact/results/work/` e nunca devem ser commitados.

## Métricas desta primeira versão

O baseline já produz:

- bytes originais e bytes do archive;
- compression ratio e savings %;
- tempo de compressão;
- tempo de verificação Pithos;
- tempo de descompressão;
- resultado por arquivo e `combined-all`;
- ranking humano do corpus combinado;
- número de vitórias por tamanho nos arquivos individuais;
- tempo e percentual de scan, chunking, fingerprinting e exact dedup;
- chunks canônicos/referenciados;
- duplicate bytes brutos;
- custo das referências;
- `net_saved_bytes` e ganho percentual potencial do exact dedup.

A economia de exact dedup nesta etapa é explicitamente **potencial**: C3 ainda é
format-neutral. Depois da integração física da `ChunkTable`, o mesmo corpus será
rodado novamente e o ganho deverá aparecer também no tamanho real do `.pts`.

## Regra de naming `.pts`

Novos arquivos Pithos usam `.pts`:

```text
report.pdf      -> report.pdf.pts
Project/        -> Project.pts
A.txt + B.bin   -> files.pts
```

`--output/-o` continua permitindo nome explícito. Arquivos legados `.pithos`
continuam sendo aceitos pelo leitor durante a transição.
