// SPDX-License-Identifier: MPL-2.0
//! Opt-in authored and generated LOD loading (compact meshes get MikkTSpace tangents at load, ADR-8025).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use regia_asset::{CookedModelPart, GpuMeshLod};

use super::*;
use crate::{MaterialImage, SharedImage};

/// Every authored part of one cooked glTF, in cooked manifest order.
#[derive(Clone, Debug, Default)]
pub struct CookedModelLods {
    /// Parts, including authored LODs above zero. Node names need not be unique.
    pub parts: Vec<CookedModelLodPart>,
    /// Complete resolved material table across all parts, in first-use order.
    /// Images share residency across every model in the loader call.
    pub materials: Vec<LabMaterial>,
    /// Cooked identities parallel to [`Self::materials`]; `None` is the default material.
    pub material_ids: Vec<Option<AssetId>>,
}

/// One authored part, with a single vertex array and all generated index LODs.
#[derive(Clone, Debug)]
pub struct CookedModelLodPart {
    /// Exact cooked node name (for example `Hull_LOD1`). Unnamed or duplicate nodes keep
    /// the cook's names; use [`Self::mesh_id`] to distinguish their primitives.
    pub node: String,
    /// Stable cooked mesh sub-asset identity, including the primitive within the node.
    pub mesh_id: AssetId,
    /// Authored node LOD from the case-insensitive `_LOD<n>` suffix, otherwise zero.
    pub authored_lod: u32,
    /// Index into the enclosing [`CookedModelLods::materials`] table.
    pub material: usize,
    /// GPU-ready vertices, including tangents: copied from a GPU-ready cooked mesh, or built with
    /// MikkTSpace tangents from a compact one.
    pub vertices: Vec<LabVertex>,
    /// Complete generated index buffer; ranges in [`Self::index_lods`] address this array.
    pub indices: Vec<u32>,
    /// Generated index LOD ranges and cooked screen thresholds, independent of authored LOD.
    /// A mesh without a LOD table gets one range covering its complete index buffer.
    pub index_lods: Vec<GpuMeshLod>,
}

impl CookedModelLodPart {
    /// Select a generated index LOD. Returns `None` for an absent level or invalid range,
    /// including a range the caller edited after loading. Does not copy vertices or indices.
    #[must_use]
    pub fn lod_indices(&self, lod: usize) -> Option<&[u32]> {
        let range = self.index_lods.get(lod)?;
        let start = usize::try_from(range.first_index).ok()?;
        let end = start.checked_add(usize::try_from(range.index_count).ok()?)?;
        self.indices.get(start..end)
    }

    /// Copy a selected generated index LOD into a rigid draw with the part's material index.
    /// Vertex/tangent bytes are preserved; no tangent generation occurs. Returns `None`
    /// under the same conditions as [`Self::lod_indices`]. The host adds its material-table
    /// offset when combining models into a scene.
    #[must_use]
    pub fn lab_mesh(&self, lod: usize) -> Option<LabMesh> {
        let indices = self.lod_indices(lod)?;
        Some(LabMesh {
            vertices: self.vertices.clone(),
            indices: indices.to_vec(),
            material: self.material,
            ..LabMesh::unskinned()
        })
    }
}

