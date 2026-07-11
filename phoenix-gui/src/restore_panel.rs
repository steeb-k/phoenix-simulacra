//! Restore page source/target disk map rows and the layout-editing toolbar.

use eframe::egui;
use egui::{Align2, CursorIcon, Rect, Rounding, Sense, Ui, Vec2};

use phoenix_core::disk::{DiskInfo, PartitionInfo};

use std::sync::Arc;

use crate::disk_map::{
    self, draw_disk_info_card, draw_partition_map, draw_partition_map_segments,
    draw_partition_segment_visual_styled, MapScale, SegmentKind, CHECKBOX_COLUMN_WIDTH,
    CHECKBOX_GAP, INFO_CARD_GAP, INFO_CARD_WIDTH, ROW_HEIGHT, ROW_VERTICAL_GAP,
};
use crate::fonts;
use crate::restore_layout::{DropFit, LayoutDrag, RestoreLayoutState};
use crate::theme::Palette;

const EDGE_HANDLE_PX: f32 = 8.0;
/// Pixel window inside which a resize edge snaps flush to the free space.
const SNAP_PX: f32 = 12.0;
/// Ghost segment that follows the cursor during a source-partition drag.
const GHOST_SIZE: Vec2 = Vec2::new(160.0, 72.0);
const TOOLBAR_BTN: f32 = 30.0;

fn drag_source_id() -> egui::Id {
    egui::Id::new("restore_drag_source")
}

/// Pixel↔byte mapping frozen at the start of a move/resize gesture. Using
/// the live per-frame mapping would let the layout shift under the cursor
/// mid-drag (segments re-flow as the slice moves) and feed back into the
/// conversion; freezing keeps one monotone cursor→byte function per gesture.
fn gesture_scale_id() -> egui::Id {
    egui::Id::new("restore_gesture_scale")
}

fn freeze_gesture_scale(ctx: &egui::Context, scale: &Arc<MapScale>) {
    ctx.data_mut(|d| d.insert_temp(gesture_scale_id(), scale.clone()));
}

fn gesture_scale(ctx: &egui::Context, live: &Arc<MapScale>) -> Arc<MapScale> {
    ctx.data(|d| d.get_temp::<Arc<MapScale>>(gesture_scale_id()))
        .unwrap_or_else(|| live.clone())
}

fn clear_gesture_scale(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove::<Arc<MapScale>>(gesture_scale_id()));
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
    layout.ensure_target(target);

    ui.label(egui::RichText::new("Source media").strong());
    ui.add_space(4.0);
    let row_width = viewport_width.max(disk_map::min_disk_row_width(&layout.source_disk));
    draw_source_row(ui, row_width, layout, target, palette);
    ui.add_space(12.0);

    // Heading row with the layout toolbar on the right.
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Target disk layout").strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if draw_layout_toolbar(ui, layout, target, palette) {
                out.plan_entries_updated = true;
            }
        });
    });
    ui.label(
        egui::RichText::new(
            "Drag a source partition onto a target partition to replace it, or into empty space to add it. Drag edges to resize, the body to move; click to select.",
        )
        .color(palette.subtle_text),
    );
    ui.add_space(4.0);

    // Baseline cursor for a drag in flight; the target row overrides it with
    // NotAllowed when hovering somewhere the source can't fit.
    if active_drag(ui.ctx()).is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    }
    // Delete key mirrors the toolbar's delete button.
    if !layout.full_disk
        && layout.selected_slot.is_some()
        && ui.ctx().input(|i| i.key_pressed(egui::Key::Delete))
        && layout.delete_selected()
    {
        out.plan_entries_updated = true;
    }

    let target_view = layout.target_view(target);
    let target_row_width = viewport_width.max(disk_map::min_disk_row_width(&target_view));
    if draw_target_row(ui, target_row_width, layout, &target_view, palette) {
        out.plan_entries_updated = true;
    }

    // Drag lifecycle: the ghost follows the pointer while the drag is live;
    // any release ends the drag (the target row's drop handler has already run
    // this frame, so a release over a valid spot mapped before we clear).
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

