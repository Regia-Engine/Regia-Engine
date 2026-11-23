// SPDX-License-Identifier: MPL-2.0
//! What a bench draws: a glTF from the fetched set, or a synthetic scene from this crate's own code.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use regia_core::math::{Mat4, Vec3};
use regia_render::{
    ALPHA_BLEND, ALPHA_MASK, ALPHA_OPAQUE, ALPHA_PREMULTIPLIED, CpuMaterial, CpuMesh, LabTexture, LabVertex,
    ThinFilmFactors, generate_vertex_normals, generate_vertex_tangents, volume_transmittance,
};

use super::BenchError;

/// The synthetic scenes: `(id, label)`. Original to this crate, no licence to record.
pub const SYNTHETIC: &[(&str, &str)] = &[
    ("synthetic_cubes", "10k cubes, 100 materials"),
    (
        "synthetic_atrium",
        "Pillared atrium: arcades, floors, 500 materials, 64 lights",
    ),
    (
        "synthetic_grass_1m",
        "Terrain patch with a 1,000,000-blade field, wind on (counters beside timings)",
    ),
    (
        "template_npc_200",
        "Plaza with 200 walking, animated, skinned NPCs (counters beside timings)",
    ),
];

/// The material probes of `regia bench --on` (ADR-2309): what the loader changes in a scene's
/// materials so a material feature has work to cost. All off is the scene as it always loaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaterialProbes {
    /// `thin-film`: a film (full coverage, the extension's default thickness) on every
    /// non-transmissive material: the whole screen pays for it, the upper bound.
    pub thin_film: bool,
    /// `transmission`: read the glTF `KHR_materials_transmission`, `_ior` and `_volume`
    /// extensions as the importer does (clear glass); the default bench ignores them.
    pub transmission: bool,
    /// `rough-transmission`: as `transmission`, with the material's roughness spreading what the
    /// glass shows (`LabMaterial::transmission_roughness`).
    pub rough_transmission: bool,
}

impl MaterialProbes {
    /// Any probe is on.
    #[must_use]
    pub fn any(self) -> bool {
        self.thin_film || self.transmission || self.rough_transmission
    }
}

/// How many distinct materials the probes changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeCounts {
    /// Materials given a film.
    pub thin_film: usize,
    /// Materials drawn as transmissive.
    pub transmission: usize,
}

/// One drawn mesh.
pub struct SceneMesh {
    /// Vertices and indices in object space.
    pub mesh: Arc<CpuMesh>,
    /// Its material.
    pub material: Arc<CpuMaterial>,
    /// Object to world.
    pub model: Mat4,
}

/// A point light of a synthetic scene.
#[derive(Clone, Copy)]
pub struct ScenePoint {
    /// World position.
    pub position: [f32; 3],
    /// Linear colour.
    pub color: [f32; 3],
    /// Luminous intensity in candela.
    pub intensity: f32,
    /// Range in metres.
    pub radius: f32,
}

/// Everything a bench run needs of a scene.
pub struct BenchScene {
    /// The meshes.
    pub meshes: Vec<SceneMesh>,
    /// Point lights (synthetic scenes only).
    pub points: Vec<ScenePoint>,
    /// Distinct materials.
    pub materials: usize,
    /// Triangles over all meshes.
    pub triangles: u64,
    /// World bounds' centre.
    pub center: Vec3,
    /// World bounds' radius.
    pub radius: f32,
    /// The camera orbit's radius as a multiple of [`Self::radius`]; under 1 orbits inside.
    pub orbit: f32,
    /// Fixed exposure in EV100.
    pub exposure_ev: f32,
    /// What the material probes changed (all zero without them).
    pub probed: ProbeCounts,
    /// What a generated scene adds to the meshes: the grass field or the crowd (ADR-2695).
    pub extra: Option<super::generated::Extra>,
}

