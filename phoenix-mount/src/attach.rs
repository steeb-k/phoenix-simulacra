//! Attach a VHD file read-only via the Windows virtual-disk API so its
//! partitions surface as real volumes. Requires Administrator.
//!
//! The attach is tied to the open handle: as long as [`AttachedDisk`] is alive
//! the disk stays attached, and dropping it detaches (we do not request
//! `PERMANENT_LIFETIME`). This makes cleanup automatic even on panic.

use std::os::windows::ffi::OsStrExt;

use phoenix_core::error::{PhoenixError, Result};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::Vhd::{
    AttachVirtualDisk, OpenVirtualDisk, ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY,
    ATTACH_VIRTUAL_DISK_PARAMETERS, OPEN_VIRTUAL_DISK_FLAG_NONE, OPEN_VIRTUAL_DISK_PARAMETERS,
    VIRTUAL_DISK_ACCESS_ATTACH_RO, VIRTUAL_DISK_ACCESS_GET_INFO, VIRTUAL_STORAGE_TYPE,
};

// VIRTUAL_STORAGE_TYPE_DEVICE_VHD = 2.
const STORAGE_TYPE_DEVICE_VHD: u32 = 2;

pub struct AttachedDisk {
    handle: HANDLE,
}

impl AttachedDisk {
    /// Open `vhd_path` and attach it read-only. Windows assigns drive letters
    /// to the contained volumes (subject to mount-manager policy).
    pub fn attach_readonly(vhd_path: &str) -> Result<Self> {
        let wide: Vec<u16> = std::ffi::OsStr::new(vhd_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut storage_type = VIRTUAL_STORAGE_TYPE {
            DeviceId: STORAGE_TYPE_DEVICE_VHD,
            VendorId: vendor_microsoft(),
        };

        let mut handle: HANDLE = INVALID_HANDLE_VALUE;
        let open_params = OPEN_VIRTUAL_DISK_PARAMETERS {
            Version: 1, // OPEN_VIRTUAL_DISK_VERSION_1
            Anonymous: unsafe { std::mem::zeroed() },
        };
        let rc = unsafe {
            OpenVirtualDisk(
                &mut storage_type,
                wide.as_ptr(),
                VIRTUAL_DISK_ACCESS_ATTACH_RO | VIRTUAL_DISK_ACCESS_GET_INFO,
                OPEN_VIRTUAL_DISK_FLAG_NONE,
                &open_params,
                &mut handle,
            )
        };
        if rc != 0 {
            return Err(PhoenixError::Disk(format!(
                "OpenVirtualDisk failed for {vhd_path} (Win32 error {rc})"
            )));
        }

        let attach_params = ATTACH_VIRTUAL_DISK_PARAMETERS {
            Version: 1, // ATTACH_VIRTUAL_DISK_VERSION_1
            Anonymous: unsafe { std::mem::zeroed() },
        };
        let rc = unsafe {
            AttachVirtualDisk(
                handle,
                std::ptr::null_mut(),
                ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY,
                0,
                &attach_params,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            unsafe { CloseHandle(handle) };
            return Err(PhoenixError::Disk(format!(
                "AttachVirtualDisk failed for {vhd_path} (Win32 error {rc}). Mounting requires \
                 Administrator."
            )));
        }
        Ok(Self { handle })
    }
}

impl Drop for AttachedDisk {
    fn drop(&mut self) {
        // Closing the handle detaches the disk (no PERMANENT_LIFETIME flag was
        // requested), so no explicit DetachVirtualDisk is needed.
        if self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

fn vendor_microsoft() -> GUID {
    // EC984AEC-A0F9-47E9-901F-71415A66345B
    GUID {
        data1: 0xEC98_4AEC,
        data2: 0xA0F9,
        data3: 0x47E9,
        data4: [0x90, 0x1F, 0x71, 0x41, 0x5A, 0x66, 0x34, 0x5B],
    }
}
