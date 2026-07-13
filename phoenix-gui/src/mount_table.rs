//! "Active mounts" table on the Mount page.
//!
//! One card per mounted backup, one sub-row per exposed drive letter. The
//! backup's folder, name, size and Unmount button belong to the whole card (a
//! two-letter mount unmounts as a unit) and align with its first letter, while
//! each letter gets its own Explore button:
//!
//! ```text
//! D:\Backups
//! fdBU.phnx     30.8 GB     I:   [Explore]   [Unmount]
//!                           J:   [Explore]
//! ```
//!
//! Hand-painted rather than built from an `egui::Grid`: a grid cannot span
//! rows, and `ui.put` inside an already-allocated card rect rewinds the
//! layout cursor. Every control is an `interact` + painter call at an
//! explicit rect, so the card owns its geometry outright.

use eframe::egui;
use egui::{Align2, Color32, CursorIcon, FontId, Rect, Rounding, Sense, Stroke, Ui, Vec2};

use crate::disk_map::{blend, with_alpha};
use crate::theme::{draw_focus_outline, Palette};
use crate::util::format_bytes;
use crate::{fonts, icon_label};

/// One mounted backup, flattened for display (keeps this module free of the
/// `phoenix_mount` types).
#[derive(Clone)]
pub struct MountRow {
    /// Folder the mounted `.phnx` lives in, shown dimmed above the name and
    /// spelled out in full on hover when it's too long for the column.
    pub folder: String,
    /// File name of the mounted `.phnx`.
    pub name: String,
    /// Size of the virtual disk the backup exposes.
    pub size: u64,
    /// Drive letters assigned, in partition order. Selected partitions Windows
    /// gave no letter to (Microsoft Reserved, an unreadable filesystem, …)
    /// simply don't appear — the table is a list of what you can browse.
    pub letters: Vec<char>,
}

/// What the user clicked this frame.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MountAction {
    None,
    /// Open this drive letter in Explorer.
    Explore(char),
    /// Unmount the backup at this index in the table's slice.
    Unmount(usize),
}

const HEADER_H: f32 = 22.0;
/// Height of one drive-letter line inside a card.
const SUB_ROW_H: f32 = 40.0;
const CARD_PAD_X: f32 = 14.0;
const CARD_PAD_Y: f32 = 6.0;
const CARD_GAP: f32 = 8.0;
const CARD_ROUNDING: f32 = 8.0;
const COL_GAP: f32 = 12.0;
const SIZE_W: f32 = 84.0;
const LETTER_W: f32 = 36.0;
const EXPLORE_W: f32 = 104.0;
const UNMOUNT_W: f32 = 112.0;
/// Floor for the elastic name column — below this the table stops shrinking
/// and the caller's horizontal `ScrollArea` takes over.
const NAME_MIN_W: f32 = 130.0;
const BTN_H: f32 = 30.0;
const BTN_ROUNDING: f32 = 6.0;
/// Leading between the dimmed folder line and the backup's name below it.
const FOLDER_GAP: f32 = 1.0;

/// Narrowest the table renders at. Callers size it to
/// `max(viewport, min_width())` — the same discipline the disk maps use —
/// so it fills a wide window and scrolls horizontally in a cramped one.
pub fn min_width() -> f32 {
    CARD_PAD_X * 2.0 + NAME_MIN_W + SIZE_W + LETTER_W + EXPLORE_W + UNMOUNT_W + COL_GAP * 4.0
}

/// Column geometry for a card/header of the given rect. The fixed columns are
/// pinned to the right edge and the name column absorbs the slack.
struct Cols {
    name_x: f32,
    name_w: f32,
    size_x: f32,
    letter_x: f32,
    explore_x: f32,
    unmount_x: f32,
}

