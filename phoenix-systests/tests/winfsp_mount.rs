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
        use_vss: false,
        verify_after: true,
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
