//! Boot repair: make a Windows installation on a disk bootable again by
//! rebuilding its boot files with the Windows-native tools (`bcdboot`,
//! `bootsect`) instead of hand-editing BCD hives.
//!
//! Used two ways: standalone from the Boot Repair page/CLI (pick an install,
//! repair its disk), and as an opt-in post-pass after a clone/restore whose
//! target carries a Windows installation ([`repair_target_disk`]).
//!
//! The intent is to stay **drive-local**: the boot files and BCD written are
//! those on the target disk, reached with `bcdboot /s <target ESP>`, and a
//! repaired drive is expected to boot on another machine through the standard
//! on-disk path (`\EFI\Microsoft\Boot\bootmgfw.efi`) rather than through an
//! NVRAM entry this machine happens to hold.
//!
//! This used to also pass `/nofirmwaresync` to make that guarantee explicit.
//! That option made `bcdboot` fail outright on the UEFI path, so it is gone —
//! which means firmware-entry behaviour is now whatever `bcdboot` does by
//! default for the `/s` target. Repairing a secondary drive on a UEFI machine
//! may therefore touch this machine's NVRAM boot entries. If that needs to be
//! prevented again it has to be done another way, not with that flag.
//!
//! GPT disks get the UEFI treatment (rebuild BCD + boot files on the EFI
//! System Partition); MBR disks get the legacy one (Windows boot code in the
//! MBR and the partition boot record via `bootsect /nt60 /mbr`, active flag
//! on the system partition, BCD via `bcdboot /f BIOS`).

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use phoenix_core::disk::{enumerate_disks, is_efi_system_partition, DiskInfo, PartitionInfo};
use phoenix_core::error::{PhoenixError, Result};
use tracing::{info, warn};
use windows_sys::Win32::Storage::FileSystem::{
    DeleteVolumeMountPointW, GetFileVersionInfoSizeW, GetFileVersionInfoW, GetLogicalDrives,
    SetVolumeMountPointW, VerQueryValueW,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A Windows installation found on some partition: a volume with
/// `\Windows\System32\ntoskrnl.exe` and a `SYSTEM` registry hive.
#[derive(Debug, Clone)]
pub struct WindowsInstall {
    pub disk_index: u32,
    pub partition_index: u32,
    pub drive_letter: Option<char>,
    pub volume_label: Option<String>,
    /// Volume device path exactly as enumerated (`\\.\X:` for lettered
    /// volumes, `\\?\Volume{GUID}` — no trailing backslash — otherwise).
    pub volume_path: String,
    /// e.g. "Windows 11 (build 26100)", from ntoskrnl.exe's version resource.
    pub version: Option<String>,
}

impl WindowsInstall {
    /// One-line human label for pickers and reports.
    pub fn display(&self) -> String {
        let what = self.version.as_deref().unwrap_or("Windows");
        let mut place = format!("disk {}, partition {}", self.disk_index, self.partition_index);
        if let Some(l) = self.drive_letter {
            place = format!("{l}: on {place}");
        }
        match &self.volume_label {
            Some(label) if !label.is_empty() => format!("{what} — {place} ({label})"),
            _ => format!("{what} — {place}"),
        }
    }
}

/// What kind of boot environment the repair will rebuild, decided by the
/// disk's partition style.
#[derive(Debug, Clone)]
pub enum BootScheme {
    /// GPT: rebuild the UEFI boot files + BCD on the EFI System Partition.
    UefiGpt { esp_partition: u32 },
    /// MBR: Windows boot code in the MBR + partition boot record, active
    /// flag on the system partition, BCD rebuilt with `/f BIOS`.
    BiosMbr {
        system_partition: u32,
        /// True when the system partition is not currently marked active and
        /// the repair will set the flag (clearing it from any other entry).
        needs_active_flag: bool,
    },
}

/// A fully resolved repair: which install, which disk, which scheme, and a
/// human-readable action list for confirmation UIs. Producing the plan is
/// read-only; nothing is modified until [`execute_boot_repair`].
#[derive(Debug, Clone)]
pub struct BootRepairPlan {
    pub disk_index: u32,
    pub install: WindowsInstall,
    pub scheme: BootScheme,
    pub actions: Vec<String>,
}

/// What a repair actually did, one line per action, for the completion UI.
#[derive(Debug, Clone, Default)]
pub struct BootRepairReport {
    pub actions: Vec<String>,
}

impl BootRepairReport {
    fn push(&mut self, line: impl Into<String>) {
        let line = line.into();
        info!("boot repair: {line}");
        self.actions.push(line);
    }
}

/// Scan the given disks for Windows installations. Probes every partition
/// that has a volume device — deliberately not filtered by detected
/// filesystem kind, since MBR enumeration leaves `fs_kind` unrefined and the
/// metadata probe is cheap either way.
pub fn detect_windows_installs(disks: &[DiskInfo]) -> Vec<WindowsInstall> {
    let mut out = Vec::new();
    for disk in disks {
        for part in &disk.partitions {
            let Some(vp) = part.volume_path.as_deref() else {
                continue;
            };
            let sys32 = volume_root(vp).join("Windows").join("System32");
            // Both the kernel and the SYSTEM hive must be present: a lone
            // Windows directory also exists on install media and in
            // Windows.old-style leftovers pruned down to skeletons.
            if !sys32.join("ntoskrnl.exe").is_file()
                || !sys32.join("config").join("SYSTEM").is_file()
            {
                continue;
            }
            out.push(WindowsInstall {
                disk_index: disk.index,
                partition_index: part.index,
                drive_letter: part.drive_letter,
                volume_label: part.volume_label.clone(),
                volume_path: vp.to_string(),
                version: kernel_version(&sys32.join("ntoskrnl.exe")),
            });
        }
    }
    out
}

/// The disk Windows is currently running from, if it is in `disks` — used by
/// UIs to warn before rewriting the live machine's own boot files.
pub fn system_disk_index(disks: &[DiskInfo]) -> Option<u32> {
    let sys = std::env::var("SystemDrive").ok()?;
    let letter = sys.chars().next()?.to_ascii_uppercase();
    disks
        .iter()
        .find(|d| d.partitions.iter().any(|p| p.drive_letter == Some(letter)))
        .map(|d| d.index)
}

/// Resolve how `install` on `disk` would be repaired. Read-only (the MBR is
/// read to learn the current active flag, nothing is written).
pub fn plan_boot_repair(disk: &DiskInfo, install: &WindowsInstall) -> Result<BootRepairPlan> {
    if install.disk_index != disk.index {
        return Err(PhoenixError::Disk(format!(
            "install is on disk {}, not disk {}",
            install.disk_index, disk.index
        )));
    }
    let mut actions = Vec::new();
    let scheme = if disk.is_gpt {
        let esp = disk
            .partitions
            .iter()
            .find(|p| is_efi_system_partition(&p.type_guid))
            .ok_or_else(|| {
                PhoenixError::Disk(format!(
                    "disk {} has no EFI System Partition — a GPT disk cannot boot without one, \
                     and boot repair does not create partitions",
                    disk.index
                ))
            })?;
        actions.push(format!(
            "Clear the existing boot configuration on partition {} (EFI System Partition), \
             keeping a .bak copy, then rebuild the UEFI boot files and BCD with bcdboot, \
             pointing at {}",
            esp.index,
            install.display()
        ));
        actions.push(
            "Write the boot files to the target disk's own EFI System Partition, so the drive \
             boots via the standard \\EFI\\Microsoft path on any machine. On a UEFI PC, bcdboot \
             may also add this drive to this PC's firmware boot entries"
                .into(),
        );
        BootScheme::UefiGpt {
            esp_partition: esp.index,
        }
    } else {
        let primaries = read_mbr_primaries(&disk.path, disk.sector_size)?;
        let install_part = disk
            .partitions
            .iter()
            .find(|p| p.index == install.partition_index)
            .ok_or_else(|| PhoenixError::Disk("install partition not found on disk".into()))?;
        // Prefer an existing active primary that carries a mountable volume
        // (the classic "System Reserved" layout); otherwise boot from the
        // Windows partition itself — which must then be a primary, since
        // neither the MBR active flag nor NT boot code can target a logical
        // partition inside an extended container.
        let active = primaries
            .iter()
            .find(|e| e.boot)
            .and_then(|e| partition_at_offset(disk, e.start_offset(disk.sector_size)))
            .filter(|p| p.volume_path.is_some());
        let system = match active {
            Some(p) => p,
            None => {
                if !primaries
                    .iter()
                    .any(|e| e.start_offset(disk.sector_size) == install_part.offset_bytes)
                {
                    return Err(PhoenixError::Disk(format!(
                        "the Windows partition on disk {} is a logical partition; an MBR disk \
                         can only boot from a primary partition",
                        disk.index
                    )));
                }
                install_part
            }
        };
        let needs_active_flag = !primaries
            .iter()
            .any(|e| e.boot && e.start_offset(disk.sector_size) == system.offset_bytes);
        if needs_active_flag {
            actions.push(format!(
                "Mark partition {} active in the MBR partition table",
                system.index
            ));
        }
        actions.push(format!(
            "Write the Windows MBR and partition boot code with bootsect /nt60 /mbr (partition {})",
            system.index
        ));
        actions.push(format!(
            "Clear the existing boot configuration on partition {}, keeping a .bak copy, then \
             rebuild the BCD with bcdboot /f BIOS, pointing at {}",
            system.index,
            install.display()
        ));
        BootScheme::BiosMbr {
            system_partition: system.index,
            needs_active_flag,
        }
    };
    Ok(BootRepairPlan {
        disk_index: disk.index,
        install: install.clone(),
        scheme,
        actions,
    })
}

/// Carry out a plan produced by [`plan_boot_repair`]. Assigns temporary
/// drive letters where the involved volumes have none (removed again before
/// returning), then shells out to the System32 tools.
pub fn execute_boot_repair(disk: &DiskInfo, plan: &BootRepairPlan) -> Result<BootRepairReport> {
    let mut report = BootRepairReport::default();
    let install_part = disk
        .partitions
        .iter()
        .find(|p| p.index == plan.install.partition_index)
        .ok_or_else(|| PhoenixError::Disk("install partition not found on disk".into()))?;
    let (src_letter, _src_guard) = ensure_letter(install_part, &mut report)?;
    let source = format!("{src_letter}:\\Windows");

    match &plan.scheme {
        BootScheme::UefiGpt { esp_partition } => {
            let esp = disk
                .partitions
                .iter()
                .find(|p| p.index == *esp_partition)
                .ok_or_else(|| PhoenixError::Disk("ESP not found on disk".into()))?;
            let (esp_letter, _esp_guard) = ensure_letter(esp, &mut report)?;
            clear_existing_bcd(
                &PathBuf::from(format!("{esp_letter}:\\EFI\\Microsoft\\Boot")),
                &mut report,
            )?;
            run_tool(
                "bcdboot",
                &[&source, "/s", &format!("{esp_letter}:"), "/f", "UEFI"],
                &mut report,
            )?;
        }
        BootScheme::BiosMbr {
            system_partition,
            needs_active_flag,
        } => {
            let system = disk
                .partitions
                .iter()
                .find(|p| p.index == *system_partition)
                .ok_or_else(|| PhoenixError::Disk("system partition not found on disk".into()))?;
            if *needs_active_flag {
                set_mbr_active(&disk.path, disk.sector_size, system.offset_bytes)?;
                report.push(format!(
                    "Marked partition {} active in the MBR",
                    system.index
                ));
            }
            let (sys_letter, _sys_guard) = ensure_letter(system, &mut report)?;
            // /mbr additionally refreshes the *disk's* MBR boot code — the
            // 440-byte loader, not the partition table, which bootsect
            // leaves alone.
            run_tool(
                "bootsect",
                &["/nt60", &format!("{sys_letter}:"), "/mbr"],
                &mut report,
            )?;
            clear_existing_bcd(&PathBuf::from(format!("{sys_letter}:\\Boot")), &mut report)?;
            run_tool(
                "bcdboot",
                &[&source, "/s", &format!("{sys_letter}:"), "/f", "BIOS"],
                &mut report,
            )?;
        }
    }
    Ok(report)
}

/// Outcome of the post-clone/restore repair pass.
#[derive(Debug, Clone)]
pub enum PostRepairOutcome {
    /// A Windows install was found on the target and its disk was repaired.
    Repaired {
        install: String,
        report: BootRepairReport,
    },
    /// The target carries no detectable Windows installation; nothing was
    /// modified. Not an error — the checkbox is offered on layouts that
    /// merely *look* bootable.
    NoWindowsFound,
}

/// [`PostRepairOutcome`] flattened for embedding in a clone/restore summary:
/// a boot-repair failure must not fail the operation that carried it (the
/// data on the target is already written and valid), so the error becomes a
/// reportable variant instead of an `Err`.
#[derive(Debug, Clone)]
pub enum BootRepairStatus {
    Repaired { install: String, actions: Vec<String> },
    NoWindowsFound,
    Failed(String),
}

impl BootRepairStatus {
    /// Multi-line human summary for CLI output and completion dialogs.
    pub fn describe(&self) -> String {
        match self {
            BootRepairStatus::Repaired { install, actions } => {
                let mut s = format!("Boot repair completed for {install}:");
                for a in actions {
                    s.push_str("\n  • ");
                    s.push_str(a);
                }
                s
            }
            BootRepairStatus::NoWindowsFound => {
                "Boot repair skipped: no Windows installation detected on the target disk".into()
            }
            BootRepairStatus::Failed(e) => format!(
                "Boot repair failed (the copied data itself is intact): {e}\n\
                 The Boot Repair page can retry once the cause is fixed."
            ),
        }
    }
}

/// Never-failing wrapper around [`repair_target_disk`] for the clone/restore
/// engines.
pub fn repair_target_disk_status(disk_index: u32) -> BootRepairStatus {
    match repair_target_disk(disk_index) {
        Ok(PostRepairOutcome::Repaired { install, report }) => BootRepairStatus::Repaired {
            install,
            actions: report.actions,
        },
        Ok(PostRepairOutcome::NoWindowsFound) => BootRepairStatus::NoWindowsFound,
        Err(e) => {
            warn!("post-operation boot repair failed: {e}");
            BootRepairStatus::Failed(e.to_string())
        }
    }
}

/// Post-clone/restore entry point: wait for the freshly written disk's
/// volumes to surface, find a Windows install on it, and repair its boot
/// environment. Volume devices arrive asynchronously after
/// `bring_disk_online`, hence the settle loop.
pub fn repair_target_disk(disk_index: u32) -> Result<PostRepairOutcome> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let disks = enumerate_disks()?;
        let disk = disks
            .iter()
            .find(|d| d.index == disk_index)
            .ok_or_else(|| {
                PhoenixError::Disk(format!("disk {disk_index} not found for boot repair"))
            })?;
        let installs = detect_windows_installs(std::slice::from_ref(disk));
        if let Some(install) = installs.first() {
            if installs.len() > 1 {
                warn!(
                    "disk {} carries {} Windows installations; repairing the boot files for \
                     the first ({}) — use the Boot Repair page to target another",
                    disk_index,
                    installs.len(),
                    install.display()
                );
            }
            let plan = plan_boot_repair(disk, install)?;
            let report = execute_boot_repair(disk, &plan)?;
            return Ok(PostRepairOutcome::Repaired {
                install: install.display(),
                report,
            });
        }
        // Every partition that can surface a volume has surfaced one and none
        // holds Windows — the answer won't change, don't sit out the full
        // settle window (a data-disk clone would otherwise stall here for the
        // whole timeout). MSR partitions never get a volume device; anything
        // else still volume-less (RAW, BitLocker-locked) might just not have
        // arrived yet, so keep waiting for those.
        let settled = disk.partitions.iter().all(|p| {
            p.volume_path.is_some() || p.fs_kind == phoenix_core::disk::FilesystemKind::Msr
        });
        if settled || Instant::now() >= deadline {
            return Ok(PostRepairOutcome::NoWindowsFound);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------
// helpers

/// Turn an enumerated volume device path into a root usable with std::fs:
/// `\\.\X:` → `X:\`, `\\?\Volume{...}` → `\\?\Volume{...}\`.
fn volume_root(volume_path: &str) -> PathBuf {
    if let Some(rest) = volume_path.strip_prefix(r"\\.\") {
        if rest.len() == 2 && rest.ends_with(':') {
            return PathBuf::from(format!("{rest}\\"));
        }
    }
    PathBuf::from(format!("{}\\", volume_path.trim_end_matches('\\')))
}

/// Removes a temporarily assigned drive letter on drop.
struct TempLetter {
    letter: char,
}

impl Drop for TempLetter {
    fn drop(&mut self) {
        let path: Vec<u16> = format!("{}:\\", self.letter)
            .encode_utf16()
            .chain([0])
            .collect();
        unsafe { DeleteVolumeMountPointW(path.as_ptr()) };
    }
}

/// The partition's drive letter, assigning a temporary one (D:–Z:, first
/// free) when it has none. The guard removes the temporary letter when the
/// repair finishes, success or not.
fn ensure_letter(
    part: &PartitionInfo,
    report: &mut BootRepairReport,
) -> Result<(char, Option<TempLetter>)> {
    if let Some(l) = part.drive_letter {
        return Ok((l, None));
    }
    let vp = part.volume_path.as_deref().ok_or_else(|| {
        PhoenixError::Disk(format!(
            "partition {} has no volume device to assign a drive letter to (unformatted or \
             unrecognized filesystem)",
            part.index
        ))
    })?;
    // SetVolumeMountPointW wants `\\?\Volume{GUID}\` with the trailing
    // backslash; enumeration hands the path without it.
    let guid_path: Vec<u16> = format!("{}\\", vp.trim_end_matches('\\'))
        .encode_utf16()
        .chain([0])
        .collect();
    let in_use = unsafe { GetLogicalDrives() };
    for i in 3..26u32 {
        if in_use & (1 << i) != 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let mount_point: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        if unsafe { SetVolumeMountPointW(mount_point.as_ptr(), guid_path.as_ptr()) } != 0 {
            report.push(format!(
                "Assigned temporary drive letter {letter}: to partition {}",
                part.index
            ));
            return Ok((letter, Some(TempLetter { letter })));
        }
    }
    Err(PhoenixError::Disk(format!(
        "no free drive letter available to expose partition {} for repair",
        part.index
    )))
}

/// Run a System32 tool, hidden-console, and fold its outcome into the
/// report. The absolute path avoids PATH lookups; both tools ship with
/// Windows (and with WinPE) on every supported version.
/// Move an existing BCD store aside so `bcdboot` builds a fresh one.
///
/// `bcdboot` *merges* into a store that is already there rather than replacing
/// it, so a repair run against a disk carrying a stale or broken BCD inherits
/// exactly the entries that made it unbootable — the classic symptom being a
/// boot menu still listing an installation that no longer exists, or pointing
/// at a partition that moved. Clearing the store first is what makes the
/// repair a rebuild rather than a top-up.
///
/// The hive's log files go too. They are the registry transaction logs for the
/// store, and leaving them next to a fresh BCD lets the loader replay the old
/// contents back over it.
///
/// Renamed rather than deleted: a `.bak` alongside costs nothing and leaves a
/// way back if a repair makes things worse. Any previous `.bak` is replaced,
/// so repeated repairs don't accumulate.
///
/// Doing this by moving files, rather than by asking `bcdboot` for a clean
/// store, is deliberate — `bcdboot`'s option set varies across Windows
/// versions, and an option the local `bcdboot` doesn't recognise fails the
/// whole repair (which is exactly how `/nofirmwaresync` broke the UEFI path).
/// Renaming a file behaves the same everywhere.
fn clear_existing_bcd(boot_dir: &Path, report: &mut BootRepairReport) -> Result<()> {
    let mut moved = Vec::new();
    for name in ["BCD", "BCD.LOG", "BCD.LOG1", "BCD.LOG2"] {
        let store = boot_dir.join(name);
        if !store.exists() {
            continue;
        }
        // The store is often read-only, which blocks the replace below.
        if let Ok(meta) = std::fs::metadata(&store) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(&store, perms);
            }
        }
        let backup = boot_dir.join(format!("{name}.bak"));
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(&store, &backup).map_err(|e| {
            PhoenixError::Disk(format!(
                "could not move the existing boot configuration {} aside: {e}",
                store.display()
            ))
        })?;
        moved.push(name);
    }
    if moved.is_empty() {
        report.push(format!(
            "No existing boot configuration in {} — bcdboot will create one",
            boot_dir.display()
        ));
    } else {
        report.push(format!(
            "Cleared the existing boot configuration in {} ({} kept as .bak)",
            boot_dir.display(),
            moved.join(", ")
        ));
    }
    Ok(())
}

