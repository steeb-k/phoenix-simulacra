//! The job-history table on the History page.
//!
//! A conventional five-column table — status, action, time, details, and a
//! remove button — with zebra-striped rows inside a rounded, bordered body:
//!
//! ```text
//! STATUS      ACTION    TIME                 DETAILS
//! ✓ OK        Backup    2026-07-12 14:32:07  Imaged Samsung SSD 990 PRO (Disk 1) → D:\work.phnx (61.2 GB)   ✕
//! ✓ OK        Verify    2026-07-12 14:41:55  Fully verified D:\work.phnx                                     ✕
//! ✕ Failed    Restore   2026-07-11 09:02:14  Restored D:\work.phnx → WDC WD10EZEX (Disk 2) — device not ready ✕
//! ```
//!
//! Hand-painted for the same reason [`crate::mount_table`] is: every cell sits
//! at an explicit rect inside a row the table owns outright, which an
//! `egui::Grid` can't do without fighting the layout cursor. Rows carry no
//! record type — the page flattens [`phoenix_core::appdata::JobRecord`]s into
//! [`HistoryRow`]s and maps a click back to the record it came from.

use eframe::egui;
use egui::{Align2, Color32, CursorIcon, Rect, Rounding, Sense, Stroke, Ui, Vec2};

use crate::disk_map::with_alpha;
use crate::theme::{draw_focus_outline, Palette};
use crate::{fonts, icon_label};

/// How a recorded job ended, as the Status column paints it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Ok,
    Failed,
    Cancelled,
}

impl RowStatus {
    fn label(self) -> &'static str {
        match self {
            RowStatus::Ok => "OK",
            RowStatus::Failed => "Failed",
            RowStatus::Cancelled => "Cancelled",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            RowStatus::Ok => egui_phosphor::fill::CHECK_CIRCLE,
            RowStatus::Failed => egui_phosphor::fill::X_CIRCLE,
            RowStatus::Cancelled => egui_phosphor::fill::PROHIBIT,
        }
    }

    fn color(self, palette: &Palette) -> Color32 {
        match self {
            RowStatus::Ok => palette.success,
            RowStatus::Failed => palette.danger,
            RowStatus::Cancelled => palette.warning,
        }
    }
}

/// One history entry, flattened for display.
pub struct HistoryRow {
    pub status: RowStatus,
    /// "Backup", "Verify", "Restore", "Clone".
    pub action: String,
    /// Absolute local timestamp — the whole point of the column is that
    /// "3 hours ago" is useless a week later.
    pub time: String,
    /// What the job did, in full: "Imaged <disk> → <file> (<size>)".
    pub details: String,
    /// Failure text, appended to the details in red. `None` unless the job
    /// failed.
    pub error: Option<String>,
}

/// What the user clicked this frame.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HistoryAction {
    None,
    /// Remove the entry at this index in the table's slice.
    Remove(usize),
}

const HEADER_H: f32 = 24.0;
const ROW_H: f32 = 42.0;
const PAD_X: f32 = 14.0;
const COL_GAP: f32 = 14.0;
const STATUS_W: f32 = 100.0;
const ACTION_W: f32 = 80.0;
/// Fits "2026-07-12 14:32:07" at 13.5pt with room to spare.
const TIME_W: f32 = 150.0;
const REMOVE_W: f32 = 26.0;
/// Floor for the elastic details column — below this the table stops shrinking
/// and the caller's horizontal `ScrollArea` takes over.
const DETAILS_MIN_W: f32 = 240.0;
const TABLE_ROUNDING: f32 = 8.0;

/// Narrowest the table renders at. Callers size it to `max(viewport,
/// min_width())`, the same discipline the disk maps and the mounts table use.
pub fn min_width() -> f32 {
    PAD_X * 2.0 + STATUS_W + ACTION_W + TIME_W + DETAILS_MIN_W + REMOVE_W + COL_GAP * 4.0
}

/// Column geometry for a row (or the header) of the given rect: the fixed
/// columns are pinned left, the remove button right, and details takes the slack.
struct Cols {
    status_x: f32,
    action_x: f32,
    time_x: f32,
    details_x: f32,
    details_w: f32,
    remove_x: f32,
}

fn columns(rect: Rect) -> Cols {
    let status_x = rect.left() + PAD_X;
    let action_x = status_x + STATUS_W + COL_GAP;
    let time_x = action_x + ACTION_W + COL_GAP;
    let details_x = time_x + TIME_W + COL_GAP;
    let remove_x = rect.right() - PAD_X - REMOVE_W;
    Cols {
        status_x,
        action_x,
        time_x,
        details_x,
        details_w: (remove_x - COL_GAP - details_x).max(0.0),
        remove_x,
    }
}

