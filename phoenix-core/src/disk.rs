//! Windows physical disk and partition enumeration.

use std::ffi::OsStr;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use tracing::{debug, warn};
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW,
    GetVolumeInformationW, ReadFile, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows_sys::Win32::Storage::IscsiDisc::{
    IOCTL_SCSI_PASS_THROUGH_DIRECT, SCSI_IOCTL_DATA_IN, SCSI_PASS_THROUGH_DIRECT,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
    IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_QUERY_PROPERTY, PARTITION_INFORMATION_EX,
    PARTITION_STYLE_GPT, STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::error::{PhoenixError, Result};

/// Ceiling on how long a single media-identification SCSI command may take.
/// Generous for a query that returns immediately from any healthy device, and
/// short enough that a wedged USB bridge can't stall disk enumeration.
const SCSI_TIMEOUT_SECS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FilesystemKind {
    Unknown = 0,
    Ntfs = 1,
    Fat = 2,
    Exfat = 3,
    Efi = 4,
    Msr = 5,
    /// BitLocker-encrypted volume whose decrypted view we can't read (no key /
    /// volume in suspended state). The boot sector starts with `-FVE-FS-`.
    /// Treated as raw for backup purposes.
    Bitlocker = 6,
    /// ReFS (Resilient File System). Used-block capture works through
    /// `FSCTL_GET_VOLUME_BITMAP` exactly like NTFS; resize is not supported
    /// (ReFS has no offline shrink and its allocator is undocumented).
    Refs = 7,
}

impl FilesystemKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Ntfs,
            2 => Self::Fat,
            3 => Self::Exfat,
            4 => Self::Efi,
            5 => Self::Msr,
            6 => Self::Bitlocker,
            7 => Self::Refs,
            _ => Self::Unknown,
        }
    }
}

/// Per-partition BitLocker status, derived by comparing the *on-disk*
/// (physical) view of the boot sector with the *volume-device* view.
///
/// The two views differ exactly when BitLocker is unlocked: the physical
/// sectors hold FVE ciphertext (`-FVE-FS-`), but the volume device — the
/// handle capture actually reads through — presents the decrypted
/// filesystem. That makes this classification impossible to get wrong
/// relative to the bytes a backup would stream:
///
/// - [`BitlockerState::Unlocked`]: disk view says BitLocker, volume view
///   reads a real filesystem → used-block capture streams **plaintext**;
///   the resulting image is a normal, unencrypted, restorable backup.
/// - [`BitlockerState::Locked`]: every readable view is ciphertext → the
///   partition must be captured raw, and a restore reproduces the locked
///   volume (the original BitLocker key is still required to unlock it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitlockerState {
    /// Not a BitLocker volume.
    #[default]
    None,
    /// BitLocker volume whose decrypted view is readable; captured as a
    /// normal used-block backup containing plaintext.
    Unlocked,
    /// BitLocker volume we can only read as ciphertext; captured raw.
    Locked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CaptureMode {
    Raw = 0,
    UsedBlocks = 1,
}

impl CaptureMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::UsedBlocks,
            _ => Self::Raw,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PartitionUsage {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl PartitionUsage {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }
}

#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub index: u32,
    pub name: String,
    pub type_guid: [u8; 16],
    /// GPT `Attributes` field (bit-packed flags: bit 0 =
    /// PlatformRequired, bit 60 = ReadOnly, bit 62 = Hidden, bit 63 =
    /// NoDriveLetter; the rest are partition-type specific).
    /// Zero for MBR partitions and for older code paths that haven't
    /// been migrated to read this. The restore path uses this when
    /// rewriting partition tables during partial restores so it
    /// doesn't accidentally strip "no auto-mount" / "hidden" flags
    /// off a Recovery partition we're meant to leave alone.
    pub gpt_attributes: u64,
    /// GPT partition **unique** GUID (`PARTITION_INFORMATION_GPT.PartitionId`)
    /// — the identity the BCD uses to reference the boot/OS partitions on a
    /// GPT disk. Captured into the manifest and written back on restore so a
    /// cloned system disk keeps its BCD device references intact (a
    /// regenerated PartitionId is the classic "cloned disk drops into
    /// Startup Repair" failure). All-zero for MBR partitions.
    pub unique_guid: [u8; 16],
    /// MBR partition **type byte** (0x07 NTFS/exFAT, 0x0C FAT32-LBA, 0x27
    /// recovery…) — the MBR analogue of `type_guid`, and the only record of
    /// what a partition claims to be on a BIOS disk. Zero for GPT sources.
    ///
    /// Captured because it cannot be reliably recovered later: inferring it
    /// from the filesystem loses the distinction between, say, a plain NTFS
    /// data partition and a hidden recovery one that happens to hold NTFS.
    pub mbr_type: u8,
    /// The MBR active/boot flag. On a BIOS disk the MBR boot code chains into
    /// whichever primary partition carries this, so a synthesized or restored
    /// disk that loses it does not boot. False for GPT sources.
    pub mbr_bootable: bool,
    pub offset_bytes: u64,
    pub size_bytes: u64,
    pub fs_kind: FilesystemKind,
    pub capture_mode: CaptureMode,
    /// BitLocker lock state (see [`BitlockerState`]). Drives whether a
    /// BitLocker partition gets a normal used-block (plaintext) capture or
    /// a raw ciphertext capture, and is recorded in the backup manifest.
    pub bitlocker: BitlockerState,
    pub volume_path: Option<String>,
    pub drive_letter: Option<char>,
    pub volume_label: Option<String>,
    pub usage: Option<PartitionUsage>,
    /// Logical sector size of the containing disk (512 or 4096). Copied from
    /// the parent [`DiskInfo`] so capture/restore code that only holds a
    /// `PartitionInfo` can align its I/O without re-querying the device.
    pub sector_size: u32,
}

#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub index: u32,
    pub path: String,
    pub size_bytes: u64,
    pub is_gpt: bool,
    pub disk_guid: Option<[u8; 16]>,
    pub disk_signature: u64,
    pub sector_size: u32,
    /// Best-effort human label for the physical device. Populated from
    /// `IOCTL_STORAGE_QUERY_PROPERTY` (vendor + product strings) when the
    /// device exposes them; falls back to a `BusType`-derived hint
    /// ("USB", "NVMe", "Virtual", ...) for devices that return blanks
    /// (USB enclosures, RAID volumes, virtual disks). `None` only when
    /// the IOCTL itself fails — the GUI then omits the trailing
    /// " - <model>" segment in the disk label.
    pub model: Option<String>,
    /// What the device *is* rather than what it's called: bus plus media class
    /// ("NVMe SSD", "SATA HDD", "USB Flash Drive"). Derived from the descriptor's
    /// `BusType`/`RemovableMedia` plus the seek-penalty IOCTL — the only
    /// generally-available signal that separates a spinning disk from an SSD.
    /// `None` when the device descriptor IOCTL itself fails, in which case the
    /// GUI card simply omits the row.
    pub drive_type: Option<String>,
    pub partitions: Vec<PartitionInfo>,
}

/// `GUID_DEVINTERFACE_DISK` ? the device-interface class every disk Windows is
/// willing to call a disk registers itself under. Spelled out rather than
/// imported so the identity of the thing we enumerate is visible right here.
const GUID_DEVINTERFACE_DISK: GUID = GUID {
    data1: 0x53f5_6307,
    data2: 0xb6bf,
    data3: 0x11d0,
    data4: [0x94, 0xf2, 0x00, 0xa0, 0xc9, 0x1e, 0xfb, 0x8b],
};

/// Every disk Windows recognizes, as `\\.\PhysicalDriveN` indices, ascending.
///
/// **Why this is not a `0..64` scan of the `\\.\PhysicalDriveN` namespace.**
/// That namespace is a *legacy alias table*, not the list of disks: the
/// partition manager hands a `PhysicalDriveN` name to block devices that
/// Windows itself does not surface as disks, and opening one succeeds. On a
/// Snapdragon/UFS machine that is not hypothetical ? the UFS chip exposes its
/// firmware LUNs (boot, persist, modem, ?) as `PhysicalDrive1..5`, all reporting
/// the same vendor/product as the real disk and differing only by SCSI LUN.
/// They carry real GPTs full of real (tiny) firmware partitions, so nothing
/// downstream can tell them apart from a disk by inspection. A brute-force scan
/// therefore offered them as backup sources and, far worse, as clone/restore
/// **targets** ? and the boot/system-disk gate does not catch them, because a
/// firmware LUN is neither. Writing one bricks the machine.
///
/// Enumerating the `GUID_DEVINTERFACE_DISK` class asks Windows the question we
/// actually mean ? *what are the disks?* ? and gets the same answer Disk
/// Management, `Get-Disk`, and `Win32_DiskDrive` give. The firmware LUNs never
/// register the interface, so they are excluded by construction rather than by a
/// heuristic we would have to keep tuning. Attached VHDX disks and USB sticks do
/// register it, so the T2/T3 suites are unaffected.
///
/// The interface class is unordered; the returned indices are sorted and
/// deduplicated so callers still see disks in the order a user expects.
fn enumerate_disk_indices() -> Result<Vec<u32>> {
    // SetupAPI reports failure as INVALID_HANDLE_VALUE, but `HDEVINFO` is an
    // `isize` in windows-sys (not a pointer like `HANDLE`), so the sentinel has
    // to be spelled as the bare -1 rather than compared to INVALID_HANDLE_VALUE.
    const INVALID_HDEVINFO: HDEVINFO = -1;

    let set = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_DISK,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if set == INVALID_HDEVINFO {
        return Err(PhoenixError::Disk(format!(
            "SetupDiGetClassDevs(GUID_DEVINTERFACE_DISK) failed (Win32 error {})",
            unsafe { GetLastError() }
        )));
    }

    let mut indices = Vec::new();
    for member in 0.. {
        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            InterfaceClassGuid: GUID_DEVINTERFACE_DISK,
            Flags: 0,
            Reserved: 0,
        };
        let more = unsafe {
            SetupDiEnumDeviceInterfaces(
                set,
                ptr::null(),
                &GUID_DEVINTERFACE_DISK,
                member,
                &mut iface,
            )
        };
        // Exhausted the class (ERROR_NO_MORE_ITEMS).
        if more == 0 {
            break;
        }
        if let Some(path) = interface_device_path(set, &iface) {
            match device_number(&path) {
                Some(index) => indices.push(index),
                // A disk-class interface with no device number is a device in
                // the middle of arriving or departing. Skip it rather than
                // guessing an index that may belong to something else.
                None => debug!(
                    device = path.as_str(),
                    "disk interface has no device number; skipping"
                ),
            }
        }
    }
    unsafe { SetupDiDestroyDeviceInfoList(set) };

    indices.sort_unstable();
    indices.dedup();
    Ok(indices)
}