fn run_tool(tool: &str, args: &[&str], report: &mut BootRepairReport) -> Result<()> {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let exe = format!("{system_root}\\System32\\{tool}.exe");
    let output = Command::new(&exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| PhoenixError::Disk(format!("failed to launch {exe}: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(PhoenixError::Disk(format!(
            "{tool} {} failed (exit {}): {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            {
                let msg = format!("{} {}", stdout.trim(), stderr.trim());
                msg.trim().to_string()
            }
        )));
    }
    let first_line = stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("OK");
    report.push(format!("{tool} {} — {first_line}", args.join(" ")));
    Ok(())
}

/// A non-empty primary slot of an MBR partition table.
struct MbrPrimary {
    boot: bool,
    start_lba: u32,
}

impl MbrPrimary {
    fn start_offset(&self, sector_size: u32) -> u64 {
        u64::from(self.start_lba) * u64::from(sector_size)
    }
}

fn partition_at_offset(disk: &DiskInfo, offset: u64) -> Option<&PartitionInfo> {
    disk.partitions.iter().find(|p| p.offset_bytes == offset)
}

fn open_disk_rw(disk_path: &str) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk_path)
        .map_err(|e| PhoenixError::Disk(format!("open {disk_path} for MBR access: {e}")))
}

fn read_boot_sector(disk_path: &str, sector_size: u32) -> Result<Vec<u8>> {
    let mut f = open_disk_rw(disk_path)?;
    let mut sector = vec![0u8; sector_size.max(512) as usize];
    f.read_exact(&mut sector)
        .map_err(|e| PhoenixError::Disk(format!("read MBR of {disk_path}: {e}")))?;
    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Err(PhoenixError::Disk(format!(
            "{disk_path} has no valid MBR (missing 55AA signature)"
        )));
    }
    Ok(sector)
}

