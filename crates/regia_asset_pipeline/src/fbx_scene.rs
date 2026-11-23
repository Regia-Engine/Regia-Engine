// SPDX-License-Identifier: MPL-2.0
//! The FBX reader: the one module that touches the `ufbx` crate (ADR-1602).
//!
//! `ufbx` (MIT OR Unlicense) parses the file in C; this module copies what the importer needs
//! out of its scene into owned [`GlbScene`] values before the `ufbx` scene is freed, so nothing
//! else in Regia Engine sees an FFI type. Every index read from the parsed scene goes through
//! `get`, so a malformed file the parser accepted still cannot panic here.
//!
//! Space conversion is `ufbx`'s: the file's axes and unit become right-handed, +Y up, front +Z,
//! one unit per meter (divided by the import `scale`), which is the glTF 2.0 frame; the glTF
//! importer then turns +Z front into Regia Engine's −Z forward (`Forward::Convert`).

use std::collections::HashMap;
use std::path::Path;

use ufbx::{
    AllocatorOpts, BakeOpts, CoordinateAxes, CoordinateAxis, FileFormat, GeometryTransformHandling,
    InheritModeHandling, LoadOpts, MirrorAxis, SpaceConversion,
};

use crate::fbx_import::{
    FbxClipInfo, FbxError, FbxImportReport, FbxImportSettings, FbxTextureInfo, FbxTextureStatus,
};
use crate::gltf_write::{
    GlbAnimation, GlbChannel, GlbImage, GlbMaterial, GlbMesh, GlbMorphTarget, GlbNode, GlbPath, GlbPrimitive,
    GlbScene, GlbSkin, GlbTextureRef, encode_uri_path,
};

/// Most UV sets written per primitive (`TEXCOORD_0..7`).
const MAX_UV_SETS: usize = 8;
/// Most skin influences kept per vertex (two `JOINTS_n` sets; the cooked mesh holds eight).
const MAX_INFLUENCES: usize = 8;

/// Parses `bytes` (already pre-scanned) and converts the scene.
pub(crate) fn read_scene(
    bytes: &[u8],
    base_dir: Option<&Path>,
    settings: &FbxImportSettings,
) -> Result<(GlbScene, FbxImportReport), FbxError> {
    let limits = &settings.limits;
    let allocator = || AllocatorOpts {
        memory_limit: limits.max_memory_bytes,
        allocation_limit: limits.max_allocations,
        ..AllocatorOpts::default()
    };
    let scale = if settings.scale.is_finite() && settings.scale > 0.0 {
        f64::from(settings.scale)
    } else {
        1.0
    };
    let opts = LoadOpts {
        temp_allocator: allocator(),
        result_allocator: allocator(),
        // The C parser never opens a file: textures are resolved below, in Rust, inside the
        // source directory.
        load_external_files: false,
        ignore_animation: !settings.import_animation,
        file_format: FileFormat::Fbx,
        no_format_from_content: true,
        no_format_from_extension: true,
        node_depth_limit: limits.max_node_depth,
        generate_missing_normals: true,
        clean_skin_weights: true,
        target_axes: CoordinateAxes {
            right: CoordinateAxis::PositiveX,
            up: CoordinateAxis::PositiveY,
            front: CoordinateAxis::PositiveZ,
        },
        target_unit_meters: 1.0 / scale,
        space_conversion: SpaceConversion::ModifyGeometry,
        geometry_transform_handling: GeometryTransformHandling::ModifyGeometry,
        inherit_mode_handling: InheritModeHandling::Compensate,
        handedness_conversion_axis: MirrorAxis::Z,
        ..LoadOpts::default()
    };
    let root = ufbx::load_memory(bytes, opts).map_err(|e| FbxError::from_ufbx(e.type_, &e.description))?;
    let scene: &ufbx::Scene = &root;

    let mut report = FbxImportReport {
        ascii: scene.metadata.ascii,
        version: scene.metadata.version,
        creator: scene.metadata.creator.to_string(),
        source_unit_meters: source_unit(&scene.settings),
        source_up_axis: axis_name(source_up(&scene.settings)).to_string(),
        scale: settings.scale,
        ..FbxImportReport::default()
    };
    for warning in scene.metadata.warnings.iter() {
        report.warnings.push(format!("ufbx: {}", &*warning.description));
    }
    if scene.nodes.len() > limits.max_nodes {
        return Err(FbxError::Limit(format!(
            "{} nodes, more than the limit of {}",
            scene.nodes.len(),
            limits.max_nodes
        )));
    }

    let mut out = GlbScene {
        generator: format!("Regia Engine FBX import (ufbx {})", ufbx_version()),
        ..GlbScene::default()
    };

    // Nodes: every node but the implicit root, in the scene's order; `typed_id` -> glTF index.
    let mut node_index: HashMap<u32, usize> = HashMap::new();
    for node in scene.nodes.iter() {
        if node.is_root {
            continue;
        }
        node_index.insert(node.element.typed_id, out.nodes.len());
        let t = &node.local_transform;
        out.nodes.push(GlbNode {
            name: node.element.name.to_string(),
            translation: vec3(&t.translation),
            rotation: [
                t.rotation.x as f32,
                t.rotation.y as f32,
                t.rotation.z as f32,
                t.rotation.w as f32,
            ],
            scale: vec3(&t.scale),
            ..GlbNode::default()
        });
    }
    for node in scene.nodes.iter() {
        let Some(&index) = node_index.get(&node.element.typed_id) else {
            continue;
        };
        let children: Vec<usize> = node
            .children
            .iter()
            .filter_map(|child| node_index.get(&child.element.typed_id).copied())
            .collect();
        if let Some(gltf_node) = out.nodes.get_mut(index) {
            gltf_node.children = children;
        }
        let parent_is_root = node.parent.as_ref().is_none_or(|p| p.is_root);
        if parent_is_root {
            out.roots.push(index);
        }
    }
    report.nodes = out.nodes.len();

    // Textures: one image per distinct texture file, resolved in Rust.
    let mut images = ImageTable::default();
    // Materials, in the scene's order.
    let mut material_index: HashMap<u32, usize> = HashMap::new();
    for material in scene.materials.iter() {
        material_index.insert(material.element.typed_id, out.materials.len());
        let converted = convert_material(material, base_dir, &mut images, &mut report, limits.max_textures)?;
        out.materials.push(converted);
    }
    report.materials = out.materials.len();

    // Meshes: one glTF mesh per FBX mesh (instances share it), one primitive per material part.
    let mut totals = Totals::default();
    let mut mesh_index: HashMap<u32, (usize, Option<SkinSource>)> = HashMap::new();
    for mesh in scene.meshes.iter() {
        let converted = convert_mesh(mesh, &material_index, settings, &mut totals, &mut report)?;
        mesh_index.insert(mesh.element.typed_id, (out.meshes.len(), converted.skin));
        out.meshes.push(converted.mesh);
    }
    report.meshes = out.meshes.len();
    report.vertices = totals.vertices;
    report.triangles = totals.indices / 3;

    // Attach meshes and skins to their nodes. A skin is per (mesh, node): its joints are nodes.
    for node in scene.nodes.iter() {
        let (Some(&index), Some(mesh)) = (node_index.get(&node.element.typed_id), node.mesh.as_ref()) else {
            continue;
        };
        let Some((gltf_mesh, skin)) = mesh_index.get(&mesh.element.typed_id) else {
            continue;
        };
        let skin_index = match skin {
            Some(skin) => {
                let joints: Vec<usize> = skin
                    .joint_nodes
                    .iter()
                    .filter_map(|typed_id| node_index.get(typed_id).copied())
                    .collect();
                if joints.len() == skin.joint_nodes.len() && !joints.is_empty() {
                    out.skins.push(GlbSkin {
                        name: node.element.name.to_string(),
                        joints,
                        inverse_bind: skin.inverse_bind.clone(),
                    });
                    report.skins += 1;
                    report.joints += skin.joint_nodes.len();
                    Some(out.skins.len() - 1)
                } else {
                    report.warnings.push(format!(
                        "mesh {:?}: a skin joint is not a scene node; imported rigid",
                        &*mesh.element.name
                    ));
                    None
                }
            }
            None => None,
        };
        if let Some(gltf_node) = out.nodes.get_mut(index) {
            gltf_node.mesh = Some(*gltf_mesh);
            gltf_node.skin = skin_index;
        }
    }

    out.images = images.images;
    report.textures = images.infos;
    if settings.import_animation {
        convert_animations(scene, &node_index, &mut out, settings, &mut report)?;
    }
    Ok((out, report))
}

