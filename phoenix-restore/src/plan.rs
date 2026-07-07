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

pub fn build_full_disk_plan(
    backup_path: &str,
    reader: &PhnxReader,
    target_disk_index: u32,
    target_disk_size: u64,
) -> RestorePlan {
    let align = alignment_bytes(reader);
    let mut offset = align;
    let mut entries: Vec<RestorePlanEntry> = Vec::new();

    for e in &reader.index {
        entries.push(RestorePlanEntry {
            source_partition_index: Some(e.index),
            target_partition_index: e.index,
            restore: true,
            target_offset_bytes: offset,
            target_size_bytes: e.original_size,
        });
        offset += e.original_size;
    }

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

    // Pack tail partitions against the end of the disk.
    let mut cursor = align_down(target_disk_size, align);
    for &i in tail.iter().rev() {
        let size = entries[i].target_size_bytes;
        if size > cursor {
            break;
        }
        cursor = cursor.saturating_sub(size);
        entries[i].target_offset_bytes = align_down(cursor, align);
        cursor = entries[i].target_offset_bytes;
    }

    // Main NTFS fills the gap from its start to the first tail partition (or disk end).
    let ntfs_end = if let Some(&first_tail) = tail.first() {
        entries[first_tail].target_offset_bytes
    } else {
        align_down(target_disk_size, align)
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
    fn zero_size_entries_are_ignored() {
        // A zero-size (unmapped) slot at the same offset must not count as
        // an overlap.
        let entries = vec![entry(0, 1024, 1000), entry(1, 1024, 0)];
        assert!(layout_problem(&entries, 10_000).is_none());
    }
}
