// SPDX-License-Identifier: MIT-0
//! Hero-asset look-dev (`hero_r1`): DO27's hero buildings and vehicles on DO27's own ground.
//!
//! Each asset (`<content>/hero/buildings/*`, `<content>/hero/vehicles/x12`) is read from its
//! glTF files (LOD0 nodes only, node transforms applied), placed on the flattest ground near
//! the map's plate cameras, and framed three ways: a wide establishing shot, a three-quarter
//! mid shot, and a close-up 1.5 m from a surface the mid shot sees (panels, fasteners, glass).
//! The sun is the map's plate sun; the camera stands three-quarters off it.
//!
//! glTF materials map onto [`LabMaterial`] as the specification reads (glTF 2.0 § 3.9 and the
//! `KHR_materials_transmission`, `_ior`, `_emissive_strength` and `_volume` extensions), the same
//! mapping the engine's cook writes into `.material.ron` v3 and `LabMaterial::from_authored_resolved`
//! reads back (test `hero_cooked`):
//!
//! - base colour factor × texture (sRGB, decoded once by the GPU);
//! - metallic / roughness factors × the B / G channels of `metallicRoughnessTexture`, with
//!   `occlusionTexture` R as occlusion: one packed ORM image (DO27 points both slots at the
//!   same file) or, when they differ, packed here;
//! - normal texture with its `scale` (tangents from UV gradients, glTF's +Y-up convention);
//! - emissive factor × `emissiveStrength` × emissive texture, in nits;
//! - `alphaMode` / `alphaCutoff`; transmission draws premultiplied;
//! - hard-surface multi-scatter and specular occlusion on (ADR-043).
//!
//! The DO27 content frame is left-handed and the harness mirrors the finished image once
//! (see [`crate::do27`]); hero glTFs are right-handed, so their local X is mirrored here and
//! the image mirror puts them back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_render::{
    ALPHA_BLEND, ALPHA_MASK, ALPHA_OPAQUE, ALPHA_PREMULTIPLIED, HardSurfaceFactors, LabMaterial, LabMesh,
    LabTexture, LabVertex,
};

use crate::{LookdevError, Planet};

/// One hero asset: its glTF parts and the map it is shown on.
#[derive(Debug, Clone)]
pub struct HeroAsset {
    /// Short name (`barracks_rem`, `x12`, ...).
    pub name: String,
    /// glTF files, all drawn together.
    pub files: Vec<PathBuf>,
    /// Map whose ground, sun and look frame it.
    pub planet: Planet,
}

