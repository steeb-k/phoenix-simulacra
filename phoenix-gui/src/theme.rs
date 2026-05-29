use eframe::egui;
use egui::{Color32, Response, Rounding, Stroke, Ui, Visuals};

/// Default Windows / WinUI accent (used when the live registry read fails).
const FALLBACK_ACCENT: Color32 = Color32::from_rgb(0x00, 0x78, 0xD4);

/// Colors used by the sidebar widget. The central panel pulls everything else
/// from the live `egui::Visuals` we install via [`apply`].
#[derive(Clone, Copy)]
pub struct Palette {
    pub accent: Color32,
    pub sidebar_bg: Color32,
    pub sidebar_selected_bg: Color32,
    pub sidebar_hover_bg: Color32,
    pub subtle_text: Color32,
    pub icon_color: Color32,
    /// Background tint for inputs that are missing a required value (e.g.
    /// the Backup name field on the Backup page). Picked to be clearly red
    /// without overwhelming the rest of the form in either theme.
    pub error_bg: Color32,
    /// Default fill color for input widgets (`TextEdit`, drag values, …),
    /// chosen to sit a notch off the panel background so form fields stand
    /// out at a glance instead of disappearing into the page. Light mode
    /// uses a recessed (slightly darker) tone; dark mode uses a raised
    /// (slightly lighter) tone — both subtle by design.
    pub input_bg: Color32,
    /// Fill for "go" action buttons (Start backup, Run restore, Quick/Full
    /// verify, …). Picked from Material green 800 for high contrast against
    /// white button text in both light and dark themes.
    pub success: Color32,
    /// Fill for "stop" action buttons (Cancel backup/restore/verify).
    /// Material red 800 — same readability story as `success`.
    pub danger: Color32,
    /// Fill for the modal's "Close" button after a job ends with a
    /// warning/cancelled outcome. Amber (Material amber 800) — readable
    /// against white button text and clearly distinct from success/danger.
    pub warning: Color32,
    pub light_mode: bool,
}

impl Palette {
    /// Blend `color` 65% toward `input_bg` to produce a faded version
    /// suitable for the disabled state of a colored button — keeps the
    /// hue so the user can still tell at a glance which action is Start
    /// vs Cancel, but mutes saturation so it reads as inactive.
    pub fn dim(&self, color: Color32) -> Color32 {
        tint(color, 0.65, self.input_bg)
    }
}

/// Read the current Windows accent + light/dark mode and apply them to `ctx`.
/// Returns the [`Palette`] used by the sidebar widget.
pub fn refresh(ctx: &egui::Context) -> Palette {
    let accent = read_accent_color();
    let light_mode = read_apps_use_light_theme();
    apply(ctx, accent, light_mode);
    palette_for(accent, light_mode)
}

fn palette_for(accent: Color32, light_mode: bool) -> Palette {
    if light_mode {
        Palette {
            accent,
            sidebar_bg: Color32::from_rgb(0xF3, 0xF3, 0xF3),
            sidebar_selected_bg: tint(accent, 0.12, Color32::WHITE),
            sidebar_hover_bg: Color32::from_black_alpha(12),
            subtle_text: Color32::from_rgb(0x60, 0x60, 0x60),
            icon_color: Color32::from_rgb(0x30, 0x30, 0x30),
            error_bg: Color32::from_rgb(0xFF, 0xD6, 0xD6),
            input_bg: Color32::from_rgb(0xF0, 0xF0, 0xF0),
            success: Color32::from_rgb(0x2E, 0x7D, 0x32),
            danger: Color32::from_rgb(0xC6, 0x28, 0x28),
            warning: Color32::from_rgb(0xFF, 0x8F, 0x00),
            light_mode: true,
        }
    } else {
        Palette {
            accent,
            sidebar_bg: Color32::from_rgb(0x1F, 0x1F, 0x1F),
            sidebar_selected_bg: tint(accent, 0.18, Color32::BLACK),
            sidebar_hover_bg: Color32::from_white_alpha(14),
            subtle_text: Color32::from_rgb(0xB0, 0xB0, 0xB0),
            icon_color: Color32::from_rgb(0xE6, 0xE6, 0xE6),
            error_bg: Color32::from_rgb(0x5A, 0x2A, 0x2A),
            input_bg: Color32::from_rgb(0x3C, 0x3C, 0x3C),
            success: Color32::from_rgb(0x2E, 0x7D, 0x32),
            danger: Color32::from_rgb(0xC6, 0x28, 0x28),
            warning: Color32::from_rgb(0xFF, 0x8F, 0x00),
            light_mode: false,
        }
    }
}

