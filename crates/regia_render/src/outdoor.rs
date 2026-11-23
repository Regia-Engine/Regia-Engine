// SPDX-License-Identifier: MPL-2.0
//! Atmosphere, froxel fog and volumetric clouds on the frame (`specs/26` § 4, M15c).
//!
//! Lookup tables are integrated on the CPU ([`crate::build_atmosphere_luts`]) and sampled here.
//! The froxel grid and the cloud march run on the GPU. Mobile, Desktop Low and Deck leave the
//! three [`crate::FrameQuality`] switches off and keep the analytic sky plus height fog.

use crate::frame_work::counted::*;
use std::borrow::Cow;
use std::collections::HashMap;

use regia_core::glam::camera::rh::proj::directx::{orthographic, perspective};
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_core::glam::{Mat4, Vec3};

use crate::atmosphere::{
    AtmosphereLuts, AtmosphereParameters, AtmospherePreset, SkyBody, sample_sky_table, sun_disk_radiance,
    sun_position,
};
use crate::clouds::{
    CLOUD_CONE_SPAN, CLOUD_CURL_STRENGTH, CLOUD_CURL_TILE, CLOUD_DARK_EDGE, CLOUD_FADE_M, CLOUD_SHADOW_FLOOR,
    CloudSettings, OvercastMapTerms, build_weather_map, cloud_noise, cloud_shadow_params,
    cloud_shadow_refresh_rects, cloud_shadow_window, overcast_factor_from_terms, overcast_map_terms,
    overcast_sky,
};
use crate::volumetrics::{FogLight, FogVolume, VolumetricFogSettings};
use crate::{DirectionalLightUniform, RenderDevice, RenderError, TimeOfDay};

mod cloud_probe;
pub use cloud_probe::{
    CloudViewDesc, CloudViewImage, cloud_ambient_radiance, render_cloud_sun_map, render_cloud_view,
};
// The `aerial_from_tables` plate (ADR-3294).
mod table_air_plate;
pub use table_air_plate::{HorizonPlate, HorizonPlateImage, HorizonWeather, render_horizon_plate};

const FROXEL_X: u32 = 40;
const FROXEL_Y: u32 = 24;
const FROXEL_Z: u32 = 16;
const SHADOW_SIZE: u32 = 256;
/// How far the camera may drift in height from the height the live atmosphere tables were
/// built for before a rebuild is worth its bake plus the on-frame LUT/IBL upload
/// ([`OutdoorState::ensure_luts`]'s hysteresis). Matches the key's 10 m height bucket, so the
/// served staleness stays inside the envelope the grid quantization already allows; the
/// density scale heights (km) make the optical difference of 10 m well under a percent.
const SKY_HEIGHT_REBUILD_M: f32 = 10.0;

/// One RGBA8 image from an outdoor pass.
#[derive(Clone, Debug)]
pub struct OutdoorImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Tightly packed RGBA8, sRGB-encoded.
    pub pixels: Vec<u8>,
}

/// Two fog frames. `second` keeps history after the density is removed.
#[derive(Clone, Debug)]
pub struct FogShaftImages {
    /// Fog on, sun shadow active.
    pub first: OutdoorImage,
    /// Density cleared; history should still show the shaft.
    pub second: OutdoorImage,
    /// Pixel whose view ray sits in the occluder's shadow.
    pub shadowed: (u32, u32),
    /// Pixel whose view ray is outside that shadow.
    pub lit: (u32, u32),
}

/// An albedo map for a sky body ([`OutdoorParams::sky_body_albedo`]): equirectangular,
/// longitude across (0 at the left edge), north pole at the top, sRGB-encoded RGBA8.
#[derive(Clone)]
pub struct SkyBodyTexture {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Tightly packed sRGB RGBA8.
    pub rgba: Vec<u8>,
    key: u64,
}

impl SkyBodyTexture {
    /// Wraps an image. `None` when the byte count does not match `width × height × 4`.
    #[must_use]
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        if width == 0 || height == 0 || rgba.len() != (width as usize) * (height as usize) * 4 {
            return None;
        }
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (width, height).hash(&mut hasher);
        rgba.hash(&mut hasher);
        Some(Self {
            width,
            height,
            rgba,
            key: hasher.finish(),
        })
    }
}

impl std::fmt::Debug for SkyBodyTexture {
    // Short on purpose: the renderer hashes the outdoor params' Debug text every frame.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SkyBodyTexture({}x{}, {:016x})",
            self.width, self.height, self.key
        )
    }
}

/// Resources the runtime copies out of the ECS. Absent fields leave the feature idle.
#[derive(Clone, Debug, Default)]
pub struct OutdoorParams {
    /// The surface state the frame shades with (spec 99 § 3.4). It travels here so every host of
    /// [`crate::FrameRenderer::set_outdoor`] shades it and the held-camera cache sees a change.
    pub surface_weather: crate::surface_weather::SurfaceWeatherFrame,
    /// The precipitation's constant extinction, 1/m (spec 99 § 3.2).
    pub precip_extinction_per_m: f32,
    /// The scene wind for foliage (spec 99 § 3.3): `None` without a
    /// `regia_scene::weather::SceneWind`; with one, what the renderer's foliage wind is set to
    /// (`Some(None)`: off, or no air).
    pub scene_wind: Option<Option<[f32; 2]>>,
    /// When set on a physical-atmosphere profile, this replaces the directional light.
    pub time: Option<TimeOfDay>,
    /// Height-fog coefficients. Density zero leaves only [`Self::volumes`].
    pub fog: VolumetricFogSettings,
    /// Box and sphere density volumes.
    pub volumes: Vec<FogVolume>,
    /// Up to four local lights the fog march can see. The runtime fills it with the strongest at
    /// the camera ([`crate::pick_fog_lights`]).
    pub lights: Vec<FogLight>,
    /// Cloud layer. Coverage zero skips the march.
    pub clouds: CloudSettings,
    /// The planet's air ([`AtmosphereParameters::from_preset`]); the runtime fills it from
    /// `LevelSettings::atmosphere`. Default Earth.
    pub planet: AtmosphereParameters,
    /// A lit body in the sky (Jupiter from Europa).
    pub sky_body: Option<SkyBody>,
    /// Moons drawn in the sky beside [`Self::sky_body`] (up to two), lit by the sun.
    pub moons: Vec<SkyBody>,
    /// Albedo map of [`Self::sky_body`] (equirectangular, longitude across, sRGB RGBA8).
    /// `None` keeps the tint and procedural bands. The game loads the image (Jupiter
    /// from a mission mosaic, say) and passes it here.
    pub sky_body_albedo: Option<std::sync::Arc<SkyBodyTexture>>,
    /// Albedo maps of [`Self::moons`] (equirectangular, longitude across, sRGB RGBA8),
    /// one per moon in order; `moon_albedo[i]` textures `moons[i]`. `None` keeps that
    /// moon's tint, exactly as before the slot existed. Uploaded and sampled on the same
    /// path as [`Self::sky_body_albedo`], lit and phase-correct from the sun direction.
    pub moon_albedo: [Option<std::sync::Arc<SkyBodyTexture>>; 2],
    /// Interior volumes (tunnels, cabins). Fog inside one sees no sky light: the roof
    /// is between it and the sky, so the air in a bore does not glow with skylight.
    /// Four are read: all of them when there are four or fewer, else the four nearest the camera
    /// ([`crate::aerial_interior_slots`]). The runtime copies the `InteriorVolume` components.
    pub interiors: Vec<crate::InteriorVolume>,
    /// The planet's surface on sky rays that meet the ground: rays below the geometric
    /// horizon that no geometry covers, such as the view past a platform edge or past the
    /// far edge of a map. `None` (the default) draws it on a planet with air and leaves it
    /// black on an airless world; `Some(_)` forces it on or off for this sky.
    ///
    /// The composite is one Lambertian colour for the whole disc. Under air the aerial
    /// perspective fades it into the horizon haze; without air nothing does, and beyond a
    /// finite map it reads as a flat, untextured band between the terrain and space (ADR-162).
    pub planet_surface: Option<bool>,
    /// Where the froxel fog writes what it read for the sun shadow each drawn fog pass (the rule,
    /// the cascade and its bias; ADR-3122), for `render.fog_shadow`. `None` writes nothing. The
    /// runtime shares the `FogSunShadowProbe` resource here.
    pub fog_shadow_probe: Option<crate::fog::FogSunShadowProbe>,
}

impl OutdoorParams {
    /// Applies a map's [`LevelAtmosphere`](regia_scene::level::LevelAtmosphere): planet
    /// preset, sun strength, sky body and moons always; the sun clock and clouds only where
    /// no explicit [`TimeOfDay`] / [`CloudSettings`] resource set them (`time`/`clouds_set`);
    /// the haze only where [`Self::fog`] has no density of its own. An airless planet then
    /// forces `clouds.coverage` to 0 even when `clouds_set` is true. The cloud pass also
    /// stays off on an airless planet (`wants_clouds`), including a cirrus sheet left on
    /// that resource.
    pub fn apply_level(&mut self, level: &regia_scene::level::LevelAtmosphere, clouds_set: bool) {
        let mut planet = AtmosphereParameters::from_preset(level.preset);
        if level.preset == AtmospherePreset::Earth {
            planet.set_aerosol_turbidity(level.aerosol_turbidity);
        }
        if let Some(lux) = level.sun_illuminance_lux
            && lux > 0.0
        {
            planet.sun_illuminance = lux;
        }
        self.planet = planet;
        self.sky_body = level.sky_body;
        self.moons = level.moons.clone();
        // The map's dust layer, unless the game inserted froxel fog of its own.
        if self.fog.density <= 0.0 && level.haze.density > 0.0 {
            self.fog = VolumetricFogSettings {
                density: level.haze.density,
                height_falloff: 1.0 / level.haze.scale_height_m.max(1.0),
                base_height: level.haze.base_height_m,
                ..self.fog
            };
        }
        if self.time.is_none()
            && let Some(clock) = level.time_of_day
        {
            self.time = Some(TimeOfDay {
                hour: clock.hour,
                day_of_year: clock.day_of_year,
                latitude_deg: clock.latitude_deg,
            });
        }
        if !clouds_set && level.cloud_coverage > 0.0 {
            self.clouds = CloudSettings {
                coverage: level.cloud_coverage.clamp(0.0, 1.0),
                cloud_type: level.cloud_type.clamp(0.0, 1.0),
                ..CloudSettings::default()
            };
        }
        if planet.is_airless() {
            self.clouds.coverage = 0.0;
        }
    }

    /// Scattering albedo and Henyey–Greenstein asymmetry the froxel fog uses.
    ///
    /// [`VolumetricFogSettings::planet_medium`] on a planet with non-Earth air: the
    /// preset's aerosol, `mie_scattering / mie_extinction` per channel and the mean of
    /// its per-channel asymmetry. Otherwise the authored `albedo` and `anisotropy`.
    #[must_use]
    pub fn fog_medium(&self) -> ([f32; 3], f32) {
        let fog = &self.fog;
        let p = &self.planet;
        let has_aerosol = p.mie_extinction.iter().all(|e| *e > 0.0);
        if fog.planet_medium && has_aerosol && !self.is_earth() {
            let albedo = std::array::from_fn(|c| (p.mie_scattering[c] / p.mie_extinction[c]).clamp(0.0, 1.0));
            let g = (p.mie_g[0] + p.mie_g[1] + p.mie_g[2]) / 3.0;
            (albedo, g)
        } else {
            (fog.albedo, fog.anisotropy)
        }
    }

    /// The layer's overcast dims the sky light (ADR-321): `CloudSettings::overcast_ibl` with
    /// clouds on an Earth sky. Airless and Mars skies never change.
    #[must_use]
    pub fn overcast_applies(&self) -> bool {
        self.clouds.overcast_ibl && self.clouds.coverage > 0.0 && !self.planet.is_airless() && self.is_earth()
    }

    /// Whether the visible sky composites the planet's surface on rays that meet the
    /// ground: [`Self::planet_surface`], else on for a planet with air and off for an
    /// airless one.
    #[must_use]
    pub fn shows_planet_surface(&self) -> bool {
        self.planet_surface.unwrap_or(!self.planet.is_airless())
    }

    /// The planet is Earth's medium: the sun's strength and the aerosol turbidity may differ
    /// from the published baseline without changing the medium's identity.
    #[must_use]
    pub fn is_earth(&self) -> bool {
        let earth = AtmosphereParameters {
            sun_illuminance: self.planet.sun_illuminance,
            mie_scattering: self.planet.mie_scattering,
            mie_extinction: self.planet.mie_extinction,
            ..AtmosphereParameters::earth()
        };
        self.planet == earth
    }
}

/// What the frame passes to [`OutdoorState::plan_cloud_shadow_map`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct CloudMapFrame {
    /// Directional light travel direction.
    pub sun_travel: [f32; 3],
    /// Camera position (metres).
    pub eye: [f32; 3],
    /// The density's curl displacement follows the cone light switch, as in the view march.
    pub cone_light: bool,
    /// The frame runs the cloud march (single-sample HDR, a preset with clouds, not a
    /// texture view).
    pub allowed: bool,
}

/// What the frame hook passes into the outdoor passes. GPU only.
pub(crate) struct OutdoorComposite<'a> {
    /// Prepared atmosphere whose background the forward pass already drew (ADR-7963).
    /// Synthetic outdoor plates leave this empty and draw sky plus aerial together.
    pub prepared_sky: Option<&'a SkyPass>,
    /// HDR colour, single-sample.
    pub hdr: &'a wgpu::TextureView,
    /// Scene depth.
    pub depth: &'a wgpu::TextureView,
    /// Cascade 0, when the frame drew one.
    pub shadow: Option<&'a wgpu::TextureView>,
    /// HDR width.
    pub width: u32,
    /// HDR height.
    pub height: u32,
    /// HDR format.
    pub format: wgpu::TextureFormat,
    /// Inverse of the view-projection used to write depth.
    pub inv_view_proj: [[f32; 4]; 4],
    /// Inverse of the unjittered view-projection. Cloud view rays are built from this
    /// so TAA jitter does not swim the march. Depth unprojection stays on
    /// [`Self::inv_view_proj`].
    pub unjittered_inv_view_proj: [[f32; 4]; 4],
    /// Previous view-projection.
    pub prev_view_proj: [[f32; 4]; 4],
    /// Camera position (metres).
    pub eye: [f32; 3],
    /// Near plane (metres).
    pub near: f32,
    /// Far plane (metres).
    pub far: f32,
    /// Directional light travel direction.
    pub sun_travel: [f32; 3],
    /// Linear sun colour.
    pub sun_color: [f32; 3],
    /// Sun illuminance (lux).
    pub sun_illuminance: f32,
    /// Cascade 0 view-projection.
    pub light_vp: [[f32; 4]; 4],
    /// Cascade edge, or 0 when shadows are off.
    pub shadow_size: f32,
    /// Run the LUT sky.
    pub atmosphere: bool,
    /// Run froxel fog.
    pub fog: bool,
    /// Run the cloud march.
    pub clouds: bool,
    /// Unjittered view-projection of this frame. The cloud history stores it, and the next
    /// frame reprojects into it.
    pub unjittered_view_proj: [[f32; 4]; 4],
    /// March one texel in 16 per frame and reconstruct the rest from the reprojected
    /// history ([`crate::FrameQuality::cloud_reprojection`]). Off marches every texel.
    pub cloud_reprojection: bool,
    /// Percent of [`CloudSettings::steps`] the view march takes
    /// ([`crate::FrameQuality::cloud_step_percent`]).
    pub cloud_step_percent: u32,
    /// Cone light march, dark-edge in-scatter probability and curl-noise detail
    /// ([`crate::FrameQuality::cloud_cone_light`]). Off keeps the v0.1 light march.
    pub cloud_cone_light: bool,
    /// Marches per texel, step ceiling and largest divisor of the view march
    /// ([`crate::FrameQuality::cloud_march`], ADR-1403).
    pub cloud_march: crate::CloudMarch,
    /// `depth` is the frame's scene depth: froxel samples behind the surface are
    /// pulled back to it. Off for synthetic test shots with a placeholder depth.
    pub depth_is_scene: bool,
    /// Pre-exposure of `hdr` (`pre_exposure_scale`): sky, aerial, cloud and fog radiance
    /// is written × this scale.
    pub pre_exposure: f32,
    /// The local-light shadow atlas, when a fog light has a tile in it (ADR-1365 phase 2).
    pub local_shadow: Option<&'a wgpu::TextureView>,
    /// Per fog-light slot, its spot shadow tile ([`FogLightShadow`]); all zero for none.
    pub fog_light_shadows: [FogLightShadow; 4],
}

/// Inputs shared by sky-background and aerial-perspective preparation.
pub(crate) struct SkyInputs<'a> {
    /// Opaque scene depth, sampled after its render pass ends.
    pub(crate) depth: &'a wgpu::TextureView,
    /// HDR attachment width.
    pub(crate) width: u32,
    /// HDR attachment height.
    pub(crate) height: u32,
    /// HDR attachment format.
    pub(crate) format: wgpu::TextureFormat,
    /// Inverse of the jittered view-projection that produced depth.
    pub(crate) inv_view_proj: [[f32; 4]; 4],
    /// Camera position in meters.
    pub(crate) eye: [f32; 3],
    /// Far plane in meters, also the weather veil's reach.
    pub(crate) far: f32,
    /// Radiance scale of the HDR attachment.
    pub(crate) pre_exposure: f32,
    /// The frame will draw clouds, whose deck attenuates the weather veil.
    pub(crate) clouds: bool,
}

/// One frame's atmosphere bindings, shared across its early background and late aerial draws.
pub(crate) struct SkyPass {
    bind: wgpu::BindGroup,
    background: wgpu::RenderPipeline,
    aerial: Option<wgpu::RenderPipeline>,
}

impl SkyPass {
    /// Fills clear-depth pixels after opaque draws and before any transparent mesh (ADR-7963).
    pub(crate) fn draw_background(&self, encoder: &mut wgpu::CommandEncoder, color: &wgpu::TextureView) {
        draw_pass_seq(
            encoder,
            &[Some(&self.background)],
            &self.bind,
            color,
            "sky background",
            false,
        );
    }

    /// Applies aerial perspective without replacing clear-sky pixels a transparent draw touched.
    fn draw_aerial(&self, encoder: &mut wgpu::CommandEncoder, color: &wgpu::TextureView) {
        if let Some(aerial) = &self.aerial {
            draw_pass_seq(
                encoder,
                &[Some(aerial)],
                &self.bind,
                color,
                "atmosphere aerial",
                false,
            );
        }
    }
}

