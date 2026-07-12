//! Blocking status modal: a centered dialog over a dimmed, input-swallowing
//! backdrop that renders the operation's predeclared step checklist, a
//! progress bar, and a Cancel button that becomes a colored Close button
//! once the job ends.
//!
//! egui 0.29 has no `egui::Modal` (added in 0.30), so we hand-roll it from a
//! foreground `Area` backdrop plus a centered `Window`. This is structured so
//! it can be swapped for `egui::Modal` if/when we upgrade egui.

use eframe::egui;
use egui::{Align2, Color32, Order, RichText};

use crate::fonts;
use crate::stripes;
use crate::theme::{self, Palette};
use crate::util::format_bytes;

/// How a finished job ended, used to color the Close button and the final
/// progress bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobOutcome {
    Success,
    Warning,
    Failure,
}

/// Snapshot of a finished job, kept around so the modal stays up (showing the
/// final step list + a Close button) until the user dismisses it.
pub struct CompletedJob {
    pub title: String,
    pub steps: Vec<String>,
    pub current_step: usize,
    pub outcome: JobOutcome,
    pub message: String,
    /// When set (successful backups: "Completed and verified." /
    /// "Completed. Unverified."; successful standalone verifies: "Backup
    /// verified."), the finished modal replaces the progress bar and step
    /// checklist with a big green checkmark over this text.
    pub success_banner: Option<String>,
    /// The `.phnx` file this (successful, unverified, single-disk) backup
    /// wrote. When set, the finished modal offers a "Verify?" button that
    /// runs a full image verify of the file after the fact.
    pub verify_target: Option<std::path::PathBuf>,
}

/// What the user did with the modal this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalAction {
    None,
    Cancel,
    Close,
    /// "Verify?" on an unverified completed backup: run a full image verify
    /// of [`CompletedJob::verify_target`].
    Verify,
}

/// Everything the modal needs to render one frame.
pub struct ModalView<'a> {
    pub title: &'a str,
    pub steps: &'a [String],
    pub current_step: usize,
    /// Sub-message under the checklist (live detail while running, or the
    /// final result message when finished).
    pub detail: &'a str,
    pub fraction: f32,
    pub current_bytes: u64,
    pub total_bytes: u64,
    /// `None` while running, `Some(outcome)` once the job has finished.
    pub outcome: Option<JobOutcome>,
    /// See [`CompletedJob::success_banner`]. Only honored together with
    /// `Some(JobOutcome::Success)`.
    pub success_banner: Option<&'a str>,
    /// Offer the "Verify?" button above Close (finished, unverified backups).
    pub offer_verify: bool,
}

const MODAL_WIDTH: f32 = 460.0;
const CANCEL_BUTTON_ID: &str = "status_modal_cancel";
const CLOSE_BUTTON_ID: &str = "status_modal_close";
const VERIFY_BUTTON_ID: &str = "status_modal_verify";
/// How long the user must hold the Cancel button before the job is actually
/// cancelled. A deliberate friction so a stray click can't kill a long backup.
const CANCEL_HOLD_SECS: f32 = 1.5;
/// Temp-memory keys for the hold-to-cancel state (see `show_hold_cancel_button`).
/// Cleared centrally when the finished modal appears so the next job starts fresh.
const CANCEL_HOLD_START_ID: &str = "status_modal_cancel_hold_start";
const CANCEL_HOLD_FIRED_ID: &str = "status_modal_cancel_hold_fired";

/// Progress-bar geometry. Taller than egui's default bar so the byte readout
/// sits comfortably inside the barber pole.
const BAR_HEIGHT: f32 = 24.0;
const BAR_ROUNDING: f32 = 6.0;
/// The bar's unfilled track — the same dark tone in *both* themes, on purpose.
/// The readout is white everywhere, so what sits under it has to be dark
/// everywhere; a track that followed the theme would put white text on a
/// near-white bar the moment the fill fell behind it.
const BAR_TRACK: Color32 = Color32::from_rgb(0x2B, 0x2B, 0x2B);
/// The black half of the barber pole's accent/black stripes.
const BAR_STRIPE: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);
/// Horizontal run of one stripe; the pole repeats every `2 *` this.
const BAR_STRIPE_WIDTH: f32 = 13.0;
/// How fast the pole turns, in pixels/second. Slow enough to read as "turning"
/// rather than "racing" — it is a heartbeat, not a speedometer.
const BAR_STRIPE_SPEED: f32 = 26.0;
/// Offsets the readout is stamped at, in soft black, before being drawn in
/// white on top: a 1px ring that keeps it legible over any accent.
const TEXT_HALO: [egui::Vec2; 4] = [
    egui::vec2(-1.0, -1.0),
    egui::vec2(1.0, -1.0),
    egui::vec2(-1.0, 1.0),
    egui::vec2(1.0, 1.0),
];
const HALO_COLOR: Color32 = Color32::from_black_alpha(160);

