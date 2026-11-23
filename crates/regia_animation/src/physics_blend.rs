// SPDX-License-Identifier: MPL-2.0
//! Blending simulated joint transforms (a ragdoll) into the animated pose.
//!
//! Specification: `docs/specs/17-animation.md` § 4 (ragdoll and partial physics blend). The
//! animation crate does not depend on physics (`docs/02-architecture.md` § 1): a gameplay
//! system (the framework's ragdoll) writes the simulated rig-space transforms of some joints
//! into a [`PhysicsPoseBlend`] on the rig, with one weight per joint, and the apply system
//! blends them over the animated pose before it reaches the joint entities and the skin
//! palette. The [`crate::Animator`]'s own pose stays the animation's, so a powered ragdoll can
//! keep driving its motors towards it.
//!
//! Rotations blend in rig (object) space so a limb hanging under gravity hangs the same way
//! whatever its parents do. A simulated joint whose parent is not simulated (the pelvis)
//! also blends its position; every other joint keeps its animated bone length.

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;

use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// Simulated rig-space joint transforms and their blend weights (module docs).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct PhysicsPoseBlend {
    /// Per joint: the simulated `(translation, rotation)` in the rig's object space, or
    /// `None` for joints without a body (they follow their parent with animated locals).
    pub joints: Vec<Option<(Vec3, Quat)>>,
    /// Per joint: how much of the simulated transform shows, `0..=1`.
    pub weights: Vec<f32>,
}

impl PhysicsPoseBlend {
    /// An empty blend for a skeleton of `len` joints (everything animated).
    pub fn new(len: usize) -> Self {
        PhysicsPoseBlend {
            joints: vec![None; len],
            weights: vec![0.0; len],
        }
    }

    /// Largest weight of any simulated joint (0 when the blend changes nothing).
    pub fn max_weight(&self) -> f32 {
        self.joints
            .iter()
            .zip(&self.weights)
            .filter(|(j, _)| j.is_some())
            .map(|(_, w)| w.clamp(0.0, 1.0))
            .fold(0.0, f32::max)
    }
}

/// `q` scaled to unit length, or `None` when its length is zero or not finite (glam 0.33's `Quat`
/// has no `try_normalize`). It is `Quat::normalize` itself, checked afterwards: `Vec4::try_normalize`
/// multiplies by the reciprocal length and rounds differently, which moved the determinism pins.
fn unit_quat(q: Quat) -> Option<Quat> {
    let unit = q.normalize();
    unit.is_finite().then_some(unit)
}

/// The rotation of a joint matrix; `None` when a scale axis is zero (the extracted quaternion is
/// NaN, audit v033b) or the matrix is not finite.
fn rotation_of(m: Mat4) -> Option<Quat> {
    let rotation = unit_quat(m.to_scale_rotation_translation().1)?;
    rotation.is_finite().then_some(rotation)
}

/// Blend `blend` over the animated local `pose` in place (module docs).
pub fn blend_physics_pose(skeleton: &Skeleton, pose: &mut Pose, blend: &PhysicsPoseBlend) {
    let n = skeleton.len().min(pose.len());
    if blend.max_weight() <= 0.0 {
        return;
    }
    crate::hot_count::count_physics_scratch();
    crate::hot_count::count_physics_scratch();
    let animated = skeleton.globals(pose, None);
    let mut blended = vec![Mat4::IDENTITY; n];
    blend_physics_matrices(skeleton, pose, blend, &animated, &mut blended);
}

/// [`blend_physics_pose`] writing into caller-owned matrix buffers.
///
/// A buffer that already fits the skeleton is refilled in place. The public function still
/// allocates, so callers outside the apply system keep the previous cost.
pub(crate) fn blend_physics_pose_reuse(
    skeleton: &Skeleton,
    pose: &mut Pose,
    blend: &PhysicsPoseBlend,
    animated: &mut Vec<Mat4>,
    blended: &mut Vec<Mat4>,
) {
    let n = skeleton.len().min(pose.len());
    if blend.max_weight() <= 0.0 {
        return;
    }
    if animated.capacity() < skeleton.len() {
        crate::hot_count::count_physics_scratch();
    }
    if blended.capacity() < n {
        crate::hot_count::count_physics_scratch();
    }
    skeleton.globals_into(pose, None, animated);
    blended.clear();
    blended.resize(n, Mat4::IDENTITY);
    blend_physics_matrices(skeleton, pose, blend, animated, blended);
}

