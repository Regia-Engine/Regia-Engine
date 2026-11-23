// SPDX-License-Identifier: MPL-2.0
//! Fracture data in the cook (ADR-1967): with `fracture: Some((...))` in a glTF's `.meta` settings,
//! `regia cook` runs the fracture tool on the mesh and writes what a game needs at runtime, which
//! cannot link this crate:
//!
//! - `fracture/chunk_<n>`, a [`regia_asset::CookedMesh`] per chunk: material slot 0 holds the pieces
//!   of the source surface, slot 1 the cut faces.
//! - `fracture/destructible`, the `.destructible.ron` text, typed
//!   [`regia_physics::destruction::DESTRUCTIBLE_TYPE_PATH`], whose chunks name their meshes in
//!   `lods` (`<source>#fracture/chunk_<n>`). The runtime reads it with
//!   [`regia_physics::destruction::DestructibleAsset::load`].
//!
//! Versioning (`specs/12` § 4a): the outputs are derived data, so the migration rule is a recook.
//! The cache key hashes the source's path (the outputs name it), the importer version (their
//! headers carry it), the mesh, the settings and [`FRACTURE_COOK_VERSION`]; an unchanged key skips
//! the fracture, which can take seconds.

use std::path::Path;

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, CookedIndexWidth, CookedMesh, CookedSubmesh, CpuMesh,
    ManifestEntry, SubAssetEntry, cooked_file_name, encode_cooked_file,
};
use regia_physics::destruction::{DESTRUCTIBLE_TYPE_PATH, DestructionMaterial};
use serde::{Deserialize, Serialize};

use crate::destructible_cook::{CookSettings, SupportRule, cook_destructible};
use crate::fracture::{ChunkRenderMesh, FractureSettings, FractureSource, fracture_source};
use crate::mesh_cook::pack_cooked_mesh;

/// Bumped whenever this cook's output changes for the same input.
pub const FRACTURE_COOK_VERSION: u32 = 1;

const MESH_TYPE_PATH: &str = "regia_asset::CookedMesh";

/// The sub-asset path of the runtime asset.
pub const FRACTURE_DESTRUCTIBLE_SUB_PATH: &str = "fracture/destructible";

/// The sub-asset path of chunk `index`'s mesh.
#[must_use]
pub fn fracture_chunk_sub_path(index: usize) -> String {
    format!("fracture/chunk_{index}")
}

/// The `fracture` block of a glTF's `.meta` settings. Omitted fields take these defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct FractureCookSettings {
    /// Chunks to make.
    pub chunks: usize,
    /// Seed of the site sampling.
    pub seed: u64,
    /// The mesh to fracture (`mesh/<node>/primitive_<i>`); `None` is the file's primary mesh.
    pub mesh: Option<String>,
    /// Newtons of joint break force per square metre of contact.
    pub strength_per_area: f32,
    /// Kilograms per cubic metre.
    pub density: f32,
    /// Seconds a joint's break force must act to break its bond.
    pub bond_seconds: f32,
    /// Chunks whose lowest point is at or below this height are held by the world; `None` is the
    /// lowest centimetre of the mesh.
    pub support_below_y: Option<f32>,
    /// No support chunks (a free object).
    pub free: bool,
    /// Sound/VFX tag of the material.
    pub fx_tag: String,
    /// UV units per metre on the cut faces.
    pub interior_uv_scale: f32,
}

impl Default for FractureCookSettings {
    fn default() -> Self {
        Self {
            chunks: 24,
            seed: 1,
            mesh: None,
            strength_per_area: 500_000.0,
            density: 2400.0,
            bond_seconds: 0.01,
            support_below_y: None,
            free: false,
            fx_tag: "default".to_string(),
            interior_uv_scale: 1.0,
        }
    }
}

/// The fracture tool's settings for a `fracture` block.
#[must_use]
pub fn fracture_settings(settings: &FractureCookSettings) -> FractureSettings {
    let mut fracture = FractureSettings::random(settings.chunks, settings.seed);
    fracture.joints.strength_per_area = settings.strength_per_area;
    fracture.render.interior_uv_scale = settings.interior_uv_scale;
    fracture
}

