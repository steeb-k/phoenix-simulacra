//! Selective drive-letter exposure for a mounted backup disk.
//!
//! The disk is attached with `NO_DRIVE_LETTER`, so nothing surfaces in
//! Explorer until we explicitly assign letters — and we assign them only to
//! the volumes whose partition the user selected. Volumes are correlated to
//! the backup's partitions by their starting byte offset on the attached
//! disk, which the mount itself laid out ([`PartitionSpan::disk_offset`]),
//! so the match is exact.

use std::time::{Duration, Instant};

use phoenix_core::error::{PhoenixError, Result};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DeleteVolumeMountPointW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    GetLogicalDrives, SetVolumeMountPointW, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::VOLUME_DISK_EXTENTS;
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::chunkstore::PartitionSpan;

/// One partition of an active mount as exposed to the user.
#[derive(Debug, Clone)]
pub struct MountedVolume {
    pub partition_index: u32,
    /// Assigned drive letter; `None` when the partition has no mountable
    /// volume (e.g. Microsoft Reserved, or a filesystem Windows can't read).
    pub drive_letter: Option<char>,
}

/// How long to wait for the mount manager to surface the attached disk's
/// volume devices (they arrive asynchronously after `AttachVirtualDisk`).
const VOLUME_WAIT: Duration = Duration::from_secs(15);
/// Consecutive unchanged polls after which the volume set counts as settled.
const SETTLE_POLLS: u32 = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(200);

struct VolumeOnDisk {
    /// `\\?\Volume{GUID}\` (trailing backslash included).
    guid_path: Vec<u16>,
    start_offset: u64,
}

/// Assign drive letters to `selected` partitions of the freshly attached
/// disk `disk_number`, matching volumes to `spans` by starting offset.
/// Returns one entry per selected partition (letter `None` when Windows
/// created no volume for it) plus the letters to remove on unmount.
pub fn expose_selected(
    disk_number: u32,
    spans: &[PartitionSpan],
    selected: &[u32],
) -> Result<(Vec<MountedVolume>, Vec<char>)> {
    let volumes = wait_for_volumes(disk_number);
    let mut out = Vec::new();
    let mut letters = Vec::new();
    for span in spans {
        if !selected.contains(&span.partition_index) {
            continue;
        }
        let vol = volumes.iter().find(|v| v.start_offset == span.disk_offset);
        let letter = match vol {
            Some(v) => match assign_free_letter(&v.guid_path) {
                Ok(l) => Some(l),
                Err(e) => {
                    tracing::warn!(
                        partition = span.partition_index,
                        "could not assign a drive letter: {e}"
                    );
                    None
                }
            },
            None => None,
        };
        if let Some(l) = letter {
            letters.push(l);
        }
        out.push(MountedVolume {
            partition_index: span.partition_index,
            drive_letter: letter,
        });
    }
    Ok((out, letters))
}

/// Remove previously assigned letters (called on unmount, before detach).
pub fn remove_letters(letters: &[char]) {
    for &l in letters {
        let path: Vec<u16> = format!("{l}:\\").encode_utf16().chain([0]).collect();
        unsafe { DeleteVolumeMountPointW(path.as_ptr()) };
    }
}

/// Poll the system volume list until the set of volumes on `disk_number`
/// stops changing (or the timeout lapses). Volume devices for a fresh attach
/// arrive asynchronously, one per recognized partition.
fn wait_for_volumes(disk_number: u32) -> Vec<VolumeOnDisk> {
    let deadline = Instant::now() + VOLUME_WAIT;
    let mut last_count = 0usize;
    let mut stable = 0u32;
    loop {
        let vols = volumes_on_disk(disk_number);
        if !vols.is_empty() && vols.len() == last_count {
            stable += 1;
            if stable >= SETTLE_POLLS {
                return vols;
            }
        } else {
            stable = 0;
            last_count = vols.len();
        }
        if Instant::now() >= deadline {
            return vols;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Enumerate all `\\?\Volume{GUID}\` volumes whose single extent lives on
/// `disk_number`, with the extent's starting byte offset.
fn volumes_on_disk(disk_number: u32) -> Vec<VolumeOnDisk> {
    let mut out = Vec::new();
    let mut name = [0u16; 128];
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return out;
    }
    loop {
        if let Some((disk, offset)) = volume_disk_extent(&name) {
            if disk == disk_number {
                let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                out.push(VolumeOnDisk {
                    guid_path: name[..=len].to_vec(), // include the NUL
                    start_offset: offset,
                });
            }
        }
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    unsafe { FindVolumeClose(find) };
    out
}

/// Disk number + starting byte offset of a volume's first extent. `None` for
/// volumes that span disks or reject the query (neither happens for the
/// simple single-extent volumes of an attached backup VHD).
fn volume_disk_extent(guid_path: &[u16]) -> Option<(u32, u64)> {
    // CreateFileW wants the volume path WITHOUT the trailing backslash.
    let len = guid_path
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(guid_path.len());
    let mut no_slash: Vec<u16> = guid_path[..len].to_vec();
    if no_slash.last() == Some(&('\\' as u16)) {
        no_slash.pop();
    }
    no_slash.push(0);

    let handle: HANDLE = unsafe {
        CreateFileW(
            no_slash.as_ptr(),
            0, // metadata queries only
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    // Room for a few extents; our volumes have exactly one.
    let mut buf = [0u8; 512];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            std::ptr::null(),
            0,
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    let extents = unsafe { &*(buf.as_ptr() as *const VOLUME_DISK_EXTENTS) };
    if extents.NumberOfDiskExtents != 1 {
        return None;
    }
    let ext = &extents.Extents[0];
    Some((ext.DiskNumber, ext.StartingOffset as u64))
}

/// Assign the first free drive letter (D: onward) to `guid_path`
/// (`\\?\Volume{GUID}\`, NUL-terminated). Requires Administrator.
fn assign_free_letter(guid_path: &[u16]) -> Result<char> {
    let in_use = unsafe { GetLogicalDrives() };
    for i in 3..26u32 {
        // start at D:
        if in_use & (1 << i) != 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let mount_point: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        if unsafe { SetVolumeMountPointW(mount_point.as_ptr(), guid_path.as_ptr()) } != 0 {
            return Ok(letter);
        }
        // Raced with another assignment or policy refusal — try the next one.
    }
    Err(PhoenixError::Disk(
        "no free drive letter available to expose the mounted volume".into(),
    ))
}
