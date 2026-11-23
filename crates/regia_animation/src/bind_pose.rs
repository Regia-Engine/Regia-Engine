// SPDX-License-Identifier: MPL-2.0
//! Per-instance bind poses (Tincal C9).
//!
//! Specification: `docs/specs/17-animation.md` § 3b. Many avatars share one [`Skeleton`] asset
//! and one clip set, but each has its own proportions (bone lengths from a body refit). The
//! instance data lives in an [`InstanceBindPose`] component on the rig entity, never in the
//! shared skeleton:
//!
//! - the instance's **rest pose** (local bind per joint) and **inverse bind matrices** (the
//!   frame its baked mesh is skinned in);
//! - [`retarget_to_instance`] moves a pose authored against the shared skeleton onto the
//!   instance's rest: rotations keep their offset from the rest rotation, translations keep
//!   their offset from the rest translation scaled by the bone-length ratio (so a hip bob or a
//!   stride scales with the legs), scales keep their ratio.
//!
//! The animator, graph, IK and inertialization keep working in the shared skeleton's space;
//! only [`crate::systems::apply_animation`] retargets the final pose, then builds the joint
//! transforms, joint globals and the [`crate::SkinPalette`] (`global × instance inverse bind`)
//! from the instance. The renderer already consumes a per-rig palette, so no render change is
//! needed, and the data does not collide with `RenderOverride` or the material instance data.
//!
//! Provenance (`docs/provenance.md`): rest-pose-relative retargeting (delta from the rest
//! pose, translation scaled by bone length) is the standard approach of animation retargeters
//! (for example the "retarget by rest-pose deltas" description in Hecker et al., "Real-time
//! Motion Retargeting to Highly Varied User-Created Morphologies", SIGGRAPH 2008); the
//! refit-from-joint-positions constructor is our own.

use std::collections::HashMap;

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;

use crate::pose::Pose;
use crate::skeleton::{LocalBind, Skeleton};

/// Errors building an [`InstanceBindPose`].
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BindPoseError {
    /// An array disagrees with the shared skeleton's joint count.
    #[error("{what} has {got} entries, the skeleton has {expected} joints")]
    WrongLength {
        /// Which array.
        what: &'static str,
        /// Entries given.
        got: usize,
        /// Joints in the shared skeleton.
        expected: usize,
    },
    /// A joint name is not in the skeleton.
    #[error("joint {0:?} is not in the skeleton")]
    UnknownJoint(String),
    /// A rest value is not finite or a scale is not positive.
    #[error("joint {joint} has a non-finite or non-positive rest value")]
    BadValue {
        /// The joint id.
        joint: usize,
    },
}

/// One avatar's own rest skeleton over a shared [`Skeleton`] (same joints and hierarchy).
#[derive(Component, Clone, Debug, PartialEq)]
pub struct InstanceBindPose {
    /// The instance's local rest pose per joint.
    pub bind_pose: Vec<LocalBind>,
    /// The instance's inverse bind matrices (its baked mesh's skinning frame).
    pub inverse_bind_matrices: Vec<Mat4>,
    /// Per joint: instance bone length over shared bone length (1 where the shared bone has no
    /// length, such as a root).
    pub length_ratio: Vec<f32>,
}

fn check_len(what: &'static str, got: usize, expected: usize) -> Result<(), BindPoseError> {
    if got == expected {
        Ok(())
    } else {
        Err(BindPoseError::WrongLength { what, got, expected })
    }
}

