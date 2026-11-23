// SPDX-License-Identifier: MPL-2.0
//! Opt-in export of what a frame drew, for offline tools such as the reference path tracer
//! (`docs/specs/27-renderer-excellence.md` § 1.1, "same inputs").
//!
//! [`crate::FrameRenderer::set_scene_export`] makes every [`crate::FrameRenderer::render_into`]
//! record a [`FrameExport`]: the mesh instances it drew (rigid and skinned, with the joint
//! palette of this frame), their materials, the camera, and the light the frame actually used
//! (the sun after the time-of-day override, the sky body's fill, the outdoor parameters). A
//! host that builds its own [`FrameScene`] (not the ECS extraction) is covered the same way.
//!
//! Updates are incremental: mesh and material data are copied once per `mesh_key` /
//! `material_key` (the identities the GPU caches already rely on) and shared by `Arc` between
//! exports, so a moving camera re-records only the instance list. Meshes submitted with the
//! ephemeral key 0 are copied every frame. Of a LOD cross-fade's two copies only the incoming
//! one is exported.
//!
//! Terrain drawn through a [`crate::SharedPassDraw`] is not a mesh list; the draw describes its
//! surface on request ([`crate::SharedPassDraw::reference_heightfield`],
//! [`crate::FrameRenderer::reference_heightfield`]).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::DirectionalLightUniform;
use crate::frame::FrameScene;
use crate::lab_frame::{LabLocalLight, LabMaterial, LabTexture};
use crate::runtime::CpuMesh;
use crate::skin::JointMatrix;

/// One mesh instance the frame drew.
#[derive(Clone, Debug)]
pub struct ExportedInstance {
    /// Key into [`FrameExport::meshes`] (a mesh submitted with the ephemeral key 0 gets
    /// `EPHEMERAL_KEY | its index in the frame`, valid for this export only).
    pub mesh_key: u64,
    /// Key into [`FrameExport::materials`] (ephemeral keys as for [`Self::mesh_key`]).
    pub material_key: u64,
    /// The frame's object identity.
    pub object_key: u64,
    /// Column-major object-to-world matrix.
    pub model: [[f32; 4]; 4],
    /// Joint palette of this frame (empty = rigid). The forward pass skins a vertex as
    /// `Σ weight · joint · position` and then applies [`Self::model`] (`lab_forward.wgsl`).
    pub joints: Vec<JointMatrix>,
}

/// The surface a shared draw renders (the terrain heightfield), for offline tools.
#[derive(Clone, Debug)]
pub struct ReferenceHeightfield {
    /// Samples along X.
    pub width: u32,
    /// Samples along Z.
    pub depth: u32,
    /// World X, Z of sample (0, 0).
    pub origin: [f32; 2],
    /// Sample spacing along X and Z, metres.
    pub spacing: [f32; 2],
    /// Heights in metres, `heights[j * width + i]`.
    pub heights: Vec<f32>,
    /// Albedo over the whole map (UV 0..1 across X and Z, row 0 at the map's −Z edge),
    /// sRGB-encoded bytes.
    pub albedo: Option<LabTexture>,
    /// Perceptual roughness of the surface.
    pub roughness: f32,
}

/// What one [`crate::FrameRenderer::render_into`] drew.
#[derive(Clone, Debug)]
pub struct FrameExport {
    /// The renderer's frame counter when this was recorded.
    pub frame_index: u64,
    /// Camera eye.
    pub eye: [f32; 3],
    /// Camera look-at point.
    pub target: [f32; 3],
    /// Vertical field of view the frame projected with (after `horizontal_plus`), radians.
    pub fov_y: f32,
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// The directional light the frame lit with (after a time-of-day override).
    pub sun: DirectionalLightUniform,
    /// The sky body's shadowless fill the frame added: `[toward the body, colour · lux]`.
    pub fill_light: Option<[[f32; 3]; 2]>,
    /// The frame's directional lights besides the sun (ADR-7211, ADR-7254), in the order it drew
    /// them; empty without extra lights.
    pub extra_directional: Vec<DirectionalLightUniform>,
    /// Flat ambient illuminance when the frame is not lit by sky IBL.
    pub ambient_lux: f32,
    /// The frame was lit by the sky (IBL from the atmosphere).
    pub ibl: bool,
    /// Atmosphere, sky body and moons the frame used.
    pub outdoor: crate::OutdoorParams,
    /// The frame's point and spot lights, with their source radii (`LightSourceSize`,
    /// ADR-1367), so the reference traces the same light sizes.
    pub local_lights: Vec<LabLocalLight>,
    /// Drawn instances, in the frame's order.
    pub instances: Vec<ExportedInstance>,
    /// Mesh data by `mesh_key`.
    pub meshes: HashMap<u64, Arc<CpuMesh>>,
    /// Materials by `material_key`.
    pub materials: HashMap<u64, Arc<LabMaterial>>,
    /// Meshes copied for this export (the rest were reused from earlier frames).
    pub meshes_copied: u32,
}

