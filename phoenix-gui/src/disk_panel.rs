//! Disk + partition selection panel for the Backup page.
//!
//! Each disk renders as a horizontal row: a small info card on the left
//! (icon, "Disk N", size, GPT/MBR) followed by a partition map whose
//! segments are sized proportionally to each partition's `size_bytes`.
//! Unallocated regions between partitions are drawn as gray segments.
//!
//! Selection lives in the caller-provided `HashSet<(disk_index,
//! partition_index)>`. Real partitions toggle on click. Unallocated regions
//! are informational only (the capture engine doesn't currently support
//! backing up arbitrary disk extents).

use std::collections::HashSet;

use eframe::egui;
use egui::{Align2, Color32, Rect, Rounding, Sense, Stroke, Ui, Vec2};

use phoenix_core::disk::{DiskInfo, FilesystemKind, PartitionInfo};

use crate::fonts;
use crate::theme::Palette;
use crate::util::format_bytes;

const CHECKBOX_COLUMN_WIDTH: f32 = 36.0;
const CHECKBOX_GAP: f32 = 6.0;
const INFO_CARD_WIDTH: f32 = 150.0;
const INFO_CARD_GAP: f32 = 10.0;
const ROW_HEIGHT: f32 = 92.0;
const SEGMENT_GUTTER: f32 = 4.0;
const FILL_BAR_HEIGHT: f32 = 5.0;
const SEGMENT_ROUNDING: f32 = 6.0;
const ROW_VERTICAL_GAP: f32 = 12.0;

/// Vertical space one drive row occupies including its trailing gap. Used by
/// the Backup page to cap the drive list height to ~3.5 rows so a 4th drive
/// peeks in to hint at scrollability.
pub fn row_stride() -> f32 {
    ROW_HEIGHT + ROW_VERTICAL_GAP
}
/// Anything below this is just metadata padding (e.g. the leading 1 MiB GPT
/// header gap or alignment tail) and we hide it from the partition map.
const MIN_GAP_BYTES: u64 = 4 * 1024 * 1024;
/// Minimum on-screen width for a partition segment so the drive letter,
/// size, and filesystem labels remain legible even for tiny partitions
/// (e.g. 16 MiB MSR or 200 MiB EFI alongside a multi-TB data partition).
const MIN_PARTITION_WIDTH: f32 = 95.0;
const MIN_UNALLOCATED_WIDTH: f32 = 70.0;

/// Sense used for each partition segment inside the partition map.
/// `Sense::click()` would also set `focusable: true`, putting every
/// partition cell into the global Tab cycle, which adds many redundant
/// stops between the top-of-page controls. Per-partition selection is a
/// mouse-driven affordance; keyboard users toggle whole disks via the
/// disk-level tri-state checkbox below, which is still tab-focusable.
const PARTITION_SENSE: Sense = Sense {
    click: true,
    drag: false,
    focusable: false,
};

enum Segment<'a> {
    Partition(&'a PartitionInfo),
    Unallocated { length: u64 },
}

/// Render every disk as a row of `[info card | partition map]`. Toggles
/// `(disk.index, partition.index)` entries in `selections` when the user
/// clicks a partition segment.
///
/// `viewport_width` is the visible width available outside any horizontal
/// scroll area. Each row is sized to `max(viewport_width, natural_min_width)`
/// so when the window is too narrow to render every partition at a readable
/// size, the row simply overflows and the surrounding ScrollArea exposes a
/// horizontal scrollbar — partitions never get squished below their minimum
/// label width.
pub fn show(
    ui: &mut Ui,
    disks: &[DiskInfo],
    selections: &mut HashSet<(u32, u32)>,
    palette: &Palette,
    viewport_width: f32,
) {
    // All rows share the same width so the info cards line up horizontally
    // even when one disk has many partitions and another has few.
    let natural_min = disks
        .iter()
        .map(min_disk_row_width)
        .fold(0.0_f32, f32::max);
    let row_width = viewport_width.max(natural_min);

    for disk in disks {
        draw_disk_row(ui, row_width, disk, selections, palette);
        ui.add_space(ROW_VERTICAL_GAP);
    }
}

