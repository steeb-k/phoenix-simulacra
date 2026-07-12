//! Sticky full-width primary-action bar pinned to the bottom of the
//! central pane.
//!
//! Every page with a Start action (Backup, Restore, Verify, Clone, Mount)
//! renders it here instead of at the end of its scrollable content:
//! the page scrolls, the action never moves. The bar is flush with the
//! pane edges and carries the pane's rounded bottom-left corner, so it
//! reads as the bottom cap of the central panel rather than a floating
//! control.

use eframe::egui;
use egui::{Color32, Mesh, Pos2, Rect, Rounding, Sense, Shape, Stroke};

use crate::theme::{tint, Palette};
use crate::{fonts, icon_label, StartAction, START_BUTTON_HEIGHT};

/// Radius of the pane's bottom-left corner. Must match `panel_rounding`
/// in `PhoenixApp::update` — the bar takes the cutout over from the
/// central panel whenever it is shown.
pub const CORNER_RADIUS: f32 = 10.0;

/// Render the bar for `action`. `interactable` is false while a modal is
/// up, which suppresses clicks, hover states, and tooltips without
/// changing the layout. Returns `(clicked, bar_rect)`; the rect lets the
/// caller extend the pane's 1px border down around the button.
pub fn show(
    ctx: &egui::Context,
    palette: &Palette,
    action: &StartAction<'_>,
    interactable: bool,
) -> (bool, Rect) {
    egui::TopBottomPanel::bottom("page_action_bar")
        .show_separator_line(false)
        .exact_height(START_BUTTON_HEIGHT)
        // The corner cutout shows this fill — sidebar_bg, same as the
        // window clear color, so it reads as part of the sidebar/status L.
        .frame(egui::Frame::none().fill(palette.sidebar_bg))
        .show(ctx, |ui| {
            let rect = ui.max_rect();
            let enabled = action.enabled && interactable;
            let response = ui.allocate_rect(
                rect,
                if enabled {
                    Sense::click()
                } else {
                    Sense::hover()
                },
            );

            let rounding = Rounding {
                sw: CORNER_RADIUS,
                ..Rounding::ZERO
            };
            if action.enabled {
                // Vertical gradient: lit at the top, settling darker at
                // the bottom. Hover lifts the whole surface; press
                // flattens it dark.
                let base = palette.success;
                let (top, bottom) = if interactable && response.is_pointer_button_down_on() {
                    (
                        tint(base, 0.16, Color32::BLACK),
                        tint(base, 0.04, Color32::BLACK),
                    )
                } else if interactable && response.hovered() {
                    (
                        tint(base, 0.24, Color32::WHITE),
                        tint(base, 0.02, Color32::WHITE),
                    )
                } else {
                    (
                        tint(base, 0.15, Color32::WHITE),
                        tint(base, 0.15, Color32::BLACK),
                    )
                };
                paint_vertical_gradient(ui.painter(), rect, rounding, top, bottom);

                // 1px sheen along the top edge sells the gradient as a
                // lit surface instead of a flat banner.
                ui.painter().hline(
                    rect.x_range(),
                    rect.top() + 0.5,
                    Stroke::new(1.0, Color32::from_white_alpha(36)),
                );
            } else {
                // Not armed yet: monochrome hazard tape (same procedural
                // stripes as the disk-wipe dialogs, minus the shouting
                // yellow) until the page's selections complete and the
                // bar turns its normal green.
                paint_hazard_tape(ui.painter(), rect, rounding, palette);
            }

            let text_color = if action.enabled {
                Color32::WHITE
            } else if palette.light_mode {
                Color32::from_rgb(0x50, 0x50, 0x50)
            } else {
                Color32::from_white_alpha(170)
            };
            let job = match action.icon {
                Some(icon) => icon_label(
                    icon,
                    fonts::icon_fill(30.0),
                    action.label,
                    fonts::bold(24.0),
                    text_color,
                ),
                None => {
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        action.label,
                        0.0,
                        egui::TextFormat {
                            font_id: fonts::bold(24.0),
                            color: text_color,
                            ..Default::default()
                        },
                    );
                    job
                }
            };
            let galley = ui.fonts(|f| f.layout_job(job));
            let pos = rect.center() - galley.size() * 0.5;
            ui.painter().galley(pos, galley, text_color);

            let mut clicked = false;
            if enabled {
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                // `allocate_rect` doesn't paint its own focus affordance,
                // so draw an inner ring and honor Enter/Space by hand to
                // keep the Tab-to-Start keyboard path the old egui
                // `Button` provided.
                if response.has_focus() {
                    ui.painter().rect_stroke(
                        rect.shrink(3.0),
                        Rounding {
                            sw: CORNER_RADIUS - 3.0,
                            ..Rounding::same(4.0)
                        },
                        Stroke::new(2.0, Color32::WHITE),
                    );
                }
                clicked = response.clicked()
                    || (response.has_focus()
                        && ui.input(|i| {
                            i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space)
                        }));
            } else if interactable {
                if let Some(hint) = action.disabled_hint {
                    response.on_hover_text(hint);
                }
            }
            (clicked, rect)
        })
        .inner
}

