// SPDX-License-Identifier: MPL-2.0
//! Ray–triangle and ray–mesh casts: Möller–Trumbore, CPU skinning, face culling. The mesh
//! cast picks the nearest triangle brute force ([`raycast_mesh`]) or through a [`MeshBvh`]
//! ([`raycast_mesh_bvh`]); both finish the hit the same way.

use std::cell::RefCell;

use regia_core::math::{Mat4, Vec3};
use regia_render::{CpuMesh, JointMatrix, SkinInfluences8, SkinningMethod, skin_vertex_reference};

use crate::bvh::MeshBvh;

thread_local! {
    /// Skinned casts evaluate positions into this buffer instead of allocating a `Vec` per
    /// cast. It grows to the largest skinned mesh a thread has cast and stays.
    static SKIN_SCRATCH: RefCell<Vec<Vec3>> = const { RefCell::new(Vec::new()) };
}

/// Which triangle faces a ray may hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FaceCull {
    /// Both faces, like the id pass (default).
    #[default]
    None,
    /// Front faces only: counter-clockwise seen from the ray origin (the renderer's front).
    Back,
}

impl FaceCull {
    /// `none` or `back`.
    ///
    /// ```
    /// use regia_pick::FaceCull;
    ///
    /// assert_eq!(FaceCull::from_name("back"), Some(FaceCull::Back));
    /// assert_eq!(FaceCull::from_name("front"), None);
    /// ```
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "back" => Some(Self::Back),
            _ => None,
        }
    }
}

/// A ray–triangle intersection ([`ray_triangle`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayTriangle {
    /// Ray parameter: the hit is `origin + direction * t` (a distance when `direction` is unit).
    pub t: f32,
    /// Weights of the triangle's vertices `a`, `b`, `c` at the hit (they sum to 1).
    pub barycentric: Vec3,
    /// The triangle winds counter-clockwise seen from the ray origin.
    pub front_face: bool,
}

/// Ray–triangle intersection (Möller & Trumbore, "Fast, Minimum Storage Ray/Triangle
/// Intersection", Journal of Graphics Tools 2(1), 1997). Hits at `t > 0` only; edges count as
/// inside (a ray along a shared edge hits both triangles, never neither): the test has an absolute
/// barycentric slack of `1e-6`, so the accepted band outside an edge grows with the edge length
/// (about 1 mm on a 1 km edge).
///
/// ```
/// use regia_core::math::Vec3;
/// use regia_pick::{FaceCull, ray_triangle};
///
/// let (a, b, c) = (Vec3::ZERO, Vec3::X, Vec3::Z);
/// let down = Vec3::NEG_Y;
/// let above = Vec3::new(0.25, 2.0, 0.25);
///
/// let hit = ray_triangle(above, down, a, b, c, FaceCull::None).unwrap();
/// assert!((hit.t - 2.0).abs() < 1e-5);
/// assert!((hit.barycentric.element_sum() - 1.0).abs() < 1e-5);
///
/// // A ray that points away from the triangle misses.
/// assert!(ray_triangle(above, Vec3::Y, a, b, c, FaceCull::None).is_none());
/// ```
#[must_use]
pub fn ray_triangle(
    origin: Vec3,
    direction: Vec3,
    a: Vec3,
    b: Vec3,
    c: Vec3,
    cull: FaceCull,
) -> Option<RayTriangle> {
    crate::stats::note_triangle();
    const EDGE: f32 = 1e-6;
    let e1 = b - a;
    let e2 = c - a;
    let p = direction.cross(e2);
    let det = e1.dot(p);
    // Parallel (or degenerate), relative to the triangle's and the direction's scale.
    let scale = e1.length() * e2.length() * direction.length();
    // Written so a NaN determinant is rejected too.
    if det.is_nan() || det.abs() <= 1e-7 * scale {
        return None;
    }
    let front_face = det > 0.0;
    if cull == FaceCull::Back && !front_face {
        return None;
    }
    let inv = 1.0 / det;
    let s = origin - a;
    let u = s.dot(p) * inv;
    if !(-EDGE..=1.0 + EDGE).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = direction.dot(q) * inv;
    if v < -EDGE || u + v > 1.0 + EDGE {
        return None;
    }
    let t = e2.dot(q) * inv;
    if t <= 0.0 || !t.is_finite() {
        return None;
    }
    Some(RayTriangle {
        t,
        barycentric: Vec3::new(1.0 - u - v, u, v),
        front_face,
    })
}

