// SPDX-License-Identifier: MPL-2.0
//! Differential oracle for the existing unit-scale, contiguous two-bone IK contract.
//!
//! Unlike `ik_edges::two_bone_reaches_fixed_seed_targets_and_keeps_bone_lengths`, these
//! cases put unequal bones below a rotated parent and check the elbow against an independent
//! f64 triangle construction. No production bend/rotation helper computes the reference.

use regia_animation::ik::{IkChain, solve_two_bone};
use regia_animation::skeleton::LocalBind;
use regia_animation::{Pose, Skeleton};
use regia_core::glam::DVec3;
use regia_core::math::{Mat4, Quat, Vec3};

const ROOT: usize = 1;
const MID: usize = 2;
const TIP: usize = 3;
const SIBLING: usize = 4;
const CHAIN: IkChain = IkChain {
    root: ROOT as u32,
    mid: MID as u32,
    tip: TIP as u32,
};

fn fixture(upper: f32, lower: f32) -> Skeleton {
    Skeleton::from_local_bind(
        ["parent", "shoulder", "elbow", "tip", "sibling"]
            .map(String::from)
            .to_vec(),
        vec![None, Some(0), Some(1), Some(2), Some(0)],
        vec![
            LocalBind {
                translation: Vec3::new(0.3, -0.2, 0.4) * (upper + lower),
                rotation: Quat::from_rotation_y(0.63) * Quat::from_rotation_x(-0.41),
                ..LocalBind::default()
            },
            LocalBind {
                translation: Vec3::new(-0.2, 0.3, 0.1) * (upper + lower),
                rotation: Quat::from_rotation_z(0.31) * Quat::from_rotation_y(-0.27),
                ..LocalBind::default()
            },
            LocalBind {
                translation: Vec3::Y * upper,
                rotation: Quat::from_rotation_x(-0.48) * Quat::from_rotation_z(0.22),
                ..LocalBind::default()
            },
            LocalBind {
                translation: Vec3::NEG_Z * lower,
                rotation: Quat::from_rotation_y(0.37),
                ..LocalBind::default()
            },
            LocalBind {
                translation: Vec3::new(0.7, -0.6, 0.2) * (upper + lower),
                rotation: Quat::from_rotation_z(-0.59),
                ..LocalBind::default()
            },
        ],
    )
    .expect("fixed, topologically sorted unit-scale fixture")
}

/// Intersect the two length spheres in the target/pole plane. Heron's area gives the
/// elbow's perpendicular height; the projection's sign handles a longer lower bone.
/// The annulus inset is the existing solver contract, not a new reach policy.
fn triangle(root: DVec3, target: DVec3, pole: DVec3, upper: f64, lower: f64) -> [DVec3; 2] {
    let direction = (target - root).normalize();
    let inset = 1e-4_f64.min(0.25 * upper.min(lower));
    let outer = upper + lower - inset;
    let inner = ((upper - lower).abs() + inset).min(outer);
    let distance = (target - root).length().clamp(inner, outer);
    let semi = (upper + lower + distance) * 0.5;
    let area = (semi * (semi - upper) * (semi - lower) * (semi - distance)).sqrt();
    let height = 2.0 * area / distance;
    let along = (upper * upper - height * height).max(0.0).sqrt()
        * (distance * distance + upper * upper - lower * lower).signum();
    let to_pole = pole - root;
    let perpendicular = to_pole - direction * to_pole.dot(direction);
    assert!(
        perpendicular.length() > 1e-3 * upper.min(lower),
        "non-singular pole"
    );
    [
        root + direction * along + perpendicular.normalize() * height,
        root + direction * distance,
    ]
}

fn position(matrix: Mat4) -> DVec3 {
    matrix.w_axis.truncate().as_dvec3()
}

fn near(actual: DVec3, expected: DVec3, tolerance: f64, label: &str) {
    let error = actual.distance(expected);
    assert!(
        error <= tolerance,
        "{label}: error {error:e} > {tolerance:e}; actual {actual}, expected {expected}"
    );
}

