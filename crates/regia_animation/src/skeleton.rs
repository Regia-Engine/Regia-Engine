// SPDX-License-Identifier: MPL-2.0
//! Skeleton assets: joint names, hierarchy, bind pose and inverse bind matrices.
//!
//! Specification: `docs/specs/17-animation.md` § 1. Joints are stored topologically sorted
//! (every parent appears before its children) so forward kinematics is a single pass.

use regia_core::math::{Mat4, Quat, Vec3};
use regia_scene::Transform;
use serde::{Deserialize, Serialize};

use crate::pose::Pose;

// Full forward-kinematics passes on this thread (unit tests only: the `apply_foot_plant` pass
// count test). Not compiled into any other build.
#[cfg(test)]
thread_local! {
    pub(crate) static FULL_GLOBALS_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One joint's local bind transform (serde-able mirror of the scene `Transform` TRS triple;
/// skeletons are assets, not scene components). The default is the identity transform
/// (zero translation, identity rotation, unit scale).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct LocalBind {
    /// Local translation, metres.
    pub translation: Vec3,
    /// Local rotation.
    pub rotation: Quat,
    /// Local scale.
    pub scale: Vec3,
}

impl Default for LocalBind {
    fn default() -> Self {
        LocalBind {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }
}

impl From<Transform> for LocalBind {
    fn from(t: Transform) -> Self {
        LocalBind {
            translation: t.translation,
            rotation: t.rotation,
            scale: t.scale,
        }
    }
}

impl LocalBind {
    /// The equivalent scene `Transform`.
    pub fn to_transform(&self) -> Transform {
        Transform {
            translation: self.translation,
            rotation: self.rotation,
            scale: self.scale,
        }
    }
}

/// A socket point defined on a joint during import, cooked as part of the skeleton.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SkeletonSocket {
    /// Socket id (e.g. `"weapon"`, `"muzzle"`).
    pub name: String,
    /// Joint this socket hangs from.
    pub joint: String,
    /// Offset from that joint in the joint's local frame.
    pub offset: LocalBind,
}

/// A skeleton: joint names, parent indices (topologically sorted: parents first), the bind pose
/// as local TRS per joint, and per-joint inverse bind matrices (object space at bind).
///
/// Multiple root joints (a forest) are allowed; glTF skins sometimes produce them.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Skeleton {
    /// Joint names, indexed by joint id. Should be unique; duplicates make
    /// [`Skeleton::joint_index`] ambiguous (the first match wins) and are rejected by
    /// [`Skeleton::validate`].
    pub joint_names: Vec<String>,
    /// Parent joint id per joint, or `None` for roots. Must be `<` the joint's own id
    /// (topological order).
    pub parents: Vec<Option<u32>>,
    /// Bind pose: local translation/rotation/scale per joint.
    pub bind_pose: Vec<LocalBind>,
    /// Inverse of each joint's object-space bind matrix. Applying it to a skinned vertex
    /// moves it into the joint's local space.
    pub inverse_bind_matrices: Vec<Mat4>,
    /// Socket attachment points authored with the skeleton (from glTF `socket_*` nodes).
    pub sockets: Vec<SkeletonSocket>,
}

/// Errors returned by [`Skeleton::validate`].
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SkeletonError {
    /// All per-joint arrays must have the same length as `joint_names`.
    #[error("skeleton arrays disagree: names={names} parents={parents} bind={bind} inverse_binds={ibm}")]
    MismatchedLengths {
        /// Number of joint names.
        names: usize,
        /// Number of parent indices.
        parents: usize,
        /// Number of bind-pose transforms.
        bind: usize,
        /// Number of inverse bind matrices.
        ibm: usize,
    },
    /// Parents must reference an earlier joint.
    #[error("joint {joint} has parent {parent}, which does not come before it (topological order)")]
    ParentNotEarlier {
        /// The offending joint id.
        joint: usize,
        /// The parent id it referenced.
        parent: u32,
    },
    /// Two joints share a name, so name lookup is ambiguous.
    #[error("duplicate joint name {name:?}")]
    DuplicateName {
        /// The duplicated name.
        name: String,
    },
    /// A joint name is empty.
    #[error("joint {joint} has an empty name")]
    EmptyName {
        /// The offending joint id.
        joint: usize,
    },
}

