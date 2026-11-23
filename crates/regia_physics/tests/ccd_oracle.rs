// SPDX-License-Identifier: MPL-2.0
//! Independent checks of `regia_physics::ccd` (added by the verification lane, ADR-2248).
//!
//! The module's own unit tests verify a TOI with the module's own distance query. These tests
//! use an independent oracle instead: parry's `distance` and `cast_shapes` (parry ships with
//! rapier, already a dependency of this crate), over seeded random shapes and poses, plus a
//! dense sampling of every sweep to check that no contact happens before the reported time and
//! that a `Separated` sweep really stays apart. Deterministic: explicit seed, no clock.

use glam::DVec3;
use rapier3d::parry::query::{self, ShapeCastOptions};
use rapier3d::parry::shape::{Ball, Capsule, Cone, ConvexPolyhedron, Cuboid, Cylinder, Shape};
use regia_core::math::{Isometry, Quat, Vec3};
use regia_physics::ccd::{
    ConvexInput, RigidMotion, ToiOptions, ToiResult, convex_distance, time_of_impact, time_of_impact_motion,
};
use regia_physics::components::ShapeAxis;

/// Counts contract violations instead of stopping at the first, so the report can give rates.
#[derive(Default)]
struct Violations {
    count: usize,
    first: Vec<String>,
}

impl Violations {
    fn record(&mut self, msg: String) {
        self.count += 1;
        if self.first.len() < 6 {
            self.first.push(msg);
        }
    }

    fn finish(&self, what: &str) {
        for m in &self.first {
            eprintln!("  {what} violation: {m}");
        }
        assert_eq!(self.count, 0, "{what}: {} violations", self.count);
    }
}

/// Identity on `bool`, so `check!` can negate any condition expression cleanly.
fn holds(condition: bool) -> bool {
    condition
}

macro_rules! check {
    ($v:expr, $cond:expr, $($fmt:tt)+) => {
        if !holds($cond) {
            $v.record(format!($($fmt)+));
        }
    };
}

/// xorshift64* with an explicit seed.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        let unit = (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32;
        lo + unit * (hi - lo)
    }

    fn unit_vec(&mut self) -> Vec3 {
        loop {
            let v = Vec3::new(
                self.range(-1.0, 1.0),
                self.range(-1.0, 1.0),
                self.range(-1.0, 1.0),
            );
            let l = v.length_squared();
            if l > 1e-3 && l <= 1.0 {
                return v / l.sqrt();
            }
        }
    }

    fn rotation(&mut self) -> Quat {
        let axis = self.unit_vec();
        Quat::from_axis_angle(axis, self.range(-std::f32::consts::PI, std::f32::consts::PI))
    }
}

/// A random convex shape and its parry twin (Y-axis shapes, which parry also uses).
fn random_shape(rng: &mut Rng) -> (ConvexInput, Box<dyn Shape>) {
    match rng.next_u64() % 6 {
        0 => {
            let r = rng.range(0.05, 0.6);
            (ConvexInput::sphere(r), Box::new(Ball::new(r)))
        }
        1 => {
            let he = Vec3::new(rng.range(0.02, 0.6), rng.range(0.02, 0.6), rng.range(0.02, 0.6));
            (
                ConvexInput::cuboid(he),
                Box::new(Cuboid::new(rapier3d::math::Vector::new(he.x, he.y, he.z))),
            )
        }
        2 => {
            let (hh, r) = (rng.range(0.0, 0.6), rng.range(0.05, 0.4));
            (
                ConvexInput::Capsule {
                    half_height: hh,
                    radius: r,
                    axis: ShapeAxis::Y,
                },
                Box::new(Capsule::new_y(hh, r)),
            )
        }
        3 => {
            let (hh, r) = (rng.range(0.05, 0.6), rng.range(0.05, 0.5));
            (
                ConvexInput::Cylinder {
                    half_height: hh,
                    radius: r,
                    axis: ShapeAxis::Y,
                },
                Box::new(Cylinder::new(hh, r)),
            )
        }
        4 => {
            let (hh, r) = (rng.range(0.05, 0.6), rng.range(0.05, 0.5));
            (
                ConvexInput::Cone {
                    half_height: hh,
                    radius: r,
                    axis: ShapeAxis::Y,
                },
                Box::new(Cone::new(hh, r)),
            )
        }
        _ => loop {
            let points: Vec<Vec3> = (0..10).map(|_| rng.unit_vec() * rng.range(0.1, 0.6)).collect();
            let pts: Vec<rapier3d::math::Vector> = points
                .iter()
                .map(|p| rapier3d::math::Vector::new(p.x, p.y, p.z))
                .collect();
            if let Some(poly) = ConvexPolyhedron::from_convex_hull(&pts) {
                break (ConvexInput::ConvexHull { points }, Box::new(poly));
            }
        },
    }
}

