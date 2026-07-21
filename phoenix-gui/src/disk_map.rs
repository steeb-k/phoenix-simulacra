//! Shared proportional disk/partition map rendering for Backup and Restore.

use std::collections::HashSet;

use egui;
use egui::{Align2, Color32, Rect, Rounding, Sense, Stroke, Ui, Vec2};

use phoenix_core::disk::{BitlockerState, DiskInfo, FilesystemKind, PartitionInfo};

use crate::fonts;
use crate::theme::Palette;
use crate::util::format_bytes;

pub const CHECKBOX_COLUMN_WIDTH: f32 = 36.0;
pub const CHECKBOX_GAP: f32 = 6.0;
pub const INFO_CARD_WIDTH: f32 = 108.0;
pub const INFO_CARD_GAP: f32 = 10.0;
pub const ROW_HEIGHT: f32 = 92.0;
pub const SEGMENT_GUTTER: f32 = 4.0;
pub const FILL_BAR_HEIGHT: f32 = 5.0;
pub const SEGMENT_ROUNDING: f32 = 6.0;
pub const ROW_VERTICAL_GAP: f32 = 12.0;
pub const MIN_GAP_BYTES: u64 = 4 * 1024 * 1024;
/// Absolute floor for a partition segment: the width used for the very smallest
/// partitions, and a comfortable click/hover target.
pub const MIN_PARTITION_WIDTH: f32 = 24.0;
/// Ceiling for the size-aware minimum (see [`min_partition_width`]).
pub const MAX_PARTITION_MIN_WIDTH: f32 = 110.0;
pub const MIN_UNALLOCATED_WIDTH: f32 = 30.0;

/// A size-aware minimum width so the map keeps a visible hierarchy even among
/// partitions that are all far too small to render proportionally against a
/// multi-hundred-GB neighbour. The floor grows with `log2(size)`, so a 16 GB
/// recovery partition always reserves more width than a 128 MB reserved one,
/// which in turn beats a 4 MB sliver — instead of every small partition
/// collapsing to one identical stub. Above the floor, the normal proportional
/// pass still applies, so large partitions grow past this minimum.
pub fn min_partition_width(size_bytes: u64) -> f32 {
    if size_bytes == 0 {
        return MIN_PARTITION_WIDTH;
    }
    // Anchored at 128 MB (log2 = 27); ~5.5 px of extra floor per doubling.
    let log2 = (size_bytes as f64).log2() as f32;
    let scaled = MIN_PARTITION_WIDTH + 5.5 * (log2 - 27.0);
    scaled.clamp(MIN_PARTITION_WIDTH, MAX_PARTITION_MIN_WIDTH)
}

const PARTITION_SENSE: Sense = Sense {
    click: true,
    drag: false,
    focusable: false,
};

pub fn row_stride() -> f32 {
    ROW_HEIGHT + ROW_VERTICAL_GAP
}

#[derive(Clone, Copy)]
pub enum SegmentKind<'a> {
    Partition(&'a PartitionInfo),
    Unallocated { offset: u64, length: u64 },
}

pub fn build_segments(disk: &DiskInfo) -> Vec<SegmentKind<'_>> {
    let mut sorted: Vec<&PartitionInfo> = disk.partitions.iter().collect();
    sorted.sort_by_key(|p| p.offset_bytes);

    let mut out: Vec<SegmentKind<'_>> = Vec::new();
    let mut cursor = 0u64;
    for p in sorted {
        if p.offset_bytes > cursor && p.offset_bytes - cursor >= MIN_GAP_BYTES {
            out.push(SegmentKind::Unallocated {
                offset: cursor,
                length: p.offset_bytes - cursor,
            });
        }
        out.push(SegmentKind::Partition(p));
        cursor = p.offset_bytes.saturating_add(p.size_bytes);
    }
    if disk.size_bytes > cursor && disk.size_bytes - cursor >= MIN_GAP_BYTES {
        out.push(SegmentKind::Unallocated {
            offset: cursor,
            length: disk.size_bytes - cursor,
        });
    }
    out
}

fn segment_length(seg: &SegmentKind<'_>) -> u64 {
    match seg {
        SegmentKind::Partition(p) => p.size_bytes,
        SegmentKind::Unallocated { length, .. } => *length,
    }
}

