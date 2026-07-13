use phoenix_core::container::{Extent, CHUNK_SIZE};
use phoenix_core::disk::FilesystemKind;
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::hash;

use crate::reader::BlockSource;

const SECTOR: u64 = 512;

#[derive(Clone, Copy, PartialEq)]
enum FatType {
    Fat12,
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
    reader: &mut impl BlockSource,
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
                // A used FAT extent that reads 0 bytes means the used-block map
                // points past the readable partition — never silently drop it.
                return Err(PhoenixError::Other(format!(
                    "capture_fat: read 0 bytes for used extent {ext_idx} at offset {} \
                     (extent spans {byte_len} bytes, {pos} captured; reader length {}). \
                     Refusing to write an incomplete backup.",
                    base + pos,
                    reader.length(),
                )));
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
    reader: &mut impl BlockSource,
    exfat: bool,
) -> Result<(Vec<Extent>, Option<String>, u32)> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let (fat_type, cluster_size, data_start, total_clusters, fat_bits) =
        parse_fat_boot(&boot, exfat)?;

    let fat_len = ((total_clusters + 2) * fat_bits as u64).div_ceil(8);
    // Raw volume/disk handles reject a ReadFile whose length isn't a multiple
    // of the sector size, and `fat_len` is only 4-byte-aligned. Read a
    // 512-rounded-up span into a padded buffer, then use just the FAT bytes.
    // (This is why FAT/exFAT capture failed with "read failed" while NTFS,
    // which reads its bitmap via FSCTL, did not.)
    let read_len = fat_len.div_ceil(SECTOR) * SECTOR;
    let mut fat_table = vec![0u8; read_len as usize];
    if read_len > 0 {
        // exFAT's FAT lives at the FatOffset sector, not the cluster heap;
        // for FAT12/16/32 `data_start` returned by `parse_fat_boot` IS the
        // post-FAT cluster region, but the FAT itself starts at
        // `(reserved sectors) * bytes_per_sector`. We re-derive the FAT
        // start here rather than threading another value out of the
        // parser to keep the parser's tuple shape compatible.
        let fat_byte_offset = match fat_type {
            FatType::Exfat => {
                u32::from_le_bytes(boot[80..84].try_into().unwrap()) as u64 * (1u64 << boot[108])
            }
            _ => {
                let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]) as u64;
                let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u64;
                reserved * bytes_per_sector
            }
        };
        reader.read_at(fat_byte_offset, &mut fat_table)?;
    }
    // Discard the sector-alignment padding so only real FAT bytes are hashed
    // and indexed.
    fat_table.truncate(fat_len as usize);

    let bitmap_hash = Some(hash::hash_hex(&fat_table));
    let extents = fat_used_extents(
        &fat_table,
        fat_type,
        cluster_size,
        data_start,
        total_clusters,
    );
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
        let cluster_count = u32::from_le_bytes(boot[92..96].try_into().unwrap()) as u64;
        // `data_start` is the byte offset where cluster #2 lives —
        // i.e. `ClusterHeapOffset (in sectors) * bytes_per_sector`.
        // The previous formula (`fat_offset + cluster_count * cluster_size`)
        // was the position of the *end* of the cluster heap and had no
        // physical meaning as a "data start". The 4 TB exFAT case with
        // those bugs made byte_start values reach trillions of bytes
        // beyond the partition end.
        let cluster_heap_offset = u32::from_le_bytes(boot[88..92].try_into().unwrap()) as u64;
        let data_start = cluster_heap_offset * bytes_per_sector;
        return Ok((FatType::Exfat, cluster_size, data_start, cluster_count, 32));
    }

    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(PhoenixError::Other("invalid FAT boot signature".into()));
    }
    let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]) as u64;
    let sectors_per_cluster = boot[13] as u64;
    if bytes_per_sector == 0 || sectors_per_cluster == 0 {
        return Err(PhoenixError::Other(
            "FAT boot sector reports zero bytes-per-sector or sectors-per-cluster".into(),
        ));
    }
    let cluster_size = bytes_per_sector * sectors_per_cluster;
    let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u64;
    let fat_count = boot[16] as u64;
    let root_entry_count = u16::from_le_bytes([boot[17], boot[18]]) as u64;

    // FATSz and total-sector counts each have a 16-bit and a 32-bit field;
    // the 16-bit form wins when non-zero (FAT12/16), otherwise the 32-bit
    // form is authoritative (FAT32 / oversized FAT16).
    let fat_size_16 = u16::from_le_bytes([boot[22], boot[23]]) as u64;
    let fat_size_32 = u32::from_le_bytes(boot[36..40].try_into().unwrap()) as u64;
    let fat_size = if fat_size_16 != 0 {
        fat_size_16
    } else {
        fat_size_32
    };
    let total_sectors = if boot[19] != 0 || boot[20] != 0 {
        u16::from_le_bytes([boot[19], boot[20]]) as u64
    } else {
        u32::from_le_bytes(boot[32..36].try_into().unwrap()) as u64
    };

    // Root-directory sectors: 0 for FAT32 (root is a normal cluster chain),
    // otherwise `ceil(root_entry_count * 32 / bytes_per_sector)`. The data
    // region (cluster 2) begins after reserved + all FATs + the root dir —
    // the previous code omitted the root-dir region, which offset every
    // FAT12/16 extent's byte position by the root-directory size.
    let root_dir_sectors = (root_entry_count * 32).div_ceil(bytes_per_sector);
    let data_start_sector = reserved + fat_count * fat_size + root_dir_sectors;
    let data_sectors = total_sectors.saturating_sub(data_start_sector);
    let cluster_count = data_sectors / sectors_per_cluster;

    // FAT type is determined SOLELY by the count of data clusters (per the
    // Microsoft FAT spec), never by which size field is populated. The old
    // code keyed on `fat_size_16 == 0` and always produced FAT16, so FAT12
    // was mis-decoded as FAT16 (2-byte entries) and FAT32's cluster count
    // was read from `BPB_RootClus` (~2), capturing almost no data.
    let (fat_type, fat_bits) = if cluster_count < 4085 {
        (FatType::Fat12, 12u32)
    } else if cluster_count < 65525 {
        (FatType::Fat16, 16)
    } else {
        (FatType::Fat32, 32)
    };
    let data_start = data_start_sector * bytes_per_sector;
    Ok((fat_type, cluster_size, data_start, cluster_count, fat_bits))
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
        // Any non-zero FAT entry means the cluster is allocated: mid-chain
        // links, end-of-chain markers (0xFF8..0xFFF etc.), and bad-cluster
        // markers all count as used. Only 0 (free) is skipped. The previous
        // check excluded the single EOC value 0xFFFF / 0x0FFF_FFFF, which
        // dropped the *final* cluster of every file (and missed the other
        // EOC values 0xFF8..0xFFE entirely) from the captured image.
        if entry != 0 {
            used.push(c);
        }
    }
    used_clusters_to_extents(&used, cluster_size, data_start)
}

