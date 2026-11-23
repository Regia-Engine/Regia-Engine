// SPDX-License-Identifier: MPL-2.0
//! Skinning palettes (`specs/13` § 8).

/// Maximum joints per skin.
pub const MAX_JOINTS_PER_SKIN: usize = 256;

/// Maximum influences per vertex.
pub const MAX_INFLUENCES_PER_VERTEX: usize = 4;

/// Row-packed `Mat3x4` joint matrix (48 bytes). Matches `regia::mesh::Mat3x4` in WGSL.
///
/// Layout is three `vec4` rows, translation in `w`. The GPU storage array stride is 48 bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct JointMatrix {
    /// Rows of the 3×4 matrix packed as `m[row][col]` with `m[*][3]` translation.
    pub m: [[f32; 4]; 3],
}

impl JointMatrix {
    /// Identity joint transform.
    #[must_use]
    pub fn identity() -> Self {
        Self {
            m: [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]],
        }
    }

    /// Translation joint, in the same space as the mesh positions.
    #[must_use]
    pub fn translation(x: f32, y: f32, z: f32) -> Self {
        Self::from_mat4(regia_core::glam::Mat4::from_translation(
            regia_core::glam::Vec3::new(x, y, z),
        ))
    }

    /// Upper 3×4 of a column-major 4×4. Drops the perspective row.
    #[must_use]
    pub fn from_mat4(matrix: regia_core::glam::Mat4) -> Self {
        let columns = matrix.to_cols_array_2d();
        Self {
            m: [
                [columns[0][0], columns[1][0], columns[2][0], columns[3][0]],
                [columns[0][1], columns[1][1], columns[2][1], columns[3][1]],
                [columns[0][2], columns[1][2], columns[2][2], columns[3][2]],
            ],
        }
    }

    /// Transforms a point (`m * vec4(p, 1)`).
    #[must_use]
    pub fn transform_point(self, point: [f32; 3]) -> [f32; 3] {
        let m = self.m;
        [
            m[0][0] * point[0] + m[0][1] * point[1] + m[0][2] * point[2] + m[0][3],
            m[1][0] * point[0] + m[1][1] * point[1] + m[1][2] * point[2] + m[1][3],
            m[2][0] * point[0] + m[2][1] * point[1] + m[2][2] * point[2] + m[2][3],
        ]
    }
}

/// Linear-blend skinning of a point by four joint matrices.
///
/// A weight sum below `1e-5` returns `position` unchanged, matching the lab shaders.
#[must_use]
pub fn skin_position(position: [f32; 3], joints: &[JointMatrix; 4], weights: [f32; 4]) -> [f32; 3] {
    let weight_sum = weights[0] + weights[1] + weights[2] + weights[3];
    if weight_sum < 1e-5 {
        return position;
    }
    let mut out = [0.0; 3];
    for influence in 0..4 {
        let skinned = joints[influence].transform_point(position);
        out[0] += weights[influence] * skinned[0];
        out[1] += weights[influence] * skinned[1];
        out[2] += weights[influence] * skinned[2];
    }
    out
}

/// Most influences per vertex the compute skinning path reads ([`crate::SkinInfluences8`]).
pub const MAX_INFLUENCES_COMPUTE: usize = 8;

/// How a skin blends its joints ([`crate::SkinningOptions::method`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SkinningMethod {
    /// Linear blend skinning: the weighted sum of the joint matrices (the default, and what the
    /// vertex-shader path draws).
    #[default]
    Linear,
    /// Dual quaternion skinning (Kavan et al. 2008): the joints' rigid transforms blend as unit
    /// dual quaternions, so a twisting or bending joint keeps its volume (no candy-wrapper
    /// collapse). Joint scale and shear blend linearly before it. Compute path only.
    DualQuaternion,
}

impl SkinningMethod {
    /// `"linear"` or `"dual_quaternion"` (the agent protocol's names).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::DualQuaternion => "dual_quaternion",
        }
    }

    /// The method a protocol name (or `"dqs"`, `"lbs"`) names.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "linear" | "lbs" => Some(Self::Linear),
            "dual_quaternion" | "dqs" => Some(Self::DualQuaternion),
            _ => None,
        }
    }
}

/// One joint of a dual quaternion palette (80 bytes, the compute kernel's layout): the joint's
/// rotation and translation as a unit dual quaternion, and the scale / shear part `R⁻¹ A` of
/// its linear part `A` as three rows.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct DualQuatJoint {
    /// Rotation quaternion `x, y, z, w`.
    pub real: [f32; 4],
    /// Dual part: `½ · (t, 0) ⊗ real`.
    pub dual: [f32; 4],
    /// Rows of `R⁻¹ A` (`w` unused): identity for a rigid joint.
    pub scale: [[f32; 4]; 3],
}

