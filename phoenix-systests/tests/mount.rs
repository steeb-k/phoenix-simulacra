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

/// Diagnostic: create a REAL fixed VHD with diskpart, read its 512-byte footer,
/// and diff it field-by-field against our synthesized footer for the same size.
/// This pinpoints exactly which footer field Windows' virtual-disk driver
/// dislikes (OpenVirtualDisk rejected our image with 0xC03A001A).
#[test]
#[ignore = "requires elevation + diskpart"]
fn compare_vhd_footer_with_windows() {
    require_admin();
    let dir = std::env::temp_dir().join("phoenix-systests");
    std::fs::create_dir_all(&dir).unwrap();
    let vhd = dir.join(format!("ref-{}.vhd", uuid::Uuid::new_v4().simple()));
    let vhd_str = vhd.to_string_lossy().to_string();

    // 3 MB is the minimum fixed VHD diskpart will make.
    let script = dir.join(format!("dp-{}.txt", uuid::Uuid::new_v4().simple()));
    std::fs::write(
        &script,
        format!("create vdisk file=\"{vhd_str}\" maximum=3 type=fixed\n"),
    )
    .unwrap();
    let out = std::process::Command::new("diskpart")
        .arg("/s")
        .arg(&script)
        .output()
        .expect("run diskpart");
    let _ = std::fs::remove_file(&script);
    assert!(
        out.status.success(),
        "diskpart create fixed vdisk failed:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let bytes = std::fs::read(&vhd).expect("read reference vhd");
    assert!(bytes.len() >= 512, "reference vhd too small");
    let real: [u8; 512] = bytes[bytes.len() - 512..].try_into().unwrap();

    // Pull the reference footer's disk size + unique id so ours matches those.
    let size = u64::from_be_bytes(real[48..56].try_into().unwrap());
    let mut unique = [0u8; 16];
    unique.copy_from_slice(&real[68..84]);
    let mine = phoenix_mount::vhd::build_footer(size, unique);

    eprintln!("=== VHD footer comparison (reference size = {size}) ===");
    let fields: [(&str, usize, usize); 15] = [
        ("cookie", 0, 8),
        ("features", 8, 12),
        ("format_version", 12, 16),
        ("data_offset", 16, 24),
        ("timestamp", 24, 28),
        ("creator_app", 28, 32),
        ("creator_version", 32, 36),
        ("creator_host_os", 36, 40),
        ("original_size", 40, 48),
        ("current_size", 48, 56),
        ("geometry", 56, 60),
        ("disk_type", 60, 64),
        ("checksum", 64, 68),
        ("unique_id", 68, 84),
        ("saved_state", 84, 85),
    ];
    for (name, a, b) in fields {
        let r = &real[a..b];
        let m = &mine[a..b];
        let mark = if r == m { "ok " } else { "DIFF" };
        eprintln!("  [{mark}] {name:16} real={:02x?} mine={:02x?}", r, m);
    }
    // Also show any diff in the reserved area.
    if real[85..] != mine[85..] {
        eprintln!("  [DIFF] reserved area differs");
    }

    let _ = std::process::Command::new("diskpart")
        .arg("/s")
        .arg({
            let s = dir.join(format!("dp2-{}.txt", uuid::Uuid::new_v4().simple()));
            std::fs::write(
                &s,
                format!("select vdisk file=\"{vhd_str}\"\ndetach vdisk\n"),
            )
            .ok();
            s
        })
        .output();
    let _ = std::fs::remove_file(&vhd);
}

/// Drive letters D..Z whose root contains a `phoenix-fixture` directory.
/// Callers snapshot this BEFORE mounting and look for a NEW letter after —
/// matching on the marker alone is not enough, because any volume another
/// suite filled with a fixture can still be attached (observed: the T3
/// real-disk target left online after its restore held `phoenix-fixture`
/// on an earlier letter than the fresh mount, and the fixture compare then
/// diffed against the wrong volume).
fn fixture_letters() -> Vec<char> {
    (b'D'..=b'Z')
        .map(|c| c as char)
        .filter(|l| Path::new(&format!("{l}:\\phoenix-fixture")).exists())
        .collect()
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
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");

    // Mount it read-only. Snapshot fixture-bearing volumes first so a
    // leftover fixture on some other attached disk can't be mistaken for
    // the mount (the source X: is naturally in this set too).
    let pre_existing = fixture_letters();
    let scratch = std::env::temp_dir().join("phoenix-systests").join("mounts");
    let session = MountSession::mount(&backup_path, &scratch).expect("mount");

    // Find the mounted volume (a NEW letter carrying the fixture) and verify.
    let mut mounted_letter = None;
    for _ in 0..30 {
        if let Some(l) = fixture_letters()
            .into_iter()
            .find(|l| !pre_existing.contains(l))
        {
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
