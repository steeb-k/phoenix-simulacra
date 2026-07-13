//! Sticky full-width primary-action bar pinned to the bottom of the
//! central pane.
//!
//! Every page with a Start action (Backup, Restore, Verify, Clone, Mount)
//! renders it here instead of at the end of its scrollable content:
//! the page scrolls, the action never moves. The bar is flush with the
//! pane edges and carries the pane's rounded bottom-left corner, so it
//! reads as the bottom cap of the central panel rather than a floating
//! control.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use eframe::egui;
use egui::{Color32, Mesh, Pos2, Rect, Rounding, Sense, Shape, Stroke};

use crate::stripes;
use crate::theme::{tint, Palette};
use crate::{fonts, icon_label, StartAction, START_BUTTON_HEIGHT};

/// Radius of the pane's bottom-left corner. Must match `panel_rounding`
/// in `PhoenixApp::update` — the bar takes the cutout over from the
/// central panel whenever it is shown.
pub const CORNER_RADIUS: f32 = 10.0;

/// Target edge length of one mosaic tile. The grid rounds this to a whole
/// number of rows across the bar's height (three, at `START_BUTTON_HEIGHT`),
/// so it never ends in a sliver row.
const TILE: f32 = 17.5;

/// Scramble rate, in shade changes per tile per second. Each tile eases
/// continuously from its current shade to its next one over the whole
/// interval — there is no hold, so the mosaic drifts rather than ticking.
const SCRAMBLE_HZ: f32 = 2.0;

/// Number of distinct shades a tile can take.
const SHADES: u32 = 8;

/// Halo drawn behind the label on the mosaic: the text stamped in soft black
/// at each of these offsets, so a letter reads the same whichever shade of
/// green a scramble parks under it. Four diagonals for the outline, plus a
/// slightly lower one to weight it as a drop shadow.
const SHADOW: Color32 = Color32::from_black_alpha(70);
const HALO: [egui::Vec2; 5] = [
    egui::Vec2::new(-1.0, -1.0),
    egui::Vec2::new(1.0, -1.0),
    egui::Vec2::new(-1.0, 1.0),
    egui::Vec2::new(1.0, 1.0),
    egui::Vec2::new(0.0, 2.0),
];

/// Frame time (≈30fps) above which the machine is judged unable to carry
/// the continuous shimmer, and this many consecutive slow frames before we
/// believe it. See [`reduced_motion`].
const SLOW_FRAME_SECS: f32 = 1.0 / 30.0;
const SLOW_FRAMES_TO_DEGRADE: u32 = 12;

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
                // Mosaic of green tiles over a vertical gradient: lit at
                // the top, settling darker at the bottom. Hover lifts the
                // whole surface; press flattens it dark.
                let base = palette.success;
                let hovered = interactable && response.hovered();
                let (top, bottom) = if interactable && response.is_pointer_button_down_on() {
                    (
                        tint(base, 0.16, Color32::BLACK),
                        tint(base, 0.04, Color32::BLACK),
                    )
                } else if hovered {
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
                // Hovering scrambles the tiles: the shade pattern walks to
                // a fresh one for as long as the pointer stays on the bar.
                let phase = advance_scramble(ctx, hovered);
                paint_tile_grid(ui.painter(), rect, rounding, palette, top, bottom, phase);

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
            let galley = ui.fonts(|f| f.layout_job(label_job(action, text_color)));
            let pos = rect.center() - galley.size() * 0.5;
            if action.enabled {
                // The mosaic puts tiles of several different greens behind
                // the label, and a scramble keeps changing which one lands
                // under any given letter. Ring the text in soft black first
                // so its contrast never depends on the tile that happens to
                // be under it.
                let shadow = ui.fonts(|f| f.layout_job(label_job(action, SHADOW)));
                for offset in HALO {
                    ui.painter().galley(pos + offset, shadow.clone(), SHADOW);
                }
            }
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

/// The bar's icon + label, laid out in `color`. Built twice per frame on the
/// mosaic — once for the halo, once for the text itself — since a galley
/// bakes in the color it was laid out with.
fn label_job(action: &StartAction<'_>, color: Color32) -> egui::text::LayoutJob {
    match action.icon {
        Some(icon) => icon_label(
            icon,
            fonts::icon_fill(30.0),
            action.label,
            fonts::bold(24.0),
            color,
        ),
        None => {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                action.label,
                0.0,
                egui::TextFormat {
                    font_id: fonts::bold(24.0),
                    color,
                    ..Default::default()
                },
            );
            job
        }
    }
}

