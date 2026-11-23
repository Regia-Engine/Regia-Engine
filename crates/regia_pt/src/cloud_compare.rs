// SPDX-License-Identifier: MPL-2.0
//! The raster clouds against the path tracer's cloud field, in numbers (ADR-2127).
//!
//! [`PtCloudField`] traces the density the raster sky draws (`regia::cloud_density`, the same
//! texels) with delta tracking for free flights and ratio tracking for shadow rays. This module
//! holds the comparison scene and the measurements:
//!
//! - [`compare_view`]: a camera under the layer. The raster side is the real `fs_cloud_march`
//!   ([`regia_render::render_cloud_view`]): in-scattered radiance and transmittance per texel. The
//!   traced side is the same camera's `volumetric` AOV (radiance of paths whose first vertex is in
//!   the cloud) and `sky` AOV over a clear trace (the transmittance).
//! - [`compare_ground`]: a camera looking down at the ground. The raster side is the real
//!   `fs_cloud_shadow_map` ([`regia_render::render_cloud_sun_map`]) and the overcast dim of the sky
//!   light; the traced side is the ground's light with one path vertex (the sun's direct beam
//!   through the cloud: the Beer–Lambert term the map stores) and with all of them.
//!
//! Both sides see the same light: the sun at the layer's height and the planet's sky table, with
//! no air between the eye and the cloud (the frame's aerial perspective is its own, separately
//! validated term, `tests/aerial_parity.rs`).

use regia_render::{
    AtmosphereParameters, CLOUD_SHADOW_FLOOR, CloudMarch, CloudSettings, CloudViewDesc, CloudViewImage,
    RenderDevice, build_atmosphere_luts_for, build_weather_map, cloud_ambient_radiance,
    cloud_overcast_factor, cloud_shadow_shade, cloud_shadow_transmittance, render_cloud_sun_map,
    render_cloud_view, transmitted_sun_at,
};

use crate::scene::{PtCamera, PtCloudField, PtCloudPhase, PtScene, PtSky, PtSun};
use crate::{PathTracer, PtError, PtImage, PtMaterial, PtSettings, scenes};

/// Half edge of the comparison ground, metres. Light a cloud scatters sideways comes back to the
/// ground tens of kilometres away.
pub const COMPARE_GROUND_HALF_M: f32 = 150_000.0;

/// Height of the comparison camera above the ground, metres (cloud bases are at 900 m and up).
pub const COMPARE_EYE_Y_M: f32 = 3.0;

fn lum(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

/// A sky the two renderers are compared under: the layer, the sun and the view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudCase {
    /// A short name for the report.
    pub name: &'static str,
    /// The layer.
    pub settings: CloudSettings,
    /// Sun elevation above the horizon, degrees.
    pub sun_elevation_deg: f32,
    /// Sun azimuth from the view's horizontal direction, degrees (0: the camera looks toward it).
    pub sun_azimuth_deg: f32,
    /// View elevation above the horizon, degrees.
    pub view_elevation_deg: f32,
    /// Vertical field of view, degrees.
    pub fov_y_deg: f32,
}

/// The sun and the sky of a [`CloudCase`], the same for both renderers.
#[derive(Clone, Debug)]
pub struct CompareLight {
    /// Unit vector toward the sun.
    pub to_sun: [f32; 3],
    /// The tracer's sun (colour and illuminance at the layer's middle height).
    pub sun: PtSun,
    /// The tracer's sky (the planet's sky table).
    pub sky: PtSky,
    /// Sky radiance the raster march reads at the cloud tops and bases.
    pub ambient: ([f32; 3], [f32; 3]),
    /// Ground albedo of the comparison ground (the planet's).
    pub ground_albedo: [f32; 3],
    /// Radius of the planet, metres.
    pub planet_radius_m: f32,
}

impl CompareLight {
    /// The sun alone: no sky behind the clouds, none in the raster's ambient.
    #[must_use]
    pub fn sun_only(&self) -> Self {
        Self {
            sky: PtSky::None,
            ambient: ([0.0; 3], [0.0; 3]),
            ..self.clone()
        }
    }

    /// The sky alone: no sun on the clouds or in the raster's ambient's ground term's place (the
    /// ambient keeps the ground bounce the frame gives it).
    #[must_use]
    pub fn sky_only(&self) -> Self {
        Self {
            sun: PtSun {
                illuminance: 0.0,
                ..self.sun
            },
            ..self.clone()
        }
    }
}

impl CloudCase {
    /// Unit vector of the view.
    #[must_use]
    pub fn view_dir(&self) -> [f32; 3] {
        let el = self.view_elevation_deg.to_radians();
        [0.0, el.sin(), el.cos()]
    }

