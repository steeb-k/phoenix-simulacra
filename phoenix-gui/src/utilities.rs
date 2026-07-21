//! The **Utilities** page and its guided-tool modals.
//!
//! Utilities are one-off maintenance tools for an already-repaired or cloned
//! drive — the things that don't fit the app's primary "pick a disk, run a
//! job" pages because they aren't a single staged operation. The page itself is
//! just a menu: a stack of full-width [`utility_card`]s, each an icon + title +
//! one-liner that reads and clicks as a single button. Clicking one opens a
//! compact [`modal_shell`] that walks the user through that tool, then hands off
//! to the app's existing hazard-tape confirm dialog and status modal for the
//! actual (destructive) work.
//!
//! ## Adding a new utility
//!
//! 1. Add a variant to [`Utility`].
//! 2. In `PhoenixApp::ui_utilities` (main.rs) add a [`utility_card`]; set
//!    `self.active_utility = Some(Utility::Yours)` when it returns `true`.
//! 3. Write a `fn your_util_modal(&mut self, ctx)` that early-returns unless
//!    `self.active_utility == Some(Utility::Yours)`, builds its body inside
//!    [`modal_shell`], and on [`ModalAction::Primary`] parks a `pending_*`
//!    behind a confirm dialog (mirror `boot_repair_modal` / `reset_hello_modal`).
//! 4. Dispatch it from `show_utility_modals`, and add any new `pending_*`/
//!    `active_utility` state to `modal_open()` so the page behind goes inert.
//!
//! The Windows-install picker ([`crate::bootrepair_table`]) and the shared
//! `windows_installs` scan are reusable by any utility that targets a Windows
//! installation.

use egui::{Align2, Color32, CursorIcon, Key, Margin, Order, Pos2, Rounding, Sense, Stroke, Vec2};

use crate::disk_map::with_alpha;
use crate::fonts;
use crate::theme::Palette;

/// Which utility's modal is currently open. `None` means the plain menu page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Utility {
    BootRepair,
    ResetHello,
    EnableAdmin,
}

/// Height of one utility card on the menu page.
const CARD_H: f32 = 66.0;
/// Left inset of the card icon, and the icon's slot width.
const CARD_ICON_X: f32 = 16.0;
const CARD_ICON_W: f32 = 30.0;
/// Gap from the icon slot to the text column.
const CARD_TEXT_GAP: f32 = 16.0;
/// Right inset of the trailing chevron.
const CARD_CHEVRON_X: f32 = 18.0;

