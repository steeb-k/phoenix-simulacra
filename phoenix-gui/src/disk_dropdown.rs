//! Rich disk dropdown shared by the Clone and Restore pages: the closed
//! face is caller-rendered (typically the selected disk's full
//! `[info card | partition map]` row, or a hint field while nothing is
//! picked), and the popup lists every disk as a full row.

use eframe::egui;
use egui::{Align2, CursorIcon, Rect, Rounding, Sense, Stroke, Ui, Vec2};

use phoenix_core::disk::DiskInfo;

use crate::disk_map::{
    self, draw_disk_info_card, draw_partition_map, draw_partition_segment_visual_styled,
    CHECKBOX_COLUMN_WIDTH, CHECKBOX_GAP, INFO_CARD_GAP, INFO_CARD_WIDTH, ROW_HEIGHT,
    ROW_VERTICAL_GAP,
};
use crate::fonts;
use crate::restore_panel::active_drag;
use crate::theme::Palette;

/// Width reserved at the right edge of a dropdown control for the chevron.
pub(crate) const CHEVRON_COL_W: f32 = 34.0;
/// Height of a dropdown's closed face while no disk is chosen yet.
pub(crate) const EMPTY_FACE_HEIGHT: f32 = 44.0;

/// Natural minimum width of a drive row: like
/// [`disk_map::min_disk_row_width`] but without the checkbox column these
/// rows don't have.
pub(crate) fn min_row_width(disk: &DiskInfo) -> f32 {
    (disk_map::min_disk_row_width(disk) - CHECKBOX_COLUMN_WIDTH - CHECKBOX_GAP).max(0.0)
}

/// A dropdown control whose closed face is rendered by `face` (the selected
/// disk's full row, a layout editor, or a hint field) and whose popup lists
/// every disk as a full `[info card | partition map]` row. `face` returns
/// true when a click on it should open the popup. `dim_index` is shown
/// dimmed and unpickable (the clone target dropdown dims the source disk).
/// Returns the disk picked from the popup this frame, if any.
#[allow(clippy::too_many_arguments)]
pub(crate) fn disk_dropdown(
    ui: &mut Ui,
    id_salt: &str,
    disks: &[DiskInfo],
    selected: Option<u32>,
    dim_index: Option<u32>,
    palette: &Palette,
    viewport_width: f32,
    face: impl FnOnce(&mut Ui, f32) -> bool,
) -> Option<u32> {
    let popup_id = ui.make_persistent_id(id_salt);
    let mut picked: Option<u32> = None;

    let control = ui.horizontal(|ui| {
        let spacing = ui.spacing().item_spacing.x;
        let face_width = (viewport_width - CHEVRON_COL_W - spacing).max(0.0);
        // Constrain the face so the chevron never gets pushed off the edge.
        let mut wants_open = false;
        ui.scope(|ui| {
            ui.set_max_width(face_width);
            wants_open = face(ui, face_width);
        });
        let face_height = ui.min_rect().height().max(EMPTY_FACE_HEIGHT);
        wants_open | dropdown_chevron(ui, face_height, popup_id, palette)
    });
    let wants_open = control.inner;
    let anchor = control.response;

    if wants_open {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
    }
    egui::popup_below_widget(
        ui,
        popup_id,
        &anchor,
        egui::PopupCloseBehavior::CloseOnClick,
        |ui| {
            ui.set_min_width(anchor.rect.width() - 16.0);
            let avail = ui.available_width();
            egui::ScrollArea::both()
                .max_height(disk_map::row_stride() * 3.6)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let natural = disks.iter().map(min_row_width).fold(0.0_f32, f32::max);
                    let row_width = avail.max(natural);
                    for disk in disks {
                        if dim_index == Some(disk.index) {
                            ui.scope(|ui| {
                                ui.set_opacity(0.4);
                                draw_disk_list_row(ui, row_width, disk, false, false, palette);
                            })
                            .response
                            .on_hover_text("This disk is the other end of the operation");
                        } else {
                            let ev = draw_disk_list_row(
                                ui,
                                row_width,
                                disk,
                                selected == Some(disk.index),
                                false,
                                palette,
                            );
                            if ev.clicked {
                                picked = Some(disk.index);
                            }
                        }
                        ui.add_space(ROW_VERTICAL_GAP);
                    }
                });
        },
    );

    picked
}

/// The `⌄` column at the right edge of a dropdown control. Returns true on
/// click (the caller toggles the popup).
fn dropdown_chevron(ui: &mut Ui, height: f32, popup_id: egui::Id, palette: &Palette) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(CHEVRON_COL_W - 6.0, height), Sense::click());
    let hovered = response.hovered();
    let open = ui.memory(|m| m.is_popup_open(popup_id));
    let painter = ui.painter_at(rect);
    if hovered {
        painter.rect_filled(rect, Rounding::same(8.0), palette.sidebar_hover_bg);
    }
    let color = if hovered || open {
        palette.accent
    } else {
        palette.icon_color
    };
    let icon = if open {
        egui_phosphor::regular::CARET_UP
    } else {
        egui_phosphor::regular::CARET_DOWN
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        fonts::icon(20.0),
        color,
    );
    let response = response.on_hover_text("Choose a disk");
    crate::theme::draw_focus_outline(ui, &response, palette);
    response.clicked()
}

