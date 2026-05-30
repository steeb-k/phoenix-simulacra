use eframe::egui;
use egui::{Align2, Color32, Margin, Rect, Response, Rounding, Sense, Ui, Vec2};

use crate::fonts;
use crate::theme::Palette;

/// All top-level pages the app can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Backup,
    Clone,
    Restore,
    Verify,
    Mount,
    History,
    Options,
}

struct NavItem {
    page: Page,
    label: &'static str,
    icon: &'static str,
}

const TOP_ITEMS: &[NavItem] = &[
    NavItem {
        page: Page::Backup,
        label: "Backup",
        icon: egui_phosphor::regular::FLOPPY_DISK,
    },
    NavItem {
        page: Page::Clone,
        label: "Clone",
        icon: egui_phosphor::regular::COPY_SIMPLE,
    },
    NavItem {
        page: Page::Restore,
        label: "Restore",
        icon: egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE,
    },
    NavItem {
        page: Page::Verify,
        label: "Verify",
        icon: egui_phosphor::regular::SHIELD_CHECK,
    },
    NavItem {
        page: Page::Mount,
        label: "Mount",
        icon: egui_phosphor::regular::HARD_DRIVES,
    },
];

const BOTTOM_ITEMS: &[NavItem] = &[
    NavItem {
        page: Page::History,
        label: "History",
        icon: egui_phosphor::regular::CLOCK_COUNTER_CLOCKWISE,
    },
    NavItem {
        page: Page::Options,
        label: "Options",
        icon: egui_phosphor::regular::GEAR,
    },
];

const SIDEBAR_WIDTH: f32 = 220.0;
const ROW_HEIGHT: f32 = 40.0;
const LOGO_SIZE: f32 = 192.0; // display size; source is 256×256 carbon-phoenix-sidebar.png
const SIDEBAR_H_PAD: f32 = 12.0;
const BRAND_TOP_PAD: f32 = 18.0;
const BRAND_BOTTOM_PAD: f32 = 18.0;
/// Trailing space inside `draw_brand` between the image and the panel edge.
const BRAND_IMAGE_TRAILER: f32 = 6.0;
const BOTTOM_NAV_TOP_PAD: f32 = 8.0;
const BOTTOM_NAV_BOTTOM_PAD: f32 = 18.0;
/// How many scrollable nav items must remain visible at the smallest allowed
/// window height. 1.5 = one full item plus a half-item "more below" hint.
const MIN_VISIBLE_TOP_ITEMS: f32 = 1.5;

/// Smallest vertical space the sidebar needs to render without clipping the
/// fixed brand and bottom-nav blocks while still showing
/// [`MIN_VISIBLE_TOP_ITEMS`] of the scrollable middle section. The main app
/// uses this to set the OS window's `min_inner_size` so the sidebar layout
/// can't collapse below "logo + 1.5-item scroll hint + History/Options".
pub fn min_content_height() -> f32 {
    let brand = BRAND_TOP_PAD + LOGO_SIZE + BRAND_IMAGE_TRAILER + BRAND_BOTTOM_PAD;
    let bottom = BOTTOM_NAV_TOP_PAD
        + (BOTTOM_ITEMS.len() as f32) * ROW_HEIGHT
        + BOTTOM_NAV_BOTTOM_PAD;
    let middle = ROW_HEIGHT * MIN_VISIBLE_TOP_ITEMS;
    brand + bottom + middle
}

