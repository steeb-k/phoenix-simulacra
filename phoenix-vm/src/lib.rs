//! Boot Phoenix Simulacra `.phnx` backups as QEMU virtual machines.
//!
//! Layered so the deterministic, testable parts build without QEMU, WinFsp, or
//! elevation, and only the live boot pulls those in:
//!
//! - [`config`] — derive a [`config::VmConfig`] from a backup's manifest and
//!   turn it into a QEMU command line (pure; unit-tested).
//! - [`qemu`] — locate an external QEMU install and its firmware.
//! - [`session`] — persistent, resumable VM sessions on disk (pure I/O; tested).
//! - [`boot`] — tie it together and launch (behind the `winfsp` feature).
//!
//! See `docs/VIRTUALIZATION.md` for the design and the boot-smoke findings the
//! defaults encode.

pub mod config;
pub mod qemu;
pub mod session;

#[cfg(feature = "winfsp")]
pub mod boot;

pub use config::{Accel, DiskController, DiskSource, Firmware, HostOptions, VmConfig};
pub use qemu::Qemu;
pub use session::{
    serve_scratch_for_backup, vm_root_for_backup, volume_root, Session, SessionManager,
    SessionMeta, WriteLayer,
};

/// Current time as an RFC 3339 string, for session timestamps. Kept in one
/// place so callers don't each reach for the clock.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}