impl Skeleton {
    /// Number of joints.
    pub fn len(&self) -> usize {
        self.joint_names.len()
    }

    /// True when the skeleton has no joints.
    pub fn is_empty(&self) -> bool {
        self.joint_names.is_empty()
    }

    /// Look up a joint id by name.
    pub fn joint_index(&self, name: &str) -> Option<u32> {
        crate::hot_count::count_joint_index();
        self.joint_names.iter().position(|n| n == name).map(|i| i as u32)
    }

    /// Validate structural invariants: equal array lengths, topological order, unique
    /// non-empty names.
    ///
    /// # Errors
    ///
    /// Returns the first violation found; see [`SkeletonError`].
    pub fn validate(&self) -> Result<(), SkeletonError> {
        let n = self.joint_names.len();
        if self.parents.len() != n || self.bind_pose.len() != n || self.inverse_bind_matrices.len() != n {
            return Err(SkeletonError::MismatchedLengths {
                names: n,
                parents: self.parents.len(),
                bind: self.bind_pose.len(),
                ibm: self.inverse_bind_matrices.len(),
            });
        }
        for (joint, parent) in self.parents.iter().enumerate() {
            if let Some(p) = parent
                && p >= &(joint as u32)
            {
                return Err(SkeletonError::ParentNotEarlier { joint, parent: *p });
            }
        }
        let mut seen = std::collections::HashSet::with_capacity(n);
        for (joint, name) in self.joint_names.iter().enumerate() {
            if name.is_empty() {
                return Err(SkeletonError::EmptyName { joint });
            }
            if !seen.insert(name.as_str()) {
                return Err(SkeletonError::DuplicateName { name: name.clone() });
            }
        }
        Ok(())
    }

    /// The parent of `joint`, or `None` for a root.
    ///
    /// A skeleton that skipped [`Skeleton::validate`] (built by hand or deserialized without
    /// the cooked-asset check) may list fewer parents than joints, or a parent that does not
    /// come before its child. Such a joint counts as a root here, exactly as
    /// [`Skeleton::globals`] composes it, so callers index nothing out of range.
    ///
    /// ```
    /// use regia_animation::Skeleton;
    ///
    /// let skeleton = Skeleton {
    ///     joint_names: vec!["a".into(), "b".into(), "c".into()],
    ///     parents: vec![None, Some(2)],
    ///     ..Skeleton::default()
    /// };
    /// assert_eq!(skeleton.parent_of(1), None, "a later parent is no parent");
    /// assert_eq!(skeleton.parent_of(2), None, "past the parent list");
    /// ```
    #[must_use]
    pub fn parent_of(&self, joint: usize) -> Option<usize> {
        match self.parents.get(joint).copied().flatten() {
            Some(p) if (p as usize) < joint => Some(p as usize),
            _ => None,
        }
    }

    /// The bind pose as a [`Pose`].
    ///
    /// The buffers have exactly the joint count as capacity, like a clone of the pose, so
    /// callers that rotate this pose with its clones keep flat capacities. Always one build.
    ///
    /// ```
    /// use regia_animation::demo_rig::{DemoRig, demo_humanoid};
    ///
    /// let skeleton = demo_humanoid(&DemoRig::default()).unwrap();
    /// let pose = skeleton.bind_pose_pose();
    /// assert_eq!(pose.len(), skeleton.len());
    /// assert!(skeleton.joint_index("hips").is_some());
    /// ```
    pub fn bind_pose_pose(&self) -> Pose {
        crate::hot_count::count_bind_pose_build();
        Pose {
            translations: self.bind_pose.iter().map(|t| t.translation).collect(),
            rotations: self.bind_pose.iter().map(|t| t.rotation).collect(),
            scales: self.bind_pose.iter().map(|t| t.scale).collect(),
        }
    }

    /// Write the bind pose into `out`.
    ///
    /// An `out` whose three buffers already fit the skeleton is refilled in place. Growing any
    /// of them counts as one bind-pose build.
    pub fn bind_pose_into(&self, out: &mut Pose) {
        let n = self.bind_pose.len();
        if out.translations.capacity() < n || out.rotations.capacity() < n || out.scales.capacity() < n {
            crate::hot_count::count_bind_pose_build();
        }
        out.translations.clear();
        out.rotations.clear();
        out.scales.clear();
        out.translations
            .extend(self.bind_pose.iter().map(|t| t.translation));
        out.rotations.extend(self.bind_pose.iter().map(|t| t.rotation));
        out.scales.extend(self.bind_pose.iter().map(|t| t.scale));
    }

