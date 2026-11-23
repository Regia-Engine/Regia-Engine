// SPDX-License-Identifier: MPL-2.0
//! Independent world-metric checks for spline arc length under affine transforms.
//!
//! The reference uses f64 Hermite evaluation and composite Simpson integration rather than the
//! spline evaluator's arc table and Newton inversion.

use regia_core::math::{Affine3A, Quat, Vec3};
use regia_scene::spline::{Spline, SplinePoint};

#[derive(Clone, Copy)]
struct Cubic {
    p0: [f64; 3],
    m0: [f64; 3],
    p1: [f64; 3],
    m1: [f64; 3],
}

impl Cubic {
    fn eval(self, u: f64) -> ([f64; 3], [f64; 3]) {
        let (u2, u3) = (u * u, u * u * u);
        let h00 = 2.0 * u3 - 3.0 * u2 + 1.0;
        let h10 = u3 - 2.0 * u2 + u;
        let h01 = -2.0 * u3 + 3.0 * u2;
        let h11 = u3 - u2;
        let d00 = 6.0 * u2 - 6.0 * u;
        let d10 = 3.0 * u2 - 4.0 * u + 1.0;
        let d01 = -6.0 * u2 + 6.0 * u;
        let d11 = 3.0 * u2 - 2.0 * u;
        let mut p = [0.0; 3];
        let mut d = [0.0; 3];
        for (axis, (position, derivative)) in p.iter_mut().zip(&mut d).enumerate() {
            *position = h00 * self.p0[axis] + h10 * self.m0[axis] + h01 * self.p1[axis] + h11 * self.m1[axis];
            *derivative =
                d00 * self.p0[axis] + d10 * self.m0[axis] + d01 * self.p1[axis] + d11 * self.m1[axis];
        }
        (p, d)
    }

    fn speed(self, u: f64) -> f64 {
        let d = self.eval(u).1;
        d.iter().map(|x| x * x).sum::<f64>().sqrt()
    }

    fn simpson_length(self, intervals: usize) -> f64 {
        assert!(intervals.is_multiple_of(2));
        let h = 1.0 / intervals as f64;
        let mut sum = self.speed(0.0) + self.speed(1.0);
        for i in 1..intervals {
            sum += if i.is_multiple_of(2) { 2.0 } else { 4.0 } * self.speed(i as f64 * h);
        }
        sum * h / 3.0
    }

    fn distance_table(self, intervals: usize) -> Vec<f64> {
        let h = 1.0 / intervals as f64;
        let mut cumulative = Vec::with_capacity(intervals + 1);
        cumulative.push(0.0);
        let mut total = 0.0;
        for i in 0..intervals {
            let a = i as f64 * h;
            let b = (i + 1) as f64 * h;
            total += (b - a) * (self.speed(a) + 4.0 * self.speed((a + b) * 0.5) + self.speed(b)) / 6.0;
            cumulative.push(total);
        }
        cumulative
    }

    fn at_fraction(self, table: &[f64], fraction: f64) -> ([f64; 3], [f64; 3]) {
        let distance = table[table.len() - 1] * fraction;
        let i = table.partition_point(|&d| d < distance).clamp(1, table.len() - 1);
        let blend = (distance - table[i - 1]) / (table[i] - table[i - 1]);
        self.eval((i as f64 - 1.0 + blend) / (table.len() - 1) as f64)
    }
}

fn local_free_controls(spline: &Spline, segment: usize) -> [Vec3; 4] {
    let next = if segment + 1 == spline.points.len() {
        0
    } else {
        segment + 1
    };
    [
        spline.points[segment].position,
        spline.points[segment].tangent_out,
        spline.points[next].position,
        spline.points[next].tangent_in,
    ]
}

fn transformed_cubic(local: [Vec3; 4], transform: &Affine3A) -> Cubic {
    let p0 = transform.transform_point3(local[0]);
    let m0 = transform.transform_vector3(local[1]);
    let p1 = transform.transform_point3(local[2]);
    let m1 = transform.transform_vector3(local[3]);
    let arr = |v: Vec3| [f64::from(v.x), f64::from(v.y), f64::from(v.z)];
    Cubic {
        p0: arr(p0),
        m0: arr(m0),
        p1: arr(p1),
        m1: arr(m1),
    }
}

