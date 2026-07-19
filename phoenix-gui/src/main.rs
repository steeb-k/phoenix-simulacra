// Ship as a GUI-subsystem binary: double-clicking the exe (or launching it from
// the launcher) must NOT flash a console window. The console is opt-in at run
// time via `--debug`, which calls `attach_debug_console()` before logging is
// set up — see `main`.
#![windows_subsystem = "windows"]

mod about_panel;
mod action_bar;
mod virtio_iso;
mod vm_panel;
mod vm_sessions_table;
mod bootrepair_table;
mod clone_panel;
mod confirm_dialog;
mod disk_dropdown;
mod disk_map;
mod disk_panel;
mod fonts;
mod history_table;
mod job;
mod mount_table;
mod raster;
mod restore_layout;
mod restore_panel;
mod sidebar;
mod single_instance;
mod status_modal;
mod stripes;
mod tex_mgr;
mod theme;
mod titlebar;
mod updater;
mod util;
mod version;

use std::collections::{BTreeMap, HashSet};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align2, Sense, Vec2};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::Window;

use phoenix_capture::backup::BackupOptions;
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{enumerate_disks, refine_partition_fs, DiskInfo};
use phoenix_core::ProgressHandle;
use phoenix_restore::restore::RestoreOptions;

use crate::confirm_dialog::{ConfirmAck, ConfirmAction, ConfirmView};
use crate::job::{
    spawn_backup, spawn_boot_repair, spawn_clone, spawn_restore, spawn_verify, BackgroundJob,
    JobKind,
};
use crate::sidebar::Page;
use crate::status_modal::{CompletedJob, JobOutcome, ModalAction, ModalView, Verdict};
use crate::theme::Palette;
use crate::util::format_bytes;

const THEME_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Minimum time the click-to-refresh dim/spinner overlay stays up. Disk
/// enumeration is usually near-instant; holding the overlay for a beat
/// makes the refresh visibly *happen* instead of flickering.
const REFRESH_OVERLAY_MIN: Duration = Duration::from_millis(1000);
/// Form-action button width (Browse…, Run restore, Verify backup).
/// Tuned to comfortably fit those labels at 16pt so the buttons render
/// at a consistent width across pages.
const FORM_BUTTON_W: f32 = 130.0;
/// Uniform height for the standard form controls (TextEdit + Browse rows,
/// colored action buttons). Roughly 16pt text + 2×8px TextEdit margin.
const ACTION_BUTTON_HEIGHT: f32 = 36.0;
/// Horizontal inner margin of the central panel — every page's content
/// starts this far from the panel edges.
const CENTRAL_PANEL_MARGIN_X: f32 = 24.0;
/// Right-hand buffer between the Backup page's controls and the window
/// edge, so every element ends on the same vertical line. The page-level
/// vertical scrollbar rides the window edge inside this gutter.
const PAGE_RIGHT_MARGIN: f32 = 15.0;
/// Height of the sticky bottom action bar (`action_bar::show`) that hosts
/// every page's primary action (Start backup, Run restore, Verify backup,
/// Clone disk) — pinned full-pane-width below the scrolling content,
/// deliberately bigger than the standard `FORM_BUTTON_W` ×
/// `ACTION_BUTTON_HEIGHT` form controls.
const START_BUTTON_HEIGHT: f32 = 52.0;

/// Decode the embedded application icon for the OS window chrome (title
/// bar / taskbar / Alt-Tab thumbnail). This is intentionally a *separate*
/// asset from the in-app UI icons (phosphor glyphs rendered via
/// `egui_phosphor`) — the OS-level icon is a full-color raster that needs
/// to look right at 16x16/32x32/256x256 against the taskbar/title bar,
/// while the in-app icons are vector glyphs tinted by the active theme.
fn app_icon() -> Option<winit::window::Icon> {
    let bytes = include_bytes!("../../assets/phoenix-appicon-256px.png");
    let image = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    winit::window::Icon::from_rgba(image.into_raw(), width, height).ok()
}

/// Approximate height of the bottom status bar (`Margin::symmetric(16.0,
/// 8.0)` + one text line). Added to the sidebar's minimum content height to
/// derive the OS window's `min_inner_size` so the sidebar can always show
/// the brand, History/Options, and 1.5 scrollable items at the smallest
/// allowed window size.
const STATUS_BAR_HEIGHT_ESTIMATE: f32 = 40.0;
const MIN_WINDOW_WIDTH: f32 = 640.0;

fn main() {
    // Serve-helper re-exec: booting a VM re-execs THIS exe with a sentinel
    // argv[1] to become the out-of-process WinFsp serve for the backup (see
    // `phoenix_vm::serve_helper`). Route it before anything else — without
    // this, the child comes up as a second GUI, the single-instance guard
    // swallows it, and the boot times out waiting for its READY handshake.
    let args: Vec<String> = std::env::args().collect();
    if let Some(res) = phoenix_vm::serve_helper::maybe_run(&args) {
        if let Err(e) = res {
            eprintln!("serve helper failed: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    // `--debug` re-enables the console this GUI-subsystem build normally
    // suppresses. Attach it BEFORE logging init so the stderr layer binds to a
    // live console handle (Rust resolves the std handles lazily per write, and
    // AllocConsole/AttachConsole populate them for a windowed process).
    let debug = debug_from_args();
    #[cfg(windows)]
    if debug {
        attach_debug_console();
    }
    let _log_guard = init_logging(debug);

    // Single instance: a second launch focuses the running window and exits.
    // Multi-drive backups/mounts all run from one window, so a second is never
    // wanted. Hold the guard for the whole run (released on process exit).
    let _instance: single_instance::Guard = match single_instance::acquire() {
        Some(guard) => guard,
        None => {
            tracing::info!("another instance is already running; focused it and exiting");
            return;
        }
    };

    // CPU-rendered egui on winit + softbuffer, not eframe/wgpu — the GPU
    // backends can't start in Windows PE (see phoenix-gui/Cargo.toml). The
    // window itself is built lazily in `resumed` (see `Renderer::new`).
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("failed to build event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let mut app = App {
        renderer: None,
        proxy,
        next_repaint: None,
        exited: false,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::error!("event loop error: {e}");
    }
}

// ── CPU-rendered winit shell ─────────────────────────────────────────────────
//
// Phoenix runs on winit + softbuffer + a hand-written egui rasterizer instead of
// eframe/wgpu, because every GPU backend fails to start in Windows PE / GPU-less
// recovery VMs — and PE is exactly where a bare-metal restore happens. See
// phoenix-gui/Cargo.toml for the full rationale. This is a single, chromeless
// top-level window; titlebar.rs re-adds the native frame behavior on top.

/// ASAP repaint requests (`after == 0`) are clamped to one frame so a
/// continuously-animating view (a running job's progress bar, the armed
/// action-bar mosaic) settles into a steady ~60 fps tick instead of spinning
/// the CPU — which matters more for a software rasterizer than a GPU one.
const REPAINT_FRAME_CAP: Duration = Duration::from_millis(16);

/// egui asked for a (possibly delayed) repaint — from a worker thread or an
/// animation. Forwarded from egui's repaint callback into the winit loop, where
/// it becomes the next wake deadline; `request_repaint` alone only sets an egui
/// flag and cannot wake a loop sleeping in `ControlFlow::Wait`.
enum UserEvent {
    Repaint { after: Duration },
}

/// Everything one window owns: the OS window, its software framebuffer, the egui
/// context + winit input bridge, the app state, and the CPU texture cache the
/// rasterizer samples.
struct Renderer {
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    app: PhoenixApp,
    tex_mgr: tex_mgr::TextureManager,
    /// Reused frame buffer — reallocating ~3 MB per frame is measurable at
    /// 60 fps, keeping it is free.
    pixels: Vec<u32>,
    /// When the most recent `paint` began. Continuous-repaint pacing anchors
    /// the next frame's deadline here instead of "now" so rasterization time
    /// doesn't stretch the frame interval (see `App::user_event`).
    last_paint_start: Instant,
    /// The window is created hidden and shown by `paint` the moment the first
    /// frame has been presented, so it never appears as an unpainted white
    /// rectangle while `PhoenixApp::new`'s disk enumeration runs.
    revealed: bool,
}

impl Renderer {
    fn new(event_loop: &ActiveEventLoop, proxy: EventLoopProxy<UserEvent>) -> Self {
        let min_height = sidebar::min_content_height() + STATUS_BAR_HEIGHT_ESTIMATE;
        let mut attrs = Window::default_attributes()
            // Title is load-bearing: single_instance::focus_existing finds the
            // running window by this exact string (FindWindowW).
            .with_title("Phoenix Simulacra")
            // Chromeless: no OS titlebar. `titlebar::show` floats a Win11-style
            // control pill and, on Windows, a wndproc subclass restores all
            // native frame behavior (drag, edge resize, snap, system menu) —
            // see `titlebar::install`.
            .with_decorations(false)
            // Born hidden, revealed by `paint` once there is a frame to show.
            // `PhoenixApp::new` below enumerates every physical disk before the
            // first paint can run, and a window that is already on screen spends
            // that whole enumeration white. See `Renderer::revealed`.
            .with_visible(false)
            .with_inner_size(LogicalSize::new(1100.0, 720.0))
            .with_min_inner_size(LogicalSize::new(MIN_WINDOW_WIDTH as f64, min_height as f64));
        if let Some(icon) = app_icon() {
            attrs = attrs.with_window_icon(Some(icon));
        }
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));

        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");

        let egui_ctx = egui::Context::default();
        // Bridge egui repaint requests into the winit loop.
        {
            let proxy = proxy.clone();
            egui_ctx.set_request_repaint_callback(move |info| {
                let _ = proxy.send_event(UserEvent::Repaint { after: info.delay });
            });
        }
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );

        // The HWND the titlebar subclass hooks (0 / unused off-Windows).
        #[cfg(windows)]
        let hwnd: isize = {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match window.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
                _ => 0,
            }
        };
        #[cfg(not(windows))]
        let hwnd: isize = 0;

        let app = PhoenixApp::new(&egui_ctx, hwnd);

        Renderer {
            window,
            surface,
            egui_ctx,
            egui_state,
            app,
            tex_mgr: tex_mgr::TextureManager::new(),
            pixels: Vec::new(),
            last_paint_start: Instant::now(),
            revealed: false,
        }
    }

    /// Run one egui frame and rasterize it into the softbuffer surface on the
    /// CPU: tessellate the UI into triangle meshes, walk each with barycentric
    /// interpolation while sampling the font/texture atlas, blend per-pixel,
    /// then present. No GPU is touched anywhere in here.
    fn paint(&mut self) {
        self.last_paint_start = Instant::now();
        let size = self.window.inner_size();
        let (w, h) = (size.width, size.height);
        if w == 0 || h == 0 {
            return;
        }

        let raw_input = self.egui_state.take_egui_input(&self.window);
        let full_output = self.egui_ctx.run(raw_input, |ctx| {
            self.app.draw(ctx);
        });
        self.egui_state
            .handle_platform_output(&self.window, full_output.platform_output);

        // Window commands egui emitted this frame. On Windows, minimize/maximize/
        // close/drag run natively through the titlebar wndproc, so the one seen
        // here in practice is `Close` (from the close-confirm dialog); the rest
        // are the portable off-Windows fallback.
        if let Some(vo) = full_output.viewport_output.get(&egui::ViewportId::ROOT) {
            for cmd in &vo.commands {
                match cmd {
                    egui::ViewportCommand::StartDrag => {
                        let _ = self.window.drag_window();
                        // The OS move-loop eats the button release, leaving
                        // egui's pointer stuck "down"; synthesize the release so
                        // the next drag can start.
                        let pos = self
                            .egui_ctx
                            .input(|i| i.pointer.latest_pos())
                            .unwrap_or(egui::Pos2::ZERO);
                        self.egui_state.egui_input_mut().events.push(
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed: false,
                                modifiers: egui::Modifiers::default(),
                            },
                        );
                    }
                    egui::ViewportCommand::Minimized(v) => self.window.set_minimized(*v),
                    egui::ViewportCommand::Maximized(v) => self.window.set_maximized(*v),
                    egui::ViewportCommand::Close => self.app.should_exit = true,
                    _ => {}
                }
            }
        }

        self.tex_mgr.update(&full_output.textures_delta);

        let ppp = full_output.pixels_per_point;
        let clipped = self.egui_ctx.tessellate(full_output.shapes, ppp);

        // Clear to the sidebar color (eframe's old `clear_color`): the central
        // panel's rounded bottom-left corner exposes it, and it must read as
        // part of the sidebar/status-bar L.
        let bg = self.app.palette.sidebar_bg;
        let bg32 = raster::to_bgra(bg.r(), bg.g(), bg.b());
        self.pixels.resize((w * h) as usize, 0);
        raster::rasterize(&clipped, &self.tex_mgr, w, h, ppp, bg32, &mut self.pixels);

        if self
            .surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .is_ok()
        {
            if let Ok(mut buf) = self.surface.buffer_mut() {
                buf.copy_from_slice(&self.pixels);
                let presented = buf.present().is_ok();
                // Reveal on the first frame that actually reached the surface,
                // never merely because we tried to draw one — a window shown
                // ahead of its content is the white flash this avoids.
                if presented && !self.revealed {
                    self.window.set_visible(true);
                    self.revealed = true;
                }
            }
        }
        // Continuous repaints are driven by the egui repaint callback ->
        // UserEvent::Repaint -> the timer-paced paint in `about_to_wait`, so we
        // deliberately do NOT request_redraw here (that would spin the CPU).
    }
}

/// The winit application: one window plus the loop's wake bookkeeping.
struct App {
    renderer: Option<Renderer>,
    proxy: EventLoopProxy<UserEvent>,
    /// Earliest pending repaint deadline; folded into `about_to_wait`'s WaitUntil.
    next_repaint: Option<Instant>,
    /// `on_exit` (unmount + apply staged update) must run exactly once.
    exited: bool,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_none() {
            let mut renderer = Renderer::new(event_loop, self.proxy.clone());
            // Paint the first frame here and now rather than asking for a
            // redraw: the window is still hidden, a hidden window gets no
            // WM_PAINT, and nothing else would drive a paint — `about_to_wait`
            // only paints once egui's repaint callback has armed a deadline,
            // which cannot happen until egui has run a frame. Waiting for
            // RedrawRequested would leave the app hung and unshown. This paint
            // is also what reveals the window.
            renderer.paint();
            // Fail open. If that paint could not present, `revealed` is still
            // false and nothing later would show the window — the app would be
            // running and unreachable. Falling back to the old behaviour costs
            // at worst the flash this change removes.
            if !renderer.revealed {
                renderer.window.set_visible(true);
                renderer.revealed = true;
            }
            self.renderer = Some(renderer);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let mut exit = false;
        if let Some(r) = self.renderer.as_mut() {
            let resp = r.egui_state.on_window_event(&r.window, &event);
            if resp.repaint {
                r.window.request_redraw();
            }
            match event {
                WindowEvent::CloseRequested => {
                    // Backups can't outlive the process (their drive letters and
                    // virtual disks are owned by it), so a close with mounts up
                    // is held for the "these will be unmounted" dialog
                    // (pending_close counts as a modal, so the page goes inert);
                    // otherwise the close goes straight through.
                    if r.app.close_needs_confirm() && !r.app.pending_close {
                        r.app.pending_close = true;
                        r.window.request_redraw();
                    } else {
                        r.app.should_exit = true;
                    }
                }
                WindowEvent::RedrawRequested => r.paint(),
                WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                    r.window.request_redraw();
                }
                _ => {}
            }
            exit = r.app.should_exit;
        }
        if exit {
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Repaint { after } => {
                let now = Instant::now();
                let deadline = if after.is_zero() {
                    // Continuous animation: anchor the ~60 fps tick to the
                    // START of the frame that asked for it, not to now —
                    // otherwise rasterization time adds to the interval and
                    // a 10 ms frame yields 26 ms ticks, not 16 ms ones.
                    let anchor = self
                        .renderer
                        .as_ref()
                        .map_or(now, |r| r.last_paint_start);
                    (anchor + REPAINT_FRAME_CAP).max(now)
                } else {
                    now + after
                };
                self.next_repaint =
                    Some(self.next_repaint.map_or(deadline, |d| d.min(deadline)));
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Service a due repaint by painting directly (not request_redraw), so
        // the loop keeps returning to the OS message pump between frames.
        let due = matches!(self.next_repaint, Some(d) if Instant::now() >= d);
        let mut exit = false;
        if let Some(r) = self.renderer.as_mut() {
            if due {
                self.next_repaint = None;
                r.paint();
            }
            exit = r.app.should_exit;
        }
        if exit {
            event_loop.exit();
            return;
        }
        // Sticky control flow: reset to Wait when nothing is pending, else an
        // elapsed WaitUntil spins the loop until the next OS event.
        match self.next_repaint {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Runs on every loop exit (close dialog, titlebar X, OS session-end):
        // the last chance to tear mounts down and apply a staged update.
        if !self.exited {
            self.exited = true;
            if let Some(r) = self.renderer.as_mut() {
                r.app.on_exit();
            }
        }
    }
}

/// Sets up tracing to a rolling log file in `%LOCALAPPDATA%\PhoenixSimulacra\
/// logs\` (always) and, when `debug` is set, also to the console `--debug`
/// attached — so users who double-click the .exe can still hand us a log, while
/// `--debug` runs additionally stream it live.
///
/// The default filter is deliberately *verbose* — `info` for every Phoenix
/// crate — so a fresh log file is immediately useful even if the user never
/// touches `RUST_LOG`. A previous filter of
/// `"phoenix_core=info,phoenix_gui=info,warn"` made successful backups look
/// silent (no log writes between startup and shutdown) which led us down
/// multiple "the binary must be stale" rabbit holes. Anything truly noisy
/// (per-chunk debug) is gated behind explicit `debug!`/`trace!` levels.
fn init_logging(debug: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "phoenix_core=info,phoenix_gui=info,phoenix_capture=info,\
             phoenix_restore=info,phoenix_vss=info,phoenix_build=info,\
             phoenix_mount=info,phoenix_clone=info,phoenix_vm=info,warn"
            .into()
    });

    let log_dir = std::env::var("LOCALAPPDATA")
        .map(|p| {
            std::path::PathBuf::from(p)
                .join("PhoenixSimulacra")
                .join("logs")
        })
        .unwrap_or_else(|_| std::env::temp_dir().join("PhoenixSimulacra").join("logs"));

    // Only attach the stderr layer in `--debug` runs. In the normal
    // GUI-subsystem build there is no console, so this would otherwise write to
    // a dead handle. `Option<Layer>` is itself a `Layer` (a no-op when `None`).
    let stderr_layer = debug.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(true)
    });

    let (file_layer, guard) = match std::fs::create_dir_all(&log_dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::daily(&log_dir, "simulacra.log");
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

/// A validated, ready-to-run clone parked while its confirmation dialog is
/// up. The dialog restates the target disk's number/size/model so the user
/// gets one last chance to catch a wrong-disk pick before anything is
/// written.
struct PendingClone {
    opts: phoenix_clone::CloneOptions,
    message: String,
    details: Vec<String>,
    subject: JobSubject,
    /// True when a 4Kn → 512e sector-size conversion is pending: the dialog
    /// becomes the conversion hazard dialog (extra copy + an acknowledgement
    /// checkbox gating Confirm). `ack` holds that checkbox's state.
    convert: bool,
    ack: bool,
}

/// A validated, ready-to-run restore parked while its confirmation dialog
/// is up. Same final wrong-disk check as [`PendingClone`].
struct PendingRestore {
    opts: RestoreOptions,
    details: Vec<String>,
    subject: JobSubject,
    /// See [`PendingClone::convert`].
    convert: bool,
    ack: bool,
}

/// A boot repair parked while its confirmation dialog is up. The dialog
/// restates the planned actions; when the target is the disk this machine is
/// running from, Confirm is additionally gated on an acknowledgement
/// checkbox (`ack`).
struct PendingBootRepair {
    disk_index: u32,
    partition_index: u32,
    message: String,
    details: Vec<String>,
    subject: JobSubject,
    /// True when the target is the live system disk.
    system_disk: bool,
    ack: bool,
}

/// A VM booted on a background thread. `boot()` blocks for the whole guest
/// session, so it can't run on the UI thread; the outcome comes back over
/// `result` and the page polls it each frame.
struct VmRun {
    /// The backup this VM is running — used to locate its session (for the QMP
    /// stop port) and to label the page.
    backup: std::path::PathBuf,
    name: String,
    started: Instant,
    /// Path to the session overlay, polled to show how much the guest has written.
    overlay_path: std::path::PathBuf,
    /// The `net use` command mapping this run's shared folder in the guest,
    /// when the share is active.
    share_cmd: Option<String>,
    /// The VM working root this run actually uses (session, serve scratch,
    /// QMP port file) — captured at boot so Stop can't be misdirected by a
    /// later scratch-dropdown change.
    vm_root: std::path::PathBuf,
    /// Whether the guest's virtual network cable is plugged in. Every VM
    /// boots unplugged; the running view grants it.
    network_connected: bool,
    result: std::sync::mpsc::Receiver<anyhow::Result<phoenix_vm::boot::BootOutcome>>,
}

/// A guest-tools ISO download (or update check) running on a worker thread;
/// the Virtualize page polls `rx` and shows `progress` as a bar.
struct DriversDl {
    rx: std::sync::mpsc::Receiver<virtio_iso::DlEvent>,
    /// (bytes downloaded, total or 0) — updated from `Progress` events.
    progress: (u64, u64),
}

/// The two ends of the running job, named the way a person would name them
/// ("Samsung SSD 990 PRO (Disk 1)", `D:\Backups\work.phnx`).
///
/// Captured when the job is *spawned*, because that is the last moment the
/// context still exists: by the time the worker finishes, the page that chose
/// the disk and the file has usually been reset, and the record would have
/// nothing left to describe. `poll_job` hands this to the history record.
#[derive(Clone, Default)]
struct JobSubject {
    source: String,
    target: String,
    /// The `.phnx` the job wrote or read, stat'd when the job ends for the
    /// size the History page shows next to it. `None` for a clone.
    image_path: Option<PathBuf>,
}

impl JobSubject {
    /// On-disk size of the job's `.phnx`, or 0 if it has none (or the file is
    /// gone — a cancelled backup deletes its half-written image).
    fn image_bytes(&self) -> u64 {
        self.image_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0)
    }
}

