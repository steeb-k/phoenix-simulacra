//! The Virtualize page: boot a `.phnx` backup as a QEMU VM.
//!
//! First cut — functional, deliberately plain; visuals to be tweaked. The heavy
//! lifting (serve helper, qcow2 overlay, sessions, QMP) lives in `phoenix-vm`;
//! this is the picker + knobs + sessions list, plus the QEMU-missing fallback
//! that mirrors the Mount page's WinFsp notice.

use egui::Ui;
use phoenix_vm::drives::{list_drives, DriveInfo};
use phoenix_vm::session::Session;

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

/// What the manifest load extracted from a picked backup: the VM's display
/// name, and any reason it can't boot. (The load doubles as validation — the
/// action bar stays disabled until a backup has parsed cleanly.)
#[derive(Clone)]
pub struct BackupSummary {
    pub hostname: String,
    /// A reason the backup can't boot (locked BitLocker), if any.
    pub blocker: Option<String>,
}

/// What the user did this frame.
pub enum VmAction {
    None,
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
/// `iso_history` is the settings-persisted list of previously-attached ISOs
/// (newest first) that seeds the Rescue ISO dropdown.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    state: &mut VmPageState,
    palette: &Palette,
    qemu: Option<&str>,
    history: &phoenix_core::appdata::History,
    iso_history: &[String],
    sessions: &[Session],
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
    ui.add_space(8.0);
        show_sessions(ui, palette, sessions, running.as_ref(), &mut action);
        return action;
    }

    // --- backup picker ------------------------------------------------------
    // The same history-dropdown + Browse… row every other backup-consuming
    // page uses (Restore / Verify / Mount).
    if crate::backup_path_picker(
        ui,
        "Backup",
        "Select or browse for a .phnx backup",
        &mut state.backup_path,
        history,
        None,
    ) {
        action = VmAction::LoadSummary;
    }
    // Auto-load when the path changed some other way (Resume fills it in).
    if state.loaded_path != state.backup_path.trim() && !state.backup_path.trim().is_empty() {
        action = VmAction::LoadSummary;
    }

    // The manifest load is silent when it succeeds — only a boot blocker
    // (locked BitLocker) is worth a line here.
    if let Some(blk) = state.summary.as_ref().and_then(|s| s.blocker.as_ref()) {
        ui.add_space(6.0);
        ui.label(egui::RichText::new(format!("⚠ {blk}")).color(palette.warning));
    }

    ui.add_space(14.0);

    // --- knobs --------------------------------------------------------------
    let (host_mem_mib, host_cpus) = host_caps_cached();
    // Floor the ceiling to a 512 MB boundary — the OS reports an odd total
    // (RAM minus reserved), and without this the slider's top stop lands on a
    // random-looking number.
    let mem_max = {
        let m = host_mem_mib.max(2048);
        m - m % 512
    };
    let cpu_max = host_cpus.max(1);
    egui::Grid::new("vm_knobs")
        .num_columns(2)
        .spacing([16.0, 10.0])
        .show(ui, |ui| {
            // VirtualBox-style sliders, capped at what this host actually has.
            ui.spacing_mut().slider_width = 320.0;

            ui.label("Memory");
            state.mem_mib = state.mem_mib.clamp(1024, mem_max);
            state.mem_mib -= state.mem_mib % 512;
            ui.add(
                egui::Slider::new(&mut state.mem_mib, 1024..=mem_max)
                    .step_by(512.0)
                    .suffix(" MB"),
            );
            ui.end_row();

            ui.label("Processors");
            state.cpus = state.cpus.clamp(1, cpu_max);
            ui.add(egui::Slider::new(&mut state.cpus, 1..=cpu_max));
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
    iso_picker(ui, state, iso_history, &mut action);
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
    ui.add_space(8.0);
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

/// Host RAM/CPU ceilings for the sliders, queried once — they don't change
/// while we run, and the page redraws every frame.
fn host_caps_cached() -> (u64, u32) {
    static CAPS: std::sync::OnceLock<(u64, u32)> = std::sync::OnceLock::new();
    *CAPS.get_or_init(phoenix_vm::host_caps)
}

/// "Rescue ISO" row: a dropdown of previously-attached ISOs with Browse… for a
/// new one — the same layout as `backup_path_picker` so the rows line up.
/// Picking an entry takes effect directly on `state`; only Browse… needs the
/// caller (it opens the file dialog and persists the pick into settings).
fn iso_picker(ui: &mut Ui, state: &mut VmPageState, iso_history: &[String], action: &mut VmAction) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Rescue ISO").font(fonts::bold(14.0)));

        let spacing = ui.spacing().item_spacing.x;
        let combo_w = (ui.available_width() - crate::FORM_BUTTON_W - spacing).max(0.0);
        ui.scope(|ui| {
            ui.spacing_mut().interact_size.y = crate::ACTION_BUTTON_HEIGHT;
            let selected = if state.iso_path.trim().is_empty() {
                egui::RichText::new("None — optionally boot a WinPE / recovery ISO")
                    .font(fonts::regular(16.0))
                    .color(ui.visuals().weak_text_color())
            } else {
                egui::RichText::new(state.iso_path.as_str()).font(fonts::regular(16.0))
            };
            egui::ComboBox::from_id_salt("vm_iso_combo")
                .width(combo_w)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    let none_now = state.iso_path.trim().is_empty();
                    if ui
                        .selectable_label(
                            none_now,
                            egui::RichText::new("None").font(fonts::regular(14.0)),
                        )
                        .clicked()
                    {
                        state.iso_path.clear();
                        state.boot_iso = false;
                    }
                    // Stat only while the popup is open, and skip dead paths so
                    // the list never offers an ISO that has since moved.
                    for iso in iso_history {
                        if !std::path::Path::new(iso).is_file() {
                            continue;
                        }
                        let is_current = iso.eq_ignore_ascii_case(&state.iso_path);
                        if ui
                            .selectable_label(
                                is_current,
                                egui::RichText::new(iso).font(fonts::regular(14.0)),
                            )
                            .clicked()
                        {
                            state.iso_path = iso.clone();
                        }
                    }
                });
        });

        let browse_label = crate::icon_label(
            egui_phosphor::regular::FOLDER,
            fonts::icon(16.0),
            "Browse…",
            fonts::regular(14.0),
            ui.visuals().widgets.inactive.fg_stroke.color,
        );
        if ui
            .add_sized(
                [crate::FORM_BUTTON_W, crate::ACTION_BUTTON_HEIGHT],
                egui::Button::new(browse_label),
            )
            .clicked()
        {
            *action = VmAction::BrowseIso;
        }
    });
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

