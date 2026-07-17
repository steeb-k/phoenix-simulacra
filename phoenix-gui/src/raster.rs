//! The CPU rasterizer behind the softbuffer shell: egui's tessellated triangle
//! meshes in, packed `0x00RRGGBB` pixels out. No GPU is touched anywhere.
//!
//! This started as a straight per-pixel barycentric loop (see Diskoria, which
//! pioneered the GPU-free path for WinPE) but that runs a full set of
//! interpolations and a gamma blend for every candidate pixel of every
//! triangle — too slow to hold 60 fps on a large window, which made running
//! jobs' progress bars visibly chunky. The rewrite keeps the same math and
//! coverage rules but restructures the work:
//!
//! - **Per-triangle setup once.** Barycentric weights, UVs and vertex colors
//!   are all affine in (x, y); their coefficients are precomputed per triangle
//!   so the inner loops never re-derive them from vertices.
//! - **Exact row spans.** For each scanline, the covered pixels form one
//!   interval (each edge constraint is monotonic in x). The span is computed
//!   analytically and certified with the same per-pixel predicate the old
//!   loop used, so coverage is identical — but the interior needs no edge
//!   tests at all.
//! - **Shading fast paths.** Solid egui fills (flat color, constant UV) are
//!   `slice::fill`s when opaque and a constant-source blend otherwise; text
//!   (flat color, varying UV over the font atlas) only interpolates UV.
//!   Gradients and image textures take the full path.
//! - **Band parallelism.** The framebuffer is split into horizontal bands,
//!   one rayon task each. Bands own disjoint pixel rows, so triangles are
//!   painted in submission order within a band and there are no data races.
//!
//! Coordinates arrive in egui points and are scaled by `pixels_per_point`
//! here, exactly like the old loop.

use rayon::prelude::*;

use crate::tex_mgr::TextureManager;

/// Slack on the rasterizer's inside-triangle test, as a fraction of a
/// triangle's own size (the test runs on normalised barycentric weights).
///
/// egui antialiases by tessellating a solid core plus a 1px feathered ring
/// that share an edge, so on any shape edge landing on a whole pixel
/// coordinate — every panel, every button — that shared edge falls exactly
/// down the middle of a row of pixel centres. Exact arithmetic would put
/// those centres on both triangles; f32 puts them a few 1e-8 outside *both*,
/// and the pixel is dropped, letting the background dot through the fill.
/// Real geometry misses by ~1e-1, so anything in between separates float
/// noise from a genuine miss with orders of magnitude to spare.
const EDGE_EPS: f32 = 1e-5;

/// Alpha at or above which a source pixel just overwrites the destination.
/// Blending at a == 1 reproduces the source value anyway (modulo one LSB of
/// rounding); skipping the blend is where the opaque-fill fast path comes from.
const OPAQUE_A: f32 = 1.0 - 0.5 / 255.0;

/// Pack an opaque pixel the way softbuffer expects it on Windows: `0x00RRGGBB`
/// (the presenter ignores the top byte).
#[inline]
pub fn to_bgra(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | (b as u32)
}

/// `[a, b, c]` coefficients of an affine quantity `q(x, y) = a + b*x + c*y`
/// (evaluated at pixel centres, in physical pixels).
type Affine = [f32; 3];

#[inline]
fn eval(q: &Affine, fx: f32, fy: f32) -> f32 {
    q[0] + q[1] * fx + q[2] * fy
}

/// How a triangle's pixels get their color, dispatched once per triangle.
enum Shade {
    /// Flat color, constant UV, effectively opaque: `slice::fill`.
    Opaque(u32),
    /// Flat color, constant UV, translucent: blend a constant source.
    /// `pre` is the gamma-linearised source premultiplied by alpha, ready
    /// for `out = sqrt(pre + dst_lin * inv)`.
    Flat { pre: [f32; 3], inv: f32 },
    /// Flat color over the font atlas with varying UV — text. Only UV is
    /// interpolated; coverage comes from the atlas.
    Glyph {
        rgb: [f32; 3],
        va: f32,
        uv: [Affine; 2],
    },
    /// Everything else: gradient vertex colors and/or an image texture.
    General {
        /// r, g, b, a in 0..=255 space, affine per channel.
        col: [Affine; 4],
        uv: [Affine; 2],
        /// Font-atlas mesh (alpha coverage) vs image texture (RGBA modulate).
        font: bool,
        tex: egui::TextureId,
    },
}

struct Tri {
    /// Pixel bounding box, inclusive, already clipped to the primitive's clip
    /// rect and the framebuffer.
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    /// Affine coefficients of the three barycentric weights.
    e: [Affine; 3],
    shade: Shade,
}

