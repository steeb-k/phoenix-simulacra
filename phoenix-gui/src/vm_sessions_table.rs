//! "Saved sessions" table on the Virtualize page.
//!
//! Deliberately the same visual language as the Mount page's Active-mounts
//! table — header hairline with small-caps column labels, one rounded card per
//! row on `content_card_bg`, dimmed folder line above the backup name, and the
//! shared hand-painted chip buttons — so the two "things you currently have"
//! tables read as siblings.

use egui::{Align2, Rect, Rounding, Sense, Stroke, Ui, Vec2};

use crate::disk_map::with_alpha;
use crate::fonts;
use crate::mount_table::{elided, table_button};
use crate::theme::Palette;
use crate::util::format_bytes;

/// One saved session, flattened for display.
pub struct SessionRow {
    /// Folder the backup lives in, dimmed above the name (mount-table style).
    pub folder: String,
    /// File name of the `.phnx`.
    pub name: String,
    pub state: RowState,
    /// Size of the session's write overlay on disk.
    pub overlay_bytes: u64,
}

/// What the "Last booted" column shows.
pub enum RowState {
    /// This session's VM is running right now.
    Running,
    /// The previous boot never tore down cleanly (crash / power cut).
    Interrupted,
    /// Clean session; when it last booted ("2026-07-19 05:04"), or "never".
    LastBooted(String),
}

/// What the user clicked this frame (index into the rows slice).
pub enum SessionAction {
    None,
    Resume(usize),
    Discard(usize),
}

const HEADER_H: f32 = 22.0;
const ROW_H: f32 = 40.0;
const CARD_PAD_X: f32 = 14.0;
const CARD_PAD_Y: f32 = 6.0;
const CARD_GAP: f32 = 8.0;
const CARD_ROUNDING: f32 = 8.0;
const COL_GAP: f32 = 12.0;
const BOOTED_W: f32 = 122.0;
const WRITES_W: f32 = 96.0;
const RESUME_W: f32 = 104.0;
const DISCARD_W: f32 = 104.0;
const NAME_MIN_W: f32 = 130.0;
const BTN_H: f32 = 30.0;
/// Leading between the dimmed folder line and the backup's name below it.
const FOLDER_GAP: f32 = 1.0;

/// Narrowest the table renders at (same discipline as the mount table).
pub fn min_width() -> f32 {
    CARD_PAD_X * 2.0 + NAME_MIN_W + BOOTED_W + WRITES_W + RESUME_W + DISCARD_W + COL_GAP * 4.0
}

/// Column geometry: fixed columns pinned right, name absorbs the slack.
struct Cols {
    name_x: f32,
    name_w: f32,
    booted_x: f32,
    writes_x: f32,
    resume_x: f32,
    discard_x: f32,
}

fn columns(rect: Rect) -> Cols {
    let discard_x = rect.right() - CARD_PAD_X - DISCARD_W;
    let resume_x = discard_x - COL_GAP - RESUME_W;
    let writes_x = resume_x - COL_GAP - WRITES_W;
    let booted_x = writes_x - COL_GAP - BOOTED_W;
    let name_x = rect.left() + CARD_PAD_X;
    Cols {
        name_x,
        name_w: (booted_x - COL_GAP - name_x).max(0.0),
        booted_x,
        writes_x,
        resume_x,
        discard_x,
    }
}

/// Render the table at exactly `width`. `buttons_enabled` is false while a VM
/// runs (one at a time), which greys the chips without hiding the rows.
pub fn show(
    ui: &mut Ui,
    width: f32,
    rows: &[SessionRow],
    buttons_enabled: bool,
    palette: &Palette,
) -> SessionAction {
    let mut action = SessionAction::None;
    let empty = rows.is_empty();

    let (header, _) = ui.allocate_exact_size(Vec2::new(width, HEADER_H), Sense::hover());
    let cols = columns(header);
    let font = fonts::bold(11.0);
    let header_color = with_alpha(palette.subtle_text, if empty { 120 } else { 255 });
    let painter = ui.painter();
    for (x, text) in [
        (cols.name_x, "BACKUP"),
        (cols.booted_x, "LAST BOOTED"),
        (cols.writes_x, "GUEST WRITES"),
    ] {
        painter.text(
            egui::pos2(x, header.center().y),
            Align2::LEFT_CENTER,
            text,
            font.clone(),
            header_color,
        );
    }
    painter.hline(
        (header.left() + CARD_PAD_X)..=(header.right() - CARD_PAD_X),
        header.bottom(),
        Stroke::new(
            1.0,
            with_alpha(palette.subtle_text, if empty { 40 } else { 70 }),
        ),
    );

    if empty {
        ui.add_space(CARD_GAP);
        empty_card(ui, width, palette);
        return action;
    }

    for (i, row) in rows.iter().enumerate() {
        ui.add_space(CARD_GAP);
        if let Some(clicked) = card(ui, width, i, row, buttons_enabled, palette) {
            action = clicked;
        }
    }
    action
}

