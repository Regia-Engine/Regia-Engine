// SPDX-License-Identifier: MPL-2.0
//! Wavefront OBJ import (pure Rust; `book/agents/fbx-import.md` § OBJ).
//!
//! An `.obj` file (and the `.mtl` libraries it names) is converted into an in-memory glTF 2.0
//! binary and cooked by the glTF importer, as FBX is (ADR-1602): the cooked formats are the glTF
//! ones. OBJ has no unit or axis metadata; it is read as meters, +Y up, right-handed (what DCC
//! tools write by default), and the glTF forward convention applies after.
//!
//! Read: `v` (with optional per-vertex `r g b` colors), `vt` (V flipped to glTF's top-left
//! origin), `vn`, `f` (polygons fan-triangulated; `v`, `v/t`, `v//n`, `v/t/n` corners; negative
//! indices count back), `o` (one mesh node per object), `usemtl` (one primitive per material),
//! `mtllib`. MTL: `newmtl`, `Kd`, `d`/`Tr`, `Ke`, `Ns` (to roughness), `Pr`, `Pm`, `map_Kd`,
//! `map_Bump`/`bump`/`norm`, `map_Ke`. Other statements are ignored. The format is the public
//! Wavefront OBJ/MTL description (Wavefront Technologies, "Advanced Visualizer" manual, Appendix B).

use std::collections::HashMap;
use std::path::Path;

use regia_asset::AssetId;
use serde::{Deserialize, Serialize};

use crate::gltf_import::{GltfImportResult, import_gltf_converted};
use crate::gltf_write::{
    GlbImage, GlbMaterial, GlbMesh, GlbNode, GlbPrimitive, GlbScene, GlbTextureRef, encode_uri_path,
};
use crate::lod_classes::LodThresholdInputs;
use crate::texture_compress::TextureCookQuality;
use crate::{Forward, ImportError};

/// Caps on one OBJ import (untrusted input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObjLimits {
    /// Largest `.obj` or `.mtl` file, in bytes (default 512 MiB).
    pub max_file_bytes: u64,
    /// Most output vertices (default 16 Mi).
    pub max_vertices: usize,
    /// Most output triangle indices (default 48 Mi).
    pub max_indices: usize,
    /// Most corners in one polygon (default 1,024).
    pub max_polygon_corners: usize,
    /// Most objects and materials (default 65,536).
    pub max_objects: usize,
}

impl Default for ObjLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 512 << 20,
            max_vertices: 16 << 20,
            max_indices: 48 << 20,
            max_polygon_corners: 1024,
            max_objects: 65_536,
        }
    }
}

/// OBJ importer settings (the `.meta` `settings` string, next to the glTF keys).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObjImportSettings {
    /// Uniform scale (OBJ has no unit; default 1 = meters).
    pub scale: f32,
    /// Hard caps.
    pub limits: ObjLimits,
}

impl Default for ObjImportSettings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            limits: ObjLimits::default(),
        }
    }
}

impl ObjImportSettings {
    /// Settings from the `.meta` `settings` string; blank is the defaults, glTF keys are
    /// ignored, a malformed string is an error.
    pub fn try_from_meta_string(settings: Option<&str>) -> Result<Self, ObjError> {
        match settings.filter(|s| !s.trim().is_empty()) {
            Some(s) => ron::from_str::<Self>(s).map_err(|e| ObjError::Settings(e.to_string())),
            None => Ok(Self::default()),
        }
    }
}

/// Why an OBJ import failed. [`ObjError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ObjError {
    /// The file could not be read.
    #[error("OBJ file read error: {0}")]
    Io(String),
    /// The file is larger than [`ObjLimits::max_file_bytes`].
    #[error("OBJ file is {size} bytes, more than the limit of {limit}")]
    TooLarge {
        /// File size in bytes.
        size: u64,
        /// The limit in bytes.
        limit: u64,
    },
    /// A malformed statement.
    #[error("OBJ line {line}: {message}")]
    Parse {
        /// 1-based line number.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// A count limit was exceeded.
    #[error("OBJ import limit exceeded: {0}")]
    Limit(String),
    /// The `.meta` settings string is malformed.
    #[error("OBJ importer settings: {0}")]
    Settings(String),
    /// The file has no faces.
    #[error("OBJ file has no faces")]
    NoMesh,
}