/// The nearest triangle of a mesh a ray hits ([`raycast_mesh`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshHit {
    /// Triangle index in the index list (`indices[3 * triangle..]`).
    pub triangle: u32,
    /// The triangle's three vertex indices.
    pub indices: [u32; 3],
    /// Weights of the three vertices at the hit (sum 1).
    pub barycentric: Vec3,
    /// Distance from the ray origin, metres.
    pub distance: f32,
    /// Hit point in the same frame as the ray and the mesh `GlobalTransform` (local coordinates
    /// after a floating-origin rebase; world space when `OriginOffset` is zero).
    pub position: Vec3,
    /// World-space unit normal of the triangle's front (counter-clockwise) side, as drawn
    /// (skinned, then the model matrix). A back-face hit keeps the front side's normal.
    pub normal: Vec3,
    /// World-space unit vertex normal interpolated at the hit (the shading normal before normal
    /// maps), on the same side as [`Self::normal`].
    pub smooth_normal: Vec3,
    /// The ray hit the triangle's front side.
    pub front_face: bool,
}

/// Joint palette and blend used by [`raycast_mesh_with`].
///
/// [`raycast_mesh`] passes four-weight linear blend. The entity cast passes the method the
/// renderer draws, including an eight-influence table when that table matches the mesh.
#[derive(Clone, Copy)]
pub(crate) struct PoseSkin<'a> {
    /// Joint matrices. Empty means the mesh is rigid.
    pub(crate) joints: &'a [JointMatrix],
    /// Linear blend or dual quaternions.
    pub(crate) method: SkinningMethod,
    /// Eight influences per vertex. `None` uses each vertex's own four weights.
    pub(crate) influences8: Option<&'a [SkinInfluences8]>,
}

impl<'a> PoseSkin<'a> {
    pub(crate) fn linear4(joints: &'a [JointMatrix]) -> Self {
        Self {
            joints,
            method: SkinningMethod::Linear,
            influences8: None,
        }
    }

    fn uses_reference(self) -> bool {
        self.method != SkinningMethod::Linear || self.influences8.is_some()
    }
}

/// The skinned object-space position and normal of vertex `i`.
///
/// Four-weight linear blend calls `skin_position`. Dual quaternions and an eight-influence
/// table call `skin_vertex_reference`, the CPU reference of the compute kernel. A joint index
/// past the palette clamps to the last joint.
fn skinned_vertex(mesh: &CpuMesh, pose: PoseSkin<'_>, i: usize) -> (Vec3, Vec3) {
    crate::stats::note_skin();
    let v = &mesh.vertices[i];
    if pose.uses_reference() {
        let mut influences = [(0u32, 0.0f32); 8];
        let count = if let Some(table) = pose.influences8 {
            let row = &table[i];
            for (slot, (&joint, &weight)) in influences.iter_mut().zip(row.joints.iter().zip(&row.weights)) {
                *slot = (joint, weight);
            }
            8
        } else {
            for (slot, (&joint, &weight)) in influences.iter_mut().zip(v.joints.iter().zip(&v.weights)) {
                *slot = (joint, weight);
            }
            4
        };
        let tangent = [v.tangent[0], v.tangent[1], v.tangent[2]];
        let (position, normal, _) = skin_vertex_reference(
            (v.position, v.normal, tangent),
            &influences[..count],
            pose.joints,
            pose.method,
        );
        return (Vec3::from_array(position), Vec3::from_array(normal));
    }
    let joint = |lane: usize| {
        // An index past the palette clamps to the last joint (as `skin_vertex_reference` does);
        // an empty palette is the identity.
        pose.joints
            .get((v.joints[lane] as usize).min(pose.joints.len().saturating_sub(1)))
            .copied()
            .unwrap_or(JointMatrix::identity())
    };
    let joints = [joint(0), joint(1), joint(2), joint(3)];
    let position = Vec3::from_array(regia_render::skin_position(v.position, &joints, v.weights));
    let weight_sum: f32 = v.weights.iter().sum();
    let normal = Vec3::from_array(v.normal);
    if weight_sum < 1e-5 {
        return (position, normal);
    }
    let mut out = Vec3::ZERO;
    for (joint, weight) in joints.iter().zip(v.weights) {
        let m = joint.m;
        out += weight
            * Vec3::new(
                m[0][0] * normal.x + m[0][1] * normal.y + m[0][2] * normal.z,
                m[1][0] * normal.x + m[1][1] * normal.y + m[1][2] * normal.z,
                m[2][0] * normal.x + m[2][1] * normal.y + m[2][2] * normal.z,
            );
    }
    (position, out)
}

