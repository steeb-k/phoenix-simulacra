use thiserror::Error;

pub type Result<T> = std::result::Result<T, PhoenixError>;

#[derive(Error, Debug)]
pub enum PhoenixError {
    #[error("invalid .phnx format: {0}")]
    InvalidFormat(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("compression error: {0}")]
    Compression(String),
    #[error("hash mismatch at chunk {chunk_index} in partition {partition_index}")]
    HashMismatch {
        partition_index: u32,
        chunk_index: u32,
    },
    #[error("manifest error: {0}")]
    Manifest(String),
    #[error("disk error: {0}")]
    Disk(String),
    #[error("partition {partition_index} does not fit target size {target_size} (needs {required} bytes)")]
    PartitionTooSmall {
        partition_index: u32,
        target_size: u64,
        required: u64,
    },
    #[error("restore plan error: {0}")]
    Plan(String),
    #[error("{0}")]
    Other(String),
}
