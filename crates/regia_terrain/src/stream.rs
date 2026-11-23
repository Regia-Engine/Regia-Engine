// SPDX-License-Identifier: MPL-2.0
//! Photoreal terrain DATA-path streaming: a memory-budgeted tile cache for the tiled macro/layer
//! textures cooked by `regia_asset_pipeline::terrain_data`.
//!
//! This module is the *runtime* half of the terrain data lane (the cook half is
//! `regia_asset_pipeline::terrain_data`). It provides:
//!
//! * [`TileKey`] — the cache key `(texture slot, mip, tile x, tile y)`.
//! * [`TiledTexture`] — the description of one streamed texture (name, size, tile grid, format)
//!   that both the renderer and the loader agree on.
//! * [`TileSource`] — the load backend: [`PakTileSource`] reads cooked tile textures from the
//!   mounted paks / cooked cache, and [`MemoryTileSource`] serves tiles from RAM for tests.
//! * [`TileCache`] — an LRU cache with a byte budget, distance-ordered requests, and a background
//!   loader thread so no tile IO ever runs on the render thread. The renderer calls
//!   [`TileCache::request`] each frame and [`TileCache::residency`] to look up resident tiles.
//!
//! The deterministic tile → asset-id mapping ([`tile_asset_id`]) matches the cooker, so a tile
//! cooked at a given `(slot, mip, x, y)` is the same asset the cache requests. See
//! `docs/formats/terrain.md` § 7 and `docs/evidence/terrain-data/`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use regia_asset::{AssetId, PakPayload};
use thiserror::Error;
use tracing::{debug, warn};

/// The edge length of a virtual-texture page, in pixels (matches the cooker's default).
pub const DEFAULT_TILE_SIZE: u32 = 256;

/// The block-compressed format of a tiled texture (mirrors the cooker's [`TileFormat`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileFormat {
    /// BC7 sRGB (albedo).
    Bc7Srgb,
    /// BC5 two-channel (normal RG).
    Bc5,
    /// BC4 single channel (AO, height).
    Bc4,
    /// BC7 linear (masks, ORM).
    Bc7Linear,
}

/// Identifies one resident tile: `(texture slot, mip level, tile column, tile row)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    /// Texture slot index (assigned when the texture is registered).
    pub texture: u8,
    /// Mip level of the source texture.
    pub mip: u8,
    /// Tile column within the mip's tile grid.
    pub x: u32,
    /// Tile row within the mip's tile grid.
    pub y: u32,
}

impl TileKey {
    /// The key of this tile in a load set: `"<texture>:<mip>:<x>:<y>"`.
    #[must_use]
    pub fn load_key(self) -> String {
        format!("{}:{}:{}:{}", self.texture, self.mip, self.x, self.y)
    }

    /// Parses [`Self::load_key`].
    #[must_use]
    pub fn parse_load_key(key: &str) -> Option<Self> {
        let mut it = key.split(':');
        let tile = Self {
            texture: it.next()?.parse().ok()?,
            mip: it.next()?.parse().ok()?,
            x: it.next()?.parse().ok()?,
            y: it.next()?.parse().ok()?,
        };
        it.next().is_none().then_some(tile)
    }
}

/// Description of one streamed texture; registered once, shared by the renderer and the loader.
#[derive(Clone, Debug, PartialEq)]
pub struct TiledTexture {
    /// Stable slot name (`"macro_albedo"`, `"mask0"`, `"layer2_orm"`, …).
    pub name: String,
    /// Source width in pixels (mip 0).
    pub width: u32,
    /// Source height in pixels (mip 0).
    pub height: u32,
    /// Tile edge length in pixels.
    pub tile_size: u32,
    /// The slot's desktop BCn choice. Android payloads are ASTC 4×4 whatever this says (read the
    /// KTX2 `vkFormat` of a tile or the tail for the real block format; ADR-5294).
    pub format: TileFormat,
    /// Total mip levels (`floor(log2(width)) + 1`).
    pub mip_count: u32,
    /// Mip levels that live in the resident tail (first mip smaller than `tile_size`).
    pub tail_mips: u32,
}

impl TiledTexture {
    /// Builds the description of a square, power-of-two tiled texture.
    ///
    /// # Panics
    ///
    /// Panics if `size` is not a power of two, or `tile_size` is not a power of two dividing `size`.
    pub fn new(name: impl Into<String>, size: u32, tile_size: u32, format: TileFormat) -> Self {
        assert!(size.is_power_of_two(), "texture size must be a power of two");
        assert!(tile_size.is_power_of_two(), "tile size must be a power of two");
        assert!(
            size.is_multiple_of(tile_size),
            "tile size must divide the texture size"
        );
        let mip_count = size.ilog2() + 1;
        Self {
            name: name.into(),
            width: size,
            height: size,
            tile_size,
            format,
            mip_count,
            tail_mips: tail_mip_count(size, tile_size),
        }
    }

    /// Tiles per side at a mip level (`0` for mips that live in the tail).
    pub fn tiles_per_side(&self, mip: u32) -> u32 {
        if mip >= self.tail_start_mip() {
            return 0;
        }
        (self.width / self.tile_size) >> mip
    }

    /// Number of tiles at a mip level.
    pub fn tile_count(&self, mip: u32) -> u32 {
        let s = self.tiles_per_side(mip);
        s * s
    }

    /// The first mip level that lives in the tail (mips `>=` this are not tiled). `0` (nothing
    /// tiled) when the tile is larger than the texture or its size is 0, as a corrupt or
    /// hand-written terrain index may say.
    pub fn tail_start_mip(&self) -> u32 {
        match self.width.checked_div(self.tile_size) {
            Some(tiles) if tiles > 0 => tiles.ilog2() + 1,
            _ => 0,
        }
    }
}

/// Number of tail mips for a `size`×`size` texture tiled at `tile_size`: the first mip smaller
/// than `tile_size` down to 4×4 (the smallest block-compressible level). Matches the cooker.
fn tail_mip_count(size: u32, tile_size: u32) -> u32 {
    let tail_start = (size / tile_size).ilog2() + 1;
    // Sizes below 4 have no block-compressible mip, hence no tail (and no underflow).
    let last = size.ilog2().saturating_sub(2);
    if tail_start <= last {
        last - tail_start + 1
    } else {
        0
    }
}

/// Errors produced by terrain streaming.
#[derive(Debug, Error)]
pub enum StreamError {
    /// Tile IO failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A tile's cooked asset is not present.
    #[error("tile {key:?} not found")]
    NotFound {
        /// The missing tile.
        key: TileKey,
    },

    /// A tile's cooked asset failed to decode.
    #[error("asset load error: {0}")]
    Asset(#[from] regia_asset::AssetLoadError),

    /// A pak file failed to read.
    #[error("pak error: {0}")]
    Pak(#[from] regia_asset::PakError),

    /// No texture is registered for a slot.
    #[error("no texture registered for slot {slot}")]
    NoTexture {
        /// The unregistered slot.
        slot: u8,
    },
}

/// The deterministic asset-path string of a tile (shared with the cooker).
pub fn tile_asset_path(slot: &str, mip: u32, x: u32, y: u32) -> String {
    format!("terrain/{slot}/mip{mip}/tile_{x}_{y}")
}

/// The deterministic [`AssetId`] of a tile, derived from the map id and the tile path.
pub fn tile_asset_id(map_id: AssetId, slot: &str, mip: u32, x: u32, y: u32) -> AssetId {
    AssetId::sub_asset(map_id, &tile_asset_path(slot, mip, x, y))
}

/// The asset-path string of a texture's resident tail (matches the cooker).
pub fn tail_asset_path(slot: &str) -> String {
    format!("terrain/{slot}/tail")
}

/// The deterministic [`AssetId`] of a texture's resident tail, derived from the map id and slot.
pub fn tail_asset_id(map_id: AssetId, slot: &str) -> AssetId {
    AssetId::sub_asset(map_id, &tail_asset_path(slot))
}

/// The `mip` value in a [`TileKey`] that addresses a texture's resident tail (the coarse mips that
/// live below the tile size). Real tile mips are always `< tail_start_mip()`, so `u8::MAX` can never
/// collide with a tile.
pub const TAIL_MIP: u8 = u8::MAX;

/// The [`TileKey`] that addresses a texture's resident tail.
pub fn tail_key(slot: u8) -> TileKey {
    TileKey {
        texture: slot,
        mip: TAIL_MIP,
        x: 0,
        y: 0,
    }
}

/// Version of the cooked terrain index format (see `docs/formats/terrain.md` § 9 and § 14). The
/// index is a postcard payload wrapped in a standard `.fckd` cooked file; loaders reject unknown
/// versions. Version 2 appends the optional exact heights, holes and photoreal material entries
/// (ADR-8024) and then [`TerrainIndex::source_mapping`] (ADR-8471): one unreleased v2 holds both
/// (ADR-8619); a version 1 index migrates with all four `None`. Version 3 appends the source
/// grid's Z spacing when it differs from X and its valid-data mask
/// ([`TerrainIndex::source_spacing_z_m`], [`TerrainIndex::source_valid_mask`], ADR-8871); a
/// version 1 or 2 index migrates with both `None`.
pub const TERRAIN_INDEX_FORMAT_VERSION: u32 = 3;

/// The index versions [`load_terrain_index_bytes`] migrates to [`TERRAIN_INDEX_FORMAT_VERSION`].
#[must_use]
pub fn terrain_index_migration_sources() -> &'static [u32] {
    &[1, 2]
}

/// Fingerprint of the cooked terrain index's field layout (`cargo xtask check-formats`): the
/// field names and types of [`TerrainIndex`] and its entries, in serialization order. A change of
/// the postcard shape must change this string and bump [`TERRAIN_INDEX_FORMAT_VERSION`].
#[must_use]
pub fn terrain_index_schema_fingerprint() -> String {
    concat!(
        "TerrainIndex{version:u32,map_id:AssetId,slots:Vec<TerrainSlotIndex>,",
        "heightfield:Option<TerrainHeightfieldIndex>,layers:Vec<String>,world_size_m:Option<f32>,",
        "world_origin:Option<[f32;3]>,heights:Option<TerrainHeightsIndex>,holes:Option<TerrainHolesIndex>,",
        "material:Option<TerrainMaterialIndex>,source_mapping:Option<CountMapping>,",
        "source_spacing_z_m:Option<f32>,source_valid_mask:Option<TerrainValidMaskIndex>}",
        "|TerrainSlotIndex{name:String,width:u32,height:u32,tile_size:u32,format:TileFormat{Bc7Srgb,Bc5,Bc4,",
        "Bc7Linear},mip_count:u32,tail_mips:u32,tile_count:u32,bytes:u64}",
        "|TerrainHeightfieldIndex{width:u32,height:u32,tile_size:u32,tiles_x:u32,tiles_y:u32,bytes:u64}",
        "|TerrainHeightsIndex{width:u32,height:u32,encoding:TerrainHeightsEncoding{Residual,Raw},",
        "height_scale:f32,height_offset:f32,bytes:u64}",
        "|TerrainHolesIndex{bytes:u64}",
        "|TerrainMaterialIndex{params:TerrainMaterialParamsIndex{macro_world_size:f32,macro_strength:f32,",
        "macro_normal_strength:f32,anti_tiling_cell:f32,detail_fade_start:f32,detail_fade_end:f32,",
        "triplanar_auto_slope:f32},macro_tiles:u32,layer_albedo:u32,layer_normal:u32,layer_height:u32,",
        "layer_orm:u32,macro_albedo_tiles:u32,macro_normal_tiles:u32,macro_coarse_albedo:bool,",
        "macro_coarse_normal:bool,bytes:u64}",
        "|CountMapping{cols:u32,rows:u32,meters_per_count:f32,zero_counts:u16,cell_m:f32,sample0_m:[f32;2]}",
        "|TerrainValidMaskIndex{cols:u32,rows:u32,missing:u32,bits:Vec<u8>}",
    )
    .to_string()
}

/// The version-1 terrain index payload: [`TerrainIndex`] without the four version-2 entries.
#[derive(serde::Serialize, serde::Deserialize)]
struct TerrainIndexV1 {
    version: u32,
    map_id: AssetId,
    slots: Vec<TerrainSlotIndex>,
    heightfield: Option<TerrainHeightfieldIndex>,
    layers: Vec<String>,
    world_size_m: Option<f32>,
    world_origin: Option<[f32; 3]>,
}

/// The version-2 terrain index payload: [`TerrainIndex`] without the two version-3 entries.
#[derive(serde::Serialize, serde::Deserialize)]
struct TerrainIndexV2 {
    version: u32,
    map_id: AssetId,
    slots: Vec<TerrainSlotIndex>,
    heightfield: Option<TerrainHeightfieldIndex>,
    layers: Vec<String>,
    world_size_m: Option<f32>,
    world_origin: Option<[f32; 3]>,
    heights: Option<TerrainHeightsIndex>,
    holes: Option<TerrainHolesIndex>,
    material: Option<TerrainMaterialIndex>,
    source_mapping: Option<crate::source::CountMapping>,
}

/// The reflect type path of a cooked terrain index (used as its `.fckd` type tag).
pub const TERRAIN_INDEX_TYPE_PATH: &str = "regia_terrain::stream::TerrainIndex";

/// The deterministic asset-path string of a map's terrain index (shared with the cooker).
pub fn terrain_index_asset_path() -> String {
    "terrain/index".to_string()
}

/// The deterministic [`AssetId`] of a map's terrain index, derived from the map id.
pub fn terrain_index_asset_id(map_id: AssetId) -> AssetId {
    AssetId::sub_asset(map_id, &terrain_index_asset_path())
}

/// The deterministic asset-path string of a map's tiled heightfield (shared with the cooker).
pub fn heightfield_asset_path() -> String {
    "terrain/heightfield".to_string()
}

/// The deterministic [`AssetId`] of a map's tiled heightfield, derived from the map id.
pub fn heightfield_asset_id(map_id: AssetId) -> AssetId {
    AssetId::sub_asset(map_id, &heightfield_asset_path())
}

/// One texture slot in a cooked terrain index: the tiling metadata the streamer needs to register
/// the slot and address its tiles (`docs/formats/terrain.md` § 9).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerrainSlotIndex {
    /// Stable slot name (`"macro_albedo"`, `"layer0_orm"`, …).
    pub name: String,
    /// Source width in pixels (mip 0).
    pub width: u32,
    /// Source height in pixels (mip 0).
    pub height: u32,
    /// Tile edge length in pixels.
    pub tile_size: u32,
    /// The slot's desktop BCn choice. Android payloads are ASTC 4×4 whatever this says (read the
    /// KTX2 `vkFormat` of a tile or the tail for the real block format; ADR-5294).
    pub format: TileFormat,
    /// Total mip levels.
    pub mip_count: u32,
    /// Mip levels that live in the resident tail.
    pub tail_mips: u32,
    /// Total number of tiles across all tiled mips (for stats and validation).
    pub tile_count: u32,
    /// Total cooked bytes of this slot (tail + every tile) on disk, for `regia cook --stats`.
    pub bytes: u64,
}

impl TerrainSlotIndex {
    /// Builds the [`TiledTexture`] description the streamer registers for this slot.
    pub fn texture(&self) -> TiledTexture {
        TiledTexture {
            name: self.name.clone(),
            width: self.width,
            height: self.height,
            tile_size: self.tile_size,
            format: self.format,
            mip_count: self.mip_count,
            tail_mips: self.tail_mips,
        }
    }
}

/// The tiled-heightfield layout of a cooked terrain (see `docs/formats/terrain.md` § 8).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerrainHeightfieldIndex {
    /// Width in samples.
    pub width: u32,
    /// Height in samples.
    pub height: u32,
    /// Tile edge length in samples.
    pub tile_size: u32,
    /// Number of tile columns.
    pub tiles_x: u32,
    /// Number of tile rows.
    pub tiles_y: u32,
    /// Cooked bytes of the tiled heightfield on disk.
    pub bytes: u64,
}

/// How `terrain/heights` stores a cooked terrain's exact heights (`docs/formats/terrain.md` § 14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TerrainHeightsEncoding {
    /// f32 residuals over the heights of the R16 heightfield (`terrain/heightfield`).
    Residual,
    /// The heights themselves, f32 metres.
    Raw,
}

/// The exact final heights of a cooked terrain (`terrain/heights`, ADR-8024).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerrainHeightsIndex {
    /// Width in samples (equals the R16 heightfield's).
    pub width: u32,
    /// Height in samples (equals the R16 heightfield's).
    pub height: u32,
    /// How the values are stored.
    pub encoding: TerrainHeightsEncoding,
    /// Metres of a full-scale R16 sample (65535): the map's height scale.
    pub height_scale: f32,
    /// Metres of a zero R16 sample: the map's height offset.
    pub height_offset: f32,
    /// Cooked bytes of `terrain/heights` on disk.
    pub bytes: u64,
}

/// The holes of a cooked terrain (`terrain/holes`, the `.holes.ron` text, ADR-8024).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerrainHolesIndex {
    /// Cooked bytes of `terrain/holes` on disk.
    pub bytes: u64,
}

/// The [`crate::MaterialParams`] of a cooked photoreal material, field for field.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerrainMaterialParamsIndex {
    /// See [`crate::MaterialParams::macro_world_size`].
    pub macro_world_size: f32,
    /// See [`crate::MaterialParams::macro_strength`].
    pub macro_strength: f32,
    /// See [`crate::MaterialParams::macro_normal_strength`].
    pub macro_normal_strength: f32,
    /// See [`crate::MaterialParams::anti_tiling_cell`].
    pub anti_tiling_cell: f32,
    /// See [`crate::MaterialParams::detail_fade_start`].
    pub detail_fade_start: f32,
    /// See [`crate::MaterialParams::detail_fade_end`].
    pub detail_fade_end: f32,
    /// See [`crate::MaterialParams::triplanar_auto_slope`].
    pub triplanar_auto_slope: f32,
}

impl From<crate::MaterialParams> for TerrainMaterialParamsIndex {
    fn from(p: crate::MaterialParams) -> Self {
        Self {
            macro_world_size: p.macro_world_size,
            macro_strength: p.macro_strength,
            macro_normal_strength: p.macro_normal_strength,
            anti_tiling_cell: p.anti_tiling_cell,
            detail_fade_start: p.detail_fade_start,
            detail_fade_end: p.detail_fade_end,
            triplanar_auto_slope: p.triplanar_auto_slope,
        }
    }
}

impl From<TerrainMaterialParamsIndex> for crate::MaterialParams {
    fn from(p: TerrainMaterialParamsIndex) -> Self {
        Self {
            macro_world_size: p.macro_world_size,
            macro_strength: p.macro_strength,
            macro_normal_strength: p.macro_normal_strength,
            anti_tiling_cell: p.anti_tiling_cell,
            detail_fade_start: p.detail_fade_start,
            detail_fade_end: p.detail_fade_end,
            triplanar_auto_slope: p.triplanar_auto_slope,
        }
    }
}