    /// Unit vector toward the sun.
    #[must_use]
    pub fn to_sun(&self) -> [f32; 3] {
        let el = self.sun_elevation_deg.to_radians();
        let az = self.sun_azimuth_deg.to_radians();
        [el.cos() * az.sin(), el.sin(), el.cos() * az.cos()]
    }

    /// The Earth's light at this sun: both renderers get the sun as it arrives at the middle of
    /// the layer, and the same sky table.
    #[must_use]
    pub fn light(&self) -> CompareLight {
        let planet = AtmosphereParameters::earth();
        let to_sun = self.to_sun();
        let luts = build_atmosphere_luts_for(&planet, to_sun, COMPARE_EYE_Y_M);
        let mid = 0.5 * (self.settings.altitude_bottom + self.settings.altitude_top);
        let (color, illuminance) = transmitted_sun_at(&luts, mid);
        CompareLight {
            to_sun,
            sun: PtSun {
                toward: to_sun,
                color,
                illuminance,
                angular_radius: planet.sun_angular_radius.max(0.0),
            },
            sky: PtSky::from_atmosphere(&luts),
            ambient: cloud_ambient_radiance(&luts),
            ground_albedo: planet.ground_albedo,
            planet_radius_m: planet.bottom_radius_km * 1000.0,
        }
    }
}

/// The skies of the measurement: the layer most frames draw (`coverage_layer(0.45)`, the default
/// settings and look, as the render lab's sky) and the three volumetric genus presets, each
/// under a morning and a noon sun, looking up 40° with the sun behind, beside and in front of
/// the camera. No wind, so every number repeats.
#[must_use]
pub fn standard_cases() -> Vec<CloudCase> {
    let still = |s: CloudSettings| CloudSettings {
        wind_m_per_s: [0.0, 0.0],
        ..s
    };
    let layers: [(&str, CloudSettings); 4] = [
        ("default_045", still(CloudSettings::coverage_layer(0.45))),
        (
            "fair_cumulus",
            still(CloudSettings::default().with_genus(regia_render::CloudGenus::FairCumulus)),
        ),
        (
            "cumulus",
            still(CloudSettings::default().with_genus(regia_render::CloudGenus::Cumulus)),
        ),
        (
            "stratocumulus",
            still(CloudSettings::default().with_genus(regia_render::CloudGenus::Stratocumulus)),
        ),
    ];
    let mut cases = Vec::new();
    for (name, settings) in layers {
        for (sun_elevation_deg, sun_azimuth_deg) in [(55.0, 180.0), (55.0, 90.0), (25.0, 180.0), (25.0, 20.0)]
        {
            cases.push(CloudCase {
                name,
                settings,
                sun_elevation_deg,
                sun_azimuth_deg,
                view_elevation_deg: 40.0,
                fov_y_deg: 55.0,
            });
        }
    }
    cases
}

/// The comparison scene: a large lambert ground at y = 0, the light, and the field (or none).
#[must_use]
pub fn compare_scene(
    camera: PtCamera,
    light: &CompareLight,
    field: Option<PtCloudField>,
    sun: bool,
) -> PtScene {
    let mut scene = PtScene::new(camera);
    let h = COMPARE_GROUND_HALF_M;
    let ground = scene.add_mesh(scenes::quad([
        [-h, 0.0, -h],
        [-h, 0.0, h],
        [h, 0.0, h],
        [h, 0.0, -h],
    ]));
    let material = scene.add_material(PtMaterial::lambert(light.ground_albedo));
    scene.add_instance(ground, material, regia_core::glam::Mat4::IDENTITY);
    scene.sky = light.sky.clone();
    scene.sun = (sun && light.sun.illuminance > 0.0).then_some(light.sun);
    scene.clouds = field;
    scene
}

/// How large a trace is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloudTraceSize {
    /// Image width, pixels.
    pub width: u32,
    /// Image height, pixels.
    pub height: u32,
    /// Samples per pixel.
    pub spp: u32,
    /// Path vertices of a full trace.
    pub max_bounces: u32,
    /// Trace seed.
    pub seed: u32,
    /// Frames of the raster march's jitter to average.
    pub raster_frames: u32,
}

impl CloudTraceSize {
    /// A trace small enough for a test on a CPU adapter.
    pub const TEST: Self = Self {
        width: 48,
        height: 32,
        spp: 48,
        max_bounces: 128,
        seed: 3,
        raster_frames: 4,
    };

    /// The size of the measurement in ADR-2127.
    pub const REPORT: Self = Self {
        width: 64,
        height: 48,
        spp: 32,
        // A cumulus core keeps light for hundreds of vertices (a white furnace reads 0.979 at 256
        // and 0.993 at 512).
        max_bounces: 512,
        seed: 3,
        raster_frames: 16,
    };
}

fn trace_settings(size: &CloudTraceSize, max_bounces: u32) -> PtSettings {
    PtSettings {
        spp: size.spp,
        max_bounces,
        seed: size.seed,
        // Past this a path keeps its weight only by surviving; thick clouds would get noisy.
        rr_start: max_bounces,
        jitter: true,
        samples_per_submit: 8,
    }
}

