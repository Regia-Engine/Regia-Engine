// SPDX-License-Identifier: MPL-2.0
//! Twist distribution and helper bones (Tincal C5).
//!
//! A forearm that follows the hand's roll with one joint pinches ("candy wrapper") where the
//! mesh meets the wrist. The usual remedy is a chain of **twist joints** along the bone that
//! each carry a fraction of the roll: [`TwistChain`] measures the roll of a source joint about
//! its bone axis (a swing-twist decomposition of the rotation away from the bind pose) and
//! distributes it over the driven joints by weight. [`HelperJoint`] is the procedural
//! counterpart for corrective joints (a shoulder-pad follower, a knee cap): it follows a
//! fraction of a source joint's rotation and can slide along an authored offset curve.
//!
//! Both are pure functions over a local [`Pose`]; [`crate::driver::CompiledRig`] runs them
//! before the drivers, and the `Skeleton`-name resolution happens once in
//! [`CompiledTwist::compile`] / [`CompiledHelper::compile`].
//!
//! Provenance (`docs/provenance.md`): swing-twist decomposition of a unit quaternion about an
//! axis is textbook (Dobrowolski, "Swing-twist decomposition in Clifford algebra", 2015 and
//! the Quaternion literature it cites); twist-bone weight distribution is the common
//! rigging practice of spreading the end joint's roll along the limb.

use regia_core::math::{Quat, Vec3, det};
use serde::{Deserialize, Serialize};

use crate::pose::Pose;
use crate::skeleton::Skeleton;

fn y_axis() -> Vec3 {
    Vec3::Y
}

fn one() -> f32 {
    1.0
}

/// One joint that receives a fraction of a [`TwistChain`]'s roll.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TwistTarget {
    /// Joint name.
    pub joint: String,
    /// Fraction of the source roll this joint takes (`0..=1` typically; `0.5` is half).
    pub weight: f32,
    /// Roll axis in this joint's local frame; the chain's axis when `None`.
    #[serde(default)]
    pub axis: Option<Vec3>,
}

/// Distributes the roll of one joint over several driven joints.
///
/// The roll is the **twist** of the source's local rotation delta (bind-relative) about
/// `axis`. Each target's local rotation is post-multiplied by a rotation about its own axis
/// by `weight * twist`. When the driven joints sit *between* the source and its parent
/// (forearm twist joints that are the hand's ancestors), set `source_keep` below one so the
/// source's own roll shrinks to `source_keep * twist` and the total roll at the end of the
/// chain is unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TwistChain {
    /// Name for the agent and the editor (`"forearm_l"`).
    pub name: String,
    /// Joint whose roll is distributed (the hand for a forearm, the upper arm for an upper arm).
    pub source: String,
    /// Bone axis in the source's local frame (default +Y, the glTF/VRM convention).
    #[serde(default = "y_axis")]
    pub axis: Vec3,
    /// Joints that take a share of the roll.
    pub driven: Vec<TwistTarget>,
    /// Fraction of its own roll the source keeps (default 1: unchanged).
    #[serde(default = "one")]
    pub source_keep: f32,
}

/// An offset key of a [`HelperJoint`]: the translation to add when the source has rotated
/// `angle_deg` away from its bind rotation.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelperOffsetKey {
    /// Total rotation angle of the source away from its bind rotation, degrees.
    pub angle_deg: f32,
    /// Local translation added to the helper at that angle, metres.
    pub offset: Vec3,
}

/// A corrective helper joint driven by another joint's rotation.
///
/// The helper's local rotation takes the fraction `follow` of the source's bind-relative
/// rotation delta (slerp from identity), so a helper with `follow: 0.5` bisects the source's
/// motion; `offset` slides the helper along a piecewise-linear path keyed on the source's
/// total rotation angle (clamped outside the keys). Source and helper are assumed to share
/// their local frame orientation (true for rigs where helpers are authored as children of the
/// source's parent with the same axes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelperJoint {
    /// Name for the agent and the editor.
    pub name: String,
    /// The helper joint that is posed.
    pub joint: String,
    /// The joint whose rotation drives the helper.
    pub source: String,
    /// Fraction of the source's rotation delta the helper copies.
    #[serde(default)]
    pub follow: f32,
    /// Translation offsets keyed on the source's rotation angle; sorted by angle on compile.
    #[serde(default)]
    pub offset: Vec<HelperOffsetKey>,
}