pub fn show(ctx: &egui::Context, palette: &Palette, view: &ModalView<'_>) -> ModalAction {
    // --- Backdrop: full-screen dim layer above panels, below the dialog.
    // Keep it on Foreground; the dialog Window uses Tooltip so hit-testing
    // always prefers the Close/Cancel button (same Order::Foreground let the
    // backdrop steal clicks after sleep/focus changes on some backends). ---
    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new("status_modal_backdrop"))
        .order(Order::Foreground)
        .fixed_pos(screen.left_top())
        .interactable(true)
        .show(ctx, |ui| {
            // Swallow any click/drag aimed at the background UI.
            let _ = ui.allocate_rect(screen, egui::Sense::click_and_drag());
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(160));
        });

    let mut action = ModalAction::None;

    if view.outcome.is_some() {
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape) {
                action = ModalAction::Close;
            }
        });
    }

    let frame = egui::Frame::window(&ctx.style())
        .fill(palette.sidebar_bg)
        .inner_margin(egui::Margin::same(22.0))
        .rounding(egui::Rounding::same(10.0));

    egui::Window::new("status_modal")
        .order(Order::Tooltip)
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .movable(false)
        .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(MODAL_WIDTH);

            ui.label(RichText::new(view.title).font(fonts::bold(18.0)));
            ui.add_space(12.0);

            if let Some(banner) = success_banner(view) {
                // Successful backup/verify: the play-by-play (bar + checklist)
                // has served its purpose — replace it with one unambiguous verdict.
                show_success_banner(ui, palette, banner);
            } else {
                show_progress_bar(ui, palette, view);
                ui.add_space(14.0);

                // Cap the checklist height so a many-partition job (e.g. a
                // verify across a dozen partitions) scrolls instead of growing
                // the modal taller than the window.
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        show_steps(ui, palette, view);
                    });
            }

            if !view.detail.is_empty() {
                ui.add_space(10.0);
                let detail = RichText::new(view.detail).color(palette.subtle_text);
                if success_banner(view).is_some() {
                    // Everything on the checkmark screen is centered; keep the
                    // result line ("Backup completed: <path>") in step with it.
                    ui.vertical_centered(|ui| ui.label(detail));
                } else {
                    ui.label(detail);
                }
            }

            ui.add_space(18.0);
            action = show_button(ui, palette, view);
        });

    if view.outcome.is_some() {
        // Keep repainting while the result modal is up so a wake-from-sleep
        // does not leave a stale, input-deaf frame until the user moves the mouse.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }

    action
}

/// Deliberately not a palette color: this is the one place in the app that
/// says "your data is safe", and it should read as green regardless of the
/// Windows accent. Legible on both the light and dark sidebar_bg fills.
const SUCCESS_GREEN: Color32 = Color32::from_rgb(22, 154, 73);

/// The banner text for the "big green checkmark" completion screen, or `None`
/// when the normal bar + checklist layout should render. Requires a Success
/// outcome so a Warning/Failure can never be dressed up as a green check.
fn success_banner<'a>(view: &ModalView<'a>) -> Option<&'a str> {
    match view.outcome {
        Some(JobOutcome::Success) => view.success_banner,
        _ => None,
    }
}

/// The completion screen for a successful backup or verify: a filled green
/// circle with a white checkmark, over the bold verdict text ("Completed and
/// verified." / "Completed. Unverified." / "Backup verified."). Replaces the
/// progress bar and step checklist.
fn show_success_banner(ui: &mut egui::Ui, palette: &Palette, banner: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(10.0);
        let radius = 36.0;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(radius * 2.0, radius * 2.0), egui::Sense::hover());
        let center = rect.center();
        let painter = ui.painter();
        painter.circle_filled(center, radius, SUCCESS_GREEN);
        // Checkmark as a polyline; the small end/joint dots round off the
        // square line caps egui strokes would otherwise leave.
        let points = vec![
            center + egui::vec2(-0.42 * radius, 0.02 * radius),
            center + egui::vec2(-0.10 * radius, 0.34 * radius),
            center + egui::vec2(0.45 * radius, -0.28 * radius),
        ];
        let stroke_width = 0.18 * radius;
        for &p in &points {
            painter.circle_filled(p, stroke_width / 2.0, Color32::WHITE);
        }
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(stroke_width, Color32::WHITE),
        ));
        ui.add_space(14.0);
        ui.label(
            RichText::new(banner)
                .font(fonts::bold(17.0))
                .color(palette.icon_color),
        );
        ui.add_space(4.0);
    });
}

