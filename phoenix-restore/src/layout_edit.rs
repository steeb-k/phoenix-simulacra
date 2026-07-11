//! Pure byte-space math for the GUI's target-layout editor: where a dragged
//! partition may move, how far an edge may resize, and when it snaps flush to
//! the free space around it. Lives here (not in phoenix-gui) so the rules sit
//! next to plan building/validation and stay unit-testable — the GUI binary
//! embeds a requireAdministrator manifest, so its own test harness can't run
//! unelevated.
//!
//! Throughout: `obstacles` are the *other* slots of the planned layout as
//! sorted, disjoint `(start, end)` byte pairs; `disk_end` is the last usable
//! byte (callers subtract the backup-GPT tail on GPT disks).

use crate::plan::{align_down, align_up};

/// Clamp `desired` into `[lo, hi]`, preferring an aligned value. If the range
/// is too narrow to hold one, the raw clamp is returned (unaligned but never
/// overlapping); `None` only when the range itself is empty.
pub fn clamp_aligned(desired: u64, lo: u64, hi: u64, align: u64) -> Option<u64> {
    if hi < lo {
        return None;
    }
    let c = desired.clamp(lo, hi);
    let up = align_up(c, align);
    if (lo..=hi).contains(&up) {
        return Some(up);
    }
    let down = align_down(c, align);
    if (lo..=hi).contains(&down) {
        return Some(down);
    }
    Some(c)
}

/// Where a body-move drag may place the slot. The slot slides freely inside
/// its current free span and butts flush against neighbours; it hops over a
/// neighbour only when the *pointer* has crossed to that neighbour's far side
/// (and the span there fits) — the hysteresis that stops accidental reorders.
pub fn move_candidate(
    obstacles: &[(u64, u64)],
    disk_end: u64,
    cur_offset: u64,
    size: u64,
    pointer_offset: u64,
    desired: u64,
    align: u64,
) -> Option<u64> {
    if size == 0 || size > disk_end {
        return None;
    }
    // Free spans between obstacles: spans[i] sits left of obstacles[i].
    let mut spans: Vec<(u64, u64)> = Vec::with_capacity(obstacles.len() + 1);
    let mut cursor = 0u64;
    for &(o_start, o_end) in obstacles {
        spans.push((cursor, o_start));
        cursor = cursor.max(o_end);
    }
    spans.push((cursor, disk_end));

    let cur_idx = spans
        .iter()
        .position(|&(lo, hi)| lo <= cur_offset && cur_offset.saturating_add(size) <= hi)?;
    let fits = |i: usize| spans[i].1.saturating_sub(spans[i].0) >= size;

    let mut idx = cur_idx;
    for j in cur_idx + 1..spans.len() {
        // obstacles[j - 1] is the rightmost neighbour before span j; the
        // pointer being past its far edge implies it crossed everything
        // between here and there.
        if pointer_offset > obstacles[j - 1].1 && fits(j) {
            idx = j;
        }
    }
    if idx == cur_idx {
        for j in (0..cur_idx).rev() {
            if pointer_offset < obstacles[j].0 && fits(j) {
                idx = j;
            }
        }
    }

    let (lo, span_end) = spans[idx];
    clamp_aligned(desired, lo, span_end - size, align)
}

/// New left edge for a left-edge resize: clamped between the previous
/// neighbour's end and `end - min_size`, snapping flush left within
/// `snap_bytes`. The right edge (`end`) never moves.
pub fn resize_left_candidate(
    obstacles: &[(u64, u64)],
    cur_offset: u64,
    end: u64,
    min_size: u64,
    pointer_offset: u64,
    snap_bytes: u64,
    align: u64,
) -> Option<u64> {
    let lo = obstacles
        .iter()
        .filter(|&&(_, o_end)| o_end <= cur_offset)
        .map(|&(_, o_end)| o_end)
        .max()
        .unwrap_or(0);
    let hi = end.checked_sub(min_size)?;
    if hi < lo {
        return None;
    }
    // Snap keys off the raw pointer, not the aligned value — alignment must
    // never push an in-window drag out of its snap.
    if pointer_offset.clamp(lo, hi).saturating_sub(lo) <= snap_bytes {
        return Some(lo);
    }
    clamp_aligned(pointer_offset, lo, hi, align)
}

