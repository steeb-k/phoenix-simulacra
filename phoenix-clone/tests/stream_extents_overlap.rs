//! T1 (no-admin) coverage for the double-buffered clone copy loop: drive
//! `stream_extents` with an in-memory source and a temp-file-backed
//! `PartitionWriter`, and byte-compare the target against a reference image.
//! The target starts sentinel-filled so the comparison also proves the loop
//! writes ONLY inside the extents. Exercises the plain path, ReadBack verify,
//! a relocation split, and cancellation (which must not hang the reader
//! thread).

use phoenix_capture::{MemoryBlockSource, PartitionWriter};
use phoenix_clone::{stream_extents, CloneOptions, ClonePlan, CloneVerify};
use phoenix_core::container::{Extent, CHUNK_SIZE, EXTENT_LBA_BYTES};
use phoenix_core::relocation::{RelocationEntry, RelocationMap};
use phoenix_core::{PhoenixError, ProgressHandle};
use uuid::Uuid;

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn opts(verify: CloneVerify, progress: Option<ProgressHandle>) -> CloneOptions {
    CloneOptions {
        source_disk_index: 0,
        target_disk_index: 0,
        plan: ClonePlan::default(),
        verify,
        use_vss: false,
        progress,
    }
}

/// Source image: 12 MiB of deterministic PRNG bytes (multi-chunk, so the
/// reader thread genuinely runs ahead of the writer).
fn source_image() -> Vec<u8> {
    let mut state = 77u64;
    let mut img = vec![0u8; 12 * 1024 * 1024];
    for b in img.chunks_exact_mut(8) {
        b.copy_from_slice(&splitmix(&mut state).to_le_bytes());
    }
    img
}

fn target_file(len: usize) -> (std::path::PathBuf, PartitionWriter) {
    let path =
        std::env::temp_dir().join(format!("clone-overlap-{}.img", Uuid::new_v4().simple()));
    std::fs::write(&path, vec![0x5Au8; len]).unwrap();
    let writer = PartitionWriter::open_disk(path.to_str().unwrap(), 0, 512).unwrap();
    (path, writer)
}

