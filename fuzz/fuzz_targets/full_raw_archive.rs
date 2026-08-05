#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    if data.len() > 16 * 1024 * 1024 {
        return;
    }
    if let Ok(mut archive) = tempfile::NamedTempFile::new()
        && archive.write_all(data).is_ok()
    {
        let _ = pithos_engine::verify(archive.path());
    }
});