    /// Object-space bind matrix per joint (the inverse of `inverse_bind_matrices`).
    pub fn bind_globals(&self) -> Vec<Mat4> {
        self.inverse_bind_matrices.iter().map(|m| m.inverse()).collect()
    }

    /// Forward kinematics: compose the given local pose into object-space joint matrices,
    /// relative to `base` (identity when `None`). Joints are topologically sorted, so a
    /// single pass suffices.
    pub fn globals(&self, pose: &Pose, base: Option<Mat4>) -> Vec<Mat4> {
        let mut globals = Vec::new();
        self.globals_into(pose, base, &mut globals);
        globals
    }

    /// Forward kinematics into `out` (see [`Self::globals`]).
    ///
    /// `out` is resized to this skeleton's joint count. Joints the pose does not cover, and any
    /// tail past the composed prefix, stay identity. A caller that keeps `out` reuses its buffer.
    /// Same as [`Self::globals_into`].
    pub fn write_globals(&self, pose: &Pose, base: Option<Mat4>, out: &mut Vec<Mat4>) {
        self.globals_into(pose, base, out);
    }

    /// Forward kinematics into `out` (see [`Skeleton::globals`]).
    ///
    /// `out` is resized to [`Skeleton::len`]. A buffer that already has that capacity is
    /// refilled in place and does not count as a new fill. Joints past the pose stay identity.
    pub fn globals_into(&self, pose: &Pose, base: Option<Mat4>, out: &mut Vec<Mat4>) {
        #[cfg(test)]
        FULL_GLOBALS_PASSES.with(|c| c.set(c.get() + 1));
        if out.capacity() < self.len() {
            crate::hot_count::count_globals_fill();
        }
        out.clear();
        out.resize(self.len(), Mat4::IDENTITY);
        self.write_globals_range(pose, base, out, 0);
    }

    /// Recompute object-space matrices from joint `from` through the last joint both `pose`
    /// and `out` cover.
    ///
    /// Joints are topologically sorted, so matrices before `from` stay valid. This does not
    /// allocate and does not count as a fill. Callers size `out` with [`Skeleton::globals_into`]
    /// first.
    pub fn globals_refresh_from(&self, pose: &Pose, base: Option<Mat4>, out: &mut [Mat4], from: usize) {
        self.write_globals_range(pose, base, out, from);
    }

    fn write_globals_range(&self, pose: &Pose, base: Option<Mat4>, globals: &mut [Mat4], from: usize) {
        let n = self
            .len()
            .min(pose.len())
            .min(self.parents.len())
            .min(globals.len());
        if from >= n {
            return;
        }
        let identity = Mat4::IDENTITY;
        let base = base.unwrap_or(identity);
        for i in from..n {
            let local = Mat4::from_scale_rotation_translation(
                pose.scales[i],
                pose.rotations[i],
                pose.translations[i],
            );
            // Copy the parent matrix out before writing: a parent that is not an earlier joint
            // (malformed skeleton) is treated as a root rather than indexing out of range.
            let parent = match self.parents.get(i).copied().flatten() {
                Some(p) if (p as usize) < i => Some(globals[p as usize]),
                _ => None,
            };
            globals[i] = match parent {
                Some(parent) => parent * local,
                None => base * local,
            };
        }
    }

    /// Skin palette: per joint `global * inverse_bind`, ready for GPU (or CPU) skinning.
    pub fn skin_palette(&self, globals: &[Mat4]) -> Vec<Mat4> {
        let mut out = Vec::new();
        self.skin_palette_into(globals, &mut out);
        out
    }

    /// Skin palette into `out` (see [`Skeleton::skin_palette`]).
    ///
    /// The length is the overlap of `globals` and the inverse-bind list. A buffer that already
    /// fits is refilled in place.
    pub fn skin_palette_into(&self, globals: &[Mat4], out: &mut Vec<Mat4>) {
        out.clear();
        out.extend(
            globals
                .iter()
                .zip(&self.inverse_bind_matrices)
                .map(|(global, inverse)| *global * *inverse),
        );
    }

