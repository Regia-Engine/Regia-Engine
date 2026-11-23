// SPDX-License-Identifier: MPL-2.0
//! Procedural demo humanoids and locomotion, for retarget and crowd tests, samples and agents.
//!
//! Specification: `docs/specs/85-retarget-crowds.md` § 6. [`demo_humanoid`] builds a 20-joint
//! humanoid in one of several real-world exporter conventions: joint naming (Mixamo, game-style,
//! Biped, plain), size, units (a centimetre rig has a 0.01 root scale), facing (−Z or +Z), rolled
//! bind frames (non-identity bind rotations everywhere), an A-pose, and extra twist / IK joints.
//! [`demo_locomotion`] authors idle, walk and run clips on the plain rig with planted feet (an
//! analytic two-bone leg IK per frame), root motion on the root joint (`XZ`). [`demo_in_place`]
//! and [`demo_travelling_hips`] turn those clips into the two shapes imported clips arrive in
//! without a root-motion track.
//!
//! The canonical layout (index: role) is: 0 root, 1 hips, 2 spine, 3 chest, 4 neck, 5 head,
//! 6–8 left upper arm / forearm / hand, 9–11 right, 12–15 left thigh / calf / foot / toes,
//! 16–19 right. Extra joints are appended after index 19.

use std::f32::consts::PI;

use regia_core::math::{Mat4, Quat, Vec3, det};

use crate::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use crate::skeleton::{LocalBind, Skeleton};

/// Joint naming convention of a demo rig.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RigNaming {
    /// `mixamorig:Hips`, `mixamorig:LeftUpLeg`, …
    Mixamo,
    /// `pelvis`, `thigh_l`, `upperarm_r`, …
    Game,
    /// `Bip01 Pelvis`, `Bip01 L Thigh`, …
    Biped,
    /// `hips`, `left_thigh`, …
    Plain,
}

/// A demo humanoid's conventions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DemoRig {
    /// Joint naming.
    pub naming: RigNaming,
    /// Uniform size factor (1 = hips at 1 m).
    pub size: f32,
    /// Face +Z (left on +X) instead of Regia's −Z.
    pub facing_positive_z: bool,
    /// Centimetre units: a 0.01 root scale with translations in centimetres.
    pub centimetres: bool,
    /// Non-identity bind rotations: every joint's frame rolled by a fixed per-joint rotation.
    pub rolled_frames: bool,
    /// Arms 40° down (A-pose) instead of a T-pose.
    pub a_pose: bool,
    /// Append twist and IK joints (which the role guesser must ignore).
    pub extra_joints: bool,
}

impl Default for DemoRig {
    fn default() -> Self {
        DemoRig {
            naming: RigNaming::Plain,
            size: 1.0,
            facing_positive_z: false,
            centimetres: false,
            rolled_frames: false,
            a_pose: false,
            extra_joints: false,
        }
    }
}

/// Parent of each canonical joint.
pub const DEMO_PARENTS: [Option<u32>; 20] = [
    None,
    Some(0),
    Some(1),
    Some(2),
    Some(3),
    Some(4),
    Some(3),
    Some(6),
    Some(7),
    Some(3),
    Some(9),
    Some(10),
    Some(1),
    Some(12),
    Some(13),
    Some(14),
    Some(1),
    Some(16),
    Some(17),
    Some(18),
];

/// Canonical index of the left and right thigh, calf, foot.
pub const DEMO_LEGS: [[usize; 3]; 2] = [[12, 13, 14], [16, 17, 18]];

