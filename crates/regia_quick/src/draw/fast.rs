// SPDX-License-Identifier: MPL-2.0
//! Allocation-free fast paths for plain shapes (no lyon): filled rectangles, filled and
//! outlined ellipses and single line segments, streamed with
//! [`regia_render::primitives::triangulation`].

use lyon::tessellation::{LineCap, LineJoin, StrokeOptions};
use regia_core::math::Affine2;
use regia_render::ImmediateVertex;

use crate::draw::mesh::Mesh;
use crate::prelude_internal::*;

/// lyon's default tolerance: the largest distance between the true curve and its polygon, in
/// physical pixels.
const TOLERANCE_PX: f32 = 0.1;

/// Appends a non-indexed triangle list (three points per triangle) transformed by `m`, with
/// texture coordinates from `uv`.
pub(crate) fn push_tris(
    mesh: &mut Mesh,
    m: &Mat4,
    tris: &[[f32; 2]],
    color: Color,
    uv: impl Fn([f32; 2]) -> [f32; 2],
) {
    let base = mesh.vertices.len() as u32;
    let c = color.to_array();
    mesh.vertices.extend(tris.iter().map(|p| {
        let q = m.transform_point3(Vec3::new(p[0], p[1], 0.0));
        ImmediateVertex::textured(q.to_array(), uv(*p), c)
    }));
    let n = (tris.len() / 3 * 3) as u32;
    mesh.indices.extend(base..base + n);
}

/// Segments for a circle of radius `r` (logical units) under `m` at `scale` physical px per
/// unit, so the polygon stays within [`TOLERANCE_PX`] of the curve.
pub(crate) fn circle_segments(r: f32, m: &Mat4, scale: f32) -> u32 {
    circle_segments_with_tolerance(r, m, scale, TOLERANCE_PX)
}

/// Like [`circle_segments`], using `tolerance_px` (the stroke's tessellation tolerance) instead
/// of the 0.1 px default. A non-finite or non-positive tolerance keeps the default.
pub(crate) fn circle_segments_with_tolerance(r: f32, m: &Mat4, scale: f32, tolerance_px: f32) -> u32 {
    let tolerance = if tolerance_px.is_finite() && tolerance_px > 0.0 {
        tolerance_px
    } else {
        TOLERANCE_PX
    };
    let stretch = m.x_axis.truncate().length().max(m.y_axis.truncate().length());
    let r_px = (r * stretch * scale).abs();
    if r_px <= tolerance {
        return 6;
    }
    let step = (1.0 - tolerance / r_px).clamp(-1.0, 1.0).acos() * 2.0;
    ((std::f32::consts::TAU / step.max(1e-4)).ceil() as u32).clamp(6, 1024)
}

/// Identity: the fast paths stream in local units and transform with the full 3D matrix.
pub(crate) const LOCAL: Affine2 = Affine2::IDENTITY;

/// Appends a filled ellipse inscribed in `rect` (`[x, y, w, h]`) as an indexed fan around its
/// centre: `segments + 1` vertices instead of three per triangle.
pub(crate) fn push_ellipse_fill(
    mesh: &mut Mesh,
    m: &Mat4,
    rect: [f32; 4],
    segments: u32,
    color: Color,
    uv: impl Fn([f32; 2]) -> [f32; 2],
) {
    let [x, y, w, h] = rect;
    let (cx, cy) = (x + w * 0.5, y + h * 0.5);
    let c = color.to_array();
    let base = mesh.vertices.len() as u32;
    let mut push = |p: [f32; 2]| {
        let q = m.transform_point3(Vec3::new(p[0], p[1], 0.0));
        mesh.vertices
            .push(ImmediateVertex::textured(q.to_array(), uv(p), c));
    };
    push([cx, cy]);
    for (sin, cos) in unit_circle(segments) {
        push([cx + cos * w * 0.5, cy + sin * h * 0.5]);
    }
    for i in 0..segments {
        let next = (i + 1) % segments;
        mesh.indices
            .extend_from_slice(&[base, base + 1 + i, base + 1 + next]);
    }
}

