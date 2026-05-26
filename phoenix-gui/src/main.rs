mod disk_panel;
mod fonts;
mod job;
mod sidebar;
mod theme;
mod util;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align2, Response, Rounding, Sense, Ui, Vec2};
use phoenix_capture::backup::BackupOptions;
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{enumerate_disks, refine_partition_fs, DiskInfo};
use phoenix_core::ProgressHandle;
use phoenix_restore::plan::{default_plan_from_backup, RestorePlan, RestorePlanEntry};
use phoenix_restore::restore::RestoreOptions;

use crate::job::{spawn_backup, spawn_restore, spawn_verify, BackgroundJob};
use crate::sidebar::Page;
use crate::theme::Palette;
use crate::util::format_bytes;

const THEME_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const REFRESH_BTN_SIZE: f32 = 52.0;
const REFRESH_ICON_SIZE: f32 = 32.0;
/// Form-action button width on the Backup page (Browse, Start backup).
/// Tuned to comfortably fit "Start backup" at 16pt so both buttons render
/// at the same width.
const FORM_BUTTON_W: f32 = 130.0;
/// Padding added back to a `TextEdit::singleline` response rect to recover
/// its outer visible height — `TextEdit::ui` subtracts its margin from
/// the returned rect, so `response.rect.height() + INPUT_MARGIN_RESTORE`
/// equals the field's visible outer height. With our `margin(10, 8)`
/// that's `2 * 8 = 16`.
const INPUT_MARGIN_RESTORE: f32 = 16.0;
/// Default height for the colored Start/Cancel action buttons on pages
/// that don't have a nearby `TextEdit` response to measure from
/// (Restore, Verify). Tuned to match the Backup page's computed
/// `input_height` (roughly 16pt text + 2×8px TextEdit margin).
const ACTION_BUTTON_HEIGHT: f32 = 36.0;

/// Decode the embedded application icon for the window / taskbar.
fn app_icon() -> egui::IconData {
    let bytes = include_bytes!("../../carbon-phoenix-icon.ico");
    let image = image::load_from_memory(bytes).expect("failed to decode carbon-phoenix-icon.ico");
    let image = image.into_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

/// Approximate height of the bottom status bar (`Margin::symmetric(16.0,
/// 8.0)` + one text line). Added to the sidebar's minimum content height to
/// derive the OS window's `min_inner_size` so the sidebar can always show
/// the brand, History/Options, and 1.5 scrollable items at the smallest
/// allowed window size.
const STATUS_BAR_HEIGHT_ESTIMATE: f32 = 40.0;
const MIN_WINDOW_WIDTH: f32 = 640.0;

fn main() -> eframe::Result<()> {
    let _log_guard = init_logging();

    let min_height = sidebar::min_content_height() + STATUS_BAR_HEIGHT_ESTIMATE;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([MIN_WINDOW_WIDTH, min_height])
            .with_icon(Arc::new(app_icon())),
        ..Default::default()
    };
    eframe::run_native(
        "Carbon Phoenix",
        options,
        Box::new(|cc| Ok(Box::new(PhoenixApp::new(cc)))),
    )
}