/// Swing-twist decomposition: `q = swing * twist`, with `twist` a rotation about `axis` (unit)
/// by the returned signed angle in radians (`-π..=π`). The swing is the remainder.
#[must_use]
pub fn swing_twist(q: Quat, axis: Vec3) -> (Quat, f32) {
    // Take the short way round so the angle stays in -π..=π.
    let q = if q.w < 0.0 { -q } else { q };
    let along = q.x * axis.x + q.y * axis.y + q.z * axis.z;
    let angle = 2.0 * det::atan2(along, q.w);
    let twist = det::quat_from_axis_angle(axis, angle);
    ((q * twist.inverse()).normalize(), angle)
}

/// Total rotation angle of `q` in radians (`0..=π`).
#[must_use]
pub fn rotation_angle(q: Quat) -> f32 {
    2.0 * det::acos(q.w.abs().min(1.0))
}

/// A [`TwistChain`] with joint names resolved.
#[derive(Clone, Debug)]
pub struct CompiledTwist {
    /// Chain name.
    pub name: String,
    source: usize,
    axis: Vec3,
    driven: Vec<(usize, f32, Vec3)>,
    source_keep: f32,
    bind_rotation: Quat,
}

impl CompiledTwist {
    /// Resolve `chain` against `skeleton`.
    ///
    /// # Errors
    ///
    /// The names that are not joints, or a zero axis.
    pub fn compile(chain: &TwistChain, skeleton: &Skeleton) -> Result<Self, Vec<String>> {
        let mut problems = Vec::new();
        let mut find = |joint: &str| {
            let found = skeleton.joint_index(joint).map(|j| j as usize);
            if found.is_none() {
                problems.push(format!("twist {:?}: no joint {joint:?}", chain.name));
            }
            found
        };
        let source = find(&chain.source);
        let driven: Vec<Option<(usize, f32, Option<Vec3>)>> = chain
            .driven
            .iter()
            .map(|t| find(&t.joint).map(|j| (j, t.weight, t.axis)))
            .collect();
        if !chain.source_keep.is_finite() {
            problems.push(format!("twist {:?}: source_keep must be finite", chain.name));
        }
        if chain.driven.iter().any(|t| !t.weight.is_finite()) {
            problems.push(format!("twist {:?}: a target weight must be finite", chain.name));
        }
        let Some(axis) = chain.axis.try_normalize() else {
            problems.push(format!("twist {:?}: axis must not be zero", chain.name));
            return Err(problems);
        };
        let mut resolved = Vec::new();
        for entry in driven.into_iter().flatten() {
            let axis_of = match entry.2 {
                Some(a) => match a.try_normalize() {
                    Some(a) => a,
                    None => {
                        problems.push(format!("twist {:?}: a target axis is zero", chain.name));
                        continue;
                    }
                },
                None => axis,
            };
            resolved.push((entry.0, entry.1, axis_of));
        }
        match (source, problems.is_empty()) {
            (Some(source), true) => Ok(CompiledTwist {
                name: chain.name.clone(),
                source,
                axis,
                driven: resolved,
                source_keep: chain.source_keep,
                bind_rotation: skeleton.bind_pose[source].rotation,
            }),
            _ => Err(problems),
        }
    }

    /// Apply the chain to `pose`; returns the measured source roll in degrees.
    pub fn apply(&self, pose: &mut Pose) -> f32 {
        if self.source >= pose.len() {
            return 0.0;
        }
        let delta = self.bind_rotation.inverse() * pose.rotations[self.source];
        let (swing, angle) = swing_twist(delta, self.axis);
        for &(joint, weight, axis) in &self.driven {
            if joint < pose.len() {
                pose.rotations[joint] =
                    (pose.rotations[joint] * det::quat_from_axis_angle(axis, angle * weight)).normalize();
            }
        }
        if (self.source_keep - 1.0).abs() > f32::EPSILON {
            let kept = det::quat_from_axis_angle(self.axis, angle * self.source_keep);
            pose.rotations[self.source] = (self.bind_rotation * swing * kept).normalize();
        }
        angle.to_degrees()
    }
}

/// A [`HelperJoint`] with joint names resolved.
#[derive(Clone, Debug)]
pub struct CompiledHelper {
    /// Helper name.
    pub name: String,
    joint: usize,
    source: usize,
    follow: f32,
    offset: Vec<HelperOffsetKey>,
    source_bind: Quat,
}

