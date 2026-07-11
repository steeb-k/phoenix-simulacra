//! Tier-2 system tests for PARTIAL restores that rewrite the target's
//! partition table — the engine work behind the GUI layout editor:
//!
//!   * MBR partial with a resized slot: the table entry must shrink to the
//!     planned size while an untouched sibling partition survives verbatim.
//!   * MBR partial that DELETES a live partition: the slot vanishes from the
//!     table (its mounted volume gets locked + dismounted first) and the
//!     sibling survives.
//!   * `reinit_style = "gpt"` partial plan: an MBR disk is re-initialized as
//!     GPT and the restored partition lands on the fresh table.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests --test partial_mbr -- --ignored --test-threads=1

use std::path::{Path, PathBuf};
use std::process::Command;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::{RestorePlan, RestorePlanEntry};
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    chkdsk_clean, cleanup_leaked_vhds, diskpart_layout, fill_fixture, require_admin,
    verify_fixture, wait_for_letter, FixtureDigest, PartSpec, TestFs, TestVhd,
};

const MIB: u64 = 1024 * 1024;

/// Create a single-NTFS **MBR** source VHD, fill it, back it up, and return
/// (backup path, fixture digest, source partition index, source size).
fn backup_mbr_disk(source_mb: u64, seed: u64) -> (PathBuf, FixtureDigest, u32, u64) {
    let source = TestVhd::create(source_mb).expect("create source vhd");
    diskpart_layout(
        source.disk_index(),
        false,
        &[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SRC".into(),
        }],
    )
    .expect("init MBR source");
    assert!(wait_for_letter('X', 15_000), "source volume never mounted");
    let digest = fill_fixture('X', seed).expect("fill fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();

    let backup_path = std::env::temp_dir().join(format!(
        "partial-mbr-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        use_vss: false,
        verify_after: true,
        progress: None,
    })
    .expect("run_backup");

    let reader = PhnxReader::open(&backup_path).unwrap();
    let entry = reader.index.first().expect("backup has one partition");
    let (src_index, src_size) = (entry.index, entry.original_size);
    (backup_path, digest, src_index, src_size)
}

/// Drive letter of the partition at `offset` on `disk_index`, polling for
/// mount-manager lag; force-assigns via PowerShell halfway through the
/// timeout (a freshly restored volume often gets no automatic letter).
fn wait_letter_at_offset(disk_index: u32, offset: u64, timeout_ms: u64) -> Option<char> {
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(timeout_ms);
    let force_after = start + std::time::Duration::from_millis(timeout_ms / 2);
    let mut forced = false;
    let reachable = |l: char| Path::new(&format!("{l}:\\")).exists();

    while std::time::Instant::now() < deadline {
        if let Ok(disks) = enumerate_disks() {
            if let Some(l) = disks
                .iter()
                .find(|d| d.index == disk_index)
                .and_then(|d| d.partitions.iter().find(|p| p.offset_bytes == offset))
                .and_then(|p| p.drive_letter)
                .filter(|&l| reachable(l))
            {
                return Some(l);
            }
        }
        if !forced && std::time::Instant::now() >= force_after {
            forced = true;
            let _ = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &format!(
                        "Get-Partition -DiskNumber {disk_index} | \
                         Where-Object Offset -eq {offset} | \
                         Add-PartitionAccessPath -AssignDriveLetter"
                    ),
                ])
                .output();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    None
}

fn partial_plan(backup: &Path, target_disk: u32, entries: Vec<RestorePlanEntry>) -> RestorePlan {
    RestorePlan {
        backup_path: backup.to_str().unwrap().to_string(),
        target_disk_index: target_disk,
        entries,
        full_disk: false,
        reinit_style: None,
    }
}

/// Restore a source partition into an MBR target's first slot at the SOURCE
/// size (smaller than the 300 MiB slot) — the partition-table entry must
/// shrink to match, and the second (preserved) partition must survive with
/// its data and letter intact.
#[test]
#[ignore = "requires elevation + diskpart"]
fn mbr_partial_resize_rewrites_table() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let (backup, digest_src, src_index, src_size) = backup_mbr_disk(128, 0x3311);

    let target = TestVhd::create(600).expect("create target");
    diskpart_layout(
        target.disk_index(),
        false,
        &[
            PartSpec {
                size_mb: 300,
                fs: TestFs::Ntfs,
                letter: 'T',
                label: "TGT1".into(),
            },
            PartSpec {
                size_mb: 0,
                fs: TestFs::Ntfs,
                letter: 'U',
                label: "KEEP".into(),
            },
        ],
    )
    .expect("init MBR target");
    assert!(wait_for_letter('U', 15_000), "target volumes never mounted");
    let digest_keep = fill_fixture('U', 0x4422).expect("fill preserved fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == target.disk_index())
        .unwrap();
    let mut parts: Vec<_> = disk.partitions.iter().collect();
    parts.sort_by_key(|p| p.offset_bytes);
    let (slot, keep) = (parts[0], parts[1]);
    assert!(src_size < slot.size_bytes, "test needs a smaller source");

    let plan = partial_plan(
        &backup,
        target.disk_index(),
        vec![
            RestorePlanEntry {
                source_partition_index: Some(src_index),
                target_partition_index: slot.index,
                restore: true,
                target_offset_bytes: slot.offset_bytes,
                target_size_bytes: src_size, // shrink the slot to the source
            },
            RestorePlanEntry {
                source_partition_index: None,
                target_partition_index: keep.index,
                restore: false,
                target_offset_bytes: keep.offset_bytes,
                target_size_bytes: keep.size_bytes,
            },
        ],
    );
    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");

    // The table must now carry the SHRUNK slot.
    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == target.disk_index())
        .unwrap();
    assert!(!disk.is_gpt, "target must remain MBR");
    let restored = disk
        .partitions
        .iter()
        .find(|p| p.offset_bytes == slot.offset_bytes)
        .expect("restored slot still in table");
    assert_eq!(
        restored.size_bytes, src_size,
        "partition table entry was not resized"
    );

    let letter = wait_letter_at_offset(target.disk_index(), slot.offset_bytes, 45_000)
        .expect("restored volume got no letter");
    chkdsk_clean(letter).expect("restored chkdsk clean");
    verify_fixture(letter, &digest_src).expect("restored fixture preserved");

    chkdsk_clean('U').expect("preserved chkdsk clean");
    verify_fixture('U', &digest_keep).expect("preserved fixture intact");

    let _ = std::fs::remove_file(&backup);
}

