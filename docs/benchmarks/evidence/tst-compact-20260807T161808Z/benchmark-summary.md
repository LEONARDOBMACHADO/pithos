# Pithos compression benchmark summary

Generated UTC: 2026-08-07T17:12:10.2928178Z

## Combined corpus

| Rank | Compressor | Profile | Archive MiB | Savings % | Compress s | Decompress s |
|---:|---|---|---:|---:|---:|---:|
| 1 | 7zip | 7z-lzma2-mx9 | 82.421 | 82.6914 | 66.594 | 5.266 |
| 2 | pithos | archive-max | 84.408 | 82.2743 | 248.164 | 275.161 |
| 3 | winrar | rar5-m5-solid | 186.73 | 60.7863 | 176.778 | 31.337 |

## Individual-file size wins

| Compressor / profile | Files won |
|---|---:|
| 7zip / 7z-lzma2-mx9 | 16 |
| winrar / rar5-m5-solid | 10 |
| pithos / archive-max | 5 |

## Pithos codec contribution probe

| Codec | Files | Size wins | Aggregate savings % | Encode total s | Decode total s |
|---|---:|---:|---:|---:|---:|
| brotli | 31 | 15 | 80.9676 | 93.751 | 3.174 |
| lzma2 | 31 | 9 | 82.2632 | 177.205 | 8.684 |
| store | 31 | 3 | 0 | 0.152 | 0.196 |
| zstd | 31 | 4 | 80.8534 | 7.887 | 0.726 |

This probe runs every mandatory codec directly on every eligible file and verifies byte-exact decode. It isolates codec cost/benefit from grouping and other Pithos stages.

## Failed benchmark records

| Case | Compressor | Profile | Detail |
|---|---|---|---|
| single-0004-____C__Projetos_TESTE_tst_compact_archives_zip_sample_file_50MB.zip | pithos | balanced | Limite de recursos excedido: group expansion ratio |
| single-0004-____C__Projetos_TESTE_tst_compact_archives_zip_sample_file_50MB.zip | pithos | archive-max | Limite de recursos excedido: group expansion ratio |
| single-0014-____C__Projetos_TESTE_tst_compact_duplicates_json_sample_file_25MB.dup1.json | pithos | balanced | Limite de recursos excedido: group expansion ratio |
| single-0014-____C__Projetos_TESTE_tst_compact_duplicates_json_sample_file_25MB.dup1.json | pithos | archive-max | Limite de recursos excedido: group expansion ratio |
| single-0016-____C__Projetos_TESTE_tst_compact_duplicates_png_sample_file_25MB.dup1.png | pithos | balanced | Limite de recursos excedido: group expansion ratio |
| single-0016-____C__Projetos_TESTE_tst_compact_duplicates_png_sample_file_25MB.dup1.png | pithos | archive-max | Limite de recursos excedido: group expansion ratio |
| single-0017-____C__Projetos_TESTE_tst_compact_duplicates_zip_sample_file_50MB.dup1.zip | pithos | balanced | Limite de recursos excedido: group expansion ratio |
| single-0017-____C__Projetos_TESTE_tst_compact_duplicates_zip_sample_file_50MB.dup1.zip | pithos | archive-max | Limite de recursos excedido: group expansion ratio |
| single-0018-____C__Projetos_TESTE_tst_compact_images_bmp_2000x1200_sample_file_9.2MB.bmp | pithos | balanced | Limite de recursos excedido: group expansion ratio |
| single-0018-____C__Projetos_TESTE_tst_compact_images_bmp_2000x1200_sample_file_9.2MB.bmp | pithos | archive-max | Limite de recursos excedido: group expansion ratio |
| single-0021-____C__Projetos_TESTE_tst_compact_images_png_sample_file_25MB.png | pithos | balanced | Limite de recursos excedido: group expansion ratio |
| single-0021-____C__Projetos_TESTE_tst_compact_images_png_sample_file_25MB.png | pithos | archive-max | Limite de recursos excedido: group expansion ratio |
| single-0022-____C__Projetos_TESTE_tst_compact_images_tiff_2000x1200_sample_file_18.3MB.tiff | pithos | balanced | Limite de recursos excedido: group expansion ratio |
| single-0022-____C__Projetos_TESTE_tst_compact_images_tiff_2000x1200_sample_file_18.3MB.tiff | pithos | archive-max | Limite de recursos excedido: group expansion ratio |
| single-0024-____C__Projetos_TESTE_tst_compact_structured_csv_sample_file_25MB.csv | pithos | balanced | Limite de recursos excedido: group expansion ratio |
| single-0024-____C__Projetos_TESTE_tst_compact_structured_csv_sample_file_25MB.csv | pithos | archive-max | Limite de recursos excedido: group expansion ratio |
| single-0025-____C__Projetos_TESTE_tst_compact_structured_json_sample_file_25MB.json | pithos | balanced | Limite de recursos excedido: group expansion ratio |
| single-0025-____C__Projetos_TESTE_tst_compact_structured_json_sample_file_25MB.json | pithos | archive-max | Limite de recursos excedido: group expansion ratio |
| combined-all | pithos | balanced | Limite de recursos excedido: group expansion ratio |

## Current Phase 3 analysis probe

- Files: 31
- Input MiB: 476.187
- Logical chunks: 966
- Exact-dedup referenced chunks: 429
- Exact-dedup potential saved MiB: 269.437
- Exact-dedup potential savings: 56.5822%

| Analysis stage | ms | % of measured analysis |
|---|---:|---:|
| scan | 217 | 5.416 |
| chunking | 299 | 7.462 |
| fingerprinting | 3321 | 82.88 |
| exact_dedup | 170 | 4.243 |

Exact-dedup savings are format-neutral potential until ChunkTable persistence is integrated into PAF.

