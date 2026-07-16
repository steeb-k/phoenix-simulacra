// Ship as a GUI-subsystem binary: double-clicking the exe (or launching it from
// the launcher) must NOT flash a console window. The console is opt-in at run
// time via `--debug`, which calls `attach_debug_console()` before logging is
// set up — see `main`.
#![windows_subsystem = "windows"]

mod about_panel;
mod action_bar;
mod clone_panel;
mod confirm_dialog;
mod disk_dropdown;
mod disk_map;
mod disk_panel;
mod fonts;
mod history_table;
mod job;
mod mount_table;
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
use crate::job::{spawn_backup, spawn_clone, spawn_restore, spawn_verify, BackgroundJob, JobKind};
use crate::sidebar::Page;
use crate::status_modal::{CompletedJob, JobOutcome, ModalAction, ModalView, Verdict};
use crate::theme::Palette;
use crate::util::format_bytes;

const THEME_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Debounce before auto-loading a `.phnx` path typed into the Restore page.
const RESTORE_BACKUP_LOAD_DEBOUNCE: Duration = Duration::from_millis(300);
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

/// Slack on the rasterizer's inside-triangle test, as a fraction of a
/// triangle's own size (the test runs on normalised barycentric weights).
///
/// egui antialiases by tessellating a solid core plus a 1px feathered ring
/// that share an edge, so on any shape edge landing on a whole pixel
/// coordinate — every panel, every button — that shared edge falls exactly
/// down the middle of a row of pixel centres. Exact arithmetic would put
/// those centres on both triangles; f32 puts them a few 1e-8 outside *both*,
/// and the pixel is dropped, letting the background dot through the fill.
/// Real geometry misses by ~1e-1, so anything in between separates float
/// noise from a genuine miss with orders of magnitude to spare.
const EDGE_EPS: f32 = 1e-5;