/// Casts a ray (the `GlobalTransform` frame; `direction` need not be unit) against `indices` of
/// `mesh` drawn with `model` and, when non-empty, the joint palette `joints` (position =
/// `model` × skinned position, four-weight linear blend, as the vertex-shader path draws it).
/// Returns the nearest hit; triangles with an out-of-range index are skipped.
#[must_use]
pub fn raycast_mesh(
    mesh: &CpuMesh,
    indices: &[u32],
    model: Mat4,
    joints: &[JointMatrix],
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
) -> Option<MeshHit> {
    raycast_mesh_with(
        mesh,
        indices,
        model,
        PoseSkin::linear4(joints),
        origin,
        direction,
        cull,
    )
}

/// [`raycast_mesh`] with the blend in `pose` (dual quaternions and eight influences when the
/// renderer draws those). An empty palette ignores `pose` and casts the rigid mesh.
#[must_use]
pub(crate) fn raycast_mesh_with(
    mesh: &CpuMesh,
    indices: &[u32],
    model: Mat4,
    pose: PoseSkin<'_>,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
) -> Option<MeshHit> {
    cast_mesh(
        mesh,
        indices,
        model,
        pose,
        None,
        origin,
        direction,
        cull,
        f32::INFINITY,
    )
}

/// [`raycast_mesh_with`] with `model`'s inverse already computed by the caller (counted) and a
/// farthest-hit distance: triangle hits farther than `max_t` along the unit ray are ignored
/// (an equal distance still counts).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub(crate) fn cast_mesh(
    mesh: &CpuMesh,
    indices: &[u32],
    model: Mat4,
    pose: PoseSkin<'_>,
    inverse: Option<Mat4>,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
    max_t: f32,
) -> Option<MeshHit> {
    let direction = direction.normalize_or_zero();
    if direction == Vec3::ZERO || !origin.is_finite() || !model.is_finite() || max_t <= 0.0 {
        return None;
    }
    let count = mesh.vertices.len();
    let skinned = !pose.joints.is_empty();
    // Rigid meshes are cast in object space (the ray parameter is the same in both spaces);
    // a mirroring model flips which winding faces the eye. Skinned meshes are skinned to world.
    let object_inverse = if skinned {
        None
    } else {
        let inverse = match inverse {
            Some(inverse) if inverse.is_finite() => inverse,
            _ => {
                crate::stats::note_inverse();
                let inverse = model.inverse();
                if !inverse.is_finite() {
                    return None;
                }
                inverse
            }
        };
        Some(inverse)
    };
    let mut world_positions = Vec::new();
    let (ray_origin, ray_dir, flip, positions): (Vec3, Vec3, bool, Option<&[Vec3]>) = if skinned {
        world_positions = SKIN_SCRATCH.with(|scratch| {
            let mut scratch = scratch.borrow_mut();
            if scratch.capacity() < count {
                crate::stats::note_skin_buffer_grow();
                let spare = count - scratch.capacity();
                scratch.reserve(spare);
            }
            scratch.clear();
            scratch.extend((0..count).map(|i| {
                let skinned = skinned_vertex(mesh, pose, i).0;
                model.transform_point3(skinned)
            }));
            std::mem::take(&mut *scratch)
        });
        (origin, direction, false, Some(world_positions.as_slice()))
    } else {
        let inverse = object_inverse?;
        (
            inverse.transform_point3(origin),
            inverse.transform_vector3(direction),
            model.determinant() < 0.0,
            None,
        )
    };
    let position = |i: u32| -> Vec3 {
        match positions {
            Some(positions) => positions[i as usize],
            None => Vec3::from_array(mesh.vertices[i as usize].position),
        }
    };
    let mut best: Option<(u32, [u32; 3], RayTriangle)> = None;
    for (triangle, tri) in indices.as_chunks::<3>().0.iter().enumerate() {
        if tri.iter().any(|&i| i as usize >= count) {
            continue;
        }
        let Some(mut hit) = ray_triangle(
            ray_origin,
            ray_dir,
            position(tri[0]),
            position(tri[1]),
            position(tri[2]),
            FaceCull::None,
        ) else {
            continue;
        };
        hit.front_face ^= flip;
        if cull == FaceCull::Back && !hit.front_face {
            continue;
        }
        if hit.t > max_t {
            continue;
        }
        if best.is_none_or(|(_, _, b)| hit.t < b.t) {
            best = Some((triangle as u32, *tri, hit));
        }
    }
    let Some((triangle, tri, hit)) = best else {
        if skinned {
            SKIN_SCRATCH.with(|scratch| {
                *scratch.borrow_mut() = world_positions;
            });
        }
        return None;
    };
    let result = finish_hit(
        mesh,
        model,
        pose,
        positions,
        object_inverse,
        origin,
        direction,
        triangle,
        tri,
        hit,
    );
    // Return the scratch buffer before the caller reads the result.
    if skinned {
        SKIN_SCRATCH.with(|scratch| {
            *scratch.borrow_mut() = world_positions;
        });
    }
    Some(result)
}

