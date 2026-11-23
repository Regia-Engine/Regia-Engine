// SPDX-License-Identifier: MPL-2.0
//! The streamed-level cook (`docs/specs/78-world-streaming.md` §§ 5.3, 6.4,
//! `docs/formats/world-stream.md`): partitions a level into cells and writes one cooked blob per
//! non-empty cell, the always-loaded set and the world index, last, so a cancelled cook never leaves
//! an index pointing at missing blobs. The first file a cook changes removes the old index first
//! (ADR-8812): a cook stopped between them leaves the level not cooked, never an old index over new
//! blob bytes.
//!
//! Output is byte-identical for the same sources: inputs are sorted and nothing records a time. A
//! blob that did not change is not rewritten, so an edit near one corner of a large world touches
//! only that corner's cells.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_asset::{AssetId, ManifestEntry, cooked_file_name};
use regia_core::id::Guid;
use regia_core::math::{Affine3A, Vec3, Vec3A};
use regia_ecs::prelude::*;
use regia_ecs::reflect::TypeRegistry;
use regia_ecs::serialize::ReflectCodec;
use regia_nav::{
    MAX_STREAMED_GRID_CELLS, NavAgentParams, NavMeshBuildParams, NavTileCache, NavTileCacheParams,
};
use regia_physics::{BodyKind, Collider, ColliderShape, RigidBody};
use regia_render::MeshSource;
use regia_render::primitives::Primitive;
use regia_render::{PointLight, SpotLight};
use regia_scene::Transform;
use regia_scene::cooked::{COOKED_SCENE_VERSION, CookedScene};
use regia_scene::floating_origin::LargeSpace;
use regia_scene::format::{SceneEntity, SceneFile};
use regia_scene::hlod::{CellProxy, HlodTree};
use regia_scene::level::LevelId;
use regia_scene::stream_grid::{CellCoord, StreamGrid};
use regia_scene::streaming::{LevelStreaming, StreamNavLayout, StreamPlacement, StreamPriorityVolume};
use regia_world::format::{
    CELL_BLOB_FORMAT_VERSION, CellBlob, CellRecord, HlodIndex, WORLD_INDEX_FORMAT_VERSION, WorldFormatError,
    WorldIndex, always_loaded_id, always_loaded_to_bytes, cell_blob_id, hlod_proxy_id, world_index_id,
};
use serde::Serialize;
use thiserror::Error;

use crate::hlod_atlas::{BakeImage, BakeMaterial};
use crate::hlod_cook::MeshSoup;
pub use scatter::{HLOD_SCATTER_VOXEL_RESOLUTION, ScatterCoverage, StreamScatter};

/// The manifest type path of a world index.
pub const WORLD_INDEX_TYPE_PATH: &str = "regia_world::format::WorldIndex";
/// The manifest type path of a cell blob.
pub const CELL_BLOB_TYPE_PATH: &str = "regia_world::format::CellBlob";
/// The manifest type path of an always-loaded set.
pub const ALWAYS_LOADED_TYPE_PATH: &str = "regia_world::format::AlwaysLoaded";

const PERSISTENT_TYPE: &str = "regia_scene::level::Persistent";
const REPLICATED_SUFFIX: &str = "::Replicated";
const RIGID_BODY_SUFFIX: &str = "::RigidBody";

/// Why a streamed level did not cook.
#[derive(Debug, Error)]
pub enum StreamCookError {
    /// The settings are invalid. The message names the field (`stream_settings_invalid`).
    #[error("stream_settings_invalid: {0}")]
    Settings(String),
    /// The scene did not validate or cook.
    #[error("scene: {0}")]
    Scene(String),
    /// A cooked file did not encode.
    #[error(transparent)]
    Format(#[from] WorldFormatError),
    /// A write failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },
}

/// What the cook knows about a referenced asset.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AssetCost {
    /// CPU bytes while resident.
    pub ram_bytes: u64,
    /// GPU bytes while resident.
    pub vram_bytes: u64,
    /// Local bounds `[min, max]` for a mesh.
    pub bounds: Option<[[f32; 3]; 2]>,
}

/// The cook's view of the project's assets. Every guid in a component value that this knows is a
/// dependency of the cell.
pub trait StreamAssets {
    /// The cost of `id`, or `None` when it is not an asset.
    fn cost(&self, id: AssetId) -> Option<AssetCost>;

    /// The triangles of a `MeshSource` path, for bounds and proxies.
    fn mesh(&self, _path: &str) -> Option<MeshSoup> {
        None
    }

    /// The assets a `MeshSource` path or material path names.
    fn assets_of(&self, _path: &str) -> Vec<AssetId> {
        Vec::new()
    }

    /// The terrains the level stands on, for the nav cook (spec 78 § 13.2). A game spawns its
    /// `regia_terrain::Terrain` itself, so a level file does not name it; the caller of the cook
    /// does, and the cook walks the same surface (holes cut) the run-time nav reads from that
    /// component. Default: none.
    fn nav_terrains(&self) -> &[NavTerrain] {
        &[]
    }

    /// The farthest a client sees replicated entities of this level, meters: the largest spatial
    /// relevance radius times `1 + hysteresis` over the net profiles the level uses (spec 78 § 14).
    /// The cook warns `stream_relevance_radius` when it exceeds `load_radius_m`. Default: unknown, no
    /// check.
    fn relevance_radius_m(&self) -> Option<f64> {
        None
    }

    /// The material a `MeshSource` names (a `.material.ron` path), decoded for the HLOD atlas bake
    /// (ADR-2483): factors and maps. Only asked when the level bakes atlases. Default: none, and the
    /// bake reads vertex colors only.
    fn bake_material(&self, _path: &str) -> Option<Arc<BakeMaterial>> {
        None
    }

    /// The platform tag the HLOD atlas textures are compressed for (`android` writes ASTC, other
    /// tags BCn; ADR-2483). Default: the host's.
    fn platform(&self) -> &str {
        crate::platform::host_platform()
    }

    /// The terrain scatter of the level (ADR-3253, spec 78 § 11.6): the cook places it, gathers each
    /// cell's instances and voxelizes them per HLOD node into a voxel proxy. Like the terrain, a
    /// game spawns its scatter itself, so the level file does not name it; the caller of the cook
    /// does ([`WithScatter`]). Default: none, and the cook writes exactly what it wrote before.
    fn scatter(&self) -> &[StreamScatter] {
        &[]
    }

    /// Voxels along the longest axis of a node's scatter at its finest level (1 to 256, rounded up
    /// to a power of two). Default [`HLOD_SCATTER_VOXEL_RESOLUTION`].
    fn scatter_voxel_resolution(&self) -> u32 {
        HLOD_SCATTER_VOXEL_RESOLUTION
    }
}

/// Another [`StreamAssets`] plus the level's terrain scatter ([`StreamAssets::scatter`]): how a host
/// that spawns its own terrain and scatter hands them to the HLOD cook (ADR-3253).
pub struct WithScatter<'a> {
    /// Everything else.
    pub assets: &'a dyn StreamAssets,
    /// The scatter sets, in the order they are placed.
    pub scatter: Vec<StreamScatter>,
    /// Voxels along the longest axis of a node's scatter ([`StreamAssets::scatter_voxel_resolution`]).
    pub resolution: u32,
}

impl StreamAssets for WithScatter<'_> {
    fn cost(&self, id: AssetId) -> Option<AssetCost> {
        self.assets.cost(id)
    }

    fn bake_material(&self, path: &str) -> Option<Arc<BakeMaterial>> {
        self.assets.bake_material(path)
    }

    fn platform(&self) -> &str {
        self.assets.platform()
    }

    fn mesh(&self, path: &str) -> Option<MeshSoup> {
        self.assets.mesh(path)
    }

    fn assets_of(&self, path: &str) -> Vec<AssetId> {
        self.assets.assets_of(path)
    }

    fn nav_terrains(&self) -> &[NavTerrain] {
        self.assets.nav_terrains()
    }

    fn relevance_radius_m(&self) -> Option<f64> {
        self.assets.relevance_radius_m()
    }

    fn scatter(&self) -> &[StreamScatter] {
        &self.scatter
    }

    fn scatter_voxel_resolution(&self) -> u32 {
        self.resolution
    }
}

/// Another [`StreamAssets`] plus the level's net relevance reach
/// ([`StreamAssets::relevance_radius_m`]): how a networked project hands it to the cook.
pub struct WithNetRelevance<'a> {
    /// Everything else.
    pub assets: &'a dyn StreamAssets,
    /// The largest spatial relevance radius times `1 + hysteresis`, meters.
    pub radius_m: f64,
}

impl StreamAssets for WithNetRelevance<'_> {
    fn cost(&self, id: AssetId) -> Option<AssetCost> {
        self.assets.cost(id)
    }

    fn bake_material(&self, path: &str) -> Option<Arc<BakeMaterial>> {
        self.assets.bake_material(path)
    }

    fn platform(&self) -> &str {
        self.assets.platform()
    }

    fn mesh(&self, path: &str) -> Option<MeshSoup> {
        self.assets.mesh(path)
    }

    fn assets_of(&self, path: &str) -> Vec<AssetId> {
        self.assets.assets_of(path)
    }

    fn nav_terrains(&self) -> &[NavTerrain] {
        self.assets.nav_terrains()
    }

    fn relevance_radius_m(&self) -> Option<f64> {
        Some(self.radius_m)
    }

    fn scatter(&self) -> &[StreamScatter] {
        self.assets.scatter()
    }

    fn scatter_voxel_resolution(&self) -> u32 {
        self.assets.scatter_voxel_resolution()
    }
}

/// A terrain surface for the nav cook: the heightfield and world mapping of a
/// `regia_terrain::Terrain` and the holes cut through it. It is in the terrain's world-anchored
/// space, which is the cook's world space.
#[derive(Debug, Clone)]
pub struct NavTerrain {
    /// The heightfield and its world mapping (`Terrain::map`).
    pub map: regia_terrain::TerrainMap,
    /// Holes punched through the surface (`Terrain::holes`, or the applied runtime mask).
    pub holes: Option<regia_terrain::HolesMask>,
}

impl NavTerrain {
    /// The surface of `terrain`, holes included.
    #[must_use]
    pub fn of(terrain: &regia_terrain::Terrain) -> Self {
        Self {
            map: terrain.map.clone(),
            holes: terrain.holes.clone(),
        }
    }

    /// Appends the surface's triangles, in row order, exactly as the run-time nav geometry
    /// (`regia_framework::nav_world::level_triangles_in`) reads them.
    fn triangles(&self, out: &mut Vec<[Vec3; 3]>) {
        let (w, h) = (self.map.width(), self.map.height());
        if w < 2 || h < 2 {
            return;
        }
        let [x0, z0] = self.map.sample_to_world(0, 0);
        let [x1, z1] = self.map.sample_to_world(w - 1, h - 1);
        let (min, max) = (
            Vec3::new(x0.min(x1), f32::MIN, z0.min(z1)),
            Vec3::new(x0.max(x1), f32::MAX, z0.max(z1)),
        );
        regia_framework::nav_world::terrain_triangles_in(&self.map, self.holes.as_ref(), min, max, out);
    }
}

/// Another [`StreamAssets`] plus the terrains the level stands on ([`StreamAssets::nav_terrains`]):
/// how a host that spawns its own terrain hands it to the nav cook.
pub struct WithNavTerrain<'a> {
    /// Everything else.
    pub assets: &'a dyn StreamAssets,
    /// The terrains, in the order their triangles go to the cook.
    pub terrains: Vec<NavTerrain>,
}

