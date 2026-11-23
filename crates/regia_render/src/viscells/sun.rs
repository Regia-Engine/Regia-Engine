// SPDX-License-Identifier: MPL-2.0
//! Sun sealing, stage 3 of indoor occlusion (ADR-8414, plan row W-23): the draws of a cell the sun
//! cannot reach leave the sun's cascades.
//!
//! Two tests, both conservative (each errs towards casting):
//!
//! - **The sun walk** ([`sun_walk`]). Every portal quad is projected along the sun's direction onto
//!   a plane across it. From the outdoors (cell 0), a portal passes sunlight when its projection
//!   overlaps the projected region the light came through (the whole plane outdoors), with more
//!   than [`SUN_PORTAL_MIN_AREA_M2`] of overlap; the cell behind it is sun-reached and the walk goes
//!   on there with the overlap as its region. A straight sun ray through portals `p1 … pk` projects
//!   to one point inside every `pi`, so any real light path is found. Order along the ray and the
//!   side a portal faces are not tested: the walk may reach more cells than light does, never
//!   fewer. The caps are the view walk's ([`MAX_PORTAL_DEPTH`], [`MAX_CELL_VISITS`]) and both
//!   count more cells as reached.
//! - **Cover** ([`SunCover`]). The cell model says a cell's walls are opaque apart from its portals,
//!   but a cell's walls are often its own draws (a tube's shell): taking them out of the cascades
//!   would let the sun in. So a cell only seals when its host vouches that its boxes lie under
//!   other cover that stays in the cascades, for the current sun ([`box_covered`] is the test,
//!   for example against the terrain over a buried tube). Without a [`SunCover`] resource nothing
//!   is sealed.
//!
//! A cell that is covered and not sun-reached is **sun-sealed**. A rigid draw whose `VisCell` (or
//! nearest tagged ancestor's) is a sun-sealed cell, and whose mesh bounds lie inside one of that
//! cell's boxes grown by [`SUN_SEAL_SLACK_M`], gets [`ShadowCast::Local`] with
//! `max_cascades: 0`: no sun cascade, no near sun tile, no traced sun scene, while local lights
//! still shadow it. A shadow-only draw (its colour already culled) in a frame with no local light
//! leaves the frame. Every ray from such a draw toward the sun first crosses the cover, which is
//! still drawn, so no receiver's sun shadow test changes.
//!
//! **Shadow views per cell** (ADR-8874) need no cover. A sun shadow only matters where a view sees
//! it: inside the boxes of the cells the views reach, and outdoors through the frusta their walks
//! record into cell 0 ([`super::OutdoorPortals`]). A rigid draw of a cell no view reaches whose
//! shadow, its box swept along the sun ([`shadow_sweep_touches_box`],
//! [`shadow_sweep_touches_planes`], grown by [`SHADOW_VIEW_MARGIN_M`] and [`SHADOW_VIEW_SPREAD`]),
//! touches none of those regions also gets [`SUN_SEALED_SHADOW`]: a deeper tunnel under the one
//! the eye is in, a side gallery the sun's shadows run away from. With a view that sees the
//! outdoors unrestricted (an eye outdoors, a capped walk, a render-to-texture view that reaches
//! cell 0) nothing is culled this way.

use std::sync::Arc;

use regia_core::math::Vec3;

use super::file::{CellBox, VisCells};
use super::traverse::{MAX_CELL_VISITS, MAX_PORTAL_DEPTH};
use crate::{CpuMesh, ShadowCast};
use regia_app::prelude::*;

/// A portal passes sunlight only when its projected overlap with the incoming region exceeds this
/// area, square metres (a portal edge-on to the sun passes none).
pub const SUN_PORTAL_MIN_AREA_M2: f32 = 1.0e-4;

/// A draw's bounds may stick out of its cell's box by this much and still count as inside it,
/// metres. [`box_covered`] grows the box by the same amount, so the slack is covered too.
pub const SUN_SEAL_SLACK_M: f32 = 0.05;

/// The sun must stand at least this high (the sine of its elevation) for [`box_covered`] to
/// vouch for anything: a grazing sun's rays run under cover for kilometres.
pub const SUN_COVER_MIN_ELEVATION_SIN: f32 = 0.02;

/// Most vertices of a projected region; a longer intersection keeps the portal alone (wider).
const MAX_SUN_POLY: usize = 32;

/// Room for one clip pass: a convex polygon gains at most one vertex per clipping edge.
const CLIP_BUF: usize = MAX_SUN_POLY + 8;