/// Which side of a comparison a raster march stands for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterMarch {
    /// The cone light march, curl swirl and dark edge.
    pub cone_light: bool,
    /// Marches per texel and step ceiling.
    pub march: CloudMarch,
    /// Percent of the layer's steps.
    pub step_percent: u32,
    /// Strength of the two multiple-scattering octaves (0: single scattering alone, with a look
    /// that has no diffusion).
    pub multiple_scattering: f32,
}

impl RasterMarch {
    /// Desktop High: 64 steps, one march per texel, cone light, the frame's octaves.
    pub const DESKTOP_HIGH: Self = Self {
        cone_light: true,
        march: CloudMarch::REALTIME,
        step_percent: 100,
        multiple_scattering: 0.5,
    };

    /// Steps as fine as the shader allows (256), for a transmittance that is the density's and
    /// not the step size's.
    pub const FINEST: Self = Self {
        cone_light: true,
        march: CloudMarch {
            samples: 1,
            max_steps: CloudMarch::STEP_LIMIT,
            max_divisor: 1,
        },
        step_percent: 400,
        multiple_scattering: 0.5,
    };
}

/// The difference between the raster clouds and the traced clouds on one view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudViewMeasure {
    /// Mean luminance of the raster's in-scattered radiance, cd/m².
    pub raster_radiance: f64,
    /// Mean luminance of the traced in-scattered radiance, cd/m².
    pub pt_radiance: f64,
    /// `log2(raster / traced)` of those means: stops, negative where the raster is darker.
    pub radiance_log2: f64,
    /// Root-mean-square `log2(raster / traced)` over the blocks with cloud in them, stops.
    pub radiance_block_rms_log2: f64,
    /// Correlation of the blocks' in-scattered luminance, -1..1.
    pub radiance_block_correlation: f64,
    /// Mean of the raster's view transmittance.
    pub raster_transmittance: f64,
    /// Mean of the traced view transmittance.
    pub pt_transmittance: f64,
    /// Root-mean-square difference of the blocks' transmittance.
    pub transmittance_block_rms: f64,
    /// Correlation of the blocks' transmittance, -1..1.
    pub transmittance_block_correlation: f64,
    /// Mean colour ratio of the in-scattered radiance raster / traced, red over blue, over the
    /// same ratio of the traced image; 1 where the two have the same tint.
    pub tint_red_over_blue: f64,
    /// Blocks with cloud in them (traced transmittance under 0.9).
    pub cloudy_blocks: u32,
}

/// Edge of a comparison block, pixels. One raster march per texel leaves its start jitter in the
/// texels and a short trace its noise; both average out over a block.
const BLOCK: u32 = 8;

/// Block means of `values` (row-major, `width` wide) over [`BLOCK`]-sized blocks.
fn block_means(values: &[f64], width: u32, height: u32) -> Vec<f64> {
    let (bw, bh) = (width / BLOCK, height / BLOCK);
    let mut out = Vec::with_capacity((bw * bh) as usize);
    for by in 0..bh {
        for bx in 0..bw {
            let mut sum = 0.0;
            for y in by * BLOCK..(by + 1) * BLOCK {
                for x in bx * BLOCK..(bx + 1) * BLOCK {
                    sum += values[(y * width + x) as usize];
                }
            }
            out.push(sum / f64::from(BLOCK * BLOCK));
        }
    }
    out
}

/// Pearson correlation; 0 for a constant input.
fn correlation(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len()).max(1) as f64;
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        ab += (x - ma) * (y - mb);
        aa += (x - ma) * (x - ma);
        bb += (y - mb) * (y - mb);
    }
    if aa > 0.0 && bb > 0.0 {
        ab / (aa * bb).sqrt()
    } else {
        0.0
    }
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

fn rms(v: impl Iterator<Item = f64>) -> f64 {
    let (mut sum, mut n) = (0.0, 0u32);
    for x in v {
        sum += x * x;
        n += 1;
    }
    (sum / f64::from(n.max(1))).sqrt()
}

/// The camera of a [`CloudCase`] as the tracer's and the raster's view description.
fn case_camera(case: &CloudCase, size: &CloudTraceSize) -> (PtCamera, CloudViewDesc) {
    let eye = [0.0, COMPARE_EYE_Y_M, 0.0];
    let dir = case.view_dir();
    let target = [
        eye[0] + 1000.0 * dir[0],
        eye[1] + 1000.0 * dir[1],
        eye[2] + 1000.0 * dir[2],
    ];
    let fov = case.fov_y_deg.to_radians();
    (
        PtCamera::pinhole(eye, target, fov, size.width, size.height),
        CloudViewDesc::realtime(size.width, size.height, eye, target, fov),
    )
}

