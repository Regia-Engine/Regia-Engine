// SPDX-License-Identifier: MPL-2.0
//! Terrain and tunnels: CDLOD heightfield rendering over the same 16-bit heightfield the game's
//! simulation reads, splat-mapped material layers, seeded deterministic scatter, crater/scorch
//! deformation hooks, holes for tunnel entrances, tunnel render meshes built from collision
//! triangles, and a shoreline water plane.
//!
//! Specification: `docs/specs/26-terrain-vfx-highend.md` §§ 1–2 (terrain rows) and
//! `docs/specs/25-sim-bridge-and-do27.md` § 4 (Terrain row). Layer files are format v4
//! (`docs/formats/terrain.md`); algorithm sources are listed in `docs/provenance.md`.
//!
//! # Sim / render agreement
//!
//! The single source of truth is the cooked [`regia_asset::Heightfield`] (a 16-bit grayscale
//! PNG at import time) plus `world_width` and height scale/offset. [`TerrainMap`] never resamples
//! or smooths it: the base surface is the bilinear interpolation of the quantised samples — the
//! same math the simulation's terrain sampler uses — so render and sim agree by construction
//! up to the triangle interpolation of the rendered grid (bounded and tested at ≤ 1 cm on
//! smooth maps; see the tests of [`cdlod`]).
//!
//! # GPU integration contract (render hook)
//!
//! The offscreen hook ([`gpu::TerrainRenderer`]) still draws into `Rgba8UnormSrgb` plus
//! `Depth32Float`. The runtime frame uses [`gpu::TerrainSharedDraw`]: the same heightfield
//! is recorded into `FrameRenderer`'s depth prepass, directional cascades, HDR colour,
//! motion vectors and the decal receiver (layer 0, the heightfield normal). That path
//! writes scene radiance when the frame is linear HDR.
//!
//! # How the pieces fit
//!
//! A [`Terrain`] component holds the source-of-truth [`TerrainMap`] (16-bit heightfield +
//! world mapping), a [`CdlodConfig`], the min/max pyramid, the material [`layers`], optional
//! [`holes`] and a [`WaterPlane`]. [`TerrainPlugin`] refreshes CDLOD [`select_nodes`] into a
//! [`TerrainDrawList`] when the view moves; a held view keeps the list. The draw path selects
//! its own nodes. Deformation ([`deform`]) writes craters/scorches into the map
//! and asks the GPU mirror to update; [`brush`] sculpts and paints it (strokes, undo records and
//! the incremental re-cook live in [`edit`]); [`scatter`] places deterministic instances, and [`tunnel`] /
//! [`mesh_tunnel`] build tunnel geometry from collision triangles.
//!
//! See `book/user-guide/terrain-vfx.md` and `book/agents/tunnels.md`.

pub mod bounce;
pub mod brush;
pub mod cdlod;
pub mod collision;
pub mod deform;
pub mod do27_gap;
pub mod edit;
pub mod generate;
pub mod gpu;
pub mod grass;
pub mod height_mips;
pub mod holes;
pub mod hot;
pub mod layer_paint;
pub mod layers;
pub mod lod_error;
pub mod lod_seams;
pub mod material;
pub mod mesh_tunnel;
pub mod mist;
pub mod noise;
pub mod pick;
pub mod road;
pub mod sampler;
pub mod scatter;
pub mod sdf;
pub mod sim;
pub mod slope_mips;
pub mod stream;
pub mod stream_hooks;
pub mod tunnel;
pub mod volume_mesh;
pub mod water;
pub mod water_body;
// Grounded placement of authored points on a heightfield (ADR-8036).
pub mod placement;

use regia_app::prelude::*;

