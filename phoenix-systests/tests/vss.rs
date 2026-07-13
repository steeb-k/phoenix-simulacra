//! Tier-2 verification of the engine's consistency policy. There is no user
//! switch: every backup takes the strongest freeze the volume allows —
//! `FSCTL_LOCK_VOLUME` first, a VSS shadow when the volume is too busy to
//! lock, abort when a lettered volume allows neither. These tests pin all
//! three arms, and prove the shadow path actually WORKS rather than merely
//! not erroring.
//!
//! Why the paranoia: `VssSession::snapshot_volume` deliberately falls back to
//! returning the live volume path on any snapshot failure, so a naive
//! round-trip test could pass with VSS completely broken. Each test below is
//! constructed so a silent fallback makes it FAIL:
//!
//! 1. `vss_snapshot_is_point_in_time` asserts the returned device path is a
//!    real shadow (`HarddiskVolumeShadowCopy`), then modifies a file and reads
//!    the ORIGINAL bytes back through the shadow — the actual point-in-time
//!    guarantee VSS exists to provide.
//! 2. `busy_volume_escalates_to_vss_shadow` runs a real backup of an NTFS
//!    volume while holding an open file handle on it. The handle makes
//!    `FSCTL_LOCK_VOLUME` fail, so the backup can only succeed if the engine
//!    escalated to a shadow on its own — a green run proves the shadow was
//!    genuinely in the read path. The fixture is then restored and verified
//!    byte-for-byte.
//! 3. `busy_volume_without_vss_aborts_backup` — VSS only supports NTFS/ReFS,
//!    so a FAT32 volume with an open handle can be neither locked nor
//!    snapshotted. The backup MUST fail with a lock error; if it ever
//!    succeeds, the engine smeared a live read into an image the user would
//!    trust.
//! 4. `idle_fat32_volume_locked_roundtrip` — FAT32, no open handles: the
//!    engine locks (never reaching the shadow arm it couldn't use anyway),
//!    backs up, releases the lock (proven by writing to the volume
//!    afterward), and the restored fixture matches byte-for-byte.
//!
//! And the lock's OUTBOUND exclusivity — while the lock is held, the rest of
//! the system must be unable to open/write files on the volume (that's the
//! consistency `FSCTL_LOCK_VOLUME` buys, and why it outranks a shadow):
//!
//! 5. `volume_lock_blocks_external_writes` — take the lock via the engine's
//!    own primitive and prove external file creates/opens fail while it's
//!    held, then succeed after release. Fully deterministic.
//! 6. `locked_backup_blocks_writers_for_duration` — back up an idle volume
//!    (which the engine therefore locks rather than snapshots) on a thread
//!    while this test hammers write attempts at it: at least one attempt must
//!    be refused during the backup window, and writes must succeed again once
//!    it completes.
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

