//! The known-backups table on the Restore + Verify page's list view.
//!
//! Same hand-painted, zebra-striped, click-to-select body as
//! [`crate::bootrepair_table`], with one extra trick: the first column is a
//! *shield* that is its own click target. A green filled shield means the
//! image has already been verified on this machine (hover says so); a red
//! filled warning shield means a verify ran and *failed* (the file is kept on
//! disk regardless); a dull-yellow outline shield means it hasn't been
//! verified yet. Clicking a red or yellow shield kicks off a verify of that one
//! image (hover says so). Clicking anywhere else on the row selects it — the
//! pick the action bar's "Restore" then acts on.
//!
//! ```text
//! ⛊   BACKUP                               CREATED               SIZE
//! ⛉   D:\Backups\work.phnx                 2026-07-12 14:32:07   482.1 GB
//! ⛊   E:\Images\laptop-os.phnx            2026-06-30 09:05:11   118.7 GB
//! ```

use egui;
use egui::{Align2, CursorIcon, Rect, Rounding, Sense, Stroke, Ui, Vec2};

use crate::disk_map::with_alpha;
use crate::fonts;
use crate::theme::Palette;

/// Verification standing of a backup, derived from the job history — drives
/// the shield's colour, glyph, and whether it's a click-to-verify target.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VerifyState {
    /// A verify (explicit or verify-after-backup) succeeded for this path and
    /// no later verify has failed — green filled shield.
    Verified,
    /// The most recent verify *ran and failed* (mismatch/error). The backup is
    /// never discarded for this — the file stays on disk and the shield turns
    /// red so the failure is visible. Clicking re-runs the verify.
    Failed,
    /// Never verified clean and no failure on record — this also covers a
    /// verify that was cancelled mid-pass (integrity simply unknown). Dull-
    /// yellow outline shield; clicking kicks off a verify.
    Unverified,
}

/// One backup, flattened for display. `created`/`size` are pre-formatted by
/// the caller (which owns the metadata cache), so the table does no file I/O.
pub struct BackupRow {
    /// Full path — shown in the elastic BACKUP column, elided, spelled out on
    /// hover.
    pub path: String,
    /// Local wall-clock creation date, or "—" when it couldn't be read.
    pub created: String,
    /// On-disk image size (e.g. "482.1 GB"), or "—".
    pub size: String,
    /// Verification standing — the shield's state (green / red / yellow).
    pub verify: VerifyState,
}

/// What one [`show`] call produced. A shield click verifies; a row click
/// selects. They are mutually exclusive per frame (the shield wins).
#[derive(Default)]
pub struct TableEvent {
    /// A row body was clicked — the new selection.
    pub row_clicked: Option<usize>,
    /// A row's dull-yellow shield was clicked — verify that image.
    pub verify_clicked: Option<usize>,
}

const HEADER_H: f32 = 24.0;
const ROW_H: f32 = 42.0;
const PAD_X: f32 = 14.0;
const COL_GAP: f32 = 14.0;
const SHIELD_W: f32 = 26.0;
/// Floor for the elastic path column — below this the table stops shrinking
/// and the caller's horizontal `ScrollArea` takes over.
const BACKUP_MIN_W: f32 = 220.0;
const CREATED_W: f32 = 150.0;
const SIZE_W: f32 = 88.0;
const TABLE_ROUNDING: f32 = 8.0;
/// Width of the accent bar marking the selected row (mirrors the sidebar's
/// selected-page marker and the Boot Repair table).
const SELECT_BAR_W: f32 = 3.0;

/// Narrowest the table renders at; callers size it to `max(viewport,
/// min_width())`, the same discipline as the Boot Repair and history tables.
pub fn min_width() -> f32 {
    PAD_X * 2.0 + SHIELD_W + BACKUP_MIN_W + CREATED_W + SIZE_W + COL_GAP * 3.0
}

/// Column x-positions: the shield is pinned left, the path column is elastic,
/// created/size are pinned rightward.
struct Cols {
    shield_x: f32,
    backup_x: f32,
    backup_w: f32,
    created_x: f32,
    size_x: f32,
}

fn columns(rect: Rect) -> Cols {
    let size_x = rect.right() - PAD_X - SIZE_W;
    let created_x = size_x - COL_GAP - CREATED_W;
    let shield_x = rect.left() + PAD_X;
    let backup_x = shield_x + SHIELD_W + COL_GAP;
    Cols {
        shield_x,
        backup_x,
        backup_w: (created_x - COL_GAP - backup_x).max(0.0),
        created_x,
        size_x,
    }
}

/// Render the table at exactly `width`. The caller owns the selection state
/// (`selected`) and both event kinds returned.
pub fn show(
    ui: &mut Ui,
    width: f32,
    rows: &[BackupRow],
    selected: Option<usize>,
    palette: &Palette,
) -> TableEvent {
    let empty = rows.is_empty();

    let (header, _) = ui.allocate_exact_size(Vec2::new(width, HEADER_H), Sense::hover());
    let cols = columns(header);
    let header_color = with_alpha(palette.subtle_text, if empty { 120 } else { 255 });
    let painter = ui.painter();
    for (x, text) in [
        (cols.backup_x, "BACKUP"),
        (cols.created_x, "CREATED"),
        (cols.size_x, "SIZE"),
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
        return TableEvent::default();
    }

    let mut event = TableEvent::default();
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
            &mut event,
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
    event
}

