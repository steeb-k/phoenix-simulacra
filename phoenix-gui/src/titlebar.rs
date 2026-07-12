//! Custom titlebar for the chromeless main window.
//!
//! The OS titlebar is turned off (`ViewportBuilder::with_decorations(false)`)
//! and this module draws the replacement: a 32px strip with the app icon,
//! window title, and a minimize/maximize/close control box styled like the
//! native Windows 11 one (same 46×32 buttons, same Segoe Fluent glyphs, same
//! hover/press colors).
//!
//! On Windows the window procedure is additionally subclassed (see [`nc`])
//! to answer `WM_NCHITTEST` with real non-client hit codes, so everything
//! *behaves* native too: edge drag-resize with the proper cursors, titlebar
//! drag with Aero Snap/Shake, double-click to maximize, right-click system
//! menu, and the Windows 11 Snap Layouts flyout on the maximize button
//! (which only appears when the button reports `HTMAXBUTTON`). Because those
//! areas become non-client, the pointer never reaches egui there — the
//! buttons here are painted with hover/press state fed back from the
//! subclass, while the click semantics run through `WM_SYSCOMMAND` like any
//! decorated window. The egui-side `interact`s below are a portable fallback
//! that only sees events when the subclass isn't installed.

use eframe::egui;
use egui::{Align2, Color32, Context, Rect, Sense, Ui, Vec2, ViewportCommand};

use crate::fonts;
use crate::theme::{self, Palette};

/// Height of the titlebar strip in logical points — the Windows 11 standard
/// caption height.
pub const TITLEBAR_HEIGHT: f32 = 32.0;
/// Caption button size (Windows 11 standard: 46×32 per button).
const BUTTON_WIDTH: f32 = 46.0;
/// Invisible resize band inside the window edges, logical points. A
/// chromeless window has no outer frame, so the grab zone overlaps the
/// client area — same approach as VS Code / Chromium.
const RESIZE_BORDER: f32 = 5.0;
/// Close-button hover red (WinUI `#C42B1C`), identical in light and dark.
const CLOSE_RED: Color32 = Color32::from_rgb(0xC4, 0x2B, 0x1C);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Caption {
    Minimize = 0,
    Maximize = 1,
    Close = 2,
}

/// Render the titlebar strip across the top of the window. Call before any
/// other panel so it spans the full width.
pub fn show(ctx: &Context, palette: &Palette) {
    egui::TopBottomPanel::top("titlebar")
        .exact_height(TITLEBAR_HEIGHT)
        .show_separator_line(false)
        .frame(egui::Frame::none().fill(palette.sidebar_bg))
        .show(ctx, |ui| draw(ui, palette));
}

fn draw(ui: &mut Ui, palette: &Palette) {
    let rect = ui.max_rect();
    let focused = ui.input(|i| i.viewport().focused.unwrap_or(true));
    let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));

    // Control box, right-aligned and flush with the window corner.
    let mut right = rect.right();
    let mut button_rects = [Rect::NOTHING; 3]; // indexed by `Caption as usize`
    for which in [Caption::Close, Caption::Maximize, Caption::Minimize] {
        let r = Rect::from_min_max(
            egui::pos2(right - BUTTON_WIDTH, rect.top()),
            egui::pos2(right, rect.bottom()),
        );
        button_rects[which as usize] = r;
        right = r.left();
        caption_button(ui, r, which, palette, focused, maximized);
    }

    // Everything left of the control box doubles as the drag strip
    // (fallback path — on Windows the subclass reports HTCAPTION here and
    // the native move loop handles dragging before egui ever sees it).
    let drag_rect = Rect::from_min_max(rect.min, egui::pos2(right, rect.bottom()));
    let drag = ui.interact(
        drag_rect,
        ui.id().with("titlebar-drag"),
        Sense::click_and_drag(),
    );
    if drag.double_clicked() {
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    } else if drag.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }

    // App icon + window title at the left, like a decorated window. The
    // title dims when the window loses focus, exactly as native chrome does.
    let icon_rect = Rect::from_center_size(
        egui::pos2(rect.left() + 12.0 + 8.0, rect.center().y),
        Vec2::splat(16.0),
    );
    egui::Image::new(egui::include_image!("../../carbon-phoenix-icon.png"))
        .fit_to_exact_size(Vec2::splat(16.0))
        .paint_at(ui, icon_rect);
    let title_color = if focused {
        palette.icon_color
    } else {
        palette.icon_color.gamma_multiply(0.4)
    };
    ui.painter().text(
        egui::pos2(icon_rect.right() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        "Carbon Phoenix",
        fonts::regular(12.0),
        title_color,
    );

    nc::publish_geometry(ui.ctx(), rect, &button_rects);
}

