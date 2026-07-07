//! End-to-end capture test that runs WITHOUT admin or a real disk: build a
//! synthetic FAT16 image in memory, run the real `fat_plan` + `capture_fat`
//! pipeline into a `.phnx` container, then reopen and verify. This exercises
//! the FAT32-cluster-count / EOC-cluster / data-region-offset fixes through
//! the full streaming + container path, not just the unit-tested planners.

use phoenix_capture::fat::{capture_fat, fat_plan};
use phoenix_capture::MemoryBlockSource;
use phoenix_core::container::{Header, PhnxReader, PhnxWriter, EXTENT_LBA_BYTES, FORMAT_VERSION};
use phoenix_core::disk::{CaptureMode, FilesystemKind};
use phoenix_core::manifest::{BackupManifest, DiskManifest, PartitionManifest};
use uuid::Uuid;

const SECTOR: usize = 512;

/// Geometry of the synthetic FAT16 volume we build below.
struct Fat16 {
    reserved: usize,
    fat_count: usize,
    fat_size: usize,
    root_dir_sectors: usize,
    data_start_sector: usize,
    total_sectors: usize,
    cluster_count: usize,
}

fn fat16_geometry() -> Fat16 {
    let reserved = 1usize;
    let fat_count = 1usize;
    let cluster_count = 5000usize; // in [4085, 65525) -> FAT16
                                   // 2 bytes per FAT16 entry, entries for clusters 0..=cluster_count+1.
    let fat_bytes = (cluster_count + 2) * 2;
    let fat_size = fat_bytes.div_ceil(SECTOR);
    let root_entry_count = 16usize;
    let root_dir_sectors = (root_entry_count * 32).div_ceil(SECTOR);
    let data_start_sector = reserved + fat_count * fat_size + root_dir_sectors;
    let total_sectors = data_start_sector + cluster_count; // 1 sector/cluster
    Fat16 {
        reserved,
        fat_count,
        fat_size,
        root_dir_sectors,
        data_start_sector,
        total_sectors,
        cluster_count,
    }
}

fn build_fat16_image(
    g: &Fat16,
    allocate: &[(u16, Option<u16>)],
    payloads: &[(u16, &[u8])],
) -> Vec<u8> {
    let mut img = vec![0u8; g.total_sectors * SECTOR];

    // --- Boot sector (BPB) ---
    img[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes()); // bytes/sector
    img[13] = 1; // sectors/cluster
    img[14..16].copy_from_slice(&(g.reserved as u16).to_le_bytes());
    img[16] = g.fat_count as u8;
    img[17..19].copy_from_slice(&16u16.to_le_bytes()); // root entries
    img[19..21].copy_from_slice(&(g.total_sectors as u16).to_le_bytes());
    img[22..24].copy_from_slice(&(g.fat_size as u16).to_le_bytes());
    img[510] = 0x55;
    img[511] = 0xAA;

    // --- FAT (first copy) ---
    let fat_off = g.reserved * SECTOR;
    let set_fat = |img: &mut [u8], cluster: u16, value: u16| {
        let o = fat_off + cluster as usize * 2;
        img[o..o + 2].copy_from_slice(&value.to_le_bytes());
    };
    // Reserved entries 0 and 1.
    set_fat(&mut img, 0, 0xFFF8);
    set_fat(&mut img, 1, 0xFFFF);
    for &(cluster, next) in allocate {
        set_fat(&mut img, cluster, next.unwrap_or(0xFFFF));
    }

    // --- Data region payloads ---
    for &(cluster, bytes) in payloads {
        let byte_off = (g.data_start_sector + (cluster as usize - 2)) * SECTOR;
        img[byte_off..byte_off + bytes.len()].copy_from_slice(bytes);
    }

    img
}

