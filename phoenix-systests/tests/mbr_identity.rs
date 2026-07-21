//! Tier-2 MBR identity: a capture of a BIOS/MBR disk must preserve the NT disk
//! signature, the partition type bytes and the active flag, and the synthesized
//! virtual disk must reproduce them in a real MBR.
//!
//! The MBR counterpart of `gpt_identity.rs`, and load-bearing for the same
//! reason: on a BIOS install the BCD names the boot disk by the four bytes at
//! offset 0x1B8, so a disk synthesized with a fresh signature fails to boot the
//! way a regenerated GPT identity fails `winload.efi` with 0xc000000e. The type
//! byte and active flag cannot be recovered afterwards either — a filesystem
//! cannot distinguish a plain NTFS volume from a hidden recovery one, and a
//! table with nothing active does not boot at all.
//!
//! This proves the *data path* (capture → manifest → synthesized MBR) without a
//! guest. Whether a BIOS guest actually boots such a disk is the job of the
//! boot tests; this is what has to be right before that can be.
//!
//! Requires an elevated shell:
//!   cargo test -p phoenix-systests --test mbr_identity -- --ignored --test-threads=1 --nocapture

use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::enumerate_disks;
use phoenix_mount::synthetic::SyntheticVhd;
use phoenix_systests::{
    cleanup_leaked_vhds, fill_fixture, require_admin, wait_for_letter, PartSpec, TestFs, TestVhd,
};

const VHD_MB: u64 = 512;

/// Read LBA 0 straight off the attached disk, so every expectation below comes
/// from Windows rather than from our own code.
fn read_mbr_sector(disk_index: u32) -> [u8; 512] {
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ_WRITE: u32 = 0x0000_0001 | 0x0000_0002;

    let mut f = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_WRITE)
        .open(format!(r"\\.\PhysicalDrive{disk_index}"))
        .expect("open physical drive");
    let mut sector = [0u8; 512];
    f.read_exact(&mut sector).expect("read MBR");
    assert_eq!(&sector[510..512], &[0x55, 0xAA], "source disk has no MBR");
    sector
}

