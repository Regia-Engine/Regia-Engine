// SPDX-License-Identifier: MPL-2.0
//! Read helpers for the retargeting editor (ADR-7801, `docs/specs/85-retarget-crowds.md` § 2.5):
//! the humanoid roles grouped into body chains, the rest-offset angle conversions the runtime uses,
//! and the bind-pose bone directions a chain alignment is measured on.
//!
//! Nothing here changes how a [`Retargeter`] transfers motion: the editor reads skeletons and
//! [`RetargetSetup`](crate::RetargetSetup) documents through these helpers and saves its edits as
//! the existing `.retarget.ron` (format v1) fields.

use regia_core::math::{EulerRot, Mat4, Quat, Vec3};

use crate::num::det_rotation_y;
use crate::retarget::{HumanoidBone, Retargeter, euler_degrees, position_of};
use crate::skeleton::Skeleton;

/// A body chain: the humanoid roles the retargeting editor lists, maps and aligns together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RetargetChain {
    /// The hips (the root of the body; carries the translation).
    Root,
    /// Spine, chest and upper chest.
    Spine,
    /// Neck and head.
    Head,
    /// Left clavicle, upper arm, forearm and hand.
    LeftArm,
    /// Right clavicle, upper arm, forearm and hand.
    RightArm,
    /// Left thigh, shin, foot and toes.
    LeftLeg,
    /// Right thigh, shin, foot and toes.
    RightLeg,
}

/// A reference pose a chain can be aligned to ([`chain_alignment_offset`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferencePose {
    /// Arms straight out to the sides, level with the shoulders.
    TPose,
    /// Arms 45 degrees below level.
    APose,
}

impl ReferencePose {
    /// The stable name (`"t_pose"`, `"a_pose"`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ReferencePose::TPose => "t_pose",
            ReferencePose::APose => "a_pose",
        }
    }

    /// The pose called `name` ([`Self::name`]).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "t_pose" => Some(ReferencePose::TPose),
            "a_pose" => Some(ReferencePose::APose),
            _ => None,
        }
    }
}

impl RetargetChain {
    /// Every chain, in display order.
    pub const ALL: [RetargetChain; 7] = [
        RetargetChain::Root,
        RetargetChain::Spine,
        RetargetChain::Head,
        RetargetChain::LeftArm,
        RetargetChain::RightArm,
        RetargetChain::LeftLeg,
        RetargetChain::RightLeg,
    ];

    /// The stable name (`"root"`, `"spine"`, `"head"`, `"left_arm"`, `"right_arm"`, `"left_leg"`,
    /// `"right_leg"`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            RetargetChain::Root => "root",
            RetargetChain::Spine => "spine",
            RetargetChain::Head => "head",
            RetargetChain::LeftArm => "left_arm",
            RetargetChain::RightArm => "right_arm",
            RetargetChain::LeftLeg => "left_leg",
            RetargetChain::RightLeg => "right_leg",
        }
    }

    /// The display label (`"Left arm"`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            RetargetChain::Root => "Root",
            RetargetChain::Spine => "Spine",
            RetargetChain::Head => "Head",
            RetargetChain::LeftArm => "Left arm",
            RetargetChain::RightArm => "Right arm",
            RetargetChain::LeftLeg => "Left leg",
            RetargetChain::RightLeg => "Right leg",
        }
    }

    /// The chain called `name` ([`Self::name`]).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The chain's roles, from the body outwards.
    #[must_use]
    pub fn roles(self) -> &'static [HumanoidBone] {
        match self {
            RetargetChain::Root => &[HumanoidBone::Hips],
            RetargetChain::Spine => &[HumanoidBone::Spine, HumanoidBone::Chest, HumanoidBone::UpperChest],
            RetargetChain::Head => &[HumanoidBone::Neck, HumanoidBone::Head],
            RetargetChain::LeftArm => &[
                HumanoidBone::LeftShoulder,
                HumanoidBone::LeftUpperArm,
                HumanoidBone::LeftLowerArm,
                HumanoidBone::LeftHand,
            ],
            RetargetChain::RightArm => &[
                HumanoidBone::RightShoulder,
                HumanoidBone::RightUpperArm,
                HumanoidBone::RightLowerArm,
                HumanoidBone::RightHand,
            ],
            RetargetChain::LeftLeg => &[
                HumanoidBone::LeftUpperLeg,
                HumanoidBone::LeftLowerLeg,
                HumanoidBone::LeftFoot,
                HumanoidBone::LeftToes,
            ],
            RetargetChain::RightLeg => &[
                HumanoidBone::RightUpperLeg,
                HumanoidBone::RightLowerLeg,
                HumanoidBone::RightFoot,
                HumanoidBone::RightToes,
            ],
        }
    }

    /// The two roles whose bone (first joint to second joint) is the chain's aim: the upper arm
    /// to the forearm, the thigh to the shin, the spine to the chest, the neck to the head. `None`
    /// for the root.
    #[must_use]
    pub fn aim_roles(self) -> Option<(HumanoidBone, HumanoidBone)> {
        match self {
            RetargetChain::Root => None,
            RetargetChain::Spine => Some((HumanoidBone::Spine, HumanoidBone::Chest)),
            RetargetChain::Head => Some((HumanoidBone::Neck, HumanoidBone::Head)),
            RetargetChain::LeftArm => Some((HumanoidBone::LeftUpperArm, HumanoidBone::LeftLowerArm)),
            RetargetChain::RightArm => Some((HumanoidBone::RightUpperArm, HumanoidBone::RightLowerArm)),
            RetargetChain::LeftLeg => Some((HumanoidBone::LeftUpperLeg, HumanoidBone::LeftLowerLeg)),
            RetargetChain::RightLeg => Some((HumanoidBone::RightUpperLeg, HumanoidBone::RightLowerLeg)),
        }
    }
}

