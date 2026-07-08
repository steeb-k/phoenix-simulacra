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

// VIRTUAL_STORAGE_TYPE_DEVICE_UNKNOWN = 0 — let the API detect VHD vs VHDX
// from the file contents rather than asserting a device type ourselves.
const STORAGE_TYPE_DEVICE_UNKNOWN: u32 = 0;
// OPEN_VIRTUAL_DISK_RW_DEPTH_DEFAULT.
const RW_DEPTH_DEFAULT: u32 = 1;

/// Decode a virtual-disk API return code. Codes in the FACILITY_VHD range
/// (0xC03Axxxx) mean the *file* was rejected as a virtual disk (bad footer,
/// checksum, or unsupported format); other codes are usually access/permission.
fn describe_vhd_error(rc: u32) -> String {
    if (0xC03A_0000..0xC03B_0000).contains(&rc) {
        format!("0x{rc:08X} (VHD format error — the synthesized disk image was rejected)")
    } else {
        format!("{rc} (0x{rc:08X})")
    }
}

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
            DeviceId: STORAGE_TYPE_DEVICE_UNKNOWN,
            VendorId: vendor_unknown(),
        };

        let mut handle: HANDLE = INVALID_HANDLE_VALUE;
        let mut open_params: OPEN_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
        open_params.Version = 1; // OPEN_VIRTUAL_DISK_VERSION_1
        unsafe {
            open_params.Anonymous.Version1.RWDepth = RW_DEPTH_DEFAULT;
        }
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
                "OpenVirtualDisk failed for {vhd_path} (Win32 error {})",
                describe_vhd_error(rc)
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
                "AttachVirtualDisk failed for {vhd_path} (Win32 error {}). Mounting requires \
                 Administrator.",
                describe_vhd_error(rc)
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

/// VIRTUAL_STORAGE_TYPE_VENDOR_UNKNOWN — the all-zero GUID, paired with
/// DEVICE_UNKNOWN so the API detects the format from the file.
fn vendor_unknown() -> GUID {
    GUID {
        data1: 0,
        data2: 0,
        data3: 0,
        data4: [0; 8],
    }
}
