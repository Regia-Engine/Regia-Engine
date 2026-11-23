// SPDX-License-Identifier: MIT-0
//! The stress scene's geometry as a cooked Regia Engine project (`vis_stress` bin).
//!
//! The scene's meshes go through the engine's own pipeline: the procedural rock variants are
//! written as glTF, DO27's hero buildings are copied without their images (geometry only; the
//! frame still textures them from the drop), every source gets `.meta` settings
//! `(meshlets: true)`, and `regia cook` (a subprocess: the sample links no importer code,
//! `docs/06` § 7) writes the cooked meshes, the `.fmlt` meshlet sub-assets and the pak. The tool
//! then reads meshes and meshlets back from the pak with the runtime readers only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_asset::{AssetManifest, CookedMesh, MetaSidecar, PakReader, decode_cooked_file, meta_path_for};
use regia_render::visbuffer::{MESHLET_TYPE_PATH, cooked_mesh_lod0, meshlet_sub_path};
use regia_render::{LabVertex, MeshletMesh};

use crate::LookdevError;

/// One cooked mesh primitive as the runtime draws it, with its cooked clusters.
#[derive(Clone, Debug)]
pub struct CookedPrimitive {
    /// LOD 0 vertices (decoded cooked streams).
    pub vertices: Vec<LabVertex>,
    /// LOD 0 indices.
    pub indices: Vec<u32>,
    /// The `.fmlt` sub-asset cooked for it, when present.
    pub meshlets: Option<Arc<MeshletMesh>>,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LookdevError + '_ {
    move |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Writes `bytes` unless the file already holds them (keeps mtimes and cook caches quiet).
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), LookdevError> {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    std::fs::write(path, bytes).map_err(io(path))
}

/// Gives `source` a `.meta` asking the cook for meshlets (an existing one keeps its id).
fn ensure_meta(source: &Path) -> Result<(), LookdevError> {
    let path = meta_path_for(source);
    let mut meta = match std::fs::read(&path) {
        Ok(bytes) => MetaSidecar::from_ron(&bytes).map_err(|e| LookdevError::Config(e.to_string()))?,
        Err(_) => MetaSidecar::new("gltf", regia_asset::AssetId::new_random()),
    };
    if meta.settings.as_deref() == Some("(meshlets: true)") {
        return Ok(());
    }
    meta.settings = Some("(meshlets: true)".into());
    let ron = meta
        .to_ron_string()
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    write_if_changed(&path, ron.as_bytes())
}

