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
pub use chunkstore::{plan_layout, ChunkStore, PartitionSpan};
#[cfg(windows)]
pub use letters::MountedVolume;
#[cfg(windows)]
pub use session::MountSession;
pub use synthetic::SyntheticVhd;
#[cfg(all(windows, feature = "winfsp"))]
pub use winfsp_mount::WinFspMount;
