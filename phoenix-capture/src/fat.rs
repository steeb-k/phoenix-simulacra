use phoenix_core::container::{Extent, CHUNK_SIZE};
use phoenix_core::disk::FilesystemKind;
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::hash;
use phoenix_core::ProgressHandle;

use crate::reader::PartitionReader;

const SECTOR: u64 = 512;

#[derive(Clone, Copy, PartialEq)]
enum FatType {
    Fat16,
    Fat32,
    Exfat,
}

/// Stream every byte covered by `extents` from the reader through `stream`.
/// The extents and `bitmap_hash` are produced by [`fat_plan`] **before**
/// streaming starts; this function deliberately does not re-read the boot
/// sector or the FAT, so the chunks' `extent_index` values line up with the
/// manifest's extent table that `begin_partition_stream` already wrote.
///
/// Why this split matters: previously `capture_fat` recomputed its own
/// extent list internally and called `stream.set_extent(idx)` over *that*
/// list, while the manifest's extent table was a single placeholder
/// `{start: 0, count: size/512}` planted by `plan_capture`. Restore would
/// then index `stream.extents[chunk.extent_index]` and panic on any
/// non-trivial FAT/exFAT volume, because every `extent_index >= 1` was
/// out of bounds. Mirror NTFS's `ntfs_plan` + `capture_ntfs` shape so the
/// manifest's extent table is the authoritative one.
pub fn capture_fat(
    reader: &mut PartitionReader,
    stream: &mut phoenix_core::container::PartitionStreamWriter<'_>,
    extents: &[Extent],
    bitmap_hash: Option<String>,
) -> Result<(u64, Option<String>)> {
    let mut total_used = 0u64;
    for (ext_idx, extent) in extents.iter().enumerate() {
        stream.set_extent(ext_idx as u32);
        let byte_len = extent.sector_count * SECTOR;
        let base = extent.start_sector * SECTOR;
        let mut pos = 0u64;
        while pos < byte_len {
            let to_read = CHUNK_SIZE.min((byte_len - pos) as usize);
            let mut buf = vec![0u8; to_read];
            let n = reader.read_at(base + pos, &mut buf)?;
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

/// Read the FAT/exFAT boot sector + FAT, compute used-cluster extents,
/// and return them along with a hash of the FAT table and the cluster
/// size in bytes. Mirror of [`crate::ntfs::ntfs_plan`]; called from
/// `plan_capture` so the planned extents end up in the manifest's
/// extent table (rather than the previous "single placeholder extent
/// covering the whole partition" hack that desynced from the per-chunk
/// `extent_index` values).
pub fn fat_plan(
    reader: &mut PartitionReader,
    exfat: bool,
) -> Result<(Vec<Extent>, Option<String>, u32)> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let (fat_type, cluster_size, data_start, total_clusters, fat_bits) =
        parse_fat_boot(&boot, exfat)?;

    let fat_len = ((total_clusters + 2) * fat_bits as u64 + 7) / 8;
    let mut fat_table = vec![0u8; fat_len as usize];
    if fat_len > 0 {
        // exFAT's FAT lives at the FatOffset sector, not the cluster heap;
        // for FAT12/16/32 `data_start` returned by `parse_fat_boot` IS the
        // post-FAT cluster region, but the FAT itself starts at
        // `(reserved sectors) * bytes_per_sector`. We re-derive the FAT
        // start here rather than threading another value out of the
        // parser to keep the parser's tuple shape compatible.
        let fat_byte_offset = match fat_type {
            FatType::Exfat => {
                u32::from_le_bytes(boot[80..84].try_into().unwrap()) as u64
                    * (1u64 << boot[108])
            }
            _ => {
                let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]) as u64;
                let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u64;
                reserved * bytes_per_sector
            }
        };
        reader.read_at(fat_byte_offset, &mut fat_table)?;
    }

    let bitmap_hash = Some(hash::hash_hex(&fat_table));
    let extents = fat_used_extents(&fat_table, fat_type, cluster_size, data_start, total_clusters);
    let bytes_per_cluster: u32 = cluster_size.try_into().unwrap_or(u32::MAX);
    Ok((extents, bitmap_hash, bytes_per_cluster))
}