/// Hand-painted rather than an `egui::ProgressBar`, which has no way to stripe
/// its fill and takes its text color from the theme (that was the bug: in light
/// mode it stamped *black* text over a dark accent fill).
fn show_progress_bar(ui: &mut egui::Ui, palette: &Palette, view: &ModalView<'_>) {
    // Running with no measurable total: nothing to fill a bar with, so an
    // indeterminate spinner it is.
    if view.outcome.is_none() && view.total_bytes == 0 {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.label(RichText::new("Working…").color(palette.subtle_text));
        });
        ui.ctx().request_repaint();
        return;
    }

    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), BAR_HEIGHT),
        egui::Sense::hover(),
    );
    let rounding = egui::Rounding::same(BAR_ROUNDING);
    let time = ui.input(|i| i.time) as f32;
    let painter = ui.painter();

    let Some(outcome) = view.outcome else {
        // Running: a dark track, and a filled part that is a barber pole of the
        // accent striped with black, turning at a constant rate. The turn is
        // pure liveness — the fraction alone carries the progress — which is why
        // it doesn't speed up, slow down, or stop short of the end.
        painter.rect_filled(rect, rounding, BAR_TRACK);
        let fraction = view.fraction.clamp(0.0, 1.0);
        let fill = egui::Rect::from_min_size(
            rect.left_top(),
            egui::vec2(rect.width() * fraction, rect.height()),
        );
        painter.rect_filled(fill, 0.0, palette.accent);
        stripes::paint(
            painter,
            fill,
            BAR_STRIPE,
            BAR_STRIPE_WIDTH,
            time * BAR_STRIPE_SPEED,
        );
        // Stripes only clip to a rectangle, so the fill just squared off the
        // bar's left corners (and, at 100%, its right ones). Carve them back.
        stripes::patch_rounded_corners(painter, rect, rounding, palette.sidebar_bg);

        let text = format!(
            "{} / {} ({:.1}%)",
            format_bytes(view.current_bytes),
            format_bytes(view.total_bytes),
            fraction * 100.0
        );
        let font = fonts::bold(13.0);
        // The readout is white over a surface that is dark everywhere — the
        // track, and the black half of the pole. The accent half is the one
        // thing here the app doesn't choose (it's whatever the user set in
        // Windows, and it can be pale), so ring the text in soft black first
        // and its contrast stops depending on that.
        for offset in TEXT_HALO {
            painter.text(
                rect.center() + offset,
                Align2::CENTER_CENTER,
                &text,
                font.clone(),
                HALO_COLOR,
            );
        }
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            font,
            Color32::WHITE,
        );
        // Keep the pole turning.
        ui.ctx().request_repaint();
        return;
    };

    // Finished: a full-width bar tinted by the outcome reads as a clear status
    // banner regardless of how far the byte counter got. No stripes and no
    // readout — the job has stopped, so nothing here should still be moving.
    painter.rect_filled(rect, rounding, outcome_color(palette, outcome));
}

fn show_steps(ui: &mut egui::Ui, palette: &Palette, view: &ModalView<'_>) {
    for (i, label) in view.steps.iter().enumerate() {
        let style = step_style(i, view, palette);
        let mut text = RichText::new(label).font(style.font).color(style.color);
        if style.italic {
            text = text.italics();
        }
        ui.label(text);
        ui.add_space(2.0);
    }
}

struct StepStyle {
    font: egui::FontId,
    color: Color32,
    italic: bool,
}

