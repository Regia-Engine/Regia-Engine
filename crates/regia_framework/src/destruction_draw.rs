// SPDX-License-Identifier: MPL-2.0
//! The chunk draw path of destruction level 2 (`regia_physics::destruction`): draws the pieces of
//! a broken structure with the meshes the fracture cook wrote, so a game does not have to.
//!
//! The break model spawns a `DestructibleRoot` entity per instance (its `Transform` is the
//! instance pose) and a `DestructionChunk` entity per debris body (its `Transform` is the body
//! pose, at the chunk's centroid), with no mesh. This module gives them one:
//!
//! - [`DestructionMeshes`] holds a [`DestructionMeshSet`] per destructible asset, keyed by the
//!   asset's `name`. [`load_destruction_meshes`] builds a set through the asset server from the
//!   chunks' `lods` (`<source>#fracture/chunk_<n>`); [`mesh_set_from_cooked`] builds one from cooked
//!   meshes already in memory. Material slot 0 of a chunk mesh is the original surface, any other
//!   slot the cut faces; each becomes its own mesh.
//! - [`DestructionDrawPlugin`] (opt-in) draws an instance once it has lost a chunk: one child of the
//!   root per still-attached chunk part (surface, cut faces) at LOD 0, and the debris LOD
//!   ([`DestructionDrawSettings::debris_lod`]) as children of each debris body. Chunks that left
//!   with no body (small ones, or gone in a snapshot) are not drawn.
//! - The intact look is the game's own kit mesh. Mark it with [`DestructionIntactMesh`]: while the
//!   pieces draw, its `MeshRenderer` is moved into [`DestructionIntactHidden`] (the renderer has no
//!   per-entity visibility switch) and put back when the instance is whole again or goes away.
//!
//! Mesh space: the cook writes chunk vertices in the asset's space (the kit's frame), the same
//! space as the chunk centroids. Attached draws therefore sit at the identity under the root. A
//! debris body's origin is the chunk centroid and its axes are the asset's, so its draws sit at
//! `-centroid` under the body.
//!
//! The systems run in `Update`, after every fixed tick of the frame, so the root and debris
//! entities that `step_destruction` spawned through commands exist. A draw whose parent is not
//! there yet is retried on the next frame. Everything iterates in id and chunk order.
//!
//! Agent surface: `physics.destruction_draw` (`book/agents/destruction-draw.md`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{
    AssetId, AssetServer, AssetTypeTag, CookedIndexWidth, CookedMesh, CookedMeshValidationError,
    CookedSubmesh, GpuMeshView,
};
use regia_core::math::Vec3;
use regia_physics::destruction::{
    ChunkState, DestructibleAsset, DestructibleId, DestructibleRoot, Destruction, DestructionChunk,
};
use regia_render::{CpuMaterial, CpuMesh, LabVertex, MeshRenderer};
use regia_scene::Transform;

/// One LOD of one chunk: the original surface and the cut faces, each optional (a chunk may have
/// no cut face, a LOD may drop one).
#[derive(Clone, Debug, Default)]
pub struct DestructionLodMeshes {
    /// The pieces of the source surface (material slot 0), drawn with the set's surface material.
    pub surface: Option<Arc<CpuMesh>>,
    /// The cut faces (material slot 1), drawn with the set's cut-face material.
    pub cut: Option<Arc<CpuMesh>>,
}

impl DestructionLodMeshes {
    /// A LOD from meshes a game built itself (asset space, see the module docs). Tangents are
    /// rebuilt with MikkTSpace; skins are ignored.
    #[must_use]
    pub fn from_cpu_parts(
        surface: Option<&regia_asset::CpuMesh>,
        cut: Option<&regia_asset::CpuMesh>,
    ) -> Self {
        let part = |mesh: Option<&regia_asset::CpuMesh>| {
            mesh.filter(|m| !m.indices.is_empty())
                .map(|m| Arc::new(CpuMesh::from_cooked_rigid(m)))
        };
        Self {
            surface: part(surface),
            cut: part(cut),
        }
    }
}

/// One chunk's meshes, LOD 0 first. Empty: the chunk is never drawn.
#[derive(Clone, Debug, Default)]
pub struct DestructionChunkMeshes {
    /// The LODs, most detailed first.
    pub lods: Vec<DestructionLodMeshes>,
}

/// The meshes of one destructible asset: chunk `i` here is chunk `i` of the asset.
#[derive(Clone, Debug)]
pub struct DestructionMeshSet {
    /// One entry per chunk, in the asset's order.
    pub chunks: Vec<DestructionChunkMeshes>,
    /// Material of the original surface (usually the kit's own).
    pub surface_material: Arc<CpuMaterial>,
    /// Material of the cut faces.
    pub cut_material: Arc<CpuMaterial>,
}

/// The mesh sets the draw path knows, keyed by the destructible asset's `name`. Two assets with the
/// same name share a set.
#[derive(Resource, Debug, Default)]
pub struct DestructionMeshes {
    sets: BTreeMap<String, Arc<DestructionMeshSet>>,
}

impl DestructionMeshes {
    /// Registers `set` for the asset named `name`; returns the set it replaces. Instances already
    /// drawn with the old set are drawn again with the new one.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        set: DestructionMeshSet,
    ) -> Option<Arc<DestructionMeshSet>> {
        self.sets.insert(name.into(), Arc::new(set))
    }

    /// The set of the asset named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<DestructionMeshSet>> {
        self.sets.get(name)
    }

    /// Removes the set of the asset named `name`; its draws go on the next update.
    pub fn remove(&mut self, name: &str) -> Option<Arc<DestructionMeshSet>> {
        self.sets.remove(name)
    }

    /// Sets registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sets.len()
    }

    /// No set is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    /// Registered asset names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.sets.keys().map(String::as_str)
    }
}

/// Why a mesh set did not load. [`DestructionDrawError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DestructionDrawError {
    /// A `lods` entry is empty or has an empty side of `#`.
    #[error("chunk {chunk}: `{path}` is not a mesh reference (`<file>#<sub-asset>`)")]
    BadPath {
        /// Chunk index.
        chunk: u32,
        /// The reference.
        path: String,
    },
    /// The mesh is not in the manifest, or no pak or cache file holds it.
    #[error("chunk {chunk}: mesh `{path}` is missing: {reason}")]
    Missing {
        /// Chunk index.
        chunk: u32,
        /// The reference.
        path: String,
        /// What the asset server said.
        reason: String,
    },
    /// The mesh is not a cooked mesh, or does not decode or validate.
    #[error("chunk {chunk}: mesh `{path}` does not decode: {reason}")]
    Decode {
        /// Chunk index.
        chunk: u32,
        /// The reference.
        path: String,
        /// What went wrong.
        reason: String,
    },
    /// The mesh list does not have one entry per chunk of the asset.
    #[error("asset `{name}` has {expected} chunks, the mesh list {found}")]
    Count {
        /// Asset name.
        name: String,
        /// Chunks in the asset.
        expected: usize,
        /// Entries given.
        found: usize,
    },
}