/// Smallest width a single disk row can be drawn at without shrinking any
/// partition segment below its readable minimum.
fn min_disk_row_width(disk: &DiskInfo) -> f32 {
    let segments = build_segments(disk);
    let segment_min_total: f32 = segments
        .iter()
        .map(|s| match s {
            Segment::Partition(_) => MIN_PARTITION_WIDTH,
            Segment::Unallocated { .. } => MIN_UNALLOCATED_WIDTH,
        })
        .sum();
    let gutter_total = SEGMENT_GUTTER * (segments.len().saturating_sub(1) as f32);
    CHECKBOX_COLUMN_WIDTH
        + CHECKBOX_GAP
        + INFO_CARD_WIDTH
        + INFO_CARD_GAP
        + segment_min_total
        + gutter_total
}

fn draw_disk_row(
    ui: &mut Ui,
    row_width: f32,
    disk: &DiskInfo,
    selections: &mut HashSet<(u32, u32)>,
    palette: &Palette,
) {
    let row_size = Vec2::new(row_width, ROW_HEIGHT);
    let (row_rect, _) = ui.allocate_exact_size(row_size, Sense::hover());

    let checkbox_rect = Rect::from_min_size(
        row_rect.left_top(),
        Vec2::new(CHECKBOX_COLUMN_WIDTH, ROW_HEIGHT),
    );
    draw_disk_checkbox(ui, checkbox_rect, disk, selections, palette);

    let info_rect = Rect::from_min_size(
        egui::pos2(checkbox_rect.right() + CHECKBOX_GAP, row_rect.top()),
        Vec2::new(INFO_CARD_WIDTH, ROW_HEIGHT),
    );
    let fully_selected = disk_check_state(disk, selections) == DiskCheckState::All;
    draw_disk_info_card(ui, info_rect, disk, palette, fully_selected);

    let map_left = info_rect.right() + INFO_CARD_GAP;
    let map_rect = Rect::from_min_max(
        egui::pos2(map_left, row_rect.top()),
        row_rect.right_bottom(),
    );
    draw_partition_map(ui, map_rect, disk, selections, palette);
}

/// Tri-state state for the disk-level checkbox.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DiskCheckState {
    /// Every partition on the disk is in `selections`.
    All,
    /// At least one but not all partitions are selected.
    Some,
    /// No partitions on this disk are selected.
    None,
}

fn disk_check_state(disk: &DiskInfo, selections: &HashSet<(u32, u32)>) -> DiskCheckState {
    if disk.partitions.is_empty() {
        return DiskCheckState::None;
    }
    let total = disk.partitions.len();
    let selected = disk
        .partitions
        .iter()
        .filter(|p| selections.contains(&(disk.index, p.index)))
        .count();
    if selected == 0 {
        DiskCheckState::None
    } else if selected == total {
        DiskCheckState::All
    } else {
        DiskCheckState::Some
    }
}

/// Render an interactive tri-state checkbox spanning the height of the disk
/// row. Click toggles between selecting every partition on this disk and
/// clearing them; the indeterminate state collapses to "select all" on click.
fn draw_disk_checkbox(
    ui: &mut Ui,
    rect: Rect,
    disk: &DiskInfo,
    selections: &mut HashSet<(u32, u32)>,
    palette: &Palette,
) {
    let state = disk_check_state(disk, selections);
    let id = ui.id().with(("disk_check", disk.index));
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter_at(rect);

    let box_size = 22.0;
    let box_rect = Rect::from_center_size(rect.center(), Vec2::splat(box_size));

    match state {
        DiskCheckState::None => {
            let stroke_color = if response.hovered() {
                palette.accent
            } else {
                palette.subtle_text
            };
            painter.rect_stroke(
                box_rect,
                Rounding::same(4.0),
                Stroke::new(1.5, stroke_color),
            );
        }
        DiskCheckState::All => {
            painter.rect_filled(box_rect, Rounding::same(4.0), palette.accent);
            painter.text(
                box_rect.center(),
                Align2::CENTER_CENTER,
                egui_phosphor::regular::CHECK,
                fonts::icon(16.0),
                Color32::WHITE,
            );
        }
        DiskCheckState::Some => {
            painter.rect_filled(box_rect, Rounding::same(4.0), palette.accent);
            painter.text(
                box_rect.center(),
                Align2::CENTER_CENTER,
                egui_phosphor::regular::MINUS,
                fonts::icon(16.0),
                Color32::WHITE,
            );
        }
    }

    // Custom-drawn checkbox: egui's standard `widgets.active.bg_stroke`
    // focus indicator doesn't reach us since we paint everything by hand.
    // Draw an explicit focus ring around the box itself (not the wider
    // hit-test column) so the keyboard-focused disk is unambiguous.
    if response.has_focus() {
        painter.rect_stroke(
            box_rect.expand(3.0),
            Rounding::same(6.0),
            Stroke::new(2.0, palette.icon_color),
        );
    }

    if response.clicked() {
        match state {
            DiskCheckState::All => {
                for p in &disk.partitions {
                    selections.remove(&(disk.index, p.index));
                }
            }
            DiskCheckState::None | DiskCheckState::Some => {
                for p in &disk.partitions {
                    selections.insert((disk.index, p.index));
                }
            }
        }
    }
}

