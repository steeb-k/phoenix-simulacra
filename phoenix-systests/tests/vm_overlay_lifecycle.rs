//! Guest-free validation of the VM overlay lifecycle and teardown ordering.
//!
//! ⚠️ CURRENT RESULT (2026-07-19): this test **hangs at `detach_wait`** and its
//! watchdog force-exits it, because it proves a *negative*: with the WinFsp
//! serve running IN THIS PROCESS, `DetachVirtualDisk` on the differencing child
//! deadlocks in the storage driver (the detach's Close IRP to the WinFsp-served
//! parent can't complete). Confirmed via the per-step diag file: every raw I/O
//! step passes; the last marker is always `[5] detach_wait…`. The force-exit can
//! leave the overlay orphaned-attached briefly (Windows reaps it, or detach via
//! diskpart). This is the evidence for moving the serve OUT OF PROCESS; once
//! that lands, this test is rewritten to spawn the serve helper and expect a
//! clean detach. Until then, do not treat a hang here as a regression.
//!
//! This exercises exactly the serve → differencing-child → attach-offline →
//! raw-I/O → **detach-wait** → stop-serve sequence that `phoenix-vm`'s boot
//! path uses, but with **no QEMU and no multi-minute guest boot** — so it runs
//! in seconds and, crucially, cannot wedge the machine the way driving a real
//! guest and force-killing it did.
//!
//! Two things it proves:
//!   1. **Teardown is deadlock-free and ordered.** `AttachedDisk::detach_wait`
//!      confirms the device is gone before the in-process WinFsp serve is
//!      stopped; the serve is only dropped on that confirmation. A watchdog
//!      thread force-exits the process if any step hangs, so even a regression
//!      cleans up (process death auto-detaches the disk) instead of wedging.
//!   2. **Resume reconnects and persists.** A marker written to the raw device
//!      in the first attach is read back after detaching, re-serving at the
//!      same deterministic path, and re-attaching the same differencing child —
//!      the core of session resume.
//!
//! Requires: elevated shell, WinFsp, `--features winfsp`:
//!   $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin"
//!   cargo test -p phoenix-systests --features winfsp --test vm_overlay_lifecycle -- \
//!       --ignored --test-threads=1 --nocapture
#![cfg(feature = "winfsp")]

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_mount::{create_differencing_vhdx, set_disk_offline, AttachedDisk, WinFspServe};
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, wait_for_letter, PartSpec, TestFs, TestVhd,
};

const MIB: u64 = 1024 * 1024;

/// Append a progress marker to a diag file, flushed + synced, so the last line
/// survives a watchdog force-exit and pinpoints exactly where a hang occurred.
fn mark(step: &str) {
    let path = std::env::temp_dir().join("vmlife-progress.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{step}");
        let _ = f.flush();
        let _ = f.sync_all();
    }
    eprintln!("{step}");
}

/// Force-exit the whole process after `secs` so a hung teardown can never wedge
/// the box: process death makes the OS auto-detach the disk and reclaim WinFsp.
fn arm_watchdog(secs: u64, label: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        eprintln!("WATCHDOG: {label} did not finish in {secs}s — teardown likely hung; \
                   force-exiting so the OS reclaims the attach/serve");
        std::process::exit(2);
    });
}

fn raw_open(drive: u32) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    let path = format!(r"\\.\PhysicalDrive{drive}");
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0x3) // FILE_SHARE_READ | FILE_SHARE_WRITE
        .open(&path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"))
}

fn raw_write(f: &mut std::fs::File, offset: u64, data: &[u8]) {
    assert_eq!(offset % 512, 0);
    assert_eq!(data.len() % 512, 0);
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(data).unwrap();
    f.sync_all().ok();
}

fn raw_read(f: &mut std::fs::File, offset: u64, len: usize) -> Vec<u8> {
    assert_eq!(offset % 512, 0);
    assert_eq!(len % 512, 0);
    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).unwrap();
    buf
}

