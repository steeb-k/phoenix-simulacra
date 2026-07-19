//! Host↔guest file exchange over the HOST's native SMB server.
//!
//! No Samba, no VVFAT, no guest drivers: Windows already runs an SMB server on
//! port 445, and a slirp (user-mode NAT) guest reaches the host's loopback at
//! the gateway address `10.0.2.2`. So the whole feature is: share a folder
//! next to the image (`net share`, we're already elevated), and hand the user
//! one command to paste inside the guest:
//!
//! ```text
//! net use S: \\10.0.2.2\SimulacraShare /user:HOSTNAME\user
//! ```
//!
//! The guest prompts for the host account's password (a Microsoft-account PC
//! accepts the Microsoft-account password). Mapping can't be automatic — we
//! can't reach into the guest before it boots — which is why the GUI surfaces
//! the command with a Copy button instead.
//!
//! One share at a time (we run one VM at a time): `ensure_share` re-points the
//! fixed share name at the current image's folder, and teardown removes it.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// `net.exe` is a console app; never flash a console from the GUI.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The fixed Windows share name. One VM (and thus one share) at a time.
pub const SHARE_NAME: &str = "SimulacraShare";

/// The default shared folder: `SimulacraShare` next to the backup image.
pub fn share_dir_for_backup(backup: &Path) -> PathBuf {
    backup
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(SHARE_NAME)
}

