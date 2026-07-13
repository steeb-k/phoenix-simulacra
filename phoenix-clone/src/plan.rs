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

/// What happens to the target's partition table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CloneTableMode {
    /// Wipe the target and re-initialize it with the SOURCE's table style,
    /// then plant the planned entries — the classic full-disk clone.
    #[default]
    ReinitMatchSource,
    /// Wipe the target and re-initialize it with the given style (the GUI's
    /// blank-layout / table-style-switch editor actions), then plant the
    /// planned entries.
    ReinitAs { gpt: bool },
    /// Keep the target's existing partition table and update it in place:
    /// planned entries are written (replacing whatever they land on), the
    /// listed live target partitions survive untouched, and any live
    /// partition in neither set is removed from the table. This is the
    /// partial-clone analogue of a partial restore.
    UpdateExisting {
        /// `PartitionInfo::index` values of the live target partitions to keep.
        preserved_target_indices: Vec<u32>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ClonePlan {
    pub entries: Vec<CloneEntry>,
    pub table_mode: CloneTableMode,
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
        ClonePlan {
            entries,
            table_mode: CloneTableMode::ReinitMatchSource,
        }
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

        // The last ~33 sectors of a GPT disk hold the backup GPT header + entry
        // array, so a partition can't extend all the way to the disk end or
        // IOCTL_DISK_SET_DRIVE_LAYOUT_EX rejects the layout (Win32 87). Leave one
        // alignment unit (>= the 16.5 KiB the backup GPT needs) free at the tail.
        let usable_end = align_down(target.size_bytes, align).saturating_sub(align);
        let mut cursor = usable_end;
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
            .unwrap_or(usable_end);
        let start = plan.entries[primary].target_offset_bytes;
        if primary_new_end > start {
            plan.entries[primary].target_size_bytes = align_down(primary_new_end - start, align);
        }
        plan
    }

    /// Validate the plan against the real source and target disks: every
    /// referenced source partition exists, each target size holds the source's
    /// used data, and the layout — including preserved live partitions in
    /// [`CloneTableMode::UpdateExisting`] — neither overlaps nor runs off the
    /// target.
    pub fn validate(&self, source: &DiskInfo, target: &DiskInfo) -> Result<()> {
        if self.entries.is_empty() {
            return Err(PhoenixError::Plan("clone plan has no partitions".into()));
        }
        if let CloneTableMode::UpdateExisting {
            preserved_target_indices,
        } = &self.table_mode
        {
            for idx in preserved_target_indices {
                if !target.partitions.iter().any(|p| p.index == *idx) {
                    return Err(PhoenixError::Plan(format!(
                        "preserved target partition {idx} not found on the target disk"
                    )));
                }
            }
            if target.is_gpt && target.disk_guid.is_none() {
                return Err(PhoenixError::Plan(
                    "target disk GUID unavailable; cannot update its GPT in place".into(),
                ));
            }
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

        // Overlap / bounds check. Preserved live partitions occupy the disk
        // too, so an update-existing plan must not overlap them either.
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
        if let CloneTableMode::UpdateExisting {
            preserved_target_indices,
        } = &self.table_mode
        {
            spans.extend(
                target
                    .partitions
                    .iter()
                    .filter(|p| preserved_target_indices.contains(&p.index))
                    .map(|p| (p.offset_bytes, p.offset_bytes + p.size_bytes, p.index)),
            );
        }
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
    // `checked_div` is `None` only for `align == 0`, which means "no alignment".
    value.checked_div(align).map_or(value, |q| q * align)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::disk::{CaptureMode, PartitionInfo, PartitionUsage};

    const MIB: u64 = 1024 * 1024;

    fn ntfs_part(index: u32, offset: u64, size: u64, used: u64) -> PartitionInfo {
        PartitionInfo {
            index,
            name: format!("P{index}"),
            type_guid: [0; 16],
            gpt_attributes: 0,
            offset_bytes: offset,
            size_bytes: size,
            fs_kind: FilesystemKind::Ntfs,
            capture_mode: CaptureMode::UsedBlocks,
            bitlocker: phoenix_core::disk::BitlockerState::None,
            unique_guid: [0u8; 16],
            volume_path: None,
            drive_letter: None,
            volume_label: None,
            usage: Some(PartitionUsage {
                total_bytes: size,
                free_bytes: size - used,
            }),
            sector_size: 512,
        }
    }

    fn disk(index: u32, size: u64, parts: Vec<PartitionInfo>) -> DiskInfo {
        DiskInfo {
            index,
            path: format!("\\\\.\\PhysicalDrive{index}"),
            size_bytes: size,
            is_gpt: true,
            disk_guid: Some([0; 16]),
            disk_signature: index as u64,
            sector_size: 512,
            model: None,
            drive_type: None,
            partitions: parts,
        }
    }

    #[test]
    fn expand_leaves_room_for_backup_gpt() {
        // 256 MiB NTFS at 1 MiB on a 256 MiB source -> expand onto a 768 MiB
        // target. The grown partition must NOT reach the disk end (the last
        // ~33 sectors are reserved for the backup GPT; a partition there makes
        // SET_DRIVE_LAYOUT_EX fail with Win32 87).
        let src = disk(0, 256 * MIB, vec![ntfs_part(0, MIB, 255 * MIB, 50 * MIB)]);
        let tgt = disk(1, 768 * MIB, vec![]);
        let plan = ClonePlan::expand_to_fill(&src, &tgt);

        let max_end = plan
            .entries
            .iter()
            .map(|e| e.target_offset_bytes + e.target_size_bytes)
            .max()
            .unwrap();
        assert!(
            max_end <= tgt.size_bytes - MIB,
            "grown partition ends at {max_end}, leaving no room for the backup GPT on a \
             {}-byte disk",
            tgt.size_bytes
        );
        // And the layout must be internally valid.
        plan.validate(&src, &tgt)
            .expect("expanded plan should validate");
        // It actually grew (target size > source size).
        assert!(plan.entries[0].target_size_bytes > 255 * MIB);
    }

    #[test]
    fn update_existing_overlap_with_preserved_is_rejected() {
        // Clone a 100 MiB partition to offset 1 MiB on a target whose
        // preserved partition occupies 50..150 MiB: the plan must be refused
        // even though the planned entries alone don't overlap anything.
        let src = disk(0, 256 * MIB, vec![ntfs_part(0, MIB, 100 * MIB, 10 * MIB)]);
        let tgt = disk(1, 256 * MIB, vec![ntfs_part(3, 50 * MIB, 100 * MIB, MIB)]);
        let mut plan = ClonePlan::identity(&src);
        plan.table_mode = CloneTableMode::UpdateExisting {
            preserved_target_indices: vec![3],
        };
        let err = plan.validate(&src, &tgt).unwrap_err();
        assert!(err.to_string().contains("overlap"), "got: {err}");
    }

    #[test]
    fn update_existing_missing_preserved_partition_is_rejected() {
        let src = disk(0, 256 * MIB, vec![ntfs_part(0, MIB, 100 * MIB, 10 * MIB)]);
        let tgt = disk(1, 256 * MIB, vec![]);
        let mut plan = ClonePlan::identity(&src);
        plan.table_mode = CloneTableMode::UpdateExisting {
            preserved_target_indices: vec![7],
        };
        let err = plan.validate(&src, &tgt).unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn update_existing_beside_preserved_passes() {
        // Planned entry at 1..101 MiB, preserved partition at 150..250 MiB:
        // no overlap, plan is valid.
        let src = disk(0, 256 * MIB, vec![ntfs_part(0, MIB, 100 * MIB, 10 * MIB)]);
        let tgt = disk(1, 512 * MIB, vec![ntfs_part(2, 150 * MIB, 100 * MIB, MIB)]);
        let mut plan = ClonePlan::identity(&src);
        plan.table_mode = CloneTableMode::UpdateExisting {
            preserved_target_indices: vec![2],
        };
        plan.validate(&src, &tgt).expect("plan should validate");
    }

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