/// The `Disk number / Size / Model` lines both confirmation dialogs show.
/// The acknowledgement the user must tick before a sector-size conversion can
/// proceed — the one thing the wrong-disk warning can't cover: the result is a
/// converted copy, not a byte-identical restore/clone.
const CONVERSION_ACK_LABEL: &str =
    "I understand this rewrites filesystem metadata, and the result is a converted copy — \
     not an identical one.";

/// Acknowledgement gating a boot repair aimed at the disk this machine is
/// currently running from — the one case where "only the selected drive is
/// modified" still means the live system's own boot files.
const BOOT_REPAIR_SYSTEM_ACK_LABEL: &str =
    "I understand this rewrites the boot files of the Windows installation this PC is \
     currently running.";

/// Detail lines describing a pending 4Kn → 512e conversion, appended to the
/// hazard dialog's details under the standard wrong-disk block (Alerts D/E).
fn conversion_detail_lines(report: &phoenix_core::sector::ConversionReport) -> Vec<String> {
    let mut lines = vec!["— Sector-size conversion (4Kn → 512e) —".to_string()];
    for n in &report.converted_names {
        lines.push(format!("convert: {n}"));
    }
    if report.bootable {
        lines.push(
            "The EFI System Partition will be converted — the disk should be bootable.".to_string(),
        );
    } else {
        lines.push(
            "The ESP is NOT being converted — the disk will hold its data but will not boot."
                .to_string(),
        );
    }
    for n in &report.unconverted_names {
        lines.push(format!("left unconverted (unreadable on 512e): {n}"));
    }
    lines
}

/// Whether a source layout could produce a bootable Windows disk — drives the
/// default of the "repair boot files" checkbox on the Clone and Restore
/// pages. GPT needs an EFI System Partition plus a Windows-capable data
/// partition; MBR just needs the latter (the repair sets the active flag
/// itself). A wrong guess is harmless — the checkbox stays editable, and the
/// repair pass reports "no Windows installation found" rather than failing.
fn source_looks_bootable(source: &DiskInfo) -> bool {
    use phoenix_core::disk::FilesystemKind as F;
    let has_windows_capable = source
        .partitions
        .iter()
        .any(|p| matches!(p.fs_kind, F::Ntfs | F::Bitlocker));
    let has_esp = source.partitions.iter().any(|p| {
        p.fs_kind == F::Efi || phoenix_core::disk::is_efi_system_partition(&p.type_guid)
    });
    has_windows_capable && (has_esp || !source.is_gpt)
}

/// Hover text for the "repair boot files" checkbox on both pages.
const REPAIR_BOOT_HOVER: &str =
    "After the data is written, detect a Windows installation on the target disk and rebuild \
     its boot files with Windows' own bcdboot (plus bootsect and the active flag on legacy MBR \
     disks). Only the target disk is modified — this PC's firmware boot entries are never \
     touched. Skipped automatically if the target turns out to hold no Windows installation.";

fn disk_confirm_details(disk: &DiskInfo) -> Vec<String> {
    vec![
        format!("Disk number:   {}", disk.index),
        format!("Size:   {}", format_bytes(disk.size_bytes)),
        format!(
            "Model:   {}",
            disk.model
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or("(unknown)")
        ),
    ]
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
    /// FIFO queue of remaining backup jobs when a multi-disk selection is
    /// in flight. The currently-running job lives in `job`; on completion
    /// we pop the next entry here and spawn it.
    pending_backups: Vec<BackupOptions>,
    /// A fully-built, validated backup queue that is waiting on the user to
    /// confirm overwriting one or more existing `.phnx` files. `Some` puts the
    /// overwrite-confirmation dialog on screen; cleared on confirm (launch) or
    /// cancel (abort).
    pending_overwrite: Option<PendingOverwrite>,
    /// Clone waiting on its confirm-the-target dialog.
    pending_clone: Option<PendingClone>,
    /// Restore waiting on its confirm-the-target dialog.
    pending_restore: Option<PendingRestore>,
    /// `--demo-confirm`: show a fake wipe-confirmation dialog for styling work.
    demo_confirm: bool,
    /// `--demo-progress`: show the status modal mid-job, its bar sweeping on a
    /// loop, for styling work on the progress bar without running a real job.
    demo_progress: bool,
    /// Total number of jobs in the current multi-disk run (for "Disk N of M"
    /// status text). Reset to 0 when the queue drains.
    total_backups: usize,
    /// 1-based index of the job currently running for the status text.
    current_backup_index: usize,
    restore_backup_path: String,
    /// Path we last successfully loaded (skip redundant opens).
    restore_loaded_path: String,
    /// Set by the history dropdown or Browse to load on the next frame.
    restore_backup_load_now: bool,
    restore_layout: Option<restore_layout::RestoreLayoutState>,
    /// Restore page's chosen target disk; `None` until the user picks one
    /// (deliberately never auto-selected — same contract as the Clone page).
    target_disk_index: Option<u32>,
    /// Restore page: run the post-restore boot repair on the target.
    /// Defaulted when a backup loads (on when its layout looks bootable),
    /// then user-editable.
    restore_repair_boot: bool,
    /// Clone page state: chosen source/target disk indices and options.
    clone_source_index: Option<u32>,
    clone_target_index: Option<u32>,
    clone_expand: bool,
    /// Clone page: run the post-clone boot repair on the target. Defaulted
    /// per source pick (on when the source carries a bootable Windows
    /// layout), then user-editable.
    clone_repair_boot: bool,
    /// The clone plan editor (shared machinery with the Restore page).
    /// `Some` only while both a source and a target disk are picked.
    clone_layout: Option<restore_layout::RestoreLayoutState>,
    /// `(source_index, target_index)` the layout was seeded from, so a
    /// selection change rebuilds it.
    clone_layout_for: Option<(u32, u32)>,
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
    /// Whether WinFsp is installed and reachable, so mounting can actually run.
    /// Probed once at startup (the underlying init caches its outcome anyway).
    /// When false the Mount page shows an install-WinFsp notice instead of the
    /// picker, and its action bar is withheld. `--demo-no-winfsp` forces this
    /// false so the notice can be eyeballed on a machine that has WinFsp.
    mount_available: bool,
    /// `--demo-mounts`: canned table rows that stand in for real mounts, so the
    /// Active-mounts table and the close-with-mounts dialog can be eyeballed
    /// without an elevated session and a `.phnx` to attach. Empty in normal
    /// runs; where it matters, the page treats these exactly like `mounts`.
    demo_mounts: Vec<mount_table::MountRow>,
    /// A window close was intercepted because backups are still mounted: the
    /// "these will be unmounted" dialog is up, waiting for the user.
    pending_close: bool,
    /// A mount awaiting the "enable temporary write access" hazard dialog. Holds
    /// the Active-mounts table index (real rows first, then demo rows).
    pending_enable_write: Option<usize>,
    /// Boot VM was clicked while saved sessions exist: the resume-or-new
    /// hazard dialog is up. Holds every saved session's (backup path, its own
    /// VM working root) — resume must go where the session lives.
    vm_boot_prompt: Option<Vec<(String, std::path::PathBuf)>>,
    /// Acknowledgement-checkbox state for the enable-write dialog.
    enable_write_ack: bool,
    /// Virtualize page controls (backup, knobs, scratch drive, ISO).
    vm: vm_panel::VmPageState,
    /// A VM running on a background thread, if any (one at a time for now).
    vm_run: Option<VmRun>,
    /// Last VM outcome/status shown on the Virtualize page.
    vm_last_status: String,
    /// An in-flight guest-tools ISO download/update-check, if any.
    drivers_dl: Option<DriversDl>,
    /// One-line outcome of the last drivers download / update check.
    drivers_note: String,
    /// Discovered QEMU install, or `None` when it isn't found — the page then
    /// shows an install notice (mirrors `mount_available`).
    qemu: Option<phoenix_vm::Qemu>,
    /// Boot Repair page: Windows installations detected on the current disk
    /// list. `None` means "scan on next page render" — cleared whenever the
    /// disk list reloads so the page never shows installs from a stale world.
    bootrepair_installs: Option<Vec<phoenix_restore::bootrepair::WindowsInstall>>,
    /// The picked installation as `(disk_index, partition_index)`.
    bootrepair_selected: Option<(u32, u32)>,
    /// Read-only repair plan preview for the picked installation, keyed by the
    /// selection it was computed for (recomputed when the pick changes). `Err`
    /// holds the human reason the install can't be repaired (e.g. GPT disk
    /// with no ESP).
    #[allow(clippy::type_complexity)]
    bootrepair_plan: Option<(
        (u32, u32),
        std::result::Result<phoenix_restore::bootrepair::BootRepairPlan, String>,
    )>,
    /// Boot repair waiting on its confirmation dialog.
    pending_boot_repair: Option<PendingBootRepair>,
    /// Persisted user settings and job history (loaded at startup).
    settings: phoenix_core::appdata::Settings,
    history: phoenix_core::appdata::History,
    /// Self-updater state (last-check throttle + a verified staged installer),
    /// persisted separately from `settings` because the app writes it.
    update_state: phoenix_core::appdata::UpdateState,
    /// An in-flight update check (manual or the throttled auto one). `None` when
    /// no check is running.
    update_check: Option<updater::UpdateCheck>,
    /// Set once a verified installer is staged: the persistent "will install on
    /// close" line shown in the status bar. `None` until then.
    update_banner: Option<String>,
    /// `--demo-history`: the history is a canned set of rows for styling work,
    /// and must never be written back over the real one.
    demo_history: bool,
    /// The history entry the user clicked ✕ on, waiting on its confirmation
    /// dialog. An index into `history.records`.
    pending_history_delete: Option<usize>,
    /// Wall-clock start of the currently-running job, for the history record.
    job_started: Option<Instant>,
    /// When the running backup's verify-after pass began (it is the last step
    /// of the same worker). Splits the job into the two history entries the
    /// user asked for: the capture, then the verify that followed it.
    verify_started: Option<Instant>,
    /// What the running job is acting on — see [`JobSubject`].
    job_subject: JobSubject,
    status: String,
    page: Page,
    job: Option<BackgroundJob>,
    /// Finished-job snapshot that keeps the status modal up (final step list
    /// and colored Close button) until the user dismisses it. `None` once
    /// dismissed or while a job is still running.
    completed: Option<CompletedJob>,
    palette: Palette,
    last_theme_refresh: Instant,
    /// The titlebar's Refresh button was clicked this frame, on this page:
    /// the disk re-enumeration (and that page's reset) runs at the START of
    /// the next frame, so the dim/spinner overlay is already on screen if
    /// enumeration blocks.
    pending_refresh: Option<Page>,
    /// Keeps the dim/spinner overlay up until this deadline (click time +
    /// [`REFRESH_OVERLAY_MIN`]), even when the refresh itself is instant.
    refresh_overlay_until: Option<Instant>,
    /// A backup/restore/clone just finished: re-enumerate disks at the start
    /// of the NEXT frame (so the completion modal paints first), keeping the
    /// drive dropdowns in step with what the job changed on disk. Unlike a
    /// titlebar Refresh this resets no page state and shows no overlay.
    pending_disk_reload: bool,
    /// Set when the app should exit after the current frame — the close-confirm
    /// dialog was accepted, or a portable `ViewportCommand::Close` arrived. The
    /// winit loop checks it after each paint and calls `event_loop.exit()`.
    should_exit: bool,
    /// Set by the status bar's "Restart to update" link: the exit that follows
    /// should wait for the staged installer and relaunch the app, rather than
    /// leaving it closed the way a plain close does.
    restart_after_update: bool,
    /// Running from the portable ZIP rather than an installed copy
    /// ([`updater::is_portable`]). Read once at startup: the marker sits beside
    /// the exe and can't change under a running process.
    portable: bool,
}

impl PhoenixApp {
    fn new(egui_ctx: &egui::Context, hwnd: isize) -> Self {
        egui_extras::install_image_loaders(egui_ctx);
        fonts::install(egui_ctx);

        // Hook the native window for the chromeless titlebar: DWM shadow +
        // rounded corners, and the WM_NCHITTEST subclass that makes drag,
        // edge-resize, Aero Snap, and the Snap Layouts flyout behave exactly
        // like a decorated window. `hwnd` comes from `Renderer::new`.
        #[cfg(target_os = "windows")]
        if hwnd != 0 {
            titlebar::install(hwnd, egui_ctx.clone());
        }
        #[cfg(not(target_os = "windows"))]
        let _ = hwnd;

        let settings = phoenix_core::appdata::Settings::load();
        let palette = theme::refresh(egui_ctx, settings.theme);
        let demo_history = demo_history_from_args();
        let is_demo_history = demo_history.is_some();
        let history = demo_history.unwrap_or_else(phoenix_core::appdata::History::load);
        let backup_folder = settings
            .default_backup_dir
            .clone()
            .unwrap_or_else(default_backup_folder);
        let update_state = phoenix_core::appdata::UpdateState::load();
        let mut app = Self {
            disks: Vec::new(),
            selections: HashSet::new(),
            backup_folder,
            backup_name: demo_backup_name_from_args(),
            pending_backups: Vec::new(),
            pending_overwrite: None,
            pending_clone: None,
            pending_restore: None,
            demo_confirm: demo_confirm_from_args(),
            demo_progress: demo_progress_from_args(),
            total_backups: 0,
            current_backup_index: 0,
            restore_backup_path: String::new(),
            restore_loaded_path: String::new(),
            restore_backup_load_now: false,
            restore_layout: None,
            target_disk_index: None,
            restore_repair_boot: false,
            clone_source_index: None,
            clone_target_index: None,
            clone_expand: false,
            clone_repair_boot: false,
            clone_layout: None,
            clone_layout_for: None,
            mount_backup_path: String::new(),
            mount_loaded_path: String::new(),
            mount_source: None,
            mount_selection: HashSet::new(),
            mounts: Vec::new(),
            mount_available: !force_no_winfsp_from_args()
                && phoenix_mount::ActiveMount::runtime_available(),
            demo_mounts: demo_mount_rows_from_args(),
            pending_close: false,
            pending_enable_write: None,
            vm_boot_prompt: None,
            enable_write_ack: false,
            vm: vm_panel::VmPageState::default(),
            vm_run: None,
            vm_last_status: String::new(),
            drivers_dl: None,
            drivers_note: String::new(),
            // Probe for QEMU once at startup; the page shows an install notice
            // when it's missing. A saved directory wins over autodetection so
            // a side-by-side build can be used without touching the system one.
            qemu: phoenix_vm::Qemu::discover(
                settings.vm_qemu_dir.as_deref().map(std::path::Path::new),
            )
            .ok(),
            bootrepair_installs: None,
            bootrepair_selected: None,
            bootrepair_plan: None,
            pending_boot_repair: None,
            settings,
            history,
            update_state,
            update_check: None,
            update_banner: None,
            demo_history: is_demo_history,
            pending_history_delete: None,
            job_started: None,
            verify_started: None,
            job_subject: JobSubject::default(),
            status: "Ready".into(),
            page: start_page_from_args().unwrap_or(Page::Backup),
            job: None,
            completed: demo_completed_job_from_args(),
            palette,
            last_theme_refresh: Instant::now(),
            pending_refresh: None,
            refresh_overlay_until: demo_refresh_overlay_from_args(),
            pending_disk_reload: false,
            should_exit: false,
            restart_after_update: false,
            portable: updater::is_portable(),
        };
        app.refresh_disks();
        // Reclaim any mount artifacts a previous crashed run left in the scratch
        // dir (stale WinFsp junctions, orphaned writable-overlay children).
        phoenix_mount::cleanup_leaked_mounts(&phoenix_mount::mount_scratch_dir());
        // `--demo-enable-write`: pop the write-access hazard dialog on a demo
        // mount at startup, so its styling can be eyeballed without an elevated
        // session and a real mount. Needs `--demo-mounts` for a row to target.
        if demo_enable_write_from_args() && !app.demo_mounts.is_empty() {
            app.page = Page::Mount;
            app.pending_enable_write = Some(0);
        }
        // Restore the persisted Virtualize knobs (scratch drive, RAM, CPUs).
        app.vm.scratch = match app.settings.vm_scratch_drive {
            Some(c) => vm_panel::ScratchChoice::Drive(c),
            None => vm_panel::ScratchChoice::SameAsImage,
        };
        if let Some(m) = app.settings.vm_memory_mib {
            app.vm.mem_mib = m;
        }
        if let Some(c) = app.settings.vm_cpus {
            app.vm.cpus = c;
        }
        app.init_updates();
        app
    }

    /// Startup update housekeeping: drop an already-applied staged installer and
    /// sweep partial downloads; re-arm the "install on close" banner if a
    /// verified installer survived from a previous session; and kick off a
    /// silent check.
    ///
    /// The check runs on every launch (no throttle): this is a manually-launched
    /// tool, not a daemon, so one background check per start is cheap and — the
    /// reason it's unconditional — a per-day throttle made the update silently
    /// skip a launch where a new release had just dropped. The status-bar banner
    /// is the only signal the user needs; the update applies silently on close.
    ///
    /// None of it happens in a portable build: no check, no staging, no banner.
    /// See [`updater::is_portable`] — the About page's manual check is the only
    /// update surface those get, and it downloads nothing.
    fn init_updates(&mut self) {
        if self.portable {
            tracing::info!("portable build — automatic updates disabled");
            return;
        }
        let running = version::BUILD_INFO.version;
        updater::cleanup_stale(&mut self.update_state, running);

        // A verified installer left staged from last session (e.g. the user
        // never closed the app) still applies on the next close.
        if let (Some(ver), Some(_path)) = (
            self.update_state.staged_version.clone(),
            self.update_state.staged_installer.clone(),
        ) {
            self.update_banner = Some(format!("Update {ver} will install when you close the app."));
        }

        // Nothing to do if an update from last session is already staged.
        if self.update_banner.is_none() {
            self.update_state.last_check_unix = phoenix_core::appdata::now_unix();
            let _ = self.update_state.save();
            self.update_check = Some(updater::UpdateCheck::spawn(
                updater::CheckMode::SilentAuto,
                running.to_string(),
                updater::Depth::Stage,
            ));
        }
    }

    /// Advance an in-flight update check and act on its events. Manual checks
    /// narrate via `self.status`; auto checks stay silent unless an update is
    /// actually staged. Called once per frame from `update()`.
    fn poll_updater(&mut self, ctx: &egui::Context) {
        let Some(check) = self.update_check.as_mut() else {
            return;
        };
        let manual = check.mode() == updater::CheckMode::Manual;
        let Some(ev) = check.poll() else {
            return;
        };
        match ev {
            updater::UpdateEvent::Progress { downloaded, total } => {
                if total > 0 {
                    let pct = (downloaded.saturating_mul(100) / total).min(100);
                    self.status = format!("Downloading update… {pct}%");
                } else {
                    self.status = "Downloading update…".into();
                }
                ctx.request_repaint();
            }
            updater::UpdateEvent::UpToDate => {
                if manual {
                    self.status = "You're on the latest version.".into();
                }
                self.update_check = None;
            }
            // Portable builds only, and only ever from the About page's button:
            // say what's out there and where to get it, then stop. Nothing was
            // downloaded and nothing will be, so this leaves no banner behind —
            // there is no "Restart to update" to offer.
            updater::UpdateEvent::Available { version } => {
                self.status = format!(
                    "Update {version} is available — download it from {}",
                    about_panel::HOME_URL
                );
                self.update_check = None;
            }
            updater::UpdateEvent::Ready(staged) => {
                self.update_state.staged_version = Some(staged.version.clone());
                self.update_state.staged_installer =
                    Some(staged.installer.display().to_string());
                let _ = self.update_state.save();
                let banner =
                    format!("Update {} will install when you close the app.", staged.version);
                self.update_banner = Some(banner.clone());
                self.status = banner;
                self.update_check = None;
                ctx.request_repaint();
            }
            updater::UpdateEvent::NoNetwork => {
                if manual {
                    self.status =
                        "Couldn't reach the update server — check your connection.".into();
                }
                self.update_check = None;
            }
            updater::UpdateEvent::Failed(msg) => {
                if manual {
                    self.status = format!("Update check failed: {msg}");
                }
                self.update_check = None;
            }
        }
    }

    fn refresh_disks(&mut self) {
        self.status = match self.reload_disks() {
            Ok(n) => format!("Found {n} disk(s)"),
            Err(e) => format!("Disk enumeration failed: {e}"),
        };
    }

