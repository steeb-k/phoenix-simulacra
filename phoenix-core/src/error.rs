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
    #[error(
        "backup file is truncated or padded: footer records a total length of {expected} bytes \
         but the file on disk is {actual} bytes"
    )]
    Truncated { expected: u64, actual: u64 },
    #[error("backup structure is corrupt: {what}")]
    TableCorrupt { what: String },
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
    /// retries, **and** a VSS shadow could not be taken either. The message names
    /// the most common holders so the user has somewhere actionable to look.
    ///
    /// It deliberately does NOT say "enable Use VSS", which is what it used to say
    /// — there is no such switch any more, and the engine has *already tried* VSS
    /// by the time this error is raised. Telling a user to turn on a thing that is
    /// always on, and that already failed, is worse than saying nothing.
    #[error(
        "Failed to freeze {drive} for a consistent read (last Win32 error: {last_error}). \
         The volume could not be locked exclusively, and a VSS snapshot of it could not be \
         taken either — so reading it now would copy a filesystem that is still changing. \
         Another process has files open on it. Common culprits: File Explorer windows, \
         antivirus or backup agents, the search indexer, or any open application using files \
         on this drive. Close them and retry."
    )]
    VolumeLockFailed { drive: String, last_error: u32 },
    #[error("{0}")]
    Other(String),
}
