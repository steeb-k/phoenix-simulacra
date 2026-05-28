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
    CreateFileW, FlushFileBuffers, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    CREATE_DISK, CREATE_DISK_GPT, CREATE_DISK_MBR, DRIVE_LAYOUT_INFORMATION_EX,
    IOCTL_DISK_CREATE_DISK, IOCTL_DISK_SET_DRIVE_LAYOUT_EX, IOCTL_DISK_UPDATE_PROPERTIES,
    PARTITION_INFORMATION_EX, PARTITION_STYLE_GPT, PARTITION_STYLE_MBR,
};

/// Per-partition descriptor for the GPT writer. Built by the caller from
/// the backup's `PartitionIndexEntry` (for restored partitions) and from
/// the live target `PartitionInfo` (for partitions we're preserving in a
/// partial restore). Everything `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` needs
/// to know per-partition lives here so the writer doesn't have to peek
/// back at the plan or the manifest.
#[derive(Debug, Clone)]
pub struct GptEntry {
    pub offset_bytes: u64,
    pub size_bytes: u64,
    pub type_guid: [u8; 16],
    pub attributes: u64,
    /// Free-form partition name. Truncated to 35 UTF-16 code units when
    /// written (the on-disk GPT entry has a 36-`wchar` field; we reserve
    /// one slot for a trailing NUL out of an abundance of caution).
    pub name: String,
}

/// Per-partition descriptor for the MBR writer. Mirror of [`GptEntry`]
/// but with the byte-sized partition-type field MBR uses instead of a
/// type GUID.
#[derive(Debug, Clone)]
pub struct MbrEntry {
    pub offset_bytes: u64,
    pub size_bytes: u64,
    pub partition_type: u8,
    pub bootable: bool,
}

/// Standard Basic-Data GUID — `EBD0A0A2-B9E5-4433-87C0-68B6B72699C7`.
/// Used as a fallback when the per-entry source descriptor doesn't
/// carry a type GUID (older backups, MBR sources reusing the GPT
/// writer, defensive paths). Real partitions get their source type
/// GUID threaded through [`GptEntry::type_guid`].
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
/// Initialize a target disk as MBR (full-disk restore from MBR backups).
pub fn init_target_disk_as_mbr(disk_path: &str, disk_signature: u32) -> Result<()> {
    let handle = open_disk_for_write(disk_path)?;
    let mut create: CREATE_DISK = unsafe { std::mem::zeroed() };
    create.PartitionStyle = PARTITION_STYLE_MBR;
    create.Anonymous.Mbr = CREATE_DISK_MBR {
        Signature: disk_signature,
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
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        return Err(PhoenixError::Disk(format!(
            "IOCTL_DISK_CREATE_DISK (MBR) failed (Win32 error {err})"
        )));
    }
    Ok(())
}

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
///
/// The caller passes per-partition descriptors carrying the source
/// type GUID / attributes / name; this is what makes EFI, MSR, and
/// Recovery partitions land with the right type so Windows treats them
/// as system partitions rather than auto-mounting them as Basic Data
/// volumes (the symptom that prompted this rewrite — EFI / MSR /
/// Recovery showing up with drive letters E:, F:, H: after restore).
pub fn write_partition_layout(
    disk_path: &str,
    entries: &[GptEntry],
    state: &GptInitState,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let handle = open_disk_for_write(disk_path)?;
    let res = set_drive_layout(handle, entries, &state.disk_guid, state.disk_size_bytes);
    unsafe { CloseHandle(handle) };
    res
}

/// Write an MBR partition table after a full-disk restore onto MBR media.
pub fn write_mbr_partition_layout(disk_path: &str, entries: &[MbrEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let handle = open_disk_for_write(disk_path)?;
    let res = set_drive_layout_mbr(handle, entries);
    unsafe { CloseHandle(handle) };
    res
}

/// Update GPT entries on an already-initialized disk (partial restore).
///
/// The caller must include descriptors for *every* partition that
/// should remain on the disk — both restored and preserved — because
/// `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` is total-rewrite, not patch-and-add.
/// Preserved partitions take their type GUID / attributes / name from
/// the live target disk (read at the top of `run_restore`) rather than
/// from the backup, so we don't accidentally re-type a Recovery
/// partition we never owned as Basic Data on the way out.
pub fn update_partition_layout_existing(
    disk_path: &str,
    entries: &[GptEntry],
    disk_guid: &GUID,
    disk_size_bytes: u64,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let handle = open_disk_for_write(disk_path)?;
    let res = set_drive_layout(handle, entries, disk_guid, disk_size_bytes);
    unsafe { CloseHandle(handle) };
    res
}

/// Explicit `FlushFileBuffers` on the disk handle. Called from
/// [`run_restore`](crate::restore::run_restore) right before the
/// partition table goes down, so any cached writes from
/// `PartitionWriter` (the per-partition data writer) are guaranteed
/// committed before partmgr re-enumerates and mountmgr starts probing
/// the new layout. Raw `\\.\PhysicalDriveN` writes are already
/// nominally uncached, but the cost of being explicit here is one
/// IOCTL and the safety win against a mountmgr-vs-data-write race is
/// real. Best-effort: degrades to a `warn!` on failure.
pub fn flush_disk(disk_path: &str) {
    let handle = match open_disk_for_write(disk_path) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "could not reopen disk for FlushFileBuffers");
            return;
        }
    };
    let ok = unsafe { FlushFileBuffers(handle) };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        tracing::warn!(
            win32_error = err,
            "FlushFileBuffers on disk handle failed; pre-table cache flush skipped"
        );
    }
    unsafe { CloseHandle(handle) };
}