/// The ids of the cells whose boxes lie under cover that stays in the sun's cascades, for the
/// current sun (module docs). The host keeps it current (agent: `render.set_viscells`
/// `sun_cover`) and recomputes it when the sun moves. Empty (the default) seals nothing.
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct SunCover {
    /// Ascending, no repeats.
    cells: Vec<u16>,
    /// The sun direction (light travel) the host computed the cover for.
    direction: Option<[f32; 3]>,
}

impl SunCover {
    /// A cover set from cell ids (any order; repeats are dropped) computed for the sun travelling
    /// along `direction` (light travel, any length).
    #[must_use]
    pub fn new(cells: impl IntoIterator<Item = u16>, direction: [f32; 3]) -> Self {
        let mut cover = Self::default();
        cover.set(cells, direction);
        cover
    }

    /// Replaces the set, keeping its capacity.
    pub fn set(&mut self, cells: impl IntoIterator<Item = u16>, direction: [f32; 3]) {
        self.cells.clear();
        self.cells.extend(cells);
        self.cells.sort_unstable();
        self.cells.dedup();
        self.direction = Some(direction);
    }

    /// Empties the set.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.direction = None;
    }

    /// Whether `cell` is covered.
    #[must_use]
    pub fn contains(&self, cell: u16) -> bool {
        self.cells.binary_search(&cell).is_ok()
    }

    /// The covered cell ids, ascending.
    #[must_use]
    pub fn cells(&self) -> &[u16] {
        &self.cells
    }

    /// The sun direction (light travel) the set was computed for, if any.
    #[must_use]
    pub fn direction(&self) -> Option<[f32; 3]> {
        self.direction
    }

    /// Whether the set was computed for a sun within about 0.06° of `direction`: a cover computed
    /// for another sun vouches for nothing (the sweep of [`box_covered`] depends on it).
    #[must_use]
    pub fn matches_sun(&self, direction: [f32; 3]) -> bool {
        let Some(a) = self.direction else {
            return false;
        };
        let (a, b) = (Vec3::from_array(a), Vec3::from_array(direction));
        let (la, lb) = (a.length(), b.length());
        la > 0.0 && lb > 0.0 && a.dot(b) >= 0.999_999 * la * lb
    }
}

/// Whether every sun ray from `cell_box` (grown by `slack` metres) toward the sun crosses cover
/// before it leaves the ground: the test behind [`SunCover`].
///
/// `to_sun` points toward the sun (any length). `top` is the highest the cover can reach (the
/// terrain's maximum height). The box's footprint is swept toward the sun until the ray has risen
/// from the box's bottom to `top`; `min_cover(lo, hi)` returns the lowest height the cover is drawn
/// at in the shadow cascades over the XZ rectangle `lo..hi`, or `None` when the cover has a gap
/// there (a hole, the map's edge, cover that casts no sun shadow). The box is covered when that
/// lowest height is above the grown box's top: a ray from inside the box starts under the cover,
/// ends above it, and so crosses it, inside the swept rectangle, where there is no gap.
#[must_use]
pub fn box_covered(
    cell_box: &CellBox,
    to_sun: Vec3,
    slack: f32,
    top: f32,
    mut min_cover: impl FnMut([f32; 2], [f32; 2]) -> Option<f32>,
) -> bool {
    let s = to_sun.normalize_or_zero();
    let usable = s.y >= SUN_COVER_MIN_ELEVATION_SIN && top.is_finite() && slack >= 0.0;
    if !usable {
        return false;
    }
    let grown = CellBox {
        half_extents: cell_box.half_extents.map(|h| h + slack),
        ..*cell_box
    };
    let corners = grown.corners();
    let (mut lo, mut hi) = (corners[0], corners[0]);
    for c in &corners[1..] {
        lo = lo.min(*c);
        hi = hi.max(*c);
    }
    if !lo.is_finite() || !hi.is_finite() {
        return false;
    }
    let rise = top - lo.y;
    if rise <= 0.0 {
        // The box reaches above every cover.
        return false;
    }
    let run = rise / s.y;
    let shift = [s.x * run, s.z * run];
    let rect_lo = [lo.x.min(lo.x + shift[0]), lo.z.min(lo.z + shift[1])];
    let rect_hi = [hi.x.max(hi.x + shift[0]), hi.z.max(hi.z + shift[1])];
    min_cover(rect_lo, rect_hi).is_some_and(|h| h > hi.y)
}

