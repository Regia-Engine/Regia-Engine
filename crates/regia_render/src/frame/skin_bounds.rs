// SPDX-License-Identifier: MPL-2.0
//! Conservative bounds of linear-blend skinned meshes from their joint palettes (ADR-248), so a
//! skinned draw is frustum-culled like a rigid one instead of always drawing.
//!
//! A skinned vertex is `Σ wᵢ Jᵢ p` (`skin_position` in `mesh.wgsl`). Once per mesh, every joint
//! gets the bind-space box of the vertices it influences (weight > 0). Per frame each box goes
//! through its joint matrix (Arvo's box transform: exact box of the transformed box) and the
//! boxes are unioned: a vertex with weights summing to 1 is a convex combination of points that
//! each lie in one of those boxes, so it lies in their union's box. Vertices the shaders leave
//! unskinned (weight sum < 1e-5) add their bind box untransformed. A small weight-sum error is
//! padded for; negative weights, a larger error, a weighted joint index past the palette, or a
//! non-finite value leave the mesh without a bound (it always draws, as before).
//!
//! Per-frame cost is one 3×4 box transform per influencing joint; the per-mesh walk over the
//! vertices runs once per mesh key and vertex count, like the rigid-skin fold check.
//!
//! The same bound is the skinned caster's bound in the shadow page caches (ADR-5418): a bounded
//! skinned caster pages like a rigid one and joins the dynamic-caster class (`specs/88`), because
//! its pose can move inside the bound.
//!
//! The joint cap: a skinned draw whose vertices weight a joint at or past its palette's length,
//! or at or past [`MAX_JOINTS_PER_SKIN`], would read another draw's joints from the frame's packed
//! palette. The frame refuses it (no pass draws it) and counts it
//! ([`crate::DrawIntegrity::skinned_refused`]), as the cooked path refuses a skin over the cap
//! (`CookedSkinError::TooManyJoints`).

use std::sync::Arc;

use super::{FrameMesh, FrameRenderer, LOD_DITHER_MESH_TAG};
use crate::lab_frame::LabVertex;
use crate::skin::{JointMatrix, MAX_JOINTS_PER_SKIN};

/// Weight sum below which the shaders leave a vertex unskinned (`skin_position`).
const UNSKINNED_WEIGHT_SUM: f32 = 1e-5;
/// Largest `|Σw − 1|` of a skinned vertex the bound pads for; beyond it the mesh has no bound.
const WEIGHT_SUM_TOLERANCE: f32 = 0.01;

/// An axis-aligned box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Box3 {
    pub(crate) min: [f32; 3],
    pub(crate) max: [f32; 3],
}

impl Box3 {
    fn point(p: [f32; 3]) -> Self {
        Self { min: p, max: p }
    }

    fn grow(&mut self, p: [f32; 3]) {
        for ((min, max), v) in self.min.iter_mut().zip(&mut self.max).zip(p) {
            *min = min.min(v);
            *max = max.max(v);
        }
    }

    fn union(&mut self, other: &Self) {
        self.grow(other.min);
        self.grow(other.max);
    }

    fn padded(mut self, pad: f32) -> Self {
        for axis in 0..3 {
            self.min[axis] -= pad;
            self.max[axis] += pad;
        }
        self
    }

    fn center_half(&self) -> ([f32; 3], [f32; 3]) {
        let mut center = [0.0; 3];
        let mut half = [0.0; 3];
        for axis in 0..3 {
            center[axis] = (self.min[axis] + self.max[axis]) * 0.5;
            half[axis] = (self.max[axis] - self.min[axis]) * 0.5;
        }
        (center, half)
    }

    fn is_finite(&self) -> bool {
        self.min.iter().chain(&self.max).all(|v| v.is_finite())
    }
}

/// The box of `b` transformed by the affine map `rows` (3×4, translation in column 3): the
/// center goes through the map, each half extent through the absolute linear part.
fn transform_box(rows: &[[f32; 4]; 3], b: &Box3) -> Box3 {
    let (c, h) = b.center_half();
    let mut out = Box3::point([0.0; 3]);
    for (i, row) in rows.iter().enumerate() {
        let center = row[0] * c[0] + row[1] * c[1] + row[2] * c[2] + row[3];
        let half = row[0].abs() * h[0] + row[1].abs() * h[1] + row[2].abs() * h[2];
        out.min[i] = center - half;
        out.max[i] = center + half;
    }
    out
}

