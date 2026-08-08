//! Cumulative Pithos engine used by the staged performance/compression branches.
//!
//! `pithos-engine` remains the compatibility baseline. Stacked benchmark
//! branches add focused modules here and explicitly override only the public
//! operations advanced by that branch.

pub use pithos_engine_legacy::*;

mod adaptive_pack;
mod affinity_plan;
mod archive_affinity;
mod compat_dispatch;
mod dedup_probe;
mod direct_pack;
mod native_archive;
mod native_pack;
mod native_verify;
mod prescreen_pack;

pub use compat_dispatch::{
    inspect, inspect_with_control, list, list_with_control, unpack, unpack_with_control,
    unpack_with_control_and_temp_limit, verify, verify_with_control, verify_with_limits,
};
pub use prescreen_pack::{pack, pack_with_control, pack_with_limits_and_control};
