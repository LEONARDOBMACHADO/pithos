//! Native codec v13: DNA-inspired compact reference stream.
//!
//! v12 proved that physical exact dedup can win, but encoded every chunk
//! reference and canonical length as fixed-width u32 values. v13 treats the
//! reference sequence as a compact symbolic alphabet: small canonical indexes
//! use unsigned LEB128 and repeated indexes use run tokens. Canonical lengths
//! are also varints. ArchiveMax races the two existing entropy backends and
//! records only the smaller payload. The transform is fully reversible and
//! falls back to the v12 decoder for older native payloads.

use pithos_analysis::{ChunkOrigin, ChunkingConfig, chunk_fastcdc};
use pithos_codecs::{Codec, CodecConfig, CodecId, Lzma2Codec, ZstdCodec};
use pithos_core::{PithosError, Result};
use std::collections::HashMap;
use std::io::Cursor;

pub const NATIVE_CODEC_ID: u16 = 4;
pub const NATIVE_CODEC_VERSION: u16 = 13;
const MAGIC: &[u8; 4] = b"PN13";
const HEADER_LEN: usize = 48;
const MAX_NATIVE_CHUNKS: usize = 50_000_000;
const ARCHIVE_MAX_NATIVE_LEVEL: i32 = 15;
const FLAG_RUN_REFERENCES: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}