    /// Longest bind-space distance from each joint to any of its descendants, in metres.
    ///
    /// Used by clip compression to propagate joint rotation error to a virtual end-effector:
    /// a rotation error of θ radians at a joint displaces an end-effector `d` metres down the
    /// chain by up to `θ·d`.
    pub fn max_descendant_distance(&self) -> Vec<f32> {
        let n = self.len();
        let mut result = vec![0.0f32; n];
        // Walk joints in reverse topological order: children are always processed first.
        for i in (0..n).rev() {
            let local_len = self.bind_pose.get(i).map_or(0.0, |b| b.translation.length());
            // A parent that is not an earlier joint (malformed skeleton) is treated as a root.
            if let Some(p) = self.parents.get(i).copied().flatten().map(|p| p as usize)
                && p < i
            {
                result[p] = result[p].max(result[i] + local_len);
            }
        }
        result
    }

    /// Build a skeleton from local bind transforms, computing inverse bind matrices by
    /// forward kinematics. `names.len()` must equal `transforms.len()`.
    ///
    /// # Errors
    ///
    /// Returns [`SkeletonError`] when the arrays disagree in length or the hierarchy is not
    /// topologically ordered.
    pub fn from_local_bind(
        names: Vec<String>,
        parents: Vec<Option<u32>>,
        bind_pose: Vec<LocalBind>,
    ) -> Result<Self, SkeletonError> {
        let skeleton = Self {
            joint_names: names,
            parents,
            bind_pose,
            inverse_bind_matrices: Vec::new(),
            sockets: Vec::new(),
        };
        // `globals` below walks all three arrays, so reject length and parent errors first.
        let n = skeleton.joint_names.len();
        if skeleton.parents.len() != n || skeleton.bind_pose.len() != n {
            return Err(SkeletonError::MismatchedLengths {
                names: n,
                parents: skeleton.parents.len(),
                bind: skeleton.bind_pose.len(),
                ibm: 0,
            });
        }
        for (joint, parent) in skeleton.parents.iter().enumerate() {
            if let Some(p) = parent
                && *p as usize >= joint
            {
                return Err(SkeletonError::ParentNotEarlier { joint, parent: *p });
            }
        }
        // Fill the inverse binds: `validate` requires equal array lengths.
        let pose = skeleton.bind_pose_pose();
        let globals = skeleton.globals(&pose, None);
        let mut skeleton = skeleton;
        skeleton.inverse_bind_matrices = globals.iter().map(|m| m.inverse()).collect();
        skeleton.validate()?;
        Ok(skeleton)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::translated;
    use crate::skeleton::LocalBind;

    fn two_joint_skeleton() -> Skeleton {
        // root at origin, child 0.5 m along +Y
        Skeleton::from_local_bind(
            vec!["root".into(), "child".into()],
            vec![None, Some(0)],
            vec![translated(Vec3::ZERO), translated(Vec3::new(0.0, 0.5, 0.0))],
        )
        .expect("valid skeleton")
    }

    #[test]
    fn malformed_parents_do_not_panic_in_forward_kinematics() {
        // Parents shorter than the names: an error, not an index panic.
        let short = Skeleton::from_local_bind(
            vec!["a".into(), "b".into()],
            vec![None],
            vec![translated(Vec3::ZERO), translated(Vec3::ZERO)],
        );
        assert!(matches!(short, Err(SkeletonError::MismatchedLengths { .. })));
        // A parent id past the joint list: an error, not an index panic.
        let far = Skeleton::from_local_bind(
            vec!["a".into(), "b".into()],
            vec![None, Some(9)],
            vec![translated(Vec3::ZERO), translated(Vec3::ZERO)],
        );
        assert!(matches!(far, Err(SkeletonError::ParentNotEarlier { .. })));
    }

    #[test]
    fn builds_inverse_binds_from_bind_pose() {
        let sk = two_joint_skeleton();
        sk.validate().expect("valid");
        let globals = sk.bind_globals();
        assert!(globals[0].abs_diff_eq(Mat4::IDENTITY, 1e-6));
        assert!((globals[1].w_axis.y - 0.5).abs() < 1e-6, "{:?}", globals[1]);
        let palette = sk.skin_palette(&globals);
        for m in &palette {
            assert!(
                m.abs_diff_eq(Mat4::IDENTITY, 1e-6),
                "bind palette must be identity"
            );
        }
    }

    #[test]
    fn globals_compose_local_rotations() {
        let sk = two_joint_skeleton();
        let mut pose = sk.bind_pose_pose();
        pose.rotations[0] = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        let globals = sk.globals(&pose, None);
        // Child sits 0.5 m along +Y of the root; rotating the root 90° about Y leaves +Y where
        // it was.
        assert!((globals[1].w_axis.y - 0.5).abs() < 1e-6);
        assert!(globals[1].w_axis.x.abs() < 1e-6);
        assert!(globals[1].w_axis.z.abs() < 1e-6);
    }

    #[test]
    fn globals_rotate_child_off_the_rotation_axis() {
        let sk = two_joint_skeleton();
        let mut pose = sk.bind_pose_pose();
        pose.rotations[0] = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
        let globals = sk.globals(&pose, None);
        // The child's +Y offset is perpendicular to the X axis, so a 90 degree turn about X
        // swings it onto +Z; the child basis turns with it.
        assert!(globals[1].w_axis.y.abs() < 1e-5, "{:?}", globals[1]);
        assert!((globals[1].w_axis.z - 0.5).abs() < 1e-5, "{:?}", globals[1]);
        assert!((globals[1].y_axis.truncate() - Vec3::Z).length() < 1e-5);
        assert!((globals[1].z_axis.truncate() + Vec3::Y).length() < 1e-5);
    }

    #[test]
    fn rejects_non_topological_parents() {
        let err = Skeleton::from_local_bind(
            vec!["a".into(), "b".into()],
            vec![Some(1), None],
            vec![LocalBind::default(), LocalBind::default()],
        )
        .expect_err("skeleton must be rejected");
        assert!(matches!(
            err,
            SkeletonError::ParentNotEarlier { joint: 0, parent: 1 }
        ));
    }

    #[test]
    fn from_local_bind_short_parents_is_mismatched_lengths() {
        let err = Skeleton::from_local_bind(
            vec!["a".into(), "b".into()],
            vec![None],
            vec![LocalBind::default(), LocalBind::default()],
        )
        .expect_err("skeleton must be rejected");
        assert!(matches!(
            err,
            SkeletonError::MismatchedLengths {
                names: 2,
                parents: 1,
                bind: 2,
                ..
            }
        ));
    }

    #[test]
    fn from_local_bind_parent_past_end_is_parent_not_earlier() {
        let err = Skeleton::from_local_bind(
            vec!["a".into(), "b".into()],
            vec![Some(99), None],
            vec![LocalBind::default(), LocalBind::default()],
        )
        .expect_err("skeleton must be rejected");
        assert!(matches!(
            err,
            SkeletonError::ParentNotEarlier { joint: 0, parent: 99 }
        ));
    }

    #[test]
    fn rejects_duplicate_names_and_empty() {
        let err = Skeleton::from_local_bind(
            vec!["a".into(), "a".into()],
            vec![None, Some(0)],
            vec![LocalBind::default(), LocalBind::default()],
        )
        .expect_err("skeleton must be rejected");
        assert!(matches!(err, SkeletonError::DuplicateName { .. }));

        let err = Skeleton::from_local_bind(
            vec!["".into(), "a".into()],
            vec![None, Some(0)],
            vec![LocalBind::default(), LocalBind::default()],
        )
        .expect_err("skeleton must be rejected");
        assert!(matches!(err, SkeletonError::EmptyName { joint: 0 }));
    }

    #[test]
    fn max_descendant_distance_measures_chain() {
        let sk = two_joint_skeleton();
        let d = sk.max_descendant_distance();
        assert_eq!(d[0], 0.5, "root reaches child at 0.5 m");
        assert_eq!(d[1], 0.0, "child has no descendants");
    }

    #[test]
    fn joint_index_lookup() {
        let sk = two_joint_skeleton();
        assert_eq!(sk.joint_index("child"), Some(1));
        assert_eq!(sk.joint_index("nope"), None);
    }
}
