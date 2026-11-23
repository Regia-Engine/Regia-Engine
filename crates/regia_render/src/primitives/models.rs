// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3),
// src/models.rs. Copyright (c) 2019-2021 Fedor Logachev; dual-licensed MIT OR Apache-2.0,
// used here under the MIT licence (see docs/ported-sources.toml).
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
//! Mesh generators for immediate 3D primitives: cube, sphere, cylinder / cone, plane,
//! parallelepiped, and the line sets of a grid and a wire cube.
//!
//! Adapted from macroquad's immediate `draw_*` functions: their shapes, parametrisation and
//! texture coordinates, rebuilt as indexed meshes with outward normals and counter-clockwise front
//! faces (macroquad emits unindexed, unlit triangles of mixed winding). Every mesh is in local
//! space around the origin; callers place it with a model matrix.

use regia_core::math::{Vec2, Vec3};

use crate::lab_frame::LabVertex;

/// An indexed triangle mesh (`LabVertex` + `u32` triangle-list indices).
pub type PrimitiveMesh = (Vec<LabVertex>, Vec<u32>);

fn vertex(position: Vec3, normal: Vec3, uv: Vec2) -> LabVertex {
    LabVertex {
        position: position.to_array(),
        normal: normal.to_array(),
        uv: uv.to_array(),
        tangent: tangent_for(normal),
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// A unit tangent perpendicular to `normal` (any; these primitives carry no normal maps).
fn tangent_for(normal: Vec3) -> [f32; 4] {
    let helper = if normal.y.abs() < 0.99 { Vec3::Y } else { Vec3::X };
    let t = helper.cross(normal).normalize_or(Vec3::X);
    [t.x, t.y, t.z, 1.0]
}

/// Appends a quad `a b c d` (counter-clockwise seen from the side `normal` points to).
fn push_quad(mesh: &mut PrimitiveMesh, corners: [(Vec3, Vec2); 4], normal: Vec3) {
    let base = mesh.0.len() as u32;
    for (p, uv) in corners {
        mesh.0.push(vertex(p, normal, uv));
    }
    mesh.1
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// A box of `size` (full extents) centred at the origin: 24 vertices, 6 faces, each face's
/// texture spanning `0..1` (macroquad `draw_cube`).
#[must_use]
pub fn cube_mesh(size: Vec3) -> PrimitiveMesh {
    let h = size.abs() * 0.5;
    let mut mesh = (Vec::with_capacity(24), Vec::with_capacity(36));
    let p = |x: f32, y: f32, z: f32| Vec3::new(x * h.x, y * h.y, z * h.z);
    let uv = |u: f32, v: f32| Vec2::new(u, v);
    // Front (+Z) and back (−Z).
    push_quad(
        &mut mesh,
        [
            (p(-1.0, -1.0, 1.0), uv(0.0, 0.0)),
            (p(1.0, -1.0, 1.0), uv(1.0, 0.0)),
            (p(1.0, 1.0, 1.0), uv(1.0, 1.0)),
            (p(-1.0, 1.0, 1.0), uv(0.0, 1.0)),
        ],
        Vec3::Z,
    );
    push_quad(
        &mut mesh,
        [
            (p(1.0, -1.0, -1.0), uv(1.0, 0.0)),
            (p(-1.0, -1.0, -1.0), uv(0.0, 0.0)),
            (p(-1.0, 1.0, -1.0), uv(0.0, 1.0)),
            (p(1.0, 1.0, -1.0), uv(1.0, 1.0)),
        ],
        Vec3::NEG_Z,
    );
    // Top (+Y) and bottom (−Y).
    push_quad(
        &mut mesh,
        [
            (p(-1.0, 1.0, 1.0), uv(0.0, 0.0)),
            (p(1.0, 1.0, 1.0), uv(1.0, 0.0)),
            (p(1.0, 1.0, -1.0), uv(1.0, 1.0)),
            (p(-1.0, 1.0, -1.0), uv(0.0, 1.0)),
        ],
        Vec3::Y,
    );
    push_quad(
        &mut mesh,
        [
            (p(-1.0, -1.0, -1.0), uv(0.0, 1.0)),
            (p(1.0, -1.0, -1.0), uv(1.0, 1.0)),
            (p(1.0, -1.0, 1.0), uv(1.0, 0.0)),
            (p(-1.0, -1.0, 1.0), uv(0.0, 0.0)),
        ],
        Vec3::NEG_Y,
    );
    // Right (+X) and left (−X).
    push_quad(
        &mut mesh,
        [
            (p(1.0, -1.0, 1.0), uv(1.0, 1.0)),
            (p(1.0, -1.0, -1.0), uv(0.0, 1.0)),
            (p(1.0, 1.0, -1.0), uv(0.0, 0.0)),
            (p(1.0, 1.0, 1.0), uv(1.0, 0.0)),
        ],
        Vec3::X,
    );
    push_quad(
        &mut mesh,
        [
            (p(-1.0, -1.0, -1.0), uv(0.0, 1.0)),
            (p(-1.0, -1.0, 1.0), uv(1.0, 1.0)),
            (p(-1.0, 1.0, 1.0), uv(1.0, 0.0)),
            (p(-1.0, 1.0, -1.0), uv(0.0, 0.0)),
        ],
        Vec3::NEG_X,
    );
    mesh
}

/// A sphere of `radius` centred at the origin with `rings` latitude bands between the poles
/// (macroquad's default is 16) and `slices` meridians (default 16). Latitude runs from the south
/// pole (−Y) as in macroquad's `draw_sphere_ex`; `uv.x` follows the meridians, `uv.y` the
/// latitude from the south pole.
#[must_use]
pub fn sphere_mesh(radius: f32, rings: u32, slices: u32) -> PrimitiveMesh {
    let rings = rings.max(1);
    let slices = slices.max(3);
    // macroquad: angle = 3π/2 + π/(rings + 1) · i, so `rings + 2` latitudes pole to pole.
    let lat_steps = rings + 1;
    let mut vertices = Vec::with_capacity(((lat_steps + 1) * (slices + 1)) as usize);
    for i in 0..=lat_steps {
        let lat = 1.5 * std::f32::consts::PI + std::f32::consts::PI / lat_steps as f32 * i as f32;
        for j in 0..=slices {
            let lon = j as f32 * std::f32::consts::TAU / slices as f32;
            let n = Vec3::new(lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos());
            let uv = Vec2::new(j as f32 / slices as f32, i as f32 / lat_steps as f32);
            vertices.push(vertex(n * radius, n, uv));
        }
    }
    let stride = slices + 1;
    let mut indices = Vec::with_capacity((lat_steps * slices * 6) as usize);
    for i in 0..lat_steps {
        for j in 0..slices {
            let a = i * stride + j;
            let b = a + 1;
            let c = a + stride;
            let d = c + 1;
            // Latitude grows upward and longitude counter-clockwise seen from +Y, so (a, b, d)
            // is counter-clockwise seen from outside.
            indices.extend_from_slice(&[a, b, d, a, d, c]);
        }
    }
    (vertices, indices)
}

/// A cylinder standing on the origin (base at `y = 0`, top at `y = height`) with `sides`
/// segments (macroquad default 16), capped at both ends. A zero radius at one end makes a cone.
#[must_use]
pub fn cylinder_mesh(radius_top: f32, radius_bottom: f32, height: f32, sides: u32) -> PrimitiveMesh {
    let sides = sides.max(3);
    let step = std::f32::consts::TAU / sides as f32;
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    // The side's slant: normal = (cos-ish radial, (rb − rt) / h) normalised.
    let slope = if height.abs() > f32::EPSILON {
        (radius_bottom - radius_top) / height
    } else {
        0.0
    };
    for i in 0..=sides {
        let a = i as f32 * step;
        let (sin, cos) = a.sin_cos();
        let radial = Vec3::new(sin, 0.0, cos);
        let normal = (radial + Vec3::Y * slope).normalize_or(radial);
        let u = i as f32 / sides as f32;
        vertices.push(vertex(radial * radius_bottom, normal, Vec2::new(u, 0.0)));
        vertices.push(vertex(
            radial * radius_top + Vec3::Y * height,
            normal,
            Vec2::new(u, 1.0),
        ));
    }
    for i in 0..sides {
        let (b0, t0, b1, t1) = (i * 2, i * 2 + 1, i * 2 + 2, i * 2 + 3);
        // Angle grows from +Z towards +X (counter-clockwise seen from −Y), so seen from outside
        // the ring (b0, b1, t1) is counter-clockwise.
        indices.extend_from_slice(&[b0, b1, t1, b0, t1, t0]);
    }
    for (y, radius, normal) in [(height, radius_top, Vec3::Y), (0.0, radius_bottom, Vec3::NEG_Y)] {
        if radius <= 0.0 {
            continue;
        }
        let centre = vertices.len() as u32;
        vertices.push(vertex(Vec3::Y * y, normal, Vec2::splat(0.5)));
        for i in 0..=sides {
            let (sin, cos) = (i as f32 * step).sin_cos();
            let p = Vec3::new(sin * radius, y, cos * radius);
            vertices.push(vertex(p, normal, Vec2::new(0.5 + sin * 0.5, 0.5 + cos * 0.5)));
        }
        for i in 0..sides {
            let (a, b) = (centre + 1 + i, centre + 2 + i);
            if normal.y > 0.0 {
                indices.extend_from_slice(&[centre, a, b]);
            } else {
                indices.extend_from_slice(&[centre, b, a]);
            }
        }
    }
    (vertices, indices)
}

/// A horizontal plane in XZ facing +Y, `half_size` (x, z) from the centre (macroquad
/// `draw_plane` takes half extents too).
#[must_use]
pub fn plane_mesh(half_size: Vec2) -> PrimitiveMesh {
    let (x, z) = (half_size.x.abs(), half_size.y.abs());
    let mut mesh = (Vec::with_capacity(4), Vec::with_capacity(6));
    push_quad(
        &mut mesh,
        [
            (Vec3::new(-x, 0.0, z), Vec2::new(0.0, 1.0)),
            (Vec3::new(x, 0.0, z), Vec2::new(1.0, 1.0)),
            (Vec3::new(x, 0.0, -z), Vec2::new(1.0, 0.0)),
            (Vec3::new(-x, 0.0, -z), Vec2::new(0.0, 0.0)),
        ],
        Vec3::Y,
    );
    mesh
}

/// A parallelepiped with a corner at the origin and edges `e1`, `e2`, `e3` (macroquad
/// `draw_affine_parallelepiped`); faces get flat normals and are wound outward whatever the
/// handedness of the edges.
#[must_use]
pub fn parallelepiped_mesh(e1: Vec3, e2: Vec3, e3: Vec3) -> PrimitiveMesh {
    let centre = (e1 + e2 + e3) * 0.5;
    let mut mesh = (Vec::with_capacity(24), Vec::with_capacity(36));
    let faces = [
        (Vec3::ZERO, e1, e2),
        (Vec3::ZERO, e1, e3),
        (Vec3::ZERO, e2, e3),
        (e3, e1, e2),
        (e2, e1, e3),
        (e1, e2, e3),
    ];
    for (offset, a, b) in faces {
        let face_centre = offset + (a + b) * 0.5;
        let mut normal = a.cross(b).normalize_or_zero();
        let (mut a, mut b) = (a, b);
        if normal.dot(face_centre - centre) < 0.0 {
            normal = -normal;
            std::mem::swap(&mut a, &mut b);
        }
        push_quad(
            &mut mesh,
            [
                (offset, Vec2::new(0.0, 0.0)),
                (offset + a, Vec2::new(1.0, 0.0)),
                (offset + a + b, Vec2::new(1.0, 1.0)),
                (offset + b, Vec2::new(0.0, 1.0)),
            ],
            normal,
        );
    }
    mesh
}

/// The lines of a square grid in the XZ plane centred at the origin: `slices` cells across
/// (`slices / 2` either side), `spacing` apart (macroquad `draw_grid`). Each line is
/// `(start, end, is_axis)`; the two lines through the origin are the axes.
#[must_use]
pub fn grid_lines(slices: u32, spacing: f32) -> Vec<(Vec3, Vec3, bool)> {
    let half = (slices / 2) as i32;
    let extent = half as f32 * spacing;
    let mut lines = Vec::with_capacity((half as usize * 2 + 1) * 2);
    for i in -half..=half {
        let t = i as f32 * spacing;
        lines.push((Vec3::new(t, 0.0, -extent), Vec3::new(t, 0.0, extent), i == 0));
        lines.push((Vec3::new(-extent, 0.0, t), Vec3::new(extent, 0.0, t), i == 0));
    }
    lines
}

/// The 12 edges of a box of `size` centred at the origin (macroquad `draw_cube_wires`).
#[must_use]
pub fn cube_wire_lines(size: Vec3) -> [(Vec3, Vec3); 12] {
    let h = size.abs() * 0.5;
    let c = |x: f32, y: f32, z: f32| Vec3::new(x * h.x, y * h.y, z * h.z);
    [
        (c(-1.0, -1.0, 1.0), c(1.0, -1.0, 1.0)),
        (c(1.0, -1.0, 1.0), c(1.0, 1.0, 1.0)),
        (c(1.0, 1.0, 1.0), c(-1.0, 1.0, 1.0)),
        (c(-1.0, 1.0, 1.0), c(-1.0, -1.0, 1.0)),
        (c(-1.0, -1.0, -1.0), c(1.0, -1.0, -1.0)),
        (c(1.0, -1.0, -1.0), c(1.0, 1.0, -1.0)),
        (c(1.0, 1.0, -1.0), c(-1.0, 1.0, -1.0)),
        (c(-1.0, 1.0, -1.0), c(-1.0, -1.0, -1.0)),
        (c(-1.0, 1.0, 1.0), c(-1.0, 1.0, -1.0)),
        (c(1.0, 1.0, 1.0), c(1.0, 1.0, -1.0)),
        (c(-1.0, -1.0, 1.0), c(-1.0, -1.0, -1.0)),
        (c(1.0, -1.0, 1.0), c(1.0, -1.0, -1.0)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every triangle faces away from `inside` and agrees with its vertex normals.
    fn assert_outward(mesh: &PrimitiveMesh, inside: Vec3, name: &str) {
        let (v, idx) = mesh;
        assert_eq!(idx.len() % 3, 0, "{name}");
        for tri in idx.as_chunks::<3>().0 {
            let [a, b, c] = [0, 1, 2].map(|k| Vec3::from_array(v[tri[k] as usize].position));
            let n = (b - a).cross(c - a);
            if n.length_squared() < 1e-12 {
                continue; // a pole's degenerate sliver
            }
            let centroid = (a + b + c) / 3.0;
            assert!(n.dot(centroid - inside) > 0.0, "{name}: inward triangle {tri:?}");
            let vn = Vec3::from_array(v[tri[0] as usize].normal);
            assert!(
                vn.dot(n) > 0.0,
                "{name}: normal disagrees with winding at {tri:?}"
            );
            for k in tri {
                let len = Vec3::from_array(v[*k as usize].normal).length();
                assert!((len - 1.0).abs() < 1e-4, "{name}: normal length {len}");
            }
        }
    }

    #[test]
    fn closed_primitives_wind_outward() {
        assert_outward(&cube_mesh(Vec3::new(2.0, 1.0, 3.0)), Vec3::ZERO, "cube");
        assert_outward(&sphere_mesh(1.5, 16, 16), Vec3::ZERO, "sphere");
        assert_outward(&cylinder_mesh(1.0, 1.0, 2.0, 16), Vec3::Y, "cylinder");
        assert_outward(&cylinder_mesh(0.0, 1.0, 2.0, 16), Vec3::Y * 0.5, "cone");
        assert_outward(
            &parallelepiped_mesh(Vec3::X * 3.0, Vec3::Y * 2.0, Vec3::Z * 5.0),
            Vec3::new(1.5, 1.0, 2.5),
            "parallelepiped",
        );
        // Left-handed edges still wind outward.
        assert_outward(
            &parallelepiped_mesh(Vec3::Z, Vec3::Y, Vec3::X),
            Vec3::splat(0.5),
            "parallelepiped (swapped)",
        );
    }

    #[test]
    fn shapes_have_the_requested_extents() {
        let (v, idx) = cube_mesh(Vec3::new(2.0, 4.0, 6.0));
        assert_eq!((v.len(), idx.len()), (24, 36));
        assert!(
            v.iter()
                .all(|v| v.position[1].abs() == 2.0 || v.position[1].abs() <= 2.0)
        );
        let (v, _) = sphere_mesh(2.0, 8, 12);
        assert!(
            v.iter()
                .all(|v| (Vec3::from_array(v.position).length() - 2.0).abs() < 1e-4)
        );
        let (v, _) = cylinder_mesh(0.5, 1.0, 3.0, 8);
        assert!(v.iter().all(|v| v.position[1] >= 0.0 && v.position[1] <= 3.0));
        let (v, idx) = plane_mesh(Vec2::new(2.0, 1.0));
        assert_eq!((v.len(), idx.len()), (4, 6));
        assert!(v.iter().all(|v| v.normal == [0.0, 1.0, 0.0]));
        let a = Vec3::from_array(v[idx[0] as usize].position);
        let b = Vec3::from_array(v[idx[1] as usize].position);
        let c = Vec3::from_array(v[idx[2] as usize].position);
        assert!((b - a).cross(c - a).y > 0.0, "the plane faces +Y");
    }

    #[test]
    fn grid_and_wire_lines() {
        let lines = grid_lines(10, 1.0);
        assert_eq!(lines.len(), 22);
        assert_eq!(lines.iter().filter(|l| l.2).count(), 2);
        assert!(
            lines
                .iter()
                .all(|(a, b, _)| a.y == 0.0 && b.y == 0.0 && (*a - *b).length() == 10.0)
        );
        let wires = cube_wire_lines(Vec3::splat(2.0));
        assert!(wires.iter().all(|(a, b)| ((*a - *b).length() - 2.0).abs() < 1e-6));
    }
}
