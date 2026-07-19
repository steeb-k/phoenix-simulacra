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
    /// "Attach guest tools and driver ISO": `None` means the default —
    /// checked whenever the ISO is present on disk. `Some` is an explicit
    /// user choice.
    pub attach_drivers: Option<bool>,
    /// Free-RAM snapshot for the memory slider's color coding, refreshed
    /// when stale so "at the time of loading this pane" stays honest.
    free_mem: Option<(std::time::Instant, u64)>,
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
            attach_drivers: None,
            free_mem: None,
        }
    }
}

impl VmPageState {
    /// Whether the drivers ISO should be attached, given it is present.
    pub fn attach_drivers_on(&self) -> bool {
        self.attach_drivers.unwrap_or(true)
    }

    /// Free physical RAM in MiB, re-queried at most every few seconds.
    fn free_mem_mib(&mut self) -> Option<u64> {
        const STALE: std::time::Duration = std::time::Duration::from_secs(5);
        match self.free_mem {
            Some((at, v)) if at.elapsed() < STALE => Some(v),
            _ => {
                let v = phoenix_vm::host_free_mem_mib()?;
                self.free_mem = Some((std::time::Instant::now(), v));
                Some(v)
            }
        }
    }
}

/// What the caller knows about the guest-tools / driver ISO this frame.
pub struct DriversView {
    /// The ISO exists next to the app.
    pub present: bool,
    /// An in-flight download: (bytes so far, total bytes or 0).
    pub downloading: Option<(u64, u64)>,
    /// One-line outcome of the last download / update check ("Up to date").
    pub note: String,
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
    /// Download the guest-tools / driver ISO (virtio-win).
    DownloadDrivers,
    /// Check whether a newer driver ISO exists; download it if so.
    CheckDriversUpdate,
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
    /// The backup the VM is running from — matched against session rows so
    /// the active one reads "running" instead of "interrupted".
    pub backup_path: &'a str,
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
    drivers: &DriversView,
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
        // Phosphor POWER, not the ⏻ char — that codepoint isn't in our fonts
        // and renders as a "?" box.
        let stop_label = crate::icon_label(
            egui_phosphor::regular::POWER,
            fonts::icon(16.0),
            "Stop VM (graceful)",
            fonts::regular(14.0),
            ui.visuals().widgets.inactive.fg_stroke.color,
        );
        if ui.add(egui::Button::new(stop_label)).clicked() {
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

    // --- saved sessions FIRST -----------------------------------------------
    // Existing sessions lead the page so a user resumes what they already
    // have instead of re-picking the image and accidentally starting fresh.
    ui.label(egui::RichText::new("Saved sessions").font(fonts::bold(14.0)));
    ui.add_space(8.0);
    show_sessions(ui, palette, sessions, None, &mut action);

    ui.add_space(14.0);
    ui.separator();
    ui.add_space(10.0);

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
    let free_mib = state.free_mem_mib();
    egui::Grid::new("vm_knobs")
        .num_columns(2)
        .spacing([16.0, 10.0])
        .show(ui, |ui| {
            // VirtualBox-style sliders, capped at what this host actually has.
            ui.spacing_mut().slider_width = 320.0;

            ui.label("Memory");
            state.mem_mib = state.mem_mib.clamp(1024, mem_max);
            state.mem_mib -= state.mem_mib % 512;
            // Trailing fill colored by pressure on the RAM that is actually
            // free right now: green ≤75% of free, yellow ≤90%, red above —
            // thresholds snapped to the slider's 512 MB stops.
            let mem_color = free_mib.map(|free| {
                let snap = |v: u64| v - v % 512;
                if state.mem_mib <= snap(free * 3 / 4) {
                    palette.success
                } else if state.mem_mib <= snap(free * 9 / 10) {
                    palette.warning
                } else {
                    palette.danger
                }
            });
            ui.scope(|ui| {
                if let Some(c) = mem_color {
                    ui.visuals_mut().selection.bg_fill = c;
                }
                ui.add(
                    egui::Slider::new(&mut state.mem_mib, 1024..=mem_max)
                        .step_by(512.0)
                        .suffix(" MB")
                        .trailing_fill(true),
                );
            });
            ui.end_row();

            ui.label("Processors");
            state.cpus = state.cpus.clamp(1, cpu_max);
            // Same idea against total cores: green ≤ half, yellow ≤ 75%.
            let cpu_color = if state.cpus <= cpu_max / 2 {
                palette.success
            } else if state.cpus * 4 <= cpu_max * 3 {
                palette.warning
            } else {
                palette.danger
            };
            ui.scope(|ui| {
                ui.visuals_mut().selection.bg_fill = cpu_color;
                ui.add(egui::Slider::new(&mut state.cpus, 1..=cpu_max).trailing_fill(true));
            });
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

    ui.add_space(14.0);

    // --- rescue ISO ---------------------------------------------------------
    iso_picker(ui, state, iso_history, &mut action);
    ui.add_enabled_ui(!state.iso_path.trim().is_empty(), |ui| {
        ui.checkbox(
            &mut state.boot_iso,
            "Boot from the ISO on the next boot (one time)",
        );
    });

    ui.add_space(14.0);

    // --- guest tools / driver ISO -------------------------------------------
    drivers_section(ui, state, palette, drivers, &mut action);

    if !last_status.is_empty() {
        ui.add_space(10.0);
        ui.label(egui::RichText::new(last_status).color(palette.subtle_text));
    }

    // Footer: which QEMU we found.
    ui.add_space(16.0);
    ui.label(egui::RichText::new(format!("Using {version}")).small().color(palette.subtle_text));

    action
}

/// The guest-tools / driver ISO block: a checkbox when the ISO is present
/// (checked by default), a download link in its place when it isn't, a
/// progress bar while a download runs, and a "Check for updates" link.
fn drivers_section(
    ui: &mut Ui,
    state: &mut VmPageState,
    palette: &Palette,
    drivers: &DriversView,
    action: &mut VmAction,
) {
    if let Some((got, total)) = drivers.downloading {
        let frac = if total > 0 {
            got as f32 / total as f32
        } else {
            0.0
        };
        let text = if total > 0 {
            format!(
                "Downloading guest tools & driver ISO… {} / {} MB",
                got / 1_000_000,
                total / 1_000_000
            )
        } else {
            format!(
                "Downloading guest tools & driver ISO… {} MB",
                got / 1_000_000
            )
        };
        ui.add(
            egui::ProgressBar::new(frac)
                .desired_width(420.0)
                .text(egui::RichText::new(text).small()),
        );
    } else if drivers.present {
        let mut on = state.attach_drivers_on();
        if ui
            .checkbox(&mut on, "Attach guest tools and driver ISO")
            .changed()
        {
            state.attach_drivers = Some(on);
        }
        let update = ui.link(
            egui::RichText::new("Check for updates")
                .small()
                .color(palette.accent),
        );
        if update.clicked() {
            *action = VmAction::CheckDriversUpdate;
        }
    } else {
        // No ISO yet: the download link stands in for the checkbox.
        let link = ui.link(
            egui::RichText::new("Download the guest tools & driver ISO (virtio-win)")
                .color(palette.accent),
        );
        if link.clicked() {
            *action = VmAction::DownloadDrivers;
        }
        ui.label(
            egui::RichText::new(
                "One-time download (~700 MB), stored next to the app. The guest browses \
                 it as a CD to install display/network drivers and helper tools.",
            )
            .small()
            .color(palette.subtle_text),
        );
    }
    if !drivers.note.is_empty() {
        ui.label(
            egui::RichText::new(&drivers.note)
                .small()
                .color(palette.subtle_text),
        );
    }
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
            // Hand-rolled columns instead of a Grid: the name column flexes to
            // absorb all free width, so the fixed columns and the buttons sit
            // in the same place on every row — buttons flush right.
            const BOOTED_W: f32 = 130.0;
            const WRITES_W: f32 = 110.0;
            // Reserved for the Resume/Discard pair when computing the flex width.
            const BUTTONS_W: f32 = 190.0;
            fn cell(ui: &mut Ui, w: f32, add: impl FnOnce(&mut Ui)) {
                ui.allocate_ui_with_layout(
                    egui::vec2(w, 0.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_width(w);
                        add(ui);
                    },
                );
            }
            let name_w =
                (ui.available_width() - BOOTED_W - WRITES_W - BUTTONS_W).max(140.0);

            ui.horizontal(|ui| {
                for (w, h) in [(name_w, "Backup"), (BOOTED_W, "Last booted"), (WRITES_W, "Guest writes")] {
                    cell(ui, w, |ui| {
                        ui.label(
                            egui::RichText::new(h)
                                .small()
                                .strong()
                                .color(palette.subtle_text),
                        );
                    });
                }
            });

            for s in sessions {
                let meta = s.meta();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let name = std::path::Path::new(&meta.backup_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| meta.backup_path.clone());
                    cell(ui, name_w, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(name).font(fonts::bold(13.0)),
                            )
                            .wrap_mode(egui::TextWrapMode::Truncate),
                        )
                        .on_hover_text(&meta.backup_path);
                    });

                    // The dirty flag is set for the whole time a guest runs, so
                    // the ACTIVE session must read "running", not "interrupted".
                    let is_this_running = running
                        .is_some_and(|r| r.backup_path.eq_ignore_ascii_case(&meta.backup_path));
                    cell(ui, BOOTED_W, |ui| {
                        if is_this_running {
                            ui.label(egui::RichText::new("running").color(palette.accent));
                        } else if !meta.clean_shutdown {
                            ui.label(
                                egui::RichText::new("interrupted").color(palette.warning),
                            );
                        } else {
                            ui.label(
                                egui::RichText::new(short_time(meta.last_booted_at.as_deref()))
                                    .color(palette.subtle_text),
                            );
                        }
                    });

                    let overlay_bytes = std::fs::metadata(s.overlay_path())
                        .map(|m| m.len())
                        .unwrap_or(0);
                    cell(ui, WRITES_W, |ui| {
                        ui.label(
                            egui::RichText::new(format!("{:.1} GB", overlay_bytes as f64 / 1e9))
                                .color(palette.subtle_text),
                        );
                    });

                    // Right-pinned actions: right_to_left adds from the edge,
                    // so Discard first keeps the visual order Resume | Discard.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_enabled_ui(!is_running, |ui| {
                            if ui.button("Discard").clicked() {
                                *action = VmAction::Discard(meta.backup_path.clone());
                            }
                            if ui.button("Resume").clicked() {
                                *action = VmAction::Resume(meta.backup_path.clone());
                            }
                        });
                    });
                });
            }
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
