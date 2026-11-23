// SPDX-License-Identifier: MPL-2.0
//! Layer chunks: the presentation section of the cooked `<map>.meshlayers`, the chunk grid on the
//! CDLOD node grid, the LOD metric and the run-time residency (spec 26 § 2a.5, ADR-8398).
//!
//! The canonical section (W-06's `WorldGeomCooked`) is the sim's. This module owns the bytes after
//! it: [`TerrainMeshLayersCooked`], a postcard value with its version first. Each layer is cut on
//! the quadtree node grid at [`LayerChunkGrid::chunk_level`]. Each chunk has up to
//! [`MAX_LAYER_LODS`] meshes, stored as `CookedMesh` v3 sub-assets of the map
//! (`meshlayers/<layer>/<x>_<z>/lod<i>`, [`chunk_lod_asset_path`]).
//!
//! **LOD metric.** Chunk LOD `i` draws while the chunk's rectangle distance is in
//! `[range(i - 1), range(i))`, with `range(l) = range_factor * node_world_size(l)`: the terrain's own
//! ranges ([`LayerChunkGrid::lod_for_distance`]).
//!
//! **Residency.** [`TerrainLayerStream`] keeps the coarsest LOD of every chunk for the life of the
//! layers, so a chunk never shows a hole while it streams. Finer LODs fit in `budget_bytes`,
//! nearest chunk first, ties in `(layer, z, x)` order.
//!
//! **LOD cross-fade** (ADR-8842). When a drawn chunk changes LOD, [`drive_layer_stream`] keeps the
//! old LOD's entity and spawns the new one beside it for [`TerrainLayerStream::fade_frames`]
//! frames, with the renderer's complementary dithers (`regia_render::MeshLodFade`: `c` on the
//! incoming LOD, `-c` on the outgoing one), so every pixel of the chunk is drawn by exactly one of
//! the two and the switch dissolves instead of popping. The switch frame is the first of the
//! fade: a length of `N` gives `N - 1` dithered frames with shares `1/N .. (N-1)/N`, then the new
//! LOD draws whole and the old entity goes. A length of 0 or 1 switches at once, as before. A
//! switch back to the outgoing LOD mid-fade continues from the share it had; a switch to a third
//! LOD drops the outgoing entity and fades the current one out. The outgoing entity keeps its
//! mesh alive by reference even when the stream has evicted that LOD.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::AssetId;
use regia_render::CpuMesh as RenderMesh;
use serde::{Deserialize, Serialize};

use crate::cdlod::{QuadNode, node_world_size};

/// The presentation section's version (2: [`CookedLayer::material_id`], ADR-8451; 3:
/// [`CookedLayer::collider`], ADR-8872). A version-2 section loads with `collider: false` (the cook
/// writes it again on the next `regia cook`, which the file header's version tells); version 1 and a
/// newer version are refused, and the cook writes them again.
pub const PRESENTATION_VERSION: u32 = 3;

/// The most LODs a chunk has (spec 26 § 2a.5).
pub const MAX_LAYER_LODS: u8 = 4;

/// The asset path of chunk `(x, z)`'s LOD `lod` in `layer`, a sub-asset of the map id.
#[must_use]
pub fn chunk_lod_asset_path(layer: &str, x: u32, z: u32, lod: u8) -> String {
    format!("meshlayers/{layer}/{x}_{z}/lod{lod}")
}

/// The asset id of [`chunk_lod_asset_path`] under `map_id`.
#[must_use]
pub fn chunk_lod_asset_id(map_id: AssetId, layer: &str, x: u32, z: u32, lod: u8) -> AssetId {
    AssetId::sub_asset(map_id, &chunk_lod_asset_path(layer, x, z, lod))
}

/// The quadtree the chunks sit on: the terrain's draw rectangle, depth and ranges.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerChunkGrid {
    /// The quadtree span, metres (`quadtree_span`).
    pub span_m: f32,
    /// The quadtree root level.
    pub root_level: u32,
    /// The chunk level: the coarsest level whose node is at most `chunk_m`, else 0.
    pub chunk_level: u32,
    /// Grid cells per node edge (`CdlodConfig::grid_size`).
    pub grid_size: u32,
    /// `CdlodConfig::range_factor`.
    pub range_factor: f32,
    /// The map's XZ grid origin, metres.
    pub origin: [f32; 2],
}

impl LayerChunkGrid {
    /// A grid over `span_m` with `root_level`, cut at the coarsest level whose node is at most
    /// `chunk_m` ([`Self::chunk_level_for`]).
    #[must_use]
    pub fn new(
        span_m: f32,
        root_level: u32,
        grid_size: u32,
        range_factor: f32,
        origin: [f32; 2],
        chunk_m: f32,
    ) -> Self {
        Self {
            span_m,
            root_level,
            chunk_level: Self::chunk_level_for(span_m, root_level, chunk_m),
            grid_size,
            range_factor,
            origin,
        }
    }

    /// The coarsest level in `0..=root_level` whose node side is at most `chunk_m`; 0 when even
    /// level 0 is larger.
    #[must_use]
    pub fn chunk_level_for(span_m: f32, root_level: u32, chunk_m: f32) -> u32 {
        (0..=root_level)
            .rev()
            .find(|&l| node_world_size(span_m, root_level, l) <= chunk_m)
            .unwrap_or(0)
    }

    /// Node side at `level`, metres. Above the root it keeps doubling.
    #[must_use]
    pub fn node_size(&self, level: u32) -> f32 {
        if level <= self.root_level {
            node_world_size(self.span_m, self.root_level, level)
        } else {
            let up = (level - self.root_level).min(30);
            self.span_m * (1u32 << up) as f32
        }
    }

    /// `range(l) = range_factor * node_size(l)`: the distance under which the terrain draws level
    /// `l` or finer.
    #[must_use]
    pub fn range(&self, level: u32) -> f32 {
        self.range_factor * self.node_size(level)
    }

    /// The simplification bound of chunk LOD `lod`: the terrain's level-`lod` cell, metres.
    #[must_use]
    pub fn lod_error_m(&self, lod: u8) -> f32 {
        self.node_size(u32::from(lod)) / self.grid_size.max(1) as f32
    }

    /// Chunks per side.
    #[must_use]
    pub fn chunks_per_side(&self) -> u32 {
        1u32 << (self.root_level.saturating_sub(self.chunk_level)).min(16)
    }

    /// The chunk node `(x, z)`.
    #[must_use]
    pub fn node(&self, x: u32, z: u32) -> QuadNode {
        QuadNode {
            level: self.chunk_level,
            x,
            z,
        }
    }

