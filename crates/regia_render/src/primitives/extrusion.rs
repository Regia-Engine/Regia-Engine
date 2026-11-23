// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/extrusion.rs (and the Extrudable perimeters of dim2.rs). Copyright (c) Bevy contributors.
// Bevy is dual-licensed MIT OR Apache-2.0; this file uses it under the MIT licence:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Extrusion mesher: a 2D [`Profile`] in the XY plane swept along Z into a prism.
//!
//! Ported from Bevy's `ExtrusionBuilder`: a front cap at `+half_depth`, a mirrored back cap, and a
//! mantle whose perimeter is either flat (one normal per edge: polygons) or smooth (interpolated
//! normals: circles). UV layout as Bevy's: front cap in `[0, 0.5]²`, back cap in
//! `[0.5, 1] × [0, 0.5]`, mantle in `v ∈ [0.5, 1]`. Regia Engine has no 2D mesh builders, so the caps
//! of arbitrary simple polygons are triangulated here by ear clipping (David Eberly,
//! "Triangulation by Ear Clipping", Geometric Tools, 2002).

use regia_core::math::{Vec2, Vec3};

use super::MeshParts;
use crate::runtime::CpuMesh;

/// The 2D outline an [`Extrusion`] sweeps.
#[derive(Debug, Clone, PartialEq)]
pub enum Profile {
    /// A simple (non-self-intersecting) polygon, either winding; hard edges between sides.
    Polygon(Vec<Vec2>),
    /// An axis-aligned rectangle centred at the origin.
    Rectangle {
        /// Half the width and height, metres.
        half_size: Vec2,
    },
    /// A circle centred at the origin with smooth sides.
    Circle {
        /// Radius, metres.
        radius: f32,
        /// Points around the circle (at least 3).
        resolution: u32,
    },
}

impl Default for Profile {
    fn default() -> Self {
        Self::Rectangle {
            half_size: Vec2::splat(0.5),
        }
    }
}

/// One run of perimeter points (indices into the cap's points), as Bevy's `PerimeterSegment`.
enum PerimeterSegment {
    /// Interpolated normals; the end normals are given (closed loops repeat the first point).
    Smooth {
        first_normal: Vec2,
        last_normal: Vec2,
        indices: Vec<u32>,
    },
    /// One normal per edge.
    Flat { indices: Vec<u32> },
}

impl Profile {
    /// The outline points, counter-clockwise.
    #[must_use]
    pub fn points(&self) -> Vec<Vec2> {
        match self {
            Self::Polygon(points) => {
                let mut points = points.clone();
                if signed_area(&points) < 0.0 {
                    points.reverse();
                }
                points
            }
            Self::Rectangle { half_size } => {
                let h = half_size.abs();
                vec![
                    Vec2::new(h.x, h.y),
                    Vec2::new(-h.x, h.y),
                    Vec2::new(-h.x, -h.y),
                    Vec2::new(h.x, -h.y),
                ]
            }
            Self::Circle { radius, resolution } => {
                let resolution = (*resolution).max(3);
                let step = std::f32::consts::TAU / resolution as f32;
                // Point 0 at the top, so the seam normal is +Y (Bevy's circle).
                (0..resolution)
                    .map(|i| Vec2::from_angle(std::f32::consts::FRAC_PI_2 + i as f32 * step) * radius.abs())
                    .collect()
            }
        }
    }

    fn perimeter(&self, count: u32) -> PerimeterSegment {
        let indices: Vec<u32> = (0..count).chain([0]).collect();
        match self {
            Self::Circle { .. } => PerimeterSegment::Smooth {
                first_normal: Vec2::Y,
                last_normal: Vec2::Y,
                indices,
            },
            Self::Polygon(_) | Self::Rectangle { .. } => PerimeterSegment::Flat { indices },
        }
    }
}

/// Twice-signed area / 2 (shoelace); positive for counter-clockwise points.
pub(super) fn signed_area(points: &[Vec2]) -> f32 {
    let n = points.len();
    (0..n)
        .map(|i| points[i].perp_dot(points[(i + 1) % n]))
        .sum::<f32>()
        * 0.5
}