    /// Re-enumerate physical disks and prune stale selections without
    /// touching the status line — the post-job reload must not stomp the
    /// "Backup completed" message the job just wrote there.
    fn reload_disks(&mut self) -> std::result::Result<usize, String> {
        match enumerate_disks() {
            Ok(mut disks) => {
                for d in &mut disks {
                    for p in &mut d.partitions {
                        refine_partition_fs(p);
                    }
                }
                self.disks = disks;
                self.prune_stale_selections();
                // The Boot Repair page's install scan and plan preview were
                // computed against the old disk list — rescan on next render.
                self.bootrepair_installs = None;
                self.bootrepair_plan = None;
                Ok(self.disks.len())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Arm a deferred disk refresh: the dim/spinner overlay goes up this
    /// frame, and the actual enumeration + page reset run at the start of
    /// the next one (see the top of `update`), so the overlay is visible
    /// even if `enumerate_disks` blocks for a while.
    fn begin_refresh(&mut self) {
        self.pending_refresh = Some(self.page);
        self.refresh_overlay_until = Some(Instant::now() + REFRESH_OVERLAY_MIN);
    }

    /// Run the disk refresh the titlebar's Refresh click armed on the
    /// previous frame, and reset whichever page was showing when it was
    /// clicked (the pages with no state to drop just get the fresh disks).
    fn run_pending_refresh(&mut self) {
        let Some(page) = self.pending_refresh.take() else {
            return;
        };
        self.refresh_disks();
        match page {
            Page::Backup => self.clear_backup_ui_state(),
            Page::Restore => self.reset_restore_page(),
            Page::Clone => self.clear_clone_ui_state(),
            // Re-read the log from disk (but never over the demo rows).
            Page::History if !self.demo_history => {
                self.history = phoenix_core::appdata::History::load()
            }
            Page::History => {}
            // The install scan is already invalidated by `reload_disks`;
            // a refresh also drops the pick, page-reset style.
            Page::BootRepair => {
                self.bootrepair_selected = None;
            }
            Page::Verify | Page::Mount | Page::Virtualize | Page::About => {}
        }
    }

    /// Full-window dim + centered spinner while a refresh is armed or the
    /// minimum overlay time hasn't elapsed. The backdrop swallows clicks so
    /// nothing underneath reacts mid-refresh.
    fn show_refresh_overlay(&mut self, ctx: &egui::Context) {
        let Some(until) = self.refresh_overlay_until else {
            return;
        };
        if self.pending_refresh.is_none() && Instant::now() >= until {
            self.refresh_overlay_until = None;
            return;
        }

        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("refresh_overlay_backdrop"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.left_top())
            .interactable(true)
            .show(ctx, |ui| {
                let _ = ui.allocate_rect(screen, Sense::click_and_drag());
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
            });
        egui::Area::new(egui::Id::new("refresh_overlay_spinner"))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                // White regardless of theme: it sits on the black-dimmed
                // backdrop, where the light palette's dark icon color would
                // vanish.
                ui.add(egui::Spinner::new().size(48.0).color(egui::Color32::WHITE));
            });
        // The Spinner only animates on repaint, and the overlay must come
        // down on time even if nothing else triggers one.
        ctx.request_repaint();
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
        self.job.is_some()
            || self.completed.is_some()
            || self.pending_overwrite.is_some()
            || self.pending_clone.is_some()
            || self.pending_restore.is_some()
            || self.pending_history_delete.is_some()
            || self.pending_close
            || self.pending_enable_write.is_some()
            || self.pending_boot_repair.is_some()
            || self.vm_boot_prompt.is_some()
    }

    fn clear_restore_ui_state(&mut self) {
        self.restore_loaded_path.clear();
        self.restore_backup_load_now = false;
        self.restore_layout = None;
        self.restore_repair_boot = false;
    }

    /// Reset the Backup page to its untouched default: nothing selected,
    /// empty name, the last-used folder. The refresh button doubles as a
    /// page reset on every page that has one.
    fn clear_backup_ui_state(&mut self) {
        self.selections.clear();
        self.backup_name.clear();
        self.backup_folder = self
            .settings
            .default_backup_dir
            .clone()
            .unwrap_or_else(default_backup_folder);
    }

    /// Reset the Restore page to its default: no backup chosen, no layout,
    /// no target picked.
    fn reset_restore_page(&mut self) {
        self.restore_backup_path.clear();
        self.clear_restore_ui_state();
        self.target_disk_index = None;
    }

    /// Reset the Clone page to its untouched default (no disks picked, no
    /// plan). Called on every terminal clone outcome — success, failure, or
    /// cancel — so the page can never show a stale, already-executed plan
    /// that leaves the user second-guessing whether they started it.
    fn clear_clone_ui_state(&mut self) {
        self.clone_source_index = None;
        self.clone_target_index = None;
        self.clone_expand = false;
        self.clone_repair_boot = false;
        self.clone_layout = None;
        self.clone_layout_for = None;
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
                let layout = restore_layout::RestoreLayoutState::from_backup(path_str, &reader);
                // Default the post-restore boot repair to on exactly when the
                // backup's own layout could produce a bootable Windows disk.
                self.restore_repair_boot = source_looks_bootable(&layout.source_disk);
                self.restore_layout = Some(layout);
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

    fn poll_restore_backup_load(&mut self) {
        if self.restore_backup_load_now {
            self.restore_backup_load_now = false;
            self.try_load_restore_backup();
        }
    }

    /// Write the finished job into the persistent history (best-effort).
    ///
    /// A backup that ran with verify-after-backup lands as TWO entries, back to
    /// back: the capture, then the verify pass the same worker ran after it.
    /// That is how the user thinks of it — and it means a verify that fails on
    /// a backup whose capture completed says exactly that, instead of a single
    /// "backup failed" row that hides which half broke.
    fn record_job(
        &mut self,
        kind: JobKind,
        outcome: phoenix_core::appdata::JobOutcome,
        snap: &phoenix_core::ProgressSnapshot,
        verify_after: bool,
    ) {
        use phoenix_core::appdata::{now_unix, JobKindTag, JobOutcome as Rec, JobRecord};

        let tag = match kind {
            JobKind::Backup => JobKindTag::Backup,
            JobKind::Restore => JobKindTag::Restore,
            JobKind::Verify => JobKindTag::Verify,
            JobKind::Clone => JobKindTag::Clone,
            JobKind::BootRepair => JobKindTag::BootRepair,
        };
        let total = self.job_started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
        let ended = now_unix();
        let started = ended - total as i64;
        // How far into the job its verify pass began, if it got that far.
        let verify_at = self
            .verify_started
            .take()
            .zip(self.job_started)
            .map(|(v, s)| v.duration_since(s).as_secs());
        self.job_started = None;

        let subject = std::mem::take(&mut self.job_subject);
        let image_bytes = subject.image_bytes();

        let Some(verify_at) = verify_at.filter(|_| kind == JobKind::Backup && verify_after) else {
            let _ = self.history.append(
                JobRecord::new(tag, outcome, started, total)
                    .with_source(subject.source)
                    .with_target(subject.target)
                    .with_bytes(snap.total, image_bytes),
            );
            return;
        };

        // The worker reached the verify step, so the capture itself finished and
        // the image was written — whatever the job's overall outcome, that half
        // succeeded. Any failure or cancel from here belongs to the verify.
        let _ = self.history.append(
            JobRecord::new(JobKindTag::Backup, Rec::Success, started, verify_at)
                .with_source(subject.source)
                .with_target(subject.target.clone())
                .with_bytes(snap.total, image_bytes),
        );
        let _ = self.history.append(
            JobRecord::new(
                JobKindTag::Verify,
                outcome,
                started + verify_at as i64,
                total.saturating_sub(verify_at),
            )
            .with_source(subject.target)
            .with_bytes(snap.total, image_bytes),
        );
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
        // A verify-after-backup is the last step of the backup worker's own
        // plan; note when it starts so the two history entries the job produces
        // get honest, non-overlapping timestamps.
        let verifying = job_kind == JobKind::Backup && job_verified && {
            let snap = job.progress.snapshot();
            !snap.steps.is_empty() && snap.current_step + 1 == snap.steps.len()
        };
        let result = job.poll();
        // Capture the final step list before dropping the job so the modal can
        // keep showing it (with a Close button) after the worker thread is gone.
        let final_snap = result.is_some().then(|| job.progress.snapshot());

        if verifying && self.verify_started.is_none() {
            self.verify_started = Some(Instant::now());
        }
        let (Some(result), Some(final_snap)) = (result, final_snap) else {
            ctx.request_repaint();
            return;
        };
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
                        false,
                    );
                    if let Some(completed) = self.completed.as_mut() {
                        completed.banner = "Completed and verified.".to_string();
                    }
                } else if let Some(next) = self.pending_backups.pop() {
                    // One disk of a multi-disk run finished: record it, then
                    // re-aim the subject at the next disk before spawning it.
                    self.record_job(
                        job_kind,
                        phoenix_core::appdata::JobOutcome::Success,
                        &final_snap,
                        job_verified,
                    );
                    self.current_backup_index += 1;
                    let path_display = next.output.display().to_string();
                    self.status = format!(
                        "Backing up disk {} of {} to {}…",
                        self.current_backup_index, self.total_backups, path_display
                    );
                    self.job_subject = self.backup_subject(&next);
                    self.job = Some(spawn_backup(next));
                } else {
                    let multi = self.total_backups > 1;
                    self.status = if multi {
                        format!("All {} backups completed", self.total_backups)
                    } else {
                        msg
                    };
                    // A restore that wrote its image, or a verify that read one
                    // through, is done with that file: clear the path field
                    // (shared by both pages) along with the loaded layout, so
                    // neither page sits there armed to re-run the same job.
                    if job_kind == JobKind::Restore || job_kind == JobKind::Verify {
                        self.restore_backup_path.clear();
                        self.clear_restore_ui_state();
                    }
                    if job_kind == JobKind::Clone {
                        self.clear_clone_ui_state();
                    }
                    self.total_backups = 0;
                    self.current_backup_index = 0;
                    self.record_job(
                        job_kind,
                        phoenix_core::appdata::JobOutcome::Success,
                        &final_snap,
                        job_verified,
                    );
                    // Offer "Verify?" only for a single unverified backup:
                    // for a multi-disk run the parked modal represents the
                    // whole queue, but the job only knows its own file.
                    let verify_target = (job_kind == JobKind::Backup && !job_verified && !multi)
                        .then_some(job_output)
                        .flatten();
                    self.finish_modal(
                        job_kind,
                        &final_snap,
                        JobOutcome::Success,
                        job_verified,
                        verify_target,
                    );
                    // The boot-repair result is multi-line (one line per
                    // action performed). The modal just captured the full
                    // text; the one-line status bar keeps only the headline.
                    if job_kind == JobKind::BootRepair {
                        if let Some(first) = self.status.lines().next() {
                            self.status = first.to_string();
                        }
                    }
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
                self.record_job(job_kind, rec_outcome, &final_snap, job_verified);
                // A finished clone — success, failure, or cancel — resets
                // the Clone page to its default view so a stale plan can't
                // sit there looking like it still needs to be started.
                if job_kind == JobKind::Clone {
                    self.clear_clone_ui_state();
                }
                self.finish_modal(job_kind, &final_snap, outcome, job_verified, None);
            }
        }