/// The photoreal material of a cooked terrain: its knobs and how many KTX2 blobs of each kind
/// sit at `terrain/material/<kind>/<i>` (ADR-8024; [`crate::cooked_terrain::material_blob_asset_id`]).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerrainMaterialIndex {
    /// The material knobs.
    pub params: TerrainMaterialParamsIndex,
    /// Macro tile grid side (`MacroBake::tiles`); 0 = no macro set.
    pub macro_tiles: u32,
    /// Layer albedo blobs.
    pub layer_albedo: u32,
    /// Layer normal blobs.
    pub layer_normal: u32,
    /// Layer height blobs.
    pub layer_height: u32,
    /// Layer ORM blobs (0 keeps the scalar roughness).
    pub layer_orm: u32,
    /// Macro albedo tile blobs.
    pub macro_albedo_tiles: u32,
    /// Macro normal tile blobs.
    pub macro_normal_tiles: u32,
    /// Whether the coarse whole-map albedo blob exists.
    pub macro_coarse_albedo: bool,
    /// Whether the coarse whole-map normal blob exists.
    pub macro_coarse_normal: bool,
    /// Cooked bytes of every material blob file on disk.
    pub bytes: u64,
}

/// The cooked tile index of one terrain map: everything the runtime streamer needs to register its
/// texture slots and address every tile deterministically (see `docs/formats/terrain.md` § 9), and
/// since version 2 the optional exact heights, holes and photoreal material a host cooked
/// (§ 12, ADR-8024).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerrainIndex {
    /// Format version; loaders reject unknown versions.
    pub version: u32,
    /// The map id every tile id derives from (the `.meta` id of the `.terrain.layers.ron` source).
    pub map_id: AssetId,
    /// Texture slots in registration order (the `texture` field of [`TileKey`] is this position).
    pub slots: Vec<TerrainSlotIndex>,
    /// Tiled-heightfield layout, or `None` when the source had no heightfield.
    pub heightfield: Option<TerrainHeightfieldIndex>,
    /// Material layer names, bottom-first (from the `.terrain.layers.ron`).
    pub layers: Vec<String>,
    /// World width in metres from the sibling `heightfield.json` (`size_m`), when present.
    #[serde(default)]
    pub world_size_m: Option<f32>,
    /// World origin in metres from the sibling `heightfield.json` (`origin`), when present.
    #[serde(default)]
    pub world_origin: Option<[f32; 3]>,
    /// The exact final heights (`terrain/heights`), when a host cooked them (version 2).
    #[serde(default)]
    pub heights: Option<TerrainHeightsIndex>,
    /// The holes (`terrain/holes`), when a host cooked them (version 2).
    #[serde(default)]
    pub holes: Option<TerrainHolesIndex>,
    /// The photoreal material (`terrain/material/...`), when a host cooked it (version 2).
    #[serde(default)]
    pub material: Option<TerrainMaterialIndex>,
    /// The heightfield's count mapping (datum, scale, cell and registration) from a schema-2
    /// `heightfield.json`, when it declares one (version 2, ADR-8471). `None` keeps the level's
    /// scale, offset and width, as every version-1 index does.
    #[serde(default)]
    pub source_mapping: Option<crate::source::CountMapping>,
    /// The source grid's sample spacing on Z, meters, when it differs from X (`source_mapping`'s
    /// `cell_m`), from a raster descriptor's `spacing_m` (version 3, ADR-8871). `None` means the
    /// spacing is `cell_m` on both axes, as in every version-1 and version-2 index; an equal Z
    /// spacing is always written as `None`, so one grid has one encoding. Needs `source_mapping`.
    #[serde(default)]
    pub source_spacing_z_m: Option<f32>,
    /// The source grid's valid-data mask, when some samples hold no data (version 3,
    /// ADR-8871). `None` means every sample holds data. Needs `source_mapping` and has its grid.
    #[serde(default)]
    pub source_valid_mask: Option<TerrainValidMaskIndex>,
}

/// The valid-data mask of a cooked count source (`TerrainIndex::source_valid_mask`, version 3,
/// ADR-8871): which samples of the tiled counts hold data. A sample without data keeps its count
/// verbatim in `terrain/heightfield` and must never be read as a height.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerrainValidMaskIndex {
    /// Columns (X), equal to the mapping's.
    pub cols: u32,
    /// Rows (Z), equal to the mapping's.
    pub rows: u32,
    /// Samples without data: at least one, fewer than `cols * rows`.
    pub missing: u32,
    /// `cols * rows` flags in canonical order (row 0 south, column 0 west), row-major, eight to a
    /// byte, least significant bit first, `1` = has data; the unused bits of the last byte are 0
    /// ([`crate::source::ValidMask::to_packed_bytes`]).
    pub bits: Vec<u8>,
}

impl TerrainValidMaskIndex {
    /// The cooked entry of `mask`, or `None` when every sample holds data (an all-valid mask is
    /// never written).
    #[must_use]
    pub fn from_valid_mask(mask: &crate::source::ValidMask) -> Option<Self> {
        let missing = u32::try_from(mask.invalid_count()).ok()?;
        if missing == 0 {
            return None;
        }
        let [cols, rows] = mask.dimensions();
        Some(Self {
            cols,
            rows,
            missing,
            bits: mask.to_packed_bytes(),
        })
    }

    /// The run-time mask.
    ///
    /// # Errors
    /// `terrain_raster_mask` when the bytes do not fit the grid, a padding bit is set, or
    /// `missing` disagrees with the bits.
    pub fn to_valid_mask(&self) -> Result<crate::source::ValidMask, crate::source::SourceError> {
        let mask = crate::source::ValidMask::from_packed_bytes(self.cols, self.rows, &self.bits)?;
        if mask.invalid_count() != self.missing as usize {
            return Err(crate::source::SourceError(format!(
                "terrain_raster_mask: the mask records {} missing samples but its bits hold {}",
                self.missing,
                mask.invalid_count()
            )));
        }
        Ok(mask)
    }
}

/// The postcard version varint at the start of a terrain-index payload.
fn terrain_index_payload_version(bytes: &[u8]) -> Result<u32, postcard::Error> {
    postcard::take_from_bytes::<u32>(bytes).map(|(version, _)| version)
}

impl TerrainIndex {
    /// Serialises to postcard bytes (the payload of the cooked `.fckd` file).
    ///
    /// # Errors
    ///
    /// Returns a postcard error only for implausibly large inputs.
    ///
    /// A version-1 index is written in the version-1 layout (no version-2 entries); a mapping,
    /// heights, holes or material on a version-1 index is refused, since that layout cannot hold it.
    /// A version-2 index is written in the version-2 layout; a Z spacing or a mask on it is refused
    /// the same way.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        if !matches!(self.version, 1 | 2) {
            return postcard::to_allocvec(self);
        }
        if self.source_spacing_z_m.is_some() || self.source_valid_mask.is_some() {
            return Err(postcard::Error::SerdeSerCustom);
        }
        if self.version == 2 {
            return postcard::to_allocvec(&TerrainIndexV2 {
                version: self.version,
                map_id: self.map_id,
                slots: self.slots.clone(),
                heightfield: self.heightfield.clone(),
                layers: self.layers.clone(),
                world_size_m: self.world_size_m,
                world_origin: self.world_origin,
                heights: self.heights.clone(),
                holes: self.holes.clone(),
                material: self.material.clone(),
                source_mapping: self.source_mapping,
            });
        }
        if self.source_mapping.is_some()
            || self.heights.is_some()
            || self.holes.is_some()
            || self.material.is_some()
        {
            return Err(postcard::Error::SerdeSerCustom);
        }
        postcard::to_allocvec(&TerrainIndexV1 {
            version: self.version,
            map_id: self.map_id,
            slots: self.slots.clone(),
            heightfield: self.heightfield.clone(),
            layers: self.layers.clone(),
            world_size_m: self.world_size_m,
            world_origin: self.world_origin,
        })
    }

    /// Deserialises a postcard payload. A version 1 payload (v0.5.0) is decoded with the v1 shape
    /// and migrated: version [`TERRAIN_INDEX_FORMAT_VERSION`], no heights, holes, material, mapping,
    /// Z spacing or mask. A version 2 payload (v0.6.0) is decoded with the v2 shape and migrated:
    /// no Z spacing or mask, every other value kept. Other versions decode with today's shape and
    /// keep their number ([`load_terrain_index_bytes`] refuses a version it does not read).
    ///
    /// # Errors
    ///
    /// Returns a postcard error when the bytes do not decode to a [`TerrainIndex`].
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, postcard::Error> {
        match terrain_index_payload_version(bytes)? {
            1 => {
                let v1: TerrainIndexV1 = postcard::from_bytes(bytes)?;
                debug_assert_eq!(v1.version, 1);
                Ok(Self {
                    version: TERRAIN_INDEX_FORMAT_VERSION,
                    map_id: v1.map_id,
                    slots: v1.slots,
                    heightfield: v1.heightfield,
                    layers: v1.layers,
                    world_size_m: v1.world_size_m,
                    world_origin: v1.world_origin,
                    heights: None,
                    holes: None,
                    material: None,
                    source_mapping: None,
                    source_spacing_z_m: None,
                    source_valid_mask: None,
                })
            }
            2 => {
                let v2: TerrainIndexV2 = postcard::from_bytes(bytes)?;
                debug_assert_eq!(v2.version, 2);
                Ok(Self {
                    version: TERRAIN_INDEX_FORMAT_VERSION,
                    map_id: v2.map_id,
                    slots: v2.slots,
                    heightfield: v2.heightfield,
                    layers: v2.layers,
                    world_size_m: v2.world_size_m,
                    world_origin: v2.world_origin,
                    heights: v2.heights,
                    holes: v2.holes,
                    material: v2.material,
                    source_mapping: v2.source_mapping,
                    source_spacing_z_m: None,
                    source_valid_mask: None,
                })
            }
            _ => postcard::from_bytes(bytes),
        }
    }

    /// The source grid's sample spacing `[X, Z]` in meters, when the index has a count mapping
    /// (`source_mapping`'s `cell_m` on X; on Z `source_spacing_z_m`, else `cell_m`).
    #[must_use]
    pub fn source_spacing_m(&self) -> Option<[f32; 2]> {
        let mapping = self.source_mapping?;
        Some([mapping.cell_m, self.source_spacing_z_m.unwrap_or(mapping.cell_m)])
    }

    /// Sets `source_mapping`, `source_spacing_z_m` and `source_valid_mask` from a count source
    /// (its [`crate::source::TerrainSource::mapping`], its Z spacing when it differs from X and
    /// its mask when a sample holds no data): what a version-3 index can hold of the source grid.
    #[must_use]
    pub fn with_source_grid(mut self, source: &crate::source::TerrainSource) -> Self {
        let mapping = source.mapping();
        let [x, z] = source.spacing_m();
        self.source_mapping = Some(mapping);
        self.source_spacing_z_m = (x.to_bits() != z.to_bits()).then_some(z);
        self.source_valid_mask = source
            .valid_mask()
            .and_then(TerrainValidMaskIndex::from_valid_mask);
        self
    }

    /// Why a consumer that builds the run-time map from `source_mapping` alone cannot use this
    /// index yet, or `None`: a Z spacing other than X (`terrain_mapping_unrepresentable`, the
    /// run-time map has one cell size) or samples without data (`terrain_raster_missing_data`,
    /// no run-time hole policy reads the mask yet). ADR-8871.
    #[must_use]
    pub fn source_grid_unsupported(&self) -> Option<String> {
        if let Some(z) = self.source_spacing_z_m {
            return Some(format!(
                "terrain_mapping_unrepresentable: the cooked source spacing {:?} differs on X and Z; \
                 the run-time map holds one cell size and nothing is resampled",
                self.source_spacing_m().unwrap_or([f32::NAN, z])
            ));
        }
        self.source_valid_mask.as_ref().map(|mask| {
            format!(
                "terrain_raster_missing_data: {} of {} cooked samples hold no data; \
                 no run-time hole policy reads the cooked mask yet",
                mask.missing,
                u64::from(mask.cols) * u64::from(mask.rows)
            )
        })
    }

    /// The slot index (the `texture` field of [`TileKey`]) for a slot name.
    pub fn slot_index(&self, name: &str) -> Option<u8> {
        self.slots.iter().position(|s| s.name == name).map(|i| i as u8)
    }

    /// Builds the [`TiledTexture`] description of the slot at `slot`.
    pub fn texture(&self, slot: u8) -> Option<TiledTexture> {
        self.slots.get(slot as usize).map(TerrainSlotIndex::texture)
    }

    /// Total cooked bytes across every slot and the heightfield (for `regia cook --stats`).
    pub fn total_bytes(&self) -> u64 {
        let slots: u64 = self.slots.iter().map(|s| s.bytes).sum();
        let hf: u64 = self.heightfield.as_ref().map(|h| h.bytes).unwrap_or(0);
        let heights = self.heights.as_ref().map_or(0, |h| h.bytes);
        let holes = self.holes.as_ref().map_or(0, |h| h.bytes);
        let material = self.material.as_ref().map_or(0, |m| m.bytes);
        slots + hf + heights + holes + material
    }
}

/// Errors decoding a cooked terrain index.
#[derive(Debug, Error)]
pub enum TerrainIndexError {
    /// Cooked wrapper decode failure.
    #[error("cooked file error: {0}")]
    Cooked(#[from] regia_asset::CookedIoError),

    /// Payload is not the terrain-index type.
    #[error("asset {id} is not a terrain index (type tag {found:#x})")]
    WrongType {
        /// Asset id.
        id: AssetId,
        /// Type tag found in the file.
        found: u64,
    },

    /// Postcard decode failure.
    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),

    /// Unsupported terrain-index version.
    #[error("unsupported terrain index version {found} (supported {supported})")]
    BadVersion {
        /// Version found.
        found: u32,
        /// Version this build reads.
        supported: u32,
    },

    /// The tiled heightfield payload is the wrong version, size, or coverage.
    #[error("tiled heightfield: {0}")]
    BadHeightfield(String),

    /// A texture slot has a size the streamer cannot tile safely.
    #[error("terrain index slot `{name}`: {reason}")]
    BadSlot {
        /// Slot name.
        name: String,
        /// What is wrong with the slot geometry.
        reason: String,
    },

