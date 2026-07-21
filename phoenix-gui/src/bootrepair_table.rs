//! The Windows-installations table on the Boot Repair page.
//!
//! Same hand-painted, zebra-striped table body as [`crate::history_table`]
//! (explicit cell rects the layout cursor can't fight), but rows here are
//! click-to-select: the picked installation is the one the action bar's
//! "Repair Boot" targets, marked with the sidebar's accent-bar treatment.
//!
//! ```text
//! VERSION                     DRIVE   DISK   PARTITION   LABEL     BOOT
//! Windows 11 (build 26100)    C:      0      2           OS        GPT · UEFI
//! Windows 11 (build 26100)    E:      3      2           OS        GPT · UEFI
//! ```

use egui;
use egui::{Align2, CursorIcon, Rect, Rounding, Sense, Stroke, Ui, Vec2};

use crate::disk_map::with_alpha;
use crate::fonts;
use crate::theme::Palette;

/// One detected installation, flattened for display.
pub struct InstallRow {
    /// "Windows 11 (build 26100)", or just "Windows" when the version
    /// resource didn't parse.
    pub version: String,
    /// "C:", or empty for an un-lettered volume.
    pub drive: String,
    pub disk: String,
    pub partition: String,
    /// Volume label; empty when the volume has none.
    pub label: String,
    /// "GPT · UEFI" / "MBR · BIOS".
    pub boot: String,
}

const HEADER_H: f32 = 24.0;
const ROW_H: f32 = 42.0;
const PAD_X: f32 = 14.0;
const COL_GAP: f32 = 14.0;
/// Floor for the elastic version column — below this the table stops
/// shrinking and the caller's horizontal `ScrollArea` takes over.
const VERSION_MIN_W: f32 = 200.0;
const DRIVE_W: f32 = 60.0;
const DISK_W: f32 = 50.0;
const PARTITION_W: f32 = 90.0;
const LABEL_W: f32 = 130.0;
const BOOT_W: f32 = 130.0;
const TABLE_ROUNDING: f32 = 8.0;
/// Width of the accent bar marking the selected row (mirrors the sidebar's
/// selected-page marker).
const SELECT_BAR_W: f32 = 3.0;

/// Narrowest the table renders at; callers size it to `max(viewport,
/// min_width())`, the same discipline as the history and mounts tables.
pub fn min_width() -> f32 {
    PAD_X * 2.0 + VERSION_MIN_W + DRIVE_W + DISK_W + PARTITION_W + LABEL_W + BOOT_W + COL_GAP * 5.0
}

/// Column x-positions for a row (or the header) of the given rect: version is
/// elastic, everything after it is pinned rightward.
struct Cols {
    version_x: f32,
    version_w: f32,
    drive_x: f32,
    disk_x: f32,
    partition_x: f32,
    label_x: f32,
    boot_x: f32,
}

fn columns(rect: Rect) -> Cols {
    let boot_x = rect.right() - PAD_X - BOOT_W;
    let label_x = boot_x - COL_GAP - LABEL_W;
    let partition_x = label_x - COL_GAP - PARTITION_W;
    let disk_x = partition_x - COL_GAP - DISK_W;
    let drive_x = disk_x - COL_GAP - DRIVE_W;
    let version_x = rect.left() + PAD_X;
    Cols {
        version_x,
        version_w: (drive_x - COL_GAP - version_x).max(0.0),
        drive_x,
        disk_x,
        partition_x,
        label_x,
        boot_x,
    }
}

/// Render the table at exactly `width`. Returns the row index clicked this
/// frame, if any (the caller owns the selection state).
pub fn show(
    ui: &mut Ui,
    width: f32,
    rows: &[InstallRow],
    selected: Option<usize>,
    palette: &Palette,
) -> Option<usize> {
    let empty = rows.is_empty();

    let (header, _) = ui.allocate_exact_size(Vec2::new(width, HEADER_H), Sense::hover());
    let cols = columns(header);
    let header_color = with_alpha(palette.subtle_text, if empty { 120 } else { 255 });
    let painter = ui.painter();
    for (x, text) in [
        (cols.version_x, "VERSION"),
        (cols.drive_x, "DRIVE"),
        (cols.disk_x, "DISK"),
        (cols.partition_x, "PARTITION"),
        (cols.label_x, "LABEL"),
        (cols.boot_x, "BOOT"),
    ] {
        painter.text(
            egui::pos2(x, header.center().y),
            Align2::LEFT_CENTER,
            text,
            fonts::bold(11.0),
            header_color,
        );
    }
    ui.add_space(4.0);

    if empty {
        empty_row(ui, width, palette);
        return None;
    }

    let mut clicked = None;
    let mut body: Option<Rect> = None;
    let last = rows.len() - 1;
    for (i, row) in rows.iter().enumerate() {
        let rect = paint_row(
            ui,
            width,
            i,
            last,
            row,
            selected == Some(i),
            palette,
            &mut clicked,
        );
        body = Some(body.map_or(rect, |b: Rect| b.union(rect)));
    }
    // Border last so it rides over the row fills.
    if let Some(body) = body {
        ui.painter().rect_stroke(
            body,
            Rounding::same(TABLE_ROUNDING),
            Stroke::new(1.0, palette.panel_border),
        );
    }
    clicked
}