impl HumanoidBone {
    /// The role called `name` ([`HumanoidBone::name`]), or `None` for a custom role.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        HumanoidBone::ALL.into_iter().find(|r| r.name() == name)
    }

    /// The body chain this role belongs to.
    #[must_use]
    pub fn chain(self) -> RetargetChain {
        RetargetChain::ALL
            .into_iter()
            .find(|c| c.roles().contains(&self))
            .unwrap_or(RetargetChain::Root)
    }
}

/// The rotation a `rest_offset_deg` value stands for: XYZ Euler angles in degrees, object space,
/// composed exactly as the [`Retargeter`] composes them.
#[must_use]
pub fn rest_offset_rotation(degrees: Vec3) -> Quat {
    euler_degrees(degrees)
}

/// The `rest_offset_deg` value (XYZ Euler degrees) of `rotation`; the inverse of
/// [`rest_offset_rotation`] up to float rounding. Values within 1e-4 degrees of zero are written
/// as zero so an identity offset saves as `(0.0, 0.0, 0.0)`.
#[must_use]
pub fn rest_offset_degrees(rotation: Quat) -> Vec3 {
    let q = rotation.normalize();
    if !q.is_finite() {
        return Vec3::ZERO;
    }
    let (x, y, z) = q.to_euler(EulerRot::XYZ);
    let clean = |v: f32| {
        let d = v.to_degrees();
        if d.abs() < 1e-4 { 0.0 } else { d }
    };
    Vec3::new(clean(x), clean(y), clean(z))
}

/// The bind-pose object-space direction from joint `from` to joint `to` of `skeleton`; `None`
/// when either index is out of range or the joints coincide.
#[must_use]
pub fn bind_bone_direction(skeleton: &Skeleton, from: u32, to: u32) -> Option<Vec3> {
    let globals = skeleton.bind_globals();
    let a = position_of(*globals.get(from as usize)?);
    let b = position_of(*globals.get(to as usize)?);
    let d = b - a;
    (d.is_finite() && d.length_squared() > 1e-12).then(|| d.normalize())
}

/// A rig's forward direction on the ground plane: `left × up` where `left` runs from the right
/// thigh (or upper arm) to the left one. The same rule the [`Retargeter`] uses for its facing turn.
fn forward_of(globals: &[Mat4], left: Option<u32>, right: Option<u32>) -> Option<Vec3> {
    let l = position_of(*globals.get(left? as usize)?);
    let r = position_of(*globals.get(right? as usize)?);
    let f = (l - r).cross(Vec3::Y);
    let f = Vec3::new(f.x, 0.0, f.z);
    (f.is_finite() && f.length_squared() > 1e-8).then(|| f.normalize())
}