/// Pull the `DevicePath` out of one `GUID_DEVINTERFACE_DISK` member.
///
/// `SP_DEVICE_INTERFACE_DETAIL_DATA_W` is a variable-length struct ? a fixed
/// header followed by an inline NUL-terminated `WCHAR` path ? so it is sized by
/// a first call, then filled into a byte buffer. `cbSize` must describe the
/// *header* (never the buffer), which is what the struct's own `size_of` is.
fn interface_device_path(set: HDEVINFO, iface: &SP_DEVICE_INTERFACE_DATA) -> Option<String> {
    let mut needed = 0u32;
    // Expected to fail with ERROR_INSUFFICIENT_BUFFER; we only want `needed`.
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set,
            iface,
            ptr::null_mut(),
            0,
            &mut needed,
            ptr::null_mut(),
        )
    };
    if (needed as usize) <= size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
        return None;
    }

    let mut buffer = vec![0u8; needed as usize];
    let detail = buffer.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
    let ok = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set,
            iface,
            detail,
            needed,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        debug!(
            last_error = unsafe { GetLastError() },
            "SetupDiGetDeviceInterfaceDetail failed; skipping disk interface"
        );
        return None;
    }

    // The path is inline at the tail of the struct, within the buffer we own.
    let path_ptr = unsafe { ptr::addr_of!((*detail).DevicePath) } as *const u16;
    let mut wide = Vec::new();
    for i in 0.. {
        let c = unsafe { *path_ptr.add(i) };
        if c == 0 {
            break;
        }
        wide.push(c);
    }
    Some(String::from_utf16_lossy(&wide))
}