/// The `ufbx` source version as `major.minor.patch`.
fn ufbx_version() -> &'static str {
    "0.23.1"
}

/// The file's up axis: the authoring application's (`OriginalUpAxis`) when the file records
/// it, else the declared one (`UpAxis`).
fn source_up(settings: &ufbx::SceneSettings) -> CoordinateAxis {
    match settings.original_axis_up {
        CoordinateAxis::Unknown => settings.axes.up,
        axis => axis,
    }
}

/// The file's unit in meters: the original one when recorded, else the declared one.
fn source_unit(settings: &ufbx::SceneSettings) -> f64 {
    if settings.original_unit_meters.is_finite() && settings.original_unit_meters > 0.0 {
        settings.original_unit_meters
    } else {
        settings.unit_meters
    }
}

fn axis_name(axis: CoordinateAxis) -> &'static str {
    match axis {
        CoordinateAxis::PositiveX => "+X",
        CoordinateAxis::NegativeX => "-X",
        CoordinateAxis::PositiveY => "+Y",
        CoordinateAxis::NegativeY => "-Y",
        CoordinateAxis::PositiveZ => "+Z",
        CoordinateAxis::NegativeZ => "-Z",
        CoordinateAxis::Unknown => "unknown",
    }
}

fn vec3(v: &ufbx::Vec3) -> [f32; 3] {
    [v.x as f32, v.y as f32, v.z as f32]
}

/// A finite `f32` copy of `v` (a non-finite component becomes 0).
fn clean3(v: &ufbx::Vec3) -> [f32; 3] {
    vec3(v).map(|c| if c.is_finite() { c } else { 0.0 })
}

/// A 3×4 `ufbx` matrix as a glTF column-major 4×4.
fn mat4(m: &ufbx::Matrix) -> [f32; 16] {
    [
        m.m00 as f32,
        m.m10 as f32,
        m.m20 as f32,
        0.0,
        m.m01 as f32,
        m.m11 as f32,
        m.m21 as f32,
        0.0,
        m.m02 as f32,
        m.m12 as f32,
        m.m22 as f32,
        0.0,
        m.m03 as f32,
        m.m13 as f32,
        m.m23 as f32,
        1.0,
    ]
}

/// Running totals checked against the limits while meshes convert.
#[derive(Default)]
struct Totals {
    vertices: usize,
    indices: usize,
}

/// What a skinned mesh needs to become a glTF skin once nodes are known.
#[derive(Clone)]
struct SkinSource {
    /// Joint nodes by `ufbx` node typed id, in joint-slot order.
    joint_nodes: Vec<u32>,
    inverse_bind: Vec<[f32; 16]>,
}

/// One blend shape's offsets per control point: positions, normals, whether it has normals.
type ShapeOffsets = (Vec<[f32; 3]>, Vec<[f32; 3]>, bool);

struct ConvertedMesh {
    mesh: GlbMesh,
    skin: Option<SkinSource>,
}

/// The images written so far, by the resolved source.
#[derive(Default)]
struct ImageTable {
    images: Vec<GlbImage>,
    infos: Vec<FbxTextureInfo>,
    by_key: HashMap<String, usize>,
}