/// One full-width clickable utility card: `icon` on the left, bold `title` with
/// a `desc` line under it, and a trailing chevron. Hovering lifts the fill and
/// shows the hand cursor; the whole rect is the button. Returns whether it was
/// clicked this frame.
///
/// Painted by hand (like the sidebar nav rows) rather than composed from
/// widgets so the icon, two text lines, and chevron sit at exact positions
/// regardless of the layout cursor, and the hover fill covers the whole card.
pub fn utility_card(
    ui: &mut egui::Ui,
    palette: &Palette,
    icon: &str,
    title: &str,
    desc: &str,
) -> bool {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, CARD_H), Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    // Unclipped painter: clipping to `rect` (as `painter_at` does) shaves the
    // outer half of the border stroke off, so the ring rendered broken on some
    // edges. The stroke is inset instead so the whole ring sits inside `rect`.
    let painter = ui.painter().clone();
    painter.rect_filled(rect, Rounding::same(8.0), palette.content_card_bg);
    // Hover lift. The faint icon-tone wash the tables use is nearly invisible on
    // a dark card, so hover does two clearly-visible things at once: a stronger
    // fill lift and an accent-colored border (which reads in both themes).
    let border = if hovered {
        painter.rect_filled(rect, Rounding::same(8.0), with_alpha(palette.icon_color, 34));
        Stroke::new(1.5, palette.accent)
    } else {
        Stroke::new(1.0, palette.panel_border)
    };
    painter.rect_stroke(rect.shrink(1.0), Rounding::same(7.0), border);

    // Icon, centered in its left slot.
    painter.text(
        Pos2::new(rect.left() + CARD_ICON_X + CARD_ICON_W / 2.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        fonts::icon(28.0),
        palette.accent,
    );

    // Two stacked text lines, left-aligned in the text column. The column runs
    // to just short of the chevron; the description elides rather than run
    // under it.
    let text_x = rect.left() + CARD_ICON_X + CARD_ICON_W + CARD_TEXT_GAP;
    let text_right = rect.right() - CARD_CHEVRON_X - 14.0;
    painter.text(
        Pos2::new(text_x, rect.center().y - 10.0),
        Align2::LEFT_CENTER,
        title,
        fonts::bold(15.0),
        palette.icon_color,
    );
    let mut job = egui::text::LayoutJob::default();
    job.append(
        desc,
        0.0,
        egui::TextFormat {
            font_id: fonts::regular(12.5),
            color: palette.subtle_text,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: (text_right - text_x).max(0.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = ui.fonts(|f| f.layout_job(job));
    painter.galley(
        Pos2::new(text_x, rect.center().y + 11.0 - galley.size().y * 0.5),
        galley,
        palette.subtle_text,
    );

    // Trailing chevron: this card leads somewhere (a modal), the same cue a
    // disclosure row carries.
    painter.text(
        Pos2::new(rect.right() - CARD_CHEVRON_X, rect.center().y),
        Align2::CENTER_CENTER,
        egui_phosphor::regular::CARET_RIGHT,
        fonts::icon(16.0),
        palette.subtle_text,
    );

    response.clicked()
}

/// What the user did with a utility modal this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalAction {
    None,
    Cancel,
    /// The primary (right-hand) button was clicked. Only reachable when the
    /// modal passed `primary_enabled: true`.
    Primary,
}

/// A compact single-panel modal shell for a utility: a dim, click-swallowing
/// backdrop and a centered card with `title`, the caller's `body`, and a
/// Cancel / primary button footer. Styled to match [`crate::confirm_dialog`]
/// (same window frame, backdrop, Escape-to-cancel) so the guided tools feel
/// like the rest of the app's dialogs — no hazard tape here, since the tape
/// belongs to the destructive confirm step this hands off to.
///
/// `content_width` fixes the body width so a reused fixed-width picker (the
/// installs table) lays out predictably. Returns the action for this frame;
/// [`ModalAction::None`] means "still open, nothing chosen".
pub fn modal_shell(
    ctx: &egui::Context,
    palette: &Palette,
    title: &str,
    primary_label: &str,
    primary_enabled: bool,
    content_width: f32,
    body: impl FnOnce(&mut egui::Ui),
) -> ModalAction {
    // Full-screen dim backdrop that swallows clicks aimed at the page behind.
    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new("utility_modal_backdrop"))
        .order(Order::Foreground)
        .fixed_pos(screen.left_top())
        .interactable(true)
        .show(ctx, |ui| {
            let _ = ui.allocate_rect(screen, Sense::click_and_drag());
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(160));
        });

    let mut action = ModalAction::None;
    ctx.input(|i| {
        if i.key_pressed(Key::Escape) {
            action = ModalAction::Cancel;
        }
    });

    let frame = egui::Frame::window(&ctx.style())
        .fill(palette.sidebar_bg)
        .inner_margin(Margin::same(22.0))
        .rounding(Rounding::same(10.0));

    egui::Window::new("utility_modal")
        .order(Order::Tooltip)
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .movable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(content_width);
            ui.label(egui::RichText::new(title).font(fonts::bold(18.0)));
            ui.add_space(12.0);

            body(ui);

            ui.add_space(20.0);
            // Cancel + primary, right-aligned as a pair — the same button
            // geometry and placement as the confirm dialog.
            ui.horizontal(|ui| {
                let button = Vec2::new(150.0, 38.0);
                let spacing = ui.spacing().item_spacing.x;
                let pair = button.x * 2.0 + spacing;
                let pad = (ui.available_width() - pair).max(0.0);
                ui.add_space(pad);

                let cancel = ui.add_sized(
                    button,
                    egui::Button::new(
                        egui::RichText::new("Cancel").color(palette.icon_color),
                    )
                    .fill(ui.visuals().widgets.inactive.bg_fill),
                );
                if cancel.clicked() {
                    action = ModalAction::Cancel;
                }

                let primary = ui.add_enabled(
                    primary_enabled,
                    egui::Button::new(
                        egui::RichText::new(primary_label).color(Color32::WHITE),
                    )
                    .fill(palette.accent)
                    .min_size(button),
                );
                if primary.clicked() {
                    action = ModalAction::Primary;
                }
            });
        });

    action
}
