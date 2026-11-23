// SPDX-License-Identifier: MPL-2.0
//! Skinning options: compute skinning, eight influences, dual quaternions (Tincal B2).
//!
//! By default a skinned mesh is skinned in each pass's vertex shader with four influences and
//! linear blending (`specs/13` § 8): the shadow cascades, the depth prepass, the velocity pass
//! and the colour pass each skin it again. [`SkinningOptions`] on an entity (or
//! [`SkinningConfig::compute`] for every skinned mesh) moves it to the **compute path**: a
//! compute pass skins the instance once per frame into a vertex buffer of its own, which every
//! pass then draws like a rigid mesh, and keeps last frame's result for motion vectors. The
//! compute path also reads up to **eight influences** per vertex ([`SkinInfluences8`]) and can
//! blend with **dual quaternions** ([`crate::SkinningMethod::DualQuaternion`]), which keeps a
//! twisting forearm or shoulder from collapsing.
//!
//! The compute path is opt-in: an app without options or the config draws exactly as before.
//! A device without compute shaders (or fewer than four storage buffers per stage) keeps the
//! vertex-shader path for every mesh, dropping the eight influences (the vertex keeps its own
//! four) and the dual quaternions; [`crate::FrameRenderer::skinning_stats`] says why.
//!
//! Options inherit down the hierarchy (`ChildOf`) like [`crate::RenderOverride`]: options on a
//! character's root cover every skinned part without options of its own. The eight-influence
//! table belongs to one mesh, so it is read from the mesh entity's own options only.
//!
//! Extraction: [`extract_skinning`] (a `Render` system after [`crate::extract_frame`]) fills
//! [`ExtractedSkinning`] with the object keys of the frame's extracted meshes. A host that
//! renders an [`crate::ExtractedFrame`] itself calls
//! [`crate::FrameRenderer::set_compute_skinning`] and
//! [`crate::FrameRenderer::set_skinning_options`] with it every frame. The compute path is
//! Regia Engine-original; the dual quaternion blend follows Kavan et al. 2008 (`docs/provenance.md`).

use std::sync::Arc;

use regia_app::prelude::*;

use crate::runtime::{ExtractedFrame, ExtractedMesh};
use crate::skin::SkinningMethod;

/// Eight joint influences of one vertex (64 bytes, the compute kernel's layout). Weights are
/// used as given (like the four of a `LabVertex`): they should sum to 1.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SkinInfluences8 {
    /// Joint indices into the instance's palette.
    pub joints: [u32; 8],
    /// Weights, one per joint.
    pub weights: [f32; 8],
}

impl SkinInfluences8 {
    /// The eight influences of every vertex of a cooked skin (both sets, in stored order,
    /// weights `unorm16 / sum` so each vertex sums to 1). A four-influence skin gives its four
    /// with four empty slots. `None` without a skin.
    #[must_use]
    pub fn from_cpu_skin(skin: &regia_asset::CpuSkin) -> Option<Arc<[Self]>> {
        if skin.joints.is_empty() {
            return None;
        }
        let table = (0..skin.joints.len())
            .map(|v| {
                let mut out = Self::default();
                let sets = [
                    (skin.joints.get(v), skin.weights.get(v)),
                    (skin.joints1.get(v), skin.weights1.get(v)),
                ];
                let mut slot = 0;
                for (joints, weights) in sets {
                    if let (Some(joints), Some(weights)) = (joints, weights) {
                        for k in 0..4 {
                            out.joints[slot] = u32::from(joints[k]);
                            out.weights[slot] = f32::from(weights[k]);
                            slot += 1;
                        }
                    }
                }
                let sum: f32 = out.weights.iter().sum();
                if sum > 0.0 {
                    for w in &mut out.weights {
                        *w /= sum;
                    }
                }
                out
            })
            .collect();
        Some(table)
    }
}

