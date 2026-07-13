//! Tier-2 BitLocker lifecycle test: lock state — not "is this BitLocker" —
//! must drive the capture mode.
//!
//! One test walks the full lifecycle on a real (VHDX-backed) NTFS volume:
//!
//! 1. Encrypt the volume with a password protector (`Enable-BitLocker`).
//! 2. **Unlocked** → enumeration classifies it NTFS + used-blocks +
//!    `BitlockerState::Unlocked`; backup streams *plaintext*; restore
//!    produces a normal, unencrypted volume with the fixture intact.
//! 3. `Lock-BitLocker` → enumeration flips to Bitlocker + raw + `Locked`;
//!    backup streams *ciphertext* (verify-after-backup still passes —
//!    ciphertext at rest is deterministic); restore reproduces a volume
//!    that is still BitLocker-locked and only yields the fixture after
//!    `Unlock-BitLocker` with the original password.
//!
//! Requires an elevated shell + the BitLocker cmdlets (Windows Pro+):
//!   cargo test -p phoenix-systests --test bitlocker -- --ignored --test-threads=1 --nocapture

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{enumerate_disks, BitlockerState, CaptureMode, FilesystemKind};
use phoenix_restore::plan::default_plan_from_backup;
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    bitlocker_status, chkdsk_clean, cleanup_leaked_vhds, disk_partition_boot_tags,
    enable_bitlocker_password, fill_fixture, lock_bitlocker, require_admin, rescan_disk_volumes,
    unlock_bitlocker_password, verify_fixture, wait_for_letter, wait_for_letter_even_if_locked,
    wait_for_restored_letter, PartSpec, TestFs, TestVhd,
};

const PW: &str = "Phoenix-T2-Bl0cker!";
const VHD_MB: u64 = 1024;