impl StreamAssets for WithNavTerrain<'_> {
    fn cost(&self, id: AssetId) -> Option<AssetCost> {
        self.assets.cost(id)
    }

    fn bake_material(&self, path: &str) -> Option<Arc<BakeMaterial>> {
        self.assets.bake_material(path)
    }

    fn platform(&self) -> &str {
        self.assets.platform()
    }

    fn mesh(&self, path: &str) -> Option<MeshSoup> {
        self.assets.mesh(path)
    }

    fn assets_of(&self, path: &str) -> Vec<AssetId> {
        self.assets.assets_of(path)
    }

    fn nav_terrains(&self) -> &[NavTerrain] {
        &self.terrains
    }

    fn relevance_radius_m(&self) -> Option<f64> {
        self.assets.relevance_radius_m()
    }

    fn scatter(&self) -> &[StreamScatter] {
        self.assets.scatter()
    }

    fn scatter_voxel_resolution(&self) -> u32 {
        self.assets.scatter_voxel_resolution()
    }
}

/// No asset information: entities use their own position and cells have no dependencies.
impl StreamAssets for () {
    fn cost(&self, _id: AssetId) -> Option<AssetCost> {
        None
    }
}

/// The manifest path up to the first `#`. `foo` and `foo#mesh/0` share a stem; `foo-extra` does not.
fn manifest_stem(path: &str) -> &str {
    path.split_once('#').map_or(path, |(stem, _)| stem)
}

/// Groups manifest indices by `manifest_stem`, preserving manifest order inside each group.
fn index_manifest(entries: &[ManifestEntry]) -> HashMap<String, Vec<usize>> {
    let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        by_stem
            .entry(manifest_stem(&entry.path).to_owned())
            .or_default()
            .push(index);
    }
    by_stem
}

/// Assets read from a cooked cache directory: file size as the cost, cooked meshes by manifest path.
pub struct CacheAssets {
    cache_dir: PathBuf,
    manifest: Vec<ManifestEntry>,
    /// Indices into `manifest`, grouped by the path before the first `#`, in manifest order.
    by_stem: HashMap<String, Vec<usize>>,
    platform: String,
    /// The project the cache belongs to: material graph sources are read from it (ADR-2713).
    project_root: Option<PathBuf>,
    /// Cooked file length in bytes, or `None` when the file is missing. Filled by the first `cost`.
    lengths: Mutex<HashMap<AssetId, Option<u64>>>,
    /// A decoded HLOD atlas image, or `None` when the texture cannot be decoded. Filled by the
    /// first `bake_image` of that id.
    images: Mutex<HashMap<AssetId, Option<Arc<BakeImage>>>>,
    /// Manifest entries examined by `assets_of` and `mesh`. Building `by_stem` counts as one walk
    /// per entry. Tests assert this stays linear in entities plus manifest size.
    #[cfg(test)]
    manifest_walks: AtomicU64,
    /// `metadata` calls made for cooked-file lengths. Tests assert a repeated id costs one.
    #[cfg(test)]
    metadata_calls: AtomicU64,
    /// Textures decoded for the HLOD atlas. Tests assert one decode per asset id.
    #[cfg(test)]
    bake_image_decodes: AtomicU64,
}

impl CacheAssets {
    /// Reads from `cache_dir`, cooked for the host platform.
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self::for_platform(cache_dir, crate::platform::host_platform())
    }

    /// Reads from `cache_dir`, cooked for `platform` (the tag HLOD atlas textures compress for).
    /// The asset rows come from the manifest the last cook wrote into the cache.
    pub fn for_platform(cache_dir: impl Into<PathBuf>, platform: &str) -> Self {
        let cache_dir = cache_dir.into();
        let manifest = regia_asset::AssetManifest::read_from_cache(&cache_dir)
            .ok()
            .flatten()
            .unwrap_or_default()
            .entries;
        Self::with_manifest(cache_dir, platform, manifest)
    }

    /// Reads cooked files from `cache_dir` but takes the asset rows from `manifest`: the cook's
    /// in-memory rows, which name what this run cooked before the manifest reaches the disk.
    pub fn with_manifest(
        cache_dir: impl Into<PathBuf>,
        platform: &str,
        manifest: Vec<ManifestEntry>,
    ) -> Self {
        let cache_dir = cache_dir.into();
        let by_stem = index_manifest(&manifest);
        Self {
            cache_dir,
            #[cfg(test)]
            manifest_walks: AtomicU64::new(manifest.len() as u64),
            manifest,
            by_stem,
            platform: platform.to_owned(),
            project_root: None,
            lengths: Mutex::new(HashMap::new()),
            images: Mutex::new(HashMap::new()),
            #[cfg(test)]
            metadata_calls: AtomicU64::new(0),
            #[cfg(test)]
            bake_image_decodes: AtomicU64::new(0),
        }
    }

    /// Counts manifest entries a lookup examined. Release builds do not keep the counter.
    fn count_walks(&self, examined: usize) {
        #[cfg(test)]
        let _ = self.manifest_walks.fetch_add(examined as u64, Ordering::Relaxed);
        #[cfg(not(test))]
        let _ = (self, examined);
    }

    /// The cooked file's length, remembered after the first stat. `None` is remembered too, so a
    /// missing file is not stated again.
    fn cooked_length(&self, id: AssetId) -> Option<u64> {
        let mut known = self.lengths.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(len) = known.get(&id) {
            return *len;
        }
        #[cfg(test)]
        let _ = self.metadata_calls.fetch_add(1, Ordering::Relaxed);
        let len = std::fs::metadata(self.cache_dir.join(cooked_file_name(id)))
            .ok()
            .map(|meta| meta.len());
        known.insert(id, len);
        len
    }

    /// Manifest entries `assets_of` and `mesh` have examined.
    #[cfg(test)]
    fn manifest_walks(&self) -> u64 {
        self.manifest_walks.load(Ordering::Relaxed)
    }

    /// Cooked-file `metadata` calls.
    #[cfg(test)]
    fn metadata_calls(&self) -> u64 {
        self.metadata_calls.load(Ordering::Relaxed)
    }

    /// Textures decoded by `bake_image`.
    #[cfg(test)]
    fn bake_image_decodes(&self) -> u64 {
        self.bake_image_decodes.load(Ordering::Relaxed)
    }

    /// Reads material graph and instance sources from `project_root` for the HLOD atlas bake,
    /// which folds them (ADR-2713); without it a graph-material mesh bakes its vertex colors.
    #[must_use]
    pub fn with_project_root(mut self, project_root: impl Into<PathBuf>) -> Self {
        self.project_root = Some(project_root.into());
        self
    }

    /// A cooked texture of the cache decoded to RGBA8 (level 0), or `None`. Each id is decoded once.
    fn bake_image(&self, id: AssetId) -> Option<Arc<BakeImage>> {
        let mut known = self.images.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(image) = known.get(&id) {
            return image.clone();
        }
        #[cfg(test)]
        let _ = self.bake_image_decodes.fetch_add(1, Ordering::Relaxed);
        let image = self.decode_bake_image(id);
        known.insert(id, image.clone());
        image
    }

    /// Reads and decodes one cooked texture. Callers remember the result, including `None`.
    fn decode_bake_image(&self, id: AssetId) -> Option<Arc<BakeImage>> {
        let bytes = std::fs::read(self.cache_dir.join(cooked_file_name(id))).ok()?;
        let (_, texture) = regia_asset::load_texture_bytes(&bytes).ok()?;
        let decoded = regia_render::CompressedTexture::from_cooked(&texture)
            .ok()?
            .decode_rgba8()?;
        BakeImage::new(decoded.width, decoded.height, decoded.rgba).map(Arc::new)
    }
}

impl StreamAssets for CacheAssets {
    fn cost(&self, id: AssetId) -> Option<AssetCost> {
        let bytes = self.cooked_length(id)?;
        Some(AssetCost {
            ram_bytes: bytes,
            vram_bytes: bytes,
            bounds: None,
        })
    }

    fn assets_of(&self, path: &str) -> Vec<AssetId> {
        let Some(bucket) = self.by_stem.get(manifest_stem(path)) else {
            return Vec::new();
        };
        self.count_walks(bucket.len());
        bucket
            .iter()
            .filter_map(|&index| {
                let entry = &self.manifest[index];
                let child = entry
                    .path
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('#'));
                (entry.path == path || child).then_some(entry.id)
            })
            .collect()
    }

    fn bake_material(&self, path: &str) -> Option<Arc<BakeMaterial>> {
        let found = self.by_stem.get(manifest_stem(path)).and_then(|bucket| {
            bucket.iter().map(|&index| &self.manifest[index]).find(|entry| {
                entry.path == path
                    && regia_core::brand::current_type_path(&entry.type_path)
                        == "regia_asset::CookedStandardMaterial"
            })
        });
        let Some(entry) = found else {
            let root = self.project_root.as_deref()?;
            return crate::hlod_atlas::read_graph_bake_material(root, path).map(Arc::new);
        };
        let bytes = std::fs::read(self.cache_dir.join(cooked_file_name(entry.id))).ok()?;
        let (_, cooked) = regia_asset::load_material_bytes(&bytes).ok()?;
        let base = &cooked.authored.base;
        let state = &cooked.authored.render_state;
        let image = |slot: &Option<regia_asset::TextureSlot>| match slot {
            Some(regia_asset::TextureSlot::Asset(id)) => self.bake_image(*id),
            _ => None,
        };
        Some(Arc::new(BakeMaterial {
            base_color: base.base_color,
            metallic: base.metallic,
            roughness: base.roughness,
            base_color_map: image(&base.base_color_map),
            normal_map: image(&base.normal_map),
            orm_map: image(&base.orm_map),
            orm_packed: base.orm_packed,
            alpha_cutoff: (state.alpha_mode == regia_asset::AlphaMode::Mask).then_some(state.alpha_cutoff),
            vertex_color: true,
        }))
    }

    fn platform(&self) -> &str {
        &self.platform
    }

    fn mesh(&self, path: &str) -> Option<MeshSoup> {
        let bucket = self.by_stem.get(manifest_stem(path))?;
        self.count_walks(bucket.len());
        let mut out = MeshSoup::default();
        for &index in bucket {
            let entry = &self.manifest[index];
            let mesh_child = entry
                .path
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with("#mesh/"));
            if !mesh_child {
                continue;
            }
            // A listed submesh whose file is missing fails the whole soup, as the linear scan did.
            let file = std::fs::read(self.cache_dir.join(cooked_file_name(entry.id))).ok()?;
            if let Ok(loaded) = regia_asset::load_mesh_bytes(&file) {
                out.append(&MeshSoup::from_cpu(&loaded.mesh), &Affine3A::IDENTITY);
            }
        }
        (!out.is_empty()).then_some(out)
    }
}

/// A warning with its stable code (spec 78 § 17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamWarning {
    /// `stream_large_entity`, `stream_cross_cell_ref`, ...
    pub code: &'static str,
    /// What and where.
    pub message: String,
}

/// One cell of a partition.
#[derive(Debug, Clone, Default)]
pub struct PartitionedCell {
    /// Hierarchies sorted by root guid, each contiguous and root first. Root transforms are
    /// relative to the anchor.
    pub entities: Vec<SceneEntity>,
    /// Lowest point of the content.
    pub min_y: f64,
    /// Highest point of the content.
    pub max_y: f64,
    /// Highest overlapping priority volume.
    pub priority: i32,
    /// Referenced assets.
    pub deps: BTreeSet<AssetId>,
    /// Entities with replication.
    pub replicated: Vec<Guid>,
    /// The anchor in the origin space.
    pub anchor: [i64; 3],
    /// Static meshes with their world poses, for the proxy.
    pub meshes: Vec<(Affine3A, Arc<MeshSoup>)>,
}

/// Where every entity of a level went.
#[derive(Debug, Clone)]
pub struct Partition {
    /// The validated settings.
    pub settings: LevelStreaming,
    /// Entities that spawn with the level, with the level's resources.
    pub always: SceneFile,
    /// The non-empty cells.
    pub cells: BTreeMap<CellCoord, PartitionedCell>,
    /// Lowest and highest cell of the world, inclusive.
    pub bounds: [CellCoord; 2],
    /// Problems for the author.
    pub warnings: Vec<StreamWarning>,
}