/// Disabled-state fill: diagonal hazard-tape stripes, built the same way
/// as `confirm_dialog::paint_tape` (45° parallelograms, dark top fade)
/// but in muted monochrome — black/gray in dark mode, white/light-gray in
/// light mode — so it signals "blocked until the form is complete"
/// without borrowing the destructive dialogs' yellow.
///
/// The stripes are painted square (they can't be clipped to a rounded
/// rect), so the pane's rounded bottom-left corner is patched back in
/// afterwards with the sidebar color.
fn paint_hazard_tape(painter: &egui::Painter, rect: Rect, rounding: Rounding, palette: &Palette) {
    let (base, stripe) = if palette.light_mode {
        (Color32::WHITE, Color32::from_rgb(0xD9, 0xD9, 0xD9))
    } else {
        (
            Color32::from_rgb(0x16, 0x16, 0x16),
            Color32::from_rgb(0x3A, 0x3A, 0x3A),
        )
    };
    // Base coat with the rounded silhouette (a flat "gradient").
    paint_vertical_gradient(painter, rect, rounding, base, base);

    // Same construction as the dialog tape: 45° slant (horizontal offset =
    // strip height), but with a fixed stripe width — height-wide stripes
    // would turn chunky on a 52px bar.
    const STRIPE_W: f32 = 22.0;
    let clipped = painter.with_clip_rect(rect);
    let h = rect.height();
    let mut x = rect.left() - h;
    while x < rect.right() {
        clipped.add(Shape::convex_polygon(
            vec![
                egui::pos2(x, rect.bottom()),
                egui::pos2(x + STRIPE_W, rect.bottom()),
                egui::pos2(x + STRIPE_W + h, rect.top()),
                egui::pos2(x + h, rect.top()),
            ],
            stripe,
            Stroke::NONE,
        ));
        x += STRIPE_W * 2.0;
    }

    // The dialogs' tape sheen, toned down: a faint top fade for depth
    // (dark mode only — on white tape it just looks like dirt).
    if !palette.light_mode {
        let mut sheen = Mesh::default();
        let top_tint = Color32::from_black_alpha(50);
        sheen.colored_vertex(rect.left_top(), top_tint);
        sheen.colored_vertex(rect.right_top(), top_tint);
        sheen.colored_vertex(rect.right_bottom(), Color32::TRANSPARENT);
        sheen.colored_vertex(rect.left_bottom(), Color32::TRANSPARENT);
        sheen.add_triangle(0, 1, 2);
        sheen.add_triangle(0, 2, 3);
        clipped.add(Shape::mesh(sheen));
    }

    // Restore the rounded bottom-left corner: repaint the spandrel between
    // the corner square and the quarter-circle arc in the sidebar color the
    // cutout is supposed to show.
    paint_sw_corner_patch(painter, rect, rounding.sw, palette.sidebar_bg);
}