/// Object-space bounds of a mesh posed by `joints`, from its bind bounds.
///
/// A conservative bound for a convex blend: every skinned position is a weighted average of
/// per-joint images of bind-position points (weights summing to at most one, the remainder
/// staying at the bind position), so the corners of every used joint's image of the bind box
/// contain the result. `None` when a weight is negative or the weights of a vertex sum past
/// one (the blend is then not a convex combination — the ray is tested against the skinned
/// triangles), or when the blend is not linear (dual quaternions normalize the blend and can
/// leave the hull slightly). A vertex with no weight stays in the bind bounds; a partial
/// weight also includes the origin, because the blend is not renormalised.
#[must_use]
pub(crate) fn posed_object_bounds(
    mesh: &CpuMesh,
    joints: &[JointMatrix],
    influences8: Option<&[SkinInfluences8]>,
    bind: (Vec3, Vec3),
) -> Option<(Vec3, Vec3)> {
    if joints.is_empty() {
        return Some(bind);
    }
    let mut include_bind = false;
    let mut include_origin = false;
    let mut used: Vec<bool> = vec![false; joints.len()];
    let mut extra: Option<usize> = None;
    for (index, vertex) in mesh.vertices.iter().enumerate() {
        let (joint_ids, weights): (&[u32], &[f32]) = match influences8 {
            Some(table) => {
                let row = table.get(index)?;
                (&row.joints, &row.weights)
            }
            None => (&vertex.joints, &vertex.weights),
        };
        if weights.iter().any(|weight| *weight < 0.0) {
            return None;
        }
        let sum: f32 = weights.iter().sum();
        if sum > 1.0 + 1e-4 {
            return None;
        }
        if sum < 1e-5 {
            include_bind = true;
            continue;
        }
        if sum < 1.0 - 1e-4 {
            include_origin = true;
        }
        for (lane, weight) in weights.iter().enumerate() {
            if *weight <= 0.0 {
                continue;
            }
            let id = (*joint_ids.get(lane)?).min(joints.len().saturating_sub(1) as u32);
            match used.get_mut(id as usize) {
                Some(slot) => *slot = true,
                None => extra = Some(id as usize),
            }
        }
    }
    let mut acc: Option<(Vec3, Vec3)> = None;
    let mut include = |point: Vec3| {
        acc = Some(match acc {
            None => (point, point),
            Some((lo, hi)) => (lo.min(point), hi.max(point)),
        });
    };
    if include_bind {
        include(bind.0);
        include(bind.1);
    }
    if include_origin {
        include(Vec3::ZERO);
    }
    let mut cover_joint = |joint: JointMatrix| {
        let (lo, hi) = bind;
        for corner in [
            Vec3::new(lo.x, lo.y, lo.z),
            Vec3::new(hi.x, lo.y, lo.z),
            Vec3::new(lo.x, hi.y, lo.z),
            Vec3::new(hi.x, hi.y, lo.z),
            Vec3::new(lo.x, lo.y, hi.z),
            Vec3::new(hi.x, lo.y, hi.z),
            Vec3::new(lo.x, hi.y, hi.z),
            Vec3::new(hi.x, hi.y, hi.z),
        ] {
            include(Vec3::from_array(joint.transform_point(corner.to_array())));
        }
    };
    for (index, on) in used.iter().enumerate() {
        if *on {
            cover_joint(joints[index]);
        }
    }
    if let Some(index) = extra {
        cover_joint(joints[index]);
    }
    acc
}
/// [`raycast_mesh`] for a rigid mesh through its [`MeshBvh`] (`bvh` built from `mesh`): the same
/// nearest triangle without testing them all. `joints` is empty by construction.
#[must_use]
pub fn raycast_mesh_bvh(
    mesh: &CpuMesh,
    bvh: &MeshBvh,
    model: Mat4,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
) -> Option<MeshHit> {
    cast_mesh_bvh(mesh, bvh, model, None, origin, direction, cull, f32::INFINITY)
}

