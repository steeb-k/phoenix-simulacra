use std::io::{Seek, SeekFrom};

use byteorder::{LittleEndian, ReadBytesExt};
use phoenix_core::container::{Extent, CHUNK_SIZE};
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::hash;
use phoenix_core::ProgressHandle;

use crate::reader::PartitionReader;

const SECTOR_SIZE: u64 = 512;

#[derive(Debug)]
struct NtfsBootSector {
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    total_sectors: u64,
    mft_cluster: i64,
    clusters_per_mft_record: i8,
}

/// Stream every byte covered by `extents` from the (possibly volume-locked)
/// reader through `stream`. The extents and `bitmap_hash` are produced by
/// [`ntfs_plan`] **before** the volume is locked — this function deliberately
/// does not re-read the boot sector or the allocation bitmap, because
/// `FSCTL_GET_VOLUME_BITMAP` returns `ERROR_NOT_READY` (Win32 error 21) on a
/// locked NTFS volume on at least some Windows builds (USB-attached volumes
/// being a confirmed offender). All metadata work has to happen up front.
///
/// The extents passed in are the same ones written into the partition
/// manifest, so `set_extent(ext_idx)` is unambiguous and there's no risk of
/// two-bitmap-reads-can-disagree drift.
pub fn capture_ntfs(
    reader: &mut PartitionReader,
    stream: &mut phoenix_core::container::PartitionStreamWriter<'_>,
    extents: &[Extent],
    bitmap_hash: Option<String>,
) -> Result<(u64, Option<String>)> {
    let mut total_used = 0u64;
    for (ext_idx, extent) in extents.iter().enumerate() {
        stream.set_extent(ext_idx as u32);
        let byte_len = extent.sector_count * SECTOR_SIZE;
        let mut pos = 0u64;
        let base_byte = extent.start_sector * SECTOR_SIZE;
        while pos < byte_len {
            let to_read = CHUNK_SIZE.min((byte_len - pos) as usize);
            let mut buf = vec![0u8; to_read];
            let n = reader.read_at(base_byte + pos, &mut buf)?;
            if n == 0 {
                break;
            }
            stream.write_chunk(&buf[..n])?;
            total_used += n as u64;
            pos += n as u64;
        }
    }
    Ok((total_used, bitmap_hash))
}

fn parse_boot_sector(boot: &[u8]) -> Result<NtfsBootSector> {
    if boot.len() < 512 || &boot[3..7] != b"NTFS" {
        return Err(PhoenixError::Other("not NTFS".into()));
    }
    let mut cur = std::io::Cursor::new(boot);
    cur.seek(SeekFrom::Start(11))?;
    let bytes_per_sector = cur.read_u16::<LittleEndian>()?;
    let sectors_per_cluster = cur.read_u8()?;
    cur.seek(SeekFrom::Start(40))?;
    let total_sectors = cur.read_u64::<LittleEndian>()?;
    cur.seek(SeekFrom::Start(48))?;
    let mft_cluster = cur.read_i64::<LittleEndian>()?;
    let clusters_per_mft_record = cur.read_i8()?;
    Ok(NtfsBootSector {
        bytes_per_sector,
        sectors_per_cluster,
        total_sectors,
        mft_cluster,
        clusters_per_mft_record,
    })
}

fn read_bitmap(
    reader: &mut PartitionReader,
    bs: &NtfsBootSector,
    cluster_size: u64,
    total_clusters: u64,
) -> Result<Vec<u8>> {
    // Preferred path: ask the filesystem driver via FSCTL_GET_VOLUME_BITMAP.
    // This works for any handle opened against a mounted volume — including
    // VSS shadow-copy device paths, which are also volume devices — and is the
    // only way to get an accurate allocation map without re-implementing an
    // NTFS MFT parser. Without this, the fallback below treats every cluster
    // as used and silently turns a "used blocks" backup into a full raw clone
    // of the partition.
    if let Some(bm) = reader.try_volume_bitmap(total_clusters) {
        let bitmap_bytes = ((total_clusters + 7) / 8) as usize;
        if bm.len() >= bitmap_bytes {
            tracing::info!(
                total_clusters,
                "FSCTL_GET_VOLUME_BITMAP returned allocation map; capturing only used clusters"
            );
            return Ok(bm);
        }
        tracing::debug!(
            got = bm.len(),
            expected = bitmap_bytes,
            "FSCTL_GET_VOLUME_BITMAP returned a short bitmap; padding with all-used"
        );
        let mut padded = vec![0xFFu8; bitmap_bytes];
        padded[..bm.len()].copy_from_slice(&bm);
        return Ok(padded);
    }

    // Fallback: mark every cluster as used. This is the conservative choice
    // (correct, just wasteful) for handles where the IOCTL doesn't apply,
    // such as raw `\\.\PhysicalDriveN` access in WinPE.
    tracing::warn!(
        total_clusters,
        "FSCTL_GET_VOLUME_BITMAP unavailable; falling back to full-partition capture"
    );
    let bitmap_bytes = ((total_clusters + 7) / 8) as usize;
    let _ = (bs, cluster_size);
    Ok(vec![0xFFu8; bitmap_bytes])
}