    /// Reading the cooked bytes failed.
    #[error("stream error: {0}")]
    Read(#[from] StreamError),

    /// The exact heights (`terrain/heights`) are malformed or do not fit the index (ADR-8024).
    #[error("terrain heights: {0}")]
    BadHeights(String),

    /// The cooked holes (`terrain/holes`) are malformed (ADR-8024).
    #[error("terrain holes: {0}")]
    BadHoles(String),

    /// The cooked photoreal material entry or a blob of it is malformed (ADR-8024).
    #[error("terrain material: {0}")]
    BadMaterial(String),

    /// The version-3 source-grid entries (Z spacing, valid-data mask) are malformed or do not fit
    /// the mapping (ADR-8871).
    #[error("terrain source grid: {0}")]
    BadSourceGrid(String),
}

/// Decodes a cooked terrain index `.fckd` file (wrapper + postcard payload) and validates its
/// version.
///
/// # Errors
///
/// Returns [`TerrainIndexError`] for a bad wrapper, a wrong type tag, a postcard failure or an
/// unsupported version.
pub fn load_terrain_index_bytes(bytes: &[u8]) -> Result<TerrainIndex, TerrainIndexError> {
    let (header, payload) = regia_asset::decode_cooked_file(bytes)?;
    let expected = regia_core::id::TypePathHash::of(TERRAIN_INDEX_TYPE_PATH);
    if header.type_tag != expected {
        return Err(TerrainIndexError::WrongType {
            id: header.asset_id,
            found: header.type_tag.get(),
        });
    }
    load_terrain_index_payload(payload)
}

/// Decodes a terrain-index postcard payload (the body of the `.fckd`): migrates a version 1
/// payload, refuses any other version this build does not read, and validates the slots and the
/// version 2 entries.
///
/// # Errors
///
/// [`TerrainIndexError`] for a postcard failure, an unsupported version or bad entries.
pub fn load_terrain_index_payload(payload: &[u8]) -> Result<TerrainIndex, TerrainIndexError> {
    // The version is read before the body: a newer index is refused with its version error
    // even when its body no longer decodes with this build's shape.
    let version = terrain_index_payload_version(payload)?;
    if version != TERRAIN_INDEX_FORMAT_VERSION && !terrain_index_migration_sources().contains(&version) {
        return Err(TerrainIndexError::BadVersion {
            found: version,
            supported: TERRAIN_INDEX_FORMAT_VERSION,
        });
    }
    let index = TerrainIndex::from_postcard(payload)?;
    validate_index_slots(&index)?;
    validate_index_cooked(&index)?;
    validate_index_source_grid(&index)?;
    Ok(index)
}

/// The payload version of a cooked terrain-index `.fckd` file, or `None` for another cooked type
/// (`regia upgrade-project`'s probe, ADR-8024).
///
/// # Errors
///
/// The failure as text when the wrapper does not decode or the payload has no version.
pub fn terrain_index_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let (header, payload) = regia_asset::decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != regia_core::id::TypePathHash::of(TERRAIN_INDEX_TYPE_PATH) {
        return Ok(None);
    }
    terrain_index_payload_version(payload)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Rewrites an older cooked terrain index at [`TERRAIN_INDEX_FORMAT_VERSION`], cooked header kept
/// (its payload length updated): a version-1 index gains empty heights, holes, material and `source_mapping`,
/// a version-1 or version-2 index an empty Z spacing and mask, every other value unchanged. `Ok(None)` for another cooked type or an index already current.
///
/// # Errors
/// A message for a bad wrapper, a payload that does not decode, or a version newer than this build.
pub fn migrate_terrain_index_file(bytes: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let Some(version) = terrain_index_file_version(bytes)? else {
        return Ok(None);
    };
    if version > TERRAIN_INDEX_FORMAT_VERSION {
        return Err(format!(
            "terrain index version {version} is newer than this build ({TERRAIN_INDEX_FORMAT_VERSION})"
        ));
    }
    if version == TERRAIN_INDEX_FORMAT_VERSION {
        return Ok(None);
    }
    let (mut header, payload) = regia_asset::decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    let mut index = load_terrain_index_payload(payload).map_err(|e| e.to_string())?;
    index.version = TERRAIN_INDEX_FORMAT_VERSION;
    let out = index.to_postcard().map_err(|e| e.to_string())?;
    header.payload_len = out.len() as u64;
    regia_asset::encode_cooked_file(&header, &out)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Rejects version 2 entries that would make a loader allocate without bound or read past the
/// R16 grid: heights of another size than the heightfield (or residuals without one), a
/// non-finite scale or offset, more layer blobs than [`crate::layers::MAX_LAYERS`] or more
/// macro tiles than the grid holds.
fn validate_index_cooked(index: &TerrainIndex) -> Result<(), TerrainIndexError> {
    if let Some(heights) = &index.heights {
        let bad = |reason: String| Err(TerrainIndexError::BadHeights(reason));
        match (&index.heightfield, heights.encoding) {
            (Some(hf), _) if hf.width != heights.width || hf.height != heights.height => {
                return bad(format!(
                    "heights are {}x{} but the heightfield is {}x{}",
                    heights.width, heights.height, hf.width, hf.height
                ));
            }
            (None, TerrainHeightsEncoding::Residual) => {
                return bad("residual heights need the R16 heightfield".into());
            }
            _ => {}
        }
        if heights.width < 2
            || heights.height < 2
            || heights.width > MAX_SLOT_EDGE
            || heights.height > MAX_SLOT_EDGE
        {
            return bad(format!(
                "heights size {}x{} is out of range",
                heights.width, heights.height
            ));
        }
        if !heights.height_scale.is_finite() || !heights.height_offset.is_finite() {
            return bad("height scale and offset must be finite".into());
        }
    }
    if let Some(material) = &index.material {
        let bad = |reason: String| Err(TerrainIndexError::BadMaterial(reason));
        let layers = crate::layers::MAX_LAYERS as u32;
        for (kind, count) in [
            ("layer_albedo", material.layer_albedo),
            ("layer_normal", material.layer_normal),
            ("layer_height", material.layer_height),
            ("layer_orm", material.layer_orm),
        ] {
            if count > layers {
                return bad(format!("{count} {kind} blobs (at most {layers})"));
            }
        }
        let side = crate::material::MAX_MACRO_TILES_PER_SIDE;
        if material.macro_tiles > side {
            return bad(format!("macro grid {} (at most {side})", material.macro_tiles));
        }
        let tiles = material.macro_tiles * material.macro_tiles;
        if material.macro_albedo_tiles > tiles || material.macro_normal_tiles > tiles {
            return bad(format!(
                "{} albedo / {} normal macro tiles for a {}x{} grid",
                material.macro_albedo_tiles,
                material.macro_normal_tiles,
                material.macro_tiles,
                material.macro_tiles
            ));
        }
    }
    Ok(())
}

/// Rejects version-3 source-grid entries that do not describe the mapping's grid: a Z spacing
/// without a mapping, not finite and positive, equal to X (written as `None`) or whose last row
/// is not past the first; a mask without a mapping, of another grid, of the wrong byte count, with
/// a padding bit set, with a `missing` count the bits disagree with, or with no sample (or every
/// sample) missing.
fn validate_index_source_grid(index: &TerrainIndex) -> Result<(), TerrainIndexError> {
    let bad = |reason: String| Err(TerrainIndexError::BadSourceGrid(reason));
    let mapping = index.source_mapping;
    if let Some(z) = index.source_spacing_z_m {
        let Some(m) = mapping else {
            return bad("a Z spacing needs the count mapping".into());
        };
        if !z.is_finite() || z <= 0.0 {
            return bad(format!("Z spacing {z} must be positive and finite"));
        }
        if z.to_bits() == m.cell_m.to_bits() {
            return bad("a Z spacing equal to X is written as none".into());
        }
        let first = f64::from(m.sample0_m[1]);
        let last = (first + f64::from(m.rows.saturating_sub(1)) * f64::from(z)) as f32;
        if !last.is_finite() || last <= m.sample0_m[1] {
            return bad("the Z extent must be finite and representable".into());
        }
    }
    if let Some(mask) = &index.source_valid_mask {
        let Some(m) = mapping else {
            return bad("a valid-data mask needs the count mapping".into());
        };
        if [mask.cols, mask.rows] != [m.cols, m.rows] {
            return bad(format!(
                "the mask is {}x{} but the mapping {}x{}",
                mask.cols, mask.rows, m.cols, m.rows
            ));
        }
        let samples = u64::from(mask.cols) * u64::from(mask.rows);
        if mask.missing == 0 || u64::from(mask.missing) >= samples {
            return bad(format!(
                "{} of {samples} samples missing (at least one and not every one)",
                mask.missing
            ));
        }
        mask.to_valid_mask()
            .map_err(|e| TerrainIndexError::BadSourceGrid(e.0))?;
    }
    Ok(())
}

/// Largest slot edge (pixels) and tiles per side at mip 0 the streamer accepts from a cooked index.
const MAX_SLOT_EDGE: u32 = 65_536;
const MAX_SLOT_TILES_PER_SIDE: u32 = 1024;

/// Rejects slot geometry that would panic or loop for ever in the tile math
/// (`tail_start_mip`, `tail_mip_count`, `TileCache::request`).
fn validate_index_slots(index: &TerrainIndex) -> Result<(), TerrainIndexError> {
    for slot in &index.slots {
        let bad = |reason: &str| TerrainIndexError::BadSlot {
            name: slot.name.clone(),
            reason: reason.to_string(),
        };
        if slot.tile_size == 0 || !slot.tile_size.is_power_of_two() {
            return Err(bad("tile size must be a non-zero power of two"));
        }
        if slot.width != slot.height || slot.width < slot.tile_size.max(4) || slot.width > MAX_SLOT_EDGE {
            return Err(bad(
                "size must be square, at least one tile and 4 pixels, and within the cap",
            ));
        }
        if !slot.width.is_multiple_of(slot.tile_size) {
            return Err(bad("tile size must divide the slot size"));
        }
        if slot.width / slot.tile_size > MAX_SLOT_TILES_PER_SIDE {
            return Err(bad("too many tiles per side"));
        }
    }
    Ok(())
}

/// Reflect type path of a cooked tiled heightfield (the cooker's `.fckd` type tag).
pub const TILED_HEIGHTFIELD_TYPE_PATH: &str = "regia_asset_pipeline::terrain_data::TiledHeightfield";

/// Postcard version of [`CookedTiledHeightfield`].
pub const TILED_HEIGHTFIELD_FORMAT_VERSION: u32 = 1;

/// One tile of a cooked heightfield. Field order matches the cooker's postcard payload
/// (`docs/formats/terrain.md` § 8).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CookedHeightfieldTile {
    /// Tile column.
    pub x: u32,
    /// Tile row.
    pub y: u32,
    /// Lowest sample in the tile.
    pub min: u16,
    /// Highest sample in the tile.
    pub max: u16,
    /// Row-major `tile_size`² samples, including the cooker's border padding.
    pub samples: Vec<u16>,
}

/// Cooked tiled heightfield. The runtime stitches this back into a [`regia_asset::Heightfield`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CookedTiledHeightfield {
    /// Format version.
    pub version: u32,
    /// Width in samples.
    pub width: u32,
    /// Height in samples.
    pub height: u32,
    /// Tile edge length in samples.
    pub tile_size: u32,
    /// Tiles in cook order.
    pub tiles: Vec<CookedHeightfieldTile>,
}

/// Copies in-bounds samples out of cooked tiles.
///
/// The cooker pads the last tile with the border sample when `width` or `height`
/// is not a multiple of `tile_size` (`docs/formats/terrain.md` § 8). Those padded
/// texels are outside the map and are dropped.
///
/// # Errors
///
/// Returns [`TerrainIndexError::BadHeightfield`] for a bad version, size, or tile.
pub fn stitch_tiled_heightfield(
    tiled: &CookedTiledHeightfield,
) -> Result<regia_asset::Heightfield, TerrainIndexError> {
    if tiled.version != TILED_HEIGHTFIELD_FORMAT_VERSION {
        return Err(TerrainIndexError::BadHeightfield(format!(
            "version {} (supported {TILED_HEIGHTFIELD_FORMAT_VERSION})",
            tiled.version
        )));
    }
    if tiled.width < 2 || tiled.height < 2 || tiled.tile_size == 0 {
        return Err(TerrainIndexError::BadHeightfield(format!(
            "size {}x{} tile {}",
            tiled.width, tiled.height, tiled.tile_size
        )));
    }
    // Validate every tile (grid position, uniqueness, length) and the declared map area against
    // the samples that really exist before allocating anything sized from the header: a hostile
    // header must not make the allocation below huge.
    let tile_area = u128::from(tiled.tile_size) * u128::from(tiled.tile_size);
    let tiles_x = tiled.width.div_ceil(tiled.tile_size);
    let tiles_y = tiled.height.div_ceil(tiled.tile_size);
    let mut seen = HashSet::with_capacity(tiled.tiles.len());
    let mut available = 0u128;
    for tile in &tiled.tiles {
        // A tile outside the grid or a second copy of one is a corrupt file: the cooker writes
        // each tile once, and a duplicate would hide an edit (`recook_tiles` updates the first).
        if tile.x >= tiles_x || tile.y >= tiles_y {
            return Err(TerrainIndexError::BadHeightfield(format!(
                "tile {},{} is outside the {tiles_x}x{tiles_y} tile grid",
                tile.x, tile.y
            )));
        }
        if !seen.insert((tile.x, tile.y)) {
            return Err(TerrainIndexError::BadHeightfield(format!(
                "tile {},{} appears twice",
                tile.x, tile.y
            )));
        }
        if tile.samples.len() as u128 != tile_area {
            return Err(TerrainIndexError::BadHeightfield(format!(
                "tile {},{} has {} samples, expected {tile_area}",
                tile.x,
                tile.y,
                tile.samples.len()
            )));
        }
        available += tile_area;
    }
    let map_area = u128::from(tiled.width) * u128::from(tiled.height);
    if map_area > available {
        return Err(TerrainIndexError::BadHeightfield(
            "tiles do not cover the heightfield".into(),
        ));
    }
    let mut samples = vec![0u16; map_area as usize];
    let mut covered = vec![false; samples.len()];
    for tile in &tiled.tiles {
        for row in 0..tiled.tile_size {
            let dst_row = tile.y.saturating_mul(tiled.tile_size).saturating_add(row);
            if dst_row >= tiled.height {
                continue;
            }
            for col in 0..tiled.tile_size {
                let dst_col = tile.x.saturating_mul(tiled.tile_size).saturating_add(col);
                if dst_col >= tiled.width {
                    continue;
                }
                let dst = dst_row as usize * tiled.width as usize + dst_col as usize;
                let src = row as usize * tiled.tile_size as usize + col as usize;
                samples[dst] = tile.samples[src];
                covered[dst] = true;
            }
        }
    }
    if covered.iter().any(|hit| !hit) {
        return Err(TerrainIndexError::BadHeightfield(
            "tiles do not cover the heightfield".into(),
        ));
    }
    Ok(regia_asset::Heightfield {
        width: tiled.width,
        height: tiled.height,
        samples,
    })
}

/// Decodes a cooked tiled-heightfield `.fckd` file and stitches it.
///
/// # Errors
///
/// Returns [`TerrainIndexError`] for a bad wrapper, the wrong type tag, a postcard
/// failure, or a tile set that does not cover the map.
pub fn load_tiled_heightfield_bytes(bytes: &[u8]) -> Result<regia_asset::Heightfield, TerrainIndexError> {
    stitch_tiled_heightfield(&decode_cooked_tiled_heightfield(bytes)?)
}

/// Decodes a cooked tiled-heightfield `.fckd` file into its tiles (no stitching): what the
/// editor's incremental re-cook updates in place ([`crate::edit::recook_tiles`]).
///
/// # Errors
///
/// Returns [`TerrainIndexError`] for a bad wrapper, the wrong type tag or a postcard failure.
pub fn decode_cooked_tiled_heightfield(bytes: &[u8]) -> Result<CookedTiledHeightfield, TerrainIndexError> {
    let (header, payload) = regia_asset::decode_cooked_file(bytes)?;
    let expected = regia_core::id::TypePathHash::of(TILED_HEIGHTFIELD_TYPE_PATH);
    if header.type_tag != expected {
        return Err(TerrainIndexError::WrongType {
            id: header.asset_id,
            found: header.type_tag.get(),
        });
    }
    Ok(postcard::from_bytes(payload)?)
}

/// A load backend for tile bytes. Implementations must be [`Send`] and [`Sync`] so they can live
/// on the cache's background loader thread.
pub trait TileSource: Send + Sync + 'static {
    /// Loads the KTX2 bytes of one tile: a view of the mapped pak for a tile stored
    /// uncompressed ([`regia_asset::PakPayload`]; build one from a `Vec<u8>` with `.into()`).
    ///
    /// # Errors
    ///
    /// Returns [`StreamError`] when the tile cannot be read or decoded.
    fn load(&self, texture: &TiledTexture, key: TileKey) -> Result<PakPayload, StreamError>;
}

/// Serves tiles from an in-memory map (tests and offline previews).
#[derive(Default)]
pub struct MemoryTileSource {
    /// Tile bytes keyed by `(slot, mip, x, y)`.
    tiles: HashMap<TileKey, Vec<u8>>,
}

impl MemoryTileSource {
    /// An empty source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts tile bytes for a key.
    pub fn insert(&mut self, key: TileKey, bytes: Vec<u8>) {
        self.tiles.insert(key, bytes);
    }
}

impl TileSource for MemoryTileSource {
    fn load(&self, _texture: &TiledTexture, key: TileKey) -> Result<PakPayload, StreamError> {
        self.tiles
            .get(&key)
            .cloned()
            .map(PakPayload::from)
            .ok_or(StreamError::NotFound { key })
    }
}

/// Loads tile textures from mounted paks and/or the cooked cache.
///
/// A tile is stored as a [`regia_asset::CookedTexture`] whose asset id is [`tile_asset_id`]; the
/// KTX2 payload is returned verbatim. Paks are searched in reverse mount order (later paks override),
/// then the cooked cache — the same precedence as [`regia_asset::AssetServer`].
pub struct PakTileSource {
    /// Mounted paks, in filename order (later overrides earlier). Shared with every tile served
    /// from them, so a resident tile keeps its pak's mapping alive.
    paks: Vec<Arc<regia_asset::PakReader>>,
    /// Cooked cache directory (dev fallback).
    cache_dir: PathBuf,
    /// Parent map id the tile ids derive from.
    map_id: AssetId,
}

impl PakTileSource {
    /// Opens paks from `content_dir` (if any `*.fpak` exist) and remembers the cooked cache.
    ///
    /// Applies the asset server's zero-copy rule ([`Self::with_verify`] with
    /// [`regia_asset::PakVerifyOptions::from_env`]): a pak this build has not verified yet is
    /// read as copies, never as views of its mapping.
    pub fn new(map_id: AssetId, content_dir: Option<&Path>, cache_dir: PathBuf) -> Self {
        let verify = regia_asset::PakVerifyOptions::from_env();
        Self::with_verify(map_id, content_dir, cache_dir, verify.as_ref())
    }

    /// [`Self::new`] with explicit integrity-check options (`None`: the check is off).
    ///
    /// A mapped, hashed pak missing from the `pak_verify` record (the first launch after an
    /// update) has its zero-copy gate closed for the life of this source: tiles are copied
    /// through the file handle, so a pak cut in place is an error, not a `SIGBUS` through a
    /// resident tile. The asset server's background check records the pak, and the next launch
    /// serves views again ([`regia_asset::pak_verify::gate_until_verified`]).
    pub fn with_verify(
        map_id: AssetId,
        content_dir: Option<&Path>,
        cache_dir: PathBuf,
        verify: Option<&regia_asset::PakVerifyOptions>,
    ) -> Self {
        let mut paks = Vec::new();
        if let Some(content) = content_dir
            && let Ok(dir) = std::fs::read_dir(content)
        {
            let mut paths: Vec<PathBuf> = dir
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "fpak"))
                .map(|e| e.path())
                .collect();
            paths.sort();
            for path in paths {
                match regia_asset::PakReader::open(&path) {
                    Ok(reader) => {
                        // No verify thread here: the gate stays closed until a later launch.
                        let _closed = regia_asset::pak_verify::gate_until_verified(&reader, &path, verify);
                        paks.push(Arc::new(reader));
                    }
                    Err(e) => warn!(
                        target: "regia_terrain",
                        pak = %path.display(),
                        error = %e,
                        "failed to open terrain pak"
                    ),
                }
            }
        }
        Self {
            paks,
            cache_dir,
            map_id,
        }
    }

    /// Reads the raw cooked-file bytes for a tile asset id: paks first, then the cache. A pak
    /// entry stored uncompressed is a view of the mapped pak (no copy).
    pub(crate) fn read_bytes(&self, id: AssetId) -> Result<PakPayload, StreamError> {
        for pak in self.paks.iter().rev() {
            if pak.contains(id) {
                return Ok(pak.read_shared(id)?);
            }
        }
        let path = self.cache_dir.join(format!("{id}.fckd"));
        Ok(std::fs::read(&path)?.into())
    }

    /// Loads and decodes the map's terrain index (`terrain/index`), reading the cooked `.fckd`
    /// file from the mounted paks or the cooked cache.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainIndexError`] when the file cannot be read or decoded.
    pub fn load_terrain_index(&self) -> Result<TerrainIndex, TerrainIndexError> {
        let id = terrain_index_asset_id(self.map_id);
        let bytes = self.read_bytes(id)?;
        load_terrain_index_bytes(&bytes)
    }

    /// Loads `terrain/heightfield`, checks the cooker type tag, and stitches the tiles
    /// into one [`regia_asset::Heightfield`].
    ///
    /// # Errors
    ///
    /// Returns [`TerrainIndexError`] when the cooked file is missing or invalid.
    pub fn load_heightfield(&self) -> Result<regia_asset::Heightfield, TerrainIndexError> {
        let id = heightfield_asset_id(self.map_id);
        let bytes = self.read_bytes(id)?;
        load_tiled_heightfield_bytes(&bytes)
    }
}

impl TileSource for PakTileSource {
    fn load(&self, texture: &TiledTexture, key: TileKey) -> Result<PakPayload, StreamError> {
        let id = if key.mip == TAIL_MIP {
            tail_asset_id(self.map_id, &texture.name)
        } else {
            tile_asset_id(self.map_id, &texture.name, u32::from(key.mip), key.x, key.y)
        };
        let bytes = self.read_bytes(id)?;
        let (_header, cooked) = regia_asset::load_texture_shared(&bytes)?;
        Ok(cooked.ktx2)
    }
}

/// One load request queued to the worker thread.
struct LoadRequest {
    key: TileKey,
    texture: TiledTexture,
}

/// One load result returned from the worker thread.
struct LoadResponse {
    key: TileKey,
    /// `None`: the request was cancelled before the worker read it.
    result: Option<Result<PakPayload, String>>,
}

/// A resident tile entry.
struct Entry {
    bytes: PakPayload,
    size: u64,
}

/// Longest wait between two retries of a tile that failed to load, in [`TileCache::request`]
/// calls (the wait doubles after each failure: 2, 4, 8, … up to this).
pub const MAX_TILE_RETRY_INTERVAL: u64 = 256;

/// The most failed tiles [`TileCache`] remembers. Past this the failure that may be retried
/// soonest is forgotten, so a long walk over sparse content does not grow the table with the
/// world (audit wave 10c, stream M2); a forgotten tile is only asked for again sooner.
pub const MAX_FAILED_TILES: usize = 4096;

/// A tile whose last load failed: when it may be requested again.
#[derive(Clone, Copy, Debug)]
struct Failure {
    /// Failed loads in a row.
    attempts: u32,
    /// The [`TileCache::request`] call count from which it may be requested again.
    retry_at: u64,
}

