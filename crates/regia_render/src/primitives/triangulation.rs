// SPDX-License-Identifier: MIT
// Portions ported from Piston graphics (https://github.com/PistonDevelopers/graphics, commit 13dadef),
// src/triangulation.rs. Copyright (c) 2014 Sven Nilsen; MIT licence
// (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! Allocation-free 2D shape fast paths: shapes stream into fixed-size triangle-list chunks.
//!
//! Each `with_*` / `stream_*` function walks a shape's outline, transforms it by an [`Affine2`]
//! and hands the resulting triangle list (three `[x, y]` points per triangle) to a callback in
//! chunks of at most [`CHUNK_VERTICES`] vertices, so ten thousand circles or rounded rectangles
//! a frame never touch the heap. Convex outlines only (a triangle fan from the first point);
//! general paths go through a tessellator instead.
//!
//! Rectangles are `[x, y, w, h]` with `(x, y)` the minimum corner, lines are `[x1, y1, x2, y2]`,
//! angles are radians, `resolution` counts segments.

use regia_core::math::{Affine2, Vec2};

/// Vertices per streamed chunk (a multiple of 3 fits: 341 triangles). Sized so a chunk of
/// `[f32; 2]` stays inside L1 (8 KiB).
pub const CHUNK_VERTICES: usize = 1023;

const TAU: f32 = std::f32::consts::TAU;
const HALF_PI: f32 = std::f32::consts::FRAC_PI_2;
const PI: f32 = std::f32::consts::PI;

/// `m` applied to `(x, y)`.
#[inline(always)]
fn xf(m: &Affine2, x: f32, y: f32) -> [f32; 2] {
    m.transform_point2(Vec2::new(x, y)).to_array()
}

/// The rotation that maps +X onto `(dx, dy)` (identity for a zero vector).
fn orient(dx: f32, dy: f32) -> Affine2 {
    let len = (dx * dx + dy * dy).sqrt();
    if len == 0.0 {
        return Affine2::IDENTITY;
    }
    Affine2::from_cols_array(&[dx / len, dy / len, -dy / len, dx / len, 0.0, 0.0])
}

/// Streams a polygon tweened between `polygons` (all of equal length) by `tween_factor`
/// (wrapping: `0` is the first polygon, `1` back to the first again).
pub fn with_lerp_polygons_tri_list<F>(m: Affine2, polygons: &[&[[f32; 2]]], tween_factor: f32, f: F)
where
    F: FnMut(&[[f32; 2]]),
{
    let Some(first) = polygons.first() else { return };
    let poly_len = polygons.len() as f32;
    let tw = tween_factor % 1.0;
    let tw = if tw < 0.0 { tw + 1.0 } else { tw };
    let tw = tw * poly_len;
    let frame = (tw as usize).min(polygons.len() - 1);
    let next_frame = (frame + 1) % polygons.len();
    let (p0, p1) = (polygons[frame], polygons[next_frame]);
    let t = tw - frame as f32;
    let n = first.len().min(p0.len()).min(p1.len());
    stream_polygon_tri_list(
        m,
        (0..n).map(|j| {
            [
                p0[j][0] + (p1[j][0] - p0[j][0]) * t,
                p0[j][1] + (p1[j][1] - p0[j][1]) * t,
            ]
        }),
        f,
    );
}

/// Streams a filled ellipse inscribed in `rect` with `resolution` segments.
pub fn with_ellipse_tri_list<F>(resolution: u32, m: Affine2, rect: [f32; 4], f: F)
where
    F: FnMut(&[[f32; 2]]),
{
    let [x, y, w, h] = rect;
    let (cw, ch) = (0.5 * w, 0.5 * h);
    let (cx, cy) = (x + cw, y + ch);
    let n = resolution.max(3);
    stream_polygon_tri_list(
        m,
        (0..n).map(|i| {
            let angle = i as f32 / n as f32 * TAU;
            [cx + angle.cos() * cw, cy + angle.sin() * ch]
        }),
        f,
    );
}