/// [`raycast_mesh_bvh`] with `model`'s inverse already computed by the caller (counted) and a
/// farthest-hit distance the tree prunes against.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub(crate) fn cast_mesh_bvh(
    mesh: &CpuMesh,
    bvh: &MeshBvh,
    model: Mat4,
    inverse: Option<Mat4>,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
    max_t: f32,
) -> Option<MeshHit> {
    let direction = direction.normalize_or_zero();
    if direction == Vec3::ZERO || !origin.is_finite() || !model.is_finite() || max_t <= 0.0 {
        return None;
    }
    let inverse = match inverse {
        Some(inverse) if inverse.is_finite() => inverse,
        _ => {
            crate::stats::note_inverse();
            let inverse = model.inverse();
            if !inverse.is_finite() {
                return None;
            }
            inverse
        }
    };
    let flip = model.determinant() < 0.0;
    let (triangle, hit) = bvh.nearest_within(
        mesh,
        inverse.transform_point3(origin),
        inverse.transform_vector3(direction),
        flip,
        cull,
        max_t,
    )?;
    let tri = bvh.triangle_indices(triangle)?;
    Some(finish_hit(
        mesh,
        model,
        PoseSkin::linear4(&[]),
        None,
        Some(inverse),
        origin,
        direction,
        triangle,
        tri,
        hit,
    ))
}

/// The world-space normals and position of the winning triangle (shared by both casts).
#[allow(clippy::too_many_arguments)]
fn finish_hit(
    mesh: &CpuMesh,
    model: Mat4,
    pose: PoseSkin<'_>,
    world_positions: Option<&[Vec3]>,
    inverse: Option<Mat4>,
    origin: Vec3,
    direction: Vec3,
    triangle: u32,
    tri: [u32; 3],
    hit: RayTriangle,
) -> MeshHit {
    let skinned = !pose.joints.is_empty();
    let world = |i: u32| match world_positions {
        Some(positions) => positions[i as usize],
        None => model.transform_point3(Vec3::from_array(mesh.vertices[i as usize].position)),
    };
    let (a, b, c) = (world(tri[0]), world(tri[1]), world(tri[2]));
    let normal = (b - a).cross(c - a).normalize_or_zero();
    let w = hit.barycentric;
    let smooth_local = if skinned {
        w.x * skinned_vertex(mesh, pose, tri[0] as usize).1
            + w.y * skinned_vertex(mesh, pose, tri[1] as usize).1
            + w.z * skinned_vertex(mesh, pose, tri[2] as usize).1
    } else {
        let n = |i: u32| Vec3::from_array(mesh.vertices[i as usize].normal);
        w.x * n(tri[0]) + w.y * n(tri[1]) + w.z * n(tri[2])
    };
    // The normal matrix is the inverse's transpose; the entity cast shares its one inverse.
    let normal_matrix = match inverse {
        Some(inverse) => inverse.transpose(),
        None => {
            crate::stats::note_inverse();
            model.inverse().transpose()
        }
    };
    let mut smooth_normal = normal_matrix.transform_vector3(smooth_local).normalize_or_zero();
    if smooth_normal == Vec3::ZERO {
        smooth_normal = normal;
    } else if smooth_normal.dot(normal) < 0.0 {
        smooth_normal = -smooth_normal;
    }
    MeshHit {
        triangle,
        indices: tri,
        barycentric: w,
        distance: hit.t,
        position: origin + direction * hit.t,
        normal,
        smooth_normal,
        front_face: hit.front_face,
    }
}

