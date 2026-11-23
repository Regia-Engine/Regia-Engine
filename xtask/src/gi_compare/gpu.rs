// SPDX-License-Identifier: MPL-2.0
//! The GPU half of `gi-compare`: the in-process bake, the raster frames through
//! `FrameRenderer` (the renderer the agent's `render.set_gi_mode` drives) and the reference
//! traces through `regia_pt`. Everything here needs an adapter; the CPU half is tested alone.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use regia_asset::baked_gi::BakedGiVolume;
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::bake_gi_volume;
use regia_pt::{PathTracer, PtImage, PtScene, PtSettings};
use regia_render::baked_gi::{GiCapsule, MAX_GI_OCCLUDERS};
use regia_render::{
    BakedGiSettings, FrameQuality, FrameRenderer, LabScene, RenderDevice, RenderProfile, RenderProfilePreset,
};

use super::fixture::Fixture;
use super::summary::{BakeSummary, GiStatus};

/// Frames rendered for a baked or direct-only capture (the second has settled shadows; the bake
/// itself needs no settling).
pub const STILL_FRAMES: u32 = 2;

/// The device and the compiled tracer.
pub struct Gpu {
    /// The headless device (`REGIA_GPU_*` pick the adapter).
    pub device: RenderDevice,
    /// The reference tracer on it.
    pub tracer: PathTracer,
}

impl Gpu {
    /// Opens the headless device and compiles the tracer.
    ///
    /// # Errors
    ///
    /// No adapter, or the tracer does not compile.
    pub fn new() -> Result<Self> {
        let device =
            RenderDevice::headless().map_err(|e| anyhow!("[gpu_unavailable] no headless device: {e}"))?;
        let tracer = PathTracer::new(&device).map_err(|e| anyhow!("[gpu_unavailable] path tracer: {e}"))?;
        Ok(Self { device, tracer })
    }

    /// Adapter name and backend.
    #[must_use]
    pub fn adapter(&self) -> String {
        let info = self.device.adapter.get_info();
        format!("{} ({:?})", info.name, info.backend)
    }
}

/// `--emissive-lights` override of the emissive area light budget (`u32::MAX` = Desktop High's).
static EMISSIVE_LIGHTS: AtomicU32 = AtomicU32::new(u32::MAX);

/// Overrides the emissive area light budget of every raster capture (`None` = Desktop High's
/// own). Set once, before the first capture.
pub fn set_emissive_lights(n: Option<u32>) {
    EMISSIVE_LIGHTS.store(n.unwrap_or(u32::MAX), Ordering::Relaxed);
}

/// The frame quality of every raster capture: Desktop High (the look the bake ships under) at
/// native resolution without temporal upscaling (one still frame, no history), without Hor+ (so
/// the field of view is exactly the tracer's) and without volumetric fog or clouds (the tracer's
/// fixture scenes have no media). `--emissive-lights` replaces the emissive area light budget.
#[must_use]
pub fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    let lights = EMISSIVE_LIGHTS.load(Ordering::Relaxed);
    if lights != u32::MAX {
        q.emissive_lights = lights;
    }
    q
}

/// The tracer's scene of a raster frame: the engine's extraction (`specs/27` § 1.1, "same
/// inputs"); the flat ambient becomes a uniform sky of radiance `ambient / π`.
#[must_use]
pub fn traced_scene(lab: &LabScene) -> PtScene {
    from_frame(
        &lab.as_frame_scene_with_quality(quality()),
        &ExtractOptions::default(),
    )
}

/// Bakes the fixture's layout in `scene` (the frame without dynamic shapes).
///
/// # Errors
///
/// The layout is invalid or the bake fails.
pub fn bake(gpu: &Gpu, fixture: &Fixture, scene: &PtScene) -> Result<(Arc<BakedGiVolume>, BakeSummary)> {
    let layout = fixture
        .volume_layout()
        .map_err(|e| anyhow!("[{}] {e}", e.code()))?;
    println!("gi-compare: baking {} probes", layout.probe_count());
    let mut last = String::new();
    let bake = bake_gi_volume(
        &gpu.tracer,
        scene,
        &layout,
        &fixture.bake_settings(),
        |stage, done, total| {
            let step = (done * 10).checked_div(total).unwrap_or(10);
            let line = format!("  bake {stage}: {step}0 %");
            if line != last {
                println!("{line}");
                last = line;
            }
            true
        },
    )
    .map_err(|e| anyhow!("[bake_failed] {e}"))?;
    let r = &bake.report;
    let summary = BakeSummary {
        source: "baked".into(),
        probes: r.probes,
        valid: r.valid,
        inside_geometry: r.inside_geometry,
        under_terrain: r.under_terrain,
        relocated: r.relocated,
        rejected_rays: r.rejected_rays,
        seconds: r.seconds,
    };
    Ok((Arc::new(bake.volume), summary))
}

/// Loads a `.fgiv` instead of baking.
///
/// # Errors
///
/// The file is missing or not a readable volume.
pub fn load_volume(path: &std::path::Path) -> Result<(Arc<BakedGiVolume>, BakeSummary)> {
    let bytes =
        std::fs::read(path).with_context(|| format!("[volume_unreadable] reading {}", path.display()))?;
    let volume = BakedGiVolume::from_bytes(&bytes)
        .map_err(|e| anyhow!("[volume_unreadable] {}: {e}", path.display()))?;
    let summary = BakeSummary {
        source: path.display().to_string(),
        probes: volume.probe_count(),
        ..BakeSummary::default()
    };
    Ok((Arc::new(volume), summary))
}

