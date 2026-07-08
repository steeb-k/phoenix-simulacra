//! Tier-2 system test: back up a real NTFS volume on an attached VHDX and
//! restore it to a second VHDX, asserting the filesystem is consistent
//! (`chkdsk /scan`) and every file's bytes survived (fixture digest).
//!
//! Requires an elevated shell. Run with:
//!   cargo test -p phoenix-systests -- --ignored --test-threads=1

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::default_plan_from_backup;
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    chkdsk_clean, cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture,
    wait_for_letter, wait_for_restored_letter, PartSpec, TestFs, TestVhd,
};

#[test]
#[ignore = "requires elevation + diskpart; run with --ignored --test-threads=1"]
fn ntfs_backup_restore_roundtrip_same_size() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // --- Source: 512 MiB disk, one NTFS partition, filled with a fixture ---
    let source = TestVhd::create(512).expect("create source vhd");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0, // rest of disk
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SRC".into(),
        }])
        .expect("init source gpt");
    assert!(wait_for_letter('X', 15_000), "source volume never mounted");
    let digest = fill_fixture('X', 0xC0FFEE).expect("fill fixture");

    // --- Back it up ---
    let disks = enumerate_disks().expect("enumerate disks");
    let src_disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .expect("source disk enumerated");
    let part_indices: Vec<u32> = src_disk.partitions.iter().map(|p| p.index).collect();
    assert!(!part_indices.is_empty(), "source disk has no partitions");

    let backup_path = std::env::temp_dir().join(format!(
        "phoenix-systest-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: part_indices,
        output: backup_path.clone(),
        use_vss: false, // volume is idle; no snapshot needed
        progress: None,
    })
    .expect("run_backup");

    // --- Restore to a fresh, same-size target ---
    let target = TestVhd::create(512).expect("create target vhd");
    let reader = PhnxReader::open(&backup_path).expect("open backup");
    let plan = default_plan_from_backup(
        backup_path.to_str().unwrap(),
        &reader,
        target.disk_index(),
        512 * 1024 * 1024,
    );
    drop(reader);

    run_restore(RestoreOptions {
        backup_path: backup_path.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");

    // --- Verify the restored volume ---
    let letter = wait_for_restored_letter(target.disk_index(), 30_000)
        .expect("restored volume got no drive letter");
    chkdsk_clean(letter).expect("chkdsk clean on restored volume");
    verify_fixture(letter, &digest).expect("fixture bytes preserved");

    let _ = std::fs::remove_file(&backup_path);
}