/// The raster march of `case` as the real frame's pass draws it.
///
/// # Errors
/// [`PtError::Gpu`] when a pass fails.
pub fn raster_view(
    device: &RenderDevice,
    case: &CloudCase,
    light: &CompareLight,
    march: &RasterMarch,
    size: &CloudTraceSize,
) -> Result<CloudViewImage, PtError> {
    let _span = tracing::info_span!("cloud_compare_raster").entered();
    let (_, mut desc) = case_camera(case, size);
    desc.to_sun = light.to_sun;
    desc.sun = light.sun.color.map(|c| c * light.sun.illuminance);
    desc.ambient = light.ambient;
    desc.planet_radius_m = light.planet_radius_m;
    desc.cone_light = march.cone_light;
    desc.march = march.march;
    desc.step_percent = march.step_percent;
    desc.multiple_scattering = march.multiple_scattering;
    desc.fade_m = CloudViewDesc::NO_FADE_M;
    desc.frames = size.raster_frames;
    render_cloud_view(device, &case.settings, &desc).map_err(|e| PtError::Gpu(e.to_string()))
}

/// The traced view of `case` under `field`: `(clouds, clear)`. The clear trace's `sky` AOV is the
/// sky behind each pixel, so `clouds.sky / clear.sky` is the view transmittance.
///
/// # Errors
/// [`PtError`] from the scene or the GPU.
pub fn traced_view(
    tracer: &PathTracer,
    case: &CloudCase,
    light: &CompareLight,
    field: PtCloudField,
    size: &CloudTraceSize,
    max_bounces: u32,
) -> Result<(PtImage, PtImage), PtError> {
    let _span = tracing::info_span!("cloud_compare_trace", spp = size.spp).entered();
    let (camera, _) = case_camera(case, size);
    let clouds = compare_scene(camera, light, Some(field), true);
    let image = tracer.render(&clouds, &trace_settings(size, max_bounces))?;
    let clear = compare_scene(camera, light, None, true);
    let clear_settings = PtSettings {
        spp: 8,
        ..trace_settings(size, 1)
    };
    let sky = tracer.render(&clear, &clear_settings)?;
    Ok((image, sky))
}

/// The numbers of one view: the raster image against the two traced images of [`traced_view`].
#[must_use]
pub fn measure_view(raster: &CloudViewImage, traced: &PtImage, clear: &PtImage) -> CloudViewMeasure {
    let (w, h) = (raster.width, raster.height);
    let raster_l: Vec<f64> = raster.rgba.iter().map(|c| lum([c[0], c[1], c[2]])).collect();
    let raster_t: Vec<f64> = raster.rgba.iter().map(|c| f64::from(c[3])).collect();
    let pt_l: Vec<f64> = traced.volumetric.iter().map(|c| lum(*c)).collect();
    let pt_t: Vec<f64> = traced
        .sky
        .iter()
        .zip(&clear.sky)
        .map(|(c, s)| (lum(*c) / lum(*s).max(1.0e-9)).clamp(0.0, 1.0))
        .collect();
    let (br, bp) = (block_means(&raster_l, w, h), block_means(&pt_l, w, h));
    let (brt, bpt) = (block_means(&raster_t, w, h), block_means(&pt_t, w, h));
    let cloudy: Vec<usize> = (0..br.len()).filter(|i| bpt[*i] < 0.9).collect();
    let log2s = cloudy
        .iter()
        .map(|&i| (br[i].max(1.0e-3) / bp[i].max(1.0e-3)).log2());
    let channel_sum = |values: &[[f32; 3]], c: usize| values.iter().map(|v| f64::from(v[c])).sum::<f64>();
    let raster_rgb: Vec<[f32; 3]> = raster.rgba.iter().map(|c| [c[0], c[1], c[2]]).collect();
    let (rr, rb) = (channel_sum(&raster_rgb, 0), channel_sum(&raster_rgb, 2));
    let (pr, pb) = (
        channel_sum(&traced.volumetric, 0),
        channel_sum(&traced.volumetric, 2),
    );
    let raster_radiance = mean(&raster_l);
    let pt_radiance = mean(&pt_l);
    CloudViewMeasure {
        raster_radiance,
        pt_radiance,
        radiance_log2: (raster_radiance.max(1.0e-6) / pt_radiance.max(1.0e-6)).log2(),
        radiance_block_rms_log2: rms(log2s),
        radiance_block_correlation: correlation(&br, &bp),
        raster_transmittance: mean(&raster_t),
        pt_transmittance: mean(&pt_t),
        transmittance_block_rms: rms(brt.iter().zip(&bpt).map(|(a, b)| a - b)),
        transmittance_block_correlation: correlation(&brt, &bpt),
        tint_red_over_blue: (rr / rb.max(1.0e-6)) / (pr / pb.max(1.0e-6)).max(1.0e-6),
        cloudy_blocks: cloudy.len() as u32,
    }
}