/// The `PhysicalDriveN` index behind a device path, via
/// `IOCTL_STORAGE_GET_DEVICE_NUMBER`.
///
/// Opened with **zero** access rights: this is a query the storage stack answers
/// without granting any read or write capability on the medium, so it does not
/// need the elevation a `GENERIC_READ` handle to a physical disk would.
fn device_number(device_path: &str) -> Option<u32> {
    // CTL_CODE(IOCTL_STORAGE_BASE, 0x0420, METHOD_BUFFERED, FILE_ANY_ACCESS).
    const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002d_1080;
    #[repr(C)]
    struct StorageDeviceNumber {
        device_type: u32,
        device_number: u32,
        partition_number: u32,
    }

    let wide = to_wide(device_path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0, // query only ? no GENERIC_READ, so no elevation required
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut info = StorageDeviceNumber {
        device_type: 0,
        device_number: 0,
        partition_number: 0,
    };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            ptr::null(),
            0,
            &mut info as *mut _ as *mut _,
            size_of::<StorageDeviceNumber>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 || (returned as usize) < size_of::<StorageDeviceNumber>() {
        return None;
    }
    Some(info.device_number)
}

pub fn enumerate_disks() -> Result<Vec<DiskInfo>> {
    let mut disks = Vec::new();
    for index in enumerate_disk_indices()? {
        let path = format!(r"\\.\PhysicalDrive{index}");
        match open_disk_readonly(&path) {
            Ok(handle) => {
                if let Ok(disk) = read_disk_info(index, &path, handle) {
                    disks.push(disk);
                }
                unsafe { CloseHandle(handle) };
            }
            // Windows named this disk but won't let us open it ? a device that
            // departed mid-enumeration, or one held exclusively. Not fatal;
            // the rest of the disks are still worth reporting.
            Err(e) => {
                warn!(disk = path.as_str(), error = %e, "skipping disk that could not be opened");
                continue;
            }
        }
    }
    for disk in &mut disks {
        for part in &mut disk.partitions {
            refine_partition_fs(part);
            // BitLocker lock-state detection needs the *physical* view of
            // the partition's boot sector (the volume-device view above
            // presents plaintext once BitLocker is unlocked), so it can't
            // live inside refine_partition_fs — do it here where the disk
            // path is in scope.
            let on_disk =
                classify_partition_on_disk(&disk.path, part.offset_bytes, disk.sector_size);
            apply_bitlocker_state(part, on_disk);
        }
    }
    Ok(disks)
}

pub fn open_disk_readonly(path: &str) -> Result<HANDLE> {
    let wide = to_wide(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x8000_0000, // GENERIC_READ
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(PhoenixError::Disk(format!("cannot open {path}")));
    }
    Ok(handle)
}

/// Open a disk for read *and* write. Only [`probe_media`] needs this: SCSI
/// pass-through is refused outright on a read-only handle, even for the
/// data-in queries we issue. Returns `None` rather than an error because every
/// caller treats a refusal as "we just don't learn anything".
fn open_disk_readwrite(path: &str) -> Option<HANDLE> {
    let wide = to_wide(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0xc000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    (handle != INVALID_HANDLE_VALUE).then_some(handle)
}

pub fn open_volume_readonly(path: &str) -> Result<HANDLE> {
    open_disk_readonly(path)
}

/// Force-dismount a mounted volume (`\\.\X:`) via `FSCTL_DISMOUNT_VOLUME`.
/// The volume object is torn down **in place** (not ejected), so the next
/// access re-mounts it fresh and re-reads the on-disk boot sector.
///
/// This is the media-agnostic way to make Windows re-recognize a volume whose
/// sectors were rewritten underneath a live mount — notably a raw BitLocker
/// ciphertext restore, where the `-FVE-FS-` header lands *after* Windows has
/// already cached a non-BitLocker view of the freshly-created partition. It
/// works on removable USB media, where `Set-Disk -IsOffline` and diskpart
/// `offline disk` are rejected ("Removable media cannot be set to offline").
pub fn dismount_volume(volume_path: &str) -> Result<()> {
    const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020;
    let handle = open_volume_readonly(volume_path)?;
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_DISMOUNT_VOLUME,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    let err = unsafe { GetLastError() };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return Err(PhoenixError::Disk(format!(
            "FSCTL_DISMOUNT_VOLUME on {volume_path} failed (Win32 error {err})"
        )));
    }
    Ok(())
}

/// A mounted volume that has been **locked and dismounted** so its on-disk
/// sectors can be overwritten through a raw physical-disk handle.
///
/// Windows guards the sectors of a mounted volume: a raw write to them from a
/// `\\.\PhysicalDriveN` handle fails with `ERROR_ACCESS_DENIED` (Win32 5). A
/// full-disk restore avoids ever tripping this by planting the partition table
/// *last*, so no volume is mounted while data lands. A **partial** restore has
/// no such luxury — it writes into an existing table whose target slot may
/// still carry a live volume — so it clears the slot first with this guard:
/// `FSCTL_LOCK_VOLUME` takes exclusive access (and flushes the FS cache), then
/// `FSCTL_DISMOUNT_VOLUME` tears the volume down in place. The volume handle is
/// held open for the guard's whole lifetime so nothing can re-mount and
/// re-guard the region mid-write. Dropping the guard unlocks and closes the
/// handle; the next access re-mounts the volume fresh, re-reading the boot
/// sector the restore just wrote.
pub struct LockedVolume {
    handle: HANDLE,
    path: String,
}

impl LockedVolume {
    /// Lock, then dismount, the volume device at `volume_path` (`\\.\X:` or a
    /// `\\?\Volume{GUID}` device path with no trailing backslash — exactly the
    /// forms [`find_volume_for_partition`] / [`find_volume_guid_for_partition`]
    /// return). The lock is retried with exponential backoff (5 attempts,
    /// ~250 ms → 4 s) to ride out a transient handle held by an AV scan or the
    /// search indexer. Returns [`PhoenixError::VolumeLockFailed`] if the volume
    /// still can't be locked — another process holds files open on it, and the
    /// slot can't be safely overwritten while it's in use.
    pub fn acquire(volume_path: &str) -> Result<Self> {
        const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
        const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020;
        const ATTEMPTS: u32 = 5;

        let handle = open_volume_readonly(volume_path)?;

        let mut backoff_ms: u64 = 250;
        let mut last_err = 0u32;
        let mut locked = false;
        for attempt in 1..=ATTEMPTS {
            let mut returned = 0u32;
            let ok = unsafe {
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
            if ok != 0 {
                locked = true;
                break;
            }
            last_err = unsafe { GetLastError() };
            warn!(
                volume = volume_path,
                attempt,
                err = last_err,
                "FSCTL_LOCK_VOLUME on target slot failed; another process has files open on it"
            );
            if attempt < ATTEMPTS {
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms = backoff_ms.saturating_mul(2).min(4_000);
            }
        }
        if !locked {
            unsafe { CloseHandle(handle) };
            return Err(PhoenixError::VolumeLockFailed {
                drive: volume_path.to_string(),
                last_error: last_err,
            });
        }

        // Tear the mounted volume down in place. The still-locked handle stays
        // open (below) so nothing re-mounts it before the raw writes finish.
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_DISMOUNT_VOLUME,
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
            unsafe { CloseHandle(handle) };
            return Err(PhoenixError::Disk(format!(
                "FSCTL_DISMOUNT_VOLUME on {volume_path} failed (Win32 error {err})"
            )));
        }

        tracing::info!(
            volume = volume_path,
            "locked + dismounted target slot volume so its sectors can be restored"
        );
        Ok(Self {
            handle,
            path: volume_path.to_string(),
        })
    }
}

impl Drop for LockedVolume {
    fn drop(&mut self) {
        const FSCTL_UNLOCK_VOLUME: u32 = 0x0009_001C;
        let mut returned = 0u32;
        unsafe {
            // Unlock explicitly (CloseHandle would release the lock anyway, but
            // being explicit keeps the intent clear and the ordering defined).
            DeviceIoControl(
                self.handle,
                FSCTL_UNLOCK_VOLUME,
                ptr::null(),
                0,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            );
            CloseHandle(self.handle);
        }
        debug!(
            volume = self.path.as_str(),
            "released target slot volume lock; it will re-mount fresh"
        );
    }
}

fn read_disk_info(index: u32, path: &str, handle: HANDLE) -> Result<DiskInfo> {
    let size_bytes = get_disk_length(handle)?;
    let layout = get_drive_layout(handle)?;
    let sector_size = get_sector_size(handle);
    // Failure here is non-fatal: the disk still enumerates, just without a
    // model or drive type.
    let descriptor = query_device_descriptor(handle);
    // The storage stack's own answer on spinning-vs-solid-state, and the only
    // one that internal disks need.
    let seek_penalty = query_seek_penalty(handle);
    // USB bridges answer nothing above (Windows itself shows them as media type
    // "Unspecified"), so ask the device directly. Gated on the seek penalty
    // coming back unknown, which keeps the read/write handle `probe_media`
    // needs off every internal disk on a routine enumeration.
    let probe = if seek_penalty.is_none() {
        probe_media(path)
    } else {
        MediaProbe::default()
    };

    let descriptor_model = descriptor.as_ref().map(|d| {
        d.model
            .clone()
            .unwrap_or_else(|| bus_type_label(d.bus_type, d.removable))
    });
    // A drive behind a USB bridge names itself far more usefully than the
    // enclosure does ("ST4000LM024-2AN17V" beats "Seagate Expansion"), so the
    // probed model wins when we got one.
    let model = probe.model.or(descriptor_model);
    let drive_type = descriptor
        .as_ref()
        .map(|d| drive_type_label(d.bus_type, d.removable, seek_penalty.or(probe.spins)));

    let (is_gpt, disk_guid, disk_signature, partitions) = match layout {
        LayoutInfo::Gpt { disk_guid, entries } => {
            let sig = u64::from_le_bytes(disk_guid[0..8].try_into().unwrap());
            let parts = entries
                .into_iter()
                .enumerate()
                .map(|(i, e)| {
                    let fs_kind = classify_partition(&e.type_guid, &e.name);
                    let capture_mode = if fs_kind == FilesystemKind::Efi
                        || fs_kind == FilesystemKind::Msr
                        || fs_kind == FilesystemKind::Bitlocker
                        || fs_kind == FilesystemKind::Unknown
                    {
                        CaptureMode::Raw
                    } else {
                        CaptureMode::UsedBlocks
                    };
                    // Prefer the drive-letter device; fall back to the
                    // un-lettered volume-GUID device (ESP/Recovery) so those
                    // partitions can be locked/snapshotted during capture
                    // instead of being read unprotected off the live disk.
                    let volume_path = find_volume_for_partition(index, e.starting_offset, e.length)
                        .or_else(|| {
                            find_volume_guid_for_partition(index, e.starting_offset, e.length)
                        });
                    let drive_letter = volume_path.as_deref().and_then(parse_drive_letter);
                    let volume_label = volume_path.as_deref().and_then(query_volume_label);
                    let usage = volume_path.as_deref().and_then(query_partition_usage);
                    PartitionInfo {
                        index: i as u32,
                        name: e.name,
                        type_guid: e.type_guid,
                        unique_guid: e.unique_guid,
                        // GPT source: no MBR identity to carry.
                        mbr_type: 0,
                        mbr_bootable: false,
                        gpt_attributes: e.attributes,
                        offset_bytes: e.starting_offset,
                        size_bytes: e.length,
                        fs_kind,
                        capture_mode,
                        bitlocker: BitlockerState::None,
                        volume_path,
                        drive_letter,
                        volume_label,
                        usage,
                        sector_size,
                    }
                })
                .collect();
            (true, Some(disk_guid), sig, parts)
        }
        LayoutInfo::Mbr { signature, entries } => {
            let parts = entries
                .into_iter()
                .enumerate()
                .map(|(i, e)| {
                    let fs_kind = FilesystemKind::Unknown;
                    let volume_path = find_volume_for_partition(index, e.starting_offset, e.length)
                        .or_else(|| {
                            find_volume_guid_for_partition(index, e.starting_offset, e.length)
                        });
                    let drive_letter = volume_path.as_deref().and_then(parse_drive_letter);
                    let volume_label = volume_path.as_deref().and_then(query_volume_label);
                    let usage = volume_path.as_deref().and_then(query_partition_usage);
                    PartitionInfo {
                        index: i as u32,
                        name: format!("Partition{i}"),
                        type_guid: [0u8; 16],
                        unique_guid: [0u8; 16],
                        mbr_type: e.partition_type,
                        mbr_bootable: e.bootable,
                        gpt_attributes: 0,
                        offset_bytes: e.starting_offset,
                        size_bytes: e.length,
                        fs_kind,
                        capture_mode: CaptureMode::Raw,
                        bitlocker: BitlockerState::None,
                        volume_path,
                        drive_letter,
                        volume_label,
                        usage,
                        sector_size,
                    }
                })
                .collect();
            (false, None, u64::from(signature), parts)
        }
    };

    Ok(DiskInfo {
        index,
        path: path.to_string(),
        size_bytes,
        is_gpt,
        disk_guid,
        disk_signature,
        sector_size,
        model,
        drive_type,
        partitions,
    })
}

/// The parts of `STORAGE_DEVICE_DESCRIPTOR` we care about, lifted out of the
/// raw buffer so the pointer arithmetic stays in one place.
struct DeviceDescriptor {
    /// Vendor + product strings, when the device exposes them. Blank for many
    /// USB bridges, virtual disks, and RAID controllers.
    model: Option<String>,
    bus_type: i32,
    removable: u8,
}

/// Ask the storage driver for the device descriptor via
/// `IOCTL_STORAGE_QUERY_PROPERTY`. Any IOCTL or parse failure produces `None`,
/// which the caller treats as "nothing is known about this device" — the GUI
/// then omits both the model and the drive-type row.
///
/// The 1024-byte buffer is intentionally generous: the descriptor header
/// is ~40 bytes and Windows packs the four ASCIIZ strings (vendor, product,
/// revision, serial) at the offsets it returns, all of which sit well
/// inside that limit on every real device we've seen. If a device ever
/// returns a longer descriptor we silently truncate at the buffer end.
fn query_device_descriptor(handle: HANDLE) -> Option<DeviceDescriptor> {
    const BUF_SIZE: usize = 1024;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0u8; 1],
    };
    let mut buffer = [0u8; BUF_SIZE];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const _,
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buffer.as_mut_ptr() as *mut _,
            BUF_SIZE as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 || (returned as usize) < size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        debug!(
            last_error = unsafe { GetLastError() },
            "IOCTL_STORAGE_QUERY_PROPERTY failed; model unknown"
        );
        return None;
    }

    // Pull the fixed header out before any pointer arithmetic on the buffer.
    let descriptor = unsafe { &*(buffer.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    let vendor_off = descriptor.VendorIdOffset as usize;
    let product_off = descriptor.ProductIdOffset as usize;
    let bus_type = descriptor.BusType;
    let removable = descriptor.RemovableMedia;

    let read_at = |off: usize| -> Option<String> {
        if off == 0 || off >= BUF_SIZE {
            return None;
        }
        let bytes = &buffer[off..];
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let s = std::str::from_utf8(&bytes[..len]).ok()?.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };

    let model = match (read_at(vendor_off), read_at(product_off)) {
        (Some(v), Some(p)) if v.eq_ignore_ascii_case(&p) => Some(p),
        (Some(v), Some(p)) => Some(format!("{v} {p}")),
        (Some(v), None) => Some(v),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    };

    Some(DeviceDescriptor {
        model,
        bus_type,
        removable,
    })
}

/// Ask whether the device incurs a seek penalty — i.e. whether it spins.
/// This is the only signal Windows offers every storage driver for telling a
/// hard disk from a solid-state one, and it is genuinely optional: plenty of
/// USB bridges and virtual disks reject the query, hence `None` for "unknown"
/// rather than a guess.
fn query_seek_penalty(handle: HANDLE) -> Option<bool> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0u8; 1],
    };
    let mut descriptor = DEVICE_SEEK_PENALTY_DESCRIPTOR {
        Version: 0,
        Size: 0,
        IncursSeekPenalty: 0,
    };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const _,
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &mut descriptor as *mut _ as *mut _,
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 || (returned as usize) < size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() {
        debug!(
            last_error = unsafe { GetLastError() },
            "seek-penalty query failed; SSD/HDD class unknown"
        );
        return None;
    }
    Some(descriptor.IncursSeekPenalty != 0)
}