    /// `(min_x, min_z, size)` of chunk `(x, z)`, metres.
    #[must_use]
    pub fn chunk_rect(&self, x: u32, z: u32) -> (f32, f32, f32) {
        self.node(x, z)
            .world_rect_at(self.span_m, self.root_level, self.origin)
    }

    /// The chunk holding XZ point `p`, or `None` outside the grid. A point on a shared border goes
    /// to the chunk on its +x / +z side.
    #[must_use]
    pub fn chunk_at(&self, p: [f32; 2]) -> Option<(u32, u32)> {
        let size = self.node_size(self.chunk_level);
        let half = self.span_m * 0.5;
        let n = self.chunks_per_side();
        let cell = |v: f32, o: f32| -> Option<u32> {
            let t = ((v - (o - half)) / size).floor();
            (t >= 0.0 && t < n as f32).then_some(t as u32)
        };
        Some((cell(p[0], self.origin[0])?, cell(p[1], self.origin[1])?))
    }

    /// Horizontal distance from `p` to chunk `(x, z)`'s rectangle (`QuadNode::rect_distance` on
    /// this grid's origin).
    #[must_use]
    pub fn rect_distance(&self, x: u32, z: u32, p: [f32; 2]) -> f32 {
        let (min_x, min_z, size) = self.chunk_rect(x, z);
        let dx = (min_x - p[0]).max(p[0] - (min_x + size)).max(0.0);
        let dz = (min_z - p[1]).max(p[1] - (min_z + size)).max(0.0);
        (dx * dx + dz * dz).sqrt()
    }

    /// The LOD of a chunk at rectangle distance `d` with `lods` LODs: the count of levels `l` with
    /// `d >= range(l)`, capped at `lods - 1`.
    #[must_use]
    pub fn lod_for_distance(&self, d: f32, lods: usize) -> usize {
        let last = lods.saturating_sub(1);
        (0..last as u32).take_while(|&l| d >= self.range(l)).count()
    }
}

/// One cooked LOD of a chunk.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedChunkLod {
    /// The `CookedMesh` v3 sub-asset ([`chunk_lod_asset_id`]).
    pub mesh: AssetId,
    /// Triangles.
    pub triangles: u32,
    /// Resident bytes counted against the budget (vertex and index bytes).
    pub bytes: u64,
    /// The simplifier's geometric error, metres (0 for LOD 0).
    pub error_m: f32,
}

/// One chunk of a layer: a node at the grid's chunk level and its LODs (LOD 0 first).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedLayerChunk {
    /// Node column.
    pub x: u32,
    /// Node row.
    pub z: u32,
    /// Bounds `[min, max]`, world metres.
    pub bounds: [[f32; 3]; 2],
    /// LODs, finest first.
    pub lods: Vec<CookedChunkLod>,
}

/// One layer's render table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedLayer {
    /// The layer id.
    pub id: String,
    /// Pick layer of the chunk entities.
    pub map_layer: u8,
    /// Chunk entities cast shadows.
    pub cast_shadows: bool,
    /// The source layer's `collider` flag (spec 26 § 2a.8): build engine-physics colliders from
    /// the layer's canonical triangles, for hosts that do not lockstep (version 3, ADR-8872;
    /// `false` in a migrated version-2 section).
    pub collider: bool,
    /// Render material asset path, as authored (empty: the engine's default rock).
    pub material: String,
    /// The cooked `CookedStandardMaterial` the cook resolved `material` to (`None`: empty
    /// `material`, the default rock; ADR-8451).
    pub material_id: Option<AssetId>,
    /// Chunks sorted by `(z, x)`.
    pub chunks: Vec<CookedLayerChunk>,
}

/// A version-2 [`CookedLayer`] (no `collider`), read only to migrate (ADR-8872).
#[derive(Deserialize)]
struct CookedLayerV2 {
    id: String,
    map_layer: u8,
    cast_shadows: bool,
    material: String,
    material_id: Option<AssetId>,
    chunks: Vec<CookedLayerChunk>,
}

/// A version-2 [`TerrainMeshLayersCooked`], read only to migrate (ADR-8872).
#[derive(Deserialize)]
struct TerrainMeshLayersCookedV2 {
    /// Always 2 (checked before this is decoded; postcard is positional).
    _version: u32,
    grid: LayerChunkGrid,
    layers: Vec<CookedLayerV2>,
    chunk_importer: u32,
}

impl From<TerrainMeshLayersCookedV2> for TerrainMeshLayersCooked {
    /// The version-2 to version-3 step: every layer's `collider` is `false` (a version-2 section
    /// does not say; the source document does, and the next cook writes it).
    fn from(v2: TerrainMeshLayersCookedV2) -> Self {
        Self {
            version: PRESENTATION_VERSION,
            grid: v2.grid,
            layers: v2
                .layers
                .into_iter()
                .map(|l| CookedLayer {
                    id: l.id,
                    map_layer: l.map_layer,
                    cast_shadows: l.cast_shadows,
                    collider: false,
                    material: l.material,
                    material_id: l.material_id,
                    chunks: l.chunks,
                })
                .collect(),
            chunk_importer: v2.chunk_importer,
        }
    }
}

/// The presentation section of a cooked `<map>.meshlayers` (spec 26 § 2a.10).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerrainMeshLayersCooked {
    /// [`PRESENTATION_VERSION`].
    pub version: u32,
    /// The chunk grid.
    pub grid: LayerChunkGrid,
    /// Layers in document order.
    pub layers: Vec<CookedLayer>,
    /// The packed importer version of the chunk LOD meshes the cook wrote (version 2): a cook whose
    /// chunk importer differs writes them again (ADR-8451).
    pub chunk_importer: u32,
}

/// Why a presentation section did not load. Codes are part of the agent protocol.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayerPresentationError {
    /// (`meshlayers_presentation_newer`) written by a newer engine: cook again.
    #[error("meshlayers presentation version {found} is newer than this engine's {supported}")]
    Newer {
        /// The version in the file.
        found: u32,
        /// This engine's version.
        supported: u32,
    },
    /// (`meshlayers_presentation_stale`) written by an older engine: cook again.
    #[error("meshlayers presentation version {found} is older than this engine's {supported}: cook again")]
    Stale {
        /// The version in the file.
        found: u32,
        /// This engine's version.
        supported: u32,
    },
    /// (`meshlayers_presentation_corrupt`) the bytes do not decode or an index is out of range.
    #[error("meshlayers presentation is corrupt: {0}")]
    Corrupt(String),
}

impl LayerPresentationError {
    /// The agent error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Newer { .. } => "meshlayers_presentation_newer",
            Self::Stale { .. } => "meshlayers_presentation_stale",
            Self::Corrupt(_) => "meshlayers_presentation_corrupt",
        }
    }
}