#[test]
fn stream_extents_copies_only_extent_bytes() {
    let img = source_image();
    // Extent 0: 5 MiB + 512 B starting at sector 0 (multi-chunk + odd tail).
    // Extent 1: 1 MiB starting at 8 MiB. The 3 MiB gap must stay sentinel.
    let extents = vec![
        Extent {
            start_sector: 0,
            sector_count: (5 * 1024 * 1024 + 512) / EXTENT_LBA_BYTES as u64,
        },
        Extent {
            start_sector: (8 * 1024 * 1024) / EXTENT_LBA_BYTES as u64,
            sector_count: (1024 * 1024) / EXTENT_LBA_BYTES as u64,
        },
    ];

    for verify in [CloneVerify::None, CloneVerify::ReadBack] {
        let mut src = MemoryBlockSource::new(img.clone());
        let (path, mut writer) = target_file(img.len());
        let copied =
            stream_extents(&mut src, &mut writer, &extents, None, &opts(verify, None), 0).unwrap();
        drop(writer);

        let mut expected_total = 0u64;
        let mut expected = vec![0x5Au8; img.len()];
        for e in &extents {
            let start = (e.start_sector * EXTENT_LBA_BYTES as u64) as usize;
            let len = (e.sector_count * EXTENT_LBA_BYTES as u64) as usize;
            expected[start..start + len].copy_from_slice(&img[start..start + len]);
            expected_total += len as u64;
        }
        // ReadBack mode double-counts progress by design; bytes returned from
        // the function reflect that (matches the serial behavior).
        let expected_copied = match verify {
            CloneVerify::None => expected_total,
            CloneVerify::ReadBack => expected_total * 2,
        };
        assert_eq!(copied, expected_copied, "bytes_done for {verify:?}");

        let actual = std::fs::read(&path).unwrap();
        for (i, (a, e)) in actual.chunks(4096).zip(expected.chunks(4096)).enumerate() {
            assert_eq!(a, e, "target differs in 4 KiB block {i} ({verify:?})");
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// A relocation map must split writes exactly as the serial loop did: bytes
/// below the boundary land identity-mapped, bytes above land at the
/// translated offset.
#[test]
fn stream_extents_honors_relocation_split() {
    let img = source_image();
    let cluster = 4096u64;
    // One extent spanning 0..6 MiB. Clusters at/above 1 MiB relocate down to
    // 512 KiB (dst), i.e. a chunk straddling 1 MiB splits into two segments.
    let extents = vec![Extent {
        start_sector: 0,
        sector_count: (6 * 1024 * 1024) / EXTENT_LBA_BYTES as u64,
    }];
    let boundary = 1024 * 1024u64;
    let reloc_src_clusters = boundary / cluster; // clusters 256.. relocate
    let reloc_dst_start = (512 * 1024) / cluster; // to cluster 128..
    let reloc_count = (5 * 1024 * 1024) / cluster;
    let map = RelocationMap {
        sector_size: EXTENT_LBA_BYTES as u64,
        cluster_size: cluster,
        // Inclusive: the last identity-mapped cluster. Cluster 256 (byte
        // 1 MiB) is the first relocated one.
        safe_max_cluster: reloc_src_clusters - 1,
        new_total_clusters: reloc_dst_start + reloc_count,
        entries: vec![RelocationEntry {
            src_cluster_start: reloc_src_clusters,
            cluster_count: reloc_count,
            dst_cluster_start: reloc_dst_start,
        }],
    };

    let mut src = MemoryBlockSource::new(img.clone());
    let (path, mut writer) = target_file(img.len());
    stream_extents(
        &mut src,
        &mut writer,
        &extents,
        Some(&map),
        &opts(CloneVerify::ReadBack, None),
        0,
    )
    .unwrap();
    drop(writer);

    let mut expected = vec![0x5Au8; img.len()];
    // Identity region: 0..1 MiB. Relocated region: source 1..6 MiB lands at
    // 512 KiB.. (overwriting part of the identity region — last write wins,
    // exactly like the serial loop's in-order writes).
    expected[..boundary as usize].copy_from_slice(&img[..boundary as usize]);
    let dst = (reloc_dst_start * cluster) as usize;
    let src_lo = boundary as usize;
    let src_hi = (6 * 1024 * 1024) as usize;
    expected[dst..dst + (src_hi - src_lo)].copy_from_slice(&img[src_lo..src_hi]);

    let actual = std::fs::read(&path).unwrap();
    for (i, (a, e)) in actual.chunks(4096).zip(expected.chunks(4096)).enumerate() {
        assert_eq!(a, e, "relocated target differs in 4 KiB block {i}");
    }
    let _ = std::fs::remove_file(&path);
}

/// Cancel mid-copy: returns `Cancelled`, and the scoped reader thread winds
/// down instead of deadlocking on the full channel (the test would hang, not
/// fail, if teardown were wrong).
#[test]
fn stream_extents_cancel_does_not_hang() {
    let img = source_image();
    let extents = vec![Extent {
        start_sector: 0,
        sector_count: img.len() as u64 / EXTENT_LBA_BYTES as u64,
    }];
    let progress = ProgressHandle::new();
    progress.cancel();
    let mut src = MemoryBlockSource::new(img.clone());
    let (path, mut writer) = target_file(img.len());
    let err = stream_extents(
        &mut src,
        &mut writer,
        &extents,
        None,
        &opts(CloneVerify::None, Some(progress)),
        0,
    )
    .unwrap_err();
    assert!(matches!(err, PhoenixError::Cancelled), "got {err:?}");
    drop(writer);
    let _ = std::fs::remove_file(&path);
    let _ = CHUNK_SIZE; // keep the import meaningful if sizes change
}
