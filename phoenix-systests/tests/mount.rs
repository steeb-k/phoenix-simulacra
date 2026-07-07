//! Tier-2 system test for mounting: back up a filled NTFS volume, mount the
//! resulting .phnx read-only, and confirm the fixture files are browsable and
//! byte-identical through the attached virtual disk.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests -- --ignored --test-threads=1

use std::path::Path;

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::disk::enumerate_disks;
use phoenix_mount::MountSession;
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, verify_fixture, wait_for_letter, PartSpec,
    TestFs, TestVhd,
};

/// Scan drive letters D..Z for one whose root contains `phoenix-fixture`,
/// excluding `exclude`. Returns the first match.
fn find_fixture_letter(exclude: char) -> Option<char> {
    for c in b'D'..=b'Z' {
        let letter = c as char;
        if letter == exclude {
            continue;
        }
        let marker = format!("{letter}:\\phoenix-fixture");
        if Path::new(&marker).exists() {
            return Some(letter);
        }
    }
    None
}

#[test]
#[ignore = "requires elevation + diskpart"]
fn mount_backup_and_browse_files() {
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
    let digest = fill_fixture('X', 0x7777).expect("fill fixture");

    // Back it up.
    let disks = enumerate_disks().unwrap();
    let disk = disks
        .iter()
        .find(|d| d.index == source.disk_index())
        .unwrap();
    let parts: Vec<u32> = disk.partitions.iter().map(|p| p.index).collect();
    let backup_path =
        std::env::temp_dir().join(format!("mount-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: parts,
        output: backup_path.clone(),
        use_vss: false,
        progress: None,
    })
    .expect("run_backup");

    // Mount it read-only.
    let scratch = std::env::temp_dir().join("phoenix-systests").join("mounts");
    let session = MountSession::mount(&backup_path, &scratch).expect("mount");

    // Find the mounted volume (a new letter carrying the fixture) and verify.
    let mut mounted_letter = None;
    for _ in 0..30 {
        if let Some(l) = find_fixture_letter('X') {
            mounted_letter = Some(l);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let letter = mounted_letter.expect("mounted fixture volume never appeared");
    verify_fixture(letter, &digest).expect("fixture readable through the mount");

    drop(session); // unmount + delete temp image
    let _ = std::fs::remove_file(&backup_path);
}