/// One view of `case`: the raster's `march` against a trace of `field`.
///
/// # Errors
/// [`PtError`] from the scene or the GPU.
pub fn compare_view(
    tracer: &PathTracer,
    case: &CloudCase,
    march: &RasterMarch,
    field: PtCloudField,
    size: &CloudTraceSize,
) -> Result<CloudViewMeasure, PtError> {
    let light = case.light();
    let raster = raster_view(tracer.device(), case, &light, march, size)?;
    let (traced, clear) = traced_view(tracer, case, &light, field, size, size.max_bounces)?;
    Ok(measure_view(&raster, &traced, &clear))
}

/// In-scattered light of one view by where it comes from: mean luminance, cd/m².
///
/// Three raster terms, drawn by the real march with one light on: the frame's ambient row alone,
/// the sun's first octave alone (with `look.diffusion` off: the octave and its Beer–Powder), and the
/// sun with the frame's octaves or diffusion. Four traced terms: the sun alone with one path vertex
/// (single scattering) and with all of them, each light with the march's phase and with the
/// droplets'. Ground bounce: in the raster's ambient row, in the traced sun's and sky's own paths.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudLightTerms {
    /// The raster's ambient term alone.
    pub raster_ambient: f64,
    /// The raster's sun, first octave only.
    pub raster_sun_single: f64,
    /// The raster's sun with its multiple-scattering term.
    pub raster_sun_full: f64,
    /// The traced sky alone, the march's phase and albedo.
    pub pt_sky_raster_phase: f64,
    /// The traced sky alone, droplets.
    pub pt_sky_droplet: f64,
    /// The traced sun, one path vertex, the march's phase and albedo.
    pub pt_sun_single_raster_phase: f64,
    /// The traced sun, every order, the march's phase and albedo.
    pub pt_sun_all_raster_phase: f64,
    /// The traced sun, every order, droplets.
    pub pt_sun_all_droplet: f64,
}

/// [`CloudLightTerms`] of `case`.
///
/// # Errors
/// [`PtError`] from the scene or the GPU.
pub fn light_terms(
    tracer: &PathTracer,
    case: &CloudCase,
    size: &CloudTraceSize,
) -> Result<CloudLightTerms, PtError> {
    let _span = tracing::info_span!("cloud_compare_terms", spp = size.spp).entered();
    let light = case.light();
    let device = tracer.device();
    let (sun, sky) = (light.sun_only(), light.sky_only());
    let raster_mean = |image: &CloudViewImage| {
        image.rgba.iter().map(|c| lum([c[0], c[1], c[2]])).sum::<f64>() / image.rgba.len().max(1) as f64
    };
    let mut first_octave = RasterMarch::DESKTOP_HIGH;
    first_octave.multiple_scattering = 0.0;
    let mut no_diffusion = *case;
    no_diffusion.settings.look.diffusion = 0.0;
    let traced = |light: &CompareLight, field: PtCloudField, bounces: u32| -> Result<f64, PtError> {
        let (camera, _) = case_camera(case, size);
        let scene = compare_scene(camera, light, Some(field), true);
        let image = tracer.render(&scene, &trace_settings(size, bounces))?;
        Ok(mean(
            &image.volumetric.iter().map(|c| lum(*c)).collect::<Vec<_>>(),
        ))
    };
    let raster_phase = raster_parameters_field(case, &light);
    let droplet = physical_field(case, &light);
    Ok(CloudLightTerms {
        raster_ambient: raster_mean(&raster_view(
            device,
            case,
            &sky,
            &RasterMarch::DESKTOP_HIGH,
            size,
        )?),
        raster_sun_single: raster_mean(&raster_view(device, &no_diffusion, &sun, &first_octave, size)?),
        raster_sun_full: raster_mean(&raster_view(
            device,
            case,
            &sun,
            &RasterMarch::DESKTOP_HIGH,
            size,
        )?),
        pt_sky_raster_phase: traced(&sky, raster_phase, size.max_bounces)?,
        pt_sky_droplet: traced(&sky, droplet, size.max_bounces)?,
        pt_sun_single_raster_phase: traced(&sun, raster_phase, 1)?,
        pt_sun_all_raster_phase: traced(&sun, raster_phase, size.max_bounces)?,
        pt_sun_all_droplet: traced(&sun, droplet, size.max_bounces)?,
    })
}

/// The field of `case` traced with the raster march's own parameters: its albedo and first-octave
/// phase, so the only differences left are the march's approximations.
#[must_use]
pub fn raster_parameters_field(case: &CloudCase, light: &CompareLight) -> PtCloudField {
    PtCloudField {
        albedo: regia_render::CLOUD_MARCH_ALBEDO,
        phase: PtCloudPhase::RASTER_DUAL_LOBE,
        ..PtCloudField::from_settings(case.settings, light.planet_radius_m)
    }
}

