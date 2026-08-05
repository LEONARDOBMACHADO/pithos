#![no_main]

use libfuzzer_sys::fuzz_target;
use pithos_agent_api::{ProtocolLimits, parse_request};

fuzz_target!(|data: &[u8]| {
    let _ = parse_request(data, &ProtocolLimits::default());
});
