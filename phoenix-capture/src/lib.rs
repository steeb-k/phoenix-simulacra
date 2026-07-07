//! Partition capture engines for Carbon Phoenix.

pub mod backup;
pub mod fat;
pub mod ntfs;
pub mod ntfs_meta;
pub mod raw;
pub mod reader;

pub use backup::{plan_capture, run_backup};
pub use fat::finalize_fat_partition;
pub use ntfs::finalize_ntfs_partition;
pub use raw::PartitionWriter;
pub use reader::{BlockSource, MemoryBlockSource, PartitionReader};
