//! System-test harness for Phoenix Simulacra (Tier 2).
//!
//! These helpers drive **real** Windows virtual disks (VHDX attached via
//! `diskpart`, which surface as ordinary `\\.\PhysicalDriveN` devices) so the
//! backup / restore / clone / mount engines can be exercised end-to-end
//! against genuine NTFS/FAT/exFAT volumes without Hyper-V and without risking
//! the host's real disks.
//!
//! Everything here needs an **elevated** process (diskpart, raw disk handles,
//! volume formatting). The integration tests that use it are all `#[ignore]`d;
//! run them explicitly:
//!
//! ```text
//! cargo test -p phoenix-systests -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` is mandatory: diskpart attach/detach and disk-index
//! discovery are process-global and race badly in parallel.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

/// Filesystem to format a test partition with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestFs {
    Ntfs,
    Fat32,
    Exfat,
    /// FAT (chosen by size; use a small partition to land on FAT16, tiny for
    /// FAT12). diskpart's `format fs=fat` picks the width from the size.
    Fat,
}

impl TestFs {
    fn diskpart_fs(self) -> &'static str {
        match self {
            TestFs::Ntfs => "ntfs",
            TestFs::Fat32 => "fat32",
            TestFs::Exfat => "exfat",
            TestFs::Fat => "fat",
        }
    }
}

/// A partition to create on a fresh test disk.
#[derive(Clone, Debug)]
pub struct PartSpec {
    /// Size in MiB. Use 0 for "the rest of the disk" (must be the last spec).
    pub size_mb: u64,
    pub fs: TestFs,
    /// Drive letter to assign (e.g. 'X'). Must be free on the host.
    pub letter: char,
    pub label: String,
}

/// Abort the test with a clear message if the process isn't elevated. Call
/// this at the top of every `#[ignore]` system test.
pub fn require_admin() {
    if !is_elevated() {
        panic!(
            "phoenix-systests requires an elevated (Administrator) shell. \
             Re-run from an elevated prompt: \
             `cargo test -p phoenix-systests -- --ignored --test-threads=1`"
        );
    }
}