/// Sets up tracing to BOTH stderr (for users who launch from a terminal) and
/// a rolling log file in `%LOCALAPPDATA%\CarbonPhoenix\logs\` so users who
/// double-click the .exe can still hand us a log when something goes wrong.
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "phoenix_core=info,phoenix_gui=info,warn".into());

    let log_dir = std::env::var("LOCALAPPDATA")
        .map(|p| std::path::PathBuf::from(p).join("CarbonPhoenix").join("logs"))
        .unwrap_or_else(|_| std::env::temp_dir().join("CarbonPhoenix").join("logs"));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true);

    let (file_layer, guard) = match std::fs::create_dir_all(&log_dir) {
        Ok(()) => {
            let appender =
                tracing_appender::rolling::daily(&log_dir, "carbon-phoenix.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_target(true);
            (Some(layer), Some(guard))
        }
        Err(_) => (None, None),
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    if guard.is_some() {
        tracing::info!(log_dir = %log_dir.display(), "log file initialized");
    }
    guard
}

struct PhoenixApp {
    disks: Vec<DiskInfo>,
    /// Set of `(disk_index, partition_index)` pairs the user has clicked on
    /// in the partition map. Survives disk refreshes by index, but stale
    /// entries get filtered out when partitions change.
    selections: HashSet<(u32, u32)>,
    /// Directory the `.phnx` file(s) get written into. The full output path
    /// is composed as `{backup_folder}/{backup_name}.phnx`, with `-disk<N>`
    /// inserted before the extension when more than one disk is selected.
    backup_folder: String,
    /// User-entered base filename (no extension, no disk suffix). Empty
    /// means "not yet provided" — the Backup name input is tinted red and
    /// the Start backup button is disabled until something is typed here.
    backup_name: String,
    use_vss: bool,
    /// FIFO queue of remaining backup jobs when a multi-disk selection is
    /// in flight. The currently-running job lives in `job`; on completion
    /// we pop the next entry here and spawn it.
    pending_backups: Vec<BackupOptions>,
    /// Total number of jobs in the current multi-disk run (for "Disk N of M"
    /// status text). Reset to 0 when the queue drains.
    total_backups: usize,
    /// 1-based index of the job currently running for the status text.
    current_backup_index: usize,
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
        fonts::install(&cc.egui_ctx);

        let palette = theme::refresh(&cc.egui_ctx);

        let mut app = Self {
            disks: Vec::new(),
            selections: HashSet::new(),
            backup_folder: default_backup_folder(),
            backup_name: String::new(),
            use_vss: false,
            pending_backups: Vec::new(),
            total_backups: 0,
            current_backup_index: 0,
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
                self.disks = disks;
                self.prune_stale_selections();
                self.status = format!("Found {} disk(s)", self.disks.len());
            }
            Err(e) => self.status = format!("Disk enumeration failed: {e}"),
        }
    }

    /// Drop selection entries whose `(disk_index, partition_index)` no
    /// longer matches any partition currently visible. Avoids ghost
    /// selections after a `Refresh disks` if the layout changed.
    fn prune_stale_selections(&mut self) {
        self.selections.retain(|(disk_idx, part_idx)| {
            self.disks
                .iter()
                .find(|d| d.index == *disk_idx)
                .map(|d| d.partitions.iter().any(|p| p.index == *part_idx))
                .unwrap_or(false)
        });
    }

    fn busy(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.is_running()) || !self.pending_backups.is_empty()
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else {
            return;
        };
        let job_kind = job.kind;
        if let Some(result) = job.poll() {
            self.job = None;
            match result {
                Ok(msg) => {
                    if let Some(next) = self.pending_backups.pop() {
                        self.current_backup_index += 1;
                        let path_display = next.output.display().to_string();
                        self.status = format!(
                            "Backing up disk {} of {} to {}…",
                            self.current_backup_index, self.total_backups, path_display
                        );
                        self.job = Some(spawn_backup(next));
                    } else {
                        self.status = if self.total_backups > 1 {
                            format!("All {} backups completed", self.total_backups)
                        } else {
                            msg
                        };
                        self.total_backups = 0;
                        self.current_backup_index = 0;
                    }
                }
                Err(e) => {
                    // `phoenix-core` returns `PhoenixError::Cancelled` with this
                    // exact Display string — match on it so the status reads as
                    // a clean "Backup cancelled" instead of "Error: operation
                    // cancelled by user". Cross-checking the kind from the job
                    // keeps the wording accurate even if the user has navigated
                    // to another page while the worker was winding down.
                    let cancelled = e.contains("cancelled by user");
                    if cancelled {
                        self.status = job_kind.cancelled_message().to_string();
                        self.pending_backups.clear();
                    } else if !self.pending_backups.is_empty() {
                        self.status = format!(
                            "Error on disk {} of {}: {e}. Remaining jobs cancelled.",
                            self.current_backup_index, self.total_backups
                        );
                        self.pending_backups.clear();
                    } else {
                        self.status = format!("Error: {e}");
                    }
                    self.total_backups = 0;
                    self.current_backup_index = 0;
                }
            }
        } else {
            ctx.request_repaint();
        }
    }

    /// Request the active worker to wind down at the next chunk boundary
    /// and drop any queued multi-disk backups so they don't auto-start
    /// after the current worker exits. We deliberately do NOT clear
    /// `self.job` here — the worker will finish its current chunk, return
    /// `Err(Cancelled)`, and `poll_job` will translate that into the
    /// final "Backup cancelled" / "Restore cancelled" / "Verify cancelled"
    /// status text.
    fn cancel_current_job(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        job.progress.cancel();
        self.pending_backups.clear();
        self.total_backups = 0;
        self.current_backup_index = 0;
        self.status = "Cancelling…".into();
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
                    show_progress(ui, &job.progress, &self.palette);
                }
                ui.label(egui::RichText::new(&self.status).color(self.palette.subtle_text));
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::Margin::symmetric(24.0, 20.0)))
            .show(ctx, |ui| {
                // Pages handle `busy` themselves now so the Cancel button
                // on each action row can stay live while the rest of the
                // form is greyed out. `add_enabled_ui` is a one-way ratchet
                // — `(true, ...)` inside `(false, ...)` does NOT re-enable
                // — so a single whole-page disable would block Cancel too.
                match self.page {
                    Page::Backup => self.ui_backup(ui, busy),
                    Page::Clone => disabled_when(ui, busy, |ui| self.ui_clone(ui)),
                    Page::Restore => self.ui_restore(ui, busy),
                    Page::Verify => self.ui_verify(ui, busy),
                    Page::Mount => disabled_when(ui, busy, |ui| self.ui_mount(ui)),
                    Page::History => disabled_when(ui, busy, |ui| self.ui_history(ui)),
                    Page::Options => disabled_when(ui, busy, |ui| self.ui_options(ui)),
                }
            });
    }
}