/// What one skinned mesh's vertices allow: the bind-space box of each influencing joint.
#[derive(Debug)]
pub(crate) struct SkinBindBounds {
    /// `(joint, bind-space box of the vertices it weights)`, ascending joint order.
    joints: Vec<(u32, Box3)>,
    /// Vertices the shaders leave unskinned: drawn at their bind position.
    unskinned: Option<Box3>,
    /// Largest joint index a weighted lane reads.
    max_joint: u32,
    /// Largest `|Σw − 1|` of a skinned vertex.
    weight_error: f32,
}

impl SkinBindBounds {
    /// The per-joint boxes of `vertices`, or `None` when no conservative bound exists (negative
    /// or non-finite weights, a weight sum off by more than [`WEIGHT_SUM_TOLERANCE`], a weighted
    /// joint index of [`MAX_JOINTS_PER_SKIN`] or more, a non-finite position).
    pub(crate) fn from_vertices(vertices: &[LabVertex]) -> Option<Self> {
        let mut boxes: Vec<Option<Box3>> = Vec::new();
        let mut unskinned: Option<Box3> = None;
        let mut max_joint = 0u32;
        let mut weight_error = 0.0f32;
        for vertex in vertices {
            let p = vertex.position;
            if !p.iter().all(|v| v.is_finite()) {
                return None;
            }
            let w = vertex.weights;
            if !w.iter().all(|v| v.is_finite() && *v >= 0.0) {
                return None;
            }
            let sum = w[0] + w[1] + w[2] + w[3];
            if sum < UNSKINNED_WEIGHT_SUM {
                match unskinned.as_mut() {
                    Some(b) => b.grow(p),
                    None => unskinned = Some(Box3::point(p)),
                }
                continue;
            }
            let error = (sum - 1.0).abs();
            if error > WEIGHT_SUM_TOLERANCE {
                return None;
            }
            weight_error = weight_error.max(error);
            for (&weight, &joint) in w.iter().zip(&vertex.joints) {
                if weight <= 0.0 {
                    continue;
                }
                if joint as usize >= MAX_JOINTS_PER_SKIN {
                    return None;
                }
                max_joint = max_joint.max(joint);
                let slot = joint as usize;
                if boxes.len() <= slot {
                    boxes.resize(slot + 1, None);
                }
                match boxes[slot].as_mut() {
                    Some(b) => b.grow(p),
                    None => boxes[slot] = Some(Box3::point(p)),
                }
            }
        }
        let joints: Vec<(u32, Box3)> = boxes
            .into_iter()
            .enumerate()
            .filter_map(|(joint, b)| b.map(|b| (joint as u32, b)))
            .collect();
        if joints.is_empty() && unskinned.is_none() {
            return None;
        }
        Some(Self {
            joints,
            unskinned,
            max_joint,
            weight_error,
        })
    }

    /// The object-space box (before the model matrix) holding every vertex skinned by `palette`,
    /// each bind position first moved up to `pad` metres along any axis (morph displacement).
    /// `None` when the palette is shorter than the joints the vertices read, or not finite.
    pub(crate) fn object_box(&self, palette: &[JointMatrix], pad: f32) -> Option<Box3> {
        let pad = pad.max(0.0);
        if !self.joints.is_empty() && palette.len() <= self.max_joint as usize {
            return None;
        }
        let mut out: Option<Box3> = self.unskinned.map(|b| b.padded(pad));
        // The farthest a joint-box point lies from the origin: a weight sum `s` scales the
        // convex combination by `s`, moving it at most `|s − 1|` of that.
        let mut reach = 0.0f32;
        for (joint, bind) in &self.joints {
            let moved = transform_box(&palette[*joint as usize].m, &bind.padded(pad));
            if !moved.is_finite() {
                return None;
            }
            let (c, h) = moved.center_half();
            let length = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            reach = reach.max(length(c) + length(h));
            match out.as_mut() {
                Some(b) => b.union(&moved),
                None => out = Some(moved),
            }
        }
        out.map(|b| b.padded(self.weight_error * reach))
    }
}