impl DualQuatJoint {
    /// Whether the scale rows are the identity within `tolerance`.
    #[must_use]
    pub fn is_rigid(&self, tolerance: f32) -> bool {
        (0..3).all(|r| (0..3).all(|c| (self.scale[r][c] - f32::from(u8::from(r == c))).abs() <= tolerance))
    }
}

impl JointMatrix {
    /// The joint as a unit dual quaternion plus its scale / shear rows ([`DualQuatJoint`]).
    ///
    /// The rotation is the linear part with its columns normalised (a reflected joint keeps a
    /// proper rotation and its reflection goes into the scale rows); `R⁻¹ A` then holds every
    /// scale or shear the joint has, so rotation × scale rows × point is the joint's own
    /// transform for any matrix.
    #[must_use]
    pub fn to_dual_quat(self) -> DualQuatJoint {
        use regia_core::glam::{Mat3, Quat, Vec3};
        let m = self.m;
        let linear = Mat3::from_cols(
            Vec3::new(m[0][0], m[1][0], m[2][0]),
            Vec3::new(m[0][1], m[1][1], m[2][1]),
            Vec3::new(m[0][2], m[1][2], m[2][2]),
        );
        let t = Vec3::new(m[0][3], m[1][3], m[2][3]);
        let mut axes = [linear.x_axis, linear.y_axis, linear.z_axis];
        for axis in &mut axes {
            let len = axis.length();
            *axis = if len > 1e-12 { *axis / len } else { Vec3::ZERO };
        }
        let mut rot = Mat3::from_cols(axes[0], axes[1], axes[2]);
        if rot.determinant() < 0.0 {
            rot.z_axis = -rot.z_axis;
        }
        // Degenerate (a zero-scaled joint): no rotation, the scale rows carry the matrix.
        let real = if rot.determinant().abs() > 0.5 {
            Quat::from_mat3(&rot).normalize()
        } else {
            Quat::IDENTITY
        };
        let scale = Mat3::from_quat(real).transpose() * linear;
        let dual = Quat::from_xyzw(t.x, t.y, t.z, 0.0) * real * 0.5;
        let row = |r: usize| {
            let v = scale.row(r);
            [v.x, v.y, v.z, 0.0]
        };
        DualQuatJoint {
            real: real.to_array(),
            dual: dual.to_array(),
            scale: [row(0), row(1), row(2)],
        }
    }
}

/// A skinned vertex: position, normal and tangent (directions unnormalised, as the shaders
/// leave them).
pub type SkinnedFrame = ([f32; 3], [f32; 3], [f32; 3]);

/// CPU reference of the compute kernel (`skin_compute.wgsl`): skins `position`, `normal` and
/// `tangent` by `influences` (`(joint, weight)`, four or eight) with `method`. Linear blending
/// matches [`skin_position`] and the vertex shaders exactly (no weight normalisation);
/// out-of-range joints clamp to the last joint. A weight sum below `1e-5` returns the input.
#[must_use]
pub fn skin_vertex_reference(
    (position, normal, tangent): SkinnedFrame,
    influences: &[(u32, f32)],
    joints: &[JointMatrix],
    method: SkinningMethod,
) -> SkinnedFrame {
    use regia_core::glam::{Quat, Vec3, Vec4};
    let wsum: f32 = influences.iter().map(|i| i.1).sum();
    if wsum < 1e-5 || joints.is_empty() {
        return (position, normal, tangent);
    }
    let joint = |j: u32| joints[(j as usize).min(joints.len() - 1)];
    let (p, n, t) = (Vec3::from(position), Vec3::from(normal), Vec3::from(tangent));
    match method {
        SkinningMethod::Linear => {
            let (mut sp, mut sn, mut st) = (Vec3::ZERO, Vec3::ZERO, Vec3::ZERO);
            for &(j, w) in influences {
                let m = joint(j).m;
                let dir = |d: Vec3| {
                    Vec3::new(
                        m[0][0] * d.x + m[0][1] * d.y + m[0][2] * d.z,
                        m[1][0] * d.x + m[1][1] * d.y + m[1][2] * d.z,
                        m[2][0] * d.x + m[2][1] * d.y + m[2][2] * d.z,
                    )
                };
                sp += w * Vec3::from(joint(j).transform_point(position));
                sn += w * dir(n);
                st += w * dir(t);
            }
            (sp.into(), sn.into(), st.into())
        }
        SkinningMethod::DualQuaternion => {
            // Signs align to the heaviest influence (the first of equals), not slot 0: an unused
            // slot (weight 0, joint 0) must not pick the hemisphere.
            let mut heaviest = influences[0];
            for &inf in &influences[1..] {
                if inf.1 > heaviest.1 {
                    heaviest = inf;
                }
            }
            let pivot = Vec4::from(joint(heaviest.0).to_dual_quat().real);
            let (mut real, mut dual) = (Vec4::ZERO, Vec4::ZERO);
            let mut rows = [Vec3::ZERO; 3];
            let scaled = joints.iter().any(|j| !j.to_dual_quat().is_rigid(1e-4));
            for &(j, w) in influences {
                let dq = joint(j).to_dual_quat();
                let sign = if Vec4::from(dq.real).dot(pivot) < 0.0 {
                    -1.0
                } else {
                    1.0
                };
                real += w * sign * Vec4::from(dq.real);
                dual += w * sign * Vec4::from(dq.dual);
                for (r, row) in rows.iter_mut().enumerate() {
                    *row += w * Vec3::new(dq.scale[r][0], dq.scale[r][1], dq.scale[r][2]);
                }
            }
            let len = real.length().max(1e-12);
            let (real, dual) = (real / len, dual / len);
            let apply = |v: Vec3| {
                if scaled {
                    Vec3::new(rows[0].dot(v), rows[1].dot(v), rows[2].dot(v)) / wsum
                } else {
                    v
                }
            };
            let q = Quat::from_xyzw(real.x, real.y, real.z, real.w);
            let (rv, dv) = (real.truncate(), dual.truncate());
            let translation = 2.0 * (real.w * dv - dual.w * rv + rv.cross(dv));
            (
                (q * apply(p) + translation).into(),
                (q * apply(n)).into(),
                (q * apply(t)).into(),
            )
        }
    }
}

