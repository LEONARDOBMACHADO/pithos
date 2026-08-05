#![no_main]

use libfuzzer_sys::fuzz_target;
use pithos_format::GroupTableRecord;

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<Vec<GroupTableRecord>>(data);
});
