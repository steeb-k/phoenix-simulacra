//! Tier-2 VSS verification: prove the shadow-copy path actually WORKS —
//! not merely that a backup with `use_vss: true` doesn't error.
//!
//! Why the paranoia: `VssSession::snapshot_volume` deliberately falls back to
//! the live volume path on any snapshot failure (a failed snapshot shouldn't
//! abort a backup), which means a naive `use_vss: true` round-trip test could
//! pass with VSS completely broken. Both tests here are constructed so a
//! silent fallback makes them FAIL:
//!
//! 1. `vss_snapshot_is_point_in_time` asserts the returned device path is a
//!    real shadow (`HarddiskVolumeShadowCopy`), then modifies a file and reads
//!    the ORIGINAL bytes back through the shadow — the actual point-in-time
//!    guarantee VSS exists to provide.
//! 2. `vss_backup_roundtrip_with_open_handle` runs a real backup with
//!    `use_vss: true` while holding an open file handle on the volume. If the
//!    shadow was used, the engine never locks the volume and the backup
//!    succeeds. If VSS silently fell back, the engine's `FSCTL_LOCK_VOLUME`
//!    hits our open handle and the backup errors — so a green run proves the
//!    shadow was genuinely in the read path. The fixture is then restored and
//!    verified byte-for-byte.
//!
//! The FALLBACK (VSS fails → live volume + `FSCTL_LOCK_VOLUME`) is verified
//! too, forced deterministically: VSS only supports NTFS/ReFS, so
//! `use_vss: true` on a FAT32 volume always fails snapshot creation and takes
//! the fallback path.
//!
//! 3. `vss_fallback_enforces_volume_lock` — FAT32 + `use_vss: true` + an open
//!    handle: the backup MUST fail with a lock error. If the fallback skipped
//!    locking (and smeared a live read), the backup would succeed and the test
//!    fails.
//! 4. `vss_fallback_locked_roundtrip` — FAT32 + `use_vss: true`, no open
//!    handles: the fallback locks, backs up, unlocks (proven by writing to the
//!    volume afterward), and the restored fixture matches byte-for-byte.
//!
//! Requires an elevated shell (diskpart + VSS need admin):
//!   cargo test -p phoenix-systests --test vss -- --ignored --test-threads=1 --nocapture

use std::io::Write;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_restore::plan::default_plan_from_backup;
use phoenix_restore::restore::{run_restore, RestoreOptions};
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture, wait_for_letter,
    wait_for_restored_letter, PartSpec, TestFs, TestVhd,
};
use phoenix_vss::VssSession;

const VHD_MB: u64 = 1024;

fn make_vhd(letter: char, label: &str, fs: TestFs) -> TestVhd {
    let vhd = TestVhd::create(VHD_MB).expect("create vhd");
    vhd.init_gpt_with(&[PartSpec {
        size_mb: 0,
        fs,
        letter,
        label: label.into(),
    }])
    .expect("init vhd");
    assert!(wait_for_letter(letter, 15_000), "test volume never mounted");
    vhd
}

fn make_ntfs_vhd(letter: char, label: &str) -> TestVhd {
    make_vhd(letter, label, TestFs::Ntfs)
}

fn backup_disk(disk_index: u32, use_vss: bool) -> phoenix_core::error::Result<std::path::PathBuf> {
    let disks = phoenix_core::disk::enumerate_disks().unwrap();
    let disk = disks.iter().find(|d| d.index == disk_index).unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    let backup_path =
        std::env::temp_dir().join(format!("vss-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index,
        partition_indices: parts,
        output: backup_path.clone(),
        use_vss,
        verify_after: true,
        progress: None,
    })
    .map(|_| backup_path)
}

fn restore_and_verify(backup_path: &std::path::Path, digest: &phoenix_systests::FixtureDigest) {
    let target = TestVhd::create(VHD_MB).expect("create target vhd");
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
    let letter = wait_for_restored_letter(target.disk_index(), 30_000)
        .expect("restored volume got no letter");
    verify_fixture(letter, digest).expect("fixture preserved");
}

