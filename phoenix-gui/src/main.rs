mod job;
mod sidebar;
mod theme;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;
use phoenix_capture::backup::BackupOptions;
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{enumerate_disks, refine_partition_fs, DiskInfo};
use phoenix_core::ProgressHandle;
use phoenix_restore::plan::{default_plan_from_backup, RestorePlan, RestorePlanEntry};
use phoenix_restore::restore::RestoreOptions;

use crate::job::{spawn_backup, spawn_restore, spawn_verify, BackgroundJob};
use crate::sidebar::Page;
use crate::theme::Palette;

const THEME_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]),
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
    page: Page,
    job: Option<BackgroundJob>,
    palette: Palette,
    last_theme_refresh: Instant,
}

impl PhoenixApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
        cc.egui_ctx.set_fonts(fonts);

        let palette = theme::refresh(&cc.egui_ctx);

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
            page: Page::Backup,
            job: None,
            palette,
            last_theme_refresh: Instant::now(),
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

    fn busy(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.is_running())
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else {
            return;
        };
        if let Some(result) = job.poll() {
            self.job = None;
            self.status = match result {
                Ok(msg) => msg,
                Err(e) => format!("Error: {e}"),
            };
        } else {
            ctx.request_repaint();
        }
    }

    fn maybe_refresh_theme(&mut self, ctx: &egui::Context) {
        if self.last_theme_refresh.elapsed() >= THEME_REFRESH_INTERVAL {
            self.palette = theme::refresh(ctx);
            self.last_theme_refresh = Instant::now();
        }
    }
}

impl eframe::App for PhoenixApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_job(ctx);
        self.maybe_refresh_theme(ctx);

        let busy = self.busy();
        sidebar::show(ctx, &mut self.page, &self.palette, busy);

        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::none()
                    .fill(self.palette.sidebar_bg)
                    .inner_margin(egui::Margin::symmetric(16.0, 8.0)),
            )
            .show(ctx, |ui| {
                if let Some(job) = &self.job {
                    show_progress(ui, &job.progress);
                }
                ui.label(egui::RichText::new(&self.status).color(self.palette.subtle_text));
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::Margin::symmetric(24.0, 20.0)))
            .show(ctx, |ui| {
                ui.add_enabled_ui(!busy, |ui| match self.page {
                    Page::Backup => self.ui_backup(ui),
                    Page::Clone => self.ui_clone(ui),
                    Page::Restore => self.ui_restore(ui),
                    Page::Verify => self.ui_verify(ui),
                    Page::Mount => self.ui_mount(ui),
                    Page::History => self.ui_history(ui),
                    Page::Options => self.ui_options(ui),
                });
                if busy {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label("Operation in progress…");
                }
            });
    }
}

fn page_header(ui: &mut egui::Ui, palette: &Palette, title: &str, subtitle: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(title).strong().size(22.0));
    ui.label(egui::RichText::new(subtitle).color(palette.subtle_text));
    ui.add_space(14.0);
}

fn coming_soon(ui: &mut egui::Ui, palette: &Palette, blurb: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(egui_phosphor::regular::SPARKLE)
                .size(56.0)
                .color(palette.accent),
        );
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Coming soon").strong().size(18.0));
        ui.add_space(4.0);
        ui.label(egui::RichText::new(blurb).color(palette.subtle_text));
    });
}

fn show_progress(ui: &mut egui::Ui, progress: &ProgressHandle) {
    let snap = progress.snapshot();
    if !snap.active {
        return;
    }
    ui.label(&snap.phase);
    if !snap.detail.is_empty() {
        ui.label(&snap.detail);
    }
    if snap.total > 0 {
        ui.add(
            egui::ProgressBar::new(snap.fraction())
                .text(format!(
                    "{} / {} ({:.1}%)",
                    format_bytes(snap.current),
                    format_bytes(snap.total),
                    snap.fraction() * 100.0
                ))
                .animate(true),
        );
    } else {
        ui.add(egui::Spinner::new());
    }
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.2} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.2} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.2} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

impl PhoenixApp {
    fn ui_backup(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "Create Backup",
            "Create a disk or partition image backup with optional compression and verification.",
        );

        if self.disks.is_empty() {
            ui.label("No disks found. Run as Administrator.");
            if ui.button("Refresh disks").clicked() {
                self.refresh_disks();
            }
            return;
        }