/// One entry. Returns its rect; pushes a click into `clicked`.
#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &mut Ui,
    width: f32,
    index: usize,
    last: usize,
    row: &InstallRow,
    selected: bool,
    palette: &Palette,
    clicked: &mut Option<usize>,
) -> Rect {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::hover());
    let cols = columns(rect);

    // Zebra striping with only the table's outer corners rounded, exactly as
    // the history table does, so the rows read as one body.
    let rounding = Rounding {
        nw: if index == 0 { TABLE_ROUNDING } else { 0.0 },
        ne: if index == 0 { TABLE_ROUNDING } else { 0.0 },
        sw: if index == last { TABLE_ROUNDING } else { 0.0 },
        se: if index == last { TABLE_ROUNDING } else { 0.0 },
    };
    let stripe = if index.is_multiple_of(2) { 80 } else { 175 };
    ui.painter()
        .rect_filled(rect, rounding, with_alpha(palette.content_card_bg, stripe));

    let response = ui.interact(
        rect,
        ui.id().with(("bootrepair_row", index)),
        Sense::click(),
    );
    if selected {
        ui.painter()
            .rect_filled(rect, rounding, with_alpha(palette.accent, 26));
        // Accent bar on the left edge — the sidebar's selected-page marker,
        // reused so "selected" reads the same everywhere.
        let bar = Rect::from_min_max(
            rect.left_top() + Vec2::new(2.0, 6.0),
            egui::pos2(rect.left() + 2.0 + SELECT_BAR_W, rect.bottom() - 6.0),
        );
        ui.painter()
            .rect_filled(bar, Rounding::same(2.0), palette.accent);
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, rounding, with_alpha(palette.icon_color, 12));
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if response.clicked() {
        *clicked = Some(index);
    }

    // The version cell is the one that can routinely run long — elide it and
    // spell it out on hover.
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &row.version,
        0.0,
        egui::TextFormat {
            font_id: fonts::regular(14.0),
            color: palette.icon_color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: cols.version_w,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = ui.fonts(|f| f.layout_job(job));
    ui.painter().galley(
        egui::pos2(cols.version_x, rect.center().y - galley.size().y * 0.5),
        galley,
        palette.icon_color,
    );

    let painter = ui.painter();
    for (x, text, color) in [
        (cols.drive_x, row.drive.as_str(), palette.icon_color),
        (cols.disk_x, row.disk.as_str(), palette.icon_color),
        (cols.partition_x, row.partition.as_str(), palette.icon_color),
        (cols.label_x, row.label.as_str(), palette.subtle_text),
        (cols.boot_x, row.boot.as_str(), palette.subtle_text),
    ] {
        painter.text(
            egui::pos2(x, rect.center().y),
            Align2::LEFT_CENTER,
            text,
            fonts::regular(13.5),
            color,
        );
    }
    rect
}

/// Placeholder shown when no installation was found: the table's own shape,
/// faded, so the page reads as "empty" rather than "missing" (same contract
/// as the mounts table).
fn empty_row(ui: &mut Ui, width: f32, palette: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::hover());
    ui.painter().rect_filled(
        rect,
        Rounding::same(TABLE_ROUNDING),
        with_alpha(palette.content_card_bg, 110),
    );
    ui.painter().text(
        egui::pos2(rect.left() + PAD_X, rect.center().y),
        Align2::LEFT_CENTER,
        "No Windows installations detected — attach the drive, then Rescan.",
        fonts::regular(14.0),
        with_alpha(palette.subtle_text, 140),
    );
}
