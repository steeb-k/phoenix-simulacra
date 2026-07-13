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
use crate::gpt::{self, GptPart, LEADING_SECTORS, TRAILING_SECTORS};
use crate::vhd::{self, VHD_MAX_BYTES};

const SECTOR: u64 = 512;
/// 1 MiB partition alignment, matching what the restore path lays down.
pub(crate) const ALIGN: u64 = 1024 * 1024;

pub struct SyntheticVhd {
    store: ChunkStore,
    /// Protective MBR + primary GPT header + entry array (the first 34 sectors).
    gpt_leading: Vec<u8>,
    /// Backup GPT entry array + header (the last 33 sectors before the footer).
    gpt_trailing: Vec<u8>,
    /// 512-byte fixed-VHD footer.
    footer: [u8; 512],
    /// Virtual disk size (excludes the trailing VHD footer).
    disk_size: u64,
    spans: Vec<PartitionSpan>,
}

impl SyntheticVhd {
    /// Build the synthesized disk from an opened backup. Consumes the reader
    /// (the chunk store owns it for on-demand reads).
    pub fn build(reader: PhnxReader) -> Result<Self> {
        let (spans, raw_disk_size) = plan_layout(&reader, ALIGN);
        // Round up to a sector multiple and leave room for the trailing GPT +
        // footer beyond the last partition.
        let disk_size = align_up(raw_disk_size + TRAILING_SECTORS * SECTOR + ALIGN, SECTOR);
        if disk_size > VHD_MAX_BYTES {
            return Err(PhoenixError::Other(format!(
                "backup describes a {disk_size}-byte disk, larger than the {VHD_MAX_BYTES}-byte \
                 fixed-VHD limit; disks this large need the VHDX path (not yet implemented)"
            )));
        }

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
                    first_lba: s.disk_offset / SECTOR,
                    last_lba: (s.disk_offset + s.size) / SECTOR - 1,
                    attributes: 0,
                    name: entry.map(|e| e.name.clone()).unwrap_or_default(),
                }
            })
            .collect();
        let gpt_img = gpt::synthesize(disk_size, disk_guid, &parts);
        let footer = vhd::build_footer(disk_size, disk_guid);

        let store = ChunkStore::new(reader, spans.clone(), disk_size)?;

        Ok(Self {
            store,
            gpt_leading: gpt_img.leading,
            gpt_trailing: gpt_img.trailing,
            footer,
            disk_size,
            spans,
        })
    }

    /// Total addressable length: the virtual disk plus its 512-byte footer.
    pub fn total_len(&self) -> u64 {
        self.disk_size + SECTOR
    }

    pub fn disk_size(&self) -> u64 {
        self.disk_size
    }

    pub fn spans(&self) -> &[PartitionSpan] {
        &self.spans
    }

    /// Read `buf.len()` bytes starting at absolute `offset` in the synthesized
    /// image, filling any region past the end with zeros.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        buf.fill(0);
        let total = self.total_len();
        let leading_end = LEADING_SECTORS * SECTOR;
        let trailing_start = self.disk_size - TRAILING_SECTORS * SECTOR;

        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            if pos >= total {
                break; // past the footer: leave zeros
            }
            let remaining = buf.len() - done;
            // Pick the region owning `pos` and copy a contiguous run from it.
            let (src_slice, run): (&[u8], usize) = if pos < leading_end {
                let off = pos as usize;
                let n = remaining.min((leading_end - pos) as usize);
                (&self.gpt_leading[off..off + n], n)
            } else if pos >= self.disk_size {
                // VHD footer.
                let off = (pos - self.disk_size) as usize;
                let n = remaining.min((total - pos) as usize);
                (&self.footer[off..off + n], n)
            } else if pos >= trailing_start {
                let off = (pos - trailing_start) as usize;
                let n = remaining.min((self.disk_size - pos) as usize);
                (&self.gpt_trailing[off..off + n], n)
            } else {
                // Partition-data region: served on demand (zeros for gaps /
                // free space). Delegate a run up to the trailing-GPT boundary.
                let n = remaining.min((trailing_start - pos) as usize);
                self.store.read_at(pos, &mut buf[done..done + n])?;
                done += n;
                continue;
            };
            buf[done..done + run].copy_from_slice(src_slice);
            done += run;
        }
        Ok(())
    }
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
        Extent, Header, PhnxWriter, EXTENT_LBA_BYTES as LBA, FORMAT_VERSION,
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
            .begin_partition_stream(
                0,
                type_guid,
                "Vol".into(),
                ext_bytes,
                FilesystemKind::Ntfs,
                CaptureMode::UsedBlocks,
                LBA,
                0,
                &extents,
                4096,
            )
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

    /// GPT type GUID at entry `i` of the entry array (LBA 2 onward).
    fn entry_type_guid(vhd: &mut SyntheticVhd, i: usize) -> [u8; 16] {
        let mut entry = [0u8; 128];
        vhd.read_at(2 * SECTOR + i as u64 * 128, &mut entry)
            .unwrap();
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
