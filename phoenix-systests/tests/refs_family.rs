//! Tier-2 ReFS coverage: back up a real ReFS volume (used-block capture via
//! FSCTL_GET_VOLUME_BITMAP), restore it, and prove Windows mounts the result
//! as ReFS with every fixture file byte-identical. Also pins the two resize
//! policies: shrink is refused outright, grow extends via FSCTL_EXTEND_VOLUME.
//!
//! ReFS **formatting** is SKU-gated (Pro for Workstations / Enterprise /
//! Server; plain Pro gained it on recent Win11 builds). Every test here first
//! tries to lay the ReFS fixture down and, if that fails, re-formats the same
//! disk NTFS to tell "this edition can't format ReFS" (→ SKIP, test passes as
//! a no-op with a loud message) apart from "the VHD plumbing is broken"
//! (→ fail). Reading/mounting ReFS works on every SKU, so once the fixture
//! exists the rest of the test has no SKU dependency.
//!
//! No `chkdsk` leg: chkdsk's offline repair semantics don't apply to ReFS
//! (it's an online-self-healing filesystem and chkdsk mostly declines to do
//! anything). The Get-Volume filesystem assertion + per-file BLAKE3 fixture
//! verify are the consistency gates instead.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests --test refs_family -- --ignored --test-threads=1

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::default_plan_from_backup;
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, partition_boot_oem_tag, require_admin, verify_fixture,
    volume_filesystem, wait_for_letter, wait_for_restored_letter, PartSpec, TestFs, TestVhd,
};

const MIB: u64 = 1024 * 1024;
/// ReFS refuses small volumes; 2 GiB clears the floor comfortably on every
/// build we know of while the expandable VHDX keeps the on-host cost tiny.
const REFS_SIZE_MB: u64 = 2048;

/// Lay out `vhd` as one ReFS partition with the given allocation unit.
/// Returns false (after printing a loud SKIP) when this Windows edition
/// cannot format ReFS — proven by re-formatting the identical layout as NTFS,
/// which must succeed; if even NTFS fails the harness is broken and we panic.
fn refs_layout_or_skip(vhd: &TestVhd, letter: char, label: &str, cluster: u32) -> bool {
    let refs_spec = [PartSpec {
        size_mb: 0,
        fs: TestFs::Refs,
        letter,
        label: label.into(),
    }];
    let refs_err = match vhd.init_gpt_with_cluster(&refs_spec, cluster) {
        Ok(()) => return true,
        Err(e) => e,
    };
    let ntfs_spec = [PartSpec {
        size_mb: 0,
        fs: TestFs::Ntfs,
        letter,
        label: label.into(),
    }];
    match vhd.init_gpt_with(&ntfs_spec) {
        Ok(()) => {
            eprintln!(
                "[refs] SKIP: this Windows edition/build cannot format ReFS (the same \
                 layout formats fine as NTFS). ReFS formatting needs Pro for Workstations, \
                 Enterprise, Server, or a recent Win11 build. Original error: {refs_err:#}"
            );
            false
        }
        Err(ntfs_err) => panic!(
            "disk layout failed even for NTFS — this is a VHD/diskpart infrastructure \
             problem, not a ReFS SKU gap. ReFS error: {refs_err:#}; NTFS error: {ntfs_err:#}"
        ),
    }
}

