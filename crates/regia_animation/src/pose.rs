// SPDX-License-Identifier: MPL-2.0
//! Compact SoA local-space pose buffers and pose blending.
//!
//! Specification: `docs/specs/17-animation.md` § 2: "Poses are computed in a compact SoA
//! local-space buffer; writing back to ECS happens once per joint."

use regia_core::math::{Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::skeleton::LocalBind;

/// A local bind transform with only a translation set (identity rotation, unit scale).
pub fn translated(translation: Vec3) -> LocalBind {
    LocalBind {
        translation,
        ..LocalBind::default()
    }
}

/// A local-space pose, structure-of-arrays, indexed by joint id.
///
/// All three arrays have the same length (the skeleton's joint count). Sampling a clip only
/// overwrites joints the clip animates, so start from [`Pose::from_identity`] or the bind pose
/// and let tracks layer on top.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Pose {
    /// Local translation per joint, metres.
    pub translations: Vec<Vec3>,
    /// Local rotation per joint (unit quaternions).
    pub rotations: Vec<Quat>,
    /// Local scale per joint.
    pub scales: Vec<Vec3>,
}

// Hand-written so `clone_from` (and `copy_from`) reuse the three buffers: a derived `Clone` only
// writes `clone`, and its default `clone_from` reallocates every array (ADR-8707: the effects chain
// in `apply_animation` copies the pose every frame).
impl Clone for Pose {
    fn clone(&self) -> Self {
        Self {
            translations: self.translations.clone(),
            rotations: self.rotations.clone(),
            scales: self.scales.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.translations.clone_from(&source.translations);
        self.rotations.clone_from(&source.rotations);
        self.scales.clone_from(&source.scales);
    }
}

impl Pose {
    /// An identity pose (zero translation, identity rotation, unit scale) for `n` joints.
    pub fn from_identity(n: usize) -> Self {
        Self {
            translations: vec![Vec3::ZERO; n],
            rotations: vec![Quat::IDENTITY; n],
            scales: vec![Vec3::ONE; n],
        }
    }

    /// Number of joints in the pose.
    pub fn len(&self) -> usize {
        self.translations.len()
    }

    /// True when the pose has no joints.
    pub fn is_empty(&self) -> bool {
        self.translations.is_empty()
    }

    /// Blend `other` into `self` with weight `w` in `0..=1`: `self = self·(1−w) ⊕ other·w`.
    ///
    /// Translations and scales lerp; rotations use normalised lerp (nlerp, glam `Quat::lerp`)
    /// along the shortest arc. Missing joints in `other` (shorter pose) are left untouched.
    pub fn blend_from(&mut self, other: &Pose, w: f32) {
        let n = self.len().min(other.len());
        for i in 0..n {
            self.translations[i] = self.translations[i].lerp(other.translations[i], w);
            self.scales[i] = self.scales[i].lerp(other.scales[i], w);
            self.rotations[i] = self.rotations[i].lerp(other.rotations[i], w);
        }
    }

    /// Blend two poses into `self` with weight `w` (see [`Pose::blend_from`] for the rules).
    pub fn blend_of(&mut self, a: &Pose, b: &Pose, w: f32) {
        self.clone_from(a);
        self.blend_from(b, w);
    }

    /// Difference of this pose from `reference` (usually the bind pose).
    ///
    /// Translation is subtracted, rotation is `reference⁻¹ · self`, and scale is the
    /// component-wise ratio (1 when a reference component is ~0). Clip evaluation samples
    /// every clip on top of the bind pose, so additive nodes convert with this before
    /// [`Pose::additive_from`]. A pose equal to the reference becomes an identity delta.
    pub fn delta_from(&self, reference: &Pose) -> Pose {
        let mut out = Pose::default();
        self.delta_into(reference, &mut out);
        out
    }