fn columns(rect: Rect) -> Cols {
    let unmount_x = rect.right() - CARD_PAD_X - UNMOUNT_W;
    let explore_x = unmount_x - COL_GAP - EXPLORE_W;
    let letter_x = explore_x - COL_GAP - LETTER_W;
    let size_x = letter_x - COL_GAP - SIZE_W;
    let name_x = rect.left() + CARD_PAD_X;
    Cols {
        name_x,
        name_w: (size_x - COL_GAP - name_x).max(0.0),
        size_x,
        letter_x,
        explore_x,
        unmount_x,
    }
}

/// Render the table at exactly `width` (the caller pre-measures the viewport;
/// `available_width` inside a horizontal `ScrollArea` is virtual and would
/// stretch the columns arbitrarily). Returns the click, if any.
pub fn show(ui: &mut Ui, width: f32, rows: &[MountRow], palette: &Palette) -> MountAction {
    let mut action = MountAction::None;
    // The section is a permanent fixture of the page, so with nothing mounted
    // the table stays put and greys out rather than vanishing.
    let empty = rows.is_empty();

    let (header, _) = ui.allocate_exact_size(Vec2::new(width, HEADER_H), Sense::hover());
    let cols = columns(header);
    let font = fonts::bold(11.0);
    let header_color = with_alpha(palette.subtle_text, if empty { 120 } else { 255 });
    let painter = ui.painter();
    for (x, text) in [
        (cols.name_x, "BACKUP"),
        (cols.size_x, "SIZE"),
        (cols.letter_x, "DRIVE"),
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
        if let Some(clicked) = card(ui, width, i, row, palette) {
            action = clicked;
        }
    }
    action
}

/// Placeholder card shown while nothing is mounted: the table's own shape,
/// faded out, so the section reads as "empty" rather than "missing".
fn empty_card(ui: &mut Ui, width: f32, palette: &Palette) {
    let height = SUB_ROW_H + CARD_PAD_Y * 2.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    ui.painter().rect_filled(
        rect,
        Rounding::same(CARD_ROUNDING),
        with_alpha(palette.content_card_bg, 110),
    );
    ui.painter().text(
        egui::pos2(rect.left() + CARD_PAD_X, rect.center().y),
        Align2::LEFT_CENTER,
        "No active mounts",
        fonts::regular(14.0),
        with_alpha(palette.subtle_text, 140),
    );
}

