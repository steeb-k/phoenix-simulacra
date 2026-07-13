//! A synthesized read-only virtual disk over a `.phnx` backup: `[protective
//! MBR + primary GPT] [partition data, served on demand from the chunk store]
//! [backup GPT] [fixed-VHD footer]`. Exposes `read_at` over the whole image
//! WITHOUT materializing anything, so it can back both the (stopgap) file
//! materialization and the space-efficient WinFsp on-demand mount — the WinFsp
//! filesystem's read handler just calls [`SyntheticVhd::read_at`].
//!
//! Every partition-data read goes through [`ChunkStore`], which decompresses and
//! BLAKE3-verifies each chunk on a cache miss, so corruption surfaces as a read
//! error rather than silent garbage.

use phoenix_core::container::PhnxReader;
use phoenix_core::error::{PhoenixError, Result};

use crate::chunkstore::{plan_layout, ChunkStore, PartitionSpan};
use crate::gpt::{self, GptPart};
use crate::vhd::{self, VHD_MAX_BYTES};
use crate::vhdx::Vhdx;

/// 1 MiB partition alignment, matching what the restore path lays down.
pub(crate) const ALIGN: u64 = 1024 * 1024;

/// How the raw disk image is wrapped for `AttachVirtualDisk`.
///
/// The two formats differ in exactly one way that matters here: a fixed VHD
/// **cannot express a sector size** (512 is baked into the format), and VHDX can.
/// So a 512e backup keeps the VHD path — it is simpler, it is what every existing
/// mount test exercises, and there is nothing to gain by moving it — while a 4Kn
/// backup gets a VHDX, without which its volumes could be attached but never
/// mounted.
enum Container {
    /// `[disk image][512-byte footer]`.
    Vhd(Box<[u8; 512]>),
    /// `[prologue][disk image]`, with the logical sector size stated in metadata.
    Vhdx(Box<Vhdx>),
}

pub struct SyntheticVhd {
    store: ChunkStore,
    /// Protective MBR + primary GPT header + entry array.
    gpt_leading: Vec<u8>,
    /// Backup GPT entry array + header, at the end of the disk.
    gpt_trailing: Vec<u8>,
    container: Container,
    /// Logical sector size the synthesized disk advertises — 512, or 4096 when the
    /// backup came from a 4Kn disk. Every GPT LBA below is in these units.
    sector_size: u64,
    /// Byte offset at which the trailing (backup) GPT begins.
    trailing_start: u64,
    /// Byte length of the leading GPT region.
    leading_end: u64,
    /// Virtual disk size (excludes the trailing VHD footer).
    disk_size: u64,
    spans: Vec<PartitionSpan>,
}

