//! Windows physical disk and partition enumeration.

use std::ffi::OsStr;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetVolumeInformationW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Ioctl::{
    DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_GET_LENGTH_INFO,
    PARTITION_INFORMATION_EX, PARTITION_STYLE_GPT,
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
}

impl FilesystemKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Ntfs,
            2 => Self::Fat,
            3 => Self::Exfat,
            4 => Self::Efi,
            5 => Self::Msr,
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

#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub index: u32,
    pub name: String,
    pub type_guid: [u8; 16],
    pub offset_bytes: u64,
    pub size_bytes: u64,
    pub fs_kind: FilesystemKind,
    pub capture_mode: CaptureMode,
    pub volume_path: Option<String>,
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
    let sector_size = 512u32;

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
                        || fs_kind == FilesystemKind::Unknown
                    {
                        CaptureMode::Raw
                    } else {
                        CaptureMode::UsedBlocks
                    };
                    PartitionInfo {
                        index: i as u32,
                        name: e.name,
                        type_guid: e.type_guid,
                        offset_bytes: e.starting_offset,
                        size_bytes: e.length,
                        fs_kind,
                        capture_mode,
                        volume_path: find_volume_for_partition(
                            index,
                            e.starting_offset,
                            e.length,
                        ),
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
                    PartitionInfo {
                        index: i as u32,
                        name: format!("Partition{i}"),
                        type_guid: [0u8; 16],
                        offset_bytes: e.starting_offset,
                        size_bytes: e.length,
                        fs_kind,
                        capture_mode: CaptureMode::Raw,
                        volume_path: find_volume_for_partition(
                            index,
                            e.starting_offset,
                            e.length,
                        ),
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
        partitions,
    })
}

struct GptEntry {
    name: String,
    type_guid: [u8; 16],
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
        // DiskId follows the fixed header; offset after PartitionStyle(u32)+PartitionCount(u32)
        let disk_id_offset = 8usize;
        let mut disk_guid_arr = [0u8; 16];
        if buffer.len() >= disk_id_offset + 16 {
            disk_guid_arr.copy_from_slice(&buffer[disk_id_offset..disk_id_offset + 16]);
        }

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
    let wide = to_wide(volume_path);
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
        return FilesystemKind::Unknown;
    }
    let len = fs_name.iter().position(|&c| c == 0).unwrap_or(0);
    let fs = String::from_utf16_lossy(&fs_name[..len]).to_uppercase();
    match fs.as_str() {
        "NTFS" => FilesystemKind::Ntfs,
        "FAT" | "FAT32" => FilesystemKind::Fat,
        "EXFAT" => FilesystemKind::Exfat,
        _ => FilesystemKind::Unknown,
    }
}

pub fn refine_partition_fs(part: &mut PartitionInfo) {
    if let Some(ref vol) = part.volume_path {
        let detected = detect_volume_fs(vol);
        if detected != FilesystemKind::Unknown {
            part.fs_kind = detected;
            part.capture_mode = match detected {
                FilesystemKind::Efi | FilesystemKind::Msr => CaptureMode::Raw,
                _ => CaptureMode::UsedBlocks,
            };
        }
    }
}

fn find_volume_for_partition(
    disk_index: u32,
    offset: u64,
    length: u64,
) -> Option<String> {
    for letter in b'C'..=b'Z' {
        let vol = format!("{}:\\", letter as char);
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
            return Some(format!(r"\\.\{letter}:"));
        }
    }
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
