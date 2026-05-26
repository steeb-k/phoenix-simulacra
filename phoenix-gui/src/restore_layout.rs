//! Restore-page partition layout state and plan generation.

use std::collections::HashMap;
use std::path::Path;

use phoenix_core::container::{PartitionIndexEntry, PhnxReader};
use phoenix_core::disk::{DiskInfo, FilesystemKind, PartitionInfo, PartitionUsage};
use phoenix_core::manifest::fs_kind_from_string;
use phoenix_restore::plan::{
    align_up, alignment_bytes, build_full_disk_plan, build_partial_plan, partition_allows_resize,
    RestorePlan,
};

#[derive(Debug, Clone)]
pub struct TargetAssignment {
    pub source_index: u32,
    pub offset_bytes: u64,
    pub size_bytes: u64,
    pub source_used_bytes: u64,
    pub source_original_size: u64,
}

#[derive(Debug, Clone)]
pub struct RestoreLayoutState {
    pub backup_path: String,
    pub source_disk: DiskInfo,
    pub index: Vec<PartitionIndexEntry>,
    pub full_disk: bool,
    pub assignments: HashMap<u32, TargetAssignment>,
    pub align_bytes: u64,
    pub dialog_message: Option<String>,
    /// Active edge/body drag: (target_part, edge or body, start offset/size)
    pub drag: Option<LayoutDrag>,
}

#[derive(Debug, Clone)]
pub enum LayoutDrag {
    MoveBody {
        target_part: u32,
        grab_offset_in_part: u64,
    },
    ResizeLeft {
        target_part: u32,
    },
    ResizeRight {
        target_part: u32,
    },
}

pub fn backup_to_disk_info(reader: &PhnxReader) -> DiskInfo {
    let is_gpt = reader.header.flags & 1 != 0;
    let mut offset = alignment_bytes(reader);
    let mut partitions = Vec::new();
    for e in &reader.index {
        partitions.push(PartitionInfo {
            index: e.index,
            name: e.name.clone(),
            type_guid: e.type_guid,
            offset_bytes: offset,
            size_bytes: e.original_size,
            fs_kind: fs_kind_for_entry(reader, e),
            capture_mode: e.capture_mode,
            volume_path: None,
            drive_letter: None,
            volume_label: None,
            usage: Some(PartitionUsage {
                total_bytes: e.original_size,
                free_bytes: e.original_size.saturating_sub(e.used_bytes),
            }),
        });
        offset += e.original_size;
    }
    let size_bytes = offset.max(
        partitions
            .last()
            .map(|p| p.offset_bytes + p.size_bytes)
            .unwrap_or(0),
    );
    DiskInfo {
        index: 0,
        path: String::new(),
        size_bytes,
        is_gpt,
        disk_guid: None,
        disk_signature: reader.header.disk_signature,
        sector_size: 512,
        model: Some(reader.manifest.hostname.clone()),
        partitions,
    }
}

impl RestoreLayoutState {
    pub fn from_backup(path: &str, reader: &PhnxReader) -> Self {
        Self {
            backup_path: path.to_string(),
            source_disk: backup_to_disk_info(reader),
            index: reader.index.clone(),
            full_disk: false,
            assignments: HashMap::new(),
            align_bytes: alignment_bytes(reader),
            dialog_message: None,
            drag: None,
        }
    }

    pub fn source_used_bytes(&self, source_index: u32) -> u64 {
        self.index
            .iter()
            .find(|e| e.index == source_index)
            .map(|e| e.used_bytes)
            .unwrap_or(0)
    }

    pub fn source_original_size(&self, source_index: u32) -> u64 {
        self.index
            .iter()
            .find(|e| e.index == source_index)
            .map(|e| e.original_size)
            .unwrap_or(0)
    }

    pub fn apply_full_disk(&mut self, target: &DiskInfo) {
        self.full_disk = true;
        self.assignments.clear();
        let reader_path = self.backup_path.clone();
        if let Ok(reader) = PhnxReader::open(Path::new(&reader_path)) {
            let plan = build_full_disk_plan(
                &self.backup_path,
                &reader,
                target.index,
                target.size_bytes,
            );
            for (slot, entry) in plan.entries.iter().enumerate() {
                if let Some(src) = entry.source_partition_index {
                    self.assignments.insert(
                        slot as u32,
                        TargetAssignment {
                            source_index: src,
                            offset_bytes: entry.target_offset_bytes,
                            size_bytes: entry.target_size_bytes,
                            source_used_bytes: self.source_used_bytes(src),
                            source_original_size: self.source_original_size(src),
                        },
                    );
                }
            }
        }
    }

    pub fn clear_full_disk(&mut self) {
        self.full_disk = false;
        self.assignments.clear();
    }