impl InstanceBindPose {
    /// An instance rest pose over `shared`; the inverse binds come from forward kinematics of
    /// `bind_pose`.
    ///
    /// # Errors
    ///
    /// [`BindPoseError`] when the length disagrees (`bind_pose`, or the shared skeleton's own
    /// `parents` list when it skipped [`Skeleton::validate`]), or a value is non-finite or a
    /// scale is not positive.
    pub fn new(shared: &Skeleton, bind_pose: Vec<LocalBind>) -> Result<Self, BindPoseError> {
        check_len("parents", shared.parents.len(), shared.len())?;
        check_len("bind_pose", bind_pose.len(), shared.len())?;
        for (joint, bind) in bind_pose.iter().enumerate() {
            let finite = bind.translation.is_finite()
                && bind.rotation.is_finite()
                && bind.scale.is_finite()
                && bind.scale.min_element() > 0.0;
            if !finite {
                return Err(BindPoseError::BadValue { joint });
            }
        }
        let pose = Pose {
            translations: bind_pose.iter().map(|b| b.translation).collect(),
            rotations: bind_pose.iter().map(|b| b.rotation.normalize()).collect(),
            scales: bind_pose.iter().map(|b| b.scale).collect(),
        };
        let inverse_bind_matrices = shared.globals(&pose, None).iter().map(|m| m.inverse()).collect();
        let length_ratio = bind_pose
            .iter()
            .zip(&shared.bind_pose)
            .map(|(inst, base)| {
                let base_len = base.translation.length();
                if base_len > 1e-5 {
                    inst.translation.length() / base_len
                } else {
                    1.0
                }
            })
            .collect();
        Ok(InstanceBindPose {
            bind_pose,
            inverse_bind_matrices,
            length_ratio,
        })
    }

    /// Replace the inverse binds with the ones the instance's baked mesh was skinned with.
    ///
    /// # Errors
    ///
    /// [`BindPoseError::WrongLength`] when the count differs from the rest pose.
    pub fn with_inverse_binds(mut self, inverse_bind_matrices: Vec<Mat4>) -> Result<Self, BindPoseError> {
        check_len(
            "inverse_bind_matrices",
            inverse_bind_matrices.len(),
            self.bind_pose.len(),
        )?;
        self.inverse_bind_matrices = inverse_bind_matrices;
        Ok(self)
    }

    /// Scale every bone length by `factor` (a uniformly taller or shorter body).
    ///
    /// # Errors
    ///
    /// [`BindPoseError::BadValue`] (joint 0) when `factor` is not finite and positive; otherwise
    /// as [`InstanceBindPose::new`].
    pub fn uniform(shared: &Skeleton, factor: f32) -> Result<Self, BindPoseError> {
        // 0 collapses every bone and a negative factor mirrors them.
        if !(factor.is_finite() && factor > 0.0) {
            return Err(BindPoseError::BadValue { joint: 0 });
        }
        let bind = shared
            .bind_pose
            .iter()
            .map(|b| LocalBind {
                translation: b.translation * factor,
                ..*b
            })
            .collect();
        Self::new(shared, bind)
    }

    /// Scale individual bones: `scales` maps a joint name to the factor for that joint's local
    /// translation (the bone leading *to* the joint). Unlisted joints keep the shared length.
    ///
    /// # Errors
    ///
    /// [`BindPoseError::UnknownJoint`] for a name not in the skeleton,
    /// [`BindPoseError::BadValue`] for a factor that is not finite and positive; otherwise as
    /// [`InstanceBindPose::new`].
    pub fn from_bone_scales(shared: &Skeleton, scales: &HashMap<String, f32>) -> Result<Self, BindPoseError> {
        let mut bind = shared.bind_pose.clone();
        for (name, factor) in scales {
            let joint = shared
                .joint_index(name)
                .ok_or_else(|| BindPoseError::UnknownJoint(name.clone()))? as usize;
            if !(factor.is_finite() && *factor > 0.0) {
                return Err(BindPoseError::BadValue { joint });
            }
            bind[joint].translation *= *factor;
        }
        Self::new(shared, bind)
    }

    /// Refit to object-space joint positions (for example the median of joint helper vertices
    /// of a body fit): rotations and scales stay the shared rest ones and each joint's local
    /// translation becomes the vector from its parent to it in the parent's rest frame.
    ///
    /// # Errors
    ///
    /// As [`InstanceBindPose::new`].
    pub fn from_joint_positions(shared: &Skeleton, positions: &[Vec3]) -> Result<Self, BindPoseError> {
        // The loop below indexes the globals built so far by parent id: a short parent list is
        // an error, and a parent at or after its joint counts as a root (`Skeleton::parent_of`,
        // as `Skeleton::globals` composes it), never an out-of-range index (deep-zai-animation 2).
        check_len("parents", shared.parents.len(), shared.len())?;
        check_len("positions", positions.len(), shared.len())?;
        let mut bind = shared.bind_pose.clone();
        let mut globals: Vec<Mat4> = Vec::with_capacity(shared.len());
        for i in 0..shared.len() {
            let parent_global = shared.parent_of(i).map_or(Mat4::IDENTITY, |p| globals[p]);
            let local = parent_global.inverse().transform_point3(positions[i]);
            bind[i].translation = local;
            globals.push(
                parent_global * Mat4::from_scale_rotation_translation(bind[i].scale, bind[i].rotation, local),
            );
        }
        Self::new(shared, bind)
    }