        // The finished job may have rewritten a partition table or written a
        // new image file — reload the disk list so the drive dropdowns don't
        // show the pre-job world. A verify changes nothing on disk, and a
        // mid-queue backup completion (self.job re-armed above) waits for the
        // whole queue to drain.
        if self.job.is_none() && job_kind != JobKind::Verify {
            self.pending_disk_reload = true;
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
        // Every finished job states its verdict under the modal's badge —
        // whichever kind it was, however it ended. A successful backup's wording
        // also records whether the verify pass ran.
        let banner = match outcome {
            JobOutcome::Success => match kind {
                JobKind::Backup if verified_backup => "Completed and verified.".to_string(),
                JobKind::Backup => "Completed. Unverified.".to_string(),
                JobKind::Verify => "Backup verified.".to_string(),
                JobKind::Restore => "Restore complete.".to_string(),
                JobKind::Clone => "Clone complete.".to_string(),
                JobKind::BootRepair => "Boot repair complete.".to_string(),
            },
            // The only thing that raises a Warning is the user cancelling.
            JobOutcome::Warning => format!("{}.", kind.cancelled_message()),
            JobOutcome::Failure => format!("{} failed.", kind.noun()),
        };
        // The detail line under the badge must not restate it. A job whose
        // status text ("Full verify OK: <path>", "Restore completed") says
        // nothing the banner hasn't already said drops to the one fact the
        // banner leaves out — the image that was verified — or to nothing.
        // The status bar keeps the fuller wording; it has no badge above it.
        let message = match (outcome, kind) {
            (JobOutcome::Success, JobKind::Verify) => self
                .job_subject
                .image_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            (JobOutcome::Success, JobKind::Restore) => String::new(),
            _ => self.status.clone(),
        };
        self.completed = Some(CompletedJob {
            title: kind.noun().to_string(),
            steps: snap.steps.clone(),
            current_step: snap.current_step,
            outcome,
            message,
            banner,
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
        self.job_subject = JobSubject {
            source: path.display().to_string(),
            target: String::new(),
            image_path: Some(path.clone()),
        };
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

    /// Poll Windows for theme changes (accent color always; light/dark only
    /// while the theme choice is System — `theme::refresh` resolves that).
    /// `update` only runs when something triggers a repaint, so ask for one
    /// at the poll interval to keep tracking Windows while the app is idle.
    fn maybe_refresh_theme(&mut self, ctx: &egui::Context) {
        ctx.request_repaint_after(THEME_REFRESH_INTERVAL);
        if self.last_theme_refresh.elapsed() >= THEME_REFRESH_INTERVAL {
            let running_job_modal = self.job.is_some();
            self.palette = theme::refresh(ctx, self.settings.theme);
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

    /// The current page's primary Start action, if it has one — rendered by
    /// the sticky bottom action bar (`action_bar::show`) instead of inline
    /// in the page content. Enabled/hint state is derived from the same app
    /// state the page forms edit, so the bar stays in step with them.
    fn current_page_action(&self, busy: bool) -> Option<StartAction<'static>> {
        let action = match self.page {
            Page::Backup => {
                // Mirror `start_backup`'s own guards exactly — it takes the
                // name through `sanitize_backup_name` (so a bare ".phnx"
                // sanitizes to nothing) and refuses an empty selection. Gating
                // on anything looser enables a button that then declines the
                // click with a status message, which reads as a broken button.
                let name_ready = !sanitize_backup_name(&self.backup_name).is_empty();
                StartAction {
                    label: "Start Backup",
                    icon: Some(egui_phosphor::fill::PLAY),
                    enabled: !busy && name_ready && !self.selections.is_empty(),
                    disabled_hint: Some(if busy {
                        "A backup is already running"
                    } else if !name_ready {
                        "Enter a backup name first"
                    } else {
                        "Select at least one partition to back up"
                    }),
                }
            }
            Page::Restore => {
                let plan_ready = self
                    .restore_layout
                    .as_ref()
                    .is_some_and(|l| l.has_restorable_entries());
                StartAction {
                    label: "Run Restore",
                    icon: Some(egui_phosphor::fill::PLAY),
                    enabled: !busy && plan_ready,
                    disabled_hint: Some(if busy {
                        "A restore is already running"
                    } else if self.restore_layout.is_none() {
                        "Choose a backup file first"
                    } else if self.target_disk_index.is_none() {
                        "Choose a target disk first"
                    } else {
                        "Map at least one partition onto the target"
                    }),
                }
            }
            Page::Verify => StartAction {
                label: "Verify Backup",
                icon: Some(egui_phosphor::fill::PLAY),
                enabled: !busy && !self.restore_backup_path.trim().is_empty(),
                disabled_hint: Some(if busy {
                    "A verify is already running"
                } else {
                    "Choose a backup file first"
                }),
            },
            Page::Clone => {
                let plan_ready = self
                    .clone_layout
                    .as_ref()
                    .is_some_and(|l| l.has_restorable_entries());
                StartAction {
                    label: "Clone Disk",
                    icon: Some(egui_phosphor::fill::PLAY),
                    enabled: !busy && plan_ready,
                    disabled_hint: Some(if busy {
                        "A job is already running"
                    } else if self.clone_layout.is_none() {
                        "Choose a source and a target disk first"
                    } else {
                        "Drag at least one source partition onto the target"
                    }),
                }
            }
            Page::Mount => {
                // No WinFsp, no mounting — the page shows an install notice in
                // place of the picker, so there is nothing to act on: withhold
                // the bar entirely rather than leave a permanently-dead Start.
                if !self.mount_available {
                    return None;
                }
                let already_mounted = self.selected_backup_already_mounted();
                StartAction {
                    label: "Mount (Read-Only)",
                    icon: Some(egui_phosphor::fill::PLAY),
                    enabled: !busy
                        && !already_mounted
                        && self.mount_source.is_some()
                        && !self.mount_selection.is_empty(),
                    disabled_hint: Some(if busy {
                        "A job is already running"
                    } else if already_mounted {
                        "That backup is already mounted — see Active mounts below"
                    } else if self.mount_source.is_none() {
                        "Choose a backup file first"
                    } else {
                        "Select at least one partition to mount"
                    }),
                }
            }
            Page::Virtualize => {
                // No QEMU or a VM already running → withhold the bar (the page
                // shows an install notice / the running-VM controls instead).
                if self.qemu.is_none() || self.vm_run.is_some() {
                    return None;
                }
                let ready = self.vm.summary.as_ref().is_some_and(|s| s.blocker.is_none());
                StartAction {
                    label: "Boot VM",
                    icon: Some(egui_phosphor::fill::PLAY),
                    enabled: !busy && ready,
                    disabled_hint: Some(if busy {
                        "A job is already running"
                    } else if self.vm.summary.is_none() {
                        "Choose a backup file first"
                    } else {
                        "This backup can't boot — see the note above"
                    }),
                }
            }
            Page::BootRepair => {
                let plan_ok = self
                    .bootrepair_plan
                    .as_ref()
                    .is_some_and(|(sel, plan)| Some(*sel) == self.bootrepair_selected && plan.is_ok());
                StartAction {
                    label: "Repair Boot",
                    icon: Some(egui_phosphor::fill::PLAY),
                    enabled: !busy && plan_ok,
                    disabled_hint: Some(if busy {
                        "A job is already running"
                    } else if self.bootrepair_selected.is_none() {
                        "Select a Windows installation first"
                    } else {
                        "The selected installation can't be repaired — see the note above"
                    }),
                }
            }
            Page::History | Page::About => return None,
        };
        Some(action)
    }
}

impl PhoenixApp {
    /// One egui frame: poll workers, refresh theme, then lay out the sidebar,
    /// titlebar, sticky action bar, central page, and the modals over the top.
    /// Called by `Renderer::paint` inside `egui_ctx.run`. (The old eframe
    /// `clear_color` — sidebar_bg for the rounded-corner cutout — is now the
    /// background fill in `Renderer::paint`.)
    fn draw(&mut self, ctx: &egui::Context) {
        self.poll_job(ctx);
        self.poll_updater(ctx);
        self.maybe_refresh_theme(ctx);
        // Runs the refresh a click armed LAST frame — that frame already
        // painted the dim/spinner overlay, so it stays on screen while the
        // enumeration below blocks.
        self.run_pending_refresh();
        // A job finished last frame: the completion modal is already on
        // screen, so a blocking enumeration here is invisible. Status stays
        // whatever the job set it to; failure just leaves the old list.
        if self.pending_disk_reload {
            self.pending_disk_reload = false;
            if let Err(e) = self.reload_disks() {
                tracing::warn!("post-job disk reload failed: {e}");
            }
        }
        // Both pages that name a `.phnx` want it parsed: the Restore page to
        // seed its layout editor, the Verify page to preview the contents.
        if matches!(self.page, Page::Restore | Page::Verify) {
            self.poll_restore_backup_load();
        }

        // The close-with-mounts guard lives in the winit loop's CloseRequested
        // handler now (it sets `pending_close`, which counts as a modal below so
        // the page behind the dialog goes inert) — nothing to do here per frame.

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
        // The sidebar owns the full window height; its brand block doubles
        // as a drag handle for the chromeless window, so the rect is handed
        // to `titlebar::show` along with the floating corner pill. That pill
        // hosts the app's one Refresh button — live only when the window is
        // otherwise idle (no job, no modal, no refresh already in flight).
        // Belt and braces: the sidebar hides unavailable pages, but nothing
        // stops `self.page` holding one that arrived by another route. Fall
        // back rather than render a page this build cannot support.
        if !sidebar::page_available(self.page, self.portable) {
            self.page = Page::Backup;
        }
        // Navigation is frozen while a VM runs, the same way a job freezes
        // it. The Virtualize page is the only place to see or stop the VM,
        // and wandering off it made it far too easy to close the app and
        // pull the served disk out from under a live guest.
        let nav = sidebar::show(
            ctx,
            &mut self.page,
            &self.palette,
            modal_open || self.vm_run.is_some(),
            &mut self.settings.theme,
            self.portable,
        );
        let brand_rect = nav.brand_rect;
        if nav.theme_changed {
            self.palette = theme::refresh(ctx, self.settings.theme);
            self.last_theme_refresh = Instant::now();
            let _ = self.settings.save();
        }
        let can_refresh = !busy && !modal_open && self.refresh_overlay_until.is_none();
        let refresh_key = ctx.input(|i| i.key_pressed(egui::Key::F5));
        let refresh_clicked = titlebar::show(ctx, &self.palette, brand_rect, can_refresh);
        if refresh_clicked || (can_refresh && refresh_key) {
            self.begin_refresh();
        }

        // Slim bottom status line for idle/non-job messages (disk count,
        // "Loaded backup…", load errors, "Ready"). While a job runs or its
        // result modal is up, the blocking modal overlays this bar.
        egui::TopBottomPanel::bottom("status")
            .show_separator_line(false)
            .frame(
                egui::Frame::none()
                    .fill(self.palette.sidebar_bg)
                    .inner_margin(egui::Margin::symmetric(16.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(!modal_open, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&self.status).color(self.palette.subtle_text),
                        );
                        // A staged update offers itself on the right as the one
                        // action the banner implies: close, install, come back
                        // on the new version. The left status line already
                        // carries the "installs when you close" wording, so this
                        // is a bare link rather than the same sentence twice.
                        if self.update_banner.is_some() {
                            let clicked = ui
                                .with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| ui.link("Restart to update").clicked(),
                                )
                                .inner;
                            if clicked {
                                self.restart_after_update = true;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                    });
                });
            });

        // Sticky primary-action bar: pages with a Start action pin it to the
        // bottom of the pane (between the content and the status bar) so the
        // page content scrolls while the action stays put. Shown before the
        // CentralPanel because egui panels claim space in declaration order.
        let page_action = self.current_page_action(busy);
        let mut action_bar_rect: Option<egui::Rect> = None;
        if let Some(action) = &page_action {
            let (clicked, bar_rect) = action_bar::show(ctx, &self.palette, action, !modal_open);
            action_bar_rect = Some(bar_rect);
            if clicked {
                match self.page {
                    Page::Backup => self.start_backup(),
                    Page::Restore => self.start_restore(),
                    Page::Verify => self.start_verify(),
                    Page::Clone => self.start_clone(),
                    Page::Mount => self.start_mount(),
                    Page::Virtualize => self.request_start_vm(),
                    Page::BootRepair => self.start_boot_repair(),
                    _ => {}
                }
            }
        }

        // Only the bottom-left corner is rounded: it nestles into the L
        // formed by the sidebar and status bar. The cutout shows through to
        // `clear_color` (sidebar_bg). When the action bar is up it forms the
        // pane's bottom edge and owns that corner, so the panel goes square.
        let panel_rounding = if action_bar_rect.is_some() {
            egui::Rounding::ZERO
        } else {
            egui::Rounding {
                sw: action_bar::CORNER_RADIUS,
                ..egui::Rounding::ZERO
            }
        };
        let central = egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(CENTRAL_PANEL_MARGIN_X, 20.0))
                    .rounding(panel_rounding),
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
                    // Not gated on `busy`: a running VM lives on a background
                    // thread, not a modal job, so the page stays interactive
                    // (Stop button, sessions). `modal_open` still disables it.
                    Page::Virtualize => self.ui_virtualize(ui),
                    Page::BootRepair => disabled_when(ui, busy, |ui| self.ui_bootrepair(ui)),
                    Page::History => disabled_when(ui, busy, |ui| self.ui_history(ui)),
                    Page::About => disabled_when(ui, busy, |ui| self.ui_about(ui)),
                });
            });

        // 1px outline along the panel's edge against the sidebar/status-bar
        // L. Drawn by hand rather than via `Frame::stroke` because the top
        // and right sides sit on the window edge and must stay line-free:
        // pushing those sides 1px past the screen clips their stroke away,
        // leaving only the left edge, bottom edge, and the rounded corner.
        let rect = central.response.rect;
        // The border wraps the whole pane — central panel plus the action
        // bar when one is up — with the rounded corner always at the pane's
        // true bottom-left (the bar's corner when present).
        let bottom = action_bar_rect.map_or(rect.max.y, |r| r.max.y);
        let border_rect = egui::Rect::from_min_max(
            egui::pos2(rect.min.x, rect.min.y - 1.0),
            egui::pos2(rect.max.x + 1.0, bottom),
        );
        ctx.layer_painter(egui::LayerId::background()).rect_stroke(
            border_rect,
            egui::Rounding {
                sw: action_bar::CORNER_RADIUS,
                ..egui::Rounding::ZERO
            },
            egui::Stroke::new(1.0, self.palette.panel_border),
        );

        self.show_status_modal(ctx);
        self.show_overwrite_dialog(ctx);
        self.show_clone_confirm_dialog(ctx);
        self.show_restore_confirm_dialog(ctx);
        self.show_boot_repair_confirm_dialog(ctx);
        self.show_history_delete_dialog(ctx);
        self.show_demo_confirm_dialog(ctx);
        self.show_demo_progress_modal(ctx);
        self.show_close_dialog(ctx);
        self.show_enable_write_dialog(ctx);
        self.show_vm_boot_dialog(ctx);
        self.show_refresh_overlay(ctx);
    }

    /// Last chance to tear the mounts down: a mount outlives its `ActiveMount`
    /// only as a stuck drive letter and a detached-but-attached virtual disk,
    /// so the app never exits while one is up. Covers the paths that skip the
    /// close dialog (a mount can only exist there if the user confirmed, but
    /// `on_exit` also runs on OS session-end).
    fn on_exit(&mut self) {
        self.unmount_all();
        self.apply_staged_update_on_exit();
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
                finished: None,
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
                finished: Some(Verdict {
                    outcome: completed.outcome,
                    banner: &completed.banner,
                }),
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
                finished: None,
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
            let mut view = ConfirmView {
                title: "Backup file already exists",
                message: &message,
                details: &pending.existing,
                confirm_label: "Overwrite",
                cancel_label: "Cancel",
                confirm_danger: true,
                hazard_tape: true,
                ack: None,
                extra_label: None,
            };
            confirm_dialog::show(ctx, &self.palette, &mut view)
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
            ConfirmAction::None | ConfirmAction::Extra => {}
        }
    }

    /// Final confirm-the-target dialog over a parked clone: restates the
    /// target disk's number/size/model so a wrong-disk pick is caught before
    /// the first byte is written. Confirm launches the worker; Cancel drops
    /// the parked plan and leaves the page untouched.
    fn show_clone_confirm_dialog(&mut self, ctx: &egui::Context) {
        // Taken out so the acknowledgement checkbox can borrow `pending.ack`
        // mutably while the rest of the view borrows other fields; re-parked on
        // ConfirmAction::None (dialog still up).
        let Some(mut pending) = self.pending_clone.take() else {
            return;
        };
        let convert = pending.convert;
        let title = if convert {
            "Convert 4Kn → 512e and clone"
        } else {
            "Confirm clone target"
        };
        let confirm_label = if convert {
            "Convert and clone"
        } else {
            "Clone disk"
        };
        let action = {
            let ack = convert.then_some(ConfirmAck {
                label: CONVERSION_ACK_LABEL,
                checked: &mut pending.ack,
            });
            let mut view = ConfirmView {
                title,
                message: &pending.message,
                details: &pending.details,
                confirm_label,
                cancel_label: "Cancel",
                confirm_danger: true,
                hazard_tape: true,
                ack,
                extra_label: None,
            };
            confirm_dialog::show(ctx, &self.palette, &mut view)
        };
        match action {
            ConfirmAction::Confirm => {
                self.completed = None;
                self.status = format!(
                    "Cloning disk {} → disk {}…",
                    pending.opts.source_disk_index, pending.opts.target_disk_index
                );
                self.job_subject = pending.subject;
                self.job = Some(spawn_clone(pending.opts));
            }
            ConfirmAction::Cancel => {
                self.status = "Clone cancelled — no changes were made".into();
            }
            ConfirmAction::None | ConfirmAction::Extra => self.pending_clone = Some(pending),
        }
    }

    /// The restore counterpart of [`show_clone_confirm_dialog`].
    fn show_restore_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut pending) = self.pending_restore.take() else {
            return;
        };
        let convert = pending.convert;
        let title = if convert {
            "Convert 4Kn → 512e and restore"
        } else {
            "Confirm restore target"
        };
        let message = if convert {
            "This backup is from a 4096-byte-sector (4Kn) disk; the target uses 512-byte sectors \
             (512e). To make it usable, the filesystem boot sectors will be rewritten. The result \
             is a CONVERTED COPY, not a byte-identical restore. Verify this is the right disk:"
        } else {
            "This will PERMANENTLY OVERWRITE the planned partitions on the target disk with the \
             backup's contents. Verify this is the right disk:"
        };
        let confirm_label = if convert {
            "Convert and restore"
        } else {
            "Run restore"
        };
        let action = {
            let ack = convert.then_some(ConfirmAck {
                label: CONVERSION_ACK_LABEL,
                checked: &mut pending.ack,
            });
            let mut view = ConfirmView {
                title,
                message,
                details: &pending.details,
                confirm_label,
                cancel_label: "Cancel",
                confirm_danger: true,
                hazard_tape: true,
                ack,
                extra_label: None,
            };
            confirm_dialog::show(ctx, &self.palette, &mut view)
        };
        match action {
            ConfirmAction::Confirm => {
                // Surfaces while `run_restore` does its pre-flight work
                // (opening the .phnx, validating, enumerating disks) before
                // the worker's first `set_phase` takes over the status text.
                self.completed = None;
                self.status = "Restoring, please wait…".into();
                self.job_subject = pending.subject;
                self.job = Some(spawn_restore(pending.opts));
            }
            ConfirmAction::Cancel => {
                self.status = "Restore cancelled — no changes were made".into();
            }
            ConfirmAction::None | ConfirmAction::Extra => self.pending_restore = Some(pending),
        }
    }

    /// The boot-repair counterpart of [`show_clone_confirm_dialog`]. When the
    /// target is the disk the running system booted from, Confirm is
    /// additionally gated on an acknowledgement checkbox.
    fn show_boot_repair_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut pending) = self.pending_boot_repair.take() else {
            return;
        };
        let action = {
            let ack = pending.system_disk.then_some(ConfirmAck {
                label: BOOT_REPAIR_SYSTEM_ACK_LABEL,
                checked: &mut pending.ack,
            });
            let mut view = ConfirmView {
                title: "Confirm boot repair",
                message: &pending.message,
                details: &pending.details,
                confirm_label: "Repair boot",
                cancel_label: "Cancel",
                confirm_danger: true,
                hazard_tape: true,
                ack,
                extra_label: None,
            };
            confirm_dialog::show(ctx, &self.palette, &mut view)
        };
        match action {
            ConfirmAction::Confirm => {
                self.completed = None;
                self.status = format!("Repairing boot files on disk {}…", pending.disk_index);
                self.job_subject = pending.subject;
                self.job = Some(spawn_boot_repair(
                    pending.disk_index,
                    pending.partition_index,
                ));
            }
            ConfirmAction::Cancel => {
                self.status = "Boot repair cancelled — no changes were made".into();
            }
            ConfirmAction::None | ConfirmAction::Extra => self.pending_boot_repair = Some(pending),
        }
    }

    /// Closing with backups still mounted: the mounts cannot outlive the
    /// process (their drive letters and virtual disks are owned by it), so the
    /// close is held until the user accepts that they come down. Hazard tape
    /// because a live drive letter can have open files behind it, and Windows
    /// pulling it out from under Explorer is exactly the kind of surprise the
    /// tape is for.
    fn show_close_dialog(&mut self, ctx: &egui::Context) {
        if !self.pending_close {
            return;
        }
        // Nothing left to warn about (the last mount came down, or the VM
        // stopped, while the dialog was up): let the close through.
        if !self.close_needs_confirm() {
            self.pending_close = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let vm_running = self.vm_run.is_some();
        let mut details = Vec::new();
        if let Some(run) = &self.vm_run {
            details.push(format!("Running VM: {}", run.name));
        }
        details.extend(self.mount_summaries());

        // The VM warning leads when there is one: losing unsaved work inside
        // a guest is a bigger deal than a disconnected drive letter, and the
        // power-off is immediate rather than orderly.
        let mounted = self.anything_mounted();
        let mount_count = self.mount_summaries().len();
        let mount_clause = format!(
            "unmounts {} and removes {} drive letters, disconnecting any open \
             files or Explorer windows on those drives",
            if mount_count == 1 {
                "the mounted backup".to_string()
            } else {
                format!("all {mount_count} mounted backups")
            },
            if mount_count == 1 { "its" } else { "their" },
        );
        let message = match (vm_running, mounted) {
            (true, true) => format!(
                "Closing Phoenix Simulacra immediately powers off the running \
                 virtual machine — the guest gets no chance to shut down, and \
                 unsaved work inside it is lost. The app serves the disk the \
                 guest is running from, so it cannot keep running without it. \
                 Closing also {mount_clause}."
            ),
            (true, false) => "Closing Phoenix Simulacra immediately powers off the running \
                 virtual machine — the guest gets no chance to shut down, and unsaved \
                 work inside it is lost. The app serves the disk the guest is running \
                 from, so it cannot keep running without it.\n\nTo shut the guest down \
                 properly, cancel and use Stop VM instead."
                .to_string(),
            (false, _) => format!("Closing Phoenix Simulacra {mount_clause}."),
        };
        let title = if vm_running {
            "A virtual machine is still running"
        } else {
            "Backups are still mounted"
        };
        let confirm_label = if vm_running {
            "Power Off & Close"
        } else {
            "Unmount & Close"
        };
        let mut view = ConfirmView {
            title,
            message: &message,
            details: &details,
            confirm_label,
            cancel_label: "Keep Open",
            confirm_danger: vm_running,
            hazard_tape: true,
            ack: None,
            extra_label: None,
        };
        match confirm_dialog::show(ctx, &self.palette, &mut view) {
            ConfirmAction::Confirm => {
                self.pending_close = false;
                // Power off first: the VM's disk is served by this process,
                // so it must stop before the mounts it rides on come down.
                self.power_off_vm_now();
                self.unmount_all();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ConfirmAction::Cancel => self.pending_close = false,
            ConfirmAction::None | ConfirmAction::Extra => {}
        }
    }

    /// `--demo-confirm` styling aid: the restore dialog with canned details.
    fn show_demo_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.demo_confirm {
            return;
        }
        let details = vec![
            "Disk number:   2".to_string(),
            "Size:          931.5 GB".to_string(),
            "Model:         WDC WD10EZEX-08WN4A0".to_string(),
        ];
        let mut view = ConfirmView {
            title: "Confirm restore target",
            message: "This will PERMANENTLY OVERWRITE the planned partitions on the target \
                      disk with the backup's contents. Verify this is the right disk:",
            details: &details,
            confirm_label: "Run restore",
            cancel_label: "Cancel",
            confirm_danger: true,
            hazard_tape: true,
            ack: None,
            extra_label: None,
        };
        if confirm_dialog::show(ctx, &self.palette, &mut view) != ConfirmAction::None {
            self.demo_confirm = false;
        }
    }

    /// Cheap eligibility check before offering write access, so the common
    /// refusal (a locked BitLocker partition, whose image is raw ciphertext)
    /// happens WITHOUT first tearing down the read-only mount. Returns the
    /// message to show on refusal.
    fn enable_write_precheck(&self, index: usize) -> std::result::Result<(), String> {
        let mount = &self.mounts[index];
        let selected: Vec<u32> = mount.volumes().iter().map(|v| v.partition_index).collect();
        let reader = phoenix_core::container::PhnxReader::open(mount.backup_path())
            .map_err(|e| format!("Couldn't read the backup to check BitLocker state: {e}"))?;
        for pm in &reader.manifest.partitions {
            if selected.contains(&pm.index) && pm.bitlocker.as_deref() == Some("locked") {
                return Err(format!(
                    "Write access is unavailable: partition {} is a locked BitLocker volume, so \
                     its captured data is raw ciphertext.",
                    pm.index
                ));
            }
        }
        Ok(())
    }

    /// The hazard-taped confirmation for enabling temporary write access. Makes
    /// unmistakable that changes go to a scratch layer and are discarded, and
    /// gates the action behind an acknowledgement checkbox.
    fn show_enable_write_dialog(&mut self, ctx: &egui::Context) {
        let Some(index) = self.pending_enable_write else {
            return;
        };
        // Resolve the target's name + letters for the details block (real mount
        // first, then demo rows). If it vanished, drop the dialog.
        let real = self.mounts.len();
        let (name, letters): (String, Vec<char>) = if index < real {
            let m = &self.mounts[index];
            (
                m.backup_path()
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "backup".into()),
                m.volumes().iter().filter_map(|v| v.drive_letter).collect(),
            )
        } else if let Some(row) = self.demo_mounts.get(index - real) {
            (row.name.clone(), row.letters.clone())
        } else {
            self.pending_enable_write = None;
            return;
        };
        let letters_str = if letters.is_empty() {
            "(no drive letters)".to_string()
        } else {
            letters
                .iter()
                .map(|l| format!("{l}:"))
                .collect::<Vec<_>>()
                .join("   ")
        };
        let details = vec![format!("Backup:  {name}"), format!("Drives:  {letters_str}")];

        let mut ack_checked = self.enable_write_ack;
        let mut view = ConfirmView {
            title: "Enable temporary write access?",
            message: "This re-mounts the backup read/write using a copy-on-write overlay. You can \
                      create, edit and delete files on the drive — but every change is written to \
                      a temporary scratch layer, NEVER to the backup. All changes are DISCARDED \
                      when you unmount or close the app, and the backup image is never modified.",
            details: &details,
            confirm_label: "Enable Write Access",
            cancel_label: "Cancel",
            confirm_danger: true,
            hazard_tape: true,
            ack: Some(ConfirmAck {
                label: "I understand my changes are temporary and will be discarded on unmount",
                checked: &mut ack_checked,
            }),
            extra_label: None,
        };
        let result = confirm_dialog::show(ctx, &self.palette, &mut view);
        self.enable_write_ack = ack_checked;
        match result {
            ConfirmAction::Confirm => {
                self.pending_enable_write = None;
                self.enable_write_ack = false;
                self.perform_enable_write(index);
            }
            ConfirmAction::Cancel => {
                self.pending_enable_write = None;
                self.enable_write_ack = false;
            }
            ConfirmAction::None | ConfirmAction::Extra => {}
        }
    }

    /// Convert a read-only mount into a writable copy-on-write overlay in place.
    /// The read-only mount must be ejected first — its parent is attached with
    /// the same GPT identity the writable child carries, and Windows will not
    /// bring two such disks online at once — so on failure we put the read-only
    /// mount back rather than leave the drive gone.
    fn perform_enable_write(&mut self, index: usize) {
        let real = self.mounts.len();
        if index >= real {
            // Demo row: just flip its state so the flow can be previewed.
            if let Some(row) = self.demo_mounts.get_mut(index - real) {
                row.writable = true;
            }
            self.status = "Write access enabled (demo row)".into();
            return;
        }

        // Capture everything we need before ejecting the read-only mount.
        let backup = self.mounts[index].backup_path().to_path_buf();
        let vols = self.mounts[index].volumes().to_vec();
        let selection: Vec<u32> = vols.iter().map(|v| v.partition_index).collect();
        let preferred: Vec<(u32, char)> = vols
            .iter()
            .filter_map(|v| v.drive_letter.map(|l| (v.partition_index, l)))
            .collect();
        let scratch = phoenix_mount::mount_scratch_dir();
        let name = backup
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Eject the read-only mount (frees its letters, detaches its disk).
        drop(self.mounts.remove(index));

        match phoenix_mount::ActiveMount::open_writable_selected(
            &backup, &scratch, &selection, &preferred,
        ) {
            Ok(m) => {
                let letters = describe_mounted_volumes(&m);
                self.mounts.insert(index, m);
                self.status = format!(
                    "Write access enabled for {name} — {letters}. Changes are temporary and \
                     discarded on unmount."
                );
            }
            Err(e) => {
                // Best-effort: restore the read-only mount so the drive returns.
                match phoenix_mount::ActiveMount::open_selected(&backup, &scratch, Some(&selection))
                {
                    Ok(m) => {
                        self.mounts.insert(index, m);
                        self.status = format!("Couldn't enable write access: {e} — kept read-only.");
                    }
                    Err(e2) => {
                        self.status = format!(
                            "Couldn't enable write access ({e}); re-mounting read-only also failed \
                             ({e2}). The backup is now unmounted."
                        );
                    }
                }
            }
        }
    }

    /// `--demo-progress` styling aid: the status modal mid-backup, its bar
    /// sweeping 0→100% on a loop so the barber pole and the byte readout can be
    /// eyeballed at every fill level (and in both themes) without a real job.
    fn show_demo_progress_modal(&mut self, ctx: &egui::Context) {
        if !self.demo_progress {
            return;
        }
        const SWEEP_SECS: f64 = 12.0;
        const TOTAL: u64 = 931_500_000_000;
        let fraction = ((ctx.input(|i| i.time) % SWEEP_SECS) / SWEEP_SECS) as f32;

        let steps = vec![
            "Reading partition table".to_string(),
            "Capturing partition 1 (EFI System, 260 MB)".to_string(),
            "Capturing partition 2 (Basic data, 931.2 GB)".to_string(),
            "Verifying backup".to_string(),
        ];
        let view = ModalView {
            title: "Backing up",
            steps: &steps,
            current_step: 2,
            detail: "Writing chunk 4,182 of 14,206…",
            fraction,
            current_bytes: (TOTAL as f64 * fraction as f64) as u64,
            total_bytes: TOTAL,
            finished: None,
            offer_verify: false,
        };
        if status_modal::show(ctx, &self.palette, &view) != ModalAction::None {
            self.demo_progress = false;
        }
        ctx.request_repaint();
    }
}

/// Wrap a "no own action row yet" page in `add_enabled_ui(!busy, …)`
/// so its widgets grey out while another page's job is running.
fn disabled_when<R>(ui: &mut egui::Ui, busy: bool, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.add_enabled_ui(!busy, body).inner
}

/// A page's primary "start" affordance, rendered by the sticky bottom
/// action bar (`action_bar::show`): green gradient bar when armed,
/// monochrome hazard tape while `enabled` is false, with an optional
/// `disabled_hint` shown on hover when the user cannot click it.
pub struct StartAction<'a> {
    pub label: &'a str,
    /// Optional phosphor glyph rendered before the label (e.g.
    /// [`egui_phosphor::regular::PLAY`] on the Start backup button).
    pub icon: Option<&'a str>,
    pub enabled: bool,
    pub disabled_hint: Option<&'a str>,
}

/// A recorded job's start time as local wall-clock: `2026-07-12 14:32:07`.
/// Deliberately absolute — "3 hours ago" tells you nothing the next time you
/// open the app, and a backup's date is exactly what you came to the History
/// page to find out.
fn format_timestamp(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "—".to_string())
}

/// The Action column: what the job was.
fn job_action_label(kind: phoenix_core::appdata::JobKindTag) -> &'static str {
    use phoenix_core::appdata::JobKindTag as K;
    match kind {
        K::Backup => "Backup",
        K::Restore => "Restore",
        K::Verify => "Verify",
        K::Clone => "Clone",
        K::BootRepair => "Boot repair",
    }
}

/// The Details column: what the job did, to what, and — when a `.phnx` was
/// involved — how big it ended up.
fn describe_job(rec: &phoenix_core::appdata::JobRecord) -> String {
    use phoenix_core::appdata::JobKindTag as K;
    let size = match rec.image_bytes {
        0 => String::new(),
        n => format!(" ({})", format_bytes(n)),
    };
    match rec.kind {
        K::Backup => format!("Imaged {} → {}{size}", rec.source, rec.target),
        K::Verify => format!("Fully verified {}{size}", rec.source),
        K::Restore => format!("Restored {} → {}", rec.source, rec.target),
        K::Clone => format!("Cloned {} → {}", rec.source, rec.target),
        K::BootRepair => format!("Repaired boot files for {} on {}", rec.source, rec.target),
    }
}

/// Inline "<phosphor glyph> label" button text. Needs a `LayoutJob`
/// because the glyph must resolve through the phosphor font family while
/// the label stays in Inter — a single `RichText` can only carry one font.
fn icon_label(
    icon: &str,
    icon_font: egui::FontId,
    label: &str,
    label_font: egui::FontId,
    color: egui::Color32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        icon,
        0.0,
        egui::TextFormat {
            font_id: icon_font,
            color,
            valign: egui::Align::Center,
            ..Default::default()
        },
    );
    job.append(
        label,
        8.0,
        egui::TextFormat {
            font_id: label_font,
            color,
            valign: egui::Align::Center,
            ..Default::default()
        },
    );
    job
}

