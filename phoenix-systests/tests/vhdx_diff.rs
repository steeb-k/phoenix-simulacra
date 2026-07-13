//! Diagnostic: dump the structure of a VHDX **Windows built itself** next to the
//! structure of the one we synthesize, so a rejected container can be diffed
//! against a known-good one instead of guessed at.
//!
//! `OpenVirtualDisk` answers `ERROR_FILE_CORRUPT` and nothing else. This is how
//! you find out which field it meant.
//!
//!   cargo test -p phoenix-systests --test vhdx_diff -- --ignored --nocapture

use phoenix_mount::vhdx::Vhdx;
use phoenix_systests::{create_vhdx_with_sector_size, require_admin};

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn dump(label: &str, buf: &[u8]) {
    println!("\n================ {label} ================");
    println!("file id      : {:?}", String::from_utf8_lossy(&buf[0..8]));

    for (name, off) in [("header1", 0x10000usize), ("header2", 0x20000)] {
        println!(
            "{name}: sig={:?} seq={} logver={} version={} loglen={} logoff={} logguid_zero={}",
            String::from_utf8_lossy(&buf[off..off + 4]),
            u64le(buf, off + 8),
            u16le(buf, off + 64),
            u16le(buf, off + 66),
            u32le(buf, off + 68),
            u64le(buf, off + 72),
            buf[off + 48..off + 64].iter().all(|&b| b == 0),
        );
    }

    for (name, off) in [("regtable1", 0x30000usize), ("regtable2", 0x40000)] {
        let n = u32le(buf, off + 8);
        println!(
            "{name}: sig={:?} entries={n}",
            String::from_utf8_lossy(&buf[off..off + 4])
        );
        for i in 0..n as usize {
            let e = off + 16 + i * 32;
            println!(
                "   guid={:02x?} off={} len={} required={}",
                &buf[e..e + 16],
                u64le(buf, e + 16),
                u32le(buf, e + 24),
                u32le(buf, e + 28)
            );
        }
    }

    // Find the metadata region by scanning region-table 1 for its GUID.
    let meta_guid: [u8; 16] = [
        0x06, 0xa2, 0x7c, 0x8b, 0x90, 0x47, 0x9a, 0x4b, 0xb8, 0xfe, 0x57, 0x5f, 0x05, 0x0f, 0x88,
        0x6e,
    ];
    let rt = 0x30000usize;
    let n = u32le(buf, rt + 8) as usize;
    let mut meta_off = None;
    for i in 0..n {
        let e = rt + 16 + i * 32;
        if buf[e..e + 16] == meta_guid {
            meta_off = Some(u64le(buf, e + 16) as usize);
        }
    }
    let Some(m) = meta_off else {
        println!("metadata: REGION NOT FOUND");
        return;
    };
    if m + 32 > buf.len() {
        println!("metadata: region at {m} is past the bytes we read");
        return;
    }
    let count = u16le(buf, m + 10) as usize;
    println!(
        "metadata @{m}: sig={:?} entries={count}",
        String::from_utf8_lossy(&buf[m..m + 8])
    );
    for i in 0..count {
        let e = m + 32 + i * 32;
        let off = u32le(buf, e + 16) as usize;
        let len = u32le(buf, e + 20) as usize;
        let flags = u32le(buf, e + 24);
        let val = &buf[m + off..m + off + len.min(16)];
        println!(
            "   item={:02x?} off={off} len={len} flags={flags:#b} value={val:02x?}",
            &buf[e..e + 16]
        );
    }
}

#[test]
#[ignore = "diagnostic; requires elevation"]
fn compare_our_vhdx_against_one_windows_made() {
    require_admin();
    let dir = std::env::temp_dir().join("phoenix-systests");
    std::fs::create_dir_all(&dir).unwrap();

    // --- The reference: a VHDX created by Windows' own virtual-disk API --------
    let real = dir.join(format!("ref-{}.vhdx", uuid::Uuid::new_v4().simple()));
    create_vhdx_with_sector_size(&real, 64 * 1024 * 1024, 4096).expect("CreateVirtualDisk");
    let real_bytes = std::fs::read(&real).expect("read reference vhdx");
    println!("reference file size: {} bytes", real_bytes.len());
    dump("WINDOWS-MADE (dynamic, 4Kn)", &real_bytes);
    let _ = std::fs::remove_file(&real);

    // --- Ours ------------------------------------------------------------------
    let v = Vhdx::new(64 * 1024 * 1024, 4096, [0x5A; 16]).expect("synthesize");
    let mut ours = vec![0u8; v.payload_start() as usize];
    v.read_at(0, &mut ours, |_, d| {
        d.fill(0);
        Ok(())
    })
    .unwrap();
    println!(
        "\nours: payload_start={} file_size={}",
        v.payload_start(),
        v.file_size()
    );
    dump("PHOENIX-MADE (4Kn)", &ours);
}
