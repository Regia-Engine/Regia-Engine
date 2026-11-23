// SPDX-License-Identifier: MPL-2.0
//! A cooked model from the runtime asset server: the meshes, materials and block-compressed
//! textures `regia cook` wrote for a glTF, rebuilt as [`LabMesh`]es and [`LabMaterial`]s with no
//! source file present (a shipped game reading its pak).
//!
//! The glTF's `model` sub-asset ([`regia_asset::CookedModel`]) lists each primitive's cooked
//! mesh and material. Meshes keep their node transforms (baked by the cook, with its forward-axis
//! conversion) and LOD 0 only; tangents are rebuilt with MikkTSpace (glTF 2.0 § 3.7.2.1: a
//! normal-mapped primitive without tangents uses MikkTSpace), because the cooked vertex stream
//! keeps only the bitangent sign. Materials go through [`LabMaterial::from_cooked`]: a texture
//! slot naming a `CookedTexture` keeps its cooked mip chain ([`crate::CompressedTexture`]), a
//! `CookedImage` (too small to block-compress) becomes an RGBA8 [`LabTexture`].
//!
//! The part lists and materials load through the asset server; then the meshes and the
//! materials' textures decode together on the compute pool ([`regia_core::tasks`]), each
//! texture read once ([`load_cooked_models`] batches several models). A desktop pak stores each mesh GPU-ready ([`regia_asset::gpu_mesh`]:
//! [`LabVertex`]es with their tangents, `u32` indices), which loads with no decode;
//! [`gpu_mesh_from_cooked`] is the migrator that builds that form from the compact mesh, exactly as
//! this module builds a [`LabMesh`] from it at load (old paks, Android).
//!
//! Hosts that retain authored LOD chains use [`load_cooked_models_all_lods`]. That opt-in path
//! takes GPU-ready meshes as stored and builds compact meshes (old paks, Android) with MikkTSpace
//! tangents at load, as the legacy loaders do; it keeps every authored part's material assignment
//! and every generated index LOD. The legacy loaders stay LOD-zero-only.

use std::collections::{HashMap, HashSet};
use std::mem::offset_of;

use regia_asset::gpu_mesh::gpu_vertex;
use regia_asset::{
    AssetId, AssetLoadError, AssetServer, AssetSource, AssetTypeTag, CookedIndexWidth, CookedMesh,
    CookedMeshValidationError, CookedStandardMaterial, CpuMesh, GPU_VERTEX_STRIDE, GpuMeshError, GpuMeshInfo,
    GpuMeshLod, GpuMeshView, PayloadReader, StrongHandle, TextureSlot,
};

use crate::hard_surface::ResolvedBaseMaps;
use crate::{CompressedMaps, CompressedTexture, FrameMesh, LabMaterial, LabMesh, LabTexture, LabVertex};

mod all_lods;
pub use all_lods::{CookedModelLodPart, CookedModelLods, CookedModelLodsError, load_cooked_models_all_lods};

/// Errors from [`load_cooked_model`].
#[derive(Debug, thiserror::Error)]
pub enum CookedModelError {
    /// An asset did not load (missing from every pak and the cache, or corrupt).
    #[error("{what} {id}: {source}")]
    Asset {
        /// What was being loaded.
        what: &'static str,
        /// Its id.
        id: AssetId,
        /// The asset server's error.
        source: regia_asset::AssetLoadError,
    },
    /// The model lists no drawable part.
    #[error("model {0} has no LOD 0 parts")]
    Empty(AssetId),
}

/// The draws of one cooked model.
#[derive(Clone, Debug, Default)]
pub struct CookedModelDraws {
    /// One mesh per LOD 0 part; [`LabMesh::material`] indexes [`Self::materials`].
    pub meshes: Vec<LabMesh>,
    /// One material per distinct cooked material (a part without one gets the default).
    pub materials: Vec<LabMaterial>,
    /// GPU bytes of the cooked texture chains the materials carry (block-compressed).
    pub texture_bytes: u64,
}

/// The `model` sub-asset id of an imported glTF (`AssetId::sub_asset(gltf, "model")`).
#[must_use]
pub fn model_id(gltf: AssetId) -> AssetId {
    AssetId::sub_asset(gltf, "model")
}

/// LOD index of a glTF node name (`*_LOD<n>`, any case); `0` when it names no LOD.
fn lod_of(node: &str) -> u32 {
    let lower = node.to_ascii_lowercase();
    lower
        .rfind("_lod")
        .and_then(|at| lower[at + 4..].parse::<u32>().ok())
        .unwrap_or(0)
}

// The GPU-ready layout is `LabVertex`, byte for byte.
const _: () = {
    assert!(size_of::<LabVertex>() == GPU_VERTEX_STRIDE as usize);
    assert!(offset_of!(LabVertex, position) == gpu_vertex::POSITION);
    assert!(offset_of!(LabVertex, normal) == gpu_vertex::NORMAL);
    assert!(offset_of!(LabVertex, uv) == gpu_vertex::UV);
    assert!(offset_of!(LabVertex, tangent) == gpu_vertex::TANGENT);
    assert!(offset_of!(LabVertex, color) == gpu_vertex::COLOR);
    assert!(offset_of!(LabVertex, joints) == gpu_vertex::JOINTS);
    assert!(offset_of!(LabVertex, weights) == gpu_vertex::WEIGHTS);
};

/// Loads every LOD 0 part of the cooked glTF `gltf` (the glTF's own asset id; its `model`
/// sub-asset is read) through `server`: paks first, then the cooked cache.
///
/// The meshes and the materials' textures decode on the compute pool ([`load_cooked_models`]);
/// they are not kept in the server's mesh or texture storage (the returned [`LabMesh`]es and
/// [`LabMaterial`]s are the only CPU copy), and are marked with
/// [`AssetServer::note_external_load`].
///
/// # Errors
///
/// The model, a part's mesh or a part's material fails to load, or the model has no LOD 0
/// part. A texture that fails to load is a warning: the material draws without it (or as the
/// neutral fallback for a missing base colour, as [`LabMaterial::from_cooked`] does).
#[tracing::instrument(skip(server))]
pub fn load_cooked_model(
    server: &mut AssetServer,
    gltf: AssetId,
) -> Result<CookedModelDraws, CookedModelError> {
    load_cooked_models(server, &[gltf])?
        .pop()
        .ok_or_else(|| CookedModelError::Empty(model_id(gltf)))
}

/// [`load_cooked_model`] for several glTFs at once, in order. Their part lists and materials load
/// through `server` first (small), then every model's meshes and every distinct texture of their
/// materials decode in one batch on the compute pool, so a level's models share one parallel
/// batch and a texture several of them use is read and decoded once (ADR-530).
///
/// # Errors
///
/// As [`load_cooked_model`]: the first error, in this order: every model's part list, then
/// every material, then the meshes in model order.
#[tracing::instrument(skip_all, fields(models = gltfs.len()))]
pub fn load_cooked_models(
    server: &mut AssetServer,
    gltfs: &[AssetId],
) -> Result<Vec<CookedModelDraws>, CookedModelError> {
    let models = model_parts(server, gltfs)?;
    let (materials, texture_ids) = model_materials(server, &models)?;
    let mesh_ids: Vec<AssetId> = models
        .iter()
        .flat_map(|(_, parts)| parts.iter().map(|&(mesh, _)| mesh))
        .collect();
    let reader = server.payload_reader();
    let (decoded_meshes, decoded_textures) = decode_batch(&reader, &mesh_ids, &texture_ids);
    let slots = texture_slots(server, &texture_ids, decoded_textures);

    let mut decoded_meshes = decoded_meshes.into_iter();
    let mut out = Vec::with_capacity(models.len());
    for (id, parts) in models {
        let mut draws = CookedModelDraws::default();
        let mut material_index: HashMap<Option<AssetId>, usize> = HashMap::new();
        for (mesh_id, material) in parts {
            let index = material_slot(
                &mut draws.materials,
                &mut draws.texture_bytes,
                &mut material_index,
                material,
                &materials,
                &slots,
            );
            // One decode result per part, in part order.
            let Some(result) = decoded_meshes.next() else {
                return Err(CookedModelError::Empty(id));
            };
            let (mut mesh, source) = result?;
            mesh.material = index;
            server.note_external_load(mesh_id, source);
            draws.meshes.push(mesh);
        }
        out.push(draws);
    }
    Ok(out)
}

/// One model's id and its LOD 0 parts `(mesh, material)`.
type ModelParts = (AssetId, Vec<(AssetId, Option<AssetId>)>);

/// Every model's LOD 0 part list, in order.
fn model_parts(server: &mut AssetServer, gltfs: &[AssetId]) -> Result<Vec<ModelParts>, CookedModelError> {
    let mut models = Vec::with_capacity(gltfs.len());
    for &gltf in gltfs {
        let id = model_id(gltf);
        let model = server.load_model(id).map_err(|source| CookedModelError::Asset {
            what: "model",
            id,
            source,
        })?;
        let parts: Vec<(AssetId, Option<AssetId>)> = model
            .parts
            .iter()
            .filter(|p| lod_of(&p.node) == 0)
            .map(|p| (p.mesh, p.material))
            .collect();
        if parts.is_empty() {
            return Err(CookedModelError::Empty(id));
        }
        models.push((id, parts));
    }
    Ok(models)
}

/// Each distinct cooked material of `models` once (`None`: it decoded to nothing), keyed by id,
/// and the distinct textures their slots name, in first-use order.
type ModelMaterials = (HashMap<AssetId, Option<CookedStandardMaterial>>, Vec<AssetId>);

/// Each distinct cooked material of `models` once (`None`: it decoded to nothing), and the
/// distinct textures their slots name, in first-use order.
fn model_materials(
    server: &mut AssetServer,
    models: &[ModelParts],
) -> Result<ModelMaterials, CookedModelError> {
    let mut materials: HashMap<AssetId, Option<CookedStandardMaterial>> = HashMap::new();
    let mut texture_ids = Vec::new();
    let mut seen_textures = HashSet::new();
    for mid in models
        .iter()
        .flat_map(|(_, parts)| parts.iter().filter_map(|&(_, material)| material))
    {
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
        materials.insert(mid, cooked);
    }
    Ok((materials, texture_ids))
}

/// The decoded texture slots by id; each loaded one is marked with
/// [`AssetServer::note_external_load`].
fn texture_slots(
    server: &mut AssetServer,
    texture_ids: &[AssetId],
    decoded: Vec<DecodedTexture>,
) -> HashMap<AssetId, Slot> {
    let mut slots = HashMap::with_capacity(texture_ids.len());
    for (id, (slot, source)) in texture_ids.iter().zip(decoded) {
        if let Some(source) = source {
            server.note_external_load(*id, source);
        }
        slots.insert(*id, slot);
    }
    slots
}

/// The index in `materials` of a part's material, built (and its block-compressed bytes added to
/// `texture_bytes`) the first time the model uses it.
fn material_slot(
    materials: &mut Vec<LabMaterial>,
    texture_bytes: &mut u64,
    material_index: &mut HashMap<Option<AssetId>, usize>,
    material: Option<AssetId>,
    cooked: &HashMap<AssetId, Option<CookedStandardMaterial>>,
    slots: &HashMap<AssetId, Slot>,
) -> usize {
    if let Some(&i) = material_index.get(&material) {
        return i;
    }
    let built = match material {
        Some(mid) => cooked_material(mid, cooked.get(&mid).and_then(Option::as_ref), slots),
        None => LabMaterial::default(),
    };
    *texture_bytes += built.compressed.as_ref().map_or(0, |c| c.bytes());
    materials.push(built);
    let i = materials.len() - 1;
    material_index.insert(material, i);
    i
}