/// Common page shell: the whole page scrolls vertically as a unit, with
/// the central panel's right margin reclaimed so the scrollbar rides the
/// window edge and the content padded `PAGE_RIGHT_MARGIN` back off it —
/// every control ends on the same vertical line, and the fixed action bar
/// below never moves while the content scrolls.
fn page_scroll_shell(ui: &mut egui::Ui, id_salt: &str, body: impl FnOnce(&mut egui::Ui)) {
    let mut full_rect = ui.max_rect();
    full_rect.max.x += CENTRAL_PANEL_MARGIN_X;
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(full_rect), |ui| {
        let out = egui::ScrollArea::vertical()
            .id_salt(id_salt)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::none()
                    .inner_margin(egui::Margin {
                        right: PAGE_RIGHT_MARGIN,
                        ..egui::Margin::ZERO
                    })
                    .show(ui, body);
            });
        // How much content sits below the fold right now.
        let below = out.content_size.y - (out.state.offset.y + out.inner_rect.height());
        paint_more_below(ui, out.inner_rect, below);
    });
}

/// Best-practices guide for booting backups as VMs.
const VM_WIKI_URL: &str = "https://github.com/steeb-k/phoenix-simulacra/wiki/Virtualization";

/// The "read the guide first" card under the Virtualize header.
///
/// Booting a backup has more sharp edges than the rest of the app — guest
/// tools, when to connect the network, what a session actually keeps — and
/// most of them are cheaper to read about than to discover. Styled as a card
/// rather than a plain line so it reads as part of the page furniture and
/// survives being scrolled past, but deliberately not hazard tape: the page
/// header already carries that, and a second alarm devalues the first.
fn vm_wiki_notice(ui: &mut egui::Ui, palette: &Palette) {
    egui::Frame::none()
        .fill(palette.content_card_bg)
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            // Two columns: the icon owns the left one at a size that reads as
            // a mark rather than punctuation, and the prose wraps in the
            // right one without flowing back under it. `horizontal` centres
            // on the cross axis, so the icon sits against the middle of
            // however many lines the text takes.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(egui_phosphor::regular::BOOK_OPEN)
                        .font(fonts::icon(30.0))
                        .color(palette.accent),
                );
                ui.add_space(14.0);
                ui.vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.label(
                            egui::RichText::new("You are strongly encouraged to read the")
                                .color(palette.subtle_text),
                        );
                        ui.hyperlink_to(
                            egui::RichText::new("Virtualization best practices guide")
                                .color(palette.accent),
                            VM_WIKI_URL,
                        );
                        ui.label(
                            egui::RichText::new(
                                "before booting a backup — it covers guest tools, \
                                 networking, and how sessions keep your changes.",
                            )
                            .color(palette.subtle_text),
                        );
                    });
                });
            });
        });
    ui.add_space(12.0);
}

/// Ignore an overflow smaller than this — a stray pixel or two of rounding
/// isn't "more to see", and an indicator that shows for it is noise.
const MORE_BELOW_MIN: f32 = 6.0;
/// One full grow-and-shrink of the line, in seconds. Slow reads as ambient;
/// fast reads as activity, which is what a progress bar looks like.
const MORE_BELOW_CYCLE: f64 = 3.4;

/// Mark the bottom edge of a scroll area that has more content below it: a
/// short line that breathes — widening and brightening together, then
/// narrowing and dimming — for as long as there is more to see.
///
/// Deliberately short and centred rather than full-width, and drawn between
/// the weak and strong text colours rather than in the accent: a full-width
/// animated line pinned to the bottom of a pane is the visual grammar of an
/// indeterminate progress bar, which this app uses for real elsewhere.
/// Short, centred and muted reads as a grab handle — "there is more here" —
/// instead. Width and brightness move on one phase so it reads as a single
/// breath rather than two effects.
fn paint_more_below(ui: &egui::Ui, rect: egui::Rect, below: f32) {
    if below <= MORE_BELOW_MIN {
        return;
    }
    const MIN_W: f32 = 42.0;
    const MAX_W: f32 = 88.0;

    let now = ui.ctx().input(|i| i.time);
    let phase = (now % MORE_BELOW_CYCLE) / MORE_BELOW_CYCLE;
    // Smooth 0→1→0 across the cycle, so growing and shrinking are symmetric
    // and neither end has a visible corner.
    let t = ((1.0 - (std::f64::consts::TAU * phase).cos()) / 2.0) as f32;

    let half = (MIN_W + (MAX_W - MIN_W) * t) / 2.0;
    // Between the theme's weak and strong text colours, so "brighter" means
    // more prominent in light mode as well as dark.
    let dim = ui.visuals().weak_text_color();
    let bright = ui.visuals().strong_text_color();
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    let color = egui::Color32::from_rgb(
        mix(dim.r(), bright.r()),
        mix(dim.g(), bright.g()),
        mix(dim.b(), bright.b()),
    );

    let y = rect.bottom() - 7.0;
    let mid = rect.center().x;
    ui.painter().line_segment(
        [egui::pos2(mid - half, y), egui::pos2(mid + half, y)],
        egui::Stroke::new(2.0, color),
    );
    ui.ctx().request_repaint();
}

/// Bold "label + TextEdit + Browse…" row used by the Verify, Restore, and
/// Mount pages for picking an existing `.phnx` file. Duplicates the Backup
/// page's "Target" row: inline 14pt bold label, field and button both
/// forced into `ACTION_BUTTON_HEIGHT` boxes via `add_sized` so they line
/// up exactly, and the button's width reserved on the right so the field
/// can't push it past the pane edge.
///
/// When `on_path_changed` is `Some`, it is called after the text field
/// loses focus with a changed value, and immediately after Browse picks a file.
/// Returns `true` if Browse selected a new file.
/// Every `.phnx` the job history has ever named — backups created, files
/// verified or restored from — plus every one reached via Browse…,
/// deduplicated case-insensitively, newest mention first, and filtered to
/// files that still exist so the dropdown never offers a dead path. Called
/// from inside the combo's popup closure, so the `is_file` stats only run
/// while the list is actually open.
///
/// Browsed paths come first: they were chosen by hand just now, which is a
/// stronger signal of intent than a job run months ago.
fn history_backup_choices(
    history: &phoenix_core::appdata::History,
    browsed: &[String],
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |cand: &String, out: &mut Vec<String>| {
        if !cand.to_ascii_lowercase().ends_with(".phnx") {
            return;
        }
        if seen.insert(cand.to_ascii_lowercase()) && Path::new(cand).is_file() {
            out.push(cand.clone());
        }
    };
    for cand in browsed {
        push(cand, &mut out);
    }
    for rec in history.records.iter().rev() {
        for cand in [&rec.target, &rec.source] {
            push(cand, &mut out);
        }
    }
    out
}

/// How many hand-browsed backups to remember. Enough to cover working with
/// a handful of external drives, short enough that the dropdown stays a
/// list rather than an archive.
const BROWSED_BACKUPS_MAX: usize = 12;

/// Record a hand-picked backup as the newest entry, dropping any duplicate
/// and any path that has since disappeared.
fn remember_browsed_backup(browsed: &mut Vec<String>, picked: &str) {
    let picked = picked.trim();
    if picked.is_empty() {
        return;
    }
    browsed.retain(|p| !p.eq_ignore_ascii_case(picked) && Path::new(p).is_file());
    browsed.insert(0, picked.to_string());
    browsed.truncate(BROWSED_BACKUPS_MAX);
}

/// The backup chooser shared by the Restore, Verify, and Mount pages: a
/// dropdown of every backup the history knows about (full path per row),
/// with Browse… as the escape hatch for a `.phnx` the history has never
/// seen. Deliberately not a text field — a picked-or-browsed path is always
/// well-formed, and typos in hand-typed paths were the only thing the old
/// free-text box ever contributed. Returns true when a path was chosen
/// (either way), so the caller can load it immediately.
fn backup_path_picker(
    ui: &mut egui::Ui,
    label: &str,
    hint: &str,
    path: &mut String,
    history: &phoenix_core::appdata::History,
    // Hand-browsed backups, newest first. Browse… appends to this, so a
    // backup the job history has never seen still shows up in the list.
    browsed: &mut Vec<String>,
    mut on_path_changed: Option<&mut dyn FnMut()>,
) -> bool {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).font(fonts::bold(14.0)));

        let spacing = ui.spacing().item_spacing.x;
        let combo_w = (ui.available_width() - FORM_BUTTON_W - spacing).max(0.0);
        let mut chosen = false;

        // The combo button's height comes from `interact_size`; raise it for
        // this row so the closed dropdown and the Browse button sit flush.
        ui.scope(|ui| {
            ui.spacing_mut().interact_size.y = ACTION_BUTTON_HEIGHT;
            let selected = if path.trim().is_empty() {
                egui::RichText::new(hint)
                    .font(fonts::regular(16.0))
                    .color(ui.visuals().weak_text_color())
            } else {
                egui::RichText::new(path.as_str()).font(fonts::regular(16.0))
            };
            egui::ComboBox::from_id_salt(("backup_path_combo", label))
                .width(combo_w)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    let choices = history_backup_choices(history, browsed);
                    if choices.is_empty() {
                        ui.label(
                            egui::RichText::new(
                                "No backups in the job history yet — use Browse…",
                            )
                            .color(ui.visuals().weak_text_color()),
                        );
                    }
                    for choice in choices {
                        let is_current = choice.eq_ignore_ascii_case(path);
                        if ui
                            .selectable_label(
                                is_current,
                                egui::RichText::new(&choice).font(fonts::regular(14.0)),
                            )
                            .clicked()
                            && !is_current
                        {
                            *path = choice;
                            chosen = true;
                        }
                    }
                });
        });
        if chosen {
            if let Some(cb) = on_path_changed.as_deref_mut() {
                cb();
            }
        }

        let browse_label = icon_label(
            egui_phosphor::regular::FOLDER,
            fonts::icon(16.0),
            "Browse…",
            fonts::regular(14.0),
            ui.visuals().widgets.inactive.fg_stroke.color,
        );
        if ui
            .add_sized(
                [FORM_BUTTON_W, ACTION_BUTTON_HEIGHT],
                egui::Button::new(browse_label),
            )
            .clicked()
        {
            if let Some(picked) = pick_backup_open_path(path) {
                *path = picked.display().to_string();
                remember_browsed_backup(browsed, path);
                chosen = true;
                if let Some(cb) = on_path_changed {
                    cb();
                }
            }
        }
        chosen
    })
    .inner
}

fn page_header(ui: &mut egui::Ui, palette: &Palette, title: &str, subtitle: &str) {
    page_header_badged(ui, palette, title, subtitle, "");
}

/// [`page_header`] plus a small, low-contrast word set beside the title —
/// for status that qualifies the whole page ("Experimental") and should read
/// as an aside rather than as part of the title.
fn page_header_badged(
    ui: &mut egui::Ui,
    palette: &Palette,
    title: &str,
    subtitle: &str,
    badge: &str,
) {
    ui.add_space(4.0);
    let title_font = fonts::bold(22.0);
    let title = ui.label(egui::RichText::new(title).font(title_font));
    if !badge.is_empty() {
        // To the right of the title, bottom-aligned to it so the two sit on
        // a common baseline. Painted rather than laid out: a bottom-aligned
        // *layout* fills whatever height it is handed, which once pushed
        // the whole page off the bottom of the screen, and painting also
        // guarantees the badge displaces nothing. Above the title it fell
        // under the window's drag-area reservation and got clipped; below
        // it simply looked wrong.
        ui.painter().text(
            egui::pos2(title.rect.right() + 8.0, title.rect.bottom()),
            egui::Align2::LEFT_BOTTOM,
            badge,
            fonts::regular(13.0),
            palette.subtle_text,
        );
    }
    if !subtitle.is_empty() {
        ui.label(egui::RichText::new(subtitle).color(palette.subtle_text));
    }
    ui.add_space(14.0);
}

/// Height of the decorative hazard band at the top of the Virtualize page.
const PAGE_HAZARD_BAND_H: f32 = 54.0;

/// Stripe width for that band. Fixed, NOT derived from the band's height:
/// the confirm dialog's tape is only a stripe or two tall so height works
/// there, but at this size it yields 70px slabs that read as blocks rather
/// than tape, and the diagonal edges then cross the fade so slowly that the
/// gradient looks stepped.
const PAGE_HAZARD_STRIPE_W: f32 = 22.0;

/// Hazard tape bled across the top of the page and dissolved into the panel
/// behind it. Allocates no layout space and is painted before the page's
/// content, so everything the page draws lands on top of it.
///
/// Muted hard on purpose: the confirm dialog's tape is a stop sign for one
/// decision, while this one qualifies a whole page you're meant to keep
/// working in. It has to register as "this is experimental" in peripheral
/// vision and then get out of the way — full-strength tape behind body text
/// would be unreadable and would cry wolf.
fn paint_page_hazard_band(ui: &egui::Ui) {
    // `clip_rect`, not `max_rect`: the latter is inset by the panel frame's
    // margin, which leaves the band stopping short of both edges instead of
    // bleeding off them. Intersected with the screen so an unbounded clip
    // rect can't turn into an absurd mesh.
    let area = ui.clip_rect().intersect(ui.ctx().screen_rect());
    if !area.is_positive() {
        return;
    }
    let rect = egui::Rect::from_min_size(
        area.left_top(),
        egui::vec2(area.width(), PAGE_HAZARD_BAND_H.min(area.height())),
    );
    let painter = ui.painter().with_clip_rect(rect);
    let yellow = egui::Color32::from_rgb(0xF6, 0xC4, 0x00);
    let black = egui::Color32::from_rgb(0x16, 0x16, 0x16);
    painter.rect_filled(rect, 0.0, black);
    stripes::paint(&painter, rect, yellow, PAGE_HAZARD_STRIPE_W, 0.0);
    // Wash it back most of the way at the top, then all the way by the
    // bottom edge, so the band has no hard border to give itself away.
    //
    // Lighter wash in light mode: the tape is dark-on-yellow, so a pale
    // panel colour laid over it flattens the contrast far faster than a
    // dark one does, and the same alpha that reads as "muted" on dark
    // reads as "invisible" on light.
    let wash = if ui.visuals().dark_mode { 205 } else { 165 };
    stripes::fade_into(&painter, rect, ui.visuals().panel_fill, wash);
}

impl PhoenixApp {
    /// Record a backup as recently used, so every page's dropdown offers it.
    ///
    /// Browsing is not the only way a backup gets chosen — Resume fills the
    /// path in from a saved session, and the job history only learns about a
    /// file once a *job* completes, which a VM boot or a mount never
    /// produces. Actually using a backup is the strongest signal that it
    /// belongs in the list, so every entry point records here.
    fn remember_backup_in_use(&mut self, path: &str) {
        let before = self.settings.browsed_backups.clone();
        remember_browsed_backup(&mut self.settings.browsed_backups, path);
        if self.settings.browsed_backups != before {
            let _ = self.settings.save();
        }
    }

    fn ui_backup(&mut self, ui: &mut egui::Ui, busy: bool) {
        // Whole-page scroll: when the window is too short, the entire backup
        // page (header, fields, drive list, options) scrolls as a unit above
        // the fixed action bar. The drive list only scrolls horizontally —
        // vertical overflow is the shell's job.
        page_scroll_shell(ui, "backup_page", |ui| {
            page_header(ui, &self.palette, "Create Backup", "");
            let name_missing = self.backup_name.trim().is_empty();
            ui.add_enabled_ui(!busy, |ui| self.ui_backup_form(ui, name_missing));
        });
    }

