//! Blocking status modal: a centered dialog over a dimmed, input-swallowing
//! backdrop that renders the operation's predeclared step checklist, a
//! progress bar, and a hold-to-cancel button.
//!
//! When the job ends the modal stays up and turns into a result screen: the
//! progress bar gives way to a [`Verdict`] badge — a colored disc with a
//! checkmark, an exclamation, or a cross — and the Cancel button becomes a
//! Close button in the same color. Every job kind and every outcome gets one;
//! the modal never leaves a finished job wearing the running layout.
//!
//! egui 0.29 has no `egui::Modal` (added in 0.30), so we hand-roll it from a
//! foreground `Area` backdrop plus a centered `Window`. This is structured so
//! it can be swapped for `egui::Modal` if/when we upgrade egui.

use egui;
use egui::{Align2, Color32, Order, RichText};

use crate::fonts;
use crate::stripes;
use crate::theme::{self, Palette};
use crate::util::format_bytes;

/// How a finished job ended: picks the badge glyph, and the color of both the
/// badge and the Close button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobOutcome {
    Success,
    Warning,
    Failure,
}

/// A finished job's verdict. Outcome and wording travel together — every job
/// that ends says how it ended, whichever kind it was, so there is no way to
/// leave a finished modal wearing the running layout.
#[derive(Clone, Copy, Debug)]
pub struct Verdict<'a> {
    pub outcome: JobOutcome,
    /// The one-line verdict shown under the badge: "Completed and verified.",
    /// "Restore failed.", "Clone cancelled.".
    pub banner: &'a str,
}

/// Snapshot of a finished job, kept around so the modal stays up (showing the
/// verdict badge + a Close button) until the user dismisses it.
pub struct CompletedJob {
    pub title: String,
    pub steps: Vec<String>,
    pub current_step: usize,
    pub outcome: JobOutcome,
    pub message: String,
    /// The [`Verdict::banner`] wording for this job.
    pub banner: String,
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
    /// `None` while running, `Some(verdict)` once the job has finished.
    pub finished: Option<Verdict<'a>>,
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