/// The field of `case` traced as water droplets are: albedo 1, one lobe of asymmetry 0.85.
#[must_use]
pub fn physical_field(case: &CloudCase, light: &CompareLight) -> PtCloudField {
    PtCloudField::from_settings(case.settings, light.planet_radius_m)
}

/// Height of the [`compare_ground`] camera, metres: below every layer's base, high enough for a
/// wide footprint.
pub const COMPARE_GROUND_EYE_Y_M: f32 = 800.0;

/// Edge of the [`compare_ground`] footprint on the ground, metres.
pub const COMPARE_GROUND_FOOTPRINT_M: f32 = 12_000.0;

/// The ground's light under the layer of one [`CloudCase`], in fractions of its clear-sky light.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroundLightMeasure {
    /// Mean sun transmittance of the raster's cloud transmittance map over the footprint.
    pub raster_sun: f64,
    /// Traced direct sun only (first vertex, no scattered light): the same Beer–Lambert term.
    pub pt_sun_direct: f64,
    /// Root-mean-square difference of the blocks of those two.
    pub sun_direct_block_rms: f64,
    /// Correlation of the blocks of those two, -1..1.
    pub sun_direct_block_correlation: f64,
    /// Traced sun with every scattering order: what the sun really leaves on the ground.
    pub pt_sun_all: f64,
    /// The raster's sky-light multiplier under this layer (`1 − 0.6 · overcast`, ADR-321).
    pub raster_sky: f64,
    /// Traced sky with every scattering order.
    pub pt_sky_all: f64,
    /// The raster's whole ground light relative to clear (sun and sky by their clear shares).
    pub raster_total: f64,
    /// The traced whole ground light relative to clear.
    pub pt_total: f64,
    /// `log2(raster_total / pt_total)`: stops, negative where the raster's ground is darker.
    pub total_log2: f64,
    /// A default frame's ground: the mean of the screen-space cloud shadow's whole-pixel multiply
    /// (`shadow_map` off: `COLUMN_FILL` and the 0.35 floor, `fs_cloud_shadow`).
    pub raster_screen_shade: f64,
    /// `log2(raster_screen_shade / pt_total)`: stops, negative where that ground is darker.
    pub screen_log2: f64,
}

/// The camera over the footprint, looking straight down (a pinhole keeps a plane's texels evenly
/// spaced on it).
fn ground_camera(width: u32, height: u32) -> PtCamera {
    let eye = [0.0, COMPARE_GROUND_EYE_Y_M, 0.0];
    let fov = 2.0 * (0.5 * COMPARE_GROUND_FOOTPRINT_M / COMPARE_GROUND_EYE_Y_M).atan();
    // The basis rule of `PtCamera::basis` picks +Z for the degenerate up.
    PtCamera::pinhole(eye, [0.0, 0.0, 0.0], fov, width, height)
}

/// Mean ground radiance of a trace per pixel, as luminance.
fn pixel_luma(image: &PtImage) -> Vec<f64> {
    image.beauty.iter().map(|c| lum(*c)).collect()
}