fn draw_disk_info_card(
    ui: &mut Ui,
    rect: Rect,
    disk: &DiskInfo,
    palette: &Palette,
    selected: bool,
) {
    let painter = ui.painter_at(rect);
    let bg = if selected {
        blend(palette.sidebar_hover_bg, palette.accent, 0.18)
    } else {
        palette.sidebar_hover_bg
    };
    painter.rect_filled(rect, Rounding::same(SEGMENT_ROUNDING), bg);
    if selected {
        painter.rect_stroke(
            rect,
            Rounding::same(SEGMENT_ROUNDING),
            Stroke::new(2.0, palette.accent),
        );
    }

    let icon_pos = egui::pos2(rect.left() + 28.0, rect.center().y);
    painter.text(
        icon_pos,
        Align2::CENTER_CENTER,
        egui_phosphor::regular::HARD_DRIVES,
        fonts::icon(30.0),
        palette.icon_color,
    );

    let text_x = rect.left() + 56.0;
    painter.text(
        egui::pos2(text_x, rect.top() + 18.0),
        Align2::LEFT_TOP,
        format!("Disk {}", disk.index),
        fonts::bold(14.0),
        palette.icon_color,
    );
    painter.text(
        egui::pos2(text_x, rect.top() + 38.0),
        Align2::LEFT_TOP,
        format_bytes(disk.size_bytes),
        fonts::regular(12.0),
        palette.icon_color,
    );
    let style = if disk.is_gpt {
        "Basic GPT"
    } else {
        "Basic MBR"
    };
    painter.text(
        egui::pos2(text_x, rect.top() + 58.0),
        Align2::LEFT_TOP,
        style,
        fonts::regular(11.0),
        palette.subtle_text,
    );
}

fn draw_partition_map(
    ui: &mut Ui,
    rect: Rect,
    disk: &DiskInfo,
    selections: &mut HashSet<(u32, u32)>,
    palette: &Palette,
) {
    if rect.width() <= 0.0 {
        return;
    }
    let segments = build_segments(disk);
    if segments.is_empty() {
        return;
    }
    let total_units: u64 = segments.iter().map(segment_length).sum();
    if total_units == 0 {
        return;
    }

    let total_gutter = SEGMENT_GUTTER * (segments.len().saturating_sub(1) as f32);
    let usable_width = (rect.width() - total_gutter).max(0.0);
    let widths = compute_segment_widths(&segments, usable_width, total_units);

    let mut x = rect.left();
    for (seg, w) in segments.iter().zip(widths.iter()) {
        let seg_rect =
            Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(*w, rect.height()));
        match seg {
            Segment::Partition(p) => {
                draw_partition_segment(ui, seg_rect, disk.index, p, selections, palette);
            }
            Segment::Unallocated { length } => {
                draw_unallocated_segment(ui, seg_rect, *length, palette);
            }
        }
        x += *w + SEGMENT_GUTTER;
    }
}