fn convert_material(
    material: &ufbx::Material,
    base_dir: Option<&Path>,
    images: &mut ImageTable,
    report: &mut FbxImportReport,
    max_textures: usize,
) -> Result<GlbMaterial, FbxError> {
    let pbr = &material.pbr;
    let name = material.element.name.to_string();
    let color = |map: &ufbx::MaterialMap, fallback: [f32; 3]| -> [f32; 3] {
        if map.has_value {
            [
                map.value_vec4.x as f32,
                map.value_vec4.y as f32,
                map.value_vec4.z as f32,
            ]
        } else {
            fallback
        }
    };
    let scalar = |map: &ufbx::MaterialMap, fallback: f32| -> f32 {
        if map.has_value {
            map.value_vec4.x as f32
        } else {
            fallback
        }
    };
    let base_factor = scalar(&pbr.base_factor, 1.0);
    let base = color(&pbr.base_color, [1.0; 3]).map(|c| c * base_factor);
    let opacity = scalar(&pbr.opacity, 1.0);
    let mut roughness = scalar(&pbr.roughness, 1.0);
    if material.features.roughness_as_glossiness.enabled {
        roughness = 1.0 - roughness;
    }
    let emission_factor = scalar(&pbr.emission_factor, 1.0);
    let emissive = color(&pbr.emission_color, [0.0; 3]).map(|c| c * emission_factor);
    let mut out = GlbMaterial {
        name: name.clone(),
        base_color: [base[0], base[1], base[2], opacity],
        metallic: scalar(&pbr.metalness, 0.0),
        roughness,
        emissive,
        alpha_blend: opacity < 1.0 || pbr.opacity.texture.is_some(),
        double_sided: material.features.double_sided.enabled,
        ..GlbMaterial::default()
    };
    let mut slot = |map: &ufbx::MaterialMap,
                    role: &str,
                    images: &mut ImageTable|
     -> Result<Option<GlbTextureRef>, FbxError> {
        let Some(texture) = map.texture.as_ref() else {
            return Ok(None);
        };
        Ok(
            resolve_texture(texture, &name, role, base_dir, images, report, max_textures)?
                .map(|image| GlbTextureRef { image, tex_coord: 0 }),
        )
    };
    out.base_color_texture = slot(&pbr.base_color, "base_color", images)?;
    out.normal_texture = slot(&pbr.normal_map, "normal", images)?;
    out.emissive_texture = slot(&pbr.emission_color, "emissive", images)?;
    let rough_texture = pbr.roughness.texture.as_ref();
    let metal_texture = pbr.metalness.texture.as_ref();
    if rough_texture.is_some() || metal_texture.is_some() {
        // Legacy shading models map a shininess (not roughness) texture to roughness: only PBR
        // shaders' maps are packed.
        let pbr_shader = !matches!(
            material.shader_type,
            ufbx::ShaderType::Unknown
                | ufbx::ShaderType::FbxLambert
                | ufbx::ShaderType::FbxPhong
                | ufbx::ShaderType::BlenderPhong
                | ufbx::ShaderType::WavefrontMtl
        );
        if pbr_shader {
            let mut load = |texture: Option<&ufbx::Ref<ufbx::Texture>>, role: &str| {
                let texture = texture?;
                let (pixels, file, status) = texture_pixels(texture, base_dir);
                if pixels.is_none() {
                    report.warnings.push(format!(
                        "material {name:?}: {role} texture {file:?} not usable; the factor is used"
                    ));
                }
                images.infos.push(FbxTextureInfo {
                    material: name.clone(),
                    role: role.to_string(),
                    file,
                    status: if pixels.is_none() && status != FbxTextureStatus::Missing {
                        FbxTextureStatus::Undecodable
                    } else {
                        status
                    },
                });
                pixels
            };
            let rough = load(rough_texture, "roughness");
            let metal = load(metal_texture, "metalness");
            if rough.is_some() || metal.is_some() {
                if images.images.len() >= max_textures {
                    return Err(FbxError::Limit(format!("more than {max_textures} textures")));
                }
                let invert = material.features.roughness_as_glossiness.enabled;
                if let Some(png) = pack_metallic_roughness(rough, metal, invert, out.roughness, out.metallic)
                {
                    images.images.push(GlbImage::Embedded {
                        bytes: png,
                        mime: "image/png",
                    });
                    out.metallic_roughness_texture = Some(GlbTextureRef {
                        image: images.images.len() - 1,
                        tex_coord: 0,
                    });
                    // The constants of a missing channel are baked into the packed map.
                    out.roughness = 1.0;
                    out.metallic = 1.0;
                }
            }
        } else {
            report.warnings.push(format!(
                "material {name:?}: a legacy shininess or reflection map is not converted to roughness/metalness; the factors are used"
            ));
        }
    }
    Ok(out)
}

/// Largest side of a packed metallic-roughness map.
const MAX_PACKED_SIDE: u32 = 16_384;

/// A texture's pixels: its embedded content, or its file inside `base_dir`; with the file name
/// as found (or as named when missing) and where it came from.
fn texture_pixels(
    texture: &ufbx::Texture,
    base_dir: Option<&Path>,
) -> (Option<image::DynamicImage>, String, FbxTextureStatus) {
    let relative = texture.relative_filename.to_string();
    let filename = texture.filename.to_string();
    let absolute = texture.absolute_filename.to_string();
    let named = [&relative, &filename, &absolute]
        .into_iter()
        .find(|name| !name.is_empty())
        .cloned()
        .unwrap_or_default();
    let ext = Path::new(&named.replace('\\', "/"))
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let content: &[u8] = &texture.content;
    if !content.is_empty() {
        let decoded = image::ImageFormat::from_extension(&ext)
            .and_then(|format| image::load_from_memory_with_format(content, format).ok())
            .or_else(|| image::load_from_memory(content).ok());
        return (decoded, named, FbxTextureStatus::Embedded);
    }
    match base_dir.and_then(|dir| find_texture_file(dir, &relative, &filename, &absolute)) {
        Some(found) => {
            let decoded = base_dir.and_then(|dir| image::open(dir.join(&found)).ok());
            (decoded, found, FbxTextureStatus::External)
        }
        None => (None, named, FbxTextureStatus::Missing),
    }
}