/// The cook's report (spec 78 § 6.4), printed by `regia assets stream-report`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StreamReport {
    /// Non-empty cells.
    pub cells: usize,
    /// Entities in the always-loaded set.
    pub always_entities: usize,
    /// Fewest entities in a cell.
    pub entities_min: u32,
    /// Median entities per cell.
    pub entities_median: u32,
    /// Most entities in a cell.
    pub entities_max: u32,
    /// Bytes of all cell blobs.
    pub cell_bytes: u64,
    /// Largest cell blob.
    pub cell_bytes_max: u64,
    /// Bytes of the always-loaded set.
    pub always_bytes: u64,
    /// Blobs written.
    pub written: usize,
    /// Blobs left as they were.
    pub unchanged: usize,
    /// HLOD proxies built.
    pub proxies: usize,
    /// Bytes of the HLOD proxies (voxel proxies included).
    pub proxy_bytes: u64,
    /// Problems for the author.
    pub warnings: Vec<StreamWarning>,
    /// HLOD nodes with a voxel proxy of the terrain scatter (ADR-3253); 0 without scatter.
    pub voxel_proxies: usize,
    /// Bytes of those voxel proxies (part of `proxy_bytes`).
    pub voxel_bytes: u64,
    /// Scatter instances the cook placed into cells for them.
    pub scatter_instances: u64,
}

/// What a cook produced.
#[derive(Debug, Clone, Default)]
pub struct StreamCookOutput {
    /// The report.
    pub report: StreamReport,
    /// Manifest rows of every output.
    pub manifest: Vec<ManifestEntry>,
}

/// Decodes a bare RON value of a type the registry knows.
fn decode<T: Reflect + TypePath>(
    codec: &mut ReflectCodec<'_>,
    registry: &TypeRegistry,
    text: &str,
) -> Result<T, String> {
    let registration = registry
        .get_with_type_path(T::type_path())
        .ok_or_else(|| format!("`{}` is not registered", T::type_path()))?;
    let value = codec
        .from_ron_typed(text, registration)
        .map_err(|e| e.to_string())?;
    value
        .downcast::<T>()
        .map(|v| *v)
        .map_err(|v| format!("decoded a {}", v.reflect_type_path()))
}

/// The streaming settings of a level, if it has any, validated. `None`: not streamed.
pub fn level_streaming(
    scene: &SceneFile,
    registry: &TypeRegistry,
) -> Result<Option<LevelStreaming>, StreamCookError> {
    let Some(raw) = scene.resources.get(LevelStreaming::type_path()) else {
        return Ok(None);
    };
    let mut codec = ReflectCodec::new(registry);
    let settings =
        decode::<LevelStreaming>(&mut codec, registry, raw.get_ron()).map_err(StreamCookError::Settings)?;
    if !settings.enabled {
        return Ok(None);
    }
    settings.validate().map_err(StreamCookError::Settings)?;
    Ok(Some(settings))
}

/// The types the cook reads, on top of whatever the caller's registry holds.
pub fn register_cook_types(registry: &mut TypeRegistry) {
    regia_scene::components::register_primitives(registry);
    registry.register::<Transform>();
    registry.register::<LevelStreaming>();
    registry.register::<StreamPlacement>();
    registry.register::<StreamPriorityVolume>();
    registry.register::<Collider>();
    registry.register::<RigidBody>();
    registry.register::<PointLight>();
    registry.register::<SpotLight>();
    registry.register::<Primitive>();
    registry.register::<MeshSource>();
    registry.register::<regia_framework::nav_world::NavAreaZone>();
    registry.register::<regia_framework::nav_world::NavSurface>();
    registry.register::<regia_framework::nav_world::NavAreaCostTable>();
    registry.register::<regia_scene::streaming::TerrainTileCollision>();
}

/// An axis-aligned box.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    min: Vec3,
    max: Vec3,
}

impl Bounds {
    fn point(p: Vec3) -> Self {
        Self { min: p, max: p }
    }

    fn add(&mut self, other: Bounds) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    /// The world box of a local box.
    fn transformed(world: &Affine3A, min: Vec3, max: Vec3) -> Self {
        let mut out: Option<Bounds> = None;
        for corner in 0..8u32 {
            let p = Vec3::new(
                if corner & 1 == 0 { min.x } else { max.x },
                if corner & 2 == 0 { min.y } else { max.y },
                if corner & 4 == 0 { min.z } else { max.z },
            );
            let q = Vec3::from(world.transform_point3a(Vec3A::from(p)));
            match &mut out {
                Some(b) => b.add(Bounds::point(q)),
                None => out = Some(Bounds::point(q)),
            }
        }
        out.unwrap_or(Bounds::point(Vec3::ZERO))
    }
}

/// Half extents of the shapes whose size the shape itself states.
///
/// Capsule, cylinder and cone are authored in Y-up shape space (the long axis is +Y, per
/// `regia_physics::components::ShapeAxis`); the half extents are rotated into the collider's
/// frame by the shape's axis before the bounds transform.
fn shape_half_extents(shape: &ColliderShape) -> Option<Vec3> {
    match shape {
        ColliderShape::Box { half_extents } => Some(*half_extents),
        ColliderShape::Sphere { radius } => Some(Vec3::splat(*radius)),
        // The capsule's caps add a radius along the long axis on top of the cylindrical half.
        ColliderShape::Capsule {
            half_height,
            radius,
            axis,
        } => Some(axis.from_y_rotation() * Vec3::new(*radius, half_height + radius, *radius)),
        ColliderShape::Cylinder {
            half_height,
            radius,
            axis,
        }
        | ColliderShape::Cone {
            half_height,
            radius,
            axis,
        } => Some(axis.from_y_rotation() * Vec3::new(*radius, *half_height, *radius)),
        // Terrain: as wide as the map, so it stays with the level.
        ColliderShape::HeightField { scale, .. } => Some(Vec3::new(scale.x * 0.5, 0.0, scale.z * 0.5)),
        _ => None,
    }
}