/// Turn a sorted list of used cluster numbers (2-based, as in FAT/exFAT) into
/// captured [`Extent`]s, always prepending the reserved region `[0, data_start)`.
///
/// The reserved region — boot sector(s), the FAT copies, and (for FAT12/16)
/// the root directory — is filesystem metadata that must ALWAYS be captured;
/// without it a restored volume has no boot sector and mounts as RAW. NTFS
/// gets this for free because its bitmap marks the metadata clusters used, but
/// the FAT/exFAT cluster scans cover only the data (cluster-heap) region, so we
/// add the reserved region explicitly. For exFAT the cluster-heap allocation
/// bitmap, up-case table, and root directory sit *inside* the heap and so are
/// already covered by `used`.
fn used_clusters_to_extents(used: &[u64], cluster_size: u64, data_start: u64) -> Vec<Extent> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    if data_start > 0 {
        ranges.push((0, data_start));
    }
    if !used.is_empty() {
        let mut start = used[0];
        let mut prev = used[0];
        let push_run = |ranges: &mut Vec<(u64, u64)>, start: u64, prev: u64| {
            let byte_start = data_start + (start - 2) * cluster_size;
            let byte_end = data_start + (prev - 2 + 1) * cluster_size;
            ranges.push((byte_start, byte_end));
        };
        for &c in used.iter().skip(1) {
            if c == prev + 1 {
                prev = c;
            } else {
                push_run(&mut ranges, start, prev);
                start = c;
                prev = c;
            }
        }
        push_run(&mut ranges, start, prev);
    }

    // Coalesce adjacent/overlapping ranges (the reserved region and the first
    // data cluster are adjacent when cluster 2 is allocated).
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }

    if merged.is_empty() {
        // A degenerate volume with no reserved region and no data: capture a
        // minimal extent so the stream isn't empty.
        return vec![Extent {
            start_sector: 0,
            sector_count: (cluster_size / SECTOR).max(8),
        }];
    }

    merged
        .into_iter()
        .map(|(s, e)| Extent {
            start_sector: s / SECTOR,
            sector_count: (e - s) / SECTOR,
        })
        .collect()
}