/// New right edge for a right-edge resize: clamped between `offset + min_size`
/// and the next neighbour's start (or disk end), snapping flush right within
/// `snap_bytes`. The left edge (`offset`) never moves.
pub fn resize_right_candidate(
    obstacles: &[(u64, u64)],
    disk_end: u64,
    offset: u64,
    min_size: u64,
    pointer_offset: u64,
    snap_bytes: u64,
    align: u64,
) -> Option<u64> {
    let hi = obstacles
        .iter()
        .filter(|&&(o_start, _)| o_start >= offset)
        .map(|&(o_start, _)| o_start)
        .min()
        .unwrap_or(disk_end);
    let lo = offset.checked_add(min_size)?;
    if hi < lo {
        return None;
    }
    // Same raw-pointer snap rule as the left edge.
    if hi.saturating_sub(pointer_offset.clamp(lo, hi)) <= snap_bytes {
        return Some(hi);
    }
    clamp_aligned(pointer_offset, lo, hi, align)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    // Layout used throughout: one obstacle at [30,40) MiB on a 100 MiB disk.
    const OBS: &[(u64, u64)] = &[(30 * MIB, 40 * MIB)];
    const DISK: u64 = 100 * MIB;

    #[test]
    fn move_butts_against_neighbour_without_hop() {
        // Slot [10,20) dragged right; pointer (35 MiB) hasn't crossed the
        // obstacle's far edge (40 MiB) → clamp flush at 20 MiB, no hop.
        let got = move_candidate(OBS, DISK, 10 * MIB, 10 * MIB, 35 * MIB, 25 * MIB, MIB);
        assert_eq!(got, Some(20 * MIB));
    }

    #[test]
    fn move_hops_right_only_when_pointer_crosses_far_edge() {
        let got = move_candidate(OBS, DISK, 10 * MIB, 10 * MIB, 41 * MIB, 38 * MIB, MIB);
        assert_eq!(got, Some(40 * MIB));
    }

    #[test]
    fn move_hops_left_only_when_pointer_crosses_near_edge() {
        // Slot [50,60) dragged left; pointer at 29 MiB is past the obstacle's
        // left edge (30 MiB) → lands in [0,30), clamped to fit: 20 MiB.
        let got = move_candidate(OBS, DISK, 50 * MIB, 10 * MIB, 29 * MIB, 25 * MIB, MIB);
        assert_eq!(got, Some(20 * MIB));
        // Pointer exactly on the edge does NOT hop.
        let no_hop = move_candidate(OBS, DISK, 50 * MIB, 10 * MIB, 30 * MIB, 25 * MIB, MIB);
        assert_eq!(no_hop, Some(40 * MIB));
    }

    #[test]
    fn move_refuses_span_too_small() {
        // Slot of 40 MiB can't hop into [0,30) even with the pointer across.
        let got = move_candidate(OBS, DISK, 40 * MIB, 40 * MIB, 5 * MIB, 5 * MIB, MIB);
        assert_eq!(got, Some(40 * MIB)); // clamped inside its own span instead
    }

    #[test]
    fn resize_right_clamps_and_snaps_to_next_start() {
        // Slot at 10 MiB growing right; next obstacle starts at 30 MiB.
        let snap = MIB / 2;
        // Well short of the boundary: aligned pass-through.
        assert_eq!(
            resize_right_candidate(OBS, DISK, 10 * MIB, 2 * MIB, 25 * MIB, snap, MIB),
            Some(25 * MIB)
        );
        // Within the snap window: flush fill.
        assert_eq!(
            resize_right_candidate(OBS, DISK, 10 * MIB, 2 * MIB, 30 * MIB - MIB / 4, snap, MIB),
            Some(30 * MIB)
        );
        // Past the boundary: clamped, never reorders.
        assert_eq!(
            resize_right_candidate(OBS, DISK, 10 * MIB, 2 * MIB, 70 * MIB, snap, MIB),
            Some(30 * MIB)
        );
        // Below the minimum size: clamped up.
        assert_eq!(
            resize_right_candidate(OBS, DISK, 10 * MIB, 2 * MIB, 5 * MIB, snap, MIB),
            Some(12 * MIB)
        );
    }

    #[test]
    fn resize_right_snaps_to_disk_end_without_neighbour() {
        let got =
            resize_right_candidate(&[], DISK, 50 * MIB, 2 * MIB, DISK - MIB / 4, MIB / 2, MIB);
        assert_eq!(got, Some(DISK));
    }

    #[test]
    fn resize_left_clamps_and_snaps_to_prev_end() {
        // Slot [10,30) shrinking/growing from the left; previous obstacle
        // [0,5). End must never move.
        let obs = &[(0, 5 * MIB)][..];
        let snap = MIB / 2;
        assert_eq!(
            resize_left_candidate(obs, 10 * MIB, 30 * MIB, 2 * MIB, 20 * MIB, snap, MIB),
            Some(20 * MIB)
        );
        // Snap flush left against the neighbour.
        assert_eq!(
            resize_left_candidate(
                obs,
                10 * MIB,
                30 * MIB,
                2 * MIB,
                5 * MIB + MIB / 4,
                snap,
                MIB
            ),
            Some(5 * MIB)
        );
        // Dragging past the neighbour clamps — the fix for the old missing
        // ResizeLeft overlap check.
        assert_eq!(
            resize_left_candidate(obs, 10 * MIB, 30 * MIB, 2 * MIB, 2 * MIB, snap, MIB),
            Some(5 * MIB)
        );
        // Dragging past the right edge respects the minimum size.
        assert_eq!(
            resize_left_candidate(obs, 10 * MIB, 30 * MIB, 2 * MIB, 50 * MIB, snap, MIB),
            Some(28 * MIB)
        );
    }

    #[test]
    fn clamp_aligned_narrow_range_returns_raw_clamp() {
        // No MiB-aligned value inside [5.25, 5.75) MiB — fall back to the raw
        // clamp rather than refusing or overlapping.
        let lo = 5 * MIB + MIB / 4;
        let hi = 5 * MIB + 3 * (MIB / 4);
        assert_eq!(clamp_aligned(3 * MIB, lo, hi, MIB), Some(lo));
        assert_eq!(clamp_aligned(u64::MAX, lo, hi, MIB), Some(hi));
        assert_eq!(clamp_aligned(0, hi, lo, MIB), None); // empty range
    }
}
