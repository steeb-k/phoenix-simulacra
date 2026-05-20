use std::path::{Path, PathBuf};

use eframe::egui;
use phoenix_capture::backup::{run_backup, BackupOptions};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{enumerate_disks, refine_partition_fs, DiskInfo};
use phoenix_restore::plan::{default_plan_from_backup, RestorePlan, RestorePlanEntry};
use phoenix_restore::restore::{run_restore, verify_backup, RestoreOptions};

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Carbon Phoenix",
        options,
        Box::new(|cc| Ok(Box::new(PhoenixApp::new(cc)))),
    )
}

struct PhoenixApp {
    disks: Vec<DiskInfo>,
    selected_disk: usize,
    selected_partitions: Vec<bool>,
    backup_path: String,
    use_vss: bool,
    restore_backup_path: String,
    restore_plan_entries: Vec<RestorePlanEntry>,
    target_disk_index: u32,
    status: String,
    tab: Tab,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Backup,
    Restore,
    Verify,
}

impl PhoenixApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self {
            disks: Vec::new(),
            selected_disk: 0,
            selected_partitions: Vec::new(),
            backup_path: default_backup_path(),
            use_vss: false,
            restore_backup_path: String::new(),
            restore_plan_entries: Vec::new(),
            target_disk_index: 0,
            status: "Ready".into(),
            tab: Tab::Backup,
        };
        app.refresh_disks();
        app
    }

    fn refresh_disks(&mut self) {
        match enumerate_disks() {
            Ok(mut disks) => {
                for d in &mut disks {
                    for p in &mut d.partitions {
                        refine_partition_fs(p);
                    }
                }
                if !disks.is_empty() {
                    let idx = self.selected_disk.min(disks.len() - 1);
                    self.selected_partitions = vec![false; disks[idx].partitions.len()];
                }
                self.disks = disks;
                self.status = format!("Found {} disk(s)", self.disks.len());
            }
            Err(e) => self.status = format!("Disk enumeration failed: {e}"),
        }
    }
}

impl eframe::App for PhoenixApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.selectable_label(self.tab == Tab::Backup, "Backup").clicked() {
                    self.tab = Tab::Backup;
                }
                if ui.selectable_label(self.tab == Tab::Restore, "Restore").clicked() {
                    self.tab = Tab::Restore;
                }
                if ui.selectable_label(self.tab == Tab::Verify, "Verify").clicked() {
                    self.tab = Tab::Verify;
                }
                ui.separator();
                if ui.button("Refresh disks").clicked() {
                    self.refresh_disks();
                }
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.label(&self.status);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            match self.tab {
                Tab::Backup => self.ui_backup(ui),
                Tab::Restore => self.ui_restore(ui),
                Tab::Verify => self.ui_verify(ui),
            }
        });
    }
}