impl DestructionDrawError {
    /// The agent-protocol error code: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadPath { .. } => "destruction_mesh_bad_path",
            Self::Missing { .. } => "destruction_mesh_missing",
            Self::Decode { .. } => "destruction_mesh_decode",
            Self::Count { .. } => "destruction_mesh_count",
        }
    }
}

/// Splits one cooked chunk mesh by material slot: slot 0 is the surface, any other slot the cut
/// faces. A mesh with no submesh is all surface. Vertices keep the cooked mesh's space; tangents are
/// rebuilt with MikkTSpace over LOD 0, as the cooked model path does.
///
/// # Errors
/// The mesh does not validate, or a submesh range is misaligned, outside the index buffer, not
/// whole triangles, or names a vertex past the last.
pub fn split_cooked_chunk(mesh: &CookedMesh) -> Result<DestructionLodMeshes, CookedMeshValidationError> {
    let cpu = mesh.to_cpu_mesh()?;
    let full = CpuMesh::from_cooked_rigid(&cpu);
    if mesh.submeshes.is_empty() {
        return Ok(DestructionLodMeshes {
            surface: compact_part(&full.vertices, &full.indices),
            cut: None,
        });
    }
    let mut surface = Vec::new();
    let mut cut = Vec::new();
    for sub in &mesh.submeshes {
        let out = if sub.material_slot == 0 {
            &mut surface
        } else {
            &mut cut
        };
        submesh_indices(mesh, sub, out)?;
    }
    Ok(DestructionLodMeshes {
        surface: compact_part(&full.vertices, &surface),
        cut: compact_part(&full.vertices, &cut),
    })
}

/// [`split_cooked_chunk`] for a GPU-ready chunk mesh: material slot 0 is the surface, any other
/// slot a cut face; a mesh with no submesh table is all surface.
///
/// # Errors
/// The text of the [`regia_asset::GpuMeshError`] for an index past the last vertex or a bad range.
pub fn split_gpu_chunk(view: &GpuMeshView<'_>) -> Result<DestructionLodMeshes, String> {
    let lab = regia_render::cooked_model::lab_mesh_from_gpu(view).map_err(|e| e.to_string())?;
    if view.submeshes.is_empty() {
        return Ok(DestructionLodMeshes {
            surface: compact_part(&lab.vertices, &lab.indices),
            cut: None,
        });
    }
    let mut surface = Vec::new();
    let mut cut = Vec::new();
    for (i, sub) in view.submeshes.iter().enumerate() {
        let out = if sub.material_slot == 0 {
            &mut surface
        } else {
            &mut cut
        };
        out.extend(view.submesh_indices(i).map_err(|e| e.to_string())?);
    }
    Ok(DestructionLodMeshes {
        surface: compact_part(&lab.vertices, &surface),
        cut: compact_part(&lab.vertices, &cut),
    })
}

/// Appends a submesh's indices to `out`, checked.
fn submesh_indices(
    mesh: &CookedMesh,
    sub: &CookedSubmesh,
    out: &mut Vec<u32>,
) -> Result<(), CookedMeshValidationError> {
    let width = match mesh.index_width {
        CookedIndexWidth::U16 => 2,
        CookedIndexWidth::U32 => 4,
    };
    let bad_size = || CookedMeshValidationError::StreamSize {
        vertices: mesh.vertex_count,
    };
    for edge in [sub.index_byte_offset, sub.index_byte_length] {
        if !(edge as usize).is_multiple_of(width) {
            return Err(CookedMeshValidationError::IndexAlignment {
                len: edge as usize,
                width,
            });
        }
    }
    let start = sub.index_byte_offset as usize;
    let end = start
        .checked_add(sub.index_byte_length as usize)
        .ok_or_else(bad_size)?;
    let bytes = mesh.indices.get(start..end).ok_or_else(bad_size)?;
    if !(bytes.len() / width).is_multiple_of(3) {
        return Err(bad_size());
    }
    let first = out.len();
    match mesh.index_width {
        CookedIndexWidth::U16 => out.extend(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u32::from(u16::from_le_bytes(*b))),
        ),
        CookedIndexWidth::U32 => out.extend(bytes.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b))),
    }
    if out[first..].iter().any(|&i| i >= mesh.vertex_count) {
        return Err(bad_size());
    }
    Ok(())
}

/// The vertices `indices` use, in first-use order, and the indices remapped to them; `None` for
/// no triangle.
fn compact_part(vertices: &[LabVertex], indices: &[u32]) -> Option<Arc<CpuMesh>> {
    if indices.is_empty() {
        return None;
    }
    let mut remap = vec![u32::MAX; vertices.len()];
    let mut out = CpuMesh {
        vertices: Vec::new(),
        indices: Vec::with_capacity(indices.len()),
    };
    for &i in indices {
        let (Some(slot), Some(vertex)) = (remap.get_mut(i as usize), vertices.get(i as usize)) else {
            // Indices are checked against the vertex count before they get here.
            continue;
        };
        if *slot == u32::MAX {
            *slot = out.vertices.len() as u32;
            out.vertices.push(*vertex);
        }
        out.indices.push(*slot);
    }
    Some(Arc::new(out))
}

/// A mesh set from cooked chunk meshes already in memory: `chunks[i]` holds chunk `i`'s LODs, most
/// detailed first, each split by [`split_cooked_chunk`].
///
/// # Errors
/// [`DestructionDrawError::Count`] when `chunks` does not have one entry per chunk of `asset`,
/// [`DestructionDrawError::Decode`] when a mesh does not split.
pub fn mesh_set_from_cooked(
    asset: &DestructibleAsset,
    chunks: &[Vec<CookedMesh>],
    surface: Arc<CpuMaterial>,
    cut: Arc<CpuMaterial>,
) -> Result<DestructionMeshSet, DestructionDrawError> {
    if chunks.len() != asset.chunks.len() {
        return Err(DestructionDrawError::Count {
            name: asset.name.clone(),
            expected: asset.chunks.len(),
            found: chunks.len(),
        });
    }
    let mut out = Vec::with_capacity(chunks.len());
    for (c, lods) in chunks.iter().enumerate() {
        let mut levels = Vec::with_capacity(lods.len());
        for (l, mesh) in lods.iter().enumerate() {
            let level = split_cooked_chunk(mesh).map_err(|e| DestructionDrawError::Decode {
                chunk: c as u32,
                path: asset
                    .chunks
                    .get(c)
                    .and_then(|chunk| chunk.lods.get(l))
                    .cloned()
                    .unwrap_or_else(|| format!("lod {l}")),
                reason: e.to_string(),
            })?;
            levels.push(level);
        }
        out.push(DestructionChunkMeshes { lods: levels });
    }
    Ok(DestructionMeshSet {
        chunks: out,
        surface_material: surface,
        cut_material: cut,
    })
}

