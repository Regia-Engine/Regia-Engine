// SPDX-License-Identifier: MPL-2.0
//! Loaded assets and their bytes per asset type (`docs/specs/83-prefab-profiler-volumes.md` § 4.1,
//! ADR-9740): what `memory.report`'s `assets_by_type` and the editor Profiler's Memory view list.
//!
//! Computed on demand by walking the asset server's typed stores; nothing is counted per load or
//! per frame. The bytes are the heap blocks the decoded payloads hold (each `Vec`'s capacity times
//! its element size, plus the record itself), which is what the `assets` subsystem of the heap
//! ledger (`regia_core::memory`) charges for them. The ledger's total is larger by the server's
//! bookkeeping (id maps, load states, sources): the documented slack is 64 KiB plus 2 % of the
//! payload bytes. A texture read from a mapped pak counts its payload length, which lives in the
//! mapped file rather than the heap.

use crate::cooked::{CookedImage, CookedTexture, CpuMesh};
use crate::material::CookedStandardMaterial;
use crate::mesh_skin::{CpuMorphTarget, CpuSkin};
use crate::server::AssetServer;
use crate::storage::Assets;

/// One asset type's loaded count and bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssetTypeMemory {
    /// `mesh`, `image`, `texture` or `material`.
    pub type_name: &'static str,
    /// Assets of this type loaded now.
    pub loaded: u64,
    /// Bytes they hold (see the module docs for what counts).
    pub bytes: u64,
}

/// Bytes of a `Vec`'s heap block.
fn vec_bytes<T>(v: &Vec<T>) -> u64 {
    (v.capacity() * size_of::<T>()) as u64
}

fn string_bytes(s: &String) -> u64 {
    s.capacity() as u64
}

fn skin_bytes(skin: &CpuSkin) -> u64 {
    size_of::<CpuSkin>() as u64
        + vec_bytes(&skin.joint_names)
        + skin.joint_names.iter().map(string_bytes).sum::<u64>()
        + vec_bytes(&skin.joints)
        + vec_bytes(&skin.weights)
        + vec_bytes(&skin.joints1)
        + vec_bytes(&skin.weights1)
}

fn morph_bytes(m: &CpuMorphTarget) -> u64 {
    string_bytes(&m.name)
        + vec_bytes(&m.vertex_indices)
        + vec_bytes(&m.position_deltas)
        + vec_bytes(&m.normal_deltas)
        + vec_bytes(&m.tangent_deltas)
}

/// Heap bytes of one decoded mesh (its streams, skin and morph targets; the record itself sits in
/// the store's slot array).
#[must_use]
pub fn mesh_bytes(mesh: &CpuMesh) -> u64 {
    vec_bytes(&mesh.positions)
        + vec_bytes(&mesh.normals)
        + vec_bytes(&mesh.uvs)
        + vec_bytes(&mesh.tangents)
        + vec_bytes(&mesh.indices)
        + vec_bytes(&mesh.colors)
        + mesh.skin.as_ref().map_or(0, skin_bytes)
        + vec_bytes(&mesh.morph_targets)
        + mesh.morph_targets.iter().map(morph_bytes).sum::<u64>()
}

/// Walks one store: (live assets, bytes), each asset's record size plus `heap(asset)`.
fn store<T>(assets: &Assets<T>, heap: impl Fn(&T) -> u64) -> (u64, u64) {
    let mut loaded = 0;
    let mut bytes = 0;
    for (_, handle) in assets.entries_from(0) {
        if let Some(asset) = assets.get(&handle) {
            loaded += 1;
            bytes += size_of::<T>() as u64 + heap(asset);
        }
    }
    (loaded, bytes)
}

impl AssetServer {
    /// Loaded count and bytes per asset type, in a fixed order (mesh, image, texture, material).
    /// Walks the stores now: call it for a report, not per frame.
    #[must_use]
    pub fn memory_by_type(&self) -> Vec<AssetTypeMemory> {
        let _span = tracing::trace_span!("asset_memory_by_type").entered();
        let rows = [
            ("mesh", store(self.meshes(), mesh_bytes)),
            (
                "image",
                store(self.images(), |i: &CookedImage| vec_bytes(&i.rgba8)),
            ),
            (
                "texture",
                store(self.textures(), |t: &CookedTexture| t.ktx2.len() as u64),
            ),
            (
                "material",
                // The authored record is stored inline; its few strings are inside the slack.
                store(self.materials(), |_: &CookedStandardMaterial| 0),
            ),
        ];
        rows.into_iter()
            .map(|(type_name, (loaded, bytes))| AssetTypeMemory {
                type_name,
                loaded,
                bytes,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_bytes_count_every_stream_by_capacity() {
        let mesh = CpuMesh {
            positions: vec![[0.0; 3]; 3],
            normals: vec![[0.0; 3]; 3],
            uvs: vec![[0.0; 2]; 3],
            indices: Vec::with_capacity(8),
            ..Default::default()
        };
        assert_eq!(mesh_bytes(&mesh), 36 + 36 + 24 + 32);
    }

    #[test]
    fn an_empty_server_lists_every_type_with_nothing_loaded() {
        let root = std::env::temp_dir().join(format!("regia-type-memory-{}", std::process::id()));
        let server = AssetServer::open(crate::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("open");
        let rows = server.memory_by_type();
        let names: Vec<&str> = rows.iter().map(|r| r.type_name).collect();
        assert_eq!(names, ["mesh", "image", "texture", "material"]);
        assert!(rows.iter().all(|r| r.loaded == 0 && r.bytes == 0));
        let _ = std::fs::remove_dir_all(&root);
    }
}
