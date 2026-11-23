// SPDX-License-Identifier: MPL-2.0
//! The built-in kinds on the generic asset API (ADR-7256, spec 12 §§ 2-3).
//!
//! Every [`AssetServer`] registers these loaders when it opens, so `load::<CpuMesh>`,
//! `load::<CookedTexture>`, `load::<CookedImage>` and `load::<CookedStandardMaterial>` return
//! counted [`super::StrongHandle`]s whose values live in the same built-in slabs the typed loads
//! use (`AssetServer::meshes`, `textures`, ...). Their CPU data is released after a registered
//! [`super::PrepareAsset`] unless the load asked for [`super::LoadSettings::keep_cpu_data`].

use super::AssetServer;
use super::asset_api::{AssetLoader, LoadContext};
use crate::cooked::{CookedImage, CookedMesh, CookedTexture, CpuMesh};
use crate::live::LoadedKind;
use crate::loader::{AssetLoadError, AssetTypeTag};
use crate::material::CookedStandardMaterial;

/// Decodes the three cooked mesh forms (compact, GPU-ready, legacy CPU) into a [`CpuMesh`].
pub(super) struct MeshLoader;

impl AssetLoader for MeshLoader {
    type Asset = CpuMesh;

    fn type_tag(&self) -> AssetTypeTag {
        AssetTypeTag::mesh()
    }

    fn accepts(&self, tag: AssetTypeTag) -> bool {
        tag == AssetTypeTag::mesh()
            || tag == AssetTypeTag::gpu_mesh()
            || tag == AssetTypeTag::mesh_legacy_cpu()
    }

    fn load(&self, payload: &[u8], ctx: &mut LoadContext<'_>) -> Result<CpuMesh, AssetLoadError> {
        let tag = AssetTypeTag(ctx.header().type_tag);
        if tag == AssetTypeTag::mesh_legacy_cpu() {
            Ok(CpuMesh::from_postcard(payload)?)
        } else if tag == AssetTypeTag::gpu_mesh() {
            Ok(crate::gpu_mesh::GpuMeshView::parse(payload)?.to_cpu_mesh()?)
        } else {
            Ok(CookedMesh::from_postcard(payload)?.to_cpu_mesh()?)
        }
    }

    fn cpu_bytes(&self, mesh: &CpuMesh) -> u64 {
        mesh_cpu_bytes(mesh)
    }

    fn kind(&self) -> &'static str {
        "mesh"
    }

    fn release_cpu_data(&self, mesh: &mut CpuMesh) {
        // The vertex and index streams go. The skin keeps its joint names (what the animation
        // side maps its palette by); its per-vertex influences go with the other streams: the
        // renderer's upload carries them (ADR-8610), and a host that reads them on the CPU loads
        // with `keep_cpu_data`.
        if let Some(skin) = mesh.skin.as_mut() {
            skin.joints = Vec::new();
            skin.weights = Vec::new();
            skin.joints1 = Vec::new();
            skin.weights1 = Vec::new();
        }
        mesh.positions = Vec::new();
        mesh.normals = Vec::new();
        mesh.uvs = Vec::new();
        mesh.tangents = Vec::new();
        mesh.indices = Vec::new();
        mesh.colors = Vec::new();
        mesh.morph_targets = Vec::new();
    }
}

/// The CPU bytes of a mesh's streams (vertex attributes, indices, colors, morph deltas, skin).
fn mesh_cpu_bytes(mesh: &CpuMesh) -> u64 {
    let morph: usize = mesh
        .morph_targets
        .iter()
        .map(|m| {
            m.vertex_indices.len() * 4
                + (m.position_deltas.len() + m.normal_deltas.len() + m.tangent_deltas.len()) * 12
        })
        .sum();
    let skin = mesh.skin.as_ref().map_or(0, |s| {
        (s.joints.len() + s.weights.len() + s.joints1.len() + s.weights1.len()) * 8
    });
    (mesh.positions.len() * 12
        + mesh.normals.len() * 12
        + mesh.uvs.len() * 8
        + mesh.tangents.len() * 16
        + mesh.indices.len() * 4
        + mesh.colors.len() * 16
        + morph
        + skin) as u64
}

/// Decodes a cooked KTX2 texture.
pub(super) struct TextureLoader;

impl AssetLoader for TextureLoader {
    type Asset = CookedTexture;

    fn type_tag(&self) -> AssetTypeTag {
        AssetTypeTag::texture()
    }

    fn load(&self, _payload: &[u8], ctx: &mut LoadContext<'_>) -> Result<CookedTexture, AssetLoadError> {
        // The KTX2 bytes stay a view of the payload: of the pak mapping when the pak serves
        // views, so a counted load copies nothing, like `load_texture` (ADR-7284).
        Ok(CookedTexture::from_postcard_shared(&ctx.payload_view())?)
    }

    fn cpu_bytes(&self, texture: &CookedTexture) -> u64 {
        texture.ktx2.len() as u64
    }

