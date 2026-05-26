//! Apply GPT partition layout to a target disk before restore.
//!
//! Two IOCTLs run in sequence here, in the same order Disk Management's
//! "Initialize Disk" wizard uses:
//!
//!   1. `IOCTL_DISK_CREATE_DISK` plants a fresh GPT header on the target,
//!      populating the protective MBR and the primary/backup GPT
//!      headers. Without this, `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` either
//!      fails outright with `ERROR_INVALID_PARAMETER` or silently leaves
//!      the disk uninitialized — which was the symptom the user hit:
//!      Disk Management showed the target as unallocated even though
//!      the data writes appeared to succeed.
//!   2. `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` writes the partition entries
//!      (boundaries + types) onto that GPT, with the `Anonymous.Gpt`
//!      header union populated (DiskId, StartingUsableOffset,
//!      UsableLength, MaxPartitionCount). The previous version of this
//!      function left those zero, which Windows rejects.
//!   3. `IOCTL_DISK_UPDATE_PROPERTIES` tells the disk class driver to
//!      re-enumerate so the partitions show up immediately in
//!      `\Device\Harddisk*\Partition*` and Disk Management without a
//!      reboot or device-manager rescan.

use std::mem::size_of;
use std::ptr;

use phoenix_core::error::{PhoenixError, Result};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    CREATE_DISK, CREATE_DISK_GPT, DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_CREATE_DISK,
    IOCTL_DISK_SET_DRIVE_LAYOUT_EX, IOCTL_DISK_UPDATE_PROPERTIES, PARTITION_INFORMATION_EX,
    PARTITION_STYLE_GPT,
};

use crate::plan::RestorePlan;

/// Standard ESP-data GUID — `EBD0A0A2-B9E5-4433-87C0-68B6B72699C7`. We
/// stamp every restored partition with this type for now; per-partition
/// type GUIDs from the source manifest are a follow-up.
const BASIC_DATA_PARTITION_TYPE: GUID = GUID {
    data1: 0xebd0a0a2,
    data2: 0xb9e5,
    data3: 0x4433,
    data4: [0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7],
};

/// Size of the GPT-required region at the start of a disk:
///   * LBA  0       — protective MBR
///   * LBA  1       — primary GPT header
///   * LBA  2..=33  — primary GPT partition entry array (128 entries)
const GPT_LEADING_RESERVED_SECTORS: u64 = 34;
/// Size of the matching backup region at the END of the disk:
///   * last LBA              — backup GPT header
///   * last 33..1 LBAs       — backup GPT partition entry array
const GPT_TRAILING_RESERVED_SECTORS: u64 = 33;
/// Microsoft tooling always provisions 128 entry slots even when only a
/// handful are populated; matching that keeps the disk indistinguishable
/// from one initialized via Disk Management or `diskpart`.
const GPT_MAX_PARTITION_ENTRIES: u32 = 128;

/// Token returned by [`init_target_disk_as_gpt`] and consumed by
/// [`write_partition_layout`]. Carries the GPT GUID we just stamped on
/// the disk so the second IOCTL passes the *same* identity to
/// `DRIVE_LAYOUT_INFORMATION_GPT::DiskId` — otherwise the partition
/// table would name a different disk than the GPT header above it.
pub struct GptInitState {
    disk_guid: GUID,
    disk_size_bytes: u64,
}

/// Phase 1 of the GPT restore dance: stamp a fresh, **partition-less**
/// GPT skeleton on the target disk. Returns a token that
/// [`write_partition_layout`] consumes during phase 3. `source_disk_guid`
/// preserves the GUID from the backup manifest when present so the
/// restored disk keeps its original identity; falls back to a random
/// GUID otherwise.
///
/// Why no partition entries here: `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` is a
/// partmgr-visible event regardless of whether the caller follows it
/// with `IOCTL_DISK_UPDATE_PROPERTIES` — partmgr / volmgr will discover
/// any non-zero partition entries asynchronously and trigger mountmgr to
/// attach an empty volume on each one, which then claims exclusive
/// access to its disk region. Once that happens, every subsequent raw
/// `WriteFile` through the disk handle to the volume's region fails
/// with `ERROR_ACCESS_DENIED` (Win32 5). The fix is to plant the
/// partition entries *after* the data writes are done, which is what
/// [`write_partition_layout`] takes care of.
pub fn init_target_disk_as_gpt(
    disk_path: &str,
    disk_size_bytes: u64,
    source_disk_guid: Option<[u8; 16]>,
) -> Result<GptInitState> {
    let disk_guid_bytes = source_disk_guid.unwrap_or_else(random_disk_guid);
    let disk_guid = bytes_to_guid(disk_guid_bytes);

    let handle = open_disk_for_write(disk_path)?;
    let res = create_gpt_disk(handle, &disk_guid);
    unsafe { CloseHandle(handle) };
    res?;

    Ok(GptInitState {
        disk_guid,
        disk_size_bytes,
    })
}

