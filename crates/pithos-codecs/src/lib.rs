//! Codecs Portfolio Trait & STORE Codec

use pithos_core::{PithosError, Result};
use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    Store = 0,
    Zstd = 1,
    Brotli = 2,
    Lzma2 = 3,
}

impl CodecId {
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Store),
            1 => Some(Self::Zstd),
            2 => Some(Self::Brotli),
            3 => Some(Self::Lzma2),
            _ => None,
        }
    }
}

/// Fixed implementation version recorded by a future codec registry.
pub type CodecVersion = u16;

const CODEC_VERSION_V1: CodecVersion = 1;
const COPY_BUFFER_SIZE: usize = 8 * 1024;
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecConfig {
    pub level: i32,
}

impl CodecConfig {
    pub const fn deterministic_default(codec: CodecId) -> Self {
        let level = match codec {
            CodecId::Store => 0,
            CodecId::Zstd | CodecId::Brotli => 9,
            CodecId::Lzma2 => 6,
        };
        Self { level }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecStats {
    pub input_bytes: u64,
    pub output_bytes: u64,
}

/// The mandatory Zstd codec, with deterministic single-threaded parameters.
pub struct ZstdCodec;

/// The mandatory Brotli codec, with deterministic fixed quality and window.
pub struct BrotliCodec;

/// The mandatory LZMA2 codec, encoded in the XZ container with a fixed preset.
pub struct Lzma2Codec;

pub trait Codec: Send + Sync {
    fn id(&self) -> CodecId;
    fn version(&self) -> CodecVersion {
        CODEC_VERSION_V1
    }
    fn encode(&self, input: &[u8], cfg: &CodecConfig, output: &mut dyn Write)
    -> Result<CodecStats>;
    fn decode(&self, input: &mut dyn Read, expected_len: u64, output: &mut dyn Write)
    -> Result<()>;
    fn memory_bound(&self, input_len: u64, cfg: &CodecConfig) -> Result<u64>;
}

fn checked_memory_bound(input_len: u64, scratch_bytes: u64) -> Result<u64> {
    input_len
        .checked_add(scratch_bytes)
        .ok_or(PithosError::IntegerOverflow)
}

fn validate_level(cfg: &CodecConfig, range: std::ops::RangeInclusive<i32>) -> Result<()> {
    if range.contains(&cfg.level) {
        Ok(())
    } else {
        Err(PithosError::InvalidMetadata("unsupported codec level"))
    }
}

fn stats(input: &[u8], output_bytes: usize) -> CodecStats {
    CodecStats {
        input_bytes: input.len() as u64,
        output_bytes: output_bytes as u64,
    }
}

fn copy_decoded_limited(
    input: &mut dyn Read,
    expected_len: u64,
    output: &mut dyn Write,
) -> Result<()> {
    let mut buffer = [0_u8; COPY_BUFFER_SIZE];
    let mut written = 0_u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        written = written
            .checked_add(read as u64)
            .ok_or(PithosError::IntegerOverflow)?;
        if written > expected_len {
            return Err(PithosError::ResourceLimit(
                "decoded output exceeds declared group length",
            ));
        }
        output.write_all(&buffer[..read])?;
    }
    if written != expected_len {
        return Err(PithosError::InvalidRange);
    }
    Ok(())
}

/// Implementação do Codec STORE (RAW sem compressão)
pub struct StoreCodec;

impl Codec for StoreCodec {
    fn id(&self) -> CodecId {
        CodecId::Store
    }

    fn encode(
        &self,
        input: &[u8],
        cfg: &CodecConfig,
        output: &mut dyn Write,
    ) -> Result<CodecStats> {
        validate_level(cfg, 0..=0)?;
        output.write_all(input)?;
        Ok(stats(input, input.len()))
    }

    fn decode(
        &self,
        input: &mut dyn Read,
        expected_len: u64,
        output: &mut dyn Write,
    ) -> Result<()> {
        let mut buffer = vec![0u8; 8192];
        let mut remaining = expected_len;
        while remaining > 0 {
            let to_read = (buffer.len() as u64).min(remaining) as usize;
            let read = input.read(&mut buffer[..to_read])?;
            if read == 0 {
                return Err(pithos_core::PithosError::InvalidRange);
            }
            output.write_all(&buffer[..read])?;
            remaining -= read as u64;
        }
        Ok(())
    }

    fn memory_bound(&self, input_len: u64, cfg: &CodecConfig) -> Result<u64> {
        validate_level(cfg, 0..=0)?;
        checked_memory_bound(input_len, 1)
    }
}

impl Codec for ZstdCodec {
    fn id(&self) -> CodecId {
        CodecId::Zstd
    }

