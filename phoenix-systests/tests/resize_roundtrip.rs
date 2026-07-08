//! Tier-2 system tests for restore WITH resizing. Restoring a full-disk backup
//! onto a LARGER target grows the NTFS partition (boot-sector left at source
//! size + FSCTL_EXTEND_VOLUME after mount); onto a SMALLER target it shrinks it
//! (relocation map + MFT / $Bitmap / $LogFile rewrite in ntfs_meta.rs). Both
//! must come up chkdsk-clean with every fixture file byte-identical.
//!
//! These use a full-disk plan (default_plan_from_backup), which initializes the
//! target GPT and resizes the data partition to fit the target disk. A partial
//! plan would NOT initialize the GPT on a blank target, so no volume would
//! mount. The test disks carry an MSR partition (index 0) plus the NTFS data
//! partition, exactly as diskpart lays them out.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests -- --ignored --test-threads=1

use std::path::{Path, PathBuf};

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::default_plan_from_backup;
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    chkdsk_clean, cleanup_leaked_vhds, fill_fixture, first_letter_on_disk, require_admin,
    verify_fixture, wait_for_letter, FixtureDigest, PartSpec, TestFs, TestVhd,
};

const MIB: u64 = 1024 * 1024;

/// Create an NTFS source VHD, fill it, and back up ALL its partitions.
fn backup_disk(source_mb: u64, seed: u64) -> (PathBuf, FixtureDigest) {
    let source = TestVhd::create(source_mb).expect("create source vhd");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source volume never mounted");
    let digest = fill_fixture('X', seed).expect("fill fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();

    let backup_path =
        std::env::temp_dir().join(format!("resize-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        use_vss: false,
        progress: None,
    })
    .expect("run_backup");
    (backup_path, digest)
}

/// Poll for the first drive letter to appear on a disk after a restore (the
/// mount manager assigns it asynchronously once the disk is brought online).
fn wait_for_disk_letter(disk_index: u32, timeout_ms: u64) -> Option<char> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if let Some(l) = first_letter_on_disk(disk_index) {
            if Path::new(&format!("{l}:\\")).exists() {
                return Some(l);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    None
}

/// Restore the backup as a full-disk layout onto `target` (whose size drives
/// the NTFS grow/shrink), then assert the restored NTFS volume is consistent
/// and its files are byte-identical.
fn restore_full_disk_and_verify(
    backup_path: &Path,
    target: &TestVhd,
    target_disk_size: u64,
    digest: &FixtureDigest,
) {
    let reader = PhnxReader::open(backup_path).unwrap();
    let plan = default_plan_from_backup(
        backup_path.to_str().unwrap(),
        &reader,
        target.disk_index(),
        target_disk_size,
    );
    for e in &plan.entries {
        eprintln!(
            "  resize plan: src={:?} offset={} size={} end={}",
            e.source_partition_index,
            e.target_offset_bytes,
            e.target_size_bytes,
            e.target_offset_bytes + e.target_size_bytes
        );
    }
    drop(reader);

    run_restore(RestoreOptions {
        backup_path: backup_path.to_path_buf(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");

    let letter = wait_for_disk_letter(target.disk_index(), 45_000)
        .expect("restored NTFS volume got no letter");
    chkdsk_clean(letter).expect("chkdsk clean");
    verify_fixture(letter, digest).expect("fixture preserved");
}

#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_restore_grow() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source 256 MiB -> restore full-disk onto a 768 MiB target: the NTFS data
    // partition expands to fill the extra space.
    let (backup, digest) = backup_disk(256, 0x1111);
    let target = TestVhd::create(768).expect("create target");
    restore_full_disk_and_verify(&backup, &target, 768 * MIB, &digest);
    let _ = std::fs::remove_file(&backup);
}

#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_restore_shrink() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source 512 MiB -> restore full-disk onto a 300 MiB target: the NTFS data
    // partition shrinks (relocation + metadata rewrite). The fixture is far
    // smaller than 300 MiB, so its data fits below the new boundary.
    let (backup, digest) = backup_disk(512, 0x2222);
    let target = TestVhd::create(300).expect("create target");
    restore_full_disk_and_verify(&backup, &target, 300 * MIB, &digest);
    let _ = std::fs::remove_file(&backup);
}