    pub fn try_map_source_to_target(&mut self, source_index: u32, target: &DiskInfo, target_part: u32) {
        let Some(slot) = target.partitions.iter().find(|p| p.index == target_part) else {
            return;
        };
        let used = self.source_used_bytes(source_index);
        let original = self.source_original_size(source_index);

        let mut size = slot.size_bytes;
        if used > size {
            if used <= max_contiguous_free(target, slot.offset_bytes, slot.size_bytes) {
                size = align_up(used, self.align_bytes);
            } else {
                self.dialog_message = Some(format!(
                    "Not enough free space to restore partition {source_index} ({used} bytes used)."
                ));
                return;
            }
        }

        self.assignments.insert(
            target_part,
            TargetAssignment {
                source_index,
                offset_bytes: slot.offset_bytes,
                size_bytes: size,
                source_used_bytes: used,
                source_original_size: original,
            },
        );
    }

    pub fn assignment_label(&self, slot: u32, target: &DiskInfo) -> Option<String> {
        let a = self.assignments.get(&slot)?;
        let src = self
            .source_disk
            .partitions
            .iter()
            .find(|p| p.index == a.source_index)?;
        let src_title = partition_title(src);
        if self.full_disk {
            return Some(src_title);
        }
        let tgt = target.partitions.iter().find(|p| p.index == slot)?;
        Some(format!("← {src_title} on {}", partition_title(tgt)))
    }

    /// Full-disk restore preview: planned offsets/sizes with source partition labels and FS types.
    pub fn planned_target_view(&self, target: &DiskInfo) -> DiskInfo {
        let mut ordered: Vec<&TargetAssignment> = self.assignments.values().collect();
        ordered.sort_by_key(|a| a.offset_bytes);

        let partitions: Vec<PartitionInfo> = ordered
            .iter()
            .enumerate()
            .filter_map(|(slot, a)| {
                let src = self
                    .source_disk
                    .partitions
                    .iter()
                    .find(|p| p.index == a.source_index)?;
                Some(PartitionInfo {
                    index: slot as u32,
                    name: src.name.clone(),
                    type_guid: src.type_guid,
                    offset_bytes: a.offset_bytes,
                    size_bytes: a.size_bytes,
                    fs_kind: src.fs_kind,
                    capture_mode: src.capture_mode,
                    volume_path: None,
                    drive_letter: src.drive_letter,
                    volume_label: src.volume_label.clone(),
                    usage: src.usage.clone(),
                })
            })
            .collect();

        let layout_end = partitions
            .last()
            .map(|p| p.offset_bytes.saturating_add(p.size_bytes))
            .unwrap_or(0);

        DiskInfo {
            index: target.index,
            path: target.path.clone(),
            size_bytes: target.size_bytes.max(layout_end),
            is_gpt: target.is_gpt,
            disk_guid: target.disk_guid,
            disk_signature: target.disk_signature,
            sector_size: target.sector_size,
            model: target.model.clone(),
            partitions,
        }
    }

    pub fn to_restore_plan(&self, target: &DiskInfo) -> RestorePlan {
        if self.full_disk {
            if let Ok(reader) = PhnxReader::open(Path::new(&self.backup_path)) {
                return build_full_disk_plan(
                    &self.backup_path,
                    &reader,
                    target.index,
                    target.size_bytes,
                );
            }
        }

        let tuples: Vec<(u32, u32, u64, u64)> = self
            .assignments
            .iter()
            .map(|(t, a)| (*t, a.source_index, a.offset_bytes, a.size_bytes))
            .collect();
        build_partial_plan(&self.backup_path, target.index, target, &tuples)
    }

    pub fn has_restorable_entries(&self) -> bool {
        !self.assignments.is_empty()
    }

    pub fn target_partition_fs(&self, target: &DiskInfo, target_part: u32) -> FilesystemKind {
        if self.full_disk {
            return self
                .assignments
                .get(&target_part)
                .and_then(|a| {
                    self.source_disk
                        .partitions
                        .iter()
                        .find(|p| p.index == a.source_index)
                })
                .map(|p| p.fs_kind)
                .unwrap_or(FilesystemKind::Unknown);
        }
        target
            .partitions
            .iter()
            .find(|p| p.index == target_part)
            .map(|p| p.fs_kind)
            .unwrap_or(FilesystemKind::Unknown)
    }

    pub fn begin_move(&mut self, target_part: u32, pointer_offset: u64) {
        if let Some(a) = self.assignments.get(&target_part) {
            let grab = pointer_offset.saturating_sub(a.offset_bytes);
            self.drag = Some(LayoutDrag::MoveBody {
                target_part,
                grab_offset_in_part: grab,
            });
        }
    }

    pub fn begin_resize(&mut self, target_part: u32, left_edge: bool) {
        self.drag = Some(if left_edge {
            LayoutDrag::ResizeLeft { target_part }
        } else {
            LayoutDrag::ResizeRight { target_part }
        });
    }

