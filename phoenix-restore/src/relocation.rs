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

    let mut entries = pack_pieces(&mut above_pieces, free_runs)?;
    entries.sort_by_key(|e| e.src_cluster_start);

    Ok(Some(RelocationMap {
        sector_size,
        cluster_size,
        safe_max_cluster,
        new_total_clusters,
        entries,
    }))
}

/// Place every above-boundary piece into below-boundary free space.
///
/// How the packing is done decides how fragmented the restored volume's run
/// lists end up. A piece placed whole keeps every file inside it contiguous,
/// so those files' run lists only change their LCN deltas; a piece split
/// across two free runs gains a run-list entry for every file it cuts
/// through, and enough of those overflow the MFT record the runs live in.
///
/// So: best-fit-decreasing. Big pieces go first (they have the fewest places
/// they can fit), each into the *smallest* free run that still swallows it
/// whole — which leaves the large runs intact for the pieces that need them.
/// Splitting is the fallback, and then across the largest runs available so
/// the piece breaks into as few fragments as possible.
///
/// Deterministic: every tie is broken on position, so the same inputs always
/// produce the same map. The pre-flight relies on this — it re-plans against
/// a map it expects the restore to rebuild identically.
fn pack_pieces(
    above_pieces: &mut [(u64, u64)],
    free_runs: Vec<(u64, u64)>,
) -> Result<Vec<RelocationEntry>> {
    let mut pool = free_runs;
    pool.sort_by_key(|&(start, _)| start);

    above_pieces.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut entries = Vec::with_capacity(above_pieces.len());
    for &mut (above_start, above_count) in above_pieces.iter_mut() {
        // Smallest run that fits the whole piece; ties to the lowest cluster.
        let whole = pool
            .iter()
            .enumerate()
            .filter(|(_, &(_, count))| count >= above_count)
            .min_by(|(ia, (_, ca)), (ib, (_, cb))| ca.cmp(cb).then(ia.cmp(ib)))
            .map(|(i, _)| i);
        if let Some(i) = whole {
            let (start, count) = pool[i];
            entries.push(RelocationEntry {
                src_cluster_start: above_start,
                cluster_count: above_count,
                dst_cluster_start: start,
            });
            if count == above_count {
                pool.remove(i);
            } else {
                pool[i] = (start + above_count, count - above_count);
            }
            continue;
        }

        let mut remaining = above_count;
        let mut src = above_start;
        while remaining > 0 {
            // Largest run first; ties to the lowest cluster.
            let Some(i) = pool
                .iter()
                .enumerate()
                .max_by(|(ia, (_, ca)), (ib, (_, cb))| ca.cmp(cb).then(ib.cmp(ia)))
                .map(|(i, _)| i)
            else {
                // The caller compares total above against total free before
                // getting here, so running dry means those sums disagreed
                // with the pool - a bug, not a user-facing condition.
                return Err(PhoenixError::Other(format!(
                    "relocation packing ran out of free space with {remaining} clusters left \
                     to place; this is a bug in build_relocation_map"
                )));
            };
            let (start, count) = pool[i];
            let take = remaining.min(count);
            entries.push(RelocationEntry {
                src_cluster_start: src,
                cluster_count: take,
                dst_cluster_start: start,
            });
            src += take;
            remaining -= take;
            if count == take {
                pool.remove(i);
            } else {
                pool[i] = (start + take, count - take);
            }
        }
    }
    Ok(entries)
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

    /// Free runs of 5, 20 and 10 clusters below the boundary, with a piece
    /// that fits three of them.
    fn three_free_runs_and(above: Extent) -> Vec<Extent> {
        vec![
            ext(5 * 8, 5 * 8),    // used 5..10   -> free 0..5
            ext(30 * 8, 5 * 8),   // used 30..35  -> free 10..30
            ext(45 * 8, 55 * 8),  // used 45..100 -> free 35..45
            above,
        ]
    }

    #[test]
    fn a_piece_goes_whole_into_the_smallest_run_that_fits_it() {
        // First-fit would drop this 8-cluster piece into the 5-cluster run at
        // 0 and split the remainder into the run at 10 - two entries, and a
        // cut through whatever files the piece contains. Best-fit places it
        // whole in the 10-cluster run at 35, leaving the 20-cluster run free
        // for a piece that actually needs it.
        let extents = three_free_runs_and(ext(200 * 8, 8 * 8));
        let map = build_relocation_map(&extents, SECTOR, CLUSTER, 100 * CLUSTER)
            .unwrap()
            .unwrap();
        assert_eq!(map.entries.len(), 1, "piece should not have been split");
        assert_eq!(map.entries[0].src_cluster_start, 200);
        assert_eq!(map.entries[0].cluster_count, 8);
        assert_eq!(map.entries[0].dst_cluster_start, 35);
    }

    #[test]
    fn an_oversized_piece_splits_across_the_largest_runs_first() {
        // 21 clusters against free runs of 5, 6 and 20: spending the biggest
        // run first costs two fragments, where first-fit's 5 + 6 + 10 costs
        // three.
        let extents = vec![
            ext(5 * 8, 5 * 8),    // used 5..10   -> free 0..5
            ext(16 * 8, 19 * 8),  // used 16..35  -> free 10..16
            ext(55 * 8, 45 * 8),  // used 55..100 -> free 35..55
            ext(200 * 8, 21 * 8),
        ];
        let map = build_relocation_map(&extents, SECTOR, CLUSTER, 100 * CLUSTER)
            .unwrap()
            .unwrap();
        assert_eq!(map.entries.len(), 2);
        assert_eq!(map.entries[0].cluster_count, 20);
        assert_eq!(map.entries[0].dst_cluster_start, 35);
        assert_eq!(map.entries[1].cluster_count, 1);
        assert_eq!(map.entries[1].dst_cluster_start, 10);
    }

    #[test]
    fn packing_is_deterministic() {
        // The pre-flight adjusts a map it expects the restore to rebuild
        // byte-identically, so the packer may not depend on iteration order.
        let extents = three_free_runs_and(ext(200 * 8, 8 * 8));
        let a = build_relocation_map(&extents, SECTOR, CLUSTER, 100 * CLUSTER).unwrap();
        let b = build_relocation_map(&extents, SECTOR, CLUSTER, 100 * CLUSTER).unwrap();
        assert_eq!(a.unwrap().entries, b.unwrap().entries);
    }

    #[test]
    fn destinations_never_overlap_or_escape_the_boundary() {
        // Property check over a spread of layouts: two pieces landing on the
        // same destination would silently destroy one of them.
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let mut extents = Vec::new();
            let mut cluster = 0u64;
            while cluster < 400 {
                let gap = next() % 7;
                let len = 1 + next() % 9;
                cluster += gap;
                extents.push(ext(cluster * 8, len * 8));
                cluster += len;
            }
            let Ok(Some(map)) = build_relocation_map(&extents, SECTOR, CLUSTER, 300 * CLUSTER)
            else {
                continue; // doesn't fit; nothing to check
            };
            let mut dst: Vec<(u64, u64)> = map
                .entries
                .iter()
                .map(|e| (e.dst_cluster_start, e.dst_end_cluster()))
                .collect();
            dst.sort();
            for w in dst.windows(2) {
                assert!(w[0].1 <= w[1].0, "destination ranges overlap: {:?}", w);
            }
            for e in &map.entries {
                assert!(e.dst_end_cluster() <= map.safe_max_cluster + 1);
                assert!(e.src_cluster_start > map.safe_max_cluster);
            }
        }
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