fn names(naming: RigNaming) -> [&'static str; 20] {
    match naming {
        RigNaming::Mixamo => [
            "Armature",
            "mixamorig:Hips",
            "mixamorig:Spine",
            "mixamorig:Spine1",
            "mixamorig:Neck",
            "mixamorig:Head",
            "mixamorig:LeftArm",
            "mixamorig:LeftForeArm",
            "mixamorig:LeftHand",
            "mixamorig:RightArm",
            "mixamorig:RightForeArm",
            "mixamorig:RightHand",
            "mixamorig:LeftUpLeg",
            "mixamorig:LeftLeg",
            "mixamorig:LeftFoot",
            "mixamorig:LeftToeBase",
            "mixamorig:RightUpLeg",
            "mixamorig:RightLeg",
            "mixamorig:RightFoot",
            "mixamorig:RightToeBase",
        ],
        RigNaming::Game => [
            "root",
            "pelvis",
            "spine_01",
            "spine_02",
            "neck_01",
            "head",
            "upperarm_l",
            "lowerarm_l",
            "hand_l",
            "upperarm_r",
            "lowerarm_r",
            "hand_r",
            "thigh_l",
            "calf_l",
            "foot_l",
            "ball_l",
            "thigh_r",
            "calf_r",
            "foot_r",
            "ball_r",
        ],
        RigNaming::Biped => [
            "Bip01",
            "Bip01 Pelvis",
            "Bip01 Spine",
            "Bip01 Spine1",
            "Bip01 Neck",
            "Bip01 Head",
            "Bip01 L UpperArm",
            "Bip01 L Forearm",
            "Bip01 L Hand",
            "Bip01 R UpperArm",
            "Bip01 R Forearm",
            "Bip01 R Hand",
            "Bip01 L Thigh",
            "Bip01 L Calf",
            "Bip01 L Foot",
            "Bip01 L Toe0",
            "Bip01 R Thigh",
            "Bip01 R Calf",
            "Bip01 R Foot",
            "Bip01 R Toe0",
        ],
        RigNaming::Plain => [
            "root",
            "hips",
            "spine",
            "chest",
            "neck",
            "head",
            "left_upper_arm",
            "left_forearm",
            "left_hand",
            "right_upper_arm",
            "right_forearm",
            "right_hand",
            "left_thigh",
            "left_calf",
            "left_foot",
            "left_toes",
            "right_thigh",
            "right_calf",
            "right_foot",
            "right_toes",
        ],
    }
}

/// Canonical bind positions, meters, facing −Z (left on −X), T-pose.
fn positions(a_pose: bool) -> [Vec3; 20] {
    let arm = if a_pose {
        Vec3::new(det::cos(40f32.to_radians()), -det::sin(40f32.to_radians()), 0.0) * 0.28
    } else {
        Vec3::new(0.28, 0.0, 0.0)
    };
    let shoulder = Vec3::new(0.18, 1.45, 0.0);
    let left = |v: Vec3| Vec3::new(-v.x, v.y, v.z);
    [
        Vec3::ZERO,
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, 1.12, 0.0),
        Vec3::new(0.0, 1.3, 0.0),
        Vec3::new(0.0, 1.52, 0.0),
        Vec3::new(0.0, 1.62, 0.0),
        left(shoulder),
        left(shoulder + arm),
        left(shoulder + arm * 2.0),
        shoulder,
        shoulder + arm,
        shoulder + arm * 2.0,
        Vec3::new(-0.1, 0.98, 0.0),
        Vec3::new(-0.1, 0.52, 0.0),
        Vec3::new(-0.1, 0.08, 0.0),
        Vec3::new(-0.1, 0.0, -0.12),
        Vec3::new(0.1, 0.98, 0.0),
        Vec3::new(0.1, 0.52, 0.0),
        Vec3::new(0.1, 0.08, 0.0),
        Vec3::new(0.1, 0.0, -0.12),
    ]
}

/// A fixed, joint-dependent bind frame rotation (rolled frames).
fn roll(joint: usize) -> Quat {
    let k = joint as f32;
    det::quat_from_euler_xyz(
        0.7 * det::sin(k * 1.3),
        1.1 * det::cos(k * 0.7),
        0.5 * det::sin(k * 2.1),
    )
    .normalize()
}

