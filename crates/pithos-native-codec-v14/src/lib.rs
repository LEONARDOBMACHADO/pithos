//! Native codec v14: archive-global canonical pool with class-specific streams.
//!
//! This codec is intended to be fed one archive-wide solid input. FastCDC
//! canonicalization is global, so identical chunks can be referenced across
//! file/content classes. Canonical chunks are then partitioned into coarse
//! content classes and each class independently selects STORE, Zstd or LZMA2.
//! The reference stream is encoded separately. This keeps one global identity
//! space while allowing different entropy behaviour for text, structured data,
//! pre-compressed material and generic binary data.

use pithos_analysis::{ChunkOrigin, ChunkingConfig, chunk_fastcdc};
use pithos_codecs::{Codec, CodecConfig, CodecId, Lzma2Codec, StoreCodec, ZstdCodec};
use pithos_core::{PithosError, Result};
use std::collections::HashMap;
use std::io::Cursor;

pub const NATIVE_CODEC_ID: u16 = 4;
pub const NATIVE_CODEC_VERSION: u16 = 14;
const MAGIC: &[u8; 4] = b"PN14";
const HEADER_LEN: usize = 44;
const DESCRIPTOR_LEN: usize = 24;
const MAX_NATIVE_CHUNKS: usize = 50_000_000;
const CLASS_COUNT: usize = 8;
const ARCHIVE_MAX_NATIVE_LEVEL: i32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub chunk_count: u32,
    pub canonical_chunks: u32,
    pub gross_duplicate_bytes: u64,
    pub representation_bytes: u64,
    pub encoded_bytes: u64,
}

#[derive(Debug)]
struct StreamDescriptor {
    class_id: u8,
    codec_id: CodecId,
    item_count: u32,
    raw_len: u64,
    encoded_len: u64,
}

pub fn encode_exact_dedup(
    input: &[u8],
    member_lengths: &[u64],
    level: i32,
) -> Result<(Vec<u8>, NativeStats)> {
    validate_members(input.len(), member_lengths)?;
    let (canonical, sequence, gross_duplicate_bytes) = canonicalize(input, member_lengths)?;
    let chunk_count = u32::try_from(sequence.len())
        .map_err(|_| PithosError::ResourceLimit("native chunk count"))?;
    let canonical_count = u32::try_from(canonical.len())
        .map_err(|_| PithosError::ResourceLimit("native canonical chunks"))?;

    let mut reference_raw = Vec::new();
    encode_reference_stream(&sequence, &mut reference_raw);
    let (reference_codec, reference_encoded) = encode_reference(&reference_raw, level)?;
    let reference_raw_len = u32::try_from(reference_raw.len())
        .map_err(|_| PithosError::ResourceLimit("native reference stream"))?;
    let reference_encoded_len = u32::try_from(reference_encoded.len())
        .map_err(|_| PithosError::ResourceLimit("native reference stream"))?;

    let mut buckets: Vec<Vec<(u32, &[u8])>> = (0..CLASS_COUNT).map(|_| Vec::new()).collect();
    for (index, bytes) in canonical.iter().enumerate() {
        let class = classify(bytes) as usize;
        let index = u32::try_from(index).map_err(|_| PithosError::IntegerOverflow)?;
        buckets[class].push((index, bytes));
    }

    let mut descriptors = Vec::new();
    let mut encoded_streams = Vec::new();
    let mut representation_bytes = reference_raw.len() as u64;
    for (class_id, bucket) in buckets.iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        let mut raw = Vec::new();
        for (index, bytes) in bucket {
            write_varint(u64::from(*index), &mut raw);
            write_varint(bytes.len() as u64, &mut raw);
            raw.extend_from_slice(bytes);
        }
        representation_bytes = representation_bytes
            .checked_add(raw.len() as u64)
            .ok_or(PithosError::IntegerOverflow)?;
        let (codec_id, encoded) = encode_best(&raw, level)?;
        descriptors.push(StreamDescriptor {
            class_id: class_id as u8,
            codec_id,
            item_count: u32::try_from(bucket.len()).map_err(|_| PithosError::IntegerOverflow)?,
            raw_len: raw.len() as u64,
            encoded_len: encoded.len() as u64,
        });
        encoded_streams.push(encoded);
    }

    let stream_count =
        u16::try_from(descriptors.len()).map_err(|_| PithosError::IntegerOverflow)?;
    let descriptor_bytes = descriptors
        .len()
        .checked_mul(DESCRIPTOR_LEN)
        .ok_or(PithosError::IntegerOverflow)?;
    let encoded_body =
        encoded_streams
            .iter()
            .try_fold(reference_encoded.len(), |total, stream| {
                total
                    .checked_add(stream.len())
                    .ok_or(PithosError::IntegerOverflow)
            })?;
    let capacity = HEADER_LEN
        .checked_add(descriptor_bytes)
        .and_then(|value| value.checked_add(encoded_body))
        .ok_or(PithosError::IntegerOverflow)?;
    let mut payload = Vec::with_capacity(capacity);
    payload.extend_from_slice(MAGIC);
    payload.extend_from_slice(&NATIVE_CODEC_VERSION.to_le_bytes());
    payload.extend_from_slice(&stream_count.to_le_bytes());
    payload.extend_from_slice(&(input.len() as u64).to_le_bytes());
    payload.extend_from_slice(&chunk_count.to_le_bytes());
    payload.extend_from_slice(&canonical_count.to_le_bytes());
    payload.extend_from_slice(&gross_duplicate_bytes.to_le_bytes());
    payload.extend_from_slice(&reference_raw_len.to_le_bytes());
    payload.extend_from_slice(&reference_encoded_len.to_le_bytes());
    payload.extend_from_slice(&(reference_codec as u16).to_le_bytes());
    payload.extend_from_slice(&0_u16.to_le_bytes());
    for descriptor in &descriptors {
        payload.push(descriptor.class_id);
        payload.push(descriptor.codec_id as u8);
        payload.extend_from_slice(&0_u16.to_le_bytes());
        payload.extend_from_slice(&descriptor.item_count.to_le_bytes());
        payload.extend_from_slice(&descriptor.raw_len.to_le_bytes());
        payload.extend_from_slice(&descriptor.encoded_len.to_le_bytes());
    }
    payload.extend_from_slice(&reference_encoded);
    for stream in encoded_streams {
        payload.extend_from_slice(&stream);
    }

    Ok((
        payload.clone(),
        NativeStats {
            chunk_count,
            canonical_chunks: canonical_count,
            gross_duplicate_bytes,
            representation_bytes,
            encoded_bytes: payload.len() as u64,
        },
    ))
}