/// The world-space bounding sphere `(center, radius)` of `object` under the column-major
/// `model`, grown by `inflate` metres.
pub(crate) fn world_sphere(model: &[[f32; 4]; 4], object: &Box3, inflate: f32) -> ([f32; 3], f32) {
    // Rows of the model's upper 3×4.
    let rows = [
        [model[0][0], model[1][0], model[2][0], model[3][0]],
        [model[0][1], model[1][1], model[2][1], model[3][1]],
        [model[0][2], model[1][2], model[2][2], model[3][2]],
    ];
    let (center, half) = transform_box(&rows, object).center_half();
    let radius = (half[0] * half[0] + half[1] * half[1] + half[2] * half[2]).sqrt();
    (center, radius + inflate.max(0.0))
}

/// Per-renderer state of the skinned bounds: the per-mesh joint boxes and the last frame's
/// counts ([`crate::DrawIntegrity`]).
pub(super) struct SkinBoundsCache {
    /// Skinned draws are culled by their palette bound ([`FrameRenderer::set_skinned_bounds`]).
    pub(super) enabled: bool,
    /// Bounded vertex-shader-skinned draws join the GPU-driven colour batches
    /// ([`FrameRenderer::set_skinned_instancing`], ADR-1368).
    pub(super) instancing: bool,
    /// Per mesh key: the vertex count it was computed on and the bounds (`None`: none exists).
    meshes: crate::key_hash::KeyMap<u64, (usize, Option<Arc<SkinBindBounds>>)>,
    /// Skinned draws of the current frame with a bound.
    pub(super) bounded: usize,
    /// Skinned draws of the current frame without one (always drawn).
    pub(super) unbounded: usize,
    /// Skinned draws of the current frame outside the view by their bound.
    pub(super) culled: usize,
    /// Per mesh key: the vertex count it was computed on and the largest joint a weighted lane
    /// of a skinned vertex reads (`None`: no vertex reads one).
    reach: crate::key_hash::KeyMap<u64, (usize, Option<u32>)>,
    /// Skinned draws of the current frame refused for reading a joint past their palette or past
    /// [`MAX_JOINTS_PER_SKIN`].
    pub(super) refused: usize,
}

impl Default for SkinBoundsCache {
    fn default() -> Self {
        Self {
            enabled: true,
            instancing: true,
            meshes: crate::key_hash::KeyMap::default(),
            bounded: 0,
            unbounded: 0,
            culled: 0,
            reach: crate::key_hash::KeyMap::default(),
            refused: 0,
        }
    }
}

impl SkinBoundsCache {
    /// Starts a frame's counts.
    pub(super) fn begin_frame(&mut self) {
        self.bounded = 0;
        self.unbounded = 0;
        self.culled = 0;
        self.refused = 0;
    }

    /// Forgets the bounds of meshes no longer resident (the sweep, released keys).
    pub(super) fn retain_resident<V>(&mut self, meshes: &crate::key_hash::KeyMap<u64, V>) {
        self.meshes
            .retain(|key, _| meshes.contains_key(key) || meshes.contains_key(&(key ^ LOD_DITHER_MESH_TAG)));
        self.reach
            .retain(|key, _| meshes.contains_key(key) || meshes.contains_key(&(key ^ LOD_DITHER_MESH_TAG)));
    }

    /// Forgets `keys` (released meshes).
    pub(super) fn forget(&mut self, keys: &[u64]) {
        for key in keys {
            self.meshes.remove(key);
            self.reach.remove(key);
        }
    }
}