/// How far the mosaic has walked along the scramble clock. One whole unit is
/// one shade change for every tile (each staggered by its own [`tile_offset`],
/// so they do not change together). It only ever moves forward, so "settling"
/// means coasting to the next whole unit — one last complete fade — and
/// stopping there.
#[derive(Clone, Copy, Default)]
struct Scramble {
    phase: f32,
    /// Phase to coast to when we are *not* free-running under the pointer.
    /// Once `phase` reaches it, the bar is at rest and stops repainting.
    target: f32,
    hovered: bool,
    /// Consecutive slow frames seen while free-running. See [`reduced_motion`].
    slow_frames: u32,
}

/// Step the scramble for this frame and return the phase to paint at.
///
/// While the pointer is on the bar the phase free-runs, which means asking
/// for a repaint every frame — the only continuous animation in the app, and
/// the reason for the [`reduced_motion`] escape hatch. Off the pointer it
/// coasts to the next whole pattern and then goes quiet: an unhovered bar
/// costs exactly as many frames as the old static gradient did (none).
fn advance_scramble(ctx: &egui::Context, hovered: bool) -> f32 {
    let id = egui::Id::new("action_bar_scramble");
    let mut s: Scramble = ctx.data_mut(|d| d.get_temp(id).unwrap_or_default());
    // Clamp: the first frame after an idle stretch carries a long dt, which
    // would otherwise teleport the mosaic several patterns forward.
    let dt = ctx.input(|i| i.stable_dt).min(0.1);

    // Under reduced motion, hovering is a single reshuffle rather than a
    // continuous one — and so is un-hovering, so the bar still acknowledges
    // the pointer coming and going.
    let free_running = hovered && !reduced_motion();
    if hovered != s.hovered {
        s.hovered = hovered;
        s.target = s.phase.floor() + 1.0;
    }

    let animating = if free_running {
        s.phase += dt * SCRAMBLE_HZ;
        s.target = s.phase;
        true
    } else {
        s.phase = (s.phase + dt * SCRAMBLE_HZ).min(s.target);
        s.phase < s.target
    };

    // Self-degrade: if the shimmer itself can't hold a reasonable frame rate
    // (software rasterizer under WinPE, most likely), stop asking it to and
    // fall back to the one-shot reshuffle for the rest of the session.
    if free_running && dt > SLOW_FRAME_SECS {
        s.slow_frames += 1;
        if s.slow_frames >= SLOW_FRAMES_TO_DEGRADE {
            DEGRADED.store(true, Ordering::Relaxed);
        }
    } else {
        s.slow_frames = 0;
    }

    ctx.data_mut(|d| d.insert_temp(id, s));
    if animating {
        ctx.request_repaint();
    }
    s.phase
}

/// True when the mosaic must not shimmer continuously: either the user
/// passed `--reduced-motion`, or the shimmer was measured to be too slow on
/// this machine and disabled itself (see [`advance_scramble`]). Hover then
/// costs one short crossfade instead of a repaint per frame.
fn reduced_motion() -> bool {
    static FORCED: OnceLock<bool> = OnceLock::new();
    *FORCED.get_or_init(|| std::env::args().skip(1).any(|a| a == "--reduced-motion"))
        || DEGRADED.load(Ordering::Relaxed)
}

static DEGRADED: AtomicBool = AtomicBool::new(false);

/// Enabled-state fill: a grid of green tiles, each a slightly different
/// shade of the `top`→`bottom` gradient it is cut from — so the bar still
/// reads as one lit surface, with the mosaic as its texture rather than as
/// stripes of unrelated color.
///
/// Every tile's shade is a pure function of its column, its row, and the
/// pattern seed, so nothing is stored per tile and no randomness is drawn at
/// paint time; `phase` crossfades tile-by-tile between seed N and seed N+1,
/// which is what makes a scramble out of two still patterns.
///
/// Like the hazard tape, the tiles are painted square and clipped, so the
/// pane's rounded bottom-left corner is patched back in afterwards.
fn paint_tile_grid(
    painter: &egui::Painter,
    rect: Rect,
    rounding: Rounding,
    palette: &Palette,
    top: Color32,
    bottom: Color32,
    phase: f32,
) {
    let rows = (rect.height() / TILE).round().max(1.0);
    let cell = rect.height() / rows;
    let cols = (rect.width() / cell).ceil().max(1.0) as u32;
    let rows = rows as u32;

    let mut mesh = Mesh::default();
    for row in 0..rows {
        // Sample the gradient at the row's midpoint: the tiles quantize the
        // vertical fade instead of erasing it.
        let base = lerp_color(top, bottom, (row as f32 + 0.5) / rows as f32);
        for col in 0..cols {
            // Each tile runs the same clock from its own starting point, so
            // the grid is always part-way between shades in a hundred
            // different places instead of flipping over all at once.
            let local = phase + tile_offset(col, row);
            let seed = local.floor();
            let t = smoothstep(local - seed);
            let seed = seed as u32;

            let from = tile_shade(base, shade_of(col, row, seed), palette.light_mode);
            let to = tile_shade(base, shade_of(col, row, seed + 1), palette.light_mode);
            let x = rect.left() + col as f32 * cell;
            let y = rect.top() + row as f32 * cell;
            mesh.add_colored_rect(
                Rect::from_min_max(
                    egui::pos2(x, y),
                    egui::pos2((x + cell).min(rect.right()), y + cell),
                ),
                lerp_color(from, to, t),
            );
        }
    }
    painter.with_clip_rect(rect).add(Shape::mesh(mesh));

    stripes::patch_rounded_corners(painter, rect, rounding, palette.sidebar_bg);
}