fn backup_disk(disk_index: u32) -> phoenix_core::error::Result<std::path::PathBuf> {
    let disks = phoenix_core::disk::enumerate_disks().unwrap();
    let disk = disks.iter().find(|d| d.index == disk_index).unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    let backup_path =
        std::env::temp_dir().join(format!("vss-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index,
        partition_indices: parts,
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .map(|_| backup_path)
}

/// A freshly filled volume is frequently still being scanned by Windows
/// Defender and the search indexer, which hold open handles that make
/// `FSCTL_LOCK_VOLUME` return ERROR_ACCESS_DENIED (Win32 error 5). Block until
/// the volume is quiescent enough to lock cleanly — proven by taking and
/// immediately releasing the lock through the engine's own primitive (its
/// internal 5×/~3.75 s retry rides out a transient scanner). This keeps the
/// scanner from racing the timed backup's pre-flight lock; best-effort on
/// timeout so a genuinely stuck volume still surfaces the real error below.
fn wait_until_lockable(volume: &str, timeout_ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        // 512: these fixtures are diskpart VHDs, which are always 512-byte
        // sector. The reader is only used to take and drop a lock here — no
        // reads — so the sector size is inert either way.
        if let Ok(mut reader) = phoenix_capture::PartitionReader::open_volume(volume, 512) {
            if reader.lock_volume(volume).is_ok() {
                return; // reader drops here → FSCTL_UNLOCK_VOLUME
            }
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
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

/// End-to-end escalation: a real backup of a volume that has an open file
/// handle. The open handle makes `FSCTL_LOCK_VOLUME` fail, so this backup can
/// only succeed if the engine escalated to a shadow on its own (which needs no
/// lock) — a green run proves the VSS arm was genuinely taken, then the
/// restored fixture proves the shadow's bytes were a faithful copy.
#[test]
#[ignore = "requires elevation + diskpart + VSS"]
fn busy_volume_escalates_to_vss_shadow() {
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

    let backup_path = backup_disk(source.disk_index()).expect(
        "run_backup failed with a handle held open — the volume can't be locked while we \
         hold it, so this almost always means the escalation to a VSS shadow failed \
         (check elevation / the VSS service / shadow storage)",
    );

    // Keep the handle alive until after the backup finishes, then release it.
    drop(held);

    // Diagnostic + guard: if FSCTL_GET_VOLUME_BITMAP failed on the shadow
    // handle, planning silently degrades to all-clusters-used and the backup
    // reads FREE space through the shadow — which volsnap leaves undefined
    // (free blocks aren't copy-on-write protected). used_bytes near the full
    // partition size is the tell.
    {
        let reader = PhnxReader::open(&backup_path).unwrap();
        for pm in &reader.manifest.partitions {
            eprintln!(
                "[vss] manifest: partition {} fs={} mode={} used={} of {} bytes",
                pm.index, pm.fs, pm.capture_mode, pm.used_bytes, pm.original_size
            );
            if pm.fs == "ntfs" {
                assert!(
                    pm.used_bytes < pm.original_size / 2,
                    "NTFS shadow capture used {} of {} bytes — the volume bitmap query \
                     likely failed on the shadow handle and degraded to all-clusters-used \
                     (capturing free space through a shadow is undefined behavior)",
                    pm.used_bytes,
                    pm.original_size
                );
            }
        }
    }

    // Restore to a fresh VHD and verify every fixture byte survived the
    // shadow-read path.
    restore_and_verify(&backup_path, &digest);
    eprintln!("[vss] end-to-end VSS backup roundtrip verified (lock was never taken)");

    let _ = std::fs::remove_file(&backup_path);
}

/// The third arm of the policy: neither freeze is available, so the backup
/// must ABORT. VSS can't snapshot FAT32, so a FAT32 volume with a handle held
/// open can be neither locked nor shadowed. If this backup ever *succeeds*,
/// the engine smeared a live read into an image the user would trust —
/// exactly the silent corruption this guards.
#[test]
#[ignore = "requires elevation + diskpart"]
fn busy_volume_without_vss_aborts_backup() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = make_vhd('X', "VSSFB1", TestFs::Fat32);
    let _ = fill_fixture('X', 0x6666).expect("fill fixture");

    let mut held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(r"X:\held-open.bin")
        .expect("open held file");
    held.write_all(b"held").expect("write held");

    let result = backup_disk(source.disk_index());
    match result {
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("lock"),
                "backup failed for an unexpected reason (wanted a volume-lock failure): {e}"
            );
            eprintln!("[vss] unfreezable volume correctly aborted the backup: {e}");
        }
        Ok(p) => {
            let _ = std::fs::remove_file(&p);
            panic!(
                "backup SUCCEEDED with VSS unavailable and a handle held open — the engine \
                 captured an unfrozen, mutating source instead of aborting"
            );
        }
    }
    drop(held);
}

/// OUTBOUND lock exclusivity, primitive level: while the engine's
/// `PartitionReader::lock_volume` holds `FSCTL_LOCK_VOLUME`, any other
/// handle's attempt to create/open files on the volume must be refused —
/// that refusal is exactly the consistency guarantee the no-VSS path relies
/// on. Deterministic: no racing against a live backup.
#[test]
#[ignore = "requires elevation + diskpart"]
fn volume_lock_blocks_external_writes() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let _vhd = make_ntfs_vhd('X', "LOCKX");
    std::fs::write(r"X:\pre-lock.txt", b"before").expect("pre-lock write");

    {
        // 512: a diskpart VHD fixture; this reader only holds a lock, never reads.
        let mut reader =
            phoenix_capture::PartitionReader::open_volume(r"\\.\X:", 512).expect("open volume");
        reader.lock_volume(r"\\.\X:").expect("lock_volume");
        assert!(reader.is_locked());

        // New file creation must be refused while the lock is held.
        let write_attempt = std::fs::write(r"X:\during-lock.txt", b"should not land");
        assert!(
            write_attempt.is_err(),
            "created a file on X: while the volume was locked — FSCTL_LOCK_VOLUME is not \
             providing exclusivity"
        );
        // Opening an existing file through a fresh handle must be refused too.
        let read_attempt = std::fs::read(r"X:\pre-lock.txt");
        assert!(
            read_attempt.is_err(),
            "opened a file on X: while the volume was locked"
        );
        eprintln!("[vss] lock held: external create + open both refused, as required");
        // Reader drops here → FSCTL_UNLOCK_VOLUME + handle close.
    }

    std::fs::write(r"X:\after-unlock.txt", b"after").expect("write after unlock");
    assert_eq!(
        std::fs::read(r"X:\pre-lock.txt").expect("read after unlock"),
        b"before"
    );
    eprintln!("[vss] lock released: volume usable again");
}