/// Streams a line with round caps: a capsule of `radius` around the segment, `resolution_cap`
/// points per half circle.
pub fn with_round_border_line_tri_list<F>(resolution_cap: u32, m: Affine2, line: [f32; 4], radius: f32, f: F)
where
    F: FnMut(&[[f32; 2]]),
{
    let cap = resolution_cap.max(2);
    let [x1, y1, x2, y2] = line;
    let (dx, dy) = (x2 - x1, y2 - y1);
    let w = (dx * dx + dy * dy).sqrt();
    let m = m * Affine2::from_translation(Vec2::new(x1, y1)) * orient(dx, dy);
    let n = cap * 2;
    stream_polygon_tri_list(
        m,
        (0..n).map(|j| {
            // One half circle at each end of the (horizontal, local) line; together a full circle
            // when the line has zero length.
            if j >= cap {
                let angle = (j - cap) as f32 / (cap - 1) as f32 * PI + PI + HALF_PI;
                [w + angle.cos() * radius, angle.sin() * radius]
            } else {
                let angle = j as f32 / (cap - 1) as f32 * PI + HALF_PI;
                [angle.cos() * radius, angle.sin() * radius]
            }
        }),
        f,
    );
}

/// The point of quarter circle `quarter` (0..4) at step `k` of `res` around the rounded corner
/// of `rect`, offset outwards from the corner centre by `radius`.
#[inline(always)]
fn corner_point(rect: [f32; 4], corner_radius: f32, res: u32, quarter: u32, k: u32, radius: f32) -> [f32; 2] {
    let [x, y, w, h] = rect;
    let r = corner_radius;
    let angle = k as f32 / (res - 1) as f32 * HALF_PI + quarter as f32 * HALF_PI;
    let (cx, cy) = match quarter {
        0 => (x + w - r, y + h - r),
        1 => (x + r, y + h - r),
        2 => (x + r, y + r),
        _ => (x + w - r, y + r),
    };
    [cx + angle.cos() * radius, cy + angle.sin() * radius]
}

/// Streams a filled rectangle with rounded corners (`round_radius`, `resolution_corner` points
/// per quarter circle).
pub fn with_round_rectangle_tri_list<F>(
    resolution_corner: u32,
    m: Affine2,
    rect: [f32; 4],
    round_radius: f32,
    f: F,
) where
    F: FnMut(&[[f32; 2]]),
{
    let res = resolution_corner.max(2);
    let n = res * 4;
    stream_polygon_tri_list(
        m,
        (0..n).map(|j| corner_point(rect, round_radius, res, j / res, j % res, round_radius)),
        f,
    );
}

/// Streams a convex polygon as a triangle fan from its first point.
///
/// `polygon` yields the outline; `f` receives the triangle list chunk by chunk, each chunk
/// `[[x0, y0], [x1, y1], [x2, y2], ...]` with three points per triangle and at most
/// [`CHUNK_VERTICES`] points. Fewer than three points draw nothing.
pub fn stream_polygon_tri_list<E, F>(m: Affine2, mut polygon: E, mut f: F)
where
    E: Iterator<Item = [f32; 2]>,
    F: FnMut(&[[f32; 2]]),
{
    let mut vertices = [[0.0f32; 2]; CHUNK_VERTICES];
    let Some(fp) = polygon.next() else { return };
    let f1 = xf(&m, fp[0], fp[1]);
    let Some(gp) = polygon.next() else { return };
    let mut g1 = xf(&m, gp[0], gp[1]);
    let mut i = 0;
    for p in polygon {
        let pos = xf(&m, p[0], p[1]);
        let out = i * 3;
        vertices[out] = f1;
        vertices[out + 1] = g1;
        vertices[out + 2] = pos;
        g1 = pos;
        i += 1;
        if (i + 1) * 3 > CHUNK_VERTICES {
            f(&vertices[..i * 3]);
            i = 0;
        }
    }
    if i > 0 {
        f(&vertices[..i * 3]);
    }
}

/// Streams an ellipse outline of half-width `border_radius` (centred on the ellipse inscribed in
/// `rect`).
pub fn with_ellipse_border_tri_list<F>(resolution: u32, m: Affine2, rect: [f32; 4], border_radius: f32, f: F)
where
    F: FnMut(&[[f32; 2]]),
{
    let [x, y, w, h] = rect;
    let (cw, ch) = (0.5 * w, 0.5 * h);
    let (cw1, ch1) = (cw + border_radius, ch + border_radius);
    let (cw2, ch2) = (cw - border_radius, ch - border_radius);
    let (cx, cy) = (x + cw, y + ch);
    let n = resolution.max(3);
    let mut i = 0;
    stream_quad_tri_list(
        m,
        || {
            if i > n {
                return None;
            }
            let angle = i as f32 / n as f32 * TAU;
            let (sin, cos) = angle.sin_cos();
            i += 1;
            Some(([cx + cos * cw1, cy + sin * ch1], [cx + cos * cw2, cy + sin * ch2]))
        },
        f,
    );
}