impl SyntheticVhd {
    /// Build the synthesized disk from an opened backup. Consumes the reader
    /// (the chunk store owns it for on-demand reads).
    pub fn build(reader: PhnxReader) -> Result<Self> {
        // The synthesized disk must advertise the SOURCE disk's sector size. A
        // volume captured from a 4Kn disk records `BytesPerSector = 4096` in its
        // own boot sector, and NTFS refuses to mount when the filesystem's sector
        // size disagrees with the device it is sitting on. Present such a volume on
        // a 512-byte device and Windows attaches the disk, then calls it RAW.
        let sector_size = match reader.manifest.disk.sector_size {
            0 | 512 => 512u64,
            4096 => 4096u64,
            other => {
                return Err(PhoenixError::Other(format!(
                    "backup came from a disk with a {other}-byte sector, which neither VHD (512 \
                     only) nor VHDX (512 or 4096) can express"
                )))
            }
        };

        let (spans, raw_disk_size) = plan_layout(&reader, ALIGN);
        let trail_bytes = gpt::trailing_sectors(sector_size) * sector_size;
        // Round up to a sector multiple and leave room for the trailing GPT past
        // the last partition.
        let disk_size = align_up(raw_disk_size + trail_bytes + ALIGN, sector_size);

        // A fixed VHD cannot say what its sector size is — 512 is the format, not
        // a field — so 4Kn must go through VHDX. VHDX also has no 2040 GiB ceiling,
        // which is the other thing that used to make a mount simply refuse.
        let container = if sector_size == 512 {
            if disk_size > VHD_MAX_BYTES {
                return Err(PhoenixError::Other(format!(
                    "backup describes a {disk_size}-byte disk, larger than the \
                     {VHD_MAX_BYTES}-byte fixed-VHD limit"
                )));
            }
            Container::Vhd(Box::new(vhd::build_footer(
                disk_size,
                *reader.header.backup_id.as_bytes(),
            )))
        } else {
            Container::Vhdx(Box::new(Vhdx::new(
                disk_size,
                sector_size as u32,
                *reader.header.backup_id.as_bytes(),
            )?))
        };

        // Synthesize the GPT + footer from the layout while we still hold the
        // reader's index.
        let disk_guid = *reader.header.backup_id.as_bytes();
        let parts: Vec<GptPart> = spans
            .iter()
            .map(|s| {
                let entry = reader.index.iter().find(|e| e.index == s.partition_index);
                // MBR-source backups store an all-zero type GUID; in GPT that
                // means "unused entry", so substitute Basic Data (as restore
                // does) or Windows presents the disk as blank.
                let type_guid = match entry.map(|e| e.type_guid) {
                    Some(g) if g != [0u8; 16] => g,
                    _ => gpt::BASIC_DATA_TYPE_GUID,
                };
                GptPart {
                    type_guid,
                    unique_guid: derive_guid(&disk_guid, s.partition_index),
                    // LBAs are in the DISK's sectors, so partition offsets divide
                    // by 4096 on a 4Kn disk, not 512. Partition spans are 1 MiB
                    // aligned, so both divide cleanly.
                    first_lba: s.disk_offset / sector_size,
                    last_lba: (s.disk_offset + s.size) / sector_size - 1,
                    attributes: 0,
                    name: entry.map(|e| e.name.clone()).unwrap_or_default(),
                }
            })
            .collect();
        let gpt_img = gpt::synthesize(disk_size, disk_guid, &parts, sector_size);

        let leading_end = gpt::leading_sectors(sector_size) * sector_size;
        let trailing_start = disk_size - gpt::trailing_sectors(sector_size) * sector_size;
        debug_assert_eq!(gpt_img.leading.len() as u64, leading_end);
        debug_assert_eq!(gpt_img.trailing.len() as u64, disk_size - trailing_start);

        let store = ChunkStore::new(reader, spans.clone(), disk_size)?;

        Ok(Self {
            store,
            sector_size,
            leading_end,
            trailing_start,
            container,
            gpt_leading: gpt_img.leading,
            gpt_trailing: gpt_img.trailing,
            disk_size,
            spans,
        })
    }

    /// Total length of the **file** the mount serves: the virtual disk plus
    /// whatever the container wraps it in.
    pub fn total_len(&self) -> u64 {
        match &self.container {
            // `[disk image][512-byte footer]`
            Container::Vhd(_) => self.disk_size + 512,
            // `[prologue][disk image][padding to a whole block]`
            Container::Vhdx(v) => v.file_size(),
        }
    }

    pub fn disk_size(&self) -> u64 {
        self.disk_size
    }

    /// The logical sector size the attached disk will report — 512, or 4096 for a
    /// backup taken from a 4Kn disk.
    pub fn sector_size(&self) -> u64 {
        self.sector_size
    }

    /// The filename to serve. `OpenVirtualDisk` sniffs the content, but the
    /// extension is what a human (and some tooling) will trust.
    pub fn image_name(&self) -> &'static str {
        match &self.container {
            Container::Vhd(_) => "backup.vhd",
            Container::Vhdx(_) => "backup.vhdx",
        }
    }

    pub fn spans(&self) -> &[PartitionSpan] {
        &self.spans
    }

    /// Read `buf.len()` bytes at `offset` **in the container file**, which is what
    /// the mount actually serves.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        // Split the borrow: the VHDX arm needs `&container` and `&mut store` at
        // once, and they are disjoint fields.
        let Self {
            store,
            gpt_leading,
            gpt_trailing,
            container,
            disk_size,
            leading_end,
            trailing_start,
            ..
        } = self;

        match container {
            Container::Vhd(footer) => {
                // `[disk image][footer]`. The footer sits past the disk, so serve
                // it here and let everything below it be the disk image.
                buf.fill(0);
                let total = *disk_size + 512;
                let mut done = 0usize;
                while done < buf.len() {
                    let pos = offset + done as u64;
                    if pos >= total {
                        break;
                    }
                    let remaining = buf.len() - done;
                    if pos >= *disk_size {
                        let off = (pos - *disk_size) as usize;
                        let n = remaining.min((total - pos) as usize);
                        buf[done..done + n].copy_from_slice(&footer[off..off + n]);
                        done += n;
                    } else {
                        let n = remaining.min((*disk_size - pos) as usize);
                        read_disk_image(
                            store,
                            gpt_leading,
                            gpt_trailing,
                            *disk_size,
                            *leading_end,
                            *trailing_start,
                            pos,
                            &mut buf[done..done + n],
                        )?;
                        done += n;
                    }
                }
                Ok(())
            }
            Container::Vhdx(v) => {
                // `[prologue][disk image]`. The VHDX knows where its payload starts
                // and hands us plain image offsets — so the disk image below is
                // written once and both containers use it.
                v.read_at(offset, buf, |image_off, dst| {
                    read_disk_image(
                        store,
                        gpt_leading,
                        gpt_trailing,
                        *disk_size,
                        *leading_end,
                        *trailing_start,
                        image_off,
                        dst,
                    )
                })
            }
        }
    }
}

