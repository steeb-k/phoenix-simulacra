mod confirm_dialog;
mod disk_map;
mod disk_panel;
mod fonts;
mod job;
mod restore_layout;
mod restore_panel;
mod sidebar;
mod status_modal;
mod theme;
mod util;
mod version;

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
use phoenix_restore::restore::RestoreOptions;

use crate::confirm_dialog::{ConfirmAction, ConfirmView};
use crate::job::{spawn_backup, spawn_clone, spawn_restore, spawn_verify, BackgroundJob, JobKind};
use crate::sidebar::Page;
use crate::status_modal::{CompletedJob, JobOutcome, ModalAction, ModalView};
use crate::theme::Palette;
use crate::util::format_bytes;

const THEME_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Debounce before auto-loading a `.phnx` path typed into the Restore page.
const RESTORE_BACKUP_LOAD_DEBOUNCE: Duration = Duration::from_millis(300);
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

/// Decode the embedded application icon for the OS window chrome (title
/// bar / taskbar / Alt-Tab thumbnail). This is intentionally a *separate*
/// asset from the in-app UI icons (phosphor glyphs rendered via
/// `egui_phosphor`) — the OS-level icon is a full-color raster that needs
/// to look right at 16x16/32x32/256x256 against the taskbar/title bar,
/// while the in-app icons are vector glyphs tinted by the active theme.
fn app_icon() -> egui::IconData {
    let bytes = include_bytes!("../../carbon-phoenix_appIcon.ico");
    let image =
        image::load_from_memory(bytes).expect("failed to decode carbon-phoenix_appIcon.ico");
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
///
/// The default filter is deliberately *verbose* — `info` for every Phoenix
/// crate — so a fresh log file is immediately useful even if the user never
/// touches `RUST_LOG`. A previous filter of
/// `"phoenix_core=info,phoenix_gui=info,warn"` made successful backups look
/// silent (no log writes between startup and shutdown) which led us down
/// multiple "the binary must be stale" rabbit holes. Anything truly noisy
/// (per-chunk debug) is gated behind explicit `debug!`/`trace!` levels.
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "phoenix_core=info,phoenix_gui=info,phoenix_capture=info,\
             phoenix_restore=info,phoenix_vss=info,phoenix_build=info,warn"
            .into()
    });

    let log_dir = std::env::var("LOCALAPPDATA")
        .map(|p| {
            std::path::PathBuf::from(p)
                .join("CarbonPhoenix")
                .join("logs")
        })
        .unwrap_or_else(|_| std::env::temp_dir().join("CarbonPhoenix").join("logs"));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true);

    let (file_layer, guard) = match std::fs::create_dir_all(&log_dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::daily(&log_dir, "carbon-phoenix.log");
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

    // The build banner must come first so every log file (and every
    // user-pasted snippet) starts with "this is binary X, built at Y,
    // commit Z". Before this, diagnosing "did my fix even land in the
    // binary the user is running?" required cross-referencing exe
    // mtimes and was easy to get wrong when multiple builds existed.
    phoenix_core::build_info::log_startup_banner(&crate::version::BUILD_INFO);

    if guard.is_some() {
        tracing::info!(log_dir = %log_dir.display(), "log file initialized");
    }
    guard
}

/// A validated backup queue parked while the overwrite-confirmation dialog is
/// up. `queue` is in disk-index order (not yet reversed for the pop-from-back
/// runner); `existing` is the subset of output paths that already exist on
/// disk, shown to the user so they know exactly what they'd replace.
struct PendingOverwrite {
    queue: Vec<BackupOptions>,
    existing: Vec<String>,
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
    /// A fully-built, validated backup queue that is waiting on the user to
    /// confirm overwriting one or more existing `.phnx` files. `Some` puts the
    /// overwrite-confirmation dialog on screen; cleared on confirm (launch) or
    /// cancel (abort).
    pending_overwrite: Option<PendingOverwrite>,
    /// Total number of jobs in the current multi-disk run (for "Disk N of M"
    /// status text). Reset to 0 when the queue drains.
    total_backups: usize,
    /// 1-based index of the job currently running for the status text.
    current_backup_index: usize,
    restore_backup_path: String,
    /// Path we last successfully loaded (skip redundant opens).
    restore_loaded_path: String,
    /// When the user edits the backup path, load after this instant.
    restore_backup_load_after: Option<Instant>,
    /// Set by Browse to load immediately on the next frame.
    restore_backup_load_now: bool,
    restore_layout: Option<restore_layout::RestoreLayoutState>,
    target_disk_index: u32,
    /// Clone page state: chosen source/target disk indices and options.
    clone_source_index: Option<u32>,
    clone_target_index: Option<u32>,
    clone_expand: bool,
    /// Mount page: chosen .phnx and the currently-attached read-only mounts.
    mount_backup_path: String,
    /// Path whose layout is currently shown on the Mount page (skip
    /// redundant re-opens).
    mount_loaded_path: String,
    /// The chosen backup's partition layout, for the selection map.
    mount_source: Option<DiskInfo>,
    /// Partitions picked on the mount map (`(0, partition_index)` — the
    /// synthesized source disk always renders as disk 0).
    mount_selection: HashSet<(u32, u32)>,
    mounts: Vec<phoenix_mount::ActiveMount>,
    /// Persisted user settings and job history (loaded at startup).
    settings: phoenix_core::appdata::Settings,
    history: phoenix_core::appdata::History,
    /// Wall-clock start of the currently-running job, for the history record.
    job_started: Option<Instant>,
    status: String,
    page: Page,
    job: Option<BackgroundJob>,
    /// Finished-job snapshot that keeps the status modal up (final step list
    /// and colored Close button) until the user dismisses it. `None` once
    /// dismissed or while a job is still running.
    completed: Option<CompletedJob>,
    palette: Palette,
    last_theme_refresh: Instant,
}