/// The destructible cook's settings for a `fracture` block.
#[must_use]
pub fn destructible_cook_settings(name: &str, settings: &FractureCookSettings) -> CookSettings {
    let mut cook = CookSettings::new(name);
    cook.density = settings.density;
    cook.bond_seconds = settings.bond_seconds;
    cook.support = if settings.free {
        SupportRule::None
    } else {
        SupportRule::BelowY(settings.support_below_y.unwrap_or(f32::NEG_INFINITY))
    };
    cook.material = DestructionMaterial {
        fx_tag: settings.fx_tag.clone(),
        ..DestructionMaterial::default()
    };
    cook.fracture_seed = Some(settings.seed);
    cook
}

/// Stores `fracture` in the settings of `source`'s `.meta` (`None` removes it) and keeps every
/// other setting. Returns the `.meta`'s path; a source without one gets one at the next cook, so
/// this creates it first.
///
/// # Errors
/// A message when the `.meta` does not parse, its settings are malformed (they stay as they
/// were) or a file cannot be written.
pub fn set_meta_fracture(
    source: &Path,
    fracture: Option<&FractureCookSettings>,
) -> Result<std::path::PathBuf, String> {
    let meta_path = regia_asset::meta_path_for(source);
    let mut meta = if meta_path.is_file() {
        let bytes = std::fs::read(&meta_path).map_err(|e| e.to_string())?;
        regia_asset::MetaSidecar::from_ron(&bytes).map_err(|e| e.to_string())?
    } else {
        regia_asset::MetaSidecar::new("gltf", AssetId::new_random())
    };
    let mut settings =
        crate::gltf_settings::GltfImporterSettings::try_from_meta_string(meta.settings.as_deref())
            .map_err(|e| format!("{}: settings: {e}", meta_path.display()))?;
    settings.fracture = fracture.cloned();
    meta.settings = Some(ron::to_string(&settings).map_err(|e| e.to_string())?);
    let text = meta.to_ron_string().map_err(|e| e.to_string())?;
    crate::cook_ledger::write_atomic(&meta_path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(meta_path)
}

/// Why a source's fracture data was not cooked.
#[derive(Debug, thiserror::Error)]
pub enum FractureCookError {
    /// The settings name a mesh the source does not have.
    #[error("{source_path}: no mesh `{mesh}` to fracture")]
    NoMesh {
        /// The source file.
        source_path: String,
        /// The mesh asked for.
        mesh: String,
    },
    /// The mesh did not unpack.
    #[error("{source_path}: mesh: {reason}")]
    Mesh {
        /// The source file.
        source_path: String,
        /// Decoder message.
        reason: String,
    },
    /// The fracture tool or the destructible cook refused it (an open mesh, bad settings).
    #[error("{source_path}: {reason} ({code})")]
    Fracture {
        /// The source file.
        source_path: String,
        /// The `fracture_*` or `destruction_*` code.
        code: &'static str,
        /// What went wrong.
        reason: String,
    },
    /// Writing a cooked file failed.
    #[error("writing fracture data: {0}")]
    Io(#[from] std::io::Error),
    /// Encoding the cooked wrapper failed.
    #[error("cooked fracture wrapper: {0}")]
    Cooked(String),
}

/// What [`cook_fracture_sub_assets`] produced.
#[derive(Debug, Default)]
pub struct FractureCookOutput {
    /// Manifest rows (`<source>#fracture/...`).
    pub manifest: Vec<ManifestEntry>,
    /// `.meta` sub-asset rows.
    pub meta: Vec<SubAssetEntry>,
    /// Chunks in the set.
    pub chunks: usize,
    /// Whether the fracture ran (`false`: the cache was current).
    pub built: bool,
}

/// Mesh `path` of a source's cooked sub-assets (path, type path, payload).
fn find_mesh<'a>(
    sub_assets: impl Iterator<Item = (&'a str, &'a str, &'a [u8])>,
    path: &str,
) -> Option<&'a [u8]> {
    sub_assets
        .into_iter()
        .find(|(p, t, _)| *p == path && *t == MESH_TYPE_PATH)
        .map(|(_, _, payload)| payload)
}

/// Bytes of one index of `mesh`.
fn index_bytes(mesh: &CookedMesh) -> u32 {
    match mesh.index_width {
        CookedIndexWidth::U16 => 2,
        CookedIndexWidth::U32 => 4,
    }
}

/// The chunk as a cooked mesh with two material slots. The triangles keep their order: the cook's
/// vertex-cache pass would mix the two slots, and a chunk is too small to gain from it.
fn chunk_mesh(render: &ChunkRenderMesh) -> Result<CookedMesh, regia_asset::MeshValidationError> {
    let cpu = CpuMesh {
        positions: render.positions.clone(),
        normals: render.normals.clone(),
        uvs: render.uvs.clone(),
        tangents: render.tangents.clone(),
        indices: render.indices.clone(),
        ..Default::default()
    };
    let mut mesh = pack_cooked_mesh(&cpu, None)?;
    let width = index_bytes(&mesh);
    let split = render.interior_start * 3 * width;
    let total = mesh.indices.len() as u32;
    mesh.submeshes = [(0, split, 0), (split, total - split, 1)]
        .into_iter()
        .filter(|(_, length, _)| *length > 0)
        .map(|(offset, length, slot)| CookedSubmesh {
            index_byte_offset: offset,
            index_byte_length: length,
            material_slot: slot,
        })
        .collect();
    Ok(mesh)
}

fn write_cooked(
    cache_dir: &Path,
    id: AssetId,
    type_path: &str,
    importer_version: u32,
    dependencies: Vec<AssetId>,
    payload: &[u8],
) -> Result<(), FractureCookError> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(type_path),
        asset_id: id,
        importer_version,
        dependencies,
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, payload).map_err(|e| FractureCookError::Cooked(e.to_string()))?;
    crate::cook_ledger::write_atomic(&cache_dir.join(cooked_file_name(id)), &file)?;
    Ok(())
}

