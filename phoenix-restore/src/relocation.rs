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

use phoenix_capture::ntfs_preflight::{
    map_can_disturb_records, record_name, scan_mft, simulate_map, source_mft_extents, MftSnapshot,
    RecordOverflowReport, VolumeRead,
};
use phoenix_core::container::Extent;
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::progress::ProgressHandle;

/// Build a relocation map for a single partition, given its source
/// extents (in sector space, as stored in the manifest) and the size
/// of the target partition in bytes.
///
/// The returned map has no entries when nothing lives past the target
/// boundary; the metadata rewriter still wants it, so the shrink path can
/// proceed exactly as the same-size path does.
///
/// Returns `Err(PhoenixError::Other)` when relocation is impossible
/// because the used data exceeds the target's free space budget.
///
/// This builds the map but does not check that the resulting run-list
/// rewrites will fit their MFT records — see [`plan_shrink`] for the
/// checked version, which is what the restore and clone paths use.
pub fn build_relocation_map(
    source_extents: &[Extent],
    sector_size: u64,
    cluster_size: u64,
    target_size_bytes: u64,
) -> Result<Option<RelocationMap>> {
    let layout = layout_for(source_extents, sector_size, cluster_size, target_size_bytes)?;
    layout.check_capacity(cluster_size, target_size_bytes)?;
    Ok(Some(layout.pack(sector_size, cluster_size)?))
}

/// Work out what a shrink to `target_size_bytes` would have to move.
fn layout_for(
    source_extents: &[Extent],
    sector_size: u64,
    cluster_size: u64,
    target_size_bytes: u64,
) -> Result<ShrinkLayout> {
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

    Ok(ShrinkLayout::from_cluster_extents(
        &cluster_extents,
        safe_max_cluster,
        new_total_clusters,
    ))
}

/// What a shrink has to move, and where there is room to put it.
///
/// Split out of [`build_relocation_map`] because the pre-flight needs the
/// same view repeatedly: once per candidate target size while searching for
/// the smallest one that works, and again while re-planning individual
/// records. `free_runs` is the *pristine* free space — every destination the
/// planner may hand out comes from inside it, which is what lets the
/// re-planner reason about eviction windows.
#[derive(Debug, Clone)]
struct ShrinkLayout {
    safe_max_cluster: u64,
    new_total_clusters: u64,
    /// Used clusters past the boundary, sorted, that must be relocated.
    above_pieces: Vec<(u64, u64)>,
    /// Below-boundary runs holding no data, sorted and merged.
    free_runs: Vec<(u64, u64)>,
}

