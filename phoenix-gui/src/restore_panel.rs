//! Restore page source/target disk map rows.

use eframe::egui;
use egui::{Rect, Sense, Ui, Vec2};

use phoenix_core::disk::DiskInfo;

use crate::disk_map::{
    self, draw_disk_info_card, draw_partition_map, draw_partition_segment_visual_styled,
    pixel_to_disk_offset, row_stride, CHECKBOX_COLUMN_WIDTH, CHECKBOX_GAP, INFO_CARD_GAP,
    INFO_CARD_WIDTH, ROW_HEIGHT, ROW_VERTICAL_GAP,
};
use crate::restore_layout::RestoreLayoutState;
use crate::theme::Palette;

const EDGE_HANDLE_PX: f32 = 6.0;

pub struct RestorePanelOutput {
    pub plan_entries_updated: bool,
}

pub fn show(
    ui: &mut Ui,
    layout: &mut RestoreLayoutState,
    target: &DiskInfo,
    palette: &Palette,
    viewport_width: f32,
) -> RestorePanelOutput {
    let mut out = RestorePanelOutput {
        plan_entries_updated: false,
    };

    if let Some(msg) = layout.dialog_message.take() {
        egui::Window::new("Restore")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label(&msg);
                if ui.button("OK").clicked() {
                    ui.close_menu();
                }
            });
    }

    ui.label(egui::RichText::new("Source media").strong());
    ui.add_space(4.0);
    let row_width = viewport_width.max(disk_map::min_disk_row_width(&layout.source_disk));
    draw_source_row(ui, row_width, layout, target, palette);
    ui.add_space(12.0);

    ui.label(egui::RichText::new("Target disk layout").strong());
    ui.label(
        egui::RichText::new(
            "Drag a source partition onto a target partition to map it. Drag edges to resize, or drag the body to move.",
        )
        .color(palette.subtle_text),
    );
    ui.add_space(4.0);
    let target_view = crate::restore_layout::synthetic_target_view(target, layout);
    let target_row_width = viewport_width.max(disk_map::min_disk_row_width(&target_view));
    if draw_target_row(ui, target_row_width, layout, target, &target_view, palette) {
        out.plan_entries_updated = true;
    }

    ui.add_space(ROW_VERTICAL_GAP);
    out
}

fn draw_source_row(
    ui: &mut Ui,
    row_width: f32,
    layout: &mut RestoreLayoutState,
    target: &DiskInfo,
    palette: &Palette,
) {
    let full_disk = layout.full_disk;
    let disk = layout.source_disk.clone();
    let row_size = Vec2::new(row_width, ROW_HEIGHT);
    let (row_rect, _) = ui.allocate_exact_size(row_size, Sense::hover());

    let checkbox_rect = Rect::from_min_size(
        row_rect.left_top(),
        Vec2::new(CHECKBOX_COLUMN_WIDTH, ROW_HEIGHT),
    );
    draw_full_disk_checkbox(ui, checkbox_rect, layout, target, palette);

    let info_rect = Rect::from_min_size(
        egui::pos2(checkbox_rect.right() + CHECKBOX_GAP, row_rect.top()),
        Vec2::new(INFO_CARD_WIDTH, ROW_HEIGHT),
    );
    draw_disk_info_card(ui, info_rect, &disk, palette, full_disk);

    let map_rect = Rect::from_min_max(
        egui::pos2(info_rect.right() + INFO_CARD_GAP, row_rect.top()),
        row_rect.right_bottom(),
    );

    let dragging_source = ui.ctx().data_mut(|d| {
        d.get_temp::<u32>(egui::Id::new("restore_drag_source"))
    });

    draw_partition_map(ui, map_rect, &disk, palette, |ui, _disk_idx, p, seg_rect| {
        let id = ui.id().with(("restore_src", p.index));
        let response = ui.interact(seg_rect, id, Sense::click_and_drag());
        if response.drag_started() {
            ui.ctx().data_mut(|d| {
                d.insert_temp(egui::Id::new("restore_drag_source"), p.index);
            });
        }
        let selected = dragging_source == Some(p.index);
        draw_partition_segment_visual_styled(
            ui,
            seg_rect,
            p,
            palette,
            response.hovered(),
            selected,
            None,
        );
        true
    });
}

fn draw_full_disk_checkbox(
    ui: &mut Ui,
    rect: Rect,
    layout: &mut RestoreLayoutState,
    target: &DiskInfo,
    palette: &Palette,
) {
    let id = ui.id().with("restore_full_disk");
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter_at(rect);
    let box_size = 22.0;
    let box_rect = Rect::from_center_size(rect.center(), Vec2::splat(box_size));
    if layout.full_disk {
        painter.rect_filled(box_rect, egui::Rounding::same(4.0), palette.accent);
        painter.text(
            box_rect.center(),
            egui::Align2::CENTER_CENTER,
            egui_phosphor::regular::CHECK,
            crate::fonts::icon(16.0),
            egui::Color32::WHITE,
        );
    } else {
        let stroke = if response.hovered() {
            palette.accent
        } else {
            palette.subtle_text
        };
        painter.rect_stroke(
            box_rect,
            egui::Rounding::same(4.0),
            egui::Stroke::new(1.5, stroke),
        );
    }
    if response.clicked() {
        if layout.full_disk {
            layout.clear_full_disk();
        } else {
            layout.apply_full_disk(target);
        }
    }
}