/// A tile's fixed head start into the scramble clock, in [0, 1). Staggering
/// the tiles is what turns the animation from a whole-field flip into a
/// drift: at any instant every tile is at a different point of its own fade.
fn tile_offset(col: u32, row: u32) -> f32 {
    (hash(col, row, 0x5EED) % 1024) as f32 / 1024.0
}

/// Shade index (0..[`SHADES`]) for one tile of pattern `seed` — an integer
/// hash, so the "random" mosaic is fixed and reproducible rather than
/// generated, and adjacent tiles land on unrelated shades.
fn shade_of(col: u32, row: u32, seed: u32) -> u32 {
    hash(col, row, seed) % SHADES
}

/// Integer hash of a tile coordinate and a seed. Cheap, and good enough that
/// neighbouring tiles land on unrelated values.
fn hash(col: u32, row: u32, seed: u32) -> u32 {
    let mut h = col.wrapping_mul(0x9E37_79B9)
        ^ row.wrapping_mul(0x85EB_CA6B)
        ^ seed.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2545_F491);
    h ^= h >> 13;
    h
}

/// Nudge `base` to the shade at `index`. The steps are deliberately small —
/// the mosaic should read as a texture in the surface, not as a checkerboard
/// painted on top of it — and the whole ladder is biased darker in dark mode
/// and lighter in light mode.
fn tile_shade(base: Color32, index: u32, light_mode: bool) -> Color32 {
    const STEP: f32 = 0.035;
    const BIAS: f32 = 0.045;
    let ladder = index as f32 - (SHADES - 1) as f32 / 2.0;
    let amount = ladder * STEP + if light_mode { BIAS } else { -BIAS };
    if amount >= 0.0 {
        tint(base, amount, Color32::WHITE)
    } else {
        tint(base, -amount, Color32::BLACK)
    }
}

/// Ease the crossfade between two patterns so tiles arrive and leave softly
/// instead of ticking linearly from shade to shade.
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Disabled-state fill: the app's diagonal stripes (see [`crate::stripes`]) in
/// muted monochrome — black/gray in dark mode, white/light-gray in light mode —
/// so the bar signals "blocked until the form is complete" without borrowing
/// the destructive dialogs' yellow, and without the progress bar's motion.
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

    // A fixed stripe width rather than the dialog tape's height-wide stripes,
    // which would turn chunky on a 52px bar.
    const STRIPE_W: f32 = 22.0;
    stripes::paint(painter, rect, stripe, STRIPE_W, 0.0);

    // The dialogs' tape sheen, toned down: a faint top fade for depth
    // (dark mode only — on white tape it just looks like dirt).
    if !palette.light_mode {
        stripes::sheen(painter, rect, 50);
    }

    stripes::patch_rounded_corners(painter, rect, rounding, palette.sidebar_bg);
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
    corner(
        &mut pts,
        l + rounding.nw,
        t + rounding.nw,
        rounding.nw,
        PI,
        1.5 * PI,
    );
    corner(
        &mut pts,
        r - rounding.ne,
        t + rounding.ne,
        rounding.ne,
        1.5 * PI,
        TAU,
    );
    corner(
        &mut pts,
        r - rounding.se,
        b - rounding.se,
        rounding.se,
        0.0,
        FRAC_PI_2,
    );
    corner(
        &mut pts,
        l + rounding.sw,
        b - rounding.sw,
        rounding.sw,
        FRAC_PI_2,
        PI,
    );
    pts
}

/// Per-channel sRGB lerp (same "paint program" tradeoff as [`tint`]).
fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}