/// One part of a [`CountedCookedModel`].
#[derive(Clone, Debug)]
pub struct CountedCookedPart {
    /// The part's mesh as a counted asset (`AssetServer::load::<CpuMesh>`): while a handle lives
    /// the mesh stays loaded; with an [`crate::AssetMeshUploads`] registered its GPU buffers are
    /// the only copy unless a reader asked for `keep_cpu_data`.
    pub mesh: StrongHandle<CpuMesh>,
    /// Index into [`CountedCookedModel::materials`].
    pub material: usize,
}

/// A cooked model whose meshes are counted assets drawn from [`crate::AssetMeshUploads`]
/// ([`load_cooked_models_counted`], ADR-7284).
#[derive(Clone, Debug, Default)]
pub struct CountedCookedModel {
    /// One part per LOD 0 part of the model.
    pub parts: Vec<CountedCookedPart>,
    /// One material per distinct cooked material (a part without one gets the default), as
    /// [`CookedModelDraws::materials`].
    pub materials: Vec<LabMaterial>,
    /// A frame material key per entry of [`Self::materials`], unique to this load.
    pub material_keys: Vec<u64>,
    /// GPU bytes of the cooked texture chains the materials carry (block-compressed).
    pub texture_bytes: u64,
}

/// Top byte of [`CountedCookedModel::material_keys`] (`'C'`).
const COUNTED_MATERIAL_KEY_TAG: u64 = 0x4300_0000_0000_0000;

impl CountedCookedModel {
    /// Appends one [`FrameMesh`] per part to `out`: the part's [`crate::asset_mesh_key`] with no
    /// CPU vertices (the frame renderer draws the buffers of the
    /// [`crate::FrameRenderer::set_asset_meshes`] uploader), drawn with `model`. Object keys are
    /// `object_key + part index`. Appending allocates only when `out` grows.
    pub fn push_frame_meshes<'a>(
        &'a self,
        model: [[f32; 4]; 4],
        object_key: u64,
        out: &mut Vec<FrameMesh<'a>>,
    ) {
        for (i, part) in self.parts.iter().enumerate() {
            let Some(material) = self.materials.get(part.material) else {
                continue;
            };
            out.push(FrameMesh {
                mesh_key: crate::asset_mesh_key(part.mesh.id()),
                material_key: self.material_keys.get(part.material).copied().unwrap_or(0),
                object_key: object_key.wrapping_add(i as u64),
                vertices: &[],
                indices: &[],
                material,
                model,
                joints: &[],
                receiver_layer: 0,
                shadow: crate::ShadowCast::All,
                uv1: &[],
                lod_dither: crate::LOD_DITHER_NONE,
                is_static: false,
            });
        }
    }
}

/// [`load_cooked_models`] with every part's mesh loaded as a counted asset
/// (`AssetServer::load::<CpuMesh>`) instead of decoded into a [`LabMesh`] (ADR-7284). Register an
/// [`crate::AssetMeshUploads`] with the server first: each mesh is then uploaded once in the frame
/// renderer's vertex layout, its CPU streams are dropped unless a reader asked for
/// `keep_cpu_data`, `assets.handles` and the per-type budget count it once, and it unloads (its
/// GPU buffers freed) after the grace frames once the last handle drops. Materials and their
/// textures load as [`load_cooked_models`] loads them.
///
/// The mesh loads run on the asset server's IO pool; this waits for them
/// ([`AssetServer::wait_for_loads`], which also completes other loads in flight).
///
/// # Errors
///
/// As [`load_cooked_models`]: a model, material or mesh that does not load.
#[tracing::instrument(skip_all, fields(models = gltfs.len()))]
pub fn load_cooked_models_counted(
    server: &mut AssetServer,
    gltfs: &[AssetId],
) -> Result<Vec<CountedCookedModel>, CookedModelError> {
    let models = model_parts(server, gltfs)?;
    let (materials, texture_ids) = model_materials(server, &models)?;
    let reader = server.payload_reader();
    let (_, decoded_textures) = decode_batch(&reader, &[], &texture_ids);
    let slots = texture_slots(server, &texture_ids, decoded_textures);
    Ok(load_counted_parts(server, &models, &materials, &slots)?
        .into_iter()
        .map(|(model, _)| model)
        .collect())
}

/// A counted model and the index of each cooked material in its list.
type CountedParts = (CountedCookedModel, HashMap<Option<AssetId>, usize>);

/// Each model's parts as counted meshes and its materials built from `slots`, with the index of
/// each cooked material in the model's list; waits for the mesh loads.
fn load_counted_parts(
    server: &mut AssetServer,
    models: &[ModelParts],
    materials: &HashMap<AssetId, Option<CookedStandardMaterial>>,
    slots: &HashMap<AssetId, Slot>,
) -> Result<Vec<CountedParts>, CookedModelError> {
    static NEXT_MATERIAL_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut out = Vec::with_capacity(models.len());
    for (_, parts) in models {
        let mut model = CountedCookedModel::default();
        let mut material_index: HashMap<Option<AssetId>, usize> = HashMap::new();
        for &(mesh_id, material) in parts {
            let index = material_slot(
                &mut model.materials,
                &mut model.texture_bytes,
                &mut material_index,
                material,
                materials,
                slots,
            );
            let mesh = server
                .load::<CpuMesh>(mesh_id)
                .map_err(|source| CookedModelError::Asset {
                    what: "mesh",
                    id: mesh_id,
                    source,
                })?;
            model.parts.push(CountedCookedPart {
                mesh,
                material: index,
            });
        }
        model.material_keys = (0..model.materials.len())
            .map(|_| {
                let n = NEXT_MATERIAL_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                COUNTED_MATERIAL_KEY_TAG | (n << 3)
            })
            .collect();
        out.push((model, material_index));
    }
    server.wait_for_loads();
    for (model, _) in &out {
        for part in &model.parts {
            let id = part.mesh.id();
            if let regia_asset::LoadState::Failed(text) = server.state(id) {
                return Err(CookedModelError::Asset {
                    what: "mesh",
                    id,
                    source: AssetLoadError::Invalid(text),
                });
            }
        }
    }
    Ok(out)
}

/// [`load_cooked_models_counted`] with the materials' base maps (base color, normal, ORM,
/// emissive) loaded as counted textures too (ADR-7330): each distinct texture is a counted
/// `CookedTexture` (KTX2) or `CookedImage` (RGBA8) load that `textures` (registered with the
/// server: [`crate::AssetTextureUploads::cooked_textures`] and
/// [`crate::AssetTextureUploads::images`]) uploads once, in sRGB for base color and emissive and
/// linear for normal and ORM, and the material binds it through
/// [`crate::MaterialImage::Asset`]: no CPU copy, one GPU texture for every material that names
/// it, counted by the asset server and freed after the grace once the last material holding it
/// is gone. The other slots (detail, thin film, parallax, edge wear) are decoded on the CPU as in
/// [`load_cooked_models`]: the frame packs them.
///
/// # Errors
///
/// As [`load_cooked_models_counted`]; a base map that does not load draws as missing (a warning),
/// as on the CPU path.
#[tracing::instrument(skip_all, fields(models = gltfs.len()))]
pub fn load_cooked_models_counted_textured(
    server: &mut AssetServer,
    gltfs: &[AssetId],
    textures: &crate::AssetTextureUploads,
) -> Result<Vec<CountedCookedModel>, CookedModelError> {
    let models = model_parts(server, gltfs)?;
    let (cooked, _) = model_materials(server, &models)?;
    // Base-map ids by color space, and the ids other slots name (decoded on the CPU).
    let mut base: Vec<(AssetId, bool)> = Vec::new();
    let mut seen = HashSet::new();
    let mut other_ids = Vec::new();
    let mut other_seen = HashSet::new();
    for authored in cooked.values().flatten().map(|m| &m.authored) {
        let b = &authored.base;
        for (slot, srgb) in [
            (&b.base_color_map, true),
            (&b.normal_map, false),
            (&b.orm_map, false),
            (&b.emissive_map, true),
        ] {
            if let Some(TextureSlot::Asset(id)) = slot
                && seen.insert(*id)
            {
                base.push((*id, srgb));
            }
        }
        // Every id a non-base slot names is decoded, also when a base slot of the same material
        // names it too (ADR-7428): the base slot binds the counted image, the other slot its
        // decoded copy, as on the CPU path. Skipping it left that slot the 1×1 white stand-in.
        for id in texture_slot_ids(authored).skip(base_slot_count(authored)) {
            if other_seen.insert(id) {
                other_ids.push(id);
            }
        }
    }
    let reader = server.payload_reader();
    let (_, decoded) = decode_batch(&reader, &[], &other_ids);
    let mut slots = texture_slots(server, &other_ids, decoded);
    // Counted loads of the base maps; the cooked header says which kind each is.
    let mut images: HashMap<AssetId, crate::AssetImage> = HashMap::new();
    for &(id, srgb) in &base {
        let is_texture = match reader
            .read(id)
            .map(|(bytes, _)| regia_asset::decode_cooked_file(&bytes).map(|(h, _)| h.type_tag))
        {
            Ok(Ok(tag)) => tag == AssetTypeTag::texture().0,
            Ok(Err(err)) => {
                tracing::warn!(target: "regia_render", %id, %err, "cooked texture did not load");
                continue;
            }
            Err(err) => {
                tracing::warn!(target: "regia_render", %id, %err, "cooked texture did not load");
                continue;
            }
        };
        textures.set_srgb(id, srgb);
        let image = if is_texture {
            server
                .load::<regia_asset::CookedTexture>(id)
                .map(|h| crate::AssetImage::texture(h, textures))
        } else {
            server
                .load::<regia_asset::CookedImage>(id)
                .map(|h| crate::AssetImage::image(h, textures))
        };
        match image {
            Ok(image) => {
                images.insert(id, image);
                // The material is built as the CPU path builds it (the same flags), then the
                // slot's own map is swapped for the counted image.
                slots.entry(id).or_insert_with(|| {
                    Slot::Rgba(LabTexture {
                        width: 1,
                        height: 1,
                        rgba: vec![255; 4],
                    })
                });
            }
            Err(err) => tracing::warn!(target: "regia_render", %id, %err, "cooked texture did not load"),
        }
    }
    let mut out = load_counted_parts(server, &models, &cooked, &slots)?;
    for (model, material_index) in &mut out {
        for (mid, &index) in material_index.iter() {
            let Some(authored) = mid
                .and_then(|mid| cooked.get(&mid)?.as_ref())
                .map(|m| &m.authored)
            else {
                continue;
            };
            let Some(material) = model.materials.get_mut(index) else {
                continue;
            };
            swap_in_counted_maps(material, &authored.base, &images);
        }
    }
    Ok(out.into_iter().map(|(model, _)| model).collect())
}

/// Replaces a built material's base maps with the counted images their slots name.
fn swap_in_counted_maps(
    material: &mut LabMaterial,
    base: &regia_asset::StandardMaterialParams,
    images: &HashMap<AssetId, crate::AssetImage>,
) {
    let counted = |slot: &Option<TextureSlot>| match slot {
        Some(TextureSlot::Asset(id)) => images.get(id).cloned().map(crate::MaterialImage::Asset),
        _ => None,
    };
    if let Some(image) = counted(&base.base_color_map) {
        material.albedo = None;
        material.images.albedo = Some(image);
    }
    if let Some(image) = counted(&base.normal_map) {
        material.normal_map = None;
        material.images.normal = Some(image);
    }
    if let Some(image) = counted(&base.orm_map) {
        material.occlusion_map = None;
        material.images.occlusion = Some(image);
    }
    if let Some(image) = counted(&base.emissive_map) {
        material.emissive_map = None;
        material.images.emissive = Some(image);
    }
}