/// One backup's card. Returns the click it produced, if any.
fn card(
    ui: &mut Ui,
    width: f32,
    index: usize,
    row: &MountRow,
    palette: &Palette,
) -> Option<MountAction> {
    // A mount whose selection exposed no letter at all still gets a row, so it
    // can be seen — and unmounted.
    let sub_rows = row.letters.len().max(1);
    let height = sub_rows as f32 * SUB_ROW_H + CARD_PAD_Y * 2.0;

    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    ui.painter()
        .rect_filled(rect, Rounding::same(CARD_ROUNDING), palette.content_card_bg);
    let cols = columns(rect);

    let sub_row_rect = |k: usize| {
        let top = rect.top() + CARD_PAD_Y + k as f32 * SUB_ROW_H;
        Rect::from_min_size(
            egui::pos2(rect.left(), top),
            Vec2::new(rect.width(), SUB_ROW_H),
        )
    };
    // Name, size and Unmount belong to the mount as a whole, but they line up
    // with its FIRST drive letter rather than floating in the middle of the
    // card: on a two-letter mount, centering left them hanging between the
    // rows. The folder rides above the name, dimmed — two mounts can share a
    // file name, and where they came from is the only thing telling them apart.
    let top_line = sub_row_rect(0);
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
    let block_top = top_line.center().y - block_h * 0.5;
    let folder_pos = egui::pos2(cols.name_x, block_top);
    ui.painter()
        .galley(folder_pos, folder.clone(), palette.subtle_text);
    // The folder is the one cell that routinely doesn't fit, so hovering it
    // spells the path out in full.
    ui.interact(
        Rect::from_min_size(folder_pos, folder.size()),
        ui.id().with(("mount_folder", index)),
        Sense::hover(),
    )
    .on_hover_text(&row.folder);
    let painter = ui.painter();
    painter.galley(
        egui::pos2(cols.name_x, block_top + folder.size().y + FOLDER_GAP),
        name,
        palette.icon_color,
    );
    painter.text(
        egui::pos2(cols.size_x, top_line.center().y),
        Align2::LEFT_CENTER,
        format_bytes(row.size),
        fonts::regular(14.0),
        palette.subtle_text,
    );

    let mut action = None;
    for (k, &letter) in row.letters.iter().enumerate() {
        let line = sub_row_rect(k);
        ui.painter().text(
            egui::pos2(cols.letter_x, line.center().y),
            Align2::LEFT_CENTER,
            format!("{letter}:"),
            fonts::bold(15.0),
            palette.icon_color,
        );
        let button = Rect::from_min_size(
            egui::pos2(cols.explore_x, line.center().y - BTN_H * 0.5),
            Vec2::new(EXPLORE_W, BTN_H),
        );
        if table_button(
            ui,
            button,
            ui.id().with(("mount_explore", index, k)),
            egui_phosphor::regular::FOLDER_OPEN,
            "Explore",
            palette.accent,
            palette,
        ) {
            action = Some(MountAction::Explore(letter));
        }
    }

    if row.letters.is_empty() {
        let line = sub_row_rect(0);
        ui.painter().text(
            egui::pos2(cols.letter_x, line.center().y),
            Align2::LEFT_CENTER,
            "—",
            fonts::regular(14.0),
            palette.subtle_text,
        );
    }

    // One Unmount for the whole backup — it takes down the attached disk,
    // letters and all — parked on the top line with the name and size.
    let unmount = Rect::from_min_size(
        egui::pos2(cols.unmount_x, top_line.center().y - BTN_H * 0.5),
        Vec2::new(UNMOUNT_W, BTN_H),
    );
    if table_button(
        ui,
        unmount,
        ui.id().with(("mount_unmount", index)),
        egui_phosphor::regular::EJECT,
        "Unmount",
        palette.danger,
        palette,
    ) {
        action = Some(MountAction::Unmount(index));
    }

    action
}

/// A card-local button: a raised chip against the card fill that tints toward
/// `hover_tint` on hover (accent for Explore, danger for Unmount). Painted by
/// hand so it can live at an arbitrary rect inside the card without disturbing
/// the layout cursor.
fn table_button(
    ui: &mut Ui,
    rect: Rect,
    id: egui::Id,
    icon: &str,
    label: &str,
    hover_tint: Color32,
    palette: &Palette,
) -> bool {
    let response = ui.interact(rect, id, Sense::click());
    let raised = if palette.light_mode {
        blend(palette.content_card_bg, Color32::WHITE, 0.55)
    } else {
        blend(palette.content_card_bg, Color32::WHITE, 0.10)
    };
    let fill = if response.is_pointer_button_down_on() {
        blend(raised, hover_tint, 0.45)
    } else if response.hovered() {
        blend(raised, hover_tint, 0.25)
    } else {
        raised
    };
    let border = if response.hovered() {
        hover_tint
    } else {
        with_alpha(palette.subtle_text, 90)
    };

    let painter = ui.painter();
    painter.rect_filled(rect, Rounding::same(BTN_ROUNDING), fill);
    painter.rect_stroke(rect, Rounding::same(BTN_ROUNDING), Stroke::new(1.0, border));
    let job = icon_label(
        icon,
        fonts::icon(15.0),
        label,
        fonts::regular(13.0),
        palette.icon_color,
    );
    let galley = ui.fonts(|f| f.layout_job(job));
    let pos = rect.center() - galley.size() * 0.5;
    ui.painter().galley(pos, galley, palette.icon_color);

    draw_focus_outline(ui, &response, palette);
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response.clicked()
        || (response.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space)))
}

/// Lay out `text` on a single line, ellipsizing anything past `max_width` —
/// long backup names shrink into the name column instead of running under the
/// size column.
fn elided(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.fonts(|f| f.layout_job(job))
}