/// A memory-budgeted, distance-requested tile cache with a background loader thread.
///
/// The render thread drives [`TileCache::request`] (enqueue nearest-first) and
/// [`TileCache::tick`] (drain finished loads, evict over-budget entries); all file IO and decoding
/// happens on the worker thread, so the render thread never blocks on a load.
pub struct TileCache {
    budget: u64,
    current: u64,
    entries: HashMap<TileKey, Entry>,
    /// LRU queue: the front is the least-recently-used tile.
    lru: VecDeque<TileKey>,
    pending: HashSet<TileKey>,
    /// Keys that must never be evicted (a texture's resident tail).
    pinned: HashSet<TileKey>,
    /// Pinned tails larger than the whole budget: dropped on arrival and never re-requested.
    oversized: HashSet<TileKey>,
    /// Tiles whose last load failed, held back from [`TileCache::request`] with a doubling
    /// wait so a missing tile neither floods the loader nor takes the request slots of the
    /// tiles behind it.
    failed: HashMap<TileKey, Failure>,
    /// [`TileCache::request`] calls so far (the clock of the retry wait).
    requests: u64,
    /// Loads handed to the loader thread so far (tiles and tails, cancelled ones included).
    loads_issued: u64,
    /// Tiles whose camera distance [`Self::request`] has evaluated.
    #[cfg(test)]
    tiles_visited: u64,
    /// Queued loads no request wants any more. The worker skips them and answers `None`, so a
    /// camera that jumps does not wait behind the reads of where it was (audit wave 10c,
    /// stream M1). Shared with the worker; holds only keys that are also `pending`.
    cancelled: Arc<std::sync::Mutex<HashSet<TileKey>>>,
    /// The tiles every [`TileCache::request`] since the last [`TileCache::tick`] wanted (all
    /// views of a frame), and the textures they asked for: `tick` cancels the queued tiles of
    /// those textures that none of them wanted.
    round_wanted: HashSet<TileKey>,
    round_slots: HashSet<u8>,
    /// Tiles wanted since the last [`TileCache::tick`], with the nearest camera distance any
    /// request this round reported. The first request of a slot in the round replaces that
    /// slot's entries; later requests merge, keeping the minimum distance (ADR-5219). Eviction
    /// takes tiles no request wants first, then the farthest wanted one, so the tile under a
    /// camera is not the first victim of a drain (audit wave 6, stream H2).
    wanted: HashMap<TileKey, f32>,
    /// Last visible-tile query per slot. A repeat of the same camera, radius and texture
    /// reuses it instead of walking the mip chain.
    scan_cache: Vec<(TileScanKey, Vec<(f32, TileKey)>)>,
    /// Wanted tiles evicted over budget. One is not requested again while it is no nearer than
    /// the farthest wanted tile still resident (it would only be evicted again), and is forgotten
    /// once no request wants it.
    evicted_wanted: HashSet<TileKey>,
    textures: HashMap<u8, TiledTexture>,
    request_tx: Option<mpsc::Sender<LoadRequest>>,
    response_rx: mpsc::Receiver<LoadResponse>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// What a visible-tile query depends on. Equal bits reuse the cached candidates.
#[derive(Clone, Copy, PartialEq, Eq)]
struct TileScanKey {
    slot: u8,
    camera: [u32; 2],
    radius: u32,
    world_width: u32,
    width: u32,
    height: u32,
    tile_size: u32,
    tail_start: u32,
}

/// Saturating `floor` of a finite `f32` into `i64`. Non-finite values go to the signed end
/// they point at, so a huge camera cannot wrap the tile index.
fn sat_floor_i64(value: f32) -> i64 {
    if !value.is_finite() {
        return if value.is_sign_negative() {
            i64::MIN
        } else {
            i64::MAX
        };
    }
    if value >= i64::MAX as f32 {
        i64::MAX
    } else if value <= i64::MIN as f32 {
        i64::MIN
    } else {
        value as i64
    }
}

/// Tile columns or rows that can meet a camera, plus one tile of margin. `per_side` is exclusive.
fn tile_axis_range(
    camera: f32,
    reach: f32,
    half: f32,
    tile_world: f32,
    per_side: u32,
) -> std::ops::Range<u32> {
    if per_side == 0
        || !tile_world.is_finite()
        || tile_world <= 0.0
        || !camera.is_finite()
        || !reach.is_finite()
        || !half.is_finite()
    {
        return 0..0;
    }
    let side = i64::from(per_side);
    let first = sat_floor_i64(((camera - reach + half) / tile_world - 0.5).floor()).saturating_sub(1);
    let last = sat_floor_i64(((camera + reach + half) / tile_world - 0.5).floor()).saturating_add(1);
    let start = first.clamp(0, side);
    let end = last.saturating_add(1).clamp(0, side);
    if start >= end {
        return 0..0;
    }
    (start as u32)..(end as u32)
}

impl TileCache {
    /// Spawns the loader thread and returns an empty cache with `budget` bytes.
    pub fn new<S: TileSource>(budget: u64, source: S) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<LoadRequest>();
        let (response_tx, response_rx) = mpsc::channel::<LoadResponse>();
        let shutdown = Arc::new(AtomicBool::new(false));

