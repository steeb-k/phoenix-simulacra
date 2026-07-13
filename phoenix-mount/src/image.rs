//! Materialize a `.phnx` backup into a fixed-VHD disk image file (a STOPGAP
//! mount path — see the space constraint below).
//!
//! The bytes come from [`SyntheticVhd`], the same on-demand provider the WinFsp
//! mount uses, so the two paths can never diverge. Only non-zero blocks are
//! written to the pre-zeroed file, so the write time tracks the backup's used
//! size even though the file is fully allocated.
//!
//! STOPGAP / hard constraint: the Windows virtual-disk driver rejects a *fixed*
//! VHD stored in a sparse file (`OpenVirtualDisk` → `0xC03A001A`, even though
//! the footer is byte-for-byte valid), so this file is fully allocated and thus
//! consumes ~the full disk size. That violates the rule that mounting must
//! NEVER double a backup's footprint. The shipping path is the WinFsp on-demand
//! mount (zero materialization); this file exists only until that lands.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use phoenix_core::container::PhnxReader;
use phoenix_core::error::Result;

use crate::chunkstore::PartitionSpan;
use crate::synthetic::SyntheticVhd;

/// Block size for streaming the synthesized image to disk.
const WRITE_BLOCK: usize = 4 * 1024 * 1024;

pub struct MaterializedImage {
    pub path: PathBuf,
    /// Virtual disk size (excludes the trailing VHD footer).
    pub disk_size: u64,
    pub spans: Vec<PartitionSpan>,
}

/// Materialize `reader`'s backup into a fixed-VHD image at `out_path`.
pub fn materialize(reader: PhnxReader, out_path: &Path) -> Result<MaterializedImage> {
    tracing::warn!(
        "mount is materializing a full-size temp VHD (stopgap); the space-efficient WinFsp \
         on-demand mount is the planned replacement and must land before this ships"
    );

    let mut vhd = SyntheticVhd::build(reader)?;
    let total = vhd.total_len();
    let disk_size = vhd.disk_size();
    let spans = vhd.spans().to_vec();

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out_path)?;
    // Fully allocate (non-sparse): the virtual-disk driver rejects sparse fixed
    // VHDs. Unwritten regions read back as zeros, so we only write non-zero
    // blocks below.
    file.set_len(total)?;

    let mut buf = vec![0u8; WRITE_BLOCK];
    let mut pos = 0u64;
    while pos < total {
        let n = WRITE_BLOCK.min((total - pos) as usize);
        let block = &mut buf[..n];
        vhd.read_at(pos, block)?;
        // Skip all-zero blocks — set_len already zeroed the file, so writing
        // them would only slow the stopgap down over free space.
        if block.iter().any(|&b| b != 0) {
            file.seek(SeekFrom::Start(pos))?;
            file.write_all(block)?;
        }
        pos += n as u64;
    }
    file.flush()?;

    Ok(MaterializedImage {
        path: out_path.to_path_buf(),
        disk_size,
        spans,
    })
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
            .begin_partition_stream(PartitionStreamSpec {
                index: 0,
                type_guid: [0x11; 16],
                name: "Vol".into(),
                original_size: ext_bytes as u64,
                fs_kind: FilesystemKind::Ntfs,
                capture_mode: CaptureMode::UsedBlocks,
                sector_size: LBA,
                used_bytes: 0,
                extents: &extents,
                bytes_per_cluster: 4096,
            })
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
                bitlocker: None,
                unique_guid: None,
                gpt_attributes: None,
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
        let reader = PhnxReader::open(&backup).unwrap();
        let out = std::env::temp_dir().join(format!("mnt_{}.vhd", Uuid::new_v4()));
        let img = materialize(reader, &out).unwrap();

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