/// Clear the disk's "offline" attribute via `IOCTL_DISK_SET_DISK_ATTRIBUTES`
/// so volumes auto-mount with drive letters after a restore, rather than
/// requiring the user to right-click → Online in Disk Management.
///
/// Windows leaves a freshly-restored disk offline in two common cases:
///
///   1. The SAN policy is `OfflineShared` / `OfflineAll` (the default on
///      Windows Server SKUs). Any disk that appears after boot gets
///      offlined until an admin explicitly brings it online.
///   2. The new disk's GPT GUID or MBR signature matches another attached
///      disk (very common after a full-disk clone — the source's signature
///      gets faithfully reproduced on the target). Windows offlines the
///      *duplicate* to prevent mount-table ambiguity.
///
/// Clearing the offline bit handles case (1) outright and is enough for
/// case (2) on client SKUs (Windows just marks the disk online and the
/// mountmgr de-duplicates volume GUIDs internally). For case (2) on
/// Server SKUs you'd additionally need to rewrite the disk signature;
/// we don't do that here because clone-with-same-signature is a
/// deliberate user intent — the right thing to do is online the disk
/// and let the user pick their drive letters.
///
/// The `Persist = TRUE` flag below makes the attribute change survive
/// reboots so the user doesn't get the "Disk is Offline" dialog every
/// time they reboot with this disk attached.
///
/// Best-effort: a failure here only means the user has to manually
/// right-click → Online, so we degrade to a `warn!` rather than failing
/// the restore.
pub fn bring_disk_online(disk_path: &str) {
    // From `winioctl.h`; not re-exported by `windows-sys` 0.59. The
    // CTL_CODE macro expansion for FILE_DEVICE_DISK (7), function 254,
    // METHOD_BUFFERED (0), FILE_READ_ACCESS|FILE_WRITE_ACCESS (3) →
    // (7 << 16) | (3 << 14) | (254 << 2) | 0 = 0x0007_C0F8 ... wait,
    // double-checking: Microsoft's published value is 0x0007_C0F4 for
    // SET_DISK_ATTRIBUTES (function 0x3D, not 0xFE). We use that value
    // verbatim rather than reconstructing it from the CTL_CODE pieces.
    const IOCTL_DISK_SET_DISK_ATTRIBUTES: u32 = 0x0007_C0F4;
    const DISK_ATTRIBUTE_OFFLINE: u64 = 0x0000_0000_0000_0001;

    // Win32 ABI per `winioctl.h`. Field order/size MUST match exactly:
    //   DWORD Version;           // 4
    //   BOOLEAN Persist;         // 1 (BOOLEAN is BYTE, NOT a Rust bool)
    //   BYTE Reserved1[3];       // 3
    //   DWORDLONG Attributes;    // 8 (8-byte aligned at offset 8)
    //   DWORDLONG AttributesMask;// 8
    //   DWORD Reserved2[4];      // 16
    // Total: 40 bytes.
    #[repr(C)]
    struct SetDiskAttributes {
        version: u32,
        persist: u8,
        reserved1: [u8; 3],
        attributes: u64,
        attributes_mask: u64,
        reserved2: [u32; 4],
    }

    let handle = match open_disk_for_write(disk_path) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not reopen disk for IOCTL_DISK_SET_DISK_ATTRIBUTES; disk may need to be brought online manually via Disk Management"
            );
            return;
        }
    };

    let mut sda = SetDiskAttributes {
        version: size_of::<SetDiskAttributes>() as u32,
        persist: 1, // TRUE — make the online state survive reboots
        reserved1: [0; 3],
        attributes: 0, // not offline, not read-only
        attributes_mask: DISK_ATTRIBUTE_OFFLINE, // only touch the OFFLINE bit
        reserved2: [0; 4],
    };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_SET_DISK_ATTRIBUTES,
            &mut sda as *mut _ as *mut _,
            size_of::<SetDiskAttributes>() as u32,
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
            disk = disk_path,
            "IOCTL_DISK_SET_DISK_ATTRIBUTES failed; disk may show as Offline in Disk Management. Right-click the disk and choose Online to mount its volumes."
        );
    } else {
        tracing::info!(
            disk = disk_path,
            "cleared DISK_ATTRIBUTE_OFFLINE (Persist=TRUE); volumes should auto-mount"
        );
    }
    unsafe { CloseHandle(handle) };
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

