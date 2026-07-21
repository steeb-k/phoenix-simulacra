//! Lifecycle + resume for the **qcow2** VM write path (the committed default).
//!
//! qcow2 performs **no VHD attach at all**: QEMU reads the WinFsp-served parent
//! VHDX directly and copies-on-write into a qcow2 file. That structurally
//! avoids the detach deadlock that makes the `.avhdx` path's teardown
//! unreliable (measured; see `docs/VIRTUALIZATION.md`), so this test is safe to
//! run — nothing here can wedge the storage stack.
//!
//! What it proves, guest-free (no QEMU VM, seconds not minutes):
//!   1. A qcow2 overlay can be created over the served parent and written to.
//!   2. Reads of *unmodified* regions resolve through the qcow2 to the served
//!      parent (the GPT signature comes back), i.e. the backing chain works.
//!   3. **Resume**: after stopping the serve helper and re-spawning it at the
//!      same deterministic path, the same qcow2 still reads back both the
//!      marker (its own writes) and the GPT (through the re-served backing).
//!
//! Requires: elevated shell (the fixture creates a VHD), QEMU, and a
//! winfsp-built `simulacra-cli` to act as the serve helper:
//!   $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin"
//!   cargo build -p phoenix-cli --features winfsp
//!   cargo test -p phoenix-systests --test vm_overlay_lifecycle -- \
//!       --ignored --test-threads=1 --nocapture

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, wait_for_letter, PartSpec, TestFs, TestVhd,
};
use phoenix_vm::serve_helper::spawn_serve_with_exe;

const MIB: u64 = 1024 * 1024;

/// Force-exit if anything hangs, so a regression can't tie up the machine.
fn arm_watchdog(secs: u64, label: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        eprintln!("WATCHDOG: {label} did not finish in {secs}s — force-exiting");
        std::process::exit(2);
    });
}

fn qemu_tool(name: &str) -> PathBuf {
    let dir = std::env::var("PHOENIX_QEMU_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Program Files\qemu"));
    let tool = dir.join(name);
    assert!(
        tool.exists(),
        "{} not found (set PHOENIX_QEMU_DIR)",
        tool.display()
    );
    tool
}

fn run_ok(cmd: &mut Command, what: &str) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{what}: spawn failed: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{what} failed ({:?})\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// The built `simulacra-cli.exe` next to this test binary's target dir.
fn cli_exe() -> PathBuf {
    let me = std::env::current_exe().expect("current exe");
    let profile_dir = me.parent().and_then(|p| p.parent()).expect("profile dir");
    let cli = profile_dir.join("simulacra-cli.exe");
    assert!(
        cli.exists(),
        "{} not found — build it: cargo build -p phoenix-cli --features winfsp",
        cli.display()
    );
    cli
}

/// Dump `count_mib` MiB from the qcow2 and return the bytes.
fn dd_from_qcow2(qemu_img: &PathBuf, overlay: &PathBuf, out: &PathBuf, count_mib: u64) -> Vec<u8> {
    let _ = std::fs::remove_file(out);
    run_ok(
        Command::new(qemu_img)
            .arg("dd")
            .args(["-f", "qcow2", "-O", "raw"])
            .arg(format!("if={}", overlay.display()))
            .arg(format!("of={}", out.display()))
            .args(["bs=1M", &format!("count={count_mib}")]),
        "qemu-img dd from qcow2",
    );
    std::fs::read(out).expect("read dd output")
}

#[test]
#[ignore = "requires elevation + QEMU + a winfsp-built simulacra-cli"]
fn qcow2_overlay_lifecycle_and_resume() {
    require_admin();
    arm_watchdog(180, "qcow2 lifecycle");
    let _ = cleanup_leaked_vhds();
    let cli = cli_exe();
    let qemu_img = qemu_tool("qemu-img.exe");
    let qemu_io = qemu_tool("qemu-io.exe");

    // --- a small 512e NTFS backup -------------------------------------------
    let source = TestVhd::create(512).expect("create source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "VMQCOW".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    fill_fixture('X', 0x00B2_FE00u64).ok();
    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    let backup =
        std::env::temp_dir().join(format!("vmqcow-{}.phnx", uuid::Uuid::new_v4().simple()));
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

    let scratch = std::env::temp_dir()
        .join("phoenix-systests")
        .join("vmqcow-serve");
    let session_dir = std::env::temp_dir()
        .join("phoenix-systests")
        .join("vmqcow-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let overlay = session_dir.join("session.qcow2");
    let _ = std::fs::remove_file(&overlay);
    let dump = session_dir.join("head.bin");

    let marker_off = 40 * MIB;

    // ===================== leg 1: first boot equivalent =====================
    {
        let serve = spawn_serve_with_exe(&cli, &backup, &scratch).expect("spawn serve helper");
        let parent = serve.parent_path().to_path_buf();
        eprintln!("leg1: helper serving {}", parent.display());

        run_ok(
            Command::new(&qemu_img)
                .args(["create", "-f", "qcow2", "-F", "vhdx", "-b"])
                .arg(&parent)
                .arg(&overlay),
            "qemu-img create qcow2 overlay",
        );

        // Unmodified region reads THROUGH the qcow2 to the served parent.
        let head = dd_from_qcow2(&qemu_img, &overlay, &dump, 1);
        assert_eq!(
            &head[512..520],
            b"EFI PART",
            "leg1: GPT not readable through the backing chain"
        );

        // Guest-style write lands in the qcow2 only.
        run_ok(
            Command::new(&qemu_io)
                .args([
                    "-f",
                    "qcow2",
                    "-c",
                    &format!("write -P 0xab {marker_off} 4M"),
                ])
                .arg(&overlay),
            "qemu-io write marker",
        );
        eprintln!("leg1: wrote marker, GPT readable through backing");

        serve.stop();
        eprintln!("leg1: helper stopped cleanly");
    }

    let overlay_len = std::fs::metadata(&overlay).unwrap().len();
    assert!(
        overlay_len > 4 * MIB,
        "overlay didn't absorb the write ({overlay_len} bytes)"
    );

    // ===================== leg 2: resume equivalent =========================
    // Re-serve at the SAME deterministic path; the qcow2's recorded backing file
    // must resolve again, and its own writes must still be there.
    {
        let serve = spawn_serve_with_exe(&cli, &backup, &scratch).expect("re-spawn serve helper");
        eprintln!("leg2: helper re-serving {}", serve.parent_path().display());

        // (a) the qcow2's own writes survived
        let readback = run_ok(
            Command::new(&qemu_io)
                .args([
                    "-f",
                    "qcow2",
                    "-c",
                    &format!("read -P 0xab {marker_off} 4M"),
                ])
                .arg(&overlay),
            "qemu-io read marker after resume",
        );
        assert!(
            !readback.contains("Pattern verification failed"),
            "RESUME FAILED: marker lost across serve restart: {readback}"
        );

        // (b) unmodified regions still resolve through the RE-SERVED parent
        let head = dd_from_qcow2(&qemu_img, &overlay, &dump, 1);
        assert_eq!(
            &head[512..520],
            b"EFI PART",
            "RESUME FAILED: backing chain did not reconnect to the re-served parent"
        );

        serve.stop();
        eprintln!("leg2: marker + backing chain verified after resume");
    }

    PhnxReader::open(&backup).expect(".phnx must still open after the lifecycle");
    eprintln!("PASS: qcow2 lifecycle + resume verified (no attach, no hang)");

    let _ = std::fs::remove_file(&overlay);
    let _ = std::fs::remove_file(&dump);
    let _ = std::fs::remove_file(&backup);
}