/// Format a ReFS source, fill the fixture, back it up with verify-after, and
/// hand back `(vhd, backup_path, digest)`. Returns None on a ReFS-format SKIP.
fn backed_up_refs_source(
    cluster: u32,
    seed: u64,
    tag: &str,
) -> Option<(TestVhd, std::path::PathBuf, phoenix_systests::FixtureDigest)> {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create(REFS_SIZE_MB).expect("create source vhd");
    if !refs_layout_or_skip(&source, 'X', "REFSRC", cluster) {
        return None;
    }
    assert!(wait_for_letter('X', 15_000), "source volume never mounted");
    assert_eq!(
        volume_filesystem('X').expect("query source fs"),
        "ReFS",
        "diskpart reported success but the source volume did not mount as ReFS"
    );
    let digest = fill_fixture('X', seed).expect("fill fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    // The enumerator must classify the live volume as ReFS before capture even
    // starts — if this trips, detection regressed and the engine would have
    // fallen back to a raw full-partition capture.
    let refs_part = disk
        .partitions
        .iter()
        .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Refs)
        .expect("enumerate_disks did not classify the source partition as ReFS");
    assert_eq!(
        refs_part.capture_mode,
        phoenix_core::disk::CaptureMode::UsedBlocks,
        "ReFS partition should be planned as used-blocks"
    );

    let backup_path = std::env::temp_dir().join(format!(
        "refsfam-{tag}-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");
    Some((source, backup_path, digest))
}

fn assert_manifest_refs_used_blocks(backup_path: &std::path::Path, expect_cluster: u32) {
    let mut reader = PhnxReader::open(backup_path).expect("open backup");
    let pm = reader
        .manifest
        .partitions
        .iter()
        .find(|p| p.fs == "refs")
        .expect("no refs partition in manifest")
        .clone();
    assert_eq!(
        pm.capture_mode, "used-blocks",
        "ReFS should capture used-blocks (FSCTL bitmap), not raw"
    );
    let part_size = REFS_SIZE_MB * MIB;
    // A fresh ReFS volume pre-allocates a noticeable metadata footprint, but
    // even so a mostly-empty 2 GiB volume plus the fixture must land far
    // below a raw full capture. This is the assertion that fails if the
    // FSCTL path silently degrades to the all-used fallback.
    assert!(
        pm.used_bytes < part_size / 2,
        "ReFS used-block backup should be well under half the {part_size}-byte partition \
         (metadata + small fixture), got {} bytes — did capture fall back to raw?",
        pm.used_bytes
    );
    let entry = reader
        .index
        .iter()
        .find(|e| e.fs_kind == phoenix_core::disk::FilesystemKind::Refs)
        .expect("no ReFS partition in backup index")
        .clone();
    let stream = reader.read_stream_header(&entry).expect("stream header");
    assert_eq!(
        stream.bytes_per_cluster, expect_cluster,
        "manifest cluster size must be the volume's real allocation unit"
    );
}

fn restore_and_verify_refs(
    backup_path: &std::path::Path,
    target_size_mb: u64,
    digest: &phoenix_systests::FixtureDigest,
) -> (TestVhd, char) {
    let target = TestVhd::create(target_size_mb).expect("create target vhd");
    let reader = PhnxReader::open(backup_path).unwrap();
    let plan = default_plan_from_backup(
        backup_path.to_str().unwrap(),
        &reader,
        target.disk_index(),
        target_size_mb * MIB,
    );
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

    let letter = wait_for_restored_letter(target.disk_index(), 30_000)
        .expect("restored volume got no letter");
    assert_eq!(
        volume_filesystem(letter).expect("query restored fs"),
        "ReFS",
        "restored volume did not mount as ReFS"
    );
    verify_fixture(letter, digest).expect("fixture preserved");
    (target, letter)
}

#[test]
#[ignore = "requires elevation + diskpart; ReFS-capable SKU (self-skips otherwise)"]
fn refs_backup_restore_roundtrip() {
    let Some((source, backup_path, digest)) = backed_up_refs_source(4096, 0x5EF5, "4k") else {
        return;
    };
    assert_manifest_refs_used_blocks(&backup_path, 4096);

    let (target, _letter) = restore_and_verify_refs(&backup_path, REFS_SIZE_MB, &digest);

    // On-disk truth, independent of the mount: the ReFS boot signature landed
    // where the data partition sits (NUL-padded name — NOT "ReFS    ").
    let disks = enumerate_disks().unwrap();
    let refs_offset = disks
        .iter()
        .find(|d| d.index == target.disk_index())
        .and_then(|d| {
            d.partitions
                .iter()
                .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Refs)
                .map(|p| p.offset_bytes)
        })
        .expect("restored disk has no ReFS-classified partition");
    let tag = partition_boot_oem_tag(target.disk_index(), refs_offset).expect("read boot tag");
    assert!(
        tag.starts_with("ReFS"),
        "restored partition boot tag is {tag:?}, expected it to start with \"ReFS\""
    );

    drop(source);
    let _ = std::fs::remove_file(&backup_path);
}

#[test]
#[ignore = "requires elevation + diskpart; ReFS-capable SKU (self-skips otherwise)"]
fn refs_backup_restore_roundtrip_64k_clusters() {
    // 64K is ReFS's other legal allocation unit and the default on Server for
    // large volumes; this is the fixture that catches any hardcoded-4096
    // assumption in the ReFS plan (the same bug class the NTFS 64K test pins).
    let Some((source, backup_path, digest)) = backed_up_refs_source(65536, 0x64EF, "64k") else {
        return;
    };
    assert_manifest_refs_used_blocks(&backup_path, 65536);
    let (_target, _letter) = restore_and_verify_refs(&backup_path, REFS_SIZE_MB, &digest);
    drop(source);
    let _ = std::fs::remove_file(&backup_path);
}