/// Apply the WinUI-flavored visuals (rounded corners, accent selection, subtle
/// panel separation between sidebar and central panel).
pub fn apply(ctx: &egui::Context, accent: Color32, light_mode: bool) {
    let mut visuals = if light_mode {
        Visuals::light()
    } else {
        Visuals::dark()
    };
    let pal = palette_for(accent, light_mode);

    visuals.selection.bg_fill = with_alpha(accent, 0x80);
    // egui's TextEdit reads `selection.stroke` for its focused-border, and
    // most other widgets (buttons, checkboxes, drag values, sliders, …)
    // read `widgets.active.bg_stroke` for their focused/pressed border.
    // The defaults are a 1px low-contrast line that's nearly invisible
    // — bumping both to a 2px foreground-colored stroke gives every
    // keyboard-focusable widget a uniform, clearly visible focus ring.
    visuals.selection.stroke = Stroke::new(2.0, pal.icon_color);
    visuals.hyperlink_color = accent;
    visuals.window_rounding = Rounding::same(8.0);
    visuals.menu_rounding = Rounding::same(6.0);

    visuals.panel_fill = if light_mode {
        Color32::from_rgb(0xFA, 0xFA, 0xFA)
    } else {
        Color32::from_rgb(0x2A, 0x2A, 0x2A)
    };
    visuals.widgets.hovered.bg_fill = pal.sidebar_hover_bg;
    visuals.widgets.active.bg_fill = pal.sidebar_selected_bg;
    visuals.widgets.active.bg_stroke = Stroke::new(2.0, pal.icon_color);
    // Slight panel-vs-input contrast so TextEdit and friends visibly read
    // as "fields" instead of blending into the page. Per-widget overrides
    // (e.g. the Backup name field's red error state) can still set their
    // own `extreme_bg_color` inside a `ui.scope`.
    visuals.extreme_bg_color = pal.input_bg;

    ctx.set_visuals(visuals);
}

/// Draw a 2px foreground-colored ring around `response.rect` while the
/// widget has keyboard focus. The global `widgets.active.bg_stroke` bump
/// in [`apply`] already gives buttons and text edits a visible focus
/// outline through their own frame painting, but widgets that paint
/// themselves directly (the refresh-disks button) or only paint a tiny
/// icon (the standard `Checkbox`) have no obvious focus affordance —
/// call this helper after creating them to add a uniform Tab indicator.
pub fn draw_focus_outline(ui: &Ui, response: &Response, palette: &Palette) {
    if !response.has_focus() {
        return;
    }
    ui.painter().rect_stroke(
        response.rect.expand(2.0),
        Rounding::same(4.0),
        Stroke::new(2.0, palette.icon_color),
    );
}

/// Lerp `a` toward `b` by `t` (0..1). Operates per-channel in sRGB space,
/// which is "good enough" for picking palette tints — strict color
/// correctness would do the lerp in linear-light, but for our use cases
/// (subtle hover tints, the progress bar's breathing animation) the
/// sRGB lerp matches what people expect a paint program would do.
pub fn tint(a: Color32, t: f32, b: Color32) -> Color32 {
    let mix = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t) as u8;
    Color32::from_rgba_unmultiplied(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
        255,
    )
}

fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

#[cfg(target_os = "windows")]
fn read_accent_color() -> Color32 {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(dwm) = hkcu.open_subkey(r"Software\Microsoft\Windows\DWM") else {
        return FALLBACK_ACCENT;
    };
    let Ok(raw) = dwm.get_value::<u32, _>("AccentColor") else {
        return FALLBACK_ACCENT;
    };
    // `AccentColor` is stored as 0xAABBGGRR (little-endian ABGR).
    let r = (raw & 0xFF) as u8;
    let g = ((raw >> 8) & 0xFF) as u8;
    let b = ((raw >> 16) & 0xFF) as u8;
    Color32::from_rgb(r, g, b)
}

#[cfg(not(target_os = "windows"))]
fn read_accent_color() -> Color32 {
    FALLBACK_ACCENT
}

#[cfg(target_os = "windows")]
fn read_apps_use_light_theme() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(personalize) = hkcu
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
    else {
        return false;
    };
    personalize
        .get_value::<u32, _>("AppsUseLightTheme")
        .map(|v| v != 0)
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
fn read_apps_use_light_theme() -> bool {
    false
}