impl FrameRenderer {
    /// Turns the joint-palette bounds of skinned draws on or off (on by default). On, a skinned
    /// mesh whose conservative bound (ADR-248: each joint's bind-space box of the vertices it
    /// weights, moved by that joint's matrix) is outside the view frustum is culled from the
    /// color passes like a rigid mesh; it still casts shadows. Off, every skinned mesh draws
    /// whatever the view, as before batch v020. Meshes whose weights cannot be bounded (negative
    /// weights, a weight sum off by more than 1 %, joint indices past the palette) always draw.
    pub fn set_skinned_bounds(&mut self, enabled: bool) {
        self.skin_bounds.enabled = enabled;
    }

    /// Whether skinned draws are culled by their joint-palette bound
    /// ([`Self::set_skinned_bounds`]).
    #[must_use]
    pub fn skinned_bounds(&self) -> bool {
        self.skin_bounds.enabled
    }

    /// Counts a skinned draw of the colour pass as bounded (`sphere` is `Some`) or not, and
    /// passes the sphere on.
    pub(super) fn count_skinned_bound(&mut self, sphere: Option<([f32; 3], f32)>) -> Option<([f32; 3], f32)> {
        if sphere.is_some() {
            self.skin_bounds.bounded += 1;
        } else {
            self.skin_bounds.unbounded += 1;
        }
        sphere
    }

    /// Whether skinned `mesh` (`key` is its submitted mesh key) weights a joint its draw cannot
    /// read: at or past its palette's length, or at or past [`MAX_JOINTS_PER_SKIN`]. Such a draw
    /// would skin from another draw's joints, so the frame refuses it and counts it here. The
    /// walk over the vertices runs once per mesh key and vertex count (every frame for key 0).
    pub(super) fn refuse_skin_past_its_palette(&mut self, key: u64, mesh: &FrameMesh<'_>) -> bool {
        if mesh.joints.is_empty() {
            return false;
        }
        let count = mesh.vertices.len();
        // A skinned counted upload has no CPU vertices: its reach was measured at the upload
        // (ADR-8610).
        let counted = if count == 0 {
            self.asset_mesh_skins.get(&key)
        } else {
            None
        };
        let reach = if let Some(skin) = counted {
            skin.reach
        } else {
            match self.skin_bounds.reach.get(&key) {
                Some(&(checked, reach)) if key != 0 && checked == count => reach,
                _ => {
                    let _span = tracing::info_span!("skin_joint_reach", vertices = count).entered();
                    let reach = weighted_joint_reach(mesh.vertices);
                    if key != 0 {
                        self.skin_bounds.reach.insert(key, (count, reach));
                    }
                    reach
                }
            }
        };
        let palette = mesh.joints.len().min(MAX_JOINTS_PER_SKIN);
        let refused = reach.is_some_and(|joint| joint as usize >= palette);
        if refused {
            self.skin_bounds.refused += 1;
            tracing::debug!(
                key,
                joint = reach,
                palette = mesh.joints.len(),
                "skinned draw refused: a vertex weights a joint past its palette or the joint cap"
            );
        }
        refused
    }

    /// Whether `object`'s palette `joints` (up to [`MAX_JOINTS_PER_SKIN`]) differs, bit for bit,
    /// from the palette it drew with last frame, or it drew with none (first frame, a changed
    /// joint count). A skinned caster whose palette did not change is not posing: its cached
    /// shadow pages already hold this pose, so the dynamic-caster class leaves it alone
    /// (ADR-5418).
    pub(super) fn palette_changed(&self, object: u64, joints: &[JointMatrix]) -> bool {
        let count = joints.len().min(MAX_JOINTS_PER_SKIN);
        let Some(&(start, n)) = self.prev_palettes.get(&object) else {
            return true;
        };
        if n as usize != count {
            return true;
        }
        let start = start as usize;
        self.prev_joints.get(start..start + count).is_none_or(|prev| {
            bytemuck::cast_slice::<_, u8>(prev) != bytemuck::cast_slice::<_, u8>(&joints[..count])
        })
    }

    /// The world bounding sphere of skinned `mesh` this frame, from its object box
    /// ([`Self::skinned_object_box`]), grown by `inflate` metres. Not counted.
    pub(super) fn skinned_sphere_uncounted(
        &mut self,
        key: u64,
        mesh: &FrameMesh<'_>,
        inflate: f32,
    ) -> Option<([f32; 3], f32)> {
        let object = self.skinned_object_box(key, mesh, inflate)?;
        Some(world_sphere(&mesh.model, &object, inflate))
    }

