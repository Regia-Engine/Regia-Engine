// SPDX-License-Identifier: MPL-2.0
//! The terrain's pipeline warm (ADR-195 "Warm coverage"): a [`regia_render::PipelineWarmQueue`]
//! job that builds every terrain pipeline a map can draw with on a warm worker, before the host
//! makes its terrain.
//!
//! Terrain pipelines are the device's ([`regia_render::RenderDevice::shared_pipelines`]): the
//! offscreen set of [`super::TerrainGpu::new`], and per HDR colour format the shared-pass sets
//! (depth, cascades, colour, velocity, decal receiver) with and without the hole cut, each with
//! and without the local-light loop (`TerrainSharedDraw::set_local_lights_enabled`). The job
//! builds them through a tiny terrain, a set at a time with a quit check between sets, so a [`super::TerrainSharedDraw`] made later
//! on the frame thread (a map loaded behind the menu, a terrain rebuilt for the next map, a
//! hole cut mid-play) compiles none of them. Draws are unchanged: the pipelines are the ones
//! the terrain would have built itself.

use regia_app::prelude::*;
use regia_asset::Heightfield;
use regia_render::pipeline_warm::{WarmJob, warm_cancelled};
use regia_render::{RenderDevice, RenderProfile};

use super::TerrainSharedDraw;
use crate::cdlod::CdlodConfig;
use crate::layers::TerrainLayers;
use crate::sampler::TerrainMap;