        let worker_source = Arc::new(source);
        let worker_shutdown = Arc::clone(&shutdown);
        let cancelled: Arc<std::sync::Mutex<HashSet<TileKey>>> = Arc::default();
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::Builder::new()
            .name("terrain-tile-loader".to_string())
            .spawn(move || {
                while !worker_shutdown.load(Ordering::Relaxed) {
                    match request_rx.recv_timeout(std::time::Duration::from_millis(5)) {
                        Ok(req) => {
                            let skip = worker_cancelled
                                .lock()
                                .map(|mut cancelled| cancelled.remove(&req.key))
                                .unwrap_or(false);
                            let result = (!skip).then(|| {
                                worker_source
                                    .load(&req.texture, req.key)
                                    .map_err(|e| e.to_string())
                            });
                            let _ = response_tx.send(LoadResponse { key: req.key, result });
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("spawn terrain tile loader");

        Self {
            budget,
            current: 0,
            entries: HashMap::new(),
            lru: VecDeque::new(),
            pending: HashSet::new(),
            pinned: HashSet::new(),
            oversized: HashSet::new(),
            failed: HashMap::new(),
            requests: 0,
            loads_issued: 0,
            #[cfg(test)]
            tiles_visited: 0,
            cancelled,
            round_wanted: HashSet::new(),
            round_slots: HashSet::new(),
            wanted: HashMap::new(),
            scan_cache: Vec::new(),
            evicted_wanted: HashSet::new(),
            textures: HashMap::new(),
            request_tx: Some(request_tx),
            response_rx,
            shutdown,
            worker: Some(worker),
        }
    }

    /// Registers a texture description under a slot index (the `texture` field of [`TileKey`]).
    ///
    /// The texture's resident tail ([`tail_key`]) is pinned so a payload that fits the budget
    /// is never evicted: the coarse mips are the overview the renderer always needs. A tail
    /// larger than the whole budget is not cached, and a later [`Self::request`] does not
    /// ask for it again.
    pub fn register_texture(&mut self, slot: u8, texture: TiledTexture) {
        self.textures.insert(slot, texture);
        self.pinned.insert(tail_key(slot));
    }

    /// The number of resident tiles.
    pub fn resident_count(&self) -> usize {
        self.entries.len()
    }

    /// The number of in-flight loads.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// The loads [`Self::request`] has handed to the loader thread since the cache was made
    /// (tiles and tails, including ones later cancelled). Unlike [`Self::pending_count`] it
    /// never goes down, so it still shows a request whose load finished and was drained by the
    /// same frame's [`Self::tick`].
    pub fn loads_issued(&self) -> u64 {
        self.loads_issued
    }

    /// The tiles the requests of the current round want, sorted (tests only).
    #[cfg(test)]
    pub(crate) fn wanted_keys(&self) -> Vec<TileKey> {
        let mut keys: Vec<TileKey> = self.wanted.keys().copied().collect();
        keys.sort();
        keys
    }

    /// The number of tiles whose last load failed (held back from [`Self::request`] until
    /// their retry wait passes, [`MAX_TILE_RETRY_INTERVAL`]).
    pub fn failed_count(&self) -> usize {
        self.failed.len()
    }

    /// Forgets every load failure, so the next [`Self::request`] asks for those tiles again
    /// at once (after a pak was mounted or the cooked cache rebuilt).
    pub fn clear_failed(&mut self) {
        self.failed.clear();
    }

    /// `true` when `key` failed to load and its retry wait has not passed yet.
    fn held_back(&self, key: TileKey) -> bool {
        self.failed.get(&key).is_some_and(|f| self.requests < f.retry_at)
    }

    /// Total resident bytes.
    ///
    /// After [`TileCache::tick`], non-pinned tiles are evicted until the total is within
    /// [`Self::budget`] or every remaining tile is pinned. Pinned tails are never evicted,
    /// so two or more tails that each fit can leave the total over the budget. One pinned
    /// payload larger than the whole budget is not cached.
    pub fn current_bytes(&self) -> u64 {
        self.current
    }

    /// The configured byte budget.
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Whether `key` is resident and immediately usable.
    pub fn is_resident(&self, key: TileKey) -> bool {
        self.entries.contains_key(&key)
    }

    /// Marks the terrain-tile items of `sets` ([`regia_asset::LoadKind::TerrainTile`], keyed by
    /// [`TileKey::load_key`]) resident or evicted to match this cache. Returns how many items
    /// changed. Call after [`Self::tick`] (cheap: one lookup per terrain-tile item).
    pub fn report_residency(&self, sets: &regia_asset::LoadSets) -> usize {
        let kind = regia_asset::LoadKind::TerrainTile;
        let mut changed = 0;
        for key in sets.keys(kind) {
            let Some(tile) = TileKey::parse_load_key(&key) else {
                continue;
            };
            let now = self.is_resident(tile);
            if now != sets.item_resident(kind, &key) {
                if now {
                    sets.mark_resident(kind, &key);
                } else {
                    sets.mark_evicted(kind, &key);
                }
                changed += 1;
            }
        }
        changed
    }

    /// Looks up a resident tile's bytes and marks it most-recently-used.
    /// The bytes are shared (a clone is an `Arc` bump), not copied.
    pub fn residency(&mut self, key: TileKey) -> Option<PakPayload> {
        let bytes = self.entries.get(&key)?.bytes.clone();
        self.touch_lru(key);
        Some(bytes)
    }

    /// Enqueues up to `max_requests` tiles of `slot` whose center is at most `visible_radius`
    /// plus one full tile edge (`tile_world`) from `camera`, nearest first. Returns the number
    /// of *tiles* enqueued. A tile can miss the disk of `visible_radius` and still be queued:
    /// the test adds the whole edge, not half the tile diagonal.
    ///
    /// `world_width` maps the texture's UV space onto the terrain in metres, so a tile's world size
    /// is `world_width / tiles_per_side(mip)`. Resident and already-pending tiles are skipped. The
    /// texture's resident tail ([`tail_key`]) is always enqueued too (once), independently of
    /// distance and of `max_requests`, so the coarse overview mips are loaded eagerly. A tail
    /// larger than the whole budget is dropped when it arrives and is not requested again.
    ///
    /// A non-finite `visible_radius` (NaN or ±∞) queues no tiles: the scan treats it as no
    /// view rather than an infinite one (queuing every tile of every mip would blow the
    /// budget). Only the resident-tail request below still applies. No current caller passes
    /// a non-finite radius; the guard exists so a bad view can never request the whole map.
    pub fn request(
        &mut self,
        slot: u8,
        camera: [f32; 2],
        world_width: f32,
        visible_radius: f32,
        max_requests: usize,
    ) -> usize {
        let Some(desc) = self.textures.get(&slot) else {
            warn!(target: "regia_terrain", slot, "request for unregistered texture");
            return 0;
        };
        let scan_key = TileScanKey {
            slot,
            camera: [camera[0].to_bits(), camera[1].to_bits()],
            radius: visible_radius.to_bits(),
            world_width: world_width.to_bits(),
            width: desc.width,
            height: desc.height,
            tile_size: desc.tile_size,
            tail_start: desc.tail_start_mip(),
        };
        self.requests += 1;
        let index = if let Some(index) = self.scan_cache.iter().position(|(key, _)| *key == scan_key) {
            index
        } else {
            let mut found = self.collect_visible(scan_key, camera, world_width, visible_radius);
            found.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            self.scan_cache.retain(|(key, _)| key.slot != slot);
            self.scan_cache.push((scan_key, found));
            self.scan_cache.len() - 1
        };
        // The first request of this slot since the last tick replaces that slot's wanted set.
        // A later view in the same round merges, keeping the nearer distance.
        if self.round_slots.insert(slot) {
            self.wanted.retain(|key, _| key.texture != slot);
        }
        let count = self.scan_cache[index].1.len();
        let mut i = 0;
        while i < count {
            let (dist, key) = {
                let cached = &self.scan_cache[index].1;
                cached[i]
            };
            i += 1;
            self.round_wanted.insert(key);
            self.wanted
                .entry(key)
                .and_modify(|have| {
                    if dist < *have {
                        *have = dist;
                    }
                })
                .or_insert(dist);
        }
        let wanted_now = &self.wanted;
        self.evicted_wanted
            .retain(|key| key.texture != slot || wanted_now.contains_key(key));
        if let Ok(mut cancelled) = self.cancelled.lock() {
            // A queued tile wanted again loads after all.
            let round = &self.round_wanted;
            cancelled.retain(|key| !round.contains(key));
        }
        let farthest_resident = self
            .entries
            .keys()
            .filter(|key| key.texture == slot && !self.pinned.contains(*key))
            .filter_map(|key| self.wanted.get(key).copied())
            .max_by(f32::total_cmp);

        let mut enqueued = 0;
        let mut i = 0;
        while i < count {
            if enqueued >= max_requests {
                break;
            }
            let (dist, key) = {
                let cached = &self.scan_cache[index].1;
                cached[i]
            };
            i += 1;
            if self.entries.contains_key(&key) || self.pending.contains(&key) || self.held_back(key) {
                continue;
            }
            if self.evicted_wanted.contains(&key) && farthest_resident.is_none_or(|far| dist >= far) {
                continue;
            }
            let Some(texture) = self.textures.get(&slot).cloned() else {
                break;
            };
            let Some(tx) = &self.request_tx else {
                break;
            };
            if tx.send(LoadRequest { key, texture }).is_ok() {
                self.pending.insert(key);
                self.loads_issued += 1;
                enqueued += 1;
            }
        }

        // Always request the resident tail (the coarse overview mips), independent of distance.
        // It is pinned (see [`TileCache::register_texture`]) so it stays resident once loaded.
        let tail = tail_key(slot);
        if !self.entries.contains_key(&tail)
            && !self.pending.contains(&tail)
            && !self.oversized.contains(&tail)
            && !self.held_back(tail)
        {
            let Some(texture) = self.textures.get(&slot).cloned() else {
                return enqueued;
            };
            let Some(tx) = &self.request_tx else {
                return enqueued;
            };
            if tx.send(LoadRequest { key: tail, texture }).is_ok() {
                self.pending.insert(tail);
                self.loads_issued += 1;
            }
        }
        enqueued
    }

    /// Tiles of `scan_key` inside the camera radius, queried by the grid range rather than
    /// every texel of every mip. Counts each distance test in [`Self::tiles_visited`].
    fn collect_visible(
        &mut self,
        scan_key: TileScanKey,
        camera: [f32; 2],
        world_width: f32,
        visible_radius: f32,
    ) -> Vec<(f32, TileKey)> {
        let mut found = Vec::new();
        if scan_key.tile_size == 0
            || !camera[0].is_finite()
            || !camera[1].is_finite()
            || !visible_radius.is_finite()
            || !world_width.is_finite()
            || world_width <= 0.0
        {
            return found;
        }
        let half = world_width * 0.5;
        for mip in 0..scan_key.tail_start {
            let per_side = (scan_key.width / scan_key.tile_size) >> mip;
            if per_side == 0 {
                continue;
            }
            let tile_world = world_width / per_side as f32;
            let reach = visible_radius + tile_world;
            let xs = tile_axis_range(camera[0], reach, half, tile_world, per_side);
            let ys = tile_axis_range(camera[1], reach, half, tile_world, per_side);
            for y in ys.start..ys.end {
                for x in xs.start..xs.end {
                    #[cfg(test)]
                    {
                        self.tiles_visited += 1;
                    }
                    let cx = -half + (x as f32 + 0.5) * tile_world;
                    let cz = -half + (y as f32 + 0.5) * tile_world;
                    let dx = cx - camera[0];
                    let dz = cz - camera[1];
                    let dist = (dx * dx + dz * dz).sqrt();
                    if dist <= reach {
                        found.push((
                            dist,
                            TileKey {
                                texture: scan_key.slot,
                                mip: mip as u8,
                                x,
                                y,
                            },
                        ));
                    }
                }
            }
        }
        found
    }

    /// Drains finished loads into the cache and evicts the least-recently-used tiles until the
    /// budget is met. Does no file IO — safe to call every frame on the render thread.
    ///
    /// First it cancels the queued loads that none of the [`Self::request`] calls since the
    /// previous tick wanted, for the textures those calls asked for: call `request` for every
    /// view of a frame, then `tick` once.
    pub fn tick(&mut self) {
        self.cancel_stale();
        while let Ok(response) = self.response_rx.try_recv() {
            self.accept(response);
        }
        self.evict_over_budget();
    }

    /// Blocks until every in-flight load has finished, then drains them like [`Self::tick`].
    ///
    /// For loading screens, offline renders and tests: a frame never calls it (it waits on
    /// file IO). Returns at once when nothing is pending, and stops waiting if the loader
    /// thread is gone.
    pub fn flush(&mut self) {
        let _span =
            tracing::info_span!(target: "regia_terrain", "tile_cache_flush", pending = self.pending.len())
                .entered();
        while !self.pending.is_empty() {
            match self.response_rx.recv() {
                Ok(response) => self.accept(response),
                Err(_) => break,
            }
        }
        self.tick();
    }

    /// Cancels the queued loads of the textures requested since the last tick that none of
    /// those requests wanted (a texture's tail is always wanted), then starts a new round.
    fn cancel_stale(&mut self) {
        if !self.round_slots.is_empty() {
            let stale: Vec<TileKey> = self
                .pending
                .iter()
                .filter(|key| {
                    self.round_slots.contains(&key.texture)
                        && **key != tail_key(key.texture)
                        && !self.round_wanted.contains(*key)
                })
                .copied()
                .collect();
            if !stale.is_empty()
                && let Ok(mut cancelled) = self.cancelled.lock()
            {
                cancelled.extend(stale);
            }
        }
        self.round_wanted.clear();
        self.round_slots.clear();
    }

    /// Queued loads marked cancelled and not yet answered.
    #[cfg(test)]
    fn cancelled_len(&self) -> usize {
        self.cancelled.lock().map_or(0, |c| c.len())
    }

    /// Whether `key` is marked cancelled.
    #[cfg(test)]
    fn cancelled_contains(&self, key: TileKey) -> bool {
        self.cancelled.lock().is_ok_and(|c| c.contains(&key))
    }

    /// Takes one finished load into the cache (or records its failure).
    fn accept(&mut self, response: LoadResponse) {
        self.pending.remove(&response.key);
        // The load is answered: a cancel that came too late for the worker (it had already read
        // the request) ends here, so the shared set holds only keys still queued.
        if let Ok(mut cancelled) = self.cancelled.lock() {
            cancelled.remove(&response.key);
        }
        let Some(result) = response.result else {
            // Cancelled before it was read: nothing arrived.
            return;
        };
        match result {
            Ok(bytes) => {
                self.failed.remove(&response.key);
                self.evicted_wanted.remove(&response.key);
                let size = bytes.len() as u64;
                if size > self.budget && self.pinned.contains(&response.key) {
                    // A pinned entry larger than the whole budget could never be evicted.
                    warn!(
                        target: "regia_terrain",
                        key = ?response.key,
                        size,
                        budget = self.budget,
                        "pinned tail exceeds the cache budget; not caching it"
                    );
                    self.oversized.insert(response.key);
                    return;
                }
                if let Some(old) = self.entries.insert(response.key, Entry { bytes, size }) {
                    // Already resident (a reload): replace it in place.
                    self.current = self.current.saturating_sub(old.size);
                    self.touch_lru(response.key);
                } else {
                    self.lru.push_back(response.key);
                }
                self.current += size;
            }
            Err(e) => {
                let attempts = self.failed.get(&response.key).map_or(0, |f| f.attempts) + 1;
                let wait = 1u64 << attempts.min(MAX_TILE_RETRY_INTERVAL.ilog2());
                self.failed.insert(
                    response.key,
                    Failure {
                        attempts,
                        retry_at: self.requests + wait.min(MAX_TILE_RETRY_INTERVAL),
                    },
                );
                if self.failed.len() > MAX_FAILED_TILES {
                    // Forget the failure that may be retried soonest (ties: the smaller key), so
                    // the table stays bounded and the newest failures keep their wait.
                    let soonest = self
                        .failed
                        .iter()
                        .min_by(|a, b| a.1.retry_at.cmp(&b.1.retry_at).then(a.0.cmp(b.0)))
                        .map(|(key, _)| *key);
                    if let Some(key) = soonest {
                        self.failed.remove(&key);
                    }
                }
                debug!(
                    target: "regia_terrain",
                    key = ?response.key,
                    attempts,
                    error = %e,
                    "tile load failed"
                );
            }
        }
    }

    /// Evicts non-pinned tiles until the budget is met: least-recently-used first among the tiles
    /// the last requests did not want, then the wanted tile farthest from its camera.
    fn evict_over_budget(&mut self) {
        // Evict the least-recently-used non-pinned tile while over budget.
        while self.current > self.budget {
            // Least recently used among the tiles no request wants; when every candidate is
            // wanted, the farthest from its camera (ties: the larger key).
            let unwanted = self
                .lru
                .iter()
                .position(|k| !self.pinned.contains(k) && !self.wanted.contains_key(k));
            let farthest = || {
                self.lru
                    .iter()
                    .enumerate()
                    .filter(|&(_, k)| !self.pinned.contains(k))
                    .max_by(|&(_, a), &(_, b)| {
                        let da = self.wanted.get(a).copied().unwrap_or(f32::INFINITY);
                        let db = self.wanted.get(b).copied().unwrap_or(f32::INFINITY);
                        da.total_cmp(&db).then(a.cmp(b))
                    })
                    .map(|(pos, _)| pos)
            };
            let Some(pos) = unwanted.or_else(farthest) else {
                break;
            };
            if let Some(key) = self.lru.get(pos)
                && self.wanted.contains_key(key)
            {
                self.evicted_wanted.insert(*key);
            }
            let key = self.lru.remove(pos).expect("position came from the queue");
            if let Some(entry) = self.entries.remove(&key) {
                self.current = self.current.saturating_sub(entry.size);
            }
        }
    }

    /// Marks `key` most-recently-used (moves it to the back of the LRU queue).
    fn touch_lru(&mut self, key: TileKey) {
        if let Some(pos) = self.lru.iter().position(|&k| k == key) {
            self.lru.remove(pos);
            self.lru.push_back(key);
        }
    }
}

impl Drop for TileCache {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        drop(self.request_tx.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn load_keys_round_trip_and_residency_reports_into_load_sets() {
        let key = TileKey {
            texture: 2,
            mip: 1,
            x: 7,
            y: 9,
        };
        assert_eq!(key.load_key(), "2:1:7:9");
        assert_eq!(TileKey::parse_load_key("2:1:7:9"), Some(key));
        assert_eq!(TileKey::parse_load_key("2:1:7"), None);
        assert_eq!(TileKey::parse_load_key("2:1:7:9:0"), None);
        let mut cache = TileCache::new(
            1 << 20,
            SlowSource {
                delay: std::time::Duration::ZERO,
            },
        );
        cache.register_texture(0, texture());
        let tail = tail_key(0);
        let sets = regia_asset::LoadSets::default();
        sets.define(regia_asset::LoadSet::new("map").with(
            regia_asset::LoadKind::TerrainTile,
            tail.load_key(),
            0,
            64,
        ));
        assert_eq!(cache.report_residency(&sets), 0);
        assert!(!sets.is_resident("map"));
        cache.request(0, [0.0, 0.0], 128.0, 100.0, 1);
        assert!(wait_for(&mut cache, |c| c.is_resident(tail)), "tail loaded");
        assert_eq!(cache.report_residency(&sets), 1);
        assert!(sets.is_resident("map"));
    }

    use super::*;

    /// A tile source that sleeps `delay` per load (to prove the render thread never blocks on IO).
    struct SlowSource {
        delay: std::time::Duration,
    }

    impl TileSource for SlowSource {
        fn load(&self, _texture: &TiledTexture, key: TileKey) -> Result<PakPayload, StreamError> {
            std::thread::sleep(self.delay);
            Ok(vec![key.texture; 64].into())
        }
    }

    fn texture() -> TiledTexture {
        TiledTexture::new("macro_albedo", 128, 32, TileFormat::Bc7Srgb)
    }

    fn key(mip: u8, x: u32, y: u32) -> TileKey {
        TileKey {
            texture: 0,
            mip,
            x,
            y,
        }
    }

    /// Fills a memory source with a `byte_len`-byte tile at every tiled mip.
    fn fill_source(src: &mut MemoryTileSource, t: &TiledTexture, byte_len: usize) {
        for mip in 0..t.tail_start_mip() {
            let per_side = t.tiles_per_side(mip);
            for y in 0..per_side {
                for x in 0..per_side {
                    src.insert(
                        TileKey {
                            texture: 0,
                            mip: mip as u8,
                            x,
                            y,
                        },
                        vec![9u8; byte_len],
                    );
                }
            }
        }
    }

    /// Ticks and briefly sleeps until `pred` holds or the timeout elapses (loads are async).
    fn wait_for<F: Fn(&TileCache) -> bool>(cache: &mut TileCache, pred: F) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            cache.tick();
            if pred(cache) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        cache.tick();
        pred(cache)
    }

    #[test]
    fn tile_grid_layout_is_correct() {
        let t = texture();
        // 128 source, 32 tile -> mips 128,64,32 tiled; 16,8,4 tail.
        assert_eq!(t.mip_count, 8); // 128..1
        assert_eq!(t.tail_mips, 3); // 16,8,4
        assert_eq!(t.tail_start_mip(), 3);
        assert_eq!(t.tiles_per_side(0), 4);
        assert_eq!(t.tiles_per_side(1), 2);
        assert_eq!(t.tiles_per_side(2), 1);
        assert_eq!(t.tiles_per_side(3), 0);
        assert_eq!(t.tile_count(0), 16);
    }

    #[test]
    fn memory_source_round_trips_and_reports_missing() {
        let mut src = MemoryTileSource::new();
        src.insert(key(0, 1, 2), vec![1, 2, 3]);
        let t = texture();
        assert_eq!(src.load(&t, key(0, 1, 2)).expect("load"), vec![1, 2, 3]);
        assert!(matches!(
            src.load(&t, key(0, 3, 3)),
            Err(StreamError::NotFound { .. })
        ));
    }

    #[test]
    fn load_happens_off_the_render_thread() {
        let mut cache = TileCache::new(
            1 << 20,
            SlowSource {
                delay: std::time::Duration::from_millis(100),
            },
        );
        cache.register_texture(0, texture());

        cache.request(0, [0.0, 0.0], 128.0, 100.0, 1);
        // The render thread's tick must return immediately while the slow load is still in flight.
        let start = std::time::Instant::now();
        cache.tick();
        assert!(
            start.elapsed() < std::time::Duration::from_millis(20),
            "tick blocked on IO: {:?}",
            start.elapsed()
        );
        assert_eq!(cache.resident_count(), 0, "tile must not be resident yet");

        // After the worker finishes, the tile is resident.
        assert!(wait_for(&mut cache, |c| c.resident_count() == 1));
    }

    #[test]
    fn nearest_tiles_resident_first() {
        let t = texture();
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 2);
        let mut cache = TileCache::new(1 << 20, src);
        cache.register_texture(0, t);

        // Camera near the -X/-Z corner requests only the nearest tile first.
        let half = 128.0 * 0.5;
        cache.request(0, [-half + 4.0, -half + 4.0], 128.0, 8.0, 1);
        assert!(wait_for(&mut cache, |c| c.resident_count() == 1));
        assert!(cache.is_resident(key(0, 0, 0)));
        assert_eq!(cache.resident_count(), 1);
    }

    #[test]
    fn budget_is_respected_under_a_camera_sweep() {
        let t = texture();
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 256); // 256 bytes per tile
        // Budget holds only two tiles (512 bytes).
        let mut cache = TileCache::new(512, src);
        cache.register_texture(0, t);

        let half = 128.0 * 0.5;
        for step in 0..20 {
            let cx = -half + (step as f32 * 6.0);
            cache.request(0, [cx, 0.0], 128.0, 32.0, 16);
            // Let in-flight loads settle so eviction actually engages.
            wait_for(&mut cache, |c| c.pending_count() == 0);
            assert!(
                cache.current_bytes() <= cache.budget(),
                "budget exceeded at step {step}: {} > {}",
                cache.current_bytes(),
                cache.budget()
            );
        }
    }

    /// Audit wave 6 (stream H2): one drain of three nearest-first loads into a two-tile budget
    /// evicted the first completion, the tile under the camera. The farthest wanted tile goes
    /// now, and the nearest stays resident over repeated requests from the same camera.
    #[test]
    fn a_drain_over_budget_keeps_the_tile_under_the_camera() {
        let t = texture();
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 256);
        let mut cache = TileCache::new(512, src);
        cache.register_texture(0, t);
        // The centre of mip-0 tile (0, 0); nearest first: that tile, the mip-1 tile over it,
        // then mip-0 tile (0, 1) at 32 m (the smaller key of the two 32 m tiles).
        let camera = [-48.0, -48.0];
        assert_eq!(cache.request(0, camera, 128.0, 0.0, 3), 3);
        cache.flush();
        assert!(cache.current_bytes() <= cache.budget());
        assert!(cache.is_resident(key(0, 0, 0)), "the tile under the camera stays");
        assert!(cache.is_resident(key(1, 0, 0)));
        assert!(!cache.is_resident(key(0, 0, 1)), "the farthest wanted tile goes");
        for round in 0..4 {
            // The two remaining wanted tiles would be evicted again: only the never-loaded ones
            // are asked for, once.
            let enqueued = cache.request(0, camera, 128.0, 0.0, 3);
            assert_eq!(enqueued, if round == 0 { 2 } else { 0 }, "round {round}");
            cache.flush();
            assert!(cache.current_bytes() <= cache.budget());
            assert!(cache.is_resident(key(0, 0, 0)), "round {round}");
            assert!(cache.is_resident(key(1, 0, 0)), "round {round}");
        }
        // The camera moves onto the evicted tile: it is nearer than a resident one, so it loads
        // and the farthest resident tile goes instead.
        cache.request(0, [-48.0, -16.0], 128.0, 0.0, 3);
        cache.flush();
        assert!(
            cache.is_resident(key(0, 0, 1)),
            "the tile under the moved camera loads"
        );
        assert!(cache.is_resident(key(1, 0, 0)));
        assert!(!cache.is_resident(key(0, 0, 0)), "the now farthest tile went");
        assert!(cache.current_bytes() <= cache.budget());
    }

    #[test]
    fn eviction_removes_least_recently_used() {
        // Populate three resident tiles directly (budget holds two), then tick to evict.
        let mut cache = TileCache::new(512, MemoryTileSource::new());
        let a = key(0, 0, 0);
        let b = key(0, 1, 0);
        let c = key(0, 2, 0);
        for (k, i) in [(a, 0u8), (b, 1), (c, 2)] {
            cache.entries.insert(
                k,
                Entry {
                    bytes: vec![i; 256].into(),
                    size: 256,
                },
            );
            cache.lru.push_back(k);
        }
        cache.current = 768; // three tiles, over the 512-byte budget

        // Touch A (moves it to the back of the LRU queue): order becomes [b, c, a].
        cache.residency(a);

        // Eviction drains the LRU front (b) until the budget is met.
        cache.tick();
        assert!(cache.current_bytes() <= 512);
        assert!(cache.is_resident(a), "touched tile must survive eviction");
        assert!(!cache.is_resident(b), "least-recently-used tile must be evicted");
        assert!(cache.is_resident(c));
    }

    #[test]
    fn tile_asset_path_matches_the_documented_scheme() {
        assert_eq!(
            tile_asset_path("macro_albedo", 2, 3, 4),
            "terrain/macro_albedo/mip2/tile_3_4"
        );
        let map = AssetId::sub_asset(AssetId::NIL, "test-map");
        assert_eq!(
            tile_asset_id(map, "macro_albedo", 2, 3, 4),
            tile_asset_id(map, "macro_albedo", 2, 3, 4)
        );
        assert_ne!(
            tile_asset_id(map, "macro_albedo", 2, 3, 4),
            tile_asset_id(map, "macro_albedo", 2, 3, 5)
        );
    }

    /// Builds a minimal valid KTX2 (one 4×4 BC7 sRGB block, one level) for round-trip tests. Mirrors
    /// the cooker's `ktx2_write` layout so `regia_asset::load_texture_bytes` accepts it.
    fn minimal_bc7_ktx2() -> Vec<u8> {
        let format = ktx2::Format::BC7_SRGB_BLOCK;
        let (basic, type_size) = ktx2::dfd::Basic::from_format(format).expect("dfd");
        let dfd_body = ktx2::dfd::Block::Basic(basic).to_vec();
        let dfd_total_len = 4u32 + dfd_body.len() as u32;
        let level_bytes: Vec<u8> = vec![0u8; 16]; // one 4x4 block
        let level_count = 1u32;
        let dfd_offset = ktx2::Header::LENGTH + ktx2::LevelIndex::LENGTH;
        let kvd_offset = dfd_offset + dfd_total_len as usize;
        let data_offset = kvd_offset;
        let header = ktx2::Header {
            format: Some(format),
            type_size,
            pixel_width: 4,
            pixel_height: 4,
            pixel_depth: 0,
            layer_count: 0,
            face_count: 1,
            level_count,
            supercompression_scheme: None,
            index: ktx2::Index {
                dfd_byte_offset: dfd_offset as u32,
                dfd_byte_length: dfd_total_len,
                kvd_byte_offset: kvd_offset as u32,
                kvd_byte_length: 0,
                sgd_byte_offset: 0,
                sgd_byte_length: 0,
            },
        };
        let mut out = Vec::new();
        out.extend_from_slice(&header.as_bytes());
        out.extend_from_slice(
            &ktx2::LevelIndex {
                byte_offset: data_offset as u64,
                byte_length: level_bytes.len() as u64,
                uncompressed_byte_length: level_bytes.len() as u64,
            }
            .as_bytes(),
        );
        out.extend_from_slice(&dfd_total_len.to_le_bytes());
        out.extend_from_slice(&dfd_body);
        out.extend_from_slice(&level_bytes);
        out
    }

    /// Writes a `CookedTexture` `.fckd` file at `id` (as the cooker does) and returns its bytes.
    fn write_texture_fckd(dir: &Path, id: AssetId, ktx2: Vec<u8>) -> Vec<u8> {
        let cooked = regia_asset::CookedTexture {
            width: 4,
            height: 4,
            usage: regia_asset::TextureCookUsage::ColorSrgb,
            ktx2: ktx2.into(),
        };
        let payload = cooked.to_postcard().expect("postcard");
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_asset::AssetTypeTag::texture().0,
            asset_id: id,
            importer_version: 1_000_000,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        let file = regia_asset::encode_cooked_file(&header, &payload).expect("encode");
        std::fs::write(dir.join(regia_asset::cooked_file_name(id)), &file).expect("write");
        file
    }

    /// A pak of one entry stored uncompressed, with a non-zero content hash (only `pak_verify`
    /// checks its value; opening does not).
    fn hashed_pak(id: AssetId, payload: &[u8]) -> Vec<u8> {
        let offset = regia_asset::FPAK_BLOB_ALIGNMENT;
        let mut bytes = vec![0u8; offset as usize];
        bytes.extend_from_slice(payload);
        let toc_offset = bytes.len() as u64;
        let toc = regia_asset::PakToc {
            entries: vec![regia_asset::PakEntry {
                id,
                type_tag: regia_core::id::TypePathHash(0),
                offset,
                size: payload.len() as u64,
                compressed_size: payload.len() as u64,
                block_table_offset: 0,
            }],
            manifest: Vec::new(),
            dependencies: Vec::new(),
        };
        let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
        bytes.extend_from_slice(&toc_bytes);
        let header = regia_asset::encode_pak_header(
            regia_asset::FPAK_VERSION,
            0,
            toc_offset,
            toc_bytes.len() as u64,
            &[1u8; 32],
        );
        bytes[..header.len()].copy_from_slice(&header);
        bytes
    }

    /// Regression (unsafe audit 2026-10-21): the tile source opened its own paks with the
    /// zero-copy gate open, so on the first launch after an update it served views of a pak the
    /// asset server was still reading as copies (a pak cut in place then faulted through a
    /// resident tile). It now applies the server's rule: no views of an unverified pak.
    #[test]
    fn pak_source_copies_from_a_pak_this_build_has_not_verified() {
        let dir = regia_test_utils::unique_temp_dir("terrain-pak-gate");
        let content = dir.join("content");
        std::fs::create_dir_all(&content).expect("content dir");
        let map = AssetId::sub_asset(AssetId::NIL, "gate-map");
        let id = terrain_index_asset_id(map);
        let payload = vec![0x5Au8; 3000];
        std::fs::write(content.join("base.fpak"), hashed_pak(id, &payload)).expect("write pak");
        let options = regia_asset::PakVerifyOptions {
            record: Some(dir.join("cache").join(regia_asset::pak_verify::PAK_VERIFY_RECORD)),
            ..Default::default()
        };

        let unverified =
            PakTileSource::with_verify(map, Some(&content), dir.join("no-cache"), Some(&options));
        assert_eq!(unverified.paks.len(), 1);
        assert!(
            !unverified.paks[0].zero_copy(),
            "the gate of an unverified pak is closed"
        );
        let copy = unverified.read_bytes(id).expect("read");
        assert!(!copy.is_mapped(), "an unverified pak serves copies");
        assert_eq!(copy, payload);

        // With the check off (`REGIA_PAK_VERIFY=0`, or no cache dir) the pak serves views.
        let unchecked = PakTileSource::with_verify(map, Some(&content), dir.join("no-cache"), None);
        let view = unchecked.read_bytes(id).expect("read");
        assert!(view.is_mapped(), "an unchecked pak serves views of its mapping");
        assert_eq!(view, payload);
        drop((copy, view, unverified, unchecked));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pak_source_load_round_trips_a_cooked_texture_file() {
        let tmp = std::env::temp_dir().join(format!("regia_terrain_stream_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cache = regia_asset::cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");

        let map = AssetId::sub_asset(AssetId::NIL, "test-map");
        let source = PakTileSource::new(map, None, cache.clone());

        // Write real `.fckd` `CookedTexture` files at the tile and tail ids the cooker uses.
        let tile_id = tile_asset_id(map, "macro_albedo", 0, 1, 2);
        let ktx2 = minimal_bc7_ktx2();
        write_texture_fckd(&cache, tile_id, ktx2.clone());
        write_texture_fckd(&cache, tail_asset_id(map, "macro_albedo"), ktx2.clone());

        let texture = TiledTexture::new("macro_albedo", 128, 32, TileFormat::Bc7Srgb);

        // `load` decodes the `.fckd` and returns the KTX2 payload verbatim (tile and tail).
        assert_eq!(
            source
                .load(
                    &texture,
                    TileKey {
                        texture: 0,
                        mip: 0,
                        x: 1,
                        y: 2
                    }
                )
                .expect("tile load"),
            ktx2
        );
        assert_eq!(source.load(&texture, tail_key(0)).expect("tail load"), ktx2);

        // An unknown tile is an IO error.
        let missing = tile_asset_id(map, "macro_albedo", 0, 9, 9);
        assert!(source.read_bytes(missing).is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tail_ids_and_key_are_stable_and_distinct() {
        let map = AssetId::sub_asset(AssetId::NIL, "test-map");
        assert_eq!(tail_asset_path("macro_albedo"), "terrain/macro_albedo/tail");
        assert_eq!(
            tail_asset_id(map, "macro_albedo"),
            tail_asset_id(map, "macro_albedo")
        );
        assert_ne!(
            tail_asset_id(map, "macro_albedo"),
            tile_asset_id(map, "macro_albedo", 0, 0, 0)
        );
        assert_eq!(TAIL_MIP, u8::MAX);
        assert_eq!(
            tail_key(3),
            TileKey {
                texture: 3,
                mip: TAIL_MIP,
                x: 0,
                y: 0
            }
        );
    }

    #[test]
    fn tail_is_requested_and_pinned_against_eviction() {
        // 64x64 tiled at 32: 5 tiled tiles (mip0 2x2 + mip1 1x1), tail mips 16,8,4.
        let t = TiledTexture::new("macro_albedo", 64, 32, TileFormat::Bc7Srgb);
        let mut src = MemoryTileSource::new();
        src.insert(tail_key(0), vec![7u8; 16]);
        fill_source(&mut src, &t, 128); // 128 bytes per tiled tile
        // Budget 200: tail (16) + one tile (128) fit; a second tile would not.
        let mut cache = TileCache::new(200, src);
        cache.register_texture(0, t);

        // Request everything (large radius) so the tail is enqueued alongside the tiles.
        cache.request(0, [0.0, 0.0], 64.0, 1000.0, 32);
        assert!(wait_for(&mut cache, |c| c.pending_count() == 0));

        // The tail must be resident and reachable via residency().
        assert!(
            cache.is_resident(tail_key(0)),
            "tail must be resident after request"
        );
        assert!(
            cache.residency(tail_key(0)).is_some(),
            "tail reachable via residency()"
        );

        // Over budget: non-pinned tiles are evicted, but the pinned tail survives.
        cache.tick();
        assert!(cache.is_resident(tail_key(0)), "tail must survive eviction");
        assert!(cache.current_bytes() <= cache.budget());
    }

    #[test]
    fn oversized_pinned_tail_is_dropped_and_not_rerequested() {
        let t = TiledTexture::new("macro_albedo", 64, 32, TileFormat::Bc7Srgb);
        let mut src = MemoryTileSource::new();
        src.insert(tail_key(0), vec![7u8; 300]);
        let mut cache = TileCache::new(100, src);
        cache.register_texture(0, t);
        cache.request(0, [0.0, 0.0], 64.0, 1000.0, 0);
        assert!(wait_for(&mut cache, |c| c.pending_count() == 0));
        assert!(!cache.is_resident(tail_key(0)));
        assert!(cache.current_bytes() <= cache.budget());
        cache.request(0, [0.0, 0.0], 64.0, 1000.0, 0);
        assert_eq!(cache.pending_count(), 0);
    }

    /// Two pinned tails that each fit are both kept, so the resident total can pass the budget.
    #[test]
    fn two_pinned_tails_can_exceed_the_budget() {
        let a = TiledTexture::new("macro_albedo", 64, 32, TileFormat::Bc7Srgb);
        let b = TiledTexture::new("macro_normal", 64, 32, TileFormat::Bc5);
        let mut src = MemoryTileSource::new();
        src.insert(tail_key(0), vec![1u8; 100]);
        src.insert(tail_key(1), vec![2u8; 100]);
        let mut cache = TileCache::new(150, src);
        cache.register_texture(0, a);
        cache.register_texture(1, b);
        cache.request(0, [0.0, 0.0], 64.0, 0.0, 0);
        cache.request(1, [0.0, 0.0], 64.0, 0.0, 0);
        cache.flush();
        assert!(cache.is_resident(tail_key(0)) && cache.is_resident(tail_key(1)));
        assert_eq!(cache.current_bytes(), 200);
        assert!(cache.current_bytes() > cache.budget());
    }

    #[test]
    fn request_skips_resident_and_pending_tiles() {
        // A 64x64 texture tiled at 32 has five tiled tiles: mip 0 (2x2) + mip 1 (1x1).
        let t = TiledTexture::new("macro_albedo", 64, 32, TileFormat::Bc7Srgb);
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 64);
        let mut cache = TileCache::new(1 << 20, src);
        cache.register_texture(0, t);
        let enqueued = cache.request(0, [0.0, 0.0], 64.0, 1000.0, 8);
        assert_eq!(enqueued, 5);
        // Re-request: all five are pending, none new.
        let again = cache.request(0, [0.0, 0.0], 64.0, 1000.0, 8);
        assert_eq!(again, 0);
        assert!(wait_for(&mut cache, |c| c.pending_count() == 0));
        // Now resident, so still nothing new.
        let again2 = cache.request(0, [0.0, 0.0], 64.0, 1000.0, 8);
        assert_eq!(again2, 0);
    }

    /// `loads_issued` counts every load handed to the loader, tail included, and never goes
    /// down when the loads finish: after a drain `pending_count` is 0 again, the count is not.
    #[test]
    fn loads_issued_counts_requests_and_survives_the_drain() {
        let t = TiledTexture::new("macro_albedo", 64, 32, TileFormat::Bc7Srgb);
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 64);
        src.insert(tail_key(0), vec![1u8; 16]);
        let mut cache = TileCache::new(1 << 20, src);
        cache.register_texture(0, t);
        assert_eq!(cache.loads_issued(), 0);
        assert_eq!(cache.request(0, [0.0, 0.0], 64.0, 1000.0, 8), 5);
        assert_eq!(cache.loads_issued(), 6, "five tiles and the tail");
        // Already pending: nothing new is issued.
        assert_eq!(cache.request(0, [0.0, 0.0], 64.0, 1000.0, 8), 0);
        assert_eq!(cache.loads_issued(), 6);
        cache.flush();
        assert_eq!(cache.pending_count(), 0);
        assert_eq!(cache.resident_count(), 6);
        assert_eq!(cache.loads_issued(), 6, "a finished load still counts as issued");
        // Resident: still nothing new.
        assert_eq!(cache.request(0, [0.0, 0.0], 64.0, 1000.0, 8), 0);
        assert_eq!(cache.loads_issued(), 6);
    }

    #[test]
    fn terrain_index_round_trips_and_ids_are_stable() {
        let map = AssetId::sub_asset(AssetId::NIL, "test-map");
        let index = TerrainIndex {
            version: TERRAIN_INDEX_FORMAT_VERSION,
            map_id: map,
            slots: vec![TerrainSlotIndex {
                name: "macro_albedo".into(),
                width: 512,
                height: 512,
                tile_size: 256,
                format: TileFormat::Bc7Srgb,
                mip_count: 10,
                tail_mips: 6,
                tile_count: 5,
                bytes: 12_345,
            }],
            heightfield: Some(TerrainHeightfieldIndex {
                width: 512,
                height: 512,
                tile_size: 256,
                tiles_x: 2,
                tiles_y: 2,
                bytes: 999,
            }),
            layers: vec!["ground".into()],
            world_size_m: Some(1000.0),
            world_origin: Some([0.0, 0.0, 0.0]),
            source_mapping: None,
            source_spacing_z_m: None,
            source_valid_mask: None,
            heights: None,
            holes: None,
            material: None,
        };

        let bytes = index.to_postcard().expect("encode");
        let back = TerrainIndex::from_postcard(&bytes).expect("decode");
        assert_eq!(index, back);
        assert_eq!(index.slot_index("macro_albedo"), Some(0));
        assert_eq!(index.slot_index("missing"), None);
        assert_eq!(index.texture(0).expect("slot 0").name, "macro_albedo");
        assert_eq!(index.texture(1), None);
        assert_eq!(index.total_bytes(), 12_345 + 999);

        // Deterministic ids, stable and distinct.
        assert_eq!(terrain_index_asset_path(), "terrain/index");
        assert_eq!(heightfield_asset_path(), "terrain/heightfield");
        assert_eq!(terrain_index_asset_id(map), terrain_index_asset_id(map));
        assert_eq!(heightfield_asset_id(map), heightfield_asset_id(map));
        assert_ne!(terrain_index_asset_id(map), heightfield_asset_id(map));
        assert_ne!(
            terrain_index_asset_id(map),
            tile_asset_id(map, "macro_albedo", 0, 0, 0)
        );
    }

    /// The v1 shape (v0.5.0) as a test-only writer: the field list of `TerrainIndexV1`.
    #[derive(serde::Serialize)]
    struct V1Writer {
        version: u32,
        map_id: AssetId,
        slots: Vec<TerrainSlotIndex>,
        heightfield: Option<TerrainHeightfieldIndex>,
        layers: Vec<String>,
        world_size_m: Option<f32>,
        world_origin: Option<[f32; 3]>,
    }

    fn cooked_index(payload: &[u8]) -> Vec<u8> {
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of(TERRAIN_INDEX_TYPE_PATH),
            asset_id: AssetId::NIL,
            importer_version: 1,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        regia_asset::encode_cooked_file(&header, payload).expect("wrap")
    }

    /// A v0.5.0 (v1) index loads as v2 with no heights, holes or material (ADR-8024), and a
    /// version past 2 is refused by its version before the body is read.
    #[test]
    fn a_v1_terrain_index_migrates_to_v2() {
        let map = AssetId::sub_asset(AssetId::NIL, "v1-map");
        let v1 = V1Writer {
            version: 1,
            map_id: map,
            slots: Vec::new(),
            heightfield: Some(TerrainHeightfieldIndex {
                width: 300,
                height: 300,
                tile_size: 256,
                tiles_x: 2,
                tiles_y: 2,
                bytes: 77,
            }),
            layers: vec!["grass".into(), "rock".into()],
            world_size_m: Some(1280.0),
            world_origin: Some([1.0, 2.0, 3.0]),
        };
        let payload = postcard::to_allocvec(&v1).expect("v1");
        let index = load_terrain_index_bytes(&cooked_index(&payload)).expect("v1 loads");
        assert_eq!(index.version, TERRAIN_INDEX_FORMAT_VERSION);
        assert_eq!(index.map_id, map);
        assert_eq!(index.layers, v1.layers);
        assert_eq!(index.heightfield, v1.heightfield);
        assert_eq!(index.world_size_m, Some(1280.0));
        assert_eq!(index.world_origin, Some([1.0, 2.0, 3.0]));
        assert!(index.heights.is_none() && index.holes.is_none() && index.material.is_none());
        // Idempotent: the migrated index written again reads back equal.
        let again =
            load_terrain_index_bytes(&cooked_index(&index.to_postcard().expect("v2"))).expect("v2 loads");
        assert_eq!(again, index);
        // Current + 1 is refused by its version even though its body is v1-shaped.
        let future = V1Writer {
            version: TERRAIN_INDEX_FORMAT_VERSION + 1,
            ..v1
        };
        let bytes = cooked_index(&postcard::to_allocvec(&future).expect("future"));
        assert!(matches!(
            load_terrain_index_bytes(&bytes),
            Err(TerrainIndexError::BadVersion { found, .. }) if found == TERRAIN_INDEX_FORMAT_VERSION + 1
        ));
    }

    /// Version 2 entries that would make a reader allocate without bound or read past the R16
    /// grid are refused at load.
    #[test]
    fn bad_v2_entries_are_refused() {
        let hf = TerrainHeightfieldIndex {
            width: 64,
            height: 64,
            tile_size: 256,
            tiles_x: 1,
            tiles_y: 1,
            bytes: 1,
        };
        let heights = TerrainHeightsIndex {
            width: 64,
            height: 64,
            encoding: TerrainHeightsEncoding::Residual,
            height_scale: 10.0,
            height_offset: 0.0,
            bytes: 1,
        };
        let material = TerrainMaterialIndex {
            params: crate::MaterialParams::default().into(),
            macro_tiles: 2,
            layer_albedo: 4,
            layer_normal: 4,
            layer_height: 4,
            layer_orm: 0,
            macro_albedo_tiles: 4,
            macro_normal_tiles: 4,
            macro_coarse_albedo: true,
            macro_coarse_normal: false,
            bytes: 1,
        };
        let base = TerrainIndex {
            version: TERRAIN_INDEX_FORMAT_VERSION,
            map_id: AssetId::NIL,
            slots: Vec::new(),
            heightfield: Some(hf.clone()),
            layers: Vec::new(),
            world_size_m: Some(64.0),
            world_origin: None,
            heights: Some(heights.clone()),
            holes: Some(TerrainHolesIndex { bytes: 1 }),
            material: Some(material.clone()),
            source_mapping: None,
            source_spacing_z_m: None,
            source_valid_mask: None,
        };
        let load = |index: &TerrainIndex| {
            load_terrain_index_bytes(&cooked_index(&index.to_postcard().expect("enc")))
        };
        let back = load(&base).expect("good v2");
        assert_eq!(back, base);
        assert_eq!(
            crate::MaterialParams::from(material.params).macro_strength,
            crate::MaterialParams::default().macro_strength
        );
        let refused = |index: TerrainIndex| {
            assert!(
                matches!(
                    load(&index),
                    Err(TerrainIndexError::BadHeights(_) | TerrainIndexError::BadMaterial(_))
                ),
                "{index:?}"
            );
        };
        refused(TerrainIndex {
            heights: Some(TerrainHeightsIndex {
                width: 63,
                ..heights.clone()
            }),
            ..base.clone()
        });
        refused(TerrainIndex {
            heightfield: None,
            ..base.clone()
        });
        refused(TerrainIndex {
            heights: Some(TerrainHeightsIndex {
                height_scale: f32::NAN,
                ..heights.clone()
            }),
            ..base.clone()
        });
        refused(TerrainIndex {
            material: Some(TerrainMaterialIndex {
                layer_albedo: 9,
                ..material.clone()
            }),
            ..base.clone()
        });
        refused(TerrainIndex {
            material: Some(TerrainMaterialIndex {
                macro_albedo_tiles: 5,
                ..material.clone()
            }),
            ..base.clone()
        });
        refused(TerrainIndex {
            material: Some(TerrainMaterialIndex {
                macro_tiles: 17,
                ..material
            }),
            ..base.clone()
        });
        // Raw heights need no R16 grid.
        let raw = TerrainIndex {
            heightfield: None,
            heights: Some(TerrainHeightsIndex {
                encoding: TerrainHeightsEncoding::Raw,
                ..heights
            }),
            ..base
        };
        assert!(load(&raw).is_ok());
    }

    #[test]
    fn stitch_drops_cooker_border_padding() {
        // 3×2 map, tile edge 2. The right tile's second column and the missing
        // second row are the cooker's border repeat and must not enter the map.
        let tile = |x: u32, samples: [u16; 4]| CookedHeightfieldTile {
            x,
            y: 0,
            min: *samples.iter().min().unwrap_or(&0),
            max: *samples.iter().max().unwrap_or(&0),
            samples: samples.to_vec(),
        };
        let tiled = CookedTiledHeightfield {
            version: TILED_HEIGHTFIELD_FORMAT_VERSION,
            width: 3,
            height: 2,
            tile_size: 2,
            tiles: vec![tile(0, [1, 2, 3, 4]), tile(1, [5, 5, 6, 6])],
        };
        let map = stitch_tiled_heightfield(&tiled).expect("stitch");
        assert_eq!(map.samples, vec![1, 2, 5, 3, 4, 6]);
    }

    #[test]
    fn stitch_rejects_huge_map_without_tiles() {
        let tiled = CookedTiledHeightfield {
            version: TILED_HEIGHTFIELD_FORMAT_VERSION,
            width: 50_000,
            height: 50_000,
            tile_size: 256,
            tiles: Vec::new(),
        };
        assert!(matches!(
            stitch_tiled_heightfield(&tiled),
            Err(TerrainIndexError::BadHeightfield(_))
        ));
    }

    #[test]
    fn index_slot_geometry_is_validated() {
        let slot = |width: u32, height: u32, tile_size: u32| TerrainSlotIndex {
            name: "s".into(),
            width,
            height,
            tile_size,
            format: TileFormat::Bc7Srgb,
            mip_count: 1,
            tail_mips: 0,
            tile_count: 0,
            bytes: 0,
        };
        let index = |s: TerrainSlotIndex| TerrainIndex {
            version: TERRAIN_INDEX_FORMAT_VERSION,
            map_id: AssetId::NIL,
            slots: vec![s],
            heightfield: None,
            layers: Vec::new(),
            world_size_m: None,
            world_origin: None,
            source_mapping: None,
            source_spacing_z_m: None,
            source_valid_mask: None,
            heights: None,
            holes: None,
            material: None,
        };
        assert!(validate_index_slots(&index(slot(512, 512, 256))).is_ok());
        for bad in [
            slot(512, 512, 0),
            slot(128, 128, 256),
            slot(512, 512, 100),
            slot(512, 256, 256),
            slot(1 << 20, 1 << 20, 1),
            slot(1 << 20, 1 << 20, 1 << 10),
        ] {
            assert!(matches!(
                validate_index_slots(&index(bad)),
                Err(TerrainIndexError::BadSlot { .. })
            ));
        }
    }

    fn cooked(
        width: u32,
        height: u32,
        tile_size: u32,
        tiles: Vec<CookedHeightfieldTile>,
    ) -> CookedTiledHeightfield {
        CookedTiledHeightfield {
            version: TILED_HEIGHTFIELD_FORMAT_VERSION,
            width,
            height,
            tile_size,
            tiles,
        }
    }

    fn flat_tile(x: u32, y: u32, tile_size: u32, value: u16) -> CookedHeightfieldTile {
        CookedHeightfieldTile {
            x,
            y,
            min: value,
            max: value,
            samples: vec![value; (tile_size * tile_size) as usize],
        }
    }

    #[test]
    fn stitch_rejects_hostile_header_before_allocating() {
        let hostile = |tiles: Vec<CookedHeightfieldTile>| CookedTiledHeightfield {
            version: TILED_HEIGHTFIELD_FORMAT_VERSION,
            width: u32::MAX,
            height: u32::MAX,
            tile_size: u32::MAX,
            tiles,
        };
        // No tiles: a huge declared map cannot be covered.
        assert!(stitch_tiled_heightfield(&hostile(Vec::new())).is_err());
        // One tile whose declared edge covers the map but carries no samples.
        let empty = CookedHeightfieldTile {
            x: 0,
            y: 0,
            min: 0,
            max: 0,
            samples: Vec::new(),
        };
        assert!(stitch_tiled_heightfield(&hostile(vec![empty])).is_err());
    }

    #[test]
    fn stitch_rejects_bad_headers_and_missing_tiles() {
        let ok = cooked(2, 2, 2, vec![flat_tile(0, 0, 2, 7)]);
        assert_eq!(stitch_tiled_heightfield(&ok).expect("stitch").samples, vec![7; 4]);
        let mut bad_version = ok.clone();
        bad_version.version = 99;
        assert!(stitch_tiled_heightfield(&bad_version).is_err());
        for (w, h, t) in [(1, 8, 2), (8, 1, 2), (0, 0, 2), (2, 2, 0)] {
            assert!(
                stitch_tiled_heightfield(&cooked(w, h, t, Vec::new())).is_err(),
                "{w}x{h} tile {t}"
            );
        }
        // One of four tiles missing: not covered.
        let gap = cooked(
            4,
            4,
            2,
            vec![
                flat_tile(0, 0, 2, 1),
                flat_tile(1, 0, 2, 1),
                flat_tile(0, 1, 2, 1),
            ],
        );
        assert!(stitch_tiled_heightfield(&gap).is_err());
        // A tile with the wrong sample count.
        let mut short = flat_tile(0, 0, 2, 1);
        short.samples.pop();
        assert!(stitch_tiled_heightfield(&cooked(2, 2, 2, vec![short])).is_err());
    }

    #[test]
    fn stitch_rejects_a_huge_header_without_allocating_it() {
        let huge = cooked(u32::MAX, u32::MAX, 2, vec![flat_tile(0, 0, 2, 1)]);
        assert!(stitch_tiled_heightfield(&huge).is_err());
        let huge_tile = cooked(2, 2, u32::MAX, Vec::new());
        assert!(stitch_tiled_heightfield(&huge_tile).is_err());
    }

    #[test]
    fn stitch_non_square_map_with_partial_tiles_on_both_edges() {
        // 5 x 3 map, tile edge 4: two tile columns, one row; the right tile and the bottom rows are padding.
        let mut left = flat_tile(0, 0, 4, 1);
        let mut right = flat_tile(1, 0, 4, 2);
        left.samples[3] = 10; // row 0, col 3
        right.samples[0] = 20; // row 0, map col 4
        let map = stitch_tiled_heightfield(&cooked(5, 3, 4, vec![left, right])).expect("stitch");
        assert_eq!((map.width, map.height), (5, 3));
        assert_eq!(&map.samples[0..5], &[1, 1, 1, 10, 20]);
        assert_eq!(&map.samples[5..], &[1, 1, 1, 1, 2, 1, 1, 1, 1, 2]);
    }

    #[test]
    fn tiled_texture_geometry_for_minimum_and_single_tile_sizes() {
        // The texture is exactly one tile: nothing above the tail.
        let one = TiledTexture::new("t", 256, 256, TileFormat::Bc4);
        assert_eq!(one.tail_start_mip(), 1);
        assert_eq!(one.tiles_per_side(0), 1);
        assert_eq!(one.tile_count(0), 1);
        assert_eq!(one.tiles_per_side(1), 0);
        assert_eq!(one.mip_count, 9);
        // Sizes below the 4x4 block size have no tail instead of underflowing.
        for size in [1u32, 2] {
            let tiny = TiledTexture::new("tiny", size, 1, TileFormat::Bc4);
            assert_eq!(tiny.tail_mips, 0, "size {size}");
        }
        let four = TiledTexture::new("four", 4, 4, TileFormat::Bc7Srgb);
        assert_eq!(four.tail_mips, 0);
        // 1024 at 256: tiled mips 0..=2 (4x4, 2x2, 1x1 tiles), tail from mip 3 down to 4x4.
        let t = TiledTexture::new("t", 1024, 256, TileFormat::Bc7Linear);
        assert_eq!(
            (t.tiles_per_side(0), t.tiles_per_side(1), t.tiles_per_side(2)),
            (4, 2, 1)
        );
        assert_eq!(t.tiles_per_side(3), 0);
        assert_eq!(t.tile_count(0), 16);
        assert_eq!(t.tail_start_mip(), 3);
        assert_eq!(t.tail_mips, 6);
        assert_eq!(t.tail_start_mip() + t.tail_mips, 9); // mips 3..=8, the 8th being 4x4
    }

    #[test]
    fn load_key_parsing_rejects_garbage_and_overflow() {
        for bad in [
            "",
            ":::",
            "a:b:c:d",
            "256:0:0:0",
            "0:256:0:0",
            "0:0:-1:0",
            "0:0:0:4294967296",
            "0:0:0: 1",
            "0:0:0",
        ] {
            assert_eq!(TileKey::parse_load_key(bad), None, "{bad:?}");
        }
        let max = TileKey {
            texture: u8::MAX,
            mip: u8::MAX,
            x: u32::MAX,
            y: u32::MAX,
        };
        assert_eq!(TileKey::parse_load_key(&max.load_key()), Some(max));
    }

    /// Loads asked of a [`CountingSource`], per key.
    type Counts = Arc<std::sync::Mutex<HashMap<TileKey, usize>>>;

    /// A memory source that counts the loads asked of it, per key.
    struct CountingSource {
        inner: MemoryTileSource,
        counts: Counts,
    }

    impl TileSource for CountingSource {
        fn load(&self, texture: &TiledTexture, key: TileKey) -> Result<PakPayload, StreamError> {
            if let Ok(mut counts) = self.counts.lock() {
                *counts.entry(key).or_insert(0) += 1;
            }
            self.inner.load(texture, key)
        }
    }

    fn counting(inner: MemoryTileSource) -> (CountingSource, Counts) {
        let counts = Arc::new(std::sync::Mutex::new(HashMap::new()));
        (
            CountingSource {
                inner,
                counts: Arc::clone(&counts),
            },
            counts,
        )
    }

    #[test]
    fn a_missing_nearest_tile_backs_off_and_does_not_starve_the_rest() {
        // Near the -X/-Z corner the wanted tiles are mip 0 (0,0), mip 1 (0,0) and mip 2 (0,0),
        // nearest first. Mip 0 (0,0) is missing; one request slot per call.
        let t = texture();
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 8);
        src.tiles.remove(&key(0, 0, 0));
        let (src, counts) = counting(src);
        let mut cache = TileCache::new(1 << 20, src);
        cache.register_texture(0, t);
        let camera = [-60.0, -60.0];
        for _ in 0..20 {
            cache.request(0, camera, 128.0, 8.0, 1);
            cache.flush();
        }
        assert!(cache.is_resident(key(1, 0, 0)), "the next tile loaded");
        assert!(cache.is_resident(key(2, 0, 0)), "and the one after");
        assert!(!cache.is_resident(key(0, 0, 0)));
        // The missing tile (and the missing tail) are held back, not re-read every call:
        // tries at calls 1, 3, 7 and 15 of 20.
        assert_eq!(cache.failed_count(), 2, "the tile and the tail");
        let tries = counts.lock().expect("counts").get(&key(0, 0, 0)).copied();
        assert_eq!(tries, Some(4));
        // Clearing the failures asks again on the next call.
        cache.clear_failed();
        assert_eq!(cache.failed_count(), 0);
        cache.request(0, camera, 128.0, 8.0, 1);
        cache.flush();
        let tries = counts.lock().expect("counts").get(&key(0, 0, 0)).copied();
        assert_eq!(tries, Some(5));
    }

    /// Audit wave 10c (stream M2): a long walk over sparse content fails thousands of distinct
    /// tiles; the failure table keeps a bounded number of them (4096) instead of growing with
    /// the world.
    #[test]
    fn the_failure_table_stays_bounded_over_many_distinct_missing_tiles() {
        // 128 x 128 tiles at mip 0 and none of them exist.
        let t = TiledTexture::new("sparse", 8192, 64, TileFormat::Bc7Srgb);
        let mut cache = TileCache::new(1 << 20, MemoryTileSource::new());
        cache.register_texture(0, t);
        let enqueued = cache.request(0, [0.0, 0.0], 8192.0, 1.0e6, 6000);
        assert_eq!(enqueued, 6000);
        cache.flush();
        assert_eq!(cache.resident_count(), 0);
        assert_eq!(cache.failed_count(), 4096, "the table is capped");
    }

    /// A tile source whose first load waits until the test releases it, counting every load.
    struct GatedSource {
        inner: MemoryTileSource,
        counts: Counts,
        gate: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl TileSource for GatedSource {
        fn load(&self, texture: &TiledTexture, key: TileKey) -> Result<PakPayload, StreamError> {
            let gate = self.gate.lock().ok().and_then(|mut gate| gate.take());
            if let Some(gate) = gate {
                let _ = gate.recv();
            }
            if let Ok(mut counts) = self.counts.lock() {
                *counts.entry(key).or_insert(0) += 1;
            }
            self.inner.load(texture, key)
        }
    }

    /// Audit wave 10c (stream M1): a camera that jumps away while loads are still queued
    /// cancels the queued tiles no request wants any more, so the new view's tiles are not
    /// stuck behind them and their bytes never reach the cache.
    #[test]
    fn queued_tiles_the_camera_left_behind_are_cancelled() {
        let t = texture();
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 8);
        src.insert(tail_key(0), vec![3; 4]);
        let counts: Counts = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let source = GatedSource {
            inner: src,
            counts: Arc::clone(&counts),
            gate: std::sync::Mutex::new(Some(gate)),
        };
        let mut cache = TileCache::new(1 << 20, source);
        cache.register_texture(0, t);
        // Near the -X/-Z corner: the four corner tiles of mip 0, three of mip 1 and mip 2.
        // One frame: request, then tick.
        let near_corner = cache.request(0, [-60.0, -60.0], 128.0, 40.0, 16);
        cache.tick();
        assert!(near_corner >= 8, "{near_corner}");
        let left_behind = [
            key(0, 0, 0),
            key(0, 1, 0),
            key(0, 0, 1),
            key(0, 1, 1),
            key(1, 0, 0),
            key(1, 1, 0),
            key(1, 0, 1),
        ];
        // The loader is held in its first load; next frame the camera is at the opposite corner.
        cache.request(0, [60.0, 60.0], 128.0, 8.0, 16);
        cache.tick();
        let _ = release.send(());
        cache.flush();
        cache.request(0, [60.0, 60.0], 128.0, 8.0, 16);
        cache.flush();
        let loaded_behind = {
            let counts = counts.lock().expect("counts");
            left_behind.iter().filter(|k| counts.contains_key(k)).count()
        };
        // At most the one load already running when the camera moved.
        assert!(
            loaded_behind <= 1,
            "{loaded_behind} tiles left behind were still loaded"
        );
        assert!(cache.is_resident(key(0, 3, 3)), "the new view's tile loaded");
        assert!(cache.is_resident(key(1, 1, 1)));
        assert_eq!(cache.pending_count(), 0);
    }

    /// A tile source whose first load reports its key and then waits until the test releases it.
    struct StartGatedSource {
        inner: MemoryTileSource,
        started: std::sync::Mutex<Option<std::sync::mpsc::Sender<TileKey>>>,
        gate: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl TileSource for StartGatedSource {
        fn load(&self, texture: &TiledTexture, key: TileKey) -> Result<PakPayload, StreamError> {
            if let Some(started) = self.started.lock().ok().and_then(|mut s| s.take()) {
                let _ = started.send(key);
                if let Some(gate) = self.gate.lock().ok().and_then(|mut g| g.take()) {
                    let _ = gate.recv();
                }
            }
            self.inner.load(texture, key)
        }
    }

    /// deep-zai-terrain-stream #9: a cancel that arrives after the worker already read the
    /// request is consumed when that load is answered; the shared set does not keep it forever.
    #[test]
    fn a_cancel_too_late_for_the_worker_does_not_stay_in_the_set() {
        let t = texture();
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 8);
        src.insert(tail_key(0), vec![3; 4]);
        let (started_tx, started_rx) = std::sync::mpsc::channel::<TileKey>();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let source = StartGatedSource {
            inner: src,
            started: std::sync::Mutex::new(Some(started_tx)),
            gate: std::sync::Mutex::new(Some(gate)),
        };
        let mut cache = TileCache::new(1 << 20, source);
        cache.register_texture(0, t);
        cache.request(0, [-60.0, -60.0], 128.0, 40.0, 16);
        cache.tick();
        // The worker holds the first request in its load: a cancel now is too late for it.
        let in_flight = started_rx.recv().expect("the worker started a load");
        cache.request(0, [60.0, 60.0], 128.0, 8.0, 16);
        cache.tick();
        assert!(
            cache.cancelled_contains(in_flight),
            "the camera left {in_flight:?} behind while it was being read"
        );
        let _ = release.send(());
        cache.flush();
        assert_eq!(cache.pending_count(), 0);
        assert_eq!(cache.cancelled_len(), 0, "every cancel was consumed or answered");
    }

    #[test]
    fn a_tile_that_appears_later_loads_after_its_wait() {
        let t = texture();
        let shared: Arc<std::sync::Mutex<Option<Vec<u8>>>> = Arc::new(std::sync::Mutex::new(None));
        struct LateSource(Arc<std::sync::Mutex<Option<Vec<u8>>>>);
        impl TileSource for LateSource {
            fn load(&self, _texture: &TiledTexture, key: TileKey) -> Result<PakPayload, StreamError> {
                match self.0.lock().ok().and_then(|bytes| bytes.clone()) {
                    Some(bytes) => Ok(bytes.into()),
                    None => Err(StreamError::NotFound { key }),
                }
            }
        }
        let mut cache = TileCache::new(1 << 20, LateSource(Arc::clone(&shared)));
        cache.register_texture(0, t);
        cache.request(0, [-60.0, -60.0], 128.0, 8.0, 1);
        cache.flush();
        assert_eq!(cache.resident_count(), 0);
        assert!(cache.failed_count() > 0);
        *shared.lock().expect("bytes") = Some(vec![1; 16]);
        for _ in 0..4 {
            cache.request(0, [-60.0, -60.0], 128.0, 8.0, 4);
            cache.flush();
        }
        assert!(cache.is_resident(key(0, 0, 0)), "loaded once it existed");
        assert!(cache.is_resident(tail_key(0)));
        assert_eq!(cache.failed_count(), 0, "a success forgets the failure");
    }

    #[test]
    fn flush_drains_every_pending_load_and_returns_when_idle() {
        let t = TiledTexture::new("macro_albedo", 64, 32, TileFormat::Bc7Srgb);
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 16);
        src.insert(tail_key(0), vec![3; 4]);
        let mut cache = TileCache::new(1 << 20, src);
        cache.register_texture(0, t);
        // Nothing pending: returns at once.
        cache.flush();
        assert_eq!(cache.resident_count(), 0);
        let enqueued = cache.request(0, [0.0, 0.0], 64.0, 1000.0, 8);
        assert_eq!(enqueued, 5);
        cache.flush();
        assert_eq!(cache.pending_count(), 0);
        assert_eq!(cache.resident_count(), 6, "five tiles and the tail");
        assert_eq!(cache.current_bytes(), 5 * 16 + 4);
    }

    #[test]
    fn odd_texture_descriptions_request_nothing_instead_of_panicking() {
        // What a corrupt terrain index can describe: a tile larger than the texture, a zero
        // tile size, a zero-sized texture. None is tiled; requests only ask for the tail.
        for (width, tile_size) in [(128u32, 256u32), (128, 0), (0, 32), (0, 0)] {
            let t = TiledTexture {
                name: "macro_albedo".into(),
                width,
                height: width,
                tile_size,
                format: TileFormat::Bc7Srgb,
                mip_count: 1,
                tail_mips: 0,
            };
            assert_eq!(t.tail_start_mip(), 0, "{width} / {tile_size}");
            assert_eq!(t.tiles_per_side(0), 0);
            assert_eq!(t.tile_count(0), 0);
            let mut cache = TileCache::new(1 << 20, MemoryTileSource::new());
            cache.register_texture(0, t);
            assert_eq!(cache.request(0, [0.0, 0.0], 128.0, 1000.0, 8), 0);
            cache.flush();
            assert_eq!(cache.resident_count(), 0);
        }
    }

    #[test]
    fn stitch_rejects_tiles_outside_the_grid_and_duplicates() {
        let tile = |x: u32, y: u32| CookedHeightfieldTile {
            x,
            y,
            min: 1,
            max: 1,
            samples: vec![1; 4],
        };
        let base = CookedTiledHeightfield {
            version: TILED_HEIGHTFIELD_FORMAT_VERSION,
            width: 4,
            height: 2,
            tile_size: 2,
            tiles: vec![tile(0, 0), tile(1, 0)],
        };
        assert!(stitch_tiled_heightfield(&base).is_ok());
        let mut outside = base.clone();
        outside.tiles.push(tile(2, 0));
        assert!(matches!(
            stitch_tiled_heightfield(&outside),
            Err(TerrainIndexError::BadHeightfield(m)) if m.contains("outside")
        ));
        let mut twice = base.clone();
        twice.tiles.push(tile(1, 0));
        assert!(matches!(
            stitch_tiled_heightfield(&twice),
            Err(TerrainIndexError::BadHeightfield(m)) if m.contains("twice")
        ));
        // A huge coordinate is refused, not wrapped onto the map.
        let mut far = base;
        far.tiles[1].x = u32::MAX;
        assert!(stitch_tiled_heightfield(&far).is_err());
    }

    /// Two views in one round both stay wanted, and a budget that fits one neighbourhood
    /// still keeps each view's nearest exclusive tile resident.
    #[test]
    fn two_views_keep_the_near_tiles_when_the_budget_is_tight() {
        let t = texture();
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 8);
        src.insert(tail_key(0), vec![3; 4]);
        // The budget holds one view's mip-0 footprint (three tiles) plus the pinned tail.
        let mut cache = TileCache::new(3 * 8 + 4, src);
        cache.register_texture(0, t);
        // One frame, two views that share no tiles: the camera at the -X/-Z corner, then a
        // far pin at the opposite corner.
        cache.request(0, [-48.0, -48.0], 128.0, 8.0, 16);
        cache.request(0, [48.0, 48.0], 128.0, 8.0, 16);
        cache.flush();
        assert!(
            cache.is_resident(key(0, 0, 0)),
            "the camera's nearest tile stays resident"
        );
        assert!(
            !cache.is_resident(key(0, 2, 3)) && !cache.is_resident(key(0, 3, 2)),
            "the far view's edge tiles are the eviction victims, not the camera's"
        );
    }