/// Render the table at exactly `width` (the caller pre-measures the viewport;
/// `available_width` inside a horizontal `ScrollArea` is virtual and would
/// stretch the columns arbitrarily). Returns the click, if any.
pub fn show(ui: &mut Ui, width: f32, rows: &[HistoryRow], palette: &Palette) -> HistoryAction {
    let empty = rows.is_empty();

    let (header, _) = ui.allocate_exact_size(Vec2::new(width, HEADER_H), Sense::hover());
    let cols = columns(header);
    let header_color = with_alpha(palette.subtle_text, if empty { 120 } else { 255 });
    let painter = ui.painter();
    for (x, text) in [
        (cols.status_x, "STATUS"),
        (cols.action_x, "ACTION"),
        (cols.time_x, "TIME"),
        (cols.details_x, "DETAILS"),
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
        return HistoryAction::None;
    }

    let mut action = HistoryAction::None;
    let mut body: Option<Rect> = None;
    let last = rows.len() - 1;
    for (i, row) in rows.iter().enumerate() {
        let rect = paint_row(ui, width, i, last, row, palette, &mut action);
        body = Some(body.map_or(rect, |b: Rect| b.union(rect)));
    }
    // The border goes on last so it rides over the row fills rather than being
    // half-covered by the one below it.
    if let Some(body) = body {
        ui.painter().rect_stroke(
            body,
            Rounding::same(TABLE_ROUNDING),
            Stroke::new(1.0, palette.panel_border),
        );
    }
    action
}

/// One entry. Returns its rect; pushes any click into `action`.
fn paint_row(
    ui: &mut Ui,
    width: f32,
    index: usize,
    last: usize,
    row: &HistoryRow,
    palette: &Palette,
    action: &mut HistoryAction,
) -> Rect {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::hover());
    let cols = columns(rect);

    // Zebra striping, with only the table's outer corners rounded so the rows
    // read as one body rather than a stack of cards.
    let rounding = Rounding {
        nw: if index == 0 { TABLE_ROUNDING } else { 0.0 },
        ne: if index == 0 { TABLE_ROUNDING } else { 0.0 },
        sw: if index == last { TABLE_ROUNDING } else { 0.0 },
        se: if index == last { TABLE_ROUNDING } else { 0.0 },
    };
    let stripe = if index.is_multiple_of(2) { 80 } else { 175 };
    ui.painter()
        .rect_filled(rect, rounding, with_alpha(palette.content_card_bg, stripe));
    let hovered = ui
        .interact(
            rect,
            ui.id().with(("history_row", index)),
            Sense::hover(),
        )
        .hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, rounding, with_alpha(palette.icon_color, 12));
    }

    let status_color = row.status.color(palette);
    let status = icon_label(
        row.status.icon(),
        fonts::icon_fill(15.0),
        row.status.label(),
        fonts::regular(13.5),
        status_color,
    );
    let galley = ui.fonts(|f| f.layout_job(status));
    ui.painter().galley(
        egui::pos2(cols.status_x, rect.center().y - galley.size().y * 0.5),
        galley,
        status_color,
    );

    let painter = ui.painter();
    painter.text(
        egui::pos2(cols.action_x, rect.center().y),
        Align2::LEFT_CENTER,
        &row.action,
        fonts::regular(14.0),
        palette.icon_color,
    );
    painter.text(
        egui::pos2(cols.time_x, rect.center().y),
        Align2::LEFT_CENTER,
        &row.time,
        fonts::regular(13.5),
        palette.subtle_text,
    );

    // Details is the one cell that routinely doesn't fit, so it elides and
    // spells itself out in full on hover.
    let details = details_galley(ui, row, palette, cols.details_w);
    let details_pos = egui::pos2(cols.details_x, rect.center().y - details.size().y * 0.5);
    let details_rect = Rect::from_min_size(details_pos, details.size());
    ui.painter().galley(details_pos, details, palette.icon_color);
    let full = match &row.error {
        Some(e) => format!("{} — {e}", row.details),
        None => row.details.clone(),
    };
    ui.interact(
        details_rect,
        ui.id().with(("history_details", index)),
        Sense::hover(),
    )
    .on_hover_text(full);

    let remove = Rect::from_center_size(
        egui::pos2(cols.remove_x + REMOVE_W * 0.5, rect.center().y),
        Vec2::splat(REMOVE_W),
    );
    if remove_button(ui, remove, index, palette) {
        *action = HistoryAction::Remove(index);
    }
    rect
}

/// The details text: what the job did, plus — when it failed — the reason, in
/// red, on the same line. Elided to `max_width`; the caller offers the full
/// text on hover.
fn details_galley(
    ui: &Ui,
    row: &HistoryRow,
    palette: &Palette,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &row.details,
        0.0,
        egui::TextFormat {
            font_id: fonts::regular(14.0),
            color: palette.icon_color,
            ..Default::default()
        },
    );
    if let Some(error) = &row.error {
        job.append(
            &format!(" — {error}"),
            0.0,
            egui::TextFormat {
                font_id: fonts::regular(14.0),
                color: palette.danger,
                ..Default::default()
            },
        );
    }
    job.wrap = egui::text::TextWrapping {
        max_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.fonts(|f| f.layout_job(job))
}

/// The row's remove button: a quiet ✕ that turns red under the pointer, since
/// it is the only destructive control on the page.
fn remove_button(ui: &mut Ui, rect: Rect, index: usize, palette: &Palette) -> bool {
    let response = ui
        .interact(rect, ui.id().with(("history_remove", index)), Sense::click())
        .on_hover_text("Remove this entry from the history");
    let color = if response.hovered() {
        palette.danger
    } else {
        with_alpha(palette.subtle_text, 150)
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        egui_phosphor::bold::X,
        fonts::icon_bold(15.0),
        color,
    );
    draw_focus_outline(ui, &response, palette);
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response.clicked()
        || (response.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space)))
}

/// Placeholder shown while nothing has been recorded: the table's own shape,
/// faded, so the page reads as "empty" rather than "missing".
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
        "No jobs recorded yet.",
        fonts::regular(14.0),
        with_alpha(palette.subtle_text, 140),
    );
}