/// Wrap a "no own action row yet" page in `add_enabled_ui(!busy, …)`
/// so its widgets grey out while another page's job is running.
fn disabled_when<R>(ui: &mut egui::Ui, busy: bool, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.add_enabled_ui(!busy, body).inner
}

/// A single "start" affordance configured by [`action_row`]: green pill,
/// disabled-and-dimmed when `enabled` is false, with an optional
/// `disabled_hint` shown on hover when the user cannot click it.
pub struct StartAction<'a> {
    pub label: &'a str,
    pub enabled: bool,
    pub disabled_hint: Option<&'a str>,
}

/// Render `starts` (one or more green Start-style buttons) followed by a
/// single red Cancel button at the end of the row. Returns `(Some(start_idx), false)`
/// when one of the start buttons was clicked, `(None, true)` when Cancel
/// was clicked, or `(None, false)` otherwise.
///
/// All buttons land at `FORM_BUTTON_W` × `height`. Disabled colored
/// buttons stay tinted (faded, via `Palette::dim`) instead of going
/// fully grey so the user still recognizes which control is Go vs Stop.
fn action_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    height: f32,
    starts: &[StartAction<'_>],
    cancel_enabled: bool,
) -> (Option<usize>, bool) {
    let mut clicked_start: Option<usize> = None;
    let mut clicked_cancel = false;
    ui.horizontal(|ui| {
        for (idx, start) in starts.iter().enumerate() {
            let fill = if start.enabled {
                palette.success
            } else {
                palette.dim(palette.success)
            };
            let resp = ui
                .add_enabled_ui(start.enabled, |ui| {
                    ui.add_sized(
                        [FORM_BUTTON_W, height],
                        egui::Button::new(
                            egui::RichText::new(start.label).color(egui::Color32::WHITE),
                        )
                        .fill(fill),
                    )
                })
                .inner;
            let resp = match start.disabled_hint {
                Some(hint) => resp.on_disabled_hover_text(hint),
                None => resp,
            };
            if resp.clicked() {
                clicked_start = Some(idx);
            }
        }
        let cancel_fill = if cancel_enabled {
            palette.danger
        } else {
            palette.dim(palette.danger)
        };
        let cancel_resp = ui
            .add_enabled_ui(cancel_enabled, |ui| {
                ui.add_sized(
                    [FORM_BUTTON_W, height],
                    egui::Button::new(
                        egui::RichText::new("Cancel").color(egui::Color32::WHITE),
                    )
                    .fill(cancel_fill),
                )
            })
            .inner
            .on_disabled_hover_text("No operation is currently running");
        if cancel_resp.clicked() {
            clicked_cancel = true;
        }
    });
    (clicked_start, clicked_cancel)
}

/// Bold "label + TextEdit + Browse…" row used by both the Verify and Restore
/// pages for picking an existing `.phnx` file. Mirrors the styling of the
/// "Save backup to folder" row on the Backup page: 14pt bold label, 16pt
/// field text with `(10, 8)` margins, and a `Browse…` button whose width
/// is reserved on the right so it never falls off the side of the pane.
fn backup_path_picker(ui: &mut egui::Ui, label: &str, hint: &str, path: &mut String) {
    ui.label(egui::RichText::new(label).font(fonts::bold(14.0)));
    ui.horizontal(|ui| {
        // Same trick as the folder picker on the Backup page: take the
        // button's fixed width out of `available_width` before sizing the
        // field, so the field can't grow tall enough to push Browse past
        // the right edge of the central panel.
        let spacing = ui.spacing().item_spacing.x;
        let text_w = (ui.available_width() - FORM_BUTTON_W - spacing).max(0.0);

        let field_response = ui.add(
            egui::TextEdit::singleline(path)
                .desired_width(text_w)
                .font(fonts::regular(16.0))
                .hint_text_font(fonts::regular(16.0))
                .margin(egui::Margin::symmetric(10.0, 8.0))
                .hint_text(hint),
        );

        // Outer (visible) height of the TextEdit — the response rect is
        // the inner text area, so add back the vertical margin to get the
        // height the user actually sees and size Browse to match.
        let visible_h = field_response.rect.height() + INPUT_MARGIN_RESTORE;

        if ui
            .add_sized(
                [FORM_BUTTON_W, visible_h],
                egui::Button::new(
                    egui::RichText::new("Browse…").font(fonts::regular(16.0)),
                ),
            )
            .clicked()
        {
            if let Some(picked) = pick_backup_open_path(path) {
                *path = picked.display().to_string();
            }
        }
    });
}

