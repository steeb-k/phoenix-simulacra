//! Host↔guest file exchange over the HOST's native SMB server.
//!
//! No Samba, no VVFAT, no guest drivers: Windows already runs an SMB server on
//! port 445, and a slirp (user-mode NAT) guest reaches the host's loopback at
//! the gateway address `10.0.2.2`. So the whole feature is: share a folder
//! next to the image (`net share`, we're already elevated), and hand the user
//! one command to paste inside the guest:
//!
//! ```text
//! cmdkey /add:10.0.2.2 /user:HOSTNAME\user /pass
//! net use S: \\10.0.2.2\SimulacraShare /persistent:yes
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
///
/// Two steps, because `net use` cannot do both at once: `/savecred` and
/// `/persistent:yes` are mutually exclusive, so a mapping can be remembered
/// or its credential can be, never both.
///
/// `cmdkey` sidesteps that by writing the credential into Credential Manager
/// independently of any mapping — `/pass` with no value prompts for it once.
/// The mapping is then a plain persistent one, and reconnects after a guest
/// reboot resolve against the stored credential without prompting.
///
/// Both live in the guest's own profile, i.e. inside the session overlay —
/// discarded with the session, never written back to the backup image.
///
/// The single source of truth: `MapShare.cmd` runs exactly this, and the
/// Running view shows exactly this to copy.
pub fn guest_mount_command() -> String {
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "HOST".into());
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    format!(
        r"cmdkey /add:10.0.2.2 /user:{host}\{user} /pass && net use S: \\10.0.2.2\{SHARE_NAME} /persistent:yes"
    )
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
         echo.\r\n\
         echo Enter the password for the account you use on the host PC.\r\n\
         echo It is saved here so the drive comes back on its own after a\r\n\
         echo reboot - this VM's disk is scratch, discarded with the session.\r\n\
         echo.\r\n\
         net use S: /delete /y >nul 2>&1\r\n\
         {mount}\r\n\
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

    // Launch the CD's own guest-tools bundler and otherwise stay out of the
    // way. Only the bundler carries the SPICE vdagent (clipboard, guest
    // display auto-resize) — the loose virtio-win-gt-x64.msi does not.
    //
    // Deliberately does NOT install silently or install the guest agent
    // unconditionally. A `/quiet` run installs every component with defaults
    // where an interactive one can decide per device, and the bundler
    // already ships qemu-ga, so a second msiexec pass put two agent installs
    // back to back while driver staging was still settling. A hand install
    // works reliably and a scripted one black-screened, so the script stays
    // as close to a hand install as it can.
    //
    // The Windows Update lockdown DOES run first, by explicit request: on a
    // connected guest WU delivers a screen-blanking display driver within
    // moments of first login, so anything applied afterwards has already
    // lost the race. `DontSearchWindowsUpdate` and `SearchOrderConfig=0` are
    // the ones that stop a driver arriving during this very install;
    // `ExcludeWUDriversInQualityUpdate` and the AU policy cover later
    // updates. The services are belt-and-braces — Windows revives wuauserv
    // on its own, which is why the policy keys matter more than `sc config`.
    //
    // Fast Startup goes too. It makes shutdown a kernel hibernate rather
    // than a real shutdown, which leaves NTFS dirty in the overlay and makes
    // the guest skip hardware re-enumeration between boots — both bad for a
    // machine whose virtual hardware we change underneath it.
    let install = "@echo off\r\n\
        title Simulacra guest tools install\r\n\
        net session >nul 2>&1\r\n\
        if errorlevel 1 (\r\n\
        echo Requesting administrator rights...\r\n\
        powershell -NoProfile -Command \"Start-Process -FilePath '%~f0' -Verb RunAs\"\r\n\
        exit /b\r\n\
        )\r\n\
        \r\n\
        set VIODRV=\r\n\
        for %%d in (C D E F G H I J K L M N O P Q R S T U V W X Y Z) do (\r\n\
        if exist \"%%d:\\virtio-win-guest-tools.exe\" set VIODRV=%%d:\r\n\
        )\r\n\
        if \"%VIODRV%\"==\"\" (\r\n\
        echo Could not find the virtio-win driver CD. Is it attached?\r\n\
        pause\r\n\
        exit /b 1\r\n\
        )\r\n\
        echo Found the driver CD at %VIODRV%\r\n\
        \r\n\
        echo Blocking driver delivery via Windows Update...\r\n\
        reg add \"HKLM\\SOFTWARE\\Policies\\Microsoft\\Windows\\WindowsUpdate\" /v ExcludeWUDriversInQualityUpdate /t REG_DWORD /d 1 /f >nul\r\n\
        reg add \"HKLM\\SOFTWARE\\Policies\\Microsoft\\Windows\\DriverSearching\" /v DontSearchWindowsUpdate /t REG_DWORD /d 1 /f >nul\r\n\
        reg add \"HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\DriverSearching\" /v SearchOrderConfig /t REG_DWORD /d 0 /f >nul\r\n\
        reg add \"HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Device Metadata\" /v PreventDeviceMetadataFromNetwork /t REG_DWORD /d 1 /f >nul\r\n\
        \r\n\
        echo Disabling Windows Update...\r\n\
        reg add \"HKLM\\SOFTWARE\\Policies\\Microsoft\\Windows\\WindowsUpdate\\AU\" /v NoAutoUpdate /t REG_DWORD /d 1 /f >nul\r\n\
        reg add \"HKLM\\SOFTWARE\\Policies\\Microsoft\\Windows\\WindowsUpdate\" /v DoNotConnectToWindowsUpdateInternetLocations /t REG_DWORD /d 1 /f >nul\r\n\
        sc stop wuauserv >nul 2>&1\r\n\
        sc config wuauserv start= disabled >nul 2>&1\r\n\
        sc config UsoSvc start= disabled >nul 2>&1\r\n\
        sc config WaaSMedicSvc start= disabled >nul 2>&1\r\n\
        \r\n\
        echo Disabling Fast Startup...\r\n\
        reg add \"HKLM\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Power\" /v HiberbootEnabled /t REG_DWORD /d 0 /f >nul\r\n\
        powercfg /h off >nul 2>&1\r\n\
        \r\n\
        echo Starting the virtio-win guest tools installer. Click through it\r\n\
        echo as you would normally - this script only launches it.\r\n\
        \"%VIODRV%\\virtio-win-guest-tools.exe\"\r\n\
        set RC=%ERRORLEVEL%\r\n\
        if not \"%RC%\"==\"0\" if not \"%RC%\"==\"3010\" (\r\n\
        echo Guest tools installer returned %RC%.\r\n\
        pause\r\n\
        exit /b %RC%\r\n\
        )\r\n\
        \r\n\
        sc query QEMU-GA >nul 2>&1\r\n\
        if errorlevel 1 if exist \"%VIODRV%\\guest-agent\\qemu-ga-x86_64.msi\" (\r\n\
        echo Installing the QEMU guest agent...\r\n\
        msiexec /i \"%VIODRV%\\guest-agent\\qemu-ga-x86_64.msi\" /qn\r\n\
        )\r\n\
        \r\n\
        echo.\r\n\
        echo Done - reboot to finish. Keep the VM's network off unless you\r\n\
        echo need it: Windows Update replaces the display driver installed\r\n\
        echo here with one that blanks the screen.\r\n\
        pause\r\n";
    root.create_file("InstallGuestDrivers.cmd")
        .context("create InstallGuestDrivers.cmd")?
        .write_all(install.as_bytes())
        .context("write InstallGuestDrivers.cmd")?;

    let readme = format!(
        "This drive belongs to Phoenix Simulacra (running on the host).\r\n\
         \r\n\
         MapShare.cmd           - map the host's shared folder as drive S:\r\n\
         (the folder next to the backup image, \\\\10.0.2.2\\{SHARE_NAME}).\r\n\
         Anything you copy into S: appears on the host, and vice versa.\r\n\
         Needs the VM's networking switched on - the share rides the NAT.\r\n\
         \r\n\
         InstallGuestDrivers.cmd - install the virtio-win guest tools and\r\n\
         the QEMU guest agent from the attached driver CD, and block driver\r\n\
         delivery via Windows Update. Reboot afterwards.\r\n\
         \r\n\
         Leave the VM's network off unless you need it. Windows Update\r\n\
         replaces the CD's display driver with one that blanks the screen\r\n\
         on the next reboot; the CD's own driver is fine.\r\n"
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
        // A normal, remembered mapping — it should come back on its own
        // after a guest reboot, credential and all, rather than needing the
        // script again or prompting.
        assert!(cmd.contains("/persistent:yes"));
        assert!(!cmd.contains("/persistent:no"));
        // The credential is stored by cmdkey, NOT by /savecred — net use
        // refuses to combine /savecred with /persistent:yes, so pairing them
        // would break the mapping outright rather than degrade it.
        assert!(cmd.contains("cmdkey /add:10.0.2.2"));
        assert!(!cmd.contains("/savecred"));
        // The script must run exactly the command the UI offers to copy, so
        // the two can't drift apart.
        assert!(cmd.contains(&guest_mount_command()));

        // The driver script installs the CD's bundler (the only source of
        // the SPICE vdagent), never the loose driver-only MSI, and blocks
        // Windows Update's display driver BEFORE installing — WU otherwise
        // wins the race on a connected guest and blanks the next boot.
        let mut inst = String::new();
        fs.root_dir()
            .open_file("InstallGuestDrivers.cmd")
            .unwrap()
            .read_to_string(&mut inst)
            .unwrap();
        assert!(inst.contains("virtio-win-guest-tools.exe"));
        assert!(!inst.contains("virtio-win-gt-x64.msi"));
        // Never silently: a /quiet run installs every component with
        // defaults, where the interactive one decides per device.
        assert!(!inst.contains("/quiet"));
        // The bundler ships qemu-ga, so only install it if it is missing.
        assert!(inst.contains("sc query QEMU-GA"));
        // Fast Startup off: it makes shutdown a kernel hibernate, leaving
        // NTFS dirty in the overlay and skipping hardware re-enumeration.
        assert!(inst.contains("HiberbootEnabled"));
        // The driver-search blocks must land BEFORE the install — that is
        // the whole point of them; applied afterwards, Windows Update has
        // already won the race on a connected guest.
        // Anchored on the LAUNCH line specifically: the drive probe names
        // the same executable earlier, and matching that instead would make
        // this assertion pass no matter where the keys went.
        let gt = inst
            .find("\"%VIODRV%\\virtio-win-guest-tools.exe\"")
            .unwrap();
        for key in [
            "ExcludeWUDriversInQualityUpdate",
            "DontSearchWindowsUpdate",
            "SearchOrderConfig",
            "NoAutoUpdate",
        ] {
            let at = inst
                .find(key)
                .unwrap_or_else(|| panic!("{key} missing from the script"));
            assert!(at < gt, "{key} must be set before the guest-tools install");
        }
        drop(fs);
        std::fs::remove_file(&img).ok();
    }
}