pub use brush::{Brush, BrushMode, BrushShape, BrushTarget, BrushThread, StrokeRecord};
pub use cdlod::{
    CdlodConfig, DEFAULT_GRID_SIZE, LodSelection, MinMaxPyramid, NodeDraw, QuadNode, cascade_min_level,
    eval_node_vertex_filtered, node_world_size, quadtree_span, select_nodes, select_nodes_min_level,
    terrain_surface_height, vertex_morph_t,
};
pub use collision::{CollisionChunk, CollisionHeightfield, SaveDrift};
pub use deform::{
    Crater, DirtyRect, MAX_SCORCH_MARKS, ScorchList, ScorchMark, apply_crater, crater_dirty_rect,
};
pub use do27_gap::{GapShot, GapSun, render_gap_shot};
pub use edit::{
    EDIT_CELL_SIZE, HeightPatchUndo, MAX_COOK_TILE_SIZE, RecookError, STROKE_LOG_CAP, StrokeUndo,
    TerrainBrushPreview, TerrainEditError, TerrainEdits, TerrainPaint, TerrainStrokeLog, apply_height_patch,
    apply_stroke, brush_outline, cook_all_tiles, cook_heightfield_tile, raycast as raycast_terrain,
    recook_tiles,
};
pub use generate::{ErosionParams, droplet_erode, ridged_fbm, synthetic_mountains, thermal_erode};
// The terrain's anisotropic tap path per adapter (ADR-3957).
pub use gpu::{
    ANISO_FOOTPRINT_ENV, ANISO_LOD_OFFSET_ENV, AnisotropyPath, MALI_ANISO_LOD_OFFSET, MAX_ANISO_LOD_OFFSET,
    RADV_ANISO_LOD_OFFSET, anisotropy_footprint_for, anisotropy_lod_offset, anisotropy_lod_offset_for,
    anisotropy_lod_offset_for_device, anisotropy_path,
};
pub use gpu::{
    BandDrawCounts, CLIFF_DENSE_BAND_M, CLIFF_DENSE_RADIUS_M, CLIFF_RELIEF_MIN_SLOPE_RAD, GrassLayer,
    ImpostorLayer, RenderedSurface, SkyAmbient, TerrainDrawStats, TerrainRelief, TerrainSharedDraw,
    TerrainVtHandle, TerrainWarm, VirtualTextureSettings, VtCache, VtDepthTest, VtStats,
    terrain_warm_formats, terrain_warm_job, warm_terrain_pipelines,
};
pub use grass::{
    GRASS_FORMAT_VERSION, GrassBudget, GrassClass, GrassError, GrassLayerRule, GrassMask, GrassMaskInputs,
    GrassQuality, GrassReport, GrassRequest, GrassSet, TerrainGrassHandle, grass_migration_sources,
    grass_schema_fingerprint,
};
pub use height_mips::{HeightMips, vertex_mip};
pub use holes::{
    HOLE_WINDOW_TEXELS, HOLES_FORMAT_VERSION, HoleEllipse, HoleRaster, HoleShape, HoleWindow, HolesFileError,
    HolesMask, MAX_HOLE_RASTER_SIDE, MAX_HOLE_WINDOWS, NodeHoleState, TerrainHoles, TerrainHolesState,
    holes_file_version, holes_migration_sources, holes_ron_path, holes_schema_fingerprint, load_holes_ron,
};
// Named holes (holes.ron v2) and runtime height patches (ADR-1595, DO-4).
pub use holes::{HolesError, MAX_HOLE_ID_LEN, NamedHole, valid_hole_id};
// Grid origin variants of the hole node tests (ADR-8179).
pub use holes::{build_node_indices_at, node_hole_state_at};
// The passes the terrain draw ran last frame, for `terrain.seam_report` (ADR-8470).
pub use gpu::pass_selections::{DrawnPass, TerrainDrawPasses};
// Lattice hole windows of the filled holes (spec 26 § 2a.4 item 2, ADR-8470).
pub use holes::{
    LATTICE_CUT_LOWER, LATTICE_CUT_UPPER, LATTICE_NEAR_RING, LATTICE_RING_FADE_CELLS, LATTICE_WINDOW_CELLS,
    LATTICE_WINDOW_MODE, LatticeHoleCut, LatticeHoleWindows, LatticeWindow, build_node_indices_lattice_at,
    node_hole_state_lattice_at,
};
pub use hot::{ScatterSets, TerrainLayersSource, reload_terrain_files};
pub use layer_paint::{LAYER_WEIGHTS_FORMAT_VERSION, LayerWeightError, LayerWeightMap, layer_weights_path};
pub use layers::{
    LayerDesc, MAX_LAYERS, TERRAIN_LAYERS_FORMAT_VERSION, TerrainLayers, terrain_layers_migration_sources,
    terrain_layers_schema_fingerprint,
};
pub use runtime_patch::{
    DEFAULT_PATCH_BUDGET, HeightBand, HeightPatch, HeightPatchError, MAX_PATCH_BUDGET, MAX_PATCH_SAMPLES,
    MAX_QUEUED_PATCH_SAMPLES, PatchFrameStats, PatchQueueState, PatchTarget, TerrainHeightPatches,
    TerrainHeightsChanged, apply_terrain_height_patches,
};
// The DO27 contract's millimetre edits over the patch queue (spec 25 § 7.4, ADR-7585).
pub use runtime_patch::{
    HeightEdit, HeightEditError, MAX_EDIT_SIDE, MAX_EDITS_PER_FRAME, TerrainHeightEdits,
};
// The coarse-grid resample rule for height patches (ADR-8179).
pub use runtime_patch::resample_cell_grid_mm;
// Effective layer albedo report (ADR-1425).
pub use layers::{LayerAlbedo, MIN_GROUND_ALBEDO, layer_albedo_report};
// The per-layer weather response of `.terrain.layers.ron` v4 (spec 99 § 7.3).
pub use layers::LayerWeather;
pub use lod_error::{LodError, lod_height_error, rendered_height};
pub use lod_seams::{SeamReport, lod_seam_report};
pub use material::{
    CookedMaterialKtx2, LayerStyle, LayerTextures, MacroBake, MaterialError, MaterialParams, Rgba8Image,
    TerrainMaterial, linear_to_srgb8, srgb8_to_linear,
};
pub use mesh_tunnel::{MeshTunnel, MeshTunnelError};
pub use mist::mist_grid_from_map;
pub use pick::{TerrainHolesRef, TerrainRayHit, pick_terrain, raycast_with_holes, terrain_holes};
pub use road::{RoadDecal, RoadRibbon, on_segment};
pub use sampler::TerrainMap;
pub use scatter::caster::{CasterPart, ScatterCasterHint, caster_coverage, reduce_cards};
pub use scatter::clump::{ClumpSettings, clump_asset, coarse_lod, with_coarse_lod};
pub use scatter::colliders::{
    BlockedBody, ColliderSphere, MeshExtent, SceneryCollider, SceneryColliderSettings, SphereBlockerSettings,
    SphereBlockers, bake_scenery_colliders,
};
pub use scatter::contact::{ContactMap, ContactRaster};
pub use scatter::control::{ScatterClassReport, ScatterControl, ScatterReport};
pub use scatter::detail::ScatterDetail;
pub use scatter::field::ScatterMeshPart;
pub use scatter::field::{
    LOD_FADE_FRAMES, LOD_HYSTERESIS, MAX_SCATTER_LODS, ScatterBuildOptions, ScatterField, ScatterFrameStats,
    ScatterLibrary, ScatterMeshAsset, ScatterMeshLod, ScatterSource, ScatterStreamStats, ScatterView,
};
pub use scatter::impostor::{ImpostorAtlas, ImpostorCard, ImpostorInstance, ImpostorProto, ImpostorSettings};
pub use scatter::painted::PAINTED_FLAG_STILL;
pub use scatter::painted::{
    MAX_PAINT_DENSITY, MAX_PAINTED_INSTANCES, MAX_SCATTER_PAINT_BYTES, MAX_STROKE_CANDIDATES,
    PAINT_TILE_SIZE_M, PaintedClassReport, PaintedInstance, PaintedScatter, SCATTER_PAINT_FORMAT_VERSION,
    SCATTER_PAINT_MAGIC, ScatterBrush, ScatterBrushMode, ScatterDelta, ScatterPaintError, ScatterPaintInputs,
    ScatterPaintSet, ScatterStroke, TerrainScatterPaint, apply_scatter_stroke, load_scatter_paint,
    paintable_scatter_set, reseat_painted_scatter, reseat_terrain_scatter, scatter_paint_migration_sources,
    scatter_paint_path, scatter_paint_schema_fingerprint, scatter_stroke,
};
pub use scatter::placement::{DensityMap, DensityMaps, PlacedInstance, PlacementInputs, place_set};
pub use scatter::placement::{
    PlacementGrid, PlacementOrigin, WorldAnchor, place_class_cell_with_origin, place_set_with_origin,
};
pub use scatter::rock::{RockPalette, RockStyle, procedural_rock, standin_library};
pub use scatter::runtime::{RuntimeScatter, ScatterRuntimePlugin, extract_runtime_scatter, scatter_view_of};
// Runtime height edits: the virtual-texture counters and the regional scatter re-sit (ADR-7585).
pub use gpu::{VT_EDIT_RECTS_MAX, VtEditCounters};
pub use scatter::field::ScatterResitStats;
pub use scatter::runtime::resit_runtime_scatter;
pub use scatter::season::{LEAF_RANK_SCALE, ScatterSeason, leaf_present, leaf_rank};
pub use scatter::{
    COOKED_SCATTER_VERSION, SCATTER_FORMAT_VERSION, SCATTER_RULE_V1_VERSION, SCATTER_SET_VERSION,
    SCATTER_TYPE_PATH, ScatterClass, ScatterError, ScatterExclusion, ScatterInstance, ScatterMeshRef,
    ScatterRule, ScatterSet, place, place_cell, scatter_migration_sources, scatter_schema_fingerprint,
};
pub use sim::{
    AgreementFlag, CLIFF_DELTA_M, DO27_PACKAGE_BUDGET_BYTES, DO27_SIM_RESOLUTION, DO27_VISUAL_RESOLUTION,
    DO27_WORLD_WIDTH_M, DRIVEABLE_DELTA_M, DRIVEABLE_SLOPE_RAD, GpuBudget, SimGround, agreement_flag,
    do27_gpu_budgets, visual_sim_delta,
};
pub use stream::{
    CookedHeightfieldTile, CookedTiledHeightfield, DEFAULT_TILE_SIZE, MemoryTileSource, PakTileSource,
    StreamError, TAIL_MIP, TILED_HEIGHTFIELD_FORMAT_VERSION, TILED_HEIGHTFIELD_TYPE_PATH, TileCache,
    TileFormat, TileKey, TileSource, TiledTexture, decode_cooked_tiled_heightfield,
    load_tiled_heightfield_bytes, stitch_tiled_heightfield, tail_asset_id, tail_asset_path, tail_key,
    tile_asset_id, tile_asset_path,
};
pub use stream_hooks::{TerrainTileStats, TerrainTileStream};
pub use tunnel::{
    CollisionProvenance, TunnelDetail, TunnelError, TunnelMeshOptions, tunnel_mesh_from_collision_with,
};
pub use water::{WaterError, WaterPlane};
pub use water::{WaterProbe, WaterShade, WaterShadeInput};
pub use water_body::{CausticStats, WaterCaustics, WaterSunShadow};
pub use water_body::{FftOcean, OceanSpectrum, Swell, SwellWave, TerrainWater, WaterHit, WaterSurface};
pub use water_body::{
    IceShelf, RibbonPoint, WaterBody, WaterBodyError, WaterClock, WaterLook, WaterObstacle, WaterPlugin,
    WaterRenderer, WaterSettings, WaterShape, WaterState, WaterTier,
};
// Ground variation and late-winter surfaces (ADR-1408).
pub mod ground_look;
pub use ground_look::{
    GroundLookHandle, TerrainGroundLook, TerrainGroundLookControl, TerrainSurfaceWeatherReport,
};
// Far-field layer weight selection (CPU mirror of the shader's math, ADR-1446).
pub mod far_layers;
// Terrain debug views switchable on a running draw (ADR-1447).
pub mod debug_view;
pub use debug_view::{
    TERRAIN_DEBUG_VIEWS, TerrainDebugView, TerrainDebugViewControl, TerrainDebugViewHandle,
};
// Far-field splat reconstruction (CPU mirror of the shader's math, ADR-1456).
pub mod far_splat;
// Runtime height patches (ADR-1595, DO-4).
pub mod runtime_patch;
// Terrain editing completion: heightmap export/import, ramp helper, shared wind (ADR-1853).
pub mod height_io;
pub mod wind;
pub use edit::ramp_mode;
pub use height_io::{
    HeightExport, HeightImport, HeightIoError, MAX_IMPORT_UNDO_SAMPLES, RawEndian, decode_r16, encode_r16,
    export_heights, import_heights, square_side,
};
pub use regia_render::wind_natural::{self, NaturalBlade, WindStyle, natural_wave};
pub use wind::{SCENE_WIND_REFERENCE_M_PER_S, TerrainWind, couple_scene_wind, sync_terrain_wind};
// Terrain edits through the floating origin (ADR-2435).
pub use edit::{
    StrokeSessionError, TerrainStrokeSession, apply_stroke_local, brush_outline_local,
    local_height_to_terrain, local_to_terrain, ramp_mode_local, raycast_local, terrain_to_local,
};
// The terrain's caster proxy for the ray-traced sun (ADR-2310).
pub mod trace_proxy;
pub use trace_proxy::{
    TerrainTraceProxyControl, TerrainTraceProxyHandle, TraceProxy, TraceProxySettings, TraceProxyStats,
};
// Terrain editing remainder: the terrain a level names, painted grass density (ADR-2411).
pub mod grass_density;
pub mod level_terrain;
pub use gpu::grass::{GRASS_BLADE_THICKNESS_MM, foliage_transmission_words};
pub use grass_density::{
    GRASS_DENSITY_FORMAT_VERSION, GrassDensityError, GrassDensityFileRefused, GrassDensityPaint,
    GrassDensityStroke, GrassDensityUndo, grass_density_path,
};
pub use level_terrain::{LevelGround, load_level_ground, meshlayers_source_path};
pub use level_terrain::{
    LevelTerrainError, LevelTerrainFiles, LevelTerrainLoad, LevelTerrainPlugin, LevelTerrainRoots,
    LevelTerrainSource, LevelTerrainSpawn, LevelTerrainStatus, despawn_other_levels, load_level_terrain,
    spawn_level_terrain,
};
pub use level_terrain::{load_mesh_layers, meshlayers_cooked_path, root_level_for};
// Mesh-layer seam cook: zipper, collar, ring attach (ADR-8291, spec 26 § 2a).
pub mod mesh_layer;
pub use mesh_layer::{
    BandMesh, BandVertex, CopiedTri, LayerMouth, RingAttach, SeamCookError, SeamMouthReport, SurfaceOwner,
    TerrainMeshLayers, ZipperInput, ZipperOutput, ZipperParams, attach_ring, attach_ring_detail,
    collar_triangles, heightfield_triangles, seam_report, seam_report_with_mips, surface_owner, zipper,
};
// Mesh-layer mouth bands (spec 26 § 2a.4 step 6, ADR-8569).
pub use mesh_layer::{BandPlacement, MouthBands, attach_ring_on};
// Mesh-layer chunk streaming and LOD (spec 26 § 2a.5, ADR-8398).
pub use mesh_layer::{
    ChunkDraw, ChunkKey, CookedChunkLod, CookedLayer, CookedLayerChunk, DEFAULT_LAYER_FADE_FRAMES,
    LayerChunkGrid, LayerPresentationError, LayerStreamStats, TerrainLayerChunk, TerrainLayerStream,
    TerrainMeshLayersCooked, chunk_lod_asset_id, chunk_lod_asset_path, layer_mesh_resident_bytes,
    load_layer_chunk_mesh,
};
// Mesh-layer joins: band picks and canonical collider pieces (spec 26 § 2a.6, § 2a.8, ADR-8537).
pub use mesh_layer::{BandHit, LayerColliderPiece, LayerReferencePiece, REFERENCE_PIECE_TAG};
// Mesh-layer chunks at LOD 0 in the reference trace (spec 26 § 2a.6, ADR-8872).
pub use mesh_layer::{REFERENCE_CHUNK_TAG, ReferenceChunks, reference_chunks_lod0};
// The cooked `<map>.meshlayers` v1 -> v2 migrator (format batch 2, ADR-8872).
pub use mesh_layer::upgrade_cooked_mesh_layers;

