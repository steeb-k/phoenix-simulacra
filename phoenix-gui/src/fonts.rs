use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId, TextStyle};

const REGULAR: &str = "inter-regular";
const BOLD: &str = "inter-bold";
const PHOSPHOR: &str = "phosphor";

/// Register Inter Regular/Bold (embedded at compile time) and Phosphor icons,
/// then map egui text styles so headings use the bold face.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    fonts.font_data.insert(
        REGULAR.into(),
        FontData::from_static(include_bytes!("../../Inter-Regular.ttf")),
    );
    fonts.font_data.insert(
        BOLD.into(),
        FontData::from_static(include_bytes!("../../Inter-Bold.ttf")),
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

    ctx.set_fonts(fonts);

    let bold_family = FontFamily::Name(BOLD.into());
    let mut style = (*ctx.style()).clone();
    style.text_styles.insert(TextStyle::Heading, FontId::new(22.0, bold_family.clone()));
    style.text_styles.insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Button, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Small, FontId::new(12.0, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace));
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
