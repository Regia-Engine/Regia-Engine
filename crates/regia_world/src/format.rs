// SPDX-License-Identifier: MPL-2.0
//! The cooked data of a streamed level (spec 78 § 6, `docs/formats/world-stream.md`): one world index
//! (`.wstream`) and one blob per non-empty cell (`.wcell`), both behind the standard cooked header.
//!
//! The world index is read on level open, a cell blob on a worker thread. Neither reader trusts its
//! bytes: every failure is a typed error, never a panic.

use regia_asset::{
    AssetId, AssetLoadError, COOKED_FORMAT_VERSION, CookedHeader, CookedIoError, decode_cooked_file,
    encode_cooked_file, pack_importer_version,
};
use regia_core::id::{Guid, TypePathHash};
use regia_scene::cooked::CookedScene;
use regia_scene::hlod::{CellProxy, HlodConfig, HlodError, HlodTree};
use regia_scene::level::LevelId;
use regia_scene::stream_grid::{CellCoord, StreamGrid};
use regia_scene::streaming::{
    HlodSettings, LevelStreaming, OriginSettings, StreamBudgets, StreamNavCook, StreamNavLayout, StreamTuning,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The current `.wstream` format version: 2 adds the baked HLOD proxy atlases
/// ([`HlodIndex::node_atlases`]) and the atlas settings (ADR-2483). Version 1 still reads
/// ([`WorldIndex::from_bytes`] migrates it).
pub const WORLD_INDEX_FORMAT_VERSION: u32 = 2;
/// The first `.wstream` version, without proxy atlases.
pub const WORLD_INDEX_FORMAT_VERSION_V1: u32 = 1;
/// The current `.wcell` format version.
pub const CELL_BLOB_FORMAT_VERSION: u32 = 1;

/// The cooked header's type path of a world index.
const WORLD_INDEX_TYPE: &str = "regia_world::format::WorldIndex";
/// The cooked header's type path of a cell blob.
const CELL_BLOB_TYPE: &str = "regia_world::format::CellBlob";
/// The cooked header's type path of an always-loaded set.
const ALWAYS_LOADED_TYPE: &str = "regia_world::format::AlwaysLoaded";

/// Everything the runtime needs to know about a streamed level without reading a cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldIndex {
    /// [`WORLD_INDEX_FORMAT_VERSION`].
    pub format_version: u32,
    /// The level.
    pub level: Guid,
    /// BLAKE3 over every cell blob hash in coordinate order.
    pub content_hash: [u8; 32],
    /// The settings as cooked (validated).
    pub settings: LevelStreaming,
    /// One record per non-empty cell, sorted by `(x, z)`.
    pub cells: Vec<CellRecord>,
    /// Lowest and highest cell of the world, inclusive per axis.
    pub bounds: [CellCoord; 2],
    /// The HLOD tree, when the level has proxies.
    pub hlod: Option<HlodIndex>,
    /// Entities with replication, sorted by guid: their cell, for binds that arrive before the cell
    /// spawned (spec 78 § 14).
    pub replicated: Vec<(Guid, CellCoord)>,
    /// The grid the cells' nav tiles were cooked on; `None` when they carry none (spec 78 § 13.2).
    pub nav: Option<StreamNavLayout>,
}

/// One non-empty cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellRecord {
    /// The cell.
    pub coord: CellCoord,
    /// Asset id of its blob.
    pub blob: AssetId,
    /// BLAKE3 of the blob file.
    pub blob_hash: [u8; 32],
    /// Entities in the blob.
    pub entity_count: u32,
    /// Decoded blob size in RAM.
    pub own_bytes: u64,
    /// Sum over referenced assets of the asset's bytes divided by the cells referencing it.
    pub dep_bytes: u64,
    /// The same amortized sum, GPU side.
    pub vram_bytes: u64,
    /// Lowest point of the cell's content, meters.
    pub min_y: f64,
    /// Highest point of the cell's content, meters.
    pub max_y: f64,
    /// The highest priority over the priority volumes overlapping the cell, else 0.
    pub priority: i32,
    /// Referenced assets, sorted.
    pub deps: Vec<AssetId>,
    /// Nav tiles in the blob (hook hint).
    pub nav_tiles: u32,
    /// The terrain covers this cell (hook hint).
    pub has_terrain_collision: bool,
}