/// One entry. Returns its rect; records any click into `event`.
#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &mut Ui,
    width: f32,
    index: usize,
    last: usize,
    row: &BackupRow,
    selected: bool,
    palette: &Palette,
    event: &mut TableEvent,
) -> Rect {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::hover());
    let cols = columns(rect);

    // Zebra striping with only the table's outer corners rounded — the rows
    // read as one body (same as the Boot Repair and history tables).
    let rounding = Rounding {
        nw: if index == 0 { TABLE_ROUNDING } else { 0.0 },
        ne: if index == 0 { TABLE_ROUNDING } else { 0.0 },
        sw: if index == last { TABLE_ROUNDING } else { 0.0 },
        se: if index == last { TABLE_ROUNDING } else { 0.0 },
    };
    let stripe = if index.is_multiple_of(2) { 80 } else { 175 };
    ui.painter()
        .rect_filled(rect, rounding, with_alpha(palette.content_card_bg, stripe));

    // The shield's own click target, allocated first so the row body below it
    // can tell (via else-if) not to double-count a shield click as a select.
    let shield_rect = Rect::from_center_size(
        egui::pos2(cols.shield_x + SHIELD_W * 0.5, rect.center().y),
        Vec2::new(SHIELD_W, ROW_H),
    );
    // The row is interacted first, the shield second, so the shield sits ON
    // TOP: egui gives an overlapped click to the last-registered widget, so a
    // click on the shield reaches the shield (not the row-select beneath it).
    let row_resp = ui.interact(rect, ui.id().with(("restore_row", index)), Sense::click());
    let shield_resp = ui.interact(
        shield_rect,
        ui.id().with(("restore_shield", index)),
        Sense::click(),
    );

    if selected {
        ui.painter()
            .rect_filled(rect, rounding, with_alpha(palette.accent, 26));
        let bar = Rect::from_min_max(
            rect.left_top() + Vec2::new(2.0, 6.0),
            egui::pos2(rect.left() + 2.0 + SELECT_BAR_W, rect.bottom() - 6.0),
        );
        ui.painter()
            .rect_filled(bar, Rounding::same(2.0), palette.accent);
    } else if row_resp.hovered() && !shield_resp.hovered() {
        ui.painter()
            .rect_filled(rect, rounding, with_alpha(palette.icon_color, 12));
    }

    // Shield: green filled = verified; red filled warning = verify ran and
    // failed (backup kept regardless); dull-yellow outline = not yet verified.
    // The two actionable states (red, yellow) brighten on hover to read as the
    // "click to verify" control they are.
    let (glyph, font, color, tip) = match row.verify {
        VerifyState::Verified => (
            egui_phosphor::fill::SHIELD_CHECK,
            fonts::icon_fill(19.0),
            palette.success,
            "This backup has been verified on this machine.",
        ),
        VerifyState::Failed => {
            let base = with_alpha(palette.danger, 210);
            let color = if shield_resp.hovered() {
                palette.danger
            } else {
                base
            };
            (
                egui_phosphor::fill::SHIELD_WARNING,
                fonts::icon_fill(19.0),
                color,
                "Verification FAILED for this backup — it did not verify clean. \
                 The file is kept on disk; click to verify again.",
            )
        }
        VerifyState::Unverified => {
            let base = with_alpha(palette.warning, 190);
            let color = if shield_resp.hovered() {
                palette.warning
            } else {
                base
            };
            (
                egui_phosphor::regular::SHIELD_CHECK,
                fonts::icon(19.0),
                color,
                "Not verified on this machine — click to verify this backup.",
            )
        }
    };
    ui.painter().text(
        egui::pos2(cols.shield_x + SHIELD_W * 0.5, rect.center().y),
        Align2::CENTER_CENTER,
        glyph,
        font,
        color,
    );

    // Whole row is a pointer target — the shield to verify (when unverified),
    // everything else to select.
    if shield_resp.hovered() || row_resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    // Click routing: a red or yellow shield verifies; anything else (including
    // a click on an already-green shield, which has nothing to verify) selects.
    let actionable = matches!(row.verify, VerifyState::Failed | VerifyState::Unverified);
    let shield_resp = shield_resp.on_hover_text(tip);
    if shield_resp.clicked() && actionable {
        event.verify_clicked = Some(index);
    } else if row_resp.clicked() || shield_resp.clicked() {
        event.row_clicked = Some(index);
    }

    // The path can routinely run long — elide it and spell it out on hover.
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &row.path,
        0.0,
        egui::TextFormat {
            font_id: fonts::regular(14.0),
            color: palette.icon_color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: cols.backup_w,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = ui.fonts(|f| f.layout_job(job));
    ui.painter().galley(
        egui::pos2(cols.backup_x, rect.center().y - galley.size().y * 0.5),
        galley,
        palette.icon_color,
    );
    // Spell out the full path anywhere on the row (the path column is the one
    // that gets elided).
    row_resp.on_hover_text(&row.path);

    let painter = ui.painter();
    for (x, text, color) in [
        (cols.created_x, row.created.as_str(), palette.subtle_text),
        (cols.size_x, row.size.as_str(), palette.subtle_text),
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

/// Placeholder shown when no backups are known yet: the table's own shape,
/// faded, so the page reads as "empty" rather than "missing" (same contract
/// as the Boot Repair and mounts tables).
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
        "No backups yet — create one on the Backup page, or use “+ Add Existing Backup”.",
        fonts::regular(14.0),
        with_alpha(palette.subtle_text, 140),
    );
}
