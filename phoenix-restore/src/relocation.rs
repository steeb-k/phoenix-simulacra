//! NTFS shrink relocation planning.
//!
//! When a backup is restored to a target smaller than the source, any
//! used clusters that lived past the new partition boundary in the
//! source need to be physically relocated below the boundary on the
//! target. This module builds the map describing that relocation.
//!
//! The data structure ([`RelocationMap`]) lives in [`phoenix_core`]
//! because the byte-stream translator in `phoenix-capture/src/raw.rs`
//! also consumes it, and `phoenix-capture` can't depend on
//! `phoenix-restore` (cycle). The *builder* lives here because it's
//! invoked from the restore planner alongside `validate_extents_fit`
//! and shares its shrink-only call site.
//!
//! The algorithm is a stripped-down variant of `ntfsresize`'s shrink
//! logic. Where `ntfsresize` walks the live filesystem, we work from
//! the manifest's pre-captured used-cluster bitmap (so the source
//! filesystem doesn't have to be online during the restore — "capture
//! once, restore anywhere").

pub use phoenix_core::relocation::{RelocationEntry, RelocationMap, WriteSegment};

use phoenix_core::container::Extent;
use phoenix_core::error::{PhoenixError, Result};

/// Build a relocation map for a single partition, given its source
/// extents (in sector space, as stored in the manifest) and the size
/// of the target partition in bytes.
///
/// Returns `Ok(None)` when no relocation is required — every source
/// extent already fits below the target boundary. The shrink path
/// can then proceed exactly as the same-size path does.
///
/// Returns `Err(PhoenixError::Other)` when relocation is impossible
/// because the used data exceeds the target's free space budget.
pub fn build_relocation_map(
    source_extents: &[Extent],
    sector_size: u64,
    cluster_size: u64,
    target_size_bytes: u64,
) -> Result<Option<RelocationMap>> {
    if cluster_size == 0 {
        return Err(PhoenixError::Other(
            "cannot build relocation map: source manifest reports zero bytes_per_cluster (raw-mode \
             capture). Restore at original size, or recapture in used-blocks mode."
                .into(),
        ));
    }
    if sector_size == 0 {
        return Err(PhoenixError::Other(
            "cannot build relocation map: source manifest reports zero sector_size".into(),
        ));
    }
    if target_size_bytes < cluster_size * 2 {
        return Err(PhoenixError::Other(format!(
            "target partition ({target_size_bytes} bytes) is smaller than two source clusters \
             ({cluster_size} bytes each); refusing to relocate"
        )));
    }

    let new_total_clusters = target_size_bytes / cluster_size;
    let safe_max_cluster = new_total_clusters - 1;

    let sectors_per_cluster = cluster_size / sector_size;
    if sectors_per_cluster == 0 || !cluster_size.is_multiple_of(sector_size) {
        return Err(PhoenixError::Other(format!(
            "cluster_size ({cluster_size}) is not a multiple of sector_size ({sector_size})"
        )));
    }

    // Convert sector-space extents to cluster space.
    let mut cluster_extents: Vec<(u64, u64)> = Vec::with_capacity(source_extents.len());
    for ext in source_extents {
        let start_byte = ext.start_sector.saturating_mul(sector_size);
        let len_bytes = ext.sector_count.saturating_mul(sector_size);
        if start_byte % cluster_size != 0 {
            return Err(PhoenixError::Other(format!(
                "source extent at sector {} is not cluster-aligned (cluster_size={})",
                ext.start_sector, cluster_size
            )));
        }
        let start_cluster = start_byte / cluster_size;
        let end_byte = start_byte.saturating_add(len_bytes);
        let end_cluster = end_byte.div_ceil(cluster_size);
        let cluster_count = end_cluster.saturating_sub(start_cluster);
        if cluster_count > 0 {
            cluster_extents.push((start_cluster, cluster_count));
        }
    }

    let mut above_pieces: Vec<(u64, u64)> = Vec::new();
    let mut below_used: Vec<(u64, u64)> = Vec::new();

    let mut needs_relocation = false;
    for &(start, count) in &cluster_extents {
        let end = start.saturating_add(count);
        if end > safe_max_cluster + 1 {
            needs_relocation = true;
            let split = start.max(safe_max_cluster + 1);
            above_pieces.push((split, end - split));
            if start < split {
                below_used.push((start, split - start));
            }
        } else {
            below_used.push((start, count));
        }
    }

    if !needs_relocation {
        // No clusters past the boundary — no physical relocation
        // required. We still return Some(empty_map) rather than
        // None so the NTFS metadata rewriter runs in `restore_ntfs`
        // and gets a chance to truncate `$Bitmap` past the new
        // boundary, stamp `$LogFile` clean, and re-mirror the first
        // four MFT records. Without that step, a shrink restore is
        // technically mountable but Windows still wants `chkdsk /F`
        // to reconcile `$Bitmap` against the new total_clusters.
        // The empty `entries` vec means `translate_write` and
        // `relocate_runs` are identity transforms in this mode.
        return Ok(Some(RelocationMap {
            sector_size,
            cluster_size,
            safe_max_cluster,
            new_total_clusters,
            entries: Vec::new(),
        }));
    }

    below_used.sort_by_key(|&(s, _)| s);
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (s, c) in below_used {
        if let Some(last) = merged.last_mut() {
            let last_end = last.0 + last.1;
            if s <= last_end {
                let new_end = last_end.max(s + c);
                last.1 = new_end - last.0;
                continue;
            }
        }
        merged.push((s, c));
    }
    let mut free_runs: Vec<(u64, u64)> = Vec::new();
    let mut cursor: u64 = 0;
    for (s, c) in &merged {
        if *s > cursor {
            free_runs.push((cursor, *s - cursor));
        }
        cursor = s.saturating_add(*c);
    }
    if cursor <= safe_max_cluster {
        free_runs.push((cursor, safe_max_cluster + 1 - cursor));
    }

    let total_above: u64 = above_pieces.iter().map(|(_, c)| *c).sum();
    let total_free: u64 = free_runs.iter().map(|(_, c)| *c).sum();
    if total_above > total_free {
        let needed_bytes = total_above.saturating_mul(cluster_size);
        let avail_bytes = total_free.saturating_mul(cluster_size);
        return Err(PhoenixError::Other(format!(
            "cannot shrink partition to {target_size_bytes} bytes: {} clusters of used data live \
             past the new boundary but only {} clusters of free space are available below it \
             (need {} bytes, have {} bytes). Pick a larger target.",
            total_above, total_free, needed_bytes, avail_bytes
        )));
    }

    let mut entries = Vec::with_capacity(above_pieces.len());
    let mut free_idx = 0usize;
    let mut free_offset = 0u64;
    above_pieces.sort_by_key(|&(s, _)| s);
    for (above_start, above_count) in above_pieces {
        let mut remaining = above_count;
        let mut src = above_start;
        while remaining > 0 {
            let (run_start, run_count) = free_runs[free_idx];
            let avail = run_count - free_offset;
            let take = remaining.min(avail);
            entries.push(RelocationEntry {
                src_cluster_start: src,
                cluster_count: take,
                dst_cluster_start: run_start + free_offset,
            });
            src += take;
            remaining -= take;
            free_offset += take;
            if free_offset >= run_count {
                free_idx += 1;
                free_offset = 0;
            }
        }
    }

    entries.sort_by_key(|e| e.src_cluster_start);

    Ok(Some(RelocationMap {
        sector_size,
        cluster_size,
        safe_max_cluster,
        new_total_clusters,
        entries,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(start_sector: u64, sector_count: u64) -> Extent {
        Extent {
            start_sector,
            sector_count,
        }
    }

    const SECTOR: u64 = 512;
    const CLUSTER: u64 = 4096;

    #[test]
    fn no_relocation_returns_empty_map_when_data_fits() {
        // Used data lives entirely below the boundary, so no clusters
        // need physical relocation. We still expect a map so the
        // NTFS metadata rewriter runs (truncates `$Bitmap`, stamps
        // `$LogFile`, etc.) — just one with no entries.
        let extents = vec![ext(0, 100 * 8)];
        let target_bytes = 200 * CLUSTER;
        let map = build_relocation_map(&extents, SECTOR, CLUSTER, target_bytes)
            .unwrap()
            .expect("expected Some(empty_map), not None");
        assert!(map.entries.is_empty());
        assert_eq!(map.new_total_clusters, 200);
        assert_eq!(map.safe_max_cluster, 199);
    }

    #[test]
    fn refuses_when_used_exceeds_target_capacity() {
        let extents = vec![ext(0, 200 * 8)];
        let target_bytes = 100 * CLUSTER;
        let err = build_relocation_map(&extents, SECTOR, CLUSTER, target_bytes).unwrap_err();
        assert!(matches!(err, PhoenixError::Other(_)));
    }

    #[test]
    fn relocates_above_boundary_clusters_to_below_free_space() {
        let extents = vec![ext(0, 50 * 8), ext(200 * 8, 20 * 8)];
        let target_bytes = 100 * CLUSTER;
        let map = build_relocation_map(&extents, SECTOR, CLUSTER, target_bytes)
            .unwrap()
            .unwrap();
        assert_eq!(map.safe_max_cluster, 99);
        assert_eq!(map.new_total_clusters, 100);
        assert_eq!(map.entries.len(), 1);
        assert_eq!(map.entries[0].src_cluster_start, 200);
        assert_eq!(map.entries[0].cluster_count, 20);
        assert_eq!(map.entries[0].dst_cluster_start, 50);
    }

    #[test]
    fn splits_extent_that_straddles_boundary() {
        let extents = vec![ext(90 * 8, 30 * 8)];
        let target_bytes = 100 * CLUSTER;
        let map = build_relocation_map(&extents, SECTOR, CLUSTER, target_bytes)
            .unwrap()
            .unwrap();
        assert_eq!(map.entries.len(), 1);
        assert_eq!(map.entries[0].src_cluster_start, 100);
        assert_eq!(map.entries[0].cluster_count, 20);
        assert_eq!(map.entries[0].dst_cluster_start, 0);
    }

    #[test]
    fn translate_write_splits_at_boundary() {
        let map = RelocationMap {
            sector_size: SECTOR,
            cluster_size: CLUSTER,
            safe_max_cluster: 99,
            new_total_clusters: 100,
            entries: vec![RelocationEntry {
                src_cluster_start: 200,
                cluster_count: 10,
                dst_cluster_start: 50,
            }],
        };
        let src_byte = 200 * CLUSTER;
        let len = 10 * CLUSTER as usize;
        let segs = map.translate_write(src_byte, len);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].dst_byte_offset, 50 * CLUSTER);
        assert_eq!(segs[0].source_offset, 0);
        assert_eq!(segs[0].len, 10 * CLUSTER as usize);
    }

    #[test]
    fn translate_write_below_boundary_is_identity() {
        let map = RelocationMap {
            sector_size: SECTOR,
            cluster_size: CLUSTER,
            safe_max_cluster: 99,
            new_total_clusters: 100,
            entries: vec![],
        };
        let segs = map.translate_write(50 * CLUSTER, 4 * CLUSTER as usize);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].dst_byte_offset, 50 * CLUSTER);
        assert_eq!(segs[0].source_offset, 0);
        assert_eq!(segs[0].len, 4 * CLUSTER as usize);
    }
}