/// A convex polygon in the sun's projection plane, counter-clockwise.
#[derive(Clone, Copy, Debug)]
struct Poly {
    v: [[f32; 2]; MAX_SUN_POLY],
    n: u8,
}

impl Poly {
    fn points(&self) -> &[[f32; 2]] {
        &self.v[..usize::from(self.n)]
    }
}

fn signed_area(p: &[[f32; 2]]) -> f32 {
    let mut twice = 0.0;
    for (i, a) in p.iter().enumerate() {
        let b = p[(i + 1) % p.len()];
        twice += a[0] * b[1] - b[0] * a[1];
    }
    0.5 * twice
}

/// `subject` clipped to the inside of every edge of the convex counter-clockwise `clip`
/// (Sutherland and Hodgman 1974). Writes into `out` and returns the vertex count, or `None` when the
/// result would not fit [`MAX_SUN_POLY`].
fn clip_convex(subject: &[[f32; 2]], clip: &[[f32; 2]], out: &mut [[f32; 2]; MAX_SUN_POLY]) -> Option<usize> {
    let mut a = [[0.0f32; 2]; CLIP_BUF];
    let mut b = [[0.0f32; 2]; CLIP_BUF];
    let mut len = subject.len().min(CLIP_BUF);
    a[..len].copy_from_slice(&subject[..len]);
    for (i, &e0) in clip.iter().enumerate() {
        if len == 0 {
            break;
        }
        let e1 = clip[(i + 1) % clip.len()];
        let edge = [e1[0] - e0[0], e1[1] - e0[1]];
        let side = |p: [f32; 2]| edge[0] * (p[1] - e0[1]) - edge[1] * (p[0] - e0[0]);
        let mut out_len = 0usize;
        for (j, &p) in a.iter().enumerate().take(len) {
            let q = a[(j + 1) % len];
            let (sp, sq) = (side(p), side(q));
            if sp >= 0.0 {
                if out_len >= CLIP_BUF {
                    return None;
                }
                b[out_len] = p;
                out_len += 1;
            }
            if (sp >= 0.0) != (sq >= 0.0) {
                let t = sp / (sp - sq);
                if out_len >= CLIP_BUF {
                    return None;
                }
                b[out_len] = [p[0] + t * (q[0] - p[0]), p[1] + t * (q[1] - p[1])];
                out_len += 1;
            }
        }
        std::mem::swap(&mut a, &mut b);
        len = out_len;
    }
    if len > MAX_SUN_POLY {
        return None;
    }
    out[..len].copy_from_slice(&a[..len]);
    Some(len)
}

/// One pending cell entry of the sun walk.
#[derive(Clone, Copy, Debug)]
struct SunEntry {
    cell: u32,
    /// The portal the light came through.
    from: u32,
    depth: u32,
    region: Poly,
}

/// The sun walk's reusable scratch.
#[derive(Default, Debug)]
pub struct SunWalkScratch {
    reached: Vec<bool>,
    stack: Vec<SunEntry>,
    flood: Vec<u32>,
    projected: Vec<Option<Poly>>,
}

impl SunWalkScratch {
    /// Per cell index: sun-reached in the last [`sun_walk`].
    #[must_use]
    pub fn reached(&self) -> &[bool] {
        &self.reached
    }
}

/// What one [`sun_walk`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SunWalk {
    /// Cell entries made (cell 0's included).
    pub visits: u32,
    /// The walk hit [`MAX_CELL_VISITS`] and counted every cell sun-reached.
    pub capped: bool,
    /// A chain hit [`MAX_PORTAL_DEPTH`]; every cell connected beyond it counted sun-reached.
    pub depth_capped: bool,
}