    if view.finished.is_some() {
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

            match view.finished {
                // Finished: the play-by-play has served its purpose. The bar
                // goes, and a badge in the outcome's color states what happened.
                Some(verdict) => {
                    show_verdict_badge(ui, palette, verdict);
                    // A clean success has nothing left to explain, so the badge
                    // stands alone. Anything else keeps the checklist below it:
                    // it is the only thing that says *where* the job stopped.
                    if verdict.outcome != JobOutcome::Success {
                        ui.add_space(14.0);
                        show_step_list(ui, palette, view);
                    }
                }
                None => {
                    show_progress_bar(ui, palette, view);
                    ui.add_space(14.0);
                    show_step_list(ui, palette, view);
                }
            }

            if !view.detail.is_empty() {
                ui.add_space(10.0);
                let detail = RichText::new(view.detail).color(palette.subtle_text);
                if bare_success(view) {
                    // Everything on the bare success screen is centered; keep
                    // the result line ("Backup completed: <path>") in step with
                    // it. Once a checklist is in play the detail is left-aligned
                    // copy under left-aligned copy.
                    ui.vertical_centered(|ui| ui.label(detail));
                } else {
                    ui.label(detail);
                }
            }

            ui.add_space(18.0);
            action = show_button(ui, palette, view);
        });

    if view.finished.is_some() {
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

/// A finished, clean success: the one layout that drops the step checklist and
/// centers everything, because there is nothing left to diagnose.
fn bare_success(view: &ModalView<'_>) -> bool {
    matches!(
        view.finished,
        Some(Verdict {
            outcome: JobOutcome::Success,
            ..
        })
    )
}

/// The color a finished job's badge, and its Close button, are painted in.
/// Distinct from [`outcome_color`]: success here is *green*, not the Windows
/// accent, because the badge is a verdict and not just an active-UI surface.
fn verdict_color(palette: &Palette, outcome: JobOutcome) -> Color32 {
    match outcome {
        JobOutcome::Success => SUCCESS_GREEN,
        JobOutcome::Warning => palette.warning,
        JobOutcome::Failure => palette.danger,
    }
}

/// The completion badge: a big filled circle in the outcome's color carrying a
/// white glyph — a checkmark for success, an exclamation for a cancel, a cross
/// for a failure — over the bold verdict line ("Completed and verified.",
/// "Restore failed."). Takes the progress bar's place once the job has ended.
fn show_verdict_badge(ui: &mut egui::Ui, palette: &Palette, verdict: Verdict<'_>) {
    ui.vertical_centered(|ui| {
        ui.add_space(10.0);
        let radius = 36.0;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(radius * 2.0, radius * 2.0), egui::Sense::hover());
        let center = rect.center();
        let painter = ui.painter();
        painter.circle_filled(center, radius, verdict_color(palette, verdict.outcome));

        // Each glyph is one or more polylines, in units of the radius. The dots
        // at every end and joint round off the square caps egui strokes would
        // otherwise leave.
        let glyph: &[&[egui::Vec2]] = match verdict.outcome {
            JobOutcome::Success => &[&[
                egui::vec2(-0.42, 0.02),
                egui::vec2(-0.10, 0.34),
                egui::vec2(0.45, -0.28),
            ]],
            JobOutcome::Failure => &[
                &[egui::vec2(-0.32, -0.32), egui::vec2(0.32, 0.32)],
                &[egui::vec2(0.32, -0.32), egui::vec2(-0.32, 0.32)],
            ],
            // An exclamation mark: its bar, then its dot — the dot drawn as a
            // zero-length stroke, so the end-cap circle *is* the dot.
            JobOutcome::Warning => &[
                &[egui::vec2(0.0, -0.44), egui::vec2(0.0, 0.12)],
                &[egui::vec2(0.0, 0.40), egui::vec2(0.0, 0.40)],
            ],
        };
        let stroke_width = 0.18 * radius;
        for stroke in glyph {
            let points: Vec<egui::Pos2> = stroke.iter().map(|&v| center + v * radius).collect();
            for &p in &points {
                painter.circle_filled(p, stroke_width / 2.0, Color32::WHITE);
            }
            painter.add(egui::Shape::line(
                points,
                egui::Stroke::new(stroke_width, Color32::WHITE),
            ));
        }

        ui.add_space(14.0);
        ui.label(
            RichText::new(verdict.banner)
                .font(fonts::bold(17.0))
                .color(palette.icon_color),
        );
        ui.add_space(4.0);
    });
}

/// The step display. Rather than a full checklist, we show a moving window of
/// at most three steps — the one just finished, the current one, and the one
/// coming up — so a many-partition job never grows the modal or needs a
/// scrollbar. The current step is the focus: larger, brighter, and framed by a
/// pair of accent rules that fade to transparent at the modal's edges.
fn show_step_list(ui: &mut egui::Ui, palette: &Palette, view: &ModalView<'_>) {
    show_steps(ui, palette, view);
}

