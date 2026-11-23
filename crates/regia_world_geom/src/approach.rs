// SPDX-License-Identifier: MPL-2.0
//! Approach strip from a mouth ring to a ramp top that does not sit on the ground (W-14b,
//! ADR-8396).
//!
//! On a fixture ground that is flat at a datum, a ramp can end metres above it (an elevated
//! base) or below it (a pit portal). The cut opens the lattice past the ramp's top edge, so a
//! body that drives off the ground into the cut reads the ramp floor tens of metres away. The
//! cook closes that one edge, and only that edge:
//!
//! 1. A band at the pins: the part of the ring past the top edge, translated inward by up to
//!    [`BAND_MM`]. It holds every ring sample, and its f32 neighbors, at the pin height, so the
//!    ring-edge collar and the 1 mm wedges under it are not needed.
//! 2. A rise from the band's inner edge to the two corners of the ramp floor's top edge. Those
//!    corners are the floor's own millimetre vertices, so the rise and the floor share that edge.
//!
//! Each ring edge of the run is an edge of exactly one band triangle, and the collar triangles
//! on those edges are not written, so the band costs about what the collar did. A body on the rise has no floor under it in the cut, so support follows the rise
//! however steep it is.
//!
//! **An open-cut climb at the ground (W-14c, ADR-8533).** A ramp whose climb is open to the sky
//! (a skylight shaft that K8 opens, `crate::sections::open_skylight_shafts`) ends on the
//! ground, so its top sits on the pins and no approach is needed for height. When K8 opens more
//! of the climb than the layout the mouth was cut from, the mesher's lips run down the trench
//! past the ring: the zipper's band does not reach the top edge, and the strip between the ring
//! and the top edge holds only the collar, open over the tunnel. The cook then asks for the strip
//! anyway ([`approach_strip`] with `on_pins`): the same band and a flat rise.

use regia_detsim::float::{det_max_f64, det_min_f64};
use regia_detsim::math::sqrt_f64;

use crate::cook::WorldTriangleMm;
use crate::cooked::TriKind;

/// Inward width of the band at the pins, millimetres. A collar triangle's centroid sits at most
/// a third of a lattice diagonal (about 0.15 m) from its ring edge; the band covers it at any
/// edge angle up to 45 degrees off the ramp axis.
pub(crate) const BAND_MM: f64 = 250.0;

/// Ring vertices count as the top edge's run only this far past it and beside it, millimetres.
/// The cut runs 1 m past a ramp top (`HOLE_END_M`), so 4 m keeps other mouths' rings out.
pub(crate) const REACH_MM: f64 = 4000.0;

/// A ramp top within this of every pin needs no approach: the hop bar is 5 cm.
pub(crate) const APPROACH_MIN_STEP_MM: i64 = 20;

/// A ring edge by its two millimetre vertices.
pub(crate) type RingEdge = ([i32; 3], [i32; 3]);

/// The top edge of a ramp floor, world millimetres.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RampTop {
    /// The floor's top-edge corner on the ramp's left.
    pub left: [i32; 3],
    /// The floor's top-edge corner on the ramp's right.
    pub right: [i32; 3],
    /// A point on the ramp axis behind the top edge (the previous sample), for the outward
    /// direction.
    pub behind: [i32; 3],
}

impl RampTop {
    fn mid(&self) -> [f64; 2] {
        [
            (f64::from(self.left[0]) + f64::from(self.right[0])) * 0.5,
            (f64::from(self.left[2]) + f64::from(self.right[2])) * 0.5,
        ]
    }
    /// Unit XZ direction out of the ramp past its top edge.
    fn out_dir(&self) -> Option<[f64; 2]> {
        let m = self.mid();
        let (dx, dz) = (m[0] - f64::from(self.behind[0]), m[1] - f64::from(self.behind[2]));
        let len = sqrt_f64(dx * dx + dz * dz);
        (len > 1.0).then(|| [dx / len, dz / len])
    }
}

