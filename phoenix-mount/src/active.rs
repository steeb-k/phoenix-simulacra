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
        Self::open_selected(backup, scratch_dir, None)
    }

    /// Mount `backup` read-only, exposing drive letters only for the
    /// partitions in `selection` (backup partition indices). `None` exposes
    /// every volume via mount-manager policy.
    pub fn open_selected(
        backup: &Path,
        scratch_dir: &Path,
        selection: Option<&[u32]>,
    ) -> Result<Self> {
        #[cfg(feature = "winfsp")]
        let inner =
            crate::winfsp_mount::WinFspMount::mount_selected(backup, scratch_dir, selection)?;
        #[cfg(not(feature = "winfsp"))]
        let inner = crate::session::MountSession::mount_selected(backup, scratch_dir, selection)?;
        Ok(Self { inner })
    }

    /// Size of the mounted virtual disk in bytes.
    pub fn disk_size(&self) -> u64 {
        self.inner.disk_size
    }

    /// Per-partition exposure of a selection mount (empty for `None` mounts:
    /// letters were assigned by Windows, not tracked here).
    pub fn volumes(&self) -> &[crate::letters::MountedVolume] {
        &self.inner.volumes
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
