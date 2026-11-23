// SPDX-License-Identifier: MPL-2.0
//! Centripetal Catmull-Rom interpolation through camera path keys.
//!
//! The curve passes through every key. Each segment is evaluated with the Barry-Goldman pyramidal
//! formulation on centripetal knots (`t_{i+1} = t_i + |P_{i+1} - P_i|^0.5`), which cannot form
//! cusps or loops inside a segment, unlike the uniform parameterization (Yuksel, Schaefer and
//! Keyser, 2011). The first and last segments use mirrored phantom points
//! (`P_-1 = 2 P_0 - P_1`), so a path of two keys is a straight line. Provenance:
//! `docs/provenance.md`, "Camera paths".
//!
//! Everything here is a pure function of its arguments: no clocks, no state, no randomness.

use regia_core::glam::DVec3;
use regia_core::math::Vec3;

/// Smallest knot interval, so two coincident points never divide by zero.
const MIN_KNOT_STEP: f32 = 1.0e-4;

/// The knot interval between two points: an IEEE `sqrt` (correctly rounded on every target),
/// never platform libm `powf(0.5)` (framework3 determinism audit).
fn knot_step(a: Vec3, b: Vec3) -> f32 {
    a.distance(b).sqrt().max(MIN_KNOT_STEP)
}

/// [`knot_step`] in `f64`, for the path heading's finite-difference samples.
fn knot_step_f64(a: DVec3, b: DVec3) -> f64 {
    a.distance(b).sqrt().max(f64::from(MIN_KNOT_STEP))
}

/// The point at parameter `u` (`0..=1`) of the segment from `p1` to `p2`, with `p0` and `p3` the
/// neighbouring points. `u = 0` returns `p1` and `u = 1` returns `p2` exactly.
///
/// The segment math lives in `regia_scene::spline` (spec 64 § 1); camera paths keep their
/// centripetal (`sqrt`) knot step, so their curves are unchanged.
#[must_use]
pub fn catmull_rom_segment(p0: Vec3, p1: Vec3, p2: Vec3, p3: Vec3, u: f32) -> Vec3 {
    regia_scene::spline::catmull_rom_segment_with(p0, p1, p2, p3, u, knot_step)
}

/// [`catmull_rom_segment`] in `f64`, for the path heading's finite difference: its two samples
/// are centimetres apart, and `f32` positions kilometres from the origin round by about 1e-4 m,
/// which turned the heading into noise (ADR-1488). Same knots, same pyramid.
#[must_use]
pub fn catmull_rom_segment_f64(p0: DVec3, p1: DVec3, p2: DVec3, p3: DVec3, u: f64) -> DVec3 {
    if u <= 0.0 {
        return p1;
    }
    if u >= 1.0 {
        return p2;
    }
    let knot = knot_step_f64;
    let t0 = 0.0;
    let t1 = t0 + knot(p0, p1);
    let t2 = t1 + knot(p1, p2);
    let t3 = t2 + knot(p2, p3);
    let t = t1 + (t2 - t1) * u;

    let a1 = p0 * ((t1 - t) / (t1 - t0)) + p1 * ((t - t0) / (t1 - t0));
    let a2 = p1 * ((t2 - t) / (t2 - t1)) + p2 * ((t - t1) / (t2 - t1));
    let a3 = p2 * ((t3 - t) / (t3 - t2)) + p3 * ((t - t2) / (t3 - t2));
    let b1 = a1 * ((t2 - t) / (t2 - t0)) + a2 * ((t - t0) / (t2 - t0));
    let b2 = a2 * ((t3 - t) / (t3 - t1)) + a3 * ((t - t1) / (t3 - t1));
    b1 * ((t2 - t) / (t2 - t1)) + b2 * ((t - t1) / (t2 - t1))
}