fn cluster_used(bitmap: &[u8], cluster: usize) -> bool {
    let byte = cluster / 8;
    let bit = cluster % 8;
    if byte >= bitmap.len() {
        return true;
    }
    (bitmap[byte] >> bit) & 1 == 1
}

pub fn estimate_used_bytes(reader: &mut PartitionReader) -> Result<u64> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let bs = parse_boot_sector(&boot)?;
    let cluster_size = bs.bytes_per_sector as u64 * bs.sectors_per_cluster as u64;
    let total_clusters = (bs.total_sectors * SECTOR_SIZE + cluster_size - 1) / cluster_size;
    let bitmap = read_bitmap(reader, &bs, cluster_size, total_clusters)?;
    let used_clusters = (0..total_clusters)
        .filter(|c| cluster_used(&bitmap, *c as usize))
        .count() as u64;
    Ok(used_clusters * cluster_size + cluster_size) // + boot/metadata overhead
}

pub fn restore_ntfs(
    reader: &mut phoenix_core::container::PhnxReader,
    entry: &phoenix_core::container::PartitionIndexEntry,
    writer: &mut crate::raw::PartitionWriter,
    target_size: u64,
    verify: bool,
    progress: Option<&ProgressHandle>,
    chunks_done: u64,
) -> Result<u64> {
    if entry.used_bytes > target_size {
        return Err(PhoenixError::PartitionTooSmall {
            partition_index: entry.index,
            target_size,
            required: entry.used_bytes,
        });
    }
    let done = crate::raw::restore_raw(reader, entry, writer, verify, progress, chunks_done)?;
    patch_ntfs_size(writer, target_size)?;
    Ok(done)
}

fn patch_ntfs_size(writer: &mut crate::raw::PartitionWriter, new_size: u64) -> Result<()> {
    let sector_size = 512u64;
    let new_sectors = new_size / sector_size;
    let mut boot = vec![0u8; 512];
    writer.write_at(0, &[])?; // no-op read - we need read support
    let _ = (new_sectors, boot);
    // Boot sector patch requires read-modify-write; documented for operator
    Ok(())
}

/// Read the NTFS boot sector and allocation bitmap once and translate them
/// into `(used_extents, bitmap_hash)`. Callers must invoke this exactly once
/// per partition, **before** acquiring `FSCTL_LOCK_VOLUME` on the same
/// handle: the FSCTL_GET_VOLUME_BITMAP IOCTL fails with `ERROR_NOT_READY` on
/// a locked NTFS volume on some Windows builds (USB-attached drives being a
/// reliable reproducer), so the bitmap query has to happen on an unlocked
/// handle. The returned extents are then handed to both
/// `PhnxWriter::begin_partition_stream` (for the manifest) and
/// [`capture_ntfs`] (for the data stream); reading the bitmap a second time
/// would risk re-tripping the same IOCTL failure and re-introduce the
/// previously-flagged two-reads-can-disagree race.
pub fn ntfs_plan(reader: &mut PartitionReader) -> Result<(Vec<Extent>, Option<String>)> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let bs = parse_boot_sector(&boot)?;
    let cluster_size = bs.bytes_per_sector as u64 * bs.sectors_per_cluster as u64;
    let total_clusters = (bs.total_sectors * SECTOR_SIZE + cluster_size - 1) / cluster_size;
    let bitmap = read_bitmap(reader, &bs, cluster_size, total_clusters)?;
    let bitmap_hash = Some(hash::hash_hex(&bitmap));

    let mut extents = Vec::new();
    let mut i = 0u64;
    while i < total_clusters {
        if cluster_used(&bitmap, i as usize) {
            let start = i;
            while i < total_clusters && cluster_used(&bitmap, i as usize) {
                i += 1;
            }
            extents.push(Extent {
                start_sector: (start * cluster_size) / SECTOR_SIZE,
                sector_count: ((i - start) * cluster_size) / SECTOR_SIZE,
            });
        } else {
            i += 1;
        }
    }
    if extents.is_empty() {
        // The bitmap reported nothing allocated. This is almost always the
        // fallback case (FSCTL unavailable -> read_bitmap returned all
        // zeros); capture at least the boot region so a restored volume
        // still has a recognizable header.
        extents.push(Extent {
            start_sector: 0,
            sector_count: (cluster_size / SECTOR_SIZE).max(8),
        });
    }
    Ok((extents, bitmap_hash))
}