impl PhoenixApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        fonts::install(&cc.egui_ctx);

        let palette = theme::refresh(&cc.egui_ctx);

        let settings = phoenix_core::appdata::Settings::load();
        let backup_folder = settings
            .default_backup_dir
            .clone()
            .unwrap_or_else(default_backup_folder);
        let mut app = Self {
            disks: Vec::new(),
            selections: HashSet::new(),
            backup_folder,
            backup_name: String::new(),
            use_vss: false,
            pending_backups: Vec::new(),
            pending_overwrite: None,
            total_backups: 0,
            current_backup_index: 0,
            restore_backup_path: String::new(),
            restore_loaded_path: String::new(),
            restore_backup_load_after: None,
            restore_backup_load_now: false,
            restore_layout: None,
            target_disk_index: 0,
            clone_source_index: None,
            clone_target_index: None,
            clone_expand: false,
            mount_backup_path: String::new(),
            mount_loaded_path: String::new(),
            mount_source: None,
            mount_selection: HashSet::new(),
            mounts: Vec::new(),
            settings,
            history: phoenix_core::appdata::History::load(),
            job_started: None,
            status: "Ready".into(),
            page: Page::Backup,
            job: None,
            completed: None,
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

    /// True while the blocking status modal is on screen (running job or
    /// finished result awaiting Close).
    fn modal_open(&self) -> bool {
        self.job.is_some() || self.completed.is_some() || self.pending_overwrite.is_some()
    }

    fn clear_restore_ui_state(&mut self) {
        self.restore_loaded_path.clear();
        self.restore_backup_load_after = None;
        self.restore_backup_load_now = false;
        self.restore_layout = None;
    }

    fn try_load_restore_backup(&mut self) {
        let path_str = self.restore_backup_path.trim();
        if path_str.is_empty() {
            self.clear_restore_ui_state();
            return;
        }
        let path = Path::new(path_str);
        if !path.is_file() {
            return;
        }
        if self.restore_loaded_path == path_str {
            return;
        }
        match PhnxReader::open(path) {
            Ok(reader) => {
                self.restore_layout = Some(restore_layout::RestoreLayoutState::from_backup(
                    path_str, &reader,
                ));
                self.restore_loaded_path = path_str.to_string();
                let n = reader.index.len();
                self.status = format!("Loaded backup — {n} partition(s)");
            }
            Err(e) => {
                self.restore_loaded_path.clear();
                self.restore_layout = None;
                self.status = format!("Load failed: {e}");
            }
        }
    }

    fn schedule_restore_backup_load(&mut self) {
        self.restore_backup_load_after = Some(Instant::now() + RESTORE_BACKUP_LOAD_DEBOUNCE);
    }

    fn poll_restore_backup_load(&mut self) {
        if self.restore_backup_load_now {
            self.restore_backup_load_now = false;
            self.restore_backup_load_after = None;
            self.try_load_restore_backup();
            return;
        }
        if let Some(deadline) = self.restore_backup_load_after {
            if Instant::now() >= deadline {
                self.restore_backup_load_after = None;
                self.try_load_restore_backup();
            }
        }
    }

    /// Append a completed-job record to the persistent history (best-effort).
    fn record_job(
        &mut self,
        kind: JobKind,
        outcome: phoenix_core::appdata::JobOutcome,
        snap: &phoenix_core::ProgressSnapshot,
    ) {
        use phoenix_core::appdata::{JobKindTag, JobRecord};
        let tag = match kind {
            JobKind::Backup => JobKindTag::Backup,
            JobKind::Restore => JobKindTag::Restore,
            JobKind::Verify => JobKindTag::Verify,
            JobKind::Clone => JobKindTag::Clone,
        };
        let duration = self.job_started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
        let rec = JobRecord::now(
            tag,
            duration,
            String::new(),
            self.status.clone(),
            None,
            outcome,
            snap.total,
        );
        let _ = self.history.append(rec);
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        if self.job.is_some() && self.job_started.is_none() {
            self.job_started = Some(Instant::now());
        }
        let Some(job) = self.job.as_mut() else {
            return;
        };
        let job_kind = job.kind;
        let job_verified = job.verify_after;
        let job_output = job.output.clone();
        if let Some(result) = job.poll() {
            // Capture the final step list before dropping the job so the
            // modal can keep showing it (with a Close button) after the
            // worker thread is gone.
            let final_snap = job.progress.snapshot();
            self.job = None;
            match result {
                Ok(msg) => {
                    if job_kind == JobKind::Verify && self.completed.is_some() {
                        // The follow-up "Verify?" of a just-completed backup:
                        // upgrade the parked completion modal in place instead
                        // of replacing it with the verify job's own step list.
                        self.status = msg;
                        self.record_job(
                            job_kind,
                            phoenix_core::appdata::JobOutcome::Success,
                            &final_snap,
                        );
                        self.job_started = None;
                        if let Some(completed) = self.completed.as_mut() {
                            completed.success_banner = Some("Completed and verified.".to_string());
                        }
                    } else if let Some(next) = self.pending_backups.pop() {
                        self.current_backup_index += 1;
                        let path_display = next.output.display().to_string();
                        self.status = format!(
                            "Backing up disk {} of {} to {}…",
                            self.current_backup_index, self.total_backups, path_display
                        );
                        self.job = Some(spawn_backup(next));
                    } else {
                        let multi = self.total_backups > 1;
                        self.status = if multi {
                            format!("All {} backups completed", self.total_backups)
                        } else {
                            msg
                        };
                        if job_kind == JobKind::Restore {
                            self.clear_restore_ui_state();
                        }
                        self.total_backups = 0;
                        self.current_backup_index = 0;
                        self.record_job(
                            job_kind,
                            phoenix_core::appdata::JobOutcome::Success,
                            &final_snap,
                        );
                        self.job_started = None;
                        // Offer "Verify?" only for a single unverified backup:
                        // for a multi-disk run the parked modal represents the
                        // whole queue, but the job only knows its own file.
                        let verify_target =
                            (job_kind == JobKind::Backup && !job_verified && !multi)
                                .then_some(job_output)
                                .flatten();
                        self.finish_modal(
                            job_kind,
                            &final_snap,
                            JobOutcome::Success,
                            job_verified,
                            verify_target,
                        );
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
                    let outcome = if cancelled {
                        JobOutcome::Warning
                    } else {
                        JobOutcome::Failure
                    };
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
                    let rec_outcome = if cancelled {
                        phoenix_core::appdata::JobOutcome::Cancelled
                    } else {
                        phoenix_core::appdata::JobOutcome::Failed(e.clone())
                    };
                    self.record_job(job_kind, rec_outcome, &final_snap);
                    self.job_started = None;
                    self.finish_modal(job_kind, &final_snap, outcome, job_verified, None);
                }
            }
        } else {
            ctx.request_repaint();
        }
    }

    /// Park the just-finished job's final step list in `self.completed` so the
    /// modal stays up until the user clicks Close. The headline message reuses
    /// the status text we just computed.
    fn finish_modal(
        &mut self,
        kind: JobKind,
        snap: &phoenix_core::ProgressSnapshot,
        outcome: JobOutcome,
        verified_backup: bool,
        verify_target: Option<std::path::PathBuf>,
    ) {
        // A successful backup swaps the progress bar + checklist for a big
        // green checkmark; the banner text records whether the verify pass ran.
        let success_banner =
            (kind == JobKind::Backup && outcome == JobOutcome::Success).then(|| {
                if verified_backup {
                    "Completed and verified.".to_string()
                } else {
                    "Completed. Unverified.".to_string()
                }
            });
        self.completed = Some(CompletedJob {
            title: kind.noun().to_string(),
            steps: snap.steps.clone(),
            current_step: snap.current_step,
            outcome,
            message: self.status.clone(),
            success_banner,
            verify_target,
        });
    }

    /// "Verify?" on an unverified completed backup: append a verify line to
    /// the parked checklist and run a full image verify of the file just
    /// written. While it runs, `show_status_modal` renders the parked backup
    /// steps with the verify job's live progress; on success `poll_job`
    /// upgrades the banner to "Completed and verified." in place.
    fn start_completed_verify(&mut self) {
        let Some(completed) = self.completed.as_mut() else {
            return;
        };
        let Some(path) = completed.verify_target.take() else {
            return;
        };
        completed.steps.push("Verifying backup".to_string());
        completed.current_step = completed.steps.len() - 1;
        self.status = format!("Verifying {}…", path.display());
        self.job = Some(spawn_verify(path, false));
        self.job_started = None;
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
            let running_job_modal = self.job.is_some();
            self.palette = theme::refresh(ctx);
            self.last_theme_refresh = Instant::now();
            // Only surrender keyboard focus while a job is actively running.
            // Doing so on the finished "Close" modal can interfere with
            // dismissing it after sleep/focus changes.
            if running_job_modal {
                ctx.memory_mut(|mem| {
                    if let Some(id) = mem.focused() {
                        mem.surrender_focus(id);
                    }
                });
            }
        }
    }
}