    fn encode(
        &self,
        input: &[u8],
        cfg: &CodecConfig,
        output: &mut dyn Write,
    ) -> Result<CodecStats> {
        validate_level(cfg, -7..=22)?;
        // The streaming helper is single-threaded and deterministic.
        let encoded = zstd::stream::encode_all(input, cfg.level)?;
        output.write_all(&encoded)?;
        Ok(stats(input, encoded.len()))
    }

    fn decode(
        &self,
        input: &mut dyn Read,
        expected_len: u64,
        output: &mut dyn Write,
    ) -> Result<()> {
        let mut decoder = zstd::stream::read::Decoder::new(input)?;
        copy_decoded_limited(&mut decoder, expected_len, output)
    }

    fn memory_bound(&self, input_len: u64, cfg: &CodecConfig) -> Result<u64> {
        validate_level(cfg, -7..=22)?;
        checked_memory_bound(input_len, 128 * MIB)
    }
}

impl Codec for BrotliCodec {
    fn id(&self) -> CodecId {
        CodecId::Brotli
    }

    fn encode(
        &self,
        input: &[u8],
        cfg: &CodecConfig,
        output: &mut dyn Write,
    ) -> Result<CodecStats> {
        validate_level(cfg, 0..=11)?;
        // Window is deliberately fixed for reproducible output.
        let mut encoder =
            brotli::CompressorWriter::new(Vec::new(), COPY_BUFFER_SIZE, cfg.level as u32, 22);
        encoder.write_all(input)?;
        let encoded = encoder.into_inner();
        output.write_all(&encoded)?;
        Ok(stats(input, encoded.len()))
    }

    fn decode(
        &self,
        input: &mut dyn Read,
        expected_len: u64,
        output: &mut dyn Write,
    ) -> Result<()> {
        let mut decoder = brotli::Decompressor::new(input, COPY_BUFFER_SIZE);
        copy_decoded_limited(&mut decoder, expected_len, output)
    }

    fn memory_bound(&self, input_len: u64, cfg: &CodecConfig) -> Result<u64> {
        validate_level(cfg, 0..=11)?;
        checked_memory_bound(input_len, 32 * MIB)
    }
}

impl Codec for Lzma2Codec {
    fn id(&self) -> CodecId {
        CodecId::Lzma2
    }

    fn encode(
        &self,
        input: &[u8],
        cfg: &CodecConfig,
        output: &mut dyn Write,
    ) -> Result<CodecStats> {
        validate_level(cfg, 0..=9)?;
        // The crate's default encoder is single-threaded.
        let encoded = liblzma::encode_all(input, cfg.level as u32)?;
        output.write_all(&encoded)?;
        Ok(stats(input, encoded.len()))
    }

    fn decode(
        &self,
        input: &mut dyn Read,
        expected_len: u64,
        output: &mut dyn Write,
    ) -> Result<()> {
        let mut decoder = liblzma::read::XzDecoder::new(input);
        copy_decoded_limited(&mut decoder, expected_len, output)
    }