/// What a device says about itself when the storage stack has nothing to say —
/// the result of talking SCSI to it directly.
#[derive(Default)]
struct MediaProbe {
    /// `Some(true)` spins, `Some(false)` is solid-state, `None` wouldn't say.
    spins: Option<bool>,
    /// The model of the drive *behind* a bridge, when it can be reached.
    model: Option<String>,
}

/// Ask a device what medium it is by issuing SCSI commands to it, for the
/// devices Windows can't classify (in practice: everything on USB).
///
/// Two routes, because neither covers every enclosure:
///   * INQUIRY VPD page 0xB1 (Block Device Characteristics), whose rotation
///     rate field the bridge answers itself; and
///   * SAT ATA PASS-THROUGH of IDENTIFY DEVICE, which reaches past the bridge
///     to the drive, yielding both the rate and the drive's real model.
///
/// Both are read-only queries. A device that implements neither (typically a
/// thumb drive) leaves everything `None` and the caller falls back to the bus.
fn probe_media(path: &str) -> MediaProbe {
    // SPTD rejects a read-only handle with ACCESS_DENIED even though we only
    // ever issue data-in commands, so this handle asks for write access it
    // never uses. If that's refused, we simply learn nothing.
    let Some(handle) = open_disk_readwrite(path) else {
        debug!(
            path,
            "no read/write handle; skipping media pass-through probe"
        );
        return MediaProbe::default();
    };

    let vpd_rate = inquiry_rotation_rate(handle);
    let identify = ata_identify(handle);
    unsafe { CloseHandle(handle) };

    let ata_rate = identify
        .as_ref()
        .and_then(|id| id.get(434..436))
        .map(|w| u16::from_le_bytes([w[0], w[1]]));
    // Words 27..46 hold the model. The two sources agree when both answer, so
    // either order works; VPD is the cheaper and more standard one.
    let model = identify.as_ref().and_then(|id| ata_string(id, 27, 20));

    MediaProbe {
        spins: vpd_rate
            .and_then(rate_spins)
            .or(ata_rate.and_then(rate_spins)),
        model,
    }
}

/// Interpret a SCSI/ATA nominal rotation rate. Both standards use the same
/// encoding: 1 means the medium doesn't rotate, a plain RPM value means it
/// does, and everything else (notably 0, "not reported") means the device
/// declined to say — which is emphatically not the same as "solid state".
fn rate_spins(rate: u16) -> Option<bool> {
    match rate {
        0x0001 => Some(false),
        0x0401..=0xfffe => Some(true),
        _ => None,
    }
}

/// Pull an ATA string out of an IDENTIFY DEVICE response. ATA stores them
/// byte-swapped within each 16-bit word, and pads with spaces.
fn ata_string(identify: &[u8], word_start: usize, words: usize) -> Option<String> {
    let bytes = identify.get(word_start * 2..(word_start + words) * 2)?;
    let s: String = bytes
        .chunks_exact(2)
        .flat_map(|w| [w[1], w[0]])
        .map(|b| if b.is_ascii_graphic() { b as char } else { ' ' })
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// INQUIRY for VPD page 0xB1, returning its MEDIUM ROTATION RATE field.
fn inquiry_rotation_rate(handle: HANDLE) -> Option<u16> {
    // opcode, EVPD=1, page 0xB1, 2-byte allocation length, control.
    const ALLOC: u16 = 64;
    let cdb = [
        0x12,
        0x01,
        0xb1,
        (ALLOC >> 8) as u8,
        (ALLOC & 0xff) as u8,
        0x00,
    ];
    let data = scsi_data_in(handle, &cdb, ALLOC as u32)?;
    Some(u16::from_be_bytes([*data.get(4)?, *data.get(5)?]))
}

/// IDENTIFY DEVICE (ATA 0xEC) wrapped in a SAT ATA PASS-THROUGH command, which
/// a SCSI/ATA-translating bridge forwards to the drive it fronts. Bridges vary
/// in which CDB length they accept, so try the 16-byte form and then the 12.
fn ata_identify(handle: HANDLE) -> Option<Vec<u8>> {
    // byte 1: protocol 4 (PIO data-in) << 1. byte 2: T_DIR=from device,
    // BYT_BLOK=blocks, T_LENGTH=in the sector-count field. Sector count 1,
    // command 0xEC — i.e. "send me one 512-byte block of IDENTIFY data".
    const CDB16: [u8; 16] = [0x85, 0x08, 0x0e, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0xec, 0];
    const CDB12: [u8; 12] = [0xa1, 0x08, 0x0e, 0, 1, 0, 0, 0, 0, 0, 0xec, 0];
    scsi_data_in(handle, &CDB16, 512).or_else(|| scsi_data_in(handle, &CDB12, 512))
}

/// A pass-through request and its sense buffer in one allocation — the sense
/// data is addressed as an offset into the very struct we submit.
#[repr(C)]
struct SptdWithSense {
    sptd: SCSI_PASS_THROUGH_DIRECT,
    sense: [u8; 32],
}

/// SPTD hands the data buffer to the adapter, which enforces its own alignment
/// mask; 512 clears anything a real controller asks for.
#[repr(C, align(512))]
struct AlignedBuf([u8; 512]);

/// Issue one data-in SCSI command and hand back the bytes the device returned.
/// A device that doesn't implement the command answers with a CHECK CONDITION
/// rather than a transport failure, so both are folded into `None`.
fn scsi_data_in(handle: HANDLE, cdb: &[u8], transfer_len: u32) -> Option<Vec<u8>> {
    debug_assert!(cdb.len() <= 16 && transfer_len as usize <= 512);
    let mut buf = AlignedBuf([0u8; 512]);
    let mut req = SptdWithSense {
        sptd: SCSI_PASS_THROUGH_DIRECT {
            Length: size_of::<SCSI_PASS_THROUGH_DIRECT>() as u16,
            ScsiStatus: 0,
            PathId: 0,
            TargetId: 0,
            Lun: 0,
            CdbLength: cdb.len() as u8,
            SenseInfoLength: 32,
            DataIn: SCSI_IOCTL_DATA_IN as u8,
            DataTransferLength: transfer_len,
            TimeOutValue: SCSI_TIMEOUT_SECS,
            DataBuffer: buf.0.as_mut_ptr() as *mut _,
            SenseInfoOffset: offset_of!(SptdWithSense, sense) as u32,
            Cdb: [0u8; 16],
        },
        sense: [0u8; 32],
    };
    req.sptd.Cdb[..cdb.len()].copy_from_slice(cdb);

    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_SCSI_PASS_THROUGH_DIRECT,
            &mut req as *mut _ as *mut _,
            size_of::<SptdWithSense>() as u32,
            &mut req as *mut _ as *mut _,
            size_of::<SptdWithSense>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        debug!(
            opcode = cdb[0],
            last_error = unsafe { GetLastError() },
            "SCSI pass-through failed"
        );
        return None;
    }
    if req.sptd.ScsiStatus != 0 {
        debug!(
            opcode = cdb[0],
            scsi_status = req.sptd.ScsiStatus,
            sense_key = req.sense[2] & 0x0f,
            asc = req.sense[12],
            "device rejected SCSI command"
        );
        return None;
    }
    Some(buf.0[..transfer_len as usize].to_vec())
}

/// Human drive type for the disk cards: the bus, plus the media class where
/// the bus alone doesn't imply it. `seek_penalty` is the SSD/HDD discriminator
/// (`Some(true)` spins, `Some(false)` doesn't, `None` wouldn't say), so a bus
/// that carries both kinds — SATA, SAS, USB — degrades to the bare bus name
/// rather than inventing a media class it can't know.
fn drive_type_label(bus_type: i32, removable: u8, seek_penalty: Option<bool>) -> String {
    // "SATA" -> "SATA SSD" / "SATA HDD" / "SATA" (unknown).
    let with_media = |bus: &str| match seek_penalty {
        Some(true) => format!("{bus} HDD"),
        Some(false) => format!("{bus} SSD"),
        None => bus.to_string(),
    };
    match bus_type {
        0x01 => with_media("SCSI"),
        0x02 | 0x03 => with_media("ATA"),
        0x04 => "FireWire".into(),
        0x06 => "Fibre Channel".into(),
        // A USB SSD/HDD enclosure reports the bridge's seek penalty when it has
        // one; thumb drives typically answer nothing and are flagged removable.
        0x07 => match seek_penalty {
            Some(true) => "USB HDD".into(),
            Some(false) => "USB SSD".into(),
            None if removable != 0 => "USB Flash Drive".into(),
            None => "USB Drive".into(),
        },
        0x08 => "RAID".into(),
        0x09 => "iSCSI".into(),
        0x0A => with_media("SAS"),
        0x0B => with_media("SATA"),
        0x0C => "SD Card".into(),
        0x0D => "eMMC".into(),
        0x0E | 0x0F => "Virtual Disk".into(),
        0x10 => "Storage Space".into(),
        // NVMe and UFS are solid-state by definition, so the bus names the media.
        0x11 => "NVMe SSD".into(),
        0x12 => "Persistent Memory".into(),
        0x13 => "UFS".into(),
        _ => match (seek_penalty, removable) {
            (Some(true), _) => "HDD".into(),
            (Some(false), _) => "SSD".into(),
            (None, r) if r != 0 => "Removable".into(),
            (None, _) => "Fixed Disk".into(),
        },
    }
}