#[test]
fn fat16_capture_reproduces_used_clusters() {
    let g = fat16_geometry();

    // Chain A: single cluster 2, EOC. Cluster 3 is left FREE (the gap that
    // splits the used region into two runs). Chain B: 4 -> 5 -> 6, EOC at 6.
    let allocate = [(2u16, None), (4u16, Some(5)), (5u16, Some(6)), (6u16, None)];
    let mut cluster2 = vec![0xAAu8; SECTOR];
    cluster2[..8].copy_from_slice(b"CLUSTER2");
    let mut cluster4 = vec![0xBBu8; SECTOR];
    cluster4[..8].copy_from_slice(b"CLUSTER4");
    let payloads: [(u16, &[u8]); 2] = [(2, &cluster2), (4, &cluster4)];
    let img = build_fat16_image(&g, &allocate, &payloads);
    assert_eq!(img.len(), g.total_sectors * SECTOR);

    // --- Plan extents from the FAT, then capture into a .phnx ---
    let mut src = MemoryBlockSource::new(img.clone());
    let (extents, bitmap_hash, bytes_per_cluster) = fat_plan(&mut src, false).unwrap();
    // Two runs: {cluster 2} and {clusters 3,4,5}.
    assert_eq!(extents.len(), 2, "expected two coalesced used-cluster runs");
    let used_clusters: u64 = extents.iter().map(|e| e.sector_count).sum();
    assert_eq!(used_clusters, 4, "4 clusters allocated (1 + 3)");

    let path = std::env::temp_dir().join(format!("fat16_{}.phnx", Uuid::new_v4()));
    let backup_id = Uuid::new_v4();
    let header = Header {
        version: FORMAT_VERSION,
        flags: 0,
        timestamp: 1,
        backup_id,
        disk_signature: 7,
        partition_count: 1,
    };
    let mut writer = PhnxWriter::create(&path, header).unwrap();
    let mut stream = writer
        .begin_partition_stream(
            0,
            [0u8; 16],
            "FAT16".into(),
            (g.total_sectors * SECTOR) as u64,
            FilesystemKind::Fat,
            CaptureMode::UsedBlocks,
            EXTENT_LBA_BYTES,
            0,
            &extents,
            bytes_per_cluster,
        )
        .unwrap();
    let (used_bytes, _) =
        capture_fat(&mut src, &mut stream, &extents, bitmap_hash.clone()).unwrap();
    assert_eq!(used_bytes, 4 * SECTOR as u64);
    let (chunks, _) = stream.finish().unwrap();

    let manifest = BackupManifest {
        format_version: 1,
        backup_id,
        parent_backup_id: None,
        hostname: "TEST".into(),
        disk: DiskManifest {
            style: "mbr".into(),
            disk_guid: None,
            sector_size: 512,
        },
        partitions: vec![PartitionManifest {
            index: 0,
            name: "FAT16".into(),
            type_guid: None,
            fs: "fat".into(),
            capture_mode: "used-blocks".into(),
            original_size: (g.total_sectors * SECTOR) as u64,
            used_bytes,
            chunks,
            bitmap_hash,
        }],
    };
    writer.finalize(&manifest).unwrap();

    // --- Reopen, verify hashes, and reassemble the captured regions ---
    let mut reader = PhnxReader::open(&path).unwrap();
    reader.verify_all(false).unwrap();

    let entry = reader.index[0].clone();
    let stream_hdr = reader.read_stream_header(&entry).unwrap();
    // Reconstruct each extent's bytes from its chunk(s) and compare against
    // the source image at the extent's disk offset. Proves the captured data
    // (including the EOC-terminated final clusters) matches the source.
    for chunk in stream_hdr.chunks.clone() {
        let data = reader.read_chunk(&chunk).unwrap();
        let ext = &stream_hdr.extents[chunk.extent_index as usize];
        let disk_off = ext.start_sector as usize * SECTOR
            + chunk.chunk_index as usize * phoenix_core::container::CHUNK_SIZE;
        assert_eq!(
            &img[disk_off..disk_off + data.len()],
            &data[..],
            "captured bytes differ from source at disk offset {disk_off}"
        );
    }

    let _ = std::fs::remove_file(&path);
    let _ = g.fat_size; // silence unused in some configs
    let _ = g.root_dir_sectors;
}
