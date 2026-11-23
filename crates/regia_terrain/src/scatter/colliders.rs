// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pilot_rocks.rs, commit f7e8f4740), ADR-8012.
// Portions copied from Direct Order 27 (shell-regia/game/src/world/rock_colliders.rs, commit f7e8f4740), ADR-8012.
// Copied under the owner's 2026-11-16 exception and adapted (ADR-8029): caps became settings, the glTF
// bounds reader became `MeshExtent`, game identifiers dropped.

//! Gameplay colliders from scatter placement (Q-08, ADR-8029).
//!
//! Scatter is drawn by [`super::field::ScatterField`], which streams cells of
//! [`place_class_cell_with_origin`](super::placement::place_class_cell_with_origin). A game whose
//! simulation has no rock placement still needs walking bodies to stop at the large rocks on
//! every peer. [`bake_scenery_colliders`] re-runs the same deterministic placement on the CPU
//! (no GPU, no cook) and keeps one sphere per instance whose top stands more than a threshold
//! above the ground; [`SphereBlockers`] is the matching runtime query that pushes a vertical body
//! out of those spheres. Small stones under the threshold stay walk-through, and a sphere never
//! gives support (a body does not stand on a rock).
//!
//! The bake must place with the same [`PlacementOrigin`] as the drawn field. With
//! [`PlacementOrigin::World`] it may run on the inner, playable map while the field draws a
//! larger map with a backdrop ring: the rocks inside agree (see
//! [`WorldAnchor`](super::placement::WorldAnchor)).
//!
//! Copied from Direct Order 27 under the owner's exception (ADR-8012): the game's caps (threshold,
//! radius factor, playable square, bucket size, push cap, body size) became settings, the glTF
//! bounds reader became [`MeshExtent`] supplied by the host, and the arithmetic is unchanged so a
//! host can match its committed bake byte for byte.

use std::collections::BTreeMap;

use super::ScatterError;
use super::field::ScatterMeshAsset;
use super::placement::{PlacementInputs, PlacementOrigin, place_set_with_origin};
use super::set::{ScatterClass, ScatterSet};

/// Size of a scatter mesh at unit scale (its LOD 0), metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshExtent {
    /// Footprint along X and Z (`max − min`).
    pub footprint: [f32; 2],
    /// Height along Y (`max − min`).
    pub height: f32,
}

impl MeshExtent {
    /// The extent of an axis-aligned box from `min` to `max` (a glTF POSITION accessor's bounds).
    #[must_use]
    pub fn from_bounds(min: [f32; 3], max: [f32; 3]) -> Self {
        Self {
            footprint: [max[0] - min[0], max[2] - min[2]],
            height: max[1] - min[1],
        }
    }

    /// The extent of `asset`'s LOD 0 vertices; `None` when LOD 0 has no vertex.
    #[must_use]
    pub fn from_asset(asset: &ScatterMeshAsset) -> Option<Self> {
        let lod = asset.lods.first()?;
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for v in &lod.vertices {
            for ((lo, hi), p) in min.iter_mut().zip(&mut max).zip(v.position) {
                *lo = lo.min(p);
                *hi = hi.max(p);
            }
        }
        (!lod.vertices.is_empty()).then(|| Self::from_bounds(min, max))
    }
}

/// What [`bake_scenery_colliders`] keeps, and how it writes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneryColliderSettings {
    /// An instance becomes a collider when its top stands more than this above the ground
    /// (metres; `height · scale · (1 − sink) · up.y`).
    pub threshold_m: f32,
    /// Sphere radius per unit of `scale · sqrt(footprint_x · footprint_z)`.
    pub radius_per_scale: f32,
    /// Keep only instances with `|x| ≤ [0]` and `|z| ≤ [1]` (after `mirror_x`): the playable
    /// area when the map placed on is larger. `None` keeps the whole map.
    pub clip_half_extent: Option<[f32; 2]>,
    /// Write `−x` instead of `x` (a host whose simulation frame mirrors X).
    pub mirror_x: bool,
}