impl TerrainMeshLayersCooked {
    /// The section's bytes (postcard, version first).
    ///
    /// # Errors
    ///
    /// [`LayerPresentationError::Corrupt`] if postcard refuses the value.
    pub fn encode(&self) -> Result<Vec<u8>, LayerPresentationError> {
        postcard::to_allocvec(self).map_err(|e| LayerPresentationError::Corrupt(e.to_string()))
    }

    /// Decodes and checks a section. A version-2 section migrates to version 3 (`collider: false`,
    /// ADR-8872); any other version is refused before the rest is decoded. An empty section (a
    /// cooked file with no presentation yet) is `Ok(None)`.
    ///
    /// # Errors
    ///
    /// [`LayerPresentationError`].
    pub fn decode(bytes: &[u8]) -> Result<Option<Self>, LayerPresentationError> {
        if bytes.is_empty() {
            return Ok(None);
        }
        let corrupt = |e: postcard::Error| LayerPresentationError::Corrupt(e.to_string());
        let (version, _) = postcard::take_from_bytes::<u32>(bytes).map_err(corrupt)?;
        if version > PRESENTATION_VERSION {
            return Err(LayerPresentationError::Newer {
                found: version,
                supported: PRESENTATION_VERSION,
            });
        }
        if version == 0 {
            return Err(LayerPresentationError::Corrupt("version 0".into()));
        }
        let (value, rest) = if version == 2 {
            let (v2, rest) =
                postcard::take_from_bytes::<TerrainMeshLayersCookedV2>(bytes).map_err(corrupt)?;
            (Self::from(v2), rest)
        } else if version < PRESENTATION_VERSION {
            return Err(LayerPresentationError::Stale {
                found: version,
                supported: PRESENTATION_VERSION,
            });
        } else {
            postcard::take_from_bytes::<Self>(bytes).map_err(corrupt)?
        };
        if !rest.is_empty() {
            return Err(LayerPresentationError::Corrupt(format!(
                "{} trailing bytes",
                rest.len()
            )));
        }
        value.check()?;
        Ok(Some(value))
    }

    /// Structural checks: a sane grid, chunks inside it and sorted by `(z, x)`, 1 to
    /// [`MAX_LAYER_LODS`] LODs each.
    ///
    /// # Errors
    ///
    /// [`LayerPresentationError::Corrupt`] naming the first problem.
    pub fn check(&self) -> Result<(), LayerPresentationError> {
        let bad = |m: String| Err(LayerPresentationError::Corrupt(m));
        let g = &self.grid;
        if !(g.span_m.is_finite() && g.span_m > 0.0)
            || g.root_level > 16
            || g.chunk_level > g.root_level
            || !g.range_factor.is_finite()
            || g.range_factor < 2.0
            || g.grid_size == 0
            || !g.origin.iter().all(|v| v.is_finite())
        {
            return bad("chunk grid out of range".into());
        }
        let n = g.chunks_per_side();
        for layer in &self.layers {
            let mut last: Option<(u32, u32)> = None;
            for c in &layer.chunks {
                if c.x >= n || c.z >= n {
                    return bad(format!(
                        "layer `{}` chunk {}_{} outside the grid",
                        layer.id, c.x, c.z
                    ));
                }
                if last.is_some_and(|l| l >= (c.z, c.x)) {
                    return bad(format!("layer `{}` chunks not sorted by (z, x)", layer.id));
                }
                last = Some((c.z, c.x));
                if c.lods.is_empty() || c.lods.len() > usize::from(MAX_LAYER_LODS) {
                    return bad(format!(
                        "layer `{}` chunk {}_{} has {} LODs",
                        layer.id,
                        c.x,
                        c.z,
                        c.lods.len()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Chunks of every layer.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.layers.iter().map(|l| l.chunks.len()).sum()
    }
}

/// The default [`TerrainLayerStream::fade_frames`]: eight frames, about 0.13 s at 60 Hz, long
/// enough for TAA to average the dither into a blend.
pub const DEFAULT_LAYER_FADE_FRAMES: u32 = 8;

/// One chunk's LOD cross-fade: the outgoing entity, its LOD, and the frames into the fade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChunkFade {
    pub(crate) outgoing: Entity,
    pub(crate) outgoing_lod: u8,
    /// Frames into the fade, the switch frame being 1.
    pub(crate) frame: u32,
}

/// A chunk's key: `(layer, z, x)`, the stream's tie order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkKey {
    /// Layer index (document order).
    pub layer: u16,
    /// Node row.
    pub z: u32,
    /// Node column.
    pub x: u32,
}

/// Tags a streamed chunk entity (spec 26 § 2a.5).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerrainLayerChunk {
    /// Layer index (document order).
    pub layer: u16,
    /// The chunk's quadtree node.
    pub node: QuadNode,
    /// The LOD the entity draws.
    pub lod: u8,
}

#[derive(Debug, Default)]
struct ChunkState {
    /// Resident LOD meshes.
    meshes: BTreeMap<u8, Arc<RenderMesh>>,
    /// LODs whose load failed (not asked again).
    failed: BTreeSet<u8>,
}

/// The coarsest LOD index of a chunk, `None` for a chunk with no LOD.
fn last_lod(c: &CookedLayerChunk) -> Option<u8> {
    c.lods
        .len()
        .checked_sub(1)
        .map(|l| l.min(usize::from(u8::MAX)) as u8)
}

/// What one chunk should draw after a [`TerrainLayerStream::step`].
#[derive(Clone, Debug)]
pub struct ChunkDraw {
    /// The chunk.
    pub key: ChunkKey,
    /// The LOD to draw and its mesh; `None`: draw nothing (unwanted, or its coarsest LOD failed).
    pub draw: Option<(u8, Arc<RenderMesh>)>,
}

/// [`TerrainLayerStream`]'s counters (`terrain.mesh_layers`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayerStreamStats {
    /// Resident meshes per LOD.
    pub resident: [u32; MAX_LAYER_LODS as usize],
    /// Finer LODs inside the budget that are not loaded yet.
    pub pending: u32,
    /// Cooked bytes of the resident LODs ([`CookedChunkLod::bytes`], the coarsest set included): the
    /// unit [`Self::budget_bytes`] counts in. Not the memory the meshes hold: that is
    /// [`Self::resident_bytes`] (about 3.5 times more on Switchbacks, ADR-8873).
    pub bytes: u64,
    /// The budget for LODs beyond the coarsest set, in cooked bytes ([`Self::bytes`]).
    pub budget_bytes: u64,
    /// Cooked bytes of the always-resident coarsest set.
    pub coarsest_bytes: u64,
    /// Bytes the resident LOD meshes hold, the coarsest set included: their [`regia_render::LabVertex`]
    /// vertices and `u32` indices by capacity ([`layer_mesh_resident_bytes`]). This is the layer
    /// geometry's RAM, and the size the renderer uploads per drawn mesh (its vertex stride is a
    /// `LabVertex`), so the layer budgets (RAM, VRAM, the Deck's layer meshes) read it (ADR-8873).
    pub resident_bytes: u64,
    /// [`Self::resident_bytes`] of the always-resident coarsest set.
    pub coarsest_resident_bytes: u64,
    /// Chunks drawn.
    pub drawn: u32,
    /// Chunks the views want.
    pub wanted: u32,
    /// Loads asked of the loader since the layers changed.
    pub loads: u64,
    /// Finer LODs dropped since the layers changed.
    pub evictions: u64,
    /// LOD loads that failed.
    pub failed: u32,
}

/// Streams the layer chunks of one terrain around the views (spec 26 § 2a.5). Insert it as a
/// resource; `TerrainPlugin` drives it each `Update` for the terrain that has cooked layers.
#[derive(Resource, Debug)]
pub struct TerrainLayerStream {
    /// A chunk whose rectangle is within this distance of a view is wanted, metres.
    pub visible_radius: f32,
    /// Bytes allowed for LODs finer than the coarsest, nearest chunk first.
    pub budget_bytes: u64,
    /// Finer LOD loads per step (the coarsest set loads in full on the first step).
    pub max_loads_per_step: usize,
    /// Frames a LOD switch cross-fades over, the switch frame included (ADR-8842); 0 or 1 switch
    /// at once. Default [`DEFAULT_LAYER_FADE_FRAMES`].
    pub fade_frames: u32,
    chunks: BTreeMap<ChunkKey, ChunkState>,
    stats: LayerStreamStats,
    /// Spawned chunk entities and the LOD each draws (the incoming LOD during a fade).
    pub(crate) entities: BTreeMap<ChunkKey, (Entity, u8)>,
    /// LOD cross-fades in progress.
    pub(crate) fades: BTreeMap<ChunkKey, ChunkFade>,
    /// Identity of the layers the state belongs to.
    pub(crate) owner: Option<(Entity, u64)>,
    /// Each layer's draw material, resolved once per owner ([`layer_material`]).
    pub(crate) materials: Vec<Arc<regia_render::CpuMaterial>>,
}

impl Default for TerrainLayerStream {
    fn default() -> Self {
        Self::new(512.0, 64 << 20)
    }
}

impl TerrainLayerStream {
    /// A stream wanting chunks within `visible_radius` and holding `budget_bytes` of finer LODs.
    #[must_use]
    pub fn new(visible_radius: f32, budget_bytes: u64) -> Self {
        Self {
            visible_radius,
            budget_bytes,
            max_loads_per_step: 8,
            fade_frames: DEFAULT_LAYER_FADE_FRAMES,
            chunks: BTreeMap::new(),
            stats: LayerStreamStats::default(),
            entities: BTreeMap::new(),
            fades: BTreeMap::new(),
            owner: None,
            materials: Vec::new(),
        }
    }

