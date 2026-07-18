//! The About page: what version is running, where it came from, and how to
//! support it.
//!
//! This is also the app's only "settings" surface now — and it deliberately
//! holds no settings. The one preference worth flipping (theme) lives in the
//! sidebar pill, the backup folder remembers itself, and verify-after-backup
//! is not negotiable. What's left is provenance.

use egui;
use egui::{Color32, Vec2};

use crate::theme::{self, Palette};
use crate::{fonts, icon_label, page_header, ACTION_BUTTON_HEIGHT};

/// Where to send people who want to see what else this author makes — and where
/// a portable build points them to pick up a new version by hand, since it never
/// downloads one itself (see [`crate::updater::is_portable`]).
pub const HOME_URL: &str = "https://kznjk.com/";
const SOURCE_URL: &str = "https://github.com/steeb-k/phoenix-simulacra";
const KOFI_URL: &str = "https://ko-fi.com/kznjk";
const COPYRIGHT: &str = "© 2026 Steve Kzenjak";

/// Display size of the app icon inside the card. The source asset is the same
/// 256px PNG the OS window icon is decoded from, so it stays crisp here.
const ICON_SIZE: f32 = 128.0;
const CARD_ROUNDING: f32 = 10.0;
const CARD_PAD: f32 = 18.0;
/// Gap between the icon and the text column beside it.
const ICON_TEXT_GAP: f32 = 20.0;
/// Width shared by the card's two buttons (Check for updates, Ko-fi), so the
/// card's right and left columns both end on a predictable line.
const BUTTON_W: f32 = 190.0;

/// Render the page. Returns `true` when "Check for updates" was clicked —
/// today a placeholder the caller answers with a status line, because there
/// is no release feed to check against yet.
pub fn show(ui: &mut egui::Ui, palette: &Palette, version: &str) -> bool {
    page_header(ui, palette, "About", "");

    egui::Frame::none()
        .fill(card_fill(palette))
        .rounding(egui::Rounding::same(CARD_ROUNDING))
        .stroke(egui::Stroke::new(1.0, palette.panel_border))
        .inner_margin(egui::Margin::same(CARD_PAD))
        .show(ui, |ui| {
            // A Frame sizes itself to its content, so without this the card
            // would only be as wide as its columns.
            // `available_width` here is already inside the inner margin.
            ui.set_width(ui.available_width());

            // Two columns: the facts (version, links, Ko-fi, copyright) take
            // the slack on the left; the identity column — app icon with the
            // update button under it — is pinned to the right at the shared
            // button width so both controls end on the same line.
            let mut clicked = false;
            ui.horizontal_top(|ui| {
                let left_w = (ui.available_width() - BUTTON_W - ICON_TEXT_GAP).max(0.0);
                ui.vertical(|ui| {
                    ui.set_width(left_w);
                    details(ui, palette, version);
                });
                ui.add_space(ICON_TEXT_GAP);
                ui.vertical(|ui| {
                    ui.set_width(BUTTON_W);
                    // Center the icon over the button beneath it.
                    ui.horizontal(|ui| {
                        ui.add_space(((BUTTON_W - ICON_SIZE) * 0.5).max(0.0));
                        ui.add(
                            egui::Image::new(egui::include_image!(
                                "../../assets/phoenix-appicon-256px.png"
                            ))
                            .fit_to_exact_size(Vec2::splat(ICON_SIZE)),
                        );
                    });
                    ui.add_space(10.0);
                    let label = icon_label(
                        egui_phosphor::regular::CLOUD_ARROW_DOWN,
                        fonts::icon(16.0),
                        "Check for updates",
                        fonts::regular(14.0),
                        ui.visuals().widgets.inactive.fg_stroke.color,
                    );
                    clicked = ui
                        .add_sized([BUTTON_W, ACTION_BUTTON_HEIGHT], egui::Button::new(label))
                        .clicked();
                });
            });
            clicked
        })
        .inner
}

/// The card's text column: version, home page, Ko-fi, copyright.
fn details(ui: &mut egui::Ui, palette: &Palette, version: &str) {
    egui::Grid::new("about_facts")
        .num_columns(2)
        .spacing([12.0, 2.0])
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Version:").font(fonts::bold(14.0)));
            ui.label(egui::RichText::new(version).font(fonts::regular(14.0)));
            ui.end_row();

            ui.label(egui::RichText::new("Homepage:").font(fonts::bold(14.0)));
            link_row(ui, HOME_URL);
            ui.end_row();

            ui.label(egui::RichText::new("Source Code:").font(fonts::bold(14.0)));
            link_row(ui, SOURCE_URL);
            ui.end_row();
        });

    ui.add_space(10.0);
    let kofi = icon_label(
        egui_phosphor::fill::HEART,
        fonts::icon_fill(16.0),
        "Support me on Ko-fi",
        fonts::regular(14.0),
        ui.visuals().widgets.inactive.fg_stroke.color,
    );
    // Sized, not natural-width: the button anchors the column's left edge with
    // the Version/URL rows above it, and a fixed box keeps it from breathing
    // as the label's glyph advance changes between themes.
    if ui
        .add_sized([BUTTON_W, ACTION_BUTTON_HEIGHT], egui::Button::new(kofi))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        ui.ctx().open_url(egui::OpenUrl::new_tab(KOFI_URL));
    }

    ui.add_space(12.0);
    ui.scope(|ui| {
        ui.visuals_mut().widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0, palette.panel_border);
        ui.separator();
    });
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(COPYRIGHT)
            .font(fonts::regular(12.0))
            .color(palette.subtle_text),
    );
}

/// One link line of the facts grid: an arrow-square-out glyph standing in for
/// the scheme, then the URL without its `https://` (or a trailing slash) —
/// the icon already says "this goes somewhere", so the protocol is noise.
fn link_row(ui: &mut egui::Ui, url: &str) {
    let display = url
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string();
    let color = ui.visuals().hyperlink_color;
    let label = icon_label(
        egui_phosphor::regular::ARROW_SQUARE_OUT,
        fonts::icon(14.0),
        &display,
        fonts::bold(14.0),
        color,
    );
    ui.hyperlink_to(label, url);
}

/// The card sits a step *below* the page, not level with it: darkened well off
/// `content_card_bg` so the provenance block reads as inset rather than as a
/// panel floating on a panel of nearly the same value.
fn card_fill(palette: &Palette) -> Color32 {
    let toward_black = if palette.light_mode { 0.10 } else { 0.45 };
    theme::tint(palette.content_card_bg, toward_black, Color32::BLACK)
}