/// Errors from [`load_cooked_models_all_lods`], separate from the legacy loader's errors.
#[derive(Debug, thiserror::Error)]
pub enum CookedModelLodsError {
    /// A model, material or mesh failed to load or validate.
    #[error(transparent)]
    Load(#[from] CookedModelError),
    /// The manifest contains no drawable parts at any authored LOD.
    #[error("model {0} has no parts")]
    Empty(AssetId),
}

/// Load every authored part and generated index LOD of several cooked glTFs, in input order.
///
/// Material tables include every authored LOD's materials (distinct asset identities remain
/// distinct). Every referenced texture is decoded once across the call. Shared image handles
/// let a renderer upload each image once per color space across the batch; packed detail/aux
/// images also share between uses of the same material. Loading itself performs no GPU work.
/// Image overrides retain the full cooked mip chain and bypass automatic material streaming.
///
/// Both mesh forms load. A GPU-ready mesh (a desktop pak built with `REGIA_PAK_GPU_MESHES=1`)
/// is copied verbatim, tangents included. A compact mesh (the default cook, the loose dev cache,
/// every Android pak, and skinned or colored meshes in any pak) goes through the legacy decode
/// once: MikkTSpace tangents over LOD 0 for the one shared vertex array, and the compact mesh's
/// own generated LOD table, exactly as [`gpu_mesh_from_cooked`] converts it offline. Skinned
/// meshes draw rigid in their bind pose and `COLOR_0` is not applied, as in [`load_cooked_models`].
/// A batch may mix both forms. Meshes and successfully decoded textures are reported through
/// `assets.status` via [`AssetServer::note_external_load`], without retaining another mesh
/// copy in the server.
///
/// Shared image handles are built from the maps [`LabMaterial::from_cooked`] already fitted to
/// the active tier's resident cap (`crate::texture_budget::resident_cap`), so a shared image
/// uploads no larger than the legacy loader's maps. A block-compressed chain is shared without
/// copying its levels; a material that drew the neutral fallback gets no shared images.
///
/// # Errors
///
/// A model/material/mesh cannot load or validate (any index of any generated LOD out of range
/// included), or a model has no parts. A missing or invalid texture warns and uses the same
/// fallback as the legacy loader.
#[tracing::instrument(skip_all, fields(models = gltfs.len()))]
pub fn load_cooked_models_all_lods(
    server: &mut AssetServer,
    gltfs: &[AssetId],
) -> Result<Vec<CookedModelLods>, CookedModelLodsError> {
    let mut models = Vec::with_capacity(gltfs.len());
    for &gltf in gltfs {
        let id = model_id(gltf);
        let model = server.load_model(id).map_err(|source| CookedModelError::Asset {
            what: "model",
            id,
            source,
        })?;
        if model.parts.is_empty() {
            return Err(CookedModelLodsError::Empty(id));
        }
        models.push(model.parts);
    }
    let mut materials = HashMap::new();
    let mut material_order = Vec::new();
    let mut texture_ids = Vec::new();
    let mut seen_textures = HashSet::new();
    for mid in models.iter().flatten().filter_map(|part| part.material) {
        if materials.contains_key(&mid) {
            continue;
        }
        let handle = server
            .load_material(mid)
            .map_err(|source| CookedModelError::Asset {
                what: "material",
                id: mid,
                source,
            })?;
        let cooked = server.materials().get(&handle).cloned();
        if let Some(cooked) = &cooked {
            for tid in texture_slot_ids(&cooked.authored) {
                if seen_textures.insert(tid) {
                    texture_ids.push(tid);
                }
            }
        }
        material_order.push(mid);
        materials.insert(mid, cooked);
    }
    let mesh_ids: Vec<_> = models.iter().flatten().map(|part| part.mesh).collect();
    let reader = server.payload_reader();
    let (meshes, textures) = decode_all_lods_batch(&reader, &mesh_ids, &texture_ids);
    let mut slots = HashMap::new();
    for (id, (slot, source)) in texture_ids.into_iter().zip(textures) {
        if let Some(source) = source {
            server.note_external_load(id, source);
        }
        slots.insert(id, slot);
    }
    // Build once per material across models, so derived packed images share their identities too.
    let mut built = HashMap::new();
    let mut shared = HashMap::new();
    for mid in material_order {
        let cooked = materials.get(&mid).and_then(Option::as_ref);
        let mut material = cooked_material(mid, cooked, &slots);
        if let Some(cooked) = cooked {
            share_material_images(&mut material, &cooked.authored, &mut shared);
        }
        built.insert(mid, material);
    }
    let mut meshes = meshes.into_iter();
    let mut output = Vec::with_capacity(models.len());
    for parts in models {
        let mut model = CookedModelLods::default();
        let mut material_indices = HashMap::new();
        for CookedModelPart { mesh, material, node } in parts {
            let index = *material_indices.entry(material).or_insert_with(|| {
                let index = model.materials.len();
                model.materials.push(match material {
                    Some(mid) => built
                        .get(&mid)
                        .cloned()
                        .unwrap_or_else(LabMaterial::neutral_fallback),
                    None => LabMaterial::default(),
                });
                model.material_ids.push(material);
                index
            });
            // The decode batch returns one result for each input, in order.
            let (mut part, source) = meshes.next().ok_or(CookedModelLodsError::Empty(mesh))??;
            part.authored_lod = lod_of(&node);
            part.node = node;
            part.material = index;
            server.note_external_load(mesh, source);
            model.parts.push(part);
        }
        output.push(model);
    }
    Ok(output)
}

type DecodedLods = Result<(CookedModelLodPart, AssetSource), CookedModelLodsError>;

fn decode_all_lods_batch(
    reader: &PayloadReader,
    meshes: &[AssetId],
    textures: &[AssetId],
) -> (Vec<DecodedLods>, Vec<DecodedTexture>) {
    let jobs: Vec<(Option<DecodedLods>, Option<DecodedTexture>)> = regia_core::tasks::scope(|scope| {
        for &id in meshes {
            scope.spawn(async move { (Some(decode_lods(reader, id)), None) });
        }
        for &id in textures {
            scope.spawn(async move { (None, Some(decode_texture(reader, id))) });
        }
    });
    let mut meshes = Vec::with_capacity(meshes.len());
    let mut textures = Vec::with_capacity(textures.len());
    for (mesh, texture) in jobs {
        meshes.extend(mesh);
        textures.extend(texture);
    }
    (meshes, textures)
}

fn decode_lods(reader: &PayloadReader, id: AssetId) -> DecodedLods {
    let _span = tracing::info_span!("cooked_mesh_all_lods_decode", %id).entered();
    let err = |source: AssetLoadError| CookedModelError::Asset {
        what: "mesh",
        id,
        source,
    };
    let (bytes, source) = reader.read(id).map_err(err)?;
    let (header, payload) = regia_asset::decode_cooked_file(&bytes).map_err(|e| err(e.into()))?;
    let (vertices, indices, mut lods) = if header.type_tag == AssetTypeTag::gpu_mesh().0 {
        let view = GpuMeshView::parse(payload).map_err(|e| err(e.into()))?;
        let indices: Vec<u32> = view
            .indices
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| u32::from_le_bytes(*chunk))
            .collect();
        let vertices: Vec<LabVertex> = bytemuck::pod_collect_to_vec(view.vertices);
        (vertices, indices, view.info.lods.clone())
    } else if header.type_tag == AssetTypeTag::mesh().0 {
        // The legacy decode, once: MikkTSpace over LOD 0 for the one vertex array every
        // generated LOD shares, and the compact mesh's own LOD table.
        let mesh = CookedMesh::from_postcard(payload).map_err(|e| err(e.into()))?;
        let (info, vertices, indices) = gpu_mesh_from_cooked(&mesh).map_err(|e| err(e.into()))?;
        (vertices, indices, info.lods)
    } else {
        // A legacy `CpuMesh` entry has no LOD table; anything else is the legacy type error.
        let loaded = regia_asset::load_mesh_bytes(&bytes).map_err(err)?;
        let mesh = lab_mesh(&loaded.mesh, 0);
        (mesh.vertices, mesh.indices, Vec::new())
    };
    // Both GPU-ready payloads and `gpu_mesh_from_cooked` carry `COLOR_0` in the color slot
    // (ADR-7919); the cooked draw keeps it white like `lab_mesh_from_gpu` and the legacy loader
    // (`specs/13` § 8), so a colored mesh draws the same through either loader.
    let mut vertices = vertices;
    for v in &mut vertices {
        v.color = [1.0; 4];
    }
    let vertex_count = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
    if let Some((at, &index)) = indices
        .iter()
        .enumerate()
        .find(|&(_, &index)| index >= vertex_count)
    {
        return Err(err(GpuMeshError::IndexOutOfRange {
            index,
            at,
            vertex_count,
        }
        .into())
        .into());
    }
    if lods.is_empty() {
        lods.push(GpuMeshLod {
            screen_relative_size: 1.0,
            first_index: 0,
            index_count: u32::try_from(indices.len()).unwrap_or(u32::MAX),
        });
    }
    let part = CookedModelLodPart {
        node: String::new(),
        mesh_id: id,
        authored_lod: 0,
        material: 0,
        vertices,
        indices,
        index_lods: lods,
    };
    if let Some(form) = regia_asset::cooked_pack::MeshForm::of(header.type_tag) {
        reader.note_mesh_form(form, &source);
    }
    Ok((part, source))
}