/// Build a skeleton from object-space joint positions and rotations (a uniform root scale).
fn from_globals(
    names: Vec<String>,
    parents: Vec<Option<u32>>,
    positions: &[Vec3],
    rotations: &[Quat],
    root_scale: f32,
) -> Option<Skeleton> {
    let mut globals: Vec<Mat4> = Vec::with_capacity(names.len());
    let mut binds = Vec::with_capacity(names.len());
    for (i, parent) in parents.iter().enumerate() {
        let (local, global) = match parent {
            None => {
                let local = LocalBind {
                    translation: positions[i],
                    rotation: rotations[i],
                    scale: Vec3::splat(root_scale),
                };
                (
                    local,
                    Mat4::from_scale_rotation_translation(
                        Vec3::splat(root_scale),
                        rotations[i],
                        positions[i],
                    ),
                )
            }
            Some(p) => {
                let parent_global = *globals.get(*p as usize)?;
                let (_, parent_rotation, _) = parent_global.to_scale_rotation_translation();
                let translation = parent_global.inverse().transform_point3(positions[i]);
                let rotation = (parent_rotation.inverse() * rotations[i]).normalize();
                let local = LocalBind {
                    translation,
                    rotation,
                    scale: Vec3::ONE,
                };
                let global =
                    parent_global * Mat4::from_scale_rotation_translation(Vec3::ONE, rotation, translation);
                (local, global)
            }
        };
        binds.push(local);
        globals.push(global);
    }
    Skeleton::from_local_bind(names, parents, binds).ok()
}

/// A demo humanoid in the conventions of `rig` (module docs). `None` only for a non-finite or
/// non-positive size.
#[must_use]
pub fn demo_humanoid(rig: &DemoRig) -> Option<Skeleton> {
    if !(rig.size.is_finite() && rig.size > 0.0) {
        return None;
    }
    let turn = if rig.facing_positive_z {
        det::quat_from_rotation_y(PI)
    } else {
        Quat::IDENTITY
    };
    let mut names: Vec<String> = names(rig.naming).iter().map(|n| n.to_string()).collect();
    let mut parents: Vec<Option<u32>> = DEMO_PARENTS.to_vec();
    let mut points: Vec<Vec3> = positions(rig.a_pose)
        .iter()
        .map(|p| turn * (*p * rig.size))
        .collect();
    if rig.extra_joints {
        // Twist joints halfway along the thighs and forearms, and an IK foot chain off the root.
        let extra = [
            ("thigh_twist_01_l", 12u32, (points[12] + points[13]) * 0.5),
            ("thigh_twist_01_r", 16, (points[16] + points[17]) * 0.5),
            ("lowerarm_twist_01_l", 7, (points[7] + points[8]) * 0.5),
            ("ik_foot_root", 0, Vec3::ZERO),
            ("ik_foot_l", 23, points[14]),
            ("ik_foot_r", 23, points[18]),
            ("headTop_End", 5, points[5] + Vec3::Y * 0.15 * rig.size),
        ];
        for (name, parent, point) in extra {
            names.push(name.to_string());
            parents.push(Some(parent));
            points.push(point);
        }
    }
    let rotations: Vec<Quat> = (0..names.len())
        .map(|i| {
            if rig.rolled_frames {
                (turn * roll(i)).normalize()
            } else {
                turn
            }
        })
        .collect();
    let root_scale = if rig.centimetres { 0.01 } else { 1.0 };
    from_globals(names, parents, &points, &rotations, root_scale)
}

/// One locomotion clip to author.
#[derive(Clone, Debug, PartialEq)]
pub struct DemoGait {
    /// Clip name.
    pub name: String,
    /// Ground speed, m/s (0: idle in place, no root motion).
    pub speed: f32,
    /// Cycle length, seconds.
    pub cycle: f32,
    /// Fraction of the cycle each foot is planted.
    pub duty: f32,
}

impl DemoGait {
    /// The default idle, walk and run.
    #[must_use]
    pub fn standard() -> Vec<DemoGait> {
        vec![
            DemoGait {
                name: "idle".into(),
                speed: 0.0,
                cycle: 2.0,
                duty: 1.0,
            },
            DemoGait {
                name: "walk".into(),
                speed: 1.3,
                cycle: 1.0,
                duty: 0.55,
            },
            DemoGait {
                name: "run".into(),
                speed: 3.2,
                cycle: 0.66,
                duty: 0.36,
            },
        ]
    }
}