    /// The counters after the last step.
    #[must_use]
    pub fn stats(&self) -> LayerStreamStats {
        self.stats
    }

    /// Chunks in a LOD cross-fade after the last [`drive_layer_stream`] (two entities each).
    #[must_use]
    pub fn fading(&self) -> u32 {
        self.fades.len() as u32
    }

    /// Resident LODs of chunk `key`, finest first.
    #[must_use]
    pub fn resident_lods(&self, key: ChunkKey) -> Vec<u8> {
        self.chunks
            .get(&key)
            .map(|c| c.meshes.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Forgets every resident mesh (the layers changed). Spawned entities are the caller's.
    pub fn reset(&mut self) {
        self.chunks.clear();
        self.stats = LayerStreamStats {
            budget_bytes: self.budget_bytes,
            ..LayerStreamStats::default()
        };
    }

    /// One step: loads the coarsest LOD of every chunk (once), picks each wanted chunk's LOD by
    /// the terrain's ranges from the nearest of `sources` (XZ metres), keeps finer LODs inside
    /// `budget_bytes` nearest first (ties in `(layer, z, x)`), drops the rest, and returns what
    /// every chunk draws. `fetch` loads one LOD mesh.
    pub fn step(
        &mut self,
        cooked: &TerrainMeshLayersCooked,
        sources: &[[f32; 2]],
        fetch: &mut dyn FnMut(AssetId) -> Result<Arc<RenderMesh>, String>,
    ) -> Vec<ChunkDraw> {
        let _span = tracing::info_span!("terrain_layer_stream").entered();
        let grid = &cooked.grid;
        // The coarsest set: loaded once, never dropped.
        for (li, layer) in cooked.layers.iter().enumerate() {
            for c in &layer.chunks {
                let key = ChunkKey {
                    layer: li as u16,
                    z: c.z,
                    x: c.x,
                };
                let Some(last) = last_lod(c) else {
                    continue;
                };
                let state = self.chunks.entry(key).or_default();
                if !state.meshes.contains_key(&last) && !state.failed.contains(&last) {
                    self.stats.loads += 1;
                    match fetch(c.lods[usize::from(last)].mesh) {
                        Ok(mesh) => {
                            state.meshes.insert(last, mesh);
                        }
                        Err(error) => {
                            tracing::warn!(target: "regia_terrain", layer = %layer.id, x = c.x, z = c.z, %error, "mesh layer chunk coarsest LOD did not load");
                            state.failed.insert(last);
                        }
                    }
                }
            }
        }
        // Wanted chunks and their LODs.
        let mut wanted: Vec<(f32, ChunkKey, u8, &CookedLayerChunk)> = Vec::new();
        for (li, layer) in cooked.layers.iter().enumerate() {
            for c in &layer.chunks {
                let d = sources
                    .iter()
                    .map(|p| grid.rect_distance(c.x, c.z, *p))
                    .fold(f32::INFINITY, f32::min);
                if d <= self.visible_radius && last_lod(c).is_some() {
                    let lod = grid.lod_for_distance(d, c.lods.len()) as u8;
                    let key = ChunkKey {
                        layer: li as u16,
                        z: c.z,
                        x: c.x,
                    };
                    wanted.push((d, key, lod, c));
                }
            }
        }
        wanted.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        // Finer LODs inside the budget.
        let mut used = 0u64;
        let mut keep: BTreeSet<(ChunkKey, u8)> = BTreeSet::new();
        let mut pending = 0u32;
        let mut loads_left = self.max_loads_per_step;
        for &(_, key, lod, c) in &wanted {
            if Some(lod) == last_lod(c) {
                continue;
            }
            let rec = c.lods[usize::from(lod)];
            if used.saturating_add(rec.bytes) > self.budget_bytes {
                continue;
            }
            let state = self.chunks.entry(key).or_default();
            if state.failed.contains(&lod) {
                continue;
            }
            used += rec.bytes;
            keep.insert((key, lod));
            if state.meshes.contains_key(&lod) {
                continue;
            }
            if loads_left == 0 {
                pending += 1;
                continue;
            }
            loads_left -= 1;
            self.stats.loads += 1;
            match fetch(rec.mesh) {
                Ok(mesh) => {
                    state.meshes.insert(lod, mesh);
                }
                Err(error) => {
                    tracing::warn!(target: "regia_terrain", x = c.x, z = c.z, lod, %error, "mesh layer chunk LOD did not load");
                    state.failed.insert(lod);
                    keep.remove(&(key, lod));
                    used -= rec.bytes;
                }
            }
        }
        // Drop finer LODs outside the kept set.
        let coarsest: BTreeMap<ChunkKey, u8> = cooked
            .layers
            .iter()
            .enumerate()
            .flat_map(|(li, l)| {
                l.chunks.iter().filter_map(move |c| {
                    Some((
                        ChunkKey {
                            layer: li as u16,
                            z: c.z,
                            x: c.x,
                        },
                        last_lod(c)?,
                    ))
                })
            })
            .collect();
        for (key, state) in &mut self.chunks {
            let last = coarsest.get(key).copied().unwrap_or(0);
            let before = state.meshes.len();
            state
                .meshes
                .retain(|lod, _| *lod == last || keep.contains(&(*key, *lod)));
            self.stats.evictions += (before - state.meshes.len()) as u64;
        }
        // What each chunk draws.
        let want: BTreeMap<ChunkKey, u8> = wanted.iter().map(|w| (w.1, w.2)).collect();
        let mut draws = Vec::with_capacity(coarsest.len());
        let mut stats = LayerStreamStats {
            pending,
            budget_bytes: self.budget_bytes,
            wanted: want.len() as u32,
            loads: self.stats.loads,
            evictions: self.stats.evictions,
            ..LayerStreamStats::default()
        };
        for (li, layer) in cooked.layers.iter().enumerate() {
            for c in &layer.chunks {
                let key = ChunkKey {
                    layer: li as u16,
                    z: c.z,
                    x: c.x,
                };
                let Some(last) = last_lod(c) else {
                    draws.push(ChunkDraw { key, draw: None });
                    continue;
                };
                let state = self.chunks.get(&key);
                if let Some(state) = state {
                    for (lod, mesh) in &state.meshes {
                        if let Some(n) = stats.resident.get_mut(usize::from(*lod)) {
                            *n += 1;
                        }
                        let b = c.lods.get(usize::from(*lod)).map_or(0, |l| l.bytes);
                        let held = layer_mesh_resident_bytes(mesh);
                        stats.bytes += b;
                        stats.resident_bytes += held;
                        if *lod == last {
                            stats.coarsest_bytes += b;
                            stats.coarsest_resident_bytes += held;
                        }
                    }
                    stats.failed += state.failed.len() as u32;
                }
                let draw = want.get(&key).and_then(|&lod| {
                    let state = state?;
                    state
                        .meshes
                        .get(&lod)
                        .map(|m| (lod, m.clone()))
                        .or_else(|| state.meshes.get(&last).map(|m| (last, m.clone())))
                });
                if draw.is_some() {
                    stats.drawn += 1;
                }
                draws.push(ChunkDraw { key, draw });
            }
        }
        self.stats = stats;
        draws
    }
}

/// The bytes a layer chunk mesh holds: its [`regia_render::LabVertex`] vertices and `u32` indices, by
/// capacity ([`LayerStreamStats::resident_bytes`]).
#[must_use]
pub fn layer_mesh_resident_bytes(mesh: &RenderMesh) -> u64 {
    (mesh.vertices.capacity() * std::mem::size_of::<regia_render::LabVertex>()
        + mesh.indices.capacity() * std::mem::size_of::<u32>()) as u64
}

/// Loads one chunk LOD as [`drive_layer_stream`] does: through the asset server when there is one
/// (paks first, then the cooked cache), else from `cooked_dir`.
///
/// The stream holds the only CPU copy: the server's cooked bytes are read
/// ([`regia_asset::AssetServer::read_cooked`]) and decoded here, not loaded as a pinned server mesh,
/// which kept a second decoded copy of every LOD ever streamed for the life of the server, evicted
/// LODs included (ADR-8873).
///
/// # Errors
///
/// The read or decode error as text, or "no asset server and no cooked directory".
pub fn load_layer_chunk_mesh(
    assets: Option<&mut regia_asset::AssetServer>,
    cooked_dir: Option<&std::path::Path>,
    id: AssetId,
) -> Result<Arc<RenderMesh>, String> {
    let cpu = if let Some(server) = assets {
        let (bytes, _) = server.read_cooked(id).map_err(|e| e.to_string())?;
        let loaded = regia_asset::load_mesh_bytes(&bytes).map_err(|e| e.to_string())?;
        RenderMesh::from_cooked_rigid(&loaded.mesh)
    } else if let Some(dir) = cooked_dir {
        let path = dir.join(regia_asset::cooked_file_name(id));
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let loaded = regia_asset::load_mesh_bytes(&bytes).map_err(|e| e.to_string())?;
        RenderMesh::from_cooked_rigid(&loaded.mesh)
    } else {
        return Err("no asset server and no cooked directory".into());
    };
    Ok(Arc::new(cpu))
}

/// The `terrain-mesh-layers-cooked` migrator (format batch 2): a cooked `<map>.meshlayers` of an
/// older file version (1: canonical section version 1, chunk section version 2 without
/// `collider`) to the current one. The world-geometry loader migrates the header and the
/// canonical section (ADR-8816: the same value at the fixed-width layout, so the canonical
/// `content_hash` does not move); the chunk section, when the file has one, is decoded (a
/// version-2 section migrates with every layer's `collider` false, ADR-8872) and written at
/// [`PRESENTATION_VERSION`]; the presentation section is kept byte for byte. A current file comes
/// back unchanged. An upgraded file's input hash is invalidated: migrating the layout does not
/// recompute geometry, so an ordinary cook must apply current skylight cuts from source
/// (ADR-10346). This leaves the canonical `content_hash` and all chunk meshes intact.
///
/// # Errors
///
/// The file or its chunk section does not load (the message carries the error code).
pub fn upgrade_cooked_mesh_layers(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use regia_world_geom::cooked::{COOKED_FORMAT_VERSION, CookedMeshLayers, cooked_file_version};
    // The version on disk decides: `decode` hands back an older file already at the current header
    // and canonical versions (ADR-8816).
    let on_disk = cooked_file_version(bytes)?;
    let mut file = CookedMeshLayers::decode(bytes).map_err(|e| format!("{e} ({})", e.code()))?;
    if on_disk == Some(COOKED_FORMAT_VERSION) {
        return Ok(bytes.to_vec());
    }
    if let Some(chunks) =
        TerrainMeshLayersCooked::decode(&file.chunks).map_err(|e| format!("{e} ({})", e.code()))?
    {
        file.chunks = chunks.encode().map_err(|e| format!("{e} ({})", e.code()))?;
    }
    file.header.version = COOKED_FORMAT_VERSION;
    // Layout migration preserves old geometry, not a proof of a current cook. In particular,
    // a pre-skylight cache must miss the cook's input key even when its chunk files still exist.
    file.header.input_hash = [0; 32];
    file.encode().map_err(|e| format!("{e} ({})", e.code()))
}

/// Mesh keys of the LOD-0 chunk meshes [`reference_chunks_lod0`] puts in a
/// [`regia_render::FrameExport`]: `REFERENCE_CHUNK_TAG | layer << 32 | z << 16 | x` (layer below
/// 256, `x` and `z` below 65536). Beside [`super::REFERENCE_PIECE_TAG`], outside the renderer's
/// ephemeral and shadow LOD ranges (`0xFE..` and `0xFF..` in the top byte).
pub const REFERENCE_CHUNK_TAG: u64 = 0xFD4D_4300_0000_0000;

/// What [`reference_chunks_lod0`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReferenceChunks {
    /// Drawn chunks whose export instances now trace their LOD 0.
    pub swapped: u32,
    /// Drawn chunks already at LOD 0 (left as drawn).
    pub at_lod0: u32,
    /// Drawn chunks whose LOD 0 did not load (left at their drawn LOD; logged with the code
    /// `meshlayers_reference_lod0`).
    pub failed: u32,
}

/// Spec 26 § 2a.6 "Path tracer" (ADR-8872): the reference trace sees every layer chunk the frame
/// drew at LOD 0. The range is the drawn set: each [`TerrainLayerChunk`] entity whose mesh has
/// instances in `export` (the frame's resident chunks, within the stream's visible radius), so
/// the trace and the raster frame hold the same chunks. A chunk drawn at a coarser LOD has its
/// instances re-pointed at its cooked LOD 0, read the way the stream reads it (the asset server
/// when the world has one, else the layers' `chunk_dir`), under
/// [`REFERENCE_CHUNK_TAG`] keys. The owner is the stream's: the first terrain entity whose
/// [`super::TerrainMeshLayers`] has a presentation section. Without one the export is unchanged.
pub fn reference_chunks_lod0(world: &mut World, export: &mut regia_render::FrameExport) -> ReferenceChunks {
    let mut out = ReferenceChunks::default();
    let mut owners = world.query::<&super::TerrainMeshLayers>();
    let Some((cooked, chunk_dir)) = owners.iter(world).find_map(|l| {
        l.presentation_arc()
            .map(|p| (p, l.chunk_dir().map(std::path::Path::to_path_buf)))
    }) else {
        return out;
    };
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.mesh_layer_reference_chunks").entered();
    // (mesh key drawn, layer, x, z) of every drawn chunk, in entity order.
    let mut drawn: Vec<(Entity, u64, u8, TerrainLayerChunk)> = world
        .query::<(Entity, &TerrainLayerChunk, &regia_render::MeshRenderer)>()
        .iter(world)
        .map(|(e, c, r)| (e, regia_render::runtime::mesh_key(&r.mesh), c.lod, *c))
        .collect();
    drawn.sort_unstable_by_key(|d| d.0);
    let mut assets = world.get_resource_mut::<regia_asset::AssetServer>();
    for (_, key, lod, chunk) in drawn {
        if !export.instances.iter().any(|i| i.mesh_key == key) {
            continue;
        }
        if lod == 0 {
            out.at_lod0 += 1;
            continue;
        }
        let (layer, x, z) = (chunk.layer, chunk.node.x, chunk.node.z);
        let id = cooked
            .layers
            .get(usize::from(layer))
            .and_then(|l| {
                l.chunks
                    .binary_search_by_key(&(z, x), |c| (c.z, c.x))
                    .ok()
                    .map(|i| &l.chunks[i])
            })
            .and_then(|c| c.lods.first())
            .map(|l0| l0.mesh);
        let fits = layer < 256 && x < 1 << 16 && z < 1 << 16;
        let loaded = match id {
            Some(id) if fits => load_layer_chunk_mesh(assets.as_deref_mut(), chunk_dir.as_deref(), id),
            Some(_) => Err("chunk index outside the reference key range".to_string()),
            None => Err("no cooked LOD 0 for the chunk".to_string()),
        };
        match loaded {
            Ok(mesh) => {
                let lod0 =
                    REFERENCE_CHUNK_TAG | (u64::from(layer) << 32) | (u64::from(z) << 16) | u64::from(x);
                export.meshes.insert(lod0, mesh);
                for inst in export.instances.iter_mut().filter(|i| i.mesh_key == key) {
                    inst.mesh_key = lod0;
                }
                out.swapped += 1;
            }
            Err(error) => {
                tracing::warn!(target: "regia_terrain", layer, x, z, lod, code = "meshlayers_reference_lod0", %error, "chunk LOD 0 did not load for the reference trace; tracing the drawn LOD");
                out.failed += 1;
            }
        }
    }
    out
}

/// The engine's default rock, the material of a layer whose `material` is empty: an opaque
/// single-sided standard material, so the chunks draw with the forward set the frame warm builds
/// (ADR-8398).
#[must_use]
pub fn layer_chunk_material() -> Arc<regia_render::CpuMaterial> {
    Arc::new(regia_render::CpuMaterial {
        base_color: [0.32, 0.29, 0.26, 1.0],
        roughness: 0.9,
        metallic: 0.0,
        ..Default::default()
    })
}

/// The material `layer`'s chunks draw with (ADR-8451): the cooked material the cook resolved
/// ([`CookedLayer::material_id`]), else the default rock ([`layer_chunk_material`]). Through
/// `assets` it carries the material's texture slots; without a server it reads the cooked file
/// from `cooked_dir`, scalars only. A material that does not load is an error log with the code
/// `meshlayers_material_load`, and the layer draws the rock.
#[must_use]
pub fn layer_material(
    layer: &CookedLayer,
    assets: Option<&regia_asset::AssetServer>,
    cooked_dir: Option<&std::path::Path>,
) -> Arc<regia_render::CpuMaterial> {
    let Some(id) = layer.material_id else {
        return layer_chunk_material();
    };
    let loaded = if let Some(server) = assets {
        regia_render::cooked_model::load_cooked_standard_material(server, id).map_err(|e| e.to_string())
    } else if let Some(dir) = cooked_dir {
        let path = dir.join(regia_asset::cooked_file_name(id));
        std::fs::read(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|bytes| regia_asset::load_material_bytes(&bytes).map_err(|e| e.to_string()))
            .map(|(_, cooked)| regia_render::CpuMaterial::from_authored(&cooked.authored))
    } else {
        Err("no asset server and no cooked directory".into())
    };
    match loaded {
        Ok(material) => Arc::new(material),
        Err(error) => {
            tracing::error!(target: "regia_terrain", layer = %layer.id, material = %layer.material, code = "meshlayers_material_load", %error, "mesh layer material did not load; drawing the default rock");
            layer_chunk_material()
        }
    }
}

/// `Update` (added by `TerrainPlugin`): steps [`TerrainLayerStream`] for the first terrain entity
/// with cooked layers and spawns or despawns one entity per drawn chunk. Without the resource, or
/// without cooked layers, it does nothing.
pub fn drive_layer_stream(
    mut commands: Commands,
    stream: Option<ResMut<TerrainLayerStream>>,
    views: Option<Res<regia_scene::streaming::StreamViews>>,
    layers: Query<(Entity, &super::TerrainMeshLayers)>,
    assets: Option<ResMut<regia_asset::AssetServer>>,
) {
    let Some(mut stream) = stream else {
        return;
    };
    let stream = &mut *stream;
    let found = layers
        .iter()
        .find_map(|(e, l)| l.presentation_arc().map(|p| (e, l, p)));
    let owner = found.as_ref().map(|(e, l, _)| (*e, l.presentation_generation()));
    if owner != stream.owner {
        for (_, (entity, _)) in std::mem::take(&mut stream.entities) {
            commands.entity(entity).despawn();
        }
        for (_, fade) in std::mem::take(&mut stream.fades) {
            commands.entity(fade.outgoing).despawn();
        }
        stream.reset();
        stream.materials.clear();
        stream.owner = owner;
    }
    let Some((_, layer_comp, cooked)) = found else {
        return;
    };
    let sources: Vec<[f32; 2]> = views
        .map(|v| v.0.iter().map(|s| [s.pos[0] as f32, s.pos[2] as f32]).collect())
        .unwrap_or_default();
    let mut assets = assets;
    let cooked_dir = layer_comp.chunk_dir().map(std::path::Path::to_path_buf);
    let mut fetch = |id: AssetId| load_layer_chunk_mesh(assets.as_deref_mut(), cooked_dir.as_deref(), id);
    let draws = stream.step(&cooked, &sources, &mut fetch);
    if stream.materials.len() != cooked.layers.len() {
        stream.materials = cooked
            .layers
            .iter()
            .map(|l| layer_material(l, assets.as_deref(), cooked_dir.as_deref()))
            .collect();
    }
    let frames = stream.fade_frames;
    for draw in draws {
        let current = stream.entities.get(&draw.key).copied();
        match (current, draw.draw) {
            (Some((entity, lod)), Some((want, _))) if lod == want => {
                advance_fade(&mut commands, stream, draw.key, entity, frames);
            }
            (current, Some((lod, mesh))) => {
                let (Some(layer), Some(material)) = (
                    cooked.layers.get(usize::from(draw.key.layer)),
                    stream.materials.get(usize::from(draw.key.layer)).cloned(),
                ) else {
                    continue;
                };
                let mut e = commands.spawn((
                    regia_render::MeshRenderer::new(mesh, material),
                    regia_scene::transform::GlobalTransform(regia_core::glam::Affine3A::IDENTITY),
                    regia_scene::components::MapLayer(layer.map_layer),
                    TerrainLayerChunk {
                        layer: draw.key.layer,
                        node: cooked.grid.node(draw.key.x, draw.key.z),
                        lod,
                    },
                ));
                if !layer.cast_shadows {
                    e.insert(regia_render::NotShadowCaster);
                }
                let incoming = e.id();
                stream.entities.insert(draw.key, (incoming, lod));
                if let Some((entity, old_lod)) = current {
                    switch_lod(
                        &mut commands,
                        stream,
                        draw.key,
                        (entity, old_lod),
                        (incoming, lod),
                        frames,
                    );
                }
            }
            (Some((entity, _)), None) => {
                commands.entity(entity).despawn();
                stream.entities.remove(&draw.key);
                if let Some(fade) = stream.fades.remove(&draw.key) {
                    commands.entity(fade.outgoing).despawn();
                }
            }
            (None, None) => {}
        }
    }
}

/// The dither share of a fade `frame` frames into a fade of `frames` frames.
fn fade_share(frame: u32, frames: u32) -> f32 {
    frame as f32 / frames as f32
}

/// Starts the cross-fade of a LOD switch (or switches at once when `frames <= 1`): `current` was
/// drawn whole or as a fade's incoming LOD, `incoming` was just spawned.
fn switch_lod(
    commands: &mut Commands,
    stream: &mut TerrainLayerStream,
    key: ChunkKey,
    current: (Entity, u8),
    incoming: (Entity, u8),
    frames: u32,
) {
    let previous = stream.fades.remove(&key);
    if frames <= 1 {
        commands.entity(current.0).despawn();
        if let Some(fade) = previous {
            commands.entity(fade.outgoing).despawn();
        }
        return;
    }
    let frame = match previous {
        // Back to the LOD that was fading out: it had `1 - c` of the pixels and goes on from
        // there. Its old entity goes; the new one takes its share.
        Some(fade) if fade.outgoing_lod == incoming.1 => {
            commands.entity(fade.outgoing).despawn();
            frames.saturating_sub(fade.frame).clamp(1, frames - 1)
        }
        // A third LOD: the one fading out goes now, the current one fades out from whole.
        Some(fade) => {
            commands.entity(fade.outgoing).despawn();
            1
        }
        None => 1,
    };
    let c = fade_share(frame, frames);
    commands.entity(incoming.0).insert(regia_render::MeshLodFade(c));
    commands.entity(current.0).insert(regia_render::MeshLodFade(-c));
    stream.fades.insert(
        key,
        ChunkFade {
            outgoing: current.0,
            outgoing_lod: current.1,
            frame,
        },
    );
}

/// One more frame of `key`'s fade, if it has one: the next shares, or at the end the incoming LOD
/// whole and the outgoing entity gone.
fn advance_fade(
    commands: &mut Commands,
    stream: &mut TerrainLayerStream,
    key: ChunkKey,
    incoming: Entity,
    frames: u32,
) {
    let Some(fade) = stream.fades.get_mut(&key) else {
        return;
    };
    fade.frame += 1;
    if fade.frame >= frames {
        let outgoing = fade.outgoing;
        stream.fades.remove(&key);
        commands.entity(outgoing).despawn();
        commands.entity(incoming).remove::<regia_render::MeshLodFade>();
        return;
    }
    let c = fade_share(fade.frame, frames);
    let outgoing = fade.outgoing;
    commands.entity(incoming).insert(regia_render::MeshLodFade(c));
    commands.entity(outgoing).insert(regia_render::MeshLodFade(-c));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> LayerChunkGrid {
        LayerChunkGrid::new(1024.0, 4, 16, 4.0, [0.0, 0.0], 64.0)
    }

    #[test]
    fn chunk_level_is_the_coarsest_node_under_chunk_m() {
        let g = grid();
        // Level 0 is 64 m, level 1 128 m.
        assert_eq!(g.chunk_level, 0);
        assert_eq!(LayerChunkGrid::chunk_level_for(1024.0, 4, 300.0), 2);
        assert_eq!(LayerChunkGrid::chunk_level_for(1024.0, 4, 10.0), 0);
        assert_eq!(g.chunks_per_side(), 16);
        assert_eq!(g.chunk_at([-512.0, -512.0]), Some((0, 0)));
        assert_eq!(g.chunk_at([511.9, 0.0]), Some((15, 8)));
        assert_eq!(g.chunk_at([512.0, 0.0]), None);
    }

    #[test]
    fn presentation_round_trips_and_refuses_newer() {
        let p = TerrainMeshLayersCooked {
            version: PRESENTATION_VERSION,
            chunk_importer: 0,
            grid: grid(),
            layers: vec![CookedLayer {
                id: "a".into(),
                map_layer: 1,
                cast_shadows: false,
                collider: true,
                material: String::new(),
                material_id: None,
                chunks: vec![CookedLayerChunk {
                    x: 1,
                    z: 2,
                    bounds: [[0.0; 3], [1.0; 3]],
                    lods: vec![CookedChunkLod {
                        mesh: AssetId::default(),
                        triangles: 4,
                        bytes: 100,
                        error_m: 0.0,
                    }],
                }],
            }],
        };
        let bytes = p.encode().unwrap_or_default();
        assert_eq!(TerrainMeshLayersCooked::decode(&bytes), Ok(Some(p.clone())));
        assert_eq!(TerrainMeshLayersCooked::decode(&[]), Ok(None));
        let mut newer = p.clone();
        newer.version = PRESENTATION_VERSION + 1;
        let err = TerrainMeshLayersCooked::decode(&newer.encode().unwrap_or_default());
        assert_eq!(err.map_err(|e| e.code()), Err("meshlayers_presentation_newer"));
        // Version 1 (before `material_id`) is stale; version 2 migrates (below).
        let mut older = p.clone();
        older.version = 1;
        let err = TerrainMeshLayersCooked::decode(&older.encode().unwrap_or_default());
        assert_eq!(err.map_err(|e| e.code()), Err("meshlayers_presentation_stale"));
        let err = TerrainMeshLayersCooked::decode(&bytes[..bytes.len() - 1]);
        assert_eq!(err.map_err(|e| e.code()), Err("meshlayers_presentation_corrupt"));
    }

    /// ADR-8872 (format batch 2): a version-2 section (no `collider`, what v0.6.0 cooked) loads as
    /// version 3 with every layer's `collider` false and everything else equal; it re-encodes as
    /// version 3 and round-trips.
    #[test]
    fn a_version_2_section_migrates_with_collider_false() {
        #[derive(Serialize)]
        struct LayerV2<'a> {
            id: &'a str,
            map_layer: u8,
            cast_shadows: bool,
            material: &'a str,
            material_id: Option<AssetId>,
            chunks: Vec<CookedLayerChunk>,
        }
        #[derive(Serialize)]
        struct SectionV2<'a> {
            version: u32,
            grid: LayerChunkGrid,
            layers: Vec<LayerV2<'a>>,
            chunk_importer: u32,
        }
        let chunk = CookedLayerChunk {
            x: 3,
            z: 1,
            bounds: [[0.0; 3], [1.0; 3]],
            lods: vec![CookedChunkLod {
                mesh: AssetId::sub_asset(AssetId::default(), "meshlayers/a/3_1/lod0"),
                triangles: 4,
                bytes: 100,
                error_m: 0.25,
            }],
        };
        let material_id = Some(AssetId::sub_asset(AssetId::default(), "materials/rock.mat.ron"));
        let v2 = SectionV2 {
            version: 2,
            grid: grid(),
            layers: vec![
                LayerV2 {
                    id: "a",
                    map_layer: 2,
                    cast_shadows: false,
                    material: "materials/rock.mat.ron",
                    material_id,
                    chunks: vec![chunk.clone()],
                },
                LayerV2 {
                    id: "b",
                    map_layer: 0,
                    cast_shadows: true,
                    material: "",
                    material_id: None,
                    chunks: Vec::new(),
                },
            ],
            chunk_importer: 7,
        };
        let bytes = postcard::to_allocvec(&v2).unwrap_or_default();
        let got = TerrainMeshLayersCooked::decode(&bytes);
        let want = TerrainMeshLayersCooked {
            version: PRESENTATION_VERSION,
            grid: grid(),
            layers: vec![
                CookedLayer {
                    id: "a".into(),
                    map_layer: 2,
                    cast_shadows: false,
                    collider: false,
                    material: "materials/rock.mat.ron".into(),
                    material_id,
                    chunks: vec![chunk],
                },
                CookedLayer {
                    id: "b".into(),
                    map_layer: 0,
                    cast_shadows: true,
                    collider: false,
                    material: String::new(),
                    material_id: None,
                    chunks: Vec::new(),
                },
            ],
            chunk_importer: 7,
        };
        assert_eq!(got, Ok(Some(want.clone())));
        let again = want.encode().unwrap_or_default();
        assert_eq!(again.first(), Some(&3), "re-encoded as version 3");
        assert_eq!(TerrainMeshLayersCooked::decode(&again), Ok(Some(want)));
        // A version-2 section with a byte missing is corrupt, not stale.
        let err = TerrainMeshLayersCooked::decode(&bytes[..bytes.len() - 1]);
        assert_eq!(err.map_err(|e| e.code()), Err("meshlayers_presentation_corrupt"));
    }
}