/// Every guid written in `text` (`8-4-4-4-12` hexadecimal).
fn guids_in(text: &str) -> Vec<Guid> {
    const DASHES: [usize; 4] = [8, 13, 18, 23];
    const LEN: usize = 36;
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at + LEN <= bytes.len() {
        let window = &bytes[at..at + LEN];
        let shaped = window.iter().enumerate().all(|(i, b)| {
            if DASHES.contains(&i) {
                *b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        });
        if shaped
            && let Ok(text) = std::str::from_utf8(window)
            && let Ok(guid) = text.parse::<Guid>()
        {
            out.push(guid);
            at += LEN;
        } else {
            at += 1;
        }
    }
    out
}

/// What the partition knows about one entity.
struct EntityInfo {
    local: Option<Transform>,
    assets: Vec<(AssetId, AssetCost)>,
    refs: Vec<Guid>,
    mesh: Option<Arc<MeshSoup>>,
    /// How far a point or spot light reaches, meters: it lights what is inside that sphere.
    light_reach: Option<f32>,
}

/// Meshes by the component text that names them: a level repeats a few props many times. With
/// `bake` (the level bakes HLOD atlases) a `MeshSource`'s material is read for the bake as well.
#[derive(Default)]
struct MeshCache {
    meshes: HashMap<String, Option<Arc<MeshSoup>>>,
    bake: bool,
}

/// The triangles of an entity's `Primitive` or `MeshSource`.
fn entity_mesh(
    entity: &SceneEntity,
    codec: &mut ReflectCodec<'_>,
    registry: &TypeRegistry,
    assets: &dyn StreamAssets,
    cache: &mut MeshCache,
) -> Option<Arc<MeshSoup>> {
    if let Some(raw) = entity.components.get(Primitive::type_path()) {
        if let Some(found) = cache.meshes.get(raw.get_ron()) {
            return found.clone();
        }
        let soup = decode::<Primitive>(codec, registry, raw.get_ron())
            .ok()
            .and_then(|p| p.shape.mesh().ok().map(|mesh| (mesh, p.color)))
            .map(|(mesh, color)| Arc::new(MeshSoup::from_primitive(&mesh).tinted(color)));
        cache.meshes.insert(raw.get_ron().to_owned(), soup.clone());
        return soup;
    }
    let raw = entity.components.get(MeshSource::type_path())?;
    if let Some(found) = cache.meshes.get(raw.get_ron()) {
        return found.clone();
    }
    let bake = cache.bake;
    let soup = decode::<MeshSource>(codec, registry, raw.get_ron())
        .ok()
        .and_then(|source| {
            let soup = MeshSoup::builtin(&source.mesh).or_else(|| assets.mesh(&source.mesh))?;
            let material = (bake && !source.material.is_empty())
                .then(|| assets.bake_material(&source.material))
                .flatten();
            Some(soup.with_material(material))
        })
        .map(Arc::new);
    cache.meshes.insert(raw.get_ron().to_owned(), soup.clone());
    soup
}

fn entity_info(
    entity: &SceneEntity,
    guids: &BTreeSet<Guid>,
    codec: &mut ReflectCodec<'_>,
    registry: &TypeRegistry,
    assets: &dyn StreamAssets,
    cache: &mut MeshCache,
) -> EntityInfo {
    let local = entity
        .components
        .get(Transform::type_path())
        .and_then(|raw| decode::<Transform>(codec, registry, raw.get_ron()).ok());
    let mut asset_rows = BTreeMap::new();
    let mut refs = BTreeSet::new();
    // A mesh reference names assets by path; everything else by guid.
    if let Some(raw) = entity.components.get(MeshSource::type_path())
        && let Ok(source) = decode::<MeshSource>(codec, registry, raw.get_ron())
    {
        for path in [&source.mesh, &source.material] {
            for id in assets.assets_of(path) {
                if let Some(cost) = assets.cost(id) {
                    asset_rows.insert(id, cost);
                }
            }
        }
    }
    for raw in entity.components.values() {
        for guid in guids_in(raw.get_ron()) {
            if guids.contains(&guid) {
                if guid != entity.guid {
                    refs.insert(guid);
                }
            } else if let Some(cost) = assets.cost(AssetId(guid)) {
                asset_rows.insert(AssetId(guid), cost);
            }
        }
    }
    EntityInfo {
        local,
        assets: asset_rows.into_iter().collect(),
        refs: refs.into_iter().collect(),
        mesh: entity_mesh(entity, codec, registry, assets, cache),
        light_reach: entity_light_reach(entity, codec, registry),
    }
}

/// The reach of the entity's point or spot light, if it has one.
fn entity_light_reach(
    entity: &SceneEntity,
    codec: &mut ReflectCodec<'_>,
    registry: &TypeRegistry,
) -> Option<f32> {
    let point = entity
        .components
        .get(PointLight::type_path())
        .and_then(|raw| decode::<PointLight>(codec, registry, raw.get_ron()).ok())
        .map(|light| light.radius);
    let spot = entity
        .components
        .get(SpotLight::type_path())
        .and_then(|raw| decode::<SpotLight>(codec, registry, raw.get_ron()).ok())
        .map(|light| light.range);
    point.or(spot).filter(|reach| reach.is_finite() && *reach > 0.0)
}

fn matrix(t: &Transform) -> Affine3A {
    Affine3A::from_scale_rotation_translation(t.scale, t.rotation, t.translation)
}

/// One hierarchy during the partition.
struct Hierarchy {
    root: usize,
    /// Indices into the sorted entities, root first, depth first by guid.
    members: Vec<usize>,
    bounds: Option<Bounds>,
    /// Static meshes with their world poses.
    meshes: Vec<(Affine3A, Arc<MeshSoup>)>,
}

/// Partitions a level by the rules of spec 78 § 5.3.
pub fn partition(
    scene: &SceneFile,
    settings: LevelStreaming,
    registry: &TypeRegistry,
    assets: &dyn StreamAssets,
) -> Result<Partition, StreamCookError> {
    let _span = tracing::info_span!("stream_partition", entities = scene.entities.len()).entered();
    let stripped = crate::cook::strip_review_notes(scene);
    let scene = &stripped;
    scene
        .validate()
        .map_err(|e| StreamCookError::Scene(e.to_string()))?;
    let grid = settings
        .grid()
        .map_err(|e| StreamCookError::Settings(e.to_string()))?;
    let space = LargeSpace::new(settings.origin.cell_size_m)
        .map_err(|e| StreamCookError::Settings(format!("origin.cell_size_m: {e}")))?;
    let mut codec = ReflectCodec::new(registry);

    // Rule 1: guid order, whatever order the file came in.
    let mut entities: Vec<&SceneEntity> = scene.entities.iter().collect();
    entities.sort_by_key(|e| e.guid);
    let index: HashMap<Guid, usize> = entities.iter().enumerate().map(|(i, e)| (e.guid, i)).collect();
    let guids: BTreeSet<Guid> = index.keys().copied().collect();
    let mut meshes = MeshCache {
        bake: settings.hlod_atlas_enabled(),
        ..MeshCache::default()
    };
    let infos: Vec<EntityInfo> = entities
        .iter()
        .map(|e| entity_info(e, &guids, &mut codec, registry, assets, &mut meshes))
        .collect();

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); entities.len()];
    let mut roots = Vec::new();
    for (i, entity) in entities.iter().enumerate() {
        match entity.parent.and_then(|p| index.get(&p)) {
            Some(parent) => children[*parent].push(i),
            None => roots.push(i),
        }
    }

    // Rule 2: a hierarchy moves as one, with world bounds over all its members.
    let mut hierarchies = Vec::with_capacity(roots.len());
    // The world pose of every entity with a transform (parent chain composed), for the priority
    // volumes below.
    let mut entity_worlds: Vec<Option<Affine3A>> = vec![None; entities.len()];
    for &root in &roots {
        let mut members = Vec::new();
        let mut bounds: Option<Bounds> = None;
        let mut hierarchy_meshes = Vec::new();
        let mut stack = vec![(root, Affine3A::IDENTITY)];
        while let Some((i, parent)) = stack.pop() {
            members.push(i);
            let world = parent * infos[i].local.as_ref().map_or(Affine3A::IDENTITY, matrix);
            if infos[i].local.is_some() {
                entity_worlds[i] = Some(world);
                let mut grow = |b: Bounds| match &mut bounds {
                    Some(all) => all.add(b),
                    None => bounds = Some(b),
                };
                grow(Bounds::point(Vec3::from(world.translation)));
                if let Some(reach) = infos[i].light_reach {
                    grow(Bounds::transformed(
                        &world,
                        Vec3::splat(-reach),
                        Vec3::splat(reach),
                    ));
                }
                for (_, cost) in &infos[i].assets {
                    if let Some([min, max]) = cost.bounds {
                        grow(Bounds::transformed(
                            &world,
                            Vec3::from_array(min),
                            Vec3::from_array(max),
                        ));
                    }
                }
                if let Some(mesh) = &infos[i].mesh {
                    if let Some([min, max]) = mesh.bounds() {
                        grow(Bounds::transformed(
                            &world,
                            Vec3::from_array(min),
                            Vec3::from_array(max),
                        ));
                    }
                    // Only static meshes go into a proxy: a body would leave its proxy behind.
                    if !entities[i]
                        .components
                        .keys()
                        .any(|k| k.ends_with(RIGID_BODY_SUFFIX))
                    {
                        hierarchy_meshes.push((world, Arc::clone(mesh)));
                    }
                }
                if let Some(raw) = entities[i].components.get(Collider::type_path())
                    && let Ok(collider) = decode::<Collider>(&mut codec, registry, raw.get_ron())
                    && let Some(half) = shape_half_extents(&collider.shape)
                {
                    let body = world * matrix(&collider.offset);
                    grow(Bounds::transformed(&body, -half, half));
                }
            }
            // Reversed, so the stack pops the lowest guid first.
            for &child in children[i].iter().rev() {
                stack.push((child, world));
            }
        }
        hierarchies.push(Hierarchy {
            root,
            members,
            bounds,
            meshes: hierarchy_meshes,
        });
    }

    let mut warnings = Vec::new();
    let mut always: Vec<usize> = Vec::new();
    let mut placed: BTreeMap<CellCoord, Vec<&Hierarchy>> = BTreeMap::new();
    let mut cell_of_entity: HashMap<usize, CellCoord> = HashMap::new();
    let limit = settings.max_entity_extent_cells * settings.cell_size_m;
    for hierarchy in &hierarchies {
        let root = entities[hierarchy.root];
        let placement = root
            .components
            .get(StreamPlacement::type_path())
            .and_then(|raw| decode::<StreamPlacement>(&mut codec, registry, raw.get_ron()).ok())
            .unwrap_or_default();
        // Rule 3: pinned, persistent, without a spatial extent, or too wide for a cell.
        let mut cell = None;
        let keep_with_level =
            placement == StreamPlacement::AlwaysLoaded || root.components.contains_key(PERSISTENT_TYPE);
        if !keep_with_level && infos[hierarchy.root].local.is_some() {
            let bounds = hierarchy.bounds.unwrap_or(Bounds::point(Vec3::ZERO));
            let size = bounds.max - bounds.min;
            if f64::from(size.x.max(size.z)) > limit {
                warnings.push(StreamWarning {
                    code: "stream_large_entity",
                    message: format!(
                        "entity {} `{}` is {:.1} x {:.1} m wide, over the {limit:.1} m limit; it goes to the \
                         always-loaded set",
                        root.guid,
                        root.name.as_deref().unwrap_or(""),
                        size.x,
                        size.z
                    ),
                });
            } else if let StreamPlacement::Cell { x, z } = placement {
                // Rule 4.
                cell = Some(CellCoord::new(x, z));
            } else {
                // Rule 5: the center of the bounds; half-open cells leave no seam.
                let center = (bounds.min + bounds.max) * 0.5;
                cell = Some(
                    grid.cell_of(f64::from(center.x), f64::from(center.z))
                        .map_err(|e| StreamCookError::Scene(format!("entity {}: {e}", root.guid)))?,
                );
            }
        }
        match cell {
            Some(cell) => {
                for member in &hierarchy.members {
                    cell_of_entity.insert(*member, cell);
                }
                placed.entry(cell).or_default().push(hierarchy);
            }
            None => always.extend(hierarchy.members.iter().copied()),
        }
    }

    // Priority volumes: cook-time data on any entity with a transform. A volume is the entity's
    // box (`half_extents` about its world pose, parent chain, rotation and scale included) and
    // covers the cells its world XZ bounds overlap. Only the cells that hold entities read a
    // priority, so each volume is kept as its cell rectangle and the cells look it up: a volume
    // spanning millions of cells costs one rectangle, not one map entry per cell (audit
    // asset_pipeline1 #3).
    let mut volumes: Vec<(CellCoord, CellCoord, i32)> = Vec::new();
    for (i, entity) in entities.iter().enumerate() {
        let Some(raw) = entity.components.get(StreamPriorityVolume::type_path()) else {
            continue;
        };
        let (Ok(volume), Some(world)) = (
            decode::<StreamPriorityVolume>(&mut codec, registry, raw.get_ron()),
            entity_worlds[i].as_ref(),
        ) else {
            continue;
        };
        let half = volume.half_extents;
        if !(half.is_finite() && half.min_element() > 0.0) {
            warnings.push(StreamWarning {
                code: "stream_priority_volume_invalid",
                message: format!(
                    "entity {} `{}`: priority half extents ({}, {}, {}) are not finite and \
                     positive; the volume is ignored",
                    entity.guid,
                    entity.name.as_deref().unwrap_or(""),
                    half.x,
                    half.y,
                    half.z
                ),
            });
            continue;
        }
        let bounds = Bounds::transformed(world, -half, half);
        let corner = |x: f32, z: f32| {
            grid.cell_of(f64::from(x), f64::from(z)).map_err(|e| {
                StreamCookError::Scene(format!(
                    "entity {} `{}`: StreamPriorityVolume half_extents {:?}: {e}",
                    entity.guid,
                    entity.name.as_deref().unwrap_or(""),
                    volume.half_extents
                ))
            })
        };
        let lo = corner(bounds.min.x, bounds.min.z)?;
        let hi = corner(bounds.max.x, bounds.max.z)?;
        volumes.push((lo, hi, volume.priority));
    }
    let priority_of = |coord: CellCoord| {
        volumes
            .iter()
            .filter(|(lo, hi, _)| (lo.x..=hi.x).contains(&coord.x) && (lo.z..=hi.z).contains(&coord.z))
            .map(|(_, _, priority)| *priority)
            .max()
            .unwrap_or(0)
    };

    // Rule 7: references between cells are weak.
    let mut seen_pairs = BTreeSet::new();
    for (i, info) in infos.iter().enumerate() {
        let Some(from) = cell_of_entity.get(&i) else {
            continue;
        };
        for target in &info.refs {
            if let Some(to) = index.get(target).and_then(|j| cell_of_entity.get(j))
                && to != from
                && seen_pairs.insert((entities[i].guid, *target))
            {
                warnings.push(StreamWarning {
                    code: "stream_cross_cell_ref",
                    message: format!(
                        "entity {} in cell {from} refers to {target} in cell {to}: the reference reads as absent \
                         while either cell is not spawned",
                        entities[i].guid
                    ),
                });
            }
        }
    }

    // Build the cells: anchor, root transforms relative to it, Y ranges (rule 6).
    let mut cells = BTreeMap::new();
    for (coord, hierarchies) in placed {
        let center = grid.cell_center(coord);
        let anchor = space
            .from_world([center[0], 0.0, center[1]])
            .map_err(|e| StreamCookError::Scene(format!("cell {coord}: {e}")))?
            .cell;
        let anchor_center: [f64; 3] = std::array::from_fn(|a| anchor[a] as f64 * space.cell_size());
        let mut cell = PartitionedCell {
            priority: priority_of(coord),
            anchor,
            min_y: f64::INFINITY,
            max_y: f64::NEG_INFINITY,
            ..PartitionedCell::default()
        };
        for hierarchy in hierarchies {
            cell.meshes.extend(hierarchy.meshes.iter().cloned());
            if let Some(bounds) = hierarchy.bounds {
                cell.min_y = cell.min_y.min(f64::from(bounds.min.y));
                cell.max_y = cell.max_y.max(f64::from(bounds.max.y));
            }
            for &member in &hierarchy.members {
                let mut entity = entities[member].clone();
                cell.deps.extend(infos[member].assets.iter().map(|(id, _)| *id));
                if entity.components.keys().any(|k| k.ends_with(REPLICATED_SUFFIX)) {
                    cell.replicated.push(entity.guid);
                }
                if member == hierarchy.root
                    && let Some(mut local) = infos[member].local
                {
                    let world: [f64; 3] = std::array::from_fn(|a| f64::from(local.translation[a]));
                    local.translation = Vec3::new(
                        (world[0] - anchor_center[0]) as f32,
                        (world[1] - anchor_center[1]) as f32,
                        (world[2] - anchor_center[2]) as f32,
                    );
                    let text = codec
                        .to_ron_typed(local.as_partial_reflect())
                        .map_err(|e| StreamCookError::Scene(format!("entity {}: {e}", entity.guid)))?;
                    let raw = ron::value::RawValue::from_boxed_ron(text.into_boxed_str())
                        .map_err(|e| StreamCookError::Scene(format!("entity {}: {e}", entity.guid)))?;
                    entity.components.insert(Transform::type_path().to_owned(), raw);
                }
                cell.entities.push(entity);
            }
        }
        if !cell.min_y.is_finite() {
            cell.min_y = 0.0;
            cell.max_y = 0.0;
        }
        cells.insert(coord, cell);
    }

    always.sort_unstable();
    let bounds = [
        CellCoord::new(
            cells.keys().map(|c| c.x).min().unwrap_or(0),
            cells.keys().map(|c| c.z).min().unwrap_or(0),
        ),
        CellCoord::new(
            cells.keys().map(|c| c.x).max().unwrap_or(0),
            cells.keys().map(|c| c.z).max().unwrap_or(0),
        ),
    ];
    let always_scene = SceneFile {
        format_version: scene.format_version,
        entities: always.iter().map(|i| entities[*i].clone()).collect(),
        resources: scene.resources.clone(),
    };
    Ok(Partition {
        settings,
        always: always_scene,
        cells,
        bounds,
        warnings,
    })
}