    /// The greyed-when-busy portion of the Backup page: name field,
    /// target folder row, disk list, and VSS toggle.
    fn ui_backup_form(&mut self, ui: &mut egui::Ui, name_missing: bool) {
        ui.scope(|ui| {
            if name_missing {
                // Dull-red field background until a name is entered —
                // this is the page's only "required" affordance.
                ui.visuals_mut().extreme_bg_color = self.palette.dim(self.palette.danger);
            }
            // Explicit hint styling: the default weak text color all but
            // vanishes against the dull-red "required" fill (the hint is
            // only ever visible while that fill is shown), but it must
            // stay a step dimmer than typed text — italic + subtle_text
            // reads as a placeholder rather than a value.
            ui.add(
                egui::TextEdit::singleline(&mut self.backup_name)
                    .desired_width(f32::INFINITY)
                    .font(fonts::regular(16.0))
                    .hint_text_font(fonts::regular(16.0))
                    .margin(egui::Margin::symmetric(10.0, 8.0))
                    .hint_text(
                        egui::RichText::new("Backup name")
                            .color(self.palette.subtle_text)
                            .italics(),
                    ),
            );
        });

        // Breathing room so the fields' focus rings don't graze each other.
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Target").font(fonts::bold(14.0)));

            // Reserve fixed width for the Browse button on the right so the
            // field can't push it past the page edge. Field and button are
            // both forced into equal-height boxes via `add_sized`, so the
            // row's vertical centering lines them up exactly.
            let spacing = ui.spacing().item_spacing.x;
            let text_w = (ui.available_width() - FORM_BUTTON_W - spacing).max(0.0);

            ui.add_sized(
                [text_w, ACTION_BUTTON_HEIGHT],
                egui::TextEdit::singleline(&mut self.backup_folder)
                    .desired_width(f32::INFINITY)
                    .font(fonts::regular(16.0))
                    .hint_text_font(fonts::regular(16.0))
                    .margin(egui::Margin::symmetric(10.0, 8.0))
                    .hint_text(r"e.g. D:\Backups"),
            );

            let browse_label = icon_label(
                egui_phosphor::regular::FOLDER,
                fonts::icon(16.0),
                "Browse…",
                fonts::regular(14.0),
                ui.visuals().widgets.inactive.fg_stroke.color,
            );
            if ui
                .add_sized(
                    [FORM_BUTTON_W, ACTION_BUTTON_HEIGHT],
                    egui::Button::new(browse_label),
                )
                .clicked()
            {
                if let Some(path) = pick_backup_save_folder(&self.backup_folder) {
                    self.backup_folder = path.display().to_string();
                }
            }
        });
        ui.add_space(16.0);

        if self.disks.is_empty() {
            ui.label("No disks found. Run as Administrator, then hit Refresh.");
            return;
        }

        ui.label(egui::RichText::new("Select Source").font(fonts::bold(16.0)));
        ui.label(
            egui::RichText::new("Choose the partitions you want to back up.")
                .color(self.palette.subtle_text),
        );
        ui.add_space(8.0);

        // Capture the visible viewport width BEFORE entering the inner
        // horizontal ScrollArea — inside it `available_width()` is virtual
        // and would push rows arbitrarily wide. We use this to size each
        // drive row to `max(viewport, natural_min)` so:
        //   * wide window → rows fit, no horizontal scroll;
        //   * narrow window → rows overflow horizontally rather than
        //     squishing partition labels into illegibility.
        //
        // Horizontal-only: the drive list renders at its full natural
        // height and the page-level vertical ScrollArea handles overflow.
        let viewport_width = ui.available_width();

        egui::ScrollArea::horizontal()
            .id_salt("backup_drive_list")
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

        let grouped = self.group_selections();
        if grouped.len() > 1 {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(format!(
                    "{} disks selected — output files will be auto-suffixed with -disk<N>.",
                    grouped.len()
                ))
                .color(self.palette.subtle_text),
            );
        }
    }

    /// How a disk is named in the history: its model plus the number Windows
    /// knows it by, because two identical drives are otherwise indistinguishable
    /// — and a disk number alone means nothing a month later.
    fn disk_label(&self, index: u32) -> String {
        let model = self
            .disks
            .iter()
            .find(|d| d.index == index)
            .and_then(|d| d.model.as_deref())
            .map(str::trim)
            .filter(|m| !m.is_empty());
        match model {
            Some(m) => format!("{m} (Disk {index})"),
            None => format!("Disk {index}"),
        }
    }

    /// The history subject for one queued backup: the disk it reads, the
    /// `.phnx` it writes.
    fn backup_subject(&self, opts: &BackupOptions) -> JobSubject {
        JobSubject {
            source: self.disk_label(opts.disk_index),
            target: opts.output.display().to_string(),
            image_path: Some(opts.output.clone()),
        }
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

        // The folder the user just backed up into becomes the one the Backup
        // page opens on next time. There is no "default backup folder" setting
        // any more — the last folder that actually worked is a better default
        // than one somebody configured once and forgot. Persisted only after
        // the folder has been validated above, so a typo'd path can't become
        // the remembered one.
        let remembered = folder.display().to_string();
        if self.settings.default_backup_dir.as_deref() != Some(remembered.as_str()) {
            self.settings.default_backup_dir = Some(remembered);
            let _ = self.settings.save();
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
        self.job_subject = self.backup_subject(&first);
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
        page_scroll_shell(ui, "restore_page", |ui| {
            page_header(ui, &self.palette, "Restore", "");
            ui.add_enabled_ui(!busy, |ui| {
                self.ui_restore_form(ui);
            });
        });
    }

    /// The `.phnx` chooser row plus the load of whatever it names into
    /// `restore_layout`. Shared by the Restore and Verify pages: both read
    /// the same image, off the same path field, so both get the loaded
    /// layout — the editor on one, the preview on the other. A choice (from
    /// the history dropdown or Browse) loads immediately; there is no typed
    /// input left to debounce.
    fn ui_backup_file_picker(&mut self, ui: &mut egui::Ui, label: &str) {
        let chosen = backup_path_picker(
            ui,
            label,
            "Select or browse for a .phnx backup",
            &mut self.restore_backup_path,
            &self.history,
            &mut self.settings.browsed_backups,
            None,
        );
        if chosen {
            self.restore_backup_load_now = true;
            let _ = self.settings.save();
        }
    }

    /// Greyed-when-busy portion of the Restore page: the `.phnx` Browse row,
    /// then the same source→target layout editor the Clone page runs, with
    /// the backup standing in for a source disk.
    fn ui_restore_form(&mut self, ui: &mut egui::Ui) {
        self.ui_backup_file_picker(ui, "Source media");

        ui.add_space(8.0);
        if self.disks.is_empty() {
            ui.label("No disks found. Run as Administrator, then hit Refresh.");
            return;
        }

        let viewport_width = ui.available_width();
        let out = restore_panel::show(
            ui,
            &self.disks,
            &mut self.target_disk_index,
            self.restore_layout.as_mut(),
            &self.palette,
            viewport_width,
        );
        if out.target_changed {
            ui.ctx().request_repaint();
        }
        if self.restore_layout.is_some() {
            ui.add_space(8.0);
            ui.checkbox(
                &mut self.restore_repair_boot,
                "Repair boot files on the target after restoring",
            )
            .on_hover_text(REPAIR_BOOT_HOVER);
        }
    }

    fn start_restore(&mut self) {
        let Some(backup_path) = resolve_backup_open_path(&self.restore_backup_path) else {
            self.status = "Restore cancelled — no backup file chosen".into();
            return;
        };
        self.restore_backup_path = backup_path.display().to_string();
        self.remember_backup_in_use(&self.restore_backup_path.clone());
        let Some(layout) = self.restore_layout.as_ref() else {
            self.status = "No restore layout — load a backup first".into();
            return;
        };
        let Some(target) = self
            .target_disk_index
            .and_then(|t| self.disks.iter().find(|d| d.index == t))
        else {
            self.status = "Choose a target disk first".into();
            return;
        };
        let plan = layout.to_restore_plan(target);
        // Cross-sector-size pre-flight, computed here so the confirm dialog can
        // describe (and, for a conversion, gate on an acknowledgement of) what
        // will happen. `opt_in = true` because the hazard-dialog checkbox — not
        // a flag — is the opt-in gate here; an unsupported mismatch or a
        // converted-partition shrink still errors and blocks the job.
        let mut convert = false;
        let mut convert_lines: Vec<String> = Vec::new();
        if let Ok(mut reader) = PhnxReader::open(&backup_path) {
            if let Err(e) = plan.validate_against_backup(&reader.manifest) {
                self.status = format!("Plan invalid: {e}");
                return;
            }
            if let Err(e) = plan.validate_extents_fit(&mut reader) {
                self.status = format!("Plan invalid: {e}");
                return;
            }
            match phoenix_restore::restore::plan_sector_conversion(
                &reader.manifest,
                &plan,
                target.sector_size,
                true,
            ) {
                Ok(None) => {}
                Ok(Some(report)) => {
                    convert = true;
                    convert_lines = conversion_detail_lines(&report);
                }
                Err(e) => {
                    self.status = format!("Cannot restore onto this disk: {e}");
                    return;
                }
            }
        }
        // Park the validated restore behind the same final confirm-the-target
        // dialog the Clone page uses, so a wrong-disk pick gets caught before
        // the worker writes anything.
        let subject = JobSubject {
            source: backup_path.display().to_string(),
            target: self.disk_label(target.index),
            image_path: Some(backup_path.clone()),
        };
        let mut details = disk_confirm_details(target);
        details.extend(convert_lines);
        self.pending_restore = Some(PendingRestore {
            subject,
            opts: RestoreOptions {
                backup_path,
                plan,
                // Read-back verification is CLI-only; the image's chunk hashes
                // were already checked while decompressing.
                verify_on_restore: false,
                convert_sector_size: convert,
                repair_boot: self.restore_repair_boot,
                progress: Some(ProgressHandle::new()),
            },
            details,
            convert,
            ack: false,
        });
    }

    fn ui_verify(&mut self, ui: &mut egui::Ui, busy: bool) {
        page_scroll_shell(ui, "verify_page", |ui| {
            page_header(ui, &self.palette, "Verify Backup", "");
            ui.add_enabled_ui(!busy, |ui| {
                self.ui_verify_form(ui);
            });
        });
    }

    /// Greyed-when-busy portion of the Verify page: the `.phnx` Browse row,
    /// then a read-only preview of the layout the chosen image holds — the
    /// same row the Restore page drags partitions off, minus the dragging.
    fn ui_verify_form(&mut self, ui: &mut egui::Ui) {
        self.ui_backup_file_picker(ui, "Backup file");

        let Some(layout) = self.restore_layout.as_ref() else {
            return;
        };
        ui.add_space(12.0);
        ui.label(egui::RichText::new("Backup contents").font(fonts::bold(14.0)));
        ui.add_space(4.0);
        let disk = &layout.source_disk;
        egui::ScrollArea::horizontal()
            .id_salt("verify_preview")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let row_width = ui.available_width().max(disk_dropdown::min_row_width(disk));
                disk_map::draw_static_disk_row(ui, row_width, disk, &self.palette);
            });
    }

    fn start_verify(&mut self) {
        let Some(path) = resolve_backup_open_path(&self.restore_backup_path) else {
            self.status = "Verify cancelled — no backup file chosen".into();
            return;
        };
        self.restore_backup_path = path.display().to_string();
        self.remember_backup_in_use(&self.restore_backup_path.clone());
        self.completed = None;
        self.status = "Verify in progress…".into();
        self.job_subject = JobSubject {
            source: path.display().to_string(),
            target: String::new(),
            image_path: Some(path.clone()),
        };
        self.job = Some(spawn_verify(path, false));
    }

    fn ui_clone(&mut self, ui: &mut egui::Ui) {
        // Same shell as the Backup page: the whole page scrolls vertically,
        // with content padded off the window edge so the scrollbar rides the
        // edge gutter; the drive lists scroll horizontally on their own.
        page_scroll_shell(ui, "clone_page", |ui| self.ui_clone_inner(ui));
    }

    fn ui_clone_inner(&mut self, ui: &mut egui::Ui) {
        page_header(ui, &self.palette, "Clone", "");

        if self.disks.is_empty() {
            ui.label("No disks detected. Run as Administrator, then hit Refresh.");
            return;
        }

        self.sync_clone_layout();

        let viewport_width = ui.available_width();
        let out = clone_panel::show(
            ui,
            &self.disks,
            &mut self.clone_source_index,
            &mut self.clone_target_index,
            self.clone_layout.as_mut(),
            self.clone_expand,
            &self.palette,
            viewport_width,
        );
        if out.selection_changed {
            ui.ctx().request_repaint();
        }

        // Options + warning, driven by the current plan shape (the Clone
        // Disk action itself lives in the sticky bottom action bar).
        if let Some(layout) = &self.clone_layout {
            let full_disk = layout.full_disk;
            if full_disk
                && ui
                    .checkbox(
                        &mut self.clone_expand,
                        "Expand the largest NTFS partition to fill a larger target",
                    )
                    .changed()
            {
                // Re-seed so the target map previews the grown layout.
                self.reseed_full_disk_clone();
            }
            ui.checkbox(
                &mut self.clone_repair_boot,
                "Repair boot files on the target after cloning",
            )
            .on_hover_text(REPAIR_BOOT_HOVER);
            ui.add_space(8.0);
            if let Some(tgt) = self.clone_target_index {
                let warning = if full_disk {
                    format!("⚠ Cloning will PERMANENTLY ERASE all data on disk {tgt}.")
                } else {
                    format!(
                        "⚠ Cloning will PERMANENTLY ERASE the target partitions it covers on \
                         disk {tgt} (and remove any deleted from the layout). Unmapped \
                         partitions are preserved."
                    )
                };
                ui.colored_label(self.palette.warning, warning);
            }
        }
    }

    /// Keep the clone layout in step with the source/target selection: drop
    /// ghost selections after a refresh, and (re)seed a full-disk clone plan
    /// whenever the selected pair changes.
    fn sync_clone_layout(&mut self) {
        if let Some(s) = self.clone_source_index {
            if !self.disks.iter().any(|d| d.index == s) {
                self.clone_source_index = None;
            }
        }
        if let Some(t) = self.clone_target_index {
            if !self.disks.iter().any(|d| d.index == t) || self.clone_source_index == Some(t) {
                self.clone_target_index = None;
            }
        }
        let (Some(s), Some(t)) = (self.clone_source_index, self.clone_target_index) else {
            self.clone_layout = None;
            self.clone_layout_for = None;
            return;
        };
        if self.clone_layout_for != Some((s, t)) || self.clone_layout.is_none() {
            self.clone_layout_for = Some((s, t));
            self.reseed_clone_layout_default();
            // Default the post-clone boot repair per source pick: on when the
            // live source actually carries a Windows installation AND the
            // layout shape can boot (an install with no ESP on GPT would just
            // make the repair fail).
            self.clone_repair_boot = self
                .disks
                .iter()
                .find(|d| d.index == s)
                .map(|src| {
                    source_looks_bootable(src)
                        && !phoenix_restore::bootrepair::detect_windows_installs(
                            std::slice::from_ref(src),
                        )
                        .is_empty()
                })
                .unwrap_or(false);
        }
    }

    fn clone_layout_disks(&self) -> Option<(DiskInfo, DiskInfo)> {
        let source = self
            .clone_source_index
            .and_then(|i| self.disks.iter().find(|d| d.index == i))
            .cloned()?;
        let target = self
            .clone_target_index
            .and_then(|i| self.disks.iter().find(|d| d.index == i))
            .cloned()?;
        Some((source, target))
    }

    /// Build (or rebuild) the clone editor for the selected pair in its
    /// default mode: an individual-partitions plan seeded from the target's
    /// live layout, waiting for a source partition to be dragged down.
    fn reseed_clone_layout_default(&mut self) {
        let Some((source, target)) = self.clone_layout_disks() else {
            self.clone_layout = None;
            return;
        };
        let mut layout = restore_layout::RestoreLayoutState::from_live_disk(&source);
        layout.rebuild_from_target(&target);
        self.clone_layout = Some(layout);
    }

    /// Rebuild the clone editor as a full-disk plan (the "Entire disk" mode's
    /// expand-option toggle re-previews the grown layout through this).
    fn reseed_full_disk_clone(&mut self) {
        let Some((source, target)) = self.clone_layout_disks() else {
            self.clone_layout = None;
            return;
        };
        let mut layout = restore_layout::RestoreLayoutState::from_live_disk(&source);
        layout.seed_full_disk_clone(&source, &target, self.clone_expand);
        self.clone_layout = Some(layout);
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
        let Some(layout) = self.clone_layout.as_ref() else {
            return;
        };
        // The map IS the plan: full-disk mode re-initializes the target with
        // the source's layout; a partial layout updates the existing table
        // in place around the dropped partition(s).
        let plan = layout.to_clone_plan();
        if let Err(e) = plan.validate(&source, &target) {
            self.status = format!("Cannot clone: {e}");
            return;
        }
        // Cross-sector-size pre-flight (see start_restore for the opt_in = true
        // rationale). An unsupported mismatch or a converted-partition shrink
        // blocks the clone here rather than failing mid-write.
        let mut convert = false;
        let mut convert_lines: Vec<String> = Vec::new();
        match phoenix_clone::plan_sector_conversion(&source, &plan, target.sector_size, true) {
            Ok(None) => {}
            Ok(Some(report)) => {
                convert = true;
                convert_lines = conversion_detail_lines(&report);
            }
            Err(e) => {
                self.status = format!("Cannot clone onto this disk: {e}");
                return;
            }
        }
        // Nothing on the page has touched a disk yet — park the validated
        // plan behind a final confirm-the-target dialog before the worker
        // gets to write.
        let mut message = if layout.full_disk {
            format!(
                "This will PERMANENTLY ERASE everything on the target disk and replace it \
                 with the contents of disk {src}. Verify this is the right disk:"
            )
        } else {
            format!(
                "This will PERMANENTLY OVERWRITE the mapped partitions on the target disk \
                 with the contents of disk {src} (unmapped partitions are preserved). Verify \
                 this is the right disk:"
            )
        };
        if convert {
            message.push_str(
                "\n\nThis clone also converts 4Kn → 512e: the filesystem boot sectors are \
                 rewritten, so the result is a CONVERTED COPY, not a byte-identical clone.",
            );
        }
        let mut details = disk_confirm_details(&target);
        details.extend(convert_lines);
        self.pending_clone = Some(PendingClone {
            subject: JobSubject {
                source: self.disk_label(src),
                target: self.disk_label(tgt),
                // A clone writes no file — nothing to size.
                image_path: None,
            },
            opts: phoenix_clone::CloneOptions {
                source_disk_index: src,
                target_disk_index: tgt,
                plan,
                verify: phoenix_clone::CloneVerify::None,
                convert_sector_size: convert,
                repair_boot: self.clone_repair_boot,
                progress: None,
            },
            message,
            details,
            convert,
            ack: false,
        });
    }

    fn ui_mount(&mut self, ui: &mut egui::Ui) {
        // Without WinFsp there is nothing to mount onto — the whole page is a
        // dead end. Replace the picker/table with a centered notice telling the
        // user why and where to get WinFsp, instead of offering controls that
        // could only ever fail.
        if !self.mount_available {
            self.ui_mount_unavailable(ui);
            return;
        }
        // Same shell as every other page: the content scrolls vertically as a
        // unit (the Active-mounts table included) under the sticky
        // "Mount (Read-Only)" action bar.
        page_scroll_shell(ui, "mount_page", |ui| {
            page_header(ui, &self.palette, "Mount", "");

            let path_before = self.mount_backup_path.clone();
            let mut chosen = false;
            ui.add_enabled_ui(self.job.is_none(), |ui| {
                chosen = backup_path_picker(
                    ui,
                    "Backup file",
                    "Select or browse for a .phnx backup",
                    &mut self.mount_backup_path,
                    &self.history,
                    &mut self.settings.browsed_backups,
                    None,
                );
            });
            if chosen {
                let _ = self.settings.save();
            }
            let path_changed = chosen && self.mount_backup_path != path_before;
            if path_changed
                || (self.mount_source.is_none()
                    && self.mount_loaded_path != self.mount_backup_path.trim())
            {
                self.try_load_mount_backup();
            }

            if let Some(source) = self.mount_source.clone() {
                ui.add_space(8.0);
                // The action bar's hover hint says this too, but a taped-over
                // bar with no visible reason is a puzzle — say it on the page.
                let hint = if self.selected_backup_already_mounted() {
                    egui::RichText::new("This backup is already mounted — see Active mounts below.")
                        .color(self.palette.warning)
                } else {
                    egui::RichText::new("Choose the partition(s) to mount.")
                        .color(self.palette.subtle_text)
                };
                ui.label(hint);
                ui.add_space(4.0);
                // Same width discipline as the Backup page: capture the real
                // viewport width before the ScrollArea virtualizes it.
                let viewport_width = ui.available_width();
                let row_width = viewport_width.max(disk_map::min_disk_row_width(&source));
                egui::ScrollArea::horizontal()
                    .id_salt("mount_partition_map")
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
            }

            ui.add_space(18.0);
            ui.label(egui::RichText::new("Active mounts").font(fonts::bold(16.0)));
            ui.add_space(6.0);
            self.ui_active_mounts(ui);
        });
    }

    /// Shown in place of the whole Mount page when WinFsp is unavailable: a
    /// notice centered both axes explaining that mounting needs WinFsp, with a
    /// link to install it. No scroll shell, header, or action bar — there is
    /// nothing to do here until WinFsp is present.
    fn ui_mount_unavailable(&self, ui: &mut egui::Ui) {
        // Estimated height of the notice block, used to place it vertically:
        // a top spacer of half the leftover height centers it in the pane.
        // (egui lays out top-down, so exact centering would need a size pass;
        // the block is fixed content, so a constant estimate reads as centered
        // across window sizes and clamps cleanly when the pane is short.)
        const NOTICE_HEIGHT: f32 = 210.0;
        const NOTICE_WIDTH: f32 = 460.0;

        let leftover = (ui.available_height() - NOTICE_HEIGHT).max(0.0);
        ui.add_space(leftover * 0.5);
        ui.vertical_centered(|ui| {
            // Cap the column so the paragraph wraps to a readable measure and
            // stays centered rather than stretching the full pane width.
            ui.set_max_width(NOTICE_WIDTH);
            ui.label(
                egui::RichText::new(egui_phosphor::regular::HARD_DRIVES)
                    .font(fonts::icon(48.0))
                    .color(self.palette.subtle_text),
            );
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new("WinFsp is required to mount backups")
                    .font(fonts::bold(18.0)),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    "Mounting exposes a backup as a browsable read-only drive using WinFsp, \
                     which isn't installed on this machine (or its driver isn't loaded). \
                     Install WinFsp and restart Simulacra to mount backups.",
                )
                .font(fonts::regular(14.0))
                .color(self.palette.subtle_text),
            );
            ui.add_space(16.0);
            // Icon + label as one clickable, link-colored widget: a single
            // RichText can't mix the phosphor (icon) and Inter Bold (text)
            // families, so hand-build the layout job. Both sections take the
            // hyperlink color so it still reads as a link.
            let mut link = egui::text::LayoutJob::default();
            link.append(
                egui_phosphor::regular::LINK,
                0.0,
                egui::TextFormat {
                    font_id: fonts::icon(16.0),
                    color: self.palette.accent,
                    valign: egui::Align::Center,
                    ..Default::default()
                },
            );
            link.append(
                "  Get WinFsp — winfsp.dev",
                0.0,
                egui::TextFormat {
                    font_id: fonts::bold(14.0),
                    color: self.palette.accent,
                    valign: egui::Align::Center,
                    ..Default::default()
                },
            );
            ui.hyperlink_to(link, "https://winfsp.dev");
        });
    }

    /// The Active-mounts table. Always on the page — empty and greyed out when
    /// nothing is mounted — so the section never appears and disappears under
    /// the user.
    fn ui_active_mounts(&mut self, ui: &mut egui::Ui) {
        let mut rows: Vec<mount_table::MountRow> = self
            .mounts
            .iter()
            .map(|m| mount_table::MountRow {
                folder: m
                    .backup_path()
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                name: m
                    .backup_path()
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "backup".into()),
                size: m.disk_size(),
                letters: m.volumes().iter().filter_map(|v| v.drive_letter).collect(),
                writable: m.is_writable(),
            })
            .collect();
        // The demo rows sit after the real ones, so `Unmount(i)` still indexes
        // `mounts` for every real row.
        let real_rows = rows.len();
        rows.extend(self.demo_mounts.iter().map(mount_table::MountRow::clone));

        // The table fills the pane and shrinks with it; below its minimum the
        // columns stop collapsing and this scroller takes over. Vertical
        // overflow belongs to the page shell, not to a scrollbar of its own.
        let palette = self.palette;
        let viewport_width = ui.available_width();
        let table_width = viewport_width.max(mount_table::min_width());
        let action = egui::ScrollArea::horizontal()
            .id_salt("active_mounts")
            .auto_shrink([false, true])
            .show(ui, |ui| mount_table::show(ui, table_width, &rows, &palette))
            .inner;

        match action {
            // Guarded so a `--demo-mounts` letter (which nothing is attached to)
            // can't send Explorer off to a drive that doesn't exist.
            mount_table::MountAction::Explore(letter) if self.is_mounted_letter(letter) => {
                let _ = std::process::Command::new("explorer.exe")
                    .arg(format!("{letter}:\\"))
                    .spawn();
            }
            mount_table::MountAction::Explore(letter) => {
                self.status = format!("{letter}: is a demo row — nothing is mounted there");
            }
            mount_table::MountAction::Unmount(i) if i < real_rows => {
                // Drop removes the drive letters, detaches the disk, and
                // cleans up the scratch state.
                let m = self.mounts.remove(i);
                self.status = format!("Unmounted {}", m.backup_path().display());
            }
            mount_table::MountAction::Unmount(i) => {
                self.demo_mounts.remove(i - real_rows);
            }
            // A real mount: refuse locked BitLocker up front, otherwise raise the
            // hazard "changes are temporary" dialog before converting it.
            mount_table::MountAction::EnableWrite(i) if i < real_rows => {
                match self.enable_write_precheck(i) {
                    Ok(()) => {
                        self.pending_enable_write = Some(i);
                        self.enable_write_ack = false;
                    }
                    Err(msg) => self.status = msg,
                }
            }
            // A demo row: no precheck, just preview the dialog + writable state.
            mount_table::MountAction::EnableWrite(i) => {
                self.pending_enable_write = Some(i);
                self.enable_write_ack = false;
            }
            mount_table::MountAction::None => {}
        }
    }

    /// Tear down every active mount (each `ActiveMount`'s `Drop` releases its
    /// drive letters and detaches the virtual disk). Called on Unmount-all and
    /// on the way out of the app — a leaked mount would leave a dead drive
    /// letter behind after the process exits.
    fn unmount_all(&mut self) {
        self.mounts.clear();
        self.demo_mounts.clear();
    }

    /// True while any backup is mounted (or standing in for one under
    /// `--demo-mounts`) — the app must not exit in that state without asking.
    fn anything_mounted(&self) -> bool {
        !self.mounts.is_empty() || !self.demo_mounts.is_empty()
    }

    /// True if `letter` is a drive letter one of the live mounts exposes.
    fn is_mounted_letter(&self, letter: char) -> bool {
        self.mounts
            .iter()
            .any(|m| m.volumes().iter().any(|v| v.drive_letter == Some(letter)))
    }

    /// Every mount the close dialog has to warn about — the real ones, plus any
    /// `--demo-mounts` stand-ins so the dialog can be exercised without one.
    fn mount_summaries(&self) -> Vec<String> {
        let real = self.mounts.iter().map(|m| {
            let name = m
                .backup_path()
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "backup".into());
            format!("{name}  —  {}", describe_mounted_volumes(m))
        });
        let demo = self.demo_mounts.iter().map(|r| {
            let letters: Vec<String> = r.letters.iter().map(|l| format!("{l}:")).collect();
            format!("{}  —  {}", r.name, letters.join(", "))
        });
        real.chain(demo).collect()
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

    /// True when the path the Mount page currently has loaded is already
    /// attached. A second mount of the same `.phnx` would hand out a second set
    /// of drive letters for identical read-only content — never what anyone
    /// wants — so the action bar refuses it (and `start_mount` re-checks, since
    /// the bar is not the only way in).
    fn selected_backup_already_mounted(&self) -> bool {
        let chosen = self.mount_backup_path.trim();
        if chosen.is_empty() {
            return false;
        }
        self.mounts
            .iter()
            .any(|m| same_file_path(m.backup_path(), Path::new(chosen)))
    }

    /// The Boot Repair page: pick a detected Windows installation, preview
    /// exactly what the repair would do, and hand off to the action bar's
    /// "Repair Boot" (which parks the job behind a hazard dialog).
    fn ui_bootrepair(&mut self, ui: &mut egui::Ui) {
        use phoenix_restore::bootrepair;

        page_scroll_shell(ui, "bootrepair_page", |ui| {
            page_header(ui, &self.palette, "Boot Repair", "");
            ui.label(
                egui::RichText::new(
                    "Point a drive's boot configuration at a Windows installation on it; for \
                     a clone that won't start, a restored system disk, or a drive moved from \
                     another machine. The boot files are rebuilt with Windows' own built-in \
                     utilities. Only the selected drive is modified.",
                )
                .color(self.palette.subtle_text),
            );
            ui.add_space(14.0);

            // Scan lazily — `reload_disks` clears the cache, so a refresh or a
            // finished job re-detects against the fresh disk list.
            if self.bootrepair_installs.is_none() {
                let installs = bootrepair::detect_windows_installs(&self.disks);
                if let Some(sel) = self.bootrepair_selected {
                    if !installs
                        .iter()
                        .any(|i| (i.disk_index, i.partition_index) == sel)
                    {
                        self.bootrepair_selected = None;
                        self.bootrepair_plan = None;
                    }
                }
                self.bootrepair_installs = Some(installs);
            }
            let installs = self.bootrepair_installs.clone().unwrap_or_default();

            let rows: Vec<bootrepair_table::InstallRow> = installs
                .iter()
                .map(|install| bootrepair_table::InstallRow {
                    version: install
                        .version
                        .clone()
                        .unwrap_or_else(|| "Windows".to_string()),
                    drive: install
                        .drive_letter
                        .map(|l| format!("{l}:"))
                        .unwrap_or_default(),
                    disk: install.disk_index.to_string(),
                    partition: install.partition_index.to_string(),
                    label: install.volume_label.clone().unwrap_or_default(),
                    boot: match self.disks.iter().find(|d| d.index == install.disk_index) {
                        Some(d) if d.is_gpt => "GPT · UEFI".to_string(),
                        Some(_) => "MBR · BIOS".to_string(),
                        None => String::new(),
                    },
                })
                .collect();
            let selected_row = self.bootrepair_selected.and_then(|sel| {
                installs
                    .iter()
                    .position(|i| (i.disk_index, i.partition_index) == sel)
            });
            // Same width discipline as the History table: fill the pane, and
            // below the columns' minimum hand over to a horizontal scroller.
            let viewport_width = ui.available_width();
            let table_width = viewport_width.max(bootrepair_table::min_width());
            let palette = self.palette;
            let clicked = egui::ScrollArea::horizontal()
                .id_salt("bootrepair_table")
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    bootrepair_table::show(ui, table_width, &rows, selected_row, &palette)
                })
                .inner;
            if let Some(i) = clicked {
                self.bootrepair_selected =
                    Some((installs[i].disk_index, installs[i].partition_index));
            }

            // The plan itself is previewed by the confirmation dialog; here we
            // only surface the cases that need saying before the click — an
            // unrepairable pick, or a pick aimed at the live system disk.
            if let Some(sel) = self.bootrepair_selected {
                let stale = self
                    .bootrepair_plan
                    .as_ref()
                    .map(|(key, _)| *key != sel)
                    .unwrap_or(true);
                if stale {
                    let plan = match (
                        self.disks.iter().find(|d| d.index == sel.0),
                        installs
                            .iter()
                            .find(|i| (i.disk_index, i.partition_index) == sel),
                    ) {
                        (Some(disk), Some(install)) => {
                            bootrepair::plan_boot_repair(disk, install).map_err(|e| e.to_string())
                        }
                        _ => Err("the selected disk is no longer present".to_string()),
                    };
                    self.bootrepair_plan = Some((sel, plan));
                }
                match &self.bootrepair_plan.as_ref().expect("computed above").1 {
                    Ok(_) => {
                        if bootrepair::system_disk_index(&self.disks) == Some(sel.0) {
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new(
                                    "This is the disk this PC is currently running from — the \
                                     live system's own boot files will be rewritten.",
                                )
                                .color(self.palette.warning),
                            );
                        }
                    }
                    Err(e) => {
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "This installation can't be repaired: {e}"
                            ))
                            .color(self.palette.warning),
                        );
                    }
                }
            }
        });
    }

    /// Validate the Boot Repair pick and park it behind the confirmation
    /// dialog. Mirrors `current_page_action`'s gating — a click can only
    /// arrive with a selected install whose plan preview is `Ok`.
    fn start_boot_repair(&mut self) {
        use phoenix_restore::bootrepair;

        let Some(sel) = self.bootrepair_selected else {
            return;
        };
        let plan = match self.bootrepair_plan.as_ref() {
            Some((key, Ok(plan))) if *key == sel => plan.clone(),
            _ => return,
        };
        let Some(disk) = self.disks.iter().find(|d| d.index == sel.0) else {
            return;
        };
        let mut details = disk_confirm_details(disk);
        details.push(String::new());
        details.extend(plan.actions.iter().cloned());
        let system_disk = bootrepair::system_disk_index(&self.disks) == Some(sel.0);
        self.pending_boot_repair = Some(PendingBootRepair {
            disk_index: sel.0,
            partition_index: sel.1,
            message: format!(
                "This will rewrite the boot files on {} so it starts {}. Verify this is the \
                 right disk:",
                self.disk_label(sel.0),
                plan.install.display()
            ),
            details,
            subject: JobSubject {
                source: plan.install.display(),
                target: self.disk_label(sel.0),
                image_path: None,
            },
            system_disk,
            ack: false,
        });
    }

    fn start_mount(&mut self) {
        if self.selected_backup_already_mounted() {
            self.status = "That backup is already mounted".into();
            return;
        }
        let path = std::path::PathBuf::from(self.mount_backup_path.trim());
        self.remember_backup_in_use(&self.mount_backup_path.clone());
        let scratch = phoenix_mount::mount_scratch_dir();
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
                // The backup has moved from "being picked" to "mounted", and
                // the table below is now the place it lives: reset the picker
                // so the page is ready for the next one instead of sitting
                // there re-offering what was just mounted.
                self.mount_backup_path.clear();
                self.mount_loaded_path.clear();
                self.mount_source = None;
                self.mount_selection.clear();
            }
            Err(e) => {
                self.status = format!("Mount failed: {e}");
            }
        }
    }

    fn ui_virtualize(&mut self, ui: &mut egui::Ui) {
        // Poll the background VM thread for completion.
        if let Some(run) = &self.vm_run {
            match run.result.try_recv() {
                Ok(Ok(outcome)) => {
                    self.vm_last_status = if outcome.exit_ok {
                        format!(
                            "VM exited cleanly. Session overlay holds {:.1} GB of guest writes; backup {}.",
                            outcome.overlay_bytes as f64 / 1e9,
                            if outcome.backup_intact { "still verifies" } else { "FAILED verification" },
                        )
                    } else {
                        // The launch died — quote QEMU's own words, they name
                        // the actual problem (bad option, missing WHPX, …).
                        let why = outcome
                            .qemu_log_tail
                            .as_deref()
                            .filter(|t| !t.is_empty())
                            .unwrap_or("(no output captured — see the session's qemu.log)");
                        tracing::warn!("QEMU exited non-zero: {why}");
                        format!("QEMU exited with an error: {why}")
                    };
                    self.vm_run = None;
                }
                Ok(Err(e)) => {
                    tracing::warn!("VM boot failed: {e:#}");
                    self.vm_last_status = format!("VM failed: {e:#}");
                    self.vm_run = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.vm_last_status = "VM thread ended unexpectedly.".into();
                    self.vm_run = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // Keep the elapsed timer / poll ticking while a VM runs.
        if self.vm_run.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        }

        // Poll the guest-tools ISO worker (download or update check).
        if let Some(dl) = &mut self.drivers_dl {
            loop {
                match dl.rx.try_recv() {
                    Ok(virtio_iso::DlEvent::Progress { downloaded, total }) => {
                        dl.progress = (downloaded, total);
                    }
                    Ok(virtio_iso::DlEvent::Done) => {
                        // No note on success — the checkbox appearing IS the
                        // signal that the ISO is ready.
                        self.drivers_note.clear();
                        self.drivers_dl = None;
                        break;
                    }
                    Ok(virtio_iso::DlEvent::UpToDate) => {
                        self.drivers_note = "The driver ISO is up to date.".into();
                        self.drivers_dl = None;
                        break;
                    }
                    Ok(virtio_iso::DlEvent::Failed(m)) => {
                        tracing::warn!("drivers ISO: {m}");
                        self.drivers_note = m;
                        self.drivers_dl = None;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.drivers_note = "Download worker ended unexpectedly.".into();
                        self.drivers_dl = None;
                        break;
                    }
                }
            }
        }
        if self.drivers_dl.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }

        // Pull everything the page needs out of `self` into owned locals so the
        // scroll-shell closure doesn't borrow `self` (which would conflict: it
        // needs `&mut self.vm` while a running view borrows `self.vm_run`).
        let qemu_info = self
            .qemu
            .as_ref()
            .map(|q| (q.version.clone(), q.gtk_clipboard));
        let qemu_dir = self.settings.vm_qemu_dir.clone();
        let sessions = phoenix_vm::SessionManager::list_all_sessions();
        let running_owned = self.vm_run.as_ref().map(|r| {
            let overlay_mib = std::fs::metadata(&r.overlay_path)
                .map(|m| m.len() / (1024 * 1024))
                .unwrap_or(0);
            (
                r.name.clone(),
                r.backup.display().to_string(),
                r.started.elapsed().as_secs(),
                overlay_mib,
                r.share_cmd.clone(),
                r.network_connected,
            )
        });
        let last = self.vm_last_status.clone();
        let drivers = vm_panel::DriversView {
            present: virtio_iso::installed().is_some(),
            downloading: self.drivers_dl.as_ref().map(|d| d.progress),
            note: self.drivers_note.clone(),
        };
        let palette = self.palette;
        let mut vm_state = std::mem::take(&mut self.vm);
        // Taken out and put back like `vm_state`: the page needs it mutably
        // while other `self.settings` fields are borrowed alongside it.
        let mut browsed = std::mem::take(&mut self.settings.browsed_backups);
        let browsed_before = browsed.len();
        let mut action = vm_panel::VmAction::None;

        // Painted against the panel rather than inside the scroll shell, so
        // it stays put at the top of the page instead of scrolling away.
        paint_page_hazard_band(ui);

        page_scroll_shell(ui, "virtualize_page", |ui| {
            page_header_badged(ui, &palette, "Virtualize", "", "Experimental");
            vm_wiki_notice(ui, &palette);
            let running =
                running_owned
                    .as_ref()
                    .map(|(n, b, e, o, share, net)| vm_panel::RunningView {
                        name: n,
                        backup_path: b,
                        elapsed_secs: *e,
                        overlay_mib: *o,
                        share_cmd: share.as_deref(),
                        network_connected: *net,
                    });
            action = vm_panel::show(
                ui,
                &mut vm_state,
                &palette,
                qemu_info.as_ref().map(|(v, c)| vm_panel::QemuView {
                    version: v,
                    clipboard: *c,
                    dir: qemu_dir.as_deref(),
                }),
                &self.history,
                &mut browsed,
                &self.settings.vm_iso_history,
                &drivers,
                &sessions,
                running,
                &last,
            );
        });
        self.vm = vm_state;
        let browsed_changed = browsed.len() != browsed_before;
        self.settings.browsed_backups = browsed;
        if browsed_changed {
            let _ = self.settings.save();
        }

        match action {
            vm_panel::VmAction::None => {}
            vm_panel::VmAction::BrowseIso => {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Disc image", &["iso"])
                    .set_title("Choose a rescue / PE ISO")
                    .pick_file()
                {
                    let picked = p.display().to_string();
                    self.vm.iso_path = picked.clone();
                    // Remember it for the dropdown: newest first, deduped,
                    // capped so the popup stays a list rather than a history.
                    let hist = &mut self.settings.vm_iso_history;
                    hist.retain(|h| !h.eq_ignore_ascii_case(&picked));
                    hist.insert(0, picked);
                    hist.truncate(8);
                    let _ = self.settings.save();
                }
            }
            vm_panel::VmAction::SetNetwork(up) => {
                match self.running_vm_qmp_port() {
                    Some(port) => {
                        match phoenix_vm::qmp::set_link(
                            port,
                            phoenix_vm::config::NET_DEVICE_ID,
                            up,
                        ) {
                            Ok(()) => {
                                if let Some(run) = self.vm_run.as_mut() {
                                    run.network_connected = up;
                                }
                                self.vm_last_status = if up {
                                    "Network connected — the guest is online.".into()
                                } else {
                                    "Network disconnected.".into()
                                };
                            }
                            Err(e) => {
                                self.vm_last_status = format!("Couldn't change the network: {e}")
                            }
                        }
                    }
                    None => {
                        self.vm_last_status = "Couldn't find the VM's control port.".into()
                    }
                }
            }
            vm_panel::VmAction::BrowseQemuDir => {
                if let Some(p) = rfd::FileDialog::new()
                    .set_title("Choose the folder containing qemu-system-x86_64.exe")
                    .pick_folder()
                {
                    // Re-probe immediately: a folder without the binaries (or
                    // without the OVMF firmware) must not silently become the
                    // saved choice, so only a successful discover sticks.
                    match phoenix_vm::Qemu::discover(Some(&p)) {
                        Ok(q) => {
                            self.vm_last_status = format!("Using {}", q.version);
                            self.qemu = Some(q);
                            self.settings.vm_qemu_dir = Some(p.display().to_string());
                            let _ = self.settings.save();
                        }
                        Err(e) => {
                            self.vm_last_status =
                                format!("That folder isn't a usable QEMU install: {e}");
                        }
                    }
                }
            }
            vm_panel::VmAction::ClearQemuDir => {
                self.settings.vm_qemu_dir = None;
                let _ = self.settings.save();
                self.qemu = phoenix_vm::Qemu::discover(None).ok();
                self.vm_last_status = match self.qemu.as_ref() {
                    Some(q) => format!("Autodetected {}", q.version),
                    None => "QEMU not found".to_string(),
                };
            }
            vm_panel::VmAction::DownloadDrivers => {
                if self.drivers_dl.is_none() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    virtio_iso::spawn_download(tx);
                    self.drivers_note.clear();
                    self.drivers_dl = Some(DriversDl { rx, progress: (0, 0) });
                }
            }
            vm_panel::VmAction::CheckDriversUpdate => {
                if self.drivers_dl.is_none() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    virtio_iso::spawn_update_check(tx);
                    self.drivers_note = "Checking for a newer driver ISO…".into();
                    self.drivers_dl = Some(DriversDl { rx, progress: (0, 0) });
                }
            }
            vm_panel::VmAction::LoadSummary => self.load_vm_summary(),
            vm_panel::VmAction::Stop => self.stop_vm(),
            vm_panel::VmAction::Resume { backup_path, vm_root } => {
                self.vm.backup_path = backup_path;
                self.load_vm_summary();
                // Resume where the session lives — never where the scratch
                // dropdown currently points.
                self.start_vm(false, Some(vm_root));
            }
            vm_panel::VmAction::Discard { backup_path, vm_root } => {
                match phoenix_core::container::PhnxReader::open(std::path::Path::new(&backup_path))
                {
                    Ok(reader) => {
                        let id = reader.header.backup_id;
                        drop(reader);
                        // Discard at the session's own root, same reasoning.
                        match phoenix_vm::SessionManager::new(vm_root.join("vm-sessions"))
                            .discard(&id)
                        {
                            Ok(()) => self.vm_last_status = "Session discarded.".into(),
                            Err(e) => self.vm_last_status = format!("Discard failed: {e}"),
                        }
                    }
                    Err(e) => self.vm_last_status = format!("Can't open backup: {e}"),
                }
            }
        }

        // Persist the VM knobs the moment they change — a fresh launch
        // otherwise resets them and surprises the next boot.
        let scratch_now = match self.vm.scratch {
            vm_panel::ScratchChoice::SameAsImage => None,
            vm_panel::ScratchChoice::Drive(c) => Some(c),
        };
        if scratch_now != self.settings.vm_scratch_drive
            || self.settings.vm_memory_mib != Some(self.vm.mem_mib)
            || self.settings.vm_cpus != Some(self.vm.cpus)
        {
            self.settings.vm_scratch_drive = scratch_now;
            self.settings.vm_memory_mib = Some(self.vm.mem_mib);
            self.settings.vm_cpus = Some(self.vm.cpus);
            let _ = self.settings.save();
        }
    }

    /// Parse the picked backup's manifest — the VM's display name plus any
    /// boot blocker (a locked-BitLocker OS volume is refused up front). A
    /// clean parse is also what arms the Boot VM action.
    fn load_vm_summary(&mut self) {
        let path = self.vm.backup_path.trim().to_string();
        self.vm.loaded_path = path.clone();
        self.vm.summary = None;
        if path.is_empty() {
            return;
        }
        match phoenix_core::container::PhnxReader::open(std::path::Path::new(&path)) {
            Ok(reader) => {
                let m = &reader.manifest;
                let blocker = m
                    .partitions
                    .iter()
                    .find(|p| p.bitlocker.as_deref() == Some("locked"))
                    .map(|p| format!("partition {} is a locked BitLocker volume — can't boot", p.index));
                self.vm.summary = Some(vm_panel::BackupSummary {
                    hostname: m.hostname.clone(),
                    blocker,
                });
            }
            Err(e) => self.vm_last_status = format!("Can't read backup: {e}"),
        }
    }

    /// Compute the VM working root from the scratch-drive choice.
    fn vm_root(&self, backup: &std::path::Path) -> std::path::PathBuf {
        match self.vm.scratch {
            vm_panel::ScratchChoice::SameAsImage => phoenix_vm::vm_root_for_backup(backup),
            vm_panel::ScratchChoice::Drive(c) => {
                std::path::PathBuf::from(format!("{c}:\\")).join("PhoenixSimulacra")
            }
        }
    }

    /// Boot VM clicked: when saved sessions exist, raise the resume-or-new
    /// hazard dialog first so a user doesn't overlook a session they meant to
    /// continue (or blow one away by re-picking its backup). No sessions →
    /// boot straight away.
    fn request_start_vm(&mut self) {
        if self.vm_run.is_some() || self.vm_boot_prompt.is_some() {
            return;
        }
        let sessions: Vec<(String, std::path::PathBuf)> =
            phoenix_vm::SessionManager::list_all_sessions()
                .into_iter()
                .map(|s| (s.meta().backup_path.clone(), s.vm_root()))
                .collect();
        if sessions.is_empty() {
            self.start_vm(false, None);
        } else {
            self.vm_boot_prompt = Some(sessions);
        }
    }

    /// The resume-or-new dialog raised by [`Self::request_start_vm`].
    fn show_vm_boot_dialog(&mut self, ctx: &egui::Context) {
        let Some(session_paths) = self.vm_boot_prompt.clone() else {
            return;
        };
        // Starting a new session for a backup that already has one discards
        // that session's saved guest changes — hence the hazard framing.
        let selected_has_session = session_paths
            .iter()
            .any(|(p, _)| p.eq_ignore_ascii_case(self.vm.backup_path.trim()));
        let single = (session_paths.len() == 1).then(|| session_paths[0].clone());

        let action = {
            let (message, confirm_label, extra_label) = match &single {
                Some((path, _root)) => {
                    let name = std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone());
                    (
                        format!(
                            "You have a saved session for {name}. Resume it, or start a \
                             new session from the selected backup?{}",
                            if selected_has_session {
                                "\n\nStarting new discards that session's saved changes."
                            } else {
                                ""
                            }
                        ),
                        "Resume",
                        Some("Start new"),
                    )
                }
                None => (
                    format!(
                        "You have {} saved sessions. To continue one of them, choose \
                         Resume next to it under Saved sessions. Start a new session \
                         from the selected backup instead?{}",
                        session_paths.len(),
                        if selected_has_session {
                            "\n\nThe selected backup already has a saved session — \
                             starting new discards its saved changes."
                        } else {
                            ""
                        }
                    ),
                    "Start new",
                    None,
                ),
            };
            let mut view = ConfirmView {
                title: "Saved session found",
                message: &message,
                details: &[],
                confirm_label,
                cancel_label: "Cancel",
                // "Start new" over an existing session is destructive; a plain
                // Resume is not.
                confirm_danger: single.is_none() && selected_has_session,
                hazard_tape: true,
                ack: None,
                extra_label,
            };
            confirm_dialog::show(ctx, &self.palette, &mut view)
        };

        match (action, &single) {
            // Single session: Confirm = resume it (whatever backup is picked),
            // at the root it actually lives under.
            (ConfirmAction::Confirm, Some((path, root))) => {
                self.vm_boot_prompt = None;
                self.vm.backup_path = path.clone();
                self.load_vm_summary();
                self.start_vm(false, Some(root.clone()));
            }
            // Single session: Extra = start new from the selected backup
            // (the scratch choice governs where NEW sessions go).
            (ConfirmAction::Extra, Some(_)) => {
                self.vm_boot_prompt = None;
                self.start_vm(selected_has_session, None);
            }
            // Multiple sessions: Confirm = start new from the selected backup.
            (ConfirmAction::Confirm, None) => {
                self.vm_boot_prompt = None;
                self.start_vm(selected_has_session, None);
            }
            (ConfirmAction::Cancel, _) => self.vm_boot_prompt = None,
            _ => {}
        }
    }

    /// `vm_root_override` is the session's own root when resuming; `None`
    /// derives the root from the scratch-drive choice (new sessions).
    fn start_vm(&mut self, fresh: bool, vm_root_override: Option<std::path::PathBuf>) {
        if self.vm_run.is_some() {
            return;
        }
        let backup = std::path::PathBuf::from(self.vm.backup_path.trim());
        self.remember_backup_in_use(&self.vm.backup_path.clone());
        if !backup.exists() {
            self.vm_last_status = "Pick a backup file first.".into();
            return;
        }
        let Some(qemu) = self.qemu.clone() else {
            self.vm_last_status = "QEMU isn't installed.".into();
            return;
        };
        // Session identity (for the overlay path we poll for progress).
        let id = match phoenix_core::container::PhnxReader::open(&backup) {
            Ok(r) => {
                let id = r.header.backup_id;
                drop(r);
                id
            }
            Err(e) => {
                self.vm_last_status = format!("Can't open backup: {e}");
                return;
            }
        };
        let name = self.vm.summary.as_ref().map(|s| s.hostname.clone()).unwrap_or_else(|| {
            backup.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
        });

        let vm_root = vm_root_override.unwrap_or_else(|| self.vm_root(&backup));
        let sessions = phoenix_vm::SessionManager::new(vm_root.join("vm-sessions"));
        let scratch = vm_root.join("vm-serve");
        let overlay_path = vm_root
            .join("vm-sessions")
            .join(id.simple().to_string())
            .join("session.qcow2");
        phoenix_vm::sweep_serve_scratch(&vm_root);

        let host = phoenix_vm::HostOptions {
            memory_mib: self.vm.mem_mib,
            smp: self.vm.cpus,
            // Always boot unplugged; the running view grants it on demand.
            network: false,
            // Cap the guest display so the QEMU window (chrome included)
            // fits the monitor — PE otherwise picks a huge video mode.
            max_resolution: phoenix_vm::usable_guest_resolution(),
            // Clipboard sharing only where the QEMU we found supports it
            // (11.1+); asking an older build for it fails the launch.
            clipboard_agent: self.qemu.as_ref().is_some_and(|q| q.gtk_clipboard),
            // Dress the QEMU window to match the app's theme.
            dark_window: !theme::is_light(self.settings.theme),
            ..phoenix_vm::HostOptions::default()
        };
        let iso = {
            let t = self.vm.iso_path.trim();
            if t.is_empty() { None } else { Some(std::path::PathBuf::from(t)) }
        };
        let boot_iso = self.vm.boot_iso;
        // The guest-tools / driver ISO rides along when present and the
        // checkbox (default on) says so.
        let drivers_iso = if self.vm.attach_drivers_on() {
            virtio_iso::installed()
        } else {
            None
        };

        // Shared folder: create/point the Windows share before boot so the
        // Running view can show the guest-side command immediately. A failure
        // is surfaced but never blocks the boot.
        // Always set up: there is no longer a checkbox for it. The share
        // costs nothing while the guest's cable is unplugged, and having it
        // ready means connecting the network is the only step between a
        // running guest and the shared folder.
        let mut share_warning = None;
        let share_cmd = {
            let dir = phoenix_vm::share::share_dir_for_backup(&backup);
            match phoenix_vm::share::ensure_share(&dir) {
                Ok(()) => Some(phoenix_vm::share::guest_mount_command()),
                Err(e) => {
                    tracing::warn!("shared folder setup failed: {e:#}");
                    share_warning = Some(format!("Shared folder unavailable: {e}"));
                    None
                }
            }
        };
        let share_active = share_cmd.is_some();
        // The "VMSCRIPTS" helper disk carries MapShare.cmd into the guest so
        // mapping the share is a double-click, not transcription — and
        // InstallGuestDrivers.cmd, which matters MOST with networking off,
        // since that is exactly when the guest can't fetch anything itself.
        // So it is never gated on the share: only MapShare.cmd needs the NAT.
        let helper_disk = {
            let p = vm_root.join("share-helper.img");
            match phoenix_vm::share::build_helper_disk(&p) {
                Ok(()) => Some(p),
                Err(e) => {
                    tracing::warn!("helper disk build failed: {e:#}");
                    None
                }
            }
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let backup_thread = backup.clone();
        std::thread::spawn(move || {
            #[cfg(feature = "winfsp")]
            let res = phoenix_vm::boot::boot(
                &backup_thread,
                &host,
                phoenix_vm::WriteLayer::Qcow2,
                fresh,
                iso.as_deref(),
                boot_iso,
                drivers_iso.as_deref(),
                helper_disk.as_deref(),
                &qemu,
                &sessions,
                &scratch,
            );
            #[cfg(not(feature = "winfsp"))]
            let res: anyhow::Result<phoenix_vm::boot::BootOutcome> = {
                let _ = (
                    &backup_thread,
                    &host,
                    fresh,
                    &iso,
                    boot_iso,
                    &drivers_iso,
                    &helper_disk,
                    &qemu,
                    &sessions,
                    &scratch,
                );
                Err(anyhow::anyhow!("this build lacks the winfsp feature required to boot a VM"))
            };
            // The share exists for the guest; the guest is gone. (The folder
            // and its contents stay.)
            if share_active {
                phoenix_vm::share::remove_share();
            }
            let _ = tx.send(res);
        });

        self.vm_last_status = share_warning.unwrap_or_default();
        self.vm_run = Some(VmRun {
            backup,
            name,
            started: Instant::now(),
            overlay_path,
            share_cmd,
            vm_root,
            // Boot unplugs the cable as soon as QMP answers.
            network_connected: false,
            result: rx,
        });
    }

    fn stop_vm(&mut self) {
        let Some(run) = &self.vm_run else { return };
        let backup = run.backup.clone();
        let vm_root = run.vm_root.clone();
        let id = match phoenix_core::container::PhnxReader::open(&backup) {
            Ok(r) => {
                let id = r.header.backup_id;
                drop(r);
                id
            }
            Err(e) => {
                self.vm_last_status = format!("Can't open backup: {e}");
                return;
            }
        };
        // The QMP port file lives in the session dir under the root the boot
        // ACTUALLY used (captured in VmRun — the dropdown may have moved on).
        let port_file = vm_root
            .join("vm-sessions")
            .join(id.simple().to_string())
            .join("qmp.port");
        let port = std::fs::read_to_string(&port_file)
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok());
        match port {
            Some(p) => match phoenix_vm::qmp::system_powerdown(p) {
                Ok(()) => {
                    self.vm_last_status = "Sent graceful shutdown; the guest is powering off…".into()
                }
                Err(e) => self.vm_last_status = format!("Graceful stop failed: {e}"),
            },
            None => self.vm_last_status = "Couldn't find the VM's control port.".into(),
        }
    }

    /// The running VM's QMP port, if there is one and we can find it.
    ///
    /// Same reasoning as `stop_vm`: the port file lives under the root the
    /// boot ACTUALLY used, which `VmRun` captured — the scratch dropdown may
    /// have moved on since.
    fn running_vm_qmp_port(&self) -> Option<u16> {
        let run = self.vm_run.as_ref()?;
        let id = phoenix_core::container::PhnxReader::open(&run.backup)
            .ok()
            .map(|r| r.header.backup_id)?;
        std::fs::read_to_string(
            run.vm_root
                .join("vm-sessions")
                .join(id.simple().to_string())
                .join("qmp.port"),
        )
        .ok()?
        .trim()
        .parse()
        .ok()
    }

    /// Cut the running VM's power. Used on close, where the app is going away
    /// and taking the VM's disk with it.
    fn power_off_vm_now(&mut self) {
        if let Some(port) = self.running_vm_qmp_port() {
            let _ = phoenix_vm::qmp::quit(port);
        }
        self.vm_run = None;
    }

    /// A close must be held for a warning: mounts can't outlive the process,
    /// and neither can a VM — the app serves the disk the guest is running
    /// from.
    fn close_needs_confirm(&self) -> bool {
        self.anything_mounted() || self.vm_run.is_some()
    }

    fn ui_history(&mut self, ui: &mut egui::Ui) {
        // Same shell as every other page: content fills the pane and scrolls
        // vertically as a unit. (The old list didn't, which is why the page
        // read as half-width.)
        page_scroll_shell(ui, "history_page", |ui| {
            page_header(ui, &self.palette, "History", "");

            let rows = self.history_rows();
            let palette = self.palette;
            // The table fills the pane and shrinks with it; below its minimum
            // the columns stop collapsing and this scroller takes over.
            let viewport_width = ui.available_width();
            let table_width = viewport_width.max(history_table::min_width());
            let action = egui::ScrollArea::horizontal()
                .id_salt("history_table")
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    history_table::show(ui, table_width, &rows, &palette)
                })
                .inner;

            if let history_table::HistoryAction::Remove(row) = action {
                // Rows run newest-first; the log runs oldest-first.
                self.pending_history_delete = Some(self.history.records.len() - 1 - row);
            }
        });
    }

    /// Flatten the log into table rows, newest first.
    fn history_rows(&self) -> Vec<history_table::HistoryRow> {
        use history_table::{HistoryRow, RowStatus};
        use phoenix_core::appdata::JobOutcome;

        self.history
            .records
            .iter()
            .rev()
            .map(|rec| {
                let (status, error) = match &rec.outcome {
                    JobOutcome::Success => (RowStatus::Ok, None),
                    JobOutcome::Cancelled => (RowStatus::Cancelled, None),
                    JobOutcome::Failed(msg) => (RowStatus::Failed, Some(msg.clone())),
                };
                HistoryRow {
                    status,
                    action: job_action_label(rec.kind).to_string(),
                    time: format_timestamp(rec.started_unix),
                    details: describe_job(rec),
                    error,
                }
            })
            .collect()
    }

    /// The ✕ on a history row. Confirming a removal that sits next to a
    /// backup's file name deserves one sentence saying what is NOT about to
    /// happen — the file stays exactly where it is.
    fn show_history_delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(index) = self.pending_history_delete else {
            return;
        };
        let Some(rec) = self.history.records.get(index) else {
            self.pending_history_delete = None;
            return;
        };
        let details = vec![
            format!(
                "{}  —  {}",
                job_action_label(rec.kind),
                format_timestamp(rec.started_unix)
            ),
            describe_job(rec),
        ];
        let mut view = ConfirmView {
            title: "Remove from history",
            message: "Remove this entry from the job history? This forgets the entry only — \
                      the backup file itself is not touched.",
            details: &details,
            confirm_label: "Remove",
            cancel_label: "Cancel",
            confirm_danger: true,
            // Taped: the entry is gone for good once removed, and it is the
            // only record that the backup it names was ever made.
            hazard_tape: true,
            ack: None,
            extra_label: None,
        };
        match confirm_dialog::show(ctx, &self.palette, &mut view) {
            ConfirmAction::Confirm => {
                self.pending_history_delete = None;
                if self.history.remove(index) && !self.demo_history {
                    let _ = self.history.save();
                }
            }
            ConfirmAction::Cancel => self.pending_history_delete = None,
            ConfirmAction::None | ConfirmAction::Extra => {}
        }
    }

    fn ui_about(&mut self, ui: &mut egui::Ui) {
        page_scroll_shell(ui, "about_page", |ui| {
            if about_panel::show(ui, &self.palette, version::BUILD_INFO.version) {
                self.start_update_check();
            }
        });
    }

    /// On close, run any verified staged installer silently and detached, then
    /// forget it. Only a fully verified file is ever recorded, and startup
    /// `cleanup_stale` already dropped any already-applied one — so a recorded
    /// path here is a genuine pending upgrade. Best-effort: if the launch fails
    /// there's nothing to surface at exit, and the file stays for next time.
    fn apply_staged_update_on_exit(&mut self) {
        // A portable build stages nothing, so there is normally nothing here —
        // but an installed copy on the same machine shares this state file, and
        // running *its* pending installer off the back of a portable close is
        // exactly the surprise the portable build exists to avoid.
        if self.portable {
            return;
        }
        let Some(path) = self.update_state.staged_installer.clone() else {
            return;
        };
        let path = PathBuf::from(path);
        if !path.exists() {
            self.update_state.clear_staged();
            let _ = self.update_state.save();
            return;
        }
        let launched = if self.restart_after_update {
            updater::launch_installer_and_restart(&path, &relaunch_target()).is_ok()
        } else {
            updater::launch_installer(&path).is_ok()
        };
        if launched {
            self.update_state.clear_staged();
            let _ = self.update_state.save();
        }
    }

    /// Kick off a manual "Check for updates" run (ignored if one is already in
    /// flight, so repeated clicks don't stack worker threads). A portable build
    /// only ever reports what it finds — see [`updater::is_portable`].
    fn start_update_check(&mut self) {
        if self.update_check.is_some() {
            return;
        }
        self.status = "Checking for updates…".into();
        self.update_check = Some(updater::UpdateCheck::spawn(
            updater::CheckMode::Manual,
            version::BUILD_INFO.version.to_string(),
            if self.portable {
                updater::Depth::Report
            } else {
                updater::Depth::Stage
            },
        ));
    }
}

