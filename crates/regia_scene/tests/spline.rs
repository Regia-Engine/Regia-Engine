// SPDX-License-Identifier: MPL-2.0
//! Spec 64 § 9 tests 1, 2, 3 and 5: spline evaluation, arc-length inversion, rotation-minimizing
//! frames and closest-point queries. Pure CPU, fixed inputs.

use regia_core::math::Vec3;
use regia_scene::spline::{Spline, SplineCurve, SplinePoint};

/// A small deterministic generator (64-bit LCG, top bits) so the test needs no RNG crate.
struct Lcg(u64);

impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / (1u64 << 24) as f32
    }
}

fn s_curve() -> Spline {
    // About 100 m long, two Free segments whose tangents are as long as the segments.
    Spline {
        points: vec![
            SplinePoint::free(Vec3::ZERO, Vec3::new(40.0, 0.0, 40.0), Vec3::new(40.0, 0.0, 40.0)),
            SplinePoint::free(
                Vec3::new(50.0, 2.0, 0.0),
                Vec3::new(45.0, 0.0, -45.0),
                Vec3::new(45.0, 0.0, -45.0),
            ),
            SplinePoint::free(
                Vec3::new(95.0, 0.0, 5.0),
                Vec3::new(40.0, 0.0, 40.0),
                Vec3::new(40.0, 0.0, 40.0),
            ),
        ],
        ..Spline::default()
    }
}

/// Cumulative chord length over a dense key grid: `(keys, lengths)`.
fn dense_lengths(c: &SplineCurve, per_segment: usize) -> (Vec<f64>, Vec<f64>) {
    let n = c.segment_count() * per_segment;
    let mut keys = Vec::with_capacity(n + 1);
    let mut lens = Vec::with_capacity(n + 1);
    let mut prev = c.at_key(0.0).position;
    let mut acc = 0.0f64;
    for k in 0..=n {
        let key = k as f64 / per_segment as f64;
        let p = c.at_key(key as f32).position;
        acc += f64::from((p - prev).length());
        prev = p;
        keys.push(key);
        lens.push(acc);
    }
    (keys, lens)
}

#[test]
fn straight_spline_and_quarter_circle() {
    let s = Spline::through([Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 6.0, 3.0)]);
    assert!((s.length() - 5.0).abs() < 1e-5);
    let mid = s.at_param(0.5).position;
    assert!(mid.abs_diff_eq(Vec3::new(2.5, 4.0, 3.0), 1e-5), "{mid:?}");

    for r in [1.0f32, 10.0, 250.0] {
        let k = 1.6569 * r;
        let s = Spline {
            points: vec![
                SplinePoint::free(
                    Vec3::new(r, 0.0, 0.0),
                    Vec3::new(0.0, 0.0, k),
                    Vec3::new(0.0, 0.0, k),
                ),
                SplinePoint::free(
                    Vec3::new(0.0, 0.0, r),
                    Vec3::new(-k, 0.0, 0.0),
                    Vec3::new(-k, 0.0, 0.0),
                ),
            ],
            ..Spline::default()
        };
        let expected = std::f32::consts::FRAC_PI_2 * r;
        let rel = (s.length() - expected).abs() / expected;
        assert!(rel < 0.001, "r {r}: length {} vs {expected}", s.length());
    }
}

#[test]
fn arc_length_inversion_within_a_millimeter() {
    let c = s_curve().curve();
    let total = f64::from(c.length());
    assert!((90.0..140.0).contains(&total), "{total}");
    let (keys, lens) = dense_lengths(&c, 200_000);
    let mut worst = 0.0f64;
    for i in 0..1000 {
        let d = total * (i as f64 + 0.5) / 1000.0;
        let sample = c.at_distance_f64(d);
        // True length to the returned key, from the dense table.
        let k = f64::from(sample.key);
        let j = keys.partition_point(|&x| x <= k).clamp(1, keys.len() - 1);
        let f = (k - keys[j - 1]) / (keys[j] - keys[j - 1]);
        let true_len = lens[j - 1] + (lens[j] - lens[j - 1]) * f;
        worst = worst.max((true_len - d).abs());
        assert!((f64::from(sample.along_m) - d).abs() < 1e-3);
    }
    assert!(worst <= 1.0e-3, "worst inversion error {worst} m");
}

/// Signed angle from `a` to `b` about `axis`.
fn angle_about(a: Vec3, b: Vec3, axis: Vec3) -> f32 {
    a.cross(b).dot(axis).atan2(a.dot(b))
}