/// A scene file of `entities`, for the cooked form. Cell blobs do not carry level resources.
fn cell_scene(scene: &SceneFile, entities: Vec<SceneEntity>) -> SceneFile {
    SceneFile {
        format_version: scene.format_version,
        entities,
        resources: BTreeMap::new(),
    }
}

thread_local! {
    /// The world index the running [`cook_partition`] on this thread removes before it writes its
    /// first changed file (ADR-8812). `None` once removed, or outside a cook.
    static STALE_INDEX: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Arms `STALE_INDEX` for one [`cook_partition`] call and restores the previous value on drop
/// (also when the cook fails or is stopped).
struct StaleIndexGuard {
    previous: Option<PathBuf>,
}

impl StaleIndexGuard {
    fn arm(index: PathBuf) -> Self {
        Self {
            previous: STALE_INDEX.with(|slot| slot.replace(Some(index))),
        }
    }
}

impl Drop for StaleIndexGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        STALE_INDEX.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Before the first changed file of a cook: removes the level's old world index, so a cook that
/// stops before it writes the new index leaves the level "not cooked" (`stream_not_cooked`)
/// instead of an old index naming new blob bytes (ADR-8812). Writing the index itself removes
/// nothing.
fn remove_stale_index(writing: &Path) -> Result<(), StreamCookError> {
    STALE_INDEX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_deref().is_none_or(|index| index == writing) {
            return Ok(());
        }
        let Some(index) = slot.take() else {
            return Ok(());
        };
        match std::fs::remove_file(&index) {
            Ok(()) => {
                tracing::debug!(target: "regia_asset_pipeline", index = %index.display(), "old world index removed before the first changed blob");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => {
                let error = StreamCookError::Io {
                    path: index.clone(),
                    source,
                };
                *slot = Some(index);
                Err(error)
            }
        }
    })
}

/// Writes atomically unless the file already holds `bytes`. Returns whether it wrote. Inside a
/// [`cook_partition`] the first changed file first removes the level's old world index
/// (ADR-8812).
pub(crate) fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool, StreamCookError> {
    let io = |source| StreamCookError::Io {
        path: path.to_owned(),
        source,
    };
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(false);
    }
    remove_stale_index(path)?;
    // pid + counter temp name, removed on either failure (audit concurrency-1103 pipeline M1/L5).
    crate::cook_ledger::write_atomic(path, bytes).map_err(io)?;
    #[cfg(test)]
    if test_hooks::note_write() {
        return Err(io(std::io::Error::other("cook stopped by the test hook")));
    }
    Ok(true)
}

/// Removes `path` if it exists. Returns whether it removed it. Like [`write_if_changed`], inside a
/// [`cook_partition`] the removal of a file the old index may still describe first removes that
/// index (ADR-8812, ADR-8860); a missing file changes nothing and removes nothing.
pub(crate) fn remove_if_present(path: &Path) -> Result<bool, StreamCookError> {
    let io = |source| StreamCookError::Io {
        path: path.to_owned(),
        source,
    };
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io(e)),
    }
    remove_stale_index(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io(e)),
    }
    #[cfg(test)]
    if test_hooks::note_write() {
        return Err(io(std::io::Error::other("cook stopped by the test hook")));
    }
    Ok(true)
}

/// Test-only counters and the stop hook of [`write_if_changed`] and [`remove_if_present`] (per
/// thread: a cook runs on one).
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::Cell;

    thread_local! {
        static WRITES: Cell<u64> = const { Cell::new(0) };
        static STOP_AFTER: Cell<Option<u64>> = const { Cell::new(None) };
    }

    /// Files [`super::write_if_changed`] wrote (and [`super::remove_if_present`] removed) on this
    /// thread.
    pub(crate) fn file_writes() -> u64 {
        WRITES.with(Cell::get)
    }

    /// Makes the `n`th write from now fail after it lands, as a cancel right after that file
    /// would (`None` disarms).
    pub(crate) fn stop_after_writes(n: Option<u64>) {
        STOP_AFTER.with(|s| s.set(n));
    }

    /// Counts one write; `true` when the armed stop is reached.
    pub(super) fn note_write() -> bool {
        WRITES.with(|w| w.set(w.get() + 1));
        STOP_AFTER.with(|s| match s.get() {
            Some(n) if n <= 1 => {
                s.set(None);
                true
            }
            Some(n) => {
                s.set(Some(n - 1));
                false
            }
            None => false,
        })
    }
}

/// Median of a sorted slice.
fn median(sorted: &[u32]) -> u32 {
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

/// The median bounding radius of the leaves, for the derived HLOD threshold.
fn median_leaf_radius(tree: &HlodTree) -> f64 {
    let mut radii: Vec<f64> = tree
        .nodes()
        .iter()
        .filter(|n| n.level == 0)
        .map(|n| n.radius)
        .collect();
    radii.sort_by(f64::total_cmp);
    radii.get(radii.len() / 2).copied().unwrap_or(1.0)
}

/// One node's proxy on disk.
struct BuiltProxies {
    index: HlodIndex,
    manifest: Vec<ManifestEntry>,
    bytes: u64,
    built: usize,
    /// The voxel table (ADR-3253), when any node has a voxel proxy.
    voxels: Option<regia_world::hlod_voxels::HlodVoxels>,
}

/// Where the HLOD proxies go: the cache (none for a report), the level's manifest path and the
/// platform its atlas textures compress for.
struct HlodOutput<'a> {
    cache_dir: Option<&'a Path>,
    rel_path: &'a str,
    platform: &'a str,
    /// The level's scatter by cell (ADR-3253); empty without scatter.
    scatter: &'a scatter::ScatterCells,
}