    /// Joints in the rest pose.
    pub fn len(&self) -> usize {
        self.bind_pose.len()
    }

    /// True when it has no joints.
    pub fn is_empty(&self) -> bool {
        self.bind_pose.is_empty()
    }

    /// Largest and smallest bone length ratio (1.0 = same as the shared skeleton).
    pub fn ratio_range(&self) -> (f32, f32) {
        self.length_ratio
            .iter()
            .fold((f32::INFINITY, 0.0f32), |(lo, hi), r| (lo.min(*r), hi.max(*r)))
    }
}

/// Move `pose` (authored against `shared`) onto the instance's rest pose, into `out`.
///
/// Per joint: `rotation = inst_rest · shared_rest⁻¹ · pose`, `translation = inst_rest +
/// (pose − shared_rest) · length_ratio`, `scale = pose · inst_rest / shared_rest`. At the
/// shared rest pose the result is exactly the instance rest pose; with an identical instance
/// it is exactly `pose`.
pub fn retarget_to_instance(shared: &Skeleton, instance: &InstanceBindPose, pose: &Pose, out: &mut Pose) {
    let n = shared.len().min(pose.len()).min(instance.len());
    out.translations.resize(n, Vec3::ZERO);
    out.rotations.resize(n, Quat::IDENTITY);
    out.scales.resize(n, Vec3::ONE);
    for i in 0..n {
        let base = &shared.bind_pose[i];
        let inst = &instance.bind_pose[i];
        out.rotations[i] = (inst.rotation * base.rotation.inverse() * pose.rotations[i]).normalize();
        out.translations[i] =
            inst.translation + (pose.translations[i] - base.translation) * instance.length_ratio[i];
        let safe = |v: f32| if v.abs() > 1e-6 { v } else { 1.0 };
        let ratio = Vec3::new(
            inst.scale.x / safe(base.scale.x),
            inst.scale.y / safe(base.scale.y),
            inst.scale.z / safe(base.scale.z),
        );
        out.scales[i] = pose.scales[i] * ratio;
    }
}

/// Skin palette for an instance: `global × instance inverse bind`.
pub fn instance_palette(instance: &InstanceBindPose, globals: &[Mat4]) -> Vec<Mat4> {
    let mut out = Vec::new();
    instance_palette_into(instance, globals, &mut out);
    out
}