impl BenchScene {
    fn from_parts(
        meshes: Vec<SceneMesh>,
        points: Vec<ScenePoint>,
        materials: usize,
    ) -> Result<Self, BenchError> {
        let (mut min, mut max) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
        let mut triangles = 0u64;
        for mesh in &meshes {
            triangles += mesh.mesh.indices.len() as u64 / 3;
            for vertex in &mesh.mesh.vertices {
                let world = mesh.model.transform_point3(Vec3::from_array(vertex.position));
                min = min.min(world);
                max = max.max(world);
            }
        }
        if meshes.is_empty() || !min.is_finite() || !max.is_finite() {
            return Err(BenchError::new("bench_load_failed", "the scene has no triangles"));
        }
        Ok(Self {
            meshes,
            points,
            materials,
            triangles,
            center: (min + max) * 0.5,
            radius: ((max - min).length() * 0.5).max(0.01),
            orbit: 2.2,
            exposure_ev: 14.5,
            probed: ProbeCounts::default(),
            extra: None,
        })
    }

    /// Applies the `thin-film` probe (the transmission probes act at load, [`load_gltf_with`]):
    /// every non-transmissive material gains a film. Shared materials stay shared.
    pub fn apply_probes(&mut self, probes: MaterialProbes) {
        if !probes.thin_film {
            return;
        }
        let mut filmed: HashMap<*const CpuMaterial, Arc<CpuMaterial>> = HashMap::new();
        for mesh in &mut self.meshes {
            if mesh.material.transmission > 0.0 || mesh.material.thin_film.is_on() {
                continue;
            }
            let key = Arc::as_ptr(&mesh.material);
            let material = filmed.entry(key).or_insert_with(|| {
                Arc::new(CpuMaterial {
                    thin_film: ThinFilmFactors {
                        factor: 1.0,
                        ..ThinFilmFactors::OFF
                    },
                    ..(*mesh.material).clone()
                })
            });
            mesh.material = Arc::clone(material);
        }
        self.probed.thin_film = filmed.len();
    }
}

