//! Restore page source/target disk map rows.

use eframe::egui;
use egui::{CursorIcon, Rect, Sense, Ui, Vec2};

use phoenix_core::disk::{DiskInfo, PartitionInfo};

use crate::disk_map::{
    self, draw_disk_info_card, draw_partition_map, draw_partition_segment_visual_styled,
    pixel_to_disk_offset, CHECKBOX_COLUMN_WIDTH, CHECKBOX_GAP, INFO_CARD_GAP,
    INFO_CARD_WIDTH, ROW_HEIGHT, ROW_VERTICAL_GAP,
};
use crate::restore_layout::{DropFit, LayoutDrag, RestoreLayoutState};
use crate::theme::Palette;

const EDGE_HANDLE_PX: f32 = 8.0;
/// Pixel window inside which a resize edge snaps flush to the free space.
const SNAP_PX: f32 = 12.0;
/// Ghost segment that follows the cursor during a source-partition drag.
const GHOST_SIZE: Vec2 = Vec2::new(160.0, 72.0);

fn drag_source_id() -> egui::Id {
    egui::Id::new("restore_drag_source")
}

/// The source partition index of the drag in flight, if any.
fn active_drag(ctx: &egui::Context) -> Option<u32> {
    ctx.data(|d| d.get_temp::<u32>(drag_source_id()))
}

fn clear_drag(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove::<u32>(drag_source_id()));
}

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

    ui.label(egui::RichText::new("Source media").strong());
    ui.add_space(4.0);
    let row_width = viewport_width.max(disk_map::min_disk_row_width(&layout.source_disk));
    draw_source_row(ui, row_width, layout, target, palette);
    ui.add_space(12.0);

    ui.label(egui::RichText::new("Target disk layout").strong());
    ui.label(
        egui::RichText::new(
            "Drag a source partition onto a target partition to replace it. Drag edges to resize, or drag the body to move.",
        )
        .color(palette.subtle_text),
    );
    ui.add_space(4.0);
    // Baseline cursor for a drag in flight; the target row overrides it with
    // NotAllowed when hovering a slot the source can't fit into.
    if active_drag(ui.ctx()).is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    }
    let target_view = if layout.full_disk {
        layout.planned_target_view(target)
    } else {
        crate::restore_layout::synthetic_target_view(target, layout)
    };
    let target_row_width = viewport_width.max(disk_map::min_disk_row_width(&target_view));
    if draw_target_row(ui, target_row_width, layout, target, &target_view, palette) {
        out.plan_entries_updated = true;
    }

    // Drag lifecycle: the ghost follows the pointer while the drag is live;
    // any release ends the drag (the target row's drop handler has already run
    // this frame, so a release over a valid slot mapped before we clear).
    let ctx = ui.ctx().clone();
    if let Some(src_index) = active_drag(&ctx) {
        if ctx.input(|i| i.pointer.any_released() || !i.pointer.any_down()) {
            clear_drag(&ctx);
        } else if let Some(src) = layout
            .source_disk
            .partitions
            .iter()
            .find(|p| p.index == src_index)
        {
            draw_drag_ghost(&ctx, palette, src);
        }
    }

    ui.add_space(ROW_VERTICAL_GAP);
    out
}

