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

    // Pick the second NTFS partition out of the backup.
    //
    // NOT by label: `PartitionIndexEntry::name` is the **GPT partition name**
    // ("Basic data partition"), which is a different thing from the volume label
    // diskpart's `format label=SRC2` sets — the manifest doesn't record volume
    // labels at all. Selecting on the label is what made the original version of
    // this test panic here on every run since the day it was written, so it never
    // once reached the mount below.
    //
    // The disk is MSR + SRC1 + SRC2, so the NTFS entries are exactly the two data
    // partitions in on-disk order. Assert that shape rather than assume it: if the
    // layout ever changes, this should fail loudly instead of quietly selecting
    // (and then "verifying") the wrong partition.
    let reader = phoenix_core::container::PhnxReader::open(&backup_path).expect("open backup");
    let ntfs: Vec<&phoenix_core::container::PartitionIndexEntry> = reader
        .index
        .iter()
        .filter(|e| e.fs_kind == phoenix_core::disk::FilesystemKind::Ntfs)
        .collect();
    assert_eq!(
        ntfs.len(),
        2,
        "expected exactly 2 NTFS partitions in the backup, got {:?}",
        reader
            .index
            .iter()
            .map(|e| (&e.name, e.fs_kind))
            .collect::<Vec<_>>()
    );
    let second = ntfs[1];
    // SRC2 is the "rest of disk" partition, so it must be the larger of the two —
    // a cheap check that we grabbed the one holding `digest2`.
    assert!(
        second.original_size > ntfs[0].original_size,
        "second NTFS partition should be the larger (rest-of-disk) one"
    );
    let second_idx = second.index;
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

/// Mount a backup taken from a **4Kn** disk.
///
/// This is what the VHDX container was built for. A fixed VHD cannot state a
/// sector size — 512 is the format, not a field — so a volume whose own boot
/// sector says `BytesPerSector = 4096` would be attached onto a 512-byte device
/// and NTFS would refuse it, leaving the user staring at a RAW disk. Mounting a
/// 4Kn backup used to be refused outright for exactly that reason.
///
/// Now the synthesized disk is a VHDX that says 4096, and Windows' own NTFS
/// driver does the rest. Green means:
///
///   * the container we hand `AttachVirtualDisk` is a VHDX Windows accepts,
///   * the attached disk really reports a 4096-byte logical sector,
///   * the GPT we synthesized at 4096-byte LBAs is one Windows can parse,
///   * and every fixture byte comes back through the whole stack.
///
/// No 4Kn hardware anywhere: the source disk is a synthesized 4Kn VHDX too.
#[test]
#[ignore = "requires elevation + WinFsp + --features winfsp"]
fn winfsp_mount_a_4kn_backup() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    let source = TestVhd::create_4kn(512).expect("create 4Kn source");
    source
        .init_gpt_with(&[PartSpec {
            size_mb: 0,
            fs: TestFs::Ntfs,
            letter: 'X',
            label: "SRC4KN".into(),
        }])
        .expect("init 4Kn source");
    assert!(wait_for_letter('X', 15_000), "4Kn source never mounted");
    let digest = fill_fixture('X', 0x4096_1234).expect("fill fixture");

    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    assert_eq!(disk.sector_size, 4096, "source fixture is not 4Kn");
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();

    let backup_path =
        std::env::temp_dir().join(format!("winfsp-4kn-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup from 4Kn");

    // Detach the source before mounting: two disks carrying the same fixture at
    // once would make "find the new fixture volume" ambiguous, and a test that
    // can accidentally verify the SOURCE proves nothing about the mount.
    drop(source);

    let pre_existing = fixture_letters();
    let scratch = std::env::temp_dir()
        .join("phoenix-systests")
        .join("winfsp-mounts");
    let session = WinFspMount::mount(&backup_path, &scratch)
        .expect("mounting a 4Kn backup (this used to be refused outright)");

    // The served file must be the VHDX, not the VHD — the container is chosen
    // from the backup's recorded sector size.
    let served = session.vhd_path();
    eprintln!("winfsp-served image: {}", served.display());
    assert_eq!(
        served.extension().and_then(|e| e.to_str()),
        Some("vhdx"),
        "a 4Kn backup must be served as VHDX; a fixed VHD cannot express its sector size"
    );

    // Windows' own view of the attached disk. This is the assertion the whole
    // feature reduces to.
    let attached = session
        .attached_disk_number()
        .expect("attached disk number");
    let ss = disk_logical_sector_size(attached);
    eprintln!("attached as PhysicalDrive{attached}, LogicalSectorSize = {ss}");
    assert_eq!(
        ss, 4096,
        "the mounted disk reports a {ss}-byte sector, so its 4Kn NTFS volume can never mount"
    );

    // And the data survives the round trip through the synthesized VHDX.
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
        "no volume appeared from the mounted 4Kn backup — Windows attached the disk but could \
         not mount its filesystem",
    );
    eprintln!("4Kn backup mounted at {letter}:");
    verify_fixture(letter, &digest).expect("fixture readable through the 4Kn WinFsp mount");

    drop(session);
    let _ = std::fs::remove_file(&backup_path);
}

/// Windows' `LogicalSectorSize` for an attached disk.
fn disk_logical_sector_size(disk_number: u32) -> u32 {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Disk -Number {disk_number}).LogicalSectorSize"),
        ])
        .output()
        .expect("Get-Disk");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}