/// Distribute `usable_width` across `segments` proportional to each segment's
/// byte length, but pin any segment whose proportional share would fall below
/// its minimum readable width. The remaining width is then redistributed
/// proportionally across the un-pinned segments. We iterate to a fixpoint so
/// shrinking the divisible pool never pushes another segment below its min.
fn compute_segment_widths(segments: &[Segment<'_>], usable_width: f32, total_units: u64) -> Vec<f32> {
    let n = segments.len();
    let mut widths = vec![0.0f32; n];
    if n == 0 || usable_width <= 0.0 || total_units == 0 {
        return widths;
    }
    let mins: Vec<f32> = segments
        .iter()
        .map(|s| match s {
            Segment::Partition(_) => MIN_PARTITION_WIDTH,
            Segment::Unallocated { .. } => MIN_UNALLOCATED_WIDTH,
        })
        .collect();

    // If we don't even have room for the minimums, give everyone its minimum
    // and let the row overflow horizontally rather than collapsing labels.
    let total_min: f32 = mins.iter().sum();
    if usable_width <= total_min {
        return mins;
    }

    let mut pinned = vec![false; n];
    let mut remaining_width = usable_width as f64;
    let mut remaining_total = total_units as f64;

    loop {
        let mut pinned_this_round = false;
        for i in 0..n {
            if pinned[i] {
                continue;
            }
            let length = segment_length(&segments[i]) as f64;
            let frac = if remaining_total > 0.0 {
                length / remaining_total
            } else {
                0.0
            };
            let proportional = (remaining_width * frac) as f32;
            if proportional < mins[i] {
                widths[i] = mins[i];
                pinned[i] = true;
                remaining_width -= mins[i] as f64;
                remaining_total -= length;
                pinned_this_round = true;
            }
        }
        if !pinned_this_round {
            break;
        }
    }

    // Spread the remaining width across un-pinned segments proportionally.
    if remaining_total > 0.0 && remaining_width > 0.0 {
        for i in 0..n {
            if pinned[i] {
                continue;
            }
            let length = segment_length(&segments[i]) as f64;
            let frac = length / remaining_total;
            widths[i] = (remaining_width * frac) as f32;
        }
    }

    widths
}

fn segment_length(seg: &Segment) -> u64 {
    match seg {
        Segment::Partition(p) => p.size_bytes,
        Segment::Unallocated { length } => *length,
    }
}

/// Build the visual segment list: every partition (sorted by offset) plus
/// any large gap between them. Leading and trailing gaps relative to
/// `disk.size_bytes` are included too, but only when they exceed
/// `MIN_GAP_BYTES` (so the standard ~1 MiB GPT header gap doesn't show up).
fn build_segments(disk: &DiskInfo) -> Vec<Segment<'_>> {
    let mut sorted: Vec<&PartitionInfo> = disk.partitions.iter().collect();
    sorted.sort_by_key(|p| p.offset_bytes);

    let mut out: Vec<Segment<'_>> = Vec::new();
    let mut cursor = 0u64;
    for p in sorted {
        if p.offset_bytes > cursor && p.offset_bytes - cursor >= MIN_GAP_BYTES {
            out.push(Segment::Unallocated {
                length: p.offset_bytes - cursor,
            });
        }
        out.push(Segment::Partition(p));
        cursor = p.offset_bytes.saturating_add(p.size_bytes);
    }
    if disk.size_bytes > cursor && disk.size_bytes - cursor >= MIN_GAP_BYTES {
        out.push(Segment::Unallocated {
            length: disk.size_bytes - cursor,
        });
    }
    out
}

