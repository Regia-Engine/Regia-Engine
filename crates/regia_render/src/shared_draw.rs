// SPDX-License-Identifier: MPL-2.0
//! A draw that records into [`crate::FrameRenderer`]'s depth, shadow, colour,
//! velocity and decal-receiver passes.
//!
//! The renderer does not know about terrain. A host installs one
//! [`SharedPassDraw`] with [`crate::FrameRenderer::set_shared_draw`]. Each frame
//! the renderer calls [`SharedPassDraw::prepare`] before it opens a command
//! encoder, then [`SharedPassDraw::draw`] inside the passes that already target
//! the shared depth and HDR textures. There is no second depth buffer and no
//! composite.

use crate::FogSettings;

/// Which shared pass is being recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedPass {
    /// Depth prepass (no colour attachment, `Depth32Float`).
    Depth,
    /// One directional cascade. `index` is `0..cascade_count`. The local-light
    /// atlas is not a cascade and is never requested.
    Shadow {
        /// Cascade slot.
        index: u32,
    },
    /// Forward colour, after opaque meshes and before the sky.
    Color,
    /// Motion vectors (`Rgba16Float`). The colour attachment loads. The shared
    /// depth is tested and not written, so a closer mesh keeps its velocity.
    Velocity,
    /// Decal receiver (`Rgba8Unorm`). The colour attachment already holds the
    /// mesh stamp (or the terrain clear). The shared depth is tested and not
    /// written, so a closer mesh keeps the layer it stamped. The draw writes
    /// layer 0 and the heightfield normal.
    Receiver,
}

