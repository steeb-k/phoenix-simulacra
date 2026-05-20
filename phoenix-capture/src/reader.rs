use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use phoenix_core::disk::{open_disk_readonly, open_volume_readonly};
use phoenix_core::error::{PhoenixError, Result};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

pub struct PartitionReader {
    handle: HANDLE,
    offset: u64,
    length: u64,
}

impl PartitionReader {
    pub fn open_disk_partition(disk_path: &str, offset_bytes: u64, length_bytes: u64) -> Result<Self> {
        let handle = open_disk_readonly(disk_path)?;
        Ok(Self {
            handle,
            offset: offset_bytes,
            length: length_bytes,
        })
    }

    pub fn open_volume(volume_path: &str) -> Result<Self> {
        let handle = open_volume_readonly(volume_path)?;
        let mut size = 0u64;
        #[repr(C)]
        struct LengthInfo {
            length: i64,
        }
        let mut info = LengthInfo { length: 0 };
        let mut returned = 0u32;
        let ok = unsafe {
            windows_sys::Win32::System::IO::DeviceIoControl(
                handle,
                windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_LENGTH_INFO,
                ptr::null(),
                0,
                &mut info as *mut _ as *mut _,
                mem::size_of::<LengthInfo>() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok != 0 {
            size = info.length as u64;
        }
        Ok(Self {
            handle,
            offset: 0,
            length: size,
        })
    }

    pub fn from_path(path: &str, offset: u64, length: u64) -> Result<Self> {
        if path.contains("PhysicalDrive") {
            Self::open_disk_partition(path, offset, length)
        } else {
            let _ = (offset, length);
            Self::open_volume(path)
        }
    }

    pub fn read_at(&mut self, position: u64, buf: &mut [u8]) -> Result<usize> {
        if position >= self.length {
            return Ok(0);
        }
        let abs_pos = self.offset + position;
        unsafe {
            let mut dist = 0i64;
            let low = (abs_pos & 0xFFFF_FFFF) as u32;
            let high = (abs_pos >> 32) as u32;
            if windows_sys::Win32::Storage::FileSystem::SetFilePointerEx(
                self.handle,
                ((high as i64) << 32) | low as i64,
                &mut dist,
                0,
            ) == 0
            {
                return Err(PhoenixError::Disk("seek failed".into()));
            }
        }
        let to_read = buf.len().min((self.length - position) as usize);
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr() as *mut _,
                to_read as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(PhoenixError::Disk("read failed".into()));
        }
        Ok(read as usize)
    }

    pub fn length(&self) -> u64 {
        self.length
    }
}

impl Drop for PartitionReader {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

pub fn open_physical_disk(path: &str) -> Result<HANDLE> {
    open_disk_readonly(path)
}