#[cfg(test)]
mod tests {
    use regia_core::math::{Mat4, Vec3};
    use regia_render::{CpuMesh, JointMatrix, LabVertex};

    use super::{FaceCull, ray_triangle, raycast_mesh};

    fn vertex(position: [f32; 3], normal: [f32; 3]) -> LabVertex {
        LabVertex::from_pos_normal_uv(position, normal, [0.0, 0.0])
    }

    /// A single triangle in the z = 0 plane: (0,0), (2,0), (0,2). Front faces +Z.
    fn xy_triangle(normal: [f32; 3]) -> CpuMesh {
        CpuMesh {
            vertices: vec![
                vertex([0.0, 0.0, 0.0], normal),
                vertex([2.0, 0.0, 0.0], normal),
                vertex([0.0, 2.0, 0.0], normal),
            ],
            indices: vec![0, 1, 2],
        }
    }

    #[test]
    fn ray_triangle_is_exact_on_a_vertex_an_edge_and_the_interior() {
        let (a, b, c) = (Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0), Vec3::new(0.0, 2.0, 0.0));
        let hit = |origin: Vec3| ray_triangle(origin, Vec3::NEG_Z, a, b, c, FaceCull::None).expect("hit");

        let interior = hit(Vec3::new(0.5, 0.5, 5.0));
        assert_eq!(interior.t, 5.0);
        assert_eq!(interior.barycentric, Vec3::new(0.5, 0.25, 0.25));
        assert!(interior.front_face);

        let edge = hit(Vec3::new(1.0, 0.0, 4.0));
        assert_eq!(edge.t, 4.0);
        assert_eq!(edge.barycentric, Vec3::new(0.5, 0.5, 0.0));

        let corner = hit(Vec3::new(0.0, 0.0, 3.0));
        assert_eq!(corner.t, 3.0);
        assert_eq!(corner.barycentric, Vec3::new(1.0, 0.0, 0.0));