impl ObjError {
    /// The agent-protocol error code.
    pub fn code(&self) -> &'static str {
        match self {
            ObjError::Io(_) => "obj_io",
            ObjError::TooLarge { .. } => "obj_too_large",
            ObjError::Parse { .. } => "obj_parse",
            ObjError::Limit(_) => "obj_limit",
            ObjError::Settings(_) => "obj_bad_settings",
            ObjError::NoMesh => "obj_no_mesh",
        }
    }
}

/// What an OBJ conversion found.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ObjImportReport {
    /// Objects (mesh nodes) written.
    pub objects: usize,
    /// Output vertices.
    pub vertices: usize,
    /// Output triangles.
    pub triangles: usize,
    /// Materials.
    pub materials: usize,
    /// Texture files found beside the OBJ (relative paths).
    pub textures: Vec<String>,
    /// Problems that did not stop the import (a missing `.mtl` or texture, an unknown material).
    pub warnings: Vec<String>,
}

/// An OBJ file converted to an in-memory glTF 2.0 binary.
#[derive(Clone, Debug)]
pub struct ObjConversion {
    /// The GLB bytes the glTF importer cooks.
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: ObjImportReport,
}

/// Reads a file refusing one larger than `limit` bytes.
fn read_capped(path: &Path, limit: u64) -> Result<Vec<u8>, ObjError> {
    use std::io::Read;
    let file = crate::mesh_convert::open_regular(path)
        .map_err(|e| ObjError::Io(format!("{}: {e}", path.display())))?;
    let size = file
        .metadata()
        .map_err(|e| ObjError::Io(format!("{}: {e}", path.display())))?
        .len();
    if size > limit {
        return Err(ObjError::TooLarge { size, limit });
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| ObjError::Io(format!("{}: {e}", path.display())))?;
    if bytes.len() as u64 > limit {
        return Err(ObjError::TooLarge {
            size: bytes.len() as u64,
            limit,
        });
    }
    Ok(bytes)
}

/// Reads an `.obj` file within `limits.max_file_bytes`.
pub fn read_obj_file(path: &Path, limits: &ObjLimits) -> Result<Vec<u8>, ObjError> {
    read_capped(path, limits.max_file_bytes)
}

/// A relative path inside `dir` that names a file, `/`-separated: the shared path rule (no `..`,
/// absolute, drive or device name, ADR-4922), and inside a project the file, symlinks resolved,
/// must stay in the project (`cook::inside_dir`): a link in the asset folder must not pull an
/// outside file into the cook.
fn inside(dir: &Path, name: &str) -> Option<String> {
    crate::cook::inside_dir(dir, name)
}

fn parse_floats<const N: usize>(parts: &[&str], line: usize, what: &str) -> Result<[f32; N], ObjError> {
    let mut out = [0.0f32; N];
    for (slot, token) in out.iter_mut().zip(parts) {
        let v: f32 = token.parse().map_err(|_| ObjError::Parse {
            line,
            message: format!("{what}: {token:?} is not a number"),
        })?;
        *slot = if v.is_finite() { v } else { 0.0 };
    }
    if parts.len() < N {
        return Err(ObjError::Parse {
            line,
            message: format!("{what} needs {N} numbers"),
        });
    }
    Ok(out)
}

/// One `newmtl` block.
#[derive(Clone, Debug, Default)]
struct MtlMaterial {
    name: String,
    diffuse: Option<[f32; 3]>,
    opacity: Option<f32>,
    emissive: Option<[f32; 3]>,
    shininess: Option<f32>,
    roughness: Option<f32>,
    metallic: Option<f32>,
    diffuse_map: Option<String>,
    normal_map: Option<String>,
    emissive_map: Option<String>,
}

/// The last whitespace-separated token of a map statement (map options come first).
fn map_file(rest: &str) -> Option<String> {
    rest.split_whitespace().last().map(str::to_string)
}