fn draw_target_row(
    ui: &mut Ui,
    row_width: f32,
    layout: &mut RestoreLayoutState,
    target: &DiskInfo,
    view: &DiskInfo,
    palette: &Palette,
) -> bool {
    let mut changed = false;
    let row_size = Vec2::new(row_width, ROW_HEIGHT);
    let (row_rect, _) = ui.allocate_exact_size(row_size, Sense::hover());

    let checkbox_rect = Rect::from_min_size(
        row_rect.left_top(),
        Vec2::new(CHECKBOX_COLUMN_WIDTH, ROW_HEIGHT),
    );
    let _ = checkbox_rect;

    let info_rect = Rect::from_min_size(
        egui::pos2(row_rect.left() + CHECKBOX_COLUMN_WIDTH + CHECKBOX_GAP, row_rect.top()),
        Vec2::new(INFO_CARD_WIDTH, ROW_HEIGHT),
    );
    draw_disk_info_card(ui, info_rect, target, palette, false);

    let map_rect = Rect::from_min_max(
        egui::pos2(info_rect.right() + INFO_CARD_GAP, row_rect.top()),
        row_rect.right_bottom(),
    );

    let dragging_source = ui.ctx().data_mut(|d| {
        d.get_temp::<u32>(egui::Id::new("restore_drag_source"))
    });

    if layout.full_disk {
        draw_partition_map(ui, map_rect, view, palette, |ui, _, p, seg_rect| {
            let overlay = layout.assignment_label(p.index, target);
            draw_partition_segment_visual_styled(
                ui,
                seg_rect,
                p,
                palette,
                false,
                layout.assignments.contains_key(&p.index),
                overlay.as_deref(),
            );
            true
        });
        return changed;
    }

    draw_partition_map(ui, map_rect, view, palette, |ui, _, p, seg_rect| {
        let part = p.index;
        let id = ui.id().with(("restore_tgt", part));
        let assigned = layout.assignments.contains_key(&part);
        let overlay = layout.assignment_label(part, target);

        if let Some(src) = dragging_source {
            let drop = ui.interact(seg_rect, id.with("drop"), Sense::click());
            if drop.hovered() && ui.input(|i| i.pointer.any_released()) {
                layout.try_map_source_to_target(src, target, part);
                ui.ctx().data_mut(|d| {
                    d.remove::<u32>(egui::Id::new("restore_drag_source"));
                });
                changed = true;
            }
        }

        let fs = layout.target_partition_fs(target, part);
        let resizable = phoenix_restore::plan::partition_allows_resize(fs);
        let body_rect = if resizable {
            seg_rect.shrink2(egui::vec2(EDGE_HANDLE_PX, 0.0))
        } else {
            seg_rect
        };
        let body = ui.interact(body_rect, id.with("body"), Sense::click_and_drag());

        if body.drag_started() && assigned {
            let off = pixel_to_disk_offset(map_rect, target.size_bytes, body.interact_pointer_pos().unwrap().x);
            layout.begin_move(part, off);
        }
        if body.dragged() && layout.drag.is_some() {
            if let Some(pos) = body.interact_pointer_pos() {
                let off = pixel_to_disk_offset(map_rect, target.size_bytes, pos.x);
                layout.update_drag(target, off);
                changed = true;
            }
        }
        if body.drag_stopped() {
            layout.end_drag();
        }

        if resizable && assigned {
            let left = Rect::from_min_max(
                seg_rect.left_top(),
                egui::pos2(seg_rect.left() + EDGE_HANDLE_PX, seg_rect.bottom()),
            );
            let right = Rect::from_min_max(
                egui::pos2(seg_rect.right() - EDGE_HANDLE_PX, seg_rect.top()),
                seg_rect.right_bottom(),
            );
            let l = ui.interact(left, id.with("l"), Sense::drag());
            let r = ui.interact(right, id.with("r"), Sense::drag());
            if l.drag_started() {
                layout.begin_resize(part, true);
            }
            if r.drag_started() {
                layout.begin_resize(part, false);
            }
            if l.dragged() || r.dragged() {
                if let Some(pos) = l.interact_pointer_pos().or_else(|| r.interact_pointer_pos()) {
                    let off = pixel_to_disk_offset(map_rect, target.size_bytes, pos.x);
                    layout.update_drag(target, off);
                    changed = true;
                }
            }
            if l.drag_stopped() || r.drag_stopped() {
                layout.end_drag();
            }
        }

        draw_partition_segment_visual_styled(
            ui,
            seg_rect,
            p,
            palette,
            body.hovered(),
            assigned,
            overlay.as_deref(),
        );
        true
    });

    changed
}

pub fn row_stride_with_gap() -> f32 {
    row_stride()
}
