use std::io::{Seek, SeekFrom};

use byteorder::{LittleEndian, ReadBytesExt};
use phoenix_core::container::{Extent, CHUNK_SIZE};
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::hash;
use phoenix_core::ProgressHandle;

use crate::reader::BlockSource;

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
    reader: &mut impl BlockSource,
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
    reader: &mut impl BlockSource,
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

/// Compute the volume's total cluster count from a parsed NTFS BPB.
///
/// `bs.total_sectors` is recorded in units of `bs.bytes_per_sector` (per the
/// NTFS spec), NOT in 512-byte LBAs. Earlier versions of this code multiplied
/// by the hardcoded `SECTOR_SIZE = 512` constant, which silently undercounted
/// 4Kn (4096-byte-logical-sector) volumes by 8x — a 1 TB drive came out as
/// ~125 GB, leaving most of the user's data uncaptured. Always compute via
/// `bytes_per_sector` from the BPB so the math is correct for both 512e (512)
/// and 4Kn (4096) media.
fn total_clusters_for(bs: &NtfsBootSector, cluster_size: u64) -> u64 {
    let volume_bytes = bs.total_sectors.saturating_mul(bs.bytes_per_sector as u64);
    (volume_bytes + cluster_size - 1) / cluster_size
}

pub fn estimate_used_bytes(reader: &mut impl BlockSource) -> Result<u64> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let bs = parse_boot_sector(&boot)?;
    let cluster_size = bs.bytes_per_sector as u64 * bs.sectors_per_cluster as u64;
    let total_clusters = total_clusters_for(&bs, cluster_size);
    let bitmap = read_bitmap(reader, &bs, cluster_size, total_clusters)?;
    let used_clusters = (0..total_clusters)
        .filter(|c| cluster_used(&bitmap, *c as usize))
        .count() as u64;
    Ok(used_clusters * cluster_size + cluster_size) // + boot/metadata overhead
}

/// Restore an NTFS partition. Returns the number of bytes written so the
/// caller can keep its byte-scaled progress meter accurate. `bytes_done`
/// is the running total before this partition starts, used by
/// `restore_raw` to render the in-flight progress bar; the function
/// returns the *delta* (this partition's bytes) so the caller can
/// accumulate it.
///
/// `relocation` is `Some` for shrink restores where data past the new
/// boundary needs translating, `None` otherwise. It's threaded straight
/// through to `restore_raw`; see `phoenix-core/src/relocation.rs` for
/// the semantics. After the relocated data is on disk, the caller is
/// expected to invoke the metadata rewriter (`phoenix-capture/src/
/// ntfs_meta.rs`) before `patch_ntfs_size` so MFT records, `$Bitmap`,
/// and `$LogFile` reflect the new cluster layout — otherwise the volume
/// mounts but reads come back as garbage.
pub fn restore_ntfs(
    reader: &mut phoenix_core::container::PhnxReader,
    entry: &phoenix_core::container::PartitionIndexEntry,
    writer: &mut crate::raw::PartitionWriter,
    target_size: u64,
    verify: bool,
    progress: Option<&ProgressHandle>,
    bytes_done: u64,
    relocation: Option<&phoenix_core::relocation::RelocationMap>,
) -> Result<u64> {
    if entry.used_bytes > target_size {
        return Err(PhoenixError::PartitionTooSmall {
            partition_index: entry.index,
            target_size,
            required: entry.used_bytes,
        });
    }
    let written = crate::raw::restore_raw(
        reader, entry, writer, verify, progress, bytes_done, relocation,
    )?;
    if let Some(map) = relocation {
        // Rewrite MFT data runs, $Bitmap, $LogFile, and the MFT mirror
        // so the on-disk metadata points at the relocated cluster
        // positions. Without this, a shrink with relocation produces a
        // mountable-but-corrupt volume: the data bytes are on disk but
        // every file references its old (now garbage) clusters.
        crate::ntfs_meta::rewrite_metadata_after_relocation(writer, map)?;
    }
    patch_ntfs_size(writer, target_size)?;
    Ok(written)
}