/// Fill the region of the bottom-left corner square that lies OUTSIDE the
/// rounded corner's quarter-circle arc. Non-convex, but star-shaped from
/// the corner point, so a triangle fan from there covers it exactly.
fn paint_sw_corner_patch(painter: &egui::Painter, rect: Rect, radius: f32, fill: Color32) {
    use std::f32::consts::{FRAC_PI_2, PI};
    const ARC_STEPS: usize = 8;
    let center = egui::pos2(rect.left() + radius, rect.bottom() - radius);
    let mut mesh = Mesh::default();
    mesh.colored_vertex(egui::pos2(rect.left(), rect.bottom()), fill);
    for i in 0..=ARC_STEPS {
        let a = FRAC_PI_2 + (PI - FRAC_PI_2) * (i as f32 / ARC_STEPS as f32);
        mesh.colored_vertex(
            egui::pos2(center.x + radius * a.cos(), center.y + radius * a.sin()),
            fill,
        );
    }
    for i in 1..=ARC_STEPS as u32 {
        mesh.add_triangle(0, i, i + 1);
    }
    painter.add(Shape::mesh(mesh));
}

/// Fill a rounded rect with a top→bottom gradient. egui has no gradient
/// fill primitive, so this builds the rounded outline by hand, colors each
/// vertex by its height, and fan-triangulates (a rounded rect is convex,
/// and barycentric interpolation reproduces a linear gradient exactly).
fn paint_vertical_gradient(
    painter: &egui::Painter,
    rect: Rect,
    rounding: Rounding,
    top: Color32,
    bottom: Color32,
) {
    let points = rounded_rect_points(rect, rounding);
    let mut mesh = Mesh::default();
    let height = rect.height().max(1.0);
    for p in &points {
        let t = ((p.y - rect.top()) / height).clamp(0.0, 1.0);
        mesh.colored_vertex(*p, lerp_color(top, bottom, t));
    }
    for i in 1..points.len() as u32 - 1 {
        mesh.add_triangle(0, i, i + 1);
    }
    painter.add(Shape::mesh(mesh));
}

/// Outline of a rounded rect, clockwise from the top-left corner, with
/// each rounded corner sampled as a short arc.
fn rounded_rect_points(rect: Rect, rounding: Rounding) -> Vec<Pos2> {
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    const ARC_STEPS: usize = 8;

    fn corner(pts: &mut Vec<Pos2>, cx: f32, cy: f32, radius: f32, a0: f32, a1: f32) {
        if radius <= 0.0 {
            pts.push(egui::pos2(cx, cy));
            return;
        }
        for i in 0..=ARC_STEPS {
            let a = a0 + (a1 - a0) * (i as f32 / ARC_STEPS as f32);
            pts.push(egui::pos2(cx + radius * a.cos(), cy + radius * a.sin()));
        }
    }

    let mut pts = Vec::with_capacity(4 * (ARC_STEPS + 1));
    let (l, r, t, b) = (rect.left(), rect.right(), rect.top(), rect.bottom());
    corner(&mut pts, l + rounding.nw, t + rounding.nw, rounding.nw, PI, 1.5 * PI);
    corner(&mut pts, r - rounding.ne, t + rounding.ne, rounding.ne, 1.5 * PI, TAU);
    corner(&mut pts, r - rounding.se, b - rounding.se, rounding.se, 0.0, FRAC_PI_2);
    corner(&mut pts, l + rounding.sw, b - rounding.sw, rounding.sw, FRAC_PI_2, PI);
    pts
}

/// Per-channel sRGB lerp (same "paint program" tradeoff as [`tint`]).
fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}