/// Joint palette for one skinned draw instance.
#[derive(Debug, Clone)]
pub struct SkinPalette {
    /// Joint matrices in bind pose or animated pose.
    pub joints: Vec<JointMatrix>,
    /// Previous frame palette. `FrameRenderer` does not read it: it keeps last frame's palette
    /// per object key itself for motion vectors.
    pub prev_joints: Vec<JointMatrix>,
}

impl SkinPalette {
    /// Creates a palette with `joint_count` identity joints.
    #[must_use]
    pub fn identity(joint_count: usize) -> Self {
        let joints = vec![JointMatrix::identity(); joint_count.min(MAX_JOINTS_PER_SKIN)];
        let prev_joints = joints.clone();
        Self { joints, prev_joints }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joint_matrix_matches_mat3x4_gpu_layout() {
        assert_eq!(size_of::<JointMatrix>(), 48);
        assert_eq!(align_of::<JointMatrix>(), 4);
    }

    #[test]
    fn from_mat4_keeps_translation_in_w() {
        let matrix = regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(2.0, -3.0, 4.0));
        let joint = JointMatrix::from_mat4(matrix);
        assert_eq!(joint.transform_point([1.0, 0.0, 0.0]), [3.0, -3.0, 4.0]);
    }

    #[test]
    fn zero_weights_skip_skinning() {
        let translated = JointMatrix::from_mat4(regia_core::glam::Mat4::from_translation(
            regia_core::glam::Vec3::X,
        ));
        let joints = [
            translated,
            JointMatrix::identity(),
            JointMatrix::identity(),
            JointMatrix::identity(),
        ];
        let position = [0.25, 0.5, -0.5];
        assert_eq!(skin_position(position, &joints, [0.0; 4]), position);
    }

    fn close(a: [f32; 3], b: [f32; 3], tolerance: f32) -> bool {
        (0..3).all(|k| (a[k] - b[k]).abs() <= tolerance)
    }