/// The HLOD tree of the cells with its proxy meshes (spec 78 § 11.1), the threshold derived from the
/// load radius unless authored. Writes the meshes when `cache_dir` is given. With the level's
/// `hlod_atlas_px` above 0 each proxy also bakes an atlas and its material (ADR-2483).
fn hlod_index(
    level: LevelId,
    partition: &Partition,
    grid: &StreamGrid,
    out: HlodOutput<'_>,
    warnings: &mut Vec<StreamWarning>,
) -> Result<Option<BuiltProxies>, StreamCookError> {
    let HlodOutput {
        cache_dir,
        rel_path,
        platform,
        scatter,
    } = out;
    let Some(hlod) = &partition.settings.hlod else {
        return Ok(None);
    };
    let _span = tracing::info_span!("hlod_cook", cells = partition.cells.len()).entered();
    // Cell proxies first: their size goes into the tree.
    let mut leaves: BTreeMap<CellCoord, (MeshSoup, Option<[f32; 4]>)> = BTreeMap::new();
    let mut proxies = Vec::with_capacity(partition.cells.len());
    // Each leaf's estimated texture bytes: the bake replaces them with the bytes written (ADR-2713).
    let mut estimated: BTreeMap<CellCoord, u64> = BTreeMap::new();
    // Each scatter leaf's voxel proxy (ADR-3253), cooked before the tree so its bytes go into it,
    // anchored at its cell's center: (proxy, anchor, instances).
    let mut leaf_voxels: BTreeMap<CellCoord, (regia_render::visbuffer::VoxelFarProxy, [i64; 3], u32)> =
        BTreeMap::new();
    let space = LargeSpace::new(partition.settings.origin.cell_size_m)
        .map_err(|e| StreamCookError::Settings(format!("origin.cell_size_m: {e}")))?;
    let empty_cell = PartitionedCell::default();
    let mut coords: BTreeSet<CellCoord> = partition.cells.keys().copied().collect();
    coords.extend(scatter.cells.keys().copied());
    for coord in &coords {
        let cell = partition.cells.get(coord).unwrap_or(&empty_cell);
        let mut merged = MeshSoup::default();
        for (world, mesh) in &cell.meshes {
            merged.append(mesh, world);
        }
        // The color is measured before simplification, which drops area.
        let color = merged.mean_color();
        let leaf = merged.simplified(hlod.leaf_triangle_ratio, hlod.min_triangles);
        // A baked proxy holds its textures as well: the residency budget counts them (ADR-2483). A
        // parent's share follows from `proxy_byte_ratio`, as its mesh's does.
        let atlas_bytes = if partition.settings.hlod_atlas_enabled() {
            crate::hlod_atlas::estimated_texture_bytes(
                &leaf,
                crate::hlod_atlas::AtlasSettings::for_level(
                    partition.settings.hlod_atlas_px,
                    partition.settings.hlod_atlas_max_px,
                    0,
                ),
            )
        } else {
            0
        };
        estimated.insert(*coord, atlas_bytes);
        let (mut min_y, mut max_y) = (cell.min_y, cell.max_y);
        let mut voxel_bytes = 0u64;
        if let Some(trees) = scatter.cells.get(coord) {
            if partition.cells.contains_key(coord) {
                min_y = min_y.min(trees.min_y);
                max_y = max_y.max(trees.max_y);
            } else {
                (min_y, max_y) = (trees.min_y, trees.max_y);
            }
            let center = grid.cell_center(*coord);
            let anchor = space
                .from_world([center[0], 0.0, center[1]])
                .map_err(|e| StreamCookError::Scene(format!("cell {coord}: {e}")))?
                .cell;
            let offset: [f64; 3] = std::array::from_fn(|a| anchor[a] as f64 * space.cell_size());
            match scatter::voxelize(
                &format!("{rel_path}#hlod/cell/{}", coord.label()),
                &trees.soup,
                offset,
                scatter.coverage.as_ref(),
                scatter.resolution,
            ) {
                Ok(proxy) => {
                    voxel_bytes = proxy.byte_size() as u64;
                    leaf_voxels.insert(*coord, (proxy, anchor, trees.instances));
                }
                Err(error) => warnings.push(StreamWarning {
                    code: "stream_hlod_voxel_failed",
                    message: format!("cell {coord}: {error}; its scatter has no voxel proxy"),
                }),
            }
        }
        proxies.push(CellProxy {
            cell: *coord,
            min_y,
            max_y,
            bytes: (leaf.positions.len() * 24 + leaf.indices.len() * 4) as u64 + atlas_bytes + voxel_bytes,
        });
        leaves.insert(*coord, (leaf, color));
    }
    let settings_error = |e: regia_scene::hlod::HlodError| StreamCookError::Settings(format!("hlod: {e}"));
    let load = partition.settings.load_radius_m;
    // The first build only measures the leaves.
    let probe =
        HlodTree::build(grid, hlod.config(load, grid.cell_size()), &proxies).map_err(settings_error)?;
    let config = hlod.config(load, median_leaf_radius(&probe));
    let tree = HlodTree::build(grid, config, &proxies).map_err(settings_error)?;
    let leaf_count = tree.nodes().iter().filter(|n| n.level == 0).count();
    let beyond = tree
        .nodes()
        .iter()
        .filter(|n| n.level == 0 && n.switch_distance > load)
        .count();
    if beyond * 20 > leaf_count {
        warnings.push(StreamWarning {
            code: "stream_hlod_switch_beyond_load",
            message: format!(
                "{beyond} of {leaf_count} leaves switch to full detail beyond load_radius_m {load}: proxies will \
                 be drawn where full detail was wanted"
            ),
        });
    }

    let mut built = BuiltProxies {
        index: HlodIndex {
            config,
            proxies,
            node_proxies: vec![None; tree.nodes().len()],
            node_anchors: vec![[0; 3]; tree.nodes().len()],
            node_colors: vec![None; tree.nodes().len()],
            node_atlases: vec![None; tree.nodes().len()],
        },
        manifest: Vec::new(),
        bytes: 0,
        built: 0,
        voxels: None,
    };
    let mut voxel_nodes: Vec<Option<regia_world::hlod_voxels::HlodVoxelNode>> =
        vec![None; tree.nodes().len()];
    // The atlas bake's source surfaces, one per cell, built on first use (ADR-2483).
    let atlas_on = partition.settings.hlod_atlas_enabled();
    let mut sources: BTreeMap<CellCoord, Option<crate::hlod_atlas::BakeSource>> = BTreeMap::new();
    let mut baked_texture_bytes: BTreeMap<CellCoord, u64> = BTreeMap::new();
    // Children sit before their parents in the node list, so each parent merges finished proxies.
    let mut soups: Vec<MeshSoup> = Vec::with_capacity(tree.nodes().len());
    for (index, node) in tree.nodes().iter().enumerate() {
        let (soup, color) = match node.cell {
            Some(cell) => leaves.remove(&cell).unwrap_or_default(),
            None => {
                let mut merged = MeshSoup::default();
                for child in &node.children {
                    if let Some(proxy) = soups.get(*child as usize) {
                        merged.append(proxy, &Affine3A::IDENTITY);
                    }
                }
                let color = merged.mean_color();
                (
                    merged.simplified(hlod.parent_triangle_ratio, hlod.min_triangles),
                    color,
                )
            }
        };
        if !soup.is_empty() {
            let center = [
                (node.bounds_min[0] + node.bounds_max[0]) * 0.5,
                0.0,
                (node.bounds_min[2] + node.bounds_max[2]) * 0.5,
            ];
            let anchor = space
                .from_world(center)
                .map_err(|e| StreamCookError::Scene(format!("hlod node {index}: {e}")))?
                .cell;
            let offset: [f64; 3] = std::array::from_fn(|a| anchor[a] as f64 * space.cell_size());
            let id = hlod_proxy_id(level, index as u32);
            let atlas = if atlas_on {
                let cells = match node.cell {
                    Some(cell) => vec![cell],
                    None => tree.leaves_under(index as u32),
                };
                for cell in &cells {
                    sources.entry(*cell).or_insert_with(|| {
                        partition
                            .cells
                            .get(cell)
                            .and_then(|c| crate::hlod_atlas::BakeSource::new(&c.meshes))
                    });
                }
                let near: Vec<&crate::hlod_atlas::BakeSource> =
                    cells.iter().filter_map(|c| sources.get(c)?.as_ref()).collect();
                let settings = crate::hlod_atlas::AtlasSettings::for_level(
                    partition.settings.hlod_atlas_px,
                    partition.settings.hlod_atlas_max_px,
                    node.level,
                );
                match crate::hlod_atlas::bake_proxy_atlas(&soup, &near, settings) {
                    Ok(baked) => Some(baked),
                    Err(error) => {
                        warnings.push(StreamWarning {
                            code: "stream_hlod_atlas_failed",
                            message: format!(
                                "hlod node {index}: {error}; the proxy draws with its flat color"
                            ),
                        });
                        None
                    }
                }
            } else {
                None
            };
            let proxy_mesh = atlas.as_ref().map_or(&soup, |baked| &baked.mesh);
            let bytes = proxy_mesh
                .relative_to(offset)
                .to_cooked_file(id)
                .map_err(|e| StreamCookError::Scene(format!("hlod node {index}: {e}")))?;
            if let Some(baked) = &atlas {
                if baked.capped {
                    warnings.push(StreamWarning {
                        code: "stream_hlod_atlas_capped",
                        message: format!(
                            "hlod node {index}: the {}-texel atlas cap holds {:.3} texels per meter of the {:.3} asked; \
                             raise hlod_atlas_max_px or lower hlod_atlas_px",
                            partition.settings.hlod_atlas_max_px, baked.texels_per_m, baked.asked_texels_per_m
                        ),
                    });
                }
                let written = crate::hlod_atlas::write_atlas(id, baked, platform, cache_dir, rel_path, index)
                    .map_err(|e| StreamCookError::Scene(format!("hlod node {index}: {e}")))?;
                built.bytes += written.bytes;
                if let Some(cell) = node.cell {
                    baked_texture_bytes.insert(cell, written.texture_bytes);
                }
                built.index.node_atlases[index] = Some(written.record);
                built.manifest.extend(written.manifest);
            }
            if let Some(dir) = cache_dir {
                write_if_changed(&dir.join(cooked_file_name(id)), &bytes)?;
            }
            built.bytes += bytes.len() as u64;
            built.built += 1;
            built.index.node_proxies[index] = Some(id);
            built.index.node_anchors[index] = anchor;
            built.index.node_colors[index] = color;
            built.manifest.push(ManifestEntry {
                path: format!("{rel_path}#hlod/{index}"),
                id,
                type_path: "regia_asset::CookedMesh".to_owned(),
            });
        }
        // The node's voxel proxy of the scatter under it (ADR-3253): a leaf's was cooked above, a
        // parent voxelizes its leaves' instanced triangles at the same resolution over its own,
        // larger box, so its voxels grow as its mesh proxy coarsens.
        let voxel = match node.cell {
            Some(cell) => leaf_voxels.remove(&cell),
            None => {
                let under = tree.leaves_under(index as u32);
                let (soup, instances) = scatter.merged(under.iter());
                if instances == 0 {
                    None
                } else {
                    let center = [
                        (node.bounds_min[0] + node.bounds_max[0]) * 0.5,
                        0.0,
                        (node.bounds_min[2] + node.bounds_max[2]) * 0.5,
                    ];
                    let anchor = space
                        .from_world(center)
                        .map_err(|e| StreamCookError::Scene(format!("hlod node {index}: {e}")))?
                        .cell;
                    let offset: [f64; 3] = std::array::from_fn(|a| anchor[a] as f64 * space.cell_size());
                    match scatter::voxelize(
                        &format!("{rel_path}#hlod/{index}/voxels"),
                        &soup,
                        offset,
                        scatter.coverage.as_ref(),
                        scatter.resolution,
                    ) {
                        Ok(proxy) => Some((proxy, anchor, instances)),
                        Err(error) => {
                            warnings.push(StreamWarning {
                                code: "stream_hlod_voxel_failed",
                                message: format!(
                                    "hlod node {index}: {error}; its scatter has no voxel proxy"
                                ),
                            });
                            None
                        }
                    }
                }
            }
        };
        if let Some((proxy, anchor, instances)) = voxel {
            let id = regia_world::hlod_voxels::hlod_voxel_proxy_id(level, index as u32);
            let bytes = scatter::cooked_voxel_file(id, &proxy)
                .map_err(|e| StreamCookError::Scene(format!("hlod node {index}: {e}")))?;
            if let Some(dir) = cache_dir {
                write_if_changed(&dir.join(cooked_file_name(id)), &bytes)?;
            }
            built.bytes += bytes.len() as u64;
            built.built += 1;
            voxel_nodes[index] = Some(regia_world::hlod_voxels::HlodVoxelNode {
                proxy: id,
                anchor,
                instances,
                voxels: proxy.voxel_count(0) as u32,
                voxel_size_m: proxy.levels.first().map_or(1.0, |l| l.voxel_size),
                bytes: bytes.len() as u64,
            });
            built.manifest.push(ManifestEntry {
                path: format!("{rel_path}#hlod/{index}/voxels"),
                id,
                type_path: regia_world::hlod_voxels::HLOD_VOXEL_PROXY_TYPE_PATH.to_owned(),
            });
        }
        soups.push(soup);
    }
    if voxel_nodes.iter().any(Option::is_some) {
        built.voxels = Some(regia_world::hlod_voxels::HlodVoxels {
            format_version: regia_world::hlod_voxels::HLOD_VOXELS_FORMAT_VERSION,
            level: level.0,
            resolution: scatter.resolution,
            nodes: voxel_nodes,
        });
    }
    if atlas_on {
        // The world index records what each leaf's textures weigh, not the estimate the tree was
        // built with: a leaf that did not bake holds none. The tree's shape does not depend on
        // bytes, so the index still rebuilds the same tree (ADR-2713).
        for proxy in &mut built.index.proxies {
            let guess = estimated.get(&proxy.cell).copied().unwrap_or(0);
            let exact = baked_texture_bytes.get(&proxy.cell).copied().unwrap_or(0);
            proxy.bytes = proxy.bytes.saturating_sub(guess).saturating_add(exact);
        }
    }
    Ok(Some(built))
}

/// A level's cooked nav tiles (spec 78 § 13.2).
struct CookedNav {
    layout: StreamNavLayout,
    /// Per cell: the blob for `CellBlob::nav` and its tile count.
    cells: BTreeMap<CellCoord, (Vec<u8>, u32)>,
}

/// Every triangle of the level's static colliders, in world meters, in guid order.
#[cfg(test)]
fn static_collider_triangles(scene: &SceneFile, registry: &TypeRegistry) -> Vec<[Vec3; 3]> {
    static_collider_surface(scene, registry).0
}

/// [`static_collider_triangles`] and, per triangle, the nav area id of its collider's
/// `NavSurface` (0 without one), for `NavTileCache::build_with_triangle_areas` (ADR-2788).
fn static_collider_surface(scene: &SceneFile, registry: &TypeRegistry) -> (Vec<[Vec3; 3]>, Vec<u8>) {
    use regia_framework::nav_world::NavSurface;
    let mut areas: Vec<u8> = Vec::new();
    let mut codec = ReflectCodec::new(registry);
    let (entities, world_of) = scene_poses(scene, registry, &mut codec);
    let (mut triangles, far) = (Vec::new(), Vec3::splat(f32::MAX));
    for (i, entity) in entities.iter().enumerate() {
        let Some(collider) = entity
            .components
            .get(Collider::type_path())
            .and_then(|raw| decode::<Collider>(&mut codec, registry, raw.get_ron()).ok())
            .filter(|c| !c.sensor)
        else {
            continue;
        };
        // The runtime's nav geometry (`level_triangles_in`, which also rebuilds dirty regions over
        // streamed tiles) takes a collider with a static body on the same entity, and nothing else.
        // The cook takes the same set, so a region rebuild never changes what a tile holds.
        let owner_static = entity
            .components
            .get(RigidBody::type_path())
            .and_then(|raw| decode::<RigidBody>(&mut codec, registry, raw.get_ron()).ok())
            .is_some_and(|body| body.kind == BodyKind::Static);
        if !owner_static {
            continue;
        }
        // Heightfields too, with the collider's diagonal: the same triangles in the same order.
        regia_framework::nav_world::collider_triangles_at(&collider, world_of(i), -far, far, &mut triangles);
        let area = entity
            .components
            .get(NavSurface::type_path())
            .and_then(|raw| decode::<NavSurface>(&mut codec, registry, raw.get_ron()).ok())
            .map_or(0, |surface| surface.area);
        areas.resize(triangles.len(), area);
    }
    (triangles, areas)
}

/// The level's `NavAreaZone`s as nav area volumes, in guid order (the paint order), at their world
/// poses (ADR-2133): the cook paints them into the cooked tiles as the runtime does.
fn nav_area_volumes(scene: &SceneFile, registry: &TypeRegistry) -> Vec<regia_nav::NavAreaVolume> {
    use regia_framework::nav_world::{NavAreaZone, nav_area_zone_volume};
    let mut codec = ReflectCodec::new(registry);
    let (entities, world_of) = scene_poses(scene, registry, &mut codec);
    let mut volumes = Vec::new();
    for (i, entity) in entities.iter().enumerate() {
        if let Some(zone) = entity
            .components
            .get(NavAreaZone::type_path())
            .and_then(|raw| decode::<NavAreaZone>(&mut codec, registry, raw.get_ron()).ok())
        {
            volumes.push(nav_area_zone_volume(&zone, world_of(i)));
        }
    }
    volumes
}