/// Ear-clipping triangulation of a counter-clockwise simple polygon; counter-clockwise triangles.
/// Falls back to a fan for whatever remains if no ear is found (degenerate input).
fn triangulate(points: &[Vec2]) -> Vec<u32> {
    let n = points.len();
    let mut remaining: Vec<usize> = (0..n).collect();
    let mut triangles = Vec::with_capacity(n.saturating_sub(2) * 3);
    while remaining.len() > 3 {
        let m = remaining.len();
        let ear = (0..m).find(|&i| {
            let (p, c, x) = (remaining[(i + m - 1) % m], remaining[i], remaining[(i + 1) % m]);
            let (a, b, d) = (points[p], points[c], points[x]);
            if (b - a).perp_dot(d - b) <= 0.0 {
                return false; // reflex or collinear corner
            }
            !remaining
                .iter()
                .filter(|&&k| k != p && k != c && k != x)
                .any(|&k| inside_triangle(points[k], a, b, d))
        });
        let Some(i) = ear else { break };
        let m = remaining.len();
        triangles.extend_from_slice(&[
            remaining[(i + m - 1) % m] as u32,
            remaining[i] as u32,
            remaining[(i + 1) % m] as u32,
        ]);
        remaining.remove(i);
    }
    for i in 1..remaining.len().saturating_sub(1) {
        triangles.extend_from_slice(&[remaining[0] as u32, remaining[i] as u32, remaining[i + 1] as u32]);
    }
    triangles
}

/// `p` inside or on the counter-clockwise triangle `abc`.
fn inside_triangle(p: Vec2, a: Vec2, b: Vec2, c: Vec2) -> bool {
    (b - a).perp_dot(p - a) >= 0.0 && (c - b).perp_dot(p - b) >= 0.0 && (a - c).perp_dot(p - c) >= 0.0
}

/// A [`Profile`] extruded along Z, centred at the origin.
#[derive(Debug, Clone, PartialEq)]
pub struct Extrusion {
    /// The outline swept along Z.
    pub profile: Profile,
    /// Half the extrusion depth, metres (zero gives a double-sided flat shape).
    pub half_depth: f32,
    /// Mantle segments along Z (at least 1).
    pub segments: u32,
}

impl Default for Extrusion {
    fn default() -> Self {
        Self {
            profile: Profile::default(),
            half_depth: 0.5,
            segments: 1,
        }
    }
}