/// Streams an arc of the ellipse inscribed in `rect` from `start_radians` to `end_radians`, as a
/// band of half-width `border_radius`. `resolution` is the segment count of a full turn (a lower
/// bound: the arc is split into equal segments no longer than that).
pub fn with_arc_tri_list<F>(
    start_radians: f32,
    end_radians: f32,
    resolution: u32,
    m: Affine2,
    rect: [f32; 4],
    border_radius: f32,
    f: F,
) where
    F: FnMut(&[[f32; 2]]),
{
    let [x, y, w, h] = rect;
    let (cw, ch) = (0.5 * w, 0.5 * h);
    let (cw1, ch1) = (cw + border_radius, ch + border_radius);
    let (cw2, ch2) = (cw - border_radius, ch - border_radius);
    let (cx, cy) = (x + cw, y + ch);
    let max_seg_size = TAU / resolution.max(1) as f32;
    let (start_radians, delta) = if (end_radians - start_radians).abs() >= TAU {
        (0.0, TAU)
    } else {
        (start_radians, ((end_radians - start_radians) % TAU + TAU) % TAU)
    };
    // Ceiling: `resolution` is a lower bound on the drawn resolution.
    let n_quads = (delta / max_seg_size).ceil().max(1.0) as u32;
    let seg_size = delta / n_quads as f32;
    let mut i = 0;
    stream_quad_tri_list(
        m,
        || {
            if i > n_quads {
                return None;
            }
            let angle = start_radians + i as f32 * seg_size;
            let (sin, cos) = angle.sin_cos();
            i += 1;
            Some(([cx + cos * cw1, cy + sin * ch1], [cx + cos * cw2, cy + sin * ch2]))
        },
        f,
    );
}

/// Streams the outline (half-width `border_radius`) of a rectangle with rounded corners.
pub fn with_round_rectangle_border_tri_list<F>(
    resolution_corner: u32,
    m: Affine2,
    rect: [f32; 4],
    round_radius: f32,
    border_radius: f32,
    f: F,
) where
    F: FnMut(&[[f32; 2]]),
{
    let res = resolution_corner.max(2);
    let radius1 = round_radius + border_radius;
    let radius2 = round_radius - border_radius;
    let n = res * 4;
    let mut i = 0;
    stream_quad_tri_list(
        m,
        || {
            if i > n {
                return None;
            }
            let j = i;
            i += 1;
            if j == n {
                // Close the loop at the first corner's start.
                let [x, y, w, h] = rect;
                let (cx, cy) = (x + w - round_radius, y + h - round_radius);
                return Some(([cx + radius1, cy], [cx + radius2, cy]));
            }
            let (quarter, k) = (j / res, j % res);
            Some((
                corner_point(rect, round_radius, res, quarter, k, radius1),
                corner_point(rect, round_radius, res, quarter, k, radius2),
            ))
        },
        f,
    );
}

/// Streams a strip of quads: `quad_edge` yields successive edges `(outer, inner)` and every two
/// consecutive edges make one quad (two triangles). Chunks as [`stream_polygon_tri_list`].
pub fn stream_quad_tri_list<E, F>(m: Affine2, mut quad_edge: E, mut f: F)
where
    E: FnMut() -> Option<([f32; 2], [f32; 2])>,
    F: FnMut(&[[f32; 2]]),
{
    let mut vertices = [[0.0f32; 2]; CHUNK_VERTICES];
    let Some((fp1, fp2)) = quad_edge() else { return };
    let mut f1 = xf(&m, fp1[0], fp1[1]);
    let mut f2 = xf(&m, fp2[0], fp2[1]);
    let mut i = 0;
    while let Some((gp1, gp2)) = quad_edge() {
        let g1 = xf(&m, gp1[0], gp1[1]);
        let g2 = xf(&m, gp2[0], gp2[1]);
        let out = i * 6;
        vertices[out] = f1;
        vertices[out + 1] = f2;
        vertices[out + 2] = g1;
        vertices[out + 3] = f2;
        vertices[out + 4] = g1;
        vertices[out + 5] = g2;
        i += 1;
        f1 = g1;
        f2 = g2;
        if (i + 1) * 6 > CHUNK_VERTICES {
            f(&vertices[..i * 6]);
            i = 0;
        }
    }
    if i > 0 {
        f(&vertices[..i * 6]);
    }
}

