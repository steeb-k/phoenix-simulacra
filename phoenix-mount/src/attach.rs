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
    AttachVirtualDisk, CreateVirtualDisk, DetachVirtualDisk, GetVirtualDiskPhysicalPath,
    OpenVirtualDisk, ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER, ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY,
    ATTACH_VIRTUAL_DISK_PARAMETERS, CREATE_VIRTUAL_DISK_FLAG_NONE, CREATE_VIRTUAL_DISK_PARAMETERS,
    CREATE_VIRTUAL_DISK_VERSION_2, DETACH_VIRTUAL_DISK_FLAG_NONE, OPEN_VIRTUAL_DISK_FLAG_NONE,
    OPEN_VIRTUAL_DISK_PARAMETERS, VIRTUAL_DISK_ACCESS_ATTACH_RO, VIRTUAL_DISK_ACCESS_ATTACH_RW,
    VIRTUAL_DISK_ACCESS_GET_INFO, VIRTUAL_DISK_ACCESS_NONE, VIRTUAL_STORAGE_TYPE,
    VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
};

/// ERROR_SHARING_VIOLATION — the overlay file is still open (usually attached
/// from a previous run that didn't exit cleanly).
const ERROR_SHARING_VIOLATION: u32 = 32;

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
        Self::attach_readonly_opts(vhd_path, true)
    }

    /// Like [`attach_readonly`](Self::attach_readonly), but with
    /// `auto_drive_letters: false` the disk attaches with NO drive letters —
    /// the caller then assigns letters to just the volumes it wants exposed
    /// (see [`crate::letters`]).
    pub fn attach_readonly_opts(vhd_path: &str, auto_drive_letters: bool) -> Result<Self> {
        let wide: Vec<u16> = std::ffi::OsStr::new(vhd_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let storage_type = VIRTUAL_STORAGE_TYPE {
            DeviceId: STORAGE_TYPE_DEVICE_UNKNOWN,
            VendorId: vendor_unknown(),
        };

        let mut handle: HANDLE = INVALID_HANDLE_VALUE;
        let mut open_params: OPEN_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
        open_params.Version = 1; // OPEN_VIRTUAL_DISK_VERSION_1
                                 // Writing a union field is safe; only reads are unsafe.
        open_params.Anonymous.Version1.RWDepth = RW_DEPTH_DEFAULT;
        let rc = unsafe {
            OpenVirtualDisk(
                &storage_type,
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
        let mut flags = ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY;
        if !auto_drive_letters {
            flags |= ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER;
        }
        let rc = unsafe {
            AttachVirtualDisk(
                handle,
                std::ptr::null_mut(),
                flags,
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

    /// Open `vhd_path` and attach it **read-write** — the writable overlay's
    /// differencing child. `RWDepth = 1` opens only the top of the chain (the
    /// child) writable and any parent(s) read-only, which is the copy-on-write
    /// guarantee: the parent (our synthesized backup image) is never written.
    /// With `auto_drive_letters: false` the disk attaches with NO drive letters,
    /// so the caller assigns them selectively (see [`crate::letters`]).
    pub fn attach_readwrite_opts(vhd_path: &str, auto_drive_letters: bool) -> Result<Self> {
        let wide: Vec<u16> = std::ffi::OsStr::new(vhd_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let storage_type = VIRTUAL_STORAGE_TYPE {
            DeviceId: STORAGE_TYPE_DEVICE_UNKNOWN,
            VendorId: vendor_unknown(),
        };

        let mut handle: HANDLE = INVALID_HANDLE_VALUE;
        let mut open_params: OPEN_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
        open_params.Version = 1; // OPEN_VIRTUAL_DISK_VERSION_1
        open_params.Anonymous.Version1.RWDepth = RW_DEPTH_DEFAULT;
        let rc = unsafe {
            OpenVirtualDisk(
                &storage_type,
                wide.as_ptr(),
                VIRTUAL_DISK_ACCESS_ATTACH_RW | VIRTUAL_DISK_ACCESS_GET_INFO,
                OPEN_VIRTUAL_DISK_FLAG_NONE,
                &open_params,
                &mut handle,
            )
        };
        if rc != 0 {
            if rc == ERROR_SHARING_VIOLATION {
                return Err(PhoenixError::Disk(format!(
                    "the overlay {vhd_path} is still in use — it is most likely attached from a \
                     previous VM run that didn't exit cleanly. Close any running VM for this \
                     backup (or reboot) and try again."
                )));
            }
            return Err(PhoenixError::Disk(format!(
                "OpenVirtualDisk (read-write) failed for {vhd_path} (Win32 error {})",
                describe_vhd_error(rc)
            )));
        }

        let attach_params = ATTACH_VIRTUAL_DISK_PARAMETERS {
            Version: 1, // ATTACH_VIRTUAL_DISK_VERSION_1
            Anonymous: unsafe { std::mem::zeroed() },
        };
        // No READ_ONLY flag: this attach is writable.
        let mut flags: i32 = 0; // ATTACH_VIRTUAL_DISK_FLAG_NONE
        if !auto_drive_letters {
            flags |= ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER;
        }
        let rc = unsafe {
            AttachVirtualDisk(
                handle,
                std::ptr::null_mut(),
                flags,
                0,
                &attach_params,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            unsafe { CloseHandle(handle) };
            return Err(PhoenixError::Disk(format!(
                "AttachVirtualDisk (read-write) failed for {vhd_path} (Win32 error {}). Mounting \
                 requires Administrator.",
                describe_vhd_error(rc)
            )));
        }
        Ok(Self { handle })
    }

    /// Physical drive number (`N` in `\\.\PhysicalDriveN`) of the attached
    /// disk, for correlating its volumes via disk-extent queries.
    pub fn physical_drive_number(&self) -> Result<u32> {
        let mut buf = [0u16; 256];
        let mut size_bytes = (buf.len() * 2) as u32;
        let rc =
            unsafe { GetVirtualDiskPhysicalPath(self.handle, &mut size_bytes, buf.as_mut_ptr()) };
        if rc != 0 {
            return Err(PhoenixError::Disk(format!(
                "GetVirtualDiskPhysicalPath failed (Win32 error {rc})"
            )));
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let path = String::from_utf16_lossy(&buf[..len]);
        let digits: String = path.chars().filter(|c| c.is_ascii_digit()).collect();
        digits.parse().map_err(|_| {
            PhoenixError::Disk(format!(
                "unexpected physical path for attached disk: {path}"
            ))
        })
    }
}

impl AttachedDisk {
    /// Explicitly detach the disk and block until its physical device is gone,
    /// up to `timeout`.
    ///
    /// This MUST run before any in-process WinFsp serve backing the disk is
    /// stopped. Merely closing the handle (as `Drop` does) triggers an
    /// *asynchronous* detach; if the serve is then stopped while that detach is
    /// still draining the parent handle, the two race and deadlock in the
    /// storage driver — an unkillable process. So here we detach synchronously
    /// and poll until the device actually disappears while the serve's
    /// filesystem threads are still alive to service the final I/O.
    ///
    /// Returns `true` if the device was confirmed gone within `timeout`. A
    /// caller backing the disk with an in-process serve MUST only stop that
    /// serve on `true`: if this returns `false` the detach is still in flight,
    /// and stopping the serve then is exactly the deadlock — better to leave
    /// serve teardown to process exit (the OS reclaims both cleanly on death).
    #[must_use]
    pub fn detach_wait(&self, timeout: std::time::Duration) -> bool {
        // SAFETY: `handle` is a live virtual-disk handle for this call.
        unsafe {
            DetachVirtualDisk(self.handle, DETACH_VIRTUAL_DISK_FLAG_NONE, 0);
        }
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            // Once detached, the disk no longer resolves to a physical path.
            if self.physical_drive_number().is_err() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        tracing::warn!("virtual disk did not detach within {timeout:?}");
        false
    }
}

impl Drop for AttachedDisk {
    fn drop(&mut self) {
        // Closing the handle detaches the disk (no PERMANENT_LIFETIME flag was
        // requested). Callers that back the disk with an in-process WinFsp serve
        // must call `detach_wait` FIRST so the detach completes before the serve
        // stops; this is the fallback for the read-only / no-serve cases.
        if self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// Force `\\.\PhysicalDrive{drive_number}` OFFLINE (non-persistent). An offline
/// disk keeps no volumes mounted, so the host filesystem stack never touches
/// it, and Windows lifts the raw-sector write protection it applies over
/// mounted-volume extents — the state a VM's raw-passthrough disk needs when a
/// differencing child is attached read-write for a guest to boot from.
///
/// The change lasts only while the disk is attached (`Persist = 0`); when the
/// owning [`AttachedDisk`] drops and Windows auto-detaches, it is gone.
pub fn set_disk_offline(drive_number: u32) -> Result<()> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::System::Ioctl::{
        DISK_ATTRIBUTE_OFFLINE, IOCTL_DISK_SET_DISK_ATTRIBUTES, SET_DISK_ATTRIBUTES,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let path = format!(r"\\.\PhysicalDrive{drive_number}");
    let disk = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| PhoenixError::Disk(format!("open {path} to set it offline: {e}")))?;

    // SAFETY: all-zero is valid for this POD struct; we then set the header and
    // the offline attribute + mask. Persist=0 keeps the change attach-scoped.
    let mut attrs: SET_DISK_ATTRIBUTES = unsafe { std::mem::zeroed() };
    attrs.Version = std::mem::size_of::<SET_DISK_ATTRIBUTES>() as u32;
    attrs.Attributes = DISK_ATTRIBUTE_OFFLINE as u64;
    attrs.AttributesMask = DISK_ATTRIBUTE_OFFLINE as u64;

    let mut returned = 0u32;
    // SAFETY: the handle is open for the call; the in-buffer outlives it; the
    // out-buffer is unused for this IOCTL; `returned` receives the out length.
    let ok = unsafe {
        DeviceIoControl(
            disk.as_raw_handle() as _,
            IOCTL_DISK_SET_DISK_ATTRIBUTES,
            (&attrs as *const SET_DISK_ATTRIBUTES).cast(),
            std::mem::size_of::<SET_DISK_ATTRIBUTES>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(PhoenixError::Disk(format!(
            "IOCTL_DISK_SET_DISK_ATTRIBUTES(offline) failed for {path}: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
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

/// VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT — {ec984aec-a0f9-47e9-901f-71415a66345b}.
/// Differencing-disk creation names the parent's storage type explicitly.
fn vendor_microsoft() -> GUID {
    GUID {
        data1: 0xec98_4aec,
        data2: 0xa0f9,
        data3: 0x47e9,
        data4: [0x90, 0x1f, 0x71, 0x41, 0x5a, 0x66, 0x34, 0x5b],
    }
}

/// Create a differencing VHDX `child` whose parent is `parent`, via
/// `CreateVirtualDisk`. The child inherits size and sector geometry from the
/// parent (we leave those fields zero) and starts a few MiB, growing only with
/// writes — the ephemeral scratch layer of a writable overlay.
///
/// `parent` may be an ordinary file or one served by a user-mode filesystem
/// (WinFsp): Windows reads it through the same file API either way, so the
/// footprint-free on-demand parent works as a differencing parent unchanged.
pub fn create_differencing_vhdx(child: &str, parent: &str) -> Result<()> {
    let child_w: Vec<u16> = std::ffi::OsStr::new(child)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let parent_w: Vec<u16> = std::ffi::OsStr::new(parent)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let storage_type = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: vendor_microsoft(),
    };

    // SAFETY: a zeroed Version2 is "dynamic, provider defaults"; we then set only
    // the parent, which is what makes it a differencing disk. Every Version2
    // field is a pointer/int/GUID/POD for which all-zero is valid.
    let mut params: CREATE_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
    params.Version = CREATE_VIRTUAL_DISK_VERSION_2;
    params.Anonymous.Version2.ParentPath = parent_w.as_ptr();
    params.Anonymous.Version2.ParentVirtualStorageType = storage_type;

    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: pointers outlive the call; both wide paths are NUL-terminated; the
    // optional security-descriptor / overlapped pointers are null.
    let rc = unsafe {
        CreateVirtualDisk(
            &storage_type,
            child_w.as_ptr(),
            VIRTUAL_DISK_ACCESS_NONE,
            std::ptr::null_mut(),
            CREATE_VIRTUAL_DISK_FLAG_NONE,
            0,
            &params,
            std::ptr::null(),
            &mut handle,
        )
    };
    if rc != 0 {
        return Err(PhoenixError::Disk(format!(
            "CreateVirtualDisk (differencing) failed for {child} over parent {parent} \
             (Win32 error {})",
            describe_vhd_error(rc)
        )));
    }
    if !handle.is_null() {
        unsafe { CloseHandle(handle) };
    }
    Ok(())
}
