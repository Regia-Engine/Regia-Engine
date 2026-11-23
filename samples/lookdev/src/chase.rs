// SPDX-License-Identifier: MIT-0
//! DO27's chase bench on DO27's own content, through the full runtime frame (`do27_chase`).
//!
//! A vehicle drives the map at 15 m/s (60 Hz steps) — The Switchbacks along its road
//! centreline (`setpieces/placement.json road_xz`) from the team-1 start, Bore Twelve from
//! the team-1 start toward the team-2 start — with a chase camera 9 m behind and 3.5 m up
//! (90° horizontal field of view). The frame is what the DO27 shell draws: the cooked
//! terrain through [`TerrainSharedDraw`], the `.scatter.ron` rocks with their contact map,
//! the map's set pieces, the map level (atmosphere, look, LUT), Desktop High native with
//! post, TAA, auto exposure and the froxel/aerial path. Two frames stay in flight, like a
//! presenting game; the target is an offscreen sRGB swapchain stand-in.
//!
//! Each run prints wall, CPU (encode / submit / total) and GPU per pass p50 / p99 over the
//! measured frames, plus the frame-path device calls after warm-up (pipelines and bind
//! groups must stay at zero).
//!
//! Like `look_r2`, the sim frame is drawn unchanged (the image would be mirrored; the cost
//! is not).

use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FrameRenderer, FrameTarget, LabScene, OutdoorParams, RenderDevice,
    SharedPassDraw,
};
use regia_terrain::{ContactMap, ScatterView, TerrainRelief, TerrainSharedDraw};

use crate::look_r2::{R2Planets, R2Shot, materials, shot_fog};
use crate::soak::{along, polyline_length};
use crate::{LookdevError, Planet};

/// Vehicle speed, m/s (DO27's chase bench).
pub const SPEED: f32 = 15.0;
/// Simulation step, seconds.
pub const DT: f32 = 1.0 / 60.0;

/// The runtime's headless device plus block compression (DO27's cooked terrain is
/// BC7/BC5/BC4). `headless_bcn_device` requests BC alone and drops the features the
/// runtime asks for (timestamps, indirect first instance, RG11B10), so the frame would
/// take other paths than the shell's and GPU timing would read zero.
pub fn runtime_device() -> Result<RenderDevice, LookdevError> {
    let base = RenderDevice::headless()?;
    let info = base.adapter.get_info();
    println!(
        "selected: {} (pci {}, driver {} {}; {:?})",
        info.name,
        info.device_pci_bus_id,
        info.driver,
        info.driver_info,
        base.adapter_selection()
    );
    let bc = wgpu::Features::TEXTURE_COMPRESSION_BC;
    if !base.adapter.features().contains(bc) || base.device.features().contains(bc) {
        return Ok(base);
    }
    let features = base.device.features() | bc;
    let (device, queue) = regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("do27 chase device"),
        required_limits: base.device.limits(),
        required_features: features,
        ..Default::default()
    }))
    .map_err(|e| LookdevError::Config(e.to_string()))?;
    let capabilities = regia_render::RenderCapabilities::from_adapter(&base.adapter, &device);
    Ok(RenderDevice::from_parts(
        base.instance,
        base.adapter,
        device,
        queue,
        capabilities,
    ))
}

/// One chase run's configuration.
#[derive(Debug, Clone)]
pub struct ChaseOptions {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Measured frames.
    pub frames: u32,
    /// Frames rendered before measuring (pipelines, exposure, TAA history, scatter tiles).
    pub warmup: u32,
    /// Park the camera at the start (the floor of a held view).
    pub held: bool,
    /// Distance along the route where the vehicle starts, metres.
    pub start_m: f32,
    /// Write the last frame (rendered once more, unmeasured) to this PNG.
    pub png: Option<std::path::PathBuf>,
    /// A UE parity plate camera instead of the chase: the plate's own eye, target and field
    /// of view, held, no vehicle, the rocks at the eye column dropped like `look_r2`.
    pub plate: Option<R2Shot>,
    /// Sway a plate camera's eye by up to 2 cm each frame so the static-screen cache never
    /// hits: every screen pass (SSAO, contact shadows, SSGI, SSR) runs as in a moving view.
    pub sway: bool,
    /// Wait for every frame before starting the next (no frames in flight): what a
    /// render-then-read-back loop does. The GPU idles between frames and may clock down.
    pub sync: bool,
    /// Sleep this long after each frame (with `sync`: the GPU idles in between, as in a
    /// loop that reads every frame back and writes it out).
    pub idle_ms: u32,
    /// A loaded host from the first measured frame on
    /// ([`regia_core::residency::MemorySqueeze`]). Starting after setup keeps the load itself
    /// fast; the pressure lands on the frames measured.
    pub memory_squeeze: Option<regia_core::residency::SqueezeSettings>,
    /// The profile's opt-in terrain shading cuts (`TerrainBudget::OFF` = the default look).
    /// Its `anisotropy` is not used: see [`Self::terrain_anisotropy`].
    pub budget: regia_render::TerrainBudget,
    /// The terrain's anisotropic filtering level (`TerrainBudget::anisotropy`, 0 = off);
    /// `None` keeps the tier's.
    pub terrain_anisotropy: Option<u8>,
    /// The vehicle's shadow policy (`All` = into every cascade it overlaps, the default).
    pub vehicle_shadow: regia_render::ShadowCast,
    /// Internal render scale override (`None` = the tier's profile). Below 1 the TAA
    /// frame is a temporal upscale.
    pub render_scale: Option<f32>,
    /// Temporal upscaler for the TAA frame (`specs/27` § 2.3): the TAAU default, TSR or FSR 3.1.
    pub upscaler: regia_render::TemporalUpscaler,
    /// TSR tuning (`None` = [`regia_render::TsrSettings::for_scale`] of the frame).
    pub tsr: Option<regia_render::TsrSettings>,
    /// FSR 3.1 tuning (`None` = the SDK defaults, [`regia_render::Fsr3Settings::default`]).
    pub fsr3: Option<regia_render::Fsr3Settings>,
    /// Redraw the sun cascades every frame instead of paging them. This also turns off
    /// the static-screen reuse, whose held-view composite stacks the aerial/fog pass
    /// frame after frame (a clean held-view reference for upscaler comparisons).
    pub no_static_reuse: bool,
    /// Draw rigid opaque meshes through the visibility buffer with clusters from this cooker
    /// (`None` = the default forward frame). The cooker lives outside the runtime layers, so
    /// a tool (the `vis_plates` example) passes it in.
    pub meshlets: Option<MeshletSource>,
    /// Drive the hitch soak script ([`crate::soak`]) instead of the road chase: `frames`
    /// comes from the script, cuts, time-of-day jumps and VFX/decal spawns are on.
    pub soak: Option<crate::soak::SoakOptions>,
    /// With [`ChaseOptions::soak`]: replace the plate-camera cuts with fixed cameras along this
    /// deterministic path ([`crate::soak_bundle::CameraPath`]).
    pub camera_path: Option<crate::soak_bundle::CameraPath>,
    /// Log every frame-path create with its label ([`ChaseRun::events`]).
    pub trace: bool,
    /// The terrain virtual texture (ADR-117; `None` = off, today's frame).
    pub vt: Option<regia_terrain::VirtualTextureSettings>,
    /// Prewarm before the first frame (`FrameRenderer::prewarm` with every scatter variant,
    /// [`RESERVE_INSTANCES`]) and stream scatter tiles at [`TILE_BUDGET`] per frame — what a
    /// game does at level load. Off reproduces the bare frame of 2026-10-07.
    pub prewarm: bool,
    /// Sky rebuilds on a worker (`FrameRenderer::set_async_sky`, the runtime's default).
    /// Off integrates them inside the frame, as the bench did on 2026-10-07.
    pub async_sky: bool,
    /// Install the soak's VFX layer and lay a permanent grid of decals around the view's
    /// target before the first frame (no soak needed): every frame runs the decal-receiver
    /// stamp over the view's rocks and the decal pass (an md5 check of that path).
    pub decals: bool,
    /// How the camera moves (the chase, a plate pan, an orbit around the vehicle).
    pub motion: crate::motion::Motion,
    /// Read every measured frame back and score the sequence (`crate::motion`). Frames
    /// are then rendered through the readback path, so the timings are not the bench's.
    pub seq: Option<crate::motion::SeqOptions>,
    /// Passes turned off for a diagnosis: any of `ssgi`, `ssao`, `ssr`, `translucent-shadows`,
    /// `contact`, `reflocc`, `bent`, `gi-probes`, `emissive`, `fog`, `clouds`,
    /// `taa`, `lodfade` (scatter LOD switches in one frame, no cross-fade) (the frame is no
    /// longer the shell's).
    pub off: Vec<String>,
    /// Opt-in renderer features turned on for a cost measurement ([`ON_FEATURES`]; the frame is
    /// no longer the shell's). Empty is the shell's frame.
    pub on: Vec<String>,
    /// The chase camera's lens (motion blur, depth of field), applied where the tier allows it.
    /// `None` is the shell's frame.
    pub lens: Option<regia_render::PhysicalCamera>,
    /// The render tier (`None` = `LOOK_R2_TIER`, [`crate::look_r2::tier_settings`]).
    pub preset: Option<regia_render::RenderProfilePreset>,
    /// Per-instance buffer reserve at prewarm ([`RESERVE_INSTANCES`]; a denser scatter needs more).
    pub reserve_instances: u32,
    /// Mars grade for this run (`None` = [`crate::look_r2::MarsGrade::from_env`]).
    ///
    /// The alpha smoke harness sets [`crate::look_r2::MarsGrade::NasaDark`] so the
    /// Switchbacks frame is graded without mutating process environment variables.
    pub mars_grade: Option<crate::look_r2::MarsGrade>,
    /// Light the frame with this baked GI volume (a `.fgiv`) instead of the live probes: the
    /// "baked" side of the live-vs-baked bench (`do27_chase --baked-gi FILE`). `None` is the
    /// live frame (DDGI and SSGI as the tier says).
    pub baked_gi: Option<std::path::PathBuf>,
    /// From this frame (loop index, warm-up included) the scene hold is on (ADR-7890): that frame
    /// renders and keeps the scene, every later one only writes the kept image back. Offscreen
    /// target only (not with a present host or a recorder). A held sample's `gpu.frame_ms` is the
    /// held GPU time and its CPU times are 0.
    pub hold_after: Option<u32>,
    /// The match-like scene (`do27_chase --match`, [`crate::match_scene`]): DO27's units,
    /// parked vehicles, buildings and props at a live match's counts, with local lights, VFX
    /// emitters and a decal grid, along the chase route. `None` is the census frame.
    pub match_scene: Option<crate::match_scene::MatchOptions>,
}

/// The opt-in renderer features `--on` turns on, each off in every shipped preset:
/// `radiance-cache` (ADR-2085), `blue-noise` (ADR-2075), `oit` (ADR-2065), `sss` (ADR-2049),
/// `shadow-occlusion` (ADR-2072) and `lens-effects` (ADR-1863, the cinematic flare, dirt and
/// chromatic aberration on a tier that allows the pass). OIT and SSS cost something only while a
/// material that uses them is on screen. Then the weather probes (spec 99, ADR-2309): `rain`
/// (the `storm` precipitation at the tier's budget), `snow` (`blizzard`), `vfx-layer` (the same
/// VFX layer under clear weather: nothing falls, so `rain` minus `vfx-layer` is the precipitation's
/// own cost) and `wet` (the surface response, [`crate::feature_cost::WET_SURFACE`]); DO27's maps
/// have none of them.
pub const ON_FEATURES: [&str; 10] = [
    "radiance-cache",
    "blue-noise",
    "oit",
    "sss",
    "shadow-occlusion",
    "lens-effects",
    "rain",
    "snow",
    "vfx-layer",
    "wet",
];

/// What `--on` changes before the first frame: quality switches, renderer switches, post.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OnSwitches {
    /// `FrameRenderer::set_radiance_cache(true)`.
    pub radiance_cache: bool,
    /// `FrameRenderer::set_shadow_occlusion(true)`.
    pub shadow_occlusion: bool,
    /// Register the lens-effects pass and set `PostSettings::lens_effects` to the cinematic look.
    pub lens_effects: bool,
    /// The precipitation layer of this weather (`rain`: storm, `snow`: blizzard, `vfx-layer`:
    /// clear, the layer alone).
    pub precipitation: Option<regia_vfx::WeatherState>,
    /// `FrameRenderer::set_surface_weather(WET_SURFACE)`.
    pub wet: bool,
}

/// Applies `on` (names from [`ON_FEATURES`]) to `quality` and returns the renderer switches.
///
/// # Errors
///
/// A name is not in [`ON_FEATURES`], or two of `rain`, `snow` and `vfx-layer` are named (one
/// weather a run).
pub fn apply_on(on: &[String], quality: &mut regia_render::FrameQuality) -> Result<OnSwitches, LookdevError> {
    let mut switches = OnSwitches::default();
    for name in on {
        match name.as_str() {
            "radiance-cache" => switches.radiance_cache = true,
            "blue-noise" => quality.blue_noise = true,
            "oit" => quality.order_independent_transparency = true,
            "sss" => quality.subsurface_scattering = true,
            "shadow-occlusion" => switches.shadow_occlusion = true,
            "lens-effects" => switches.lens_effects = true,
            "rain" | "snow" | "vfx-layer" => {
                let weather = match name.as_str() {
                    "rain" => regia_vfx::WeatherState::STORM,
                    "snow" => regia_vfx::WeatherState::BLIZZARD,
                    _ => regia_vfx::WeatherState::CLEAR,
                };
                if switches.precipitation.is_some_and(|w| w != weather) {
                    return Err(LookdevError::Config(
                        "--on rain, snow, vfx-layer: one weather a run".into(),
                    ));
                }
                switches.precipitation = Some(weather);
            }
            "wet" => switches.wet = true,
            other => {
                return Err(LookdevError::Config(format!(
                    "--on {other}: {}",
                    ON_FEATURES.join("|")
                )));
            }
        }
    }
    Ok(switches)
}

