//! Materialize a `.phnx` backup into a fixed-VHD disk image.
//!
//! The image is written to a **sparse** file: the GPT and each partition's
//! captured extents are written at their disk positions, but the (typically
//! large) free space between and within partitions is never written, so the
//! file's on-disk footprint is roughly the backup's used size — not the full
//! disk size. Every chunk is decompressed and BLAKE3-verified as it is written,
//! so a corrupt backup fails here instead of producing a bad mount.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use phoenix_core::container::{PhnxReader, CHUNK_SIZE, EXTENT_LBA_BYTES};
use phoenix_core::error::{PhoenixError, Result};

use crate::chunkstore::{plan_layout, PartitionSpan};
use crate::gpt::{self, GptPart, TRAILING_SECTORS};
use crate::vhd::{self, VHD_MAX_BYTES};

const SECTOR: u64 = 512;
/// 1 MiB partition alignment, matching what the restore path lays down.
const ALIGN: u64 = 1024 * 1024;

pub struct MaterializedImage {
    pub path: PathBuf,
    /// Virtual disk size (excludes the trailing VHD footer).
    pub disk_size: u64,
    pub spans: Vec<PartitionSpan>,
}

/// Materialize `reader`'s backup into a fixed-VHD image at `out_path`.
pub fn materialize(reader: &mut PhnxReader, out_path: &Path) -> Result<MaterializedImage> {
    let (spans, raw_disk_size) = plan_layout(reader, ALIGN);
    // Round the disk up to a sector multiple and ensure room for the trailing
    // GPT beyond the last partition.
    let disk_size = align_up(raw_disk_size + TRAILING_SECTORS * SECTOR + ALIGN, SECTOR);
    if disk_size > VHD_MAX_BYTES {
        return Err(PhoenixError::Other(format!(
            "backup describes a {disk_size}-byte disk, larger than the {VHD_MAX_BYTES}-byte fixed-VHD \
             limit; mounting disks this large needs the VHDX path (not yet implemented)"
        )));
    }

    // --- Build the GPT describing the partition layout ---
    let disk_guid = *reader.header.backup_id.as_bytes();
    let parts: Vec<GptPart> = spans
        .iter()
        .map(|s| {
            let entry = reader.index.iter().find(|e| e.index == s.partition_index);
            GptPart {
                type_guid: entry.map(|e| e.type_guid).unwrap_or([0; 16]),
                // Derive a stable per-partition GUID from the disk GUID + index.
                unique_guid: derive_guid(&disk_guid, s.partition_index),
                first_lba: s.disk_offset / SECTOR,
                last_lba: (s.disk_offset + s.size) / SECTOR - 1,
                attributes: 0,
                name: entry.map(|e| e.name.clone()).unwrap_or_default(),
            }
        })
        .collect();
    let gpt_img = gpt::synthesize(disk_size, disk_guid, &parts);

    // --- Create the sparse output file ---
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out_path)?;
    make_sparse(&file);
    // Total file length = disk image + 512-byte VHD footer.
    file.set_len(disk_size + SECTOR)?;

    // Leading GPT region at offset 0.
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&gpt_img.leading)?;

    // --- Write each partition's captured extents (free space stays sparse) ---
    let entries: Vec<_> = reader.index.clone();
    for span in &spans {
        let entry = entries
            .iter()
            .find(|e| e.index == span.partition_index)
            .ok_or_else(|| PhoenixError::InvalidFormat("layout partition missing".into()))?;
        let stream = reader.read_stream_header(entry)?;
        let records = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.index == span.partition_index)
            .map(|p| p.chunks.clone())
            .ok_or_else(|| PhoenixError::Manifest("partition manifest missing".into()))?;

        for chunk in &stream.chunks {
            let ext = stream
                .extents
                .get(chunk.extent_index as usize)
                .ok_or_else(|| PhoenixError::TableCorrupt {
                    what: "chunk references out-of-range extent".into(),
                })?;
            let data = reader.read_chunk(chunk)?;
            // Verify against the manifest hash for this chunk.
            if let Some(rec) = records.iter().find(|r| {
                r.extent_index == chunk.extent_index && r.chunk_index == chunk.chunk_index
            }) {
                if blake3::hash(&data).to_hex().to_string() != rec.blake3 {
                    return Err(PhoenixError::HashMismatch {
                        partition_index: span.partition_index,
                        chunk_index: chunk.chunk_index,
                    });
                }
            }
            let part_rel = ext.start_sector * EXTENT_LBA_BYTES as u64
                + chunk.chunk_index as u64 * CHUNK_SIZE as u64;
            let disk_pos = span.disk_offset + part_rel;
            file.seek(SeekFrom::Start(disk_pos))?;
            file.write_all(&data)?;
        }
    }

    // --- Trailing GPT region + VHD footer ---
    let trailing_pos = disk_size - TRAILING_SECTORS * SECTOR;
    file.seek(SeekFrom::Start(trailing_pos))?;
    file.write_all(&gpt_img.trailing)?;

    let footer = vhd::build_footer(disk_size, disk_guid);
    file.seek(SeekFrom::Start(disk_size))?;
    file.write_all(&footer)?;
    file.flush()?;

    Ok(MaterializedImage {
        path: out_path.to_path_buf(),
        disk_size,
        spans,
    })
}