    /// Write [`Pose::delta_from`] into `out`, reusing its joint buffers.
    pub fn delta_into(&self, reference: &Pose, out: &mut Pose) {
        let n = self.len().min(reference.len());
        if out.len() != n {
            out.translations.resize(n, Vec3::ZERO);
            out.rotations.resize(n, Quat::IDENTITY);
            out.scales.resize(n, Vec3::ONE);
        }
        for i in 0..n {
            out.translations[i] = self.translations[i] - reference.translations[i];
            out.rotations[i] = (reference.rotations[i].inverse() * self.rotations[i]).normalize();
            out.scales[i] = Vec3::new(
                scale_ratio(self.scales[i].x, reference.scales[i].x),
                scale_ratio(self.scales[i].y, reference.scales[i].y),
                scale_ratio(self.scales[i].z, reference.scales[i].z),
            );
        }
    }

    /// Apply `additive` on top of `self` with weight `w`. The additive pose holds deltas
    /// relative to the pose it was authored against (the bind pose, via [`Pose::delta_from`],
    /// or identity when the clip stores deltas directly): translations add, rotations
    /// pre-multiply the delta, scales multiply deviations from one.
    pub fn additive_from(&mut self, additive: &Pose, w: f32) {
        let n = self.len().min(additive.len());
        for i in 0..n {
            self.translations[i] += additive.translations[i] * w;
            let delta = Quat::IDENTITY.lerp(additive.rotations[i], w);
            self.rotations[i] = (self.rotations[i] * delta).normalize();
            self.scales[i] *= Vec3::ONE.lerp(additive.scales[i], w);
        }
    }

    /// Copy `other` into `self` (grows/shrinks to match).
    pub fn copy_from(&mut self, other: &Pose) {
        self.clone_from(other);
    }

    /// Blend `other` into `self` only where `mask > 0`, with per-joint effective weight
    /// `w · mask[i]`. Used by animation layers.
    pub fn masked_blend_from(&mut self, other: &Pose, mask: &[f32], w: f32) {
        let n = self.len().min(other.len()).min(mask.len());
        for (i, &mask_weight) in mask.iter().enumerate().take(n) {
            let eff = w * mask_weight;
            if eff <= 0.0 {
                continue;
            }
            self.translations[i] = self.translations[i].lerp(other.translations[i], eff);
            self.scales[i] = self.scales[i].lerp(other.scales[i], eff);
            self.rotations[i] = self.rotations[i].lerp(other.rotations[i], eff);
        }
    }

    /// Apply `additive` on top of `self` only where `mask > 0` (see [`Pose::additive_from`]).
    pub fn masked_additive_from(&mut self, additive: &Pose, mask: &[f32], w: f32) {
        let n = self.len().min(additive.len()).min(mask.len());
        for (i, &mask_weight) in mask.iter().enumerate().take(n) {
            let eff = w * mask_weight;
            if eff <= 0.0 {
                continue;
            }
            self.translations[i] += additive.translations[i] * eff;
            let delta = Quat::IDENTITY.lerp(additive.rotations[i], eff);
            self.rotations[i] = (self.rotations[i] * delta).normalize();
            self.scales[i] *= Vec3::ONE.lerp(additive.scales[i], eff);
        }
    }
}

fn scale_ratio(sample: f32, reference: f32) -> f32 {
    if reference.abs() > 1e-8 {
        sample / reference
    } else {
        1.0
    }
}

/// A joint mask: a set of joints (optionally including their descendants) used by animation
/// layers to restrict a pose to part of the body (for example an upper-body wave).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct JointMask {
    /// Masked joint ids, in skeleton indexing.
    pub joints: Vec<u32>,
    /// Also mask every descendant of the listed joints.
    pub include_children: bool,
}

impl JointMask {
    /// Expand the mask to a per-joint weight (1.0 masked, 0.0 not) for a skeleton with `len`
    /// joints and `parents`.
    pub fn weights(&self, parents: &[Option<u32>], len: usize) -> Vec<f32> {
        let mut weights = Vec::new();
        self.weights_into(parents, len, &mut weights);
        weights
    }

