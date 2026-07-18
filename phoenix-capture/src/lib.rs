//! Partition capture engines for Phoenix Simulacra.

pub mod backup;
pub mod fat;
pub mod ntfs;
pub mod ntfs_meta;
pub mod raw;
pub mod reader;
pub mod refs;
pub mod sector_convert;

pub use backup::{plan_capture, run_backup};
pub use fat::finalize_fat_partition;
pub use ntfs::finalize_ntfs_partition;
pub use raw::PartitionWriter;
pub use reader::{BlockSource, MemoryBlockSource, PartitionReader};
pub use sector_convert::{apply_sector_conversion, ConvertOutcome};