    fn memory_bound(&self, input_len: u64, cfg: &CodecConfig) -> Result<u64> {
        validate_level(cfg, 0..=9)?;
        let dictionary_bound = 1_u64
            .checked_shl((cfg.level as u32) + 20)
            .ok_or(PithosError::IntegerOverflow)?;
        checked_memory_bound(input_len, dictionary_bound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const SAMPLE: &[u8] =
        b"Pithos codec conformance vector: \x00\x01\x02 repeated repeated repeated.";

    fn config(codec: CodecId) -> CodecConfig {
        CodecConfig::deterministic_default(codec)
    }

    fn assert_round_trip(codec: &dyn Codec) {
        let mut encoded = Vec::new();
        let stats = codec
            .encode(SAMPLE, &config(codec.id()), &mut encoded)
            .unwrap();
        assert_eq!(stats.input_bytes, SAMPLE.len() as u64);
        assert_eq!(stats.output_bytes, encoded.len() as u64);
        let mut decoded = Vec::new();
        codec
            .decode(&mut Cursor::new(encoded), SAMPLE.len() as u64, &mut decoded)
            .unwrap();
        assert_eq!(decoded, SAMPLE);
    }

    #[test]
    fn every_mandatory_codec_round_trips_the_conformance_vector() {
        assert_round_trip(&StoreCodec);
        assert_round_trip(&ZstdCodec);
        assert_round_trip(&BrotliCodec);
        assert_round_trip(&Lzma2Codec);
    }

    #[test]
    fn compressed_codecs_are_deterministic_for_fixed_parameters() {
        for codec in [&ZstdCodec as &dyn Codec, &BrotliCodec, &Lzma2Codec] {
            let input = vec![b'a'; 32 * 1024];
            let mut first = Vec::new();
            let mut second = Vec::new();
            codec
                .encode(&input, &config(codec.id()), &mut first)
                .unwrap();
            codec
                .encode(&input, &config(codec.id()), &mut second)
                .unwrap();
            assert_eq!(first, second, "{:?} must be deterministic", codec.id());
        }
    }

    #[test]
    fn mandatory_codec_conformance_vectors_have_stable_bytes() {
        let codecs = [
            ("STORE", &StoreCodec as &dyn Codec),
            ("Zstandard", &ZstdCodec as &dyn Codec),
            ("Brotli", &BrotliCodec as &dyn Codec),
            ("LZMA2", &Lzma2Codec as &dyn Codec),
        ];
        let actual = codecs.map(|(name, codec)| {
            let mut encoded = Vec::new();
            codec
                .encode(SAMPLE, &config(codec.id()), &mut encoded)
                .unwrap();
            (
                name,
                encoded.len(),
                blake3::hash(&encoded).to_hex().to_string(),
            )
        });
        assert_eq!(
            actual,
            [
                (
                    "STORE",
                    64,
                    "50cc8ea42bc6bf890d36afe45d76241c2ab3534e14b7ea3f98eacfe975e3f09e".to_owned(),
                ),
                (
                    "Zstandard",
                    62,
                    "535bc8743723197f8ffeea422da15315837e9feb2527b573354402930e973b49".to_owned(),
                ),
                (
                    "Brotli",
                    58,
                    "9edb15fc9b624927b0f6a71b44016edcea83e515671609e15842254e222600dd".to_owned(),
                ),
                (
                    "LZMA2",
                    112,
                    "ae12db73ef72d5a167722828c74d6bc69848f7363cee6978439e734e64d2fbb9".to_owned(),
                ),
            ]
        );
    }

    #[test]
    fn decoder_rejects_output_larger_than_declared_length() {
        for codec in [&ZstdCodec as &dyn Codec, &BrotliCodec, &Lzma2Codec] {
            let mut encoded = Vec::new();
            codec
                .encode(&vec![7; 16 * 1024], &config(codec.id()), &mut encoded)
                .unwrap();
            let result = codec.decode(&mut Cursor::new(encoded), 1, &mut Vec::new());
            assert!(matches!(
                result,
                Err(pithos_core::PithosError::ResourceLimit(_))
            ));
        }
    }

    #[test]
    fn compressed_codecs_reject_corrupted_payloads() {
        for codec in [&ZstdCodec as &dyn Codec, &BrotliCodec, &Lzma2Codec] {
            let mut encoded = Vec::new();
            codec
                .encode(&vec![3; 4 * 1024], &config(codec.id()), &mut encoded)
                .unwrap();
            let corruption_offset = encoded.len() / 2;
            encoded[corruption_offset] ^= 0x80;
            assert!(
                codec
                    .decode(&mut Cursor::new(encoded), 4 * 1024, &mut Vec::new())
                    .is_err()
            );
        }
    }

    #[test]
    fn store_rejects_truncated_input() {
        let result = StoreCodec.decode(&mut Cursor::new([1, 2]), 3, &mut Vec::new());
        assert!(matches!(
            result,
            Err(pithos_core::PithosError::InvalidRange)
        ));
    }

    #[test]
    fn codec_memory_bounds_are_checked_and_nonzero() {
        for codec in [
            &StoreCodec as &dyn Codec,
            &ZstdCodec,
            &BrotliCodec,
            &Lzma2Codec,
        ] {
            let bound = codec.memory_bound(64 * 1024, &config(codec.id())).unwrap();
            assert!(bound >= 64 * 1024);
            assert_eq!(codec.version(), 1);
        }
        assert!(
            StoreCodec
                .memory_bound(u64::MAX, &config(CodecId::Store))
                .is_err()
        );
    }

    #[test]
    fn codec_rejects_levels_outside_its_supported_range() {
        let invalid = CodecConfig { level: 99 };
        for codec in [&ZstdCodec as &dyn Codec, &BrotliCodec, &Lzma2Codec] {
            assert!(codec.encode(SAMPLE, &invalid, &mut Vec::new()).is_err());
            assert!(codec.memory_bound(1, &invalid).is_err());
        }
    }
}