/// `--debug`: re-enable the console that this GUI-subsystem build suppresses by
/// default, and stream logs to it. This is the flag the eventual "Simulacra
/// (Debug)" Start-menu entry passes through the launcher.
fn debug_from_args() -> bool {
    std::env::args().skip(1).any(|a| a == "--debug")
}

/// Give the process a console when launched with `--debug`. Prefer the invoking
/// terminal's console (a developer running the exe from a shell); fall back to a
/// fresh console window (double-clicked, or launched elevated via the launcher,
/// where there is no parent console to attach to).
#[cfg(windows)]
fn attach_debug_console() {
    use windows_sys::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};

    // SAFETY: both are argument-free FFI calls. AttachConsole returns 0 (false)
    // when there is no parent console, in which case we allocate our own.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            AllocConsole();
        }
    }
}

/// Debug/verification aid: `--demo-confirm` opens the app with a fake
/// restore-confirmation dialog up, for eyeballing the dialog styling without
/// staging a real destructive operation.
fn demo_confirm_from_args() -> bool {
    std::env::args().skip(1).any(|a| a == "--demo-confirm")
}

/// Debug/verification aid: `--demo-progress` opens the app with the status
/// modal up and its progress bar sweeping on a loop, for eyeballing the barber
/// pole and the byte readout without running a real backup.
fn demo_progress_from_args() -> bool {
    std::env::args().skip(1).any(|a| a == "--demo-progress")
}

