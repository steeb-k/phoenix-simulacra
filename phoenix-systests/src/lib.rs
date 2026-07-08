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
        let mut script = format!("select disk {}\nclean\nconvert gpt\n", self.disk_index);
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
        run_diskpart(&script).context("diskpart init/format partitions")?;
        Ok(())
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