pub fn decode_exact_dedup(payload: &[u8], expected_len: u64) -> Result<Vec<u8>> {
    if payload.len() < HEADER_LEN || &payload[..4] != MAGIC {
        return pithos_native_v13::decode_exact_dedup(payload, expected_len);
    }
    let version = read_u16(payload, 4)?;
    let stream_count = read_u16(payload, 6)? as usize;
    let original_len = read_u64(payload, 8)?;
    let chunk_count = read_u32(payload, 16)? as usize;
    let canonical_count = read_u32(payload, 20)? as usize;
    let _gross_duplicate_bytes = read_u64(payload, 24)?;
    let reference_raw_len = read_u32(payload, 32)? as usize;
    let reference_encoded_len = read_u32(payload, 36)? as usize;
    let reference_codec = codec_from_u16(read_u16(payload, 40)?)?;
    let reserved = read_u16(payload, 42)?;
    if version != NATIVE_CODEC_VERSION || original_len != expected_len || reserved != 0 {
        return Err(PithosError::InvalidMetadata("native v14 header"));
    }
    if chunk_count > MAX_NATIVE_CHUNKS
        || canonical_count > chunk_count
        || stream_count > CLASS_COUNT
    {
        return Err(PithosError::ResourceLimit("native v14 counts"));
    }

    let descriptors_end = HEADER_LEN
        .checked_add(
            stream_count
                .checked_mul(DESCRIPTOR_LEN)
                .ok_or(PithosError::IntegerOverflow)?,
        )
        .ok_or(PithosError::IntegerOverflow)?;
    if descriptors_end > payload.len() {
        return Err(PithosError::InvalidRange);
    }
    let mut descriptors = Vec::with_capacity(stream_count);
    let mut cursor = HEADER_LEN;
    for _ in 0..stream_count {
        let class_id = *payload.get(cursor).ok_or(PithosError::InvalidRange)?;
        let codec_id = codec_from_u16(u16::from(
            *payload.get(cursor + 1).ok_or(PithosError::InvalidRange)?,
        ))?;
        let reserved = read_u16(payload, cursor + 2)?;
        let item_count = read_u32(payload, cursor + 4)?;
        let raw_len = read_u64(payload, cursor + 8)?;
        let encoded_len = read_u64(payload, cursor + 16)?;
        if class_id as usize >= CLASS_COUNT || reserved != 0 {
            return Err(PithosError::InvalidMetadata("native v14 stream descriptor"));
        }
        descriptors.push(StreamDescriptor {
            class_id,
            codec_id,
            item_count,
            raw_len,
            encoded_len,
        });
        cursor += DESCRIPTOR_LEN;
    }

    let ref_end = cursor
        .checked_add(reference_encoded_len)
        .ok_or(PithosError::IntegerOverflow)?;
    let reference_encoded = payload
        .get(cursor..ref_end)
        .ok_or(PithosError::InvalidRange)?;
    let reference_raw =
        decode_stream(reference_codec, reference_encoded, reference_raw_len as u64)?;
    let sequence = decode_reference_stream(&reference_raw, chunk_count)?;
    cursor = ref_end;

    let mut canonical: Vec<Option<Vec<u8>>> = (0..canonical_count).map(|_| None).collect();
    let mut filled = 0usize;
    for descriptor in descriptors {
        let encoded_len =
            usize::try_from(descriptor.encoded_len).map_err(|_| PithosError::MemoryLimit)?;
        let end = cursor
            .checked_add(encoded_len)
            .ok_or(PithosError::IntegerOverflow)?;
        let encoded = payload.get(cursor..end).ok_or(PithosError::InvalidRange)?;
        let raw = decode_stream(descriptor.codec_id, encoded, descriptor.raw_len)?;
        let mut raw_cursor = 0usize;
        for _ in 0..descriptor.item_count {
            let index = usize::try_from(read_varint(&raw, &mut raw_cursor)?)
                .map_err(|_| PithosError::IntegerOverflow)?;
            let length = usize::try_from(read_varint(&raw, &mut raw_cursor)?)
                .map_err(|_| PithosError::MemoryLimit)?;
            let item_end = raw_cursor
                .checked_add(length)
                .ok_or(PithosError::IntegerOverflow)?;
            let bytes = raw
                .get(raw_cursor..item_end)
                .ok_or(PithosError::InvalidRange)?;
            let slot = canonical
                .get_mut(index)
                .ok_or(PithosError::InvalidMetadata("native canonical index"))?;
            if slot.is_some() {
                return Err(PithosError::InvalidMetadata(
                    "duplicate native canonical index",
                ));
            }
            *slot = Some(bytes.to_vec());
            filled += 1;
            raw_cursor = item_end;
        }
        if raw_cursor != raw.len() {
            return Err(PithosError::InvalidMetadata(
                "native v14 trailing class bytes",
            ));
        }
        cursor = end;
    }
    if cursor != payload.len() || filled != canonical_count {
        return Err(PithosError::InvalidMetadata("native v14 stream coverage"));
    }

    let expected_len_usize = usize::try_from(expected_len).map_err(|_| PithosError::MemoryLimit)?;
    let mut output = Vec::with_capacity(expected_len_usize);
    for index in sequence {
        let bytes = canonical
            .get(index as usize)
            .and_then(Option::as_ref)
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

fn canonicalize(input: &[u8], member_lengths: &[u64]) -> Result<(Vec<Vec<u8>>, Vec<u32>, u64)> {
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
            let mut found = None;
            if let Some(indexes) = candidates.get(&hash) {
                for index in indexes {
                    if canonical[*index as usize].as_slice() == bytes {
                        found = Some(*index);
                        gross_duplicate_bytes = gross_duplicate_bytes
                            .checked_add(bytes.len() as u64)
                            .ok_or(PithosError::IntegerOverflow)?;
                        break;
                    }
                }
            }
            let index = match found {
                Some(index) => index,
                None => {
                    let index = u32::try_from(canonical.len())
                        .map_err(|_| PithosError::ResourceLimit("native canonical chunks"))?;
                    canonical.push(bytes.to_vec());
                    candidates.entry(hash).or_default().push(index);
                    index
                }
            };
            sequence.push(index);
        }
        member_base = member_base
            .checked_add(member_length)
            .ok_or(PithosError::IntegerOverflow)?;
    }
    Ok((canonical, sequence, gross_duplicate_bytes))
}