/// A shared handle for one fitted slot. A block-compressed chain wins (its levels are views of
/// the same cooked bytes the material holds, so sharing copies no texels); otherwise the fitted
/// RGBA8 pixels. `None` when the material has neither.
fn shared_image(
    rgba: Option<&LabTexture>,
    chain: Option<&CompressedTexture>,
    srgb: bool,
) -> Option<MaterialImage> {
    let built = match (chain, rgba) {
        (Some(chain), _) => SharedImage::new(
            chain.width,
            chain.height,
            Vec::new(),
            Some(Arc::new(chain.clone())),
            srgb,
        ),
        (None, Some(t)) => SharedImage::rgba(t.width, t.height, t.rgba.clone(), srgb),
        (None, None) => return None,
    };
    match built {
        Ok(image) => Some(MaterialImage::Shared(image)),
        Err(error) => {
            tracing::warn!(%error, "cooked image cannot be shared");
            None
        }
    }
}

/// Shared handles for the four base slots, one per (image, color space) across the batch.
///
/// Built from the maps [`LabMaterial::from_cooked`] left on `material`, which are already
/// fitted to the resident cap; the frame binds `images` instead of those maps and does not
/// fit again. A slot the material does not draw (the neutral fallback, a map that did not
/// load) gets no handle. The packed detail and aux slots share only a cooked chain the
/// material already holds (the `Arc` itself); RGBA8 detail and aux images keep the legacy
/// per-material packing at upload, so no packed copy stays in RAM.
fn share_material_images(
    material: &mut LabMaterial,
    authored: &regia_asset::MaterialAssetRon,
    shared: &mut HashMap<(AssetId, bool), Option<MaterialImage>>,
) {
    let maps = material.compressed.clone();
    let maps = maps.as_deref();
    let base = [
        (
            &authored.base.base_color_map,
            material.albedo.as_ref(),
            maps.and_then(|m| m.albedo.as_ref()),
            true,
        ),
        (
            &authored.base.normal_map,
            material.normal_map.as_ref(),
            maps.and_then(|m| m.normal.as_ref()),
            false,
        ),
        (
            &authored.base.orm_map,
            material.occlusion_map.as_ref(),
            maps.and_then(|m| m.occlusion.as_ref()),
            false,
        ),
        (
            &authored.base.emissive_map,
            material.emissive_map.as_ref(),
            maps.and_then(|m| m.emissive.as_ref()),
            true,
        ),
    ];
    let [albedo, normal, occlusion, emissive] = base.map(|(slot, rgba, chain, srgb)| {
        let Some(TextureSlot::Asset(id)) = slot else {
            return None;
        };
        if rgba.is_none() && chain.is_none() {
            return None;
        }
        shared
            .entry((*id, srgb))
            .or_insert_with(|| shared_image(rgba, chain, srgb))
            .clone()
    });
    let packed = |chain: Option<&Arc<CompressedTexture>>| {
        let chain = chain?;
        SharedImage::new(
            chain.width,
            chain.height,
            Vec::new(),
            Some(Arc::clone(chain)),
            false,
        )
        .ok()
        .map(MaterialImage::Shared)
    };
    material.images.detail = packed(maps.and_then(|m| m.detail.as_ref()));
    material.images.detail_normal = packed(maps.and_then(|m| m.detail_normal.as_ref()));
    material.images.aux = packed(maps.and_then(|m| m.aux.as_ref()));
    material.images.albedo = albedo;
    material.images.normal = normal;
    material.images.occlusion = occlusion;
    material.images.emissive = emissive;
}