/// exFAT geometry pulled from the boot sector — everything [`exfat_plan`]
/// needs to locate the FAT, the root directory, and the cluster heap.
struct ExfatGeom {
    cluster_size: u64,
    cluster_count: u64,
    /// Byte offset of cluster #2 (the start of the cluster heap).
    data_start: u64,
    fat_byte_offset: u64,
    fat_byte_len: u64,
    root_dir_first_cluster: u64,
}

fn parse_exfat_geom(boot: &[u8]) -> Result<ExfatGeom> {
    if boot.len() < 512 || &boot[3..11] != b"EXFAT   " {
        return Err(PhoenixError::Other("not exFAT".into()));
    }
    let bps_shift = boot[108];
    let spc_shift = boot[109];
    if !(9..=12).contains(&bps_shift) {
        return Err(PhoenixError::Other(format!(
            "exFAT BytesPerSectorShift {bps_shift} out of range (expected 9..=12)"
        )));
    }
    if spc_shift as u32 > 25 - bps_shift as u32 {
        return Err(PhoenixError::Other(format!(
            "exFAT SectorsPerClusterShift {spc_shift} out of range"
        )));
    }
    let bytes_per_sector = 1u64 << bps_shift;
    let cluster_size = bytes_per_sector << spc_shift;
    let fat_offset = u32::from_le_bytes(boot[80..84].try_into().unwrap()) as u64;
    let fat_length = u32::from_le_bytes(boot[84..88].try_into().unwrap()) as u64;
    let cluster_heap_offset = u32::from_le_bytes(boot[88..92].try_into().unwrap()) as u64;
    let cluster_count = u32::from_le_bytes(boot[92..96].try_into().unwrap()) as u64;
    let root_dir_first_cluster = u32::from_le_bytes(boot[96..100].try_into().unwrap()) as u64;
    if cluster_count == 0 || cluster_heap_offset == 0 || fat_length == 0 {
        return Err(PhoenixError::Other(
            "exFAT boot sector reports zero cluster_count / cluster_heap_offset / fat_length"
                .into(),
        ));
    }
    if root_dir_first_cluster < 2 || root_dir_first_cluster > cluster_count + 1 {
        return Err(PhoenixError::Other(format!(
            "exFAT FirstClusterOfRootDirectory {root_dir_first_cluster} out of range"
        )));
    }
    Ok(ExfatGeom {
        cluster_size,
        cluster_count,
        data_start: cluster_heap_offset * bytes_per_sector,
        fat_byte_offset: fat_offset * bytes_per_sector,
        fat_byte_len: fat_length * bytes_per_sector,
        root_dir_first_cluster,
    })
}

/// The next-cluster value for `cluster` from the exFAT FAT. exFAT FAT entries
/// are full 32-bit (unlike FAT32's 28-bit); end-of-chain is `0xFFFF_FFFF`.
fn exfat_fat_next(fat: &[u8], cluster: u64) -> Option<u64> {
    let idx = (cluster * 4) as usize;
    let b = fat.get(idx..idx + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64)
}