pub fn encode_exact_dedup(
    input: &[u8],
    member_lengths: &[u64],
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    validate_members(input.len(), member_lengths)?;
    let config = ChunkingConfig::default();
    let mut canonical = Vec::<Vec<u8>>::new();
    let mut candidates = HashMap::<[u8; 32], Vec<u32>>::new();
    let mut sequence = Vec::<u32>::new();
    let mut gross_duplicate_bytes = 0_u64;
    let mut member_base = 0_u64;

    for (member_id, member_length) in member_lengths.iter().copied().enumerate() {
        let start = usize::try_from(member_base).map_err(|_| PithosError::IntegerOverflow)?;
        let member_len =
            usize::try_from(member_length).map_err(|_| PithosError::IntegerOverflow)?;
        let end = start
            .checked_add(member_len)
            .ok_or(PithosError::IntegerOverflow)?;
        let member = input.get(start..end).ok_or(PithosError::InvalidRange)?;
        let drafts = chunk_fastcdc(
            member,
            ChunkOrigin {
                entry_id: member_id as u64,
                object_id: 0,
                base_offset: member_base,
            },
            &config,
        )?;

        for draft in drafts {
            if sequence.len() >= MAX_NATIVE_CHUNKS {
                return Err(PithosError::ResourceLimit("native chunk count"));
            }
            let chunk_start =
                usize::try_from(draft.logical_offset).map_err(|_| PithosError::IntegerOverflow)?;
            let chunk_end = chunk_start
                .checked_add(draft.length as usize)
                .ok_or(PithosError::IntegerOverflow)?;
            let bytes = input
                .get(chunk_start..chunk_end)
                .ok_or(PithosError::InvalidRange)?;
            let hash = *blake3::hash(bytes).as_bytes();

            let mut canonical_index = None;
            if let Some(indexes) = candidates.get(&hash) {
                for index in indexes {
                    let candidate = canonical
                        .get(*index as usize)
                        .ok_or(PithosError::InvalidMetadata("native canonical index"))?;
                    if candidate.as_slice() == bytes {
                        canonical_index = Some(*index);
                        gross_duplicate_bytes = gross_duplicate_bytes
                            .checked_add(bytes.len() as u64)
                            .ok_or(PithosError::IntegerOverflow)?;
                        break;
                    }
                }
            }

            let index = if let Some(index) = canonical_index {
                index
            } else {
                let index = u32::try_from(canonical.len())
                    .map_err(|_| PithosError::ResourceLimit("native canonical chunks"))?;
                canonical.push(bytes.to_vec());
                candidates.entry(hash).or_default().push(index);
                index
            };
            sequence.push(index);
        }
        member_base = member_base
            .checked_add(member_length)
            .ok_or(PithosError::IntegerOverflow)?;
    }

    let chunk_count = u32::try_from(sequence.len())
        .map_err(|_| PithosError::ResourceLimit("native chunk count"))?;
    let canonical_count = u32::try_from(canonical.len())
        .map_err(|_| PithosError::ResourceLimit("native canonical chunks"))?;

    let mut sequence_stream = Vec::new();
    encode_reference_stream(&sequence, &mut sequence_stream);
    let sequence_bytes = u32::try_from(sequence_stream.len())
        .map_err(|_| PithosError::ResourceLimit("native reference stream"))?;

    let mut representation = Vec::new();
    representation
        .try_reserve(
            sequence_stream
                .len()
                .saturating_add(input.len().saturating_sub(gross_duplicate_bytes as usize)),
        )
        .map_err(|_| PithosError::MemoryLimit)?;
    representation.extend_from_slice(&sequence_stream);
    for bytes in &canonical {
        write_varint(bytes.len() as u64, &mut representation);
        representation.extend_from_slice(bytes);
    }

    let (inner_codec, compressed) = encode_inner(&representation, level)?;

    let mut payload = Vec::with_capacity(HEADER_LEN + compressed.len());
    payload.extend_from_slice(MAGIC);
    payload.extend_from_slice(&NATIVE_CODEC_VERSION.to_le_bytes());
    payload.extend_from_slice(&(inner_codec as u16).to_le_bytes());
    payload.extend_from_slice(&(input.len() as u64).to_le_bytes());
    payload.extend_from_slice(&(representation.len() as u64).to_le_bytes());
    payload.extend_from_slice(&chunk_count.to_le_bytes());
    payload.extend_from_slice(&canonical_count.to_le_bytes());
    payload.extend_from_slice(&gross_duplicate_bytes.to_le_bytes());
    payload.extend_from_slice(&sequence_bytes.to_le_bytes());
    payload.extend_from_slice(&FLAG_RUN_REFERENCES.to_le_bytes());
    payload.extend_from_slice(&compressed);

    let stats = NativeStats {
        chunk_count,
        canonical_chunks: canonical_count,
        gross_duplicate_bytes,
        representation_bytes: representation.len() as u64,
        encoded_bytes: payload.len() as u64,
    };
    Ok((payload, stats))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    if payload.len() < HEADER_LEN || &payload[..4] != MAGIC {
        return pithos_native_v12::decode_exact_dedup(payload, expected_len);
    }
    let version = read_u16(payload, 4)?;
    let inner_codec = read_u16(payload, 6)?;
    let original_len = read_u64(payload, 8)?;
    let representation_len = read_u64(payload, 16)?;
    let chunk_count = read_u32(payload, 24)? as usize;
    let canonical_count = read_u32(payload, 28)? as usize;
    let _gross_duplicate_bytes = read_u64(payload, 32)?;
    let sequence_bytes = read_u32(payload, 40)? as usize;
    let flags = read_u32(payload, 44)?;
    if version != NATIVE_CODEC_VERSION
        || original_len != expected_len
        || flags != FLAG_RUN_REFERENCES
    {
        return Err(PithosError::InvalidMetadata("native v13 header"));
    }
    if chunk_count > MAX_NATIVE_CHUNKS || canonical_count > chunk_count {
        return Err(PithosError::ResourceLimit("native chunk count"));
    }
    let codec_id = CodecId::from_u16(inner_codec).ok_or(PithosError::UnsupportedCodec)?;
    if !matches!(codec_id, CodecId::Zstd | CodecId::Lzma2) {
        return Err(PithosError::UnsupportedCodec);
    }
    let representation_len_usize =
        usize::try_from(representation_len).map_err(|_| PithosError::MemoryLimit)?;
    let mut representation = Vec::with_capacity(representation_len_usize);
    codec_for_id(codec_id).decode(
        &mut Cursor::new(&payload[HEADER_LEN..]),
        representation_len,
        &mut representation,
    )?;
    if representation.len() != representation_len_usize || sequence_bytes > representation.len() {
        return Err(PithosError::InvalidRange);
    }

    let sequence = decode_reference_stream(&representation[..sequence_bytes], chunk_count)?;
    let mut cursor = sequence_bytes;
    let mut canonical = Vec::<&[u8]>::with_capacity(canonical_count);
    for _ in 0..canonical_count {
        let length = read_varint(&representation, &mut cursor)?;
        let length = usize::try_from(length).map_err(|_| PithosError::MemoryLimit)?;
        let end = cursor
            .checked_add(length)
            .ok_or(PithosError::IntegerOverflow)?;
        let bytes = representation
            .get(cursor..end)
            .ok_or(PithosError::InvalidRange)?;
        canonical.push(bytes);
        cursor = end;
    }
    if cursor != representation.len() {
        return Err(PithosError::InvalidMetadata("native v13 trailing bytes"));
    }

    let expected_len_usize = usize::try_from(expected_len).map_err(|_| PithosError::MemoryLimit)?;
    let mut output = Vec::with_capacity(expected_len_usize);
    for index in sequence {
        let bytes = canonical
            .get(index as usize)
            .ok_or(PithosError::InvalidMetadata("native reference"))?;
        if output
            .len()
            .checked_add(bytes.len())
            .is_none_or(|next| next > expected_len_usize)
        {
            return Err(PithosError::ResourceLimit("native decoded output"));
        }
        output.extend_from_slice(bytes);
    }
    if output.len() != expected_len_usize {
        return Err(PithosError::InvalidRange);
    }
    Ok(output)
}