/// One fog light's spot shadow tile in the local atlas, six rows: the tile's view-projection
/// (four columns), its atlas rect (`scale.xy`, `offset.zw`) and its bias row (`x` = texel size
/// per unit view depth, `y` = `1/near − 1/far`, `z` = 1 for a perspective tile, `w` = 1 when
/// the slot is shadowed). All zero leaves the light unshadowed.
pub(crate) type FogLightShadow = [[f32; 4]; 6];

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SkyGpu {
    inv_view_proj: [[f32; 4]; 4],
    sun: [f32; 4],
    camera: [f32; 4],
    disk: [f32; 4],
    misc: [f32; 4],
    body_dir: [f32; 4],
    body_color: [f32; 4],
    stars: [f32; 4],
    /// Two moons: direction (xyz) and angular radius (w, 0 = none); tint (rgb) and a
    /// textured flag (w = 1 samples that moon's albedo map).
    moons: [[f32; 4]; 4],
    /// x = 1 when the sky body samples its albedo map. y = HDR pre-exposure
    /// (`fs_sky_hdr` / `fs_aerial` write radiance × y). z = camera height above the
    /// surface in kilometres (at least [`crate::atmosphere::MIN_CAMERA_HEIGHT_M`]).
    body_tex: [f32; 4],
    /// Planet surface seen by a ray that meets the ground: rgb is Lambertian radiance
    /// (cd/m²), w is the bottom radius in kilometres. w ≤ 0 skips the hit test, so the
    /// analytic sky is unchanged.
    ground: [f32; 4],
    /// The `aerial_from_tables` row (ADR-3294), read only by the table entries: x = 1 full, 2 lite;
    /// y = the weather's extinction (1/m); z = how far a sky pixel's rain reaches (m); w = the
    /// drawn deck's overcast factor.
    table_air: [f32; 4],
    /// The rain veil's constant part (ADR-3294,
    /// [`crate::horizon_seam::rain_inscatter_constant`]): rgb in cd/m², w unused. Zero unless
    /// the table entries draw.
    table_ground: [f32; 4],
    /// The interior boxes the aerial entries and the veil take out of the air (ADR-4757): the
    /// fog's rows (`interior_slots`). All zero leaves every entry as before.
    interiors: [[f32; 4]; 8],
    /// The cloud transmittance map for the air (ADR-5944; `cloud_shadow.wgsl`): extent (m), layer
    /// base and top, 1 when the aerial entries read it ([`OutdoorState::cloud_sun_params`]). All
    /// zero leaves every entry as before.
    cloud: [f32; 4],
    /// x = the overcast factor the sky's share of the ground light takes under the layer
    /// (ADR-321's rule; 0 leaves it). yzw unused. Read only under `cloud.w`.
    cloud_air: [f32; 4],
    /// The light on flat ground under a clear sky ([`crate::ground_light`]): the sun's share
    /// (rgb, irradiance / π), which the air's multiple scattering loses where the layer shadows
    /// the ray (ADR-5944). w unused. Read only under `cloud.w`.
    cloud_ground_sun: [f32; 4],
    /// The sky's share of the same light (rgb). w unused. Read only under `cloud.w`.
    cloud_ground_sky: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FogGpu {
    inv_view_proj: [[f32; 4]; 4],
    light_vp: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    camera: [f32; 4],
    prev_camera: [f32; 4],
    sun_travel: [f32; 4],
    sun_color: [f32; 4],
    grid: [f32; 4],
    media: [f32; 4],
    albedo: [f32; 4],
    misc: [f32; 4],
    screen: [f32; 4],
    volumes: [[f32; 4]; 8],
    /// Four local lights, three rows each: position (xyz) and radius (w); colour (rgb) and
    /// candela (w); the cone row ([`FogLight::gpu_cone`]).
    lights: [[f32; 4]; 12],
    ambient: [f32; 4],
    interiors: [[f32; 4]; 8],
    /// Cloud transmittance map (`cloud_shadow.wgsl`): extent, layer base and top, 1 when the
    /// fog reads it (`CloudSettings::fog_coupling`); zero leaves the fog as before.
    cloud: [f32; 4],
    /// The fog lights' spot shadow tiles, six rows per slot ([`FogLightShadow`]).
    light_shadow: [[f32; 4]; 24],
    /// Valley mist rows ([`crate::ValleyMist::gpu_rows`]); row 2's `w` of zero leaves it off.
    mist: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CloudGpu {
    inv_view_proj: [[f32; 4]; 4],
    sun: [f32; 4],
    sun_color: [f32; 4],
    layer: [f32; 4],
    wind: [f32; 4],
    shape: [f32; 4],
    camera: [f32; 4],
    ambient_top: [f32; 4],
    ambient_bottom: [f32; 4],
    draw: [f32; 4],
    target: [f32; 4],
    /// x = the HDR target's pre-exposure (the composite writes radiance × x).
    /// y = the march's resolution divisor.
    hdr: [f32; 4],
    /// Inverse of the unjittered view-projection (cloud view rays).
    ray_from_clip: [[f32; 4]; 4],
    /// Unjittered view-projection of the frame that wrote the history being read.
    history_view_proj: [[f32; 4]; 4],
    /// x, y = the 4×4 sub-position this frame marches; z, w = fresh target size.
    temporal: [f32; 4],
    /// x, y = wind drift (m) since the history frame; z = 1 when the history is valid.
    reproject: [f32; 4],
    /// Sun colour × lux transmitted to the layer base (rgb); w = 1 when the aerial LUT is
    /// bound.
    sun_bottom: [f32; 4],
    /// The same at the layer top (rgb); w = 1 for the cone light march.
    sun_top: [f32; 4],
    /// x = curl displacement (m), y = dark-edge strength, z = curl tile (m), w = cone length
    /// as a fraction of the layer thickness.
    light: [f32; 4],
    /// Staleness of each 4×4 sub-position's history, `[y][x]` ([`cloud_sub_staleness`]).
    stale: [[f32; 4]; 4],
    /// [`crate::CloudLook::look_row`]: diffusion, erosion, breakup, breakup tile (ADR-1408).
    look: [f32; 4],
    /// [`crate::CloudLook::cirrus_row`]: coverage, altitude (m), optical depth, streak tile (m).
    cirrus: [f32; 4],
    /// The sun transmitted to the cirrus sheet (colour × lux, rgb); w = streak stretch.
    cirrus_sun: [f32; 4],
    /// [`crate::cirrus_direction_row`]: streak direction (x, z) and the sheet's drift (m).
    cirrus_dir: [f32; 4],
    /// [`crate::CloudLook::cirrus_fiber_row`]: fiber, fiber scale (m), shear, hook (ADR-1509).
    cirrus_fiber: [f32; 4],
    /// [`crate::CloudLook::cirrus_shear_row`]: shear direction (cos, sin), breakup, fibrous flag.
    cirrus_shear: [f32; 4],
    /// [`crate::CloudLook::cirrus_crisp_row`]: crisp amount, sheet samples per texel (ADR-1516).
    cirrus_crisp: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<SkyGpu>() == 496);
const _: () = assert!(std::mem::size_of::<FogGpu>() == 1264);
const _: () = assert!(std::mem::size_of::<CloudGpu>() == 624);

struct Tex {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// Views a sky group binds (sky LUT, aerial LUT, depth, body albedo, moon0 albedo, moon1 albedo,
/// cloud map, the aerial table's sun share).
type SkyBindKey = (
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
);
/// Views a fog group binds (shadow, scatter, integrated, history, depth, cloud map, local shadow
/// atlas, mist grid).
type FogBindKey = (
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
);
/// Views a cloud group binds (weather, depth, cloud colour, shape, detail, cloud distance,
/// history colour, history distance, aerial LUT, curl field).
type CloudBindKey = (
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
);

/// Distance targets of the cloud march: R scatter distance, G footprint-centre scene
/// distance (metres, half float), B the reconstruction's confidence.
const CLOUD_DIST_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The reduced-resolution cloud targets: a ping-pong history (colour and distance) at the
/// march resolution and the fresh target at one texel per 4×4 block.
struct CloudTargets {
    low: [u32; 2],
    color: [Tex; 2],
    dist: [Tex; 2],
    fresh: Tex,
    fresh_dist: Tex,
}

/// Frames between two marches of one cloud texel (`CLOUD_BLOCK`², one texel per block a
/// frame): the history holds texels marched on any of the last this many frames.
const CLOUD_REFRESH_FRAMES: usize = (crate::clouds::CLOUD_BLOCK * crate::clouds::CLOUD_BLOCK) as usize;

/// What the cloud history holds: the slot last written, the unjittered view-projection and
/// wind drift it was marched with, and the march size.
#[derive(Debug, Clone, Copy)]
struct CloudHistory {
    slot: usize,
    view_proj: [[f32; 4]; 4],
    drift: [f32; 2],
    low: [u32; 2],
    /// The layer's weather values on the last [`CLOUD_REFRESH_FRAMES`] frames, newest first:
    /// the history holds texels marched with any of them (ADR-429).
    weather: [crate::clouds::CloudWeatherSample; CLOUD_REFRESH_FRAMES],
}

/// How stale the weather leaves the cloud history, `0..1` (`reproject.w`, ADR-429): the largest
/// [`crate::clouds::cloud_weather_motion`] from any weather of the refresh window to `now`,
/// times [`CLOUD_REFRESH_FRAMES`], capped at 1.
///
/// - The window, not the previous frame: a texel is marched once every
///   [`CLOUD_REFRESH_FRAMES`] frames, so after a transition ends the history still holds clouds
///   of its last frames. Measured against the previous frame, the clamp fell to 0 on the first
///   held frame and that history faded out at the fresh-sample weight, one halving per refresh.
/// - The gain: the cut thresholds are per frame, and a texel keeps `(1 − w)` of a stale history
///   when it is marched (`w = mix(CLOUD_FRESH_WEIGHT, 1, clamp)`). Scaled 1:1, a change of half
///   a cut over the window left a quarter of it in the next refresh, several times the march
///   noise. A change of 1/16 of a cut over the window now replaces the history whole; steady
///   weather still reads exactly 0.
fn cloud_history_weather_motion(
    window: &[crate::clouds::CloudWeatherSample],
    now: crate::clouds::CloudWeatherSample,
) -> f32 {
    let moved = window
        .iter()
        .map(|then| crate::clouds::cloud_weather_motion(*then, now))
        .fold(0.0, f32::max);
    (moved * CLOUD_REFRESH_FRAMES as f32).min(1.0)
}

/// How stale the weather leaves the history of each 4×4 sub-position on cloud frame `frame`,
/// `[y][x]` (`stale` in `clouds.wgsl`, ADR-1410): [`crate::clouds::cloud_weather_motion`] from the
/// weather of the sub-position's last march to `now`, times [`CLOUD_REFRESH_FRAMES`], capped at 1.
/// `window` is the history's weather, newest (the previous frame's) first.
///
/// The sub-position marched `a` frames ago (Bayer order, [`crate::clouds::cloud_bayer_offset`])
/// holds clouds of `window[a - 1]`; this frame's own sub-position was last marched 16 frames ago.
/// [`cloud_history_weather_motion`] took the largest over the whole window for every texel: after
/// a transition ended, texels already re-marched at the new weather stayed clamped into their
/// fresh neighbours' box for up to 16 frames and lost their sub-block detail (the held-frame error
/// of `cloud_weather::coverage_transition_tracks_the_full_march`).
fn cloud_sub_staleness(
    window: &[crate::clouds::CloudWeatherSample; CLOUD_REFRESH_FRAMES],
    now: crate::clouds::CloudWeatherSample,
    frame: u32,
) -> [[f32; 4]; 4] {
    let mut stale = [[0.0; 4]; 4];
    for (age, then) in (1u32..).zip(window.iter()) {
        let [x, y] = crate::clouds::cloud_bayer_offset(frame.wrapping_sub(age));
        let moved = crate::clouds::cloud_weather_motion(*then, now);
        stale[y as usize][x as usize] = (moved * CLOUD_REFRESH_FRAMES as f32).min(1.0);
    }
    stale
}

/// Format of the cloud transmittance map (ADR-320).
const CLOUD_MAP_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R16Float;

/// What the cloud transmittance map holds (ADR-320): the window it was last marched for and the
/// keys whose change marches it whole.
#[derive(Debug, Clone, Copy, Default)]
struct CloudMapState {
    /// First cell of the window last marched (`None`: the next frame marches the whole map).
    start: Option<[i32; 2]>,
    /// Frames since the map was created (selects the refresh band).
    frame: u32,
    /// Hash of the settings the content depends on, except the wind clock.
    key: u64,
    /// Sun direction, quantised like the IBL key.
    sun_key: (i32, i32, i32),
    /// Wind drift (m) the map was last marched with.
    drift: [f32; 2],
    /// Map edge in texels.
    size: u32,
}

/// The map pass planned for this frame, recorded at the start of the frame encoder.
struct PendingCloudMap {
    pipe: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    view: wgpu::TextureView,
    rects: Vec<[u32; 4]>,
}

/// Overcast factor and the weather key it was measured for (ADR-321).
type OvercastCache = Option<(u64, f32)>;

/// IBL bake key: sun direction and camera height (quantised), and the overcast factor in
/// percent.
type IblKey = (i32, i32, i32, i32, i32);

struct OutdoorGpu {
    /// Bind groups by the views they bind: rebuilt only when a view is replaced, never per
    /// frame (`specs/27` § 2.4).
    sky_binds: crate::bind_cache::BindCache<SkyBindKey>,
    fog_binds: crate::bind_cache::BindCache<FogBindKey>,
    cloud_binds: crate::bind_cache::BindCache<CloudBindKey>,
    /// The cloud transmittance map pass's groups (its own uniform buffer).
    cloud_map_binds: crate::bind_cache::BindCache<CloudBindKey>,
    sky_bgl: wgpu::BindGroupLayout,
    sky_layout: wgpu::PipelineLayout,
    sky_module: wgpu::ShaderModule,
    sky_pipes: HashMap<(wgpu::TextureFormat, u32), wgpu::RenderPipeline>,
    fog_bgl: wgpu::BindGroupLayout,
    fog_layout: wgpu::PipelineLayout,
    fog_module: wgpu::ShaderModule,
    fog_pipes: HashMap<(wgpu::TextureFormat, u32), wgpu::RenderPipeline>,
    cloud_bgl: wgpu::BindGroupLayout,
    cloud_layout: wgpu::PipelineLayout,
    cloud_module: wgpu::ShaderModule,
    cloud_pipes: HashMap<(wgpu::TextureFormat, u32), wgpu::RenderPipeline>,
    /// The device's pipelines: the sky, fog and cloud pipelines are built once per device
    /// (ADR-220), so a renderer's first frame on a map with clouds or fog, or a second
    /// renderer, compiles nothing a warm built.
    shared: std::sync::Arc<crate::device_pipelines::DevicePipelines>,
    /// The lab's fog occluder (built by its first draw; no game frame draws it).
    occluder: Option<wgpu::RenderPipeline>,
    sky_buf: wgpu::Buffer,
    fog_buf: wgpu::Buffer,
    cloud_buf: wgpu::Buffer,
    /// Uniform of the cloud transmittance map pass: recorded before the frame's passes, while
    /// `cloud_buf` holds the view march's values for the whole submission.
    cloud_map_buf: wgpu::Buffer,
    /// The cloud transmittance map (edge in texels, texture), ADR-320.
    cloud_map: Option<(u32, Tex)>,
    /// Bumped whenever `cloud_map` is created or dropped (bind groups that hold it rebuild).
    cloud_map_generation: u64,
    linear: wgpu::Sampler,
    shape_noise: Option<Tex>,
    detail_noise: Option<Tex>,
    /// 2D curl field that swirls the detail erosion ([`crate::clouds::cloud_curl`]).
    curl_noise: Option<Tex>,
    /// History pair and fresh target, resized with the frame.
    cloud_targets: Option<CloudTargets>,
    /// 1×1 stand-in bound while the march target is the colour attachment.
    cloud_dummy: Tex,
    dummy_depth: Tex,
    dummy_rgba: Tex,
    /// The sky body's albedo map (keyed); `white_srgb` stands in when there is none.
    body_albedo: Option<(u64, Tex)>,
    /// Each moon's albedo map (keyed); `white_srgb` stands in when there is none.
    moon_albedo: [Option<(u64, Tex)>; 2],
    white_srgb: Tex,
    sky_lut: Option<Tex>,
    aerial_lut: Option<Tex>,
    /// The sun's single-scattering share of the aerial table (ADR-5944) and the table
    /// generation it holds. Uploaded only on frames whose aerial composite reads the cloud map.
    aerial_sun_lut: Option<(Option<u64>, Tex)>,
    /// [`OutdoorState::lut_generation`] the LUT textures hold.
    lut_generation: Option<u64>,
    scatter: Option<Tex>,
    integrated: [Option<Tex>; 2],
    shadow: Option<Tex>,
    weather: Option<(u64, Tex)>,
    /// The valley mist's terrain grid (content key, texture), `dummy_rgba` stands in without one.
    mist_grid: Option<(u64, Tex)>,
    occluder_vb: wgpu::Buffer,
}

/// Persistent outdoor state on [`crate::FrameRenderer`].
#[derive(Default)]
pub struct OutdoorState {
    params: OutdoorParams,
    luts: Option<AtmosphereLuts>,
    lut_key: Option<(i32, i32)>,
    lut_planet: Option<AtmosphereParameters>,
    /// Bumped whenever [`Self::luts`] is replaced (the GPU copy re-uploads only then).
    lut_generation: u64,
    /// The planet's sun-independent tables, reused by every rebuild.
    base: Option<std::sync::Arc<crate::atmosphere::AtmosphereBase>>,
    ibl_key: Option<IblKey>,
    /// Rebuild the sun/height tables and the IBL on a worker thread ([`Self::set_async_sky`]).
    async_sky: bool,
    /// The running rebuild, if any.
    sky_job: Option<SkyJob>,
    /// A baked IBL the worker finished, waiting for [`Self::write_ibl`].
    ready_ibl: Option<(IblKey, BakedIbl)>,
    gpu: Option<OutdoorGpu>,
    prev_eye: [f32; 3],
    have_history: bool,
    history_slot: u32,
    cloud_frame: u32,
    /// The reconstructed clouds the next frame reprojects, `None` after a camera cut.
    cloud_history: Option<CloudHistory>,
    /// Test shots: the LDR sky pass renders a panorama.
    panorama: bool,
    /// Keep each star's light the same at every output height ([`Self::set_star_flux`], ADR-2486).
    star_flux: bool,
    /// The cloud transmittance map's content (ADR-320).
    cloud_map: CloudMapState,
    /// The map pass planned this frame ([`Self::plan_cloud_shadow_map`]).
    pending_cloud_map: Option<PendingCloudMap>,
    /// The map is current for this frame's sun and window (planned this frame).
    cloud_map_live: bool,
    /// Overcast factor `0..1` of this frame's layer (0 unless `CloudSettings::overcast_ibl`
    /// applies, ADR-321), and its cache.
    overcast: f32,
    /// The share of the sun this frame's layer takes from the ground on average (mean cover
    /// times the opacity to the sun of a column of that cover,
    /// `crate::clouds::sun_cover_from_terms`), when the cloud transmittance map shadows the
    /// ground and the overcast sky light applies (`CloudSettings::shadow_map` and
    /// `overcast_ibl`, [`OutdoorParams::overcast_applies`]): the image light's ground bounce
    /// then comes from that shadowed ground (ADR-5944). `None` keeps the bounce of a sunlit
    /// ground, as before: the image light changes only under `overcast_ibl`, so a frame with
    /// the map alone (and a texture view lit by the main view's image light, ADR-184) keeps it.
    ground_cover: Option<f32>,
    overcast_cache: OvercastCache,
    /// Packed-map terms for the quantized weather key (ADR-5159). Altitude and density are
    /// applied on top and do not rebuild the map.
    overcast_terms: Option<(u64, f32, f32)>,
    /// The weather map's noise field for the layer's resolution and seed (ADR-429): a coverage
    /// or type change re-thresholds it instead of rebuilding the noise.
    weather_field: Option<std::sync::Arc<crate::clouds::WeatherField>>,
    /// The layer's weather values the last frame used ([`crate::cloud_weather_cut`]).
    last_weather: Option<crate::clouds::CloudWeatherSample>,
    /// The valley mist ([`crate::ValleyMist`], ADR-1394) and the quality tier this frame draws it at.
    mist: Option<crate::ValleyMist>,
    mist_quality: crate::MistQuality,
    /// Every sun-cascade view the froxel fog may sample ([`Self::set_fog_shadow_views`]): a cut
    /// to another view range can pick another cascade, so its groups are built ahead (ADR-2303).
    fog_shadow_views: Vec<wgpu::TextureView>,
    /// This frame's `aerial_from_tables` mode ([`Self::set_table_air`], ADR-3294).
    table_air: TableAir,
    /// Sky passes drawn with the table entries ([`Self::table_air_frames`]).
    table_air_frames: u64,
    /// Weather veils drawn ([`Self::weather_veils`]).
    weather_veils: u64,
    /// The aerial composite ignores the interior volumes ([`Self::set_aerial_interiors`], ADR-4757): off by
    /// default, so the composite reads them.
    aerial_interiors_off: bool,
    /// The last sky pass bound the cloud transmittance map for the air (ADR-5944): the weather
    /// veil binds the same group.
    sky_cloud_air: bool,
    /// Sky passes whose aerial composite read the cloud transmittance map
    /// ([`Self::cloud_air_frames`], ADR-5944).
    cloud_air_frames: u64,
}

/// The sky pipeline entry of the weather veil (`fs_weather_veil`, ADR-3294).
const VEIL_ENTRY: u32 = 6;

/// Where an atmosphere frame's ground haze and weather extinction come from
/// ([`crate::RenderProfile::aerial_from_tables`], ADR-3294).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TableAir {
    /// The default: the sky's tables give the ground its aerial perspective, and the forward
    /// pass, the lite pass and the terrain add the weather's extinction with the analytic
    /// `haze_inscatter` (the sky gets none).
    #[default]
    Off,
    /// The outdoor composite adds the weather's extinction to the sky, the clouds and the ground
    /// as one term (the weather veil) with the horizon sky from the tables as its in-scatter,
    /// and carries the aerial perspective past the last slice to the sky's last row. Two aerial
    /// slices, four texel loads each, plus four for the horizon sky past the last slice.
    Full,
    /// [`Self::Full`] for the lite (Mobile) path: one bilinear tap of one aerial slice (four
    /// texel loads), plus four for the horizon sky past the last slice. No froxel volume.
    Lite,
}

impl TableAir {
    /// The mode a frame takes: [`Self::Off`] unless the profile asks for it and the frame
    /// draws the atmosphere tables (`atmosphere_frame`: the LUT sky pass runs, under air);
    /// [`Self::Lite`] when the frame draws with the lite forward shader.
    #[must_use]
    pub fn for_frame(quality: &crate::FrameQuality, atmosphere_frame: bool) -> Self {
        if !quality.aerial_from_tables || !atmosphere_frame {
            Self::Off
        } else if quality.forward_lite == crate::forward_lite::ForwardLite::Off {
            Self::Full
        } else {
            Self::Lite
        }
    }

    /// Stable lower-case name (`"off"`, `"full"`, `"lite"`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Full => "full",
            Self::Lite => "lite",
        }
    }

    /// Whether the outdoor composite owns the weather's extinction this frame (so the forward
    /// pass, the lite pass and the terrain must not add theirs).
    #[must_use]
    pub const fn is_on(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Texel loads the aerial term of one ground pixel costs, without and with the horizon-sky
    /// tap (past the last slice). The default pass is 8 and 8. Under precipitation the weather
    /// veil adds four per pixel, sky and ground alike.
    #[must_use]
    pub const fn aerial_fetches(self) -> (u32, u32) {
        match self {
            Self::Off => (8, 8),
            Self::Full => (8, 12),
            Self::Lite => (4, 8),
        }
    }

    /// The sky pipeline entries the mode draws with: the sky, then the aerial perspective.
    const fn sky_entries(self) -> (u32, u32) {
        match self {
            Self::Off => (1, 2),
            Self::Full => (1, 4),
            Self::Lite => (1, 5),
        }
    }
}

impl OutdoorState {
    /// Sets this frame's `aerial_from_tables` mode ([`TableAir::for_frame`]). The frame calls
    /// it before the forward uniforms and the outdoor composite; [`TableAir::Off`] (the default)
    /// draws the sky exactly as before.
    pub fn set_table_air(&mut self, mode: TableAir) {
        self.table_air = mode;
    }

    /// Sky passes drawn with the table entries since the state was made.
    #[must_use]
    pub fn table_air_frames(&self) -> u64 {
        self.table_air_frames
    }

    /// Weather veils drawn since the state was made ([`TableAir`], ADR-3294).
    #[must_use]
    pub fn weather_veils(&self) -> u64 {
        self.weather_veils
    }

    /// Sky passes, since the state was made, whose aerial composite read the cloud
    /// transmittance map: the air under the layer took its shadow (ADR-5944). It counts on
    /// exactly the frames whose surfaces read the map for their sun term
    /// (`CloudSettings::shadow_map` on, a map planned, a planet with air).
    #[must_use]
    pub fn cloud_air_frames(&self) -> u64 {
        self.cloud_air_frames
    }

    /// The veil runs on a table frame under air with a precipitation extinction.
    fn veil_this_frame(&self) -> bool {
        self.table_air.is_on()
            && self.params.precip_extinction_per_m > 0.0
            && self.luts.as_ref().is_some_and(|luts| !luts.params.is_airless())
    }

    /// The precipitation's extinction over the whole image (`fs_weather_veil`): draws into the
    /// HDR target with the sky uniform `draw_sky` wrote this frame.
    fn draw_weather_veil(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        frame: &OutdoorComposite<'_>,
    ) {
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        let bg = gpu.sky_bind(device, frame.depth, self.sky_cloud_air);
        let pipe = gpu.sky_pipe(device, frame.format, VEIL_ENTRY);
        draw_pass(encoder, &pipe, &bg, frame.hdr, "weather veil", false);
        self.weather_veils += 1;
    }
    /// Whether the aerial composite takes the interior volumes' air out of the in-scatter (ADR-4757; on by
    /// default). Off, it draws the air as before ADR-4757 (the before plates of a look comparison).
    pub(crate) fn set_aerial_interiors(&mut self, on: bool) {
        self.aerial_interiors_off = !on;
    }

    /// Whether [`Self::set_aerial_interiors`] is on.
    pub(crate) fn aerial_interiors(&self) -> bool {
        !self.aerial_interiors_off
    }

    /// Keeps the star field at every output height (ADR-2486): the pixel angle from `pixel_angle_chord` and the
    /// star brightness times `(height / 1080)²`. Off by default.
    pub(crate) fn set_star_flux(&mut self, on: bool) {
        self.star_flux = on;
    }

    /// Whether [`Self::set_star_flux`] is on.
    pub(crate) fn star_flux(&self) -> bool {
        self.star_flux
    }

    /// The sun-cascade views of this frame, any of which the fog may sample on a later frame
    /// (empty without sun shadows). Cheap when unchanged.
    pub(crate) fn set_fog_shadow_views(&mut self, views: &[wgpu::TextureView]) {
        if self.fog_shadow_views.as_slice() != views {
            self.fog_shadow_views = views.to_vec();
        }
    }

    /// Replaces the resources extracted for this frame.
    ///
    /// A weather cut ([`crate::cloud_weather_cut`]: coverage, type or density jumping further in
    /// one frame than a transition moves them) drops the cloud history and the cloud
    /// transmittance map's window and restarts the cloud noise phase, like a camera cut, so the
    /// next cloud frame marches both whole (ADR-429).
    pub fn set_params(&mut self, params: OutdoorParams) {
        if params.fog != self.params.fog || params.volumes != self.params.volumes {
            self.have_history = false;
        }
        let weather = params.clouds.weather_sample();
        if let Some(prev) = self.last_weather
            && crate::clouds::cloud_weather_cut(prev, weather)
        {
            self.cloud_history = None;
            self.cloud_frame = 0;
            self.cloud_map.start = None;
            crate::clouds::count_cloud_work(|c| c.weather_cuts += 1);
        }
        self.last_weather = Some(weather);
        self.params = params;
        self.overcast = self.measure_overcast();
        self.ground_cover = self.measure_ground_cover();
    }

    /// The cached weather field for the layer ([`crate::WeatherField`]), built when the
    /// map resolution or seed changes. `None` for patterns that need none.
    fn weather_field(&mut self) -> Option<std::sync::Arc<crate::clouds::WeatherField>> {
        let settings = self.params.clouds;
        if settings.pattern != crate::CloudPattern::Coverage {
            return None;
        }
        if let Some(field) = self.weather_field.as_ref().filter(|f| f.matches(&settings)) {
            return Some(std::sync::Arc::clone(field));
        }
        let field = std::sync::Arc::new(crate::clouds::WeatherField::new(&settings));
        self.weather_field = Some(std::sync::Arc::clone(&field));
        Some(field)
    }

    /// The layer's overcast factor (ADR-321) when [`OutdoorParams::overcast_applies`], else 0.
    /// The weather map is rebuilt on the CPU only when its key changes.
    fn measure_overcast(&mut self) -> f32 {
        if !self.params.overcast_applies() {
            return 0.0;
        }
        self.layer_overcast()
    }

    /// The layer's mean cover of the ground when the cloud transmittance map shadows it and
    /// the overcast sky light applies ([`Self::ground_cover`]), else `None`.
    fn measure_ground_cover(&mut self) -> Option<f32> {
        if !(self.params.clouds.shadow_map && self.params.overcast_applies() && self.wants_clouds()) {
            return None;
        }
        self.layer_sun_cover()
    }

    /// The share of the sun this frame's layer takes from the ground on average
    /// (`crate::clouds::sun_cover_from_terms`), from the weather map's cached terms.
    fn layer_sun_cover(&mut self) -> Option<f32> {
        // The overcast measure fills `overcast_terms` for this weather map.
        let _ = self.layer_overcast();
        let settings = self.params.clouds;
        let (_, mean_cover, profile) = self
            .overcast_terms
            .filter(|(cached, _, _)| *cached == weather_key(&settings))?;
        Some(crate::clouds::sun_cover_from_terms(
            &settings,
            OvercastMapTerms { mean_cover, profile },
        ))
    }

    /// The share of the sun the GI probes' ray hits keep under this frame's cloud layer
    /// (ADR-5944 item 9): one minus the layer's mean cover of the sun ([`Self::layer_sun_cover`])
    /// on a frame whose surfaces read the cloud transmittance map for their sun term
    /// ([`Self::cloud_sun_params`], so after [`Self::plan_cloud_shadow_map`]), and exactly 1 on
    /// every other frame. The probe tracer lights a hit with the sun through its own shadow
    /// rays and does not read the map: without this the probes kept a sunlit ground's bounce
    /// under a closed cover. The mean, not the cover over each hit: under a broken layer the
    /// bounce is right on average and too even from place to place.
    pub(crate) fn probe_sun_share(&mut self) -> f32 {
        if self.cloud_sun_params()[3] < 0.5 {
            return 1.0;
        }
        let share = self
            .layer_sun_cover()
            .map_or(1.0, |cover| (1.0 - cover).clamp(0.0, 1.0));
        if share < PROBE_SUN_SHARE_FLOOR { 0.0 } else { share }
    }

    /// The layer's overcast factor without the `overcast_ibl` gate (the same measure and cache):
    /// above 0, the `aerial_from_tables` rain veil takes the deck (ADR-3294).
    ///
    /// The packed map depends on the quantized weather key only. Thickness and density change
    /// the scalar and leave the texels alone (ADR-5159).
    fn layer_overcast(&mut self) -> f32 {
        if self.params.clouds.coverage <= 0.0 {
            return 0.0;
        }
        let settings = self.params.clouds;
        let map_key = weather_key(&settings);
        let thickness = (settings.altitude_top - settings.altitude_bottom).max(0.0);
        // The same density the factor reads (`overcast_factor_from_terms`: `density.max(0.0)`,
        // so NaN and -inf are 0 and +inf is opaque): a key that folded +inf into 0 kept a
        // clear deck's cached factor for an opaque one.
        let density = settings.density.max(0.0);
        let scalar = map_key
            ^ u64::from(density.to_bits()).rotate_left(17)
            ^ u64::from(thickness.to_bits()).rotate_left(29);
        if let Some((cached, factor)) = self.overcast_cache
            && cached == scalar
        {
            return factor;
        }
        let terms = if let Some((cached, mean_cover, profile)) = self.overcast_terms
            && cached == map_key
        {
            OvercastMapTerms { mean_cover, profile }
        } else {
            let map = match self.weather_field() {
                Some(field) => field.weather_map(&settings),
                None => build_weather_map(&settings),
            };
            let terms = overcast_map_terms(&settings, &map);
            self.overcast_terms = Some((map_key, terms.mean_cover, terms.profile));
            terms
        };
        let factor = overcast_factor_from_terms(&settings, terms);
        self.overcast_cache = Some((scalar, factor));
        factor
    }

    /// Drops the froxel fog and cloud histories and restarts the cloud noise phase (camera
    /// cut). The next cloud frame marches every texel.
    pub(crate) fn reset_history(&mut self) {
        self.have_history = false;
        self.cloud_frame = 0;
        self.cloud_history = None;
        self.cloud_map.start = None;
    }

    /// Current resources.
    #[must_use]
    pub fn params(&self) -> &OutdoorParams {
        &self.params
    }

    /// The level sky body's planet-shine for a sun travelling along `sun_travel`:
    /// `[toward the body, tint (unit luminance) · lux]`, or `None` without a sky body or
    /// with no light. The illuminance is the body's authored `fill_lux`, else the physical
    /// phase-dependent value for the planet's sun (`SkyBody::planet_shine_lux`).
    #[must_use]
    pub fn fill_light(&self, sun_travel: [f32; 3]) -> Option<[[f32; 3]; 2]> {
        let body = self.params.sky_body?;
        let to_sun = [-sun_travel[0], -sun_travel[1], -sun_travel[2]];
        let lux = body.fill_illuminance(self.params.planet.sun_illuminance, to_sun);
        let d = body.direction;
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if lux.is_nan() || lux <= 0.0 || len < 1.0e-6 {
            return None;
        }
        let t = body.tint;
        let luma = (0.2126 * t[0] + 0.7152 * t[1] + 0.0722 * t[2]).max(1.0e-4);
        Some([
            [d[0] / len, d[1] / len, d[2] / len],
            [t[0] / luma * lux, t[1] / luma * lux, t[2] / luma * lux],
        ])
    }

    /// Directional light after a [`TimeOfDay`] override.
    ///
    /// The override runs only when `enabled` is set and a time resource is present.
    /// The atmosphere sun is still cached so the sky and IBL can follow an authored light.
    pub fn apply_sun(
        &mut self,
        enabled: bool,
        authored: DirectionalLightUniform,
        eye_y: f32,
    ) -> DirectionalLightUniform {
        let toward = if let Some(time) = self.params.time {
            sun_position(&time).direction
        } else {
            [
                -authored.direction[0],
                -authored.direction[1],
                -authored.direction[2],
            ]
        };
        // The sky always integrates the planet's sun strength (or the level's override):
        // scenes tuned against the fixed-brightness default sky keep their exposure when an
        // authored light is dimmer or brighter. An authored light only sets the direction.
        let planet = self.params.planet;
        if enabled {
            self.ensure_luts(&planet, toward, eye_y);
        }
        if !enabled || self.params.time.is_none() {
            return authored;
        }
        let Some(luts) = self.luts.as_ref() else {
            return authored;
        };
        let (color, lux) = crate::transmitted_sun(luts, &luts.params);
        let travel = [-toward[0], -toward[1], -toward[2]];
        // The authored light's specular disc (ADR-3190) survives the time-of-day override.
        DirectionalLightUniform::new(travel, color, lux).with_specular_disc(authored.specular_disc())
    }

    /// [`Self::apply_sun`] with the tables as they are: no rebuild for another eye height.
    /// A texture view (ADR-184) takes the sun the main view's tables transmit this frame.
    #[must_use]
    pub(crate) fn sun_from_tables(
        &self,
        enabled: bool,
        authored: DirectionalLightUniform,
    ) -> DirectionalLightUniform {
        let Some(time) = self.params.time.filter(|_| enabled) else {
            return authored;
        };
        let Some(luts) = self.luts.as_ref() else {
            return authored;
        };
        let toward = sun_position(&time).direction;
        let (color, lux) = crate::transmitted_sun(luts, &luts.params);
        DirectionalLightUniform::new([-toward[0], -toward[1], -toward[2]], color, lux)
            .with_specular_disc(authored.specular_disc())
    }

    /// Writes atmosphere irradiance and prefilter into the existing IBL textures.
    pub fn write_ibl(
        &mut self,
        queue: &crate::device::TrackedQueue,
        irradiance: &wgpu::Texture,
        prefilter: &wgpu::Texture,
    ) {
        let Some(luts) = self.luts.as_ref() else {
            return;
        };
        let overcast = self.overcast;
        let ground_cover = self.ground_cover;
        let key = ibl_key(luts.sun_direction, luts.camera_height_m, overcast, ground_cover);
        if let Some((ready_key, baked)) = self.ready_ibl.take() {
            write_f16_mip(queue, irradiance, 0, &baked.irradiance, 64, 32);
            for (level, (width, height, bytes)) in baked.prefilter.iter().enumerate() {
                write_f16_mip(queue, prefilter, level as u32, bytes, *width, *height);
            }
            self.ibl_key = Some(ready_key);
        }
        // A planet change rebuilds the tables, which clears `ibl_key`.
        if self.ibl_key == Some(key) {
            return;
        }
        if self.async_sky && self.ibl_key.is_some() {
            // Keep the current IBL; bake the new one off the frame.
            if self.sky_job.is_none()
                && let Some(base) = self.base.clone()
            {
                let luts = luts.clone();
                let _ = self.spawn_sky_job(None, &base, [0.0; 3], 0.0, Some(luts));
            }
            return;
        }
        let Some(luts) = self.luts.as_ref() else {
            return;
        };
        if self.ibl_key.is_some() {
            crate::frame_stats::note_inline_bake();
        }
        let baked = bake_ibl(luts, overcast, ground_cover);
        write_f16_mip(queue, irradiance, 0, &baked.irradiance, 64, 32);
        for (level, (width, height, bytes)) in baked.prefilter.iter().enumerate() {
            write_f16_mip(queue, prefilter, level as u32, bytes, *width, *height);
        }
        self.ibl_key = Some(key);
    }

    /// Sky, then clouds, then fog, onto the image the caller already shaded.
    ///
    /// `stamp(encoder, name, end)` brackets each pass that actually draws.
    /// `end` is false at the start and true after the draw. Skipped passes
    /// write no timestamps.
    #[tracing::instrument(skip_all)]
    pub(crate) fn composite(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        shared: &std::sync::Arc<crate::device_pipelines::DevicePipelines>,
        encoder: &mut wgpu::CommandEncoder,
        frame: &OutdoorComposite<'_>,
        mut stamp: impl FnMut(&mut wgpu::CommandEncoder, &'static str, bool),
    ) -> Result<(), RenderError> {
        self.ensure_gpu(device, shared)?;
        let clouds = frame.clouds && self.wants_clouds();
        // ADR-3294: the weather veil goes over the sky, the clouds and the ground, last of the
        // two, inside that pass's stamps.
        let veil = frame.atmosphere && self.veil_this_frame();
        if frame.atmosphere {
            stamp(encoder, "atmosphere", false);
            if let Some(sky) = frame.prepared_sky {
                sky.draw_aerial(encoder, frame.hdr);
            } else {
                self.draw_sky(device, queue, encoder, frame)?;
            }
            if veil && !clouds {
                self.draw_weather_veil(device, encoder, frame);
            }
            stamp(encoder, "atmosphere", true);
        }
        if clouds {
            stamp(encoder, "clouds", false);
            self.draw_clouds(device, queue, encoder, frame)?;
            if veil {
                self.draw_weather_veil(device, encoder, frame);
            }
            stamp(encoder, "clouds", true);
        }
        if frame.fog && self.wants_fog() {
            stamp(encoder, "fog", false);
            self.draw_fog(device, queue, encoder, frame, None)?;
            stamp(encoder, "fog", true);
        }
        Ok(())
    }

    /// Radiance (cd/m²) a ray escaping an interior sees, from the atmosphere tables:
    /// the sky above the horizon, the sunlit ground below it. `None` before the first
    /// LUT frame.
    pub(crate) fn environment_fn(&self) -> Option<impl Fn([f32; 3]) -> [f32; 3] + '_> {
        let luts = self.luts.as_ref()?;
        let ground = ground_radiance(luts);
        Some(move |dir: [f32; 3]| environment_radiance(luts, ground, dir))
    }

    /// Replaces the valley mist for subsequent frames (`None` removes it).
    pub fn set_valley_mist(&mut self, mist: Option<crate::ValleyMist>) {
        if self.mist != mist {
            self.have_history = false;
        }
        self.mist = mist;
    }

    /// Picks the mist's quality tier for this frame from the profile's `volumetric_fog` flag
    /// and reports whether the fog pass must run for the mist even though the profile has no
    /// volumetric fog (the Low, Deck and Mobile fallback).
    pub(crate) fn plan_mist(&mut self, volumetric_fog: bool) -> bool {
        self.mist_quality = self.mist.as_ref().map_or(crate::MistQuality::Off, |mist| {
            crate::mist_quality(mist, volumetric_fog)
        });
        !volumetric_fog && self.mist_quality == crate::MistQuality::Reduced
    }

    /// Height fog, a volume or the valley mist has positive density.
    pub(crate) fn wants_fog(&self) -> bool {
        self.mist_quality != crate::MistQuality::Off
            || self.params.fog.density > 0.0
            || self
                .params
                .volumes
                .iter()
                .any(|volume| volume_density_positive(*volume))
    }

    fn ensure_luts(&mut self, planet: &AtmosphereParameters, toward: [f32; 3], eye_y: f32) {
        let elev = (toward[1] * 1000.0).round() as i32;
        let height = (eye_y.max(0.0) * 0.1).round() as i32;
        let key = (elev, height);
        self.poll_sky_job(planet, Some((elev, eye_y)));
        let same_planet = self.lut_planet.as_ref() == Some(planet);
        if let Some(luts) = self.luts.as_mut()
            && self.lut_key == Some(key)
            && same_planet
        {
            luts.sun_direction = unit(toward);
            return;
        }
        // Height hysteresis: the key's height bucket is 10 m (decameters), but it is a grid,
        // not a drift bound — a camera that oscillates across one bucket edge (rolling
        // ground under a bobbing chase view) re-keys on every crossing, and each re-key is a
        // bake plus the LUT/IBL upload that lands on the frame thread. A rebuild is only
        // worth that once the camera has drifted [`SKY_HEIGHT_REBUILD_M`] from the height the
        // live tables were built for, which keeps the served staleness inside the envelope
        // the 10 m grid already serves. A held camera builds at its exact height, so still
        // frames and the goldens are unchanged (ADR-4908).
        if same_planet
            && self.lut_key.is_some_and(|built| built.0 == elev)
            && let Some(luts) = self.luts.as_ref()
            && (eye_y.max(0.0) - luts.camera_height_m).abs() <= SKY_HEIGHT_REBUILD_M
            && let Some(luts) = self.luts.as_mut()
        {
            luts.sun_direction = unit(toward);
            return;
        }
        let base = match self.base.as_ref() {
            Some(base) if base.params == *planet => std::sync::Arc::clone(base),
            _ => {
                let base = std::sync::Arc::new(crate::atmosphere::AtmosphereBase::new(planet));
                self.base = Some(std::sync::Arc::clone(&base));
                base
            }
        };
        // Streaming: the frame keeps the current tables (sun direction included) while a
        // worker integrates the new ones; the first tables and a planet change stay inline
        // so a still frame is the same either way. The sun direction must not move before
        // the worker installs its tables: the sky-view and aerial textures were integrated
        // for the old key, so pairing them with a new sun would last the worker's wall
        // time and differ machine to machine (audit-g-render3-determinism 2).
        if self.async_sky
            && same_planet
            && self.luts.is_some()
            && (self.sky_job.is_some() || self.spawn_sky_job(Some(key), &base, toward, eye_y, None))
        {
            return;
        }
        if same_planet && self.luts.is_some() {
            // A moved sun or camera rebuilt inside the frame (not a planet's first tables).
            crate::frame_stats::note_inline_bake();
        }
        self.sky_job = None;
        self.ready_ibl = None;
        self.luts = Some(crate::atmosphere::build_atmosphere_luts_from(
            &base,
            toward,
            eye_y.max(0.0),
        ));
        self.lut_key = Some(key);
        self.lut_planet = Some(*planet);
        self.lut_generation += 1;
        self.ibl_key = None;
    }

    /// Starts a worker that integrates the tables for `toward` / `eye_y` (when `lut_key` is
    /// set) or bakes the IBL of `ibl_from` (the current tables), then the IBL of the result.
    /// `false` when no thread could be started (the caller builds inline).
    fn spawn_sky_job(
        &mut self,
        lut_key: Option<(i32, i32)>,
        base: &std::sync::Arc<crate::atmosphere::AtmosphereBase>,
        toward: [f32; 3],
        eye_y: f32,
        ibl_from: Option<AtmosphereLuts>,
    ) -> bool {
        let base = std::sync::Arc::clone(base);
        let planet = base.params;
        let overcast = self.overcast;
        let ground_cover = self.ground_cover;
        // Registered (ADR-287) as CPU work: listed at exit, never waited for.
        let spawned = regia_platform::GpuWorkerBuilder::new("regia-sky")
            .driver_calls_marked()
            .spawn(move |_| {
                // Spare cycles only: never the render thread's core on a busy machine.
                let _ = crate::platform::lower_current_thread_priority();
                let luts = match ibl_from {
                    Some(luts) => luts,
                    None => crate::atmosphere::build_atmosphere_luts_from(&base, toward, eye_y.max(0.0)),
                };
                let ibl_key = ibl_key(luts.sun_direction, luts.camera_height_m, overcast, ground_cover);
                let ibl = bake_ibl(&luts, overcast, ground_cover);
                SkyJobResult {
                    luts: lut_key.map(|_| luts),
                    ibl_key,
                    ibl,
                }
            });
        match spawned {
            Ok(handle) => {
                self.sky_job = Some(SkyJob {
                    lut_key,
                    planet,
                    handle,
                });
                true
            }
            Err(err) => {
                tracing::warn!(target: "regia_render", "sky worker did not start ({err}); building inline");
                false
            }
        }
    }

    /// Installs a finished worker's tables and IBL (dropped when the planet changed since).
    ///
    /// `keep_live` is the frame's sun bucket and eye height. When it is set and the camera is
    /// still inside the hysteresis of the tables on screen, a table rebuild is dropped: the
    /// frame has already decided those tables stay, and the worker's bucket can match the
    /// bucket the camera is in now (ADR-4908). `None` (settle) always installs.
    fn poll_sky_job(&mut self, planet: &AtmosphereParameters, keep_live: Option<(i32, f32)>) {
        if !self.sky_job.as_ref().is_some_and(|job| job.handle.is_finished()) {
            return;
        }
        let Some(job) = self.sky_job.take() else { return };
        let Ok(result) = job.handle.join() else {
            tracing::warn!(target: "regia_render", "sky worker panicked; the next frame rebuilds");
            return;
        };
        if job.planet != *planet || self.lut_planet.as_ref() != Some(planet) {
            return;
        }
        if let Some((elev, eye_y)) = keep_live
            && job.lut_key.is_some()
            && self.lut_key.is_some_and(|built| built.0 == elev)
            && self
                .luts
                .as_ref()
                .is_some_and(|live| (eye_y.max(0.0) - live.camera_height_m).abs() <= SKY_HEIGHT_REBUILD_M)
        {
            return;
        }
        if let Some(luts) = result.luts {
            self.luts = Some(luts);
            self.lut_key = job.lut_key;
            self.lut_generation += 1;
        }
        self.ready_ibl = Some((result.ibl_key, result.ibl));
    }

    /// Integrate sun/height changes of the atmosphere tables and the sky IBL on a worker
    /// thread instead of inside the frame (~50 ms of CPU each, `specs/27` § 2.4). The first
    /// tables of a planet are still built inline, so a still frame does not change; a moving
    /// sun or camera shows the previous tables until the new ones land (a few frames).
    pub fn set_async_sky(&mut self, enabled: bool) {
        self.async_sky = enabled;
    }

    /// Whether sky rebuilds run on a worker ([`Self::set_async_sky`]).
    pub(crate) fn async_sky(&self) -> bool {
        self.async_sky
    }

    /// A sky worker is integrating tables or an IBL right now.
    pub(crate) fn sky_busy(&self) -> bool {
        self.sky_job.is_some()
    }

    /// Waits for a running sky worker and installs its result (tests and captures that
    /// need the settled sky; never on the frame path).
    pub fn settle_sky(&mut self) {
        if let Some(job) = self.sky_job.as_ref() {
            let planet = job.planet;
            while !self.sky_job.as_ref().is_some_and(|job| job.handle.is_finished()) {
                std::thread::yield_now();
            }
            self.poll_sky_job(&planet, None);
        }
    }

    /// `[sky, ground]` radiance of the current atmosphere tables — the two halves of the
    /// ambient bake (`ambient_irradiance_table` above the horizon, `ground_radiance`
    /// below) — for the terrain's hemisphere ambient. `None` before the tables exist.
    /// The ground is the bake's: the shadowed one when the cloud transmittance map shadows it
    /// under an overcast sky light (`ground_cover`, ADR-5944 item 6), else the sunlit one.
    pub(crate) fn hemisphere_ambient(&self) -> Option<[[f32; 3]; 2]> {
        let luts = self.luts.as_ref()?;
        let ground = match self.ground_cover {
            Some(cover) => ground_radiance_under(luts, self.overcast, cover),
            None => ground_radiance(luts),
        };
        Some([
            overcast_sky(crate::ambient_irradiance_table(luts), self.overcast),
            ground,
        ])
    }

    /// The planet has no air: no aerial perspective, no clouds.
    pub(crate) fn airless(&self) -> bool {
        self.params.planet.is_airless()
    }

    /// Creates the outdoor GPU state on the first call (the pipelines are built by the
    /// passes that use them, once per device in `shared`).
    pub(crate) fn ensure_gpu(
        &mut self,
        device: &wgpu::Device,
        shared: &std::sync::Arc<crate::device_pipelines::DevicePipelines>,
    ) -> Result<(), RenderError> {
        if self.gpu.is_some() {
            return Ok(());
        }
        self.gpu = Some(OutdoorGpu::new(device, std::sync::Arc::clone(shared))?);
        Ok(())
    }

    /// Frame warms: builds every pipeline the frame path's outdoor composite can draw into
    /// `format` (the frame's HDR target): the sky and aerial perspective, and the froxel fog
    /// and the clouds where the profile has them, whatever the current map shows. A map
    /// loaded after the warm with clouds or fog the warm frame's map did not have (DO27's
    /// second map, v0.1.11: `cached_pipe` on its first frame) compiles nothing then. Returns
    /// the pipelines asked for (built now or already on the device).
    pub(crate) fn prewarm(
        &mut self,
        device: &wgpu::Device,
        shared: &std::sync::Arc<crate::device_pipelines::DevicePipelines>,
        format: wgpu::TextureFormat,
        (fog, clouds): (bool, bool),
    ) -> Result<usize, RenderError> {
        self.ensure_gpu(device, shared)?;
        let gpu = self.gpu.as_mut().expect("outdoor gpu built above");
        let hdr = wgpu::TextureFormat::Rgba16Float;
        let mut asked = 0;
        // `draw_sky`: the sky (1) and, under air, the aerial perspective (2).
        for entry in [1, 2] {
            crate::pipeline_warm::warm_checkpoint()?;
            gpu.sky_pipe(device, format, entry);
            asked += 1;
        }
        // `aerial_from_tables` (ADR-3294): its aerial entry and the weather veil, only when the
        // switch is on.
        if self.table_air.is_on() {
            let (_, aerial) = self.table_air.sky_entries();
            for entry in [aerial, VEIL_ENTRY] {
                crate::pipeline_warm::warm_checkpoint()?;
                gpu.sky_pipe(device, format, entry);
                asked += 1;
            }
        }
        // `draw_fog`: scatter (0) and integrate (1) in the froxel format, the blend (2).
        if fog {
            for (target, entry) in [(hdr, 0), (hdr, 1), (format, 2)] {
                crate::pipeline_warm::warm_checkpoint()?;
                gpu.fog_pipe(device, target, entry);
                asked += 1;
            }
        }
        // `draw_clouds`: the shadow (3), the march (2) at reduced size, the composite (4), and with
        // the reprojected march (cloud reprojection, on by default) the fresh-texel pass (5) and the
        // reconstruct pass (6) in the froxel format.
        // With the cloud transmittance map (ADR-320) the map pass (7) as well.
        if clouds {
            for (target, entry) in [
                (format, 3),
                (hdr, 2),
                (format, 4),
                (hdr, 5),
                (hdr, 6),
                (CLOUD_MAP_FORMAT, 7),
            ] {
                crate::pipeline_warm::warm_checkpoint()?;
                gpu.cloud_pipe(device, target, entry);
                asked += 1;
            }
        }
        Ok(asked)
    }
}

/// Below this share of the sun the GI probes' ray hits get none
/// ([`OutdoorState::probe_sun_share`]): a closed deck's last thousandth is nothing a probe
/// shows, and a sun that is exactly dark does not trip the probes' light-cut test (a relative
/// change of the sun, `ddgi::light_cut`) on every frame while the cover settles.
const PROBE_SUN_SHARE_FLOOR: f32 = 1.0 / 1024.0;

/// A sky rebuild running on a worker thread ([`OutdoorState::set_async_sky`]).
struct SkyJob {
    /// The table key it integrates (`None`: an IBL bake of the current tables).
    lut_key: Option<(i32, i32)>,
    /// The planet it integrates for.
    planet: AtmosphereParameters,
    handle: std::thread::JoinHandle<SkyJobResult>,
}

/// What a [`SkyJob`] hands back.
struct SkyJobResult {
    luts: Option<AtmosphereLuts>,
    ibl_key: IblKey,
    ibl: BakedIbl,
}

/// The atmosphere's IBL as RGBA16F bytes (physical radiance over `SKY_IBL_SCALE`, not
/// clipped at 1).
struct BakedIbl {
    irradiance: Vec<u8>,
    /// `(width, height, bytes)` per mip.
    prefilter: Vec<(u32, u32, Vec<u8>)>,
}

/// Radiance toward `dir` for the ambient bake: the sky-view table above the horizon and a
/// Lambertian ground below it, lit by the transmitted sun and the sky (one bounce). An
/// airless moon's ambient is then the ground bounce alone, not a blue sky.
fn environment_radiance(luts: &AtmosphereLuts, ground: [f32; 3], dir: [f32; 3]) -> [f32; 3] {
    if dir[1] >= 0.0 {
        return sample_sky_table(luts, dir);
    }
    let sky = sample_sky_table(luts, dir);
    [sky[0] + ground[0], sky[1] + ground[1], sky[2] + ground[2]]
}

/// [`environment_radiance`] under a cloud layer's overcast factor (ADR-321): the sky part greys
/// and dims ([`overcast_sky`]); the ground bounce is kept. Factor 0 is the clear bake exactly.
fn overcast_environment(luts: &AtmosphereLuts, ground: [f32; 3], dir: [f32; 3], overcast: f32) -> [f32; 3] {
    if overcast <= 0.0 {
        return environment_radiance(luts, ground, dir);
    }
    let sky = overcast_sky(sample_sky_table(luts, dir), overcast);
    if dir[1] >= 0.0 {
        return sky;
    }
    [sky[0] + ground[0], sky[1] + ground[1], sky[2] + ground[2]]
}

/// Ground radiance (cd/m²) under the sun and sky of `luts`.
///
/// The same value [`crate::atmosphere::ground_radiance`] composites into a view ray that
/// meets the planet. This bake adds it unattenuated for every downward direction, on top
/// of the inscatter-only sky table, so the two must stay the same colour.
fn ground_radiance(luts: &AtmosphereLuts) -> [f32; 3] {
    crate::atmosphere::ground_radiance(luts)
}

/// Cosine-weighted samples per texel of the baked irradiance map.
const IBL_IRRADIANCE_SAMPLES: u32 = 64;

/// The ground the image light shows below the horizon under a cloud layer that takes `cover`
/// of the sun from it on average and puts the sky light under `overcast` (ADR-5944):
/// [`ground_radiance`] with the sun's share of its light times `1 − cover` and the sky's share
/// under the overcast rule. The terrain and the meshes on it are lit the same way through the
/// cloud transmittance map, so a wall or a tree no longer takes a sunlit ground's bounce from
/// ground that is in shadow.
fn ground_radiance_under(luts: &AtmosphereLuts, overcast: f32, cover: f32) -> [f32; 3] {
    let (sun, sky) = crate::atmosphere::ground_light(luts);
    let sky = overcast_sky(sky, overcast);
    let lit = 1.0 - cover.clamp(0.0, 1.0);
    std::array::from_fn(|c| luts.params.ground_albedo[c] * (sun[c] * lit + sky[c]))
}

/// `ground_cover`: the layer's mean cover of the ground when the cloud transmittance map
/// shadows it under an overcast sky light ([`ground_radiance_under`]); `None` bakes the sunlit
/// ground, bit for bit as before.
fn bake_ibl(luts: &AtmosphereLuts, overcast: f32, ground_cover: Option<f32>) -> BakedIbl {
    let scale = crate::SKY_IBL_SCALE;
    let ground = match ground_cover {
        Some(cover) => ground_radiance_under(luts, overcast, cover),
        None => ground_radiance(luts),
    };
    let irradiance = equirect_f16(64, 32, |dir| {
        let n = dir;
        let mut sum = [0.0; 3];
        // 64 cosine samples: with 16, faces toward a low sun over- or under-shot the dusty
        // circumsolar sky by ~20 % (Mars; within 5 % of a 4096-sample reference now).
        for i in 0..IBL_IRRADIANCE_SAMPLES {
            let local = cosine_hemisphere(hammersley(i, IBL_IRRADIANCE_SAMPLES));
            let world = to_world(local, n);
            let sky = overcast_environment(luts, ground, world, overcast);
            sum[0] += sky[0];
            sum[1] += sky[1];
            sum[2] += sky[2];
        }
        let k = 1.0 / (IBL_IRRADIANCE_SAMPLES as f32 * scale);
        [sum[0] * k, sum[1] * k, sum[2] * k]
    });
    let mut prefilter = Vec::new();
    let mut width = 64u32;
    let mut height = 32u32;
    for mip in 0..4u32 {
        let roughness = mip as f32 / 3.0;
        let image = equirect_f16(width, height, |dir| {
            let mut sum = [0.0; 3];
            let mut weight = 0.0;
            for i in 0..8u32 {
                let jitter = hammersley(i, 8);
                let cone = (roughness * jitter[1]).sin();
                let offset = [
                    cone * jitter[0].cos(),
                    cone * (1.0 - jitter[1]).cos(),
                    cone * jitter[0].sin(),
                ];
                let sample_dir = unit([dir[0] + offset[0], dir[1] + offset[1], dir[2] + offset[2]]);
                let sky = overcast_environment(luts, ground, sample_dir, overcast);
                sum[0] += sky[0];
                sum[1] += sky[1];
                sum[2] += sky[2];
                weight += 1.0;
            }
            [
                sum[0] / weight / scale,
                sum[1] / weight / scale,
                sum[2] / weight / scale,
            ]
        });
        prefilter.push((width, height, image));
        width = (width / 2).max(1);
        height = (height / 2).max(1);
    }
    BakedIbl {
        irradiance,
        prefilter,
    }
}

/// Equirectangular RGBA16F bytes of `f` (linear, unclipped).
fn equirect_f16(width: u32, height: u32, f: impl Fn([f32; 3]) -> [f32; 3]) -> Vec<u8> {
    let mut texels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let u = (x as f32 + 0.5) / width as f32;
            let v = (y as f32 + 0.5) / height as f32;
            texels.push(f(dir_from_equirect(u, v)));
        }
    }
    crate::ibl::rgb_to_rgba16f(&texels)
}