/// The four primary partition-table slots that are populated.
fn read_mbr_primaries(disk_path: &str, sector_size: u32) -> Result<Vec<MbrPrimary>> {
    let sector = read_boot_sector(disk_path, sector_size)?;
    let mut out = Vec::new();
    for slot in 0..4usize {
        let e = &sector[0x1BE + slot * 16..0x1BE + slot * 16 + 16];
        if e[4] == 0 {
            continue;
        }
        out.push(MbrPrimary {
            boot: e[0] & 0x80 != 0,
            start_lba: u32::from_le_bytes([e[8], e[9], e[10], e[11]]),
        });
    }
    Ok(out)
}

/// Set the active flag on the primary entry starting at `offset_bytes` and
/// clear it everywhere else. A surgical rewrite of the flag bytes in sector
/// 0 — the partition entries, boot code, and disk signature are untouched,
/// and sector 0 lies outside every volume extent so the write is not subject
/// to Windows' mounted-volume write blocking.
fn set_mbr_active(disk_path: &str, sector_size: u32, offset_bytes: u64) -> Result<()> {
    let mut sector = read_boot_sector(disk_path, sector_size)?;
    let mut found = false;
    for slot in 0..4usize {
        let base = 0x1BE + slot * 16;
        if sector[base + 4] == 0 {
            continue;
        }
        let start_lba =
            u32::from_le_bytes(sector[base + 8..base + 12].try_into().expect("4 bytes"));
        let is_target = u64::from(start_lba) * u64::from(sector_size) == offset_bytes;
        sector[base] = if is_target { 0x80 } else { 0x00 };
        found |= is_target;
    }
    if !found {
        return Err(PhoenixError::Disk(
            "system partition not found among the MBR's primary entries".into(),
        ));
    }
    let mut f = open_disk_rw(disk_path)?;
    f.seek(SeekFrom::Start(0))
        .and_then(|_| f.write_all(&sector))
        .and_then(|_| f.flush())
        .map_err(|e| PhoenixError::Disk(format!("write MBR active flag on {disk_path}: {e}")))
}