fn draw_partition_segment(
    ui: &mut Ui,
    rect: Rect,
    disk_index: u32,
    p: &PartitionInfo,
    selections: &mut HashSet<(u32, u32)>,
    palette: &Palette,
) {
    let id = ui.id().with(("partition", disk_index, p.index));
    let response = ui.interact(rect, id, PARTITION_SENSE);
    let selected = selections.contains(&(disk_index, p.index));

    let painter = ui.painter_at(rect);
    let bg = if response.hovered() || selected {
        blend(palette.sidebar_hover_bg, palette.accent, 0.18)
    } else {
        palette.sidebar_hover_bg
    };
    painter.rect_filled(rect, Rounding::same(SEGMENT_ROUNDING), bg);

    // Track for the used/free fill bar.
    let track_rect = Rect::from_min_size(rect.left_top(), Vec2::new(rect.width(), FILL_BAR_HEIGHT));
    painter.rect_filled(
        track_rect,
        Rounding {
            nw: SEGMENT_ROUNDING,
            ne: SEGMENT_ROUNDING,
            sw: 0.0,
            se: 0.0,
        },
        with_alpha(palette.subtle_text, 60),
    );

    let (usage_frac, usage_alpha) = match p.usage {
        Some(u) => {
            let total = u.total_bytes.max(1) as f64;
            let frac = (u.used_bytes() as f64 / total) as f32;
            (frac.clamp(0.0, 1.0), 255u8)
        }
        None => (1.0, 140), // unknown -> flat dimmed fill
    };
    let usage_w = (rect.width() * usage_frac).max(0.0);
    if usage_w > 0.5 {
        let usage_rect =
            Rect::from_min_size(rect.left_top(), Vec2::new(usage_w, FILL_BAR_HEIGHT));
        painter.rect_filled(
            usage_rect,
            Rounding {
                nw: SEGMENT_ROUNDING,
                ne: if (usage_w - rect.width()).abs() < 0.5 {
                    SEGMENT_ROUNDING
                } else {
                    0.0
                },
                sw: 0.0,
                se: 0.0,
            },
            with_alpha(palette.accent, usage_alpha),
        );
    }

    if selected {
        painter.rect_stroke(
            rect,
            Rounding::same(SEGMENT_ROUNDING),
            Stroke::new(2.0, palette.accent),
        );
    }

    let text_x = rect.left() + 10.0;
    let title_top = rect.top() + FILL_BAR_HEIGHT + 8.0;
    let title = match (&p.drive_letter, p.volume_label.as_deref()) {
        (Some(c), Some(label)) if !label.is_empty() => format!("{c}: {label}"),
        (Some(c), _) => format!("{c}:"),
        (None, Some(label)) if !label.is_empty() => format!("*: {label}"),
        (None, _) => "*:".into(),
    };
    painter.text(
        egui::pos2(text_x, title_top),
        Align2::LEFT_TOP,
        &title,
        fonts::bold(13.0),
        palette.icon_color,
    );
    painter.text(
        egui::pos2(text_x, title_top + 20.0),
        Align2::LEFT_TOP,
        format_bytes(p.size_bytes),
        fonts::regular(12.0),
        palette.icon_color,
    );
    let fs_text = describe_filesystem(p);
    painter.text(
        egui::pos2(text_x, title_top + 40.0),
        Align2::LEFT_TOP,
        &fs_text,
        fonts::regular(11.0),
        palette.subtle_text,
    );

    if response.clicked() {
        if selected {
            selections.remove(&(disk_index, p.index));
        } else {
            selections.insert((disk_index, p.index));
        }
    }
}

/// Pick a short filesystem label, falling back to "Encrypted / locked" for
/// volumes whose disk-free-space query failed despite having a mount point
/// (BitLocker-locked, raw, or otherwise unreadable).
fn describe_filesystem(p: &PartitionInfo) -> String {
    let known = match p.fs_kind {
        FilesystemKind::Ntfs => Some("NTFS"),
        FilesystemKind::Fat => Some("FAT32"),
        FilesystemKind::Exfat => Some("exFAT"),
        FilesystemKind::Efi => Some("EFI System"),
        FilesystemKind::Msr => Some("Microsoft Reserved"),
        FilesystemKind::Bitlocker => Some("BitLocker"),
        FilesystemKind::Unknown => None,
    };
    if let Some(label) = known {
        return label.to_string();
    }
    if p.volume_path.is_some() && p.usage.is_none() {
        return "Encrypted / locked".into();
    }
    "Unknown".into()
}

fn draw_unallocated_segment(ui: &mut Ui, rect: Rect, length: u64, palette: &Palette) {
    let painter = ui.painter_at(rect);
    let bg = if palette.light_mode {
        Color32::from_rgb(0xE2, 0xE2, 0xE2)
    } else {
        Color32::from_rgb(0x3A, 0x3A, 0x3A)
    };
    painter.rect_filled(rect, Rounding::same(SEGMENT_ROUNDING), bg);

    let text_x = rect.left() + 10.0;
    painter.text(
        egui::pos2(text_x, rect.top() + 14.0),
        Align2::LEFT_TOP,
        "Unallocated",
        fonts::bold(12.0),
        palette.subtle_text,
    );
    painter.text(
        egui::pos2(text_x, rect.top() + 32.0),
        Align2::LEFT_TOP,
        format_bytes(length),
        fonts::regular(11.0),
        palette.subtle_text,
    );
}

fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t) as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}
