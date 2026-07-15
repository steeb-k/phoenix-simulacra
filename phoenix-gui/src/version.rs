//! Compile-time build provenance for the GUI binary.
//!
//! The values come from `cargo:rustc-env` directives in `build.rs`
//! (see `windows/build_info.rs` for the shared emitter). They're resolved
//! at compile time via `env!()` so the *running* binary's hash is what
//! gets logged — exactly the question we couldn't answer before: "is the
//! process that user is running today the one I just rebuilt, or a stale
//! one from this morning?"
//!
//! Falls back to `"unknown"` for each field via `option_env!()` so a
//! contributor on a non-cargo build path (e.g. `rustc` directly, or a
//! `build.rs` that fails to invoke `emit_build_info`) still gets a
//! compiling binary rather than a missing-env-var compile error.

use phoenix_core::build_info::BuildInfo;

pub const BUILD_INFO: BuildInfo = BuildInfo {
    binary_name: "simulacra-gui",
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