/// Camera, sun and target description for one frame.
///
/// `sun_dir` is the direction of light travel (from the sun toward the scene),
/// the same vector [`crate::DirectionalLightUniform::direction`] stores.
#[derive(Clone, Copy, Debug)]
pub struct SharedFrameInfo {
    /// Jittered view-projection (column-major). Depth and colour must use this.
    pub view_proj: [[f32; 4]; 4],
    /// View-projection without the temporal jitter.
    pub unjittered_view_proj: [[f32; 4]; 4],
    /// Previous frame's un-jittered view-projection. Equal to `unjittered_view_proj` on the first frame.
    pub prev_view_proj: [[f32; 4]; 4],
    /// Camera eye, world metres.
    pub eye: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Near plane, metres.
    pub near: f32,
    /// Far plane, metres.
    pub far: f32,
    /// Internal colour width (after render scale).
    pub width: u32,
    /// Internal colour height (after render scale).
    pub height: u32,
    /// Colour attachment format of the forward pass.
    pub color_format: wgpu::TextureFormat,
    /// The forward target is scene-referred HDR. Post applies exposure.
    pub linear_hdr: bool,
    /// EV100 exposure the post stack will apply.
    pub exposure_ev: f32,
    /// Pre-exposure of the linear HDR target (`pre_exposure_scale`): a colour pass writing
    /// scene radiance into it writes radiance × this scale. 1 when not `linear_hdr`.
    pub hdr_scale: f32,
    /// Light travel direction (from the sun toward the scene).
    pub sun_dir: [f32; 3],
    /// Linear sun colour.
    pub sun_color: [f32; 3],
    /// Sun illuminance at the surface, lux.
    pub sun_illuminance: f32,
    /// Flat ambient illuminance, lux.
    pub ambient_lux: f32,
    /// The hemisphere ambient meshes take from the planet's atmosphere this frame, as
    /// radiance (`E / π`, cd/m²): `[sky, ground]` — the cosine-weighted sky table and the
    /// Lambertian ground bounce of the IBL bake (ADR-040 § 3; an airless planet has a black
    /// sky and ground bounce only). `None` when meshes use the flat or procedural ambient.
    pub sky_ambient: Option<[[f32; 3]; 2]>,
    /// Planet-shine from the level's sky body (Jupiter-shine on Europa): `[toward the body,
    /// linear colour · lux]`, a shadowless directional fill meshes take as well. `None`
    /// without a sky body.
    pub fill_light: Option<[[f32; 3]; 2]>,
    /// The frame's extra directional lights besides the sun (ADR-7211, ADR-7254), packed as
    /// [`crate::LightsViewsUniform::extra_dir`] packs them: per slot the travel direction (`w` = 1
    /// when the slot is on, 0 when off), then linear colour × lux (`w` = 0). Unshadowed: a shared
    /// draw shades them as the forward does. All zero without extra lights.
    pub extra_directional: [[f32; 4]; 2 * crate::MAX_EXTRA_DIRECTIONAL],
    /// Large meshes' occlusion and bounce proxies (`FrameRenderer::set_mesh_occluders`),
    /// packed as the shaders read them (count 0: none). The shared colour pass lights the
    /// ground with them: each box replaces the sky it hides with its faces' radiance and
    /// shadows the fill.
    pub mesh_occluders: crate::MeshOccluderBlock,
    /// Height fog. Disabled fog must not change the shaded colour.
    pub fog: FogSettings,
    /// Cascade view-projections, slot 0 first. Unused slots are zero.
    pub cascades: [[[f32; 4]; 4]; 4],
    /// Active directional cascades. Zero means the colour pass is unshadowed.
    pub cascade_count: u32,
    /// Cascade split distances in view-space metres, slot 0 first.
    pub csm_splits: [f32; 4],
    /// Per-cascade receiver bias. `x` is the world size of one texel, `y` is
    /// `1 / depth extent`, `z` is the map edge in pixels. Unused slots are zero.
    pub csm_bias: [[f32; 4]; 4],
    /// PCSS filter tap count for the terrain colour sample. `0` means 16.
    pub pcss_filter_taps: u32,
    /// Per cascade, the shadow-map UV rectangle `[u0, v0, u1, v1]` (V down) the renderer
    /// will redraw for moved mesh casters this frame without clearing the rest: the union
    /// of the dirty pages. `[0, 0, 1, 1]` is a whole-cascade redraw, an empty rectangle
    /// (`u1 <= u0`) none. [`SharedPassDraw::draw_shadow_region`] draws into it.
    pub shadow_regions: [[f32; 4]; 4],
    /// The profile's opt-in terrain shading cuts (`FrameQuality::terrain_budget`).
    pub terrain_budget: crate::TerrainBudget,
    /// The surface weather the frame shades with (spec 99 § 6.4).
    pub surface_weather: crate::SurfaceWeatherFrame,
    /// The precipitation occlusion view, when the frame has one.
    pub precip_occlusion: Option<crate::PrecipOcclusionView>,
    /// The precipitation's constant extinction, 1/m (spec 99 § 3.2).
    pub precip_extinction_per_m: f32,
    /// Frame index (mod 16) the mesh path shifts its screen-space dither by under temporal AA
    /// (0 without): a shared draw that dithers against meshes (scatter impostors handing
    /// over from a LOD-dithered mesh) uses it to stay their exact complement.
    pub noise_index: f32,
    /// Texture mip bias for material samples (log2 units, ≤ 0): negative while TSR
    /// upscales, so textures pick the output resolution's mip; 0 otherwise.
    pub texture_lod_bias: f32,
    /// Texture mip bias for the terrain (log2 units, ≤ 0): [`Self::texture_lod_bias`] on TSR
    /// frames, and the same `log2(render / output) + TsrSettings::texture_bias_offset` on TAAU
    /// frames that upscale (ADR-192; mesh materials keep 0 under TAAU); 0 otherwise.
    /// `regia_terrain`'s `TerrainGpu::upload_shared` applies it.
    pub terrain_texture_lod_bias: f32,
    /// Stochastic SSR runs this frame and reads a surface code from the HDR alpha
    /// ([`crate::pack_surface_code`]). Set only when [`SharedPassDraw::screen_space_reflective`]
    /// or a mesh can be glossy. A draw that opted in writes the code where it is glossy; the
    /// older 0..1 alpha is not traced.
    pub sssr_codes: bool,
    /// World position of the local origin: the sum of the floating-origin rebases so far
    /// ([`crate::FrameRenderer::shift_history`], spec 78 § 12.2). Every position in this struct is
    /// local. A draw that keeps its own world-anchored space (terrain) adds this to local points;
    /// [`Self::in_world_space`] does it for the whole frame.
    pub origin_offset: [f32; 3],
    /// Sun cascades (bit `i` = cascade `i`) the dynamic-caster shadow class refreshes this
    /// frame (ADR-2028): the pages under [`SharedPassDraw::dynamic_shadow_boxes`] are redrawn
    /// there every frame, so content that sways may draw its moving pose into them. 0 when the
    /// class is off or the sun shadows are not paged.
    pub dynamic_shadow_cascades: u8,
    /// The scene wind (`FrameRenderer::set_foliage_wind`), m/s on world X and Z, sanitized;
    /// `None` without one, and `None` while a `SceneWind` sets it: the terrain's own wind follows
    /// that resource's gust-free mean, so grass would take it twice (ADR-2043). Shared content
    /// that sways (grass) may follow it.
    pub foliage_wind: Option<[f32; 2]>,
}

