//! Mounting `.phnx` backups as browsable read-only disks.
//!
//! The default (build-safe) strategy materializes the backup into a fixed-VHD
//! image in a temp file and attaches it read-only via the Windows virtual-disk
//! API, so Windows' own NTFS/FAT drivers present the partitions in Explorer.
//! Materialization decompresses and BLAKE3-verifies every chunk, so a corrupt
//! backup fails at mount time rather than serving silent garbage.
//!
//! The zero-disk-space on-demand path (serving reads straight from the `.phnx`
//! through a WinFsp filesystem) is gated behind the `winfsp` cargo feature; it
//! needs the WinFsp SDK to build and WinFsp installed to run.

pub mod chunkstore;
pub mod gpt;
pub mod image;
pub mod synthetic;
pub mod vhd;

#[cfg(windows)]
pub mod active;
#[cfg(windows)]
pub mod attach;
#[cfg(windows)]
pub mod session;
#[cfg(all(windows, feature = "winfsp"))]
pub mod winfsp_mount;

#[cfg(windows)]
pub use active::ActiveMount;
pub use chunkstore::{plan_layout, ChunkStore, PartitionSpan};
#[cfg(windows)]
pub use session::MountSession;
pub use synthetic::SyntheticVhd;
#[cfg(all(windows, feature = "winfsp"))]
pub use winfsp_mount::WinFspMount;