/// Phase 3 of the GPT restore dance: lay the actual partition entries
/// onto the GPT skeleton planted by [`init_target_disk_as_gpt`]. Must
/// be called **after** every chunk of partition data has been written,
/// otherwise mountmgr will lazy-mount empty volumes on the new
/// partitions and our raw disk writes start hitting `ERROR_ACCESS_DENIED`
/// in the middle of the data stream (the symptom users saw was a
/// `WriteFile of 4194304 bytes at disk offset N failed (Win32 error 5)`
/// some hundreds of MiB into a previously-fine restore).
pub fn write_partition_layout(
    disk_path: &str,
    plan: &RestorePlan,
    state: &GptInitState,
) -> Result<()> {
    let restoring: Vec<_> = plan.entries.iter().filter(|e| e.restore).collect();
    if restoring.is_empty() {
        return Ok(());
    }
    let handle = open_disk_for_write(disk_path)?;
    let res = set_drive_layout(handle, &restoring, &state.disk_guid, state.disk_size_bytes);
    unsafe { CloseHandle(handle) };
    res
}

/// Tell the disk class driver to re-read its partition table so Disk
/// Management, Explorer, and mountmgr notice the layout we just wrote.
/// Call this only **after** all partition data writes have completed
/// (calling it earlier risks an auto-mount race with our raw writes).
/// Best-effort: a failure here only means the user has to manually
/// rescan in Disk Management, so we degrade to a `warn!` rather than
/// failing the whole restore.
pub fn notify_disk_updated(disk_path: &str) {
    let handle = match open_disk_for_write(disk_path) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not reopen disk for IOCTL_DISK_UPDATE_PROPERTIES"
            );
            return;
        }
    };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_UPDATE_PROPERTIES,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        tracing::warn!(
            win32_error = err,
            "IOCTL_DISK_UPDATE_PROPERTIES failed; partitions may not show up in Disk Management until the disk is rescanned manually"
        );
    }
    unsafe { CloseHandle(handle) };
}

fn create_gpt_disk(handle: HANDLE, disk_guid: &GUID) -> Result<()> {
    let mut create: CREATE_DISK = unsafe { std::mem::zeroed() };
    create.PartitionStyle = PARTITION_STYLE_GPT;
    create.Anonymous.Gpt = CREATE_DISK_GPT {
        DiskId: *disk_guid,
        MaxPartitionCount: GPT_MAX_PARTITION_ENTRIES,
    };

    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_CREATE_DISK,
            &create as *const _ as *mut _,
            size_of::<CREATE_DISK>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        return Err(PhoenixError::Disk(format!(
            "IOCTL_DISK_CREATE_DISK failed (Win32 error {err}); the target disk could not be initialized as GPT. Common causes: another process holds the disk open, the disk is read-only, or the application is not running as Administrator."
        )));
    }
    Ok(())
}

