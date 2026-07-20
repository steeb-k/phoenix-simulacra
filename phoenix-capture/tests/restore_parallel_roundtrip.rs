//! Round-trip the parallel restore path WITHOUT admin or a real disk:
//! capture synthetic multi-extent data into a `.phnx` through the pipelined
//! writer, then `restore_raw` it — parallel decode included — into a
//! temp-file-backed `PartitionWriter` (Win32 `CreateFileW` works the same on
//! a regular file), and byte-compare the result against a reference image.
//!
//! The target file is pre-filled with a sentinel byte so the comparison also
//! proves restore writes ONLY inside the extents (no stray writes below the
//! partition base offset, between extents, or past the extent ends).

use phoenix_capture::raw::{restore_raw, PartitionWriter, RestoreOpts};
use phoenix_core::container::{
    Extent, Header, PartitionStreamSpec, PhnxReader, PhnxWriter, CHUNK_SIZE, EXTENT_LBA_BYTES,
    FORMAT_VERSION,
};
use phoenix_core::disk::{CaptureMode, FilesystemKind};
use phoenix_core::manifest::{BackupManifest, DiskManifest, PartitionManifest};
use uuid::Uuid;

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Chunk plan mirroring real capture: full CHUNK_SIZE chunks then a shorter
/// tail per extent (restore computes offsets as `chunk_index * CHUNK_SIZE`,
/// so mid-extent chunks must be full-size, exactly like `capture_ntfs`
/// produces them).
fn extent_chunks(full: usize, tail: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut state = seed;
    let mut chunks = Vec::new();
    for i in 0..full {
        let mut c = vec![(i as u8).wrapping_mul(37); CHUNK_SIZE];
        // Make half of every chunk incompressible so zstd output sizes vary.
        for b in c[..CHUNK_SIZE / 2].chunks_exact_mut(8) {
            b.copy_from_slice(&splitmix(&mut state).to_le_bytes());
        }
        chunks.push(c);
    }
    if tail > 0 {
        let mut t = vec![0x11u8; tail];
        for b in t.chunks_exact_mut(8) {
            b.copy_from_slice(&splitmix(&mut state).to_le_bytes());
        }
        chunks.push(t);
    }
    chunks
}