fn encode_inner(representation: &[u8], level: i32) -> Result<(CodecId, Vec<u8>)> {
    if level < ARCHIVE_MAX_NATIVE_LEVEL {
        return Ok((
            CodecId::Zstd,
            encode_with(CodecId::Zstd, level.clamp(-7, 19), representation)?,
        ));
    }
    let (zstd, lzma) = std::thread::scope(|scope| {
        let zstd = scope.spawn(|| encode_with(CodecId::Zstd, 19, representation));
        let lzma = scope.spawn(|| encode_with(CodecId::Lzma2, 9, representation));
        (zstd.join(), lzma.join())
    });
    let zstd = zstd.map_err(|_| PithosError::InvalidMetadata("native zstd worker panic"))??;
    let lzma = lzma.map_err(|_| PithosError::InvalidMetadata("native lzma worker panic"))??;
    if lzma.len() < zstd.len() {
        Ok((CodecId::Lzma2, lzma))
    } else {
        Ok((CodecId::Zstd, zstd))
    }
}

fn encode_with(codec: CodecId, level: i32, input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    codec_for_id(codec).encode(input, &CodecConfig { level }, &mut output)?;
    Ok(output)
}

fn encode_reference_stream(sequence: &[u32], output: &mut Vec<u8>) {
    let mut cursor = 0usize;
    while cursor < sequence.len() {
        let index = sequence[cursor];
        let mut run = 1usize;
        while cursor + run < sequence.len() && sequence[cursor + run] == index {
            run += 1;
        }
        if run >= 3 {
            write_varint(((run as u64) << 1) | 1, output);
            write_varint(index as u64, output);
            cursor += run;
        } else {
            for _ in 0..run {
                write_varint((index as u64) << 1, output);
                cursor += 1;
            }
        }
    }
}

fn decode_reference_stream(bytes: &[u8], expected: usize) -> Result<Vec<u32>> {
    let mut output = Vec::with_capacity(expected);
    let mut cursor = 0usize;
    while cursor < bytes.len() && output.len() < expected {
        let token = read_varint(bytes, &mut cursor)?;
        if token & 1 == 0 {
            let index = u32::try_from(token >> 1).map_err(|_| PithosError::IntegerOverflow)?;
            output.push(index);
        } else {
            let run = usize::try_from(token >> 1).map_err(|_| PithosError::IntegerOverflow)?;
            if run < 3 || output.len().saturating_add(run) > expected {
                return Err(PithosError::InvalidMetadata("native reference run"));
            }
            let index = u32::try_from(read_varint(bytes, &mut cursor)?)
                .map_err(|_| PithosError::IntegerOverflow)?;
            output.extend(std::iter::repeat_n(index, run));
        }
    }
    if cursor != bytes.len() || output.len() != expected {
        return Err(PithosError::InvalidMetadata("native reference stream"));
    }
    Ok(output)
}

fn write_varint(mut value: u64, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    for _ in 0..10 {
        let byte = *bytes.get(*cursor).ok_or(PithosError::InvalidRange)?;
        *cursor = cursor.checked_add(1).ok_or(PithosError::IntegerOverflow)?;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(PithosError::InvalidMetadata("native varint"))
}

fn codec_for_id(codec_id: CodecId) -> &'static dyn Codec {
    static ZSTD: ZstdCodec = ZstdCodec;
    static LZMA2: Lzma2Codec = Lzma2Codec;
    match codec_id {
        CodecId::Zstd => &ZSTD,
        CodecId::Lzma2 => &LZMA2,
        _ => &ZSTD,
    }
}

fn validate_members(input_len: usize, member_lengths: &[u64]) -> Result<()> {
    if member_lengths.is_empty() && input_len != 0 {
        return Err(PithosError::InvalidMetadata("native member boundaries"));
    }
    let total = member_lengths.iter().try_fold(0_u64, |total, length| {
        total
            .checked_add(*length)
            .ok_or(PithosError::IntegerOverflow)
    })?;
    if total != input_len as u64 {
        return Err(PithosError::InvalidMetadata("native member boundaries"));
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or(PithosError::InvalidRange)?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_stream_roundtrip_with_runs() {
        let source = [1, 1, 1, 1, 2, 3, 3, 7, 7, 7];
        let mut encoded = Vec::new();
        encode_reference_stream(&source, &mut encoded);
        assert_eq!(
            decode_reference_stream(&encoded, source.len()).unwrap(),
            source
        );
        assert!(encoded.len() < source.len() * 4);
    }

    #[test]
    fn exact_duplicate_members_roundtrip() {
        let file = b"duplicate-member-payload".repeat(64 * 1024);
        let mut input = file.clone();
        input.extend_from_slice(&file);
        let lengths = [file.len() as u64, file.len() as u64];
        let (payload, stats) = encode_exact_dedup(&input, &lengths, 15).unwrap();
        assert!(stats.gross_duplicate_bytes >= file.len() as u64);
        assert_eq!(
            decode_exact_dedup(&payload, input.len() as u64).unwrap(),
            input
        );
    }
}