/// How one skinned entity is skinned (see the module docs). The default changes nothing.
#[derive(Component, Debug, Clone, Default)]
pub struct SkinningOptions {
    /// Linear (default) or dual quaternion blending. Dual quaternions need the compute path,
    /// so they put the mesh on it.
    pub method: SkinningMethod,
    /// Skin on the compute path even with linear blending and four influences (one skinning
    /// per frame shared by every pass, instead of one per pass).
    pub compute: bool,
    /// Eight influences per vertex, one entry per mesh vertex (a table of another length is
    /// ignored). Puts the mesh on the compute path. Read from the mesh entity's own options
    /// only, never inherited.
    pub influences8: Option<Arc<[SkinInfluences8]>>,
}

impl SkinningOptions {
    /// Dual quaternion skinning (on the compute path).
    #[must_use]
    pub fn dual_quaternion() -> Self {
        Self {
            method: SkinningMethod::DualQuaternion,
            ..Self::default()
        }
    }

    /// The options a cooked skin asks for (ADR-1622): eight influences on the compute path when
    /// the mesh opted in ([`regia_asset::CpuSkin::eight_influences`], cooked mesh
    /// `skin_influences` 8) and has a second influence set; `None` otherwise, so a default
    /// mesh gets no options and draws exactly as before (the vertex-shader path's four
    /// strongest, [`crate::CpuMesh::from_cooked`]). Insert the result on the mesh entity
    /// itself: the table is never inherited.
    ///
    /// ```
    /// use regia_render::SkinningOptions;
    ///
    /// let mut skin = regia_asset::CpuSkin {
    ///     joint_names: (0..8).map(|i| format!("j{i}")).collect(),
    ///     joints: vec![[0, 1, 2, 3]],
    ///     weights: vec![[20000, 10000, 5000, 535]],
    ///     joints1: vec![[4, 5, 6, 7]],
    ///     weights1: vec![[10000, 10000, 5000, 5000]],
    ///     ..Default::default()
    /// };
    /// assert!(SkinningOptions::for_cooked_skin(&skin).is_none()); // the default: four
    /// skin.eight_influences = true;
    /// let options = SkinningOptions::for_cooked_skin(&skin).expect("opted in");
    /// assert!(options.wants_compute());
    /// assert_eq!(options.influences8.as_ref().map(|t| t.len()), Some(1));
    /// ```
    #[must_use]
    pub fn for_cooked_skin(skin: &regia_asset::CpuSkin) -> Option<Self> {
        if !skin.eight_influences || skin.joints1.is_empty() {
            return None;
        }
        Some(Self {
            influences8: Some(SkinInfluences8::from_cpu_skin(skin)?),
            ..Self::default()
        })
    }

    /// Linear skinning on the compute path.
    #[must_use]
    pub fn compute() -> Self {
        Self {
            compute: true,
            ..Self::default()
        }
    }

    /// Whether these options put a skinned mesh on the compute path.
    #[must_use]
    pub fn wants_compute(&self) -> bool {
        self.compute || self.method != SkinningMethod::Linear || self.influences8.is_some()
    }

    /// The options a child inherits: everything but the eight-influence table.
    #[must_use]
    pub fn inherited(&self) -> Self {
        Self {
            method: self.method,
            compute: self.compute,
            influences8: None,
        }
    }
}

/// App-wide skinning switch (a resource; absent = off).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkinningConfig {
    /// Every skinned mesh on the compute path ([`crate::FrameRenderer::set_compute_skinning`]).
    pub compute: bool,
}

/// This frame's skinning setup, filled by [`extract_skinning`].
#[derive(Resource, Debug, Clone, Default)]
pub struct ExtractedSkinning {
    /// [`SkinningConfig::compute`].
    pub compute_all: bool,
    /// `(object key, options)` of every extracted mesh that has options (its own or inherited).
    pub keys: Vec<(u64, SkinningOptions)>,
}

/// The object key [`crate::ExtractedFrame::frame_scene`] gives an extracted mesh.
fn object_key(mesh: &ExtractedMesh) -> u64 {
    mesh.entity.to_bits() ^ mesh.object_key_xor
}

