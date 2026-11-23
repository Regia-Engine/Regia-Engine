// SPDX-License-Identifier: MPL-2.0
//! Convex decomposition at import (`collision: ConvexDecomposition` in a glTF `.meta`, ADR-2301).
//!
//! For every cooked mesh primitive this writes a sibling sub-asset `collision/<node>/primitive_<i>`
//! (type [`regia_physics::hull_set::HULL_SET_TYPE_PATH`], `FCHS` bytes) holding the convex hulls
//! of LOD 0, built by [`regia_physics::convex_decomposition::decompose`]. A primitive whose cached
//! hulls were built from the same mesh bytes with the same settings is not rebuilt.

use std::path::Path;

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, CookedMesh, ManifestEntry, SubAssetEntry, cooked_file_name,
    encode_cooked_file,
};
use regia_core::math::Vec3;
use regia_core::mesh_query::TriMesh;
use regia_physics::convex_decomposition::{DecompositionParams, decompose};
use regia_physics::hull_set::{HULL_SET_FORMAT_VERSION, HULL_SET_TYPE_PATH, HullSet};

use crate::gltf_settings::CollisionHullSettings;

/// Bumped whenever the collision cooker's output changes for the same input.
pub const COLLISION_COOK_VERSION: u32 = 1;

const MESH_TYPE_PATH: &str = "regia_asset::CookedMesh";

/// Why a primitive's hulls were not cooked.
#[derive(Debug, thiserror::Error)]
pub enum CollisionCookError {
    /// The cooked mesh payload does not decode.
    #[error("{path}: cooked mesh: {reason}")]
    Mesh {
        /// Sub-asset path.
        path: String,
        /// Decoder message.
        reason: String,
    },
    /// The decomposition failed (bad setting, empty or flat mesh).
    #[error("{path}: convex decomposition: {reason}")]
    Decompose {
        /// Sub-asset path.
        path: String,
        /// The decomposition's message.
        reason: String,
    },
    /// Writing the cooked file failed.
    #[error("writing collision hulls: {0}")]
    Io(#[from] std::io::Error),
    /// Encoding the cooked wrapper failed.
    #[error("cooked hull wrapper: {0}")]
    Cooked(String),
}

/// What [`cook_collision_sub_assets`] produced.
#[derive(Debug, Default)]
pub struct CollisionCookOutput {
    /// Manifest rows (`<source>#collision/...`).
    pub manifest: Vec<ManifestEntry>,
    /// `.meta` sub-asset rows.
    pub meta: Vec<SubAssetEntry>,
    /// Primitives whose hulls were rebuilt (the rest were current in the cache).
    pub built: usize,
}

/// The sub-asset path of the hulls cooked for the mesh sub-asset `mesh_sub_path`
/// (`mesh/<node>/primitive_<i>` becomes `collision/<node>/primitive_<i>`). `None` for other paths.
#[must_use]
pub fn collision_sub_path(mesh_sub_path: &str) -> Option<String> {
    mesh_sub_path
        .strip_prefix("mesh/")
        .map(|rest| format!("collision/{rest}"))
}

/// The decomposition parameters a setting block means.
#[must_use]
pub fn params_of(settings: &CollisionHullSettings) -> DecompositionParams {
    DecompositionParams {
        max_hulls: settings.max_hulls,
        resolution: settings.resolution,
        max_concavity: settings.max_concavity,
        max_depth: settings.max_depth,
        max_hull_vertices: settings.max_hull_vertices,
    }
}

/// Decomposes a cooked mesh payload (postcard [`CookedMesh`], LOD 0) into a hull set.
///
/// # Errors
///
/// The payload does not decode, or the decomposition rejects the mesh or a setting.
pub fn hulls_for_cooked_mesh(
    path: &str,
    payload: &[u8],
    settings: &CollisionHullSettings,
) -> Result<HullSet, CollisionCookError> {
    let mesh_err = |reason: String| CollisionCookError::Mesh {
        path: path.to_string(),
        reason,
    };
    let mesh = CookedMesh::from_postcard(payload).map_err(|e| mesh_err(e.to_string()))?;
    let cpu = regia_render::visbuffer::cooked_mesh_lod0(&mesh).map_err(mesh_err)?;
    let positions = cpu.positions.iter().map(|&p| Vec3::from_array(p)).collect();
    let tri = TriMesh::from_indices(positions, &cpu.indices).map_err(|e| CollisionCookError::Mesh {
        path: path.to_string(),
        reason: e.to_string(),
    })?;
    let hulls = decompose(&tri, &params_of(settings)).map_err(|e| CollisionCookError::Decompose {
        path: path.to_string(),
        reason: e.to_string(),
    })?;
    Ok(HullSet::from_hulls(&hulls))
}

fn cache_key(importer_version: u32, payload: &[u8], settings: &CollisionHullSettings) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"fchs");
    hasher.update(&HULL_SET_FORMAT_VERSION.to_le_bytes());
    hasher.update(&COLLISION_COOK_VERSION.to_le_bytes());
    // The cooked header embeds the importer version, so a bump must rewrite the file (audit F7).
    hasher.update(&importer_version.to_le_bytes());
    hasher.update(&settings.max_hulls.to_le_bytes());
    hasher.update(&settings.resolution.to_le_bytes());
    hasher.update(&settings.max_concavity.to_le_bytes());
    hasher.update(&settings.max_depth.to_le_bytes());
    hasher.update(&settings.max_hull_vertices.to_le_bytes());
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

