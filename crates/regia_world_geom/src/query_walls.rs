// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/tunnels.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 616-749, mark: adapt (Stage B).
// Edits: canonical candidates, settings, contact result, det_* extrema; arithmetic and single-pass order retained.
//! One-pass disc confinement over canonical wall triangles.
//!
//! Speed (plan row W-11, ADR-8815): the walls of the fixed neighborhood are gathered from the
//! segment tables (`query_seg_index`: a box test per candidate, no triangle decoded), and a wall
//! is applied only when the disc, where it stands at that wall's turn, is within the wall's reach
//! of its bounds. Any other wall of the sequence is a no-op, so the
//! walls that move the disc, their order and every bit of the result are those of applying the
//! whole neighborhood in order (`push_disc_in_full_order`, the oracle).

use crate::WorldGeom;
use crate::cooked::TriKind;
use crate::edit::GeomState;
use crate::geom::GeomTriangle;
use crate::query::{Band, Push};
use crate::query_index::{canonical_mm_key, cross, sub};
use crate::query_seg_index::{Entry, PLUMB_REACH, QWindow, misses};
use regia_detsim::float::{det_clamp, det_max, det_min};

/// Slack of the near-wall test, metres, over the reach of [`near_factors`]: it covers the
/// rounding of `push_one`'s distance and span tests at world coordinates up to 1e4 m.
const NEAR_SLACK_M: f32 = 0.01;

/// The two factors `[k1, k2]` of a wall's reach: `push_one` can move a disc of radius `r` only
/// while its center is within `r * k1 + k2` (plus rounding, [`NEAR_SLACK_M`]) of the wall's XZ
/// bounds (ADR-8815).
///
/// `push_one` acts when the center `p` is less than `r` from the line through `a` across the
/// wall's horizontal normal `n`, and projects inside the wall's longest horizontal edge `ab`.
/// With `e` the unit direction of `ab`, `L` its length, and `p = a + u e + w e_perp`, that is
/// `0 <= u <= L` and `|u (n . e) + w (n . e_perp)| < r`, so `|w| < (r + L |n . e|) / |n . e_perp|`:
/// `k1 = 1 / |n . e_perp|`, `k2 = L |n . e| / |n . e_perp|`. A plumb wall has `n` across `ab`:
/// `k1 = 1`, `k2 = 0`. Both are rounded up by a part in a thousand. A wall `push_one` never
/// applies (no horizontal normal, or a longest edge under 0.1 mm) gets `-inf`: never near. A wall
/// whose normal runs along its edge gets `+inf`: always near.
pub(crate) fn near_factors(v: [[f32; 3]; 3]) -> [f32; 2] {
    let Some((normal, a, b)) = wall_segment(v) else {
        return [f32::NEG_INFINITY; 2];
    };
    let abx = b[0] - a[0];
    let abz = b[2] - a[2];
    // The same `f32` test as `push_one`.
    if abx * abx + abz * abz < 1e-8 {
        return [f32::NEG_INFINITY; 2];
    }
    let (ex, ez) = (f64::from(abx), f64::from(abz));
    let (nx, nz) = (f64::from(normal[0]), f64::from(normal[1]));
    // A load-time bound for an accelerator: no answer reads it (ADR-8815).
    let len = regia_detsim::math::sqrt_f64(ex * ex + ez * ez);
    let across = (nx * ez - nz * ex).abs() / len;
    let along = (nx * ex + nz * ez).abs() / len;
    if across.is_nan() || across < 1e-3 {
        return [f32::INFINITY; 2];
    }
    [(1.001 / across) as f32, (1.001 * len * along / across) as f32]
}