fn step_style(i: usize, view: &ModalView<'_>, palette: &Palette) -> StepStyle {
    // Finished step: italic + lightly grayed.
    let done = StepStyle {
        font: fonts::regular(15.0),
        color: palette.subtle_text,
        italic: true,
    };
    // Current step: bold + bright.
    let current = StepStyle {
        font: fonts::bold(15.0),
        color: palette.icon_color,
        italic: false,
    };
    // Upcoming step: darkly grayed out.
    let upcoming = StepStyle {
        font: fonts::regular(15.0),
        color: palette.dim(palette.subtle_text),
        italic: false,
    };

    match view.outcome {
        Some(JobOutcome::Success) => done,
        Some(outcome) => {
            // Warning/Failure: everything before the stop point is done, the
            // step we stopped on is highlighted in the outcome color, and
            // anything after stays dimmed (never reached).
            if i < view.current_step {
                done
            } else if i == view.current_step {
                StepStyle {
                    font: fonts::bold(15.0),
                    color: outcome_color(palette, outcome),
                    italic: false,
                }
            } else {
                upcoming
            }
        }
        None => {
            if i < view.current_step {
                done
            } else if i == view.current_step {
                current
            } else {
                upcoming
            }
        }
    }
}

fn show_button(ui: &mut egui::Ui, palette: &Palette, view: &ModalView<'_>) -> ModalAction {
    let mut action = ModalAction::None;
    ui.vertical_centered(|ui| match view.outcome {
        // Running: a hold-to-cancel button. A quick click does nothing; the
        // job is cancelled only once the user holds it long enough to fill the
        // bar (see `show_hold_cancel_button`).
        None => {
            if show_hold_cancel_button(ui, palette) {
                action = ModalAction::Cancel;
            }
        }
        // Finished: a Close button tinted by the outcome (blue success,
        // amber warning/cancelled, red failure).
        Some(outcome) => {
            // The job is over — clear any hold-to-cancel latch so the next
            // job's button starts from a clean, empty state.
            ui.data_mut(|d| {
                d.remove::<f64>(egui::Id::new(CANCEL_HOLD_START_ID));
                d.remove::<bool>(egui::Id::new(CANCEL_HOLD_FIRED_ID));
            });
            // On the green-checkmark completion screen the button follows the
            // checkmark's green rather than the blue accent, so the whole
            // modal reads as one verdict.
            let fill = if success_banner(view).is_some() {
                SUCCESS_GREEN
            } else {
                outcome_color(palette, outcome)
            };
            // Unverified backup: offer an after-the-fact verify of the file
            // just written, as a quieter secondary button above Close.
            if view.offer_verify {
                let vresp = ui
                    .push_id(egui::Id::new(VERIFY_BUTTON_ID), |ui| {
                        ui.add_sized(
                            [160.0, 34.0],
                            egui::Button::new(
                                RichText::new("Verify?")
                                    .size(13.0)
                                    .color(palette.icon_color),
                            ),
                        )
                    })
                    .inner;
                if vresp.clicked() {
                    action = ModalAction::Verify;
                }
                ui.add_space(6.0);
            }
            let resp = ui
                .push_id(egui::Id::new(CLOSE_BUTTON_ID), |ui| {
                    ui.add_sized(
                        [160.0, 38.0],
                        egui::Button::new(RichText::new("Close").color(Color32::WHITE)).fill(fill),
                    )
                })
                .inner;
            resp.request_focus();
            if resp.clicked() {
                action = ModalAction::Close;
            }
        }
    });
    action
}