/// The level's entities in guid order and a function from an index to its world pose (parents'
/// transforms applied).
fn scene_poses<'a>(
    scene: &'a SceneFile,
    registry: &TypeRegistry,
    codec: &mut ReflectCodec<'_>,
) -> (Vec<&'a SceneEntity>, impl Fn(usize) -> Affine3A + use<>) {
    let mut entities: Vec<&SceneEntity> = scene.entities.iter().collect();
    entities.sort_by_key(|e| e.guid);
    let index: HashMap<Guid, usize> = entities.iter().enumerate().map(|(i, e)| (e.guid, i)).collect();
    let locals: Vec<Affine3A> = entities
        .iter()
        .map(|e| {
            e.components
                .get(Transform::type_path())
                .and_then(|raw| decode::<Transform>(codec, registry, raw.get_ron()).ok())
                .as_ref()
                .map_or(Affine3A::IDENTITY, matrix)
        })
        .collect();
    let parents: Vec<Option<usize>> = entities
        .iter()
        .map(|e| e.parent.and_then(|p| index.get(&p).copied()))
        .collect();
    let world_of = move |start: usize| {
        let mut pose = locals[start];
        let mut at = start;
        // A cycle is refused by `SceneFile::validate`; the bound only keeps this total.
        for _ in 0..parents.len() {
            let Some(parent) = parents[at] else {
                break;
            };
            pose = locals[parent] * pose;
            at = parent;
        }
        pose
    };
    (entities, world_of)
}

/// Builds the level's nav grid and writes each tile into the cell its center falls in. A cell with
/// tiles and no entities joins the partition, so it streams like any other (spec 78 § 13.2).
fn cook_nav_tiles(
    scene: &SceneFile,
    partition: &mut Partition,
    grid: &StreamGrid,
    registry: &TypeRegistry,
    terrains: &[NavTerrain],
) -> Result<Option<CookedNav>, StreamCookError> {
    let Some(cook) = partition.settings.nav else {
        return Ok(None);
    };
    let _span = tracing::info_span!("stream_cook_nav").entered();
    let (mut triangles, mut triangle_areas) = static_collider_surface(scene, registry);
    // The terrain surfaces after the colliders, as the run-time geometry source orders them
    // differently but a tile is built from the set, not the order of its triangles.
    for terrain in terrains {
        terrain.triangles(&mut triangles);
    }
    triangle_areas.resize(triangles.len(), 0);
    if let Some(&area) = triangle_areas
        .iter()
        .find(|&&a| usize::from(a) >= regia_nav::MAX_NAV_AREAS)
    {
        return Err(StreamCookError::Settings(format!(
            "nav surface: area {area} is past the {} ids a tile can hold",
            regia_nav::MAX_NAV_AREAS - 1
        )));
    }
    // No painted triangle: the plain call, so an unpainted level cooks the bytes it always did.
    if triangle_areas.iter().all(|&a| a == 0) {
        triangle_areas.clear();
    }
    if triangles.is_empty() {
        partition.warnings.push(StreamWarning {
            code: "stream_nav_no_geometry",
            message: "settings ask for nav tiles and the level has no static collider or terrain".to_owned(),
        });
        return Ok(None);
    }
    // The runtime refuses a grid past this: fail the cook, not every cell at run time.
    let (mut low, mut high) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
    for vertex in triangles.iter().flatten() {
        low = low.min(*vertex);
        high = high.max(*vertex);
    }
    let along =
        |extent: f32| ((extent + 2.0 * cook.border_size) / cook.cell_size.max(1.0e-3)).ceil() as f64 + 1.0;
    let cells = along(high.x - low.x) * along(high.z - low.z);
    // A non-finite extent is the builder's error (non-finite geometry), not a size.
    if cells.is_finite() && cells > MAX_STREAMED_GRID_CELLS as f64 {
        return Err(StreamCookError::Settings(format!(
            "nav: the level needs about {cells:.0} grid cells at cell_size {}, past the {MAX_STREAMED_GRID_CELLS} a \
             streamed grid may hold; raise cell_size",
            cook.cell_size
        )));
    }
    let params = NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: cook.cell_size,
            cell_height: cook.cell_height,
            border_size: cook.border_size,
            agent: NavAgentParams {
                radius: cook.agent_radius,
                height: cook.agent_height,
                max_climb: cook.agent_max_climb,
                max_slope: cook.agent_max_slope,
            },
        },
        tile_size: cook.tile_size,
    };
    let mut cache = NavTileCache::build_with_triangle_areas(params, &triangles, &triangle_areas)
        .map_err(|e| StreamCookError::Settings(format!("nav: {e}")))?;
    // The level's area zones paint the cooked cells (ADR-2133); the runtime paints the same ids.
    let volumes = nav_area_volumes(scene, registry);
    if !volumes.is_empty() {
        for volume in volumes {
            cache
                .add_area_volume(volume)
                .map_err(|e| StreamCookError::Settings(format!("nav area zone: {e}")))?;
        }
        cache.update();
    }
    let layout = cache.layout();
    let (tiles_x, tiles_z) = cache.tile_grid();
    let tile_m = f64::from(cook.tile_size) * f64::from(cook.cell_size);
    let mut per_cell: BTreeMap<CellCoord, Vec<(u32, u32)>> = BTreeMap::new();
    for tz in 0..tiles_z {
        for tx in 0..tiles_x {
            let x = f64::from(layout.origin.x) + (f64::from(tx) + 0.5) * tile_m;
            let z = f64::from(layout.origin.z) + (f64::from(tz) + 0.5) * tile_m;
            let cell = grid
                .cell_of(x, z)
                .map_err(|e| StreamCookError::Settings(format!("nav: {e}")))?;
            per_cell.entry(cell).or_default().push((tx, tz));
        }
    }
    let mut cells = BTreeMap::new();
    for (cell, tiles) in per_cell {
        let bytes = cache
            .cook_cell(&tiles)
            .map_err(|e| StreamCookError::Settings(format!("nav: {e}")))?;
        partition.cells.entry(cell).or_default();
        let [low, high] = &mut partition.bounds;
        *low = CellCoord::new(low.x.min(cell.x), low.z.min(cell.z));
        *high = CellCoord::new(high.x.max(cell.x), high.z.max(cell.z));
        cells.insert(cell, (bytes, tiles.len() as u32));
    }
    let origin = layout.origin;
    Ok(Some(CookedNav {
        layout: StreamNavLayout {
            cook,
            origin: [origin.x, origin.y, origin.z],
            width: layout.width as u32,
            height: layout.height as u32,
        },
        cells,
    }))
}

/// Whether the always-loaded set holds a heightfield collider: the level has terrain, and its cells
/// wait for that collider.
fn has_heightfield(always: &SceneFile, registry: &TypeRegistry) -> bool {
    let mut codec = ReflectCodec::new(registry);
    always.entities.iter().any(|entity| {
        entity
            .components
            .get(Collider::type_path())
            .and_then(|raw| decode::<Collider>(&mut codec, registry, raw.get_ron()).ok())
            .is_some_and(|c| matches!(c.shape, ColliderShape::HeightField { .. }))
    })
}

/// The warnings about how the level's other systems line up with its cells (spec 78 §§ 13.1, 14):
/// `stream_terrain_misaligned` when the always-loaded set has a tiled terrain collider
/// (`TerrainTileCollision`) whose tile size neither divides nor is divided by the cell size, and
/// `stream_relevance_radius` when the net relevance reach the assets report is beyond
/// `load_radius_m`.
pub fn link_warnings(
    always: &SceneFile,
    settings: &LevelStreaming,
    registry: &TypeRegistry,
    assets: &dyn StreamAssets,
) -> Vec<StreamWarning> {
    use regia_scene::streaming::TerrainTileCollision;
    let mut warnings = Vec::new();
    let mut codec = ReflectCodec::new(registry);
    let cell = settings.cell_size_m;
    for entity in &always.entities {
        let Some(tiles) = entity
            .components
            .get(TerrainTileCollision::type_path())
            .and_then(|raw| decode::<TerrainTileCollision>(&mut codec, registry, raw.get_ron()).ok())
        else {
            continue;
        };
        let tile = f64::from(tiles.tile_m);
        if !TerrainTileCollision::aligned_with(tile, cell) {
            warnings.push(StreamWarning {
                code: "stream_terrain_misaligned",
                message: format!(
                    "entity {}: terrain collision tiles of {tile} m and cells of {cell} m do not divide each \
                     other; it works, but each cell holds tiles that reach into its neighbors",
                    entity.guid
                ),
            });
        }
    }
    if let Some(reach) = assets.relevance_radius_m()
        && reach.is_finite()
        && reach > settings.load_radius_m
    {
        warnings.push(StreamWarning {
            code: "stream_relevance_radius",
            message: format!(
                "net relevance reaches {reach} m but load_radius_m is {}: clients get binds for entities whose \
                 cells they have not loaded (they wait, buffered, until the cell spawns)",
                settings.load_radius_m
            ),
        });
    }
    warnings
}

/// The GPU bytes of the built-in primitives each cell draws (ADR-4993). A `Primitive` names no
/// asset, so the dependency costs miss it, but the renderer uploads one mesh per distinct shape
/// (`PrimitiveCache`) and shares it: each distinct shape's vertex and index bytes count once,
/// split between the cells that draw it, like an asset's cost.
fn primitive_vram_by_cell(
    cells: &BTreeMap<CellCoord, PartitionedCell>,
    registry: &TypeRegistry,
) -> BTreeMap<CellCoord, u64> {
    let mut codec = ReflectCodec::new(registry);
    // Component text to shape key (a level repeats a few texts), and shape key to bytes.
    let mut keys: HashMap<String, Option<String>> = HashMap::new();
    let mut bytes: HashMap<String, u64> = HashMap::new();
    let mut shapes_of: BTreeMap<CellCoord, BTreeSet<String>> = BTreeMap::new();
    let mut cells_drawing: HashMap<String, u64> = HashMap::new();
    for (coord, cell) in cells {
        let mut shapes = BTreeSet::new();
        for entity in &cell.entities {
            let Some(raw) = entity.components.get(Primitive::type_path()) else {
                continue;
            };
            let key = keys.entry(raw.get_ron().to_owned()).or_insert_with(|| {
                let primitive = decode::<Primitive>(&mut codec, registry, raw.get_ron()).ok()?;
                let key = format!("{:?}", primitive.shape);
                if !bytes.contains_key(&key) {
                    let mesh = primitive.shape.mesh().ok()?;
                    let size = mesh.vertices.len() * std::mem::size_of::<regia_render::LabVertex>()
                        + mesh.indices.len() * std::mem::size_of::<u32>();
                    bytes.insert(key.clone(), size as u64);
                }
                Some(key)
            });
            if let Some(key) = key {
                shapes.insert(key.clone());
            }
        }
        for key in &shapes {
            *cells_drawing.entry(key.clone()).or_insert(0) += 1;
        }
        shapes_of.insert(*coord, shapes);
    }
    shapes_of
        .into_iter()
        .map(|(coord, shapes)| {
            let total = shapes
                .iter()
                .map(|key| {
                    let share = cells_drawing.get(key).copied().unwrap_or(1).max(1);
                    bytes.get(key).copied().unwrap_or(0) / share
                })
                .sum();
            (coord, total)
        })
        .collect()
}

