use eframe::egui;
use egui::{Color32, Rounding, Stroke, Visuals};

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
    pub light_mode: bool,
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

    visuals.selection.bg_fill = with_alpha(accent, 0x80);
    visuals.selection.stroke = Stroke::new(1.0, accent);
    visuals.hyperlink_color = accent;
    visuals.window_rounding = Rounding::same(8.0);
    visuals.menu_rounding = Rounding::same(6.0);

    let pal = palette_for(accent, light_mode);
    visuals.panel_fill = if light_mode {
        Color32::from_rgb(0xFA, 0xFA, 0xFA)
    } else {
        Color32::from_rgb(0x2A, 0x2A, 0x2A)
    };
    visuals.widgets.hovered.bg_fill = pal.sidebar_hover_bg;
    visuals.widgets.active.bg_fill = pal.sidebar_selected_bg;

    ctx.set_visuals(visuals);
}

/// Lerp `a` toward `b` by `t` (0..1).
fn tint(a: Color32, t: f32, b: Color32) -> Color32 {
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