/// Map `STORAGE_BUS_TYPE` values (defined in `ntddstor.h`) to a short
/// human label. Used as the fallback when the device descriptor returns
/// no vendor/product strings; falls through to "Removable" / "Fixed"
/// for bus types we don't have a friendlier label for. windows-sys
/// exposes `STORAGE_BUS_TYPE` as `i32`, so we match on that directly.
fn bus_type_label(bus_type: i32, removable: u8) -> String {
    match bus_type {
        0x01 => "SCSI".into(),
        0x03 => "ATA".into(),
        0x04 => "FireWire".into(),
        0x07 => "USB".into(),
        0x08 => "RAID".into(),
        0x09 => "iSCSI".into(),
        0x0A => "SAS".into(),
        0x0B => "SATA".into(),
        0x0C => "SD".into(),
        0x0D => "MMC".into(),
        0x0E | 0x0F => "Virtual".into(),
        0x11 => "NVMe".into(),
        _ => {
            if removable != 0 {
                "Removable".into()
            } else {
                "Fixed".into()
            }
        }
    }
}

struct GptEntry {
    name: String,
    type_guid: [u8; 16],
    unique_guid: [u8; 16],
    attributes: u64,
    starting_offset: u64,
    length: u64,
}

struct MbrEntry {
    starting_offset: u64,
    length: u64,
    /// MBR partition type byte (0x07 NTFS/exFAT, 0x0C FAT32-LBA, 0x27
    /// recovery…). The MBR analogue of a GPT type GUID.
    partition_type: u8,
    /// The active/boot flag. On a BIOS disk this is what the MBR boot code
    /// looks for to decide which partition to chain into.
    bootable: bool,
}

enum LayoutInfo {
    Gpt {
        disk_guid: [u8; 16],
        entries: Vec<GptEntry>,
    },
    Mbr {
        signature: u32,
        entries: Vec<MbrEntry>,
    },
}

/// Query the disk's logical (addressable) sector size via
/// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`. Falls back to 512 on failure so a
/// device that refuses the IOCTL still enumerates. 4Kn disks report 4096
/// here; that value drives I/O alignment in the capture/restore paths (the
/// on-disk extent unit itself stays a fixed 512-byte LBA — see the format
/// spec — so this only affects read/write alignment, not extent math).
// `HANDLE` is a raw pointer in the type system, but it is never dereferenced
// here: it is handed to `DeviceIoControl`, which the kernel validates and fails
// cleanly on a bad handle. Callers (restore's layout writer and NTFS extender)
// already hold an open device handle and want its geometry — asking them to
// wrap this in `unsafe` would imply a soundness obligation that doesn't exist.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn get_sector_size(handle: HANDLE) -> u32 {
    // Value of CTL_CODE(IOCTL_DISK_BASE, 0x0028, METHOD_BUFFERED, FILE_ANY_ACCESS).
    const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
    // DISK_GEOMETRY_EX begins with DISK_GEOMETRY; BytesPerSector is the last
    // u32 of that leading struct, at offset 20. A 32-byte buffer covers it.
    let mut buf = [0u8; 32];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            ptr::null(),
            0,
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 || returned < 24 {
        return 512;
    }
    let bytes_per_sector = u32::from_le_bytes(buf[20..24].try_into().unwrap());
    // Sanity-clamp: sector sizes are powers of two between 512 and 4096 on
    // all supported media. Anything else is treated as the 512 default.
    match bytes_per_sector {
        512 | 1024 | 2048 | 4096 => bytes_per_sector,
        _ => 512,
    }
}

fn get_disk_length(handle: HANDLE) -> Result<u64> {
    #[repr(C)]
    struct LengthInfo {
        length: i64,
    }
    let mut info = LengthInfo { length: 0 };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            ptr::null(),
            0,
            &mut info as *mut _ as *mut _,
            size_of::<LengthInfo>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(PhoenixError::Disk(
            "IOCTL_DISK_GET_LENGTH_INFO failed".into(),
        ));
    }
    Ok(info.length as u64)
}

fn get_drive_layout(handle: HANDLE) -> Result<LayoutInfo> {
    let buf_size =
        size_of::<DRIVE_LAYOUT_INFORMATION_EX>() + 128 * size_of::<PARTITION_INFORMATION_EX>();
    let mut buffer = vec![0u8; buf_size];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
            ptr::null(),
            0,
            buffer.as_mut_ptr() as *mut _,
            buf_size as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(PhoenixError::Disk(
            "IOCTL_DISK_GET_DRIVE_LAYOUT_EX failed".into(),
        ));
    }

    let layout = unsafe { &*(buffer.as_ptr() as *const DRIVE_LAYOUT_INFORMATION_EX) };
    if layout.PartitionStyle == PARTITION_STYLE_GPT as u32 {
        let gpt = unsafe { layout.Anonymous.Gpt };
        let disk_guid_arr: [u8; 16] = unsafe { std::mem::transmute(gpt.DiskId) };

        let entry_ptr = layout.PartitionEntry.as_ptr();
        let mut entries = Vec::new();
        for i in 0..layout.PartitionCount {
            let part = unsafe { &*entry_ptr.add(i as usize) };
            let gpt = unsafe { part.Anonymous.Gpt };
            let name_wide: Vec<u16> = gpt.Name.iter().take_while(|&&c| c != 0).copied().collect();
            let name = String::from_utf16_lossy(&name_wide);
            let type_guid: [u8; 16] = unsafe { std::mem::transmute(gpt.PartitionType) };
            let unique_guid: [u8; 16] = unsafe { std::mem::transmute(gpt.PartitionId) };
            entries.push(GptEntry {
                name,
                type_guid,
                unique_guid,
                attributes: gpt.Attributes,
                starting_offset: part.StartingOffset as u64,
                length: part.PartitionLength as u64,
            });
        }
        Ok(LayoutInfo::Gpt {
            disk_guid: disk_guid_arr,
            entries,
        })
    } else {
        let entry_ptr = layout.PartitionEntry.as_ptr();
        let mut entries = Vec::new();
        for i in 0..layout.PartitionCount {
            let part = unsafe { &*entry_ptr.add(i as usize) };
            if part.PartitionLength == 0 {
                continue;
            }
            // SAFETY: the union's Mbr arm is the live one — this branch runs
            // only when the layout reported PARTITION_STYLE_MBR.
            let mbr = unsafe { &part.Anonymous.Mbr };
            entries.push(MbrEntry {
                starting_offset: part.StartingOffset as u64,
                length: part.PartitionLength as u64,
                partition_type: mbr.PartitionType,
                bootable: mbr.BootIndicator != 0,
            });
        }
        let signature = unsafe { layout.Anonymous.Mbr }.Signature;
        Ok(LayoutInfo::Mbr { signature, entries })
    }
}

fn classify_partition(type_guid: &[u8; 16], name: &str) -> FilesystemKind {
    // Well-known GPT type GUIDs (little-endian byte order as stored)
    const EFI_SYSTEM: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    const MSR: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";
    const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

    let guid_str = format_guid(type_guid);
    let upper = guid_str.to_uppercase();
    match upper.as_str() {
        EFI_SYSTEM => FilesystemKind::Efi,
        MSR => FilesystemKind::Msr,
        BASIC_DATA => {
            // FS detected from volume when possible
            FilesystemKind::Unknown
        }
        _ => {
            let n = name.to_lowercase();
            if n.contains("efi") {
                FilesystemKind::Efi
            } else if n.contains("recovery") {
                FilesystemKind::Ntfs
            } else {
                FilesystemKind::Unknown
            }
        }
    }
}

pub fn detect_volume_fs(volume_path: &str) -> FilesystemKind {
    if let Some(detected) = detect_volume_fs_via_get_volume_information(volume_path) {
        return detected;
    }
    // GetVolumeInformationW returns success-with-"Unknown" on some
    // BitLocker-protected NTFS volumes even when they're unlocked, because the
    // BitLocker filter driver intercepts the FS query. Fall back to reading
    // the partition's first sector and matching well-known FS magic bytes.
    detect_volume_fs_via_boot_sector(volume_path).unwrap_or(FilesystemKind::Unknown)
}

/// Ask Windows for the filesystem name via `GetVolumeInformationW`. Returns
/// `None` either when the call fails or when the reported FS name is
/// something we don't know how to map (e.g. the literal string "Unknown" that
/// Windows can return for BitLocker-protected volumes).
fn detect_volume_fs_via_get_volume_information(volume_path: &str) -> Option<FilesystemKind> {
    // GetVolumeInformationW expects a volume mount point such as `C:\` or
    // `\\?\Volume{guid}\`. The `\\.\X:` device-namespace form used elsewhere
    // for CreateFileW is rejected, which is why every NTFS drive previously
    // came back as Unknown. Convert to the `X:\` root form here.
    let letter = parse_drive_letter(volume_path)?;
    let root = format!("{letter}:\\");
    let wide = to_wide(&root);
    let mut fs_name = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        warn!(
            volume = root.as_str(),
            err = unsafe { GetLastError() },
            "GetVolumeInformationW failed (will try boot-sector fallback)"
        );
        return None;
    }
    let len = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    let fs = String::from_utf16_lossy(&fs_name[..len]).to_uppercase();
    debug!(
        volume = root.as_str(),
        fs = fs.as_str(),
        "GetVolumeInformationW returned"
    );
    match fs.as_str() {
        "NTFS" => Some(FilesystemKind::Ntfs),
        "FAT" | "FAT32" | "FAT16" | "FAT12" => Some(FilesystemKind::Fat),
        "EXFAT" => Some(FilesystemKind::Exfat),
        "REFS" => Some(FilesystemKind::Refs),
        _ => None,
    }
}