/// Streams a convex polygon given as a slice (see [`stream_polygon_tri_list`]).
pub fn with_polygon_tri_list<F>(m: Affine2, polygon: &[[f32; 2]], f: F)
where
    F: FnMut(&[[f32; 2]]),
{
    stream_polygon_tri_list(m, polygon.iter().copied(), f);
}

/// The two triangles of a rectangle.
#[must_use]
pub fn rect_tri_list_xy(m: Affine2, rect: [f32; 4]) -> [[f32; 2]; 6] {
    let [x, y, w, h] = rect;
    let (x2, y2) = (x + w, y + h);
    [
        xf(&m, x, y),
        xf(&m, x2, y),
        xf(&m, x, y2),
        xf(&m, x2, y),
        xf(&m, x2, y2),
        xf(&m, x, y2),
    ]
}

/// The eight triangles of a rectangle's outline of half-width `border_radius` (centred on the
/// rectangle's edges).
#[must_use]
pub fn rect_border_tri_list_xy(m: Affine2, rect: [f32; 4], border_radius: f32) -> [[f32; 2]; 24] {
    let [x, y, w, h] = rect;
    let (w1, h1) = (w + border_radius, h + border_radius);
    let (w2, h2) = (w - border_radius, h - border_radius);
    let (x11, y11) = (x - border_radius, y - border_radius);
    let (x21, y21) = (x + border_radius, y + border_radius);
    let (x12, y12) = (x + w1, y + h1);
    let (x22, y22) = (x + w2, y + h2);
    [
        xf(&m, x11, y11),
        xf(&m, x12, y11),
        xf(&m, x21, y21),
        xf(&m, x21, y21),
        xf(&m, x12, y11),
        xf(&m, x22, y21),
        xf(&m, x22, y21),
        xf(&m, x12, y11),
        xf(&m, x12, y12),
        xf(&m, x22, y21),
        xf(&m, x12, y12),
        xf(&m, x22, y22),
        xf(&m, x12, y12),
        xf(&m, x22, y22),
        xf(&m, x11, y12),
        xf(&m, x22, y22),
        xf(&m, x11, y12),
        xf(&m, x21, y22),
        xf(&m, x11, y12),
        xf(&m, x21, y21),
        xf(&m, x21, y22),
        xf(&m, x11, y12),
        xf(&m, x11, y11),
        xf(&m, x21, y21),
    ]
}