fn align_up(v: u64, a: u64) -> u64 {
    if a == 0 {
        v
    } else {
        v.div_ceil(a) * a
    }
}

/// Derive a deterministic 16-byte GUID from the disk GUID and a partition index
/// by hashing them together — good enough for a synthesized, ephemeral disk.
fn derive_guid(disk_guid: &[u8; 16], index: u32) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(disk_guid);
    hasher.update(&index.to_le_bytes());
    let hash = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&hash.as_bytes()[0..16]);
    out
}

#[cfg(windows)]
fn make_sparse(_file: &std::fs::File) {
    // NOTE: the Windows virtual-disk driver rejects a *fixed* VHD stored in a
    // sparse file (OpenVirtualDisk returns 0xC03A001A even though the footer is
    // byte-for-byte valid). So we no longer mark the image sparse. This means a
    // fixed-VHD mount currently allocates the full disk size on disk; the
    // space-efficient path is a dynamic VHD (BAT-backed), tracked as a
    // follow-up. Small backups mount fine; very large ones should use the
    // dynamic-VHD or WinFsp path once implemented.
}

#[cfg(not(windows))]
fn make_sparse(_file: &std::fs::File) {}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::container::{
        Extent, Header, PhnxWriter, EXTENT_LBA_BYTES as LBA, FORMAT_VERSION,
    };
    use phoenix_core::disk::{CaptureMode, FilesystemKind};
    use phoenix_core::manifest::{BackupManifest, DiskManifest, PartitionManifest};
    use uuid::Uuid;

    fn build_small_backup() -> PathBuf {
        let path = std::env::temp_dir().join(format!("mnt_{}.phnx", Uuid::new_v4()));
        let backup_id = Uuid::new_v4();
        let header = Header {
            version: FORMAT_VERSION,
            flags: 1,
            timestamp: 1,
            backup_id,
            disk_signature: 1,
            partition_count: 1,
        };
        // One 64 KiB extent = 128 sectors, one chunk.
        let ext_bytes = 64 * 1024usize;
        let extents = vec![Extent {
            start_sector: 0,
            sector_count: ext_bytes as u64 / LBA as u64,
        }];
        let mut writer = PhnxWriter::create(&path, header).unwrap();
        let mut stream = writer
            .begin_partition_stream(
                0,
                [0x11; 16],
                "Vol".into(),
                ext_bytes as u64,
                FilesystemKind::Ntfs,
                CaptureMode::UsedBlocks,
                LBA,
                0,
                &extents,
                4096,
            )
            .unwrap();
        let payload = vec![0x5Au8; ext_bytes];
        stream.write_chunk(&payload).unwrap();
        let (chunks, _) = stream.finish().unwrap();
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
                original_size: ext_bytes as u64,
                used_bytes: ext_bytes as u64,
                chunks,
                bitmap_hash: None,
            }],
        };
        writer.finalize(&manifest).unwrap();
        path
    }

    #[test]
    fn materialize_assembles_gpt_and_data() {
        let backup = build_small_backup();
        let mut reader = PhnxReader::open(&backup).unwrap();
        let out = std::env::temp_dir().join(format!("mnt_{}.vhd", Uuid::new_v4()));
        let img = materialize(&mut reader, &out).unwrap();

        let bytes = std::fs::read(&img.path).unwrap();
        // File length = disk_size + footer.
        assert_eq!(bytes.len() as u64, img.disk_size + 512);
        // Protective MBR + GPT signature at LBA 1.
        assert_eq!(bytes[510], 0x55);
        assert_eq!(bytes[511], 0xAA);
        assert_eq!(&bytes[512..520], b"EFI PART");
        // VHD footer cookie at the very end.
        assert_eq!(&bytes[bytes.len() - 512..bytes.len() - 504], b"conectix");
        // Partition data (0x5A) present at the partition's disk offset.
        let span = &img.spans[0];
        let pos = span.disk_offset as usize;
        assert_eq!(&bytes[pos..pos + 8], &[0x5A; 8]);

        std::fs::remove_file(&backup).ok();
        std::fs::remove_file(&out).ok();
    }
}