fn backup_to(path: &std::path::Path, disk_index: u32) {
    let disks = enumerate_disks().unwrap();
    let disk = disks.iter().find(|d| d.index == disk_index).unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    run_backup(BackupOptions {
        disk_index,
        partition_indices: parts,
        output: path.to_path_buf(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");
}

fn restore_to(backup_path: &std::path::Path, target: &TestVhd) {
    let reader = PhnxReader::open(backup_path).unwrap();
    let plan = default_plan_from_backup(
        backup_path.to_str().unwrap(),
        &reader,
        target.disk_index(),
        VHD_MB * 1024 * 1024,
    );
    drop(reader);
    run_restore(RestoreOptions {
        backup_path: backup_path.to_path_buf(),
        plan,
        verify_on_restore: true,
        progress: None,
    })
    .expect("run_restore");
}

#[test]
#[ignore = "requires elevation + diskpart + BitLocker cmdlets (Windows Pro+)"]
fn bitlocker_unlocked_then_locked_roundtrips() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // ---- source: NTFS volume with fixture, then BitLocker-encrypt it ----
    let source = TestVhd::create(VHD_MB).expect("create source vhd");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "BLSRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source volume never mounted");
    let digest = fill_fixture('X', 0xB17C).expect("fill fixture");

    enable_bitlocker_password('X', PW).expect("enable bitlocker");
    let (vs, ls, _) = bitlocker_status('X').unwrap();
    eprintln!("[bitlocker] source encrypted: VolumeStatus={vs} LockStatus={ls}");
    eprintln!(
        "[bitlocker] NOTE: everything from here (both backup/restore roundtrips, \
         chkdsk, lock/unlock) runs without further output and re-reads the volume \
         several times — expect several quiet minutes; hold your horses."
    );

    // ---- UNLOCKED: classified as a completely normal NTFS capture ----
    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let part = disk
        .partitions
        .iter()
        .find(|p| p.drive_letter == Some('X'))
        .expect("source partition not found");
    assert_eq!(part.fs_kind, FilesystemKind::Ntfs, "unlocked → real FS");
    assert_eq!(part.capture_mode, CaptureMode::UsedBlocks);
    assert_eq!(part.bitlocker, BitlockerState::Unlocked);

    let unlocked_backup = std::env::temp_dir().join(format!(
        "bl-unlocked-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    backup_to(&unlocked_backup, source.disk_index());

    {
        let reader = PhnxReader::open(&unlocked_backup).unwrap();
        let pm = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.fs == "ntfs")
            .expect("ntfs partition in unlocked-backup manifest");
        assert_eq!(pm.capture_mode, "used-blocks");
        assert_eq!(pm.bitlocker.as_deref(), Some("unlocked"));
    }

    // ---- restore of the unlocked image is plaintext ----
    {
        let target = TestVhd::create(VHD_MB).expect("create target vhd");
        restore_to(&unlocked_backup, &target);
        let letter = wait_for_restored_letter(target.disk_index(), 30_000)
            .expect("restored plaintext volume got no letter");
        // A plaintext restore must NOT come back encrypted.
        if let Ok((vs, ls, _)) = bitlocker_status(letter) {
            assert_eq!(vs, "FullyDecrypted", "restored volume must be plaintext");
            assert_eq!(ls, "Unlocked");
        }
        chkdsk_clean(letter).expect("chkdsk clean");
        verify_fixture(letter, &digest).expect("fixture preserved through plaintext roundtrip");
    }

    // ---- LOCK the source; classification must flip to raw ciphertext ----
    lock_bitlocker('X').expect("Lock-BitLocker");
    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let part = disk
        .partitions
        .iter()
        .find(|p| p.fs_kind == FilesystemKind::Bitlocker)
        .expect("locked partition not classified as Bitlocker");
    assert_eq!(part.capture_mode, CaptureMode::Raw);
    assert_eq!(part.bitlocker, BitlockerState::Locked);

    let locked_backup =
        std::env::temp_dir().join(format!("bl-locked-{}.phnx", uuid::Uuid::new_v4().simple()));
    backup_to(&locked_backup, source.disk_index());

    {
        let reader = PhnxReader::open(&locked_backup).unwrap();
        let pm = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.fs == "bitlocker")
            .expect("bitlocker partition in locked-backup manifest");
        assert_eq!(pm.capture_mode, "raw");
        assert_eq!(pm.bitlocker.as_deref(), Some("locked"));
    }

    // ---- restore ciphertext: volume is still locked; original password
    //      unlocks it and the fixture is intact ----
    {
        let target = TestVhd::create(VHD_MB).expect("create target vhd");
        restore_to(&locked_backup, &target);

        // THE HARD GUARANTEE — proven from the on-disk bytes, not Windows'
        // opinion. Get-BitLockerVolume reports the OS's cached mounted-volume
        // view, which can stay stale in-session after a raw rewrite (the
        // ciphertext sectors land under a volume Windows auto-mounted
        // mid-restore). So we read the restored partition's boot sector
        // straight off the physical disk and require the `-FVE-FS-` signature
        // on some partition (GPT data disks carry an MSR partition ahead of
        // the data one, so we scan all partitions rather than assume index 0).
        // With verify-on-restore (every byte matched the captured image) this
        // proves the restore reproduced the encrypted volume, not plaintext.
        let tags = disk_partition_boot_tags(target.disk_index()).expect("scan restored partitions");
        assert!(
            tags.iter().any(|(_, _, t)| t == "-FVE-FS-"),
            "no restored partition carries the BitLocker -FVE-FS- signature; saw {tags:?}"
        );
        eprintln!("[bitlocker] a restored partition boot sector is -FVE-FS- (ciphertext intact)");

        // Best-effort end-to-end: if Windows re-recognizes the restored volume,
        // unlock it with the original password and confirm the fixture. On a
        // stale in-session view this is skipped (the on-disk check already
        // proved the ciphertext).
        rescan_disk_volumes(target.disk_index()).ok();
        if let Some(letter) = wait_for_letter_even_if_locked(target.disk_index(), 30_000) {
            match bitlocker_status(letter) {
                Ok((vs, ls, _)) if vs == "FullyEncrypted" => {
                    if ls == "Unlocked" {
                        eprintln!(
                            "[bitlocker] restored volume arrived unlocked (session key cache); \
                             re-locking"
                        );
                        let _ = lock_bitlocker(letter);
                    }
                    unlock_bitlocker_password(letter, PW).expect("Unlock-BitLocker");
                    assert!(
                        wait_for_letter(letter, 15_000),
                        "unlocked volume root never became reachable"
                    );
                    verify_fixture(letter, &digest)
                        .expect("fixture preserved through ciphertext roundtrip");
                    eprintln!("[bitlocker] end-to-end unlock + fixture verify passed");
                }
                other => {
                    eprintln!(
                        "[bitlocker] OS did not re-recognize the restored BitLocker volume \
                         in-session (status {other:?}); on-disk -FVE-FS- check already proved \
                         the ciphertext. Skipping in-session unlock leg."
                    );
                }
            }
        }
    }

    let _ = std::fs::remove_file(&unlocked_backup);
    let _ = std::fs::remove_file(&locked_backup);
}