/// Texture coordinates matching [`rect_tri_list_xy`] for the `source_rect` (pixels,
/// `[x, y, w, h]`) of an image of `image_size` pixels.
#[must_use]
pub fn rect_tri_list_uv(image_size: [u32; 2], source_rect: [f32; 4]) -> [[f32; 2]; 6] {
    let (w, h) = (image_size[0].max(1) as f32, image_size[1].max(1) as f32);
    let [sx, sy, sw, sh] = source_rect;
    let (x1, y1) = (sx / w, sy / h);
    let (x2, y2) = ((sx + sw) / w, (sy + sh) / h);
    [[x1, y1], [x2, y1], [x1, y2], [x2, y1], [x2, y2], [x1, y2]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(run: impl FnOnce(&mut dyn FnMut(&[[f32; 2]]))) -> (Vec<[f32; 2]>, usize) {
        let mut all = Vec::new();
        let mut chunks = 0;
        run(&mut |chunk: &[[f32; 2]]| {
            assert!(chunk.len() <= CHUNK_VERTICES);
            assert_eq!(chunk.len() % 3, 0);
            all.extend_from_slice(chunk);
            chunks += 1;
        });
        (all, chunks)
    }

    fn area(tris: &[[f32; 2]]) -> f32 {
        tris.as_chunks::<3>()
            .0
            .iter()
            .map(|t| {
                0.5 * ((t[1][0] - t[0][0]) * (t[2][1] - t[0][1]) - (t[2][0] - t[0][0]) * (t[1][1] - t[0][1]))
                    .abs()
            })
            .sum()
    }

    #[test]
    fn ellipse_fan_covers_the_ellipse() {
        let (tris, _) = collect(|f| with_ellipse_tri_list(256, Affine2::IDENTITY, [0.0, 0.0, 20.0, 10.0], f));
        assert_eq!(tris.len(), 254 * 3);
        let exact = PI * 10.0 * 5.0;
        assert!((area(&tris) - exact).abs() / exact < 1e-3);
    }

    #[test]
    fn large_polygons_stream_in_chunks() {
        let (tris, chunks) =
            collect(|f| with_ellipse_tri_list(4000, Affine2::IDENTITY, [0.0, 0.0, 2.0, 2.0], f));
        assert_eq!(tris.len(), 3998 * 3);
        assert!(chunks >= 12, "{chunks} chunks");
    }

    #[test]
    fn transform_applies_to_every_point() {
        let m = Affine2::from_translation(Vec2::new(100.0, 50.0));
        let rect = rect_tri_list_xy(m, [0.0, 0.0, 2.0, 4.0]);
        assert_eq!(rect[0], [100.0, 50.0]);
        assert_eq!(rect[4], [102.0, 54.0]);
        assert_eq!(area(&rect), 8.0);
    }

    #[test]
    fn rounded_rectangle_area() {
        let r = 3.0;
        let (tris, _) =
            collect(|f| with_round_rectangle_tri_list(64, Affine2::IDENTITY, [0.0, 0.0, 20.0, 10.0], r, f));
        let exact = 20.0 * 10.0 - (4.0 - PI) * r * r;
        assert!((area(&tris) - exact).abs() / exact < 2e-3, "{}", area(&tris));
    }

    #[test]
    fn round_line_is_a_capsule() {
        let (tris, _) = collect(|f| {
            with_round_border_line_tri_list(64, Affine2::IDENTITY, [0.0, 0.0, 0.0, 10.0], 1.0, f)
        });
        let exact = 10.0 * 2.0 + PI;
        assert!((area(&tris) - exact).abs() / exact < 2e-3, "{}", area(&tris));
        // The capsule runs along +Y (oriented to the line).
        assert!(
            tris.iter()
                .all(|p| p[0].abs() <= 1.0 + 1e-4 && p[1] >= -1.0 - 1e-4 && p[1] <= 11.0 + 1e-4)
        );
    }

    #[test]
    fn borders_are_rings() {
        let (ring, _) =
            collect(|f| with_ellipse_border_tri_list(256, Affine2::IDENTITY, [0.0, 0.0, 20.0, 20.0], 1.0, f));
        let exact = PI * (11.0 * 11.0 - 9.0 * 9.0);
        assert!((area(&ring) - exact).abs() / exact < 1e-3);
        let (arc, _) = collect(|f| {
            with_arc_tri_list(0.0, PI, 64, Affine2::IDENTITY, [0.0, 0.0, 20.0, 20.0], 1.0, f);
        });
        assert!((area(&arc) - exact / 2.0).abs() / exact < 1e-3);
        let (rr, _) = collect(|f| {
            with_round_rectangle_border_tri_list(32, Affine2::IDENTITY, [0.0, 0.0, 20.0, 10.0], 3.0, 1.0, f);
        });
        assert!(area(&rr) > 2.0 * (20.0 + 10.0) * 2.0 * 0.85 && area(&rr) < 2.0 * (20.0 + 10.0) * 2.0);
        let border = rect_border_tri_list_xy(Affine2::IDENTITY, [0.0, 0.0, 10.0, 10.0], 1.0);
        assert!((area(&border) - (12.0 * 12.0 - 8.0 * 8.0)).abs() < 1e-3);
    }

    #[test]
    fn lerp_polygons_and_uvs() {
        let a: &[[f32; 2]] = &[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        let b: &[[f32; 2]] = &[[0.0, 0.0], [3.0, 0.0], [0.0, 3.0]];
        let (tris, _) = collect(|f| with_lerp_polygons_tri_list(Affine2::IDENTITY, &[a, b], 0.25, f));
        assert_eq!(tris, vec![[0.0, 0.0], [2.0, 0.0], [0.0, 2.0]]);
        let uv = rect_tri_list_uv([64, 32], [16.0, 8.0, 32.0, 16.0]);
        assert_eq!(uv[0], [0.25, 0.25]);
        assert_eq!(uv[4], [0.75, 0.75]);
    }
}
