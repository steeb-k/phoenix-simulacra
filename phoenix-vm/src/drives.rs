//! Enumerate fixed drive letters with their free space — for the GUI's
//! scratch-drive picker (where a VM session's growing overlay should live).

/// One mounted drive and how much room it has.
#[derive(Debug, Clone)]
pub struct DriveInfo {
    pub letter: char,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

impl DriveInfo {
    /// `"D:  1.4 TB free of 1.9 TB"`.
    pub fn label(&self) -> String {
        format!(
            "{}:  {} free of {}",
            self.letter,
            human_bytes(self.free_bytes),
            human_bytes(self.total_bytes)
        )
    }
}

fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Every ready drive letter with usable space, in A→Z order. Skips drives that
/// aren't ready (empty optical/card readers), which `GetDiskFreeSpaceExW`
/// reports by failing.
pub fn list_drives() -> Vec<DriveInfo> {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetLogicalDrives};

    let mut out = Vec::new();
    // Bitmask: bit 0 = A:, bit 1 = B:, ...
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut free_avail = 0u64;
        let mut total = 0u64;
        // SAFETY: `root` is a NUL-terminated wide string; the out pointers are
        // valid u64 locations; the third (total-free) arg is optional/null.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                root.as_ptr(),
                &mut free_avail,
                &mut total,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 && total > 0 {
            out.push(DriveInfo {
                letter,
                free_bytes: free_avail,
                total_bytes: total,
            });
        }
    }
    out
}