fn is_elevated() -> bool {
    // `net session` succeeds only for administrators; it's a cheap, dependency
    // -free elevation probe that doesn't require linking advapi32 bindings.
    Command::new("net")
        .args(["session"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A live, attached VHDX exposed as a real physical disk. Dropping it detaches
/// the disk and deletes the backing file.
pub struct TestVhd {
    path: PathBuf,
    /// `\\.\PhysicalDriveN` for the attached disk.
    physical_path: String,
    /// The `N` in PhysicalDriveN.
    disk_index: u32,
    detached: bool,
}

impl TestVhd {
    /// Create an expandable VHDX of `size_mb` MiB, attach it, and resolve its
    /// physical-disk index. The disk is left uninitialized (RAW); call
    /// [`TestVhd::init_gpt_with`] to lay down partitions.
    pub fn create(size_mb: u64) -> Result<Self> {
        require_admin();
        let (path, path_str) = new_vhdx_path()?;

        // Snapshot disk locations before attach so we can identify the new one
        // even if PhysicalDrive numbering is non-monotonic.
        let before = disk_numbers_by_location()?;

        run_diskpart(&format!(
            "create vdisk file=\"{path_str}\" maximum={size_mb} type=expandable\n\
             select vdisk file=\"{path_str}\"\n\
             attach vdisk\n"
        ))
        .context("diskpart create/attach vdisk")?;

        Self::finish_attach(path, path_str, before)
    }

    /// Create and attach an expandable VHDX with a **4096-byte logical sector
    /// size** (true 4Kn), then resolve its physical-disk index. Left
    /// uninitialized (RAW), same as [`TestVhd::create`].
    ///
    /// diskpart's `create vdisk` has no way to set the sector size — it always
    /// produces 512-byte logical sectors, which is why every other T2 fixture is
    /// 512-only. So the file is created through `CreateVirtualDisk`
    /// (virtdisk.dll), which takes an explicit `SectorSizeInBytes`. That is the
    /// plain Win32 VHD API present on **every** Windows edition (it is the same
    /// DLL `phoenix-mount` already uses to attach) — deliberately **not** the
    /// Hyper-V `New-VHD` cmdlet, which requires a Pro+ SKU and would put a
    /// license floor under the test suite.
    ///
    /// Attach still goes through diskpart, which is happy to attach a VHDX it
    /// did not create.
    pub fn create_4kn(size_mb: u64) -> Result<Self> {
        require_admin();
        let (path, path_str) = new_vhdx_path()?;

        create_vhdx_with_sector_size(&path, size_mb * 1024 * 1024, 4096)
            .context("CreateVirtualDisk (4Kn)")?;

        let before = disk_numbers_by_location()?;
        run_diskpart(&format!("select vdisk file=\"{path_str}\"\nattach vdisk\n"))
            .context("diskpart attach vdisk (4Kn)")?;

        Self::finish_attach(path, path_str, before)
    }

    /// Resolve the just-attached disk's number by its VHDX location (robust
    /// against parallel disk changes; we still require `--test-threads=1`).
    fn finish_attach(path: PathBuf, path_str: String, before: Vec<(u32, String)>) -> Result<Self> {
        let disk_index = resolve_disk_number_for_vhd(&path_str, &before)
            .context("resolving attached VHD disk number")?;

        Ok(TestVhd {
            path,
            physical_path: format!(r"\\.\PhysicalDrive{disk_index}"),
            disk_index,
            detached: false,
        })
    }

    pub fn disk_index(&self) -> u32 {
        self.disk_index
    }

    pub fn physical_path(&self) -> &str {
        &self.physical_path
    }

    /// Initialize the disk as GPT and create + format the given partitions,
    /// assigning drive letters. Returns once the volumes are mounted.
    pub fn init_gpt_with(&self, parts: &[PartSpec]) -> Result<()> {
        diskpart_layout(self.disk_index, true, parts)
    }

    /// As [`TestVhd::init_gpt_with`], but formats with an explicit
    /// allocation-unit size (e.g. 65536 for 64K clusters) instead of the
    /// filesystem default.
    pub fn init_gpt_with_cluster(&self, parts: &[PartSpec], cluster_bytes: u32) -> Result<()> {
        diskpart_layout_with_cluster(self.disk_index, true, parts, Some(cluster_bytes))
    }

    /// Detach the VHD explicitly (also done on Drop). Idempotent.
    pub fn detach(&mut self) -> Result<()> {
        if self.detached {
            return Ok(());
        }
        let path_str = self.path.to_string_lossy().to_string();
        let _ = run_diskpart(&format!("select vdisk file=\"{path_str}\"\ndetach vdisk\n"));
        self.detached = true;
        Ok(())
    }
}

impl Drop for TestVhd {
    fn drop(&mut self) {
        let _ = self.detach();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Make sure `letter` is actually assignable before a fixture demands it.
///
/// Two distinct squatters produce Windows' "the requested access path is
/// already in use":
///
/// * A **stale mount-manager mapping** — a crashed prior run can leave
///   `mountvol` mapping the letter to a volume that no longer exists
///   (observed after a force-dismounted BitLocker volume's disk was cleared,
///   and again after an aborted boot-disk run). No live partition owns the
///   letter, so dropping the dead mapping is safe: self-heal with
///   `mountvol <letter>: /D`.
/// * A **live volume** — e.g. a restored test-clone disk that is still
///   online and grabbed letters on auto-mount. Freeing someone's mounted
///   volume is not our call; fail fast with a message that names the owner
///   so the user can offline the disk or move the letter. Exception: a
///   volume on `target_disk` itself — the layout is about to `clean` that
///   disk, which frees the letter anyway.
fn ensure_letter_free(letter: char, target_disk: u32) -> Result<()> {
    let out = powershell(&format!(
        "$p = Get-Partition -ErrorAction SilentlyContinue | \
             Where-Object DriveLetter -eq '{letter}' | Select-Object -First 1; \
         if ($p) {{ \"LIVE $($p.DiskNumber) $($p.PartitionNumber)\" }} \
         else {{ mountvol {letter}: /D 2>$null; 'FREE' }}"
    ))?;
    if let Some(owner) = out.trim().strip_prefix("LIVE ") {
        let mut fields = owner.split_whitespace();
        let disk: Option<u32> = fields.next().and_then(|s| s.parse().ok());
        let part = fields.next().unwrap_or("?");
        if disk == Some(target_disk) {
            return Ok(());
        }
        bail!(
            "drive letter {letter}: is in use by disk {} partition {part}, but the system \
             tests need it free. If that's a restored test-clone disk, take it offline \
             (`Set-Disk -Number <n> -IsOffline $true`) or remove the letter, then re-run.",
            disk.map(|d| d.to_string()).unwrap_or_else(|| "?".into()),
        );
    }
    Ok(())
}

/// Wipe `disk_index`, initialize its partition table (`gpt` true → GPT, else
/// MBR), and create + format + letter the given partitions via diskpart.
/// DESTRUCTIVE — used by both [`TestVhd`] and [`RealDisk`] (which safety-checks
/// the target first).
pub fn diskpart_layout(disk_index: u32, gpt: bool, parts: &[PartSpec]) -> Result<()> {
    diskpart_layout_with_cluster(disk_index, gpt, parts, None)
}

/// As [`diskpart_layout`], but formats every partition with an explicit
/// allocation-unit size (diskpart's `format ... unit=`) instead of the
/// filesystem's default.
///
/// The default matters more than it looks: NTFS picks 4096 for any volume up to
/// 16 TB, so *every* fixture in this suite is a 4K-cluster volume — which is
/// exactly why a hardcoded `bytes_per_cluster = 4096` in the capture planner
/// survived unnoticed. 64K clusters are common on real large data volumes, and a
/// fixture that uses them is the only thing that can catch that class of bug.
///
/// The override is per-layout rather than per-`PartSpec` on purpose: only the
/// cluster-size regression test needs it, and threading an `Option` through all
/// 30 `PartSpec` literals would be churn for no coverage.
pub fn diskpart_layout_with_cluster(
    disk_index: u32,
    gpt: bool,
    parts: &[PartSpec],
    cluster_bytes: Option<u32>,
) -> Result<()> {
    for p in parts {
        ensure_letter_free(p.letter, disk_index)?;
    }
    let style = if gpt { "gpt" } else { "mbr" };
    let mut script = format!("select disk {disk_index}\nclean\nconvert {style}\n");
    // diskpart takes the allocation unit in bytes (`unit=65536`).
    let unit = cluster_bytes
        .map(|c| format!(" unit={c}"))
        .unwrap_or_default();
    for p in parts {
        if p.size_mb == 0 {
            script.push_str("create partition primary\n");
        } else {
            script.push_str(&format!("create partition primary size={}\n", p.size_mb));
        }
        script.push_str(&format!(
            "format fs={} label={}{unit} quick\n",
            p.fs.diskpart_fs(),
            p.label
        ));
        script.push_str(&format!("assign letter={}\n", p.letter));
    }
    run_diskpart(&script).context("diskpart layout")?;
    Ok(())
}

/// Lay out a disk using PowerShell storage cmdlets, which (unlike diskpart's
/// `format`) work on **removable** USB media. Clears the disk, initializes the
/// partition table, then creates + formats + letters each partition.
pub fn powershell_layout(disk_index: u32, gpt: bool, parts: &[PartSpec]) -> Result<()> {
    for p in parts {
        ensure_letter_free(p.letter, disk_index)?;
    }
    let style = if gpt { "GPT" } else { "MBR" };
    // Clear-Disk removes partitions but leaves the disk *initialized* (not RAW),
    // so Initialize-Disk would reject it. Only initialize a genuinely-RAW disk;
    // otherwise the existing (already-correct) style is reused. A wrong existing
    // style would need a partition-table conversion we don't do here (removable
    // disks are always MBR anyway).
    let mut script = format!(
        "$ErrorActionPreference='Stop'; \
         $d = Get-Disk -Number {disk_index}; \
         if ($d.PartitionStyle -ne 'RAW') {{ \
             Clear-Disk -Number {disk_index} -RemoveData -RemoveOEM -Confirm:$false; \
             $d = Get-Disk -Number {disk_index} }}; \
         if ($d.PartitionStyle -eq 'RAW') {{ \
             Initialize-Disk -Number {disk_index} -PartitionStyle {style} }} \
         elseif ($d.PartitionStyle -ne '{style}') {{ \
             throw \"disk {disk_index} is $($d.PartitionStyle), expected {style}\" }}; "
    );
    for p in parts {
        let size = if p.size_mb == 0 {
            "-UseMaximumSize".to_string()
        } else {
            format!("-Size {}MB", p.size_mb)
        };
        let fs = match p.fs {
            TestFs::Ntfs => "NTFS",
            TestFs::Fat32 => "FAT32",
            TestFs::Exfat => "exFAT",
            TestFs::Fat => "FAT",
        };
        // Stale-mapping / live-collision handling for the letter happens in
        // the `ensure_letter_free` preflight above.
        script.push_str(&format!(
            "New-Partition -DiskNumber {disk_index} {size} -DriveLetter {letter} | Out-Null; \
             Format-Volume -DriveLetter {letter} -FileSystem {fs} \
                 -NewFileSystemLabel {label} -Confirm:$false | Out-Null; ",
            letter = p.letter,
            label = p.label
        ));
    }
    powershell(&script).context("powershell disk layout")?;
    Ok(())
}

/// Drive letters assigned to a disk's partitions, in on-disk (offset) order.
pub fn disk_volume_letters(disk_index: u32) -> Result<Vec<char>> {
    let out = powershell(&format!(
        "Get-Partition -DiskNumber {disk_index} -ErrorAction SilentlyContinue | \
         Where-Object DriveLetter | Sort-Object Offset | ForEach-Object {{ $_.DriveLetter }}"
    ))?;
    Ok(out
        .lines()
        .filter_map(|l| l.trim().chars().next())
        .filter(|c| c.is_ascii_alphabetic())
        .collect())
}

/// Poll until a disk has at least `want` lettered volumes (all roots
/// reachable), force-assigning letters to any un-lettered partitions if the
/// mount manager is slow. Returns the letters in on-disk order.
pub fn wait_for_disk_volumes(disk_index: u32, want: usize, timeout_ms: u64) -> Result<Vec<char>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut forced = false;
    loop {
        let letters = disk_volume_letters(disk_index).unwrap_or_default();
        let reachable: Vec<char> = letters
            .into_iter()
            .filter(|l| Path::new(&format!("{l}:\\")).exists())
            .collect();
        if reachable.len() >= want {
            return Ok(reachable);
        }
        if std::time::Instant::now() >= deadline {
            if !forced {
                forced = true;
                let _ = assign_letters_to_disk(disk_index);
                continue;
            }
            bail!(
                "disk {disk_index}: only {} of {want} volumes got drive letters",
                reachable.len()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Assign a drive letter to every un-lettered partition on a disk (best effort).
fn assign_letters_to_disk(disk_index: u32) -> Result<()> {
    powershell(&format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         Get-Partition -DiskNumber {disk_index} | Where-Object {{ -not $_.DriveLetter -and $_.Size -gt 1MB }} | \
         ForEach-Object {{ Add-PartitionAccessPath -DiskNumber {disk_index} -PartitionNumber $_.PartitionNumber -AssignDriveLetter }}"
    ))?;
    Ok(())
}

/// A summary of a disk's partitions (offset, size, type string), sorted by
/// offset — used to assert partition-table integrity across a restore.
pub fn partition_summary(disk_index: u32) -> Result<Vec<(u64, u64, String)>> {
    let out = powershell(&format!(
        "Get-Partition -DiskNumber {disk_index} -ErrorAction SilentlyContinue | \
         Sort-Object Offset | ForEach-Object {{ \"$($_.Offset)|$($_.Size)|$($_.Type)\" }}"
    ))?;
    let mut parts = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.trim().split('|').collect();
        if f.len() >= 3 {
            let off = f[0].trim().parse().unwrap_or(0);
            let size = f[1].trim().parse().unwrap_or(0);
            parts.push((off, size, f[2].trim().to_string()));
        }
    }
    Ok(parts)
}

/// Outcome of the safety gate: the disk's serial and whether it's **removable**
/// media (a throwaway USB flash stick) vs. a non-removable disk (internal disk
/// OR external USB HDD), which holds real data and is only allowed under a
/// stricter opt-in. Removability — not bus type — is the right axis: a 4 TB
/// USB HDD reports `BusType=USB` but is non-removable (and GPT-capable), so it
/// must get the same protection as an internal disk.
struct ValidatedDisk {
    serial: String,
    is_removable: bool,
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        })
        .unwrap_or(false)
}

fn env_gb(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
}

/// Re-run the full safety gate for `index`. Bails (never returns Ok) if ANY
/// gate fails, so a destructive op can never proceed against the wrong disk
/// even if indices shift between calls.
///
/// Two target classes, keyed on **removable media** (Win32_DiskDrive
/// `MediaType` = "Removable Media"), NOT bus type:
/// - **Removable** (a USB flash stick): allowed with the historical gates —
///   not boot/system, size in range, optional serial pin.
/// - **Non-removable** (internal disk OR external USB HDD): only when
///   `PHOENIX_T3_ALLOW_FIXED=1` is set AND `PHOENIX_T3_SERIAL` pins the exact
///   device (MANDATORY here — a real disk holds real data, so we require the
///   operator to name the precise serial). This is the class GPT-on-real-
///   hardware needs, since Windows won't make removable media GPT.
///
/// Size bounds default to 16–64 GB (a USB-stick heuristic) and are widenable
/// via `PHOENIX_T3_MIN_GB` / `PHOENIX_T3_MAX_GB` for a larger fixed test disk.
///
/// Unknown/blank `MediaType` fails safe: treated as NON-removable (strict).
/// Validate a real disk, reading its serial pin from `serial_var`.
///
/// There is deliberately **no** convenience wrapper that defaults `serial_var` to
/// `PHOENIX_T3_SERIAL`. A disk-to-disk clone has two real disks with two
/// independent pins, and the first version of this had exactly that wrapper — so
/// `RealDisk::clean()` on the *source* re-checked disk 2's serial against the
/// *target's* pin and refused to run. That was the harmless direction. With the
/// roles reversed it is the other kind of bug entirely, so the pin now travels
/// with the disk and there is no way to ask this question without saying which
/// disk you mean.
fn validate_real_disk_pinned(index: u32, serial_var: &str) -> Result<ValidatedDisk> {
    let info = powershell(&format!(
        "$d = Get-Disk -Number {index} -ErrorAction Stop; \
         $dd = Get-CimInstance Win32_DiskDrive -Filter \"Index={index}\"; \
         \"$($d.BusType)|$($d.IsBoot)|$($d.IsSystem)|$($d.Size)|$($d.SerialNumber)|$($dd.MediaType)\""
    ))
    .with_context(|| format!("querying disk {index}"))?;
    let f: Vec<&str> = info.trim().split('|').collect();
    let bus = f.first().copied().unwrap_or("");
    let is_boot = f.get(1).copied().unwrap_or("");
    let is_system = f.get(2).copied().unwrap_or("");
    let size: u64 = f.get(3).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let serial = f.get(4).copied().unwrap_or("").trim().to_string();
    let media = f.get(5).copied().unwrap_or("").trim();

    // "Removable Media" = a flash stick. "External/Fixed hard disk media" =
    // non-removable. Blank/unknown → fail safe as non-removable (strict).
    let is_removable = media.to_lowercase().contains("removable");
    if !is_removable {
        if !env_truthy("PHOENIX_T3_ALLOW_FIXED") {
            bail!(
                "SAFETY: disk {index} is non-removable media (MediaType={media:?}, \
                 BusType={bus:?}) — refusing. Non-removable disks (internal, or external USB \
                 HDDs) can hold real data. To target one you MUST set PHOENIX_T3_ALLOW_FIXED=1 \
                 AND pin the exact device with PHOENIX_T3_SERIAL."
            );
        }
        // The exact-serial pin is MANDATORY (belt-and-suspenders against wiping
        // the wrong drive now that the removable safety net is gone).
        let want = std::env::var(serial_var).map_err(|_| {
            anyhow!(
                "SAFETY: targeting a non-removable disk requires {serial_var} set to the exact \
                 device serial (got PHOENIX_T3_ALLOW_FIXED but no serial pin)"
            )
        })?;
        if !serial.eq_ignore_ascii_case(want.trim()) {
            bail!("SAFETY: disk {index} serial {serial:?} != {serial_var} {want:?} — refusing");
        }
    }
    if is_boot.eq_ignore_ascii_case("True") {
        bail!("SAFETY: disk {index} is the BOOT disk — refusing");
    }
    if is_system.eq_ignore_ascii_case("True") {
        bail!("SAFETY: disk {index} is the SYSTEM disk — refusing");
    }
    let min_gb = env_gb("PHOENIX_T3_MIN_GB").unwrap_or(16.0);
    let max_gb = env_gb("PHOENIX_T3_MAX_GB").unwrap_or(64.0);
    let gb = size as f64 / 1e9;
    if !(min_gb..=max_gb).contains(&gb) {
        bail!(
            "SAFETY: disk {index} size {gb:.1} GB outside {min_gb}–{max_gb} GB — refusing \
             (widen with PHOENIX_T3_MIN_GB / PHOENIX_T3_MAX_GB if this really is your test disk)"
        );
    }
    // Removable path keeps the historical *optional* serial pin.
    if is_removable {
        if let Ok(want) = std::env::var(serial_var) {
            if !serial.eq_ignore_ascii_case(want.trim()) {
                bail!("SAFETY: disk {index} serial {serial:?} != {serial_var} {want:?} — refusing");
            }
        }
    }
    Ok(ValidatedDisk {
        serial,
        is_removable,
    })
}

/// A real, physical test disk opted into via the `PHOENIX_T3_DISK` env var and
/// guarded so destructive tests can never hit a boot/system disk, an out-of-size
/// disk, or (without an explicit opt-in) a non-removable disk.
pub struct RealDisk {
    index: u32,
    is_removable: bool,
    /// The env var holding *this* disk's serial pin — `PHOENIX_T3_SERIAL` for the
    /// target, `PHOENIX_T3_SRC_SERIAL` for a clone's source.
    ///
    /// Carried on the disk rather than looked up at the point of use, because
    /// every destructive method re-validates before it writes, and a re-check
    /// against the *other* disk's serial is not a safety check — it is a
    /// coin-flip that either refuses a disk it should allow (which is how this
    /// field came to exist) or, with the roles reversed, allows one it should
    /// refuse.
    serial_var: String,
}

impl RealDisk {
    /// Resolve the opt-in real test disk. Returns `Ok(None)` when
    /// `PHOENIX_T3_DISK` is unset (the test should skip). Every safety gate is
    /// checked; a failure returns `Err` rather than a disk.
    pub fn acquire() -> Result<Option<Self>> {
        Self::acquire_env("PHOENIX_T3_DISK", "PHOENIX_T3_SERIAL")
    }

    /// The **source** disk of a real disk-to-disk clone, opted into via
    /// `PHOENIX_T3_SRC_DISK` / `PHOENIX_T3_SRC_SERIAL`.
    ///
    /// Gated exactly as hard as the target, and for the same reason: the clone
    /// tests lay their own fixture down on the source before reading it, so it is
    /// every bit as destroyable as the disk being cloned onto. A "source" that
    /// skipped the boot/system check would be the easiest way imaginable to
    /// reformat someone's laptop.
    pub fn acquire_source() -> Result<Option<Self>> {
        Self::acquire_env("PHOENIX_T3_SRC_DISK", "PHOENIX_T3_SRC_SERIAL")
    }

    fn acquire_env(disk_var: &str, serial_var: &str) -> Result<Option<Self>> {
        let Ok(raw) = std::env::var(disk_var) else {
            return Ok(None);
        };
        let index: u32 = raw
            .trim()
            .parse()
            .with_context(|| format!("{disk_var} must be a disk number"))?;
        let v = validate_real_disk_pinned(index, serial_var)?;
        let role = if disk_var.contains("SRC") {
            "source"
        } else {
            "target"
        };
        eprintln!(
            "[T3] {role} = disk {index}, {}, serial {} — safety gates passed",
            if v.is_removable {
                "removable (USB flash)"
            } else {
                "NON-REMOVABLE (internal / external HDD)"
            },
            v.serial
        );
        // The target may have been parked offline between runs (recommended
        // hygiene so a restored test clone's volumes don't squat on drive
        // letters). Consent to destroy this disk is explicit — the env
        // opt-in plus the gates above — so bring it back online rather than
        // letting every diskpart clean / Clear-Disk fail with "The disk is
        // offline". Also drop a read-only attribute a previous offline may
        // have left behind. Best-effort: an actual failure surfaces on the
        // first destructive op with a precise error.
        let onlined = powershell(&format!(
            "$d = Get-Disk -Number {index}; \
             if ($d.IsReadOnly) {{ Set-Disk -Number {index} -IsReadOnly $false }}; \
             if ($d.IsOffline) {{ Set-Disk -Number {index} -IsOffline $false; 'onlined' }}"
        ));
        if matches!(onlined.as_deref().map(str::trim), Ok("onlined")) {
            eprintln!("[T3] {role} disk {index} was offline — brought it online");
        }
        Ok(Some(Self {
            index,
            is_removable: v.is_removable,
            serial_var: serial_var.to_string(),
        }))
    }

    /// Re-run every safety gate against **this** disk's own serial pin.
    ///
    /// Called before each destructive operation, so a disk that was swapped,
    /// re-numbered, or brought online as the boot disk between acquisition and
    /// use cannot slip through on a stale check.
    fn revalidate(&self) -> Result<()> {
        validate_real_disk_pinned(self.index, &self.serial_var)?;
        Ok(())
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// Whether the target is removable media (a USB flash stick). GPT tests
    /// skip a removable target — Windows won't make removable media GPT.
    pub fn is_removable(&self) -> bool {
        self.is_removable
    }

    /// Wipe the disk (re-validates safety first).
    ///
    /// diskpart `clean` is timing-sensitive: a freshly (re)mounted volume on
    /// the disk — e.g. a just-unlocked BitLocker volume being scanned by the
    /// search indexer or AV — can hold handles that surface as a spurious
    /// "Access is denied" (0x80070005). Retry with backoff, then fall back
    /// to `Clear-Disk`, which force-dismounts volumes and has succeeded on
    /// disks diskpart refused.
    pub fn clean(&self) -> Result<()> {
        self.revalidate()?;
        let mut last_err = None;
        for attempt in 1u32..=3 {
            match run_diskpart(&format!("select disk {}\nclean\n", self.index)) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    eprintln!("[T3] diskpart clean attempt {attempt} failed: {e:#}");
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_secs(2 * attempt as u64));
                }
            }
        }
        eprintln!("[T3] falling back to Clear-Disk");
        self.revalidate()?;
        powershell(&format!(
            "Clear-Disk -Number {} -RemoveData -RemoveOEM -Confirm:$false",
            self.index
        ))
        .with_context(|| {
            format!(
                "clean real disk: diskpart failed 3x ({}), then Clear-Disk failed",
                last_err
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "unknown".into())
            )
        })?;
        Ok(())
    }

    /// Lay out partitions on the disk (re-validates safety first). Uses the
    /// PowerShell storage cmdlets, which work on removable USB media.
    pub fn layout(&self, gpt: bool, parts: &[PartSpec]) -> Result<()> {
        self.revalidate()?;
        powershell_layout(self.index, gpt, parts)
    }
}

/// Run a diskpart script from a temp file, returning an error with diskpart's
/// output attached on failure.
fn run_diskpart(script: &str) -> Result<String> {
    let script_path = scratch_dir()?.join(format!("dp-{}.txt", new_id()));
    std::fs::write(&script_path, script).context("writing diskpart script")?;
    let out = Command::new("diskpart")
        .arg("/s")
        .arg(&script_path)
        .output()
        .context("spawning diskpart")?;
    let _ = std::fs::remove_file(&script_path);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        bail!(
            "diskpart failed (exit {:?}):\n--- script ---\n{script}\n--- output ---\n{stdout}",
            out.status.code()
        );
    }
    Ok(stdout)
}