impl SharedFrameInfo {
    /// This frame re-expressed in the space that does not move with a rebase: a point at `p` here
    /// is at `p - origin_offset` in the frame's local space. Whole cells are exact in `f32`. The
    /// occluder block is moved too; the shadow-map rectangles are cascade UVs and need nothing.
    #[must_use]
    pub fn in_world_space(&self) -> Self {
        let offset = self.origin_offset;
        let to_world = offset.map(|axis| -axis);
        let mut out = *self;
        out.view_proj = crate::frame::shifted_view_proj(self.view_proj, to_world);
        out.unjittered_view_proj = crate::frame::shifted_view_proj(self.unjittered_view_proj, to_world);
        out.prev_view_proj = crate::frame::shifted_view_proj(self.prev_view_proj, to_world);
        out.eye = std::array::from_fn(|a| self.eye[a] + offset[a]);
        out.cascades = self
            .cascades
            .map(|cascade| crate::frame::shifted_view_proj(cascade, to_world));
        out.mesh_occluders.shift(offset);
        out
    }
}

/// Largest [`GroundBounceLevel`] grid the renderer accepts, cells per side.
pub const GROUND_BOUNCE_MAX: u32 = 64;

/// Most [`GroundBounceLevel`]s the renderer reads (finest first).
pub const GROUND_BOUNCE_LEVELS: usize = 2;

/// One square grid of [`GroundBounce`] cells around the camera.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroundBounceLevel {
    /// World XZ of the grid's −X/−Z corner, metres.
    pub origin: [f32; 2],
    /// Cell edge, metres.
    pub cell_m: f32,
    /// Cells per side (at most [`GROUND_BOUNCE_MAX`]).
    pub size: u32,
    /// Bounce radiance per cell centre (RGB, A unused), row-major with rows along +Z.
    pub radiance: Vec<[f32; 4]>,
    /// The terrain's horizon per cell, same layout: `tan θ_h` of the highest ground in each
    /// diagonal sector (azimuths 45°, 135°, 225°, 315° from +X toward +Z), 0 for open sky.
    /// The forward pass takes the sky it hides out of a mesh's sky light (a mesh in a canyon
    /// sees less sky, as the terrain beside it does). Empty: open sky everywhere.
    pub horizon: Vec<[f32; 4]>,
}

impl GroundBounceLevel {
    /// Whether the renderer can read this level (size within [`GROUND_BOUNCE_MAX`] and one
    /// radiance per cell).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.size > 0
            && self.size <= GROUND_BOUNCE_MAX
            && self.radiance.len() == (self.size * self.size) as usize
            && (self.horizon.is_empty() || self.horizon.len() == self.radiance.len())
    }
}

