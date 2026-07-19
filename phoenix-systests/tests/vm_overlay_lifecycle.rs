//! Validates the VM overlay lifecycle with an **out-of-process** WinFsp serve.
//!
//! Background: `DetachVirtualDisk` on a differencing child deadlocks when the
//! parent VHDX is served by WinFsp *in the same process* (root-caused earlier).
//! The fix serves the parent from a separate helper process. This test proves
//! the fix, guest-free (no QEMU), in seconds:
//!
//!   1. **Teardown no longer deadlocks.** With the serve out-of-process,
//!      `detach_wait` returns `true` (the device really goes away) and the
//!      helper stops cleanly. A watchdog force-exits on any hang so a
//!      regression can't wedge the machine.
//!   2. **Resume reconnects and persists.** A marker written to the raw device
//!      in the first attach is read back after detaching, re-spawning the serve
//!      helper at the same deterministic path, and re-attaching the same child.
//!
//! The serve helper is the built `simulacra-cli` re-exec'd with the sentinel;
//! this test spawns it by path (a test binary doesn't route the sentinel). So
//! `simulacra-cli` must be built **with `--features winfsp`** first:
//!   $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin"
//!   cargo build -p phoenix-cli --features winfsp
//!   cargo test -p phoenix-systests --test vm_overlay_lifecycle -- \
//!       --ignored --test-threads=1 --nocapture
//!
//! (This test itself needs no winfsp feature — it only attaches/detaches and
//! drives the helper process.)

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_mount::{create_differencing_vhdx, set_disk_offline, wait_for_device_gone, AttachedDisk};
use phoenix_vm::serve_helper::spawn_serve_with_exe;
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, wait_for_letter, PartSpec, TestFs, TestVhd,
};

const MIB: u64 = 1024 * 1024;

/// Force-exit the whole process after `secs` so a hung teardown can never wedge
/// the box: process death makes the OS auto-detach the disk and reclaim WinFsp.
fn arm_watchdog(secs: u64, label: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        eprintln!("WATCHDOG: {label} did not finish in {secs}s — force-exiting");
        std::process::exit(2);
    });
}

/// The built `simulacra-cli.exe` next to this test binary's target dir.
fn cli_exe() -> PathBuf {
    // test exe: target/<profile>/deps/vm_overlay_lifecycle-HASH.exe
    // cli exe:  target/<profile>/simulacra-cli.exe
    let me = std::env::current_exe().expect("current exe");
    let profile_dir = me.parent().and_then(|p| p.parent()).expect("profile dir");
    let cli = profile_dir.join("simulacra-cli.exe");
    assert!(
        cli.exists(),
        "{} not found — build it first: cargo build -p phoenix-cli --features winfsp",
        cli.display()
    );
    cli
}

fn raw_open(drive: u32) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    let path = format!(r"\\.\PhysicalDrive{drive}");
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0x3)
        .open(&path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"))
}

fn raw_write(f: &mut std::fs::File, offset: u64, data: &[u8]) {
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(data).unwrap();
    f.sync_all().ok();
}

fn raw_read(f: &mut std::fs::File, offset: u64, len: usize) -> Vec<u8> {
    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).unwrap();
    buf
}

#[test]
#[ignore = "requires elevation + a winfsp-built simulacra-cli"]
fn overlay_teardown_out_of_process_and_resume_reconnects() {
    require_admin();
    arm_watchdog(90, "overlay lifecycle");
    let _ = cleanup_leaked_vhds();
    let cli = cli_exe();

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
    fill_fixture('X', 0x00A1_FE00u64).ok();
    let disks = enumerate_disks().unwrap();
    let disk = disks.iter().find(|d| d.index == source.disk_index()).unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    let backup = std::env::temp_dir().join(format!("vmlife-{}.phnx", uuid::Uuid::new_v4().simple()));
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

    let scratch = std::env::temp_dir().join("phoenix-systests").join("vmlife-serve");
    let session_dir = std::env::temp_dir().join("phoenix-systests").join("vmlife-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let overlay = session_dir.join("session.avhdx");
    let _ = std::fs::remove_file(&overlay);

    let marker: Vec<u8> = (0..512u32).map(|i| (i * 7 + 3) as u8).collect();
    let write_off: u64;

    // ===================== leg 1: first boot equivalent =====================
    {
        let serve = spawn_serve_with_exe(&cli, &backup, &scratch).expect("spawn serve helper");
        let parent = serve.parent_path().to_path_buf();
        eprintln!("leg1: helper serving parent at {}", parent.display());

        // The served parent carries the partition layout in its GPT; place the
        // marker safely inside the (large) data partition.
        write_off = 40 * MIB;

        create_differencing_vhdx(overlay.to_str().unwrap(), parent.to_str().unwrap())
            .expect("create differencing child");
        let att = AttachedDisk::attach_readwrite_opts(overlay.to_str().unwrap(), false)
            .expect("attach child rw");
        let drive = att.physical_drive_number().expect("phys drive");
        set_disk_offline(drive).expect("offline");
        eprintln!("leg1: attached as PhysicalDrive{drive} (offline)");

        let mut dev = raw_open(drive);
        raw_write(&mut dev, write_off, &marker);
        let echo = raw_read(&mut dev, write_off, marker.len());
        assert_eq!(echo, marker, "marker did not read back in leg1");
        drop(dev);

        // THE FIX UNDER TEST: detach by DROPPING (CloseHandle → async detach),
        // never by an explicit DetachVirtualDisk (that deadlocks), then poll
        // until the device is really gone before stopping the serve.
        eprintln!("leg1: drop attach (async detach)…");
        drop(att);
        eprintln!("leg1: wait_for_device_gone…");
        let detached = wait_for_device_gone(drive, Duration::from_secs(15));
        eprintln!("leg1: wait_for_device_gone -> {detached}");
        assert!(detached, "device never went away after dropping the attach");
        serve.stop();
        eprintln!("leg1: helper stopped cleanly — no hang");
    }

    let overlay_len = std::fs::metadata(&overlay).unwrap().len();
    assert!(overlay_len > 4 * MIB, "overlay didn't grow ({overlay_len} bytes)");

    // ===================== leg 2: resume equivalent =========================
    {
        let serve = spawn_serve_with_exe(&cli, &backup, &scratch).expect("re-spawn serve helper");
        let parent = serve.parent_path().to_path_buf();
        assert!(overlay.exists(), "overlay vanished before resume");
        let att = AttachedDisk::attach_readwrite_opts(overlay.to_str().unwrap(), false)
            .expect("re-attach existing child (resume)");
        let drive = att.physical_drive_number().expect("phys drive on resume");
        set_disk_offline(drive).ok();
        eprintln!("leg2: re-attached overlay over re-served {}", parent.display());

        let mut dev = raw_open(drive);
        let echo = raw_read(&mut dev, write_off, marker.len());
        assert_eq!(echo, marker, "RESUME FAILED: marker did not survive detach + re-serve");
        drop(dev);

        drop(att);
        let detached = wait_for_device_gone(drive, Duration::from_secs(15));
        assert!(detached, "device never went away on resume leg");
        serve.stop();
        eprintln!("leg2: resume marker verified, helper stopped cleanly");
    }

    PhnxReader::open(&backup).expect(".phnx must still open after the lifecycle");
    eprintln!("PASS: out-of-process teardown (no hang) + resume reconnect verified");

    let _ = std::fs::remove_file(&overlay);
    let _ = std::fs::remove_file(&backup);
}