fn ibl_key(dir: [f32; 3], height: f32, overcast: f32, ground_cover: Option<f32>) -> IblKey {
    (
        (dir[0] * 200.0).round() as i32,
        (dir[1] * 200.0).round() as i32,
        (height * 0.1).round() as i32,
        (overcast * 100.0).round() as i32,
        // The layer's mean cover of the ground in percent; −1 when the map does not shadow it.
        ground_cover.map_or(-1, |cover| (cover * 100.0).round() as i32),
    )
}

impl OutdoorState {
    /// Drops the IBL cache so the next physical frame rewrites the sky.
    pub(crate) fn forget_ibl(&mut self) {
        self.ibl_key = None;
    }

    /// Background inputs not captured by the host's outdoor-parameter hash (ADR-7963).
    /// A worker can install new tables while the camera and requested sun stay still.
    pub(crate) fn sky_background_key(&self) -> (u64, [u32; 3], bool) {
        (
            self.lut_generation,
            self.luts
                .as_ref()
                .map_or([0; 3], |luts| luts.sun_direction.map(f32::to_bits)),
            self.star_flux,
        )
    }

    /// Coverage above zero, or a cirrus sheet.
    pub(crate) fn wants_clouds(&self) -> bool {
        // A cirrus sheet draws without volumetric coverage (ADR-1408).
        self.params.clouds.draws_anything() && !self.airless()
    }

    /// Sky-view LUT on far pixels, then aerial perspective on opaque pixels.
    pub(crate) fn draw_sky(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        frame: &OutdoorComposite<'_>,
    ) -> Result<(), RenderError> {
        if let Some(sky) = self.prepare_sky(
            device,
            queue,
            &SkyInputs {
                depth: frame.depth,
                width: frame.width,
                height: frame.height,
                format: frame.format,
                inv_view_proj: frame.inv_view_proj,
                eye: frame.eye,
                far: frame.far,
                pre_exposure: frame.pre_exposure,
                clouds: frame.clouds,
            },
        ) {
            draw_pass_seq(
                encoder,
                &[Some(&sky.background), sky.aerial.as_ref()],
                &sky.bind,
                frame.hdr,
                "atmosphere sky",
                false,
            );
        }
        Ok(())
    }

    /// Prepares the existing atmosphere once, after `ensure_gpu` and before forward recording.
    #[tracing::instrument(skip_all)]
    pub(crate) fn prepare_sky(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        frame: &SkyInputs<'_>,
    ) -> Option<SkyPass> {
        let generation = self.lut_generation;
        // ADR-3294: under a drawn deck the rain veil's in-scatter (sky and ground halves) is
        // greyed and dimmed by the deck (`overcast_sky`'s rule), and the deck blocks that share
        // of the sun. Rain falls from a covered column, so the deck is that column's opacity
        // (`rain_deck_opacity`), not the layer's mean overcast.
        let deck =
            if self.table_air.is_on() && frame.clouds && self.wants_clouds() && self.layer_overcast() > 0.0 {
                crate::horizon_seam::rain_deck_opacity(&self.params.clouds)
            } else {
                0.0
            };
        let luts = self.luts.as_ref()?;
        let body = self.params.sky_body;
        let airless = luts.params.is_airless();
        // ADR-5944: on a frame whose surfaces read the cloud transmittance map for their sun
        // term, the air in front of them reads it too. Zero (every other frame) leaves the
        // aerial entries as they were.
        let cloud = if airless {
            [0.0; 4]
        } else {
            cloud_shadow_params(
                &self.params.clouds,
                self.cloud_map_live && self.params.clouds.shadow_map,
            )
        };
        let cloud_air = cloud[3] > 0.5;
        self.sky_cloud_air = cloud_air;
        if cloud_air {
            self.cloud_air_frames += 1;
        }
        let overcast = self.overcast;
        // The clear ground light the air's multiple scattering follows under the layer.
        let ground_light = cloud_air.then(|| crate::atmosphere::ground_light(luts));
        let gpu = self.gpu.as_mut().expect("outdoor gpu");
        gpu.ensure_sky_luts(device, queue, luts, Some(generation));
        if cloud_air {
            gpu.ensure_aerial_sun_lut(device, queue, luts, Some(generation));
        }
        gpu.ensure_body_albedo(device, queue, self.params.sky_body_albedo.as_deref());
        gpu.ensure_moon_albedo(device, queue, &self.params.moon_albedo);
        let bytes = sky_uniform(
            Some(luts),
            body,
            &self.params.moons,
            self.params.sky_body_albedo.is_some(),
            [
                self.params.moon_albedo[0].is_some(),
                self.params.moon_albedo[1].is_some(),
            ],
            self.params.shows_planet_surface(),
            frame.inv_view_proj,
            frame.eye,
            frame.width,
            frame.height,
            SkyMode::Hdr {
                pre_exposure: frame.pre_exposure,
            },
        );
        let mut bytes = bytes;
        if self.star_flux {
            bytes.body_tex[3] = star_flux_gain(frame.height);
            bytes.stars[1] = pixel_angle_chord(frame.inv_view_proj, frame.height);
        }
        // ADR-3294: the table entries read the weather's extinction and the sky's rain reach.
        let table_air = if airless { TableAir::Off } else { self.table_air };
        if table_air.is_on() {
            bytes.table_air = [
                if table_air == TableAir::Lite { 2.0 } else { 1.0 },
                self.params.precip_extinction_per_m.max(0.0),
                frame.far.max(0.0),
                deck,
            ];
            let rest = crate::horizon_seam::rain_inscatter_constant(luts, deck);
            bytes.table_ground = [rest[0], rest[1], rest[2], 0.0];
            self.table_air_frames += 1;
        }
        // ADR-4757: the air inside an interior box takes no in-scatter (the fog's boxes).
        if !self.aerial_interiors_off {
            bytes.interiors = interior_slots(&self.params.interiors, frame.eye);
        }
        if let Some((sun, sky)) = ground_light {
            bytes.cloud = cloud;
            bytes.cloud_air = [overcast, 0.0, 0.0, 0.0];
            bytes.cloud_ground_sun = [sun[0], sun[1], sun[2], 0.0];
            bytes.cloud_ground_sky = [sky[0], sky[1], sky[2], 0.0];
        }
        let (sky_entry, aerial_entry) = table_air.sky_entries();
        queue.write_buffer(&gpu.sky_buf, 0, bytemuck::bytes_of(&bytes));
        let bg = gpu.sky_bind(device, frame.depth, cloud_air);
        let pipe = gpu.sky_pipe(device, frame.format, sky_entry);
        // No air, no aerial perspective: the far ice is as crisp as the near ice.
        let aerial = (!airless).then(|| gpu.sky_pipe(device, frame.format, aerial_entry));
        Some(SkyPass {
            bind: bg,
            background: pipe,
            aerial,
        })
    }