/// The saved-sessions table, in a card of its own (contrasting fill) so it
/// reads as existing state rather than more knobs for the next boot.
fn show_sessions(
    ui: &mut Ui,
    palette: &Palette,
    sessions: &[Session],
    running: Option<&RunningView<'_>>,
    action: &mut VmAction,
) {
    let is_running = running.is_some();
    egui::Frame::none()
        .fill(palette.content_card_bg)
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::symmetric(16.0, 12.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if sessions.is_empty() {
                ui.label(
                    egui::RichText::new("No saved sessions yet — boot a backup to create one.")
                        .color(palette.subtle_text),
                );
                return;
            }
            egui::Grid::new("vm_sessions_table")
                .num_columns(4)
                .spacing([28.0, 10.0])
                .show(ui, |ui| {
                    for h in ["Backup", "Last booted", "Guest writes", ""] {
                        ui.label(
                            egui::RichText::new(h)
                                .small()
                                .strong()
                                .color(palette.subtle_text),
                        );
                    }
                    ui.end_row();

                    for s in sessions {
                        let meta = s.meta();
                        let name = std::path::Path::new(&meta.backup_path)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| meta.backup_path.clone());
                        // Extend, never wrap: a Grid column takes its width from
                        // its cells, and a wrapping label collapses to the header's
                        // width and breaks the name one character per line. The
                        // full path lives in the tooltip.
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(name).font(fonts::bold(13.0)),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        )
                        .on_hover_text(&meta.backup_path);

                        if !meta.clean_shutdown {
                            ui.label(
                                egui::RichText::new("interrupted").color(palette.warning),
                            );
                        } else {
                            ui.label(
                                egui::RichText::new(short_time(meta.last_booted_at.as_deref()))
                                    .color(palette.subtle_text),
                            );
                        }

                        let overlay_bytes = std::fs::metadata(s.overlay_path())
                            .map(|m| m.len())
                            .unwrap_or(0);
                        ui.label(
                            egui::RichText::new(format!(
                                "{:.1} GB",
                                overlay_bytes as f64 / 1e9
                            ))
                            .color(palette.subtle_text),
                        );

                        ui.horizontal(|ui| {
                            ui.add_enabled_ui(!is_running, |ui| {
                                if ui.button("Resume").clicked() {
                                    *action = VmAction::Resume(meta.backup_path.clone());
                                }
                                if ui.button("Discard").clicked() {
                                    *action = VmAction::Discard(meta.backup_path.clone());
                                }
                            });
                        });
                        ui.end_row();
                    }
                });
        });
}

/// RFC 3339 ("2026-07-19T04:12:33Z") → "2026-07-19 04:12", or "never".
fn short_time(ts: Option<&str>) -> String {
    match ts {
        Some(t) => match (t.get(..10), t.get(11..16)) {
            (Some(d), Some(hm)) => format!("{d} {hm}"),
            _ => t.to_string(),
        },
        None => "never".into(),
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
