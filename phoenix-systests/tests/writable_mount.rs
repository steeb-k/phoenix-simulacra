//! Tier-2 system test for the PRODUCTION writable-overlay mount, driven through
//! the same `ActiveMount` API the GUI uses — and on a **512e** source, the case
//! the feasibility spikes never covered (they used 4Kn, which already forces
//! VHDX). This is what proves `force_vhdx` makes a 512e backup a valid
//! differencing parent.
//!
//! It mirrors the GUI's "Enable Write Access" flow exactly:
//!   1. mount the backup READ-ONLY (`ActiveMount::open_selected`), noting letters,
//!   2. eject it,
//!   3. re-mount WRITABLE preserving those letters
//!      (`ActiveMount::open_writable_selected`),
//! then asserts the overlay is writable, existing data still reads through, the
//! preferred letter was kept, the backing `.phnx` still verifies, and the
//! ephemeral child is gone after unmount.
//!
//! Requires: elevated shell, WinFsp installed, `--features winfsp` (libclang):
//!   $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin"
//!   cargo test -p phoenix-systests --features winfsp --test writable_mount -- \
//!       --ignored --test-threads=1 --nocapture
#![cfg(feature = "winfsp")]

use std::io::{Read, Write};
use std::path::Path;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_mount::{cleanup_leaked_mounts, ActiveMount};
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture, wait_for_letter, PartSpec,
    TestFs, TestVhd,
};

fn child_files(scratch: &Path) -> Vec<String> {
    std::fs::read_dir(scratch)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            (n.starts_with("child-") && n.ends_with(".avhdx")).then_some(n)
        })
        .collect()
}

#[test]
#[ignore = "requires elevation + WinFsp + --features winfsp"]
fn enable_write_access_on_a_512e_backup() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // --- A 512e NTFS source with a fixture, backed up --------------------------
    let source = TestVhd::create(256).expect("create 512e source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "WRSRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    let digest = fill_fixture('X', 0x0FEE_3E7A).expect("fill fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    assert_eq!(disk.sector_size, 512, "source should be 512e for this test");
    let selection: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();

    let backup_path =
        std::env::temp_dir().join(format!("wr-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: selection.clone(),
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");
    drop(source);

    let scratch = std::env::temp_dir()
        .join("phoenix-systests")
        .join("writable-mounts");
    let _ = std::fs::create_dir_all(&scratch);

    // --- 1. Mount READ-ONLY, note its letters (the GUI's starting state) -------
    let ro = ActiveMount::open_selected(&backup_path, &scratch, Some(&selection))
        .expect("read-only mount");
    assert!(!ro.is_writable(), "open_selected must be read-only");
    let preferred: Vec<(u32, char)> = ro
        .volumes()
        .iter()
        .filter_map(|v| v.drive_letter.map(|l| (v.partition_index, l)))
        .collect();
    assert!(
        !preferred.is_empty(),
        "the read-only mount exposed no drive letter to preserve"
    );
    let ro_letter = preferred[0].1;
    eprintln!("read-only mount exposed {ro_letter}: — converting to writable");

    // Prove the read-only drive really is read-only: a write must fail.
    let ro_probe = format!("{ro_letter}:\\should-fail.txt");
    assert!(
        std::fs::File::create(&ro_probe).is_err(),
        "the read-only mount accepted a write — it is not actually read-only"
    );

    // --- 2. Eject read-only, --- 3. re-mount WRITABLE preserving letters -------
    drop(ro);
    let rw = ActiveMount::open_writable_selected(&backup_path, &scratch, &selection, &preferred)
        .expect("writable overlay mount");
    assert!(rw.is_writable(), "open_writable_selected must be writable");

    // The lettered volume is the NTFS partition (partition 0 is the letter-less
    // Microsoft Reserved partition).
    let letter = rw
        .volumes()
        .iter()
        .find_map(|v| v.drive_letter)
        .expect("writable mount exposed no drive letter");
    eprintln!("writable overlay mounted at {letter}:");
    assert_eq!(
        letter, ro_letter,
        "the writable remount did not preserve the read-only mount's drive letter"
    );

    // Exactly one ephemeral child exists while mounted.
    assert_eq!(
        child_files(&scratch).len(),
        1,
        "expected exactly one differencing child while the writable mount is up"
    );

    // --- Existing data reads through, and a new write persists -----------------
    verify_fixture(letter, &digest).expect("fixture must read back through the writable overlay");

    let proof_path = format!("{letter}:\\overlay-proof.bin");
    let proof: Vec<u8> = (0..96 * 1024).map(|i| (i * 5 % 256) as u8).collect();
    {
        let mut f =
            std::fs::File::create(&proof_path).expect("write must SUCCEED on the writable overlay");
        f.write_all(&proof).expect("write overlay file");
        f.flush().ok();
    }
    let mut back = Vec::new();
    std::fs::File::open(&proof_path)
        .expect("reopen overlay file")
        .read_to_end(&mut back)
        .expect("read overlay file");
    assert_eq!(back, proof, "overlay write did not read back intact");

    // --- Unmount: the ephemeral child is deleted, the .phnx is pristine --------
    drop(rw);
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(
        child_files(&scratch).is_empty(),
        "the ephemeral differencing child was not deleted on unmount: {:?}",
        child_files(&scratch)
    );

    let mut reader = PhnxReader::open(&backup_path).expect("reopen backup");
    reader
        .verify_all(false)
        .expect("backing .phnx must still pass a full verify after the writable overlay");
    eprintln!("backing .phnx passes full verify_all — writable overlay left it pristine");

    // Cleanup sweep must run without error and leave nothing of ours behind.
    cleanup_leaked_mounts(&scratch);

    let _ = std::fs::remove_file(&backup_path);
}