fn parse_mtl(text: &str, out: &mut Vec<MtlMaterial>, limits: &ObjLimits) -> Result<(), ObjError> {
    for (n, raw) in text.lines().enumerate() {
        let line = n + 1;
        let content = raw.split('#').next().unwrap_or("").trim();
        let mut split = content.splitn(2, char::is_whitespace);
        let keyword = split.next().unwrap_or("");
        let rest = split.next().unwrap_or("").trim();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if keyword == "newmtl" {
            if out.len() >= limits.max_objects {
                return Err(ObjError::Limit(format!(
                    "more than {} materials",
                    limits.max_objects
                )));
            }
            out.push(MtlMaterial {
                name: rest.to_string(),
                ..MtlMaterial::default()
            });
            continue;
        }
        let Some(material) = out.last_mut() else { continue };
        match keyword {
            "Kd" => material.diffuse = Some(parse_floats::<3>(&parts, line, "Kd")?),
            "Ke" => material.emissive = Some(parse_floats::<3>(&parts, line, "Ke")?),
            "d" => material.opacity = Some(parse_floats::<1>(&parts, line, "d")?[0]),
            "Tr" => material.opacity = Some(1.0 - parse_floats::<1>(&parts, line, "Tr")?[0]),
            "Ns" => material.shininess = Some(parse_floats::<1>(&parts, line, "Ns")?[0]),
            "Pr" => material.roughness = Some(parse_floats::<1>(&parts, line, "Pr")?[0]),
            "Pm" => material.metallic = Some(parse_floats::<1>(&parts, line, "Pm")?[0]),
            "map_Kd" => material.diffuse_map = map_file(rest),
            "map_Bump" | "map_bump" | "bump" | "norm" => material.normal_map = map_file(rest),
            "map_Ke" => material.emissive_map = map_file(rest),
            _ => {}
        }
    }
    Ok(())
}

/// One OBJ corner: position, texture coordinate and normal indices (0-based).
type Corner = (usize, Option<usize>, Option<usize>);

/// Resolves a 1-based (or negative, relative) OBJ index against `count` elements.
fn resolve(token: &str, count: usize, line: usize, what: &str) -> Result<usize, ObjError> {
    let value: i64 = token.parse().map_err(|_| ObjError::Parse {
        line,
        message: format!("{what} index {token:?} is not an integer"),
    })?;
    let index = if value > 0 {
        usize::try_from(value - 1).ok()
    } else if value < 0 {
        usize::try_from(value.unsigned_abs())
            .ok()
            .and_then(|back| count.checked_sub(back))
    } else {
        None
    };
    index.filter(|i| *i < count).ok_or_else(|| ObjError::Parse {
        line,
        message: format!("{what} index {token} outside the {count} defined so far"),
    })
}

/// The primitive being filled for one (object, material) pair.
#[derive(Default)]
struct Builder {
    primitive: GlbPrimitive,
    unique: HashMap<Corner, u32>,
}

/// One object's primitives by material name (`None` before any `usemtl`), in first-use order.
type ObjectPrimitives = Vec<(Option<String>, Builder)>;