#[test]
#[ignore = "requires elevation + WinFsp + --features winfsp"]
fn overlay_teardown_is_ordered_and_resume_reconnects() {
    require_admin();
    arm_watchdog(40, "overlay lifecycle");
    let _ = cleanup_leaked_vhds();

    // --- a small 512e NTFS backup -------------------------------------------
    let source = TestVhd::create(512).expect("create source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "VMLIFE".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    fill_fixture('X', 0x00A1_FE00u64).ok(); // seed not important here
    let disks = enumerate_disks().unwrap();
    let disk = disks.iter().find(|d| d.index == source.disk_index()).unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    let backup = std::env::temp_dir().join(format!(
        "vmlife-{}.phnx",
        uuid::Uuid::new_v4().simple()
    ));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");
    drop(source);

    // Deterministic serve scratch + session overlay, both on the temp drive.
    let scratch = std::env::temp_dir().join("phoenix-systests").join("vmlife-serve");
    let session_dir = std::env::temp_dir().join("phoenix-systests").join("vmlife-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let overlay = session_dir.join("session.avhdx");
    let _ = std::fs::remove_file(&overlay);

    let marker: Vec<u8> = (0..512u32).map(|i| (i * 7 + 3) as u8).collect();
    let write_off: u64;

    // ===================== leg 1: first boot equivalent =====================
    {
        let serve = WinFspServe::serve_for_vm(&backup, &scratch).expect("serve_for_vm");
        let parent = serve.image_path();
        let span = serve
            .spans()
            .iter()
            .max_by_key(|s| s.size)
            .cloned()
            .expect("no partition span");
        write_off = (span.disk_offset + 16 * MIB) / 512 * 512;

        mark("leg1: [0a] create_differencing_vhdx…");
        create_differencing_vhdx(
            overlay.to_str().unwrap(),
            parent.to_str().unwrap(),
        )
        .expect("create differencing child");
        mark("leg1: [0b] attach_readwrite_opts…");
        let att = AttachedDisk::attach_readwrite_opts(overlay.to_str().unwrap(), false)
            .expect("attach child rw");
        mark("leg1: [0c] physical_drive_number…");
        let drive = att.physical_drive_number().expect("phys drive");
        mark("leg1: [0d] set_disk_offline…");
        set_disk_offline(drive).expect("offline");
        mark(&format!("leg1: attached as PhysicalDrive{drive} (offline)"));

        // Simulate guest write: a marker into the differencing child.
        mark("leg1: [1] raw_open…");
        let mut dev = raw_open(drive);
        mark("leg1: [2] raw_write…");
        raw_write(&mut dev, write_off, &marker);
        mark("leg1: [3] raw_read…");
        let echo = raw_read(&mut dev, write_off, marker.len());
        assert_eq!(echo, marker, "marker did not read back within leg1");
        mark("leg1: [4] drop dev…");
        drop(dev);

        // Teardown UNDER TEST: detach must complete before the serve stops.
        mark("leg1: [5] detach_wait…");
        let detached = att.detach_wait(Duration::from_secs(10));
        mark(&format!("leg1: [6] detach_wait returned {detached}"));
        assert!(detached, "detach_wait timed out — teardown ordering is unsafe");
        drop(att);
        mark("leg1: [7] drop serve…");
        drop(serve); // safe only because `detached` is true
        mark("leg1: detached cleanly, serve stopped — no hang");
    }

    let overlay_len = std::fs::metadata(&overlay).unwrap().len();
    assert!(overlay_len > 4 * MIB, "overlay didn't grow ({overlay_len} bytes)");

    // ===================== leg 2: resume equivalent =========================
    // Re-serve at the SAME deterministic path, re-attach the SAME child, and
    // read the marker back — proving the differencing child reconnected to the
    // re-served parent and guest writes persisted.
    {
        let serve = WinFspServe::serve_for_vm(&backup, &scratch).expect("re-serve_for_vm");
        assert!(overlay.exists(), "overlay vanished before resume");
        let att = AttachedDisk::attach_readwrite_opts(overlay.to_str().unwrap(), false)
            .expect("re-attach existing child (resume)");
        let drive = att.physical_drive_number().expect("phys drive on resume");
        set_disk_offline(drive).ok();
        eprintln!("leg2: re-attached existing overlay as PhysicalDrive{drive}");

        let mut dev = raw_open(drive);
        let echo = raw_read(&mut dev, write_off, marker.len());
        assert_eq!(echo, marker, "RESUME FAILED: marker did not survive detach + re-serve");
        drop(dev);

        let detached = att.detach_wait(Duration::from_secs(10));
        assert!(detached, "detach_wait timed out on resume leg");
        drop(att);
        drop(serve);
        eprintln!("leg2: resume marker verified, detached cleanly");
    }

    // Backup stayed immutable and openable.
    PhnxReader::open(&backup).expect(".phnx must still open after the lifecycle");
    eprintln!("PASS: ordered teardown (no hang) + resume reconnect verified");

    let _ = std::fs::remove_file(&overlay);
    let _ = std::fs::remove_file(&backup);
}