/// Two-bone leg IK in the sagittal plane: thigh and calf angles (rotation about +X; positive
/// swings the leg towards −Z) that put the ankle at `ankle` from a hip joint at `hip`.
///
/// A leg the IK cannot bend (a bone shorter than 0.1 mm, non-finite lengths: a rig without the
/// leg joints, or with coincident ones) keeps its bind angles `(0, 0)`.
fn leg_angles(hip: Vec3, ankle: Vec3, thigh: f32, calf: f32) -> (f32, f32) {
    let (reach_min, reach_max) = ((thigh - calf).abs() + 1e-4, thigh + calf - 1e-4);
    // `f32::clamp` panics on NaN or inverted bounds.
    if !(thigh >= 1e-4 && calf >= 1e-4 && reach_min <= reach_max && reach_max.is_finite()) {
        return (0.0, 0.0);
    }
    let dy = ankle.y - hip.y;
    let dz = ankle.z - hip.z;
    let d = (dy * dy + dz * dz).sqrt().clamp(reach_min, reach_max);
    let toward = det::atan2(-dz, -dy);
    // The deterministic kernels keep the cooked demo clips bit-identical across platform
    // libm (audit-v040 L12).
    let hip_angle = det::acos(((thigh * thigh + d * d - calf * calf) / (2.0 * thigh * d)).clamp(-1.0, 1.0));
    let knee_inner =
        det::acos(((thigh * thigh + calf * calf - d * d) / (2.0 * thigh * calf)).clamp(-1.0, 1.0));
    let upper = toward + hip_angle;
    (upper, -(PI - knee_inner))
}