/// Map every disk's VHD/backing location to its disk number via PowerShell.
/// Non-VHD disks report an empty location and are skipped.
fn disk_numbers_by_location() -> Result<Vec<(u32, String)>> {
    let out = powershell("Get-Disk | ForEach-Object { \"$($_.Number)`t$($_.Location)\" }")?;
    let mut result = Vec::new();
    for line in out.lines() {
        let mut it = line.splitn(2, '\t');
        if let (Some(num), Some(loc)) = (it.next(), it.next()) {
            if let Ok(n) = num.trim().parse::<u32>() {
                result.push((n, loc.trim().to_string()));
            }
        }
    }
    Ok(result)
}

/// Find the disk number whose location matches the VHD path. Falls back to
/// "the disk number that appeared since `before`" if the location string
/// doesn't match exactly (path casing / short-vs-long path differences).
fn resolve_disk_number_for_vhd(vhd_path: &str, before: &[(u32, String)]) -> Result<u32> {
    let after = disk_numbers_by_location()?;
    let want = vhd_path.to_ascii_lowercase();

    if let Some((n, _)) = after
        .iter()
        .find(|(_, loc)| loc.to_ascii_lowercase() == want)
    {
        return Ok(*n);
    }
    // Fallback: the disk number present in `after` but not `before`.
    let before_nums: std::collections::HashSet<u32> = before.iter().map(|(n, _)| *n).collect();
    let mut fresh: Vec<u32> = after
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !before_nums.contains(n))
        .collect();
    match fresh.len() {
        1 => Ok(fresh.pop().unwrap()),
        0 => Err(anyhow!(
            "attached VHD {vhd_path} did not appear as a new disk; \
             locations seen: {after:?}"
        )),
        _ => Err(anyhow!(
            "ambiguous: multiple new disks appeared after attach ({fresh:?}); \
             run system tests single-threaded"
        )),
    }
}