/// Read a FAT-chained cluster stream (the root directory) starting at
/// `first_cluster`, stopping at end-of-chain / an out-of-range link or after
/// `max_clusters` (a runaway guard). exFAT's root directory is always
/// FAT-chained (never `NoFatChain`), so following the chain is spec-correct.
fn read_exfat_chain(
    reader: &mut impl BlockSource,
    geom: &ExfatGeom,
    fat: &[u8],
    first_cluster: u64,
    max_clusters: u64,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut cluster = first_cluster;
    let mut count = 0u64;
    while cluster >= 2 && cluster <= geom.cluster_count + 1 && count < max_clusters {
        let off = geom.data_start + (cluster - 2) * geom.cluster_size;
        let mut buf = vec![0u8; geom.cluster_size as usize];
        let n = reader.read_at(off, &mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        count += 1;
        match exfat_fat_next(fat, cluster) {
            Some(next) if next >= 2 && next <= geom.cluster_count + 1 => cluster = next,
            // 0xFFFF_FFFF (EOC), bad-cluster markers, or anything out of range
            // terminates the chain.
            _ => break,
        }
    }
    Ok(out)
}

/// Read exactly `data_length` bytes of a stream (the allocation bitmap)
/// starting at `first_cluster`. Follows the FAT chain when it points to a
/// valid next cluster; otherwise assumes the stream is contiguous. Real exFAT
/// allocation bitmaps are laid out contiguously (and some formatters leave
/// their FAT entries unset, `NoFatChain`-style), so the contiguous fallback is
/// the common path.
fn read_exfat_stream_by_len(
    reader: &mut impl BlockSource,
    geom: &ExfatGeom,
    fat: &[u8],
    first_cluster: u64,
    data_length: u64,
) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(data_length as usize);
    let mut cluster = first_cluster;
    let mut guard = 0u64;
    let max_iters = geom.cluster_count + 2;
    while (out.len() as u64) < data_length {
        if cluster < 2 || cluster > geom.cluster_count + 1 {
            break;
        }
        let off = geom.data_start + (cluster - 2) * geom.cluster_size;
        let mut buf = vec![0u8; geom.cluster_size as usize];
        let n = reader.read_at(off, &mut buf)?;
        if n == 0 {
            break;
        }
        let remaining = (data_length - out.len() as u64) as usize;
        let take = remaining.min(n);
        out.extend_from_slice(&buf[..take]);
        cluster = match exfat_fat_next(fat, cluster) {
            Some(next) if next >= 2 && next <= geom.cluster_count + 1 => next,
            _ => cluster + 1,
        };
        guard += 1;
        if guard > max_iters {
            break;
        }
    }
    out.truncate(data_length as usize);
    Ok(out)
}

/// Scan root-directory entries (32 bytes each) for the in-use Allocation
/// Bitmap entry (type `0x81`, BitmapFlags bit 0 clear = the first/only
/// bitmap on a non-TexFAT volume) and return its `(first_cluster, data_length)`.
fn find_exfat_alloc_bitmap(root: &[u8]) -> Result<(u64, u64)> {
    let mut off = 0usize;
    while off + 32 <= root.len() {
        let entry_type = root[off];
        if entry_type == 0x00 {
            break; // end-of-directory marker
        }
        if entry_type == 0x81 && root[off + 1] & 1 == 0 {
            let first_cluster =
                u32::from_le_bytes(root[off + 20..off + 24].try_into().unwrap()) as u64;
            let data_length = u64::from_le_bytes(root[off + 24..off + 32].try_into().unwrap());
            return Ok((first_cluster, data_length));
        }
        off += 32;
    }
    Err(PhoenixError::Other(
        "exFAT allocation bitmap directory entry (0x81) not found in root directory".into(),
    ))
}

/// Build used-cluster extents from the exFAT allocation bitmap. Bit `i`
/// (LSB-first) covers cluster `i + 2`; a set bit means allocated. This is
/// authoritative for ALL allocated clusters — including `NoFatChain`
/// contiguous files, which a FAT scan misses entirely.
fn exfat_used_extents(
    bitmap: &[u8],
    cluster_size: u64,
    data_start: u64,
    cluster_count: u64,
) -> Vec<Extent> {
    let mut used = Vec::new();
    for i in 0..cluster_count {
        let byte = (i / 8) as usize;
        let bit = (i % 8) as u32;
        if byte < bitmap.len() && (bitmap[byte] >> bit) & 1 == 1 {
            used.push(i + 2);
        }
    }
    used_clusters_to_extents(&used, cluster_size, data_start)
}

