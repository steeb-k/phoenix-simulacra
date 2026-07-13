//! Tier 2-4Kn — system tests on a **true 4Kn** virtual disk (4096-byte logical
//! sectors).
//!
//! Every other T2 fixture is 512-byte-sector, because diskpart's `create vdisk`
//! cannot set a sector size. `TestVhd::create_4kn` builds the VHDX through
//! `CreateVirtualDisk` (virtdisk.dll — core Windows, no Hyper-V, no Pro SKU)
//! with an explicit `SectorSizeInBytes`, which is the only way to get a 4Kn disk
//! without 4Kn hardware.
//!
//! This is the tier that covers the ARM64 laptop's UFS drive *if* it turns out
//! to report 4096-byte logical sectors — and it covers it here, on x64, without
//! that hardware.
//!
//! ```text
//! cargo test -p phoenix-systests --test sector_4kn -- --ignored --test-threads=1 --nocapture
//! ```

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_clone::{run_clone, CloneEntry, CloneOptions, ClonePlan, CloneTableMode, CloneVerify};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_restore::plan::default_plan_from_backup;
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    chkdsk_clean, cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture,
    wait_for_letter, wait_for_letter_at_offset, wait_for_restored_letter, PartSpec, TestFs,
    TestVhd,
};

/// The disk really is 4Kn, and our own enumeration agrees.
///
/// This is the foundation the rest of the tier stands on: if `get_sector_size`
/// doesn't report 4096 here, every downstream 4Kn assertion is meaningless.
#[test]
#[ignore = "requires elevation + diskpart"]
fn vhdx_4kn_reports_4096_byte_sectors() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let vhd = TestVhd::create_4kn(2048).expect("create 4Kn VHDX");

    let disks = enumerate_disks().expect("enumerate_disks");
    let d = disks
        .iter()
        .find(|d| d.index == vhd.disk_index())
        .unwrap_or_else(|| panic!("disk {} not enumerated", vhd.disk_index()));

    assert_eq!(
        d.sector_size, 4096,
        "CreateVirtualDisk was asked for a 4096-byte logical sector, but the \
         attached disk enumerates as {} — the fixture itself is wrong, so no \
         4Kn conclusion drawn from it would be trustworthy",
        d.sector_size
    );
}

/// Capture an NTFS volume off a 4Kn disk, with verify-after-backup on.
///
/// This is the test that H1 was blocking. Before the reader learned to bounce
/// sub-sector reads through an aligned span, this died on the *first* read —
/// `ntfs_plan` asks for a 512-byte boot sector at offset 0, and a raw handle on
/// a 4Kn device rejects that outright:
///
/// ```text
/// CAPTURE FAILED: ReadFile of 512 bytes at offset 0 failed (Win32 error 87)
/// ```
///
/// Passing proves three things at once: the boot sector parsed (so used-block
/// planning ran rather than silently degrading to a raw capture), the extent
/// streaming survived 4096-byte alignment, and verify-after re-read the source
/// and agreed with the image byte for byte.
#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_4kn_backup_verifies_against_source() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let vhd = TestVhd::create_4kn(1024).expect("create 4Kn VHDX");
    vhd.init_gpt_with(&[PartSpec {
        size_mb: 0, // rest of disk
        fs: TestFs::Ntfs,
        letter: 'X',
        label: "SRC4KN".into(),
    }])
    .expect("init 4Kn gpt + format NTFS");
    assert!(wait_for_letter('X', 15_000), "4Kn volume never mounted");
    fill_fixture('X', 0x4096_5EED).expect("fill fixture");

    let disks = enumerate_disks().expect("enumerate disks");
    let disk = disks
        .iter()
        .find(|d| d.index == vhd.disk_index())
        .expect("4Kn disk enumerated");
    assert_eq!(disk.sector_size, 4096, "fixture is not actually 4Kn");

    let ntfs = disk
        .partitions
        .iter()
        .find(|p| p.capture_mode == phoenix_core::disk::CaptureMode::UsedBlocks)
        .expect(
            "no partition planned as UsedBlocks — the NTFS boot sector failed to parse, which \
             means the 4Kn read path silently degraded to a raw capture instead of erroring",
        );
    assert_eq!(ntfs.sector_size, 4096);

    let backup = std::env::temp_dir().join(format!(
        "phoenix-systest-4kn-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: vhd.disk_index(),
        partition_indices: disk.partitions.iter().map(|p| p.index).collect(),
        output: backup.clone(),
        // The point of the test: re-read the 4Kn source and confirm the image
        // matches it. A capture that "succeeded" by reading the wrong offsets
        // would fail here rather than pass quietly.
        verify_after: true,
        verify_image: true,
        progress: None,
    })
    .expect("run_backup on a 4Kn disk");

    // The manifest must record the source's real geometry, not the format's
    // 512-byte extent unit (see docs/phnx-format.md — the two are decoupled).
    let reader = PhnxReader::open(&backup).expect("open backup");
    assert_eq!(
        reader.manifest.disk.sector_size, 4096,
        "manifest lost the source disk's 4Kn geometry"
    );
    drop(reader);

    let _ = std::fs::remove_file(&backup);
}