/// Author `gaits` on `skeleton`, a plain demo rig ([`DemoRig::default`] layout: meters, −Z
/// forward, identity bind frames, possibly sized). Feet are planted during stance (leg IK),
/// swing in an arc, and the root carries `XZ` root motion; idle bobs the hips and sways the spine.
/// Clips are sampled at 30 Hz.
#[must_use]
pub fn demo_locomotion(skeleton: &Skeleton, gaits: &[DemoGait]) -> Vec<AnimationClip> {
    let bind = skeleton.bind_globals();
    let at = |j: usize| bind.get(j).map_or(Vec3::ZERO, |m| m.w_axis.truncate());
    let thigh = (at(12) - at(13)).length();
    let calf = (at(13) - at(14)).length();
    let hip_offset = at(12) - at(1);
    gaits
        .iter()
        .map(|gait| {
            let cycle = gait.cycle.max(0.1);
            let frames = ((cycle * 30.0).round() as usize).max(2);
            let times: Vec<f32> = (0..=frames).map(|k| k as f32 / frames as f32 * cycle).collect();
            let stride = gait.speed * cycle;
            let mut root = Vec::new();
            let mut hips = Vec::new();
            let mut spine = Vec::new();
            let mut legs: [[Vec<Quat>; 3]; 2] = Default::default();
            for &t in &times {
                let phase = t / cycle;
                let travel = gait.speed * t;
                root.push(Vec3::new(0.0, 0.0, -travel));
                // Hips dip so the legs can reach; bob twice per cycle.
                let (dip, bob) = if gait.speed > 0.0 {
                    (0.1, 0.02 * det::cos(4.0 * PI * phase))
                } else {
                    (0.02, 0.01 * det::cos(2.0 * PI * phase))
                };
                let hips_local = at(1) - Vec3::Y * (dip + bob);
                hips.push(hips_local);
                spine.push(det::quat_from_rotation_z(0.04 * det::sin(2.0 * PI * phase)));
                for (side, offset) in [0.0f32, 0.5].into_iter().enumerate() {
                    let local_phase = (phase + offset).rem_euclid(1.0);
                    // Forward distance of the hip from the start of this cycle.
                    let hip_forward = travel;
                    let foot_forward = if gait.speed <= 0.0 {
                        0.0
                    } else if local_phase < gait.duty {
                        // Planted: from half a stance ahead of the hip at contact.
                        let contact = local_phase * cycle;
                        hip_forward - gait.speed * contact + stride * gait.duty * 0.5
                    } else {
                        let u = (local_phase - gait.duty) / (1.0 - gait.duty);
                        let lift_from = stride * gait.duty * -0.5;
                        let lift_to = stride * gait.duty * 0.5;
                        hip_forward + lift_from + (lift_to - lift_from) * u
                    };
                    let lift = if gait.speed > 0.0 && local_phase >= gait.duty {
                        0.08 * det::sin(PI * (local_phase - gait.duty) / (1.0 - gait.duty))
                    } else {
                        0.0
                    };
                    let hip = Vec3::new(0.0, hips_local.y + hip_offset.y, -travel);
                    let ankle = Vec3::new(0.0, at(14).y + lift, -foot_forward);
                    let (upper, knee) = leg_angles(hip, ankle, thigh, calf);
                    legs[side][0].push(det::quat_from_rotation_x(upper));
                    legs[side][1].push(det::quat_from_rotation_x(knee));
                    legs[side][2].push(det::quat_from_rotation_x(-(upper + knee)));
                }
            }
            let mut clip = AnimationClip::empty(gait.name.clone(), cycle);
            clip.sample_rate = 30.0;
            let animated = |values: Vec<Vec3>| TrackCurve::Animated {
                times: times.clone(),
                values,
                exact_rotation: false,
            };
            let rotated = |values: Vec<Quat>| TrackCurve::Animated {
                times: times.clone(),
                values,
                exact_rotation: false,
            };
            let rest = |j: usize| skeleton.bind_pose.get(j).copied().unwrap_or_default();
            clip.tracks.push(JointTrack {
                joint: 0,
                translations: animated(root),
                rotations: TrackCurve::Static(rest(0).rotation),
                scales: TrackCurve::Static(rest(0).scale),
            });
            clip.tracks.push(JointTrack {
                joint: 1,
                translations: animated(hips),
                rotations: TrackCurve::Static(rest(1).rotation),
                scales: TrackCurve::Static(rest(1).scale),
            });
            clip.tracks.push(JointTrack {
                joint: 2,
                translations: TrackCurve::Static(rest(2).translation),
                rotations: rotated(spine),
                scales: TrackCurve::Static(rest(2).scale),
            });
            for (side, chain) in DEMO_LEGS.iter().enumerate() {
                for (k, &joint) in chain.iter().enumerate() {
                    clip.tracks.push(JointTrack {
                        joint: joint as u32,
                        translations: TrackCurve::Static(rest(joint).translation),
                        rotations: rotated(std::mem::take(&mut legs[side][k])),
                        scales: TrackCurve::Static(rest(joint).scale),
                    });
                }
            }
            clip.tracks.sort_by_key(|t| t.joint);
            if gait.speed > 0.0 {
                clip.root_motion = Some(RootMotionTrack {
                    joint: 0,
                    mode: RootMotionMode::XZ,
                });
            }
            clip
        })
        .collect()
}

/// `clip` (from [`demo_locomotion`]) as an in-place treadmill clip: no root-motion track, the
/// root joint holds still, and the planted feet slide back at the gait speed. This is how a clip
/// authored "in place" arrives from an importer.
#[must_use]
pub fn demo_in_place(clip: &AnimationClip) -> AnimationClip {
    let mut out = clip.clone();
    out.root_motion = None;
    if let Some(track) = out.tracks.iter_mut().find(|t| t.joint == 0) {
        track.translations = TrackCurve::Static(track.translations.sample(0.0));
    }
    out
}