/// The colour formats a runtime frame with `profile` draws the terrain into on `device`: the
/// HDR target (`Rgba16Float`, or `Rg11b10Ufloat` when the profile asks for it and the device
/// renders it), plus the format of every target the host registered for the warm
/// ([`regia_render::pipeline_warm::register_warm_target`]; a frame without post draws the
/// terrain straight into it, ADR-7885).
#[must_use]
pub fn terrain_warm_formats(profile: &RenderProfile, device: &RenderDevice) -> Vec<wgpu::TextureFormat> {
    let mut formats = vec![profile.hdr_color_format(&device.capabilities)];
    for format in regia_render::pipeline_warm::registered_warm_formats() {
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    formats
}

/// Builds the terrain pipelines for `formats` on `device` now ([`terrain_warm_job`] on the calling
/// thread). Returns the pipelines created (0 when a terrain or an earlier warm built them).
///
/// # Errors
///
/// The warm terrain could not be built (a device without the terrain's texture formats), or the
/// warm queue is shutting down (`"canceled at shutdown"`).
pub fn warm_terrain_pipelines(device: &RenderDevice, formats: &[wgpu::TextureFormat]) -> Result<u64, String> {
    let _span = tracing::info_span!("terrain_pipeline_warm").entered();
    // A quit stops the job between its compile batches (at most one set of five in flight):
    // shipping builds (`panic = "abort"`) never unwind out of a cancelled job.
    let check = || {
        if warm_cancelled() {
            Err(String::from("canceled at shutdown"))
        } else {
            Ok(())
        }
    };
    let before = regia_render::pipeline_cache::pipelines_created();
    check()?;
    let map = warm_map().map_err(|e| e.to_string())?;
    // The offscreen set. The draw exists only to compile pipelines: built with
    // `new_for_warm`, it does not publish its anisotropy path or LOD offset to the
    // process-global report, so a warm worker that runs after the host's terrain (and any
    // host override) cannot make `terrain.get` report the throwaway draw's automatic path.
    let mut draw =
        TerrainSharedDraw::new_for_warm(device, map, &TerrainLayers::default(), CdlodConfig::default(), 0)
            .map_err(|e| e.to_string())?;
    let gpu = draw.gpu_mut();
    for &format in formats {
        check()?;
        // The plain shared set (no hole mask yet, so not the hole-cut one with it).
        gpu.ensure_shared(format);
        check()?;
        gpu.warm_hole_set();
        // The same two without the local-light loop (a host that turns the lights off).
        check()?;
        gpu.warm_other_light_variant(false);
        check()?;
        gpu.warm_other_light_variant(true);
        check()?;
        gpu.warm_cloud_map_variant();
    }
    check()?;
    // The offscreen hole-cut pipeline (`TerrainGpu::set_holes`).
    gpu.warm_offscreen_holes();
    Ok(regia_render::pipeline_cache::pipelines_created().saturating_sub(before))
}

/// A [`regia_render::PipelineWarmQueue`] job ([`warm_terrain_pipelines`]): push it on the map
/// tier at boot, before the host builds its terrain. `TerrainPlugin` queues it by itself
/// ([`crate::TerrainWarm`]).
pub fn terrain_warm_job(formats: Vec<wgpu::TextureFormat>) -> WarmJob {
    Box::new(move |device: &RenderDevice| warm_terrain_pipelines(device, &formats).map(drop))
}

/// A 17×17 flat map over 32 m.
fn warm_map() -> Result<TerrainMap, crate::sampler::TerrainMapError> {
    let n = 17;
    TerrainMap::new(
        &Heightfield {
            width: n,
            height: n,
            samples: vec![32_768; (n * n) as usize],
        },
        32.0,
        8.0,
        0.0,
    )
}

/// Whether `TerrainPlugin` queues [`terrain_warm_job`] on the app's
/// [`regia_render::PipelineWarmQueue`] (map tier) once the render device exists (default on).
/// A game without a terrain on some platform, or one that queues the job itself, turns it off
/// before the first `Update`.
///
/// The job warms the HDR format of the current `RenderSettings` profile; when the host applies
/// settings that change it (saved settings switching `Rg11b10Ufloat` and `Rgba16Float`), the new
/// format is queued too. Frames without post draw the terrain into the window's format, which
/// is not warmed (it compiles at level load, on the thread building the terrain).
#[derive(Resource, Clone, Debug)]
pub struct TerrainWarm {
    /// Queue the job (default `true`).
    pub enabled: bool,
    /// The formats queued so far.
    formats: Vec<wgpu::TextureFormat>,
}

impl Default for TerrainWarm {
    fn default() -> Self {
        Self {
            enabled: true,
            formats: Vec::new(),
        }
    }
}

impl TerrainWarm {
    /// Whether the job has been queued.
    #[must_use]
    pub fn is_queued(&self) -> bool {
        !self.formats.is_empty()
    }
}

/// `Update`: queues [`terrain_warm_job`] once the render device and the warm queue exist, and
/// again for a new HDR format when `RenderSettings` change it.
pub fn queue_terrain_warm(
    mut warm: ResMut<TerrainWarm>,
    device: Option<Res<RenderDevice>>,
    queue: Option<Res<regia_render::PipelineWarmQueue>>,
    settings: Option<Res<regia_render::RenderSettings>>,
) {
    if !warm.enabled {
        return;
    }
    let (Some(device), Some(queue)) = (device, queue) else {
        return;
    };
    if warm.is_queued() && !settings.as_ref().is_some_and(Res::is_changed) {
        return;
    }
    let profile = settings.map_or_else(
        || RenderProfile::from_preset(regia_render::RenderProfilePreset::runtime_default()),
        |s| s.profile.clone(),
    );
    let formats: Vec<_> = terrain_warm_formats(&profile, &device)
        .into_iter()
        .filter(|format| !warm.formats.contains(format))
        .collect();
    if formats.is_empty() {
        return;
    }
    warm.formats.extend_from_slice(&formats);
    queue.push(
        regia_render::WarmTier::Map,
        "terrain pipelines",
        terrain_warm_job(formats),
    );
}

/// A configured terrain being built by [`prepare_terrain`] on the pipeline warm queue.
///
/// Poll [`Self::is_finished`] while presenting a loading screen, then call [`Self::take`].
/// Only a successful prepare makes [`Self::is_ready`] true. Errors, queue shutdown and an
/// unwinding builder finish with an error instead of leaving the host waiting indefinitely.
/// Dropping this handle discards the result; it does not interrupt a driver compile.
#[must_use = "poll the preparation and take its terrain before installing"]
pub struct TerrainPreparation {
    state: std::sync::Arc<std::sync::Mutex<PreparationState>>,
}

enum PreparationState {
    Pending,
    Finished(Result<Box<TerrainSharedDraw>, String>),
    Taken,
}

impl TerrainPreparation {
    /// Whether preparation has ended, successfully or with an error (also true after take).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        !matches!(
            *self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            PreparationState::Pending
        )
    }

    /// Whether a fully prepared draw is available. False on failure and after take.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(
            *self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            PreparationState::Finished(Ok(_))
        )
    }

    /// Takes the result once, or returns `None` while pending or after a previous take.
    ///
    /// A success is already boxed for `FrameRenderer::set_shared_draw`: taking and installing
    /// it performs no terrain construction, upload or pipeline compilation. Install it only
    /// on the queue's device. Retire any previous draw outside the frame-critical path if
    /// releasing its GPU resources would be expensive.
    pub fn take(&self) -> Option<Result<Box<TerrainSharedDraw>, String>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, PreparationState::Pending | PreparationState::Taken) {
            return None;
        }
        match std::mem::replace(&mut *state, PreparationState::Taken) {
            PreparationState::Finished(result) => Some(result),
            PreparationState::Pending | PreparationState::Taken => None,
        }
    }
}