/// Distance past the top edge (`t`) and toward the left corner (`s`), millimetres.
fn frame(top: &RampTop, out: [f64; 2], p: [i32; 3]) -> (f64, f64) {
    let m = top.mid();
    let (dx, dz) = (f64::from(p[0]) - m[0], f64::from(p[2]) - m[1]);
    let t = dx * out[0] + dz * out[1];
    // Left is the outward direction turned so the left corner has positive s.
    let lx = f64::from(top.left[0]) - m[0];
    let lz = f64::from(top.left[2]) - m[1];
    let llen = det_max_f64(sqrt_f64(lx * lx + lz * lz), 1.0);
    (t, (dx * lx + dz * lz) / llen)
}

/// The ring vertices past the top edge, as one run ordered from the left side to the right.
fn end_run(ring: &[[i32; 3]], top: &RampTop, out: [f64; 2]) -> Vec<[i32; 3]> {
    let n = ring.len();
    if n < 3 {
        return Vec::new();
    }
    let half = det_max_f64(
        frame(top, out, top.left).1.abs(),
        frame(top, out, top.right).1.abs(),
    );
    let past: Vec<bool> = ring
        .iter()
        .map(|&p| {
            let (t, s) = frame(top, out, p);
            t > 0.0 && t < REACH_MM && s.abs() < half + REACH_MM
        })
        .collect();
    // Start the walk on a vertex that is not past, so every run is contiguous in the walk.
    let Some(start) = (0..n).find(|&i| !past[i]) else {
        return Vec::new();
    };
    let mut runs: Vec<Vec<[i32; 3]>> = Vec::new();
    let mut current = Vec::new();
    for k in 1..=n {
        let i = (start + k) % n;
        if past[i] {
            current.push(ring[i]);
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    // The run that holds the vertex nearest the ramp axis is the top edge's.
    let axis = |run: &Vec<[i32; 3]>| {
        run.iter()
            .map(|&p| frame(top, out, p).1.abs())
            .fold(f64::MAX, det_min_f64)
    };
    let Some(mut run) = runs.into_iter().min_by(|a, b| axis(a).total_cmp(&axis(b))) else {
        return Vec::new();
    };
    if run.len() >= 2 && frame(top, out, run[0]).1 < frame(top, out, run[run.len() - 1]).1 {
        run.reverse();
    }
    run
}

fn floor_tri(a: [i32; 3], b: [i32; 3], c: [i32; 3]) -> Option<WorldTriangleMm> {
    let cross = (i64::from(b[0]) - i64::from(a[0])) * (i64::from(c[2]) - i64::from(a[2]))
        - (i64::from(c[0]) - i64::from(a[0])) * (i64::from(b[2]) - i64::from(a[2]));
    if cross == 0 {
        return None;
    }
    // Wind so the face reads up (the cook's floors have a negative XZ cross, as the mesher's).
    let v = if cross > 0 { [a, c, b] } else { [a, b, c] };
    Some(WorldTriangleMm {
        v,
        kind: TriKind::Floor,
        flags: 0,
    })
}

/// `true` when the ramp top is off the ring's pins past it by more than
/// [`APPROACH_MIN_STEP_MM`].
pub(crate) fn needs_approach(ring: &[[i32; 3]], top: &RampTop) -> bool {
    let Some(out) = top.out_dir() else {
        return false;
    };
    let run = end_run(ring, top, out);
    let y = |p: [i32; 3]| i64::from(p[1]);
    run.iter().any(|&p| {
        (y(top.left) - y(p)).abs() > APPROACH_MIN_STEP_MM
            || (y(top.right) - y(p)).abs() > APPROACH_MIN_STEP_MM
    })
}

/// The point `t` past the top edge and `s` toward the left corner, at height `y`, millimetres.
fn at_frame(top: &RampTop, out: [f64; 2], t: f64, s: f64, y: i32) -> [i32; 3] {
    let m = top.mid();
    let lx = f64::from(top.left[0]) - m[0];
    let lz = f64::from(top.left[2]) - m[1];
    let llen = det_max_f64(sqrt_f64(lx * lx + lz * lz), 1.0);
    [
        (m[0] + out[0] * t + lx / llen * s).round_ties_even() as i32,
        y,
        (m[1] + out[1] * t + lz / llen * s).round_ties_even() as i32,
    ]
}

/// Triangulates the strip between two open polylines that run the same way (greedy on
/// normalized arc length, the Turk/Levoy zipper on open ends).
fn stitch_open(a: &[[i32; 3]], b: &[[i32; 3]]) -> Vec<WorldTriangleMm> {
    let arc = |line: &[[i32; 3]]| -> Vec<f64> {
        let mut cum = vec![0.0; line.len()];
        for k in 1..line.len() {
            let (dx, dz) = (
                f64::from(line[k][0]) - f64::from(line[k - 1][0]),
                f64::from(line[k][2]) - f64::from(line[k - 1][2]),
            );
            cum[k] = cum[k - 1] + sqrt_f64(dx * dx + dz * dz);
        }
        let total = det_max_f64(cum[line.len() - 1], 1.0e-9);
        cum.iter().map(|c| c / total).collect()
    };
    let (arc_a, arc_b) = (arc(a), arc(b));
    let (mut i, mut j) = (0, 0);
    let mut tris = Vec::new();
    while i + 1 < a.len() || j + 1 < b.len() {
        let mut advance_a = j + 1 >= b.len() || (i + 1 < a.len() && arc_a[i + 1] <= arc_b[j + 1]);
        // A degenerate step would leave that edge of `a` in no triangle: step `b` first.
        if advance_a && j + 1 < b.len() && floor_tri(a[i], a[i + 1], b[j]).is_none() {
            advance_a = false;
        }
        if advance_a {
            tris.extend(floor_tri(a[i], a[i + 1], b[j]));
            i += 1;
        } else {
            tris.extend(floor_tri(a[i], b[j + 1], b[j]));
            j += 1;
        }
    }
    tris
}

/// The band and the rise for one ramp top on one ring, and the ring edges the band holds (each an
/// edge of exactly one band triangle). Empty when the ring has no run past the top edge, the run is too close to it, or the
/// top already sits on the pins and `on_pins` is `false`. With `on_pins` the strip is written for a
/// top on the pins too (an open-cut climb the zipper gave no band, ADR-8533); the rise is then flat.
pub(crate) fn approach_strip(
    ring: &[[i32; 3]],
    top: &RampTop,
    on_pins: bool,
) -> (Vec<WorldTriangleMm>, Vec<RingEdge>) {
    let none = (Vec::new(), Vec::new());
    let Some(out) = top.out_dir() else {
        return none;
    };
    if !on_pins && !needs_approach(ring, top) {
        return none;
    }
    let full = end_run(ring, top, out);
    if full.len() < 2 {
        return none;
    }
    let (_, s_left) = frame(top, out, top.left);
    let (_, s_right) = frame(top, out, top.right);
    // The inner line runs straight across the top edge's width, BAND_MM inside the nearest ring
    // vertex in that width.
    let across: Vec<&[i32; 3]> = full
        .iter()
        .filter(|&&p| {
            let s = frame(top, out, p).1;
            s <= s_left + BAND_MM && s >= s_right - BAND_MM
        })
        .collect();
    let Some(near) = across.iter().map(|&&p| frame(top, out, p).0).reduce(det_min_f64) else {
        return none;
    };
    let t_in = near - BAND_MM;
    if t_in < 50.0 {
        return none;
    }
    let pin_at = |s: f64| -> i32 {
        full.iter()
            .min_by(|a, b| {
                (frame(top, out, **a).1 - s)
                    .abs()
                    .total_cmp(&(frame(top, out, **b).1 - s).abs())
            })
            .map_or(0, |p| p[1])
    };
    let inner_left = at_frame(top, out, t_in, s_left, pin_at(s_left));
    let inner_right = at_frame(top, out, t_in, s_right, pin_at(s_right));
    // Every ring edge of the run is an edge of exactly one band triangle (spec 121 § 3, the ring
    // pin, item 3); the cook does not write the collar triangles on the edges the band holds.
    let (first, last) = (full[0], full[full.len() - 1]);
    let mut tris = stitch_open(&full, &[first, inner_left, inner_right, last]);
    let held: Vec<RingEdge> = full
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .filter(|&(a, b)| tris.iter().any(|t| has_edge(t, a, b)))
        .collect();
    // Rise to the floor's own top-edge corners. Nothing is written between the run's ends and
    // the corners: a steep wedge there covers ring samples beside the corner above the pin.
    tris.extend(floor_tri(inner_left, inner_right, top.right));
    tris.extend(floor_tri(inner_left, top.right, top.left));
    (tris, held)
}

/// `true` when `a`-`b` is an edge of `t`.
pub(crate) fn has_edge(t: &WorldTriangleMm, a: [i32; 3], b: [i32; 3]) -> bool {
    (0..3).any(|e| {
        let (p, q) = (t.v[e], t.v[(e + 1) % 3]);
        (p == a && q == b) || (p == b && q == a)
    })
}

/// `true` when the XZ projections of `a` and `b` share interior area (a shared edge or corner
/// alone is not an overlap). Exact integer separating-axis test.
pub(crate) fn xz_overlap(a: &WorldTriangleMm, b: &WorldTriangleMm) -> bool {
    let pa = a.v.map(|p| [i64::from(p[0]), i64::from(p[2])]);
    let pb = b.v.map(|p| [i64::from(p[0]), i64::from(p[2])]);
    let separates = |tri: &[[i64; 2]; 3], other: &[[i64; 2]; 3]| -> bool {
        let area = (tri[1][0] - tri[0][0]) * (tri[2][1] - tri[0][1])
            - (tri[2][0] - tri[0][0]) * (tri[1][1] - tri[0][1]);
        if area == 0 {
            return true;
        }
        (0..3).any(|e| {
            let (p, q) = (tri[e], tri[(e + 1) % 3]);
            let side =
                |r: [i64; 2]| ((q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0])) * area.signum();
            other.iter().all(|&r| side(r) <= 0)
        })
    };
    !separates(&pa, &pb) && !separates(&pb, &pa)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane_y(t: &WorldTriangleMm, x: f64, z: f64) -> Option<f64> {
        let p = t.v.map(|q| q.map(f64::from));
        let det = (p[1][0] - p[0][0]) * (p[2][2] - p[0][2]) - (p[2][0] - p[0][0]) * (p[1][2] - p[0][2]);
        if det == 0.0 {
            return None;
        }
        let l1 = ((x - p[0][0]) * (p[2][2] - p[0][2]) - (p[2][0] - p[0][0]) * (z - p[0][2])) / det;
        let l2 = ((p[1][0] - p[0][0]) * (z - p[0][2]) - (x - p[0][0]) * (p[1][2] - p[0][2])) / det;
        let l0 = 1.0 - l1 - l2;
        let e = -1.0e-9;
        (l0 >= e && l1 >= e && l2 >= e).then(|| l0 * p[0][1] + l1 * p[1][1] + l2 * p[2][1])
    }

    /// A 20 m ramp along -x ends 28.81 m over a ring 1 m past it, cut at the lattice pitch.
    fn elevated() -> (Vec<[i32; 3]>, RampTop) {
        let top = RampTop {
            left: [-190_000, 28_810, 50_000],
            right: [-190_000, 28_810, 30_000],
            behind: [-189_000, 28_000, 40_000],
        };
        // Ring around x in [-191_000, -150_000], z in [27_500, 52_500], every 312.5 mm.
        let mut ring = Vec::new();
        let step = 312;
        let mut z = 27_500;
        while z < 52_500 {
            ring.push([-191_000, 0, z]);
            z += step;
        }
        let mut x = -191_000;
        while x < -150_000 {
            ring.push([x, 0, 52_500]);
            x += step;
        }
        let mut z = 52_500;
        while z > 27_500 {
            ring.push([-150_000, 0, z]);
            z -= step;
        }
        let mut x = -150_000;
        while x > -191_000 {
            ring.push([x, 0, 27_500]);
            x -= step;
        }
        (ring, top)
    }

    #[test]
    fn approach_is_continuous_from_the_ring_to_the_top_edge() {
        let (ring, top) = elevated();
        let (tris, held) = approach_strip(&ring, &top, false);
        assert!(!tris.is_empty());
        // The run past the top edge: the end line and the corner stubs of the sides.
        let run_edges = ring
            .iter()
            .zip(ring.iter().cycle().skip(1))
            .filter(|(a, b)| a[0] < -190_000 && b[0] < -190_000)
            .count();
        assert_eq!(held.len(), run_edges, "the band holds every ring edge of the run");
        assert!(
            tris.len() <= run_edges + 4,
            "{} triangles for {run_edges} run edges",
            tris.len()
        );
        for &(a, b) in &held {
            assert_eq!(
                tris.iter().filter(|t| has_edge(t, a, b)).count(),
                1,
                "ring edge {a:?}-{b:?}"
            );
        }
        // Along the axis from the ring to the top edge: one surface, from the pin to the top.
        let z = 40_000.0;
        let mut previous = None;
        for k in 0..=1000 {
            let x = -191_000.0 + 1000.0 * f64::from(k) / 1000.0;
            let ys: Vec<f64> = tris.iter().filter_map(|t| plane_y(t, x, z)).collect();
            assert!(!ys.is_empty(), "no approach floor at x={x}");
            let lo = ys.iter().copied().fold(f64::MAX, det_min_f64);
            let hi = ys.iter().copied().fold(f64::MIN, det_max_f64);
            assert!(hi - lo < 1.0, "two approach floors at x={x}: {ys:?}");
            if let Some(p) = previous {
                assert!(lo >= p - 1.0, "the approach steps down at x={x}");
            }
            previous = Some(lo);
        }
        assert!(previous.is_some_and(|y| (y - 28_810.0).abs() < 1.0));
        // The band holds the ring edge at the pins.
        for t in &tris {
            for v in t.v {
                if v[0] == -191_000 {
                    assert_eq!(v[1], 0);
                }
            }
        }
    }

    #[test]
    fn a_top_on_the_pins_gets_no_approach() {
        let (ring, mut top) = elevated();
        top.left[1] = 10;
        top.right[1] = 10;
        assert!(approach_strip(&ring, &top, false).0.is_empty());
    }

    /// ADR-8533: an open-cut climb's top on the pins still gets the strip when the cook asks for it,
    /// and it is flat: one floor at the pin height from the ring to the top edge.
    #[test]
    fn a_top_on_the_pins_gets_a_flat_strip_when_asked() {
        let (ring, mut top) = elevated();
        top.left[1] = 0;
        top.right[1] = 0;
        top.behind[1] = -2_500;
        let (tris, held) = approach_strip(&ring, &top, true);
        assert!(!tris.is_empty(), "a strip for the open-cut climb");
        assert!(!held.is_empty());
        for &(a, b) in &held {
            assert_eq!(
                tris.iter().filter(|t| has_edge(t, a, b)).count(),
                1,
                "ring edge {a:?}-{b:?}"
            );
        }
        for t in &tris {
            assert!(t.v.iter().all(|v| v[1] == 0), "flat at the pins: {:?}", t.v);
        }
        // Across the top edge's width, from the ring to the top edge: covered everywhere.
        for zk in 0..=20 {
            let z = 30_500.0 + 19_000.0 * f64::from(zk) / 20.0;
            for k in 0..=100 {
                let x = -191_000.0 + 1000.0 * f64::from(k) / 100.0;
                assert!(
                    tris.iter().any(|t| plane_y(t, x, z).is_some()),
                    "no strip floor at x={x}, z={z}"
                );
            }
        }
    }

    #[test]
    fn xz_overlap_needs_shared_area() {
        let tri = |v: [[i32; 3]; 3]| WorldTriangleMm {
            v,
            kind: TriKind::Floor,
            flags: 0,
        };
        let a = tri([[0, 0, 0], [1000, 0, 0], [0, 0, 1000]]);
        let inside = tri([[100, 5, 100], [200, 5, 100], [100, 5, 200]]);
        let edge = tri([[1000, 0, 0], [0, 0, 1000], [1000, 0, 1000]]);
        let apart = tri([[2000, 0, 0], [3000, 0, 0], [2000, 0, 1000]]);
        assert!(xz_overlap(&a, &inside));
        assert!(xz_overlap(&inside, &a));
        assert!(!xz_overlap(&a, &edge));
        assert!(!xz_overlap(&a, &apart));
    }
}
