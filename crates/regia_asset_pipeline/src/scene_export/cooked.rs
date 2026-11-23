// SPDX-License-Identifier: MPL-2.0
//! The cook as the export's second source (ADR-3016): when a mesh, material or texture file is gone,
//! the cooked sub-assets of the project's dev cache stand in for it.
//!
//! A cooked file is found by its asset id. The `.meta` beside the missing source names the ids; with
//! the `.meta` gone too, the asset database's row names the primary id and the sub-asset ids follow from
//! it. The caches are `.regia/cooked/<platform>/`, the host's first, then the others by name.
//!
//! What a cook keeps is what comes back: positions, normals, tangents, the first UV set and vertex colors
//! of LOD 0, the cooked material's factors and maps, level 0 of each cooked texture decoded to PNG. A skin,
//! morph targets, a second UV set and the source's node hierarchy are not in the cooked mesh, so they are
//! left out, each with a warning.

use std::path::{Path, PathBuf};

use regia_asset::{
    AssetId, CookedImage, CookedMesh, CookedModel, CookedStandardMaterial, CookedTexture, MetaSidecar,
    SubAssetEntry, cooked_file_name, decode_cooked_file,
};
use regia_core::id::TypePathHash;

use super::{Exporter, SrcPrim, encode_png, safe_relative};
use crate::Forward;
use crate::gltf_write::GlbTextureRef;

/// A source file's cooked identity.
struct CookedAsset {
    id: AssetId,
    sub_assets: Vec<SubAssetEntry>,
}

impl CookedAsset {
    /// The id of a sub-asset: the `.meta`'s (it survives renames), else the derived one.
    fn sub_id(&self, path: &str) -> AssetId {
        self.sub_assets
            .iter()
            .find(|entry| entry.path == path)
            .map_or_else(|| crate::subasset_remap::sub_id(self.id, path), |entry| entry.id)
    }
}

/// `.regia/cooked/<platform>/`: the host's first, then the others by name, so a run repeats.
fn cooked_dirs(root: &Path) -> Vec<PathBuf> {
    let host = regia_asset::cooked_cache_dir(root, crate::platform::host_platform());
    let mut dirs = Vec::new();
    if host.is_dir() {
        dirs.push(host.clone());
    }
    let parent = regia_core::brand::project_state_dir(root).join("cooked");
    let mut others: Vec<PathBuf> = std::fs::read_dir(parent)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir() && *path != host)
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    dirs.extend(others);
    dirs
}

