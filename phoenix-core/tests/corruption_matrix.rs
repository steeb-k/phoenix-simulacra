//! The bulletproof-verification matrix: build a valid multi-partition,
//! multi-chunk `.phnx`, then corrupt it in every structurally-distinct way and
//! assert the reader/verifier catches each with the *specific* error it should.
//! Pure file I/O — no admin, no disks — so it runs everywhere.

use phoenix_core::container::{
    Extent, Header, PhnxReader, PhnxWriter, EXTENT_LBA_BYTES, FORMAT_VERSION,
};
use phoenix_core::disk::{CaptureMode, FilesystemKind};
use phoenix_core::error::PhoenixError;
use phoenix_core::manifest::{BackupManifest, DiskManifest, PartitionManifest};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use uuid::Uuid;

const CHUNK: usize = 4 * 1024 * 1024;

/// Build a valid backup with one partition that has two extents; the first
/// extent holds two full 4 MiB chunks (so we exercise multi-chunk streams).
fn build_backup() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("corrupt_{}.phnx", Uuid::new_v4()));
    let backup_id = Uuid::new_v4();
    let header = Header {
        version: FORMAT_VERSION,
        flags: 0,
        timestamp: 1,
        backup_id,
        disk_signature: 1,
        partition_count: 1,
    };
    // Extent 0: 2 chunks * 4 MiB = 8 MiB -> 16384 sectors.
    // Extent 1: 1 chunk of 1 MiB -> 2048 sectors.
    let ext0_bytes = 2 * CHUNK;
    let ext1_bytes = 1024 * 1024;
    let extents = vec![
        Extent {
            start_sector: 0,
            sector_count: (ext0_bytes as u64) / EXTENT_LBA_BYTES as u64,
        },
        Extent {
            start_sector: 100_000,
            sector_count: (ext1_bytes as u64) / EXTENT_LBA_BYTES as u64,
        },
    ];

    let mut writer = PhnxWriter::create(&path, header).unwrap();
    let mut stream = writer
        .begin_partition_stream(
            0,
            [0; 16],
            "P".into(),
            (ext0_bytes + ext1_bytes) as u64,
            FilesystemKind::Ntfs,
            CaptureMode::UsedBlocks,
            EXTENT_LBA_BYTES,
            0,
            &extents,
            4096,
        )
        .unwrap();
    stream.set_extent(0);
    stream.write_chunk(&vec![0x11u8; CHUNK]).unwrap();
    stream.write_chunk(&vec![0x22u8; CHUNK]).unwrap();
    stream.set_extent(1);
    stream.write_chunk(&vec![0x33u8; ext1_bytes]).unwrap();
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
            name: "P".into(),
            type_guid: None,
            fs: "ntfs".into(),
            capture_mode: "used-blocks".into(),
            original_size: (ext0_bytes + ext1_bytes) as u64,
            used_bytes: (ext0_bytes + ext1_bytes) as u64,
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

fn flip_byte(path: &std::path::Path, offset: u64) {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    b[0] ^= 0xFF;
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&b).unwrap();
}

fn truncate_to(path: &std::path::Path, len: u64) {
    let f = OpenOptions::new().write(true).open(path).unwrap();
    f.set_len(len).unwrap();
}

fn append_garbage(path: &std::path::Path, n: usize) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(&vec![0xABu8; n]).unwrap();
}

fn file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// The good file opens and fully verifies.
#[test]
fn good_backup_verifies() {
    let path = build_backup();
    let mut r = PhnxReader::open(&path).unwrap();
    r.verify_all(false).unwrap();
    r.verify_all(true).unwrap(); // quick also clean
    let report = r.verify_structure().unwrap();
    assert_eq!(report.partitions, 1);
    assert_eq!(report.total_chunks, 3);
    std::fs::remove_file(&path).ok();
}

/// Flipping a byte inside chunk payload is caught by a full verify.
#[test]
fn corrupt_chunk_payload_caught_by_full_verify() {
    let path = build_backup();
    // First chunk's data starts right after the stream map header. Locate it.
    let (off, _len) = {
        let mut r = PhnxReader::open(&path).unwrap();
        let entry = r.index[0].clone();
        let sh = r.read_stream_header(&entry).unwrap();
        (sh.chunks[0].file_offset, sh.chunks[0].compressed_len)
    };
    flip_byte(&path, off + 5);
    let mut r = PhnxReader::open(&path).unwrap();
    let err = r.verify_all(false).unwrap_err();
    // A flipped compressed byte fails either the zstd decode or the BLAKE3.
    assert!(
        matches!(
            err,
            PhoenixError::HashMismatch { .. } | PhoenixError::Compression(_)
        ),
        "got {err:?}"
    );
    std::fs::remove_file(&path).ok();
}

