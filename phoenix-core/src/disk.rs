//! Windows physical disk and partition enumeration.

use std::ffi::OsStr;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use tracing::{debug, warn};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetVolumeInformationW, ReadFile, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Ioctl::{
    DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_STORAGE_QUERY_PROPERTY, PARTITION_INFORMATION_EX, PARTITION_STYLE_GPT,
    PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
    StorageDeviceProperty,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::error::{PhoenixError, Result};

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
            _ => Self::Unknown,
        }
    }
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
    pub offset_bytes: u64,
    pub size_bytes: u64,
    pub fs_kind: FilesystemKind,
    pub capture_mode: CaptureMode,
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
    pub partitions: Vec<PartitionInfo>,
}

pub fn enumerate_disks() -> Result<Vec<DiskInfo>> {
    let mut disks = Vec::new();
    for i in 0..64 {
        let path = format!(r"\\.\PhysicalDrive{i}");
        match open_disk_readonly(&path) {
            Ok(handle) => {
                if let Ok(disk) = read_disk_info(i, &path, handle) {
                    disks.push(disk);
                }
                unsafe { CloseHandle(handle) };
            }
            Err(_) => continue,
        }
    }
    for disk in &mut disks {
        for part in &mut disk.partitions {
            refine_partition_fs(part);
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

pub fn open_volume_readonly(path: &str) -> Result<HANDLE> {
    open_disk_readonly(path)
}

fn read_disk_info(index: u32, path: &str, handle: HANDLE) -> Result<DiskInfo> {
    let size_bytes = get_disk_length(handle)?;
    let layout = get_drive_layout(handle)?;
    let sector_size = get_sector_size(handle);
    // Failure here is non-fatal: the disk still enumerates without a model.
    let model = query_disk_model(handle);

    let (is_gpt, disk_guid, disk_signature, partitions) = match layout {
        LayoutInfo::Gpt {
            disk_guid,
            entries,
        } => {
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
                    let volume_path = find_volume_for_partition(
                        index,
                        e.starting_offset,
                        e.length,
                    );
                    let drive_letter = volume_path
                        .as_deref()
                        .and_then(parse_drive_letter);
                    let volume_label = volume_path
                        .as_deref()
                        .and_then(query_volume_label);
                    let usage = volume_path
                        .as_deref()
                        .and_then(query_partition_usage);
                    PartitionInfo {
                        index: i as u32,
                        name: e.name,
                        type_guid: e.type_guid,
                        gpt_attributes: e.attributes,
                        offset_bytes: e.starting_offset,
                        size_bytes: e.length,
                        fs_kind,
                        capture_mode,
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
                    let volume_path = find_volume_for_partition(
                        index,
                        e.starting_offset,
                        e.length,
                    );
                    let drive_letter = volume_path
                        .as_deref()
                        .and_then(parse_drive_letter);
                    let volume_label = volume_path
                        .as_deref()
                        .and_then(query_volume_label);
                    let usage = volume_path
                        .as_deref()
                        .and_then(query_partition_usage);
                    PartitionInfo {
                        index: i as u32,
                        name: format!("Partition{i}"),
                        type_guid: [0u8; 16],
                        gpt_attributes: 0,
                        offset_bytes: e.starting_offset,
                        size_bytes: e.length,
                        fs_kind,
                        capture_mode: CaptureMode::Raw,
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
        partitions,
    })
}

/// Ask the storage driver for the device's vendor + product strings via
/// `IOCTL_STORAGE_QUERY_PROPERTY`. When those are blank (common for USB
/// bridges, virtual disks, and some RAID controllers) fall back to a label
/// derived from `BusType` / `RemovableMedia` so the GUI dropdown still has
/// *something* useful to show. Any IOCTL or parse failure produces `None`,
/// which the caller treats as "this disk has no model" — the dropdown just
/// omits the trailing segment.
///
/// The 1024-byte buffer is intentionally generous: the descriptor header
/// is ~40 bytes and Windows packs the four ASCIIZ strings (vendor, product,
/// revision, serial) at the offsets it returns, all of which sit well
/// inside that limit on every real device we've seen. If a device ever
/// returns a longer descriptor we silently truncate at the buffer end.
fn query_disk_model(handle: HANDLE) -> Option<String> {
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
        debug!(last_error = unsafe { GetLastError() }, "IOCTL_STORAGE_QUERY_PROPERTY failed; model unknown");
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

    let combined = match (read_at(vendor_off), read_at(product_off)) {
        (Some(v), Some(p)) if v.eq_ignore_ascii_case(&p) => Some(p),
        (Some(v), Some(p)) => Some(format!("{v} {p}")),
        (Some(v), None) => Some(v),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    };

    combined.or_else(|| Some(bus_type_label(bus_type, removable)))
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
    attributes: u64,
    starting_offset: u64,
    length: u64,
}

struct MbrEntry {
    starting_offset: u64,
    length: u64,
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
fn get_sector_size(handle: HANDLE) -> u32 {
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
        return Err(PhoenixError::Disk("IOCTL_DISK_GET_LENGTH_INFO failed".into()));
    }
    Ok(info.length as u64)
}

fn get_drive_layout(handle: HANDLE) -> Result<LayoutInfo> {
    let buf_size = size_of::<DRIVE_LAYOUT_INFORMATION_EX>() + 128 * size_of::<PARTITION_INFORMATION_EX>();
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
        return Err(PhoenixError::Disk("IOCTL_DISK_GET_DRIVE_LAYOUT_EX failed".into()));
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
            let name_wide: Vec<u16> = gpt
                .Name
                .iter()
                .take_while(|&&c| c != 0)
                .copied()
                .collect();
            let name = String::from_utf16_lossy(&name_wide);
            let type_guid: [u8; 16] = unsafe { std::mem::transmute(gpt.PartitionType) };
            entries.push(GptEntry {
                name,
                type_guid,
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
            entries.push(MbrEntry {
                starting_offset: part.StartingOffset as u64,
                length: part.PartitionLength as u64,
            });
        }
        let signature = unsafe { layout.Anonymous.Mbr }.Signature;
        Ok(LayoutInfo::Mbr {
            signature,
            entries,
        })
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
    let len = fs_name.iter().position(|&c| c == 0).unwrap_or(fs_name.len());
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
        _ => None,
    }
}

/// Open the partition by its `\\.\X:` path and read the first 512 bytes; match
/// well-known signatures in the boot sector / VBR. This sidesteps
/// `GetVolumeInformationW` entirely, so it works for BitLocker-protected NTFS
/// volumes whose decrypted view the filter driver hides from the FS query.
///
/// Magic bytes:
/// - `NTFS    ` at offset 3 (8 bytes) -> NTFS
/// - `EXFAT   ` at offset 3           -> exFAT
/// - `FAT32   ` at offset 82          -> FAT32
/// - `FAT12   ` / `FAT16   ` at offset 54 -> FAT
/// - `-FVE-FS-` at offset 3           -> BitLocker (locked or
///   metadata-shadowed view; we can't read the underlying NTFS).
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

    let mut buf = [0u8; 512];
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(
            handle,
            buf.as_mut_ptr(),
            buf.len() as u32,
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

    let result = classify_boot_sector(&buf);
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
    if fat1216_label == b"FAT12   " || fat1216_label == b"FAT16   " || fat1216_label == b"FAT     " {
        return Some(FilesystemKind::Fat);
    }
    None
}

pub fn refine_partition_fs(part: &mut PartitionInfo) {
    if let Some(ref vol) = part.volume_path {
        let detected = detect_volume_fs(vol);
        if detected != FilesystemKind::Unknown {
            part.fs_kind = detected;
            part.capture_mode = match detected {
                FilesystemKind::Efi
                | FilesystemKind::Msr
                | FilesystemKind::Bitlocker => CaptureMode::Raw,
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
pub fn find_volume_for_partition(
    disk_index: u32,
    offset: u64,
    length: u64,
) -> Option<String> {
    for letter in b'C'..=b'Z' {
        // Open the volume *device* (`\\.\C:`), not the directory `C:\`. The
        // latter requires `FILE_FLAG_BACKUP_SEMANTICS` to open at all, and
        // even then it's a directory handle that IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS
        // doesn't accept. The previous formatting silently failed for every
        // letter, leaving `volume_path = None` for all partitions.
        let vol = format!(r"\\.\{}:", letter as char);
        let wide = to_wide(&vol);
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
            continue;
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
        if ok != 0
            && extents.number_of_disk_extents > 0
            && extents.extents[0].disk_number == disk_index
            && extents.extents[0].starting_offset as u64 == offset
            && extents.extents[0].extent_length as u64 == length
        {
            let path = format!(r"\\.\{}:", letter as char);
            debug!(
                volume = path.as_str(),
                disk = disk_index,
                offset = offset,
                length = length,
                "find_volume_for_partition: matched"
            );
            return Some(path);
        }
    }
    debug!(
        disk = disk_index,
        offset = offset,
        length = length,
        "find_volume_for_partition: no match"
    );
    None
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn classify_boot_sector_returns_none_for_garbage() {
        let buf = [0u8; 512];
        assert_eq!(classify_boot_sector(&buf), None);
    }

    #[test]
    fn parse_drive_letter_handles_dos_namespace() {
        assert_eq!(parse_drive_letter(r"\\.\C:"), Some('C'));
        assert_eq!(parse_drive_letter(r"\\?\D:\"), Some('D'));
        assert_eq!(parse_drive_letter("E:"), Some('E'));
        assert_eq!(parse_drive_letter(r"\\.\PhysicalDrive0"), None);
    }
}