/// A glTF metallic-roughness map (roughness in G, metalness in B) as PNG bytes, from separate
/// roughness and metalness images (their luminance; roughness inverted when the file stores
/// glossiness). A missing channel is the constant factor. The metalness image is resized to the
/// roughness image's size when they differ. `None` when the size is over `MAX_PACKED_SIDE`.
fn pack_metallic_roughness(
    rough: Option<image::DynamicImage>,
    metal: Option<image::DynamicImage>,
    invert_rough: bool,
    rough_factor: f32,
    metal_factor: f32,
) -> Option<Vec<u8>> {
    let (width, height) = match (&rough, &metal) {
        (Some(r), _) => (r.width(), r.height()),
        (None, Some(m)) => (m.width(), m.height()),
        (None, None) => return None,
    };
    if width == 0 || height == 0 || width > MAX_PACKED_SIDE || height > MAX_PACKED_SIDE {
        return None;
    }
    let channel = |source: Option<image::DynamicImage>| -> Option<image::GrayImage> {
        let gray = source?.to_luma8();
        Some(if gray.dimensions() == (width, height) {
            gray
        } else {
            image::imageops::resize(&gray, width, height, image::imageops::FilterType::Triangle)
        })
    };
    let to_byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let rough = channel(rough);
    let metal = channel(metal);
    let mut packed = image::RgbaImage::new(width, height);
    for (x, y, pixel) in packed.enumerate_pixels_mut() {
        let r = rough.as_ref().map_or(to_byte(rough_factor), |g| {
            let v = g.get_pixel(x, y)[0];
            if invert_rough { 255 - v } else { v }
        });
        let m = metal
            .as_ref()
            .map_or(to_byte(metal_factor), |g| g.get_pixel(x, y)[0]);
        *pixel = image::Rgba([255, r, m, 255]);
    }
    let mut png = std::io::Cursor::new(Vec::new());
    packed.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(png.into_inner())
}

/// The image index for `texture`: its embedded bytes, or its file inside `base_dir`.
fn resolve_texture(
    texture: &ufbx::Texture,
    material: &str,
    role: &str,
    base_dir: Option<&Path>,
    images: &mut ImageTable,
    report: &mut FbxImportReport,
    max_textures: usize,
) -> Result<Option<usize>, FbxError> {
    let relative = texture.relative_filename.to_string();
    let filename = texture.filename.to_string();
    let absolute = texture.absolute_filename.to_string();
    let named = [&relative, &filename, &absolute]
        .into_iter()
        .find(|name| !name.is_empty())
        .cloned()
        .unwrap_or_default();
    let key = format!("{}|{}", texture.element.element_id, named);
    if let Some(&index) = images.by_key.get(&key) {
        return Ok(Some(index));
    }
    if images.images.len() >= max_textures {
        return Err(FbxError::Limit(format!("more than {max_textures} textures")));
    }
    let ext = Path::new(&named.replace('\\', "/"))
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let mut info = FbxTextureInfo {
        material: material.to_string(),
        role: role.to_string(),
        file: named.clone(),
        status: FbxTextureStatus::Missing,
    };
    let content: &[u8] = &texture.content;
    let image = if !content.is_empty() {
        info.status = FbxTextureStatus::Embedded;
        embedded_image(content, &ext)
    } else {
        match base_dir.and_then(|dir| find_texture_file(dir, &relative, &filename, &absolute)) {
            Some(found) => {
                info.status = FbxTextureStatus::External;
                info.file = found.clone();
                if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
                    Some(GlbImage::Uri(encode_uri_path(&found)))
                } else {
                    // TGA, BMP … carry no magic the glTF importer can sniff: re-encode as PNG.
                    base_dir
                        .and_then(|dir| std::fs::read(dir.join(&found)).ok())
                        .and_then(|bytes| embedded_image(&bytes, &ext))
                }
            }
            None => None,
        }
    };
    let Some(image) = image else {
        if info.status != FbxTextureStatus::Missing {
            report.warnings.push(format!(
                "material {material:?}: texture {named:?} could not be decoded; slot dropped"
            ));
            info.status = FbxTextureStatus::Undecodable;
        } else {
            report.warnings.push(format!(
                "material {material:?}: texture {named:?} not found beside the FBX; slot dropped"
            ));
        }
        images.infos.push(info);
        return Ok(None);
    };
    images.images.push(image);
    images.infos.push(info);
    let index = images.images.len() - 1;
    images.by_key.insert(key, index);
    Ok(Some(index))
}

/// Embedded texture bytes as a glTF image: PNG and JPEG as they are, anything the `image` crate
/// decodes by extension re-encoded as PNG.
fn embedded_image(bytes: &[u8], ext: &str) -> Option<GlbImage> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some(GlbImage::Embedded {
            bytes: bytes.to_vec(),
            mime: "image/png",
        });
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(GlbImage::Embedded {
            bytes: bytes.to_vec(),
            mime: "image/jpeg",
        });
    }
    let format = image::ImageFormat::from_extension(ext)?;
    let decoded = image::load_from_memory_with_format(bytes, format).ok()?;
    let mut png = std::io::Cursor::new(Vec::new());
    decoded.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(GlbImage::Embedded {
        bytes: png.into_inner(),
        mime: "image/png",
    })
}

/// A texture file inside `dir`, as a `/`-separated path relative to it: the FBX's relative
/// path when it stays inside `dir`, else the bare file name beside the FBX (artists' absolute
/// paths name their own machines). Never a path that leaves `dir`, nor, inside a project, a file
/// whose symlinks lead out of the project (`cook::join_portable`'s bound).
fn find_texture_file(dir: &Path, relative: &str, filename: &str, absolute: &str) -> Option<String> {
    // The shared path rule (ADR-4922): no `..`, absolute, drive or device name, no symlink out of
    // the project.
    let inside = |candidate: &str| crate::cook::inside_dir(dir, candidate);
    if let Some(found) = inside(relative) {
        return Some(found);
    }
    for name in [relative, filename, absolute] {
        let normal = name.replace('\\', "/");
        if let Some(base) = normal.rsplit('/').next()
            && let Some(found) = inside(base)
        {
            return Some(found);
        }
    }
    None
}

