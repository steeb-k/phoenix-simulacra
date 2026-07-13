//! Tier-3 DESTRUCTIVE tests against a REAL physical disk.
//!
//! These WIPE the target disk repeatedly. They only run when opted in via the
//! `PHOENIX_T3_DISK` env var, and the harness ([`RealDisk::acquire`]) refuses
//! the boot/system disk, an out-of-size disk, or — by default — any
//! non-removable disk, re-checking before every destructive step. Optionally
//! pin the exact device with `PHOENIX_T3_SERIAL`.
//!
//! Run the USB/MBR matrix (ELEVATED):
//! ```text
//! $env:PHOENIX_T3_DISK="2"; $env:PHOENIX_T3_SERIAL="04018bdbdd996130c3c9"
//! cargo test -p phoenix-systests --test real_disk -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Windows won't make a removable USB flash drive GPT, so the USB scenarios use
//! MBR (a real coverage gap the all-GPT T2 VHD suite never touches). To validate
//! **GPT on real hardware** you need a FIXED (non-removable) disk, which the
//! gate only allows under an explicit opt-in (see `real_gpt_multifs_roundtrip`):
//! ```text
//! $env:PHOENIX_T3_DISK="3"; $env:PHOENIX_T3_ALLOW_FIXED="1"
//! $env:PHOENIX_T3_SERIAL="<exact-serial>"   # MANDATORY for a fixed disk
//! $env:PHOENIX_T3_MAX_GB="4100"             # widen if the disk is >64 GB
//! $env:PHOENIX_T3_LAYOUT_GB="16"            # cap restore layout on a huge disk
//! cargo test -p phoenix-systests --test real_disk -- --ignored --test-threads=1 --nocapture real_gpt_multifs_roundtrip
//! ```
//!
//! `PHOENIX_T3_LAYOUT_GB` caps how far the restore lays partitions into a big
//! disk (so NTFS doesn't grow to fill 4 TB); GPT still spans the whole disk.

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_clone::{run_clone, CloneOptions, ClonePlan, CloneVerify};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{enumerate_disks, refine_partition_fs};
use phoenix_restore::plan::{default_plan_from_backup, RestorePlan, RestorePlanEntry};
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    bitlocker_status, chkdsk_clean, chkdsk_offline_fix, disk_partition_boot_tags,
    enable_bitlocker_password, fill_fixture, lock_bitlocker, partition_summary, require_admin,
    rescan_disk_volumes, unlock_bitlocker_password, verify_fixture, wait_for_disk_volumes,
    wait_for_letter, wait_for_letter_even_if_locked, wait_for_restored_letter, PartSpec, RealDisk,
    TestFs, TestVhd,
};

const MB: u64 = 1024 * 1024;