/// A `lods` entry as (file, sub-asset): `"a.glb#fracture/chunk_0"` or a whole-file `"a.mesh"`.
/// `None` when it is empty or a side of `#` is.
fn split_reference(path: &str) -> Option<(&str, Option<&str>)> {
    match path.split_once('#') {
        Some((file, sub)) if !file.is_empty() && !sub.is_empty() => Some((file, Some(sub))),
        Some(_) => None,
        None if path.is_empty() => None,
        None => Some((path, None)),
    }
}

/// The asset id of a `lods` entry: the file's id from the manifest, then the sub-asset's
/// (`AssetId::sub_asset`); the whole reference as a manifest path as a fallback.
fn resolve_mesh_id(server: &AssetServer, chunk: u32, path: &str) -> Result<AssetId, DestructionDrawError> {
    let Some((file, sub)) = split_reference(path) else {
        return Err(DestructionDrawError::BadPath {
            chunk,
            path: path.to_string(),
        });
    };
    let parent = server.id_for_path(file);
    let id = match sub {
        Some(sub) => parent
            .map(|p| AssetId::sub_asset(p, sub))
            .or_else(|| server.id_for_path(path)),
        None => parent,
    };
    id.ok_or_else(|| DestructionDrawError::Missing {
        chunk,
        path: path.to_string(),
        reason: "not in the asset manifest".to_string(),
    })
}

/// One cooked chunk file as a LOD. A GPU-ready mesh (desktop paks, layout v2) keeps its submesh
/// ranges, so it splits like the loose file; a v1 payload (no ranges) is all surface.
fn decode_chunk_file(bytes: &[u8]) -> Result<DestructionLodMeshes, String> {
    let (header, payload) = regia_asset::decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag == AssetTypeTag::gpu_mesh().0 {
        let view = GpuMeshView::parse(payload).map_err(|e| e.to_string())?;
        return split_gpu_chunk(&view);
    }
    if header.type_tag != AssetTypeTag::mesh().0 {
        return Err("not a cooked mesh".to_string());
    }
    let mesh = CookedMesh::from_postcard(payload).map_err(|e| e.to_string())?;
    split_cooked_chunk(&mesh).map_err(|e| e.to_string())
}

/// Loads the meshes every chunk of `asset` names in its `lods` through `server` (paks first, then
/// the cooked cache) and splits each into surface and cut faces. A chunk with no `lods` is never
/// drawn. Each mesh id is marked with [`AssetServer::track_external`], so a hot reload bumps
/// [`AssetServer::generation`] for the caller to load the set again.
///
/// # Errors
/// [`DestructionDrawError::BadPath`] for a malformed reference, [`DestructionDrawError::Missing`]
/// for one the server cannot find or read, [`DestructionDrawError::Decode`] for a mesh that does not
/// decode.
pub fn load_destruction_meshes(
    server: &mut AssetServer,
    asset: &DestructibleAsset,
    surface: Arc<CpuMaterial>,
    cut: Arc<CpuMaterial>,
) -> Result<DestructionMeshSet, DestructionDrawError> {
    let _span = tracing::info_span!("asset_load", kind = "destruction_meshes").entered();
    let mut chunks = Vec::with_capacity(asset.chunks.len());
    for (c, chunk) in asset.chunks.iter().enumerate() {
        let c = c as u32;
        let mut lods = Vec::with_capacity(chunk.lods.len());
        for path in &chunk.lods {
            let id = resolve_mesh_id(server, c, path)?;
            server.track_external(id);
            let (bytes, _) = server
                .read_cooked(id)
                .map_err(|e| DestructionDrawError::Missing {
                    chunk: c,
                    path: path.clone(),
                    reason: e.to_string(),
                })?;
            let lod = decode_chunk_file(&bytes).map_err(|reason| DestructionDrawError::Decode {
                chunk: c,
                path: path.clone(),
                reason,
            })?;
            lods.push(lod);
        }
        chunks.push(DestructionChunkMeshes { lods });
    }
    Ok(DestructionMeshSet {
        chunks,
        surface_material: surface,
        cut_material: cut,
    })
}

/// Options of the draw path.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestructionDrawSettings {
    /// The LOD debris draws with (clamped to the chunk's last LOD). Attached chunks draw LOD 0.
    pub debris_lod: u8,
    /// Hide the [`DestructionIntactMesh`] entities of an instance while its pieces draw.
    pub hide_intact: bool,
}

impl Default for DestructionDrawSettings {
    fn default() -> Self {
        Self {
            debris_lod: 0,
            hide_intact: true,
        }
    }
}

/// What the draw path holds after its last update (for tools and the agent surface).
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct DestructionDrawStats {
    /// Broken instances drawn in pieces.
    pub instances_drawn: usize,
    /// Draw entities under roots (one per attached chunk part).
    pub attached_draws: usize,
    /// Draw entities under debris bodies.
    pub debris_draws: usize,
    /// Intact meshes hidden ([`DestructionIntactHidden`]).
    pub intact_hidden: usize,
    /// Instances whose asset has no registered mesh set.
    pub missing_sets: usize,
    /// Instances whose asset's set has another chunk count.
    pub mismatched_sets: usize,
    /// Names of the assets with no registered set, sorted.
    pub missing: Vec<String>,
}

/// On the game's intact kit mesh entity of an instance: hidden while the instance draws in pieces.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestructionIntactMesh(pub DestructibleId);

/// The `MeshRenderer` of a hidden intact mesh, put back when the instance is whole again or gone.
#[derive(Component, Clone, Debug)]
pub struct DestructionIntactHidden(pub MeshRenderer);

/// On each draw entity the plugin spawns (a child of a root or of a debris body).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestructionChunkDraw {
    /// The instance.
    pub destructible: DestructibleId,
    /// Chunk index.
    pub chunk: u32,
    /// The LOD drawn.
    pub lod: u8,
    /// The cut faces (else the surface).
    pub cut: bool,
    /// Under a debris body (else under the root).
    pub debris: bool,
}

