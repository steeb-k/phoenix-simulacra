//! Debug probe: open a .phnx, build the SyntheticVhd, and dump what the
//! mount would serve for each partition's boot sector, plus a full scan of
//! every captured extent to surface on-demand read errors.
//!
//! Usage: cargo run -p phoenix-mount --example probe -- <backup.phnx> [--scan]

use phoenix_core::container::PhnxReader;
use phoenix_mount::SyntheticVhd;

fn hexdump(buf: &[u8]) {
    for row in buf.chunks(16) {
        let hex: Vec<String> = row.iter().map(|b| format!("{b:02X}")).collect();
        let ascii: String = row
            .iter()
            .map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("    {} |{}|", hex.join(" "), ascii);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: probe <backup.phnx> [--scan]");
    let scan = args.next().as_deref() == Some("--scan");

    let reader = PhnxReader::open(std::path::Path::new(&path)).unwrap();
    println!("disk style: {}", reader.manifest.disk.style);
    for e in &reader.index {
        println!(
            "index entry {}: name={:?} fs={:?} mode={:?} original_size={} used={} type_guid={:02X?}",
            e.index, e.name, e.fs_kind, e.capture_mode, e.original_size, e.used_bytes, e.type_guid
        );
    }

    let mut vhd = SyntheticVhd::build(reader).unwrap();
    let spans: Vec<_> = vhd.spans().to_vec();
    println!(
        "\ndisk_size={} ({:.1} MiB)",
        vhd.disk_size(),
        vhd.disk_size() as f64 / 1048576.0
    );

    for s in &spans {
        println!(
            "\npartition {}: disk_offset={} ({} MiB) size={} ({:.1} MiB)",
            s.partition_index,
            s.disk_offset,
            s.disk_offset / 1048576,
            s.size,
            s.size as f64 / 1048576.0
        );
        let mut boot = vec![0u8; 512];
        match vhd.read_at(s.disk_offset, &mut boot) {
            Ok(()) => {
                println!("  boot sector (first 96 bytes):");
                hexdump(&boot[..96]);
                println!(
                    "  sig 0x55AA at 510: {}",
                    if boot[510] == 0x55 && boot[511] == 0xAA {
                        "YES"
                    } else {
                        "NO"
                    }
                );
                if &boot[3..7] == b"NTFS" {
                    let total_sectors = u64::from_le_bytes(boot[0x28..0x30].try_into().unwrap());
                    let hidden = u32::from_le_bytes(boot[0x1C..0x20].try_into().unwrap());
                    println!(
                        "  NTFS: TotalSectors={} (= {:.1} MiB), partition holds {} sectors; HiddenSectors={}",
                        total_sectors,
                        total_sectors as f64 * 512.0 / 1048576.0,
                        s.size / 512,
                        hidden
                    );
                } else if &boot[3..11] == b"EXFAT   " {
                    let vol_len = u64::from_le_bytes(boot[0x48..0x50].try_into().unwrap());
                    let part_off = u64::from_le_bytes(boot[0x40..0x48].try_into().unwrap());
                    println!(
                        "  exFAT: VolumeLength={} sectors (= {:.1} MiB), partition holds {} sectors; PartitionOffset field={}",
                        vol_len,
                        vol_len as f64 * 512.0 / 1048576.0,
                        s.size / 512,
                        part_off
                    );
                } else {
                    println!("  no NTFS/exFAT signature at partition start");
                }
            }
            Err(e) => println!("  BOOT SECTOR READ ERROR: {e}"),
        }
        // Last sector of the partition (NTFS keeps a backup boot sector there).
        let mut tail = vec![0u8; 512];
        match vhd.read_at(s.disk_offset + s.size - 512, &mut tail) {
            Ok(()) => println!(
                "  last sector: sig 0x55AA: {}  first bytes: {:02X?}",
                if tail[510] == 0x55 && tail[511] == 0xAA {
                    "YES"
                } else {
                    "NO"
                },
                &tail[..8]
            ),
            Err(e) => println!("  LAST SECTOR READ ERROR: {e}"),
        }
    }

    if scan {
        println!("\nscanning all partition data through read_at (1 MiB reads)…");
        let mut buf = vec![0u8; 1048576];
        for s in &spans {
            let mut errors = 0u32;
            let mut off = s.disk_offset;
            let end = s.disk_offset + s.size;
            while off < end {
                let n = buf.len().min((end - off) as usize);
                if let Err(e) = vhd.read_at(off, &mut buf[..n]) {
                    if errors == 0 {
                        println!(
                            "  partition {}: FIRST READ ERROR at partition byte {}: {e}",
                            s.partition_index,
                            off - s.disk_offset
                        );
                    }
                    errors += 1;
                }
                off += n as u64;
            }
            println!(
                "  partition {}: scan done, {} failed reads",
                s.partition_index, errors
            );
        }
    }
}
