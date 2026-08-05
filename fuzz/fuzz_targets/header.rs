#![no_main]

use libfuzzer_sys::fuzz_target;
use pithos_format::{GlobalHeader, HEADER_LEN};

fuzz_target!(|data: &[u8]| {
    if let Ok(buffer) = <&[u8; HEADER_LEN]>::try_from(data) {
        let _ = GlobalHeader::decode(buffer);
    }
});
