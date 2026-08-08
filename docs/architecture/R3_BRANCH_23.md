# R3 branch 23

Native v13 introduces compact symbolic references before entropy coding: unsigned LEB128 canonical indexes, run tokens for repeated references, unsigned LEB128 canonical chunk lengths, and ArchiveMax arbitration between Zstd 19 and LZMA2 9. All older native payloads remain readable through the v12 fallback.