/// Render the left sidebar. `current` is updated when the user clicks a nav
/// item. While `busy` is true, items remain visible but click-through is
/// disabled while the status modal is up so navigation can't interrupt a job.
///
/// Layout is three vertically-stacked regions:
///   * fixed top — brand/logo;
///   * scrollable middle — the primary nav (Backup, Clone, Restore, Verify,
///     Mount). Scrolls when the window is short;
///   * fixed bottom — History and Options, always pinned in view so they
///     don't disappear as the user shrinks the window.
pub fn show(ctx: &egui::Context, current: &mut Page, palette: &Palette, busy: bool) {
    egui::SidePanel::left("sidebar")
        .resizable(false)
        .exact_width(SIDEBAR_WIDTH)
        .frame(
            egui::Frame::none()
                .fill(palette.sidebar_bg)
                .inner_margin(Margin::symmetric(SIDEBAR_H_PAD, 0.0)),
        )
        .show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                // Fixed brand block at the top.
                egui::TopBottomPanel::top("sidebar_brand")
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(
                        egui::Frame::none().inner_margin(Margin {
                            left: 0.0,
                            right: 0.0,
                            top: BRAND_TOP_PAD,
                            bottom: BRAND_BOTTOM_PAD,
                        }),
                    )
                    .show_inside(ui, |ui| {
                        draw_brand(ui);
                    });

                // Fixed bottom nav (History + Options) — pinned, always
                // visible. Declared before the central panel so the
                // remaining space is correctly handed to the scrollable
                // middle.
                egui::TopBottomPanel::bottom("sidebar_bottom_nav")
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(
                        egui::Frame::none().inner_margin(Margin {
                            left: 0.0,
                            right: 0.0,
                            top: BOTTOM_NAV_TOP_PAD,
                            bottom: BOTTOM_NAV_BOTTOM_PAD,
                        }),
                    )
                    .show_inside(ui, |ui| {
                        for item in BOTTOM_ITEMS {
                            nav_row(ui, item, current, palette);
                        }
                    });

                // Scrollable middle: primary nav. Fills whatever vertical
                // room is left between the brand and bottom nav.
                egui::CentralPanel::default()
                    .frame(egui::Frame::none())
                    .show_inside(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("sidebar_top_nav")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for item in TOP_ITEMS {
                                    nav_row(ui, item, current, palette);
                                }
                            });
                    });
            });
        });
}

fn draw_brand(ui: &mut Ui) {
    ui.vertical_centered(|ui| {
        ui.add(
            egui::Image::new(egui::include_image!("../../carbon-phoenix-sidebar.png"))
                .fit_to_exact_size(Vec2::splat(LOGO_SIZE)),
        );
        ui.add_space(BRAND_IMAGE_TRAILER);
    });
}

fn nav_row(ui: &mut Ui, item: &NavItem, current: &mut Page, palette: &Palette) -> Response {
    let selected = *current == item.page;
    let desired = Vec2::new(ui.available_width(), ROW_HEIGHT);
    // `Sense::click()` would also set `focusable: true`, which would put
    // every sidebar nav row into the global Tab cycle ahead of the main
    // pane controls. We want sidebar items to be clickable but skipped by
    // keyboard navigation so Tab lands in the central panel first.
    let sense = Sense {
        click: true,
        drag: false,
        focusable: false,
    };
    let (rect, response) = ui.allocate_exact_size(desired, sense);

    let hovered = response.hovered();
    let painter = ui.painter_at(rect);

    // Selected and hovered rows share the same fill; the accent dot on the
    // left is the only visual cue for the selected state.
    let bg = if selected || hovered {
        palette.sidebar_hover_bg
    } else {
        Color32::TRANSPARENT
    };
    if bg.a() > 0 {
        painter.rect_filled(rect, Rounding::same(6.0), bg);
    }

    if selected {
        let bar = Rect::from_min_max(
            rect.left_top() + Vec2::new(2.0, 8.0),
            rect.left_bottom() + Vec2::new(5.0, -8.0),
        );
        painter.rect_filled(bar, Rounding::same(2.0), palette.accent);
    }

    let text_color = if selected {
        // Strong contrast for the selected label.
        if palette.light_mode {
            Color32::from_rgb(0x10, 0x10, 0x10)
        } else {
            Color32::WHITE
        }
    } else if hovered {
        palette.icon_color
    } else {
        palette.icon_color.gamma_multiply(0.85)
    };
    let icon_color = text_color;

    let icon_pos = rect.left_center() + Vec2::new(18.0, 0.0);
    painter.text(
        icon_pos,
        Align2::CENTER_CENTER,
        item.icon,
        fonts::icon(20.0),
        icon_color,
    );

    let text_pos = rect.left_center() + Vec2::new(42.0, 0.0);
    painter.text(
        text_pos,
        Align2::LEFT_CENTER,
        item.label,
        fonts::regular(14.0),
        text_color,
    );

    if response.clicked() {
        *current = item.page;
    }

    response
}
