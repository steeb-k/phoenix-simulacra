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

/// Where to send people who want to see what else this author makes.
const HOME_URL: &str = "https://kznjk.com/";
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
            // would only be as wide as the icon plus the longest line of text.
            // `available_width` here is already inside the inner margin.
            ui.set_width(ui.available_width());
            let inner = ui.max_rect();

            ui.horizontal_top(|ui| {
                ui.add(
                    egui::Image::new(egui::include_image!(
                        "../../assets/phoenix-appicon-256px.png"
                    ))
                    .fit_to_exact_size(Vec2::splat(ICON_SIZE)),
                );
                ui.add_space(ICON_TEXT_GAP);
                ui.vertical(|ui| details(ui, palette, version));
            });

            check_for_updates_button(ui, inner)
        })
        .inner
}

/// The card's top-right corner. Lives *inside* the card rather than beside the
/// page title — the chromeless window floats its Refresh pill over the top-right
/// of the pane, and a header-level button there sits under it.
///
/// Placed at an explicit rect rather than laid out as a column of the card, so
/// it floats over the corner without reserving width that the icon and the text
/// beside it would then have to squeeze into. Nothing in the card's content
/// reaches that corner, so there is nothing for it to overlap.
fn check_for_updates_button(ui: &mut egui::Ui, card: egui::Rect) -> bool {
    let label = icon_label(
        egui_phosphor::regular::CLOUD_ARROW_DOWN,
        fonts::icon(16.0),
        "Check for updates",
        fonts::regular(14.0),
        ui.visuals().widgets.inactive.fg_stroke.color,
    );
    let rect = egui::Rect::from_min_size(
        egui::pos2(card.right() - BUTTON_W, card.top()),
        Vec2::new(BUTTON_W, ACTION_BUTTON_HEIGHT),
    );
    ui.put(rect, egui::Button::new(label)).clicked()
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

            ui.label(egui::RichText::new("URL:").font(fonts::bold(14.0)));
            ui.hyperlink_to(
                egui::RichText::new(HOME_URL).font(fonts::bold(14.0)),
                HOME_URL,
            );
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

/// The card sits a step *below* the page, not level with it: darkened well off
/// `content_card_bg` so the provenance block reads as inset rather than as a
/// panel floating on a panel of nearly the same value.
fn card_fill(palette: &Palette) -> Color32 {
    let toward_black = if palette.light_mode { 0.10 } else { 0.45 };
    theme::tint(palette.content_card_bg, toward_black, Color32::BLACK)
}