fn page_header(ui: &mut egui::Ui, palette: &Palette, title: &str, subtitle: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(title).font(fonts::bold(22.0)));
    ui.label(egui::RichText::new(subtitle).color(palette.subtle_text));
    ui.add_space(14.0);
}

fn coming_soon(ui: &mut egui::Ui, palette: &Palette, blurb: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(egui_phosphor::regular::SPARKLE)
                .font(fonts::icon(56.0))
                .color(palette.accent),
        );
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Coming soon").font(fonts::bold(18.0)));
        ui.add_space(4.0);
        ui.label(egui::RichText::new(blurb).color(palette.subtle_text));
    });
}

fn show_progress(ui: &mut egui::Ui, progress: &ProgressHandle, palette: &Palette) {
    let snap = progress.snapshot();
    if !snap.active {
        return;
    }
    ui.label(&snap.phase);
    if !snap.detail.is_empty() {
        ui.label(&snap.detail);
    }
    if snap.total > 0 {
        let fraction = snap.fraction();
        // Custom breathing fill: lerps the bar between a desaturated
        // ~30%-accent + ~70%-input_bg blend and the full accent over a
        // ~2s cycle. This swaps egui's built-in `.animate(true)` — which
        // adds a hard-to-see grey "spinner arc" at the bar's leading edge
        // and only pulses brightness by a barely-visible 30% — for a
        // noticeably louder "this UI is alive" cue that doesn't need a
        // spinner widget at all. We pulse via `ProgressBar::fill()` (a
        // public builder method) instead of forking the widget.
        let fill = if fraction >= 1.0 {
            palette.accent
        } else {
            let t = ui.input(|i| i.time) as f32;
            // 0..1 sinusoidal pulse with a ~2s period — matches the
            // tempo of a calm human exhale-inhale, which avoids feeling
            // anxious / busy.
            let pulse = 0.5 + 0.5 * (t * std::f32::consts::PI).sin();
            let dim_end = theme::tint(palette.accent, 0.7, palette.input_bg);
            theme::tint(dim_end, pulse, palette.accent)
        };

        ui.add(
            egui::ProgressBar::new(fraction)
                .fill(fill)
                .text(format!(
                    "{} / {} ({:.1}%)",
                    format_bytes(snap.current),
                    format_bytes(snap.total),
                    fraction * 100.0
                )),
        );
        if fraction < 1.0 {
            // egui's `.animate(true)` would call this for us. Now that
            // we're driving the animation ourselves we have to ask for
            // the next frame explicitly or the pulse would freeze.
            ui.ctx().request_repaint();
        }
    } else {
        ui.add(egui::Spinner::new());
    }
}

/// Large phosphor refresh control for re-enumerating disks.
fn refresh_disks_button(ui: &mut Ui, palette: &Palette) -> Response {
    let size = Vec2::splat(REFRESH_BTN_SIZE);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());

    let hovered = response.hovered();
    let painter = ui.painter_at(rect);
    if hovered {
        painter.rect_filled(rect, Rounding::same(8.0), palette.sidebar_hover_bg);
    }

    let icon_color = if hovered {
        palette.accent
    } else {
        palette.icon_color
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        egui_phosphor::regular::ARROWS_CLOCKWISE,
        fonts::icon(REFRESH_ICON_SIZE),
        icon_color,
    );

    let response = response.on_hover_text("Refresh disks");
    // This button paints itself directly with the phosphor icon and has no
    // egui-managed frame, so the global `widgets.active.bg_stroke` focus
    // bump never applies — add an explicit focus ring here.
    theme::draw_focus_outline(ui, &response, palette);
    response
}

