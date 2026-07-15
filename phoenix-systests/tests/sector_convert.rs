//! Tier 2 — cross-sector-size conversion (4Kn → 512e) on restore.
//!
//! Capture a volume off a **true 4Kn** virtual disk (4096-byte logical sectors,
//! built via `CreateVirtualDisk` — see `sector_4kn.rs`) and restore it onto a
//! **512e** virtual disk (the ordinary diskpart-made kind). Without the
//! conversion feature that restore comes up RAW, because the 4Kn boot sector's
//! `BytesPerSector = 4096` lands verbatim on a 512-byte device. With
//! `--convert-sector-size` / `RestoreOptions::convert_sector_size`, the engine
//! rewrites the filesystem boot sectors to 512e and the volume mounts, `chkdsk`
//! finds nothing to fix, and every fixture byte survives.
//!
//! These are the shipping-feature counterparts of the two hardware spikes
//! recorded in docs/SECTOR-SIZE-CONVERSION.md (NTFS + FAT32, per-partition).
//!
//! ```text
//! cargo test -p phoenix-systests --test sector_convert -- --ignored --test-threads=1 --nocapture
//! ```

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_core::error::PhoenixError;
use phoenix_restore::plan::{build_partial_plan, default_plan_from_backup};
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    chkdsk_clean, cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture,
    wait_for_letter, wait_for_letter_at_offset, wait_for_restored_letter, PartSpec, TestFs, TestVhd,
};

/// Capture a 4Kn NTFS volume and restore it, converted, onto a larger 512e disk.
///
/// `verify_on_restore = true` is deliberate: the restore's in-pipeline hash
/// check must NOT false-fail on the boot sectors we rewrite (the rewrite happens
/// after the streamed data is verified against the image, so a converted restore
/// verifies clean). A green run proves the boot sector was rewritten to 512e
/// (else the volume would be RAW and get no letter), the NTFS grew into the
/// larger target, `chkdsk` is clean, and every fixture byte survived.
#[test]
#[ignore = "requires elevation + diskpart"]
fn convert_4kn_ntfs_to_512e() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create_4kn(512).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "CVT4KN".into(),
        }])
        .expect("init 4Kn NTFS source");
    assert!(wait_for_letter('X', 15_000), "4Kn source never mounted");
    let digest = fill_fixture('X', 0x4096_C001).expect("fill fixture");

    let disks = enumerate_disks().expect("enumerate disks");
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .expect("source enumerated");
    assert_eq!(disk.sector_size, 4096, "source fixture is not 4Kn");

    let backup = std::env::temp_dir().join(format!(
        "phoenix-cvt-ntfs-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: disk.partitions.iter().map(|p| p.index).collect(),
        output: backup.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup from 4Kn");

    // Restore onto a 512e disk (twice the size — the NTFS also grows, exercising
    // convert + FSCTL_EXTEND together).
    let target = TestVhd::create(1024).expect("create 512e target");
    {
        let d = enumerate_disks().expect("enumerate");
        let t = d.iter().find(|d| d.index == target.disk_index()).unwrap();
        assert_eq!(t.sector_size, 512, "target fixture is not 512e");
    }

    let reader = PhnxReader::open(&backup).expect("open backup");
    let plan = default_plan_from_backup(
        backup.to_str().unwrap(),
        &reader,
        target.disk_index(),
        1024 * 1024 * 1024,
    );
    drop(reader);

    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: true,
        convert_sector_size: true,
        progress: None,
    })
    .expect("converted restore 4Kn -> 512e");

    let letter = wait_for_restored_letter(target.disk_index(), 60_000)
        .expect("converted NTFS volume never got a drive letter (came up RAW?)");
    chkdsk_clean(letter).expect("converted NTFS volume is chkdsk-clean");
    verify_fixture(letter, &digest).expect("fixture survived the 4Kn -> 512e conversion");

    let _ = std::fs::remove_file(&backup);
}