/// exFAT used-block plan: derive used-cluster extents from the **allocation
/// bitmap** (not the FAT). Mirrors [`fat_plan`]/[`crate::ntfs::ntfs_plan`] in
/// shape — returns `(extents, bitmap_hash, bytes_per_cluster)` — so the
/// planned extents become the manifest's authoritative extent table.
///
/// Why the bitmap and not the FAT: exFAT stores contiguous files with the
/// `NoFatChain` flag, meaning their clusters are NOT linked in the FAT (their
/// FAT entries read as free). A FAT scan therefore silently drops every
/// contiguous file — the reason exFAT was previously captured raw. The
/// allocation bitmap marks every allocated cluster regardless.
pub fn exfat_plan(reader: &mut impl BlockSource) -> Result<(Vec<Extent>, Option<String>, u32)> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let geom = parse_exfat_geom(&boot)?;

    let mut fat = vec![0u8; geom.fat_byte_len as usize];
    reader.read_at(geom.fat_byte_offset, &mut fat)?;

    // The Allocation Bitmap / Up-case / Volume Label entries sit at the very
    // start of the root directory, so a modest cluster cap covers them while
    // still tolerating a fragmented root.
    let root = read_exfat_chain(reader, &geom, &fat, geom.root_dir_first_cluster, 256)?;
    let (bitmap_first_cluster, bitmap_len) = find_exfat_alloc_bitmap(&root)?;
    if bitmap_first_cluster < 2 || bitmap_first_cluster > geom.cluster_count + 1 {
        return Err(PhoenixError::Other(format!(
            "exFAT allocation bitmap first cluster {bitmap_first_cluster} out of range"
        )));
    }
    let expected_len = geom.cluster_count.div_ceil(8);
    if bitmap_len < expected_len {
        return Err(PhoenixError::Other(format!(
            "exFAT allocation bitmap DataLength {bitmap_len} < {expected_len} needed for {} clusters",
            geom.cluster_count
        )));
    }
    let bitmap = read_exfat_stream_by_len(reader, &geom, &fat, bitmap_first_cluster, bitmap_len)?;
    if (bitmap.len() as u64) < expected_len {
        return Err(PhoenixError::Other(format!(
            "exFAT allocation bitmap read short: got {} bytes, need {expected_len}",
            bitmap.len()
        )));
    }

    let extents = exfat_used_extents(
        &bitmap,
        geom.cluster_size,
        geom.data_start,
        geom.cluster_count,
    );
    let bitmap_hash = Some(hash::hash_hex(&bitmap[..expected_len as usize]));
    let bytes_per_cluster: u32 = geom.cluster_size.try_into().unwrap_or(u32::MAX);
    Ok((extents, bitmap_hash, bytes_per_cluster))
}