/// OUTBOUND lock exclusivity, end-to-end: an idle volume — which the engine
/// therefore locks rather than snapshots — is backed up on a worker thread
/// while this thread hammers write attempts at it. At least one attempt must
/// be refused while the backup window is open — proving the engine really
/// holds the lock for the capture's duration — and writes must succeed again
/// after it returns. (A `refused == 0` failure also catches the engine
/// wrongly escalating an idle volume to a shadow, which leaves it writable.)
#[test]
#[ignore = "requires elevation + diskpart"]
fn locked_backup_blocks_writers_for_duration() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = make_ntfs_vhd('X', "LOCKDUR");
    let digest = fill_fixture('X', 0x8888).expect("fill fixture");
    let disk_index = source.disk_index();

    // Let Defender / the indexer finish scanning the just-written fixture and
    // release their handles, so the backup's pre-flight lock isn't racing them
    // (they, not our writer, are what starved the lock across all 5 retries).
    wait_until_lockable(r"\\.\X:", 30_000);

    let backup_thread = std::thread::spawn(move || backup_disk(disk_index));

    // Give the engine's pre-flight lock a brief contention-free window to land
    // before we start hammering — the writer's own transient handles can
    // otherwise coincide with FSCTL_LOCK_VOLUME. The backup + verify-after
    // window is far longer than this head start, so probes still overlap the
    // locked period and `refused > 0` holds.
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Hammer the volume while the backup runs. The pre-flight lock is held
    // by now; every attempt must fail until run_backup returns (the lock is
    // held through verify-after).
    let mut refused = 0u32;
    let mut allowed = 0u32;
    while !backup_thread.is_finished() {
        match std::fs::write(r"X:\writer-probe.txt", b"probe") {
            Ok(_) => allowed += 1,
            Err(_) => refused += 1,
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let backup_path = backup_thread
        .join()
        .expect("backup thread panicked")
        .expect("locked backup failed");
    eprintln!(
        "[vss] during backup: {refused} write attempts refused, {allowed} allowed \
         (pre-lock window only)"
    );
    assert!(
        refused > 0,
        "no write attempt was ever refused during a locked backup — the engine did not \
         hold FSCTL_LOCK_VOLUME for the capture window"
    );

    // Lock released after completion; volume writable again.
    std::fs::write(r"X:\post-backup.txt", b"unlocked").expect("volume still locked after backup");

    // And the captured image is faithful despite the write attempts.
    restore_and_verify(&backup_path, &digest);
    let _ = std::fs::remove_file(&backup_path);
    eprintln!("[vss] locked backup held exclusivity for its duration and restored faithfully");
}

/// The preferred arm, on a volume that has no shadow to fall back to: FAT32,
/// no open handles. The engine must lock the live volume, produce a faithful
/// backup (verify-after re-reads under the same lock), release the lock on
/// completion (proven by writing to the volume afterward), and restore
/// byte-for-byte.
#[test]
#[ignore = "requires elevation + diskpart"]
fn idle_fat32_volume_locked_roundtrip() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = make_vhd('X', "VSSFB2", TestFs::Fat32);
    let digest = fill_fixture('X', 0x7777).expect("fill fixture");

    let backup_path = backup_disk(source.disk_index()).expect("live-volume lock backup failed");

    // The lock must be released once the backup completes.
    std::fs::write(r"X:\post-backup-write.txt", b"unlocked again")
        .expect("volume still locked after backup returned");

    restore_and_verify(&backup_path, &digest);
    eprintln!("[vss] idle-volume lock round-trip verified (lock taken + released)");

    let _ = std::fs::remove_file(&backup_path);
}
