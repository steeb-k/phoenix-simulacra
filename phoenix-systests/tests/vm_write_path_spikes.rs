//! VM write-path spikes (virtualization branch, milestone 2 — see
//! `docs/VIRTUALIZATION.md`).
//!
//! Both spikes answer the same question from opposite ends: **how should guest
//! writes work when QEMU boots a `.phnx`?**
//!
//! * **Spike A — raw passthrough over the existing `.avhdx` overlay.** The
//!   served parent + differencing child from the writable mount, but the child
//!   is attached with NO drive letters and the disk forced OFFLINE, then all
//!   I/O goes through `\\.\PhysicalDriveN` exactly the way QEMU's raw driver
//!   would. Proves: the host never surfaces the guest's volumes, raw reads
//!   flow child→WinFsp→chunkstore, raw writes land in the child, and QEMU's
//!   own block layer (`qemu-img dd`) reads the device byte-identically.
//!
//! * **Spike B — qcow2 overlay over the WinFsp-served VHDX.** No disk attach
//!   at all: the served parent VHDX is the read-only backing file of a qcow2,
//!   and QEMU's own tools do the CoW. Proves: QEMU's `vhdx` driver parses our
//!   synthesized VHDX, reads are byte-correct through the qcow2, writes land
//!   in the qcow2 only, and the base stays pristine.
//!
//! Either way the `.phnx` must still pass a full `verify_all` afterwards.
//!
//! Requires: elevated shell, WinFsp installed, QEMU installed (default
//! `C:\Program Files\qemu`, override with `PHOENIX_QEMU_DIR`), and
//! `--features winfsp` (needs libclang):
//!   $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin"
//!   cargo test -p phoenix-systests --features winfsp --test vm_write_path_spikes -- \
//!       --ignored --test-threads=1 --nocapture
#![cfg(feature = "winfsp")]

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_mount::WinFspServe;
use phoenix_systests::{
    cleanup_leaked_vhds, create_differencing_vhdx, fill_fixture, require_admin, set_disk_offline,
    wait_for_letter, AttachedRw, PartSpec, TestFs, TestVhd,
};

const MIB: u64 = 1024 * 1024;

/// QEMU install dir: `PHOENIX_QEMU_DIR`, else the stock installer location.
fn qemu_tool(name: &str) -> PathBuf {
    let dir = std::env::var("PHOENIX_QEMU_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Program Files\qemu"));
    let tool = dir.join(name);
    assert!(
        tool.exists(),
        "{} not found — install QEMU or set PHOENIX_QEMU_DIR",
        tool.display()
    );
    tool
}

/// Run a command, panic with full output on failure, return stdout.
fn run_ok(cmd: &mut Command, what: &str) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("{what}: spawn failed: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{what} failed ({:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status
    );
    stdout
}

fn align_down(x: u64, align: u64) -> u64 {
    x / align * align
}

/// Drive letters D..Z whose root holds a `phoenix-fixture` directory.
fn fixture_letters() -> Vec<char> {
    (b'D'..=b'Z')
        .map(|c| c as char)
        .filter(|l| Path::new(&format!("{l}:\\phoenix-fixture")).exists())
        .collect()
}

