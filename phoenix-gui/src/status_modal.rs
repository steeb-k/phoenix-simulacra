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
}

/// What the user did with the modal this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalAction {
    None,
    Cancel,
    Close,
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
}

const MODAL_WIDTH: f32 = 460.0;
const CANCEL_BUTTON_ID: &str = "status_modal_cancel";
const CLOSE_BUTTON_ID: &str = "status_modal_close";

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

            show_progress_bar(ui, palette, view);
            ui.add_space(14.0);

            // Cap the checklist height so a many-partition job (e.g. a verify
            // across a dozen partitions) scrolls instead of growing the modal
            // taller than the window.
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    show_steps(ui, palette, view);
                });

            if !view.detail.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new(view.detail).color(palette.subtle_text));
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

fn show_progress_bar(ui: &mut egui::Ui, palette: &Palette, view: &ModalView<'_>) {
    match view.outcome {
        // Finished: a full-width bar tinted by the outcome reads as a clear
        // status banner regardless of how far the byte counter got.
        Some(outcome) => {
            let fill = outcome_color(palette, outcome);
            ui.add(egui::ProgressBar::new(1.0).fill(fill));
        }
        // Running with a known total: live fraction with a breathing fill.
        None if view.total_bytes > 0 => {
            let fraction = view.fraction;
            let fill = if fraction >= 1.0 {
                palette.accent
            } else {
                // ~2s sinusoidal pulse between a desaturated accent/input_bg
                // blend and the full accent — a calm "this UI is alive" cue.
                let t = ui.input(|i| i.time) as f32;
                let pulse = 0.5 + 0.5 * (t * std::f32::consts::PI).sin();
                let dim_end = theme::tint(palette.accent, 0.7, palette.input_bg);
                theme::tint(dim_end, pulse, palette.accent)
            };
            ui.add(egui::ProgressBar::new(fraction).fill(fill).text(format!(
                "{} / {} ({:.1}%)",
                format_bytes(view.current_bytes),
                format_bytes(view.total_bytes),
                fraction * 100.0
            )));
            if fraction < 1.0 {
                ui.ctx().request_repaint();
            }
        }
        // Running with no measurable total: indeterminate spinner.
        None => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(RichText::new("Working…").color(palette.subtle_text));
            });
            ui.ctx().request_repaint();
        }
    }
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
    ui.vertical_centered(|ui| {
        let resp = match view.outcome {
            // Running: a plain, theme-colored button — cancelling is no longer
            // styled as a "danger" action.
            None => {
                let resp = ui
                    .push_id(egui::Id::new(CANCEL_BUTTON_ID), |ui| {
                        ui.add_sized([160.0, 38.0], egui::Button::new("Cancel"))
                    })
                    .inner;
                resp.request_focus();
                resp
            }
            // Finished: a Close button tinted by the outcome (blue success,
            // amber warning/cancelled, red failure).
            Some(outcome) => {
                let resp = ui
                    .push_id(egui::Id::new(CLOSE_BUTTON_ID), |ui| {
                        ui.add_sized(
                            [160.0, 38.0],
                            egui::Button::new(RichText::new("Close").color(Color32::WHITE))
                                .fill(outcome_color(palette, outcome)),
                        )
                    })
                    .inner;
                resp.request_focus();
                resp
            }
        };
        if resp.clicked() {
            action = match view.outcome {
                None => ModalAction::Cancel,
                Some(_) => ModalAction::Close,
            };
        }
    });
    action
}

fn outcome_color(palette: &Palette, outcome: JobOutcome) -> Color32 {
    match outcome {
        // Blue (the live Windows accent, used elsewhere for "active" UI).
        JobOutcome::Success => palette.accent,
        JobOutcome::Warning => palette.warning,
        JobOutcome::Failure => palette.danger,
    }
}