fn parse_fat_boot(boot: &[u8], exfat: bool) -> Result<(FatType, u64, u64, u64, u32)> {
    if exfat {
        // Tighter magic check than the previous `boot[3] != 0x45`: an exFAT
        // OEM ID is the literal 8 bytes "EXFAT   " (5 letters + 3 spaces),
        // and there are pathological boot sectors where byte 3 happens to
        // be 'E' but the rest of the OEM ID isn't "EXFAT". Verify the full
        // string before trusting the rest of our offsets.
        if boot.len() < 512 || &boot[3..11] != b"EXFAT   " {
            return Err(PhoenixError::Other("not exFAT".into()));
        }
        // `cluster_size` is `1 << (BytesPerSectorShift + SectorsPerClusterShift)`.
        // The previous code just used `1 << boot[108]` (BytesPerSectorShift
        // alone), which on a default-formatted exFAT volume returns 512
        // instead of the actual cluster size — typically 131072 for >32 GiB
        // volumes — and broke every downstream calculation that depended
        // on it.
        let bps_shift = boot[108];
        let spc_shift = boot[109];
        let bytes_per_sector = 1u64 << bps_shift;
        let cluster_size = bytes_per_sector << spc_shift;
        // ClusterCount is a 32-bit field at offset 92, NOT 116. The old
        // `boot[116..120]` read landed in a Reserved area that the spec
        // says MUST be zero, so cluster_count came back as 0 and the
        // capture path silently degenerated to a tiny placeholder extent.
        let cluster_count =
            u32::from_le_bytes(boot[92..96].try_into().unwrap()) as u64;
        // `data_start` is the byte offset where cluster #2 lives —
        // i.e. `ClusterHeapOffset (in sectors) * bytes_per_sector`.
        // The previous formula (`fat_offset + cluster_count * cluster_size`)
        // was the position of the *end* of the cluster heap and had no
        // physical meaning as a "data start". The 4 TB exFAT case with
        // those bugs made byte_start values reach trillions of bytes
        // beyond the partition end.
        let cluster_heap_offset =
            u32::from_le_bytes(boot[88..92].try_into().unwrap()) as u64;
        let data_start = cluster_heap_offset * bytes_per_sector;
        return Ok((FatType::Exfat, cluster_size, data_start, cluster_count, 32));
    }

    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(PhoenixError::Other("invalid FAT boot signature".into()));
    }
    let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]) as u64;
    let sectors_per_cluster = boot[13] as u64;
    let cluster_size = bytes_per_sector * sectors_per_cluster;
    let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u64;
    let fat_count = boot[16] as u64;
    let fat_size_sectors = u16::from_le_bytes([boot[22], boot[23]]) as u64;
    let total_sectors = if boot[19] != 0 {
        u16::from_le_bytes([boot[19], boot[20]]) as u64
    } else {
        u32::from_le_bytes(boot[32..36].try_into().unwrap()) as u64
    };
    let fat_type = if fat_size_sectors == 0 {
        let fat_size32 = u32::from_le_bytes(boot[36..40].try_into().unwrap()) as u64;
        let root_clusters = u32::from_le_bytes(boot[44..48].try_into().unwrap()) as u64;
        let data_start =
            (reserved + fat_count * fat_size32) * bytes_per_sector;
        let cluster_count = root_clusters;
        (FatType::Fat32, cluster_size, data_start, cluster_count, 32)
    } else {
        let data_start = (reserved + fat_count * fat_size_sectors) * bytes_per_sector;
        let cluster_count = (total_sectors - reserved - fat_count * fat_size_sectors)
            / sectors_per_cluster;
        (FatType::Fat16, cluster_size, data_start, cluster_count, 16)
    };
    Ok(fat_type)
}

fn fat_used_extents(
    fat: &[u8],
    fat_type: FatType,
    cluster_size: u64,
    data_start: u64,
    total_clusters: u64,
) -> Vec<Extent> {
    let mut used = Vec::new();
    for c in 2..=total_clusters + 1 {
        let entry = fat_cluster_value(fat, c, fat_type);
        if entry != 0 && entry != 0x0FFF_FFFF && entry != 0xFFFF {
            used.push(c);
        }
    }
    if used.is_empty() {
        return vec![Extent {
            start_sector: 0,
            sector_count: (cluster_size / SECTOR).max(8),
        }];
    }

    let mut extents = Vec::new();
    let mut start = used[0];
    let mut prev = used[0];
    for &c in used.iter().skip(1) {
        if c == prev + 1 {
            prev = c;
        } else {
            let byte_start = data_start + (start - 2) * cluster_size;
            extents.push(Extent {
                start_sector: byte_start / SECTOR,
                sector_count: ((prev - start + 1) * cluster_size) / SECTOR,
            });
            start = c;
            prev = c;
        }
    }
    let byte_start = data_start + (start - 2) * cluster_size;
    extents.push(Extent {
        start_sector: byte_start / SECTOR,
        sector_count: ((prev - start + 1) * cluster_size) / SECTOR,
    });
    extents
}