impl eframe::App for PhoenixApp {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_job(ctx);
        self.maybe_refresh_theme(ctx);
        if self.page == Page::Restore {
            self.poll_restore_backup_load();
        }

        let focus_regained = ctx.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::WindowFocused(true)))
        });
        if focus_regained && self.completed.is_some() {
            ctx.request_repaint();
        }

        let busy = self.busy();
        let modal_open = self.modal_open();
        sidebar::show(ctx, &mut self.page, &self.palette, modal_open);

        // Slim bottom status line for idle/non-job messages (disk count,
        // "Loaded backup…", load errors, "Ready"). While a job runs or its
        // result modal is up, the blocking modal overlays this bar.
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::none()
                    .fill(self.palette.sidebar_bg)
                    .inner_margin(egui::Margin::symmetric(16.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(!modal_open, |ui| {
                    ui.label(egui::RichText::new(&self.status).color(self.palette.subtle_text));
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(24.0, 20.0)),
            )
            .show(ctx, |ui| {
                // Pages handle `busy` themselves so Start stays gated while
                // a job runs; `modal_open` disables the whole panel so nothing
                // behind the status modal receives clicks or scroll.
                ui.add_enabled_ui(!modal_open, |ui| match self.page {
                    Page::Backup => self.ui_backup(ui, busy),
                    Page::Clone => disabled_when(ui, busy, |ui| self.ui_clone(ui)),
                    Page::Restore => self.ui_restore(ui, busy),
                    Page::Verify => self.ui_verify(ui, busy),
                    Page::Mount => disabled_when(ui, busy, |ui| self.ui_mount(ui)),
                    Page::History => disabled_when(ui, busy, |ui| self.ui_history(ui)),
                    Page::Options => disabled_when(ui, busy, |ui| self.ui_options(ui)),
                });
            });

        self.show_status_modal(ctx);
        self.show_overwrite_dialog(ctx);
    }
}

