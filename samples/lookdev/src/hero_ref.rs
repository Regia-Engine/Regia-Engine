// SPDX-License-Identifier: MIT-0
//! Hero material references: reproduce a reference render's studio (a sun of known elevation,
//! azimuth and illuminance, a flat or atmospheric sky, a grey ground plane, a fixed camera) with
//! a DO27 hero asset, and read the frame back as linear HDR so its materials can be matched to
//! the reference by numbers as well as by eye.
//!
//! The comparison is made in reflectance units: both images are divided by the sunlit ground's
//! radiance, which the setup fixes (`albedo · (E_sun · sin elevation + E_sky) / π` on a flat
//! ground), so neither renderer's exposure or radiometric unit matters.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FrameRenderer, HdrCapture, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderSettings, box_mesh,
};

use crate::LookdevError;
use crate::hero::HeroModel;

/// The sky of a studio setup.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StudioSky {
    /// A uniform sky of this horizontal illuminance (lux): radiance `lux / π` in every direction.
    Flat {
        /// Illuminance the sky alone delivers to an unoccluded horizontal surface.
        lux: f32,
    },
    /// A uniform sky of this linear RGB radiance (cd/m² per channel), e.g. a Cycles flat
    /// world colour scaled into the setup's photometric units.
    Uniform {
        /// Radiance per channel.
        radiance: [f32; 3],
    },
    /// The engine's Earth atmosphere for the setup's sun (a physical clear sky).
    Atmosphere,
}

impl StudioSky {
    /// Illuminance (lux) the sky alone delivers to an unoccluded horizontal surface; zero for
    /// the atmosphere, whose sky light is not known in closed form.
    #[must_use]
    pub fn horizontal_lux(&self) -> f32 {
        match *self {
            Self::Flat { lux } => lux,
            Self::Uniform { radiance: [r, g, b] } => {
                std::f32::consts::PI * (0.2126 * r + 0.7152 * g + 0.0722 * b)
            }
            Self::Atmosphere => 0.0,
        }
    }
}

/// One reference setup: sun, sky, ground and camera.
#[derive(Debug, Clone, PartialEq)]
pub struct StudioSetup {
    /// Sun elevation above the horizon, degrees.
    pub sun_elevation_deg: f32,
    /// Sun azimuth, degrees from +Z toward +X (the engine's frame, +Y up).
    pub sun_azimuth_deg: f32,
    /// Sun illuminance on a surface facing it, lux.
    pub sun_lux: f32,
    /// The sky.
    pub sky: StudioSky,
    /// Ground plane albedo (the references use 0.18 grey).
    pub ground_albedo: f32,
    /// Camera eye, metres.
    pub eye: [f32; 3],
    /// Camera look-at point, metres.
    pub target: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Frames rendered before the capture (temporal effects settle).
    pub frames: u32,
    /// Factor on every material's emission (1 = as authored, in nits).
    pub emissive_scale: f32,
    /// Temporal anti-aliasing over the frames (a static camera converges to an anti-aliased
    /// frame, as a path tracer's pixel filter does).
    pub taa: bool,
    /// Fixed exposure (EV100); `None` puts the sunlit ground at middle grey.
    pub exposure_ev: Option<f32>,
}

/// The rendered reference frame.
pub struct StudioFrame {
    /// Linear HDR, cd/m².
    pub hdr: HdrCapture,
    /// Luminance of the flat, sunlit, unoccluded ground under this setup (cd/m²): the unit
    /// [`reflectance_image`] divides by.
    pub ground_luminance: f32,
    /// Frames rendered for texture streaming to go idle before the shot restarted (`0`: it was
    /// idle after the first frame).
    pub settle_frames: u32,
}

/// Unit vector toward the sun for `elevation` and `azimuth` (degrees, azimuth from +Z toward +X).
#[must_use]
pub fn toward_sun(elevation_deg: f32, azimuth_deg: f32) -> Vec3 {
    let (el, az) = (elevation_deg.to_radians(), azimuth_deg.to_radians());
    Vec3::new(el.cos() * az.sin(), el.sin(), el.cos() * az.cos())
}

/// Illuminance on the open, flat ground under `setup`'s sun and sky (lux); the atmosphere's
/// sky light counts as zero.
#[must_use]
pub fn ground_irradiance(setup: &StudioSetup) -> f32 {
    setup.sun_lux * setup.sun_elevation_deg.to_radians().sin().max(0.0) + setup.sky.horizontal_lux()
}

/// Luminance of a flat Lambertian ground of `albedo` under `setup`'s sun and sky, cd/m².
/// The atmosphere's sky light is not known in closed form; it counts as zero here and the
/// caller measures the ground in the frame instead.
#[must_use]
pub fn flat_ground_luminance(setup: &StudioSetup) -> f32 {
    setup.ground_albedo * ground_irradiance(setup) / std::f32::consts::PI
}

/// A studio frame ready to render: the lab scene, its quality, and the environment the
/// renderer's IBL must hold (`None` = the physical atmosphere).
pub struct StudioScene {
    /// The scene (asset, ground plane, sun, camera, post).
    pub lab: LabScene,
    /// Frame quality (horizontal-plus off, atmosphere per the sky).
    pub quality: regia_render::FrameQuality,
    /// Uniform sky radiance and the ground bounce below the horizon (cd/m²), for
    /// [`FrameRenderer::set_studio_sky`].
    pub studio: Option<([f32; 3], [f32; 3])>,
    /// [`flat_ground_luminance`] of the setup.
    pub ground_luminance: f32,
}

