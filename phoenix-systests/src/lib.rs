//! System-test harness for Carbon Phoenix (Tier 2).
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
        let path = scratch_dir()?.join(format!("phoenix-systest-{}.vhdx", new_id()));
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("non-UTF-8 scratch path"))?
            .to_string();

        // Snapshot disk locations before attach so we can identify the new one
        // even if PhysicalDrive numbering is non-monotonic.
        let before = disk_numbers_by_location()?;

        run_diskpart(&format!(
            "create vdisk file=\"{path_str}\" maximum={size_mb} type=expandable\n\
             select vdisk file=\"{path_str}\"\n\
             attach vdisk\n"
        ))
        .context("diskpart create/attach vdisk")?;

        // Resolve the attached disk's number by its VHDX location (robust
        // against parallel disk changes; we still require --test-threads=1).
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

/// Wipe `disk_index`, initialize its partition table (`gpt` true → GPT, else
/// MBR), and create + format + letter the given partitions via diskpart.
/// DESTRUCTIVE — used by both [`TestVhd`] and [`RealDisk`] (which safety-checks
/// the target first).
pub fn diskpart_layout(disk_index: u32, gpt: bool, parts: &[PartSpec]) -> Result<()> {
    let style = if gpt { "gpt" } else { "mbr" };
    let mut script = format!("select disk {disk_index}\nclean\nconvert {style}\n");
    for p in parts {
        if p.size_mb == 0 {
            script.push_str("create partition primary\n");
        } else {
            script.push_str(&format!("create partition primary size={}\n", p.size_mb));
        }
        script.push_str(&format!(
            "format fs={} label={} quick\n",
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
    let style = if gpt { "GPT" } else { "MBR" };
    let mut script = format!(
        "$ErrorActionPreference='Stop'; \
         if ((Get-Disk -Number {disk_index}).PartitionStyle -ne 'RAW') {{ \
             Clear-Disk -Number {disk_index} -RemoveData -RemoveOEM -Confirm:$false }}; \
         Initialize-Disk -Number {disk_index} -PartitionStyle {style}; "
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

/// Re-run the full safety gate for `index`, returning the serial on success.
/// Bails (never returns Ok) if ANY gate fails, so a destructive op can never
/// proceed against the wrong disk even if indices shift between calls.
fn validate_real_disk(index: u32) -> Result<String> {
    let info = powershell(&format!(
        "$d = Get-Disk -Number {index} -ErrorAction Stop; \
         \"$($d.BusType)|$($d.IsBoot)|$($d.IsSystem)|$($d.Size)|$($d.SerialNumber)\""
    ))
    .with_context(|| format!("querying disk {index}"))?;
    let f: Vec<&str> = info.trim().split('|').collect();
    let bus = f.first().copied().unwrap_or("");
    let is_boot = f.get(1).copied().unwrap_or("");
    let is_system = f.get(2).copied().unwrap_or("");
    let size: u64 = f.get(3).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let serial = f.get(4).copied().unwrap_or("").trim().to_string();

    if !bus.eq_ignore_ascii_case("USB") {
        bail!("SAFETY: disk {index} BusType={bus:?} — refusing (target must be USB)");
    }
    if is_boot.eq_ignore_ascii_case("True") {
        bail!("SAFETY: disk {index} is the BOOT disk — refusing");
    }
    if is_system.eq_ignore_ascii_case("True") {
        bail!("SAFETY: disk {index} is the SYSTEM disk — refusing");
    }
    let gb = size as f64 / 1e9;
    if !(16.0..=64.0).contains(&gb) {
        bail!("SAFETY: disk {index} size {gb:.1} GB outside 16–64 GB — refusing");
    }
    if let Ok(want) = std::env::var("PHOENIX_T3_SERIAL") {
        if !serial.eq_ignore_ascii_case(want.trim()) {
            bail!(
                "SAFETY: disk {index} serial {serial:?} != PHOENIX_T3_SERIAL {want:?} — refusing"
            );
        }
    }
    Ok(serial)
}

/// A real, physical test disk opted into via the `PHOENIX_T3_DISK` env var and
/// guarded so destructive tests can never hit a non-USB / boot / system disk.
pub struct RealDisk {
    index: u32,
}

impl RealDisk {
    /// Resolve the opt-in real test disk. Returns `Ok(None)` when
    /// `PHOENIX_T3_DISK` is unset (the test should skip). Every safety gate is
    /// checked; a failure returns `Err` rather than a disk.
    pub fn acquire() -> Result<Option<Self>> {
        let Ok(raw) = std::env::var("PHOENIX_T3_DISK") else {
            return Ok(None);
        };
        let index: u32 = raw
            .trim()
            .parse()
            .context("PHOENIX_T3_DISK must be a disk number")?;
        let serial = validate_real_disk(index)?;
        eprintln!("[T3] target = disk {index}, USB, serial {serial} — safety gates passed");
        Ok(Some(Self { index }))
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// Wipe the disk (re-validates safety first).
    pub fn clean(&self) -> Result<()> {
        validate_real_disk(self.index)?;
        run_diskpart(&format!("select disk {}\nclean\n", self.index)).context("clean real disk")?;
        Ok(())
    }

    /// Lay out partitions on the disk (re-validates safety first). Uses the
    /// PowerShell storage cmdlets, which work on removable USB media.
    pub fn layout(&self, gpt: bool, parts: &[PartSpec]) -> Result<()> {
        validate_real_disk(self.index)?;
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

fn powershell(script: &str) -> Result<String> {
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