/// Light a shared draw's surface (terrain) bounces onto the ground around the camera, for
/// meshes standing on it: nested square grids of cells ([`GroundBounceLevel`], finest first),
/// each cell holding the bounce radiance a horizontal receiver gets there (irradiance / π,
/// linear RGB in the frame's HDR units).
///
/// The forward pass adds `albedo · radiance` (bilinear in the finest grid covering the
/// point, blended into the next one over its last cell) to a mesh's diffuse indirect light,
/// so a rock on a shadowed trench floor gets the light the floor gets from the sunlit walls.
/// Outside every grid the term is zero. With [`GroundBounceLevel::horizon`] it also removes
/// the sky the terrain hides from the mesh's sky light (atmosphere IBL frames only).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroundBounce {
    /// The grids, finest first; the renderer reads the first [`GROUND_BOUNCE_LEVELS`].
    pub levels: Vec<GroundBounceLevel>,
}

/// Records into the renderer's shared passes.
///
/// `draw` borrows `self` for the pass lifetime so the implementation can bind
/// its own groups. The renderer calls `prepare` first, on the CPU, with no
/// render pass open.
pub trait SharedPassDraw: Send + Sync {
    /// Upload uniforms and select geometry for this frame. Do not begin a pass.
    ///
    /// Buffer writes go through `upload`, which packs them into the frame's
    /// staging belt. They are copied in the frame encoder before the first pass,
    /// so the frame still submits once.
    fn prepare(&mut self, info: &SharedFrameInfo, upload: &mut SharedUpload<'_>);

    /// Draw into the pass the renderer has already begun.
    fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, kind: SharedPass);

    /// Whether this draw can write a glossy stochastic-SSR surface code when
    /// [`SharedFrameInfo::sssr_codes`] is set: a wet or icy material that opted in. The default
    /// (no) keeps the draw out of stochastic SSR: its older 0..1 alpha is not traced, and a frame
    /// whose only possibly glossy surface is this draw runs no reflection pass at all.
    fn screen_space_reflective(&self) -> bool {
        false
    }

    /// Builds, now, every pipeline variant a later frame of the configuration this draw was
    /// last prepared for can switch to: variants that depend on state set mid-play (a terrain's
    /// hole-cut set before any hole is cut). Called by the engine's frame warms
    /// ([`crate::FrameRenderer::warm_extracted`], [`crate::FrameRenderer::warm_scene`]) after
    /// their frames, on the warm worker, so the frame path never compiles them. Must not change
    /// what the draw records. The default builds nothing.
    fn warm_pipelines(&mut self) {}

    /// Binds the engine cascaded shadow maps for the colour pass.
    ///
    /// The default ignores them. The views are the same textures the cascade
    /// passes write; call this before those passes are recorded and before
    /// [`SharedPass::Color`]. Four views, unused slots included.
    fn set_engine_csm(&mut self, _views: &[wgpu::TextureView]) {}

    /// Binds the frame's screen-space ambient occlusion for the colour pass.
    ///
    /// The SSAO pass runs on the depth the shared draw wrote in [`SharedPass::Depth`], so
    /// the texture already holds the shared geometry's contact occlusion (cliff bases,
    /// rock contacts). R in `0..1`, white = none; it may be half resolution. `None`
    /// when the profile runs no SSAO. The default ignores it.
    fn set_engine_ao(&mut self, _ao: Option<&wgpu::TextureView>) {}

    /// Binds the frame's screen-space contact shadows for the colour pass.
    ///
    /// Like the SSAO the pass marches the depth the shared draw wrote in
    /// [`SharedPass::Depth`], so the texture holds the shared geometry's own contact shadows
    /// (rocks seated on the ground, micro relief). R in `0..1` is the sun's visibility
    /// (white = lit), full colour-target resolution; multiply it into the sun term only.
    /// `None` when the profile runs none this frame. The default ignores it.
    fn set_engine_contact_shadows(&mut self, _shadows: Option<&wgpu::TextureView>) {}