/// The running-job Cancel control: a hold-to-confirm button. A quick click does
/// nothing. The user must hold it — pointer down, or Space/Enter while focused —
/// for [`CANCEL_HOLD_SECS`], during which a red bar fills the button from left
/// to right in a single smooth sweep. Cancellation fires only on the frame the
/// bar reaches the far edge, and exactly once per hold: after it fires, the bar
/// latches full and reads "Cancelling…" (rather than re-arming and re-filling
/// while the worker takes a moment to actually stop). Returns `true` on the one
/// frame the hold completes.
fn show_hold_cancel_button(ui: &mut egui::Ui, palette: &Palette) -> bool {
    let size = egui::vec2(180.0, 38.0);
    let (rect, resp) = ui
        .push_id(egui::Id::new(CANCEL_BUTTON_ID), |ui| {
            ui.allocate_exact_size(size, egui::Sense::click_and_drag())
        })
        .inner;
    // Keep the button focused so it always shows an affordance (focus ring +
    // hover fill) and so keyboard users can hold Space/Enter to cancel.
    resp.request_focus();

    let now = ui.input(|i| i.time);
    let key_held = resp.has_focus()
        && ui.input(|i| i.key_down(egui::Key::Space) || i.key_down(egui::Key::Enter));
    let held = resp.is_pointer_button_down_on() || key_held;

    // Two bits of per-widget temp state: when the current hold began, and
    // whether it has already fired. Releasing (or a job restart, when the first
    // idle frame runs) clears both, so every cancel is a fresh 1.5 s sweep.
    let start_id = egui::Id::new(CANCEL_HOLD_START_ID);
    let fired_id = egui::Id::new(CANCEL_HOLD_FIRED_ID);
    let start: Option<f64> = ui.data(|d| d.get_temp(start_id));
    let fired: bool = ui.data(|d| d.get_temp(fired_id)).unwrap_or(false);

    let mut just_completed = false;
    let progress = if held {
        let started_at = match start {
            Some(s) => s,
            None => {
                ui.data_mut(|d| d.insert_temp(start_id, now));
                now
            }
        };
        let p = (((now - started_at) as f32) / CANCEL_HOLD_SECS).clamp(0.0, 1.0);
        if p >= 1.0 && !fired {
            just_completed = true;
            ui.data_mut(|d| d.insert_temp(fired_id, true));
        }
        p
    } else {
        // Released before firing: reset so the next hold starts from empty.
        // Once it has *fired*, we deliberately keep the `fired` latch — the bar
        // must stay full/red until the job actually ends (cleared centrally
        // when the finished modal appears), so letting go can't "un-cancel".
        if !fired && start.is_some() {
            ui.data_mut(|d| d.remove::<f64>(start_id));
        }
        0.0
    };
    // Once fired, keep the bar visibly full while the worker winds down.
    let display_progress = if fired { 1.0 } else { progress };

    // --- paint ---
    let rounding = egui::Rounding::same(6.0);
    let inactive = ui.visuals().widgets.inactive.bg_fill;
    // Make hover obvious: clearly lift the fill and switch the border to the
    // accent. The default `widgets.hovered.bg_fill` tint is too subtle to
    // notice on this hand-painted button. (Suppressed while held/latched, when
    // the red bar already carries the state.)
    let show_hover = resp.hovered() && !held && !fired;
    let contrast = if palette.light_mode {
        Color32::BLACK
    } else {
        Color32::WHITE
    };
    let base = if show_hover {
        theme::tint(inactive, 0.35, contrast)
    } else {
        inactive
    };
    let (stroke_w, stroke_c) = if show_hover {
        (2.0, palette.accent)
    } else {
        (1.0, palette.subtle_text)
    };
    let fill = palette.danger;
    let painter = ui.painter();
    painter.rect_filled(rect, rounding, base);
    if display_progress > 0.0 {
        let fill_rect = egui::Rect::from_min_size(
            rect.left_top(),
            egui::vec2(rect.width() * display_progress, rect.height()),
        );
        // Square off the leading edge until the bar is full, then round it to
        // match the button's corners.
        let full = display_progress >= 1.0;
        painter.rect_filled(
            fill_rect,
            egui::Rounding {
                nw: rounding.nw,
                sw: rounding.sw,
                ne: if full { rounding.ne } else { 0.0 },
                se: if full { rounding.se } else { 0.0 },
            },
            fill,
        );
    }
    painter.rect_stroke(rect, rounding, egui::Stroke::new(stroke_w, stroke_c));
    let label = if fired {
        "Cancelling…"
    } else if held {
        "Keep holding…"
    } else {
        "Hold to Cancel"
    };
    // White reads cleanly over the red fill; at rest the fill is absent but the
    // hold-to-cancel copy is still legible on the neutral button.
    let text_color = if display_progress > 0.35 {
        Color32::WHITE
    } else {
        palette.icon_color
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        fonts::bold(15.0),
        text_color,
    );
    theme::draw_focus_outline(ui, &resp, palette);

    // Keep the modal ticking: while the sweep animates, and after it has fired
    // (even if the pointer is released) so we notice the moment the job
    // finishes and this button is swapped for the Close button.
    if held || fired {
        ui.ctx().request_repaint();
    }
    just_completed
}

fn outcome_color(palette: &Palette, outcome: JobOutcome) -> Color32 {
    match outcome {
        // Blue (the live Windows accent, used elsewhere for "active" UI).
        JobOutcome::Success => palette.accent,
        JobOutcome::Warning => palette.warning,
        JobOutcome::Failure => palette.danger,
    }
}