/// The raw disk image, container-free: `[leading GPT][partition data][backup GPT]`.
///
/// Both containers are only framing around this. Keeping it a free function is
/// what lets the VHDX arm borrow the chunk store mutably while borrowing the
/// container immutably.
#[allow(clippy::too_many_arguments)]
fn read_disk_image(
    store: &mut ChunkStore,
    gpt_leading: &[u8],
    gpt_trailing: &[u8],
    disk_size: u64,
    leading_end: u64,
    trailing_start: u64,
    offset: u64,
    buf: &mut [u8],
) -> Result<()> {
    buf.fill(0);
    let mut done = 0usize;
    while done < buf.len() {
        let pos = offset + done as u64;
        if pos >= disk_size {
            break; // past the disk: leave zeros
        }
        let remaining = buf.len() - done;
        let (src, run): (&[u8], usize) = if pos < leading_end {
            let off = pos as usize;
            let n = remaining.min((leading_end - pos) as usize);
            (&gpt_leading[off..off + n], n)
        } else if pos >= trailing_start {
            let off = (pos - trailing_start) as usize;
            let n = remaining.min((disk_size - pos) as usize);
            (&gpt_trailing[off..off + n], n)
        } else {
            // Partition data: served on demand, zeros for gaps and free space.
            let n = remaining.min((trailing_start - pos) as usize);
            store.read_at(pos, &mut buf[done..done + n])?;
            done += n;
            continue;
        };
        buf[done..done + run].copy_from_slice(src);
        done += run;
    }
    Ok(())
}

fn align_up(v: u64, a: u64) -> u64 {
    if a == 0 {
        v
    } else {
        v.div_ceil(a) * a
    }
}