fn check_world_cubic(spline: &Spline, transform: &Affine3A, segment: usize, position_epsilon: f32) {
    let oracle = transformed_cubic(local_free_controls(spline, segment), transform);
    let fine = oracle.simpson_length(16384);
    let table = oracle.distance_table(16384);
    let curve = spline.world_curve(transform);
    let segment_count = if spline.closed {
        spline.points.len()
    } else {
        spline.points.len() - 1
    };
    let expected_total: f64 = (0..segment_count)
        .map(|i| transformed_cubic(local_free_controls(spline, i), transform).simpson_length(16384))
        .sum();
    let total_tolerance = 2.0e-4_f64.max(expected_total * 2.0e-6);
    assert!(
        (curve.length_f64() - expected_total).abs() < total_tolerance,
        "world total {} vs independent {expected_total} (tol {total_tolerance})",
        curve.length_f64()
    );
    let tolerance = 2.0e-4_f64.max(fine * 2.0e-6);
    assert!(
        (f64::from(curve.segment_length(segment)) - fine).abs() < tolerance,
        "segment {segment}: world length {} vs independent {fine} (tol {tolerance})",
        curve.segment_length(segment)
    );
    for fraction in [0.07, 0.23, 0.5, 0.79, 0.94] {
        let segment_start: f64 = (0..segment).map(|i| f64::from(curve.segment_length(i))).sum();
        let d = curve.segment_length(segment) as f64 * fraction;
        let actual = curve.at_distance_f64(segment_start + d);
        let (position, derivative) = oracle.at_fraction(&table, fraction);
        let expected = Vec3::new(position[0] as f32, position[1] as f32, position[2] as f32);
        let tangent = Vec3::new(derivative[0] as f32, derivative[1] as f32, derivative[2] as f32).normalize();
        assert!(
            actual.position.distance(expected) < position_epsilon,
            "fraction {fraction}: {:?} vs {:?}, error {}",
            actual.position,
            expected,
            actual.position.distance(expected)
        );
        assert!(
            actual.tangent.dot(tangent) > 0.9999,
            "tangent mismatch at {fraction}: {:?} vs {tangent:?}",
            actual.tangent
        );
        assert!((actual.tangent.length() - 1.0).abs() < 2.0e-5);
        assert!((actual.up.length() - 1.0).abs() < 2.0e-4);
        assert!((actual.right.length() - 1.0).abs() < 2.0e-4);
        assert!(actual.tangent.dot(actual.up).abs() < 2.0e-4);
        assert!(actual.right.abs_diff_eq(actual.tangent.cross(actual.up), 3.0e-4));
    }
}

fn transformed_free_cubic_fixture() -> (Spline, Affine3A) {
    let spline = Spline {
        points: vec![
            SplinePoint::free(
                Vec3::new(-2.0, 1.0, 0.5),
                Vec3::new(4.0, 3.0, -1.0),
                Vec3::new(4.0, 3.0, -1.0),
            ),
            SplinePoint::free(
                Vec3::new(5.0, 2.0, 3.0),
                Vec3::new(2.0, -2.0, 4.0),
                Vec3::new(2.0, -2.0, 4.0),
            ),
        ],
        ..Spline::default()
    };
    let transform = Affine3A::from_scale_rotation_translation(
        Vec3::new(2.0, 0.7, 1.4),
        Quat::from_rotation_y(0.73) * Quat::from_rotation_x(-0.31),
        Vec3::new(100_000.0, -80_000.0, 120_000.0),
    );
    (spline, transform)
}