/// Builds `setup`'s studio around `model` (in its own frame, standing on y = 0).
///
/// # Errors
///
/// The studio's sky or ground is outside the IBL's range.
pub fn studio_scene(model: &HeroModel, setup: &StudioSetup) -> Result<StudioScene, LookdevError> {
    let mut materials = model.materials.clone();
    for m in &mut materials {
        m.emissive_intensity *= setup.emissive_scale;
    }
    let ground_material = materials.len();
    let a = setup.ground_albedo.clamp(0.0, 1.0);
    // Diagnostic: HERO_OPAQUE_BLACK (see `hero::load`) blacks the ground plane too.
    let black = std::env::var_os("HERO_OPAQUE_BLACK").is_some();
    materials.push(LabMaterial {
        base_color: if black {
            [0.0, 0.0, 0.0, 1.0]
        } else {
            [a, a, a, 1.0]
        },
        metallic: 0.0,
        roughness: 1.0,
        unlit: black,
        ..LabMaterial::EMPTY
    });
    let mut meshes: Vec<LabMesh> = model.meshes.clone();
    // Diagnostic: HERO_NO_GROUND drops the ground plane (BRDF studies of direct light alone).
    if std::env::var_os("HERO_NO_GROUND").is_none() {
        let (vertices, indices) = box_mesh([400.0, 0.05, 400.0]);
        meshes.push(LabMesh {
            vertices,
            indices,
            material: ground_material,
            model: Mat4::from_translation(Vec3::new(0.0, -0.05, 0.0)).to_cols_array_2d(),
            joints: Vec::new(),
        });
    }
    let travel = -toward_sun(setup.sun_elevation_deg, setup.sun_azimuth_deg);
    let settings = RenderSettings::default();
    let mut post: PostSettings = settings.resolve_post();
    post.taa = setup.taa;
    post.auto_exposure = false;
    let ground = flat_ground_luminance(setup).max(1e-3);
    // Fixed exposure: the sunlit ground at middle grey unless the setup fixes it (the HDR
    // target is pre-exposed with the previous frame's exposure, so passes that follow each
    // other keep one exposure).
    post.exposure_ev = setup
        .exposure_ev
        .unwrap_or_else(|| regia_render::ev100_from_luminance(ground));
    let mut quality = settings.quality();
    // The reference camera's field of view is exact: no horizontal-plus widening at 4:3.
    quality.horizontal_plus = false;
    // HERO_GI=balanced|depth picks the lighting option (default: the preset's, Depth on
    // Desktop High).
    match std::env::var("HERO_GI").as_deref() {
        Ok("balanced") => quality.gi_probes = false,
        Ok("depth") => quality.gi_probes = true,
        _ => {}
    }
    // Native resolution, as the reference is: Desktop High's 67 % temporal upscale softens
    // thin panel seams (they read brighter than the reference, the path tracer's do not).
    // HERO_RENDER_SCALE=<0..1> overrides (diagnostic).
    quality.render_scale_permille = std::env::var("HERO_RENDER_SCALE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .map_or(1000, |s| (s.clamp(0.05, 1.0) * 1000.0).round() as u16);
    // Shadow diagnostics: HERO_SHADOW_FILTER=hard|pcf3|pcf|pcss, HERO_PAGED_SHADOWS=0|1,
    // HERO_CSM_RES=<edge>, HERO_CONTACT_SHADOWS=0|1 (the shadow-detail lane's isolation runs).
    // Glass isolation (lane glass-reflection): HERO_NO_SSR / HERO_NO_SSGI turn the screen-space
    // reflections / GI off.
    if std::env::var_os("HERO_NO_SSR").is_some() {
        quality.ssr = false;
    }
    if std::env::var_os("HERO_NO_SSGI").is_some() {
        quality.screen_space_gi = false;
    }
    if let Ok(v) = std::env::var("HERO_CONTACT_SHADOWS") {
        quality.contact_shadows = v != "0";
    }
    if let Ok(v) = std::env::var("HERO_SHADOW_FILTER") {
        quality.shadow_filter = match v.as_str() {
            "hard" => regia_render::ShadowFilter::Hardware2x2,
            "pcf3" => regia_render::ShadowFilter::Pcf3x3,
            "pcf" => regia_render::ShadowFilter::Pcf,
            _ => regia_render::ShadowFilter::PcssLite,
        };
    }
    // HERO_NEAR_TILE=<edge> sets the near sun tile's edge in texels, 0 = off (ADR-164 cost and
    // look isolation).
    if let Some(edge) = std::env::var("HERO_NEAR_TILE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        quality.near_sun_tile = edge;
    }
    if let Ok(v) = std::env::var("HERO_PAGED_SHADOWS") {
        quality.paged_sun_shadows = v != "0";
    }
    if let Some(res) = std::env::var("HERO_CSM_RES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        quality.csm_resolution = res.clamp(256, 8192);
    }
    // HERO_NO_TRANSLUCENT_SHADOWS=1: blended glass casts no shadow (the before of the
    // translucent-shadows comparison).
    if std::env::var_os("HERO_NO_TRANSLUCENT_SHADOWS").is_some() {
        quality.translucent_shadow_cascades = 0;
        quality.translucent_local_shadows = false;
    }
    // HERO_REFLOCC=0 turns the reflection occlusion off (diagnostic: before/after sheets).
    if std::env::var("HERO_REFLOCC").is_ok_and(|v| v == "0") {
        quality.reflection_occlusion = false;
    }
    let uniform = match setup.sky {
        StudioSky::Flat { lux } => Some([lux / std::f32::consts::PI; 3]),
        StudioSky::Uniform { radiance } => Some(radiance),
        StudioSky::Atmosphere => None,
    };
    let studio = match uniform {
        Some(radiance) => {
            // A uniform world above the horizon and the ground plane's first bounce below it,
            // as IBL (the lab's flat ambient is a tinted gradient, not a uniform sky, and a
            // uniform world would light the asset's underside with sky instead of ground). The
            // atmosphere bake would overwrite the IBL, so the frame runs the lab sky instead.
            let g = setup.ground_albedo.clamp(0.0, 1.0) * ground_irradiance(setup) / std::f32::consts::PI;
            let ground = [g; 3];
            // Diagnostic: HERO_NO_INDIRECT renders the sun alone (no studio IBL).
            let (radiance, ground) = if std::env::var_os("HERO_NO_INDIRECT").is_some() {
                ([0.0; 3], [0.0; 3])
            } else {
                (radiance, ground)
            };
            if radiance
                .iter()
                .chain(ground.iter())
                .any(|&r| r > regia_render::SKY_IBL_SCALE)
            {
                return Err(LookdevError::Config(format!(
                    "studio sky {radiance:?} / ground {g} cd/m² is above the IBL's range ({} cd/m²)",
                    regia_render::SKY_IBL_SCALE
                )));
            }
            quality.physical_atmosphere = false;
            Some((radiance, ground))
        }
        None => {
            quality.physical_atmosphere = true;
            None
        }
    };
    let size = (model.max - model.min).length().max(1.0);
    let lab = LabScene {
        width: setup.width,
        height: setup.height,
        eye: setup.eye,
        target: setup.target,
        fov_y: setup.fov_y,
        near: (size * 0.002).clamp(0.02, 0.5),
        far: (size * 40.0).max(1000.0),
        depth_prepass: settings.profile.depth_prepass,
        directional_shadows: true,
        ibl: true,
        exposure_ev: post.exposure_ev,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(post),
        // Diagnostic: HERO_NO_SUN drops the sun from the shading (exposure and IBL unchanged).
        directional: DirectionalLightUniform::new(
            travel.to_array(),
            [1.0, 1.0, 1.0],
            if std::env::var_os("HERO_NO_SUN").is_some() {
                0.0
            } else {
                setup.sun_lux
            },
        ),
        local_lights: Vec::new(),
        materials,
        meshes,
    };
    Ok(StudioScene {
        lab,
        quality,
        studio,
        ground_luminance: ground,
    })
}

/// Renders `model` (in its own frame, standing on y = 0) in `setup`'s studio and reads back
/// the linear HDR of the last frame.
///
/// # Errors
///
/// The frame or its HDR readback failed.
pub fn render_studio(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    model: &HeroModel,
    setup: &StudioSetup,
) -> Result<StudioFrame, LookdevError> {
    render_studio_with(device, renderer, model, setup, |_| {})
}

/// [`render_studio`] with the frame quality adjusted by `tweak` (e.g. the emissive-light
/// budget for an on/off comparison).
///
/// # Errors
///
/// The frame or its HDR readback failed.
pub fn render_studio_with(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    model: &HeroModel,
    setup: &StudioSetup,
    tweak: impl FnOnce(&mut regia_render::FrameQuality),
) -> Result<StudioFrame, LookdevError> {
    let mut scene = studio_scene(model, setup)?;
    tweak(&mut scene.quality);
    renderer.set_studio_sky(device, scene.studio);
    let frame = scene.lab.as_frame_scene_with_quality(scene.quality);
    let frames = setup.frames.max(1);
    // Texture streaming (the game preset's budget) can still be filling or fading sets after
    // the first frame: a renderer shared across cameras gives each camera's re-keyed materials
    // what the earlier cameras' copies left of the budget, so they start at the floor and grow
    // under a time-based upload budget. Render until streaming is idle, then restart the shot
    // (camera cut: fresh temporal history) so the capture is the settled frame rather than
    // wherever the uploads got to (hero-regress). Streaming starts a frame after a material's
    // first use, so every frame is checked; the shot restarts at most once, and a shot that
    // never streams renders exactly as before.
    let mut settle_frames = 0;
    let mut restarted = false;
    let mut i = 0;
    while i < frames {
        if i + 1 == frames {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(device, &frame)?;
        i += 1;
        if !restarted && stream_busy(renderer) {
            restarted = true;
            while stream_busy(renderer) && settle_frames < STREAM_SETTLE_MAX_FRAMES {
                renderer.render_offscreen(device, &frame)?;
                settle_frames += 1;
            }
            if stream_busy(renderer) {
                tracing::warn!(
                    target: "lookdev",
                    "texture streaming still busy after {settle_frames} settle frames; capturing anyway"
                );
            }
            renderer.camera_cut();
            i = 0;
        }
    }
    let hdr = renderer
        .take_hdr_capture(device)?
        .ok_or_else(|| LookdevError::Config("the frame ran no post stack: no HDR to capture".into()))?;
    // Diagnostic: HERO_TIMING=<frames> times that many more frames of the settled shot on the
    // GPU and prints the medians (cost checks of shadow and material changes).
    if let Some(n) = std::env::var("HERO_TIMING")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        renderer.set_gpu_timing(device, true);
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n + 4 {
            renderer.render_offscreen(device, &frame)?;
            samples.push(renderer.gpu_timings());
        }
        // The first frames publish no (or stale) timestamps.
        let samples = &samples[4..];
        let median = |f: fn(&regia_render::GpuTimings) -> f32| {
            let mut v: Vec<f32> = samples.iter().map(f).collect();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        println!(
            "hero_ref timing ({n} frames, medians): shadows {:.3} ms, prepass {:.3} ms, forward {:.3} ms, \
             contact {:.3} ms, scene {:.3} ms, frame {:.3} ms",
            median(|t| t.shadows_ms),
            median(|t| t.prepass_ms),
            median(|t| t.forward_ms),
            median(|t| t.contact_shadows_ms),
            median(|t| t.scene_ms),
            median(|t| t.frame_ms),
        );
        renderer.set_gpu_timing(device, false);
    }
    Ok(StudioFrame {
        hdr,
        ground_luminance: scene.ground_luminance,
        settle_frames,
    })
}

/// Frames [`render_studio`] renders at most to let texture streaming go idle before it restarts
/// the shot (a 4 K set grows one level at a time, each level fading in over 24 frames).
const STREAM_SETTLE_MAX_FRAMES: u32 = 1200;

/// Material texture streaming is filling or fading a set.
fn stream_busy(renderer: &FrameRenderer) -> bool {
    let report = renderer.stream_report();
    report.pending + report.fading > 0
}

/// Path-traces the same studio with the engine's own BRDF (`regia_pt`): the uniform world as
/// the sky, the ground plane as real geometry, a sun disc of `sun_angular_radius` radians. The
/// beauty in cd/m², `width × height`, top row first.
///
/// # Errors
///
/// The studio could not be built or the trace failed.
pub fn trace_studio(
    tracer: &regia_pt::PathTracer,
    model: &HeroModel,
    setup: &StudioSetup,
    spp: u32,
    sun_angular_radius: f32,
) -> Result<Vec<[f32; 3]>, LookdevError> {
    Ok(trace_studio_image(tracer, model, setup, spp, sun_angular_radius)?.beauty)
}

/// [`trace_studio`] with at most `max_bounces` path vertices: 1 is direct light at the first
/// hit only, which on the [`AuxPass::Sun`] model and setup is cos θ × the sun disc's
/// visibility, the tracer's counterpart of the raster's sun pass.
///
/// # Errors
///
/// The studio could not be built or the trace failed.
pub fn trace_studio_bounces(
    tracer: &regia_pt::PathTracer,
    model: &HeroModel,
    setup: &StudioSetup,
    spp: u32,
    sun_angular_radius: f32,
    max_bounces: u32,
) -> Result<Vec<[f32; 3]>, LookdevError> {
    Ok(trace_studio_image_bounces(tracer, model, setup, spp, sun_angular_radius, max_bounces)?.beauty)
}

/// [`trace_studio`] with every AOV (sun visibility, normals, depth, emission seen directly).
/// Diagnostic: `HERO_PT_BOUNCES=<n>` caps the path length (1 = direct light only; default 8).
///
/// # Errors
///
/// The studio could not be built or the trace failed.
pub fn trace_studio_image(
    tracer: &regia_pt::PathTracer,
    model: &HeroModel,
    setup: &StudioSetup,
    spp: u32,
    sun_angular_radius: f32,
) -> Result<regia_pt::PtImage, LookdevError> {
    let max_bounces = std::env::var("HERO_PT_BOUNCES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    trace_studio_image_bounces(tracer, model, setup, spp, sun_angular_radius, max_bounces)
}

/// [`trace_studio_image`] with at most `max_bounces` path vertices (see [`trace_studio_bounces`]).
///
/// # Errors
///
/// The studio could not be built or the trace failed.
pub fn trace_studio_image_bounces(
    tracer: &regia_pt::PathTracer,
    model: &HeroModel,
    setup: &StudioSetup,
    spp: u32,
    sun_angular_radius: f32,
    max_bounces: u32,
) -> Result<regia_pt::PtImage, LookdevError> {
    let scene = studio_scene(model, setup)?;
    let frame = scene.lab.as_frame_scene_with_quality(scene.quality);
    let sky = match setup.sky {
        StudioSky::Flat { lux } => regia_pt::PtSky::Uniform([lux / std::f32::consts::PI; 3]),
        StudioSky::Uniform { radiance } => regia_pt::PtSky::Uniform(radiance),
        StudioSky::Atmosphere => regia_pt::PtSky::None,
    };
    let pt_scene = regia_pt::extract::from_frame(
        &frame,
        &regia_pt::extract::ExtractOptions {
            sky: Some(sky),
            sun_angular_radius,
            terrain: None,
            skip_blended: false,
            lens: None,
            media: regia_pt::PtMedia::default(),
            sky_bodies: Vec::new(),
            // A studio reference has no star field.
            stars: 0.0,
            ..regia_pt::extract::ExtractOptions::default()
        },
    );
    let settings = regia_pt::PtSettings {
        spp,
        max_bounces: max_bounces.max(1),
        seed: 1,
        rr_start: 3,
        jitter: true,
        samples_per_submit: 4,
    };
    let mut session = tracer
        .begin(&pt_scene, &settings)
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    session
        .run_with(spp, |_| true)
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    session.image().map_err(|e| LookdevError::Config(e.to_string()))
}

/// `hdr` divided by `unit` (normally the sunlit ground luminance): a reflectance-like image
/// that two renderers with different exposures and units can be compared in.
#[must_use]
pub fn reflectance_image(hdr: &[[f32; 3]], unit: f32) -> Vec<[f32; 3]> {
    let k = 1.0 / unit.max(1e-9);
    hdr.iter().map(|c| [c[0] * k, c[1] * k, c[2] * k]).collect()
}

/// Blender's "Standard" view of linear RGB: the sRGB transfer function (IEC 61966-2-1),
/// clipped, no tone curve — the transform the references' PNGs use. RGBA8, top row first.
#[must_use]
pub fn standard_view(linear: &[[f32; 3]], scale: f32) -> Vec<u8> {
    let enc = |v: f32| {
        let v = (v * scale).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0).round() as u8
    };
    let mut out = Vec::with_capacity(linear.len() * 4);
    for c in linear {
        out.extend_from_slice(&[enc(c[0]), enc(c[1]), enc(c[2]), 255]);
    }
    out
}

/// A Blender (Z-up, right-handed) point or direction in the engine's frame (Y-up,
/// right-handed): glTF's export convention, `(x, y, z) -> (x, z, -y)`.
#[must_use]
pub fn blender_to_engine(p: [f32; 3]) -> [f32; 3] {
    [p[0], p[2], -p[1]]
}

/// A Blender sun azimuth (0° = +Y, increasing toward +X) as [`toward_sun`]'s azimuth
/// (0° = +Z, increasing toward +X): Blender's +Y is the engine's −Z.
#[must_use]
pub fn blender_azimuth_to_engine(azimuth_deg: f32) -> f32 {
    180.0 - azimuth_deg
}

/// Vertical field of view (radians) of a Blender camera with sensor fit `AUTO`: the sensor
/// width spans the image's longer side.
#[must_use]
pub fn blender_fov_y(focal_mm: f32, sensor_mm: f32, width: u32, height: u32) -> f32 {
    let half = 0.5 * sensor_mm / focal_mm.max(1e-3);
    let aspect = height as f32 / width.max(1) as f32;
    2.0 * if width >= height { half * aspect } else { half }.atan()
}

/// [`CyclesRef::units`] by default: ten times the references' nominal 93 lm/W.
pub const DEFAULT_UNITS: f32 = 930.0;

/// One of DO27's Cycles hero references, read from its `<asset>_ref.json`: the studio in the
/// engine's frame and photometric units (Blender radiometric units × the reference's luminous
/// efficacy), its cameras, and the glTF files it drew.
#[derive(Debug, Clone, PartialEq)]
pub struct CyclesRef {
    /// Asset name.
    pub asset: String,
    /// glTF file names the reference drew (LOD0 or unsuffixed only).
    pub files: Vec<String>,
    /// The asset's folder in DO27's tree (`assets_ferrite/<kind>/<name>`), when the JSON
    /// states it: picks between same-named copies in the drop.
    pub source_dir: Option<String>,
    /// Sun elevation, degrees.
    pub sun_elevation_deg: f32,
    /// Sun azimuth in the engine's convention ([`toward_sun`]).
    pub sun_azimuth_deg: f32,
    /// Sun illuminance normal to the ray, lux.
    pub sun_lux: f32,
    /// The world.
    pub sky: StudioSky,
    /// Ground albedo.
    pub ground_albedo: f32,
    /// Render width.
    pub width: u32,
    /// Render height.
    pub height: u32,
    /// Exposure (EV) the reference applied to its PNG.
    pub display_exposure_ev: f32,
    /// Lumens per Blender watt (the reference's nominal luminous efficacy).
    pub efficacy: f32,
    /// Blender emission strength per glTF nit the reference used
    /// (`emission_conversion.factor`, 1/93 in DO27's refreshed set); `None` in the first set,
    /// which fed the glTF strength to Cycles unconverted.
    pub emission_factor: Option<f32>,
    /// The studio's photometric scale: cd/m² (and lux) per Blender radiometric unit. The
    /// comparison is in ground units, so any scale works; [`DEFAULT_UNITS`] puts the sun at
    /// daylight (93 klx) and the ground bounce well inside the IBL's 8-bit range.
    pub units: f32,
    /// Cameras: name, eye, target (engine frame, metres) and vertical field of view (radians).
    pub cameras: Vec<(String, [f32; 3], [f32; 3], f32)>,
}

impl CyclesRef {
    /// Parses a reference JSON.
    ///
    /// # Errors
    ///
    /// Malformed JSON or a missing field.
    pub fn parse(text: &str) -> Result<Self, LookdevError> {
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| LookdevError::Config(format!("reference JSON: {e}")))?;
        let missing = |what: &str| LookdevError::Config(format!("reference JSON: no {what}"));
        let num = |x: &serde_json::Value, what: &str| -> Result<f32, LookdevError> {
            x.as_f64().map(|f| f as f32).ok_or_else(|| missing(what))
        };
        let vec3 = |x: &serde_json::Value, what: &str| -> Result<[f32; 3], LookdevError> {
            Ok([num(&x[0], what)?, num(&x[1], what)?, num(&x[2], what)?])
        };
        let sun = &v["sun"];
        let efficacy = sun["luminous_efficacy_nominal_lm_per_w"]
            .as_f64()
            .map_or(93.0, |f| f as f32);
        let units = DEFAULT_UNITS;
        let sun_lux = match sun["energy_wm2_normal"].as_f64() {
            Some(e) => e as f32 * units,
            None => num(&sun["direct_normal_illuminance_lux"], "sun energy")? / efficacy * units,
        };
        let sky_type = v["sky"]["type"].as_str().unwrap_or_default().to_ascii_lowercase();
        let sky = if sky_type.contains("nishita") && !sky_type.contains("no nishita") {
            StudioSky::Atmosphere
        } else {
            let c = vec3(&v["sky"]["rgb"], "sky rgb")?;
            StudioSky::Uniform {
                radiance: c.map(|x| x * units),
            }
        };
        let (width, height) = v["render"]["resolution"]
            .as_str()
            .and_then(|r| r.split_once('x'))
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .ok_or_else(|| missing("render resolution"))?;
        let mut cameras = Vec::new();
        if let Some(cams) = v["cameras"].as_object() {
            for (name, c) in cams {
                let fov = blender_fov_y(
                    num(&c["focal_length_mm"], "focal length")?,
                    c["sensor_width_mm"].as_f64().map_or(36.0, |f| f as f32),
                    width,
                    height,
                );
                cameras.push((
                    name.clone(),
                    blender_to_engine(vec3(&c["position"], "camera position")?),
                    blender_to_engine(vec3(&c["look_at"], "camera look_at")?),
                    fov,
                ));
            }
        }
        let files = v["files"]
            .as_object()
            .map(|f| f.keys().filter(|k| !is_lower_lod(k)).cloned().collect())
            .unwrap_or_default();
        Ok(Self {
            asset: v["asset"].as_str().ok_or_else(|| missing("asset"))?.to_string(),
            files,
            source_dir: v["source_dir"].as_str().map(str::to_string),
            sun_elevation_deg: num(&sun["elevation_deg"], "sun elevation")?,
            sun_azimuth_deg: blender_azimuth_to_engine(num(&sun["azimuth_deg"], "sun azimuth")?),
            sun_lux,
            sky,
            ground_albedo: v["ground"]["albedo"].as_f64().map_or(0.18, |f| f as f32),
            width,
            height,
            display_exposure_ev: v["display_exposure_ev"].as_f64().map_or(0.0, |f| f as f32),
            efficacy,
            emission_factor: v["emission_conversion"]["factor"].as_f64().map(|f| f as f32),
            units,
            cameras,
        })
    }

    /// The studio for one camera.
    #[must_use]
    pub fn setup(&self, eye: [f32; 3], target: [f32; 3], fov_y: f32) -> StudioSetup {
        StudioSetup {
            sun_elevation_deg: self.sun_elevation_deg,
            sun_azimuth_deg: self.sun_azimuth_deg,
            sun_lux: self.sun_lux,
            sky: self.sky,
            ground_albedo: self.ground_albedo,
            eye,
            target,
            fov_y,
            width: self.width,
            height: self.height,
            frames: 32,
            emissive_scale: 1.0,
            taa: true,
            exposure_ev: None,
        }
    }

    /// Scale from cd/m² to the reference PNG's linear values (its display exposure, Blender
    /// units).
    #[must_use]
    pub fn png_scale(&self) -> f32 {
        2f32.powf(self.display_exposure_ev) / self.units.max(1e-6)
    }

    /// Rescales the studio to `units` cd/m² per Blender unit.
    pub fn set_units(&mut self, units: f32) {
        let k = units / self.units.max(1e-9);
        self.sun_lux *= k;
        if let StudioSky::Uniform { radiance } = &mut self.sky {
            *radiance = radiance.map(|r| r * k);
        }
        self.units = units;
    }

    /// The listed files found by name under `content` (recursively), and the names not found
    /// (or not glTF). The drop holds several copies of some files (`hero/…` and the asset
    /// folder, not always identical): the copy in the reference's `source_dir` (its path under
    /// `content` with the `assets_ferrite/` prefix dropped) wins, else the first by path.
    #[must_use]
    pub fn resolve_files(&self, content: &std::path::Path) -> (Vec<std::path::PathBuf>, Vec<String>) {
        let mut index: std::collections::HashMap<String, Vec<std::path::PathBuf>> =
            std::collections::HashMap::new();
        let mut stack = vec![content.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                    index.entry(n.to_string()).or_default().push(p);
                }
            }
        }
        let home = self.source_dir.as_deref().map(|d| {
            let d = d.trim_matches('/');
            content.join(d.strip_prefix("assets_ferrite/").unwrap_or(d))
        });
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for f in &self.files {
            let pick = index.get_mut(f).and_then(|copies| {
                copies.sort();
                home.as_ref()
                    .and_then(|h| copies.iter().find(|p| p.parent() == Some(h.as_path())))
                    .or_else(|| copies.first())
                    .cloned()
            });
            match pick {
                Some(p) if f.ends_with(".gltf") => found.push(p),
                _ => missing.push(f.clone()),
            }
        }
        found.sort();
        (found, missing)
    }

    /// Material names in the order [`crate::hero::load_files`] appends them for `files`.
    #[must_use]
    pub fn material_names(&self, files: &[std::path::PathBuf]) -> Vec<String> {
        let mut out = Vec::new();
        for f in files {
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let doc: Option<serde_json::Value> = std::fs::read_to_string(f)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok());
            let mats = doc
                .as_ref()
                .and_then(|d| d["materials"].as_array().cloned())
                .unwrap_or_default();
            if mats.is_empty() {
                out.push(format!("{stem}/<default>"));
            }
            for (i, m) in mats.iter().enumerate() {
                out.push(format!(
                    "{stem}/{}",
                    m["name"].as_str().map_or_else(|| format!("#{i}"), str::to_string)
                ));
            }
        }
        out
    }
}

