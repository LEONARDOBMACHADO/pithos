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