/// The hero assets in a content drop, sorted by name. Rem variants stand on the
/// Switchbacks, A52 variants on Bore Twelve, the X12 on the Switchbacks.
#[must_use]
pub fn catalog(content: &Path) -> Vec<HeroAsset> {
    let mut groups: HashMap<String, (Vec<PathBuf>, Planet)> = HashMap::new();
    let buildings = content.join("hero/buildings");
    if let Ok(dirs) = std::fs::read_dir(&buildings) {
        for dir in dirs.flatten() {
            for file in gltf_files(&dir.path()) {
                let stem = file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                let a52 = stem.to_ascii_lowercase().contains("a52");
                let base = dir
                    .file_name()
                    .to_string_lossy()
                    .trim_end_matches("_a52")
                    .to_string();
                let name = format!("{base}_{}", if a52 { "a52" } else { "rem" });
                let planet = if a52 { Planet::Europa } else { Planet::Mars };
                groups
                    .entry(name)
                    .or_insert_with(|| (Vec::new(), planet))
                    .0
                    .push(file);
            }
        }
    }
    let x12 = content.join("hero/vehicles/x12");
    let parts: Vec<PathBuf> = gltf_files(&x12)
        .into_iter()
        // `Hull_00b` is a second export of `Hull_00` (same nodes and buffer).
        .filter(|p| !p.to_string_lossy().ends_with("_00b.gltf"))
        .collect();
    if !parts.is_empty() {
        groups.insert("x12".into(), (parts, Planet::Mars));
    }
    // The X-11 as the game ships it (`vehicles/x11`: hull, glass, cockpit and crew; not the
    // `wreck/` set or the cargo box), on both maps: its canopy reflects a dusty sky on Mars and space on Europa.
    let x11: Vec<PathBuf> = std::fs::read_dir(content.join("vehicles/x11"))
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "gltf"))
                // `SM_X11_Interior.gltf` is the X-12's rear cargo box (DO27's hero-ref note);
                // the X-11's cockpit is `SM_X11_Interior_00.gltf`.
                .filter(|p| !p.ends_with("SM_X11_Interior.gltf"))
                .collect()
        })
        .unwrap_or_default();
    if !x11.is_empty() {
        groups.insert("x11".into(), (x11.clone(), Planet::Mars));
        groups.insert("x11_europa".into(), (x11, Planet::Europa));
    }
    // The Remora as DO27's shell loads it (`SM_Remora*.gltf`): not the `_00` re-exports
    // (`HullU0_00`, `Interior_00`, …, a second copy of the same hull) or the fastener sheet.
    let remora: Vec<PathBuf> = std::fs::read_dir(content.join("vehicles/remora"))
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "gltf"))
                .filter(|p| {
                    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
                    !stem.ends_with("_00") && !stem.contains("ZFasteners")
                })
                .collect()
        })
        .unwrap_or_default();
    if !remora.is_empty() {
        groups.insert("remora".into(), (remora.clone(), Planet::Mars));
        groups.insert("remora_europa".into(), (remora, Planet::Europa));
    }
    let mut out: Vec<HeroAsset> = groups
        .into_iter()
        .map(|(name, (mut files, planet))| {
            files.sort();
            HeroAsset { name, files, planet }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `*.gltf` under `dir` (one level of subdirectories, e.g. `deployed/`).
fn gltf_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Ok(sub) = std::fs::read_dir(&p) {
                out.extend(
                    sub.flatten()
                        .map(|s| s.path())
                        .filter(|s| s.extension().is_some_and(|x| x == "gltf")),
                );
            }
        } else if p.extension().is_some_and(|x| x == "gltf") {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// A loaded asset in its own frame (metres, +Y up, mirrored for the content frame).
pub struct HeroModel {
    /// Meshes (material indices into [`Self::materials`]).
    pub meshes: Vec<LabMesh>,
    /// Materials.
    pub materials: Vec<LabMaterial>,
    /// Bounds minimum.
    pub min: Vec3,
    /// Bounds maximum.
    pub max: Vec3,
}

fn bad(path: &Path, what: &str) -> LookdevError {
    LookdevError::Config(format!("{}: {what}", path.display()))
}

fn read_json(path: &Path) -> Result<serde_json::Value, LookdevError> {
    let text = std::fs::read_to_string(path).map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|e| bad(path, &e.to_string()))
}

/// Loads every file of `asset` (LOD0 meshes, textures at most `max_texture` texels a side).
///
/// # Errors
///
/// A file, buffer or image is missing or malformed.
pub fn load(asset: &HeroAsset, max_texture: u32) -> Result<HeroModel, LookdevError> {
    load_files(&asset.name, &asset.files, max_texture, true)
}

/// Loads `files` as one model (LOD0 meshes, textures at most `max_texture` texels a side).
/// `mirror` mirrors local X into the DO27 content frame (as [`load`] does); without it the
/// model keeps glTF's right-handed frame, which is what a reference render made from the same
/// glTF (Blender, Cycles) shows.
///
/// # Errors
///
/// A file, buffer or image is missing or malformed.
pub fn load_files(
    name: &str,
    files: &[PathBuf],
    max_texture: u32,
    mirror: bool,
) -> Result<HeroModel, LookdevError> {
    load_files_lod(name, files, max_texture, mirror, false)
}

/// [`load_files`] for a lower-LOD export (`*_LOD1.gltf`, `*_LOD2.gltf`): its `_LOD1`/`_LOD2`
/// nodes are drawn instead of skipped (sockets still are).
///
/// # Errors
///
/// A file, buffer or image is missing or malformed.
pub fn load_lod_files(
    name: &str,
    files: &[PathBuf],
    max_texture: u32,
    mirror: bool,
) -> Result<HeroModel, LookdevError> {
    load_files_lod(name, files, max_texture, mirror, true)
}

fn load_files_lod(
    name: &str,
    files: &[PathBuf],
    max_texture: u32,
    mirror: bool,
    any_lod: bool,
) -> Result<HeroModel, LookdevError> {
    let mut model = HeroModel {
        meshes: Vec::new(),
        materials: Vec::new(),
        min: Vec3::splat(f32::MAX),
        max: Vec3::splat(f32::MIN),
    };
    for file in files {
        load_file(file, max_texture, mirror, any_lod, &mut model)?;
    }
    for mesh in &model.meshes {
        for v in &mesh.vertices {
            let p = Vec3::from_array(v.position);
            model.min = model.min.min(p);
            model.max = model.max.max(p);
        }
    }
    if model.meshes.is_empty() {
        return Err(LookdevError::Config(format!("{name}: no LOD0 meshes")));
    }
    Ok(model)
}

fn node_matrix(node: &serde_json::Value) -> Mat4 {
    let f = |v: &serde_json::Value, i: usize, d: f32| {
        v.get(i)
            .and_then(serde_json::Value::as_f64)
            .map_or(d, |x| x as f32)
    };
    if let Some(m) = node["matrix"].as_array() {
        let mut cols = [0.0f32; 16];
        for (i, c) in cols.iter_mut().enumerate() {
            *c = m.get(i).and_then(serde_json::Value::as_f64).unwrap_or(0.0) as f32;
        }
        return Mat4::from_cols_array(&cols);
    }
    let t = &node["translation"];
    let r = &node["rotation"];
    let s = &node["scale"];
    Mat4::from_scale_rotation_translation(
        Vec3::new(f(s, 0, 1.0), f(s, 1, 1.0), f(s, 2, 1.0)),
        Quat::from_xyzw(f(r, 0, 0.0), f(r, 1, 0.0), f(r, 2, 0.0), f(r, 3, 1.0)).normalize(),
        Vec3::new(f(t, 0, 0.0), f(t, 1, 0.0), f(t, 2, 0.0)),
    )
}

/// LOD1+ (unless `any_lod`) and sockets are skipped; any other mesh node is drawn.
fn wanted(name: &str, any_lod: bool) -> bool {
    let n = name.to_ascii_lowercase();
    let lower_lod = n.contains("_lod1") || n.contains("_lod2") || n.contains("_lod3");
    !((lower_lod && !any_lod) || n.starts_with("socket"))
}

#[allow(clippy::too_many_lines)] // one glTF file: buffers, images, materials, the node walk
fn load_file(
    path: &Path,
    max_texture: u32,
    mirror: bool,
    any_lod: bool,
    model: &mut HeroModel,
) -> Result<(), LookdevError> {
    let doc = read_json(path)?;
    let base = path.parent().unwrap_or(Path::new("."));
    let empty = Vec::new();
    let list = |key: &str| doc[key].as_array().unwrap_or(&empty);
    let buffers: Vec<Vec<u8>> = list("buffers")
        .iter()
        .map(|b| {
            let uri = b["uri"]
                .as_str()
                .ok_or_else(|| bad(path, "embedded buffers are not supported"))?;
            let p = base.join(uri);
            std::fs::read(&p).map_err(|source| LookdevError::Io {
                path: p.display().to_string(),
                source,
            })
        })
        .collect::<Result<_, _>>()?;
    let images = list("images");
    let textures = list("textures");
    let mut cache: HashMap<usize, LabTexture> = HashMap::new();
    let image_of = |info: &serde_json::Value| -> Option<usize> {
        let tex = textures.get(info["index"].as_u64()? as usize)?;
        tex["source"].as_u64().map(|v| v as usize)
    };
    let load_image =
        |source: usize, cache: &mut HashMap<usize, LabTexture>| -> Result<Option<LabTexture>, LookdevError> {
            if let Some(t) = cache.get(&source) {
                return Ok(Some(t.clone()));
            }
            let Some(uri) = images.get(source).and_then(|i| i["uri"].as_str()) else {
                return Ok(None);
            };
            let p = base.join(uri);
            if !p.exists() {
                tracing::warn!(target: "lookdev", "missing image {}", p.display());
                return Ok(None);
            }
            let t = crate::do27::load_png_scaled(&p, max_texture)?;
            cache.insert(source, t.clone());
            Ok(Some(t))
        };
    let first_material = model.materials.len();
    for mat in list("materials") {
        let pbr = &mat["pbrMetallicRoughness"];
        let num = |v: &serde_json::Value, d: f32| v.as_f64().map_or(d, |x| x as f32);
        let vec = |v: &serde_json::Value, d: f32, n: usize| -> Vec<f32> {
            (0..n)
                .map(|i| {
                    v.get(i)
                        .and_then(serde_json::Value::as_f64)
                        .map_or(d, |x| x as f32)
                })
                .collect()
        };
        let c = vec(&pbr["baseColorFactor"], 1.0, 4);
        let e = vec(&mat["emissiveFactor"], 0.0, 3);
        let ext = &mat["extensions"];
        let albedo = match image_of(&pbr["baseColorTexture"]) {
            Some(i) => load_image(i, &mut cache)?,
            None => None,
        };
        let normal = match image_of(&mat["normalTexture"]) {
            Some(i) => load_image(i, &mut cache)?,
            None => None,
        };
        let emissive_map = match image_of(&mat["emissiveTexture"]) {
            Some(i) => load_image(i, &mut cache)?,
            None => None,
        };
        let occ_src = image_of(&mat["occlusionTexture"]);
        let mr_src = image_of(&pbr["metallicRoughnessTexture"]);
        let orm = match (occ_src, mr_src) {
            (Some(o), Some(m)) if o == m => load_image(o, &mut cache)?,
            (o, Some(m)) => {
                let mr = load_image(m, &mut cache)?;
                let occ = match o {
                    Some(o) => load_image(o, &mut cache)?,
                    None => None,
                };
                mr.map(|mr| pack_orm(occ.as_ref(), &mr))
            }
            (Some(o), None) => load_image(o, &mut cache)?.map(|occ| {
                // Occlusion alone: G and B white so the factors pass unchanged.
                let rgba = occ
                    .rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|p| [p[0], 255, 255, 255])
                    .collect();
                LabTexture { rgba, ..occ }
            }),
            (None, None) => None,
        };
        let transmission = num(&ext["KHR_materials_transmission"]["transmissionFactor"], 0.0);
        let alpha_mode = match mat["alphaMode"].as_str() {
            _ if transmission > 0.0 => ALPHA_PREMULTIPLIED,
            Some("BLEND") => ALPHA_BLEND,
            Some("MASK") => ALPHA_MASK,
            _ => ALPHA_OPAQUE,
        };
        model.materials.push(LabMaterial {
            base_color: [c[0], c[1], c[2], c[3]],
            metallic: num(&pbr["metallicFactor"], 1.0).max(0.0),
            roughness: num(&pbr["roughnessFactor"], 1.0).max(0.0),
            normal_scale: if normal.is_some() {
                num(&mat["normalTexture"]["scale"], 1.0)
            } else {
                0.0
            },
            occlusion_strength: num(&mat["occlusionTexture"]["strength"], 1.0),
            emissive: [e[0], e[1], e[2]],
            emissive_intensity: num(&ext["KHR_materials_emissive_strength"]["emissiveStrength"], 1.0),
            alpha_mode,
            alpha_cutoff: num(&mat["alphaCutoff"], 0.5),
            albedo,
            normal_map: normal,
            orm_packed: orm.is_some() && mr_src.is_some(),
            occlusion_map: orm,
            emissive_map,
            transmission,
            ior: num(&ext["KHR_materials_ior"]["ior"], 1.5),
            transmission_tint: {
                let vol = &ext["KHR_materials_volume"];
                let a = vec(&vol["attenuationColor"], 1.0, 3);
                // glTF's default distance is +inf (no absorption).
                let d = vol["attenuationDistance"].as_f64().map_or(0.0, |x| x as f32);
                regia_render::volume_transmittance(num(&vol["thicknessFactor"], 0.0), [a[0], a[1], a[2]], d)
            },
            // The cooked bridge carries the volume only with a transmission (as here).
            transmission_thickness: if transmission > 0.0 {
                num(&ext["KHR_materials_volume"]["thicknessFactor"], 0.0).max(0.0)
            } else {
                0.0
            },
            hard_surface: HardSurfaceFactors {
                multi_scatter: 1.0,
                specular_occlusion: 1.0,
                ..HardSurfaceFactors::OFF
            },
            // glTF 2.0 § 3.9.6: back faces are culled unless `doubleSided`.
            double_sided: mat["doubleSided"].as_bool().unwrap_or(false),
            ..LabMaterial::default()
        });
    }
    if list("materials").is_empty() {
        model.materials.push(LabMaterial::default());
    }
    // Diagnostics: HERO_NO_NORMAL / HERO_NO_ORM / HERO_NO_ALBEDO drop one channel;
    // HERO_FLIP_NORMAL_G inverts the normal map's green channel; HERO_NO_AO sets the occlusion
    // strength to 0; HERO_NO_SPECOCC turns the authored specular occlusion off; HERO_SINGLE_SIDED
    // drops glTF `doubleSided` (the cost A/B of the cull-none draws); HERO_NO_MS turns
    // multi-scatter off (single-scatter GGX, the reference for Cycles' `GGX` distribution).
    for m in &mut model.materials[first_material..] {
        if std::env::var_os("HERO_SINGLE_SIDED").is_some() {
            m.double_sided = false;
        }
        if std::env::var_os("HERO_NO_NORMAL").is_some() {
            m.normal_scale = 0.0;
            m.normal_map = None;
        }
        if std::env::var_os("HERO_NO_ORM").is_some() {
            m.occlusion_map = None;
            m.orm_packed = false;
        }
        if std::env::var_os("HERO_NO_ALBEDO").is_some() {
            m.albedo = None;
        }
        if std::env::var_os("HERO_NO_AO").is_some() {
            m.occlusion_strength = 0.0;
        }
        if std::env::var_os("HERO_NO_SPECOCC").is_some() {
            m.hard_surface.specular_occlusion = 0.0;
        }
        if std::env::var_os("HERO_NO_MS").is_some() {
            m.hard_surface.multi_scatter = 0.0;
        }
        if std::env::var_os("HERO_FLIP_NORMAL_G").is_some()
            && let Some(n) = &mut m.normal_map
        {
            for px in n.rgba.as_chunks_mut::<4>().0 {
                px[1] = 255 - px[1];
            }
        }
        // Glass isolation (lane glass-reflection): HERO_GLASS_ONE_SIDED culls glass back faces;
        // HERO_GLASS_OPAQUE draws glass as an opaque dielectric (as `regia_pt` does);
        // HERO_OPAQUE_BLACK makes every other surface unlit black, leaving the glass's own light.
        let glass = m.transmission > 0.0 || m.alpha_mode == ALPHA_BLEND;
        // HERO_GLASS_FLAT_NORMAL drops the glass's normal map (DO27's X-11 glass maps hold a
        // constant 55° tilt inside their UV islands; lane hull-look's content A/B).
        if glass && std::env::var_os("HERO_GLASS_FLAT_NORMAL").is_some() {
            m.normal_scale = 0.0;
            m.normal_map = None;
        }
        if glass && std::env::var_os("HERO_GLASS_ONE_SIDED").is_some() {
            m.double_sided = false;
        }
        if glass && std::env::var_os("HERO_GLASS_OPAQUE").is_some() {
            m.transmission = 0.0;
            m.alpha_mode = ALPHA_OPAQUE;
            m.base_color[3] = 1.0;
        }
        if !glass && std::env::var_os("HERO_OPAQUE_BLACK").is_some() {
            *m = LabMaterial {
                base_color: [0.0, 0.0, 0.0, 1.0],
                unlit: true,
                alpha_mode: m.alpha_mode,
                alpha_cutoff: m.alpha_cutoff,
                albedo: (m.alpha_mode == ALPHA_MASK).then(|| m.albedo.clone()).flatten(),
                double_sided: m.double_sided,
                ..LabMaterial::EMPTY
            };
        }
    }

    let accessors = list("accessors");
    let views = list("bufferViews");
    let accessor = |index: u64| -> Result<(&[u8], usize, usize, u64, usize), LookdevError> {
        let a = accessors
            .get(index as usize)
            .ok_or_else(|| bad(path, "accessor out of range"))?;
        let view = views
            .get(
                a["bufferView"]
                    .as_u64()
                    .ok_or_else(|| bad(path, "sparse accessors are not supported"))? as usize,
            )
            .ok_or_else(|| bad(path, "bufferView out of range"))?;
        let buffer = buffers
            .get(view["buffer"].as_u64().unwrap_or(0) as usize)
            .ok_or_else(|| bad(path, "buffer out of range"))?;
        let comps = match a["type"].as_str().unwrap_or("SCALAR") {
            "VEC2" => 2,
            "VEC3" => 3,
            "VEC4" => 4,
            _ => 1,
        };
        let ctype = a["componentType"].as_u64().unwrap_or(5126);
        let size = match ctype {
            5120 | 5121 => 1,
            5122 | 5123 => 2,
            _ => 4,
        };
        let stride = view["byteStride"].as_u64().map_or(comps * size, |v| v as usize);
        let start =
            (view["byteOffset"].as_u64().unwrap_or(0) + a["byteOffset"].as_u64().unwrap_or(0)) as usize;
        let count = a["count"].as_u64().unwrap_or(0) as usize;
        let end = start + stride * count.saturating_sub(1) + comps * size;
        let bytes = buffer
            .get(start..end.max(start))
            .ok_or_else(|| bad(path, "accessor past its buffer"))?;
        Ok((bytes, count, comps, ctype, stride))
    };
    let floats = |index: u64, want: usize| -> Result<Vec<f32>, LookdevError> {
        let (bytes, count, comps, ctype, stride) = accessor(index)?;
        if ctype != 5126 || comps != want {
            return Err(bad(path, "expected float vertex data"));
        }
        let mut out = Vec::with_capacity(count * comps);
        for i in 0..count {
            for c in 0..comps {
                let o = i * stride + c * 4;
                out.push(f32::from_le_bytes([
                    bytes[o],
                    bytes[o + 1],
                    bytes[o + 2],
                    bytes[o + 3],
                ]));
            }
        }
        Ok(out)
    };

    // Walk the scene graph from the roots with accumulated transforms.
    let nodes = list("nodes");
    let mut is_child = vec![false; nodes.len()];
    for n in nodes {
        for c in n["children"].as_array().unwrap_or(&empty) {
            if let Some(i) = c.as_u64().and_then(|i| is_child.get_mut(i as usize)) {
                *i = true;
            }
        }
    }
    let mut stack: Vec<(usize, Mat4)> = (0..nodes.len())
        .filter(|&i| !is_child[i])
        .map(|i| (i, Mat4::IDENTITY))
        .collect();
    // The content frame is the mirror image of the glTF frame (see the module docs).
    let mirror = if mirror {
        Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0))
    } else {
        Mat4::IDENTITY
    };
    let mesh_list = list("meshes");
    while let Some((index, parent)) = stack.pop() {
        let node = &nodes[index];
        let world = parent * node_matrix(node);
        for c in node["children"].as_array().unwrap_or(&empty) {
            if let Some(i) = c.as_u64() {
                stack.push((i as usize, world));
            }
        }
        let name = node["name"].as_str().unwrap_or_default();
        let Some(mesh) = node["mesh"].as_u64().and_then(|m| mesh_list.get(m as usize)) else {
            continue;
        };
        if !wanted(name, any_lod) {
            continue;
        }
        let to_world = mirror * world;
        let normal_m = to_world.inverse().transpose();
        let flips = to_world.determinant() < 0.0;
        for prim in mesh["primitives"].as_array().unwrap_or(&empty) {
            let attrs = &prim["attributes"];
            let Some(pos_index) = attrs["POSITION"].as_u64() else {
                continue;
            };
            let positions = floats(pos_index, 3)?;
            let n = positions.len() / 3;
            let normals = match attrs["NORMAL"].as_u64() {
                Some(i) => floats(i, 3)?,
                None => [0.0, 1.0, 0.0].repeat(n),
            };
            let uvs = match attrs["TEXCOORD_0"].as_u64() {
                Some(i) => floats(i, 2)?,
                None => vec![0.0; n * 2],
            };
            let mut indices: Vec<u32> = match prim["indices"].as_u64() {
                Some(i) => {
                    let (bytes, count, _, ctype, stride) = accessor(i)?;
                    (0..count)
                        .map(|k| {
                            let o = k * stride;
                            match ctype {
                                5121 => u32::from(bytes[o]),
                                5123 => u32::from(u16::from_le_bytes([bytes[o], bytes[o + 1]])),
                                _ => u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]),
                            }
                        })
                        .collect()
                }
                None => (0..n as u32).collect(),
            };
            if normals.len() != n * 3 || uvs.len() != n * 2 {
                return Err(bad(path, "attribute counts differ"));
            }
            if flips {
                for tri in indices.as_chunks_mut::<3>().0 {
                    tri.swap(1, 2);
                }
            }
            let mut vertices: Vec<LabVertex> = (0..n)
                .map(|k| {
                    let p = to_world.transform_point3(Vec3::new(
                        positions[k * 3],
                        positions[k * 3 + 1],
                        positions[k * 3 + 2],
                    ));
                    let nn = normal_m
                        .transform_vector3(Vec3::new(normals[k * 3], normals[k * 3 + 1], normals[k * 3 + 2]))
                        .normalize_or_zero();
                    LabVertex::from_pos_normal_uv(p.to_array(), nn.to_array(), [uvs[k * 2], uvs[k * 2 + 1]])
                })
                .collect();
            // Coincident duplicate triangles (DO27's cargo boxes carry every panel twice, the
            // copy with transposed UVs) z-fight into a moiré: keep the first copy, as the
            // cook does (`regia_render::drop_coincident_triangles`). HERO_KEEP_DUPLICATES
            // keeps them (diagnostic).
            if std::env::var_os("HERO_KEEP_DUPLICATES").is_none() {
                let corners: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
                regia_render::drop_coincident_triangles(&corners, &mut indices);
            }
            crate::do27::compute_tangents(&mut vertices, &indices);
            model.meshes.push(LabMesh {
                vertices,
                indices,
                material: first_material + prim["material"].as_u64().unwrap_or(0) as usize,
                ..LabMesh::unskinned()
            });
        }
    }
    Ok(())
}