impl PhoenixApp {
    /// Render the blocking status modal when a job is running or its result
    /// is awaiting dismissal, then act on the user's Cancel/Close click.
    fn show_status_modal(&mut self, ctx: &egui::Context) {
        let mut action = ModalAction::None;
        if let (Some(completed), Some(job)) = (&self.completed, &self.job) {
            // A follow-up "Verify?" is running over a parked completed backup
            // (the only state where both exist): show the backup's finished
            // checklist — with the "Verifying backup" line appended by
            // `start_completed_verify` — driven by the verify job's live
            // progress bar and detail.
            let snap = job.progress.snapshot();
            let view = ModalView {
                title: "Verifying",
                steps: &completed.steps,
                current_step: completed.current_step,
                detail: &snap.detail,
                fraction: snap.fraction(),
                current_bytes: snap.current,
                total_bytes: snap.total,
                outcome: None,
                success_banner: None,
                offer_verify: false,
            };
            action = status_modal::show(ctx, &self.palette, &view);
        } else if let Some(completed) = &self.completed {
            let view = ModalView {
                title: &completed.title,
                steps: &completed.steps,
                current_step: completed.current_step,
                detail: &completed.message,
                fraction: 1.0,
                current_bytes: 0,
                total_bytes: 0,
                outcome: Some(completed.outcome),
                success_banner: completed.success_banner.as_deref(),
                offer_verify: completed.verify_target.is_some(),
            };
            action = status_modal::show(ctx, &self.palette, &view);
        } else if let Some(job) = &self.job {
            let snap = job.progress.snapshot();
            // A verifying backup is no longer "Backing up": the verify pass is
            // the last step of the plan (only present when verify-after is on),
            // so the headline flips once the job reaches it.
            let title = if job.kind == JobKind::Backup
                && job.verify_after
                && !snap.steps.is_empty()
                && snap.current_step + 1 == snap.steps.len()
            {
                "Verifying"
            } else {
                job.kind.title()
            };
            let view = ModalView {
                title,
                steps: &snap.steps,
                current_step: snap.current_step,
                detail: &snap.detail,
                fraction: snap.fraction(),
                current_bytes: snap.current,
                total_bytes: snap.total,
                outcome: None,
                success_banner: None,
                offer_verify: false,
            };
            action = status_modal::show(ctx, &self.palette, &view);
        }

        match action {
            ModalAction::Cancel => self.cancel_current_job(),
            ModalAction::Close => self.completed = None,
            ModalAction::Verify => self.start_completed_verify(),
            ModalAction::None => {}
        }
    }

    /// Render the "file already exists" confirmation over a parked backup queue.
    /// Confirm launches it (overwriting); Cancel drops the queue and leaves the
    /// backup form untouched so the user can rename and try again.
    fn show_overwrite_dialog(&mut self, ctx: &egui::Context) {
        if self.pending_overwrite.is_none() {
            return;
        }
        let action = {
            let pending = self.pending_overwrite.as_ref().unwrap();
            let count = pending.existing.len();
            let message = if count == 1 {
                "A backup file with this name already exists. Overwrite it?".to_string()
            } else {
                format!("{count} backup files with these names already exist. Overwrite them?")
            };
            let view = ConfirmView {
                title: "Backup file already exists",
                message: &message,
                details: &pending.existing,
                confirm_label: "Overwrite",
                cancel_label: "Cancel",
                confirm_danger: true,
            };
            confirm_dialog::show(ctx, &self.palette, &view)
        };

        match action {
            ConfirmAction::Confirm => {
                let pending = self.pending_overwrite.take().expect("dialog was open");
                self.launch_backup_queue(pending.queue);
            }
            ConfirmAction::Cancel => {
                self.pending_overwrite = None;
                self.status =
                    "Backup cancelled — choose a different name to keep the old one".into();
            }
            ConfirmAction::None => {}
        }
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

/// Format a duration in seconds as a compact "N units ago" string for the
/// history list.
fn relative_time(secs_ago: i64) -> String {
    let s = secs_ago.max(0);
    if s < 60 {
        "just now".to_string()
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else if s < 86_400 {
        format!("{} hr ago", s / 3600)
    } else {
        format!("{} days ago", s / 86_400)
    }
}

/// Truncate a string to `max` chars with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Render `starts` (one or more green Start-style buttons) in a row.
/// Returns `Some(start_idx)` when one of them was clicked, `None` otherwise.
///
/// Cancellation lives in the blocking status modal now (its Cancel button
/// calls [`PhoenixApp::cancel_current_job`]), so this row no longer renders
/// a per-page Cancel control.
///
/// All buttons land at `FORM_BUTTON_W` × `height`. Disabled colored
/// buttons stay tinted (faded, via `Palette::dim`) instead of going
/// fully grey so the user still recognizes the Go control.
fn action_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    height: f32,
    starts: &[StartAction<'_>],
) -> Option<usize> {
    let mut clicked_start: Option<usize> = None;
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
    });
    clicked_start
}

/// Bold "label + TextEdit + Browse…" row used by both the Verify and Restore
/// pages for picking an existing `.phnx` file. Mirrors the styling of the
/// "Save backup to folder" row on the Backup page: 14pt bold label, 16pt
/// field text with `(10, 8)` margins, and a `Browse…` button whose width
/// is reserved on the right so it never falls off the side of the pane.
///
/// When `on_path_changed` is `Some`, it is called after the text field
/// loses focus with a changed value, and immediately after Browse picks a file.
/// Returns `true` if Browse selected a new file.
fn backup_path_picker(
    ui: &mut egui::Ui,
    label: &str,
    hint: &str,
    path: &mut String,
    mut on_path_changed: Option<&mut dyn FnMut()>,
) -> bool {
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
        if let Some(cb) = on_path_changed.as_deref_mut() {
            if field_response.lost_focus() && field_response.changed() {
                cb();
            }
        }

        // Outer (visible) height of the TextEdit — the response rect is
        // the inner text area, so add back the vertical margin to get the
        // height the user actually sees and size Browse to match.
        let visible_h = field_response.rect.height() + INPUT_MARGIN_RESTORE;

        let mut browsed = false;
        if ui
            .add_sized(
                [FORM_BUTTON_W, visible_h],
                egui::Button::new(egui::RichText::new("Browse…").font(fonts::regular(16.0))),
            )
            .clicked()
        {
            if let Some(picked) = pick_backup_open_path(path) {
                *path = picked.display().to_string();
                browsed = true;
                if let Some(cb) = on_path_changed {
                    cb();
                }
            }
        }
        browsed
    })
    .inner
}

