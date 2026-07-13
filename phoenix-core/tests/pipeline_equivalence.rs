//! The parallel encode pipeline must be a drop-in replacement for the old
//! serial `write_chunk` loop: same chunk order, same file offsets, same
//! compressed bytes, same manifest records — byte-identical layout. These
//! tests compute the expected layout with a serial reference implementation
//! (the exact algorithm the old loop used) and compare everything the
//! pipelined writer produced against it, then round-trip through `PhnxReader`
//! with a full verify.

use phoenix_core::container::{
    compress_chunk, Extent, Header, PartitionStreamSpec, PhnxReader, PhnxWriter, CHUNK_SIZE,
    EXTENT_LBA_BYTES, FORMAT_VERSION, HEADER_SIZE, INDEX_TABLE_RESERVE,
};
use phoenix_core::disk::{CaptureMode, FilesystemKind};
use phoenix_core::hash;
use phoenix_core::manifest::{BackupManifest, ChunkRecord, DiskManifest, PartitionManifest};
use phoenix_core::{PhoenixError, ProgressHandle};
use uuid::Uuid;

/// SplitMix64 so "random" chunk content is deterministic across runs.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Build a per-extent chunk plan with content that exercises the pipeline:
/// highly compressible runs, incompressible pseudo-random runs, and odd
/// (sector-multiple) lengths so no two chunks compress to the same size.
fn build_chunks(count: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut state = seed;
    (0..count)
        .map(|i| {
            let len = 512 * (1 + (splitmix(&mut state) % 96) as usize); // 512B..48KiB
            match i % 3 {
                0 => vec![(i % 251) as u8; len], // compressible
                1 => {
                    // incompressible: raw PRNG bytes
                    let mut v = Vec::with_capacity(len);
                    while v.len() < len {
                        v.extend_from_slice(&splitmix(&mut state).to_le_bytes());
                    }
                    v.truncate(len);
                    v
                }
                _ => {
                    // half-and-half
                    let mut v = vec![0xCDu8; len];
                    for b in v[..len / 2].chunks_exact_mut(8) {
                        b.copy_from_slice(&splitmix(&mut state).to_le_bytes());
                    }
                    v
                }
            }
        })
        .collect()
}

fn extent_for(chunks: &[Vec<u8>], start_sector: u64) -> Extent {
    let bytes: u64 = chunks.iter().map(|c| c.len() as u64).sum();
    assert_eq!(bytes % EXTENT_LBA_BYTES as u64, 0);
    Extent {
        start_sector,
        sector_count: bytes / EXTENT_LBA_BYTES as u64,
    }
}

fn manifest_for(backup_id: Uuid, parts: Vec<PartitionManifest>) -> BackupManifest {
    BackupManifest {
        format_version: 1,
        backup_id,
        parent_backup_id: None,
        hostname: "PIPE-TEST".into(),
        disk: DiskManifest {
            style: "gpt".into(),
            disk_guid: None,
            sector_size: 512,
        },
        partitions: parts,
    }
}

fn part_manifest(index: u32, chunks: Vec<ChunkRecord>, size: u64) -> PartitionManifest {
    PartitionManifest {
        index,
        name: format!("P{index}"),
        type_guid: None,
        fs: "ntfs".into(),
        capture_mode: "used-blocks".into(),
        original_size: size,
        used_bytes: size,
        bitlocker: None,
        unique_guid: None,
        gpt_attributes: None,
        chunks,
        bitmap_hash: None,
    }
}

