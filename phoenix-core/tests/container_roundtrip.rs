use phoenix_core::container::{
    Extent, Header, PartitionStreamSpec, PhnxReader, PhnxWriter, FORMAT_VERSION,
};
use phoenix_core::disk::{CaptureMode, FilesystemKind};
use phoenix_core::manifest::{BackupManifest, ChunkRecord, DiskManifest, PartitionManifest};
use std::io::{Seek, SeekFrom};
use tempfile::NamedTempFile;
use uuid::Uuid;

#[test]
fn phnx_header_roundtrip_file() {
    let mut tmp = NamedTempFile::new().unwrap();
    let header = Header {
        version: FORMAT_VERSION,
        flags: 1,
        timestamp: 1,
        backup_id: Uuid::new_v4(),
        disk_signature: 99,
        partition_count: 0,
    };
    header.write(&mut tmp).unwrap();
    tmp.seek(SeekFrom::Start(0)).unwrap();
    let h2 = Header::read(&mut tmp).unwrap();
    assert_eq!(h2.backup_id, header.backup_id);
}

#[test]
fn phnx_write_read_manifest() {
    let path = std::env::temp_dir().join(format!("test_{}.phnx", Uuid::new_v4()));
    let backup_id = Uuid::new_v4();
    let header = Header {
        version: FORMAT_VERSION,
        flags: 1,
        timestamp: 1_700_000_000,
        backup_id,
        disk_signature: 1,
        partition_count: 0,
    };
    let mut writer = PhnxWriter::create(&path, header).unwrap();
    let extents = vec![Extent {
        start_sector: 0,
        sector_count: 8,
    }];
    let mut stream = writer
        .begin_partition_stream(PartitionStreamSpec {
            index: 0,
            type_guid: [0; 16],
            name: "Test".into(),
            original_size: 4096,
            fs_kind: FilesystemKind::Unknown,
            capture_mode: CaptureMode::Raw,
            sector_size: 512,
            used_bytes: 0,
            extents: &extents,
            bytes_per_cluster: 0,
        })
        .unwrap();
    // The single extent covers 8 sectors = 4096 bytes, so the captured chunk
    // must be exactly 4096 bytes for the structure check's per-extent coverage
    // to balance.
    let payload = vec![0x5Au8; 4096];
    stream.write_chunk(&payload).unwrap();
    let (chunks, _) = stream.finish().unwrap();
    assert_eq!(chunks.len(), 1);

    let manifest = BackupManifest {
        format_version: 1,
        backup_id,
        parent_backup_id: None,
        hostname: "TEST".into(),
        disk: DiskManifest {
            style: "gpt".into(),
            disk_guid: None,
            disk_signature: None,
            mbr_boot_code: None,
            sector_size: 512,
        },
        partitions: vec![PartitionManifest {
            index: 0,
            name: "Test".into(),
            type_guid: None,
            fs: "unknown".into(),
            capture_mode: "raw".into(),
            original_size: 4096,
            used_bytes: 4096,
            bitlocker: None,
            unique_guid: None,
            gpt_attributes: None,
            mbr_type: None,
            mbr_bootable: None,
            chunks: vec![ChunkRecord {
                chunk_index: 0,
                extent_index: 0,
                uncompressed_len: 4096,
                blake3: phoenix_core::hash::hash_hex(&payload),
            }],
            bitmap_hash: None,
        }],
    };
    writer.finalize(&manifest).unwrap();

    let mut reader = PhnxReader::open(&path).unwrap();
    reader.verify_all(false).unwrap();
    let _ = std::fs::remove_file(path);
}
