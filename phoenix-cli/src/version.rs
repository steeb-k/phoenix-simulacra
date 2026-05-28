//! Compile-time build provenance for the CLI binary.
//!
//! Mirror of `phoenix-gui/src/version.rs` — see that file's doc comment
//! for the design rationale on why every binary needs its own copy.

use phoenix_core::build_info::BuildInfo;

pub const BUILD_INFO: BuildInfo = BuildInfo {
    binary_name: "carbon-phoenix",
    version: env!("CARGO_PKG_VERSION"),
    git_hash: match option_env!("PHOENIX_BUILD_GIT_HASH") {
        Some(v) => v,
        None => "unknown",
    },
    git_dirty: match option_env!("PHOENIX_BUILD_GIT_DIRTY") {
        Some(v) => v,
        None => "unknown",
    },
    build_timestamp: match option_env!("PHOENIX_BUILD_TIMESTAMP") {
        Some(v) => v,
        None => "unknown",
    },
    profile: match option_env!("PHOENIX_BUILD_PROFILE") {
        Some(v) => v,
        None => "unknown",
    },
    target_triple: match option_env!("PHOENIX_BUILD_TARGET") {
        Some(v) => v,
        None => "unknown",
    },
    rustc_version: match option_env!("PHOENIX_BUILD_RUSTC") {
        Some(v) => v,
        None => "unknown",
    },
};
