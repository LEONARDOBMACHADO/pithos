# PAF 0.1-draft — perfil RAW/STORE

Este documento descreve o layout implementado pelo Gate A. Todos os inteiros
fixos são little-endian. O arquivo é determinístico para as mesmas entradas,
metadados e versão do Pithos.

## Layout

| Região | Conteúdo |
|---|---|
| `0..96` | Global Header com magic, versão, contagens, offsets e CRC32C |
| após o header | 6 registros de 32 bytes do Section Directory |
| PayloadArea | bytes STORE dos arquivos regulares, em ordem determinística |
| EntryTable | registros JSON compactos com paths lossless e tipos de entrada |
| GroupTable | grupos STORE, offsets, comprimentos e CRC32C |
| RestoreMap | associação entre entradas regulares e grupos |
| CentralIndex | associação ordenada entre path, entry e group |
| IntegrityTree | BLAKE3 por entrada |
| últimos 64 bytes | Footer com tamanho, BLAKE3 global, CRC do diretório e CRC próprio |

Os seis tipos obrigatórios são `EntryTable`, `GroupTable`, `PayloadArea`,
`RestoreMap`, `CentralIndex` e `IntegrityTree`. Seções desconhecidas, duplicadas,
ausentes, sobrepostas ou fora dos limites são rejeitadas.

## Integridade

- CRC32C do header;
- CRC32C de cada seção e grupo;
- BLAKE3 de cada arquivo lógico;
- BLAKE3 de todos os bytes anteriores ao footer;
- CRC32C do footer.

O decoder valida estrutura, limites e integridade antes de publicar qualquer
arquivo. A extração ocorre num diretório temporário irmão do destino e só é
publicada por rename depois da restauração completa.

## Paths e links

Paths são sequências de componentes, nunca strings concatenadas. Componentes
UTF-8 usam bytes canónicos; nomes não UTF-8 do Unix e UTF-16 inválido do Windows
têm representações lossless próprias. Componentes absolutos, `..`, prefixes,
separadores embutidos e nomes de dispositivo do Windows são rejeitados.

Symlinks absolutos ou que resolvam fora da raiz de entrada são rejeitados.
Symlinks válidos são criados por último durante a restauração. Hardlinks são
registrados em relação à primeira entrada canónica e recriados após os arquivos.