/// Rasterize `clipped` into `pixels` (row-major, `w`×`h`, pre-sized by the
/// caller), clearing it to `bg` first.
pub fn rasterize(
    clipped: &[egui::ClippedPrimitive],
    tex: &TextureManager,
    w: u32,
    h: u32,
    ppp: f32,
    bg: u32,
    pixels: &mut [u32],
) {
    debug_assert_eq!(pixels.len(), (w * h) as usize);
    pixels.fill(bg);

    let tris = setup(clipped, tex, w, h, ppp);
    if tris.is_empty() {
        return;
    }

    // More bands than threads so a content-heavy strip doesn't serialize the
    // frame; each band walks the triangle list in submission order.
    let bands = (rayon::current_num_threads() * 4).max(1);
    let rows_per_band = ((h as usize) + bands - 1) / bands;
    if rows_per_band == 0 {
        return;
    }
    pixels
        .par_chunks_mut(rows_per_band * w as usize)
        .enumerate()
        .for_each(|(band_idx, band)| {
            let band_y0 = (band_idx * rows_per_band) as i32;
            let band_y1 = band_y0 + (band.len() / w as usize) as i32 - 1;
            for tri in &tris {
                if tri.y1 < band_y0 || tri.y0 > band_y1 {
                    continue;
                }
                shade_tri(tri, tex, band, band_y0, band_y1, w);
            }
        });
}

/// Build the per-triangle setup list: cull degenerate/empty triangles, derive
/// the affine coefficient sets, and pick each triangle's shading path.
fn setup(
    clipped: &[egui::ClippedPrimitive],
    tex: &TextureManager,
    w: u32,
    h: u32,
    ppp: f32,
) -> Vec<Tri> {
    let mut tris = Vec::new();
    for prim in clipped {
        let clip = prim.clip_rect;
        let clip_x0 = (clip.min.x * ppp).floor() as i32;
        let clip_y0 = (clip.min.y * ppp).floor() as i32;
        let clip_x1 = (clip.max.x * ppp).ceil() as i32;
        let clip_y1 = (clip.max.y * ppp).ceil() as i32;

        let egui::epaint::Primitive::Mesh(mesh) = &prim.primitive else {
            continue;
        };
        let verts = &mesh.vertices;
        let indices = &mesh.indices;
        let is_font_tex = mesh.texture_id == egui::TextureId::default();

        for tri in 0..indices.len() / 3 {
            let i0 = indices[tri * 3] as usize;
            let i1 = indices[tri * 3 + 1] as usize;
            let i2 = indices[tri * 3 + 2] as usize;
            if i0 >= verts.len() || i1 >= verts.len() || i2 >= verts.len() {
                continue;
            }
            let v0 = &verts[i0];
            let v1 = &verts[i1];
            let v2 = &verts[i2];

            let p0 = (v0.pos.x * ppp, v0.pos.y * ppp);
            let p1 = (v1.pos.x * ppp, v1.pos.y * ppp);
            let p2 = (v2.pos.x * ppp, v2.pos.y * ppp);

            let min_x = p0.0.min(p1.0).min(p2.0).floor() as i32;
            let min_y = p0.1.min(p1.1).min(p2.1).floor() as i32;
            let max_x = p0.0.max(p1.0).max(p2.0).ceil() as i32;
            let max_y = p0.1.max(p1.1).max(p2.1).ceil() as i32;

            let x0 = min_x.max(clip_x0).max(0);
            let y0 = min_y.max(clip_y0).max(0);
            let x1 = max_x.min(clip_x1).min(w as i32 - 1);
            let y1 = max_y.min(clip_y1).min(h as i32 - 1);
            if x0 > x1 || y0 > y1 {
                continue;
            }

            let denom = (p1.1 - p2.1) * (p0.0 - p2.0) + (p2.0 - p1.0) * (p0.1 - p2.1);
            if denom.abs() < 0.001 {
                continue;
            }

            // w0 and w1 as affine functions of the pixel centre; w2 = 1-w0-w1.
            let b0 = (p1.1 - p2.1) / denom;
            let c0 = (p2.0 - p1.0) / denom;
            let a0 = -(b0 * p2.0 + c0 * p2.1);
            let b1 = (p2.1 - p0.1) / denom;
            let c1 = (p0.0 - p2.0) / denom;
            let a1 = -(b1 * p2.0 + c1 * p2.1);
            let e = [
                [a0, b0, c0],
                [a1, b1, c1],
                [1.0 - a0 - a1, -b0 - b1, -c0 - c1],
            ];

            let affine_of = |q0: f32, q1: f32, q2: f32| -> Affine {
                [
                    q0 * e[0][0] + q1 * e[1][0] + q2 * e[2][0],
                    q0 * e[0][1] + q1 * e[1][1] + q2 * e[2][1],
                    q0 * e[0][2] + q1 * e[1][2] + q2 * e[2][2],
                ]
            };
            let uv = [
                affine_of(v0.uv.x, v1.uv.x, v2.uv.x),
                affine_of(v0.uv.y, v1.uv.y, v2.uv.y),
            ];

            let flat_color = v0.color == v1.color && v1.color == v2.color;
            let const_uv = v0.uv == v1.uv && v1.uv == v2.uv;

            let shade = if is_font_tex && flat_color && const_uv {
                // Solid fill: coverage is one constant atlas texel (egui's
                // white pixel for plain shapes — usually exactly 1.0).
                let cov = tex.sample_alpha_f(v0.uv.x, v0.uv.y);
                let a = (v0.color.a() as f32 / 255.0 * cov).clamp(0.0, 1.0);
                if a < 1.0 / 255.0 {
                    continue; // fully transparent: nothing to draw
                }
                let rgb = [
                    v0.color.r() as f32,
                    v0.color.g() as f32,
                    v0.color.b() as f32,
                ];
                if a >= OPAQUE_A {
                    Shade::Opaque(to_bgra(
                        (rgb[0] + 0.5) as u8,
                        (rgb[1] + 0.5) as u8,
                        (rgb[2] + 0.5) as u8,
                    ))
                } else {
                    Shade::Flat {
                        pre: [lin(rgb[0]) * a, lin(rgb[1]) * a, lin(rgb[2]) * a],
                        inv: 1.0 - a,
                    }
                }
            } else if is_font_tex && flat_color {
                Shade::Glyph {
                    rgb: [
                        v0.color.r() as f32,
                        v0.color.g() as f32,
                        v0.color.b() as f32,
                    ],
                    va: v0.color.a() as f32 / 255.0,
                    uv,
                }
            } else {
                Shade::General {
                    col: [
                        affine_of(
                            v0.color.r() as f32,
                            v1.color.r() as f32,
                            v2.color.r() as f32,
                        ),
                        affine_of(
                            v0.color.g() as f32,
                            v1.color.g() as f32,
                            v2.color.g() as f32,
                        ),
                        affine_of(
                            v0.color.b() as f32,
                            v1.color.b() as f32,
                            v2.color.b() as f32,
                        ),
                        affine_of(
                            v0.color.a() as f32,
                            v1.color.a() as f32,
                            v2.color.a() as f32,
                        ),
                    ],
                    uv,
                    font: is_font_tex,
                    tex: mesh.texture_id,
                }
            };

            tris.push(Tri {
                x0,
                y0,
                x1,
                y1,
                e,
                shade,
            });
        }
    }
    tris
}