/// The blend loop shared by the allocating and the reused entry points.
fn blend_physics_matrices(
    skeleton: &Skeleton,
    pose: &mut Pose,
    blend: &PhysicsPoseBlend,
    animated: &[Mat4],
    blended: &mut [Mat4],
) {
    let n = skeleton
        .len()
        .min(pose.len())
        .min(animated.len())
        .min(blended.len());
    for i in 0..n {
        let parent = skeleton.parent_of(i);
        let parent_global = parent.map_or(Mat4::IDENTITY, |p| blended[p]);
        let simulated = blend.joints.get(i).copied().flatten();
        let w = blend.weights.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);
        // A joint (or parent) scaled to zero has no rotation to blend from: it keeps the animated
        // local pose (audit v033b), as does a non-finite simulated pose.
        let usable = simulated.filter(|(t, r)| w > 0.0 && t.is_finite() && r.is_finite());
        if let Some(((sim_translation, sim_rotation), anim_rotation, parent_rotation)) =
            usable.and_then(|sim| Some((sim, rotation_of(animated[i])?, rotation_of(parent_global)?)))
        {
            let mut sim_rotation = sim_rotation;
            if anim_rotation.dot(sim_rotation) < 0.0 {
                sim_rotation = -sim_rotation;
            }
            let rotation = anim_rotation.slerp(sim_rotation, w).normalize();
            pose.rotations[i] = (parent_rotation.inverse() * rotation).normalize();
            let parent_simulated = parent.is_some_and(|p| blend.joints.get(p).copied().flatten().is_some());
            if !parent_simulated {
                let anim_position = animated[i].w_axis.truncate();
                let position = anim_position.lerp(sim_translation, w);
                let local = parent_global.inverse().transform_point3(position);
                if local.is_finite() {
                    pose.translations[i] = local;
                }
            }
        }
        blended[i] = parent_global
            * Mat4::from_scale_rotation_translation(pose.scales[i], pose.rotations[i], pose.translations[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::translated;

    fn chain() -> Skeleton {
        Skeleton::from_local_bind(
            vec!["pelvis".into(), "spine".into(), "head".into()],
            vec![None, Some(0), Some(1)],
            vec![
                translated(Vec3::new(0.0, 1.0, 0.0)),
                translated(Vec3::new(0.0, 0.5, 0.0)),
                translated(Vec3::new(0.0, 0.5, 0.0)),
            ],
        )
        .expect("skeleton")
    }

    #[test]
    fn full_weight_matches_the_simulation_and_zero_changes_nothing() {
        let sk = chain();
        let mut blend = PhysicsPoseBlend::new(3);
        // The ragdoll lies on its back: pelvis at 0.2 m, everything rotated −90° about X.
        let lying = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2);
        blend.joints[0] = Some((Vec3::new(0.0, 0.2, 0.0), lying));
        blend.joints[1] = Some((Vec3::new(0.0, 0.2, 0.5), lying));
        let mut pose = sk.bind_pose_pose();
        blend_physics_pose(&sk, &mut pose, &blend);
        assert_eq!(pose, sk.bind_pose_pose(), "zero weights change nothing");

        blend.weights = vec![1.0; 3];
        blend_physics_pose(&sk, &mut pose, &blend);
        let g = sk.globals(&pose, None);
        assert!((g[0].w_axis.truncate() - Vec3::new(0.0, 0.2, 0.0)).length() < 1e-5);
        // The head (unsimulated) follows the spine with its animated local.
        let head = g[2].w_axis.truncate();
        assert!((head - Vec3::new(0.0, 0.2, -1.0)).length() < 1e-4, "{head}");
        // The spine keeps its bone length (its simulated translation is ignored).
        assert!((g[1].w_axis.truncate() - Vec3::new(0.0, 0.2, -0.5)).length() < 1e-4);
    }

    /// Audit v033b (High): a parent joint scaled to zero makes the child's global matrix singular;
    /// the extracted rotation was NaN and was written into the pose. The joint keeps the animated
    /// local pose instead.
    #[test]
    fn a_zero_scaled_parent_keeps_the_animated_pose() {
        let sk = chain();
        let mut pose = sk.bind_pose_pose();
        pose.scales[0] = Vec3::ZERO;
        let before = pose.clone();
        let mut blend = PhysicsPoseBlend::new(3);
        blend.joints[1] = Some((Vec3::new(0.0, 0.2, 0.5), Quat::from_rotation_x(0.3)));
        blend.weights[1] = 1.0;
        blend_physics_pose(&sk, &mut pose, &blend);
        for i in 0..3 {
            assert!(
                pose.rotations[i].is_finite(),
                "joint {i}: {:?}",
                pose.rotations[i]
            );
            assert!(
                pose.translations[i].is_finite(),
                "joint {i}: {:?}",
                pose.translations[i]
            );
        }
        assert_eq!(pose, before, "nothing usable to blend: the animation stays");
    }

    #[test]
    fn partial_weight_on_one_joint_bends_only_that_branch() {
        let sk = chain();
        let mut blend = PhysicsPoseBlend::new(3);
        blend.joints[1] = Some((Vec3::new(0.0, 1.5, 0.0), Quat::from_rotation_z(1.0)));
        blend.weights[1] = 0.5;
        let mut pose = sk.bind_pose_pose();
        blend_physics_pose(&sk, &mut pose, &blend);
        assert_eq!(pose.rotations[0], Quat::IDENTITY);
        assert!((pose.rotations[1].angle_between(Quat::IDENTITY) - 0.5).abs() < 1e-4);
        assert_eq!(
            pose.translations[1],
            Vec3::new(0.0, 0.5, 0.0),
            "child keeps its bone length"
        );
    }
}
