// SPDX-License-Identifier: MPL-2.0
//! Occlusion and bounce proxies for large meshes (vehicles, hulls, containers) standing on the
//! ground.
//!
//! The mesh IBL's lower half and the terrain's sky term assume open, sunlit surroundings. Under a
//! parked hull that is wrong twice: the ground the hull's skirt looks down on lies in the hull's
//! own shadow (the IBL gave down-facing faces about 2× the reference path tracer's light), and the
//! ground in the hull's shadow sees the hull instead of the sky (no bounce off the hull, no
//! darkening at the contact). A host registers each large mesh as a [`MeshOccluder`] — an
//! oriented box with the mesh's average albedo — with
//! [`crate::FrameRenderer::set_mesh_occluders`]. Each frame the renderer packs them into a
//! [`MeshOccluderBlock`] the forward pass and the shared terrain pass read:
//!
//! - **Mesh receivers** (forward pass): the sun shadow of each box on its ground plane (a convex
//!   polygon) removes the sun's share of the ground radiance from the IBL ground half over the
//!   polygon's view factor; a box beside the receiver replaces the sky or ground it hides with its
//!   faces' radiance; the box shadows the sky body's fill.
//! - **Terrain receivers** (shared colour pass): each box replaces the sky it hides with its
//!   faces' radiance (the hull's bounce onto the ground and the contact darkening under it) and
//!   shadows the fill.
//!
//! View factors are exact for the planar polygons (Lambert's polygon formula, clipped to the
//! receiver's tangent plane); the face radiances are computed here once per box per frame from
//! the sun, the fill and the hemisphere sky/ground ambient. With no occluders the block's count is
//! zero and both passes skip the term (frames are unchanged).

use bytemuck::{Pod, Zeroable};
use regia_core::glam::{Mat3, Mat4, Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_scene::GlobalTransform;

use crate::runtime::{ExtractedFrame, MeshRenderer};

/// Most occluders a frame shades at full quality (the nearest to the eye are kept).
pub const MAX_MESH_OCCLUDERS: usize = 32;

/// Most far-band contact blobs a frame shades past the nearest [`MAX_MESH_OCCLUDERS`] (ADR-7970):
/// the next nearest proxies that opted in with [`MeshOccluder::far_band`]. Each costs 20 bytes of
/// the occluder uniform (2.5 KiB for all 128; the block stays under the 16 KiB mobile floor).
pub const MAX_FAR_MESH_OCCLUDERS: usize = 128;

/// Metres over which a far-band proxy's full-quality term fades into its contact blob as its
/// distance key nears the nearest far-band proxy's (ADR-7970): no hull pops at the
/// [`MAX_MESH_OCCLUDERS`] boundary.
pub const FAR_BAND_FADE: f32 = 5.0;

/// A contact blob is full inside this multiple of its footprint radius and gone at
/// `BLOB_OUTER` (`occ_blob` in the shaders).
const BLOB_INNER: f32 = 0.6;
const BLOB_OUTER: f32 = 1.25;

/// Smallest footprint radius and top a blob is shaded with, metres (a flat or zero box).
const BLOB_MIN: f32 = 0.05;

/// Most vertices of a box's sun-shadow polygon the block packs (a box projects
/// to at most a hexagon; clipping at the ground plane can add a seventh, which
/// the pack drops — the receivers use the disc approximation near the cap).
const MAX_SHADOW_VERTS: usize = 6;

/// Box-term influence ends between these multiples of the box's bounding radius (a face's
/// view factor below ~2 % there).
const BOX_FADE: [f32; 2] = [4.0, 5.5];

/// Exact (clipped polygon) box faces within this multiple of the box's largest half extent of
/// its surface; each face as a disc beyond.
const NEAR_FIELD: f32 = 1.5;

/// The exact shadow polygon within this multiple of its radius of its centre; a disc beyond.
const SHADOW_NEAR: f32 = 2.0;

/// Shadow-polygon influence ends between these multiples of the polygon's radius.
const SHADOW_FADE: [f32; 2] = [3.0, 4.5];

/// Angular radius (radians) of the soft fill shadow: the sky body's disc spread (Jupiter from
/// Europa is ~12° across).
pub const FILL_SHADOW_RADIUS: f32 = 0.1;

/// A large mesh's occlusion and bounce proxy: an oriented box with the mesh's average albedo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshOccluder {
    /// Box centre, world metres.
    pub center: [f32; 3],
    /// The box's unit axes (orthonormal).
    pub axes: [[f32; 3]; 3],
    /// Half extent along each axis, metres.
    pub half_extents: [f32; 3],
    /// Average linear albedo of the mesh's surfaces.
    pub albedo: [f32; 3],
    /// Height of the ground the mesh stands on (the plane its sun shadow falls on), metres.
    pub ground_y: f32,
    /// Past the nearest [`MAX_MESH_OCCLUDERS`], shade this proxy as a coarse ground-contact blob
    /// (one of the next [`MAX_FAR_MESH_OCCLUDERS`]) instead of dropping it, and fade its
    /// full-quality term into that blob near the boundary (ADR-7970). Off by default: a frame
    /// whose proxies leave it off is shaded exactly as before.
    pub far_band: bool,
}

struct OccluderBuildCache {
    occluders: Vec<MeshOccluder>,
    light: OccluderLighting,
    block: MeshOccluderBlock,
}

thread_local! {
    static HULL_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static OCCLUDER_BUILD: std::cell::RefCell<Option<OccluderBuildCache>> =
        const { std::cell::RefCell::new(None) };
}

fn cached_occluder_block(occluders: &[MeshOccluder], light: &OccluderLighting) -> Option<MeshOccluderBlock> {
    OCCLUDER_BUILD.with(|slot| {
        let cache = slot.borrow();
        let cache = cache.as_ref()?;
        (cache.light == *light && cache.occluders.as_slice() == occluders).then_some(cache.block)
    })
}

fn store_cached_occluders(occluders: &[MeshOccluder], light: &OccluderLighting, block: MeshOccluderBlock) {
    OCCLUDER_BUILD.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(cache) = slot.as_mut() {
            cache.occluders.clear();
            cache.occluders.extend_from_slice(occluders);
            cache.light = *light;
            cache.block = block;
        } else {
            *slot = Some(OccluderBuildCache {
                occluders: occluders.to_vec(),
                light: *light,
                block,
            });
        }
    });
}

#[cfg(test)]
fn reset_occluder_build_cache() {
    OCCLUDER_BUILD.with(|slot| *slot.borrow_mut() = None);
}

fn note_hull_call() {
    HULL_CALLS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(test)]
fn hull_calls() -> u64 {
    HULL_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn reset_hull_calls() {
    HULL_CALLS.with(|count| count.set(0));
}

impl MeshOccluder {
    /// The box bounding the object-space AABB `min`..`max` under `model` (column-major object
    /// to world), standing on its lowest corner.
    ///
    /// A general transform can shear the columns (a parent's non-uniform
    /// scale times a child's rotation): the packed axes must stay orthonormal
    /// — the receivers and the shaders slab-test in them — so the columns are
    /// orthonormalized (Gram–Schmidt, in X, Y, Z order) and each half extent
    /// refit by projecting the transformed corners onto the new axes. For a
    /// plain `T × R × S` the result is identical to scaling each normalized
    /// column by its length.
    #[must_use]
    pub fn from_local_bounds(model: [[f32; 4]; 4], min: [f32; 3], max: [f32; 3], albedo: [f32; 3]) -> Self {
        let m = Mat4::from_cols_array_2d(&model);
        let (lo, hi) = (Vec3::from_array(min), Vec3::from_array(max));
        let center = m.transform_point3((lo + hi) * 0.5);
        let half = (hi - lo) * 0.5;
        let cols = [Vec3::X, Vec3::Y, Vec3::Z].map(|unit| m.transform_vector3(unit));
        let mut axes = [[0.0; 3]; 3];
        let mut half_extents = [0.0; 3];
        for k in 0..3 {
            // Gram–Schmidt: drop the components along the earlier axes.
            let mut axis = cols[k];
            for prev in &axes[..k] {
                let prev = Vec3::from_array(*prev);
                axis -= prev * axis.dot(prev);
            }
            let axis = if axis.length_squared() > 1.0e-12 {
                axis.normalize()
            } else {
                // A degenerate (zero or parallel) column: pick any direction
                // perpendicular to the earlier axes so the frame stays
                // orthonormal; the extent refit below still bounds the
                // original corners.
                let mut pick = Vec3::X;
                for probe in [Vec3::X, Vec3::Y, Vec3::Z] {
                    let mut cand = probe;
                    for prev in &axes[..k] {
                        let prev = Vec3::from_array(*prev);
                        cand -= prev * cand.dot(prev);
                    }
                    if cand.length_squared() > 1.0e-8 {
                        pick = cand.normalize();
                        break;
                    }
                }
                pick
            };
            axes[k] = axis.to_array();
        }
        // Refit: the new box must hold the original parallelepiped, the convex
        // hull of centre ± Σ col_j · half_j, so each extent is the largest
        // projection of those corners onto the new axis.
        for (k, extent) in half_extents.iter_mut().enumerate() {
            let axis = Vec3::from_array(axes[k]);
            *extent = cols
                .iter()
                .zip(half.to_array())
                .map(|(col, h)| col.dot(axis).abs() * h)
                .sum::<f32>();
        }
        let mut occluder = Self {
            center: center.to_array(),
            axes,
            half_extents,
            albedo,
            ground_y: 0.0,
            far_band: false,
        };
        occluder.ground_y = occluder
            .corners()
            .iter()
            .map(|c| c.y)
            .fold(f32::INFINITY, f32::min);
        occluder
    }

    /// The box around `positions` (object space) under `model`, standing on its lowest corner.
    /// `None` for an empty mesh.
    #[must_use]
    pub fn from_positions(
        positions: impl IntoIterator<Item = [f32; 3]>,
        model: [[f32; 4]; 4],
        albedo: [f32; 3],
    ) -> Option<Self> {
        local_bounds(positions).map(|(lo, hi)| Self::from_local_bounds(model, lo, hi, albedo))
    }

    /// Sets the ground plane height (where the sun shadow falls), e.g. the terrain under the
    /// wheels when the box starts above it.
    #[must_use]
    pub fn with_ground(mut self, ground_y: f32) -> Self {
        self.ground_y = ground_y;
        self
    }

    /// Opts the proxy in or out of the far band ([`Self::far_band`]).
    #[must_use]
    pub fn with_far_band(mut self, far_band: bool) -> Self {
        self.far_band = far_band;
        self
    }

    /// The proxy's ground-contact blob: `[x, z, footprint radius, ground y, top above the
    /// ground]` (metres). The footprint radius is the box's XZ-projected half diagonal
    /// (`√Σ h_k² (1 − a_k.y²)`) and the top the box's highest point above `ground_y`; the shaders
    /// derive the same from a near item (`occ_item_blob`), so a proxy crossing the band
    /// boundary keeps its blob.
    #[must_use]
    pub fn contact_blob(&self) -> [f32; 5] {
        let mut r2 = 0.0;
        let mut up = 0.0;
        for k in 0..3 {
            let a = self.axis(k);
            let h = self.half_extents[k];
            r2 += h * h * (1.0 - a.y * a.y).max(0.0);
            up += a.y.abs() * h;
        }
        [
            self.center[0],
            self.center[2],
            r2.sqrt(),
            self.ground_y,
            self.center[1] + up - self.ground_y,
        ]
    }

    fn axis(&self, k: usize) -> Vec3 {
        Vec3::from_array(self.axes[k])
    }

    /// Bounding-sphere radius of the box.
    #[must_use]
    pub fn radius(&self) -> f32 {
        Vec3::from_array(self.half_extents).length()
    }

    /// The eight corners.
    fn corners(&self) -> [Vec3; 8] {
        let c = Vec3::from_array(self.center);
        let h = self.half_extents;
        std::array::from_fn(|i| {
            let s = |bit: usize| if i & (1 << bit) == 0 { -1.0 } else { 1.0 };
            c + self.axis(0) * h[0] * s(0) + self.axis(1) * h[1] * s(1) + self.axis(2) * h[2] * s(2)
        })
    }

    /// Face `f` (0..6: +A0, −A0, +A1, −A1, +A2, −A2): outward normal and the four corners in
    /// loop order.
    fn face(&self, f: usize) -> (Vec3, [Vec3; 4]) {
        let k = f / 2;
        let s = if f.is_multiple_of(2) { 1.0 } else { -1.0 };
        let a = self.axis(k) * s;
        let u = self.axis((k + 1) % 3) * self.half_extents[(k + 1) % 3];
        let v = self.axis((k + 2) % 3) * self.half_extents[(k + 2) % 3];
        let fc = Vec3::from_array(self.center) + a * self.half_extents[k];
        (a, [fc + u + v, fc + u - v, fc - u - v, fc - u + v])
    }

    /// The box's sun shadow on its ground plane: the convex hull of the corners
    /// projected along the light, counter-clockwise in XZ. Empty with the sun
    /// below ~3°. Corners below the ground plane are left out; an edge crossing
    /// the plane contributes its crossing point (a below-plane corner's own XZ
    /// is only right when the edge crossing the plane is vertical, as with the
    /// default lowest-corner ground, which never puts a corner below the plane).
    #[must_use]
    pub fn sun_shadow_polygon(&self, toward_sun: [f32; 3]) -> Vec<[f32; 2]> {
        let (pts, n) = self.sun_shadow_points(toward_sun);
        pts[..n].iter().map(|p| p.to_array()).collect()
    }

    /// The hull in a fixed buffer. `n` is the vertex count (at most eight; a box projects to
    /// at most six).
    fn sun_shadow_points(&self, toward_sun: [f32; 3]) -> ([Vec2; 8], usize) {
        let l = Vec3::from_array(toward_sun).normalize_or_zero();
        if l.y < 0.05 {
            return ([Vec2::ZERO; 8], 0);
        }
        let corners = self.corners();
        if corners.iter().all(|c| c.y >= self.ground_y) {
            // Every corner is on or above the ground plane (always so for the default
            // lowest-corner ground): the hull of the eight projected corners, no allocation.
            let mut pts = [Vec2::ZERO; 8];
            for (i, c) in corners.into_iter().enumerate() {
                let t = (c.y - self.ground_y).max(0.0) / l.y;
                let g = c - l * t;
                pts[i] = Vec2::new(g.x, g.z);
            }
            let n = convex_hull_8(&mut pts);
            return (pts, n);
        }
        let project = |p: Vec3| {
            let t = (p.y - self.ground_y) / l.y;
            let g = p - l * t;
            Vec2::new(g.x, g.z)
        };
        // A box has 12 edges; each endpoint pair differs in one corner bit.
        const EDGES: [(usize, usize); 12] = [
            (0, 1),
            (2, 3),
            (4, 5),
            (6, 7),
            (0, 2),
            (1, 3),
            (4, 6),
            (5, 7),
            (0, 4),
            (1, 5),
            (2, 6),
            (3, 7),
        ];
        let mut clipped: Vec<Vec2> = Vec::with_capacity(14);
        for (a, b) in EDGES {
            let (ca, cb) = (corners[a], corners[b]);
            let (a_up, b_up) = (ca.y >= self.ground_y, cb.y >= self.ground_y);
            if a_up {
                clipped.push(project(ca));
            }
            if a_up != b_up {
                let s = (self.ground_y - ca.y) / (cb.y - ca.y);
                clipped.push(project(ca.lerp(cb, s)));
            }
        }
        let hull = convex_hull(&clipped);
        let mut pts = [Vec2::ZERO; 8];
        let n = hull.len().min(pts.len());
        pts[..n].copy_from_slice(&hull[..n]);
        (pts, n)
    }
}

/// Marks a [`MeshRenderer`] entity (a parked vehicle, a hull, a container) as a large mesh
/// that shadows and bounces onto what is under it: [`extract_mesh_occluders`] turns it into a
/// [`MeshOccluder`] around the mesh's bounds at the entity's transform each frame.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct OcclusionProxy {
    /// Average linear albedo of the mesh; `None` takes the material's base colour.
    pub albedo: Option<[f32; 3]>,
    /// Height of the ground the mesh stands on; `None` takes the bounds' lowest corner (right
    /// for a vehicle whose wheels touch the ground).
    pub ground_y: Option<f32>,
    /// Shade the proxy as a far-band contact blob when it is not among the nearest
    /// [`MAX_MESH_OCCLUDERS`] ([`MeshOccluder::far_band`], ADR-7970). Off by default.
    #[reflect(default)]
    pub far_band: bool,
}