impl PhoenixApp {
    fn ui_backup(&mut self, ui: &mut egui::Ui, busy: bool) {
        // Whole-page scroll: when the window is too short, the entire backup
        // page (header, drive list, path field, options, buttons) scrolls as
        // a unit. The inner drive list still has its own scroll area capped
        // at ~3.5 rows so the rest of the controls stay close at hand.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.ui_backup_inner(ui, busy);
            });
    }

    fn ui_backup_inner(&mut self, ui: &mut egui::Ui, busy: bool) {
        page_header(
            ui,
            &self.palette,
            "Create Backup",
            "Create a disk or partition image backup with optional compression and verification.",
        );

        // Compute name_missing OUTSIDE the disabled scope so the action
        // row below can still gate Start on it.
        let name_missing = self.backup_name.trim().is_empty();

        // We need to fish the Backup name TextEdit's response out of an
        // `add_enabled_ui` scope so the rest of the page can size buttons
        // to match its height. `ui.add_enabled_ui` returns `InnerResponse`,
        // so we just bubble the inner closure value back up.
        let name_response = ui
            .add_enabled_ui(!busy, |ui| {
                self.ui_backup_form(ui, name_missing)
            })
            .inner;

        let input_height = name_response.rect.height() + INPUT_MARGIN_RESTORE;

        // Action row sits OUTSIDE the disabled wrap so the Cancel button
        // can stay live while the form is greyed out.
        let starts = [StartAction {
            label: "Start backup",
            enabled: !busy && !name_missing,
            disabled_hint: Some(if busy {
                "A backup is already running"
            } else {
                "Enter a backup name first"
            }),
        }];
        let (start_clicked, cancel_clicked) =
            action_row(ui, &self.palette, input_height, &starts, busy);
        if start_clicked == Some(0) {
            self.start_backup();
        }
        if cancel_clicked {
            self.cancel_current_job();
        }
    }

    /// The greyed-when-busy portion of the Backup page: name field,
    /// folder field, disk list, and VSS toggle. Returns the Backup-name
    /// `TextEdit` response so the caller can use its height to size the
    /// action row's buttons.
    fn ui_backup_form(&mut self, ui: &mut egui::Ui, name_missing: bool) -> egui::Response {
        ui.label(egui::RichText::new("Backup name").font(fonts::bold(14.0)));

        let name_response = ui
            .scope(|ui| {
                if name_missing {
                    // `extreme_bg_color` is what TextEdit uses as its field
                    // fill. Hint text is drawn with `weak_text_color()`,
                    // which blends `text_color()` toward `weak_bg_fill` —
                    // so we also steer that weak_bg_fill to the error tint
                    // so the placeholder lands on a readable pinkish color
                    // instead of disappearing into the red.
                    ui.visuals_mut().extreme_bg_color = self.palette.error_bg;
                    ui.visuals_mut().widgets.noninteractive.weak_bg_fill =
                        self.palette.error_bg;
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.backup_name)
                        .desired_width(f32::INFINITY)
                        .font(fonts::regular(16.0))
                        .hint_text_font(fonts::regular(16.0))
                        .margin(egui::Margin::symmetric(10.0, 8.0))
                        .hint_text("e.g. workstation-2026-05-23"),
                )
            })
            .inner;

        // Single source of truth for the visible outer height of an input
        // on this page. Reused for the Browse button so every form widget
        // sits on a consistent baseline.
        let input_height = name_response.rect.height() + INPUT_MARGIN_RESTORE;

        ui.label(
            egui::RichText::new(
                "Used as the base filename. `.phnx` is added automatically, and a \
                 `-disk<N>` suffix is inserted when multiple disks are selected.",
            )
            .color(self.palette.subtle_text),
        );
        ui.add_space(12.0);

        if self.disks.is_empty() {
            ui.label("No disks found. Run as Administrator.");
            if refresh_disks_button(ui, &self.palette).clicked() {
                self.refresh_disks();
            }
            return name_response;
        }

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Select Source").font(fonts::bold(16.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if refresh_disks_button(ui, &self.palette).clicked() {
                    self.refresh_disks();
                }
            });
        });
        ui.label(
            egui::RichText::new("Choose the partitions you want to back up.")
                .color(self.palette.subtle_text),
        );
        ui.add_space(8.0);

        // Capture the visible viewport width BEFORE entering the inner
        // ScrollArea::both — inside `both()` `available_width()` is virtual
        // and would push rows arbitrarily wide. We use this to size each
        // drive row to `max(viewport, natural_min)` so:
        //   * wide window → rows fit, no horizontal scroll;
        //   * narrow window → rows overflow horizontally rather than
        //     squishing partition labels into illegibility.
        let viewport_width = ui.available_width();
        let drive_pane_max_height = disk_panel::row_stride() * 3.5;

        egui::ScrollArea::both()
            .id_salt("backup_drive_list")
            .max_height(drive_pane_max_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                disk_panel::show(
                    ui,
                    &self.disks,
                    &mut self.selections,
                    &self.palette,
                    viewport_width,
                );
            });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        ui.label(egui::RichText::new("Save backup to folder").font(fonts::bold(14.0)));
        ui.horizontal(|ui| {
            // Reserve fixed width for the Browse button on the right so it
            // always fits inside the central panel, then let the TextEdit
            // fill the remainder. Without this reservation, the field's
            // `desired_width(f32::INFINITY)` consumed everything in the
            // horizontal row and pushed Browse off the right edge of the
            // window at every window size.
            let spacing = ui.spacing().item_spacing.x;
            let text_w = (ui.available_width() - FORM_BUTTON_W - spacing).max(0.0);

            ui.add(
                egui::TextEdit::singleline(&mut self.backup_folder)
                    .desired_width(text_w)
                    .font(fonts::regular(16.0))
                    .hint_text_font(fonts::regular(16.0))
                    .margin(egui::Margin::symmetric(10.0, 8.0))
                    .hint_text(r"e.g. D:\Backups"),
            );

            if ui
                .add_sized(
                    [FORM_BUTTON_W, input_height],
                    egui::Button::new(
                        egui::RichText::new("Browse…").font(fonts::regular(16.0)),
                    ),
                )
                .clicked()
            {
                if let Some(path) = pick_backup_save_folder(&self.backup_folder) {
                    self.backup_folder = path.display().to_string();
                }
            }
        });

        let grouped = self.group_selections();
        if grouped.len() > 1 {
            ui.label(
                egui::RichText::new(format!(
                    "{} disks selected — output files will be auto-suffixed with -disk<N>.",
                    grouped.len()
                ))
                .color(self.palette.subtle_text),
            );
        }

        let vss_response = ui.checkbox(&mut self.use_vss, "Use VSS (live Windows)");
        theme::draw_focus_outline(ui, &vss_response, &self.palette);

        name_response
    }

    /// Group the current selection set by disk index, returning a sorted map
    /// of `disk_index -> Vec<partition_index>` (partition indices sorted).
    fn group_selections(&self) -> BTreeMap<u32, Vec<u32>> {
        let mut grouped: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (disk_idx, part_idx) in &self.selections {
            grouped.entry(*disk_idx).or_default().push(*part_idx);
        }
        for parts in grouped.values_mut() {
            parts.sort_unstable();
        }
        grouped
    }

    fn start_backup(&mut self) {
        let name = sanitize_backup_name(&self.backup_name);
        if name.is_empty() {
            self.status = "Enter a backup name first".into();
            return;
        }

        let grouped = self.group_selections();
        if grouped.is_empty() {
            self.status = "Select at least one partition to back up".into();
            return;
        }

        // Fall back to the default Documents folder if the user blanked the
        // folder field — better than erroring out with "folder is empty".
        let folder_input = self.backup_folder.trim();
        let folder = if folder_input.is_empty() {
            let default_dir = default_backup_folder();
            self.backup_folder = default_dir.clone();
            PathBuf::from(default_dir)
        } else {
            PathBuf::from(folder_input)
        };
        if folder.as_os_str().is_empty() {
            self.status = "Choose a folder to save the backup into".into();
            return;
        }
        if !folder.exists() {
            self.status = format!(
                "Backup folder does not exist: {}",
                folder.display()
            );
            return;
        }
        if !folder.is_dir() {
            self.status = format!(
                "Backup destination is not a folder: {}",
                folder.display()
            );
            return;
        }

        let base_path = folder.join(format!("{name}.phnx"));

        let multi = grouped.len() > 1;
        let mut queue: Vec<BackupOptions> = Vec::with_capacity(grouped.len());
        for (disk_idx, parts) in grouped {
            let output = if multi {
                suffix_path(&base_path, disk_idx)
            } else {
                base_path.clone()
            };
            queue.push(BackupOptions {
                disk_index: disk_idx,
                partition_indices: parts,
                output,
                use_vss: self.use_vss,
                progress: Some(ProgressHandle::new()),
            });
        }

        self.total_backups = queue.len();
        self.current_backup_index = 1;
        // pop() draws from the back, so reverse to preserve disk-index order.
        queue.reverse();
        let first = queue.pop().expect("queue non-empty");
        self.pending_backups = queue;
        let path_display = first.output.display().to_string();
        self.status = if self.total_backups > 1 {
            format!(
                "Backing up disk 1 of {} to {}…",
                self.total_backups, path_display
            )
        } else {
            format!("Backing up to {}…", path_display)
        };
        self.job = Some(spawn_backup(first));
    }

    fn ui_restore(&mut self, ui: &mut egui::Ui, busy: bool) {
        page_header(
            ui,
            &self.palette,
            "Restore",
            "Apply a .phnx backup back onto a target disk.",
        );

        ui.add_enabled_ui(!busy, |ui| {
            self.ui_restore_form(ui);
        });

        let plan_ready = !self.restore_plan_entries.is_empty();
        let starts = [StartAction {
            label: "Run restore",
            enabled: !busy && plan_ready,
            disabled_hint: Some(if busy {
                "A restore is already running"
            } else {
                "Load a backup & plan first"
            }),
        }];
        let (start_clicked, cancel_clicked) =
            action_row(ui, &self.palette, ACTION_BUTTON_HEIGHT, &starts, busy);
        if start_clicked == Some(0) {
            self.start_restore();
        }
        if cancel_clicked {
            self.cancel_current_job();
        }
    }

    /// "Select target" header + disk dropdown + refresh button on the
    /// Restore page. Mirrors the layout of the Backup page's "Select
    /// Source" header (header label on the left, refresh affordance on
    /// the right) and binds the dropdown directly to
    /// `self.target_disk_index` so the existing restore plumbing — which
    /// already keys off that field — works unchanged.
    ///
    /// When `self.disks` is empty (typically because we're not running
    /// elevated or `enumerate_disks` failed), the dropdown is replaced
    /// with the same "No disks found / Refresh" affordance the Backup
    /// page shows in that situation.
    fn ui_restore_target_picker(&mut self, ui: &mut egui::Ui) {
        if self.disks.is_empty() {
            ui.label("No disks found. Run as Administrator.");
            if refresh_disks_button(ui, &self.palette).clicked() {
                self.refresh_disks();
            }
            return;
        }

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Select target").font(fonts::bold(16.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if refresh_disks_button(ui, &self.palette).clicked() {
                    self.refresh_disks();
                    // After a refresh the previously-selected disk may
                    // have vanished (drive yanked, virtual disk
                    // unmounted). Snap to the first disk in the new list
                    // so the dropdown's `selected_text` always reflects
                    // a real entry rather than stranding the user on a
                    // ghost index.
                    if !self
                        .disks
                        .iter()
                        .any(|d| d.index == self.target_disk_index)
                    {
                        if let Some(first) = self.disks.first() {
                            self.target_disk_index = first.index;
                        }
                    }
                }
            });
        });
        ui.label(
            egui::RichText::new("Choose the physical disk to restore the backup onto.")
                .color(self.palette.subtle_text),
        );
        ui.add_space(4.0);

        let selected_label = self
            .disks
            .iter()
            .find(|d| d.index == self.target_disk_index)
            .map(format_disk_choice)
            .unwrap_or_else(|| "Pick a target disk".to_string());

        // `from_id_salt` (rather than `from_label`) keeps the dropdown
        // anonymous: the heading row above already announces what this
        // control is, and a duplicate "Select target" tag next to the
        // box would just be visual noise.
        egui::ComboBox::from_id_salt("restore_target_disk")
            .selected_text(selected_label)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for disk in &self.disks {
                    ui.selectable_value(
                        &mut self.target_disk_index,
                        disk.index,
                        format_disk_choice(disk),
                    );
                }
            });
    }

    /// Greyed-when-busy portion of the Restore page.
    fn ui_restore_form(&mut self, ui: &mut egui::Ui) {
        backup_path_picker(
            ui,
            "Backup file",
            "Path to .phnx file",
            &mut self.restore_backup_path,
        );

        ui.add_space(8.0);
        self.ui_restore_target_picker(ui);
        ui.add_space(4.0);

        if ui.button("Load backup & plan").clicked() {
            let Some(path) = resolve_backup_open_path(&self.restore_backup_path) else {
                self.status = "Load cancelled — no backup file chosen".into();
                return;
            };
            self.restore_backup_path = path.display().to_string();
            match PhnxReader::open(&path) {
                Ok(reader) => {
                    // `self.disks` is the single source of truth — the new
                    // target picker drives `self.target_disk_index` from
                    // exactly that vector, and the user can re-poll Windows
                    // via the picker's Refresh button. No need to re-run
                    // `enumerate_disks` here.
                    let size = self
                        .disks
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
    }

    fn start_restore(&mut self) {
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
        // Surfaces while `run_restore` is doing its pre-flight work
        // (opening the .phnx, validating the plan, enumerating disks,
        // and on a GPT source initializing the target via
        // CREATE_DISK + SET_DRIVE_LAYOUT_EX) before the first
        // `set_phase` call inside the worker thread takes over the
        // status text. The user perceives that gap as "the button
        // didn't do anything", so we say so explicitly.
        self.status = "Preparing to restore, please wait…".into();
        self.job = Some(spawn_restore(RestoreOptions {
            backup_path,
            plan,
            verify_on_restore: true,
            progress: Some(progress),
        }));
    }

    fn ui_verify(&mut self, ui: &mut egui::Ui, busy: bool) {
        page_header(
            ui,
            &self.palette,
            "Verify Backup",
            "Run a quick header check or a full BLAKE3 verification across the archive.",
        );

        ui.add_enabled_ui(!busy, |ui| {
            backup_path_picker(
                ui,
                "Backup file",
                "Path to .phnx file",
                &mut self.restore_backup_path,
            );
        });

        let path_filled = !self.restore_backup_path.trim().is_empty();
        let disabled_hint = if busy {
            "A verify is already running"
        } else {
            "Choose a backup file first"
        };
        let starts = [
            StartAction {
                label: "Quick verify",
                enabled: !busy && path_filled,
                disabled_hint: Some(disabled_hint),
            },
            StartAction {
                label: "Full verify",
                enabled: !busy && path_filled,
                disabled_hint: Some(disabled_hint),
            },
        ];
        let (start_clicked, cancel_clicked) =
            action_row(ui, &self.palette, ACTION_BUTTON_HEIGHT, &starts, busy);
        match start_clicked {
            Some(0) => self.start_verify(true),
            Some(1) => self.start_verify(false),
            _ => {}
        }
        if cancel_clicked {
            self.cancel_current_job();
        }
    }

    fn start_verify(&mut self, quick: bool) {
        let Some(path) = resolve_backup_open_path(&self.restore_backup_path) else {
            self.status = "Verify cancelled — no backup file chosen".into();
            return;
        };
        self.restore_backup_path = path.display().to_string();
        self.status = if quick {
            "Quick verify in progress…".into()
        } else {
            "Full verify in progress…".into()
        };
        self.job = Some(spawn_verify(path, quick));
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

fn default_backup_folder() -> String {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return format!(r"{profile}\Documents");
    }
    String::new()
}

/// Render a disk as the `"Disk N - 3.67 TB - Samsung SSD 970 EVO"` label
/// used by the Restore page's target dropdown. The model segment is
/// elided when `DiskInfo::model` is `None` (the IOCTL queried in
/// `phoenix-core` failed) so the dropdown collapses cleanly to
/// `"Disk N - 3.67 TB"` rather than printing a meaningless dangling
/// dash.
fn format_disk_choice(disk: &DiskInfo) -> String {
    match disk.model.as_deref() {
        Some(model) if !model.is_empty() => {
            format!("Disk {} - {} - {}", disk.index, format_bytes(disk.size_bytes), model)
        }
        _ => format!("Disk {} - {}", disk.index, format_bytes(disk.size_bytes)),
    }
}

/// Insert `-disk<N>` before the extension of `base`. Used when the user
/// picked one save path but selected partitions from multiple physical
/// disks, so each disk gets its own `.phnx` next to the chosen file.
fn suffix_path(base: &Path, disk_index: u32) -> PathBuf {
    let parent = base.parent().unwrap_or_else(|| Path::new(""));
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("backup");
    let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("phnx");
    parent.join(format!("{stem}-disk{disk_index}.{ext}"))
}

/// Trim the user-entered backup name and strip a trailing `.phnx` (any
/// case) so that the final output is always `<name>.phnx` rather than
/// `<name>.phnx.phnx` when someone helpfully types the extension.
fn sanitize_backup_name(name: &str) -> String {
    let trimmed = name.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 5 && bytes[bytes.len() - 5..].eq_ignore_ascii_case(b".phnx") {
        // Safe: `.phnx` is pure ASCII, so the last 5 bytes are 1-byte chars
        // and `len - 5` is on a UTF-8 boundary regardless of earlier glyphs.
        trimmed[..trimmed.len() - 5].trim_end().to_string()
    } else {
        trimmed.to_string()
    }
}

fn pick_backup_save_folder(current: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Choose backup folder");

    let path = Path::new(current);
    if path.is_dir() {
        dialog = dialog.set_directory(path);
    } else if let Some(parent) = path.parent().filter(|p| p.exists()) {
        dialog = dialog.set_directory(parent);
    } else if let Ok(profile) = std::env::var("USERPROFILE") {
        dialog = dialog.set_directory(format!(r"{profile}\Documents"));
    }

    dialog.pick_folder()
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