/// A meshlet cooker and the path's knobs, handed to the chase (see [`ChaseOptions::meshlets`]).
#[derive(Clone)]
pub struct MeshletSource {
    /// The cooker.
    pub builder: regia_render::MeshletBuilder,
    /// LOD threshold, cluster capacity, occlusion.
    pub settings: regia_render::VisBufferSettings,
}

impl std::fmt::Debug for MeshletSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MeshletSource")
    }
}

/// Per-instance buffer reserve of the chase: DO27's busiest view draws ~1 600 rocks and set
/// pieces; the buffers are sized for ten times that.
pub const RESERVE_INSTANCES: u32 = 16_384;
/// Scatter tiles placed per frame at most (nearest first). A DO27 rock tile is ~0.06 ms of
/// CPU (the build server, 2026-10-07), so 12 is ~0.7 ms; a cut's backlog drains in ~0.3–0.5 s.
pub const TILE_BUDGET: usize = 12;

impl Default for ChaseOptions {
    fn default() -> Self {
        Self {
            width: 2560,
            height: 1440,
            frames: 180,
            warmup: 60,
            held: false,
            start_m: 0.0,
            png: None,
            plate: None,
            sway: false,
            sync: false,
            idle_ms: 0,
            memory_squeeze: None,
            budget: regia_render::TerrainBudget::OFF,
            terrain_anisotropy: None,
            vehicle_shadow: regia_render::ShadowCast::All,
            render_scale: None,
            upscaler: regia_render::TemporalUpscaler::Taau,
            tsr: None,
            fsr3: None,
            no_static_reuse: false,
            meshlets: None,
            soak: None,
            camera_path: None,
            trace: false,
            vt: None,
            prewarm: true,
            async_sky: true,
            decals: false,
            motion: crate::motion::Motion::Chase,
            seq: None,
            off: Vec::new(),
            on: Vec::new(),
            lens: None,
            preset: None,
            reserve_instances: RESERVE_INSTANCES,
            mars_grade: None,
            baked_gi: None,
            hold_after: None,
            match_scene: None,
        }
    }
}

/// One measured frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChaseSample {
    /// Wall time of the frame loop iteration (two frames in flight), ms.
    pub wall_ms: f32,
    /// CPU phases of `render_into`, ms.
    pub encode_ms: f32,
    /// Time inside `queue.submit`, ms.
    pub submit_ms: f32,
    /// Whole `render_into`, ms.
    pub cpu_ms: f32,
    /// Scatter update + contact map + collect on the host, ms.
    pub scatter_ms: f32,
    /// Of which: tile streaming (`ScatterField::update`), ms.
    pub scatter_update_ms: f32,
    /// Of which: the contact-map rebuild and upload (`ContactMap::update`), ms.
    pub contact_ms: f32,
    /// Of which: per-instance cull and LOD (`ScatterField::collect`), ms.
    pub collect_ms: f32,
    /// GPU pass times (two frames late), ms.
    pub gpu: regia_render::GpuTimings,
    /// Stochastic SSR work read back a few frames late (zeros while the pass does not run).
    pub sssr: regia_render::SssrStats,
    /// Rocks drawn.
    pub rocks: u32,
    /// Shadow jobs per kind (cascades 0..3, atlas) this frame.
    pub shadow_jobs: [u32; 5],
    /// The frame thread's shadow CPU work by part (`FrameRenderer::shadow_cpu_timings`), ms.
    pub shadow_cpu: regia_render::ShadowCpuTimings,
    /// Terrain per cascade: 0 none, 1 whole cascade, 2 page region.
    pub shared_cascades: [u8; 4],
    /// Time blocked waiting for the oldest frame in flight, ms.
    pub wait_ms: f32,
    /// Frame-path device calls this frame (renderer thread).
    pub counters: regia_render::FramePathCounters,
    /// Buffer writes and bytes the terrain queued through `SharedUpload` this frame.
    pub shared_upload: (u32, u64),
    /// Soak events this frame ([`crate::soak::EVENT_CUT`] …).
    pub events: u8,
    /// Time this thread was runnable but not running during the frame (the scheduler gave
    /// its core to another process), ms; 0 where the OS does not report it.
    pub runq_ms: f32,
    /// The same for the renderer's encode workers (`regia-encode-*`, summed): a preempted
    /// worker shows up as a long `submit` with no run-queue wait on this thread, ms.
    pub runq_workers_ms: f32,
    /// Pressure stall (PSI `some`) of this process's cgroup during the frame: time at least
    /// one of its threads waited for a CPU, ms (Linux with PSI; 0 elsewhere).
    pub psi_cpu_ms: f32,
    /// The same for memory (reclaim, refaults), ms.
    pub psi_mem_ms: f32,
    /// Page faults of the process during the frame (minor + major).
    pub faults: u32,
    /// Of which major (the page was read back from disk or swap: the thread slept in I/O).
    pub major_faults: u32,
    /// The soak's effect spawns and their CPU step (`SoakVfx::spawn` + `SoakVfx::update`)
    /// on the render thread, ms. The rest of the frame's host work is in the other phases.
    pub spawn_ms: f32,
    /// Major page faults the render thread took inside [`Self::spawn_ms`].
    pub spawn_major_faults: u32,
    /// The core canary ([`CoreCanary`]) just before and just after the frame, the slower of
    /// the two, µs. Its median is the render thread's core running alone; a frame whose
    /// canary is far above it ran on a contended core (SMT sibling, shared cache).
    pub canary_us: f32,
    /// Scatter tiles placed this frame.
    pub tiles_placed: u32,
    /// A sky rebuild was running on its worker after this frame.
    pub sky_worker: bool,
    /// The terrain virtual texture's counters this frame (`None` when it is off).
    pub vt: Option<regia_terrain::VtStats>,
    /// Meshes drawn in the colour passes (`DrawStats::drawn`, telemetry's `drawn`).
    pub drawn: u32,
    /// Mesh instances submitted (telemetry's `counts.mesh_instances`).
    pub instances: u32,
    /// Opaque shadow casters, culled ones included (`FrameRenderer::shadow_caster_count`,
    /// telemetry's `counts.shadow_casters`).
    pub casters: u32,
    /// Local lights submitted (telemetry's `counts.local_lights`).
    pub local_lights: u32,
    /// What the shadow caches redrew this frame (`FrameRenderer::shadow_cache_report`).
    pub shadow_cache: ShadowCacheCounts,
}

/// Sun shadow pages along a cascade's edge: the renderer's page grid (`paged_shadow::SUN_PAGES_PER_AXIS`,
/// 8 since lane terrain-perf round 4), which the crate does not export.
pub const SUN_PAGES_PER_AXIS: u32 = 8;

/// One frame's shadow-cache counters, from `regia_render::ShadowCacheReport` (ADR-7986).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShadowCacheCounts {
    /// Sun pages redrawn per cascade (a whole-cascade redraw counts every page).
    pub sun_pages: [u32; 4],
    /// Cascades redrawn whole (matrix change, first frame, or every page dirty), as bits.
    pub whole_cascades: u8,
    /// Mesh casters that moved, appeared and disappeared, summed over the kept cascades.
    pub caster_events: [u32; 3],
    /// Local lights the atlas cache marked dirty.
    pub atlas_dirty_lights: u32,
    /// Atlas tiles redrawn.
    pub atlas_tiles: u32,
    /// Tiles of dirty lights kept because no moved caster reaches them.
    pub atlas_faces_kept: u32,
    /// Caster draws over the atlas jobs.
    pub atlas_caster_draws: u32,
    /// Caster draws per sun cascade (a caster over several redrawn pages counts once per page).
    pub sun_caster_draws: [u32; 4],
}

impl ShadowCacheCounts {
    /// Reads `report`; `pages_per_axis` is the sun page grid's edge.
    #[must_use]
    pub fn from_report(report: &regia_render::ShadowCacheReport, pages_per_axis: u32) -> Self {
        let mut sun_pages = [0u32; 4];
        let mut whole_cascades = 0u8;
        for (cascade, &full) in report.cascade_full_redraw.iter().enumerate().take(4) {
            if full {
                whole_cascades |= 1 << cascade;
                sun_pages[cascade] = pages_per_axis * pages_per_axis;
            }
        }
        for page in &report.dirty_pages {
            let c = page.cascade as usize;
            if c < 4 && whole_cascades & (1 << c) == 0 {
                sun_pages[c] += 1;
            }
        }
        let mut caster_events = [0u32; 3];
        for events in &report.caster_events {
            for (sum, &n) in caster_events.iter_mut().zip(events) {
                *sum += n;
            }
        }
        Self {
            sun_pages,
            whole_cascades,
            caster_events,
            atlas_dirty_lights: u32::try_from(report.dirty_atlas_tiles.len()).unwrap_or(u32::MAX),
            atlas_tiles: report.atlas_tiles_drawn,
            atlas_faces_kept: report.atlas_faces_kept,
            atlas_caster_draws: report.atlas_caster_draws,
            sun_caster_draws: report.sun_caster_draws,
        }
    }
}

/// Mean per frame over a run of what the shadow caches redrew, as printable lines: the sun page
/// hit rate per cascade (pages kept / pages), whole-cascade frames, caster events, and the atlas
/// tiles and caster draws. `casters` is the run's p50 caster count: the base drew every caster
/// into every redrawn atlas tile and named a dirty light by a tile index (ADR-7986), so its atlas
/// caster draws were `dirty lights x casters`.
#[must_use]
pub fn shadow_cache_lines(run: &ChaseRun, casters: f32) -> Vec<String> {
    let n = run.samples.len().max(1) as f32;
    let pages = (SUN_PAGES_PER_AXIS * SUN_PAGES_PER_AXIS) as f32;
    let mean = |f: &dyn Fn(&ShadowCacheCounts) -> f32| {
        run.samples.iter().map(|s| f(&s.shadow_cache)).sum::<f32>() / n
    };
    let mut lines = Vec::new();
    for c in 0..4 {
        let redrawn = mean(&|s| s.sun_pages[c] as f32);
        let whole = run
            .samples
            .iter()
            .filter(|s| s.shadow_cache.whole_cascades & (1 << c) != 0)
            .count();
        lines.push(format!(
            "  sun cascade {c}: {redrawn:.2} of {pages:.0} pages redrawn/frame, hit rate {:.1} %, whole-cascade {whole} of {} frames, {:.0} caster draws/frame",
            100.0 * (1.0 - redrawn / pages),
            run.samples.len(),
            mean(&|s| s.sun_caster_draws[c] as f32)
        ));
    }
    lines.push(format!(
        "  sun caster events/frame (kept cascades): moved {:.2}, appeared {:.2}, gone {:.2}",
        mean(&|s| s.caster_events[0] as f32),
        mean(&|s| s.caster_events[1] as f32),
        mean(&|s| s.caster_events[2] as f32)
    ));
    let dirty = mean(&|s| s.atlas_dirty_lights as f32);
    lines.push(format!(
        "  atlas: {dirty:.2} dirty lights/frame, {:.2} tiles redrawn, {:.2} faces kept, {:.0} caster draws/frame (base: {:.0} = dirty lights x casters)",
        mean(&|s| s.atlas_tiles as f32),
        mean(&|s| s.atlas_faces_kept as f32),
        mean(&|s| s.atlas_caster_draws as f32),
        dirty * casters
    ));
    lines
}

/// Nanoseconds this thread has spent waiting on a run queue (Linux `schedstat`, field 2).
#[must_use]
pub fn runqueue_wait_ns() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    text.split_whitespace().nth(1)?.parse().ok()
}

/// Major page faults so far: of the calling thread (`thread`) or the whole process
/// (`/proc/{thread-self,self}/stat` field 12; 0 where the OS does not report it).
#[must_use]
pub fn major_faults(thread: bool) -> u64 {
    let path = if thread {
        "/proc/thread-self/stat"
    } else {
        "/proc/self/stat"
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            // The fields after the parenthesised command name, which may hold spaces.
            let rest = text.rsplit_once(')')?.1;
            rest.split_whitespace().nth(9)?.parse::<u64>().ok()
        })
        .unwrap_or(0)
}

/// Thread ids of this process whose name starts with `prefix` (Linux; empty elsewhere).
#[must_use]
pub fn thread_ids(prefix: &str) -> Vec<u64> {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    dir.filter_map(Result::ok)
        .filter_map(|entry| {
            let tid = entry.file_name().to_str()?.parse::<u64>().ok()?;
            let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
            comm.starts_with(prefix).then_some(tid)
        })
        .collect()
}

/// Nanoseconds the threads `tids` of this process have spent waiting on a run queue, summed.
#[must_use]
pub fn threads_runqueue_wait_ns(tids: &[u64]) -> u64 {
    tids.iter()
        .filter_map(|tid| std::fs::read_to_string(format!("/proc/self/task/{tid}/schedstat")).ok())
        .filter_map(|text| text.split_whitespace().nth(1)?.parse::<u64>().ok())
        .sum()
}

/// Per-frame machine probes: the cgroup's CPU and memory pressure stall (PSI) and the
/// process's page faults (Linux; zeros elsewhere or without PSI).
pub struct MachineProbe {
    cpu: Option<std::path::PathBuf>,
    mem: Option<std::path::PathBuf>,
}