fn fat_cluster_value(fat: &[u8], cluster: u64, fat_type: FatType) -> u32 {
    match fat_type {
        FatType::Fat16 => {
            let off = (cluster * 2) as usize;
            if off + 2 > fat.len() {
                return 0;
            }
            u16::from_le_bytes([fat[off], fat[off + 1]]) as u32
        }
        FatType::Fat32 | FatType::Exfat => {
            let off = (cluster * 4) as usize;
            if off + 4 > fat.len() {
                return 0;
            }
            u32::from_le_bytes(fat[off..off + 4].try_into().unwrap()) & 0x0FFF_FFFF
        }
    }
}

/// Wrapper kept for symmetry with the NTFS path; FAT and exFAT now share
/// the same `capture_fat` body since the divergence (boot-sector layout,
/// cluster sizing) is fully handled by `fat_plan` upstream.
pub fn capture_exfat(
    reader: &mut PartitionReader,
    stream: &mut phoenix_core::container::PartitionStreamWriter<'_>,
    extents: &[Extent],
    bitmap_hash: Option<String>,
) -> Result<(u64, Option<String>)> {
    capture_fat(reader, stream, extents, bitmap_hash)
}

pub fn estimate_fat_used(reader: &mut PartitionReader, exfat: bool) -> Result<u64> {
    let (extents, _hash, _bpc) = fat_plan(reader, exfat)?;
    Ok(extents.iter().map(|e| e.sector_count * SECTOR).sum())
}

/// Restore a FAT/exFAT partition. See [`crate::ntfs::restore_ntfs`] for
/// the byte-vs-chunk progress contract — same shape here. After the
/// data stream is on disk, dispatch to the correct boot-sector patcher
/// so a resize-during-restore actually produces a mountable volume
/// instead of a RAW partition. `fs` distinguishes FAT(12/16/32) from
/// exFAT, which have different on-disk size fields (and exFAT has a
/// boot-region checksum that has to be recomputed).
pub fn restore_fat(
    reader: &mut phoenix_core::container::PhnxReader,
    entry: &phoenix_core::container::PartitionIndexEntry,
    writer: &mut crate::raw::PartitionWriter,
    target_size: u64,
    fs: FilesystemKind,
    verify: bool,
    progress: Option<&ProgressHandle>,
    bytes_done: u64,
) -> Result<u64> {
    if entry.used_bytes > target_size {
        return Err(PhoenixError::PartitionTooSmall {
            partition_index: entry.index,
            target_size,
            required: entry.used_bytes,
        });
    }
    // FAT/exFAT shrink relocation is out of scope for this round (NTFS
    // only). Pre-flight in `validate_extents_fit` still refuses any
    // FAT/exFAT shrink whose used data lives past the boundary, so this
    // None is correct: by the time we get here, the source already fits.
    let written = crate::raw::restore_raw(reader, entry, writer, verify, progress, bytes_done, None)?;
    match fs {
        FilesystemKind::Fat => patch_fat_size(writer, target_size)?,
        FilesystemKind::Exfat => patch_exfat_size(writer, target_size)?,
        // Defensive: callers should only pass FilesystemKind::{Fat,Exfat},
        // but if someone routes a non-FAT FS through here we'd rather
        // skip the patch than corrupt the boot sector with FAT-shaped
        // writes.
        _ => {}
    }
    Ok(written)
}