/// The running job's progress bar — a dark track, and a filled part that is a
/// barber pole of the accent striped with black, turning at a constant rate.
/// The turn is pure liveness (the fraction alone carries the progress), which
/// is why it doesn't speed up, slow down, or stop short of the end.
///
/// Only ever drawn while the job runs: once it ends, the verdict badge takes
/// this slot, so nothing here has to have a "stopped" look.
///
/// Hand-painted rather than an `egui::ProgressBar`, which has no way to stripe
/// its fill and takes its text color from the theme (that was the bug: in light
/// mode it stamped *black* text over a dark accent fill).
fn show_progress_bar(ui: &mut egui::Ui, palette: &Palette, view: &ModalView<'_>) {
    // No measurable total: nothing to fill a bar with, so an indeterminate
    // spinner it is.
    if view.total_bytes == 0 {
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
    // The readout is white over a surface that is dark everywhere — the track,
    // and the black half of the pole. The accent half is the one thing here the
    // app doesn't choose (it's whatever the user set in Windows, and it can be
    // pale), so ring the text in soft black first and its contrast stops
    // depending on that.
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
}

/// Font size of the current (focused) step. Slightly smaller than the first
/// pass — it is still the headline, but no longer shouts.
const STEP_CENTER_SIZE: f32 = 20.0;
/// Font size a step settles to once it has floated out to a flanking slot.
const STEP_FLANK_SIZE: f32 = 14.0;
/// Vertical gap between adjacent step slots. A flanking step sits this far above
/// or below the centered current step.
const STEP_ROW_SPACING: f32 = 32.0;
/// Half the distance between the two accent rules — they sit this far above and
/// below the display's center line, framing whatever step is centered.
const STEP_RULE_GAP: f32 = 22.0;
/// Thickness of the accent rules that frame the current step.
const STEP_RULE_HEIGHT: f32 = 2.0;
/// Whether to frame the centered step with the pair of accent rules.
const STEP_SHOW_RULES: bool = false;
/// Fixed height of the step display region. Sized to hold the centered step
/// plus one flanking step above and below with room for them to fade off.
const STEP_REGION_HEIGHT: f32 = 128.0;
/// How long a step takes to float from one slot to the next when the job
/// advances. Short enough to feel responsive, long enough to read as motion.
const STEP_ANIM_SECS: f32 = 0.35;
/// Temp-memory keys for the step tween (see `animated_position`).
const STEP_FROM_ID: &str = "status_modal_step_from";
const STEP_TARGET_ID: &str = "status_modal_step_target";
const STEP_START_ID: &str = "status_modal_step_start";

/// The step display. Rather than a checklist, it shows the current step centered
/// and bright, with the just-finished and upcoming steps flanking it, smaller
/// and dimmer. When the job advances, the whole stack animates by one slot: the
/// completing step shrinks and darkens as it floats up and out, and the upcoming
/// step enlarges and brightens as it slides into the center — so at most three
/// steps are ever on screen and the transition between them is continuous.
fn show_steps(ui: &mut egui::Ui, palette: &Palette, view: &ModalView<'_>) {
    let len = view.steps.len();
    if len == 0 {
        return;
    }
    let current = view.current_step.min(len - 1) as f32;
    // Eased position of the "center" of the display, in step-index units. Equal
    // to `current` at rest; between two indices mid-transition.
    let anim = animated_position(ui, current);

    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), STEP_REGION_HEIGHT),
        egui::Sense::hover(),
    );
    let center_y = rect.center().y;
    let rule_color = match view.finished {
        Some(verdict) => verdict_color(palette, verdict.outcome),
        None => palette.accent,
    };
    // The bright color the centered step reaches: the outcome's color on a
    // finished modal, otherwise the normal bright text.
    let bright = match view.finished {
        Some(verdict) => verdict_color(palette, verdict.outcome),
        None => palette.icon_color,
    };

    // The two accent rules stay put, framing the center line; steps float
    // through them. Painted first so text sits on top.
    if STEP_SHOW_RULES {
        paint_hrule(ui.painter(), rect, center_y - STEP_RULE_GAP, rule_color);
        paint_hrule(ui.painter(), rect, center_y + STEP_RULE_GAP, rule_color);
    }

    // Draw every step whose slot is close enough to the center to be visible.
    let lo = (anim.floor() as isize) - 2;
    let hi = (anim.ceil() as isize) + 2;
    for i in lo..=hi {
        if i < 0 || i as usize >= len {
            continue;
        }
        let d = i as f32 - anim; // signed slot distance from center
        let ad = d.abs();
        if ad >= 1.5 {
            continue;
        }
        // t: 0 at the center slot, 1 out at a flanking slot.
        let t = ad.min(1.0);
        let size = STEP_CENTER_SIZE + (STEP_FLANK_SIZE - STEP_CENTER_SIZE) * t;
        // Dim target depends on which side: the finished step above grays a
        // little less than the not-yet-reached step below.
        let dim = if d < 0.0 {
            palette.subtle_text
        } else {
            palette.dim(palette.subtle_text)
        };
        let color = lerp_color(bright, dim, t);
        // Fade to nothing as a step leaves the window past the flanking slots.
        let alpha = 1.0 - smoothstep(1.0, 1.5, ad);
        let color = color.gamma_multiply(alpha);
        let y = center_y + d * STEP_ROW_SPACING;
        ui.painter().text(
            egui::pos2(rect.center().x, y),
            Align2::CENTER_CENTER,
            &view.steps[i as usize],
            fonts::bold(size),
            color,
        );
    }
}