fn caption_button(
    ui: &mut Ui,
    rect: Rect,
    which: Caption,
    palette: &Palette,
    focused: bool,
    maximized: bool,
) {
    // Portable fallback interaction — inert on Windows, where these clicks
    // arrive as non-client messages and run through WM_SYSCOMMAND instead.
    let resp = ui.interact(rect, ui.id().with(("caption", which as u8)), Sense::click());
    if resp.clicked() {
        match which {
            Caption::Minimize => ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true)),
            Caption::Maximize => ui
                .ctx()
                .send_viewport_cmd(ViewportCommand::Maximized(!maximized)),
            Caption::Close => ui.ctx().send_viewport_cmd(ViewportCommand::Close),
        }
    }

    let nc_hover = nc::hovered() == Some(which);
    let hovered = nc_hover || resp.hovered();
    let pressed =
        (nc_hover && nc::pressed() == Some(which)) || resp.is_pointer_button_down_on();

    // Win11 styling: subtle overlay for minimize/maximize, fixed red for
    // close. Native pressed states are *fainter* than hover, not stronger.
    let subtle = |alpha: u8| {
        if palette.light_mode {
            Color32::from_black_alpha(alpha)
        } else {
            Color32::from_white_alpha(alpha)
        }
    };
    let idle_glyph = if focused {
        palette.icon_color
    } else {
        palette.icon_color.gamma_multiply(0.4)
    };
    let (fill, glyph_color) = match (which, pressed, hovered) {
        (Caption::Close, true, _) => (
            theme::tint(CLOSE_RED, 0.22, palette.sidebar_bg),
            Color32::WHITE.gamma_multiply(0.8),
        ),
        (Caption::Close, false, true) => (CLOSE_RED, Color32::WHITE),
        (_, true, _) => (subtle(10), palette.icon_color.gamma_multiply(0.85)),
        (_, false, true) => (subtle(16), palette.icon_color),
        _ => (Color32::TRANSPARENT, idle_glyph),
    };
    let painter = ui.painter();
    if fill != Color32::TRANSPARENT {
        painter.rect_filled(rect, 0.0, fill);
    }

    // Native glyphs (Segoe Fluent Icons / MDL2 "Chrome*" codepoints at 10pt,
    // the exact glyphs and size DWM uses), with a phosphor stand-in when no
    // system caption font was found.
    let (fluent, phosphor) = match which {
        Caption::Minimize => ("\u{E921}", egui_phosphor::regular::MINUS),
        Caption::Maximize if maximized => ("\u{E923}", egui_phosphor::regular::COPY_SIMPLE),
        Caption::Maximize => ("\u{E922}", egui_phosphor::regular::SQUARE),
        Caption::Close => ("\u{E8BB}", egui_phosphor::regular::X),
    };
    match fonts::caption_icon(10.0) {
        Some(font) => {
            painter.text(rect.center(), Align2::CENTER_CENTER, fluent, font, glyph_color);
        }
        None => {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                phosphor,
                fonts::icon(14.0),
                glyph_color,
            );
        }
    }
}

#[cfg(target_os = "windows")]
pub use nc::install;

