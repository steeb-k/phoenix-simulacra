//! Clone page: source/target disk dropdowns whose popups expose full drive
//! rows, the source→target flow arrow, and the drag-to-partial-clone layout
//! editor.
//!
//! The page reuses the Restore page's layout-editing machinery
//! ([`RestoreLayoutState`] + [`restore_panel::draw_target_row`]): the target
//! dropdown's closed face IS the editor once both disks are picked. By
//! default the plan is a full-disk clone (source layout stamped over the
//! whole target); dragging a partition off the source's face flips the plan
//! to a partial clone of just what gets dropped, with the same
//! move/resize/delete editing as a partial restore.

use eframe::egui;
use egui::{CursorIcon, Rect, Rounding, Sense, Stroke, Ui, Vec2};

use phoenix_core::disk::DiskInfo;

use crate::disk_dropdown::{disk_dropdown, draw_disk_list_row, draw_empty_face, min_row_width};
use crate::disk_map;
use crate::restore_layout::RestoreLayoutState;
use crate::restore_panel::{
    active_drag, drag_lifecycle, drag_source_id, draw_layout_toolbar, draw_target_row,
};
use crate::theme::Palette;

pub struct ClonePanelOutput {
    pub refresh_clicked: bool,
    /// A different source/target disk was picked this frame; the caller
    /// rebuilds the layout state before the next frame renders.
    pub selection_changed: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    disks: &[DiskInfo],
    source_index: &mut Option<u32>,
    target_index: &mut Option<u32>,
    mut layout: Option<&mut RestoreLayoutState>,
    expand: bool,
    palette: &Palette,
    viewport_width: f32,
) -> ClonePanelOutput {
    let mut out = ClonePanelOutput {
        refresh_clicked: false,
        selection_changed: false,
    };

    // While a drag is in flight the grab cursor rules the page; the drop
    // targets override it with NotAllowed where the payload can't land.
    if active_drag(ui.ctx()).is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    }

    // ---- Source dropdown ----
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("The disk to clone from.").color(palette.subtle_text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if crate::refresh_disks_button(ui, palette).clicked() {
                out.refresh_clicked = true;
            }
        });
    });
    ui.add_space(6.0);

    let source_disk = source_index.and_then(|s| disks.iter().find(|d| d.index == s));
    let can_drag = source_disk.is_some() && layout.is_some();
    let mut drag_started: Option<u32> = None;
    let picked = disk_dropdown(
        ui,
        "clone_source_dropdown",
        disks,
        *source_index,
        None,
        palette,
        viewport_width,
        |ui, face_width| match source_disk {
            Some(disk) => {
                let mut wants_open = false;
                egui::ScrollArea::horizontal()
                    .id_salt("clone_source_face")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let row_width = face_width.max(min_row_width(disk));
                        let ev =
                            draw_disk_list_row(ui, row_width, disk, true, can_drag, palette);
                        wants_open = ev.clicked;
                        drag_started = ev.drag_started;
                    });
                wants_open
            }
            None => draw_empty_face(ui, face_width, "Choose a source disk…", palette),
        },
    );
    if let Some(idx) = picked {
        if *source_index != Some(idx) {
            *source_index = Some(idx);
            if *target_index == Some(idx) {
                *target_index = None;
            }
            out.selection_changed = true;
        }
    }

    // A drag lifting off the source while the plan is still "entire disk"
    // switches to a partial clone: the editor re-seeds from the target's
    // LIVE layout, and the dragged partition lands wherever it's dropped.
    if let Some(part) = drag_started {
        ui.ctx()
            .data_mut(|d| d.insert_temp(drag_source_id(), part));
        if let Some(layout) = layout.as_deref_mut() {
            if layout.full_disk {
                if let Some(target) =
                    target_index.and_then(|t| disks.iter().find(|d| d.index == t))
                {
                    layout.clear_full_disk(target);
                }
            }
        }
    }

    // ---- Flow arrow ----
    draw_flow_arrow(ui, palette, viewport_width);

    // ---- Target dropdown ----
    let target_disk = target_index.and_then(|t| disks.iter().find(|d| d.index == t));
    ui.horizontal(|ui| {
        // The descriptor only earns its keep while the dropdown is empty;
        // once a disk row is showing, what it is is self-evident.
        if target_disk.is_none() {
            ui.label(egui::RichText::new("The disk to clone onto.").color(palette.subtle_text));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let (Some(layout), Some(target)) = (layout.as_deref_mut(), target_disk) {
                if !out.selection_changed {
                    draw_layout_toolbar(ui, layout, target, palette);
                }
            }
        });
    });
    // Mode switch, shown once both ends are picked, with the explainer for
    // the active mode directly beneath it.
    if let (Some(layout), Some(target)) = (layout.as_deref_mut(), target_disk) {
        if !out.selection_changed {
            let source_disk = source_index.and_then(|s| disks.iter().find(|d| d.index == s));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if mode_button(
                    ui,
                    egui_phosphor::regular::HARD_DRIVE,
                    "Entire disk",
                    layout.full_disk,
                    palette,
                ) && !layout.full_disk
                {
                    if let Some(source) = source_disk {
                        layout.seed_full_disk_clone(source, target, expand);
                    }
                }
                if mode_button(
                    ui,
                    egui_phosphor::regular::PUZZLE_PIECE,
                    "Individual partitions",
                    !layout.full_disk,
                    palette,
                ) && layout.full_disk
                {
                    layout.clear_full_disk(target);
                }
            });
            ui.add_space(2.0);
            let caption = if layout.full_disk {
                "Full-disk clone — the target's entire contents are replaced with the source \
                 layout."
            } else {
                "Drag a source partition onto a target partition to replace it, or into empty \
                 space to add it. Drag edges to resize, the body to move; click to select. \
                 Unmapped partitions are preserved."
            };
            ui.label(egui::RichText::new(caption).color(palette.subtle_text));

            // Delete key mirrors the toolbar's delete button (partial only).
            if !layout.full_disk
                && layout.selected_slot.is_some()
                && ui.ctx().input(|i| i.key_pressed(egui::Key::Delete))
            {
                layout.delete_selected();
            }
        }
    }
    ui.add_space(6.0);

    let show_editor = target_disk.is_some() && layout.is_some() && !out.selection_changed;
    let picked = disk_dropdown(
        ui,
        "clone_target_dropdown",
        disks,
        *target_index,
        *source_index,
        palette,
        viewport_width,
        |ui, face_width| {
            if show_editor {
                let layout = layout.as_deref_mut().expect("checked is_some");
                let target = target_disk.expect("checked is_some");
                let view = layout.target_view(target);
                egui::ScrollArea::horizontal()
                    .id_salt("clone_target_face")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let row_width = face_width.max(min_row_width(&view));
                        let _ = draw_target_row(ui, row_width, 0.0, layout, &view, palette);
                    });
                // The editor owns clicks (slot selection); the chevron is
                // the only way to reopen the list.
                false
            } else if let Some(disk) = target_disk {
                let mut wants_open = false;
                egui::ScrollArea::horizontal()
                    .id_salt("clone_target_face")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let row_width = face_width.max(min_row_width(disk));
                        let ev = draw_disk_list_row(ui, row_width, disk, true, false, palette);
                        wants_open = ev.clicked;
                    });
                wants_open
            } else {
                draw_empty_face(ui, face_width, "Choose a target disk…", palette)
            }
        },
    );
    if let Some(idx) = picked {
        if *target_index != Some(idx) {
            *target_index = Some(idx);
            out.selection_changed = true;
        }
    }

    if let Some(layout) = layout.as_deref() {
        drag_lifecycle(ui.ctx(), layout, palette);
    }

    out
}

