//! A small, reusable in-app confirmation dialog: a centered box over a dimmed,
//! input-swallowing backdrop with a message, an optional list of detail lines
//! (e.g. the file paths that would be overwritten), and Confirm / Cancel
//! buttons. Styled to match [`crate::status_modal`] so the app never has to
//! fall back to an out-of-place native Windows dialog.
//!
//! Like the status modal, this is hand-rolled from a foreground `Area` backdrop
//! plus a centered `Window` because egui 0.29 has no `egui::Modal`.

use eframe::egui;
use egui::{Align2, Color32, Order, RichText};

use crate::fonts;
use crate::stripes;
use crate::theme::Palette;

/// What the user did with the dialog this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmAction {
    None,
    Confirm,
    Cancel,
}

/// Everything the dialog needs to render one frame.
pub struct ConfirmView<'a> {
    pub title: &'a str,
    /// Main prompt shown under the title.
    pub message: &'a str,
    /// Optional monospaced detail lines (e.g. the paths at risk). Rendered in a
    /// height-capped scroll area so a many-disk overwrite can't grow the box
    /// past the window.
    pub details: &'a [String],
    pub confirm_label: &'a str,
    pub cancel_label: &'a str,
    /// Tint the confirm button with `palette.danger` (destructive) vs
    /// `palette.accent` (neutral).
    pub confirm_danger: bool,
    /// Paint hazard-tape strips across the top and bottom edges. Reserved for
    /// the confirmations that yank something out from under the user — the
    /// disk wipes, and closing the app with backups still mounted — so the
    /// tape keeps its meaning instead of becoming generic dialog chrome.
    pub hazard_tape: bool,
}

const DIALOG_WIDTH: f32 = 460.0;
const TAPE_HEIGHT: f32 = 32.0;
/// Horizontal padding around the dialog body (`Frame` margin normally, a
/// nested frame when the tape needs to bleed to the window edges).
const BODY_MARGIN: f32 = 22.0;

/// Diagonal yellow/black hazard stripes filling `rect`: stripes as wide as the
/// strip is tall, with a dark top fade for a bit of tape sheen.
///
/// Deliberately still yellow, and deliberately still standing still, while the
/// progress bar's barber pole (`status_modal`) shares the same geometry in the
/// accent color and turns. Yellow is what makes this read as *hazard* rather
/// than as chrome, and the tape marks a decision to stop and think about — the
/// last thing it should do is draw the eye with movement.
fn paint_tape(painter: &egui::Painter, rect: egui::Rect) {
    let yellow = Color32::from_rgb(0xF6, 0xC4, 0x00);
    let black = Color32::from_rgb(0x16, 0x16, 0x16);
    painter.rect_filled(rect, 0.0, black);
    stripes::paint(painter, rect, yellow, rect.height(), 0.0);
    stripes::sheen(painter, rect, 80);
}

/// Render the dialog and return the user's action for this frame. Escape maps
/// to Cancel; there is deliberately no Enter-to-confirm binding, so a
/// destructive action always needs an explicit click.
pub fn show(ctx: &egui::Context, palette: &Palette, view: &ConfirmView<'_>) -> ConfirmAction {
    // Full-screen dim backdrop that swallows clicks aimed at the UI behind it.
    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new("confirm_dialog_backdrop"))
        .order(Order::Foreground)
        .fixed_pos(screen.left_top())
        .interactable(true)
        .show(ctx, |ui| {
            let _ = ui.allocate_rect(screen, egui::Sense::click_and_drag());
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(160));
        });

    let mut action = ConfirmAction::None;
    ctx.input(|i| {
        if i.key_pressed(egui::Key::Escape) {
            action = ConfirmAction::Cancel;
        }
    });

    // With tape the strips must bleed to the window edges, so the margin moves
    // to a nested frame and the corners go square (stripes can't be clipped to
    // a rounded rect, and sharp corners suit the hazard look anyway).
    let frame = if view.hazard_tape {
        egui::Frame::window(&ctx.style())
            .fill(palette.sidebar_bg)
            .inner_margin(egui::Margin::ZERO)
            .rounding(egui::Rounding::ZERO)
    } else {
        egui::Frame::window(&ctx.style())
            .fill(palette.sidebar_bg)
            .inner_margin(egui::Margin::same(BODY_MARGIN))
            .rounding(egui::Rounding::same(10.0))
    };

    let body = |ui: &mut egui::Ui, action: &mut ConfirmAction| {
        ui.set_width(DIALOG_WIDTH);

        ui.label(RichText::new(view.title).font(fonts::bold(18.0)));
        ui.add_space(12.0);

        ui.label(RichText::new(view.message).color(palette.icon_color));

        if !view.details.is_empty() {
            ui.add_space(10.0);
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for line in view.details {
                        ui.label(
                            RichText::new(line)
                                .font(fonts::regular(13.0))
                                .color(palette.subtle_text),
                        );
                    }
                });
        }

        ui.add_space(20.0);
        ui.horizontal(|ui| {
            // Right-align the button pair: Cancel then Confirm, so the
            // destructive action sits at the far (default-reach) edge but
            // still needs a deliberate click.
            let button = egui::vec2(150.0, 38.0);
            let spacing = ui.spacing().item_spacing.x;
            let pair_width = button.x * 2.0 + spacing;
            let pad = (ui.available_width() - pair_width).max(0.0);
            ui.add_space(pad);

            let cancel = ui.add_sized(
                button,
                egui::Button::new(RichText::new(view.cancel_label).color(palette.icon_color))
                    .fill(ui.visuals().widgets.inactive.bg_fill),
            );
            if cancel.clicked() {
                *action = ConfirmAction::Cancel;
            }

            let confirm_fill = if view.confirm_danger {
                palette.danger
            } else {
                palette.accent
            };
            let confirm = ui.add_sized(
                button,
                egui::Button::new(RichText::new(view.confirm_label).color(Color32::WHITE))
                    .fill(confirm_fill),
            );
            if confirm.clicked() {
                *action = ConfirmAction::Confirm;
            }
        });
    };

    egui::Window::new("confirm_dialog")
        .order(Order::Tooltip)
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .movable(false)
        .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .frame(frame)
        .show(ctx, |ui| {
            if view.hazard_tape {
                let width = DIALOG_WIDTH + BODY_MARGIN * 2.0;
                ui.set_width(width);
                // The strips and the body frame butt up against each other;
                // the body's own margin provides all the breathing room.
                let body_spacing = ui.spacing().item_spacing;
                ui.spacing_mut().item_spacing.y = 0.0;

                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(width, TAPE_HEIGHT), egui::Sense::hover());
                paint_tape(ui.painter(), rect);

                egui::Frame::none()
                    .inner_margin(egui::Margin::same(BODY_MARGIN))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = body_spacing;
                        body(ui, &mut action);
                    });

                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(width, TAPE_HEIGHT), egui::Sense::hover());
                paint_tape(ui.painter(), rect);
            } else {
                body(ui, &mut action);
            }
        });

    action
}