impl MachineProbe {
    /// Finds this process's cgroup v2 pressure files.
    #[must_use]
    pub fn new() -> Self {
        let dir = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|text| {
                let path = text.lines().find_map(|line| line.strip_prefix("0::"))?;
                Some(Path::new("/sys/fs/cgroup").join(path.trim().trim_start_matches('/')))
            });
        let file = |name: &str| {
            dir.as_ref()
                .map(|dir| dir.join(name))
                .filter(|path| std::fs::read_to_string(path).is_ok())
        };
        Self {
            cpu: file("cpu.pressure"),
            mem: file("memory.pressure"),
        }
    }

    /// `(cpu some µs, memory some µs, page faults)` so far.
    #[must_use]
    pub fn sample(&self) -> (u64, u64, u64) {
        let some_total = |path: &Option<std::path::PathBuf>| {
            path.as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|text| {
                    let line = text.lines().find(|line| line.starts_with("some"))?;
                    line.split_whitespace()
                        .find_map(|field| field.strip_prefix("total="))?
                        .parse::<u64>()
                        .ok()
                })
                .unwrap_or(0)
        };
        // `/proc/self/stat` sums the faults of every thread: minflt is field 10, majflt 12
        // (fields after the parenthesised command name, which may hold spaces).
        let faults = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|text| {
                let rest = text.rsplit_once(')')?.1;
                let fields: Vec<&str> = rest.split_whitespace().collect();
                let min = fields.get(7)?.parse::<u64>().ok()?;
                let maj = fields.get(9)?.parse::<u64>().ok()?;
                Some(min + maj)
            })
            .unwrap_or(0);
        (some_total(&self.cpu), some_total(&self.mem), faults)
    }
}

/// A fixed ~20 µs integer workload on a 32 KiB table (cache resident), timed on the render
/// thread outside the frame's wall time: the speed of the core the frame ran on.
pub struct CoreCanary {
    table: Vec<u64>,
    acc: u64,
}

impl CoreCanary {
    /// The table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            table: (0..4096u64)
                .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
                .collect(),
            acc: 1,
        }
    }

    /// One run, µs.
    pub fn run(&mut self) -> f32 {
        let started = Instant::now();
        let mask = self.table.len() - 1;
        let mut acc = self.acc;
        for _ in 0..8_000 {
            acc = acc.rotate_left(7) ^ self.table[(acc as usize) & mask];
        }
        self.acc = std::hint::black_box(acc);
        started.elapsed().as_secs_f32() * 1e6
    }
}

impl Default for CoreCanary {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for MachineProbe {
    fn default() -> Self {
        Self::new()
    }
}

/// A whole run.
#[derive(Debug, Clone)]
pub struct ChaseRun {
    /// Map name.
    pub map: &'static str,
    /// Per-frame samples.
    pub samples: Vec<ChaseSample>,
    /// Device calls over the measured frames.
    pub counters: regia_render::FramePathCounters,
    /// Submits per measured frame (from the CPU timings).
    pub submits: u32,
    /// Terrain cascade draws over the measured frames.
    pub cascade_draws: u64,
    /// Terrain page-region cascade draws (under moved mesh casters) over the measured frames.
    pub region_draws: u64,
    /// Route length used, metres.
    pub route_m: f32,
    /// Visibility-buffer counts of the last frame (all zero on the forward frame).
    pub visbuffer: regia_render::VisBufferStats,
    /// With [`ChaseOptions::trace`]: every frame-path create of the measured frames, by
    /// measured-frame index.
    pub events: Vec<(u32, regia_render::FramePathEvent)>,
    /// Temporal metrics when the run captured its frames (`ChaseOptions::seq`).
    pub motion: Option<crate::motion::MotionReport>,
    /// Wall time of [`FrameRenderer::prewarm`] (and the terrain draw's own prewarm), ms.
    pub prewarm_ms: f32,
    /// Wall time of the first submitted frame, after prewarm, ms.
    pub first_present_ms: f32,
    /// Exposure bias the map level applied (`LevelLook::exposure_bias_ev`).
    pub exposure_bias_ev: f32,
    /// `host_checklist_report_for_level` of the first measured frame.
    pub checklist: String,
    /// Differences in [`Self::checklist`].
    pub checklist_diffs: usize,
    /// Error lines from that checklist (`item: actual`).
    pub checklist_errors: Vec<String>,
    /// Difference lines (`item: actual (harness expected)`), at most 12.
    pub checklist_diff_lines: Vec<String>,
    /// Resident material-texture bytes at the end of the run.
    pub resident_bytes: u64,
    /// Budget those bytes were checked against.
    pub texture_budget_bytes: u64,
    /// Resident textures exceeded the budget.
    pub texture_over_budget: bool,
    /// Uncompressed material textures.
    pub uncompressed_textures: usize,
    /// The frame had a shared terrain draw installed.
    pub shared_draw: bool,
    /// `render.problems` lines other than texture-budget notes.
    pub frame_problems: Vec<String>,
    /// The renderer's baked GI status after the last frame (`installed` is false on the live
    /// frame; `active` says the volume lit the frame).
    pub baked_gi: regia_render::BakedGiStatus,
    /// What `--match` placed ([`ChaseOptions::match_scene`]; `None` on the census frame).
    pub match_report: Option<crate::match_scene::MatchReport>,
}

impl ChaseRun {
    /// The § 2.4 hitch report over the measured frames (wall time as the frame interval).
    #[must_use]
    pub fn hitch_report(&self) -> regia_render::HitchReport {
        let frames: Vec<regia_render::HitchFrame> = self
            .samples
            .iter()
            .map(|s| regia_render::HitchFrame {
                frame_ms: s.wall_ms,
                cpu_ms: s.cpu_ms,
                gpu_ms: s.gpu.frame_ms,
                counters: s.counters,
            })
            .collect();
        regia_render::HitchReport::new(&frames, regia_render::HitchTargets::SPEC)
    }
}

/// `CHASE_CAM=<back>,<up>` (metres) for the chase and orbit cameras; 9 m back, 3.5 m up without.
fn chase_cam_offset() -> (f32, f32) {
    std::env::var("CHASE_CAM")
        .ok()
        .and_then(|v| {
            let (b, u) = v.split_once(',')?;
            Some((b.trim().parse().ok()?, u.trim().parse().ok()?))
        })
        .unwrap_or((9.0, 3.5))
}

/// The chase camera 9 m behind and 3.5 m above the vehicle at `p` heading `dir`.
fn chase_camera(map: &regia_terrain::TerrainMap, p: [f32; 2], dir: [f32; 2]) -> ([f32; 3], [f32; 3]) {
    let ground = map.sample_bilinear(p[0], p[1]);
    let back = [p[0] - dir[0] * 9.0, p[1] - dir[1] * 9.0];
    let eye_y = (ground + 3.5).max(map.sample_bilinear(back[0], back[1]) + 1.5);
    ([back[0], eye_y, back[1]], [p[0], ground + 1.2, p[1]])
}

/// The chase route of a map: XZ points in the sim frame.
///
/// # Errors
///
/// The content's `placement.json` or `layout.json` is missing or malformed.
pub fn route(content: &Path, planet: Planet) -> Result<Vec<[f32; 2]>, LookdevError> {
    let map_dir = content.join("maps").join(crate::do27::map_name(planet));
    let read = |path: &Path| -> Result<serde_json::Value, LookdevError> {
        let text = std::fs::read_to_string(path).map_err(|source| LookdevError::Io {
            path: path.display().to_string(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|e| LookdevError::Config(format!("{}: {e}", path.display())))
    };
    let placement = read(&map_dir.join("setpieces").join("placement.json"))?;
    if let Some(road) = placement.get("road_xz").and_then(serde_json::Value::as_array) {
        let points: Vec<[f32; 2]> = road
            .iter()
            .filter_map(|p| {
                let p = p.as_array()?;
                Some([p.first()?.as_f64()? as f32, p.get(1)?.as_f64()? as f32])
            })
            .collect();
        if points.len() >= 2 {
            return Ok(points);
        }
    }
    let layout = read(&map_dir.join("layout.json"))?;
    let starts: Vec<[f32; 2]> = layout
        .get("team_starts")
        .and_then(serde_json::Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(|s| Some([s.get("x_m")?.as_f64()? as f32, s.get("z_m")?.as_f64()? as f32]))
                .collect()
        })
        .unwrap_or_default();
    if starts.len() >= 2 {
        Ok(starts)
    } else {
        Err(LookdevError::Config(format!(
            "{}: no road_xz and no two team starts",
            map_dir.display()
        )))
    }
}

/// Stops the chase's warm workers when the run ends, also on an early error return (their device
/// clone must not outlive the run), then writes the pipeline cache. The app does that save on
/// its own exit path. A two-frame cold launch ends while deferred compiles are still queued, and
/// those workers do not save once shutdown has started, so the chase has to.
struct WarmQueueGuard(regia_render::PipelineWarmQueue);

impl Drop for WarmQueueGuard {
    fn drop(&mut self) {
        self.0.shutdown();
        if let Some(Err(err)) = regia_render::pipeline_cache::save_installed() {
            tracing::warn!(target: "regia_render", "pipeline cache save at chase exit failed: {err}");
        }
    }
}

/// Draws one chase frame into a view the presenter acquired (format, width, height).
pub type PresentDraw<'a> =
    dyn FnMut(&wgpu::TextureView, wgpu::TextureFormat, u32, u32) -> Result<(), LookdevError> + 'a;

/// Presents one chase frame into a surface the caller owns (a window swapchain).
///
/// `draw` records the frame into the view the presenter acquired. `Ok(true)` keeps the
/// chase going. `Ok(false)` stops it because the window closed; that is a clean quit,
/// not an error. The presenter calls `draw` before it returns.
pub trait FramePresent {
    /// Acquire a frame, run `draw`, and present it.
    ///
    /// # Errors
    ///
    /// The surface could not be acquired or configured, or `draw` failed.
    fn present_frame(
        &mut self,
        device: &RenderDevice,
        draw: &mut PresentDraw<'_>,
    ) -> Result<bool, LookdevError>;
}

/// `CHASE_EXIT_TIMING=1`: the clean exit's renderer step (ADR-188 step 5) on the chase's
/// renderer: the GPU-idle wait, then [`FrameRenderer::drop_timed`] with the wgpu objects it
/// freed. Printed as `exit timing: …` lines.
fn exit_timing(device: &RenderDevice, renderer: FrameRenderer) {
    // CHASE_EXIT_TIMING=app (`--features overlay`): the whole clean exit with the engine's hooks.
    #[cfg(feature = "overlay")]
    if std::env::var("CHASE_EXIT_TIMING").is_ok_and(|v| v == "app") {
        exit_through_app(device, renderer);
        return;
    }
    let t = Instant::now();
    let idle = device.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(std::time::Duration::from_secs(5)),
    });
    println!(
        "exit timing: gpu idle {:.1} ms ({})",
        t.elapsed().as_secs_f64() * 1e3,
        if idle.is_ok() { "idle" } else { "timed out" }
    );
    let before = regia_render::LiveGpuObjects::of(&device.instance);
    let report = renderer.drop_timed();
    let after = regia_render::LiveGpuObjects::of(&device.instance);
    println!(
        "exit timing: renderer drop {:.1} ms; slowest: {}",
        report.total.as_secs_f64() * 1e3,
        report.slowest_text(8)
    );
    for (name, d) in &report.parts {
        println!("exit timing: part {name}: {:.2} ms", d.as_secs_f64() * 1e3);
    }
    if let (Some(b), Some(a)) = (before, after) {
        println!(
            "exit timing: freed {} buffers, {} textures, {} views, {} bind groups, {} pipelines ({} objects left)",
            b.buffers.saturating_sub(a.buffers),
            b.textures.saturating_sub(a.textures),
            b.texture_views.saturating_sub(a.texture_views),
            b.bind_groups.saturating_sub(a.bind_groups),
            b.pipelines.saturating_sub(a.pipelines),
            a.total()
        );
    }
}