fn convert_mesh(
    mesh: &ufbx::Mesh,
    material_index: &HashMap<u32, usize>,
    settings: &FbxImportSettings,
    totals: &mut Totals,
    report: &mut FbxImportReport,
) -> Result<ConvertedMesh, FbxError> {
    let limits = &settings.limits;
    let name = mesh.element.name.to_string();
    let positions = &mesh.vertex_position;
    let normals = &mesh.vertex_normal;
    // UV sets: the chosen one first (it becomes `TEXCOORD_0`, the one the cook keeps).
    let mut uv_sets: Vec<&ufbx::UvSet> = mesh.uv_sets.iter().take(MAX_UV_SETS).collect();
    if let Some(wanted) = &settings.uv_set
        && let Some(pos) = uv_sets.iter().position(|set| &*set.name == wanted.as_str())
    {
        let chosen = uv_sets.remove(pos);
        uv_sets.insert(0, chosen);
    }
    for set in &uv_sets {
        let set_name = set.name.to_string();
        if !report.uv_sets.contains(&set_name) {
            report.uv_sets.push(set_name);
        }
    }
    if mesh.uv_sets.len() > MAX_UV_SETS {
        report.warnings.push(format!(
            "mesh {name:?}: {} UV sets, only the first {MAX_UV_SETS} are kept",
            mesh.uv_sets.len()
        ));
    }
    let colors = mesh.color_sets.first().map(|set| &set.vertex_color);

    // Skin: per control point, the strongest influences.
    let skin_deformer = mesh.skin_deformers.first();
    let mut skin_source: Option<SkinSource> = None;
    let mut cluster_slot: Vec<Option<u16>> = Vec::new();
    if let Some(skin) = skin_deformer {
        let mut joint_nodes = Vec::new();
        let mut inverse_bind = Vec::new();
        for cluster in skin.clusters.iter() {
            match cluster.bone_node.as_ref() {
                Some(bone) if joint_nodes.len() < usize::from(u16::MAX) => {
                    cluster_slot.push(Some(joint_nodes.len() as u16));
                    joint_nodes.push(bone.element.typed_id);
                    inverse_bind.push(mat4(&cluster.geometry_to_bone));
                }
                _ => cluster_slot.push(None),
            }
        }
        if !joint_nodes.is_empty() {
            skin_source = Some(SkinSource {
                joint_nodes,
                inverse_bind,
            });
        }
    }

    // Morph targets: per control point offsets of every blend channel's target shape.
    let mut target_names = Vec::new();
    let mut default_weights = Vec::new();
    let mut shape_offsets: Vec<ShapeOffsets> = Vec::new();
    for deformer in mesh.blend_deformers.iter() {
        for channel in deformer.channels.iter() {
            let shape = channel
                .target_shape
                .as_ref()
                .or_else(|| channel.keyframes.last().map(|k| &k.shape));
            let Some(shape) = shape else { continue };
            let mut pos = vec![[0.0f32; 3]; mesh.num_vertices];
            let mut nrm = vec![[0.0f32; 3]; mesh.num_vertices];
            let has_normals = !shape.normal_offsets.is_empty();
            for (i, &vertex) in shape.offset_vertices.iter().enumerate() {
                let weight = shape.offset_weights.get(i).map_or(1.0, |w| *w as f32);
                let Some(slot) = pos.get_mut(vertex as usize) else {
                    continue;
                };
                if let Some(offset) = shape.position_offsets.get(i) {
                    *slot = clean3(offset).map(|c| c * weight);
                }
                if let (Some(slot), Some(offset)) =
                    (nrm.get_mut(vertex as usize), shape.normal_offsets.get(i))
                {
                    *slot = clean3(offset).map(|c| c * weight);
                }
            }
            target_names.push(channel.element.name.to_string());
            let weight = channel.weight as f32;
            default_weights.push(if weight.is_finite() { weight } else { 0.0 });
            shape_offsets.push((pos, nrm, has_normals));
        }
    }
    report.morph_targets += target_names.len();

    // Material parts (always at least one when the mesh has faces).
    let all_faces: Vec<u32> = (0..mesh.faces.len() as u32).collect();
    let parts: Vec<(Option<usize>, &[u32])> = if mesh.material_parts.is_empty() {
        vec![(None, all_faces.as_slice())]
    } else {
        mesh.material_parts
            .iter()
            .map(|part| {
                let material = mesh
                    .materials
                    .get(part.index as usize)
                    .and_then(|m| material_index.get(&m.element.typed_id).copied());
                (material, part.face_indices.as_ref())
            })
            .collect()
    };

    let mut primitives = Vec::new();
    let mut triangle = vec![0u32; mesh.max_face_triangles.max(1).saturating_mul(3)];
    for (material, faces) in parts {
        let mut primitive = GlbPrimitive {
            material,
            uvs: vec![Vec::new(); uv_sets.len()],
            ..GlbPrimitive::default()
        };
        let mut control_points: Vec<u32> = Vec::new();
        let mut unique: HashMap<Vec<u32>, u32> = HashMap::new();
        let mut key = Vec::with_capacity(4 + uv_sets.len());
        for &face_index in faces {
            let Some(face) = mesh.faces.get(face_index as usize) else {
                continue;
            };
            if face.num_indices < 3 {
                continue;
            }
            let count = ufbx::triangulate_face(&mut triangle, mesh, *face) as usize;
            for &corner in triangle.iter().take(count * 3) {
                let ix = corner as usize;
                let Some(&control_point) = mesh.vertex_indices.get(ix) else {
                    continue;
                };
                key.clear();
                key.push(control_point);
                key.push(positions.indices.get(ix).copied().unwrap_or(0));
                key.push(if normals.exists {
                    normals.indices.get(ix).copied().unwrap_or(0)
                } else {
                    0
                });
                for set in &uv_sets {
                    key.push(set.vertex_uv.indices.get(ix).copied().unwrap_or(0));
                }
                if let Some(c) = colors {
                    key.push(c.indices.get(ix).copied().unwrap_or(0));
                }
                if let Some(&vertex) = unique.get(&key) {
                    primitive.indices.push(vertex);
                    continue;
                }
                let vertex = primitive.positions.len() as u32;
                unique.insert(key.clone(), vertex);
                primitive.indices.push(vertex);
                let p = positions
                    .indices
                    .get(ix)
                    .and_then(|i| positions.values.get(*i as usize))
                    .map_or([0.0; 3], clean3);
                primitive.positions.push(p);
                if normals.exists {
                    let n = normals
                        .indices
                        .get(ix)
                        .and_then(|i| normals.values.get(*i as usize))
                        .map_or([0.0, 1.0, 0.0], clean3);
                    primitive.normals.push(n);
                }
                for (slot, set) in primitive.uvs.iter_mut().zip(&uv_sets) {
                    let uv = set
                        .vertex_uv
                        .indices
                        .get(ix)
                        .and_then(|i| set.vertex_uv.values.get(*i as usize))
                        .map_or([0.0, 0.0], |uv| [uv.x as f32, uv.y as f32]);
                    // FBX's V runs up from the bottom; glTF's runs down from the top.
                    let uv = [uv[0], 1.0 - uv[1]].map(|c| if c.is_finite() { c } else { 0.0 });
                    slot.push(uv);
                }
                if let Some(c) = colors {
                    let color = c
                        .indices
                        .get(ix)
                        .and_then(|i| c.values.get(*i as usize))
                        .map_or([1.0; 4], |v| [v.x as f32, v.y as f32, v.z as f32, v.w as f32]);
                    primitive
                        .colors
                        .push(color.map(|c| if c.is_finite() { c } else { 1.0 }));
                }
                control_points.push(control_point);
            }
            totals.indices = totals.indices.saturating_add(count * 3);
            if totals.indices > limits.max_indices {
                return Err(FbxError::Limit(format!(
                    "more than {} triangle indices",
                    limits.max_indices
                )));
            }
        }
        totals.vertices = totals.vertices.saturating_add(primitive.positions.len());
        if totals.vertices > limits.max_vertices {
            return Err(FbxError::Limit(format!(
                "more than {} vertices",
                limits.max_vertices
            )));
        }
        if primitive.indices.is_empty() {
            continue;
        }
        if let (Some(skin), Some(_)) = (skin_deformer, &skin_source) {
            write_skin(&mut primitive, skin, &cluster_slot, &control_points);
        }
        for (pos, nrm, has_normals) in &shape_offsets {
            let at = |table: &Vec<[f32; 3]>, cp: &u32| table.get(*cp as usize).copied().unwrap_or([0.0; 3]);
            primitive.targets.push(GlbMorphTarget {
                positions: control_points.iter().map(|cp| at(pos, cp)).collect(),
                normals: if *has_normals && !primitive.normals.is_empty() {
                    control_points.iter().map(|cp| at(nrm, cp)).collect()
                } else {
                    Vec::new()
                },
            });
        }
        primitives.push(primitive);
    }
    Ok(ConvertedMesh {
        mesh: GlbMesh {
            name,
            primitives,
            target_names,
            weights: default_weights,
            ..GlbMesh::default()
        },
        skin: skin_source,
    })
}

