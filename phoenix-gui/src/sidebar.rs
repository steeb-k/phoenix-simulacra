use eframe::egui;
use egui::{Align2, Color32, Key, Margin, Rect, Response, Rounding, Sense, Stroke, Ui, Vec2};
use phoenix_core::appdata::ThemeChoice;

use crate::fonts;
use crate::theme::{self, Palette};

/// All top-level pages the app can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Backup,
    Clone,
    Restore,
    Verify,
    Mount,
    History,
    About,
}

struct NavItem {
    page: Page,
    label: &'static str,
    icon: &'static str,
    /// Alt+key accelerator. Must match the first letter of `label` — the
    /// underline hint drawn while Alt is held always sits under that letter.
    key: Key,
}

const TOP_ITEMS: &[NavItem] = &[
    NavItem {
        page: Page::Backup,
        label: "Backup",
        icon: egui_phosphor::regular::FLOPPY_DISK,
        key: Key::B,
    },
    NavItem {
        page: Page::Clone,
        label: "Clone",
        icon: egui_phosphor::regular::COPY_SIMPLE,
        key: Key::C,
    },
    NavItem {
        page: Page::Restore,
        label: "Restore",
        icon: egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE,
        key: Key::R,
    },
    NavItem {
        page: Page::Verify,
        label: "Verify",
        icon: egui_phosphor::regular::SHIELD_CHECK,
        key: Key::V,
    },
    NavItem {
        page: Page::Mount,
        label: "Mount",
        icon: egui_phosphor::regular::HARD_DRIVES,
        key: Key::M,
    },
];

const BOTTOM_ITEMS: &[NavItem] = &[
    NavItem {
        page: Page::History,
        label: "History",
        icon: egui_phosphor::regular::CLOCK_COUNTER_CLOCKWISE,
        key: Key::H,
    },
    NavItem {
        page: Page::About,
        label: "About",
        icon: egui_phosphor::regular::INFO,
        key: Key::A,
    },
];

const SIDEBAR_WIDTH: f32 = 220.0;
const ROW_HEIGHT: f32 = 40.0;
/// Display width of the sidebar phoenix image. Source `phoenix-sidebar.png` is
/// 1024×976, so the height follows via [`LOGO_ASPECT`] rather than forcing a
/// square (which would squash the art).
const LOGO_WIDTH: f32 = 192.0;
/// height / width of `phoenix-sidebar.png` (1024×976).
const LOGO_ASPECT: f32 = 976.0 / 1024.0;
/// Gap between the phoenix image and the "Simulacra" wordmark below it.
const BRAND_WORDMARK_GAP: f32 = 6.0;
/// Approximate rendered height of the fitted "Simulacra" wordmark. Used only
/// to size the OS window's minimum height (see [`min_content_height`]); the
/// brand panel itself sizes to the real glyph height, so this only needs to be
/// close.
const WORDMARK_HEIGHT_EST: f32 = 48.0;
const SIDEBAR_H_PAD: f32 = 12.0;
const BRAND_TOP_PAD: f32 = 18.0;
const BRAND_BOTTOM_PAD: f32 = 18.0;
/// Trailing space inside `draw_brand` between the wordmark and the panel edge.
const BRAND_IMAGE_TRAILER: f32 = 6.0;
const BOTTOM_NAV_TOP_PAD: f32 = 8.0;
const BOTTOM_NAV_BOTTOM_PAD: f32 = 18.0;
/// Height of the segmented System/Dark/Light pill pinned under the bottom nav.
const THEME_PILL_HEIGHT: f32 = 28.0;
/// Gap between the last nav row and the theme pill.
const THEME_PILL_TOP_PAD: f32 = 10.0;
/// How many scrollable nav items must remain visible at the smallest allowed
/// window height. 1.5 = one full item plus a half-item "more below" hint.
const MIN_VISIBLE_TOP_ITEMS: f32 = 1.5;

/// Smallest vertical space the sidebar needs to render without clipping the
/// fixed brand and bottom-nav blocks while still showing
/// [`MIN_VISIBLE_TOP_ITEMS`] of the scrollable middle section. The main app
/// uses this to set the OS window's `min_inner_size` so the sidebar layout
/// can't collapse below "logo + 1.5-item scroll hint + History/About + theme
/// pill".
pub fn min_content_height() -> f32 {
    let brand = BRAND_TOP_PAD
        + LOGO_WIDTH * LOGO_ASPECT
        + BRAND_WORDMARK_GAP
        + WORDMARK_HEIGHT_EST
        + BRAND_IMAGE_TRAILER
        + BRAND_BOTTOM_PAD;
    let bottom = BOTTOM_NAV_TOP_PAD
        + (BOTTOM_ITEMS.len() as f32) * ROW_HEIGHT
        + THEME_PILL_TOP_PAD
        + THEME_PILL_HEIGHT
        + BOTTOM_NAV_BOTTOM_PAD;
    let middle = ROW_HEIGHT * MIN_VISIBLE_TOP_ITEMS;
    brand + bottom + middle
}

