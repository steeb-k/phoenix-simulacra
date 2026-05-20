//! Partition capture engines for Carbon Phoenix.

pub mod backup;
pub mod fat;
pub mod ntfs;
pub mod raw;
pub mod reader;

pub use backup::run_backup;
pub use reader::PartitionReader;