/// ORM from separate occlusion (R) and metallic-roughness (G, B) images; the occlusion is
/// sampled at the metallic-roughness resolution (nearest).
fn pack_orm(occ: Option<&LabTexture>, mr: &LabTexture) -> LabTexture {
    let mut rgba = mr.rgba.clone();
    for y in 0..mr.height {
        for x in 0..mr.width {
            let i = ((y * mr.width + x) * 4) as usize;
            rgba[i] = occ.map_or(255, |o| {
                let ox = (x * o.width / mr.width.max(1)).min(o.width.saturating_sub(1));
                let oy = (y * o.height / mr.height.max(1)).min(o.height.saturating_sub(1));
                o.rgba[((oy * o.width + ox) * 4) as usize]
            });
            rgba[i + 3] = 255;
        }
    }
    LabTexture {
        width: mr.width,
        height: mr.height,
        rgba,
    }
}

/// Ray / triangle hit distance (Möller–Trumbore).
fn ray_triangle(o: Vec3, d: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let e1 = b - a;
    let e2 = c - a;
    let p = d.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-9 {
        return None;
    }
    let inv = 1.0 / det;
    let s = o - a;
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = d.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(q) * inv;
    (t > 1e-4).then_some(t)
}

/// First hit of the ray `o + t·d` on the placed meshes: point and geometric normal (facing
/// the ray's origin).
#[must_use]
pub fn raycast(meshes: &[LabMesh], o: Vec3, d: Vec3) -> Option<(Vec3, Vec3)> {
    let mut best: Option<(f32, Vec3)> = None;
    for m in meshes {
        for tri in m.indices.as_chunks::<3>().0 {
            let [a, b, c] =
                [tri[0], tri[1], tri[2]].map(|i| Vec3::from_array(m.vertices[i as usize].position));
            if let Some(t) = ray_triangle(o, d, a, b, c)
                && best.is_none_or(|(bt, _)| t < bt)
            {
                let mut n = (b - a).cross(c - a).normalize_or_zero();
                if n.dot(d) > 0.0 {
                    n = -n;
                }
                best = Some((t, n));
            }
        }
    }
    best.map(|(t, n)| (o + d * t, n))
}

