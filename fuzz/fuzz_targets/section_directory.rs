#![no_main]

use libfuzzer_sys::fuzz_target;
use pithos_format::{SECTION_ENTRY_LEN, SectionDirectoryRecord};

fuzz_target!(|data: &[u8]| {
    for chunk in data.chunks_exact(SECTION_ENTRY_LEN).take(64) {
        if let Ok(buffer) = <&[u8; SECTION_ENTRY_LEN]>::try_from(chunk) {
            let _ = SectionDirectoryRecord::decode(buffer);
        }
    }
});
