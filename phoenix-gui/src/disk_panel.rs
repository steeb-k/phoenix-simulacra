//! Disk + partition selection panel for the Backup page.

use std::collections::HashSet;

use eframe::egui;
use egui::Ui;

use phoenix_core::disk::DiskInfo;

use crate::disk_map;
use crate::theme::Palette;

/// Render every disk as a row of `[checkbox | info card | partition map]`.
pub fn show(
    ui: &mut Ui,
    disks: &[DiskInfo],
    selections: &mut HashSet<(u32, u32)>,
    palette: &Palette,
    viewport_width: f32,
) {
    let natural_min = disks
        .iter()
        .map(disk_map::min_disk_row_width)
        .fold(0.0_f32, f32::max);
    let row_width = viewport_width.max(natural_min);

    for disk in disks {
        disk_map::draw_backup_disk_row(ui, row_width, disk, selections, palette);
        ui.add_space(disk_map::ROW_VERTICAL_GAP);
    }
}