/// Back up every partition of `idx` to a fresh `.phnx` (verify-after on).
fn backup_all(idx: u32) -> std::path::PathBuf {
    let parts = all_partition_indices(idx);
    eprintln!("[T3] backing up disk {idx} partitions {parts:?}");
    let backup = std::env::temp_dir().join(format!("t3-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: idx,
        partition_indices: parts,
        output: backup.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");
    backup
}

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

/// Shared body for the MBR and GPT multi-filesystem round-trips: lay out an
/// NTFS + FAT32 disk, fill fixtures, back up every partition, wipe, full-disk
/// restore, and verify the partition table + data. `gpt` selects the partition
/// style — the only real difference on the wire, since the engine reads/writes
/// GPT vs MBR tables through different paths.
fn multifs_roundtrip(disk: &RealDisk, gpt: bool) {
    let idx = disk.index();
    let style = if gpt { "GPT" } else { "MBR" };

    disk.layout(
        gpt,
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
    .unwrap_or_else(|e| panic!("{style} layout: {e:#}"));
    assert!(wait_for_letter('R', 20_000), "NTFS volume didn't mount");
    assert!(wait_for_letter('S', 20_000), "FAT volume didn't mount");
    let d_ntfs = fill_fixture('R', 0xAA01).expect("fill ntfs fixture");
    let d_fat = fill_fixture('S', 0xAA02).expect("fill fat fixture");

    let before = partition_summary(idx).expect("summary before");
    eprintln!("[T3] {style} source partitions: {before:?}");
    let disk_size = disk_size_bytes(idx);
    // On a very large disk, cap the region the restore lays into (so NTFS
    // doesn't auto-grow to fill e.g. 4 TB and drag chkdsk out). This does NOT
    // reduce GPT coverage: the partition table still spans the whole disk —
    // the backup GPT header is written at the disk's true last LBA regardless
    // of how far the partitions extend. Unset → use the full disk (the 30 GB
    // USB tests still exercise grow-to-fill).
    let layout_size = std::env::var("PHOENIX_T3_LAYOUT_GB")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|gb| ((gb * 1e9) as u64).min(disk_size))
        .unwrap_or(disk_size);
    if layout_size != disk_size {
        eprintln!(
            "[T3] {style} capping restore layout to {:.1} GB of the {:.1} GB disk \
             (PHOENIX_T3_LAYOUT_GB); GPT still spans the full disk",
            layout_size as f64 / 1e9,
            disk_size as f64 / 1e9
        );
    }

    // --- Back up every partition ---
    let parts = all_partition_indices(idx);
    eprintln!("[T3] backing up disk {idx} partitions {parts:?}");
    let backup = std::env::temp_dir().join(format!(
        "t3-{}-{}.phnx",
        style.to_ascii_lowercase(),
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: idx,
        partition_indices: parts,
        output: backup.clone(),
        verify_after: true,
        verify_image: false,
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
    let plan = default_plan_from_backup(backup.to_str().unwrap(), &reader, idx, layout_size);
    for e in &plan.entries {
        eprintln!(
            "[T3] {style} restore plan: src={:?} off={} size={} end={}",
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
    eprintln!("[T3] {style} restored letters (offset order): {letters:?}");
    let after = partition_summary(idx).expect("summary after");
    eprintln!("[T3] {style} restored partitions: {after:?}");
    assert_eq!(
        after.len(),
        before.len(),
        "restore changed the partition count"
    );

    // Offset order matches the layout order: [0] = NTFS, [1] = FAT32.
    chkdsk_clean(letters[0]).expect("chkdsk on restored NTFS");
    verify_fixture(letters[0], &d_ntfs).expect("NTFS fixture preserved across restore");
    verify_fixture(letters[1], &d_fat).expect("FAT32 fixture preserved across restore");
    // Deeper structural check: offline chkdsk /F on the restored NTFS (last —
    // it force-dismounts the volume).
    chkdsk_offline_fix(letters[0]).expect("offline chkdsk on restored NTFS");

    let _ = std::fs::remove_file(&backup);
    eprintln!(
        "[T3] real_{}_multifs_roundtrip PASSED",
        style.to_ascii_lowercase()
    );
}

#[test]
#[ignore = "DESTRUCTIVE: wipes the real USB disk; set PHOENIX_T3_DISK"]
fn real_mbr_multifs_roundtrip() {
    require_admin();
    let Some(disk) = skip_or_disk() else {
        return;
    };
    multifs_roundtrip(&disk, false);
}

/// GPT on real hardware. Windows won't make a removable USB drive GPT, so this
/// needs a FIXED disk: set PHOENIX_T3_ALLOW_FIXED=1 and pin PHOENIX_T3_SERIAL
/// (see `RealDisk::acquire`). Skips cleanly if the acquired target is USB.
#[test]
#[ignore = "DESTRUCTIVE: wipes a real FIXED disk; needs PHOENIX_T3_ALLOW_FIXED=1 + PHOENIX_T3_SERIAL"]
fn real_gpt_multifs_roundtrip() {
    require_admin();
    let Some(disk) = skip_or_disk() else {
        return;
    };
    if disk.is_removable() {
        eprintln!(
            "[T3] skipping GPT test: target disk {} is removable media (Windows can't make it \
             GPT). Use a non-removable disk (internal or external HDD) with \
             PHOENIX_T3_ALLOW_FIXED=1 + PHOENIX_T3_SERIAL.",
            disk.index()
        );
        return;
    }
    multifs_roundtrip(&disk, true);
}

#[test]
#[ignore = "DESTRUCTIVE: wipes the real USB disk; set PHOENIX_T3_DISK"]
fn real_mbr_restore_shrink() {
    require_admin();
    let Some(disk) = skip_or_disk() else {
        return;
    };
    let idx = disk.index();

    // Source: NTFS 2 GB + FAT32 1 GB, filled.
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
    assert!(wait_for_letter('R', 20_000), "NTFS didn't mount");
    assert!(wait_for_letter('S', 20_000), "FAT didn't mount");
    let d_ntfs = fill_fixture('R', 0xBB01).expect("fill ntfs");
    let d_fat = fill_fixture('S', 0xBB02).expect("fill fat");

    let backup = backup_all(idx);

    // Restore with the NTFS SHRUNK 2 GB -> 512 MB (forces relocation of the
    // metadata NTFS parks near the volume end), FAT32 kept at 1 GB right after.
    let ntfs_off = MB;
    let ntfs_size = 512 * MB;
    let fat_off = ntfs_off + ntfs_size;
    let plan = RestorePlan {
        backup_path: backup.to_str().unwrap().to_string(),
        target_disk_index: idx,
        full_disk: true,
        reinit_style: None,
        entries: vec![
            RestorePlanEntry {
                source_partition_index: Some(0),
                target_partition_index: 0,
                restore: true,
                target_offset_bytes: ntfs_off,
                target_size_bytes: ntfs_size,
            },
            RestorePlanEntry {
                source_partition_index: Some(1),
                target_partition_index: 1,
                restore: true,
                target_offset_bytes: fat_off,
                target_size_bytes: 1024 * MB,
            },
        ],
    };
    for e in &plan.entries {
        eprintln!(
            "[T3] shrink plan: src={:?} off={} size={}",
            e.source_partition_index, e.target_offset_bytes, e.target_size_bytes
        );
    }

    disk.clean().expect("clean");
    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore (shrink)");

    let letters = wait_for_disk_volumes(idx, 2, 60_000).expect("restored volumes");
    let after = partition_summary(idx).expect("summary after");
    eprintln!("[T3] restored (shrunk) partitions: {after:?}");
    assert!(
        after[0].1 <= 600 * MB,
        "NTFS should have shrunk to ~512 MB, got {} bytes",
        after[0].1
    );
    chkdsk_clean(letters[0]).expect("chkdsk on shrunk NTFS");
    verify_fixture(letters[0], &d_ntfs).expect("NTFS fixture preserved across shrink");
    verify_fixture(letters[1], &d_fat).expect("FAT32 fixture preserved");
    // Deeper structural check on the shrunk+relocated NTFS: an offline
    // chkdsk /F (works where the online /scan can't snapshot). Do this last —
    // it force-dismounts the volume.
    chkdsk_offline_fix(letters[0]).expect("offline chkdsk on shrunk NTFS");

    let _ = std::fs::remove_file(&backup);
    eprintln!("[T3] real_mbr_restore_shrink PASSED");
}

#[test]
#[ignore = "DESTRUCTIVE: wipes the real USB disk; set PHOENIX_T3_DISK"]
fn real_mbr_exfat_roundtrip() {
    require_admin();
    let Some(disk) = skip_or_disk() else {
        return;
    };
    let idx = disk.index();

    // exFAT (used-block capture via the allocation bitmap) + NTFS. exFAT first
    // (kept at size), NTFS last (grows to fill on the full-disk restore).
    disk.layout(
        false,
        &[
            PartSpec {
                size_mb: 512,
                fs: TestFs::Exfat,
                letter: 'R',
                label: "T3EXFAT".into(),
            },
            PartSpec {
                size_mb: 512,
                fs: TestFs::Ntfs,
                letter: 'S',
                label: "T3NTFS".into(),
            },
        ],
    )
    .expect("MBR layout");
    assert!(wait_for_letter('R', 20_000), "exFAT didn't mount");
    assert!(wait_for_letter('S', 20_000), "NTFS didn't mount");
    let d_exfat = fill_fixture('R', 0xCC01).expect("fill exfat");
    let d_ntfs = fill_fixture('S', 0xCC02).expect("fill ntfs");

    let backup = backup_all(idx);
    let disk_size = disk_size_bytes(idx);

    disk.clean().expect("clean");
    let reader = PhnxReader::open(&backup).unwrap();
    let plan = default_plan_from_backup(backup.to_str().unwrap(), &reader, idx, disk_size);
    for e in &plan.entries {
        eprintln!(
            "[T3] exfat restore plan: src={:?} off={} size={}",
            e.source_partition_index, e.target_offset_bytes, e.target_size_bytes
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

    let letters = wait_for_disk_volumes(idx, 2, 60_000).expect("restored volumes");
    eprintln!("[T3] restored letters: {letters:?}");
    // Offset order: [0] = exFAT, [1] = NTFS.
    verify_fixture(letters[0], &d_exfat).expect("exFAT fixture preserved across restore");
    chkdsk_clean(letters[1]).expect("chkdsk on restored NTFS");
    verify_fixture(letters[1], &d_ntfs).expect("NTFS fixture preserved across restore");

    let _ = std::fs::remove_file(&backup);
    eprintln!("[T3] real_mbr_exfat_roundtrip PASSED");
}

#[test]
#[ignore = "DESTRUCTIVE: wipes the real USB disk; set PHOENIX_T3_DISK"]
fn real_mbr_bitlocker_roundtrip() {
    use phoenix_core::disk::{BitlockerState, CaptureMode, FilesystemKind};

    const PW: &str = "Phoenix-T3-Bl0cker!";

    require_admin();
    let Some(disk) = skip_or_disk() else {
        return;
    };
    let idx = disk.index();

    // Real MBR NTFS volume on real flash, then BitLocker-encrypt it.
    disk.layout(
        false,
        &[PartSpec {
            size_mb: 2048,
            fs: TestFs::Ntfs,
            letter: 'R',
            label: "T3BL".into(),
        }],
    )
    .expect("MBR layout");
    assert!(wait_for_letter('R', 20_000), "NTFS didn't mount");
    let digest = fill_fixture('R', 0xEE01).expect("fill fixture");
    enable_bitlocker_password('R', PW).expect("enable bitlocker");

    // UNLOCKED → normal NTFS used-block classification and capture.
    let disks = enumerate_disks().unwrap();
    let part = disks
        .iter()
        .find(|d| d.index == idx)
        .unwrap()
        .partitions
        .iter()
        .find(|p| p.drive_letter == Some('R'))
        .expect("source partition")
        .clone();
    assert_eq!(part.fs_kind, FilesystemKind::Ntfs);
    assert_eq!(part.capture_mode, CaptureMode::UsedBlocks);
    assert_eq!(part.bitlocker, BitlockerState::Unlocked);
    let unlocked_backup = backup_all(idx);
    {
        let reader = PhnxReader::open(&unlocked_backup).unwrap();
        let pm = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.fs == "ntfs")
            .expect("ntfs in manifest");
        assert_eq!(pm.bitlocker.as_deref(), Some("unlocked"));
    }

    // LOCKED → raw ciphertext capture.
    lock_bitlocker('R').expect("lock");
    let disks = enumerate_disks().unwrap();
    let part = disks
        .iter()
        .find(|d| d.index == idx)
        .unwrap()
        .partitions
        .iter()
        .find(|p| p.fs_kind == FilesystemKind::Bitlocker)
        .expect("locked partition classified Bitlocker")
        .clone();
    assert_eq!(part.capture_mode, CaptureMode::Raw);
    assert_eq!(part.bitlocker, BitlockerState::Locked);
    let locked_backup = backup_all(idx);
    {
        let reader = PhnxReader::open(&locked_backup).unwrap();
        let pm = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.fs == "bitlocker")
            .expect("bitlocker in manifest");
        assert_eq!(pm.capture_mode, "raw");
        assert_eq!(pm.bitlocker.as_deref(), Some("locked"));
    }

    let disk_size = disk_size_bytes(idx);

    // Restore the CIPHERTEXT image: volume comes back locked, original
    // password unlocks it, fixture intact.
    //
    // A `clean` of this disk once failed with 0x80310000 (FVE facility)
    // while the volume sat in the locked, force-dismounted state — likely a
    // wedged dismount rather than "locked disks can't be wiped" (diskpart
    // normally cleans locked BitLocker disks fine). Unlocking first is
    // cheap insurance either way; the ciphertext is already safely
    // captured in `locked_backup`.
    unlock_bitlocker_password('R', PW).expect("unlock source before clean");
    disk.clean().expect("clean");
    let reader = PhnxReader::open(&locked_backup).unwrap();
    let plan = default_plan_from_backup(locked_backup.to_str().unwrap(), &reader, idx, disk_size);
    drop(reader);
    run_restore(RestoreOptions {
        backup_path: locked_backup.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore (ciphertext)");

    // THE HARD GUARANTEE — proven from the on-disk bytes, not Windows' opinion.
    //
    // `Get-BitLockerVolume`/`manage-bde` report the OS's *cached mounted-volume*
    // view, which on removable USB media stays stale in-session after a raw
    // rewrite: the ciphertext sectors land under a volume Windows auto-mounted
    // mid-restore, and nothing short of a re-plug/reboot makes it re-read them
    // (FSCTL_DISMOUNT_VOLUME doesn't clear it on removable media). So we assert
    // the property directly: read the restored partition's boot sector straight
    // off `\\.\PhysicalDriveN` and require the BitLocker `-FVE-FS-` signature.
    // Combined with verify-on-restore (every byte matched the captured image)
    // this proves the restore reproduced the encrypted volume, not plaintext.
    // Scan all partitions for the signature (robust to layout/MSR ordering).
    let tags = disk_partition_boot_tags(idx).expect("scan restored partitions");
    assert!(
        tags.iter().any(|(_, _, t)| t == "-FVE-FS-"),
        "no restored partition carries the BitLocker -FVE-FS- signature; saw {tags:?}"
    );
    eprintln!("[T3] a restored partition boot sector is -FVE-FS- (ciphertext intact): {tags:?}");

    // Best-effort end-to-end: try to get Windows to recognize + unlock the
    // restored volume and confirm the fixture. On removable media Windows may
    // refuse to re-read the volume in-session (see above), so this leg is
    // advisory — a failure to *recognize* is logged and skipped, but if it
    // DOES unlock, the fixture must be intact.
    rescan_disk_volumes(idx).ok();
    if let Some(letter) = wait_for_letter_even_if_locked(idx, 30_000) {
        match bitlocker_status(letter) {
            Ok((vs, ls, _)) if vs == "FullyEncrypted" => {
                if ls == "Unlocked" {
                    eprintln!(
                        "[T3] restored volume arrived unlocked (session key cache); re-locking"
                    );
                    let _ = lock_bitlocker(letter);
                }
                unlock_bitlocker_password(letter, PW).expect("unlock restored volume");
                assert!(wait_for_letter(letter, 15_000), "unlocked root unreachable");
                verify_fixture(letter, &digest)
                    .expect("fixture preserved through ciphertext roundtrip");
                eprintln!("[T3] end-to-end unlock + fixture verify passed");
            }
            other => {
                eprintln!(
                    "[T3] OS did not re-recognize the restored BitLocker volume in-session \
                     (status {other:?}); on-disk -FVE-FS- check already proved the ciphertext. \
                     Skipping the in-session unlock leg (would need a re-plug/reboot)."
                );
            }
        }
    }

    // Restore the PLAINTEXT (unlocked-capture) image: volume comes back as
    // a normal unencrypted NTFS volume.
    disk.clean().expect("clean");
    let reader = PhnxReader::open(&unlocked_backup).unwrap();
    let plan = default_plan_from_backup(unlocked_backup.to_str().unwrap(), &reader, idx, disk_size);
    drop(reader);
    run_restore(RestoreOptions {
        backup_path: unlocked_backup.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore (plaintext)");
    let letter = wait_for_restored_letter(idx, 30_000).expect("restored plaintext got no letter");
    if let Ok((vs, ls, _)) = bitlocker_status(letter) {
        assert_eq!(
            vs, "FullyDecrypted",
            "plaintext restore must not be encrypted"
        );
        assert_eq!(ls, "Unlocked");
    }
    chkdsk_clean(letter).expect("chkdsk on restored plaintext NTFS");
    verify_fixture(letter, &digest).expect("fixture preserved through plaintext roundtrip");

    let _ = std::fs::remove_file(&unlocked_backup);
    let _ = std::fs::remove_file(&locked_backup);
    eprintln!("[T3] real_mbr_bitlocker_roundtrip PASSED");
}

/// The lock-then-VSS escalation on real media. Adapts to what the hardware
/// supports — in both cases we hold a file handle open, which is exactly what
/// makes `FSCTL_LOCK_VOLUME` fail and forces the engine onto its second arm:
/// - **Non-removable** target (external/internal HDD): VSS should work, so the
///   engine must escalate to a shadow and the backup succeeds despite our
///   handle. Then restore+verify.
/// - **Removable** flash: Windows can't shadow removable media, so there is no
///   second arm — the backup MUST abort with a lock error. With handles closed
///   it must then lock, capture faithfully, and release.
#[test]
#[ignore = "DESTRUCTIVE: wipes the real disk; set PHOENIX_T3_DISK"]
fn real_vss_backup_roundtrip() {
    use std::io::Write;

    require_admin();
    let Some(disk) = skip_or_disk() else {
        return;
    };
    let idx = disk.index();

    disk.layout(
        !disk.is_removable(), // GPT where possible, MBR on removable
        &[PartSpec {
            size_mb: 2048,
            fs: TestFs::Ntfs,
            letter: 'R',
            label: "T3VSS".into(),
        }],
    )
    .expect("layout");
    assert!(wait_for_letter('R', 20_000), "NTFS didn't mount");
    let digest = fill_fixture('R', 0xF0F0).expect("fill fixture");
    let disk_size = disk_size_bytes(idx);

    // Probe: can this volume actually be shadowed? (Removable flash can't.)
    let shadow_works = {
        let mut probe = phoenix_vss::VssSession::new();
        let path = probe.snapshot_volume(r"\\.\R:").unwrap_or_default();
        path.contains("HarddiskVolumeShadowCopy")
    };
    eprintln!("[T3] VSS probe on R:: shadow_works={shadow_works}");

    let mut held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(r"R:\held-open.bin")
        .expect("open held file");
    held.write_all(b"held during backup").expect("write held");

    let backup =
        std::env::temp_dir().join(format!("t3-vss-{}.phnx", uuid::Uuid::new_v4().simple()));
    let result = run_backup(BackupOptions {
        disk_index: idx,
        partition_indices: all_partition_indices(idx),
        output: backup.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    });

    let backup = if shadow_works {
        // Shadow media: the held handle blocks the lock, so a success here
        // proves the engine escalated to a shadow on its own.
        result.expect("backup failed on shadow-capable media — escalation to VSS did not happen");
        drop(held);
        eprintln!("[T3] VSS shadow backup succeeded with a handle held open");
        backup
    } else {
        // Removable media: no shadow available, so the unlockable volume must
        // abort the backup rather than smear a live read.
        match result {
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                assert!(
                    msg.contains("lock"),
                    "unexpected failure (wanted lock): {e}"
                );
                eprintln!("[T3] unfreezable volume correctly aborted the backup: {e}");
            }
            Ok(_) => panic!(
                "backup succeeded with VSS unavailable and a handle held open — the engine \
                 captured an unfrozen, mutating source instead of aborting"
            ),
        }
        drop(held);
        // Now with handles closed the volume must lock and produce a good backup.
        run_backup(BackupOptions {
            disk_index: idx,
            partition_indices: all_partition_indices(idx),
            output: backup.clone(),
            verify_after: true,
            verify_image: false,
            progress: None,
        })
        .expect("locked backup failed");
        std::fs::write(r"R:\post-backup.txt", b"unlocked").expect("volume still locked");
        backup
    };

    // Wipe + restore back onto the same disk; fixture must survive.
    disk.clean().expect("clean");
    let reader = PhnxReader::open(&backup).unwrap();
    let layout_size = std::env::var("PHOENIX_T3_LAYOUT_GB")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|gb| ((gb * 1e9) as u64).min(disk_size))
        .unwrap_or(disk_size);
    let plan = default_plan_from_backup(backup.to_str().unwrap(), &reader, idx, layout_size);
    drop(reader);
    run_restore(RestoreOptions {
        backup_path: backup.clone(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");
    let letter = wait_for_restored_letter(idx, 30_000).expect("restored volume got no letter");
    verify_fixture(letter, &digest).expect("fixture preserved through VSS/fallback backup");

    let _ = std::fs::remove_file(&backup);
    eprintln!("[T3] real_vss_backup_roundtrip PASSED");
}

#[test]
#[ignore = "DESTRUCTIVE: wipes the real USB disk; set PHOENIX_T3_DISK"]
fn real_clone_to_vhd() {
    require_admin();
    let Some(disk) = skip_or_disk() else {
        return;
    };
    let idx = disk.index();

    // Source: the real USB disk, MBR NTFS + FAT32, filled.
    disk.layout(
        false,
        &[
            PartSpec {
                size_mb: 1024,
                fs: TestFs::Ntfs,
                letter: 'R',
                label: "T3NTFS".into(),
            },
            PartSpec {
                size_mb: 512,
                fs: TestFs::Fat32,
                letter: 'S',
                label: "T3FAT".into(),
            },
        ],
    )
    .expect("MBR layout");
    assert!(wait_for_letter('R', 20_000), "NTFS didn't mount");
    assert!(wait_for_letter('S', 20_000), "FAT didn't mount");
    let d_ntfs = fill_fixture('R', 0xDD01).expect("fill ntfs");
    let d_fat = fill_fixture('S', 0xDD02).expect("fill fat");

    // Target: a VHD at least as large as the real disk (identity clone).
    let target = TestVhd::create(31_000).expect("create target vhd");

    // Enumerate both disks with filesystems refined (clone needs fs types).
    let mut disks = enumerate_disks().unwrap();
    for d in &mut disks {
        for p in &mut d.partitions {
            refine_partition_fs(p);
        }
    }
    let src = disks.iter().find(|d| d.index == idx).unwrap().clone();
    let tgt = disks
        .iter()
        .find(|d| d.index == target.disk_index())
        .unwrap()
        .clone();

    let plan = ClonePlan::identity(&src);
    plan.validate(&src, &tgt).expect("clone plan valid");
    run_clone(CloneOptions {
        source_disk_index: idx,
        target_disk_index: target.disk_index(),
        plan,
        verify: CloneVerify::ReadBack,
        use_vss: false,
        progress: None,
    })
    .expect("run_clone real -> vhd");

    // Verify the CLONED VHD's volumes carry the fixtures byte-for-byte.
    let letters =
        wait_for_disk_volumes(target.disk_index(), 2, 60_000).expect("cloned volumes got letters");
    eprintln!("[T3] cloned VHD letters: {letters:?}");
    chkdsk_clean(letters[0]).expect("chkdsk on cloned NTFS");
    verify_fixture(letters[0], &d_ntfs).expect("NTFS fixture preserved by clone");
    verify_fixture(letters[1], &d_fat).expect("FAT32 fixture preserved by clone");

    eprintln!("[T3] real_clone_to_vhd PASSED");
}