#[test]
fn transformed_cubic_simpson_quadrature_converges_once() {
    let (spline, transform) = transformed_free_cubic_fixture();
    let oracle = transformed_cubic(local_free_controls(&spline, 0), &transform);
    let coarse = oracle.simpson_length(8192);
    let fine = oracle.simpson_length(16384);
    assert!(
        (fine - coarse).abs() < 2.0e-7,
        "quadrature did not converge: {coarse} vs {fine}"
    );
}

#[test]
fn world_line_lengths_follow_axis_scale_exactly() {
    for (axis, expected) in [(Vec3::X, 25.0), (Vec3::Z, 4.0)] {
        let spline = Spline::through([Vec3::ZERO, axis * 10.0]);
        let transform = Affine3A::from_scale(Vec3::new(2.5, 1.3, 0.4));
        let curve = spline.world_curve(&transform);
        assert!(
            (curve.length() - expected).abs() < 2.0e-5,
            "{} vs {expected}",
            curve.length()
        );
        assert!((curve.segment_length(0) - expected).abs() < 2.0e-5);
        for fraction in [0.0, 0.2, 0.5, 0.8, 1.0] {
            assert!(
                curve
                    .at_distance_f64(f64::from(expected) * fraction)
                    .position
                    .abs_diff_eq(axis * (expected * fraction as f32), 2.0e-5)
            );
        }
    }
}

#[test]
fn transformed_free_cubic_uses_world_metric_with_rotation_and_translation() {
    let (spline, transform) = transformed_free_cubic_fixture();
    check_world_cubic(&spline, &transform, 0, 0.04);
}

#[test]
fn reversed_cubic_and_closed_seam_keep_world_metric_and_frames() {
    let pts = [
        (Vec3::new(0.0, 0.0, 0.0), Vec3::new(2.0, 1.0, 0.0)),
        (Vec3::new(4.0, 1.0, 0.0), Vec3::new(0.0, 2.0, 1.0)),
        (Vec3::new(2.0, 3.0, 4.0), Vec3::new(-2.0, 0.0, 1.0)),
    ];
    let spline = Spline {
        points: pts.iter().map(|(p, t)| SplinePoint::free(*p, *t, *t)).collect(),
        closed: true,
        ..Spline::default()
    };
    let transform = Affine3A::from_scale_rotation_translation(
        Vec3::new(1.8, 0.6, 2.2),
        Quat::from_rotation_z(0.41) * Quat::from_rotation_y(-0.62),
        Vec3::new(-12.0, 7.0, 31.0),
    );
    for segment in 0..spline.segment_count() {
        check_world_cubic(&spline, &transform, segment, 2.0e-3);
    }
    let seam = spline.world_curve(&transform);
    let start = seam.at_distance_f64(0.0);
    let end = seam.at_distance_f64(seam.length_f64());
    assert!(start.position.abs_diff_eq(end.position, 2.0e-5));
    assert!(start.tangent.dot(end.tangent) > 0.9999);
    let seam_up_angle = start.up.dot(end.up).clamp(-1.0, 1.0).acos().to_degrees();
    assert!(
        seam_up_angle < 0.1,
        "closed world-frame seam differs by {seam_up_angle}°"
    );

    let reverse = Spline {
        points: spline
            .points
            .iter()
            .rev()
            .map(|p| SplinePoint::free(p.position, -p.tangent_out, -p.tangent_in))
            .collect(),
        closed: true,
        ..Spline::default()
    };
    let reversed_curve = reverse.world_curve(&transform);
    let forward_segment = spline.world_curve(&transform).segment_length(0);
    let backward_segment = reversed_curve.segment_length(1);
    assert!((forward_segment - backward_segment).abs() < 2.0e-4);
    let forward = spline
        .world_curve(&transform)
        .at_distance_f64(0.37 * f64::from(forward_segment));
    let backward_segment_start = f64::from(reversed_curve.segment_length(0));
    let backward =
        reversed_curve.at_distance_f64(backward_segment_start + f64::from(backward_segment) * 0.63);
    assert!(forward.position.abs_diff_eq(backward.position, 2.0e-4));
    assert!(forward.tangent.dot(-backward.tangent) > 0.9999);
}
