//! The Virtualize page: boot a `.phnx` backup as a QEMU VM.
//!
//! First cut — functional, deliberately plain; visuals to be tweaked. The heavy
//! lifting (serve helper, qcow2 overlay, sessions, QMP) lives in `phoenix-vm`;
//! this is the picker + knobs + sessions list, plus the QEMU-missing fallback
//! that mirrors the Mount page's WinFsp notice.

use egui::Ui;
use phoenix_vm::drives::{list_drives, DriveInfo};
use phoenix_vm::session::SessionMeta;

use crate::fonts;
use crate::theme::Palette;

/// Where a session's growing overlay should live.
#[derive(Clone, PartialEq, Eq)]
pub enum ScratchChoice {
    /// The same volume as the `.phnx` (the engine default).
    SameAsImage,
    /// A specific drive letter the user picked.
    Drive(char),
}

/// Persisted-across-frames state for the page's controls.
pub struct VmPageState {
    pub backup_path: String,
    /// Path whose manifest summary is currently loaded into `summary`.
    pub loaded_path: String,
    pub summary: Option<BackupSummary>,
    pub mem_mib: u64,
    pub cpus: u32,
    pub network: bool,
    pub scratch: ScratchChoice,
    pub iso_path: String,
    pub boot_iso: bool,
}

impl Default for VmPageState {
    fn default() -> Self {
        VmPageState {
            backup_path: String::new(),
            loaded_path: String::new(),
            summary: None,
            mem_mib: 6144,
            cpus: 4,
            network: true,
            scratch: ScratchChoice::SameAsImage,
            iso_path: String::new(),
            boot_iso: false,
        }
    }
}

/// A one-line read of a backup's manifest, shown after it is picked.
#[derive(Clone)]
pub struct BackupSummary {
    pub hostname: String,
    pub firmware: &'static str,
    pub sector_size: u32,
    pub partitions: usize,
    /// A reason the backup can't boot (locked BitLocker), if any.
    pub blocker: Option<String>,
}

/// What the user did this frame.
pub enum VmAction {
    None,
    BrowseBackup,
    BrowseIso,
    /// Gracefully power down the running VM.
    Stop,
    /// Load the manifest summary for the current `backup_path`.
    LoadSummary,
    /// Boot this saved session's backup (fills the picker and boots).
    Resume(String),
    /// Discard this saved session (backup path).
    Discard(String),
}

/// A running VM, for the header status line.
pub struct RunningView<'a> {
    pub name: &'a str,
    pub elapsed_secs: u64,
    pub overlay_mib: u64,
}

/// Render the page. `qemu` is `Some(version)` when QEMU was found.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    state: &mut VmPageState,
    palette: &Palette,
    qemu: Option<&str>,
    sessions: &[SessionMeta],
    running: Option<RunningView<'_>>,
    last_status: &str,
) -> VmAction {
    let Some(version) = qemu else {
        show_unavailable(ui, palette);
        return VmAction::None;
    };

    let mut action = VmAction::None;

    // A VM is running: show its status + a graceful Stop, and nothing else
    // actionable (one VM at a time in this first cut).
    if let Some(run) = &running {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("Running: {}", run.name)).font(fonts::bold(16.0)),
        );
        ui.label(
            egui::RichText::new(format!(
                "up {}m {:02}s · overlay {} MB of guest writes",
                run.elapsed_secs / 60,
                run.elapsed_secs % 60,
                run.overlay_mib
            ))
            .color(palette.subtle_text),
        );
        ui.add_space(8.0);
        if ui.button("⏻  Stop VM (graceful)").clicked() {
            action = VmAction::Stop;
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Sends an ACPI power-down; Windows shuts down on its own, then \
                 the session is saved for resume.",
            )
            .small()
            .color(palette.subtle_text),
        );
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Saved sessions").font(fonts::bold(14.0)));
        show_sessions(ui, palette, sessions, running.as_ref(), &mut action);
        return action;
    }

    // --- backup picker ------------------------------------------------------
    ui.horizontal(|ui| {
        ui.label("Backup:");
        let resp = ui.add(
            egui::TextEdit::singleline(&mut state.backup_path)
                .hint_text("Select or browse for a .phnx backup")
                .desired_width(ui.available_width() - 90.0),
        );
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            action = VmAction::LoadSummary;
        }
        if ui.button("Browse…").clicked() {
            action = VmAction::BrowseBackup;
        }
    });
    // Auto-load the manifest when the path changes.
    if state.loaded_path != state.backup_path.trim() && !state.backup_path.trim().is_empty() {
        action = VmAction::LoadSummary;
    }

    if let Some(sum) = &state.summary {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(format!(
                "{} · {} · {}-byte sectors · {} partition(s)",
                sum.hostname, sum.firmware, sum.sector_size, sum.partitions
            ))
            .color(palette.subtle_text),
        );
        if let Some(blk) = &sum.blocker {
            ui.label(egui::RichText::new(format!("⚠ {blk}")).color(palette.warning));
        }
    }

    ui.add_space(14.0);

    // --- knobs --------------------------------------------------------------
    egui::Grid::new("vm_knobs")
        .num_columns(2)
        .spacing([16.0, 10.0])
        .show(ui, |ui| {
            ui.label("Memory (MB)");
            ui.add(egui::DragValue::new(&mut state.mem_mib).range(1024..=131072).speed(256));
            ui.end_row();

            ui.label("vCPUs");
            ui.add(egui::DragValue::new(&mut state.cpus).range(1..=32));
            ui.end_row();

            ui.label("Networking");
            ui.checkbox(&mut state.network, "Give the guest internet (user-mode NAT)");
            ui.end_row();

            // Scratch-drive picker: every mounted drive + its free space, with
            // "same drive as the image" as the default.
            ui.label("Scratch drive");
            scratch_dropdown(ui, state);
            ui.end_row();
        });

    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(
            "The session's write overlay lives here — the backup stays untouched. \
             Only the overlay grows; put it on a drive with room (an SSD is faster).",
        )
        .small()
        .color(palette.subtle_text),
    );

    ui.add_space(14.0);

    // --- rescue ISO ---------------------------------------------------------
    ui.label(egui::RichText::new("Rescue ISO (optional)").font(fonts::bold(14.0)));
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.iso_path)
                .hint_text("Attach a WinPE / recovery ISO as a CD-ROM")
                .desired_width(ui.available_width() - 90.0),
        );
        if ui.button("Browse…").clicked() {
            action = VmAction::BrowseIso;
        }
    });
    ui.add_enabled_ui(!state.iso_path.trim().is_empty(), |ui| {
        ui.checkbox(
            &mut state.boot_iso,
            "Boot from the ISO on the next boot (one time)",
        );
    });

    ui.add_space(18.0);
    ui.separator();
    ui.add_space(8.0);
    ui.label(egui::RichText::new("Saved sessions").font(fonts::bold(14.0)));
    show_sessions(ui, palette, sessions, None, &mut action);

    if !last_status.is_empty() {
        ui.add_space(10.0);
        ui.label(egui::RichText::new(last_status).color(palette.subtle_text));
    }

    // Footer: which QEMU we found.
    ui.add_space(16.0);
    ui.label(egui::RichText::new(format!("Using {version}")).small().color(palette.subtle_text));

    action
}