impl PhoenixApp {
    fn ui_backup(&mut self, ui: &mut egui::Ui) {
        ui.heading("Create backup");
        if self.disks.is_empty() {
            ui.label("No disks found. Run as Administrator.");
            return;
        }

        ui.horizontal(|ui| {
            ui.label("Disk:");
            egui::ComboBox::from_id_salt("disk")
                .selected_text(format!(
                    "PhysicalDrive{}",
                    self.disks.get(self.selected_disk).map(|d| d.index).unwrap_or(0)
                ))
                .show_ui(ui, |ui| {
                    for (i, d) in self.disks.iter().enumerate() {
                        if ui
                            .selectable_value(&mut self.selected_disk, i, &d.path)
                            .clicked()
                        {
                            self.selected_partitions = vec![false; d.partitions.len()];
                        }
                    }
                });
        });

        if let Some(disk) = self.disks.get(self.selected_disk) {
            ui.label("Partitions:");
            for (i, p) in disk.partitions.iter().enumerate() {
                let mut sel = self.selected_partitions.get(i).copied().unwrap_or(false);
                ui.checkbox(&mut sel, format!("[{}] {} ({:?})", p.index, p.name, p.fs_kind));
                if i < self.selected_partitions.len() {
                    self.selected_partitions[i] = sel;
                }
            }
        }

        ui.separator();
        ui.label("Save backup to:");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.backup_path)
                    .desired_width(f32::INFINITY)
                    .hint_text(r"e.g. D:\Backups\machine.phnx"),
            );
            if ui.button("Browse…").clicked() {
                if let Some(path) = pick_backup_save_path(&self.backup_path) {
                    self.backup_path = path.display().to_string();
                }
            }
        });

        ui.checkbox(&mut self.use_vss, "Use VSS (live Windows)");

        if ui.button("Start backup").clicked() {
            let Some(output_path) = pick_backup_save_path(&self.backup_path) else {
                self.status = "Backup cancelled — no save location chosen".into();
                return;
            };
            self.backup_path = output_path.display().to_string();

            let Some(disk) = self.disks.get(self.selected_disk) else {
                return;
            };
            let parts: Vec<u32> = disk
                .partitions
                .iter()
                .enumerate()
                .filter(|(i, _)| self.selected_partitions.get(*i).copied().unwrap_or(false))
                .map(|(_, p)| p.index)
                .collect();

            if parts.is_empty() {
                self.status = "Select at least one partition to back up".into();
                return;
            }

            self.status = format!("Backing up to {}…", self.backup_path);
            ui.ctx().request_repaint();

            match run_backup(BackupOptions {
                disk_index: disk.index,
                partition_indices: parts,
                output: output_path,
                use_vss: self.use_vss,
            }) {
                Ok(()) => self.status = format!("Backup completed: {}", self.backup_path),
                Err(e) => self.status = format!("Backup failed: {e}"),
            }
        }
    }

    fn ui_restore(&mut self, ui: &mut egui::Ui) {
        ui.heading("Restore");
        ui.label("Backup file:");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.restore_backup_path)
                    .desired_width(f32::INFINITY)
                    .hint_text("Path to .phnx file"),
            );
            if ui.button("Browse…").clicked() {
                if let Some(path) = pick_backup_open_path(&self.restore_backup_path) {
                    self.restore_backup_path = path.display().to_string();
                }
            }
        });
        ui.add(egui::DragValue::new(&mut self.target_disk_index).prefix("Target disk "));

        if ui.button("Load backup & plan").clicked() {
            let Some(path) = resolve_backup_open_path(&self.restore_backup_path) else {
                self.status = "Load cancelled — no backup file chosen".into();
                return;
            };
            self.restore_backup_path = path.display().to_string();
            match PhnxReader::open(&path) {
                Ok(reader) => {
                    let disks = enumerate_disks().unwrap_or_default();
                    let size = disks
                        .iter()
                        .find(|d| d.index == self.target_disk_index)
                        .map(|d| d.size_bytes)
                        .unwrap_or(0);
                    let plan = default_plan_from_backup(
                        &self.restore_backup_path,
                        &reader,
                        self.target_disk_index,
                        size,
                    );
                    self.restore_plan_entries = plan.entries;
                    self.status = "Plan loaded — edit sizes below".into();
                }
                Err(e) => self.status = format!("Load failed: {e}"),
            }
        }

        for entry in &mut self.restore_plan_entries {
            ui.group(|ui| {
                ui.checkbox(&mut entry.restore, format!("Partition {}", entry.source_partition_index));
                ui.label(format!("Offset: {} bytes", entry.target_offset_bytes));
                ui.add(
                    egui::DragValue::new(&mut entry.target_size_bytes)
                        .speed(1_000_000)
                        .prefix("Size "),
                );
            });
        }

        if ui.button("Run restore").clicked() {
            let Some(backup_path) = resolve_backup_open_path(&self.restore_backup_path) else {
                self.status = "Restore cancelled — no backup file chosen".into();
                return;
            };
            self.restore_backup_path = backup_path.display().to_string();
            let plan = RestorePlan {
                backup_path: self.restore_backup_path.clone(),
                target_disk_index: self.target_disk_index,
                entries: self.restore_plan_entries.clone(),
            };
            match run_restore(RestoreOptions {
                backup_path,
                plan,
                verify_on_restore: true,
            }) {
                Ok(()) => self.status = "Restore completed".into(),
                Err(e) => self.status = format!("Restore failed: {e}"),
            }
        }
    }

    fn ui_verify(&mut self, ui: &mut egui::Ui) {
        ui.heading("Verify backup");
        ui.label("Backup file:");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.restore_backup_path)
                    .desired_width(f32::INFINITY)
                    .hint_text("Path to .phnx file"),
            );
            if ui.button("Browse…").clicked() {
                if let Some(path) = pick_backup_open_path(&self.restore_backup_path) {
                    self.restore_backup_path = path.display().to_string();
                }
            }
        });
        if ui.button("Quick verify").clicked() {
            let path = match resolve_backup_open_path(&self.restore_backup_path) {
                Some(p) => p,
                None => {
                    self.status = "Verify cancelled — no backup file chosen".into();
                    return;
                }
            };
            self.restore_backup_path = path.display().to_string();
            match verify_backup(path.as_path(), true) {
                Ok(()) => self.status = "Quick verify OK".into(),
                Err(e) => self.status = format!("Verify failed: {e}"),
            }
        }
        if ui.button("Full verify").clicked() {
            let path = match resolve_backup_open_path(&self.restore_backup_path) {
                Some(p) => p,
                None => {
                    self.status = "Verify cancelled — no backup file chosen".into();
                    return;
                }
            };
            self.restore_backup_path = path.display().to_string();
            match verify_backup(path.as_path(), false) {
                Ok(()) => self.status = "Full verify OK".into(),
                Err(e) => self.status = format!("Verify failed: {e}"),
            }
        }
    }
}

fn default_backup_path() -> String {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return format!(r"{profile}\Documents\carbon-phoenix-backup.phnx");
    }
    "backup.phnx".into()
}

fn pick_backup_save_path(current: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new()
        .add_filter("Carbon Phoenix backup", &["phnx"])
        .set_title("Save backup as");

    let path = Path::new(current);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        dialog = dialog.set_directory(parent);
    } else if let Ok(profile) = std::env::var("USERPROFILE") {
        dialog = dialog.set_directory(format!(r"{profile}\Documents"));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("carbon-phoenix-backup.phnx");
    dialog.set_file_name(name).save_file()
}

fn pick_backup_open_path(current: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new()
        .add_filter("Carbon Phoenix backup", &["phnx"])
        .set_title("Open backup");

    let path = Path::new(current);
    if path.is_file() {
        if let Some(parent) = path.parent() {
            dialog = dialog.set_directory(parent);
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            dialog = dialog.set_file_name(name);
        }
    } else if let Some(parent) = path.parent().filter(|p| p.exists()) {
        dialog = dialog.set_directory(parent);
    } else if let Ok(profile) = std::env::var("USERPROFILE") {
        dialog = dialog.set_directory(format!(r"{profile}\Documents"));
    }

    dialog.pick_file()
}

/// Use typed path if it exists; otherwise prompt for a file.
fn resolve_backup_open_path(current: &str) -> Option<PathBuf> {
    let path = PathBuf::from(current.trim());
    if path.is_file() {
        return Some(path);
    }
    pick_backup_open_path(current)
}