impl Extrusion {
    /// Triangle count: two caps of `n − 2` triangles plus `2 × n × segments` mantle triangles for
    /// an `n`-point outline (no mantle at zero depth).
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        let n = self.profile.points().len();
        let mantle = if self.half_depth == 0.0 {
            0
        } else {
            2 * n * self.segments.max(1) as usize
        };
        2 * n.saturating_sub(2) + mantle
    }

    /// Builds the mesh.
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let points = self.profile.points();
        let half_depth = self.half_depth.abs();
        let segments = self.segments.max(1) as usize;
        let cap_triangles = triangulate(&points);

        // Cap UVs from the outline's bounding box, v down (Bevy's 2D convention).
        let min = points.iter().copied().fold(Vec2::splat(f32::MAX), Vec2::min);
        let max = points.iter().copied().fold(Vec2::splat(f32::MIN), Vec2::max);
        let extent = (max - min).max(Vec2::splat(f32::EPSILON));
        let base_uv = |p: Vec2| {
            let t = (p - min) / extent;
            [t.x, 1.0 - t.y]
        };

        let mut parts =
            MeshParts::with_capacity(points.len() * (2 + 2 * (segments + 1)), self.triangle_count() * 3);
        // Front cap at +half_depth, uvs in [0, 0.5]².
        let front = parts.positions.len() as u32;
        for p in &points {
            let [u, v] = base_uv(*p);
            parts.push([p.x, p.y, half_depth], [0.0, 0.0, 1.0], [u * 0.5, v * 0.5]);
        }
        parts.indices.extend(cap_triangles.iter().map(|i| front + i));
        // Back cap: mirrored in Z, winding reversed, uvs shifted to [0.5, 1] × [0, 0.5].
        let back = parts.positions.len() as u32;
        for p in &points {
            let [u, v] = base_uv(*p);
            parts.push(
                [p.x, p.y, -half_depth],
                [0.0, 0.0, -1.0],
                [u * 0.5 + 0.5, v * 0.5],
            );
        }
        for tri in cap_triangles.chunks(3) {
            parts
                .indices
                .extend_from_slice(&[back + tri[1], back + tri[0], back + tri[2]]);
        }
        // An extrusion of depth 0 needs no mantle.
        if half_depth == 0.0 {
            return parts.finish();
        }

        let layers = segments + 1;
        let layer_depth_delta = half_depth * 2.0 / segments as f32;
        let cap = |i: u32| points[i as usize].extend(half_depth);
        // One perimeter run per outline here; Bevy splits the U range across several runs.
        let perimeter = [self.profile.perimeter(points.len() as u32)];
        let uv_segment_delta = 1.0 / perimeter.len() as f32;
        for (run, segment) in perimeter.into_iter().enumerate() {
            let uv_start = run as f32 * uv_segment_delta;
            match segment {
                PerimeterSegment::Flat { indices } => {
                    let uv_delta = uv_segment_delta / (indices.len() - 1) as f32;
                    for i in 0..(indices.len() - 1) {
                        let uv_x = uv_start + uv_delta * i as f32;
                        let a = cap(indices[i]);
                        let b = cap(indices[i + 1]);
                        // The edge's outward normal (outline is counter-clockwise).
                        let n = Vec3::new(b.y - a.y, a.x - b.x, 0.0)
                            .normalize_or_zero()
                            .to_array();
                        let index = parts.positions.len() as u32;
                        for layer in 0..layers {
                            let layer = layer as f32;
                            let z = a.z - layer_depth_delta * layer;
                            let uv_y = 0.5 + 0.5 * layer / segments as f32;
                            parts.push([a.x, a.y, z], n, [uv_x, uv_y]);
                            parts.push([b.x, b.y, z], n, [uv_x + uv_delta, uv_y]);
                        }
                        for s in 0..segments as u32 {
                            let base = index + 2 * s;
                            parts.indices.extend_from_slice(&[
                                base,
                                base + 2,
                                base + 1,
                                base + 1,
                                base + 2,
                                base + 3,
                            ]);
                        }
                    }
                }
                PerimeterSegment::Smooth {
                    first_normal,
                    last_normal,
                    indices,
                } => {
                    let uv_delta = uv_segment_delta / (indices.len() - 1) as f32;
                    let base_index = parts.positions.len() as u32;
                    let columns = indices.len();
                    for column in 0..columns {
                        let p = cap(indices[column]);
                        let n = if column == 0 {
                            first_normal
                        } else if column == columns - 1 {
                            last_normal
                        } else {
                            // Average of the two edge directions, turned outward; nearer
                            // neighbours weigh more because the directions are normalised first.
                            let a = cap(indices[column - 1]).truncate();
                            let c = cap(indices[column + 1]).truncate();
                            let b = p.truncate();
                            let t = (b - a).normalize_or_zero() + (c - b).normalize_or_zero();
                            Vec2::new(t.y, -t.x).normalize_or_zero()
                        };
                        let uv_x = uv_start + uv_delta * column as f32;
                        for layer in 0..layers {
                            let layer = layer as f32;
                            let z = p.z - layer_depth_delta * layer;
                            let uv_y = 0.5 + 0.5 * layer / segments as f32;
                            parts.push([p.x, p.y, z], n.extend(0.0).to_array(), [uv_x, uv_y]);
                        }
                    }
                    let layers = layers as u32;
                    for s in 0..segments as u32 {
                        for column in 0..(columns as u32 - 1) {
                            let index = base_index + s + column * layers;
                            parts.indices.extend_from_slice(&[
                                index,
                                index + 1,
                                index + layers,
                                index + layers,
                                index + 1,
                                index + layers + 1,
                            ]);
                        }
                    }
                }
            }
        }
        parts.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::check;

    fn l_shape() -> Vec<Vec2> {
        // Clockwise on purpose: the mesher must fix the winding.
        vec![
            Vec2::new(0.0, 0.0),
            Vec2::new(0.0, 2.0),
            Vec2::new(1.0, 2.0),
            Vec2::new(1.0, 1.0),
            Vec2::new(2.0, 1.0),
            Vec2::new(2.0, 0.0),
        ]
    }

    #[test]
    fn extrusions_are_closed_and_counts_match() {
        let profiles = [
            ("rectangle", Profile::default()),
            (
                "circle",
                Profile::Circle {
                    radius: 0.5,
                    resolution: 24,
                },
            ),
            (
                "triangle ramp",
                Profile::Polygon(vec![
                    Vec2::new(-1.0, 0.0),
                    Vec2::new(1.0, 0.0),
                    Vec2::new(1.0, 0.6),
                ]),
            ),
            ("concave L", Profile::Polygon(l_shape())),
        ];
        for (what, profile) in profiles {
            for segments in [1, 3] {
                let extrusion = Extrusion {
                    profile: profile.clone(),
                    half_depth: 0.75,
                    segments,
                };
                let mesh = extrusion.build();
                let what = format!("{what} x{segments}");
                check::assert_well_formed(&mesh, &what);
                check::assert_watertight(&mesh, &what);
                assert_eq!(check::triangles(&mesh), extrusion.triangle_count(), "{what}");
            }
        }
    }

    #[test]
    fn ear_clipping_covers_the_concave_area() {
        let mut points = l_shape();
        points.reverse();
        let triangles = triangulate(&points);
        assert_eq!(triangles.len(), 12);
        let area: f32 = triangles
            .chunks(3)
            .map(|t| {
                let [a, b, c] = [0, 1, 2].map(|k| points[t[k] as usize]);
                (b - a).perp_dot(c - a) * 0.5
            })
            .sum();
        assert!((area - 3.0).abs() < 1.0e-5, "area {area}");
    }

    #[test]
    fn flat_extrusion_is_double_sided() {
        let mesh = Extrusion {
            half_depth: 0.0,
            ..Extrusion::default()
        }
        .build();
        check::assert_well_formed(&mesh, "flat");
        assert_eq!(check::triangles(&mesh), 4);
    }
}