impl Default for SceneryColliderSettings {
    fn default() -> Self {
        Self {
            threshold_m: 0.7,
            radius_per_scale: 0.4,
            clip_half_extent: None,
            mirror_x: false,
        }
    }
}

/// One baked collider: a sphere in integer millimetres, sorted by `(x, z, y, r)`.
/// `y_mm + r_mm` is the top of the rock and `r_mm` its XZ radius.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SceneryCollider {
    /// Centre X, millimetres.
    pub x_mm: i64,
    /// Centre Z, millimetres.
    pub z_mm: i64,
    /// Centre Y, millimetres (the top minus the radius).
    pub y_mm: i64,
    /// Radius, millimetres.
    pub r_mm: i64,
}

impl SceneryCollider {
    /// The sphere in metres. Each value is `mm / 1000` in `f64` narrowed to `f32`: the same bits
    /// as parsing the three-decimal text of the value.
    #[must_use]
    pub fn sphere(&self) -> ColliderSphere {
        let m = |v: i64| (v as f64 / 1000.0) as f32;
        ColliderSphere {
            x: m(self.x_mm),
            y: m(self.y_mm),
            z: m(self.z_mm),
            r: m(self.r_mm),
        }
    }
}

fn mm(v: f32) -> i64 {
    (f64::from(v) * 1000.0).round() as i64
}

/// Can any mesh of `class` stand taller than the threshold?
fn class_can_collide(class: &ScatterClass, extents: &[MeshExtent], threshold_m: f32) -> bool {
    class
        .meshes
        .iter()
        .zip(extents)
        .any(|(m, e)| e.height * m.max_scale * (1.0 - class.sink) > threshold_m)
}

