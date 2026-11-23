// SPDX-License-Identifier: MPL-2.0
//! Biome areas as the evaluator sees them, and claims (spec 65 § 4).
//!
//! An area's claim value at `p` is 1 deeper than `blend_m` inside its outline and falls linearly to
//! 0 at the boundary (optionally with a value-noise wobble). Where areas overlap, the area with the
//! highest effective priority owns the spot with its value `w`; the next one gets `1 - w`. Map
//! areas claim per biome from an 8-bit key image, blurred over `blend_m` with a box filter (a
//! summed-area table of integer counts, so the result is exact and platform independent).

use std::cmp::Reverse;
use std::sync::Arc;

use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_core::math::hash::mix;

use crate::biome::def::BiomeDef;
use crate::data::{ShapeData, ShapeForm, quat};

/// An 8-bit RGB key image for `Map` areas: row-major, row 0 at the rectangle's `min_z`.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyImage {
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// `width * height` keys.
    pub keys: Vec<[u8; 3]>,
}

/// Per-biome summed-area tables of a key image (built once per capture).
#[derive(Clone, Debug, PartialEq)]
pub struct KeyTables {
    width: usize,
    height: usize,
    /// One table per candidate biome, `(width + 1) * (height + 1)` counts.
    tables: Vec<Vec<u32>>,
    /// Texels whose key matches no candidate biome.
    pub unknown_texels: u64,
}

impl KeyTables {
    /// Builds the tables of `image` for candidate keys (in candidate order).
    pub fn build(image: &KeyImage, keys: &[[u8; 3]]) -> Self {
        let (w, h) = (image.width as usize, image.height as usize);
        let mut tables = vec![vec![0u32; (w + 1) * (h + 1)]; keys.len()];
        let mut unknown = 0u64;
        for r in 0..h {
            for c in 0..w {
                let key = image.keys.get(r * w + c).copied().unwrap_or([0; 3]);
                let hit = keys.iter().position(|k| *k == key);
                if hit.is_none() {
                    unknown += 1;
                }
                for (b, t) in tables.iter_mut().enumerate() {
                    let v = u32::from(hit == Some(b));
                    t[(r + 1) * (w + 1) + c + 1] =
                        v + t[r * (w + 1) + c + 1] + t[(r + 1) * (w + 1) + c] - t[r * (w + 1) + c];
                }
            }
        }
        Self {
            width: w,
            height: h,
            tables,
            unknown_texels: unknown,
        }
    }

    /// Matching texels of candidate `b` in the inclusive texel box `[c0, c1] × [r0, r1]` (clamped),
    /// and the box's texel count.
    fn count(&self, b: usize, c0: i64, r0: i64, c1: i64, r1: i64) -> (u32, u32) {
        let Some(t) = self.tables.get(b) else {
            return (0, 0);
        };
        if self.width == 0 || self.height == 0 {
            return (0, 0);
        }
        let cl = |v: i64, n: usize| v.clamp(0, n as i64 - 1) as usize;
        let (c0, c1) = (cl(c0, self.width), cl(c1, self.width));
        let (r0, r1) = (cl(r0, self.height), cl(r1, self.height));
        let w1 = self.width + 1;
        let at = |r: usize, c: usize| t[r * w1 + c];
        let n = at(r1 + 1, c1 + 1) + at(r0, c0) - at(r0, c1 + 1) - at(r1 + 1, c0);
        (n, ((c1 - c0 + 1) * (r1 - r0 + 1)) as u32)
    }
}

/// The outline of an area.
#[derive(Clone, Debug, PartialEq)]
pub enum AreaOutline {
    /// A box or sphere, used as a vertical prism over its local XZ footprint.
    Shape(ShapeData),
    /// A closed polygon in XZ (a closed spline, sampled).
    Polygon(Vec<[f32; 2]>),
    /// A key image over an XZ rectangle; biomes are matched by [`BiomeDef::key`].
    Map {
        /// `[min_x, min_z, max_x, max_z]`.
        rect: [f32; 4],
        /// The image.
        image: Arc<KeyImage>,
        /// Its tables over the area's candidate biomes.
        tables: Arc<KeyTables>,
    },
}