/// `JOINTS_n` / `WEIGHTS_n` for a primitive from its vertices' control points: the strongest
/// [`MAX_INFLUENCES`] influences, normalized to sum to one.
fn write_skin(
    primitive: &mut GlbPrimitive,
    skin: &ufbx::SkinDeformer,
    cluster_slot: &[Option<u16>],
    control_points: &[u32],
) {
    let mut per_vertex: Vec<Vec<(u16, f32)>> = Vec::with_capacity(control_points.len());
    let mut sets = 1;
    for &cp in control_points {
        let mut influences: Vec<(u16, f32)> = Vec::new();
        if let Some(vertex) = skin.vertices.get(cp as usize) {
            let begin = vertex.weight_begin as usize;
            let end = begin.saturating_add(vertex.num_weights as usize);
            for weight in skin.weights.get(begin..end).unwrap_or(&[]) {
                let w = weight.weight as f32;
                if let Some(Some(slot)) = cluster_slot.get(weight.cluster_index as usize)
                    && w.is_finite()
                    && w > 0.0
                {
                    influences.push((*slot, w));
                }
            }
        }
        influences.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        influences.truncate(MAX_INFLUENCES);
        let sum: f32 = influences.iter().map(|i| i.1).sum();
        if sum > 0.0 {
            for influence in &mut influences {
                influence.1 /= sum;
            }
        }
        if influences.len() > 4 {
            sets = 2;
        }
        per_vertex.push(influences);
    }
    primitive.joints = vec![Vec::with_capacity(control_points.len()); sets];
    primitive.weights = vec![Vec::with_capacity(control_points.len()); sets];
    for influences in &per_vertex {
        for set in 0..sets {
            let mut joints = [0u16; 4];
            let mut weights = [0.0f32; 4];
            for (k, (joint, weight)) in influences.iter().skip(set * 4).take(4).enumerate() {
                joints[k] = *joint;
                weights[k] = *weight;
            }
            if set == 0 && influences.is_empty() {
                // An unweighted vertex follows the first joint rigidly.
                weights[0] = 1.0;
            }
            primitive.joints[set].push(joints);
            primitive.weights[set].push(weights);
        }
    }
}