/// The load-bearing test: submit ~120 mixed chunks across two extents and two
/// partitions, then check every byte and every table entry against a serial
/// reference computed with the same primitives the old loop used.
#[test]
fn parallel_writer_matches_serial_reference() {
    let path = std::env::temp_dir().join(format!("pipe-eq-{}.phnx", Uuid::new_v4().simple()));
    let header = Header {
        version: FORMAT_VERSION,
        flags: 1,
        timestamp: 1_700_000_000,
        backup_id: Uuid::new_v4(),
        disk_signature: 99,
        partition_count: 0,
    };

    // Partition 0: two extents, 80 + 30 chunks. Partition 1: one extent,
    // 10 chunks — proves the offset handoff between streams stays exact.
    let p0e0 = build_chunks(80, 1);
    let p0e1 = build_chunks(30, 2);
    let p1e0 = build_chunks(10, 3);
    let p0_extents = vec![extent_for(&p0e0, 0), extent_for(&p0e1, 1 << 20)];
    let p1_extents = vec![extent_for(&p1e0, 0)];
    let p0_size: u64 = (1 << 21) * EXTENT_LBA_BYTES as u64;
    let p1_size: u64 = p1_extents[0].sector_count * EXTENT_LBA_BYTES as u64;

    let mut writer = PhnxWriter::create(&path, header.clone()).unwrap();

    let mut stream = writer
        .begin_partition_stream(PartitionStreamSpec {
            index: 0,
            type_guid: [1u8; 16],
            name: "P0".into(),
            original_size: p0_size,
            fs_kind: FilesystemKind::Ntfs,
            capture_mode: CaptureMode::UsedBlocks,
            sector_size: EXTENT_LBA_BYTES,
            used_bytes: 0,
            extents: &p0_extents,
            bytes_per_cluster: 4096,
        })
        .unwrap();
    stream.set_extent(0);
    for c in &p0e0 {
        stream.write_chunk(c).unwrap();
    }
    stream.set_extent(1);
    for c in &p0e1 {
        stream.write_chunk(c).unwrap();
    }
    let (p0_records, _) = stream.finish().unwrap();

    let mut stream = writer
        .begin_partition_stream(PartitionStreamSpec {
            index: 1,
            type_guid: [2u8; 16],
            name: "P1".into(),
            original_size: p1_size,
            fs_kind: FilesystemKind::Ntfs,
            capture_mode: CaptureMode::UsedBlocks,
            sector_size: EXTENT_LBA_BYTES,
            used_bytes: 0,
            extents: &p1_extents,
            bytes_per_cluster: 4096,
        })
        .unwrap();
    stream.set_extent(0);
    for c in &p1e0 {
        stream.write_chunk(c).unwrap();
    }
    let (p1_records, _) = stream.finish().unwrap();

    let manifest = manifest_for(
        header.backup_id,
        vec![
            part_manifest(0, p0_records.clone(), p0_size),
            part_manifest(1, p1_records.clone(), p1_size),
        ],
    );
    writer.finalize(&manifest).unwrap();

    // ---- Serial reference: replay the old algorithm offset by offset ----
    let file_bytes = std::fs::read(&path).unwrap();
    let p0_stream_offset = HEADER_SIZE as u64 + INDEX_TABLE_RESERVE;
    let p0_data_start = p0_stream_offset + 12 + p0_extents.len() as u64 * 16;

    // (extent_index, chunk_in_extent, plaintext) in submission order.
    let p0_plan: Vec<(u32, u32, &Vec<u8>)> = p0e0
        .iter()
        .enumerate()
        .map(|(i, c)| (0u32, i as u32, c))
        .chain(p0e1.iter().enumerate().map(|(i, c)| (1u32, i as u32, c)))
        .collect();

    let mut offset = p0_data_start;
    for (global_idx, (ext, cie, plain)) in p0_plan.iter().enumerate() {
        let expected_compressed = compress_chunk(plain).unwrap();
        let rec = &p0_records[global_idx];
        assert_eq!(rec.chunk_index, global_idx as u32, "record order");
        assert_eq!(rec.extent_index, *ext, "record extent");
        assert_eq!(rec.uncompressed_len, plain.len() as u32);
        assert_eq!(rec.blake3, hash::hash_hex(plain), "record hash");
        assert_eq!(
            &file_bytes[offset as usize..offset as usize + expected_compressed.len()],
            &expected_compressed[..],
            "compressed bytes for chunk {global_idx} at file offset {offset}"
        );
        let _ = cie;
        offset += expected_compressed.len() as u64;
    }

    // ---- Reader view: chunk table matches the same reference walk ----
    let mut reader = PhnxReader::open(&path).unwrap();
    let entry0 = reader.index[0].clone();
    let sh0 = reader.read_stream_header(&entry0).unwrap();
    assert_eq!(sh0.chunks.len(), p0_plan.len());
    let mut offset = p0_data_start;
    for (global_idx, (ext, cie, plain)) in p0_plan.iter().enumerate() {
        let ci = &sh0.chunks[global_idx];
        let expected_compressed = compress_chunk(plain).unwrap();
        assert_eq!(ci.file_offset, offset, "chunk {global_idx} file offset");
        assert_eq!(ci.compressed_len, expected_compressed.len() as u32);
        assert_eq!(ci.uncompressed_len, plain.len() as u32);
        assert_eq!(ci.extent_index, *ext);
        assert_eq!(ci.chunk_index, *cie, "within-extent ordinal");
        offset += expected_compressed.len() as u64;
    }

    // Partition 1 begins exactly where partition 0's chunk table ended.
    let entry1 = reader.index[1].clone();
    assert_eq!(
        entry1.stream_offset,
        entry0.stream_offset + entry0.stream_length,
        "stream handoff between partitions"
    );

    // ---- Full round-trip: verify everything, then byte-compare chunks ----
    reader.verify_all(false).unwrap();
    let sh1 = reader.read_stream_header(&entry1).unwrap();
    for (chunks, sh) in [
        (&[&p0e0[..], &p0e1[..]][..], &sh0),
        (&[&p1e0[..]][..], &sh1),
    ] {
        let all: Vec<&Vec<u8>> = chunks.iter().flat_map(|s| s.iter()).collect();
        for (i, ci) in sh.chunks.clone().iter().enumerate() {
            let data = reader.read_chunk(ci).unwrap();
            assert_eq!(&data, all[i], "round-tripped chunk {i}");
        }
    }

    drop(reader);
    let _ = std::fs::remove_file(&path);
}