fn page_header(ui: &mut egui::Ui, palette: &Palette, title: &str, subtitle: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(title).font(fonts::bold(22.0)));
    ui.label(egui::RichText::new(subtitle).color(palette.subtle_text));
    ui.add_space(14.0);
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
            .add_enabled_ui(!busy, |ui| self.ui_backup_form(ui, name_missing))
            .inner;

        let input_height = name_response.rect.height() + INPUT_MARGIN_RESTORE;

        let starts = [StartAction {
            label: "Start backup",
            enabled: !busy && !name_missing,
            disabled_hint: Some(if busy {
                "A backup is already running"
            } else {
                "Enter a backup name first"
            }),
        }];
        if action_row(ui, &self.palette, input_height, &starts) == Some(0) {
            self.start_backup();
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
                    let error_stroke = egui::Stroke::new(1.5, self.palette.danger);
                    ui.visuals_mut().widgets.inactive.bg_stroke = error_stroke;
                    ui.visuals_mut().widgets.hovered.bg_stroke = error_stroke;
                    ui.visuals_mut().widgets.active.bg_stroke = error_stroke;
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

        if name_missing {
            ui.label(
                egui::RichText::new("Required")
                    .color(self.palette.danger)
                    .size(12.0),
            );
        }

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
                    egui::Button::new(egui::RichText::new("Browse…").font(fonts::regular(16.0))),
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
            self.status = format!("Backup folder does not exist: {}", folder.display());
            return;
        }
        if !folder.is_dir() {
            self.status = format!("Backup destination is not a folder: {}", folder.display());
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
                verify_after: self.settings.verify_after_backup,
                // Full BLAKE3 hash of the written image — the GUI never runs
                // the source read-back comparison (CLI-only).
                verify_image: true,
                progress: Some(ProgressHandle::new()),
            });
        }

        // If any target file already exists, don't silently clobber it — park
        // the built queue and ask the user first (in-app dialog, not a native
        // one). Otherwise launch straight away.
        let existing: Vec<String> = queue
            .iter()
            .filter(|o| o.output.exists())
            .map(|o| o.output.display().to_string())
            .collect();
        if existing.is_empty() {
            self.launch_backup_queue(queue);
        } else {
            self.pending_overwrite = Some(PendingOverwrite { queue, existing });
        }
    }

    /// Kick off a fully-built backup queue: the first job starts immediately and
    /// the rest wait in `pending_backups` (drained by `poll_job`). `queue` must
    /// be in disk-index order — this reverses it internally so the pop-from-back
    /// runner preserves that order.
    fn launch_backup_queue(&mut self, mut queue: Vec<BackupOptions>) {
        self.completed = None;
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

        let plan_ready = self
            .restore_layout
            .as_ref()
            .is_some_and(|l| l.has_restorable_entries());
        let starts = [StartAction {
            label: "Run restore",
            enabled: !busy && plan_ready,
            disabled_hint: Some(if busy {
                "A restore is already running"
            } else {
                "Choose a backup file first"
            }),
        }];
        if action_row(ui, &self.palette, ACTION_BUTTON_HEIGHT, &starts) == Some(0) {
            self.start_restore();
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
                    if !self.disks.iter().any(|d| d.index == self.target_disk_index) {
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
        let path_before = self.restore_backup_path.clone();
        let mut path_edited = false;
        let browsed = backup_path_picker(
            ui,
            "Source media",
            "Path to .phnx file",
            &mut self.restore_backup_path,
            Some(&mut || path_edited = true),
        );
        if browsed {
            self.restore_backup_load_now = true;
        } else if path_edited && self.restore_backup_path != path_before {
            if self.restore_backup_path.trim().is_empty() {
                self.clear_restore_ui_state();
            } else {
                self.schedule_restore_backup_load();
            }
        }

        ui.add_space(8.0);
        let prev_target = self.target_disk_index;
        self.ui_restore_target_picker(ui);
        if self.target_disk_index != prev_target && !self.restore_backup_path.trim().is_empty() {
            self.restore_loaded_path.clear();
            self.restore_backup_load_now = true;
        }
        ui.add_space(4.0);

        if let Some(target) = self
            .disks
            .iter()
            .find(|d| d.index == self.target_disk_index)
            .cloned()
        {
            if let Some(layout) = self.restore_layout.as_mut() {
                let panel_out =
                    restore_panel::show(ui, layout, &target, &self.palette, ui.available_width());
                if panel_out.plan_entries_updated {
                    // layout drives plan at run time
                }
            }
        } else if self.restore_layout.is_some() {
            ui.label(
                egui::RichText::new("Select a target disk to map partitions.")
                    .color(self.palette.subtle_text),
            );
        }
    }

    fn start_restore(&mut self) {
        let Some(backup_path) = resolve_backup_open_path(&self.restore_backup_path) else {
            self.status = "Restore cancelled — no backup file chosen".into();
            return;
        };
        self.restore_backup_path = backup_path.display().to_string();
        let Some(layout) = self.restore_layout.as_ref() else {
            self.status = "No restore layout — load a backup first".into();
            return;
        };
        let Some(target) = self
            .disks
            .iter()
            .find(|d| d.index == self.target_disk_index)
        else {
            self.status = "Target disk not found".into();
            return;
        };
        let plan = layout.to_restore_plan(target);
        if let Ok(mut reader) = PhnxReader::open(&backup_path) {
            if let Err(e) = plan.validate_against_backup(&reader.manifest) {
                self.status = format!("Plan invalid: {e}");
                return;
            }
            if let Err(e) = plan.validate_extents_fit(&mut reader) {
                self.status = format!("Plan invalid: {e}");
                return;
            }
        }
        let progress = ProgressHandle::new();
        // Surfaces while `run_restore` is doing its pre-flight work
        // (opening the .phnx, validating the plan, enumerating disks,
        // and on a GPT source initializing the target via
        // CREATE_DISK + SET_DRIVE_LAYOUT_EX) before the first
        // `set_phase` call inside the worker thread takes over the
        // status text.
        self.completed = None;
        self.status = "Restoring, please wait…".into();
        self.job = Some(spawn_restore(RestoreOptions {
            backup_path,
            plan,
            // Read-back verification is CLI-only; the image's chunk hashes
            // were already checked while decompressing.
            verify_on_restore: false,
            progress: Some(progress),
        }));
    }

    fn ui_verify(&mut self, ui: &mut egui::Ui, busy: bool) {
        page_header(
            ui,
            &self.palette,
            "Verify Backup",
            "Checks the backup's structure, then decompresses and BLAKE3-checks every chunk.",
        );

        ui.add_enabled_ui(!busy, |ui| {
            let _ = backup_path_picker(
                ui,
                "Backup file",
                "Path to .phnx file",
                &mut self.restore_backup_path,
                None,
            );
        });

        let path_filled = !self.restore_backup_path.trim().is_empty();
        let disabled_hint = if busy {
            "A verify is already running"
        } else {
            "Choose a backup file first"
        };
        let starts = [StartAction {
            label: "Verify backup",
            enabled: !busy && path_filled,
            disabled_hint: Some(disabled_hint),
        }];
        if action_row(ui, &self.palette, ACTION_BUTTON_HEIGHT, &starts) == Some(0) {
            self.start_verify();
        }
    }

    fn start_verify(&mut self) {
        let Some(path) = resolve_backup_open_path(&self.restore_backup_path) else {
            self.status = "Verify cancelled — no backup file chosen".into();
            return;
        };
        self.restore_backup_path = path.display().to_string();
        self.completed = None;
        self.status = "Verify in progress…".into();
        self.job = Some(spawn_verify(path, false));
    }

    fn ui_clone(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "Clone",
            "Copy one disk directly to another without going through a backup file.",
        );

        let busy = self.busy();
        if self.disks.is_empty() {
            ui.label("No disks detected. Run as Administrator, then refresh.");
            if refresh_disks_button(ui, &self.palette).clicked() {
                self.refresh_disks();
            }
            return;
        }

        // Build a stable list of (index, label) for the dropdowns.
        let disk_labels: Vec<(u32, String)> = self
            .disks
            .iter()
            .map(|d| {
                (
                    d.index,
                    format!(
                        "Disk {} — {} ({})",
                        d.index,
                        d.model.as_deref().unwrap_or("disk"),
                        format_bytes(d.size_bytes)
                    ),
                )
            })
            .collect();

        ui.add_enabled_ui(!busy, |ui| {
            egui::Grid::new("clone_grid").num_columns(2).show(ui, |ui| {
                ui.label("Source disk:");
                let src_label = self
                    .clone_source_index
                    .and_then(|i| disk_labels.iter().find(|(d, _)| *d == i))
                    .map(|(_, l)| l.clone())
                    .unwrap_or_else(|| "Choose…".into());
                egui::ComboBox::from_id_salt("clone_src")
                    .selected_text(src_label)
                    .show_ui(ui, |ui| {
                        for (idx, label) in &disk_labels {
                            ui.selectable_value(&mut self.clone_source_index, Some(*idx), label);
                        }
                    });
                ui.end_row();

                ui.label("Target disk:");
                let tgt_label = self
                    .clone_target_index
                    .and_then(|i| disk_labels.iter().find(|(d, _)| *d == i))
                    .map(|(_, l)| l.clone())
                    .unwrap_or_else(|| "Choose…".into());
                egui::ComboBox::from_id_salt("clone_tgt")
                    .selected_text(tgt_label)
                    .show_ui(ui, |ui| {
                        for (idx, label) in &disk_labels {
                            // Never offer the source as a target.
                            if Some(*idx) != self.clone_source_index {
                                ui.selectable_value(
                                    &mut self.clone_target_index,
                                    Some(*idx),
                                    label,
                                );
                            }
                        }
                    });
                ui.end_row();
            });

            ui.checkbox(
                &mut self.clone_expand,
                "Expand the last NTFS partition to fill a larger target",
            );
        });

        ui.add_space(8.0);
        let ready = !busy
            && self.clone_source_index.is_some()
            && self.clone_target_index.is_some()
            && self.clone_source_index != self.clone_target_index;

        if let Some(tgt) = self.clone_target_index {
            ui.colored_label(
                self.palette.warning,
                format!("⚠ Cloning will PERMANENTLY ERASE all data on disk {tgt}."),
            );
        }

        ui.add_enabled_ui(ready, |ui| {
            if ui.button("Clone disk").clicked() {
                self.start_clone();
            }
        });
    }

    fn start_clone(&mut self) {
        let (Some(src), Some(tgt)) = (self.clone_source_index, self.clone_target_index) else {
            return;
        };
        let source = self.disks.iter().find(|d| d.index == src).cloned();
        let target = self.disks.iter().find(|d| d.index == tgt).cloned();
        let (Some(source), Some(target)) = (source, target) else {
            self.status = "Selected disk no longer present; refresh".into();
            return;
        };
        let plan = if self.clone_expand {
            phoenix_clone::ClonePlan::expand_to_fill(&source, &target)
        } else {
            phoenix_clone::ClonePlan::identity(&source)
        };
        if let Err(e) = plan.validate(&source, &target) {
            self.status = format!("Cannot clone: {e}");
            return;
        }
        self.status = format!("Cloning disk {src} → disk {tgt}…");
        self.job = Some(spawn_clone(phoenix_clone::CloneOptions {
            source_disk_index: src,
            target_disk_index: tgt,
            plan,
            verify: phoenix_clone::CloneVerify::None,
            use_vss: true,
            progress: None,
        }));
    }

    fn ui_mount(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "Mount",
            "Attach a backup read-only so its files are browsable in Explorer.",
        );

        let path_before = self.mount_backup_path.clone();
        let mut path_edited = false;
        ui.add_enabled_ui(self.job.is_none(), |ui| {
            let browsed = backup_path_picker(
                ui,
                "Backup file",
                "Path to .phnx file",
                &mut self.mount_backup_path,
                Some(&mut || path_edited = true),
            );
            path_edited |= browsed;
        });
        let path_changed = path_edited && self.mount_backup_path != path_before;
        if path_changed
            || (self.mount_source.is_none()
                && self.mount_loaded_path != self.mount_backup_path.trim())
        {
            self.try_load_mount_backup();
        }

        if let Some(source) = self.mount_source.clone() {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Choose the partition(s) to mount.")
                    .color(self.palette.subtle_text),
            );
            ui.add_space(4.0);
            // Same width discipline as the Backup page: capture the real
            // viewport width before ScrollArea::both virtualizes it.
            let viewport_width = ui.available_width();
            let row_width = viewport_width.max(disk_map::min_disk_row_width(&source));
            egui::ScrollArea::both()
                .id_salt("mount_partition_map")
                .max_height(disk_map::row_stride() * 1.5)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    disk_map::draw_backup_disk_row(
                        ui,
                        row_width,
                        &source,
                        &mut self.mount_selection,
                        &self.palette,
                    );
                });

            let any_selected = !self.mount_selection.is_empty();
            ui.add_space(4.0);
            ui.add_enabled_ui(any_selected, |ui| {
                if ui.button("Mount selected read-only").clicked() {
                    self.start_mount();
                }
            });
            if !any_selected {
                ui.label(
                    egui::RichText::new("Select at least one partition above.")
                        .color(self.palette.subtle_text),
                );
            }
        }

        if !self.mounts.is_empty() {
            ui.add_space(12.0);
            ui.heading("Active mounts");
            let mut unmount_idx: Option<usize> = None;
            for (i, m) in self.mounts.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        m.backup_path()
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "backup".into()),
                    );
                    ui.label(format!("({:.1} GB)", m.disk_size() as f64 / 1e9));
                    ui.label(describe_mounted_volumes(m));
                    let first_letter = m.volumes().iter().find_map(|v| v.drive_letter);
                    if let Some(l) = first_letter {
                        if ui.button("Open in Explorer").clicked() {
                            let _ = std::process::Command::new("explorer.exe")
                                .arg(format!("{l}:\\"))
                                .spawn();
                        }
                    }
                    if ui.button("Unmount").clicked() {
                        unmount_idx = Some(i);
                    }
                });
            }
            if let Some(i) = unmount_idx {
                self.mounts.remove(i); // Drop removes letters, detaches, cleans up
                self.status = "Unmounted".into();
            }
        }
    }

    /// (Re)load the chosen backup's partition layout for the mount map. All
    /// partitions start selected.
    fn try_load_mount_backup(&mut self) {
        let path_str = self.mount_backup_path.trim().to_string();
        if path_str.is_empty() {
            self.mount_loaded_path.clear();
            self.mount_source = None;
            self.mount_selection.clear();
            return;
        }
        if self.mount_loaded_path == path_str {
            return;
        }
        // Record the attempt (success or not) so a bad path isn't re-opened
        // every frame.
        self.mount_loaded_path = path_str.clone();
        let path = Path::new(&path_str);
        if !path.is_file() {
            self.mount_source = None;
            self.mount_selection.clear();
            return;
        }
        match PhnxReader::open(path) {
            Ok(reader) => {
                let source = restore_layout::backup_to_disk_info(&reader);
                self.mount_selection = source
                    .partitions
                    .iter()
                    .map(|p| (source.index, p.index))
                    .collect();
                self.mount_source = Some(source);
                self.status = format!("Loaded backup — {} partition(s)", reader.index.len());
            }
            Err(e) => {
                self.mount_source = None;
                self.mount_selection.clear();
                self.status = format!("Load failed: {e}");
            }
        }
    }

    fn start_mount(&mut self) {
        let path = std::path::PathBuf::from(self.mount_backup_path.trim());
        let scratch = std::env::temp_dir().join("CarbonPhoenix").join("mounts");
        let selected: Vec<u32> = self.mount_selection.iter().map(|&(_, part)| part).collect();
        self.status = if phoenix_mount::ActiveMount::space_efficient() {
            "Mounting read-only (on-demand)…".into()
        } else {
            "Materializing and attaching… (this can take a moment)".into()
        };
        // The mount holds a non-Send disk handle, so do it inline. The WinFsp
        // path is instant; the fallback's time scales with the used size.
        match phoenix_mount::ActiveMount::open_selected(&path, &scratch, Some(&selected)) {
            Ok(session) => {
                let letters = describe_mounted_volumes(&session);
                self.status = format!(
                    "Mounted {} — {letters}",
                    path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                );
                self.mounts.push(session);
            }
            Err(e) => {
                self.status = format!("Mount failed: {e}");
            }
        }
    }

    fn ui_history(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "History",
            "Previous backups, restores, clones, and verification runs from this machine.",
        );

        if self.history.records.is_empty() {
            ui.label("No jobs recorded yet.");
            return;
        }

        ui.horizontal(|ui| {
            if ui.button("Clear history").clicked() {
                self.history.records.clear();
                let _ = self.history.save();
            }
        });
        ui.add_space(6.0);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            // Newest first.
            for rec in self.history.records.iter().rev() {
                let (label, color) = match &rec.outcome {
                    phoenix_core::appdata::JobOutcome::Success => ("OK", self.palette.success),
                    phoenix_core::appdata::JobOutcome::Cancelled => {
                        ("Cancelled", self.palette.warning)
                    }
                    phoenix_core::appdata::JobOutcome::Failed(_) => {
                        ("Failed", egui::Color32::from_rgb(0xD3, 0x2F, 0x2F))
                    }
                };
                ui.horizontal(|ui| {
                    ui.colored_label(color, format!("[{label}]"));
                    ui.label(format!("{:?}", rec.kind));
                    ui.label(relative_time(now - rec.started_unix));
                    ui.label(format!("{}s", rec.duration_secs));
                    ui.label(format_bytes(rec.bytes_processed));
                    if let phoenix_core::appdata::JobOutcome::Failed(msg) = &rec.outcome {
                        ui.colored_label(
                            egui::Color32::from_rgb(0xD3, 0x2F, 0x2F),
                            truncate(msg, 60),
                        );
                    }
                });
            }
        });
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            &self.palette,
            "Options",
            "Application preferences and live theme info.",
        );

        // --- Persisted settings ---
        let mut changed = false;
        ui.heading("Preferences");
        ui.horizontal(|ui| {
            ui.label("Default backup folder:");
            let mut dir = self.settings.default_backup_dir.clone().unwrap_or_default();
            if ui.text_edit_singleline(&mut dir).changed() {
                self.settings.default_backup_dir = if dir.trim().is_empty() {
                    None
                } else {
                    Some(dir)
                };
                changed = true;
            }
            if ui.button("Browse…").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.settings.default_backup_dir = Some(path.display().to_string());
                    changed = true;
                }
            }
        });
        changed |= ui
            .checkbox(
                &mut self.settings.verify_after_backup,
                "Verify each backup after it completes",
            )
            .changed();
        ui.horizontal(|ui| {
            ui.label("Theme:");
            use phoenix_core::appdata::ThemeChoice;
            for (choice, label) in [
                (ThemeChoice::System, "System"),
                (ThemeChoice::Dark, "Dark"),
                (ThemeChoice::Light, "Light"),
            ] {
                if ui
                    .selectable_label(self.settings.theme == choice, label)
                    .clicked()
                {
                    self.settings.theme = choice;
                    changed = true;
                }
            }
        });
        if changed {
            let _ = self.settings.save();
        }

        ui.add_space(12.0);
        ui.heading("Theme (live)");
        ui.horizontal(|ui| {
            ui.label("Accent color:");
            let (r, g, b, _) = self.palette.accent.to_tuple();
            let swatch_size = egui::vec2(20.0, 20.0);
            let (rect, _) = ui.allocate_exact_size(swatch_size, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::Rounding::same(4.0), self.palette.accent);
            ui.monospace(format!("#{:02X}{:02X}{:02X}", r, g, b));
        });

        ui.horizontal(|ui| {
            ui.label("Theme:");
            ui.label(if self.palette.light_mode {
                "Light"
            } else {
                "Dark"
            });
        });

        ui.add_space(8.0);
        if ui.button("Refresh theme from Windows").clicked() {
            let running_job_modal = self.job.is_some();
            let ctx = ui.ctx().clone();
            self.palette = theme::refresh(ui.ctx());
            self.last_theme_refresh = Instant::now();
            self.status = "Theme refreshed from Windows settings".into();
            if running_job_modal {
                ctx.memory_mut(|mem| {
                    if let Some(id) = mem.focused() {
                        mem.surrender_focus(id);
                    }
                });
            }
        }
    }
}

fn default_backup_folder() -> String {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return format!(r"{profile}\Documents");
    }
    String::new()
}

/// Short per-partition exposure summary for an active mount, e.g.
/// `"E:, F:"` or `"E: (+1 with no mountable volume)"`.
fn describe_mounted_volumes(m: &phoenix_mount::ActiveMount) -> String {
    let vols = m.volumes();
    if vols.is_empty() {
        return "browse it in Explorer".into();
    }
    let letters: Vec<String> = vols
        .iter()
        .filter_map(|v| v.drive_letter.map(|l| format!("{l}:")))
        .collect();
    let unexposed = vols.len() - letters.len();
    match (letters.is_empty(), unexposed) {
        (true, _) => "no mountable volume in the selection".into(),
        (false, 0) => letters.join(", "),
        (false, n) => format!("{} (+{n} with no mountable volume)", letters.join(", ")),
    }
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
            format!(
                "Disk {} - {} - {}",
                disk.index,
                format_bytes(disk.size_bytes),
                model
            )
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
