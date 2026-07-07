//! Runtime-visible build provenance for both binaries.
//!
//! Each binary crate (`phoenix-gui`, `phoenix-cli`) has its own `build.rs`
//! that runs `git` at compile time and emits a handful of
//! `cargo:rustc-env=PHOENIX_BUILD_*` directives. Those env vars get baked
//! into the binary as compile-time string constants the crate can read via
//! `env!()`. The pattern is unavoidably per-crate: `env!()` is resolved
//! against the *consuming* crate's env, not the workspace, so `phoenix-core`
//! can't read those vars on a binary's behalf.
//!
//! What this module provides instead is the *format* and the *log shape* —
//! the binary populates a [`BuildInfo`] with its own `env!()` calls and
//! hands it here. That way the on-disk banner is identical for both
//! binaries and we have a single place to evolve it as we collect more
//! fields. See `phoenix-gui/src/version.rs` for the consumer side.
//!
//! Why this matters: when a user reports "the fix isn't taking effect",
//! the very first question is *which binary are they running?* — and
//! without the banner we've historically had no answer except "check
//! the mtime on the .exe" (which is brittle and easy to get wrong when
//! multiple builds exist on disk or a stale process is still resident).

use tracing::info;

/// Snapshot of the compile-time state of the binary calling us.
///
/// Every field is `&'static str` because the values come from
/// `env!()`/`option_env!()` and are baked into the binary at build time.
/// Missing fields (e.g. `git` not on PATH when building) should be passed
/// as `"unknown"` rather than left empty so the banner stays scannable.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo {
    /// Display name of the binary (e.g. `"carbon-phoenix-gui"`). Lets the
    /// banner tell GUI and CLI sessions apart in a multiplexed log.
    pub binary_name: &'static str,
    /// Crate version from `CARGO_PKG_VERSION` (the value of
    /// `version` in the binary crate's `Cargo.toml`).
    pub version: &'static str,
    /// `git rev-parse HEAD` short hash at build time, or `"unknown"` if
    /// the build host had no git / no repo.
    pub git_hash: &'static str,
    /// `"clean"` or `"dirty"` based on `git status --porcelain` being
    /// empty at build time. `"unknown"` if git wasn't available.
    pub git_dirty: &'static str,
    /// ISO-8601 UTC timestamp captured by `build.rs` (e.g.
    /// `"2026-05-27T17:25:00Z"`). This is the *build* time, not the
    /// process-start time.
    pub build_timestamp: &'static str,
    /// `"debug"` or `"release"` from cargo's `PROFILE` env var.
    pub profile: &'static str,
    /// Rust target triple from cargo's `TARGET` env var (e.g.
    /// `"x86_64-pc-windows-msvc"`).
    pub target_triple: &'static str,
    /// `rustc --version` output captured at build time. Helps when a
    /// toolchain bump is suspected of the bug.
    pub rustc_version: &'static str,
}

impl BuildInfo {
    /// Compact one-line summary suitable for window titles / status bars.
    pub fn short(&self) -> String {
        format!(
            "{name} {ver} ({hash}{dirty}, {profile})",
            name = self.binary_name,
            ver = self.version,
            hash = self.git_hash,
            dirty = if self.git_dirty == "dirty" {
                "-dirty"
            } else {
                ""
            },
            profile = self.profile,
        )
    }
}

/// Emit a single, hard-to-miss multi-line `INFO` banner so every log file
/// starts with a clear "this binary, built then, by that toolchain"
/// header. The banner intentionally uses `=` separators and a fixed prefix
/// so it's easy to spot when scrolling a long log.
///
/// Also logs the runtime context (process ID, executable path, OS, current
/// working dir, host) so the operator can correlate a backup run with the
/// machine and the exact binary on disk that produced it.
pub fn log_startup_banner(info: &BuildInfo) {
    let pid = std::process::id();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<current_exe failed: {e}>"));
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<current_dir failed: {e}>"));
    let host = hostname();

    info!(target: "phoenix_build", "================ Carbon Phoenix session start ================");
    info!(
        target: "phoenix_build",
        binary = info.binary_name,
        version = info.version,
        git_hash = info.git_hash,
        git_dirty = info.git_dirty,
        profile = info.profile,
        build_timestamp = info.build_timestamp,
        target = info.target_triple,
        rustc = info.rustc_version,
        "build provenance"
    );
    info!(
        target: "phoenix_build",
        pid = pid,
        exe = %exe,
        cwd = %cwd,
        host = %host,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "runtime context"
    );
    info!(target: "phoenix_build", "==============================================================");
}

/// Best-effort hostname lookup so the banner identifies the machine the
/// backup ran on. Tries `COMPUTERNAME` (Windows) then `HOSTNAME` (Unix-y),
/// falling back to `"unknown"` rather than panicking — a missing hostname
/// is never a fatal condition.
fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into())
}