/// Patch the FAT12/FAT16/FAT32 boot sector to match a resized target
/// partition.
///
/// FAT records the total sector count in either `BPB_TotSec16` (offset
/// 19, 2 bytes) for small volumes or `BPB_TotSec32` (offset 32, 4 bytes)
/// for FAT32 / oversized FAT16. Whichever field is in use must match
/// the partition the volume now lives in, otherwise Windows mounts the
/// volume as RAW. FAT32 additionally keeps a duplicate of sector 0 at
/// the LBA recorded in `BPB_BkBootSec` (offset 50, 2 bytes; typically
/// sector 6) — Windows compares the two at mount, so we patch both.
///
/// What this *doesn't* fix: the file allocation table itself was
/// allocated for the source volume's cluster count. For grows, the
/// trailing clusters past the source's count exist on disk but aren't
/// indexed by the FAT — `chkdsk /F` reconciles. For shrinks, the FAT
/// has entries describing clusters past the new boundary; chkdsk treats
/// those as cross-linked / lost and clears them.
fn patch_fat_size(writer: &mut crate::raw::PartitionWriter, new_size: u64) -> Result<()> {
    writer.flush()?;

    let mut sector = vec![0u8; 512];
    writer.read_at(0, &mut sector)?;
    if sector[510] != 0x55 || sector[511] != 0xAA {
        return Err(PhoenixError::Other(
            "expected FAT boot signature (0x55 0xAA) at LBA 0 of the restored partition; got \
             something else"
                .into(),
        ));
    }
    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]) as u64;
    if bytes_per_sector == 0 {
        return Err(PhoenixError::Other(
            "FAT boot sector reports zero bytes per sector".into(),
        ));
    }

    let new_total_sectors_u64 = new_size / bytes_per_sector;
    if new_total_sectors_u64 == 0 {
        return Err(PhoenixError::Other(format!(
            "target partition is too small ({} bytes) for a FAT volume",
            new_size
        )));
    }
    let new_total_u32: u32 = new_total_sectors_u64.try_into().map_err(|_| {
        PhoenixError::Other(
            "FAT cannot address volumes larger than 2^32 sectors; pick a smaller target size".into(),
        )
    })?;

    // Choose the size field FAT mounts will inspect: TotSec16 for small
    // FAT12/16 (preserved when in use), TotSec32 otherwise. Always
    // populate both consistently — historical Windows versions look at
    // whichever is non-zero, and disagreement between them is one of
    // the things `chkdsk` warns about.
    let fat_size_16 = u16::from_le_bytes([sector[22], sector[23]]);
    let prefer_total16 = fat_size_16 != 0 && new_total_u32 <= u16::MAX as u32;

    let cur_total16 = u16::from_le_bytes([sector[19], sector[20]]) as u32;
    let cur_total32 = u32::from_le_bytes(sector[32..36].try_into().unwrap());
    let cur_effective = if cur_total16 != 0 { cur_total16 } else { cur_total32 };
    if cur_effective == new_total_u32 {
        return Ok(());
    }

    if prefer_total16 {
        sector[19..21].copy_from_slice(&(new_total_u32 as u16).to_le_bytes());
        sector[32..36].copy_from_slice(&0u32.to_le_bytes());
    } else {
        sector[19..21].copy_from_slice(&0u16.to_le_bytes());
        sector[32..36].copy_from_slice(&new_total_u32.to_le_bytes());
    }

    let backup_idx = u16::from_le_bytes([sector[50], sector[51]]) as u64;
    writer.write_at(0, &sector)?;
    if backup_idx != 0 && backup_idx != 0xFFFF {
        let backup_offset = backup_idx * bytes_per_sector;
        if backup_offset + bytes_per_sector <= new_size {
            writer.write_at(backup_offset, &sector)?;
        }
    }

    tracing::info!(
        old_total_sectors = cur_effective,
        new_total_sectors = new_total_u32,
        new_size_bytes = new_size,
        used_total16 = prefer_total16,
        backup_boot_lba = backup_idx,
        "patched FAT boot sector (primary + backup) to match resized partition"
    );
    Ok(())
}