    /// Scatter, integrate, then composite. `ldr` tonemaps into an RGBA8 target.
    pub(crate) fn draw_fog(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        frame: &OutdoorComposite<'_>,
        ldr: Option<&wgpu::TextureView>,
    ) -> Result<(), RenderError> {
        let write_slot = if self.have_history {
            1 - self.history_slot
        } else {
            0
        };
        let history_slot = self.history_slot;
        let have_history = self.have_history;
        let prev_eye = self.prev_eye;
        let params = &self.params;
        // The valley mist: its rows for the tier planned this frame, its terrain grid, and no
        // history blend, so a frame is a function of its own inputs (the mist clock).
        let mist_rows = self
            .mist
            .as_ref()
            .map_or([[0.0; 4]; 4], |mist| mist.gpu_rows(self.mist_quality));
        let mist_grid = self
            .mist
            .as_ref()
            .filter(|_| self.mist_quality != crate::MistQuality::Off)
            .and_then(|mist| mist.grid.clone());
        let have_history = have_history && self.mist_quality == crate::MistQuality::Off;
        // Sky ambient from the atmosphere tables; a fixed dim sky when there are none (tests).
        let ambient = self
            .luts
            .as_ref()
            .map_or([60.0, 75.0, 100.0], crate::ambient_irradiance_table);
        // Under an overcast the sky the fog sees greys and dims like the IBL (ADR-321).
        let ambient = overcast_sky(ambient, self.overcast);
        // Sunlit air reads the cloud transmittance map: god rays through gaps (ADR-320).
        let fog_cloud =
            cloud_shadow_params(&params.clouds, self.cloud_map_live && params.clouds.fog_coupling);
        let gpu = self.gpu.as_mut().expect("outdoor gpu");
        gpu.ensure_fog_targets(device);
        gpu.ensure_mist_grid(device, queue, mist_grid.as_deref());
        let mut uniform = fog_uniform(frame, params, have_history, prev_eye, ambient);
        uniform.cloud = fog_cloud;
        uniform.mist = mist_rows;
        queue.write_buffer(&gpu.fog_buf, 0, bytemuck::bytes_of(&uniform));
        if let Some(probe) = &params.fog_shadow_probe {
            let cascade = fog_sun_cascade(frame);
            probe.publish(crate::fog::FogSunShadowFrame {
                rule: params.fog.sun_shadow,
                cascade_index: frame
                    .shadow
                    .filter(|_| cascade.is_some())
                    .and_then(|view| self.fog_shadow_views.iter().position(|v| v == view)),
                cascade_count: self.fog_shadow_views.len(),
                cascade,
                sun_travel: frame.sun_travel,
            });
        }
        let shadow_view = frame.shadow.unwrap_or(&gpu.dummy_depth.view).clone();
        let scatter = gpu.scatter.as_ref().expect("scatter").view.clone();
        let target = gpu.integrated[write_slot as usize]
            .as_ref()
            .expect("integrated")
            .view
            .clone();
        let dummy = gpu.dummy_rgba.view.clone();
        // The history atlas is the colour target on the first frame. Binding it
        // while it is attached fails validation, and the shader ignores it.
        let history = if have_history {
            gpu.integrated[history_slot as usize]
                .as_ref()
                .expect("history")
                .view
                .clone()
        } else {
            dummy.clone()
        };
        let depth = frame.depth.clone();
        let local_shadow = frame.local_shadow.unwrap_or(&gpu.dummy_depth.view).clone();
        let shadows = [&shadow_view, &local_shadow];
        let built_before = gpu.fog_binds.built();
        let bg_scatter = gpu.fog_bind(device, shadows, &dummy, &dummy, &history, &depth);
        let scatter_pipe = gpu.fog_pipe(device, wgpu::TextureFormat::Rgba16Float, 0);
        draw_pass(
            encoder,
            &scatter_pipe,
            &bg_scatter,
            &scatter,
            "froxel scatter",
            true,
        );
        let bg_int = gpu.fog_bind(device, shadows, &scatter, &dummy, &history, &depth);
        let integrate = gpu.fog_pipe(device, wgpu::TextureFormat::Rgba16Float, 1);
        draw_pass(encoder, &integrate, &bg_int, &target, "froxel integrate", true);
        if let Some(ldr_view) = ldr {
            let bg = gpu.fog_bind(device, shadows, &scatter, &target, &history, &depth);
            let pipe = gpu.fog_pipe(device, wgpu::TextureFormat::Rgba8Unorm, 3);
            draw_pass(encoder, &pipe, &bg, ldr_view, "volumetric fog ldr", true);
        } else {
            let bg = gpu.fog_bind(device, shadows, &scatter, &target, &history, &depth);
            let blend = gpu.fog_pipe(device, frame.format, 2);
            draw_pass(encoder, &blend, &bg, frame.hdr, "volumetric fog", false);
        }
        // A camera cut draws without the history (the stand-in bound, slot 0 written), and a cut
        // to another view range can sample another sun cascade (`fog_cascade_index`). A frame
        // that built a fog group (the warm-up, or a view replaced) builds those variants too, so
        // the first such cut only looks them up (ADR-2303). Other frames skip this.
        if gpu.fog_binds.built() != built_before
            && let [Some(a), Some(b)] = &gpu.integrated
        {
            let _span = tracing::trace_span!("fog_bind_variants").entered();
            let (a, b) = (a.view.clone(), b.view.clone());
            // (history, written slot): no history, then the two ping-pong frames.
            let mut slots = vec![(dummy.clone(), a.clone())];
            if self.mist_quality == crate::MistQuality::Off {
                slots.push((a.clone(), b.clone()));
                slots.push((b, a));
            }
            let mut cascades = self.fog_shadow_views.clone();
            if !cascades.contains(&shadow_view) {
                cascades.push(shadow_view.clone());
            }
            for cascade in &cascades {
                let pair = [cascade, &local_shadow];
                for (history, written) in &slots {
                    let _ = gpu.fog_bind(device, pair, &dummy, &dummy, history, &depth);
                    let _ = gpu.fog_bind(device, pair, &scatter, &dummy, history, &depth);
                    let _ = gpu.fog_bind(device, pair, &scatter, written, history, &depth);
                }
            }
        }
        self.history_slot = write_slot;
        self.prev_eye = frame.eye;
        self.have_history = true;
        Ok(())
    }

    /// Cloud shadow on opaque pixels, the reduced-resolution clouds, then a nearest-depth
    /// composite over the whole frame.
    ///
    /// With [`OutdoorComposite::cloud_reprojection`] and a history of the same size, the
    /// march covers one texel of every 4×4 block (Bayer order) and a reconstruct pass
    /// reprojects the rest. Otherwise (first frame, camera cut, resize, off) every texel
    /// is marched. Either way the result becomes the next frame's history.
    pub(crate) fn draw_clouds(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        frame: &OutdoorComposite<'_>,
    ) -> Result<(), RenderError> {
        let settings = self.params.clouds;
        let ambient = self.luts.as_ref().map(cloud_ambient);
        // The sun transmitted to the layer base and top, when the sun comes from the tables
        // (a time of day) rather than an authored light.
        let sun_at_layer = self
            .luts
            .as_ref()
            .filter(|_| self.params.time.is_some())
            .map(|luts| {
                let at = |height: f32| {
                    let (color, lux) = crate::atmosphere::transmitted_sun_at(luts, height);
                    [color[0] * lux, color[1] * lux, color[2] * lux]
                };
                (
                    at(settings.altitude_bottom),
                    at(settings.altitude_top),
                    at(settings.look.cirrus.sanitized().altitude_m),
                )
            });
        let planet_radius_m = self.params.planet.bottom_radius_km * 1000.0;
        // The cloud transmittance map darkens the sun term of meshes and terrain instead of
        // this pass's whole-pixel multiply and its 0.35 floor (ADR-320).
        let screen_shadow = !(settings.shadow_map && self.cloud_map_live);
        let debug_map = settings.shadow_map_debug && self.cloud_map_live;
        self.cloud_frame = self.cloud_frame.wrapping_add(1);
        let frame_index = self.cloud_frame % 64;
        let field = self.weather_field();
        let gpu = self.gpu.as_mut().expect("outdoor gpu");
        let weather = gpu.ensure_weather(device, queue, &settings, field.as_deref());
        gpu.ensure_cloud_noise(device, queue);
        let div = frame.cloud_march.divisor(settings.resolution_divisor);
        let low = [frame.width.div_ceil(div), frame.height.div_ceil(div)];
        let fresh_size = crate::clouds::cloud_fresh_extent(low);
        let targets = gpu.ensure_cloud_targets(device, low);
        let drift = settings.drift_m();
        let history = self
            .cloud_history
            .filter(|h| frame.cloud_reprojection && h.low == low);
        crate::clouds::count_cloud_work(|c| {
            if history.is_some() {
                c.view_reprojected += 1;
                c.view_texels += u64::from(fresh_size[0]) * u64::from(fresh_size[1]);
            } else {
                c.view_full_marches += 1;
                c.view_texels += u64::from(low[0]) * u64::from(low[1]);
            }
        });
        let write = history.map_or(0, |h| 1 - h.slot);
        let read = 1 - write;
        let out_color = targets.color[write].view.clone();
        let out_dist = targets.dist[write].view.clone();
        let prev_color = targets.color[read].view.clone();
        let prev_dist = targets.dist[read].view.clone();
        let fresh = targets.fresh.view.clone();
        let fresh_dist = targets.fresh_dist.view.clone();
        let mut bytes = cloud_uniform(frame, &settings, 0, planet_radius_m, ambient);
        bytes.camera[3] = frame_index as f32;
        bytes.hdr[1] = div as f32;
        bytes.target = [
            low[0] as f32,
            low[1] as f32,
            frame.width as f32,
            frame.height as f32,
        ];
        let sub = crate::clouds::cloud_bayer_offset(self.cloud_frame);
        bytes.temporal = [
            sub[0] as f32,
            sub[1] as f32,
            fresh_size[0] as f32,
            fresh_size[1] as f32,
        ];
        if let Some(h) = history {
            bytes.history_view_proj = h.view_proj;
            bytes.reproject = [
                drift[0] - h.drift[0],
                drift[1] - h.drift[1],
                1.0,
                cloud_history_weather_motion(&h.weather, settings.weather_sample()),
            ];
            bytes.stale = cloud_sub_staleness(&h.weather, settings.weather_sample(), self.cloud_frame);
        }
        if let Some((bottom, top, cirrus)) = sun_at_layer {
            bytes.sun_bottom[..3].copy_from_slice(&bottom);
            bytes.sun_top[..3].copy_from_slice(&top);
            bytes.cirrus_sun[..3].copy_from_slice(&cirrus);
        }
        // The sky pass uploaded this frame's aerial table only when it ran.
        let aerial = frame.atmosphere && self.luts.is_some() && gpu.aerial_lut.is_some();
        bytes.sun_bottom[3] = if aerial { 1.0 } else { 0.0 };
        queue.write_buffer(&gpu.cloud_buf, 0, bytemuck::bytes_of(&bytes));
        let dummy = gpu.cloud_placeholder();
        let bg_march = gpu.cloud_bind(device, &weather, frame.depth, [&dummy, &dummy, &dummy, &dummy]);
        if screen_shadow {
            let shadow = gpu.cloud_pipe(device, frame.format, 3);
            draw_pass(encoder, &shadow, &bg_march, frame.hdr, "cloud shadow", false);
        }
        if history.is_some() {
            let fresh_pipe = gpu.cloud_pipe(device, wgpu::TextureFormat::Rgba16Float, 5);
            draw_march(
                encoder,
                &fresh_pipe,
                &bg_march,
                &fresh,
                &fresh_dist,
                "cloud fresh march",
            );
            let bg_rebuild = gpu.cloud_bind(
                device,
                &weather,
                frame.depth,
                [&fresh, &fresh_dist, &prev_color, &prev_dist],
            );
            let rebuild = gpu.cloud_pipe(device, wgpu::TextureFormat::Rgba16Float, 6);
            draw_march(
                encoder,
                &rebuild,
                &bg_rebuild,
                &out_color,
                &out_dist,
                "cloud reconstruct",
            );
        } else {
            let march = gpu.cloud_pipe(device, wgpu::TextureFormat::Rgba16Float, 2);
            draw_march(encoder, &march, &bg_march, &out_color, &out_dist, "cloud march");
        }
        let bg_comp = gpu.cloud_bind(
            device,
            &weather,
            frame.depth,
            [&out_color, &out_dist, &dummy, &dummy],
        );
        let composite = gpu.cloud_pipe(device, frame.format, 4);
        draw_pass(
            encoder,
            &composite,
            &bg_comp,
            frame.hdr,
            "volumetric clouds",
            false,
        );
        // The `cloud_shadow_map` debug view over the composite (ADR-320).
        let debug_view = if debug_map {
            gpu.cloud_map.as_ref().map(|(_, tex)| tex.view.clone())
        } else {
            None
        };
        if let Some(map) = debug_view {
            let bg_debug = gpu.cloud_bind(device, &weather, frame.depth, [&map, &dummy, &dummy, &dummy]);
            let pipe = gpu.cloud_pipe(device, frame.format, 8);
            draw_pass(
                encoder,
                &pipe,
                &bg_debug,
                frame.hdr,
                "cloud shadow map debug",
                false,
            );
        }
        // A full march (no history) marched every texel with this frame's weather.
        let now = settings.weather_sample();
        let mut weather = history.map_or([now; CLOUD_REFRESH_FRAMES], |h| h.weather);
        weather.rotate_right(1);
        weather[0] = now;
        self.cloud_history = Some(CloudHistory {
            slot: write,
            view_proj: frame.unjittered_view_proj,
            drift,
            low,
            weather,
        });
        Ok(())
    }
}

impl OutdoorState {
    /// Plans this frame's cloud transmittance map pass (ADR-320) and uploads its uniform; the
    /// pass itself is recorded by [`Self::encode_cloud_shadow_map`] at the start of the frame
    /// encoder, before the forward pass reads the map. `allowed` is false on frames without the
    /// cloud march (texture views, MSAA, LDR, presets without clouds). Nothing is planned when the
    /// layer has no map switch on, no coverage, or a sun below `CLOUD_SHADOW_MIN_SUN_Y`.
    ///
    /// The map is marched in bands of rows, one per frame (`CLOUD_SHADOW_REFRESH_FRAMES`), plus
    /// the strips a moved window brings in. A camera cut, a new sun (quantised like the IBL), a
    /// change of any setting the content depends on, a resize or a wind-clock jump of a texel
    /// marches it whole.
    #[tracing::instrument(skip_all)]
    pub(crate) fn plan_cloud_shadow_map(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        shared: &std::sync::Arc<crate::device_pipelines::DevicePipelines>,
        frame: CloudMapFrame,
    ) -> Result<(), RenderError> {
        let CloudMapFrame {
            sun_travel,
            eye,
            cone_light,
            allowed,
        } = frame;
        self.pending_cloud_map = None;
        self.cloud_map_live = false;
        let settings = self.params.clouds;
        if !(settings.wants_shadow_map() && self.wants_clouds()) {
            self.cloud_map.start = None;
            return Ok(());
        }
        // A frame without the cloud march (a texture view between main frames, say) neither
        // marches nor forgets the map: the main view's next frame carries on with its bands.
        if !allowed {
            return Ok(());
        }
        let to_sun = unit([-sun_travel[0], -sun_travel[1], -sun_travel[2]]);
        let size = settings.shadow_map_texels();
        let texel_m = settings.shadow_map_extent() / size as f32;
        let Some(start) = cloud_shadow_window(eye, to_sun, settings.altitude_bottom, size, texel_m) else {
            self.cloud_map.start = None;
            return Ok(());
        };
        self.ensure_gpu(device, shared)?;
        let key = cloud_map_key(&settings, cone_light);
        let sun_key = (
            (to_sun[0] * 200.0).round() as i32,
            (to_sun[1] * 200.0).round() as i32,
            (to_sun[2] * 200.0).round() as i32,
        );
        let drift = settings.drift_m();
        let state = self.cloud_map;
        let jumped =
            (drift[0] - state.drift[0]).abs() > texel_m || (drift[1] - state.drift[1]).abs() > texel_m;
        let planet_radius_m = self.params.planet.bottom_radius_km * 1000.0;
        let field = self.weather_field();
        let gpu = self.gpu.as_mut().expect("outdoor gpu");
        let created = gpu.ensure_cloud_map(device, size);
        let prev = state.start.filter(|_| {
            !created && !jumped && state.key == key && state.sun_key == sun_key && state.size == size
        });
        let rects = cloud_shadow_refresh_rects(state.frame, size, prev, start);
        crate::clouds::count_cloud_work(|c| {
            if prev.is_none() {
                c.map_whole += 1;
            }
            c.map_texels += rects
                .iter()
                .map(|r| u64::from(r[2]) * u64::from(r[3]))
                .sum::<u64>();
        });
        self.cloud_map = CloudMapState {
            start: Some(start),
            frame: state.frame.wrapping_add(1),
            key,
            sun_key,
            drift,
            size,
        };
        let weather = gpu.ensure_weather(device, queue, &settings, field.as_deref());
        gpu.ensure_cloud_noise(device, queue);
        let uniform = cloud_map_uniform(
            &settings,
            planet_radius_m,
            to_sun,
            cone_light,
            start,
            size,
            texel_m,
        );
        queue.write_buffer(&gpu.cloud_map_buf, 0, bytemuck::bytes_of(&uniform));
        let bind = gpu.cloud_map_bind(device, &weather);
        let pipe = gpu.cloud_pipe(device, CLOUD_MAP_FORMAT, 7);
        let view = gpu
            .cloud_map
            .as_ref()
            .expect("cloud map created above")
            .1
            .view
            .clone();
        self.pending_cloud_map = Some(PendingCloudMap {
            pipe,
            bind,
            view,
            rects,
        });
        self.cloud_map_live = true;
        Ok(())
    }

    /// Records the map pass [`Self::plan_cloud_shadow_map`] planned (nothing when none was).
    pub(crate) fn encode_cloud_shadow_map(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let Some(pending) = self.pending_cloud_map.take() else {
            return;
        };
        let _span = tracing::info_span!("cloud_shadow_map", rects = pending.rects.len()).entered();
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("cloud shadow map"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &pending.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            ..Default::default()
        });
        pass.counted_set_pipeline(&pending.pipe);
        pass.counted_set_bind_group(0, &pending.bind, &[]);
        for [x, y, w, h] in pending.rects {
            if w == 0 || h == 0 {
                continue;
            }
            pass.set_scissor_rect(x, y, w, h);
            pass.counted_draw(0..3, 0..1);
        }
    }

    /// The cloud transmittance map, once a frame has created it (it stays bound, all-lit
    /// parameters switch it off). `None` before.
    pub(crate) fn cloud_shadow_view(&self) -> Option<&wgpu::TextureView> {
        self.gpu
            .as_ref()
            .and_then(|gpu| gpu.cloud_map.as_ref())
            .map(|(_, tex)| &tex.view)
    }

    /// Changes whenever [`Self::cloud_shadow_view`] does (bind groups holding it rebuild).
    pub(crate) fn cloud_shadow_generation(&self) -> u64 {
        self.gpu.as_ref().map_or(0, |gpu| gpu.cloud_map_generation)
    }

    /// The map parameters the sun term of meshes and terrain reads this frame
    /// (`cloud_shadow.wgsl`): zero unless `CloudSettings::shadow_map` is on and the map was
    /// planned this frame.
    pub(crate) fn cloud_sun_params(&self) -> [f32; 4] {
        cloud_shadow_params(
            &self.params.clouds,
            self.cloud_map_live && self.params.clouds.shadow_map,
        )
    }
}