/// Patch the NTFS boot sector to match a resized target partition.
///
/// Why this matters: NTFS records the volume length in
/// `BPB.TotalSectors` (8-byte LE at offset 40). At mount time Windows
/// cross-checks that field against the partition table entry — if the
/// boot sector says the volume is 4 TB but the partition is only 3 TB,
/// or vice versa, mountmgr declines to recognize the file system and
/// the user sees a RAW partition that `chkdsk` can't even examine. The
/// previous behavior of this function — `Ok(())` — was correct only for
/// the same-size restore path; the moment the GUI's per-partition size
/// override changed, every restore came up RAW.
///
/// Convention: NTFS stores `total_sectors` as `partition_sectors - 1`.
/// The very last sector of the partition is reserved for a duplicate of
/// the boot sector so a damaged primary can be reconstructed from the
/// tail (Windows verifies both at mount). Resizing therefore needs two
/// writes: the patched primary at LBA 0, and a copy at the *new* last
/// LBA — but only on **shrink**.
///
/// Grow handling moved out of here. Setting `total_sectors` to the new
/// (larger) value here makes the volume come up RAW: Windows refuses
/// to mount NTFS when `$Bitmap` doesn't cover the cluster range
/// advertised by the boot sector, and our on-disk `$Bitmap` is sized
/// for the source. The previous "let chkdsk reconcile" comment was
/// optimistic — modern Windows declines to mount it at all, so chkdsk
/// never gets a chance. The fix is to leave the boot sector at its
/// source size (so the volume mounts at source size, as a normal
/// NTFS volume with the partition advertising more space than the FS
/// uses), then issue `FSCTL_EXTEND_VOLUME` against the mounted volume
/// after the partition table is in place. See
/// `phoenix_restore::grow::extend_ntfs_volume`.
///
/// What this *still* doesn't fix on shrinks: the trailing bits past the
/// new `total_clusters` are leftover allocation state that `chkdsk /F`
/// reconciles. The pre-flight extent-overflow check in
/// `RestorePlan::validate_extents_fit` guarantees no captured data
/// lives past the new boundary, so chkdsk's job is purely metadata
/// reconciliation, not data recovery.
fn patch_ntfs_size(writer: &mut crate::raw::PartitionWriter, new_size: u64) -> Result<()> {
    // Make sure any pending data writes are off the cache before we read
    // the boot sector back — see `PartitionWriter::flush` for the
    // longer rationale.
    writer.flush()?;

    let mut sector = vec![0u8; 512];
    writer.read_at(0, &mut sector)?;
    if &sector[3..7] != b"NTFS" {
        return Err(PhoenixError::Other(
            "expected NTFS boot sector at LBA 0 of the restored partition; got something else \
             (the data stream may not have been written correctly)"
                .into(),
        ));
    }
    let bytes_per_sector = u16::from_le_bytes(sector[11..13].try_into().unwrap()) as u64;
    if bytes_per_sector == 0 {
        return Err(PhoenixError::Other(
            "NTFS boot sector reports zero bytes per sector".into(),
        ));
    }
    if new_size < 4 * bytes_per_sector {
        return Err(PhoenixError::Other(format!(
            "target partition is too small ({} bytes) for an NTFS volume",
            new_size
        )));
    }

    let cur_total_field = u64::from_le_bytes(sector[40..48].try_into().unwrap());
    let new_total_field = new_size / bytes_per_sector - 1;
    if cur_total_field == new_total_field {
        // Same size as the source. The boot sector is already correct;
        // skip both writes. This is the common path when the user takes
        // the default plan.
        return Ok(());
    }

    // Grow case: leave the boot sector pointing at the source size and
    // let the post-restore `FSCTL_EXTEND_VOLUME` step (see
    // `phoenix_restore::grow::extend_ntfs_volume`) ask NTFS to grow
    // itself once it's mounted. Patching `total_sectors` here would
    // make the volume come up RAW because `$Bitmap` is still sized
    // for the source.
    if new_total_field > cur_total_field {
        tracing::info!(
            cur_total_sectors = cur_total_field,
            requested_total_sectors = new_total_field,
            new_size_bytes = new_size,
            bytes_per_sector,
            "NTFS grow: leaving boot sector at source size; volume will be extended via \
             FSCTL_EXTEND_VOLUME after the partition table is written"
        );
        return Ok(());
    }

    sector[40..48].copy_from_slice(&new_total_field.to_le_bytes());

    // Shrink case only: write the patched primary at LBA 0 and a
    // duplicate at the new last LBA. The source's tail backup sector
    // sits past the new end of the partition and is discarded.
    writer.write_at(0, &sector)?;
    writer.write_at(new_size - bytes_per_sector, &sector)?;

    tracing::info!(
        old_total_sectors = cur_total_field,
        new_total_sectors = new_total_field,
        new_size_bytes = new_size,
        bytes_per_sector,
        "patched NTFS boot sector (primary + backup) to match shrunk partition"
    );
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
pub fn ntfs_plan(reader: &mut impl BlockSource) -> Result<(Vec<Extent>, Option<String>)> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let bs = parse_boot_sector(&boot)?;
    let cluster_size = bs.bytes_per_sector as u64 * bs.sectors_per_cluster as u64;
    let total_clusters = total_clusters_for(&bs, cluster_size);
    tracing::debug!(
        target: "phoenix_capture::ntfs",
        bytes_per_sector = bs.bytes_per_sector,
        sectors_per_cluster = bs.sectors_per_cluster,
        total_sectors = bs.total_sectors,
        cluster_size = cluster_size,
        total_clusters = total_clusters,
        "ntfs_plan computed volume geometry"
    );
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