        // A ray that starts on the surface has t = 0 and is not a hit from either side.
        assert!(ray_triangle(Vec3::new(0.5, 0.5, 0.0), Vec3::NEG_Z, a, b, c, FaceCull::None).is_none());
        assert!(ray_triangle(Vec3::new(0.5, 0.5, 0.0), Vec3::Z, a, b, c, FaceCull::None).is_none());
    }

    #[test]
    fn the_nearest_listed_triangle_wins_and_a_bad_index_is_skipped() {
        let mut mesh = xy_triangle([0.0, 0.0, 1.0]);
        // The nearer copy (z = 2) is listed second, so the winner is not just the first triangle.
        mesh.vertices.extend([
            vertex([0.0, 0.0, 2.0], [0.0, 0.0, 1.0]),
            vertex([2.0, 0.0, 2.0], [0.0, 0.0, 1.0]),
            vertex([0.0, 2.0, 2.0], [0.0, 0.0, 1.0]),
        ]);
        mesh.indices = vec![0, 1, 2, 3, 4, 5];
        let origin = Vec3::new(0.5, 0.5, 9.0);
        let hit = raycast_mesh(
            &mesh,
            &mesh.indices,
            Mat4::IDENTITY,
            &[],
            origin,
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("near");
        assert_eq!(hit.triangle, 1);
        assert_eq!(hit.indices, [3, 4, 5]);
        assert_eq!(hit.distance, 7.0);
        assert_eq!(hit.position, Vec3::new(0.5, 0.5, 2.0));
        assert_eq!(hit.barycentric, Vec3::new(0.5, 0.25, 0.25));
        assert_eq!(hit.normal, Vec3::Z);
        assert!(hit.front_face);

        // The same plane twice: equal distance keeps the earlier triangle.
        let mut tied = xy_triangle([0.0, 0.0, 1.0]);
        let copy = tied.vertices.clone();
        tied.vertices.extend(copy);
        tied.indices = vec![0, 1, 2, 3, 4, 5];
        let tie = raycast_mesh(
            &tied,
            &tied.indices,
            Mat4::IDENTITY,
            &[],
            origin,
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("tie");
        assert_eq!(tie.triangle, 0);
        assert_eq!(tie.distance, 9.0);

        // An out-of-range first triangle is skipped; the second still hits.
        let one = xy_triangle([0.0, 0.0, 1.0]);
        let skipped = raycast_mesh(
            &one,
            &[u32::MAX, 0, 1, 0, 1, 2],
            Mat4::IDENTITY,
            &[],
            origin,
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("second triangle");
        assert_eq!(skipped.triangle, 1);
        assert_eq!(skipped.indices, [0, 1, 2]);
        assert_eq!(skipped.distance, 9.0);
        assert_eq!(skipped.position, origin + Vec3::NEG_Z * skipped.distance);

        // A shared edge hits both triangles; the earlier one is kept.
        let mut quad = xy_triangle([0.0, 0.0, 1.0]);
        quad.vertices.push(vertex([2.0, 2.0, 0.0], [0.0, 0.0, 1.0]));
        quad.indices = vec![0, 1, 2, 1, 3, 2];
        let edge = raycast_mesh(
            &quad,
            &quad.indices,
            Mat4::IDENTITY,
            &[],
            Vec3::new(1.0, 1.0, 6.0),
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("shared edge");
        assert_eq!(edge.triangle, 0);
        assert_eq!(edge.barycentric, Vec3::new(0.0, 0.5, 0.5));
        assert_eq!(edge.distance, 6.0);
        assert_eq!(edge.position, Vec3::new(1.0, 1.0, 0.0));
    }

    #[test]
    fn back_faces_keep_the_front_normal_and_a_mirror_flips_the_cull() {
        let mesh = xy_triangle([0.0, 0.0, 1.0]);
        let from_back = Vec3::new(0.5, 0.5, -5.0);
        assert!(
            raycast_mesh(
                &mesh,
                &mesh.indices,
                Mat4::IDENTITY,
                &[],
                from_back,
                Vec3::Z,
                FaceCull::Back,
            )
            .is_none()
        );
        let back = raycast_mesh(
            &mesh,
            &mesh.indices,
            Mat4::IDENTITY,
            &[],
            from_back,
            Vec3::Z,
            FaceCull::None,
        )
        .expect("back face");
        assert!(!back.front_face);
        assert_eq!(back.normal, Vec3::Z);
        assert_eq!(back.smooth_normal, Vec3::Z);
        assert_eq!(back.distance, 5.0);
        assert_eq!(back.position, Vec3::new(0.5, 0.5, 0.0));

        // Vertex normals of zero fall back to the geometric normal.
        let mut flat = xy_triangle([0.0, 0.0, 0.0]);
        let fallen = raycast_mesh(
            &flat,
            &flat.indices,
            Mat4::IDENTITY,
            &[],
            Vec3::new(0.5, 0.5, 5.0),
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("zero normals");
        assert_eq!(fallen.normal, Vec3::Z);
        assert_eq!(fallen.smooth_normal, fallen.normal);

        // A smooth normal that points backward is flipped onto the front side.
        for v in &mut flat.vertices {
            v.normal = [0.0, 0.0, -1.0];
        }
        let flipped = raycast_mesh(
            &flat,
            &flat.indices,
            Mat4::IDENTITY,
            &[],
            Vec3::new(0.5, 0.5, 5.0),
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("flipped smooth normal");
        assert_eq!(flipped.normal, Vec3::Z);
        assert_eq!(flipped.smooth_normal, Vec3::Z);

        // Translation moves the plane; the ray parameter stays the world distance.
        let moved = raycast_mesh(
            &mesh,
            &mesh.indices,
            Mat4::from_translation(Vec3::new(0.0, 0.0, 3.0)),
            &[],
            Vec3::new(0.5, 0.5, 8.0),
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("translated");
        assert_eq!(moved.distance, 5.0);
        assert_eq!(moved.position, Vec3::new(0.5, 0.5, 3.0));
        assert_eq!(moved.normal, Vec3::Z);
        assert!(moved.front_face);

        // An X mirror reverses winding, so the world-facing side is a back face.
        let mirror = Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0));
        let aimed = Vec3::new(-0.5, 0.5, 5.0);
        assert!(
            raycast_mesh(
                &mesh,
                &mesh.indices,
                mirror,
                &[],
                aimed,
                Vec3::NEG_Z,
                FaceCull::Back
            )
            .is_none()
        );
        let mirrored = raycast_mesh(
            &mesh,
            &mesh.indices,
            mirror,
            &[],
            aimed,
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("mirror");
        assert!(!mirrored.front_face);
        assert_eq!(mirrored.normal, Vec3::NEG_Z);
        assert_eq!(mirrored.smooth_normal, Vec3::NEG_Z);
        assert_eq!(mirrored.distance, 5.0);
        assert_eq!(mirrored.position, Vec3::new(-0.5, 0.5, 0.0));
    }

    #[test]
    fn zero_weights_stay_put_and_a_rigid_joint_skins_position_and_normal() {
        let mut mesh = xy_triangle([0.0, 0.0, 1.0]);
        let yaw_90 = JointMatrix {
            // x' = z, y' = y, z' = -x. A +Z normal becomes +X.
            m: [[0.0, 0.0, 1.0, 0.0], [0.0, 1.0, 0.0, 0.0], [-1.0, 0.0, 0.0, 0.0]],
        };
        let lift = JointMatrix::translation(0.0, 0.0, 4.0);
        let model = Mat4::from_translation(Vec3::new(0.0, 0.0, 1.0));

        let cast = |mesh: &CpuMesh, joints: &[JointMatrix], origin: Vec3, direction: Vec3| {
            raycast_mesh(
                mesh,
                &mesh.indices,
                model,
                joints,
                origin,
                direction,
                FaceCull::None,
            )
        };

        let still = cast(&mesh, &[lift], Vec3::new(0.5, 0.5, 9.0), Vec3::NEG_Z).expect("zero weight");
        // Weights are zero, so the joint does nothing and only the model translation applies.
        assert_eq!(still.distance, 8.0);
        assert_eq!(still.position, Vec3::new(0.5, 0.5, 1.0));
        assert_eq!(still.normal, Vec3::Z);
        assert_eq!(still.smooth_normal, Vec3::Z);

        for v in &mut mesh.vertices {
            v.weights = [1.0, 0.0, 0.0, 0.0];
        }
        let lifted = cast(&mesh, &[lift], Vec3::new(0.5, 0.5, 9.0), Vec3::NEG_Z).expect("lifted");
        assert_eq!(lifted.distance, 4.0);
        assert_eq!(lifted.position, Vec3::new(0.5, 0.5, 5.0));
        assert_eq!(lifted.normal, Vec3::Z);
        assert_eq!(lifted.barycentric, Vec3::new(0.5, 0.25, 0.25));
        assert!(lifted.front_face);

        // The model shifts the yawed triangle by +1 z, so this ray meets it on the far edge.
        let turned = cast(&mesh, &[yaw_90], Vec3::new(5.0, 0.5, -0.5), Vec3::NEG_X).expect("yaw");
        assert_eq!(turned.distance, 5.0);
        assert_eq!(turned.position, Vec3::new(0.0, 0.5, -0.5));
        assert_eq!(turned.normal, Vec3::X);
        assert_eq!(turned.smooth_normal, Vec3::X);
        assert_eq!(turned.barycentric, Vec3::new(0.0, 0.75, 0.25));
        assert!(turned.front_face);

        // The model is applied after skinning, so the yawed triangle shifts by +1 z.
        let shifted = cast(&mesh, &[yaw_90], Vec3::new(5.0, 0.5, 0.5), Vec3::NEG_X).expect("yaw plus model");
        assert_eq!(shifted.position, Vec3::new(0.0, 0.5, 0.5));
        assert_eq!(shifted.distance, 5.0);
        assert_eq!(shifted.barycentric, Vec3::new(0.5, 0.25, 0.25));
        assert_eq!(shifted.normal, Vec3::X);
    }
}