/// Open the partition by its `\\.\X:` path and read its boot sector / VBR, then
/// match well-known signatures. This sidesteps `GetVolumeInformationW` entirely,
/// so it works for partitions with no drive letter (Recovery, ESP) ? which
/// `GetVolumeInformationW` cannot even be asked about ? and for
/// BitLocker-protected NTFS volumes whose decrypted view the filter driver hides
/// from the FS query.
///
/// Magic bytes (all within the first 512 bytes):
/// - `NTFS    ` at offset 3 (8 bytes) -> NTFS
/// - `EXFAT   ` at offset 3           -> exFAT
/// - `ReFS\0\0\0\0` at offset 3 + `FSRS` at offset 16 -> ReFS
/// - `FAT32   ` at offset 82          -> FAT32
/// - `FAT12   ` / `FAT16   ` at offset 54 -> FAT
/// - `-FVE-FS-` at offset 3           -> BitLocker (locked or
///   metadata-shadowed view; we can't read the underlying NTFS).
///
/// **Reads a whole sector, not 512 bytes.** A read through a volume *device*
/// handle must transfer a multiple of the volume's logical sector size, so on
/// 4Kn media a 512-byte `ReadFile` fails outright with `ERROR_INVALID_PARAMETER`
/// (87) ? it does not return a short read. That made this function fail on
/// *every* volume of a 4Kn disk, which silently disabled both the no-drive-letter
/// FS detection and all BitLocker volume-view detection (the only thing that
/// spots `-FVE-FS-` here). A 4096-byte read is legal on 512-byte media too (it
/// is a multiple of 512, at aligned offset 0), so the larger read is correct on
/// both rather than conditional. Only the first 512 bytes are ever classified.
///
/// Returns `None` when the volume can't be opened (typically locked
/// BitLocker, raw EFI/MSR, or otherwise unmounted).
pub fn detect_volume_fs_via_boot_sector(volume_path: &str) -> Option<FilesystemKind> {
    let wide = to_wide(volume_path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x8000_0000, // GENERIC_READ
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        warn!(
            volume = volume_path,
            err = unsafe { GetLastError() },
            "detect_volume_fs_via_boot_sector: CreateFileW failed"
        );
        return None;
    }

    // One sector, sized to the volume itself; `get_sector_size` falls back to
    // 512 if the device won't answer, and `max` keeps the transfer legal on the
    // 4Kn media where that fallback would otherwise be too small.
    let read_len = get_sector_size(handle).max(512) as usize;
    let mut buf = vec![0u8; read_len];
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(
            handle,
            buf.as_mut_ptr(),
            read_len as u32,
            &mut read,
            ptr::null_mut(),
        )
    };
    let read_err = unsafe { GetLastError() };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        warn!(
            volume = volume_path,
            err = read_err,
            "detect_volume_fs_via_boot_sector: ReadFile failed"
        );
        return None;
    }
    if (read as usize) < buf.len() {
        warn!(
            volume = volume_path,
            read = read,
            "detect_volume_fs_via_boot_sector: short read"
        );
        return None;
    }

    let result = classify_boot_sector(&buf[..512]);
    if result.is_none() {
        warn!(
            volume = volume_path,
            oem = String::from_utf8_lossy(&buf[3..11]).to_string().as_str(),
            "detect_volume_fs_via_boot_sector: no FS magic recognized"
        );
    } else {
        debug!(
            volume = volume_path,
            kind = ?result,
            "detect_volume_fs_via_boot_sector: success"
        );
    }
    result
}

/// Pure helper for `detect_volume_fs_via_boot_sector`; split out so it can be
/// unit-tested without touching Win32.
fn classify_boot_sector(buf: &[u8]) -> Option<FilesystemKind> {
    if buf.len() < 512 {
        return None;
    }
    let oem = &buf[3..11];
    if oem == b"NTFS    " {
        return Some(FilesystemKind::Ntfs);
    }
    // ReFS has no BPB: bytes 3..11 hold `ReFS\0\0\0\0` (NUL-padded, not
    // space-padded like the OEM IDs above) and the FSRS superblock header
    // magic sits at offset 16. Require both so a stray "ReFS" string in a
    // garbage sector can't misclassify.
    if oem == b"ReFS\0\0\0\0" && &buf[16..20] == b"FSRS" {
        return Some(FilesystemKind::Refs);
    }
    if oem == b"EXFAT   " {
        return Some(FilesystemKind::Exfat);
    }
    if oem == b"-FVE-FS-" {
        return Some(FilesystemKind::Bitlocker);
    }
    // FAT32 stores its FS-type label at offset 82..90.
    let fat32_label = &buf[82..90];
    if fat32_label == b"FAT32   " {
        return Some(FilesystemKind::Fat);
    }
    // FAT12/FAT16 store theirs at offset 54..62.
    let fat1216_label = &buf[54..62];
    if fat1216_label == b"FAT12   " || fat1216_label == b"FAT16   " || fat1216_label == b"FAT     "
    {
        return Some(FilesystemKind::Fat);
    }
    None
}

pub fn refine_partition_fs(part: &mut PartitionInfo) {
    // Never re-type an EFI System or MSR partition. The ESP contains a FAT32
    // filesystem, so now that un-lettered partitions get a volume path (for
    // lock/snapshot consistency), boot-sector detection would reclassify it
    // as plain `Fat` — flipping capture to used-blocks and, on an MBR
    // restore, stamping type 0x0C instead of 0xEF. The GPT type GUID is the
    // authoritative identity for these; keep them Efi/Msr + raw.
    if matches!(part.fs_kind, FilesystemKind::Efi | FilesystemKind::Msr) {
        return;
    }
    if let Some(ref vol) = part.volume_path {
        let detected = detect_volume_fs(vol);
        if detected != FilesystemKind::Unknown {
            part.fs_kind = detected;
            part.capture_mode = match detected {
                FilesystemKind::Efi | FilesystemKind::Msr | FilesystemKind::Bitlocker => {
                    CaptureMode::Raw
                }
                _ => CaptureMode::UsedBlocks,
            };
        }
    }
    if part.drive_letter.is_none() {
        part.drive_letter = part.volume_path.as_deref().and_then(parse_drive_letter);
    }
    if part.volume_label.is_none() {
        part.volume_label = part.volume_path.as_deref().and_then(query_volume_label);
    }
    if part.usage.is_none() {
        part.usage = part.volume_path.as_deref().and_then(query_partition_usage);
    }
}

/// Classify the **on-disk (physical) view** of a partition's boot sector by
/// reading through the disk device (`\\.\PhysicalDriveN`) at the partition
/// offset. For a BitLocker volume this always sees the FVE ciphertext header
/// (`-FVE-FS-`) regardless of lock state — unlike the volume-device view,
/// which presents the decrypted filesystem once the volume is unlocked.
/// Comparing the two views is what tells us a volume is BitLocker *and*
/// unlocked (see [`apply_bitlocker_state`]).
///
/// Reads `max(512, sector_size)` bytes so the read stays sector-aligned on
/// 4Kn media. Returns `None` on any open/seek/read failure or when no FS
/// magic is recognized.
pub fn classify_partition_on_disk(
    disk_path: &str,
    offset_bytes: u64,
    sector_size: u32,
) -> Option<FilesystemKind> {
    use windows_sys::Win32::Storage::FileSystem::SetFilePointerEx;

    let wide = to_wide(disk_path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x8000_0000, // GENERIC_READ
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        debug!(
            disk = disk_path,
            err = unsafe { GetLastError() },
            "classify_partition_on_disk: CreateFileW failed"
        );
        return None;
    }

    let read_len = (sector_size.max(512)) as usize;
    let mut buf = vec![0u8; read_len];
    let mut read = 0u32;
    let ok = unsafe {
        let mut new_pos = 0i64;
        if SetFilePointerEx(handle, offset_bytes as i64, &mut new_pos, 0) == 0 {
            CloseHandle(handle);
            return None;
        }
        ReadFile(
            handle,
            buf.as_mut_ptr(),
            read_len as u32,
            &mut read,
            ptr::null_mut(),
        )
    };
    let read_err = unsafe { GetLastError() };
    unsafe { CloseHandle(handle) };
    if ok == 0 || (read as usize) < 512 {
        debug!(
            disk = disk_path,
            offset = offset_bytes,
            read = read,
            err = read_err,
            "classify_partition_on_disk: read failed or short"
        );
        return None;
    }
    classify_boot_sector(&buf[..512])
}

/// Combine the volume-device view (already refined into `part.fs_kind`) with
/// the on-disk physical view of the boot sector to resolve the partition's
/// BitLocker state, adjusting `fs_kind`/`capture_mode` for the locked case.
///
/// Decision table (`on_disk` = physical view, `part.fs_kind` = volume view):
///
/// | on_disk       | volume view        | result                                   |
/// |---------------|--------------------|------------------------------------------|
/// | BitLocker     | NTFS/FAT/exFAT/ReFS| `Unlocked` — normal used-block capture   |
/// | BitLocker     | anything else     | `Locked` — fs=Bitlocker, raw ciphertext  |
/// | not BitLocker | Bitlocker         | `Locked` — volume handle reads FVE bytes |
/// | not BitLocker | anything else     | `None`                                   |
///
/// The third row covers the "metadata-shadowed" volume view (the volume
/// device itself presents `-FVE-FS-`): whatever handle capture opens will
/// read ciphertext, so raw is the only capture that faithfully round-trips.
pub fn apply_bitlocker_state(part: &mut PartitionInfo, on_disk: Option<FilesystemKind>) {
    let disk_is_fve = on_disk == Some(FilesystemKind::Bitlocker);
    let volume_is_plaintext = matches!(
        part.fs_kind,
        FilesystemKind::Ntfs | FilesystemKind::Fat | FilesystemKind::Exfat | FilesystemKind::Refs
    );
    if disk_is_fve && volume_is_plaintext {
        part.bitlocker = BitlockerState::Unlocked;
    } else if disk_is_fve || part.fs_kind == FilesystemKind::Bitlocker {
        part.bitlocker = BitlockerState::Locked;
        part.fs_kind = FilesystemKind::Bitlocker;
        part.capture_mode = CaptureMode::Raw;
    } else {
        part.bitlocker = BitlockerState::None;
    }
}