/// Gamma-linearise one 0..=255 channel (gamma ≈ 2.0: cheap, matches the blend).
#[inline]
fn lin(c: f32) -> f32 {
    (c / 255.0) * (c / 255.0)
}

/// Gamma-correct blend of one channel: `src_pre` is the linearised source
/// already multiplied by alpha, `dst` the destination in 0..=255.
#[inline]
fn blend_channel(src_pre: f32, dst: f32, inv: f32) -> u8 {
    ((src_pre + lin(dst) * inv).sqrt() * 255.0 + 0.5) as u8
}

#[inline]
fn unpack(px: u32) -> (f32, f32, f32) {
    (
        ((px >> 16) & 0xFF) as f32,
        ((px >> 8) & 0xFF) as f32,
        (px & 0xFF) as f32,
    )
}

/// Blend a straight-alpha source over a packed destination pixel.
#[inline]
fn blend_px(dst: u32, r: f32, g: f32, b: f32, a: f32) -> u32 {
    let (dr, dg, db) = unpack(dst);
    let inv = 1.0 - a;
    to_bgra(
        blend_channel(lin(r) * a, dr, inv),
        blend_channel(lin(g) * a, dg, inv),
        blend_channel(lin(b) * a, db, inv),
    )
}

/// The covered pixels of `tri` on row `fy` form one interval (each barycentric
/// constraint is monotonic in x): compute it analytically, then certify the
/// endpoints with the exact per-pixel predicate so coverage matches a
/// pixel-by-pixel scan bit for bit.
#[inline]
fn row_span(tri: &Tri, fy: f32) -> Option<(i32, i32)> {
    // Per-row bases: w_k(x) = base_k + b_k * fx.
    let base = [
        tri.e[0][0] + tri.e[0][2] * fy,
        tri.e[1][0] + tri.e[1][2] * fy,
        tri.e[2][0] + tri.e[2][2] * fy,
    ];
    let mut lo = tri.x0 as f32;
    let mut hi = tri.x1 as f32;
    for k in 0..3 {
        let b = tri.e[k][1];
        if b == 0.0 {
            if base[k] < -EDGE_EPS {
                return None;
            }
        } else {
            // Boundary pixel-centre x of w_k = -EDGE_EPS, ±1 px of slack for
            // the division's rounding; the exact scans below take up the rest.
            let x = (-EDGE_EPS - base[k]) / b - 0.5;
            if b > 0.0 {
                lo = lo.max(x - 1.0);
            } else {
                hi = hi.min(x + 1.0);
            }
        }
    }
    let mut lo = (lo.floor() as i32).max(tri.x0);
    let mut hi = (hi.ceil() as i32).min(tri.x1);
    let inside = |x: i32| {
        let fx = x as f32 + 0.5;
        base[0] + tri.e[0][1] * fx >= -EDGE_EPS
            && base[1] + tri.e[1][1] * fx >= -EDGE_EPS
            && base[2] + tri.e[2][1] * fx >= -EDGE_EPS
    };
    while lo <= hi && !inside(lo) {
        lo += 1;
    }
    while hi >= lo && !inside(hi) {
        hi -= 1;
    }
    (lo <= hi).then_some((lo, hi))
}