fn set_drive_layout(
    handle: HANDLE,
    restoring: &[&crate::plan::RestorePlanEntry],
    disk_guid: &GUID,
    disk_size_bytes: u64,
) -> Result<()> {
    let header_size = size_of::<DRIVE_LAYOUT_INFORMATION_EX>();
    let entry_size = size_of::<PARTITION_INFORMATION_EX>();
    // `DRIVE_LAYOUT_INFORMATION_EX` already declares `PartitionEntry[1]`
    // inline, so a buffer of `header_size + (count - 1) * entry_size`
    // would suffice, but rounding up to `header_size + count * entry_size`
    // is harmless and keeps the math obvious.
    let buf_size = header_size + restoring.len() * entry_size;
    let mut buffer = vec![0u8; buf_size];

    // SAFETY: `buffer` is heap-allocated and the layout's first field is
    // a `DRIVE_LAYOUT_INFORMATION_EX`, so casting the buffer base to
    // `*mut DRIVE_LAYOUT_INFORMATION_EX` and reading/writing through it
    // is well-defined as long as we stay inside `buf_size` bytes.
    let layout = unsafe { &mut *(buffer.as_mut_ptr() as *mut DRIVE_LAYOUT_INFORMATION_EX) };
    layout.PartitionStyle = PARTITION_STYLE_GPT as u32;
    layout.PartitionCount = restoring.len() as u32;
    let sector_size: u64 = 512;
    layout.Anonymous.Gpt = windows_sys::Win32::System::Ioctl::DRIVE_LAYOUT_INFORMATION_GPT {
        DiskId: *disk_guid,
        StartingUsableOffset: (GPT_LEADING_RESERVED_SECTORS * sector_size) as i64,
        UsableLength: disk_size_bytes
            .saturating_sub((GPT_LEADING_RESERVED_SECTORS + GPT_TRAILING_RESERVED_SECTORS) * sector_size)
            as i64,
        MaxPartitionCount: GPT_MAX_PARTITION_ENTRIES,
    };

    let entry_ptr = layout.PartitionEntry.as_mut_ptr();
    for (i, entry) in restoring.iter().enumerate() {
        // SAFETY: `i` is bounded by `restoring.len()`, and we sized the
        // buffer to fit at least that many `PARTITION_INFORMATION_EX`
        // slots starting at `PartitionEntry`.
        let part = unsafe { &mut *entry_ptr.add(i) };
        part.PartitionStyle = PARTITION_STYLE_GPT;
        part.StartingOffset = entry.target_offset_bytes as i64;
        part.PartitionLength = entry.target_size_bytes as i64;
        part.PartitionNumber = (i + 1) as u32;
        part.RewritePartition = 1;
        // Writing to a union field (`PARTITION_INFORMATION_EX.Anonymous`)
        // is safe in current Rust — only *reading* from a union without
        // knowing which variant is active is unsafe — so no `unsafe`
        // wrapper here.
        part.Anonymous.Gpt = windows_sys::Win32::System::Ioctl::PARTITION_INFORMATION_GPT {
            PartitionType: BASIC_DATA_PARTITION_TYPE,
            PartitionId: GUID {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8],
            },
            Attributes: 0,
            Name: [0; 36],
        };
    }

    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_SET_DRIVE_LAYOUT_EX,
            buffer.as_ptr() as *mut _,
            buf_size as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        return Err(PhoenixError::Disk(format!(
            "IOCTL_DISK_SET_DRIVE_LAYOUT_EX failed (Win32 error {err}); the GPT was created but partition entries could not be written. Common causes: the offsets/sizes in the restore plan exceed the target disk's size, or another process opened a volume on the disk between CREATE_DISK and SET_DRIVE_LAYOUT_EX."
        )));
    }
    Ok(())
}

/// Generate a fresh GUID without pulling the `uuid` crate's `v4`
/// machinery in here — we already need uuid in `restore.rs` for parsing,
/// but the random-GUID helper there just feeds back into our
/// `bytes_to_guid` conversion. Using `uuid::Uuid::new_v4` keeps the
/// randomness logic in one place.
fn random_disk_guid() -> [u8; 16] {
    let u = uuid::Uuid::new_v4();
    // Match the on-disk byte order our capture path stores: little-endian
    // for the first three GUID fields, raw for the last 8 bytes.
    let be = u.as_bytes();
    let mut out = [0u8; 16];
    out[0] = be[3];
    out[1] = be[2];
    out[2] = be[1];
    out[3] = be[0];
    out[4] = be[5];
    out[5] = be[4];
    out[6] = be[7];
    out[7] = be[6];
    out[8..16].copy_from_slice(&be[8..16]);
    out
}

/// Convert the on-disk little-endian GUID byte order our capture path
/// records back into the `windows_sys::core::GUID` struct that the
/// IOCTL APIs consume. Mirror of the parser in `restore.rs`.
fn bytes_to_guid(bytes: [u8; 16]) -> GUID {
    GUID {
        data1: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        data2: u16::from_le_bytes([bytes[4], bytes[5]]),
        data3: u16::from_le_bytes([bytes[6], bytes[7]]),
        data4: [
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ],
    }
}

fn open_disk_for_write(path: &str) -> Result<HANDLE> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
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
            "cannot open {path} for write (Win32 error {err}); ensure the application is running as Administrator and the device exists."
        )));
    }
    Ok(handle)
}