    /// Binds the frame's cloud transmittance map for the colour pass (ADR-320): the map
    /// (`R16Float`, read with `textureLoad`) and its parameters `[extent m, layer base y,
    /// layer top y, 1]`, for `cloud_sun_transmittance` in [`crate::cloud_shadow_wgsl`].
    /// Multiply it into the sun term only (the ambient stays). Called every single-sample frame
    /// before [`Self::finish_prepare`]; `None` when the frame has no map
    /// (`CloudSettings::shadow_map` off, no clouds, a low sun). The default ignores it.
    fn set_engine_cloud_shadow(&mut self, _map: Option<(&wgpu::TextureView, [f32; 4])>) {}

    /// Binds the sky light meshes are lit with this frame, exactly when
    /// [`SharedFrameInfo::sky_ambient`] is `Some` (`None` otherwise): `.0` the irradiance map
    /// (equirectangular `Rgba16Float`, irradiance / π per direction over `SKY_IBL_SCALE`),
    /// `.1` the split-sum BRDF table (`Rgba8Unorm`, u = N·V, v = perceptual roughness;
    /// R = F0 scale, G = bias). A shared draw that shades with them gets the same
    /// directional sky light and sky reflection as a mesh of the same normal (the
    /// circumsolar sky and the ground bounce included), not a uniform dome. The default
    /// ignores them.
    fn set_engine_sky_irradiance(&mut self, _sky: Option<(&wgpu::TextureView, &wgpu::TextureView)>) {}

    /// Called after [`Self::prepare`] once the engine views ([`Self::set_engine_csm`] and
    /// the engine SSAO) are bound for this frame, before any pass: upload what depends on
    /// them. The renderer prepares the shared draw before it builds the shadow jobs (so
    /// [`Self::cascade_shadow_key`] can decide them), which is before those views exist.
    /// The default does nothing.
    fn finish_prepare(&mut self, info: &SharedFrameInfo, upload: &mut SharedUpload<'_>) {
        let _ = (info, upload);
    }

    /// Identity of the geometry [`SharedPass::Shadow`] would draw this frame.
    ///
    /// When it matches the key of the last terrain draw into a cascade, that
    /// cascade's view-projection is unchanged and no mesh job cleared it, the
    /// renderer keeps the stored depth and skips the draw. `None` (the default)
    /// redraws every frame.
    fn shadow_key(&self) -> Option<u64> {
        None
    }

    /// Identity of the geometry [`SharedPass::Shadow`] would draw into cascade `index`.
    ///
    /// The renderer keeps a retained cascade while this key and the cascade's
    /// view-projection match the last draw into it, and clears and redraws that cascade
    /// alone when the key changes. The default is [`Self::shadow_key`], one key for every
    /// cascade.
    fn cascade_shadow_key(&self, index: u32) -> Option<u64> {
        let _ = index;
        self.shadow_key()
    }