/// Paint one triangle into the rows of `band` (absolute rows `band_y0..=band_y1`).
fn shade_tri(
    tri: &Tri,
    tex: &TextureManager,
    band: &mut [u32],
    band_y0: i32,
    band_y1: i32,
    w: u32,
) {
    let y_lo = tri.y0.max(band_y0);
    let y_hi = tri.y1.min(band_y1);
    for py in y_lo..=y_hi {
        let fy = py as f32 + 0.5;
        let Some((lo, hi)) = row_span(tri, fy) else {
            continue;
        };
        let row_off = ((py - band_y0) as u32 * w) as usize;
        let row = &mut band[row_off + lo as usize..=row_off + hi as usize];

        match &tri.shade {
            Shade::Opaque(px) => row.fill(*px),
            Shade::Flat { pre, inv } => {
                for dst in row.iter_mut() {
                    let (dr, dg, db) = unpack(*dst);
                    *dst = to_bgra(
                        blend_channel(pre[0], dr, *inv),
                        blend_channel(pre[1], dg, *inv),
                        blend_channel(pre[2], db, *inv),
                    );
                }
            }
            Shade::Glyph { rgb, va, uv } => {
                let mut fx = lo as f32 + 0.5;
                for dst in row.iter_mut() {
                    let cov = tex.sample_alpha_f(eval(&uv[0], fx, fy), eval(&uv[1], fx, fy));
                    let a = (*va * cov).clamp(0.0, 1.0);
                    if a >= 1.0 / 255.0 {
                        *dst = if a >= OPAQUE_A {
                            to_bgra(
                                (rgb[0] + 0.5) as u8,
                                (rgb[1] + 0.5) as u8,
                                (rgb[2] + 0.5) as u8,
                            )
                        } else {
                            blend_px(*dst, rgb[0], rgb[1], rgb[2], a)
                        };
                    }
                    fx += 1.0;
                }
            }
            Shade::General {
                col,
                uv,
                font,
                tex: tex_id,
            } => {
                let mut fx = lo as f32 + 0.5;
                for dst in row.iter_mut() {
                    let uv_x = eval(&uv[0], fx, fy);
                    let uv_y = eval(&uv[1], fx, fy);
                    let vr = eval(&col[0], fx, fy);
                    let vg = eval(&col[1], fx, fy);
                    let vb = eval(&col[2], fx, fy);
                    let va = eval(&col[3], fx, fy);
                    let (r, g, b, a) = if *font {
                        let cov = tex.sample_alpha_f(uv_x, uv_y);
                        (vr, vg, vb, va / 255.0 * cov)
                    } else {
                        let [tr, tg, tb, ta] = tex.sample_rgba(*tex_id, uv_x, uv_y);
                        (tr * vr, tg * vg, tb * vb, ta * va / 255.0)
                    };
                    // Interpolated alpha can land a hair outside [0, 1] on the
                    // EDGE_EPS ring; unclamped it would sqrt() a negative and
                    // paint a black speck.
                    let a = a.clamp(0.0, 1.0);
                    if a >= 1.0 / 255.0 {
                        *dst = if a >= OPAQUE_A {
                            to_bgra(
                                (r + 0.5).clamp(0.0, 255.0) as u8,
                                (g + 0.5).clamp(0.0, 255.0) as u8,
                                (b + 0.5).clamp(0.0, 255.0) as u8,
                            )
                        } else {
                            blend_px(*dst, r, g, b, a)
                        };
                    }
                    fx += 1.0;
                }
            }
        }
    }
}