        ui.horizontal(|ui| {
            ui.label("Disk:");
            egui::ComboBox::from_id_salt("disk")
                .selected_text(format!(
                    "PhysicalDrive{}",
                    self.disks
                        .get(self.selected_disk)
                        .map(|d| d.index)
                        .unwrap_or(0)
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
            if ui.button("Refresh").clicked() {
                self.refresh_disks();
            }
        });

        if let Some(disk) = self.disks.get(self.selected_disk) {
            ui.label("Partitions:");
            for (i, p) in disk.partitions.iter().enumerate() {
                let mut sel = self.selected_partitions.get(i).copied().unwrap_or(false);
                ui.checkbox(
                    &mut sel,
                    format!("[{}] {} ({:?})", p.index, p.name, p.fs_kind),
                );
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

            let progress = ProgressHandle::new();
            self.status = format!("Backing up to {}…", self.backup_path);
            self.job = Some(spawn_backup(BackupOptions {
                disk_index: disk.index,
                partition_indices: parts,
                output: output_path,
                use_vss: self.use_vss,
                progress: Some(progress),
            }));
        }
    }

    fn ui_restore(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "Restore",
            "Apply a .phnx backup back onto a target disk.",
        );
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
                ui.checkbox(
                    &mut entry.restore,
                    format!("Partition {}", entry.source_partition_index),
                );
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
            let progress = ProgressHandle::new();
            self.status = "Restore in progress…".into();
            self.job = Some(spawn_restore(RestoreOptions {
                backup_path,
                plan,
                verify_on_restore: true,
                progress: Some(progress),
            }));
        }
    }

    fn ui_verify(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "Verify Backup",
            "Run a quick header check or a full BLAKE3 verification across the archive.",
        );
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
            let Some(path) = resolve_backup_open_path(&self.restore_backup_path) else {
                self.status = "Verify cancelled — no backup file chosen".into();
                return;
            };
            self.restore_backup_path = path.display().to_string();
            self.status = "Quick verify in progress…".into();
            self.job = Some(spawn_verify(path, true));
        }
        if ui.button("Full verify").clicked() {
            let Some(path) = resolve_backup_open_path(&self.restore_backup_path) else {
                self.status = "Verify cancelled — no backup file chosen".into();
                return;
            };
            self.restore_backup_path = path.display().to_string();
            self.status = "Full verify in progress…".into();
            self.job = Some(spawn_verify(path, false));
        }
    }

    fn ui_clone(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "Clone",
            "Copy one disk directly to another without going through a backup file.",
        );
        let palette = self.palette;
        coming_soon(
            ui,
            &palette,
            "Disk-to-disk cloning will land in a future release.",
        );
    }

    fn ui_mount(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "Mount",
            "Browse partitions inside a .phnx backup as if they were drives.",
        );
        let palette = self.palette;
        coming_soon(
            ui,
            &palette,
            "Backup mounting is on the roadmap — files inside backups will be browsable soon.",
        );
    }

    fn ui_history(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "History",
            "Previous backups, restores, and verification runs from this machine.",
        );
        let palette = self.palette;
        coming_soon(
            ui,
            &palette,
            "Job history will be tracked once persistent settings land.",
        );
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "Options",
            "Application preferences and live theme info.",
        );

        ui.horizontal(|ui| {
            ui.label("Accent color:");
            let (r, g, b, _) = self.palette.accent.to_tuple();
            let swatch_size = egui::vec2(20.0, 20.0);
            let (rect, _) = ui.allocate_exact_size(swatch_size, egui::Sense::hover());
            ui.painter().rect_filled(
                rect,
                egui::Rounding::same(4.0),
                self.palette.accent,
            );
            ui.monospace(format!("#{:02X}{:02X}{:02X}", r, g, b));
        });

        ui.horizontal(|ui| {
            ui.label("Theme:");
            ui.label(if self.palette.light_mode { "Light" } else { "Dark" });
        });

        ui.add_space(8.0);
        if ui.button("Refresh theme from Windows").clicked() {
            self.palette = theme::refresh(ui.ctx());
            self.last_theme_refresh = Instant::now();
            self.status = "Theme refreshed from Windows settings".into();
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

fn resolve_backup_open_path(current: &str) -> Option<PathBuf> {
    let path = PathBuf::from(current.trim());
    if path.is_file() {
        return Some(path);
    }
    pick_backup_open_path(current)
}