fn pose(xf: Isometry) -> rapier3d::math::Pose {
    regia_physics::convert::to_rapier_pose(xf.translation, xf.rotation)
}

/// Parry's GJK distance between the two placed shapes. Like any GJK it returns an upper bound
/// that can stop short of convergence (the verification lane saw it over-report by 2.6 mm on a
/// capsule against a cylinder rim, where ours was right), so it is an oracle for "ours is not
/// worse than parry", not an exact truth.
fn parry_distance(a: &dyn Shape, xa: Isometry, b: &dyn Shape, xb: Isometry) -> f32 {
    match query::distance(&pose(xa), a, &pose(xb), b) {
        Ok(d) => d.distance,
        Err(e) => panic!("parry distance unsupported: {e:?}"),
    }
}

/// Whether the two placed shapes interpenetrate, by parry's boolean test.
fn parry_intersecting(a: &dyn Shape, xa: Isometry, b: &dyn Shape, xb: Isometry) -> bool {
    match query::intersection_test(&pose(xa), a, &pose(xb), b) {
        Ok(i) => i.intersecting,
        Err(e) => panic!("parry intersection unsupported: {e:?}"),
    }
}

fn rv(v: Vec3) -> rapier3d::math::Vector {
    rapier3d::math::Vector::new(v.x, v.y, v.z)
}

/// The gap at the given poses as seen by both queries. Our witness points are checked to lie
/// on the shapes (parry's point query) and to be at least `distance` apart, so their gap is
/// a valid upper bound and `ours` is never above it; `best` is the smaller of that witness gap
/// and parry's distance (both upper bounds on the true distance).
struct Gap {
    /// parry's boolean test says the shapes interpenetrate (its GJK works to a relative
    /// tolerance of about 1.1e-3, `sqrt(10 * f32::EPSILON)`, so near contact it can say
    /// "intersecting" for shapes a few 1e-5 apart).
    intersecting: bool,
    /// A rigorous lower bound on the distance: the width of the slab between the shapes
    /// along our normal, from parry's exact support maps (`min_B n.y - max_A n.x`).
    slab: f32,
    witness_ok: bool,
    ours: f32,
    /// Distance between our witness points (an upper bound; `ours` is below it only when
    /// GJK stopped with its duality gap open and reported the lower bound).
    witness_gap: f32,
    parry: f32,
    best: f32,
}

impl Gap {
    /// Interpenetration beyond `depth` (parry-based tests use 1e-3, parry's own precision;
    /// the exact f64 test below checks the 1e-5 level): parry says intersecting and our slab bound cannot
    /// certify the shapes within `depth` of apart.
    fn penetrates(&self, depth: f32) -> bool {
        self.intersecting && self.slab < -depth
    }
}

fn gap(a: &ConvexInput, pa: &dyn Shape, xa: Isometry, b: &ConvexInput, pb: &dyn Shape, xb: Isometry) -> Gap {
    let ours = match convex_distance(a, xa, b, xb) {
        Ok(d) => d,
        Err(e) => panic!("convex_distance failed: {e:?}"),
    };
    let intersecting = parry_intersecting(pa, xa, pb, xb);
    // parry builds its own hull from the points, which can sit ~2e-5 inside the exact hull,
    // hence the 5e-5 allowance.
    let on_a = pa.distance_to_point(&pose(xa), rv(ours.point_a), true);
    let on_b = pb.distance_to_point(&pose(xb), rv(ours.point_b), true);
    let witness_ok = intersecting
        || (on_a <= 5e-5 && on_b <= 5e-5 && (ours.point_b - ours.point_a).length() >= ours.distance - 1e-5);
    let parry = parry_distance(pa, xa, pb, xb);
    let witness_gap = (ours.point_b - ours.point_a).length();
    let n = rv(if witness_gap > 1e-9 {
        (ours.point_b - ours.point_a) / witness_gap
    } else {
        ours.normal
    });
    let (Some(sa), Some(sb)) = (pa.as_support_map(), pb.as_support_map()) else {
        panic!("oracle shapes are support maps");
    };
    let slab = sb.support_point(&pose(xb), -n).dot(n) - sa.support_point(&pose(xa), n).dot(n);
    Gap {
        intersecting,
        slab,
        witness_ok,
        witness_gap,
        ours: ours.distance,
        parry,
        best: witness_gap.min(parry),
    }
}