/// The row of layout-editing actions: table style switch, blank layout,
/// delete selected, reset. Laid out right-to-left (callers wrap us in a
/// right-to-left layout), so buttons are added in reverse visual order.
/// Returns true when the planned layout changed.
fn draw_layout_toolbar(
    ui: &mut Ui,
    layout: &mut RestoreLayoutState,
    target: &DiskInfo,
    palette: &Palette,
) -> bool {
    let mut changed = false;
    let editable = !layout.full_disk;

    if toolbar_button(
        ui,
        palette,
        egui_phosphor::regular::ARROW_U_UP_LEFT,
        "Reset the layout to the disk's current partitions",
        editable,
    ) {
        layout.rebuild_from_target(target);
        changed = true;
    }
    if toolbar_button(
        ui,
        palette,
        egui_phosphor::regular::TRASH,
        "Delete the selected partition from the layout",
        editable && layout.selected_slot.is_some(),
    ) && layout.delete_selected()
    {
        changed = true;
    }
    if toolbar_button(
        ui,
        palette,
        egui_phosphor::regular::ERASER,
        "Blank layout — start empty (re-initializes the partition table on restore)",
        editable,
    ) {
        layout.blank_layout();
        changed = true;
    }
    let style_tip = if layout.table_gpt {
        "Switch the planned partition table to MBR (clears the layout)"
    } else {
        "Switch the planned partition table to GPT (clears the layout)"
    };
    if toolbar_button(
        ui,
        palette,
        egui_phosphor::regular::ARROWS_LEFT_RIGHT,
        style_tip,
        editable,
    ) {
        let gpt = !layout.table_gpt;
        layout.set_table_style(gpt);
        changed = true;
    }
    ui.label(
        egui::RichText::new(if layout.table_gpt { "GPT" } else { "MBR" })
            .font(fonts::bold(12.0))
            .color(palette.subtle_text),
    );
    changed
}

/// Icon button in the style of the disk-list refresh button; dimmed and inert
/// when disabled. Returns true on click.
fn toolbar_button(ui: &mut Ui, palette: &Palette, icon: &str, tip: &str, enabled: bool) -> bool {
    let size = Vec2::splat(TOOLBAR_BTN);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);

    let hovered = enabled && response.hovered();
    let painter = ui.painter_at(rect);
    if hovered {
        painter.rect_filled(rect, Rounding::same(8.0), palette.sidebar_hover_bg);
    }
    let color = if !enabled {
        disk_map::with_alpha(palette.subtle_text, 110)
    } else if hovered {
        palette.accent
    } else {
        palette.icon_color
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        fonts::icon(18.0),
        color,
    );
    let response = response.on_hover_text(tip);
    crate::theme::draw_focus_outline(ui, &response, palette);
    enabled && response.clicked()
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
            layout.clear_full_disk(target);
        } else {
            layout.apply_full_disk(target);
        }
    }
}