/// Keys in the renderer's ephemeral range ([`crate::EPHEMERAL_KEY`]).
fn is_ephemeral(key: u64) -> bool {
    key >> 56 == crate::EPHEMERAL_KEY >> 56
}

/// Whether `a` and `b` (the same material key) share every value the renderer rewrites in
/// place under an unchanged key: the scalar factors, the per-material blocks, the sampler
/// choice, and the surface graph's code, parameter block, time flag and vertex-offset
/// bound. Foliage wind sets `uses_time` and `max_vertex_offset` without a new code key.
/// Textures are not compared: a changed image arrives under a changed key
/// (`material_texture_key`, or a new runtime `Arc<CpuMaterial>`).
fn same_parameters(a: &LabMaterial, b: &LabMaterial) -> bool {
    let graph = match (a.surface_graph.as_deref(), b.surface_graph.as_deref()) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            std::ptr::eq(x, y)
                || (x.key == y.key
                    && x.params == y.params
                    && x.uses_time == y.uses_time
                    && x.max_vertex_offset == y.max_vertex_offset)
        }
        _ => false,
    };
    graph
        && a.base_color == b.base_color
        && a.metallic == b.metallic
        && a.roughness == b.roughness
        && a.normal_scale == b.normal_scale
        && a.occlusion_strength == b.occlusion_strength
        && a.emissive == b.emissive
        && a.emissive_intensity == b.emissive_intensity
        && a.alpha_mode == b.alpha_mode
        && a.alpha_cutoff == b.alpha_cutoff
        && a.unlit == b.unlit
        && a.hard_surface == b.hard_surface
        && a.orm_packed == b.orm_packed
        && a.transmission == b.transmission
        && a.ior == b.ior
        && a.transmission_tint == b.transmission_tint
        && a.transmission_thickness == b.transmission_thickness
        && a.transmission_roughness == b.transmission_roughness
        && a.double_sided == b.double_sided
        && a.weather == b.weather
        && a.hair == b.hair
        && a.vertex_ao == b.vertex_ao
        && a.thin_film == b.thin_film
        && a.samplers == b.samplers
        && a.subsurface == b.subsurface
}

/// The persistent side of the export: data already copied, keyed like the GPU caches.
#[derive(Default)]
pub(crate) struct SceneExporter {
    meshes: HashMap<u64, Arc<CpuMesh>>,
    materials: HashMap<u64, Arc<LabMaterial>>,
    pub(crate) last: Option<FrameExport>,
}

/// The light a frame used, as [`crate::FrameRenderer::render_into`] resolved it.
pub(crate) struct ExportLight<'a> {
    pub sun: DirectionalLightUniform,
    pub fill_light: Option<[[f32; 3]; 2]>,
    pub extra_directional: Vec<DirectionalLightUniform>,
    pub outdoor: &'a crate::OutdoorParams,
    pub fov_y: f32,
}