/// Places `set` the way the drawn field does (same `inputs`, same `origin`) and returns one
/// sorted, de-duplicated collider per instance that rises above `settings.threshold_m`.
///
/// `extent_of` gives each mesh path's [`MeshExtent`] (from [`MeshExtent::from_asset`] or the
/// host's own bounds). Classes that can never reach the threshold are not placed at all; this
/// does not move the other classes, whose cell streams are keyed by class name.
///
/// # Errors
///
/// [`ScatterError::Asset`] when `extent_of` has no extent for a mesh the set names, or when
/// placement fails (a density map is missing).
pub fn bake_scenery_colliders(
    set: &ScatterSet,
    inputs: PlacementInputs<'_>,
    origin: &PlacementOrigin,
    mut extent_of: impl FnMut(&str) -> Option<MeshExtent>,
    settings: &SceneryColliderSettings,
) -> Result<Vec<SceneryCollider>, ScatterError> {
    let _span = tracing::info_span!(target: "regia_terrain", "scatter.bake_colliders").entered();
    let mut kept = ScatterSet {
        classes: Vec::new(),
        ..set.clone()
    };
    let mut extents: Vec<Vec<MeshExtent>> = Vec::new();
    for class in &set.classes {
        let e = class
            .meshes
            .iter()
            .map(|m| {
                extent_of(&m.mesh).ok_or_else(|| ScatterError::Asset {
                    path: m.mesh.clone(),
                    reason: format!("no mesh extent for class '{}'", class.name),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if class_can_collide(class, &e, settings.threshold_m) {
            kept.classes.push(class.clone());
            extents.push(e);
        }
    }
    let placed = place_set_with_origin(&kept, inputs, origin)?;
    let mut out = Vec::new();
    for p in &placed {
        let class = &kept.classes[usize::from(p.class)];
        let e = extents[usize::from(p.class)][usize::from(p.mesh)];
        let up_y = p.up[1].clamp(0.2, 1.0);
        // The field buries `sink` of the height, then stands the mesh along `up`.
        let rise = (e.height * p.scale * (1.0 - class.sink)) * up_y;
        if rise <= settings.threshold_m {
            continue;
        }
        let x = if settings.mirror_x {
            -p.position[0]
        } else {
            p.position[0]
        };
        let z = p.position[2];
        if let Some([hx, hz]) = settings.clip_half_extent
            && (x.abs() > hx || z.abs() > hz)
        {
            continue;
        }
        let r = settings.radius_per_scale * p.scale * (e.footprint[0] * e.footprint[1]).max(0.0).sqrt();
        let top = p.position[1] + rise;
        out.push(SceneryCollider {
            x_mm: mm(x),
            z_mm: mm(z),
            y_mm: mm(top - r),
            r_mm: mm(r),
        });
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// One collider sphere in metres. `y + r` is the top of the rock, `r` its XZ radius.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColliderSphere {
    /// Centre X, metres.
    pub x: f32,
    /// Centre Y, metres.
    pub y: f32,
    /// Centre Z, metres.
    pub z: f32,
    /// Radius, metres.
    pub r: f32,
}

impl ColliderSphere {
    /// The top of the rock (`y + r`).
    #[inline]
    #[must_use]
    pub fn top(&self) -> f32 {
        self.y + self.r
    }
}

/// The vertical body [`SphereBlockers`] pushes (a walking character's capsule).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockedBody {
    /// Capsule radius, metres.
    pub radius: f32,
    /// Height from the feet to the top, metres.
    pub height: f32,
    /// Step height: a rock whose top is at most this above the feet does not block, metres.
    pub step: f32,
    /// Extra clearance kept from a rock, metres.
    pub skin: f32,
}

/// Tuning of [`SphereBlockers`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SphereBlockerSettings {
    /// Bucket edge, metres. A sphere is listed in every bucket its disc (plus the margin) touches.
    pub bucket_m: f32,
    /// Extra reach a sphere is bucketed with (at least the largest body radius plus skin).
    pub bucket_margin_m: f32,
    /// Most one [`SphereBlockers::push_out`] call moves a body: one that starts inside a rock is
    /// walked out over several calls instead of snapping.
    pub max_push_m: f32,
}

impl Default for SphereBlockerSettings {
    fn default() -> Self {
        Self {
            bucket_m: 16.0,
            bucket_margin_m: 1.0,
            max_push_m: 0.5,
        }
    }
}

/// Collider spheres bucketed on an XZ grid for [`Self::push_out`]. The spheres are sorted by
/// their bit patterns and the buckets live in a `BTreeMap`, so two hosts given the same spheres
/// in any order push in the same order and get the same bits. Plain `f32` arithmetic and
/// `sqrt` only (IEEE-exact on every target).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SphereBlockers {
    spheres: Vec<ColliderSphere>,
    buckets: BTreeMap<(i32, i32), Vec<u32>>,
    settings: SphereBlockerSettings,
}

/// A total order on the `f32` bit patterns of a sphere (x, z, y, r).
fn order_key(s: &ColliderSphere) -> [u32; 4] {
    let key = |v: f32| {
        let bits = v.to_bits();
        if bits >> 31 == 1 {
            !bits
        } else {
            bits | 0x8000_0000
        }
    };
    [key(s.x), key(s.z), key(s.y), key(s.r)]
}

impl SphereBlockers {
    /// Sorts `spheres` (their order does not matter), drops non-finite or non-positive ones and
    /// duplicates, and builds the buckets.
    #[must_use]
    pub fn from_spheres(mut spheres: Vec<ColliderSphere>, settings: SphereBlockerSettings) -> Self {
        spheres.retain(|s| {
            s.x.is_finite() && s.y.is_finite() && s.z.is_finite() && s.r.is_finite() && s.r > 0.0
        });
        spheres.sort_by_key(order_key);
        spheres.dedup();
        let bucket_m = if settings.bucket_m.is_finite() && settings.bucket_m > 0.0 {
            settings.bucket_m
        } else {
            SphereBlockerSettings::default().bucket_m
        };
        let settings = SphereBlockerSettings { bucket_m, ..settings };
        let index = |v: f32| (v / bucket_m).floor() as i32;
        let mut buckets: BTreeMap<(i32, i32), Vec<u32>> = BTreeMap::new();
        for (i, s) in spheres.iter().enumerate() {
            let reach = s.r + settings.bucket_margin_m;
            for bz in index(s.z - reach)..=index(s.z + reach) {
                for bx in index(s.x - reach)..=index(s.x + reach) {
                    buckets.entry((bx, bz)).or_default().push(i as u32);
                }
            }
        }
        Self {
            spheres,
            buckets,
            settings,
        }
    }

    /// [`Self::from_spheres`] of baked colliders ([`SceneryCollider::sphere`]).
    #[must_use]
    pub fn from_colliders(colliders: &[SceneryCollider], settings: SphereBlockerSettings) -> Self {
        Self::from_spheres(colliders.iter().map(SceneryCollider::sphere).collect(), settings)
    }

    /// The spheres, sorted.
    #[must_use]
    pub fn spheres(&self) -> &[ColliderSphere] {
        &self.spheres
    }

    /// Number of spheres.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spheres.len()
    }

    /// True when there is no sphere.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spheres.is_empty()
    }

    /// The settings in use.
    #[must_use]
    pub fn settings(&self) -> SphereBlockerSettings {
        self.settings
    }

    /// Does `sphere` stop `body` with its feet at `feet_y`? It does while the rock's top is more
    /// than a step above the feet and its bottom is below the body's top.
    #[inline]
    #[must_use]
    pub fn blocks(sphere: &ColliderSphere, body: &BlockedBody, feet_y: f32) -> bool {
        sphere.top() > feet_y + body.step && sphere.y - sphere.r < feet_y + body.height
    }

    /// Pushes `(x, z)` out of every sphere that blocks `body` at `feet_y`: the body ends at least
    /// `r + radius + skin` from each rock's axis, unless it started deeper than
    /// [`SphereBlockerSettings::max_push_m`] inside (then it walks out over a few calls). Up to
    /// three passes over the body's bucket; the total move is capped per call.
    #[must_use]
    pub fn push_out(&self, x: f32, z: f32, feet_y: f32, body: &BlockedBody) -> [f32; 2] {
        if self.spheres.is_empty() {
            return [x, z];
        }
        let bucket_m = self.settings.bucket_m;
        let max_push = self.settings.max_push_m;
        let index = |v: f32| (v / bucket_m).floor() as i32;
        let (x0, z0) = (x, z);
        let mut x = x;
        let mut z = z;
        let reach = body.radius + body.skin;
        for _ in 0..3 {
            let mut moved = false;
            let Some(list) = self.buckets.get(&(index(x), index(z))) else {
                break;
            };
            for &i in list {
                let s = &self.spheres[i as usize];
                if !Self::blocks(s, body, feet_y) {
                    continue;
                }
                let dx = x - s.x;
                let dz = z - s.z;
                let want = s.r + reach;
                let d2 = dx * dx + dz * dz;
                if d2 >= want * want {
                    continue;
                }
                let d = d2.sqrt();
                let (nx, nz) = if d > 1e-4 { (dx / d, dz / d) } else { (1.0, 0.0) };
                let push = (want - d).min(max_push);
                x += nx * push;
                z += nz * push;
                moved = true;
            }
            if !moved {
                break;
            }
        }
        // The cap is per call, not per pass.
        let (mx, mz) = (x - x0, z - z0);
        let moved_m = (mx * mx + mz * mz).sqrt();
        if moved_m > max_push {
            let k = max_push / moved_m;
            return [x0 + mx * k, z0 + mz * k];
        }
        [x, z]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::TerrainMap;
    use crate::scatter::placement::WorldAnchor;
    use crate::scatter::set::ScatterMeshRef;

    /// A test body (not any game's values).
    const BODY: BlockedBody = BlockedBody {
        radius: 0.35,
        height: 1.8,
        step: 0.35,
        skin: 0.02,
    };

    fn boulder() -> ColliderSphere {
        // 2.4 m wide rock standing 1.6 m above the ground at y = 0.
        ColliderSphere {
            x: 10.0,
            y: 0.4,
            z: -5.0,
            r: 1.2,
        }
    }

    fn blockers(spheres: Vec<ColliderSphere>) -> SphereBlockers {
        SphereBlockers::from_spheres(spheres, SphereBlockerSettings::default())
    }

    fn dist(a: [f32; 2], s: &ColliderSphere) -> f32 {
        ((a[0] - s.x) * (a[0] - s.x) + (a[1] - s.z) * (a[1] - s.z)).sqrt()
    }

    fn walk_to(rocks: &SphereBlockers, from: [f32; 2], to: [f32; 2], feet_y: f32) -> [f32; 2] {
        // The body keeps walking the same way; the rock pushes it out each call.
        let (dx, dz) = (to[0] - from[0], to[1] - from[1]);
        let len = (dx * dx + dz * dz).sqrt();
        let steps = (len / 0.13).ceil() as i32;
        let (sx, sz) = (dx / steps as f32, dz / steps as f32);
        let mut at = from;
        for _ in 0..steps {
            at = [at[0] + sx, at[1] + sz];
            at = rocks.push_out(at[0], at[1], feet_y, &BODY);
        }
        at
    }

    #[test]
    fn a_body_walking_into_a_boulder_from_eight_directions_is_blocked() {
        let b = boulder();
        let rocks = blockers(vec![b]);
        let min = b.r + BODY.radius;
        for k in 0..8 {
            let a = k as f32 * std::f32::consts::FRAC_PI_4;
            let (ux, uz) = (a.cos(), a.sin());
            let from = [b.x + ux * 8.0, b.z + uz * 8.0];
            let to = [b.x - ux * 3.0, b.z - uz * 3.0];
            let end = walk_to(&rocks, from, to, 0.0);
            let d = dist(end, &b);
            assert!(
                d >= min - 1e-3,
                "direction {k} ended inside the rock: {d} < {min}"
            );
            assert!(d < min + 0.6, "direction {k} stopped far from the rock: {d}");
        }
    }

    #[test]
    fn a_rock_under_the_step_does_not_block() {
        // Top at 0.30 m: under the step height.
        let pebble = ColliderSphere {
            x: 0.0,
            y: -0.1,
            z: 0.0,
            r: 0.4,
        };
        let end = walk_to(&blockers(vec![pebble]), [-3.0, 0.0], [3.0, 0.0], 0.0);
        assert!(
            (end[0] - 3.0).abs() < 1e-4 && end[1].abs() < 1e-4,
            "walked through: {end:?}"
        );
    }

    #[test]
    fn the_step_rule_lets_a_body_above_the_top_minus_step_pass() {
        let b = boulder();
        let rocks = blockers(vec![b]);
        let top = b.top();
        assert!(SphereBlockers::blocks(&b, &BODY, top - BODY.step - 0.01));
        assert!(!SphereBlockers::blocks(&b, &BODY, top - BODY.step + 0.01));
        // A body wholly above the rock's bottom-to-top span never touches it either.
        assert!(!SphereBlockers::blocks(&b, &BODY, b.y - b.r - BODY.height - 0.01));
        let over = walk_to(&rocks, [b.x - 4.0, b.z], [b.x + 4.0, b.z], top - 0.2);
        assert!(
            (over[0] - (b.x + 4.0)).abs() < 1e-4,
            "a body above the rock crosses it: {over:?}"
        );
    }

    #[test]
    fn a_body_that_starts_inside_a_rock_is_walked_out_gently() {
        let b = boulder();
        let rocks = blockers(vec![b]);
        let cap = rocks.settings().max_push_m;
        let mut at = [b.x + 0.1, b.z];
        let mut last_step = 0.0f32;
        for _ in 0..20 {
            let next = rocks.push_out(at[0], at[1], 0.0, &BODY);
            last_step = ((next[0] - at[0]).powi(2) + (next[1] - at[1]).powi(2)).sqrt();
            assert!(last_step <= cap + 1e-4, "push {last_step} exceeds the cap");
            at = next;
        }
        assert!(dist(at, &b) >= b.r + BODY.radius - 1e-3, "still inside");
        assert!(last_step < 1e-4, "settled");
        // Dead centre takes a deterministic direction (+X).
        let c = rocks.push_out(b.x, b.z, 0.0, &BODY);
        assert!(c[0] > b.x && (c[1] - b.z).abs() < 1e-6);
    }

    #[test]
    fn loading_does_not_depend_on_the_order_of_the_spheres() {
        let s = |x, y, z, r| ColliderSphere { x, y, z, r };
        let (a, b, c) = (
            s(1.0, 0.0, 2.0, 1.0),
            s(-40.5, 1.0, 30.0, 2.0),
            s(17.0, 0.5, -3.0, 1.5),
        );
        let one = blockers(vec![a, b, c]);
        let two = blockers(vec![
            c,
            a,
            b,
            a,
            s(f32::NAN, 0.0, 0.0, 1.0),
            s(0.0, 0.0, 0.0, 0.0),
        ]);
        assert_eq!(one, two);
        assert_eq!(one.len(), 3);
        // Integer millimetres give the same spheres as their three-decimal text parsed.
        let baked = [
            SceneryCollider {
                x_mm: 17_000,
                z_mm: -3_000,
                y_mm: 500,
                r_mm: 1_500,
            },
            SceneryCollider {
                x_mm: 1_000,
                z_mm: 2_000,
                y_mm: 0,
                r_mm: 1_000,
            },
            SceneryCollider {
                x_mm: -40_500,
                z_mm: 30_000,
                y_mm: 1_000,
                r_mm: 2_000,
            },
        ];
        assert_eq!(
            SphereBlockers::from_colliders(&baked, SphereBlockerSettings::default()),
            one
        );
        let odd = SceneryCollider {
            x_mm: -1_234,
            z_mm: 7,
            y_mm: -1,
            r_mm: 999,
        };
        let parsed = |t: &str| t.parse::<f64>().expect("number") as f32;
        let s = odd.sphere();
        assert_eq!(
            [s.x, s.z, s.y, s.r].map(f32::to_bits),
            [
                parsed("-1.234"),
                parsed("0.007"),
                parsed("-0.001"),
                parsed("0.999")
            ]
            .map(f32::to_bits)
        );
    }

    #[test]
    fn a_rock_across_a_bucket_edge_blocks_from_both_sides() {
        let edge = ColliderSphere {
            x: SphereBlockerSettings::default().bucket_m,
            y: 0.4,
            z: 0.5,
            r: 1.2,
        };
        let rocks = blockers(vec![edge]);
        for sign in [-1.0f32, 1.0] {
            let end = walk_to(&rocks, [edge.x + sign * 6.0, 0.5], [edge.x, 0.5], 0.0);
            assert!((end[0] - edge.x).abs() >= edge.r + BODY.radius - 1e-3, "{end:?}");
        }
    }

    #[test]
    fn an_empty_set_moves_nothing() {
        let rocks = SphereBlockers::default();
        assert!(rocks.is_empty());
        assert_eq!(rocks.push_out(1.0, 2.0, 0.0, &BODY), [1.0, 2.0]);
        // A bad bucket size falls back to the default instead of dividing by zero.
        let bad = SphereBlockers::from_spheres(
            vec![boulder()],
            SphereBlockerSettings {
                bucket_m: 0.0,
                ..SphereBlockerSettings::default()
            },
        );
        assert_eq!(bad.settings().bucket_m, 16.0);
        let end = walk_to(&bad, [0.0, -5.0], [20.0, -5.0], 0.0);
        assert!(end[0] < 10.0 - 1.2 - BODY.radius + 1e-3, "{end:?}");
    }

    // ---- bake ----

    /// 129 × 129 samples over 256 m from integer arithmetic only (same bits everywhere).
    fn terraces(n: u32, width: f32, offset: i64) -> TerrainMap {
        let mut heights = Vec::new();
        for row in 0..n {
            for col in 0..n {
                let (c, r) = (i64::from(col) - offset, i64::from(row) - offset);
                heights.push(10.0 + (c * 3 + r * 5).rem_euclid(13) as f32 * 0.125);
            }
        }
        TerrainMap::from_heights(n, n, heights, width, 1.0, 0.0).expect("map")
    }

    fn rock_set() -> ScatterSet {
        let mesh = |name: &str, min, max| ScatterMeshRef {
            mesh: name.into(),
            weight: 1.0,
            min_scale: min,
            max_scale: max,
            scale_bias: 1.0,
        };
        ScatterSet {
            seed: 0x0a27_0008,
            classes: vec![
                ScatterClass {
                    name: "boulders".into(),
                    meshes: vec![mesh("boulder.glb", 0.5, 2.0), mesh("slab.glb", 0.8, 1.2)],
                    cell_size: 6.0,
                    density: 1.5,
                    slope_max: 1.2,
                    sink: 0.15,
                    tilt_jitter: 0.1,
                    ..ScatterClass::default()
                },
                ScatterClass {
                    name: "pebbles".into(),
                    meshes: vec![mesh("pebble.glb", 0.2, 0.6)],
                    cell_size: 2.0,
                    density: 2.0,
                    ..ScatterClass::default()
                },
            ],
            ..ScatterSet::default()
        }
    }

    fn extent_of(path: &str) -> Option<MeshExtent> {
        match path {
            "boulder.glb" => Some(MeshExtent::from_bounds([-0.8, 0.0, -0.6], [0.8, 1.4, 0.6])),
            "slab.glb" => Some(MeshExtent::from_bounds([-1.0, 0.0, -0.5], [1.0, 0.5, 0.5])),
            "pebble.glb" => Some(MeshExtent::from_bounds([-0.2, 0.0, -0.2], [0.2, 0.3, 0.2])),
            _ => None,
        }
    }

    fn inputs(map: &TerrainMap) -> PlacementInputs<'_> {
        PlacementInputs {
            map,
            holes: None,
            layers: None,
            density_maps: None,
        }
    }

    #[test]
    fn bake_keeps_tall_rocks_sorted_unique_and_matches_the_placement() {
        let map = terraces(129, 256.0, 0);
        let set = rock_set();
        let origin = PlacementOrigin::World(WorldAnchor::at([0.0, 0.0]));
        let settings = SceneryColliderSettings::default();
        let baked = bake_scenery_colliders(&set, inputs(&map), &origin, extent_of, &settings).expect("bake");
        assert!(baked.len() > 200, "{} colliders", baked.len());
        assert!(baked.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
        // Every collider is a boulder or slab instance of the full placement, recomputed by hand.
        let placed = place_set_with_origin(&set, inputs(&map), &origin).expect("place");
        let mut expected = Vec::new();
        for p in placed.iter().filter(|p| p.class == 0) {
            let e = extent_of(&set.classes[0].meshes[usize::from(p.mesh)].mesh).expect("extent");
            let rise = e.height * p.scale * (1.0 - 0.15) * p.up[1].clamp(0.2, 1.0);
            if rise <= settings.threshold_m {
                continue;
            }
            let r = 0.4 * p.scale * (e.footprint[0] * e.footprint[1]).sqrt();
            let top = p.position[1] + rise;
            expected.push(SceneryCollider {
                x_mm: mm(p.position[0]),
                z_mm: mm(p.position[2]),
                y_mm: mm(top - r),
                r_mm: mm(r),
            });
        }
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(baked, expected);
        // Pebbles never reach the threshold (0.3 m × 0.6 at most) and are not even placed.
        assert!(!class_can_collide(
            &set.classes[1],
            &[extent_of("pebble.glb").expect("e")],
            0.7
        ));
        // Some boulders stay under the threshold (scale bias toward small).
        assert!(expected.len() < placed.iter().filter(|p| p.class == 0).count());
    }

    #[test]
    fn bake_clips_and_mirrors() {
        let map = terraces(129, 256.0, 0);
        let set = rock_set();
        let origin = PlacementOrigin::World(WorldAnchor::at([0.0, 0.0]));
        let plain = SceneryColliderSettings::default();
        let all = bake_scenery_colliders(&set, inputs(&map), &origin, extent_of, &plain).expect("bake");
        let clipped = SceneryColliderSettings {
            clip_half_extent: Some([64.0, 32.0]),
            ..plain
        };
        let inner = bake_scenery_colliders(&set, inputs(&map), &origin, extent_of, &clipped).expect("bake");
        assert!(!inner.is_empty() && inner.len() < all.len());
        assert!(
            inner
                .iter()
                .all(|c| c.x_mm.abs() <= 64_000 && c.z_mm.abs() <= 32_000)
        );
        assert!(inner.iter().all(|c| all.binary_search(c).is_ok()));
        let mirrored = SceneryColliderSettings {
            mirror_x: true,
            ..plain
        };
        let flipped =
            bake_scenery_colliders(&set, inputs(&map), &origin, extent_of, &mirrored).expect("bake");
        let mut back: Vec<_> = flipped
            .iter()
            .map(|c| SceneryCollider { x_mm: -c.x_mm, ..*c })
            .collect();
        back.sort_unstable();
        assert_eq!(back, all);
    }

    #[test]
    fn bake_without_an_extent_is_an_error() {
        let map = terraces(65, 128.0, 0);
        let err = bake_scenery_colliders(
            &rock_set(),
            inputs(&map),
            &PlacementOrigin::MapEdge,
            |p: &str| (p != "slab.glb").then(|| MeshExtent::from_bounds([0.0; 3], [1.0; 3])),
            &SceneryColliderSettings::default(),
        )
        .expect_err("missing extent");
        assert!(
            matches!(err, ScatterError::Asset { ref path, .. } if path == "slab.glb"),
            "{err}"
        );
    }

    /// The point of Q-08 for colliders: a host bakes on the inner map while the drawn field
    /// places on a map grown by a backdrop ring, and the playable colliders agree exactly.
    #[test]
    fn backdrop_extension_moves_no_collider() {
        let inner = terraces(129, 256.0, 0);
        // 64 m ring on every side, same 2 m spacing, same samples inside.
        let extended = terraces(193, 384.0, 32);
        let set = rock_set();
        let origin = PlacementOrigin::World(WorldAnchor::pinned_to([0.0, 0.0], &inner));
        let settings = SceneryColliderSettings {
            // One sample in from the inner edge, where the inner normal is one-sided.
            clip_half_extent: Some([126.0, 126.0]),
            ..SceneryColliderSettings::default()
        };
        let a = bake_scenery_colliders(&set, inputs(&inner), &origin, extent_of, &settings).expect("bake");
        let b = bake_scenery_colliders(&set, inputs(&extended), &origin, extent_of, &settings).expect("bake");
        assert!(a.len() > 200);
        assert_eq!(a, b);
        // Edge-anchored, the same backdrop reshuffles them.
        let edge = |map: &TerrainMap| {
            bake_scenery_colliders(&set, inputs(map), &PlacementOrigin::MapEdge, extent_of, &settings)
                .expect("bake")
        };
        assert_ne!(edge(&inner), edge(&extended));
    }

    #[test]
    fn mesh_extent_from_an_asset_reads_lod0() {
        let style = crate::scatter::rock::RockPalette::Mars.style(0);
        let asset = crate::scatter::rock::procedural_rock(7, style);
        let e = MeshExtent::from_asset(&asset).expect("stand-in rock has vertices");
        assert!(
            e.height > 0.0 && e.footprint[0] > 0.0 && e.footprint[1] > 0.0,
            "{e:?}"
        );
    }
}
