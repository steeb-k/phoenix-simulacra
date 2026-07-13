//! Tier-2 system test for the ZERO-SPACE WinFsp mount: back up a filled NTFS
//! volume, mount the `.phnx` read-only through WinFsp (no materialization), and
//! confirm (a) the synthesized `backup.vhd` is served correctly through the
//! WinFsp filesystem and (b) `AttachVirtualDisk` accepts that WinFsp-served
//! file and the fixture is byte-identical through the attached disk.
//!
//! This is the load-bearing spike for the space-efficient mount: it proves that
//! attaching a virtual disk over a user-mode (WinFsp) filesystem works.
//!
//! Requires: elevated shell, WinFsp installed, and building with the `winfsp`
//! feature (needs libclang):
//!   $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin"
//!   cargo test -p phoenix-systests --features winfsp --test winfsp_mount -- \
//!       --ignored --test-threads=1 --nocapture
#![cfg(feature = "winfsp")]

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::disk::enumerate_disks;
use phoenix_mount::WinFspMount;
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture, wait_for_letter, PartSpec,
    TestFs, TestVhd,
};

/// Drive letters D..Z whose root holds a `phoenix-fixture` directory. See
/// `mount.rs`: snapshot before mounting, then look for a NEW letter — a
/// leftover fixture volume on another attached disk (e.g. the T3 real-disk
/// target left online after its restore) must not be mistaken for the mount.
fn fixture_letters() -> Vec<char> {
    (b'D'..=b'Z')
        .map(|c| c as char)
        .filter(|l| Path::new(&format!("{l}:\\phoenix-fixture")).exists())
        .collect()
}

#[test]
#[ignore = "requires elevation + WinFsp + --features winfsp"]
fn winfsp_mount_and_browse_files() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source: 256 MiB NTFS with a fixture.
    let source = TestVhd::create(256).expect("create source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SRC".into(),
        }])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "source never mounted");
    let digest = fill_fixture('X', 0x9A9A).expect("fill fixture");

    // Back it up.
    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    let backup_path =
        std::env::temp_dir().join(format!("winfsp-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");

    // Mount read-only through WinFsp (zero materialization). Snapshot
    // fixture-bearing volumes first so only a NEW letter counts as ours.
    let pre_existing = fixture_letters();
    let scratch = std::env::temp_dir()
        .join("phoenix-systests")
        .join("winfsp-mounts");
    let session = WinFspMount::mount(&backup_path, &scratch).expect("winfsp mount");

    // (a) Prove the FS read path: read backup.vhd straight through WinFsp and
    // check the synthesized GPT + VHD footer are served correctly.
    let vhd_path = session.vhd_path();
    eprintln!("winfsp-served vhd: {}", vhd_path.display());
    let mut f = std::fs::File::open(&vhd_path).expect("open backup.vhd via WinFsp");
    let mut sec0 = [0u8; 512];
    f.read_exact(&mut sec0).expect("read sector 0");
    assert_eq!(
        (sec0[510], sec0[511]),
        (0x55, 0xAA),
        "protective MBR signature not served through WinFsp"
    );
    // Footer cookie at the very end (disk_size .. +512).
    let mut cookie = [0u8; 8];
    f.seek(SeekFrom::Start(session.disk_size)).unwrap();
    f.read_exact(&mut cookie).expect("read footer");
    assert_eq!(&cookie, b"conectix", "VHD footer not served through WinFsp");
    drop(f);

    // (b) The disk is attached from that WinFsp-served file; find the mounted
    // fixture volume and verify every file byte-for-byte.
    let mut mounted = None;
    for _ in 0..30 {
        if let Some(l) = fixture_letters()
            .into_iter()
            .find(|l| !pre_existing.contains(l))
        {
            mounted = Some(l);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let letter = mounted.expect("mounted fixture volume never appeared");
    eprintln!("attached volume letter: {letter}");
    verify_fixture(letter, &digest).expect("fixture readable through the WinFsp-backed mount");

    drop(session); // detach + unmount + cleanup
    let _ = std::fs::remove_file(&backup_path);
}

/// Selective mount: back up a two-partition disk, mount with only the second
/// partition selected, and confirm exactly that partition gets a (new) drive
/// letter, its data verifies, and the letter is removed again on unmount.
#[test]
#[ignore = "requires elevation + WinFsp + --features winfsp"]
fn winfsp_mount_selected_partition_only() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Source: two NTFS partitions with distinct fixtures.
    let source = TestVhd::create(384).expect("create source");
    source
        .init_gpt_with(&[
            PartSpec {
                size_mb: 128,
                fs: TestFs::Ntfs,
                letter: 'X',
                label: "SRC1".into(),
            },
            PartSpec {
                size_mb: 0,
                fs: TestFs::Ntfs,
                letter: 'Y',
                label: "SRC2".into(),
            },
        ])
        .expect("init source");
    assert!(wait_for_letter('X', 15_000), "first source never mounted");
    assert!(wait_for_letter('Y', 15_000), "second source never mounted");
    let _digest1 = fill_fixture('X', 0x1111).expect("fill fixture 1");
    let digest2 = fill_fixture('Y', 0x2222).expect("fill fixture 2");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    let backup_path =
        std::env::temp_dir().join(format!("winfsp-sel-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");

    // Pick the second partition's index out of the backup by its label.
    let reader = phoenix_core::container::PhnxReader::open(&backup_path).expect("open backup");
    let second_idx = reader
        .index
        .iter()
        .find(|e| e.name.contains("SRC2"))
        .unwrap_or_else(|| {
            panic!(
                "no SRC2 entry in backup index (names: {:?})",
                reader.index.iter().map(|e| &e.name).collect::<Vec<_>>()
            )
        })
        .index;
    drop(reader);

    let pre_existing = fixture_letters();
    let scratch = std::env::temp_dir()
        .join("phoenix-systests")
        .join("winfsp-mounts");
    let session = WinFspMount::mount_selected(&backup_path, &scratch, Some(&[second_idx]))
        .expect("winfsp selective mount");

    // Exactly one exposure, with a letter, and it's the selected partition.
    assert_eq!(
        session.volumes.len(),
        1,
        "one selected partition, one entry"
    );
    assert_eq!(session.volumes[0].partition_index, second_idx);
    let letter = session.volumes[0]
        .drive_letter
        .expect("selected NTFS partition should get a drive letter");
    eprintln!("selected partition exposed as {letter}:");
    verify_fixture(letter, &digest2).expect("selected partition readable");

    // The unselected partition must NOT have surfaced: the only new
    // fixture-bearing letter is the one we were handed.
    let new_letters: Vec<char> = fixture_letters()
        .into_iter()
        .filter(|l| !pre_existing.contains(l))
        .collect();
    assert_eq!(
        new_letters,
        vec![letter],
        "unselected partition leaked a drive letter"
    );

    // Unmount removes the letter again.
    drop(session);
    let root = format!("{letter}:\\");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while Path::new(&root).exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert!(
        !Path::new(&root).exists(),
        "assigned letter still present after unmount"
    );
    let _ = std::fs::remove_file(&backup_path);
}