/// The draw path's systems, in `Update`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DestructionDrawSet;

/// Draws broken destructibles with their registered mesh sets (see the module docs). Adds
/// [`DestructionMeshes`], [`DestructionDrawSettings`] and [`DestructionDrawStats`] (keeping ones
/// the app inserted first). Opt-in: without it nothing is drawn and nothing changes. Add it with
/// `DestructionPlugin`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DestructionDrawPlugin;

impl Plugin for DestructionDrawPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DestructionMeshes>()
            .init_resource::<DestructionDrawSettings>()
            .init_resource::<DestructionDrawStats>()
            .init_resource::<DrawTracker>()
            .add_systems(
                Update,
                (sync_chunk_draws, sync_intact_meshes)
                    .chain()
                    .in_set(DestructionDrawSet),
            );
    }
}

/// The draws of one instance.
#[derive(Debug)]
struct TrackedInstance {
    root: Entity,
    asset: Arc<DestructibleAsset>,
    set: Arc<DestructionMeshSet>,
    /// Draw entities per attached chunk (empty for a chunk with no mesh).
    attached: BTreeMap<u32, Vec<Entity>>,
    /// Debris body and its draw entities per detached chunk.
    debris: BTreeMap<u32, (Entity, Vec<Entity>)>,
}

/// What the plugin spawned, so it can take it down again.
#[derive(Resource, Debug, Default)]
struct DrawTracker {
    instances: BTreeMap<DestructibleId, TrackedInstance>,
    /// The debris LOD the debris draws were made with.
    debris_lod: Option<u8>,
    /// A parent was not there yet: run again next frame.
    retry: bool,
}

fn despawn_all(commands: &mut Commands<'_, '_>, entities: &[Entity]) {
    for &e in entities {
        commands.entity(e).try_despawn();
    }
}

fn despawn_instance(commands: &mut Commands<'_, '_>, tracked: &TrackedInstance) {
    for drawn in tracked.attached.values() {
        despawn_all(commands, drawn);
    }
    for (_, drawn) in tracked.debris.values() {
        despawn_all(commands, drawn);
    }
}

/// What to spawn for one chunk.
struct Part<'a> {
    destructible: DestructibleId,
    chunk: u32,
    lod: u8,
    parent: Entity,
    translation: Vec3,
    debris: bool,
    set: &'a DestructionMeshSet,
}

/// Spawns a chunk LOD's surface and cut-face draws under `part.parent`.
fn spawn_parts(commands: &mut Commands<'_, '_>, part: &Part<'_>) -> Vec<Entity> {
    let Some(level) = part
        .set
        .chunks
        .get(part.chunk as usize)
        .and_then(|c| c.lods.get(usize::from(part.lod)))
    else {
        return Vec::new();
    };
    [
        (&level.surface, &part.set.surface_material, false),
        (&level.cut, &part.set.cut_material, true),
    ]
    .into_iter()
    .filter_map(|(mesh, material, cut)| {
        let mesh = mesh.as_ref()?;
        Some(
            commands
                .spawn((
                    MeshRenderer::new(Arc::clone(mesh), Arc::clone(material)),
                    Transform::from_translation(part.translation),
                    ChildOf(part.parent),
                    DestructionChunkDraw {
                        destructible: part.destructible,
                        chunk: part.chunk,
                        lod: part.lod,
                        cut,
                        debris: part.debris,
                    },
                ))
                .id(),
        )
    })
    .collect()
}

/// The LOD a chunk draws at when `want` is asked: the last one it has past that, `None` with none.
fn clamp_lod(chunk: Option<&DestructionChunkMeshes>, want: u8) -> Option<u8> {
    let last = chunk?.lods.len().checked_sub(1)?;
    Some(want.min(u8::try_from(last).unwrap_or(u8::MAX)))
}

fn is_broken(states: &[ChunkState]) -> bool {
    states.contains(&ChunkState::Detached)
}

/// Entities a draw may hang under: instance roots and debris bodies.
type DrawParents = Or<(With<DestructibleRoot>, With<DestructionChunk>)>;

/// What `sync_intact_meshes` reads per marked kit mesh.
type IntactRow = (
    Entity,
    &'static DestructionIntactMesh,
    Option<&'static MeshRenderer>,
    Option<&'static DestructionIntactHidden>,
);