/// Capture a 4Kn FAT32 volume and restore it, converted, onto a 512e disk.
///
/// Mirrors the ESP spike: the ESP is FAT32, and the converter auto-detects a
/// FAT32 boot sector (the ESP restores through the raw path, so a kind-driven
/// dispatch would miss it — this proves the auto-detect + FAT32 field rewrite).
#[test]
#[ignore = "requires elevation + diskpart"]
fn convert_4kn_fat32_to_512e() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create_4kn(768).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Fat32,
            letter: 'X',
            label: "CVTFAT".into(),
        }])
        .expect("init 4Kn FAT32 source");
    assert!(wait_for_letter('X', 15_000), "4Kn FAT32 source never mounted");
    let digest = fill_fixture('X', 0x4096_FA32).expect("fill fixture");

    let disks = enumerate_disks().expect("enumerate disks");
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .expect("source enumerated");
    assert_eq!(disk.sector_size, 4096, "source fixture is not 4Kn");

    let backup = std::env::temp_dir().join(format!(
        "phoenix-cvt-fat32-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: disk.partitions.iter().map(|p| p.index).collect(),
        output: backup.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup from 4Kn FAT32");

    // Same-size-class target (front-packed FAT32 stays its original size, so this
    // is the convert-without-resize path).
    let target = TestVhd::create(1536).expect("create 512e target");
    let reader = PhnxReader::open(&backup).expect("open backup");
    let plan = default_plan_from_backup(
        backup.to_str().unwrap(),
        &reader,
        target.disk_index(),
        1536 * 1024 * 1024,
    );
    drop(reader);

    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: true,
        convert_sector_size: true,
        progress: None,
    })
    .expect("converted FAT32 restore 4Kn -> 512e");

    let letter = wait_for_restored_letter(target.disk_index(), 60_000)
        .expect("converted FAT32 volume never got a drive letter (came up RAW?)");
    chkdsk_clean(letter).expect("converted FAT32 volume is chkdsk-clean");
    verify_fixture(letter, &digest).expect("fixture survived the FAT32 4Kn -> 512e conversion");

    let _ = std::fs::remove_file(&backup);
}

/// The load-bearing refusal: a 512e backup cannot be converted onto a 4Kn disk.
/// Fine → coarse can't be faked, so the engine must refuse before touching the
/// target — no `--convert-sector-size` override exists for this direction.
#[test]
#[ignore = "requires elevation + diskpart"]
fn convert_refuses_512e_to_4kn() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create(512).expect("create 512e source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SRC512".into(),
        }])
        .expect("init 512e source");
    assert!(wait_for_letter('X', 15_000), "512e source never mounted");
    let _ = fill_fixture('X', 0x0512_4096).expect("fill fixture");

    let disks = enumerate_disks().expect("enumerate disks");
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .expect("source enumerated");
    assert_eq!(disk.sector_size, 512, "source fixture is not 512e");

    let backup = std::env::temp_dir().join(format!(
        "phoenix-cvt-refuse-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: disk.partitions.iter().map(|p| p.index).collect(),
        output: backup.clone(),
        verify_after: false,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup from 512e");

    let target = TestVhd::create_4kn(1024).expect("create 4Kn target");
    let reader = PhnxReader::open(&backup).expect("open backup");
    let plan = default_plan_from_backup(
        backup.to_str().unwrap(),
        &reader,
        target.disk_index(),
        1024 * 1024 * 1024,
    );
    drop(reader);

    // Even with the opt-in flag set, 512e -> 4Kn is a hard refusal.
    let err = run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: false,
        convert_sector_size: true,
        progress: None,
    })
    .expect_err("512e -> 4Kn must be refused");
    assert!(
        matches!(err, PhoenixError::SectorSizeUnsupported { .. }),
        "expected SectorSizeUnsupported, got: {err:?}"
    );

    let _ = std::fs::remove_file(&backup);
}