/// How many of [`texture_slot_ids`]' first ids are the base maps' (base color, normal, ORM,
/// emissive asset slots): the rest name the detail, thin-film, parallax and edge-wear slots.
fn base_slot_count(authored: &regia_asset::MaterialAssetRon) -> usize {
    let b = &authored.base;
    [&b.base_color_map, &b.normal_map, &b.orm_map, &b.emissive_map]
        .into_iter()
        .filter(|slot| matches!(slot, Some(TextureSlot::Asset(_))))
        .count()
}

/// The asset ids a material's texture slots name, in slot order.
fn texture_slot_ids(authored: &regia_asset::MaterialAssetRon) -> impl Iterator<Item = AssetId> + '_ {
    let detail = authored.hard_surface.as_ref().and_then(|h| h.detail.as_ref());
    let film = authored.thin_film.as_ref();
    let hard = authored.hard_surface.as_ref();
    [
        Some(&authored.base.base_color_map),
        Some(&authored.base.normal_map),
        Some(&authored.base.orm_map),
        Some(&authored.base.emissive_map),
        detail.map(|d| &d.albedo),
        detail.map(|d| &d.normal),
        detail.map(|d| &d.roughness),
        detail.map(|d| &d.mask),
    ]
    .into_iter()
    .flatten()
    .chain(film.map(|film| &film.factor_map))
    .chain(film.map(|film| &film.thickness_map))
    .chain(hard.and_then(|h| h.parallax.as_ref()).map(|p| &p.height_map))
    .chain(hard.and_then(|h| h.edge_wear.as_ref()).map(|w| &w.curvature_map))
    .filter_map(|slot| match slot {
        Some(TextureSlot::Asset(id)) => Some(*id),
        Some(TextureSlot::Path(_)) | None => None,
    })
}

type DecodedMesh = Result<(LabMesh, AssetSource), CookedModelError>;

/// A decoded texture slot and where its bytes came from (`None`: it did not load).
type DecodedTexture = (Slot, Option<AssetSource>);

/// Decodes `meshes` and `textures` together on the compute pool; each result list is in its
/// input's order.
#[tracing::instrument(skip_all, fields(meshes = meshes.len(), textures = textures.len()))]
fn decode_batch(
    reader: &PayloadReader,
    meshes: &[AssetId],
    textures: &[AssetId],
) -> (Vec<DecodedMesh>, Vec<DecodedTexture>) {
    if meshes.len() + textures.len() < 2 {
        return (
            meshes.iter().map(|&id| decode_mesh(reader, id)).collect(),
            textures.iter().map(|&id| decode_texture(reader, id)).collect(),
        );
    }
    // Every job yields one of the two; the pool returns them in spawn order.
    let jobs: Vec<(Option<DecodedMesh>, Option<DecodedTexture>)> = regia_core::tasks::scope(|scope| {
        for &id in meshes {
            scope.spawn(async move { (Some(decode_mesh(reader, id)), None) });
        }
        for &id in textures {
            scope.spawn(async move { (None, Some(decode_texture(reader, id))) });
        }
    });
    let mut decoded_meshes = Vec::with_capacity(meshes.len());
    let mut decoded_textures = Vec::with_capacity(textures.len());
    for (mesh, texture) in jobs {
        decoded_meshes.extend(mesh);
        decoded_textures.extend(texture);
    }
    (decoded_meshes, decoded_textures)
}

/// One mesh as a [`LabMesh`] (material 0): a GPU-ready payload is copied as is; a compact one is
/// decoded and gets MikkTSpace tangents.
fn decode_mesh(reader: &PayloadReader, id: AssetId) -> DecodedMesh {
    decode_mesh_mapped(reader, id).map(|(mesh, source, _)| (mesh, source))
}

/// `decode_mesh` that also returns the bytes of the entry read through a pak's memory mapping
/// (`0` when the payload was copied or came from the cache): the host memory budget counts them
/// (`crate::host_memory`, ADR-7891).
pub(crate) fn decode_mesh_counted(
    reader: &PayloadReader,
    id: AssetId,
) -> Result<(LabMesh, u64), CookedModelError> {
    decode_mesh_mapped(reader, id).map(|(mesh, _, mapped)| (mesh, mapped))
}

fn decode_mesh_mapped(
    reader: &PayloadReader,
    id: AssetId,
) -> Result<(LabMesh, AssetSource, u64), CookedModelError> {
    let _span = tracing::info_span!("cooked_mesh_decode").entered();
    let err = |source: AssetLoadError| CookedModelError::Asset {
        what: "mesh",
        id,
        source,
    };
    let (bytes, source) = reader.read(id).map_err(err)?;
    let mapped = if bytes.is_mapped() { bytes.len() as u64 } else { 0 };
    let (header, payload) = regia_asset::decode_cooked_file(&bytes).map_err(|e| err(e.into()))?;
    let mesh = if header.type_tag == AssetTypeTag::gpu_mesh().0 {
        GpuMeshView::parse(payload)
            .and_then(|view| lab_mesh_from_gpu(&view))
            .map_err(|e| err(e.into()))?
    } else {
        let loaded = regia_asset::load_mesh_bytes(&bytes).map_err(err)?;
        lab_mesh(&loaded.mesh, 0)
    };
    if let Some(form) = regia_asset::cooked_pack::MeshForm::of(header.type_tag) {
        reader.note_mesh_form(form, &source);
    }
    Ok((mesh, source, mapped))
}

/// One cooked mesh by id, as a [`LabMesh`] (LOD 0), on any thread: the HLOD proxy path, which has no
/// model to load through.
///
/// # Errors
///
/// The asset does not read or decode.
pub fn load_cooked_mesh(reader: &PayloadReader, id: AssetId) -> Result<LabMesh, CookedModelError> {
    decode_mesh(reader, id).map(|(mesh, _)| mesh)
}

/// One cooked standard material by id with its textures decoded, on any thread: the baked HLOD
/// proxy atlas path (ADR-2483), which has no model to load through. Slots as the model path
/// resolves them (a texture that does not load is a warning and the slot draws without it).
///
/// # Errors
///
/// The material does not read or decode.
pub(crate) fn load_cooked_material(
    reader: &PayloadReader,
    id: AssetId,
) -> Result<LabMaterial, CookedModelError> {
    let _span = tracing::info_span!("cooked_material_decode").entered();
    let err = |source: AssetLoadError| CookedModelError::Asset {
        what: "material",
        id,
        source,
    };
    let (bytes, _) = reader.read(id).map_err(err)?;
    let (_, cooked) = regia_asset::load_material_bytes(&bytes).map_err(err)?;
    let slots: HashMap<AssetId, Slot> = texture_slot_ids(&cooked.authored)
        .map(|tid| (tid, decode_texture(reader, tid).0))
        .collect();
    Ok(cooked_material(id, Some(&cooked), &slots))
}

/// One cooked standard material as a cooked model's part draws it: its scalars and every texture
/// slot it names, read through `server` (paks first, then the cooked cache). A texture that does
/// not load is a warning and the slot draws without it. Used by the terrain mesh-layer chunks
/// (ADR-8451).
///
/// # Errors
///
/// The material does not read or decode.
pub fn load_cooked_standard_material(
    server: &AssetServer,
    id: AssetId,
) -> Result<LabMaterial, CookedModelError> {
    load_cooked_material(&server.payload_reader(), id)
}

/// A GPU-ready mesh as a [`LabMesh`] (LOD 0, material 0): one copy of the vertex bytes, no
/// decode. Vertex colors are white, as the compact path draws them (`specs/13` § 8: the forward
/// shader multiplies `LabVertex::color` into base color, so `COLOR_0`, which the payload carries
/// for `AssetServer::load_mesh`, would tint existing cooks; ADR-7919).
///
/// # Errors
///
/// An index past the last vertex.
pub fn lab_mesh_from_gpu(view: &GpuMeshView<'_>) -> Result<LabMesh, GpuMeshError> {
    let mut vertices: Vec<LabVertex> = bytemuck::pod_collect_to_vec(view.vertices);
    for v in &mut vertices {
        v.color = [1.0; 4];
    }
    Ok(LabMesh {
        vertices,
        indices: view.lod_indices(0)?,
        ..LabMesh::unskinned()
    })
}

/// The migrator from the compact cooked mesh to the GPU-ready layout (version
/// [`regia_asset::GPU_MESH_VERSION`]): bounds, every LOD's indices, and the vertices exactly as
/// [`load_cooked_model`] builds them from the compact form (decoded normals and UVs, MikkTSpace
/// tangents over LOD 0), so both forms draw the same pixels. The one difference is the color slot:
/// it holds the mesh's `COLOR_0` (white without one) so `AssetServer::load_mesh` keeps the stream,
/// and [`lab_mesh_from_gpu`] draws it white like the compact path (ADR-7919).
///
/// # Errors
///
/// The compact mesh is invalid (stream sizes, index alignment, an index out of range).
pub fn gpu_mesh_from_cooked(
    mesh: &CookedMesh,
) -> Result<(GpuMeshInfo, Vec<LabVertex>, Vec<u32>), CookedMeshValidationError> {
    let cpu = mesh.to_cpu_mesh()?;
    let mut lab = lab_mesh(&cpu, 0);
    // `to_cpu_mesh` validated the stream: one color per vertex, or none.
    for (v, c) in lab.vertices.iter_mut().zip(&cpu.colors) {
        v.color = *c;
    }
    let width = match mesh.index_width {
        CookedIndexWidth::U16 => 2,
        CookedIndexWidth::U32 => 4,
    };
    let bad = || CookedMeshValidationError::StreamSize {
        vertices: mesh.vertex_count,
    };
    let indices: Vec<u32> = match mesh.index_width {
        CookedIndexWidth::U16 => mesh
            .indices
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u32::from(u16::from_le_bytes(*c)))
            .collect(),
        CookedIndexWidth::U32 => mesh
            .indices
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect(),
    };
    if indices.iter().any(|&i| i >= mesh.vertex_count) {
        return Err(bad());
    }
    let mut lods = Vec::with_capacity(mesh.lods.len().max(1));
    for (i, lod) in mesh.lods.iter().enumerate() {
        let (offset, len) = (lod.index_byte_offset as usize, lod.index_byte_length as usize);
        if !offset.is_multiple_of(width) || !len.is_multiple_of(width) || offset + len > mesh.indices.len() {
            return Err(CookedMeshValidationError::IndexAlignment { len, width });
        }
        let index_count = (len / width) as u32;
        if !index_count.is_multiple_of(3) {
            // The GPU-ready layout only draws triangle lists (`GpuMeshView::parse` rejects
            // anything else); refuse to write a file the loader would discard.
            return Err(CookedMeshValidationError::NotTriangles {
                lod: i,
                count: index_count,
            });
        }
        lods.push(GpuMeshLod {
            screen_relative_size: lod.screen_relative_size,
            first_index: (offset / width) as u32,
            index_count,
        });
    }
    if lods.is_empty() {
        let index_count = indices.len() as u32;
        if !index_count.is_multiple_of(3) {
            return Err(CookedMeshValidationError::NotTriangles {
                lod: 0,
                count: index_count,
            });
        }
        lods.push(GpuMeshLod {
            screen_relative_size: 1.0,
            first_index: 0,
            index_count,
        });
    }
    let info = GpuMeshInfo {
        aabb_min: mesh.aabb_min,
        aabb_max: mesh.aabb_max,
        bounding_sphere: mesh.bounding_sphere,
        lods,
    };
    Ok((info, lab.vertices, indices))
}