/// Extract the drive letter (e.g. `'C'`) from a `\\.\X:` style volume path.
pub fn parse_drive_letter(volume_path: &str) -> Option<char> {
    // Accept formats produced by find_volume_for_partition (`\\.\X:`) and the
    // Win32 `\\?\X:\` form for good measure.
    let stripped = volume_path
        .strip_prefix(r"\\.\")
        .or_else(|| volume_path.strip_prefix(r"\\?\"))
        .unwrap_or(volume_path);
    let mut chars = stripped.chars();
    let letter = chars.next()?;
    let colon = chars.next()?;
    if colon != ':' || !letter.is_ascii_alphabetic() {
        return None;
    }
    Some(letter.to_ascii_uppercase())
}

/// Read the volume label using `GetVolumeInformationW` on the `X:\` form of
/// the volume path. Returns `None` for unmounted, locked, or unlabeled volumes.
pub fn query_volume_label(volume_path: &str) -> Option<String> {
    let letter = parse_drive_letter(volume_path)?;
    let root = format!("{letter}:\\");
    let wide = to_wide(&root);
    let mut name = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        warn!(
            volume = root.as_str(),
            err = unsafe { GetLastError() },
            "query_volume_label: GetVolumeInformationW failed"
        );
        return None;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let label = String::from_utf16_lossy(&name[..len]);
    if label.trim().is_empty() {
        None
    } else {
        Some(label)
    }
}

/// Query total/free bytes for a partition using `GetDiskFreeSpaceExW`. Returns
/// `None` for volumes that aren't mounted or whose filesystem can't be read
/// (BitLocker-locked drives, raw EFI/MSR partitions, etc.).
pub fn query_partition_usage(volume_path: &str) -> Option<PartitionUsage> {
    let letter = parse_drive_letter(volume_path)?;
    let root = format!("{letter}:\\");
    let wide = to_wide(&root);
    let mut free_to_caller = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        warn!(
            volume = root.as_str(),
            err = unsafe { GetLastError() },
            "query_partition_usage: GetDiskFreeSpaceExW failed"
        );
        return None;
    }
    if total == 0 {
        warn!(
            volume = root.as_str(),
            "query_partition_usage: GetDiskFreeSpaceExW reported total=0"
        );
        return None;
    }
    debug!(
        volume = root.as_str(),
        total = total,
        free = total_free,
        "query_partition_usage: success"
    );
    Some(PartitionUsage {
        total_bytes: total,
        free_bytes: total_free,
    })
}

/// Find the `\\.\X:` volume device path whose extent exactly covers
/// the partition at `(disk_index, offset, length)`. Used at enumeration
/// time to attach drive letters to partitions, and at restore time to
/// hand the post-restore extender (`phoenix_restore::grow`) a handle
/// it can call `FSCTL_EXTEND_VOLUME` against. Returns `None` if no
/// mounted volume matches (unmounted partitions, EFI/MSR, etc.).
pub fn find_volume_for_partition(disk_index: u32, offset: u64, length: u64) -> Option<String> {
    for letter in b'C'..=b'Z' {
        // Open the volume *device* (`\\.\C:`), not the directory `C:\`. The
        // latter requires `FILE_FLAG_BACKUP_SEMANTICS` to open at all, and
        // even then it's a directory handle that IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS
        // doesn't accept. The previous formatting silently failed for every
        // letter, leaving `volume_path = None` for all partitions.
        let vol = format!(r"\\.\{}:", letter as char);
        if volume_covers_extent(&vol, disk_index, offset, length) {
            debug!(
                volume = vol.as_str(),
                disk = disk_index,
                offset = offset,
                length = length,
                "find_volume_for_partition: matched"
            );
            return Some(vol);
        }
    }
    debug!(
        disk = disk_index,
        offset = offset,
        length = length,
        "find_volume_for_partition: no drive-letter match"
    );
    None
}

/// Find the `\\?\Volume{GUID}` device for a partition that has **no drive
/// letter** — the EFI System and Recovery partitions on a typical Windows
/// disk. Mountmgr creates a volume device for every filesystem-bearing
/// partition regardless of letter assignment, so these can be opened,
/// snapshotted, and — critically — **locked** like any lettered volume.
/// Without this, un-lettered partitions were captured raw from the live
/// physical disk with no consistency protection at all, and a multi-hour
/// backup window let the OS write to the ESP between capture and verify
/// (caught live by verify-after-backup on a real boot-disk capture).
///
/// Returns the path WITHOUT the trailing backslash (`\\?\Volume{...}`), the
/// form `CreateFileW` needs to open the volume *device* rather than its
/// root directory. Returns `None` for partitions with no volume device
/// (MSR — not a filesystem volume at all).
pub fn find_volume_guid_for_partition(disk_index: u32, offset: u64, length: u64) -> Option<String> {
    let mut name = [0u16; 512];
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut result = None;
    loop {
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let vol = String::from_utf16_lossy(&name[..len]);
        let device = vol.trim_end_matches('\\').to_string();
        if volume_covers_extent(&device, disk_index, offset, length) {
            debug!(
                volume = device.as_str(),
                disk = disk_index,
                offset = offset,
                length = length,
                "find_volume_guid_for_partition: matched un-lettered volume"
            );
            result = Some(device);
            break;
        }
        let more = unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) };
        if more == 0 {
            break;
        }
    }
    unsafe { FindVolumeClose(find) };
    result
}

