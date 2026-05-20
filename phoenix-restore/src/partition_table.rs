//! Apply GPT partition layout to a target disk before restore.

use std::mem::size_of;
use std::ptr;

use phoenix_core::error::{PhoenixError, Result};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_SET_DRIVE_LAYOUT_EX, PARTITION_INFORMATION_EX,
    PARTITION_STYLE_GPT,
};

use crate::plan::RestorePlan;

/// Update GPT partition entry offsets/sizes on the target disk from the restore plan.
///
/// Requires Administrator. The target disk should already be initialized as GPT.
pub fn apply_gpt_layout(disk_path: &str, plan: &RestorePlan) -> Result<()> {
    let handle = open_disk_for_write(disk_path)?;
    let restoring: Vec<_> = plan.entries.iter().filter(|e| e.restore).collect();
    if restoring.is_empty() {
        return Ok(());
    }

    let header_size = size_of::<DRIVE_LAYOUT_INFORMATION_EX>();
    let entry_size = size_of::<PARTITION_INFORMATION_EX>();
    let buf_size = header_size + restoring.len() * entry_size;
    let mut buffer = vec![0u8; buf_size];

    let layout = unsafe { &mut *(buffer.as_mut_ptr() as *mut DRIVE_LAYOUT_INFORMATION_EX) };
    layout.PartitionStyle = PARTITION_STYLE_GPT as u32;
    layout.PartitionCount = restoring.len() as u32;

    let entry_ptr = layout.PartitionEntry.as_mut_ptr();
    for (i, entry) in restoring.iter().enumerate() {
        let part = unsafe { &mut *entry_ptr.add(i) };
        part.PartitionStyle = PARTITION_STYLE_GPT;
        part.StartingOffset = entry.target_offset_bytes as i64;
        part.PartitionLength = entry.target_size_bytes as i64;
        part.PartitionNumber = (i + 1) as u32;
        part.RewritePartition = 1;
        unsafe {
            part.Anonymous.Gpt = windows_sys::Win32::System::Ioctl::PARTITION_INFORMATION_GPT {
                PartitionType: windows_sys::core::GUID {
                    data1: 0xebd0a0a2,
                    data2: 0xb9e5,
                    data3: 0x4433,
                    data4: [0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7],
                },
                PartitionId: windows_sys::core::GUID {
                    data1: 0,
                    data2: 0,
                    data3: 0,
                    data4: [0, 0, 0, 0, 0, 0, 0, 0],
                },
                Attributes: 0,
                Name: [0; 36],
            };
        }
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
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }
    if ok == 0 {
        tracing::warn!(
            "Could not set partition layout via IOCTL; ensure partitions exist at planned offsets"
        );
    }
    Ok(())
}

fn open_disk_for_write(path: &str) -> Result<HANDLE> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateFileW(
            wide.as_ptr(),
            0x4000_0000 | 0x8000_0000,
            windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
            ptr::null(),
            windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(PhoenixError::Disk("cannot open disk for write".into()));
    }
    Ok(handle)
}