/// Which cells the sun travelling along `direction` (light travel, any length) reaches through the
/// portals from the outdoors (module docs). Writes [`SunWalkScratch::reached`] (cell 0 always). A
/// zero or non-finite direction reaches every cell. Allocation-free once warm.
pub fn sun_walk(cells: &VisCells, direction: Vec3, scratch: &mut SunWalkScratch) -> SunWalk {
    let n = cells.cells.len();
    scratch.reached.clear();
    scratch.reached.resize(n, false);
    scratch.stack.clear();
    let mut walk = SunWalk::default();
    if n == 0 {
        return walk;
    }
    scratch.reached[0] = true;
    let s = -direction.normalize_or_zero();
    if s == Vec3::ZERO || !s.is_finite() {
        scratch.reached.fill(true);
        return walk;
    }
    let u = if s.x.abs() < 0.9 {
        Vec3::X.cross(s)
    } else {
        Vec3::Y.cross(s)
    }
    .normalize();
    let v = s.cross(u);
    scratch.projected.clear();
    scratch.projected.extend(cells.portals.iter().map(|portal| {
        let mut poly = Poly {
            v: [[0.0; 2]; MAX_SUN_POLY],
            n: 4,
        };
        for (i, corner) in portal.quad.iter().enumerate() {
            poly.v[i] = [corner.dot(u), corner.dot(v)];
        }
        let area = signed_area(poly.points());
        if area < 0.0 {
            poly.v[..4].reverse();
        }
        (area.abs() > SUN_PORTAL_MIN_AREA_M2).then_some(poly)
    }));
    // Outdoors the region is the whole plane: each of cell 0's portals passes as projected.
    walk.visits = 1;
    for &pi in &cells.cells[0].portals {
        let Some(poly) = scratch.projected[pi as usize] else {
            continue;
        };
        scratch.stack.push(SunEntry {
            cell: cells.portals[pi as usize].other(0),
            from: pi,
            depth: 1,
            region: poly,
        });
    }
    while let Some(entry) = scratch.stack.pop() {
        walk.visits += 1;
        if walk.visits > MAX_CELL_VISITS {
            walk.capped = true;
            scratch.reached.fill(true);
            scratch.stack.clear();
            return walk;
        }
        scratch.reached[entry.cell as usize] = true;
        for &pi in &cells.cells[entry.cell as usize].portals {
            if pi == entry.from {
                continue;
            }
            let Some(portal_poly) = scratch.projected[pi as usize] else {
                continue;
            };
            let mut out = [[0.0f32; 2]; MAX_SUN_POLY];
            let region = match clip_convex(portal_poly.points(), entry.region.points(), &mut out) {
                Some(len) => {
                    if signed_area(&out[..len]) <= SUN_PORTAL_MIN_AREA_M2 {
                        continue;
                    }
                    Poly { v: out, n: len as u8 }
                }
                // Too many vertices: keep the portal alone, a wider region.
                None => portal_poly,
            };
            let other = cells.portals[pi as usize].other(entry.cell);
            if entry.depth >= MAX_PORTAL_DEPTH {
                walk.depth_capped = true;
                flood(cells, other, &mut scratch.reached, &mut scratch.flood);
                continue;
            }
            scratch.stack.push(SunEntry {
                cell: other,
                from: pi,
                depth: entry.depth + 1,
                region,
            });
        }
    }
    walk
}

/// Marks every cell connected to `start` (through any portals) reached.
fn flood(cells: &VisCells, start: u32, reached: &mut [bool], stack: &mut Vec<u32>) {
    stack.clear();
    stack.push(start);
    while let Some(cell) = stack.pop() {
        if std::mem::replace(&mut reached[cell as usize], true) && cell != start {
            continue;
        }
        for &pi in &cells.cells[cell as usize].portals {
            let other = cells.portals[pi as usize].other(cell);
            if !reached[other as usize] {
                stack.push(other);
            }
        }
    }
}

/// Mesh bounds the seal tests read, cached per mesh (the cache holds the `Arc`, so an address is
/// never reused while cached).
#[derive(Default)]
pub(crate) struct MeshBoundsCache {
    entries: Vec<(Arc<CpuMesh>, Option<MeshAabb>)>,
}

/// An object-space AABB, `(min, max)`.
type MeshAabb = ([f32; 3], [f32; 3]);

/// More cached meshes than this empties the cache (a steady scene holds far fewer sealed meshes).
const MESH_BOUNDS_CACHE_CAP: usize = 4096;

impl MeshBoundsCache {
    /// The mesh's object-space AABB (`None` for a mesh without vertices).
    pub(crate) fn bounds(&mut self, mesh: &Arc<CpuMesh>) -> Option<MeshAabb> {
        if let Some((_, b)) = self.entries.iter().find(|(m, _)| Arc::ptr_eq(m, mesh)) {
            return *b;
        }
        if self.entries.len() >= MESH_BOUNDS_CACHE_CAP {
            self.entries.clear();
        }
        let b = mesh.vertices.first().map(|first| {
            let (mut lo, mut hi) = (first.position, first.position);
            for vertex in &mesh.vertices[1..] {
                for c in 0..3 {
                    lo[c] = lo[c].min(vertex.position[c]);
                    hi[c] = hi[c].max(vertex.position[c]);
                }
            }
            (lo, hi)
        });
        self.entries.push((Arc::clone(mesh), b));
        b
    }
}