fn convert_animations(
    scene: &ufbx::Scene,
    node_index: &HashMap<u32, usize>,
    out: &mut GlbScene,
    settings: &FbxImportSettings,
    report: &mut FbxImportReport,
) -> Result<(), FbxError> {
    let limits = &settings.limits;
    let rate = if settings.sample_rate.is_finite() && settings.sample_rate > 0.0 {
        f64::from(settings.sample_rate.clamp(1.0, 240.0))
    } else {
        30.0
    };
    let mut total_keys = 0usize;
    for stack in scene.anim_stacks.iter() {
        let duration = stack.time_end - stack.time_begin;
        if !duration.is_finite() || duration < 0.0 {
            report.warnings.push(format!(
                "clip {:?}: invalid time range; skipped",
                &*stack.element.name
            ));
            continue;
        }
        let opts = BakeOpts {
            temp_allocator: AllocatorOpts {
                memory_limit: limits.max_memory_bytes,
                allocation_limit: limits.max_allocations,
                ..AllocatorOpts::default()
            },
            result_allocator: AllocatorOpts {
                memory_limit: limits.max_memory_bytes,
                allocation_limit: limits.max_allocations,
                ..AllocatorOpts::default()
            },
            trim_start_time: true,
            // Non-linear segments resample at `rate`, at most 32 samples per source key
            // (`ufbx`'s default), so the key count is bounded by the file's own keys.
            resample_rate: rate,
            ..BakeOpts::default()
        };
        let baked = ufbx::bake_anim(scene, &stack.anim, opts)
            .map_err(|e| FbxError::from_ufbx(e.type_, &e.description))?;
        let mut clip = GlbAnimation {
            name: stack.element.name.to_string(),
            channels: Vec::new(),
        };
        for baked_node in baked.nodes.iter() {
            let Some(&node) = node_index.get(&baked_node.typed_id) else {
                continue;
            };
            let rest = out.nodes.get(node).cloned().unwrap_or_default();
            let translation: Vec<(f32, [f32; 3])> = baked_node
                .translation_keys
                .iter()
                .map(|k| (k.time as f32, clean3(&k.value)))
                .collect();
            let rotation: Vec<(f32, [f32; 4])> = baked_node
                .rotation_keys
                .iter()
                .map(|k| {
                    (
                        k.time as f32,
                        [
                            k.value.x as f32,
                            k.value.y as f32,
                            k.value.z as f32,
                            k.value.w as f32,
                        ],
                    )
                })
                .collect();
            let scale: Vec<(f32, [f32; 3])> = baked_node
                .scale_keys
                .iter()
                .map(|k| (k.time as f32, clean3(&k.value)))
                .collect();
            total_keys = total_keys.saturating_add(translation.len() + rotation.len() + scale.len());
            if total_keys > limits.max_animation_keys {
                return Err(FbxError::Limit(format!(
                    "more than {} animation keys",
                    limits.max_animation_keys
                )));
            }
            push_channel(
                &mut clip,
                node,
                GlbPath::Translation,
                &translation,
                rest.translation,
            );
            push_channel(&mut clip, node, GlbPath::Rotation, &rotation, rest.rotation);
            push_channel(&mut clip, node, GlbPath::Scale, &scale, rest.scale);
        }
        // Morph weights: blend channels' `DeformPercent`, resampled onto one shared key list
        // per mesh node.
        let mut channel_keys: HashMap<u32, Vec<(f32, f32)>> = HashMap::new();
        for element in baked.elements.iter() {
            for prop in element.props.iter() {
                if &*prop.name == "DeformPercent" {
                    let keys: Vec<(f32, f32)> = prop
                        .keys
                        .iter()
                        .map(|k| (k.time as f32, (k.value.x / 100.0) as f32))
                        .filter(|(t, v)| t.is_finite() && v.is_finite())
                        .collect();
                    channel_keys.insert(element.element_id, keys);
                }
            }
        }
        if !channel_keys.is_empty() {
            for node in scene.nodes.iter() {
                let (Some(&gltf_node), Some(mesh)) =
                    (node_index.get(&node.element.typed_id), node.mesh.as_ref())
                else {
                    continue;
                };
                let channels: Vec<&ufbx::BlendChannel> = mesh
                    .blend_deformers
                    .iter()
                    .flat_map(|d| d.channels.iter().map(|c| &**c))
                    .filter(|c| c.target_shape.is_some() || !c.keyframes.is_empty())
                    .collect();
                if channels.is_empty()
                    || !channels
                        .iter()
                        .any(|c| channel_keys.contains_key(&c.element.element_id))
                {
                    continue;
                }
                let gltf_mesh = out
                    .nodes
                    .get(gltf_node)
                    .and_then(|n| n.mesh)
                    .and_then(|m| out.meshes.get(m));
                if gltf_mesh.is_none_or(|m| m.target_names.len() != channels.len()) {
                    continue;
                }
                let mut times: Vec<f32> = channels
                    .iter()
                    .filter_map(|c| channel_keys.get(&c.element.element_id))
                    .flat_map(|keys| keys.iter().map(|k| k.0))
                    .collect();
                times.sort_by(f32::total_cmp);
                times.dedup();
                let mut values = Vec::with_capacity(times.len() * channels.len());
                for &t in &times {
                    for channel in &channels {
                        let value = match channel_keys.get(&channel.element.element_id) {
                            Some(keys) => sample_linear(keys, t),
                            None => channel.weight as f32,
                        };
                        values.push(value);
                    }
                }
                total_keys = total_keys.saturating_add(values.len());
                if total_keys > limits.max_animation_keys {
                    return Err(FbxError::Limit(format!(
                        "more than {} animation keys",
                        limits.max_animation_keys
                    )));
                }
                clip.channels.push(GlbChannel {
                    node: gltf_node,
                    path: GlbPath::Weights,
                    times,
                    values,
                });
            }
        }
        if clip.channels.is_empty() {
            continue;
        }
        let duration = clip
            .channels
            .iter()
            .filter_map(|c| c.times.last().copied())
            .fold(0.0f32, f32::max);
        report.clips.push(FbxClipInfo {
            name: clip.name.clone(),
            duration_seconds: duration,
            channels: clip.channels.len(),
        });
        out.animations.push(clip);
    }
    Ok(())
}