    /// The object-space box (before the model matrix) of skinned `mesh` this frame (`key` is its
    /// submitted mesh key), every bind position padded by `inflate` metres, or `None` when the
    /// draw has no safe bound (bounds off, ephemeral key 0, unboundable weights).
    pub(super) fn skinned_object_box(
        &mut self,
        key: u64,
        mesh: &FrameMesh<'_>,
        inflate: f32,
    ) -> Option<Box3> {
        if !self.skin_bounds.enabled || key == 0 || mesh.joints.is_empty() {
            return None;
        }
        let count = mesh.vertices.len();
        // A skinned counted upload: the joint boxes measured at the upload (ADR-8610).
        if count == 0
            && let Some(skin) = self.asset_mesh_skins.get(&key)
        {
            let palette = &mesh.joints[..mesh.joints.len().min(MAX_JOINTS_PER_SKIN)];
            return skin.bind.as_ref()?.object_box(palette, inflate);
        }
        let bind = match self.skin_bounds.meshes.get(&key) {
            Some((checked, bind)) if *checked == count => bind.clone(),
            _ => {
                let _span = tracing::info_span!("skin_bind_bounds", vertices = count).entered();
                let bind = SkinBindBounds::from_vertices(mesh.vertices).map(Arc::new);
                self.skin_bounds.meshes.insert(key, (count, bind.clone()));
                bind
            }
        }?;
        let palette = &mesh.joints[..mesh.joints.len().min(MAX_JOINTS_PER_SKIN)];
        bind.object_box(palette, inflate)
    }
}

/// The model-space bounding sphere of `object`: its centre and the length of its half extent,
/// the shape of a rigid mesh's bound, so the shadow caches page a skinned caster like a rigid one.
pub(super) fn object_sphere(object: &Box3) -> super::MeshBounds {
    let (center, half) = object.center_half();
    super::MeshBounds {
        center,
        radius: (half[0] * half[0] + half[1] * half[1] + half[2] * half[2]).sqrt(),
    }
}

/// The largest joint index a weighted lane (weight not zero) of a vertex the shaders skin
/// (weight sum not below [`UNSKINNED_WEIGHT_SUM`]) reads, or `None` when no vertex reads one.
/// A zero-weight lane adds nothing whatever joint it names.
pub(super) fn weighted_joint_reach(vertices: &[LabVertex]) -> Option<u32> {
    let mut reach: Option<u32> = None;
    for vertex in vertices {
        let w = vertex.weights;
        if w[0] + w[1] + w[2] + w[3] < UNSKINNED_WEIGHT_SUM {
            continue;
        }
        for (&weight, &joint) in w.iter().zip(&vertex.joints) {
            if weight != 0.0 {
                reach = Some(reach.map_or(joint, |r| r.max(joint)));
            }
        }
    }
    reach
}

#[cfg(test)]
mod tests {
    use super::{Box3, SkinBindBounds, object_sphere, weighted_joint_reach, world_sphere};
    use crate::lab_frame::LabVertex;
    use crate::skin::{JointMatrix, skin_position};
    use regia_core::glam::{Mat4, Quat, Vec3};