/// Cancel must surface as `PhoenixError::Cancelled` from `write_chunk`, and
/// dropping the abandoned stream must not hang (the pipeline joins its
/// threads in Drop).
#[test]
fn cancelled_progress_fails_write_chunk() {
    let path = std::env::temp_dir().join(format!("pipe-cancel-{}.phnx", Uuid::new_v4().simple()));
    let header = Header {
        version: FORMAT_VERSION,
        flags: 0,
        timestamp: 1,
        backup_id: Uuid::new_v4(),
        disk_signature: 1,
        partition_count: 0,
    };
    let progress = ProgressHandle::new();
    let mut writer =
        PhnxWriter::create_with_progress(&path, header, Some(progress.clone())).unwrap();
    let chunks = build_chunks(8, 7);
    let extents = vec![extent_for(&chunks, 0)];
    let mut stream = writer
        .begin_partition_stream(PartitionStreamSpec {
            index: 0,
            type_guid: [0u8; 16],
            name: "C".into(),
            original_size: extents[0].sector_count * EXTENT_LBA_BYTES as u64,
            fs_kind: FilesystemKind::Ntfs,
            capture_mode: CaptureMode::UsedBlocks,
            sector_size: EXTENT_LBA_BYTES,
            used_bytes: 0,
            extents: &extents,
            bytes_per_cluster: 4096,
        })
        .unwrap();
    stream.set_extent(0);
    stream.write_chunk(&chunks[0]).unwrap();
    progress.cancel();
    let err = stream.write_chunk(&chunks[1]).unwrap_err();
    assert!(matches!(err, PhoenixError::Cancelled), "got {err:?}");
    drop(stream); // must join pipeline threads without hanging
    drop(writer);
    let _ = std::fs::remove_file(&path);
}