/// [`gpu_mesh_from_cooked`] over a whole cooked file: the GPU-ready cooked file for a compact
/// mesh file (same asset id, importer version and dependencies), `None` for any other payload.
/// The pak writer runs this on every mesh of a desktop pak opted in to GPU-ready meshes.
///
/// Every compact mesh converts. `COLOR_0` goes into the vertex color slot (ADR-7919). A skin
/// (joint names, both influence sets, `skin_influences`) and morph targets go into the payload's
/// trailing block (GPU mesh layout v3, ADR-10080), so `AssetServer::load_mesh` of the entry returns
/// them as it returns the compact file's; the vertex joint and weight slots stay zero, as the
/// cooked draw path draws such a mesh rigid.
///
/// # Errors
///
/// The file is a compact mesh that does not decode or validate.
pub fn gpu_mesh_file_from_cooked(file: &[u8]) -> Result<Option<Vec<u8>>, AssetLoadError> {
    let (header, payload) = regia_asset::decode_cooked_file(file)?;
    if header.type_tag != AssetTypeTag::mesh().0 {
        return Ok(None);
    }
    let mesh = CookedMesh::from_postcard(payload)?;
    let (info, vertices, indices) = gpu_mesh_from_cooked(&mesh)?;
    let extras =
        (mesh.skin.is_some() || !mesh.morph_targets.is_empty()).then(|| regia_asset::GpuMeshExtras {
            skin: mesh.skin.clone(),
            skin_influences: mesh.skin_influences,
            morph_targets: mesh.morph_targets.clone(),
        });
    let file = regia_asset::encode_gpu_mesh_file_parts(
        header.asset_id,
        header.importer_version,
        header.dependencies,
        &regia_asset::GpuMeshParts {
            info: &info,
            submeshes: &[],
            vertices: bytemuck::cast_slice(&vertices),
            indices: &indices,
            extras: extras.as_ref(),
        },
    )?;
    Ok(Some(file))
}

/// A loaded texture slot: a cooked chain or a small RGBA8 image.
#[derive(Clone)]
enum Slot {
    Compressed(CompressedTexture),
    Rgba(LabTexture),
    Missing,
}

/// One texture slot from its cooked file, read once: a block-compressed chain
/// (`CookedTexture`, its levels views of the pak entry) or, for an image too small to
/// block-compress, an RGBA8 `CookedImage`; the cooked header's type says which. A slot that does
/// not load is a warning and draws as missing.
fn decode_texture(reader: &PayloadReader, id: AssetId) -> DecodedTexture {
    let _span = tracing::info_span!("cooked_texture_decode").entered();
    let _slow = regia_core::telemetry::SlowLoad::start("texture", &id);
    let (bytes, source) = match reader.read(id) {
        Ok(read) => read,
        Err(err) => {
            tracing::warn!(target: "regia_render", %id, %err, "cooked texture did not load");
            return (Slot::Missing, None);
        }
    };
    let is_texture = match regia_asset::decode_cooked_file(&bytes) {
        Ok((header, _)) => header.type_tag == AssetTypeTag::texture().0,
        Err(err) => {
            tracing::warn!(target: "regia_render", %id, %err, "cooked texture did not load");
            return (Slot::Missing, None);
        }
    };
    let slot = if is_texture {
        match regia_asset::load_texture_shared(&bytes).map(|(_, tex)| CompressedTexture::from_cooked(&tex)) {
            Ok(Ok(t)) => {
                reader.note_texture(&source);
                Slot::Compressed(t)
            }
            Ok(Err(err)) => {
                tracing::warn!(target: "regia_render", %id, %err, "cooked texture is not a format the renderer samples");
                Slot::Missing
            }
            Err(err) => {
                tracing::warn!(target: "regia_render", %id, %err, "cooked texture did not load");
                Slot::Missing
            }
        }
    } else {
        match regia_asset::load_image_bytes(&bytes) {
            Ok((_, img)) => {
                reader.note_image(&source);
                Slot::Rgba(LabTexture {
                    width: img.width,
                    height: img.height,
                    rgba: img.rgba8,
                })
            }
            Err(err) => {
                tracing::warn!(target: "regia_render", %id, %err, "cooked texture did not load");
                Slot::Missing
            }
        }
    };
    let loaded = !matches!(slot, Slot::Missing);
    (slot, loaded.then_some(source))
}

/// The decoded slot a material's texture slot names (`missing` for none, or a path slot: it names
/// a source file, and a pak build has none).
fn slot_of<'a>(slot: &Option<TextureSlot>, slots: &'a HashMap<AssetId, Slot>, missing: &'a Slot) -> &'a Slot {
    match slot {
        Some(TextureSlot::Asset(id)) => slots.get(id).unwrap_or(missing),
        Some(TextureSlot::Path(_)) | None => missing,
    }
}

/// The material `id` from its cooked form (`None`: the neutral fallback) with its texture slots
/// taken from `slots` (decoded by [`decode_texture`]).
fn cooked_material(
    id: AssetId,
    cooked: Option<&CookedStandardMaterial>,
    slots: &HashMap<AssetId, Slot>,
) -> LabMaterial {
    let Some(cooked) = cooked else {
        return LabMaterial::neutral_fallback();
    };
    let authored = &cooked.authored;
    let missing = Slot::Missing;
    let detail = authored.hard_surface.as_ref().and_then(|h| h.detail.as_ref());
    let detail_slot = |pick: fn(&regia_asset::DetailMapParams) -> &Option<TextureSlot>| {
        detail.map_or(&missing, |d| slot_of(pick(d), slots, &missing))
    };
    let detail_slots = [
        detail_slot(|d| &d.albedo),
        detail_slot(|d| &d.normal),
        detail_slot(|d| &d.roughness),
        detail_slot(|d| &d.mask),
    ];
    let film_slots = slots;
    let slots = [
        slot_of(&authored.base.base_color_map, slots, &missing),
        slot_of(&authored.base.normal_map, slots, &missing),
        slot_of(&authored.base.orm_map, slots, &missing),
        slot_of(&authored.base.emissive_map, slots, &missing),
    ];
    let rgba = |s: &Slot| match s {
        Slot::Rgba(t) => Some(t.clone()),
        _ => None,
    };
    let block = |s: &Slot| match s {
        Slot::Compressed(t) => Some(t.clone()),
        _ => None,
    };
    let base = ResolvedBaseMaps {
        albedo: rgba(slots[0]),
        normal: rgba(slots[1]),
        occlusion: rgba(slots[2]),
        emissive: rgba(slots[3]),
    };
    let compressed = CompressedMaps {
        albedo: block(slots[0]),
        normal: block(slots[1]),
        occlusion: block(slots[2]),
        emissive: block(slots[3]),
        // A block-compressed detail map is the packed one: albedo with roughness in alpha, normal
        // with the mask in alpha (`docs/formats/gltf-extensions.md`).
        detail: block(detail_slots[0]).map(std::sync::Arc::new),
        detail_normal: block(detail_slots[1]).map(std::sync::Arc::new),
        ..CompressedMaps::default()
    };
    let name = authored.name.clone().unwrap_or_else(|| id.to_string());
    let mut material = LabMaterial::from_cooked(&name, authored, base, compressed);
    material.detail_albedo = rgba(detail_slots[0]);
    material.detail_normal = rgba(detail_slots[1]);
    material.detail_roughness = rgba(detail_slots[2]);
    material.detail_mask = rgba(detail_slots[3]);
    if let Some(hs) = &authored.hard_surface {
        // Parallax height and edge-wear curvature resolve like any other slot; a
        // block-compressed slot decodes (the same rule the thin-film maps use) and the
        // frame packs height (R) and curvature (G) into the aux texture.
        let resolved = |slot: &Option<TextureSlot>| match slot_of(slot, film_slots, &missing) {
            Slot::Rgba(t) => Some(t.clone()),
            Slot::Compressed(t) => t.decode_rgba8(),
            Slot::Missing => None,
        };
        if let Some(pom) = &hs.parallax {
            material.height_map = resolved(&pom.height_map);
        }
        if let Some(wear) = &hs.edge_wear {
            material.curvature_map = resolved(&wear.curvature_map);
        }
    }
    if let Some(film) = &authored.thin_film {
        // The film's map packs two cooked images into one, so a block-compressed slot decodes.
        let decoded = |slot: &Option<TextureSlot>| match slot_of(slot, film_slots, &missing) {
            Slot::Rgba(t) => Some(t.clone()),
            Slot::Compressed(t) => t.decode_rgba8(),
            Slot::Missing => None,
        };
        let cap = crate::texture_budget::resident_cap();
        material.thin_film_map = crate::pack_thin_film_map(
            decoded(&film.factor_map).as_ref(),
            decoded(&film.thickness_map).as_ref(),
        )
        .map(|map| {
            if cap == 0 {
                map
            } else {
                crate::texture_budget::fit_texture(map, cap, false)
            }
        });
    }
    material
}

/// The lab vertices [`load_cooked_model`] draws for a cooked mesh (MikkTSpace tangents, white,
/// rigid): what [`crate::AssetMeshUploads`] uploads, so both paths draw the same bytes.
pub(crate) fn lab_vertices(cpu: &regia_asset::CpuMesh) -> Vec<LabVertex> {
    cooked_vertices(cpu, false)
}

/// The vertices [`crate::CpuMesh::from_cooked`] builds: [`lab_vertices`] with the skin's four
/// strongest influences (a skinned counted upload, ADR-8610).
pub(crate) fn skinned_lab_vertices(cpu: &regia_asset::CpuMesh) -> Vec<LabVertex> {
    cooked_vertices(cpu, true)
}

/// A cooked mesh as a lab mesh: MikkTSpace tangents, rigid, identity model matrix.
///
/// A skinned part draws rigid in its bind pose here: [`load_cooked_model`] has no joint palette,
/// and a vertex with weights but no palette would read another draw's joints. Animated
/// characters build their mesh with [`crate::CpuMesh::from_cooked`] and supply the palette.
fn lab_mesh(cpu: &regia_asset::CpuMesh, material: usize) -> LabMesh {
    LabMesh {
        vertices: cooked_vertices(cpu, false),
        indices: cpu.indices.clone(),
        material,
        ..LabMesh::unskinned()
    }
}

/// Lab vertices of a cooked mesh: MikkTSpace tangents, white vertex colour, and — when `skinned`
/// and the mesh has a skin — the four strongest joint influences
/// ([`regia_asset::CpuSkin::top4`]). `COLOR_0` is not applied: the cooked colour is carried for
/// materials that read it, and the lab shaders multiply vertex colour into base colour, which
/// would change existing cooked looks.
///
/// The tangents are the mesh's stored ones when [`stored_tangents_usable`] accepts them (a
/// GPU-ready pak entry stores the MikkTSpace tangents this function made at cook, so a load
/// skips the pass, ADR-10080); otherwise (the compact form keeps only the bitangent sign) they are
/// generated here.
fn cooked_vertices(cpu: &regia_asset::CpuMesh, skinned: bool) -> Vec<LabVertex> {
    let n = cpu.positions.len();
    let normals: Vec<[f32; 3]> = (0..n)
        .map(|i| cpu.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]))
        .collect();
    let uvs: Vec<[f32; 2]> = (0..n)
        .map(|i| cpu.uvs.get(i).copied().unwrap_or([0.0; 2]))
        .collect();
    let tangents = if stored_tangents_usable(cpu) {
        cpu.tangents.clone()
    } else {
        let mut tangents = vec![[1.0, 0.0, 0.0, 1.0]; n];
        crate::tangents::generate_vertex_tangents(
            &cpu.positions,
            &normals,
            &uvs,
            &cpu.indices,
            &mut tangents,
        );
        tangents
    };
    let skin = cpu.skin.as_ref().filter(|_| skinned);
    (0..n)
        .map(|i| {
            let (joints, weights) = skin.and_then(|s| s.top4(i)).unwrap_or(([0; 4], [0.0; 4]));
            LabVertex {
                tangent: tangents[i],
                joints,
                weights,
                ..LabVertex::from_pos_normal_uv(cpu.positions[i], normals[i], uvs[i])
            }
        })
        .collect()
}

