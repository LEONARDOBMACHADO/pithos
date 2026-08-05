#![no_main]

use libfuzzer_sys::fuzz_target;
use pithos_format::CodecRegistry;

fuzz_target!(|data: &[u8]| {
    let _ = CodecRegistry::decode(data, 4_096);
});