/// Corrupting an extent's sector_count breaks the per-extent coverage check.
#[test]
fn corrupt_extent_table_caught_by_structure() {
    let path = build_backup();
    let stream_offset = {
        let r = PhnxReader::open(&path).unwrap();
        r.index[0].stream_offset
    };
    // Extent 0 sector_count is at stream_offset + 12 (map header) + 8.
    flip_byte(&path, stream_offset + 12 + 8);
    let mut r = PhnxReader::open(&path).unwrap();
    let err = r.verify_structure().unwrap_err();
    assert!(
        matches!(err, PhoenixError::TableCorrupt { .. }),
        "got {err:?}"
    );
    std::fs::remove_file(&path).ok();
}

/// Corrupting a partition index entry is caught at open() by the index hash.
#[test]
fn corrupt_index_entry_caught_at_open() {
    let path = build_backup();
    // Index region starts at HEADER_SIZE (64); flip a byte in the first entry.
    flip_byte(&path, 64 + 112); // original_size field
    let err = PhnxReader::open(&path).err().expect("open should fail");
    assert!(
        matches!(err, PhoenixError::TableCorrupt { .. }),
        "got {err:?}"
    );
    std::fs::remove_file(&path).ok();
}

/// Truncating the file (dropping bytes) is caught at open() by total length.
#[test]
fn truncation_caught_at_open() {
    let path = build_backup();
    let len = file_len(&path);
    truncate_to(&path, len - 4096); // drop a chunk boundary's worth
    let err = PhnxReader::open(&path).err().expect("open should fail");
    // Either the footer no longer parses, or (if footer survived) the length
    // check fires. Both are acceptable "damaged file" outcomes.
    assert!(
        matches!(
            err,
            PhoenixError::Truncated { .. }
                | PhoenixError::TableCorrupt { .. }
                | PhoenixError::InvalidFormat(_)
                | PhoenixError::Io(_)
        ),
        "got {err:?}"
    );
    std::fs::remove_file(&path).ok();
}

/// Appending trailing garbage changes the real length; caught at open().
#[test]
fn trailing_garbage_caught_at_open() {
    let path = build_backup();
    append_garbage(&path, 512);
    let err = PhnxReader::open(&path).err().expect("open should fail");
    assert!(
        matches!(
            err,
            PhoenixError::Truncated { .. }
                | PhoenixError::TableCorrupt { .. }
                | PhoenixError::InvalidFormat(_)
        ),
        "got {err:?}"
    );
    std::fs::remove_file(&path).ok();
}

/// Corrupting a footer byte is caught by the footer CRC at open().
#[test]
fn corrupt_footer_caught_at_open() {
    let path = build_backup();
    let len = file_len(&path);
    // Flip a byte inside the footer's manifest_offset (first 8 bytes of the
    // 112-byte footer), which is covered by the footer CRC.
    flip_byte(&path, len - 112 + 2);
    let err = PhnxReader::open(&path).err().expect("open should fail");
    assert!(
        matches!(err, PhoenixError::TableCorrupt { .. }),
        "got {err:?}"
    );
    std::fs::remove_file(&path).ok();
}

/// A backup whose stream chunk_count field is zeroed disagrees with the
/// manifest; caught by the structure/pairing check.
#[test]
fn chunk_count_mismatch_caught() {
    let path = build_backup();
    let stream_offset = {
        let r = PhnxReader::open(&path).unwrap();
        r.index[0].stream_offset
    };
    // chunk_count is the 2nd u32 of the map header (offset stream_offset + 4).
    // Zero it so the stream reports 0 chunks while the manifest lists 3.
    {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.seek(SeekFrom::Start(stream_offset + 4)).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
    }
    let mut r = PhnxReader::open(&path).unwrap();
    let err = r.verify_structure().unwrap_err();
    assert!(
        matches!(
            err,
            PhoenixError::ChunkCountMismatch { .. } | PhoenixError::TableCorrupt { .. }
        ),
        "got {err:?}"
    );
    std::fs::remove_file(&path).ok();
}