/// Pack an opaque pixel the way softbuffer expects it on Windows: `0x00RRGGBB`
/// (the presenter ignores the top byte).
#[inline]
fn to_bgra(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | (b as u32)
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
        }
    }

    /// Run one egui frame and rasterize it into the softbuffer surface on the
    /// CPU: tessellate the UI into triangle meshes, walk each with barycentric
    /// interpolation while sampling the font/texture atlas, blend per-pixel,
    /// then present. No GPU is touched anywhere in here.
    fn paint(&mut self) {
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
        let bg32 = to_bgra(bg.r(), bg.g(), bg.b());
        let mut pixels: Vec<u32> = vec![bg32; (w * h) as usize];

        for prim in &clipped {
            let clip = prim.clip_rect;
            let clip_x0 = (clip.min.x * ppp).floor() as i32;
            let clip_y0 = (clip.min.y * ppp).floor() as i32;
            let clip_x1 = (clip.max.x * ppp).ceil() as i32;
            let clip_y1 = (clip.max.y * ppp).ceil() as i32;

            let egui::epaint::Primitive::Mesh(mesh) = &prim.primitive else {
                continue;
            };
            let verts = &mesh.vertices;
            let indices = &mesh.indices;
            let is_font_tex = mesh.texture_id == egui::TextureId::default();

            for tri in 0..indices.len() / 3 {
                let i0 = indices[tri * 3] as usize;
                let i1 = indices[tri * 3 + 1] as usize;
                let i2 = indices[tri * 3 + 2] as usize;
                if i0 >= verts.len() || i1 >= verts.len() || i2 >= verts.len() {
                    continue;
                }
                let v0 = &verts[i0];
                let v1 = &verts[i1];
                let v2 = &verts[i2];

                let p0 = (v0.pos.x * ppp, v0.pos.y * ppp);
                let p1 = (v1.pos.x * ppp, v1.pos.y * ppp);
                let p2 = (v2.pos.x * ppp, v2.pos.y * ppp);

                let min_x = p0.0.min(p1.0).min(p2.0).floor() as i32;
                let min_y = p0.1.min(p1.1).min(p2.1).floor() as i32;
                let max_x = p0.0.max(p1.0).max(p2.0).ceil() as i32;
                let max_y = p0.1.max(p1.1).max(p2.1).ceil() as i32;

                let x0 = min_x.max(clip_x0).max(0);
                let y0 = min_y.max(clip_y0).max(0);
                let x1 = max_x.min(clip_x1).min(w as i32 - 1);
                let y1 = max_y.min(clip_y1).min(h as i32 - 1);

                let denom = (p1.1 - p2.1) * (p0.0 - p2.0) + (p2.0 - p1.0) * (p0.1 - p2.1);
                if denom.abs() < 0.001 {
                    continue;
                }

                for py in y0..=y1 {
                    for px in x0..=x1 {
                        let fx = px as f32 + 0.5;
                        let fy = py as f32 + 0.5;

                        let w0 =
                            ((p1.1 - p2.1) * (fx - p2.0) + (p2.0 - p1.0) * (fy - p2.1)) / denom;
                        let w1 =
                            ((p2.1 - p0.1) * (fx - p2.0) + (p0.0 - p2.0) * (fy - p2.1)) / denom;
                        let w2 = 1.0 - w0 - w1;
                        if w0 < -EDGE_EPS || w1 < -EDGE_EPS || w2 < -EDGE_EPS {
                            continue;
                        }

                        let uv_x = v0.uv.x * w0 + v1.uv.x * w1 + v2.uv.x * w2;
                        let uv_y = v0.uv.y * w0 + v1.uv.y * w1 + v2.uv.y * w2;

                        let vr = v0.color.r() as f32 * w0
                            + v1.color.r() as f32 * w1
                            + v2.color.r() as f32 * w2;
                        let vg = v0.color.g() as f32 * w0
                            + v1.color.g() as f32 * w1
                            + v2.color.g() as f32 * w2;
                        let vb = v0.color.b() as f32 * w0
                            + v1.color.b() as f32 * w1
                            + v2.color.b() as f32 * w2;
                        let va = v0.color.a() as f32 * w0
                            + v1.color.a() as f32 * w1
                            + v2.color.a() as f32 * w2;

                        let (r, g, b, final_a) = if is_font_tex {
                            let cov = self.tex_mgr.sample_alpha_f(uv_x, uv_y);
                            (vr, vg, vb, va / 255.0 * cov)
                        } else {
                            let [tr, tg, tb, ta] =
                                self.tex_mgr.sample_rgba(mesh.texture_id, uv_x, uv_y);
                            (tr * vr, tg * vg, tb * vb, ta * va / 255.0)
                        };
                        // A pixel admitted by EDGE_EPS sits marginally outside the
                        // triangle, so its interpolated alpha can land a hair
                        // outside [0, 1]. Left alone, a negative `inv` below can
                        // push the blend under zero, and `sqrt` of that is NaN —
                        // which casts to 0 and paints the black speck this whole
                        // epsilon exists to remove.
                        let final_a = final_a.clamp(0.0, 1.0);
                        if final_a < 1.0 / 255.0 {
                            continue;
                        }

                        let idx = (py as u32 * w + px as u32) as usize;
                        let dst = pixels[idx];
                        let dr = ((dst >> 16) & 0xFF) as f32;
                        let dg = ((dst >> 8) & 0xFF) as f32;
                        let db = (dst & 0xFF) as f32;

                        // Gamma-correct blend (gamma ≈ 2.0): linearise, blend,
                        // re-encode. Approximate vs true sRGB ~2.2 but cheap.
                        let inv = 1.0 - final_a;
                        let r_lin = (r / 255.0) * (r / 255.0);
                        let g_lin = (g / 255.0) * (g / 255.0);
                        let b_lin = (b / 255.0) * (b / 255.0);
                        let dr_lin = (dr / 255.0) * (dr / 255.0);
                        let dg_lin = (dg / 255.0) * (dg / 255.0);
                        let db_lin = (db / 255.0) * (db / 255.0);
                        let out_r = ((r_lin * final_a + dr_lin * inv).sqrt() * 255.0 + 0.5) as u8;
                        let out_g = ((g_lin * final_a + dg_lin * inv).sqrt() * 255.0 + 0.5) as u8;
                        let out_b = ((b_lin * final_a + db_lin * inv).sqrt() * 255.0 + 0.5) as u8;

                        pixels[idx] = to_bgra(out_r, out_g, out_b);
                    }
                }
            }
        }

        if self
            .surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .is_ok()
        {
            if let Ok(mut buf) = self.surface.buffer_mut() {
                buf.copy_from_slice(&pixels);
                let _ = buf.present();
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
            let renderer = Renderer::new(event_loop, self.proxy.clone());
            renderer.window.request_redraw();
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
                    if r.app.anything_mounted() && !r.app.pending_close {
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
                let delay = if after.is_zero() { REPAINT_FRAME_CAP } else { after };
                let deadline = Instant::now() + delay;
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
             phoenix_mount=info,phoenix_clone=info,warn"
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
    /// When the user edits the backup path, load after this instant.
    restore_backup_load_after: Option<Instant>,
    /// Set by Browse to load immediately on the next frame.
    restore_backup_load_now: bool,
    restore_layout: Option<restore_layout::RestoreLayoutState>,
    /// Restore page's chosen target disk; `None` until the user picks one
    /// (deliberately never auto-selected — same contract as the Clone page).
    target_disk_index: Option<u32>,
    /// Clone page state: chosen source/target disk indices and options.
    clone_source_index: Option<u32>,
    clone_target_index: Option<u32>,
    clone_expand: bool,
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
            restore_backup_load_after: None,
            restore_backup_load_now: false,
            restore_layout: None,
            target_disk_index: None,
            clone_source_index: None,
            clone_target_index: None,
            clone_expand: false,
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
            should_exit: false,
            restart_after_update: false,
            portable: updater::is_portable(),
        };
        app.refresh_disks();
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
            Page::Verify | Page::Mount | Page::About => {}
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
    }

    fn clear_restore_ui_state(&mut self) {
        self.restore_loaded_path.clear();
        self.restore_backup_load_after = None;
        self.restore_backup_load_now = false;
        self.restore_layout = None;
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
            Page::Backup => StartAction {
                label: "Start Backup",
                icon: Some(egui_phosphor::fill::PLAY),
                enabled: !busy && !self.backup_name.trim().is_empty(),
                disabled_hint: Some(if busy {
                    "A backup is already running"
                } else {
                    "Enter a backup name first"
                }),
            },
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
        let nav = sidebar::show(
            ctx,
            &mut self.page,
            &self.palette,
            modal_open,
            &mut self.settings.theme,
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
        self.show_history_delete_dialog(ctx);
        self.show_demo_confirm_dialog(ctx);
        self.show_demo_progress_modal(ctx);
        self.show_close_dialog(ctx);
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
            ConfirmAction::None => {}
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
            ConfirmAction::None => self.pending_clone = Some(pending),
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
            ConfirmAction::None => self.pending_restore = Some(pending),
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
        // Nothing left to warn about (the last mount was unmounted while the
        // dialog was up): let the close through.
        if !self.anything_mounted() {
            self.pending_close = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let details = self.mount_summaries();
        let message = format!(
            "Closing Phoenix Simulacra unmounts {} and removes {} drive letters. \
             Any open files or Explorer windows on those drives will be disconnected.",
            if details.len() == 1 {
                "the mounted backup".to_string()
            } else {
                format!("all {} mounted backups", details.len())
            },
            if details.len() == 1 { "its" } else { "their" },
        );
        let mut view = ConfirmView {
            title: "Backups are still mounted",
            message: &message,
            details: &details,
            confirm_label: "Unmount & Close",
            cancel_label: "Keep Open",
            confirm_danger: false,
            hazard_tape: true,
            ack: None,
        };
        match confirm_dialog::show(ctx, &self.palette, &mut view) {
            ConfirmAction::Confirm => {
                self.pending_close = false;
                self.unmount_all();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ConfirmAction::Cancel => self.pending_close = false,
            ConfirmAction::None => {}
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
        };
        if confirm_dialog::show(ctx, &self.palette, &mut view) != ConfirmAction::None {
            self.demo_confirm = false;
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
        egui::ScrollArea::vertical()
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
    });
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
fn backup_path_picker(
    ui: &mut egui::Ui,
    label: &str,
    hint: &str,
    path: &mut String,
    mut on_path_changed: Option<&mut dyn FnMut()>,
) -> bool {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).font(fonts::bold(14.0)));

        let spacing = ui.spacing().item_spacing.x;
        let text_w = (ui.available_width() - FORM_BUTTON_W - spacing).max(0.0);

        let field_response = ui.add_sized(
            [text_w, ACTION_BUTTON_HEIGHT],
            egui::TextEdit::singleline(path)
                .desired_width(f32::INFINITY)
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

        let browse_label = icon_label(
            egui_phosphor::regular::FOLDER,
            fonts::icon(16.0),
            "Browse…",
            fonts::regular(14.0),
            ui.visuals().widgets.inactive.fg_stroke.color,
        );
        let mut browsed = false;
        if ui
            .add_sized(
                [FORM_BUTTON_W, ACTION_BUTTON_HEIGHT],
                egui::Button::new(browse_label),
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
    if !subtitle.is_empty() {
        ui.label(egui::RichText::new(subtitle).color(palette.subtle_text));
    }
    ui.add_space(14.0);
}

impl PhoenixApp {
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

    /// The `.phnx` Browse row plus the (debounced) load of whatever it names
    /// into `restore_layout`. Shared by the Restore and Verify pages: both
    /// read the same image, off the same path field, so both get the loaded
    /// layout — the editor on one, the preview on the other.
    fn ui_backup_file_picker(&mut self, ui: &mut egui::Ui, label: &str) {
        let path_before = self.restore_backup_path.clone();
        let mut path_edited = false;
        let browsed = backup_path_picker(
            ui,
            label,
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
                        "Expand the last NTFS partition to fill a larger target",
                    )
                    .changed()
            {
                // Re-seed so the target map previews the grown layout.
                self.reseed_full_disk_clone();
            }
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

    fn start_mount(&mut self) {
        if self.selected_backup_already_mounted() {
            self.status = "That backup is already mounted".into();
            return;
        }
        let path = std::path::PathBuf::from(self.mount_backup_path.trim());
        let scratch = std::env::temp_dir().join("PhoenixSimulacra").join("mounts");
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
        };
        match confirm_dialog::show(ctx, &self.palette, &mut view) {
            ConfirmAction::Confirm => {
                self.pending_history_delete = None;
                if self.history.remove(index) && !self.demo_history {
                    let _ = self.history.save();
                }
            }
            ConfirmAction::Cancel => self.pending_history_delete = None,
            ConfirmAction::None => {}
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
        },
        mount_table::MountRow {
            folder: r"\\nas\archive\simulacra\weekly\workstation\2026\july".into(),
            name: "workstation-2026-07-12.phnx".into(),
            size: 512_110_190_592,
            letters: vec!['K'],
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