impl WorldGeom {
    /// Push a disc once through the walls in a fixed world-space neighborhood (radius plus the
    /// cooked wall pad) around the input center, in canonical millimetre-key order (ADR-8246 R3).
    /// Each wall sees the previous wall's output. Radius is bounded by the cooked wall padding.
    /// The physical index cell is only an accelerator: 4 m and 16 m cells visit the same walls.
    #[must_use]
    pub fn push_disc(&self, state: &GeomState, x: f32, z: f32, radius: f32, band: Band) -> Push {
        let mut result = Push {
            xz: [x, z],
            contacts: 0,
            last: None,
        };
        if ![x, z, radius, band.lo, band.hi].iter().all(|v| v.is_finite())
            || radius <= 0.0
            || band.lo > band.hi
        {
            return result;
        }
        // A pad below 5 cm cannot support the specified minimum radius: no contacts are safe.
        if self.settings().wall_pad_m < 0.05 {
            return result;
        }
        let radius = det_clamp(radius, 0.05, self.settings().wall_pad_m);
        let reach = radius + self.settings().wall_pad_m;
        let square = [x - reach, x + reach, z - reach, z + reach];
        let seg = self.seg_index();
        // The first gather reads only the cells within the largest reach of any wall of the map
        // (ADR-8815): a wall near the starting center has its bounds inside that smaller square,
        // so the first pass sees every wall it would see with the whole neighborhood. The whole
        // neighborhood is gathered at the first contact, when the center has moved.
        let quant = seg.quant();
        let [k1, k2] = seg.wall_reach();
        // The near test works in quanta, a few of them wider than the reach in metres.
        let first = radius * k1 + k2 + 2.0 * NEAR_SLACK_M + 8.0 * quant.quantum_m();
        let mut whole = first.is_nan() || first >= reach;
        let cells_of = |half: f32| [x - half, x + half, z - half, z + half];
        let mut chosen = [Entry::default(); 256];
        let mut gathered = gather_walls(
            self,
            state,
            if whole { square } else { cells_of(first) },
            square,
            band,
            &mut chosen,
        );
        if gathered.is_none() && !whole {
            whole = true;
            gathered = gather_walls(self, state, square, square, band, &mut chosen);
        }
        let Some(mut n) = gathered else {
            // More walls than the stack buffer.
            self.push_in_key_order(state, square, radius, band, &mut result);
            return result;
        };
        // A wall is near when the center's quantum is within the wall's reach, in quanta rounded
        // up, of its quantized bounds: a superset of the walls within reach in metres.
        let plumb = quant.span(radius * PLUMB_REACH[0] + PLUMB_REACH[1] + NEAR_SLACK_M);
        // `center` is the center's quanta, `[[x below, x above], [z below, z above]]`.
        let near = |entry: Entry, center: [[i32; 2]; 2]| {
            let reach = match seg.skew_reach(entry) {
                None => plumb,
                Some([k1, k2]) => quant.span(radius * k1 + k2 + NEAR_SLACK_M),
            };
            let b = seg.tri_box(entry.slot());
            (i32::from(b[0]) - reach <= center[0][1])
                & (i32::from(b[1]) + reach >= center[0][0])
                & (i32::from(b[4]) - reach <= center[1][1])
                & (i32::from(b[5]) + reach >= center[1][0])
        };
        // The walls after `after` in canonical order whose bounds are near the center, lowest
        // rank first. The center moves only at a contact, so between contacts the near set
        // is fixed: one pass finds it, and a contact starts the next pass at the walls after it.
        let index = self.query_index();
        let mut after: Option<u32> = None;
        let mut todo = [(0u32, 0u32); 256];
        'passes: loop {
            let center = result.xz;
            let quanta = [
                [quant.below(0, center[0]), quant.above(0, center[0])],
                [quant.below(2, center[1]), quant.above(2, center[1])],
            ];
            let mut m = 0usize;
            for &entry in &chosen[..n] {
                if !near(entry, quanta) {
                    continue;
                }
                let id = seg.id(entry.slot());
                let rank = index.rank(id);
                if after.is_none_or(|a| rank > a) {
                    // Insert in ascending rank; the near set is a handful of walls.
                    let mut at = m;
                    while at > 0 && todo[at - 1].0 > rank {
                        todo[at] = todo[at - 1];
                        at -= 1;
                    }
                    todo[at] = (rank, id);
                    m += 1;
                }
            }
            for &(rank, id) in &todo[..m] {
                after = Some(rank);
                // The sequence is the walls whose own bounds meet the neighborhood (the gather
                // tested a quantized box, which can be a few centimetres wider).
                let tri = self.tri(id);
                if !hits_square(tri.v, square[0], square[1], square[2], square[3]) {
                    continue;
                }
                push_one(self, state, &tri, radius, band, &mut result);
                if result.xz != center {
                    if !whole {
                        whole = true;
                        match gather_walls(self, state, square, square, band, &mut chosen) {
                            Some(all) => n = all,
                            None => {
                                // More walls than the stack buffer: the whole sequence again,
                                // from the input, without the buffer.
                                result = Push {
                                    xz: [x, z],
                                    contacts: 0,
                                    last: None,
                                };
                                self.push_in_key_order(state, square, radius, band, &mut result);
                                return result;
                            }
                        }
                    }
                    continue 'passes;
                }
            }
            break;
        }
        result
    }
    /// Every wall of the neighborhood `square` (`[x0, x1, z0, z1]`) applied in canonical key order,
    /// found by walking the cooked tiles once per wall: no buffer, so no wall is dropped.
    fn push_in_key_order(
        &self,
        state: &GeomState,
        square: [f32; 4],
        radius: f32,
        band: Band,
        result: &mut Push,
    ) {
        let [x0, x1, z0, z1] = square;
        let mut prev: Option<([[i32; 3]; 3], u16, u32)> = None;
        loop {
            let mut best: Option<([[i32; 3]; 3], u16, u32, u32)> = None;
            for (tile_i, tile) in self.cooked().tiles.iter().enumerate() {
                for local in 0..tile.triangles.len() {
                    let id = ((tile_i as u32) << 16) | (local as u32);
                    let tri = self.tri(id);
                    if !pushes(self, state, &tri) || !hits_square(tri.v, x0, x1, z0, z1) {
                        continue;
                    }
                    let key = (canonical_mm_key(tri.v), tri.tri.layer, tri.tri.triangle);
                    if prev.is_some_and(|p| key <= p) {
                        continue;
                    }
                    if best.is_none_or(|b| key < (b.0, b.1, b.2)) {
                        best = Some((key.0, key.1, key.2, id));
                    }
                }
            }
            let Some((_, _, _, id)) = best else {
                break;
            };
            let tri = self.tri(id);
            prev = Some((canonical_mm_key(tri.v), tri.tri.layer, tri.tri.triangle));
            push_one(self, state, &tri, radius, band, result);
        }
    }
}
#[cfg(any(test, feature = "stats"))]
impl WorldGeom {
    /// Reference for [`WorldGeom::push_disc`]: every wall of the same fixed neighborhood, found by
    /// a scan of every cooked triangle (no index, no near test), applied in canonical
    /// `(mm key, layer, index)` order. Allocates; for bit-equivalence checks (ADR-8815).
    #[must_use]
    pub fn push_disc_in_full_order(
        &self,
        state: &GeomState,
        x: f32,
        z: f32,
        radius: f32,
        band: Band,
    ) -> Push {
        let mut result = Push {
            xz: [x, z],
            contacts: 0,
            last: None,
        };
        if ![x, z, radius, band.lo, band.hi].iter().all(|v| v.is_finite())
            || radius <= 0.0
            || band.lo > band.hi
            || self.settings().wall_pad_m < 0.05
        {
            return result;
        }
        let radius = det_clamp(radius, 0.05, self.settings().wall_pad_m);
        let reach = radius + self.settings().wall_pad_m;
        let (x0, x1, z0, z1) = (x - reach, x + reach, z - reach, z + reach);
        let mut walls = Vec::new();
        for (tile_i, tile) in self.cooked().tiles.iter().enumerate() {
            for local in 0..tile.triangles.len() {
                let id = ((tile_i as u32) << 16) | (local as u32);
                let tri = self.tri(id);
                if pushes(self, state, &tri) && hits_square(tri.v, x0, x1, z0, z1) {
                    walls.push((canonical_mm_key(tri.v), tri.tri.layer, tri.tri.triangle, id));
                }
            }
        }
        walls.sort_unstable();
        for (_, _, _, id) in walls {
            push_one(self, state, &self.tri(id), radius, band, &mut result);
        }
        result
    }
}
fn hits_square(v: [[f32; 3]; 3], x0: f32, x1: f32, z0: f32, z1: f32) -> bool {
    let (mut min_x, mut max_x, mut min_z, mut max_z) = (v[0][0], v[0][0], v[0][2], v[0][2]);
    for p in &v[1..] {
        min_x = det_min(min_x, p[0]);
        max_x = det_max(max_x, p[0]);
        min_z = det_min(min_z, p[2]);
        max_z = det_max(max_z, p[2]);
    }
    min_x <= x1 && max_x >= x0 && min_z <= z1 && max_z >= z0
}
/// A wall, or the face of an active blocker plug (spec 121 § 5.3 rule 6; lane W-13).
#[inline]
fn pushes(geom: &WorldGeom, state: &GeomState, tri: &GeomTriangle) -> bool {
    tri.kind == TriKind::Wall
        || (tri.kind == TriKind::Blocker && state.any_blocker_active() && geom.plug_on(state, tri.section))
}
/// The walls of the neighborhood `square` that `push_one` can apply (band, plug state), read from
/// the cells the square `cells` covers (`[x0, x1, z0, z1]` each; `cells` is `square` or a part of
/// it). The square and the band are tested on the quantized boxes, so a wall a few centimetres
/// outside either can be listed: `push_one` tests the band itself, and the caller tests the square
/// before it applies a wall. `Some(n)` entries in `out`, or `None` when they do not fit or the cell
/// range is out of bounds.
// Inlined into `push_disc`, its one caller: left to the optimizer it flips with unrelated changes
// elsewhere in the crate, and `push_disc` then costs 0.5 to 1.3 µs more a call (ADR-8817).
#[inline]
fn gather_walls(
    geom: &WorldGeom,
    state: &GeomState,
    cells: [f32; 4],
    square: [f32; 4],
    band: Band,
    out: &mut [Entry],
) -> Option<usize> {
    let [x0, x1, z0, z1] = square;
    let seg = geom.seg_index();
    let cell = seg.cell;
    let lo_x = (cells[0] / cell).floor() as i32;
    let hi_x = (cells[1] / cell).floor() as i32;
    let lo_z = (cells[2] / cell).floor() as i32;
    let hi_z = (cells[3] / cell).floor() as i32;
    let span = (i64::from(hi_x) - i64::from(lo_x) + 1) * (i64::from(hi_z) - i64::from(lo_z) + 1);
    if !(1..=4096).contains(&span) {
        return None;
    }
    let window: QWindow = seg.quant().window([x0, band.lo, z0], [x1, band.hi, z1]);
    let blockers = state.any_blocker_active();
    let mut n = 0usize;
    for cz in lo_z..=hi_z {
        for cx in lo_x..=hi_x {
            for &entry in seg.walls_at([cx, cz]) {
                // A wall `push_one` never applies, or one wholly outside the neighborhood or the
                // band (a no-op in `push_one`, its first test; ADR-8662), changes no push.
                if entry.never_pushes() || misses(seg.tri_box(entry.slot()), &window) {
                    continue;
                }
                // The face of an active blocker plug only (spec 121 § 5.3 rule 6; lane W-13).
                if entry.is_blocker() {
                    let id = seg.id(entry.slot());
                    let section =
                        geom.cooked().tiles[(id >> 16) as usize].triangles[(id & 0xffff) as usize].section;
                    if !(blockers && geom.plug_on(state, section)) {
                        continue;
                    }
                }
                if out[..n].iter().any(|seen| seen.slot() == entry.slot()) {
                    continue;
                }
                if n == out.len() {
                    return None;
                }
                out[n] = entry;
                n += 1;
            }
        }
    }
    Some(n)
}
fn push_one(
    geom: &WorldGeom,
    state: &GeomState,
    tri: &GeomTriangle,
    radius: f32,
    band: Band,
    result: &mut Push,
) {
    if !pushes(geom, state, tri) {
        return;
    }
    let [a, b, c] = tri.v;
    let top = det_max(det_max(a[1], b[1]), c[1]);
    let bottom = det_min(det_min(a[1], b[1]), c[1]);
    if top < band.lo || bottom > band.hi {
        return;
    }
    let Some((normal, a, b)) = wall_segment(tri.v) else {
        return;
    };
    let [px, pz] = result.xz;
    // The tests below are independent of each other; the cheap ones run first (ADR-8815).
    let [nx, nz] = normal;
    let dist = nx * (px - a[0]) + nz * (pz - a[2]);
    if dist >= radius || dist <= -radius {
        return;
    }
    let abx = b[0] - a[0];
    let abz = b[2] - a[2];
    let len2 = abx * abx + abz * abz;
    if len2 < 1e-8 {
        return;
    }
    let t = ((px - a[0]) * abx + (pz - a[2]) * abz) / len2;
    if !(0.0..=1.0).contains(&t) {
        return;
    }
    if let Some((lo, hi)) = wall_span(tri.v, a, b, px, pz)
        && (hi < band.lo || lo > band.hi)
    {
        return;
    }
    let target = if dist >= 0.0 { radius } else { -radius };
    let push = target - dist;
    let next = [px + nx * push, pz + nz * push];
    if next != result.xz {
        result.contacts = result.contacts.saturating_add(1);
        result.last = Some(tri.tri);
    }
    result.xz = next;
}
type WallSegment = ([f32; 2], [f32; 3], [f32; 3]);
#[expect(
    clippy::disallowed_methods,
    reason = "ADR-8246: spec 121 §15.3 requires IEEE sqrt for the wall normal"
)]
fn wall_segment(v: [[f32; 3]; 3]) -> Option<WallSegment> {
    let [nx, ny, nz] = cross(sub(v[1], v[0]), sub(v[2], v[0]));
    // det-scan: allow(std_float_fn): spec 121 §15.3 pins IEEE sqrt in the disc push.
    let horiz = (nx * nx + nz * nz).sqrt();
    if horiz < 1e-6 || ny.abs() > horiz {
        return None;
    }
    let inv = 1.0 / horiz;
    let mut best: Option<(f32, [f32; 3], [f32; 3])> = None;
    for i in 0..3 {
        for j in i + 1..3 {
            let dx = v[i][0] - v[j][0];
            let dz = v[i][2] - v[j][2];
            let d2 = dx * dx + dz * dz;
            if best.is_none_or(|(bd, _, _)| d2 > bd) {
                best = Some((d2, v[i], v[j]));
            }
        }
    }
    best.map(|(_, a, b)| ([nx * inv, nz * inv], a, b))
}
fn wall_span(v: [[f32; 3]; 3], a: [f32; 3], b: [f32; 3], px: f32, pz: f32) -> Option<(f32, f32)> {
    let abx = b[0] - a[0];
    let abz = b[2] - a[2];
    let len2 = abx * abx + abz * abz;
    if len2 < 1e-8 {
        return None;
    }
    let param = |p: [f32; 3]| ((p[0] - a[0]) * abx + (p[2] - a[2]) * abz) / len2;
    let t = det_clamp(((px - a[0]) * abx + (pz - a[2]) * abz) / len2, 0.0, 1.0);
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for i in 0..3 {
        let (p, q) = (v[i], v[(i + 1) % 3]);
        let (tp, tq) = (param(p), param(q));
        if t < det_min(tp, tq) - 1e-4 || t > det_max(tp, tq) + 1e-4 {
            continue;
        }
        if (tq - tp).abs() < 1e-6 {
            lo = det_min(lo, det_min(p[1], q[1]));
            hi = det_max(hi, det_max(p[1], q[1]));
        } else {
            let y = p[1] + (q[1] - p[1]) * ((t - tp) / (tq - tp));
            lo = det_min(lo, y);
            hi = det_max(hi, y);
        }
    }
    (lo <= hi).then_some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-8815: wherever `push_one`'s own tests let a wall move a disc, the center is within
    /// `r * k1 + k2` (plus the slack) of the wall's XZ bounds, for plumb walls, slanted walls up to
    /// the 45 degree limit and walls whose longest edge runs along their slope.
    #[test]
    fn a_wall_moves_only_a_disc_within_its_reach() {
        let mut seed = 0x2545_f491u32;
        let mut unit = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        let (mut acted, mut plumb, mut skew) = (0u32, 0u32, 0u32);
        for i in 0..4000 {
            let origin = [
                (unit() - 0.5) * 1200.0,
                (unit() - 0.5) * 60.0,
                (unit() - 0.5) * 1200.0,
            ];
            // A wall quad corner triangle: plumb on even rounds, leaning on odd ones.
            let run = [(unit() - 0.5) * 12.0, (unit() - 0.5) * 12.0];
            let lean = if i % 2 == 0 {
                [0.0, 0.0]
            } else {
                [(unit() - 0.5) * 6.0, (unit() - 0.5) * 6.0]
            };
            let rise = 0.5 + unit() * 6.0;
            let v = [
                origin,
                [origin[0] + run[0], origin[1] + unit(), origin[2] + run[1]],
                [origin[0] + lean[0], origin[1] + rise, origin[2] + lean[1]],
            ];
            let [k1, k2] = near_factors(v);
            let Some((normal, a, b)) = wall_segment(v) else {
                assert_eq!(k1, f32::NEG_INFINITY);
                continue;
            };
            // A plumb wall's normal runs across its edge up to rounding; a leaning one's does not.
            if k1 < 1.002 && k2 < 1e-3 {
                plumb += 1;
            } else if k2 > 0.01 {
                skew += 1;
            }
            let xs = [v[0][0], v[1][0], v[2][0]];
            let zs = [v[0][2], v[1][2], v[2][2]];
            let lo = [
                xs.iter().copied().fold(f32::INFINITY, det_min),
                zs.iter().copied().fold(f32::INFINITY, det_min),
            ];
            let hi = [
                xs.iter().copied().fold(f32::NEG_INFINITY, det_max),
                zs.iter().copied().fold(f32::NEG_INFINITY, det_max),
            ];
            for _ in 0..64 {
                let radius = 0.05 + unit() * 7.95;
                let p = [
                    origin[0] + (unit() - 0.5) * 60.0,
                    origin[2] + (unit() - 0.5) * 60.0,
                ];
                // `push_one`'s tests, in its arithmetic.
                let dist = normal[0] * (p[0] - a[0]) + normal[1] * (p[1] - a[2]);
                let (abx, abz) = (b[0] - a[0], b[2] - a[2]);
                let len2 = abx * abx + abz * abz;
                if dist >= radius || dist <= -radius || len2 < 1e-8 {
                    continue;
                }
                let t = ((p[0] - a[0]) * abx + (p[1] - a[2]) * abz) / len2;
                if !(0.0..=1.0).contains(&t) {
                    continue;
                }
                acted += 1;
                let reach = radius * k1 + k2 + NEAR_SLACK_M;
                assert!(
                    p[0] >= lo[0] - reach
                        && p[0] <= hi[0] + reach
                        && p[1] >= lo[1] - reach
                        && p[1] <= hi[1] + reach,
                    "wall {v:?} radius {radius} center {p:?} reach {reach} factors {k1} {k2}"
                );
            }
        }
        assert!(
            acted > 2000 && plumb > 500 && skew > 500,
            "acted {acted} plumb {plumb} skew {skew}"
        );
    }
}