    fn kind(&self) -> &'static str {
        "texture"
    }

    fn release_cpu_data(&self, texture: &mut CookedTexture) {
        // Width, height and usage stay: they describe the GPU copy.
        texture.ktx2 = crate::pak::PakPayload::from(Vec::new());
    }
}

/// Decodes a cooked RGBA8 image.
pub(super) struct ImageLoader;

impl AssetLoader for ImageLoader {
    type Asset = CookedImage;

    fn type_tag(&self) -> AssetTypeTag {
        AssetTypeTag::image()
    }

    fn load(&self, payload: &[u8], _ctx: &mut LoadContext<'_>) -> Result<CookedImage, AssetLoadError> {
        Ok(CookedImage::from_postcard(payload)?)
    }

    fn cpu_bytes(&self, image: &CookedImage) -> u64 {
        image.rgba8.len() as u64
    }

    fn kind(&self) -> &'static str {
        "image"
    }

    fn release_cpu_data(&self, image: &mut CookedImage) {
        image.rgba8 = Vec::new();
    }
}

/// Decodes a cooked standard material. Its value is small and is never released.
pub(super) struct MaterialLoader;

impl AssetLoader for MaterialLoader {
    type Asset = CookedStandardMaterial;

    fn type_tag(&self) -> AssetTypeTag {
        AssetTypeTag::standard_material()
    }

    fn load(
        &self,
        payload: &[u8],
        _ctx: &mut LoadContext<'_>,
    ) -> Result<CookedStandardMaterial, AssetLoadError> {
        Ok(CookedStandardMaterial::from_postcard(payload)?)
    }

    fn kind(&self) -> &'static str {
        "material"
    }
}

/// Registers the four built-in loaders on a server that just opened.
pub(super) fn register_builtin_loaders(server: &mut AssetServer) {
    server.register_loader(MeshLoader);
    server.register_loader(TextureLoader);
    server.register_loader(ImageLoader);
    server.register_loader(MaterialLoader);
}

/// The [`LoadedKind`] a hot reload of a counted built-in value reports.
pub(super) fn builtin_loaded_kind(type_id: std::any::TypeId) -> Option<LoadedKind> {
    use std::any::TypeId;
    if type_id == TypeId::of::<CpuMesh>() {
        Some(LoadedKind::Mesh)
    } else if type_id == TypeId::of::<CookedTexture>() {
        Some(LoadedKind::Texture)
    } else if type_id == TypeId::of::<CookedImage>() {
        Some(LoadedKind::Image)
    } else if type_id == TypeId::of::<CookedStandardMaterial>() {
        Some(LoadedKind::Material)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releasing_a_mesh_keeps_its_skin_and_empties_its_streams() {
        let mut mesh = CpuMesh {
            positions: vec![[0.0; 3]; 3],
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        assert_eq!(MeshLoader.cpu_bytes(&mesh), 3 * 12 + 3 * 4);
        MeshLoader.release_cpu_data(&mut mesh);
        assert_eq!(MeshLoader.cpu_bytes(&mesh), 0);
        assert_eq!(
            mesh.positions.capacity(),
            0,
            "the allocation goes, not just the length"
        );
    }

    /// ADR-8610: a skinned mesh's release keeps the joint names and drops the per-vertex
    /// influences (the counted upload carries them), so `assets.handles` shows no CPU bytes.
    #[test]
    fn releasing_a_skinned_mesh_keeps_its_joint_names_and_drops_its_influences() {
        let mut mesh = CpuMesh {
            positions: vec![[0.0; 3]; 3],
            indices: vec![0, 1, 2],
            skin: Some(crate::CpuSkin {
                joint_names: vec!["root".into(), "tip".into()],
                joints: vec![[0, 1, 0, 0]; 3],
                weights: vec![[40000, 25535, 0, 0]; 3],
                joints1: Vec::new(),
                weights1: Vec::new(),
                eight_influences: false,
            }),
            ..Default::default()
        };
        assert_eq!(MeshLoader.cpu_bytes(&mesh), 3 * 12 + 3 * 4 + 2 * 3 * 8);
        MeshLoader.release_cpu_data(&mut mesh);
        assert_eq!(MeshLoader.cpu_bytes(&mesh), 0);
        let skin = mesh.skin.as_ref().expect("the skin stays");
        assert_eq!(skin.joint_names, ["root", "tip"]);
        assert!(skin.joints.is_empty() && skin.weights.is_empty());
        assert_eq!(skin.joints.capacity(), 0, "the allocation goes");
    }

    #[test]
    fn the_mesh_loader_accepts_every_cooked_mesh_form_and_nothing_else() {
        assert!(MeshLoader.accepts(AssetTypeTag::mesh()));
        assert!(MeshLoader.accepts(AssetTypeTag::gpu_mesh()));
        assert!(MeshLoader.accepts(AssetTypeTag::mesh_legacy_cpu()));
        assert!(!MeshLoader.accepts(AssetTypeTag::texture()));
        assert!(TextureLoader.accepts(AssetTypeTag::texture()));
        assert!(!TextureLoader.accepts(AssetTypeTag::image()));
    }
}