/// A 512e NTFS source with fixture data, backed up to a `.phnx`; the source
/// VHD is detached before returning so nothing can be confused with it.
fn make_backup_fixture(tag: &str, seed: u64) -> PathBuf {
    let source = TestVhd::create(512).expect("create 512e source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: format!("VMSPK{tag}"),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    fill_fixture('X', seed).expect("fill fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    assert_eq!(disk.sector_size, 512, "spike fixtures are 512e on purpose");
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();

    let backup = std::env::temp_dir().join(format!(
        "vmspk-{tag}-{}.phnx",
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
    backup
}

/// GPT + NTFS signature checks over the head bytes of the synthesized disk.
fn check_disk_head(head: &[u8], what: &str) {
    assert_eq!(
        &head[510..512],
        &[0x55, 0xAA],
        "{what}: protective MBR signature missing"
    );
    assert_eq!(
        &head[512..520],
        b"EFI PART",
        "{what}: primary GPT header signature missing at LBA 1"
    );
}

#[test]
#[ignore = "requires elevation + WinFsp + QEMU + --features winfsp"]
fn spike_a_raw_passthrough_over_offline_avhdx_child() {
    require_admin();
    let _ = cleanup_leaked_vhds();
    let qemu_img = qemu_tool("qemu-img.exe");

    let backup = make_backup_fixture("a", 0x00D1_5C05);

    // --- serve the parent, hang the .avhdx child off it (as the writable mount)
    let scratch = std::env::temp_dir().join("phoenix-systests").join("vmspk-a");
    let serve = WinFspServe::serve(&backup, &scratch, true).expect("WinFsp serve");
    let parent = serve.image_path();
    let vdisk_size = serve.disk_size();
    let span = serve
        .spans()
        .iter()
        .max_by_key(|s| s.size)
        .cloned()
        .expect("no partition spans");
    eprintln!(
        "served parent {} | data partition at {} MiB, {} MiB",
        parent.display(),
        span.disk_offset / MIB,
        span.size / MIB
    );

    let child = std::env::temp_dir().join(format!(
        "vmspk-a-child-{}.avhdx",
        uuid::Uuid::new_v4().simple()
    ));
    create_differencing_vhdx(&child, &parent).expect("differencing child over served parent");
    let child_len_fresh = std::fs::metadata(&child).unwrap().len();

    // --- attach with NO letters, force the disk offline --------------------
    let pre_letters = fixture_letters();
    let attached = AttachedRw::attach_no_letters(&child).expect("attach child RW, no letters");
    let drive = attached
        .physical_drive_number()
        .expect("physical drive number of attached child");
    set_disk_offline(drive).expect("force the attached child offline");
    eprintln!("child attached as PhysicalDrive{drive}, offline, no letters");

    // The host must never surface the guest's volumes.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let new_letters: Vec<char> = fixture_letters()
        .into_iter()
        .filter(|l| !pre_letters.contains(l))
        .collect();
    assert!(
        new_letters.is_empty(),
        "offline/no-letter attach still surfaced volumes: {new_letters:?}"
    );

    // --- raw reads through \\.\PhysicalDriveN (QEMU's exact access path) ----
    let raw_path = format!(r"\\.\PhysicalDrive{drive}");
    let mut raw = raw_open(&raw_path);

    let head = raw_read(&mut raw, 0, 8 * MIB as usize);
    check_disk_head(&head, "raw passthrough");

    let boot = raw_read(&mut raw, span.disk_offset, 4096);
    assert_eq!(&boot[3..11], b"NTFS    ", "raw NTFS boot sector missing");

    let tail = raw_read(&mut raw, vdisk_size - 4096, 4096);
    assert_eq!(
        &tail[4096 - 512..4096 - 504],
        b"EFI PART",
        "backup GPT header missing at the last LBA"
    );

    let t = Instant::now();
    let bulk = raw_read(&mut raw, span.disk_offset, 64 * MIB as usize);
    let secs = t.elapsed().as_secs_f64();
    assert!(bulk.iter().any(|&b| b != 0), "bulk read came back all zero");
    eprintln!(
        "raw bulk read: 64 MiB in {secs:.2}s ({:.0} MB/s)",
        64.0 / secs
    );

    // Release the device before QEMU touches it: qemu-img's restrictive share
    // mode conflicts with our open write handle (in production QEMU is the
    // only opener, so this is purely test sequencing).
    drop(raw);

    // --- QEMU's own block layer reads the same bytes ------------------------
    let dd_out = std::env::temp_dir().join(format!(
        "vmspk-a-dd-{}.bin",
        uuid::Uuid::new_v4().simple()
    ));
    run_ok(
        Command::new(&qemu_img)
            .arg("dd")
            .args(["-f", "raw", "-O", "raw"])
            .arg(format!("if={raw_path}"))
            .arg(format!("of={}", dd_out.display()))
            .args(["bs=1M", "count=8"]),
        "qemu-img dd from the raw physical drive",
    );
    let qemu_head = std::fs::read(&dd_out).expect("read qemu-img dd output");
    assert_eq!(
        qemu_head.len(),
        (8 * MIB) as usize,
        "qemu-img dd short read"
    );
    assert_eq!(
        qemu_head, head,
        "QEMU's raw driver read different bytes than the direct handle"
    );
    eprintln!("qemu-img read PhysicalDrive{drive}: 8 MiB byte-identical to the direct handle");

    // --- raw write lands in the child and persists --------------------------
    let mut raw = raw_open(&raw_path);
    let woff = align_down(span.disk_offset + span.size - 8 * MIB, MIB);
    let pattern: Vec<u8> = (0..MIB as usize).map(|i| (i * 31 + 7) as u8).collect();
    raw_write(&mut raw, woff, &pattern);
    raw.sync_all().expect("flush raw write");
    let echo = raw_read(&mut raw, woff, pattern.len());
    assert_eq!(echo, pattern, "raw write did not read back");
    drop(raw);

    // A fresh handle must see the same bytes (they live in the child now).
    let mut raw2 = raw_open(&raw_path);
    let echo2 = raw_read(&mut raw2, woff, pattern.len());
    assert_eq!(echo2, pattern, "raw write did not survive handle reopen");
    drop(raw2);
    eprintln!("raw write at {} MiB persisted through reopen", woff / MIB);

    // --- teardown: child grew, parent/.phnx pristine ------------------------
    drop(attached);
    std::thread::sleep(std::time::Duration::from_secs(1));
    let child_len_after = std::fs::metadata(&child).unwrap().len();
    assert!(
        child_len_after > child_len_fresh,
        "child did not grow ({child_len_fresh} -> {child_len_after})"
    );
    eprintln!("child grew {child_len_fresh} -> {child_len_after} bytes");
    drop(serve);

    let mut reader = PhnxReader::open(&backup).expect("reopen backup");
    reader.verify_all(false).expect(".phnx must verify after spike A");
    eprintln!("SPIKE A PASS: offline raw passthrough works end-to-end, .phnx pristine");

    let _ = std::fs::remove_file(&dd_out);
    let _ = std::fs::remove_file(&child);
    let _ = std::fs::remove_file(&backup);
}

#[test]
#[ignore = "requires elevation + WinFsp + QEMU + --features winfsp"]
fn spike_b_qcow2_overlay_over_served_vhdx() {
    require_admin();
    let _ = cleanup_leaked_vhds();
    let qemu_img = qemu_tool("qemu-img.exe");
    let qemu_io = qemu_tool("qemu-io.exe");

    let backup = make_backup_fixture("b", 0x00D1_5C0B);

    // --- serve the parent; NOTHING is ever attached in this spike -----------
    let scratch = std::env::temp_dir().join("phoenix-systests").join("vmspk-b");
    let serve = WinFspServe::serve(&backup, &scratch, true).expect("WinFsp serve");
    let parent = serve.image_path();
    let vdisk_size = serve.disk_size();
    let span = serve
        .spans()
        .iter()
        .max_by_key(|s| s.size)
        .cloned()
        .expect("no partition spans");

    // --- the sharp unknown: QEMU's vhdx driver vs our synthesized VHDX ------
    let info = run_ok(
        Command::new(&qemu_img)
            .args(["info", "--output=json"])
            .arg(&parent),
        "qemu-img info on the served VHDX",
    );
    let flat: String = info.split_whitespace().collect();
    assert!(
        flat.contains(r#""format":"vhdx""#),
        "qemu-img did not recognize the served image as vhdx: {info}"
    );
    assert!(
        flat.contains(&format!(r#""virtual-size":{vdisk_size},"#))
            || flat.contains(&format!(r#""virtual-size":{vdisk_size}}}"#)),
        "qemu-img reports a different virtual size than the synthesized disk \
         ({vdisk_size}): {info}"
    );
    eprintln!(
        "qemu-img parsed the served VHDX: vhdx, virtual size {} MiB",
        vdisk_size / MIB
    );

    // --- qcow2 overlay backed by the served VHDX ----------------------------
    let overlay = std::env::temp_dir().join(format!(
        "vmspk-b-{}.qcow2",
        uuid::Uuid::new_v4().simple()
    ));
    run_ok(
        Command::new(&qemu_img)
            .args(["create", "-f", "qcow2", "-F", "vhdx", "-b"])
            .arg(&parent)
            .arg(&overlay),
        "qemu-img create qcow2 overlay",
    );
    let overlay_len_fresh = std::fs::metadata(&overlay).unwrap().len();

    // --- reads through the overlay are byte-correct -------------------------
    let dd = |args: &[String], what: &str| {
        run_ok(
            Command::new(&qemu_img)
                .arg("dd")
                .args(["-f", "qcow2", "-O", "raw"])
                .args(args),
            what,
        )
    };
    let head_bin = std::env::temp_dir().join("vmspk-b-head.bin");
    dd(
        &[
            format!("if={}", overlay.display()),
            format!("of={}", head_bin.display()),
            "bs=1M".into(),
            "count=8".into(),
        ],
        "qemu-img dd head through qcow2",
    );
    let head = std::fs::read(&head_bin).expect("read head dump");
    check_disk_head(&head, "qcow2 overlay");

    assert_eq!(span.disk_offset % MIB, 0, "span not MiB-aligned");
    let part_bin = std::env::temp_dir().join("vmspk-b-part.bin");
    let t = Instant::now();
    dd(
        &[
            format!("if={}", overlay.display()),
            format!("of={}", part_bin.display()),
            "bs=1M".into(),
            "count=64".into(),
            format!("skip={}", span.disk_offset / MIB),
        ],
        "qemu-img dd partition bulk through qcow2",
    );
    let secs = t.elapsed().as_secs_f64();
    let part = std::fs::read(&part_bin).expect("read partition dump");
    assert_eq!(&part[3..11], b"NTFS    ", "qcow2 NTFS boot sector missing");
    assert!(part.iter().any(|&b| b != 0), "bulk read came back all zero");
    eprintln!(
        "qcow2 bulk read: 64 MiB in {secs:.2}s ({:.0} MB/s)",
        64.0 / secs
    );

    // --- writes go to the qcow2 only, and persist across processes ----------
    let woff = align_down(span.disk_offset + span.size - 8 * MIB, MIB);
    run_ok(
        Command::new(&qemu_io)
            .args(["-f", "qcow2", "-c", &format!("write -P 0xcd {woff} 4M")])
            .arg(&overlay),
        "qemu-io write through the overlay",
    );
    let read_back = run_ok(
        Command::new(&qemu_io)
            .args(["-f", "qcow2", "-c", &format!("read -P 0xcd {woff} 4M")])
            .arg(&overlay),
        "qemu-io pattern read-back (fresh process)",
    );
    assert!(
        !read_back.contains("Pattern verification failed"),
        "overlay write did not persist: {read_back}"
    );
    let overlay_len_after = std::fs::metadata(&overlay).unwrap().len();
    assert!(
        overlay_len_after > overlay_len_fresh + 4 * MIB - 1,
        "overlay did not absorb the write ({overlay_len_fresh} -> {overlay_len_after})"
    );
    eprintln!(
        "qemu-io write persisted; overlay grew {overlay_len_fresh} -> {overlay_len_after} bytes"
    );

    // --- CoW isolation: base reads are unchanged after the write ------------
    let head_bin2 = std::env::temp_dir().join("vmspk-b-head2.bin");
    dd(
        &[
            format!("if={}", overlay.display()),
            format!("of={}", head_bin2.display()),
            "bs=1M".into(),
            "count=8".into(),
        ],
        "qemu-img dd head after the write",
    );
    assert_eq!(
        std::fs::read(&head_bin2).unwrap(),
        head,
        "unmodified region changed after an overlay write"
    );

    drop(serve);
    let mut reader = PhnxReader::open(&backup).expect("reopen backup");
    reader.verify_all(false).expect(".phnx must verify after spike B");
    eprintln!("SPIKE B PASS: QEMU vhdx backing + qcow2 CoW works end-to-end, .phnx pristine");

    for f in [&overlay, &head_bin, &part_bin, &head_bin2] {
        let _ = std::fs::remove_file(f);
    }
    let _ = std::fs::remove_file(&backup);
}

// --- raw device I/O helpers (sector-aligned, as QEMU's raw driver does) -----

fn raw_open(path: &str) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0x3) // FILE_SHARE_READ | FILE_SHARE_WRITE
        .open(path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"))
}

fn raw_read(f: &mut std::fs::File, offset: u64, len: usize) -> Vec<u8> {
    assert_eq!(offset % 512, 0, "raw reads must be sector-aligned");
    assert_eq!(len % 512, 0, "raw reads must be sector-multiples");
    f.seek(SeekFrom::Start(offset)).expect("seek raw device");
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)
        .unwrap_or_else(|e| panic!("raw read {len}B at {offset}: {e}"));
    buf
}

fn raw_write(f: &mut std::fs::File, offset: u64, data: &[u8]) {
    assert_eq!(offset % 512, 0, "raw writes must be sector-aligned");
    assert_eq!(data.len() % 512, 0, "raw writes must be sector-multiples");
    f.seek(SeekFrom::Start(offset)).expect("seek raw device");
    f.write_all(data)
        .unwrap_or_else(|e| panic!("raw write {}B at {offset}: {e}", data.len()));
}