/// `clip` (from [`demo_locomotion`]) with its ground travel on the hips joint and no root-motion
/// track: the root joint holds still and the whole body walks away from the object origin. This
/// is how a clip exported with its travel on the pelvis arrives from an importer whose root
/// motion setting is off (the default).
#[must_use]
pub fn demo_travelling_hips(clip: &AnimationClip) -> AnimationClip {
    let mut out = demo_in_place(clip);
    let root = clip.tracks.iter().find(|t| t.joint == 0);
    if let (Some(root), Some(hips)) = (root, out.tracks.iter_mut().find(|t| t.joint == 1))
        && let TrackCurve::Animated { times, values, .. } = &mut hips.translations
    {
        let origin = root.translations.sample(0.0);
        for (time, value) in times.iter().zip(values.iter_mut()) {
            *value += root.translations.sample(*time) - origin;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_rigs_keep_their_shape_in_every_convention() {
        let plain = demo_humanoid(&DemoRig::default()).expect("plain");
        let base = plain.bind_globals();
        for rig in [
            DemoRig {
                naming: RigNaming::Mixamo,
                centimetres: true,
                rolled_frames: true,
                ..DemoRig::default()
            },
            DemoRig {
                naming: RigNaming::Game,
                facing_positive_z: true,
                extra_joints: true,
                ..DemoRig::default()
            },
        ] {
            let sk = demo_humanoid(&rig).expect("rig");
            let globals = sk.globals(&sk.bind_pose_pose(), None);
            let turn = if rig.facing_positive_z {
                det::quat_from_rotation_y(PI)
            } else {
                Quat::IDENTITY
            };
            for (j, (b, g)) in base.iter().zip(&globals).take(20).enumerate() {
                let want = turn * b.w_axis.truncate();
                let got = g.w_axis.truncate();
                assert!((want - got).length() < 1e-4, "{rig:?} joint {j}: {want} vs {got}");
            }
        }
        assert!(
            demo_humanoid(&DemoRig {
                size: 0.0,
                ..DemoRig::default()
            })
            .is_none()
        );
    }

    #[test]
    fn demo_walk_plants_the_feet() {
        let sk = demo_humanoid(&DemoRig::default()).expect("plain");
        let clips = demo_locomotion(&sk, &DemoGait::standard());
        let walk = &clips[1];
        // The left foot is planted for the first 60 % of the cycle: its world position holds.
        let mut pose = sk.bind_pose_pose();
        let mut first = None;
        for k in 0..=15 {
            let t = k as f32 / 30.0;
            pose.copy_from(&sk.bind_pose_pose());
            walk.sample_pose(t, &mut pose);
            let foot = sk.globals(&pose, None)[14].w_axis.truncate();
            let first = *first.get_or_insert(foot);
            assert!((foot - first).length() < 0.01, "t {t}: {foot} vs {first}");
        }
        let (delta, _) = walk
            .root_motion_delta_full(0.0, walk.duration, false)
            .expect("root motion");
        assert!((delta.z + 1.3).abs() < 1e-3, "{delta}");
    }

    /// audit-v034b-conc M2: a skeleton without the leg joints (or with coincident leg joints)
    /// gave zero-length thigh and calf, and `f32::clamp` panicked on its inverted bounds.
    #[test]
    fn locomotion_on_a_short_or_degenerate_rig_does_not_panic() {
        let short = Skeleton::from_local_bind(
            vec!["root".into(), "hips".into()],
            vec![None, Some(0)],
            vec![LocalBind::default(); 2],
        )
        .expect("two joints");
        let clips = demo_locomotion(&short, &DemoGait::standard());
        assert_eq!(clips.len(), DemoGait::standard().len());
        let n = 15;
        let coincident = Skeleton::from_local_bind(
            (0..n).map(|i| format!("j{i}")).collect(),
            (0..n)
                .map(|i: usize| i.checked_sub(1).map(|p| p as u32))
                .collect(),
            vec![LocalBind::default(); n],
        )
        .expect("fifteen joints at one point");
        for clip in demo_locomotion(&coincident, &DemoGait::standard()) {
            let mut pose = coincident.bind_pose_pose();
            clip.sample_pose(clip.duration * 0.5, &mut pose);
            let globals = coincident.globals(&pose, None);
            assert!(
                globals.iter().all(|m| m.is_finite()),
                "{}: finite pose",
                clip.name
            );
        }
        assert_eq!(leg_angles(Vec3::ZERO, Vec3::NEG_Y, 0.0, 0.0), (0.0, 0.0));
        assert_eq!(leg_angles(Vec3::ZERO, Vec3::NEG_Y, f32::NAN, 0.4), (0.0, 0.0));
    }
}