/// The ground under the layer of `case`: the raster's sun map and sky dim against a trace, over a
/// square footprint (`COMPARE_GROUND_FOOTPRINT_M`). `size.width` is the image's edge.
///
/// # Errors
/// [`PtError`] from the scene or the GPU.
pub fn compare_ground(
    tracer: &PathTracer,
    case: &CloudCase,
    field: PtCloudField,
    size: &CloudTraceSize,
) -> Result<GroundLightMeasure, PtError> {
    let _span = tracing::info_span!("cloud_compare_ground", spp = size.spp).entered();
    let light = case.light();
    let edge = size.width;
    let camera = ground_camera(edge, edge);
    let (_, right, up) = camera.basis();
    let run = |field: Option<PtCloudField>, sun: bool, sky: bool, bounces: u32| -> Result<PtImage, PtError> {
        let mut scene = compare_scene(camera, &light, field, sun);
        if !sky {
            scene.sky = PtSky::None;
        }
        tracer.render(&scene, &trace_settings(size, bounces))
    };
    // The sun alone (direct, then all orders) and the sky alone, each over its clear trace. One
    // vertex is the sun's beam through the cloud; ground bounce and scattered light come after.
    let sun_clear = pixel_luma(&run(None, true, false, 1)?);
    let sun_direct = pixel_luma(&run(Some(field), true, false, 1)?);
    let sun_all = pixel_luma(&run(Some(field), true, false, size.max_bounces)?);
    let sun_clear_all = pixel_luma(&run(None, true, false, size.max_bounces)?);
    let sky_all = pixel_luma(&run(Some(field), false, true, size.max_bounces)?);
    let sky_clear = pixel_luma(&run(None, false, true, size.max_bounces)?);
    let ratio_means = |a: &[f64], b: &[f64]| mean(a) / mean(b).max(1.0e-9);

    // The raster's map over the same ground, texel per pixel: a ground point looks up the map where
    // its sun line crosses the layer base.
    let to_sun = light.to_sun;
    let map_edge = (2 * edge).max(256);
    let map_texel = 2.0 * COMPARE_GROUND_FOOTPRINT_M / map_edge as f32;
    let map = render_cloud_sun_map(
        tracer.device(),
        &case.settings,
        to_sun,
        light.planet_radius_m,
        field.curl,
        map_edge,
        map_texel,
    )
    .map_err(|e| PtError::Gpu(e.to_string()))?;
    let rise = case.settings.altitude_bottom / to_sun[1].max(0.05);
    let half_ground = 0.5 * COMPARE_GROUND_FOOTPRINT_M;
    let map_half = (map_edge / 2) as i32;
    let at_map = |x: f32, z: f32| -> f64 {
        let cx = ((x / map_texel).floor() as i32 + map_half).clamp(0, map_edge as i32 - 1) as u32;
        let cz = ((z / map_texel).floor() as i32 + map_half).clamp(0, map_edge as i32 - 1) as u32;
        f64::from(map[(cz * map_edge + cx) as usize])
    };
    let mut raster_t = vec![0.0_f64; (edge * edge) as usize];
    for py in 0..edge {
        for px in 0..edge {
            let sx = ((px as f32 + 0.5) / edge as f32) * 2.0 - 1.0;
            let sy = 1.0 - ((py as f32 + 0.5) / edge as f32) * 2.0;
            let ground = right * (sx * half_ground) + up * (sy * half_ground);
            let x = ground.x + to_sun[0] * rise;
            let z = ground.z + to_sun[2] * rise;
            raster_t[(py * edge + px) as usize] = at_map(x, z);
        }
    }
    let direct_t: Vec<f64> = sun_direct
        .iter()
        .zip(&sun_clear)
        .map(|(a, b)| a / b.max(1.0e-9))
        .collect();
    let (rb, pb) = (
        block_means(&raster_t, edge, edge),
        block_means(&direct_t, edge, edge),
    );
    // The raster's sky dim: its column-mean overcast (ADR-321) on the layer's own weather map.
    let weather = build_weather_map(&case.settings);
    let overcast = f64::from(cloud_overcast_factor(&case.settings, &weather));
    let raster_sky = 1.0 - f64::from(regia_render::CLOUD_OVERCAST_DIM) * overcast;
    // The frame without the map: the column shadow along the sun, over the same ground.
    let travel = [-to_sun[0], -to_sun[1], -to_sun[2]];
    let mut screen = 0.0_f64;
    for py in 0..edge {
        for px in 0..edge {
            let sx = ((px as f32 + 0.5) / edge as f32) * 2.0 - 1.0;
            let sy = 1.0 - ((py as f32 + 0.5) / edge as f32) * 2.0;
            let ground = right * (sx * half_ground) + up * (sy * half_ground);
            let t = cloud_shadow_transmittance([ground.x, ground.z], 0.0, travel, &case.settings, &weather);
            screen += f64::from(cloud_shadow_shade(t, CLOUD_SHADOW_FLOOR));
        }
    }
    let raster_screen_shade = screen / f64::from(edge * edge);
    let raster_sun = mean(&raster_t);
    let pt_sun_all = ratio_means(&sun_all, &sun_clear_all);
    let pt_sky_all = ratio_means(&sky_all, &sky_clear);
    // Clear shares by the traced clear light: the sun's direct and bounced light and the sky's.
    let (e_sun, e_sky) = (mean(&sun_clear_all), mean(&sky_clear));
    let share = e_sun / (e_sun + e_sky).max(1.0e-9);
    let raster_total = share * raster_sun + (1.0 - share) * raster_sky;
    let pt_total = share * pt_sun_all + (1.0 - share) * pt_sky_all;
    Ok(GroundLightMeasure {
        raster_sun,
        pt_sun_direct: ratio_means(&sun_direct, &sun_clear),
        sun_direct_block_rms: rms(rb.iter().zip(&pb).map(|(a, b)| a - b)),
        sun_direct_block_correlation: correlation(&rb, &pb),
        pt_sun_all,
        raster_sky,
        pt_sky_all,
        raster_total,
        pt_total,
        total_log2: (raster_total.max(1.0e-6) / pt_total.max(1.0e-6)).log2(),
        raster_screen_shade,
        screen_log2: (raster_screen_shade.max(1.0e-6) / pt_total.max(1.0e-6)).log2(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_and_rms_are_what_they_say() {
        let a = [1.0, 2.0, 3.0, 4.0];
        let b = [2.0, 4.0, 6.0, 8.0];
        assert!((correlation(&a, &b) - 1.0).abs() < 1.0e-9);
        let c = [4.0, 3.0, 2.0, 1.0];
        assert!((correlation(&a, &c) + 1.0).abs() < 1.0e-9);
        assert_eq!(correlation(&a, &[5.0; 4]), 0.0);
        assert!((rms([3.0, 4.0].into_iter()) - (12.5_f64).sqrt()).abs() < 1.0e-9);
    }

    #[test]
    fn block_means_average_each_block() {
        let values: Vec<f64> = (0..(16 * 8))
            .map(|i| if i % 16 < 8 { 1.0 } else { 3.0 })
            .collect();
        assert_eq!(block_means(&values, 16, 8), vec![1.0, 3.0]);
        // A partial block at the edge is left out.
        assert_eq!(block_means(&[2.0; 20 * 8], 20, 8).len(), 2);
    }

    #[test]
    fn a_case_points_where_it_says() {
        let case = standard_cases()[0];
        let view = case.view_dir();
        assert!((view[1] - 40f32.to_radians().sin()).abs() < 1.0e-6);
        let sun = case.to_sun();
        assert!((sun[1] - 55f32.to_radians().sin()).abs() < 1.0e-6);
        // Azimuth 180°: the sun is behind the camera.
        assert!(sun[2] < 0.0 && view[2] > 0.0);
        let len = sun.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((len - 1.0).abs() < 1.0e-5);
    }

    #[test]
    fn the_standard_skies_are_the_shipped_settings_without_wind() {
        let cases = standard_cases();
        assert_eq!(cases.len(), 16);
        assert!(cases.iter().all(|c| c.settings.wind_m_per_s == [0.0, 0.0]));
        let first = cases[0].settings;
        assert_eq!(first.coverage, 0.45);
        assert_eq!(first.density, CloudSettings::default().density);
        let names: Vec<_> = cases.iter().map(|c| c.name).collect();
        for genus in ["fair_cumulus", "cumulus", "stratocumulus"] {
            assert!(names.contains(&genus));
        }
    }

    #[test]
    fn a_field_packs_after_the_scene_and_bad_values_are_refused() {
        let case = standard_cases()[0];
        let light = case.light();
        let (camera, _) = case_camera(&case, &CloudTraceSize::TEST);
        let plain = crate::pack::pack_with(&compare_scene(camera, &light, None, true), None).expect("pack");
        assert_eq!((plain.globals.media[2], plain.globals.media[3]), (0, 0));
        let field = physical_field(&case, &light);
        let packed =
            crate::pack::pack_with(&compare_scene(camera, &light, Some(field), true), None).expect("pack");
        assert_eq!(packed.globals.media[2], 1);
        // The field's data comes after everything the scene had, so nothing else moved.
        assert_eq!(packed.textures[..plain.textures.len()], plain.textures[..]);
        assert_eq!(packed.env[..plain.env.len()], plain.env[..]);
        assert_eq!(packed.globals.media[3] as usize, plain.env.len());
        // Weather, shape, detail and curl: RGBA8 words.
        let n = regia_render::SHAPE_NOISE_SIZE as usize;
        let c = regia_render::CURL_SIZE as usize;
        let w = case.settings.map_resolution as usize;
        assert_eq!(
            packed.textures.len() - plain.textures.len(),
            w * w + 2 * n * n * n + c * c
        );
        let refused = |bad: PtCloudField| {
            crate::pack::pack_with(&compare_scene(camera, &light, Some(bad), true), None).is_err()
        };
        let mut nan = field;
        nan.settings.density = f32::NAN;
        assert!(refused(nan));
        let mut upside_down = field;
        upside_down.settings.altitude_top = upside_down.settings.altitude_bottom;
        assert!(refused(upside_down));
        let mut small = field;
        small.planet_radius_m = 1.0;
        assert!(refused(small));
        let mut bad_uniform = field;
        bad_uniform.density = crate::scene::PtCloudDensity::Uniform(f32::INFINITY);
        assert!(refused(bad_uniform));
    }

    #[test]
    fn the_scene_of_a_comparison_holds_the_field_only_when_given_one() {
        let case = standard_cases()[0];
        let light = case.light();
        let size = CloudTraceSize::TEST;
        let (camera, _) = case_camera(&case, &size);
        assert!(compare_scene(camera, &light, None, true).clouds.is_none());
        let field = physical_field(&case, &light);
        assert_eq!(field.albedo, 1.0);
        let raster = raster_parameters_field(&case, &light);
        assert_eq!(raster.albedo, regia_render::CLOUD_MARCH_ALBEDO);
        let with = compare_scene(camera, &light, Some(field), true);
        assert_eq!(with.clouds, Some(field));
        // A scene without clouds hashes as it did; one with a field does not hash the same.
        assert_ne!(
            with.content_hash(),
            compare_scene(camera, &light, None, true).content_hash()
        );
    }
}
