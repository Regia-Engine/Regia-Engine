// SPDX-License-Identifier: MPL-2.0
//! Drawing generator instance sets and saved `InstancedMeshSet`s (spec 62 § 7.3, spec 63 § 13.3).
//!
//! The host registers meshes by asset path in [`ProcMeshLibrary`] (the game or editor that loaded
//! them). Each frame [`extract_proc_instances`] appends one `ExtractedMesh` per visible instance,
//! all sharing the library's mesh and material `Arc`s, so the frame renderer batches them as
//! instances of one draw (as it does for the runtime scatter). Instances beyond an entry's draw
//! distance are culled; the last `fade_m` meters fade out through the LOD dither.
//! [`crate::exec::InstanceSet::cull_below_px`] is stored on the set and is not read here.
//!
//! Generated meshes (road ribbons, spline-deformed pieces; spec 64 § 6) become one `CpuMesh` per
//! [`GeneratedMesh`] the first frame their output is seen: the `Arc` is the upload key, so a published
//! output uploads once and the GPU range is retired when the output goes. Their materials come from
//! [`ProcMaterialLibrary`], by the same asset paths graphs name.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};

use regia_app::prelude::*;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_render::{CpuMaterial, CpuMesh, ExtractedFrame, ExtractedMesh, LabVertex, ShadowCast};
use regia_scene::GlobalTransform;
use regia_scene::layers::{LayerParked, LayerTable};

use crate::exec::{GeneratedMesh, RunOutput};
use crate::world::{InstancedMeshSet, ProcOutput};

/// Meshes generators may draw, by asset path (filled by the host).
#[derive(Resource, Default, Clone)]
pub struct ProcMeshLibrary(pub BTreeMap<String, (Arc<CpuMesh>, Arc<CpuMaterial>)>);

impl ProcMeshLibrary {
    /// Registers (or replaces) a mesh and its material under an asset path.
    pub fn insert(&mut self, path: impl Into<String>, mesh: Arc<CpuMesh>, material: Arc<CpuMaterial>) {
        self.0.insert(path.into(), (mesh, material));
    }
}

/// Materials by asset path (filled by the host): generated meshes and instance-set material
/// overrides read it.
#[derive(Resource, Default, Clone)]
pub struct ProcMaterialLibrary(pub BTreeMap<String, Arc<CpuMaterial>>);

impl ProcMaterialLibrary {
    /// Registers (or replaces) a material under an asset path.
    pub fn insert(&mut self, path: impl Into<String>, material: Arc<CpuMaterial>) {
        self.0.insert(path.into(), material);
    }
}

/// What the last frame drew (`procgen.list` `draw`).
#[derive(Resource, Default, Clone, Debug, PartialEq, Eq)]
pub struct ProcDrawStats {
    /// Instances drawn.
    pub drawn: usize,
    /// Instances beyond their draw distance.
    pub culled: usize,
    /// Mesh paths no library entry exists for (their instances are not drawn).
    pub missing_meshes: BTreeSet<String>,
    /// Generated meshes drawn.
    pub generated_meshes: usize,
    /// Triangles of the generated meshes drawn.
    pub generated_triangles: usize,
    /// Material paths no library entry exists for (drawn with the default material).
    pub missing_materials: BTreeSet<String>,
    /// Instance lists rebuilt since the process started. An unchanged eye and output do not add one.
    pub rebuilds: u64,
}

/// One instance's draw: column-major model matrix and the fade dither (0 = whole).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InstanceDraw {
    /// Object-to-world matrix.
    pub model: [[f32; 4]; 4],
    /// Fade dither in [0, 1].
    pub dither: f32,
    /// Index of the instance in its set.
    pub index: usize,
}