/// Adds a channel unless it is one key equal to the rest pose (an unanimated property).
fn push_channel<const N: usize>(
    clip: &mut GlbAnimation,
    node: usize,
    path: GlbPath,
    keys: &[(f32, [f32; N])],
    rest: [f32; N],
) {
    if keys.is_empty() {
        return;
    }
    let constant = keys
        .iter()
        .all(|(_, v)| v.iter().zip(&keys[0].1).all(|(a, b)| (a - b).abs() <= 1e-6));
    if constant && keys[0].1.iter().zip(&rest).all(|(a, b)| (a - b).abs() <= 1e-6) {
        return;
    }
    let keys: Vec<&(f32, [f32; N])> = keys.iter().filter(|(t, _)| t.is_finite()).collect();
    let mut times = Vec::with_capacity(keys.len());
    let mut values = Vec::with_capacity(keys.len() * N);
    for (t, v) in keys {
        // Strictly increasing key times (glTF § 3.11).
        if times.last().is_some_and(|last| *t <= *last) {
            continue;
        }
        times.push(*t);
        values.extend_from_slice(v);
    }
    clip.channels.push(GlbChannel {
        node,
        path,
        times,
        values,
    });
}

/// Linear interpolation of `(time, value)` keys at `t` (clamped at both ends).
fn sample_linear(keys: &[(f32, f32)], t: f32) -> f32 {
    let Some(first) = keys.first() else { return 0.0 };
    if t <= first.0 {
        return first.1;
    }
    for pair in keys.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if t <= b.0 {
            let span = b.0 - a.0;
            return if span > 0.0 {
                a.1 + (b.1 - a.1) * (t - a.0) / span
            } else {
                b.1
            };
        }
    }
    keys.last().map_or(0.0, |k| k.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_sampling_clamps_and_interpolates() {
        let keys = [(0.0, 0.0), (1.0, 1.0), (2.0, 0.0)];
        assert_eq!(sample_linear(&keys, -1.0), 0.0);
        assert_eq!(sample_linear(&keys, 0.5), 0.5);
        assert_eq!(sample_linear(&keys, 1.5), 0.5);
        assert_eq!(sample_linear(&keys, 9.0), 0.0);
        assert_eq!(sample_linear(&[], 1.0), 0.0);
    }

    #[test]
    fn texture_lookup_never_leaves_the_source_directory() {
        let dir = regia_test_utils::unique_temp_dir("fbx-texture-lookup");
        std::fs::create_dir_all(dir.join("tex")).expect("dir");
        std::fs::write(dir.join("tex/a.png"), b"x").expect("file");
        std::fs::write(dir.join("b.png"), b"x").expect("file");
        assert_eq!(
            find_texture_file(&dir, "tex/a.png", "", ""),
            Some("tex/a.png".into())
        );
        assert_eq!(
            find_texture_file(&dir, "tex\\a.png", "", ""),
            Some("tex/a.png".into())
        );
        assert_eq!(
            find_texture_file(&dir, "./tex/a.png", "", ""),
            Some("tex/a.png".into())
        );
        // An artist's absolute path falls back to the file beside the FBX.
        assert_eq!(
            find_texture_file(&dir, "", "", "C:\\Users\\art\\b.png"),
            Some("b.png".into())
        );
        assert_eq!(
            find_texture_file(&dir, "../outside/b.png", "", ""),
            Some("b.png".into())
        );
        assert_eq!(
            find_texture_file(&dir, "../../etc/passwd", "/etc/passwd", "/etc/passwd"),
            None
        );
        assert_eq!(find_texture_file(&dir, "", "", ""), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// audit-v034b-formats L10: a texture beside the FBX that is a symlink out of the project was
    /// read and cooked.
    #[cfg(unix)]
    #[test]
    fn a_texture_linked_out_of_the_project_is_not_found() {
        let root = regia_test_utils::unique_temp_dir("fbx-texture-link");
        let project = root.join("game");
        let models = project.join("assets/models");
        std::fs::create_dir_all(&models).expect("dirs");
        std::fs::write(project.join(regia_core::brand::MANIFEST_FILE), "").expect("manifest");
        std::fs::write(root.join("secret.png"), b"outside").expect("outside file");
        std::os::unix::fs::symlink(root.join("secret.png"), models.join("tex.png")).expect("link out");
        assert_eq!(find_texture_file(&models, "tex.png", "tex.png", ""), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-4922: the texture reference takes the shared path rule. Names that are plain files on
    /// Linux but a device, a drive-relative path or a stream on Windows are not found on any host,
    /// nor is a `..` climb or a link out of the project (each planted so only the rule refuses it).
    #[test]
    fn texture_references_take_the_shared_path_rule() {
        let root = regia_test_utils::unique_temp_dir("fbx-texture-rule");
        let project = root.join("game");
        let models = project.join("assets/models");
        std::fs::create_dir_all(&models).expect("dirs");
        std::fs::write(project.join(regia_core::brand::MANIFEST_FILE), "").expect("manifest");
        std::fs::write(project.join("assets/up.png"), b"x").expect("up");
        std::fs::write(models.join("ok.png"), b"x").expect("ok");
        assert_eq!(
            find_texture_file(&models, "ok.png", "ok.png", ""),
            Some("ok.png".into())
        );
        for planted in ["nul.png", "con.png", "C:x.png", "t.png:alt", "aux"] {
            let _ = std::fs::write(models.join(planted), b"x");
            assert_eq!(
                find_texture_file(&models, planted, planted, ""),
                None,
                "{planted}"
            );
        }
        assert_eq!(find_texture_file(&models, "../up.png", "", ""), None);
        assert_eq!(find_texture_file(&models, "..\\up.png", "", ""), None);
        #[cfg(unix)]
        {
            std::fs::write(root.join("secret.png"), b"outside").expect("outside");
            std::os::unix::fs::symlink(&root, models.join("out")).expect("dir link");
            assert_eq!(find_texture_file(&models, "out/secret.png", "", ""), None);
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