/// Run a PowerShell script (non-interactive, profile-less) and return its
/// stdout, failing with stderr attached. Public so tests can drive tooling
/// the harness doesn't wrap (e.g. the BitLocker cmdlets).
pub fn powershell(script: &str) -> Result<String> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output()
        .context("spawning powershell")?;
    if !out.status.success() {
        bail!(
            "powershell failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Scratch directory for VHDs and scripts (under the system temp dir).
fn scratch_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("phoenix-systests");
    std::fs::create_dir_all(&dir).context("creating scratch dir")?;
    Ok(dir)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A fresh, unused `<scratch>/phoenix-systest-<uuid>.vhdx` path, as both a
/// `PathBuf` and the UTF-8 string diskpart scripts need.
fn new_vhdx_path() -> Result<(PathBuf, String)> {
    let path = scratch_dir()?.join(format!("phoenix-systest-{}.vhdx", new_id()));
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 scratch path"))?
        .to_string();
    Ok((path, path_str))
}

/// Create (but do not attach) a dynamically-expanding VHDX with an explicit
/// **logical** sector size, via `CreateVirtualDisk` from virtdisk.dll.
///
/// This exists because diskpart cannot set a sector size. `CREATE_VIRTUAL_DISK_
/// PARAMETERS` **Version2** carries `SectorSizeInBytes` (the logical size the
/// disk reports to Windows, i.e. what makes a disk "4Kn") and
/// `PhysicalSectorSizeInBytes`; Version1 has no physical-size field at all, so
/// Version2 is required to describe 4Kn honestly rather than as 512e.
///
/// Sector size and edition support: virtdisk.dll is core Windows, present on
/// Home. Only VHDX supports a 4096-byte logical sector (the older VHD format is
/// 512-only), hence `VIRTUAL_STORAGE_TYPE_DEVICE_VHDX`.
pub fn create_vhdx_with_sector_size(
    path: &Path,
    size_bytes: u64,
    logical_sector: u32,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::core::GUID;
    use windows_sys::Win32::Storage::Vhd::{
        CreateVirtualDisk, CREATE_VIRTUAL_DISK_FLAG_NONE, CREATE_VIRTUAL_DISK_PARAMETERS,
        CREATE_VIRTUAL_DISK_VERSION_2, VIRTUAL_DISK_ACCESS_NONE, VIRTUAL_STORAGE_TYPE,
        VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
    };

    // VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT — {ec984aec-a0f9-47e9-901f-71415a66345b}
    const VENDOR_MICROSOFT: GUID = GUID {
        data1: 0xec98_4aec,
        data2: 0xa0f9,
        data3: 0x47e9,
        data4: [0x90, 0x1f, 0x71, 0x41, 0x5a, 0x66, 0x34, 0x5b],
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    let storage_type = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: VENDOR_MICROSOFT,
    };

    // Zero-init and set only what we mean: a zeroed Version2 already says
    // "dynamic, no parent, no source, provider-default block size", which is
    // exactly the fixture we want. Building it field-by-field would also have to
    // track fields we don't care about (SourceLimitPath, BackingStorageType).
    // SAFETY: every field of Version2 is a pointer, integer, GUID, or POD struct,
    // for all of which all-zero is a valid value.
    let mut params: CREATE_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
    params.Version = CREATE_VIRTUAL_DISK_VERSION_2;
    params.Anonymous.Version2.MaximumSize = size_bytes;
    params.Anonymous.Version2.SectorSizeInBytes = logical_sector;
    params.Anonymous.Version2.PhysicalSectorSizeInBytes = logical_sector;

    let mut handle = std::ptr::null_mut();
    // SAFETY: `storage_type`/`params` outlive the call, `wide` is NUL-terminated,
    // and the optional security-descriptor / overlapped pointers are null.
    // A dynamic VHDX is created by default (no FIXED flag), so nothing is
    // preallocated and the call returns immediately.
    let err = unsafe {
        CreateVirtualDisk(
            &storage_type,
            wide.as_ptr(),
            VIRTUAL_DISK_ACCESS_NONE,
            std::ptr::null_mut(),
            CREATE_VIRTUAL_DISK_FLAG_NONE,
            0,
            &params,
            std::ptr::null(),
            &mut handle,
        )
    };
    if err != 0 {
        bail!(
            "CreateVirtualDisk failed for {} (Win32 error {err}); \
             logical sector {logical_sector}",
            path.display()
        );
    }
    // The handle only keeps the disk open; the file is fully created. Closing it
    // does not delete anything.
    if !handle.is_null() {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    }
    Ok(())
}

/// Create a differencing VHDX `child` whose parent is `parent`, via
/// `CreateVirtualDisk` (virtdisk.dll). Leaving MaximumSize / SectorSize zero
/// makes the child inherit them from the parent — the declaration of "I am a
/// copy-on-write diff of that disk". Nothing is preallocated; the child starts a
/// few MiB and grows only with writes.
///
/// The parent may be an ordinary file or one served by a user-mode filesystem
/// (WinFsp): Windows reads it through the same file API either way.
pub fn create_differencing_vhdx(child: &Path, parent: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::Vhd::{
        CreateVirtualDisk, CREATE_VIRTUAL_DISK_FLAG_NONE, CREATE_VIRTUAL_DISK_PARAMETERS,
        CREATE_VIRTUAL_DISK_VERSION_2, VIRTUAL_DISK_ACCESS_NONE, VIRTUAL_STORAGE_TYPE,
        VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
    };

    const VENDOR_MICROSOFT: GUID = GUID {
        data1: 0xec98_4aec,
        data2: 0xa0f9,
        data3: 0x47e9,
        data4: [0x90, 0x1f, 0x71, 0x41, 0x5a, 0x66, 0x34, 0x5b],
    };

    let child_w: Vec<u16> = child.as_os_str().encode_wide().chain(Some(0)).collect();
    let parent_w: Vec<u16> = parent.as_os_str().encode_wide().chain(Some(0)).collect();
    let storage = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: VENDOR_MICROSOFT,
    };

    // SAFETY: a zeroed Version2 means "dynamic, provider defaults"; we then set
    // only the parent, which is what turns it into a differencing disk. Every
    // field of Version2 is a pointer/int/GUID/POD for which all-zero is valid.
    let mut params: CREATE_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
    params.Version = CREATE_VIRTUAL_DISK_VERSION_2;
    params.Anonymous.Version2.ParentPath = parent_w.as_ptr();
    params.Anonymous.Version2.ParentVirtualStorageType = storage;

    let mut handle = std::ptr::null_mut();
    // SAFETY: pointers outlive the call; both wide paths are NUL-terminated; the
    // optional security-descriptor / overlapped pointers are null.
    let err = unsafe {
        CreateVirtualDisk(
            &storage,
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
    if err != 0 {
        bail!(
            "CreateVirtualDisk (differencing) failed for {} over parent {} (Win32 error {err}, \
             0x{err:08X})",
            child.display(),
            parent.display()
        );
    }
    if !handle.is_null() {
        unsafe { CloseHandle(handle) };
    }
    Ok(())
}

/// A read-WRITE attach of a virtual disk (e.g. a differencing child). Dropping
/// detaches it — no `PERMANENT_LIFETIME` is requested, so closing the handle is
/// the detach, mirroring `phoenix_mount::attach`.
pub struct AttachedRw {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl AttachedRw {
    /// Open `vhdx_path` read-write and attach it, letting the mount manager
    /// assign drive letters to its volumes. `RWDepth = 1` opens only the top of
    /// a differencing chain (the child) writable and any parent(s) read-only —
    /// exactly the copy-on-write guarantee.
    pub fn attach(vhdx_path: &Path) -> Result<Self> {
        use std::os::windows::ffi::OsStrExt;

        use windows_sys::core::GUID;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::Vhd::{
            AttachVirtualDisk, OpenVirtualDisk, ATTACH_VIRTUAL_DISK_FLAG_NONE,
            ATTACH_VIRTUAL_DISK_PARAMETERS, OPEN_VIRTUAL_DISK_FLAG_NONE, OPEN_VIRTUAL_DISK_PARAMETERS,
            VIRTUAL_DISK_ACCESS_ATTACH_RW, VIRTUAL_DISK_ACCESS_GET_INFO, VIRTUAL_STORAGE_TYPE,
            VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        };

        const VENDOR_MICROSOFT: GUID = GUID {
            data1: 0xec98_4aec,
            data2: 0xa0f9,
            data3: 0x47e9,
            data4: [0x90, 0x1f, 0x71, 0x41, 0x5a, 0x66, 0x34, 0x5b],
        };

        let w: Vec<u16> = vhdx_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let storage = VIRTUAL_STORAGE_TYPE {
            DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
            VendorId: VENDOR_MICROSOFT,
        };

        let mut handle: HANDLE = INVALID_HANDLE_VALUE;
        let mut open: OPEN_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
        open.Version = 1; // OPEN_VIRTUAL_DISK_VERSION_1
        open.Anonymous.Version1.RWDepth = 1;
        let rc = unsafe {
            OpenVirtualDisk(
                &storage,
                w.as_ptr(),
                VIRTUAL_DISK_ACCESS_ATTACH_RW | VIRTUAL_DISK_ACCESS_GET_INFO,
                OPEN_VIRTUAL_DISK_FLAG_NONE,
                &open,
                &mut handle,
            )
        };
        if rc != 0 {
            bail!(
                "OpenVirtualDisk (RW) failed for {} (Win32 error {rc}, 0x{rc:08X})",
                vhdx_path.display()
            );
        }

        // Read-WRITE attach (no READ_ONLY flag), auto drive letters (no
        // NO_DRIVE_LETTER flag).
        let attach = ATTACH_VIRTUAL_DISK_PARAMETERS {
            Version: 1, // ATTACH_VIRTUAL_DISK_VERSION_1
            Anonymous: unsafe { std::mem::zeroed() },
        };
        let rc = unsafe {
            AttachVirtualDisk(
                handle,
                std::ptr::null_mut(),
                ATTACH_VIRTUAL_DISK_FLAG_NONE,
                0,
                &attach,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            unsafe { CloseHandle(handle) };
            bail!(
                "AttachVirtualDisk (RW) failed for {} (Win32 error {rc}, 0x{rc:08X}); \
                 mounting requires Administrator",
                vhdx_path.display()
            );
        }
        Ok(Self { handle })
    }

    /// `N` in `\\.\PhysicalDriveN` for the attached disk, for diagnostics.
    pub fn physical_drive_number(&self) -> Option<u32> {
        use windows_sys::Win32::Storage::Vhd::GetVirtualDiskPhysicalPath;
        let mut buf = [0u16; 256];
        let mut size_bytes = (buf.len() * 2) as u32;
        let rc =
            unsafe { GetVirtualDiskPhysicalPath(self.handle, &mut size_bytes, buf.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    }
}

impl Drop for AttachedRw {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// Clean up any leaked VHDs from a previous crashed run: detach every attached
/// disk whose location points into our scratch dir, then delete the files.
/// Safe to call at the start of a test run.
pub fn cleanup_leaked_vhds() -> Result<()> {
    let dir = scratch_dir()?;
    let dir_lc = dir.to_string_lossy().to_ascii_lowercase();
    for (_, loc) in disk_numbers_by_location()? {
        if loc.to_ascii_lowercase().starts_with(&dir_lc) {
            let _ = run_diskpart(&format!("select vdisk file=\"{loc}\"\ndetach vdisk\n"));
        }
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.path().extension().map(|x| x == "vhdx").unwrap_or(false) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

pub mod fixture;
pub use fixture::{fill_fixture, verify_fixture, FixtureDigest};

/// Console progress reporter for long-running engine operations in tests:
/// hand `handle()` to `BackupOptions`/`RestoreOptions` and a background
/// thread prints step transitions and byte progress to stderr (visible under
/// `--nocapture`). Dropping the reporter stops the thread, prints a total
/// elapsed/throughput summary, and appends a row to the perf log (see
/// [`perf_log_path`]) so run-over-run speed changes are measurable instead of
/// vibes.
///
/// Prints on: step/phase change (with the finished phase's elapsed time and
/// rate), every +1% (or +1 GiB, whichever is larger), and a 30 s heartbeat so
/// multi-hour phases never look hung.
pub struct ConsoleProgress {
    handle: phoenix_core::ProgressHandle,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    label: String,
    started: std::time::Instant,
}

impl ConsoleProgress {
    pub fn start(label: &str) -> Self {
        use std::sync::atomic::Ordering;
        let handle = phoenix_core::ProgressHandle::new();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let label_owned = label.to_string();
        let label = label.to_string();
        let h = handle.clone();
        let s = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut last_phase = String::new();
            let mut last_detail = String::new();
            let mut last_printed_bytes = 0u64;
            let mut last_print = std::time::Instant::now();
            // Per-phase timing: when the phase label changes, report how long
            // the finished phase took and how many bytes it moved. Sampled at
            // 1 Hz, so sub-second phases can be missed — fine for perf notes.
            let mut phase_started = std::time::Instant::now();
            let mut phase_start_bytes = 0u64;
            let mut prev_current = 0u64;
            let gb = |b: u64| b as f64 / 1e9;
            while !s.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(1000));
                let snap = h.snapshot();
                if !snap.active && snap.phase.is_empty() {
                    continue;
                }
                let step_changed = snap.phase != last_phase;
                let detail_changed = !snap.detail.is_empty() && snap.detail != last_detail;
                let byte_step = (snap.total / 100).max(1_000_000_000);
                let bytes_moved = snap.current.saturating_sub(last_printed_bytes) >= byte_step;
                let heartbeat = last_print.elapsed().as_secs() >= 30;
                if step_changed {
                    if !last_phase.is_empty() {
                        let secs = phase_started.elapsed().as_secs_f64();
                        let bytes = prev_current.saturating_sub(phase_start_bytes);
                        if bytes > 0 {
                            eprintln!(
                                "[{label}] time: {last_phase}: {} in {}",
                                phoenix_core::progress::format_rate(bytes, secs),
                                phoenix_core::progress::format_elapsed(secs),
                            );
                        } else {
                            eprintln!(
                                "[{label}] time: {last_phase}: {}",
                                phoenix_core::progress::format_elapsed(secs),
                            );
                        }
                    }
                    phase_started = std::time::Instant::now();
                    // A `begin()` between phases resets the byte counter; start
                    // the new phase's baseline from wherever the counter is now.
                    phase_start_bytes = if snap.current < prev_current {
                        snap.current
                    } else {
                        prev_current
                    };
                }
                if step_changed || detail_changed || bytes_moved || heartbeat {
                    let pct = if snap.total > 0 {
                        (snap.current as f64 / snap.total as f64 * 100.0).min(100.0)
                    } else {
                        0.0
                    };
                    eprintln!(
                        "[{label}] {} — {:.1} / {:.1} GB ({pct:.0}%){}",
                        if snap.phase.is_empty() {
                            "working"
                        } else {
                            &snap.phase
                        },
                        gb(snap.current),
                        gb(snap.total),
                        if snap.detail.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", snap.detail)
                        }
                    );
                    last_phase = snap.phase.clone();
                    last_detail = snap.detail.clone();
                    last_printed_bytes = snap.current;
                    last_print = std::time::Instant::now();
                }
                prev_current = snap.current;
            }
        });
        Self {
            handle,
            stop,
            thread: Some(thread),
            label: label_owned,
            started: std::time::Instant::now(),
        }
    }

    pub fn handle(&self) -> phoenix_core::ProgressHandle {
        self.handle.clone()
    }
}

impl Drop for ConsoleProgress {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // `ProgressHandle::end()` snaps `current` to `total`, so the final
        // snapshot still carries the operation's byte count. On an error path
        // (drop during unwind) it reflects however far the op got — the log
        // row is still honest about elapsed time and bytes moved.
        let snap = self.handle.snapshot();
        let secs = self.started.elapsed().as_secs_f64();
        eprintln!(
            "[{}] TOTAL: {} in {}",
            self.label,
            phoenix_core::progress::format_rate(snap.current, secs),
            phoenix_core::progress::format_elapsed(secs),
        );
        match append_perf_row(&self.label, secs, snap.current) {
            Ok(path) => eprintln!("[{}] perf row appended to {}", self.label, path.display()),
            Err(e) => eprintln!("[{}] perf-log append failed: {e}", self.label),
        }
    }
}

/// Where perf rows land: `PHOENIX_PERF_LOG` if set, else
/// `<workspace>/target/perf-log.csv` (survives across runs, dies with
/// `cargo clean` — export it somewhere durable if you care long-term).
pub fn perf_log_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PHOENIX_PERF_LOG") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("target")
        .join("perf-log.csv")
}

/// Append one operation's timing to the perf log (CSV, header written on
/// first use). `workers` records the pipeline width so runs with different
/// `PHOENIX_WORKERS` settings stay comparable. Returns the log path.
fn append_perf_row(label: &str, secs: f64, bytes: u64) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    let path = perf_log_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let write_header = !path.exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if write_header {
        writeln!(f, "timestamp,label,elapsed_s,bytes,mb_per_s,workers")?;
    }
    let mb_per_s = if secs > 0.0 {
        bytes as f64 / 1e6 / secs
    } else {
        0.0
    };
    writeln!(
        f,
        "{},{},{:.1},{},{:.1},{}",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        label,
        secs,
        bytes,
        mb_per_s,
        phoenix_core::pipeline::worker_count(),
    )?;
    Ok(path)
}

// ---- BitLocker test helpers (Windows Pro+; used by the T2 `bitlocker` and
// ---- T3 `real_disk` BitLocker scenarios) ----

/// `(VolumeStatus, LockStatus, ProtectionStatus)` for a mount point, via
/// `Get-BitLockerVolume`. Errors if the cmdlet can't see the volume.
pub fn bitlocker_status(letter: char) -> Result<(String, String, String)> {
    let out = powershell(&format!(
        "$v = Get-BitLockerVolume -MountPoint '{letter}:' -ErrorAction Stop; \
         \"$($v.VolumeStatus)|$($v.LockStatus)|$($v.ProtectionStatus)\""
    ))?;
    let line = out.trim().to_string();
    let f: Vec<&str> = line.split('|').collect();
    if f.len() != 3 {
        bail!("unexpected Get-BitLockerVolume output: {line}");
    }
    Ok((f[0].to_string(), f[1].to_string(), f[2].to_string()))
}

/// Encrypt a data volume with a password protector (used-space-only for
/// speed) and block until conversion reports `FullyEncrypted`.
pub fn enable_bitlocker_password(letter: char, password: &str) -> Result<()> {
    powershell(&format!(
        "$ErrorActionPreference = 'Stop'; \
         $pw = ConvertTo-SecureString '{password}' -AsPlainText -Force; \
         Enable-BitLocker -MountPoint '{letter}:' -PasswordProtector -Password $pw \
             -UsedSpaceOnly | Out-Null"
    ))
    .context("Enable-BitLocker")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        let (volume_status, _, _) = bitlocker_status(letter)?;
        if volume_status == "FullyEncrypted" {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("BitLocker conversion did not finish (status {volume_status})");
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Lock an encrypted volume (dismounting any open handles).
pub fn lock_bitlocker(letter: char) -> Result<()> {
    powershell(&format!(
        "Lock-BitLocker -MountPoint '{letter}:' -ForceDismount | Out-Null"
    ))
    .context("Lock-BitLocker")?;
    Ok(())
}

/// Unlock an encrypted volume with its password.
pub fn unlock_bitlocker_password(letter: char, password: &str) -> Result<()> {
    powershell(&format!(
        "$ErrorActionPreference = 'Stop'; \
         $pw = ConvertTo-SecureString '{password}' -AsPlainText -Force; \
         Unlock-BitLocker -MountPoint '{letter}:' -Password $pw | Out-Null"
    ))
    .context("Unlock-BitLocker")?;
    Ok(())
}

/// Read the 8-byte OEM/FS tag at offset 3 of a partition's first sector,
/// reading through the **physical disk** handle (`\\.\PhysicalDriveN`). This
/// bypasses the volume mount entirely, so it reports the on-disk truth
/// regardless of how Windows has the volume cached — the deterministic,
/// media-agnostic way to prove what a restore actually wrote. Returns e.g.
/// `"NTFS    "`, `"-FVE-FS-"` (BitLocker), or `"EXFAT   "`.
pub fn partition_boot_oem_tag(disk_index: u32, offset_bytes: u64) -> Result<String> {
    let path = format!(r"\\.\PhysicalDrive{disk_index}");
    // Ask the device for its logical sector size rather than assuming 512: on a
    // 4Kn disk the reader needs it to bounce this sub-sector read into an
    // aligned span, and hardcoding 512 here would make the probe itself the
    // thing that fails on the media we most want to check.
    let sector_size = phoenix_core::disk::enumerate_disks()
        .map_err(|e| anyhow!("enumerate disks: {e}"))?
        .into_iter()
        .find(|d| d.index == disk_index)
        .map(|d| d.sector_size)
        .ok_or_else(|| anyhow!("disk {disk_index} not found"))?;
    // Read one 512-byte sector at the partition offset. `length` only bounds
    // the reader; a single sector covers the OEM tag at bytes 3..11.
    let mut pr = phoenix_capture::PartitionReader::open_disk_partition(
        &path,
        offset_bytes,
        4096,
        sector_size,
    )
    .map_err(|e| anyhow!("open partition reader at offset {offset_bytes}: {e}"))?;
    let mut buf = [0u8; 512];
    let mut got = 0usize;
    while got < buf.len() {
        let n = phoenix_capture::BlockSource::read_at(&mut pr, got as u64, &mut buf[got..])
            .map_err(|e| anyhow!("read boot sector: {e}"))?;
        if n == 0 {
            break;
        }
        got += n;
    }
    if got < 11 {
        bail!("short read of boot sector at offset {offset_bytes} (got {got} bytes)");
    }
    Ok(String::from_utf8_lossy(&buf[3..11]).to_string())
}

/// For every partition on a disk, read its boot-sector OEM tag off the
/// physical device and return `(offset, size, tag)`. Lets a caller prove a
/// signature (e.g. BitLocker's `-FVE-FS-`) is present on the disk without
/// assuming which partition index it landed at — GPT data disks carry a
/// Microsoft Reserved (MSR) partition ahead of the data one, so "partition 0"
/// is not the volume of interest.
pub fn disk_partition_boot_tags(disk_index: u32) -> Result<Vec<(u64, u64, String)>> {
    let disks = phoenix_core::disk::enumerate_disks().map_err(|e| anyhow!("enumerate: {e}"))?;
    let disk = disks
        .iter()
        .find(|d| d.index == disk_index)
        .ok_or_else(|| anyhow!("disk {disk_index} not found"))?;
    let mut out = Vec::new();
    for p in &disk.partitions {
        let tag = partition_boot_oem_tag(disk_index, p.offset_bytes).unwrap_or_default();
        out.push((p.offset_bytes, p.size_bytes, tag));
    }
    Ok(out)
}

/// Force Windows to re-read a disk's volumes after raw sectors were written
/// underneath them. Restoring a raw ciphertext BitLocker image writes the
/// `-FVE-FS-` boot sector into a partition Windows already auto-mounted
/// (and cached the pre-write, non-BitLocker view of), so `Get-BitLockerVolume`
/// / `manage-bde` report the stale "Fully Decrypted, Unknown size" view even
/// though the on-disk bytes are a valid encrypted volume.
///
/// We dismount each mounted volume on the disk with `FSCTL_DISMOUNT_VOLUME`
/// (via [`phoenix_core::disk::dismount_volume`]); the next access re-mounts it
/// fresh and reads the on-disk truth — the same refresh a reboot or re-plug
/// gives in production. This works on removable USB media, unlike
/// `Set-Disk -IsOffline` / diskpart `offline disk`, which reject removable
/// disks. A storage-cache nudge follows to hurry re-enumeration along.
pub fn rescan_disk_volumes(disk_index: u32) -> Result<()> {
    let letters = disk_volume_letters(disk_index).unwrap_or_default();
    for l in letters {
        let vol = format!(r"\\.\{l}:");
        if let Err(e) = phoenix_core::disk::dismount_volume(&vol) {
            eprintln!("[rescan] dismount {vol} failed (continuing): {e}");
        }
    }
    // Best-effort re-enumeration nudge; ignore failures (the subsequent
    // letter-wait force-assigns and reachability-polls regardless).
    let _ = powershell(&format!(
        "Update-Disk -Number {disk_index} -ErrorAction SilentlyContinue"
    ));
    std::thread::sleep(std::time::Duration::from_millis(750));
    Ok(())
}

/// Wait for a disk's partition to get a drive letter WITHOUT requiring the
/// volume root to be reachable — a restored BitLocker-locked volume has a
/// letter but no browsable filesystem until it's unlocked, so
/// [`wait_for_restored_letter`] would time out on it.
pub fn wait_for_letter_even_if_locked(disk_index: u32, timeout_ms: u64) -> Option<char> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if let Some(l) = disk_volume_letters(disk_index)
            .ok()
            .and_then(|ls| ls.into_iter().next())
        {
            return Some(l);
        }
        if std::time::Instant::now() >= deadline {
            return assign_letter_to_largest(disk_index);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Run `chkdsk /scan` (read-only) on a drive letter and return an error if it
/// reports problems. Used after restore/clone to assert the filesystem is
/// consistent.
///
/// `chkdsk /scan` performs an *online* scan that requires a VSS shadow copy.
/// On a small or nearly-full volume that snapshot can't be created ("Insufficient
/// storage available to create ... the shadow copy", exit 11) — an environmental
/// limitation, not filesystem corruption. We treat that specific outcome as an
/// inconclusive skip rather than a failure, since the per-file `verify_fixture`
/// digest check is the authoritative correctness gate.
pub fn chkdsk_clean(letter: char) -> Result<()> {
    let out = Command::new("chkdsk")
        .arg(format!("{letter}:"))
        .arg("/scan")
        .output()
        .context("spawning chkdsk")?;
    if out.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    let snapshot_failure = stdout.contains("shadow copy")
        || stdout.contains("snapshot")
        || stdout.contains("insufficient storage");
    if snapshot_failure {
        eprintln!(
            "chkdsk {letter}: /scan could not create a VSS snapshot (small/full volume); \
             skipping the online scan and relying on the fixture digest for correctness."
        );
        return Ok(());
    }
    bail!(
        "chkdsk {letter}: /scan reported problems (exit {:?}):\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Run an **offline** `chkdsk /F /X` (force-dismount + full fix) on a drive
/// letter and fail unless the volume ends up clean. Unlike [`chkdsk_clean`]
/// (online `/scan`, which needs a VSS snapshot and is skipped on small/full
/// volumes), this dismounts the volume and always runs a full structural
/// check — the deeper post-restore / post-shrink NTFS validation that `/scan`
/// can't do on the small T3 volumes.
///
/// chkdsk exit codes: `0` = no problems. `1` = errors found and fixed. `>=2` =
/// could not complete the check.
///
/// A same-size restore should be `0` on the first pass. The **shrink**-
/// relocation path, by design, leaves trailing `$Bitmap` allocation slack for
/// `chkdsk` to reconcile (documented in `phoenix-capture/src/ntfs.rs` —
/// `validate_extents_fit` guarantees no real data lives past the new boundary,
/// so it's pure metadata cleanup). That surfaces here as `1` (errors fixed) on
/// the first pass. To keep this a real gate rather than a rubber stamp, when
/// the first pass reports `1` we run chkdsk **again** and require a clean `0`:
/// the one-time reconciliation must fully resolve the volume, proving nothing
/// deeper is wrong. `>=2`, or a second pass that still isn't clean, fails.
///
/// `/X` forces a dismount, so this never prompts or schedules a reboot-time
/// check (these are data volumes, never the system volume). The volume
/// remounts on next access.
pub fn chkdsk_offline_fix(letter: char) -> Result<()> {
    let run = |pass: u32| -> Result<i32> {
        let out = Command::new("chkdsk")
            .arg(format!("{letter}:"))
            .arg("/F")
            .arg("/X")
            .output()
            .with_context(|| format!("spawning offline chkdsk /F /X (pass {pass})"))?;
        let code = out.status.code().unwrap_or(-1);
        if code >= 2 {
            bail!(
                "offline chkdsk {letter}: /F /X could not complete (exit {code}, pass {pass}):\n{}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        Ok(code)
    };

    match run(1)? {
        0 => {
            eprintln!("chkdsk {letter}: /F /X clean on first pass (offline structural check)");
            Ok(())
        }
        _ => {
            // First pass fixed something (expected for shrink-relocation's
            // documented $Bitmap slack). Require the second pass to be clean.
            eprintln!(
                "chkdsk {letter}: /F /X reconciled metadata on first pass (expected for shrink); \
                 re-running to require a clean volume"
            );
            match run(2)? {
                0 => {
                    eprintln!("chkdsk {letter}: /F /X clean on second pass (reconciliation stuck)");
                    Ok(())
                }
                code => bail!(
                    "offline chkdsk {letter}: still reported fixes on the SECOND pass (exit \
                     {code}); a single reconciliation should have left the volume clean — the \
                     restore/shrink produced a persistently inconsistent filesystem"
                ),
            }
        }
    }
}

/// Force a drive letter onto the largest partition of a disk and return it.
/// After a restore the engine brings the disk online, but the mount manager
/// assigns letters asynchronously and can be slow — especially for a freshly
/// shrunk+relocated NTFS volume. This explicitly assigns one (idempotent: if a
/// letter already exists it's returned as-is) so tests don't depend on mountmgr
/// timing.
pub fn assign_letter_to_largest(disk_index: u32) -> Option<char> {
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         $p = Get-Partition -DiskNumber {disk_index} | Sort-Object Size -Descending | \
              Select-Object -First 1; \
         if ($p -and -not $p.DriveLetter) {{ \
             Add-PartitionAccessPath -DiskNumber {disk_index} \
                 -PartitionNumber $p.PartitionNumber -AssignDriveLetter \
         }}; \
         (Get-Partition -DiskNumber {disk_index} | Where-Object DriveLetter | \
          Sort-Object Size -Descending | Select-Object -First 1 -ExpandProperty DriveLetter)"
    );
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().chars().next()
}

/// Get a restored volume's drive letter, robust to mount-manager timing. Polls
/// for an auto-assigned letter whose root is reachable; if none appears within
/// the first half of `timeout_ms`, force-assigns one to the disk's largest
/// partition (a freshly shrunk+relocated NTFS volume is often slow to
/// auto-mount). Returns the reachable letter, or `None` on timeout.
pub fn wait_for_restored_letter(disk_index: u32, timeout_ms: u64) -> Option<char> {
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(timeout_ms);
    let force_after = start + std::time::Duration::from_millis(timeout_ms / 2);
    let mut forced = false;
    let reachable = |l: char| Path::new(&format!("{l}:\\")).exists();

    while std::time::Instant::now() < deadline {
        if let Some(l) = first_letter_on_disk(disk_index).filter(|&l| reachable(l)) {
            return Some(l);
        }
        if !forced && std::time::Instant::now() >= force_after {
            forced = true;
            if let Some(l) = assign_letter_to_largest(disk_index).filter(|&l| reachable(l)) {
                return Some(l);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assign_letter_to_largest(disk_index).filter(|&l| reachable(l))
}

/// Drive letter of the partition at an **exact byte offset**, waiting for it to
/// mount and assigning one if Windows doesn't.
///
/// [`wait_for_restored_letter`] returns the *first* lettered partition on the
/// disk, which is fine when the test just restored the whole disk and there is
/// only one candidate. It is actively wrong for a **partial** clone or restore:
/// the target keeps its other partitions, so "first letter on the disk" can just
/// as easily hand back the partition we were supposed to leave alone — and then
/// verifying the source's fixture against it would be a false pass, or a false
/// failure, depending on which way the dice fell.
///
/// Identify the slot by the one thing the plan actually pinned: its offset.
pub fn wait_for_letter_at_offset(
    disk_index: u32,
    offset_bytes: u64,
    timeout_ms: u64,
) -> Option<char> {
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(timeout_ms);
    let force_after = start + std::time::Duration::from_millis(timeout_ms / 2);
    let mut forced = false;
    let reachable = |l: char| Path::new(&format!("{l}:\\")).exists();

    let letter_at = || -> Option<char> {
        phoenix_core::disk::enumerate_disks()
            .ok()?
            .into_iter()
            .find(|d| d.index == disk_index)?
            .partitions
            .into_iter()
            .find(|p| p.offset_bytes == offset_bytes)?
            .drive_letter
    };

    while std::time::Instant::now() < deadline {
        if let Some(l) = letter_at().filter(|&l| reachable(l)) {
            return Some(l);
        }
        if !forced && std::time::Instant::now() >= force_after {
            forced = true;
            // Windows sometimes leaves a freshly-rewritten slot unmounted until
            // something pokes it; assign letters to any lettered-less partition
            // on the disk, then look again for OUR offset specifically.
            let _ = assign_letters_to_disk(disk_index);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    letter_at().filter(|&l| reachable(l))
}

/// Drive letter currently assigned to the first lettered partition of a
/// physical disk (via PowerShell `Get-Partition`), if any.
pub fn first_letter_on_disk(disk_index: u32) -> Option<char> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "(Get-Partition -DiskNumber {disk_index} | \
                  Where-Object DriveLetter | \
                  Select-Object -First 1 -ExpandProperty DriveLetter)"
            ),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().chars().next()
}

/// Wait (polling) for a drive letter to become available after a mount, up to
/// `timeout_ms`. Returns true if it appeared.
pub fn wait_for_letter(letter: char, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let root = format!("{letter}:\\");
    while std::time::Instant::now() < deadline {
        if Path::new(&root).exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}