/// The HLOD tree of a streamed level. The tree itself is rebuilt from these inputs on load:
/// [`HlodTree::build`] is deterministic, so the cooked node order and this one agree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HlodIndex {
    /// How the tree was built.
    pub config: HlodConfig,
    /// The leaves' inputs, in cell order.
    pub proxies: Vec<CellProxy>,
    /// Proxy mesh asset per tree node, in tree order; `None` for a node with nothing to draw.
    pub node_proxies: Vec<Option<AssetId>>,
    /// Each proxy's position in the origin space: its vertices are relative to this cell's center.
    pub node_anchors: Vec<[i64; 3]>,
    /// Each proxy's flat linear RGBA base color, in tree order: the area-weighted mean of the
    /// `Primitive` colors it merges (spec 78 § 11.1, ADR-2022); `None` when no source named a color,
    /// and the proxy draws with the default material.
    pub node_colors: Vec<Option<[f32; 4]>>,
    /// Each proxy's baked atlas, in tree order (format version 2, ADR-2483); `None` for a proxy drawn
    /// flat (the level's `hlod_atlas_px` is 0, the node has no proxy, or the file is version 1).
    /// May be shorter than the node list: a missing entry is `None`.
    pub node_atlases: Vec<Option<HlodAtlas>>,
}

/// A proxy's baked atlas (ADR-2483): an ordinary cooked material over three cooked textures (base
/// color, tangent-space normal, packed occlusion-roughness-metallic) in the proxy mesh's UV0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HlodAtlas {
    /// The cooked standard material the proxy draws with.
    pub material: AssetId,
    /// The base color, normal and ORM textures, in that order.
    pub textures: [AssetId; 3],
    /// Atlas width and height, texels.
    pub size: [u32; 2],
    /// Texels per meter of proxy surface the atlas reached (after any cap).
    pub texels_per_m: f32,
    /// Texels per meter the level asked for at this node's tree level.
    pub asked_texels_per_m: f32,
    /// The atlas cap (`hlod_atlas_max_px`) truncated the density.
    pub capped: bool,
    /// Root mean square distance between the proxy surface and the source surface it sampled,
    /// meters, over the baked texels.
    pub bake_error_m: f32,
    /// Share of the texels inside proxy triangles that found no source surface (filled by
    /// dilation), `0..=1`.
    pub miss_ratio: f32,
}

impl HlodIndex {
    /// Node `index`'s atlas, if it has one.
    pub fn atlas(&self, index: usize) -> Option<&HlodAtlas> {
        self.node_atlases.get(index).and_then(Option::as_ref)
    }
}

impl HlodIndex {
    /// Rebuilds the tree over `grid`.
    pub fn tree(&self, grid: &StreamGrid) -> Result<HlodTree, HlodError> {
        HlodTree::build(grid, self.config, &self.proxies)
    }
}

/// The cooked entities of one cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellBlob {
    /// [`CELL_BLOB_FORMAT_VERSION`].
    pub format_version: u32,
    /// The cell.
    pub coord: CellCoord,
    /// The entities; hierarchies sorted by root guid, each one contiguous and root first. Root
    /// transforms are relative to the center of `anchor`.
    pub scene: CookedScene,
    /// The cell's position in the origin space (`LargePosition::cell`).
    pub anchor: [i64; 3],
    /// Cooked nav tile data for this cell, opaque to the runtime.
    pub nav: Option<Vec<u8>>,
}

/// The asset id of a level's world index.
pub fn world_index_id(level: LevelId) -> AssetId {
    AssetId::sub_asset(AssetId(level.0), "wstream")
}

/// The asset id of a level's always-loaded set.
pub fn always_loaded_id(level: LevelId) -> AssetId {
    AssetId::sub_asset(AssetId(level.0), "always")
}

/// The asset id of a cell blob.
pub fn cell_blob_id(level: LevelId, cell: CellCoord) -> AssetId {
    AssetId::sub_asset(AssetId(level.0), &format!("cells/{}", cell.label()))
}

/// The asset id of an HLOD proxy mesh.
pub fn hlod_proxy_id(level: LevelId, node: u32) -> AssetId {
    AssetId::sub_asset(AssetId(level.0), &format!("hlod/{node}"))
}