/// The visible instances of a set seen from `eye`, with `parent` applied (identity for generator
/// output, the entity transform for a saved set).
pub fn instance_draws(
    eye: [f32; 3],
    parent: Mat4,
    draw_distance_m: f32,
    fade_m: f32,
    instances: impl Iterator<Item = (Vec3, Quat, Vec3)>,
) -> (Vec<InstanceDraw>, usize) {
    let eye = Vec3::from(eye);
    let mut out = Vec::new();
    let mut culled = 0;
    for (index, (t, r, s)) in instances.enumerate() {
        let model = parent * Mat4::from_scale_rotation_translation(s, r, t);
        let dist = (model.w_axis.truncate() - eye).length();
        if dist > draw_distance_m {
            culled += 1;
            continue;
        }
        let dither = if fade_m > 0.0 && dist > draw_distance_m - fade_m {
            ((dist - (draw_distance_m - fade_m)) / fade_m).clamp(0.0, 1.0)
        } else {
            0.0
        };
        out.push(InstanceDraw {
            model: model.to_cols_array_2d(),
            dither,
            index,
        });
    }
    (out, culled)
}

/// A generated mesh in the renderer's vertex layout: world-space vertices (drawn with an identity
/// model matrix), tangents from the UVs, white where the mesh has no colors.
pub fn cpu_mesh(m: &GeneratedMesh) -> CpuMesh {
    let mut tangents = vec![[1.0, 0.0, 0.0, 1.0]; m.positions.len()];
    // A degenerate mesh keeps the fallback tangent: nothing it can shade differently.
    let _ =
        regia_render::generate_vertex_tangents(&m.positions, &m.normals, &m.uvs, &m.indices, &mut tangents);
    let vertices = m
        .positions
        .iter()
        .enumerate()
        .map(|(i, position)| LabVertex {
            position: *position,
            normal: m.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]),
            uv: m.uvs.get(i).copied().unwrap_or_default(),
            tangent: tangents[i],
            color: m.colors.get(i).copied().unwrap_or([1.0; 4]),
            joints: [0; 4],
            weights: [1.0, 0.0, 0.0, 0.0],
        })
        .collect();
    CpuMesh {
        vertices,
        indices: m.indices.clone(),
    }
}

/// An output's generated meshes in renderer form, kept while that output is the published one.
pub struct GeneratedDraws {
    source: Arc<RunOutput>,
    meshes: Vec<Arc<CpuMesh>>,
}

fn generated_extracted(
    entity: Entity,
    index: usize,
    mesh: Arc<CpuMesh>,
    material: Arc<CpuMaterial>,
) -> ExtractedMesh {
    ExtractedMesh {
        is_static: false,
        entity,
        mesh,
        material,
        layer: 0,
        model: Mat4::IDENTITY.to_cols_array_2d(),
        joints: Vec::new(),
        decal_layer: 0,
        uv1: None,
        shadow: ShadowCast::All,
        draw_indices: None,
        lod_key: 0,
        // Outside the instance-set key range (set index in bits 40 and up, instance index below).
        object_key_xor: (1u64 << 63) ^ (index as u64) ^ entity.to_bits().rotate_left(29),
        gi_proxy: false,
        lod_dither: 0.0,
    }
}

fn extracted(
    entity: Entity,
    set_index: usize,
    mesh: &(Arc<CpuMesh>, Arc<CpuMaterial>),
    d: &InstanceDraw,
    shadows: bool,
) -> ExtractedMesh {
    ExtractedMesh {
        is_static: false,
        entity,
        mesh: Arc::clone(&mesh.0),
        material: Arc::clone(&mesh.1),
        layer: 0,
        model: d.model,
        joints: Vec::new(),
        decal_layer: 0,
        uv1: None,
        shadow: if shadows { ShadowCast::All } else { ShadowCast::Off },
        draw_indices: None,
        lod_key: 0,
        // One uniform per instance: the entity key alone would be shared by all of them.
        object_key_xor: ((set_index as u64) << 40) ^ (d.index as u64) ^ entity.to_bits().rotate_left(17),
        gi_proxy: false,
        lod_dither: d.dither,
    }
}

