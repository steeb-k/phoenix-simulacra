//! Feasibility spike 2 (Phase 1, approach A): a **zero-footprint** writable
//! overlay — the differencing parent is served **on demand by WinFsp** straight
//! from the compressed `.phnx`, never materialized to disk.
//!
//! Spike 1 (`rw_overlay_spike.rs`) proved the differencing mechanism against a
//! parent VHDX written out as a real ~520 MiB file. That doubles the footprint,
//! which is the very thing the mount design forbids. This spike closes that gap:
//! the parent's bytes exist nowhere on disk — each read Windows makes of the
//! parent is answered on the fly by `WinFspServe` → `SyntheticVhd` → the chunk
//! store (decompress + BLAKE3-verify). Only the differencing child, holding the
//! writes, occupies disk.
//!
//! What this proves on top of spike 1:
//!
//!   1. `CreateVirtualDisk` accepts a **WinFsp-served file** as a differencing
//!      parent (it reads the parent's VHDX metadata through the user-mode FS).
//!   2. Attaching the child read-write and mounting NTFS drives real
//!      copy-on-write reads back through WinFsp for every un-modified block —
//!      the actual production read path, hash-verified per chunk.
//!   3. The backing `.phnx` still passes a full `verify_all`, and the parent is
//!      never attached as its own disk (only the writable child is).
//!
//! The parent is served but NOT attached (see `WinFspServe`): attaching it too
//! would put a second disk online with the same GPT GUIDs as the child, which
//! Windows keeps offline.
//!
//! A 4Kn source is used only because our container chooses VHDX for 4Kn; the
//! real feature would force VHDX for 512e too.
//!
//! Requires: elevated shell, WinFsp installed, `--features winfsp` (needs
//! libclang):
//!   $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin"
//!   cargo test -p phoenix-systests --features winfsp --test rw_overlay_winfsp_spike -- \
//!       --ignored --test-threads=1 --nocapture
#![cfg(feature = "winfsp")]

use std::io::{Read, Write};
use std::path::Path;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_mount::WinFspServe;
use phoenix_systests::{
    cleanup_leaked_vhds, create_differencing_vhdx, fill_fixture, require_admin, verify_fixture,
    wait_for_letter, AttachedRw, PartSpec, TestFs, TestVhd,
};

/// Drive letters D..Z whose root holds a `phoenix-fixture` directory.
fn fixture_letters() -> Vec<char> {
    (b'D'..=b'Z')
        .map(|c| c as char)
        .filter(|l| Path::new(&format!("{l}:\\phoenix-fixture")).exists())
        .collect()
}

#[test]
#[ignore = "requires elevation + WinFsp + --features winfsp"]
fn differencing_overlay_over_winfsp_served_parent() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // --- 1. A 4Kn NTFS source with a fixture, backed up to a .phnx ------------
    let source = TestVhd::create_4kn(512).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "OVLSRC2".into(),
        }])
        .expect("init 4Kn source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    let digest = fill_fixture('X', 0x0FEE_2CD7).expect("fill fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    assert_eq!(disk.sector_size, 4096, "source fixture is not 4Kn");
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();

    let backup_path =
        std::env::temp_dir().join(format!("ovl2-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");

    // Detach the source so its identical volume can't be mistaken for the mount.
    drop(source);

    // --- 2. Serve the parent VHDX on demand via WinFsp (NOT attached) ---------
    let scratch = std::env::temp_dir()
        .join("phoenix-systests")
        .join("ovl-serve");
    let serve =
        WinFspServe::serve(&backup_path, &scratch, true).expect("WinFsp serve (no attach)");
    let parent_path = serve.image_path();
    eprintln!(
        "WinFsp-served parent (never materialized): {}",
        parent_path.display()
    );
    assert_eq!(
        parent_path.extension().and_then(|e| e.to_str()),
        Some("vhdx"),
        "the served parent must be a VHDX; a fixed VHD cannot be a differencing parent"
    );

    // --- 3. Differencing child over the WINFSP-SERVED parent ------------------
    // The sharp new claim: CreateVirtualDisk reads the parent's VHDX metadata
    // straight through the user-mode filesystem and accepts it.
    let child_vhdx =
        std::env::temp_dir().join(format!("ovl2-child-{}.avhdx", uuid::Uuid::new_v4().simple()));
    create_differencing_vhdx(&child_vhdx, &parent_path)
        .expect("Windows must accept the WinFsp-served VHDX as a differencing parent");
    let child_len_fresh = std::fs::metadata(&child_vhdx).unwrap().len();
    eprintln!("differencing child over served parent: {child_len_fresh} bytes fresh");

    // --- 4. Attach the child READ-WRITE; its CoW reads flow back through WinFsp
    let pre_existing = fixture_letters();
    let attached = AttachedRw::attach(&child_vhdx).expect("read-write attach of the diff child");
    if let Some(n) = attached.physical_drive_number() {
        eprintln!("child attached as PhysicalDrive{n} (read-write, parent served on demand)");
    }

    let mut mounted = None;
    for _ in 0..40 {
        if let Some(l) = fixture_letters()
            .into_iter()
            .find(|l| !pre_existing.contains(l))
        {
            mounted = Some(l);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let letter = mounted.expect(
        "the differencing child attached but no writable NTFS volume came online — \
         Windows may have left the disk offline, or reads through WinFsp failed",
    );
    eprintln!("zero-footprint writable overlay mounted at {letter}:");

    // --- 5a. Existing data reads correctly through WinFsp + the diff ----------
    verify_fixture(letter, &digest)
        .expect("fixture must read back through the WinFsp-served parent + differencing child");

    // --- 5b. A brand-new write persists in the child --------------------------
    let proof_path = format!("{letter}:\\overlay-proof.bin");
    let proof_bytes: Vec<u8> = (0..128 * 1024).map(|i| (i * 7 % 256) as u8).collect();
    {
        let mut f = std::fs::File::create(&proof_path).expect("create file on writable overlay");
        f.write_all(&proof_bytes).expect("write to overlay");
        f.flush().ok();
    }
    let mut read_back = Vec::new();
    std::fs::File::open(&proof_path)
        .expect("reopen overlay file")
        .read_to_end(&mut read_back)
        .expect("read overlay file");
    assert_eq!(read_back, proof_bytes, "overlay write did not read back intact");
    eprintln!("wrote and read back {} bytes on the overlay", proof_bytes.len());

    // --- 6. Detach the child, then tear down the served parent ----------------
    drop(attached);
    std::thread::sleep(std::time::Duration::from_secs(1));
    let child_len_after = std::fs::metadata(&child_vhdx).unwrap().len();
    assert!(
        child_len_after > child_len_fresh,
        "differencing child did not grow ({child_len_fresh} -> {child_len_after}); \
         writes did not land in the child"
    );
    eprintln!("child grew {child_len_fresh} -> {child_len_after} bytes (only the diff hit disk)");
    drop(serve);

    // --- 7. The backing .phnx is pristine -------------------------------------
    let mut reader = PhnxReader::open(&backup_path).expect("reopen backup");
    reader
        .verify_all(false)
        .expect("backing .phnx must still pass a full verify after the writable overlay");
    eprintln!("backing .phnx passes full verify_all — served overlay left it pristine");

    // --- cleanup -------------------------------------------------------------
    let _ = std::fs::remove_file(&child_vhdx);
    let _ = std::fs::remove_file(&backup_path);
}