/// Hash of what the cloud transmittance map's content depends on, except the wind (its drift is
/// checked frame to frame: a jump of a texel marches the whole map), the weather values a
/// transition moves (coverage, type, density: the band refresh follows them, and a weather cut
/// drops the window, ADR-429) and the three switches: a change marches the whole map.
fn cloud_map_key(settings: &CloudSettings, cone_light: bool) -> u64 {
    let pattern = weather_shape_key(settings);
    let words = [
        pattern as u32,
        (pattern >> 32) as u32,
        settings.altitude_bottom.to_bits(),
        settings.altitude_top.to_bits(),
        settings.shape_scale_m.to_bits(),
        settings.detail_scale_m.to_bits(),
        settings.shadow_map_texels(),
        settings.shadow_map_extent().to_bits(),
        u32::from(cone_light),
    ];
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The map pass's uniform: the layer, wind, noise and curl of the view march (the same density)
/// and the sun; `temporal` = (window start x, z in cells, map size, texel metres).
fn cloud_map_uniform(
    settings: &CloudSettings,
    planet_radius_m: f32,
    to_sun: [f32; 3],
    cone_light: bool,
    start: [i32; 2],
    size: u32,
    texel_m: f32,
) -> CloudGpu {
    let mut gpu: CloudGpu = bytemuck::Zeroable::zeroed();
    gpu.sun = [to_sun[0], to_sun[1], to_sun[2], 0.0];
    gpu.layer = [
        settings.altitude_bottom,
        settings.altitude_top.max(settings.altitude_bottom + 1.0),
        settings.density.max(0.0),
        settings.coverage.clamp(0.0, 1.0),
    ];
    gpu.wind = settings.wind_uniform();
    gpu.shape = [
        settings.shape_scale_m.max(1.0),
        settings.detail_scale_m.max(1.0),
        settings.cloud_type.clamp(0.0, 1.0),
        planet_radius_m.max(1000.0),
    ];
    gpu.draw = [
        7.0,
        crate::clouds::CLOUD_SHADOW_MAP_STEPS as f32,
        settings.map_resolution.max(1) as f32,
        0.0,
    ];
    gpu.temporal = [start[0] as f32, start[1] as f32, size as f32, texel_m];
    // The map marches the same density as the view (erosion and breakup, ADR-1408).
    gpu.look = settings.look.look_row();
    if cone_light {
        gpu.light = [
            CLOUD_CURL_STRENGTH * settings.detail_scale_m.max(1.0),
            CLOUD_DARK_EDGE,
            CLOUD_CURL_TILE * settings.detail_scale_m.max(1.0),
            CLOUD_CONE_SPAN,
        ];
    }
    gpu
}

/// Ambient radiance at the cloud tops (the sky dome) and bases (sky plus ground bounce).
fn cloud_ambient(luts: &AtmosphereLuts) -> ([f32; 3], [f32; 3]) {
    let sky = crate::ambient_irradiance_table(luts);
    let ground = ground_radiance(luts);
    let top = sky;
    let bottom = [
        0.5 * sky[0] + 0.5 * ground[0],
        0.5 * sky[1] + 0.5 * ground[1],
        0.5 * sky[2] + 0.5 * ground[2],
    ];
    (top, bottom)
}

fn fog_uniform(
    frame: &OutdoorComposite<'_>,
    params: &OutdoorParams,
    history: bool,
    prev_eye: [f32; 3],
    ambient: [f32; 3],
) -> FogGpu {
    let mut volumes = [[0.0; 4]; 8];
    for slot in 0..4 {
        volumes[slot * 2][3] = -1.0;
    }
    for (i, volume) in params.volumes.iter().take(4).enumerate() {
        match *volume {
            FogVolume::Box {
                center,
                extents,
                density,
            } => {
                volumes[i * 2] = [center[0], center[1], center[2], 0.0];
                volumes[i * 2 + 1] = [
                    extents[0].abs(),
                    extents[1].abs(),
                    extents[2].abs(),
                    density.max(0.0),
                ];
            }
            FogVolume::Sphere {
                center,
                radius,
                density,
            } => {
                volumes[i * 2] = [center[0], center[1], center[2], 1.0];
                volumes[i * 2 + 1] = [radius.max(0.0), 0.0, 0.0, density.max(0.0)];
            }
        }
    }
    let mut lights = [[0.0; 4]; 12];
    for (i, lamp) in params.lights.iter().take(crate::FOG_LIGHT_SLOTS).enumerate() {
        lights[i * 3] = [
            lamp.position[0],
            lamp.position[1],
            lamp.position[2],
            lamp.radius.max(0.0),
        ];
        lights[i * 3 + 1] = [
            lamp.color[0],
            lamp.color[1],
            lamp.color[2],
            lamp.intensity.max(0.0),
        ];
        lights[i * 3 + 2] = lamp.gpu_cone();
    }
    let fog = params.fog;
    let (albedo, anisotropy) = params.fog_medium();
    FogGpu {
        inv_view_proj: frame.inv_view_proj,
        light_vp: frame.light_vp,
        prev_view_proj: frame.prev_view_proj,
        camera: [frame.eye[0], frame.eye[1], frame.eye[2], frame.near.max(0.05)],
        prev_camera: [
            prev_eye[0],
            prev_eye[1],
            prev_eye[2],
            frame.far.max(frame.near + 0.1),
        ],
        sun_travel: [
            frame.sun_travel[0],
            frame.sun_travel[1],
            frame.sun_travel[2],
            frame.sun_illuminance.max(0.0),
        ],
        // The compare's depth bias: the rule's (ADR-3122; legacy 0.002 of the depth extent).
        sun_color: [
            frame.sun_color[0],
            frame.sun_color[1],
            frame.sun_color[2],
            fog_sun_cascade(frame).map_or(crate::fog::FOG_LEGACY_SHADOW_BIAS_NDC, |c| {
                c.bias_ndc(fog.sun_shadow, frame.sun_travel)
            }),
        ],
        // w: the sun-shadow rule (1 places samples in the air in front of a surface, ADR-3122).
        grid: [
            FROXEL_X as f32,
            FROXEL_Y as f32,
            FROXEL_Z as f32,
            fog.sun_shadow.gpu_flag(),
        ],
        media: [
            fog.density.max(0.0),
            fog.height_falloff.max(0.0),
            fog.base_height,
            anisotropy,
        ],
        albedo: [albedo[0], albedo[1], albedo[2], fog.history_weight],
        misc: [
            if frame.shadow_size > 0.0 { 1.0 } else { 0.0 },
            frame.shadow_size.max(1.0),
            if history { 1.0 } else { 0.0 },
            if frame.depth_is_scene { 1.0 } else { 0.0 },
        ],
        screen: [
            frame.width as f32,
            frame.height as f32,
            crate::post::exposure_from_ev100(11.5),
            frame.pre_exposure,
        ],
        volumes,
        lights,
        ambient: [ambient[0], ambient[1], ambient[2], 0.0],
        interiors: interior_slots(&params.interiors, frame.eye),
        cloud: [0.0; 4],
        light_shadow: light_shadow_rows(frame, params),
        mist: [[0.0; 4]; 4],
    }
}

/// The sun cascade the froxel fog reads this frame, `None` without sun shadows.
fn fog_sun_cascade(frame: &OutdoorComposite<'_>) -> Option<crate::fog::FogSunCascade> {
    (frame.shadow_size > 0.0)
        .then(|| crate::fog::FogSunCascade::new(frame.light_vp, frame.shadow_size as u32))
}

/// The fog uniform's shadow rows: each slot's tile from [`OutdoorComposite::fog_light_shadows`]
/// when the atlas is bound and the slot holds a light, else zero (unshadowed).
fn light_shadow_rows(frame: &OutdoorComposite<'_>, params: &OutdoorParams) -> [[f32; 4]; 24] {
    let mut rows = [[0.0; 4]; 24];
    if frame.local_shadow.is_none() {
        return rows;
    }
    let slots = params.lights.len().min(crate::FOG_LIGHT_SLOTS);
    for (slot, tile) in frame.fog_light_shadows.iter().enumerate().take(slots) {
        rows[slot * 6..slot * 6 + 6].copy_from_slice(tile);
    }
    rows
}

/// Centre (xyz, w = 1 when used) and half extents of up to four interior boxes: the ones
/// [`crate::aerial_interior_slots`] picks for the camera at `eye` (ADR-7661).
fn interior_slots(interiors: &[crate::InteriorVolume], eye: [f32; 3]) -> [[f32; 4]; 8] {
    let mut slots = [[0.0; 4]; 8];
    for (i, index) in crate::aerial_interior_slots(interiors, eye)
        .into_iter()
        .enumerate()
    {
        let Some(volume) = index.map(|index| &interiors[index]) else {
            continue;
        };
        if volume.half_extent.iter().any(|h| !h.is_finite() || *h <= 0.0)
            || volume.center.iter().any(|v| !v.is_finite())
        {
            continue;
        }
        slots[i * 2] = [volume.center[0], volume.center[1], volume.center[2], 1.0];
        slots[i * 2 + 1] = [
            volume.half_extent[0],
            volume.half_extent[1],
            volume.half_extent[2],
            0.0,
        ];
    }
    slots
}

/// View-march steps: `percent` of the layer's [`CloudSettings::steps`], 1..=`march.max_steps`
/// (96 on the real-time presets; the shader's loop bound is [`crate::CloudMarch::STEP_LIMIT`]).
/// Desktop Ultra takes 150 % (64 → 96) with the time the reprojection saves; Cinematic 300 %
/// (64 → 192, ADR-1403).
fn cloud_steps(steps: u32, percent: u32, march: crate::CloudMarch) -> u32 {
    march.steps(steps, percent)
}

fn cloud_uniform(
    frame: &OutdoorComposite<'_>,
    settings: &CloudSettings,
    mode: u32,
    planet_radius_m: f32,
    ambient: Option<([f32; 3], [f32; 3])>,
) -> CloudGpu {
    let toward = unit([-frame.sun_travel[0], -frame.sun_travel[1], -frame.sun_travel[2]]);
    let (top, bottom) = ambient.unwrap_or(([3000.0, 4500.0, 7000.0], [1500.0, 1700.0, 2000.0]));
    let lux = frame.sun_illuminance.max(0.0);
    let sun_rad = [
        frame.sun_color[0] * lux,
        frame.sun_color[1] * lux,
        frame.sun_color[2] * lux,
    ];
    CloudGpu {
        inv_view_proj: frame.inv_view_proj,
        sun: [toward[0], toward[1], toward[2], frame.sun_illuminance.max(0.0)],
        sun_color: [frame.sun_color[0], frame.sun_color[1], frame.sun_color[2], 0.5],
        layer: [
            settings.altitude_bottom,
            settings.altitude_top.max(settings.altitude_bottom + 1.0),
            settings.density.max(0.0),
            settings.coverage.clamp(0.0, 1.0),
        ],
        wind: settings.wind_uniform(),
        shape: [
            settings.shape_scale_m.max(1.0),
            settings.detail_scale_m.max(1.0),
            settings.cloud_type.clamp(0.0, 1.0),
            planet_radius_m.max(1000.0),
        ],
        camera: [frame.eye[0], frame.eye[1], frame.eye[2], 0.0],
        ambient_top: [top[0], top[1], top[2], CLOUD_FADE_M],
        ambient_bottom: [bottom[0], bottom[1], bottom[2], crate::clouds::CLOUD_MARCH_ALBEDO],
        draw: [
            mode as f32,
            cloud_steps(settings.steps, frame.cloud_step_percent, frame.cloud_march) as f32,
            settings.map_resolution.max(1) as f32,
            CLOUD_SHADOW_FLOOR,
        ],
        target: [
            frame.width as f32,
            frame.height as f32,
            frame.width as f32,
            frame.height as f32,
        ],
        // z = the cloud transmittance map's extent (its debug view, ADR-320); w = marches per
        // texel (ADR-1403; 1 = the single centred march).
        hdr: [
            frame.pre_exposure,
            frame.cloud_march.divisor(settings.resolution_divisor) as f32,
            settings.shadow_map_extent(),
            frame.cloud_march.sample_count() as f32,
        ],
        ray_from_clip: frame.unjittered_inv_view_proj,
        history_view_proj: frame.unjittered_view_proj,
        temporal: [0.0; 4],
        reproject: [0.0; 4],
        // Without the atmosphere tables the sun is the frame's (camera-height) sun at both
        // heights; `draw_clouds` replaces these with the sun at the layer's altitudes.
        sun_bottom: [sun_rad[0], sun_rad[1], sun_rad[2], 0.0],
        sun_top: [
            sun_rad[0],
            sun_rad[1],
            sun_rad[2],
            if frame.cloud_cone_light { 1.0 } else { 0.0 },
        ],
        light: if frame.cloud_cone_light {
            [
                CLOUD_CURL_STRENGTH * settings.detail_scale_m.max(1.0),
                CLOUD_DARK_EDGE,
                CLOUD_CURL_TILE * settings.detail_scale_m.max(1.0),
                CLOUD_CONE_SPAN,
            ]
        } else {
            [0.0; 4]
        },
        stale: [[0.0; 4]; 4],
        look: settings.look.look_row(),
        cirrus: settings.look.cirrus_row(),
        // `draw_clouds` replaces the colour with the sun at the sheet's altitude.
        cirrus_sun: [
            sun_rad[0],
            sun_rad[1],
            sun_rad[2],
            settings.look.cirrus.sanitized().stretch,
        ],
        cirrus_dir: crate::cloud_types::cirrus_direction_row(settings),
        cirrus_fiber: settings.look.cirrus_fiber_row(),
        cirrus_shear: settings.look.cirrus_shear_row(),
        cirrus_crisp: settings.look.cirrus_crisp_row(),
    }
}

fn volume_density_positive(volume: FogVolume) -> bool {
    match volume {
        FogVolume::Box { density, .. } | FogVolume::Sphere { density, .. } => density > 0.0,
    }
}

impl OutdoorGpu {
    fn new(
        device: &wgpu::Device,
        shared: std::sync::Arc<crate::device_pipelines::DevicePipelines>,
    ) -> Result<Self, RenderError> {
        let sky_module = module(
            device,
            include_str!("../shaders/outdoor_sky.wgsl"),
            "outdoor_sky.wgsl",
        )?;
        let fog_module = module(
            device,
            include_str!("../shaders/volumetric_fog.wgsl"),
            "volumetric_fog.wgsl",
        )?;
        let cloud_module = module(device, include_str!("../shaders/clouds.wgsl"), "clouds.wgsl")?;
        let sky_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("outdoor sky bgl"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::FRAGMENT | wgpu::ShaderStages::VERTEX),
                tex_entry(1, wgpu::TextureSampleType::Float { filterable: false }),
                tex_entry(2, wgpu::TextureSampleType::Float { filterable: false }),
                depth_entry(3),
                tex_entry(4, wgpu::TextureSampleType::Float { filterable: true }),
                samp_entry(5, wgpu::SamplerBindingType::Filtering),
                tex_entry(6, wgpu::TextureSampleType::Float { filterable: true }),
                tex_entry(7, wgpu::TextureSampleType::Float { filterable: true }),
                // Cloud transmittance map (ADR-320) for the air under the layer (ADR-5944), read
                // with `textureLoad`.
                tex_entry(8, wgpu::TextureSampleType::Float { filterable: false }),
                // The sun's single-scattering share of the aerial table (ADR-5944).
                tex_entry(9, wgpu::TextureSampleType::Float { filterable: false }),
            ],
        });
        let fog_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("outdoor fog bgl"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::FRAGMENT | wgpu::ShaderStages::VERTEX),
                depth_entry(1),
                tex_entry(2, wgpu::TextureSampleType::Float { filterable: false }),
                tex_entry(3, wgpu::TextureSampleType::Float { filterable: false }),
                tex_entry(4, wgpu::TextureSampleType::Float { filterable: false }),
                depth_entry(5),
                // Cloud transmittance map (ADR-320), read with `textureLoad`.
                tex_entry(6, wgpu::TextureSampleType::Float { filterable: false }),
                // The local-light shadow atlas (fog spot shafts, ADR-1365).
                depth_entry(7),
                // The valley mist's terrain grid (ADR-1394), read with `textureLoad`.
                tex_entry(8, wgpu::TextureSampleType::Float { filterable: false }),
            ],
        });
        let cloud_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("outdoor cloud bgl"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::FRAGMENT | wgpu::ShaderStages::VERTEX),
                tex_entry(1, wgpu::TextureSampleType::Float { filterable: true }),
                samp_entry(2, wgpu::SamplerBindingType::Filtering),
                depth_entry(3),
                tex3d_entry(4),
                tex3d_entry(5),
                tex_entry(6, wgpu::TextureSampleType::Float { filterable: true }),
                tex_entry(7, wgpu::TextureSampleType::Float { filterable: true }),
                tex_entry(8, wgpu::TextureSampleType::Float { filterable: true }),
                tex_entry(9, wgpu::TextureSampleType::Float { filterable: true }),
                tex_entry(10, wgpu::TextureSampleType::Float { filterable: false }),
                tex_entry(11, wgpu::TextureSampleType::Float { filterable: true }),
            ],
        });
        let sky_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("outdoor sky"),
            bind_group_layouts: &[Some(&sky_bgl)],
            immediate_size: 0,
        });
        let fog_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("outdoor fog"),
            bind_group_layouts: &[Some(&fog_bgl)],
            immediate_size: 0,
        });
        let cloud_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("outdoor cloud"),
            bind_group_layouts: &[Some(&cloud_bgl)],
            immediate_size: 0,
        });
        // Two slabs with a gap along X, above the fog box. Sunlight falls
        // through the gap; both sides stay in shadow. A half-plane would not.
        let y = 4.0_f32;
        let z0 = -20.0_f32;
        let z1 = 30.0_f32;
        let gap = 1.2_f32;
        let mut verts = [0.0_f32; 36];
        for (slot, (x0, x1)) in [(-20.0_f32, -gap), (gap, 20.0_f32)].into_iter().enumerate() {
            let tri = [x0, y, z0, x1, y, z0, x1, y, z1, x0, y, z0, x1, y, z1, x0, y, z1];
            let start = slot * 18;
            verts[start..start + 18].copy_from_slice(&tri);
        }
        let occluder_vb = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fog occluder"),
            size: std::mem::size_of_val(&verts) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: true,
        });
        {
            let mut mapped = occluder_vb
                .slice(..)
                .get_mapped_range_mut()
                .expect("occluder buffer was created mapped");
            mapped.copy_from_slice(bytemuck::bytes_of(&verts));
        }
        occluder_vb.unmap();
        Ok(Self {
            sky_binds: crate::bind_cache::BindCache::default(),
            fog_binds: crate::bind_cache::BindCache::default(),
            cloud_binds: crate::bind_cache::BindCache::default(),
            cloud_map_binds: crate::bind_cache::BindCache::default(),
            sky_bgl,
            sky_layout,
            sky_module,
            sky_pipes: HashMap::new(),
            fog_bgl,
            fog_layout,
            fog_module,
            fog_pipes: HashMap::new(),
            cloud_bgl,
            cloud_layout,
            cloud_module,
            cloud_pipes: HashMap::new(),
            shared,
            occluder: None,
            sky_buf: uniform_buffer(device, "sky", std::mem::size_of::<SkyGpu>()),
            fog_buf: uniform_buffer(device, "fog", std::mem::size_of::<FogGpu>()),
            cloud_buf: uniform_buffer(device, "cloud", std::mem::size_of::<CloudGpu>()),
            cloud_map_buf: uniform_buffer(device, "cloud shadow map", std::mem::size_of::<CloudGpu>()),
            cloud_map: None,
            cloud_map_generation: 0,
            linear: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("outdoor linear repeat"),
                address_mode_u: wgpu::AddressMode::Repeat,
                address_mode_v: wgpu::AddressMode::Repeat,
                address_mode_w: wgpu::AddressMode::Repeat,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            shape_noise: None,
            detail_noise: None,
            curl_noise: None,
            cloud_targets: None,
            cloud_dummy: rgba_tex(
                device,
                1,
                1,
                wgpu::TextureFormat::Rgba16Float,
                wgpu::TextureUsages::TEXTURE_BINDING,
                "cloud dummy",
            ),
            dummy_depth: depth_tex(device, 1, 1, "dummy depth"),
            dummy_rgba: rgba_tex(
                device,
                1,
                1,
                wgpu::TextureFormat::Rgba32Float,
                wgpu::TextureUsages::TEXTURE_BINDING,
                "dummy rgba",
            ),
            body_albedo: None,
            moon_albedo: [None, None],
            // Bound when the body has no map; the shader then does not sample it.
            white_srgb: rgba_tex(
                device,
                1,
                1,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                wgpu::TextureUsages::TEXTURE_BINDING,
                "sky body placeholder",
            ),
            sky_lut: None,
            aerial_lut: None,
            aerial_sun_lut: None,
            lut_generation: None,
            scatter: None,
            integrated: [None, None],
            shadow: None,
            weather: None,
            mist_grid: None,
            occluder_vb,
        })
    }

    /// Uploads the sky-view and aerial tables unless the textures already hold `generation`
    /// (`None` = always upload).
    fn ensure_sky_luts(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        luts: &AtmosphereLuts,
        generation: Option<u64>,
    ) {
        if generation.is_some()
            && self.lut_generation == generation
            && self.sky_lut.is_some()
            && self.aerial_lut.is_some()
        {
            return;
        }
        self.lut_generation = generation;
        self.sky_lut = Some(upload_lut(
            device,
            queue,
            self.sky_lut.take(),
            &luts.sky_view,
            "sky view lut",
        ));
        self.aerial_lut = Some(upload_lut(
            device,
            queue,
            self.aerial_lut.take(),
            &luts.aerial,
            "aerial lut",
        ));
    }

    /// Uploads the sun's share of the aerial table (ADR-5944) unless the texture already holds
    /// `generation`. Called only on frames whose aerial composite reads the cloud map, so a
    /// frame without the map uploads nothing more than before.
    fn ensure_aerial_sun_lut(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        luts: &AtmosphereLuts,
        generation: Option<u64>,
    ) {
        if generation.is_some()
            && self
                .aerial_sun_lut
                .as_ref()
                .is_some_and(|(held, _)| *held == generation)
        {
            return;
        }
        let _span = tracing::info_span!("aerial_sun_lut_upload").entered();
        let reuse = self.aerial_sun_lut.take().map(|(_, tex)| tex);
        let tex = upload_lut(device, queue, reuse, &luts.aerial_sun, "aerial sun lut");
        self.aerial_sun_lut = Some((generation, tex));
    }

    fn ensure_fog_targets(&mut self, device: &wgpu::Device) {
        let width = FROXEL_X * FROXEL_Z;
        if self.scatter.is_none() {
            self.scatter = Some(rgba_tex(
                device,
                width,
                FROXEL_Y,
                wgpu::TextureFormat::Rgba16Float,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                "froxel scatter",
            ));
        }
        for slot in &mut self.integrated {
            if slot.is_none() {
                *slot = Some(rgba_tex(
                    device,
                    width,
                    FROXEL_Y,
                    wgpu::TextureFormat::Rgba16Float,
                    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                    "froxel integrated",
                ));
            }
        }
    }

    fn ensure_shadow(&mut self, device: &wgpu::Device) -> wgpu::TextureView {
        if self.shadow.is_none() {
            self.shadow = Some(depth_tex(device, SHADOW_SIZE, SHADOW_SIZE, "fog shaft shadow"));
        }
        self.shadow.as_ref().expect("fog shadow").view.clone()
    }

    /// The weather map texture for `settings`, rebuilt when its key changes. A map of the same
    /// resolution is written into the existing texture (ADR-429: a weather transition creates no
    /// texture and no bind group); `field` is the cached noise field, if any.
    fn ensure_weather(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        settings: &CloudSettings,
        field: Option<&crate::clouds::WeatherField>,
    ) -> wgpu::TextureView {
        let key = weather_key(settings);
        let stale = self.weather.as_ref().is_none_or(|(cached, _)| *cached != key);
        if stale {
            let _span = tracing::info_span!("cloud_weather_map").entered();
            let _crumb = crate::breadcrumbs::frame_pass("cloud_weather_map");
            let map = match field {
                Some(field) => field.weather_map(settings),
                None => build_weather_map(settings),
            };
            let reuse = self.weather.take().filter(|(_, tex)| {
                tex.texture.width() == map.resolution && tex.texture.height() == map.resolution
            });
            let tex = if let Some((_, tex)) = reuse {
                crate::clouds::count_cloud_work(|c| c.weather_uploads += 1);
                tex
            } else {
                crate::clouds::count_cloud_work(|c| c.weather_textures += 1);
                rgba_tex(
                    device,
                    map.resolution,
                    map.resolution,
                    wgpu::TextureFormat::Rgba8Unorm,
                    wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    "weather",
                )
            };
            write_rgba8(queue, &tex.texture, &map.rgba, map.resolution, map.resolution);
            self.weather = Some((key, tex));
        }
        self.weather.as_ref().expect("weather").1.view.clone()
    }

    /// Uploads the valley mist's terrain grid when its content changed (`None` drops it).
    fn ensure_mist_grid(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        grid: Option<&crate::MistHeightGrid>,
    ) {
        let key = grid.map(crate::MistHeightGrid::key);
        if self.mist_grid.as_ref().map(|(have, _)| *have) == key {
            return;
        }
        // Fog groups hold the old view.
        self.fog_binds.clear();
        self.mist_grid = grid.map(|grid| {
            let _span = tracing::info_span!("mist_grid_upload").entered();
            let tex = rgba_tex(
                device,
                grid.width(),
                grid.depth(),
                wgpu::TextureFormat::Rg32Float,
                wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                "valley mist grid",
            );
            write_f16_mip(
                queue,
                &tex.texture,
                0,
                bytemuck::cast_slice(grid.texels()),
                grid.width(),
                grid.depth(),
            );
            (grid.key(), tex)
        });
    }

    /// `cloud_air`: this frame's aerial entries read the cloud transmittance map and the sun's
    /// share of the aerial table (ADR-5944).
    fn sky_bind(
        &mut self,
        device: &wgpu::Device,
        depth: &wgpu::TextureView,
        cloud_air: bool,
    ) -> wgpu::BindGroup {
        let sky = self
            .sky_lut
            .as_ref()
            .map_or(&self.dummy_rgba.view, |tex| &tex.view)
            .clone();
        let aerial = self
            .aerial_lut
            .as_ref()
            .map_or(&self.dummy_rgba.view, |tex| &tex.view)
            .clone();
        let body = self
            .body_albedo
            .as_ref()
            .map_or(&self.white_srgb.view, |(_, tex)| &tex.view)
            .clone();
        let moon0 = self.moon_albedo[0]
            .as_ref()
            .map_or(&self.white_srgb.view, |(_, tex)| &tex.view)
            .clone();
        let moon1 = self.moon_albedo[1]
            .as_ref()
            .map_or(&self.white_srgb.view, |(_, tex)| &tex.view)
            .clone();
        // The cloud transmittance map and the sun's share of the aerial table when the air
        // reads them (ADR-5944); the stand-in otherwise, so a frame without the map binds what
        // it always did plus two 1×1 views the shader never loads.
        let (cloud, aerial_sun) = if cloud_air {
            (
                self.cloud_map
                    .as_ref()
                    .map_or(&self.dummy_rgba.view, |(_, tex)| &tex.view)
                    .clone(),
                self.aerial_sun_lut
                    .as_ref()
                    .map_or(&self.dummy_rgba.view, |(_, tex)| &tex.view)
                    .clone(),
            )
        } else {
            (self.dummy_rgba.view.clone(), self.dummy_rgba.view.clone())
        };
        let key = (
            sky.clone(),
            aerial.clone(),
            depth.clone(),
            body.clone(),
            moon0.clone(),
            moon1.clone(),
            cloud.clone(),
            aerial_sun.clone(),
        );
        let (layout, buf, linear) = (&self.sky_bgl, &self.sky_buf, &self.linear);
        self.sky_binds.get_or_insert_with(key, || {
            crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("outdoor sky"),
                layout,
                entries: &[
                    buf_bind(0, buf),
                    tex_bind(1, &sky),
                    tex_bind(2, &aerial),
                    depth_bind(3, depth),
                    tex_bind(4, &body),
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: wgpu::BindingResource::Sampler(linear),
                    },
                    tex_bind(6, &moon0),
                    tex_bind(7, &moon1),
                    tex_bind(8, &cloud),
                    tex_bind(9, &aerial_sun),
                ],
            })
        })
    }

    /// Uploads the sky body's albedo map when it changed; drops it when it is gone.
    fn ensure_body_albedo(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        texture: Option<&SkyBodyTexture>,
    ) {
        match texture {
            Some(texture) => upload_albedo(device, queue, &mut self.body_albedo, texture, "sky body albedo"),
            None => self.body_albedo = None,
        }
    }

    /// Uploads each moon's albedo map when it changed; drops a slot when it is gone.
    fn ensure_moon_albedo(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        textures: &[Option<std::sync::Arc<SkyBodyTexture>>; 2],
    ) {
        for (slot, texture) in self.moon_albedo.iter_mut().zip(textures) {
            match texture {
                Some(texture) => upload_albedo(device, queue, slot, texture, "sky moon albedo"),
                None => *slot = None,
            }
        }
    }

    fn sky_pipe(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        entry: u32,
    ) -> wgpu::RenderPipeline {
        cached_pipe(
            &mut self.sky_pipes,
            &self.shared,
            device,
            (&self.sky_layout, &self.sky_module),
            (format, entry),
            "outdoor sky",
        )
    }

    /// `shadows` is the sun cascade and the local-light shadow atlas.
    fn fog_bind(
        &mut self,
        device: &wgpu::Device,
        shadows: [&wgpu::TextureView; 2],
        scatter: &wgpu::TextureView,
        integrated: &wgpu::TextureView,
        history: &wgpu::TextureView,
        depth: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let [shadow, local_shadow] = shadows;
        // The cloud transmittance map when it exists (the uniform says whether to read it).
        let cloud = self
            .cloud_map
            .as_ref()
            .map_or(&self.dummy_rgba.view, |(_, tex)| &tex.view)
            .clone();
        // The valley mist's terrain grid when one is uploaded (the uniform says whether to read it).
        let mist = self
            .mist_grid
            .as_ref()
            .map_or(&self.dummy_rgba.view, |(_, tex)| &tex.view)
            .clone();
        let key = (
            shadow.clone(),
            scatter.clone(),
            integrated.clone(),
            history.clone(),
            depth.clone(),
            cloud.clone(),
            local_shadow.clone(),
            mist.clone(),
        );
        let (layout, buf) = (&self.fog_bgl, &self.fog_buf);
        self.fog_binds.get_or_insert_with(key, || {
            crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("outdoor fog"),
                layout,
                entries: &[
                    buf_bind(0, buf),
                    depth_bind(1, shadow),
                    tex_bind(2, scatter),
                    tex_bind(3, integrated),
                    tex_bind(4, history),
                    depth_bind(5, depth),
                    tex_bind(6, &cloud),
                    depth_bind(7, local_shadow),
                    tex_bind(8, &mist),
                ],
            })
        })
    }

    fn fog_pipe(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        entry: u32,
    ) -> wgpu::RenderPipeline {
        cached_pipe(
            &mut self.fog_pipes,
            &self.shared,
            device,
            (&self.fog_layout, &self.fog_module),
            (format, entry),
            "outdoor fog",
        )
    }

    /// `views` are the cloud colour and distance (bindings 6, 7) and the history colour and
    /// distance (8, 9).
    fn cloud_bind(
        &mut self,
        device: &wgpu::Device,
        weather: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        views: [&wgpu::TextureView; 4],
    ) -> wgpu::BindGroup {
        let [clouds, distance, history, history_dist] = views;
        let shape = self.shape_noise.as_ref().expect("cloud noise").view.clone();
        let detail = self.detail_noise.as_ref().expect("cloud noise").view.clone();
        let curl = self.curl_noise.as_ref().expect("cloud noise").view.clone();
        // The aerial table the sky pass uploaded this frame, or the 1×1 stand-in.
        let aerial = self
            .aerial_lut
            .as_ref()
            .map_or(&self.dummy_rgba.view, |tex| &tex.view)
            .clone();
        let key = (
            weather.clone(),
            depth.clone(),
            clouds.clone(),
            shape.clone(),
            detail.clone(),
            distance.clone(),
            history.clone(),
            history_dist.clone(),
            aerial.clone(),
            curl.clone(),
        );
        let (layout, buf, linear) = (&self.cloud_bgl, &self.cloud_buf, &self.linear);
        self.cloud_binds.get_or_insert_with(key, || {
            crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("outdoor clouds"),
                layout,
                entries: &[
                    buf_bind(0, buf),
                    tex_bind(1, weather),
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(linear),
                    },
                    depth_bind(3, depth),
                    tex_bind(4, &shape),
                    tex_bind(5, &detail),
                    tex_bind(6, clouds),
                    tex_bind(7, distance),
                    tex_bind(8, history),
                    tex_bind(9, history_dist),
                    tex_bind(10, &aerial),
                    tex_bind(11, &curl),
                ],
            })
        })
    }

    /// Uploads the shared 3D noise volumes and the curl field on first use.
    fn ensure_cloud_noise(&mut self, device: &wgpu::Device, queue: &crate::device::TrackedQueue) {
        if self.shape_noise.is_some() && self.detail_noise.is_some() && self.curl_noise.is_some() {
            return;
        }
        let n = crate::clouds::CURL_SIZE;
        let curl = rgba_tex(
            device,
            n,
            n,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            "cloud curl noise",
        );
        write_rgba8(queue, &curl.texture, crate::clouds::cloud_curl(), n, n);
        self.curl_noise = Some(curl);
        let noise = cloud_noise();
        self.shape_noise = Some(volume_tex(
            device,
            queue,
            crate::clouds::SHAPE_NOISE_SIZE,
            &noise.shape,
            "cloud shape noise",
        ));
        self.detail_noise = Some(volume_tex(
            device,
            queue,
            crate::clouds::DETAIL_NOISE_SIZE,
            &noise.detail,
            "cloud detail noise",
        ));
    }

    /// The cloud history pair and fresh target for a `low` march size, recreated (and the
    /// history dropped by the caller's size check) when the size changes.
    fn ensure_cloud_targets(&mut self, device: &wgpu::Device, low: [u32; 2]) -> &CloudTargets {
        if self.cloud_targets.as_ref().is_some_and(|t| t.low != low) {
            self.cloud_targets = None;
        }
        self.cloud_targets.get_or_insert_with(|| {
            let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
            let fresh_size = crate::clouds::cloud_fresh_extent(low);
            let color = |label| {
                rgba_tex(
                    device,
                    low[0],
                    low[1],
                    wgpu::TextureFormat::Rgba16Float,
                    usage,
                    label,
                )
            };
            let dist = |label| rgba_tex(device, low[0], low[1], CLOUD_DIST_FORMAT, usage, label);
            CloudTargets {
                low,
                color: [color("cloud history 0"), color("cloud history 1")],
                dist: [dist("cloud distance 0"), dist("cloud distance 1")],
                fresh: rgba_tex(
                    device,
                    fresh_size[0],
                    fresh_size[1],
                    wgpu::TextureFormat::Rgba16Float,
                    usage,
                    "cloud fresh",
                ),
                fresh_dist: rgba_tex(
                    device,
                    fresh_size[0],
                    fresh_size[1],
                    CLOUD_DIST_FORMAT,
                    usage,
                    "cloud fresh distance",
                ),
            }
        })
    }

    /// The cloud transmittance map at `size²` texels (ADR-320). `true` when it was (re)created:
    /// its content is undefined until marched whole.
    fn ensure_cloud_map(&mut self, device: &wgpu::Device, size: u32) -> bool {
        if self.cloud_map.as_ref().is_some_and(|(have, _)| *have == size) {
            return false;
        }
        let tex = rgba_tex(
            device,
            size,
            size,
            CLOUD_MAP_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            "cloud shadow map",
        );
        self.cloud_map = Some((size, tex));
        self.cloud_map_generation += 1;
        // Fog groups hold the old view.
        self.fog_binds.clear();
        true
    }

    /// The map pass's group: the cloud layout with the map pass's own uniform buffer.
    fn cloud_map_bind(&mut self, device: &wgpu::Device, weather: &wgpu::TextureView) -> wgpu::BindGroup {
        let shape = self.shape_noise.as_ref().expect("cloud noise").view.clone();
        let detail = self.detail_noise.as_ref().expect("cloud noise").view.clone();
        let curl = self.curl_noise.as_ref().expect("cloud noise").view.clone();
        let dummy = self.cloud_dummy.view.clone();
        let depth = self.dummy_depth.view.clone();
        let aerial = self.dummy_rgba.view.clone();
        let key = (
            weather.clone(),
            depth.clone(),
            dummy.clone(),
            shape.clone(),
            detail.clone(),
            dummy.clone(),
            dummy.clone(),
            dummy.clone(),
            aerial.clone(),
            curl.clone(),
        );
        let (layout, buf, linear) = (&self.cloud_bgl, &self.cloud_map_buf, &self.linear);
        self.cloud_map_binds.get_or_insert_with(key, || {
            crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("cloud shadow map"),
                layout,
                entries: &[
                    buf_bind(0, buf),
                    tex_bind(1, weather),
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(linear),
                    },
                    depth_bind(3, &depth),
                    tex_bind(4, &shape),
                    tex_bind(5, &detail),
                    tex_bind(6, &dummy),
                    tex_bind(7, &dummy),
                    tex_bind(8, &dummy),
                    tex_bind(9, &dummy),
                    tex_bind(10, &aerial),
                    tex_bind(11, &curl),
                ],
            })
        })
    }

    /// The cached 1×1 stand-in for the march colour and distance while they are attachments.
    fn cloud_placeholder(&self) -> wgpu::TextureView {
        self.cloud_dummy.view.clone()
    }

    fn cloud_pipe(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        entry: u32,
    ) -> wgpu::RenderPipeline {
        cached_pipe(
            &mut self.cloud_pipes,
            &self.shared,
            device,
            (&self.cloud_layout, &self.cloud_module),
            (format, entry),
            "outdoor clouds",
        )
    }

    fn draw_occluder(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        shadow: &wgpu::TextureView,
    ) {
        let (depth, rgba) = (self.dummy_depth.view.clone(), self.dummy_rgba.view.clone());
        let bg = self.fog_bind(device, [&depth, &depth], &rgba, &rgba, &rgba, &depth);
        let occluder = match &self.occluder {
            Some(pipe) => pipe.clone(),
            None => {
                let pipe = occluder_pipeline(device, &self.fog_layout, &self.fog_module);
                self.occluder = Some(pipe.clone());
                pipe
            }
        };
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("fog occluder"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: shadow,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            ..Default::default()
        });
        pass.counted_set_pipeline(&occluder);
        pass.counted_set_bind_group(0, &bg, &[]);
        pass.set_vertex_buffer(0, self.occluder_vb.slice(..));
        pass.counted_draw(0..12, 0..1);
    }
}

fn sky_entry(entry: u32) -> &'static str {
    match entry {
        0 => "fs_sky_ldr",
        1 => "fs_sky_hdr",
        // ADR-3294, `aerial_from_tables`.
        4 => "fs_aerial_tables",
        5 => "fs_aerial_tables_lite",
        6 => "fs_weather_veil",
        _ => "fs_aerial",
    }
}

fn fog_entry(entry: u32) -> &'static str {
    match entry {
        0 => "fs_scatter",
        1 => "fs_integrate",
        2 => "fs_fog_blend",
        _ => "fs_fog_ldr",
    }
}

fn cloud_entry(entry: u32) -> &'static str {
    match entry {
        0 => "fs_cloud_coverage",
        1 => "fs_cloud_ground",
        2 => "fs_cloud_march",
        3 => "fs_cloud_shadow",
        5 => "fs_cloud_fresh",
        6 => "fs_cloud_reconstruct",
        7 => "fs_cloud_shadow_map",
        8 => "fs_cloud_shadow_debug",
        _ => "fs_cloud_composite",
    }
}

fn blends(entry: u32, kind: u32) -> Option<wgpu::BlendState> {
    let on = match kind {
        0 => matches!(entry, 2 | 4 | 5 | 6),
        1 => entry == 2,
        _ => entry == 3 || entry == 4 || entry == 8,
    };
    if !on {
        return None;
    }
    let component = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::SrcAlpha,
        operation: wgpu::BlendOperation::Add,
    };
    Some(wgpu::BlendState {
        color: component,
        alpha: component,
    })
}

fn cached_pipe(
    cache: &mut HashMap<(wgpu::TextureFormat, u32), wgpu::RenderPipeline>,
    shared: &crate::device_pipelines::DevicePipelines,
    device: &wgpu::Device,
    (layout, module): (&wgpu::PipelineLayout, &wgpu::ShaderModule),
    (format, entry): (wgpu::TextureFormat, u32),
    label: &str,
) -> wgpu::RenderPipeline {
    if let Some(pipe) = cache.get(&(format, entry)) {
        return pipe.clone();
    }
    let key =
        crate::device_pipelines::pipeline_key(&(label, format, entry, crate::shader_reload::shader_epoch()));
    let pipe = shared.get_or_create(key, || build_pipe(device, layout, module, (format, entry), label));
    cache.insert((format, entry), pipe.clone());
    pipe
}

/// One outdoor pipeline: `label` names the pass family (sky, fog, clouds), `entry` its
/// fragment entry point.
fn build_pipe(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    (format, entry): (wgpu::TextureFormat, u32),
    label: &str,
) -> wgpu::RenderPipeline {
    let name = if label.contains("sky") {
        sky_entry(entry)
    } else if label.contains("fog") {
        fog_entry(entry)
    } else {
        cloud_entry(entry)
    };
    let kind = if label.contains("sky") {
        0
    } else if label.contains("fog") {
        1
    } else {
        2
    };
    let primary = wgpu::ColorTargetState {
        format,
        blend: blends(entry, kind),
        write_mask: wgpu::ColorWrites::ALL,
    };
    // The march, fresh and reconstruct passes write colour and a distance target.
    let distance = wgpu::ColorTargetState {
        format: CLOUD_DIST_FORMAT,
        blend: None,
        write_mask: wgpu::ColorWrites::ALL,
    };
    let march_mrt = kind == 2 && matches!(entry, 2 | 5 | 6);
    let one = [Some(primary.clone())];
    let two = [Some(primary), Some(distance)];
    let targets: &[Option<wgpu::ColorTargetState>] = if march_mrt { &two } else { &one };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_fullscreen"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(name),
            targets,
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    })
}