/// A partial plan that simply OMITS a live partition deletes it: the entry
/// disappears from the MBR table (after its mounted volume is locked +
/// dismounted) while the remaining partition survives untouched.
#[test]
#[ignore = "requires elevation + diskpart"]
fn mbr_partial_delete_removes_partition() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Any backup satisfies the plan reader; nothing is restored.
    let (backup, _digest_src, _src_index, _src_size) = backup_mbr_disk(128, 0x5533);

    let target = TestVhd::create(400).expect("create target");
    diskpart_layout(
        target.disk_index(),
        false,
        &[
            PartSpec {
                size_mb: 100,
                fs: TestFs::Ntfs,
                letter: 'T',
                label: "DEL".into(),
            },
            PartSpec {
                size_mb: 0,
                fs: TestFs::Ntfs,
                letter: 'U',
                label: "KEEP".into(),
            },
        ],
    )
    .expect("init MBR target");
    assert!(wait_for_letter('U', 15_000), "target volumes never mounted");
    let digest_keep = fill_fixture('U', 0x6644).expect("fill preserved fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == target.disk_index())
        .unwrap();
    let mut parts: Vec<_> = disk.partitions.iter().collect();
    parts.sort_by_key(|p| p.offset_bytes);
    let (doomed, keep) = (parts[0], parts[1]);

    // One entry only: the preserved partition. The first slot is deleted.
    let plan = partial_plan(
        &backup,
        target.disk_index(),
        vec![RestorePlanEntry {
            source_partition_index: None,
            target_partition_index: keep.index,
            restore: false,
            target_offset_bytes: keep.offset_bytes,
            target_size_bytes: keep.size_bytes,
        }],
    );
    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == target.disk_index())
        .unwrap();
    assert!(!disk.is_gpt, "target must remain MBR");
    assert!(
        !disk
            .partitions
            .iter()
            .any(|p| p.offset_bytes == doomed.offset_bytes),
        "deleted partition still present in the table"
    );
    assert!(
        disk.partitions
            .iter()
            .any(|p| p.offset_bytes == keep.offset_bytes),
        "preserved partition vanished"
    );

    chkdsk_clean('U').expect("preserved chkdsk clean");
    verify_fixture('U', &digest_keep).expect("preserved fixture intact");

    let _ = std::fs::remove_file(&backup);
}

/// `reinit_style = "gpt"` on a partial plan: an MBR target is re-initialized
/// as GPT (blank-layout path in the GUI) and the restored partition comes up
/// clean on the fresh table.
#[test]
#[ignore = "requires elevation + diskpart"]
fn mbr_to_gpt_reinit_partial_restore() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let (backup, digest_src, src_index, src_size) = backup_mbr_disk(128, 0x7755);

    let target = TestVhd::create(300).expect("create target");
    diskpart_layout(
        target.disk_index(),
        false,
        &[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'T',
            label: "OLD".into(),
        }],
    )
    .expect("init MBR target");
    assert!(wait_for_letter('T', 15_000), "target volume never mounted");

    let offset = MIB; // fresh slot on the blank GPT layout
    let mut plan = partial_plan(
        &backup,
        target.disk_index(),
        vec![RestorePlanEntry {
            source_partition_index: Some(src_index),
            target_partition_index: 0,
            restore: true,
            target_offset_bytes: offset,
            target_size_bytes: src_size,
        }],
    );
    plan.reinit_style = Some("gpt".into());
    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == target.disk_index())
        .unwrap();
    assert!(disk.is_gpt, "target was not re-initialized as GPT");
    assert_eq!(disk.partitions.len(), 1, "expected exactly the fresh slot");

    let letter = wait_letter_at_offset(target.disk_index(), offset, 45_000)
        .expect("restored volume got no letter");
    chkdsk_clean(letter).expect("restored chkdsk clean");
    verify_fixture(letter, &digest_src).expect("restored fixture preserved");

    let _ = std::fs::remove_file(&backup);
}
