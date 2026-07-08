//! Tier-2 regression tests for the FAT-family fixes (FAT32 cluster count,
//! exFAT geometry, EOC-cluster capture): back up a real FAT32 and a real
//! exFAT volume and restore them, verifying every file's bytes survive.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests -- --ignored --test-threads=1

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::default_plan_from_backup;
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    chkdsk_clean, cleanup_leaked_vhds, fill_fixture, first_letter_on_disk, require_admin,
    verify_fixture, wait_for_letter, PartSpec, TestFs, TestVhd,
};

fn roundtrip_fs(fs: TestFs, size_mb: u64, seed: u64) {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create(size_mb).expect("create source vhd");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs,
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
        std::env::temp_dir().join(format!("fatfam-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        use_vss: false,
        progress: None,
    })
    .expect("run_backup");

    let target = TestVhd::create(size_mb).expect("create target vhd");
    let reader = PhnxReader::open(&backup_path).unwrap();
    let target_disk_size = size_mb * 1024 * 1024;
    let plan = default_plan_from_backup(
        backup_path.to_str().unwrap(),
        &reader,
        target.disk_index(),
        target_disk_size,
    );
    drop(reader);
    run_restore(RestoreOptions {
        backup_path: backup_path.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");

    let letter = first_letter_on_disk(target.disk_index()).expect("restored letter");
    assert!(
        wait_for_letter(letter, 30_000),
        "restored volume never mounted"
    );
    chkdsk_clean(letter).expect("chkdsk clean");
    verify_fixture(letter, &digest).expect("fixture preserved");
    let _ = std::fs::remove_file(&backup_path);
}

#[test]
#[ignore = "requires elevation + diskpart"]
fn fat32_backup_restore_roundtrip() {
    // FAT32 needs >= ~33 MiB to form; use 512 MiB to be safely FAT32.
    roundtrip_fs(TestFs::Fat32, 512, 0x3333);
}

#[test]
#[ignore = "requires elevation + diskpart"]
fn exfat_backup_restore_roundtrip() {
    roundtrip_fs(TestFs::Exfat, 512, 0x4444);
}