/// A linear capture and how long its frames took.
pub struct Capture {
    /// Row-major linear RGB, cd/m², before exposure and the tonemap.
    pub rgb: Vec<[f32; 3]>,
    /// Wall time of every frame rendered for it, milliseconds.
    pub ms: f64,
}

fn capture(
    gpu: &Gpu,
    renderer: &mut FrameRenderer,
    lab: &LabScene,
    q: FrameQuality,
    frames: u32,
) -> Result<Capture> {
    let _span = tracing::info_span!("gi_compare_capture", frames).entered();
    let started = Instant::now();
    let frames = frames.max(1);
    for i in 0..frames {
        if i + 1 == frames {
            renderer.request_hdr_capture();
        }
        renderer
            .render_offscreen(&gpu.device, &lab.as_frame_scene_with_quality(q))
            .map_err(|e| anyhow!("[render_failed] {e}"))?;
    }
    let hdr = renderer
        .take_hdr_capture(&gpu.device)
        .map_err(|e| anyhow!("[render_failed] HDR readback: {e}"))?
        .ok_or_else(|| anyhow!("[render_failed] the frame recorded no HDR capture (no post stack)"))?;
    super::input::check_capture((lab.width, lab.height), (hdr.width, hdr.height), &hdr.rgb)?;
    Ok(Capture {
        rgb: hdr.rgb,
        ms: started.elapsed().as_secs_f64() * 1e3,
    })
}

/// The baked frame: the volume installed, its occluders set, the baked look on (as
/// `render.set_gi_mode {"mode": "baked"}` selects it), and the status the agent's
/// `render.get_gi_mode` reports.
///
/// # Errors
///
/// Rendering or the readback failed.
pub fn baked_frame(
    gpu: &Gpu,
    lab: &LabScene,
    volume: &Arc<BakedGiVolume>,
    settings: BakedGiSettings,
    occluders: &[GiCapsule],
) -> Result<(Capture, GiStatus)> {
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume.clone()));
    renderer.set_baked_gi_settings(settings);
    renderer.set_baked_gi_enabled(Some(true));
    renderer.set_gi_occluders(occluders);
    let mut q = quality();
    q.baked_gi = true;
    let cap = capture(gpu, &mut renderer, lab, q, STILL_FRAMES)?;
    let s = renderer.baked_gi_status();
    let status = GiStatus {
        installed: s.installed,
        active: s.active,
        probes: s.probes,
        gpu_bytes: s.gpu_bytes,
        error: s.error.clone(),
        scales: s.scales.to_vec(),
        sun_mismatch_deg: s.sun_mismatch_deg,
        occluders: s.occluders,
        occluder_cap: MAX_GI_OCCLUDERS,
        occluders_dropped: s.occluders_dropped,
        interior_bricks: s.interior_bricks,
        emitters: s.emitters,
        emissive_lights: {
            let e = renderer.emissive_lights();
            (e.uploaded, e.baked)
        },
    };
    Ok((cap, status))
}

/// The live-GI frame for context: no volume, DDGI forced on (the `depth` look), `frames` frames so
/// the probes settle.
///
/// # Errors
///
/// Rendering or the readback failed.
pub fn ddgi_frame(gpu: &Gpu, lab: &LabScene, frames: u32) -> Result<Capture> {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(true));
    let mut q = quality();
    q.baked_gi = false;
    q.gi_probes = true;
    capture(gpu, &mut renderer, lab, q, frames)
}

/// The direct-only frame: no volume, no DDGI, no SSGI, and `lab` built without its flat ambient.
/// What it shows is the frame's analytic light (sun, local lights, emitters seen directly), the
/// part `baked − direct` removes to isolate the indirect light.
///
/// # Errors
///
/// Rendering or the readback failed.
pub fn direct_frame(gpu: &Gpu, lab: &LabScene) -> Result<Capture> {
    let mut renderer = FrameRenderer::new();
    renderer.set_ddgi_enabled(Some(false));
    renderer.set_baked_gi_enabled(Some(false));
    let mut q = quality();
    q.baked_gi = false;
    q.gi_probes = false;
    q.screen_space_gi = false;
    capture(gpu, &mut renderer, lab, q, STILL_FRAMES)
}

/// One reference trace and its wall time, milliseconds.
///
/// # Errors
///
/// The trace failed.
pub fn trace(
    gpu: &Gpu,
    scene: &PtScene,
    spp: u32,
    max_bounces: Option<u32>,
    what: &str,
) -> Result<(PtImage, f64)> {
    let defaults = PtSettings::default();
    let settings = PtSettings {
        spp,
        max_bounces: max_bounces.unwrap_or(defaults.max_bounces),
        ..defaults
    };
    let started = Instant::now();
    let mut session = gpu
        .tracer
        .begin(scene, &settings)
        .map_err(|e| anyhow!("[trace_failed] {e}"))?;
    let mut shown = 0u32;
    session
        .run_with(spp, |done| {
            let tenth = done * 10 / spp.max(1);
            if tenth > shown {
                shown = tenth;
                println!("  {what}: {done}/{spp} spp");
            }
            true
        })
        .map_err(|e| anyhow!("[trace_failed] {e}"))?;
    let image = session.image().map_err(|e| anyhow!("[trace_failed] {e}"))?;
    Ok((image, started.elapsed().as_secs_f64() * 1e3))
}

/// `scene` with every material's emission off: its `direct` AOV is the light the raster's
/// analytic terms draw (sun and local lights), without the emitters' first bounce.
#[must_use]
pub fn without_emitters(scene: &PtScene) -> PtScene {
    let mut s = scene.clone();
    for m in &mut s.materials {
        m.emissive = [0.0; 3];
    }
    s
}
