//! Cumulative Pithos engine used by the staged performance/compression branches.
//!
//! `pithos-engine` remains the compatibility baseline. Stacked benchmark
//! branches add focused modules here and explicitly override only the public
//! operations advanced by that branch.

pub use pithos_engine_legacy::*;

mod adaptive_pack;
mod native_archive;
mod native_pack;
mod native_verify;

pub use native_archive::{
    inspect, list, unpack, unpack_with_control, unpack_with_control_and_temp_limit,
};
pub use native_pack::{pack, pack_with_control, pack_with_limits_and_control};
pub use native_verify::{verify, verify_with_control, verify_with_limits};
