// SPDX-License-Identifier: MPL-2.0
//! [`TerrainSharedDraw`] implements [`regia_render::SharedPassDraw`] so the
//! heightfield is drawn into the frame's depth, cascades, HDR colour and
//! motion vectors. The offscreen `Rgba8UnormSrgb` path is unchanged.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use regia_render::{
    GroundBounce, GroundBounceLevel, RenderDevice, SharedFrameInfo, SharedPass, SharedPassDraw, SharedUpload,
};

/// Cell of the ground-bounce grid handed to meshes, metres (64 cells: a 256 m window).
/// Mesh ground-bounce grids (`GroundBounce`): (cell metres, cells per side, supersample).
/// The fine grid point-samples the terrain's own bounce field every 2 m within ±64 m of the
/// camera (the field steps where a horizon sample crosses a wall, so 4 m cells blended a
/// slab on a trench floor with the brighter cells by the wall); the coarse one averages it
/// over 16 m cells out to ±256 m.
const BOUNCE_GRIDS: [(f32, u32, u32); 2] = [(2.0, 64, 1), (16.0, 32, 2)];

use super::shadow_lists::{CascadeShadow, EditLog, ShadowInputs};
use super::{TerrainFrame, TerrainGpu};
use crate::TerrainError;
use crate::cdlod::{CdlodConfig, LodSelection, MinMaxPyramid, select_nodes};
use crate::layers::TerrainLayers;
use crate::sampler::TerrainMap;
use crate::stream::{PakTileSource, TerrainIndex};

/// Counters of one [`TerrainSharedDraw`], readable after the draw is boxed
/// into a [`regia_render::FrameRenderer`]. Updated by every `prepare`.
#[derive(Debug, Default)]
pub struct TerrainDrawStats {
    node_writes: AtomicU32,
    node_bytes: AtomicU64,
    selected: AtomicU32,
    prepare_us: AtomicU64,
    camera_nodes: AtomicU32,
    /// Nodes in the view frustum the outdoor frusta left out, last main-view frame (ADR-8413).
    portal_culled: AtomicU32,
    /// Keep a copy of the camera passes' node boxes each main-view frame ([`Self::camera_node_bounds`]).
    record_camera_nodes: std::sync::atomic::AtomicBool,
    camera_node_bounds: std::sync::Mutex<Vec<([f32; 3], [f32; 3])>>,
    cascade_nodes: [AtomicU32; 4],
    dense_nodes: AtomicU32,
    hole_tests: AtomicU32,
    vt: std::sync::Mutex<Option<super::vt::VtStats>>,
    /// Runtime height bands applied with a changed sample, since the draw was made (ADR-7585).
    edit_boxes: AtomicU64,
    /// Edit boxes that reselected and re-keyed each main-camera cascade, since the draw was made.
    cascade_edit_hits: [AtomicU64; 4],
    /// Edit boxes that made the grass select its patches again, since the draw was made.
    grass_edit_reselects: AtomicU64,
    /// What height edits invalidated in the virtual texture, as of the last frame.
    vt_edits: std::sync::Mutex<super::VtEditCounters>,
    /// Mouth-band draws per band camera (main, texture views) and pass slot
    /// (`BAND_SLOT_*`), since the draw was made (ADR-8824).
    band_draws: [[AtomicU64; BAND_SLOTS]; super::BAND_CAMERAS],
}

/// Band draw counter slots: colour, depth, velocity, receiver, the four full cascade draws,
/// the four cascade page redraws, the precipitation occlusion tile.
const BAND_SLOTS: usize = 13;
const BAND_SLOT_CASCADE: usize = 4;
const BAND_SLOT_REGION: usize = 8;
const BAND_SLOT_PRECIP: usize = 12;

/// Mouth-band draws one camera issued in each terrain pass, since the draw was made
/// ([`TerrainDrawStats::band_draws`], ADR-8824). Each pass that draws terrain draws the band
/// once per open-mouth draw it makes; a pass that ran without a band counts nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BandDrawCounts {
    /// The colour pass ([`SharedPass::Color`]).
    pub color: u64,
    /// The depth prepass ([`SharedPass::Depth`]).
    pub depth: u64,
    /// The terrain velocity pass ([`SharedPass::Velocity`]).
    pub velocity: u64,
    /// The decal receiver pass ([`SharedPass::Receiver`]).
    pub receiver: u64,
    /// Full draws of each sun cascade ([`SharedPass::Shadow`]).
    pub cascades: [u64; 4],
    /// Page redraws of each sun cascade under moved meshes
    /// ([`SharedPassDraw::draw_shadow_region`]).
    pub cascade_regions: [u64; 4],
    /// The precipitation occlusion tile ([`SharedPassDraw::draw_precip_occlusion`]).
    pub precip_tile: u64,
}

impl BandDrawCounts {
    /// Every pass's draws, summed.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.color
            + self.depth
            + self.velocity
            + self.receiver
            + self.cascades.iter().sum::<u64>()
            + self.cascade_regions.iter().sum::<u64>()
            + self.precip_tile
    }
}

