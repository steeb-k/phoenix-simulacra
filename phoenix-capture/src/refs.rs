//! ReFS (Resilient File System) capture planning.
//!
//! ReFS has no BPB and no `$Bitmap` file we could parse — its allocators
//! (Container Allocator / Schema tables) are undocumented and their layout
//! shifts between on-disk format versions (1.x, 3.x, 3.14…), so this module
//! deliberately does NOT read ReFS metadata beyond the boot sector. Used-block
//! detection instead goes through `FSCTL_GET_VOLUME_BITMAP`, which the ReFS
//! driver answers exactly like NTFS does (the defrag IOCTLs are
//! filesystem-agnostic). When the IOCTL is unavailable (raw physical-disk
//! handles, WinPE) we degrade to full-partition capture — correct, just
//! wasteful — mirroring the NTFS fallback.
//!
//! Boot sector layout (reverse-engineered, stable across ReFS 1.x and 3.x):
//!
//! ```text
//! 0x00  3 bytes   jump/zero
//! 0x03  8 bytes   "ReFS\0\0\0\0"      (NUL-padded, unlike NTFS's OEM field)
//! 0x10  4 bytes   "FSRS"              superblock header magic
//! 0x14  u16       structure length
//! 0x16  u16       checksum
//! 0x18  u64       total sectors
//! 0x20  u32       bytes per sector
//! 0x24  u32       sectors per cluster
//! 0x28  u8        major version
//! 0x29  u8        minor version
//! ```
//!
//! Resize policy: ReFS is never shrunk (no offline shrink exists and we will
//! not rewrite allocator state we can't parse) — `partition_allows_resize`
//! excludes it and `restore_refs` refuses a smaller target outright. Grow is
//! handled the NTFS way: leave the boot sector alone, then let
//! `FSCTL_EXTEND_VOLUME` grow the mounted volume (ReFS supports online
//! extension).

use std::io::{Seek, SeekFrom};

use byteorder::{LittleEndian, ReadBytesExt};
use phoenix_core::container::Extent;
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::hash;

use crate::reader::BlockSource;

const SECTOR_SIZE: u64 = 512;

#[derive(Debug)]
pub(crate) struct RefsBootSector {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub total_sectors: u64,
    pub major_version: u8,
    pub minor_version: u8,
}

pub(crate) fn parse_boot_sector(boot: &[u8]) -> Result<RefsBootSector> {
    if boot.len() < 512 || &boot[3..11] != b"ReFS\0\0\0\0" || &boot[16..20] != b"FSRS" {
        return Err(PhoenixError::Other("not ReFS".into()));
    }
    let mut cur = std::io::Cursor::new(boot);
    cur.seek(SeekFrom::Start(0x18))?;
    let total_sectors = cur.read_u64::<LittleEndian>()?;
    let bytes_per_sector = cur.read_u32::<LittleEndian>()?;
    let sectors_per_cluster = cur.read_u32::<LittleEndian>()?;
    let major_version = cur.read_u8()?;
    let minor_version = cur.read_u8()?;

    // Sanity-gate the geometry before any arithmetic. A corrupted FSRS block
    // that still carries both magics must not send a zero or absurd cluster
    // size into the extent math (division by zero / a bitmap sized in
    // exabytes). ReFS itself only formats 4K and 64K clusters on 512/4Kn
    // sectors, but accept any power-of-two in the plausible device range so
    // future format revisions don't get misread as corruption.
    let plausible_sector = matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096);
    let plausible_cluster = sectors_per_cluster != 0
        && sectors_per_cluster.is_power_of_two()
        && (bytes_per_sector as u64 * sectors_per_cluster as u64) <= 16 * 1024 * 1024;
    if !plausible_sector || !plausible_cluster || total_sectors == 0 {
        return Err(PhoenixError::Other(format!(
            "ReFS boot sector has implausible geometry \
             (bytes_per_sector={bytes_per_sector}, sectors_per_cluster={sectors_per_cluster}, \
             total_sectors={total_sectors})"
        )));
    }
    Ok(RefsBootSector {
        bytes_per_sector,
        sectors_per_cluster,
        total_sectors,
        major_version,
        minor_version,
    })
}

/// Total cluster count, floored — same rationale as the NTFS twin: a phantom
/// rounded-up trailing cluster would point capture past the readable end of
/// the volume.
fn total_clusters_for(bs: &RefsBootSector, cluster_size: u64) -> u64 {
    let volume_bytes = bs.total_sectors.saturating_mul(bs.bytes_per_sector as u64);
    volume_bytes / cluster_size
}