/// Whether `cpu.tangents` are real tangents [`cooked_vertices`] may draw instead of running
/// MikkTSpace (ADR-10080): one per vertex, each a finite unit xyz (within 1e-3) with a ±1
/// bitangent sign, and not every one the `(1, 0, 0, ±1)` stand-in `CookedMesh::to_cpu_mesh`
/// writes for the compact form (which keeps only the sign). A mesh whose real tangents all are
/// `(1, 0, 0)` is regenerated, which gives the same tangents.
fn stored_tangents_usable(cpu: &regia_asset::CpuMesh) -> bool {
    let n = cpu.positions.len();
    if n == 0 || cpu.tangents.len() != n {
        return false;
    }
    let unit = |t: &[f32; 4]| {
        let len2 = t[0] * t[0] + t[1] * t[1] + t[2] * t[2];
        t.iter().all(|c| c.is_finite()) && (len2.sqrt() - 1.0).abs() <= 1e-3 && t[3].abs() == 1.0
    };
    cpu.tangents.iter().all(unit) && cpu.tangents.iter().any(|t| t[..3] != [1.0, 0.0, 0.0])
}

/// A cooked skin the renderer cannot draw skinned.
#[derive(Debug, thiserror::Error)]
pub enum CookedSkinError {
    /// More joints than one skin palette holds ([`crate::MAX_JOINTS_PER_SKIN`]).
    #[error("skin has {joints} joints; the renderer draws at most {max} per skin")]
    TooManyJoints {
        /// Joints in the cooked skin.
        joints: usize,
        /// [`crate::MAX_JOINTS_PER_SKIN`].
        max: usize,
    },
}

impl crate::CpuMesh {
    /// The render mesh of a cooked mesh (cooked mesh format v2). A skinned mesh keeps its
    /// joint indices (cooked skeleton joint ids) and weights (the four strongest influences,
    /// summing to 1), so it deforms with the palette of that skeleton; a rigid mesh has zero
    /// weights. Tangents are the mesh's stored MikkTSpace tangents when it has them (a
    /// GPU-ready pak entry, `GpuMeshView::to_cpu_mesh`: no pass at load, ADR-10080) and are
    /// rebuilt with MikkTSpace otherwise (the compact form). `COLOR_0` stays on the cooked mesh:
    /// every lab vertex is white, because the forward shader multiplies `LabVertex::color`
    /// into base color.
    ///
    /// # Errors
    ///
    /// [`CookedSkinError::TooManyJoints`] when the skin has more than
    /// [`crate::MAX_JOINTS_PER_SKIN`] joints (use [`Self::from_cooked_rigid`] to draw it
    /// unskinned).
    pub fn from_cooked(cpu: &regia_asset::CpuMesh) -> Result<Self, CookedSkinError> {
        if let Some(skin) = &cpu.skin
            && skin.joint_names.len() > crate::MAX_JOINTS_PER_SKIN
        {
            return Err(CookedSkinError::TooManyJoints {
                joints: skin.joint_names.len(),
                max: crate::MAX_JOINTS_PER_SKIN,
            });
        }
        Ok(Self {
            vertices: cooked_vertices(cpu, true),
            indices: cpu.indices.clone(),
        })
    }