/// Why cooked world data could not be read. [`Self::cell_reason`] is the stable code.
#[derive(Debug, Error)]
pub enum WorldFormatError {
    /// The cooked header is wrong, the file is truncated, or the hash does not match the record.
    #[error("corrupt: {0}")]
    Corrupt(String),
    /// The file has a format version this build does not read.
    #[error("format version {found} is newer than {supported}")]
    Newer {
        /// Version in the file.
        found: u32,
        /// Highest version this build reads.
        supported: u32,
    },
    /// The header and hash were fine and the payload still did not decode.
    #[error("decode failed: {0}")]
    Decode(String),
}

impl WorldFormatError {
    /// The cell failure reason of spec 78 § 17.
    pub fn cell_reason(&self) -> &'static str {
        match self {
            Self::Corrupt(_) => "cell_corrupt",
            Self::Newer { .. } => "cell_format_newer",
            Self::Decode(_) => "cell_decode",
        }
    }
}

impl From<CookedIoError> for WorldFormatError {
    fn from(e: CookedIoError) -> Self {
        Self::Corrupt(e.to_string())
    }
}

impl From<AssetLoadError> for WorldFormatError {
    fn from(e: AssetLoadError) -> Self {
        Self::Corrupt(e.to_string())
    }
}

fn encode<T: Serialize>(
    value: &T,
    id: AssetId,
    type_path: &str,
    dependencies: Vec<AssetId>,
) -> Result<Vec<u8>, WorldFormatError> {
    let payload = postcard::to_allocvec(value).map_err(|e| WorldFormatError::Decode(e.to_string()))?;
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of(type_path),
        asset_id: id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies,
        payload_len: payload.len() as u64,
    };
    Ok(encode_cooked_file(&header, &payload)?)
}

/// The header-checked payload of a cooked file of type `type_path`.
fn payload<'a>(bytes: &'a [u8], id: AssetId, type_path: &str) -> Result<&'a [u8], WorldFormatError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != TypePathHash::of(type_path) {
        return Err(WorldFormatError::Corrupt(format!("not a {type_path}")));
    }
    if header.asset_id != id {
        return Err(WorldFormatError::Corrupt(format!(
            "file holds asset {:?}, expected {id:?}",
            header.asset_id
        )));
    }
    Ok(payload)
}

/// The leading `format_version` of a payload, refused when newer than `supported`.
fn check_version(payload: &[u8], supported: u32) -> Result<(), WorldFormatError> {
    let (found, _) = postcard::take_from_bytes::<u32>(payload)
        .map_err(|e| WorldFormatError::Decode(format!("format version: {e}")))?;
    if found > supported {
        return Err(WorldFormatError::Newer { found, supported });
    }
    Ok(())
}

/// The frozen `.wstream` version 1 layout (before ADR-2483), read by the migrator only.
mod v1 {
    use super::*;

    /// `LevelStreaming` before `hlod_atlas_px` and `hlod_atlas_max_px`.
    #[derive(Deserialize, Serialize)]
    pub(super) struct LevelStreamingV1 {
        pub version: u32,
        pub enabled: bool,
        pub cell_size_m: f64,
        pub load_radius_m: f64,
        pub unload_radius_m: f64,
        pub max_entity_extent_cells: f64,
        pub hlod: Option<HlodSettings>,
        pub nav: Option<StreamNavCook>,
        pub origin: OriginSettings,
        pub budgets: Option<StreamBudgets>,
        pub tuning: StreamTuning,
    }

    /// `HlodIndex` before `node_atlases`.
    #[derive(Deserialize, Serialize)]
    pub(super) struct HlodIndexV1 {
        pub config: HlodConfig,
        pub proxies: Vec<CellProxy>,
        pub node_proxies: Vec<Option<AssetId>>,
        pub node_anchors: Vec<[i64; 3]>,
        pub node_colors: Vec<Option<[f32; 4]>>,
    }

    /// `WorldIndex` version 1.
    #[derive(Deserialize, Serialize)]
    pub(super) struct WorldIndexV1 {
        pub format_version: u32,
        pub level: Guid,
        pub content_hash: [u8; 32],
        pub settings: LevelStreamingV1,
        pub cells: Vec<CellRecord>,
        pub bounds: [CellCoord; 2],
        pub hlod: Option<HlodIndexV1>,
        pub replicated: Vec<(Guid, CellCoord)>,
        pub nav: Option<StreamNavLayout>,
    }