impl ShrinkLayout {
    fn from_cluster_extents(
        cluster_extents: &[(u64, u64)],
        safe_max_cluster: u64,
        new_total_clusters: u64,
    ) -> Self {
        let mut above_pieces: Vec<(u64, u64)> = Vec::new();
        let mut below_used: Vec<(u64, u64)> = Vec::new();
        for &(start, count) in cluster_extents {
            let end = start.saturating_add(count);
            if end > safe_max_cluster + 1 {
                let split = start.max(safe_max_cluster + 1);
                above_pieces.push((split, end - split));
                if start < split {
                    below_used.push((start, split - start));
                }
            } else {
                below_used.push((start, count));
            }
        }
        above_pieces.sort_by_key(|&(s, _)| s);

        below_used.sort_by_key(|&(s, _)| s);
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (s, c) in below_used {
            if let Some(last) = merged.last_mut() {
                let last_end = last.0 + last.1;
                if s <= last_end {
                    last.1 = last_end.max(s + c) - last.0;
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

        Self {
            safe_max_cluster,
            new_total_clusters,
            above_pieces,
            free_runs,
        }
    }

    fn total_above(&self) -> u64 {
        self.above_pieces.iter().map(|(_, c)| *c).sum()
    }

    fn total_free(&self) -> u64 {
        self.free_runs.iter().map(|(_, c)| *c).sum()
    }

    /// Does the used data even fit below the new boundary?
    fn check_capacity(&self, cluster_size: u64, target_size_bytes: u64) -> Result<()> {
        let (above, free) = (self.total_above(), self.total_free());
        if above > free {
            return Err(PhoenixError::Other(format!(
                "cannot shrink partition to {target_size_bytes} bytes: {above} clusters of used                  data live past the new boundary but only {free} clusters of free space are                  available below it (need {} bytes, have {} bytes). Pick a larger target.",
                above.saturating_mul(cluster_size),
                free.saturating_mul(cluster_size),
            )));
        }
        Ok(())
    }

    fn pack(&self, sector_size: u64, cluster_size: u64) -> Result<RelocationMap> {
        // No clusters past the boundary — no physical relocation required. We
        // still return a map (with no entries) rather than nothing, so the
        // NTFS metadata rewriter still runs in `restore_ntfs` and gets a
        // chance to truncate `$Bitmap` past the new boundary, stamp `$LogFile`
        // clean, and re-mirror the first four MFT records. Without that step a
        // shrink restore is technically mountable but Windows still wants
        // `chkdsk /F` to reconcile `$Bitmap` against the new total_clusters.
        // Empty `entries` makes `translate_write` and `relocate_runs`
        // identity transforms.
        let mut entries = if self.above_pieces.is_empty() {
            Vec::new()
        } else {
            let mut pieces = self.above_pieces.clone();
            pack_pieces(&mut pieces, self.free_runs.clone())?
        };
        entries.sort_by_key(|e| e.src_cluster_start);
        Ok(RelocationMap {
            sector_size,
            cluster_size,
            safe_max_cluster: self.safe_max_cluster,
            new_total_clusters: self.new_total_clusters,
            entries,
        })
    }
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

/// A shrink plan that has been checked against the source volume's MFT.
#[derive(Debug, Clone)]
pub struct ShrinkPlan {
    pub map: RelocationMap,
    /// How many records needed their clusters re-placed to make the rewrite
    /// fit. Zero on a straightforward shrink; useful in logs and tests.
    pub replanned_records: u64,
}

/// Why a shrink cannot be done at a given size.
enum Infeasible {
    /// The used data does not fit below the boundary at all.
    Capacity(String),
    /// Some MFT record's run lists cannot be made to fit its record.
    Record(Box<RecordOverflowReport>),
}

/// How many defragment-and-recheck passes to allow before giving up.
/// Re-placing one record's clusters can disturb another's, so the loop is
/// not guaranteed to settle; the cap turns a pathological layout into a
/// clean plan-time refusal instead of a hang.
const MAX_REPLAN_PASSES: usize = 32;

/// Build a relocation map for a shrink and *prove* it works before anything
/// is written.
///
/// The map alone is not enough. Relocating clusters rewrites the run lists in
/// the source's MFT records, and a rewritten list can outgrow the record it
/// lives in — which the old code only discovered after streaming the whole
/// partition, leaving the target holding data its metadata no longer
/// described.
///
/// So this reads the source's MFT, simulates the rewrite, and for any record
/// that would overflow, *changes the plan*: the offending record's clusters
/// are re-placed into one contiguous destination, which collapses its run
/// list instead of growing it. Since the planner is what decides where
/// relocated clusters land, this is nearly always possible. Repeat until
/// nothing overflows.
///
/// If it still cannot be done, the error names the smallest target size that
/// would work — a number the caller can act on, rather than "pick a less
/// aggressive shrink target".
///
/// `source` reads the volume as it exists *now*: a `.phnx` for a restore, a
/// live disk or snapshot for a clone.
pub fn plan_shrink(
    source_extents: &[Extent],
    sector_size: u64,
    cluster_size: u64,
    target_size_bytes: u64,
    original_size_bytes: u64,
    source: &mut dyn VolumeRead,
    progress: Option<&ProgressHandle>,
) -> Result<ShrinkPlan> {
    // A malformed geometry (no cluster size, misaligned extents) is a hard
    // error about the backup itself, not a shrink that happens not to fit —
    // it propagates rather than turning into shrink advice.
    let layout = layout_for(source_extents, sector_size, cluster_size, target_size_bytes)?;

    // Nothing crosses the boundary, so translation is the identity and no run
    // list can change. Skipping the MFT scan here keeps a mild shrink as fast
    // as it was before the pre-flight existed.
    if layout.above_pieces.is_empty() {
        let map = layout.pack(sector_size, cluster_size)?;
        debug_assert!(!map_can_disturb_records(&map));
        return Ok(ShrinkPlan {
            map,
            replanned_records: 0,
        });
    }

    if let Some(p) = progress {
        p.set_detail("Shrink pre-flight: reading the source MFT");
    }
    let snapshot = scan_mft(source, progress)?;

    match attempt_plan(
        &snapshot,
        source_extents,
        sector_size,
        cluster_size,
        target_size_bytes,
        progress,
    )? {
        Ok(plan) => {
            if plan.replanned_records > 0 {
                tracing::info!(
                    replanned_records = plan.replanned_records,
                    entries = plan.map.entries.len(),
                    "shrink pre-flight defragmented the relocation plan to fit the MFT"
                );
            }
            Ok(plan)
        }
        Err(reason) => Err(infeasible_error(
            reason,
            &snapshot,
            source_extents,
            sector_size,
            cluster_size,
            target_size_bytes,
            original_size_bytes,
            source,
        )),
    }
}

/// One end-to-end attempt at a given target size: build, simulate, re-plan
/// until it fits or provably won't. Pure with respect to the volume — it
/// reuses `snapshot` and never reads again, which is what makes the
/// minimum-size search below affordable.
fn attempt_plan(
    snapshot: &MftSnapshot,
    source_extents: &[Extent],
    sector_size: u64,
    cluster_size: u64,
    target_size_bytes: u64,
    progress: Option<&ProgressHandle>,
) -> Result<std::result::Result<ShrinkPlan, Infeasible>> {
    let layout = match layout_for(source_extents, sector_size, cluster_size, target_size_bytes) {
        Ok(l) => l,
        Err(e) => return Ok(Err(Infeasible::Capacity(e.to_string()))),
    };
    if let Err(e) = layout.check_capacity(cluster_size, target_size_bytes) {
        return Ok(Err(Infeasible::Capacity(e.to_string())));
    }
    let mut map = layout.pack(sector_size, cluster_size)?;
    if map.entries.is_empty() {
        return Ok(Ok(ShrinkPlan {
            map,
            replanned_records: 0,
        }));
    }

    let mut replanned = 0u64;
    for pass in 0..MAX_REPLAN_PASSES {
        let overflows = simulate_map(snapshot, &map);
        if overflows.is_empty() {
            verify_map(&map, &layout)?;
            return Ok(Ok(ShrinkPlan {
                map,
                replanned_records: replanned,
            }));
        }
        if let Some(p) = progress {
            p.set_detail(format!(
                "Shrink pre-flight: defragmenting the plan for {} record(s) (pass {})",
                overflows.len(),
                pass + 1
            ));
        }
        let mut moved_something = false;
        for overflow in &overflows {
            if replan_record(&mut map, &layout, &overflow.above_ranges)? {
                replanned += 1;
                moved_something = true;
            }
        }
        if !moved_something {
            return Ok(Err(Infeasible::Record(Box::new(worst_of(overflows)))));
        }
    }

    // Passes exhausted: re-placed records kept disturbing each other.
    let overflows = simulate_map(snapshot, &map);
    if overflows.is_empty() {
        verify_map(&map, &layout)?;
        Ok(Ok(ShrinkPlan {
            map,
            replanned_records: replanned,
        }))
    } else {
        Ok(Err(Infeasible::Record(Box::new(worst_of(overflows)))))
    }
}

fn worst_of(mut overflows: Vec<RecordOverflowReport>) -> RecordOverflowReport {
    overflows.sort_by_key(|o| (o.needed_bytes.saturating_sub(o.available), o.record_index));
    overflows.pop().expect("caller checked the list is non-empty")
}

/// Give one record's above-boundary clusters a contiguous home.
///
/// Returns `false` when the record could not be improved, which tells the
/// caller to stop rather than spin.
///
/// The record's current destinations are released first: they are exactly
/// the scattered fragments that made its run list too long, and handing them
/// back often coalesces enough space to re-place the record whole. Failing
/// that, we take the smallest pristine free run big enough, evict whatever
/// else was placed inside it, and re-place the evicted data elsewhere — that
/// may in turn overflow, which the next pass catches.
fn replan_record(
    map: &mut RelocationMap,
    layout: &ShrinkLayout,
    ranges: &[(u64, u64)],
) -> Result<bool> {
    let needed: u64 = ranges.iter().map(|(_, c)| *c).sum();
    if needed == 0 {
        return Ok(false);
    }

    let before = map.entries.clone();
    unassign(&mut map.entries, ranges);
    let free = free_space(&layout.free_runs, &map.entries);

    if let Some(dst) = best_fit_start(&free, needed) {
        assign_contiguous(&mut map.entries, ranges, dst);
        map.entries.sort_by_key(|e| e.src_cluster_start);
        // Landing back where it started is not progress, and saying it was
        // would keep the outer loop running against an unchanged map.
        return Ok(map.entries != before);
    }

    let Some(window) = best_fit_start(&layout.free_runs, needed) else {
        // Not even the pristine layout has this many contiguous clusters
        // below the boundary. Put the record back and report no progress.
        map.entries = before;
        return Ok(false);
    };

    let evicted = evict_window(&mut map.entries, window, needed);
    assign_contiguous(&mut map.entries, ranges, window);
    let free_after = free_space(&layout.free_runs, &map.entries);
    if !assign_scattered(&mut map.entries, &evicted, &free_after) {
        // Could not re-home what we displaced; leave the map as it was.
        map.entries = before;
        return Ok(false);
    }
    map.entries.sort_by_key(|e| e.src_cluster_start);
    Ok(map.entries != before)
}

/// Drop `ranges` (ascending, non-overlapping source clusters) out of the
/// map, splitting any entry that only partly covers them.
fn unassign(entries: &mut Vec<RelocationEntry>, ranges: &[(u64, u64)]) {
    let mut kept = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        let start = entry.src_cluster_start;
        let end = entry.src_end_cluster();
        let mut cursor = start;
        for &(range_start, range_count) in ranges {
            let range_end = range_start + range_count;
            if range_end <= cursor || range_start >= end {
                continue;
            }
            if range_start > cursor {
                kept.push(sub_entry(&entry, cursor, range_start.min(end)));
            }
            cursor = cursor.max(range_end);
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            kept.push(sub_entry(&entry, cursor, end));
        }
    }
    *entries = kept;
}

/// The slice of `entry` covering source clusters `[start, end)`.
fn sub_entry(entry: &RelocationEntry, start: u64, end: u64) -> RelocationEntry {
    RelocationEntry {
        src_cluster_start: start,
        cluster_count: end - start,
        dst_cluster_start: entry.dst_cluster_start + (start - entry.src_cluster_start),
    }
}

/// Map `ranges` onto consecutive destinations starting at `dst_start`.
///
/// Because the ranges go in ascending source order into one contiguous
/// block, anything contiguous at the source stays contiguous at the
/// destination — which is the whole point: it collapses the record's run
/// list rather than lengthening it.
fn assign_contiguous(entries: &mut Vec<RelocationEntry>, ranges: &[(u64, u64)], dst_start: u64) {
    let mut dst = dst_start;
    for &(src, count) in ranges {
        entries.push(RelocationEntry {
            src_cluster_start: src,
            cluster_count: count,
            dst_cluster_start: dst,
        });
        dst += count;
    }
}

/// Place `ranges` anywhere they fit, preferring whole placements and then
/// the largest runs. Returns false if space ran out.
fn assign_scattered(
    entries: &mut Vec<RelocationEntry>,
    ranges: &[(u64, u64)],
    free: &[(u64, u64)],
) -> bool {
    let mut pool = free.to_vec();
    for &(src_start, count) in ranges {
        let mut remaining = count;
        let mut src = src_start;
        while remaining > 0 {
            let whole = pool
                .iter()
                .enumerate()
                .filter(|(_, &(_, c))| c >= remaining)
                .min_by(|(ia, (_, ca)), (ib, (_, cb))| ca.cmp(cb).then(ia.cmp(ib)))
                .map(|(i, _)| i);
            let idx = whole.or_else(|| {
                pool.iter()
                    .enumerate()
                    .max_by(|(ia, (_, ca)), (ib, (_, cb))| ca.cmp(cb).then(ib.cmp(ia)))
                    .map(|(i, _)| i)
            });
            let Some(i) = idx else {
                return false;
            };
            let (start, avail) = pool[i];
            let take = remaining.min(avail);
            entries.push(RelocationEntry {
                src_cluster_start: src,
                cluster_count: take,
                dst_cluster_start: start,
            });
            src += take;
            remaining -= take;
            if avail == take {
                pool.remove(i);
            } else {
                pool[i] = (start + take, avail - take);
            }
        }
    }
    true
}

/// Remove everything currently placed in `[window, window + count)`,
/// returning the source ranges that need a new home.
fn evict_window(entries: &mut Vec<RelocationEntry>, window: u64, count: u64) -> Vec<(u64, u64)> {
    let window_end = window + count;
    let mut kept = Vec::with_capacity(entries.len());
    let mut evicted: Vec<(u64, u64)> = Vec::new();
    for entry in entries.drain(..) {
        let dst_start = entry.dst_cluster_start;
        let dst_end = entry.dst_end_cluster();
        if dst_end <= window || dst_start >= window_end {
            kept.push(entry);
            continue;
        }
        if dst_start < window {
            let n = window - dst_start;
            kept.push(sub_entry(
                &entry,
                entry.src_cluster_start,
                entry.src_cluster_start + n,
            ));
        }
        if dst_end > window_end {
            let skip = window_end - dst_start;
            kept.push(sub_entry(
                &entry,
                entry.src_cluster_start + skip,
                entry.src_end_cluster(),
            ));
        }
        let overlap_start = dst_start.max(window);
        let overlap_end = dst_end.min(window_end);
        evicted.push((
            entry.src_cluster_start + (overlap_start - dst_start),
            overlap_end - overlap_start,
        ));
    }
    *entries = kept;
    merge_ranges(&mut evicted);
    evicted
}

fn merge_ranges(ranges: &mut Vec<(u64, u64)>) {
    ranges.sort_by_key(|&(s, _)| s);
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for &(start, count) in ranges.iter() {
        if let Some(last) = merged.last_mut() {
            let last_end = last.0 + last.1;
            if start <= last_end {
                last.1 = last_end.max(start + count) - last.0;
                continue;
            }
        }
        merged.push((start, count));
    }
    *ranges = merged;
}

/// Pristine free space minus everything the map currently places.
fn free_space(pristine: &[(u64, u64)], entries: &[RelocationEntry]) -> Vec<(u64, u64)> {
    let mut used: Vec<(u64, u64)> = entries
        .iter()
        .map(|e| (e.dst_cluster_start, e.cluster_count))
        .collect();
    merge_ranges(&mut used);

    let mut out = Vec::new();
    for &(free_start, free_count) in pristine {
        let free_end = free_start + free_count;
        let mut cursor = free_start;
        for &(used_start, used_count) in &used {
            let used_end = used_start + used_count;
            if used_end <= cursor || used_start >= free_end {
                continue;
            }
            if used_start > cursor {
                out.push((cursor, used_start - cursor));
            }
            cursor = cursor.max(used_end);
            if cursor >= free_end {
                break;
            }
        }
        if cursor < free_end {
            out.push((cursor, free_end - cursor));
        }
    }
    out
}

/// Smallest run holding `needed` clusters; ties to the lowest cluster.
fn best_fit_start(runs: &[(u64, u64)], needed: u64) -> Option<u64> {
    runs.iter()
        .filter(|(_, count)| *count >= needed)
        .min_by(|(sa, ca), (sb, cb)| ca.cmp(cb).then(sa.cmp(sb)))
        .map(|(start, _)| *start)
}

/// Assert the structural properties every consumer of a map depends on.
///
/// `$Bitmap` marks destinations from `entries`, `translate_write` routes data
/// through them, and `relocate_runs` binary-searches them: overlapping or
/// unsorted entries would silently corrupt the restored volume rather than
/// fail, so the map is checked before it can be used.
fn verify_map(map: &RelocationMap, layout: &ShrinkLayout) -> Result<()> {
    let bug = |what: String| PhoenixError::Other(format!("relocation map is inconsistent: {what}"));

    let mut src: Vec<(u64, u64)> = Vec::with_capacity(map.entries.len());
    let mut dst: Vec<(u64, u64)> = Vec::with_capacity(map.entries.len());
    for (i, e) in map.entries.iter().enumerate() {
        if i > 0 && map.entries[i - 1].src_cluster_start > e.src_cluster_start {
            return Err(bug("entries are not sorted by source cluster".into()));
        }
        if e.cluster_count == 0 {
            return Err(bug(format!("entry {i} moves zero clusters")));
        }
        if e.src_cluster_start <= map.safe_max_cluster {
            return Err(bug(format!(
                "entry {i} relocates cluster {} which is below the boundary {}",
                e.src_cluster_start, map.safe_max_cluster
            )));
        }
        if e.dst_end_cluster() > map.safe_max_cluster + 1 {
            return Err(bug(format!(
                "entry {i} lands at {}..{} which is past the new boundary {}",
                e.dst_cluster_start,
                e.dst_end_cluster(),
                map.safe_max_cluster + 1
            )));
        }
        src.push((e.src_cluster_start, e.cluster_count));
        dst.push((e.dst_cluster_start, e.cluster_count));
    }

    for (label, mut ranges) in [("source", src), ("destination", dst)] {
        ranges.sort_by_key(|&(s, _)| s);
        for pair in ranges.windows(2) {
            if pair[0].0 + pair[0].1 > pair[1].0 {
                return Err(bug(format!(
                    "two entries overlap in {label} space at cluster {}",
                    pair[1].0
                )));
            }
        }
    }

    // Every above-boundary cluster must still be accounted for, and every
    // destination must lie in space that was actually free.
    let mapped: u64 = map.entries.iter().map(|e| e.cluster_count).sum();
    if mapped != layout.total_above() {
        return Err(bug(format!(
            "map moves {mapped} clusters but {} live past the boundary",
            layout.total_above()
        )));
    }
    for e in &map.entries {
        let inside = layout
            .free_runs
            .iter()
            .any(|&(s, c)| e.dst_cluster_start >= s && e.dst_end_cluster() <= s + c);
        if !inside {
            return Err(bug(format!(
                "entry lands at {}..{}, which is not free space",
                e.dst_cluster_start,
                e.dst_end_cluster()
            )));
        }
    }
    Ok(())
}

/// The smallest target size that would actually work, found by bisecting
/// between the size that failed and the source's own size (which always
/// works — it needs no relocation at all).
///
/// Reuses the MFT snapshot, so each probe is pure computation.
fn minimum_viable_target(
    snapshot: &MftSnapshot,
    source_extents: &[Extent],
    sector_size: u64,
    cluster_size: u64,
    failed_bytes: u64,
    original_bytes: u64,
) -> Option<u64> {
    let mut low = failed_bytes / cluster_size; // known bad
    let mut high = original_bytes / cluster_size; // known good
    if high <= low {
        return None;
    }
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        let feasible = matches!(
            attempt_plan(
                snapshot,
                source_extents,
                sector_size,
                cluster_size,
                mid * cluster_size,
                None,
            ),
            Ok(Ok(_))
        );
        if feasible {
            high = mid;
        } else {
            low = mid;
        }
    }
    Some(high * cluster_size)
}

#[allow(clippy::too_many_arguments)]
fn infeasible_error(
    reason: Infeasible,
    snapshot: &MftSnapshot,
    source_extents: &[Extent],
    sector_size: u64,
    cluster_size: u64,
    target_size_bytes: u64,
    original_size_bytes: u64,
    source: &mut dyn VolumeRead,
) -> PhoenixError {
    let smallest = minimum_viable_target(
        snapshot,
        source_extents,
        sector_size,
        cluster_size,
        target_size_bytes,
        original_size_bytes,
    );
    let advice = match smallest {
        Some(bytes) => format!(
            " The smallest target that works is {bytes} bytes ({:.1} GB).",
            bytes as f64 / 1e9
        ),
        None => String::new(),
    };
    match reason {
        Infeasible::Capacity(detail) => PhoenixError::Other(format!("{detail}{advice}")),
        Infeasible::Record(report) => {
            let name = source_mft_extents(source, snapshot)
                .ok()
                .and_then(|extents| record_name(source, snapshot, &extents, report.record_index))
                .map(|n| format!(" ({n})"))
                .unwrap_or_default();
            PhoenixError::Other(format!(
                "cannot shrink this NTFS partition to {target_size_bytes} bytes: MFT record {}{} \
                 is so fragmented that its relocated run list needs {} bytes of record space but \
                 only {} are available, and its clusters cannot be given a contiguous home below \
                 the new boundary.{advice}",
                report.record_index, name, report.needed_bytes, report.available,
            ))
        }
    }
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

    // ---- end-to-end pre-flight over a synthetic NTFS volume ---------------

    const REC: usize = 1024;
    const MFT_LCN: u64 = 4;

    /// A boot sector plus a two-cluster MFT holding the given records.
    /// Only the boot sector and MFT are materialised — the pre-flight never
    /// reads file data, so the clusters those records point at need not
    /// exist in the image.
    fn volume_image(records: &[Vec<u8>]) -> Vec<u8> {
        let cluster = 4096usize;
        let mut image = vec![0u8; 32 * cluster];
        image[3..7].copy_from_slice(b"NTFS");
        image[11..13].copy_from_slice(&512u16.to_le_bytes());
        image[13] = (cluster / 512) as u8;
        image[48..56].copy_from_slice(&MFT_LCN.to_le_bytes());
        image[56..64].copy_from_slice(&(MFT_LCN + 16).to_le_bytes());
        image[64] = 0xF6; // -10 => 1024-byte records

        let mft = MFT_LCN as usize * cluster;
        // Record 0: $MFT's own $DATA, describing the two clusters above.
        let rec0 = file_record(
            &[(2u64, MFT_LCN)],
            0,
        );
        image[mft..mft + REC].copy_from_slice(&rec0);
        for (i, rec) in records.iter().enumerate() {
            let at = mft + (i + 1) * REC;
            image[at..at + REC].copy_from_slice(rec);
        }
        image
    }

    /// A FILE record with one non-resident $DATA holding `runs`, followed by
    /// a resident filler attribute of `filler` bytes — the filler is how a
    /// test controls how much slack the record has left for growth.
    fn file_record(runs: &[(u64, u64)], filler: usize) -> Vec<u8> {
        let encoded = encode_runs(runs);
        let attr_len = (64 + encoded.len()).div_ceil(8) * 8;
        let mut rec = vec![0u8; REC];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&48u16.to_le_bytes());
        rec[6..8].copy_from_slice(&3u16.to_le_bytes());
        rec[20..22].copy_from_slice(&56u16.to_le_bytes());
        rec[28..32].copy_from_slice(&(REC as u32).to_le_bytes());

        let a = 56usize;
        rec[a..a + 4].copy_from_slice(&0x80u32.to_le_bytes()); // $DATA
        rec[a + 4..a + 8].copy_from_slice(&(attr_len as u32).to_le_bytes());
        rec[a + 8] = 1; // non-resident
        rec[a + 10..a + 12].copy_from_slice(&64u16.to_le_bytes());
        rec[a + 32..a + 34].copy_from_slice(&64u16.to_le_bytes());
        rec[a + 64..a + 64 + encoded.len()].copy_from_slice(&encoded);

        let mut end = a + attr_len;
        if filler > 0 {
            let flen = (24 + filler).div_ceil(8) * 8;
            rec[end..end + 4].copy_from_slice(&0x30u32.to_le_bytes()); // $FILE_NAME
            rec[end + 4..end + 8].copy_from_slice(&(flen as u32).to_le_bytes());
            rec[end + 16..end + 20].copy_from_slice(&(filler as u32).to_le_bytes());
            rec[end + 20..end + 22].copy_from_slice(&24u16.to_le_bytes());
            end += flen;
        }
        rec[end..end + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        rec[24..28].copy_from_slice(&((end + 4) as u32).to_le_bytes());

        let usn: u16 = 1;
        rec[48..50].copy_from_slice(&usn.to_le_bytes());
        for i in 1..3usize {
            let stride_end = i * 512;
            rec[48 + i * 2] = rec[stride_end - 2];
            rec[48 + i * 2 + 1] = rec[stride_end - 1];
            rec[stride_end - 2..stride_end].copy_from_slice(&usn.to_le_bytes());
        }
        rec
    }

    /// Encode (length, lcn) pairs the way NTFS does, so the fixture's run
    /// lists are byte-accurate.
    fn encode_runs(runs: &[(u64, u64)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prev: i64 = 0;
        for &(length, lcn) in runs {
            let delta = lcn as i64 - prev;
            let len_bytes = signed_width(length as i64).max(width(length));
            let lcn_bytes = signed_width(delta);
            out.push((lcn_bytes << 4) | len_bytes);
            for i in 0..len_bytes {
                out.push(((length >> (i * 8)) & 0xFF) as u8);
            }
            for i in 0..lcn_bytes {
                out.push(((delta >> (i * 8)) & 0xFF) as u8);
            }
            prev = lcn as i64;
        }
        out.push(0);
        out
    }

    fn width(v: u64) -> u8 {
        (1..=8u8).find(|i| v >> (*i as u32 * 8) == 0).unwrap_or(8)
    }

    fn signed_width(v: i64) -> u8 {
        if v == 0 {
            return 1;
        }
        (1..=8u8)
            .find(|n| {
                let bits = *n as u32 * 8;
                v >= -(1i64 << (bits - 1)) && v <= (1i64 << (bits - 1)) - 1
            })
            .unwrap_or(8)
    }

    struct Image(Vec<u8>);
    impl VolumeRead for Image {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let start = offset as usize;
            let end = start + buf.len();
            if end > self.0.len() {
                return Err(PhoenixError::Other("read past test image".into()));
            }
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }
    }

    /// Below-boundary used clusters leaving one 120-cluster free run at 100
    /// and eight 10-cluster runs further along, plus ten above-boundary
    /// pieces belonging to one fragmented file.
    fn fragmented_layout() -> (Vec<Extent>, Vec<(u64, u64)>) {
        let mut extents = vec![ext(0, 100 * 8)];
        // used blocks that leave 10-cluster holes at 400, 600, ... 1800
        let mut start = 220u64;
        while start < 2000 {
            let end = (start + 180).min(2000);
            extents.push(ext(start * 8, (end - start) * 8));
            start = end + 10;
        }
        let file_runs: Vec<(u64, u64)> = (0..10).map(|i| (10u64, 2000 + i * 20)).collect();
        for &(len, lcn) in &file_runs {
            extents.push(ext(lcn * 8, len * 8));
        }
        (extents, file_runs)
    }

    const TARGET: u64 = 2000 * CLUSTER;
    const ORIGINAL: u64 = 4000 * CLUSTER;

    #[test]
    fn preflight_defragments_a_record_the_packer_would_have_overflowed() {
        let (extents, file_runs) = fragmented_layout();
        // Slack of 4 bytes: enough that the file's list fits as-is, not
        // enough to absorb the wider LCN deltas a scattered relocation
        // produces. This is the shape of the real failure.
        let image = volume_image(&[file_record(&file_runs, 840)]);

        // Without pre-flight, the plain packed map overflows the record.
        let naive = build_relocation_map(&extents, SECTOR, CLUSTER, TARGET)
            .unwrap()
            .unwrap();
        let mut probe = Image(image.clone());
        let snapshot = scan_mft(&mut probe, None).unwrap();
        let overflows = simulate_map(&snapshot, &naive);
        assert_eq!(
            overflows.len(),
            1,
            "fixture must reproduce the overflow the pre-flight exists to fix"
        );

        // With pre-flight, the record's clusters get a contiguous home and
        // the rewrite fits.
        let mut src = Image(image);
        let plan = plan_shrink(&extents, SECTOR, CLUSTER, TARGET, ORIGINAL, &mut src, None)
            .unwrap();
        assert!(
            plan.replanned_records > 0,
            "the plan should have been adjusted"
        );
        assert!(
            simulate_map(&snapshot, &plan.map).is_empty(),
            "the adjusted plan must leave every record fitting"
        );

        // And the file's ten pieces must now land consecutively.
        let mut dsts: Vec<u64> = file_runs
            .iter()
            .map(|&(_, lcn)| plan.map.translate_cluster(lcn).unwrap())
            .collect();
        dsts.sort();
        for pair in dsts.windows(2) {
            assert_eq!(pair[1] - pair[0], 10, "pieces should be back to back");
        }
    }

    #[test]
    fn preflight_keeps_the_map_structurally_valid() {
        let (extents, file_runs) = fragmented_layout();
        let image = volume_image(&[file_record(&file_runs, 840)]);
        let mut src = Image(image);
        let plan =
            plan_shrink(&extents, SECTOR, CLUSTER, TARGET, ORIGINAL, &mut src, None).unwrap();
        let layout = layout_for(&extents, SECTOR, CLUSTER, TARGET).unwrap();
        // Every above-boundary cluster still accounted for, nothing
        // overlapping, nothing landing outside free space.
        verify_map(&plan.map, &layout).unwrap();
    }

    #[test]
    fn preflight_is_deterministic() {
        let (extents, file_runs) = fragmented_layout();
        let image = volume_image(&[file_record(&file_runs, 840)]);
        let a = plan_shrink(
            &extents,
            SECTOR,
            CLUSTER,
            TARGET,
            ORIGINAL,
            &mut Image(image.clone()),
            None,
        )
        .unwrap();
        let b = plan_shrink(
            &extents,
            SECTOR,
            CLUSTER,
            TARGET,
            ORIGINAL,
            &mut Image(image),
            None,
        )
        .unwrap();
        assert_eq!(a.map.entries, b.map.entries);
    }

    #[test]
    fn a_shrink_with_nothing_past_the_boundary_skips_the_scan() {
        // No above-boundary data at all: the map is an identity transform,
        // so there is nothing to simulate and no MFT to read. The image here
        // is deliberately garbage — reaching the scanner would error.
        let extents = vec![ext(0, 100 * 8)];
        struct Unreadable;
        impl VolumeRead for Unreadable {
            fn read_exact_at(&mut self, _: u64, _: &mut [u8]) -> Result<()> {
                panic!("pre-flight must not read the volume when nothing moves");
            }
        }
        let plan = plan_shrink(
            &extents,
            SECTOR,
            CLUSTER,
            TARGET,
            ORIGINAL,
            &mut Unreadable,
            None,
        )
        .unwrap();
        assert!(plan.map.entries.is_empty());
        assert_eq!(plan.replanned_records, 0);
    }

    #[test]
    fn an_impossible_shrink_names_the_smallest_target_that_works() {
        // Far more used data than the target can hold.
        let (extents, file_runs) = fragmented_layout();
        let image = volume_image(&[file_record(&file_runs, 840)]);
        let mut src = Image(image);
        let err = plan_shrink(
            &extents,
            SECTOR,
            CLUSTER,
            300 * CLUSTER,
            ORIGINAL,
            &mut src,
            None,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("smallest target that works"),
            "error should tell the user what would work: {msg}"
        );
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