impl CompiledHelper {
    /// Resolve `helper` against `skeleton`.
    ///
    /// # Errors
    ///
    /// The names that are not joints, or a helper that drives itself.
    pub fn compile(helper: &HelperJoint, skeleton: &Skeleton) -> Result<Self, Vec<String>> {
        let joint = skeleton.joint_index(&helper.joint).map(|j| j as usize);
        let source = skeleton.joint_index(&helper.source).map(|j| j as usize);
        let mut problems = Vec::new();
        if joint.is_none() {
            problems.push(format!("helper {:?}: no joint {:?}", helper.name, helper.joint));
        }
        if source.is_none() {
            problems.push(format!("helper {:?}: no joint {:?}", helper.name, helper.source));
        }
        if joint.is_some() && joint == source {
            problems.push(format!("helper {:?}: a joint cannot drive itself", helper.name));
        }
        if !helper.follow.is_finite() {
            problems.push(format!("helper {:?}: follow must be finite", helper.name));
        }
        if helper
            .offset
            .iter()
            .any(|k| !k.angle_deg.is_finite() || !k.offset.is_finite())
        {
            problems.push(format!("helper {:?}: offset keys must be finite", helper.name));
        }
        let (Some(joint), Some(source), true) = (joint, source, problems.is_empty()) else {
            return Err(problems);
        };
        let mut offset = helper.offset.clone();
        offset.sort_by(|a, b| a.angle_deg.total_cmp(&b.angle_deg));
        Ok(CompiledHelper {
            name: helper.name.clone(),
            joint,
            source,
            follow: helper.follow,
            offset,
            source_bind: skeleton.bind_pose[source].rotation,
        })
    }

    fn offset_at(&self, angle_deg: f32) -> Vec3 {
        let (Some(first), Some(last)) = (self.offset.first(), self.offset.last()) else {
            return Vec3::ZERO;
        };
        if angle_deg <= first.angle_deg {
            return first.offset;
        }
        if angle_deg >= last.angle_deg {
            return last.offset;
        }
        let end = self.offset.partition_point(|k| k.angle_deg <= angle_deg);
        let (a, b) = (self.offset[end - 1], self.offset[end]);
        let span = b.angle_deg - a.angle_deg;
        if span <= f32::EPSILON {
            return b.offset;
        }
        a.offset.lerp(b.offset, (angle_deg - a.angle_deg) / span)
    }