#[test]
#[ignore = "requires elevation + diskpart; ReFS-capable SKU (self-skips otherwise)"]
fn refs_shrink_is_refused() {
    // ReFS cannot shrink (no offline shrink; backup superblocks live at the
    // volume's tail), so a plan whose ReFS entry is smaller than the source
    // must be refused loudly — even when the used extents would fit.
    let Some((_source, backup_path, _digest)) = backed_up_refs_source(4096, 0x5544, "shrink")
    else {
        return;
    };
    let target = TestVhd::create(REFS_SIZE_MB).expect("create target vhd");
    let reader = PhnxReader::open(&backup_path).unwrap();
    let mut plan = default_plan_from_backup(
        backup_path.to_str().unwrap(),
        &reader,
        target.disk_index(),
        REFS_SIZE_MB * MIB,
    );
    drop(reader);
    // Shave 64 MiB off the ReFS entry (the largest entry in the plan).
    let entry = plan
        .entries
        .iter_mut()
        .filter(|e| e.restore)
        .max_by_key(|e| e.target_size_bytes)
        .expect("plan has no restorable entries");
    entry.target_size_bytes -= 64 * MIB;

    let err = run_restore(RestoreOptions {
        backup_path: backup_path.clone(),
        plan,
        verify_on_restore: false,
        convert_sector_size: false,
        repair_boot: false,
        progress: None,
    })
    .expect_err("shrinking a ReFS partition must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("ReFS") || msg.contains("cannot shrink"),
        "refusal should name the ReFS shrink policy, got: {msg}"
    );
    let _ = std::fs::remove_file(&backup_path);
}

#[test]
#[ignore = "requires elevation + diskpart; ReFS-capable SKU (self-skips otherwise)"]
fn refs_restore_grow_extends_volume() {
    // Restore into a slot 512 MiB larger than the source: the boot sector is
    // left at source size and the post-table FSCTL_EXTEND_VOLUME pass must
    // grow the mounted volume to fill the partition.
    let Some((_source, backup_path, digest)) = backed_up_refs_source(4096, 0x64F0, "grow") else {
        return;
    };
    let grow_mb = REFS_SIZE_MB + 512;
    let target = TestVhd::create(grow_mb + 64).expect("create target vhd");
    let reader = PhnxReader::open(&backup_path).unwrap();
    let mut plan = default_plan_from_backup(
        backup_path.to_str().unwrap(),
        &reader,
        target.disk_index(),
        (grow_mb + 64) * MIB,
    );
    drop(reader);
    let entry = plan
        .entries
        .iter_mut()
        .filter(|e| e.restore)
        .max_by_key(|e| e.target_size_bytes)
        .expect("plan has no restorable entries");
    let source_part_bytes = entry.target_size_bytes;
    entry.target_size_bytes = source_part_bytes + 512 * MIB;

    run_restore(RestoreOptions {
        backup_path: backup_path.clone(),
        plan,
        verify_on_restore: true,
        convert_sector_size: false,
        repair_boot: false,
        progress: None,
    })
    .expect("run_restore");

    let letter = wait_for_restored_letter(target.disk_index(), 30_000)
        .expect("restored volume got no letter");
    assert_eq!(volume_filesystem(letter).expect("query fs"), "ReFS");
    verify_fixture(letter, &digest).expect("fixture preserved");

    // The mounted volume must now be materially larger than the source
    // partition — i.e. FSCTL_EXTEND_VOLUME actually ran (it logs a warning
    // and leaves the volume at source size on failure, which this catches).
    let vol_size: u64 =
        phoenix_systests::powershell(&format!("(Get-Volume -DriveLetter {letter}).Size"))
            .expect("query volume size")
            .trim()
            .parse()
            .expect("parse volume size");
    assert!(
        vol_size > source_part_bytes + 256 * MIB,
        "restored ReFS volume is {vol_size} bytes; expected it extended well past the \
         source partition size ({source_part_bytes} bytes) — FSCTL_EXTEND_VOLUME \
         didn't take effect"
    );
    let _ = std::fs::remove_file(&backup_path);
}
