// SPDX-License-Identifier: MPL-2.0
//! Obstacle shapes of scatter meshes for clearance queries: a trunk capsule and a crown
//! spheroid per mesh, measured from the mesh's own geometry (ADR-1503).
//!
//! A tree is a thin vertical trunk under a wide crown; a rock or a bush is all "crown". The
//! shape is measured once per mesh at scale 1 (pivot at the base centre, +Y up, the
//! `.scatter.ron` mesh convention) from LOD 0 and every material part's finest non-empty LOD:
//!
//! - **trunk radius**: the widest horizontal extent of the vertices in the lowest fifth of the
//!   mesh (the same footprint the scatter field grounds instances with);
//! - **crown base**: the lowest height where the mesh is clearly wider than its trunk
//!   ([`CROWN_WIDTH_RATIO`] times the trunk radius, and at least [`CROWN_MIN_WIDTH_M`]); a mesh that
//!   never widens has no crown and is a trunk to its top;
//! - **crown**: a vertical-axis spheroid spanning the crown base to the top, as wide as the
//!   widest vertex above the base.
//!
//! The spheroid distance is the scaled-sphere bound `(|q| − 1) · min(a, b)` with `q` the point
//! in units of the semi-axes: never more than the true distance outside the spheroid, so a
//! clearance it reports is a clearance the camera has.

use super::super::field::{ScatterMeshAsset, ScatterMeshLod};

/// The crown begins where the mesh is this many times wider than its trunk.
pub const CROWN_WIDTH_RATIO: f32 = 2.5;
/// ... and at least this wide (radius, metres at scale 1): a thick trunk is not a crown.
pub const CROWN_MIN_WIDTH_M: f32 = 0.3;
/// Fraction of the mesh height measured for the trunk radius (the lowest fifth).
const TRUNK_BAND: f32 = 0.2;
/// Height slices the crown base search walks.
const SLICES: usize = 64;

/// Trunk and crown of one scatter mesh at scale 1, metres, pivot at the base centre, +Y up.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct ObstacleShape {
    /// Lowest vertex height (usually 0 or slightly below: the buried root flare).
    pub bottom: f32,
    /// Highest vertex height.
    pub top: f32,
    /// Trunk radius (horizontal extent of the lowest fifth of the mesh; the widest point when
    /// the mesh has no crown).
    pub trunk_radius: f32,
    /// Horizontal extent of the lowest fifth of the mesh (the footprint the scatter field
    /// buries a tilted base by).
    pub footprint: f32,
    /// Height where the crown begins; equal to `top` when the mesh has no crown.
    pub crown_base: f32,
    /// Horizontal radius of the crown (0 without a crown).
    pub crown_radius: f32,
    /// Bounding radius of the whole mesh about its vertical axis (horizontal reach).
    pub reach: f32,
}

impl ObstacleShape {
    /// Measures `asset` (LOD 0 and every part's finest non-empty LOD).
    #[must_use]
    pub fn of_asset(asset: &ScatterMeshAsset) -> Self {
        let mut points: Vec<[f32; 3]> = Vec::new();
        if let Some(lod) = asset.lods.first() {
            push_used(lod, &mut points);
        }
        for part in &asset.parts {
            if let Some(lod) = part
                .lods
                .iter()
                .find(|l| !l.indices.is_empty() && !l.vertices.is_empty())
            {
                push_used(lod, &mut points);
            }
        }
        Self::of_points(&points)
    }