    /// Write [`JointMask::weights`] into `out`, reusing its allocation.
    pub fn weights_into(&self, parents: &[Option<u32>], len: usize, out: &mut Vec<f32>) {
        out.clear();
        out.resize(len, 0.0);
        let weights = out;
        for &j in &self.joints {
            let j = j as usize;
            if j < len {
                weights[j] = 1.0;
            }
        }
        if self.include_children {
            // Parents come before children, so one forward pass propagates downwards.
            for i in 0..len {
                if let Some(&Some(p)) = parents.get(i)
                    && let Some(&parent_weight) = weights.get(p as usize)
                    && parent_weight > 0.0
                {
                    weights[i] = weights[i].max(parent_weight);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_from_reference_is_identity_when_poses_match() {
        let bind = Pose {
            translations: vec![Vec3::new(0.0, 1.0, 0.0)],
            rotations: vec![Quat::from_rotation_y(0.4)],
            scales: vec![Vec3::new(1.0, 2.0, 1.0)],
        };
        let delta = bind.delta_from(&bind);
        assert!(delta.translations[0].length() < 1e-5);
        assert!(delta.rotations[0].dot(Quat::IDENTITY) > 0.999);
        assert!(delta.scales[0].abs_diff_eq(Vec3::ONE, 1e-5));
    }

    #[test]
    fn blend_interpolates_translations() {
        let mut a = Pose::from_identity(1);
        let mut b = Pose::from_identity(1);
        a.translations[0] = Vec3::new(0.0, 0.0, 0.0);
        b.translations[0] = Vec3::new(10.0, 0.0, 0.0);
        a.blend_from(&b, 0.25);
        assert!(a.translations[0].abs_diff_eq(Vec3::new(2.5, 0.0, 0.0), 1e-6));
    }

    #[test]
    fn blend_rotations_shortest_arc() {
        let mut a = Pose::from_identity(1);
        let mut b = Pose::from_identity(1);
        a.rotations[0] = Quat::from_rotation_y(0.0);
        b.rotations[0] = Quat::from_rotation_y(std::f32::consts::PI);
        a.blend_from(&b, 0.5);
        let angle = a.rotations[0].angle_between(Quat::from_rotation_y(0.0));
        // Halfway between 0 and π about Y (either +π/2 or −π/2).
        assert!(
            (angle - std::f32::consts::FRAC_PI_2).abs() < 1e-4,
            "angle {angle}"
        );
    }

    #[test]
    fn blend_weights_zero_and_one_are_exact() {
        let mut a = Pose::from_identity(1);
        let mut b = Pose::from_identity(1);
        a.translations[0] = Vec3::new(1.0, 2.0, 3.0);
        b.translations[0] = Vec3::new(-4.0, 5.0, -6.0);
        let save = a.clone();
        a.blend_from(&b, 0.0);
        assert_eq!(a, save);
        a.blend_from(&b, 1.0);
        assert_eq!(a, b);
    }

    #[test]
    fn additive_translations_and_scales() {
        let mut base = Pose::from_identity(1);
        base.scales[0] = Vec3::splat(2.0);
        let mut add = Pose::from_identity(1);
        add.translations[0] = Vec3::new(1.0, 0.0, 0.0);
        add.scales[0] = Vec3::splat(1.5);
        base.additive_from(&add, 1.0);
        assert!(base.translations[0].abs_diff_eq(Vec3::new(1.0, 0.0, 0.0), 1e-6));
        assert!(base.scales[0].abs_diff_eq(Vec3::splat(3.0), 1e-6));
    }

    #[test]
    fn mask_includes_children() {
        // root=0, spine=1 (child of 0), head=2 (child of 1), leg=3 (child of 0)
        let parents = [None, Some(0), Some(1), Some(0)];
        let mask = JointMask {
            joints: vec![1],
            include_children: true,
        };
        assert_eq!(mask.weights(&parents, 4), vec![0.0, 1.0, 1.0, 0.0]);
        let mask = JointMask {
            joints: vec![1],
            include_children: false,
        };
        assert_eq!(mask.weights(&parents, 4), vec![0.0, 1.0, 0.0, 0.0]);
    }
}