/// The object-space AABB of `positions`; `None` when there are none (or only NaNs).
fn local_bounds(positions: impl IntoIterator<Item = [f32; 3]>) -> Option<([f32; 3], [f32; 3])> {
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for p in positions {
        let p = Vec3::from_array(p);
        lo = lo.min(p);
        hi = hi.max(p);
    }
    lo.cmple(hi).all().then(|| (lo.to_array(), hi.to_array()))
}

/// Object-space bounds of the proxies' meshes by mesh allocation (`Arc::as_ptr`), with the run
/// that last used them. The `Weak` holds the allocation, so no other mesh can appear at that
/// address while the entry lives; a mesh edited through `Arc::make_mut` moves to a new
/// allocation for the same reason.
type ProxyBounds = std::collections::HashMap<
    usize,
    (
        std::sync::Weak<crate::runtime::CpuMesh>,
        Option<([f32; 3], [f32; 3])>,
        u64,
    ),
>;

/// Copies every [`OcclusionProxy`] entity into [`ExtractedFrame::mesh_occluders`] (runs after
/// [`crate::extract_frame`]). The bounds are the mesh's object-space AABB, scanned once per mesh
/// rather than every frame (ADR-385).
#[tracing::instrument(skip_all)]
pub fn extract_mesh_occluders(
    mut extracted: ResMut<ExtractedFrame>,
    proxies: Query<
        (
            &MeshRenderer,
            &OcclusionProxy,
            Option<&GlobalTransform>,
            crate::visibility::DrawVisibility,
        ),
        Without<regia_scene::layers::LayerParked>,
    >,
    mut bounds: Local<ProxyBounds>,
    mut run: Local<u64>,
) {
    let _span = tracing::debug_span!("extract_mesh_occluders").entered();
    extracted.mesh_occluders.clear();
    *run += 1;
    let now = *run;
    // Meshes no proxy used last run, or that nothing holds any more, give their entry back.
    bounds.retain(|_, (mesh, _, seen)| *seen + 1 >= now && mesh.strong_count() > 0);
    for (renderer, proxy, transform, inherited) in &proxies {
        if !crate::visibility::is_drawn(inherited) {
            continue;
        }
        let model = transform.map_or(Mat4::IDENTITY, |t| Mat4::from(t.0));
        let albedo = proxy.albedo.unwrap_or_else(|| {
            let c = renderer.material.base_color;
            [c[0], c[1], c[2]]
        });
        let entry = bounds
            .entry(std::sync::Arc::as_ptr(&renderer.mesh) as usize)
            .or_insert_with(|| {
                (
                    std::sync::Arc::downgrade(&renderer.mesh),
                    local_bounds(renderer.mesh.vertices.iter().map(|v| v.position)),
                    now,
                )
            });
        entry.2 = now;
        let local = entry.1;
        if let Some((lo, hi)) = local {
            let occluder = MeshOccluder::from_local_bounds(model.to_cols_array_2d(), lo, hi, albedo)
                .with_far_band(proxy.far_band);
            extracted.mesh_occluders.push(match proxy.ground_y {
                Some(y) => occluder.with_ground(y),
                None => occluder,
            });
        }
    }
}

/// Andrew's monotone chain over any point set (the clipped shadow of a box with corners below
/// its ground plane; the common case uses `convex_hull_8`).
fn convex_hull(points: &[Vec2]) -> Vec<Vec2> {
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    pts.dedup_by(|a, b| a.distance_squared(*b) < 1e-12);
    if pts.len() < 3 {
        return pts;
    }
    let cross = |o: Vec2, a: Vec2, b: Vec2| (a - o).perp_dot(b - o);
    let mut hull: Vec<Vec2> = Vec::with_capacity(pts.len() * 2);
    for pass in 0..2 {
        let start = hull.len();
        let iter: Box<dyn Iterator<Item = &Vec2>> = if pass == 0 {
            Box::new(pts.iter())
        } else {
            Box::new(pts.iter().rev())
        };
        for &p in iter {
            while hull.len() >= start + 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 1e-9 {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    hull
}

/// Andrew's monotone chain over the eight projected corners, written back into `pts`.
///
/// Same sort, dedup and cross test as the previous heap chain. A box's shadow has at most six
/// vertices; the buffer holds eight.
fn convex_hull_8(pts: &mut [Vec2; 8]) -> usize {
    note_hull_call();
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    let src = *pts;
    let mut n = 0;
    for p in src {
        if n == 0 || p.distance_squared(pts[n - 1]) >= 1e-12 {
            pts[n] = p;
            n += 1;
        }
    }
    if n < 3 {
        return n;
    }
    let cross = |o: Vec2, a: Vec2, b: Vec2| (a - o).perp_dot(b - o);
    let mut hull = [Vec2::ZERO; 16];
    let mut h = 0;
    for pass in 0..2 {
        let start = h;
        for step in 0..n {
            let p = if pass == 0 { pts[step] } else { pts[n - 1 - step] };
            while h >= start + 2 && cross(hull[h - 2], hull[h - 1], p) <= 1e-9 {
                h -= 1;
            }
            hull[h] = p;
            h += 1;
        }
        h -= 1;
    }
    let count = h.min(pts.len());
    pts[..count].copy_from_slice(&hull[..count]);
    count
}

/// Cosine-weighted view factor (`E / (π L)` for a uniform radiance `L`) from a point with unit
/// normal `n` to the convex planar polygon `verts` (relative to the point), clipped to the
/// point's upper hemisphere: Lambert's polygon formula `(1/2π) |Σ θ_i n·ĝ_i|` (Baum, Rushmeier &
/// Winget 1989; Arvo 1995). The CPU mirror of `occ_polygon_ff` in the shaders.
#[must_use]
pub fn polygon_view_factor(n: Vec3, verts: &[Vec3]) -> f32 {
    let count = verts.len();
    if count < 3 {
        return 0.0;
    }
    // A half-plane clips each edge to at most two vertices. Sixteen is the frame path (a shadow
    // has at most six); a longer polygon falls back to the heap.
    if count <= 16 {
        let mut clipped = [Vec3::ZERO; 32];
        let m = clip_to_hemisphere(n, verts, &mut clipped);
        return hemisphere_factor(n, &clipped[..m]);
    }
    let mut clipped = vec![Vec3::ZERO; count * 2];
    let m = clip_to_hemisphere(n, verts, &mut clipped);
    hemisphere_factor(n, &clipped[..m])
}

/// Clip `verts` to the half-space `normal · p >= 0`, writing at most two points per edge.
fn clip_to_hemisphere(normal: Vec3, verts: &[Vec3], clipped: &mut [Vec3]) -> usize {
    let count = verts.len();
    let mut m = 0;
    for i in 0..count {
        let a = verts[i];
        let b = verts[(i + 1) % count];
        let (da, db) = (a.dot(normal), b.dot(normal));
        if da >= 0.0 {
            clipped[m] = a;
            m += 1;
        }
        if (da >= 0.0) != (db >= 0.0) {
            clipped[m] = a.lerp(b, da / (da - db));
            m += 1;
        }
    }
    m
}

fn hemisphere_factor(normal: Vec3, clipped: &[Vec3]) -> f32 {
    let m = clipped.len();
    if m < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..m {
        let a = clipped[i] / clipped[i].length().max(1e-6);
        let b = clipped[(i + 1) % m] / clipped[(i + 1) % m].length().max(1e-6);
        let c = a.dot(b).clamp(-1.0, 1.0);
        let theta = c.acos();
        let s = (1.0 - c * c).max(1e-12).sqrt();
        sum += a.cross(b).dot(normal) * (theta / s);
    }
    sum.abs() * (0.5 / std::f32::consts::PI)
}

/// One occluder as the shaders read it (208 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct GpuMeshOccluder {
    /// Box centre xyz; w = ground plane height.
    pub center: [f32; 4],
    /// Unit axes xyz; w = half extent along each.
    pub axes: [[f32; 4]; 3],
    /// Outgoing radiance (cd/m², rgb) of the faces +A0, −A0, +A1, −A1, +A2, −A2. `w`: `[0]`
    /// bounding radius, `[1]` shadow vertex count, `[2]` / `[3]` shadow polygon centre x / z,
    /// `[4]` shadow polygon radius, `[5]` shadow polygon area (m²).
    pub faces: [[f32; 4]; 6],
    /// Sun shadow polygon on the ground plane: up to six XZ vertices, two per row.
    pub shadow: [[f32; 4]; 3],
}

/// Cells per side of the proxy grid ([`MeshOccluderBlock::cells`]).
pub const OCCLUDER_GRID: usize = 32;

/// The frame's occluders and the light they are shaded with, as the forward pass
/// (`lab_forward.wgsl`, group 0 binding 23) and the terrain's shared pass read it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct MeshOccluderBlock {
    /// x = occluder count (0: the passes skip the term), y = the fill shadow's angular radius,
    /// z = far-band blob count ([`Self::far`]), w = proxies dropped (neither band; the shaders
    /// do not read it).
    pub header: [f32; 4],
    /// rgb: sky radiance behind a box (the cosine-weighted sky, `E_up / π`, cd/m²).
    pub sky: [f32; 4],
    /// rgb: the IBL bake's ground radiance (cd/m²).
    pub ground: [f32; 4],
    /// rgb: what a patch of [`Self::ground`] in a box's sun shadow lacks: the sun's share, less
    /// the sky body's fill the patch still receives.
    pub ground_sun: [f32; 4],
    /// xyz toward the sky body's fill, w = 1 when the frame has one.
    pub fill_dir: [f32; 4],
    /// The proxy grid over the XZ bounds of every reach sphere: xy = its origin (m), z =
    /// cells per metre (0: no grid). Points outside it are out of every proxy's reach. w = the
    /// far blobs' largest XZ reach (m; 0 without a far band).
    pub grid: [f32; 4],
    /// Per cell of the [`OCCLUDER_GRID`]² grid (row-major, z rows), the occluders whose reach
    /// sphere's XZ disc meets it (bit `i` = occluder `i`), four cells per row: a pixel tests
    /// only these spheres ([`Self::cell_mask`]).
    pub cells: [[u32; 4]; OCCLUDER_GRID * OCCLUDER_GRID / 4],
    /// Per occluder, a sphere (centre xyz, radius w) holding every point its terms reach (the
    /// box term and the shadow polygon's): the passes test these first and shade only the
    /// occluders whose sphere holds the pixel.
    pub bounds: [[f32; 4]; MAX_MESH_OCCLUDERS],
    /// Per occluder, its sun shadow polygon as reflections test it first: xy = centre (x, z),
    /// z = radius (−1 without a polygon of three or more vertices), w = ground height.
    pub shadow_bounds: [[f32; 4]; MAX_MESH_OCCLUDERS],
    /// The occluders, nearest to the eye first; `header.x` are valid.
    pub items: [GpuMeshOccluder; MAX_MESH_OCCLUDERS],
    /// Per occluder (four per row), how far its full-quality term has faded into its contact
    /// blob near the far band's boundary: 0 = the full term and no blob (every slot unless a far
    /// band is shaded), 1 = the blob alone (ADR-7970).
    pub near_fade: [[f32; 4]; MAX_MESH_OCCLUDERS / 4],
    /// The far band's contact blobs ([`MeshOccluder::contact_blob`]), sorted by x: x, z =
    /// footprint centre, z = footprint radius, w = ground height; `header.z` are valid.
    pub far: [[f32; 4]; MAX_FAR_MESH_OCCLUDERS],
    /// Per far blob (four per row), its top above its ground.
    pub far_top: [[f32; 4]; MAX_FAR_MESH_OCCLUDERS / 4],
}

/// How a frame's proxies split between the bands (`render.mesh_occluders` `bands`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OccluderBands {
    /// Shaded at full quality (at most [`MAX_MESH_OCCLUDERS`]).
    pub near: usize,
    /// Shaded as far-band contact blobs (at most [`MAX_FAR_MESH_OCCLUDERS`]).
    pub far: usize,
    /// Neither: past the nearest [`MAX_MESH_OCCLUDERS`] without [`MeshOccluder::far_band`], past
    /// the far band's capacity, or with a non-finite centre or extent.
    pub dropped: usize,
}