/// The flattest sunlit ground within 240 m of `anchor` for an asset of footprint `radius` and
/// half height `half_height`: the centre at the highest ground under the footprint. `height`
/// samples the heightfield; a spot whose centre or footprint edges see a ridge toward the sun
/// (20 m steps over 1.5 km) is only taken when nothing else is left.
pub fn sunlit_spot(
    height: impl Fn(f32, f32) -> f32,
    anchor: Vec3,
    toward_sun: Vec3,
    radius: f32,
    half_height: f32,
) -> Vec3 {
    let mut best = (f32::MAX, anchor);
    for i in -16..=16 {
        for j in -16..=16 {
            let p = anchor + Vec3::new(i as f32 * 15.0, 0.0, j as f32 * 15.0);
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for k in 0..12 {
                let a = k as f32 * std::f32::consts::TAU / 12.0;
                for r in [0.0, 0.5, 1.0] {
                    let h = height(p.x + a.sin() * radius * r, p.z + a.cos() * radius * r);
                    lo = lo.min(h);
                    hi = hi.max(h);
                }
            }
            let shaded = [(0.0f32, 0.0f32), (1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)]
                .iter()
                .any(|&(dx, dz)| {
                    let start = Vec3::new(p.x + dx * radius, hi + half_height, p.z + dz * radius);
                    (1..75).any(|k| {
                        let q = start + toward_sun * (k as f32 * 20.0);
                        height(q.x, q.z) > q.y
                    })
                });
            let score = (hi - lo) + 0.002 * (p - anchor).length() + if shaded { 1.0e3 } else { 0.0 };
            if score < best.0 {
                best = (score, Vec3::new(p.x, hi, p.z));
            }
        }
    }
    best.1
}

/// Moves every mesh by `offset` and turns it `yaw` radians about +Y.
pub fn place(model: &mut HeroModel, yaw: f32, offset: Vec3) {
    let m = Mat4::from_translation(offset) * Mat4::from_rotation_y(yaw);
    for mesh in &mut model.meshes {
        for v in &mut mesh.vertices {
            v.position = m.transform_point3(Vec3::from_array(v.position)).to_array();
            v.normal = m.transform_vector3(Vec3::from_array(v.normal)).to_array();
            let t = m.transform_vector3(Vec3::new(v.tangent[0], v.tangent[1], v.tangent[2]));
            v.tangent = [t.x, t.y, t.z, v.tangent[3]];
        }
    }
    model.min = Vec3::splat(f32::MAX);
    model.max = Vec3::splat(f32::MIN);
    for mesh in &model.meshes {
        for v in &mesh.vertices {
            let p = Vec3::from_array(v.position);
            model.min = model.min.min(p);
            model.max = model.max.max(p);
        }
    }
}