/// `_LOD1` … `_LOD9` in a file name (the references list every LOD; only LOD0 is drawn).
fn is_lower_lod(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    (1..10).any(|i| n.contains(&format!("_lod{i}")))
}

/// An auxiliary pass of a studio frame, read back aligned with the shaded frame so the
/// comparison can split the asset by material, surface class and sun visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxPass {
    /// Material index `i` as `(i % 8 + 1, i / 8 % 8 + 1, i / 64 + 1)` cd/m² (small integers
    /// that survive any HDR format); unlit.
    Ids,
    /// Linear base colour (factor × texture); unlit.
    Albedo,
    /// `(1, roughness factor, metallic factor)` × the sRGB-decoded ORM texture (or the factors
    /// alone without one); unlit. Re-encode the texture part with the sRGB curve to read it.
    Orm,
    /// Every surface white Lambertian (normal maps kept) under the sun alone: radiance ÷
    /// (sun lux / π) is cos θ × sun visibility.
    Sun,
}

/// `model` with every material replaced for `pass` (see [`AuxPass`]). Render [`AuxPass::Sun`]
/// with the setup's sun, a black sky and a black ground ([`aux_setup`]).
#[must_use]
pub fn aux_model(model: &HeroModel, pass: AuxPass) -> HeroModel {
    let materials = model
        .materials
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let masked = m.alpha_mode == regia_render::ALPHA_MASK;
            let base = LabMaterial {
                base_color: [0.0, 0.0, 0.0, 1.0],
                metallic: 0.0,
                roughness: 1.0,
                emissive_intensity: 1.0,
                alpha_mode: if masked {
                    regia_render::ALPHA_MASK
                } else {
                    regia_render::ALPHA_OPAQUE
                },
                alpha_cutoff: m.alpha_cutoff,
                // Alpha-tested cut-outs keep their coverage.
                albedo: masked.then(|| m.albedo.clone()).flatten(),
                ..LabMaterial::EMPTY
            };
            match pass {
                AuxPass::Ids => LabMaterial {
                    emissive: [(i % 8 + 1) as f32, (i / 8 % 8 + 1) as f32, (i / 64 + 1) as f32],
                    ..base
                },
                AuxPass::Albedo => LabMaterial {
                    emissive: [m.base_color[0], m.base_color[1], m.base_color[2]],
                    emissive_map: m.albedo.clone(),
                    ..base
                },
                AuxPass::Orm => LabMaterial {
                    emissive: [1.0, m.roughness, m.metallic],
                    emissive_map: m.occlusion_map.clone().filter(|_| m.orm_packed),
                    ..base
                },
                AuxPass::Sun => LabMaterial {
                    base_color: [1.0, 1.0, 1.0, 1.0],
                    emissive_intensity: 0.0,
                    normal_map: m.normal_map.clone(),
                    normal_scale: m.normal_scale,
                    ..base
                },
            }
        })
        .collect();
    HeroModel {
        meshes: model.meshes.clone(),
        materials,
        min: model.min,
        max: model.max,
    }
}

