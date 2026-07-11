//! Restore-page target-layout editing state and plan generation.
//!
//! The planned target layout is a list of [`LayoutSlot`]s — an editable model
//! seeded from the live target disk. Slots can be replaced by dropping a
//! source partition onto them, created by dropping into unallocated space,
//! moved/resized by dragging, deleted, or wiped wholesale (blank layout /
//! partition-table style switch). The restore plan is generated directly from
//! the slots, so what the map shows is exactly what the engine writes.

use std::path::Path;

use phoenix_core::container::{PartitionIndexEntry, PhnxReader};
use phoenix_core::disk::{DiskInfo, FilesystemKind, PartitionInfo, PartitionUsage};
use phoenix_core::manifest::{bitlocker_state_from_manifest, fs_kind_from_string};
use phoenix_restore::layout_edit::{move_candidate, resize_left_candidate, resize_right_candidate};
use phoenix_restore::plan::{
    align_up, alignment_bytes, build_full_disk_plan, layout_ceiling, partition_allows_resize,
    RestorePlan, RestorePlanEntry,
};

/// The source partition mapped into a slot.
#[derive(Debug, Clone)]
pub struct SlotSource {
    pub source_index: u32,
    pub used_bytes: u64,
}

/// One partition of the planned target layout.
#[derive(Debug, Clone)]
pub struct LayoutSlot {
    /// Stable UI key — survives reorders, never reused within a layout.
    pub id: u32,
    pub offset_bytes: u64,
    pub size_bytes: u64,
    /// The live target partition this slot descends from; `None` for slots
    /// created by dropping into unallocated space. Kept so a preserved slot
    /// retains its on-disk identity and a replaced slot can be volume-locked
    /// before the engine overwrites it.
    pub existing: Option<PartitionInfo>,
    /// Mapped source partition; `None` = preserved as-is.
    pub source: Option<SlotSource>,
}

#[derive(Debug, Clone)]
pub struct RestoreLayoutState {
    pub backup_path: String,
    pub source_disk: DiskInfo,
    pub index: Vec<PartitionIndexEntry>,
    pub full_disk: bool,
    pub align_bytes: u64,
    /// Active edge/body drag.
    pub drag: Option<LayoutDrag>,
    /// The planned layout, unordered (sort by offset for display/plan).
    pub slots: Vec<LayoutSlot>,
    /// Planned partition-table style; seeded from the live target.
    pub table_gpt: bool,
    /// True once the layout stops deriving from the live table (blank layout
    /// or style switch) — the restore must then re-initialize the disk.
    pub reinit_table: bool,
    /// Slot id the toolbar's delete button acts on.
    pub selected_slot: Option<u32>,
    next_slot_id: u32,
    /// (index, size) of the target the slots were built from, so the layout
    /// re-seeds when the target changes under it.
    target_key: Option<(u32, u64)>,
    target_size: u64,
    target_sector: u32,
}

/// How a dragged source partition would fit if dropped. The available room is
/// the replaced slot itself plus any contiguous unallocated space after it
/// (or the gap being dropped into).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropFit {
    /// Source keeps its own original size.
    Fits(u64),
    /// Source is larger than the available room but its data fits: it shrinks
    /// to exactly fill the room.
    Shrinks(u64),
    /// Even the used data doesn't fit (or the source filesystem can't shrink,
    /// or an MBR layout is already at its four-partition limit): refuse.
    TooSmall,
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