    pub fn update_drag(&mut self, layout_disk: &DiskInfo, pointer_offset: u64) {
        let Some(drag) = self.drag.clone() else {
            return;
        };
        match drag {
            LayoutDrag::MoveBody {
                target_part,
                grab_offset_in_part,
            } => {
                let new_offset = align_up(
                    pointer_offset.saturating_sub(grab_offset_in_part),
                    self.align_bytes,
                );
                let size = self
                    .assignments
                    .get(&target_part)
                    .map(|a| a.size_bytes)
                    .unwrap_or(0);
                if size > 0
                    && new_offset.saturating_add(size) <= layout_disk.size_bytes
                    && !overlaps_assignments(
                        &self.assignments,
                        target_part,
                        new_offset,
                        size,
                        layout_disk.size_bytes,
                    )
                {
                    if let Some(a) = self.assignments.get_mut(&target_part) {
                        a.offset_bytes = new_offset;
                    }
                }
            }
            LayoutDrag::ResizeLeft { target_part } => {
                if let Some(a) = self.assignments.get(&target_part).cloned() {
                    let fs = self.target_partition_fs(layout_disk, target_part);
                    if !partition_allows_resize(fs) {
                        return;
                    }
                    let new_offset = align_up(pointer_offset, self.align_bytes);
                    let end = a.offset_bytes + a.size_bytes;
                    if new_offset < end {
                        let new_size = end - new_offset;
                        if new_size >= a.source_used_bytes {
                            if let Some(slot) = self.assignments.get_mut(&target_part) {
                                slot.offset_bytes = new_offset;
                                slot.size_bytes = new_size;
                            }
                        }
                    }
                }
            }
            LayoutDrag::ResizeRight { target_part } => {
                if let Some(a) = self.assignments.get(&target_part).cloned() {
                    let fs = self.target_partition_fs(layout_disk, target_part);
                    if !partition_allows_resize(fs) {
                        return;
                    }
                    let new_end = align_up(pointer_offset, self.align_bytes);
                    if new_end > a.offset_bytes {
                        let new_size = new_end - a.offset_bytes;
                        let offset = a.offset_bytes;
                        if new_size >= a.source_used_bytes
                            && offset.saturating_add(new_size) <= layout_disk.size_bytes
                            && !overlaps_assignments(
                                &self.assignments,
                                target_part,
                                offset,
                                new_size,
                                layout_disk.size_bytes,
                            )
                        {
                            if let Some(slot) = self.assignments.get_mut(&target_part) {
                                slot.size_bytes = new_size;
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn end_drag(&mut self) {
        self.drag = None;
    }
}

pub fn partition_title(p: &PartitionInfo) -> String {
    match (&p.drive_letter, p.volume_label.as_deref()) {
        (Some(c), Some(label)) if !label.is_empty() => format!("{c}: {label}"),
        (Some(c), _) => format!("{c}:"),
        (None, Some(label)) if !label.is_empty() => format!("{label}"),
        _ => format!("Part {}", p.index),
    }
}

fn max_contiguous_free(target: &DiskInfo, offset: u64, size: u64) -> u64 {
    let end = offset + size;
    let mut free = size;
    for p in &target.partitions {
        if p.offset_bytes >= end {
            break;
        }
        if p.offset_bytes + p.size_bytes <= offset {
            continue;
        }
        // overlapping — not truly free
        if p.offset_bytes < end && p.offset_bytes + p.size_bytes > offset {
            return size;
        }
    }
    let tail = target.size_bytes.saturating_sub(end);
    free = free.saturating_add(tail);
    free
}

fn overlaps_assignments(
    assignments: &HashMap<u32, TargetAssignment>,
    skip: u32,
    offset: u64,
    size: u64,
    disk_size: u64,
) -> bool {
    let end = offset.saturating_add(size);
    if end > disk_size {
        return true;
    }
    assignments.iter().any(|(slot, a)| {
        if *slot == skip {
            return false;
        }
        let a_end = a.offset_bytes.saturating_add(a.size_bytes);
        offset < a_end && end > a.offset_bytes
    })
}

fn overlaps_other(target: &DiskInfo, skip: u32, offset: u64, size: u64) -> bool {
    let end = offset.saturating_add(size);
    target.partitions.iter().any(|p| {
        if p.index == skip {
            return false;
        }
        let p_end = p.offset_bytes.saturating_add(p.size_bytes);
        offset < p_end && end > p.offset_bytes
    })
}

fn fs_kind_for_entry(reader: &PhnxReader, e: &PartitionIndexEntry) -> FilesystemKind {
    if e.fs_kind != FilesystemKind::Unknown {
        return e.fs_kind;
    }
    reader
        .manifest
        .partitions
        .iter()
        .find(|p| p.index == e.index)
        .map(|p| fs_kind_from_string(&p.fs))
        .unwrap_or(FilesystemKind::Unknown)
}

pub fn synthetic_target_view(target: &DiskInfo, layout: &RestoreLayoutState) -> DiskInfo {
    let mut disk = target.clone();
    for part in &mut disk.partitions {
        if let Some(a) = layout.assignments.get(&part.index) {
            part.offset_bytes = a.offset_bytes;
            part.size_bytes = a.size_bytes;
        }
    }
    disk.partitions.sort_by_key(|p| p.offset_bytes);
    disk
}