/// What one `show` call produced.
pub struct SidebarOutput {
    /// The brand block's rect — the chromeless window uses it as a drag
    /// handle (see `titlebar::show`).
    pub brand_rect: Rect,
    /// The theme pill was clicked and `theme` now holds a new choice: the
    /// caller re-applies the palette and persists the setting.
    pub theme_changed: bool,
}

/// Render the left sidebar. `current` is updated when the user clicks a nav
/// item, `theme` when the user picks a segment of the theme pill. While `busy`
/// is true, items remain visible but click-through is disabled while the status
/// modal is up so navigation can't interrupt a job.
///
/// Layout is three vertically-stacked regions:
///   * fixed top — brand/logo;
///   * scrollable middle — the primary nav (Backup, Clone, Restore, Verify,
///     Mount). Scrolls when the window is short;
///   * fixed bottom — History and About plus the theme pill, always pinned in
///     view so they don't disappear as the user shrinks the window. The theme
///     lives here rather than on a settings page because it's the one
///     preference people flip on a whim, and it wants to be one click away
///     from wherever they are.
pub fn show(
    ctx: &egui::Context,
    current: &mut Page,
    palette: &Palette,
    busy: bool,
    theme: &mut ThemeChoice,
) -> SidebarOutput {
    // Alt+<first letter> jumps straight to a page. Consumed here (before any
    // widget sees the key) but only while navigation is allowed, mirroring
    // the click gating below. `consume_key` requires Alt to be the only
    // modifier held, so Ctrl+Alt combos (e.g. AltGr input) pass through.
    if !busy {
        for item in TOP_ITEMS.iter().chain(BOTTOM_ITEMS) {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, item.key)) {
                *current = item.page;
            }
        }
    }

    egui::SidePanel::left("sidebar")
        .resizable(false)
        .show_separator_line(false)
        .exact_width(SIDEBAR_WIDTH)
        .frame(
            egui::Frame::none()
                .fill(palette.sidebar_bg)
                .inner_margin(Margin::symmetric(SIDEBAR_H_PAD, 0.0)),
        )
        .show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                // Fixed brand block at the top. Its rect is bubbled back out
                // as the window's drag handle.
                let brand_rect = egui::TopBottomPanel::top("sidebar_brand")
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(egui::Frame::none().inner_margin(Margin {
                        left: 0.0,
                        right: 0.0,
                        top: BRAND_TOP_PAD,
                        bottom: BRAND_BOTTOM_PAD,
                    }))
                    .show_inside(ui, |ui| {
                        draw_brand(ui, palette);
                    })
                    .response
                    .rect;

                // Fixed bottom nav (History + About + theme pill) — pinned,
                // always visible. Declared before the central panel so the
                // remaining space is correctly handed to the scrollable
                // middle.
                let theme_changed = egui::TopBottomPanel::bottom("sidebar_bottom_nav")
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(egui::Frame::none().inner_margin(Margin {
                        left: 0.0,
                        right: 0.0,
                        top: BOTTOM_NAV_TOP_PAD,
                        bottom: BOTTOM_NAV_BOTTOM_PAD,
                    }))
                    .show_inside(ui, |ui| {
                        for item in BOTTOM_ITEMS {
                            nav_row(ui, item, current, palette);
                        }
                        ui.add_space(THEME_PILL_TOP_PAD);
                        theme_pill(ui, theme, palette)
                    })
                    .inner;

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

                SidebarOutput {
                    brand_rect,
                    theme_changed,
                }
            })
            .inner
        })
        .inner
}

/// How long the accent thumb takes to slide between segments.
const THEME_PILL_SLIDE: f32 = 0.14;