/// Whether the object-space box `lo..hi` drawn with the column-major `model` lies inside `cell_box`
/// grown by [`SUN_SEAL_SLACK_M`] (all eight corners: the box is convex).
pub(crate) fn inside_box(lo: [f32; 3], hi: [f32; 3], model: &[[f32; 4]; 4], cell_box: &CellBox) -> bool {
    let half = Vec3::from_array(cell_box.half_extents) + Vec3::splat(SUN_SEAL_SLACK_M);
    (0..8).all(|i| {
        let p = [
            if i & 1 == 0 { lo[0] } else { hi[0] },
            if i & 2 == 0 { lo[1] } else { hi[1] },
            if i & 4 == 0 { lo[2] } else { hi[2] },
        ];
        let world = Vec3::new(
            model[0][0] * p[0] + model[1][0] * p[1] + model[2][0] * p[2] + model[3][0],
            model[0][1] * p[0] + model[1][1] * p[1] + model[2][1] * p[2] + model[3][1],
            model[0][2] * p[0] + model[1][2] * p[1] + model[2][2] * p[2] + model[3][2],
        );
        world.is_finite() && cell_box.to_local(world).abs().cmple(half).all()
    })
}

/// Shadow-view culling (ADR-8874): how far a caster's swept shadow may pass a visible region and
/// still count as touching it, metres. Covers the cascades' filter kernels, normal offset and
/// texel snapping, which can darken a receiver a little beyond the caster's hard shadow.
pub const SHADOW_VIEW_MARGIN_M: f32 = 1.0;

/// Shadow-view culling (ADR-8874): the swept shadow also widens by this much per metre it travels
/// (about 1.1°, four times the sun's angular radius): the soft penumbra of a far caster.
pub const SHADOW_VIEW_SPREAD: f32 = 0.02;

/// Narrows `[t0, t1]` to the `t` where `a + b·t <= 0`.
fn narrow(a: f32, b: f32, t0: &mut f32, t1: &mut f32) {
    if b > 0.0 {
        *t1 = t1.min(-a / b);
    } else if b < 0.0 {
        *t0 = t0.max(-a / b);
    } else if a > 0.0 {
        *t1 = f32::NEG_INFINITY;
    }
}

/// Whether the shadow of the world box `lo..hi` can touch the oriented `receiver` box (ADR-8874):
/// the box swept along `travel` (the sun's light travel, any length) for every `t >= 0`, grown by
/// `margin + spread·t` metres, overlaps the receiver. The caster box is taken into the receiver's
/// frame (a box turned about +Y encloses it there), so the overlap test along the sweep is exact
/// per axis: each axis gives an interval of `t` and the sweep touches when the intervals meet.
/// The answer errs toward `true` (non-finite input touches).
#[must_use]
pub fn shadow_sweep_touches_box(
    lo: [f32; 3],
    hi: [f32; 3],
    travel: Vec3,
    receiver: &CellBox,
    margin: f32,
    spread: f32,
) -> bool {
    let d = travel.normalize_or_zero();
    let (lo, hi) = (Vec3::from_array(lo), Vec3::from_array(hi));
    if d == Vec3::ZERO || !d.is_finite() || !lo.is_finite() || !hi.is_finite() {
        return true;
    }
    let centre = receiver.to_local((lo + hi) * 0.5);
    let half = (hi - lo) * 0.5;
    let (s, c) = receiver.rotation_radians.sin_cos();
    let (s, c) = (s.abs(), c.abs());
    let half = Vec3::new(c * half.x + s * half.z, half.y, s * half.x + c * half.z);
    let origin = receiver.to_local(Vec3::ZERO);
    let dir = receiver.to_local(d) - origin;
    let (mut t0, mut t1) = (0.0f32, f32::INFINITY);
    for (axis, receiver_half) in receiver.half_extents.iter().enumerate() {
        let extent = half[axis] + receiver_half + margin;
        // |centre + t·dir| <= extent + spread·t, as two half-lines in t.
        narrow(centre[axis] - extent, dir[axis] - spread, &mut t0, &mut t1);
        narrow(-centre[axis] - extent, -dir[axis] - spread, &mut t0, &mut t1);
    }
    // A NaN on either side touches.
    t0 <= t1 || t0.is_nan() || t1.is_nan()
}