impl Exporter<'_> {
    /// The cook caches of the project, read once.
    fn cooked_dirs(&mut self) -> &[PathBuf] {
        self.cooked_dirs
            .get_or_insert_with(|| cooked_dirs(self.root))
            .as_slice()
    }

    /// The header's type tag and the payload of a cooked file, from the first cache that has it.
    fn cooked_payload(&mut self, id: AssetId) -> Option<(TypePathHash, Vec<u8>)> {
        let name = cooked_file_name(id);
        for dir in self.cooked_dirs().to_vec() {
            let Ok(bytes) = std::fs::read(dir.join(&name)) else {
                continue;
            };
            match decode_cooked_file(&bytes) {
                Ok((header, payload)) => return Some((header.type_tag, payload.to_vec())),
                Err(error) => {
                    self.warnings.push(format!("cooked_unreadable: {name}: {error}"));
                }
            }
        }
        None
    }

    /// The cooked identity of a project path whose file may be gone: its `.meta`, else the database row.
    fn cooked_asset(&mut self, path: &str) -> Option<CookedAsset> {
        let rel = safe_relative(path)?;
        for base in [self.root.join(&rel), self.root.join("assets").join(&rel)] {
            let name = base.file_name().and_then(|n| n.to_str())?.to_owned();
            let meta = base.with_file_name(format!("{name}.meta"));
            if let Ok(bytes) = std::fs::read(&meta)
                && let Ok(meta) = MetaSidecar::from_ron(&bytes)
            {
                return Some(CookedAsset {
                    id: meta.id,
                    sub_assets: meta.sub_assets,
                });
            }
        }
        // Read only when it is there: opening the database creates its directory.
        if !regia_asset::asset_db_path(self.root).is_file() {
            return None;
        }
        let db = crate::AssetDatabase::open(self.root).ok()?;
        let under_assets = format!("assets/{path}");
        db.entries
            .values()
            .find(|row| row.source_path == path || row.source_path == under_assets)
            .map(|row| CookedAsset {
                id: row.asset_id,
                sub_assets: Vec::new(),
            })
    }

    /// One LOD 0 part of a cooked model, in the exporter's axes, with its material's id.
    fn cooked_part(&mut self, mesh_path: &str, id: AssetId) -> Option<SrcPrim> {
        let (_, payload) = self.cooked_payload(id)?;
        let cpu = CookedMesh::from_postcard(&payload)
            .ok()
            .and_then(|mesh| mesh.to_cpu_mesh().ok());
        let Some(cpu) = cpu else {
            self.warnings
                .push(format!("cooked_unreadable: `{mesh_path}` mesh {id}"));
            return None;
        };
        if cpu.skin.is_some() || !cpu.morph_targets.is_empty() {
            self.warnings.push(format!(
                "mesh_from_cooked_rigid: `{mesh_path}`: the skin and morph targets of the cooked mesh are not exported"
            ));
        }
        // A cooked vertex already has the importer's half turn. `Forward::Keep` writes source vertices
        // without it, so a cooked vertex gives it back.
        let turn = |v: [f32; 3]| match self.options.forward {
            Forward::Convert => v,
            Forward::Keep => [-v[0], v[1], -v[2]],
        };
        Some(SrcPrim {
            positions: cpu.positions.iter().map(|v| turn(*v)).collect(),
            normals: cpu.normals.iter().map(|v| turn(*v)).collect(),
            tangents: cpu
                .tangents
                .iter()
                .map(|t| {
                    let [x, y, z] = turn([t[0], t[1], t[2]]);
                    [x, y, z, t[3]]
                })
                .collect(),
            uvs: vec![cpu.uvs],
            colors: cpu.colors,
            joints: Vec::new(),
            weights: Vec::new(),
            indices: cpu.indices,
            material: None,
        })
    }

    /// The mesh of `mesh_path` rebuilt from the cook, or `None` when the cache does not have it.
    /// `override_material` wins over the cooked materials, as it does for a source.
    pub(super) fn cooked_mesh(
        &mut self,
        mesh_path: &str,
        override_material: Option<usize>,
        entity: &str,
    ) -> Option<usize> {
        let asset = self.cooked_asset(mesh_path)?;
        let (_, model) = self.cooked_payload(asset.sub_id("model"))?;
        let model = match CookedModel::from_postcard(&model) {
            Ok(model) => model,
            Err(error) => {
                self.warnings
                    .push(format!("cooked_unreadable: `{mesh_path}` model: {error}"));
                return None;
            }
        };
        let mut parts = Vec::with_capacity(model.parts.len());
        for part in &model.parts {
            // A part of a higher LOD repeats the model at less detail.
            if matches!(crate::gltf_conventions::parse_lod_name(&part.node), Some((_, level)) if level > 0) {
                continue;
            }
            let Some(prim) = self.cooked_part(mesh_path, part.mesh) else {
                continue;
            };
            let material = match override_material {
                Some(index) => Some(index),
                None => part.material.and_then(|id| self.cooked_material(id)),
            };
            parts.push((prim, material));
        }
        if parts.is_empty() {
            return None;
        }
        self.warnings.push(format!(
            "mesh_from_cooked: `{entity}` names `{mesh_path}`, which is missing; the mesh comes from the cook"
        ));
        let name = Path::new(mesh_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("mesh")
            .to_owned();
        Some(self.push_mesh(name, parts))
    }

    /// A cooked material sub-asset as a glTF material.
    fn cooked_material(&mut self, id: AssetId) -> Option<usize> {
        let key = format!("cooked:{id}");
        if let Some(found) = self.materials.get(&key) {
            return Some(*found);
        }
        let (_, payload) = self.cooked_payload(id)?;
        let cooked = match CookedStandardMaterial::from_postcard(&payload) {
            Ok(cooked) => cooked,
            Err(error) => {
                self.warnings
                    .push(format!("cooked_unreadable: material {id}: {error}"));
                return None;
            }
        };
        Some(self.authored_material(key, &cooked.authored, ""))
    }

    /// A `.material.ron` that is gone, from its cooked form.
    pub(super) fn cooked_material_at(&mut self, path: &str) -> Option<usize> {
        let asset = self.cooked_asset(path)?;
        let material = self.cooked_material(asset.id)?;
        self.warnings.push(format!(
            "material_from_cooked: `{path}` is missing; the material comes from the cook"
        ));
        Some(material)
    }

    /// Level 0 of a cooked texture or image as an embedded PNG.
    pub(super) fn cooked_texture(&mut self, id: AssetId) -> Option<GlbTextureRef> {
        let key = format!("cooked:{id}");
        if let Some(image) = self.images.get(&key) {
            return Some(GlbTextureRef {
                image: *image,
                tex_coord: 0,
            });
        }
        let (tag, payload) = self.cooked_payload(id)?;
        let image = if tag == TypePathHash::of("regia_asset::CookedImage") {
            CookedImage::from_postcard(&payload)
                .ok()
                .and_then(|image| image::RgbaImage::from_raw(image.width, image.height, image.rgba8))
        } else {
            CookedTexture::from_postcard(&payload)
                .ok()
                .and_then(|texture| regia_render::CompressedTexture::from_cooked(&texture).ok())
                .and_then(|texture| texture.decode_rgba8())
                .and_then(|level| image::RgbaImage::from_raw(level.width, level.height, level.rgba))
        };
        let png = image.and_then(|image| encode_png(&image::DynamicImage::ImageRgba8(image)));
        let Some(png) = png else {
            self.warnings.push(format!(
                "texture_skipped: cooked texture {id} cannot be decoded (an HDR format has no RGBA8 form)"
            ));
            return None;
        };
        let image = self.image(&key, || Some((png, "image/png")))?;
        self.warnings.push(format!(
            "texture_from_cooked: texture {id} has no source; level 0 of the cooked texture is embedded"
        ));
        Some(GlbTextureRef { image, tex_coord: 0 })
    }

    /// A texture path whose file is gone, through its `.meta`.
    pub(super) fn cooked_texture_at(&mut self, path: &str) -> Option<GlbTextureRef> {
        let asset = self.cooked_asset(path)?;
        self.cooked_texture(asset.id)
    }
}