fn read_bitmap(reader: &mut impl BlockSource, total_clusters: u64) -> Vec<u8> {
    let bitmap_bytes = total_clusters.div_ceil(8) as usize;
    // Preferred path: FSCTL_GET_VOLUME_BITMAP via the volume handle. The ReFS
    // driver implements the defrag IOCTLs, so this is exactly as reliable as
    // it is on NTFS — and the only allocation source we trust (see module
    // docs for why we refuse to parse ReFS allocators ourselves).
    if let Some(bm) = reader.try_volume_bitmap(total_clusters) {
        if bm.len() >= bitmap_bytes {
            tracing::info!(
                total_clusters,
                "FSCTL_GET_VOLUME_BITMAP returned ReFS allocation map; capturing only used clusters"
            );
            return bm;
        }
        tracing::debug!(
            got = bm.len(),
            expected = bitmap_bytes,
            "FSCTL_GET_VOLUME_BITMAP returned a short ReFS bitmap; padding with all-used"
        );
        let mut padded = vec![0xFFu8; bitmap_bytes];
        padded[..bm.len()].copy_from_slice(&bm);
        return padded;
    }
    // Conservative fallback: every cluster used (full-partition capture).
    tracing::warn!(
        total_clusters,
        "FSCTL_GET_VOLUME_BITMAP unavailable on ReFS volume; falling back to full-partition capture"
    );
    vec![0xFFu8; bitmap_bytes]
}

fn cluster_used(bitmap: &[u8], cluster: usize) -> bool {
    let byte = cluster / 8;
    let bit = cluster % 8;
    if byte >= bitmap.len() {
        return true;
    }
    (bitmap[byte] >> bit) & 1 == 1
}