/// The eased center position of the step display, in step-index units. Tracks a
/// tween in temp memory: whenever `target` changes it retargets from wherever
/// the display currently sits, so an advance mid-animation stays smooth. A large
/// jump (e.g. a brand-new job resetting to step 0) snaps rather than scrolling
/// through every intervening step.
fn animated_position(ui: &mut egui::Ui, target: f32) -> f32 {
    let from_id = egui::Id::new(STEP_FROM_ID);
    let target_id = egui::Id::new(STEP_TARGET_ID);
    let start_id = egui::Id::new(STEP_START_ID);
    let now = ui.input(|i| i.time);

    let stored_target: Option<f32> = ui.data(|d| d.get_temp(target_id));
    let from: f32 = ui.data(|d| d.get_temp(from_id)).unwrap_or(target);
    let start: f64 = ui.data(|d| d.get_temp(start_id)).unwrap_or(now);

    let eased = smoothstep(0.0, 1.0, (((now - start) / STEP_ANIM_SECS as f64) as f32).clamp(0.0, 1.0));
    let displayed = from + (stored_target.unwrap_or(target) - from) * eased;

    if stored_target != Some(target) {
        // Retarget from where we are now — unless the gap is large, in which
        // case snap so we don't fling through a dozen steps.
        let new_from = if (target - displayed).abs() > 2.5 {
            target
        } else {
            displayed
        };
        ui.data_mut(|d| {
            d.insert_temp(from_id, new_from);
            d.insert_temp(target_id, target);
            d.insert_temp(start_id, now);
        });
        return new_from;
    }

    if (displayed - target).abs() > 0.001 {
        ui.ctx().request_repaint();
    }
    displayed
}

/// Linear interpolation between two opaque colors, channel-wise.
fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Smooth Hermite interpolation: 0 below `a`, 1 above `b`, eased in between.
fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A full-width horizontal rule at height `y` within `rect`, painted as a
/// gradient mesh: `color` at the center, fading to transparent at both ends.
fn paint_hrule(painter: &egui::Painter, rect: egui::Rect, y: f32, color: Color32) {
    let transparent = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 0);
    let (top, bot) = (y - STEP_RULE_HEIGHT / 2.0, y + STEP_RULE_HEIGHT / 2.0);
    let (xl, xc, xr) = (rect.left(), rect.center().x, rect.right());

    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(egui::pos2(xl, top), transparent); // 0
    mesh.colored_vertex(egui::pos2(xl, bot), transparent); // 1
    mesh.colored_vertex(egui::pos2(xc, top), color); // 2
    mesh.colored_vertex(egui::pos2(xc, bot), color); // 3
    mesh.colored_vertex(egui::pos2(xr, top), transparent); // 4
    mesh.colored_vertex(egui::pos2(xr, bot), transparent); // 5
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(2, 1, 3);
    mesh.add_triangle(2, 3, 4);
    mesh.add_triangle(4, 3, 5);
    painter.add(mesh);
}

fn show_button(ui: &mut egui::Ui, palette: &Palette, view: &ModalView<'_>) -> ModalAction {
    let mut action = ModalAction::None;
    ui.vertical_centered(|ui| match view.finished {
        // Running: a hold-to-cancel button. A quick click does nothing; the
        // job is cancelled only once the user holds it long enough to fill the
        // bar (see `show_hold_cancel_button`).
        None => {
            if show_hold_cancel_button(ui, palette) {
                action = ModalAction::Cancel;
            }
        }
        // Finished: a Close button in the badge's own color (green success,
        // amber cancelled, red failure), so the whole modal reads as one verdict.
        Some(verdict) => {
            // The job is over — clear any hold-to-cancel latch so the next
            // job's button starts from a clean, empty state.
            ui.data_mut(|d| {
                d.remove::<f64>(egui::Id::new(CANCEL_HOLD_START_ID));
                d.remove::<bool>(egui::Id::new(CANCEL_HOLD_FIRED_ID));
            });
            let fill = verdict_color(palette, verdict.outcome);
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