#[test]
#[ignore = "requires elevation + diskpart"]
fn mbr_identity_survives_capture_and_synthesis() {
    require_admin();
    let _ = cleanup_leaked_vhds();

    // Two partitions, shaped like a BIOS Windows layout: a small "System
    // Reserved" and a larger volume.
    let mut source = TestVhd::create(VHD_MB).expect("create source vhd");
    source
        .init_mbr_with(&[
            PartSpec {
                size_mb: 100,
                fs: TestFs::Ntfs,
                letter: 'X',
                label: "SYSRES".into(),
            },
            PartSpec {
                size_mb: 0,
                fs: TestFs::Ntfs,
                letter: 'Y',
                label: "MBRWIN".into(),
            },
        ])
        .expect("init source as MBR");
    assert!(wait_for_letter('X', 15_000), "system volume never mounted");
    assert!(wait_for_letter('Y', 15_000), "data volume never mounted");
    // diskpart leaves a fresh MBR disk with NOTHING active, so a fixture that
    // means to model a bootable layout has to set it — as Windows Setup does
    // on System Reserved, the first partition here.
    source.set_mbr_active(1).expect("mark partition 1 active");
    fill_fixture('X', 0xB105).expect("fill system fixture");
    fill_fixture('Y', 0xB106).expect("fill data fixture");

    // Give the disk real MBR boot code, as a bootable disk has and a freshly
    // partitioned one does not. bootsect is what Windows Setup uses, and it is
    // what our own boot-repair path shells out to.
    let bootsect = std::process::Command::new("bootsect.exe")
        .args(["/nt60", "X:", "/mbr"])
        .output()
        .expect("run bootsect");
    assert!(
        bootsect.status.success(),
        "bootsect failed: {}{}",
        String::from_utf8_lossy(&bootsect.stdout),
        String::from_utf8_lossy(&bootsect.stderr)
    );

    // What Windows says the disk is, before we touch it.
    let source_mbr = read_mbr_sector(source.disk_index());
    let expected_signature = u32::from_le_bytes(source_mbr[0x1B8..0x1BC].try_into().unwrap());
    let expected_boot_code = source_mbr[..440].to_vec();
    assert_ne!(expected_signature, 0, "diskpart left a zero disk signature");
    assert!(
        expected_boot_code.iter().any(|&b| b != 0),
        "bootsect left no MBR boot code to capture"
    );

    let disk = enumerate_disks()
        .unwrap()
        .into_iter()
        .find(|d| d.index == source.disk_index())
        .expect("source disk enumerates");
    assert!(!disk.is_gpt, "fixture disk should be MBR");
    let expected_types: Vec<u8> = disk.partitions.iter().map(|p| p.mbr_type).collect();
    let expected_active: Vec<bool> = disk.partitions.iter().map(|p| p.mbr_bootable).collect();
    assert!(
        expected_types.iter().all(|&t| t != 0),
        "enumeration read no MBR partition types: {expected_types:?}"
    );
    assert!(
        expected_active.iter().any(|&b| b),
        "no partition is active on the source disk"
    );

    let backup_path =
        std::env::temp_dir().join(format!("mbrid-{}.phnx", uuid::Uuid::new_v4().simple()));
    run_backup(BackupOptions {
        disk_index: source.disk_index(),
        partition_indices: disk.partitions.iter().map(|p| p.index).collect(),
        output: backup_path.clone(),
        verify_after: true,
        verify_image: false,
        progress: None,
    })
    .expect("run_backup");

    // 1. The manifest carries the identity. Without this the rest is moot —
    //    it cannot be reconstructed later from anything on the disk.
    let reader = PhnxReader::open(&backup_path).unwrap();
    assert_eq!(
        reader.manifest.disk.style, "mbr",
        "manifest should record an MBR source"
    );
    assert_eq!(
        reader.manifest.disk.disk_signature,
        Some(expected_signature),
        "NT disk signature must survive capture — the BCD names the boot disk by it"
    );
    assert_eq!(
        reader
            .manifest
            .disk
            .mbr_boot_code
            .as_deref()
            .and_then(phoenix_core::hash::hex_decode_vec),
        Some(expected_boot_code.clone()),
        "MBR boot code must survive capture — it lives outside every partition, \
         so nothing else in the backup holds it, and without it the disk hangs"
    );
    for (i, p) in reader.manifest.partitions.iter().enumerate() {
        assert_eq!(
            p.mbr_type,
            Some(expected_types[i]),
            "partition {i} lost its MBR type byte"
        );
        assert_eq!(
            p.mbr_bootable,
            Some(expected_active[i]),
            "partition {i} lost its active flag"
        );
    }

    // 2. The synthesized disk reproduces it as a REAL MBR — not GPT's 0xEE
    //    protective one, which would tell a BIOS guest the disk is empty.
    let mut synth = SyntheticVhd::build(reader).expect("synthesize");
    let mut sector = vec![0u8; 512];
    synth.read_at(0, &mut sector).expect("read synthesized MBR");

    assert_eq!(&sector[510..512], &[0x55, 0xAA], "no MBR boot signature");
    assert_eq!(
        &sector[..440],
        &expected_boot_code[..],
        "synthesized disk lost the boot code — a BIOS guest would execute zeros"
    );
    assert_eq!(
        &sector[0x1B8..0x1BC],
        &expected_signature.to_le_bytes(),
        "synthesized disk has the wrong NT signature"
    );
    // No GPT: LBA 1 must not be an EFI PART header.
    let mut lba1 = vec![0u8; 512];
    synth.read_at(512, &mut lba1).expect("read LBA 1");
    assert_ne!(
        &lba1[0..8],
        b"EFI PART",
        "synthesized a GPT for an MBR source"
    );

    for (i, want_type) in expected_types.iter().enumerate() {
        let base = 446 + i * 16;
        assert_eq!(sector[base + 4], *want_type, "entry {i} type byte");
        assert_eq!(
            sector[base] == 0x80,
            expected_active[i],
            "entry {i} active flag"
        );
        assert_ne!(
            sector[base + 4],
            0xEE,
            "entry {i} is a protective GPT entry"
        );
    }

    // 3. Partition data still reads back through the synthesized disk. The
    //    leading region is ONE sector for MBR (34 for GPT), so a wrong
    //    boundary would surface here as garbage rather than fixture bytes.
    let first_lba = u32::from_le_bytes(sector[454..458].try_into().unwrap()) as u64;
    assert!(first_lba > 0, "first partition starts at LBA 0");
    let mut head = vec![0u8; 512];
    synth
        .read_at(first_lba * 512, &mut head)
        .expect("read first partition");
    assert_eq!(
        &head[3..11],
        b"NTFS    ",
        "first partition does not start with an NTFS boot sector"
    );

    drop(synth);
    source.detach().ok();
    std::fs::remove_file(&backup_path).ok();
}
