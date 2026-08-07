//! Cumulative Pithos engine used by the staged performance/compression branches.
//!
//! `pithos-engine` remains the compatibility baseline. Stacked benchmark
//! branches add focused modules here and explicitly override only the public
//! operations advanced by that branch.

pub use pithos_engine_legacy::*;

mod adaptive_pack;
mod unpack_once;

pub use adaptive_pack::{pack, pack_with_control, pack_with_limits_and_control};
pub use unpack_once::{unpack, unpack_with_control, unpack_with_control_and_temp_limit};