impl AreaOutline {
    /// Signed horizontal depth inside the outline at `(x, z)`, meters (> 0 inside, <= 0 outside).
    /// Not defined for maps (0).
    pub fn depth(&self, x: f32, z: f32) -> f32 {
        match self {
            Self::Shape(s) => {
                let local =
                    quat(s.rotation).inverse() * (Vec3::new(x, s.position[1], z) - Vec3::from(s.position));
                match s.form {
                    // A tilted box (any rotation that is not a pure yaw) is a prism over its
                    // world-XZ projection, a convex polygon of its eight corners. Upright and
                    // yaw-only boxes keep the rectangle distance below, bit for bit.
                    ShapeForm::Box { half_extents } if s.rotation[0] != 0.0 || s.rotation[2] != 0.0 => {
                        polygon_depth(&box_footprint(s, half_extents), x, z)
                    }
                    ShapeForm::Box { half_extents } => {
                        let dx = half_extents[0] - local.x.abs();
                        let dz = half_extents[2] - local.z.abs();
                        if dx > 0.0 && dz > 0.0 {
                            dx.min(dz)
                        } else {
                            // Outside: minus the distance to the rectangle.
                            let ox = (-dx).max(0.0);
                            let oz = (-dz).max(0.0);
                            -(ox * ox + oz * oz).sqrt()
                        }
                    }
                    ShapeForm::Sphere { radius } => radius - (local.x * local.x + local.z * local.z).sqrt(),
                }
            }
            Self::Polygon(pts) => polygon_depth(pts, x, z),
            Self::Map { .. } => 0.0,
        }
    }

    /// XZ bounds `[min_x, min_z, max_x, max_z]`.
    pub fn xz_bounds(&self) -> [f32; 4] {
        match self {
            Self::Shape(s) => {
                let (lo, hi) = ShapeData { edge_m: 0.0, ..*s }.world_aabb();
                [lo[0], lo[2], hi[0], hi[2]]
            }
            Self::Polygon(pts) => pts.iter().fold(
                [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY],
                |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])],
            ),
            Self::Map { rect, .. } => *rect,
        }
    }

    /// The kind name for the agent (`shape`, `spline`, `map`).
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Shape(_) => "shape",
            Self::Polygon(_) => "spline",
            Self::Map { .. } => "map",
        }
    }
}

