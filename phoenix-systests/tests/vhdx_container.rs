//! Does **Windows** accept the VHDX we synthesize?
//!
//! `phoenix-mount`'s unit tests prove the container is internally consistent — the
//! signatures are in the right places, the CRC-32Cs validate, the BAT maps blocks
//! where the payload actually is. None of that is worth anything if Windows
//! disagrees, and Windows does not explain itself: a VHDX with (say) the IEEE
//! CRC-32 instead of Castagnoli is structurally perfect and simply refuses to
//! open, with an error code that says nothing about which field is wrong.
//!
//! So write the bytes out, hand them to `AttachVirtualDisk`, and ask Windows what
//! it thinks the disk's sector size is. That is the only test that can distinguish
//! "a valid VHDX" from "a file we are very confident is a valid VHDX".
//!
//! This needs no 4Kn hardware. It needs no hardware at all — the entire point.
//!
//!   cargo test -p phoenix-systests --test vhdx_container -- --ignored --test-threads=1 --nocapture

use std::io::Write;

use phoenix_mount::attach::AttachedDisk;
use phoenix_mount::vhdx::Vhdx;
use phoenix_systests::require_admin;

/// Materialize the synthesized container to a real file, zero-filling the payload.
///
/// A real mount never does this — it serves the same bytes on demand through
/// WinFsp — but writing them out is what lets Windows' own virtual-disk stack be
/// the judge of whether they are a VHDX.
fn write_vhdx(path: &std::path::Path, disk_size: u64, sector_size: u32) -> Vhdx {
    let v = Vhdx::new(disk_size, sector_size, [0x5A; 16]).expect("synthesize vhdx");
    let mut f = std::fs::File::create(path).expect("create vhdx file");
    let mut off = 0u64;
    let mut buf = vec![0u8; 1024 * 1024];
    while off < v.file_size() {
        let n = (v.file_size() - off).min(buf.len() as u64) as usize;
        v.read_at(off, &mut buf[..n], |_img_off, dst| {
            // The virtual disk is blank: we are testing the container, not content.
            dst.fill(0);
            Ok(())
        })
        .expect("read container");
        f.write_all(&buf[..n]).expect("write vhdx");
        off += n as u64;
    }
    f.flush().expect("flush");
    v
}

fn attached_sector_size(disk_number: u32) -> u32 {
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
        .unwrap_or_else(|_| {
            panic!(
                "could not read LogicalSectorSize of disk {disk_number}: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
}

/// The headline: a VHDX we built by hand, attached by Windows, reporting **4096**.
///
/// If this passes, mounting a 4Kn backup is a wiring problem. If it fails, the
/// container is wrong and no amount of wiring would have saved it.
#[test]
#[ignore = "requires elevation (AttachVirtualDisk)"]
fn windows_attaches_our_synthesized_4kn_vhdx() {
    require_admin();
    let dir = std::env::temp_dir().join("phoenix-systests");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join(format!("vhdx-4kn-{}.vhdx", uuid::Uuid::new_v4().simple()));

    // 64 MiB is plenty: the container's correctness does not depend on its size,
    // and a small file keeps the test honest about being fast.
    write_vhdx(&path, 64 * 1024 * 1024, 4096);

    let attached = AttachedDisk::attach_readonly(path.to_str().unwrap())
        .expect("Windows REFUSED our synthesized VHDX — the container is malformed");
    let n = attached
        .physical_drive_number()
        .expect("physical drive number");
    let ss = attached_sector_size(n);
    eprintln!("[vhdx] attached as PhysicalDrive{n}, LogicalSectorSize = {ss}");
    drop(attached);
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        ss, 4096,
        "Windows attached the disk but reports a {ss}-byte sector — the LogicalSectorSize \
         metadata item is not being honored, so a 4Kn volume still would not mount on it"
    );
}

/// The same container at 512, which is what a normal backup will use once the
/// mount path moves onto VHDX. Proves the sector size is genuinely driven by the
/// metadata rather than hardcoded, in both directions.
#[test]
#[ignore = "requires elevation (AttachVirtualDisk)"]
fn windows_attaches_our_synthesized_512_vhdx() {
    require_admin();
    let dir = std::env::temp_dir().join("phoenix-systests");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join(format!("vhdx-512-{}.vhdx", uuid::Uuid::new_v4().simple()));

    write_vhdx(&path, 64 * 1024 * 1024, 512);

    let attached = AttachedDisk::attach_readonly(path.to_str().unwrap())
        .expect("Windows refused our 512-byte-sector VHDX");
    let n = attached
        .physical_drive_number()
        .expect("physical drive number");
    let ss = attached_sector_size(n);
    eprintln!("[vhdx] attached as PhysicalDrive{n}, LogicalSectorSize = {ss}");
    drop(attached);
    let _ = std::fs::remove_file(&path);

    assert_eq!(ss, 512);
}