/// ComboBox-like placeholder face shown until a disk is picked. Returns
/// true on click.
pub(crate) fn draw_empty_face(ui: &mut Ui, width: f32, hint: &str, palette: &Palette) -> bool {
    let response = draw_face_frame(ui, width, hint, palette, true);
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    crate::theme::draw_focus_outline(ui, &response, palette);
    response.clicked()
}

/// Inert twin of [`draw_empty_face`]: the Restore page's source row is filled
/// by the Browse field above it, not by a click here, so the placeholder must
/// not advertise itself as a control.
pub(crate) fn draw_hint_face(ui: &mut Ui, width: f32, hint: &str, palette: &Palette) {
    draw_face_frame(ui, width, hint, palette, false);
}

fn draw_face_frame(
    ui: &mut Ui,
    width: f32,
    hint: &str,
    palette: &Palette,
    interactive: bool,
) -> egui::Response {
    let sense = if interactive {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, EMPTY_FACE_HEIGHT), sense);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, Rounding::same(6.0), palette.content_card_bg);
    let stroke_color = if interactive && response.hovered() {
        palette.accent
    } else {
        disk_map::with_alpha(palette.subtle_text, 90)
    };
    // Inset by half the stroke width (radius reduced to match) so the whole
    // stroke — corner arcs included — lands inside the clip rect and hugs the
    // fill's silhouette. A stroke centered on the rect edge gets its straight
    // runs halved by the clip while the corner arcs escape it, leaving bare
    // panel background showing past the rounded fill at each corner (same
    // artifact `disk_map::draw_selection_stroke` fixes on the cards).
    let w = 1.0;
    painter.rect_stroke(
        rect.shrink(w / 2.0),
        Rounding::same(6.0 - w / 2.0),
        Stroke::new(w, stroke_color),
    );
    painter.text(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        hint,
        fonts::regular(15.0),
        palette.subtle_text,
    );
    response
}

pub(crate) struct RowEvents {
    pub(crate) clicked: bool,
    pub(crate) drag_started: Option<u32>,
}

/// One drive row: `[info card | partition map]`, no checkbox column.
/// Clicking reports `clicked` (select in a popup, reopen on a closed face);
/// when `draggable`, partitions can be picked up for the partial-clone drop
/// gesture instead.
pub(crate) fn draw_disk_list_row(
    ui: &mut Ui,
    row_width: f32,
    disk: &DiskInfo,
    selected: bool,
    draggable: bool,
    palette: &Palette,
) -> RowEvents {
    let mut events = RowEvents {
        clicked: false,
        drag_started: None,
    };
    let row_size = Vec2::new(row_width, ROW_HEIGHT);
    let (row_rect, _) = ui.allocate_exact_size(row_size, Sense::hover());

    let info_rect =
        Rect::from_min_size(row_rect.left_top(), Vec2::new(INFO_CARD_WIDTH, ROW_HEIGHT));
    let info_resp = ui.interact(
        info_rect,
        ui.id().with(("disk_row", disk.index)),
        Sense::click(),
    );
    if info_resp.clicked() {
        events.clicked = true;
    }
    if info_resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    draw_disk_info_card(ui, info_rect, disk, palette, selected);
    crate::theme::draw_focus_outline(ui, &info_resp, palette);

    let map_rect = Rect::from_min_max(
        egui::pos2(info_rect.right() + INFO_CARD_GAP, row_rect.top()),
        row_rect.right_bottom(),
    );
    let dragging = active_drag(ui.ctx());
    draw_partition_map(ui, map_rect, disk, palette, |ui, _disk_idx, p, seg_rect| {
        let id = ui.id().with(("disk_part", disk.index, p.index));
        let sense = if draggable {
            Sense::click_and_drag()
        } else {
            Sense::click()
        };
        let response = ui.interact(seg_rect, id, sense);
        if response.clicked() {
            events.clicked = true;
        }
        if draggable {
            if response.drag_started() {
                events.drag_started = Some(p.index);
            }
            if response.hovered() && dragging.is_none() {
                ui.ctx().set_cursor_icon(CursorIcon::Grab);
            }
        }
        let lifted = draggable && dragging == Some(p.index);
        draw_partition_segment_visual_styled(
            ui,
            seg_rect,
            p,
            palette,
            response.hovered(),
            lifted,
            None,
        );
        response.on_hover_ui_at_pointer(|ui| disk_map::partition_tooltip(ui, p));
        true
    });

    events
}
