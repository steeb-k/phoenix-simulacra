use serde::{Deserialize, Deserializer, Serialize};

use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{DiskInfo, FilesystemKind};
use phoenix_core::error::{PhoenixError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlan {
    pub backup_path: String,
    pub target_disk_index: u32,
    pub entries: Vec<RestorePlanEntry>,
    /// When true, the GUI built a full-disk layout from the backup.
    #[serde(default)]
    pub full_disk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlanEntry {
    #[serde(default, deserialize_with = "deserialize_source_partition")]
    pub source_partition_index: Option<u32>,
    #[serde(default)]
    pub target_partition_index: u32,
    #[serde(default = "default_true")]
    pub restore: bool,
    pub target_offset_bytes: u64,
    pub target_size_bytes: u64,
}

fn default_true() -> bool {
    true
}

/// TOML plans may use `source_partition_index = 2` (legacy integer) or omit it.
fn deserialize_source_partition<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SourceField {
        Present(u32),
        Opt(Option<u32>),
    }
    match SourceField::deserialize(deserializer)? {
        SourceField::Present(n) => Ok(Some(n)),
        SourceField::Opt(o) => Ok(o),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    FullDisk,
    Partial,
}

impl RestorePlan {
    pub fn from_toml(path: &std::path::Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| PhoenixError::Plan(format!("read plan: {e}")))?;
        toml::from_str(&s).map_err(|e| PhoenixError::Plan(format!("parse plan: {e}")))
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| PhoenixError::Plan(format!("serialize plan: {e}")))
    }

    pub fn mode(&self) -> RestoreMode {
        if self.full_disk {
            return RestoreMode::FullDisk;
        }
        let restoring = self.entries.iter().filter(|e| e.restore).count();
        let with_source = self
            .entries
            .iter()
            .filter(|e| e.restore && e.source_partition_index.is_some())
            .count();
        if restoring > 0 && with_source == restoring && self.covers_all_backup_partitions() {
            RestoreMode::FullDisk
        } else {
            RestoreMode::Partial
        }
    }

    fn covers_all_backup_partitions(&self) -> bool {
        // Heuristic without reader: full_disk flag is authoritative when set.
        false
    }

    pub fn layout_entries(&self) -> Vec<&RestorePlanEntry> {
        self.entries
            .iter()
            .filter(|e| e.restore || self.mode() == RestoreMode::Partial)
            .collect()
    }

    pub fn validate_against_backup(
        &self,
        manifest: &phoenix_core::manifest::BackupManifest,
    ) -> Result<()> {
        for entry in &self.entries {
            if !entry.restore {
                continue;
            }
            let Some(src) = entry.source_partition_index else {
                return Err(PhoenixError::Plan(format!(
                    "target partition {} is marked restore but has no source mapping",
                    entry.target_partition_index
                )));
            };
            let part = manifest
                .partitions
                .iter()
                .find(|p| p.index == src)
                .ok_or_else(|| PhoenixError::Plan(format!("partition {src} not in backup")))?;
            if part.used_bytes > entry.target_size_bytes {
                return Err(PhoenixError::PartitionTooSmall {
                    partition_index: src,
                    target_size: entry.target_size_bytes,
                    required: part.used_bytes,
                });
            }
        }
        Ok(())
    }

    /// Verify the target layout is physically sane before any writes: no
    /// two partitions overlap and none runs past the end of the target
    /// disk. Guards against a hand-edited `plan.toml` and against a bug in
    /// the auto-layout (`expand_ntfs_slack`) producing an overlapping plan.
    /// `disk_size` is the target disk's total byte size.
    pub fn validate_layout(&self, disk_size: u64) -> Result<()> {
        if let Some(problem) = layout_problem(&self.entries, disk_size) {
            return Err(PhoenixError::Plan(problem));
        }
        Ok(())
    }

    pub fn validate_extents_fit(
        &self,
        reader: &mut phoenix_core::container::PhnxReader,
    ) -> Result<()> {
        for entry in &self.entries {
            if !entry.restore {
                continue;
            }
            let Some(src) = entry.source_partition_index else {
                continue;
            };
            let idx_entry = reader
                .index
                .iter()
                .find(|e| e.index == src)
                .cloned()
                .ok_or_else(|| {
                    PhoenixError::Plan(format!("partition {src} not in backup index"))
                })?;
            if entry.target_size_bytes >= idx_entry.original_size {
                continue;
            }
            let stream = reader.read_stream_header(&idx_entry)?;
            let sector_size = idx_entry.sector_size as u64;
            if sector_size == 0 {
                return Err(PhoenixError::Plan(format!(
                    "partition {src} has zero sector_size in backup index"
                )));
            }

            if matches!(idx_entry.fs_kind, FilesystemKind::Ntfs) {
                let cluster_size = stream.bytes_per_cluster as u64;
                match crate::relocation::build_relocation_map(
                    &stream.extents,
                    sector_size,
                    cluster_size,
                    entry.target_size_bytes,
                ) {
                    Ok(_) => continue,
                    Err(e) => {
                        return Err(PhoenixError::Plan(format!(
                            "NTFS partition {src} cannot be shrunk to {} bytes: {e}",
                            entry.target_size_bytes
                        )));
                    }
                }
            }

            let max_sector = entry.target_size_bytes / sector_size;
            let mut offending = Vec::new();
            for (idx, ext) in stream.extents.iter().enumerate() {
                let last_sector = ext.start_sector.saturating_add(ext.sector_count);
                if last_sector > max_sector {
                    offending.push((idx, ext.start_sector, ext.sector_count, last_sector));
                }
            }
            if !offending.is_empty() {
                let sample = offending
                    .iter()
                    .take(3)
                    .map(|(i, s, c, l)| {
                        format!("extent[{i}]: start={s}, count={c}, last_sector={l}")
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(PhoenixError::Plan(format!(
                    "partition {src} cannot shrink to {} bytes: {} of {} extents past sector {} ({sample})",
                    entry.target_size_bytes,
                    offending.len(),
                    stream.extents.len(),
                    max_sector,
                )));
            }
        }
        Ok(())
    }
}

/// Return a human-readable description of the first layout violation
/// (overlap or past-end-of-disk) among the space-occupying entries, or
/// `None` if the layout is sound. Entries with zero size are ignored (they
/// don't occupy the disk). Used both by `RestorePlan::validate_layout` and
/// by `expand_ntfs_slack` to reject its own bad output.
fn layout_problem(entries: &[RestorePlanEntry], disk_size: u64) -> Option<String> {
    let mut spans: Vec<(u64, u64, u32)> = entries
        .iter()
        .filter(|e| e.target_size_bytes > 0)
        .map(|e| {
            (
                e.target_offset_bytes,
                e.target_offset_bytes.saturating_add(e.target_size_bytes),
                e.target_partition_index,
            )
        })
        .collect();
    spans.sort_by_key(|s| s.0);

    for w in spans.windows(2) {
        let (a_start, a_end, a_idx) = w[0];
        let (b_start, _b_end, b_idx) = w[1];
        if a_end > b_start {
            return Some(format!(
                "partition {a_idx} (bytes {a_start}..{a_end}) overlaps partition {b_idx} \
                 starting at byte {b_start}"
            ));
        }
    }
    if let Some(&(start, end, idx)) = spans.last() {
        if disk_size > 0 && end > disk_size {
            return Some(format!(
                "partition {idx} (bytes {start}..{end}) extends past the target disk end \
                 ({disk_size} bytes)"
            ));
        }
    }
    None
}

pub fn alignment_bytes(reader: &PhnxReader) -> u64 {
    if reader.header.flags & 1 != 0 {
        1024 * 1024
    } else {
        1024 * 512
    }
}

pub fn align_up(value: u64, align: u64) -> u64 {
    if align == 0 {
        return value;
    }
    value.div_ceil(align) * align
}

pub fn align_down(value: u64, align: u64) -> u64 {
    if align == 0 {
        return value;
    }
    (value / align) * align
}

pub fn default_plan_from_backup(
    backup_path: &str,
    reader: &PhnxReader,
    target_disk_index: u32,
    target_disk_size: u64,
) -> RestorePlan {
    build_full_disk_plan(backup_path, reader, target_disk_index, target_disk_size)
}

/// Pack partitions of the given `sizes` sequentially onto a `disk_size`-byte
/// GPT disk: each start is aligned, and the final partition is clamped so the
/// layout leaves room for the backup GPT (33 LBAs at the disk end). A `.phnx`
/// records partition sizes but not their source disk offsets, so re-packing
/// from the front can drift the last partition past the disk end when the
/// source had inter-partition gaps (e.g. an MSR partition) — the clamp keeps
/// the reconstructed layout valid. Returns `(offset, size)` per input size.
fn pack_full_disk(sizes: &[u64], disk_size: u64, align: u64) -> Vec<(u64, u64)> {
    // GPT reserves 34 LBAs at the front (protective MBR + primary header +
    // entry array) and 33 at the back (backup entry array + header). No
    // partition may start before the front reserve or end past the back one.
    const GPT_LEADING_BYTES: u64 = 34 * 512;
    const GPT_TRAILING_BYTES: u64 = 33 * 512;
    let usable_end = disk_size.saturating_sub(GPT_TRAILING_BYTES);
    let n = sizes.len();
    if n == 0 {
        return Vec::new();
    }
    let last = sizes[n - 1];
    let min_lead_4k = align_up(GPT_LEADING_BYTES, 4096);

    // Pass 1 (preferred): pack EVERY partition sequentially from the front at
    // 1 MiB alignment. When it fits, this is the layout to use — it leaves the
    // extra room as trailing free space, which `expand_ntfs_slack` then grows
    // the data partition into (so restoring to a larger disk actually enlarges
    // the volume rather than stranding a gap).
    {
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(n);
        let mut offset = align;
        for &sz in sizes {
            let off = align_up(offset, align);
            out.push((off, sz));
            offset = off + sz;
        }
        if out.last().map(|(o, s)| o + s <= usable_end).unwrap_or(true) {
            return out;
        }
    }

    // Pass 2: front-packing overshot — the source packed the partitions tight
    // (no 1 MiB lead) and they nearly fill the disk. A `.phnx` records sizes but
    // not source offsets, so reconstruct by packing the earlier partitions from
    // the front and ANCHORING the last partition's end at the disk's usable end,
    // keeping every original size (no shrink — essential for raw-captured exFAT
    // or fixed-geometry FAT that can't be resized). Try 1 MiB first, then 4 KiB
    // alignment with a minimal GPT-boundary lead; the first that fits wins.
    for &(lead, a) in &[
        (align.max(GPT_LEADING_BYTES), align),
        (min_lead_4k, 4096u64),
    ] {
        let anchored_start = align_down(usable_end.saturating_sub(last), a);
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(n);
        let mut offset = lead;
        for &sz in &sizes[..n - 1] {
            let off = align_up(offset, a).max(lead);
            out.push((off, sz));
            offset = off + sz;
        }
        let earlier_end = out.last().map(|(o, s)| o + s).unwrap_or(lead);
        if last > 0 && anchored_start >= earlier_end {
            out.push((anchored_start, last));
            return out;
        }
    }

    // Tightest fallback: the partitions genuinely don't fit at full size. Pack
    // sequentially at 4 KiB from the GPT boundary and clamp the last partition
    // to leave room for the backup GPT. `validate_against_backup` then enforces
    // that the used data still fits and surfaces PartitionTooSmall if not.
    let a = 4096u64;
    let mut out = Vec::with_capacity(n);
    let mut offset = min_lead_4k;
    for (i, &sz) in sizes.iter().enumerate() {
        let off = align_up(offset, a).max(min_lead_4k);
        let size = if i + 1 == n && off.saturating_add(sz) > usable_end {
            align_down(usable_end.saturating_sub(off), a)
        } else {
            sz
        };
        out.push((off, size));
        offset = off + size;
    }
    out
}

pub fn build_full_disk_plan(
    backup_path: &str,
    reader: &PhnxReader,
    target_disk_index: u32,
    target_disk_size: u64,
) -> RestorePlan {
    let align = alignment_bytes(reader);
    // The final 33 LBAs of a GPT disk hold the backup GPT header + entry array;
    // no partition may end past there or IOCTL_DISK_SET_DRIVE_LAYOUT_EX rejects
    // the layout. A `.phnx` records partition sizes but not their source disk
    // offsets, so re-packing from the front can drift the last partition past
    // the disk end when the source had inter-partition gaps (e.g. an MSR
    // partition). Aligning each start and clamping the final partition keeps
    // the reconstructed layout valid.
    let sizes: Vec<u64> = reader.index.iter().map(|e| e.original_size).collect();
    let placed = pack_full_disk(&sizes, target_disk_size, align);
    // Diagnostic: dump the reconstructed full-disk layout so a restore that
    // fails validation can be traced back to the exact source sizes vs. placed
    // offsets/sizes without another round-trip.
    for (e, (off, size)) in reader.index.iter().zip(&placed) {
        tracing::info!(
            partition = e.index,
            original_size = e.original_size,
            used_bytes = e.used_bytes,
            target_offset = off,
            target_size = size,
            disk_size = target_disk_size,
            "full-disk plan placement"
        );
    }
    let mut entries: Vec<RestorePlanEntry> = reader
        .index
        .iter()
        .zip(placed)
        .map(|(e, (off, size))| RestorePlanEntry {
            source_partition_index: Some(e.index),
            target_partition_index: e.index,
            restore: true,
            target_offset_bytes: off,
            target_size_bytes: size,
        })
        .collect();

    expand_ntfs_slack(&mut entries, reader, target_disk_size, align);

    RestorePlan {
        backup_path: backup_path.to_string(),
        target_disk_index,
        entries,
        full_disk: true,
    }
}

/// Grow NTFS partition(s) into free space at the end of the disk. Partitions that
/// sat after NTFS in the backup layout (e.g. Windows RE) are moved to the tail
/// so NTFS can expand without overlapping them.
fn expand_ntfs_slack(
    entries: &mut [RestorePlanEntry],
    reader: &PhnxReader,
    target_disk_size: u64,
    align: u64,
) {
    // Snapshot the pre-expansion offsets/sizes so we can revert if the
    // repack produces an overlapping or out-of-bounds layout (pathological
    // multi-NTFS / trailing-partition arrangements). A non-expanded plan is
    // always valid; a corrupt one would fail the restore, so falling back
    // is strictly safer.
    let snapshot: Vec<(u64, u64)> = entries
        .iter()
        .map(|e| (e.target_offset_bytes, e.target_size_bytes))
        .collect();

    expand_ntfs_slack_inner(entries, reader, target_disk_size, align);

    if let Some(problem) = layout_problem(entries, target_disk_size) {
        tracing::warn!(
            problem = %problem,
            "NTFS slack expansion produced an invalid layout; reverting to the un-expanded plan"
        );
        for (e, (off, size)) in entries.iter_mut().zip(snapshot) {
            e.target_offset_bytes = off;
            e.target_size_bytes = size;
        }
    }
}

fn expand_ntfs_slack_inner(
    entries: &mut [RestorePlanEntry],
    reader: &PhnxReader,
    target_disk_size: u64,
    align: u64,
) {
    let ntfs_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.restore
                && reader
                    .index
                    .iter()
                    .find(|i| i.index == e.source_partition_index.unwrap_or(u32::MAX))
                    .is_some_and(|i| i.fs_kind == FilesystemKind::Ntfs)
        })
        .map(|(i, _)| i)
        .collect();

    if ntfs_indices.is_empty() {
        return;
    }

    // Latest-starting NTFS is treated as the main data partition to expand.
    let primary_ntfs = *ntfs_indices
        .iter()
        .max_by_key(|&&i| entries[i].target_offset_bytes)
        .unwrap();
    let ntfs_start = entries[primary_ntfs].target_offset_bytes;
    let ntfs_orig_end = ntfs_start.saturating_add(entries[primary_ntfs].target_size_bytes);

    // Tail = every other partition at/after the original NTFS end (RE, etc.).
    let mut tail: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(i, e)| {
            *i != primary_ntfs
                && !ntfs_indices.contains(i)
                && e.target_offset_bytes >= ntfs_orig_end.saturating_sub(align)
        })
        .map(|(i, _)| i)
        .collect();
    tail.sort_by_key(|&i| entries[i].target_offset_bytes);

    // The last ~33 sectors of a GPT disk are reserved for the backup GPT, so a
    // partition can't reach the very end or IOCTL_DISK_SET_DRIVE_LAYOUT_EX fails
    // with Win32 87. Leave one alignment unit (>= the backup GPT's 16.5 KiB)
    // free at the tail.
    let usable_end = align_down(target_disk_size, align).saturating_sub(align);

    // Pack tail partitions against the (usable) end of the disk.
    let mut cursor = usable_end;
    for &i in tail.iter().rev() {
        let size = entries[i].target_size_bytes;
        if size > cursor {
            break;
        }
        cursor = cursor.saturating_sub(size);
        entries[i].target_offset_bytes = align_down(cursor, align);
        cursor = entries[i].target_offset_bytes;
    }

    // Main NTFS fills the gap from its start to the first tail partition (or the
    // usable disk end).
    let ntfs_end = if let Some(&first_tail) = tail.first() {
        entries[first_tail].target_offset_bytes
    } else {
        usable_end
    };

    if ntfs_end > ntfs_start {
        entries[primary_ntfs].target_size_bytes =
            align_down(ntfs_end.saturating_sub(ntfs_start), align);
    }

    // Additional NTFS volumes (rare) share any space after the tail pack.
    let extra_ntfs: Vec<usize> = ntfs_indices
        .iter()
        .copied()
        .filter(|&i| i != primary_ntfs)
        .collect();
    if extra_ntfs.is_empty() {
        return;
    }
    let layout_end = entries
        .iter()
        .map(|e| e.target_offset_bytes.saturating_add(e.target_size_bytes))
        .max()
        .unwrap_or(0);
    let trailing = target_disk_size.saturating_sub(layout_end);
    if trailing < align {
        return;
    }
    let total_extra_orig: u64 = extra_ntfs
        .iter()
        .map(|&i| entries[i].target_size_bytes)
        .sum();
    let mut remaining = trailing;
    for (pos, &idx) in extra_ntfs.iter().enumerate() {
        let share = if pos + 1 == extra_ntfs.len() {
            remaining
        } else if total_extra_orig > 0 {
            trailing * entries[idx].target_size_bytes / total_extra_orig
        } else {
            trailing / extra_ntfs.len() as u64
        };
        remaining = remaining.saturating_sub(share);
        entries[idx].target_size_bytes =
            align_up(entries[idx].target_size_bytes.saturating_add(share), align);
    }
}

