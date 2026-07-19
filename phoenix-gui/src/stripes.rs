//! The app's diagonal-stripe motif, drawn procedurally so it stays crisp at
//! any DPI and tiles to any width.
//!
//! Three places use it, for three different reasons:
//!   * [`crate::confirm_dialog`] — yellow/black hazard tape on the dialogs that
//!     wipe a disk.
//!   * [`crate::action_bar`] — muted monochrome tape on a Start bar that isn't
//!     armed yet.
//!   * [`crate::status_modal`] — an accent/black barber pole on the running
//!     progress bar, the one caller that gives it a moving `phase`.
//!
//! Stripes can only be clipped to a rectangle, so anything drawn with them
//! comes out square-cornered. Callers that need a rounded silhouette get it
//! back from [`patch_rounded_corners`].

use egui;
use egui::{Color32, Mesh, Rect, Rounding, Shape, Stroke};

/// Lay 45° bars of `stripe` across `rect`, over whatever the caller has
/// already painted there.
///
/// `width` is one bar's horizontal run; the gap between bars matches it, so the
/// pattern repeats every `2 * width` pixels. `phase` slides the pattern right
/// by that many pixels — leave it at 0 for static tape, or feed it a rising
/// value (time × speed) to turn the pattern into a barber pole.
pub fn paint(painter: &egui::Painter, rect: Rect, stripe: Color32, width: f32, phase: f32) {
    let painter = painter.with_clip_rect(rect);
    let period = width * 2.0;
    // Each bar leans right by the strip's own height — that's what makes it
    // 45° — so begin a full lean *and* a full period to the left of the rect:
    // far enough out that neither the lean nor the phase can leave the left
    // edge bare. Wrapping the phase keeps `x` from drifting away over a long
    // job (an hours-long backup would otherwise push it into float mush).
    let lean = rect.height();
    let mut x = rect.left() - lean - period + phase.rem_euclid(period);
    while x < rect.right() {
        painter.add(Shape::convex_polygon(
            vec![
                egui::pos2(x, rect.bottom()),
                egui::pos2(x + width, rect.bottom()),
                egui::pos2(x + width + lean, rect.top()),
                egui::pos2(x + lean, rect.top()),
            ],
            stripe,
            Stroke::NONE,
        ));
        x += period;
    }
}

/// A top-down dark fade over `rect`, `alpha` at the top edge and gone by the
/// bottom. The bit of sheen that sells the stripes as a strip of tape rather
/// than a flat pattern.
pub fn sheen(painter: &egui::Painter, rect: Rect, alpha: u8) {
    let mut mesh = Mesh::default();
    let top = Color32::from_black_alpha(alpha);
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), Color32::TRANSPARENT);
    mesh.colored_vertex(rect.left_bottom(), Color32::TRANSPARENT);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.with_clip_rect(rect).add(Shape::mesh(mesh));
}

/// A top-down fade *into* `fill`: `fill` at `top_alpha` along the top edge,
/// fully opaque `fill` by the bottom. The inverse of [`sheen`] — that one
/// shades a strip to sell it as tape, this one dissolves a strip into the
/// surface behind it so decoration can sit under content without competing
/// with it.
pub fn fade_into(painter: &egui::Painter, rect: Rect, fill: Color32, top_alpha: u8) {
    let top = Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), top_alpha);
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), fill);
    mesh.colored_vertex(rect.left_bottom(), fill);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.with_clip_rect(rect).add(Shape::mesh(mesh));
}

/// Repaint each of `rect`'s rounded corners' spandrels — the sliver between the
/// corner square and the quarter-circle arc that should be cut out of it — in
/// `fill`, the color showing *behind* the widget.
///
/// This is how a square-cornered pattern (stripes, a tile mesh) gets a rounded
/// silhouette: paint it square, then carve the corners back out. Each sliver is
/// non-convex but star-shaped from its corner point, so a triangle fan anchored
/// there covers it exactly. Corners with a zero radius are skipped.
pub fn patch_rounded_corners(
    painter: &egui::Painter,
    rect: Rect,
    rounding: Rounding,
    fill: Color32,
) {
    use std::f32::consts::{FRAC_PI_2, PI};
    const ARC_STEPS: usize = 8;

    // `from` is the angle (y-down, so clockwise) at which that corner's arc
    // starts; every arc sweeps a quarter turn from there.
    let patch = |corner: egui::Pos2, center: egui::Pos2, radius: f32, from: f32| {
        if radius <= 0.0 {
            return;
        }
        let mut mesh = Mesh::default();
        mesh.colored_vertex(corner, fill);
        for i in 0..=ARC_STEPS {
            let a = from + FRAC_PI_2 * (i as f32 / ARC_STEPS as f32);
            mesh.colored_vertex(
                egui::pos2(center.x + radius * a.cos(), center.y + radius * a.sin()),
                fill,
            );
        }
        for i in 1..=ARC_STEPS as u32 {
            mesh.add_triangle(0, i, i + 1);
        }
        painter.add(Shape::mesh(mesh));
    };

    let (l, r, t, b) = (rect.left(), rect.right(), rect.top(), rect.bottom());
    let Rounding { nw, ne, sw, se } = rounding;
    patch(egui::pos2(l, t), egui::pos2(l + nw, t + nw), nw, PI);
    patch(
        egui::pos2(r, t),
        egui::pos2(r - ne, t + ne),
        ne,
        PI + FRAC_PI_2,
    );
    patch(egui::pos2(r, b), egui::pos2(r - se, b - se), se, 0.0);
    patch(egui::pos2(l, b), egui::pos2(l + sw, b - sw), sw, FRAC_PI_2);
}