/// Semi-transparent miniature of the dragged source partition, pinned to the
/// pointer on the tooltip layer so it rides above both disk rows.
fn draw_drag_ghost(ctx: &egui::Context, palette: &Palette, src: &PartitionInfo) {
    let Some(pos) = ctx.pointer_latest_pos() else {
        return;
    };
    egui::Area::new(egui::Id::new("restore_drag_ghost"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + egui::vec2(14.0, 10.0))
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_opacity(0.85);
            let (rect, _) = ui.allocate_exact_size(GHOST_SIZE, Sense::hover());
            draw_partition_segment_visual_styled(ui, rect, src, palette, false, false, None);
        });
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

    let dragging_source = active_drag(ui.ctx());

    draw_partition_map(
        ui,
        map_rect,
        &disk,
        palette,
        |ui, _disk_idx, p, seg_rect| {
            let id = ui.id().with(("restore_src", p.index));
            let response = ui.interact(seg_rect, id, Sense::click_and_drag());
            if !full_disk {
                if response.drag_started() {
                    ui.ctx()
                        .data_mut(|d| d.insert_temp(drag_source_id(), p.index));
                }
                if response.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::Grab);
                }
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
            response.on_hover_ui_at_pointer(|ui| disk_map::partition_tooltip(ui, p));
            true
        },
    );
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
        egui::pos2(
            row_rect.left() + CHECKBOX_COLUMN_WIDTH + CHECKBOX_GAP,
            row_rect.top(),
        ),
        Vec2::new(INFO_CARD_WIDTH, ROW_HEIGHT),
    );
    draw_disk_info_card(ui, info_rect, target, palette, false);

    let map_rect = Rect::from_min_max(
        egui::pos2(info_rect.right() + INFO_CARD_GAP, row_rect.top()),
        row_rect.right_bottom(),
    );

    let dragging_source = active_drag(ui.ctx());

    draw_partition_map(ui, map_rect, view, palette, |ui, _, p, seg_rect| {
        let part = p.index;
        let id = ui.id().with(("restore_tgt", part));
        let assigned = layout.assignments.contains_key(&part);
        let overlay = layout.assignment_label(part);

        // Live drop feedback while a source drag is in flight: bright accent
        // glow = dropping here replaces this partition; danger glow + refusal
        // cursor = the source can't fit even after shrinking.
        let mut drop_ok: Option<bool> = None;
        if !layout.full_disk {
            if let Some(src) = dragging_source {
                if ui.rect_contains_pointer(seg_rect) {
                    let ok = layout.drop_fit(src, view, part) != DropFit::TooSmall;
                    drop_ok = Some(ok);
                    if !ok {
                        ui.ctx().set_cursor_icon(CursorIcon::NotAllowed);
                    } else if ui.input(|i| i.pointer.primary_released()) {
                        changed = layout.apply_drop(src, view, part);
                        clear_drag(ui.ctx());
                    }
                }
            }
        }

        let fs = layout.target_partition_fs(view, part);
        let resizable = phoenix_restore::plan::partition_allows_resize(fs);

        // The resize snap window (~12 px) expressed in disk bytes.
        let snap_bytes = (SNAP_PX as f64 / map_rect.width().max(1.0) as f64
            * view.size_bytes as f64) as u64;

        // Every assigned partition can be moved by dragging its body.
        let move_rect = if resizable && assigned {
            seg_rect.shrink2(egui::vec2(EDGE_HANDLE_PX, 0.0))
        } else {
            seg_rect
        };
        let move_body = ui.interact(move_rect, id.with("move"), Sense::drag());

        if move_body.drag_started() && assigned {
            if let Some(pos) = move_body.interact_pointer_pos() {
                let off = pixel_to_disk_offset(map_rect, view.size_bytes, pos.x);
                layout.begin_move(part, off);
            }
        }
        if move_body.dragged() && layout.drag.is_some() {
            if let Some(pos) = move_body.interact_pointer_pos() {
                let off = pixel_to_disk_offset(map_rect, view.size_bytes, pos.x);
                layout.update_drag(view, off, snap_bytes);
                changed = true;
            }
        }
        if move_body.drag_stopped() {
            layout.end_drag();
        }

        let mut edge_hovered = false;
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
            edge_hovered = l.hovered() || r.hovered();
            if l.drag_started() {
                layout.begin_resize(part, true);
            }
            if r.drag_started() {
                layout.begin_resize(part, false);
            }
            if l.dragged() || r.dragged() {
                if let Some(pos) = l
                    .interact_pointer_pos()
                    .or_else(|| r.interact_pointer_pos())
                {
                    let off = pixel_to_disk_offset(map_rect, view.size_bytes, pos.x);
                    layout.update_drag(view, off, snap_bytes);
                    changed = true;
                }
            }
            if l.drag_stopped() || r.drag_stopped() {
                layout.end_drag();
            }
        }

        // Cursor + "active" affordance: the partition a click would act on
        // glows; the cursor says what the click would do. During a move or
        // resize the cursor stays pinned even if the pointer outruns the
        // handle strip. A live drop-drag owns the cursor instead.
        let moving_this = matches!(
            &layout.drag,
            Some(LayoutDrag::MoveBody { target_part, .. }) if *target_part == part
        );
        let resizing_this = matches!(
            &layout.drag,
            Some(LayoutDrag::ResizeLeft { target_part })
            | Some(LayoutDrag::ResizeRight { target_part }) if *target_part == part
        );
        let idle = layout.drag.is_none() && dragging_source.is_none();
        if resizing_this || (idle && edge_hovered) {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        } else if moving_this || (idle && assigned && move_body.hovered()) {
            ui.ctx().set_cursor_icon(CursorIcon::Move);
        }
        let active = moving_this
            || resizing_this
            || (idle && assigned && (move_body.hovered() || edge_hovered));

        draw_partition_segment_visual_styled(
            ui,
            seg_rect,
            p,
            palette,
            active,
            assigned,
            overlay.as_deref(),
        );
        if active {
            disk_map::draw_segment_glow(ui, seg_rect, palette.accent);
        }
        match drop_ok {
            Some(true) => disk_map::draw_segment_glow(ui, seg_rect, palette.accent),
            Some(false) => disk_map::draw_segment_glow(ui, seg_rect, palette.danger),
            None => {}
        }
        move_body.on_hover_ui_at_pointer(|ui| disk_map::partition_tooltip(ui, p));
        true
    });

    changed
}