fn classify(bytes: &[u8]) -> u8 {
    if bytes.is_empty() {
        return 0;
    }
    if bytes.starts_with(b"\x89PNG")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF8")
        || bytes.starts_with(b"RIFF")
    {
        return 5;
    }
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"\x1f\x8b")
        || bytes.starts_with(b"7z\xbc\xaf\x27\x1c")
        || bytes.starts_with(b"\xfd7zXZ")
    {
        return 4;
    }
    let sample = &bytes[..bytes.len().min(4096)];
    let printable = sample
        .iter()
        .filter(|byte| matches!(**byte, b'\n' | b'\r' | b'\t' | 0x20..=0x7e))
        .count();
    if printable.saturating_mul(100) >= sample.len().saturating_mul(90) {
        let first = sample
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace());
        if matches!(first, Some(b'{') | Some(b'[') | Some(b'<')) {
            return 2;
        }
        return 1;
    }
    let zeros = sample.iter().filter(|byte| **byte == 0).count();
    if zeros.saturating_mul(100) >= sample.len().saturating_mul(12) {
        return 3;
    }
    if bytes.len() < 1024 {
        return 7;
    }
    6
}

fn encode_reference(raw: &[u8], level: i32) -> Result<(CodecId, Vec<u8>)> {
    if raw.len() < 128 {
        return Ok((CodecId::Store, raw.to_vec()));
    }
    let zstd = encode_with(CodecId::Zstd, level.clamp(1, 19), raw)?;
    if zstd.len() < raw.len() {
        Ok((CodecId::Zstd, zstd))
    } else {
        Ok((CodecId::Store, raw.to_vec()))
    }
}