    /// Draws cascade `index` where the renderer redraws only part of it (the pages under
    /// moved mesh casters, `SharedFrameInfo::shadow_regions[index]`). The pass is already
    /// scissored to that region; an implementation may draw only what overlaps it. The
    /// default draws the whole cascade ([`SharedPass::Shadow`]).
    fn draw_shadow_region<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, index: u32) {
        self.draw(pass, SharedPass::Shadow { index });
    }

    /// Draws the ground into the precipitation occlusion tile with
    /// [`SharedFrameInfo::precip_occlusion`]'s matrix (spec 99 § 5.6): splashes land on what the
    /// tile holds. The pass is the atlas, already scissored to the tile. The default draws nothing.
    fn draw_precip_occlusion<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>) {}

    /// Binds the shadow atlas that holds the precipitation occlusion tile
    /// ([`SharedFrameInfo::precip_occlusion`]); `None` when the frame has none. The default
    /// ignores it.
    fn set_engine_precip_occlusion(&mut self, _atlas: Option<&wgpu::TextureView>) {}

    /// Asks [`Self::warm_pipelines`] to build the surface-weather variant too, so the first wet
    /// frame compiles nothing. The default ignores it.
    fn set_warm_surface_weather(&mut self, _on: bool) {}

    /// `true` while the draw runs its surface-weather variant (spec 99 § 6.4). Default `false`.
    fn surface_weather_variant_on(&self) -> bool {
        false
    }

    /// Identity of what the draw shades that the frame's draw list cannot see: edited
    /// heights, rebound or rewritten textures and uniforms. A held camera reuses the last
    /// frame's screen-space composite only while this key is unchanged. The default (0)
    /// says the content never changes.
    fn content_key(&self) -> u64 {
        0
    }

    /// The light this draw's surface bounces onto the ground around the camera this frame,
    /// for meshes standing on it ([`GroundBounce`]), read after [`Self::prepare`]. `None`
    /// (the default) adds nothing.
    fn ground_bounce(&self) -> Option<&GroundBounce> {
        None
    }

    /// The heightfield the GPU irradiance probes trace (`crate::ddgi`): the draw's height
    /// texture (`R32Float`, level 0 read with `textureLoad`, world height = texel ×
    /// `height_scale` + `height_offset`) and its mapping. Read after [`Self::prepare`].
    /// `None` (the default) leaves the probes tracing meshes and sky only.
    fn gi_heightfield(&self) -> Option<(&wgpu::TextureView, crate::HeightfieldInfo)> {
        None
    }

    /// The ground albedo the probe rays see across the heightfield
    /// ([`Self::gi_heightfield`]): an `Rgba8UnormSrgb` texture over the whole map (u along
    /// +X, v along +Z, row 0 at −Z). `None` (the default) uses `HeightfieldInfo::albedo`.
    /// Its identity is `HeightfieldInfo::albedo_generation`.
    fn gi_albedo_map(&self) -> Option<&wgpu::TextureView> {
        None
    }

    /// Holes cut through the heightfield ([`Self::gi_heightfield`]) for the probe rays, and
    /// the mask's identity (it changes whenever the mask does): a storage buffer of
    /// `[count, 0, 0, 0]`, then per window eight words (centre x, z, cos and sin of the
    /// rotation, 1 / half extent x, z, two zero; f32 bits), then each window's 256 × 256
    /// coverage bytes, four a word, little end first (inside where the bilinear coverage
    /// reaches 50 %; `regia_terrain::HolesMask::gpu_words`). Inside a hole the probe rays see
    /// no ground, and probes there are not buried. `None` (the default) cuts nothing.
    fn gi_holes(&self) -> Option<(&wgpu::Buffer, u64)> {
        None
    }

    /// Binds the frame's irradiance probes for the colour pass: the `DdgiUniform` buffer
    /// and the probe data words ([`crate::ddgi_shading_wgsl`] reads both). Called every
    /// frame before [`Self::finish_prepare`], with `None` when the probes are off. A draw
    /// that shades with them takes its diffuse indirect light (sky, terrain bounce) from
    /// the probes where they cover it. The default ignores them.
    fn set_engine_gi(&mut self, _gi: Option<(&wgpu::Buffer, &wgpu::Buffer)>) {}

    /// Binds the frame's clustered point and spot lights for the colour pass: the forward
    /// pass's frame uniform, lights, froxel lists, shadow tiles and local-light shadow atlas
    /// ([`crate::EngineLights`]). Called every frame before [`Self::finish_prepare`] (single-
    /// sample frames; `None` when the buffers do not exist yet). A draw that shades with
    /// [`crate::shared_local_lights_wgsl`] lights its surface as a mesh of the same material
    /// would be lit: muzzle flashes, lamps and explosions on the ground. The default ignores
    /// them.
    fn set_engine_lights(&mut self, _lights: Option<&crate::EngineLights>) {}

    /// Binds the frame's emitter probe atlas, whose bottom bands hold the IES profiles of the
    /// frame's point and spot lights (`LightProfile`, ADR-2592), for a draw that shades with
    /// [`crate::shared_local_lights_profiled_wgsl`] (ADR-3247). Called every frame next to
    /// [`Self::set_engine_lights`]; `None` when the frame has no atlas. The default ignores it.
    fn set_engine_light_profiles(&mut self, _atlas: Option<&wgpu::TextureView>) {}

    /// Records this draw's own compute work at the start of the frame, after the frame's
    /// uploads and before any pass (the terrain bakes virtual-texture pages here). The
    /// default records nothing.
    fn encode_before_passes(&self, _encoder: &mut wgpu::CommandEncoder) {}

    /// Records work that reads what the shared colour pass wrote, right after it (the
    /// terrain reduces its virtual-texture feedback and copies the requests out for the CPU
    /// here). The default records nothing.
    fn encode_after_color(&self, _encoder: &mut wgpu::CommandEncoder) {}

    /// The surface this draw renders as a heightfield (heights, world mapping, albedo), for
    /// offline tools such as the reference path tracer
    /// ([`crate::FrameRenderer::reference_heightfield`]). Built on request, never per frame.
    /// `None` (the default) when the draw is not a heightfield.
    fn reference_heightfield(&self) -> Option<crate::ReferenceHeightfield> {
        None
    }

    /// Selects the camera the following [`Self::prepare`] and draws are for: `None` is the
    /// host's main view, `Some(id)` the render-to-texture camera with that asset id. The
    /// renderer calls it with `Some` before a texture view's frame and with `None` right after,
    /// so a draw that keeps per-camera state (a LOD selection, uploaded node slots, the CPU
    /// mirrors that skip unchanged uploads) keeps one set per camera and a texture view leaves
    /// the main view's untouched. While a texture view is selected the renderer does not call
    /// [`Self::encode_before_passes`] or [`Self::encode_after_color`]. The default keeps one
    /// state for every camera.
    fn set_view(&mut self, _view: Option<u64>) {}

    /// The outdoors the main view sees from inside a cell (plan row W-22, ADR-8413;
    /// [`crate::FrameRenderer::set_outdoor_portals`]). A draw of outdoor content (the terrain) may
    /// leave out of its camera passes what lies in none of the list's frusta, on frames whose eye
    /// [`crate::viscells::OutdoorPortals::restricts`]; shadow passes are not affected. The default
    /// ignores it.
    fn set_outdoor_portals(&mut self, _portals: Option<&crate::viscells::OutdoorPortals>) {}

    /// What the last main-view prepare culled through [`Self::set_outdoor_portals`]'s list. `None`
    /// (the default) for a draw that does not cull.
    fn outdoor_cull_counts(&self) -> Option<crate::viscells::OutdoorCullCounts> {
        None
    }

    /// Releases the per-camera state of texture view `id` (its camera is gone). The default
    /// does nothing.
    fn drop_view(&mut self, _view: u64) {}

    /// World boxes (`[min, max]`, metres) of content this draw animates in its shadow pass
    /// this frame (grass swaying in the wind): the dynamic-caster shadow class (ADR-2028) treats
    /// each as a dynamic caster, so the sun pages it covers in the refreshed cascades, and the
    /// local-light tiles it reaches, are redrawn every frame and the draw is asked for those
    /// regions ([`Self::draw_shadow_region`]). Only report content whose shadow actually moves;
    /// the order should be stable from frame to frame (an entry's index is its identity). The
    /// default (none) keeps every shadow page this draw writes cached as before.
    fn dynamic_shadow_boxes(&self) -> &[[[f32; 3]; 2]] {
        &[]
    }

    /// Sun cascades (bit `i` = cascade `i`) the content of [`Self::dynamic_shadow_boxes`] draws
    /// its moving pose into: a refreshed cascade outside this mask keeps its pages. The default
    /// is every cascade.
    fn dynamic_shadow_box_cascades(&self) -> u8 {
        u8::MAX
    }

    /// Tells the draw whether the frame's sun traces scene geometry (ADR-2310): `true` while
    /// [`crate::RenderProfile::sun_shadow_trace`] asks for ray queries or the compute reference,
    /// `false` when it stops. A draw that can hand the trace its casters
    /// ([`Self::trace_casters`]) builds them here when asked, and may release them when told
    /// `false`. Called on the main view before the frame chooses its variant, and only when the
    /// request changes. The default does nothing.
    fn set_trace_casters(&mut self, _on: bool) {}

    /// The casters this draw adds to the traced sun's scene, in its world-anchored space
    /// ([`crate::hwrt::SharedTraceCasters`]): a coarse proxy is enough, since the screen shading
    /// keeps the draw's own detail. `None` (the default) means the draw's casters cannot be
    /// traced, and a frame that asks for ray queries takes the screen-space trace on the shadow
    /// map instead (`rt_shadows_screen_shared_casters`), so no shadow is lost.
    fn trace_casters(&self) -> Option<crate::hwrt::SharedTraceCasters<'_>> {
        None
    }

    /// Whether the draw can take its sun visibility from the trace alone (the texture
    /// [`Self::set_engine_contact_shadows`] binds) and read no cascade, once told so by
    /// [`Self::set_engine_sun_traced`]. While it can, a traced frame stops drawing the sun
    /// cascades nothing else reads (ADR-2310). The default (no) keeps every cascade drawn.
    fn takes_traced_sun(&self) -> bool {
        false
    }

    /// `true` on a frame that skips the sun cascades: the draw must take its sun visibility from
    /// the trace alone (the cascades hold stale depth). Called before [`Self::finish_prepare`]
    /// on frames that trace, and once with `false` when that stops. Only called on a draw whose
    /// [`Self::takes_traced_sun`] is `true`. The default ignores it.
    fn set_engine_sun_traced(&mut self, _traced: bool) {}
}