    impl From<WorldIndexV1> for WorldIndex {
        fn from(old: WorldIndexV1) -> Self {
            let s = old.settings;
            Self {
                format_version: WORLD_INDEX_FORMAT_VERSION,
                level: old.level,
                content_hash: old.content_hash,
                settings: LevelStreaming {
                    version: s.version,
                    enabled: s.enabled,
                    cell_size_m: s.cell_size_m,
                    load_radius_m: s.load_radius_m,
                    unload_radius_m: s.unload_radius_m,
                    max_entity_extent_cells: s.max_entity_extent_cells,
                    hlod: s.hlod,
                    nav: s.nav,
                    origin: s.origin,
                    budgets: s.budgets,
                    tuning: s.tuning,
                    hlod_atlas_px: 0,
                    hlod_atlas_max_px: regia_scene::streaming::HLOD_ATLAS_MAX_PX_DEFAULT,
                },
                cells: old.cells,
                bounds: old.bounds,
                hlod: old.hlod.map(|h| HlodIndex {
                    config: h.config,
                    proxies: h.proxies,
                    node_atlases: vec![None; h.node_proxies.len()],
                    node_proxies: h.node_proxies,
                    node_anchors: h.node_anchors,
                    node_colors: h.node_colors,
                }),
                replicated: old.replicated,
                nav: old.nav,
            }
        }
    }

    impl WorldIndexV1 {
        /// The version 1 form of `index`, for tests and fixtures: the atlas fields are dropped.
        pub(super) fn of(index: &WorldIndex) -> Self {
            let s = &index.settings;
            Self {
                format_version: WORLD_INDEX_FORMAT_VERSION_V1,
                level: index.level,
                content_hash: index.content_hash,
                settings: LevelStreamingV1 {
                    version: s.version,
                    enabled: s.enabled,
                    cell_size_m: s.cell_size_m,
                    load_radius_m: s.load_radius_m,
                    unload_radius_m: s.unload_radius_m,
                    max_entity_extent_cells: s.max_entity_extent_cells,
                    hlod: s.hlod,
                    nav: s.nav,
                    origin: s.origin,
                    budgets: s.budgets,
                    tuning: s.tuning,
                },
                cells: index.cells.clone(),
                bounds: index.bounds,
                hlod: index.hlod.as_ref().map(|h| HlodIndexV1 {
                    config: h.config,
                    proxies: h.proxies.clone(),
                    node_proxies: h.node_proxies.clone(),
                    node_anchors: h.node_anchors.clone(),
                    node_colors: h.node_colors.clone(),
                }),
                replicated: index.replicated.clone(),
                nav: index.nav,
            }
        }
    }
}

/// The `format_version` of a cooked world index file, or `Ok(None)` when the file is a cooked file of
/// another type or its cooked header does not read (for `regia upgrade-project`, which probes every
/// `.fckd`).
///
/// # Errors
///
/// A world index payload has no readable version.
pub fn world_index_file_version(bytes: &[u8]) -> Result<Option<u32>, WorldFormatError> {
    // A file whose cooked header does not read is no type's this probe can name.
    let Ok((header, body)) = decode_cooked_file(bytes) else {
        return Ok(None);
    };
    if header.type_tag != TypePathHash::of(WORLD_INDEX_TYPE) {
        return Ok(None);
    }
    let (found, _) = postcard::take_from_bytes::<u32>(body)
        .map_err(|e| WorldFormatError::Decode(format!("format version: {e}")))?;
    Ok(Some(found))
}

/// Rewrites a cooked world index file of an older version at [`WORLD_INDEX_FORMAT_VERSION`], the
/// cooked header's asset id kept (`regia upgrade-project`). `Ok(None)` when the file is current or
/// not a world index.
///
/// # Errors
///
/// The file is corrupt, newer than this build, or does not decode.
pub fn migrate_world_index_file(bytes: &[u8]) -> Result<Option<Vec<u8>>, WorldFormatError> {
    let Some(version) = world_index_file_version(bytes)? else {
        return Ok(None);
    };
    if version >= WORLD_INDEX_FORMAT_VERSION {
        check_version_of(version, WORLD_INDEX_FORMAT_VERSION)?;
        return Ok(None);
    }
    let (header, _) = decode_cooked_file(bytes)?;
    let index = WorldIndex::decode_any(bytes)?;
    encode(&index, header.asset_id, WORLD_INDEX_TYPE, header.dependencies).map(Some)
}

/// `Newer` when `found` is above `supported`.
fn check_version_of(found: u32, supported: u32) -> Result<(), WorldFormatError> {
    if found > supported {
        return Err(WorldFormatError::Newer { found, supported });
    }
    Ok(())
}