/// The segmented System/Dark/Light control pinned under the bottom nav.
/// Painted by hand rather than built from `selectable_label`s so the three
/// segments read as one pill: a single rounded well with one accent-colored
/// thumb that *slides* to the segment you pick rather than blinking there.
/// Returns whether `theme` changed.
fn theme_pill(ui: &mut Ui, theme: &mut ThemeChoice, palette: &Palette) -> bool {
    const CHOICES: [(ThemeChoice, &str); 3] = [
        (ThemeChoice::System, "System"),
        (ThemeChoice::Dark, "Dark"),
        (ThemeChoice::Light, "Light"),
    ];

    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), THEME_PILL_HEIGHT),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    let radius = rect.height() / 2.0;
    painter.rect_filled(rect, Rounding::same(radius), palette.sidebar_hover_bg);

    let seg_width = rect.width() / CHOICES.len() as f32;
    let seg_rect = |i: f32| {
        Rect::from_min_size(
            rect.left_top() + Vec2::new(i * seg_width, 0.0),
            Vec2::new(seg_width, rect.height()),
        )
    };
    let selected_index = CHOICES
        .iter()
        .position(|(choice, _)| choice == theme)
        .unwrap_or(0) as f32;

    // The thumb's position is animated, not snapped: egui eases the stored
    // value toward the selected index and asks for repaints until it lands.
    // Everything else (hover tints, label weight) still keys off the real
    // selection, so only the accent block is in motion.
    let thumb_at = ui.ctx().animate_value_with_time(
        ui.id().with("theme_thumb"),
        selected_index,
        THEME_PILL_SLIDE,
    );
    // Inset from the well so a sliver of track still shows around the thumb —
    // that ring is what makes the pill read as a track with something moving
    // along it rather than three abutting buttons.
    painter.rect_filled(
        seg_rect(thumb_at).shrink(2.0),
        Rounding::same(radius - 2.0),
        palette.accent,
    );

    let mut changed = false;
    for (i, (choice, label)) in CHOICES.iter().enumerate() {
        let seg = seg_rect(i as f32);
        let response = ui.interact(seg, ui.id().with(("theme_pill", i)), Sense::click());
        let selected = *theme == *choice;

        if !selected && response.hovered() {
            painter.rect_filled(
                seg.shrink(2.0),
                Rounding::same(radius - 2.0),
                palette.sidebar_hover_bg,
            );
        }

        // Fade each label between its on-accent and off-accent color by how
        // much of the thumb currently covers it, so a label never sits in the
        // wrong color while the thumb is passing over it.
        let covered = (1.0 - (thumb_at - i as f32).abs()).clamp(0.0, 1.0);
        let text_color = theme::tint(
            palette.icon_color.gamma_multiply(0.85),
            covered,
            theme::contrast_text(palette.accent),
        );
        painter.text(
            seg.center(),
            Align2::CENTER_CENTER,
            label,
            if selected {
                fonts::bold(12.0)
            } else {
                fonts::regular(12.0)
            },
            text_color,
        );

        if response.clicked() && !selected {
            *theme = *choice;
            changed = true;
        }
    }
    changed
}

fn draw_brand(ui: &mut Ui, palette: &Palette) {
    ui.vertical_centered(|ui| {
        ui.add(
            egui::Image::new(egui::include_image!("../../assets/phoenix-sidebar.png"))
                .fit_to_exact_size(Vec2::new(LOGO_WIDTH, LOGO_WIDTH * LOGO_ASPECT)),
        );
        ui.add_space(BRAND_WORDMARK_GAP);
        draw_wordmark(ui, palette);
        ui.add_space(BRAND_IMAGE_TRAILER);
    });
}

/// Paint the "Simulacra" wordmark in Inter ExtraBold, scaled so the word spans
/// the full sidebar content width.
fn draw_wordmark(ui: &mut Ui, palette: &Palette) {
    const WORD: &str = "Simulacra";
    let target_w = ui.available_width();

    // epaint lays text out by summing per-glyph advances (no kerning/GPOS), so
    // the advance sum at a reference size gives an exact scale factor to hit
    // the target width — no iterative fitting needed.
    const REF_SIZE: f32 = 100.0;
    let ref_font = fonts::extrabold(REF_SIZE);
    let ref_w: f32 = ui.fonts(|f| WORD.chars().map(|c| f.glyph_width(&ref_font, c)).sum());
    let size = if ref_w > 0.0 {
        REF_SIZE * target_w / ref_w
    } else {
        REF_SIZE
    };

    let font = fonts::extrabold(size);
    let row_h = ui.fonts(|f| f.row_height(&font));
    let (rect, _) = ui.allocate_exact_size(Vec2::new(target_w, row_h), Sense::hover());
    ui.painter().text(
        rect.center_top(),
        Align2::CENTER_TOP,
        WORD,
        font,
        palette.icon_color,
    );
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

    let label_font = fonts::regular(14.0);
    let text_pos = rect.left_center() + Vec2::new(42.0, 0.0);
    let text_rect = painter.text(
        text_pos,
        Align2::LEFT_CENTER,
        item.label,
        label_font.clone(),
        text_color,
    );

    // While Alt is held (and navigation is allowed), underline the first
    // letter as the accelerator hint, Windows-menu style.
    let alt_held = ui.is_enabled() && ui.input(|i| i.modifiers.alt);
    if alt_held {
        let first = item.label.chars().next().unwrap_or('?');
        let width = ui.fonts(|f| f.glyph_width(&label_font, first));
        let y = text_rect.bottom() - 1.0;
        painter.line_segment(
            [
                egui::pos2(text_rect.left(), y),
                egui::pos2(text_rect.left() + width, y),
            ],
            Stroke::new(1.0, text_color),
        );
    }

    if response.clicked() {
        *current = item.page;
    }

    response
}