/// Converts OBJ bytes to an in-memory glTF 2.0 binary. `base_dir` is the OBJ's directory:
/// `mtllib` files and textures are read inside it only (none when `None`).
pub fn convert_obj_to_glb(
    bytes: &[u8],
    base_dir: Option<&Path>,
    settings: &ObjImportSettings,
) -> Result<ObjConversion, ObjError> {
    let _span = tracing::info_span!("obj_convert", bytes = bytes.len()).entered();
    let limits = &settings.limits;
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(ObjError::TooLarge {
            size: bytes.len() as u64,
            limit: limits.max_file_bytes,
        });
    }
    let text = String::from_utf8_lossy(bytes);
    let scale = if settings.scale.is_finite() && settings.scale > 0.0 {
        settings.scale
    } else {
        1.0
    };
    let mut report = ObjImportReport::default();
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut colors: Vec<Option<[f32; 3]>> = Vec::new();
    let mut texcoords: Vec<[f32; 2]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut materials: Vec<MtlMaterial> = Vec::new();
    // Objects in file order: (name, primitives by material name in first-use order).
    let mut objects: Vec<(String, ObjectPrimitives)> = Vec::new();
    let mut current_material: Option<String> = None;
    let mut corners: Vec<Corner> = Vec::new();
    let mut total_indices = 0usize;
    let mut total_vertices = 0usize;
    for (n, raw) in text.lines().enumerate() {
        let line = n + 1;
        let content = raw.split('#').next().unwrap_or("").trim();
        let mut split = content.splitn(2, char::is_whitespace);
        let keyword = split.next().unwrap_or("");
        let rest = split.next().unwrap_or("").trim();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        match keyword {
            "v" => {
                let p = parse_floats::<3>(&parts, line, "v")?;
                positions.push(p.map(|c| c * scale));
                colors.push(if parts.len() >= 6 {
                    Some(parse_floats::<3>(&parts[3..], line, "v color")?)
                } else {
                    None
                });
            }
            "vt" => {
                let uv = parse_floats::<1>(&parts, line, "vt")?;
                let v = if parts.len() >= 2 {
                    parse_floats::<1>(&parts[1..], line, "vt")?[0]
                } else {
                    0.0
                };
                texcoords.push([uv[0], 1.0 - v]);
            }
            "vn" => normals.push(parse_floats::<3>(&parts, line, "vn")?),
            "o" => {
                if objects.len() >= limits.max_objects {
                    return Err(ObjError::Limit(format!(
                        "more than {} objects",
                        limits.max_objects
                    )));
                }
                objects.push((rest.to_string(), Vec::new()));
            }
            "usemtl" => current_material = Some(rest.to_string()),
            "mtllib" => {
                for name in &parts {
                    let Some(dir) = base_dir else {
                        report
                            .warnings
                            .push(format!("mtllib {name}: no source directory"));
                        continue;
                    };
                    match inside(dir, name) {
                        Some(found) => {
                            let mtl = read_capped(&dir.join(&found), limits.max_file_bytes)?;
                            parse_mtl(&String::from_utf8_lossy(&mtl), &mut materials, limits)?;
                        }
                        None => report
                            .warnings
                            .push(format!("mtllib {name} not found beside the OBJ")),
                    }
                }
            }
            "f" => {
                if parts.len() < 3 {
                    return Err(ObjError::Parse {
                        line,
                        message: "a face needs three corners".into(),
                    });
                }
                if parts.len() > limits.max_polygon_corners {
                    return Err(ObjError::Limit(format!(
                        "a face with {} corners (limit {})",
                        parts.len(),
                        limits.max_polygon_corners
                    )));
                }
                corners.clear();
                for token in &parts {
                    let mut fields = token.split('/');
                    let v = resolve(fields.next().unwrap_or(""), positions.len(), line, "vertex")?;
                    let t = match fields.next() {
                        Some(t) if !t.is_empty() => Some(resolve(t, texcoords.len(), line, "texture")?),
                        _ => None,
                    };
                    let normal = match fields.next() {
                        Some(n) if !n.is_empty() => Some(resolve(n, normals.len(), line, "normal")?),
                        _ => None,
                    };
                    corners.push((v, t, normal));
                }
                if objects.is_empty() {
                    objects.push((String::new(), Vec::new()));
                }
                let Some((_, primitives)) = objects.last_mut() else {
                    continue;
                };
                let index = match primitives.iter().position(|(m, _)| *m == current_material) {
                    Some(index) => index,
                    None => {
                        primitives.push((current_material.clone(), Builder::default()));
                        primitives.len() - 1
                    }
                };
                let Some((_, builder)) = primitives.get_mut(index) else {
                    continue;
                };
                // Fan triangulation from the first corner.
                for pair in corners[1..].windows(2) {
                    for corner in [corners[0], pair[0], pair[1]] {
                        let vertex = match builder.unique.get(&corner) {
                            Some(&vertex) => vertex,
                            None => {
                                let vertex = builder.primitive.positions.len() as u32;
                                let (v, t, normal) = corner;
                                builder.primitive.positions.push(positions[v]);
                                if let Some(Some(color)) = colors.get(v) {
                                    builder.primitive.colors.push([color[0], color[1], color[2], 1.0]);
                                }
                                if let Some(t) = t {
                                    if builder.primitive.uvs.is_empty() {
                                        builder.primitive.uvs.push(Vec::new());
                                    }
                                    builder.primitive.uvs[0].push(texcoords[t]);
                                }
                                if let Some(normal) = normal {
                                    builder.primitive.normals.push(normals[normal]);
                                }
                                builder.unique.insert(corner, vertex);
                                total_vertices += 1;
                                if total_vertices > limits.max_vertices {
                                    return Err(ObjError::Limit(format!(
                                        "more than {} vertices",
                                        limits.max_vertices
                                    )));
                                }
                                vertex
                            }
                        };
                        builder.primitive.indices.push(vertex);
                    }
                    total_indices += 3;
                    if total_indices > limits.max_indices {
                        return Err(ObjError::Limit(format!(
                            "more than {} triangle indices",
                            limits.max_indices
                        )));
                    }
                }
            }
            _ => {}
        }
    }

    let mut scene = GlbScene {
        generator: "Regia Engine OBJ import".into(),
        ..GlbScene::default()
    };
    let mut material_index: HashMap<String, usize> = HashMap::new();
    let mut images: HashMap<String, usize> = HashMap::new();
    let mut texture = |name: &Option<String>, report: &mut ObjImportReport, scene: &mut GlbScene| {
        let name = name.as_ref()?;
        let Some(found) = base_dir.and_then(|dir| inside(dir, name)) else {
            report
                .warnings
                .push(format!("texture {name} not found beside the OBJ; slot dropped"));
            return None;
        };
        if let Some(&image) = images.get(&found) {
            return Some(GlbTextureRef { image, tex_coord: 0 });
        }
        let ext = found.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        let image = if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
            GlbImage::Uri(encode_uri_path(&found))
        } else {
            // TGA, BMP …: re-encode as PNG (the glTF importer sniffs PNG and JPEG).
            let decoded = base_dir.and_then(|dir| image::open(dir.join(&found)).ok());
            let Some(decoded) = decoded else {
                report
                    .warnings
                    .push(format!("texture {name} could not be decoded; slot dropped"));
                return None;
            };
            let mut png = std::io::Cursor::new(Vec::new());
            decoded.write_to(&mut png, image::ImageFormat::Png).ok()?;
            GlbImage::Embedded {
                bytes: png.into_inner(),
                mime: "image/png",
            }
        };
        scene.images.push(image);
        let index = scene.images.len() - 1;
        images.insert(found.clone(), index);
        report.textures.push(found);
        Some(GlbTextureRef {
            image: index,
            tex_coord: 0,
        })
    };
    for material in &materials {
        let roughness = material
            .roughness
            .or_else(|| material.shininess.map(|ns| (2.0 / (ns.max(0.0) + 2.0)).sqrt()))
            .unwrap_or(1.0);
        let diffuse = material.diffuse.unwrap_or([0.8; 3]);
        let opacity = material.opacity.unwrap_or(1.0);
        let converted = GlbMaterial {
            name: material.name.clone(),
            base_color: [diffuse[0], diffuse[1], diffuse[2], opacity],
            metallic: material.metallic.unwrap_or(0.0),
            roughness,
            emissive: material.emissive.unwrap_or([0.0; 3]),
            base_color_texture: texture(&material.diffuse_map, &mut report, &mut scene),
            metallic_roughness_texture: None,
            normal_texture: texture(&material.normal_map, &mut report, &mut scene),
            emissive_texture: texture(&material.emissive_map, &mut report, &mut scene),
            alpha_blend: opacity < 1.0,
            double_sided: false,
        };
        material_index.insert(material.name.clone(), scene.materials.len());
        scene.materials.push(converted);
    }
    report.materials = scene.materials.len();

    for (name, primitives) in objects {
        let mut mesh = GlbMesh {
            name: name.clone(),
            ..GlbMesh::default()
        };
        for (material, builder) in primitives {
            let mut primitive = builder.primitive;
            let count = primitive.positions.len();
            // An attribute some corners lack is dropped for the whole primitive.
            if primitive.normals.len() != count {
                primitive.normals.clear();
            }
            if primitive.uvs.first().is_some_and(|uvs| uvs.len() != count) {
                primitive.uvs.clear();
            }
            if primitive.colors.len() != count {
                primitive.colors.clear();
            }
            primitive.material = match &material {
                Some(m) => {
                    let found = material_index.get(m).copied();
                    if found.is_none() {
                        report.warnings.push(format!("usemtl {m}: no such material"));
                    }
                    found
                }
                None => None,
            };
            if !primitive.indices.is_empty() {
                report.vertices += count;
                report.triangles += primitive.indices.len() / 3;
                mesh.primitives.push(primitive);
            }
        }
        if mesh.primitives.is_empty() {
            continue;
        }
        scene.meshes.push(mesh);
        scene.nodes.push(GlbNode {
            name,
            mesh: Some(scene.meshes.len() - 1),
            ..GlbNode::default()
        });
        scene.roots.push(scene.nodes.len() - 1);
    }
    report.objects = scene.nodes.len();
    if scene.meshes.is_empty() {
        return Err(ObjError::NoMesh);
    }
    for warning in &report.warnings {
        tracing::warn!(target: "regia_asset_pipeline", "OBJ import: {warning}");
    }
    // The OBJ builder replaces non-finite components, so this cannot fire for well-formed
    // input; a failure here means the sanitizer missed a path, and the message says so.
    let glb = scene.to_glb().map_err(|e| ObjError::Parse {
        line: 0,
        message: e.to_string(),
    })?;
    Ok(ObjConversion { glb, report })
}