#[test]
fn rotation_minimizing_frames_on_a_helix_and_a_closed_loop() {
    // Helix (r cos θ, c θ, r sin θ): torsion c / (r² + c²); relative to the Frenet frame the
    // rotation-minimizing frame turns by the torsion per unit length.
    let (r, c) = (5.0f32, 1.0f32);
    let step = std::f32::consts::PI / 12.0;
    let turns = 2.0;
    let n = (turns * 2.0 * std::f32::consts::PI / step).round() as usize;
    let point = |th: f32| Vec3::new(r * th.cos(), c * th, r * th.sin());
    let deriv = |th: f32| Vec3::new(-r * th.sin(), c, r * th.cos()) * step;
    let s = Spline {
        points: (0..=n)
            .map(|i| {
                let th = i as f32 * step;
                SplinePoint::free(point(th), deriv(th), deriv(th))
            })
            .collect(),
        ..Spline::default()
    };
    let curve = s.curve();
    let speed = (r * r + c * c).sqrt();
    let tau = c / (r * r + c * c);
    // Frenet normal points at the axis.
    let frenet = |th: f32| Vec3::new(-th.cos(), 0.0, -th.sin());
    let mut total = 0.0f32;
    let mut prev: Option<f32> = None;
    let len = curve.length();
    let steps = 400;
    for k in 0..=steps {
        let d = len * k as f32 / steps as f32;
        let sm = curve.at_distance(d);
        let th = d / speed;
        let phi = angle_about(frenet(th), sm.up, sm.tangent);
        if let Some(p) = prev {
            let mut dp = phi - p;
            if dp > std::f32::consts::PI {
                dp -= 2.0 * std::f32::consts::PI;
            }
            if dp < -std::f32::consts::PI {
                dp += 2.0 * std::f32::consts::PI;
            }
            total += dp;
        }
        prev = Some(phi);
    }
    let expected = tau * len;
    assert!(
        (total.abs() - expected).abs().to_degrees() < 0.5,
        "twist {} vs analytic {} (degrees)",
        total.abs().to_degrees(),
        expected.to_degrees()
    );

    // A closed, non-planar loop: the end frame equals the start frame.
    let loop_pts: Vec<Vec3> = (0..8)
        .map(|i| {
            let t = i as f32 * std::f32::consts::TAU / 8.0;
            Vec3::new(10.0 * t.cos(), 4.0 * (2.0 * t).sin(), 10.0 * t.sin())
        })
        .collect();
    let lp = Spline::through(loop_pts).closed().curve();
    let a = lp.at_distance(0.0);
    let b = lp.at_distance(lp.length());
    let ang = a.up.dot(b.up).clamp(-1.0, 1.0).acos().to_degrees();
    assert!(ang < 0.1, "closed loop frame mismatch {ang}°");
    assert!(a.position.abs_diff_eq(b.position, 1e-4));
    // Frames stay orthonormal.
    for k in 0..50 {
        let sm = lp.at_param(k as f32 / 50.0);
        assert!(sm.tangent.dot(sm.up).abs() < 1e-4 && (sm.up.length() - 1.0).abs() < 1e-4);
        assert!(sm.right.abs_diff_eq(sm.tangent.cross(sm.up), 1e-4));
    }
}

#[test]
fn closest_beats_dense_sampling() {
    let c = s_curve().curve();
    let dense: Vec<Vec3> = (0..10_000)
        .map(|k| c.at_param(k as f32 / 9_999.0).position)
        .collect();
    let (lo, hi) = c.bounds();
    let mut rng = Lcg(0x5eed_0064);
    for _ in 0..1000 {
        let p = Vec3::new(
            lo.x - 5.0 + rng.next_f32() * (hi.x - lo.x + 10.0),
            lo.y - 5.0 + rng.next_f32() * (hi.y - lo.y + 10.0),
            lo.z - 5.0 + rng.next_f32() * (hi.z - lo.z + 10.0),
        );
        let hit = c.closest(p);
        let best = dense.iter().map(|q| q.distance(p)).fold(f32::INFINITY, f32::min);
        assert!(
            hit.distance_m <= best + 1.0e-3,
            "{p:?}: {} vs dense {best}",
            hit.distance_m
        );
        assert!((hit.position.distance(p) - hit.distance_m).abs() < 1e-3);
        let back = c.at_key(hit.key).position;
        assert!(back.abs_diff_eq(hit.position, 1e-3));
    }
}

#[test]
fn cuts_keep_the_shape_exactly() {
    let mut s = Spline::through([
        Vec3::ZERO,
        Vec3::new(10.0, 1.0, 5.0),
        Vec3::new(20.0, 0.0, -5.0),
        Vec3::new(30.0, 2.0, 0.0),
    ]);
    s.points[1].mode = regia_scene::spline::TangentMode::Linear;
    let c = s.curve();
    let len = f64::from(c.length());
    let cuts = [3.3, 12.0, len * 0.5, len - 1.0];
    let whole = c.with_points_at(&cuts).curve();
    assert_eq!(whole.segment_count(), c.segment_count() + cuts.len());
    assert!((f64::from(whole.length()) - len).abs() < 1e-3);
    for k in 0..=200 {
        let d = len * k as f64 / 200.0;
        let (a, b) = (c.at_distance_f64(d).position, whole.at_distance_f64(d).position);
        assert!(a.abs_diff_eq(b, 2e-3), "{d}: {a:?} vs {b:?}");
    }
    let pieces = c.split_at(&cuts);
    assert_eq!(pieces.len(), cuts.len() + 1);
    let total: f32 = pieces.iter().map(Spline::length).sum();
    assert!((f64::from(total) - len).abs() < 1e-3);
    // Each piece starts where the previous ended, with the same tangent direction.
    for w in pieces.windows(2) {
        let (a, b) = (w[0].at_param(1.0), w[1].at_param(0.0));
        assert!(a.position.abs_diff_eq(b.position, 1e-4));
        assert!(a.tangent.abs_diff_eq(b.tangent, 1e-3));
    }
    // A closed spline cut twice gives two pieces, one across the seam.
    let ring = Spline::through([
        Vec3::ZERO,
        Vec3::new(10.0, 0.0, 0.0),
        Vec3::new(10.0, 0.0, 10.0),
        Vec3::new(0.0, 0.0, 10.0),
    ])
    .closed()
    .curve();
    let l = f64::from(ring.length());
    let halves = ring.split_at(&[l * 0.25, l * 0.75]);
    assert_eq!(halves.len(), 2);
    assert!((f64::from(halves[1].length()) - l * 0.5).abs() < 1e-3);
    let reclosed = ring.with_points_at(&[l * 0.1]);
    assert!(reclosed.closed && reclosed.points.len() == 5);
    assert!((f64::from(reclosed.length()) - l).abs() < 1e-3);
}