/// A hidden kit mesh whose mark the game took off.
type UnmarkedRow = (Entity, &'static DestructionIntactHidden, Has<MeshRenderer>);

/// Rows `sync_intact_meshes` built in one app (tests insert it; app-local, so parallel tests
/// cannot add to it). An empty, clean tracker does not increment it.
#[cfg(test)]
#[derive(Resource, Default)]
pub(crate) struct IntactRowBuilds(pub(crate) u64);

/// Brings the chunk draws in line with the break model: spawns, moves and despawns them.
fn sync_chunk_draws(
    destruction: Option<Res<Destruction>>,
    meshes: Res<DestructionMeshes>,
    settings: Res<DestructionDrawSettings>,
    mut tracker: ResMut<DrawTracker>,
    mut stats: ResMut<DestructionDrawStats>,
    parents: Query<(), DrawParents>,
    mut commands: Commands,
) {
    let _span = tracing::debug_span!(target: "regia_framework", "destruction_draw_chunks").entered();
    let Some(destruction) = destruction else {
        // Shared field reads do not mark the tracker changed. An empty tracker has nothing to
        // despawn, so this frame must not take `&mut *tracker`.
        if tracker.instances.is_empty() {
            return;
        }
        let tracker = &mut *tracker;
        for tracked in std::mem::take(&mut tracker.instances).values() {
            despawn_instance(&mut commands, tracked);
        }
        let intact_hidden = stats.intact_hidden;
        *stats = DestructionDrawStats {
            intact_hidden,
            ..DestructionDrawStats::default()
        };
        return;
    };
    let dirty = destruction.is_changed() || meshes.is_changed() || settings.is_changed() || tracker.retry;
    if !dirty {
        return;
    }
    let tracker = &mut *tracker;
    tracker.retry = false;
    // A new debris LOD redraws everything.
    if tracker.debris_lod != Some(settings.debris_lod) {
        for tracked in std::mem::take(&mut tracker.instances).values() {
            despawn_instance(&mut commands, tracked);
        }
        tracker.debris_lod = Some(settings.debris_lod);
    }
    // Instances that left, were replaced (a snapshot), went back to whole, or changed set.
    let stale: Vec<DestructibleId> = tracker
        .instances
        .iter()
        .filter(|(id, t)| match destruction.world.instance(**id) {
            None => true,
            Some(inst) => {
                !Arc::ptr_eq(inst.asset(), &t.asset)
                    || destruction.root_entity(**id) != Some(t.root)
                    || meshes
                        .get(&inst.asset().name)
                        .is_none_or(|set| !Arc::ptr_eq(set, &t.set))
                    || !is_broken(inst.chunk_states())
            }
        })
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        if let Some(tracked) = tracker.instances.remove(&id) {
            despawn_instance(&mut commands, &tracked);
        }
    }

    let mut missing = BTreeSet::new();
    let mut missing_sets = 0;
    let mut mismatched_sets = 0;
    let mut retry = false;
    for (id, inst) in destruction.world.instances() {
        let asset = inst.asset();
        let Some(set) = meshes.get(&asset.name) else {
            missing_sets += 1;
            missing.insert(asset.name.clone());
            continue;
        };
        if set.chunks.len() != asset.chunks.len() {
            mismatched_sets += 1;
            continue;
        }
        if !is_broken(inst.chunk_states()) {
            continue;
        }
        let Some(root) = destruction.root_entity(id) else {
            // Spawned on the next fixed tick, which changes `Destruction`.
            continue;
        };
        if !parents.contains(root) {
            retry = true;
            continue;
        }
        let tracked = tracker.instances.entry(id).or_insert_with(|| TrackedInstance {
            root,
            asset: Arc::clone(asset),
            set: Arc::clone(set),
            attached: BTreeMap::new(),
            debris: BTreeMap::new(),
        });
        let drawn_set = Arc::clone(&tracked.set);
        let (attached, debris) = (&mut tracked.attached, &mut tracked.debris);
        for (c, state) in inst.chunk_states().iter().enumerate() {
            let c = c as u32;
            let chunk_meshes = drawn_set.chunks.get(c as usize);
            match state {
                ChunkState::Attached => {
                    if attached.contains_key(&c) {
                        continue;
                    }
                    let drawn = match clamp_lod(chunk_meshes, 0) {
                        Some(lod) => spawn_parts(
                            &mut commands,
                            &Part {
                                destructible: id,
                                chunk: c,
                                lod,
                                parent: root,
                                translation: Vec3::ZERO,
                                debris: false,
                                set: &drawn_set,
                            },
                        ),
                        None => Vec::new(),
                    };
                    attached.insert(c, drawn);
                }
                ChunkState::Detached => {
                    if let Some(drawn) = attached.remove(&c) {
                        despawn_all(&mut commands, &drawn);
                    }
                    let body = destruction.chunk_entity(id, c);
                    let have = debris.get(&c).map(|(b, _)| *b);
                    if body.is_some() && body == have {
                        continue;
                    }
                    if let Some((_, drawn)) = debris.remove(&c) {
                        despawn_all(&mut commands, &drawn);
                    }
                    let Some(body) = body else {
                        continue;
                    };
                    if !parents.contains(body) {
                        retry = true;
                        continue;
                    }
                    let centroid = asset
                        .chunks
                        .get(c as usize)
                        .map_or(Vec3::ZERO, |chunk| Vec3::from_array(chunk.centroid));
                    let drawn = match clamp_lod(chunk_meshes, settings.debris_lod) {
                        Some(lod) => spawn_parts(
                            &mut commands,
                            &Part {
                                destructible: id,
                                chunk: c,
                                lod,
                                parent: body,
                                translation: -centroid,
                                debris: true,
                                set: &drawn_set,
                            },
                        ),
                        None => Vec::new(),
                    };
                    debris.insert(c, (body, drawn));
                }
            }
        }
    }
    tracker.retry = retry;

    let intact_hidden = stats.intact_hidden;
    *stats = DestructionDrawStats {
        instances_drawn: tracker.instances.len(),
        attached_draws: tracker
            .instances
            .values()
            .flat_map(|t| t.attached.values())
            .map(Vec::len)
            .sum(),
        debris_draws: tracker
            .instances
            .values()
            .flat_map(|t| t.debris.values())
            .map(|(_, drawn)| drawn.len())
            .sum(),
        intact_hidden,
        missing_sets,
        mismatched_sets,
        missing: missing.into_iter().collect(),
    };
}

/// Hides the intact meshes of instances drawn in pieces and puts the others back.
fn sync_intact_meshes(
    settings: Res<DestructionDrawSettings>,
    tracker: Res<DrawTracker>,
    mut stats: ResMut<DestructionDrawStats>,
    intact: Query<IntactRow>,
    unmarked: Query<UnmarkedRow, Without<DestructionIntactMesh>>,
    mut commands: Commands,
    #[cfg(test)] builds: Option<ResMut<IntactRowBuilds>>,
) {
    let _span = tracing::debug_span!(target: "regia_framework", "destruction_draw_intact").entered();
    if tracker.instances.is_empty() && stats.intact_hidden == 0 && unmarked.is_empty() {
        return;
    }
    #[cfg(test)]
    if let Some(mut builds) = builds {
        builds.0 += 1;
    }
    let mut rows: Vec<_> = intact.iter().collect();
    rows.sort_unstable_by_key(|row| row.0);
    let mut hidden = 0;
    for (e, mark, renderer, stash) in rows {
        let hide = settings.hide_intact && tracker.instances.contains_key(&mark.0);
        match (hide, renderer, stash) {
            (true, Some(renderer), _) => {
                // A renderer the game put back while hidden replaces the stash.
                commands
                    .entity(e)
                    .try_remove::<MeshRenderer>()
                    .try_insert(DestructionIntactHidden(renderer.clone()));
                hidden += 1;
            }
            (true, None, Some(_)) => hidden += 1,
            (false, None, Some(stash)) => {
                commands
                    .entity(e)
                    .try_insert(stash.0.clone())
                    .try_remove::<DestructionIntactHidden>();
            }
            (false, Some(_), Some(_)) => {
                commands.entity(e).try_remove::<DestructionIntactHidden>();
            }
            (true, None, None) | (false, _, None) => {}
        }
    }
    // The game took the mark off a hidden mesh: give it back its renderer.
    let mut rows: Vec<_> = unmarked.iter().collect();
    rows.sort_unstable_by_key(|row| row.0);
    for (e, stash, has_renderer) in rows {
        let mut entity = commands.entity(e);
        if !has_renderer {
            entity.try_insert(stash.0.clone());
        }
        entity.try_remove::<DestructionIntactHidden>();
    }
    stats.intact_hidden = hidden;
}