impl WorldIndex {
    /// The cooked file of this index.
    pub fn to_bytes(&self) -> Result<Vec<u8>, WorldFormatError> {
        let id = world_index_id(LevelId(self.level));
        encode(self, id, WORLD_INDEX_TYPE, Vec::new())
    }

    /// Reads the cooked file of `level`'s index.
    ///
    /// A version 1 file (before ADR-2483) is migrated: no node has an atlas and the atlas setting
    /// is off, so every proxy draws flat as it did.
    pub fn from_bytes(bytes: &[u8], level: LevelId) -> Result<Self, WorldFormatError> {
        let body = payload(bytes, world_index_id(level), WORLD_INDEX_TYPE)?;
        Self::decode_body(body)
    }

    /// Reads a cooked world index of any level (the level is the one the file names), for tools.
    ///
    /// # Errors
    ///
    /// As [`Self::from_bytes`], without the asset id check.
    pub fn decode_any(bytes: &[u8]) -> Result<Self, WorldFormatError> {
        let (header, body) = decode_cooked_file(bytes)?;
        if header.type_tag != TypePathHash::of(WORLD_INDEX_TYPE) {
            return Err(WorldFormatError::Corrupt(format!("not a {WORLD_INDEX_TYPE}")));
        }
        let index = Self::decode_body(body)?;
        if header.asset_id != world_index_id(LevelId(index.level)) {
            return Err(WorldFormatError::Corrupt(format!(
                "file holds asset {:?}, which is not the index of level {}",
                header.asset_id, index.level
            )));
        }
        Ok(index)
    }

    /// The payload at either version.
    fn decode_body(body: &[u8]) -> Result<Self, WorldFormatError> {
        check_version(body, WORLD_INDEX_FORMAT_VERSION)?;
        let (found, _) = postcard::take_from_bytes::<u32>(body)
            .map_err(|e| WorldFormatError::Decode(format!("format version: {e}")))?;
        if found <= WORLD_INDEX_FORMAT_VERSION_V1 {
            let old: v1::WorldIndexV1 =
                postcard::from_bytes(body).map_err(|e| WorldFormatError::Decode(e.to_string()))?;
            return Ok(old.into());
        }
        postcard::from_bytes(body).map_err(|e| WorldFormatError::Decode(e.to_string()))
    }

    /// This index as a version 1 file, the atlas fields dropped: what a cook before ADR-2483 wrote,
    /// for migration tests and fuzz fixtures.
    ///
    /// # Errors
    ///
    /// The index does not encode.
    pub fn to_v1_bytes(&self) -> Result<Vec<u8>, WorldFormatError> {
        let id = world_index_id(LevelId(self.level));
        encode(&v1::WorldIndexV1::of(self), id, WORLD_INDEX_TYPE, Vec::new())
    }

    /// The BLAKE3 hash of the cell hashes in coordinate order, for [`Self::content_hash`].
    pub fn hash_cells(cells: &[CellRecord]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for cell in cells {
            hasher.update(&cell.blob_hash);
        }
        *hasher.finalize().as_bytes()
    }

    /// Checks what the runtime relies on: settings, sorted unique cells, known bounds.
    pub fn validate(&self) -> Result<(), WorldFormatError> {
        self.settings
            .validate()
            .map_err(|e| WorldFormatError::Corrupt(format!("settings: {e}")))?;
        for pair in self.cells.windows(2) {
            if pair[0].coord >= pair[1].coord {
                return Err(WorldFormatError::Corrupt(format!(
                    "cell records are not sorted at {}",
                    pair[1].coord
                )));
            }
        }
        let [lo, hi] = self.bounds;
        if let Some(cell) = self
            .cells
            .iter()
            .find(|c| c.coord.x < lo.x || c.coord.x > hi.x || c.coord.z < lo.z || c.coord.z > hi.z)
        {
            return Err(WorldFormatError::Corrupt(format!(
                "cell {} is outside the bounds",
                cell.coord
            )));
        }
        Ok(())
    }
}

impl CellBlob {
    /// The cooked file of this blob. `dependencies` are the record's `deps`.
    pub fn to_bytes(&self, id: AssetId, dependencies: Vec<AssetId>) -> Result<Vec<u8>, WorldFormatError> {
        encode(self, id, CELL_BLOB_TYPE, dependencies)
    }