    /// Measures a point cloud (positions at scale 1).
    #[must_use]
    pub fn of_points(points: &[[f32; 3]]) -> Self {
        if points.is_empty() {
            return Self {
                bottom: 0.0,
                top: 0.0,
                trunk_radius: 0.0,
                footprint: 0.0,
                crown_base: 0.0,
                crown_radius: 0.0,
                reach: 0.0,
            };
        }
        let (mut bottom, mut top) = (f32::INFINITY, f32::NEG_INFINITY);
        for p in points {
            bottom = bottom.min(p[1]);
            top = top.max(p[1]);
        }
        let height = (top - bottom).max(1e-3);
        let horizontal = |p: &[f32; 3]| (p[0] * p[0] + p[2] * p[2]).sqrt();
        let mut trunk_radius = 0.0f32;
        let mut reach = 0.0f32;
        let mut slice_width = [0.0f32; SLICES];
        for p in points {
            let r = horizontal(p);
            reach = reach.max(r);
            if p[1] <= bottom + TRUNK_BAND * height {
                trunk_radius = trunk_radius.max(r);
            }
            let s = (((p[1] - bottom) / height) * SLICES as f32) as usize;
            let s = s.min(SLICES - 1);
            slice_width[s] = slice_width[s].max(r);
        }
        let threshold = (trunk_radius * CROWN_WIDTH_RATIO).max(CROWN_MIN_WIDTH_M);
        let crown_slice = slice_width.iter().position(|&w| w > threshold);
        match crown_slice {
            Some(s) if reach > threshold => {
                let crown_base = bottom + s as f32 / SLICES as f32 * height;
                let crown_radius = points
                    .iter()
                    .filter(|p| p[1] >= crown_base)
                    .map(horizontal)
                    .fold(0.0f32, f32::max);
                Self {
                    bottom,
                    top,
                    trunk_radius,
                    footprint: trunk_radius,
                    crown_base,
                    crown_radius,
                    reach,
                }
            }
            _ => Self {
                bottom,
                top,
                // No crown: the whole mesh is one upright capsule as wide as its widest point.
                trunk_radius: reach,
                footprint: trunk_radius,
                crown_base: top,
                crown_radius: 0.0,
                reach,
            },
        }
    }

    /// Whether the mesh has a crown (a tree, not a pole or a rock).
    #[must_use]
    pub fn has_crown(&self) -> bool {
        self.crown_radius > 0.0 && self.top > self.crown_base
    }
}

/// Appends the vertices `lod`'s triangles use (the scatter field adds unreferenced bounds
/// corners to its own copies; an asset from a cook has none, but skipping unused vertices keeps
/// the measure honest either way).
fn push_used(lod: &ScatterMeshLod, out: &mut Vec<[f32; 3]>) {
    let mut used = vec![false; lod.vertices.len()];
    for &i in &lod.indices {
        if let Some(u) = used.get_mut(i as usize) {
            *u = true;
        }
    }
    out.extend(
        lod.vertices
            .iter()
            .zip(&used)
            .filter(|(_, u)| **u)
            .map(|(v, _)| v.position),
    );
}

/// What part of an obstacle a clearance was measured against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObstaclePart {
    /// The trunk capsule (or the whole mesh, when it has no crown).
    Trunk,
    /// The crown spheroid.
    Crown,
    /// The terrain surface under the path.
    Terrain,
}

impl ObstaclePart {
    /// Protocol name (`trunk`, `crown`, `terrain`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Trunk => "trunk",
            Self::Crown => "crown",
            Self::Terrain => "terrain",
        }
    }
}

/// One placed obstacle in world space: an upright (or tilted) trunk capsule plus a crown
/// spheroid along the instance's up axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldObstacle {
    /// Trunk axis start (the buried pivot), world metres.
    pub trunk_start: [f32; 3],
    /// Trunk axis end (the crown base, or the top without a crown).
    pub trunk_end: [f32; 3],
    /// Trunk radius, metres.
    pub trunk_radius: f32,
    /// Crown centre (on the up axis), world metres.
    pub crown_centre: [f32; 3],
    /// Unit up axis of the instance.
    pub up: [f32; 3],
    /// Crown horizontal semi-axis (0 = no crown), metres.
    pub crown_radius: f32,
    /// Crown vertical semi-axis, metres.
    pub crown_half_height: f32,
    /// Highest point of the instance (world Y).
    pub top_y: f32,
}