    #[test]
    fn both_views_stay_wanted_after_a_round() {
        let t = texture();
        let mut src = MemoryTileSource::new();
        fill_source(&mut src, &t, 64);
        src.insert(tail_key(0), vec![1; 8]);
        // View B's neighbourhood is five tiles (radius 8 around tile (3, 3)); the tail is 8 bytes.
        // The union of both views is larger, so eviction has to choose.
        let budget = 5 * 64 + 8;
        let mut cache = TileCache::new(budget, src);
        cache.register_texture(0, t);
        let view_a = [-48.0, -48.0];
        let view_b = [48.0, 48.0];
        for _ in 0..3 {
            cache.request(0, view_a, 128.0, 8.0, 8);
            cache.request(0, view_b, 128.0, 8.0, 8);
            cache.flush();
        }
        assert!(
            cache.wanted.contains_key(&key(0, 0, 0)),
            "view A stays wanted, got {:?}",
            cache.wanted.keys().collect::<Vec<_>>()
        );
        assert!(cache.wanted.contains_key(&key(0, 3, 3)), "view B stays wanted");
        assert!(
            cache.is_resident(key(0, 0, 0)),
            "view A nearest tile stays resident"
        );
        assert!(
            cache.is_resident(key(0, 3, 3)),
            "view B nearest tile stays resident"
        );
        assert!(cache.current_bytes() <= budget);
    }