/// The options `entity` is skinned with: its own, else the nearest ancestor's without the
/// eight-influence table (`ChildOf`, bounded walk).
fn resolve_with(
    entity: Entity,
    own: impl Fn(Entity) -> Option<SkinningOptions>,
    parent: impl Fn(Entity) -> Option<Entity>,
) -> Option<SkinningOptions> {
    if let Some(found) = own(entity) {
        return Some(found);
    }
    let mut current = entity;
    for _ in 0..256 {
        current = parent(current)?;
        if let Some(found) = own(current) {
            return Some(found.inherited());
        }
    }
    None
}

/// The options `entity` in `world` is skinned with (its own, else inherited).
#[must_use]
pub fn resolve_skinning_options(world: &World, entity: Entity) -> Option<SkinningOptions> {
    resolve_with(
        entity,
        |e| world.get::<SkinningOptions>(e).cloned(),
        |e| world.get::<ChildOf>(e).map(ChildOf::parent),
    )
}

impl ExtractedSkinning {
    /// The options of the skinned `meshes`, each resolved with `resolve`.
    pub fn collect(
        compute_all: bool,
        meshes: &[ExtractedMesh],
        resolve: impl Fn(Entity) -> Option<SkinningOptions>,
    ) -> Self {
        let keys = meshes
            .iter()
            .filter(|mesh| !mesh.joints.is_empty())
            .filter_map(|mesh| resolve(mesh.entity).map(|o| (object_key(mesh), o)))
            .collect();
        Self { compute_all, keys }
    }

    /// The skinning setup of `meshes` in `world` (a host that extracts by hand).
    #[must_use]
    pub fn from_world(world: &World, meshes: &[ExtractedMesh]) -> Self {
        let compute_all = world.get_resource::<SkinningConfig>().is_some_and(|c| c.compute);
        if world.component_id::<SkinningOptions>().is_none() {
            return Self {
                compute_all,
                keys: Vec::new(),
            };
        }
        Self::collect(compute_all, meshes, |entity| {
            resolve_skinning_options(world, entity)
        })
    }
}