/// The affine minimiser of `|sum a_i p_i|` subject to `sum a_i = 1`, over the points `pts`
/// (at most 4), by Gaussian elimination on the KKT system `[G 1; 1^T 0] [a; mu] = [0; 1]`.
fn affine_min_weights(pts: &[DVec3]) -> Option<Vec<f64>> {
    let n = pts.len();
    let m = n + 1;
    let mut a = vec![vec![0.0_f64; m + 1]; m];
    for i in 0..n {
        for j in 0..n {
            a[i][j] = pts[i].dot(pts[j]);
        }
        a[i][n] = 1.0;
        a[n][i] = 1.0;
    }
    a[n][m] = 1.0;
    for col in 0..m {
        let piv = (col..m).max_by(|&x, &y| a[x][col].abs().total_cmp(&a[y][col].abs()))?;
        if a[piv][col].abs() < 1e-18 {
            return None;
        }
        a.swap(col, piv);
        let pivot_row = a[col].clone();
        for (r, row) in a.iter_mut().enumerate() {
            if r != col {
                let f = row[col] / pivot_row[col];
                for (x, p) in row[col..].iter_mut().zip(&pivot_row[col..]) {
                    *x -= f * p;
                }
            }
        }
    }
    Some((0..n).map(|i| a[i][m] / a[i][i]).collect())
}

/// Exact distance from the origin to the convex hull of `w` in f64, by Wolfe's minimum-norm
/// point algorithm (P. Wolfe, "Finding the nearest point in a polytope", Mathematical
/// Programming 11, 1976): an active-set method with finite termination, independent of the
/// GJK code under test.
fn hull_origin_distance_f64(w: &[DVec3]) -> f64 {
    let scale = w.iter().fold(0.0_f64, |m, p| m.max(p.length_squared()));
    let mut set: Vec<usize> = vec![
        (0..w.len())
            .min_by(|&i, &j| w[i].length_squared().total_cmp(&w[j].length_squared()))
            .unwrap_or(0),
    ];
    let mut lambda = vec![1.0_f64];
    let mut x = w[set[0]];
    for _ in 0..1000 {
        let j = (0..w.len())
            .min_by(|&i, &k| w[i].dot(x).total_cmp(&w[k].dot(x)))
            .unwrap_or(0);
        if x.length_squared() - w[j].dot(x) <= 1e-14 * scale || set.contains(&j) {
            break;
        }
        set.push(j);
        lambda.push(0.0);
        loop {
            let pts: Vec<DVec3> = set.iter().map(|&i| w[i]).collect();
            let Some(alpha) = affine_min_weights(&pts) else {
                // Affinely dependent set: drop the newest point and stop this cycle.
                set.pop();
                lambda.pop();
                break;
            };
            if alpha.iter().all(|a| *a > 1e-12) {
                lambda = alpha;
                break;
            }
            let mut theta = 1.0_f64;
            for (l, a) in lambda.iter().zip(&alpha) {
                if *a <= 1e-12 && l - a > 0.0 {
                    theta = theta.min(l / (l - a));
                }
            }
            for (l, a) in lambda.iter_mut().zip(&alpha) {
                *l = (1.0 - theta) * *l + theta * a;
            }
            let mut k = 0;
            while k < set.len() {
                if lambda[k] <= 1e-12 {
                    set.remove(k);
                    lambda.remove(k);
                } else {
                    k += 1;
                }
            }
            if set.is_empty() {
                return 0.0;
            }
        }
        x = set
            .iter()
            .zip(&lambda)
            .fold(DVec3::ZERO, |acc, (&i, l)| acc + w[i] * *l);
        if set.len() == 4 || x.length_squared() <= 1e-20 * scale {
            // Origin inside a full-dimensional simplex of the hull, or on it.
            if x.length_squared() <= 1e-20 * scale || set.len() == 4 {
                return x.length();
            }
        }
    }
    x.length()
}

/// `[d, d]` for the exact hull distance (kept as bounds so callers read the same for every
/// exact family).
fn hull_origin_bounds_f64(w: &[DVec3], _done: impl Fn(f64, f64) -> bool) -> (f64, f64) {
    let d = hull_origin_distance_f64(w);
    (d, d)
}

#[test]
fn convex_distance_never_worse_than_parry_on_3000_random_pairs() {
    let mut v = Violations::default();
    let mut rng = Rng(0xD15_7A2C_E0FF_1CE5);
    let (mut separated, mut overlapping, mut parry_over) = (0, 0, 0);
    let (mut ours_over_max, mut parry_over_max) = (0.0_f32, 0.0_f32);
    let (mut fallback, mut fallback_max) = (0, 0.0_f32);
    for case in 0..3000 {
        let (a, pa) = random_shape(&mut rng);
        let (b, pb) = random_shape(&mut rng);
        let xa = Isometry::new(rng.rotation(), rng.unit_vec() * rng.range(0.0, 2.0));
        let xb = Isometry::new(rng.rotation(), rng.unit_vec() * rng.range(0.0, 2.0));
        let g = gap(&a, &*pa, xa, &b, &*pb, xb);
        if g.intersecting {
            overlapping += 1;
            check!(
                v,
                g.ours <= 1e-4,
                "case {case}: parry says intersecting, ours {}",
                g.ours
            );
            continue;
        }
        separated += 1;
        check!(
            v,
            g.witness_ok,
            "case {case}: witness points not on the shapes ({a:?} at {xa:?}, {b:?} at {xb:?})"
        );
        ours_over_max = ours_over_max.max(g.ours - g.parry);
        if g.witness_gap - g.ours > 1e-6 {
            fallback += 1;
            fallback_max = fallback_max.max(g.witness_gap - g.ours);
        }
        check!(
            v,
            g.ours <= g.parry + 2e-5,
            "case {case}: ours {} vs parry {} ({a:?} at {xa:?}, {b:?} at {xb:?})",
            g.ours,
            g.parry
        );
        if g.parry - g.ours > 1e-4 {
            parry_over += 1;
            parry_over_max = parry_over_max.max(g.parry - g.ours);
        }
    }
    eprintln!(
        "convex_distance: {separated} separated, {overlapping} overlapping; ours above parry by at \
         most {ours_over_max:e}; parry above ours (ours certified by its witnesses) by > 0.1 mm in \
         {parry_over} cases, max {parry_over_max:e}; lower-bound fallback (open duality gap) in \
         {fallback} cases, at most {fallback_max:e} below the witness gap"
    );
    v.finish("convex_distance vs parry");
    assert!(
        separated >= 500 && overlapping >= 300,
        "{separated} / {overlapping}"
    );
}