/// The full round-trip on 4Kn: capture a 4Kn NTFS volume and restore it to a
/// second 4Kn disk, same size.
///
/// This is what H2-H5 were blocking. Capture worked once the reader learned to
/// bounce sub-sector reads (H1), but everything downstream still counted in
/// 512-byte units on a device whose sectors are 4096:
///
///   * the backup GPT was reserved as `33 * 512` instead of `33 * 4096`, so the
///     last partition overlapped the backup GPT entry array;
///   * `StartingUsableOffset` was `34 * 512` = 17,408 — not even a multiple of
///     4096, so `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` could not accept it;
///   * `FSCTL_EXTEND_VOLUME` was handed a sector count derived from 512, i.e.
///     8x the sectors the partition actually has.
///
/// A green run means the partition table landed, the volume mounted, chkdsk
/// found nothing to fix, and every fixture byte survived.
#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_4kn_backup_restore_roundtrip() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create_4kn(1024).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SRC4KN".into(),
        }])
        .expect("init 4Kn source");
    assert!(wait_for_letter('X', 15_000), "4Kn source never mounted");
    let digest = fill_fixture('X', 0x4096_0001).expect("fill fixture");

    let disks = enumerate_disks().expect("enumerate disks");
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .expect("source enumerated");
    assert_eq!(disk.sector_size, 4096, "source fixture is not 4Kn");

    let backup = std::env::temp_dir().join(format!(
        "phoenix-4kn-rt-{}.phnx",
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

    // Restore onto a second 4Kn disk of the same size. Same sector size on both
    // ends is the only case that can work: NTFS records its bytes-per-sector in
    // its own boot sector, so a 4Kn volume's bytes are simply not mountable on a
    // 512-byte device no matter how faithfully we copy them.
    let target = TestVhd::create_4kn(1024).expect("create 4Kn target");
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
        progress: None,
    })
    .expect("run_restore to 4Kn");

    let letter = wait_for_restored_letter(target.disk_index(), 45_000)
        .expect("restored 4Kn volume never got a drive letter");
    chkdsk_clean(letter).expect("restored 4Kn volume is chkdsk-clean");
    verify_fixture(letter, &digest).expect("fixture survived the 4Kn round-trip");

    let _ = std::fs::remove_file(&backup);
}

/// Disk-to-disk **clone** between two 4Kn disks, same size and expanded.
///
/// The clone path shares the fixed reader (H1) and `extend_ntfs_volume` (H2) with
/// backup/restore, so it was *expected* to work once those landed — but expected
/// is not tested, and clone reaches them by a different route. It writes the
/// partition table itself (`set_drive_layout`, where the GPT reserve and
/// `StartingUsableOffset` counted in 512s), and it grows NTFS through its own
/// call site, which is the one the original audit found was *already* passing the
/// right sector size while restore was passing the wrong one.
///
/// Both source and target are synthesized 4Kn disks. No 4Kn hardware.
#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_4kn_clone_same_size_and_expanded() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create_4kn(1024).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "CLN4KN".into(),
        }])
        .expect("init 4Kn source");
    assert!(wait_for_letter('X', 15_000), "4Kn source never mounted");
    let digest = fill_fixture('X', 0x4096_C10E).expect("fill fixture");

    // --- Same-size clone onto a second 4Kn disk -----------------------------
    let target = TestVhd::create_4kn(1024).expect("create 4Kn target");
    let d = clone_disks();
    let src = disk_at(&d, source.disk_index());
    assert_eq!(src.sector_size, 4096, "source is not 4Kn");
    let tgt = disk_at(&d, target.disk_index());
    assert_eq!(tgt.sector_size, 4096, "target is not 4Kn");

    run_clone(CloneOptions {
        source_disk_index: source.disk_index(),
        target_disk_index: target.disk_index(),
        plan: ClonePlan::identity(&src),
        verify: CloneVerify::ReadBack,
        use_vss: false,
        progress: None,
    })
    .expect("same-size clone between 4Kn disks");

    let letter = wait_for_restored_letter(target.disk_index(), 60_000)
        .expect("cloned 4Kn volume never got a drive letter");
    chkdsk_clean(letter).expect("cloned 4Kn volume is chkdsk-clean");
    verify_fixture(letter, &digest).expect("fixture survived the 4Kn clone");
    drop(target);

    // --- Expand-to-fill onto a LARGER 4Kn disk ------------------------------
    // This is the arm that exercises the grow: NTFS must be extended into the
    // extra space, and `FSCTL_EXTEND_VOLUME` counts in the DEVICE's sectors —
    // 4096 here, not the 512 the restore path used to assume.
    let bigger = TestVhd::create_4kn(2048).expect("create larger 4Kn target");
    let d = clone_disks();
    let src = disk_at(&d, source.disk_index());
    let big = disk_at(&d, bigger.disk_index());
    assert_eq!(big.sector_size, 4096);

    run_clone(CloneOptions {
        source_disk_index: source.disk_index(),
        target_disk_index: bigger.disk_index(),
        plan: ClonePlan::expand_to_fill(&src, &big),
        verify: CloneVerify::ReadBack,
        use_vss: false,
        progress: None,
    })
    .expect("expand-to-fill clone between 4Kn disks");

    let letter = wait_for_restored_letter(bigger.disk_index(), 90_000)
        .expect("expanded 4Kn volume never got a drive letter");
    chkdsk_clean(letter).expect("expanded 4Kn volume is chkdsk-clean");
    verify_fixture(letter, &digest).expect("fixture survived the expanding 4Kn clone");

    // The volume really did grow — otherwise this passes on a clone that quietly
    // stayed at the source's size and proves nothing about the extend.
    let after = clone_disks();
    let grown = disk_at(&after, bigger.disk_index());
    let ntfs = grown
        .partitions
        .iter()
        .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .expect("no NTFS partition on the expanded target");
    let src_ntfs = src
        .partitions
        .iter()
        .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .unwrap();
    assert!(
        ntfs.size_bytes > src_ntfs.size_bytes,
        "expand-to-fill left the partition at {} bytes, the same as the source — it did not grow",
        ntfs.size_bytes
    );
}