/// `CHASE_EXIT_TIMING=app`: the chase's renderer becomes a headless app's `RuntimeRenderer`
/// and the app runs the clean exit (`regia_platform::run_clean_exit`, ADR-188) with the
/// engine's own exit hooks; prints `exit step <name> <ms>` per step. With
/// `CHASE_EXIT_SQUEEZE=MB[:file]` the process's cgroup is first asked to reclaim MB MiB
/// (`memory.reclaim`, as `--memory-squeeze` does), so the exit path's cold code and data page
/// out as on a loaded box.
#[cfg(feature = "overlay")]
fn exit_through_app(device: &RenderDevice, renderer: FrameRenderer) {
    use regia_app::prelude::*;
    use regia_platform::{AppLifecycle, LifecycleEvent, LifecycleTracker, run_clean_exit};

    // The exit's own log lines (renderer parts, device step parts), from here on.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stdout)
        .try_init();
    let dir = std::env::temp_dir().join(format!("regia-chase-exit-{}", std::process::id()));
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(regia_asset::AssetPlugin {
            settings: regia_asset::AssetServerSettings {
                project_root: dir.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        })
        .add_plugins(regia_render::RenderPlugin { headless: true })
        .init_resource::<AppLifecycle>()
        .add_message::<LifecycleEvent>()
        .insert_resource(device.clone());
    app.finish_plugins();
    app.update();
    app.insert_resource(regia_render::RuntimeRenderer(renderer));
    #[cfg(target_os = "linux")]
    if std::env::var("CHASE_EXIT_SQUEEZE").is_ok_and(|v| v == "self") {
        let t = Instant::now();
        let (paged, failed) = page_out_self();
        println!(
            "exit timing: paged out {paged} of this process's mappings ({failed} refused) in {:.0} ms",
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    if let Some(text) = std::env::var("CHASE_EXIT_SQUEEZE").ok().filter(|v| v != "self")
        && let Some(settings) =
            regia_core::residency::SqueezeSettings::parse(&format!("{}:1000", text.trim()))
    {
        // One reclaim round, then the exit.
        match regia_core::residency::MemorySqueeze::start(settings) {
            Some(squeeze) => {
                let t = Instant::now();
                while squeeze.rounds() == 0 && t.elapsed() < std::time::Duration::from_secs(5) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
                drop(squeeze);
                println!("exit timing: squeezed {text} before the exit");
            }
            None => println!("exit timing: no memory.reclaim here; not squeezed"),
        }
    }
    let mut tracker = LifecycleTracker::default();
    tracker.resumed();
    let report = run_clean_exit(&mut app, &tracker.exiting());
    for step in &report.steps {
        println!(
            "exit step {} {:.1} ms",
            step.step.name(),
            step.elapsed.as_secs_f64() * 1e3
        );
    }
    println!(
        "exit total {:.1} ms; gpu {:?}",
        report.total.as_secs_f64() * 1e3,
        regia_render::last_gpu_exit_report()
    );
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Asks the kernel to page out every mapping of this process (`madvise(MADV_PAGEOUT)`, Linux
/// 5.4+): clean file pages (the code) are dropped and anonymous pages swapped where there is
/// swap, so the next code that runs takes major page faults as on a loaded box. Returns the
/// mappings paged out and those refused (device memory, the vDSO).
#[cfg(all(feature = "overlay", target_os = "linux"))]
#[allow(unsafe_code)] // one madvise per mapping; see the SAFETY note
fn page_out_self() -> (usize, usize) {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return (0, 0);
    };
    let (mut paged, mut failed) = (0, 0);
    for line in maps.lines() {
        let Some((start, end)) = line
            .split_whitespace()
            .next()
            .and_then(|range| range.split_once('-'))
            .and_then(|(a, b)| {
                Some((
                    usize::from_str_radix(a, 16).ok()?,
                    usize::from_str_radix(b, 16).ok()?,
                ))
            })
        else {
            continue;
        };
        if line.contains("[vsyscall]") || line.contains("[vvar]") || line.contains("[vdso]") || end <= start {
            continue;
        }
        // SAFETY: MADV_PAGEOUT only drops or swaps the pages of an existing mapping of this
        // process; their contents are unchanged and fault back in on the next access.
        let r = unsafe { libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_PAGEOUT) };
        if r == 0 {
            paged += 1;
        } else {
            failed += 1;
        }
    }
    (paged, failed)
}

/// Runs the chase on `planet` and returns the samples.
///
/// # Errors
///
/// Content loading, the GPU upload or a frame failed.
pub fn run(
    device: &RenderDevice,
    planets: &mut R2Planets,
    content: &Path,
    planet: Planet,
    options: &ChaseOptions,
) -> Result<ChaseRun, LookdevError> {
    run_with(device, planets, content, planet, options, &mut |_, _| {})
}

/// [`run`], calling `after_frame(i, renderer)` after every frame `i` (warm-up included) has
/// been submitted: the memory report reads the renderer's allocations there.
///
/// # Errors
///
/// Content loading, the GPU upload or a frame failed.
pub fn run_with(
    device: &RenderDevice,
    planets: &mut R2Planets,
    content: &Path,
    planet: Planet,
    options: &ChaseOptions,
    after_frame: &mut dyn FnMut(u32, &FrameRenderer),
) -> Result<ChaseRun, LookdevError> {
    run_with_present(device, planets, content, planet, options, after_frame, None)
}

/// [`run_with`] that presents each frame through `present` when it is set.
///
/// The offscreen target is still created (the warm frame uses its format). When `present`
/// returns `Ok(false)` the chase stops. A sequence recorder ([`ChaseOptions::seq`]) keeps
/// the readback path and does not call `present`.
///
/// # Errors
///
/// Content loading, the GPU upload, a frame, or the presenter failed.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // one bench: scene setup, frame loop, bookkeeping
pub fn run_with_present(
    device: &RenderDevice,
    planets: &mut R2Planets,
    content: &Path,
    planet: Planet,
    options: &ChaseOptions,
    after_frame: &mut dyn FnMut(u32, &FrameRenderer),
    mut present: Option<&mut dyn FramePresent>,
) -> Result<ChaseRun, LookdevError> {
    let shot = options.plate.unwrap_or(match planet {
        Planet::Mars => R2Shot::SwitchbacksMid,
        Planet::Europa => R2Shot::BoreTwelveMid,
    });
    let path = route(content, planet)?;
    let route_m = polyline_length(&path);
    let scene = planets.get(planet, 1025)?;
    // The set-piece materials move into the frame's scene (and back at the end): cloning them
    // held every decoded image twice, which memory measurements counted as the host's.
    let prop_materials = scene
        .do27
        .as_mut()
        .map(|d| std::mem::take(&mut d.map.prop_materials))
        .unwrap_or_default();
    let Some(do27) = scene.do27.as_deref() else {
        return Err(LookdevError::Config("set REGIA_DO27_CONTENT".into()));
    };
    let level = crate::look_r2::do27_level_with(shot, do27, options.mars_grade);
    let pan_plate = match options.motion {
        crate::motion::Motion::Pan { plate, .. } => Some(plate),
        _ => None,
    };
    let plate_camera = match options.plate.or(pan_plate) {
        Some(plate) => Some(
            do27.cameras
                .get(plate.name())
                .copied()
                .ok_or_else(|| LookdevError::Config(format!("no plate camera {}", plate.name())))?,
        ),
        None => None,
    };
    let mut sun = do27.map.sun;
    let (el, az) = crate::do27::plate_sun(planet);
    sun.toward = crate::do27::sun_toward(el, az);
    let toward_sun = Vec3::from_array(sun.toward);

    // The runtime frame: Desktop High native, its post stack (TAA, auto exposure, bloom),
    // the map's look and LUT.
    let settings = options.preset.map_or_else(
        crate::look_r2::tier_settings,
        regia_render::RenderSettings::from_profile,
    );
    let mut post = settings.resolve_post();
    post.auto_exposure = settings.profile.auto_exposure;
    regia_render::apply_level_look(&mut post, &level.look);
    if let Some(lens) = options.lens.as_ref() {
        lens.apply(&mut post, &settings.profile);
    }
    if let Some(lut) = level.look.lut.as_deref() {
        post.lut = Some(
            regia_render::ColorLut::load(Path::new(lut)).map_err(|e| LookdevError::Config(e.to_string()))?,
        );
    }
    let mut quality = settings.quality();
    let tier_anisotropy = quality.terrain_budget.anisotropy;
    quality.terrain_budget = options.budget;
    quality.terrain_budget.anisotropy = options.terrain_anisotropy.unwrap_or(tier_anisotropy);
    if options.no_static_reuse {
        quality.paged_sun_shadows = false;
    }
    if let Some(scale) = options.render_scale {
        quality.render_scale_permille = (scale.clamp(0.05, 1.0) * 1000.0).round() as u16;
    }
    for pass in &options.off {
        match pass.as_str() {
            "ssgi" => quality.screen_space_gi = false,
            "ssao" => quality.ssao = regia_render::SsaoMode::Off,
            "ssr" => quality.ssr = false,
            "translucent-shadows" => {
                quality.translucent_shadow_cascades = 0;
                quality.translucent_local_shadows = false;
            }
            "contact" => quality.contact_shadows = false,
            "reflocc" => quality.reflection_occlusion = false,
            "bent" => quality.bent_normals = false,
            "gi-probes" => quality.gi_probes = false,
            "emissive" => quality.emissive_lights = 0,
            "fog" => quality.volumetric_fog = false,
            // The near sun tile (ADR-164), for its cost.
            "near-tile" => quality.near_sun_tile = 0,
            // The depth prepass (`RenderProfile::depth_prepass`): the forward pass then clears
            // depth and shades every fragment that passes the depth test at draw time, so the
            // forward time rises by the overdraw the prepass removes (gpu-cost-midhigh).
            "prepass" => {}
            "clouds" => quality.volumetric_clouds = false,
            "taa" => {
                quality.taau = false;
                post.taa = false;
            }
            "lodfade" => {
                if let Some(field) = scene.scatter.as_mut() {
                    field.set_lod_crossfade(false);
                }
            }
            other => {
                return Err(LookdevError::Config(format!(
                    "--off {other}: ssgi|ssao|ssr|translucent-shadows|contact|reflocc|bent|gi-probes|emissive|fog|near-tile|prepass|clouds|taa|lodfade"
                )));
            }
        }
    }
    let mut on = apply_on(&options.on, &mut quality)?;
    // The runtime runs the lens pass only on a tier whose profile allows it (Desktop High, Ultra).
    if on.lens_effects && !settings.profile.lens_effects {
        eprintln!("  --on lens-effects: this tier does not allow the pass; it stays off");
        on.lens_effects = false;
    }
    if on.lens_effects {
        post.lens_effects = regia_render::LensEffects::CINEMATIC;
    }

    // The runtime's renderer (`RuntimeRenderer`): sky rebuilds off the frame.
    let mut renderer = FrameRenderer::new();
    // Diagnostic: CHASE_SKINNED_INSTANCING=0 draws a skinned crowd (CHASE_SKINNED) one by one,
    // the A/B of skinned instancing (ADR-1368).
    if std::env::var("CHASE_SKINNED_INSTANCING").is_ok_and(|v| v == "0") {
        renderer.set_skinned_instancing(false);
    }
    renderer.set_temporal_upscaler(options.upscaler);
    renderer.set_tsr_settings(
        options
            .tsr
            .unwrap_or_else(|| regia_render::TsrSettings::for_scale(quality.render_scale())),
    );
    renderer.set_fsr3_settings(options.fsr3.unwrap_or_default());
    if let Some(source) = options.meshlets.as_ref() {
        renderer.set_meshlet_builder(Some(source.builder.clone()));
        renderer.set_visbuffer_settings(source.settings);
        quality.visibility_buffer = true;
    }
    renderer.set_async_sky(options.async_sky);
    if on.radiance_cache {
        renderer.set_radiance_cache(true);
    }
    if on.shadow_occlusion {
        renderer.set_shadow_occlusion(true);
    }
    if on.lens_effects && !renderer.enable_lens_effects() {
        return Err(LookdevError::Config(
            "--on lens-effects: the pass did not register".into(),
        ));
    }
    if let Some(path) = options.baked_gi.as_deref() {
        let volume = regia_asset::baked_gi::BakedGiVolume::read(path)
            .map_err(|e| LookdevError::Config(format!("--baked-gi {}: {e}", path.display())))?;
        renderer.set_baked_gi(Some(std::sync::Arc::new(volume)));
        renderer.set_baked_gi_enabled(Some(true));
    }
    renderer.set_gpu_timing(device, true);
    let mut draw = TerrainSharedDraw::new(
        device,
        scene.map.clone(),
        &scene.layers,
        scene.cdlod.clone(),
        scene.root_level,
    )?;
    if crate::look_r2::memory_tier() {
        draw.set_layer_mip_skip(regia_terrain::layer_mip_skip_for(crate::look_r2::tier_preset()));
    }
    draw.set_material_cooked(&do27.map.cooked, do27.map.params, do27.map.macro_tiles, &[])?;
    // CHASE_NO_CLIFF_RELIEF=1: the bench without cliff relief (its cost A/B).
    draw.set_relief(TerrainRelief {
        cliff_relief: if std::env::var_os("CHASE_NO_CLIFF_RELIEF").is_some() {
            0.0
        } else {
            TerrainRelief::default().cliff_relief
        },
        ..TerrainRelief::default()
    });
    // CHASE_TERRAIN_DEBUG=<n>: a terrain debug view (98 = the surface-cost probe).
    if let Some(view) = std::env::var("CHASE_TERRAIN_DEBUG")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        draw.set_debug_view(view);
    }
    let mut contact = ContactMap::new(&device.device);
    draw.set_contact_map(Some(&contact));
    // REGIA_IMPOSTORS=1: the scatter's octahedral impostor far field (ADR-116).
    let impostor_fov = {
        let aspect = options.width as f32 / options.height.max(1) as f32;
        plate_camera.map_or_else(
            || 2.0 * ((90.0f32.to_radians() * 0.5).tan() / aspect).atan(),
            |cam| cam.fov_y(aspect),
        )
    };
    let mut impostors = crate::look_r2::impostor_layer(
        device,
        &mut scene.scatter,
        &mut scene.impostor_atlas,
        options.height,
        impostor_fov,
    );
    draw.set_impostors(impostors.as_ref());
    draw.set_virtual_texture(options.vt)?;
    // CHASE_HOLES=mouths|shell: the terrain cut as the game makes it (diagnosis; the bench
    // draws the uncut terrain by default): DO27's four tunnel-mouth trenches
    // (`do27::tunnel_mouths`), or the shell's 42-ellipse chain over them (`do27::shell_holes`).
    match std::env::var("CHASE_HOLES").as_deref() {
        Ok("mouths") => draw.set_holes(Some(&do27.map.holes)),
        Ok("shell") => {
            let map_dir = content.join("maps").join(crate::do27::map_name(planet));
            draw.set_holes(crate::do27::shell_holes(&map_dir)?.as_ref());
        }
        _ => {}
    }
    let terrain_stats = draw.stats();
    let prewarm_at = Instant::now();
    // Warm workers, as the runtime has them: the forward set's rarely used variants are
    // deferred (`pipeline_warm::DeferredPipeline`); the warm below builds every one a loaded
    // material can draw, the rest build in the background after the first frame.
    let _warm_queue = options.prewarm.then(|| {
        let queue = regia_render::PipelineWarmQueue::default();
        queue.start(device);
        WarmQueueGuard(queue)
    });
    draw.prewarm(wgpu::TextureFormat::Rgba16Float);
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    if options.prewarm
        && let Some(field) = scene.scatter.as_mut()
    {
        // Level load: every rock LOD × variant resident and pinned, the per-instance buffers
        // sized, and tiles streamed a few per frame.
        field.set_tile_budget(TILE_BUDGET);
        let variants = field.prewarm_meshes();
        renderer
            .prewarm(device, &variants, options.reserve_instances)
            .map_err(|e| LookdevError::Config(e.to_string()))?;
    }
    let mut outdoor = OutdoorParams {
        fog: shot_fog(shot),
        ..OutdoorParams::default()
    };
    outdoor.apply_level(&level.atmosphere, false);
    renderer.set_outdoor(outdoor);
    if on.wet {
        renderer.set_surface_weather(crate::feature_cost::WET_SURFACE);
    }

    // Set pieces, then the vehicle built at the origin facing +X (moved by its model matrix).
    let mut meshes = do27.map.props.clone();
    let vehicle_first = meshes.len();
    // A 4.2 × 1.6 × 2.0 m hull (long axis +X), resting on the ground.
    let (hull_v, hull_i) = regia_render::box_mesh([2.1, 0.8, 1.0]);
    meshes.push(regia_render::LabMesh {
        vertices: regia_render::translate_mesh(&hull_v, [0.0, 0.8, 0.0]),
        indices: hull_i,
        material: 0,
        ..regia_render::LabMesh::unskinned()
    });
    // Diagnostic: CHASE_VEHICLE=x11 drives DO27's X-11 (hull, glass canopy, cockpit) instead of
    // the box, so the near sun tile and the glass passes are timed on a real hull (ADR-164/165).
    let mut hero_materials = Vec::new();
    if std::env::var("CHASE_VEHICLE").is_ok_and(|v| v == "x11") {
        let asset = crate::hero::catalog(content)
            .into_iter()
            .find(|a| a.name == "x11")
            .ok_or_else(|| LookdevError::Config("CHASE_VEHICLE=x11: no vehicles/x11 in the drop".into()))?;
        let mut model = crate::hero::load(&asset, 1024)?;
        // Nose along +X like the box, centred, resting on the ground.
        crate::hero::place(
            &mut model,
            std::f32::consts::FRAC_PI_2,
            regia_core::glam::Vec3::ZERO,
        );
        let c = (model.min + model.max) * 0.5;
        let lift = -model.min.y;
        crate::hero::place(&mut model, 0.0, regia_core::glam::Vec3::new(-c.x, lift, -c.z));
        meshes.truncate(vehicle_first);
        meshes.extend(model.meshes);
        hero_materials = model.materials;
    }
    if plate_camera.is_some() {
        meshes.truncate(vehicle_first);
    }
    let vehicle_count = meshes.len() - vehicle_first;
    let mut mats = materials();
    let prop_base = mats.len();
    mats.extend(prop_materials);
    // The X-11's materials go after the set pieces' (whose indices assume `prop_base`) and are
    // dropped again before those return to the map.
    let prop_end = mats.len();
    if !hero_materials.is_empty() {
        for mesh in meshes.iter_mut().skip(vehicle_first).take(vehicle_count) {
            mesh.material += prop_end;
        }
        mats.extend(hero_materials);
    }
    // Diagnostic: CHASE_UNITS=N adds N copies of the vehicle that drive circles around it, so the
    // paged shadows redraw pages under many moving casters every frame (a skirmish's load).
    let units: usize = std::env::var("CHASE_UNITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let unit_first = vehicle_first + vehicle_count;
    // Diagnostic: CHASE_CHURN=1 (with CHASE_UNITS) draws the units as a host that builds a new
    // `Arc<CpuMesh>` per unit mesh every frame (same content) and keys it like the runtime path
    // (`regia_render::mesh_key`), releasing the previous frame's (roadmap 7a's churn case).
    let churn = units > 0 && std::env::var("CHASE_CHURN").is_ok_and(|v| v == "1");
    // Diagnostic: CHASE_SKINNED=1 (with CHASE_UNITS) skins the units with two joints (the upper
    // half of each part on joint 1, swaying every frame) and gives every copy of a part one mesh
    // key, as a runtime host keys one asset: a skinned crowd (ADR-1368).
    let skinned_units = units > 0 && std::env::var("CHASE_SKINNED").is_ok_and(|v| v == "1");
    if units > 0 && vehicle_count > 0 {
        let mut proto: Vec<_> = meshes[vehicle_first..unit_first].to_vec();
        if skinned_units {
            proto.iter_mut().for_each(skin_two_joints);
        }
        for _ in 0..units {
            meshes.extend(proto.iter().cloned());
        }
    }
    // Diagnostic: CHASE_CASTERS=N adds N static props beside the route, so the sun-shadow
    // encode sees a game's caster mix (DO27's Switchbacks: ~770 casters, most of them set
    // pieces and unit parts with their own mesh). Every lab mesh slot is its own mesh key, so
    // each prop is its own shadow batch, the worst case of that mix.
    if let Some(count) = std::env::var("CHASE_CASTERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        let distinct = 17usize;
        // Around the drive: 80 m behind the start to 200 m ahead (a chase covers ~105 m).
        let (from, to) = (options.start_m - 80.0, options.start_m + 200.0);
        let variants: Vec<(Vec<regia_render::LabVertex>, Vec<u32>)> = (0..distinct)
            .map(|v| {
                // Distinct vertex data per variant: a distinct mesh key in the frame.
                let s = 0.4 + (v % 17) as f32 * 0.15;
                regia_render::box_mesh([s, 0.3 + (v % 7) as f32 * 0.2 + v as f32 * 1.0e-3, s * 0.8])
            })
            .collect();
        for k in 0..count {
            let s = from + (k as f32 + 0.5) / count as f32 * (to - from);
            let (p, dir) = crate::soak::along(&path, s);
            let side = if k % 2 == 0 { 1.0 } else { -1.0 };
            let off = side * (4.0 + (k * 7 % 23) as f32 * 2.5);
            let (x, z) = (p[0] - dir[1] * off, p[1] + dir[0] * off);
            let y = scene.map.sample_bilinear(x, z);
            let (vertices, indices) = variants[k % distinct].clone();
            meshes.push(regia_render::LabMesh {
                vertices,
                indices,
                material: 0,
                model: (Mat4::from_translation(Vec3::new(x, y, z)) * Mat4::from_rotation_y(k as f32 * 0.7))
                    .to_cols_array_2d(),
                ..regia_render::LabMesh::unskinned()
            });
        }
    }
    // `--match`: the match's assets after everything else, one prototype per part at the origin
    // (uploaded once, as a game keeps one mesh per asset), and their layout along the route.
    let match_first = meshes.len();
    let match_frame = match options.match_scene.as_ref() {
        Some(m) => {
            let loaded_at = Instant::now();
            let loaded = crate::match_scene::load(content, m)?;
            let material_base = mats.len();
            mats.extend(loaded.materials.iter().map(|mat| {
                if m.flat {
                    crate::match_scene::flat_material(mat)
                } else {
                    mat.clone()
                }
            }));
            let mut parts = Vec::with_capacity(loaded.meshes.len());
            for proto_meshes in &loaded.meshes {
                let first = meshes.len() - match_first;
                for mesh in proto_meshes {
                    let mut mesh = mesh.clone();
                    mesh.material += material_base;
                    meshes.push(mesh);
                }
                parts.push(first..meshes.len() - match_first);
            }
            let layout =
                crate::match_scene::place(&m.plan, &loaded.protos, &path, options.start_m, &|x, z| {
                    scene.map.sample_bilinear(x, z)
                });
            let report = crate::match_scene::MatchReport::new(
                &m.plan,
                &layout,
                &loaded.protos,
                loaded_at.elapsed().as_secs_f32() * 1000.0,
            );
            println!("{}", report.line());
            Some(MatchFrame {
                first: match_first,
                end: meshes.len(),
                parts,
                layout,
                report,
            })
        }
        None => None,
    };
    let aspect = options.width as f32 / options.height.max(1) as f32;
    let fov_y = plate_camera.map_or_else(
        || 2.0 * ((90.0f32.to_radians() * 0.5).tan() / aspect).atan(),
        |cam| cam.fov_y(aspect),
    );
    let mut lab = LabScene {
        width: options.width,
        height: options.height,
        eye: [0.0; 3],
        target: [0.0, 0.0, 1.0],
        fov_y,
        near: 0.1,
        far: 12_000.0,
        depth_prepass: settings.profile.depth_prepass && !options.off.iter().any(|p| p == "prepass"),
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: regia_render::Environment::default().ambient_lux,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new((-toward_sun).to_array(), sun.color, sun.lux),
        local_lights: match_frame
            .as_ref()
            .map(|m| m.layout.lights.clone())
            .unwrap_or_default(),
        materials: mats,
        meshes,
    };

    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("chase target"),
        size: wgpu::Extent3d {
            width: options.width,
            height: options.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    // The frame scene is built once, as the runtime's extraction keeps its keys: the lab
    // conversion hashes every texture's bytes into the material key, which costs ~100 ms a
    // frame on the set pieces' textures and is not part of a game frame.
    let base = lab.as_frame_scene_with_quality(quality);
    if match_frame.is_some() && base.meshes.len() != lab.meshes.len() {
        return Err(LookdevError::Config(format!(
            "--match: {} of {} meshes reached the frame (a material index is out of range)",
            base.meshes.len(),
            lab.meshes.len()
        )));
    }
    // Comprehensive warm (`FrameRenderer::warm_scene`): `renderer.prewarm` above built the
    // base, material-graph, glass, double-sided and terrain pipelines; the post stack, shadow
    // cascades, sky, volumetric fog and the screen-space passes (SSAO/SSGI/SSR) are built by a
    // frame. Rendering this frame once offscreen moves that whole cold compile into the prewarm
    // span, so the first presented game frame only allocates its targets. (Synchronous here:
    // the bench's model of the runtime's `GameFrameWarm` on a `PipelineWarmQueue` worker.)
    // CHASE_WARM_WORKER=busy|idle: the same warm on a `PipelineWarmQueue` worker
    // (`warm_renderer`, as the runtime's `GameFrameWarm` does), the main thread meanwhile
    // clearing and submitting a window-sized target every 16 ms (`busy`, the window's splash /
    // loading screen) or sleeping (`idle`). Prints both phases' times.
    let warm_worker = std::env::var("CHASE_WARM_WORKER").ok();
    if options.prewarm
        && let Some(mode) = warm_worker.as_deref()
    {
        crate::spans::reset_warm_spans();
        let warm_at = Instant::now();
        renderer = warm_on_worker(device, renderer, lab.clone(), quality, format, mode == "busy")?;
        println!(
            "warm on worker ({mode} main thread): {:.1} ms",
            warm_at.elapsed().as_secs_f32() * 1000.0
        );
        crate::spans::print_warm_spans("worker");
    } else if options.prewarm {
        crate::spans::reset_warm_spans();
        let warm_at = Instant::now();
        renderer
            .warm_scene(device, &base, format, false)
            .map_err(|e| LookdevError::Config(e.to_string()))?;
        println!("warm inline: {:.1} ms", warm_at.elapsed().as_secs_f32() * 1000.0);
        crate::spans::print_warm_spans("inline");
    }
    let prewarm_ms = prewarm_at.elapsed().as_secs_f32() * 1000.0;
    // The first game frame is next: unused variants build in the background from here.
    regia_render::pipeline_warm::release_deferred();
    // The soak: its script, and a VFX layer that spawns effects and decals on cue.
    let script = options.soak.map(|soak| {
        let cams: Vec<([f32; 3], [f32; 3], f32)> = R2Shot::PARITY
            .iter()
            .filter(|shot| shot.planet() == planet)
            .filter_map(|shot| do27.cameras.get(shot.name()))
            .map(|cam| (cam.eye, cam.target, cam.fov_h_deg))
            .collect();
        let cams = match options.camera_path {
            Some(path) => path
                .waypoints(6, scene.map.world_width() * 0.42)
                .into_iter()
                .filter_map(|cut| match cut {
                    crate::soak::SoakCut::Fixed(eye, target, fov) => Some((eye, target, fov)),
                    crate::soak::SoakCut::Teleport(_) => None,
                })
                .collect(),
            None => cams,
        };
        crate::soak::SoakScript::new(
            &path,
            scene.map.world_width(),
            &cams,
            crate::do27::plate_sun(planet),
            soak,
        )
    });
    let measured = script
        .as_ref()
        .map_or(options.frames, crate::soak::SoakScript::frames);
    let mut vfx = if script.is_some() || options.decals || match_frame.is_some() {
        Some(crate::soak::SoakVfx::new(device)?)
    } else {
        None
    };
    let precipitation = match on.precipitation.as_ref() {
        Some(weather) => Some(crate::feature_cost::OnPrecipitation::new(
            device, weather, &settings,
        )?),
        None => None,
    };
    let layers: Vec<_> = vfx
        .iter()
        .map(crate::soak::SoakVfx::layer)
        .chain(
            precipitation
                .iter()
                .map(crate::feature_cost::OnPrecipitation::layer),
        )
        .collect();
    if !layers.is_empty() {
        renderer.set_translucent_layers(layers);
    }
    if let Some(precip) = precipitation.as_ref() {
        let max_edge = device.device.limits().max_texture_dimension_2d;
        renderer.set_precipitation_occlusion_budget(precip.occlusion_budget(), max_edge);
    }
    let fov_h = |deg: f32| 2.0 * ((deg.to_radians() * 0.5).tan() / aspect).atan();
    regia_render::trace_frame_path(options.trace);
    let mut traced: Vec<(u32, regia_render::FramePathEvent)> = Vec::new();
    let mut in_flight: VecDeque<wgpu::SubmissionIndex> = VecDeque::new();
    // The paused-scene hold (ADR-7890, `hold_after`).
    let mut scene_hold = regia_render::SceneHold::default();
    let mut retained = regia_render::RetainedScene::default();
    let mut held_frame: Option<regia_render::SceneHoldFrame> = None;
    let mut samples = Vec::with_capacity(options.frames as usize);
    let exposure_bias_ev = level.look.exposure_bias_ev;
    let mut first_present_ms = 0.0f32;
    let mut checklist = String::new();
    let mut checklist_diffs = 0usize;
    let mut checklist_errors = Vec::new();
    let mut checklist_diff_lines = Vec::new();
    let mut counters_start = regia_render::frame_path_counters();
    let mut draws_start = renderer.shared_shadow_draws();
    let mut regions_start = renderer.shared_shadow_region_draws();
    let mut submits = 0u32;
    // The camera of measured step `step`.
    let heights = &scene.map;
    let motion_pose = |step: u32| -> Pose {
        let parked = matches!(options.motion, crate::motion::Motion::Orbit { .. });
        let s = options.start_m + if parked { 0.0 } else { step as f32 * SPEED * DT };
        let (p, dir) = along(&path, s);
        let ground = heights.sample_bilinear(p[0], p[1]);
        let (eye, target) = match (plate_camera, options.motion) {
            (Some(cam), crate::motion::Motion::Pan { deg_per_frame, .. }) => {
                let yaw = Mat4::from_rotation_y((step as f32 * deg_per_frame).to_radians());
                let look = Vec3::from_array(cam.target) - Vec3::from_array(cam.eye);
                (
                    cam.eye,
                    (Vec3::from_array(cam.eye) + yaw.transform_vector3(look)).to_array(),
                )
            }
            (Some(cam), _) => (cam.eye, cam.target),
            (
                None,
                crate::motion::Motion::Flyover {
                    height_m,
                    m_per_frame,
                },
            ) => {
                // Heading toward the route 150 m on (a winding road's local direction would
                // swing the view), pitched 15° down: the horizon and the far field in view.
                let s = options.start_m + step as f32 * m_per_frame;
                let (q, d) = along(&path, s);
                let (ahead, _) = along(&path, s + 150.0);
                let h = Vec3::new(ahead[0] - q[0], 0.0, ahead[1] - q[1])
                    .try_normalize()
                    .unwrap_or(Vec3::new(d[0], 0.0, d[1]));
                let eye = Vec3::new(q[0], heights.sample_bilinear(q[0], q[1]) + height_m, q[1]);
                let look = h - Vec3::Y * 15f32.to_radians().tan();
                (eye.to_array(), (eye + look * 100.0).to_array())
            }
            (None, crate::motion::Motion::Dolly { m_per_frame }) => {
                let (p0, d0) = along(&path, options.start_m);
                let g0 = heights.sample_bilinear(p0[0], p0[1]);
                let back = [p0[0] - d0[0] * 9.0, p0[1] - d0[1] * 9.0];
                let y = (g0 + 3.5).max(heights.sample_bilinear(back[0], back[1]) + 1.5);
                let t = step as f32 * m_per_frame;
                let eye = [back[0] + d0[0] * t, y, back[1] + d0[1] * t];
                // The start pose's pitch: the target stays 9 m ahead and as far below.
                let drop = y - (g0 + 1.2);
                (
                    [eye[0], eye[1], eye[2]],
                    [eye[0] + d0[0] * 9.0, y - drop, eye[2] + d0[1] * 9.0],
                )
            }
            (None, motion) => {
                let orbit = match motion {
                    crate::motion::Motion::Turn { deg_per_frame }
                    | crate::motion::Motion::Orbit { deg_per_frame } => {
                        (step as f32 * deg_per_frame).to_radians()
                    }
                    _ => 0.0,
                };
                let (sin, cos) = orbit.sin_cos();
                // `dir` rotated by the orbit angle: the camera sits 9 m behind along it.
                // Diagnostic: CHASE_CAM=<back>,<up> moves it (e.g. 4.5,2 puts the hull within
                // the near sun tile's 4 m, ADR-164's cost run).
                let (back_m, up_m) = chase_cam_offset();
                let back_dir = [dir[0] * cos - dir[1] * sin, dir[0] * sin + dir[1] * cos];
                let back = [p[0] - back_dir[0] * back_m, p[1] - back_dir[1] * back_m];
                let eye_y = (ground + up_m).max(heights.sample_bilinear(back[0], back[1]) + 1.5);
                ([back[0], eye_y, back[1]], [p[0], ground + 1.2, p[1]])
            }
        };
        Pose {
            eye,
            target,
            at: p,
            dir,
            ground,
        }
    };
    let mut recorder = match options.seq.clone() {
        Some(seq) => Some(crate::motion::Recorder::new(seq, options.width, options.height)?),
        None => None,
    };
    let mut last_frame: Option<crate::motion::Frame> = None;
    // Scatter LOD use over the measured frames: instance-frames per LOD and cross-fading.
    let mut lod_frames = [0u64; regia_terrain::MAX_SCATTER_LODS];
    let mut lod_fading = 0u64;
    let mut workers: Vec<u64> = Vec::new();
    let machine = MachineProbe::new();
    let mut canary = CoreCanary::new();
    let mut squeeze: Option<regia_core::residency::MemorySqueeze> = None;
    for i in 0..options.warmup + measured {
        let step = if options.held || i < options.warmup {
            0
        } else {
            i - options.warmup
        };
        let frame_counters = regia_render::frame_path_counters();
        if i == 1 {
            // The encode workers start with the first frame.
            workers = thread_ids("regia-encode");
        }
        let workers_start = threads_runqueue_wait_ns(&workers);
        let machine_start = machine.sample();
        let canary_before = canary.run();
        let runq_start = runqueue_wait_ns();
        let pose = script.as_ref().map(|script| script.pose(step));
        if i == options.warmup {
            if let Some(settings) = options.memory_squeeze {
                squeeze = regia_core::residency::MemorySqueeze::start(settings);
                if squeeze.is_none() {
                    return Err(LookdevError::Config(
                        "memory squeeze: this process's cgroup has no writable memory.reclaim".into(),
                    ));
                }
            }
            counters_start = regia_render::frame_path_counters();
            draws_start = renderer.shared_shadow_draws();
            regions_start = renderer.shared_shadow_region_draws();
        }
        let mut fov_y = lab.fov_y;
        let mut fixed_camera = plate_camera.is_some();
        let (mut eye, mut target, p, dir, ground) = match pose {
            // The soak script drives the vehicle, the camera and its cuts.
            Some(pose) => {
                let (p, dir) = (pose.vehicle, pose.heading);
                let ground = scene.height_at(p[0], p[1]);
                let (eye, target) = match (plate_camera, pose.camera) {
                    (Some(cam), _) => (cam.eye, cam.target),
                    (None, Some(crate::soak::SoakCamera::Fixed(eye, target, fov))) => {
                        fov_y = fov_h(fov);
                        fixed_camera = true;
                        (eye, target)
                    }
                    (None, Some(crate::soak::SoakCamera::Chase(q, d))) => chase_camera(&scene.map, q, d),
                    (None, None) => chase_camera(&scene.map, p, dir),
                };
                (eye, target, p, dir, ground)
            }
            // The road chase or one of the motion-look moves (`ChaseOptions::motion`).
            None => {
                let Pose {
                    eye,
                    target,
                    at,
                    dir,
                    ground,
                } = motion_pose(step);
                (eye, target, at, dir, ground)
            }
        };
        if let (None, Ok(mode)) = (plate_camera, std::env::var("CHASE_ONFOOT")) {
            // Diagnosis: DO27's on-foot camera, 1.7 m over the ground. `front`: 9 m ahead of the
            // vehicle and 2 m aside, looking back at it; otherwise 3 m behind it, along the route.
            if mode == "front" {
                let at = [
                    p[0] + dir[0] * 9.0 - dir[1] * 2.0,
                    p[1] + dir[1] * 9.0 + dir[0] * 2.0,
                ];
                eye = [at[0], scene.height_at(at[0], at[1]) + 1.7, at[1]];
                target = [p[0], scene.height_at(p[0], p[1]) + 0.8, p[1]];
            } else {
                let back = [p[0] - dir[0] * 3.0, p[1] - dir[1] * 3.0];
                eye = [back[0], scene.height_at(back[0], back[1]) + 1.7, back[1]];
                let ahead = [p[0] + dir[0] * 10.0, p[1] + dir[1] * 10.0];
                target = [ahead[0], scene.height_at(ahead[0], ahead[1]) + 1.2, ahead[1]];
            }
        }
        if options.sway && plate_camera.is_some() {
            // `--sway`: move the plate eye 2 cm per frame so the screen cache never hits and
            // every screen pass runs (per-pass cost on the plates).
            let a = i as f32 * 0.7;
            eye[0] += 0.02 * a.sin();
            eye[2] += 0.02 * a.cos();
        }
        let events = pose.map_or(0, |pose| pose.events);
        let major_start = major_faults(false);
        let spawn_major_start = major_faults(true);
        let started = Instant::now();
        let spawn_span =
            tracing::info_span!("soak_spawn", spawn = events & crate::soak::EVENT_SPAWN != 0).entered();
        if let Some(vfx) = vfx.as_mut() {
            if options.decals && i == 0 {
                // 16 × 16 decals, 48 m across, centred 30 m ahead of the eye: the near
                // ground and its rocks, where the receiver stamp decides the decals' pixels.
                let ahead =
                    Vec3::new(target[0] - eye[0], 0.0, target[2] - eye[2]).normalize_or(Vec3::X) * 30.0;
                vfx.spawn_decal_grid(&scene.map, [eye[0] + ahead.x, eye[2] + ahead.z], 16, 3.0);
            }
            if let Some(m) = match_frame.as_ref() {
                let plan = options.match_scene.as_ref().map(|o| o.plan).unwrap_or_default();
                if i == 0 && plan.decal_grid > 0 {
                    vfx.spawn_decal_grid(&scene.map, m.layout.decal_centre, plan.decal_grid, 5.0);
                }
                // Each emitter spawns a showcase effect and a scorch decal on its own beat.
                for (k, at) in m.layout.emitters.iter().enumerate() {
                    if (i + k as u32 * 23).is_multiple_of(plan.emit_every.max(1)) {
                        vfx.spawn(&scene.map, *at, [0.0, 0.0], i + k as u32);
                    }
                }
            }
            if events & crate::soak::EVENT_SPAWN != 0 {
                vfx.spawn(&scene.map, p, dir, step);
            }
            vfx.update(eye);
        }
        if let Some(precip) = precipitation.as_ref() {
            precip.step(DT);
        }
        drop(spawn_span);
        let spawn_ms = started.elapsed().as_secs_f32() * 1000.0;
        let spawn_major_faults =
            u32::try_from(major_faults(true).saturating_sub(spawn_major_start)).unwrap_or(u32::MAX);
        let scatter_started = Instant::now();
        let scatter_view = ScatterView::look_at(eye, target, fov_y, aspect, lab.near, lab.far)
            .with_sun(toward_sun.to_array());
        let (mut update_ms, mut contact_ms, mut collect_ms) = (0.0, 0.0, 0.0);
        let mut tiles_placed = 0;
        if let Some(field) = scene.scatter.as_mut() {
            if i == 0 && impostors.is_some() {
                // Level load: the impostor far field is placed before the first frame.
                let t = Instant::now();
                let settled = field.settle_streaming(eye);
                eprintln!(
                    "impostors: far field settled in {:.2} s ({} tiles, {} instances resident)",
                    t.elapsed().as_secs_f32(),
                    settled.placed_tiles,
                    field.len()
                );
            }
            let t = Instant::now();
            tiles_placed = field.update(eye).placed_tiles;
            update_ms = t.elapsed().as_secs_f32() * 1000.0;
            let t = Instant::now();
            contact.update(&device.queue, field, eye);
            contact_ms = t.elapsed().as_secs_f32() * 1000.0;
            if let Some(layer) = impostors.as_mut() {
                layer.update(&device.queue, field);
            }
        }
        let mut rocks = Vec::new();
        let unit_end = (unit_first + units * vehicle_count).min(base.meshes.len());
        let churn_meshes: Vec<std::sync::Arc<regia_render::CpuMesh>> = if churn {
            renderer.release_meshes(&regia_render::take_released_mesh_keys());
            base.meshes[unit_first.min(unit_end)..unit_end]
                .iter()
                .map(|mesh| {
                    std::sync::Arc::new(regia_render::CpuMesh {
                        vertices: mesh.vertices.to_vec(),
                        indices: mesh.indices.to_vec(),
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        // This frame's sway of each skinned unit (CHASE_SKINNED), and the mesh key every copy of
        // a part shares.
        let unit_palettes: Vec<[regia_render::JointMatrix; 2]> = if skinned_units {
            (0..units)
                .map(|u| {
                    let sway = 0.05 * (i as f32 * 0.15 + u as f32).sin();
                    [
                        regia_render::JointMatrix::identity(),
                        regia_render::JointMatrix::translation(sway, 0.0, 0.0),
                    ]
                })
                .collect()
        } else {
            Vec::new()
        };
        let part_keys: Vec<u64> = base.meshes[unit_first.min(unit_end)..unit_end]
            .iter()
            .take(vehicle_count)
            .map(|mesh| mesh.mesh_key)
            .collect();
        let mut frame = base.clone();
        for (mesh, arc) in frame.meshes[unit_first.min(unit_end)..unit_end]
            .iter_mut()
            .zip(&churn_meshes)
        {
            mesh.mesh_key = regia_render::mesh_key(arc);
            mesh.vertices = &arc.vertices;
            mesh.indices = &arc.indices;
        }
        frame.eye = eye;
        frame.target = target;
        frame.fov_y = fov_y;
        if let Some(pose) = pose {
            let toward = Vec3::from_array(pose.sun_toward);
            frame.directional = DirectionalLightUniform::new((-toward).to_array(), sun.color, sun.lux);
        }
        if let Some(field) = scene.scatter.as_ref() {
            let t = Instant::now();
            let stats = field.collect(&scatter_view, &mut rocks);
            collect_ms = t.elapsed().as_secs_f32() * 1000.0;
            if i >= options.warmup {
                for (total, n) in lod_frames.iter_mut().zip(stats.per_lod) {
                    *total += u64::from(n);
                }
                lod_fading += u64::from(stats.lod_fading);
            }
        }
        if fixed_camera {
            // As `look_r2`: the plate cameras never stand inside a rock.
            rocks.retain(|m| keeps_clear(m, eye));
        }
        let scatter_ms = scatter_started.elapsed().as_secs_f32() * 1000.0;
        let rock_count = rocks.len() as u32;
        frame.meshes.append(&mut rocks);
        let yaw = (-dir[1]).atan2(dir[0]);
        let model = (Mat4::from_translation(Vec3::new(p[0], ground, p[1])) * Mat4::from_rotation_y(yaw))
            .to_cols_array_2d();
        for mesh in frame.meshes.iter_mut().skip(vehicle_first).take(vehicle_count) {
            mesh.model = model;
            mesh.shadow = options.vehicle_shadow;
        }
        for u in 0..units {
            let a = u as f32 * 2.399 + i as f32 * 0.01 * (1.0 + (u % 3) as f32);
            let r = 12.0 + (u % 10) as f32 * 5.0;
            let (x, z) = (p[0] + r * a.cos(), p[1] + r * a.sin());
            let unit_model = (Mat4::from_translation(Vec3::new(x, ground, z)) * Mat4::from_rotation_y(-a))
                .to_cols_array_2d();
            let first = unit_first + u * vehicle_count;
            for (part, mesh) in frame
                .meshes
                .iter_mut()
                .skip(first)
                .take(vehicle_count)
                .enumerate()
            {
                mesh.model = unit_model;
                mesh.shadow = options.vehicle_shadow;
                if let (Some(palette), Some(&key)) = (unit_palettes.get(u), part_keys.get(part)) {
                    mesh.joints = &palette[..];
                    mesh.mesh_key = key;
                }
            }
        }
        if let Some(m) = match_frame.as_ref() {
            m.instance(&mut frame, i, eye);
        }
        if i == 2 && std::env::var_os("CHASE_PROBES").is_some() {
            // DO27's reflection probes (diagnosis): one probe captured once at the first view,
            // half a metre over the eye, as the shell captures its sites at map load.
            let centre = [eye[0], eye[1] + 0.5, eye[2]];
            let probe = renderer
                .capture_reflection_probe(device, &frame, 1, centre, [40.0, 15.0, 40.0])
                .map_err(|e| LookdevError::Config(e.to_string()))?;
            if let Some(dir) = std::env::var_os("CHASE_PROBES_DUMP") {
                let path = Path::new(&dir).join("probe_equirect.png");
                if let Err(err) =
                    crate::default_look::write_png(&path, probe.width, probe.height, &probe.equirect)
                {
                    eprintln!("probe dump: {err}");
                }
            }
            renderer.set_reflection_probes(vec![probe]);
        }
        let present_at = (i == 0).then(Instant::now);
        if recorder.is_some() {
            let image = renderer
                .render_offscreen(device, &frame)
                .map_err(|e| LookdevError::Config(e.to_string()))?;
            last_frame = Some(crate::motion::Frame {
                width: image.width,
                height: image.height,
                pixels: image.pixels,
            });
        } else if let Some(host) = present.as_mut() {
            let keep = host.present_frame(device, &mut |target, target_format, target_w, target_h| {
                renderer
                    .render_into(
                        device,
                        &frame,
                        FrameTarget::full(target, target_format, target_w, target_h),
                    )
                    .map_err(|e| LookdevError::Config(e.to_string()))
            })?;
            if !keep {
                break;
            }
        } else {
            scene_hold.set(options.hold_after.is_some_and(|n| i >= n));
            let step = retained.plan(
                &scene_hold,
                Some(regia_render::scene_hold::HoldKey {
                    width: options.width,
                    height: options.height,
                    format,
                }),
            );
            if step.renders_scene() {
                renderer
                    .render_into(
                        device,
                        &frame,
                        FrameTarget::full(&view, format, options.width, options.height),
                    )
                    .map_err(|e| LookdevError::Config(e.to_string()))?;
                retained.note_scene_views(1);
            }
            held_frame = None;
            if step != regia_render::scene_hold::HoldStep::Full {
                let mut encoder = device
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("chase scene hold"),
                    });
                retained
                    .record(device, &mut encoder, &view, Some(&texture), format)
                    .map_err(|e| LookdevError::Config(e.to_string()))?;
                retained.finish_ui(&mut encoder);
                let submitted = device.queue.submit([encoder.finish()]);
                retained.after_submit(device);
                if step == regia_render::scene_hold::HoldStep::Held {
                    in_flight.push_back(submitted);
                    held_frame = Some(retained.last_frame());
                }
            }
        }
        if let Some(at) = present_at {
            first_present_ms = at.elapsed().as_secs_f32() * 1000.0;
        }
        if i >= options.warmup
            && let (Some(rec), Some(image)) = (recorder.as_mut(), last_frame.take())
        {
            rec.push(
                i - options.warmup,
                image,
                crate::motion::Camera {
                    eye,
                    target,
                    fov_y: lab.fov_y,
                },
            )?;
        }
        if let Some(submission) = renderer.take_submission() {
            in_flight.push_back(submission);
        }
        after_frame(i, &renderer);
        if i == options.warmup {
            if units > 0 {
                // The units' draw counters (skinned crowds: ADR-1368).
                let d = renderer.draw_stats();
                eprintln!(
                    "units: {units} x {vehicle_count} parts, skinned {skinned_units}, skinned instancing {}: \
                     colour instanced {} per_object {}; skinned draws {} instanced {} batches {} possible {}",
                    renderer.skinned_instancing(),
                    d.instanced,
                    d.per_object,
                    d.skinned_draws,
                    d.skinned_instanced,
                    d.skinned_batches,
                    d.skinned_batches_possible
                );
            }
            let report = renderer.host_checklist_report_for_level(&frame, &level);
            if std::env::var_os("LOOK_R2_CHECKLIST").is_some() {
                println!("{report}");
            }
            checklist_diffs = report.differences().len();
            checklist_errors = report
                .errors()
                .iter()
                .map(|item| format!("{}: {}", item.item, item.actual))
                .collect();
            checklist_diff_lines = report
                .differences()
                .iter()
                .take(12)
                .map(|item| format!("{}: {} (harness {})", item.item, item.actual, item.expected))
                .collect();
            checklist = report.to_string();
        }
        let wait_started = Instant::now();
        if in_flight.len() >= if options.sync { 1 } else { 3 }
            && let Some(oldest) = in_flight.pop_front()
        {
            device
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(oldest),
                    timeout: None,
                })
                .map_err(|e| LookdevError::Config(e.to_string()))?;
        }
        let wait_ms = wait_started.elapsed().as_secs_f32() * 1000.0;
        let runq_ms = match (runq_start, runqueue_wait_ns()) {
            (Some(a), Some(b)) => b.saturating_sub(a) as f32 * 1e-6,
            _ => 0.0,
        };
        let wall = started.elapsed().as_secs_f32() * 1000.0;
        let runq_workers_ms = threads_runqueue_wait_ns(&workers).saturating_sub(workers_start) as f32 * 1e-6;
        let canary_us = canary.run().max(canary_before);
        let machine_end = machine.sample();
        let psi_cpu_ms = machine_end.0.saturating_sub(machine_start.0) as f32 * 1e-3;
        let psi_mem_ms = machine_end.1.saturating_sub(machine_start.1) as f32 * 1e-3;
        let faults = u32::try_from(machine_end.2.saturating_sub(machine_start.2)).unwrap_or(u32::MAX);
        let major_faults = u32::try_from(major_faults(false).saturating_sub(major_start)).unwrap_or(u32::MAX);
        let counters = regia_render::frame_path_counters().since(frame_counters);
        for event in regia_render::take_frame_path_events() {
            if i >= options.warmup {
                traced.push((i - options.warmup, event));
            }
        }
        if options.idle_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(u64::from(options.idle_ms)));
        }
        if i >= options.warmup {
            // A held frame ran no renderer: its CPU times are 0 and its GPU time is the hold's.
            let cpu = if held_frame.is_some() {
                regia_render::FrameCpuTimings::default()
            } else {
                renderer.cpu_timings()
            };
            submits = submits.max(cpu.submits);
            samples.push(ChaseSample {
                wall_ms: wall,
                encode_ms: cpu.encode_ms,
                submit_ms: cpu.submit_ms,
                cpu_ms: cpu.total_ms,
                scatter_ms,
                scatter_update_ms: update_ms,
                contact_ms,
                collect_ms,
                gpu: match held_frame {
                    Some(held) => regia_render::GpuTimings {
                        frame_ms: held.gpu_ms,
                        ..Default::default()
                    },
                    None => renderer.gpu_timings(),
                },
                sssr: renderer.sssr_stats().unwrap_or_default(),
                rocks: rock_count,
                shadow_jobs: renderer.shadow_cache_report().jobs_per_kind,
                shadow_cpu: renderer.shadow_cpu_timings(),
                shared_cascades: renderer.shadow_cache_report().shared_per_cascade,
                wait_ms,
                counters,
                shared_upload: renderer.shared_upload_stats(),
                events,
                runq_ms,
                runq_workers_ms,
                psi_cpu_ms,
                psi_mem_ms,
                faults,
                major_faults,
                spawn_ms,
                spawn_major_faults,
                canary_us,
                tiles_placed,
                sky_worker: renderer.sky_rebuilding(),
                vt: terrain_stats.virtual_texture(),
                drawn: u32::try_from(renderer.draw_stats().drawn).unwrap_or(u32::MAX),
                instances: u32::try_from(frame.meshes.len()).unwrap_or(u32::MAX),
                casters: u32::try_from(renderer.shadow_caster_count()).unwrap_or(u32::MAX),
                local_lights: u32::try_from(frame.local_lights.len()).unwrap_or(u32::MAX),
                shadow_cache: ShadowCacheCounts::from_report(
                    renderer.shadow_cache_report(),
                    SUN_PAGES_PER_AXIS,
                ),
            });
        }
    }
    if let Some(squeeze) = squeeze.take() {
        println!(
            "memory squeeze: {} reclaim requests over the measured frames",
            squeeze.rounds()
        );
    }
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    regia_render::trace_frame_path(false);
    if precipitation.is_some() {
        println!(
            "precipitation occlusion view: {:?}",
            renderer.precipitation_occlusion()
        );
    }
    if lod_frames.iter().any(|&n| n > 0) {
        eprintln!("  scatter LOD instance-frames {lod_frames:?}, cross-fading {lod_fading}");
    }
    let counters = regia_render::frame_path_counters().since(counters_start);
    let cascade_draws = renderer.shared_shadow_draws() - draws_start;
    let region_draws = renderer.shared_shadow_region_draws() - regions_start;
    let textures = renderer.texture_memory();
    let frame_problems = renderer
        .frame_problems()
        .iter()
        .filter(|problem| {
            problem.code != regia_render::PROBLEM_TEXTURE_BUDGET
                && problem.code != regia_render::PROBLEM_UNCOMPRESSED_TEXTURES
        })
        .map(|problem| format!("{}: {}", problem.code, problem.message))
        .collect();
    let shared_draw = renderer.has_shared_draw();
    let mut motion = None;
    if options.png.is_some() || recorder.is_some() {
        // The last pose once more through the readback path, for a look at what was
        // measured; held until the history converged when the run is scored.
        // LOOK_R2_PNG_MOVING=1: one more step along the route, so the written frame is
        // mid-motion (TAA history from the previous pose; ghosting checks).
        let extra = u32::from(std::env::var_os("LOOK_R2_PNG_MOVING").is_some());
        let Pose {
            eye,
            target,
            at: p,
            dir,
            ground,
        } = motion_pose(if options.held {
            0
        } else {
            options.frames.saturating_sub(1) + extra
        });
        let mut frame = base.clone();
        frame.eye = eye;
        frame.target = target;
        let mut rocks = Vec::new();
        let scatter_view =
            ScatterView::look_at(frame.eye, frame.target, lab.fov_y, aspect, lab.near, lab.far)
                .with_sun(toward_sun.to_array());
        if let Some(field) = scene.scatter.as_ref() {
            field.collect(&scatter_view, &mut rocks);
        }
        if plate_camera.is_some() {
            let eye = frame.eye;
            rocks.retain(|m| keeps_clear(m, eye));
        }
        frame.meshes.extend(rocks);
        let yaw = (-dir[1]).atan2(dir[0]);
        let model = (Mat4::from_translation(Vec3::new(p[0], ground, p[1])) * Mat4::from_rotation_y(yaw))
            .to_cols_array_2d();
        for mesh in frame.meshes.iter_mut().skip(vehicle_first).take(vehicle_count) {
            mesh.model = model;
        }
        if let Some(m) = match_frame.as_ref() {
            m.instance(&mut frame, options.warmup + options.frames, eye);
        }
        let hold = options.seq.as_ref().map_or(1, |seq| seq.converge.max(1));
        let mut image = None;
        for _ in 0..hold {
            image = Some(
                renderer
                    .render_offscreen(device, &frame)
                    .map_err(|e| LookdevError::Config(e.to_string()))?,
            );
        }
        if let Some(image) = image {
            if let Some(png) = options.png.as_deref() {
                crate::default_look::write_png(png, image.width, image.height, &image.pixels)?;
            }
            if let Some(rec) = recorder.take() {
                // The same pose native: no jitter, no temporal resolve (the § 2.3 reference).
                let mut native_post = post.clone();
                native_post.taa = false;
                frame.post = Some(&native_post);
                frame.quality.taau = false;
                let native = renderer
                    .render_offscreen(device, &frame)
                    .map_err(|e| LookdevError::Config(e.to_string()))?;
                let as_frame = |image: regia_render::ForwardResult| crate::motion::Frame {
                    width: image.width,
                    height: image.height,
                    pixels: image.pixels,
                };
                motion = Some(rec.finish(&as_frame(image), &as_frame(native))?);
            }
        }
    }
    if let Some(d) = scene.do27.as_mut() {
        lab.materials.truncate(prop_end);
        d.map.prop_materials = lab.materials.split_off(prop_base);
    }
    let visbuffer = renderer.visbuffer_stats();
    let baked_gi = renderer.baked_gi_status();
    // CHASE_EXIT_TIMING=1: the clean exit's renderer step on this map (ADR-188 step 5): wait for
    // the GPU, then drop the renderer part by part and print the parts' times and the wgpu
    // objects it freed.
    if std::env::var_os("CHASE_EXIT_TIMING").is_some() {
        exit_timing(device, renderer);
    }
    Ok(ChaseRun {
        map: crate::do27::map_name(planet),
        samples,
        counters,
        submits,
        cascade_draws,
        region_draws,
        route_m,
        visbuffer,
        events: traced,
        motion,
        prewarm_ms,
        first_present_ms,
        exposure_bias_ev,
        checklist,
        checklist_diffs,
        checklist_errors,
        checklist_diff_lines,
        resident_bytes: textures.resident_bytes,
        texture_budget_bytes: textures.checked_budget(),
        texture_over_budget: textures.over_budget(),
        uncompressed_textures: textures.uncompressed,
        shared_draw,
        frame_problems,
        baked_gi,
        match_report: match_frame.map(|m| m.report),
    })
}

/// The match in the frame list: its prototypes' slots in the base scene and the layout.
struct MatchFrame {
    /// First prototype slot in the base frame's meshes.
    first: usize,
    /// One past the last prototype slot.
    end: usize,
    /// Each proto's parts, relative to `first`.
    parts: Vec<std::ops::Range<usize>>,
    layout: crate::match_scene::MatchLayout,
    report: crate::match_scene::MatchReport,
}

impl MatchFrame {
    /// Replaces the prototypes in `frame` (still at the origin) with frame `index`'s instances:
    /// per object every part with the object's model, a stable object key, static unless it
    /// drives, and casting only within the caster range of `eye`.
    fn instance(&self, frame: &mut regia_render::FrameScene<'_>, index: u32, eye: [f32; 3]) {
        if self.end > frame.meshes.len() {
            return;
        }
        let protos: Vec<regia_render::FrameMesh<'_>> = frame.meshes.drain(self.first..self.end).collect();
        let eye = Vec3::from_array(eye);
        for (object_ix, object) in self.layout.objects.iter().enumerate() {
            let Some(range) = self.parts.get(object.proto as usize) else {
                continue;
            };
            let model = self.layout.model(object, index);
            let shadow = if self.layout.casts(model.w_axis.truncate(), eye) {
                regia_render::ShadowCast::All
            } else {
                regia_render::ShadowCast::Off
            };
            for (part, proto) in protos.get(range.clone()).unwrap_or_default().iter().enumerate() {
                let mut mesh = *proto;
                mesh.model = (model * Mat4::from_cols_array_2d(&proto.model)).to_cols_array_2d();
                mesh.object_key = crate::match_scene::MatchLayout::object_key(object_ix, part as u32);
                mesh.is_static = !object.moving;
                mesh.shadow = shadow;
                frame.meshes.push(mesh);
            }
        }
    }
}

/// One camera pose of the run and the vehicle under it.
struct Pose {
    eye: [f32; 3],
    target: [f32; 3],
    /// Vehicle XZ.
    at: [f32; 2],
    /// Vehicle heading (unit XZ).
    dir: [f32; 2],
    /// Ground height under the vehicle.
    ground: f32,
}

/// `false` for a rock whose bounds reach the eye column (`look_r2`'s plate rule).
fn keeps_clear(mesh: &regia_render::FrameMesh<'_>, eye: [f32; 3]) -> bool {
    let [c0, c1, c2, at] = mesh.model;
    let scale = Vec3::from_slice(&c0[..3])
        .length()
        .max(Vec3::from_slice(&c1[..3]).length())
        .max(Vec3::from_slice(&c2[..3]).length());
    let d = Vec3::new(at[0] - eye[0], 0.0, at[2] - eye[2]).length();
    d > scale * 1.5 + 1.5
}

/// Per cascade over a run: mean shadow jobs per frame, frames the terrain redrew the whole
/// cascade, frames it redrew a page region, and mean ms.
#[must_use]
pub fn cascade_summary(run: &ChaseRun) -> [(f32, u32, u32, f32); 4] {
    let n = run.samples.len().max(1) as f32;
    std::array::from_fn(|c| {
        let jobs = run.samples.iter().map(|s| s.shadow_jobs[c] as f32).sum::<f32>() / n;
        let full = run.samples.iter().filter(|s| s.shared_cascades[c] == 1).count() as u32;
        let region = run.samples.iter().filter(|s| s.shared_cascades[c] == 2).count() as u32;
        let ms = run.samples.iter().map(|s| s.gpu.shadow_kind_ms[c]).sum::<f32>() / n;
        (jobs, full, region, ms)
    })
}

/// `p`-quantile (nearest rank) of `values`.
#[must_use]
pub fn percentile(values: &[f32], p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f32 * p).round() as usize]
}

/// Skins `mesh` with two joints at rest (CHASE_SKINNED): vertices above the middle of its height
/// on joint 1, the rest on joint 0, full weight each. Two joints keep it off the rigid-skin fold.
fn skin_two_joints(mesh: &mut regia_render::LabMesh) {
    let (low, high) = mesh
        .vertices
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), v| {
            (low.min(v.position[1]), high.max(v.position[1]))
        });
    let middle = (low + high) * 0.5;
    for v in &mut mesh.vertices {
        v.joints = [u32::from(v.position[1] > middle), 0, 0, 0];
        v.weights = [1.0, 0.0, 0.0, 0.0];
    }
    mesh.joints = vec![regia_render::JointMatrix::identity(); 2];
}