/// Build a plan covering every target partition; only mapped slots have `restore: true`.
pub fn build_partial_plan(
    backup_path: &str,
    target_disk_index: u32,
    target: &DiskInfo,
    assignments: &[(u32, u32, u64, u64)], // target_part, source_part, offset, size
) -> RestorePlan {
    let mut entries: Vec<RestorePlanEntry> = target
        .partitions
        .iter()
        .map(|p| {
            let mapped = assignments.iter().find(|(t, _, _, _)| *t == p.index);
            RestorePlanEntry {
                source_partition_index: mapped.map(|(_, s, _, _)| *s),
                target_partition_index: p.index,
                restore: mapped.is_some(),
                target_offset_bytes: mapped.map(|(_, _, o, _)| *o).unwrap_or(p.offset_bytes),
                target_size_bytes: mapped.map(|(_, _, _, sz)| *sz).unwrap_or(p.size_bytes),
            }
        })
        .collect();
    entries.sort_by_key(|e| e.target_offset_bytes);

    RestorePlan {
        backup_path: backup_path.to_string(),
        target_disk_index,
        entries,
        full_disk: false,
    }
}

pub fn partition_allows_resize(fs: FilesystemKind) -> bool {
    matches!(
        fs,
        FilesystemKind::Ntfs | FilesystemKind::Fat | FilesystemKind::Exfat
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(idx: u32, offset: u64, size: u64) -> RestorePlanEntry {
        RestorePlanEntry {
            source_partition_index: Some(idx),
            target_partition_index: idx,
            restore: true,
            target_offset_bytes: offset,
            target_size_bytes: size,
        }
    }

    #[test]
    fn valid_layout_passes() {
        let entries = vec![
            entry(0, 1024, 1000),
            entry(1, 2024, 1000),
            entry(2, 3024, 500),
        ];
        assert!(layout_problem(&entries, 10_000).is_none());
    }

    #[test]
    fn overlapping_layout_is_rejected() {
        // Entry 1 starts at 1500, inside entry 0's 1024..2024 span.
        let entries = vec![entry(0, 1024, 1000), entry(1, 1500, 1000)];
        let problem = layout_problem(&entries, 10_000).expect("overlap must be caught");
        assert!(problem.contains("overlap"), "got: {problem}");
    }

    #[test]
    fn past_disk_end_is_rejected() {
        let entries = vec![entry(0, 1024, 1000), entry(1, 9500, 1000)];
        let problem = layout_problem(&entries, 10_000).expect("out-of-bounds must be caught");
        assert!(
            problem.contains("past the target disk end"),
            "got: {problem}"
        );
    }

    #[test]
    fn full_disk_pack_leaves_room_for_backup_gpt() {
        // Reproduces the FAT/exFAT failure: a 16 MiB MSR-like partition plus a
        // data partition sized to reach the source's usable end. Re-packed onto
        // a same-size disk it would overshoot; pack_full_disk must clamp it.
        let disk = 512 * 1024 * 1024u64;
        let align = 1024 * 1024u64;
        let msr = 16 * 1024 * 1024u64;
        // Data sized so msr + data + 1 MiB lead > disk (forces a clamp).
        let data = disk - align - msr + 2 * 1024 * 1024;
        let placed = pack_full_disk(&[msr, data], disk, align);
        let last = placed.last().unwrap();
        let end = last.0 + last.1;
        assert!(
            end <= disk - 33 * 512,
            "last partition ends at {end}, past the GPT usable end on a {disk}-byte disk"
        );
        // Starts stay at least logical-sector-aligned (the tight fallback relaxes
        // the 1 MiB preference to 4 KiB when partitions nearly fill the disk).
        for (off, _) in &placed {
            assert_eq!(off % 4096, 0, "offset {off} not 4 KiB-aligned");
        }
    }

    #[test]
    fn full_disk_pack_relaxes_alignment_before_shrinking() {
        // The exact geometry from the failing exFAT roundtrip on a 512 MiB VHD:
        // diskpart put the data partition immediately after the MSR (start
        // 16825856 — NOT MiB-aligned) running to the usable end, so its
        // original 520028160 bytes only fit if the re-pack keeps the tight
        // start. Aligning up to 1 MiB drifted the start to 17825792 and the
        // clamp shrank the partition by ~980 KiB → PartitionTooSmall for a
        // fixed-geometry filesystem. The pack must keep the ORIGINAL size by
        // relaxing the start to 4 KiB alignment instead.
        // Exact sizes captured from the failing hardware run (via the test's
        // diagnostic print): MSR + exFAT sum to 536787968 on a 536870912-byte
        // disk, leaving only ~80 KiB — no room for a 1 MiB lead.
        let disk = 536_870_912u64;
        let align = 1024 * 1024u64;
        let msr = 16_759_808u64;
        let data = 520_028_160u64; // reaches ~the GPT usable end
        let placed = pack_full_disk(&[msr, data], disk, align);
        let (off, size) = placed[1];
        assert_eq!(size, data, "fixed-geometry partition must not be shrunk");
        assert_eq!(off % 512, 0, "start must stay logical-sector-aligned");
        assert!(
            off + size <= disk - 33 * 512,
            "must still leave room for the backup GPT (ends at {})",
            off + size
        );
    }

    #[test]
    fn full_disk_pack_anchors_last_without_shrinking() {
        // MSR (15 MiB) + data (490 MiB) on a 512 MiB disk. Anchoring places the
        // data partition at its true end position keeping its full size (no
        // shrink), which is what stops raw/exFAT restores from tripping
        // PartitionTooSmall.
        let disk = 512 * 1024 * 1024u64;
        let align = 1024 * 1024u64;
        let msr = 15 * 1024 * 1024u64;
        let data = 490 * 1024 * 1024u64;
        let placed = pack_full_disk(&[msr, data], disk, align);
        assert_eq!(placed[1].1, data, "last partition size must be preserved");
        let end = placed[1].0 + placed[1].1;
        assert!(
            end <= disk - 33 * 512,
            "last partition must leave room for backup GPT"
        );
        // No overlap with the MSR.
        assert!(
            placed[0].0 + placed[0].1 <= placed[1].0,
            "MSR overlaps data"
        );
    }

    #[test]
    fn full_disk_pack_front_packs_when_there_is_room() {
        // MSR (16 MiB) + NTFS (239 MiB) restored onto a 768 MiB disk. There's
        // plenty of room, so the partitions must be FRONT-packed (NTFS right
        // after the MSR), leaving the extra space as a trailing gap for
        // expand_ntfs_slack to grow into — NOT anchored at the disk end.
        let disk = 768 * 1024 * 1024u64;
        let align = 1024 * 1024u64;
        let msr = 16 * 1024 * 1024u64;
        let ntfs = 239 * 1024 * 1024u64;
        let placed = pack_full_disk(&[msr, ntfs], disk, align);
        // NTFS packed right after the MSR (near the front), not at the end.
        assert_eq!(placed[0], (align, msr));
        assert_eq!(
            placed[1].0,
            align + msr,
            "NTFS must be front-packed after MSR"
        );
        // Sizes preserved and generous trailing free space remains.
        assert_eq!(placed[1].1, ntfs);
        let end = placed[1].0 + placed[1].1;
        assert!(
            usable(disk) - end > 400 * 1024 * 1024,
            "expected a large trailing gap for expansion, end={end}"
        );
    }

    fn usable(disk: u64) -> u64 {
        disk - 33 * 512
    }

    #[test]
    fn full_disk_pack_no_clamp_when_it_fits() {
        // A single partition that already fits is left untouched.
        let disk = 512 * 1024 * 1024u64;
        let align = 1024 * 1024u64;
        let size = disk - align - 33 * 512; // ends exactly at usable end
        let placed = pack_full_disk(&[size], disk, align);
        assert_eq!(placed[0], (align, size));
    }

    #[test]
    fn zero_size_entries_are_ignored() {
        // A zero-size (unmapped) slot at the same offset must not count as
        // an overlap.
        let entries = vec![entry(0, 1024, 1000), entry(1, 1024, 0)];
        assert!(layout_problem(&entries, 10_000).is_none());
    }
}
