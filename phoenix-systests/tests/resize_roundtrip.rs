//! Tier-2 system tests for restore WITH resizing: growing an NTFS partition
//! into a larger target, and shrinking one into a smaller target. The shrink
//! path exercises the relocation map + NTFS metadata rewrite in the engine.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests -- --ignored --test-threads=1

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::{RestorePlan, RestorePlanEntry};
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    chkdsk_clean, cleanup_leaked_vhds, fill_fixture, first_letter_on_disk, require_admin,
    verify_fixture, wait_for_letter, PartSpec, TestFs, TestVhd,
};

const MIB: u64 = 1024 * 1024;

/// Back up the single NTFS partition of a freshly-filled `source_mb` disk to a
/// temp .phnx, returning (backup_path, fixture_digest, source_used_bytes).
fn backup_single_ntfs(
    source_mb: u64,
    seed: u64,
) -> (std::path::PathBuf, phoenix_systests::FixtureDigest, u64) {
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
    let used: u64 = disk.partitions.iter().map(|p| p.size_bytes).sum();

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
    (backup_path, digest, used)
}

/// Build a single-partition plan placing source partition 0 at 1 MiB with the
/// chosen target size.
fn single_partition_plan(
    backup_path: &std::path::Path,
    target_disk: u32,
    target_size: u64,
) -> RestorePlan {
    RestorePlan {
        backup_path: backup_path.to_string_lossy().to_string(),
        target_disk_index: target_disk,
        full_disk: false,
        entries: vec![RestorePlanEntry {
            source_partition_index: Some(0),
            target_partition_index: 0,
            restore: true,
            target_offset_bytes: MIB,
            target_size_bytes: target_size,
        }],
    }
}

fn restore_and_verify(
    backup_path: &std::path::Path,
    target: &TestVhd,
    target_size: u64,
    digest: &phoenix_systests::FixtureDigest,
) {
    let plan = single_partition_plan(backup_path, target.disk_index(), target_size);
    run_restore(RestoreOptions {
        backup_path: backup_path.to_path_buf(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");

    let letter = first_letter_on_disk(target.disk_index()).expect("restored partition has letter");
    assert!(
        wait_for_letter(letter, 30_000),
        "restored volume never mounted"
    );
    chkdsk_clean(letter).expect("chkdsk clean");
    verify_fixture(letter, digest).expect("fixture preserved");
}

#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_restore_grow() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source 256 MiB NTFS -> restore into a ~1 GiB partition (grow).
    let (backup_path, digest, _used) = backup_single_ntfs(256, 0x1111);
    let target = TestVhd::create(1100).expect("create target");
    // Leave 1 MiB lead + a little tail slack under the disk end.
    let target_size = 1024 * MIB;
    restore_and_verify(&backup_path, &target, target_size, &digest);
    let _ = std::fs::remove_file(&backup_path);
}

#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_restore_shrink() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source 512 MiB NTFS with a modest fixture -> restore into a 300 MiB
    // partition. The fixture is far smaller than 300 MiB, so it fits; the
    // engine still runs the shrink path (relocation map + metadata rewrite)
    // whenever target_size < original_size.
    let (backup_path, digest, _used) = backup_single_ntfs(512, 0x2222);
    let target = TestVhd::create(400).expect("create target");
    let target_size = 300 * MIB;
    restore_and_verify(&backup_path, &target, target_size, &digest);
    let _ = std::fs::remove_file(&backup_path);
}