/// The per-pass rows of a run: `(name, p50, p99)`.
#[must_use]
pub fn rows(run: &ChaseRun) -> Vec<(&'static str, f32, f32)> {
    columns(run)
        .into_iter()
        .map(|(name, v)| (name, percentile(&v, 0.5), percentile(&v, 0.99)))
        .collect()
}

/// The per-pass columns of a run: `(name, one value per measured frame)`, in [`rows`]' order.
#[must_use]
pub fn columns(run: &ChaseRun) -> Vec<(&'static str, Vec<f32>)> {
    let col = |f: &dyn Fn(&ChaseSample) -> f32| -> Vec<f32> { run.samples.iter().map(f).collect() };
    let mut out = Vec::new();
    let mut push = |name: &'static str, v: Vec<f32>| out.push((name, v));
    push("wall", col(&|s| s.wall_ms));
    push("cpu render_into", col(&|s| s.cpu_ms));
    push("cpu encode", col(&|s| s.encode_ms));
    push("cpu submit", col(&|s| s.submit_ms));
    // The frame thread's shadow work (recording; the finish runs on the encode workers).
    push("cpu sun shadows", col(&|s| s.shadow_cpu.total_ms()));
    push("  shadow plan", col(&|s| s.shadow_cpu.plan_ms));
    push(
        "  terrain prepare (in plan)",
        col(&|s| s.shadow_cpu.shared_prepare_ms),
    );
    push("  caster lists (in plan)", col(&|s| s.shadow_cpu.caster_lists_ms));
    push("  cascade 0 encode", col(&|s| s.shadow_cpu.encode_ms[0]));
    push("  cascade 1 encode", col(&|s| s.shadow_cpu.encode_ms[1]));
    push("  cascade 2 encode", col(&|s| s.shadow_cpu.encode_ms[2]));
    push("  cascade 3 encode", col(&|s| s.shadow_cpu.encode_ms[3]));
    push("  atlas encode", col(&|s| s.shadow_cpu.encode_ms[4]));
    push("cpu scatter (host)", col(&|s| s.scatter_ms));
    push("  scatter update", col(&|s| s.scatter_update_ms));
    push("  contact map", col(&|s| s.contact_ms));
    push("  scatter collect", col(&|s| s.collect_ms));
    push("gpu frame", col(&|s| s.gpu.frame_ms));
    push("gpu shadows", col(&|s| s.gpu.shadows_ms));
    push("  cascade 0", col(&|s| s.gpu.shadow_kind_ms[0]));
    push("  cascade 1", col(&|s| s.gpu.shadow_kind_ms[1]));
    push("  cascade 2", col(&|s| s.gpu.shadow_kind_ms[2]));
    push("  cascade 3", col(&|s| s.gpu.shadow_kind_ms[3]));
    push("  local atlas", col(&|s| s.gpu.shadow_kind_ms[4]));
    push("gpu clusters", col(&|s| s.gpu.clusters_ms));
    push("gpu prepass", col(&|s| s.gpu.prepass_ms));
    push("  terrain (in prepass)", col(&|s| s.gpu.shared_prepass_ms));
    push("gpu cull", col(&|s| s.gpu.cull_ms));
    push("gpu ssao", col(&|s| s.gpu.ssao_ms));
    // GI Depth's probe update and screen lookup (`GpuTimings::gi_ms`; 0 under Balanced).
    push("gpu gi", col(&|s| s.gpu.gi_ms));
    push("gpu contact shadows", col(&|s| s.gpu.contact_shadows_ms));
    push("gpu forward", col(&|s| s.gpu.forward_ms));
    push("  terrain (in forward)", col(&|s| s.gpu.shared_forward_ms));
    push(
        "  meshes+sky (forward-terrain)",
        col(&|s| s.gpu.forward_ms - s.gpu.shared_forward_ms),
    );
    // The translucent section of the forward pass (glass and other blended draws), and what
    // is left: opaque meshes, plus the sky only where the frame has no LUT sky pass (DO27's
    // planets draw their sky in the atmosphere pass, so on their maps this is the meshes).
    push("  glass (in forward)", col(&|s| s.gpu.forward_glass_ms));
    // Before the translucent section, inside forward (ADR-7986): the refraction copy and the
    // glass front-layer pass; 0 on frames without a visible refracting draw.
    push(
        "  refraction copy (in forward)",
        col(&|s| s.gpu.refraction_copy_ms),
    );
    push(
        "  glass front layer (in forward)",
        col(&|s| s.gpu.glass_front_layer_ms),
    );
    push(
        "  meshes+sky (forward-terrain-glass-copy-front)",
        col(&|s| {
            s.gpu.forward_ms
                - s.gpu.shared_forward_ms
                - s.gpu.forward_glass_ms
                - s.gpu.refraction_copy_ms
                - s.gpu.glass_front_layer_ms
        }),
    );
    if run.samples.iter().any(|s| s.vt.is_some()) {
        let vt = |f: &dyn Fn(&regia_terrain::VtStats) -> f32| col(&|s| s.vt.as_ref().map_or(0.0, f));
        push("  vt bake (in shadows)", vt(&|v| v.bake_ms));
        push("  vt feedback reduce", vt(&|v| v.reduce_ms));
        push(
            "terrain forward + vt",
            col(&|s| s.gpu.shared_forward_ms + s.vt.as_ref().map_or(0.0, |v| v.bake_ms + v.reduce_ms)),
        );
        push("vt pages baked (count)", vt(&|v| v.baked as f32));
        push("vt pages re-baked (count)", vt(&|v| v.rebaked as f32));
        push("vt resident (count)", vt(&|v| v.resident as f32));
        push("vt missing pages (count)", vt(&|v| v.missing as f32));
        push("vt oldest miss (frames)", vt(&|v| v.oldest_miss_frames as f32));
        push("vt hit rate (%)", vt(&|v| v.hit_rate * 100.0));
    }
    push("gpu ssgi", col(&|s| s.gpu.ssgi_ms));
    push("gpu ssr", col(&|s| s.gpu.ssr_ms));
    push("gpu atmosphere", col(&|s| s.gpu.atmosphere_ms));
    push("gpu clouds", col(&|s| s.gpu.clouds_ms));
    push("gpu fog", col(&|s| s.gpu.fog_ms));
    push("gpu taau", col(&|s| s.gpu.taau_ms));
    push("gpu post", col(&|s| s.gpu.post_ms));
    push("rocks emitted (count)", col(&|s| s.rocks as f32));
    push("scatter tiles placed (count)", col(&|s| s.tiles_placed as f32));
    // The tester's frame-summary counts (telemetry's `drawn` and `counts.*`).
    push("drawn (count)", col(&|s| s.drawn as f32));
    push("mesh instances (count)", col(&|s| s.instances as f32));
    push("shadow casters (count)", col(&|s| s.casters as f32));
    push("local lights (count)", col(&|s| s.local_lights as f32));
    out
}