/// A 4Kn -> 512e restore without the opt-in is refused (naming the flag); with
/// the opt-in it succeeds. Proves the gate, and that a refused attempt leaves the
/// target untouched (the second attempt restores into the same disk).
#[test]
#[ignore = "requires elevation + diskpart"]
fn convert_requires_optin() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create_4kn(512).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "OPTIN".into(),
        }])
        .expect("init 4Kn source");
    assert!(wait_for_letter('X', 15_000), "4Kn source never mounted");
    let digest = fill_fixture('X', 0x0F17_4096).expect("fill fixture");

    let disks = enumerate_disks().expect("enumerate disks");
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .expect("source enumerated");

    let backup = std::env::temp_dir().join(format!(
        "phoenix-cvt-optin-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: disk.partitions.iter().map(|p| p.index).collect(),
        output: backup.clone(),
        verify_after: false,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup from 4Kn");

    let target = TestVhd::create(1024).expect("create 512e target");
    let make_plan = || {
        let reader = PhnxReader::open(&backup).expect("open backup");
        let plan = default_plan_from_backup(
            backup.to_str().unwrap(),
            &reader,
            target.disk_index(),
            1024 * 1024 * 1024,
        );
        drop(reader);
        plan
    };

    // Without the flag: refused, naming the opt-in.
    let err = run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan: make_plan(),
        verify_on_restore: false,
        convert_sector_size: false,
        progress: None,
    })
    .expect_err("4Kn -> 512e without opt-in must be refused");
    assert!(
        matches!(err, PhoenixError::SectorConversionRequired { .. }),
        "expected SectorConversionRequired, got: {err:?}"
    );

    // With the flag: succeeds (and the earlier refusal left the target pristine).
    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan: make_plan(),
        verify_on_restore: false,
        convert_sector_size: true,
        progress: None,
    })
    .expect("opted-in restore should succeed");

    let letter = wait_for_restored_letter(target.disk_index(), 60_000)
        .expect("opted-in converted volume never got a drive letter");
    chkdsk_clean(letter).expect("converted volume is chkdsk-clean");
    verify_fixture(letter, &digest).expect("fixture survived the opted-in conversion");

    let _ = std::fs::remove_file(&backup);
}

/// Conversion is per-partition, not full-disk-only: restore a single 4Kn NTFS
/// partition into an existing slot on a 512e disk (partial mode) with conversion.
#[test]
#[ignore = "requires elevation + diskpart"]
fn convert_partial_restore_4kn_ntfs_to_512e() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create_4kn(512).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "PART4KN".into(),
        }])
        .expect("init 4Kn source");
    assert!(wait_for_letter('X', 15_000), "4Kn source never mounted");
    let digest = fill_fixture('X', 0x4096_2211).expect("fill fixture");

    let disks = enumerate_disks().expect("enumerate disks");
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .expect("source enumerated");
    let src_ntfs = disk
        .partitions
        .iter()
        .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .expect("source NTFS partition");

    let backup = std::env::temp_dir().join(format!(
        "phoenix-cvt-partial-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: vec![src_ntfs.index],
        output: backup.clone(),
        verify_after: false,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup from 4Kn");

    // A 512e target that already carries a GPT table with one (larger) NTFS
    // slot; partial restore lands the source into that existing slot.
    let target = TestVhd::create(1024).expect("create 512e target");
    target
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'Y',
            label: "TGTSLOT".into(),
        }])
        .expect("init 512e target slot");
    assert!(wait_for_letter('Y', 15_000), "target slot never mounted");

    let target_info = {
        let mut d = enumerate_disks().expect("enumerate");
        for disk in &mut d {
            for p in &mut disk.partitions {
                phoenix_core::disk::refine_partition_fs(p);
            }
        }
        d.into_iter()
            .find(|d| d.index == target.disk_index())
            .expect("target enumerated")
    };
    assert_eq!(target_info.sector_size, 512, "target is not 512e");
    let slot = target_info
        .partitions
        .iter()
        .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .expect("target NTFS slot");
    assert!(
        slot.size_bytes >= src_ntfs.size_bytes,
        "target slot must be at least the source size (no shrink under conversion)"
    );

    // Map source NTFS -> the existing target slot, in place.
    let plan = build_partial_plan(
        backup.to_str().unwrap(),
        target.disk_index(),
        &target_info,
        &[(
            slot.index,
            src_ntfs.index,
            slot.offset_bytes,
            slot.size_bytes,
        )],
    );

    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: true,
        convert_sector_size: true,
        progress: None,
    })
    .expect("partial converted restore 4Kn -> 512e");

    let letter = wait_for_letter_at_offset(target.disk_index(), slot.offset_bytes, 90_000)
        .expect("partial converted volume never got a drive letter");
    chkdsk_clean(letter).expect("partial converted volume is chkdsk-clean");
    verify_fixture(letter, &digest).expect("fixture survived the partial conversion");

    let _ = std::fs::remove_file(&backup);
}