/// `setup` for an auxiliary pass: the sun only for [`AuxPass::Sun`], no light at all for the
/// unlit passes; never temporal (index and class passes must not blend across pixels).
#[must_use]
pub fn aux_setup(setup: &StudioSetup, pass: AuxPass) -> StudioSetup {
    StudioSetup {
        sun_lux: if pass == AuxPass::Sun { setup.sun_lux } else { 0.0 },
        sky: StudioSky::Uniform { radiance: [0.0; 3] },
        ground_albedo: 0.0,
        emissive_scale: 1.0,
        taa: false,
        frames: 4,
        exposure_ev: Some(
            setup.exposure_ev.unwrap_or_else(|| {
                regia_render::ev100_from_luminance(flat_ground_luminance(setup).max(1e-3))
            }),
        ),
        ..setup.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sun_direction_and_flat_ground_match_the_setup() {
        let s = toward_sun(35.0, 30.0);
        assert!((s.length() - 1.0).abs() < 1e-5);
        assert!((s.y - 35f32.to_radians().sin()).abs() < 1e-5);
        assert!(s.x > 0.0 && s.z > 0.0, "{s:?}");
        let setup = StudioSetup {
            sun_elevation_deg: 30.0,
            sun_azimuth_deg: 0.0,
            sun_lux: 100_000.0,
            sky: StudioSky::Flat { lux: 20_000.0 },
            ground_albedo: 0.18,
            eye: [0.0, 1.0, -5.0],
            target: [0.0; 3],
            fov_y: 0.6,
            width: 64,
            height: 64,
            frames: 1,
            emissive_scale: 1.0,
            taa: false,
            exposure_ev: None,
        };
        // 0.18 · (100 klx · sin 30° + 20 klx) / π
        let expected = 0.18 * 70_000.0 / std::f32::consts::PI;
        assert!((flat_ground_luminance(&setup) - expected).abs() < 1e-2);
    }

    #[test]
    fn a_cycles_reference_maps_onto_the_engine_frame() {
        let json = r#"{"asset":"t","files":{"A_LOD0.gltf":"x","A_LOD1.gltf":"y","B.gltf":"z"},
            "sun":{"elevation_deg":35.0,"azimuth_deg":30.0,"energy_wm2_normal":100.0,
                   "direction_from_origin":[0.409576,0.709406,0.573576],
                   "direct_normal_illuminance_lux":9300.0,"luminous_efficacy_nominal_lm_per_w":93.0},
            "sky":{"type":"flat color (no Nishita, no sun)","rgb":[0.02,0.02,0.03]},
            "ground":{"albedo":0.18},"display_exposure_ev":-4.0,
            "render":{"resolution":"1536x1152"},
            "cameras":{"side":{"position":[24.0,0.0,6.0],"look_at":[0.0,0.0,4.0],
                       "focal_length_mm":50.0,"sensor_width_mm":36.0}}}"#;
        let r = CyclesRef::parse(json).expect("parses");
        // The reference's own sun direction (Blender frame) is the engine's toward_sun.
        let want = Vec3::from_array(blender_to_engine([0.409576, 0.709406, 0.573576]));
        let got = toward_sun(r.sun_elevation_deg, r.sun_azimuth_deg);
        assert!((got - want).length() < 1e-4, "{got:?} vs {want:?}");
        assert_eq!(r.sun_lux, 100.0 * DEFAULT_UNITS);
        assert_eq!(r.files, vec!["A_LOD0.gltf".to_string(), "B.gltf".to_string()]);
        let StudioSky::Uniform { radiance } = r.sky else {
            panic!("flat world");
        };
        assert!((radiance[2] - 0.03 * DEFAULT_UNITS).abs() < 1e-3);
        let (name, eye, target, fov) = &r.cameras[0];
        assert_eq!(name, "side");
        assert_eq!(*eye, [24.0, 6.0, -0.0]);
        assert_eq!(*target, [0.0, 4.0, -0.0]);
        // 50 mm on a 36 mm sensor across 1536 px: 39.6° horizontal, 30.2° vertical at 4:3.
        assert!((fov.to_degrees() - 30.18).abs() < 0.05, "{}", fov.to_degrees());
        // PNG: cd/m² ÷ units × 2^-4.
        assert!((r.png_scale() - 1.0 / (DEFAULT_UNITS * 16.0)).abs() < 1e-9);
        let mut r93 = r.clone();
        r93.set_units(93.0);
        assert!((r93.sun_lux - 9300.0).abs() < 1e-2);
        // The first set has no emission conversion; the refreshed one states nits / 93.
        assert_eq!(r.emission_factor, None);
        let refreshed = json.replacen(
            r#""ground":"#,
            r#""emission_conversion":{"factor":0.01075269},"ground":"#,
            1,
        );
        let r2 = CyclesRef::parse(&refreshed).expect("parses");
        assert!((r2.emission_factor.unwrap_or(0.0) - 1.0 / 93.0).abs() < 1e-6);
    }

    #[test]
    fn resolve_files_prefers_the_copy_in_the_source_dir() {
        let root = std::env::temp_dir().join(format!("hero_ref_resolve_{}", std::process::id()));
        let home = root.join("buildings/guntower");
        let stale = root.join("hero/buildings/guntower");
        for d in [&home, &stale] {
            std::fs::create_dir_all(d).expect("temp dir");
            std::fs::write(d.join("A_LOD0.gltf"), "{}").expect("temp file");
        }
        let json = r#"{"asset":"a","source_dir":"assets_ferrite/buildings/guntower",
            "files":{"A_LOD0.gltf":"x","Gone.gltf":"y"},
            "sun":{"elevation_deg":35.0,"azimuth_deg":30.0,"energy_wm2_normal":100.0},
            "sky":{"type":"flat color","rgb":[0.0,0.0,0.0]},"render":{"resolution":"64x48"}}"#;
        let r = CyclesRef::parse(json).expect("parses");
        let (found, missing) = r.resolve_files(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found, vec![home.join("A_LOD0.gltf")]);
        assert_eq!(missing, vec!["Gone.gltf".to_string()]);
    }

    #[test]
    fn standard_view_is_the_srgb_transfer_clipped() {
        let px = standard_view(&[[0.0, 0.18, 1.0], [2.0, 0.002, 0.5]], 1.0);
        assert_eq!(&px[..4], &[0, 118, 255, 255]);
        assert_eq!(px[4], 255);
        assert_eq!(px[5], 7);
        assert_eq!(px[6], 188);
    }
}