fn same_axes(actual: Quat, expected: Quat) {
    for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
        near(
            (actual * axis).as_dvec3(),
            (expected * axis).as_dvec3(),
            0.0,
            "unchanged local rotation axis",
        );
    }
}

fn unchanged_locals(before: &Pose, after: &Pose) {
    assert_eq!(
        after.translations, before.translations,
        "IK only writes rotations"
    );
    assert_eq!(after.scales, before.scales, "unit scales stay fixed");
    for joint in [0, TIP, SIBLING] {
        same_axes(after.rotations[joint], before.rotations[joint]);
    }
}

/// Allow f32 FK roundoff proportional to limb length and coordinate magnitude. Near an
/// annulus edge the elbow height amplifies distance roundoff; 8e-5 of total limb length
/// covers that conditioning. Segment lengths get the tighter 4e-6 relative bound.
fn tolerance(total: f64, coordinate: f64, elbow: bool) -> f64 {
    (if elbow { 8e-5 } else { 4e-6 }) * total + 8.0 * f64::from(f32::EPSILON) * coordinate
}

fn solve_checked(sk: &Skeleton, input: &Pose, target: Vec3, pole: Vec3, weight: f32) -> Vec<Mat4> {
    let before = sk.globals(input, None);
    let mut solved = input.clone();
    solve_two_bone(sk, &mut solved, &CHAIN, target, Some(pole), weight);
    unchanged_locals(input, &solved);
    let actual = sk.globals(&solved, None);
    let upper = input.translations[MID].as_dvec3().length();
    let lower = input.translations[TIP].as_dvec3().length();
    let total = upper + lower;
    let coordinate = actual.iter().map(|m| position(*m).length()).fold(total, f64::max);
    let length_tolerance = tolerance(total, coordinate, false);
    for (a, b, length) in [(ROOT, MID, upper), (MID, TIP, lower)] {
        let error = (position(actual[a]).distance(position(actual[b])) - length).abs();
        assert!(error <= length_tolerance, "segment {a}->{b}: {error:e}");
    }
    for joint in [0, ROOT, SIBLING] {
        near(
            position(actual[joint]),
            position(before[joint]),
            length_tolerance,
            "fixed joint",
        );
    }
    if weight == 0.0 {
        for (after, before) in actual.iter().zip(&before) {
            near(position(*after), position(*before), 0.0, "zero weight position");
            for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
                near(
                    after.transform_vector3(axis).as_dvec3(),
                    before.transform_vector3(axis).as_dvec3(),
                    0.0,
                    "zero weight axis",
                );
            }
        }
    } else {
        let expected = triangle(
            position(before[ROOT]),
            target.as_dvec3(),
            pole.as_dvec3(),
            upper,
            lower,
        );
        for (joint, expected) in [MID, TIP].into_iter().zip(expected) {
            near(
                position(actual[joint]),
                expected,
                tolerance(total, coordinate, joint == MID),
                "f64 triangle",
            );
        }
    }
    actual
}

