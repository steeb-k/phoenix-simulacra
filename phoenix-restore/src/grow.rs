//! Post-restore NTFS volume extension via `FSCTL_EXTEND_VOLUME`.
//!
//! When a user restores an NTFS partition into a larger slot than the
//! source (the "grow" case — e.g. cloning a 950 GB C: onto a 4 TB
//! drive), the on-disk `$Bitmap` and `$MFT` describe the source's
//! cluster layout. The previous behavior of `patch_ntfs_size`
//! (rewriting `BPB.TotalSectors` to the new larger value) made the
//! volume come up RAW because modern Windows refuses to mount NTFS
//! when `$Bitmap` doesn't cover all clusters advertised by the boot
//! sector.
//!
//! The fix is two-stage: leave the boot sector at its source size
//! during the restore so the volume mounts cleanly, then ask NTFS
//! itself to extend `$Bitmap` / `$MFT` / `total_sectors` after mount
//! via `FSCTL_EXTEND_VOLUME`. This is the same path Disk Management's
//! "Extend Volume" wizard uses, so we're not introducing a novel code
//! path through NTFS — we're letting Windows do its own bookkeeping
//! on a mounted volume.
//!
//! Timing: `IOCTL_DISK_UPDATE_PROPERTIES` is asynchronous —
//! mountmgr's discovery of the new partition layout and its auto-
//! mount of any volumes can take several seconds, particularly on
//! large drives or USB enclosures. The poll loop in
//! [`extend_ntfs_volume`] handles that.

use std::ffi::OsStr;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::time::{Duration, Instant};

use phoenix_core::disk::find_volume_for_partition;
use phoenix_core::error::{PhoenixError, Result};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_EXTEND_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// How long we'll wait for mountmgr to mount the newly-restored
/// partition. Disk Management's auto-mount kicks in within ~1s on a
/// fast SSD but can drag to 10–20s on a USB-attached spinning drive.
/// 30s leaves enough headroom without making the user stare at the
/// "Extending volume" status indefinitely on a failure.
const MOUNT_WAIT: Duration = Duration::from_secs(30);

/// Polling interval while waiting for the volume to come up. 250 ms is
/// well below the visible reaction time but rare enough that we don't
/// burn CPU enumerating drive letters.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Layout of the `FSCTL_EXTEND_VOLUME` input buffer. Mirrors
/// `EXTEND_VOLUME_BUFFER` from `<ntddvol.h>`: a single `LARGE_INTEGER`
/// (8-byte signed) with the new total sector count after extension.
/// `repr(C)` and a single 8-byte field, so layout is identical to the
/// Win32 declaration and no extra padding sneaks in.
#[repr(C)]
struct ExtendVolumeBuffer {
    new_number_of_sectors: i64,
}

/// Ask the NTFS filesystem driver to extend its mounted volume to
/// cover `new_size_bytes` of the partition. The caller already wrote
/// the partition table at the new (larger) size; this call is what
/// makes NTFS actually use the new space — without it, the volume
/// stays at its source size and the trailing free space on the
/// partition is invisible to userspace.
///
/// `disk_index` / `partition_offset_bytes` / `partition_size_bytes`
/// identify the *new* partition slot on disk; we use them to find the
/// mounted volume by exact extent match (the same logic
/// `enumerate_disks` uses to attach drive letters).
///
/// `bytes_per_sector` is the volume's own sector size — read from the
/// source backup's `PartitionIndexEntry::sector_size`. We can't trust
/// the disk's logical-sector size for this (a 4Kn drive hosting a
/// volume formatted on a 512e drive will still expose 512-byte
/// volumes), and re-reading the boot sector here would just duplicate
/// what the capture path already recorded.
///
/// Returns `Ok(())` on a successful extend OR if the volume couldn't
/// be located after `MOUNT_WAIT` — the partition is still restored and
/// usable, just at its source size, and Disk Management's "Extend
/// Volume" gets the user the rest. Hard `Err` is reserved for the
/// case where we *did* find the volume but the FSCTL itself failed,
/// which is worth surfacing because it usually means something
/// fundamental is wrong (bad permissions, in-use file, FS corruption).
pub fn extend_ntfs_volume(
    disk_index: u32,
    partition_offset_bytes: u64,
    partition_size_bytes: u64,
    bytes_per_sector: u64,
) -> Result<()> {
    if bytes_per_sector == 0 {
        return Err(PhoenixError::Other(
            "extend_ntfs_volume: bytes_per_sector cannot be zero".into(),
        ));
    }

    let volume_path = match poll_for_volume(
        disk_index,
        partition_offset_bytes,
        partition_size_bytes,
    ) {
        Some(path) => path,
        None => {
            warn!(
                disk_index,
                partition_offset_bytes,
                partition_size_bytes,
                wait_secs = MOUNT_WAIT.as_secs(),
                "extend_ntfs_volume: volume did not mount within timeout; leaving NTFS at \
                 source size (user can extend manually via Disk Management)"
            );
            return Ok(());
        }
    };

    let new_sectors: i64 = (partition_size_bytes / bytes_per_sector)
        .try_into()
        .map_err(|_| {
            PhoenixError::Other(
                "extend_ntfs_volume: partition size in sectors exceeds i64::MAX".into(),
            )
        })?;

    info!(
        volume = volume_path.as_str(),
        new_sectors,
        partition_size_bytes,
        bytes_per_sector,
        "extending NTFS volume to fill restored partition"
    );

    let handle = open_volume(&volume_path)?;
    let result = run_extend(handle, new_sectors);
    unsafe { CloseHandle(handle) };
    result
}