    /// Apply the helper to `pose`; returns the source's rotation away from bind in degrees.
    pub fn apply(&self, pose: &mut Pose) -> f32 {
        if self.source >= pose.len() || self.joint >= pose.len() {
            return 0.0;
        }
        let delta = self.source_bind.inverse() * pose.rotations[self.source];
        let delta = if delta.w < 0.0 { -delta } else { delta };
        let angle = rotation_angle(delta).to_degrees();
        let followed = Quat::IDENTITY.slerp(delta, self.follow);
        pose.rotations[self.joint] = (pose.rotations[self.joint] * followed).normalize();
        pose.translations[self.joint] += self.offset_at(angle);
        angle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::LocalBind;

    /// root → upper → fore → twist1, twist2 (children of fore) and hand (child of fore).
    fn arm() -> Skeleton {
        let mk = |y: f32| LocalBind {
            translation: Vec3::new(0.0, y, 0.0),
            ..LocalBind::default()
        };
        Skeleton::from_local_bind(
            ["root", "upper", "fore", "twist1", "twist2", "hand"]
                .map(String::from)
                .to_vec(),
            vec![None, Some(0), Some(1), Some(2), Some(2), Some(2)],
            vec![mk(0.0), mk(0.3), mk(0.3), mk(0.1), mk(0.2), mk(0.3)],
        )
        .expect("valid")
    }

    fn chain(keep: f32) -> TwistChain {
        TwistChain {
            name: "fore".into(),
            source: "hand".into(),
            axis: Vec3::Y,
            driven: vec![
                TwistTarget {
                    joint: "twist1".into(),
                    weight: 1.0 / 3.0,
                    axis: None,
                },
                TwistTarget {
                    joint: "twist2".into(),
                    weight: 2.0 / 3.0,
                    axis: None,
                },
            ],
            source_keep: keep,
        }
    }

    #[test]
    fn swing_twist_recovers_the_roll_and_the_remainder() {
        let roll = Quat::from_axis_angle(Vec3::Y, 1.0);
        let bend = Quat::from_axis_angle(Vec3::X, 0.4);
        let (swing, angle) = swing_twist(bend * roll, Vec3::Y);
        assert!((angle - 1.0).abs() < 1e-5, "{angle}");
        assert!(swing.angle_between(bend) < 1e-5);
        // A pure bend has no roll; a rotation the long way round wraps.
        assert!(swing_twist(bend, Vec3::Y).1.abs() < 1e-6);
        let (_, wrapped) = swing_twist(-roll, Vec3::Y);
        assert!((wrapped - 1.0).abs() < 1e-5);
    }

    #[test]
    fn twist_is_distributed_by_weight() {
        let skeleton = arm();
        let compiled = CompiledTwist::compile(&chain(1.0), &skeleton).expect("compiles");
        let mut pose = skeleton.bind_pose_pose();
        pose.rotations[5] = Quat::from_axis_angle(Vec3::Y, 1.2);
        let degrees = compiled.apply(&mut pose);
        assert!((degrees - 1.2f32.to_degrees()).abs() < 1e-3);
        let (_, t1) = swing_twist(pose.rotations[3], Vec3::Y);
        let (_, t2) = swing_twist(pose.rotations[4], Vec3::Y);
        assert!((t1 - 0.4).abs() < 1e-5 && (t2 - 0.8).abs() < 1e-5, "{t1} {t2}");
        // The source keeps its roll by default.
        assert!(pose.rotations[5].angle_between(Quat::from_axis_angle(Vec3::Y, 1.2)) < 1e-5);
    }

    #[test]
    fn source_keep_removes_the_distributed_share_but_keeps_the_swing() {
        let skeleton = arm();
        let compiled = CompiledTwist::compile(&chain(0.25), &skeleton).expect("compiles");
        let bend = Quat::from_axis_angle(Vec3::X, 0.5);
        let mut pose = skeleton.bind_pose_pose();
        pose.rotations[5] = bend * Quat::from_axis_angle(Vec3::Y, 1.2);
        compiled.apply(&mut pose);
        let (swing, roll) = swing_twist(pose.rotations[5], Vec3::Y);
        assert!((roll - 0.3).abs() < 1e-5, "{roll}");
        assert!(swing.angle_between(bend) < 1e-5);
    }

    #[test]
    fn an_unrolled_source_changes_nothing() {
        let skeleton = arm();
        let compiled = CompiledTwist::compile(&chain(0.5), &skeleton).expect("compiles");
        let mut pose = skeleton.bind_pose_pose();
        let before = pose.clone();
        compiled.apply(&mut pose);
        for (a, b) in pose.rotations.iter().zip(&before.rotations) {
            assert!(a.angle_between(*b) < 1e-6);
        }
    }

    #[test]
    fn unknown_joints_are_all_reported() {
        let skeleton = arm();
        let mut bad = chain(1.0);
        bad.source = "nope".into();
        bad.driven[0].joint = "nada".into();
        let problems = CompiledTwist::compile(&bad, &skeleton).expect_err("fails");
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn helper_follows_a_fraction_and_slides_along_its_offset() {
        let skeleton = arm();
        let helper = HelperJoint {
            name: "pad".into(),
            joint: "twist1".into(),
            source: "upper".into(),
            follow: 0.5,
            offset: vec![
                HelperOffsetKey {
                    angle_deg: 90.0,
                    offset: Vec3::new(0.0, 0.0, 0.04),
                },
                HelperOffsetKey {
                    angle_deg: 0.0,
                    offset: Vec3::ZERO,
                },
            ],
        };
        let compiled = CompiledHelper::compile(&helper, &skeleton).expect("compiles");
        let mut pose = skeleton.bind_pose_pose();
        pose.rotations[1] = Quat::from_axis_angle(Vec3::X, 90f32.to_radians());
        let angle = compiled.apply(&mut pose);
        assert!((angle - 90.0).abs() < 1e-3);
        let expected = Quat::from_axis_angle(Vec3::X, 45f32.to_radians());
        assert!(pose.rotations[3].angle_between(expected) < 1e-4);
        assert!((pose.translations[3].z - 0.04).abs() < 1e-5);
        // Halfway along the offset path at 45 degrees.
        pose = skeleton.bind_pose_pose();
        pose.rotations[1] = Quat::from_axis_angle(Vec3::X, 45f32.to_radians());
        compiled.apply(&mut pose);
        assert!((pose.translations[3].z - 0.02).abs() < 1e-5);
    }

    #[test]
    fn a_helper_cannot_drive_itself() {
        let skeleton = arm();
        let helper = HelperJoint {
            name: "loop".into(),
            joint: "upper".into(),
            source: "upper".into(),
            follow: 1.0,
            offset: Vec::new(),
        };
        assert!(CompiledHelper::compile(&helper, &skeleton).is_err());
    }
}