/// Errors returned by terrain building, loading and rendering.
#[derive(Debug, thiserror::Error)]
pub enum TerrainError {
    /// The heightfield or its world mapping is unusable (empty grid, non-finite or non-positive
    /// world width, non-finite height scale/offset).
    #[error("invalid terrain data: {0}")]
    InvalidTerrain(#[from] sampler::TerrainMapError),
    /// The CDLOD configuration is invalid.
    #[error("invalid CDLOD configuration: {0}")]
    InvalidCdlod(#[from] cdlod::CdlodError),
    /// The material layer stack is invalid.
    #[error("invalid terrain layers: {0}")]
    InvalidLayers(#[from] layers::LayersError),
    /// The photoreal material is invalid.
    #[error("invalid terrain material: {0}")]
    InvalidMaterial(#[from] material::MaterialError),
    /// A scatter rule is invalid.
    #[error("invalid scatter rule: {0}")]
    InvalidScatter(#[from] scatter::ScatterError),
    /// The holes mask is invalid.
    #[error("invalid holes mask: {0}")]
    InvalidHoles(#[from] holes::HolesError),
    /// The deformation parameters are invalid.
    #[error("invalid deformation: {0}")]
    InvalidDeform(#[from] deform::DeformError),
    /// The tunnel mesh inputs are invalid.
    #[error("invalid tunnel input: {0}")]
    InvalidTunnel(#[from] tunnel::TunnelError),
    /// A mesh tunnel (hole + mesh + collision) is invalid.
    #[error("invalid mesh tunnel: {0}")]
    InvalidMeshTunnel(#[from] mesh_tunnel::MeshTunnelError),
    /// The water plane parameters are invalid.
    #[error("invalid water plane: {0}")]
    InvalidWater(#[from] water::WaterError),
    /// A RON asset could not be parsed.
    #[error("RON parse error: {0}")]
    Ron(#[from] ron::error::SpannedError),
    /// A RON asset could not be serialised.
    #[error("RON serialize error: {0}")]
    RonSer(#[from] ron::error::Error),
    /// A GPU operation failed.
    #[error("GPU error: {0}")]
    Gpu(#[from] regia_render::RenderError),
    /// The terrain virtual texture's settings do not fit the map or the device.
    #[error("terrain virtual texture: {0}")]
    VirtualTexture(String),
}

/// One terrain in the world: the heightfield source of truth, the CDLOD configuration and the
/// optional dressing (holes, layers, water).
///
/// The GPU side ([`gpu::TerrainGpu`]) is built from this component; after deformation the
/// component's [`TerrainMap`] stays authoritative and the GPU mirror is updated through
/// [`gpu::TerrainGpu::update_heights`].
#[derive(Component, Debug, Clone)]
pub struct Terrain {
    /// Heightfield plus world mapping (source of truth, deformations apply here). A direct
    /// deformation through [`apply_crater`] must also refresh [`Terrain::minmax`]
    /// (`terrain.minmax.update_region(&terrain.map, &dirty)`): picks and CDLOD culling read
    /// the pyramid, so a crater applied to the map alone stays invisible to them.
    pub map: TerrainMap,
    /// LOD quadtree configuration (grid size per node, ranges, morph band).
    pub cdlod: CdlodConfig,
    /// Quadtree depth: `root_level + 1` levels exist, level 0 is the finest.
    pub root_level: u32,
    /// Height min/max pyramid used for frustum culling and LOD selection bounds.
    pub minmax: MinMaxPyramid,
    /// Material layer stack (splat).
    pub layers: TerrainLayers,
    /// Holes punched through the surface (tunnel entrances).
    pub holes: Option<HolesMask>,
    /// Shoreline water plane, if this map has water.
    pub water: Option<WaterPlane>,
}

impl Terrain {
    /// Builds a terrain from a cooked heightfield and its world mapping, computing the min/max
    /// pyramid.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] if the heightfield or the parameters are invalid (see
    /// [`TerrainMap::new`] and [`CdlodConfig::validate`]).
    pub fn new(
        heightfield: &regia_asset::Heightfield,
        world_width: f32,
        height_scale: f32,
        height_offset: f32,
        cdlod: CdlodConfig,
        root_level: u32,
        layers: TerrainLayers,
    ) -> Result<Self, TerrainError> {
        let map = TerrainMap::new(heightfield, world_width, height_scale, height_offset)?;
        cdlod.validate()?;
        let minmax = MinMaxPyramid::build(&map, root_level)?;
        Ok(Self {
            map,
            cdlod,
            root_level,
            minmax,
            layers,
            holes: None,
            water: None,
        })
    }
}

/// LOD selection of every [`Terrain`] in the world, filled by [`TerrainPlugin`] when the
/// view's XZ or the terrain inputs change. A held view keeps the previous list. The draw
/// path selects its own nodes; hosts may still read this one.
#[derive(Resource, Debug, Default)]
pub struct TerrainDrawList {
    /// One selection per terrain entity, in query order.
    pub selections: Vec<LodSelection>,
}

/// Last inputs `update_terrain_selection` built, plus the reused walk stack.
#[derive(Resource, Default)]
struct TerrainSelectionMemo {
    eye: [u32; 2],
    stamp: u64,
    count: u32,
    ready: bool,
    stack: Vec<QuadNode>,
}

/// Times [`update_terrain_selection`] actually walked the quadtree (tests).
#[cfg(test)]
static TERRAIN_SELECTION_BUILDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Camera + environment the terrain should be selected and drawn with. Set this resource (or
/// build a [`gpu::TerrainFrame`] directly when integrating with the M5 render graph).
#[derive(Resource, Debug, Clone, Copy)]
pub struct TerrainView {
    /// World-space eye point.
    pub position: [f32; 3],
    /// Point the camera looks at.
    pub target: [f32; 3],
}

/// Terrain system configuration resource.
#[derive(Resource, Debug, Clone)]
pub struct TerrainSettings {
    /// Number of selections kept before the list is pruned.
    pub max_selections: usize,
}

impl Default for TerrainSettings {
    fn default() -> Self {
        Self { max_selections: 64 }
    }
}

/// `Render`: refresh CDLOD selection when the eye XZ or the terrain inputs changed.
/// A held view keeps the list and its node buffers.
#[tracing::instrument(skip_all, target = "regia_terrain")]
fn update_terrain_selection(
    view: Option<Res<TerrainView>>,
    terrains: Query<&Terrain>,
    settings: Res<TerrainSettings>,
    mut list: ResMut<TerrainDrawList>,
    mut memo: ResMut<TerrainSelectionMemo>,
) {
    let Some(view) = view else { return };
    let eye = [view.position[0], view.position[2]];
    let mut stamp = settings.max_selections as u64;
    let mut count = 0u32;
    for terrain in &terrains {
        count += 1;
        stamp = stamp
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(terrain.map.world_width().to_bits() as u64);
        stamp = stamp
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(u64::from(terrain.map.width()));
        stamp = stamp
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(u64::from(terrain.map.height()));
        stamp = stamp
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(u64::from(terrain.root_level));
        stamp = stamp
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(u64::from(terrain.cdlod.range_factor.to_bits()));
        stamp = stamp
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(u64::from(terrain.cdlod.morph_fraction.to_bits()));
        stamp = stamp
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(u64::from(terrain.cdlod.grid_size));
        if count as usize >= settings.max_selections {
            break;
        }
    }
    let kept = count.min(settings.max_selections as u32);
    let eye_bits = [eye[0].to_bits(), eye[1].to_bits()];
    if memo.ready
        && memo.eye == eye_bits
        && memo.stamp == stamp
        && memo.count == kept
        && list.selections.len() == kept as usize
    {
        return;
    }
    if list.selections.len() > kept as usize {
        list.selections.truncate(kept as usize);
    }
    while list.selections.len() < kept as usize {
        list.selections.push(LodSelection::default());
    }
    for (index, terrain) in terrains.iter().enumerate() {
        if index >= kept as usize {
            tracing::warn!(
                target: "regia_terrain",
                "more than {} terrains; LOD selection truncated",
                settings.max_selections
            );
            break;
        }
        #[cfg(test)]
        TERRAIN_SELECTION_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cdlod::fill_lod_selection(
            &cdlod::LodWalk {
                map: &terrain.map,
                pyramid: &terrain.minmax,
                config: &terrain.cdlod,
                root_level: terrain.root_level,
                camera_xz: eye,
                frustum: None,
                min_level: 0,
            },
            &mut list.selections[index],
            &mut memo.stack,
        );
    }
    memo.eye = eye_bits;
    memo.stamp = stamp;
    memo.count = kept;
    memo.ready = true;
}

/// Adds terrain LOD selection to the app's `Render` schedule.
///
/// GPU work itself is owned by [`gpu::TerrainGpu`] and drawn through the
/// [`gpu::TerrainRenderer`] hook; the M5 render graph calls it once that graph exists.
#[derive(Default)]
pub struct TerrainPlugin;

impl Plugin for TerrainPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TerrainSettings>()
            .init_resource::<TerrainDrawList>()
            .init_resource::<TerrainSelectionMemo>()
            .init_resource::<ScatterControl>()
            .init_resource::<ScatterSets>()
            .init_resource::<TerrainHoles>()
            .init_resource::<TerrainDrawPasses>()
            .init_resource::<TerrainHeightPatches>()
            .add_message::<TerrainHeightsChanged>()
            .add_message::<regia_asset::SourceAssetChanged>()
            .init_resource::<TerrainStrokeLog>()
            .init_resource::<TerrainVtHandle>()
            .init_resource::<TerrainWarm>()
            .add_systems(Update, (reload_terrain_files, gpu::warm::queue_terrain_warm))
            .add_systems(
                Update,
                (
                    runtime_patch::forward_terrain_strokes,
                    apply_terrain_height_patches,
                )
                    .chain(),
            )
            .add_systems(Update, scatter::painted::reseat_painted_scatter)
            .add_systems(Update, (wind::couple_scene_wind, wind::sync_terrain_wind).chain())
            .add_systems(Update, stream_hooks::drive_tile_stream)
            .add_systems(Update, mesh_layer::stream::drive_layer_stream)
            // The mesh-layer mouths draw from the cooked lattice cut (ADR-8470).
            .add_systems(Update, mesh_layer::sync_mesh_layer_lattice)
            .add_systems(Render, update_terrain_selection)
            // The heightfield's SunCover provider (opt-in, ADR-8874): between the extraction (its
            // sun) and the cell cull that reads the cover.
            .add_systems(
                Render,
                sun_cover::update_terrain_sun_cover
                    .after(regia_render::extract_frame)
                    .before(regia_render::viscells::cull_unreachable_cells),
            )
            // A parked or hidden terrain stops drawing (spec 37 § 13, ADR-7200).
            .init_resource::<parked_draw::WithheldTerrainDraw>()
            .add_systems(
                Render,
                parked_draw::withhold_parked_terrain_draw.before(regia_render::render_texture_views),
            )
            .add_systems(
                Update,
                edit::draw_brush_preview
                    .run_if(resource_exists::<TerrainBrushPreview>)
                    .run_if(
                        regia_render::gizmos::gizmo_group_enabled::<
                            regia_render::gizmos::DefaultGizmoConfigGroup,
                        >,
                    ),
            );
    }
}

/// Top levels of the cooked layer arrays a preset leaves out
/// ([`TerrainSharedDraw::set_layer_mip_skip`]): one on the Steam Deck and mobile (a quarter
/// of the layer memory; DO27's 2 K layers sample at 1 K there), none on desktop.
#[must_use]
pub fn layer_mip_skip_for(preset: regia_render::RenderProfilePreset) -> u32 {
    use regia_render::RenderProfilePreset as P;
    match preset {
        P::Deck | P::MobileLow | P::MobileHigh => 1,
        _ => 0,
    }
}

/// Rust and WGSL struct rows of the terrain, grass, impostor and water shaders: the host type of
/// each uniform and storage struct, built beside the private types (`regia_render::shader_contract`,
/// ADR-3002).
#[must_use]
pub fn shader_contract_rows() -> Vec<regia_render::ContractRow> {
    let mut rows = gpu::contract_rows();
    rows.extend(water_body::shaft_contract_rows());
    rows
}

pub use gpu::warm::{TerrainPreparation, prepare_terrain};

#[cfg(test)]
mod selection_tests {
    use std::sync::atomic::Ordering;

    use super::*;

    fn flat_terrain() -> Terrain {
        let n = 33u32;
        let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 128.0, 10.0, 0.0).expect("map");
        let heightfield = map.to_heightfield();
        Terrain::new(
            &heightfield,
            128.0,
            10.0,
            0.0,
            CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain")
    }

    #[test]
    fn a_held_view_does_not_rebuild_lod_selection() {
        let mut app = App::new();
        app.init_resource::<TerrainSettings>()
            .init_resource::<TerrainDrawList>()
            .init_resource::<TerrainSelectionMemo>()
            .add_systems(Render, update_terrain_selection);
        app.world_mut().spawn(flat_terrain());
        app.insert_resource(TerrainView {
            position: [0.0, 8.0, 0.0],
            target: [10.0, 0.0, 0.0],
        });
        TERRAIN_SELECTION_BUILDS.store(0, Ordering::Relaxed);
        app.update();
        let builds = TERRAIN_SELECTION_BUILDS.load(Ordering::Relaxed);
        assert_eq!(builds, 1, "the first frame walks once");
        let list = app.world().resource::<TerrainDrawList>();
        assert_eq!(list.selections.len(), 1);
        let nodes = list.selections[0].nodes.len();
        let cap = list.selections[0].nodes.capacity();
        assert!(nodes > 0, "the flat map selects nodes");
        app.update();
        assert_eq!(
            TERRAIN_SELECTION_BUILDS.load(Ordering::Relaxed),
            builds,
            "a held view does not walk again"
        );
        let list = app.world().resource::<TerrainDrawList>();
        assert_eq!(list.selections[0].nodes.len(), nodes);
        assert_eq!(list.selections[0].nodes.capacity(), cap);
        app.world_mut().resource_mut::<TerrainView>().position = [40.0, 8.0, 0.0];
        app.update();
        assert_eq!(TERRAIN_SELECTION_BUILDS.load(Ordering::Relaxed), builds + 1);
        assert!(
            app.world().resource::<TerrainDrawList>().selections[0]
                .nodes
                .capacity()
                >= cap
        );
    }
}
// A parked or hidden terrain stops drawing (spec 37 § 13, ADR-7200).
pub mod parked_draw;
// Non-destructive terrain edit layers and their `<map>.terrain.editlayers` sidecar (ADR-7825).
pub mod edit_layers;
pub use edit_layers::{
    EDIT_LAYERS_FORMAT_VERSION, EditLayer, EditLayerBlend, EditLayerError, EditLayerLogEntry, EditLayerOp,
    EditLayerOpOutcome, EditLayerPatch, LayerSampleWrite, LayerStrokeUndo, ProcgenLayerTarget,
    TerrainEditLayerLog, TerrainEditLayers, absorb_world_stroke, apply_op, edit_layers_path, install_stack,
    load_edit_layers, migrate_edit_layers_bytes, set_active_layer,
};

// Count-based authoring sources and CPU overview baking (ADR-8063).
pub mod overview;
pub mod source;

// Heightfield operators: noise, domain warp, terraces, craters, flattened pads (DO27 absorb C-18, ADR-8465).
pub mod ops;
// Cooked photoreal terrain: exact heights, holes and a pak-backed material in the cooked map (ADR-8024).
pub mod cooked_terrain;
pub use cooked_terrain::{
    CookedTerrainMaterial, MaterialBlobKind, TERRAIN_FINAL_HEIGHTS_FORMAT_VERSION,
    final_heights_migration_sources, final_heights_schema_fingerprint,
};
pub use stream::{
    TerrainHeightsEncoding, TerrainHeightsIndex, TerrainHolesIndex, TerrainMaterialIndex,
    TerrainMaterialParamsIndex, terrain_index_migration_sources, terrain_index_schema_fingerprint,
};
// Visual-only far backdrop ring past the playable bounds (ADR-8459).
pub mod backdrop;
pub use backdrop::{
    BackdropBuild, BackdropError, BackdropRing, BackdropSpec, BackdropState, BackdropStatus, Landform,
    RingProfile, RingProfileOverlay, RingWidth, build_visual_map, extend_visual_map,
};

// Where the planet-shine fill takes the canyon-scale occlusion only (ADR-8412).
pub mod fill_canyon;
pub use fill_canyon::{FillCanyonScope, fill_visibility, near_occluder_shade};

// The registered overview artifact: descriptor, cache and atomic pair publication (ADR-8596).
pub mod overview_artifact;

// The heightfield's SunCover provider for sun sealing (v0.7 item E, ADR-8874).
pub mod sun_cover;
pub use sun_cover::{
    TerrainSunCover, TerrainSunCoverStats, coarsest_shadow_cell_m, fill_terrain_sun_cover,
    terrain_covers_box, terrain_min_cover, update_terrain_sun_cover,
};
// The version-3 cooked terrain index's source valid-data mask (ADR-8871).
pub use stream::TerrainValidMaskIndex;