/// An OBJ file imported through the glTF importer.
#[derive(Clone, Debug)]
pub struct ObjImportResult {
    /// The glTF importer's result for the converted scene.
    pub gltf: GltfImportResult,
    /// The converted GLB.
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: ObjImportReport,
}

/// Imports an `.obj` from disk through the glTF importer, with the `.meta` settings string.
pub fn import_obj_path(
    source: &Path,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
) -> Result<ObjImportResult, ImportError> {
    let settings = ObjImportSettings::try_from_meta_string(meta_settings)?;
    let bytes = read_obj_file(source, &settings.limits)?;
    let conversion = convert_obj_to_glb(&bytes, source.parent(), &settings)?;
    let lod = LodThresholdInputs::resolve(&[], "", None).0;
    let gltf = import_gltf_converted(
        &conversion.glb,
        source.parent(),
        parent_id,
        forward,
        platform,
        meta_settings,
        texture_quality,
        &lod,
    )?;
    Ok(ObjImportResult {
        gltf,
        glb: conversion.glb,
        report: conversion.report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_resolve_forward_and_backward() {
        assert_eq!(resolve("1", 3, 1, "v"), Ok(0));
        assert_eq!(resolve("-1", 3, 1, "v"), Ok(2));
        assert_eq!(resolve("-3", 3, 1, "v"), Ok(0));
        for bad in ["0", "4", "-4", "x", "", "99999999999999999999"] {
            assert_eq!(
                resolve(bad, 3, 7, "v").map_err(|e| e.code()),
                Err("obj_parse"),
                "{bad}"
            );
        }
    }

    /// audit-v034b-formats L10: a side file (`.mtl`, texture) beside the OBJ was checked by name
    /// only, so a symlink in a project's asset folder pulled a file from outside the project into
    /// the cook. Links that stay inside the project still work.
    #[cfg(unix)]
    #[test]
    fn a_side_file_linked_out_of_the_project_is_not_found() {
        let root = regia_test_utils::unique_temp_dir("obj_side_link");
        let project = root.join("game");
        let assets = project.join("assets");
        std::fs::create_dir_all(&assets).expect("dirs");
        std::fs::write(project.join(regia_core::brand::MANIFEST_FILE), "").expect("manifest");
        std::fs::write(root.join("secret.png"), b"outside").expect("outside file");
        std::fs::write(assets.join("real.png"), b"inside").expect("inside file");
        std::os::unix::fs::symlink(root.join("secret.png"), assets.join("tex.png")).expect("link out");
        std::os::unix::fs::symlink(assets.join("real.png"), assets.join("alias.png")).expect("link in");
        assert_eq!(inside(&assets, "real.png").as_deref(), Some("real.png"));
        assert_eq!(inside(&assets, "alias.png").as_deref(), Some("alias.png"));
        assert_eq!(
            inside(&assets, "tex.png"),
            None,
            "the link leads out of the project"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mtl_statements_parse() {
        let mut out = Vec::new();
        parse_mtl(
            "newmtl red\nKd 1 0 0\nd 0.5\nNs 98\nmap_Kd -s 1 1 1 tex/red.png\nnewmtl blue\nKd 0 0 1\nTr 0.25\n",
            &mut out,
            &ObjLimits::default(),
        )
        .expect("parse");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].diffuse, Some([1.0, 0.0, 0.0]));
        assert_eq!(out[0].opacity, Some(0.5));
        assert_eq!(out[0].diffuse_map.as_deref(), Some("tex/red.png"));
        assert_eq!(out[1].opacity, Some(0.75));
        assert!(parse_mtl("newmtl x\nKd a b c\n", &mut out, &ObjLimits::default()).is_err());
    }

    #[test]
    fn error_codes_are_protocol_shaped() {
        let errors = [
            ObjError::Io(String::new()),
            ObjError::TooLarge { size: 0, limit: 0 },
            ObjError::Parse {
                line: 1,
                message: String::new(),
            },
            ObjError::Limit(String::new()),
            ObjError::Settings(String::new()),
            ObjError::NoMesh,
        ];
        for error in errors {
            let code = error.code();
            assert!(code.starts_with("obj_"), "{code}");
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
    }
}
