// SPDX-License-Identifier: MPL-2.0
//! Cooked meshlets (`.fmlt`) for the visibility-buffer path (`docs/specs/27` § 4.2 stage 3).
//!
//! With `meshlets: true` in a glTF's `.meta` settings, `regia cook` writes one
//! [`regia_render::visbuffer::MESHLET_TYPE_PATH`] sub-asset per cooked mesh primitive
//! (`meshlets/<node>/primitive_<i>`, next to `mesh/<node>/primitive_<i>`). The clusters are
//! built over the cooked mesh as the runtime decodes it
//! ([`regia_render::visbuffer::cooked_mesh_lod0`]),
//! so they index the vertex buffer a game uploads. The runtime reads them with
//! [`regia_render::MeshletMesh::from_cooked_file`] and links no cooker.
//!
//! Versioning (`specs/12` § 4a): the payload carries [`regia_render::visbuffer::MESHLET_FORMAT_VERSION`]
//! and loaders reject any other value. Meshlets are derived data, so the migration rule is a
//! recook: the cache key below hashes the cooked mesh bytes, the format version and
//! [`MESHLET_COOK_VERSION`], and a changed key rebuilds the clusters on the next cook.

use std::path::Path;

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, CookedMesh, ManifestEntry, SubAssetEntry, cooked_file_name,
    encode_cooked_file,
};

use crate::meshlet::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};

/// Bumped whenever the meshlet cooker's output changes for the same input.
pub const MESHLET_COOK_VERSION: u32 = 1;

const MESH_TYPE_PATH: &str = "regia_asset::CookedMesh";