/// Writes a hull-set sub-asset for every cooked mesh primitive in `sub_assets` (path, id, type
/// path, payload) into `cache_dir`.
///
/// # Errors
///
/// A mesh fails to decode or to decompose, or a write fails.
pub fn cook_collision_sub_assets<'a>(
    cache_dir: &Path,
    rel_path: &str,
    importer_version: u32,
    parent: AssetId,
    settings: &CollisionHullSettings,
    sub_assets: impl Iterator<Item = (&'a str, AssetId, &'a str, &'a [u8])>,
) -> Result<CollisionCookOutput, CollisionCookError> {
    params_of(settings)
        .validate()
        .map_err(|e| CollisionCookError::Decompose {
            path: rel_path.to_string(),
            reason: e.to_string(),
        })?;
    let mut out = CollisionCookOutput::default();
    for (sub_path, mesh_id, type_path, payload) in sub_assets {
        if type_path != MESH_TYPE_PATH {
            continue;
        }
        let Some(path) = collision_sub_path(sub_path) else {
            continue;
        };
        let id = AssetId::sub_asset(parent, &path);
        let file = cache_dir.join(cooked_file_name(id));
        let key_file = cache_dir.join(format!("{}.key", cooked_file_name(id)));
        let key = cache_key(importer_version, payload, settings);
        let current = file.is_file() && std::fs::read(&key_file).is_ok_and(|k| k == key);
        if !current {
            let _span = tracing::info_span!("cook_collision", path = %path).entered();
            let bytes = match hulls_for_cooked_mesh(&path, payload, settings) {
                Ok(set) => set.to_bytes(),
                // A flat or empty primitive (a plane, a decal quad) has no volume to cover; it
                // gets no hull set and the rest of the file still imports.
                Err(CollisionCookError::Decompose { path, reason }) => {
                    tracing::warn!(target: "regia_asset_pipeline", %path, %reason, "no collision hulls for this primitive");
                    continue;
                }
                Err(e) => return Err(e),
            };
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: regia_core::id::TypePathHash::of(HULL_SET_TYPE_PATH),
                asset_id: id,
                importer_version,
                dependencies: vec![mesh_id],
                payload_len: bytes.len() as u64,
            };
            let cooked =
                encode_cooked_file(&header, &bytes).map_err(|e| CollisionCookError::Cooked(e.to_string()))?;
            // The key goes first, then the file, then the key again: a reader never sees a torn
            // file and an interrupted write is not taken for current (audit F7).
            match std::fs::remove_file(&key_file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            crate::cook_ledger::write_atomic(&file, &cooked)?;
            crate::cook_ledger::write_atomic(&key_file, &key)?;
            out.built += 1;
        }
        out.manifest.push(ManifestEntry {
            path: format!("{rel_path}#{path}"),
            id,
            type_path: HULL_SET_TYPE_PATH.to_string(),
        });
        out.meta.push(SubAssetEntry {
            path,
            id,
            type_path: HULL_SET_TYPE_PATH.to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxes_payload(offsets: &[f32]) -> Vec<u8> {
        let mut positions: Vec<[f32; 3]> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        for &x0 in offsets {
            let base = positions.len() as u32;
            for i in 0..8u32 {
                positions.push([x0 + (i & 1) as f32, ((i >> 1) & 1) as f32, ((i >> 2) & 1) as f32]);
            }
            // Counter-clockwise from outside; corner index bits are (x, y, z).
            let quads: [[u32; 4]; 6] = [
                [0, 4, 6, 2],
                [1, 3, 7, 5],
                [0, 1, 5, 4],
                [2, 6, 7, 3],
                [0, 2, 3, 1],
                [4, 5, 7, 6],
            ];
            for q in quads {
                indices.extend_from_slice(&[base + q[0], base + q[1], base + q[2]]);
                indices.extend_from_slice(&[base + q[0], base + q[2], base + q[3]]);
            }
        }
        let n = positions.len();
        let cpu = regia_asset::CpuMesh {
            positions,
            normals: vec![[0.0, 1.0, 0.0]; n],
            uvs: vec![[0.0, 0.0]; n],
            tangents: Vec::new(),
            indices,
            ..Default::default()
        };
        let cooked =
            crate::mesh_cook::pack_cooked_mesh(&crate::mesh_cook::cook_mesh(cpu), None).expect("pack");
        cooked.to_postcard().expect("postcard")
    }

    #[test]
    fn two_separate_boxes_cook_to_two_hulls_and_the_cache_is_reused() {
        let dir = regia_test_utils::unique_temp_dir("collision-cook");
        let payload = boxes_payload(&[0.0, 3.0]);
        let parent = AssetId::new_random();
        let mesh_id = AssetId::sub_asset(parent, "mesh/Pair/primitive_0");
        let subs = [(
            "mesh/Pair/primitive_0",
            mesh_id,
            MESH_TYPE_PATH,
            payload.as_slice(),
        )];
        let settings = CollisionHullSettings::default();
        let first =
            cook_collision_sub_assets(&dir, "a/pair.gltf", 1, parent, &settings, subs.iter().copied())
                .expect("cook");
        assert_eq!(first.built, 1);
        assert_eq!(first.manifest[0].path, "a/pair.gltf#collision/Pair/primitive_0");
        assert_eq!(first.manifest[0].type_path, HULL_SET_TYPE_PATH);
        let again =
            cook_collision_sub_assets(&dir, "a/pair.gltf", 1, parent, &settings, subs.iter().copied())
                .expect("recook");
        assert_eq!(again.built, 0);
        // A changed setting recooks.
        let changed = CollisionHullSettings {
            max_hulls: 2,
            ..settings
        };
        let third = cook_collision_sub_assets(&dir, "a/pair.gltf", 1, parent, &changed, subs.iter().copied())
            .expect("changed");
        assert_eq!(third.built, 1);
        // audit F7: the header carries the importer version, so a bump rewrites the file.
        let bumped =
            cook_collision_sub_assets(&dir, "a/pair.gltf", 2, parent, &changed, subs.iter().copied())
                .expect("bumped");
        assert_eq!(bumped.built, 1);
        let set = hulls_for_cooked_mesh("p", &payload, &settings).expect("hulls");
        assert!(set.hulls.len() >= 2, "got {} hulls", set.hulls.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_setting_is_an_error_and_other_asset_types_are_skipped() {
        let dir = regia_test_utils::unique_temp_dir("collision-cook-bad");
        let parent = AssetId::new_random();
        let bad = CollisionHullSettings {
            max_hulls: 0,
            ..CollisionHullSettings::default()
        };
        let err = cook_collision_sub_assets(&dir, "a.gltf", 1, parent, &bad, std::iter::empty())
            .expect_err("bad setting");
        assert!(matches!(err, CollisionCookError::Decompose { .. }));
        let img = [(
            "image/x",
            AssetId::new_random(),
            "regia_asset::CookedImage",
            &[0u8][..],
        )];
        let ok = cook_collision_sub_assets(
            &dir,
            "a.gltf",
            1,
            parent,
            &CollisionHullSettings::default(),
            img.iter().copied(),
        )
        .expect("skip");
        assert!(ok.manifest.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sub_path_maps_mesh_to_collision() {
        assert_eq!(
            collision_sub_path("mesh/A/primitive_1").as_deref(),
            Some("collision/A/primitive_1")
        );
        assert_eq!(collision_sub_path("image/A"), None);
    }
}