/// `CHASE_WARM_WORKER`: warms `renderer` on `lab`'s frame on a [`regia_render::PipelineWarmQueue`]
/// worker while this thread keeps submitting a window-sized clear every 16 ms (`busy`) or
/// sleeps, and returns it warmed. With `CHASE_WARM_SPANS=1` it prints the span totals of
/// every thread (warm worker, material-graph workers, this thread).
fn warm_on_worker(
    device: &RenderDevice,
    renderer: FrameRenderer,
    lab: LabScene,
    quality: regia_render::FrameQuality,
    format: wgpu::TextureFormat,
    busy: bool,
) -> Result<FrameRenderer, LookdevError> {
    use regia_render::{PipelineWarmQueue, WarmTier};
    let queue = PipelineWarmQueue::new(regia_render::pipeline_warm::default_threads());
    queue.start(device);
    let handle = queue.warm_renderer(WarmTier::Map, "game frame", renderer, move |r, dev| {
        let scene = lab.as_frame_scene_with_quality(quality);
        r.warm_scene(dev, &scene, format, false)
            .map_err(|e| e.to_string())
    });
    let screen = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("busy main thread target"),
        size: wgpu::Extent3d {
            width: 2560,
            height: 1440,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let screen_view = screen.create_view(&wgpu::TextureViewDescriptor::default());
    let mut ticks = 0u32;
    let mut slowest_tick = 0.0f32;
    while !handle.is_done() {
        let tick = Instant::now();
        if busy {
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("busy clear"),
                });
            drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("busy clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &screen_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            }));
            device.queue.submit([encoder.finish()]);
            let _ = device.device.poll(wgpu::PollType::Poll);
            ticks += 1;
            slowest_tick = slowest_tick.max(tick.elapsed().as_secs_f32() * 1000.0);
        }
        std::thread::sleep(std::time::Duration::from_millis(16).saturating_sub(tick.elapsed()));
    }
    let warmed = handle
        .take()
        .ok_or_else(|| LookdevError::Config("warm handle empty".into()))?;
    queue.shutdown();
    println!(
        "warm job: {:.1} ms, {} pipelines on the warm thread, error {:?}; main thread {ticks} ticks, slowest {slowest_tick:.1} ms",
        warmed.ms, warmed.pipelines, warmed.error
    );
    warmed
        .renderer
        .ok_or_else(|| LookdevError::Config("warm job panicked".into()))
}
