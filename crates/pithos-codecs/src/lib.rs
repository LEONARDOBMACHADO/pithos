//! Codecs Portfolio Trait & STORE Codec

use pithos_core::Result;
use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    Store = 0,
    Zstd = 1,
    Brotli = 2,
    Lzma2 = 3,
}

pub trait Codec: Send + Sync {
    fn id(&self) -> CodecId;
    fn encode(&self, input: &[u8], output: &mut dyn Write) -> Result<u64>;
    fn decode(&self, input: &mut dyn Read, expected_len: u64, output: &mut dyn Write)
    -> Result<()>;
}

/// Implementação do Codec STORE (RAW sem compressão)
pub struct StoreCodec;

impl Codec for StoreCodec {
    fn id(&self) -> CodecId {
        CodecId::Store
    }

    fn encode(&self, input: &[u8], output: &mut dyn Write) -> Result<u64> {
        output.write_all(input)?;
        Ok(input.len() as u64)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const SAMPLE: &[u8] = b"Pithos codec conformance vector: \x00\x01\x02 repeated repeated repeated.";

    fn assert_round_trip(codec: &dyn Codec) {
        let mut encoded = Vec::new();
        codec.encode(SAMPLE, &mut encoded).unwrap();
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
            codec.encode(&input, &mut first).unwrap();
            codec.encode(&input, &mut second).unwrap();
            assert_eq!(first, second, "{:?} must be deterministic", codec.id());
        }
    }

    #[test]
    fn decoder_rejects_output_larger_than_declared_length() {
        for codec in [&ZstdCodec as &dyn Codec, &BrotliCodec, &Lzma2Codec] {
            let mut encoded = Vec::new();
            codec.encode(&vec![7; 16 * 1024], &mut encoded).unwrap();
            let result = codec.decode(&mut Cursor::new(encoded), 1, &mut Vec::new());
            assert!(matches!(result, Err(pithos_core::PithosError::ResourceLimit(_))));
        }
    }

    #[test]
    fn store_rejects_truncated_input() {
        let result = StoreCodec.decode(&mut Cursor::new([1, 2]), 3, &mut Vec::new());
        assert!(matches!(result, Err(pithos_core::PithosError::InvalidRange)));
    }
}
