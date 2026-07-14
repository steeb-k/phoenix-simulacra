use std::path::Path;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, PhoenixError>;

/// Win32 error codes that all mean the same thing to a backup: **the volume we
/// were writing to is no longer there.** A removable destination is the normal
/// case for this tool, so this is an expected failure mode, not an exotic one.
///
/// Observed live (2026-07-14, ARM64): a USB enclosure on a flaky cable dropped
/// off the bus mid-backup. The disk driver logged one retried IO, exFAT then
/// logged six `{Delayed Write Failed}` events against the .phnx, and our next
/// write came back `ERROR_NO_SUCH_DEVICE`. The user saw only
/// "IO error: A device which does not exist was specified. (os error 433)".
const DEVICE_LOST_CODES: &[i32] = &[
    21,   // ERROR_NOT_READY            — device not ready
    31,   // ERROR_GEN_FAILURE          — "a device attached to the system is not functioning"
    55,   // ERROR_DEV_NOT_EXIST        — the specified network/device resource is gone
    433,  // ERROR_NO_SUCH_DEVICE       — what the USB drop actually produced
    1006, // ERROR_FILE_INVALID         — the volume was dismounted under the open handle
    1117, // ERROR_IO_DEVICE            — the IO operation failed at the device level
    1167, // ERROR_DEVICE_NOT_CONNECTED
];

/// True when `code` means the destination device vanished rather than the write
/// merely being rejected (out of space, permissions, read-only, …).
fn is_device_lost(code: i32) -> bool {
    DEVICE_LOST_CODES.contains(&code)
}

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
    /// The destination the backup was being written to stopped responding
    /// part-way through — the device was unplugged, a cable or port went marginal
    /// under sustained write load, or the drive dropped off the bus.
    ///
    /// This gets its own variant because for a backup tool it is an *expected*
    /// failure (destinations are usually removable), and because the raw OS text
    /// is actively unhelpful: `ERROR_NO_SUCH_DEVICE` renders as "A device which
    /// does not exist was specified", which reads like a bug in the caller rather
    /// than a drive that fell off the bus.
    ///
    /// It warns about the destination filesystem too. Windows buffers writes, so
    /// the loss surfaces as a *delayed* write failure: the bytes we already
    /// "wrote" successfully were sitting in cache and are discarded, which can
    /// leave the destination filesystem itself damaged (observed: an exFAT volume
    /// left needing a full repair).
    #[error(
        "The backup destination {path} stopped responding while the backup was being written \
         (Win32 error {last_error}). The device was disconnected, or a cable, port or enclosure \
         went marginal under sustained write load — this is not a problem with the disk being \
         backed up. The partial backup file is incomplete and cannot be restored from; delete it. \
         Windows caches writes, so the destination's own filesystem may also have been left \
         damaged — check it (`chkdsk`) before reusing the drive. Then re-run the backup, ideally \
         on a different cable or port."
    )]
    DestinationLost { path: String, last_error: i32 },
    #[error("{0}")]
    Other(String),
}

impl PhoenixError {
    /// Reclassify an error raised while writing the backup file at `path`.
    ///
    /// An [`PhoenixError::Io`] whose OS code means "the device is gone" becomes a
    /// [`PhoenixError::DestinationLost`] naming the destination; everything else
    /// passes through untouched. Applied only on the *output* path, so a source
    /// disk that disappears is never misreported as a destination failure.
    pub fn destination_context(self, path: &Path) -> Self {
        let Self::Io(ref e) = self else {
            return self;
        };
        match e.raw_os_error() {
            Some(code) if is_device_lost(code) => Self::DestinationLost {
                path: path.display().to_string(),
                last_error: code,
            },
            _ => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn io_err(code: i32) -> PhoenixError {
        PhoenixError::Io(io::Error::from_raw_os_error(code))
    }

    #[test]
    fn device_lost_write_names_the_destination() {
        // 433 = ERROR_NO_SUCH_DEVICE, exactly what the dropped USB enclosure gave us.
        let err = io_err(433).destination_context(Path::new(r"D:\armUFSBU.phnx"));
        let PhoenixError::DestinationLost { path, last_error } = err else {
            panic!("expected DestinationLost, got: {err:?}");
        };
        assert_eq!(last_error, 433);
        assert_eq!(path, r"D:\armUFSBU.phnx");
        // The message has to be actionable, not just a Win32 string.
        let msg = PhoenixError::DestinationLost {
            path,
            last_error: 433,
        }
        .to_string();
        assert!(msg.contains(r"D:\armUFSBU.phnx"));
        assert!(msg.contains("chkdsk"));
    }

    #[test]
    fn ordinary_write_failures_are_left_alone() {
        // A full destination is a real, different problem with its own remedy —
        // reclassifying it as "the device vanished" would send the user hunting
        // for a cable fault that isn't there.
        const ERROR_DISK_FULL: i32 = 112;
        const ERROR_ACCESS_DENIED: i32 = 5;
        for code in [ERROR_DISK_FULL, ERROR_ACCESS_DENIED] {
            let err = io_err(code).destination_context(Path::new(r"D:\b.phnx"));
            assert!(
                matches!(err, PhoenixError::Io(_)),
                "os error {code} should pass through as Io, got: {err:?}"
            );
        }
    }

    #[test]
    fn non_io_errors_pass_through() {
        let err = PhoenixError::Cancelled.destination_context(Path::new(r"D:\b.phnx"));
        assert!(matches!(err, PhoenixError::Cancelled));
    }
}