/// One output's built instance list, reused while the output allocation, the eye's meter and the
/// mesh and material libraries are unchanged.
struct InstanceListCache {
    entity: Entity,
    /// The [`RunOutput`] the list was built from. A `Weak` keeps the allocation, so a later output
    /// cannot reuse its address and match a stale list.
    output: Weak<RunOutput>,
    /// Eye position floored to whole meters. Culling can stay stale by up to one meter.
    eye_m: [i32; 3],
    /// Indices of the sets hidden by their data layer when the list was built.
    hidden: Vec<usize>,
    meshes: Vec<ExtractedMesh>,
    drawn: usize,
    culled: usize,
    missing_meshes: BTreeSet<String>,
    missing_materials: BTreeSet<String>,
}

/// Eye position in whole meters. `floor` keeps negative coordinates on the correct side of zero.
fn eye_meters(eye: [f32; 3]) -> [i32; 3] {
    [
        eye[0].floor() as i32,
        eye[1].floor() as i32,
        eye[2].floor() as i32,
    ]
}

/// What [`extract_proc_instances`] keeps between frames.
#[derive(Default)]
pub struct ProcDrawCache {
    /// Each output's generated meshes in renderer form.
    draws: BTreeMap<Entity, GeneratedDraws>,
    /// The material of a mesh whose own is missing: one `Arc`, so its render key stays put.
    fallback: Arc<CpuMaterial>,
    /// Instance lists rebuilt since the process started. Not cleared with the per-frame stats.
    rebuilds: u64,
    /// Built instance lists. An unchanged output and eye meter are copied forward.
    instances: Vec<InstanceListCache>,
    /// Whether the mesh and material libraries existed when the lists were built.
    libraries: (bool, bool),
}

