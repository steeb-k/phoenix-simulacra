use phoenix_core::container::{Extent, Header, PhnxReader, PhnxWriter, FORMAT_VERSION};
use phoenix_core::disk::{CaptureMode, FilesystemKind};
use phoenix_core::manifest::{
    BackupManifest, ChunkRecord, DiskManifest, PartitionManifest,
};
use std::io::{Read, Seek, SeekFrom, Write};
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
        .begin_partition_stream(
            0,
            [0; 16],
            "Test".into(),
            4096,
            FilesystemKind::Unknown,
            CaptureMode::Raw,
            512,
            0,
            &extents,
            0,
        )
        .unwrap();
    stream.write_chunk(b"hello backup world!!!").unwrap();
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
            sector_size: 512,
        },
        partitions: vec![PartitionManifest {
            index: 0,
            name: "Test".into(),
            type_guid: None,
            fs: "unknown".into(),
            capture_mode: "raw".into(),
            original_size: 4096,
            used_bytes: 23,
            chunks: vec![ChunkRecord {
                chunk_index: 0,
                extent_index: 0,
                uncompressed_len: 23,
                blake3: phoenix_core::hash::hash_hex(b"hello backup world!!!"),
            }],
            bitmap_hash: None,
        }],
    };
    writer.finalize(&manifest).unwrap();

    let mut reader = PhnxReader::open(&path).unwrap();
    reader.verify_all(false).unwrap();
    let _ = std::fs::remove_file(path);
}