/// VSS's core promise: the shadow is a frozen point-in-time copy. Snapshot,
/// modify a file, then read the ORIGINAL content back through the shadow
/// device. Also hard-asserts we got a real shadow path — a silent fallback
/// returns the live volume path and fails the first assertion.
#[test]
#[ignore = "requires elevation + diskpart + VSS"]
fn vss_snapshot_is_point_in_time() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let _vhd = make_ntfs_vhd('X', "VSSPIT");
    let marker = std::path::PathBuf::from(r"X:\vss-marker.txt");
    std::fs::write(&marker, b"ORIGINAL-CONTENT").expect("write marker");

    // Session must outlive the shadow reads; its Drop deletes the snapshot.
    let mut session = VssSession::new();
    let shadow = session
        .snapshot_volume(r"\\.\X:")
        .expect("snapshot_volume errored");
    assert!(
        shadow.contains("HarddiskVolumeShadowCopy"),
        "expected a real shadow device path, got {shadow:?} — VSS silently fell back \
         to the live volume (snapshot creation failed; check elevation / VSS service)"
    );
    eprintln!("[vss] shadow created: {shadow}");

    // Mutate the live file AFTER the snapshot.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&marker)
        .expect("reopen marker");
    f.write_all(b"MODIFIED-AFTER-SNAPSHOT").expect("modify");
    f.sync_all().ok();
    drop(f);

    // The live volume sees the new content; the shadow must still serve the old.
    let live = std::fs::read(&marker).expect("read live");
    assert_eq!(live, b"MODIFIED-AFTER-SNAPSHOT");
    let shadow_file = format!("{shadow}\\vss-marker.txt");
    let frozen =
        std::fs::read(&shadow_file).unwrap_or_else(|e| panic!("reading {shadow_file} failed: {e}"));
    assert_eq!(
        frozen, b"ORIGINAL-CONTENT",
        "shadow did not preserve the point-in-time content"
    );
    eprintln!("[vss] point-in-time semantics verified (shadow serves pre-modification bytes)");
}

/// End-to-end: a real backup through VSS while the volume has an open file
/// handle. The open handle makes `FSCTL_LOCK_VOLUME` fail, so this backup can
/// only succeed if the engine read from a shadow (which needs no lock) — a
/// green run proves the VSS path was genuinely used, then the restored fixture
/// proves the shadow's bytes were a faithful copy.
#[test]
#[ignore = "requires elevation + diskpart + VSS"]
fn vss_backup_roundtrip_with_open_handle() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = make_ntfs_vhd('X', "VSSSRC");
    let digest = fill_fixture('X', 0x5555).expect("fill fixture");

    // Hold an open handle (with unflushed writes for good measure) for the
    // entire backup. FSCTL_LOCK_VOLUME fails while this handle is open, so a
    // silent VSS fallback -> live-volume lock path cannot pass.
    let mut held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(r"X:\held-open.bin")
        .expect("open held file");
    held.write_all(b"held open during backup")
        .expect("write held");

    let backup_path = backup_disk(source.disk_index(), true).expect(
        "run_backup with use_vss=true failed — with a handle held open this almost always \
         means VSS snapshot creation failed and the engine fell back to a live-volume lock",
    );

    // Keep the handle alive until after the backup finishes, then release it.
    drop(held);

    // Restore to a fresh VHD and verify every fixture byte survived the
    // shadow-read path.
    restore_and_verify(&backup_path, &digest);
    eprintln!("[vss] end-to-end VSS backup roundtrip verified (lock was never taken)");

    let _ = std::fs::remove_file(&backup_path);
}

/// FORCED-FALLBACK exclusivity: VSS can't snapshot FAT32, so `use_vss: true`
/// on a FAT32 volume always falls back to the live volume — where the engine
/// must take `FSCTL_LOCK_VOLUME`. With a handle held open, that lock MUST
/// fail and abort the backup. If this backup ever *succeeds*, the fallback
/// skipped locking and smeared a live read — exactly the bug this guards.
#[test]
#[ignore = "requires elevation + diskpart"]
fn vss_fallback_enforces_volume_lock() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = make_vhd('X', "VSSFB1", TestFs::Fat32);
    let _ = fill_fixture('X', 0x6666).expect("fill fixture");

    let mut held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(r"X:\held-open.bin")
        .expect("open held file");
    held.write_all(b"held").expect("write held");

    let result = backup_disk(source.disk_index(), true);
    match result {
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("lock"),
                "backup failed for an unexpected reason (wanted a volume-lock failure): {e}"
            );
            eprintln!("[vss] fallback correctly enforced the volume lock: {e}");
        }
        Ok(p) => {
            let _ = std::fs::remove_file(&p);
            panic!(
                "backup SUCCEEDED with VSS unavailable and a handle held open — the \
                 live-volume fallback did not enforce FSCTL_LOCK_VOLUME"
            );
        }
    }
    drop(held);
}

/// FORCED-FALLBACK round-trip: FAT32 + `use_vss: true`, no open handles. The
/// fallback must lock the live volume, produce a faithful backup
/// (verify-after re-reads under the same lock), release the lock on
/// completion (proven by writing to the volume afterward), and restore
/// byte-for-byte.
#[test]
#[ignore = "requires elevation + diskpart"]
fn vss_fallback_locked_roundtrip() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = make_vhd('X', "VSSFB2", TestFs::Fat32);
    let digest = fill_fixture('X', 0x7777).expect("fill fixture");

    let backup_path =
        backup_disk(source.disk_index(), true).expect("fallback (live-volume lock) backup failed");

    // The lock must be released once the backup completes.
    std::fs::write(r"X:\post-backup-write.txt", b"unlocked again")
        .expect("volume still locked after backup returned");

    restore_and_verify(&backup_path, &digest);
    eprintln!("[vss] forced-fallback locked backup round-trip verified (lock taken + released)");

    let _ = std::fs::remove_file(&backup_path);
}
