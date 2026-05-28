// Shared `build.rs` helper that captures git + toolchain provenance into
// `cargo:rustc-env=PHOENIX_BUILD_*` directives. Each binary crate's own
// `build.rs` does `include!("../windows/build_info.rs")` and then calls
// `emit_build_info()`. The file deliberately lives outside any crate so
// both `phoenix-gui/build.rs` and `phoenix-cli/build.rs` use a *single*
// definition — we got bitten before by per-crate copies drifting out of
// sync and then arguing for hours about which build a user was running.
//
// Why `include!` rather than a build-dep crate: `build-dependencies` add
// noticeable compile time because cargo treats them as a separate
// compilation unit per crate that depends on them, and the logic here is
// ~80 lines of `Command` invocations with no third-party deps. The
// include-from-workspace pattern keeps the build dependency graph flat.
//
// All identifiers below are marked `#[allow(dead_code)]` individually
// because `include!`-style sharing means an inner `#![allow(...)]`
// attribute isn't allowed (it would have to come before all items in
// the *containing* file, which is the consuming `build.rs`).

use std::process::Command;

/// Drive the whole build-info pipeline. Call exactly once from `main()`
/// in the consuming crate's `build.rs`. Each emitted env var is also
/// retrievable from the consuming crate's source via `env!("PHOENIX_BUILD_*")`.
///
/// All git invocations are best-effort: a failed `git` (not installed,
/// no repo, detached HEAD on a worktree without `.git/HEAD`, …) does
/// **not** fail the build. Instead the corresponding field is set to
/// `"unknown"` so the runtime banner stays readable.
#[allow(dead_code)]
pub fn emit_build_info() {
    let git_hash = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let git_dirty = match git(&["status", "--porcelain"]) {
        Some(out) if out.trim().is_empty() => "clean".to_string(),
        Some(_) => "dirty".to_string(),
        None => "unknown".to_string(),
    };
    let git_branch =
        git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into());

    // Trigger a rebuild whenever the working tree's git state changes so
    // the embedded hash/dirty flag stays honest. Without these, cargo
    // will happily reuse a cached compilation unit even after a commit
    // or a `git stash` and the banner will lie about the source state.
    rerun_if_changed("../.git/HEAD");
    rerun_if_changed("../.git/index");

    let build_timestamp = now_iso8601();
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    let rustc = rustc_version().unwrap_or_else(|| "unknown".into());

    // `cargo:rustc-env=NAME=VALUE` exposes the var to `env!()` in the
    // *same* crate. These are NOT inherited by other crates — that's
    // why every binary that wants a banner needs to call this helper
    // from its own `build.rs`.
    emit("PHOENIX_BUILD_GIT_HASH", &git_hash);
    emit("PHOENIX_BUILD_GIT_DIRTY", &git_dirty);
    emit("PHOENIX_BUILD_GIT_BRANCH", &git_branch);
    emit("PHOENIX_BUILD_TIMESTAMP", &build_timestamp);
    emit("PHOENIX_BUILD_PROFILE", &profile);
    emit("PHOENIX_BUILD_TARGET", &target);
    emit("PHOENIX_BUILD_RUSTC", &rustc);
}

#[allow(dead_code)]
fn emit(key: &str, value: &str) {
    println!("cargo:rustc-env={}={}", key, value);
}

#[allow(dead_code)]
fn rerun_if_changed(path: &str) {
    println!("cargo:rerun-if-changed={}", path);
}

/// Run `git <args>` from the crate's manifest directory and return stdout
/// trimmed. Returns `None` on any failure mode so callers can decide on
/// their own fallback.
#[allow(dead_code)]
fn git(args: &[&str]) -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Capture `rustc --version` so a toolchain bump shows up in the banner.
/// We deliberately don't use `rustc-version` crate or similar — staying
/// dependency-free here keeps clean builds fast.
#[allow(dead_code)]
fn rustc_version() -> Option<String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = Command::new(rustc).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim().to_string())
}

/// Hand-rolled UTC ISO-8601 because pulling `chrono` as a build-dep would
/// add 30s+ to clean compiles on Windows. Resolution is seconds —
/// sufficient for "which build is this" forensics, more than we need.
#[allow(dead_code)]
fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = epoch_to_ymdhms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, minute, second
    )
}

/// Convert seconds-since-epoch to (Y, M, D, h, m, s) UTC. Standard
/// civil-from-days algorithm (Howard Hinnant), no leap-second handling
/// (we don't care; this is for a build banner).
#[allow(dead_code)]
fn epoch_to_ymdhms(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let time_of_day = (secs % 86_400) as u32;
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;

    // Hinnant's civil_from_days, with epoch 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m, d, hour, minute, second)
}