/// Bordered mode-switch button with a leading icon; the active mode gets an
/// accent border and tinted fill so the pair reads as a two-state control.
fn mode_button(ui: &mut Ui, icon: &str, label: &str, selected: bool, palette: &Palette) -> bool {
    // Text stays at full contrast in both states — the accent border and
    // tinted fill carry the selection; accent-colored text on the accent
    // tint would be hard to read.
    let text = crate::icon_label(icon, 16.0, label, 14.0, palette.icon_color);
    let fill = if selected {
        disk_map::blend(palette.content_card_bg, palette.accent, 0.18)
    } else {
        palette.content_card_bg
    };
    let stroke = if selected {
        Stroke::new(1.5, palette.accent)
    } else {
        Stroke::new(1.0, disk_map::with_alpha(palette.subtle_text, 90))
    };
    let response = ui.add(egui::Button::new(text).fill(fill).stroke(stroke));
    crate::theme::draw_focus_outline(ui, &response, palette);
    response.clicked()
}

/// Big source→target arrow between the two dropdowns, centered on the
/// visible viewport (not the virtual scroll width). Painted as a filled
/// shaft + head rather than a phosphor glyph: the icon font only ships the
/// outline variant, and this wants to read as one solid accent-colored mark.
fn draw_flow_arrow(ui: &mut Ui, palette: &Palette, viewport_width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(viewport_width, 52.0), Sense::hover());
    let c = rect.center();
    let w = 40.0; // head width
    let h = 42.0; // total height
    let shaft_w = w * 0.45;
    let head_h = h * 0.45;
    let top = c.y - h / 2.0;
    let bottom = c.y + h / 2.0;
    let painter = ui.painter();
    // Shaft overlaps the head by half a pixel so anti-aliasing can't open a
    // hairline seam between the two shapes.
    painter.rect_filled(
        Rect::from_min_max(
            egui::pos2(c.x - shaft_w / 2.0, top),
            egui::pos2(c.x + shaft_w / 2.0, bottom - head_h + 0.5),
        ),
        Rounding {
            nw: 2.0,
            ne: 2.0,
            ..Rounding::ZERO
        },
        palette.accent,
    );
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(c.x - w / 2.0, bottom - head_h),
            egui::pos2(c.x + w / 2.0, bottom - head_h),
            egui::pos2(c.x, bottom),
        ],
        palette.accent,
        Stroke::NONE,
    ));
}