/// Publishes a terminal result when a queued closure is discarded or unwinds as well as when
/// it returns. The expensive construction and destruction always happen outside the lock.
struct PreparationGuard {
    state: std::sync::Arc<std::sync::Mutex<PreparationState>>,
}

impl PreparationGuard {
    fn finish(&self, result: Result<Box<TerrainSharedDraw>, String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, PreparationState::Pending) {
            *state = PreparationState::Finished(result);
        }
    }
}

impl Drop for PreparationGuard {
    fn drop(&mut self) {
        self.finish(Err("terrain preparation canceled or builder panicked".into()));
    }
}

/// Prepares a terrain's GPU state and pipelines on the engine warm queue before installation.
///
/// `build` runs on a map-tier worker with the queue's device. Construct the draw there (using
/// `TerrainSharedDraw::new` or `TerrainSharedDraw::from_cooked`), upload its material and set
/// its holes, ground look, grass, impostors and virtual-texture policy before returning it.
/// The worker then calls `TerrainSharedDraw::prewarm` for `color_format`, including the
/// configured optional pipelines, and publishes the boxed draw through the returned handle.
/// All constructor and prewarm compiles count in `render.pipeline_warm`'s map tier.
///
/// Start the queue with the installation device using `PipelineWarmQueue::start`. Queueing
/// before start is supported. Its worker count/priority bounds concurrency; an individual
/// driver compile cannot be preempted. No driver work runs in this function or in `take`.
///
/// Use the first frame's actual color format and terrain settings. For profile-controlled
/// virtual textures call `draw.gpu_mut().set_virtual_texture_from_budget(&profile.terrain_budget)`
/// in `build`; for weather call `draw.gpu_mut().set_warm_surface_weather(true)`. Changing
/// configuration or device after preparation can require another prepare. Separate water-body
/// renderers are not owned by this draw and need their own warm job.
///
/// Synchronous constructors remain supported. Their pipeline creates are visible in the
/// warm telemetry's outside-queue counters; they can still block the calling thread.
pub fn prepare_terrain(
    queue: &regia_render::PipelineWarmQueue,
    color_format: wgpu::TextureFormat,
    build: impl FnOnce(&RenderDevice) -> Result<TerrainSharedDraw, String> + Send + 'static,
) -> TerrainPreparation {
    let state = std::sync::Arc::new(std::sync::Mutex::new(PreparationState::Pending));
    let guard = PreparationGuard { state: state.clone() };
    if queue.is_shutting_down() {
        drop(guard);
        return TerrainPreparation { state };
    }
    queue.push(regia_render::WarmTier::Map, "prepare terrain", move |device| {
        let _span = tracing::info_span!("prepare_terrain").entered();
        let result = (|| {
            let mut draw = build(device)?;
            if !std::sync::Arc::ptr_eq(&draw.gpu().shared_pipelines, device.shared_pipelines()) {
                return Err("terrain was constructed on a different render device".into());
            }
            if warm_cancelled() {
                return Err(regia_render::pipeline_warm::CANCELLED_AT_SHUTDOWN.into());
            }
            draw.prewarm(color_format);
            if draw.gpu().virtual_texture().is_some() && draw.gpu().vt_supported() && !draw.gpu().vt_active()
            {
                return Err("terrain virtual texture could not be prepared on this device".into());
            }
            if warm_cancelled() {
                return Err(regia_render::pipeline_warm::CANCELLED_AT_SHUTDOWN.into());
            }
            Ok(Box::new(draw))
        })();
        let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
        guard.finish(result);
        status
    });
    // `push` accepts jobs even after shutdown. Cover shutdown racing the initial check:
    // otherwise such an unrun job would not drop until the last queue handle is released.
    if queue.is_shutting_down() {
        PreparationGuard { state: state.clone() }
            .finish(Err(regia_render::pipeline_warm::CANCELLED_AT_SHUTDOWN.into()));
    }
    TerrainPreparation { state }
}