/// A one-node, one-primitive glTF 2.0 (JSON + `.bin`) of `vertices` / `indices`, no material.
fn write_gltf(path: &Path, name: &str, vertices: &[LabVertex], indices: &[u32]) -> Result<(), LookdevError> {
    let mut bin: Vec<u8> = Vec::new();
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    for v in vertices {
        for (i, c) in v.position.iter().enumerate() {
            lo[i] = lo[i].min(*c);
            hi[i] = hi[i].max(*c);
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    let normals_at = bin.len();
    for v in vertices {
        for c in v.normal {
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    let uvs_at = bin.len();
    for v in vertices {
        for c in v.uv {
            bin.extend_from_slice(&c.to_le_bytes());
        }
    }
    let indices_at = bin.len();
    for i in indices {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let bin_name = format!("{name}.bin");
    let n = vertices.len();
    let view = |offset: usize, length: usize, target: u32| serde_json::json!({ "buffer": 0, "byteOffset": offset, "byteLength": length, "target": target });
    let doc = serde_json::json!({
        "asset": { "version": "2.0", "generator": "regia lookdev vis_stress" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "name": name, "mesh": 0 }],
        "meshes": [{ "primitives": [{
            "attributes": { "POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2 },
            "indices": 3
        }] }],
        "buffers": [{ "uri": bin_name, "byteLength": bin.len() }],
        "bufferViews": [
            view(0, n * 12, 34962),
            view(normals_at, n * 12, 34962),
            view(uvs_at, n * 8, 34962),
            view(indices_at, indices.len() * 4, 34963),
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": n, "type": "VEC3", "min": lo, "max": hi },
            { "bufferView": 1, "componentType": 5126, "count": n, "type": "VEC3" },
            { "bufferView": 2, "componentType": 5126, "count": n, "type": "VEC2" },
            { "bufferView": 3, "componentType": 5125, "count": indices.len(), "type": "SCALAR" }
        ]
    });
    let json = serde_json::to_vec_pretty(&doc).map_err(|e| LookdevError::Json(e.to_string()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    write_if_changed(&dir.join(&bin_name), &bin)?;
    write_if_changed(path, &json)
}

/// Copies a glTF's geometry (JSON without images, textures, samplers and materials, plus its
/// buffers) into `dir`. Returns the copy's path.
fn copy_geometry(source: &Path, dir: &Path) -> Result<PathBuf, LookdevError> {
    let bytes = std::fs::read(source).map_err(io(source))?;
    let mut doc: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| LookdevError::Json(format!("{}: {e}", source.display())))?;
    let src_dir = source.parent().unwrap_or(Path::new("."));
    if let Some(buffers) = doc["buffers"].as_array() {
        for b in buffers {
            if let Some(uri) = b["uri"].as_str() {
                let from = src_dir.join(uri);
                let data = std::fs::read(&from).map_err(io(&from))?;
                write_if_changed(&dir.join(uri), &data)?;
            }
        }
    }
    if let Some(obj) = doc.as_object_mut() {
        for key in ["images", "textures", "samplers", "materials"] {
            obj.remove(key);
        }
        if let Some(used) = obj.get_mut("extensionsUsed").and_then(|v| v.as_array_mut()) {
            used.retain(|e| {
                !e.as_str()
                    .is_some_and(|s| s.starts_with("KHR_materials") || s.starts_with("KHR_texture"))
            });
        }
        obj.remove("extensionsRequired");
    }
    if let Some(meshes) = doc["meshes"].as_array_mut() {
        for mesh in meshes {
            if let Some(prims) = mesh["primitives"].as_array_mut() {
                for prim in prims {
                    if let Some(p) = prim.as_object_mut() {
                        p.remove("material");
                    }
                }
            }
        }
    }
    let name = source
        .file_name()
        .map_or_else(|| "hero.gltf".into(), |n| n.to_string_lossy().into_owned());
    let out = dir.join(name);
    let json = serde_json::to_vec_pretty(&doc).map_err(|e| LookdevError::Json(e.to_string()))?;
    write_if_changed(&out, &json)?;
    Ok(out)
}

/// The project-relative source path of rock variant `v`.
#[must_use]
pub fn rock_source(v: usize) -> String {
    format!("assets/rocks/rock_{v}.gltf")
}

/// The project-relative source path of hero file `i` (`file` is its name in the drop).
#[must_use]
pub fn hero_source(i: usize, file: &Path) -> String {
    let name = file
        .file_name()
        .map_or_else(|| "hero.gltf".into(), |n| n.to_string_lossy().into_owned());
    format!("assets/heroes/{i:02}/{name}")
}

/// Writes the project under `dir`: one glTF per rock variant and a geometry copy of every hero
/// file, each with `(meshlets: true)`.
///
/// # Errors
///
/// A file failed to read or write.
pub fn prepare_project(
    dir: &Path,
    rocks: &[(Vec<LabVertex>, Vec<u32>)],
    heroes: &[PathBuf],
) -> Result<(), LookdevError> {
    for (v, (vertices, indices)) in rocks.iter().enumerate() {
        let path = dir.join(rock_source(v));
        write_gltf(&path, &format!("rock_{v}"), vertices, indices)?;
        ensure_meta(&path)?;
    }
    for (i, file) in heroes.iter().enumerate() {
        let target = dir.join(hero_source(i, file));
        let parent = target.parent().unwrap_or(dir);
        let copy = copy_geometry(file, parent)?;
        ensure_meta(&copy)?;
    }
    Ok(())
}

/// The `regia` CLI: `REGIA_CLI` when that file exists, otherwise the copy for this
/// executable's cargo target dir and profile.
///
/// When neither is present, builds `regia` once (`cargo build -p regia_cli --bin regia`)
/// into that directory. `None` when the override is missing or that build fails; [`run_cook`]
/// returns the reason, which names the binary and the build command.
#[must_use]
pub fn regia_cli() -> Option<PathBuf> {
    crate::workspace_bin::ensure_regia_cli().ok()
}

/// Runs `regia cook --project dir` (cooks meshes and meshlets, writes the pak).
///
/// # Errors
///
/// The CLI is missing and the on-demand build failed (the error names the binary and
/// `cargo build -p regia_cli --bin regia`), or the cook failed.
pub fn run_cook(dir: &Path) -> Result<(), LookdevError> {
    let cli = crate::workspace_bin::ensure_regia_cli().map_err(LookdevError::Config)?;
    let status = std::process::Command::new(&cli)
        .args(["cook", "--project"])
        .arg(dir)
        .status()
        .map_err(io(&cli))?;
    if status.success() {
        Ok(())
    } else {
        Err(LookdevError::Config(format!("regia cook exited {status}")))
    }
}

/// Reads every cooked mesh primitive of the project's pak (`build/paks/<platform>/base.fpak`)
/// and its meshlets, keyed by manifest path (`assets/...gltf#mesh/<node>/primitive_<i>`).
///
/// # Errors
///
/// The pak or an entry failed to read or decode.
pub fn load_cooked(dir: &Path) -> Result<HashMap<String, CookedPrimitive>, LookdevError> {
    let platform = if cfg!(windows) { "windows" } else { "linux" };
    let pak_path = dir.join("build").join("paks").join(platform).join("base.fpak");
    let pak = PakReader::open(&pak_path)
        .map_err(|e| LookdevError::Config(format!("{}: {e}", pak_path.display())))?;
    let bad = |what: &str, e: String| LookdevError::Config(format!("{what}: {e}"));
    let read = |id| -> Result<Vec<u8>, LookdevError> {
        pak.read_decompressed(id)
            .map_err(|e| bad("pak entry", e.to_string()))
    };
    let manifest = AssetManifest {
        entries: pak.manifest().to_vec(),
    };
    let by_path: HashMap<&str, _> = manifest.entries.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut out = HashMap::new();
    for entry in &manifest.entries {
        if entry.type_path != "regia_asset::CookedMesh" {
            continue;
        }
        let Some((source, sub)) = entry.path.split_once('#') else {
            continue;
        };
        let bytes = read(entry.id)?;
        let (header, payload) = decode_cooked_file(&bytes).map_err(|e| bad(&entry.path, e.to_string()))?;
        // Desktop paks store meshes GPU-ready (`regia_asset::gpu_mesh`): same vertex order, LOD 0
        // indices, MikkTSpace tangents included.
        let (vertices, indices) = if header.type_tag == regia_asset::AssetTypeTag::gpu_mesh().0 {
            let mesh = regia_asset::GpuMeshView::parse(payload)
                .and_then(|view| regia_render::cooked_model::lab_mesh_from_gpu(&view))
                .map_err(|e| bad(&entry.path, e.to_string()))?;
            (mesh.vertices, mesh.indices)
        } else {
            let mesh = CookedMesh::from_postcard(payload).map_err(|e| bad(&entry.path, e.to_string()))?;
            let cpu = cooked_mesh_lod0(&mesh).map_err(|e| bad(&entry.path, e))?;
            let vertices = (0..cpu.positions.len())
                .map(|i| {
                    let mut v = LabVertex::from_pos_normal_uv(
                        cpu.positions[i],
                        cpu.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]),
                        cpu.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                    );
                    if let Some(t) = cpu.tangents.get(i) {
                        v.tangent = *t;
                    }
                    v
                })
                .collect();
            (vertices, cpu.indices)
        };
        let meshlets = match meshlet_sub_path(sub)
            .and_then(|p| by_path.get(format!("{source}#{p}").as_str()).copied())
        {
            Some(m) if m.type_path == MESHLET_TYPE_PATH => {
                let bytes = read(m.id)?;
                Some(Arc::new(
                    MeshletMesh::from_cooked_file(&bytes).map_err(|e| bad(&m.path, e.to_string()))?,
                ))
            }
            _ => None,
        };
        out.insert(
            entry.path.clone(),
            CookedPrimitive {
                vertices,
                indices,
                meshlets,
            },
        );
    }
    Ok(out)
}

/// Material index (glTF `materials`) of every mesh primitive of `gltf`, keyed by the cook's
/// sub-asset path `mesh/<node>/primitive_<i>` (node names sanitised as the importer does).
///
/// # Errors
///
/// The file does not read or parse.
pub fn primitive_materials(gltf: &Path) -> Result<HashMap<String, usize>, LookdevError> {
    let bytes = std::fs::read(gltf).map_err(io(gltf))?;
    let doc: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| LookdevError::Json(format!("{}: {e}", gltf.display())))?;
    let mut out = HashMap::new();
    let empty = Vec::new();
    let meshes = doc["meshes"].as_array().unwrap_or(&empty);
    for node in doc["nodes"].as_array().unwrap_or(&empty) {
        let Some(mesh) = node["mesh"].as_u64().and_then(|m| meshes.get(m as usize)) else {
            continue;
        };
        let name = node["name"]
            .as_str()
            .unwrap_or("Unnamed")
            .replace(['/', '\\', '#'], "_");
        for (p, prim) in mesh["primitives"].as_array().unwrap_or(&empty).iter().enumerate() {
            if let Some(material) = prim["material"].as_u64() {
                out.insert(format!("mesh/{name}/primitive_{p}"), material as usize);
            }
        }
    }
    Ok(out)
}
