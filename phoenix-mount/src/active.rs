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

/// The live WinFsp mount behind an [`ActiveMount`]: read-only, or a writable
/// copy-on-write overlay. Both expose the same `disk_size` / `volumes` /
/// `backup_path` surface.
#[cfg(feature = "winfsp")]
enum WinfspInner {
    ReadOnly(crate::winfsp_mount::WinFspMount),
    Writable(crate::winfsp_mount::WinFspWritableMount),
}

pub struct ActiveMount {
    #[cfg(feature = "winfsp")]
    inner: WinfspInner,
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
        let inner = WinfspInner::ReadOnly(crate::winfsp_mount::WinFspMount::mount_selected(
            backup,
            scratch_dir,
            selection,
        )?);
        #[cfg(not(feature = "winfsp"))]
        let inner = crate::session::MountSession::mount_selected(backup, scratch_dir, selection)?;
        Ok(Self { inner })
    }

    /// Mount `backup` **writable** with a copy-on-write overlay: the backup is
    /// served read-only as a differencing parent and all writes land in an
    /// ephemeral child, so the `.phnx` is never touched and changes are
    /// discarded on unmount. `selection` holds the partition indices to expose;
    /// `preferred_letters` maps a partition index to the drive letter it should
    /// keep if free (to preserve letters when converting a read-only mount).
    ///
    /// Only the `winfsp` build supports this — the materialize fallback cannot
    /// do a zero-footprint writable overlay.
    #[cfg(feature = "winfsp")]
    pub fn open_writable_selected(
        backup: &Path,
        scratch_dir: &Path,
        selection: &[u32],
        preferred_letters: &[(u32, char)],
    ) -> Result<Self> {
        let inner = crate::winfsp_mount::WinFspWritableMount::mount_selected(
            backup,
            scratch_dir,
            selection,
            preferred_letters,
        )?;
        Ok(Self {
            inner: WinfspInner::Writable(inner),
        })
    }

    #[cfg(not(feature = "winfsp"))]
    pub fn open_writable_selected(
        _backup: &Path,
        _scratch_dir: &Path,
        _selection: &[u32],
        _preferred_letters: &[(u32, char)],
    ) -> Result<Self> {
        Err(phoenix_core::error::PhoenixError::Other(
            "writable mounts require the WinFsp build".into(),
        ))
    }

    /// Size of the mounted virtual disk in bytes.
    pub fn disk_size(&self) -> u64 {
        #[cfg(feature = "winfsp")]
        {
            match &self.inner {
                WinfspInner::ReadOnly(m) => m.disk_size,
                WinfspInner::Writable(m) => m.disk_size,
            }
        }
        #[cfg(not(feature = "winfsp"))]
        {
            self.inner.disk_size
        }
    }

    /// Per-partition exposure of a selection mount (empty for `None` mounts:
    /// letters were assigned by Windows, not tracked here).
    pub fn volumes(&self) -> &[crate::letters::MountedVolume] {
        #[cfg(feature = "winfsp")]
        {
            match &self.inner {
                WinfspInner::ReadOnly(m) => &m.volumes,
                WinfspInner::Writable(m) => &m.volumes,
            }
        }
        #[cfg(not(feature = "winfsp"))]
        {
            &self.inner.volumes
        }
    }

    /// Path of the backup that is mounted.
    pub fn backup_path(&self) -> &Path {
        #[cfg(feature = "winfsp")]
        {
            match &self.inner {
                WinfspInner::ReadOnly(m) => &m.backup_path,
                WinfspInner::Writable(m) => &m.backup_path,
            }
        }
        #[cfg(not(feature = "winfsp"))]
        {
            &self.inner.backup_path
        }
    }

    /// Whether this mount is a writable copy-on-write overlay (vs read-only).
    pub fn is_writable(&self) -> bool {
        #[cfg(feature = "winfsp")]
        {
            matches!(self.inner, WinfspInner::Writable(_))
        }
        #[cfg(not(feature = "winfsp"))]
        {
            false
        }
    }

    /// Whether this build mounts with no meaningful extra disk footprint.
    pub const fn space_efficient() -> bool {
        cfg!(feature = "winfsp")
    }

    /// Whether mounting can actually run right now. The `winfsp` build needs
    /// the WinFsp runtime present (see [`winfsp_mount::is_available`]); the
    /// materialize-to-temp-VHD fallback build needs nothing external, so it is
    /// always available. The GUI uses this to gate its Mount page.
    pub fn runtime_available() -> bool {
        #[cfg(feature = "winfsp")]
        {
            crate::winfsp_mount::is_available()
        }
        #[cfg(not(feature = "winfsp"))]
        {
            true
        }
    }
}