impl WorldObstacle {
    /// The obstacle of `shape` placed with uniform `scale` at `pivot` along unit `up`.
    #[must_use]
    pub fn place(shape: &ObstacleShape, pivot: [f32; 3], up: [f32; 3], scale: f32) -> Self {
        let at = |h: f32| {
            [
                pivot[0] + up[0] * h * scale,
                pivot[1] + up[1] * h * scale,
                pivot[2] + up[2] * h * scale,
            ]
        };
        let crown_half_height = if shape.has_crown() {
            0.5 * (shape.top - shape.crown_base) * scale
        } else {
            0.0
        };
        Self {
            trunk_start: at(shape.bottom.min(0.0)),
            trunk_end: at(shape.crown_base),
            trunk_radius: shape.trunk_radius * scale,
            crown_centre: at(0.5 * (shape.crown_base + shape.top)),
            up,
            crown_radius: if shape.has_crown() {
                shape.crown_radius * scale
            } else {
                0.0
            },
            crown_half_height,
            top_y: at(shape.top)[1].max(pivot[1]),
        }
    }

    /// Distance from `p` to the trunk surface (negative inside).
    #[must_use]
    pub fn trunk_distance(&self, p: [f32; 3]) -> f32 {
        point_segment_distance(p, self.trunk_start, self.trunk_end) - self.trunk_radius
    }

    /// Lower bound of the distance from `p` to the crown surface (negative inside; infinite
    /// without a crown).
    #[must_use]
    pub fn crown_distance(&self, p: [f32; 3]) -> f32 {
        if self.crown_radius <= 0.0 || self.crown_half_height <= 0.0 {
            return f32::INFINITY;
        }
        let d = sub(p, self.crown_centre);
        let axial = dot(d, self.up);
        let radial = sub(d, scale3(self.up, axial));
        let r = dot(radial, radial).sqrt();
        let (a, b) = (self.crown_radius, self.crown_half_height);
        let k = ((r / a) * (r / a) + (axial / b) * (axial / b)).sqrt();
        (k - 1.0) * a.min(b)
    }

    /// The nearer of the trunk and crown distances, and which part it is.
    #[must_use]
    pub fn distance(&self, p: [f32; 3]) -> (f32, ObstaclePart) {
        let trunk = self.trunk_distance(p);
        let crown = self.crown_distance(p);
        if crown < trunk {
            (crown, ObstaclePart::Crown)
        } else {
            (trunk, ObstaclePart::Trunk)
        }
    }

    /// Whether the vertical line through `p` (upward from `p`) passes through the crown above
    /// `p` (canopy cover overhead).
    #[must_use]
    pub fn covers(&self, p: [f32; 3]) -> bool {
        if self.crown_radius <= 0.0 || self.crown_centre[1] + self.crown_half_height <= p[1] {
            return false;
        }
        let (dx, dz) = (p[0] - self.crown_centre[0], p[2] - self.crown_centre[2]);
        dx * dx + dz * dz <= self.crown_radius * self.crown_radius
    }

    /// Horizontal distance from the instance's axis that the obstacle can reach.
    #[must_use]
    pub fn horizontal_reach(&self) -> f32 {
        self.trunk_radius.max(self.crown_radius)
    }
}