    #[test]
    fn one_joint_skins_the_same_with_either_method() {
        use regia_core::glam::{Mat4, Quat, Vec3};
        let joints = [
            JointMatrix::from_mat4(Mat4::from_rotation_translation(
                Quat::from_euler(regia_core::glam::EulerRot::XYZ, 0.4, -1.1, 2.3),
                Vec3::new(0.5, -2.0, 3.0),
            )),
            // Non-uniform scale: the scale rows carry it.
            JointMatrix::from_mat4(Mat4::from_scale_rotation_translation(
                Vec3::new(1.5, 0.5, 2.0),
                Quat::from_rotation_z(0.7),
                Vec3::new(-1.0, 0.0, 0.25),
            )),
            // A reflection keeps a proper rotation; the scale rows hold the mirror.
            JointMatrix::from_mat4(Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0))),
        ];
        let frame = ([0.3, -0.7, 1.2], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]);
        for (j, joint) in joints.iter().enumerate() {
            let dq = joint.to_dual_quat();
            let q = regia_core::glam::Quat::from_array(dq.real);
            assert!((q.length() - 1.0).abs() < 1e-5, "joint {j}: unit rotation");
            assert_eq!(dq.is_rigid(1e-4), j == 0, "joint {j}");
            let linear = skin_vertex_reference(frame, &[(0, 1.0)], &joints[j..=j], SkinningMethod::Linear);
            let dual =
                skin_vertex_reference(frame, &[(0, 1.0)], &joints[j..=j], SkinningMethod::DualQuaternion);
            assert!(close(linear.0, joint.transform_point(frame.0), 1e-5), "joint {j}");
            assert!(
                close(dual.0, linear.0, 1e-4),
                "joint {j}: {:?} vs {:?}",
                dual.0,
                linear.0
            );
            assert!(close(dual.1, linear.1, 1e-4), "joint {j}: normal");
            assert!(close(dual.2, linear.2, 1e-4), "joint {j}: tangent");
        }
    }

    #[test]
    fn dual_quaternions_keep_the_radius_of_a_half_twist() {
        use regia_core::glam::{Mat4, Quat};
        let joints = [
            JointMatrix::identity(),
            JointMatrix::from_mat4(Mat4::from_quat(Quat::from_rotation_y(170f32.to_radians()))),
        ];
        let p = ([0.1, 0.5, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]);
        let blend = [(0, 0.5), (1, 0.5)];
        let linear = skin_vertex_reference(p, &blend, &joints, SkinningMethod::Linear);
        let dual = skin_vertex_reference(p, &blend, &joints, SkinningMethod::DualQuaternion);
        let radius = |q: [f32; 3]| q[0].hypot(q[2]);
        assert!(radius(linear.0) < 0.01, "linear collapses: {}", radius(linear.0));
        assert!(
            (radius(dual.0) - 0.1).abs() < 1e-5,
            "dual keeps 0.1: {}",
            radius(dual.0)
        );
        assert!((dual.0[1] - 0.5).abs() < 1e-5);
        // Weights below the threshold leave the vertex alone.
        assert_eq!(
            skin_vertex_reference(p, &[(1, 0.0)], &joints, SkinningMethod::DualQuaternion),
            p
        );
        assert_eq!(
            SkinningMethod::from_name("dqs"),
            Some(SkinningMethod::DualQuaternion)
        );
        assert_eq!(
            SkinningMethod::from_name(SkinningMethod::Linear.name()),
            Some(SkinningMethod::Linear)
        );
        assert_eq!(size_of::<DualQuatJoint>(), 80);
    }

    #[test]
    fn one_influence_matches_the_joint() {
        let translated = JointMatrix::from_mat4(regia_core::glam::Mat4::from_translation(
            regia_core::glam::Vec3::new(0.0, 1.5, 0.0),
        ));
        let joints = [
            translated,
            JointMatrix::identity(),
            JointMatrix::identity(),
            JointMatrix::identity(),
        ];
        assert_eq!(
            skin_position([0.0, 0.0, 0.0], &joints, [1.0, 0.0, 0.0, 0.0]),
            [0.0, 1.5, 0.0]
        );
    }

    /// Review w11 #7: an unused slot 0 (weight 0, joint 0 at rest) does not pick the sign
    /// hemisphere. Joints 1 and 2 turn 179° and 181° about Z, half each: the blend is a 180° turn,
    /// where aligning to joint 0's identity flipped one of them and the blend collapsed to rest.
    #[test]
    fn a_zero_weight_slot_zero_is_not_the_dual_quaternion_pivot() {
        use regia_core::glam::{Mat4, Quat};
        let turn =
            |deg: f32| JointMatrix::from_mat4(Mat4::from_quat(Quat::from_rotation_z(deg.to_radians())));
        let joints = [JointMatrix::identity(), turn(179.0), turn(181.0)];
        let frame = ([1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        let (p, n, _) = skin_vertex_reference(
            frame,
            &[(0, 0.0), (1, 0.5), (2, 0.5), (0, 0.0)],
            &joints,
            SkinningMethod::DualQuaternion,
        );
        assert!((p[0] + 1.0).abs() < 1e-3 && p[1].abs() < 1e-3, "{p:?}");
        assert!((n[0] + 1.0).abs() < 1e-3, "{n:?}");
        // The same influences without the empty slot give the same answer.
        let (q, _, _) = skin_vertex_reference(
            frame,
            &[(1, 0.5), (2, 0.5)],
            &joints,
            SkinningMethod::DualQuaternion,
        );
        assert!(
            (regia_core::glam::Vec3::from(p) - regia_core::glam::Vec3::from(q)).length() < 1e-5,
            "{p:?} vs {q:?}"
        );
    }
}
