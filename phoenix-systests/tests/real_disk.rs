//! Tier-3 DESTRUCTIVE tests against a REAL physical USB disk.
//!
//! These WIPE the target disk repeatedly. They only run when opted in via the
//! `PHOENIX_T3_DISK` env var, and the harness ([`RealDisk::acquire`]) refuses
//! any disk that isn't USB / is the boot or system disk / is outside a sane USB
//! size — re-checking before every destructive step. Optionally pin the exact
//! device with `PHOENIX_T3_SERIAL`.
//!
//! Run (ELEVATED):
//! ```text
//! $env:PHOENIX_T3_DISK="2"; $env:PHOENIX_T3_SERIAL="04018bdbdd996130c3c9"
//! cargo test -p phoenix-systests --test real_disk -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! NOTE: Windows won't make a removable USB flash drive GPT, so these use MBR
//! layouts — which also fills a real coverage gap (the T2 VHD tests are all GPT).

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::default_plan_from_backup;
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    chkdsk_clean, fill_fixture, partition_summary, require_admin, verify_fixture,
    wait_for_disk_volumes, wait_for_letter, PartSpec, RealDisk, TestFs,
};

/// Acquire the opt-in target disk, or `None` to skip (env unset). A failed
/// safety gate panics rather than skipping.
fn skip_or_disk() -> Option<RealDisk> {
    match RealDisk::acquire() {
        Ok(Some(d)) => Some(d),
        Ok(None) => {
            eprintln!("PHOENIX_T3_DISK not set — skipping real-disk test");
            None
        }
        Err(e) => panic!("real-disk safety check failed: {e:#}"),
    }
}

fn disk_size_bytes(idx: u32) -> u64 {
    enumerate_disks()
        .unwrap()
        .iter()
        .find(|d| d.index == idx)
        .expect("target disk vanished")
        .size_bytes
}

fn all_partition_indices(idx: u32) -> Vec<u32> {
    enumerate_disks()
        .unwrap()
        .iter()
        .find(|d| d.index == idx)
        .unwrap()
        .partitions
        .iter()
        .map(|p| p.index)
        .collect()
}

#[test]
#[ignore = "DESTRUCTIVE: wipes the real USB disk; set PHOENIX_T3_DISK"]
fn real_mbr_multifs_roundtrip() {
    require_admin();
    let Some(disk) = skip_or_disk() else {
        return;
    };
    let idx = disk.index();

    // --- Lay out an MBR disk: NTFS + FAT32, fill deterministic fixtures ---
    disk.layout(
        false,
        &[
            PartSpec {
                size_mb: 2048,
                fs: TestFs::Ntfs,
                letter: 'R',
                label: "T3NTFS".into(),
            },
            PartSpec {
                size_mb: 1024,
                fs: TestFs::Fat32,
                letter: 'S',
                label: "T3FAT".into(),
            },
        ],
    )
    .expect("MBR layout");
    assert!(wait_for_letter('R', 20_000), "NTFS volume didn't mount");
    assert!(wait_for_letter('S', 20_000), "FAT volume didn't mount");
    let d_ntfs = fill_fixture('R', 0xAA01).expect("fill ntfs fixture");
    let d_fat = fill_fixture('S', 0xAA02).expect("fill fat fixture");

    let before = partition_summary(idx).expect("summary before");
    eprintln!("[T3] source partitions: {before:?}");
    let disk_size = disk_size_bytes(idx);

    // --- Back up every partition ---
    let parts = all_partition_indices(idx);
    eprintln!("[T3] backing up disk {idx} partitions {parts:?}");
    let backup =
        std::env::temp_dir().join(format!("t3-mbr-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: idx,
        partition_indices: parts,
        output: backup.clone(),
        use_vss: false,
        verify_after: true,
        progress: None,
    })
    .expect("run_backup");

    // --- Full-verify the backup before we destroy the source ---
    PhnxReader::open(&backup)
        .unwrap()
        .verify_all(false)
        .expect("backup fails full verification");

    // --- Wipe + full-disk restore back onto the same disk ---
    disk.clean().expect("clean disk");
    let reader = PhnxReader::open(&backup).unwrap();
    let plan = default_plan_from_backup(backup.to_str().unwrap(), &reader, idx, disk_size);
    for e in &plan.entries {
        eprintln!(
            "[T3] restore plan: src={:?} off={} size={} end={}",
            e.source_partition_index,
            e.target_offset_bytes,
            e.target_size_bytes,
            e.target_offset_bytes + e.target_size_bytes
        );
    }
    drop(reader);
    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");

    // --- Partition-table + data integrity ---
    let letters = wait_for_disk_volumes(idx, 2, 60_000).expect("restored volumes got letters");
    eprintln!("[T3] restored letters (offset order): {letters:?}");
    let after = partition_summary(idx).expect("summary after");
    eprintln!("[T3] restored partitions: {after:?}");
    assert_eq!(
        after.len(),
        before.len(),
        "restore changed the partition count"
    );

    // Offset order matches the layout order: [0] = NTFS, [1] = FAT32.
    chkdsk_clean(letters[0]).expect("chkdsk on restored NTFS");
    verify_fixture(letters[0], &d_ntfs).expect("NTFS fixture preserved across restore");
    verify_fixture(letters[1], &d_fat).expect("FAT32 fixture preserved across restore");

    let _ = std::fs::remove_file(&backup);
    eprintln!("[T3] real_mbr_multifs_roundtrip PASSED");
}