pub(crate) fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub(crate) fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub(crate) fn scale3(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

/// Distance from `p` to the segment `a..b`, and the segment parameter of the nearest point.
pub(crate) fn point_segment(p: [f32; 3], a: [f32; 3], b: [f32; 3]) -> (f32, f32) {
    let ab = sub(b, a);
    let len2 = dot(ab, ab);
    let t = if len2 > 0.0 {
        (dot(sub(p, a), ab) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let q = [a[0] + ab[0] * t, a[1] + ab[1] * t, a[2] + ab[2] * t];
    let d = sub(p, q);
    (dot(d, d).sqrt(), t)
}

fn point_segment_distance(p: [f32; 3], a: [f32; 3], b: [f32; 3]) -> f32 {
    point_segment(p, a, b).0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 0.2 m trunk up to 6 m under a 2 m crown from 6 m to 10 m.
    fn tree_points() -> Vec<[f32; 3]> {
        let mut pts = Vec::new();
        for i in 0..=20 {
            let y = i as f32 * 0.3;
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                pts.push([0.2 * a.cos(), y, 0.2 * a.sin()]);
            }
        }
        for i in 0..=10 {
            let y = 6.0 + i as f32 * 0.4;
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                pts.push([2.0 * a.cos(), y, 2.0 * a.sin()]);
            }
        }
        pts
    }

    #[test]
    fn a_tree_splits_into_trunk_and_crown() {
        let shape = ObstacleShape::of_points(&tree_points());
        assert!((shape.trunk_radius - 0.2).abs() < 1e-4, "{shape:?}");
        assert!(shape.has_crown());
        // The first slice that holds a crown vertex starts at or just below 6 m.
        assert!((5.8..=6.0).contains(&shape.crown_base), "{shape:?}");
        assert!((shape.crown_radius - 2.0).abs() < 1e-4);
        assert!((shape.top - 10.0).abs() < 1e-4);
    }

    #[test]
    fn a_rock_is_all_trunk() {
        // A squat blob: wide at the bottom too, so it never widens past its footprint.
        let mut pts = Vec::new();
        for i in 0..=8 {
            let y = i as f32 * 0.1;
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                pts.push([0.5 * a.cos(), y, 0.5 * a.sin()]);
            }
        }
        let shape = ObstacleShape::of_points(&pts);
        assert!(!shape.has_crown());
        assert!((shape.trunk_radius - 0.5).abs() < 1e-4);
        assert!((shape.crown_base - shape.top).abs() < 1e-6);
    }

    #[test]
    fn distances_are_signed_and_conservative() {
        let shape = ObstacleShape::of_points(&tree_points());
        let o = WorldObstacle::place(&shape, [10.0, 5.0, 0.0], [0.0, 1.0, 0.0], 2.0);
        // Scale 2: trunk radius 0.4, crown radius 4 centred at 5 + 16 = 21 m.
        assert!((o.trunk_distance([11.4, 7.0, 0.0]) - 1.0).abs() < 1e-4);
        assert!(o.trunk_distance([10.0, 7.0, 0.0]) < 0.0);
        let c = o.crown_centre;
        assert!((c[1] - (5.0 + 2.0 * 0.5 * (shape.crown_base + shape.top))).abs() < 1e-4);
        assert!(o.crown_distance(c) < 0.0);
        // Straight out along the horizontal semi-axis the bound is exact.
        let out = [c[0] + o.crown_radius + 1.5, c[1], c[2]];
        let d = o.crown_distance(out);
        assert!(d > 0.0 && d <= 1.5 + 1e-4, "{d}");
        // A point above the crown is covered below it only.
        assert!(o.covers([10.5, 6.0, 0.0]));
        assert!(!o.covers([10.5, 40.0, 0.0]));
        assert!(!o.covers([20.0, 6.0, 0.0]));
    }

    #[test]
    fn crown_bound_never_exceeds_the_true_distance() {
        // Sample points around an oblate crown; compare with a brute-force surface distance.
        let o = WorldObstacle {
            trunk_start: [0.0, 0.0, 0.0],
            trunk_end: [0.0, 4.0, 0.0],
            trunk_radius: 0.2,
            crown_centre: [0.0, 8.0, 0.0],
            up: [0.0, 1.0, 0.0],
            crown_radius: 4.0,
            crown_half_height: 2.0,
            top_y: 10.0,
        };
        let surface: Vec<[f32; 3]> = (0..=180)
            .flat_map(|i| {
                let t = i as f32 / 180.0 * std::f32::consts::PI;
                (0..4).map(move |k| {
                    let a = k as f32 * std::f32::consts::FRAC_PI_2;
                    [
                        4.0 * t.sin() * a.cos(),
                        8.0 + 2.0 * t.cos(),
                        4.0 * t.sin() * a.sin(),
                    ]
                })
            })
            .collect();
        for p in [
            [7.0, 8.0, 0.0],
            [0.0, 13.0, 0.0],
            [5.0, 11.0, 0.0],
            [3.0, 5.0, 3.0],
        ] {
            let truth = surface
                .iter()
                .map(|s| dot(sub(p, *s), sub(p, *s)).sqrt())
                .fold(f32::INFINITY, f32::min);
            let bound = o.crown_distance(p);
            assert!(bound <= truth + 0.02, "{p:?}: bound {bound} truth {truth}");
        }
    }
}