/// Loop on `find_volume_for_partition` until either we get a match or
/// the deadline expires. Splitting this out keeps the FSCTL call site
/// linear and lets the poll loop be unit-tested in isolation if we
/// ever decide we need that — for now the loop body is pure I/O so
/// there's no test value-add, but the structure makes it cheap to
/// inject a clock later.
fn poll_for_volume(disk_index: u32, offset: u64, length: u64) -> Option<String> {
    let deadline = Instant::now() + MOUNT_WAIT;
    loop {
        if let Some(path) = find_volume_for_partition(disk_index, offset, length) {
            return Some(path);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Open a `\\.\X:` volume device for `FSCTL_EXTEND_VOLUME`. Mirror of
/// `open_disk_for_write` in `partition_table.rs`, with a different
/// error tag for diagnostics: extender errors point users at the
/// "volume is in use" / "not Administrator" failure modes, which are
/// the ones we'd actually expect here as opposed to the disk-handle
/// "another process holds the disk" failures elsewhere.
fn open_volume(path: &str) -> Result<HANDLE> {
    let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x4000_0000 | 0x8000_0000, // GENERIC_WRITE | GENERIC_READ
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(PhoenixError::Disk(format!(
            "cannot open volume {path} for FSCTL_EXTEND_VOLUME (Win32 error {err}); ensure the \
             application is running as Administrator and no other process holds the volume \
             exclusively."
        )));
    }
    Ok(handle)
}

/// Three-step extend dance: lock the volume so NTFS isn't racing user
/// writes, ask it to extend, unlock. The lock isn't strictly required
/// by `FSCTL_EXTEND_VOLUME` — NTFS can extend a hot volume in many
/// cases — but lock+unlock makes the operation atomic from userspace
/// and prevents a passing-by Explorer enumeration from confusing the
/// driver mid-extend. On Windows the lock auto-releases when the
/// handle closes, so the unlock is belt-and-suspenders.
fn run_extend(handle: HANDLE, new_sectors: i64) -> Result<()> {
    // Best-effort lock. A failure here typically means another process
    // (Explorer, an antivirus, a search indexer) opened a file on the
    // volume the moment it auto-mounted, which is common — fall
    // through and let the extend itself decide whether it can proceed.
    let mut returned = 0u32;
    let lock_ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_LOCK_VOLUME,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if lock_ok == 0 {
        let err = unsafe { GetLastError() };
        warn!(
            win32_error = err,
            "FSCTL_LOCK_VOLUME failed before extend; proceeding with an unlocked extend (this \
             is usually fine on a freshly-mounted volume that no other process has opened yet)"
        );
    }

    let buf = ExtendVolumeBuffer {
        new_number_of_sectors: new_sectors,
    };
    let extend_ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_EXTEND_VOLUME,
            &buf as *const _ as *mut _,
            size_of::<ExtendVolumeBuffer>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    let extend_err = if extend_ok == 0 {
        Some(unsafe { GetLastError() })
    } else {
        None
    };

    if lock_ok != 0 {
        let unlock_ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_UNLOCK_VOLUME,
                ptr::null(),
                0,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if unlock_ok == 0 {
            // Not fatal — `CloseHandle` will auto-unlock — but worth
            // noting because a stuck-locked volume can confuse later
            // tooling.
            let err = unsafe { GetLastError() };
            warn!(
                win32_error = err,
                "FSCTL_UNLOCK_VOLUME after extend failed (CloseHandle will auto-unlock)"
            );
        }
    }

    if let Some(err) = extend_err {
        return Err(PhoenixError::Disk(format!(
            "FSCTL_EXTEND_VOLUME failed (Win32 error {err}); the partition was restored at its \
             source size but could not be expanded to fill the new partition slot. Use Disk \
             Management's \"Extend Volume\" to grow it manually."
        )));
    }
    Ok(())
}