/// Appends the outline of an ellipse inscribed in `rect`, `half_width` either side of the curve,
/// as an indexed ring (two vertices per segment).
pub(crate) fn push_ellipse_ring(
    mesh: &mut Mesh,
    m: &Mat4,
    rect: [f32; 4],
    segments: u32,
    half_width: f32,
    color: Color,
    uv: impl Fn([f32; 2]) -> [f32; 2],
) {
    let [x, y, w, h] = rect;
    let (cx, cy) = (x + w * 0.5, y + h * 0.5);
    let c = color.to_array();
    let base = mesh.vertices.len() as u32;
    for (sin, cos) in unit_circle(segments) {
        // Offset along the geometric normal, (cos / a, sin / b) scaled by a * b, so the outline
        // keeps its width on a flat ellipse; a circle gives (cos, sin) as before.
        let (a, b) = (w * 0.5, h * 0.5);
        let n = Vec2::new(cos * b, sin * a);
        let n = if n.length_squared() > 1e-12 {
            n.normalize()
        } else {
            Vec2::new(cos, sin)
        };
        for r in [half_width, -half_width] {
            let p = [cx + cos * a + n.x * r, cy + sin * b + n.y * r];
            let q = m.transform_point3(Vec3::new(p[0], p[1], 0.0));
            mesh.vertices
                .push(ImmediateVertex::textured(q.to_array(), uv(p), c));
        }
    }
    for i in 0..segments {
        let (o0, i0) = (base + 2 * i, base + 2 * i + 1);
        let next = (i + 1) % segments;
        let (o1, i1) = (base + 2 * next, base + 2 * next + 1);
        mesh.indices.extend_from_slice(&[o0, i0, o1, i0, i1, o1]);
    }
}

