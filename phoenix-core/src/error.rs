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
    #[error(
        "chunk-count mismatch in partition {partition_index}: the stored data stream has \
         {stream_chunks} chunk(s) but the manifest records {manifest_chunks}. The backup is \
         corrupt or truncated."
    )]
    ChunkCountMismatch {
        partition_index: u32,
        stream_chunks: usize,
        manifest_chunks: usize,
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
    #[error("operation cancelled by user")]
    Cancelled,
    /// `FSCTL_LOCK_VOLUME` failed for the named drive after exhausting all
    /// retries. The message embeds the most common holders so the user has
    /// somewhere actionable to look without consulting separate docs.
    #[error(
        "Failed to lock {drive} for exclusive backup access (last Win32 error: {last_error}). \
         Another process has files open on this volume. Common culprits: File Explorer windows, \
         antivirus or backup agents, the search indexer, or any open applications using files on \
         this drive. Close them and retry, or enable Use VSS for an alternative consistency strategy."
    )]
    VolumeLockFailed { drive: String, last_error: u32 },
    #[error("{0}")]
    Other(String),
}