fn cache_key(
    rel_path: &str,
    importer_version: u32,
    settings: &FractureCookSettings,
    mesh: &[u8],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"fracture");
    hasher.update(&FRACTURE_COOK_VERSION.to_le_bytes());
    hasher.update(&importer_version.to_le_bytes());
    hasher.update(&(rel_path.len() as u64).to_le_bytes());
    hasher.update(rel_path.as_bytes());
    hasher.update(&crate::fracture::FRACTURE_FORMAT_VERSION.to_le_bytes());
    hasher.update(&regia_asset::COOKED_MESH_FORMAT_VERSION.to_le_bytes());
    hasher.update(ron::to_string(settings).unwrap_or_default().as_bytes());
    hasher.update(mesh);
    *hasher.finalize().as_bytes()
}

/// Cooks the fracture sub-assets of one source into `cache_dir`. `primary` is the source's primary
/// mesh payload (postcard [`CookedMesh`]) and `sub_assets` its other cooked sub-assets (path, type
/// path, payload). A source whose path, mesh and settings did not change is not fractured again (a
/// `<cooked file>.key` beside the destructible holds the key and the chunk count).
///
/// # Errors
/// A mesh the settings name is missing, the mesh is not a closed solid or the settings are bad, or
/// a write fails.
pub fn cook_fracture_sub_assets<'a>(
    cache_dir: &Path,
    rel_path: &str,
    importer_version: u32,
    parent: AssetId,
    settings: &FractureCookSettings,
    primary: &[u8],
    sub_assets: impl Iterator<Item = (&'a str, &'a str, &'a [u8])>,
) -> Result<FractureCookOutput, FractureCookError> {
    let payload = match settings.mesh.as_deref() {
        None => primary,
        Some(path) => find_mesh(sub_assets, path).ok_or_else(|| FractureCookError::NoMesh {
            source_path: rel_path.to_string(),
            mesh: path.to_string(),
        })?,
    };
    let destructible_id = AssetId::sub_asset(parent, FRACTURE_DESTRUCTIBLE_SUB_PATH);
    let destructible_file = cache_dir.join(cooked_file_name(destructible_id));
    let key_file = cache_dir.join(format!("{}.key", cooked_file_name(destructible_id)));
    let key = cache_key(rel_path, importer_version, settings, payload);
    let cached = std::fs::read(&key_file)
        .ok()
        .filter(|k| destructible_file.is_file() && k.len() == 36 && k[..32] == key)
        .map(|k| u32::from_le_bytes([k[32], k[33], k[34], k[35]]) as usize)
        .filter(|chunks| {
            (0..*chunks).all(|i| {
                let id = AssetId::sub_asset(parent, &fracture_chunk_sub_path(i));
                cache_dir.join(cooked_file_name(id)).is_file()
            })
        });
    let (chunks, built) = match cached {
        Some(chunks) => (chunks, false),
        None => (
            {
                // The key says the files are current only while all of them belong to it: drop it
                // before the first output is rewritten, so an interrupted or failed rebuild is not
                // taken for the old state when the settings go back (audit F3).
                match std::fs::remove_file(&key_file) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                build(
                    cache_dir,
                    rel_path,
                    importer_version,
                    parent,
                    settings,
                    payload,
                    destructible_id,
                )?
            },
            true,
        ),
    };
    if built {
        let mut stored = key.to_vec();
        stored.extend_from_slice(&(chunks as u32).to_le_bytes());
        crate::cook_ledger::write_atomic(&key_file, &stored)?;
    }
    let mut out = FractureCookOutput {
        chunks,
        built,
        ..FractureCookOutput::default()
    };
    let mut add = |path: String, type_path: &str| {
        let id = AssetId::sub_asset(parent, &path);
        out.manifest.push(ManifestEntry {
            path: format!("{rel_path}#{path}"),
            id,
            type_path: type_path.to_string(),
        });
        out.meta.push(SubAssetEntry {
            path,
            id,
            type_path: type_path.to_string(),
        });
    };
    for i in 0..chunks {
        add(fracture_chunk_sub_path(i), MESH_TYPE_PATH);
    }
    add(FRACTURE_DESTRUCTIBLE_SUB_PATH.to_string(), DESTRUCTIBLE_TYPE_PATH);
    Ok(out)
}