fn scratch_dropdown(ui: &mut Ui, state: &mut VmPageState) {
    let drives: Vec<DriveInfo> = list_drives();
    let current = match &state.scratch {
        ScratchChoice::SameAsImage => "Same drive as the image".to_string(),
        ScratchChoice::Drive(c) => drives
            .iter()
            .find(|d| d.letter == *c)
            .map(DriveInfo::label)
            .unwrap_or_else(|| format!("{c}:")),
    };
    egui::ComboBox::from_id_salt("vm_scratch")
        .selected_text(current)
        .width(320.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(
                &mut state.scratch,
                ScratchChoice::SameAsImage,
                "Same drive as the image",
            );
            for d in &drives {
                ui.selectable_value(&mut state.scratch, ScratchChoice::Drive(d.letter), d.label());
            }
        });
}

fn show_sessions(
    ui: &mut Ui,
    palette: &Palette,
    sessions: &[SessionMeta],
    running: Option<&RunningView<'_>>,
    action: &mut VmAction,
) {
    if sessions.is_empty() {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("No saved sessions yet — boot a backup to create one.")
                .color(palette.subtle_text),
        );
        return;
    }
    for s in sessions {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let name = std::path::Path::new(&s.backup_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| s.backup_path.clone());
            ui.label(egui::RichText::new(name).font(fonts::bold(13.0)));
            let state_txt = if !s.clean_shutdown {
                egui::RichText::new("dirty").color(palette.warning)
            } else {
                egui::RichText::new(s.last_booted_at.as_deref().unwrap_or("never"))
                    .color(palette.subtle_text)
            };
            ui.label(state_txt);

            // Actions on the right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let is_running = running.is_some();
                ui.add_enabled_ui(!is_running, |ui| {
                    if ui.button("Discard").clicked() {
                        *action = VmAction::Discard(s.backup_path.clone());
                    }
                    if ui.button("Resume").clicked() {
                        *action = VmAction::Resume(s.backup_path.clone());
                    }
                });
            });
        });
    }
}

/// Shown when QEMU isn't found — mirrors the Mount page's WinFsp notice.
fn show_unavailable(ui: &mut Ui, palette: &Palette) {
    const NOTICE_HEIGHT: f32 = 230.0;
    const NOTICE_WIDTH: f32 = 470.0;
    let leftover = (ui.available_height() - NOTICE_HEIGHT).max(0.0);
    ui.add_space(leftover * 0.5);
    ui.vertical_centered(|ui| {
        ui.set_max_width(NOTICE_WIDTH);
        ui.label(
            egui::RichText::new(egui_phosphor::regular::MONITOR)
                .font(fonts::icon(48.0))
                .color(palette.subtle_text),
        );
        ui.add_space(14.0);
        ui.label(egui::RichText::new("QEMU is required to boot backups as VMs").font(fonts::bold(18.0)));
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Booting a backup runs it as a virtual machine with QEMU, which isn't \
                 installed on this machine (or couldn't be found). Install QEMU and \
                 restart Simulacra to boot backups.",
            )
            .font(fonts::regular(14.0))
            .color(palette.subtle_text),
        );
        ui.add_space(16.0);
        let mut link = egui::text::LayoutJob::default();
        link.append(
            egui_phosphor::regular::LINK,
            0.0,
            egui::TextFormat {
                font_id: fonts::icon(16.0),
                color: palette.accent,
                valign: egui::Align::Center,
                ..Default::default()
            },
        );
        link.append(
            "  Get QEMU — qemu.org/download",
            0.0,
            egui::TextFormat {
                font_id: fonts::bold(14.0),
                color: palette.accent,
                valign: egui::Align::Center,
                ..Default::default()
            },
        );
        ui.hyperlink_to(link, "https://www.qemu.org/download/#windows");
    });
}