/// Chunks larger than the format's CHUNK_SIZE ceiling never happen (capture
/// reads are clamped), but a full-CHUNK_SIZE chunk is the common case on real
/// volumes — prove one round-trips exactly.
#[test]
fn full_size_chunks_roundtrip() {
    let path = std::env::temp_dir().join(format!("pipe-full-{}.phnx", Uuid::new_v4().simple()));
    let header = Header {
        version: FORMAT_VERSION,
        flags: 0,
        timestamp: 1,
        backup_id: Uuid::new_v4(),
        disk_signature: 1,
        partition_count: 0,
    };
    let mut state = 11u64;
    let mut chunk = vec![0u8; CHUNK_SIZE];
    for b in chunk.chunks_exact_mut(8) {
        b.copy_from_slice(&splitmix(&mut state).to_le_bytes());
    }
    let chunks = vec![chunk, vec![0xEEu8; CHUNK_SIZE]];
    let extents = vec![extent_for(&chunks, 0)];
    let size = extents[0].sector_count * EXTENT_LBA_BYTES as u64;

    let mut writer = PhnxWriter::create(&path, header.clone()).unwrap();
    let mut stream = writer
        .begin_partition_stream(PartitionStreamSpec {
            index: 0,
            type_guid: [0u8; 16],
            name: "F".into(),
            original_size: size,
            fs_kind: FilesystemKind::Ntfs,
            capture_mode: CaptureMode::UsedBlocks,
            sector_size: EXTENT_LBA_BYTES,
            used_bytes: 0,
            extents: &extents,
            bytes_per_cluster: 4096,
        })
        .unwrap();
    stream.set_extent(0);
    for c in &chunks {
        stream.write_chunk(c).unwrap();
    }
    let (records, _) = stream.finish().unwrap();
    let manifest = manifest_for(header.backup_id, vec![part_manifest(0, records, size)]);
    writer.finalize(&manifest).unwrap();

    let mut reader = PhnxReader::open(&path).unwrap();
    reader.verify_all(false).unwrap();
    let entry = reader.index[0].clone();
    let sh = reader.read_stream_header(&entry).unwrap();
    for (i, ci) in sh.chunks.clone().iter().enumerate() {
        assert_eq!(reader.read_chunk(ci).unwrap(), chunks[i]);
    }
    drop(reader);
    let _ = std::fs::remove_file(&path);
}

/// Throughput probe, not a pass/fail test — run manually with
/// `cargo test -p phoenix-core --release pipeline_throughput -- --ignored --nocapture`
/// to compare 1-worker vs default-worker wall time on this machine.
#[test]
#[ignore]
fn pipeline_throughput() {
    let mb = 512usize;
    let mut state = 42u64;
    let mut chunk = vec![0u8; CHUNK_SIZE];
    for b in chunk[..CHUNK_SIZE / 2].chunks_exact_mut(8) {
        b.copy_from_slice(&splitmix(&mut state).to_le_bytes());
    }
    let chunks: Vec<Vec<u8>> = (0..mb / 4).map(|_| chunk.clone()).collect();
    let extents = vec![extent_for(&chunks, 0)];
    let size = extents[0].sector_count * EXTENT_LBA_BYTES as u64;

    for workers in ["1", ""] {
        if workers.is_empty() {
            std::env::remove_var("PHOENIX_WORKERS");
        } else {
            std::env::set_var("PHOENIX_WORKERS", workers);
        }
        let path =
            std::env::temp_dir().join(format!("pipe-bench-{}.phnx", Uuid::new_v4().simple()));
        let header = Header {
            version: FORMAT_VERSION,
            flags: 0,
            timestamp: 1,
            backup_id: Uuid::new_v4(),
            disk_signature: 1,
            partition_count: 0,
        };
        let start = std::time::Instant::now();
        let mut writer = PhnxWriter::create(&path, header.clone()).unwrap();
        let mut stream = writer
            .begin_partition_stream(PartitionStreamSpec {
                index: 0,
                type_guid: [0u8; 16],
                name: "B".into(),
                original_size: size,
                fs_kind: FilesystemKind::Ntfs,
                capture_mode: CaptureMode::UsedBlocks,
                sector_size: EXTENT_LBA_BYTES,
                used_bytes: 0,
                extents: &extents,
                bytes_per_cluster: 4096,
            })
            .unwrap();
        stream.set_extent(0);
        for c in &chunks {
            stream.write_chunk(c).unwrap();
        }
        let (records, _) = stream.finish().unwrap();
        let manifest = manifest_for(header.backup_id, vec![part_manifest(0, records, size)]);
        writer.finalize(&manifest).unwrap();
        let write_dt = start.elapsed();

        let start = std::time::Instant::now();
        let mut reader = PhnxReader::open(&path).unwrap();
        reader.verify_all(false).unwrap();
        let verify_dt = start.elapsed();

        let label = if workers.is_empty() {
            "default"
        } else {
            workers
        };
        println!(
            "workers={label}: write {mb} MiB in {write_dt:?} ({:.0} MB/s), full verify in {verify_dt:?} ({:.0} MB/s)",
            mb as f64 / write_dt.as_secs_f64(),
            mb as f64 / verify_dt.as_secs_f64(),
        );
        drop(reader);
        let _ = std::fs::remove_file(&path);
    }
    std::env::remove_var("PHOENIX_WORKERS");
}
