use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId, TextStyle};

const REGULAR: &str = "inter-regular";
const BOLD: &str = "inter-bold";
const PHOSPHOR: &str = "phosphor";
const PHOSPHOR_FILL: &str = "phosphor-fill";
const PHOSPHOR_BOLD: &str = "phosphor-bold";
const CAPTION: &str = "caption-icons";

/// Whether the system caption-glyph font (Segoe Fluent Icons / Segoe MDL2
/// Assets) was found and registered. Read via [`caption_icon`]; the titlebar
/// falls back to phosphor glyphs when it's absent (non-Windows dev builds).
static CAPTION_LOADED: AtomicBool = AtomicBool::new(false);

/// Register Inter Regular/Bold (embedded at compile time) and Phosphor icons,
/// then map egui text styles so headings use the bold face.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    fonts.font_data.insert(
        REGULAR.into(),
        FontData::from_static(include_bytes!("../../assets/Inter-Regular.ttf")),
    );
    fonts.font_data.insert(
        BOLD.into(),
        FontData::from_static(include_bytes!("../../assets/Inter-Bold.ttf")),
    );

    fonts
        .families
        .insert(FontFamily::Proportional, vec![REGULAR.into()]);
    fonts
        .families
        .insert(FontFamily::Monospace, vec![REGULAR.into()]);

    // Named family used by `fonts::bold()` and `TextStyle::Heading`. Must be
    // listed in `families`, not just `font_data`, or egui panics at first use.
    fonts.families.insert(
        FontFamily::Name(BOLD.into()),
        vec![BOLD.into(), REGULAR.into()],
    );

    // The caption buttons (minimize/maximize/close) render the exact glyphs
    // native Windows titlebars use: Segoe Fluent Icons on Windows 11, Segoe
    // MDL2 Assets on Windows 10 (same codepoints). Loaded from the system
    // font directory at runtime — these fonts are not redistributable, and
    // every supported Windows install ships one of them.
    if let Some(data) = load_caption_font() {
        fonts.font_data.insert(CAPTION.into(), data);
        fonts.families.insert(
            FontFamily::Name(CAPTION.into()),
            vec![CAPTION.into(), REGULAR.into()],
        );
        CAPTION_LOADED.store(true, Ordering::Relaxed);
    }

    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    // Icons must use the phosphor family directly. Inter sits first in
    // Proportional and claims PUA codepoints with blank/wrong glyphs, so
    // fallback never reaches phosphor when using FontId::proportional.
    //
    // Include Inter as a fallback here too: epaint needs '?' (or '◻') in the
    // font chain to build a "missing glyph" placeholder. Phosphor has neither,
    // which produced three WARN lines per startup (one per icon atlas size).
    fonts.families.insert(
        FontFamily::Name(PHOSPHOR.into()),
        vec![PHOSPHOR.into(), REGULAR.into()],
    );

    // Solid (filled) phosphor variant, used by the primary action buttons.
    // `egui_phosphor::add_to_fonts` always registers under the "phosphor"
    // key, so a second variant has to be added by hand.
    fonts.font_data.insert(
        PHOSPHOR_FILL.into(),
        egui_phosphor::Variant::Fill.font_data(),
    );
    fonts.families.insert(
        FontFamily::Name(PHOSPHOR_FILL.into()),
        vec![PHOSPHOR_FILL.into(), REGULAR.into()],
    );

    // Heavy-stroke phosphor variant, to pair an icon with Inter Bold (the
    // titlebar's Refresh button). Every variant maps the same codepoints, so
    // the weight is chosen purely by which family the `FontId` names.
    fonts.font_data.insert(
        PHOSPHOR_BOLD.into(),
        egui_phosphor::Variant::Bold.font_data(),
    );
    fonts.families.insert(
        FontFamily::Name(PHOSPHOR_BOLD.into()),
        vec![PHOSPHOR_BOLD.into(), BOLD.into()],
    );

    ctx.set_fonts(fonts);

    let bold_family = FontFamily::Name(BOLD.into());
    let mut style = (*ctx.style()).clone();
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::new(22.0, bold_family.clone()));
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    // Buttons app-wide land at 16pt to match the form inputs on the Backup
    // page (and the upgraded action row everywhere else). Pairs with the
    // larger `button_padding` set below so every button reads as the same
    // chunky pill-shaped control.
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(16.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(12.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(14.0, FontFamily::Monospace),
    );

    // Wider button padding so every `ui.button(…)` in the app renders at
    // ~36px tall (16pt text + 2*10px padding) and matches the TextEdit
    // outer height used on the Backup page. `interact_size.y` bumps the
    // minimum hit-target for buttons that compute their natural size
    // (e.g. short labels like "Browse…") so they don't collapse to the
    // old 18px default.
    style.spacing.button_padding = egui::vec2(12.0, 10.0);
    style.spacing.interact_size.y = 36.0;
    ctx.set_style(style);
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn icon(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(PHOSPHOR.into()))
}

/// Solid (filled) phosphor glyphs — pair with `egui_phosphor::fill::*`.
pub fn icon_fill(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(PHOSPHOR_FILL.into()))
}

/// Heavy-stroke phosphor glyphs — pair with `egui_phosphor::bold::*` (and
/// with [`bold`] text).
pub fn icon_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(PHOSPHOR_BOLD.into()))
}

/// FontId for the native Windows caption glyphs, or `None` when no system
/// caption font was found at startup.
pub fn caption_icon(size: f32) -> Option<FontId> {
    CAPTION_LOADED
        .load(Ordering::Relaxed)
        .then(|| FontId::new(size, FontFamily::Name(CAPTION.into())))
}

#[cfg(target_os = "windows")]
fn load_caption_font() -> Option<FontData> {
    let windir = std::env::var_os("WINDIR")?;
    let fonts_dir = std::path::PathBuf::from(windir).join("Fonts");
    // Windows 11 first (Segoe Fluent Icons), then the Windows 10 equivalent.
    for file in ["SegoeIcons.ttf", "segmdl2.ttf"] {
        if let Ok(bytes) = std::fs::read(fonts_dir.join(file)) {
            return Some(FontData::from_owned(bytes));
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn load_caption_font() -> Option<FontData> {
    None
}