/// The lab's fog occluder: two slabs in the light's depth ([`render_fog_shaft`]).
fn occluder_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("fog occluder"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_occluder"),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: 12,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &[wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 0,
                }],
            })],
            compilation_options: Default::default(),
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: None,
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    })
}

fn draw_pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bg: &wgpu::BindGroup,
    view: &wgpu::TextureView,
    label: &str,
    clear: bool,
) {
    draw_pass_seq(encoder, &[Some(pipeline)], bg, view, label, clear);
}

/// A cloud march, fresh or reconstruct pass: colour and distance, both cleared.
fn draw_march(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bg: &wgpu::BindGroup,
    color: &wgpu::TextureView,
    distance: &wgpu::TextureView,
    label: &str,
) {
    let _span = tracing::info_span!("outdoor_pass", pass = label).entered();
    let ops = wgpu::Operations {
        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
        store: wgpu::StoreOp::Store,
    };
    let attachment = |view| wgpu::RenderPassColorAttachment {
        view,
        depth_slice: None,
        resolve_target: None,
        ops,
    };
    let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(attachment(color)), Some(attachment(distance))],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        ..Default::default()
    });
    pass.counted_set_bind_group(0, bg, &[]);
    pass.counted_set_pipeline(pipeline);
    pass.counted_draw(0..3, 0..1);
}

/// [`draw_pass`] with one full-screen draw per present pipeline, in order, in one pass.
fn draw_pass_seq(
    encoder: &mut wgpu::CommandEncoder,
    pipelines: &[Option<&wgpu::RenderPipeline>],
    bg: &wgpu::BindGroup,
    view: &wgpu::TextureView,
    label: &str,
    clear: bool,
) {
    let _span = tracing::info_span!("outdoor_pass", pass = label).entered();
    let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: if clear {
                    wgpu::LoadOp::Clear(wgpu::Color::BLACK)
                } else {
                    wgpu::LoadOp::Load
                },
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        ..Default::default()
    });
    pass.counted_set_bind_group(0, bg, &[]);
    for pipeline in pipelines.iter().flatten() {
        pass.counted_set_pipeline(pipeline);
        pass.counted_draw(0..3, 0..1);
    }
}

fn module(device: &wgpu::Device, source: &str, path: &str) -> Result<wgpu::ShaderModule, RenderError> {
    let composed = crate::shader_lib::compose_source(source, path, HashMap::new())?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(path),
        source: wgpu::ShaderSource::Naga(Cow::Owned(composed)),
    }))
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn tex_entry(binding: u32, sample_type: wgpu::TextureSampleType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn tex3d_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D3,
            multisampled: false,
        },
        count: None,
    }
}

/// An RGBA8 cube of `size³` texels.
fn volume_tex(
    device: &wgpu::Device,
    queue: &crate::device::TrackedQueue,
    size: u32,
    texels: &[u8],
    label: &str,
) -> Tex {
    let extent = wgpu::Extent3d {
        width: size,
        height: size,
        depth_or_array_layers: size,
    };
    let texture = crate::device::TrackedDevice::observe(device).create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        texels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size * 4),
            rows_per_image: Some(size),
        },
        extent,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Tex { texture, view }
}

fn samp_entry(binding: u32, ty: wgpu::SamplerBindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(ty),
        count: None,
    }
}

fn depth_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Depth,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn buf_bind(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn tex_bind(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

fn depth_bind(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    tex_bind(binding, view)
}

fn uniform_buffer(device: &wgpu::Device, label: &str, size: usize) -> wgpu::Buffer {
    crate::device::TrackedDevice::observe(device).create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn depth_tex(device: &wgpu::Device, width: u32, height: u32, label: &str) -> Tex {
    let texture = crate::device::TrackedDevice::observe(device).create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: extent(width, height),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Tex { texture, view }
}

fn rgba_tex(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
    label: &str,
) -> Tex {
    // Counted (ADR-2585): the scatter, fog and cloud targets come through here when a size or a
    // setting changes, and the hitch gate must see one made after warm-up.
    let texture = crate::device::TrackedDevice::observe(device).create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: extent(width, height),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: usage | crate::resource_inspector::debug_usage(),
        view_formats: &[],
    });
    crate::resource_inspector::track(label, &texture);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Tex { texture, view }
}

fn color_target(device: &wgpu::Device, width: u32, height: u32) -> Tex {
    let texture = crate::device::TrackedDevice::observe(device).create_texture(&wgpu::TextureDescriptor {
        label: Some("outdoor rgba8"),
        size: extent(width, height),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Tex { texture, view }
}

fn extent(width: u32, height: u32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width: width.max(1),
        height: height.max(1),
        depth_or_array_layers: 1,
    }
}

fn upload_lut(
    device: &wgpu::Device,
    queue: &crate::device::TrackedQueue,
    existing: Option<Tex>,
    image: &crate::LutImage,
    label: &str,
) -> Tex {
    let fits = existing
        .as_ref()
        .is_some_and(|tex| tex.texture.width() == image.width && tex.texture.height() == image.height);
    let tex = if fits {
        existing.expect("checked")
    } else {
        rgba_tex(
            device,
            image.width,
            image.height,
            wgpu::TextureFormat::Rgba32Float,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            label,
        )
    };
    let mut bytes = Vec::with_capacity(image.texels.len() * 16);
    for texel in &image.texels {
        bytes.extend_from_slice(bytemuck::bytes_of(texel));
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(image.width.max(1) * 16),
            rows_per_image: Some(image.height.max(1)),
        },
        extent(image.width, image.height),
    );
    tex
}

fn write_rgba8(
    queue: &crate::device::TrackedQueue,
    texture: &wgpu::Texture,
    rgba: &[u8],
    width: u32,
    height: u32,
) {
    write_rgba8_mip(queue, texture, 0, rgba, width, height);
}

/// Uploads one sky-body albedo map into `slot`, keyed so an unchanged map is not re-uploaded.
fn upload_albedo(
    device: &wgpu::Device,
    queue: &crate::device::TrackedQueue,
    slot: &mut Option<(u64, Tex)>,
    texture: &SkyBodyTexture,
    label: &str,
) {
    if slot.as_ref().is_some_and(|(key, _)| *key == texture.key) {
        return;
    }
    let tex = rgba_tex(
        device,
        texture.width,
        texture.height,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        label,
    );
    write_rgba8(queue, &tex.texture, &texture.rgba, texture.width, texture.height);
    *slot = Some((texture.key, tex));
}

fn write_rgba8_mip(
    queue: &crate::device::TrackedQueue,
    texture: &wgpu::Texture,
    level: u32,
    rgba: &[u8],
    width: u32,
    height: u32,
) {
    let (padded, bytes_per_row) = pad_rows(rgba, width.max(1), height.max(1));
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: level,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &padded,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(height.max(1)),
        },
        extent(width, height),
    );
}

/// Writes one mip of an `Rgba16Float` texture from 8-byte texels.
fn write_f16_mip(
    queue: &crate::device::TrackedQueue,
    texture: &wgpu::Texture,
    level: u32,
    bytes: &[u8],
    width: u32,
    height: u32,
) {
    let (padded, bytes_per_row) = pad_rows_bytes(bytes, width.max(1) * 8, height.max(1));
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: level,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &padded,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(height.max(1)),
        },
        extent(width, height),
    );
}

/// Pads rows of `row` bytes to the 256-byte copy alignment. Sizes are computed in `usize`
/// (a `u32` product wrapped to a short buffer for huge images); a row that cannot be padded
/// inside `u32` comes back empty with `u32::MAX`, which the copy validation rejects.
pub(crate) fn pad_rows_bytes(bytes: &[u8], row: u32, height: u32) -> (Vec<u8>, u32) {
    let (row, height) = (row as usize, height as usize);
    let aligned = row.div_ceil(256) * 256;
    let Ok(aligned_u32) = u32::try_from(aligned) else {
        return (Vec::new(), u32::MAX);
    };
    if aligned == row && bytes.len() >= row * height {
        return (bytes.to_vec(), aligned_u32);
    }
    let mut padded = vec![0u8; aligned * height];
    for y in 0..height {
        let src = y * row;
        let dst = y * aligned;
        let end = src + row;
        if end <= bytes.len() {
            padded[dst..dst + row].copy_from_slice(&bytes[src..end]);
        }
    }
    (padded, aligned_u32)
}

fn pad_rows(rgba: &[u8], width: u32, height: u32) -> (Vec<u8>, u32) {
    // `width * 4` fits `u32` only below 2^30; a wider image is rejected by the caller's
    // texture limits, so saturate instead of wrapping.
    pad_rows_bytes(rgba, width.saturating_mul(4), height)
}

/// Writes the procedural sky back into IBL textures the atmosphere previously replaced.
pub(crate) fn write_procedural_ibl(
    queue: &crate::device::TrackedQueue,
    irradiance: &wgpu::Texture,
    prefilter: &wgpu::Texture,
) {
    let maps = crate::ibl::build_procedural_ibl();
    write_f16_mip(
        queue,
        irradiance,
        0,
        &crate::ibl::rgba8_to_rgba16f(&maps.irradiance.rgba),
        maps.irradiance.width,
        maps.irradiance.height,
    );
    for (level, image) in maps.prefilter.iter().enumerate() {
        write_f16_mip(
            queue,
            prefilter,
            level as u32,
            &crate::ibl::rgba8_to_rgba16f(&image.rgba),
            image.width,
            image.height,
        );
    }
}

fn weather_key(settings: &CloudSettings) -> u64 {
    weather_words_key(settings, true)
}

/// [`weather_key`] without coverage and cloud type: the weather map's layout (tile, resolution,
/// seed, pattern), which a weather transition leaves alone (ADR-429).
fn weather_shape_key(settings: &CloudSettings) -> u64 {
    weather_words_key(settings, false)
}

/// One step of the weather value grid (ADR-429). Matches `regia_vfx`'s `WEATHER_VALUE_STEP`.
const WEATHER_VALUE_STEP: f32 = 1.0 / 256.0;

/// Finite values round to [`WEATHER_VALUE_STEP`]; anything else is 0. The shape key does not
/// use this.
fn quantize_weather_word(value: f32) -> u32 {
    if !value.is_finite() {
        return 0.0f32.to_bits();
    }
    ((value / WEATHER_VALUE_STEP).round() * WEATHER_VALUE_STEP).to_bits()
}

fn weather_words_key(settings: &CloudSettings, with_values: bool) -> u64 {
    let pattern = match settings.pattern {
        crate::CloudPattern::Coverage => 0u32,
        crate::CloudPattern::SplitHemisphere => 1,
        crate::CloudPattern::Constant => 2,
    };
    let (coverage, cloud_type) = if with_values {
        (
            quantize_weather_word(settings.coverage),
            quantize_weather_word(settings.cloud_type),
        )
    } else {
        (0, 0)
    };
    let words = [
        coverage,
        cloud_type,
        settings.domain_m.to_bits(),
        settings.map_resolution,
        settings.seed,
        pattern,
    ];
    // FNV-1a over the inputs that change the map.
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

fn unit(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < 1.0e-8 {
        [0.0, 1.0, 0.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

fn dir_from_equirect(u: f32, v: f32) -> [f32; 3] {
    let phi = (u - 0.5) * std::f32::consts::TAU;
    let theta = v * std::f32::consts::PI;
    let y = theta.cos();
    let radial = theta.sin();
    [radial * phi.cos(), y, radial * phi.sin()]
}

fn hammersley(i: u32, n: u32) -> [f32; 2] {
    [i as f32 / n as f32, radical_inverse(i)]
}

fn radical_inverse(mut bits: u32) -> f32 {
    bits = bits.rotate_right(16);
    bits = ((bits & 0x5555_5555) << 1) | ((bits & 0xAAAA_AAAA) >> 1);
    bits = ((bits & 0x3333_3333) << 2) | ((bits & 0xCCCC_CCCC) >> 2);
    bits = ((bits & 0x0F0F_0F0F) << 4) | ((bits & 0xF0F0_F0F0) >> 4);
    bits = ((bits & 0x00FF_00FF) << 8) | ((bits & 0xFF00_FF00) >> 8);
    bits as f32 * 2.328_306_4e-10
}

fn cosine_hemisphere(xi: [f32; 2]) -> [f32; 3] {
    let radius = xi[0].sqrt();
    let phi = std::f32::consts::TAU * xi[1];
    [
        radius * phi.cos(),
        radius * phi.sin(),
        (1.0 - xi[0]).max(0.0).sqrt(),
    ]
}

fn to_world(local: [f32; 3], n: [f32; 3]) -> [f32; 3] {
    let up = if n[2].abs() < 0.999 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let tangent = unit(cross(up, n));
    let bitangent = cross(n, tangent);
    unit([
        tangent[0] * local[0] + bitangent[0] * local[1] + n[0] * local[2],
        tangent[1] * local[0] + bitangent[1] * local[1] + n[1] * local[2],
        tangent[2] * local[0] + bitangent[2] * local[1] + n[2] * local[2],
    ])
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

struct Look {
    inv_view_proj: [[f32; 4]; 4],
    view_proj: [[f32; 4]; 4],
    eye: [f32; 3],
}

fn look(eye: Vec3, target: Vec3, up: Vec3, fov_y: f32, aspect: f32, near: f32, far: f32) -> Look {
    let view = crate::view_math::look_at_view(eye, target, up);
    let (fov_y, near, far) = crate::view_math::usable_projection(fov_y, near, far);
    let proj = perspective(fov_y, aspect, near, far);
    let vp = proj * view;
    Look {
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        view_proj: vp.to_cols_array_2d(),
        eye: eye.to_array(),
    }
}

fn project_px(vp: [[f32; 4]; 4], point: [f32; 3], width: u32, height: u32) -> (u32, u32) {
    let clip = Mat4::from_cols_array_2d(&vp) * Vec3::from_array(point).extend(1.0);
    let ndc = clip.truncate() / clip.w;
    let u = (ndc.x * 0.5 + 0.5).clamp(0.0, 0.999);
    let v = (0.5 - ndc.y * 0.5).clamp(0.0, 0.999);
    ((u * width as f32) as u32, (v * height as f32) as u32)
}

fn submit(device: &RenderDevice, encoder: wgpu::CommandEncoder) -> Result<(), RenderError> {
    device.queue.submit([encoder.finish()]);
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .map(|_| ())
        .map_err(RenderError::Poll)
}

fn read_target(
    device: &RenderDevice,
    target: &Tex,
    width: u32,
    height: u32,
) -> Result<OutdoorImage, RenderError> {
    let pixels = crate::frame::read_rgba8(&device.device, &device.queue, &target.texture, width, height)?;
    Ok(OutdoorImage {
        width,
        height,
        pixels,
    })
}

/// How the sky pass encodes its output.
#[derive(Clone, Copy, PartialEq)]
enum SkyMode {
    /// Scene-referred radiance × `pre_exposure` into the HDR target.
    Hdr {
        /// The HDR target's pre-exposure.
        pre_exposure: f32,
    },
    /// Exposure + AgX + sRGB into an RGBA8 target (tests). `false`: the analytic gradient.
    Ldr { physical: bool, exposure: f32 },
}

/// Angle (radians) one pixel spans at the centre of the view.
fn pixel_angle(inv_view_proj: [[f32; 4]; 4], height: u32) -> f32 {
    let inv = Mat4::from_cols_array_2d(&inv_view_proj);
    let dir = |ndc_y: f32| {
        let far = inv.project_point3(Vec3::new(0.0, ndc_y, 1.0));
        let near = inv.project_point3(Vec3::new(0.0, ndc_y, 0.0));
        (far - near).normalize_or_zero()
    };
    let step = 2.0 / height.max(1) as f32;
    dir(0.0).angle_between(dir(step)).max(1.0e-5)
}

/// Angle (radians) one pixel spans at the centre of the view, from the chord between the two view directions:
/// `2·asin(|a − b| / 2)` (ADR-2486). [`pixel_angle`] takes `acos` of their dot product, which in `f32` loses the
/// angle at high resolution: near 1 the dot product's spacing is 6e-8, and one pixel at 3840x2160 with a 45° field of
/// view changes it by about 7e-8, so the angle rounds to a few discrete values or to its floor. The star field is
/// drawn a pixel wide from it, so at 4K the stars shrink to specks. Used with star flux on.
fn pixel_angle_chord(inv_view_proj: [[f32; 4]; 4], height: u32) -> f32 {
    // In f64: in f32 the far-plane point (NDC z = 1, kilometres out) is metres off, and points near the camera lose
    // their precision to the world coordinates; either bends one pixel's direction by several percent at 4K.
    let inv = Mat4::from_cols_array_2d(&inv_view_proj).as_dmat4();
    let dir = |ndc_y: f64| {
        let far = inv.project_point3(regia_core::glam::DVec3::new(0.0, ndc_y, 1.0));
        let near = inv.project_point3(regia_core::glam::DVec3::new(0.0, ndc_y, 0.0));
        (far - near).normalize_or_zero()
    };
    let step = 2.0 / f64::from(height.max(1));
    let chord = (dir(-0.5 * step) - dir(0.5 * step)).length();
    ((2.0 * (0.5 * chord).clamp(0.0, 1.0).asin()) as f32).max(1.0e-5)
}

/// The star brightness gain of a frame `height` pixels tall with star flux on (ADR-2486): `(height / 1080)²`, so a
/// star a pixel wide keeps the light it has at 1080p.
fn star_flux_gain(height: u32) -> f32 {
    let k = height.max(1) as f32 / 1080.0;
    k * k
}

#[allow(clippy::too_many_arguments)] // each argument is an independent sky input
fn sky_uniform(
    luts: Option<&AtmosphereLuts>,
    body: Option<SkyBody>,
    moons: &[SkyBody],
    body_texture: bool,
    moon_textures: [bool; 2],
    planet_surface: bool,
    inv_view_proj: [[f32; 4]; 4],
    eye: [f32; 3],
    width: u32,
    height: u32,
    mode: SkyMode,
) -> SkyGpu {
    let params = luts.map_or_else(AtmosphereParameters::default, |luts| luts.params);
    let (sun, disk) = if let Some(luts) = luts {
        let disk = sun_disk_radiance(luts, &params);
        (
            [
                luts.sun_direction[0],
                luts.sun_direction[1],
                luts.sun_direction[2],
                params.sun_angular_radius,
            ],
            [
                disk[0],
                disk[1],
                disk[2],
                if disk[0] + disk[1] + disk[2] > 0.0 {
                    1.0
                } else {
                    0.0
                },
            ],
        )
    } else {
        ([0.0, 1.0, 0.0, params.sun_angular_radius], [0.0; 4])
    };
    let (physical, exposure, ldr) = match mode {
        SkyMode::Hdr { .. } => (true, 0.0, false),
        SkyMode::Ldr { physical, exposure } => (physical, exposure, true),
    };
    let (body_dir, body_color) = match body {
        Some(body) if body.angular_diameter_deg > 0.0 => {
            let d = unit(body.direction);
            (
                [d[0], d[1], d[2], (0.5 * body.angular_diameter_deg).to_radians()],
                [
                    body.tint[0],
                    body.tint[1],
                    body.tint[2],
                    body.bands.clamp(0.0, 1.0),
                ],
            )
        }
        _ => ([0.0; 4], [0.0; 4]),
    };
    // `ground.w` = 0 turns the shader's planet hit test off: those rays keep the sky
    // table, which is black on an airless world (ADR-162).
    let (ground, cam_km) = if let Some(luts) = luts {
        let g = crate::atmosphere::ground_radiance(luts);
        let ground = if planet_surface {
            [g[0], g[1], g[2], params.bottom_radius_km]
        } else {
            [0.0; 4]
        };
        (
            ground,
            luts.camera_height_m.max(crate::atmosphere::MIN_CAMERA_HEIGHT_M) / 1000.0,
        )
    } else {
        ([0.0; 4], 0.0)
    };
    SkyGpu {
        inv_view_proj,
        sun,
        camera: [eye[0], eye[1], eye[2], width as f32],
        disk,
        misc: [
            if physical { 1.0 } else { 0.0 },
            exposure,
            if ldr && physical { 1.0 } else { 0.0 },
            height as f32,
        ],
        body_dir,
        body_color,
        stars: [
            params.day_star_radiance,
            pixel_angle(inv_view_proj, height),
            params.sun_illuminance,
            if params.is_airless() { 1.0 } else { 0.0 },
        ],
        moons: {
            let mut rows = [[0.0; 4]; 4];
            let mut slot = 0;
            for (orig, moon) in moons.iter().enumerate() {
                if slot >= 2 {
                    break;
                }
                if moon.angular_diameter_deg <= 0.0 {
                    continue;
                }
                let d = unit(moon.direction);
                rows[slot * 2] = [d[0], d[1], d[2], (0.5 * moon.angular_diameter_deg).to_radians()];
                // w = 1 tells the shader this moon samples its albedo map instead of the tint.
                let textured = moon_textures.get(orig).copied().unwrap_or(false);
                rows[slot * 2 + 1] = [
                    moon.tint[0],
                    moon.tint[1],
                    moon.tint[2],
                    if textured { 1.0 } else { 0.0 },
                ];
                slot += 1;
            }
            rows
        },
        body_tex: [
            if body_texture && body.is_some() { 1.0 } else { 0.0 },
            match mode {
                SkyMode::Hdr { pre_exposure } => pre_exposure,
                SkyMode::Ldr { .. } => 1.0,
            },
            cam_km,
            0.0,
        ],
        ground,
        table_air: [0.0; 4],
        table_ground: [0.0; 4],
        interiors: [[0.0; 4]; 8],
        cloud: [0.0; 4],
        cloud_air: [0.0; 4],
        cloud_ground_sun: [0.0; 4],
        cloud_ground_sky: [0.0; 4],
    }
}

fn draw_ldr_sky(
    state: &mut OutdoorState,
    device: &RenderDevice,
    look: &Look,
    width: u32,
    height: u32,
    physical: bool,
) -> Result<OutdoorImage, RenderError> {
    state.ensure_gpu(&device.device, device.shared_pipelines())?;
    let luts = if physical { state.luts.clone() } else { None };
    let exposure =
        crate::post::exposure_from_ev100(if luts.as_ref().is_some_and(|l| l.params.is_airless()) {
            12.0
        } else {
            14.0
        });
    if let Some(gpu) = state.gpu.as_mut() {
        gpu.ensure_body_albedo(
            &device.device,
            &device.tracked_queue(),
            state.params.sky_body_albedo.as_deref(),
        );
        gpu.ensure_moon_albedo(&device.device, &device.tracked_queue(), &state.params.moon_albedo);
    }
    let bytes = sky_uniform(
        luts.as_ref(),
        state.params.sky_body,
        &state.params.moons,
        state.params.sky_body_albedo.is_some(),
        [
            state.params.moon_albedo[0].is_some(),
            state.params.moon_albedo[1].is_some(),
        ],
        state.params.shows_planet_surface(),
        look.inv_view_proj,
        look.eye,
        width,
        height,
        SkyMode::Ldr { physical, exposure },
    );
    let mut bytes = bytes;
    if state.star_flux && !state.panorama {
        bytes.body_tex[3] = star_flux_gain(height);
        bytes.stars[1] = pixel_angle_chord(look.inv_view_proj, height);
    }
    if state.panorama && physical {
        bytes.misc[0] = 2.0;
        bytes.stars[1] = std::f32::consts::TAU / width.max(1) as f32;
    }
    let depth = depth_tex(&device.device, 1, 1, "sky depth");
    let target = color_target(&device.device, width, height);
    let gpu = state.gpu.as_mut().expect("outdoor gpu");
    if let Some(luts) = luts.as_ref() {
        gpu.ensure_sky_luts(
            &device.device,
            &crate::device::TrackedQueue::observe(&device.queue),
            luts,
            None,
        );
    }
    device
        .queue
        .write_buffer(&gpu.sky_buf, 0, bytemuck::bytes_of(&bytes));
    let bg = gpu.sky_bind(&device.device, &depth.view, false);
    let pipe = gpu.sky_pipe(&device.device, wgpu::TextureFormat::Rgba8Unorm, 0);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("outdoor sky"),
        });
    draw_pass(&mut encoder, &pipe, &bg, &target.view, "sky ldr", true);
    submit(device, encoder)?;
    read_target(device, &target, width, height)
}

/// Analytic sky gradient. No sun disk and no LUT march.
#[tracing::instrument(skip_all)]
pub fn render_analytic_sky(
    device: &RenderDevice,
    width: u32,
    height: u32,
    fov_y: f32,
) -> Result<OutdoorImage, RenderError> {
    let look = look(
        Vec3::new(0.0, 2.0, 0.0),
        Vec3::new(0.0, 2.2, -1.0),
        Vec3::Y,
        fov_y,
        width as f32 / height.max(1) as f32,
        0.1,
        100.0,
    );
    draw_ldr_sky(&mut OutdoorState::default(), device, &look, width, height, false)
}

/// Physical sky for one time of day. `look_at_sun` aims the camera at the solar disk;
/// otherwise it looks a few degrees above the horizon, away from the sun.
#[tracing::instrument(skip_all)]
pub fn render_time_of_day_sky(
    device: &RenderDevice,
    time: &TimeOfDay,
    size: (u32, u32),
    fov_y: f32,
    look_at_sun: bool,
) -> Result<OutdoorImage, RenderError> {
    let toward = Vec3::from_array(sun_position(time).direction);
    let look_dir = if look_at_sun {
        toward
    } else {
        let away = Vec3::new(-toward.x, 0.0, -toward.z);
        let away = if away.length_squared() < 1.0e-8 {
            Vec3::Z
        } else {
            away.normalize()
        };
        away + Vec3::Y * 0.25
    };
    render_sky_view(
        device,
        &SkyView {
            time: *time,
            planet: AtmosphereParameters::earth(),
            body: None,
            size,
            fov_y,
            look_dir: look_dir.to_array(),
            panorama: false,
        },
    )
}

/// A sky-only shot: planet, sun clock, optional body, camera direction.
#[derive(Clone, Debug)]
pub struct SkyView {
    /// Sun clock.
    pub time: TimeOfDay,
    /// Planet medium.
    pub planet: AtmosphereParameters,
    /// Body in the sky.
    pub body: Option<SkyBody>,
    /// Image size.
    pub size: (u32, u32),
    /// Vertical field of view (radians).
    pub fov_y: f32,
    /// World direction the camera looks along (2 m above the ground).
    pub look_dir: [f32; 3],
    /// Ignore the camera: an equirectangular panorama, azimuth clockwise from north
    /// (+Z, through east +X) across the width, elevation +90° to -10° down the height.
    pub panorama: bool,
}

/// Renders the LUT sky (sun disc, stars, sky body) for `view` into RGBA8.
#[tracing::instrument(skip_all)]
pub fn render_sky_view(device: &RenderDevice, view: &SkyView) -> Result<OutdoorImage, RenderError> {
    let mut state = OutdoorState::default();
    state.params.time = Some(view.time);
    state.params.planet = view.planet;
    state.params.sky_body = view.body;
    let _ = state.apply_sun(
        true,
        DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 1.0),
        2.0,
    );
    let eye = Vec3::new(0.0, 2.0, 0.0);
    let dir = Vec3::from_array(view.look_dir).normalize_or(Vec3::Z);
    let target = eye + dir;
    let up = if dir.dot(Vec3::Y).abs() > 0.95 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    let (width, height) = view.size;
    let look = look(
        eye,
        target,
        up,
        view.fov_y,
        width as f32 / height.max(1) as f32,
        0.1,
        40.0,
    );
    state.panorama = view.panorama;
    draw_ldr_sky(&mut state, device, &look, width, height, true)
}

/// Fog volume with a shadowed shaft, then the same view after the density is cleared.
///
/// The second frame keeps history, so the shaft must still be visible.
#[tracing::instrument(skip_all)]
pub fn render_fog_shaft(device: &RenderDevice) -> Result<FogShaftImages, RenderError> {
    let width = 128u32;
    let height = 72u32;
    let mut state = OutdoorState::default();
    state.params.fog = VolumetricFogSettings {
        density: 0.0,
        albedo: [0.95, 0.95, 0.95],
        anisotropy: 0.6,
        history_weight: 0.85,
        ..VolumetricFogSettings::default()
    };
    state.params.volumes = vec![FogVolume::Box {
        center: [0.0, 2.0, 6.0],
        extents: [6.0, 1.15, 4.0],
        density: 0.25,
    }];
    let eye = Vec3::new(0.0, 2.2, 16.0);
    let aim = Vec3::new(0.0, 1.5, 6.0);
    let first_look = look(
        eye,
        aim,
        Vec3::Y,
        50.0_f32.to_radians(),
        width as f32 / height as f32,
        0.3,
        40.0,
    );
    let light_view = look_at_mat4(Vec3::new(0.0, 24.0, 6.0), Vec3::new(0.0, 0.0, 6.0), Vec3::Z);
    let light_vp = (orthographic(-20.0, 20.0, -20.0, 20.0, 0.1, 40.0) * light_view).to_cols_array_2d();
    let lit = project_px(first_look.view_proj, [3.0, 1.5, 6.0], width, height);
    let shadowed = project_px(first_look.view_proj, [-3.0, 1.5, 6.0], width, height);
    state.ensure_gpu(&device.device, device.shared_pipelines())?;
    let shadow = state
        .gpu
        .as_mut()
        .expect("outdoor gpu")
        .ensure_shadow(&device.device);
    let depth = depth_tex(&device.device, 1, 1, "fog scene depth");
    let frame = |look: &Look, prev: [[f32; 4]; 4]| OutdoorComposite {
        prepared_sky: None,
        depth_is_scene: false,
        pre_exposure: 1.0,
        hdr: &depth.view,
        depth: &depth.view,
        shadow: Some(&shadow),
        width,
        height,
        format: wgpu::TextureFormat::Rgba8Unorm,
        inv_view_proj: look.inv_view_proj,
        unjittered_inv_view_proj: look.inv_view_proj,
        prev_view_proj: prev,
        eye: look.eye,
        near: 0.3,
        far: 40.0,
        sun_travel: [0.0, -1.0, 0.0],
        sun_color: [1.0, 1.0, 1.0],
        // Low enough that AgX keeps a ramp across the shaft instead of clipping
        // the lit froxels to white.
        sun_illuminance: 18_000.0,
        light_vp,
        shadow_size: SHADOW_SIZE as f32,
        atmosphere: false,
        fog: true,
        clouds: false,
        unjittered_view_proj: look.view_proj,
        cloud_reprojection: false,
        cloud_step_percent: 100,
        cloud_cone_light: false,
        cloud_march: crate::CloudMarch::REALTIME,
        local_shadow: None,
        fog_light_shadows: [[[0.0; 4]; 6]; 4],
    };
    let target = color_target(&device.device, width, height);
    {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fog shaft"),
            });
        let shot = frame(&first_look, first_look.view_proj);
        let uniform = fog_uniform(&shot, state.params(), false, shot.eye, [60.0, 75.0, 100.0]);
        {
            let gpu = state.gpu.as_mut().expect("outdoor gpu");
            device
                .queue
                .write_buffer(&gpu.fog_buf, 0, bytemuck::bytes_of(&uniform));
            gpu.draw_occluder(&device.device, &mut encoder, &shadow);
        }
        state.draw_fog(
            &device.device,
            &crate::device::TrackedQueue::observe(&device.queue),
            &mut encoder,
            &shot,
            Some(&target.view),
        )?;
        submit(device, encoder)?;
    }
    let first = read_target(device, &target, width, height)?;
    state.params.volumes.clear();
    state.params.fog.density = 0.0;
    let eye2 = eye + Vec3::new(0.15, 0.0, 0.0);
    let second_look = look(
        eye2,
        aim,
        Vec3::Y,
        50.0_f32.to_radians(),
        width as f32 / height as f32,
        0.3,
        40.0,
    );
    {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fog history"),
            });
        let shot = frame(&second_look, first_look.view_proj);
        state.draw_fog(
            &device.device,
            &crate::device::TrackedQueue::observe(&device.queue),
            &mut encoder,
            &shot,
            Some(&target.view),
        )?;
        submit(device, encoder)?;
    }
    let second = read_target(device, &target, width, height)?;
    Ok(FogShaftImages {
        first,
        second,
        shadowed,
        lit,
    })
}