fn vertex(position: [f32; 3], normal: [f32; 3], uv: [f32; 2], tangent: [f32; 4]) -> LabVertex {
    LabVertex {
        position,
        normal,
        uv,
        tangent,
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// Reads an image into the RGBA8 a material slot takes; `None` for a format this bench cannot use.
fn lab_texture(
    texture: &gltf::Texture<'_>,
    images: &[gltf::image::Data],
    cache: &mut HashMap<usize, Option<LabTexture>>,
) -> Option<LabTexture> {
    let index = texture.source().index();
    cache
        .entry(index)
        .or_insert_with(|| {
            let image = images.get(index)?;
            let pixels = image.width as usize * image.height as usize;
            let rgba = match image.format {
                gltf::image::Format::R8G8B8A8 => image.pixels.clone(),
                gltf::image::Format::R8G8B8 => image
                    .pixels
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .flat_map(|p| [p[0], p[1], p[2], 255])
                    .collect(),
                gltf::image::Format::R8G8 => image
                    .pixels
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|p| [p[0], p[1], 0, 255])
                    .collect(),
                gltf::image::Format::R8 => image.pixels.iter().flat_map(|p| [*p, *p, *p, 255]).collect(),
                _ => return None,
            };
            (rgba.len() == pixels * 4).then_some(LabTexture {
                width: image.width,
                height: image.height,
                rgba,
            })
        })
        .clone()
}

fn gltf_material(
    material: &gltf::Material<'_>,
    images: &[gltf::image::Data],
    cache: &mut HashMap<usize, Option<LabTexture>>,
    probes: MaterialProbes,
) -> CpuMaterial {
    let mut out = gltf_base_material(material, images, cache);
    if probes.transmission || probes.rough_transmission {
        apply_transmission(&mut out, material, probes.rough_transmission);
    }
    out
}

/// The importer's transmission mapping (`regia_asset_pipeline`'s glTF import, then
/// `hard_surface`'s material build): factor, IOR, Beer–Lambert tint, thickness, premultiplied.
fn apply_transmission(out: &mut CpuMaterial, material: &gltf::Material<'_>, rough: bool) {
    let Some(t) = material.transmission() else {
        return;
    };
    let factor = t.transmission_factor().clamp(0.0, 1.0);
    if factor <= 0.0 {
        return;
    }
    out.transmission = factor;
    out.ior = material
        .ior()
        .filter(|v| v.is_finite() && *v >= 1.0)
        .unwrap_or(1.5);
    if let Some(v) = material.volume() {
        let d = v.attenuation_distance();
        let distance = if d.is_finite() && d > 0.0 { d } else { 0.0 };
        out.transmission_thickness = v.thickness_factor().max(0.0);
        out.transmission_tint =
            volume_transmittance(out.transmission_thickness, v.attenuation_color(), distance);
    }
    out.transmission_roughness = if rough { out.roughness.clamp(0.0, 1.0) } else { 0.0 };
    out.alpha_mode = ALPHA_PREMULTIPLIED;
}

fn gltf_base_material(
    material: &gltf::Material<'_>,
    images: &[gltf::image::Data],
    cache: &mut HashMap<usize, Option<LabTexture>>,
) -> CpuMaterial {
    let pbr = material.pbr_metallic_roughness();
    let emissive = material.emissive_factor();
    CpuMaterial {
        base_color: pbr.base_color_factor(),
        metallic: pbr.metallic_factor(),
        roughness: pbr.roughness_factor(),
        normal_scale: material.normal_texture().map_or(1.0, |t| t.scale()),
        occlusion_strength: material.occlusion_texture().map_or(1.0, |t| t.strength()),
        emissive,
        emissive_intensity: 1.0,
        alpha_mode: match material.alpha_mode() {
            gltf::material::AlphaMode::Opaque => ALPHA_OPAQUE,
            gltf::material::AlphaMode::Mask => ALPHA_MASK,
            gltf::material::AlphaMode::Blend => ALPHA_BLEND,
        },
        alpha_cutoff: material.alpha_cutoff().unwrap_or(0.5),
        albedo: pbr
            .base_color_texture()
            .and_then(|info| lab_texture(&info.texture(), images, cache)),
        normal_map: material
            .normal_texture()
            .and_then(|info| lab_texture(&info.texture(), images, cache)),
        occlusion_map: material
            .occlusion_texture()
            .and_then(|info| lab_texture(&info.texture(), images, cache)),
        emissive_map: material
            .emissive_texture()
            .and_then(|info| lab_texture(&info.texture(), images, cache)),
        ..CpuMaterial::default()
    }
}

/// A primitive's mesh, or `None` for one that draws no triangles. Skins are drawn in the bind pose.
fn gltf_primitive(primitive: &gltf::Primitive<'_>, buffers: &[gltf::buffer::Data]) -> Option<CpuMesh> {
    if primitive.mode() != gltf::mesh::Mode::Triangles {
        return None;
    }
    let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data.0[..]));
    let positions: Vec<[f32; 3]> = reader.read_positions()?.collect();
    if positions.is_empty() {
        return None;
    }
    let count = positions.len();
    let indices: Vec<u32> = reader
        .read_indices()
        .map_or_else(|| (0..count as u32).collect(), |iter| iter.into_u32().collect());
    if indices.len() < 3 || indices.iter().any(|&index| index as usize >= count) {
        return None;
    }
    let normals: Vec<[f32; 3]> = reader.read_normals().map_or_else(
        || {
            let mut generated = vec![[0.0; 3]; count];
            generate_vertex_normals(&positions, &indices, &mut generated);
            generated
        },
        Iterator::collect,
    );
    let uvs: Vec<[f32; 2]> = reader
        .read_tex_coords(0)
        .map_or_else(|| vec![[0.0; 2]; count], |iter| iter.into_f32().collect());
    let tangents: Vec<[f32; 4]> = reader.read_tangents().map_or_else(
        || {
            let mut generated = vec![[1.0, 0.0, 0.0, 1.0]; count];
            generate_vertex_tangents(&positions, &normals, &uvs, &indices, &mut generated);
            generated
        },
        Iterator::collect,
    );
    if normals.len() != count || uvs.len() != count || tangents.len() != count {
        return None;
    }
    let vertices = (0..count)
        .map(|i| vertex(positions[i], normals[i], uvs[i], tangents[i]))
        .collect();
    Some(CpuMesh { vertices, indices })
}