/// Whether the volume device at `vol_path` consists of exactly one disk
/// extent matching `(disk_index, offset, length)`. Shared by the drive-letter
/// and volume-GUID discovery paths.
fn volume_covers_extent(vol_path: &str, disk_index: u32, offset: u64, length: u64) -> bool {
    let wide = to_wide(vol_path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    #[repr(C)]
    struct DiskExtent {
        disk_number: u32,
        starting_offset: i64,
        extent_length: i64,
    }
    #[repr(C)]
    struct VolumeDiskExtents {
        number_of_disk_extents: u32,
        extents: [DiskExtent; 1],
    }
    let mut extents = VolumeDiskExtents {
        number_of_disk_extents: 0,
        extents: [DiskExtent {
            disk_number: 0,
            starting_offset: 0,
            extent_length: 0,
        }],
    };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            ptr::null(),
            0,
            &mut extents as *mut _ as *mut _,
            size_of::<VolumeDiskExtents>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    ok != 0
        && extents.number_of_disk_extents > 0
        && extents.extents[0].disk_number == disk_index
        && extents.extents[0].starting_offset as u64 == offset
        && extents.extents[0].extent_length as u64 == length
}

fn format_guid(bytes: &[u8; 16]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
        u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn guid_to_string(g: &[u8; 16]) -> String {
    format_guid(g)
}

/// The GPT partition type GUID of the EFI System Partition
/// (`C12A7328-F81F-11D2-BA4B-00A0C93EC93B`), stored on-disk in the mixed-endian
/// byte layout `format_guid` decodes.
pub const EFI_SYSTEM_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];

/// True when `type_guid` identifies an EFI System Partition, regardless of the
/// filesystem probed on it or whether the OS has (unusually) assigned it a
/// drive letter. The ESP is small, static, and kept mounted by the firmware/OS
/// — Windows refuses both `FSCTL_LOCK_VOLUME` and a VSS snapshot on it — so the
/// capture/clone freeze logic treats it as read-unfrozen rather than aborting,
/// exactly as it does for an ESP that has no letter.
pub fn is_efi_system_partition(type_guid: &[u8; 16]) -> bool {
    type_guid == &EFI_SYSTEM_TYPE_GUID
}

/// Parse a dashed GUID string back into the on-disk mixed-endian byte layout
/// `format_guid`/`guid_to_string` produce (data1/2/3 little-endian, data4
/// verbatim — the same layout `windows-sys`'s `GUID` uses). Returns `None`
/// for malformed input.
pub fn guid_from_string(s: &str) -> Option<[u8; 16]> {
    let u = uuid::Uuid::parse_str(s.trim()).ok()?;
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
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bus types (SATA, USB) that carry either medium must never invent one:
    /// "not reported" is not "solid state".
    #[test]
    fn rotation_rate_only_claims_what_the_device_said() {
        assert_eq!(rate_spins(0x0001), Some(false)); // non-rotating
        assert_eq!(rate_spins(7200), Some(true));
        assert_eq!(rate_spins(5526), Some(true)); // real ST4000LM024 value
        assert_eq!(rate_spins(0x0000), None); // "not reported"
        assert_eq!(rate_spins(0x0200), None); // reserved
        assert_eq!(rate_spins(0xffff), None); // reserved
    }

    #[test]
    fn drive_type_names_media_only_when_it_is_known() {
        assert_eq!(drive_type_label(0x0b, 0, Some(true)), "SATA HDD");
        assert_eq!(drive_type_label(0x0b, 0, Some(false)), "SATA SSD");
        assert_eq!(drive_type_label(0x0b, 0, None), "SATA");
        // NVMe is solid-state by definition; no probe needed.
        assert_eq!(drive_type_label(0x11, 0, None), "NVMe SSD");
        // A USB HDD, once the pass-through probe reaches the drive behind it.
        assert_eq!(drive_type_label(0x07, 0, Some(true)), "USB HDD");
        // A thumb drive answers nothing, but flags itself removable.
        assert_eq!(drive_type_label(0x07, 1, None), "USB Flash Drive");
        assert_eq!(drive_type_label(0x07, 0, None), "USB Drive");
    }

    /// ATA packs its strings byte-swapped within each 16-bit word, and pads
    /// them out with spaces.
    #[test]
    fn ata_string_unswaps_and_trims() {
        let model = b"ST4000LM024-2AN17V  ";
        let mut identify = vec![0u8; 512];
        for (word, pair) in model.chunks_exact(2).enumerate() {
            identify[54 + word * 2] = pair[1];
            identify[54 + word * 2 + 1] = pair[0];
        }
        assert_eq!(
            ata_string(&identify, 27, 10).as_deref(),
            Some("ST4000LM024-2AN17V")
        );
        // A drive that reports nothing yields nothing, not an empty string.
        assert_eq!(ata_string(&vec![0u8; 512], 27, 20), None);
        // Short or truncated responses are refused rather than panicking.
        assert_eq!(ata_string(&[0u8; 4], 27, 20), None);
    }

    fn boot_sector_with(oem: &[u8; 8]) -> [u8; 512] {
        let mut buf = [0u8; 512];
        buf[3..11].copy_from_slice(oem);
        buf
    }

    #[test]
    fn classify_boot_sector_recognizes_ntfs() {
        let buf = boot_sector_with(b"NTFS    ");
        assert_eq!(classify_boot_sector(&buf), Some(FilesystemKind::Ntfs));
    }

    #[test]
    fn classify_boot_sector_recognizes_exfat() {
        let buf = boot_sector_with(b"EXFAT   ");
        assert_eq!(classify_boot_sector(&buf), Some(FilesystemKind::Exfat));
    }

    #[test]
    fn classify_boot_sector_recognizes_bitlocker() {
        let buf = boot_sector_with(b"-FVE-FS-");
        assert_eq!(classify_boot_sector(&buf), Some(FilesystemKind::Bitlocker));
    }

    #[test]
    fn classify_boot_sector_recognizes_fat32_label() {
        let mut buf = [0u8; 512];
        buf[3..11].copy_from_slice(b"MSWIN4.1");
        buf[82..90].copy_from_slice(b"FAT32   ");
        assert_eq!(classify_boot_sector(&buf), Some(FilesystemKind::Fat));
    }

    #[test]
    fn classify_boot_sector_recognizes_fat16_label() {
        let mut buf = [0u8; 512];
        buf[54..62].copy_from_slice(b"FAT16   ");
        assert_eq!(classify_boot_sector(&buf), Some(FilesystemKind::Fat));
    }

    #[test]
    fn classify_boot_sector_recognizes_refs() {
        // ReFS pads its FS name with NULs (not spaces) and carries the FSRS
        // superblock magic at offset 16.
        let mut buf = boot_sector_with(b"ReFS\0\0\0\0");
        buf[16..20].copy_from_slice(b"FSRS");
        assert_eq!(classify_boot_sector(&buf), Some(FilesystemKind::Refs));
    }

    #[test]
    fn classify_boot_sector_refuses_refs_name_without_fsrs_magic() {
        // The name alone must not classify — a stray "ReFS" string in a
        // garbage sector is not a ReFS volume.
        let buf = boot_sector_with(b"ReFS\0\0\0\0");
        assert_eq!(classify_boot_sector(&buf), None);
        // Space-padded (NTFS-style) name is equally not ReFS.
        let mut spaced = boot_sector_with(b"ReFS    ");
        spaced[16..20].copy_from_slice(b"FSRS");
        assert_eq!(classify_boot_sector(&spaced), None);
    }

    #[test]
    fn classify_boot_sector_returns_none_for_garbage() {
        let buf = [0u8; 512];
        assert_eq!(classify_boot_sector(&buf), None);
    }

    #[test]
    fn refs_round_trips_through_u8_wire_format() {
        // Byte 120 of a PartitionIndexEntry is the on-disk fs_kind; make sure
        // the new discriminant survives and old bytes still map sanely.
        assert_eq!(
            FilesystemKind::from_u8(FilesystemKind::Refs as u8),
            FilesystemKind::Refs
        );
        assert_eq!(FilesystemKind::Refs as u8, 7);
        assert_eq!(FilesystemKind::from_u8(200), FilesystemKind::Unknown);
    }

    fn part_with(fs_kind: FilesystemKind, capture_mode: CaptureMode) -> PartitionInfo {
        PartitionInfo {
            index: 0,
            name: "test".into(),
            type_guid: [0u8; 16],
            unique_guid: [0u8; 16],
            mbr_type: 0,
            mbr_bootable: false,
            gpt_attributes: 0,
            offset_bytes: 1024 * 1024,
            size_bytes: 64 * 1024 * 1024,
            fs_kind,
            capture_mode,
            bitlocker: BitlockerState::None,
            volume_path: Some(r"\\.\X:".into()),
            drive_letter: Some('X'),
            volume_label: None,
            usage: None,
            sector_size: 512,
        }
    }

    #[test]
    fn bitlocker_unlocked_keeps_used_block_capture() {
        // Physical view = FVE ciphertext, volume view = plaintext NTFS:
        // the unlocked case must stay a completely normal capture.
        let mut p = part_with(FilesystemKind::Ntfs, CaptureMode::UsedBlocks);
        apply_bitlocker_state(&mut p, Some(FilesystemKind::Bitlocker));
        assert_eq!(p.bitlocker, BitlockerState::Unlocked);
        assert_eq!(p.fs_kind, FilesystemKind::Ntfs);
        assert_eq!(p.capture_mode, CaptureMode::UsedBlocks);
    }

    #[test]
    fn bitlocker_unlocked_fat_and_exfat_also_normal() {
        for fs in [FilesystemKind::Fat, FilesystemKind::Exfat] {
            let mut p = part_with(fs, CaptureMode::UsedBlocks);
            apply_bitlocker_state(&mut p, Some(FilesystemKind::Bitlocker));
            assert_eq!(p.bitlocker, BitlockerState::Unlocked);
            assert_eq!(p.fs_kind, fs);
            assert_eq!(p.capture_mode, CaptureMode::UsedBlocks);
        }
    }

    #[test]
    fn bitlocker_locked_forces_raw() {
        // Physical view = FVE, volume view unreadable/unknown: locked.
        let mut p = part_with(FilesystemKind::Unknown, CaptureMode::Raw);
        apply_bitlocker_state(&mut p, Some(FilesystemKind::Bitlocker));
        assert_eq!(p.bitlocker, BitlockerState::Locked);
        assert_eq!(p.fs_kind, FilesystemKind::Bitlocker);
        assert_eq!(p.capture_mode, CaptureMode::Raw);
    }

    #[test]
    fn bitlocker_locked_when_volume_view_is_fve() {
        // Volume device itself presents -FVE-FS- (metadata-shadowed view),
        // even if the physical read failed: capture reads ciphertext → raw.
        let mut p = part_with(FilesystemKind::Bitlocker, CaptureMode::Raw);
        apply_bitlocker_state(&mut p, None);
        assert_eq!(p.bitlocker, BitlockerState::Locked);
        assert_eq!(p.fs_kind, FilesystemKind::Bitlocker);
        assert_eq!(p.capture_mode, CaptureMode::Raw);
    }

    #[test]
    fn non_bitlocker_partitions_untouched() {
        for (fs, mode) in [
            (FilesystemKind::Ntfs, CaptureMode::UsedBlocks),
            (FilesystemKind::Efi, CaptureMode::Raw),
            (FilesystemKind::Unknown, CaptureMode::Raw),
        ] {
            let mut p = part_with(fs, mode);
            apply_bitlocker_state(&mut p, Some(fs));
            assert_eq!(p.bitlocker, BitlockerState::None);
            assert_eq!(p.fs_kind, fs);
            assert_eq!(p.capture_mode, mode);
        }
    }

    #[test]
    fn guid_string_roundtrip() {
        let g: [u8; 16] = [
            0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
            0xC9, 0x3B,
        ];
        let s = guid_to_string(&g);
        assert_eq!(s, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(guid_from_string(&s), Some(g));
        assert_eq!(guid_from_string("not-a-guid"), None);
    }

    #[test]
    fn parse_drive_letter_handles_dos_namespace() {
        assert_eq!(parse_drive_letter(r"\\.\C:"), Some('C'));
        assert_eq!(parse_drive_letter(r"\\?\D:\"), Some('D'));
        assert_eq!(parse_drive_letter("E:"), Some('E'));
        assert_eq!(parse_drive_letter(r"\\.\PhysicalDrive0"), None);
    }

    #[test]
    fn efi_system_partition_guid_is_recognized() {
        // The constant must decode to the canonical ESP type GUID, or the
        // freeze logic would abort on a lettered ESP again.
        assert_eq!(
            guid_to_string(&EFI_SYSTEM_TYPE_GUID),
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        );
        assert!(is_efi_system_partition(&EFI_SYSTEM_TYPE_GUID));
        // Basic data partition GUID must NOT be treated as an ESP.
        let basic_data =
            guid_from_string("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7").expect("valid guid");
        assert!(!is_efi_system_partition(&basic_data));
    }
}