/// The rest offset that aligns `chain` to `pose` under `retargeter`: with it set on every role of
/// the chain, the target's aim bone ([`RetargetChain::aim_roles`]) points along the reference
/// direction while the source stands in its bind pose. The reference directions are in the
/// target's object space: arms out to the target's sides (level for a T-pose, 45 degrees down for
/// an A-pose), legs straight down, spine and head straight up.
///
/// The automatic rest correction already turns each target bone onto the source's bind direction;
/// this offset turns that direction onto the reference one, so a source whose bind pose is not the
/// reference pose (an A-pose capture driving a T-pose rig, or the other way round) can be shown in
/// the reference pose at rest.
///
/// `None` for the root, or when the chain's aim roles are not both mapped on both rigs (or the
/// target has no left/right pair to find its sides from, for the arms).
#[must_use]
pub fn chain_alignment_offset(
    retargeter: &Retargeter,
    chain: RetargetChain,
    pose: ReferencePose,
) -> Option<Quat> {
    let (from, to) = chain.aim_roles()?;
    let target = retargeter.target();
    let role_joint = |role: HumanoidBone| -> Option<u32> {
        (0..target.len() as u32).find(|&t| retargeter.role_of(t) == Some(role.name()))
    };
    let (t0, t1) = (role_joint(from)?, role_joint(to)?);
    let (s0, s1) = (retargeter.source_of(t0)?, retargeter.source_of(t1)?);
    let facing = det_rotation_y(retargeter.facing_yaw());
    let source_dir = facing * bind_bone_direction(retargeter.source(), s0, s1)?;
    let wanted = match chain {
        RetargetChain::Root => return None,
        RetargetChain::Spine | RetargetChain::Head => Vec3::Y,
        RetargetChain::LeftLeg | RetargetChain::RightLeg => Vec3::NEG_Y,
        RetargetChain::LeftArm | RetargetChain::RightArm => {
            let globals = target.bind_globals();
            let pair = |l: HumanoidBone, r: HumanoidBone| forward_of(&globals, role_joint(l), role_joint(r));
            let forward = pair(HumanoidBone::LeftUpperLeg, HumanoidBone::RightUpperLeg)
                .or_else(|| pair(HumanoidBone::LeftUpperArm, HumanoidBone::RightUpperArm))?;
            // left = up × forward (a rig facing −Z has its left at −X).
            let left = Vec3::Y.cross(forward).normalize();
            let side = if chain == RetargetChain::LeftArm {
                left
            } else {
                -left
            };
            match pose {
                ReferencePose::TPose => side,
                ReferencePose::APose => (side - Vec3::Y).normalize(),
            }
        }
    };
    Some(Quat::from_rotation_arc(source_dir, wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_has_one_chain_and_chains_round_trip_by_name() {
        for role in HumanoidBone::ALL {
            let owners = RetargetChain::ALL
                .iter()
                .filter(|c| c.roles().contains(&role))
                .count();
            assert_eq!(owners, 1, "{role:?}");
            assert_eq!(HumanoidBone::from_name(role.name()), Some(role));
            assert!(role.chain().roles().contains(&role));
        }
        for chain in RetargetChain::ALL {
            assert_eq!(RetargetChain::parse(chain.name()), Some(chain));
        }
        assert_eq!(HumanoidBone::from_name("tail1"), None);
        assert_eq!(ReferencePose::parse("a_pose"), Some(ReferencePose::APose));
    }

    #[test]
    fn rest_offset_degrees_inverts_the_runtime_composition() {
        for deg in [
            Vec3::ZERO,
            Vec3::new(0.0, 0.0, -45.0),
            Vec3::new(10.0, 20.0, 30.0),
            Vec3::new(-80.0, 5.0, 170.0),
        ] {
            let q = rest_offset_rotation(deg);
            let back = rest_offset_rotation(rest_offset_degrees(q));
            assert!(q.dot(back).abs() > 1.0 - 1e-6, "{deg}: {q} vs {back}");
        }
        assert_eq!(rest_offset_degrees(Quat::IDENTITY), Vec3::ZERO);
    }
}