/// Loads a glTF or GLB with the document's default scene (or its first), with the material
/// probes applied ([`MaterialProbes`]; all off is the scene as it always loaded).
///
/// # Errors
/// `bench_load_failed` when the file does not parse or draws nothing.
pub fn load_gltf_with(path: &Path, probes: MaterialProbes) -> Result<BenchScene, BenchError> {
    let failed = |message: String| BenchError::new("bench_load_failed", message);
    let (document, buffers, images) =
        gltf::import(path).map_err(|err| failed(format!("{}: {err}", path.display())))?;
    let scene = document
        .default_scene()
        .or_else(|| document.scenes().next())
        .ok_or_else(|| failed(format!("{}: no scene", path.display())))?;

    let mut cache = HashMap::new();
    let mut materials: Vec<Arc<CpuMaterial>> = document
        .materials()
        .map(|material| Arc::new(gltf_material(&material, &images, &mut cache, probes)))
        .collect();
    let transmissive = materials.iter().filter(|m| m.transmission > 0.0).count();
    let default_material = materials.len();
    materials.push(Arc::new(CpuMaterial::default()));

    let mut meshes = Vec::new();
    let mut stack: Vec<(gltf::Node<'_>, Mat4)> = scene.nodes().map(|node| (node, Mat4::IDENTITY)).collect();
    while let Some((node, parent)) = stack.pop() {
        let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        if let Some(mesh) = node.mesh() {
            for primitive in mesh.primitives() {
                let Some(cpu) = gltf_primitive(&primitive, &buffers) else {
                    continue;
                };
                let index = primitive.material().index().unwrap_or(default_material);
                meshes.push(SceneMesh {
                    mesh: Arc::new(cpu),
                    material: Arc::clone(&materials[index]),
                    model: world,
                });
            }
        }
        stack.extend(node.children().map(|child| (child, world)));
    }
    let mut scene = BenchScene::from_parts(meshes, Vec::new(), document.materials().len().max(1))?;
    scene.probed.transmission = transmissive;
    scene.apply_probes(probes);
    Ok(scene)
}

/// A unit box: 24 vertices, 12 triangles, centered on the origin.
pub(super) fn unit_box() -> CpuMesh {
    const FACES: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for (normal, u, v) in FACES {
        let base = vertices.len() as u32;
        let (n, u, v) = (Vec3::from_array(normal), Vec3::from_array(u), Vec3::from_array(v));
        for (su, sv) in [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
            let position = n * 0.5 + u * su + v * sv;
            vertices.push(vertex(
                position.to_array(),
                normal,
                [su + 0.5, 0.5 - sv],
                [u.x, u.y, u.z, 1.0],
            ));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    CpuMesh { vertices, indices }
}

/// A plain material whose color and roughness walk through `index` of `count`.
fn palette_material(index: usize, count: usize) -> Arc<CpuMaterial> {
    let t = index as f32 / count.max(1) as f32;
    Arc::new(CpuMaterial {
        base_color: [0.25 + 0.6 * t, 0.45, 0.85 - 0.55 * t, 1.0],
        roughness: 0.3 + 0.6 * ((index * 7) % count.max(1)) as f32 / count.max(1) as f32,
        ..CpuMaterial::default()
    })
}

/// A grid of boxes.
fn cubes(count: usize, materials: usize) -> Result<BenchScene, BenchError> {
    let mesh = Arc::new(unit_box());
    let palette: Vec<_> = (0..materials).map(|i| palette_material(i, materials)).collect();
    let side = (count as f32).sqrt().ceil() as usize;
    let meshes = (0..count)
        .map(|i| SceneMesh {
            mesh: Arc::clone(&mesh),
            material: Arc::clone(&palette[i % materials]),
            model: Mat4::from_translation(Vec3::new(
                (i % side) as f32 * 1.5,
                ((i * 13) % 5) as f32 * 0.5,
                (i / side) as f32 * 1.5,
            )),
        })
        .collect();
    BenchScene::from_parts(meshes, Vec::new(), materials)
}

/// An interior stand-in with the layout that makes large architecture scenes expensive: two floors
/// of arcades around a long hall, heavy occlusion, thousands of draws, many materials and local
/// lights. Original geometry from boxes; it has no textures, so it measures geometry and lighting.
fn atrium() -> Result<BenchScene, BenchError> {
    const MATERIALS: usize = 500;
    const BAYS: i32 = 24;
    const LIGHTS: usize = 64;
    let mesh = Arc::new(unit_box());
    let palette: Vec<_> = (0..MATERIALS).map(|i| palette_material(i, MATERIALS)).collect();
    let mut next = 0usize;
    let mut meshes = Vec::new();
    let mut piece = |center: Vec3, size: Vec3| {
        let material = Arc::clone(&palette[next % MATERIALS]);
        next += 1;
        meshes.push(SceneMesh {
            mesh: Arc::clone(&mesh),
            material,
            model: Mat4::from_translation(center) * Mat4::from_scale(size),
        });
    };
    let (bay, half_width, storey) = (4.0f32, 9.0f32, 6.0f32);
    let length = BAYS as f32 * bay;
    piece(Vec3::new(0.0, -0.25, 0.0), Vec3::new(40.0, 0.5, length + 12.0));
    for side in [-1.0f32, 1.0] {
        // The outer walls, and the roof over each aisle: the hall's sun comes through the middle.
        piece(
            Vec3::new(side * (half_width + 6.0), storey, 0.0),
            Vec3::new(0.6, storey * 2.0, length + 12.0),
        );
        piece(
            Vec3::new(side * (half_width + 3.0), storey * 2.0, 0.0),
            Vec3::new(6.0, 0.4, length + 12.0),
        );
        for b in 0..BAYS {
            let z = (b as f32 + 0.5 - BAYS as f32 * 0.5) * bay;
            for floor in 0..2 {
                let y = floor as f32 * storey;
                let x = side * half_width;
                // A column of drums, then arch voussoirs stepping across the bay.
                for drum in 0..8 {
                    let t = drum as f32 / 8.0;
                    let width = 0.9 - 0.2 * (t - 0.5).abs();
                    piece(
                        Vec3::new(x, y + (t + 0.0625) * storey * 0.7, z - bay * 0.5),
                        Vec3::new(width, storey * 0.7 / 8.0, width),
                    );
                }
                for stone in 0..11 {
                    let angle = std::f32::consts::PI * (stone as f32 + 0.5) / 11.0;
                    piece(
                        Vec3::new(
                            x,
                            y + storey * 0.7 + angle.sin() * bay * 0.45,
                            z - angle.cos() * bay * 0.45,
                        ),
                        Vec3::new(0.7, 0.45, 0.45),
                    );
                }
                // The gallery floor behind the arcade, and a balustrade along it.
                piece(
                    Vec3::new(x + side * 3.0, y + storey - 0.2, z),
                    Vec3::new(6.0, 0.4, bay),
                );
                if floor == 1 {
                    for post in 0..6 {
                        let along = (post as f32 + 0.5) / 6.0 - 0.5;
                        piece(Vec3::new(x, y + 0.5, z + along * bay), Vec3::new(0.2, 1.0, 0.2));
                    }
                }
            }
        }
    }
    let materials = next.min(MATERIALS);
    let points = (0..LIGHTS)
        .map(|i| {
            let side = if i % 2 == 0 { -1.0 } else { 1.0 };
            let along = ((i / 2) as f32 + 0.5) / (LIGHTS / 2) as f32 - 0.5;
            let warm = i as f32 / LIGHTS as f32;
            ScenePoint {
                position: [side * (half_width + 2.5), 3.0, along * length],
                color: [1.0, 0.8 - 0.2 * warm, 0.55 + 0.3 * warm],
                intensity: 800.0,
                radius: 12.0,
            }
        })
        .collect();
    let mut scene = BenchScene::from_parts(meshes, points, materials)?;
    // Inside the hall, under the arcades' tops.
    scene.center = Vec3::new(0.0, 3.5, 0.0);
    scene.radius = length * 0.5;
    scene.orbit = 0.15;
    // The hall is open to the sun down its middle, so it meters as daylight: 11.5 washed it out.
    scene.exposure_ev = 14.5;
    Ok(scene)
}

/// Builds a synthetic scene by id, or `None` when the id is not one.
///
/// # Errors
/// `bench_load_failed` from an empty scene (a bug here).
pub fn synthetic(id: &str, size: super::generated::SceneSize) -> Option<Result<BenchScene, BenchError>> {
    match id {
        "synthetic_cubes" => Some(cubes(10_000, 100)),
        "synthetic_atrium" => Some(atrium()),
        _ => super::generated::scene(id, size),
    }
}

#[cfg(test)]
mod tests {
    use super::super::generated::SceneSize;
    use super::*;

    #[test]
    fn unit_box_is_closed_and_outward() {
        let mesh = unit_box();
        assert_eq!((mesh.vertices.len(), mesh.indices.len()), (24, 36));
        for tri in mesh.indices.as_chunks::<3>().0 {
            let [a, b, c] = tri.map(|i| Vec3::from_array(mesh.vertices[i as usize].position));
            let normal = Vec3::from_array(mesh.vertices[tri[0] as usize].normal);
            assert!((b - a).cross(c - a).dot(normal) > 0.0, "winding of {tri:?}");
            assert!(a.dot(normal) > 0.0, "normal of {tri:?} points inward");
        }
    }

    /// Every synthetic id builds, with stable counts: a published number names the scene.
    #[test]
    fn synthetic_scenes_have_stable_counts() {
        for (id, _) in SYNTHETIC {
            assert!(synthetic(id, SceneSize::Small).is_some(), "{id}");
        }
        assert!(synthetic("nope", SceneSize::Full).is_none());
        let cubes = synthetic("synthetic_cubes", SceneSize::Full)
            .expect("id")
            .expect("scene");
        assert_eq!(cubes.meshes.len(), 10_000);
        assert_eq!(cubes.triangles, 120_000);
        assert_eq!(cubes.materials, 100);
        let atrium = synthetic("synthetic_atrium", SceneSize::Full)
            .expect("id")
            .expect("scene");
        assert_eq!(atrium.materials, 500);
        assert_eq!(atrium.points.len(), 64);
        assert!(atrium.meshes.len() > 1_500, "{}", atrium.meshes.len());
        assert!(atrium.orbit < 1.0, "the atrium's camera stays inside");
    }

    /// A minimal embedded GLB-free glTF: one triangle, no normals, no material.
    #[test]
    fn a_minimal_gltf_loads_with_generated_attributes() {
        let dir = regia_test_utils::unique_temp_dir("bench_gltf");
        let positions: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 2.0, 0.0]];
        let bytes: Vec<u8> = positions.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(dir.join("t.bin"), &bytes).expect("write");
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0,"translation":[1,0,0]}}],
            "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}}}}]}}],
            "buffers":[{{"uri":"t.bin","byteLength":{len}}}],"bufferViews":[{{"buffer":0,"byteLength":{len}}}],
            "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[2,2,0]}}]}}"#,
            len = bytes.len()
        );
        let path = dir.join("t.gltf");
        std::fs::write(&path, json).expect("write");
        let scene = load_gltf_with(&path, MaterialProbes::default()).expect("load");
        assert_eq!(scene.triangles, 1);
        assert_eq!(scene.meshes.len(), 1);
        assert!(
            (scene.center - Vec3::new(2.0, 1.0, 0.0)).length() < 1e-5,
            "{}",
            scene.center
        );
        let normal = scene.meshes[0].mesh.vertices[0].normal;
        assert!((normal[2].abs() - 1.0).abs() < 1e-5, "{normal:?}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_broken_gltf_is_an_error_not_a_panic() {
        let dir = regia_test_utils::unique_temp_dir("bench_gltf_bad");
        let path = dir.join("bad.glb");
        std::fs::write(&path, b"not a model").expect("write");
        assert_eq!(
            load_gltf_with(&path, MaterialProbes::default())
                .err()
                .map(|e| e.code),
            Some("bench_load_failed")
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// Two triangles: a glass material (`KHR_materials_transmission` + `_volume`, roughness 0.4)
    /// and a plain one. The default load ignores the extension; the probes read it, film the rest.
    #[test]
    fn material_probes_read_the_transmission_and_film_the_rest() {
        let dir = regia_test_utils::unique_temp_dir("bench_gltf_probes");
        let positions: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 2.0, 0.0]];
        let bytes: Vec<u8> = positions.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(dir.join("t.bin"), &bytes).expect("write");
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"extensionsUsed":["KHR_materials_transmission","KHR_materials_volume"],
            "scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],
            "materials":[
              {{"pbrMetallicRoughness":{{"roughnessFactor":0.4}},"extensions":{{
                "KHR_materials_transmission":{{"transmissionFactor":1.0}},
                "KHR_materials_volume":{{"thicknessFactor":0.5,"attenuationDistance":1.0,"attenuationColor":[0.5,1.0,1.0]}}}}}},
              {{"pbrMetallicRoughness":{{"roughnessFactor":0.7}}}}],
            "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"material":0}},{{"attributes":{{"POSITION":0}},"material":1}}]}}],
            "buffers":[{{"uri":"t.bin","byteLength":{len}}}],"bufferViews":[{{"buffer":0,"byteLength":{len}}}],
            "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[2,2,0]}}]}}"#,
            len = bytes.len()
        );
        let path = dir.join("t.gltf");
        std::fs::write(&path, json).expect("write");
        let glass_of = |scene: &BenchScene| {
            scene
                .meshes
                .iter()
                .map(|m| {
                    (
                        m.material.transmission,
                        m.material.transmission_roughness,
                        m.material.thin_film.is_on(),
                    )
                })
                .collect::<Vec<_>>()
        };

        let plain = load_gltf_with(&path, MaterialProbes::default()).expect("load");
        assert_eq!(glass_of(&plain), [(0.0, 0.0, false), (0.0, 0.0, false)]);
        assert_eq!(plain.probed, ProbeCounts::default());

        let clear = load_gltf_with(
            &path,
            MaterialProbes {
                transmission: true,
                ..MaterialProbes::default()
            },
        )
        .expect("load");
        assert_eq!(glass_of(&clear), [(1.0, 0.0, false), (0.0, 0.0, false)]);
        let glass = &clear.meshes[0].material;
        assert_eq!(glass.alpha_mode, ALPHA_PREMULTIPLIED);
        assert_eq!(glass.transmission_thickness, 0.5);
        assert!(
            (glass.transmission_tint[0] - 0.5f32.powf(0.5)).abs() < 1e-6,
            "{:?}",
            glass.transmission_tint
        );
        assert_eq!(clear.probed.transmission, 1);

        let rough = load_gltf_with(
            &path,
            MaterialProbes {
                rough_transmission: true,
                thin_film: true,
                ..MaterialProbes::default()
            },
        )
        .expect("load");
        // The glass spreads by its roughness and takes no film (the film skips transmissive
        // surfaces); the plain material is filmed.
        assert_eq!(glass_of(&rough), [(1.0, 0.4, false), (0.0, 0.0, true)]);
        assert_eq!(
            rough.probed,
            ProbeCounts {
                thin_film: 1,
                transmission: 1
            }
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// The film probe on a synthetic scene films each distinct material once and keeps sharing.
    #[test]
    fn the_film_probe_keeps_materials_shared() {
        let mut cubes = synthetic("synthetic_cubes", SceneSize::Full)
            .expect("id")
            .expect("scene");
        cubes.apply_probes(MaterialProbes {
            thin_film: true,
            ..MaterialProbes::default()
        });
        assert_eq!(cubes.probed.thin_film, 100);
        let distinct: std::collections::HashSet<_> =
            cubes.meshes.iter().map(|m| Arc::as_ptr(&m.material)).collect();
        assert_eq!(distinct.len(), 100);
        assert!(cubes.meshes.iter().all(|m| m.material.thin_film.is_on()));
        // Off changes nothing.
        let mut again = synthetic("synthetic_cubes", SceneSize::Full)
            .expect("id")
            .expect("scene");
        again.apply_probes(MaterialProbes::default());
        assert_eq!(again.probed, ProbeCounts::default());
        assert!(again.meshes.iter().all(|m| !m.material.thin_film.is_on()));
    }
}