/// `Render`: appends generator instance sets and saved instance sets to the frame's meshes.
pub fn extract_proc_instances(
    (library, materials): (Option<Res<ProcMeshLibrary>>, Option<Res<ProcMaterialLibrary>>),
    outputs: Query<(Entity, &ProcOutput, regia_render::DrawVisibility), Without<LayerParked>>,
    saved: Query<
        (
            Entity,
            &InstancedMeshSet,
            Option<&GlobalTransform>,
            regia_render::DrawVisibility,
        ),
        Without<LayerParked>,
    >,
    layers: Option<Res<LayerTable>>,
    frame: Option<ResMut<ExtractedFrame>>,
    mut stats: ResMut<ProcDrawStats>,
    mut cache: Local<ProcDrawCache>,
) {
    let Some(mut frame) = frame else {
        return;
    };
    // A built list holds the meshes and materials the libraries gave it, and the missing paths: a
    // library that changed, appeared or went away invalidates every list.
    let libraries = (library.is_some(), materials.is_some());
    let libraries_changed = library.as_ref().is_some_and(|r| r.is_changed())
        || materials.as_ref().is_some_and(|r| r.is_changed())
        || libraries != cache.libraries;
    if libraries_changed {
        cache.instances.clear();
        cache.libraries = libraries;
    }
    let empty_meshes = ProcMeshLibrary::default();
    let library = library.as_deref().unwrap_or(&empty_meshes);
    let empty_materials = ProcMaterialLibrary::default();
    let materials = materials.as_deref().unwrap_or(&empty_materials);
    let Some(view) = frame.primary_view() else {
        return;
    };
    let _span = tracing::info_span!("procgen.draw").entered();
    let eye = view.camera.eye;
    let mut next = ProcDrawStats::default();
    let mut meshes = std::mem::take(&mut frame.meshes);
    let ProcDrawCache {
        draws: generated,
        fallback,
        rebuilds,
        instances,
        ..
    } = &mut *cache;
    instances.retain(|cached| outputs.get(cached.entity).is_ok());
    // An output without meshes keeps nothing alive from the output before it.
    generated.retain(|e, _| outputs.get(*e).is_ok_and(|(_, o, _)| !o.0.meshes.is_empty()));
    for (entity, out, inherited) in &outputs {
        // A hidden generator draws nothing; its built lists stay cached for when it shows again
        // (the renderer's shared filter, ADR-7200).
        if !regia_render::is_drawn(inherited) {
            continue;
        }
        if !out.0.meshes.is_empty() {
            let draws = generated
                .entry(entity)
                .and_modify(|d| {
                    if !Arc::ptr_eq(&d.source, &out.0) {
                        d.meshes.clear();
                    }
                })
                .or_insert_with(|| GeneratedDraws {
                    source: out.0.clone(),
                    meshes: Vec::new(),
                });
            if draws.meshes.is_empty() {
                draws.source = out.0.clone();
                draws.meshes = out.0.meshes.iter().map(|m| Arc::new(cpu_mesh(m))).collect();
            }
            for (k, (m, cpu)) in out.0.meshes.iter().zip(&draws.meshes).enumerate() {
                let material = match materials.0.get(&m.material) {
                    Some(mat) => mat.clone(),
                    None => {
                        if !m.material.is_empty() {
                            next.missing_materials.insert(m.material.clone());
                        }
                        fallback.clone()
                    }
                };
                next.generated_meshes += 1;
                next.generated_triangles += m.indices.len() / 3;
                meshes.push(generated_extracted(entity, k, cpu.clone(), material));
            }
        }
        if out.0.instance_sets.is_empty() {
            instances.retain(|cached| cached.entity != entity);
            continue;
        }
        let meter = eye_meters(eye);
        // Sets whose `target_layer` is not `Live` are not drawn (spec 63 § 13.1, ADR-7214).
        let hidden: Vec<usize> = out
            .0
            .instance_sets
            .iter()
            .enumerate()
            .filter(|(_, set)| {
                set.layer
                    .as_deref()
                    .is_some_and(|l| crate::world::layer_hidden(layers.as_deref(), l))
            })
            .map(|(k, _)| k)
            .collect();
        if let Some(index) = instances.iter().position(|cached| {
            cached.entity == entity
                && std::ptr::eq(cached.output.as_ptr(), Arc::as_ptr(&out.0))
                && cached.eye_m == meter
                && cached.hidden == hidden
        }) {
            let cached = &instances[index];
            meshes.extend(cached.meshes.iter().cloned());
            next.drawn += cached.drawn;
            next.culled += cached.culled;
            next.missing_meshes.extend(cached.missing_meshes.iter().cloned());
            next.missing_materials
                .extend(cached.missing_materials.iter().cloned());
            continue;
        }
        let mut built = Vec::new();
        let mut drawn = 0usize;
        let mut culled = 0usize;
        let mut missing_meshes = BTreeSet::new();
        let mut missing_materials = BTreeSet::new();
        for (k, set) in out.0.instance_sets.iter().enumerate() {
            if hidden.contains(&k) {
                continue;
            }
            let Some(mesh) = library.0.get(&set.mesh) else {
                missing_meshes.insert(set.mesh.clone());
                continue;
            };
            let overridden =
                set.material
                    .as_ref()
                    .filter(|p| !p.is_empty())
                    .and_then(|p| match materials.0.get(p) {
                        Some(mat) => Some((mesh.0.clone(), mat.clone())),
                        None => {
                            missing_materials.insert(p.clone());
                            None
                        }
                    });
            let mesh = overridden.as_ref().unwrap_or(mesh);
            let (draws, set_culled) = instance_draws(
                eye,
                Mat4::IDENTITY,
                set.draw_distance_m,
                set.fade_m,
                set.instances.iter().map(|i| {
                    (
                        Vec3::from(i.translation),
                        Quat::from_array(i.rotation),
                        Vec3::from(i.scale),
                    )
                }),
            );
            culled += set_culled;
            drawn += draws.len();
            built.extend(
                draws
                    .iter()
                    .map(|d| extracted(entity, k, mesh, d, set.cast_shadows)),
            );
        }
        meshes.extend(built.iter().cloned());
        next.drawn += drawn;
        next.culled += culled;
        next.missing_meshes.extend(missing_meshes.iter().cloned());
        next.missing_materials.extend(missing_materials.iter().cloned());
        instances.retain(|cached| cached.entity != entity);
        instances.push(InstanceListCache {
            entity,
            output: Arc::downgrade(&out.0),
            eye_m: meter,
            hidden,
            meshes: built,
            drawn,
            culled,
            missing_meshes,
            missing_materials,
        });
        *rebuilds += 1;
    }
    for (entity, set, gt, inherited) in &saved {
        if !regia_render::is_drawn(inherited) {
            continue;
        }
        let Some(mesh) = library.0.get(&set.mesh) else {
            next.missing_meshes.insert(set.mesh.clone());
            continue;
        };
        let parent = gt.map_or(Mat4::IDENTITY, |g| Mat4::from(g.0));
        let (draws, culled) = instance_draws(
            eye,
            parent,
            set.draw_distance_m,
            set.fade_m,
            set.instances.iter().map(|i| (i.translation, i.rotation, i.scale)),
        );
        next.culled += culled;
        next.drawn += draws.len();
        meshes.extend(
            draws
                .iter()
                .map(|d| extracted(entity, 0, mesh, d, set.cast_shadows)),
        );
    }
    next.rebuilds = *rebuilds;
    frame.meshes = meshes;
    *stats = next;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Collision, Instance, InstanceSet, RunOutput};
    use regia_ecs::prelude::bevy_ecs::system::RunSystemOnce;
    use regia_render::{ExtractedCamera, ExtractedTarget, ExtractedView, PostSettings};

    #[test]
    fn the_system_appends_shared_mesh_draws_and_reports_missing_meshes() {
        let mut world = World::new();
        let mut frame = ExtractedFrame::default();
        frame.views.push(ExtractedView {
            camera: ExtractedCamera {
                eye: [0.0; 3],
                target: [0.0, 0.0, -1.0],
                fov_y: 1.0,
                near: 0.1,
                far: 1000.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: PostSettings::default(),
        });
        world.insert_resource(frame);
        let mesh = Arc::new(CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let mut lib = ProcMeshLibrary::default();
        lib.insert("rock.glb", mesh.clone(), Arc::new(CpuMaterial::default()));
        world.insert_resource(lib);
        world.init_resource::<ProcDrawStats>();
        let inst = |x: f32| Instance {
            translation: [x, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            color: [1.0; 4],
            custom: [0.0; 4],
        };
        let set = |mesh: &str| InstanceSet {
            mesh: mesh.to_string(),
            material: None,
            cast_shadows: true,
            collision: Collision::None,
            draw_distance_m: 100.0,
            fade_m: 0.0,
            cull_below_px: 2.0,
            instances: vec![inst(5.0), inst(50.0), inst(500.0)],
            layer: None,
        };
        let out = RunOutput {
            instance_sets: vec![set("rock.glb"), set("missing.glb")],
            ..RunOutput::default()
        };
        world.spawn(ProcOutput(Arc::new(out)));
        world
            .run_system_once(extract_proc_instances)
            .expect("system runs");
        let frame = world.resource::<ExtractedFrame>();
        assert_eq!(frame.meshes.len(), 2, "two rocks within 100 m");
        assert!(
            frame.meshes.iter().all(|m| Arc::ptr_eq(&m.mesh, &mesh)),
            "one shared mesh: drawn as instances"
        );
        assert_ne!(frame.meshes[0].object_key_xor, frame.meshes[1].object_key_xor);
        let stats = world.resource::<ProcDrawStats>();
        assert_eq!((stats.drawn, stats.culled), (2, 1));
        assert!(stats.missing_meshes.contains("missing.glb"));
    }

    /// A generated mesh draws once per frame from one uploaded `Arc`; a new output replaces the
    /// `Arc`; its material comes from the library, and an unknown path is reported.
    #[test]
    fn generated_meshes_draw_from_one_arc_until_the_output_changes() {
        let mut world = World::new();
        let mut frame = ExtractedFrame::default();
        frame.views.push(ExtractedView {
            camera: ExtractedCamera {
                eye: [0.0; 3],
                target: [0.0, 0.0, -1.0],
                fov_y: 1.0,
                near: 0.1,
                far: 1000.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: PostSettings::default(),
        });
        world.insert_resource(frame);
        world.init_resource::<ProcDrawStats>();
        let asphalt = Arc::new(CpuMaterial::default());
        let mut materials = ProcMaterialLibrary::default();
        materials.insert("asphalt", asphalt.clone());
        world.insert_resource(materials);
        let mesh = |material: &str| GeneratedMesh {
            material: material.to_string(),
            positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            normals: vec![[0.0, 1.0, 0.0]; 3],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: vec![0, 1, 2],
            colors: vec![[0.5, 0.25, 0.125, 1.0]; 3],
            collision: false,
        };
        let e = world
            .spawn(ProcOutput(Arc::new(RunOutput {
                meshes: vec![mesh("asphalt"), mesh("gone")],
                ..RunOutput::default()
            })))
            .id();
        // A registered system keeps its `Local` upload cache between runs.
        let system = world.register_system(extract_proc_instances);
        world.run_system(system).expect("system runs");
        let first: Vec<Arc<CpuMesh>> = {
            let frame = world.resource::<ExtractedFrame>();
            assert_eq!(frame.meshes.len(), 2);
            assert!(Arc::ptr_eq(&frame.meshes[0].material, &asphalt));
            assert_eq!(frame.meshes[0].model, Mat4::IDENTITY.to_cols_array_2d());
            assert_eq!(frame.meshes[0].mesh.vertices[1].position, [1.0, 0.0, 0.0]);
            assert_eq!(frame.meshes[0].mesh.vertices[1].color, [0.5, 0.25, 0.125, 1.0]);
            frame.meshes.iter().map(|m| m.mesh.clone()).collect()
        };
        {
            let stats = world.resource::<ProcDrawStats>();
            assert_eq!((stats.generated_meshes, stats.generated_triangles), (2, 2));
            assert!(stats.missing_materials.contains("gone"));
        }
        world.resource_mut::<ExtractedFrame>().meshes.clear();
        world.run_system(system).expect("system runs");
        assert!(
            Arc::ptr_eq(&world.resource::<ExtractedFrame>().meshes[0].mesh, &first[0]),
            "the same output draws the same Arc (no re-upload)"
        );
        world.entity_mut(e).insert(ProcOutput(Arc::new(RunOutput {
            meshes: vec![mesh("asphalt")],
            ..RunOutput::default()
        })));
        world.resource_mut::<ExtractedFrame>().meshes.clear();
        world.run_system(system).expect("system runs");
        let frame = world.resource::<ExtractedFrame>();
        assert_eq!(frame.meshes.len(), 1);
        assert!(
            !Arc::ptr_eq(&frame.meshes[0].mesh, &first[0]),
            "a new output uploads a new mesh"
        );
    }

    #[test]
    fn culls_beyond_the_draw_distance_and_fades_the_last_band() {
        let inst = [10.0f32, 95.0, 150.0].map(|x| (Vec3::new(x, 0.0, 0.0), Quat::IDENTITY, Vec3::ONE));
        let (draws, culled) = instance_draws([0.0; 3], Mat4::IDENTITY, 100.0, 10.0, inst.into_iter());
        assert_eq!(culled, 1);
        assert_eq!(draws.len(), 2);
        assert_eq!(draws[0].dither, 0.0);
        assert!(
            (draws[1].dither - 0.5).abs() < 1e-6,
            "95 m is halfway through the 90..100 m fade"
        );
        assert_eq!(draws[1].model[3][0], 95.0, "translation in the fourth column");
        let parent = Mat4::from_translation(Vec3::new(0.0, 5.0, 0.0));
        let (draws, _) = instance_draws([0.0; 3], parent, 100.0, 0.0, inst.into_iter().take(1));
        assert_eq!(draws[0].model[3][1], 5.0, "the saved set's transform applies");
    }

    /// The same output and the same meter of eye rebuild the instance list once.
    #[test]
    fn perf_g_unchanged_eye_reuses_the_instance_list() {
        let mut world = World::new();
        let mut frame = ExtractedFrame::default();
        frame.views.push(ExtractedView {
            camera: ExtractedCamera {
                eye: [0.0; 3],
                target: [0.0, 0.0, -1.0],
                fov_y: 1.0,
                near: 0.1,
                far: 1000.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: PostSettings::default(),
        });
        world.insert_resource(frame);
        let mesh = Arc::new(CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let mut lib = ProcMeshLibrary::default();
        lib.insert("rock.glb", mesh, Arc::new(CpuMaterial::default()));
        world.insert_resource(lib);
        world.init_resource::<ProcDrawStats>();
        let inst = |x: f32| Instance {
            translation: [x, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            color: [1.0; 4],
            custom: [0.0; 4],
        };
        let set = |mesh: &str| InstanceSet {
            mesh: mesh.to_string(),
            material: None,
            cast_shadows: true,
            collision: Collision::None,
            draw_distance_m: 100.0,
            fade_m: 0.0,
            cull_below_px: 2.0,
            instances: vec![inst(5.0), inst(50.0), inst(500.0)],
            layer: None,
        };
        world.spawn(ProcOutput(Arc::new(RunOutput {
            instance_sets: vec![set("rock.glb"), set("missing.glb")],
            ..RunOutput::default()
        })));
        let system = world.register_system(extract_proc_instances);
        let run = |world: &mut World, eye: [f32; 3]| {
            {
                let mut frame = world.resource_mut::<ExtractedFrame>();
                frame.views[0].camera.eye = eye;
                frame.meshes.clear();
            }
            world.run_system(system).expect("system runs");
        };
        run(&mut world, [0.0; 3]);
        {
            let stats = world.resource::<ProcDrawStats>();
            assert_eq!((stats.drawn, stats.culled), (2, 1));
            assert_eq!(stats.rebuilds, 1);
            assert!(stats.missing_meshes.contains("missing.glb"));
        }
        run(&mut world, [0.0; 3]);
        assert_eq!(world.resource::<ProcDrawStats>().rebuilds, 1, "same eye");
        run(&mut world, [0.1, 0.0, 0.0]);
        assert_eq!(
            world.resource::<ProcDrawStats>().rebuilds,
            1,
            "a move inside one meter"
        );
        assert_eq!(world.resource::<ProcDrawStats>().culled, 1);
        run(&mut world, [400.0, 0.0, 0.0]);
        let stats = world.resource::<ProcDrawStats>();
        assert_eq!(stats.rebuilds, 2, "the eye crossed a meter");
        assert_eq!((stats.drawn, stats.culled), (1, 2));
    }

    /// A mesh that arrives in the library after the first frame is drawn on the next frame, even
    /// with a still camera, and stops being reported missing (corr-procgen: the instance-list cache
    /// used to copy the first frame's "missing" list forward forever). A material that arrives
    /// later is picked up the same way, and a library that goes away drops the cached draws.
    #[test]
    fn a_library_change_rebuilds_the_cached_instance_list() {
        let mut world = World::new();
        let mut frame = ExtractedFrame::default();
        frame.views.push(ExtractedView {
            camera: ExtractedCamera {
                eye: [0.0; 3],
                target: [0.0, 0.0, -1.0],
                fov_y: 1.0,
                near: 0.1,
                far: 1000.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: PostSettings::default(),
        });
        world.insert_resource(frame);
        let mesh = || {
            Arc::new(CpuMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            })
        };
        world.insert_resource(ProcMeshLibrary::default());
        world.init_resource::<ProcDrawStats>();
        let inst = |x: f32| Instance {
            translation: [x, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            color: [1.0; 4],
            custom: [0.0; 4],
        };
        world.spawn(ProcOutput(Arc::new(RunOutput {
            instance_sets: vec![InstanceSet {
                mesh: "rock.glb".to_string(),
                material: Some("moss.mat".to_string()),
                cast_shadows: true,
                collision: Collision::None,
                draw_distance_m: 100.0,
                fade_m: 0.0,
                cull_below_px: 2.0,
                instances: vec![inst(5.0), inst(50.0), inst(500.0)],
                layer: None,
            }],
            ..RunOutput::default()
        })));
        let system = world.register_system(extract_proc_instances);
        let run = |world: &mut World| {
            world.resource_mut::<ExtractedFrame>().meshes.clear();
            world.run_system(system).expect("system runs");
        };
        run(&mut world);
        {
            let stats = world.resource::<ProcDrawStats>();
            assert_eq!(stats.drawn, 0);
            assert!(stats.missing_meshes.contains("rock.glb"));
        }
        // The mesh loads; the camera does not move.
        world
            .resource_mut::<ProcMeshLibrary>()
            .insert("rock.glb", mesh(), Arc::new(CpuMaterial::default()));
        run(&mut world);
        {
            let stats = world.resource::<ProcDrawStats>();
            assert_eq!(stats.drawn, 2, "the loaded mesh is drawn");
            assert!(stats.missing_meshes.is_empty(), "{:?}", stats.missing_meshes);
            assert!(stats.missing_materials.contains("moss.mat"));
            assert_eq!(world.resource::<ExtractedFrame>().meshes.len(), 2);
        }
        // The material library appears with the override material.
        let mut mats = ProcMaterialLibrary::default();
        mats.0
            .insert("moss.mat".to_string(), Arc::new(CpuMaterial::default()));
        world.insert_resource(mats);
        run(&mut world);
        assert!(
            world.resource::<ProcDrawStats>().missing_materials.is_empty(),
            "the loaded material is used"
        );
        // An unchanged frame still reuses the list.
        let rebuilds = world.resource::<ProcDrawStats>().rebuilds;
        run(&mut world);
        assert_eq!(
            world.resource::<ProcDrawStats>().rebuilds,
            rebuilds,
            "nothing changed"
        );
        // The mesh library goes away: nothing is drawn from the stale list.
        world.remove_resource::<ProcMeshLibrary>();
        run(&mut world);
        let stats = world.resource::<ProcDrawStats>();
        assert_eq!(stats.drawn, 0, "no library, no draws");
        assert!(stats.missing_meshes.contains("rock.glb"));
    }
    /// v0.5.1 (deep-zai-procgen-runtime finding 4): an output whose entity is parked by a layer
    /// (`LayerParked`, spec 62 § 18) is not drawn; it is drawn again once the layer goes live.
    #[test]
    fn a_parked_output_is_not_drawn() {
        let mut world = World::new();
        let mut frame = ExtractedFrame::default();
        frame.views.push(ExtractedView {
            camera: ExtractedCamera {
                eye: [0.0; 3],
                target: [0.0, 0.0, -1.0],
                fov_y: 1.0,
                near: 0.1,
                far: 1000.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: PostSettings::default(),
        });
        world.insert_resource(frame);
        let mut lib = ProcMeshLibrary::default();
        lib.insert(
            "rock.glb",
            Arc::new(CpuMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            }),
            Arc::new(CpuMaterial::default()),
        );
        world.insert_resource(lib);
        world.init_resource::<ProcDrawStats>();
        let out = world
            .spawn(ProcOutput(Arc::new(RunOutput {
                instance_sets: vec![InstanceSet {
                    mesh: "rock.glb".to_string(),
                    material: None,
                    cast_shadows: true,
                    collision: Collision::None,
                    draw_distance_m: 100.0,
                    fade_m: 0.0,
                    cull_below_px: 0.0,
                    layer: None,
                    instances: vec![Instance {
                        translation: [5.0, 0.0, 0.0],
                        rotation: [0.0, 0.0, 0.0, 1.0],
                        scale: [1.0; 3],
                        color: [1.0; 4],
                        custom: [0.0; 4],
                    }],
                }],
                ..RunOutput::default()
            })))
            .id();
        let system = world.register_system(extract_proc_instances);
        let run = |world: &mut World| {
            world.resource_mut::<ExtractedFrame>().meshes.clear();
            world.run_system(system).expect("system runs");
            world.resource::<ProcDrawStats>().drawn
        };
        assert_eq!(run(&mut world), 1, "live: drawn");
        world.entity_mut(out).insert(regia_scene::layers::LayerParked {
            restore: regia_scene::components::Visibility::default(),
            inherited: false,
        });
        assert_eq!(run(&mut world), 0, "parked: not drawn");
        assert!(world.resource::<ExtractedFrame>().meshes.is_empty());
        world.entity_mut(out).remove::<regia_scene::layers::LayerParked>();
        assert_eq!(run(&mut world), 1, "live again: drawn");
    }
}