/// [`instance_palette`] into `out`. A buffer that already fits is refilled in place.
pub fn instance_palette_into(instance: &InstanceBindPose, globals: &[Mat4], out: &mut Vec<Mat4>) {
    out.clear();
    out.extend(
        globals
            .iter()
            .zip(&instance.inverse_bind_matrices)
            .map(|(global, inverse)| *global * *inverse),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::translated;

    /// pelvis (1 m up) → spine (+0.3) → head (+0.3); pelvis → thigh (±0.1, −0.1) → foot (−0.5).
    fn body() -> Skeleton {
        let names = ["pelvis", "spine", "head", "thigh", "foot"];
        let parents = vec![None, Some(0), Some(1), Some(0), Some(3)];
        let binds = vec![
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(0.0, 0.3, 0.0)),
            translated(Vec3::new(0.0, 0.3, 0.0)),
            translated(Vec3::new(0.1, -0.1, 0.0)),
            translated(Vec3::new(0.0, -0.5, 0.0)),
        ];
        Skeleton::from_local_bind(names.iter().map(|s| s.to_string()).collect(), parents, binds)
            .expect("valid skeleton")
    }

    fn position(sk: &Skeleton, pose: &Pose, joint: usize) -> Vec3 {
        sk.globals(pose, None)[joint].w_axis.truncate()
    }

    #[test]
    fn identical_instance_is_the_identity_retarget() {
        let sk = body();
        let inst = InstanceBindPose::uniform(&sk, 1.0).expect("instance");
        let mut pose = sk.bind_pose_pose();
        pose.rotations[1] = Quat::from_rotation_x(0.4);
        pose.translations[0] += Vec3::new(0.0, 0.05, 0.0);
        let mut out = Pose::default();
        retarget_to_instance(&sk, &inst, &pose, &mut out);
        for i in 0..sk.len() {
            assert!(out.rotations[i].angle_between(pose.rotations[i]) < 1e-5);
            assert!((out.translations[i] - pose.translations[i]).length() < 1e-5);
            assert!((out.scales[i] - pose.scales[i]).length() < 1e-6);
        }
        let globals = sk.globals(&out, None);
        let a = instance_palette(&inst, &globals);
        let b = sk.skin_palette(&globals);
        for (x, y) in a.iter().zip(&b) {
            assert!(x.abs_diff_eq(*y, 1e-5));
        }
    }

    #[test]
    fn rest_pose_gives_an_identity_palette_for_any_proportions() {
        let sk = body();
        let mut scales = HashMap::new();
        scales.insert("spine".to_string(), 1.5);
        scales.insert("foot".to_string(), 0.8);
        let inst = InstanceBindPose::from_bone_scales(&sk, &scales).expect("instance");
        let mut out = Pose::default();
        retarget_to_instance(&sk, &inst, &sk.bind_pose_pose(), &mut out);
        let palette = instance_palette(&inst, &sk.globals(&out, None));
        for m in &palette {
            assert!(m.abs_diff_eq(Mat4::IDENTITY, 1e-4), "{m:?}");
        }
        // The shared palette would not be identity for the same pose: the point of the feature.
        let shared = sk.skin_palette(&sk.globals(&out, None));
        assert!(shared.iter().any(|m| !m.abs_diff_eq(Mat4::IDENTITY, 1e-3)));
        let (lo, hi) = inst.ratio_range();
        assert!((lo - 0.8).abs() < 1e-5 && (hi - 1.5).abs() < 1e-5);
    }

    #[test]
    fn a_uniformly_scaled_body_moves_like_the_shared_one_scaled() {
        let sk = body();
        let inst = InstanceBindPose::uniform(&sk, 1.2).expect("instance");
        let mut pose = sk.bind_pose_pose();
        pose.rotations[3] = Quat::from_rotation_x(-0.8); // a raised thigh
        pose.translations[0] += Vec3::new(0.0, -0.1, 0.0); // a crouch dip
        let mut out = Pose::default();
        retarget_to_instance(&sk, &inst, &pose, &mut out);
        for joint in 0..sk.len() {
            let shared = position(&sk, &pose, joint);
            let scaled = position(&sk, &out, joint);
            assert!(
                (scaled - shared * 1.2).length() < 1e-4,
                "joint {joint}: {scaled} vs {shared}"
            );
        }
    }

    #[test]
    fn longer_legs_reach_further_with_the_same_clip() {
        let sk = body();
        let mut scales = HashMap::new();
        scales.insert("foot".to_string(), 1.4);
        let inst = InstanceBindPose::from_bone_scales(&sk, &scales).expect("instance");
        let mut pose = sk.bind_pose_pose();
        pose.rotations[3] = Quat::from_rotation_x(-0.6);
        let mut out = Pose::default();
        retarget_to_instance(&sk, &inst, &pose, &mut out);
        let shared_reach = (position(&sk, &pose, 4) - position(&sk, &pose, 3)).length();
        let inst_reach = (position(&sk, &out, 4) - position(&sk, &out, 3)).length();
        assert!((inst_reach / shared_reach - 1.4).abs() < 1e-4);
        // Rotations are the clip's.
        assert!(out.rotations[3].angle_between(pose.rotations[3]) < 1e-5);
    }

    #[test]
    fn joint_positions_refit_the_rest_pose() {
        let sk = body();
        let positions = [
            Vec3::new(0.0, 1.1, 0.0),
            Vec3::new(0.0, 1.5, 0.0),
            Vec3::new(0.0, 1.9, 0.0),
            Vec3::new(0.15, 1.0, 0.0),
            Vec3::new(0.15, 0.4, 0.0),
        ];
        let inst = InstanceBindPose::from_joint_positions(&sk, &positions).expect("instance");
        let mut out = Pose::default();
        retarget_to_instance(&sk, &inst, &sk.bind_pose_pose(), &mut out);
        for (joint, want) in positions.iter().enumerate() {
            assert!(
                (position(&sk, &out, joint) - *want).length() < 1e-4,
                "joint {joint}"
            );
        }
        assert!(matches!(
            InstanceBindPose::from_joint_positions(&sk, &positions[..3]),
            Err(BindPoseError::WrongLength { .. })
        ));
    }

    #[test]
    fn bad_input_is_rejected() {
        let sk = body();
        assert!(matches!(
            InstanceBindPose::new(&sk, vec![LocalBind::default(); 2]),
            Err(BindPoseError::WrongLength { .. })
        ));
        let mut scales = HashMap::new();
        scales.insert("tail".to_string(), 2.0);
        assert!(matches!(
            InstanceBindPose::from_bone_scales(&sk, &scales),
            Err(BindPoseError::UnknownJoint(_))
        ));
        let mut bind = sk.bind_pose.clone();
        bind[1].scale = Vec3::ZERO;
        assert!(matches!(
            InstanceBindPose::new(&sk, bind),
            Err(BindPoseError::BadValue { joint: 1 })
        ));
        let inst = InstanceBindPose::uniform(&sk, 1.0).expect("instance");
        assert!(inst.with_inverse_binds(vec![Mat4::IDENTITY; 2]).is_err());
        // Review w11 #9: 0 collapses the bones and a negative factor mirrors them.
        for factor in [0.0, -1.0, f32::NAN] {
            assert!(
                matches!(
                    InstanceBindPose::uniform(&sk, factor),
                    Err(BindPoseError::BadValue { .. })
                ),
                "{factor}"
            );
            let mut scales = HashMap::new();
            scales.insert("spine".to_string(), factor);
            assert!(
                matches!(
                    InstanceBindPose::from_bone_scales(&sk, &scales),
                    Err(BindPoseError::BadValue { joint: 1 })
                ),
                "{factor}"
            );
        }
    }

    #[test]
    fn a_crowd_of_100_avatars_of_60_joints_retargets_every_joint_to_each_instance_rest_length() {
        // 60 joints, branching parents; every instance has its own uniform bone length.
        let names: Vec<String> = (0..60).map(|i| format!("j{i}")).collect();
        let parents: Vec<Option<u32>> = (0..60)
            .map(|i| if i == 0 { None } else { Some(((i - 1) / 2) as u32) })
            .collect();
        let binds = vec![translated(Vec3::new(0.0, 0.1, 0.0)); 60];
        let sk = Skeleton::from_local_bind(names, parents, binds).expect("skeleton");
        let mut pose = sk.bind_pose_pose();
        for r in &mut pose.rotations {
            *r = Quat::from_rotation_x(0.1);
        }
        let mut out = Pose::default();
        for i in 0..100 {
            let factor = 0.8 + i as f32 * 0.005;
            let inst = InstanceBindPose::uniform(&sk, factor).expect("instance");
            retarget_to_instance(&sk, &inst, &pose, &mut out);
            assert_eq!(out.len(), 60, "every joint is retargeted");
            // The pose keeps the shared rest translations, so each joint lands on the
            // instance's rest translation.
            let t = out.translations[59];
            assert!(
                t.abs_diff_eq(Vec3::new(0.0, 0.1 * factor, 0.0), 1e-5),
                "{i}: {t:?}"
            );
            let joints = out.translations.iter().zip(&out.rotations).zip(&inst.bind_pose);
            for (j, ((translation, rotation), rest)) in joints.enumerate() {
                // Every joint lands exactly on the instance's rest translation; the rotation is
                // the authored one (the instance rest rotations are identity).
                assert!(
                    translation.abs_diff_eq(rest.translation, 1e-5),
                    "{i} joint {j}: {translation:?} vs {:?}",
                    rest.translation
                );
                assert!(rotation.angle_between(Quat::from_rotation_x(0.1)) < 1e-4);
            }
            let globals = sk.globals(&out, None);
            let palette = instance_palette(&inst, &globals);
            assert_eq!(palette.len(), 60);
            assert!(palette[59].w_axis.y.is_finite());
        }
        // The wall-clock budget for this crowd lives in the benches, not in a unit test.
    }
}