fn draw_target_row(
    ui: &mut Ui,
    row_width: f32,
    layout: &mut RestoreLayoutState,
    view: &DiskInfo,
    palette: &Palette,
) -> bool {
    let mut changed = false;
    let row_size = Vec2::new(row_width, ROW_HEIGHT);
    let (row_rect, _) = ui.allocate_exact_size(row_size, Sense::hover());

    let info_rect = Rect::from_min_size(
        egui::pos2(
            row_rect.left() + CHECKBOX_COLUMN_WIDTH + CHECKBOX_GAP,
            row_rect.top(),
        ),
        Vec2::new(INFO_CARD_WIDTH, ROW_HEIGHT),
    );
    draw_disk_info_card(ui, info_rect, view, palette, false);

    let map_rect = Rect::from_min_max(
        egui::pos2(info_rect.right() + INFO_CARD_GAP, row_rect.top()),
        row_rect.right_bottom(),
    );

    let dragging_source = active_drag(ui.ctx());
    // Pixel↔byte mapping for THIS frame's rendered layout; gestures freeze a
    // copy at drag start so cursor motion stays 1:1 with the slice.
    let live_scale = Arc::new(disk_map::compute_map_scale(map_rect, view));

    draw_partition_map_segments(ui, map_rect, view, palette, |ui, seg, seg_rect| {
        let p = match seg {
            SegmentKind::Partition(p) => *p,
            SegmentKind::Unallocated { offset, length } => {
                // Unallocated space accepts drops as a NEW partition.
                let (off, len) = (*offset, *length);
                disk_map::draw_unallocated_segment(ui, seg_rect, len, palette);
                if !layout.full_disk {
                    if let Some(src) = dragging_source {
                        if ui.rect_contains_pointer(seg_rect) {
                            let ok = layout.gap_drop_fit(src, off, len) != DropFit::TooSmall;
                            if ok {
                                disk_map::draw_segment_glow(ui, seg_rect, palette.accent);
                                if ui.input(|i| i.pointer.primary_released()) {
                                    changed |= layout.apply_gap_drop(src, off, len);
                                    clear_drag(ui.ctx());
                                }
                            } else {
                                ui.ctx().set_cursor_icon(CursorIcon::NotAllowed);
                                disk_map::draw_segment_glow(ui, seg_rect, palette.danger);
                            }
                        }
                    }
                }
                return true;
            }
        };

        let part = p.index; // slot id
        let id = ui.id().with(("restore_tgt", part));
        let assigned = layout
            .slot(part)
            .map(|s| s.source.is_some())
            .unwrap_or(false);
        let overlay = layout.assignment_label(part);

        // Live drop feedback while a source drag is in flight: bright accent
        // glow = dropping here replaces this partition; danger glow + refusal
        // cursor = the source can't fit even after shrinking.
        let mut drop_ok: Option<bool> = None;
        if !layout.full_disk {
            if let Some(src) = dragging_source {
                if ui.rect_contains_pointer(seg_rect) {
                    let ok = layout.drop_fit(src, part) != DropFit::TooSmall;
                    drop_ok = Some(ok);
                    if !ok {
                        ui.ctx().set_cursor_icon(CursorIcon::NotAllowed);
                    } else if ui.input(|i| i.pointer.primary_released()) {
                        changed = layout.apply_drop(src, part);
                        clear_drag(ui.ctx());
                    }
                }
            }
        }

        let resizable = layout.slot_resizable(part);

        // Every assigned partition can be moved by dragging its body; a click
        // selects the slot for the toolbar.
        let move_rect = if resizable && assigned {
            seg_rect.shrink2(egui::vec2(EDGE_HANDLE_PX, 0.0))
        } else {
            seg_rect
        };
        let move_body = ui.interact(move_rect, id.with("move"), Sense::click_and_drag());

        if !layout.full_disk && move_body.clicked() {
            layout.selected_slot = if layout.selected_slot == Some(part) {
                None
            } else {
                Some(part)
            };
        }

        if move_body.drag_started() && assigned {
            if let Some(pos) = move_body.interact_pointer_pos() {
                freeze_gesture_scale(ui.ctx(), &live_scale);
                let off = live_scale.x_to_offset(pos.x);
                layout.begin_move(part, off);
            }
        }
        if move_body.dragged() && layout.drag.is_some() {
            if let Some(pos) = move_body.interact_pointer_pos() {
                let scale = gesture_scale(ui.ctx(), &live_scale);
                let off = scale.x_to_offset(pos.x);
                let snap = (SNAP_PX as f64 * scale.bytes_per_px_at(pos.x)) as u64;
                layout.update_drag(off, snap);
                changed = true;
            }
        }
        if move_body.drag_stopped() {
            layout.end_drag();
            clear_gesture_scale(ui.ctx());
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
                freeze_gesture_scale(ui.ctx(), &live_scale);
                layout.begin_resize(part, true);
            }
            if r.drag_started() {
                freeze_gesture_scale(ui.ctx(), &live_scale);
                layout.begin_resize(part, false);
            }
            if l.dragged() || r.dragged() {
                if let Some(pos) = l
                    .interact_pointer_pos()
                    .or_else(|| r.interact_pointer_pos())
                {
                    let scale = gesture_scale(ui.ctx(), &live_scale);
                    let off = scale.x_to_offset(pos.x);
                    let snap = (SNAP_PX as f64 * scale.bytes_per_px_at(pos.x)) as u64;
                    layout.update_drag(off, snap);
                    changed = true;
                }
            }
            if l.drag_stopped() || r.drag_stopped() {
                layout.end_drag();
                clear_gesture_scale(ui.ctx());
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
        let selected_now = layout.selected_slot == Some(part);

        draw_partition_segment_visual_styled(
            ui,
            seg_rect,
            p,
            palette,
            active,
            assigned,
            overlay.as_deref(),
        );
        if active || selected_now {
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