/// Whether the shadow of the world box `lo..hi` swept along `travel` (grown as in
/// [`shadow_sweep_touches_box`]) can enter the convex volume `planes`: no plane has the whole
/// sweep outside it (the box's farthest corner is more than `margin` outside, and the sweep never
/// turns toward the inside faster than it widens). Errs toward `true`.
#[must_use]
pub fn shadow_sweep_touches_planes(
    lo: [f32; 3],
    hi: [f32; 3],
    travel: Vec3,
    planes: &super::PlaneSet,
    margin: f32,
    spread: f32,
) -> bool {
    let d = travel.normalize_or_zero();
    if d == Vec3::ZERO || !d.is_finite() {
        return true;
    }
    !planes.planes().iter().any(|p| {
        let n = Vec3::new(p[0], p[1], p[2]);
        let len = n.length();
        if !(len.is_finite() && len > 0.0) {
            return false;
        }
        let far = Vec3::new(
            if p[0] >= 0.0 { hi[0] } else { lo[0] },
            if p[1] >= 0.0 { hi[1] } else { lo[1] },
            if p[2] >= 0.0 { hi[2] } else { lo[2] },
        );
        let distance = n.dot(far) + p[3];
        distance + margin * len < 0.0 && n.dot(d) + spread * len <= 0.0
    })
}

/// The world AABB of the object-space box `lo..hi` drawn with the column-major `model`.
pub(crate) fn world_aabb(lo: [f32; 3], hi: [f32; 3], model: &[[f32; 4]; 4]) -> ([f32; 3], [f32; 3]) {
    let mut out_lo = [f32::INFINITY; 3];
    let mut out_hi = [f32::NEG_INFINITY; 3];
    for i in 0..8 {
        let p = [
            if i & 1 == 0 { lo[0] } else { hi[0] },
            if i & 2 == 0 { lo[1] } else { hi[1] },
            if i & 4 == 0 { lo[2] } else { hi[2] },
        ];
        let world: [f32; 3] = std::array::from_fn(|axis| {
            model[0][axis] * p[0] + model[1][axis] * p[1] + model[2][axis] * p[2] + model[3][axis]
        });
        for ((lo, hi), w) in out_lo.iter_mut().zip(out_hi.iter_mut()).zip(world) {
            *lo = lo.min(w);
            *hi = hi.max(w);
        }
    }
    (out_lo, out_hi)
}