#[test]
fn hull_distance_matches_exact_f64_on_500_random_pairs() {
    let mut v = Violations::default();
    // For two point hulls the Minkowski difference is the hull of the pairwise differences,
    // so the exact distance is computable by brute force in f64.
    let mut rng = Rng(0x00F6_4E8A_C7D1_5A11);
    let mut max_err = 0.0_f64;
    for case in 0..500 {
        let cloud =
            |rng: &mut Rng| -> Vec<Vec3> { (0..12).map(|_| rng.unit_vec() * rng.range(0.05, 0.6)).collect() };
        let (pa, pb) = (cloud(&mut rng), cloud(&mut rng));
        let xa = Isometry::new(rng.rotation(), rng.unit_vec() * rng.range(0.0, 2.0));
        let xb = Isometry::new(rng.rotation(), rng.unit_vec() * rng.range(0.0, 2.0));
        let a = ConvexInput::ConvexHull { points: pa.clone() };
        let b = ConvexInput::ConvexHull { points: pb.clone() };
        let ours = match convex_distance(&a, xa, &b, xb) {
            Ok(d) => d,
            Err(e) => panic!("case {case}: {e:?}"),
        };
        let wa: Vec<DVec3> = pa.iter().map(|p| xa.transform_point(*p).as_dvec3()).collect();
        let wb: Vec<DVec3> = pb.iter().map(|p| xb.transform_point(*p).as_dvec3()).collect();
        let w: Vec<DVec3> = wa.iter().flat_map(|p| wb.iter().map(move |q| *p - *q)).collect();
        let d = f64::from(ours.distance);
        let (lo, hi) = hull_origin_bounds_f64(&w, |lo, hi| hi - lo <= 1e-7);
        let err = (d - lo).abs().max((d - hi).abs());
        max_err = max_err.max(err);
        check!(
            v,
            d >= lo - 2e-5 && d <= hi + 2e-5,
            "case {case}: ours {d} vs exact {lo}..{hi}"
        );
    }
    eprintln!("hull distance vs exact f64: max |error| {max_err:e}");
    v.finish("hull distance vs exact");
}

