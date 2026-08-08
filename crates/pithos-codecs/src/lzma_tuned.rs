use liblzma::read::XzEncoder;
use liblzma::stream::{Check, Filters, LzmaOptions, MtStreamBuilder};
use std::io::{Cursor, Error, Read, Result};

pub(crate) const ARCHIVE_MAX_DICT_SIZE: u32 = 128 * 1024 * 1024;
const ARCHIVE_MAX_BLOCK_SIZE: u64 = 256 * 1024 * 1024;
const ARCHIVE_MAX_THREADS: u32 = 2;

pub(crate) fn encode_archive_max(input: &[u8], level: u32) -> Result<Vec<u8>> {
    let mut options = LzmaOptions::new_preset(level).map_err(as_io_error)?;
    options.dict_size(ARCHIVE_MAX_DICT_SIZE);

    let mut filters = Filters::new();
    filters.lzma2(&options);

    let mut builder = MtStreamBuilder::new();
    builder
        .threads(ARCHIVE_MAX_THREADS)
        .block_size(ARCHIVE_MAX_BLOCK_SIZE)
        .filters(filters)
        .check(Check::Crc64)
        .timeout_ms(0);
    let stream = builder.encoder().map_err(as_io_error)?;
    let mut encoder = XzEncoder::new_stream(Cursor::new(input), stream);
    let mut encoded = Vec::new();
    encoder.read_to_end(&mut encoded)?;
    Ok(encoded)
}

fn as_io_error(error: liblzma::stream::Error) -> Error {
    Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use liblzma::read::XzDecoder;

    #[test]
    fn tuned_archive_max_stream_roundtrips() {
        let input = b"pithos-large-dictionary-vector".repeat(32 * 1024);
        let encoded = encode_archive_max(&input, 9).unwrap();
        let mut decoder = XzDecoder::new(Cursor::new(encoded));
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn tuned_archive_max_stream_is_deterministic() {
        let input = b"deterministic-parallel-lzma2".repeat(16 * 1024);
        let first = encode_archive_max(&input, 9).unwrap();
        let second = encode_archive_max(&input, 9).unwrap();
        assert_eq!(first, second);
    }
}