/// The point on the spline through `points` at segment `index` (between `points[index]` and
/// `points[index + 1]`) and parameter `u`. Phantom end points are mirrored. Needs at least two
/// points and `index + 1 < points.len()`; anything else returns the nearest point (or the origin
/// for an empty slice).
#[must_use]
pub fn catmull_rom(points: &[Vec3], index: usize, u: f32) -> Vec3 {
    regia_scene::spline::catmull_rom_with(points, index, u, knot_step)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Vec<Vec3> {
        vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(4.0, 0.0, 0.0),
            Vec3::new(4.0, 0.0, 4.0),
            Vec3::new(0.0, 0.0, 4.0),
        ]
    }

    #[test]
    fn passes_through_every_key_exactly() {
        let points = square();
        for i in 0..points.len() - 1 {
            assert_eq!(catmull_rom(&points, i, 0.0), points[i]);
            assert_eq!(catmull_rom(&points, i, 1.0), points[i + 1]);
        }
    }

    #[test]
    fn two_points_make_a_straight_line() {
        let points = [Vec3::new(0.0, 1.0, 2.0), Vec3::new(10.0, 1.0, 2.0)];
        let mid = catmull_rom(&points, 0, 0.5);
        assert!(mid.abs_diff_eq(Vec3::new(5.0, 1.0, 2.0), 1.0e-4), "{mid:?}");
    }

    #[test]
    fn collinear_evenly_spaced_points_stay_on_the_line() {
        let points: Vec<Vec3> = (0..5).map(|i| Vec3::new(i as f32, 2.0, -1.0)).collect();
        for i in 0..4 {
            let p = catmull_rom(&points, i, 0.5);
            assert!(
                p.abs_diff_eq(Vec3::new(i as f32 + 0.5, 2.0, -1.0), 1.0e-4),
                "{p:?}"
            );
        }
    }

    #[test]
    fn the_curve_bends_through_a_corner_without_overshooting_wildly() {
        let points = square();
        let p = catmull_rom(&points, 1, 0.5);
        // Between (4,0,0) and (4,0,4) the curve bulges outward by 0.5 (worked by hand from the
        // pyramid: knots 0, 2, 4, 6, A1 = (6,0,0), A2 = (4,0,2), A3 = (6,0,4), C = (4.5,0,2)).
        assert!(p.abs_diff_eq(Vec3::new(4.5, 0.0, 2.0), 1.0e-4), "{p:?}");
    }

    #[test]
    fn coincident_points_do_not_produce_nan() {
        let points = [Vec3::ONE, Vec3::ONE, Vec3::ONE, Vec3::new(3.0, 1.0, 1.0)];
        for i in 0..3 {
            for step in 0..=10 {
                let p = catmull_rom(&points, i, step as f32 / 10.0);
                assert!(p.is_finite(), "{p:?}");
            }
        }
    }

    #[test]
    fn the_f64_segment_matches_the_f32_one() {
        let points = square();
        let d = |p: Vec3| p.as_dvec3();
        for step in 0..=20 {
            let u = step as f32 / 20.0;
            let single = catmull_rom_segment(points[0], points[1], points[2], points[3], u);
            let double = catmull_rom_segment_f64(
                d(points[0]),
                d(points[1]),
                d(points[2]),
                d(points[3]),
                f64::from(u),
            );
            assert!(
                single.abs_diff_eq(double.as_vec3(), 1.0e-5),
                "u {u}: {single:?} vs {double:?}"
            );
        }
        assert_eq!(
            catmull_rom_segment_f64(DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::Z, 0.0),
            DVec3::X
        );
        assert_eq!(
            catmull_rom_segment_f64(DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::Z, 1.0),
            DVec3::Y
        );
        let flat = catmull_rom_segment_f64(DVec3::ONE, DVec3::ONE, DVec3::ONE, DVec3::ONE, 0.5);
        assert!(flat.is_finite(), "coincident points: {flat:?}");
    }

    /// The centripetal exponent 1/2 must be an IEEE sqrt, never platform libm `powf`: the knot
    /// bits must be the same on every target (framework3-audit finding 6). The s4 truck keys,
    /// pinned as literal `sqrt` bits (correctly rounded on every IEEE target, so a `powf`
    /// regression fails here even on a libm whose `powf(x, 0.5)` happens to equal `sqrt(x)`
    /// today — the old restated-formula assertion could not):
    #[test]
    fn knot_step_is_sqrt() {
        // `sqrt` of the f32/f64 distances of the s4 truck keys, bit-exact on every target.
        const TRUCK_KNOT_F32_BITS: u32 = 0x404b_7e91;
        const TRUCK_KNOT_F64_BITS: u64 = 0x4009_6fd2_28f4_5bf4;
        const MIN_KNOT_STEP_BITS: u32 = 0x38d1_b717;
        const MIN_KNOT_STEP_F64_BITS: u64 = 0x3f1a_36e2_e000_0000;
        let a = Vec3::new(-1752.0, 353.2, -2203.0);
        let b = Vec3::new(-1758.69, 354.7, -2195.57);
        for (p, q) in [(a, b), (b, a), (a, a)] {
            let d = p.distance(q);
            assert_eq!(
                knot_step(p, q).to_bits(),
                d.sqrt().max(MIN_KNOT_STEP).to_bits(),
                "f32 knot {p:?} -> {q:?}"
            );
            let dd = p.as_dvec3().distance(q.as_dvec3());
            assert_eq!(
                knot_step_f64(p.as_dvec3(), q.as_dvec3()).to_bits(),
                dd.sqrt().max(f64::from(MIN_KNOT_STEP)).to_bits(),
                "f64 knot {p:?} -> {q:?}"
            );
        }
        // The literal pins: identical on every target, whatever its libm's `powf` says.
        assert_eq!(knot_step(a, b).to_bits(), TRUCK_KNOT_F32_BITS);
        assert_eq!(knot_step(b, a).to_bits(), TRUCK_KNOT_F32_BITS);
        assert_eq!(knot_step(a, a).to_bits(), MIN_KNOT_STEP_BITS);
        assert_eq!(
            knot_step_f64(a.as_dvec3(), b.as_dvec3()).to_bits(),
            TRUCK_KNOT_F64_BITS
        );
        assert_eq!(
            knot_step_f64(a.as_dvec3(), a.as_dvec3()).to_bits(),
            MIN_KNOT_STEP_F64_BITS
        );
    }

    #[test]
    fn out_of_range_indices_return_the_nearest_point() {
        assert_eq!(catmull_rom(&[], 0, 0.5), Vec3::ZERO);
        let one = [Vec3::new(1.0, 2.0, 3.0)];
        assert_eq!(catmull_rom(&one, 0, 0.5), one[0]);
        let two = [Vec3::ZERO, Vec3::X];
        assert_eq!(catmull_rom(&two, 5, 0.5), Vec3::X);
    }
}