/// Fractures the mesh and writes every output; returns the chunk count.
fn build(
    cache_dir: &Path,
    rel_path: &str,
    importer_version: u32,
    parent: AssetId,
    settings: &FractureCookSettings,
    payload: &[u8],
    destructible_id: AssetId,
) -> Result<usize, FractureCookError> {
    let _span = tracing::info_span!("cook_fracture", path = %rel_path).entered();
    let source_path = || rel_path.to_string();
    let bad_mesh = |reason: String| FractureCookError::Mesh {
        source_path: source_path(),
        reason,
    };
    let mesh = CookedMesh::from_postcard(payload)
        .map_err(|e| bad_mesh(e.to_string()))?
        .to_cpu_mesh()
        .map_err(|e| bad_mesh(e.to_string()))?;
    let fracture = fracture_settings(settings);
    let source = FractureSource {
        positions: &mesh.positions,
        indices: &mesh.indices,
        normals: (mesh.normals.len() == mesh.positions.len()).then_some(mesh.normals.as_slice()),
        uvs: (mesh.uvs.len() == mesh.positions.len()).then_some(mesh.uvs.as_slice()),
    };
    let set = fracture_source(&source, &fracture).map_err(|e| FractureCookError::Fracture {
        source_path: source_path(),
        code: e.code(),
        reason: e.to_string(),
    })?;
    let name = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("destructible");
    let mut asset = cook_destructible(&set, &destructible_cook_settings(name, settings)).map_err(|e| {
        FractureCookError::Fracture {
            source_path: source_path(),
            code: e.code(),
            reason: e.to_string(),
        }
    })?;
    let mut chunk_ids = Vec::with_capacity(set.chunks.len());
    for (i, chunk) in set.chunks.iter().enumerate() {
        let path = fracture_chunk_sub_path(i);
        let id = AssetId::sub_asset(parent, &path);
        let cooked = chunk_mesh(&chunk.render).map_err(|e| bad_mesh(e.to_string()))?;
        let bytes = cooked.to_postcard().map_err(|e| bad_mesh(e.to_string()))?;
        write_cooked(
            cache_dir,
            id,
            MESH_TYPE_PATH,
            importer_version,
            Vec::new(),
            &bytes,
        )?;
        chunk_ids.push(id);
        if let Some(c) = asset.chunks.get_mut(i) {
            c.lods = vec![format!("{rel_path}#{path}")];
        }
    }
    let text = asset.to_ron().map_err(|e| FractureCookError::Fracture {
        source_path: source_path(),
        code: e.code(),
        reason: e.to_string(),
    })?;
    write_cooked(
        cache_dir,
        destructible_id,
        DESTRUCTIBLE_TYPE_PATH,
        importer_version,
        chunk_ids,
        text.as_bytes(),
    )?;
    Ok(set.chunks.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fracture::{box_mesh, fracture_mesh};

    #[test]
    fn a_chunk_mesh_has_a_surface_slot_and_a_cut_slot_covering_every_index() {
        let (p, i) = box_mesh([1.0, 1.0, 1.0]).expect("box");
        let set = fracture_mesh(&p, &i, &FractureSettings::random(4, 3)).expect("fracture");
        let render = &set.chunks[0].render;
        let mesh = chunk_mesh(render).expect("pack");
        let width = index_bytes(&mesh);
        assert_eq!(mesh.submeshes.len(), 2);
        assert_eq!(mesh.submeshes[0].material_slot, 0);
        assert_eq!(mesh.submeshes[1].material_slot, 1);
        assert_eq!(
            mesh.submeshes[0].index_byte_length,
            render.interior_start * 3 * width
        );
        assert_eq!(
            mesh.submeshes[0].index_byte_length + mesh.submeshes[1].index_byte_length,
            mesh.indices.len() as u32
        );
        mesh.validate().expect("valid");
        let back = mesh.to_cpu_mesh().expect("unpack");
        assert_eq!(back.indices, render.indices, "the triangles keep their order");
        assert_eq!(back.positions, render.positions);
    }

    #[test]
    fn settings_read_from_ron_with_defaults_for_the_rest() {
        let s: FractureCookSettings = ron::from_str("(chunks: 8, fx_tag: \"brick\")").expect("ron");
        assert_eq!(s.chunks, 8);
        assert_eq!(s.fx_tag, "brick");
        assert_eq!(s.density, 2400.0);
    }

    #[test]
    fn the_key_follows_what_the_outputs_carry() {
        let settings = FractureCookSettings::default();
        let key = cache_key("assets/a.glb", 1, &settings, &[1, 2, 3]);
        assert_eq!(key, cache_key("assets/a.glb", 1, &settings, &[1, 2, 3]));
        assert_ne!(key, cache_key("assets/b.glb", 1, &settings, &[1, 2, 3]));
        assert_ne!(key, cache_key("assets/a.glb", 2, &settings, &[1, 2, 3]));
    }

    /// audit F3: a failed rebuild must not leave the old key standing over rewritten files.
    #[test]
    fn a_failed_rebuild_drops_the_key_so_reverting_settings_rebuilds() {
        let dir = regia_test_utils::unique_temp_dir("fracture-key-drop");
        let (p, i) = box_mesh([1.0, 1.0, 1.0]).expect("box");
        let n = p.len();
        let cpu = CpuMesh {
            positions: p,
            normals: vec![[0.0, 1.0, 0.0]; n],
            uvs: vec![[0.0, 0.0]; n],
            indices: i,
            ..Default::default()
        };
        let good = pack_cooked_mesh(&cpu, None)
            .expect("pack")
            .to_postcard()
            .expect("postcard");
        let parent = AssetId::new_random();
        let a = FractureCookSettings::default();
        let run = |payload: &[u8]| {
            cook_fracture_sub_assets(&dir, "assets/a.glb", 1, parent, &a, payload, std::iter::empty())
        };
        assert!(run(&good).expect("first").built);
        assert!(!run(&good).expect("second").built, "unchanged: cached");
        // A rebuild for other input fails part-way.
        assert!(run(b"not a mesh").is_err());
        // Back to the first input: the cache cannot be trusted any more.
        assert!(run(&good).expect("third").built);
    }

    #[test]
    fn a_named_mesh_must_exist() {
        let sub = [("mesh/a", MESH_TYPE_PATH, [1u8].as_slice())];
        assert!(find_mesh(sub.iter().copied(), "mesh/a").is_some());
        assert!(find_mesh(sub.iter().copied(), "mesh/b").is_none());
        let other = [("mesh/a", "other", [1u8].as_slice())];
        assert!(find_mesh(other.iter().copied(), "mesh/a").is_none());
    }
}