impl TerrainDrawStats {
    /// The terrain virtual texture's counters on the last frame (`None` when it is off).
    #[must_use]
    pub fn virtual_texture(&self) -> Option<super::vt::VtStats> {
        *self.vt.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Node-uniform writes and bytes queued by the last frame. A held camera is `(0, 0)`.
    #[must_use]
    pub fn last_node_upload(&self) -> (u32, u64) {
        (
            self.node_writes.load(Ordering::Relaxed),
            self.node_bytes.load(Ordering::Relaxed),
        )
    }

    /// Nodes selected by the last frame.
    #[must_use]
    pub fn selected_nodes(&self) -> u32 {
        self.selected.load(Ordering::Relaxed)
    }

    /// CPU time of the last `prepare` (select + upload), milliseconds.
    #[must_use]
    pub fn last_prepare_ms(&self) -> f32 {
        self.prepare_us.load(Ordering::Relaxed) as f32 / 1000.0
    }

    /// Selected nodes the last frame drew with the fine cliff-relief grid
    /// ([`crate::TerrainRelief::cliff_relief`]).
    #[must_use]
    pub fn dense_nodes(&self) -> u32 {
        self.dense_nodes.load(Ordering::Relaxed)
    }

    /// Nodes whose hole state the last frame computed ([`crate::HolesMask`] tests). Each node's
    /// state is kept until the mask changes, so a held or slowly moving view tests none.
    #[must_use]
    pub fn hole_tests(&self) -> u32 {
        self.hole_tests.load(Ordering::Relaxed)
    }

    /// Nodes the last frame's camera passes (depth, colour, velocity) draw after culling.
    #[must_use]
    pub fn camera_nodes(&self) -> u32 {
        self.camera_nodes.load(Ordering::Relaxed)
    }

    /// Nodes in the last main-view frame's view frustum that the outdoor frusta left out of the
    /// camera passes: the view was inside a cell and they lie behind its walls (plan row W-22,
    /// ADR-8413, [`regia_render::FrameRenderer::set_outdoor_portals`]). [`Self::camera_nodes`]
    /// counts the nodes still drawn. 0 outdoors.
    #[must_use]
    pub fn portal_culled_nodes(&self) -> u32 {
        self.portal_culled.load(Ordering::Relaxed)
    }

    /// Keep (or stop keeping) a copy of the world boxes of the nodes the camera passes draw, each
    /// main-view frame, for [`Self::camera_node_bounds`]. Off by default (a test and debug aid).
    pub fn set_record_camera_nodes(&self, on: bool) {
        self.record_camera_nodes.store(on, Ordering::Relaxed);
    }

    /// The world boxes (`min`, `max`, metres; cliff-relief padded) of the nodes the camera passes
    /// drew on the last main-view frame, in draw order, while [`Self::set_record_camera_nodes`] is
    /// on (empty before).
    #[must_use]
    pub fn camera_node_bounds(&self) -> Vec<([f32; 3], [f32; 3])> {
        self.camera_node_bounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Nodes each sun cascade's shadow pass would draw on the last frame (a cascade the
    /// renderer kept from an earlier frame draws none of them).
    #[must_use]
    pub fn cascade_nodes(&self) -> [u32; 4] {
        std::array::from_fn(|i| self.cascade_nodes[i].load(Ordering::Relaxed))
    }

    /// Runtime height bands that changed at least one sample, since the draw was made
    /// (ADR-7585): each is one edit box the cascades, the grass and the virtual texture test.
    #[must_use]
    pub fn height_edit_boxes(&self) -> u64 {
        self.edit_boxes.load(Ordering::Relaxed)
    }

    /// Edit boxes that invalidated each main-camera sun cascade, since the draw was made: a box
    /// invalidates a cascade (reselects its nodes and changes its shadow key, so the renderer
    /// redraws it) only when it lands inside that cascade's box (ADR-7585).
    #[must_use]
    pub fn cascade_edit_invalidations(&self) -> [u64; 4] {
        std::array::from_fn(|i| self.cascade_edit_hits[i].load(Ordering::Relaxed))
    }

    /// What runtime height edits invalidated in the virtual texture as of the last frame
    /// ([`TerrainGpu::vt_edit_counters`], ADR-7585).
    #[must_use]
    pub fn virtual_texture_edits(&self) -> super::VtEditCounters {
        *self
            .vt_edits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Edit boxes that made the bound grass select its patches again (the box met the grass
    /// range around its eye), since the draw was made (ADR-7585).
    #[must_use]
    pub fn grass_edit_reselects(&self) -> u64 {
        self.grass_edit_reselects.load(Ordering::Relaxed)
    }

    /// The mouth-band draws the main camera's passes issued, since the draw was made
    /// (ADR-8824).
    #[must_use]
    pub fn band_draws(&self) -> BandDrawCounts {
        self.band_counts(0)
    }

    /// The mouth-band draws every texture view's passes issued (probe captures, mirrors,
    /// screens, stereo eyes; [`SharedPassDraw::set_view`]), summed, since the draw was made
    /// (ADR-8824).
    #[must_use]
    pub fn view_band_draws(&self) -> BandDrawCounts {
        self.band_counts(1)
    }

    fn band_counts(&self, camera: usize) -> BandDrawCounts {
        let c = &self.band_draws[camera];
        let at = |i: usize| c[i].load(Ordering::Relaxed);
        BandDrawCounts {
            color: at(0),
            depth: at(1),
            velocity: at(2),
            receiver: at(3),
            cascades: std::array::from_fn(|i| at(BAND_SLOT_CASCADE + i)),
            cascade_regions: std::array::from_fn(|i| at(BAND_SLOT_REGION + i)),
            precip_tile: at(BAND_SLOT_PRECIP),
        }
    }

    /// Counts one band draw of `camera` in `slot`.
    fn count_band(&self, camera: u32, slot: usize) {
        if let Some(counter) = self
            .band_draws
            .get(camera as usize)
            .and_then(|camera| camera.get(slot))
        {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The band counter slot of a full draw of `kind` (the precipitation tile's own slot is
/// `BAND_SLOT_PRECIP`).
fn band_slot(kind: SharedPass) -> usize {
    match kind {
        SharedPass::Color => 0,
        SharedPass::Depth => 1,
        SharedPass::Velocity => 2,
        SharedPass::Receiver => 3,
        SharedPass::Shadow { index } if index < 4 => BAND_SLOT_CASCADE + index as usize,
        SharedPass::Shadow { .. } => BAND_SLOT_PRECIP,
    }
}

/// Sky light on the terrain in the shared colour pass, as radiance (`E / π`, the units
/// [`regia_render::ambient_irradiance_table`] returns): the upper hemisphere (`sky`) and
/// the ground below the horizon (`ground`).
///
/// A surface with normal `n` receives `sky · (1 + n.y) / 2 + ground · (1 − n.y) / 2`, the
/// view factors of an infinite sky above and an infinite ground plane below (hemisphere
/// lighting; see `docs/provenance.md`). Flat ground sees only the sky, a vertical wall half
/// of each, an overhang mostly the ground.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyAmbient {
    /// Cosine-weighted radiance of the sky hemisphere, linear RGB.
    pub sky: [f32; 3],
    /// Radiance of the ground bounce, linear RGB.
    pub ground: [f32; 3],
}

/// Zenith colour of the engine's procedural sky (`regia_render` `utils.wgsl` `sky_color`):
/// the flat-ambient tint meshes get on up-facing surfaces.
const ENGINE_SKY_ZENITH: [f32; 3] = [0.20, 0.40, 0.92];

/// Ground colour of the engine's procedural sky (neutral bounce, `utils.wgsl` `sky_color`).
const ENGINE_GROUND: [f32; 3] = [0.34, 0.33, 0.31];

impl SkyAmbient {
    /// The default when nothing better is known: the frame's flat `ambient_lux` tinted like
    /// the mesh path's procedural sky (zenith blue above, neutral ground below).
    #[must_use]
    pub fn from_flat_lux(ambient_lux: f32) -> Self {
        let l = ambient_lux.max(0.0) / std::f32::consts::PI;
        Self {
            sky: ENGINE_SKY_ZENITH.map(|c| c * l),
            ground: ENGINE_GROUND.map(|c| c * l),
        }
    }

    /// The sky light of a planet's atmosphere for a sun direction (unit vector toward the
    /// sun): `sky` is the cosine-weighted sky-view table without the sun disc
    /// ([`regia_render::ambient_irradiance_table`], the term the renderer's ambient bake
    /// lights meshes with), `ground` the Lambertian bounce of the planet's ground albedo
    /// under the transmitted sun plus that sky. Builds the atmosphere tables on the CPU
    /// (milliseconds): call it when the map or the sun changes, not every frame.
    #[must_use]
    pub fn from_atmosphere(params: &regia_render::AtmosphereParameters, to_sun: [f32; 3]) -> Self {
        let luts = regia_render::build_atmosphere_luts_for(params, to_sun, 2.0);
        let sky = regia_render::ambient_irradiance_table(&luts);
        let (sun_color, sun_lux) = regia_render::transmitted_sun(&luts, params);
        let len = (to_sun[0] * to_sun[0] + to_sun[1] * to_sun[1] + to_sun[2] * to_sun[2])
            .sqrt()
            .max(1e-6);
        let sin_el = (to_sun[1] / len).max(0.0);
        let mut ground = [0.0; 3];
        for c in 0..3 {
            // Radiance of a Lambertian ground: albedo · (E_sun · sin(el) + π · L_sky) / π.
            let e = sun_lux * sun_color[c] * sin_el / std::f32::consts::PI + sky[c];
            ground[c] = params.ground_albedo[c].clamp(0.0, 1.0) * e;
        }
        Self { sky, ground }
    }

    /// An airless body (Europa, the Moon; `AtmospherePreset::Airless`): a black sky and a
    /// Lambertian ground lit by the sun, `ground = albedo · E_sun · sin(elevation) / π`.
    /// This is what the airless ambient bake gives meshes (ADR-040 § 3); without it the
    /// terrain kept the flat `ambient_lux` in the Earth sky's zenith blue, which on a 4.4 klx
    /// Europa sun out-shone the sun three to one and tinted every slope blue.
    #[must_use]
    pub fn airless(sun_lux: f32, sun_elevation_rad: f32, ground_albedo: [f32; 3]) -> Self {
        let e = sun_lux.max(0.0) * sun_elevation_rad.sin().max(0.0) / std::f32::consts::PI;
        Self {
            sky: [0.0; 3],
            ground: ground_albedo.map(|a| a.clamp(0.0, 1.0) * e),
        }
    }
}

/// One terrain registered on a [`regia_render::FrameRenderer`].
pub struct TerrainSharedDraw {
    gpu: TerrainGpu,
    map: TerrainMap,
    pyramid: MinMaxPyramid,
    config: CdlodConfig,
    root_level: u32,
    selection: LodSelection,
    /// Samples reconstructed from the cooked tiled heightfield, when this draw
    /// was built by [`Self::from_cooked`].
    cooked_samples: Option<Vec<u16>>,
    /// Eye (x, z) the selection was made from.
    selected_eye: Option<[f32; 2]>,
    /// Hash of `selection`, part of the shadow key.
    selection_key: u64,
    /// Bumped by [`Self::update_heights`], part of the shadow key.
    heights_generation: u64,
    /// Bumped with `heights_generation` by whole-map changes only (holes, [`Self::update_heights`]):
    /// every cascade reselects. Runtime height bands go to `edit_log` instead (ADR-7585).
    shadow_generation: u64,
    /// The world boxes of the applied runtime height bands, tested per cascade (ADR-7585).
    edit_log: EditLog,
    /// The sample rectangles the last `heights_generation` step changed, for the probes' max
    /// pyramid (`HeightfieldInfo::edits`, ADR-7659); the default after a whole-map change.
    gi_edits: regia_render::HeightEdits,
    /// The heights generation the grass keys its patch selection on: whole-map changes, and the
    /// runtime bands whose box meets the grass range (ADR-7585).
    grass_generation: u64,
    /// `TerrainRelief::bounce` (the same strength meshes get).
    bounce_strength: f32,
    /// Ground albedo the engine's probes see this frame where no albedo map is bound: the
    /// layers' mean (tint × texture mean) when the layer textures are known, else the one the
    /// frame's ground light implies (`implied_albedo`), else the layer tints.
    gi_albedo: [f32; 3],
    /// Ground albedo map for the engine's probes (`set_gi_albedo_map`) and its generation.
    gi_albedo_map: Option<(wgpu::TextureView, u64)>,
    /// The local origin in the terrain's space, from the last prepared frame (spec 78 § 12.2).
    origin_offset: [f32; 3],
    /// `TerrainFrame::debug_view` for the shared colour pass ([`Self::set_debug_view`],
    /// [`Self::debug_view_handle`]), read at every prepare.
    debug_view: crate::TerrainDebugViewHandle,
    /// Opted in to stochastic SSR ([`Self::set_screen_space_reflections`]).
    screen_space_reflections: bool,
    /// Bounce factor grid around the camera (CPU mirror of the shader's bounce).
    bounce_grids: [crate::bounce::BounceGrid; 2],
    /// What [`SharedPassDraw::ground_bounce`] returns this frame.
    ground_bounce: Option<GroundBounce>,
    bounce_scale_key: [u32; 3],
    stats: Arc<TerrainDrawStats>,
    /// The ground look the host or agent sets (ADR-1408) and the generation last applied.
    ground_look: crate::GroundLookHandle,
    ground_look_applied: u64,
    /// World AABB (`min`, `max`) of each selected node, rebuilt with the selection.
    node_bounds: Vec<([f32; 3], [f32; 3])>,
    /// Selected nodes inside the camera frustum (depth, colour, velocity passes).
    camera_list: Vec<u32>,
    /// Each sun cascade's anchored selection and culled list ([`super::shadow_lists`]).
    shadows: [CascadeShadow; 4],
    /// Draw lists of this frame, as node uniform slots: 0..=3 cascades, 4 camera,
    /// 5..=8 the cascade parts under redrawn mesh pages. Uploaded in this order, so a
    /// camera list change does not move the (stable) cascade lists.
    lists: [Vec<u32>; 10],
    /// The four cascade selections followed by the camera selection. Node uniform slot
    /// `i` is entry `i`. Cascades first: a camera LOD change does not move their slots.
    draw_list: LodSelection,
    /// First slot of each cascade selection and of the camera selection in `draw_list`.
    bases: [u32; 5],
    /// Sky light override set by the host; `None` follows the frame (`SharedFrameInfo::sky_ambient`, then `ambient_lux`).
    sky_ambient: Option<SkyAmbient>,
    /// The drawn surface, shared with placement code ([`Self::surface`]).
    surface: RenderedSurface,
    /// The layer stack, for [`SharedPassDraw::reference_heightfield`].
    reference_layers: TerrainLayers,
    /// Mean linear albedo of each layer's texture ([`Self::set_material`]); `None` = the tint.
    reference_layer_means: Vec<Option<[f32; 3]>>,
    /// An albedo baked by the host for the heights of `.0` ([`Self::set_reference_albedo`]).
    reference_albedo: Option<(u64, regia_render::LabTexture)>,
    /// Holes requested through the agent protocol ([`Self::holes_handle`]).
    holes_handle: crate::TerrainHoles,
    /// The passes drawn last frame ([`Self::draw_passes_handle`], ADR-8470).
    draw_passes: super::pass_selections::TerrainDrawPasses,
    /// Per band set (camera, cascades 0-3): the key its placed band was written for
    /// (selection or cascade key, morph eye bits, heights generation, bands generation), ADR-8569.
    band_keys: [Option<BandKey>; super::BAND_PASSES],
    /// Per band set: the vertices last placed (published to `draw_passes`).
    band_placed: [Vec<[f32; 3]>; super::BAND_PASSES],
    /// The texture view whose placement band camera 1 holds, and its per-set keys: the views
    /// take turns on that camera, so another view's prepare places again (ADR-8824).
    view_band_owner: Option<u64>,
    view_band_keys: [Option<BandKey>; super::BAND_PASSES],
    /// Runtime height patches ([`Self::height_patches_handle`], ADR-1595).
    height_patches: crate::TerrainHeightPatches,
    /// Bound scatter impostor layer ([`Self::set_impostors`]).
    impostors: Option<super::impostor::ImpostorDraw>,
    /// Bound grass field ([`Self::set_grass`], ADR-1400).
    grass: Option<super::grass::GrassLayer>,
    /// The render-to-texture camera whose state is swapped in ([`SharedPassDraw::set_view`]);
    /// `None` is the host's main camera.
    active_view: Option<u64>,
    /// The outdoors the main view sees from inside a cell, as the frame gave it (local space;
    /// [`SharedPassDraw::set_outdoor_portals`], ADR-8413), and the same in the terrain's space.
    outdoor_portals: regia_render::viscells::OutdoorPortals,
    outdoor_world: regia_render::viscells::OutdoorPortals,
    /// What the last main-view prepare culled through them.
    outdoor_counts: regia_render::viscells::OutdoorCullCounts,
    /// The other cameras' selections, lists and GPU state, by texture view id. While a view
    /// is active, its entry holds the main camera's.
    views: std::collections::HashMap<u64, Box<TerrainView>>,
    /// The caster proxy of the ray-traced sun ([`SharedPassDraw::set_trace_casters`], ADR-2310):
    /// built while the frame traces, dropped when it stops.
    trace_proxy: Option<crate::TraceProxy>,
    /// Its settings and stats, shared with the agent ([`Self::trace_proxy_handle`]).
    trace_proxy_handle: crate::TerrainTraceProxyHandle,
    /// The frame skips the sun cascades: the colour pass takes the trace alone
    /// ([`SharedPassDraw::set_engine_sun_traced`], ADR-2310).
    engine_sun_traced: bool,
}

/// One camera's share of the draw: its LOD selection, cascade selections, draw lists, ground
/// bounce and shared-path GPU state ([`super::ViewGpu`]).
struct TerrainView {
    selection: LodSelection,
    selected_eye: Option<[f32; 2]>,
    selection_key: u64,
    node_bounds: Vec<([f32; 3], [f32; 3])>,
    camera_list: Vec<u32>,
    shadows: [CascadeShadow; 4],
    lists: [Vec<u32>; 10],
    draw_list: LodSelection,
    bases: [u32; 5],
    bounce_grids: [crate::bounce::BounceGrid; 2],
    ground_bounce: Option<GroundBounce>,
    bounce_scale_key: [u32; 3],
    gpu: super::ViewGpu,
}

impl TerrainView {
    fn new(gpu: &TerrainGpu) -> Self {
        Self {
            selection: LodSelection::default(),
            selected_eye: None,
            selection_key: 0,
            node_bounds: Vec::new(),
            camera_list: Vec::new(),
            shadows: Default::default(),
            lists: Default::default(),
            draw_list: LodSelection::default(),
            bases: [0; 5],
            bounce_grids: Default::default(),
            ground_bounce: None,
            bounce_scale_key: [0; 3],
            gpu: super::ViewGpu::new(gpu),
        }
    }
}

impl TerrainSharedDraw {
    /// Uploads `map` and selects nodes from it each frame.
    ///
    /// `config.grid_size` is a minimum too: the runtime frame draws at least
    /// [`SHARED_MIN_GRID_SIZE`] cells per node edge, so a cell projects to about 16 px at
    /// 1440p and the drawn surface stays within ~0.22 m of the heightfield inside 200 m on
    /// decimetre-rough ground ([`crate::lod_error`]). Placement that must touch the drawn
    /// ground reads [`Self::rendered_heights`].
    ///
    /// `root_level` is a minimum: the quadtree is deepened until a leaf node's grid cell is
    /// no larger than one heightfield texel ([`full_detail_root_level`]), so the mesh at
    /// the camera carries every sample of the map. A 4096² map over 1280 m drawn with
    /// `root_level` 4 and a 16-cell grid had 5 m cells under the camera — the big flat
    /// facets of the 2026-10-07 close-up plates.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when the GPU upload or the CDLOD pyramid fails.
    pub fn new(
        device: &RenderDevice,
        map: TerrainMap,
        layers: &TerrainLayers,
        config: CdlodConfig,
        root_level: u32,
    ) -> Result<Self, TerrainError> {
        Self::new_inner(device, map, layers, config, root_level, true)
    }

    /// [`Self::new`] for the pipeline-warm job's throwaway terrain: the draw exists only to
    /// compile pipelines, so it does not publish its anisotropy path or LOD offset to the
    /// process-global report ([`crate::anisotropy_path`]). See [`super::TerrainGpu::new_for_warm`].
    pub(crate) fn new_for_warm(
        device: &RenderDevice,
        map: TerrainMap,
        layers: &TerrainLayers,
        config: CdlodConfig,
        root_level: u32,
    ) -> Result<Self, TerrainError> {
        Self::new_inner(device, map, layers, config, root_level, false)
    }

    fn new_inner(
        device: &RenderDevice,
        map: TerrainMap,
        layers: &TerrainLayers,
        mut config: CdlodConfig,
        root_level: u32,
        publish_aniso_report: bool,
    ) -> Result<Self, TerrainError> {
        config.grid_size = config.grid_size.max(SHARED_MIN_GRID_SIZE);
        let root_level = root_level.max(full_detail_root_level(&map, &config));
        let pyramid = MinMaxPyramid::build(&map, root_level)?;
        let gpu = TerrainGpu::new_inner(
            device,
            &map,
            layers,
            &config,
            root_level,
            None,
            None,
            publish_aniso_report,
        )?;
        let height_patches = crate::TerrainHeightPatches::linked(&map);
        Ok(Self {
            gpu,
            map,
            pyramid,
            config,
            root_level,
            selection: LodSelection::default(),
            cooked_samples: None,
            selected_eye: None,
            selection_key: 0,
            heights_generation: 0,
            shadow_generation: 0,
            edit_log: EditLog::default(),
            gi_edits: regia_render::HeightEdits::default(),
            grass_generation: 0,
            gi_albedo: [0.5; 3],
            gi_albedo_map: None,
            origin_offset: [0.0; 3],
            bounce_strength: 1.0,
            debug_view: crate::TerrainDebugViewHandle::default(),
            screen_space_reflections: false,
            bounce_grids: Default::default(),
            ground_bounce: None,
            bounce_scale_key: [0; 3],
            stats: Arc::default(),
            ground_look: crate::GroundLookHandle::default(),
            ground_look_applied: 0,
            node_bounds: Vec::new(),
            camera_list: Vec::new(),
            shadows: Default::default(),
            lists: Default::default(),
            draw_list: LodSelection::default(),
            bases: [0; 5],
            reference_layers: layers.clone(),
            reference_layer_means: Vec::new(),
            reference_albedo: None,
            sky_ambient: None,
            surface: RenderedSurface::default(),
            holes_handle: crate::TerrainHoles::linked(),
            draw_passes: super::pass_selections::TerrainDrawPasses::linked(),
            band_keys: [None; super::BAND_PASSES],
            band_placed: Default::default(),
            view_band_owner: None,
            view_band_keys: [None; super::BAND_PASSES],
            height_patches,
            impostors: None,
            grass: None,
            active_view: None,
            outdoor_portals: regia_render::viscells::OutdoorPortals::default(),
            outdoor_world: regia_render::viscells::OutdoorPortals::default(),
            outdoor_counts: regia_render::viscells::OutdoorCullCounts::default(),
            views: std::collections::HashMap::new(),
            trace_proxy: None,
            trace_proxy_handle: crate::TerrainTraceProxyHandle::default(),
            engine_sun_traced: false,
        })
    }

    /// Sets the resolution of the ray-traced sun's caster proxy ([`crate::TraceProxy`],
    /// ADR-2310). A proxy already built is rebuilt with it the next time the frame traces.
    pub fn set_trace_proxy_settings(&mut self, settings: crate::TraceProxySettings) {
        self.trace_proxy_handle.set_settings(settings);
    }

    /// What the caster proxy holds, `None` while the frame does not trace (`terrain.trace_proxy`).
    #[must_use]
    pub fn trace_proxy_stats(&self) -> Option<crate::TraceProxyStats> {
        self.trace_proxy.as_ref().map(crate::TraceProxy::stats)
    }

    /// The handle the agent's `terrain.trace_proxy` / `terrain.set_trace_proxy` use: insert
    /// `TerrainTraceProxyControl(draw.trace_proxy_handle())` with the draw.
    #[must_use]
    pub fn trace_proxy_handle(&self) -> crate::TerrainTraceProxyHandle {
        self.trace_proxy_handle.clone()
    }

    /// A handle on the passes this draw ran last frame for the host's camera: `main` and each
    /// active `cascadeN`, with the selection drawn and its morph centre (ADR-8470). Insert it as
    /// a resource for `terrain.seam_report`, which then measures the drawn selections. The handle
    /// also reads this draw's band draw counters ([`TerrainDrawStats::band_draws`], ADR-8824).
    #[must_use]
    pub fn draw_passes_handle(&self) -> super::pass_selections::TerrainDrawPasses {
        self.draw_passes.link_stats(Arc::clone(&self.stats));
        self.draw_passes.clone()
    }

    /// Exchanges the active camera's share of the draw with `view`.
    fn swap_view_state(&mut self, view: &mut TerrainView) {
        use std::mem::swap;
        swap(&mut self.selection, &mut view.selection);
        swap(&mut self.selected_eye, &mut view.selected_eye);
        swap(&mut self.selection_key, &mut view.selection_key);
        swap(&mut self.node_bounds, &mut view.node_bounds);
        swap(&mut self.camera_list, &mut view.camera_list);
        swap(&mut self.shadows, &mut view.shadows);
        swap(&mut self.lists, &mut view.lists);
        swap(&mut self.draw_list, &mut view.draw_list);
        swap(&mut self.bases, &mut view.bases);
        swap(&mut self.bounce_grids, &mut view.bounce_grids);
        swap(&mut self.ground_bounce, &mut view.ground_bounce);
        swap(&mut self.bounce_scale_key, &mut view.bounce_scale_key);
        self.gpu.swap_view(&mut view.gpu);
    }

    /// Texture-view cameras holding their own state ([`SharedPassDraw::set_view`]).
    #[must_use]
    pub fn view_count(&self) -> usize {
        self.views.len()
    }

    /// Loads the cooked terrain index and tiled heightfield from `source`,
    /// stitches the samples, and uploads the map.
    ///
    /// `world_width` is used only when the index has no `world_size_m`. Height
    /// scale and offset are runtime mapping (they are not stored in a standard cook's index).
    ///
    /// A host-cooked photoreal terrain (index version 2 with `heights`, ADR-8024) builds the map
    /// from its exact final heights instead, with the index's scale and offset (they win over
    /// `height_scale` / `height_offset`): one upload, bit for bit the host's heights. Its cooked
    /// holes (`holes`) are cut before the first frame. Its material is not installed here: read
    /// it with [`PakTileSource::load_cooked_material`] and pass it to
    /// [`Self::set_material_cooked`] (after [`Self::set_layer_mip_skip`] for a memory tier).
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when the pak cannot be read, the payload is the
    /// wrong type, or the stitched grid is not a valid map.
    #[allow(clippy::too_many_arguments)] // each argument is an independent cook input
    pub fn from_cooked(
        device: &RenderDevice,
        source: &PakTileSource,
        layers: &TerrainLayers,
        world_width: f32,
        height_scale: f32,
        height_offset: f32,
        config: CdlodConfig,
        root_level: u32,
    ) -> Result<(Self, TerrainIndex), TerrainError> {
        let _span = tracing::info_span!("terrain_from_cooked").entered();
        let index = source.load_terrain_index().map_err(cooked_error)?;
        let heightfield = source.load_heightfield().map_err(cooked_error)?;
        let world = index.world_size_m.filter(|w| *w > 0.0).unwrap_or(world_width);
        let exact = source
            .load_final_heights(&index, Some(&heightfield))
            .map_err(cooked_error)?;
        let map = match (exact, index.heights.as_ref()) {
            (Some(heights), Some(entry)) => TerrainMap::from_heights(
                heightfield.width,
                heightfield.height,
                heights,
                world,
                entry.height_scale,
                entry.height_offset,
            )?,
            _ => TerrainMap::new(&heightfield, world, height_scale, height_offset)?,
        };
        let holes = source.load_holes(&index).map_err(cooked_error)?;
        let mut draw = Self::new(device, map, layers, config, root_level)?;
        draw.cooked_samples = Some(heightfield.samples);
        if let Some(mask) = holes {
            draw.set_holes(Some(&mask));
        }
        Ok((draw, index))
    }

    /// Nodes selected on the last [`SharedPassDraw::prepare`].
    #[must_use]
    pub fn selection(&self) -> &LodSelection {
        &self.selection
    }

    /// Stitched height samples, present only for [`Self::from_cooked`].
    #[must_use]
    pub fn cooked_samples(&self) -> Option<&[u16]> {
        self.cooked_samples.as_deref()
    }

    /// Re-uploads the heights inside `dirty` (craters, deformation) after
    /// `map` was edited, and invalidates the cached terrain shadow cascades.
    ///
    /// The draw's own copy of the map and its min/max pyramid take the edit too (ADR-2301), so
    /// node selection bounds and [`Self::rendered_heights`] see a crater without a rebuild; every
    /// camera re-selects on the next prepare. A `map` of another size is only uploaded.
    ///
    /// Only the samples whose heights differ from the draw's map are redrawn: a call that
    /// writes the heights the draw already holds (a whole-map refresh of an unedited map)
    /// changes nothing, not even the caches keyed on the heights' generation (ADR-2450).
    pub fn update_heights(&mut self, map: &TerrainMap, dirty: &crate::deform::DirtyRect) {
        let dirty = match self.sync_own_map(map, dirty) {
            Some(None) => return,
            Some(Some(changed)) => {
                self.gpu
                    .update_heights_with_range(map, &changed, self.altitude_range());
                changed
            }
            None => {
                self.gpu.update_heights(map, dirty);
                *dirty
            }
        };
        if let Some(proxy) = self.trace_proxy.as_mut() {
            proxy.mark_dirty(&dirty);
            proxy.refresh(map);
            self.trace_proxy_handle.set_stats(Some(proxy.stats()));
        }
        self.heights_generation += 1;
        self.shadow_generation += 1;
        self.grass_generation += 1;
        // A host's heights are a whole-map change for the probes' pyramid (ADR-7659).
        self.gi_edits = regia_render::HeightEdits::default();
        self.surface.heights_changed();
    }

    /// Builds the shared depth, shadow, colour and velocity pipelines for
    /// `color_format` now, at load time, instead of on the first frame, and the hole-cut set
    /// ([`TerrainGpu::warm_shared`]): a hole cut later compiles nothing. On a device where a
    /// terrain or [`crate::terrain_warm_job`] built them already, this compiles nothing.
    pub fn prewarm(&mut self, color_format: wgpu::TextureFormat) {
        let _span = tracing::info_span!("terrain_prewarm").entered();
        self.apply_ground_look();
        self.gpu.set_cost_view(self.debug_view.get());
        self.gpu.warm_shared(color_format);
        if let Some(impostors) = self.impostors.as_mut() {
            impostors.ensure_pipelines(&self.gpu, color_format);
        }
        if let Some(grass) = self.grass.as_mut() {
            grass.ensure_pipelines(&self.gpu, color_format);
            grass.warm_weather(&self.gpu, color_format);
        }
    }

    /// Binds a scatter impostor layer (ADR-116): its instances past their class's fade start
    /// are culled on the GPU each frame and drawn as octahedral impostor cards in the depth,
    /// colour, velocity and receiver passes, right after the terrain. Keep the layer updated
    /// with [`crate::ImpostorLayer::update`]. `None` unbinds it; an unbound draw records
    /// exactly what it recorded before impostors existed.
    pub fn set_impostors(&mut self, layer: Option<&crate::ImpostorLayer>) {
        self.impostors = layer.map(|layer| super::impostor::ImpostorDraw::new(&self.gpu, layer));
    }

    /// Binds a grass field (ADR-1400, `book/agents/terrain-grass.md`): its clumps are generated
    /// on the GPU each frame around the main camera and drawn in the depth, colour and velocity
    /// passes right after the terrain, and into the finest sun cascades the tier allows. Drive
    /// it through [`crate::GrassLayer::handle`] (insert it as a resource for the agent
    /// protocol; set the sway clock on it). `None` unbinds it; an unbound draw records exactly
    /// what it recorded before grass existed.
    pub fn set_grass(&mut self, layer: Option<crate::GrassLayer>) {
        self.grass = layer;
    }

    /// The bound grass field's handle (`None` without one).
    #[must_use]
    pub fn grass_handle(&self) -> Option<crate::TerrainGrassHandle> {
        self.grass.as_ref().map(crate::GrassLayer::handle)
    }

    /// Turns the terrain virtual texture on or off (ADR-117 "Terrain virtual texture";
    /// opt-in per ADR-041): the photoreal surface is cached in pages baked by the terrain
    /// shader's own `surface_photoreal`, and pixels whose pages are resident read them
    /// instead of shading it. Frames match the uncached surface to the texels' footprint and
    /// filtering; a page miss shades in full. Call before or after boxing the draw (through
    /// [`Self::gpu_mut`] before); [`Self::prewarm`] builds its pipelines at load time.
    ///
    /// # Errors
    ///
    /// [`TerrainError::VirtualTexture`] when the settings are invalid or do not fit the map
    /// or the device; the texture stays off.
    ///
    /// Calling it (`None` included) makes the choice the host's: the render profile's
    /// `TerrainBudget::virtual_texture`, which the quality detector sets on Desktop Low on
    /// 6 GB+ discrete GPUs, no longer drives the texture (ADR-157 addendum v0.1.2).
    pub fn set_virtual_texture(
        &mut self,
        settings: Option<super::vt::VirtualTextureSettings>,
    ) -> Result<(), TerrainError> {
        self.gpu.set_virtual_texture(settings)
    }

    /// The virtual texture's counters on the last frame (`None` when it is off).
    #[must_use]
    pub fn vt_stats(&self) -> Option<super::vt::VtStats> {
        self.gpu.vt_stats()
    }

    /// Diagnostics of the virtual texture (page grid, resident pages, page readback) that
    /// keep working after the draw is boxed into the renderer.
    #[must_use]
    pub fn vt_handle(&self) -> super::vt::TerrainVtHandle {
        self.gpu.vt_handle()
    }

    /// Binds a scatter contact map: the ground under scatter instances is darkened and tinted
    /// by it (soft AO pool). Call before boxing the draw; keep updating the map with
    /// [`crate::ContactMap::update`] each frame. `None` unbinds it.
    pub fn set_contact_map(&mut self, contact: Option<&crate::ContactMap>) {
        self.gpu.set_contact(contact);
    }

    /// Counters shared with this draw; keep the handle before boxing the draw.
    #[must_use]
    pub fn stats(&self) -> Arc<TerrainDrawStats> {
        Arc::clone(&self.stats)
    }

    /// The GPU state, for counters such as [`TerrainGpu::last_node_upload`].
    #[must_use]
    pub fn gpu(&self) -> &TerrainGpu {
        &self.gpu
    }

    /// The map the draw shades.
    #[must_use]
    pub fn map(&self) -> &TerrainMap {
        &self.map
    }

    /// The CDLOD configuration in use (after the [`SHARED_MIN_GRID_SIZE`] floor).
    #[must_use]
    pub fn config(&self) -> &CdlodConfig {
        &self.config
    }

    /// The quadtree depth in use (after [`full_detail_root_level`]).
    #[must_use]
    pub fn root_level(&self) -> u32 {
        self.root_level
    }

    /// Heights of the surface this draw put on screen last frame at world `(x, z)` points:
    /// the selected node's morphed, pyramid-filtered triangles ([`crate::rendered_height`]),
    /// not the true heightfield. Scatter and props that must touch the visible ground
    /// (instead of floating or sinking by the far-LOD error) read these. Before the first
    /// [`SharedPassDraw::prepare`] (no selection yet) the true bilinear height is returned.
    /// A point inside a hole ([`Self::set_holes`]) has no ground: its height is NaN.
    #[must_use]
    pub fn rendered_heights(&self, points: &[[f32; 2]]) -> Vec<f32> {
        let mut heights = self.rendered_heights_uncut(points);
        if let Some(mask) = self.gpu.holes() {
            for (h, p) in heights.iter_mut().zip(points) {
                if mask.contains(p[0], p[1]) {
                    *h = f32::NAN;
                }
            }
        }
        heights
    }

    /// Cuts holes through the drawn terrain: tunnel mouths, open-cut ramps, craters
    /// ([`crate::HolesMask`]: ellipses, oriented rectangles, polygons, coverage rasters in
    /// terrain space). Inside a hole no terrain is drawn, in the colour pass, depth
    /// prepass, velocity, decal receiver or any sun cascade, so the cut casts no shadow;
    /// the ground around it still does. The cut edge is per pixel and fixed in world XZ
    /// (LOD morphing and cliff relief do not move it). [`Self::rendered_heights`] and the
    /// [`Self::surface`] report NaN inside, and scatter seated on the surface is not drawn
    /// there. Physics cuts its own collider ([`crate::HolesMask::cut_heights`]: NaN samples).
    /// `None` or an empty mask removes every hole.
    pub fn set_holes(&mut self, mask: Option<&crate::HolesMask>) {
        let before = self.gpu.holes_generation();
        self.gpu.set_holes(mask);
        if self.gpu.holes_generation() != before {
            self.surface.set_holes(self.gpu.holes().cloned());
            // Cached terrain shadow cascades and seat caches hold the uncut surface.
            self.heights_generation += 1;
            self.shadow_generation += 1;
            self.grass_generation += 1;
            self.gi_edits = regia_render::HeightEdits::default();
        }
        // The handle keeps the whole mask, closed named holes included (ADR-1595): the GPU
        // drops a mask that cuts nothing, but the next `terrain.set_hole` edits these holes.
        self.holes_handle.applied_if_changed(mask);
    }

    /// The hole mask in use ([`Self::set_holes`]).
    #[must_use]
    pub fn holes(&self) -> Option<&crate::HolesMask> {
        self.gpu.holes()
    }

    /// Draws the open named holes a mesh layer fills from their ground-lattice cut
    /// (`TerrainGpu::set_hole_lattice`, ADR-8470): the cut is the cooked lattice cut cell for
    /// cell in every pass and at every LOD and morph, and cliff relief is off within two
    /// lattice cells of it. `None` draws them from their coverage windows. Also reachable
    /// through the holes handle ([`crate::TerrainHoles::request_lattice`]).
    pub fn set_hole_lattice(&mut self, lattice: Option<crate::holes::LatticeHoleCut>) {
        let before = self.gpu.holes_generation();
        self.gpu.set_hole_lattice(lattice);
        if self.gpu.holes_generation() != before {
            // Cached terrain shadow cascades hold the previous cut.
            self.heights_generation += 1;
            self.shadow_generation += 1;
        }
    }

    /// A handle on this draw's holes that stays usable after the draw is boxed into the
    /// frame renderer: insert it as a resource for the agent protocol's `terrain.holes` and
    /// `terrain.set_holes`. A mask requested through it is cut on the next prepare.
    #[must_use]
    pub fn holes_handle(&self) -> crate::TerrainHoles {
        self.holes_handle.clone()
    }

    /// The runtime height patch queue of this draw (ADR-1595, `crate::runtime_patch`): insert
    /// it as the [`crate::TerrainHeightPatches`] resource. Bands the queue takes each frame
    /// are written into this draw's map, min/max pyramid and GPU height texture on the next
    /// prepare, and invalidate its cached shadow cascades and surface caches.
    #[must_use]
    pub fn height_patches_handle(&self) -> crate::TerrainHeightPatches {
        self.height_patches.clone()
    }

    /// Copies the samples of `dirty` from `map` into this draw's map, refreshes the min/max
    /// pyramid over the ones that changed and clears the cached selections. Returns the
    /// smallest rectangle holding the changed samples (`Some(None)`: `map` held this draw's
    /// heights already, nothing to redraw); nothing happens (`None`) for an empty rectangle or
    /// a map of another size.
    fn sync_own_map(
        &mut self,
        map: &TerrainMap,
        dirty: &crate::deform::DirtyRect,
    ) -> Option<Option<crate::deform::DirtyRect>> {
        if dirty.is_empty()
            || map.width() != self.map.width()
            || map.height() != self.map.height()
            || dirty.col_end > map.width()
            || dirty.row_end > map.height()
        {
            return None;
        }
        let stride = map.width() as usize;
        let mut changed = None;
        for row in dirty.row_start..dirty.row_end {
            let base = row as usize * stride;
            for col in dirty.col_start..dirty.col_end {
                let index = base + col as usize;
                let h = map.height_at_index(index);
                if self.map.height_at_index(index).to_bits() != h.to_bits() {
                    self.map.set_height(index, h);
                    changed = Some(crate::deform::DirtyRect::include(changed, col, row));
                }
            }
        }
        let Some(rect) = changed else {
            return Some(None);
        };
        self.pyramid.update_region(&self.map, &rect);
        self.selected_eye = None;
        for view in self.views.values_mut() {
            view.selected_eye = None;
        }
        Some(Some(rect))
    }

    /// Lowest and highest height of this draw's map, from the min/max pyramid's root (the
    /// exact `map.min_max_height()`, without a pass over the samples after an edit).
    fn altitude_range(&self) -> [f32; 2] {
        self.pyramid
            .node_min_max(crate::cdlod::QuadNode::root(self.root_level))
    }

    /// Writes the queued height bands into this draw (`prepare` calls it first). Returns the
    /// bands that changed a height; only the samples that changed are redrawn, and a band that
    /// writes the heights the map holds redraws nothing (ADR-2450).
    fn apply_height_patches(&mut self) -> usize {
        let bands = self.height_patches.take_draw_bands();
        if bands.is_empty() {
            return 0;
        }
        let _span = tracing::info_span!("terrain_height_patches", bands = bands.len()).entered();
        let mut applied = 0;
        let mut gi_edits = regia_render::HeightEdits::since(self.heights_generation);
        for band in &bands {
            // The heights under the band and one sample around it before the write: the drawn
            // triangles reaching the changed samples span these and the new ones.
            let around = grow_rect(&band.rect, &self.map);
            let before = rect_min_max(&self.map, &around);
            let Some(changed) = band.apply_changed(&mut self.map) else {
                continue;
            };
            if let Some(cooked) = self.cooked_samples.as_mut() {
                let w = (band.rect.col_end - band.rect.col_start) as usize;
                let stride = self.map.width() as usize;
                for (r, row) in (band.rect.row_start..band.rect.row_end).enumerate() {
                    let base = row as usize * stride + band.rect.col_start as usize;
                    cooked[base..base + w].copy_from_slice(&band.samples[r * w..(r + 1) * w]);
                }
            }
            let Some(rect) = changed else {
                continue;
            };
            self.note_edit_box(&rect, before);
            gi_edits.push([rect.col_start, rect.row_start, rect.col_end, rect.row_end]);
            self.pyramid.update_region(&self.map, &rect);
            self.gpu
                .update_heights_with_range(&self.map, &rect, self.altitude_range());
            if let Some(proxy) = self.trace_proxy.as_mut() {
                proxy.mark_dirty(&rect);
            }
            applied += 1;
        }
        if applied > 0 {
            self.heights_generation += 1;
            // The probes' max pyramid re-reduces only these samples' texels (ADR-7659).
            self.gi_edits = gi_edits;
            self.surface.heights_changed();
            // Every camera's selection was made against the old pyramid bounds.
            self.selected_eye = None;
            for view in self.views.values_mut() {
                view.selected_eye = None;
            }
        }
        applied
    }

    /// Records the world box of one applied band's changed samples (ADR-7585): the cascades
    /// test it, and the grass selects again only when it meets the grass range. `before` is the
    /// height range under the band before the write.
    fn note_edit_box(&mut self, rect: &crate::deform::DirtyRect, before: [f32; 2]) {
        let around = grow_rect(rect, &self.map);
        let after = rect_min_max(&self.map, &around);
        let ([x0, z0], [x1, z1]) = crate::PatchTarget::of(&self.map).footprint(&around);
        let lo = [x0, before[0].min(after[0]), z0];
        let hi = [x1, before[1].max(after[1]), z1];
        self.edit_log.push((lo, hi));
        self.stats.edit_boxes.fetch_add(1, Ordering::Relaxed);
        let grass_meets = self
            .grass
            .as_ref()
            .is_none_or(|grass| grass.meets_heights_box([lo[0], lo[2]], [hi[0], hi[2]]));
        if grass_meets {
            self.grass_generation += 1;
            if self.grass.is_some() {
                self.stats.grass_edit_reselects.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn rendered_heights_uncut(&self, points: &[[f32; 2]]) -> Vec<f32> {
        let Some(eye) = self.selected_eye else {
            return points
                .iter()
                .map(|p| self.map.sample_bilinear(p[0], p[1]))
                .collect();
        };
        self.gpu.with_height_mips(|mips| {
            points
                .iter()
                .map(|p| {
                    let draw = self.selection.nodes.iter().find(|draw| {
                        let (min_x, min_z, size) = draw.node.map_rect(&self.map, self.root_level);
                        p[0] >= min_x && p[0] <= min_x + size && p[1] >= min_z && p[1] <= min_z + size
                    });
                    match draw {
                        Some(draw) => crate::lod_error::rendered_height(
                            &self.map,
                            mips,
                            &self.config,
                            self.root_level,
                            draw,
                            eye,
                            p[0],
                            p[1],
                        ),
                        None => self.map.sample_bilinear(p[0], p[1]),
                    }
                })
                .collect()
        })
    }

    /// A cloneable handle on the surface this draw puts on screen: it keeps answering after
    /// the draw is boxed into a `FrameRenderer`, from the selection of the last
    /// [`SharedPassDraw::prepare`]. Scatter seats rocks on it
    /// ([`crate::ScatterField::set_rendered_surface`]).
    #[must_use]
    pub fn surface(&self) -> RenderedSurface {
        self.surface.clone()
    }

    /// Replaces the splat layer stack after construction (bands, tints, tiling, blend
    /// knobs). Textures installed by [`Self::set_material`] or
    /// [`Self::set_material_cooked`] stay bound slot for slot.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError::InvalidLayers`] when the stack fails validation; the
    /// previous stack stays in use.
    pub fn set_layers(&mut self, layers: &TerrainLayers) -> Result<(), TerrainError> {
        self.gpu.set_layers(layers)?;
        self.reference_layers = layers.clone();
        Ok(())
    }

    /// Installs the albedo [`SharedPassDraw::reference_heightfield`] reports for the current
    /// heights (sRGB RGBA8 over the whole map, row 0 at the map's −Z edge): the shader's own
    /// albedo baked top-down (`regia_pt::terrain_bake`). Without one, or after the heights
    /// change, the reference albedo is the first-order layer-stack estimate. `None` clears it.
    pub fn set_reference_albedo(&mut self, albedo: Option<regia_render::LabTexture>) {
        self.reference_albedo = albedo.map(|t| (self.heights_generation, t));
    }

    /// Installs the photoreal material from in-memory textures (per-layer albedo, normal,
    /// height, ORM and the macro set) and switches the shared colour pass onto the
    /// material path. `macro_resident` names the macro tiles to upload (`&[]` = all).
    /// See [`TerrainGpu::set_material`].
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when the material fails validation.
    pub fn set_material(
        &mut self,
        material: &crate::material::TerrainMaterial,
        macro_resident: &[u32],
    ) -> Result<(), TerrainError> {
        self.gpu.set_material(material, macro_resident)?;
        self.reference_layer_means = material
            .layers
            .iter()
            .map(|l| l.as_ref().map(|t| reference::mean_linear(&t.albedo.data)))
            .collect();
        // The layer tint multiplies the texture: report what the ground comes to (ADR-1425).
        let report = crate::layer_albedo_report(&self.reference_layers, &self.reference_layer_means);
        for layer in &report {
            if let Some(warning) = layer.warning {
                tracing::warn!(
                    target: "regia_terrain",
                    layer = %layer.name,
                    tint = ?layer.tint,
                    texture_mean = ?layer.texture_mean,
                    effective = ?layer.effective,
                    "terrain layer albedo: {warning}"
                );
            }
        }
        self.ground_look.set_layer_albedo(report);
        Ok(())
    }

    /// Whether the shared colour pass runs the surface-weather variant, and how many frames did
    /// (spec 99 § 4.5).
    #[must_use]
    pub fn surface_weather_report(&self) -> crate::TerrainSurfaceWeatherReport {
        self.gpu.surface_weather_report()
    }

    /// What each layer's albedo comes to on screen (tint × texture mean) and any warning, as of
    /// the last [`Self::set_material`] (ADR-1425; `regia_terrain::layer_albedo_report`).
    #[must_use]
    pub fn layer_albedo_report(&self) -> Vec<crate::LayerAlbedo> {
        self.ground_look.layer_albedo()
    }

    /// Sets the ground albedo the engine's irradiance probes see across the map: `rgba` is
    /// sRGB-encoded RGBA8 over the whole map (row 0 at −Z), `None` returns to one albedo
    /// for the whole map. A host that knows the terrain's source textures (the cooked path
    /// cannot read its compressed ones) builds this the way the reference tracer does.
    pub fn set_gi_albedo_map(&mut self, map: Option<&crate::material::Rgba8Image>) {
        let generation = self.gi_albedo_map.as_ref().map_or(0, |(_, g)| *g) + 1;
        self.gi_albedo_map =
            map.map(|image| (self.gpu.upload_srgb_image("terrain gi albedo", image), generation));
    }

    /// Bakes the terrain's albedo as this draw's own shader computes it, over the whole map:
    /// sRGB RGBA8, row 0 at the map's −Z edge, column 0 at −X. Renders the map top-down
    /// through the shared colour pass's "blended albedo" debug view (`debug_view == 3`), one
    /// orthographic tile of `tile_px`² texels at a time, `texel_m` metres per texel, the
    /// distance-faded terms evaluated from `view_height_m` above each tile. Call before the
    /// renderer's first frame with this draw (the next `prepare` rewrites the uniforms it set).
    ///
    /// # Errors
    ///
    /// The LOD pyramid or an offscreen render failed.
    pub fn bake_albedo_map(
        &mut self,
        device: &RenderDevice,
        texel_m: f32,
        tile_px: u32,
        view_height_m: f32,
    ) -> Result<crate::material::Rgba8Image, TerrainError> {
        use regia_core::glam::camera::rh::proj::directx::orthographic;
        use regia_core::glam::camera::rh::view::look_at_mat4;
        use regia_core::glam::{Mat4, Vec3};
        let _span = tracing::info_span!("terrain_albedo_bake").entered();
        let map = self.map().clone();
        let config = self.config().clone();
        let root_level = self.root_level();
        let pyramid = MinMaxPyramid::build(&map, root_level)?;
        let o = map.sample_to_world(0, 0);
        let far = map.sample_to_world(map.width() - 1, map.height() - 1);
        let extent = [far[0] - o[0], far[1] - o[1]];
        let texel = texel_m.max(0.05);
        let res = [
            ((extent[0] / texel).ceil() as u32).clamp(2, 16_384),
            ((extent[1] / texel).ceil() as u32).clamp(2, 16_384),
        ];
        let (lo, hi) = map
            .heights()
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &h| {
                (a.min(h), b.max(h))
            });
        let tile = tile_px.clamp(16, 2048);
        let step = [extent[0] / res[0] as f32, extent[1] / res[1] as f32];
        let mut rgba = vec![0u8; (res[0] * res[1] * 4) as usize];
        self.gpu.set_linear_output(false);
        for ty in 0..res[1].div_ceil(tile) {
            for tx in 0..res[0].div_ceil(tile) {
                // Tile rect in world X/Z (texel edges) and an orthographic view looking down
                // with image-up = −Z, so row 0 is the tile's −Z edge.
                let x0 = o[0] + (tx * tile) as f32 * step[0];
                let z0 = o[1] + (ty * tile) as f32 * step[1];
                let half = [0.5 * tile as f32 * step[0], 0.5 * tile as f32 * step[1]];
                let centre = [x0 + half[0], z0 + half[1]];
                let top = hi + 50.0;
                let eye = Vec3::new(centre[0], top, centre[1]);
                let view = look_at_mat4(eye, eye - Vec3::Y, Vec3::NEG_Z);
                let proj = orthographic(-half[0], half[0], -half[1], half[1], 1.0, top - lo + 100.0);
                let view_proj: Mat4 = proj * view;
                let ground = map.sample_bilinear(centre[0], centre[1]);
                let frame = TerrainFrame {
                    view_proj: view_proj.to_cols_array_2d(),
                    camera_pos: [centre[0], ground + view_height_m, centre[1]],
                    sun_dir: [0.0, 1.0, 0.0],
                    sun_color: [1.0; 3],
                    illuminance_lux: 0.0,
                    ambient: [0.0; 3],
                    wetness: 0.0,
                    time_seconds: 0.0,
                    debug_view: 3,
                    fog: regia_render::FogSettings::default(),
                    shadow_focus: [centre[0], ground, centre[1]],
                    shadow_radius: half[0].max(half[1]),
                    exposure_compensation_ev: 0.0,
                };
                let selection = select_nodes(&map, &pyramid, &config, root_level, centre, None);
                let pixels = super::render_offscreen_opts(
                    &mut self.gpu,
                    device,
                    &frame,
                    &selection,
                    tile,
                    tile,
                    [0.0, 0.0, 0.0, 1.0],
                    false,
                )?;
                for j in 0..tile {
                    let row = ty * tile + j;
                    if row >= res[1] {
                        break;
                    }
                    let cols = tile.min(res[0] - tx * tile);
                    let src = (j * tile * 4) as usize;
                    let dst = ((row * res[0] + tx * tile) * 4) as usize;
                    rgba[dst..dst + (cols * 4) as usize]
                        .copy_from_slice(&pixels[src..src + (cols * 4) as usize]);
                }
            }
        }
        // Debug views write alpha 1; keep the image opaque regardless.
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        Ok(crate::material::Rgba8Image {
            width: res[0],
            height: res[1],
            data: rgba,
        })
    }

    /// The terrain's albedo as this draw's own shader computes it for one camera: the
    /// "blended albedo" debug view (`debug_view == 3`) rendered from `eye` through
    /// `view_proj` (a 0..1-depth projection, glam's `*_rh` directx convention) at
    /// `width`×`height`, so every texel carries the layer weights, mips and distance-faded
    /// near-field terms the frame shades that pixel with. sRGB RGBA8, top row first; alpha
    /// 255 where terrain covers the pixel, 0 elsewhere. Terrain only (meshes do not occlude).
    /// Call before the renderer's first frame with this draw, like [`Self::bake_albedo_map`].
    ///
    /// # Errors
    ///
    /// The LOD pyramid or the offscreen render failed.
    pub fn render_albedo_view(
        &mut self,
        device: &RenderDevice,
        view_proj: [[f32; 4]; 4],
        eye: [f32; 3],
        width: u32,
        height: u32,
    ) -> Result<crate::material::Rgba8Image, TerrainError> {
        let _span = tracing::info_span!("terrain_albedo_view").entered();
        self.render_debug_view(device, 3, view_proj, eye, width, height)
    }

    /// The shading normal this draw's shader lights each pixel with, for one camera: debug view
    /// 16 (world normal as 0.5 + 0.5·n, sRGB-encoded like any colour) rendered as
    /// [`Self::render_albedo_view`] renders the albedo: same camera convention, layout and
    /// alpha coverage, same timing rule.
    ///
    /// # Errors
    ///
    /// The LOD pyramid or the offscreen render failed.
    pub fn render_normal_view(
        &mut self,
        device: &RenderDevice,
        view_proj: [[f32; 4]; 4],
        eye: [f32; 3],
        width: u32,
        height: u32,
    ) -> Result<crate::material::Rgba8Image, TerrainError> {
        let _span = tracing::info_span!("terrain_normal_view").entered();
        self.render_debug_view(device, 16, view_proj, eye, width, height)
    }

    /// One camera view of the terrain through the shared colour pass's debug view `view`
    /// (sRGB offscreen target, cleared to alpha 0).
    fn render_debug_view(
        &mut self,
        device: &RenderDevice,
        view: u32,
        view_proj: [[f32; 4]; 4],
        eye: [f32; 3],
        width: u32,
        height: u32,
    ) -> Result<crate::material::Rgba8Image, TerrainError> {
        let map = self.map().clone();
        let config = self.config().clone();
        let root_level = self.root_level();
        let pyramid = MinMaxPyramid::build(&map, root_level)?;
        let frame = TerrainFrame {
            view_proj,
            camera_pos: eye,
            sun_dir: [0.0, 1.0, 0.0],
            sun_color: [1.0; 3],
            illuminance_lux: 0.0,
            ambient: [0.0; 3],
            wetness: 0.0,
            time_seconds: 0.0,
            debug_view: view,
            fog: regia_render::FogSettings::default(),
            shadow_focus: eye,
            shadow_radius: 48.0,
            exposure_compensation_ev: 0.0,
        };
        let selection = select_nodes(&map, &pyramid, &config, root_level, [eye[0], eye[2]], None);
        self.gpu.set_linear_output(false);
        let data = super::render_offscreen_opts(
            &mut self.gpu,
            device,
            &frame,
            &selection,
            width.max(1),
            height.max(1),
            [0.0, 0.0, 0.0, 0.0],
            false,
        )?;
        Ok(crate::material::Rgba8Image {
            width: width.max(1),
            height: height.max(1),
            data,
        })
    }

    /// Bakes the albedo map ([`Self::bake_albedo_map`], 2 m texels) and gives it to the
    /// engine's irradiance probes ([`Self::set_gi_albedo_map`]), so the ground their rays hit
    /// has the albedo the frame shades it with. Hosts call it once at load, before the first
    /// frame, when the probes may run (the Depth lighting option); without it the probes see
    /// one albedo for the whole map.
    ///
    /// # Errors
    ///
    /// As [`Self::bake_albedo_map`].
    pub fn bake_gi_albedo_map(&mut self, device: &RenderDevice) -> Result<(), TerrainError> {
        let image = self.bake_albedo_map(device, 2.0, 256, 150.0)?;
        self.set_gi_albedo_map(Some(&image));
        Ok(())
    }

    /// Mean albedo of the layer stack (tint × texture mean where the texture is known).
    fn layer_albedo(&self) -> [f32; 3] {
        let tints = &self.reference_layers.layers;
        if tints.is_empty() {
            return [0.5; 3];
        }
        let mut sum = [0.0f32; 3];
        for (i, tint) in tints.iter().map(|l| l.albedo).enumerate() {
            let tex = self
                .reference_layer_means
                .get(i)
                .copied()
                .flatten()
                .unwrap_or([1.0; 3]);
            for c in 0..3 {
                sum[c] += tint[c] * tex[c];
            }
        }
        sum.map(|v| (v / tints.len() as f32).clamp(0.02, 0.95))
    }

    /// Leaves the top `skip` levels of the cooked layer arrays out of the next
    /// [`Self::set_material_cooked`] (a memory tier; [`crate::layer_mip_skip_for`]).
    pub fn set_layer_mip_skip(&mut self, skip: u32) {
        self.gpu.set_layer_mip_skip(skip);
    }

    /// Installs or removes splat weight packs over the whole map: `macro/pack0.png` and `macro/pack1.png` of
    /// the cook convention (`docs/formats/terrain.md` § 9). See [`TerrainGpu::set_splat_weights`].
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when the packs differ in size or exceed the texture limit.
    pub fn set_splat_weights(
        &mut self,
        packs: Option<[&crate::material::Rgba8Image; 2]>,
    ) -> Result<(), TerrainError> {
        self.gpu.set_splat_weights(packs)
    }

    /// Installs a cooked (KTX2, block-compressed) photoreal material: the layer arrays
    /// and macro tiles a terrain cook produces. See [`TerrainGpu::set_material_cooked`].
    /// The blobs may be views of a mapped pak: pass what
    /// [`PakTileSource::load_cooked_material`] returns, no copy (ADR-8024).
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when a blob cannot be parsed or uploaded.
    pub fn set_material_cooked<B: AsRef<[u8]>>(
        &mut self,
        cooked: &crate::material::CookedMaterialKtx2<B>,
        params: crate::material::MaterialParams,
        macro_tiles: u32,
        macro_resident: &[u32],
    ) -> Result<(), TerrainError> {
        self.gpu
            .set_material_cooked(cooked, params, macro_tiles, macro_resident)
    }

    /// Terrain debug view of the shared colour pass (`TerrainFrame::debug_view`: 0 = off,
    /// 1 LOD tint, 2 N·L, 3 albedo, 4 layer weights, 5 slope, 6 height, 7 sim agreement,
    /// 8 sun light split: R cascade shadow, G heightfield shadow times micro shadow, B N·L,
    /// each times the views' scale (R×G is the lit visibility only when one shadow is 0 or 1);
    /// 9 indirect split, unscaled: R sky/ground ambient, G planet-shine, B bounce before probes;
    /// 10 near-field detail: R strength, G icy (crust), B frost coverage; 11 cascade shadow
    /// alone, grey; 12 cascade occluder depth; 13 far heightfield sun; 14 indirect occlusion:
    /// R heightfield cavity, G macro AO, B probe coverage; 15 indirect occlusion: R layers' ORM
    /// occlusion, G SSAO, B 1 (the views' scale); 16 shading normal, 0.5 + 0.5·n).
    /// [`crate::TERRAIN_DEBUG_VIEWS`] names them.
    pub fn set_debug_view(&mut self, view: u32) {
        self.debug_view.set(view);
    }

    /// The draw's debug view as a shared handle: a host keeps it (or inserts
    /// [`crate::TerrainDebugViewControl`] for the agent) to switch the view after the draw has
    /// gone into the renderer. The draw reads it at its next prepare.
    #[must_use]
    pub fn debug_view_handle(&self) -> crate::TerrainDebugViewHandle {
        self.debug_view.clone()
    }

    /// The planet-shine fill's visibility on the ground: `true` = the canyon-scale macro AO and
    /// the SSAO only (meshes take the fill without AO); `false` (default) = the full indirect
    /// occlusion stack. See [`super::TerrainGpu::set_fill_canyon_visibility`]. `true` is
    /// [`crate::FillCanyonScope::MapWide`], `false` [`crate::FillCanyonScope::Off`]
    /// ([`Self::set_fill_canyon_scope`]).
    pub fn set_fill_canyon_visibility(&mut self, on: bool) {
        self.set_fill_canyon_scope(if on {
            crate::FillCanyonScope::MapWide
        } else {
            crate::FillCanyonScope::Off
        });
    }

    /// Where the planet-shine fill on the ground takes the canyon-scale visibility only
    /// (ADR-8412; default [`crate::FillCanyonScope::Off`]). On airless maps with hull boxes sent
    /// as mesh occluders, [`crate::FillCanyonScope::NearOccluders`] lifts the fill in the shade
    /// under and beside the hulls without lifting every trench and canyon floor of the map
    /// ([`super::TerrainGpu::set_fill_canyon_scope`]). Stored in the draw's
    /// [`Self::ground_look_handle`], so the agent's `terrain.get_ground_look` reports it and
    /// `terrain.set_ground_look` changes it.
    pub fn set_fill_canyon_scope(&mut self, scope: crate::FillCanyonScope) {
        self.ground_look.set_fill_canyon_scope(scope);
        self.gpu.set_fill_canyon_scope(scope);
    }

    /// The scope in effect ([`Self::set_fill_canyon_scope`]).
    #[must_use]
    pub fn fill_canyon_scope(&self) -> crate::FillCanyonScope {
        self.ground_look.fill_canyon_scope()
    }

    /// Whether the ground takes the frame's point and spot lights (default on where the device
    /// has room for them: [`super::TerrainGpu::local_lights_supported`]). Off, the terrain
    /// shades as before batch v020: lamps, muzzle flashes and explosions light meshes only, and its
    /// colour pass is built without the light loop, so it costs what it did before batch v020. The warm
    /// ([`Self::prewarm`], [`crate::terrain_warm_job`]) builds both variants, so the switch
    /// compiles nothing after it.
    pub fn set_local_lights_enabled(&mut self, on: bool) {
        self.gpu.set_local_lights_enabled(on);
    }

    /// Opts this terrain in to the engine's stochastic screen-space reflections (Desktop High
    /// and Ultra): for a map whose ground can be glossy (polished ice, wet rock). Off by
    /// default: rough ground shows no traced reflection, so a landscape map pays nothing for
    /// the pass and keeps no reflection targets. When on, glossy pixels (perceptual roughness
    /// under the engine's cutoff) write a surface code and get their sky reflection traced.
    pub fn set_screen_space_reflections(&mut self, on: bool) {
        self.screen_space_reflections = on;
    }

    /// Sets the ground variation and late-winter surfaces of the photoreal path
    /// (see [`crate::TerrainGroundLook`], ADR-1408; off by default).
    pub fn set_ground_look(&mut self, look: crate::TerrainGroundLook) {
        self.ground_look.set(look);
        self.apply_ground_look();
    }

    /// The handle the draw reads its ground look from at each prepare: insert it as
    /// [`crate::TerrainGroundLookControl`] for the agent, or keep it to change the look later.
    pub fn ground_look_handle(&self) -> crate::GroundLookHandle {
        self.ground_look.clone()
    }

    /// Uploads the handle's look when its generation moved, and takes its fill-canyon scope.
    fn apply_ground_look(&mut self) {
        let (generation, look) = self.ground_look.get();
        if generation != self.ground_look_applied {
            self.ground_look_applied = generation;
            self.gpu.set_ground_look(look);
        }
        self.gpu
            .set_fill_canyon_scope(self.ground_look.fill_canyon_scope());
    }

    /// Sets the strata and near-field relief lighting of the photoreal path
    /// (see [`super::TerrainRelief`]; on by default).
    pub fn set_relief(&mut self, relief: super::TerrainRelief) {
        self.gpu.set_relief(relief);
        self.bounce_strength = if relief.bounce.is_finite() {
            relief.bounce.clamp(0.0, 2.0)
        } else {
            0.0
        };
    }

    /// The ground bounce handed to the renderer for meshes ([`SharedPassDraw::ground_bounce`]).
    #[must_use]
    pub fn ground_bounce_grid(&self) -> Option<&GroundBounce> {
        self.ground_bounce.as_ref()
    }

    /// Recomputes the bounce grid for meshes (linear HDR frames, bounce on). The wall albedo
    /// is the one the frame's ground light implies: `ground = ρ · (E_sun · sin(el) / π + sky)`
    /// (`SkyAmbient::from_atmosphere`), so meshes see the same albedo the ambient bake used.
    fn update_ground_bounce(&mut self, info: &SharedFrameInfo, ambient: &SkyAmbient) {
        if !info.linear_hdr || self.bounce_strength <= 0.0 || info.sun_illuminance <= 0.0 {
            self.ground_bounce = None;
            return;
        }
        let travel = info.sun_dir;
        let len = (travel[0] * travel[0] + travel[1] * travel[1] + travel[2] * travel[2])
            .sqrt()
            .max(1e-6);
        let to_sun = [-travel[0] / len, -travel[1] / len, -travel[2] / len];
        let albedo = implied_albedo(info, ambient);
        let map = &self.map;
        let generation = self.heights_generation;
        let grids = &mut self.bounce_grids;
        let recomputed = self.gpu.with_height_mips(|mips| {
            let mut any = false;
            for (grid, &(cell_m, size, ss)) in grids.iter_mut().zip(BOUNCE_GRIDS.iter()) {
                any |= grid.update(
                    map,
                    mips,
                    [info.eye[0], info.eye[2]],
                    to_sun,
                    generation,
                    cell_m,
                    size,
                    ss,
                );
            }
            any
        });
        let scale: [f32; 3] = std::array::from_fn(|c| {
            albedo[c] * info.sun_illuminance * info.sun_color[c] * self.bounce_strength / std::f32::consts::PI
        });
        let key = scale.map(f32::to_bits);
        if !recomputed && self.ground_bounce.is_some() && self.bounce_scale_key == key {
            return;
        }
        self.bounce_scale_key = key;
        self.ground_bounce = Some(GroundBounce {
            levels: self
                .bounce_grids
                .iter()
                .filter(|g| g.size > 0)
                .map(|g| GroundBounceLevel {
                    origin: g.origin,
                    cell_m: g.cell_m,
                    size: g.size,
                    radiance: g
                        .factor
                        .iter()
                        .map(|f| [scale[0] * f, scale[1] * f, scale[2] * f, 0.0])
                        .collect(),
                    horizon: g.horizon.clone(),
                })
                .collect(),
        });
    }

    /// Overrides the sky light the shared colour pass uses (see [`SkyAmbient`]). `None`
    /// (the default) follows the frame: the planet atmosphere's hemisphere ambient the
    /// renderer lights meshes with (`SharedFrameInfo::sky_ambient`), or, when the frame
    /// has none, the flat `ambient_lux` ([`SkyAmbient::from_flat_lux`]). Hosts normally
    /// leave it unset.
    pub fn set_sky_ambient(&mut self, ambient: Option<SkyAmbient>) {
        self.sky_ambient = ambient;
    }

    /// The sky light set by [`Self::set_sky_ambient`].
    #[must_use]
    pub fn sky_ambient(&self) -> Option<SkyAmbient> {
        self.sky_ambient
    }

    /// Mutable GPU state for setters this type does not forward (road decal, scorch
    /// marks, sim mirror). Heights must still go through [`Self::update_heights`] so the
    /// cached shadow cascades are invalidated.
    pub fn gpu_mut(&mut self) -> &mut TerrainGpu {
        &mut self.gpu
    }
}

impl TerrainSharedDraw {
    /// Updates the cascades' anchored selections and this frame's draw lists.
    /// `camera_changed`: the camera selection was remade this frame.
    fn build_lists(&mut self, info: &SharedFrameInfo, camera_changed: bool) {
        let eye = [info.eye[0], info.eye[2]];
        let inputs = ShadowInputs {
            map: &self.map,
            pyramid: &self.pyramid,
            config: &self.config,
            root_level: self.root_level,
            heights_generation: self.shadow_generation,
            edits: &self.edit_log,
        };
        let mut rebuild = camera_changed || self.draw_list.nodes.is_empty();
        for (cascade, shadow) in self.shadows.iter_mut().enumerate() {
            rebuild |= if (cascade as u32) < info.cascade_count {
                shadow.update(&inputs, eye, info.csm_bias[cascade][0], &info.cascades[cascade])
            } else {
                shadow.clear()
            };
        }
        if rebuild {
            self.draw_list.nodes.clear();
            for (cascade, shadow) in self.shadows.iter().enumerate() {
                self.bases[cascade] = self.draw_list.nodes.len() as u32;
                self.draw_list.nodes.extend_from_slice(&shadow.selection.nodes);
            }
            self.bases[4] = self.draw_list.nodes.len() as u32;
            self.draw_list.nodes.extend_from_slice(&self.selection.nodes);
        }
        let [c0, c1, c2, c3, camera, r0, r1, r2, r3, tile] = &mut self.lists;
        camera.clear();
        camera.extend(self.camera_list.iter().map(|i| i + self.bases[4]));
        // The nodes under the precipitation occlusion tile, from the camera's selection.
        tile.clear();
        if let Some(view) = info.precip_occlusion {
            let selected = self.selection.nodes.len().min(self.node_bounds.len());
            cull_into(tile, &self.node_bounds[..selected], &view.view_proj, [0.0; 2]);
            for slot in tile.iter_mut() {
                *slot += self.bases[4];
            }
        }
        for (cascade, (list, region)) in [(c0, r0), (c1, r1), (c2, r2), (c3, r3)].into_iter().enumerate() {
            let shadow = &self.shadows[cascade];
            let base = self.bases[cascade];
            list.clear();
            list.extend(shadow.list.iter().map(|i| i + base));
            // Nodes under the pages the renderer redraws for moved meshes; the rest of
            // the cascade already holds them.
            shadow.region(info.shadow_regions[cascade], region);
            for slot in region.iter_mut() {
                *slot += base;
            }
        }
        // The host camera's passes, for `terrain.seam_report` (ADR-8470).
        if self.active_view.is_none() {
            const CASCADES: [&str; 4] = ["cascade0", "cascade1", "cascade2", "cascade3"];
            let main = std::iter::once(("main", eye, self.selection.nodes.as_slice()));
            let cascades = self.shadows.iter().zip(CASCADES).filter_map(|(shadow, name)| {
                shadow.eye().map(|e| (name, e, shadow.selection.nodes.as_slice()))
            });
            self.draw_passes.publish(main.chain(cascades));
        }
    }
}

impl TerrainSharedDraw {
    /// Sets the open mouths' bands this draw places and draws in every pass (spec 26 § 2a.4 step
    /// 6, ADR-8569; [`crate::MouthBands::from_layers`]). `None` draws none. The agent and the
    /// `TerrainMeshLayers` sync request them through [`Self::holes_handle`].
    pub fn set_mouth_bands(&mut self, bands: Option<Arc<crate::mesh_layer::MouthBands>>) {
        self.gpu.set_mouth_bands(bands);
        self.band_keys = [None; super::BAND_PASSES];
        self.band_placed = Default::default();
        self.view_band_owner = None;
        self.view_band_keys = [None; super::BAND_PASSES];
        self.draw_passes.clear_bands();
    }

    /// Places the bands on each pass's selection (spec 26 § 2a.4 step 5): set 0 on the camera's
    /// selection and eye, set `1 + i` on cascade `i`'s anchored selection and morph eye. A set
    /// whose key (selection, eye, heights, bands) is unchanged is neither placed nor uploaded.
    /// The main camera writes band camera 0; a texture view writes band camera 1 with its own
    /// selection and cascades, again whenever another view wrote it last (ADR-8824). Only the
    /// main camera's placements are published to [`Self::draw_passes_handle`].
    fn place_bands(&mut self, info: &SharedFrameInfo, upload: &mut SharedUpload<'_>) {
        let Some(bands) = self
            .gpu
            .mouth_bands()
            .map(|b| (b.generation, b.ring_points.len()))
        else {
            return;
        };
        let view = self.active_view;
        if view.is_some() && self.view_band_owner != view {
            self.view_band_keys = [None; super::BAND_PASSES];
            self.view_band_owner = view;
        }
        let mut keys = if view.is_some() {
            self.view_band_keys
        } else {
            self.band_keys
        };
        let _span = tracing::info_span!("terrain_place_bands", ring = bands.1).entered();
        let eye = [info.eye[0], info.eye[2]];
        let mut sets: [Option<(u64, [f32; 2])>; super::BAND_PASSES] = [None; super::BAND_PASSES];
        sets[0] = Some((self.selection_key, eye));
        for (i, shadow) in self.shadows.iter().enumerate() {
            sets[1 + i] = shadow.eye().map(|e| (shadow.key, e));
        }
        for (set, entry) in sets.into_iter().enumerate() {
            let Some((selection_key, morph_eye)) = entry else {
                keys[set] = None;
                continue;
            };
            let key = (
                selection_key,
                morph_eye.map(f32::to_bits),
                self.shadow_generation,
                bands.0 ^ (bands.1 as u64).rotate_left(32),
            );
            if keys[set] == Some(key) {
                continue;
            }
            let selection = if set == 0 {
                &self.selection
            } else {
                &self.shadows[set - 1].selection
            };
            let (map, config, root) = (&self.map, &self.config, self.root_level);
            let gpu = &self.gpu;
            let placed = gpu.with_height_mips(|mips| {
                gpu.mouth_bands()
                    .map(|b| b.place(map, config, root, mips, selection, morph_eye))
            });
            if let Some(placed) = placed {
                gpu.write_band_positions(set, &placed, upload);
                if view.is_none() {
                    self.band_placed[set] = placed;
                }
            }
            keys[set] = Some(key);
        }
        if view.is_some() {
            self.view_band_keys = keys;
            return;
        }
        self.band_keys = keys;
        const NAMES: [&str; super::BAND_PASSES] = ["main", "cascade0", "cascade1", "cascade2", "cascade3"];
        for (name, placed) in NAMES.into_iter().zip(&self.band_placed) {
            self.draw_passes.set_band(name, placed);
        }
    }
}

/// A band set's placement key: selection or cascade key, morph eye bits, heights generation,
/// bands generation (ADR-8569).
type BandKey = (u64, [u32; 2], u64, u64);

/// One hash of a band set's key for the cascade invalidation key.
fn band_key_hash(key: &BandKey) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

/// `rect` grown by one sample on every side, clamped to `map`.
fn grow_rect(rect: &crate::deform::DirtyRect, map: &TerrainMap) -> crate::deform::DirtyRect {
    crate::deform::DirtyRect {
        col_start: rect.col_start.saturating_sub(1),
        col_end: (rect.col_end + 1).min(map.width()),
        row_start: rect.row_start.saturating_sub(1),
        row_end: (rect.row_end + 1).min(map.height()),
    }
}

/// Lowest and highest height of `map` over `rect` (`[inf, -inf]` for an empty one).
fn rect_min_max(map: &TerrainMap, rect: &crate::deform::DirtyRect) -> [f32; 2] {
    let stride = map.width() as usize;
    let mut out = [f32::INFINITY, f32::NEG_INFINITY];
    for row in rect.row_start..rect.row_end {
        for col in rect.col_start..rect.col_end {
            let h = map.height_at_index(row as usize * stride + col as usize);
            out = [out[0].min(h), out[1].max(h)];
        }
    }
    out
}

/// The draw list of the precipitation occlusion tile, after the camera's and the cascades'.
const PRECIP_TILE_LIST: usize = 9;

/// Smallest grid (cells per node edge) the runtime frame draws: with the default range factor
/// a cell projects to about 16 px at 1440p, and the far surface departs from the true
/// heightfield by 0.22 m at most inside 200 m on rolling ground with 0.25 m crust (16 cells:
/// 0.36 m; 64 cells: 0.105 m but over the terrain budget; `lod_error` tests).
pub const SHARED_MIN_GRID_SIZE: u32 = 32;

/// The shallowest quadtree whose leaf cells (`world / 2^root / grid_size`) are no larger
/// than one heightfield texel, capped at 12 levels.
#[must_use]
pub fn full_detail_root_level(map: &TerrainMap, config: &CdlodConfig) -> u32 {
    let cells_needed =
        (map.width().max(map.height()).saturating_sub(1)) as f32 / config.grid_size.max(1) as f32;
    let mut root = 0u32;
    while root < 12 && ((1u32 << root) as f32) < cells_needed {
        root += 1;
    }
    root
}

fn cooked_error(err: impl std::fmt::Display) -> TerrainError {
    TerrainError::Gpu(regia_render::RenderError::Readback(err.to_string()))
}

impl SharedPassDraw for TerrainSharedDraw {
    fn screen_space_reflective(&self) -> bool {
        self.screen_space_reflections
    }

    fn warm_pipelines(&mut self) {
        if let Some(format) = self.gpu.shared_format() {
            self.gpu.warm_shared(format);
            // The blades' weather variant rides on the terrain's weather warm (spec 99 § 6.4).
            if let Some(grass) = self.grass.as_mut() {
                grass.warm_weather(&self.gpu, format);
            }
        }
        // The other formats this terrain was drawn into (a probe capture's target).
        self.gpu.warm_seen_formats();
    }

    fn prepare(&mut self, info: &SharedFrameInfo, upload: &mut SharedUpload<'_>) {
        let _span = tracing::info_span!("terrain_prepare").entered();
        let started = std::time::Instant::now();
        // The eye as the frame gives it (local space): the outdoor frusta are matched on it.
        let frame_eye = info.eye;
        // The terrain stays in its own world-anchored space: a floating-origin rebase moves only
        // this boundary (spec 78 § 12.2, ADR-2022). Zero until the first rebase: the same frame.
        let world_space;
        let info = if info.origin_offset == [0.0; 3] {
            info
        } else {
            world_space = info.in_world_space();
            &world_space
        };
        self.origin_offset = info.origin_offset;
        self.apply_ground_look();
        self.gpu.set_linear_output(info.linear_hdr);
        self.gpu
            .set_hdr_scale(if info.linear_hdr { info.hdr_scale } else { 1.0 });
        self.gpu.set_budget(&info.terrain_budget);
        self.gpu.set_texture_lod_bias(info.terrain_texture_lod_bias);
        self.gpu
            .set_sssr_codes(self.screen_space_reflections && info.sssr_codes && info.linear_hdr);
        self.gpu
            .set_motion_matrices(info.prev_view_proj, info.unjittered_view_proj);
        if let Some(mask) = self.holes_handle.take_request() {
            self.set_holes(Some(&mask));
        }
        if let Some(lattice) = self.holes_handle.take_lattice_request() {
            self.set_hole_lattice(lattice);
        }
        if let Some(bands) = self.holes_handle.take_bands_request() {
            self.set_mouth_bands(bands);
        }
        self.apply_height_patches();
        self.gpu.ensure_shared(info.color_format);
        // `terrain.set_vt` (agent protocol): a virtual-texture change requested on the handle.
        let vt_handle = self.gpu.vt_handle();
        if let Some(request) = vt_handle.take_request() {
            let result = self.gpu.set_virtual_texture(request);
            vt_handle.set_error(result.err().map(|e| e.to_string()));
        }
        // The profile's lever (ADR-157 addendum v0.1.2), unless the host or the agent chose above.
        self.gpu.set_virtual_texture_from_budget(&info.terrain_budget);
        let travel = info.sun_dir;
        // Hemisphere sky/ground ambient (`SkyAmbient`). Precedence: the host override,
        // then the planet's atmosphere bake the meshes are lit with this frame
        // (`SharedFrameInfo::sky_ambient`; airless = ground bounce only), then the flat
        // fallback, which matches the mesh path with IBL off on up-facing ground (zenith
        // `sky_color(+Y)` · `ambient_lux / π`, lab_forward.wgsl) plus a neutral ground
        // bounce for walls and overhangs. The linear HDR target is not pre-exposed; the
        // post stack applies EV100.
        let ambient = self
            .sky_ambient
            .or_else(|| info.sky_ambient.map(|[sky, ground]| SkyAmbient { sky, ground }))
            .unwrap_or_else(|| SkyAmbient::from_flat_lux(info.ambient_lux));
        // `SkyAmbient` is physical radiance. The linear HDR target takes it as is (post
        // exposes the whole frame); on an LDR frame the terrain shader tonemaps itself and
        // `TerrainFrame::ambient` is pre-exposed, exactly like the sun term's
        // `illuminance · REFERENCE_EXPOSURE`. Without this the airless ambient of an LDR frame
        // came out ~10⁴× too strong against the sun (DO27 pin 1132).
        // The layers' own mean first: the atmosphere's ground light is a sky-model guess, and
        // where it disagreed with the terrain's material (Mars's implied ground vs a 0.18
        // test ground) a mesh on the terrain took a different bounce than the terrain beside it.
        self.gi_albedo = if self.reference_layer_means.iter().any(Option::is_some) {
            self.layer_albedo()
        } else if ambient.ground.iter().sum::<f32>() > 1e-3 {
            implied_albedo(info, &ambient)
        } else {
            self.layer_albedo()
        };
        {
            let _bounce = tracing::info_span!("terrain_bounce").entered();
            self.update_ground_bounce(info, &ambient);
        }
        let ambient = if info.linear_hdr {
            ambient
        } else {
            let e = super::REFERENCE_EXPOSURE;
            SkyAmbient {
                sky: ambient.sky.map(|c| c * e),
                ground: ambient.ground.map(|c| c * e),
            }
        };
        self.gpu.set_ground_ambient(Some(ambient.ground));
        self.gpu.set_fill_light(info.fill_light);
        self.gpu.set_extra_lights(info.extra_directional);
        let frame = TerrainFrame {
            view_proj: info.view_proj,
            camera_pos: info.eye,
            // The frame uniform points toward the sun. The engine stores travel.
            sun_dir: [-travel[0], -travel[1], -travel[2]],
            sun_color: info.sun_color,
            illuminance_lux: info.sun_illuminance,
            ambient: ambient.sky,
            wetness: 0.0,
            time_seconds: 0.0,
            debug_view: self.debug_view.get(),
            fog: info.fog,
            shadow_focus: [info.eye[0], 0.0, info.eye[2]],
            shadow_radius: 48.0,
            exposure_compensation_ev: 0.0,
        };
        let camera_changed;
        // Inside a cell, the main view sees the outdoors only through the portal frusta (plan row
        // W-22, ADR-8413): the rest lies behind the cell's walls. The nodes, the grass patches
        // and the impostor tiles are culled through them (ADR-8843).
        let restricted = self.active_view.is_none() && self.outdoor_portals.restricts(frame_eye);
        if restricted {
            self.outdoor_portals
                .shift_into(info.origin_offset, &mut self.outdoor_world);
        }
        {
            let _select = tracing::info_span!("terrain_select").entered();
            let eye = [info.eye[0], info.eye[2]];
            // CDLOD selection depends only on the eye and the map. A held camera
            // keeps last frame's list (and its shadow key).
            camera_changed = self.selected_eye != Some(eye);
            if camera_changed {
                self.selection =
                    select_nodes(&self.map, &self.pyramid, &self.config, self.root_level, eye, None);
                self.selected_eye = Some(eye);
                self.selection_key = selection_key(&self.selection);
            }
            // Placement reads the main camera's surface: a texture view publishes nothing.
            if camera_changed && self.active_view.is_none() {
                self.surface.publish(SurfaceSnapshot {
                    nodes: self
                        .selection
                        .nodes
                        .iter()
                        .map(|d| (d.node, *d))
                        .collect::<NodeMap>(),
                    selection_key: self.selection_key,
                    eye,
                    config: self.config.clone(),
                    root_level: self.root_level,
                    mips: self.gpu.height_mips_handle(),
                });
            }
            if camera_changed {
                // Cliff relief moves steep-face vertices up to `CLIFF_RELIEF_MAX_M`.
                let pad = crate::gpu::CLIFF_RELIEF_MAX_M;
                self.node_bounds = self
                    .selection
                    .nodes
                    .iter()
                    .map(|draw| {
                        let (min_x, min_z, size) = draw.node.map_rect(&self.map, self.root_level);
                        let [lo, hi] = self.pyramid.node_min_max(draw.node);
                        (
                            [min_x - pad, lo - pad, min_z - pad],
                            [min_x + size + pad, hi + pad, min_z + size + pad],
                        )
                    })
                    .collect();
            }
            // The camera passes draw only nodes in the view frustum; each cascade
            // draws only nodes inside its box (a caster behind the camera still
            // shadows what the camera sees, so the cascades use their own test).
            let selected = self.selection.nodes.len().min(self.node_bounds.len());
            // Unjittered, widened by two pixels: covers every TAA jitter, so a held
            // camera keeps the same list (and uploads nothing) from frame to frame.
            let slack = [4.0 / info.width.max(1) as f32, 4.0 / info.height.max(1) as f32];
            cull_into(
                &mut self.camera_list,
                &self.node_bounds[..selected],
                &info.unjittered_view_proj,
                slack,
            );
            let mut portal_culled = 0u32;
            if restricted {
                let (bounds, portals) = (&self.node_bounds, &self.outdoor_world);
                self.camera_list.retain(|&index| {
                    let (lo, hi) = bounds[index as usize];
                    let keep = portals.admits_box(lo, hi, regia_render::viscells::OUTDOOR_NODE_MARGIN_M);
                    portal_culled += u32::from(!keep);
                    keep
                });
            }
            if self.active_view.is_none() {
                self.outdoor_counts = regia_render::viscells::OutdoorCullCounts {
                    restricted,
                    nodes_drawn: self.camera_list.len() as u32,
                    nodes_culled: portal_culled,
                    ..Default::default()
                };
            }
        }
        {
            let _lists = tracing::info_span!("terrain_lists").entered();
            self.build_lists(info, camera_changed);
        }
        // Every camera places its own bands: the main camera and each texture view (ADR-8824).
        self.place_bands(info, upload);
        // Impostors cull on the GPU for the main camera; a texture view draws none.
        let portals = restricted.then_some(&self.outdoor_world);
        if self.active_view.is_none()
            && let Some(impostors) = self.impostors.as_mut()
        {
            impostors.prepare(&self.gpu, info, upload, portals);
            let (kept, culled) = impostors.portal_counts();
            self.outdoor_counts.impostors_kept = kept;
            self.outdoor_counts.impostors_culled = culled;
        }
        // Grass grows around the main camera only, like the impostors.
        if self.active_view.is_none()
            && let Some(grass) = self.grass.as_mut()
        {
            grass.prepare(&self.gpu, &self.map, info, upload, self.grass_generation, portals);
            let (drawn, culled) = grass.portal_counts();
            self.outdoor_counts.grass_patches_drawn = drawn;
            self.outdoor_counts.grass_patches_culled = culled;
        }
        let lists: [&[u32]; 10] = std::array::from_fn(|i| self.lists[i].as_slice());
        let eyes: [Option<[f32; 2]>; 4] = std::array::from_fn(|i| self.shadows[i].eye());
        self.gpu.set_detail_eye(Some([info.eye[0], info.eye[2]]));
        {
            let _upload = tracing::info_span!("terrain_upload").entered();
            self.gpu
                .upload_shared(&frame, info, &self.draw_list, &lists, &eyes, upload);
        }
        // A texture view samples the pages baked for the main camera and requests none. Its
        // colour pass still writes the feedback buffer, in its own submission: before the main
        // frame's (then the main frame's `encode_before_passes` clears it before its own colour
        // pass) or after it (then the main frame already reduced in the same encoder, and the
        // next one clears it first). Queue order keeps a view's writes out of every reduction.
        if self.active_view.is_some() {
            return;
        }
        if self.gpu.vt_active() {
            let base = self.bases[4];
            let mut nodes = self
                .selection
                .nodes
                .iter()
                .enumerate()
                .map(|(i, d)| (d.node, base + i as u32));
            self.gpu
                .prepare_vt(info.eye, self.selection_key, base, Some(&mut nodes), upload);
        }
        *self
            .stats
            .vt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.gpu.vt_stats();
        *self
            .stats
            .vt_edits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.gpu.vt_edit_counters();
        let (writes, bytes) = self.gpu.last_node_upload();
        self.stats.node_writes.store(writes, Ordering::Relaxed);
        self.stats.node_bytes.store(bytes, Ordering::Relaxed);
        self.stats
            .selected
            .store(self.selection.nodes.len() as u32, Ordering::Relaxed);
        self.stats
            .dense_nodes
            .store(self.gpu.dense_node_count() as u32, Ordering::Relaxed);
        self.stats
            .hole_tests
            .store(self.gpu.last_hole_tests(), Ordering::Relaxed);
        self.stats
            .camera_nodes
            .store(self.camera_list.len() as u32, Ordering::Relaxed);
        self.stats
            .portal_culled
            .store(self.outdoor_counts.nodes_culled, Ordering::Relaxed);
        if self.stats.record_camera_nodes.load(Ordering::Relaxed) {
            let mut kept = self
                .stats
                .camera_node_bounds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            kept.clear();
            kept.extend(self.camera_list.iter().map(|&i| self.node_bounds[i as usize]));
        }
        for (count, shadow) in self.stats.cascade_nodes.iter().zip(&self.shadows) {
            count.store(shadow.list.len() as u32, Ordering::Relaxed);
        }
        for (hits, shadow) in self.stats.cascade_edit_hits.iter().zip(&self.shadows) {
            hits.store(shadow.edit_hits, Ordering::Relaxed);
        }
        self.stats
            .prepare_us
            .store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, kind: SharedPass) {
        let _span = tracing::trace_span!("terrain_draw", ?kind).entered();
        let list = match kind {
            SharedPass::Shadow { index } => (index as usize).min(3),
            SharedPass::Depth | SharedPass::Color | SharedPass::Velocity | SharedPass::Receiver => 4,
        };
        self.gpu.draw_shared(pass, kind, &self.draw_list, list);
        // The mouth bands, placed on this pass's selection by this camera (ADR-8569, ADR-8824).
        if self.gpu.draw_mouth_bands(pass, kind) {
            self.stats.count_band(self.gpu.band_camera(), band_slot(kind));
        }
        if self.active_view.is_none()
            && let Some(impostors) = self.impostors.as_ref()
        {
            impostors.draw(&self.gpu, pass, kind);
        }
        if self.active_view.is_none()
            && let Some(grass) = self.grass.as_ref()
        {
            grass.draw(&self.gpu, pass, kind);
        }
    }

    fn draw_shadow_region<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, index: u32) {
        let _span = tracing::trace_span!("terrain_draw_region", index).entered();
        self.gpu.draw_shared(
            pass,
            SharedPass::Shadow { index },
            &self.draw_list,
            5 + (index as usize).min(3),
        );
        if self.gpu.draw_mouth_bands(pass, SharedPass::Shadow { index }) {
            self.stats
                .count_band(self.gpu.band_camera(), BAND_SLOT_REGION + (index as usize).min(3));
        }
        if self.active_view.is_none()
            && let Some(grass) = self.grass.as_ref()
        {
            grass.draw(&self.gpu, pass, SharedPass::Shadow { index });
        }
    }

    fn cascade_shadow_key(&self, index: u32) -> Option<u64> {
        let grass = self
            .grass
            .as_ref()
            .filter(|_| self.active_view.is_none())
            .map_or(0, |grass| grass.cascade_shadow_key(index));
        // A mouth band opening, closing or re-placing redraws the cascade (ADR-8569), a texture
        // view's cascade too (ADR-8824).
        let keys = if self.active_view.is_none() {
            &self.band_keys
        } else {
            &self.view_band_keys
        };
        let bands = keys
            .get(1 + index as usize)
            .copied()
            .flatten()
            .map_or(0, |k| band_key_hash(&k));
        self.shadows
            .get(index as usize)
            .map(|shadow| shadow.key ^ grass ^ bands)
    }

    fn set_engine_csm(&mut self, views: &[wgpu::TextureView]) {
        self.gpu.set_engine_csm(views);
    }

    fn set_engine_sky_irradiance(&mut self, sky: Option<(&wgpu::TextureView, &wgpu::TextureView)>) {
        // A host override (`set_sky_ambient`) keeps the hemisphere model it asked for.
        let sky = if self.sky_ambient.is_some() { None } else { sky };
        self.gpu.set_sky_irradiance(sky);
    }

    fn set_engine_ao(&mut self, ao: Option<&wgpu::TextureView>) {
        self.gpu.set_engine_ao(ao);
    }

    fn set_engine_contact_shadows(&mut self, shadows: Option<&wgpu::TextureView>) {
        self.gpu.set_engine_contact_shadows(shadows);
    }

    fn set_engine_cloud_shadow(&mut self, map: Option<(&wgpu::TextureView, [f32; 4])>) {
        self.gpu.set_engine_cloud_shadow(map);
    }

    fn set_engine_precip_occlusion(&mut self, atlas: Option<&wgpu::TextureView>) {
        self.gpu.set_engine_precip_occlusion(atlas);
    }

    fn draw_precip_occlusion<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        self.gpu.draw_shared(
            pass,
            SharedPass::Shadow {
                index: super::PRECIP_SHADOW_SLOT,
            },
            &self.draw_list,
            PRECIP_TILE_LIST,
        );
        // The open mouths' bands cover the ground the cut removed (ADR-8824): the tile's nodes are
        // the camera's, so it draws the camera's band set.
        let kind = SharedPass::Shadow {
            index: super::PRECIP_SHADOW_SLOT,
        };
        if self.gpu.draw_mouth_bands(pass, kind) {
            self.stats.count_band(self.gpu.band_camera(), BAND_SLOT_PRECIP);
        }
    }

    fn surface_weather_variant_on(&self) -> bool {
        self.gpu.surface_weather_report().variant_on
    }

    fn set_warm_surface_weather(&mut self, on: bool) {
        self.gpu.set_warm_surface_weather(on);
    }

    fn finish_prepare(&mut self, info: &SharedFrameInfo, upload: &mut SharedUpload<'_>) {
        let world_space;
        let info = if info.origin_offset == [0.0; 3] {
            info
        } else {
            world_space = info.in_world_space();
            &world_space
        };
        // A frame that skips the sun cascades: no cascade is read, the trace is the sun's shadow.
        let untraced;
        let info = if self.engine_sun_traced {
            untraced = SharedFrameInfo {
                cascade_count: 0,
                ..*info
            };
            &untraced
        } else {
            info
        };
        // The engine-CSM uniform carries the "SSAO bound" and "contact shadows bound" flags
        // `set_engine_ao` / `set_engine_contact_shadows` just set.
        self.gpu.upload_engine_csm_shared(info, upload);
    }

    fn shadow_key(&self) -> Option<u64> {
        Some(self.selection_key ^ self.heights_generation.rotate_left(32))
    }

    fn ground_bounce(&self) -> Option<&GroundBounce> {
        self.ground_bounce.as_ref()
    }

    fn encode_before_passes(&self, encoder: &mut wgpu::CommandEncoder) {
        if self.active_view.is_some() {
            return;
        }
        if let Some(impostors) = self.impostors.as_ref() {
            impostors.encode(encoder);
        }
        if let Some(grass) = self.grass.as_ref() {
            grass.encode(encoder);
        }
        self.gpu.encode_vt_before(encoder);
    }

    fn encode_after_color(&self, encoder: &mut wgpu::CommandEncoder) {
        if self.active_view.is_none() {
            self.gpu.encode_vt_after(encoder);
        }
    }

    fn set_view(&mut self, view: Option<u64>) {
        if view == self.active_view {
            return;
        }
        if let Some(id) = self.active_view.take()
            && let Some(mut state) = self.views.remove(&id)
        {
            self.swap_view_state(&mut state);
            self.views.insert(id, state);
        }
        if let Some(id) = view {
            let mut state = match self.views.remove(&id) {
                Some(state) => state,
                None => Box::new(TerrainView::new(&self.gpu)),
            };
            self.swap_view_state(&mut state);
            self.views.insert(id, state);
            self.active_view = Some(id);
        }
    }

    fn set_outdoor_portals(&mut self, portals: Option<&regia_render::viscells::OutdoorPortals>) {
        match portals {
            Some(portals) => self.outdoor_portals.clone_from(portals),
            None => self.outdoor_portals.clear(),
        }
    }

    fn outdoor_cull_counts(&self) -> Option<regia_render::viscells::OutdoorCullCounts> {
        Some(self.outdoor_counts)
    }

    fn drop_view(&mut self, view: u64) {
        if self.active_view == Some(view) {
            self.set_view(None);
        }
        self.views.remove(&view);
    }

    fn dynamic_shadow_boxes(&self) -> &[[[f32; 3]; 2]] {
        // The swaying grass (spec 88 § 2.4): main camera only, like its shadow draw.
        match self.grass.as_ref().filter(|_| self.active_view.is_none()) {
            Some(grass) => grass.dynamic_shadow_boxes(),
            None => &[],
        }
    }

    fn dynamic_shadow_box_cascades(&self) -> u8 {
        self.grass
            .as_ref()
            .map_or(0, crate::GrassLayer::dynamic_shadow_cascades)
    }

    fn set_trace_casters(&mut self, on: bool) {
        if !on {
            self.trace_proxy = None;
            self.trace_proxy_handle.set_stats(None);
            return;
        }
        let settings = self.trace_proxy_handle.settings();
        // Settings an agent or host set that the build would refuse keep the last good proxy.
        let settings = if settings.invalid().is_some() {
            crate::TraceProxySettings::default()
        } else {
            settings
        };
        match self
            .trace_proxy
            .as_mut()
            .filter(|proxy| proxy.settings() == settings)
        {
            Some(proxy) => {
                if proxy.refresh(&self.map) > 0 {
                    self.trace_proxy_handle.set_stats(Some(proxy.stats()));
                }
            }
            None => {
                let proxy = crate::TraceProxy::build(&self.map, settings);
                self.trace_proxy_handle.set_stats(Some(proxy.stats()));
                self.trace_proxy = Some(proxy);
            }
        }
    }

    fn trace_casters(&self) -> Option<regia_render::hwrt::SharedTraceCasters<'_>> {
        self.trace_proxy.as_ref().map(crate::TraceProxy::casters)
    }

    /// The ground's own colour pass multiplies the trace into its sun term, but grass and far
    /// impostors read only the cascades, so a draw with either keeps them.
    fn takes_traced_sun(&self) -> bool {
        self.grass.is_none() && self.impostors.is_none()
    }

    fn set_engine_sun_traced(&mut self, traced: bool) {
        self.engine_sun_traced = traced;
    }

    fn content_key(&self) -> u64 {
        // A hole or mouth band change moves the ground the precipitation tile stores (ADR-8824);
        // 0 until the first one, so a draw without holes keeps its key.
        self.heights_generation.rotate_left(32)
            ^ self.gpu.holes_generation().rotate_left(48)
            ^ self.gpu.contact_key()
            ^ self
                .impostors
                .as_ref()
                .map_or(0, super::impostor::ImpostorDraw::content_key)
            ^ self
                .grass
                .as_ref()
                .map_or(0, super::grass::GrassLayer::content_key)
    }

    fn gi_heightfield(&self) -> Option<(&wgpu::TextureView, regia_render::HeightfieldInfo)> {
        let map = &self.map;
        if map.width() != map.height() || map.width() < 2 {
            return None;
        }
        Some((
            self.gpu.height_view(),
            regia_render::HeightfieldInfo {
                samples: map.width(),
                // The grid origin folds into the offset lane (ADR-8179).
                half_width: map.gpu_half_width(),
                world_width: map.world_width(),
                height_scale: map.height_scale(),
                height_offset: map.height_offset(),
                albedo: self.gi_albedo,
                albedo_generation: self.gi_albedo_map.as_ref().map_or(0, |(_, g)| *g),
                generation: self.heights_generation,
                origin_offset: self.origin_offset,
                edits: self.gi_edits,
            },
        ))
    }

    fn set_engine_gi(&mut self, gi: Option<(&wgpu::Buffer, &wgpu::Buffer)>) {
        self.gpu.set_engine_gi(gi);
    }

    fn set_engine_lights(&mut self, lights: Option<&regia_render::EngineLights>) {
        self.gpu.set_engine_lights(lights);
    }

    fn set_engine_light_profiles(&mut self, atlas: Option<&wgpu::TextureView>) {
        self.gpu.set_engine_light_profiles(atlas);
    }

    fn gi_albedo_map(&self) -> Option<&wgpu::TextureView> {
        self.gi_albedo_map.as_ref().map(|(view, _)| view)
    }

    fn gi_holes(&self) -> Option<(&wgpu::Buffer, u64)> {
        self.gpu
            .holes()
            .map(|_| (self.gpu.gi_holes_buffer(), self.gpu.holes_generation()))
    }

    fn reference_heightfield(&self) -> Option<regia_render::ReferenceHeightfield> {
        let mut hf = reference::heightfield(
            &self.map,
            &self.reference_layers,
            &self.reference_layer_means,
            self.reference_albedo
                .as_ref()
                .is_none_or(|(generation, _)| *generation != self.heights_generation),
        );
        if let Some((generation, albedo)) = &self.reference_albedo
            && *generation == self.heights_generation
        {
            hf.albedo = Some(albedo.clone());
        }
        // Samples in a hole are NaN: the path tracer drops every cell touching one.
        if let Some(mask) = self.gpu.holes() {
            let (w, origin, spacing) = (hf.width.max(1) as usize, hf.origin, hf.spacing);
            mask.cut_heights(&mut hf.heights, 0.0, |i| {
                [
                    origin[0] + (i % w) as f32 * spacing[0],
                    origin[1] + (i / w) as f32 * spacing[1],
                ]
            });
        }
        // The export is in the frame's local space, like the meshes beside it.
        hf.origin[0] -= self.origin_offset[0];
        hf.origin[1] -= self.origin_offset[2];
        if self.origin_offset[1] != 0.0 {
            for height in &mut hf.heights {
                *height -= self.origin_offset[1];
            }
        }
        Some(hf)
    }
}

/// The ground albedo the frame's light implies: `ground = ρ · (E_sun · sin(el) / π + sky)`
/// (`SkyAmbient::from_atmosphere`), so meshes, probes and the ambient bake see one albedo.
fn implied_albedo(info: &SharedFrameInfo, ambient: &SkyAmbient) -> [f32; 3] {
    let travel = info.sun_dir;
    let len = (travel[0] * travel[0] + travel[1] * travel[1] + travel[2] * travel[2])
        .sqrt()
        .max(1e-6);
    let sin_el = (-travel[1] / len).max(0.0);
    std::array::from_fn(|c| {
        let lit = info.sun_illuminance * info.sun_color[c] * sin_el / std::f32::consts::PI + ambient.sky[c];
        if lit > 1e-3 {
            (ambient.ground[c] / lit).clamp(0.0, 0.95)
        } else {
            0.5
        }
    })
}

/// The CPU description of the drawn terrain for the reference path tracer.
mod reference {
    use regia_render::{LabTexture, ReferenceHeightfield};

    use crate::layers::TerrainLayers;
    use crate::sampler::TerrainMap;

    /// Albedo bake resolution per side.
    const ALBEDO_RES: u32 = 512;

    fn srgb_to_linear(c: f32) -> f32 {
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn linear_to_srgb(c: f32) -> f32 {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    }

    /// Mean linear RGB of sRGB-encoded RGBA8 bytes.
    pub(super) fn mean_linear(rgba: &[u8]) -> [f32; 3] {
        let lut: Vec<f32> = (0..256).map(|v| srgb_to_linear(v as f32 / 255.0)).collect();
        let mut sum = [0.0f64; 3];
        let mut n = 0u64;
        for px in rgba.as_chunks::<4>().0 {
            for k in 0..3 {
                sum[k] += f64::from(lut[px[k] as usize]);
            }
            n += 1;
        }
        let n = n.max(1) as f64;
        [(sum[0] / n) as f32, (sum[1] / n) as f32, (sum[2] / n) as f32]
    }

    /// Heights at full resolution and, with `bake_albedo`, an albedo estimated from the layer
    /// stack: slope/altitude weights × each layer's tint × the mean of its albedo texture (the
    /// first-order colour of the splat; detail textures, strata and the macro tint are not
    /// baked — the shader-exact bake is `regia_pt::terrain_bake`, installed with
    /// [`super::TerrainSharedDraw::set_reference_albedo`]).
    pub(super) fn heightfield(
        map: &TerrainMap,
        layers: &TerrainLayers,
        means: &[Option<[f32; 3]>],
        bake_albedo: bool,
    ) -> ReferenceHeightfield {
        let _span = tracing::info_span!("terrain_reference_heightfield").entered();
        let (w, h) = (map.width(), map.height());
        let o = map.sample_to_world(0, 0);
        let far = map.sample_to_world(w.saturating_sub(1), h.saturating_sub(1));
        let spacing = [
            (far[0] - o[0]) / w.saturating_sub(1).max(1) as f32,
            (far[1] - o[1]) / h.saturating_sub(1).max(1) as f32,
        ];
        let range = {
            let (lo, hi) = map.min_max_height();
            [lo, hi]
        };
        let layer_albedo: Vec<[f32; 3]> = (0..layers.count())
            .map(|i| {
                let tint = layers.layer(i).map_or([0.5; 3], |l| l.albedo);
                match means.get(i).copied().flatten() {
                    Some(m) => [tint[0] * m[0], tint[1] * m[1], tint[2] * m[2]],
                    None => tint,
                }
            })
            .collect();
        let roughness = if layers.count() == 0 {
            0.9
        } else {
            (0..layers.count())
                .filter_map(|i| layers.layer(i).map(|l| l.roughness))
                .sum::<f32>()
                / layers.count() as f32
        };
        let res = if bake_albedo { ALBEDO_RES } else { 0 };
        let mut rgba = Vec::with_capacity((res * res * 4) as usize);
        for j in 0..res {
            for i in 0..res {
                let x = o[0] + (far[0] - o[0]) * (i as f32 + 0.5) / res as f32;
                let z = o[1] + (far[1] - o[1]) * (j as f32 + 0.5) / res as f32;
                let n = map.normal_at(x, z);
                let slope = n[1].clamp(-1.0, 1.0).acos();
                let weights = layers.weights_in(slope, map.sample_bilinear(x, z), Some(range));
                let total: f32 = weights.iter().sum();
                let a = if total > 1e-5 {
                    let mut a = [0.0f32; 3];
                    for (wi, la) in weights.iter().zip(&layer_albedo) {
                        for k in 0..3 {
                            a[k] += wi * la[k] / total;
                        }
                    }
                    a
                } else {
                    layer_albedo.first().copied().unwrap_or([0.3; 3])
                };
                for v in a {
                    rgba.push((linear_to_srgb(v) * 255.0 + 0.5) as u8);
                }
                rgba.push(255);
            }
        }
        ReferenceHeightfield {
            width: w,
            depth: h,
            origin: o,
            spacing,
            heights: map.heights().to_vec(),
            albedo: bake_albedo.then_some(LabTexture {
                width: res,
                height: res,
                rgba,
            }),
            roughness: roughness.clamp(0.0, 1.0),
        }
    }
}

/// Fills `out` with the indices of `bounds` that [`bounds_visible`] keeps.
fn cull_into(
    out: &mut Vec<u32>,
    bounds: &[([f32; 3], [f32; 3])],
    view_proj: &[[f32; 4]; 4],
    slack: [f32; 2],
) {
    out.clear();
    for (index, (lo, hi)) in bounds.iter().enumerate() {
        if bounds_visible(*lo, *hi, view_proj, slack) {
            out.push(index as u32);
        }
    }
}

/// `false` when a column-major perspective `view_proj` places every corner of the
/// box outside one side plane (widened by `slack` in NDC) or behind the eye. Depth
/// range is not tested.
fn bounds_visible(lo: [f32; 3], hi: [f32; 3], view_proj: &[[f32; 4]; 4], slack: [f32; 2]) -> bool {
    let m = view_proj;
    // Bit set while every corner so far is outside that plane.
    let mut outside_all = 0b11111u8;
    for corner in 0..8u8 {
        let p = [
            if corner & 1 == 0 { lo[0] } else { hi[0] },
            if corner & 2 == 0 { lo[1] } else { hi[1] },
            if corner & 4 == 0 { lo[2] } else { hi[2] },
        ];
        let clip = |row: usize| m[0][row] * p[0] + m[1][row] * p[1] + m[2][row] * p[2] + m[3][row];
        let (x, y, w) = (clip(0), clip(1), clip(3));
        let (wx, wy) = (w * (1.0 + slack[0]), w * (1.0 + slack[1]));
        let mut outside = 0u8;
        if x < -wx {
            outside |= 1;
        }
        if x > wx {
            outside |= 2;
        }
        if y < -wy {
            outside |= 4;
        }
        if y > wy {
            outside |= 8;
        }
        if w <= 0.0 {
            outside |= 16;
        }
        outside_all &= outside;
        if outside_all == 0 {
            return true;
        }
    }
    false
}

/// Hash of the selected nodes and their morph ranges.
fn selection_key(selection: &LodSelection) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    selection.nodes.len().hash(&mut hasher);
    for draw in &selection.nodes {
        draw.node.hash(&mut hasher);
        draw.morph_start.to_bits().hash(&mut hasher);
        draw.morph_end.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

/// Hasher for quadtree nodes: three small integers, mixed with a multiply-rotate (the
/// default SipHash dominated the per-rock seating cost).
#[derive(Default, Clone, Copy)]
pub(crate) struct NodeHasher(u64);

impl std::hash::Hasher for NodeHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u32(&mut self, v: u32) {
        self.write_u64(u64::from(v));
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

type NodeMap = std::collections::HashMap<
    crate::cdlod::QuadNode,
    crate::cdlod::NodeDraw,
    std::hash::BuildHasherDefault<NodeHasher>,
>;

/// One frame's drawn surface: the selected nodes, the eye they morph toward and the shared
/// height pyramid.
struct SurfaceSnapshot {
    nodes: NodeMap,
    /// Identity of the selection (nodes and morph bands).
    selection_key: u64,
    eye: [f32; 2],
    config: CdlodConfig,
    root_level: u32,
    mips: crate::height_mips::SharedHeightMips,
}

/// A cloneable, thread-safe view of the terrain surface a [`TerrainSharedDraw`] drew last
/// frame (ADR-048's rendered-surface query, kept reachable after the draw is boxed).
#[derive(Clone, Default)]
pub struct RenderedSurface {
    /// Replaced whole by each publish; queries clone the [`Arc`] and drop the lock (ADR-303).
    inner: Arc<std::sync::RwLock<Option<Arc<SurfaceSnapshot>>>>,
    generation: Arc<AtomicU64>,
    /// Bumped when the drawn heights change under the same selection (deformation).
    heights: Arc<AtomicU64>,
    /// The holes cut through the drawn surface ([`TerrainSharedDraw::set_holes`]).
    holes: Arc<std::sync::RwLock<Option<Arc<crate::HolesMask>>>>,
}

/// One locked frame of a [`RenderedSurface`] for many queries ([`RenderedSurface::query`]).
pub(crate) struct SurfaceQuery<'a> {
    snap: &'a SurfaceSnapshot,
    mips: &'a crate::height_mips::HeightMips,
    map: &'a TerrainMap,
    /// Heights generation: cached results keyed on an older value are stale.
    pub(crate) heights_generation: u64,
    holes: Option<Arc<crate::HolesMask>>,
}

impl SurfaceQuery<'_> {
    /// The holes cut through the surface, when any.
    pub(crate) fn holes(&self) -> Option<&crate::HolesMask> {
        self.holes.as_deref()
    }

    /// Identity of this frame's selected nodes and their morph bands.
    pub(crate) fn selection_key(&self) -> u64 {
        self.snap.selection_key
    }

    /// The draw of `node` when it is selected this frame.
    pub(crate) fn node_draw(&self, node: crate::cdlod::QuadNode) -> Option<crate::cdlod::NodeDraw> {
        self.snap.nodes.get(&node).copied()
    }

    /// The eye (XZ) the selection morphs toward.
    pub(crate) fn eye(&self) -> [f32; 2] {
        self.snap.eye
    }

    /// [`Self::triangle`] plus the morph slack ([`crate::lod_error::RenderedTriangle::morph_ts_slack`]).
    pub(crate) fn triangle_slack(
        &self,
        draw: &crate::cdlod::NodeDraw,
        x: f32,
        z: f32,
    ) -> (crate::lod_error::RenderedTriangle, [f32; 3], f32) {
        let tri = crate::lod_error::rendered_triangle(
            self.map,
            &self.snap.config,
            self.snap.root_level,
            draw.node,
            x,
            z,
        );
        let (ts, slack) = tri.morph_ts_slack(
            self.map,
            &self.snap.config,
            self.snap.root_level,
            draw,
            self.snap.eye,
        );
        (tri, ts, slack)
    }
}

impl SurfaceQuery<'_> {
    /// The selected node drawing `(x, z)`: `hint` when it is still selected (selected nodes
    /// tile the map, so a selected node that contains the point is the one), else the
    /// quadtree descent from the root.
    pub(crate) fn node_at(
        &self,
        x: f32,
        z: f32,
        hint: Option<crate::cdlod::QuadNode>,
    ) -> Option<crate::cdlod::NodeDraw> {
        if let Some(node) = hint
            && let Some(draw) = self.snap.nodes.get(&node)
        {
            return Some(*draw);
        }
        let world = self.map.quadtree_width();
        // The quadtree square's −X/−Z corner, moved by the map's grid origin (ADR-8179); the
        // map's own corner on a square map (ADR-8369).
        let half = world * 0.5;
        let [ox, oz] = self.map.grid_origin();
        let (min_x, min_z) = (-half + ox, -half + oz);
        let root = self.snap.root_level;
        for level in (0..=root).rev() {
            let size = crate::cdlod::node_world_size(world, root, level);
            let count = 1u32 << (root - level);
            let cell = |v: f32, min: f32| (((v - min) / size).floor().max(0.0) as u32).min(count - 1);
            let node = crate::cdlod::QuadNode {
                level,
                x: cell(x, min_x),
                z: cell(z, min_z),
            };
            if let Some(draw) = self.snap.nodes.get(&node) {
                return Some(*draw);
            }
        }
        None
    }

    /// The covering triangle and its vertex morph factors for this frame's eye.
    pub(crate) fn triangle(
        &self,
        draw: &crate::cdlod::NodeDraw,
        x: f32,
        z: f32,
    ) -> (crate::lod_error::RenderedTriangle, [f32; 3]) {
        let tri = crate::lod_error::rendered_triangle(
            self.map,
            &self.snap.config,
            self.snap.root_level,
            draw.node,
            x,
            z,
        );
        let ts = tri.morph_ts(
            self.map,
            &self.snap.config,
            self.snap.root_level,
            draw,
            self.snap.eye,
        );
        (tri, ts)
    }

    /// The drawn height for a triangle and morph factors from [`Self::triangle`].
    pub(crate) fn height(
        &self,
        draw: &crate::cdlod::NodeDraw,
        tri: &crate::lod_error::RenderedTriangle,
        ts: [f32; 3],
        x: f32,
        z: f32,
    ) -> f32 {
        tri.height(
            self.map,
            self.mips,
            &self.snap.config,
            self.snap.root_level,
            draw.node,
            ts,
            x,
            z,
        )
    }
}

impl RenderedSurface {
    fn publish(&self, snapshot: SurfaceSnapshot) {
        let mut slot = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(Arc::new(snapshot));
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn heights_changed(&self) {
        // Release: a query that sees the new count also sees the edited pyramid (`query`).
        self.heights.fetch_add(1, Ordering::Release);
    }

    /// Bumped whenever the selection changes (a new eye).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Runs `f` on this frame's surface for queries on `map` (the map the draw was built
    /// from); `None` before the draw has selected anything.
    ///
    /// `f` reads snapshots and runs with no lock held, so it may fan out on the compute pool
    /// (ADR-303): a thread waiting in a scope runs other pool tasks, and one of them may query,
    /// publish or edit this surface.
    pub(crate) fn query<R>(&self, map: &TerrainMap, f: impl FnOnce(Option<&SurfaceQuery<'_>>) -> R) -> R {
        let snap = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(snap) = snap else {
            return f(None);
        };
        // Read before the pyramid: an edit lands in the pyramid before its count is bumped, so
        // the count never runs ahead of the levels read (a result cached under it is never
        // stale; at worst a fresh pyramid is keyed under the old count and recomputed once).
        let heights_generation = self.heights.load(Ordering::Acquire);
        let mips = snap.mips.snapshot();
        let query = SurfaceQuery {
            snap: &snap,
            mips: &mips,
            map,
            heights_generation,
            holes: self.holes_mask(),
        };
        f(Some(&query))
    }

    fn holes_mask(&self) -> Option<Arc<crate::HolesMask>> {
        self.holes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_holes(&self, mask: Option<crate::HolesMask>) {
        *self
            .holes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mask.map(Arc::new);
        self.heights_changed();
    }

    /// `true` when world `(x, z)` lies in a hole cut through the surface
    /// ([`TerrainSharedDraw::set_holes`]): there is no ground there.
    #[must_use]
    pub fn in_hole(&self, x: f32, z: f32) -> bool {
        self.holes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|m| m.contains(x, z))
    }

    /// Heights of the drawn surface at world `(x, z)` points of `map` (the map the draw was
    /// built from), or `None` before the draw has selected anything. Each point finds its
    /// selected node by descending the quadtree from the root. A point inside a hole
    /// ([`Self::in_hole`]) has no ground: its height is NaN.
    #[must_use]
    pub fn heights(&self, map: &TerrainMap, points: &[[f32; 2]]) -> Option<Vec<f32>> {
        self.query(map, |q| {
            let q = q?;
            Some(
                points
                    .iter()
                    .map(|p| match q.node_at(p[0], p[1], None) {
                        _ if q.holes().is_some_and(|m| m.contains(p[0], p[1])) => f32::NAN,
                        Some(draw) => {
                            let (tri, ts) = q.triangle(&draw, p[0], p[1]);
                            q.height(&draw, &tri, ts, p[0], p[1])
                        }
                        None => map.sample_bilinear(p[0], p[1]),
                    })
                    .collect(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::height_mips::{HeightMips, SharedHeightMips};

    fn hills() -> TerrainMap {
        let n = 129u32;
        let heights = (0..n * n)
            .map(|i| {
                let (col, row) = ((i % n) as f32, (i / n) as f32);
                3.0 * (col * 0.11).sin() * (row * 0.07).cos()
            })
            .collect();
        TerrainMap::from_heights(n, n, heights, 64.0, 1.0, 0.0).expect("map")
    }

    const EYE: [f32; 2] = [3.0, -5.0];

    /// What `TerrainSharedDraw::prepare` publishes, without a GPU: 16 m leaves, 0.5 m cells.
    fn snapshot_of(map: &TerrainMap, mips: &SharedHeightMips, eye: [f32; 2]) -> SurfaceSnapshot {
        let config = CdlodConfig::with_grid_size(32);
        let root_level = 2;
        let pyramid = MinMaxPyramid::build(map, root_level).expect("pyramid");
        let selection = select_nodes(map, &pyramid, &config, root_level, eye, None);
        SurfaceSnapshot {
            nodes: selection.nodes.iter().map(|d| (d.node, *d)).collect::<NodeMap>(),
            selection_key: selection_key(&selection),
            eye,
            config,
            root_level,
            mips: mips.clone(),
        }
    }

    /// ADR-303: the scatter collect fans out on the compute pool inside a surface query, and a
    /// thread waiting in that scope runs other pool tasks. Here the scope's own tasks query,
    /// republish and cut the same surface. Under the old query (the snapshot's read lock and the
    /// pyramid mutex held while the closure ran) they wait on the querying thread, which waits
    /// on them: a hang.
    #[test]
    fn a_fan_out_inside_a_surface_query_may_query_publish_and_cut_the_surface() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let map = hills();
            let mips = SharedHeightMips::new(HeightMips::build(&map));
            let surface = RenderedSurface::default();
            surface.publish(snapshot_of(&map, &mips, EYE));
            let points: Vec<[f32; 2]> = (0..16)
                .map(|i| [i as f32 * 3.7 - 28.0, 11.0 - i as f32 * 2.3])
                .collect();
            let alone = surface.heights(&map, &points);
            let inside = surface.query(&map, |q| {
                assert!(q.is_some(), "the surface is published");
                regia_core::tasks::scope(|scope| {
                    for task in 0..8u32 {
                        let (surface, map, mips, points) = (&surface, &map, &mips, &points);
                        scope.spawn(async move {
                            match task {
                                0 => surface.publish(snapshot_of(map, mips, EYE)),
                                1 => surface.set_holes(None),
                                _ => {}
                            }
                            surface.heights(map, points)
                        });
                    }
                })
            });
            let _ = tx.send((alone, inside));
        });
        // A failure guard only: the queries finish in milliseconds.
        let (alone, inside) = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("a fan-out inside a surface query deadlocked");
        let alone = alone.expect("the surface is published");
        assert!(alone.iter().all(|h| h.is_finite()));
        assert_eq!(inside.len(), 8);
        for heights in inside {
            assert_eq!(
                heights.as_deref(),
                Some(alone.as_slice()),
                "the same selection gives the same heights"
            );
        }
    }

    /// A query reads the snapshot it started with: a publish or an edit during it does not
    /// change what it sees, and a query after the edit sees the edit. The publish runs on the
    /// querying thread, which the old read lock (held while the closure ran) hung on.
    #[test]
    fn a_query_keeps_its_snapshot() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let map = hills();
            let mips = SharedHeightMips::new(HeightMips::build(&map));
            let surface = RenderedSurface::default();
            assert!(surface.query(&map, |q| q.is_none()), "nothing published yet");
            surface.publish(snapshot_of(&map, &mips, EYE));
            let moved = [20.0, 20.0];
            let (eye, generation) = surface.query(&map, |q| {
                let q = q.expect("published");
                surface.heights_changed();
                surface.publish(snapshot_of(&map, &mips, moved));
                (q.eye(), q.heights_generation)
            });
            let after = surface.query(&map, |q| q.map(|q| (q.eye(), q.heights_generation)));
            let _ = tx.send((eye, generation, after, moved));
        });
        // A failure guard only.
        let (eye, generation, after, moved) = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("a publish inside a surface query deadlocked");
        assert_eq!(eye, EYE, "the query keeps the snapshot it started with");
        assert_eq!(after, Some((moved, generation + 1)));
    }
}