/// True when the closed outline `points` is convex (every turn has the same sign).
pub(crate) fn is_convex(points: &[Vec2]) -> bool {
    let n = points.len();
    let mut sign = 0.0f32;
    for i in 0..n {
        let (a, b, c) = (points[i], points[(i + 1) % n], points[(i + 2) % n]);
        let cross = (b - a).perp_dot(c - b);
        if cross.abs() <= f32::EPSILON {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
}

/// Appends a filled convex polygon as an indexed fan from its first point.
pub(crate) fn push_convex_fill(mesh: &mut Mesh, m: &Mat4, points: &[(Vec2, Vec2)], color: Color) {
    if points.len() < 3 {
        return;
    }
    let c = color.to_array();
    let base = mesh.vertices.len() as u32;
    for (p, uv) in points {
        let q = m.transform_point3(p.extend(0.0));
        mesh.vertices
            .push(ImmediateVertex::textured(q.to_array(), uv.to_array(), c));
    }
    for i in 1..points.len() as u32 - 1 {
        mesh.indices.extend_from_slice(&[base, base + i, base + i + 1]);
    }
}

/// A closed outline whose caps do not show and whose joins are miters that a ring can draw.
///
/// Round caps, round joins, unequal caps and bevel joins stay on lyon. A miter that would clip
/// (SVG miter limit) is rejected by [`push_fill_and_miter_ring`], not here.
pub(crate) fn stroke_is_plain(opts: &StrokeOptions) -> bool {
    opts.variable_line_width.is_none()
        && opts.start_cap == opts.end_cap
        && opts.start_cap != LineCap::Round
        && matches!(opts.line_join, LineJoin::Miter | LineJoin::MiterClip)
        && opts.line_width.is_finite()
        && opts.line_width > 0.0
        && opts.miter_limit.is_finite()
}

/// Fills `points` when `fill` is set, then appends an 2-vertex-per-corner miter ring.
///
/// Writes nothing and returns `false` when a corner's miter exceeds `miter_limit` (the caller
/// falls through to lyon). `points` has 3 to 8 vertices. The ring's outer corner for a
/// right angle sits one half-width past the vertex on both axes.
pub(crate) fn push_fill_and_miter_ring(
    mesh: &mut Mesh,
    m: &Mat4,
    points: &[(Vec2, Vec2)],
    half_width: f32,
    miter_limit: f32,
    fill: Option<Color>,
    stroke: Color,
) -> bool {
    let n = points.len();
    if !(3..=8).contains(&n) || !(half_width.is_finite() && half_width > 0.0) {
        return false;
    }
    let mut area = 0.0f32;
    for i in 0..n {
        let a = points[i].0;
        let b = points[(i + 1) % n].0;
        area += a.x * b.y - b.x * a.y;
    }
    if area.abs() <= 1e-8 {
        return false;
    }
    // Shoelace is negative for a clockwise ring when +Y is up. Outward is then the left normal.
    let clockwise = area < 0.0;
    let mut outer = [Vec2::ZERO; 8];
    let mut inner = [Vec2::ZERO; 8];
    let stroke_w = half_width * 2.0;
    let limit = miter_limit.max(1.0);
    for i in 0..n {
        let prev = points[(i + n - 1) % n].0;
        let curr = points[i].0;
        let next = points[(i + 1) % n].0;
        let d0 = curr - prev;
        let d1 = next - curr;
        let len0 = d0.length();
        let len1 = d1.length();
        if len0 <= 1e-8 || len1 <= 1e-8 {
            return false;
        }
        let d0 = d0 / len0;
        let d1 = d1 / len1;
        let n0 = outward(d0, clockwise) * half_width;
        let n1 = outward(d1, clockwise) * half_width;
        let Some(out) = line_hit(curr + n0, d0, curr + n1, d1) else {
            return false;
        };
        let Some(inn) = line_hit(curr - n0, d0, curr - n1, d1) else {
            return false;
        };
        if !out.is_finite() || !inn.is_finite() {
            return false;
        }
        // SVG: miter length is the distance between the outer tip and the inner corner,
        // divided by the stroke width. A square's ratio is sqrt(2), under the default limit 4.
        if (out - inn).length() / stroke_w > limit {
            return false;
        }
        outer[i] = out;
        inner[i] = inn;
    }
    if let Some(color) = fill {
        push_convex_fill(mesh, m, points, color);
    }
    let c = stroke.to_array();
    let base = mesh.vertices.len() as u32;
    for i in 0..n {
        let uv = points[i].1.to_array();
        for p in [outer[i], inner[i]] {
            let q = m.transform_point3(p.extend(0.0));
            mesh.vertices.push(ImmediateVertex::textured(q.to_array(), uv, c));
        }
    }
    for i in 0..n as u32 {
        let o0 = base + 2 * i;
        let i0 = o0 + 1;
        let next = (i + 1) % n as u32;
        let o1 = base + 2 * next;
        let i1 = o1 + 1;
        mesh.indices.extend_from_slice(&[o0, i0, o1, i0, i1, o1]);
    }
    true
}

/// One indexed quad (four vertices, two triangles).
pub(crate) fn push_quad(
    mesh: &mut Mesh,
    m: &Mat4,
    points: [Vec2; 4],
    color: Color,
    uv: impl Fn(Vec2) -> [f32; 2],
) {
    let c = color.to_array();
    let base = mesh.vertices.len() as u32;
    for p in points {
        let q = m.transform_point3(p.extend(0.0));
        mesh.vertices
            .push(ImmediateVertex::textured(q.to_array(), uv(p), c));
    }
    mesh.indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn outward(dir: Vec2, clockwise: bool) -> Vec2 {
    if clockwise {
        Vec2::new(-dir.y, dir.x)
    } else {
        Vec2::new(dir.y, -dir.x)
    }
}

fn line_hit(origin: Vec2, dir: Vec2, other: Vec2, other_dir: Vec2) -> Option<Vec2> {
    let den = dir.perp_dot(other_dir);
    if den.abs() < 1e-8 {
        return None;
    }
    let t = (other - origin).perp_dot(other_dir) / den;
    Some(origin + dir * t)
}

/// `(sin, cos)` of `segments` equal steps around the circle, by complex rotation (one `sin_cos`
/// per shape instead of one per vertex). Exact again every 64 steps to bound the drift.
fn unit_circle(segments: u32) -> impl Iterator<Item = (f32, f32)> {
    let step = std::f32::consts::TAU / segments.max(1) as f32;
    let (ds, dc) = step.sin_cos();
    let (mut s, mut c) = (0.0f32, 1.0f32);
    (0..segments).map(move |i| {
        if i.is_multiple_of(64) {
            (s, c) = (i as f32 * step).sin_cos();
        }
        let out = (s, c);
        (s, c) = (s * dc + c * ds, c * dc - s * ds);
        out
    })
}