/// Debug/verification aid: `--demo-complete [success|cancelled|failed]` (default
/// `success`) opens the app on a finished restore's modal, so all three verdict
/// badges can be eyeballed without staging a real destructive operation. Parks a
/// canned [`CompletedJob`], which is exactly what a real finished job leaves
/// behind — so this exercises the shipping render path, not a copy of it.
fn demo_completed_job_from_args() -> Option<CompletedJob> {
    let mut args = std::env::args()
        .skip(1)
        .skip_while(|a| a != "--demo-complete");
    args.next()?;
    let outcome = match args.next().as_deref() {
        Some("cancelled") => JobOutcome::Warning,
        Some("failed") => JobOutcome::Failure,
        _ => JobOutcome::Success,
    };
    let (banner, message) = match outcome {
        JobOutcome::Success => (
            "Restore complete.",
            "Restore completed to disk 2".to_string(),
        ),
        JobOutcome::Warning => (
            "Restore cancelled.",
            "Restore cancelled — the target disk is now in an incomplete state".to_string(),
        ),
        JobOutcome::Failure => (
            "Restore failed.",
            "Error: write to \\\\.\\PhysicalDrive2 failed at offset 4,294,967,296 \
             (The device is not ready.)"
                .to_string(),
        ),
    };
    Some(CompletedJob {
        title: "Restore".to_string(),
        steps: vec![
            "Writing partition table".to_string(),
            "Restoring partition 1 (EFI System, 260 MB)".to_string(),
            "Restoring partition 2 (Basic data, 931.2 GB)".to_string(),
        ],
        current_step: 2,
        outcome,
        message,
        banner: banner.to_string(),
        verify_target: None,
    })
}

/// Debug/verification aid: `--demo-refresh` opens the app with the
/// click-to-refresh dim/spinner overlay held up for a few seconds, for
/// eyeballing the overlay styling (a real refresh drops it after a second,
/// too quick to screenshot deliberately).
fn demo_refresh_overlay_from_args() -> Option<Instant> {
    std::env::args()
        .skip(1)
        .any(|a| a == "--demo-refresh")
        .then(|| Instant::now() + Duration::from_secs(5))
}

/// Debug/verification aid: `--demo-mounts` seeds the Active-mounts table with
/// canned rows — a two-letter mount and a single-letter one — so the table's
/// multi-letter layout, its buttons, and the close-with-mounts dialog can all be
/// eyeballed without an elevated session and a real `.phnx` to attach. The rows
/// are inert: Explore reports instead of opening Explorer.
/// `--demo-enable-write`: open the write-access hazard dialog at startup on a
/// demo mount, a styling aid for the dialog (pairs with `--demo-mounts`).
fn demo_enable_write_from_args() -> bool {
    std::env::args().skip(1).any(|a| a == "--demo-enable-write")
}

fn demo_mount_rows_from_args() -> Vec<mount_table::MountRow> {
    if !std::env::args().skip(1).any(|a| a == "--demo-mounts") {
        return Vec::new();
    }
    vec![
        mount_table::MountRow {
            folder: r"D:\Backups".into(),
            name: "fdBU.phnx".into(),
            size: 30_800_000_000,
            letters: vec!['I', 'J'],
            writable: false,
        },
        mount_table::MountRow {
            folder: r"\\nas\archive\simulacra\weekly\workstation\2026\july".into(),
            name: "workstation-2026-07-12.phnx".into(),
            size: 512_110_190_592,
            letters: vec!['K'],
            writable: true,
        },
    ]
}

/// Debug/verification aid: `--demo-history` fills the History page with canned
/// entries — a verified backup's two rows, a cancel, a failure with a long
/// error, a clone — so the table's columns, striping, elision and remove flow
/// can all be eyeballed without running (and failing) real jobs first. The rows
/// stand in for the real log and are never written back over it.
fn demo_history_from_args() -> Option<phoenix_core::appdata::History> {
    if !std::env::args().skip(1).any(|a| a == "--demo-history") {
        return None;
    }
    use phoenix_core::appdata::{now_unix, History, JobKindTag as K, JobOutcome as O, JobRecord};
    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;
    let now = now_unix();

    // Oldest first — the page renders them newest first.
    let records = vec![
        JobRecord::new(K::Clone, O::Success, now - 6 * DAY, 4_512)
            .with_source("Samsung SSD 990 PRO 2TB (Disk 1)")
            .with_target("WDC WD10EZEX-08WN4A0 (Disk 2)")
            .with_bytes(931_500_000_000, 0),
        JobRecord::new(
            K::Restore,
            O::Failed("the device is not ready (os error 21)".into()),
            now - 3 * DAY,
            18,
        )
        .with_source(r"\\nas\archive\simulacra\weekly\workstation-2026-07-06.phnx")
        .with_target("SanDisk Extreme USB (Disk 3)")
        .with_bytes(0, 512_110_190_592),
        JobRecord::new(K::Backup, O::Cancelled, now - DAY, 340)
            .with_source("SanDisk Extreme USB (Disk 3)")
            .with_target(r"D:\Backups\usb-stick.phnx")
            .with_bytes(12_000_000_000, 0),
        JobRecord::new(K::Backup, O::Success, now - 2 * HOUR, 1_284)
            .with_source("Samsung SSD 990 PRO 2TB (Disk 1)")
            .with_target(r"D:\Backups\workstation.phnx")
            .with_bytes(65_700_000_000, 61_240_000_000),
        JobRecord::new(K::Verify, O::Success, now - 2 * HOUR + 1_284, 402)
            .with_source(r"D:\Backups\workstation.phnx")
            .with_bytes(65_700_000_000, 61_240_000_000),
    ];
    Some(History {
        records,
        ..History::default()
    })
}

/// Debug/verification aid: `--demo-armed` opens the Backup page with a name
/// already typed in, which is all it takes to arm the sticky action bar — so
/// its enabled styling (the scrambling green mosaic) can be eyeballed without
/// synthesizing keystrokes into an elevated window, which UIPI forbids.
fn demo_backup_name_from_args() -> String {
    match std::env::args().skip(1).any(|a| a == "--demo-armed") {
        true => "demo-backup".into(),
        false => String::new(),
    }
}

/// What "Restart to update" comes back as. The installed bundle is dual-arch,
/// so the launcher — not this exe — is the entry point that picks the right
/// build for the machine; falling back to our own path covers a dev or portable
/// run, where there may be no launcher beside us.
fn relaunch_target() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("simulacra.exe"));
    let launcher = exe.with_file_name("simulacra-launcher.exe");
    if launcher.is_file() {
        launcher
    } else {
        exe
    }
}

/// Debug/verification aid: `--demo-no-winfsp` forces the Mount page's
/// "WinFsp required" gate on, so its centered install notice can be eyeballed
/// on a machine where WinFsp is actually installed.
fn force_no_winfsp_from_args() -> bool {
    std::env::args().skip(1).any(|a| a == "--demo-no-winfsp")
}

/// Debug/verification aid: `--page clone` (etc.) opens the app on that page
/// instead of Backup. Unknown or absent values fall back to the default.
fn start_page_from_args() -> Option<Page> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--page" {
            return match args.next()?.to_ascii_lowercase().as_str() {
                "backup" => Some(Page::Backup),
                "clone" => Some(Page::Clone),
                "restore" => Some(Page::Restore),
                "verify" => Some(Page::Verify),
                "mount" => Some(Page::Mount),
                "virtualize" | "vm" => Some(Page::Virtualize)
                    .filter(|p| sidebar::page_available(*p, updater::is_portable())),
                "bootrepair" | "boot-repair" => Some(Page::BootRepair),
                "history" => Some(Page::History),
                "about" => Some(Page::About),
                _ => None,
            };
        }
    }
    None
}

fn default_backup_folder() -> String {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return format!(r"{profile}\Documents");
    }
    String::new()
}

/// Whether two paths name the same backup, compared case-insensitively the way
/// Windows itself would. Deliberately not `fs::canonicalize`: this is called
/// every frame to gate the Mount action, and canonicalizing opens the file —
/// once per frame against a backup sitting on a NAS is a real stall. The paths
/// being compared are the ones the picker produced, so they match verbatim in
/// every case that matters; an exotic spelling of an already-mounted path is
/// caught by Windows at attach time, not silently double-mounted.
fn same_file_path(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
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
        .add_filter("Phoenix Simulacra backup", &["phnx"])
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