#[test]
fn restore_raw_parallel_roundtrips_and_stays_in_bounds() {
    let dir = std::env::temp_dir();
    let phnx_path = dir.join(format!("restore-par-{}.phnx", Uuid::new_v4().simple()));
    let target_path = dir.join(format!("restore-par-{}.img", Uuid::new_v4().simple()));

    // Extent 0 at sector 0: 2 full chunks + 1 MiB tail = 9 MiB.
    // Extent 1 at 16 MiB: 1 full chunk + 512 B tail.
    let e0 = extent_chunks(2, 1024 * 1024, 5);
    let e1 = extent_chunks(1, 512, 6);
    let e0_bytes: u64 = e0.iter().map(|c| c.len() as u64).sum();
    let e1_bytes: u64 = e1.iter().map(|c| c.len() as u64).sum();
    let e1_start_sector = (16 * 1024 * 1024) / EXTENT_LBA_BYTES as u64;
    let extents = vec![
        Extent {
            start_sector: 0,
            sector_count: e0_bytes / EXTENT_LBA_BYTES as u64,
        },
        Extent {
            start_sector: e1_start_sector,
            sector_count: e1_bytes / EXTENT_LBA_BYTES as u64,
        },
    ];
    let part_size: u64 = 24 * 1024 * 1024;

    // ---- Capture ----
    let backup_id = Uuid::new_v4();
    let header = Header {
        version: FORMAT_VERSION,
        flags: 0,
        timestamp: 1,
        backup_id,
        disk_signature: 3,
        partition_count: 1,
    };
    let mut writer = PhnxWriter::create(&phnx_path, header).unwrap();
    let mut stream = writer
        .begin_partition_stream(PartitionStreamSpec {
            index: 0,
            type_guid: [0u8; 16],
            name: "R".into(),
            original_size: part_size,
            fs_kind: FilesystemKind::Ntfs,
            capture_mode: CaptureMode::UsedBlocks,
            sector_size: EXTENT_LBA_BYTES,
            used_bytes: 0,
            extents: &extents,
            bytes_per_cluster: 4096,
        })
        .unwrap();
    stream.set_extent(0);
    for c in &e0 {
        stream.write_chunk(c).unwrap();
    }
    stream.set_extent(1);
    for c in &e1 {
        stream.write_chunk(c).unwrap();
    }
    let (records, _) = stream.finish().unwrap();
    let manifest = BackupManifest {
        format_version: 1,
        backup_id,
        parent_backup_id: None,
        hostname: "T".into(),
        disk: DiskManifest {
            style: "gpt".into(),
            disk_guid: None,
            disk_signature: None,
            mbr_boot_code: None,
            sector_size: 512,
        },
        partitions: vec![PartitionManifest {
            index: 0,
            name: "R".into(),
            type_guid: None,
            fs: "ntfs".into(),
            capture_mode: "used-blocks".into(),
            original_size: part_size,
            used_bytes: e0_bytes + e1_bytes,
            bitlocker: None,
            unique_guid: None,
            gpt_attributes: None,
            mbr_type: None,
            mbr_bootable: None,
            chunks: records,
            bitmap_hash: None,
        }],
    };
    writer.finalize(&manifest).unwrap();

    // ---- Restore into a sentinel-filled regular file ----
    let base_offset: u64 = 1024 * 1024; // pretend the partition starts at 1 MiB
    let file_len = base_offset + part_size;
    std::fs::write(&target_path, vec![0x5Au8; file_len as usize]).unwrap();

    let mut reader = PhnxReader::open(&phnx_path).unwrap();
    let entry = reader.index[0].clone();
    let mut pw =
        PartitionWriter::open_disk(target_path.to_str().unwrap(), base_offset, 512).unwrap();
    let bytes = restore_raw(
        &mut reader,
        &entry,
        &mut pw,
        RestoreOpts {
            verify: true, // verify chunk hashes on the parallel decode path
            progress: None,
            bytes_done: 0,
        },
        None,
        part_size,
    )
    .unwrap();
    assert_eq!(bytes, e0_bytes + e1_bytes, "bytes restored");
    drop(pw);

    // ---- Reference image: sentinel everywhere except extent data ----
    let mut expected = vec![0x5Au8; file_len as usize];
    let mut off = base_offset as usize; // extent 0 starts at sector 0
    for c in &e0 {
        expected[off..off + c.len()].copy_from_slice(c);
        off += c.len();
    }
    let mut off = (base_offset + e1_start_sector * EXTENT_LBA_BYTES as u64) as usize;
    for c in &e1 {
        expected[off..off + c.len()].copy_from_slice(c);
        off += c.len();
    }

    let actual = std::fs::read(&target_path).unwrap();
    assert_eq!(actual.len(), expected.len());
    // Compare in blocks so a failure prints a locatable offset, not 25 MB.
    for (i, (a, e)) in actual.chunks(4096).zip(expected.chunks(4096)).enumerate() {
        assert_eq!(a, e, "restored image differs in 4 KiB block {i}");
    }

    // ---- Corruption in the .phnx must fail a verifying restore ----
    let ci = {
        let sh = reader.read_stream_header(&entry).unwrap();
        sh.chunks[1].clone()
    };
    drop(reader);
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&phnx_path)
            .unwrap();
        // Flip a byte in the middle of chunk 1's compressed payload.
        f.seek(SeekFrom::Start(
            ci.file_offset + ci.compressed_len as u64 / 2,
        ))
        .unwrap();
        let mut b = [0u8; 1];
        use std::io::Read;
        f.read_exact(&mut b).unwrap();
        f.seek(SeekFrom::Start(
            ci.file_offset + ci.compressed_len as u64 / 2,
        ))
        .unwrap();
        f.write_all(&[b[0] ^ 0xFF]).unwrap();
    }
    let mut reader = PhnxReader::open(&phnx_path).unwrap();
    let mut pw =
        PartitionWriter::open_disk(target_path.to_str().unwrap(), base_offset, 512).unwrap();
    let err = restore_raw(
        &mut reader,
        &entry,
        &mut pw,
        RestoreOpts {
            verify: true,
            progress: None,
            bytes_done: 0,
        },
        None,
        part_size,
    );
    assert!(
        err.is_err(),
        "verifying restore must fail on a corrupted chunk"
    );
    drop(pw);
    drop(reader);

    let _ = std::fs::remove_file(&phnx_path);
    let _ = std::fs::remove_file(&target_path);
}
