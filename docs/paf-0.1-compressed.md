# PAF 0.1 compressed extension

Status: implemented and covered by local conformance, roundtrip, fuzz-build and
resource-limit gates.

This document describes the compressed extension of the PAF 0.1-draft container.
The global header, section directory, footer, entry table, central index and
integrity rules remain those of the RAW baseline. A compressed archive adds a
required `CodecRegistry` section and permits one physical group to restore
multiple logical file entries.

## Mandatory codecs

| Codec ID | Name | Version | Deterministic default level |
|---:|---|---:|---:|
| 0 | STORE | 1 | 0 |
| 1 | Zstandard | 1 | 9 |
| 2 | Brotli | 1 | 9, fixed window 22 |
| 3 | LZMA2 in XZ | 1 | 6 |

Encoding is single-threaded within each codec invocation. The scheduler may run
independent groups concurrently, but output assembly is ordered by group ID.
Conformance tests pin both encoded length and BLAKE3 for a fixed vector for every
mandatory codec.

## Codec registry

The section begins with a little-endian `u32` record count followed by fixed
16-byte records:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u32` | non-zero chain ID; zero is reserved for RAW |
| 4 | `u16` | codec ID |
| 6 | `u16` | codec implementation version |
| 8 | `i32` | codec level |
| 12 | `u32` | flags; bit 0 means required |

Chain IDs must be unique. Unknown flags, duplicate/reserved chains, unsupported
required codecs, inconsistent lengths and counts over the decoder limit are
rejected before payload decoding.

## Profiles and solid groups

| Profile | Target | Candidates |
|---|---:|---|
| `stream` | 4 MiB | STORE, Zstandard |
| `random` | 8 MiB | STORE, Zstandard |
| `balanced` | 64 MiB | STORE, Zstandard, Brotli, LZMA2 |
| `archive-max` | 512 MiB | STORE, Zstandard, Brotli, LZMA2 |

Files are ordered by their normalized archive path. Consecutive files are added
to a group while their checked total does not exceed the profile target. A file
larger than the target remains one unsplit group. Empty files retain their
logical position and restore mapping.

Every candidate uses fixed parameters. Selection minimizes payload plus codec,
group, index, integrity and padding costs. Equal totals are resolved by the
lowest codec ID. STORE is always a candidate, so incompressible data does not
need to expand merely to select a compressed backend.

Each group record names its codec chain and contains checked compressed and
uncompressed lengths, chunk count, payload offset and CRC32C. The restore map
maps every logical entry to a checked `(group_id, group_offset, length)` slice.
Extraction decodes only the owning group and validates the selected entry's
BLAKE3 before publication.

## Resource and integrity rules

- input size, metadata, entries, memory, temporary space and final output have
  independent limits;
- codec tasks declare input, scratch and output bounds before scheduler dispatch;
- decoded output cannot exceed the declared group length or expansion limits;
- compressed input is bound to the file identity, length and modification time
  captured during scan, then verified by BLAKE3 across the second read;
- group payload CRC32C, entry BLAKE3 and archive integrity root are mandatory;
- cancellation is checked during scan, hashing, group reads, codec selection and
  transactional publication;
- a failure never publishes a partial final archive or restore destination.

The decoder accepts RAW archives without a registry and compressed archives with
exactly one valid registry. A registry/section-count mismatch or non-zero codec
chain without a registry is a format error.
