//! Unified mount entry point for the CLI/GUI.
//!
//! When built with the `winfsp` feature this is the space-efficient WinFsp
//! on-demand mount (zero extra disk space). Without the feature it falls back
//! to the materialize-to-temp-VHD path — a dev/stopgap only. The fallback is a
//! BUILD choice, never a runtime one: a `winfsp` build that can't reach WinFsp
//! errors out rather than silently materializing (which would double the
//! footprint, the one thing mounting must never do).

use std::path::Path;

use phoenix_core::error::Result;

pub struct ActiveMount {
    #[cfg(feature = "winfsp")]
    inner: crate::winfsp_mount::WinFspMount,
    #[cfg(not(feature = "winfsp"))]
    inner: crate::session::MountSession,
}

impl ActiveMount {
    /// Mount `backup` read-only. `scratch_dir` holds any transient state (the
    /// WinFsp mount point, or the temp image for the fallback).
    pub fn open(backup: &Path, scratch_dir: &Path) -> Result<Self> {
        #[cfg(feature = "winfsp")]
        let inner = crate::winfsp_mount::WinFspMount::mount(backup, scratch_dir)?;
        #[cfg(not(feature = "winfsp"))]
        let inner = crate::session::MountSession::mount(backup, scratch_dir)?;
        Ok(Self { inner })
    }

    /// Size of the mounted virtual disk in bytes.
    pub fn disk_size(&self) -> u64 {
        self.inner.disk_size
    }

    /// Path of the backup that is mounted.
    pub fn backup_path(&self) -> &Path {
        &self.inner.backup_path
    }

    /// Whether this build mounts with no meaningful extra disk footprint.
    pub const fn space_efficient() -> bool {
        cfg!(feature = "winfsp")
    }
}