/// Froxel in-scatter (cd/m², linear RGB) at the centre of a view into homogeneous height fog
/// of extinction `density` (1/m, no height falloff), lit only by the uniform fallback sky
/// ambient (no sun, no LUTs), seen to a far plane `far_m` away: the fog composite's HDR output
/// over a black scene. Tests compare it with the closed form of the CPU mirror
/// ([`crate::integrate_segment`]); a thin haze (1e-5 /m, Mars dust) once read 5× too dim.
#[tracing::instrument(skip_all)]
pub fn render_uniform_fog_inscatter(
    device: &RenderDevice,
    density: f32,
    far_m: f32,
) -> Result<[f32; 3], RenderError> {
    let size = 32u32;
    let mut state = OutdoorState::default();
    state.params.fog = VolumetricFogSettings {
        density,
        height_falloff: 0.0,
        base_height: 0.0,
        albedo: [0.85, 0.88, 0.92],
        history_weight: 0.0,
        ..VolumetricFogSettings::default()
    };
    let near = 0.5;
    let look = look(
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(0.0, 0.0, -1.0),
        Vec3::Y,
        0.5,
        1.0,
        near,
        far_m,
    );
    state.ensure_gpu(&device.device, device.shared_pipelines())?;
    let depth = depth_tex(&device.device, size, size, "uniform fog depth");
    let hdr = rgba_tex(
        &device.device,
        size,
        size,
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        "uniform fog hdr",
    );
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("uniform fog"),
        });
    {
        // Far-plane depth (every pixel is "sky" at the far plane) and a black scene.
        let _pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("uniform fog clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &hdr.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth.view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            ..Default::default()
        });
    }
    let shot = OutdoorComposite {
        prepared_sky: None,
        depth_is_scene: false,
        pre_exposure: 1.0,
        hdr: &hdr.view,
        depth: &depth.view,
        shadow: None,
        width: size,
        height: size,
        format: wgpu::TextureFormat::Rgba16Float,
        inv_view_proj: look.inv_view_proj,
        unjittered_inv_view_proj: look.inv_view_proj,
        prev_view_proj: look.view_proj,
        eye: look.eye,
        near,
        far: far_m,
        sun_travel: [0.0, -1.0, 0.0],
        sun_color: [1.0, 1.0, 1.0],
        sun_illuminance: 0.0,
        light_vp: look.view_proj,
        shadow_size: 0.0,
        atmosphere: false,
        fog: true,
        clouds: false,
        unjittered_view_proj: look.view_proj,
        cloud_reprojection: false,
        cloud_step_percent: 100,
        cloud_cone_light: false,
        cloud_march: crate::CloudMarch::REALTIME,
        local_shadow: None,
        fog_light_shadows: [[[0.0; 4]; 6]; 4],
    };
    state.draw_fog(
        &device.device,
        &crate::device::TrackedQueue::observe(&device.queue),
        &mut encoder,
        &shot,
        None,
    )?;
    let row = size * 8;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("uniform fog readback"),
        size: u64::from(row * size),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        hdr.texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(size),
            },
        },
        extent(size, size),
    );
    submit(device, encoder)?;
    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|e| RenderError::Readback(e.to_string()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let data = slice
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?
        .to_vec();
    let at = ((size / 2) * row + (size / 2) * 8) as usize;
    let half = |i: usize| f16_to_f32(u16::from_le_bytes([data[at + 2 * i], data[at + 2 * i + 1]]));
    Ok([half(0), half(1), half(2)])
}

/// IEEE half to single precision (normal, subnormal, zero, inf/NaN).
fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let mant = f32::from(h & 0x3ff);
    match exp {
        0 => sign * mant * 2f32.powi(-24),
        31 => {
            if mant == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + mant / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// One local light in a fog box, sun off. The centre of the image faces the lamp.
#[tracing::instrument(skip_all)]
pub fn render_local_fog(device: &RenderDevice) -> Result<OutdoorImage, RenderError> {
    let width = 64u32;
    let height = 48u32;
    let mut state = OutdoorState::default();
    state.params.fog.albedo = [0.95, 0.95, 0.95];
    state.params.fog.anisotropy = 0.0;
    state.params.volumes = vec![FogVolume::Box {
        center: [0.0, 2.0, 4.0],
        extents: [6.0, 3.0, 6.0],
        density: 0.2,
    }];
    state.params.lights = vec![FogLight::point([0.0, 2.0, 4.0], [1.0, 0.95, 0.8], 4000.0, 8.0)];
    let look = look(
        Vec3::new(0.0, 2.0, 10.0),
        Vec3::new(0.0, 2.0, 0.0),
        Vec3::Y,
        50.0_f32.to_radians(),
        width as f32 / height as f32,
        0.3,
        30.0,
    );
    state.ensure_gpu(&device.device, device.shared_pipelines())?;
    let depth = depth_tex(&device.device, 1, 1, "local fog depth");
    let target = color_target(&device.device, width, height);
    let shot = OutdoorComposite {
        prepared_sky: None,
        depth_is_scene: false,
        pre_exposure: 1.0,
        hdr: &target.view,
        depth: &depth.view,
        shadow: None,
        width,
        height,
        format: wgpu::TextureFormat::Rgba8Unorm,
        inv_view_proj: look.inv_view_proj,
        unjittered_inv_view_proj: look.inv_view_proj,
        prev_view_proj: look.view_proj,
        eye: look.eye,
        near: 0.3,
        far: 30.0,
        sun_travel: [0.0, -1.0, 0.0],
        sun_color: [1.0, 1.0, 1.0],
        sun_illuminance: 0.0,
        light_vp: look.view_proj,
        shadow_size: 0.0,
        atmosphere: false,
        fog: true,
        clouds: false,
        unjittered_view_proj: look.view_proj,
        cloud_reprojection: false,
        cloud_step_percent: 100,
        cloud_cone_light: false,
        cloud_march: crate::CloudMarch::REALTIME,
        local_shadow: None,
        fog_light_shadows: [[[0.0; 4]; 6]; 4],
    };
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("local fog"),
        });
    state.draw_fog(
        &device.device,
        &crate::device::TrackedQueue::observe(&device.queue),
        &mut encoder,
        &shot,
        Some(&target.view),
    )?;
    submit(device, encoder)?;
    read_target(device, &target, width, height)
}

fn render_cloud_image(
    device: &RenderDevice,
    settings: &CloudSettings,
    entry: u32,
    sun: [f32; 3],
) -> Result<OutdoorImage, RenderError> {
    let size = settings.map_resolution.max(1);
    let mut state = OutdoorState::default();
    state.params.clouds = *settings;
    state.ensure_gpu(&device.device, device.shared_pipelines())?;
    let look = look(
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, 2.0, 0.0),
        Vec3::Z,
        1.0,
        1.0,
        0.1,
        10.0,
    );
    let depth = depth_tex(&device.device, 1, 1, "cloud depth");
    let target = color_target(&device.device, size, size);
    let shot = OutdoorComposite {
        prepared_sky: None,
        depth_is_scene: false,
        pre_exposure: 1.0,
        hdr: &target.view,
        depth: &depth.view,
        shadow: None,
        width: size,
        height: size,
        format: wgpu::TextureFormat::Rgba8Unorm,
        inv_view_proj: look.inv_view_proj,
        unjittered_inv_view_proj: look.inv_view_proj,
        prev_view_proj: look.view_proj,
        eye: look.eye,
        near: 0.1,
        far: 10.0,
        sun_travel: sun,
        sun_color: [1.0; 3],
        sun_illuminance: 0.0,
        light_vp: look.view_proj,
        shadow_size: 0.0,
        atmosphere: false,
        fog: false,
        clouds: true,
        unjittered_view_proj: look.view_proj,
        cloud_reprojection: false,
        cloud_step_percent: 100,
        cloud_cone_light: false,
        cloud_march: crate::CloudMarch::REALTIME,
        local_shadow: None,
        fog_light_shadows: [[[0.0; 4]; 6]; 4],
    };
    let gpu = state.gpu.as_mut().expect("outdoor gpu");
    let weather = gpu.ensure_weather(
        &device.device,
        &crate::device::TrackedQueue::observe(&device.queue),
        settings,
        None,
    );
    gpu.ensure_cloud_noise(
        &device.device,
        &crate::device::TrackedQueue::observe(&device.queue),
    );
    let mut bytes = cloud_uniform(&shot, settings, 0, 6_360_000.0, None);
    bytes.draw[2] = size as f32;
    device
        .queue
        .write_buffer(&gpu.cloud_buf, 0, bytemuck::bytes_of(&bytes));
    let dummy = gpu.cloud_placeholder();
    let bg = gpu.cloud_bind(
        &device.device,
        &weather,
        &depth.view,
        [&dummy, &dummy, &dummy, &dummy],
    );
    let pipe = gpu.cloud_pipe(&device.device, wgpu::TextureFormat::Rgba8Unorm, entry);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("clouds"),
        });
    draw_pass(&mut encoder, &pipe, &bg, &target.view, "cloud image", true);
    submit(device, encoder)?;
    read_target(device, &target, size, size)
}

/// Top-down view of one weather tile: white where a column is cloudy, blue where clear.
#[tracing::instrument(skip_all)]
pub fn render_cloud_coverage(
    device: &RenderDevice,
    settings: &CloudSettings,
) -> Result<OutdoorImage, RenderError> {
    render_cloud_image(device, settings, 0, [0.0, -1.0, 0.0])
}

/// Ground shaded by the projected cloud column. Noon sun, shadow under the cloud.
#[tracing::instrument(skip_all)]
pub fn render_cloud_ground_shadow(
    device: &RenderDevice,
    settings: &CloudSettings,
) -> Result<OutdoorImage, RenderError> {
    render_cloud_image(device, settings, 1, [0.0, -1.0, 0.0])
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::ContractRow::new(
            "outdoor_sky.wgsl",
            "SkyGpu",
            size_of::<SkyGpu>(),
            vec![
                ("inv_view_proj", std::mem::offset_of!(SkyGpu, inv_view_proj)),
                ("sun", std::mem::offset_of!(SkyGpu, sun)),
                ("camera", std::mem::offset_of!(SkyGpu, camera)),
                ("disk", std::mem::offset_of!(SkyGpu, disk)),
                ("misc", std::mem::offset_of!(SkyGpu, misc)),
                ("body_dir", std::mem::offset_of!(SkyGpu, body_dir)),
                ("body_color", std::mem::offset_of!(SkyGpu, body_color)),
                ("stars", std::mem::offset_of!(SkyGpu, stars)),
                ("moon0_dir", std::mem::offset_of!(SkyGpu, moons)),
                ("moon0_color", std::mem::offset_of!(SkyGpu, moons) + 16),
                ("moon1_dir", std::mem::offset_of!(SkyGpu, moons) + 32),
                ("moon1_color", std::mem::offset_of!(SkyGpu, moons) + 48),
                ("body_tex", std::mem::offset_of!(SkyGpu, body_tex)),
                ("ground", std::mem::offset_of!(SkyGpu, ground)),
                ("table_air", std::mem::offset_of!(SkyGpu, table_air)),
                ("table_ground", std::mem::offset_of!(SkyGpu, table_ground)),
                ("interiors", std::mem::offset_of!(SkyGpu, interiors)),
                ("cloud", std::mem::offset_of!(SkyGpu, cloud)),
                ("cloud_air", std::mem::offset_of!(SkyGpu, cloud_air)),
                ("cloud_ground_sun", std::mem::offset_of!(SkyGpu, cloud_ground_sun)),
                ("cloud_ground_sky", std::mem::offset_of!(SkyGpu, cloud_ground_sky)),
            ],
        ),
        crate::contract_row!(
            "volumetric_fog.wgsl",
            "FogGpu",
            FogGpu {
                inv_view_proj, light_vp, prev_view_proj, camera, prev_camera, sun_travel, sun_color, grid, media, albedo,
                misc, screen, volumes, lights, ambient, interiors, cloud, light_shadow, valley => mist
            }
        ),
        crate::contract_row!(
            "clouds.wgsl",
            "CloudGpu",
            CloudGpu {
                inv_view_proj, sun, sun_color, layer, wind, shape, camera, ambient_top, ambient_bottom, draw,
                targets => target, hdr, ray_from_clip, history_view_proj, temporal, reproject, sun_bottom, sun_top,
                light, stale, look, cirrus, cirrus_sun, cirrus_dir, cirrus_fiber, cirrus_shear, cirrus_crisp
            }
        ),
    ]
}

#[cfg(test)]
mod tests {
    /// The history's weather motion stays raised after a transition ends, until the last frame
    /// marched during it leaves the refresh window, and is 0 for a steady sky.
    #[test]
    fn cloud_history_motion_covers_the_refresh_window() {
        use super::{CLOUD_REFRESH_FRAMES, cloud_history_weather_motion};
        let at = |coverage: f32| crate::clouds::CloudWeatherSample {
            coverage,
            ..crate::CloudSettings::coverage_layer(0.25).weather_sample()
        };
        let steady = [at(0.25); CLOUD_REFRESH_FRAMES];
        assert_eq!(
            cloud_history_weather_motion(&steady, at(0.25)).to_bits(),
            0.0_f32.to_bits()
        );
        // 1/32 of a cut over the window clamps half way; 1/16 or more replaces the history.
        let half = cloud_history_weather_motion(&steady, at(0.25 + 0.05 / 32.0));
        assert!((half - 0.5).abs() < 0.01, "{half}");
        assert_eq!(cloud_history_weather_motion(&steady, at(0.25 + 0.05 / 8.0)), 1.0);
        // A ramp of 0.01 a frame (a fifth of a cut) reached 0.6 on this frame; the window then
        // fills with the held weather one frame at a time, newest first.
        let mut window = [at(0.25); CLOUD_REFRESH_FRAMES];
        for (k, w) in window.iter_mut().enumerate() {
            *w = at(0.59 - 0.01 * k as f32);
        }
        let mut motions = Vec::new();
        for _ in 0..CLOUD_REFRESH_FRAMES {
            motions.push(cloud_history_weather_motion(&window, at(0.6)));
            window.rotate_right(1);
            window[0] = at(0.6);
        }
        assert_eq!(motions[0], 1.0, "the window holds 16 frames of ramp: {motions:?}");
        assert!(motions.windows(2).all(|m| m[1] <= m[0]), "{motions:?}");
        assert!(motions[CLOUD_REFRESH_FRAMES - 1] > 0.0, "{motions:?}");
        assert_eq!(
            cloud_history_weather_motion(&window, at(0.6)).to_bits(),
            0.0_f32.to_bits()
        );
        // The previous frame's weather alone read a fifth of a cut, then 0 on the first held frame.
        let last_step = crate::clouds::cloud_weather_motion(at(0.59), at(0.6));
        assert!(last_step < 0.25, "{last_step}");
    }

    /// ADR-1410: after a transition, the sub-positions marched since the weather settled read
    /// 0 and only those last marched before it read stale; each of the 16 is set once a frame,
    /// at every frame of the Bayer cycle, and steady weather reads exactly 0 everywhere.
    #[test]
    fn sub_position_staleness_follows_each_texels_last_march() {
        use super::{CLOUD_REFRESH_FRAMES, cloud_sub_staleness};
        let at = |coverage: f32| crate::clouds::CloudWeatherSample {
            coverage,
            ..crate::CloudSettings::coverage_layer(0.25).weather_sample()
        };
        let steady = [at(0.6); CLOUD_REFRESH_FRAMES];
        for frame in [0u32, 5, 16, 1_000_003, u32::MAX] {
            let stale = cloud_sub_staleness(&steady, at(0.6), frame);
            assert!(stale.iter().flatten().all(|&k| k.to_bits() == 0.0_f32.to_bits()));
        }
        // The weather settled at 0.6 `settled` frames ago; before that it was a whole cut off.
        for settled in 0..CLOUD_REFRESH_FRAMES {
            let mut window = [at(0.55); CLOUD_REFRESH_FRAMES];
            window[..settled].fill(at(0.6));
            for frame in 100u32..116 {
                let stale = cloud_sub_staleness(&window, at(0.6), frame);
                let fresh: Vec<[u32; 2]> = (1..=settled as u32)
                    .map(|age| crate::clouds::cloud_bayer_offset(frame - age))
                    .collect();
                for (y, row) in stale.iter().enumerate() {
                    for (x, &k) in row.iter().enumerate() {
                        let marched_since = fresh.contains(&[x as u32, y as u32]);
                        let want = if marched_since { 0.0 } else { 1.0 };
                        assert_eq!(k, want, "settled {settled} frame {frame} sub ({x}, {y})");
                    }
                }
            }
        }
        // This frame's own sub-position reads its march 16 frames ago.
        let mut window = [at(0.6); CLOUD_REFRESH_FRAMES];
        window[CLOUD_REFRESH_FRAMES - 1] = at(0.55);
        let [x, y] = crate::clouds::cloud_bayer_offset(40);
        let stale = cloud_sub_staleness(&window, at(0.6), 40);
        assert_eq!(stale[y as usize][x as usize], 1.0);
        assert_eq!(stale.iter().flatten().filter(|&&k| k > 0.0).count(), 1);
    }

    #[test]
    fn row_padding_uses_wide_arithmetic() {
        // 256-byte alignment: 3 texels (12 bytes) pad to 256 and keep their rows.
        let (padded, row) = super::pad_rows(&[7u8; 24], 3, 2);
        assert_eq!((padded.len(), row), (512, 256));
        assert_eq!(padded[0], 7);
        assert_eq!(padded[256], 7);
        // A row that cannot be aligned inside u32 is refused with an empty buffer.
        let (none, row) = super::pad_rows_bytes(&[], u32::MAX, 4);
        assert!(none.is_empty());
        assert_eq!(row, u32::MAX);
    }

    use super::*;
    use regia_scene::level::{CLEAR_DAY_AEROSOL_TURBIDITY, LevelAtmosphere, TODAY_AEROSOL_TURBIDITY};

    fn apply(level: &LevelAtmosphere) -> OutdoorParams {
        let mut params = OutdoorParams::default();
        params.apply_level(level, false);
        params
    }

    fn cloudy(preset: AtmospherePreset, overcast_ibl: bool) -> OutdoorParams {
        OutdoorParams {
            planet: AtmosphereParameters::from_preset(preset),
            clouds: CloudSettings {
                overcast_ibl,
                ..CloudSettings::coverage_layer(0.8)
            },
            ..OutdoorParams::default()
        }
    }

    /// ADR-321: the overcast dims the sky light only on an Earth sky with the switch on; an
    /// airless or Mars sky, or the switch off, measures 0 and bakes the clear IBL.
    #[test]
    fn overcast_applies_only_to_earth_with_the_switch() {
        let mut state = OutdoorState::default();
        state.set_params(cloudy(AtmospherePreset::Earth, true));
        assert!(state.params().overcast_applies());
        let earth = state.overcast;
        assert!(earth > 0.1 && earth <= 1.0, "an 80 % layer is overcast, {earth}");
        for (preset, switch) in [
            (AtmospherePreset::Earth, false),
            (AtmospherePreset::Mars, true),
            (AtmospherePreset::Airless, true),
        ] {
            state.set_params(cloudy(preset, switch));
            assert!(!state.params().overcast_applies(), "{preset:?} switch {switch}");
            assert_eq!(state.overcast, 0.0, "{preset:?} switch {switch}");
        }
        // The cached factor comes back for the same layer.
        state.set_params(cloudy(AtmospherePreset::Earth, true));
        assert_eq!(state.overcast, earth);
    }

    fn small_overcast(coverage: f32, bottom: f32, top: f32, ibl: bool) -> OutdoorParams {
        OutdoorParams {
            planet: AtmosphereParameters::from_preset(AtmospherePreset::Earth),
            clouds: CloudSettings {
                overcast_ibl: ibl,
                coverage,
                altitude_bottom: bottom,
                altitude_top: top,
                map_resolution: 8,
                pattern: crate::clouds::CloudPattern::Constant,
                ..CloudSettings::default()
            },
            ..OutdoorParams::default()
        }
    }