    /// Reads a cooked blob, checking `id` and the record's hash first. Never panics on bad bytes.
    pub fn from_bytes(bytes: &[u8], id: AssetId, hash: &[u8; 32]) -> Result<Self, WorldFormatError> {
        if blake3::hash(bytes).as_bytes() != hash {
            return Err(WorldFormatError::Corrupt("hash mismatch".into()));
        }
        let body = payload(bytes, id, CELL_BLOB_TYPE)?;
        check_version(body, CELL_BLOB_FORMAT_VERSION)?;
        let blob: Self = postcard::from_bytes(body).map_err(|e| WorldFormatError::Decode(e.to_string()))?;
        if blob.scene.format_version > regia_scene::cooked::COOKED_SCENE_VERSION {
            return Err(WorldFormatError::Newer {
                found: blob.scene.format_version,
                supported: regia_scene::cooked::COOKED_SCENE_VERSION,
            });
        }
        Ok(blob)
    }
}

/// The cooked file of a level's always-loaded set.
pub fn always_loaded_to_bytes(level: LevelId, scene: &CookedScene) -> Result<Vec<u8>, WorldFormatError> {
    encode(scene, always_loaded_id(level), ALWAYS_LOADED_TYPE, Vec::new())
}

/// Reads the cooked always-loaded set of `level`.
pub fn always_loaded_from_bytes(bytes: &[u8], level: LevelId) -> Result<CookedScene, WorldFormatError> {
    let body = payload(bytes, always_loaded_id(level), ALWAYS_LOADED_TYPE)?;
    check_version(body, regia_scene::cooked::COOKED_SCENE_VERSION)?;
    postcard::from_bytes(body).map_err(|e| WorldFormatError::Decode(e.to_string()))
}

/// The serde field list of `.wstream`, for `check-formats`.
pub fn world_index_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<WorldIndex>(),
        regia_core::schema::serde_field_names::<CellRecord>(),
        regia_core::schema::serde_field_names::<HlodIndex>(),
        regia_core::schema::serde_field_names::<HlodAtlas>(),
        regia_core::schema::serde_field_names::<LevelStreaming>(),
    ])
}

/// The `from` versions of `.wstream`'s migration steps, for `check-formats`: version 1 gains the
/// proxy atlases (all `None`) and the atlas settings (off) on the way to 2 (ADR-2483).
pub fn world_index_migration_sources() -> Vec<u32> {
    vec![WORLD_INDEX_FORMAT_VERSION_V1]
}