/// Placeholder card shown with no sessions saved: the table's own shape, faded
/// out, so the section reads as "empty" rather than "missing".
fn empty_card(ui: &mut Ui, width: f32, palette: &Palette) {
    let height = ROW_H + CARD_PAD_Y * 2.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    ui.painter().rect_filled(
        rect,
        Rounding::same(CARD_ROUNDING),
        with_alpha(palette.content_card_bg, 110),
    );
    ui.painter().text(
        egui::pos2(rect.left() + CARD_PAD_X, rect.center().y),
        Align2::LEFT_CENTER,
        "No saved sessions yet — boot a backup to create one",
        fonts::regular(14.0),
        with_alpha(palette.subtle_text, 140),
    );
}

/// One session's card. Returns the click it produced, if any.
fn card(
    ui: &mut Ui,
    width: f32,
    index: usize,
    row: &SessionRow,
    buttons_enabled: bool,
    palette: &Palette,
) -> Option<SessionAction> {
    let height = ROW_H + CARD_PAD_Y * 2.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    ui.painter()
        .rect_filled(rect, Rounding::same(CARD_ROUNDING), palette.content_card_bg);
    let cols = columns(rect);

    // Folder dimmed above the name, exactly like a mount card — two sessions
    // can share a file name, and where the backup lives tells them apart.
    let folder = elided(
        ui,
        &row.folder,
        fonts::regular(11.5),
        palette.subtle_text,
        cols.name_w,
    );
    let name = elided(
        ui,
        &row.name,
        fonts::regular(14.0),
        palette.icon_color,
        cols.name_w,
    );
    let block_h = folder.size().y + FOLDER_GAP + name.size().y;
    let block_top = rect.center().y - block_h * 0.5;
    let folder_pos = egui::pos2(cols.name_x, block_top);
    ui.painter()
        .galley(folder_pos, folder.clone(), palette.subtle_text);
    ui.interact(
        Rect::from_min_size(folder_pos, folder.size()),
        ui.id().with(("session_folder", index)),
        Sense::hover(),
    )
    .on_hover_text(&row.folder);
    let painter = ui.painter();
    painter.galley(
        egui::pos2(cols.name_x, block_top + folder.size().y + FOLDER_GAP),
        name,
        palette.icon_color,
    );

    let (state_text, state_color) = match &row.state {
        RowState::Running => ("running".to_string(), palette.accent),
        RowState::Interrupted => ("interrupted".to_string(), palette.warning),
        RowState::LastBooted(when) => (when.clone(), palette.subtle_text),
    };
    painter.text(
        egui::pos2(cols.booted_x, rect.center().y),
        Align2::LEFT_CENTER,
        state_text,
        fonts::regular(14.0),
        state_color,
    );
    painter.text(
        egui::pos2(cols.writes_x, rect.center().y),
        Align2::LEFT_CENTER,
        format_bytes(row.overlay_bytes),
        fonts::regular(14.0),
        palette.subtle_text,
    );

    let mut action = None;
    let resume = Rect::from_min_size(
        egui::pos2(cols.resume_x, rect.center().y - BTN_H * 0.5),
        Vec2::new(RESUME_W, BTN_H),
    );
    if table_button(
        ui,
        resume,
        ui.id().with(("session_resume", index)),
        egui_phosphor::regular::PLAY,
        "Resume",
        palette.accent,
        buttons_enabled,
        palette,
    ) {
        action = Some(SessionAction::Resume(index));
    }
    let discard = Rect::from_min_size(
        egui::pos2(cols.discard_x, rect.center().y - BTN_H * 0.5),
        Vec2::new(DISCARD_W, BTN_H),
    );
    if table_button(
        ui,
        discard,
        ui.id().with(("session_discard", index)),
        egui_phosphor::regular::TRASH,
        "Discard",
        palette.danger,
        buttons_enabled,
        palette,
    ) {
        action = Some(SessionAction::Discard(index));
    }

    action
}