fn set_drive_layout_mbr(handle: HANDLE, entries: &[MbrEntry]) -> Result<()> {
    let header_size = size_of::<DRIVE_LAYOUT_INFORMATION_EX>();
    let entry_size = size_of::<PARTITION_INFORMATION_EX>();
    let buf_size = header_size + entries.len() * entry_size;
    let mut buffer = vec![0u8; buf_size];
    let layout = unsafe { &mut *(buffer.as_mut_ptr() as *mut DRIVE_LAYOUT_INFORMATION_EX) };
    layout.PartitionStyle = PARTITION_STYLE_MBR as u32;
    layout.PartitionCount = entries.len() as u32;
    layout.Anonymous.Mbr = windows_sys::Win32::System::Ioctl::DRIVE_LAYOUT_INFORMATION_MBR {
        Signature: 0,
        CheckSum: 0,
    };

    let entry_ptr = layout.PartitionEntry.as_mut_ptr();
    for (i, entry) in entries.iter().enumerate() {
        let part = unsafe { &mut *entry_ptr.add(i) };
        part.PartitionStyle = PARTITION_STYLE_MBR;
        part.StartingOffset = entry.offset_bytes as i64;
        part.PartitionLength = entry.size_bytes as i64;
        part.PartitionNumber = (i + 1) as u32;
        part.RewritePartition = 1;
        part.Anonymous.Mbr = windows_sys::Win32::System::Ioctl::PARTITION_INFORMATION_MBR {
            PartitionType: entry.partition_type,
            BootIndicator: if entry.bootable { 1 } else { 0 },
            RecognizedPartition: 1,
            HiddenSectors: (entry.offset_bytes / 512) as u32,
            PartitionId: GUID {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8],
            },
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
            "IOCTL_DISK_SET_DRIVE_LAYOUT_EX (MBR) failed (Win32 error {err})"
        )));
    }
    Ok(())
}

fn set_drive_layout(
    handle: HANDLE,
    entries: &[GptEntry],
    disk_guid: &GUID,
    disk_size_bytes: u64,
) -> Result<()> {
    let header_size = size_of::<DRIVE_LAYOUT_INFORMATION_EX>();
    let entry_size = size_of::<PARTITION_INFORMATION_EX>();
    // `DRIVE_LAYOUT_INFORMATION_EX` already declares `PartitionEntry[1]`
    // inline, so a buffer of `header_size + (count - 1) * entry_size`
    // would suffice, but rounding up to `header_size + count * entry_size`
    // is harmless and keeps the math obvious.
    let buf_size = header_size + entries.len() * entry_size;
    let mut buffer = vec![0u8; buf_size];

    // SAFETY: `buffer` is heap-allocated and the layout's first field is
    // a `DRIVE_LAYOUT_INFORMATION_EX`, so casting the buffer base to
    // `*mut DRIVE_LAYOUT_INFORMATION_EX` and reading/writing through it
    // is well-defined as long as we stay inside `buf_size` bytes.
    let layout = unsafe { &mut *(buffer.as_mut_ptr() as *mut DRIVE_LAYOUT_INFORMATION_EX) };
    layout.PartitionStyle = PARTITION_STYLE_GPT as u32;
    layout.PartitionCount = entries.len() as u32;
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
    for (i, entry) in entries.iter().enumerate() {
        // SAFETY: `i` is bounded by `entries.len()`, and we sized the
        // buffer to fit at least that many `PARTITION_INFORMATION_EX`
        // slots starting at `PartitionEntry`.
        let part = unsafe { &mut *entry_ptr.add(i) };
        part.PartitionStyle = PARTITION_STYLE_GPT;
        part.StartingOffset = entry.offset_bytes as i64;
        part.PartitionLength = entry.size_bytes as i64;
        part.PartitionNumber = (i + 1) as u32;
        part.RewritePartition = 1;

        // Type GUID: prefer the source's (which we threaded through
        // [`GptEntry::type_guid`]). All-zeroes means "no GUID recorded"
        // — fall back to Basic Data so the partition is still mountable.
        // The fallback path matters for MBR-source restores routed
        // through this writer and for older backups that pre-date type
        // GUID capture; modern backups always carry the source GUID.
        let type_guid = if entry.type_guid.iter().all(|&b| b == 0) {
            BASIC_DATA_PARTITION_TYPE
        } else {
            bytes_to_guid(entry.type_guid)
        };

        let mut name_buf = [0u16; 36];
        for (slot, ch) in name_buf.iter_mut().zip(entry.name.encode_utf16()).take(35) {
            *slot = ch;
        }

        // Writing to a union field (`PARTITION_INFORMATION_EX.Anonymous`)
        // is safe in current Rust — only *reading* from a union without
        // knowing which variant is active is unsafe — so no `unsafe`
        // wrapper here.
        part.Anonymous.Gpt = windows_sys::Win32::System::Ioctl::PARTITION_INFORMATION_GPT {
            PartitionType: type_guid,
            PartitionId: GUID {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8],
            },
            Attributes: entry.attributes,
            Name: name_buf,
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