/// The serde field list of `.wcell`, for `check-formats`.
pub fn cell_blob_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<CellBlob>()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::cooked::COOKED_SCENE_VERSION;

    fn level() -> LevelId {
        LevelId(Guid::from_u128(77))
    }

    fn empty_scene() -> CookedScene {
        CookedScene {
            format_version: COOKED_SCENE_VERSION,
            strings: vec!["a".into()],
            entities: vec![],
            resources: vec![],
        }
    }

    fn blob() -> CellBlob {
        CellBlob {
            format_version: CELL_BLOB_FORMAT_VERSION,
            coord: CellCoord::new(-3, 4),
            scene: empty_scene(),
            anchor: [0, 0, 1],
            nav: Some(vec![1, 2, 3]),
        }
    }

    fn record(coord: CellCoord) -> CellRecord {
        CellRecord {
            coord,
            blob: cell_blob_id(level(), coord),
            blob_hash: [coord.x as u8; 32],
            entity_count: 1,
            own_bytes: 10,
            dep_bytes: 5,
            vram_bytes: 1,
            min_y: -1.0,
            max_y: 2.0,
            priority: 3,
            deps: vec![],
            nav_tiles: 0,
            has_terrain_collision: false,
        }
    }

    fn index() -> WorldIndex {
        let cells = vec![record(CellCoord::new(0, 0)), record(CellCoord::new(1, -2))];
        WorldIndex {
            format_version: WORLD_INDEX_FORMAT_VERSION,
            level: level().0,
            content_hash: WorldIndex::hash_cells(&cells),
            settings: LevelStreaming {
                enabled: true,
                ..LevelStreaming::default()
            },
            cells,
            bounds: [CellCoord::new(0, -2), CellCoord::new(1, 0)],
            hlod: None,
            replicated: vec![(Guid::from_u128(1), CellCoord::new(0, 0))],
            nav: None,
        }
    }

    fn hlod_index() -> HlodIndex {
        use regia_scene::hlod::HlodConfig;
        HlodIndex {
            config: HlodConfig::default(),
            proxies: vec![],
            node_proxies: vec![Some(hlod_proxy_id(level(), 0)), None],
            node_anchors: vec![[1, 0, 2], [0; 3]],
            node_colors: vec![Some([0.5, 0.25, 1.0, 1.0]), None],
            node_atlases: vec![None, None],
        }
    }

    #[test]
    fn a_version_1_index_migrates_with_flat_proxies_and_the_atlas_off() {
        let current = WorldIndex {
            hlod: Some(hlod_index()),
            ..index()
        };
        let old = current.to_v1_bytes().expect("encode v1");
        assert_eq!(world_index_file_version(&old).expect("probe"), Some(1));
        let read = WorldIndex::from_bytes(&old, level()).expect("v1 reads");
        assert_eq!(read, current, "a v1 file reads as the same index with no atlas");
        assert_eq!(read.format_version, WORLD_INDEX_FORMAT_VERSION);
        assert_eq!(read.settings.hlod_atlas_px, 0);
        let hlod = read.hlod.as_ref().expect("hlod");
        assert!(hlod.node_atlases.iter().all(Option::is_none));
        assert_eq!(hlod.node_atlases.len(), hlod.node_proxies.len());

        // upgrade-project: rewritten at the current version, header kept; current files untouched.
        let upgraded = migrate_world_index_file(&old).expect("migrate").expect("older");
        assert_eq!(world_index_file_version(&upgraded).expect("probe"), Some(2));
        assert_eq!(
            WorldIndex::from_bytes(&upgraded, level()).expect("v2 reads"),
            current
        );
        assert_eq!(upgraded, current.to_bytes().expect("encode"));
        assert_eq!(migrate_world_index_file(&upgraded).expect("current"), None);
        // Another cooked type is not this format's.
        let blob = blob()
            .to_bytes(cell_blob_id(level(), CellCoord::new(-3, 4)), vec![])
            .expect("blob");
        assert_eq!(world_index_file_version(&blob).expect("probe"), None);
        assert_eq!(migrate_world_index_file(&blob).expect("not ours"), None);
        assert_eq!(world_index_file_version(b"garbage").expect("probe"), None);
    }

    #[test]
    fn an_index_with_atlases_round_trips_and_names_them() {
        let mut hlod = hlod_index();
        hlod.node_atlases[0] = Some(HlodAtlas {
            material: AssetId::sub_asset(hlod_proxy_id(level(), 0), "material"),
            textures: [1u128, 2, 3].map(|n| AssetId(Guid::from_u128(n))),
            size: [64, 32],
            texels_per_m: 3.5,
            asked_texels_per_m: 4.0,
            capped: true,
            bake_error_m: 0.01,
            miss_ratio: 0.0,
        });
        let index = WorldIndex {
            hlod: Some(hlod),
            settings: LevelStreaming {
                enabled: true,
                hlod_atlas_px: 4,
                ..LevelStreaming::default()
            },
            ..index()
        };
        let bytes = index.to_bytes().expect("encode");
        let read = WorldIndex::from_bytes(&bytes, level()).expect("decode");
        assert_eq!(read, index);
        assert_eq!(WorldIndex::decode_any(&bytes).expect("any level"), index);
        let hlod = read.hlod.expect("hlod");
        assert_eq!(hlod.atlas(0).map(|a| a.size), Some([64, 32]));
        assert!(hlod.atlas(1).is_none() && hlod.atlas(9).is_none());
        assert_eq!(world_index_migration_sources(), vec![1]);
    }

    #[test]
    fn world_index_round_trips() {
        let original = index();
        original.validate().expect("valid");
        let bytes = original.to_bytes().expect("encode");
        assert_eq!(WorldIndex::from_bytes(&bytes, level()).expect("decode"), original);
    }

    #[test]
    fn cell_blob_round_trips() {
        let id = cell_blob_id(level(), CellCoord::new(-3, 4));
        let bytes = blob().to_bytes(id, vec![]).expect("encode");
        let hash = *blake3::hash(&bytes).as_bytes();
        assert_eq!(CellBlob::from_bytes(&bytes, id, &hash).expect("decode"), blob());
    }

    #[test]
    fn a_newer_version_is_refused_with_its_code() {
        let id = cell_blob_id(level(), CellCoord::new(-3, 4));
        let newer = CellBlob {
            format_version: CELL_BLOB_FORMAT_VERSION + 1,
            ..blob()
        };
        let bytes = newer.to_bytes(id, vec![]).expect("encode");
        let hash = *blake3::hash(&bytes).as_bytes();
        let err = CellBlob::from_bytes(&bytes, id, &hash).expect_err("newer");
        assert_eq!(err.cell_reason(), "cell_format_newer");

        let index = WorldIndex {
            format_version: WORLD_INDEX_FORMAT_VERSION + 1,
            ..index()
        };
        let bytes = index.to_bytes().expect("encode");
        assert!(matches!(
            WorldIndex::from_bytes(&bytes, level()),
            Err(WorldFormatError::Newer { .. })
        ));
    }

    #[test]
    fn a_blob_with_a_newer_inner_scene_is_refused() {
        let id = cell_blob_id(level(), CellCoord::new(-3, 4));
        let mut newer = blob();
        newer.scene.format_version = COOKED_SCENE_VERSION + 1;
        let bytes = newer.to_bytes(id, vec![]).expect("encode");
        let hash = *blake3::hash(&bytes).as_bytes();
        let err = CellBlob::from_bytes(&bytes, id, &hash).expect_err("newer");
        assert_eq!(err.cell_reason(), "cell_format_newer");
    }

    #[test]
    fn every_truncation_and_a_flipped_byte_fail_without_a_panic() {
        let id = cell_blob_id(level(), CellCoord::new(-3, 4));
        let bytes = blob().to_bytes(id, vec![]).expect("encode");
        let hash = *blake3::hash(&bytes).as_bytes();
        for len in 0..bytes.len() {
            let err = CellBlob::from_bytes(&bytes[..len], id, &hash).expect_err("truncated");
            assert_eq!(err.cell_reason(), "cell_corrupt", "length {len}");
        }
        // Truncations whose hash was computed for the short file reach the parsers, which must
        // still refuse them cleanly.
        for len in 0..bytes.len() {
            let short = &bytes[..len];
            let short_hash = *blake3::hash(short).as_bytes();
            assert!(
                CellBlob::from_bytes(short, id, &short_hash).is_err(),
                "length {len}"
            );
        }
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x55;
            let err = CellBlob::from_bytes(&flipped, id, &hash).expect_err("flipped");
            assert_eq!(err.cell_reason(), "cell_corrupt", "byte {at}");
        }
    }

    #[test]
    fn a_blob_of_another_asset_or_type_is_corrupt() {
        let id = cell_blob_id(level(), CellCoord::new(-3, 4));
        let bytes = blob().to_bytes(id, vec![]).expect("encode");
        let hash = *blake3::hash(&bytes).as_bytes();
        let other = cell_blob_id(level(), CellCoord::new(0, 0));
        assert_eq!(
            CellBlob::from_bytes(&bytes, other, &hash)
                .expect_err("other")
                .cell_reason(),
            "cell_corrupt"
        );
        let index_bytes = index().to_bytes().expect("encode");
        assert!(WorldIndex::from_bytes(&bytes, level()).is_err());
        assert!(
            CellBlob::from_bytes(&index_bytes, world_index_id(level()), &[0; 32]).is_err(),
            "wrong type"
        );
    }

    #[test]
    fn validate_refuses_unsorted_cells_and_cells_outside_the_bounds() {
        let mut unsorted = index();
        unsorted.cells.reverse();
        assert!(unsorted.validate().is_err());
        let mut outside = index();
        outside.bounds[1] = CellCoord::new(0, 0);
        assert!(outside.validate().is_err());
        let mut bad = index();
        bad.settings.cell_size_m = 1.0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn ids_are_stable_for_the_same_level_and_differ_per_cell() {
        let a = cell_blob_id(level(), CellCoord::new(1, 2));
        assert_eq!(a, cell_blob_id(level(), CellCoord::new(1, 2)));
        assert_ne!(a, cell_blob_id(level(), CellCoord::new(2, 1)));
        assert_ne!(hlod_proxy_id(level(), 0), hlod_proxy_id(level(), 1));
        assert_ne!(world_index_id(level()), always_loaded_id(level()));
    }

    #[test]
    fn always_loaded_round_trips() {
        let bytes = always_loaded_to_bytes(level(), &empty_scene()).expect("encode");
        assert_eq!(
            always_loaded_from_bytes(&bytes, level()).expect("decode"),
            empty_scene()
        );
        assert!(always_loaded_from_bytes(&bytes, LevelId(Guid::from_u128(5))).is_err());
    }
}