impl Default for MeshOccluderBlock {
    fn default() -> Self {
        Self::zeroed()
    }
}

impl MeshOccluderBlock {
    /// Moves every position in the block by `by`: a reader that works in another space than the
    /// frame's local one (terrain after a floating-origin rebase, spec 78 § 12.2). A grid cell is
    /// an offset from the grid's origin, so the masks stay.
    pub fn shift(&mut self, by: [f32; 3]) {
        let count = (self.header[0] as usize).min(MAX_MESH_OCCLUDERS);
        self.grid[0] += by[0];
        self.grid[1] += by[2];
        for bound in &mut self.bounds[..count] {
            for (value, amount) in bound.iter_mut().zip(by) {
                *value += amount;
            }
        }
        for bound in &mut self.shadow_bounds[..count] {
            bound[0] += by[0];
            bound[1] += by[2];
            bound[3] += by[1];
        }
        let far = (self.header[2] as usize).min(MAX_FAR_MESH_OCCLUDERS);
        for blob in &mut self.far[..far] {
            blob[0] += by[0];
            blob[1] += by[2];
            blob[3] += by[1];
        }
        for item in &mut self.items[..count] {
            for (value, amount) in item.center.iter_mut().zip(by) {
                *value += amount;
            }
            item.center[3] += by[1];
            item.faces[2][3] += by[0];
            item.faces[3][3] += by[2];
            for row in &mut item.shadow {
                row[0] += by[0];
                row[1] += by[2];
                row[2] += by[0];
                row[3] += by[2];
            }
        }
    }
}

const _: () = assert!(size_of::<GpuMeshOccluder>() == 208);
const _: () = assert!(
    size_of::<MeshOccluderBlock>()
        == 96
            + 4 * OCCLUDER_GRID * OCCLUDER_GRID
            + (32 + 208 + 4) * MAX_MESH_OCCLUDERS
            + (16 + 4) * MAX_FAR_MESH_OCCLUDERS
);
// The block is one uniform binding: it must fit the mobile tier's 16 KiB floor
// (`MinspecProfile::Mobile`).
const _: () = assert!(size_of::<MeshOccluderBlock>() <= 16 << 10);

/// The light the proxies are shaded with this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OccluderLighting {
    /// Unit vector toward the sun.
    pub toward_sun: [f32; 3],
    /// Sun colour × illuminance (lux).
    pub sun: [f32; 3],
    /// Hemisphere ambient `[sky, ground]` radiance (`SharedFrameInfo::sky_ambient`).
    pub sky_ambient: [[f32; 3]; 2],
    /// The sky body's fill `[toward, colour × lux]`.
    pub fill: Option<[[f32; 3]; 2]>,
    /// The eye (nearest occluders are kept).
    pub eye: [f32; 3],
}

impl MeshOccluderBlock {
    /// Number of occluders the passes read.
    #[must_use]
    pub fn count(&self) -> usize {
        self.header[0] as usize
    }

    /// Number of far-band contact blobs the passes read.
    #[must_use]
    pub fn far_count(&self) -> usize {
        self.header[2] as usize
    }

    /// The near / far / dropped split of the proxies this block was built from.
    #[must_use]
    pub fn bands(&self) -> OccluderBands {
        OccluderBands {
            near: self.count(),
            far: self.far_count(),
            dropped: self.header[3] as usize,
        }
    }

    /// How far near occluder `i`'s full-quality term has faded into its contact blob
    /// ([`Self::near_fade`]).
    #[must_use]
    pub fn near_fade_of(&self, i: usize) -> f32 {
        self.near_fade.get(i / 4).map_or(0.0, |row| row[i % 4])
    }

    /// The occluders whose reach-sphere cell holds `p` (bit `i` = occluder `i`; a superset of
    /// the spheres that hold it), CPU mirror of `mesh_occluder_cell_mask` in the shaders.
    #[must_use]
    pub fn cell_mask(&self, p: Vec3) -> u32 {
        let [ox, oz, per_m, _] = self.grid;
        let n = OCCLUDER_GRID as f32;
        let (x, z) = ((p.x - ox) * per_m, (p.z - oz) * per_m);
        if !(x >= 0.0 && z >= 0.0 && x <= n && z <= n) {
            return 0;
        }
        let cell = (z as usize).min(OCCLUDER_GRID - 1) * OCCLUDER_GRID + (x as usize).min(OCCLUDER_GRID - 1);
        self.cells[cell / 4][cell % 4]
    }

    /// Fills [`Self::grid`] and [`Self::cells`] from the first `count` [`Self::bounds`].
    fn build_grid(&mut self, count: usize) {
        let spheres = &self.bounds[..count];
        if spheres.is_empty() {
            return;
        }
        // The XZ bounds of every sphere, a little wider so rounding never puts a point of a
        // sphere outside the grid.
        let lo = spheres.iter().fold(Vec2::splat(f32::INFINITY), |m, b| {
            m.min(Vec2::new(b[0] - b[3], b[2] - b[3]))
        }) - Vec2::splat(0.1);
        let hi = spheres.iter().fold(Vec2::splat(f32::NEG_INFINITY), |m, b| {
            m.max(Vec2::new(b[0] + b[3], b[2] + b[3]))
        }) + Vec2::splat(0.1);
        let cell = ((hi - lo).max_element() / OCCLUDER_GRID as f32).max(1e-3);
        self.grid = [lo.x, lo.y, 1.0 / cell, 0.0];
        let last = OCCLUDER_GRID as i64 - 1;
        let index = |v: f32| (((v / cell).floor() as i64).clamp(0, last)) as usize;
        for (i, b) in spheres.iter().enumerate() {
            let (c, r) = (Vec2::new(b[0], b[2]) - lo, b[3]);
            for z in index(c.y - r)..=index(c.y + r) {
                for x in index(c.x - r)..=index(c.x + r) {
                    // The disc meets the cell's square (closest point within r).
                    let min = Vec2::new(x as f32, z as f32) * cell;
                    let closest = c.clamp(min, min + Vec2::splat(cell));
                    if closest.distance_squared(c) <= r * r * 1.0001 + 1e-6 {
                        let k = z * OCCLUDER_GRID + x;
                        self.cells[k / 4][k % 4] |= 1 << i;
                    }
                }
            }
        }
    }

    /// Packs `occluders` (nearest [`MAX_MESH_OCCLUDERS`] to the eye) with their face radiance,
    /// and the next nearest [`MAX_FAR_MESH_OCCLUDERS`] that opted into the far band
    /// ([`MeshOccluder::far_band`]) as contact blobs.
    #[must_use]
    pub fn build(occluders: &[MeshOccluder], light: &OccluderLighting) -> Self {
        if occluders.is_empty() {
            return Self::default();
        }
        if let Some(block) = cached_occluder_block(occluders, light) {
            return block;
        }
        let block = assemble_occluder_block(occluders, light);
        store_cached_occluders(occluders, light, block);
        block
    }
}