/// Create the folder (if needed) and point the Windows share at it, granting
/// the current user full access. Needs elevation — the VM path already runs
/// elevated. An existing share of the same name is re-pointed (`net share`
/// cannot move one in place, so it is dropped and recreated).
pub fn ensure_share(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    remove_share();

    let user = std::env::var("USERNAME").context("USERNAME not set")?;
    let spec = format!("{SHARE_NAME}={}", dir.display());
    let grant = format!("/GRANT:{user},FULL");
    let out = Command::new("net")
        .args(["share", &spec, &grant])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("run net share")?;
    if !out.status.success() {
        bail!(
            "net share failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    tracing::info!("shared {} as \\\\10.0.2.2\\{SHARE_NAME}", dir.display());
    Ok(())
}

/// Drop the share (the folder and its contents stay). Best-effort — a share
/// that never existed is fine.
pub fn remove_share() {
    let _ = Command::new("net")
        .args(["share", SHARE_NAME, "/delete", "/y"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

/// The command the user pastes INSIDE the guest to map the share. `10.0.2.2`
/// is slirp's gateway — the host's loopback as seen from the guest.
pub fn guest_mount_command() -> String {
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "HOST".into());
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    format!(r"net use S: \\10.0.2.2\{SHARE_NAME} /user:{host}\{user}")
}

/// Build the guest helper disk: a small FAT image (regenerated every boot)
/// holding `MapShare.cmd`, attached to the guest as an extra drive labelled
/// `SIMULACRA`. Clipboard sharing needs guest tools the guest may not have
/// yet, and hand-typing the mount command is miserable — double-clicking a
/// script on a drive that is simply *there* needs neither. A real image
/// rather than QEMU's VVFAT: writable VVFAT is corruption-prone and IDE disks
/// can't attach read-only, while guest writes to this image land in a
/// throwaway per-session file — safe by construction.
pub fn build_helper_disk(path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    const DISK_SIZE: u64 = 16 * 1024 * 1024;
    const SECTOR: u64 = 512;
    /// Partition start: 1 MiB, the standard alignment.
    const PART_START_LBA: u32 = 2048;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.set_len(DISK_SIZE).context("size helper disk")?;

    // A real MBR with one FAT16 partition. A FIXED disk (which `ide-hd`
    // presents) MUST carry a partition table: Windows mounts partitionless
    // "superfloppy" layouts only from removable media, and shows a fixed one
    // as an uninitialized disk with no drive letter (observed — the SIMULACRA
    // drive was invisible in the guest).
    let total_sectors = (DISK_SIZE / SECTOR) as u32;
    let part_sectors = total_sectors - PART_START_LBA;
    let mut mbr = [0u8; 512];
    let entry = &mut mbr[0x1BE..0x1CE];
    entry[0] = 0x00; // not bootable — this disk is never booted
    entry[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]); // CHS sentinel: use LBA
    entry[4] = 0x0E; // FAT16 (LBA)
    entry[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    entry[8..12].copy_from_slice(&PART_START_LBA.to_le_bytes());
    entry[12..16].copy_from_slice(&part_sectors.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    file.seek(SeekFrom::Start(0)).context("seek to MBR")?;
    file.write_all(&mbr).context("write MBR")?;

    // Format the partition region (not the whole disk) as FAT.
    let mut part = fscommon::StreamSlice::new(file, PART_START_LBA as u64 * SECTOR, DISK_SIZE)
        .context("slice helper partition")?;
    fatfs::format_volume(
        &mut part,
        fatfs::FormatVolumeOptions::new().volume_label(*b"SIMULACRA  "),
    )
    .context("format helper disk")?;
    let fs = fatfs::FileSystem::new(part, fatfs::FsOptions::new())
        .context("open helper disk filesystem")?;
    let root = fs.root_dir();

    // Batch files want CRLF line endings.
    let mount = guest_mount_command();
    let cmd = format!(
        "@echo off\r\n\
         title Simulacra shared folder\r\n\
         echo Mapping \\\\10.0.2.2\\{SHARE_NAME} as S: ...\r\n\
         echo (Sign in with the account password you use on the host PC.)\r\n\
         echo.\r\n\
         net use S: /delete /y >nul 2>&1\r\n\
         {mount} /persistent:no\r\n\
         if errorlevel 1 (\r\n\
         echo.\r\n\
         echo Mapping failed - is the shared folder enabled on the host?\r\n\
         pause\r\n\
         exit /b 1\r\n\
         )\r\n\
         start explorer S:\\\r\n\
         echo Done - the shared folder is drive S:\r\n\
         timeout /t 3 >nul\r\n"
    );
    root.create_file("MapShare.cmd")
        .context("create MapShare.cmd")?
        .write_all(cmd.as_bytes())
        .context("write MapShare.cmd")?;

    let readme = format!(
        "This drive belongs to Phoenix Simulacra (running on the host).\r\n\
         \r\n\
         Run MapShare.cmd to map the host's shared folder as drive S:\r\n\
         (the folder next to the backup image, \\\\10.0.2.2\\{SHARE_NAME}).\r\n\
         \r\n\
         Anything you copy into S: appears on the host, and vice versa.\r\n"
    );
    root.create_file("README.txt")
        .context("create README.txt")?
        .write_all(readme.as_bytes())
        .context("write README.txt")?;

    tracing::info!("built guest helper disk at {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn helper_disk_builds_and_reads_back() {
        let dir = std::env::temp_dir().join("phnx-share-helper-test");
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("helper.img");
        build_helper_disk(&img).unwrap();

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&img)
            .unwrap();
        // The MBR is what makes Windows mount a FIXED disk at all: one FAT16
        // (LBA) partition at 1 MiB, boot signature present.
        let mut mbr = [0u8; 512];
        file.read_exact(&mut mbr).unwrap();
        assert_eq!(&mbr[510..512], &[0x55, 0xAA]);
        assert_eq!(mbr[0x1BE + 4], 0x0E);
        assert_eq!(&mbr[0x1BE + 8..0x1BE + 12], &2048u32.to_le_bytes());

        let part =
            fscommon::StreamSlice::new(file, 2048 * 512, 16 * 1024 * 1024).unwrap();
        let fs = fatfs::FileSystem::new(part, fatfs::FsOptions::new()).unwrap();
        assert_eq!(fs.volume_label(), "SIMULACRA");
        let mut cmd = String::new();
        fs.root_dir()
            .open_file("MapShare.cmd")
            .unwrap()
            .read_to_string(&mut cmd)
            .unwrap();
        assert!(cmd.contains(r"\\10.0.2.2\SimulacraShare"));
        assert!(cmd.contains("net use S:"));
        drop(fs);
        std::fs::remove_file(&img).ok();
    }
}