/// "Windows 11 (build 26100)"-style label from ntoskrnl.exe's fixed version
/// resource. Best-effort — `None` simply drops the version from the display.
fn kernel_version(kernel: &Path) -> Option<String> {
    /// Leading fields of VS_FIXEDFILEINFO — the flag/date tail is not read,
    /// so the buffer only has to be this long.
    #[repr(C)]
    struct VsFixedFileInfo {
        signature: u32,
        struc_version: u32,
        file_version_ms: u32,
        file_version_ls: u32,
        product_version_ms: u32,
        product_version_ls: u32,
    }
    let wide: Vec<u16> = kernel.as_os_str().encode_wide().chain([0]).collect();
    let mut ignored = 0u32;
    let size = unsafe { GetFileVersionInfoSizeW(wide.as_ptr(), &mut ignored) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(wide.as_ptr(), 0, size, buf.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let sub: Vec<u16> = "\\".encode_utf16().chain([0]).collect();
    let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut len = 0u32;
    let ok = unsafe { VerQueryValueW(buf.as_ptr().cast(), sub.as_ptr(), &mut ptr, &mut len) };
    if ok == 0 || ptr.is_null() || (len as usize) < std::mem::size_of::<VsFixedFileInfo>() {
        return None;
    }
    let info = unsafe { &*(ptr as *const VsFixedFileInfo) };
    if info.signature != 0xFEEF_04BD {
        return None;
    }
    let major = info.product_version_ms >> 16;
    let minor = info.product_version_ms & 0xFFFF;
    let build = info.product_version_ls >> 16;
    let name = match (major, minor) {
        (10, _) if build >= 22000 => "Windows 11",
        (10, _) => "Windows 10",
        (6, 3) | (6, 2) => "Windows 8",
        (6, 1) => "Windows 7",
        _ => return Some(format!("Windows {major}.{minor} (build {build})")),
    };
    Some(format!("{name} (build {build})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_root_forms() {
        assert_eq!(volume_root(r"\\.\C:"), PathBuf::from(r"C:\"));
        assert_eq!(
            volume_root(r"\\?\Volume{aaaa-bbbb}"),
            PathBuf::from(r"\\?\Volume{aaaa-bbbb}\")
        );
        assert_eq!(
            volume_root(r"\\?\Volume{aaaa-bbbb}\"),
            PathBuf::from(r"\\?\Volume{aaaa-bbbb}\")
        );
    }

    /// The running system's kernel always has a parseable version resource;
    /// exercises the FFI path end to end.
    #[test]
    fn kernel_version_of_live_system() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let kernel = PathBuf::from(root).join("System32").join("ntoskrnl.exe");
        if kernel.is_file() {
            let v = kernel_version(&kernel);
            assert!(v.is_some(), "version resource should parse");
            assert!(v.unwrap().starts_with("Windows"));
        }
    }
}