/// Why a primitive's meshlets were not cooked.
#[derive(Debug, thiserror::Error)]
pub enum MeshletCookError {
    /// The cooked mesh payload does not decode.
    #[error("{path}: cooked mesh: {reason}")]
    Mesh {
        /// Sub-asset path.
        path: String,
        /// Decoder message.
        reason: String,
    },
    /// The cluster builder failed.
    #[error("{path}: meshlets: {reason}")]
    Build {
        /// Sub-asset path.
        path: String,
        /// Builder message.
        reason: String,
    },
    /// Writing the cooked file failed.
    #[error("writing meshlets: {0}")]
    Io(#[from] std::io::Error),
    /// Encoding the cooked wrapper failed.
    #[error("cooked meshlet wrapper: {0}")]
    Cooked(String),
}

/// What [`cook_meshlet_sub_assets`] produced.
#[derive(Debug, Default)]
pub struct MeshletCookOutput {
    /// Manifest rows (`<source>#meshlets/...`).
    pub manifest: Vec<ManifestEntry>,
    /// `.meta` sub-asset rows.
    pub meta: Vec<SubAssetEntry>,
    /// Primitives whose clusters were rebuilt (the rest were current in the cache).
    pub built: usize,
}

/// Builds the clusters of a cooked mesh payload (postcard [`CookedMesh`]) as the cook does.
///
/// # Errors
///
/// The payload does not decode or the builder fails.
pub fn meshlets_for_cooked_mesh(
    path: &str,
    payload: &[u8],
) -> Result<regia_render::MeshletMesh, MeshletCookError> {
    let bad = |reason: String| MeshletCookError::Mesh {
        path: path.to_string(),
        reason,
    };
    let mesh = CookedMesh::from_postcard(payload).map_err(|e| bad(e.to_string()))?;
    let cpu = regia_render::visbuffer::cooked_mesh_lod0(&mesh).map_err(bad)?;
    let attributes = (cpu.normals.len() == cpu.positions.len() && cpu.uvs.len() == cpu.positions.len())
        .then_some(MeshletAttributes {
            normals: &cpu.normals,
            uvs: &cpu.uvs,
        });
    build_meshlet_mesh_with_attributes(
        &cpu.positions,
        attributes,
        &cpu.indices,
        &MeshletSettings::default(),
    )
    .map_err(|e| MeshletCookError::Build {
        path: path.to_string(),
        reason: e.to_string(),
    })
}

fn cache_key(payload: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"fmlt");
    hasher.update(&regia_render::visbuffer::MESHLET_FORMAT_VERSION.to_le_bytes());
    hasher.update(&MESHLET_COOK_VERSION.to_le_bytes());
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

/// Writes a meshlet sub-asset for every cooked mesh primitive in `sub_assets` (path, id,
/// type path, payload) into `cache_dir`. A primitive whose cached clusters were built from the
/// same mesh bytes by the same cooker is not rebuilt (a `<cooked file>.key` beside it holds the
/// key).
///
/// # Errors
///
/// A mesh fails to decode or to cluster, or a write fails.
pub fn cook_meshlet_sub_assets<'a>(
    cache_dir: &Path,
    rel_path: &str,
    importer_version: u32,
    parent: AssetId,
    sub_assets: impl Iterator<Item = (&'a str, AssetId, &'a str, &'a [u8])>,
) -> Result<MeshletCookOutput, MeshletCookError> {
    let mut out = MeshletCookOutput::default();
    for (sub_path, mesh_id, type_path, payload) in sub_assets {
        if type_path != MESH_TYPE_PATH {
            continue;
        }
        let Some(path) = regia_render::visbuffer::meshlet_sub_path(sub_path) else {
            continue;
        };
        let id = AssetId::sub_asset(parent, &path);
        let file = cache_dir.join(cooked_file_name(id));
        let key_file = cache_dir.join(format!("{}.key", cooked_file_name(id)));
        let key = cache_key(payload);
        let current = file.is_file() && std::fs::read(&key_file).is_ok_and(|k| k == key);
        if !current {
            let _span = tracing::info_span!("cook_meshlets", path = %path).entered();
            let mesh = meshlets_for_cooked_mesh(&path, payload)?;
            let bytes = mesh.to_bytes();
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: regia_core::id::TypePathHash::of(regia_render::visbuffer::MESHLET_TYPE_PATH),
                asset_id: id,
                importer_version,
                dependencies: vec![mesh_id],
                payload_len: bytes.len() as u64,
            };
            let cooked =
                encode_cooked_file(&header, &bytes).map_err(|e| MeshletCookError::Cooked(e.to_string()))?;
            crate::cook_ledger::write_atomic(&file, &cooked)?;
            crate::cook_ledger::write_atomic(&key_file, &key)?;
            out.built += 1;
        }
        out.manifest.push(ManifestEntry {
            path: format!("{rel_path}#{path}"),
            id,
            type_path: regia_render::visbuffer::MESHLET_TYPE_PATH.to_string(),
        });
        out.meta.push(SubAssetEntry {
            path,
            id,
            type_path: regia_render::visbuffer::MESHLET_TYPE_PATH.to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cooked_sphere() -> Vec<u8> {
        // A UV sphere through the mesh cook, as a glTF primitive would be.
        let (rings, segments) = (12u32, 24u32);
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut uvs = Vec::new();
        for r in 0..=rings {
            let phi = std::f32::consts::PI * r as f32 / rings as f32;
            for s in 0..=segments {
                let theta = std::f32::consts::TAU * s as f32 / segments as f32;
                let n = [phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin()];
                positions.push(n);
                normals.push(n);
                uvs.push([s as f32 / segments as f32, r as f32 / rings as f32]);
            }
        }
        let mut indices = Vec::new();
        for r in 0..rings {
            for s in 0..segments {
                let a = r * (segments + 1) + s;
                let b = a + segments + 1;
                indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
            }
        }
        let cpu = regia_asset::CpuMesh {
            positions,
            normals,
            uvs,
            tangents: Vec::new(),
            indices,
            ..Default::default()
        };
        let cooked =
            crate::mesh_cook::pack_cooked_mesh(&crate::mesh_cook::cook_mesh(cpu), None).expect("pack");
        cooked.to_postcard().expect("postcard")
    }

    #[test]
    fn cooked_meshlets_index_the_runtime_mesh_and_round_trip() {
        let dir = std::env::temp_dir().join(format!("regia-meshlet-cook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let payload = cooked_sphere();
        let parent = AssetId::new_random();
        let mesh_id = AssetId::sub_asset(parent, "mesh/Rock/primitive_0");
        let subs = [(
            "mesh/Rock/primitive_0",
            mesh_id,
            MESH_TYPE_PATH,
            payload.as_slice(),
        )];
        let first =
            cook_meshlet_sub_assets(&dir, "assets/rock.gltf", 1, parent, subs.iter().copied()).expect("cook");
        assert_eq!(first.built, 1);
        assert_eq!(first.manifest.len(), 1);
        assert_eq!(
            first.manifest[0].path,
            "assets/rock.gltf#meshlets/Rock/primitive_0"
        );
        // Same mesh bytes: the cached clusters are reused.
        let again = cook_meshlet_sub_assets(&dir, "assets/rock.gltf", 1, parent, subs.iter().copied())
            .expect("recook");
        assert_eq!(again.built, 0);
        // The runtime reads the cooked file with no cooker and it matches the runtime mesh.
        let bytes = std::fs::read(dir.join(cooked_file_name(first.manifest[0].id))).expect("read");
        let meshlets = regia_render::MeshletMesh::from_cooked_file(&bytes).expect("load");
        let cpu =
            regia_render::visbuffer::cooked_mesh_lod0(&CookedMesh::from_postcard(&payload).expect("mesh"))
                .expect("lod0");
        assert_eq!(meshlets.source_vertex_count as usize, cpu.positions.len());
        assert_eq!(meshlets.source_index_count as usize, cpu.indices.len());
        assert!(meshlets.level_count() > 1, "a simplification DAG, not one level");
        // Another asset type is refused.
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of(MESH_TYPE_PATH),
            asset_id: mesh_id,
            importer_version: 1,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        let wrong = encode_cooked_file(&header, &payload).expect("encode");
        assert!(matches!(
            regia_render::MeshletMesh::from_cooked_file(&wrong),
            Err(regia_render::visbuffer::MeshletFormatError::WrongType)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