/// Transform the entire object frame by editing just the chain's ancestor and the two
/// object-space points. The sibling shares the ancestor but is outside the solved chain.
fn compare_frames(sk: &Skeleton, distance: f32, direction: Vec3, pole_offset: Vec3, weight: f32) {
    let input = sk.bind_pose_pose();
    let root = sk.globals(&input, None)[ROOT].w_axis.truncate();
    let target = root + direction.normalize() * distance;
    let pole = root + pole_offset * (input.translations[MID].length() + input.translations[TIP].length());
    let baseline = solve_checked(sk, &input, target, pole, weight);
    let total = f64::from(input.translations[MID].length() + input.translations[TIP].length());
    for (rotation, offset) in [
        (
            Quat::from_axis_angle(Vec3::new(0.3, -0.7, 0.6).normalize(), 0.87),
            Vec3::new(-0.4, 0.7, -0.2),
        ),
        (
            Quat::from_rotation_z(-1.1) * Quat::from_rotation_x(0.72),
            Vec3::new(1.3, -0.5, 0.6),
        ),
        (
            Quat::from_axis_angle(Vec3::new(-0.6, 0.2, 0.7).normalize(), 2.13),
            Vec3::new(-0.8, -0.3, 0.9),
        ),
    ] {
        let offset = offset * total as f32;
        let mut moved = input.clone();
        moved.rotations[0] = (rotation * input.rotations[0]).normalize();
        moved.translations[0] = rotation * input.translations[0] + offset;
        let transformed = solve_checked(
            sk,
            &moved,
            rotation * target + offset,
            rotation * pole + offset,
            weight,
        );
        for (actual, original) in transformed.iter().zip(&baseline) {
            let expected = rotation * original.w_axis.truncate() + offset;
            let coordinate = position(*actual).length().max(expected.as_dvec3().length());
            near(
                position(*actual),
                expected.as_dvec3(),
                2.0 * tolerance(total, coordinate, true),
                "rigid position",
            );
            for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
                near(
                    actual.transform_vector3(axis).as_dvec3(),
                    (rotation * original.transform_vector3(axis)).as_dvec3(),
                    3e-4,
                    "rigid rotated axis",
                );
            }
        }
    }
}

#[test]
fn reachable_triangles_follow_rotated_parent_frames() {
    for scale in [0.2, 1.0, 3.0] {
        for (upper, lower) in [(0.83, 0.47), (0.43, 0.91)] {
            let sk = fixture(upper * scale, lower * scale);
            for (direction, pole) in [
                (Vec3::new(0.6, -0.2, -0.7), Vec3::new(-0.3, 1.1, 0.5)),
                (Vec3::new(-0.4, 0.8, 0.3), Vec3::new(0.9, -0.2, 0.7)),
                (Vec3::new(0.2, 0.5, -0.9), Vec3::new(-0.8, 0.3, -0.4)),
            ] {
                for weight in [0.0, 1.0] {
                    compare_frames(&sk, (upper + lower) * scale * 0.72, direction, pole, weight);
                }
            }
        }
    }
}

#[test]
fn annulus_edges_match_the_reference_under_rigid_motion() {
    for scale in [0.2, 1.0, 3.0] {
        for (upper, lower) in [(0.83, 0.47), (0.43, 0.91)] {
            let (upper, lower) = (upper * scale, lower * scale);
            let sk = fixture(upper, lower);
            let margin = 1e-4_f32.min(0.25 * upper.min(lower));
            let inner = (upper - lower).abs();
            let outer = upper + lower;
            // Both physical boundaries and both numerical clamp boundaries, on either side.
            for distance in [
                inner - margin * 0.5,
                inner + margin * 0.5,
                inner + margin * 1.5,
                outer - margin * 1.5,
                outer - margin * 0.5,
                outer + margin * 0.5,
            ] {
                compare_frames(
                    &sk,
                    distance,
                    Vec3::new(0.6, -0.3, -0.7),
                    Vec3::new(-0.4, 0.9, 0.6),
                    1.0,
                );
            }
        }
    }
}

#[test]
fn quaternion_sign_equivalent_parents_produce_the_same_geometry() {
    let sk = fixture(0.43, 0.91);
    let input = sk.bind_pose_pose();
    let root = sk.globals(&input, None)[ROOT].w_axis.truncate();
    let target = root + Vec3::new(-0.7, 0.4, -0.3);
    let pole = root + Vec3::new(0.6, 0.9, 0.2);
    for weight in [0.0, 1.0] {
        let original = solve_checked(&sk, &input, target, pole, weight);
        let mut negated = input.clone();
        negated.rotations[0] = -negated.rotations[0];
        let equivalent = solve_checked(&sk, &negated, target, pole, weight);
        for (a, b) in original.iter().zip(equivalent) {
            near(position(*a), position(b), 1e-6, "double cover position");
            for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
                near(
                    a.transform_vector3(axis).as_dvec3(),
                    b.transform_vector3(axis).as_dvec3(),
                    1e-6,
                    "double cover axis",
                );
            }
        }
    }
}