/// Patch the exFAT boot region to match a resized target partition.
///
/// exFAT is the most fiddly of the three: the layout has *two* boot
/// regions (primary at sectors 0-11, backup at 12-23, each 12 sectors
/// long), and sector 11 / 23 are checksum sectors computed over their
/// region with three specific bytes excluded. Patching just the size
/// fields without recomputing the checksum leaves Windows seeing a
/// region whose checksum doesn't match its contents, and the mount
/// path falls back to "format unknown" → RAW.
///
/// We modify two fields:
///   * `VolumeLength` (offset 72, 8 bytes) — total volume sectors.
///   * `ClusterCount` (offset 92, 4 bytes) — clusters in the heap,
///     derived from `(VolumeLength − ClusterHeapOffset) >>
///     SectorsPerClusterShift`.
/// Then recompute the 32-bit boot checksum and stamp it across all 128
/// `u32` slots of sector 11 / 23 (each checksum sector replicates the
/// scalar to fill its 512 bytes — the spec is explicit about this).
///
/// The bytes excluded from the checksum (per the exFAT spec) are
/// `VolumeFlags` at offsets 106-107 and `PercentInUse` at offset 112,
/// since both are mutated during normal use and including them would
/// invalidate the checksum on every write.
fn patch_exfat_size(writer: &mut crate::raw::PartitionWriter, new_size: u64) -> Result<()> {
    writer.flush()?;

    // Read the full primary boot region (sectors 0..=10) so we can
    // recompute the checksum without re-reading sectors 1..10 later.
    let mut region = vec![0u8; 11 * 512];
    writer.read_at(0, &mut region)?;
    if &region[3..11] != b"EXFAT   " {
        return Err(PhoenixError::Other(
            "expected exFAT boot signature ('EXFAT   ') at LBA 0 of the restored partition; got \
             something else"
                .into(),
        ));
    }

    let bytes_per_sector_shift = region[108];
    if !(9..=12).contains(&bytes_per_sector_shift) {
        return Err(PhoenixError::Other(format!(
            "exFAT boot sector reports invalid BytesPerSectorShift {bytes_per_sector_shift}"
        )));
    }
    let bytes_per_sector = 1u64 << bytes_per_sector_shift;
    let sectors_per_cluster_shift = region[109];
    if sectors_per_cluster_shift > 25 - bytes_per_sector_shift {
        return Err(PhoenixError::Other(format!(
            "exFAT boot sector reports invalid SectorsPerClusterShift {sectors_per_cluster_shift}"
        )));
    }
    let cluster_heap_offset =
        u32::from_le_bytes(region[88..92].try_into().unwrap()) as u64;

    let new_volume_sectors = new_size / bytes_per_sector;
    if new_volume_sectors <= cluster_heap_offset {
        return Err(PhoenixError::Other(format!(
            "target partition ({} bytes) is smaller than the exFAT cluster heap offset ({} \
             sectors); cannot resize",
            new_size, cluster_heap_offset
        )));
    }
    let new_cluster_count_u64 =
        (new_volume_sectors - cluster_heap_offset) >> sectors_per_cluster_shift;
    let new_cluster_count: u32 = new_cluster_count_u64.try_into().map_err(|_| {
        PhoenixError::Other(
            "exFAT cannot address >2^32 clusters; pick a smaller target size or larger cluster size"
                .into(),
        )
    })?;

    let cur_volume_sectors = u64::from_le_bytes(region[72..80].try_into().unwrap());
    let cur_cluster_count = u32::from_le_bytes(region[92..96].try_into().unwrap());
    if cur_volume_sectors == new_volume_sectors && cur_cluster_count == new_cluster_count {
        return Ok(());
    }

    region[72..80].copy_from_slice(&new_volume_sectors.to_le_bytes());
    region[92..96].copy_from_slice(&new_cluster_count.to_le_bytes());

    // exFAT boot checksum: 32-bit rotate-right-and-add over sectors 0..=10
    // with bytes 106, 107 (VolumeFlags) and 112 (PercentInUse) skipped.
    let region_len = 11 * bytes_per_sector as usize;
    let mut checksum: u32 = 0;
    for (i, &byte) in region[..region_len].iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        let hibit: u32 = if checksum & 1 == 1 { 0x8000_0000 } else { 0 };
        checksum = hibit.wrapping_add(checksum >> 1).wrapping_add(byte as u32);
    }

    // The checksum sector is the 32-bit value repeated to fill the entire
    // sector (spec says "all of the U32 entries shall contain the
    // checksum value"). For 512-byte sectors that's 128 copies.
    let mut checksum_sector = vec![0u8; bytes_per_sector as usize];
    let slot_count = (bytes_per_sector / 4) as usize;
    for slot in 0..slot_count {
        checksum_sector[slot * 4..slot * 4 + 4].copy_from_slice(&checksum.to_le_bytes());
    }

    let primary_boot = &region[..bytes_per_sector as usize];

    // Primary boot region: patch sector 0 + checksum sector 11.
    writer.write_at(0, primary_boot)?;
    writer.write_at(11 * bytes_per_sector, &checksum_sector)?;

    // Backup boot region: sectors 13..22 already mirror primary 1..10
    // (restore_raw streamed them verbatim from the source), so we only
    // need to patch sector 12 (backup boot sector) and 23 (backup
    // checksum). Both copies use the same checksum because they're
    // computed over identical content.
    let backup_base = 12 * bytes_per_sector;
    let backup_checksum = 23 * bytes_per_sector;
    if backup_checksum + bytes_per_sector <= new_size {
        writer.write_at(backup_base, primary_boot)?;
        writer.write_at(backup_checksum, &checksum_sector)?;
    } else {
        tracing::warn!(
            backup_checksum,
            new_size,
            "exFAT backup boot region falls outside the resized partition; skipping backup patch"
        );
    }

    tracing::info!(
        old_volume_sectors = cur_volume_sectors,
        new_volume_sectors,
        old_cluster_count = cur_cluster_count,
        new_cluster_count,
        new_size_bytes = new_size,
        "patched exFAT boot region (primary + backup, with recomputed boot checksum) to match \
         resized partition"
    );
    Ok(())
}