    /// The render mesh of a cooked mesh with its skin ignored (zero weights).
    #[must_use]
    pub fn from_cooked_rigid(cpu: &regia_asset::CpuMesh) -> Self {
        Self {
            vertices: cooked_vertices(cpu, false),
            indices: cpu.indices.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_cooked_material_resolves_its_detail_slots() {
        let (albedo, normal) = (AssetId::new_random(), AssetId::new_random());
        let image = LabTexture {
            width: 1,
            height: 1,
            rgba: vec![128, 128, 255, 255],
        };
        let mut authored = regia_asset::MaterialAssetRon::default_standard();
        authored.hard_surface.get_or_insert_with(Default::default).detail =
            Some(regia_asset::DetailMapParams {
                albedo: Some(TextureSlot::Asset(albedo)),
                normal: Some(TextureSlot::Asset(normal)),
                uv_scale: [64.0, 32.0],
                ..regia_asset::DetailMapParams::default()
            });
        assert_eq!(texture_slot_ids(&authored).count(), 2, "the loader decodes both");
        let cooked = CookedStandardMaterial::new(authored);
        let slots = HashMap::from([
            (albedo, Slot::Rgba(image.clone())),
            (normal, Slot::Rgba(image.clone())),
        ]);
        let material = cooked_material(AssetId::new_random(), Some(&cooked), &slots);
        assert_eq!(material.detail_albedo.as_ref().map(|t| t.width), Some(1));
        assert_eq!(material.detail_normal.as_ref().map(|t| t.height), Some(1));
        assert!(material.detail_roughness.is_none() && material.detail_mask.is_none());
        assert_eq!(material.hard_surface.detail_uv_scale, [64.0, 32.0]);
    }

    #[test]
    fn a_cooked_material_resolves_height_and_curvature() {
        let (height, curvature) = (AssetId::new_random(), AssetId::new_random());
        let image = LabTexture {
            width: 1,
            height: 1,
            rgba: vec![128, 128, 255, 255],
        };
        let mut authored = regia_asset::MaterialAssetRon::default_standard();
        let hs = authored.hard_surface.get_or_insert_with(Default::default);
        hs.parallax = Some(regia_asset::ParallaxParams {
            height_map: Some(TextureSlot::Asset(height)),
            scale: 0.12,
            ..regia_asset::ParallaxParams::default()
        });
        hs.edge_wear = Some(regia_asset::EdgeWearParams {
            curvature_map: Some(TextureSlot::Asset(curvature)),
            intensity: 1.0,
            ..regia_asset::EdgeWearParams::default()
        });
        assert_eq!(texture_slot_ids(&authored).count(), 2, "the loader decodes both");
        let cooked = CookedStandardMaterial::new(authored);
        let slots = HashMap::from([
            (height, Slot::Rgba(image.clone())),
            (curvature, Slot::Rgba(image)),
        ]);
        let material = cooked_material(AssetId::new_random(), Some(&cooked), &slots);
        assert_eq!(material.height_map.as_ref().map(|t| t.width), Some(1));
        assert_eq!(material.curvature_map.as_ref().map(|t| t.height), Some(1));
        assert!(
            (material.hard_surface.pom_scale - 0.12).abs() < 1.0e-6,
            "the authored scale survives the cooked path"
        );
        assert!(
            (material.hard_surface.edge_intensity - 1.0).abs() < 1.0e-6,
            "the authored edge wear survives the cooked path"
        );
    }

    use super::*;
    use crate::{JointMatrix, skin_position};

    fn skinned_quad(joint_count: usize, eight: bool) -> regia_asset::CpuMesh {
        let last = (joint_count - 1) as u16;
        regia_asset::CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2, 2, 1, 3],
            skin: Some(regia_asset::CpuSkin {
                joint_names: (0..joint_count).map(|i| format!("j{i}")).collect(),
                joints: vec![[last, 0, 1, 2]; 4],
                weights: if eight {
                    vec![[30000, 5000, 5000, 5000]; 4]
                } else {
                    vec![[65535, 0, 0, 0]; 4]
                },
                joints1: if eight { vec![[3, 4, 5, 6]; 4] } else { Vec::new() },
                weights1: if eight {
                    vec![[10535, 5000, 5000, 0]; 4]
                } else {
                    Vec::new()
                },
                eight_influences: false,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn cooked_skin_reaches_the_render_vertices() {
        for joint_count in [67, 82, 256] {
            let cpu = skinned_quad(joint_count, false);
            let mesh = crate::CpuMesh::from_cooked(&cpu).expect("within the joint cap");
            let last = joint_count as u32 - 1;
            for vertex in &mesh.vertices {
                assert_eq!(vertex.joints[0], last, "{joint_count} joints");
                assert_eq!(vertex.weights, [1.0, 0.0, 0.0, 0.0]);
            }
            // The vertex follows its joint: a palette that lifts the last joint lifts the mesh.
            let mut palette = vec![JointMatrix::identity(); joint_count];
            palette[last as usize] = JointMatrix::translation(0.0, 2.0, 0.0);
            let v = mesh.vertices[3];
            let joints = v.joints.map(|j| palette[j as usize]);
            assert_eq!(skin_position(v.position, &joints, v.weights), [1.0, 3.0, 0.0]);
        }
    }

    #[test]
    fn eight_influences_draw_the_strongest_four() {
        let mesh = crate::CpuMesh::from_cooked(&skinned_quad(82, true)).expect("82 joints");
        let v = mesh.vertices[0];
        assert_eq!(v.joints, [81, 3, 0, 1]);
        let sum: f32 = v.weights.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
    }

    #[test]
    fn skins_past_the_joint_cap_are_refused_not_misdrawn() {
        let cpu = skinned_quad(300, false);
        assert!(matches!(
            crate::CpuMesh::from_cooked(&cpu),
            Err(CookedSkinError::TooManyJoints {
                joints: 300,
                max: 256
            })
        ));
        let rigid = crate::CpuMesh::from_cooked_rigid(&cpu);
        assert!(
            rigid
                .vertices
                .iter()
                .all(|v| v.weights == [0.0; 4] && v.joints == [0; 4])
        );
    }

    #[test]
    fn rigid_cooked_meshes_are_unchanged() {
        let mut cpu = skinned_quad(67, false);
        cpu.skin = None;
        let skinned_path = crate::CpuMesh::from_cooked(&cpu).expect("rigid");
        let lab = lab_mesh(&cpu, 0);
        assert_eq!(
            bytemuck::cast_slice::<LabVertex, u8>(&skinned_path.vertices),
            bytemuck::cast_slice::<LabVertex, u8>(&lab.vertices)
        );
        assert!(lab.vertices.iter().all(|v| v.weights == [0.0; 4]));
    }

    /// `COLOR_0` stays on the cooked asset. Lab vertices stay white so the forward
    /// multiply does not tint a cook that already shipped.
    #[test]
    fn cooked_color0_stays_on_the_asset_and_lab_vertices_stay_white() {
        let mut cpu = skinned_quad(1, false);
        cpu.skin = None;
        cpu.colors = vec![[1.0, 0.0, 0.0, 0.0]; 4];
        let mesh = crate::CpuMesh::from_cooked(&cpu).expect("rigid");
        let rigid = crate::CpuMesh::from_cooked_rigid(&cpu);
        assert!(mesh.vertices.iter().all(|v| v.color == [1.0, 1.0, 1.0, 1.0]));
        assert!(rigid.vertices.iter().all(|v| v.color == [1.0, 1.0, 1.0, 1.0]));
        assert_eq!(cpu.colors[0], [1.0, 0.0, 0.0, 0.0]);
    }

    /// A compact cooked quad (two LODs, 16-bit indices) as the pipeline packs it.
    fn compact_quad() -> CookedMesh {
        let positions = [
            [0.0f32, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.2],
        ];
        let uvs = [[0u16, 0], [0x3C00, 0], [0x3C00, 0x3C00], [0, 0x3800]];
        let mut mesh = CookedMesh {
            aabb_min: [0.0; 3],
            aabb_max: [1.0, 1.0, 0.2],
            bounding_sphere: [0.5, 0.5, 0.1, 0.8],
            stream_positions: Vec::new(),
            stream_normals_tangents: Vec::new(),
            stream_uv0: Vec::new(),
            indices: Vec::new(),
            index_width: CookedIndexWidth::U16,
            vertex_count: 4,
            lods: vec![
                regia_asset::CookedMeshLod {
                    screen_relative_size: 1.0,
                    index_byte_offset: 0,
                    index_byte_length: 12,
                },
                regia_asset::CookedMeshLod {
                    screen_relative_size: 0.5,
                    index_byte_offset: 12,
                    index_byte_length: 6,
                },
            ],
            submeshes: Vec::new(),
            lod_metric: regia_asset::LOD_METRIC_ERROR,
            format_version: regia_asset::COOKED_MESH_FORMAT_VERSION,
            skin: None,
            stream_color0: Vec::new(),
            morph_targets: Vec::new(),
            skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
        };
        for (p, uv) in positions.iter().zip(uvs) {
            for c in p {
                mesh.stream_positions.extend_from_slice(&c.to_le_bytes());
            }
            let (x, y) = regia_asset::encode_octahedral_normal([0.0, 0.2, 0.98]);
            for w in [x, y, 0, -1] {
                mesh.stream_normals_tangents.extend_from_slice(&w.to_le_bytes());
            }
            for h in uv {
                mesh.stream_uv0.extend_from_slice(&h.to_le_bytes());
            }
        }
        for i in [0u16, 1, 2, 0, 2, 3, 0, 1, 3] {
            mesh.indices.extend_from_slice(&i.to_le_bytes());
        }
        mesh
    }

    /// The migrator's GPU-ready mesh loads into exactly the `LabMesh` the compact form decodes
    /// to (same vertices, MikkTSpace tangents included, LOD 0 indices), so both forms draw the
    /// same pixels; the cooked file round-trips through the asset loader too.
    #[test]
    fn gpu_ready_mesh_loads_like_the_compact_one() {
        let compact = compact_quad();
        let slow = lab_mesh(&compact.to_cpu_mesh().expect("compact"), 0);
        assert!(
            slow.vertices.iter().any(|v| v.tangent[..3] != [1.0, 0.0, 0.0]),
            "the compact path generated real tangents"
        );

        let id = AssetId::new_random();
        let payload = compact.to_postcard().expect("postcard");
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh().0,
            asset_id: id,
            importer_version: 7,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = regia_asset::encode_cooked_file(&header, &payload).expect("file");
        let gpu_file = gpu_mesh_file_from_cooked(&file)
            .expect("migrate")
            .expect("a mesh");
        assert!(
            gpu_mesh_file_from_cooked(&gpu_file)
                .expect("not compact")
                .is_none()
        );

        let (gpu_header, gpu_payload) = regia_asset::decode_cooked_file(&gpu_file).expect("wrapper");
        assert_eq!(gpu_header.type_tag, AssetTypeTag::gpu_mesh().0);
        assert_eq!((gpu_header.asset_id, gpu_header.importer_version), (id, 7));
        let view = GpuMeshView::parse(gpu_payload).expect("parse");
        assert_eq!(view.info.lods.len(), 2);
        assert_eq!(view.lod_indices(1).expect("lod 1"), vec![0, 1, 3]);
        let fast = lab_mesh_from_gpu(&view).expect("gpu");
        assert_eq!(
            bytemuck::cast_slice::<LabVertex, u8>(&fast.vertices),
            bytemuck::cast_slice::<LabVertex, u8>(&slow.vertices)
        );
        assert_eq!(fast.indices, slow.indices);
        assert_eq!(fast.indices, vec![0, 1, 2, 0, 2, 3]);

        // `AssetServer::load_mesh` / tools read the GPU-ready file as a `CpuMesh`.
        let cpu = regia_asset::load_mesh_bytes(&gpu_file).expect("load").mesh;
        assert_eq!(cpu.indices, slow.indices);
        assert_eq!(cpu.tangents[1], slow.vertices[1].tangent);
    }

    /// A compact quad as a cooked mesh file (fresh id, importer version 1).
    fn compact_file(mesh: &CookedMesh) -> Vec<u8> {
        let payload = mesh.to_postcard().expect("postcard");
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh().0,
            asset_id: AssetId::new_random(),
            importer_version: 1,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        regia_asset::encode_cooked_file(&header, &payload).expect("file")
    }

    /// ADR-7919: a mesh with `COLOR_0` (no skin, no morph) converts to the GPU-ready form. The
    /// payload's COLOR slot holds the stream, so `load_mesh` of the pak entry returns the same
    /// colors as the compact file; `load_cooked_model` still draws it white (spec 13), so both
    /// forms draw the same vertices.
    #[test]
    fn a_colored_mesh_converts_and_keeps_color0() {
        let mut colored = compact_quad();
        let colors = [
            [1.0, 0.5, 0.25, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            [1.0; 4],
            [0.1, 0.2, 0.3, 0.4],
        ];
        colored.stream_color0 = regia_asset::pack_colors_unorm16(&colors);
        let file = compact_file(&colored);
        let gpu_file = gpu_mesh_file_from_cooked(&file)
            .expect("decode")
            .expect("a mesh with COLOR_0 converts");

        let compact = regia_asset::load_mesh_bytes(&file).expect("compact").mesh;
        let fast = regia_asset::load_mesh_bytes(&gpu_file).expect("gpu-ready").mesh;
        assert_eq!(compact.colors.len(), 4);
        assert_eq!(fast.colors, compact.colors, "COLOR_0 survives the GPU-ready form");
        assert_eq!(fast.positions, compact.positions);

        let (_, payload) = regia_asset::decode_cooked_file(&gpu_file).expect("wrapper");
        let view = GpuMeshView::parse(payload).expect("parse");
        let drawn = lab_mesh_from_gpu(&view).expect("gpu");
        let slow = lab_mesh(&compact, 0);
        assert_eq!(
            bytemuck::cast_slice::<LabVertex, u8>(&drawn.vertices),
            bytemuck::cast_slice::<LabVertex, u8>(&slow.vertices),
            "load_cooked_model draws both forms alike"
        );
        assert!(drawn.vertices.iter().all(|v| v.color == [1.0; 4]));
    }

    /// A mesh without `COLOR_0` converts with white COLOR slots and loads with no colors, as
    /// before ADR-7919.
    #[test]
    fn a_mesh_without_color0_stays_white() {
        let gpu_file = gpu_mesh_file_from_cooked(&compact_file(&compact_quad()))
            .expect("decode")
            .expect("converts");
        let (_, payload) = regia_asset::decode_cooked_file(&gpu_file).expect("wrapper");
        let view = GpuMeshView::parse(payload).expect("parse");
        for v in view.vertices.as_chunks::<{ GPU_VERTEX_STRIDE as usize }>().0 {
            let color: [f32; 4] = bytemuck::pod_read_unaligned(&v[gpu_vertex::COLOR..gpu_vertex::COLOR + 16]);
            assert_eq!(color, [1.0; 4]);
        }
        let cpu = regia_asset::load_mesh_bytes(&gpu_file).expect("load").mesh;
        assert!(cpu.colors.is_empty());
    }

    /// The cooked skin and morph target of the GPU mesh v3 tests: an eight-influence skin (both
    /// sets sum to 65535 per vertex) and a sparse target with normal deltas (values exact in f16).
    fn skin8_and_morph() -> (regia_asset::CookedSkin, regia_asset::CookedMorphTarget) {
        let skin = regia_asset::CpuSkin {
            joint_names: vec!["root".into(), "arm".into(), "hand".into()],
            joints: vec![[0, 1, 2, 0]; 4],
            weights: vec![[30000, 10000, 5000, 5000]; 4],
            joints1: vec![[2, 1, 0, 2]; 4],
            weights1: vec![[10000, 5000, 535, 0]; 4],
            ..Default::default()
        }
        .pack(4)
        .expect("skin");
        let morph = regia_asset::CpuMorphTarget {
            name: "smile".into(),
            vertex_indices: vec![1, 3],
            position_deltas: vec![[0.0, 0.125, 0.0], [0.5, -0.25, 1.0]],
            normal_deltas: vec![[0.0, 0.0, 0.5]; 2],
            tangent_deltas: Vec::new(),
        }
        .pack(4)
        .expect("morph");
        (skin, morph)
    }

    /// ADR-10080 acceptance (format batch 2, GPU mesh v3): a skinned mesh with eight influences
    /// converts to the GPU-ready form (it stayed compact with layout v2, ADR-7919) and
    /// `load_mesh` of the pak entry returns the same skin as the compact file: joint names, both
    /// influence sets and the eight-influence opt-in, with or without `COLOR_0`.
    /// `load_cooked_model` draws both forms alike (rigid, bind pose).
    #[test]
    fn an_eight_influence_skinned_mesh_round_trips_gpu_ready() {
        let (skin, _) = skin8_and_morph();
        for with_color in [false, true] {
            let mut skinned = compact_quad();
            skinned.skin = Some(skin.clone());
            skinned.skin_influences = regia_asset::EIGHT_SKIN_INFLUENCES;
            if with_color {
                skinned.stream_color0 = regia_asset::pack_colors_unorm16(&[[1.0, 0.5, 0.25, 1.0]; 4]);
            }
            let file = compact_file(&skinned);
            let gpu_file = gpu_mesh_file_from_cooked(&file)
                .expect("decode")
                .expect("an eight-influence skinned mesh converts (GPU mesh v3)");
            let compact = regia_asset::load_mesh_bytes(&file).expect("compact").mesh;
            let fast = regia_asset::load_mesh_bytes(&gpu_file).expect("gpu-ready").mesh;
            let compact_skin = compact.skin.as_ref().expect("compact skin");
            assert!(compact_skin.eight_influences && compact_skin.influences_per_vertex() == 8);
            assert_eq!(fast.skin, compact.skin, "the skin survives the GPU-ready form");
            assert_eq!(fast.colors, compact.colors);
            assert_eq!(
                (&fast.positions, &fast.indices),
                (&compact.positions, &compact.indices)
            );

            let (header, payload) = regia_asset::decode_cooked_file(&gpu_file).expect("wrapper");
            assert_eq!(header.type_tag, AssetTypeTag::gpu_mesh().0);
            let view = GpuMeshView::parse(payload).expect("parse");
            assert_eq!(
                view.extras.as_ref().map(|x| x.skin_influences),
                Some(regia_asset::EIGHT_SKIN_INFLUENCES)
            );
            let drawn = lab_mesh_from_gpu(&view).expect("gpu");
            let slow = lab_mesh(&compact, 0);
            assert_eq!(
                bytemuck::cast_slice::<LabVertex, u8>(&drawn.vertices),
                bytemuck::cast_slice::<LabVertex, u8>(&slow.vertices),
                "load_cooked_model draws both forms alike"
            );
            // A skinned draw of either form builds the same render vertices.
            let a = crate::CpuMesh::from_cooked(&fast).expect("gpu-ready render mesh");
            let b = crate::CpuMesh::from_cooked(&compact).expect("compact render mesh");
            assert_eq!(
                bytemuck::cast_slice::<LabVertex, u8>(&a.vertices),
                bytemuck::cast_slice::<LabVertex, u8>(&b.vertices)
            );
        }
    }

    /// ADR-10080 acceptance: a morphing mesh converts to the GPU-ready form and `load_mesh` of the
    /// pak entry returns its targets (names, vertex lists, position and normal deltas) equal to
    /// the compact file's, alone and together with a skin.
    #[test]
    fn a_morphing_mesh_round_trips_its_deltas_gpu_ready() {
        let (skin, morph) = skin8_and_morph();
        for with_skin in [false, true] {
            let mut morphing = compact_quad();
            morphing.morph_targets = vec![morph.clone()];
            if with_skin {
                morphing.skin = Some(skin.clone());
            }
            let file = compact_file(&morphing);
            let gpu_file = gpu_mesh_file_from_cooked(&file)
                .expect("decode")
                .expect("a morphing mesh converts (GPU mesh v3)");
            let compact = regia_asset::load_mesh_bytes(&file).expect("compact").mesh;
            let fast = regia_asset::load_mesh_bytes(&gpu_file).expect("gpu-ready").mesh;
            assert_eq!(compact.morph_targets.len(), 1);
            assert_eq!(fast.morph_targets, compact.morph_targets, "the deltas survive");
            assert_eq!(fast.morph_targets[0].position_deltas[1], [0.5, -0.25, 1.0]);
            assert_eq!(fast.skin, compact.skin);
            assert_eq!(
                fast.skin.as_ref().map(|s| s.eight_influences),
                with_skin.then_some(false)
            );
        }
    }

    /// ADR-10080: only real tangents skip the pass: the compact form's `(1, 0, 0, ±1)` stand-ins,
    /// a missing or short tangent list, zero, non-unit, non-finite or unsigned tangents are
    /// regenerated (one pass each).
    #[test]
    fn stand_in_or_broken_tangents_are_regenerated() {
        let compact = compact_quad().to_cpu_mesh().expect("compact");
        assert!(!stored_tangents_usable(&compact), "the compact stand-ins");
        let real = lab_mesh(&compact, 0);
        let mut good = compact.clone();
        good.tangents = real.vertices.iter().map(|v| v.tangent).collect();
        assert!(stored_tangents_usable(&good));
        let broken = |f: &dyn Fn(&mut Vec<[f32; 4]>)| {
            let mut m = good.clone();
            f(&mut m.tangents);
            m
        };
        for (what, mesh) in [
            ("empty", broken(&|t| t.clear())),
            (
                "short",
                broken(&|t| {
                    t.pop();
                }),
            ),
            ("zero", broken(&|t| t[0] = [0.0; 4])),
            ("long", broken(&|t| t[1] = [2.0, 0.0, 0.0, 1.0])),
            ("nan", broken(&|t| t[2] = [f32::NAN, 0.0, 0.0, 1.0])),
            ("unsigned", broken(&|t| t[3][3] = 0.5)),
        ] {
            assert!(!stored_tangents_usable(&mesh), "{what}");
            let before = crate::tangent_passes_on_this_thread();
            let vertices = cooked_vertices(&mesh, false);
            assert_eq!(
                crate::tangent_passes_on_this_thread() - before,
                1,
                "{what}: one pass"
            );
            assert_eq!(
                bytemuck::cast_slice::<LabVertex, u8>(&vertices),
                bytemuck::cast_slice::<LabVertex, u8>(&real.vertices),
                "{what}: regenerated"
            );
        }
    }

    /// ADR-10080 acceptance (plan W-14 "MikkTSpace at load"): `CpuMesh::from_cooked` of a mesh
    /// loaded from a GPU-ready entry reuses the stored tangents and runs no MikkTSpace pass; the
    /// compact form still runs one, and both give the same render vertices.
    #[test]
    fn from_cooked_of_a_gpu_ready_mesh_runs_no_tangent_pass() {
        let (skin, morph) = skin8_and_morph();
        let mut skinned = compact_quad();
        skinned.skin = Some(skin);
        skinned.morph_targets = vec![morph];
        let file = compact_file(&skinned);
        let gpu_file = gpu_mesh_file_from_cooked(&file)
            .expect("decode")
            .expect("converts");
        let compact = regia_asset::load_mesh_bytes(&file).expect("compact").mesh;
        let fast = regia_asset::load_mesh_bytes(&gpu_file).expect("gpu-ready").mesh;

        let before = crate::tangent_passes_on_this_thread();
        let slow_mesh = crate::CpuMesh::from_cooked(&compact).expect("compact");
        let after_compact = crate::tangent_passes_on_this_thread();
        assert_eq!(
            after_compact - before,
            1,
            "the compact form has no tangents: one pass"
        );
        let fast_mesh = crate::CpuMesh::from_cooked(&fast).expect("gpu-ready");
        let fast_rigid = crate::CpuMesh::from_cooked_rigid(&fast);
        let fast_counted = skinned_lab_vertices(&fast);
        assert_eq!(
            crate::tangent_passes_on_this_thread() - after_compact,
            0,
            "stored tangents: no MikkTSpace pass at load"
        );
        assert_eq!(
            bytemuck::cast_slice::<LabVertex, u8>(&fast_mesh.vertices),
            bytemuck::cast_slice::<LabVertex, u8>(&slow_mesh.vertices),
            "the stored tangents are the ones the pass makes"
        );
        assert_eq!(
            bytemuck::cast_slice::<LabVertex, u8>(&fast_counted),
            bytemuck::cast_slice::<LabVertex, u8>(&slow_mesh.vertices)
        );
        assert_eq!(fast_rigid.indices, slow_mesh.indices);
    }

    #[test]
    fn migrator_rejects_an_out_of_range_index() {
        let mut bad = compact_quad();
        bad.indices[16..18].copy_from_slice(&9u16.to_le_bytes());
        assert!(gpu_mesh_from_cooked(&bad).is_err());
    }

    #[test]
    fn migrator_rejects_a_non_triangle_index_count() {
        // Four u16 indices: aligned, in range, and not a whole triangle list. The compact
        // loader accepts it; the GPU-ready layout must not, because `GpuMeshView::parse`
        // rejects a LOD whose index count is not a multiple of 3.
        let mut bad = compact_quad();
        bad.lods = vec![regia_asset::CookedMeshLod {
            screen_relative_size: 1.0,
            index_byte_offset: 0,
            index_byte_length: 8,
        }];
        assert!(gpu_mesh_from_cooked(&bad).is_err());
        // The same hole through the synthetic LOD used when a mesh declares no LODs.
        let mut bare = compact_quad();
        bare.lods = Vec::new();
        bare.indices.truncate(8);
        assert!(gpu_mesh_from_cooked(&bare).is_err());
    }

    #[test]
    fn lod_suffixes() {
        assert_eq!(lod_of("SM_Silo_Rem_LOD0"), 0);
        assert_eq!(lod_of("SM_Silo_Rem_LOD2"), 2);
        assert_eq!(lod_of("hull_lod1"), 1);
        assert_eq!(lod_of("Glass"), 0);
        assert_eq!(lod_of("lod_marker"), 0);
    }

    /// A fresh directory unique to this process and call.
    fn unique_dir(tag: &str) -> std::path::PathBuf {
        regia_test_utils::unique_temp_dir(tag)
    }

    /// Writes `payload` into `cache` as the cooked file of `id`.
    fn put(cache: &std::path::Path, id: AssetId, tag: AssetTypeTag, payload: &[u8]) {
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: tag.0,
            asset_id: id,
            importer_version: 1,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = regia_asset::encode_cooked_file(&header, payload).expect("file");
        let path = cache.join(regia_asset::cooked_file_name(id));
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, file).expect("write");
        std::fs::rename(tmp, path).expect("publish");
    }

    /// Two cooked models in a dev cache whose parts share one material and its small (RGBA8)
    /// base colour image; returns the project root, its cache, the glTF ids, the mesh ids and the
    /// image id.
    fn shared_texture_project() -> (
        std::path::PathBuf,
        std::path::PathBuf,
        [AssetId; 2],
        [AssetId; 2],
        AssetId,
    ) {
        let root = unique_dir("cooked-model-batch");
        let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
        std::fs::create_dir_all(&cache).expect("cache dir");
        let image = AssetId::new_random();
        let material = AssetId::new_random();
        let gltfs = [AssetId::new_random(), AssetId::new_random()];
        let meshes = [AssetId::new_random(), AssetId::new_random()];
        let pixels = regia_asset::CookedImage {
            width: 2,
            height: 2,
            rgba8: vec![200; 16],
        };
        put(
            &cache,
            image,
            AssetTypeTag::image(),
            &pixels.to_postcard().expect("image"),
        );
        let mut authored = regia_asset::MaterialAssetRon::default_standard();
        authored.base.base_color_map = Some(TextureSlot::Asset(image));
        let cooked = CookedStandardMaterial::new(authored);
        put(
            &cache,
            material,
            AssetTypeTag::standard_material(),
            &cooked.to_postcard().expect("material"),
        );
        for (gltf, mesh) in gltfs.iter().zip(meshes) {
            put(
                &cache,
                mesh,
                AssetTypeTag::mesh(),
                &compact_quad().to_postcard().expect("mesh"),
            );
            let model = regia_asset::CookedModel {
                version: regia_asset::COOKED_MODEL_VERSION,
                parts: vec![regia_asset::CookedModelPart {
                    mesh,
                    material: Some(material),
                    node: "Hull".into(),
                }],
            };
            put(
                &cache,
                model_id(*gltf),
                AssetTypeTag::model(),
                &model.to_postcard().expect("model"),
            );
        }
        (root, cache, gltfs, meshes, image)
    }

    fn open_server(root: &std::path::Path) -> AssetServer {
        AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: regia_asset::current_platform(),
            content_dir: None,
        })
        .expect("open server")
    }

    /// The batch path (meshes and textures decoded together on the pool, the image read through
    /// one typed read) builds the same draws as one load per model, and records the meshes and the
    /// shared image as loaded from the cache.
    #[test]
    fn a_batch_loads_models_like_single_loads() {
        let (root, _cache, gltfs, meshes, image) = shared_texture_project();
        let mut server = open_server(&root);
        let batch = load_cooked_models(&mut server, &gltfs).expect("batch");
        assert_eq!(batch.len(), 2);
        let mut single_server = open_server(&root);
        for (draws, gltf) in batch.iter().zip(gltfs) {
            let single = load_cooked_model(&mut single_server, gltf).expect("single");
            assert_eq!(draws.meshes.len(), 1);
            assert_eq!(draws.meshes.len(), single.meshes.len());
            assert_eq!(
                bytemuck::cast_slice::<LabVertex, u8>(&draws.meshes[0].vertices),
                bytemuck::cast_slice::<LabVertex, u8>(&single.meshes[0].vertices)
            );
            assert_eq!(draws.meshes[0].indices, single.meshes[0].indices);
            assert_eq!(draws.meshes[0].material, 0);
            assert_eq!(draws.materials.len(), 1);
            let albedo = draws.materials[0].albedo.as_ref().expect("the shared image");
            assert_eq!((albedo.width, albedo.height), (2, 2));
        }
        for id in meshes.into_iter().chain([image]) {
            assert_eq!(server.source_of(id), AssetSource::DevCache);
            assert_eq!(server.state(id), regia_asset::LoadState::Loaded);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The HLOD proxy atlas path (ADR-2483): one cooked material read by id, with its texture slots
    /// resolved as the model path does; an id that does not read is an error.
    #[test]
    fn a_material_loads_alone_with_its_textures() {
        let (root, _cache, gltfs, _meshes, _image) = shared_texture_project();
        let mut server = open_server(&root);
        let model = server.load_model(model_id(gltfs[0])).expect("model");
        let material = model.parts[0].material.expect("a material");
        let reader = server.payload_reader();
        let alone = load_cooked_material(&reader, material).expect("material");
        let albedo = alone.albedo.as_ref().expect("the image");
        assert_eq!((albedo.width, albedo.height), (2, 2));
        assert!(load_cooked_material(&reader, AssetId::new_random()).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A texture that does not load is a warning (the model still loads, the image is not
    /// marked loaded); a mesh that does not load fails the model.
    #[test]
    fn a_missing_texture_warns_and_a_missing_mesh_fails() {
        let (root, cache, gltfs, meshes, image) = shared_texture_project();
        std::fs::remove_file(cache.join(regia_asset::cooked_file_name(image))).expect("remove image");
        let mut server = open_server(&root);
        let draws = load_cooked_model(&mut server, gltfs[0]).expect("loads without its texture");
        assert_eq!(draws.meshes.len(), 1);
        assert_ne!(server.state(image), regia_asset::LoadState::Loaded);

        std::fs::remove_file(cache.join(regia_asset::cooked_file_name(meshes[1]))).expect("remove mesh");
        let mut server = open_server(&root);
        assert!(matches!(
            load_cooked_models(&mut server, &gltfs),
            Err(CookedModelError::Asset { what: "mesh", id, .. }) if id == meshes[1]
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Both models and both authored LODs share a material. Preserve deliberately unusual
    /// cooked tangents so a call to the old tangent-generation path fails byte equality.
    fn all_lods_project() -> (std::path::PathBuf, [AssetId; 2], Vec<LabVertex>) {
        let (root, cache, gltfs, meshes, _) = shared_texture_project();
        let mut server = open_server(&root);
        let (info, mut vertices, indices) = gpu_mesh_from_cooked(&compact_quad()).expect("offline cook");
        for vertex in &mut vertices {
            vertex.tangent = [0.0, 0.0, 1.0, -1.0];
        }
        for (gltf, mesh) in gltfs.into_iter().zip(meshes) {
            let mut model = server.load_model(model_id(gltf)).expect("model");
            model.parts[0].node = "Hull_LOD0".into();
            let mut far = model.parts[0].clone();
            far.node = "Hull_LOD1".into();
            model.parts.push(far);
            put(
                &cache,
                model_id(gltf),
                AssetTypeTag::model(),
                &model.to_postcard().expect("model"),
            );
            let file = regia_asset::encode_gpu_mesh_file(
                mesh,
                1,
                vec![],
                &info,
                bytemuck::cast_slice(&vertices),
                &indices,
            )
            .expect("GPU mesh");
            let path = cache.join(regia_asset::cooked_file_name(mesh));
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, file).expect("mesh");
            std::fs::rename(tmp, path).expect("publish mesh");
        }
        (root, gltfs, vertices)
    }

    #[test]
    fn all_lods_shared_texture_uploads_once_across_models_and_authored_lods() {
        use crate::texture_residency::{ResidentTextures, UploadEnv, bind_material_image};
        let (root, gltfs, vertices) = all_lods_project();
        let mut server = open_server(&root);
        let models = load_cooked_models_all_lods(&mut server, &gltfs).expect("all LODs");
        assert_eq!(
            server.load_forms().images_rgba8,
            1,
            "one image decode for the entire batch"
        );
        assert_eq!(
            server.load_forms().compact_meshes,
            0,
            "no tangent-reconstruction path"
        );
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let mut cache = ResidentTextures::default();
        let before = crate::frame_stats::counters();
        let mut bindings = Vec::new();
        for (model_index, model) in models.iter().enumerate() {
            assert_eq!(model.parts.len(), 2);
            assert_eq!(model.materials.len(), 1);
            for part in &model.parts {
                assert_eq!(
                    bytemuck::cast_slice::<_, u8>(&part.vertices),
                    bytemuck::cast_slice::<_, u8>(&vertices)
                );
                let image = model.materials[part.material]
                    .images
                    .albedo
                    .as_ref()
                    .expect("shared image");
                let env = UploadEnv {
                    features: device.device.features(),
                    material_key: model_index as u64,
                    name: None,
                    now: 0,
                    device: &device.device,
                    queue: &queue,
                };
                bindings
                    .push(bind_material_image(&mut cache, &env, image, "base_color", true).expect("bind"));
            }
        }
        let uploaded = crate::frame_stats::counters().since(before);
        assert_eq!(cache.len(), 1, "four parts use one resident GPU texture");
        assert_eq!(
            uploaded.upload_bytes, 20,
            "one upload chain: 2x2 RGBA plus its 1x1 mip"
        );
        assert_eq!(bindings.len(), 4);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Compact meshes (the default cook, the dev cache, Android paks) load through the legacy
    /// decode once: the vertices, MikkTSpace tangents included, are the legacy loader's bytes,
    /// and the compact mesh's own LOD table becomes the generated index LODs.
    #[test]
    fn all_lods_decodes_compact_meshes_like_the_legacy_loader() {
        let (root, _, gltfs, meshes, _) = shared_texture_project();
        let mut server = open_server(&root);
        let all = load_cooked_models_all_lods(&mut server, &gltfs).expect("compact meshes load");
        assert_eq!(
            server.load_forms().compact_meshes,
            2,
            "one legacy decode per mesh"
        );
        assert_eq!(server.load_forms().gpu_meshes, 0);
        let legacy = load_cooked_models(&mut open_server(&root), &gltfs).expect("legacy");
        let compact = compact_quad();
        for ((model, draws), mesh) in all.iter().zip(&legacy).zip(meshes) {
            let part = &model.parts[0];
            assert_eq!(part.mesh_id, mesh);
            assert_eq!(
                bytemuck::cast_slice::<_, u8>(&part.vertices),
                bytemuck::cast_slice::<_, u8>(&draws.meshes[0].vertices),
                "the legacy decode's vertices and tangents"
            );
            assert_eq!(part.index_lods.len(), compact.lods.len());
            for (range, lod) in part.index_lods.iter().zip(&compact.lods) {
                assert_eq!(range.first_index, lod.index_byte_offset / 2);
                assert_eq!(range.index_count, lod.index_byte_length / 2);
                assert_eq!(range.screen_relative_size, lod.screen_relative_size);
            }
            assert_eq!(part.lod_indices(0), Some(draws.meshes[0].indices.as_slice()));
            assert_eq!(part.lod_indices(1), Some([0, 1, 3].as_slice()));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// One batch may mix a GPU-ready and a compact mesh (a desktop pak keeps skinned and
    /// colored meshes compact); a compact index past the last vertex in any LOD is refused.
    #[test]
    fn all_lods_mixes_forms_and_checks_compact_indices_in_every_lod() {
        let (root, gltfs, _) = all_lods_project();
        let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
        let model = open_server(&root).load_model(model_id(gltfs[1])).expect("model");
        let compact_id = model.parts[0].mesh;
        put(
            &cache,
            compact_id,
            AssetTypeTag::mesh(),
            &compact_quad().to_postcard().expect("mesh"),
        );
        let mut server = open_server(&root);
        let loaded = load_cooked_models_all_lods(&mut server, &gltfs).expect("mixed batch");
        assert_eq!(loaded.len(), 2);
        assert_eq!(server.load_forms().gpu_meshes, 2, "model 0: both authored parts");
        assert_eq!(
            server.load_forms().compact_meshes,
            2,
            "model 1: both authored parts"
        );
        let mut bad = compact_quad();
        // The last index belongs only to LOD 1; the legacy loader never reads it.
        let end = bad.indices.len();
        bad.indices[end - 2..].copy_from_slice(&7u16.to_le_bytes());
        put(
            &cache,
            compact_id,
            AssetTypeTag::mesh(),
            &bad.to_postcard().expect("mesh"),
        );
        assert!(matches!(
            load_cooked_models_all_lods(&mut open_server(&root), &gltfs[1..]),
            Err(CookedModelLodsError::Load(CookedModelError::Asset { what: "mesh", id, .. })) if id == compact_id
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn all_lods_accepts_lod_one_only_and_keeps_legacy_empty_error() {
        let (root, gltfs, _) = all_lods_project();
        let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
        let mut model = open_server(&root).load_model(model_id(gltfs[0])).expect("model");
        model.parts.remove(0);
        put(
            &cache,
            model_id(gltfs[0]),
            AssetTypeTag::model(),
            &model.to_postcard().expect("model"),
        );
        let loaded = load_cooked_models_all_lods(&mut open_server(&root), &gltfs[..1]).expect("LOD 1 only");
        assert_eq!(loaded[0].parts[0].authored_lod, 1);
        assert!(matches!(
            load_cooked_models(&mut open_server(&root), &gltfs[..1]),
            Err(CookedModelError::Empty(_))
        ));
        assert!(
            load_cooked_models_all_lods(&mut open_server(&root), &[])
                .expect("empty batch")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn all_lods_checks_indices_outside_level_zero() {
        let (root, gltfs, _) = all_lods_project();
        let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
        let model = open_server(&root).load_model(model_id(gltfs[0])).expect("model");
        let mesh_id = model.parts[0].mesh;
        let path = cache.join(regia_asset::cooked_file_name(mesh_id));
        let file = std::fs::read(&path).expect("mesh file");
        let (header, payload) = regia_asset::decode_cooked_file(&file).expect("wrapper");
        let mut payload = payload.to_vec();
        let end = payload.len();
        payload[end - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
        put(&cache, mesh_id, AssetTypeTag(header.type_tag), &payload);
        // The corrupt final index belongs only to generated LOD 1.
        assert!(load_cooked_models(&mut open_server(&root), &gltfs[..1]).is_ok());
        assert!(matches!(
            load_cooked_models_all_lods(&mut open_server(&root), &gltfs[..1]),
            Err(CookedModelLodsError::Load(CookedModelError::Asset { what: "mesh", id, .. })) if id == mesh_id
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn all_lods_missing_texture_retains_the_legacy_fallback() {
        let (root, gltfs, _) = all_lods_project();
        let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
        let mut server = open_server(&root);
        let model = server.load_model(model_id(gltfs[0])).expect("model");
        let mid = model.parts[0].material.expect("material id");
        let handle = server.load_material(mid).expect("material");
        let material = server.materials().get(&handle).expect("loaded material");
        let TextureSlot::Asset(image) = material.authored.base.base_color_map.as_ref().expect("image") else {
            panic!("fixture must name an asset");
        };
        std::fs::remove_file(cache.join(regia_asset::cooked_file_name(*image))).expect("remove texture");
        let all = load_cooked_models_all_lods(&mut server, &gltfs).expect("fallback");
        let legacy = load_cooked_models(&mut open_server(&root), &gltfs).expect("legacy fallback");
        assert_eq!(
            format!("{:?}", all[0].materials),
            format!("{:?}", legacy[0].materials)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn all_lods_without_a_generated_table_exposes_one_checked_range() {
        let (root, gltfs, vertices) = all_lods_project();
        let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
        let model = open_server(&root).load_model(model_id(gltfs[0])).expect("model");
        let (mut info, _, indices) = gpu_mesh_from_cooked(&compact_quad()).expect("cook");
        info.lods.clear();
        let id = model.parts[0].mesh;
        let file = regia_asset::encode_gpu_mesh_file(
            id,
            1,
            vec![],
            &info,
            bytemuck::cast_slice(&vertices),
            &indices,
        )
        .expect("no LOD table");
        let (header, payload) = regia_asset::decode_cooked_file(&file).expect("wrapper");
        put(&cache, id, AssetTypeTag(header.type_tag), payload);
        let mut loaded = load_cooked_models_all_lods(&mut open_server(&root), &gltfs[..1]).expect("load");
        let part = &mut loaded[0].parts[0];
        assert_eq!(part.index_lods.len(), 1);
        assert_eq!(part.lod_indices(0), Some(indices.as_slice()));
        assert!(part.lod_indices(1).is_none());
        part.index_lods[0].first_index = u32::MAX;
        part.index_lods[0].index_count = u32::MAX;
        assert!(part.lod_indices(0).is_none());
        assert!(part.lab_mesh(0).is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