fn assemble_occluder_block(occluders: &[MeshOccluder], light: &OccluderLighting) -> MeshOccluderBlock {
    let mut block = MeshOccluderBlock::default();
    let pi = std::f32::consts::PI;
    let l = Vec3::from_array(light.toward_sun).normalize_or_zero();
    let [sky, ground] = light.sky_ambient;
    // What a patch of ground in a box's sun shadow lacks: the sun's share of the bake's
    // ground radiance (albedo ρ = ground / (E_sun + sky)), less the sky body's fill the
    // shadowed patch still gets (the bake leaves the fill out of lit and shadowed ground
    // alike; under a hull on Europa it is most of the light).
    let fill_ground: [f32; 3] = light.fill.map_or([0.0; 3], |[d, col]| {
        let fy = Vec3::from_array(d).normalize_or_zero().y.max(0.0);
        col.map(|c| c * fy / pi)
    });
    let ground_sun: [f32; 3] = std::array::from_fn(|c| {
        let e_sun = light.sun[c] * l.y.max(0.0) / pi;
        let total = e_sun + sky[c];
        if total > 0.0 {
            (ground[c] * (e_sun - fill_ground[c]) / total).max(0.0)
        } else {
            0.0
        }
    });
    block.sky = [sky[0], sky[1], sky[2], 0.0];
    block.ground = [ground[0], ground[1], ground[2], 0.0];
    block.ground_sun = [ground_sun[0], ground_sun[1], ground_sun[2], 0.0];
    if let Some([dir, _]) = light.fill {
        let d = Vec3::from_array(dir).normalize_or_zero();
        block.fill_dir = [d.x, d.y, d.z, 1.0];
    }
    let eye = Vec3::from_array(light.eye);
    let mut order: Vec<(f32, &MeshOccluder)> = occluders
        .iter()
        .filter(|o| {
            o.center
                .iter()
                .chain(o.half_extents.iter())
                .all(|v| v.is_finite())
        })
        .map(|o| ((Vec3::from_array(o.center) - eye).length() - o.radius(), o))
        .collect();
    order.sort_by(|a, b| a.0.total_cmp(&b.0));
    // The far band (ADR-7970): the next nearest opted-in proxies past the full-quality cap, as
    // contact blobs sorted by x (the shaders binary-search the x range a pixel can reach).
    let mut far: Vec<(f32, [f32; 5])> = order
        .iter()
        .skip(MAX_MESH_OCCLUDERS)
        .filter(|(_, o)| o.far_band)
        .take(MAX_FAR_MESH_OCCLUDERS)
        .map(|(key, o)| (*key, o.contact_blob()))
        .collect();
    // The nearest far-band proxy's key: an opted-in near proxy within `FAR_BAND_FADE` of it
    // fades into its blob, reaching the blob alone where the two keys meet.
    let cut = far.first().map(|(key, _)| *key);
    far.sort_by(|a, b| a.1[0].total_cmp(&b.1[0]));
    let mut far_reach: f32 = 0.0;
    for (i, (_, blob)) in far.iter().enumerate() {
        block.far[i] = [blob[0], blob[1], blob[2], blob[3]];
        block.far_top[i / 4][i % 4] = blob[4];
        far_reach = far_reach.max(BLOB_OUTER * blob[2].max(BLOB_MIN));
    }
    let far_count = far.len();
    let mut count = 0;
    for (((slot, bound), shadow), (key, occ)) in block
        .items
        .iter_mut()
        .zip(block.bounds.iter_mut())
        .zip(block.shadow_bounds.iter_mut())
        .zip(order.iter().copied())
    {
        if let Some(cut) = cut
            && occ.far_band
        {
            block.near_fade[count / 4][count % 4] = smoothstep(cut - FAR_BAND_FADE, cut, key);
        }
        *slot = gpu_occluder(occ, l, light, ground_sun);
        *bound = reach(slot);
        let radius = if slot.faces[1][3] >= 3.0 {
            slot.faces[4][3]
        } else {
            -1.0
        };
        *shadow = [slot.faces[2][3], slot.faces[3][3], radius, slot.center[3]];
        count += 1;
    }
    let dropped = occluders.len().saturating_sub(count + far_count);
    block.header = [count as f32, FILL_SHADOW_RADIUS, far_count as f32, dropped as f32];
    block.build_grid(count);
    block.grid[3] = far_reach;
    block
}

/// A sphere holding every point the occluder's terms reach: the box term within
/// `BOX_FADE[1]` bounding radii of its centre, the shadow polygon's within `SHADOW_FADE[1]` of
/// its radii of its centre (the smallest sphere around both).
fn reach(occ: &GpuMeshOccluder) -> [f32; 4] {
    let c = Vec3::from_slice(&occ.center[..3]);
    let r = BOX_FADE[1] * occ.faces[0][3];
    let (c, r) = if occ.faces[1][3] >= 3.0 {
        let pc = Vec3::new(occ.faces[2][3], occ.center[3], occ.faces[3][3]);
        let rp = SHADOW_FADE[1] * occ.faces[4][3].max(0.1);
        let d = c.distance(pc);
        if d + rp <= r {
            (c, r)
        } else if d + r <= rp {
            (pc, rp)
        } else {
            let big = 0.5 * (d + r + rp);
            (c + (pc - c) * ((big - r) / d), big)
        }
    } else {
        (c, r)
    };
    // Margin for rounding in the shaders' distance tests.
    [c.x, c.y, c.z, r * 1.001 + 0.01]
}