#[cfg(test)]
mod tests {
    use regia_physics::destruction::{
        BoxGrid, Damage, DamageKind, DestructiblePose, DestructionMaterial, DestructionPlugin, GridSupport,
    };
    use regia_physics::{PhysicsPlugin, PhysicsWorld};

    use super::*;

    fn grid(name: &str) -> Arc<DestructibleAsset> {
        Arc::new(
            DestructibleAsset::box_grid(&BoxGrid {
                name: name.into(),
                counts: [3, 2, 1],
                cell: [0.5, 0.5, 0.5],
                density: 2000.0,
                strength_per_area: 200.0,
                support: GridSupport::Bottom,
                material: DestructionMaterial::default(),
            })
            .expect("grid"),
        )
    }

    fn tri(at: f32) -> Arc<CpuMesh> {
        Arc::new(CpuMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([at, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
                LabVertex::from_pos_normal_uv([at + 1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([at, 1.0, 0.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
            ],
            indices: vec![0, 1, 2],
        })
    }

    /// LOD 0 has a surface and cut faces, LOD 1 a surface only.
    fn mesh_set(asset: &DestructibleAsset) -> DestructionMeshSet {
        DestructionMeshSet {
            chunks: (0..asset.chunks.len())
                .map(|c| DestructionChunkMeshes {
                    lods: vec![
                        DestructionLodMeshes {
                            surface: Some(tri(c as f32)),
                            cut: Some(tri(c as f32 + 0.5)),
                        },
                        DestructionLodMeshes {
                            surface: Some(tri(c as f32 + 0.25)),
                            cut: None,
                        },
                    ],
                })
                .collect(),
            surface_material: Arc::new(CpuMaterial::default()),
            cut_material: Arc::new(CpuMaterial::default()),
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        app.insert_resource(FixedTime::default());
        app.add_plugins((PhysicsPlugin, DestructionPlugin::default(), DestructionDrawPlugin));
        app
    }

    fn define(app: &mut App, asset: Arc<DestructibleAsset>, x: f32) -> DestructibleId {
        app.world_mut().resource_scope(|world, mut d: Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            d.define(&mut physics, asset, DestructiblePose::at([x, 0.0, 0.0]), 7)
                .expect("define")
        })
    }

    fn next_tick(app: &mut App) {
        let tick = |app: &App| app.world().resource::<Destruction>().world.tick();
        let at = tick(app);
        for _ in 0..16 {
            app.update();
            if tick(app) > at {
                return;
            }
        }
        panic!("the fixed step stopped at tick {at}");
    }

    /// Chunk 4 (top middle) loses its three bonds and falls; the others stand.
    fn knock_out_chunk_4(app: &mut App, id: DestructibleId) {
        app.world_mut()
            .resource_mut::<Destruction>()
            .world
            .damage(Damage {
                target: Some(id),
                kind: DamageKind::Impulse {
                    chunk: 4,
                    impulse: [0.0, 0.0, -1000.0],
                },
            })
            .expect("queue");
    }

    struct Drawn {
        draw: DestructionChunkDraw,
        parent: Entity,
        mesh: Arc<CpuMesh>,
        translation: Vec3,
    }

    fn draws(app: &mut App, id: DestructibleId) -> Vec<Drawn> {
        let world = app.world_mut();
        let mut q = world.query::<(&DestructionChunkDraw, &ChildOf, &MeshRenderer, &Transform)>();
        let mut out: Vec<Drawn> = q
            .iter(world)
            .filter(|(d, ..)| d.destructible == id)
            .map(|(d, parent, r, t)| Drawn {
                draw: *d,
                parent: parent.parent(),
                mesh: Arc::clone(&r.mesh),
                translation: t.translation,
            })
            .collect();
        out.sort_by_key(|d| (d.draw.debris, d.draw.chunk, d.draw.cut));
        out
    }

    fn all_draws(app: &mut App) -> usize {
        let world = app.world_mut();
        world.query::<&DestructionChunkDraw>().iter(world).count()
    }

    fn same(a: &Arc<CpuMesh>, b: Option<&Arc<CpuMesh>>) -> bool {
        b.is_some_and(|b| Arc::ptr_eq(a, b))
    }

    #[test]
    fn a_break_draws_the_pieces_hides_the_kit_and_removal_cleans_up() {
        let mut app = app();
        let asset = grid("draw_wall");
        app.world_mut()
            .resource_mut::<DestructionMeshes>()
            .register("draw_wall", mesh_set(&asset));
        let set = Arc::clone(
            app.world()
                .resource::<DestructionMeshes>()
                .get("draw_wall")
                .expect("registered"),
        );
        // Asked past the last LOD: debris draws LOD 1.
        app.world_mut()
            .resource_mut::<DestructionDrawSettings>()
            .debris_lod = 9;
        let id = define(&mut app, Arc::clone(&asset), 0.0);
        let other = define(&mut app, grid("unregistered"), 10.0);
        let kit_mesh = tri(-5.0);
        let kit = app
            .world_mut()
            .spawn((
                DestructionIntactMesh(id),
                MeshRenderer::new(Arc::clone(&kit_mesh), Arc::new(CpuMaterial::default())),
                Transform::default(),
            ))
            .id();
        next_tick(&mut app);
        next_tick(&mut app);

        assert_eq!(all_draws(&mut app), 0, "nothing draws before a break");
        assert!(app.world().get::<MeshRenderer>(kit).is_some());
        let stats = app.world().resource::<DestructionDrawStats>().clone();
        assert_eq!(stats.instances_drawn, 0);
        assert_eq!(stats.missing_sets, 1);
        assert_eq!(stats.missing, vec!["unregistered".to_string()]);

        knock_out_chunk_4(&mut app, id);
        knock_out_chunk_4(&mut app, other);
        next_tick(&mut app);
        app.update();

        let destruction = app.world().resource::<Destruction>();
        let root = destruction.root_entity(id).expect("root");
        let body = destruction.chunk_entity(id, 4).expect("chunk 4 is debris");
        let states = destruction
            .world
            .instance(id)
            .expect("instance")
            .chunk_states()
            .to_vec();
        assert_eq!(states[4], ChunkState::Detached);
        assert_eq!(states.iter().filter(|s| **s == ChunkState::Attached).count(), 5);

        let drawn = draws(&mut app, id);
        let (attached, debris): (Vec<&Drawn>, Vec<&Drawn>) = drawn.iter().partition(|d| !d.draw.debris);
        assert_eq!(attached.len(), 10, "surface and cut faces of five chunks");
        for d in &attached {
            let c = d.draw.chunk as usize;
            assert_ne!(c, 4);
            assert_eq!(d.parent, root);
            assert_eq!(d.draw.lod, 0);
            assert_eq!(d.translation, Vec3::ZERO);
            let lod0 = &set.chunks[c].lods[0];
            let want = if d.draw.cut {
                lod0.cut.as_ref()
            } else {
                lod0.surface.as_ref()
            };
            assert!(same(&d.mesh, want), "chunk {c} cut {}", d.draw.cut);
        }
        assert_eq!(debris.len(), 1, "LOD 1 has no cut faces");
        let d = debris[0];
        assert_eq!((d.draw.chunk, d.draw.lod, d.draw.cut), (4, 1, false));
        assert_eq!(d.parent, body);
        assert!(same(&d.mesh, set.chunks[4].lods[1].surface.as_ref()));
        assert_eq!(d.translation, -Vec3::from_array(asset.chunks[4].centroid));

        assert!(
            app.world().get::<MeshRenderer>(kit).is_none(),
            "the kit is hidden"
        );
        assert!(app.world().get::<DestructionIntactHidden>(kit).is_some());
        assert!(draws(&mut app, other).is_empty(), "no set, no draws");
        let stats = app.world().resource::<DestructionDrawStats>().clone();
        assert_eq!(
            (stats.instances_drawn, stats.attached_draws, stats.debris_draws),
            (1, 10, 1)
        );
        assert_eq!(
            (stats.intact_hidden, stats.missing_sets, stats.mismatched_sets),
            (1, 1, 0)
        );

        app.world_mut().resource_scope(|world, mut d: Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            assert!(d.remove(&mut physics, id));
        });
        next_tick(&mut app);
        app.update();

        assert_eq!(all_draws(&mut app), 0, "the draws went with their parents");
        assert!(app.world().resource::<DrawTracker>().instances.is_empty());
        let stats = app.world().resource::<DestructionDrawStats>().clone();
        assert_eq!(
            (
                stats.instances_drawn,
                stats.attached_draws,
                stats.debris_draws,
                stats.intact_hidden
            ),
            (0, 0, 0, 0)
        );
        let back = app.world().get::<MeshRenderer>(kit).expect("the kit is back");
        assert!(Arc::ptr_eq(&back.mesh, &kit_mesh));
        assert!(app.world().get::<DestructionIntactHidden>(kit).is_none());
    }

    #[test]
    fn a_set_with_another_chunk_count_draws_nothing() {
        let mut app = app();
        let asset = grid("short");
        let mut set = mesh_set(&asset);
        set.chunks.pop();
        app.world_mut()
            .resource_mut::<DestructionMeshes>()
            .register("short", set);
        let id = define(&mut app, asset, 0.0);
        next_tick(&mut app);
        knock_out_chunk_4(&mut app, id);
        next_tick(&mut app);
        app.update();
        assert_eq!(all_draws(&mut app), 0);
        let stats = app.world().resource::<DestructionDrawStats>().clone();
        assert_eq!((stats.mismatched_sets, stats.instances_drawn), (1, 0));
    }

    #[test]
    fn the_registry_keys_sets_by_asset_name() {
        let asset = grid("a");
        let mut meshes = DestructionMeshes::default();
        assert!(meshes.is_empty());
        assert!(meshes.register("a", mesh_set(&asset)).is_none());
        assert!(meshes.register("a", mesh_set(&asset)).is_some(), "replaced");
        meshes.register("b", mesh_set(&asset));
        assert_eq!(meshes.len(), 2);
        assert_eq!(meshes.names().collect::<Vec<_>>(), ["a", "b"]);
        assert!(meshes.get("a").is_some());
        assert!(meshes.remove("a").is_some());
        assert!(meshes.get("a").is_none());
        assert_eq!(meshes.len(), 1);
    }

    /// Four vertices, two triangles: the first in slot 0, the second in slot 1, 16-bit indices.
    fn two_slot_mesh() -> CookedMesh {
        let positions = [
            [0.0f32, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.5],
        ];
        let uvs = [[0u16, 0], [0x3C00, 0], [0x3C00, 0x3C00], [0, 0x3C00]];
        let mut mesh = CookedMesh {
            aabb_min: [0.0; 3],
            aabb_max: [1.0, 1.0, 0.5],
            bounding_sphere: [0.5, 0.5, 0.25, 0.8],
            stream_positions: Vec::new(),
            stream_normals_tangents: Vec::new(),
            stream_uv0: Vec::new(),
            indices: Vec::new(),
            index_width: CookedIndexWidth::U16,
            vertex_count: 4,
            lods: Vec::new(),
            submeshes: vec![
                CookedSubmesh {
                    index_byte_offset: 0,
                    index_byte_length: 6,
                    material_slot: 0,
                },
                CookedSubmesh {
                    index_byte_offset: 6,
                    index_byte_length: 6,
                    material_slot: 1,
                },
            ],
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
            let (x, y) = regia_asset::encode_octahedral_normal([0.0, 0.0, 1.0]);
            for w in [x, y, 0, 1] {
                mesh.stream_normals_tangents.extend_from_slice(&w.to_le_bytes());
            }
            for h in uv {
                mesh.stream_uv0.extend_from_slice(&h.to_le_bytes());
            }
        }
        for i in [0u16, 1, 2, 0, 2, 3] {
            mesh.indices.extend_from_slice(&i.to_le_bytes());
        }
        mesh
    }

    /// The compact two-slot chunk as the desktop pak stores it: GPU-ready layout v2 with its
    /// submesh table, built the way `regia_asset_pipeline`'s pak writer builds it.
    fn gpu_two_slot_file(with_submeshes: bool) -> Vec<u8> {
        let mesh = two_slot_mesh();
        let payload = mesh.to_postcard().expect("postcard");
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh().0,
            asset_id: AssetId::new_random(),
            importer_version: 1,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        let compact = regia_asset::encode_cooked_file(&header, &payload).expect("file");
        let gpu = regia_render::cooked_model::gpu_mesh_file_from_cooked(&compact)
            .expect("convert")
            .expect("a rigid mesh converts");
        let (gh, gp) = regia_asset::decode_cooked_file(&gpu).expect("gpu file");
        let view = GpuMeshView::parse(gp).expect("parse");
        let indices: Vec<u32> = view
            .indices
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        let subs: Vec<regia_asset::GpuMeshSubmesh> = if with_submeshes {
            mesh.submeshes
                .iter()
                .map(|s| regia_asset::GpuMeshSubmesh {
                    first_index: s.index_byte_offset / 2,
                    index_count: s.index_byte_length / 2,
                    material_slot: s.material_slot,
                })
                .collect()
        } else {
            Vec::new()
        };
        regia_asset::encode_gpu_mesh_file_with_submeshes(
            gh.asset_id,
            gh.importer_version,
            gh.dependencies,
            &view.info,
            &subs,
            view.vertices,
            &indices,
        )
        .expect("encode")
    }

    /// A chunk mesh stored GPU-ready keeps its cut faces: the selection is the loose file's.
    #[test]
    fn a_gpu_ready_chunk_splits_like_the_loose_one() {
        let loose = split_cooked_chunk(&two_slot_mesh()).expect("loose");
        let packed = decode_chunk_file(&gpu_two_slot_file(true)).expect("packed");
        let (ls, lc) = (loose.surface.expect("surface"), loose.cut.expect("cut"));
        let (ps, pc) = (packed.surface.expect("surface"), packed.cut.expect("cut"));
        assert_eq!(ps.indices, ls.indices);
        assert_eq!(pc.indices, lc.indices);
        let at = |m: &CpuMesh| m.vertices.iter().map(|v| v.position).collect::<Vec<_>>();
        assert_eq!(at(&ps), at(&ls));
        assert_eq!(at(&pc), at(&lc));
        // A payload without a submesh table (the old shape) is all surface, no cut.
        let old = decode_chunk_file(&gpu_two_slot_file(false)).expect("no ranges");
        assert_eq!(old.surface.expect("surface").indices.len(), 6);
        assert!(old.cut.is_none());
    }

    #[test]
    fn a_cooked_chunk_splits_into_surface_and_cut_faces() {
        let mesh = two_slot_mesh();
        let lod = split_cooked_chunk(&mesh).expect("split");
        let surface = lod.surface.expect("surface");
        let cut = lod.cut.expect("cut");
        assert_eq!(surface.indices, [0, 1, 2]);
        assert_eq!(cut.indices, [0, 1, 2], "remapped to its own vertices");
        fn at(m: &CpuMesh) -> Vec<[f32; 3]> {
            m.vertices.iter().map(|v| v.position).collect()
        }
        assert_eq!(at(&surface), [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0]]);
        assert_eq!(at(&cut), [[0.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.5]]);
        assert!(
            surface
                .vertices
                .iter()
                .chain(&cut.vertices)
                .all(|v| v.tangent.iter().all(|t| t.is_finite()))
        );

        // No submesh: all surface.
        let mut whole = two_slot_mesh();
        whole.submeshes.clear();
        let lod = split_cooked_chunk(&whole).expect("whole");
        assert_eq!(lod.surface.expect("surface").indices.len(), 6);
        assert!(lod.cut.is_none());

        // A range past the buffer, a misaligned one, and one that is not whole triangles.
        for (offset, length) in [(6, 12), (1, 6), (0, 4)] {
            let mut bad = two_slot_mesh();
            bad.submeshes[1].index_byte_offset = offset;
            bad.submeshes[1].index_byte_length = length;
            assert!(split_cooked_chunk(&bad).is_err(), "{offset} {length}");
        }
    }

    #[test]
    fn a_set_from_cooked_meshes_checks_the_chunk_count() {
        let asset = grid("cooked");
        let surface = Arc::new(CpuMaterial::default());
        let cut = Arc::new(CpuMaterial::default());
        let chunks: Vec<Vec<CookedMesh>> = (0..asset.chunks.len()).map(|_| vec![two_slot_mesh()]).collect();
        let set = mesh_set_from_cooked(&asset, &chunks, Arc::clone(&surface), Arc::clone(&cut)).expect("set");
        assert_eq!(set.chunks.len(), 6);
        assert!(
            set.chunks
                .iter()
                .all(|c| c.lods.len() == 1 && c.lods[0].cut.is_some())
        );
        assert!(Arc::ptr_eq(&set.cut_material, &cut));

        let err = mesh_set_from_cooked(&asset, &chunks[..5], Arc::clone(&surface), Arc::clone(&cut))
            .expect_err("short");
        assert_eq!(err.code(), "destruction_mesh_count");
        let mut broken = chunks;
        broken[2][0].submeshes[0].index_byte_length = 7;
        let err = mesh_set_from_cooked(&asset, &broken, surface, cut).expect_err("decode");
        assert_eq!(err.code(), "destruction_mesh_decode");
        assert!(matches!(err, DestructionDrawError::Decode { chunk: 2, .. }));
    }

    #[test]
    fn mesh_references_split_at_the_hash() {
        assert_eq!(
            split_reference("assets/wall.glb#fracture/chunk_3"),
            Some(("assets/wall.glb", Some("fracture/chunk_3")))
        );
        assert_eq!(
            split_reference("assets/rock.glb"),
            Some(("assets/rock.glb", None))
        );
        for bad in ["", "#fracture/chunk_0", "assets/wall.glb#"] {
            assert_eq!(split_reference(bad), None, "{bad}");
        }
        let parts = DestructionLodMeshes::from_cpu_parts(
            Some(&regia_asset::CpuMesh {
                positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 3],
                uvs: vec![[0.0; 2]; 3],
                indices: vec![0, 1, 2],
                ..Default::default()
            }),
            None,
        );
        assert_eq!(parts.surface.map(|m| m.vertices.len()), Some(3));
        assert!(parts.cut.is_none());
    }

    #[test]
    fn error_codes_follow_the_protocol_grammar() {
        let errors = [
            DestructionDrawError::BadPath {
                chunk: 0,
                path: String::new(),
            },
            DestructionDrawError::Missing {
                chunk: 0,
                path: String::new(),
                reason: String::new(),
            },
            DestructionDrawError::Decode {
                chunk: 0,
                path: String::new(),
                reason: String::new(),
            },
            DestructionDrawError::Count {
                name: String::new(),
                expected: 1,
                found: 0,
            },
        ];
        for e in errors {
            assert!(
                e.code()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                e.code()
            );
        }
    }

    /// Intact rows are built only when a break, a hidden kit, or an unmarked stash needs work.
    #[test]
    fn an_idle_intact_set_is_not_rebuilt() {
        let mut app = app();
        for i in 0..8u32 {
            app.world_mut()
                .spawn((DestructionIntactMesh(DestructibleId(i)), Transform::default()));
        }
        app.update();
        app.init_resource::<IntactRowBuilds>();
        app.update();
        assert_eq!(
            app.world().resource::<IntactRowBuilds>().0,
            0,
            "an empty tracker does not collect the intact meshes"
        );
    }
}