/// Buffer uploads from [`SharedPassDraw::prepare`] into the frame's staging belt.
///
/// Each write is one copy recorded in the frame encoder before the first pass;
/// the belt uploads its chunk with a single `write_buffer`, so there is no
/// second submit. The destination needs `COPY_DST`, and the byte count must be
/// a multiple of 4.
pub struct SharedUpload<'a> {
    belt: &'a mut crate::staging::StagingBelt,
    device: &'a wgpu::Device,
    writes: u32,
    bytes: u64,
}

impl<'a> SharedUpload<'a> {
    pub(crate) fn new(belt: &'a mut crate::staging::StagingBelt, device: &'a wgpu::Device) -> Self {
        Self {
            belt,
            device,
            writes: 0,
            bytes: 0,
        }
    }

    /// Queues `data` for `dst` at `offset`.
    pub fn write(&mut self, dst: &wgpu::Buffer, offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.belt.defer(self.device, dst, offset, data);
        self.writes += 1;
        self.bytes += data.len() as u64;
    }

    /// Queues `data` for `dst` at offset 0 unless `cache` already holds it.
    /// Returns `true` when a write was queued.
    pub fn write_if_changed(&mut self, cache: &mut Vec<u8>, dst: &wgpu::Buffer, data: &[u8]) -> bool {
        if cache.as_slice() == data {
            return false;
        }
        self.write(dst, 0, data);
        cache.clear();
        cache.extend_from_slice(data);
        true
    }

    /// Writes queued so far.
    #[must_use]
    pub fn writes(&self) -> u32 {
        self.writes
    }

    /// Bytes queued so far.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}