pub fn compute_segment_widths(
    segments: &[SegmentKind<'_>],
    usable_width: f32,
    total_units: u64,
) -> Vec<f32> {
    let n = segments.len();
    let mut widths = vec![0.0f32; n];
    if n == 0 || usable_width <= 0.0 || total_units == 0 {
        return widths;
    }
    let mins: Vec<f32> = segments
        .iter()
        .map(|s| match s {
            SegmentKind::Partition(p) => min_partition_width(p.size_bytes),
            SegmentKind::Unallocated { .. } => MIN_UNALLOCATED_WIDTH,
        })
        .collect();

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

pub fn min_disk_row_width(disk: &DiskInfo) -> f32 {
    let segments = build_segments(disk);
    let segment_min_total: f32 = segments
        .iter()
        .map(|s| match s {
            SegmentKind::Partition(p) => min_partition_width(p.size_bytes),
            SegmentKind::Unallocated { .. } => MIN_UNALLOCATED_WIDTH,
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

/// Accent border for a selected card. A stroke centered on the card edge gets
/// its straight runs halved by the square clip while the corner arcs escape it
/// and bulge past the rounded fill, reading as dark corner wedges. Inset by
/// half the stroke width (radius reduced to match, keeping the same corner
/// center) so the stroke's outer edge lands exactly on the fill's silhouette.
fn draw_selection_stroke(painter: &egui::Painter, rect: Rect, palette: &Palette) {
    let w = 2.0;
    painter.rect_stroke(
        rect.shrink(w / 2.0),
        Rounding::same(SEGMENT_ROUNDING - w / 2.0),
        Stroke::new(w, palette.accent),
    );
}

pub fn draw_disk_info_card(
    ui: &mut Ui,
    rect: Rect,
    disk: &DiskInfo,
    palette: &Palette,
    selected: bool,
) {
    let painter = ui.painter_at(rect);
    let bg = if selected {
        blend(palette.content_card_bg, palette.accent, 0.18)
    } else {
        palette.content_card_bg
    };
    painter.rect_filled(rect, Rounding::same(SEGMENT_ROUNDING), bg);
    if selected {
        draw_selection_stroke(&painter, rect, palette);
    }

    let text_x = rect.left() + 14.0;
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
    // Drive type sits above the partition style, and only when it's known — a
    // backup image has no physical device behind it, so that card keeps the
    // single subtitle line it has always had, in its original spot. Two lines
    // start slightly higher so the pair stays centered under the size.
    let drive_type = disk.drive_type.as_deref();
    let first_row = if drive_type.is_some() { 55.0 } else { 58.0 };
    let mut y = rect.top() + first_row;
    for line in drive_type.into_iter().chain([style]) {
        painter.text(
            egui::pos2(text_x, y),
            Align2::LEFT_TOP,
            line,
            fonts::regular(11.0),
            palette.subtle_text,
        );
        y += 17.0;
    }
}

/// Segment-level map renderer: the callback sees every segment — partitions
/// AND unallocated gaps — and returns true when it fully handled the drawing.
/// On false, the default visuals apply (interactive partition segment with
/// tooltip, plain unallocated block).
pub fn draw_partition_map_segments(
    ui: &mut Ui,
    rect: Rect,
    disk: &DiskInfo,
    palette: &Palette,
    mut on_segment: impl FnMut(&mut Ui, &SegmentKind<'_>, Rect) -> bool,
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
        let seg_rect = Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(*w, rect.height()));
        if !on_segment(ui, seg, seg_rect) {
            match seg {
                SegmentKind::Partition(p) => {
                    let id = ui.id().with(("partition", disk.index, p.index));
                    let response = ui.interact(seg_rect, id, PARTITION_SENSE);
                    draw_partition_segment_visual(ui, seg_rect, p, palette, response.hovered());
                    response.on_hover_ui_at_pointer(|ui| partition_tooltip(ui, p));
                }
                SegmentKind::Unallocated { length, .. } => {
                    draw_unallocated_segment(ui, seg_rect, *length, palette);
                }
            }
        }
        x += *w + SEGMENT_GUTTER;
    }
}

pub fn draw_partition_map(
    ui: &mut Ui,
    rect: Rect,
    disk: &DiskInfo,
    palette: &Palette,
    mut on_partition: impl FnMut(&mut Ui, u32, &PartitionInfo, Rect) -> bool,
) {
    let disk_index = disk.index;
    draw_partition_map_segments(ui, rect, disk, palette, |ui, seg, seg_rect| match seg {
        SegmentKind::Partition(p) => on_partition(ui, disk_index, p, seg_rect),
        SegmentKind::Unallocated { .. } => false,
    });
}

pub fn draw_partition_segment_visual(
    ui: &mut Ui,
    rect: Rect,
    p: &PartitionInfo,
    palette: &Palette,
    hovered: bool,
) {
    draw_partition_segment_visual_styled(ui, rect, p, palette, hovered, false, None);
}

pub fn draw_partition_segment_visual_styled(
    ui: &mut Ui,
    rect: Rect,
    p: &PartitionInfo,
    palette: &Palette,
    hovered: bool,
    selected: bool,
    overlay_title: Option<&str>,
) {
    let painter = ui.painter_at(rect);
    let bg = if hovered || selected {
        blend(palette.content_card_bg, palette.accent, 0.18)
    } else {
        palette.content_card_bg
    };
    painter.rect_filled(rect, Rounding::same(SEGMENT_ROUNDING), bg);

    // The fill bar is shorter than the corner radius, so a standalone rounded
    // rect gets its radius clamped to bar_height/2 and pokes past the card's
    // corner curve. Paint the full card shape instead, clipped to the bar
    // strip, so the corner geometry matches the background exactly.
    let track_clip = Rect::from_min_size(rect.left_top(), Vec2::new(rect.width(), FILL_BAR_HEIGHT));
    painter.with_clip_rect(track_clip).rect_filled(
        rect,
        Rounding::same(SEGMENT_ROUNDING),
        with_alpha(palette.subtle_text, 60),
    );

    let (usage_frac, usage_alpha) = match p.usage {
        Some(u) => {
            let total = u.total_bytes.max(1) as f64;
            let frac = (u.used_bytes() as f64 / total) as f32;
            (frac.clamp(0.0, 1.0), 255u8)
        }
        None => (1.0, 140),
    };
    let usage_w = (rect.width() * usage_frac).max(0.0);
    if usage_w > 0.5 {
        let usage_clip = Rect::from_min_size(rect.left_top(), Vec2::new(usage_w, FILL_BAR_HEIGHT));
        painter.with_clip_rect(usage_clip).rect_filled(
            rect,
            Rounding::same(SEGMENT_ROUNDING),
            with_alpha(palette.accent, usage_alpha),
        );
    }

    if selected {
        draw_selection_stroke(&painter, rect, palette);
    }

    // All three lines are always drawn; `painter_at(rect)` clips them to the
    // segment, so on a narrow partition they simply truncate at the edge rather
    // than overflowing into the neighbour. The hover tooltip carries the full,
    // un-clipped details.
    let text_x = rect.left() + 8.0;
    let title_top = rect.top() + FILL_BAR_HEIGHT + 8.0;
    let title = overlay_title
        .map(|s| s.to_string())
        .unwrap_or_else(|| partition_title(p));
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
    painter.text(
        egui::pos2(text_x, title_top + 40.0),
        Align2::LEFT_TOP,
        describe_filesystem(p),
        fonts::regular(11.0),
        palette.subtle_text,
    );
}

/// The short `C: OS` / `*: label` heading shown on a partition segment.
pub fn partition_title(p: &PartitionInfo) -> String {
    match (&p.drive_letter, p.volume_label.as_deref()) {
        (Some(c), Some(label)) if !label.is_empty() => format!("{c}: {label}"),
        (Some(c), _) => format!("{c}:"),
        (None, Some(label)) if !label.is_empty() => format!("*: {label}"),
        (None, _) => "*:".into(),
    }
}

/// Full partition details for a hover tooltip — always available even when the
/// segment is too narrow to show inline text.
pub fn partition_tooltip(ui: &mut Ui, p: &PartitionInfo) {
    ui.strong(partition_title(p));
    ui.label(format!("Size: {}", format_bytes(p.size_bytes)));
    ui.label(describe_filesystem(p));
    if let Some(u) = p.usage {
        ui.label(format!(
            "Used: {} of {}",
            format_bytes(u.used_bytes()),
            format_bytes(u.total_bytes),
        ));
    }
}

pub fn describe_filesystem(p: &PartitionInfo) -> String {
    let known = match p.fs_kind {
        FilesystemKind::Ntfs => Some("NTFS"),
        FilesystemKind::Fat => Some("FAT32"),
        FilesystemKind::Exfat => Some("exFAT"),
        FilesystemKind::Efi => Some("EFI System"),
        FilesystemKind::Msr => Some("Microsoft Reserved"),
        FilesystemKind::Bitlocker => Some("BitLocker"),
        FilesystemKind::Refs => Some("ReFS"),
        FilesystemKind::Unknown => None,
    };
    if let Some(label) = known {
        return match p.bitlocker {
            BitlockerState::Unlocked => format!("{label} (BitLocker, unlocked)"),
            BitlockerState::Locked => "BitLocker (locked)".to_string(),
            BitlockerState::None => label.to_string(),
        };
    }
    if p.volume_path.is_some() && p.usage.is_none() {
        return "Encrypted / locked".into();
    }
    "Unknown".into()
}

pub fn draw_unallocated_segment(ui: &mut Ui, rect: Rect, length: u64, palette: &Palette) {
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

/// Bright multi-stroke halo marking the segment a click or drop would act on.
/// Painted with the un-clipped painter so the outer strokes can breathe past
/// the segment rect (the gutters between segments give them the room).
pub fn draw_segment_glow(ui: &Ui, rect: Rect, color: Color32) {
    let painter = ui.painter();
    painter.rect_stroke(
        rect,
        Rounding::same(SEGMENT_ROUNDING),
        Stroke::new(2.0, color),
    );
    painter.rect_stroke(
        rect.expand(2.0),
        Rounding::same(SEGMENT_ROUNDING + 1.0),
        Stroke::new(2.0, with_alpha(color, 90)),
    );
    painter.rect_stroke(
        rect.expand(4.0),
        Rounding::same(SEGMENT_ROUNDING + 2.0),
        Stroke::new(2.0, with_alpha(color, 40)),
    );
}

pub fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t) as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Piecewise pixel↔byte mapping matching the rendered segment layout.
///
/// Segments are NOT drawn proportionally — small partitions get log-scaled
/// minimum widths and huge unallocated spans absorb the squeeze — so a
/// linear `x / width × disk_size` mapping makes the cursor and a dragged
/// slice move at wildly different speeds on big disks. This mirrors the
/// exact per-segment rects the renderer produced, so a pointer position
/// converts to the byte the user is visually pointing at.
#[derive(Clone, Debug)]
pub struct MapScale {
    /// (byte_start, byte_len, px_start, px_len), in visual order.
    spans: Vec<(u64, u64, f32, f32)>,
    disk_size: u64,
}

pub fn compute_map_scale(rect: Rect, disk: &DiskInfo) -> MapScale {
    let segments = build_segments(disk);
    let total_units: u64 = segments.iter().map(segment_length).sum();
    let total_gutter = SEGMENT_GUTTER * (segments.len().saturating_sub(1) as f32);
    let usable_width = (rect.width() - total_gutter).max(0.0);
    let widths = compute_segment_widths(&segments, usable_width, total_units.max(1));

    let mut spans = Vec::with_capacity(segments.len());
    let mut x = rect.left();
    for (seg, w) in segments.iter().zip(widths.iter()) {
        let (byte_start, byte_len) = match seg {
            SegmentKind::Partition(p) => (p.offset_bytes, p.size_bytes),
            SegmentKind::Unallocated { offset, length } => (*offset, *length),
        };
        spans.push((byte_start, byte_len, x, *w));
        x += *w + SEGMENT_GUTTER;
    }
    MapScale {
        spans,
        disk_size: disk.size_bytes,
    }
}

impl MapScale {
    /// The disk byte visually under pixel `x`. Gutters (and the sub-4MiB
    /// byte gaps they hide) resolve to the next segment's start; outside the
    /// map clamps to the disk ends.
    pub fn x_to_offset(&self, x: f32) -> u64 {
        for &(byte_start, byte_len, px_start, px_len) in &self.spans {
            if x < px_start {
                return byte_start;
            }
            if x < px_start + px_len {
                let frac = ((x - px_start) / px_len.max(0.5)) as f64;
                return byte_start + (frac * byte_len as f64) as u64;
            }
        }
        self.disk_size
    }

    /// Local bytes-per-pixel around `x` — the scale of the segment under the
    /// cursor, for converting pixel thresholds (snap windows) into bytes.
    pub fn bytes_per_px_at(&self, x: f32) -> f64 {
        let mut nearest: Option<(f32, f64)> = None;
        for &(_, byte_len, px_start, px_len) in &self.spans {
            let ratio = byte_len as f64 / px_len.max(0.5) as f64;
            let dist = if x < px_start {
                px_start - x
            } else if x > px_start + px_len {
                x - (px_start + px_len)
            } else {
                return ratio;
            };
            if nearest.map(|(d, _)| dist < d).unwrap_or(true) {
                nearest = Some((dist, ratio));
            }
        }
        nearest.map(|(_, r)| r).unwrap_or(0.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiskCheckState {
    All,
    Some,
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

pub fn draw_disk_checkbox(
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

pub fn draw_backup_disk_row(
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
    draw_partition_map(
        ui,
        map_rect,
        disk,
        palette,
        |ui, disk_index, p, seg_rect| {
            let id = ui.id().with(("partition", disk.index, p.index));
            let response = ui.interact(seg_rect, id, PARTITION_SENSE);
            let selected = selections.contains(&(disk_index, p.index));
            draw_partition_segment_visual_styled(
                ui,
                seg_rect,
                p,
                palette,
                response.hovered(),
                selected,
                None,
            );
            let response = response.on_hover_ui_at_pointer(|ui| partition_tooltip(ui, p));
            if response.clicked() {
                let key = (disk_index, p.index);
                if selected {
                    selections.remove(&key);
                } else {
                    selections.insert(key);
                }
            }
            true
        },
    );
}