impl SceneExporter {
    /// Records `scene` as the latest export.
    pub(crate) fn record(&mut self, frame_index: u64, scene: &FrameScene<'_>, light: &ExportLight<'_>) {
        let _span = tracing::info_span!("scene_export", meshes = scene.meshes.len()).entered();
        let mut instances = Vec::with_capacity(scene.meshes.len());
        let mut meshes = HashMap::new();
        let mut materials = HashMap::new();
        let mut copied = 0u32;
        for (index, fm) in scene.meshes.iter().enumerate() {
            if fm.vertices.is_empty() || fm.indices.is_empty() {
                continue;
            }
            // A LOD cross-fade draws an instance twice, each copy on its share of the pixels;
            // the export keeps the incoming copy (or the whole draw) once.
            if fm.lod_dither < 0.0 {
                continue;
            }
            // Key 0 is ephemeral (`FrameMesh`): fresh data every frame, never cached.
            let ephemeral_mesh = fm.mesh_key == 0 || is_ephemeral(fm.mesh_key);
            let mesh_key = if ephemeral_mesh {
                crate::EPHEMERAL_KEY | index as u64
            } else {
                fm.mesh_key
            };
            // The GPU cache trusts the key; the counts catch a key reused for other data.
            let fresh = !ephemeral_mesh
                && self.meshes.get(&mesh_key).is_some_and(|m| {
                    m.vertices.len() == fm.vertices.len() && m.indices.len() == fm.indices.len()
                });
            if fresh {
                if let Some(m) = self.meshes.get(&mesh_key) {
                    meshes.entry(mesh_key).or_insert_with(|| Arc::clone(m));
                }
            } else {
                copied += 1;
                let m = Arc::new(CpuMesh {
                    vertices: fm.vertices.to_vec(),
                    indices: fm.indices.to_vec(),
                });
                if !ephemeral_mesh {
                    self.meshes.insert(mesh_key, Arc::clone(&m));
                }
                meshes.insert(mesh_key, m);
            }
            let ephemeral_material = fm.material_key == 0 || is_ephemeral(fm.material_key);
            let material_key = if ephemeral_material {
                crate::EPHEMERAL_KEY | index as u64
            } else {
                fm.material_key
            };
            if ephemeral_material {
                materials.insert(material_key, Arc::new(fm.material.clone()));
            } else if let std::collections::hash_map::Entry::Vacant(slot) = materials.entry(material_key) {
                // The first draw of a key in this frame decides its export. A scalar edit
                // keeps the key (`material_texture_key`): the renderer rewrites the uniform in
                // place, so the export takes the edited copy too.
                let material = match self.materials.get(&material_key) {
                    Some(cached) if same_parameters(cached, fm.material) => Arc::clone(cached),
                    _ => {
                        let fresh = Arc::new(fm.material.clone());
                        self.materials.insert(material_key, Arc::clone(&fresh));
                        fresh
                    }
                };
                slot.insert(material);
            }
            instances.push(ExportedInstance {
                mesh_key,
                material_key,
                object_key: fm.object_key,
                model: fm.model,
                joints: fm.joints.to_vec(),
            });
        }
        // Forget data no frame draws any more (bounded memory while the export stays on).
        let live: HashSet<u64> = meshes.keys().copied().collect();
        self.meshes.retain(|k, _| live.contains(k));
        let live: HashSet<u64> = materials.keys().copied().collect();
        self.materials.retain(|k, _| live.contains(k));
        self.last = Some(FrameExport {
            frame_index,
            eye: scene.eye,
            target: scene.target,
            fov_y: light.fov_y,
            width: scene.width.max(1),
            height: scene.height.max(1),
            sun: light.sun,
            fill_light: light.fill_light,
            extra_directional: light.extra_directional.clone(),
            ambient_lux: scene.ambient_lux,
            ibl: scene.ibl,
            outdoor: light.outdoor.clone(),
            local_lights: scene.local_lights.to_vec(),
            instances,
            meshes,
            materials,
            meshes_copied: copied,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameMesh, FrameQuality, LabVertex, ShadowCast};

    fn scene<'a>(meshes: Vec<FrameMesh<'a>>) -> FrameScene<'a> {
        FrameScene {
            width: 16,
            height: 8,
            eye: [0.0, 1.0, 5.0],
            target: [0.0; 3],
            fov_y: 0.9,
            near: 0.1,
            far: 100.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 10.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: None,
            fog: crate::FogSettings::default(),
            directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 1000.0),
            local_lights: &[],
            quality: FrameQuality::LAB,
            meshes,
        }
    }

    #[test]
    fn meshes_are_copied_once_and_reused() {
        let verts = vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]); 3];
        let idx = vec![0u32, 1, 2];
        let mat = LabMaterial::EMPTY;
        let mesh = |x: f32, key: u64| FrameMesh {
            is_static: false,
            mesh_key: key,
            material_key: 9,
            object_key: key,
            vertices: &verts,
            indices: &idx,
            material: &mat,
            model: regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::X * x).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: crate::LOD_DITHER_NONE,
        };
        let light = ExportLight {
            sun: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 500.0),
            fill_light: None,
            extra_directional: Vec::new(),
            outdoor: &crate::OutdoorParams::default(),
            fov_y: 0.9,
        };
        let mut exporter = SceneExporter::default();
        exporter.record(1, &scene(vec![mesh(0.0, 1), mesh(2.0, 1), mesh(4.0, 2)]), &light);
        let first = exporter.last.clone().expect("export");
        assert_eq!(first.instances.len(), 3);
        assert_eq!(first.meshes.len(), 2);
        assert_eq!(first.materials.len(), 1);
        assert_eq!(first.meshes_copied, 2);
        assert_eq!(first.sun.illuminance, 500.0);
        // A moved camera / moved instances: nothing is copied again, the data is shared.
        exporter.record(2, &scene(vec![mesh(1.0, 1), mesh(4.0, 2)]), &light);
        let second = exporter.last.clone().expect("export");
        assert_eq!(second.meshes_copied, 0);
        assert!(Arc::ptr_eq(&first.meshes[&1], &second.meshes[&1]));
        // Key 2 no longer drawn: dropped from the cache.
        exporter.record(3, &scene(vec![mesh(1.0, 1)]), &light);
        assert_eq!(exporter.meshes.len(), 1);
        // Ephemeral key 0: copied every frame under a per-frame key, never cached; the
        // outgoing copy of a LOD cross-fade is not exported.
        let outgoing = FrameMesh {
            lod_dither: -0.3,
            ..mesh(6.0, 1)
        };
        exporter.record(4, &scene(vec![mesh(0.0, 0), mesh(2.0, 0), outgoing]), &light);
        let fourth = exporter.last.clone().expect("export");
        assert_eq!(fourth.instances.len(), 2);
        assert_eq!(fourth.meshes_copied, 2);
        assert_ne!(fourth.instances[0].mesh_key, fourth.instances[1].mesh_key);
        assert!(fourth.instances.iter().all(|i| is_ephemeral(i.mesh_key)));
        assert!(exporter.meshes.is_empty(), "ephemeral meshes are not cached");
    }

    /// A scalar edit keeps the material key (`material_texture_key`; the renderer rewrites the
    /// uniform in place), so the export must take the new parameters under the same key
    /// instead of the copy from the key's first frame (audit render10 #1).
    #[test]
    fn a_scalar_edit_under_the_same_key_reaches_the_export() {
        let verts = vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]); 3];
        let idx = vec![0u32, 1, 2];
        let rough = LabMaterial::EMPTY;
        let smooth = LabMaterial {
            roughness: 0.2,
            ..LabMaterial::EMPTY
        };
        fn mesh<'a>(material: &'a LabMaterial, verts: &'a [LabVertex], idx: &'a [u32]) -> FrameMesh<'a> {
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 9,
                object_key: 1,
                vertices: verts,
                indices: idx,
                material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: crate::LOD_DITHER_NONE,
            }
        }
        let light = ExportLight {
            sun: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 500.0),
            fill_light: None,
            extra_directional: Vec::new(),
            outdoor: &crate::OutdoorParams::default(),
            fov_y: 0.9,
        };
        let mut exporter = SceneExporter::default();
        exporter.record(1, &scene(vec![mesh(&rough, &verts, &idx)]), &light);
        let first = exporter.last.clone().expect("export");
        assert_eq!(first.materials[&9].roughness, rough.roughness);
        exporter.record(2, &scene(vec![mesh(&smooth, &verts, &idx)]), &light);
        let second = exporter.last.clone().expect("export");
        assert_eq!(
            second.materials[&9].roughness, 0.2,
            "the second frame drew roughness 0.2 under key 9"
        );
        // An unchanged material under the key is still shared, not copied again.
        exporter.record(3, &scene(vec![mesh(&smooth, &verts, &idx)]), &light);
        let third = exporter.last.clone().expect("export");
        assert!(Arc::ptr_eq(&second.materials[&9], &third.materials[&9]));
        // A changed surface graph's parameters are a parameter edit too.
        let graph = |p: f32| {
            let mut g = crate::SurfaceGraph::new("fn material_surface() {}");
            g.params[1][0] = p;
            Arc::new(g)
        };
        let a = LabMaterial {
            surface_graph: Some(graph(0.25)),
            ..LabMaterial::EMPTY
        };
        let b = LabMaterial {
            surface_graph: Some(graph(0.75)),
            ..LabMaterial::EMPTY
        };
        exporter.record(4, &scene(vec![mesh(&a, &verts, &idx)]), &light);
        exporter.record(5, &scene(vec![mesh(&b, &verts, &idx)]), &light);
        let fifth = exporter.last.clone().expect("export");
        let params = fifth.materials[&9].surface_graph.as_ref().map(|g| g.params[1][0]);
        assert_eq!(params, Some(0.75));
    }

    /// Foliage wind sets `uses_time` and `max_vertex_offset` without changing the graph's
    /// code key or parameter block. The rasterizer reads those flags every frame; the
    /// export must not keep the still graph under the same material key.
    #[test]
    fn a_graph_flag_edit_under_the_same_key_reaches_the_export() {
        let verts = vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]); 3];
        let idx = vec![0u32, 1, 2];
        let mut graph = crate::SurfaceGraph::new("fn material_surface() {}");
        graph.params[1][0] = 0.4;
        let mut swaying_graph = graph.clone();
        swaying_graph.uses_time = true;
        swaying_graph.max_vertex_offset = 1.5;
        let still = LabMaterial {
            surface_graph: Some(std::sync::Arc::new(graph)),
            ..LabMaterial::EMPTY
        };
        let swaying = LabMaterial {
            surface_graph: Some(std::sync::Arc::new(swaying_graph)),
            ..LabMaterial::EMPTY
        };
        fn mesh<'a>(material: &'a LabMaterial, verts: &'a [LabVertex], idx: &'a [u32]) -> FrameMesh<'a> {
            FrameMesh {
                mesh_key: 1,
                material_key: 9,
                object_key: 1,
                vertices: verts,
                indices: idx,
                material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                is_static: false,
                lod_dither: crate::LOD_DITHER_NONE,
            }
        }
        let light = ExportLight {
            sun: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 500.0),
            fill_light: None,
            extra_directional: Vec::new(),
            outdoor: &crate::OutdoorParams::default(),
            fov_y: 0.9,
        };
        let mut exporter = SceneExporter::default();
        exporter.record(1, &scene(vec![mesh(&still, &verts, &idx)]), &light);
        exporter.record(2, &scene(vec![mesh(&swaying, &verts, &idx)]), &light);
        let second = exporter.last.clone().expect("export");
        let exported = second.materials[&9].surface_graph.as_ref().expect("graph");
        assert!(
            exported.uses_time,
            "the second frame's graph reads time under the same key"
        );
        assert_eq!(exported.max_vertex_offset, 1.5);
    }

    #[test]
    fn the_export_keeps_the_local_lights_and_their_source_size() {
        let lights = [crate::LabLocalLight::Point(crate::LabPointLight {
            position: [0.0, 2.0, 0.0],
            color: [1.0; 3],
            intensity: 800.0,
            radius: 10.0,
            cast_shadows: true,
            source_radius: Some(0.2),
        })];
        let light = ExportLight {
            sun: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 500.0),
            fill_light: None,
            extra_directional: Vec::new(),
            outdoor: &crate::OutdoorParams::default(),
            fov_y: 0.9,
        };
        let mut exporter = SceneExporter::default();
        let frame = FrameScene {
            local_lights: &lights,
            ..scene(Vec::new())
        };
        exporter.record(1, &frame, &light);
        let export = exporter.last.clone().expect("export");
        assert_eq!(export.local_lights.len(), 1);
        assert_eq!(
            crate::light_source::light_source_radius(&export.local_lights[0]),
            Some(0.2)
        );
    }
}
