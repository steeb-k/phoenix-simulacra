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
    chkdsk_clean, cleanup_leaked_vhds, fill_fixture, fill_fixture_fragmented, require_admin,
    verify_fixture, wait_for_letter, wait_for_restored_letter, FixtureDigest, PartSpec, TestFs,
    TestVhd,
};

const MIB: u64 = 1024 * 1024;

/// Create an NTFS source VHD, fill it, and back up ALL its partitions.
fn backup_disk(source_mb: u64, seed: u64) -> (PathBuf, FixtureDigest) {
    backup_disk_with(source_mb, seed, false)
}

/// As [`backup_disk`], but `fragmented` swaps in the heavy-fragmentation
/// fixture whose one large file spans the whole volume.
fn backup_disk_with(source_mb: u64, seed: u64, fragmented: bool) -> (PathBuf, FixtureDigest) {
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
    let digest = if fragmented {
        fill_fixture_fragmented('X', seed, source_mb).expect("fill fragmented fixture")
    } else {
        fill_fixture('X', seed).expect("fill fixture")
    };

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
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");
    (backup_path, digest)
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
        convert_sector_size: false,
        repair_boot: false,
        progress: None,
    })
    .expect("run_restore");

    let letter = wait_for_restored_letter(target.disk_index(), 45_000)
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

/// Shrink a volume whose data is fragmented badly enough to overflow an MFT
/// record — the case that used to fail *after* streaming the whole partition.
///
/// The fixture leaves one large file spread over the entire 512 MiB volume in
/// hundreds of pieces, with everything else deleted. Shrinking to 300 MiB
/// therefore has to relocate roughly half of that file while its run lists are
/// already long enough to fill their records. Before the shrink pre-flight and
/// in-record attribute growth, this died with "relocated run list grew to N
/// bytes, past the N-1 byte attribute budget"; it must now come back
/// chkdsk-clean and byte-identical.
#[test]
#[ignore = "requires elevation + diskpart; run with --ignored --test-threads=1"]
fn ntfs_restore_shrink_heavily_fragmented() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let (backup, digest) = backup_disk_with(512, 0x3333, true);
    let target = TestVhd::create(300).expect("create target");
    restore_full_disk_and_verify(&backup, &target, 300 * MIB, &digest);
    let _ = std::fs::remove_file(&backup);
}

/// A shrink that cannot work must be refused *before* the target is touched.
///
/// The old code discovered an unfittable shrink only once the target's GPT had
/// been re-initialized and the data streamed; the point of the pre-flight is
/// that the target comes back byte-for-byte as it started.
///
/// The assertion deliberately does not pin the wording. A shrink can be
/// impossible for more than one reason, and they are caught by different
/// guards at different depths — here the used data simply exceeds the target,
/// which the cheap capacity check rejects long before the MFT pre-flight would
/// get a chance to compute a suggested size. The pre-flight's own
/// smallest-workable-target message is covered by unit tests in
/// `phoenix_restore::relocation`, which can construct that narrower case
/// without needing a real disk.
#[test]
#[ignore = "requires elevation + diskpart; run with --ignored --test-threads=1"]
fn ntfs_restore_impossible_shrink_leaves_the_target_untouched() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // The fragmented fixture leaves ~205 MiB of live data (80% of the volume
    // written as pairs, half of it deleted), which cannot fit a 100 MiB
    // partition under any layout.
    let (backup, _digest) = backup_disk_with(512, 0x4444, true);
    let target = TestVhd::create(100).expect("create target");

    let before = read_disk_head(&target);

    let reader = PhnxReader::open(&backup).unwrap();
    let plan = default_plan_from_backup(
        backup.to_str().unwrap(),
        &reader,
        target.disk_index(),
        100 * MIB,
    );
    drop(reader);

    let err = run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: false,
        convert_sector_size: false,
        repair_boot: false,
        progress: None,
    })
    .expect_err("shrinking ~205 MiB of data into 100 MiB must fail");

    // The headline property: nothing was written.
    let after = read_disk_head(&target);
    assert_eq!(
        before, after,
        "an impossible shrink must fail before writing anything to the target \
         (refusal was: {err})"
    );
    let _ = std::fs::remove_file(&backup);
}

/// First MiB of a target disk, for proving a failed restore wrote nothing.
fn read_disk_head(target: &TestVhd) -> Vec<u8> {
    use std::io::Read;
    let path = format!("\\\\.\\PhysicalDrive{}", target.disk_index());
    let mut f = std::fs::File::open(path).expect("open target disk for read-back");
    let mut buf = vec![0u8; MIB as usize];
    f.read_exact(&mut buf).expect("read target head");
    buf
}

/// Shrink an NTFS volume formatted with **64K clusters**.
///
/// Every other fixture in this suite takes NTFS's default allocation unit,
/// which is 4096 bytes for any volume up to 16 TB. That uniformity is what let
/// `plan_capture` get away with discarding the cluster size `ntfs_plan` had just
/// parsed and substituting a literal `4096`: for every disk the tests ever
/// built, the lie was true.
///
/// It is not true for a 64K-cluster volume — common on real large data volumes.
/// The wrong cluster size feeds `build_relocation_map`, so the shrink path
/// computed its relocation map at 1/16th the real cluster stride. This test is
/// the one fixture in the tree that can tell the difference.
#[test]
#[ignore = "requires elevation + diskpart; run with --ignored --test-threads=1"]
fn ntfs_restore_shrink_64k_clusters() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create(512).expect("create source vhd");
    source
        .init_gpt_with_cluster(
            &[PartSpec {
                size_mb: 0,
                fs: TestFs::Ntfs,
                letter: 'X',
                label: "SRC64K".into(),
            }],
            65536,
        )
        .expect("init source with 64K clusters");
    assert!(wait_for_letter('X', 15_000), "source volume never mounted");
    let digest = fill_fixture('X', 0x6464).expect("fill fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();

    let backup =
        std::env::temp_dir().join(format!("resize64k-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");

    // The manifest must carry the volume's REAL cluster size. If this reads 4096
    // the capture planner is lying about the source's geometry, and the shrink
    // below would be relocating clusters at the wrong stride — so assert it here,
    // where the failure names the actual cause, rather than waiting for chkdsk to
    // report damage it can't explain.
    let mut reader = PhnxReader::open(&backup).expect("open backup");
    let ntfs_entry = reader
        .index
        .iter()
        .find(|e| e.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .expect("no NTFS partition in backup")
        .clone();
    let stream = reader
        .read_stream_header(&ntfs_entry)
        .expect("read stream header");
    assert_eq!(
        stream.bytes_per_cluster, 65536,
        "manifest recorded {} bytes per cluster for a 64K-cluster volume — the capture \
         planner discarded the parsed cluster size",
        stream.bytes_per_cluster
    );
    drop(reader);

    // Now the part the wrong cluster size would actually corrupt: shrink it.
    let target = TestVhd::create(300).expect("create target");
    restore_full_disk_and_verify(&backup, &target, 300 * MIB, &digest);
    let _ = std::fs::remove_file(&backup);
}