impl LayoutDrag {
    pub fn target_part(&self) -> u32 {
        match self {
            LayoutDrag::MoveBody { target_part, .. }
            | LayoutDrag::ResizeLeft { target_part }
            | LayoutDrag::ResizeRight { target_part } => *target_part,
        }
    }
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
            gpt_attributes: 0,
            offset_bytes: offset,
            size_bytes: e.original_size,
            fs_kind: fs_kind_for_entry(reader, e),
            capture_mode: e.capture_mode,
            bitlocker: bitlocker_state_from_manifest(
                reader
                    .manifest
                    .partitions
                    .iter()
                    .find(|p| p.index == e.index)
                    .and_then(|p| p.bitlocker.as_deref()),
            ),
            unique_guid: reader
                .manifest
                .partitions
                .iter()
                .find(|p| p.index == e.index)
                .and_then(|p| p.unique_guid.as_deref())
                .and_then(phoenix_core::disk::guid_from_string)
                .unwrap_or([0u8; 16]),
            volume_path: None,
            drive_letter: None,
            volume_label: None,
            usage: Some(PartitionUsage {
                total_bytes: e.original_size,
                free_bytes: e.original_size.saturating_sub(e.used_bytes),
            }),
            sector_size: 512,
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
            align_bytes: alignment_bytes(reader),
            drag: None,
            slots: Vec::new(),
            table_gpt: false,
            reinit_table: false,
            selected_slot: None,
            next_slot_id: 0,
            target_key: None,
            target_size: 0,
            target_sector: 512,
        }
    }

    /// Re-seed the slots from the live target whenever the target under the
    /// panel changes (first frame included). No-op while it stays the same, so
    /// user edits survive.
    pub fn ensure_target(&mut self, target: &DiskInfo) {
        let key = (target.index, target.size_bytes);
        if self.target_key != Some(key) {
            self.target_key = Some(key);
            self.rebuild_from_target(target);
        }
    }

    /// Discard every edit and mirror the target disk's current partitions.
    pub fn rebuild_from_target(&mut self, target: &DiskInfo) {
        self.target_size = target.size_bytes;
        self.target_sector = target.sector_size;
        self.table_gpt = target.is_gpt;
        self.reinit_table = false;
        self.selected_slot = None;
        self.drag = None;
        self.full_disk = false;
        self.slots.clear();
        let mut parts: Vec<&PartitionInfo> = target.partitions.iter().collect();
        parts.sort_by_key(|p| p.offset_bytes);
        for p in parts {
            let id = self.fresh_id();
            self.slots.push(LayoutSlot {
                id,
                offset_bytes: p.offset_bytes,
                size_bytes: p.size_bytes,
                existing: Some((*p).clone()),
                source: None,
            });
        }
    }

    fn fresh_id(&mut self) -> u32 {
        let id = self.next_slot_id;
        self.next_slot_id += 1;
        id
    }

    pub fn slot(&self, id: u32) -> Option<&LayoutSlot> {
        self.slots.iter().find(|s| s.id == id)
    }

    fn slot_mut(&mut self, id: u32) -> Option<&mut LayoutSlot> {
        self.slots.iter_mut().find(|s| s.id == id)
    }

    /// The filesystem that will occupy the slot after the restore.
    pub fn slot_fs(&self, slot: &LayoutSlot) -> FilesystemKind {
        if let Some(src) = &slot.source {
            return self
                .source_disk
                .partitions
                .iter()
                .find(|p| p.index == src.source_index)
                .map(|p| p.fs_kind)
                .unwrap_or(FilesystemKind::Unknown);
        }
        slot.existing
            .as_ref()
            .map(|p| p.fs_kind)
            .unwrap_or(FilesystemKind::Unknown)
    }

    pub fn slot_resizable(&self, id: u32) -> bool {
        self.slot(id)
            .map(|s| s.source.is_some() && partition_allows_resize(self.slot_fs(s)))
            .unwrap_or(false)
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

    fn slot_source(&self, source_index: u32) -> SlotSource {
        SlotSource {
            source_index,
            used_bytes: self.source_used_bytes(source_index),
        }
    }

    /// Last usable byte of the planned layout: GPT reserves its backup header
    /// tail; MBR can't address past the 32-bit sector horizon.
    pub fn usable_end(&self) -> u64 {
        if self.table_gpt {
            self.target_size
                .saturating_sub(33 * u64::from(self.target_sector.max(512)))
        } else {
            layout_ceiling(self.target_size, false)
        }
    }

    /// Contiguous unallocated bytes starting at `end`, bounded by the next
    /// slot or the usable end of the disk.
    fn gap_after(&self, end: u64) -> u64 {
        self.slots
            .iter()
            .map(|s| s.offset_bytes)
            .filter(|&o| o >= end)
            .min()
            .unwrap_or(self.usable_end())
            .max(end)
            - end
    }

    fn fit_into(&self, source_index: u32, avail: u64) -> DropFit {
        let original = self.source_original_size(source_index);
        if original == 0 {
            return DropFit::TooSmall;
        }
        if original <= avail {
            return DropFit::Fits(original);
        }
        let can_shrink = self
            .source_disk
            .partitions
            .iter()
            .find(|p| p.index == source_index)
            .map(|p| partition_allows_resize(p.fs_kind))
            .unwrap_or(false);
        if can_shrink && align_up(self.source_used_bytes(source_index), self.align_bytes) <= avail {
            DropFit::Shrinks(avail)
        } else {
            DropFit::TooSmall
        }
    }

    /// How `source_index` would fit if dropped onto slot `slot_id`, replacing
    /// it: same start offset, own size if it fits in the slot plus the free
    /// space behind it, shrunk to that room otherwise.
    pub fn drop_fit(&self, source_index: u32, slot_id: u32) -> DropFit {
        let Some(slot) = self.slot(slot_id) else {
            return DropFit::TooSmall;
        };
        let avail = slot.size_bytes + self.gap_after(slot.offset_bytes + slot.size_bytes);
        self.fit_into(source_index, avail)
    }

    /// Drop `source_index` onto slot `slot_id`. Returns false if refused.
    pub fn apply_drop(&mut self, source_index: u32, slot_id: u32) -> bool {
        let size = match self.drop_fit(source_index, slot_id) {
            DropFit::Fits(s) | DropFit::Shrinks(s) => s,
            DropFit::TooSmall => return false,
        };
        let src = self.slot_source(source_index);
        let Some(slot) = self.slot_mut(slot_id) else {
            return false;
        };
        slot.size_bytes = size;
        slot.source = Some(src);
        true
    }

    /// Aligned start a gap-drop would use inside the unallocated segment at
    /// `gap_offset`.
    fn gap_drop_start(&self, gap_offset: u64) -> u64 {
        align_up(gap_offset.max(self.align_bytes), self.align_bytes)
    }

    /// How `source_index` would fit as a NEW slot inside the unallocated
    /// segment `[gap_offset, gap_offset + gap_len)`.
    pub fn gap_drop_fit(&self, source_index: u32, gap_offset: u64, gap_len: u64) -> DropFit {
        // MBR holds at most four primary partitions (no extended support).
        if !self.table_gpt && self.slots.len() >= 4 {
            return DropFit::TooSmall;
        }
        let start = self.gap_drop_start(gap_offset);
        let end = gap_offset.saturating_add(gap_len).min(self.usable_end());
        if end <= start {
            return DropFit::TooSmall;
        }
        self.fit_into(source_index, end - start)
    }

    /// Drop `source_index` into unallocated space, creating a new slot at the
    /// gap's (aligned) start. Returns false if refused.
    pub fn apply_gap_drop(&mut self, source_index: u32, gap_offset: u64, gap_len: u64) -> bool {
        let size = match self.gap_drop_fit(source_index, gap_offset, gap_len) {
            DropFit::Fits(s) | DropFit::Shrinks(s) => s,
            DropFit::TooSmall => return false,
        };
        let offset_bytes = self.gap_drop_start(gap_offset);
        let source = Some(self.slot_source(source_index));
        let id = self.fresh_id();
        self.slots.push(LayoutSlot {
            id,
            offset_bytes,
            size_bytes: size,
            existing: None,
            source,
        });
        true
    }

    /// Toolbar: remove the selected slot from the planned layout.
    pub fn delete_selected(&mut self) -> bool {
        let Some(id) = self.selected_slot.take() else {
            return false;
        };
        let before = self.slots.len();
        self.slots.retain(|s| s.id != id);
        self.slots.len() != before
    }

    /// Toolbar: wipe the planned layout entirely.
    pub fn blank_layout(&mut self) {
        self.slots.clear();
        self.selected_slot = None;
        self.drag = None;
        self.full_disk = false;
        self.reinit_table = true;
    }

    /// Toolbar: switch the planned partition-table style. Implies a blank
    /// layout — a style switch re-initializes the table, so nothing on the
    /// disk survives it anyway.
    pub fn set_table_style(&mut self, gpt: bool) {
        if self.table_gpt != gpt {
            self.table_gpt = gpt;
            self.blank_layout();
        }
    }

    pub fn apply_full_disk(&mut self, target: &DiskInfo) {
        self.full_disk = true;
        self.selected_slot = None;
        self.drag = None;
        let reader_path = self.backup_path.clone();
        if let Ok(reader) = PhnxReader::open(Path::new(&reader_path)) {
            let plan =
                build_full_disk_plan(&self.backup_path, &reader, target.index, target.size_bytes);
            self.slots.clear();
            self.table_gpt = self.source_disk.is_gpt;
            self.reinit_table = true;
            for entry in plan.entries.iter().filter(|e| e.restore) {
                if let Some(src) = entry.source_partition_index {
                    let source = Some(self.slot_source(src));
                    let id = self.fresh_id();
                    self.slots.push(LayoutSlot {
                        id,
                        offset_bytes: entry.target_offset_bytes,
                        size_bytes: entry.target_size_bytes,
                        existing: None,
                        source,
                    });
                }
            }
        }
    }

    pub fn clear_full_disk(&mut self, target: &DiskInfo) {
        self.rebuild_from_target(target);
    }

    pub fn assignment_label(&self, slot_id: u32) -> Option<String> {
        let slot = self.slot(slot_id)?;
        let src = slot.source.as_ref()?;
        let p = self
            .source_disk
            .partitions
            .iter()
            .find(|p| p.index == src.source_index)?;
        Some(partition_title(p))
    }

    /// The planned layout as a `DiskInfo` for the shared map renderer: mapped
    /// slots take the source partition's identity, preserved slots keep the
    /// live target partition's. Partition `index` is the slot id.
    pub fn target_view(&self, target: &DiskInfo) -> DiskInfo {
        let mut partitions: Vec<PartitionInfo> = self
            .slots
            .iter()
            .filter_map(|s| {
                let mut p = if let Some(src) = &s.source {
                    let mut p = self
                        .source_disk
                        .partitions
                        .iter()
                        .find(|p| p.index == src.source_index)?
                        .clone();
                    p.usage = Some(PartitionUsage {
                        total_bytes: s.size_bytes,
                        free_bytes: s.size_bytes.saturating_sub(src.used_bytes),
                    });
                    p
                } else {
                    s.existing.clone()?
                };
                p.index = s.id;
                p.offset_bytes = s.offset_bytes;
                p.size_bytes = s.size_bytes;
                p.volume_path = None;
                Some(p)
            })
            .collect();
        partitions.sort_by_key(|p| p.offset_bytes);

        let layout_end = partitions
            .last()
            .map(|p| p.offset_bytes.saturating_add(p.size_bytes))
            .unwrap_or(0);

        DiskInfo {
            index: target.index,
            path: target.path.clone(),
            size_bytes: target.size_bytes.max(layout_end),
            is_gpt: self.table_gpt,
            disk_guid: target.disk_guid,
            disk_signature: target.disk_signature,
            sector_size: target.sector_size,
            model: target.model.clone(),
            partitions,
        }
    }

    /// Build the restore plan straight from the slots — the map IS the plan.
    /// New slots get target indexes above every live partition's so they can
    /// never be mistaken for one (the engine looks entries up by index for
    /// volume locking and preserved-identity).
    pub fn to_restore_plan(&self, target: &DiskInfo) -> RestorePlan {
        let mut ordered: Vec<&LayoutSlot> = self.slots.iter().collect();
        ordered.sort_by_key(|s| s.offset_bytes);

        let mut next_index = target
            .partitions
            .iter()
            .map(|p| p.index + 1)
            .max()
            .unwrap_or(0);
        let entries: Vec<RestorePlanEntry> = ordered
            .iter()
            .map(|s| {
                let target_partition_index = match &s.existing {
                    Some(e) => e.index,
                    None => {
                        let i = next_index;
                        next_index += 1;
                        i
                    }
                };
                RestorePlanEntry {
                    source_partition_index: s.source.as_ref().map(|src| src.source_index),
                    target_partition_index,
                    restore: s.source.is_some(),
                    target_offset_bytes: s.offset_bytes,
                    target_size_bytes: s.size_bytes,
                }
            })
            .collect();

        RestorePlan {
            backup_path: self.backup_path.clone(),
            target_disk_index: target.index,
            entries,
            full_disk: self.full_disk,
            reinit_style: (!self.full_disk && self.reinit_table)
                .then(|| if self.table_gpt { "gpt" } else { "mbr" }.to_string()),
        }
    }

    pub fn has_restorable_entries(&self) -> bool {
        self.slots.iter().any(|s| s.source.is_some())
    }

    pub fn begin_move(&mut self, slot_id: u32, pointer_offset: u64) {
        if let Some(s) = self.slot(slot_id) {
            let grab = pointer_offset.saturating_sub(s.offset_bytes);
            self.drag = Some(LayoutDrag::MoveBody {
                target_part: slot_id,
                grab_offset_in_part: grab,
            });
        }
    }

    pub fn begin_resize(&mut self, slot_id: u32, left_edge: bool) {
        self.drag = Some(if left_edge {
            LayoutDrag::ResizeLeft {
                target_part: slot_id,
            }
        } else {
            LayoutDrag::ResizeRight {
                target_part: slot_id,
            }
        });
    }

    /// Advance the active move/resize drag. `pointer_offset` is the pointer's
    /// byte position on the disk; `snap_bytes` is the resize snap window.
    pub fn update_drag(&mut self, pointer_offset: u64, snap_bytes: u64) {
        let Some(drag) = self.drag.clone() else {
            return;
        };
        let id = drag.target_part();
        let Some(slot) = self.slot(id).cloned() else {
            return;
        };

        // Every other slot blocks us, and so does the table region at the
        // front of the disk (capped at the lowest slot start so legacy
        // sub-MiB layouts don't wedge the math).
        let front_reserve = self
            .slots
            .iter()
            .map(|s| s.offset_bytes)
            .min()
            .unwrap_or(self.align_bytes)
            .min(self.align_bytes);
        let mut obstacles: Vec<(u64, u64)> = self
            .slots
            .iter()
            .filter(|s| s.id != id)
            .map(|s| (s.offset_bytes, s.offset_bytes + s.size_bytes))
            .collect();
        if front_reserve > 0 {
            obstacles.push((0, front_reserve));
        }
        obstacles.sort_unstable();
        let disk_end = self.usable_end();
        let min_size = slot
            .source
            .as_ref()
            .map(|s| align_up(s.used_bytes, self.align_bytes))
            .unwrap_or(0)
            .max(self.align_bytes);

        match drag {
            LayoutDrag::MoveBody {
                grab_offset_in_part,
                ..
            } => {
                let desired = pointer_offset.saturating_sub(grab_offset_in_part);
                if let Some(new_offset) = move_candidate(
                    &obstacles,
                    disk_end,
                    slot.offset_bytes,
                    slot.size_bytes,
                    pointer_offset,
                    desired,
                    self.align_bytes,
                ) {
                    if let Some(s) = self.slot_mut(id) {
                        s.offset_bytes = new_offset;
                    }
                }
            }
            LayoutDrag::ResizeLeft { .. } => {
                if !self.slot_resizable(id) {
                    return;
                }
                let end = slot.offset_bytes + slot.size_bytes;
                if let Some(new_offset) = resize_left_candidate(
                    &obstacles,
                    slot.offset_bytes,
                    end,
                    min_size,
                    pointer_offset,
                    snap_bytes,
                    self.align_bytes,
                ) {
                    if let Some(s) = self.slot_mut(id) {
                        s.offset_bytes = new_offset;
                        s.size_bytes = end - new_offset;
                    }
                }
            }
            LayoutDrag::ResizeRight { .. } => {
                if !self.slot_resizable(id) {
                    return;
                }
                if let Some(new_end) = resize_right_candidate(
                    &obstacles,
                    disk_end,
                    slot.offset_bytes,
                    min_size,
                    pointer_offset,
                    snap_bytes,
                    self.align_bytes,
                ) {
                    if let Some(s) = self.slot_mut(id) {
                        s.size_bytes = new_end - slot.offset_bytes;
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
        (None, Some(label)) if !label.is_empty() => label.to_string(),
        _ => format!("Part {}", p.index),
    }
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