    /// A small deterministic generator (no test dependency).
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
        }

        fn range(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (hi - lo) * self.next()
        }
    }

    fn vertex(p: [f32; 3], joints: [u32; 4], weights: [f32; 4]) -> LabVertex {
        let mut v = LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.0, 0.0]);
        v.joints = joints;
        v.weights = weights;
        v
    }

    fn inside(b: &Box3, p: [f32; 3], slack: f32) -> bool {
        (0..3).all(|axis| p[axis] >= b.min[axis] - slack && p[axis] <= b.max[axis] + slack)
    }

    /// Every skinned vertex lies in the object box, for random meshes (up to 4 influences with
    /// normalized weights, some unskinned vertices) and random palettes (rotation, scale,
    /// translations far outside the bind box).
    #[test]
    fn every_skinned_vertex_lies_in_the_palette_box() {
        let mut rng = Lcg(0x5eed);
        for case in 0..200 {
            let joint_count = 1 + (case % 12) as u32;
            let mut vertices = Vec::new();
            for i in 0..64 {
                let p = [rng.range(-1.0, 1.0), rng.range(0.0, 2.0), rng.range(-0.5, 0.5)];
                if i % 17 == 0 {
                    vertices.push(vertex(p, [0; 4], [0.0; 4]));
                    continue;
                }
                let mut w = [rng.next(), rng.next(), rng.next(), rng.next()];
                if i % 3 == 0 {
                    w[2] = 0.0;
                    w[3] = 0.0;
                }
                let sum: f32 = w.iter().sum();
                let w = w.map(|x| x / sum);
                let j = [0, 1, 2, 3].map(|_| (rng.next() * joint_count as f32) as u32 % joint_count);
                vertices.push(vertex(p, j, w));
            }
            let bind = SkinBindBounds::from_vertices(&vertices).expect("boundable");
            let palette: Vec<JointMatrix> = (0..joint_count)
                .map(|_| {
                    let rotation = Quat::from_euler(
                        regia_core::glam::EulerRot::XYZ,
                        rng.range(-3.0, 3.0),
                        rng.range(-3.0, 3.0),
                        rng.range(-3.0, 3.0),
                    );
                    let scale = Vec3::new(rng.range(0.5, 2.0), rng.range(0.5, 2.0), rng.range(0.5, 2.0));
                    let translation = Vec3::new(
                        rng.range(-40.0, 40.0),
                        rng.range(-5.0, 5.0),
                        rng.range(-40.0, 40.0),
                    );
                    JointMatrix::from_mat4(Mat4::from_scale_rotation_translation(
                        scale,
                        rotation,
                        translation,
                    ))
                })
                .collect();
            let object = bind.object_box(&palette, 0.0).expect("palette long enough");
            for v in &vertices {
                let pick = |lane: usize| palette[v.joints[lane] as usize];
                let p = skin_position(v.position, &[pick(0), pick(1), pick(2), pick(3)], v.weights);
                assert!(inside(&object, p, 1e-3), "case {case}: {p:?} outside {object:?}");
            }
        }
    }

    /// One joint translated 50 m: the box follows it (not the bind pose), and stays tight.
    #[test]
    fn a_translated_joint_moves_the_box() {
        let vertices = vec![
            vertex([-0.5, 0.0, 0.0], [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
            vertex([0.5, 1.0, 0.0], [1, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
        ];
        let bind = SkinBindBounds::from_vertices(&vertices).expect("bound");
        let palette = [JointMatrix::identity(), JointMatrix::translation(50.0, 0.0, 0.0)];
        let b = bind.object_box(&palette, 0.0).expect("box");
        assert_eq!(b.min, [-0.5, 0.0, 0.0]);
        assert_eq!(b.max, [50.5, 1.0, 0.0]);
        // A morph pad grows the bind boxes before the joint moves them.
        let padded = bind.object_box(&palette, 0.25).expect("box");
        assert_eq!(padded.min, [-0.75, -0.25, -0.25]);
        assert_eq!(padded.max, [50.75, 1.25, 0.25]);
    }

    /// A weight sum off by up to 1 % stays bounded (padded); negative weights, a larger error,
    /// non-finite values and joint indices past the palette have no bound.
    #[test]
    fn unboundable_skins_have_no_bound() {
        let ok = vertex([1.0, 0.0, 0.0], [0, 1, 0, 0], [0.6, 0.395, 0.0, 0.0]);
        let bind = SkinBindBounds::from_vertices(&[ok]).expect("within tolerance");
        let far = [
            JointMatrix::translation(10.0, 0.0, 0.0),
            JointMatrix::translation(10.0, 0.0, 0.0),
        ];
        let b = bind.object_box(&far, 0.0).expect("box");
        let p = skin_position(ok.position, &[far[0], far[1], far[0], far[0]], ok.weights);
        assert!(inside(&b, p, 0.0), "{p:?} in {b:?}");
        // Palette shorter than the joints the vertices read.
        assert!(bind.object_box(&far[..1], 0.0).is_none());
        let nan = JointMatrix {
            m: [
                [f32::NAN, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
            ],
        };
        assert!(bind.object_box(&[nan, nan], 0.0).is_none());
        for bad in [
            vertex([0.0; 3], [0, 0, 0, 0], [1.2, -0.2, 0.0, 0.0]),
            vertex([0.0; 3], [0, 0, 0, 0], [0.9, 0.0, 0.0, 0.0]),
            vertex([0.0; 3], [300, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
            vertex([f32::INFINITY, 0.0, 0.0], [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
        ] {
            assert!(SkinBindBounds::from_vertices(&[bad]).is_none(), "{bad:?}");
        }
        // A zero weight on an out-of-range lane is never read by the shader's sum.
        let zero_lane = vertex([0.0; 3], [0, 999, 0, 0], [1.0, 0.0, 0.0, 0.0]);
        assert!(SkinBindBounds::from_vertices(&[zero_lane]).is_some());
    }

    /// Unskinned vertices (weight sum below 1e-5) stay at their bind position.
    #[test]
    fn unskinned_vertices_keep_their_bind_box() {
        let vertices = vec![
            vertex([2.0, 0.0, 0.0], [0; 4], [0.0; 4]),
            vertex([0.0, 0.0, 0.0], [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
        ];
        let bind = SkinBindBounds::from_vertices(&vertices).expect("bound");
        let b = bind
            .object_box(&[JointMatrix::translation(0.0, 9.0, 0.0)], 0.0)
            .expect("box");
        assert_eq!(b.min, [0.0, 0.0, 0.0]);
        assert_eq!(b.max, [2.0, 9.0, 0.0]);
    }

    /// The world sphere goes through the model (here a 2× scale and a translation).
    #[test]
    fn world_sphere_applies_the_model() {
        let b = Box3 {
            min: [-1.0, -1.0, -1.0],
            max: [1.0, 1.0, 1.0],
        };
        let model =
            Mat4::from_scale_rotation_translation(Vec3::splat(2.0), Quat::IDENTITY, Vec3::new(5.0, 0.0, 0.0))
                .to_cols_array_2d();
        let (center, radius) = world_sphere(&model, &b, 0.5);
        assert_eq!(center, [5.0, 0.0, 0.0]);
        assert!((radius - (12.0f32.sqrt() + 0.5)).abs() < 1e-5, "{radius}");
    }

    #[test]
    fn the_object_sphere_holds_its_box() {
        let b = Box3 {
            min: [-1.0, 0.0, -2.0],
            max: [1.0, 2.0, 0.0],
        };
        let s = object_sphere(&b);
        assert_eq!(s.center, [0.0, 1.0, -1.0]);
        assert!((s.radius - 3.0f32.sqrt()).abs() < 1e-6, "{}", s.radius);
    }

    /// The joint reach counts weighted lanes of skinned vertices only: a zero-weight lane and an
    /// unskinned vertex (weight sum under 1e-5, drawn at its bind position) read no joint.
    #[test]
    fn the_joint_reach_reads_weighted_lanes_of_skinned_vertices() {
        let p = [0.0; 3];
        assert_eq!(weighted_joint_reach(&[]), None);
        assert_eq!(
            weighted_joint_reach(&[vertex(p, [0, 0, 0, 0], [0.0; 4])]),
            None,
            "an unskinned vertex reads nothing"
        );
        assert_eq!(
            weighted_joint_reach(&[vertex(p, [3, 900, 0, 0], [1.0, 0.0, 0.0, 0.0])]),
            Some(3),
            "a zero-weight lane is ignored"
        );
        assert_eq!(
            weighted_joint_reach(&[
                vertex(p, [3, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
                vertex(p, [1, 300, 0, 0], [0.5, 0.5, 0.0, 0.0]),
            ]),
            Some(300)
        );
        // A negative lane is still read by the shader.
        assert_eq!(
            weighted_joint_reach(&[vertex(p, [2, 7, 0, 0], [1.2, -0.2, 0.0, 0.0])]),
            Some(7)
        );
    }
}