/// Non-client integration: wndproc subclass + DWM shadow/corners.
#[cfg(target_os = "windows")]
mod nc {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};

    use eframe::egui::{Context, Rect};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE,
        DWMWCP_ROUND,
    };
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::Controls::MARGINS;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
    };
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetSystemMenu, IsZoomed, PostMessageW, TrackPopupMenu, HTBOTTOM,
        HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLOSE, HTLEFT, HTMAXBUTTON, HTMINBUTTON,
        HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_RESTORE,
        TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCDESTROY, WM_NCHITTEST,
        WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE,
        WM_NCRBUTTONUP, WM_SYSCOMMAND,
    };

    use super::Caption;

    const SUBCLASS_ID: usize = 0x504E_5854; // "PNXT"

    /// HT code of the caption button the pointer is over / pressing
    /// (0 = none). Written by the wndproc, read by the egui painter.
    static HOVER: AtomicU32 = AtomicU32::new(0);
    static PRESSED: AtomicU32 = AtomicU32::new(0);

    /// Titlebar geometry in *physical client pixels*. Republished by egui
    /// every frame so window resizes and DPI changes are always current.
    #[derive(Clone, Copy)]
    struct Geometry {
        titlebar_h: i32,
        border: i32,
        /// (left, top, right, bottom), indexed by `Caption as usize`.
        buttons: [(i32, i32, i32, i32); 3],
    }
    static GEOMETRY: Mutex<Geometry> = Mutex::new(Geometry {
        titlebar_h: 0,
        border: 0,
        buttons: [(0, 0, 0, 0); 3],
    });
    static CONTEXT: OnceLock<Context> = OnceLock::new();

    /// Hook the native window: DWM drop shadow + Win11 rounded corners, and
    /// a wndproc subclass that gives the chromeless window native non-client
    /// behavior (see module docs on [`super`]).
    pub fn install(hwnd: isize, ctx: Context) {
        let _ = CONTEXT.set(ctx);
        let hwnd = hwnd as HWND;
        unsafe {
            // A 1px frame extension is the canonical trick to make DWM draw
            // the standard drop shadow around a borderless window; the pixel
            // itself is overdrawn by our opaque UI.
            let margins = MARGINS {
                cxLeftWidth: 0,
                cxRightWidth: 0,
                cyTopHeight: 0,
                cyBottomHeight: 1,
            };
            DwmExtendFrameIntoClientArea(hwnd, &margins);
            // Windows 11 rounds decorated windows automatically; ask
            // explicitly since we're borderless. No-op on Windows 10.
            let pref = DWMWCP_ROUND;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &pref as *const _ as *const _,
                std::mem::size_of_val(&pref) as u32,
            );
            SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0);
        }
    }

    pub(super) fn publish_geometry(ctx: &Context, titlebar: Rect, buttons: &[Rect; 3]) {
        let ppp = ctx.pixels_per_point();
        let px = |v: f32| (v * ppp).round() as i32;
        let mut geo = GEOMETRY.lock().unwrap();
        geo.titlebar_h = px(titlebar.bottom());
        geo.border = px(super::RESIZE_BORDER).max(4);
        for (i, r) in buttons.iter().enumerate() {
            geo.buttons[i] = (px(r.left()), px(r.top()), px(r.right()), px(r.bottom()));
        }
    }

    pub(super) fn hovered() -> Option<Caption> {
        caption_from_ht(HOVER.load(Ordering::Relaxed))
    }

    pub(super) fn pressed() -> Option<Caption> {
        caption_from_ht(PRESSED.load(Ordering::Relaxed))
    }

    fn caption_from_ht(ht: u32) -> Option<Caption> {
        match ht {
            HTMINBUTTON => Some(Caption::Minimize),
            HTMAXBUTTON => Some(Caption::Maximize),
            HTCLOSE => Some(Caption::Close),
            _ => None,
        }
    }

    fn caption_ht(ht: u32) -> u32 {
        if caption_from_ht(ht).is_some() {
            ht
        } else {
            0
        }
    }

    fn set_hover(ht: u32) {
        if HOVER.swap(ht, Ordering::Relaxed) != ht {
            repaint();
        }
    }

    fn repaint() {
        if let Some(ctx) = CONTEXT.get() {
            ctx.request_repaint();
        }
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        match msg {
            WM_NCHITTEST => {
                if let Some(ht) = hit_test(hwnd, lparam) {
                    return ht as LRESULT;
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_NCMOUSEMOVE => {
                set_hover(caption_ht(wparam as u32));
                // Ask for WM_NCMOUSELEAVE so the hover highlight clears when
                // the pointer leaves the window through the titlebar.
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_MOUSEMOVE | WM_NCMOUSELEAVE => {
                set_hover(0);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => {
                let ht = caption_ht(wparam as u32);
                if ht != 0 {
                    if PRESSED.swap(ht, Ordering::Relaxed) != ht {
                        repaint();
                    }
                    // Swallow: the click fires on WM_NCLBUTTONUP, like the
                    // native buttons. Letting DefWindowProc see a press on
                    // HTMAXBUTTON & co. would start classic button tracking.
                    return 0;
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_NCLBUTTONUP => {
                let was = PRESSED.swap(0, Ordering::Relaxed);
                if was != 0 {
                    repaint();
                    // Only fire if released over the same button it went
                    // down on (native press-and-slide-away cancels).
                    if was == wparam as u32 {
                        let cmd = match was {
                            HTMINBUTTON => SC_MINIMIZE,
                            HTMAXBUTTON => {
                                if IsZoomed(hwnd) != 0 {
                                    SC_RESTORE
                                } else {
                                    SC_MAXIMIZE
                                }
                            }
                            _ => SC_CLOSE,
                        };
                        PostMessageW(hwnd, WM_SYSCOMMAND, cmd as WPARAM, 0);
                    }
                    return 0;
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONUP => {
                if PRESSED.swap(0, Ordering::Relaxed) != 0 {
                    repaint();
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_NCRBUTTONUP => {
                if wparam as u32 == HTCAPTION {
                    show_system_menu(hwnd, lparam);
                    return 0;
                }
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }

    /// Map a screen-space `WM_NCHITTEST` point to our hit codes: resize
    /// borders first (they win over the buttons at the very edge, matching
    /// native precedence), then caption buttons, then the drag strip.
    /// `None` falls through to the default (`HTCLIENT` inside the window).
    unsafe fn hit_test(hwnd: HWND, lparam: LPARAM) -> Option<u32> {
        let mut pt = POINT {
            x: (lparam & 0xFFFF) as i16 as i32,
            y: ((lparam >> 16) & 0xFFFF) as i16 as i32,
        };
        if ScreenToClient(hwnd, &mut pt) == 0 {
            return None;
        }
        let mut client = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetClientRect(hwnd, &mut client) == 0 {
            return None;
        }
        let (w, h) = (client.right, client.bottom);
        if pt.x < 0 || pt.y < 0 || pt.x >= w || pt.y >= h {
            return None;
        }

        // try_lock: never block the wndproc. Zero height means egui hasn't
        // published a frame yet — behave like a plain client area.
        let geo = *GEOMETRY.try_lock().ok()?;
        if geo.titlebar_h == 0 {
            return None;
        }

        if IsZoomed(hwnd) == 0 {
            let b = geo.border;
            let c = b * 2; // wider diagonal grab zones at the corners
            let top = pt.y < b;
            let bottom = pt.y >= h - b;
            let left = pt.x < b;
            let right = pt.x >= w - b;
            if top || bottom || left || right {
                let near_l = pt.x < c;
                let near_r = pt.x >= w - c;
                let near_t = pt.y < c;
                let near_b = pt.y >= h - c;
                return Some(if top && near_l || left && near_t {
                    HTTOPLEFT
                } else if top && near_r || right && near_t {
                    HTTOPRIGHT
                } else if bottom && near_l || left && near_b {
                    HTBOTTOMLEFT
                } else if bottom && near_r || right && near_b {
                    HTBOTTOMRIGHT
                } else if top {
                    HTTOP
                } else if bottom {
                    HTBOTTOM
                } else if left {
                    HTLEFT
                } else {
                    HTRIGHT
                });
            }
        }

        if pt.y < geo.titlebar_h {
            for (i, (l, t, r, btm)) in geo.buttons.iter().copied().enumerate() {
                if pt.x >= l && pt.x < r && pt.y >= t && pt.y < btm {
                    return Some([HTMINBUTTON, HTMAXBUTTON, HTCLOSE][i]);
                }
            }
            return Some(HTCAPTION);
        }
        None
    }

    /// Right-click on the titlebar: the standard system menu, dispatched
    /// through WM_SYSCOMMAND exactly like a decorated window's.
    unsafe fn show_system_menu(hwnd: HWND, lparam: LPARAM) {
        let menu = GetSystemMenu(hwnd, 0);
        if menu.is_null() {
            return;
        }
        let x = (lparam & 0xFFFF) as i16 as i32;
        let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            x,
            y,
            0,
            hwnd,
            std::ptr::null(),
        );
        if cmd != 0 {
            PostMessageW(hwnd, WM_SYSCOMMAND, cmd as WPARAM, 0);
        }
    }
}

/// Stubs so the egui layer compiles unchanged off-Windows (the fallback
/// `interact`s above provide the behavior there).
#[cfg(not(target_os = "windows"))]
mod nc {
    use eframe::egui::{Context, Rect};

    use super::Caption;

    pub(super) fn publish_geometry(_: &Context, _: Rect, _: &[Rect; 3]) {}

    pub(super) fn hovered() -> Option<Caption> {
        None
    }

    pub(super) fn pressed() -> Option<Caption> {
        None
    }
}