/// Determinism pin for the audit's High finding (audit-g-core-determinism § 1): a yawing sweep
/// whose first contact sits at the tolerance band. Shape A (a 0.6/0.5/0.2 cuboid) travels
/// through a static twin while yawing from identity to the audit's 90° quat; the reported `t`
/// shifts by several ulps when the sweep pose follows a different rounding of the slerp (the
/// pre-fix code on an SSE2-no-FMA host gives `t` bits 0x3e8d101e for this exact input; the
/// deterministic path gives 0x3e8d1021). With rotation interpolation deterministic, the variant
/// and `t` are bit-stable on every target.
#[test]
fn yawing_sweep_toi_variant_and_t_are_pinned() {
    let a = ConvexInput::cuboid(Vec3::new(0.6, 0.5, 0.2));
    let b = ConvexInput::cuboid(Vec3::new(0.6, 0.5, 0.2));
    let s = f32::from_bits(0x3f35_04f3);
    let yaw90 = Quat::from_xyzw(0.0, s, 0.0, s);
    let start = Isometry::new(Quat::IDENTITY, Vec3::new(-2.0, 0.0, 0.0));
    let end = Isometry::new(yaw90, Vec3::new(f32::from_bits(0x3f4c_ccb3), 0.0, 0.0));
    let result = time_of_impact(
        &a,
        start,
        end,
        &b,
        Isometry::IDENTITY,
        Isometry::IDENTITY,
        0.0,
        0.0,
        1000,
    );
    match result {
        ToiResult::Hit { t, .. } => {
            assert_eq!(
                t.to_bits(),
                0x3e8d_1021,
                "reported t must be the deterministic value"
            );
        }
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn translation_toi_agrees_with_parry_cast_shapes_on_2000_random_sweeps() {
    let mut v = Violations::default();
    let mut rng = Rng(0x7011_CA57_5EED_0042);
    let tol = 1e-4;
    let (mut both_hit, mut both_miss, mut band, mut parry_false_hit) = (0, 0, 0, 0);
    let mut max_disagreement = 0.0_f32;
    for case in 0..2000 {
        let (a, pa) = random_shape(&mut rng);
        let (b, pb) = random_shape(&mut rng);
        let dir = rng.unit_vec();
        let start = -dir * rng.range(2.0, 5.0);
        let side = rng.unit_vec() * rng.range(0.0, 1.2);
        let end = start + dir * start.length() * rng.range(0.6, 2.0) + side;
        let rot_a = rng.rotation();
        let xa0 = Isometry::new(rot_a, start);
        let xa1 = Isometry::new(rot_a, end);
        let xb = Isometry::new(rng.rotation(), Vec3::ZERO);
        if parry_intersecting(&*pa, xa0, &*pb, xb) {
            continue;
        }
        let gap_at = |s: f32| gap(&a, &*pa, xa0.interpolate(&xa1, s), &b, &*pb, xb);
        let ours = time_of_impact(&a, xa0, xa1, &b, xb, xb, 0.0, tol, 256);
        let vel = end - start;
        let options = ShapeCastOptions {
            max_time_of_impact: 1.0,
            target_distance: 0.0,
            stop_at_penetration: true,
            compute_impact_geometry_on_penetration: true,
        };
        let truth = match query::cast_shapes(
            &pose(xa0),
            rv(vel),
            &*pa,
            &pose(xb),
            rapier3d::math::Vector::ZERO,
            &*pb,
            options,
        ) {
            Ok(hit) => hit.map(|h| h.time_of_impact),
            Err(e) => panic!("case {case}: parry cast unsupported: {e:?}"),
        };
        let horizon = match (ours, truth) {
            (ToiResult::Hit { t, .. }, Some(tp)) => {
                both_hit += 1;
                let g = gap_at(t);
                check!(
                    v,
                    !g.penetrates(1e-3) && g.best <= tol + 2e-5,
                    "case {case}: at our t {t}: intersecting {} slab {} gap {}",
                    g.intersecting,
                    g.slab,
                    g.best
                );
                max_disagreement = max_disagreement.max(((tp - t) * vel.length()).abs());
                t
            }
            (ToiResult::Separated, None) => {
                both_miss += 1;
                1.0
            }
            (ToiResult::Separated, Some(tp)) => {
                let g = gap_at(tp);
                check!(
                    v,
                    !g.penetrates(1e-3) && g.best > tol,
                    "case {case}: parry hit at {tp} (gap {}) but ours Separated",
                    g.best
                );
                parry_false_hit += 1;
                1.0
            }
            (ToiResult::Hit { t, .. }, None) => {
                band += 1;
                let g = gap_at(t);
                check!(
                    v,
                    !g.penetrates(1e-3) && g.best <= tol + 2e-5,
                    "case {case}: hit at {t} with gap {}, parry sees no contact",
                    g.best
                );
                t
            }
            (other, truth) => panic!("case {case}: ours {other:?}, parry {truth:?}"),
        };
        // No contact anywhere before the horizon (sampled).
        if let Some(s) = (0..100)
            .map(|k| horizon * k as f32 / 100.0)
            .find(|s| gap_at(*s).penetrates(1e-3))
        {
            v.record(format!("case {case}: contact at {s} < {horizon}"));
        }
    }
    eprintln!(
        "toi vs parry cast_shapes: {both_hit} both hit, {both_miss} both miss, {band} near-miss \
         within tolerance, {parry_false_hit} parry hits refuted; max contact disagreement along the \
         path {max_disagreement:e} m"
    );
    v.finish("translation toi vs parry");
    assert!(both_hit >= 300 && both_miss >= 300, "{both_hit} / {both_miss}");
}

#[test]
fn rotating_sweeps_have_no_contact_before_t_and_separated_means_apart() {
    let mut v = Violations::default();
    // Both shapes translate and rotate (endpoint orientations up to ~170 degrees apart).
    // Every sweep is sampled densely: before a reported hit, and over the whole interval of a
    // `Separated` sweep, the shapes must not intersect and the gap must stay at or above
    // target_gap (within the two distance queries' accuracy). Sampling can miss a contact
    // shorter than one sample, so this is a necessary check, not a proof.
    let mut rng = Rng(0x0BAD_5EED_C0FF_EE11);
    let (target_gap, tol) = (0.01_f32, 1e-3_f32);
    let (mut hits, mut misses, mut overlapped_start) = (0, 0, 0);
    let mut min_margin = f32::INFINITY;
    for case in 0..400 {
        let (a, pa) = random_shape(&mut rng);
        let (b, pb) = random_shape(&mut rng);
        let dir = rng.unit_vec();
        let start = -dir * rng.range(1.0, 3.0);
        let end = start + dir * start.length() * rng.range(0.5, 2.0) + rng.unit_vec() * 0.8;
        let ra0 = rng.rotation();
        let ra1 = ra0 * Quat::from_axis_angle(rng.unit_vec(), rng.range(0.0, 3.0));
        let rb0 = rng.rotation();
        let rb1 = rb0 * Quat::from_axis_angle(rng.unit_vec(), rng.range(0.0, 3.0));
        let (xa0, xa1) = (Isometry::new(ra0, start), Isometry::new(ra1, end));
        let (xb0, xb1) = (Isometry::new(rb0, Vec3::ZERO), Isometry::new(rb1, Vec3::ZERO));
        let result = time_of_impact(&a, xa0, xa1, &b, xb0, xb1, target_gap, tol, 1000);
        let gap_at = |s: f32| {
            gap(
                &a,
                &*pa,
                xa0.interpolate(&xa1, s),
                &b,
                &*pb,
                xb0.interpolate(&xb1, s),
            )
        };
        let horizon = match result {
            ToiResult::Hit { t, .. } => {
                hits += 1;
                let g = gap_at(t);
                check!(
                    v,
                    !g.penetrates(1e-3)
                        && g.slab.max(g.best.min(g.parry)) >= target_gap - 1e-4
                        && g.best <= target_gap + tol + 2e-5,
                    "case {case}: gap {} at reported t {t}",
                    g.best
                );
                t
            }
            ToiResult::Separated => {
                misses += 1;
                1.0
            }
            ToiResult::OverlappedAtStart => {
                check!(
                    v,
                    gap_at(0.0).intersecting,
                    "case {case}: OverlappedAtStart but apart"
                );
                overlapped_start += 1;
                continue;
            }
            ToiResult::Failed(e) => panic!("case {case}: failed {e:?}"),
        };
        // One violation per case at most (the first bad sample).
        for k in 0..400 {
            let s = horizon * k as f32 / 400.0;
            let g = gap_at(s);
            min_margin = min_margin.min(g.best - target_gap);
            if g.penetrates(1e-3) || g.best < target_gap - 1e-4 {
                v.record(format!(
                    "case {case}: gap {} at s {s} before the reported horizon {horizon} ({result:?})",
                    g.best
                ));
                break;
            }
        }
    }
    eprintln!(
        "rotating sweeps: {hits} hits, {misses} separated, {overlapped_start} overlapped at start; \
         smallest sampled gap - target before the horizon {min_margin:e}"
    );
    v.finish("rotating sweeps");
    assert!(hits >= 50 && misses >= 50, "{hits} / {misses}");
}

/// Exact (f64) distance between two segments `p0..p1` and `q0..q1`: minimises over the
/// first segment's parameter with the second's clamped, plus the endpoint cases (the
/// distance between segments is convex in the parameters, so a fine golden-section search on
/// the clamped one-dimensional function converges to the minimum).
fn segment_distance_f64(p0: DVec3, p1: DVec3, q0: DVec3, q1: DVec3) -> f64 {
    let d2 = q1 - q0;
    let len2 = d2.length_squared();
    let at = |s: f64| -> f64 {
        let p = p0 + (p1 - p0) * s;
        let t = if len2 > 0.0 {
            ((p - q0).dot(d2) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (p - (q0 + d2 * t)).length()
    };
    let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
    let g = 0.5 * (5.0_f64.sqrt() - 1.0);
    for _ in 0..200 {
        let m1 = hi - g * (hi - lo);
        let m2 = lo + g * (hi - lo);
        if at(m1) <= at(m2) {
            hi = m2;
        } else {
            lo = m1;
        }
    }
    at(0.5 * (lo + hi)).min(at(0.0)).min(at(1.0))
}

/// A shape with an exact f64 distance: a point hull, or a capsule (a sphere when the
/// half-height is 0) along Y.
enum Exact {
    Hull(Vec<Vec3>),
    Rounded { half_height: f32, radius: f32 },
}

impl Exact {
    fn input(&self) -> ConvexInput {
        match self {
            Exact::Hull(points) => ConvexInput::ConvexHull {
                points: points.clone(),
            },
            Exact::Rounded { half_height, radius } => ConvexInput::Capsule {
                half_height: *half_height,
                radius: *radius,
                axis: ShapeAxis::Y,
            },
        }
    }
}

/// Bounds `[lo, hi]` on the exact distance between two exact shapes of the same family
/// (0 when overlapping); `done(lo, hi)` lets the hull iteration stop once the caller's
/// question is settled. Capsule pairs are exact (`lo == hi`).
fn exact_bounds(
    a: &Exact,
    xa: Isometry,
    b: &Exact,
    xb: Isometry,
    done: impl Fn(f64, f64) -> bool,
) -> (f64, f64) {
    let place = |xf: Isometry, p: Vec3| xf.transform_point(p).as_dvec3();
    match (a, b) {
        (Exact::Hull(pa), Exact::Hull(pb)) => {
            let w: Vec<DVec3> = pa
                .iter()
                .flat_map(|p| pb.iter().map(move |q| place(xa, *p) - place(xb, *q)))
                .collect();
            hull_origin_bounds_f64(&w, done)
        }
        (
            Exact::Rounded {
                half_height: ha,
                radius: ra,
            },
            Exact::Rounded {
                half_height: hb,
                radius: rb,
            },
        ) => {
            let seg = |xf: Isometry, h: f32| (place(xf, Vec3::Y * h), place(xf, Vec3::Y * -h));
            let ((p0, p1), (q0, q1)) = (seg(xa, *ha), seg(xb, *hb));
            let e = (segment_distance_f64(p0, p1, q0, q1) - f64::from(*ra) - f64::from(*rb)).max(0.0);
            (e, e)
        }
        _ => unreachable!("same family only"),
    }
}

#[test]
fn toi_is_exact_to_1e5_against_f64_distance_for_hulls_and_capsules() {
    // Rotating and translating sweeps of shapes whose distance is exactly computable in f64.
    // For every reported hit the exact gap must be in [target, target + tol] (within 1e-5,
    // the f32 resolution of the poses), and 200 samples before the hit (or over the whole
    // interval of a Separated sweep) must stay at or above target_gap - 1e-5.
    let mut rng = Rng(0xE8AC_7F64_0D15_7A2C);
    let mut v = Violations::default();
    let tol = 1e-4_f32;
    let (mut hits, mut misses, mut overlapped_start) = (0, 0, 0);
    let mut worst_hit = 0.0_f64;
    let mut undecided = 0;
    for case in 0..600 {
        let make = |rng: &mut Rng| -> Exact {
            if case % 2 == 0 {
                Exact::Hull((0..10).map(|_| rng.unit_vec() * rng.range(0.05, 0.6)).collect())
            } else {
                let hh = if rng.next_u64().is_multiple_of(3) {
                    0.0
                } else {
                    rng.range(0.0, 0.6)
                };
                Exact::Rounded {
                    half_height: hh,
                    radius: rng.range(0.02, 0.4),
                }
            }
        };
        let (ea, eb) = (make(&mut rng), make(&mut rng));
        let target_gap = if rng.next_u64().is_multiple_of(2) {
            0.0
        } else {
            0.01
        };
        let dir = rng.unit_vec();
        let start = -dir * rng.range(1.0, 3.0);
        let end = start + dir * start.length() * rng.range(0.5, 2.0) + rng.unit_vec() * 0.6;
        let ra0 = rng.rotation();
        let ra1 = ra0 * Quat::from_axis_angle(rng.unit_vec(), rng.range(0.0, 2.5));
        let rb0 = rng.rotation();
        let rb1 = rb0 * Quat::from_axis_angle(rng.unit_vec(), rng.range(0.0, 2.5));
        let (xa0, xa1) = (Isometry::new(ra0, start), Isometry::new(ra1, end));
        let (xb0, xb1) = (Isometry::new(rb0, Vec3::ZERO), Isometry::new(rb1, Vec3::ZERO));
        let bounds_at = |s: f32, done: &dyn Fn(f64, f64) -> bool| {
            exact_bounds(&ea, xa0.interpolate(&xa1, s), &eb, xb0.interpolate(&xb1, s), done)
        };
        let result = time_of_impact(
            &ea.input(),
            xa0,
            xa1,
            &eb.input(),
            xb0,
            xb1,
            target_gap,
            tol,
            1000,
        );
        let tg = f64::from(target_gap);
        let (band_lo, band_hi) = (tg - 1e-5, tg + f64::from(tol) + 1e-5);
        let horizon = match result {
            ToiResult::Hit { t, .. } => {
                hits += 1;
                let (lo, hi) = bounds_at(t, &|lo, hi| {
                    (lo >= band_lo && hi <= band_hi) || hi < band_lo || lo > band_hi
                });
                worst_hit = worst_hit.max((hi - tg - f64::from(tol) / 2.0).abs());
                check!(
                    v,
                    lo >= band_lo && hi <= band_hi,
                    "case {case}: exact gap {lo}..{hi} at reported t {t} (target {target_gap})"
                );
                t
            }
            ToiResult::Separated => {
                misses += 1;
                1.0
            }
            ToiResult::OverlappedAtStart => {
                overlapped_start += 1;
                let (lo, _) = bounds_at(0.0, &|lo, hi| lo > 1e-5 || hi <= 1e-5);
                check!(v, lo <= 1e-5, "case {case}: OverlappedAtStart, exact gap >= {lo}");
                continue;
            }
            ToiResult::Failed(e) => {
                v.record(format!("case {case}: failed {e:?}"));
                continue;
            }
        };
        for k in 0..200 {
            let s = horizon * k as f32 / 200.0;
            let (lo, hi) = bounds_at(s, &|lo, hi| lo >= band_lo || hi < band_lo);
            if hi < band_lo {
                v.record(format!(
                    "case {case}: exact gap <= {hi} at {s} before horizon {horizon} ({result:?})"
                ));
                break;
            }
            if lo < band_lo {
                undecided += 1;
            }
        }
    }
    eprintln!(
        "toi vs exact f64: {hits} hits, {misses} separated, {overlapped_start} overlapped at start; \
         worst |exact gap - (target + tol/2)| at a hit {worst_hit:e}; {undecided} samples the \
         f64 bounds could not settle"
    );
    v.finish("toi vs exact f64");
    assert!(hits >= 100 && misses >= 50, "{hits} / {misses}");
}

#[test]
fn motion_toi_is_exact_to_1e5_against_f64_distance_for_multi_turn_spins() {
    // ADR-2423: the angular sweep along constant-velocity rigid motions, refereed by exact f64
    // distances (not by the module's own GJK). Spins up to two whole turns per interval about
    // off-origin centres, so the path leaves any endpoint slerp. Same contract as the endpoint
    // test above: the exact gap at a hit is in the band, and no sample before the hit (or over
    // a Separated interval) is below target_gap - 1e-5.
    let mut rng = Rng(0x2423_A4C1_D00D_5EED);
    let mut v = Violations::default();
    let tol = 1e-4_f32;
    let (mut hits, mut misses) = (0, 0);
    for case in 0..400 {
        let make = |rng: &mut Rng| -> Exact {
            if case % 2 == 0 {
                Exact::Hull((0..10).map(|_| rng.unit_vec() * rng.range(0.05, 0.6)).collect())
            } else {
                Exact::Rounded {
                    half_height: rng.range(0.0, 0.7),
                    radius: rng.range(0.02, 0.3),
                }
            }
        };
        let (ea, eb) = (make(&mut rng), make(&mut rng));
        let target_gap = if rng.next_u64().is_multiple_of(2) {
            0.0
        } else {
            0.01
        };
        let dir = rng.unit_vec();
        let start = -dir * rng.range(1.6, 3.0);
        let ma = RigidMotion {
            start: Isometry::new(rng.rotation(), start),
            local_center: rng.unit_vec() * rng.range(0.0, 0.2),
            linear: dir * start.length() * rng.range(0.3, 1.5) + rng.unit_vec() * 0.5,
            angular: rng.unit_vec() * rng.range(0.0, 4.0 * core::f32::consts::PI),
        };
        let mb = RigidMotion {
            start: Isometry::new(rng.rotation(), Vec3::ZERO),
            local_center: rng.unit_vec() * rng.range(0.0, 0.1),
            linear: Vec3::ZERO,
            angular: rng.unit_vec() * rng.range(0.0, 2.0 * core::f32::consts::PI),
        };
        let bounds_at = |s: f32, done: &dyn Fn(f64, f64) -> bool| {
            exact_bounds(&ea, ma.pose_at(s), &eb, mb.pose_at(s), done)
        };
        // Starting inside the other shape is not this test's subject.
        let (lo0, _) = bounds_at(0.0, &|lo, hi| lo > 0.05 || hi <= 0.05);
        if lo0 <= 0.05 {
            continue;
        }
        let options = ToiOptions {
            target_gap,
            tolerance: tol,
            max_iterations: 2000,
        };
        let result = time_of_impact_motion(&ea.input(), &ma, &eb.input(), &mb, options);
        let tg = f64::from(target_gap);
        let (band_lo, band_hi) = (tg - 1e-5, tg + f64::from(tol) + 1e-5);
        let horizon = match result {
            ToiResult::Hit { t, .. } => {
                hits += 1;
                let (lo, hi) = bounds_at(t, &|lo, hi| {
                    (lo >= band_lo && hi <= band_hi) || hi < band_lo || lo > band_hi
                });
                check!(
                    v,
                    lo >= band_lo && hi <= band_hi,
                    "case {case}: exact gap {lo}..{hi} at reported t {t} (target {target_gap})"
                );
                t
            }
            ToiResult::Separated => {
                misses += 1;
                1.0
            }
            other => {
                v.record(format!("case {case}: unexpected {other:?}"));
                continue;
            }
        };
        for k in 0..200 {
            let s = horizon * k as f32 / 200.0;
            let (_, hi) = bounds_at(s, &|lo, hi| lo >= band_lo || hi < band_lo);
            if hi < band_lo {
                v.record(format!(
                    "case {case}: exact gap <= {hi} at {s} before horizon {horizon} ({result:?})"
                ));
                break;
            }
        }
    }
    eprintln!("motion toi vs exact f64: {hits} hits, {misses} separated");
    v.finish("motion toi vs exact f64");
    assert!(hits >= 80 && misses >= 30, "{hits} / {misses}");
}