    /// A second request for the same view must not walk the tile grid again, and a move of
    /// more than one tile walks a smaller neighbourhood than the full mip chain.
    #[test]
    fn an_unchanged_view_does_not_rescan_the_tile_grid() {
        let t = TiledTexture::new("macro_albedo", 4096, 256, TileFormat::Bc7Srgb);
        let mut cache = TileCache::new(1 << 20, MemoryTileSource::new());
        cache.register_texture(0, t);
        // The full mip chain is 16² + 8² + 4² + 2² + 1 = 341 tiles. A 300 m radius on a
        // 4096 m map queries the window around the camera: 119 tiles at the origin, 112 after
        // a 300 m move along X. Mip 0 tiles are 256 m.
        cache.request(0, [0.0, 0.0], 4096.0, 300.0, 4);
        let first = cache.tiles_visited;
        assert_eq!(first, 119, "the first request walked {first} tiles");
        cache.request(0, [0.0, 0.0], 4096.0, 300.0, 4);
        assert_eq!(cache.tiles_visited, first, "a held view does not rescan");
        cache.request(0, [300.0, 0.0], 4096.0, 300.0, 4);
        let scanned = cache.tiles_visited - first;
        assert_eq!(scanned, 112, "the moved scan walked {scanned} tiles");
    }
}

#[cfg(test)]
mod index_v2_tests {
    use super::*;
    use crate::source::CountMapping;