fn clone_disks() -> Vec<phoenix_core::disk::DiskInfo> {
    let mut d = enumerate_disks().expect("enumerate disks");
    for disk in &mut d {
        for p in &mut disk.partitions {
            phoenix_core::disk::refine_partition_fs(p);
        }
    }
    d
}

fn disk_at(disks: &[phoenix_core::disk::DiskInfo], index: u32) -> phoenix_core::disk::DiskInfo {
    disks
        .iter()
        .find(|d| d.index == index)
        .unwrap_or_else(|| panic!("disk {index} not enumerated"))
        .clone()
}

/// Clone NTFS into a **smaller** slot on a 4Kn disk: the volume must shrink.
///
/// This is the arm worth having most. Shrinking is the only path that rewrites
/// NTFS metadata in place — the relocation map, then the MFT / `$Bitmap` /
/// `$LogFile` rewrite — and it is precisely where 4Kn hid its nastiest bug:
/// **NTFS on a 4Kn volume reports 4096-byte sectors but still writes 1024-byte
/// MFT records**, so the fixup code computed its first sector boundary *past the
/// end of the record it was fixing up*:
///
/// ```text
/// MFT record sector boundary 4096 past record length 1024
/// ```
///
/// That was found through the *restore* path. Clone reaches the same rewriter by
/// its own route, so this pins that the fix holds there too — and a wrong fixup
/// stride does not fail loudly, it writes a subtly corrupt filesystem, which is
/// why `chkdsk` and the fixture digest both have to agree before this passes.
#[test]
#[ignore = "requires elevation + diskpart"]
fn ntfs_4kn_clone_shrinks_into_a_smaller_slot() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create_4kn(1024).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SHR4KN".into(),
        }])
        .expect("init 4Kn source");
    assert!(wait_for_letter('X', 15_000), "4Kn source never mounted");
    let digest = fill_fixture('X', 0x4096_5152).expect("fill fixture");

    let target = TestVhd::create_4kn(1024).expect("create 4Kn target");
    let d = clone_disks();
    let src = disk_at(&d, source.disk_index());
    assert_eq!(src.sector_size, 4096, "source is not 4Kn");
    assert_eq!(
        disk_at(&d, target.disk_index()).sector_size,
        4096,
        "target is not 4Kn"
    );

    let src_ntfs = src
        .partitions
        .iter()
        .find(|p| p.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .expect("source NTFS partition");

    // Half the source. The fixture is far smaller than that, so its data fits —
    // but the volume's own metadata lives past the new boundary and has to be
    // relocated to make it so, which is the whole point.
    let shrunk = src_ntfs.size_bytes / 2;
    let offset = 1024 * 1024u64;
    assert!(shrunk < src_ntfs.size_bytes);

    run_clone(CloneOptions {
        source_disk_index: source.disk_index(),
        target_disk_index: target.disk_index(),
        plan: ClonePlan {
            entries: vec![CloneEntry {
                source_partition_index: src_ntfs.index,
                target_offset_bytes: offset,
                target_size_bytes: shrunk,
            }],
            table_mode: CloneTableMode::ReinitMatchSource,
        },
        verify: CloneVerify::ReadBack,
        use_vss: false,
        progress: None,
    })
    .expect("shrinking clone between 4Kn disks");

    // The table says it shrank...
    let after = clone_disks();
    let cloned = disk_at(&after, target.disk_index())
        .partitions
        .into_iter()
        .find(|p| p.offset_bytes == offset)
        .expect("cloned partition missing from the target's table");
    assert_eq!(
        cloned.size_bytes, shrunk,
        "the partition table does not reflect the shrunk size"
    );

    // ...and the filesystem inside it is actually coherent, not merely present.
    let letter = wait_for_letter_at_offset(target.disk_index(), offset, 90_000)
        .expect("shrunk 4Kn volume never got a drive letter");
    chkdsk_clean(letter).expect("shrunk 4Kn volume is chkdsk-clean");
    verify_fixture(letter, &digest).expect("fixture survived the shrinking 4Kn clone");
}