    /// An infinite density does not share the cached factor of density 0: the key reads the
    /// density the factor does (corr-low-render, corr-render-rest finding 19).
    #[test]
    fn an_infinite_density_is_not_cached_as_a_clear_deck() {
        let mut opaque = small_overcast(0.5, 1_500.0, 2_500.0, false);
        opaque.clouds.density = f32::INFINITY;
        let mut fresh = OutdoorState::default();
        fresh.set_params(opaque.clone());
        let want = fresh.layer_overcast();
        let mut clear = small_overcast(0.5, 1_500.0, 2_500.0, false);
        clear.clouds.density = 0.0;
        let mut state = OutdoorState::default();
        state.set_params(clear);
        let zero = state.layer_overcast();
        state.set_params(opaque);
        let got = state.layer_overcast();
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "after density 0 ({zero}): {got}, fresh {want}"
        );
        assert!(want > zero, "an opaque deck {want} over a clear one {zero}");
    }

    /// Altitude is not an input of the weather RGBA. Two frames that differ only there must not
    /// pack another map. Coverage uploads on the driver's 1/256 step and not on a smaller nudge.
    #[test]
    fn altitude_does_not_repack_the_overcast_map() {
        let mut state = OutdoorState::default();
        let mut params = small_overcast(0.5, 1_500.0, 2_500.0, true);
        let before = crate::clouds::cloud_work_counters();
        state.set_params(params.clone());
        assert_eq!(
            crate::clouds::cloud_work_counters().since(before).weather_maps,
            1,
            "the first overcast measure packs one map"
        );
        let factor = state.overcast;

        params.clouds.altitude_bottom += 80.0;
        params.clouds.altitude_top += 80.0;
        let before = crate::clouds::cloud_work_counters();
        state.set_params(params.clone());
        assert_eq!(
            crate::clouds::cloud_work_counters().since(before).weather_maps,
            0,
            "a pure altitude shift must not pack a weather map"
        );
        assert_eq!(state.overcast.to_bits(), factor.to_bits());

        params.clouds.altitude_top += 400.0;
        let before = crate::clouds::cloud_work_counters();
        state.set_params(params.clone());
        assert_eq!(
            crate::clouds::cloud_work_counters().since(before).weather_maps,
            0,
            "thickness changes the scalar, not the RGBA"
        );
        assert_ne!(state.overcast.to_bits(), factor.to_bits());

        // Table air reads the same measure with the IBL switch off.
        let mut bare = small_overcast(0.5, 1_500.0, 2_500.0, false);
        state.set_params(bare.clone());
        let before = crate::clouds::cloud_work_counters();
        let deck = state.layer_overcast();
        assert!(deck > 0.0);
        let packed = crate::clouds::cloud_work_counters().since(before).weather_maps;
        assert!(
            packed <= 1,
            "the deck measure packs at most once, packed {packed}"
        );
        bare.clouds.altitude_bottom += 40.0;
        bare.clouds.altitude_top += 40.0;
        state.set_params(bare);
        let before = crate::clouds::cloud_work_counters();
        assert_eq!(state.layer_overcast().to_bits(), deck.to_bits());
        assert_eq!(
            crate::clouds::cloud_work_counters().since(before).weather_maps,
            0,
            "table air must not repack on altitude"
        );
    }

    #[test]
    fn coverage_uploads_on_the_weather_step_and_not_on_a_nudge() {
        let mut state = OutdoorState::default();
        let mut params = small_overcast(0.5, 1_500.0, 2_500.0, true);
        state.set_params(params.clone());
        let key = weather_key(&params.clouds);

        params.clouds.coverage += 1.0 / 1024.0;
        assert_eq!(
            weather_key(&params.clouds),
            key,
            "a coverage nudge below 1/256 must not change the upload key"
        );
        let before = crate::clouds::cloud_work_counters();
        state.set_params(params.clone());
        assert_eq!(
            crate::clouds::cloud_work_counters().since(before).weather_maps,
            0,
            "a sub-step must not pack or upload"
        );

        params.clouds.coverage = 0.5 + 1.0 / 256.0;
        assert_ne!(
            weather_key(&params.clouds),
            key,
            "one 1/256 step changes the upload key"
        );
        let before = crate::clouds::cloud_work_counters();
        state.set_params(params);
        assert_eq!(
            crate::clouds::cloud_work_counters().since(before).weather_maps,
            1,
            "one coverage step packs (and therefore uploads) once"
        );
    }

    /// Overcast factor 0 is the clear bake bit for bit, in every direction; a factor dims it.
    #[test]
    fn overcast_zero_leaves_the_ibl_unchanged() {
        let luts = crate::atmosphere::build_atmosphere_luts([0.3, 0.7, 0.2], 2.0);
        let ground = ground_radiance(&luts);
        let mut dimmer = 0;
        for i in 0..64 {
            let dir = dir_from_equirect((i as f32 + 0.5) / 64.0, ((i * 7) % 32) as f32 / 32.0 + 0.01);
            let clear = environment_radiance(&luts, ground, dir);
            let zero = overcast_environment(&luts, ground, dir, 0.0);
            assert_eq!(
                clear.map(f32::to_bits),
                zero.map(f32::to_bits),
                "factor 0 at {dir:?}"
            );
            let grey = overcast_environment(&luts, ground, dir, 0.7);
            let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
            if luma(grey) < luma(clear) {
                dimmer += 1;
            }
        }
        assert!(
            dimmer >= 30,
            "an overcast dims the sky light, {dimmer} of 64 directions"
        );
        let clear = bake_ibl(&luts, 0.0, None);
        let zero = bake_ibl(&luts, 0.0, None);
        assert_eq!(clear.irradiance, zero.irradiance, "the bake is deterministic");
        assert_ne!(bake_ibl(&luts, 0.5, None).irradiance, clear.irradiance);
        assert_ne!(
            ibl_key([0.0, 1.0, 0.0], 2.0, 0.0, None),
            ibl_key([0.0, 1.0, 0.0], 2.0, 0.5, None)
        );
    }

    /// ADR-5944: the image light's ground follows the cover only with the cloud map and the
    /// overcast sky light both on (Earth): the map alone leaves the image light as it was, so a
    /// texture view lit by it renders as with the map off (`cloud_shadow.rs`), and so does the
    /// overcast sky light alone (ADR-321 keeps the ground bounce there).
    #[test]
    fn the_ground_cover_needs_the_map_and_the_overcast_sky_light() {
        let with = |shadow_map: bool, overcast_ibl: bool, preset: AtmospherePreset| {
            let mut params = cloudy(preset, overcast_ibl);
            params.clouds.shadow_map = shadow_map;
            let mut state = OutdoorState::default();
            state.set_params(params);
            state.ground_cover
        };
        let both = with(true, true, AtmospherePreset::Earth);
        assert!(both.is_some_and(|cover| cover > 0.1 && cover <= 1.0), "{both:?}");
        assert_eq!(with(true, false, AtmospherePreset::Earth), None, "the map alone");
        assert_eq!(
            with(false, true, AtmospherePreset::Earth),
            None,
            "the overcast sky light alone"
        );
        assert_eq!(with(false, false, AtmospherePreset::Earth), None);
        assert_eq!(
            with(true, true, AtmospherePreset::Mars),
            None,
            "the overcast sky light is Earth's"
        );
    }

    /// ADR-5944: the terrain's hemisphere ambient takes its ground half from the ground the
    /// image light shows. With the cloud map and the overcast sky light both on that is the
    /// shadowed ground (`ground_radiance_under`); with either alone, or neither, the sunlit one,
    /// as before. Before, the ground half stayed sunlit under any cover, and a tree or a slope
    /// on the terrain took a sunlit ground's bounce there.
    #[test]
    fn the_hemisphere_ambients_ground_follows_the_cover_with_the_image_lights() {
        let luts = crate::atmosphere::build_atmosphere_luts([0.3, 0.7, 0.2], 2.0);
        let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let ground_of = |shadow_map: bool, overcast_ibl: bool| {
            let mut params = cloudy(AtmospherePreset::Earth, overcast_ibl);
            params.clouds.shadow_map = shadow_map;
            let mut state = OutdoorState::default();
            state.set_params(params);
            state.luts = Some(luts.clone());
            let [_, ground] = state
                .hemisphere_ambient()
                .unwrap_or_else(|| panic!("no hemisphere ambient with tables"));
            (ground, state.overcast, state.ground_cover)
        };
        let clear = ground_radiance(&luts);
        for (shadow_map, overcast_ibl) in [(false, false), (true, false), (false, true)] {
            let (ground, _, cover) = ground_of(shadow_map, overcast_ibl);
            assert_eq!(cover, None);
            assert_eq!(
                ground, clear,
                "map {shadow_map}, overcast sky light {overcast_ibl}: the sunlit ground, as before"
            );
        }
        let (ground, overcast, cover) = ground_of(true, true);
        let cover = cover.unwrap_or_else(|| panic!("the cover applies with both switches"));
        assert_eq!(
            ground,
            ground_radiance_under(&luts, overcast, cover),
            "the hemisphere ambient's ground is not the image light's under the cover"
        );
        assert!(
            luma(ground) < luma(clear),
            "under the cover the bounce is the shadowed ground's: {} against {}",
            luma(ground),
            luma(clear)
        );
    }

    /// ADR-5944 item 9: on a frame whose surfaces read the cloud transmittance map the GI
    /// probes' ray hits keep the share of the sun the layer leaves on the ground on average;
    /// on every other frame exactly 1, so the probes' light is the frame's, bit for bit.
    #[test]
    fn the_probes_sun_follows_the_layers_cover_under_the_map() {
        let state_of = |shadow_map: bool, overcast_ibl: bool, live: bool, coverage: f32| {
            let mut params = small_overcast(coverage, 1_500.0, 4_000.0, overcast_ibl);
            params.clouds.shadow_map = shadow_map;
            let mut state = OutdoorState::default();
            state.set_params(params);
            // What `plan_cloud_shadow_map` records when the map pass is planned for the frame.
            state.cloud_map_live = live;
            state
        };
        // The map off, or not planned this frame (a tier that draws no clouds, a texture view):
        // the sun as it is.
        assert_eq!(state_of(false, false, true, 1.0).probe_sun_share(), 1.0);
        assert_eq!(state_of(false, true, true, 1.0).probe_sun_share(), 1.0);
        assert_eq!(state_of(true, false, false, 1.0).probe_sun_share(), 1.0);
        // No layer: the map is not planned, and a planned one would hold no cloud.
        assert_eq!(state_of(true, false, true, 0.0).probe_sun_share(), 1.0);
        // The map alone is enough (the surfaces lose their sun under it without the overcast
        // sky light too): a closed deck leaves the hits almost none of the sun, a thinner cover
        // more of it.
        let closed = state_of(true, false, true, 1.0).probe_sun_share();
        assert!(
            closed < 0.05,
            "a closed deck leaves the probes {closed} of the sun"
        );
        // Nothing between none and the floor: a share that small would move by a large factor
        // from frame to frame as a cover closes, and the probes read that as a light cut.
        assert!(
            closed == 0.0 || closed >= PROBE_SUN_SHARE_FLOOR,
            "a share under the floor ({closed})"
        );
        let half = state_of(true, false, true, 0.5).probe_sun_share();
        assert!(
            half > closed + 0.2 && half < 1.0,
            "half a cover leaves the probes {half} of the sun, a closed deck {closed}"
        );
        // With the overcast sky light on as well it is the share the image light's ground keeps.
        let mut both = state_of(true, true, true, 0.5);
        let cover = both
            .ground_cover
            .unwrap_or_else(|| panic!("the ground cover applies with both switches"));
        assert_eq!(both.probe_sun_share(), 1.0 - cover);
    }

    /// ADR-5944: with the cloud transmittance map shadowing the ground, the image light's
    /// ground is that shadowed ground: a layer that takes all of the sun leaves it the sky's
    /// share of its light, one that takes none leaves it whole, and the overcast rule dims the
    /// sky's share. The sky above the horizon is not touched, and without the map (`None`) the
    /// bake is the old one.
    #[test]
    fn the_image_lights_ground_follows_the_cover_when_the_map_shadows_it() {
        let luts = crate::atmosphere::build_atmosphere_luts([0.3, 0.7, 0.2], 2.0);
        let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let clear = ground_radiance(&luts);
        let open = ground_radiance_under(&luts, 0.0, 0.0);
        for c in 0..3 {
            assert!(
                (open[c] - clear[c]).abs() <= 1.0e-5 * clear[c],
                "no cover, the same ground"
            );
        }
        let (sun, sky) = crate::atmosphere::ground_light(&luts);
        let closed = ground_radiance_under(&luts, 0.0, 1.0);
        let sky_share = luma(sky) / (luma(sun) + luma(sky));
        assert!(
            (luma(closed) / luma(clear) - sky_share).abs() < 0.02,
            "a closed cover leaves the ground the sky's share: {} against {sky_share}",
            luma(closed) / luma(clear)
        );
        assert!(
            sky_share < 0.5,
            "the sun carries most of a clear day's ground light"
        );
        let half = ground_radiance_under(&luts, 0.0, 0.5);
        assert!(luma(closed) < luma(half) && luma(half) < luma(clear));
        assert!(
            luma(ground_radiance_under(&luts, 0.8, 1.0)) < luma(closed),
            "the overcast dims the sky's share"
        );
        // The bake: the map off is the old bake; the key tells the three states apart.
        let old = bake_ibl(&luts, 0.6, None);
        let covered = bake_ibl(&luts, 0.6, Some(0.9));
        assert_ne!(covered.irradiance, old.irradiance);
        assert_eq!(bake_ibl(&luts, 0.6, None).irradiance, old.irradiance);
        let key = |cover| ibl_key([0.0, 1.0, 0.0], 2.0, 0.6, cover);
        assert_ne!(key(None), key(Some(0.0)));
        assert_ne!(key(Some(0.0)), key(Some(0.9)));
        // Straight down the image light shows the ground alone: it falls with the cover. Straight
        // up it shows the sky alone: the cover's ground term changes nothing there.
        let ground_old = overcast_environment(&luts, ground_radiance(&luts), [0.0, -1.0, 0.0], 0.6);
        let ground_new = overcast_environment(
            &luts,
            ground_radiance_under(&luts, 0.6, 0.9),
            [0.0, -1.0, 0.0],
            0.6,
        );
        assert!(luma(ground_new) < 0.5 * luma(ground_old));
        let up = [0.0, 1.0, 0.0];
        assert_eq!(
            overcast_environment(&luts, ground_radiance(&luts), up, 0.6),
            overcast_environment(&luts, ground_radiance_under(&luts, 0.6, 0.9), up, 0.6)
        );
    }

    /// The map keys: the wind clock does not change the content key. Since ADR-429 neither do
    /// the wind velocity (its drift is checked frame to frame, and a drift jump of a texel
    /// marches the whole map) nor the weather values a transition moves (the band refresh
    /// follows them; a weather cut drops the window). The layer's altitudes, the weather map's
    /// layout and the cone light switch (it moves the curl displacement) do.
    #[test]
    fn cloud_map_key_ignores_the_clock_but_not_the_layer() {
        let base = CloudSettings {
            shadow_map: true,
            ..CloudSettings::coverage_layer(0.5)
        };
        let key = cloud_map_key(&base, true);
        let later = CloudSettings { time_s: 90.0, ..base };
        assert_eq!(cloud_map_key(&later, true), key);
        let windy = CloudSettings {
            wind_m_per_s: [12.0, 0.0],
            ..base
        };
        assert_eq!(cloud_map_key(&windy, true), key);
        let wetter = CloudSettings {
            coverage: 0.52,
            density: 0.045,
            ..base
        };
        assert_eq!(cloud_map_key(&wetter, true), key);
        let higher = CloudSettings {
            altitude_bottom: 1600.0,
            ..base
        };
        assert_ne!(cloud_map_key(&higher, true), key);
        let reseeded = CloudSettings { seed: 8, ..base };
        assert_ne!(cloud_map_key(&reseeded, true), key);
        assert_ne!(cloud_map_key(&base, false), key);
        let fog_too = CloudSettings {
            fog_coupling: true,
            ..base
        };
        assert_eq!(
            cloud_map_key(&fog_too, true),
            key,
            "the switches do not change the content"
        );
        // The forward parameters are zero unless the sun-term switch is on.
        assert_eq!(cloud_shadow_params(&fog_too, false), [0.0; 4]);
        let on = cloud_shadow_params(&base, true);
        assert_eq!(on[3], 1.0);
        assert_eq!(on[1], base.altitude_bottom);
    }

    /// ADR-162: an airless sky leaves rays that miss the scene black by default; air keeps
    /// the planet surface; `planet_surface` overrides either way.
    #[test]
    fn the_planet_surface_composite_is_off_on_an_airless_world_by_default() {
        let earth = apply(&LevelAtmosphere::default());
        let mars = apply(&LevelAtmosphere {
            preset: AtmospherePreset::Mars,
            ..LevelAtmosphere::default()
        });
        let europa = apply(&LevelAtmosphere {
            preset: AtmospherePreset::Airless,
            ..LevelAtmosphere::default()
        });
        assert!(earth.shows_planet_surface() && mars.shows_planet_surface());
        assert!(!europa.shows_planet_surface(), "airless default must stay space");
        let forced = OutdoorParams {
            planet_surface: Some(true),
            ..europa.clone()
        };
        assert!(forced.shows_planet_surface());
        let off = OutdoorParams {
            planet_surface: Some(false),
            ..earth.clone()
        };
        assert!(!off.shows_planet_surface());

        // The uniform: radius 0 (no hit test) when off, the bottom radius when on.
        let sun = [0.0, 0.6, 0.8];
        let ice = crate::atmosphere::build_atmosphere_luts_for(&europa.planet, sun, 59.0);
        let ground = |on: bool| {
            sky_uniform(
                Some(&ice),
                None,
                &[],
                false,
                [false, false],
                on,
                [[0.0; 4]; 4],
                [0.0; 3],
                8,
                8,
                SkyMode::Hdr { pre_exposure: 1.0 },
            )
            .ground
        };
        assert_eq!(ground(false), [0.0; 4]);
        let lit = ground(true);
        assert!((lit[3] - europa.planet.bottom_radius_km).abs() < 1.0e-3);
        assert!(lit[0] > 0.0, "the forced airless surface is lit: {lit:?}");
    }

    /// An explicit cloud resource still loses its coverage on an airless planet, and the
    /// march predicate (`draws_anything && !airless`) stays false.
    #[test]
    fn airless_explicit_clouds_are_forced_off() {
        let mut params = OutdoorParams {
            clouds: CloudSettings {
                coverage: 0.45,
                ..CloudSettings::default()
            },
            ..OutdoorParams::default()
        };
        assert!(params.clouds.draws_anything());
        params.apply_level(
            &LevelAtmosphere {
                preset: AtmospherePreset::Airless,
                cloud_coverage: 0.9,
                ..LevelAtmosphere::default()
            },
            true,
        );
        assert_eq!(params.clouds.coverage, 0.0);
        assert!(params.planet.is_airless());
        assert!(!params.clouds.draws_anything());
        let march = params.clouds.draws_anything() && !params.planet.is_airless();
        assert!(!march);
    }

    /// `apply_sun` with a fixed sun toward `[elev_y, 1, 0]` (unit), async sky off so every
    /// rebuild beyond the first tables is an inline bake the frame-path counters see.
    fn sun_at(state: &mut super::OutdoorState, eye_y: f32, elev_y: f32) {
        let toward = super::unit([elev_y, 1.0, 0.0]);
        let dir = [-toward[0], -toward[1], -toward[2]];
        let _ = state.apply_sun(
            true,
            crate::DirectionalLightUniform::new(dir, [1.0; 3], 1.0),
            eye_y,
        );
    }

    #[test]
    fn camera_height_drift_under_the_hysteresis_keeps_the_sky_tables() {
        // A driving camera crossing and recrossing a 10 m bucket edge must not re-key the
        // atmosphere tables: within [`SKY_HEIGHT_REBUILD_M`] of the built height they stay.
        let mut state = super::OutdoorState::default();
        sun_at(&mut state, 12.0, 0.0);
        let built = state.luts.as_ref().expect("first tables").camera_height_m;
        assert_eq!(built, 12.0, "the first tables build at the exact eye height");
        let before = crate::frame_stats::counters().inline_bakes;
        sun_at(&mut state, 12.05, 0.0); // still the same 10 m bucket
        sun_at(&mut state, 14.0, 0.0); // 2 m of drift: still no rebuild
        sun_at(&mut state, 21.9, 0.0); // 9.9 m of drift: still no rebuild
        let after = crate::frame_stats::counters().inline_bakes;
        assert_eq!(after - before, 0, "no inline bake within the hysteresis");
        assert_eq!(
            state.luts.as_ref().expect("tables").camera_height_m,
            12.0,
            "the live tables are the ones built at 12 m"
        );
        sun_at(&mut state, 22.1, 0.0); // 10.1 m of drift: rebuild
        let after = crate::frame_stats::counters().inline_bakes;
        assert_eq!(after - before, 1, "one inline bake past the hysteresis");
        assert_eq!(state.luts.as_ref().expect("tables").camera_height_m, 22.1);
    }

    /// Async sky: a worker started past the hysteresis must not replace the live tables once
    /// the camera has come back inside [`super::SKY_HEIGHT_REBUILD_M`] of them. The worker's
    /// bucket can match the bucket the camera is in now, and `poll_sky_job` used to install it.
    #[test]
    fn audfix_hysteresis_drops_a_sky_job_the_camera_has_left() {
        let mut state = super::OutdoorState::default();
        sun_at(&mut state, 12.0, 0.0);
        assert_eq!(state.luts.as_ref().expect("tables").camera_height_m, 12.0);
        state.set_async_sky(true);
        sun_at(&mut state, 22.1, 0.0);
        assert!(state.sky_busy(), "the drift past 10 m started a worker");
        assert_eq!(
            state.luts.as_ref().expect("tables").camera_height_m,
            12.0,
            "the frame keeps the live tables while the worker runs"
        );
        let mut spins = 0u32;
        while state.sky_busy() {
            std::thread::yield_now();
            sun_at(&mut state, 14.0, 0.0);
            spins += 1;
            assert!(spins < 10_000_000, "sky worker did not finish");
        }
        assert_eq!(
            state.luts.as_ref().expect("tables").camera_height_m,
            12.0,
            "a finished worker for 22.1 m must not replace tables the camera is back inside"
        );
    }

    #[test]
    fn a_moved_sun_rebuilds_the_sky_tables_regardless_of_height() {
        // The hysteresis only covers camera height: a changed sun elevation re-keys
        // immediately (the sky-view table turns with the sun).
        let mut state = super::OutdoorState::default();
        sun_at(&mut state, 12.0, 0.0);
        let before = crate::frame_stats::counters().inline_bakes;
        sun_at(&mut state, 12.2, 0.05);
        let after = crate::frame_stats::counters().inline_bakes;
        assert_eq!(after - before, 1, "the sun moved, the tables rebuilt");
    }

    #[test]
    fn a_held_camera_builds_tables_at_its_exact_height() {
        // Still frames (goldens, plates, held views) build at the exact eye height: the
        // hysteresis changes nothing when nothing moves.
        let mut state = super::OutdoorState::default();
        sun_at(&mut state, 21.0, 0.0);
        sun_at(&mut state, 21.0, 0.0);
        let luts = state.luts.as_ref().expect("tables");
        assert_eq!(luts.camera_height_m, 21.0);
        let key = state.lut_key.expect("key");
        assert_eq!(key.1, 2, "the key's height bucket rounds 21 m to 2 dam");
    }

    #[test]
    fn apply_level_feeds_turbidity_into_the_earth_mie_density() {
        let clean = apply(&LevelAtmosphere::default());
        // A fresh Earth level is a clear day, not the near-clean baseline.
        assert!((clean.planet.aerosol_optical_depth() - CLEAR_DAY_AEROSOL_TURBIDITY).abs() < 1.0e-6);
        assert!(clean.is_earth(), "a turbid Earth is still Earth's medium");

        // An existing level that omits the field keeps the published baseline exactly.
        let existing = apply(&LevelAtmosphere {
            aerosol_turbidity: TODAY_AEROSOL_TURBIDITY,
            ..LevelAtmosphere::default()
        });
        assert_eq!(existing.planet, AtmosphereParameters::earth());

        // Turbidity is ignored on a non-Earth preset.
        let mars = apply(&LevelAtmosphere {
            preset: AtmospherePreset::Mars,
            aerosol_turbidity: 0.5,
            ..LevelAtmosphere::default()
        });
        assert_eq!(mars.planet, AtmosphereParameters::mars());
        assert!(!mars.is_earth());
    }

    /// Star flux (ADR-2486): 1080p is unchanged, 4K gives a pixel-wide star four times the brightness (the light
    /// a star twice as wide had), and the flag starts off.
    #[test]
    fn star_flux_gain_keeps_a_star_s_light_across_heights() {
        use super::{OutdoorState, star_flux_gain};
        assert!((star_flux_gain(1080) - 1.0).abs() < 1e-6);
        assert!((star_flux_gain(2160) - 4.0).abs() < 1e-6);
        assert!((star_flux_gain(1440) - (1440.0f32 / 1080.0).powi(2)).abs() < 1e-6);
        assert!(star_flux_gain(0) > 0.0);
        let mut state = OutdoorState::default();
        assert!(!state.star_flux());
        state.set_star_flux(true);
        assert!(state.star_flux());
    }

    /// The chord pixel angle matches the field of view over the height at every resolution; the `acos` form drifts
    /// at 4K and beyond (ADR-2486: why star flux swaps it in).
    #[test]
    fn the_chord_pixel_angle_holds_at_4k_where_acos_does_not() {
        use super::{pixel_angle, pixel_angle_chord};
        use regia_core::glam::Vec3;
        use regia_core::glam::camera::rh::proj::directx::perspective;
        use regia_core::glam::camera::rh::view::look_at_mat4;
        let fov = 45f32.to_radians();
        let mut acos_worst = 0.0f32;
        for height in [720u32, 1080, 1440, 2160, 4320] {
            let proj = perspective(fov, 16.0 / 9.0, 0.3, 20000.0);
            let view = look_at_mat4(
                Vec3::new(0.0, 420.0, 0.0),
                Vec3::new(0.0, 560.0, -1000.0),
                Vec3::Y,
            );
            let inv = (proj * view).inverse().to_cols_array_2d();
            // Small-angle: one row at the centre spans about (2 tan(fov/2) / height) radians.
            let truth = 2.0 * (0.5 * fov).tan() / height as f32;
            let chord = pixel_angle_chord(inv, height);
            assert!(
                (chord / truth - 1.0).abs() < 0.01,
                "{height}: chord {chord} vs {truth}"
            );
            acos_worst = acos_worst.max((pixel_angle(inv, height) / truth - 1.0).abs());
        }
        // Not asserted as a bound on the old form, only that it is the worse of the two at these sizes.
        assert!(
            acos_worst > 0.01,
            "the acos form stays within 1 % ({acos_worst}); revisit ADR-2486"
        );
    }

    /// ADR-3294: the modes map to their entries and blends, and a frame takes the mode only when
    /// the profile asks and the frame draws the tables.
    #[test]
    fn table_air_entries_blends_and_frame_modes() {
        use super::{TableAir, VEIL_ENTRY, blends, sky_entry};
        use crate::{FrameQuality, RenderProfile, RenderProfilePreset};
        assert_eq!(
            TableAir::Off.sky_entries(),
            (1, 2),
            "the default keeps its entries"
        );
        assert_eq!(TableAir::Full.sky_entries(), (1, 4));
        assert_eq!(TableAir::Lite.sky_entries(), (1, 5));
        assert_eq!(sky_entry(2), "fs_aerial");
        assert_eq!(sky_entry(4), "fs_aerial_tables");
        assert_eq!(sky_entry(5), "fs_aerial_tables_lite");
        assert_eq!(sky_entry(VEIL_ENTRY), "fs_weather_veil");
        for entry in [2, 4, 5, VEIL_ENTRY] {
            assert!(blends(entry, 0).is_some(), "entry {entry} blends over the image");
        }
        assert!(blends(1, 0).is_none() && blends(0, 0).is_none());
        assert_eq!(TableAir::Full.aerial_fetches(), (8, 12));
        assert_eq!(TableAir::Lite.aerial_fetches(), (4, 8));
        for preset in RenderProfilePreset::WITH_OFFLINE {
            let mut profile = RenderProfile::from_preset(preset);
            assert!(
                !profile.aerial_from_tables,
                "{preset:?} ships with the switch off"
            );
            let quality = FrameQuality::from_profile(&profile);
            assert_eq!(TableAir::for_frame(&quality, true), TableAir::Off);
            profile.aerial_from_tables = true;
            let quality = FrameQuality::from_profile(&profile);
            assert_eq!(
                TableAir::for_frame(&quality, false),
                TableAir::Off,
                "no tables, no mode"
            );
            let lite = quality.forward_lite != crate::forward_lite::ForwardLite::Off;
            let expect = if lite { TableAir::Lite } else { TableAir::Full };
            assert_eq!(TableAir::for_frame(&quality, true), expect, "{preset:?}");
        }
        const { assert!(!FrameQuality::LAB.aerial_from_tables && !FrameQuality::EDITOR.aerial_from_tables) };
        // The shader's deck rule uses `overcast_sky`'s dimming.
        let wgsl = include_str!("../shaders/outdoor_sky.wgsl");
        let line = format!("const TABLE_OVERCAST_DIM: f32 = {:?};", crate::CLOUD_OVERCAST_DIM);
        assert!(wgsl.contains(&line), "outdoor_sky.wgsl must declare `{line}`");
    }

    /// audit-g-render3-determinism 2: while an async sky worker integrates the tables for
    /// a new sun-elevation/height bucket, the installed tables keep the sun direction they
    /// were built for. Overwriting `sun_direction` early pairs the old sky-view and aerial
    /// tables with a new sun for the worker's wall time, so two machines polling at
    /// different times present different skies for the same sun step.
    #[test]
    fn async_sky_holds_sky_view_until_the_worker_installs() {
        use crate::atmosphere::{AtmosphereBase, AtmosphereParameters, build_atmosphere_luts_from};

        let planet = AtmosphereParameters::default();
        let mut state = super::OutdoorState::default();
        state.set_async_sky(true);
        // The first tables of a planet build inline; y = 0.1004 rounds to elevation bucket 100.
        let toward_old = [0.0, 0.1004_f32, 1.0];
        state.ensure_luts(&planet, toward_old, 0.0);
        let Some(before) = state.luts.as_ref() else {
            panic!("the first tables build inline");
        };
        let generation_before = state.lut_generation;
        let sky_view_before = before.sky_view.texels.clone();
        let sun_before = before.sun_direction;

        // The next frame's sun steps across one `lut_key` boundary (bucket 102): the async
        // path hands the rebuild to a worker instead of rebuilding inline.
        let toward_new = [0.0, 0.1016_f32, 1.0];
        state.ensure_luts(&planet, toward_new, 0.0);
        let Some(luts) = state.luts.as_ref() else {
            panic!("the installed tables stay until the worker's land");
        };
        assert!(
            state.sky_job.is_some(),
            "the async path must take the rebuild to a worker"
        );
        assert_eq!(state.lut_generation, generation_before, "no tables installed yet");
        assert_eq!(
            luts.sky_view.texels, sky_view_before,
            "the old sky-view table stays"
        );
        assert_eq!(
            luts.sun_direction, sun_before,
            "the sun direction stays with the tables it was built for"
        );

        // Settling (captures, `set_complete_frames`) installs the worker's tables: an
        // inline build of the new sun, nothing of the old key left.
        state.settle_sky();
        let expected = build_atmosphere_luts_from(&AtmosphereBase::new(&planet), toward_new, 0.0);
        let Some(installed) = state.luts.as_ref() else {
            panic!("settle_sky installs the worker's tables");
        };
        assert_eq!(installed.sun_direction, expected.sun_direction);
        assert_eq!(installed.camera_height_m, expected.camera_height_m);
        assert_eq!(installed.sky_view.texels, expected.sky_view.texels);
    }

    /// `apply_sun(true)` integrates the planet, including Mars and an airless body on a
    /// preset whose `physical_atmosphere` flag is off. Earth on that preset is analytic
    /// only because the frame does not pass the flag ([`crate::atmosphere::planet_lut_sky`]).
    #[test]
    fn apply_sun_builds_tables_when_the_frame_asks() {
        let sun = crate::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 1_000.0);
        for planet in [
            AtmosphereParameters::mars(),
            AtmosphereParameters::airless(),
            AtmosphereParameters::earth(),
        ] {
            let mut state = OutdoorState::default();
            state.params.planet = planet;
            assert_eq!(state.params.is_earth(), planet == AtmosphereParameters::earth());
            let _ = state.apply_sun(true, sun, 1.7);
            let luts = state
                .luts
                .as_ref()
                .expect("enabled apply_sun integrates the planet");
            assert_eq!(luts.params, planet);
        }
        let mut idle = OutdoorState::default();
        idle.params.planet = AtmosphereParameters::mars();
        let _ = idle.apply_sun(false, sun, 1.7);
        assert!(idle.luts.is_none(), "the analytic path does not build tables");
    }

    /// ADR-7975: the packed interior slots. Up to four volumes they are the list's, slot for slot,
    /// whatever the eye (the packing before ADR-7975, byte for byte); past four they hold the
    /// four nearest the eye in list order, so the passage the eye stands in is always bound.
    #[test]
    fn interior_slots_hold_the_list_up_to_four_and_the_nearest_past_it() {
        // The packing before ADR-7975: the first four in list order, a box with no air unused.
        fn first_four(interiors: &[crate::InteriorVolume]) -> [[f32; 4]; 8] {
            let mut slots = [[0.0; 4]; 8];
            for (i, v) in interiors.iter().take(4).enumerate() {
                if v.half_extent.iter().any(|h| !h.is_finite() || *h <= 0.0)
                    || v.center.iter().any(|c| !c.is_finite())
                {
                    continue;
                }
                slots[i * 2] = [v.center[0], v.center[1], v.center[2], 1.0];
                slots[i * 2 + 1] = [v.half_extent[0], v.half_extent[1], v.half_extent[2], 0.0];
            }
            slots
        }
        let at = |x: f32| crate::InteriorVolume::with_floor([x, 2.0, 0.0], [4.0, 2.0, 4.0]);
        let broken = crate::InteriorVolume::with_floor([0.0; 3], [0.0, 1.0, 1.0]);
        let pool = [at(-100.0), broken, at(50.0), at(100.0)];
        for eye in [
            [0.0, 2.0, 0.0],
            [100.0, 2.0, 0.0],
            [f32::NAN; 3],
            [1.0e9, 0.0, -1.0e9],
        ] {
            for n in 0..=4 {
                let list = &pool[..n];
                let (got, base) = (interior_slots(list, eye), first_four(list));
                assert_eq!(
                    bytemuck::bytes_of(&got),
                    bytemuck::bytes_of(&base),
                    "{n} volumes, eye {eye:?}: the slots before ADR-7975"
                );
            }
        }
        // Five passages, the eye in the fifth: its box is bound (slot 3, list order kept) and the
        // farthest, the first listed, is left out.
        let five = [at(-100.0), at(-50.0), at(50.0), at(100.0), at(200.0)];
        let slots = interior_slots(&five, [200.0, 2.0, 0.0]);
        assert_eq!(slots[6], [200.0, 2.0, 0.0, 1.0]);
        assert_eq!(slots[7], [4.0, 2.0, 4.0, 0.0]);
        assert_eq!(slots[0], [-50.0, 2.0, 0.0, 1.0]);
        assert!(
            (0..4).all(|k| slots[k * 2][0] != -100.0),
            "the farthest passage is not bound"
        );
    }
}