/// Cooks a partition. With `cache_dir` it writes the blobs, the always-loaded set and last the world
/// index; without it nothing touches the disk, which is `regia assets stream-report`.
pub fn cook_partition(
    level: LevelId,
    scene: &SceneFile,
    partition: Partition,
    registry: &TypeRegistry,
    assets: &dyn StreamAssets,
    cache_dir: Option<&Path>,
    rel_path: &str,
) -> Result<StreamCookOutput, StreamCookError> {
    let _span = tracing::info_span!("stream_cook", cells = partition.cells.len()).entered();
    let _stale_index =
        cache_dir.map(|dir| StaleIndexGuard::arm(dir.join(cooked_file_name(world_index_id(level)))));
    let grid = partition
        .settings
        .grid()
        .map_err(|e| StreamCookError::Settings(e.to_string()))?;
    let mut output = StreamCookOutput::default();
    let mut report = StreamReport {
        cells: partition.cells.len(),
        always_entities: partition.always.entities.len(),
        warnings: partition.warnings.clone(),
        ..StreamReport::default()
    };

    // Amortization: an asset's cost is shared by the cells referencing it.
    let mut references: BTreeMap<AssetId, u64> = BTreeMap::new();
    for cell in partition.cells.values() {
        for id in &cell.deps {
            *references.entry(*id).or_insert(0) += 1;
        }
    }
    let primitive_vram = primitive_vram_by_cell(&partition.cells, registry);

    let mut partition = partition;
    let nav = cook_nav_tiles(scene, &mut partition, &grid, registry, assets.nav_terrains())?;
    report.cells = partition.cells.len();
    report.warnings.clone_from(&partition.warnings);
    let has_terrain = has_heightfield(&partition.always, registry);
    report.warnings.extend(link_warnings(
        &partition.always,
        &partition.settings,
        registry,
        assets,
    ));
    let mut records = Vec::with_capacity(partition.cells.len());
    let mut replicated: Vec<(Guid, CellCoord)> = Vec::new();
    let mut counts = Vec::with_capacity(partition.cells.len());
    for (coord, cell) in &partition.cells {
        let file = cell_scene(scene, cell.entities.clone());
        let (cooked, _unregistered) =
            CookedScene::cook(&file, registry).map_err(|e| StreamCookError::Scene(e.to_string()))?;
        debug_assert_eq!(cooked.format_version, COOKED_SCENE_VERSION);
        let id = cell_blob_id(level, *coord);
        let deps: Vec<AssetId> = cell.deps.iter().copied().collect();
        let blob = CellBlob {
            format_version: CELL_BLOB_FORMAT_VERSION,
            coord: *coord,
            scene: cooked,
            anchor: cell.anchor,
            nav: nav
                .as_ref()
                .and_then(|n| n.cells.get(coord))
                .map(|(bytes, _)| bytes.clone()),
        };
        let bytes = blob.to_bytes(id, deps.clone())?;
        if let Some(dir) = cache_dir {
            if write_if_changed(&dir.join(cooked_file_name(id)), &bytes)? {
                report.written += 1;
            } else {
                report.unchanged += 1;
            }
        }
        let (mut dep_bytes, mut vram_bytes) = (0u64, primitive_vram.get(coord).copied().unwrap_or(0));
        for dep in &deps {
            let share = references.get(dep).copied().unwrap_or(1).max(1);
            if let Some(cost) = assets.cost(*dep) {
                dep_bytes = dep_bytes.saturating_add(cost.ram_bytes / share);
                vram_bytes = vram_bytes.saturating_add(cost.vram_bytes / share);
            }
        }
        replicated.extend(cell.replicated.iter().map(|g| (*g, *coord)));
        let entity_count = cell.entities.len() as u32;
        counts.push(entity_count);
        report.cell_bytes += bytes.len() as u64;
        report.cell_bytes_max = report.cell_bytes_max.max(bytes.len() as u64);
        records.push(CellRecord {
            coord: *coord,
            blob: id,
            blob_hash: *blake3::hash(&bytes).as_bytes(),
            entity_count,
            own_bytes: bytes.len() as u64,
            dep_bytes,
            vram_bytes,
            min_y: cell.min_y,
            max_y: cell.max_y,
            priority: cell.priority,
            deps,
            nav_tiles: nav
                .as_ref()
                .and_then(|n| n.cells.get(coord))
                .map_or(0, |(_, tiles)| *tiles),
            has_terrain_collision: has_terrain,
        });
        output.manifest.push(ManifestEntry {
            path: format!("{rel_path}#cells/{}", coord.label()),
            id,
            type_path: CELL_BLOB_TYPE_PATH.to_owned(),
        });
    }
    counts.sort_unstable();
    report.entities_min = counts.first().copied().unwrap_or(0);
    report.entities_max = counts.last().copied().unwrap_or(0);
    report.entities_median = median(&counts);
    replicated.sort_unstable();

    let (always, _unregistered) =
        CookedScene::cook(&partition.always, registry).map_err(|e| StreamCookError::Scene(e.to_string()))?;
    let always_bytes = always_loaded_to_bytes(level, &always)?;
    report.always_bytes = always_bytes.len() as u64;
    let always_id = always_loaded_id(level);
    output.manifest.push(ManifestEntry {
        path: format!("{rel_path}#always"),
        id: always_id,
        type_path: ALWAYS_LOADED_TYPE_PATH.to_owned(),
    });

    let mut warnings = report.warnings.clone();
    // The terrain scatter, placed and gathered by cell for the voxel proxies (ADR-3253). Nothing is
    // placed without HLOD or without scatter, so such a level cooks as it did.
    let scatter_cells = if partition.settings.hlod.is_some() && !assets.scatter().is_empty() {
        scatter::gather(
            assets.scatter(),
            assets.scatter_voxel_resolution(),
            &grid,
            &|path| assets.mesh(path),
        )
    } else {
        scatter::ScatterCells::default()
    };
    warnings.extend(scatter_cells.warnings.iter().cloned());
    report.scatter_instances = scatter_cells.cells.values().map(|c| u64::from(c.instances)).sum();
    let built = hlod_index(
        level,
        &partition,
        &grid,
        HlodOutput {
            cache_dir,
            rel_path,
            platform: assets.platform(),
            scatter: &scatter_cells,
        },
        &mut warnings,
    )?;
    report.warnings = warnings;
    let mut voxel_table = None;
    let hlod = built.map(|mut proxies| {
        report.proxies = proxies.built;
        report.proxy_bytes = proxies.bytes;
        output.manifest.append(&mut proxies.manifest);
        voxel_table = proxies.voxels.take();
        proxies.index
    });
    if let Some(table) = &voxel_table {
        report.voxel_proxies = table.count();
        report.voxel_bytes = table.nodes.iter().flatten().map(|n| n.bytes).sum();
        let id = regia_world::hlod_voxels::hlod_voxels_id(level);
        let bytes = table.to_bytes()?;
        report.proxy_bytes += bytes.len() as u64;
        if let Some(dir) = cache_dir {
            write_if_changed(&dir.join(cooked_file_name(id)), &bytes)?;
        }
        output.manifest.push(ManifestEntry {
            path: format!("{rel_path}#hlod_voxels"),
            id,
            type_path: regia_world::hlod_voxels::HLOD_VOXELS_TYPE_PATH.to_owned(),
        });
    } else if let Some(dir) = cache_dir {
        // No node has voxels now: a table an earlier cook wrote must go, or the runtime, which
        // probes the table by the level's id in the cache, draws the old forest over this tree.
        // The removal is a change like a write: the old index goes first (ADR-8860).
        remove_if_present(&dir.join(cooked_file_name(regia_world::hlod_voxels::hlod_voxels_id(level))))?;
    }
    let index = WorldIndex {
        format_version: WORLD_INDEX_FORMAT_VERSION,
        level: level.0,
        content_hash: WorldIndex::hash_cells(&records),
        settings: partition.settings,
        cells: records,
        bounds: partition.bounds,
        hlod,
        replicated,
        nav: nav.map(|n| n.layout),
    };
    index.validate()?;
    let index_id = world_index_id(level);
    output.manifest.push(ManifestEntry {
        path: format!("{rel_path}#wstream"),
        id: index_id,
        type_path: WORLD_INDEX_TYPE_PATH.to_owned(),
    });
    if let Some(dir) = cache_dir {
        write_if_changed(&dir.join(cooked_file_name(always_id)), &always_bytes)?;
        // Last: a cancelled cook leaves no index pointing at missing blobs, and the first changed
        // blob above already removed the old index (ADR-8812), so none names new blob bytes.
        write_if_changed(&dir.join(cooked_file_name(index_id)), &index.to_bytes()?)?;
    }
    output.report = report;
    Ok(output)
}

/// Partitions and cooks one level. `Ok(None)`: the level is not streamed, nothing was written.
pub fn cook_streamed_level(
    level: LevelId,
    scene: &SceneFile,
    registry: &TypeRegistry,
    assets: &dyn StreamAssets,
    cache_dir: Option<&Path>,
    rel_path: &str,
) -> Result<Option<StreamCookOutput>, StreamCookError> {
    let Some(settings) = level_streaming(scene, registry)? else {
        return Ok(None);
    };
    let partition = partition(scene, settings, registry, assets)?;
    // The cells and every per-level pass (nav tiles, collider surfaces) see the same scene: review
    // markers stripped, so content parented under a marker is in neither (review
    // sprint-w12-rev-g15 #11).
    let stripped = crate::cook::strip_review_notes(scene);
    cook_partition(level, &stripped, partition, registry, assets, cache_dir, rel_path).map(Some)
}

/// The level sources under `assets_root` that mention streaming settings: `*.level.ron` files and
/// `*.level.d` directories, sorted. A staging directory of `regia upgrade-project --write`
/// (`.<name>.<pid>.stage`) is not searched: its copy of a level is not a second level (ADR-8861).
pub fn discover_streamed_levels(assets_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(assets_root)
        .into_iter()
        .filter_entry(|e| !is_upgrade_stage_dir(e))
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let file = if entry.file_type().is_file() && name.ends_with(".level.ron") {
            path.to_path_buf()
        } else if entry.file_type().is_dir() && name.ends_with(".level.d") {
            path.join("level.ron")
        } else {
            continue;
        };
        if std::fs::read_to_string(&file).is_ok_and(|text| text.contains("LevelStreaming")) {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    out
}

/// Whether `entry` is a staging directory `regia upgrade-project --write` left (ADR-8861).
pub(crate) fn is_upgrade_stage_dir(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .is_some_and(regia_scene::level_dir::is_upgrade_stage_dir_name)
}

/// The level's id: its `.meta` sidecar's, written on first cook, so a recook gives the same ids.
fn level_id(source: &Path) -> Result<LevelId, String> {
    crate::db::AssetDatabase::load_or_create_meta(source, "streamed_level")
        .map(|meta| LevelId(meta.id.0))
        .map_err(|e| e.to_string())
}

/// The `regia cook` pass: cooks every streamed level under `assets_root` and replaces its manifest
/// rows. A level that fails is an error of the report; other levels still cook.
pub(crate) fn cook_project_levels(
    assets_root: &Path,
    cache_dir: &Path,
    platform: &str,
    manifest: &mut BTreeMap<String, ManifestEntry>,
    report: &mut crate::cook::CookReport,
) {
    let levels = discover_streamed_levels(assets_root);
    if levels.is_empty() {
        return;
    }
    let mut registry = TypeRegistry::default();
    register_cook_types(&mut registry);
    // The rows this cook just wrote, not the manifest the previous cook left on disk: on a fresh
    // cache that one is empty, and a mesh added since the last cook would be missing from the
    // cells' dependencies, costs and HLOD proxies (deep-zai-terrain-stream #1).
    let mut assets = CacheAssets::with_manifest(cache_dir, platform, manifest.values().cloned().collect());
    if let Some(project_root) = assets_root.parent() {
        assets = assets.with_project_root(project_root);
    }
    for source in levels {
        let rel = source
            .strip_prefix(assets_root)
            .unwrap_or(&source)
            .to_string_lossy()
            .replace('\\', "/");
        let rel = format!("assets/{rel}");
        let cooked = regia_scene::level_dir::read_level_source(&source)
            .map_err(|e| e.to_string())
            .and_then(|scene| {
                let level = level_id(&source)?;
                cook_streamed_level(level, &scene, &registry, &assets, Some(cache_dir), &rel)
                    .map_err(|e| e.to_string())
            });
        match cooked {
            Ok(Some(output)) => {
                let prefix = format!("{rel}#");
                manifest.retain(|path, _| !path.starts_with(&prefix));
                let written = output.report.written;
                for entry in output.manifest {
                    manifest.insert(entry.path.clone(), entry);
                }
                report.cooked += written;
                report.skipped += output.report.unchanged;
                report.warnings.extend(
                    output
                        .report
                        .warnings
                        .into_iter()
                        .map(|w| format!("{rel}: {}: {}", w.code, w.message)),
                );
            }
            Ok(None) => {}
            Err(error) => report.errors.push((rel, error)),
        }
    }
}

#[cfg(test)]
mod atlas_tests;
pub mod scatter;
#[cfg(test)]
mod scatter_tests;
#[cfg(test)]
mod tests;