/// Spec 64 § 2.1 `Aligned`: the incoming tangent takes the outgoing direction at the stored
/// incoming length, so the curve equals a `Free` point with that tangent.
#[test]
fn aligned_points_take_the_outgoing_direction_at_the_incoming_length() {
    use regia_scene::spline::TangentMode;
    let mid = |mode: TangentMode, tin: Vec3| {
        SplinePoint::free(Vec3::new(10.0, 0.0, 0.0), tin, Vec3::new(6.0, 0.0, 3.0)).with_mode(mode)
    };
    let mk = |p: SplinePoint| {
        let mut s = Spline::through([Vec3::ZERO, Vec3::ZERO, Vec3::new(20.0, 0.0, 4.0)]);
        s.points[1] = p;
        s
    };
    // The stored incoming tangent points the wrong way; only its length (5) counts.
    let aligned = mk(mid(TangentMode::Aligned, Vec3::new(0.0, 0.0, -5.0)));
    let dir = Vec3::new(6.0, 0.0, 3.0).normalize();
    let free = mk(mid(TangentMode::Free, dir * 5.0));
    assert!((aligned.length() - free.length()).abs() < 1e-4);
    for k in 0..=40 {
        let key = k as f32 / 20.0;
        let (a, b) = (aligned.at_key(key).position, free.at_key(key).position);
        assert!(a.abs_diff_eq(b, 1e-4), "key {key}: {a:?} vs {b:?}");
    }
    // And it differs from `Free` with the stored (wrong) incoming tangent: the mode matters.
    let raw = mk(mid(TangentMode::Free, Vec3::new(0.0, 0.0, -5.0)));
    assert!(raw.at_key(0.5).position.distance(aligned.at_key(0.5).position) > 0.1);
}

/// Spec 64 § 2.2 `WorldY`: the frame keeps world up on a sloped path and falls back to the
/// rotation-minimizing frame where the tangent is within 1° of vertical.
#[test]
fn world_y_frames_keep_up_and_fall_back_on_vertical_segments() {
    use regia_scene::spline::{SplineUp, TangentMode};
    let lin = |s: &mut Spline| {
        for p in &mut s.points {
            p.mode = TangentMode::Linear;
        }
    };
    // A 30° ramp: WorldY up is world Y projected off the tangent, so its x is negative and it
    // has no sideways (z) part.
    let mut ramp = Spline::through([Vec3::ZERO, Vec3::new(10.0, 10.0 * 30f32.to_radians().tan(), 0.0)]);
    lin(&mut ramp);
    ramp.up = SplineUp::WorldY;
    let s = ramp.at_param(0.5);
    let t = s.tangent;
    let want = (Vec3::Y - t * t.dot(Vec3::Y)).normalize();
    assert!(s.up.abs_diff_eq(want, 1e-4), "{:?} vs {want:?}", s.up);
    assert!(s.right.dot(Vec3::Y).abs() < 1e-4, "right stays horizontal");
    // A vertical shaft with a kink (tangent exactly vertical on the first segment): WorldY
    // equals the rotation-minimizing frame there.
    let pts = [Vec3::ZERO, Vec3::new(0.0, 10.0, 0.0), Vec3::new(5.0, 12.0, 1.0)];
    let mut wy = Spline::through(pts);
    lin(&mut wy);
    wy.up = SplineUp::WorldY;
    let mut rm = Spline::through(pts);
    lin(&mut rm);
    rm.up = SplineUp::RotationMinimizing;
    for k in 0..=10 {
        let key = k as f32 / 10.0 * 0.999;
        let (a, b) = (wy.at_key(key), rm.at_key(key));
        assert!(a.tangent.dot(Vec3::Y) > 0.9998, "segment 0 is vertical");
        assert!(
            a.up.abs_diff_eq(b.up, 1e-4),
            "key {key}: {:?} vs {:?}",
            a.up,
            b.up
        );
        assert!(a.up.is_finite() && (a.up.length() - 1.0).abs() < 1e-4);
    }
}
