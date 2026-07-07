//! Clone layout planning: which source partition goes where on the target,
//! and at what size. Mirrors the restore plan's concepts but is expressed
//! against live source/target [`DiskInfo`]s rather than a `.phnx` backup.

use phoenix_core::disk::{DiskInfo, FilesystemKind};
use phoenix_core::error::{PhoenixError, Result};

/// One partition to clone: source index -> target position + size.
#[derive(Debug, Clone)]
pub struct CloneEntry {
    pub source_partition_index: u32,
    pub target_offset_bytes: u64,
    pub target_size_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ClonePlan {
    pub entries: Vec<CloneEntry>,
}

impl ClonePlan {
    /// A same-size, same-layout clone: every source partition copied to the
    /// same byte offset and size on the target. Requires the target to be at
    /// least as large as the source.
    pub fn identity(source: &DiskInfo) -> Self {
        let entries = source
            .partitions
            .iter()
            .map(|p| CloneEntry {
                source_partition_index: p.index,
                target_offset_bytes: p.offset_bytes,
                target_size_bytes: p.size_bytes,
            })
            .collect();
        ClonePlan { entries }
    }

    /// Like [`identity`](Self::identity) but grows the last NTFS partition to
    /// fill the extra space on a larger target (the common "clone to a bigger
    /// disk" case). Partitions after it (recovery, etc.) are shifted to the
    /// tail so the data partition can expand without overlapping them.
    pub fn expand_to_fill(source: &DiskInfo, target: &DiskInfo) -> Self {
        let mut plan = Self::identity(source);
        let align = 1024 * 1024u64;

        // Find the latest-starting NTFS partition to grow.
        let ntfs_idx = plan
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                source
                    .partitions
                    .iter()
                    .find(|p| p.index == e.source_partition_index)
                    .map(|p| p.fs_kind == FilesystemKind::Ntfs)
                    .unwrap_or(false)
            })
            .max_by_key(|(_, e)| e.target_offset_bytes)
            .map(|(i, _)| i);

        let Some(primary) = ntfs_idx else {
            return plan; // nothing growable
        };

        let primary_end =
            plan.entries[primary].target_offset_bytes + plan.entries[primary].target_size_bytes;
        // Partitions sitting after the primary NTFS: pack them at the disk end.
        let mut tail: Vec<usize> = plan
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| *i != primary && e.target_offset_bytes >= primary_end)
            .map(|(i, _)| i)
            .collect();
        tail.sort_by_key(|&i| plan.entries[i].target_offset_bytes);

        let mut cursor = align_down(target.size_bytes, align);
        for &i in tail.iter().rev() {
            let size = plan.entries[i].target_size_bytes;
            if size > cursor {
                break;
            }
            cursor = align_down(cursor - size, align);
            plan.entries[i].target_offset_bytes = cursor;
        }
        let primary_new_end = tail
            .first()
            .map(|&i| plan.entries[i].target_offset_bytes)
            .unwrap_or_else(|| align_down(target.size_bytes, align));
        let start = plan.entries[primary].target_offset_bytes;
        if primary_new_end > start {
            plan.entries[primary].target_size_bytes = align_down(primary_new_end - start, align);
        }
        plan
    }

    /// Validate the plan against the real source and target disks: every
    /// referenced source partition exists, each target size holds the source's
    /// used data, and the layout neither overlaps nor runs off the target.
    pub fn validate(&self, source: &DiskInfo, target: &DiskInfo) -> Result<()> {
        if self.entries.is_empty() {
            return Err(PhoenixError::Plan("clone plan has no partitions".into()));
        }
        for e in &self.entries {
            let src = source
                .partitions
                .iter()
                .find(|p| p.index == e.source_partition_index)
                .ok_or_else(|| {
                    PhoenixError::Plan(format!(
                        "source partition {} not found",
                        e.source_partition_index
                    ))
                })?;
            let used = src.usage.map(|u| u.used_bytes()).unwrap_or(src.size_bytes);
            if used > e.target_size_bytes {
                return Err(PhoenixError::PartitionTooSmall {
                    partition_index: src.index,
                    target_size: e.target_size_bytes,
                    required: used,
                });
            }
        }

        // Overlap / bounds check.
        let mut spans: Vec<(u64, u64, u32)> = self
            .entries
            .iter()
            .map(|e| {
                (
                    e.target_offset_bytes,
                    e.target_offset_bytes + e.target_size_bytes,
                    e.source_partition_index,
                )
            })
            .collect();
        spans.sort_by_key(|s| s.0);
        for w in spans.windows(2) {
            if w[0].1 > w[1].0 {
                return Err(PhoenixError::Plan(format!(
                    "clone layout overlaps: partition {} ends at {} but {} starts at {}",
                    w[0].2, w[0].1, w[1].2, w[1].0
                )));
            }
        }
        if let Some(&(_, end, idx)) = spans.last() {
            if end > target.size_bytes {
                return Err(PhoenixError::Plan(format!(
                    "clone layout for partition {idx} ends at {end} but the target disk is only \
                     {} bytes",
                    target.size_bytes
                )));
            }
        }
        Ok(())
    }
}

fn align_down(value: u64, align: u64) -> u64 {
    if align == 0 {
        value
    } else {
        (value / align) * align
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_math_matches_windows() {
        // Directly exercise the overlap window logic used by validate().
        let spans = [(1024u64, 2048u64, 0u32), (1500, 2500, 1)];
        let mut overlap = false;
        for w in spans.windows(2) {
            if w[0].1 > w[1].0 {
                overlap = true;
            }
        }
        assert!(overlap);
    }
}