/// Deterministic 16-byte GUID from the disk GUID + partition index (for the
/// synthesized GPT's per-partition unique id).
fn derive_guid(disk_guid: &[u8; 16], index: u32) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(disk_guid);
    hasher.update(&index.to_le_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&hasher.finalize().as_bytes()[0..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::container::{
        Extent, Header, PartitionStreamSpec, PhnxWriter, EXTENT_LBA_BYTES as LBA, FORMAT_VERSION,
    };
    use phoenix_core::disk::{CaptureMode, FilesystemKind};
    use phoenix_core::manifest::{BackupManifest, DiskManifest, PartitionManifest};
    use uuid::Uuid;

    fn build_backup() -> std::path::PathBuf {
        build_backup_with_type_guid([0x11; 16])
    }

    fn build_backup_with_type_guid(type_guid: [u8; 16]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("synth_{}.phnx", Uuid::new_v4()));
        let backup_id = Uuid::new_v4();
        let header = Header {
            version: FORMAT_VERSION,
            flags: 1,
            timestamp: 1,
            backup_id,
            disk_signature: 1,
            partition_count: 1,
        };
        let ext_bytes = 64 * 1024u64; // one 64 KiB extent
        let extents = vec![Extent {
            start_sector: 0,
            sector_count: ext_bytes / LBA as u64,
        }];
        let mut w = PhnxWriter::create(&path, header).unwrap();
        let mut s = w
            .begin_partition_stream(PartitionStreamSpec {
                index: 0,
                type_guid,
                name: "Vol".into(),
                original_size: ext_bytes,
                fs_kind: FilesystemKind::Ntfs,
                capture_mode: CaptureMode::UsedBlocks,
                sector_size: LBA,
                used_bytes: 0,
                extents: &extents,
                bytes_per_cluster: 4096,
            })
            .unwrap();
        s.write_chunk(&vec![0x5Au8; ext_bytes as usize]).unwrap();
        let (chunks, _) = s.finish().unwrap();
        let manifest = BackupManifest {
            format_version: 1,
            backup_id,
            parent_backup_id: None,
            hostname: "T".into(),
            disk: DiskManifest {
                style: "gpt".into(),
                disk_guid: None,
                sector_size: 512,
            },
            partitions: vec![PartitionManifest {
                index: 0,
                name: "Vol".into(),
                type_guid: None,
                fs: "ntfs".into(),
                capture_mode: "used-blocks".into(),
                original_size: ext_bytes,
                used_bytes: ext_bytes,
                bitlocker: None,
                unique_guid: None,
                gpt_attributes: None,
                chunks,
                bitmap_hash: None,
            }],
        };
        w.finalize(&manifest).unwrap();
        path
    }

    #[test]
    fn synthetic_disk_serves_gpt_data_and_footer() {
        let path = build_backup();
        let reader = PhnxReader::open(&path).unwrap();
        let mut vhd = SyntheticVhd::build(reader).unwrap();

        // Protective MBR signature at the end of sector 0.
        let mut sec0 = vec![0u8; 512];
        vhd.read_at(0, &mut sec0).unwrap();
        assert_eq!(sec0[510], 0x55);
        assert_eq!(sec0[511], 0xAA);
        // GPT signature at LBA 1.
        let mut sec1 = vec![0u8; 512];
        vhd.read_at(512, &mut sec1).unwrap();
        assert_eq!(&sec1[0..8], b"EFI PART");

        // Partition data (0x5A) at the partition's disk offset.
        let poff = vhd.spans()[0].disk_offset;
        let mut data = vec![0u8; 4096];
        vhd.read_at(poff, &mut data).unwrap();
        assert!(data.iter().all(|&b| b == 0x5A), "partition data not served");

        // VHD footer cookie at the very end.
        let total = vhd.total_len();
        let mut foot = vec![0u8; 512];
        vhd.read_at(total - 512, &mut foot).unwrap();
        assert_eq!(&foot[0..8], b"conectix");

        // A read straddling the GPT-leading / data-gap boundary is zero past the
        // GPT and returns without error.
        let mut straddle = vec![0xFFu8; 1024];
        vhd.read_at(16 * 512, &mut straddle).unwrap(); // sector 16, inside leading region+gap
                                                       // (No panic / no error is the assertion; content is GPT-or-zero.)

        std::fs::remove_file(&path).ok();
    }

    /// GPT type GUID at entry `i` of the entry array, which starts at LBA 2 — so
    /// its byte offset depends on the disk's sector size.
    fn entry_type_guid(vhd: &mut SyntheticVhd, i: usize) -> [u8; 16] {
        let at = 2 * vhd.sector_size() + i as u64 * 128;
        let mut entry = [0u8; 128];
        vhd.read_at(at, &mut entry).unwrap();
        entry[0..16].try_into().unwrap()
    }

    /// Regression test: backups of MBR disks record an all-zero type GUID,
    /// which in GPT marks the entry as UNUSED — Windows saw a blank disk and
    /// mounted no volumes. The synthesized GPT must substitute Basic Data.
    #[test]
    fn zero_type_guid_falls_back_to_basic_data() {
        let path = build_backup_with_type_guid([0u8; 16]);
        let reader = PhnxReader::open(&path).unwrap();
        let mut vhd = SyntheticVhd::build(reader).unwrap();
        assert_eq!(entry_type_guid(&mut vhd, 0), gpt::BASIC_DATA_TYPE_GUID);
        std::fs::remove_file(&path).ok();
    }

    /// A real (GPT-source) type GUID must pass through untouched.
    #[test]
    fn nonzero_type_guid_is_preserved() {
        let path = build_backup();
        let reader = PhnxReader::open(&path).unwrap();
        let mut vhd = SyntheticVhd::build(reader).unwrap();
        assert_eq!(entry_type_guid(&mut vhd, 0), [0x11; 16]);
        std::fs::remove_file(&path).ok();
    }
}
