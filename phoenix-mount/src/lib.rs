//! Mounting `.phnx` backups as browsable read-only disks.
//!
//! The shipping strategy — the `winfsp` cargo feature, which the GUI/CLI
//! binaries enable by default — serves reads on demand straight from the
//! compressed `.phnx` through a WinFsp filesystem, so mounting adds no
//! meaningful disk footprint. It needs the WinFsp SDK (+ libclang) to build
//! and WinFsp installed to run.
//!
//! Without the feature (`--no-default-features` on the binaries), a stopgap
//! materializes the backup into a full-size fixed-VHD image in a temp file
//! and attaches that — dev-only, since it briefly doubles the footprint.
//! Both paths synthesize identical bytes ([`SyntheticVhd`]) and decompress +
//! BLAKE3-verify every chunk they serve, so a corrupt backup fails at read
//! time rather than serving silent garbage.
//!
//! Either way the attached disk's volumes surface in Explorer via Windows'
//! own NTFS/FAT drivers; with a partition selection ([`ActiveMount`]
//! `open_selected`) only the chosen partitions get drive letters.

pub mod chunkstore;
pub mod gpt;
pub mod mbr;
pub mod image;
pub mod synthetic;
pub mod vhd;
pub mod vhdx;

#[cfg(windows)]
pub mod active;
#[cfg(windows)]
pub mod attach;
#[cfg(windows)]
pub mod letters;
#[cfg(windows)]
pub mod session;
#[cfg(all(windows, feature = "winfsp"))]
pub mod winfsp_mount;

#[cfg(windows)]
pub use active::ActiveMount;
pub use attach::{
    create_differencing_vhdx, set_disk_offline, wait_for_device_gone, AttachedDisk,
};
pub use chunkstore::{plan_layout, ChunkStore, PartitionSpan};
#[cfg(windows)]
pub use letters::MountedVolume;
#[cfg(windows)]
pub use session::MountSession;
pub use synthetic::SyntheticVhd;
#[cfg(all(windows, feature = "winfsp"))]
pub use winfsp_mount::{WinFspMount, WinFspServe, WinFspWritableMount};

/// The scratch directory mounts use for transient state — WinFsp mount-point
/// junctions and the writable overlay's differencing children. Callers pass
/// this into the mount entry points; [`cleanup_leaked_mounts`] sweeps it at
/// startup. Factored here so the CLI and GUI agree on one location.
pub fn mount_scratch_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("PhoenixSimulacra").join("mounts")
}

/// Reclaim mount artifacts left behind by a previous **crashed** run.
///
/// A clean unmount removes everything via `Drop`. A crash cannot: what remains
/// under the scratch dir is stale WinFsp junctions (`mount-*` / `serve-*`) and
/// orphaned writable-overlay children (`child-*.avhdx`). The attached virtual
/// disks are already gone — we never request `PERMANENT_LIFETIME`, so Windows
/// auto-detaches them when the owning process dies — so reclaiming is just
/// deleting the leftover files and junctions. Best-effort and quiet; call once
/// at startup, before any new mount.
pub fn cleanup_leaked_mounts(scratch_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(scratch_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && (name.starts_with("mount-") || name.starts_with("serve-")) {
            let _ = std::fs::remove_dir_all(entry.path());
        } else if !is_dir && name.starts_with("child-") && name.ends_with(".avhdx") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