/// The world-XZ footprint of a box: the convex hull of its eight corners projected onto XZ,
/// counter-clockwise (monotone chain).
fn box_footprint(s: &ShapeData, half: [f32; 3]) -> Vec<[f32; 2]> {
    let q = quat(s.rotation);
    let center = Vec3::from(s.position);
    let mut pts: Vec<[f32; 2]> = Vec::with_capacity(8);
    for sx in [-1.0f32, 1.0] {
        for sy in [-1.0f32, 1.0] {
            for sz in [-1.0f32, 1.0] {
                let p = center + q * Vec3::new(sx * half[0], sy * half[1], sz * half[2]);
                pts.push([p.x, p.z]);
            }
        }
    }
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    fn cross(o: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    }
    // Lower then upper chain; each drops its last point (the other chain's first).
    fn chain<'a>(points: impl Iterator<Item = &'a [f32; 2]>, hull: &mut Vec<[f32; 2]>) {
        let start = hull.len();
        for &p in points {
            while hull.len() >= start + 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    let mut hull: Vec<[f32; 2]> = Vec::with_capacity(pts.len() + 1);
    chain(pts.iter(), &mut hull);
    chain(pts.iter().rev(), &mut hull);
    hull
}

/// Signed distance-to-edge of a closed polygon (even-odd inside test; > 0 inside).
pub fn polygon_depth(pts: &[[f32; 2]], x: f32, z: f32) -> f32 {
    if pts.len() < 3 {
        return f32::NEG_INFINITY;
    }
    let mut inside = false;
    let mut best = f32::INFINITY;
    let n = pts.len();
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        if (a[1] > z) != (b[1] > z) {
            let t = (z - a[1]) / (b[1] - a[1]);
            if x < a[0] + t * (b[0] - a[0]) {
                inside = !inside;
            }
        }
        let (ex, ez) = (b[0] - a[0], b[1] - a[1]);
        let len2 = ex * ex + ez * ez;
        let t = if len2 > 0.0 {
            (((x - a[0]) * ex + (z - a[1]) * ez) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (dx, dz) = (x - (a[0] + t * ex), z - (a[1] + t * ez));
        best = best.min((dx * dx + dz * dz).sqrt());
    }
    if inside { best } else { -best }
}

/// One biome area captured for a run.
#[derive(Clone, Debug, PartialEq)]
pub struct BiomeAreaSnap {
    /// The area entity's guid.
    pub guid: Guid,
    /// Its name.
    pub name: String,
    /// Outline.
    pub outline: AreaOutline,
    /// Soft edge width inside the area, meters.
    pub blend_m: f32,
    /// `(scale_m, amplitude 0..1)` of the edge wobble.
    pub blend_noise: Option<(f32, f32)>,
    /// Added to each biome's priority for this area.
    pub priority_offset: i32,
    /// Editor: regenerate this area alone.
    pub preview_alone: bool,
    /// `(asset path, definition)`: one for shape and spline areas, the candidates for maps.
    pub biomes: Vec<(String, Arc<BiomeDef>)>,
}

impl BiomeAreaSnap {
    /// The raw claim value of candidate biome `b` at `(x, z)` (before overlap resolution).
    pub fn value(&self, b: usize, x: f32, z: f32) -> f32 {
        crate::probe::hit_value();
        match &self.outline {
            AreaOutline::Map { rect, image, tables } => {
                let (w, h) = (image.width.max(1) as f32, image.height.max(1) as f32);
                if !(x >= rect[0] && x <= rect[2] && z >= rect[1] && z <= rect[3]) {
                    return 0.0;
                }
                let tw = (rect[2] - rect[0]).max(1e-6) / w;
                let th = (rect[3] - rect[1]).max(1e-6) / h;
                let c = ((x - rect[0]) / tw).floor() as i64;
                let r = ((z - rect[1]) / th).floor() as i64;
                if self.blend_m <= 0.0 {
                    let (n, _) = tables.count(b, c, r, c, r);
                    return n as f32;
                }
                // Box filter of side 2 * blend_m around the texel.
                let kc = (self.blend_m / tw).round().max(0.0) as i64;
                let kr = (self.blend_m / th).round().max(0.0) as i64;
                let (n, total) = tables.count(b, c - kc, r - kr, c + kc, r + kr);
                if total == 0 { 0.0 } else { n as f32 / total as f32 }
            }
            outline => {
                if b != 0 {
                    return 0.0;
                }
                let d = outline.depth(x, z);
                if d <= 0.0 {
                    return 0.0;
                }
                if self.blend_m <= 0.0 {
                    return 1.0;
                }
                let d = match self.blend_noise {
                    Some((scale, amp)) if scale > 0.0 && amp > 0.0 => {
                        let n = crate::nodes::density::fbm(
                            f64::from(x) / f64::from(scale),
                            f64::from(z) / f64::from(scale),
                            2,
                            0.5,
                            2.0,
                            mix(self.guid.0 as u32, (self.guid.0 >> 32) as u32),
                        );
                        d + (amp * self.blend_m) * (2.0 * n as f32 - 1.0)
                    }
                    _ => d,
                };
                (d / self.blend_m).clamp(0.0, 1.0)
            }
        }
    }

    /// Effective priority of candidate biome `b`.
    pub fn priority(&self, b: usize) -> i64 {
        self.biomes.get(b).map_or(i64::MIN, |(_, d)| {
            i64::from(d.priority) + i64::from(self.priority_offset)
        })
    }
}

/// One claim at a position: which (area, biome) and its weight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Claim {
    /// Index into the captured areas.
    pub area: usize,
    /// Index into the area's biomes.
    pub biome: usize,
    /// Weight in [0, 1].
    pub weight: f32,
}

/// The owner and the second claim at a position (spec 65 § 4).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClaimAt {
    /// The highest-priority claim with a positive value.
    pub owner: Option<Claim>,
    /// The next one, with weight `1 - owner.weight` (absent when that is 0).
    pub second: Option<Claim>,
}

impl ClaimAt {
    /// The weight of `(area, biome)` here (0 when it neither owns nor is second).
    pub fn weight_of(&self, area: usize, biome: usize) -> f32 {
        [self.owner, self.second]
            .into_iter()
            .flatten()
            .find(|c| c.area == area && c.biome == biome)
            .map_or(0.0, |c| c.weight)
    }
}