    /// The terrain index as every release before ADR-8471 declared it (field for field), so the
    /// version-1 layout this build writes and reads is pinned to the shipped one.
    #[derive(serde::Serialize)]
    struct ShippedV1 {
        version: u32,
        map_id: AssetId,
        slots: Vec<TerrainSlotIndex>,
        heightfield: Option<TerrainHeightfieldIndex>,
        layers: Vec<String>,
        #[serde(default)]
        world_size_m: Option<f32>,
        #[serde(default)]
        world_origin: Option<[f32; 3]>,
    }

    fn v1() -> TerrainIndex {
        TerrainIndex {
            version: 1,
            map_id: AssetId::sub_asset(AssetId::NIL, "map"),
            slots: Vec::new(),
            heightfield: Some(TerrainHeightfieldIndex {
                width: 9,
                height: 5,
                tile_size: 256,
                tiles_x: 1,
                tiles_y: 1,
                bytes: 90,
            }),
            layers: vec!["grass".into()],
            world_size_m: Some(64.0),
            world_origin: Some([1.0, -2.0, 3.0]),
            heights: None,
            holes: None,
            material: None,
            source_mapping: None,
            source_spacing_z_m: None,
            source_valid_mask: None,
        }
    }

    fn cooked(payload: &[u8]) -> Vec<u8> {
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of(TERRAIN_INDEX_TYPE_PATH),
            asset_id: AssetId::NIL,
            importer_version: 7,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        regia_asset::encode_cooked_file(&header, payload).expect("encode")
    }

    #[test]
    fn a_shipped_v1_index_loads_without_a_mapping_and_migrates_to_v2() {
        let old = v1();
        let shipped = postcard::to_allocvec(&ShippedV1 {
            version: 1,
            map_id: old.map_id,
            slots: Vec::new(),
            heightfield: old.heightfield.clone(),
            layers: old.layers.clone(),
            world_size_m: old.world_size_m,
            world_origin: old.world_origin,
        })
        .expect("shipped bytes");
        assert_eq!(
            old.to_postcard().expect("v1"),
            shipped,
            "v1 is written in the shipped layout"
        );
        let file = cooked(&shipped);
        assert_eq!(terrain_index_file_version(&file).expect("probe"), Some(1));
        // One unreleased v2 holds ADR-8024's entries and ADR-8471's mapping (ADR-8619): the loader
        // migrates a v1 payload to the current version on load, so the loaded index is `old` at v2.
        assert_eq!(
            load_terrain_index_bytes(&file).expect("v1 loads"),
            TerrainIndex {
                version: TERRAIN_INDEX_FORMAT_VERSION,
                ..old.clone()
            }
        );

        let migrated = migrate_terrain_index_file(&file)
            .expect("migrate")
            .expect("older");
        let (header, _) = regia_asset::decode_cooked_file(&migrated).expect("wrapper");
        assert_eq!(header.importer_version, 7, "the cooked header is kept");
        let back = load_terrain_index_bytes(&migrated).expect("v2 loads");
        assert_eq!(back.version, TERRAIN_INDEX_FORMAT_VERSION);
        assert_eq!(
            back,
            TerrainIndex {
                version: TERRAIN_INDEX_FORMAT_VERSION,
                ..old
            }
        );
        assert_eq!(migrate_terrain_index_file(&migrated).expect("current"), None);
        assert_eq!(terrain_index_migration_sources(), vec![1, 2]);
    }

    #[test]
    fn a_v2_index_round_trips_its_mapping_bit_for_bit_and_v1_cannot_hold_one() {
        let mapping = CountMapping {
            cols: 9,
            rows: 5,
            meters_per_count: 0.125,
            zero_counts: 19_950,
            cell_m: 2.5,
            sample0_m: [-101.5, 640.0],
        };
        let index = TerrainIndex {
            version: TERRAIN_INDEX_FORMAT_VERSION,
            source_mapping: Some(mapping),
            source_spacing_z_m: None,
            source_valid_mask: None,
            ..v1()
        };
        let back = load_terrain_index_bytes(&cooked(&index.to_postcard().expect("v2"))).expect("v2 loads");
        assert_eq!(
            back.source_mapping.map(|m| m.canonical_bytes()),
            Some(mapping.canonical_bytes())
        );
        let bad = TerrainIndex {
            version: 1,
            source_mapping: Some(mapping),
            source_spacing_z_m: None,
            source_valid_mask: None,
            ..v1()
        };
        assert!(bad.to_postcard().is_err(), "the v1 layout has no mapping");
        // A file of another cooked type is not a terrain index.
        let file = cooked(&v1().to_postcard().expect("v1"));
        let (mut header, payload) = regia_asset::decode_cooked_file(&file).expect("wrapper");
        header.type_tag = regia_core::id::TypePathHash::of(TILED_HEIGHTFIELD_TYPE_PATH);
        let other = regia_asset::encode_cooked_file(&header, payload).expect("encode");
        assert_eq!(terrain_index_file_version(&other).expect("probe"), None);
        assert_eq!(migrate_terrain_index_file(&other).expect("skip"), None);
    }
}

/// Terrain index version 3 (ADR-8871): the source grid's Z spacing and valid-data mask.
#[cfg(test)]
mod index_v3_tests {
    use super::*;
    use crate::source::{CountMapping, ImportLimits, RasterDescriptor, TerrainSource, ValidMask};

    /// The v0.6.0 (version 2) payload, field for field as v0.6.0 wrote it: this test's own copy,
    /// not the loader's `TerrainIndexV2`.
    #[derive(serde::Serialize)]
    struct ShippedV2 {
        version: u32,
        map_id: AssetId,
        slots: Vec<TerrainSlotIndex>,
        heightfield: Option<TerrainHeightfieldIndex>,
        layers: Vec<String>,
        world_size_m: Option<f32>,
        world_origin: Option<[f32; 3]>,
        heights: Option<TerrainHeightsIndex>,
        holes: Option<TerrainHolesIndex>,
        material: Option<TerrainMaterialIndex>,
        source_mapping: Option<CountMapping>,
    }

    fn mapping() -> CountMapping {
        CountMapping {
            cols: 5,
            rows: 3,
            meters_per_count: 0.01,
            zero_counts: 1000,
            cell_m: 2.0,
            sample0_m: [10.0, -4.0],
        }
    }

    fn base(version: u32) -> TerrainIndex {
        TerrainIndex {
            version,
            map_id: AssetId::sub_asset(AssetId::NIL, "maps/grid"),
            slots: Vec::new(),
            heightfield: Some(TerrainHeightfieldIndex {
                width: 5,
                height: 3,
                tile_size: 256,
                tiles_x: 1,
                tiles_y: 1,
                bytes: 64,
            }),
            layers: vec!["ground".into()],
            world_size_m: None,
            world_origin: None,
            heights: None,
            holes: Some(TerrainHolesIndex { bytes: 12 }),
            material: None,
            source_mapping: Some(mapping()),
            source_spacing_z_m: None,
            source_valid_mask: None,
        }
    }

    fn cooked(payload: &[u8]) -> Vec<u8> {
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of(TERRAIN_INDEX_TYPE_PATH),
            asset_id: AssetId::NIL,
            importer_version: 9,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        regia_asset::encode_cooked_file(&header, payload).expect("encode")
    }

    /// A 5x3 raster, stored north first, spacing 2 m on X and 0.5 m on Z, two nodata samples.
    fn raster_source() -> TerrainSource {
        let descriptor = RasterDescriptor::from_json(
            br#"{"raster_descriptor": 1, "spacing_m": [2.0, 0.5], "row0": "north", "col0": "west",
                 "registration": "center", "origin_m": [10.0, -4.0], "nodata_counts": 65535}"#,
            ImportLimits::default(),
        )
        .expect("descriptor");
        let mut counts: Vec<u16> = (0..15).map(|i| 1000 + i * 7).collect();
        counts[1] = 65535;
        counts[13] = 65535;
        TerrainSource::from_raster(counts, [5, 3], 0.01, 1000, &descriptor, ImportLimits::default())
            .expect("raster source")
    }

    #[test]
    fn current_version_is_3_and_reads_1_and_2() {
        assert_eq!(TERRAIN_INDEX_FORMAT_VERSION, 3);
        assert_eq!(terrain_index_migration_sources(), [1, 2]);
        let fp = terrain_index_schema_fingerprint();
        assert!(fp.contains("source_spacing_z_m:Option<f32>"), "{fp}");
        assert!(
            fp.contains("TerrainValidMaskIndex{cols:u32,rows:u32,missing:u32,bits:Vec<u8>}"),
            "{fp}"
        );
    }

    #[test]
    fn a_shipped_v2_index_migrates_to_v3_with_no_spacing_or_mask() {
        let old = base(2);
        let shipped = postcard::to_allocvec(&ShippedV2 {
            version: 2,
            map_id: old.map_id,
            slots: Vec::new(),
            heightfield: old.heightfield.clone(),
            layers: old.layers.clone(),
            world_size_m: None,
            world_origin: None,
            heights: None,
            holes: old.holes.clone(),
            material: None,
            source_mapping: old.source_mapping,
        })
        .expect("shipped bytes");
        assert_eq!(
            old.to_postcard().expect("v2"),
            shipped,
            "v2 is written in the shipped layout"
        );
        let file = cooked(&shipped);
        assert_eq!(terrain_index_file_version(&file).expect("probe"), Some(2));
        let current = TerrainIndex {
            version: TERRAIN_INDEX_FORMAT_VERSION,
            ..old.clone()
        };
        assert_eq!(load_terrain_index_bytes(&file).expect("v2 loads"), current);

        let migrated = migrate_terrain_index_file(&file)
            .expect("migrate")
            .expect("older");
        let (header, payload) = regia_asset::decode_cooked_file(&migrated).expect("wrapper");
        assert_eq!(header.importer_version, 9, "the cooked header is kept");
        assert_eq!(payload.first(), Some(&3), "the payload is version 3");
        assert_eq!(
            payload,
            &current.to_postcard().expect("v3")[..],
            "the migrated payload is what the v3 writer writes"
        );
        assert_eq!(load_terrain_index_bytes(&migrated).expect("v3 loads"), current);
        // Idempotent: a second run leaves the file as it is.
        assert_eq!(migrate_terrain_index_file(&migrated).expect("current"), None);
        // A v3 index with neither entry is the v2 bytes plus two `None` bytes and the version.
        let mut expect = shipped.clone();
        expect[0] = 3;
        expect.extend_from_slice(&[0, 0]);
        assert_eq!(current.to_postcard().expect("v3"), expect);
    }

    #[test]
    fn a_v3_index_round_trips_unequal_spacing_and_the_mask_and_v2_cannot_hold_them() {
        let source = raster_source();
        assert_eq!(source.spacing_m(), [2.0, 0.5]);
        assert_eq!(source.missing_samples(), 2);
        // The cook's v2 path refuses this source; v3 holds it.
        assert!(source.count_mapping().is_err());
        let index = base(TERRAIN_INDEX_FORMAT_VERSION).with_source_grid(&source);
        assert_eq!(index.source_spacing_z_m, Some(0.5));
        assert_eq!(index.source_spacing_m(), Some([2.0, 0.5]));
        let mask = index.source_valid_mask.clone().expect("a mask");
        assert_eq!((mask.cols, mask.rows, mask.missing), (5, 3, 2));
        // Canonical order is south first: stored row 0 (north) is canonical row 2, so stored
        // samples 1 (north row, col 1) and 13 (south row, col 3) are canonical 11 and 3.
        let flags = mask.to_valid_mask().expect("mask").to_flags();
        let missing: Vec<usize> = flags
            .iter()
            .enumerate()
            .filter(|(_, v)| !**v)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(missing, vec![3, 11]);
        assert_eq!(mask.bits, vec![0b1111_0111, 0b0111_0111], "bit layout pinned");
        assert_eq!(
            &mask.to_valid_mask().expect("mask"),
            source.valid_mask().expect("source mask")
        );

        let bytes = index.to_postcard().expect("v3");
        let back = load_terrain_index_bytes(&cooked(&bytes)).expect("v3 loads");
        assert_eq!(back, index);
        assert_eq!(back.source_spacing_z_m.map(f32::to_bits), Some(0.5f32.to_bits()));
        assert_eq!(back.to_postcard().expect("again"), bytes, "byte stable");

        for version in [1, 2] {
            let old = TerrainIndex {
                version,
                ..index.clone()
            };
            assert!(
                old.to_postcard().is_err(),
                "v{version} cannot hold a Z spacing or a mask"
            );
        }
        // An equal-spacing source with every sample writes neither entry.
        let plain = TerrainSource::from_counts((0..15).collect(), mapping(), ImportLimits::default())
            .expect("plain source");
        let plain_index = base(TERRAIN_INDEX_FORMAT_VERSION).with_source_grid(&plain);
        assert_eq!(plain_index.source_spacing_z_m, None);
        assert_eq!(plain_index.source_valid_mask, None);
        assert_eq!(plain_index.source_grid_unsupported(), None);
    }

    #[test]
    fn a_consumer_of_the_mapping_alone_is_told_why_it_cannot_use_a_v3_grid() {
        let index = base(TERRAIN_INDEX_FORMAT_VERSION).with_source_grid(&raster_source());
        let why = index.source_grid_unsupported().expect("refused");
        assert!(why.starts_with("terrain_mapping_unrepresentable:"), "{why}");
        let masked = TerrainIndex {
            source_spacing_z_m: None,
            ..index
        };
        let why = masked.source_grid_unsupported().expect("refused");
        assert!(why.starts_with("terrain_raster_missing_data: 2 of 15"), "{why}");
    }

    #[test]
    fn bad_source_grid_entries_are_refused() {
        let good = base(TERRAIN_INDEX_FORMAT_VERSION).with_source_grid(&raster_source());
        let mask = good.source_valid_mask.clone().expect("mask");
        let refused = |index: TerrainIndex, what: &str| {
            let bytes = index.to_postcard().expect("encodes");
            match load_terrain_index_payload(&bytes) {
                Err(TerrainIndexError::BadSourceGrid(_)) => {}
                other => panic!("{what}: {other:?}"),
            }
        };
        let with_z = |z: f32| TerrainIndex {
            source_spacing_z_m: Some(z),
            ..good.clone()
        };
        refused(with_z(0.0), "zero Z spacing");
        refused(with_z(-0.5), "negative Z spacing");
        refused(with_z(f32::NAN), "NaN Z spacing");
        refused(with_z(f32::INFINITY), "infinite Z spacing");
        refused(with_z(2.0), "a Z spacing equal to X");
        refused(with_z(f32::MAX), "a Z extent past f32");
        refused(
            TerrainIndex {
                source_mapping: None,
                source_valid_mask: None,
                ..good.clone()
            },
            "a Z spacing without a mapping",
        );
        refused(
            TerrainIndex {
                source_mapping: None,
                source_spacing_z_m: None,
                ..good.clone()
            },
            "a mask without a mapping",
        );
        let with_mask = |m: TerrainValidMaskIndex| TerrainIndex {
            source_valid_mask: Some(m),
            ..good.clone()
        };
        refused(
            with_mask(TerrainValidMaskIndex {
                cols: 3,
                rows: 5,
                ..mask.clone()
            }),
            "a mask of another grid",
        );
        refused(
            with_mask(TerrainValidMaskIndex {
                bits: vec![0xf7],
                ..mask.clone()
            }),
            "a short mask",
        );
        refused(
            with_mask(TerrainValidMaskIndex {
                bits: vec![0xf7, 0x77, 0],
                ..mask.clone()
            }),
            "a long mask",
        );
        refused(
            with_mask(TerrainValidMaskIndex {
                bits: vec![0xf7, 0xf7],
                ..mask.clone()
            }),
            "a padding bit set",
        );
        refused(
            with_mask(TerrainValidMaskIndex {
                missing: 3,
                ..mask.clone()
            }),
            "a missing count the bits disagree with",
        );
        refused(
            with_mask(TerrainValidMaskIndex {
                missing: 0,
                bits: vec![0xff, 0x7f],
                ..mask.clone()
            }),
            "an all-valid mask (written as none)",
        );
        refused(
            with_mask(TerrainValidMaskIndex {
                missing: 15,
                bits: vec![0, 0],
                ..mask.clone()
            }),
            "an all-missing mask",
        );
        // The good one loads.
        load_terrain_index_payload(&good.to_postcard().expect("v3")).expect("good loads");
    }

    #[test]
    fn a_newer_index_is_refused_with_its_version() {
        let mut bytes = base(TERRAIN_INDEX_FORMAT_VERSION).to_postcard().expect("v3");
        bytes[0] = 4;
        match load_terrain_index_payload(&bytes) {
            Err(TerrainIndexError::BadVersion {
                found: 4,
                supported: 3,
            }) => {}
            other => panic!("{other:?}"),
        }
        let err = migrate_terrain_index_file(&cooked(&bytes)).expect_err("newer");
        assert!(err.contains("version 4 is newer"), "{err}");
    }

    #[test]
    fn packed_mask_bytes_round_trip() {
        for (cols, rows) in [(2u32, 2u32), (8, 1), (9, 7), (64, 3), (65, 65)] {
            let flags: Vec<bool> = (0..cols * rows).map(|i| (i * 7 + 3) % 5 != 0).collect();
            let mask = ValidMask::from_flags(cols, rows, &flags).expect("mask");
            let bytes = mask.to_packed_bytes();
            assert_eq!(bytes.len(), (cols * rows).div_ceil(8) as usize);
            let back = ValidMask::from_packed_bytes(cols, rows, &bytes).expect("unpack");
            assert_eq!(back, mask, "{cols}x{rows}");
        }
    }
}