/// Read the ReFS boot sector and allocation bitmap once and translate them
/// into `(used_extents, bytes_per_cluster, bitmap_hash)`. Same contract and
/// ordering constraint as [`crate::ntfs::ntfs_plan`]: call exactly once per
/// partition, **before** `FSCTL_LOCK_VOLUME`, because the bitmap IOCTL fails
/// with `ERROR_NOT_READY` on a locked volume on some Windows builds. The
/// extents feed both the manifest and the streaming step, which walks them
/// without re-reading any metadata.
pub fn refs_plan(reader: &mut impl BlockSource) -> Result<(Vec<Extent>, u32, Option<String>)> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let bs = parse_boot_sector(&boot)?;
    let cluster_size = bs.bytes_per_sector as u64 * bs.sectors_per_cluster as u64;
    let total_clusters = total_clusters_for(&bs, cluster_size);
    tracing::debug!(
        target: "phoenix_capture::refs",
        bytes_per_sector = bs.bytes_per_sector,
        sectors_per_cluster = bs.sectors_per_cluster,
        total_sectors = bs.total_sectors,
        cluster_size,
        total_clusters,
        refs_version = format!("{}.{}", bs.major_version, bs.minor_version).as_str(),
        "refs_plan computed volume geometry"
    );
    let bitmap = read_bitmap(reader, total_clusters);
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
        // Nothing reported allocated — capture at least the boot region so a
        // restored volume still has a recognizable header (mirrors ntfs_plan).
        extents.push(Extent {
            start_sector: 0,
            sector_count: (cluster_size / SECTOR_SIZE).max(8),
        });
    }
    Ok((extents, cluster_size as u32, bitmap_hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::MemoryBlockSource;

    /// Assemble a 512-byte ReFS boot sector. `total_sectors` is in units of
    /// `bytes_per_sector`, matching the on-disk field.
    pub(crate) fn build_refs_boot(
        bytes_per_sector: u32,
        sectors_per_cluster: u32,
        total_sectors: u64,
    ) -> Vec<u8> {
        let mut boot = vec![0u8; 512];
        boot[3..7].copy_from_slice(b"ReFS");
        // bytes 7..11 stay NUL — ReFS pads the FS name with zeros.
        boot[16..20].copy_from_slice(b"FSRS");
        boot[0x14..0x16].copy_from_slice(&0x200u16.to_le_bytes()); // structure length
        boot[0x16..0x18].copy_from_slice(&0u16.to_le_bytes()); // checksum (unchecked)
        boot[0x18..0x20].copy_from_slice(&total_sectors.to_le_bytes());
        boot[0x20..0x24].copy_from_slice(&bytes_per_sector.to_le_bytes());
        boot[0x24..0x28].copy_from_slice(&sectors_per_cluster.to_le_bytes());
        boot[0x28] = 3; // major version
        boot[0x29] = 14; // minor version
        boot
    }

    #[test]
    fn parses_valid_boot_sector_4k_clusters() {
        // 512-byte sectors, 8 sectors/cluster = 4K clusters, 1 GiB volume.
        let boot = build_refs_boot(512, 8, 2 * 1024 * 1024);
        let bs = parse_boot_sector(&boot).unwrap();
        assert_eq!(bs.bytes_per_sector, 512);
        assert_eq!(bs.sectors_per_cluster, 8);
        assert_eq!(bs.total_sectors, 2 * 1024 * 1024);
        assert_eq!((bs.major_version, bs.minor_version), (3, 14));
    }

    #[test]
    fn parses_64k_clusters_and_4kn_sectors() {
        // 64K clusters on 512e media.
        let bs = parse_boot_sector(&build_refs_boot(512, 128, 4096)).unwrap();
        assert_eq!(
            bs.bytes_per_sector as u64 * bs.sectors_per_cluster as u64,
            65536
        );
        // 4Kn media, 4K clusters (1 sector per cluster).
        let bs = parse_boot_sector(&build_refs_boot(4096, 1, 262_144)).unwrap();
        assert_eq!(bs.bytes_per_sector, 4096);
        assert_eq!(bs.sectors_per_cluster, 1);
    }

    #[test]
    fn rejects_ntfs_and_garbage() {
        let mut ntfs = vec![0u8; 512];
        ntfs[3..11].copy_from_slice(b"NTFS    ");
        assert!(parse_boot_sector(&ntfs).is_err());
        assert!(parse_boot_sector(&[0u8; 512]).is_err());
        assert!(parse_boot_sector(&[0u8; 100]).is_err());
        // "ReFS" name without the FSRS magic must not classify.
        let mut half = vec![0u8; 512];
        half[3..7].copy_from_slice(b"ReFS");
        assert!(parse_boot_sector(&half).is_err());
    }

    #[test]
    fn rejects_implausible_geometry() {
        // Zero sectors-per-cluster (division-by-zero guard).
        assert!(parse_boot_sector(&build_refs_boot(512, 0, 2048)).is_err());
        // Non-power-of-two cluster.
        assert!(parse_boot_sector(&build_refs_boot(512, 3, 2048)).is_err());
        // Absurd sector size.
        assert!(parse_boot_sector(&build_refs_boot(123, 8, 2048)).is_err());
        // Zero-length volume.
        assert!(parse_boot_sector(&build_refs_boot(512, 8, 0)).is_err());
    }

    #[test]
    fn total_clusters_floor_not_ceil() {
        // 1 GiB + one trailing 512-byte sector: the slack must NOT become a
        // phantom cluster (capture would read past the volume end).
        let bs = parse_boot_sector(&build_refs_boot(512, 8, 2 * 1024 * 1024 + 1)).unwrap();
        assert_eq!(total_clusters_for(&bs, 4096), 262_144);
    }

    #[test]
    fn plan_without_fsctl_falls_back_to_full_capture() {
        // MemoryBlockSource has no volume handle → try_volume_bitmap = None →
        // every cluster captured. 1 MiB volume, 4K clusters.
        let mut image = vec![0u8; 1024 * 1024];
        let boot = build_refs_boot(512, 8, 2048);
        image[..512].copy_from_slice(&boot);
        let mut src = MemoryBlockSource::new(image);
        let (extents, bytes_per_cluster, bitmap_hash) = refs_plan(&mut src).unwrap();
        assert_eq!(bytes_per_cluster, 4096);
        assert!(bitmap_hash.is_some());
        // All-used bitmap coalesces into a single extent spanning the volume.
        assert_eq!(extents.len(), 1);
        assert_eq!(extents[0].start_sector, 0);
        assert_eq!(extents[0].sector_count, 2048);
    }

    #[test]
    fn plan_reports_cluster_size_for_64k_format() {
        let mut image = vec![0u8; 2 * 1024 * 1024];
        let boot = build_refs_boot(512, 128, 4096);
        image[..512].copy_from_slice(&boot);
        let mut src = MemoryBlockSource::new(image);
        let (extents, bytes_per_cluster, _) = refs_plan(&mut src).unwrap();
        assert_eq!(bytes_per_cluster, 65536);
        assert_eq!(extents.len(), 1);
        assert_eq!(extents[0].sector_count, 4096);
    }
}