/// Resolves the claims at `(x, z)` over `areas` (in guid order). Ties in priority break by guid
/// (lower wins), then by raw value (higher wins), then by candidate index.
pub fn claims_at(areas: &[BiomeAreaSnap], x: f32, z: f32) -> ClaimAt {
    crate::probe::hit_claims();
    // Candidates `(priority, guid, value, area, biome)`; `rank` is smaller for the better one.
    type Cand = (i64, Guid, f32, usize, usize);
    let rank = |c: &Cand| (Reverse(c.0), c.1.0, Reverse(c.2.to_bits()), c.3, c.4);
    let mut best: Option<Cand> = None;
    let mut next: Option<Cand> = None;
    for (ai, area) in areas.iter().enumerate() {
        let b = area.outline.xz_bounds();
        if x < b[0] || x > b[2] || z < b[1] || z > b[3] {
            continue;
        }
        for bi in 0..area.biomes.len() {
            let v = area.value(bi, x, z);
            if v <= 0.0 {
                continue;
            }
            // Positive floats order like their bits.
            let c = (area.priority(bi), area.guid, v, ai, bi);
            match &best {
                Some(cur) if rank(&c) > rank(cur) => {
                    if next.as_ref().is_none_or(|n| rank(&c) < rank(n)) {
                        next = Some(c);
                    }
                }
                _ => {
                    next = best.take();
                    best = Some(c);
                }
            }
        }
    }
    let Some(o) = best else {
        return ClaimAt::default();
    };
    let owner = Claim {
        area: o.3,
        biome: o.4,
        weight: o.2,
    };
    let second = next.and_then(|s| {
        let w = 1.0 - o.2;
        (w > 0.0).then_some(Claim {
            area: s.3,
            biome: s.4,
            weight: w,
        })
    });
    ClaimAt {
        owner: Some(owner),
        second,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;

    fn area(guid: u128, half: f32, priority: i32, blend: f32) -> BiomeAreaSnap {
        BiomeAreaSnap {
            guid: Guid(guid),
            name: String::new(),
            outline: AreaOutline::Shape(ShapeData {
                form: ShapeForm::Box {
                    half_extents: [half, 10.0, half],
                },
                position: [0.0; 3],
                rotation: [0.0, 0.0, 0.0, 1.0],
                edge_m: 0.0,
                clip_xz: None,
            }),
            blend_m: blend,
            blend_noise: None,
            priority_offset: 0,
            preview_alone: false,
            biomes: vec![(
                "b.biome.ron".into(),
                Arc::new(BiomeDef {
                    priority,
                    ..BiomeDef::default()
                }),
            )],
        }
    }

    #[test]
    fn polygon_depth_is_signed_distance_to_the_edges() {
        let sq = [[-10.0, -10.0], [10.0, -10.0], [10.0, 10.0], [-10.0, 10.0]];
        assert!((polygon_depth(&sq, 0.0, 0.0) - 10.0).abs() < 1e-5);
        assert!((polygon_depth(&sq, 7.0, 0.0) - 3.0).abs() < 1e-5);
        assert!((polygon_depth(&sq, 12.0, 0.0) + 2.0).abs() < 1e-5);
    }

    /// The historic local-frame box depth, kept as the bit-identity oracle for upright and
    /// yaw-rotated boxes.
    fn legacy_box_depth(half: [f32; 3], local: Vec3) -> f32 {
        let dx = half[0] - local.x.abs();
        let dz = half[2] - local.z.abs();
        if dx > 0.0 && dz > 0.0 {
            dx.min(dz)
        } else {
            let ox = (-dx).max(0.0);
            let oz = (-dz).max(0.0);
            -(ox * ox + oz * oz).sqrt()
        }
    }

    fn box_outline(half: [f32; 3], rotation: [f32; 4]) -> AreaOutline {
        AreaOutline::Shape(ShapeData {
            form: ShapeForm::Box { half_extents: half },
            position: [0.0; 3],
            rotation,
            edge_m: 0.0,
            clip_xz: None,
        })
    }

    #[test]
    fn box_depth_is_bit_identical_for_upright_and_yaw_boxes() {
        // Upright and yaw-only rotations keep the local XZ axes horizontal, so the prism over
        // the world-XZ projection is exactly the historic rectangle distance.
        let yaw = Quat::from_rotation_y(0.7).to_array();
        for rotation in [[0.0, 0.0, 0.0, 1.0], yaw] {
            let outline = box_outline([2.0, 0.5, 3.0], rotation);
            let q = crate::data::quat(rotation);
            for (x, z) in [
                (0.0, 0.0),
                (1.9, 2.9),
                (-1.9, -2.9),
                (2.1, 0.3),
                (0.4, 3.2),
                (2.1, 3.2),
                (5.0, -5.0),
            ] {
                let local = q.inverse() * (Vec3::new(x, 0.0, z));
                assert_eq!(
                    outline.depth(x, z),
                    legacy_box_depth([2.0, 0.5, 3.0], local),
                    "bit drift at ({x}, {z}), rotation {rotation:?}"
                );
            }
        }
    }

    #[test]
    fn pitched_box_depth_is_the_prism_over_its_xz_projection() {
        // Pitched 90° about X, half (1, 0.1, 1): the box lies flat; its world-XZ footprint is
        // x ∈ [-1, 1], z ∈ [-0.1, 0.1]. The old local-frame code read local.z = ±y and called
        // points tens of meters above or below the box "inside" an infinite slab.
        let flat = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2).to_array();
        let outline = box_outline([1.0, 0.1, 1.0], flat);
        assert!(outline.depth(0.0, 0.05) > 0.0, "inside the footprint");
        assert!(outline.depth(0.9, -0.05) > 0.0, "inside the footprint");
        assert!(outline.depth(0.0, 50.0) <= 0.0, "far above the flat box");
        assert!(outline.depth(0.0, -50.0) <= 0.0, "far below the flat box");
        assert!(outline.depth(1.5, 0.0) <= 0.0);
        // The exclusion test reads depth through `contains`.
        let region = crate::biome::ExclusionRegion::Shape(match outline {
            AreaOutline::Shape(s) => s,
            _ => unreachable!(),
        });
        assert!(!region.contains([0.0, 0.0, 50.0]));
        assert!(region.contains([0.0, 0.0, 0.05]));
    }

    #[test]
    fn box_prism_depth_never_escapes_the_world_aabb() {
        // The exclusion prefilter (`exclusion_hit`) tests the world-XZ AABB before `contains`;
        // a contained point outside it would be silently dropped by the prefilter — a region
        // the node can never exclude. The prism over the projection stays inside the AABB by
        // construction; the old local-frame depth did not (it measured |local.z| = |z·cos θ|,
        // wider than the AABB for a pitched box).
        let pitched = Quat::from_rotation_x(0.6).to_array();
        let outline = box_outline([1.0, 0.1, 1.0], pitched);
        let bounds = outline.xz_bounds();
        let region = crate::biome::ExclusionRegion::Shape(match outline {
            AreaOutline::Shape(s) => s,
            _ => unreachable!(),
        });
        for ix in -30..=30 {
            for iz in -30..=30 {
                let (x, z) = (ix as f32 * 0.1, iz as f32 * 0.1);
                assert!(
                    !region.contains([x, 0.0, z])
                        || (x >= bounds[0] && x <= bounds[2] && z >= bounds[1] && z <= bounds[3]),
                    "({x}, {z}) is contained but outside the AABB {bounds:?}"
                );
            }
        }
    }

    #[test]
    fn a_lone_area_ramps_over_its_blend_band() {
        let a = [area(1, 50.0, 0, 8.0)];
        assert_eq!(claims_at(&a, 0.0, 0.0).owner.map(|c| c.weight), Some(1.0));
        let w = claims_at(&a, 46.0, 0.0).owner.map_or(0.0, |c| c.weight);
        assert!((w - 0.5).abs() < 1e-5, "{w}");
        assert_eq!(claims_at(&a, 60.0, 0.0), ClaimAt::default());
    }

    #[test]
    fn map_keys_select_biomes_and_blur_over_the_band() {
        // Left half key A, right half key B, 1 m texels over [0, 20] x [0, 4].
        let (w, h) = (20u32, 4u32);
        let keys: Vec<[u8; 3]> = (0..w * h)
            .map(|i| if i % w < 10 { [255, 0, 0] } else { [0, 255, 0] })
            .collect();
        let image = Arc::new(KeyImage {
            width: w,
            height: h,
            keys,
        });
        let cands = [[255, 0, 0], [0, 255, 0]];
        let tables = Arc::new(KeyTables::build(&image, &cands));
        let def = |r: u8, g: u8| {
            Arc::new(BiomeDef {
                map_key: Some((r, g, 0)),
                ..BiomeDef::default()
            })
        };
        let mut a = BiomeAreaSnap {
            blend_m: 0.0,
            outline: AreaOutline::Map {
                rect: [0.0, 0.0, 20.0, 4.0],
                image,
                tables,
            },
            biomes: vec![("a".into(), def(255, 0)), ("b".into(), def(0, 255))],
            ..area(1, 1.0, 0, 0.0)
        };
        assert_eq!(a.value(0, 2.5, 2.0), 1.0);
        assert_eq!(a.value(1, 2.5, 2.0), 0.0);
        assert_eq!(a.value(1, 15.5, 2.0), 1.0);
        a.blend_m = 2.0;
        // Texel 9 (x in [9, 10)): window texels 7..=11, 3 of A and 2 of B.
        assert!((a.value(0, 9.5, 2.0) - 0.6).abs() < 1e-6);
        assert!((a.value(1, 9.5, 2.0) - 0.4).abs() < 1e-6);
    }
}
