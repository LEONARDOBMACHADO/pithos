#![no_main]

use libfuzzer_sys::fuzz_target;
use pithos_format::RestoreMapRecord;

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<Vec<RestoreMapRecord>>(data);
});