fn fat_cluster_value(fat: &[u8], cluster: u64, fat_type: FatType) -> u32 {
    match fat_type {
        FatType::Fat12 => {
            // Each entry is 12 bits, packed two-per-three-bytes. Entry N
            // begins at byte offset `N + N/2` (= floor(1.5 * N)); even
            // clusters take the low 12 bits of the little-endian u16 there,
            // odd clusters take the high 12 bits.
            let off = (cluster + cluster / 2) as usize;
            if off + 2 > fat.len() {
                return 0;
            }
            let raw = u16::from_le_bytes([fat[off], fat[off + 1]]);
            if cluster & 1 == 1 {
                (raw >> 4) as u32
            } else {
                (raw & 0x0FFF) as u32
            }
        }
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
    reader: &mut impl BlockSource,
    stream: &mut phoenix_core::container::PartitionStreamWriter<'_>,
    extents: &[Extent],
    bitmap_hash: Option<String>,
) -> Result<(u64, Option<String>)> {
    capture_fat(reader, stream, extents, bitmap_hash)
}

pub fn estimate_fat_used(reader: &mut impl BlockSource, exfat: bool) -> Result<u64> {
    // exFAT used-blocks come from the allocation bitmap (the FAT misses
    // NoFatChain files); FAT12/16/32 come from the FAT itself.
    let (extents, _hash, _bpc) = if exfat {
        exfat_plan(reader)?
    } else {
        fat_plan(reader, false)?
    };
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
    opts: crate::raw::RestoreOpts<'_>,
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
    let written = crate::raw::restore_raw(reader, entry, writer, opts, None, target_size)?;
    finalize_fat_partition(writer, target_size, fs)?;
    Ok(written)
}

/// Post-stream FAT/exFAT finalization shared by restore and clone: patch the
/// boot-sector size fields (and, for exFAT, recompute the boot checksum) so a
/// resized target mounts instead of coming up RAW. Source-agnostic — the bytes
/// may have arrived from a `.phnx` (restore) or straight off another disk
/// (clone).
pub fn finalize_fat_partition(
    writer: &mut crate::raw::PartitionWriter,
    target_size: u64,
    fs: FilesystemKind,
) -> Result<()> {
    match fs {
        FilesystemKind::Fat => patch_fat_size(writer, target_size)?,
        FilesystemKind::Exfat => patch_exfat_size(writer, target_size)?,
        // Defensive: callers should only pass FilesystemKind::{Fat,Exfat},
        // but if someone routes a non-FAT FS through here we'd rather
        // skip the patch than corrupt the boot sector with FAT-shaped
        // writes.
        _ => {}
    }
    Ok(())
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
            "FAT cannot address volumes larger than 2^32 sectors; pick a smaller target size"
                .into(),
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
    let cur_effective = if cur_total16 != 0 {
        cur_total16
    } else {
        cur_total32
    };
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
///
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
    let cluster_heap_offset = u32::from_le_bytes(region[88..92].try_into().unwrap()) as u64;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic FAT12/16/32 boot sector from a geometry spec so we
    /// can assert `parse_fat_boot` derives the right type and cluster count.
    #[allow(clippy::too_many_arguments)]
    fn make_boot(
        bytes_per_sector: u16,
        sectors_per_cluster: u8,
        reserved: u16,
        fat_count: u8,
        root_entry_count: u16,
        fat_size_16: u16,
        total_sectors_16: u16,
        fat_size_32: u32,
        total_sectors_32: u32,
    ) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[11..13].copy_from_slice(&bytes_per_sector.to_le_bytes());
        b[13] = sectors_per_cluster;
        b[14..16].copy_from_slice(&reserved.to_le_bytes());
        b[16] = fat_count;
        b[17..19].copy_from_slice(&root_entry_count.to_le_bytes());
        b[19..21].copy_from_slice(&total_sectors_16.to_le_bytes());
        b[22..24].copy_from_slice(&fat_size_16.to_le_bytes());
        b[32..36].copy_from_slice(&total_sectors_32.to_le_bytes());
        b[36..40].copy_from_slice(&fat_size_32.to_le_bytes());
        // BPB_RootClus (offset 44) is 2 on a real FAT32 volume — the value
        // the old buggy code mistook for the cluster count. Set it so the
        // regression test proves we no longer read it.
        b[44..48].copy_from_slice(&2u32.to_le_bytes());
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn parse_fat16_geometry() {
        // 5000 data clusters → FAT16 range [4085, 65525).
        let boot = make_boot(512, 4, 4, 2, 512, 20, 20076, 0, 0);
        let (ft, cluster_size, data_start, count, bits) = parse_fat_boot(&boot, false).unwrap();
        assert!(ft == FatType::Fat16);
        assert_eq!(bits, 16);
        assert_eq!(cluster_size, 2048);
        assert_eq!(count, 5000);
        // data_start = (reserved 4 + 2*fat 20 + root_dir 32) * 512.
        assert_eq!(data_start, (4 + 40 + 32) * 512);
    }

    #[test]
    fn parse_fat12_geometry() {
        // 2000 data clusters → FAT12 range (< 4085); must NOT be read as FAT16.
        let boot = make_boot(512, 1, 1, 2, 224, 6, 2027, 0, 0);
        let (ft, _cs, _ds, count, bits) = parse_fat_boot(&boot, false).unwrap();
        assert!(ft == FatType::Fat12);
        assert_eq!(bits, 12);
        assert_eq!(count, 2000);
    }

    #[test]
    fn parse_fat32_cluster_count_not_rootclus() {
        // 70000 data clusters → FAT32. The old code returned ~2 here
        // (BPB_RootClus), which is the whole "captures almost no data" bug.
        let boot = make_boot(512, 8, 32, 2, 0, 0, 0, 547, 561126);
        let (ft, _cs, _ds, count, bits) = parse_fat_boot(&boot, false).unwrap();
        assert!(ft == FatType::Fat32);
        assert_eq!(bits, 32);
        assert_eq!(count, 70000);
    }

    fn set_fat16(fat: &mut [u8], cluster: u64, value: u16) {
        let off = (cluster * 2) as usize;
        fat[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn set_fat12(fat: &mut [u8], cluster: u64, value: u16) {
        let off = (cluster + cluster / 2) as usize;
        let v = value & 0x0FFF;
        if cluster & 1 == 1 {
            fat[off] = (fat[off] & 0x0F) | ((v << 4) as u8 & 0xF0);
            fat[off + 1] = (v >> 4) as u8;
        } else {
            fat[off] = (v & 0xFF) as u8;
            fat[off + 1] = (fat[off + 1] & 0xF0) | ((v >> 8) as u8 & 0x0F);
        }
    }

    #[test]
    fn fat12_packed_reads_roundtrip() {
        let mut fat = vec![0u8; 64];
        set_fat12(&mut fat, 2, 0x123); // even
        set_fat12(&mut fat, 3, 0xFFF); // odd, EOC
        set_fat12(&mut fat, 4, 0xABC); // even
        assert_eq!(fat_cluster_value(&fat, 2, FatType::Fat12), 0x123);
        assert_eq!(fat_cluster_value(&fat, 3, FatType::Fat12), 0xFFF);
        assert_eq!(fat_cluster_value(&fat, 4, FatType::Fat12), 0xABC);
    }

    #[test]
    fn eoc_terminated_clusters_are_captured() {
        // Chain 2->3->4(EOC) plus a lone cluster 6(EOC); cluster 5 is free.
        // The final cluster of each file (the EOC-marked one) must appear
        // in the extents — this is the "FAT file tails dropped" regression.
        let cluster_size = 2048u64;
        let data_start = 76 * 512u64;
        let total_clusters = 6u64;
        let mut fat = vec![0u8; (total_clusters as usize + 2) * 2 + 16];
        set_fat16(&mut fat, 2, 3);
        set_fat16(&mut fat, 3, 4);
        set_fat16(&mut fat, 4, 0xFFFF); // EOC
        set_fat16(&mut fat, 6, 0xFFFF); // EOC (single-cluster file)

        let extents = fat_used_extents(
            &fat,
            FatType::Fat16,
            cluster_size,
            data_start,
            total_clusters,
        );
        // The reserved region [0, data_start) is always captured, and it
        // coalesces with the adjacent cluster-2 run. So: run 1 = reserved +
        // clusters 2,3,4; run 2 = cluster 6 (after the cluster-5 free gap).
        assert_eq!(extents.len(), 2, "expected two coalesced runs");
        assert_eq!(
            extents[0].start_sector, 0,
            "reserved region must be captured"
        );
        assert_eq!(
            extents[0].sector_count,
            (data_start + 3 * cluster_size) / 512
        );
        // Run 2: cluster 6 only.
        assert_eq!(
            extents[1].start_sector,
            (data_start + 4 * cluster_size) / 512
        );
        assert_eq!(extents[1].sector_count, cluster_size / 512);
    }

    #[test]
    fn bad_and_reserved_markers_count_as_used() {
        // Bad-cluster (0xFFF7) and reserved (0xFFF0) markers are non-zero,
        // so they must be captured; only 0 (free) is skipped.
        let cluster_size = 512u64;
        let total_clusters = 3u64;
        let mut fat = vec![0u8; (total_clusters as usize + 2) * 2 + 8];
        set_fat16(&mut fat, 2, 0xFFF7); // bad
        set_fat16(&mut fat, 3, 0); // free
        set_fat16(&mut fat, 4, 0xFFF0); // reserved
        let extents = fat_used_extents(&fat, FatType::Fat16, cluster_size, 0, total_clusters);
        // Clusters 2 and 4 used, 3 free → two separate runs.
        assert_eq!(extents.len(), 2);
    }

    // ---- exFAT allocation-bitmap plan ----

    /// Build a minimal but valid-enough exFAT image (512-byte sectors,
    /// 1-sector clusters) whose allocation bitmap marks clusters 2, 3, 5, 6
    /// used — where clusters 5 & 6 are a **`NoFatChain` contiguous file**
    /// (their FAT entries are 0/free). A FAT scan would miss 5 & 6; the
    /// bitmap scan must catch them.
    ///
    /// Layout: boot @0, FAT @sector 24 (1 sector), cluster heap @sector 32.
    /// Root dir = cluster 2, allocation bitmap = cluster 3.
    fn build_exfat_image() -> Vec<u8> {
        const SEC: usize = 512;
        let fat_offset_sec = 24usize;
        let heap_offset_sec = 32usize;
        let cluster_count = 10u32;
        // Cover the whole heap (clusters 2..=11): heap end = (32 + 10) sectors.
        let total_sectors = heap_offset_sec + cluster_count as usize; // 42
        let mut img = vec![0u8; total_sectors * SEC];

        // Boot sector.
        img[3..11].copy_from_slice(b"EXFAT   ");
        img[80..84].copy_from_slice(&(fat_offset_sec as u32).to_le_bytes());
        img[84..88].copy_from_slice(&1u32.to_le_bytes()); // FatLength = 1 sector
        img[88..92].copy_from_slice(&(heap_offset_sec as u32).to_le_bytes());
        img[92..96].copy_from_slice(&cluster_count.to_le_bytes());
        img[96..100].copy_from_slice(&2u32.to_le_bytes()); // root dir @ cluster 2
        img[108] = 9; // BytesPerSectorShift → 512
        img[109] = 0; // SectorsPerClusterShift → 1 sector/cluster

        // FAT (sector 24): entries are 4 bytes each.
        let fat_base = fat_offset_sec * SEC;
        let set_fat = |img: &mut [u8], cluster: usize, val: u32| {
            let o = fat_base + cluster * 4;
            img[o..o + 4].copy_from_slice(&val.to_le_bytes());
        };
        set_fat(&mut img, 0, 0xFFFF_FFF8); // media
        set_fat(&mut img, 1, 0xFFFF_FFFF);
        set_fat(&mut img, 2, 0xFFFF_FFFF); // root dir: single-cluster EOC
        set_fat(&mut img, 3, 0xFFFF_FFFF); // bitmap: single-cluster EOC
                                           // Clusters 5 & 6 deliberately left 0 (NoFatChain contiguous file).

        let cluster_off = |c: usize| (heap_offset_sec + (c - 2)) * SEC;

        // Root directory (cluster 2): one Allocation Bitmap entry, then EOD.
        let rd = cluster_off(2);
        img[rd] = 0x81; // Allocation Bitmap
        img[rd + 1] = 0x00; // flags: first/only bitmap
        img[rd + 20..rd + 24].copy_from_slice(&3u32.to_le_bytes()); // FirstCluster = 3
        img[rd + 24..rd + 32].copy_from_slice(&2u64.to_le_bytes()); // DataLength = 2 bytes
                                                                    // (Byte at rd + 32 is already 0x00 = end-of-directory.)

        // Allocation bitmap (cluster 3): bit i → cluster i+2.
        // Set clusters 2,3,5,6 → bits 0,1,3,4 → 0b0001_1011 = 0x1B.
        let bm = cluster_off(3);
        img[bm] = 0x1B;
        img[bm + 1] = 0x00;

        img
    }

    #[test]
    fn exfat_plan_captures_nofatchain_file_via_bitmap() {
        use crate::reader::MemoryBlockSource;
        let img = build_exfat_image();
        let mut src = MemoryBlockSource::new(img);
        let (extents, bitmap_hash, bytes_per_cluster) = exfat_plan(&mut src).unwrap();
        assert_eq!(bytes_per_cluster, 512);
        assert!(bitmap_hash.is_some());

        // data_start = 32 sectors, cluster_size = 512.
        // Used clusters 2,3,5,6 → reserved [0,32) + clusters 2-3 coalesce to
        // sectors [0, 34); clusters 5-6 → sectors [35, 37).
        assert_eq!(
            extents.len(),
            2,
            "expected reserved+2-3 run and the 5-6 run"
        );
        assert_eq!(extents[0].start_sector, 0);
        assert_eq!(extents[0].sector_count, 34);
        // The NoFatChain file (clusters 5,6) MUST be captured — this is the
        // whole point of using the bitmap instead of the FAT.
        assert_eq!(extents[1].start_sector, 35);
        assert_eq!(extents[1].sector_count, 2);
    }

    #[test]
    fn exfat_used_extents_marks_only_allocated_bits() {
        // clusters 2 and 4 used (bits 0 and 2), cluster 3 free.
        let bitmap = [0b0000_0101u8];
        let extents = exfat_used_extents(&bitmap, 512, 32 * 512, 3);
        // reserved [0,32) coalesces with cluster 2 → sectors [0, 33);
        // cluster 4 (bit2) → sectors [34, 35).
        assert_eq!(extents.len(), 2);
        assert_eq!(extents[0].start_sector, 0);
        assert_eq!(extents[0].sector_count, 33);
        assert_eq!(extents[1].start_sector, 34);
        assert_eq!(extents[1].sector_count, 1);
    }

    #[test]
    fn exfat_plan_rejects_non_exfat() {
        use crate::reader::MemoryBlockSource;
        let mut src = MemoryBlockSource::new(vec![0u8; 4096]);
        assert!(exfat_plan(&mut src).is_err());
    }
}
