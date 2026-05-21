//! Carbon Phoenix core library: `.phnx` format, manifest, hashing, disk enumeration.

pub mod container;
pub mod disk;
pub mod error;
pub mod hash;
pub mod manifest;
pub mod progress;

pub use container::{Footer, Header, PartitionIndexEntry, PhnxReader, PhnxWriter, CHUNK_SIZE};
pub use disk::{enumerate_disks, CaptureMode, DiskInfo, FilesystemKind, PartitionInfo};
pub use error::{PhoenixError, Result};
pub use manifest::{BackupManifest, ChunkRecord, PartitionManifest};
pub use progress::{ProgressHandle, ProgressSnapshot};