/// `Render`: fills [`ExtractedSkinning`] for the frame [`crate::extract_frame`] just extracted.
pub fn extract_skinning(
    extracted: Res<ExtractedFrame>,
    config: Option<Res<SkinningConfig>>,
    options: Query<&SkinningOptions>,
    parents: Query<&ChildOf>,
    mut out: ResMut<ExtractedSkinning>,
) {
    let compute_all = config.is_some_and(|c| c.compute);
    if options.is_empty() {
        if !out.keys.is_empty() || out.compute_all != compute_all {
            out.keys.clear();
            out.compute_all = compute_all;
        }
        return;
    }
    let _span = tracing::info_span!("extract_skinning", meshes = extracted.meshes.len()).entered();
    *out = ExtractedSkinning::collect(compute_all, &extracted.meshes, |entity| {
        resolve_with(
            entity,
            |e| options.get(e).ok().cloned(),
            |e| parents.get(e).ok().map(ChildOf::parent),
        )
    });
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "skin_compute.wgsl",
        "Influences8",
        SkinInfluences8 { joints, weights }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_change_nothing() {
        assert!(!SkinningOptions::default().wants_compute());
        assert!(SkinningOptions::compute().wants_compute());
        assert!(SkinningOptions::dual_quaternion().wants_compute());
        let eight = SkinningOptions {
            influences8: Some(Arc::from(vec![SkinInfluences8::default()])),
            ..SkinningOptions::default()
        };
        assert!(eight.wants_compute());
        assert!(eight.inherited().influences8.is_none());
        assert_eq!(size_of::<SkinInfluences8>(), 64);
    }

    #[test]
    fn options_inherit_without_the_influence_table() {
        let mut world = World::new();
        let table: Arc<[SkinInfluences8]> = Arc::from(vec![SkinInfluences8::default(); 3]);
        let root = world
            .spawn(SkinningOptions {
                method: SkinningMethod::DualQuaternion,
                influences8: Some(table),
                ..SkinningOptions::default()
            })
            .id();
        let child = world.spawn(ChildOf(root)).id();
        let own = world.spawn((ChildOf(root), SkinningOptions::compute())).id();
        let loose = world.spawn_empty().id();
        let inherited = resolve_skinning_options(&world, child).expect("inherited");
        assert_eq!(inherited.method, SkinningMethod::DualQuaternion);
        assert!(inherited.influences8.is_none());
        assert!(resolve_skinning_options(&world, root).is_some_and(|o| o.influences8.is_some()));
        let own = resolve_skinning_options(&world, own).expect("own");
        assert_eq!(own.method, SkinningMethod::Linear);
        assert!(own.compute);
        assert!(resolve_skinning_options(&world, loose).is_none());
    }

    /// An eight-influence vertex: four light influences on joints 0..3 and four heavier ones on
    /// 4..7, so the four strongest are not the first set.
    fn eight_influence_skin(eight_influences: bool) -> regia_asset::CpuSkin {
        regia_asset::CpuSkin {
            joint_names: (0..8).map(|i| format!("j{i}")).collect(),
            joints: vec![[0, 1, 2, 3]; 2],
            weights: vec![[3000, 4000, 5000, 6000]; 2],
            joints1: vec![[4, 5, 6, 7]; 2],
            weights1: vec![[20000, 12000, 10000, 5535]; 2],
            eight_influences,
        }
    }

    fn cooked_cpu_mesh(eight_influences: bool) -> regia_asset::CpuMesh {
        regia_asset::CpuMesh {
            positions: vec![[0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
            normals: vec![[0.0, 1.0, 0.0]; 2],
            uvs: vec![[0.0; 2]; 2],
            indices: vec![0, 1, 0],
            skin: Some(eight_influence_skin(eight_influences)),
            ..Default::default()
        }
    }

    #[test]
    fn the_cooked_opt_in_alone_moves_a_skin_to_eight_influences() {
        assert!(SkinningOptions::for_cooked_skin(&eight_influence_skin(false)).is_none());
        let four_only = regia_asset::CpuSkin {
            joints1: Vec::new(),
            weights1: Vec::new(),
            weights: vec![[65535, 0, 0, 0]; 2],
            ..eight_influence_skin(true)
        };
        assert!(
            SkinningOptions::for_cooked_skin(&four_only).is_none(),
            "no second set: nothing to opt into"
        );
        let options = SkinningOptions::for_cooked_skin(&eight_influence_skin(true)).expect("opted in");
        assert!(options.wants_compute());
        assert_eq!(options.method, SkinningMethod::Linear);
        assert!(!options.compute, "the table alone puts it on the compute path");
        let table = options.influences8.clone().expect("table");
        assert_eq!(table.len(), 2);
        assert_eq!(table[0].joints, [0, 1, 2, 3, 4, 5, 6, 7]);
        assert!(options.inherited().influences8.is_none(), "never inherited");
    }

    #[test]
    fn the_vertex_path_mesh_is_byte_identical_with_the_opt_in_on_or_off() {
        let off = crate::CpuMesh::from_cooked(&cooked_cpu_mesh(false)).expect("off");
        let on = crate::CpuMesh::from_cooked(&cooked_cpu_mesh(true)).expect("on");
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&off.vertices),
            bytemuck::cast_slice::<_, u8>(&on.vertices),
            "the vertex-shader path keeps the four strongest either way"
        );
        assert_eq!(off.indices, on.indices);
        assert_eq!(off.vertices[0].joints, [4, 5, 6, 3]);
    }

    #[test]
    fn eight_influences_skin_differently_from_the_four_strongest() {
        use crate::skin::{JointMatrix, skin_vertex_reference};
        use regia_core::glam::{Mat4, Vec3};
        // Joint j translates by (j + 1) along x: every influence moves the vertex differently.
        let joints: Vec<JointMatrix> = (0..8)
            .map(|j| JointMatrix::from_mat4(Mat4::from_translation(Vec3::new(j as f32 + 1.0, 0.0, 0.0))))
            .collect();
        let frame = ([0.0, 1.0, 0.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]);
        let mesh = crate::CpuMesh::from_cooked(&cooked_cpu_mesh(true)).expect("mesh");
        let v = mesh.vertices[0];
        let four: Vec<(u32, f32)> = v.joints.iter().copied().zip(v.weights).collect();
        let table = SkinningOptions::for_cooked_skin(&eight_influence_skin(true))
            .and_then(|o| o.influences8)
            .expect("table");
        let eight: Vec<(u32, f32)> = table[0].joints.iter().copied().zip(table[0].weights).collect();
        let by_four = skin_vertex_reference(frame, &four, &joints, SkinningMethod::Linear);
        let by_eight = skin_vertex_reference(frame, &eight, &joints, SkinningMethod::Linear);
        // Expected x offsets: weighted mean of (j + 1) over the influences used.
        let w8 = [3000.0, 4000.0, 5000.0, 6000.0, 20000.0, 12000.0, 10000.0, 5535.0];
        let want8: f32 = w8
            .iter()
            .enumerate()
            .map(|(j, w)| w * (j as f32 + 1.0))
            .sum::<f32>()
            / 65535.0;
        assert!(
            (by_eight.0[0] - want8).abs() < 1e-4,
            "{} vs {want8}",
            by_eight.0[0]
        );
        assert!(
            (by_eight.0[0] - by_four.0[0]).abs() > 0.1,
            "eight influences move the vertex differently ({} vs {})",
            by_eight.0[0],
            by_four.0[0]
        );
    }

    #[test]
    fn cooked_skins_give_eight_normalised_influences() {
        let skin = regia_asset::CpuSkin {
            joint_names: (0..8).map(|i| format!("j{i}")).collect(),
            joints: vec![[0, 1, 2, 3]],
            weights: vec![[20000, 10000, 5000, 535]],
            joints1: vec![[4, 5, 6, 7]],
            weights1: vec![[10000, 10000, 5000, 5000]],
            eight_influences: false,
        };
        let table = SkinInfluences8::from_cpu_skin(&skin).expect("table");
        assert_eq!(table[0].joints, [0, 1, 2, 3, 4, 5, 6, 7]);
        let sum: f32 = table[0].weights.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "{sum}");
        assert!((table[0].weights[0] - 20000.0 / 65535.0).abs() < 1e-6);
    }

    #[test]
    fn the_extraction_system_keys_skinned_meshes_by_object_key() {
        use regia_ecs::ecs::system::RunSystemOnce;
        let mut world = World::new();
        world.init_resource::<ExtractedFrame>();
        world.init_resource::<ExtractedSkinning>();
        let (vertices, indices) = crate::box_mesh([0.5; 3]);
        let mesh = Arc::new(crate::CpuMesh { vertices, indices });
        let material = Arc::new(crate::CpuMaterial::default());
        let transform = regia_scene::GlobalTransform::IDENTITY;
        let renderer = || crate::MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material));
        let skinned = world
            .spawn((renderer(), transform, SkinningOptions::dual_quaternion()))
            .id();
        world.spawn((renderer(), transform, SkinningOptions::compute()));
        world.run_system_once(crate::extract_frame).expect("extract");
        // Give the first mesh a palette: only skinned meshes are keyed.
        {
            let mut frame = world.resource_mut::<ExtractedFrame>();
            for mesh in &mut frame.meshes {
                if mesh.entity == skinned {
                    mesh.joints = vec![crate::JointMatrix::identity()];
                }
            }
        }
        world.run_system_once(extract_skinning).expect("skinning");
        let out = world.resource::<ExtractedSkinning>().clone();
        assert!(!out.compute_all);
        assert_eq!(out.keys.len(), 1);
        assert_eq!(out.keys[0].0, skinned.to_bits());
        assert_eq!(out.keys[0].1.method, SkinningMethod::DualQuaternion);
        world.insert_resource(SkinningConfig { compute: true });
        world.run_system_once(extract_skinning).expect("skinning");
        assert!(world.resource::<ExtractedSkinning>().compute_all);
    }
}