/// The shadow a sun-sealed draw keeps: local lights only.
pub const SUN_SEALED_SHADOW: ShadowCast = ShadowCast::Local {
    reach: 0.0,
    max_cascades: 0,
    shadow_scale: 1.0,
    shadow_lift: 0.0,
    anchor: None,
    lod_switch_keeps_pages: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Cell 1, a buried box; cell 2 behind it; a vertical mouth from 0 to 1 facing +Z, and a
    /// junction from 1 to 2 in the floor.
    fn tube() -> VisCells {
        VisCells::from_ron(
            "(cells: [
                (id: 1, bounds: [(center: (0.0, -10.0, 0.0), half_extents: (2.0, 2.0, 10.0))]),
                (id: 2, bounds: [(center: (0.0, -20.0, 0.0), half_extents: (2.0, 2.0, 2.0))]),
            ], portals: [
                (a: 0, b: 1, quad: [(-2.0, -12.0, 10.0), (2.0, -12.0, 10.0), (2.0, -8.0, 10.0), (-2.0, -8.0, 10.0)]),
                (a: 1, b: 2, quad: [(-1.0, -12.0, -1.0), (1.0, -12.0, -1.0), (1.0, -12.0, 1.0), (-1.0, -12.0, 1.0)]),
            ])",
        )
        .expect("loads")
    }

    #[test]
    fn a_sun_behind_the_mouth_reaches_nothing_inside() {
        let cells = tube();
        let mut scratch = SunWalkScratch::default();
        // Straight down: the vertical mouth is edge-on.
        sun_walk(&cells, Vec3::new(0.0, -1.0, 0.0), &mut scratch);
        assert_eq!(scratch.reached(), &[true, false, false]);
    }

    #[test]
    fn a_sun_into_the_mouth_reaches_the_tube_and_through_the_floor_portal() {
        let cells = tube();
        let mut scratch = SunWalkScratch::default();
        // Low sun travelling toward −Z and down: the mouth's projection overlaps the floor
        // portal's, so the walk goes on into cell 2.
        let walk = sun_walk(&cells, Vec3::new(0.0, -0.4, -1.0), &mut scratch);
        assert_eq!(scratch.reached(), &[true, true, true], "{walk:?}");
        // Travelling sideways (+X): the mouth (in a Z plane) and the floor portal are both
        // edge-on.
        sun_walk(&cells, Vec3::new(1.0, 0.0, 0.0), &mut scratch);
        assert_eq!(scratch.reached(), &[true, false, false]);
    }

    #[test]
    fn a_zero_sun_reaches_every_cell() {
        let mut scratch = SunWalkScratch::default();
        sun_walk(&tube(), Vec3::ZERO, &mut scratch);
        assert!(scratch.reached().iter().all(|r| *r));
    }

    #[test]
    fn cover_needs_ground_over_the_swept_footprint() {
        let b = CellBox {
            center: [0.0, -10.0, 0.0],
            half_extents: [2.0, 2.0, 2.0],
            rotation_radians: 0.0,
        };
        let flat = |h: f32| move |_: [f32; 2], _: [f32; 2]| Some(h);
        let up = Vec3::Y;
        assert!(box_covered(&b, up, SUN_SEAL_SLACK_M, 0.0, flat(0.0)));
        assert!(
            !box_covered(&b, up, SUN_SEAL_SLACK_M, 0.0, flat(-8.0)),
            "ground below the box top"
        );
        assert!(!box_covered(&b, up, SUN_SEAL_SLACK_M, 0.0, |_, _| None), "a gap");
        assert!(
            !box_covered(&b, Vec3::new(1.0, 0.0, 0.0), SUN_SEAL_SLACK_M, 0.0, flat(0.0)),
            "a sun on the horizon"
        );
        // A 45° sun from +X sweeps the footprint 12.05 m toward +X (the rise from the grown
        // bottom, −12.05, to 0).
        let mut seen = None;
        assert!(box_covered(
            &b,
            Vec3::new(1.0, 1.0, 0.0),
            SUN_SEAL_SLACK_M,
            0.0,
            |lo, hi| {
                seen = Some((lo, hi));
                Some(0.0)
            }
        ));
        let (lo, hi) = seen.expect("asked");
        assert!(
            (lo[0] + 2.05).abs() < 1e-3 && (hi[0] - 14.1).abs() < 1e-3,
            "{lo:?} {hi:?}"
        );
        assert!((lo[1] + 2.05).abs() < 1e-3 && (hi[1] - 2.05).abs() < 1e-3);
    }

    #[test]
    fn a_box_and_its_slack_hold_a_draw() {
        let b = CellBox {
            center: [0.0, 0.0, 0.0],
            half_extents: [1.0, 1.0, 1.0],
            rotation_radians: 0.0,
        };
        let identity = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        assert!(inside_box([-1.04; 3], [1.04; 3], &identity, &b));
        assert!(!inside_box([-1.1; 3], [1.0; 3], &identity, &b));
        let mut moved = identity;
        moved[3] = [0.6, 0.0, 0.0, 1.0];
        assert!(!inside_box([-0.5; 3], [0.5; 3], &moved, &b));
    }

    #[test]
    fn a_shadow_sweep_touches_only_boxes_downstream() {
        let tube = CellBox {
            center: [0.0, -10.0, 0.0],
            half_extents: [3.0, 3.0, 20.0],
            rotation_radians: 0.0,
        };
        let down = Vec3::new(0.0, -1.0, 0.0);
        // A caster 10 m under the tube: its shadow goes further down.
        assert!(!shadow_sweep_touches_box(
            [-1.0, -26.0, -1.0],
            [1.0, -24.0, 1.0],
            down,
            &tube,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        // The same caster above the tube shadows into it.
        assert!(shadow_sweep_touches_box(
            [-1.0, 4.0, -1.0],
            [1.0, 6.0, 1.0],
            down,
            &tube,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        // Beside the tube at its depth, 20 m off in X: a straight-down sun and a 45° sun toward
        // −X both miss it (the 45° shadow sinks under the tube before it gets there); a sun
        // that travels toward −X almost level reaches its side.
        let beside = ([19.0, -11.0, -1.0], [21.0, -9.0, 1.0]);
        assert!(!shadow_sweep_touches_box(
            beside.0,
            beside.1,
            down,
            &tube,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        assert!(!shadow_sweep_touches_box(
            beside.0,
            beside.1,
            Vec3::new(-1.0, -1.0, 0.0),
            &tube,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        assert!(shadow_sweep_touches_box(
            beside.0,
            beside.1,
            Vec3::new(-1.0, -0.05, 0.0),
            &tube,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        // The margin: 0.9 m off the tube's side under a straight-down sun touches, 1.2 m does not.
        assert!(shadow_sweep_touches_box(
            [3.9, -9.0, -1.0],
            [5.0, -8.0, 1.0],
            down,
            &tube,
            SHADOW_VIEW_MARGIN_M,
            0.0
        ));
        assert!(!shadow_sweep_touches_box(
            [4.2, -9.0, -1.0],
            [5.0, -8.0, 1.0],
            down,
            &tube,
            SHADOW_VIEW_MARGIN_M,
            0.0
        ));
        // A turned receiver: the same tube turned 90° runs along X, so a caster above x = 15 hits it.
        let turned = CellBox {
            rotation_radians: std::f32::consts::FRAC_PI_2,
            ..tube
        };
        assert!(shadow_sweep_touches_box(
            [14.0, 4.0, -1.0],
            [16.0, 6.0, 1.0],
            down,
            &turned,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        assert!(!shadow_sweep_touches_box(
            [14.0, 4.0, -1.0],
            [16.0, 6.0, 1.0],
            down,
            &tube,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        // Broken input touches.
        assert!(shadow_sweep_touches_box(
            [f32::NAN; 3],
            [0.0; 3],
            down,
            &tube,
            1.0,
            0.0
        ));
        assert!(shadow_sweep_touches_box(
            [0.0; 3],
            [1.0; 3],
            Vec3::ZERO,
            &tube,
            1.0,
            0.0
        ));
    }

    #[test]
    fn a_shadow_sweep_leaves_a_volume_only_moving_away() {
        // The half-space y >= 0 (one inward plane).
        let mut frustum = [[0.0f32; 4]; 6];
        frustum[0] = [0.0, 1.0, 0.0, 0.0];
        for p in &mut frustum[1..] {
            // Planes that take everything: 0·p + 1e6 >= 0.
            *p = [0.0, 0.0, 0.0, 1.0e6];
        }
        let above = super::super::PlaneSet::from_frustum(frustum);
        let deep = ([-1.0, -20.0, -1.0], [1.0, -18.0, 1.0]);
        assert!(!shadow_sweep_touches_planes(
            deep.0,
            deep.1,
            Vec3::new(0.3, -1.0, 0.0),
            &above,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        // Light travelling up out of the ground could reach it (never a sun, but the test errs
        // toward true).
        assert!(shadow_sweep_touches_planes(
            deep.0,
            deep.1,
            Vec3::new(0.0, 1.0, 0.0),
            &above,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        // Within the margin of the plane.
        assert!(shadow_sweep_touches_planes(
            [-1.0, -2.0, -1.0],
            [1.0, -0.5, 1.0],
            Vec3::new(0.0, -1.0, 0.0),
            &above,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
        // A grazing sun widens faster than it sinks: kept.
        assert!(shadow_sweep_touches_planes(
            deep.0,
            deep.1,
            Vec3::new(1.0, -0.01, 0.0),
            &above,
            SHADOW_VIEW_MARGIN_M,
            SHADOW_VIEW_SPREAD
        ));
    }

    #[test]
    fn a_world_aabb_encloses_the_moved_box() {
        let mut model = [
            [2.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [5.0, -3.0, 1.0, 1.0],
        ];
        let (lo, hi) = world_aabb([-0.5; 3], [0.5; 3], &model);
        assert_eq!((lo, hi), ([4.0, -3.5, 0.5], [6.0, -2.5, 1.5]));
        // Turned 90° about Y: X and Z swap.
        model[0] = [0.0, 0.0, -1.0, 0.0];
        model[2] = [1.0, 0.0, 0.0, 0.0];
        let (lo, hi) = world_aabb([-1.0, -0.5, -0.25], [1.0, 0.5, 0.25], &model);
        assert_eq!((lo, hi), ([4.75, -3.5, 0.0], [5.25, -2.5, 2.0]));
    }

    #[test]
    fn a_cover_set_only_vouches_for_its_own_sun() {
        let cover = SunCover::new([3, 1, 3], [0.0, -1.0, 0.0]);
        assert_eq!(cover.cells(), &[1, 3]);
        assert!(cover.contains(3) && !cover.contains(2));
        assert!(cover.matches_sun([0.0, -2.0, 0.0]));
        assert!(!cover.matches_sun([0.1, -1.0, 0.0]));
        assert!(!SunCover::default().matches_sun([0.0, -1.0, 0.0]));
    }
}