fn gpu_occluder(
    occ: &MeshOccluder,
    l: Vec3,
    light: &OccluderLighting,
    ground_sun: [f32; 3],
) -> GpuMeshOccluder {
    let pi = std::f32::consts::PI;
    let [sky, ground] = light.sky_ambient;
    let mut out = GpuMeshOccluder::zeroed();
    out.center = [occ.center[0], occ.center[1], occ.center[2], occ.ground_y];
    for k in 0..3 {
        let a = occ.axis(k);
        out.axes[k] = [a.x, a.y, a.z, occ.half_extents[k]];
    }
    let (shadow_pts, shadow_len) = occ.sun_shadow_points(light.toward_sun);
    let shadow_n = shadow_len.min(MAX_SHADOW_VERTS);
    let shadow = &shadow_pts[..shadow_n];
    let mut shadow3 = [Vec3::ZERO; MAX_SHADOW_VERTS];
    for (i, p) in shadow.iter().enumerate() {
        shadow3[i] = Vec3::new(p.x, occ.ground_y, p.y);
    }
    for (f, face) in out.faces.iter_mut().enumerate() {
        let (n, corners) = occ.face(f);
        let centre = corners.iter().copied().sum::<Vec3>() * 0.25 + n * 0.02;
        // Sample the ground shadow from just above the plane: a bottom face that
        // lies in the plane (the default lowest-corner ground puts it there)
        // would sample 2 cm below it, fail the height test, and be shaded as if
        // it saw the full sunlit ground.
        let centre = Vec3::new(centre.x, centre.y.max(occ.ground_y + 0.02), centre.z);
        // Share of the face's ground view that lies in the box's own shadow.
        let ground_view = (0.5 - 0.5 * n.y).max(0.0);
        let in_shadow = if ground_view > 1e-3 && centre.y > occ.ground_y && shadow_n >= 3 {
            let mut rel = [Vec3::ZERO; MAX_SHADOW_VERTS];
            for i in 0..shadow_n {
                rel[i] = shadow3[i] - centre;
            }
            (polygon_view_factor(n, &rel[..shadow_n]) / ground_view).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let sun_cos = n.dot(l).max(0.0);
        let fill_cos = light.fill.map_or(0.0, |[d, _]| {
            n.dot(Vec3::from_array(d).normalize_or_zero()).max(0.0)
        });
        let up = 0.5 + 0.5 * n.y;
        for c in 0..3 {
            let fill = light.fill.map_or(0.0, |[_, col]| col[c]);
            let ground_seen = ground[c] - ground_sun[c] * in_shadow;
            let e = light.sun[c] * sun_cos + fill * fill_cos + pi * (sky[c] * up + ground_seen * (1.0 - up));
            face[c] = occ.albedo[c] * e / pi;
        }
    }
    out.faces[0][3] = occ.radius();
    out.faces[1][3] = shadow_n as f32;
    if shadow_n > 0 {
        let centre = shadow.iter().copied().fold(Vec2::ZERO, |s, p| s + p) / shadow_n as f32;
        let radius = shadow.iter().map(|p| p.distance(centre)).fold(0.0, f32::max);
        out.faces[2][3] = centre.x;
        out.faces[3][3] = centre.y;
        out.faces[4][3] = radius;
        // Shoelace area.
        out.faces[5][3] = 0.5
            * (0..shadow_n)
                .map(|i| {
                    let (a, b) = (shadow[i], shadow[(i + 1) % shadow_n]);
                    a.x * b.y - b.x * a.y
                })
                .sum::<f32>()
                .abs();
        for (i, p) in shadow.iter().enumerate() {
            out.shadow[i / 2][(i % 2) * 2] = p.x;
            out.shadow[i / 2][(i % 2) * 2 + 1] = p.y;
        }
    }
    out
}

/// The light a point receives from the block's occluders, CPU mirror of
/// `mesh_occluder_light` in the shaders: `(added irradiance / π, rgb; fill visibility)`.
/// `terrain` selects the terrain receiver (no ground-shadow polygon; the hidden light is sky).
#[must_use]
pub fn occluder_light(block: &MeshOccluderBlock, p: Vec3, n: Vec3, terrain: bool) -> ([f32; 3], f32) {
    let mut delta = Vec3::ZERO;
    let mut fill_vis = 1.0;
    let sky = Vec3::from_slice(&block.sky[..3]);
    let ground = Vec3::from_slice(&block.ground[..3]);
    let ground_sun = Vec3::from_slice(&block.ground_sun[..3]);
    let fill_dir = Vec3::from_slice(&block.fill_dir[..3]);
    let n_occ = block.count().min(MAX_MESH_OCCLUDERS);
    let listed = block.cell_mask(p);
    for (index, (occ, bound)) in block.items.iter().zip(&block.bounds).take(n_occ).enumerate() {
        if listed & (1 << index) == 0
            || p.distance_squared(Vec3::from_slice(&bound[..3])) >= bound[3] * bound[3]
        {
            continue;
        }
        // Near the far band's boundary the full term fades into the contact blob (ADR-7970).
        let fade = block.near_fade_of(index);
        let full = 1.0 - fade;
        let mut fill_near = 1.0;
        let c = Vec3::from_slice(&occ.center[..3]);
        let r = occ.faces[0][3];
        let axes: [Vec3; 3] = std::array::from_fn(|k| Vec3::from_slice(&occ.axes[k][..3]));
        let half: [f32; 3] = std::array::from_fn(|k| occ.axes[k][3]);
        let d = p.distance(c);
        if d < BOX_FADE[1] * r {
            let w = (1.0 - smoothstep(BOX_FADE[0] * r, BOX_FADE[1] * r, d)) * full;
            let mut q = p;
            let local: [f32; 3] = std::array::from_fn(|k| (p - c).dot(axes[k]));
            let inside = (0..3).all(|k| local[k].abs() < half[k] + 0.02);
            if inside && terrain {
                // Ground under the box (between the wheels): seen from just below its bottom.
                let depth = (0..3).map(|k| axes[k].y.abs() * half[k]).sum::<f32>();
                q.y = c.y - depth - 0.01;
            }
            if !inside || terrain {
                let rel = q - c;
                let local_q = Vec3::new(rel.dot(axes[0]), rel.dot(axes[1]), rel.dot(axes[2]));
                let surface = (local_q.abs() - Vec3::from_array(half)).max(Vec3::ZERO).length();
                let near = surface < NEAR_FIELD * half[0].max(half[1]).max(half[2]);
                for k in 0..3 {
                    let l = rel.dot(axes[k]);
                    if l.abs() <= half[k] + 1e-4 {
                        continue;
                    }
                    let s = if l > 0.0 { 1.0 } else { -1.0 };
                    let f = 2 * k + usize::from(l < 0.0);
                    let u = axes[(k + 1) % 3] * half[(k + 1) % 3];
                    let v = axes[(k + 2) % 3] * half[(k + 2) % 3];
                    let fc = c + axes[k] * (s * half[k]) - q;
                    let ff = if near {
                        polygon_view_factor(n, &[fc + u + v, fc + u - v, fc - u - v, fc - u + v])
                    } else {
                        disc_view_factor(4.0 * u.length() * v.length(), fc, axes[k] * s, n)
                    };
                    if ff <= 0.0 {
                        continue;
                    }
                    let hidden = if terrain {
                        sky
                    } else {
                        let t = smoothstep(-0.15, 0.15, fc.normalize_or_zero().y);
                        (ground - ground_sun * 0.5).lerp(sky, t)
                    };
                    delta += (Vec3::from_slice(&occ.faces[f][..3]) - hidden) * (ff * w);
                }
                // The fill's rays only reach the box when its bounding sphere meets their cone.
                let cq = c - q;
                let along = cq.dot(fill_dir);
                let reach = r + along.max(0.0) * block.header[1] * 0.7 * 1.05 + 0.01;
                let cone = along > -r && cq.length_squared() - along * along < reach * reach;
                if cone && block.fill_dir[3] > 0.5 && n.dot(fill_dir) > 0.0 {
                    fill_near = 1.0 - fill_hits(q, fill_dir, block.header[1], c, &axes, &half) / 5.0;
                }
            }
        }
        let verts = occ.faces[1][3] as usize;
        let ground_y = occ.center[3];
        if !terrain && verts >= 3 && p.y > ground_y + 0.01 {
            let pc = Vec3::new(occ.faces[2][3], ground_y, occ.faces[3][3]);
            let rp = occ.faces[4][3].max(0.1);
            let dp = p.distance(pc);
            if dp < SHADOW_FADE[1] * rp {
                let w = (1.0 - smoothstep(SHADOW_FADE[0] * rp, SHADOW_FADE[1] * rp, dp)) * full;
                let ff = if dp < SHADOW_NEAR * rp {
                    let poly: Vec<Vec3> = (0..verts.min(MAX_SHADOW_VERTS))
                        .map(|i| {
                            let row = occ.shadow[i / 2];
                            Vec3::new(row[(i % 2) * 2], ground_y, row[(i % 2) * 2 + 1]) - p
                        })
                        .collect();
                    polygon_view_factor(n, &poly)
                } else {
                    disc_view_factor(occ.faces[5][3], pc - p, Vec3::Y, n)
                };
                delta -= ground_sun * (ff * w);
            }
        }
        if fade > 0.0 {
            // The rest is the proxy's contact blob: it hides the sky and, mixed with the full
            // term's fill shadow, the fill.
            let o = blob_occlusion(p, n, item_blob(occ));
            delta -= sky * (fade * o);
            fill_vis *= (1.0 - o) + (fill_near - (1.0 - o)) * full;
        } else {
            fill_vis *= fill_near;
        }
    }
    let (hidden, vis) = far_blobs(block, p, n);
    delta -= sky * hidden;
    fill_vis *= vis;
    (delta.to_array(), fill_vis)
}

/// The contact blob of near item `occ` (CPU mirror of `occ_item_blob`): the same figures
/// [`MeshOccluder::contact_blob`] gives the proxy it was packed from.
fn item_blob(occ: &GpuMeshOccluder) -> [f32; 5] {
    let mut r2 = 0.0;
    let mut up = 0.0;
    for axis in &occ.axes {
        r2 += axis[3] * axis[3] * (1.0 - axis[1] * axis[1]).max(0.0);
        up += axis[1].abs() * axis[3];
    }
    [
        occ.center[0],
        occ.center[2],
        r2.sqrt(),
        occ.center[3],
        occ.center[1] + up - occ.center[3],
    ]
}

/// How much of the sky and fill a contact blob (`[x, z, footprint radius, ground y, top]`)
/// hides at `p` with normal `n` (CPU mirror of `occ_blob`): the on-axis view factor of a disc of
/// the footprint radius `R` at half the top `h` above the receiver, `R² / (R² + h²)` (a
/// differential area to a parallel coaxial disc; Howell's catalogue), full within `0.6 R` of
/// the footprint centre and gone at `1.25 R`, faded out above half the top and by the
/// receiver's cosine toward the disc. 0 outside the footprint, so every other point is
/// unchanged.
fn blob_occlusion(p: Vec3, n: Vec3, blob: [f32; 5]) -> f32 {
    let [x, z, radius, ground_y, top] = blob;
    let (radius, top) = (radius.max(BLOB_MIN), top.max(BLOB_MIN));
    let d = Vec2::new(p.x - x, p.z - z).length();
    let radial = 1.0 - smoothstep(BLOB_INNER * radius, BLOB_OUTER * radius, d);
    let vertical = 1.0 - smoothstep(0.5 * top, top, (p.y - ground_y).abs());
    let h = 0.5 * top;
    let strength = radius * radius / (radius * radius + h * h);
    strength * radial * vertical * n.y.clamp(0.0, 1.0)
}

/// The far band's blobs at `p` (CPU mirror of the far loop in `mesh_occluder_light`): the sky
/// they hide (summed, a share of the sky radiance) and the fill visibility they leave (the
/// product of each blob's). The blobs are sorted by x: only those within the largest reach of
/// `p.x` are tested.
fn far_blobs(block: &MeshOccluderBlock, p: Vec3, n: Vec3) -> (f32, f32) {
    let far = block.far_count().min(MAX_FAR_MESH_OCCLUDERS);
    let reach = block.grid[3];
    let (mut hidden, mut vis) = (0.0, 1.0);
    if far == 0 {
        return (hidden, vis);
    }
    let start = block.far[..far].partition_point(|b| b[0] < p.x - reach);
    for (i, b) in block.far[..far].iter().enumerate().skip(start) {
        if b[0] > p.x + reach {
            break;
        }
        let o = blob_occlusion(p, n, [b[0], b[1], b[2], b[3], block.far_top[i / 4][i % 4]]);
        hidden += o;
        vis *= 1.0 - o;
    }
    (hidden, vis)
}

/// The far band's shade at `p` (normal `n`): the share of the sky and fill the contact blobs of
/// the far proxies and of the near proxies fading into them leave (1 = no blob reaches `p`;
/// ADR-7970). The fill visibility [`occluder_light`] returns is this times the full-quality
/// terms'.
#[must_use]
pub fn far_band_shade(block: &MeshOccluderBlock, p: Vec3, n: Vec3) -> f32 {
    let mut vis = far_blobs(block, p, n).1;
    let listed = block.cell_mask(p);
    let n_occ = block.count().min(MAX_MESH_OCCLUDERS);
    for (index, (occ, bound)) in block.items.iter().zip(&block.bounds).take(n_occ).enumerate() {
        let fade = block.near_fade_of(index);
        if fade <= 0.0
            || listed & (1 << index) == 0
            || p.distance_squared(Vec3::from_slice(&bound[..3])) >= bound[3] * bound[3]
        {
            continue;
        }
        vis *= 1.0 - fade * blob_occlusion(p, n, item_blob(occ));
    }
    vis
}

/// Share of the reflected ray `p + t r`'s ground hit that lies in a proxy's sun shadow, soft
/// over the lobe footprint (`rough` = GGX alpha): CPU mirror of
/// `mesh_occluder_reflection_shadow`. The forward pass takes the sun's share of the ground
/// radiance out of the reflection by this much.
#[must_use]
pub fn reflection_shadow(block: &MeshOccluderBlock, p: Vec3, r: Vec3, rough: f32) -> f32 {
    if r.y > -1e-3 {
        return 0.0;
    }
    let mut cover = 0.0f32;
    for (index, occ) in block
        .items
        .iter()
        .take(block.count().min(MAX_MESH_OCCLUDERS))
        .enumerate()
    {
        let verts = (occ.faces[1][3] as usize).min(MAX_SHADOW_VERTS);
        let ground_y = occ.center[3];
        if verts < 3 || p.y <= ground_y + 0.01 {
            continue;
        }
        let t = (ground_y - p.y) / r.y;
        let hit = Vec2::new(p.x + r.x * t, p.z + r.z * t);
        let width = (t * rough * 2.0).max(0.05);
        if hit.distance(Vec2::new(occ.faces[2][3], occ.faces[3][3])) > occ.faces[4][3] + 2.0 * width {
            continue;
        }
        let vert = |j: usize| {
            let row = occ.shadow[j / 2];
            Vec2::new(row[(j % 2) * 2], row[(j % 2) * 2 + 1])
        };
        let mut sd = f32::MAX;
        for j in 0..verts {
            let (a, b) = (vert(j), vert((j + 1) % verts));
            let e = b - a;
            let inward = Vec2::new(-e.y, e.x) / e.length().max(1e-6);
            sd = sd.min((hit - a).dot(inward));
        }
        cover = cover.max(smoothstep(-width, width, sd) * (1.0 - block.near_fade_of(index)));
    }
    cover
}

/// Far-field view factor of a flat patch of area `area` and normal `face_n` whose centre is at
/// `fc` (relative to the receiver) as a disc: `A · cosθ_face · cosθ_receiver / (π d² + A)`
/// (exact on the axis of a disc; Bunnell 2005).
fn disc_view_factor(area: f32, fc: Vec3, face_n: Vec3, n: Vec3) -> f32 {
    let d2 = fc.length_squared().max(1e-4);
    let dir = fc / d2.sqrt();
    area * (-dir.dot(face_n)).max(0.0) * dir.dot(n).max(0.0) / (std::f32::consts::PI * d2 + area)
}

/// Rays (of five over the fill's disc) from `p` that hit the box.
fn fill_hits(p: Vec3, dir: Vec3, spread: f32, c: Vec3, axes: &[Vec3; 3], half: &[f32; 3]) -> f32 {
    let t1 = dir.cross(Vec3::Y);
    let t1 = if t1.length_squared() < 1e-6 {
        Vec3::X
    } else {
        t1.normalize()
    };
    let t2 = dir.cross(t1);
    let offsets = [(0.0, 0.0), (0.7, 0.0), (-0.7, 0.0), (0.0, 0.7), (0.0, -0.7)];
    let rot = Mat3::from_cols(axes[0], axes[1], axes[2]).transpose();
    let o = rot * (p - c);
    offsets
        .iter()
        .filter(|(a, b)| {
            let d = rot * (dir + (t1 * *a + t2 * *b) * spread).normalize();
            slab_hit(o, d, half)
        })
        .count() as f32
}

/// Whether the ray `o + t d` (t ≥ 0, box frame) hits the box of half extents `half`.
fn slab_hit(o: Vec3, d: Vec3, half: &[f32; 3]) -> bool {
    let (mut t0, mut t1) = (0.0f32, f32::MAX);
    for k in 0..3 {
        if d[k].abs() < 1e-6 {
            if o[k].abs() > half[k] {
                return false;
            }
        } else {
            let ta = (-half[k] - o[k]) / d[k];
            let tb = (half[k] - o[k]) / d[k];
            t0 = t0.max(ta.min(tb));
            t1 = t1.min(ta.max(tb));
        }
    }
    t0 <= t1
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Contract rows of the occluder block (`shader_contract`, ADR-3002). The block is read by the forward
/// pass and the terrain's shared pass alike, so the row applies to every module that declares it.
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "",
            "MeshOccluders",
            MeshOccluderBlock {
                header,
                sky,
                ground,
                ground_sun,
                fill_dir,
                grid,
                cells,
                bounds,
                shadow_bounds,
                items,
                near_fade,
                far,
                far_top
            }
        ),
        crate::contract_row!(
            "",
            "MeshOccluder",
            GpuMeshOccluder {
                center,
                axes,
                faces,
                shadow
            }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box mesh from `(-half, 0, -half)` to `(half, 2 half, half)`.
    fn box_mesh(half: f32) -> std::sync::Arc<crate::runtime::CpuMesh> {
        let corners = [[-half, 0.0, -half], [half, 2.0 * half, half], [0.0, half, 0.0]];
        std::sync::Arc::new(crate::runtime::CpuMesh {
            vertices: corners
                .iter()
                .map(|p| crate::LabVertex::from_pos_normal_uv(*p, [0.0, 1.0, 0.0], [0.0, 0.0]))
                .collect(),
            indices: vec![0, 1, 2],
        })
    }

    fn expected(mesh: &crate::runtime::CpuMesh) -> Option<MeshOccluder> {
        let albedo = crate::CpuMaterial::default().base_color;
        MeshOccluder::from_positions(
            mesh.vertices.iter().map(|v| v.position),
            Mat4::IDENTITY.to_cols_array_2d(),
            [albedo[0], albedo[1], albedo[2]],
        )
    }

    #[test]
    fn cached_proxy_bounds_follow_the_mesh_the_entity_draws() {
        use regia_ecs::ecs::system::System as _;
        let mut world = World::new();
        world.init_resource::<ExtractedFrame>();
        let material = std::sync::Arc::new(crate::CpuMaterial::default());
        let small = box_mesh(1.0);
        let entity = world
            .spawn((
                MeshRenderer::new(small.clone(), material.clone()),
                OcclusionProxy::default(),
            ))
            .id();
        let mut system = IntoSystem::into_system(extract_mesh_occluders);
        system.initialize(&mut world);
        // The same occluder every frame, from the bounds scanned on the first.
        for _ in 0..3 {
            let _ = system.run((), &mut world);
            let occluders = world.resource::<ExtractedFrame>().mesh_occluders.clone();
            assert_eq!(occluders, expected(&small).into_iter().collect::<Vec<_>>());
        }
        // A new mesh (a new allocation) is scanned again.
        let large = box_mesh(3.0);
        world
            .entity_mut(entity)
            .insert(MeshRenderer::new(large.clone(), material.clone()));
        drop(small);
        let _ = system.run((), &mut world);
        let occluders = world.resource::<ExtractedFrame>().mesh_occluders.clone();
        assert_eq!(occluders, expected(&large).into_iter().collect::<Vec<_>>());
        assert_ne!(expected(&large), expected(&box_mesh(1.0)), "the two boxes differ");
        // An empty mesh gives no occluder.
        let empty = std::sync::Arc::new(crate::runtime::CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        world
            .entity_mut(entity)
            .insert(MeshRenderer::new(empty, material));
        let _ = system.run((), &mut world);
        assert!(world.resource::<ExtractedFrame>().mesh_occluders.is_empty());
    }

    fn lcg(state: &mut u64) -> f32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((*state >> 40) as f32) / (1u64 << 24) as f32
    }

    /// Monte Carlo cosine-weighted view factor of a convex polygon (plane through `verts`).
    fn mc_view_factor(n: Vec3, verts: &[Vec3], samples: u32) -> f32 {
        let (t1, t2) = n.any_orthonormal_pair();
        let normal = (verts[1] - verts[0]).cross(verts[2] - verts[0]).normalize();
        let mut hits = 0u32;
        let mut s = 7u64;
        for _ in 0..samples {
            let (u1, u2) = (lcg(&mut s), lcg(&mut s));
            let r = u1.sqrt();
            let phi = std::f32::consts::TAU * u2;
            let d = t1 * (r * phi.cos()) + t2 * (r * phi.sin()) + n * (1.0 - u1).max(0.0).sqrt();
            let denom = d.dot(normal);
            if denom.abs() < 1e-8 {
                continue;
            }
            let t = verts[0].dot(normal) / denom;
            if t <= 0.0 {
                continue;
            }
            let x = d * t;
            let inside = (0..verts.len()).all(|i| {
                let a = verts[i];
                let b = verts[(i + 1) % verts.len()];
                (b - a).cross(x - a).dot(normal) >= 0.0
            }) || (0..verts.len()).all(|i| {
                let a = verts[i];
                let b = verts[(i + 1) % verts.len()];
                (b - a).cross(x - a).dot(normal) <= 0.0
            });
            hits += u32::from(inside);
        }
        hits as f32 / samples as f32
    }

    #[test]
    fn a_disc_like_square_overhead_matches_the_analytic_factor() {
        // Unit square 1 m above, centred: F = analytic rectangle factor (Howell's catalogue C-11)
        // via 4 quadrants of 0.5 × 0.5 at height 1.
        let verts = [
            Vec3::new(0.5, 1.0, 0.5),
            Vec3::new(0.5, 1.0, -0.5),
            Vec3::new(-0.5, 1.0, -0.5),
            Vec3::new(-0.5, 1.0, 0.5),
        ];
        let f = polygon_view_factor(Vec3::Y, &verts);
        let quadrant = |a: f32, b: f32, c: f32| {
            let (x, y) = (a / c, b / c);
            let (sx, sy) = ((1.0 + x * x).sqrt(), (1.0 + y * y).sqrt());
            (x / sx * (y / sx).atan() + y / sy * (x / sy).atan()) / (2.0 * std::f32::consts::PI)
        };
        let want = 4.0 * quadrant(0.5, 0.5, 1.0);
        assert!((f - want).abs() < 1e-4, "{f} vs {want}");
    }

    #[test]
    fn clipped_and_tilted_polygons_match_monte_carlo() {
        // A planar pentagon partly behind some of the receivers.
        let (o, eu, ev) = (
            Vec3::new(0.2, 0.4, 1.0),
            Vec3::new(1.0, 0.3, 0.2),
            Vec3::new(-0.1, 0.8, -0.4),
        );
        let poly: Vec<Vec3> = [(-1.2, -1.0), (1.5, -0.8), (1.8, 0.9), (0.0, 1.6), (-1.4, 0.7)]
            .iter()
            .map(|&(a, b)| o + eu * a + ev * b)
            .collect();
        for n in [
            Vec3::Y,
            Vec3::Z,
            Vec3::new(0.3, 0.2, 0.9).normalize(),
            Vec3::new(1.0, -1.0, 0.4).normalize(),
        ] {
            let f = polygon_view_factor(n, &poly);
            let mc = mc_view_factor(n, &poly, 200_000);
            assert!((f - mc).abs() < 0.006, "n {n}: {f} vs {mc}");
        }
        // Entirely behind the receiver.
        assert_eq!(polygon_view_factor(-Vec3::Z, &poly), 0.0);
    }

    fn hull() -> MeshOccluder {
        MeshOccluder::from_local_bounds(
            Mat4::from_rotation_y(0.6).to_cols_array_2d(),
            [-2.25, 0.35, -1.2],
            [2.25, 1.8, 1.2],
            [0.18; 3],
        )
        .with_ground(0.0)
    }

    fn europa() -> OccluderLighting {
        OccluderLighting {
            toward_sun: Vec3::new(0.7, 0.57, 0.3).normalize().to_array(),
            sun: [50_000.0; 3],
            sky_ambient: [[0.0; 3], [5000.0; 3]],
            fill: Some([[-0.3, 0.3, 0.9], [1600.0, 1400.0, 1100.0]]),
            eye: [0.0, 1.6, 8.0],
        }
    }

    /// Twenty hulls on a golden-angle ring 6–40 m out (the `dyn_light --bench` layout).
    fn ring(offset: Vec3) -> Vec<MeshOccluder> {
        (0..20)
            .map(|k| {
                let a = k as f32 * 2.399_963;
                let r = if k == 0 {
                    0.0
                } else {
                    6.0 + 34.0 * (k as f32 / 20.0).sqrt()
                };
                let at = offset + Vec3::new(a.cos() * r, 0.0, a.sin() * r);
                MeshOccluder::from_local_bounds(
                    (Mat4::from_translation(at) * Mat4::from_rotation_y(a * 1.7)).to_cols_array_2d(),
                    [-2.25, 0.35, -1.2],
                    [2.25, 1.8, 1.2],
                    [0.18; 3],
                )
                .with_ground(at.y)
            })
            .collect()
    }

    #[test]
    fn every_reach_sphere_that_holds_a_point_is_listed_in_its_cell() {
        let mut light = europa();
        light.toward_sun = Vec3::new(0.9, 0.2, 0.3).normalize().to_array();
        let block = MeshOccluderBlock::build(&ring(Vec3::new(3.0, 1.0, -2.0)), &light);
        assert_eq!(block.count(), 20);
        let mut state = 7u64;
        let (mut reached, mut listed_extra) = (0usize, 0usize);
        for _ in 0..200_000 {
            let p = Vec3::new(
                (lcg(&mut state) - 0.5) * 160.0,
                (lcg(&mut state) - 0.5) * 30.0,
                (lcg(&mut state) - 0.5) * 160.0,
            );
            let listed = block.cell_mask(p);
            for (i, b) in block.bounds.iter().take(block.count()).enumerate() {
                let holds = p.distance_squared(Vec3::from_slice(&b[..3])) < b[3] * b[3];
                let bit = listed & (1 << i) != 0;
                assert!(
                    !holds || bit,
                    "point {p} in sphere {i} but not listed ({listed:#x})"
                );
                reached += usize::from(holds);
                listed_extra += usize::from(bit && !holds);
            }
            assert_eq!(listed >> block.count(), 0, "bits past the count");
        }
        assert!(reached > 10_000, "the sample reaches the spheres ({reached})");
        for (occ, sb) in block.items.iter().zip(&block.shadow_bounds).take(block.count()) {
            let radius = if occ.faces[1][3] >= 3.0 {
                occ.faces[4][3]
            } else {
                -1.0
            };
            assert_eq!(*sb, [occ.faces[2][3], occ.faces[3][3], radius, occ.center[3]]);
        }
        eprintln!("reached {reached}, listed but outside the sphere {listed_extra}");
    }

    #[test]
    fn points_beyond_every_reach_sphere_list_nothing() {
        let block = MeshOccluderBlock::build(&ring(Vec3::new(5000.0, 0.0, 0.0)), &europa());
        for p in [
            Vec3::ZERO,
            Vec3::new(0.0, 0.0, 300.0),
            Vec3::new(4000.0, 2.0, 0.0),
            Vec3::NAN,
        ] {
            assert_eq!(block.cell_mask(p), 0, "{p}");
        }
        assert_eq!(MeshOccluderBlock::default().cell_mask(Vec3::ZERO), 0);
        assert_ne!(block.cell_mask(Vec3::new(5000.0, 0.5, 0.0)), 0);
    }

    #[test]
    fn the_shadow_polygon_contains_the_footprint_and_points_away_from_the_sun() {
        let occ = hull();
        let poly = occ.sun_shadow_polygon(europa().toward_sun);
        assert!((4..=MAX_SHADOW_VERTS).contains(&poly.len()), "{poly:?}");
        let centre = poly.iter().fold(Vec2::ZERO, |s, p| s + Vec2::from_array(*p)) / poly.len() as f32;
        let sun = Vec3::from_array(europa().toward_sun);
        // Away from the sun in XZ.
        assert!(centre.dot(Vec2::new(sun.x, sun.z)) < 0.0);
        // The box centre's foot (0, 0) lies inside (counter-clockwise hull).
        let inside = (0..poly.len()).all(|i| {
            let a = Vec2::from_array(poly[i]);
            let b = Vec2::from_array(poly[(i + 1) % poly.len()]);
            (b - a).perp_dot(-a) >= 0.0
        });
        assert!(inside, "{poly:?}");
    }

    #[test]
    fn the_shadow_polygon_clips_edges_at_the_ground_plane() {
        // A pitched box cut by the plane (a slope, or bounds reaching through
        // the contact height): the hull must use the edges' plane crossings,
        // not the below-plane corners' XZ. Axes 30° about Z, half extents
        // (2, 1, 1), centred at the origin, ground at y = 0, a low sun.
        let occ = MeshOccluder::from_local_bounds(
            Mat4::from_rotation_z(30.0_f32.to_radians()).to_cols_array_2d(),
            [-2.0, -1.0, -1.0],
            [2.0, 1.0, 1.0],
            [0.18; 3],
        )
        .with_ground(0.0);
        let sun = Vec3::new(0.95, 0.15, 0.0).normalize().to_array();
        let poly = occ.sun_shadow_polygon(sun);
        assert!(poly.len() >= 3, "{poly:?}");
        // The edge from about (-1.23, -1.87) to (2.23, 0.13) crosses y = 0 at
        // x = 2; the receiver beside the crossing must fall inside the hull.
        let inside = |x: f32, z: f32| {
            (0..poly.len()).all(|i| {
                let a = Vec2::from_array(poly[i]);
                let b = Vec2::from_array(poly[(i + 1) % poly.len()]);
                (b - a).perp_dot(Vec2::new(x, z) - a) >= 0.0
            })
        };
        assert!(
            inside(1.9, 0.0),
            "the hull reaches the plane crossing at x = 2: {poly:?}"
        );
        // The below-plane corner projected flat would cap +X near 1.38.
        let max_x = poly.iter().map(|p| p[0]).fold(f32::NEG_INFINITY, f32::max);
        assert!(max_x > 1.5, "{poly:?}");
    }

    #[test]
    fn an_empty_block_changes_nothing() {
        let block = MeshOccluderBlock::build(&[], &europa());
        assert_eq!(block.count(), 0);
        assert_eq!(
            bytemuck::bytes_of(&block),
            bytemuck::bytes_of(&MeshOccluderBlock::default())
        );
        let (d, v) = occluder_light(&block, Vec3::new(0.0, 0.5, 0.0), -Vec3::Y, false);
        assert_eq!((d, v), ([0.0; 3], 1.0));
    }

    /// A shifted block lights a shifted point exactly as the block lit the point (spec 78 § 12.2).
    #[test]
    fn a_shifted_block_lights_a_shifted_point_the_same() {
        let block = MeshOccluderBlock::build(&ring(Vec3::new(3.0, 1.0, -2.0)), &europa());
        let by = [1024.0, 0.0, -2048.0];
        let mut moved = block;
        moved.shift(by);
        let by = Vec3::from_array(by);
        for p in [
            Vec3::new(3.0, 0.3, -2.0),
            Vec3::new(5.0, 0.01, -1.0),
            Vec3::new(0.0, 2.0, 0.0),
            Vec3::new(40.0, 0.5, 40.0),
        ] {
            for n in [Vec3::Y, Vec3::X, -Vec3::Y] {
                let (d0, v0) = occluder_light(&block, p, n, false);
                let (d1, v1) = occluder_light(&moved, p + by, n, false);
                for c in 0..3 {
                    assert!(
                        (d0[c] - d1[c]).abs() < 1e-2 * (1.0 + d0[c].abs()),
                        "{p} {n}: {d0:?} {d1:?}"
                    );
                }
                assert!((v0 - v1).abs() < 1e-2, "{p} {n}: {v0} {v1}");
            }
            assert_eq!(block.cell_mask(p), moved.cell_mask(p + by), "{p}");
        }
    }

    /// The default hull: no `with_ground`, so the ground is the bounds' lowest
    /// corner and the bottom face lies in the ground plane.
    fn grounded_hull() -> MeshOccluder {
        MeshOccluder::from_local_bounds(
            Mat4::from_rotation_y(0.6).to_cols_array_2d(),
            [-2.25, 0.35, -1.2],
            [2.25, 1.8, 1.2],
            [0.18; 3],
        )
    }

    #[test]
    fn the_default_lowest_corner_ground_shadows_the_belly() {
        // The bottom face is in the ground plane; its 2 cm outward sample must
        // still see the box's own shadow. Otherwise the belly is shaded with
        // the full sunlit ground (0.18 × 5000 ≈ 900 cd/m²) and bounces it onto
        // the terrain under the hull (the shadowed figure is ~45 cd/m²).
        let hull = grounded_hull();
        assert!((hull.ground_y - 0.35).abs() < 1.0e-5, "{hull:?}");
        let block = MeshOccluderBlock::build(&[hull], &europa());
        let occ = &block.items[0];
        let c = Vec3::from_slice(&occ.center[..3]);
        let beside = c + Vec3::from_slice(&occ.axes[0][..3]) * 3.0 - Vec3::Y * c.y;
        let (d, _) = occluder_light(&block, beside, Vec3::Y, true);
        assert!(d[0] > 0.0, "{d:?}");
        let (under, _) = occluder_light(&block, Vec3::new(c.x, hull.ground_y, c.z), Vec3::Y, true);
        assert!(
            under[0] > 0.0 && under[0] < 0.2 * d[0],
            "the belly in its own shadow bounces little ({under:?}) beside {d:?}"
        );
    }

    #[test]
    fn a_down_face_under_the_hull_loses_the_suns_share_of_the_ground() {
        let block = MeshOccluderBlock::build(&[hull()], &europa());
        // A point on the hull's bottom (inside its box): the box term skips it, the shadow
        // polygon below removes most of the ground's sun share.
        let (d, _) = occluder_light(&block, Vec3::new(0.0, 0.36, 0.0), -Vec3::Y, false);
        let share = -d[0] / block.ground_sun[0];
        assert!((0.5..=1.0).contains(&share), "{share}");
        // An up face sees none of it.
        let (d, _) = occluder_light(&block, Vec3::new(0.0, 1.81, 0.0), Vec3::Y, false);
        assert!(d[0].abs() < 1e-6);
    }

    #[test]
    fn ground_beside_the_hull_takes_its_bounce_and_under_it_loses_the_fill() {
        let block = MeshOccluderBlock::build(&[hull()], &europa());
        // Black sky (airless): the hull only adds light.
        let occ = &block.items[0];
        let c = Vec3::from_slice(&occ.center[..3]);
        let a0 = Vec3::from_slice(&occ.axes[0][..3]);
        let beside = c + a0 * 3.0 - Vec3::Y * c.y;
        let (d, _) = occluder_light(&block, beside, Vec3::Y, true);
        assert!(d[0] > 0.0, "{d:?}");
        let under = Vec3::new(c.x, 0.0, c.z);
        // Under it: the belly, lit by the hull's own shadow on the ground, sends little back.
        let (d_under, fill) = occluder_light(&block, under, Vec3::Y, true);
        assert!(d_under[0] > 0.0 && d_under[0] < d[0], "{d_under:?} vs {d:?}");
        // Under the box the fill is blocked; far away nothing changes.
        assert_eq!(fill, 0.0);
        let (far, far_fill) = occluder_light(&block, c + Vec3::new(40.0, -c.y, 0.0), Vec3::Y, true);
        assert_eq!((far, far_fill), ([0.0; 3], 1.0));
    }

    #[test]
    fn a_sky_dome_is_hidden_by_the_box() {
        let mut light = europa();
        light.sky_ambient = [[2000.0; 3], [5000.0; 3]];
        light.fill = None;
        let block = MeshOccluderBlock::build(&[hull()], &light);
        let c = Vec3::from_slice(&block.items[0].center[..3]);
        let (d, fill) = occluder_light(&block, Vec3::new(c.x, 0.0, c.z) + Vec3::X * 2.6, Vec3::Y, true);
        // The shaded side of a 0.18 box is far darker than a 2000 cd/m² sky.
        assert!(d[0] < 0.0, "{d:?}");
        assert_eq!(fill, 1.0);
    }

    #[test]
    fn the_far_field_form_agrees_with_the_exact_factor_where_it_takes_over() {
        // A 4.5 × 1.45 m face, seen from 1.5 × 2.25 m off its surface and beyond.
        let (u, v) = (Vec3::new(2.25, 0.0, 0.0), Vec3::new(0.0, 0.725, 0.0));
        for (d, off) in [(3.4, 0.0), (5.0, 1.5), (8.0, -2.0)] {
            let fc = Vec3::new(off, 0.3, d);
            let n = Vec3::new(0.0, 0.4, -1.0).normalize();
            let exact = polygon_view_factor(n, &[fc + u + v, fc + u - v, fc - u - v, fc - u + v]);
            let disc = disc_view_factor(4.0 * u.length() * v.length(), fc, -Vec3::Z, n);
            assert!(
                (disc - exact).abs() < 0.2 * exact + 0.002,
                "d {d}: disc {disc} vs exact {exact}"
            );
        }
    }

    #[test]
    fn the_reach_sphere_holds_every_point_the_terms_reach() {
        let mut low_sun = europa();
        low_sun.toward_sun = Vec3::new(0.9, 0.12, 0.2).normalize().to_array();
        for light in [europa(), low_sun] {
            let block = MeshOccluderBlock::build(&[hull()], &light);
            let occ = &block.items[0];
            let b = block.bounds[0];
            let c = Vec3::from_slice(&occ.center[..3]);
            let pc = Vec3::new(occ.faces[2][3], occ.center[3], occ.faces[3][3]);
            let (r, rp) = (occ.faces[0][3], occ.faces[4][3].max(0.1));
            let mut reached = 0;
            for i in 0..60 {
                for j in 0..60 {
                    for y in [0.05, 1.0, 4.0] {
                        let p = Vec3::new(i as f32 - 30.0, y, j as f32 - 30.0) * 1.3;
                        let box_term = p.distance(c) < BOX_FADE[1] * r;
                        let shadow_term = p.distance(pc) < SHADOW_FADE[1] * rp;
                        if box_term || shadow_term {
                            reached += 1;
                            assert!(
                                p.distance(Vec3::from_slice(&b[..3])) < b[3],
                                "{p} reached but outside {b:?}"
                            );
                        }
                    }
                }
            }
            assert!(reached > 100);
        }
    }

    #[test]
    fn the_fill_cone_cull_never_skips_a_box_the_rays_hit() {
        let block = MeshOccluderBlock::build(&[hull()], &europa());
        let occ = &block.items[0];
        let c = Vec3::from_slice(&occ.center[..3]);
        let r = occ.faces[0][3];
        let axes: [Vec3; 3] = std::array::from_fn(|k| Vec3::from_slice(&occ.axes[k][..3]));
        let half: [f32; 3] = std::array::from_fn(|k| occ.axes[k][3]);
        let fill_dir = Vec3::from_slice(&block.fill_dir[..3]);
        let mut hit_points = 0;
        for i in 0..40 {
            for j in 0..40 {
                let q = c + Vec3::new(i as f32 * 0.9 - 18.0, 0.05, j as f32 * 0.9 - 18.0) - Vec3::Y * c.y;
                let hits = fill_hits(q, fill_dir, block.header[1], c, &axes, &half);
                let cq = c - q;
                let along = cq.dot(fill_dir);
                let reach = r + along.max(0.0) * block.header[1] * 0.7 * 1.05 + 0.01;
                let cone = along > -r && cq.length_squared() - along * along < reach * reach;
                assert!(cone || hits == 0.0, "culled with {hits} hits at {q}");
                hit_points += usize::from(hits > 0.0);
            }
        }
        assert!(hit_points > 10, "only {hit_points} points in the fill shadow");
    }

    #[test]
    fn the_shadow_disc_agrees_with_the_exact_polygon_where_it_takes_over() {
        let block = MeshOccluderBlock::build(&[hull()], &europa());
        let occ = &block.items[0];
        let verts = occ.faces[1][3] as usize;
        let pc = Vec3::new(occ.faces[2][3], 0.0, occ.faces[3][3]);
        let rp = occ.faces[4][3];
        for (dir, n) in [
            (Vec3::new(0.0, 1.0, 1.0), Vec3::new(0.0, 0.0, -1.0)),
            (Vec3::new(1.0, 0.6, 0.0), Vec3::new(-1.0, -0.3, 0.0)),
            (Vec3::new(-0.5, 0.3, -1.0), Vec3::new(0.0, -1.0, 0.0)),
        ] {
            let p = pc + dir.normalize() * (SHADOW_NEAR * rp);
            let n = n.normalize();
            let poly: Vec<Vec3> = (0..verts)
                .map(|i| {
                    Vec3::new(
                        occ.shadow[i / 2][(i % 2) * 2],
                        0.0,
                        occ.shadow[i / 2][(i % 2) * 2 + 1],
                    ) - p
                })
                .collect();
            let exact = polygon_view_factor(n, &poly);
            let disc = disc_view_factor(occ.faces[5][3], pc - p, Vec3::Y, n);
            assert!(
                (disc - exact).abs() < 0.25 * exact + 0.003,
                "dir {dir}: disc {disc} vs exact {exact}"
            );
        }
    }

    #[test]
    fn a_reflection_into_the_shadow_sees_shadowed_ground() {
        let block = MeshOccluderBlock::build(&[hull()], &europa());
        let poly = hull().sun_shadow_polygon(europa().toward_sun);
        let centre = poly.iter().fold(Vec2::ZERO, |s, p| s + Vec2::from_array(*p)) / poly.len() as f32;
        let p = Vec3::new(centre.x, 1.0, centre.y) + Vec3::new(0.3, 0.0, 0.0);
        let into = (Vec3::new(centre.x, 0.0, centre.y) - p).normalize();
        assert!(reflection_shadow(&block, p, into, 0.1) > 0.99);
        // Upward or far away: none.
        assert_eq!(reflection_shadow(&block, p, Vec3::Y, 0.1), 0.0);
        let away = (Vec3::new(centre.x + 60.0, 0.0, centre.y) - p).normalize();
        assert!(reflection_shadow(&block, p, away, 0.1) < 1e-3);
    }

    #[test]
    fn the_nearest_occluders_are_kept() {
        let far = MeshOccluder {
            center: [500.0, 1.0, 0.0],
            ..hull()
        };
        let mut list = vec![far; MAX_MESH_OCCLUDERS + 4];
        list.push(hull());
        let block = MeshOccluderBlock::build(&list, &europa());
        assert_eq!(block.count(), MAX_MESH_OCCLUDERS);
        assert_eq!(block.items[0].center[0], hull().center[0]);
    }

    /// The `hull()` box at `at` (its ground under it), opted into the far band.
    fn hull_at(at: Vec3) -> MeshOccluder {
        MeshOccluder::from_local_bounds(
            (Mat4::from_translation(at) * Mat4::from_rotation_y(0.6)).to_cols_array_2d(),
            [-2.25, 0.35, -1.2],
            [2.25, 1.8, 1.2],
            [0.18; 3],
        )
        .with_ground(at.y)
        .with_far_band(true)
    }

    /// Europa light seen from the origin.
    fn europa_at_origin() -> OccluderLighting {
        OccluderLighting {
            eye: [0.0, 1.6, 0.0],
            ..europa()
        }
    }

    /// The distance key the pack sorts by.
    fn key(o: &MeshOccluder, eye: [f32; 3]) -> f32 {
        (Vec3::from_array(o.center) - Vec3::from_array(eye)).length() - o.radius()
    }

    /// ADR-7970: 100 opted-in proxies on a 10 × 10 grid 12 m apart: the nearest 32 at full
    /// quality, the other 68 as contact blobs that shade the ground under each (sky and fill
    /// both), and nothing between the hulls.
    #[test]
    fn the_far_band_shades_every_proxy_past_the_cap() {
        let light = europa_at_origin();
        let proxies: Vec<MeshOccluder> = (0..100)
            .map(|k| {
                hull_at(Vec3::new(
                    6.0 + 12.0 * (k % 10) as f32,
                    0.0,
                    6.0 + 12.0 * (k / 10) as f32,
                ))
            })
            .collect();
        let block = MeshOccluderBlock::build(&proxies, &light);
        assert_eq!(
            block.bands(),
            OccluderBands {
                near: 32,
                far: 68,
                dropped: 0
            }
        );
        let mut ranked: Vec<&MeshOccluder> = proxies.iter().collect();
        ranked.sort_by(|a, b| key(a, light.eye).total_cmp(&key(b, light.eye)));
        for (rank, o) in ranked.iter().enumerate().skip(MAX_MESH_OCCLUDERS) {
            let under = Vec3::new(o.center[0], o.ground_y, o.center[2]);
            let shade = far_band_shade(&block, under, Vec3::Y);
            assert!(shade < 0.5, "rank {rank} at {under}: shade {shade}");
            // The fill (Jupiter on Europa) under the far hull is shadowed too.
            let (_, fill) = occluder_light(&block, under, Vec3::Y, true);
            assert!(fill < 0.5, "rank {rank}: fill {fill}");
        }
        // Between the hulls (6 m from every centre, past every blob's reach): unchanged.
        for i in 0..10 {
            for j in 0..10 {
                let between = Vec3::new(12.0 * i as f32, 0.0, 12.0 * j as f32);
                assert_eq!(far_band_shade(&block, between, Vec3::Y), 1.0, "{between}");
            }
        }
        // With the band off the same 68 are dropped and their ground is unshaded.
        let off: Vec<MeshOccluder> = proxies.iter().map(|o| o.with_far_band(false)).collect();
        let plain = MeshOccluderBlock::build(&off, &light);
        assert_eq!(
            plain.bands(),
            OccluderBands {
                near: 32,
                far: 0,
                dropped: 68
            }
        );
        for o in ranked.iter().skip(MAX_MESH_OCCLUDERS) {
            let under = Vec3::new(o.center[0], o.ground_y, o.center[2]);
            assert_eq!(far_band_shade(&plain, under, Vec3::Y), 1.0, "{under}");
            let (_, with_band) = occluder_light(&block, under, Vec3::Y, true);
            let (_, without) = occluder_light(&plain, under, Vec3::Y, true);
            assert!(with_band < without, "{under}: {with_band} vs {without}");
        }
    }

    /// More opted-in proxies than both bands hold: the farthest are dropped and counted.
    #[test]
    fn proxies_past_both_bands_are_counted_as_dropped() {
        let proxies: Vec<MeshOccluder> = (0..200)
            .map(|k| hull_at(Vec3::new(8.0 * (k % 20) as f32, 0.0, 8.0 * (k / 20) as f32)))
            .collect();
        let block = MeshOccluderBlock::build(&proxies, &europa_at_origin());
        assert_eq!(
            block.bands(),
            OccluderBands {
                near: MAX_MESH_OCCLUDERS,
                far: MAX_FAR_MESH_OCCLUDERS,
                dropped: 200 - MAX_MESH_OCCLUDERS - MAX_FAR_MESH_OCCLUDERS
            }
        );
        // Sorted by x for the shaders' binary search.
        let far = &block.far[..block.far_count()];
        assert!(far.windows(2).all(|w| w[0][0] <= w[1][0]));
    }

    /// ADR-7970: a hull driving away across the near/far boundary over 16 fixed steps. The
    /// ground under it moves with it, so only the band changes: its fill visibility and its
    /// added light change monotonically and no step carries more than half the change.
    #[test]
    fn a_hull_crossing_the_band_boundary_does_not_pop() {
        let light = europa_at_origin();
        // 31 hulls close to the eye on the −x side (keys under 15 m), one at key ≈ 27 m on −z
        // (the 32nd until the mover passes it), and four far out on +z.
        let mut fixed: Vec<MeshOccluder> = (0..31)
            .map(|k| {
                let a = std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * (k as f32 + 0.5) / 31.0;
                let r = 8.0 + 6.0 * (k % 3) as f32;
                hull_at(Vec3::new(a.cos() * r, 0.0, a.sin() * r))
            })
            .collect();
        fixed.push(hull_at(Vec3::new(0.0, 0.0, -30.0)));
        fixed.extend((0..4).map(|k| hull_at(Vec3::new(10.0 * k as f32, 0.0, 120.0))));
        let mut samples = Vec::new();
        for step in 0..16 {
            let x = 20.0 + 20.0 * step as f32 / 15.0;
            let mover = hull_at(Vec3::new(x, 0.0, 0.0));
            let mut proxies = fixed.clone();
            proxies.push(mover);
            let block = MeshOccluderBlock::build(&proxies, &light);
            assert_eq!(block.count(), MAX_MESH_OCCLUDERS);
            assert_eq!(block.far_count(), proxies.len() - MAX_MESH_OCCLUDERS);
            let under = Vec3::new(mover.center[0], 0.0, mover.center[2]);
            let (delta, fill) = occluder_light(&block, under, Vec3::Y, true);
            samples.push((delta[0], fill));
        }
        let first = samples[0];
        let last = samples[15];
        assert!(
            first.1 < 0.01,
            "under the full-quality hull the fill is blocked: {first:?}"
        );
        assert!(last.1 > 0.05, "under its blob some fill gets through: {last:?}");
        for (name, pick) in [("delta", 0usize), ("fill", 1)] {
            let value = |s: (f32, f32)| if pick == 0 { s.0 } else { s.1 };
            let total = value(last) - value(first);
            assert!(
                total.abs() > 1e-4,
                "{name} changes across the boundary: {samples:?}"
            );
            for pair in samples.windows(2) {
                let step = value(pair[1]) - value(pair[0]);
                assert!(
                    step * total.signum() >= -1e-6,
                    "{name} is not monotonic: {samples:?}"
                );
                assert!(
                    step.abs() <= 0.5 * total.abs(),
                    "{name} pops by {step} of {total}: {samples:?}"
                );
            }
        }
    }

    /// With 32 proxies or fewer there is no far band: opting in changes no byte of the block
    /// (the shaders then run exactly the base terms).
    #[test]
    fn at_most_32_proxies_pack_the_same_block_with_or_without_the_far_band() {
        let light = europa_at_origin();
        for n in [1, 20, MAX_MESH_OCCLUDERS] {
            let on: Vec<MeshOccluder> = (0..n)
                .map(|k| hull_at(Vec3::new(9.0 * (k % 8) as f32, 0.0, 9.0 * (k / 8) as f32)))
                .collect();
            let off: Vec<MeshOccluder> = on.iter().map(|o| o.with_far_band(false)).collect();
            let a = MeshOccluderBlock::build(&on, &light);
            let b = MeshOccluderBlock::build(&off, &light);
            assert_eq!(bytemuck::bytes_of(&a), bytemuck::bytes_of(&b), "{n} proxies");
            assert_eq!(
                a.bands(),
                OccluderBands {
                    near: n,
                    far: 0,
                    dropped: 0
                }
            );
            assert_eq!((a.header[2], a.header[3], a.grid[3]), (0.0, 0.0, 0.0));
            assert!(a.near_fade.iter().flatten().all(|v| *v == 0.0));
            assert!(
                a.far
                    .iter()
                    .flatten()
                    .chain(a.far_top.iter().flatten())
                    .all(|v| *v == 0.0)
            );
        }
    }

    /// The far band moves with a floating-origin shift like the rest of the block.
    #[test]
    fn a_shifted_far_band_shades_a_shifted_point_the_same() {
        let proxies: Vec<MeshOccluder> = (0..60)
            .map(|k| hull_at(Vec3::new(7.0 * (k % 10) as f32, 0.5, 7.0 * (k / 10) as f32)))
            .collect();
        let block = MeshOccluderBlock::build(&proxies, &europa_at_origin());
        assert!(block.far_count() > 0);
        let by = Vec3::new(1024.0, 3.0, -2048.0);
        let mut moved = block;
        moved.shift(by.to_array());
        for o in &proxies {
            let p = Vec3::new(o.center[0] + 0.7, 0.5, o.center[2] - 0.4);
            let (a, b) = (
                far_band_shade(&block, p, Vec3::Y),
                far_band_shade(&moved, p + by, Vec3::Y),
            );
            assert!((a - b).abs() < 1e-3, "{p}: {a} vs {b}");
        }
    }

    /// The near item's blob is the proxy's own (`occ_item_blob` mirrors `contact_blob`).
    #[test]
    fn a_packed_item_keeps_its_proxys_contact_blob() {
        let o = hull_at(Vec3::new(3.0, 0.2, -1.0));
        let block = MeshOccluderBlock::build(&[o], &europa_at_origin());
        let (a, b) = (o.contact_blob(), item_blob(&block.items[0]));
        for k in 0..5 {
            assert!((a[k] - b[k]).abs() < 1e-5, "{a:?} vs {b:?}");
        }
        // A 4.5 × 2.4 m footprint on edge: the half diagonal; the top 1.8 m above the ground.
        assert!((a[2] - (2.25f32 * 2.25 + 1.2 * 1.2).sqrt()).abs() < 1e-4, "{a:?}");
        assert!((a[4] - 1.8).abs() < 1e-4, "{a:?}");
    }

    /// The skewed chain finding-5 case: a parent's non-uniform scale times a
    /// child's rotation shears the columns (extract_mesh_occluders passes the
    /// full GlobalTransform, parent × child).
    fn skewed_model() -> Mat4 {
        Mat4::from_scale(Vec3::new(4.0, 1.0, 1.0)) * Mat4::from_rotation_y(std::f32::consts::FRAC_PI_4)
    }

    #[test]
    fn a_sheared_transform_still_bounds_its_box() {
        let occ = MeshOccluder::from_local_bounds(
            skewed_model().to_cols_array_2d(),
            [-0.5; 3],
            [0.5; 3],
            [0.18; 3],
        );
        // The axes are orthonormal by construction now.
        for k in 0..3 {
            let a = occ.axis(k);
            assert!((a.length() - 1.0).abs() < 1.0e-5, "axis {k} is unit");
            for j in (k + 1)..3 {
                assert!(
                    a.dot(occ.axis(j)).abs() < 1.0e-5,
                    "axes {k} and {j} are orthogonal"
                );
            }
        }
        // Every corner of the original parallelepiped lies inside the refit box
        // (a hair inside: the hull test's epsilon is 2 cm).
        let c = Vec3::from_array(occ.center);
        for i in 0..8 {
            let corner = c + skewed_model().transform_vector3(Vec3::new(
                if i & 1 == 0 { -0.49 } else { 0.49 },
                if i & 2 == 0 { -0.49 } else { 0.49 },
                if i & 4 == 0 { -0.49 } else { 0.49 },
            ));
            let local: Vec<f32> = (0..3).map(|k| (corner - c).dot(occ.axis(k))).collect();
            for k in 0..3 {
                assert!(
                    local[k].abs() < occ.half_extents[k] - 0.005,
                    "corner {i} axis {k}: {local:?} vs half {:?}",
                    occ.half_extents
                );
            }
        }
        // A point just outside one face is outside.
        let out = c + occ.axis(0) * (occ.half_extents[0] + 0.1);
        assert!(
            (out - c).dot(occ.axis(0)).abs() > occ.half_extents[0],
            "outside the +A0 face"
        );
        // And the bounce reaches the terrain beside the sheared hull.
        let block = MeshOccluderBlock::build(&[occ], &europa());
        let beside = c + occ.axis(0) * 3.0 - Vec3::Y * c.y;
        let (d, _) = occluder_light(&block, beside, Vec3::Y, true);
        assert!(d[0] > 0.0, "bounce beside the sheared hull: {d:?}");
    }

    #[test]
    fn from_positions_bounds_a_rotated_mesh() {
        let model = (Mat4::from_translation(Vec3::new(10.0, 2.0, 0.0)) * Mat4::from_rotation_y(0.5))
            .to_cols_array_2d();
        let occ = MeshOccluder::from_positions([[-1.0, -2.0, -0.5], [1.0, 1.0, 0.5]], model, [0.5; 3])
            .expect("box");
        assert!((occ.half_extents[1] - 1.5).abs() < 1e-5);
        assert!((occ.ground_y - 0.0).abs() < 1e-5);
        assert!(MeshOccluder::from_positions(std::iter::empty(), model, [0.5; 3]).is_none());
    }

    #[test]
    fn a_repeated_build_does_not_rehull() {
        let light = europa();
        let proxies = [hull()];
        reset_occluder_build_cache();
        reset_hull_calls();
        let first = MeshOccluderBlock::build(&proxies, &light);
        assert_eq!(hull_calls(), 1, "one proxy hulls once");
        let second = MeshOccluderBlock::build(&proxies, &light);
        assert_eq!(first, second);
        assert_eq!(hull_calls(), 1, "the same proxies and light rebuilt the hull");
        let mut moved = light;
        moved.eye[0] += 25.0;
        let _ = MeshOccluderBlock::build(&proxies, &moved);
        assert_eq!(hull_calls(), 2, "a moved eye rebuilds the hull");
    }
}
