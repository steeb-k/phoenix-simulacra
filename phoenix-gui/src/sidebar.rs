use eframe::egui;
use egui::{
    Align, Align2, Color32, FontId, Layout, Rect, Response, Rounding, Sense, Stroke, Ui, Vec2,
};

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
const LOGO_SIZE: f32 = 96.0;

/// Render the left sidebar. `current` is updated when the user clicks a nav
/// item. While `busy` is true, items remain visible but click-through is
/// disabled so an in-flight backup/restore can't be interrupted by navigation.
pub fn show(ctx: &egui::Context, current: &mut Page, palette: &Palette, busy: bool) {
    egui::SidePanel::left("sidebar")
        .resizable(false)
        .exact_width(SIDEBAR_WIDTH)
        .frame(
            egui::Frame::none()
                .fill(palette.sidebar_bg)
                .inner_margin(egui::Margin::symmetric(12.0, 18.0)),
        )
        .show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                draw_brand(ui, palette);
                ui.add_space(18.0);

                for item in TOP_ITEMS {
                    nav_row(ui, item, current, palette);
                }

                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    for item in BOTTOM_ITEMS.iter().rev() {
                        nav_row(ui, item, current, palette);
                    }
                });
            });
        });
}

fn draw_brand(ui: &mut Ui, palette: &Palette) {
    ui.vertical_centered(|ui| {
        ui.add(
            egui::Image::new(egui::include_image!("../../carbon-phoenix.png"))
                .fit_to_exact_size(Vec2::splat(LOGO_SIZE)),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Carbon Phoenix")
                .strong()
                .size(16.0)
                .color(if palette.light_mode {
                    Color32::from_rgb(0x20, 0x20, 0x20)
                } else {
                    Color32::WHITE
                }),
        );
    });
}

fn nav_row(ui: &mut Ui, item: &NavItem, current: &mut Page, palette: &Palette) -> Response {
    let selected = *current == item.page;
    let desired = Vec2::new(ui.available_width(), ROW_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(desired, Sense::click());

    let hovered = response.hovered();
    let painter = ui.painter_at(rect);

    let bg = if selected {
        palette.sidebar_selected_bg
    } else if hovered {
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

    let icon_color = if selected {
        palette.accent
    } else if hovered {
        palette.icon_color
    } else {
        palette.icon_color.gamma_multiply(0.85)
    };
    let text_color = if selected {
        // Strong contrast for the selected label.
        if palette.light_mode {
            Color32::from_rgb(0x10, 0x10, 0x10)
        } else {
            Color32::WHITE
        }
    } else {
        palette.icon_color
    };

    let icon_pos = rect.left_center() + Vec2::new(18.0, 0.0);
    painter.text(
        icon_pos,
        Align2::CENTER_CENTER,
        item.icon,
        FontId::proportional(20.0),
        icon_color,
    );

    let text_pos = rect.left_center() + Vec2::new(42.0, 0.0);
    painter.text(
        text_pos,
        Align2::LEFT_CENTER,
        item.label,
        FontId::proportional(14.0),
        text_color,
    );

    if response.clicked() {
        *current = item.page;
    }

    // A faint border around the selected row helps in light mode where the
    // tinted fill alone can read as a tooltip.
    if selected && palette.light_mode {
        painter.rect_stroke(
            rect,
            Rounding::same(6.0),
            Stroke::new(1.0, palette.accent.gamma_multiply(0.4)),
        );
    }

    response
}