fn encode_best(raw: &[u8], level: i32) -> Result<(CodecId, Vec<u8>)> {
    if raw.len() < 256 {
        return Ok((CodecId::Store, raw.to_vec()));
    }
    let zstd_level = if level >= ARCHIVE_MAX_NATIVE_LEVEL {
        19
    } else {
        level.clamp(1, 19)
    };
    let (zstd, lzma) = std::thread::scope(|scope| {
        let zstd = scope.spawn(|| encode_with(CodecId::Zstd, zstd_level, raw));
        let lzma = scope.spawn(|| encode_with(CodecId::Lzma2, 9, raw));
        (zstd.join(), lzma.join())
    });
    let zstd = zstd.map_err(|_| PithosError::InvalidMetadata("native zstd worker panic"))??;
    let lzma = lzma.map_err(|_| PithosError::InvalidMetadata("native lzma worker panic"))??;
    let mut best = (CodecId::Store, raw.to_vec());
    if zstd.len() < best.1.len() {
        best = (CodecId::Zstd, zstd);
    }
    if lzma.len() < best.1.len() {
        best = (CodecId::Lzma2, lzma);
    }
    Ok(best)
}

fn encode_with(codec: CodecId, level: i32, input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    codec_for_id(codec).encode(input, &CodecConfig { level }, &mut output)?;
    Ok(output)
}

fn decode_stream(codec: CodecId, encoded: &[u8], raw_len: u64) -> Result<Vec<u8>> {
    if codec == CodecId::Store {
        if encoded.len() as u64 != raw_len {
            return Err(PithosError::InvalidRange);
        }
        return Ok(encoded.to_vec());
    }
    let capacity = usize::try_from(raw_len).map_err(|_| PithosError::MemoryLimit)?;
    let mut output = Vec::with_capacity(capacity);
    codec_for_id(codec).decode(&mut Cursor::new(encoded), raw_len, &mut output)?;
    if output.len() != capacity {
        return Err(PithosError::InvalidRange);
    }
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
            output.push(u32::try_from(token >> 1).map_err(|_| PithosError::IntegerOverflow)?);
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

fn codec_from_u16(value: u16) -> Result<CodecId> {
    CodecId::from_u16(value).ok_or(PithosError::UnsupportedCodec)
}

fn codec_for_id(codec_id: CodecId) -> &'static dyn Codec {
    static STORE: StoreCodec = StoreCodec;
    static ZSTD: ZstdCodec = ZstdCodec;
    static LZMA2: Lzma2Codec = Lzma2Codec;
    match codec_id {
        CodecId::Store => &STORE,
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
    fn global_pool_roundtrips_across_mixed_members() {
        // v14 is exact chunk deduplication. Identical members are chunked from
        // the same local origin and therefore must share canonical chunks even
        // when unrelated text/binary members sit between them in the archive.
        // Shifted/embedded repetitions after different prefixes are handled by
        // the later implicit-global-reference transform rather than being an
        // invariant of FastCDC chunk boundaries.
        let text = b"{\"kind\":\"text\"}\n".repeat(32 * 1024);
        let binary = vec![0, 1, 2, 3].repeat(64 * 1024);
        let shared = b"shared-global-pool-block".repeat(64 * 1024);

        let mut input = text.clone();
        input.extend_from_slice(&shared);
        input.extend_from_slice(&binary);
        input.extend_from_slice(&shared);
        let lengths = [
            text.len() as u64,
            shared.len() as u64,
            binary.len() as u64,
            shared.len() as u64,
        ];

        let (payload, stats) = encode_exact_dedup(&input, &lengths, 15).unwrap();
        assert!(stats.gross_duplicate_bytes > 0);
        assert!(stats.canonical_chunks < stats.chunk_count);
        assert_eq!(
            decode_exact_dedup(&payload, input.len() as u64).unwrap(),
            input
        );
    }
}
