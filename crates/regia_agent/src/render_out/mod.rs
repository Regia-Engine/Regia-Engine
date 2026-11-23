// SPDX-License-Identifier: MPL-2.0
//! Offline fixed-dt render-out: `render.out_start` / `render.out_progress` / `render.out_cancel`
//! (book `agents/render-out.md`, guide topic `render_out`, ADR-267).
//!
//! A job takes over the game clock ([`TimeUpdateStrategy::ManualDuration`] of exactly one frame
//! at the requested rate) and, once per app frame, renders the extracted frame offscreen at the
//! requested size times the supersampling factor, with the game HUD laid out for the output
//! ([`regia_render::RenderOutHud`], ADR-202), averages it down, hashes it and hands it to a
//! [`Sink`] (a PNG per frame or an `ffmpeg` pipe). The path reads no wall clock: the step, the
//! cut frames and the order of work come from the request alone, so the same project and request
//! give the same frame hashes (lavapipe: byte for byte). Frames are not paced; they are made as
//! fast as they render.
//!
//! Frame 0 and every `cuts` frame start with a camera cut ([`regia_render::FrameRenderer::camera_cut`]):
//! TAA/TSR history, fog history, the static-screen cache, cached shadow cascades (the terrain's
//! shared draw included), the irradiance probes and the metered exposure restart as in a fresh
//! renderer, which also makes the jitter sequence start at 0 at every cut instead of at the
//! renderer's global frame count.
//!
//! With auto exposure on, a job cut is also metered (ADR-1455): the meter restarts, reads the cut
//! frame in `EXPOSURE_READBACK_FRAMES` undelivered renders of the same game frame, and a second cut
//! (`CutExposure::Fade`) restarts every history again but keeps that reading, so the delivered
//! frame is exposed for itself instead of at the meter's initial EV. A job waits for its exposure
//! readbacks (`set_wait_for_exposure_readback`), so the meter reads the same histograms on the same
//! frames in every run.
//!
//! Shutter accumulation and the 16-bit readout (ADR-1404): with `subframes` > 1 a delivered frame
//! averages that many renders taken while a virtual shutter is open (`shutter`); the game clock
//! steps between them and every frame still advances by exactly one step. With `bit_depth: 16`
//! the frame is rendered into a half-float target (`deep`) and written as a 16-bit PNG. Both
//! sum the samples in linear light and quantize once (`accumulate`).
//!
//! A time map (`time_map`, ADR-1519, ADR-1524) scales the simulation time each delivered frame
//! covers, per frame range (slow motion, speed ramps). The job runs two clocks: the render clock
//! (`TimeUpdateStrategy`, so `Time::delta` and the UI clock) steps exactly as without a map, so VFX,
//! shots, HUD animation, the material clock and the shutter keep the output's speed; the
//! simulation clock (`regia_app::SimStep`, so `Time::sim_delta`, the fixed loop and sim bridges)
//! steps by each frame's own simulation step. The fixed loop keeps its fixed rate; a slowed frame
//! runs fewer fixed steps. A replay armed with `replay.play { start: "render_out" }` starts on
//! frame 0 at its stated tick and plays on the simulation clock, so its `speed` composes with the
//! map.
//!
//! AOVs (`aovs`, ADR-1521): depth, instance and class ids and motion vectors are drawn by their
//! own unjittered pass (`aov_pass`) once per delivered frame, at its middle subframe, and written
//! next to the frame as EXR or raw files with the encodings in the manifest (`aov`). `format:
//! "exr"` writes the scene-linear frame (the post stack's input, before exposure and the
//! tonemap) instead of the display image (`exr_file`).
//!
//! Frame files (PNG, or `raw`) are encoded and written by a bounded pool of worker threads
//! (`writer`, ADR-1412), so the render thread only renders, hashes and queues; it waits only when
//! the queue is full. The hash is of the delivered pixels before any encoding, so it does not
//! depend on the format, the PNG level or the number of writers. Cancel and failure keep exactly
//! the frames from 0 up to the first one that is missing, and no later file.
//!
//! A job closes its outputs off the app thread (ADR-1881). When the last frame is delivered (or the
//! job is cancelled or fails) the app thread hands the clocks back and ends the mixdown; a closing
//! thread then waits for the frame writers or `ffmpeg` and cuts, syncs and renames the WAVs, which
//! can take a minute on a busy disk. The app keeps running and `render.out_progress` keeps answering
//! (`status: "running"`, `phase: "closing"`); the status changes, and the manifest is written, once
//! every file is in place.

mod accumulate;
mod aov;
mod aov_pass;
mod aov_png;
mod audio;
pub(crate) mod capture;
// The progress board a connection answers from while the app thread is inside a step (ADR-2582).
mod board;
// Camera shakes a job restarts or puts back (ADR-2687).
mod camera_state;
mod deep;
mod encode_sink;
mod exr_file;
mod plan;
// A job's product: frames from the product viewer's raster renderer (ADR-2757).
mod product;
mod shape;
mod shutter;
mod sink;
mod time_map;
mod timing;
mod writer;
// Takes, cut lists and the shot drive of a `.render_job.ron` (ADR-1616).
mod takes;

use regia_app::prelude::*;
use regia_render::runtime::RuntimeRenderer;
use regia_render::{DisplayCalibration, RenderOutHud, RenderOutSpec};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};
use accumulate::{Accumulator, Depth};
pub use audio::{RenderOutFileSync, SyncFile};
pub use board::{BOARD_ANSWER_AFTER, RenderOutBoard};
pub(crate) use plan::resolve_under_roots;
use plan::{Request, downsample, fnv1a, fold_hash, warmup_size};
pub(crate) use shape::param_shape;
use sink::Sink;
pub(crate) use takes::{drive_job_sequences, drive_job_shots, out_job};
use writer::FrameData;

/// The directory a relative `out` is joined to: the asset server's project root; in an app without
/// one (a headless host such as the showcase, a test app), the project directory the agent was
/// given (`AgentSettings::project_dir`); else the working directory (ADR-1561).
fn out_root(world: &World) -> Option<std::path::PathBuf> {
    world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .or_else(|| {
            world
                .get_resource::<crate::AgentSettings>()
                .and_then(|s| s.project_dir.clone())
        })
        .or_else(|| std::env::current_dir().ok())
}

/// Confines a caller-supplied file or directory `path` (parameter `key`) to the same output roots as
/// `render.out_start`: the project root, system temp dir and `agent.toml` `output_roots`.
/// Relative paths are joined to the project root; `..` and symlinks leading out are refused.
pub(crate) fn confine_path(
    world: &World,
    key: &str,
    path: &std::path::Path,
) -> Result<std::path::PathBuf, RpcError> {
    let root = out_root(world)
        .ok_or_else(|| RpcError::internal(format!("no project root to resolve `{key}` against")))?;
    let extra_roots = world
        .get_resource::<crate::safety::AgentPermissions>()
        .map(|p| p.config.output_roots.clone())
        .unwrap_or_default();
    plan::confine_out_dir(&root, &extra_roots, path).map_err(|mut e| {
        e.message = e.message.replace("`out`", &format!("`{key}`"));
        e
    })
}

/// Most frame hashes one `render.out_progress` call returns.
const HASHES_PER_CALL: usize = 4096;
/// Finished jobs kept for `render.out_progress`.
const KEEP_FINISHED: usize = 8;

/// What the job borrowed from the game clock and the renderer.
struct Saved {
    strategy: TimeUpdateStrategy,
    paused: bool,
    time_scale: f32,
    /// The renderer's exposure-readback wait before the job turned it on.
    wait_readback: bool,
    /// The renderer's complete-frames switch before the job turned it on (ADR-2582).
    complete_frames: bool,
    /// `FixedTime::max_substeps` before a time-mapped job raised it (`None`: untouched).
    max_substeps: Option<u32>,
    /// The host's simulation step before a time-mapped job set its own (ADR-1524).
    sim_step: Option<SimStep>,
    /// `Time::elapsed` and `Time::sim_elapsed` when the job took the clocks; the job runs them from
    /// zero and adds what it ran to these at the end (ADR-1881).
    clocks: (std::time::Duration, std::time::Duration),
    /// The weather's clock and drift, likewise.
    weather: Option<regia_vfx::WeatherClock>,
    /// The live effects as a held job found them; they go back when its frames end (ADR-2633).
    effects: Option<regia_vfx::live_state::LiveEffectsCapture>,
    /// The camera shakes as a held job found them, likewise (ADR-2687).
    shakes: Option<camera_state::HeldShakes>,
    /// The effects layer's history the job restarted; the app's seed stream goes back at the end
    /// (ADR-2687).
    effects_start: Option<regia_vfx::live_state::EffectsJobStart>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// The frame `render.out_start` was handled in ran on the old clock; it is skipped.
    Arming,
    /// Frames rendered small and thrown away (frames left), so VFX and streaming settle.
    Warmup(u64),
    Rendering,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Status {
    Done,
    Cancelled,
    Failed(String),
}

impl Status {
    fn name(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Failed(_) => "failed",
        }
    }
}

struct Job {
    id: u64,
    request: Request,
    sink: Option<Sink>,
    hud: Option<RenderOutHud>,
    phase: Phase,
    delivered: u64,
    hashes: Vec<u64>,
    sequence: u64,
    saved: Saved,
    cancel: bool,
    cuts_before: u64,
    /// Job cuts that metered their frame's exposure: each one is two renderer cuts.
    metered_cuts: u64,
    /// The subframe the next render is (0 when the shutter is off).
    subframe: u32,
    /// Sums of the frame being accumulated (shutter or 16-bit jobs only).
    acc: Option<Accumulator>,
    /// The half-float render target of a 16-bit job, kept at the render size.
    deep_target: Option<regia_render::DeepTarget>,
    /// Simulation time of frame `delivered` after frame 0, nanoseconds (the time map's running
    /// sum).
    sim_ns: u64,
    /// The tick an armed replay started from on frame 0 (`replay.play { start: "render_out" }`).
    replay_from: Option<u32>,
    /// The product the job renders instead of the world (ADR-2757).
    product: Option<product::ProductRun>,
    /// The WAV mixdown and stems (`audio`, ADR-1520).
    audio: Option<audio::AudioOut>,
    /// The AOV pass and the AOVs of the frame being made (ADR-1521).
    aov: aov_pass::AovState,
    /// Sum of the scene-linear subframes of the frame being made (format `exr`).
    hdr_sum: Option<Vec<f32>>,
    /// Delivered frames a shot fade was visible on, with the fade drawn (ADR-1522, ADR-1554).
    fades: Vec<(u64, regia_render::FadeState)>,
    /// The takes of a `render.out_job` (ADR-1616): which frames go where, and the shot drive.
    /// `None` for a plain `render.out_start`.
    takes: Option<takes::TakeRun>,
    /// The app's clocks when the job started (the manifest's `clock` block).
    start_clock: StartClock,
    /// Set once the job has ended: its closing thread is at work (ADR-1881).
    closing: Option<Closing>,
    /// A made frame whose hand-over to a full output was interrupted by a cancel request
    /// (ADR-2396): handed over (and booked) before anything else is rendered.
    pending: Option<Pending>,
    /// The clocks' steps while a pending frame waits: the app frames meanwhile step neither clock,
    /// so frame N is still frame N once it is handed over (ADR-2396).
    held: Option<(Option<TimeUpdateStrategy>, Option<SimStep>)>,
    /// `cancel_notes()` at the end of the job's last app frame: a cancel request noted since may
    /// not have reached dispatch yet, so it ends a wait for a full output (ADR-2396).
    notes_seen: u64,
    /// Wall milliseconds of each delivered frame's render and readback (`timings`, ADR-2444).
    render_ms: Vec<f64>,
    /// App frames of the job that rendered (warm-up, settle or delivered): `0` while the job's
    /// first render, which compiles pipelines and uploads the scene on a cold host, has not
    /// finished (`render.out_progress` `warming`, ADR-2582).
    renders: u64,
}

/// A frame made but not yet handed over (ADR-2396).
struct Pending {
    index: u64,
    frame: FrameData,
    aovs: Vec<writer::AovFile>,
    hash: u64,
}

/// A job that stopped making frames and is closing its outputs.
struct Closing {
    /// The closing thread ([`close_outputs`]).
    thread: std::thread::JoinHandle<Closed>,
    /// The job's cuts when it ended.
    cuts: u64,
    /// The `audio` block `render.out_progress` shows meanwhile.
    audio: Option<Value>,
    /// The status the job's frames ended with (`render.out_cancel` reports it while closing; the
    /// closing thread can still turn it into `failed`).
    ending: &'static str,
}

/// What the closing thread closes: everything that can block on the disk.
struct CloseWork {
    request: Request,
    status: Status,
    delivered: u64,
    sink: Option<Sink>,
    audio: Option<audio::AudioClose>,
    takes: Option<takes::TakeRun>,
    sync: RenderOutFileSync,
}

/// What closing the outputs found.
struct Closed {
    /// The job's status, failed when a file could not be closed.
    status: Status,
    files: Vec<std::path::PathBuf>,
    /// Frames on disk, when fewer may have landed than were delivered (a frame sequence).
    on_disk: Option<u64>,
    /// The manifest's `audio` block (`null` without audio).
    audio: Value,
    takes: Option<takes::TakeRun>,
    take_results: Vec<takes::TakeResult>,
}

impl Closed {
    /// Nothing could be closed: the job ends with `status`.
    fn nothing(status: Status) -> Self {
        Self {
            status,
            files: Vec::new(),
            on_disk: None,
            audio: Value::Null,
            takes: None,
            take_results: Vec::new(),
        }
    }
}

/// The app's clocks at `render.out_start`, before the job ran them from zero: how far the world
/// had got. The simulation time is the part that says where the world stood; the frame count only
/// counts app updates, and a paused app runs those at the speed of its host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StartClock {
    /// `Time::elapsed`, nanoseconds.
    game_ns: u64,
    /// `Time::sim_elapsed`, nanoseconds.
    sim_ns: u64,
    /// `Time::frame_count`.
    app_frames: u64,
    /// The job holds the world, so the live effects go back when its frames end (ADR-2633).
    effects_restored: bool,
    /// Nothing was alive in the particle pool, so its clock and ring started over (ADR-2687).
    particle_clock_restarted: bool,
}

impl StartClock {
    fn of(world: &World) -> Self {
        world
            .get_resource::<Time>()
            .map_or_else(Self::default, |time| Self {
                game_ns: u64::try_from(time.elapsed().as_nanos()).unwrap_or(u64::MAX),
                sim_ns: u64::try_from(time.sim_elapsed().as_nanos()).unwrap_or(u64::MAX),
                app_frames: time.frame_count(),
                effects_restored: false,
                particle_clock_restarted: false,
            })
    }

    /// The manifest's `clock` block (ADR-1656, ADR-1881): which clock the frames were made on, that
    /// the job ran it from zero, and where the app's clocks stood at the start. Frames repeat for
    /// the same request from the same world state, and these say how far that world had got.
    fn to_json(self) -> Value {
        json!({
            "mode": "fixed_step",
            "wall_clock": false,
            "step_clock": "request.fps / request.fps_den",
            "job_time_origin": "zero",
            "app_game_time_at_start_ns": self.game_ns,
            "app_sim_time_at_start_ns": self.sim_ns,
            "app_frame_at_start": self.app_frames,
            "live_effects": if self.effects_restored { "restored" } else { "moved_on" },
            "effect_seeds": "job_stream",
            "particle_clock": if self.particle_clock_restarted { "restarted" } else { "continued" },
        })
    }
}

/// A finished job, kept for `render.out_progress`.
struct Report {
    id: u64,
    status: Status,
    request: Request,
    delivered: u64,
    hashes: Vec<u64>,
    sequence: u64,
    files: Vec<String>,
    cuts: u64,
    metered_cuts: u64,
    manifest: Option<String>,
    /// Simulation time of frame `delivered` after frame 0, nanoseconds.
    sim_ns: u64,
    /// The tick an armed replay started from on frame 0.
    replay_from: Option<u32>,
    /// The manifest's `audio` block (`null` without audio).
    audio: Value,
    /// The manifest's `job` block (`null` without takes, ADR-1616).
    job: Value,
}

/// The running and the recently finished render-out jobs.
#[derive(Resource, Default)]
pub struct RenderOutJobs {
    next_id: u64,
    active: Option<Job>,
    finished: Vec<Report>,
}

fn hex(hash: u64) -> String {
    format!("{hash:016x}")
}

fn request_json(request: &Request) -> Value {
    json!({
        "width": request.width,
        "height": request.height,
        "fps": request.fps_num,
        "fps_den": request.fps_den,
        "frames": request.frames,
        "supersample": request.supersample,
        "format": request.format.name(),
        "out": request.shown(&request.out_dir),
        "name": request.name,
        "hud": request.hud,
        "timings": request.timings,
        "cuts": request.cuts,
        "warmup_frames": request.warmup,
        "settle_frames": request.settle,
        "step_ns": request.step().as_nanos() as u64,
        "subframes": request.shutter.subframes,
        "shutter_angle": request.shutter.angle_deg(),
        "bit_depth": request.bit_depth.bits(),
        "png_compression": request.png_level.name(),
        "writers": request.writer_threads(),
        "time_map": request.time_map.to_json(),
        "audio": request.audio.as_ref().map(audio::AudioRequest::to_json),
        "aovs": request.aovs.to_json()["aovs"],
        "aov_format": request.aovs.to_json()["aov_format"],
        "encode": request.format.is_encoded().then(|| {
            let mut encode = serde_json::to_value(&request.encode).unwrap_or(Value::Null);
            encode["threads"] = json!(request.encode.worker_threads(
                std::thread::available_parallelism().map_or(1, usize::from)
            ));
            encode
        }),
    })
}

/// Fixed steps one app frame of `peak_ns` game time can need: the accumulator holds under one
/// step before the frame, so at most `1 + peak / step`.
fn fixed_steps_needed(peak_ns: u64, fixed_step_ns: u64) -> u64 {
    1 + peak_ns / fixed_step_ns.max(1)
}

/// Refuses a time map whose fastest frame would need more fixed steps than the fixed loop runs in
/// one frame (`MAX_SUBSTEPS_LIMIT`), and returns the steps it needs (`None` without a map or a
/// fixed clock).
fn time_map_substeps(world: &World, request: &Request) -> Result<Option<u32>, RpcError> {
    if request.time_map.is_identity() {
        return Ok(None);
    }
    let Some(fixed) = world.get_resource::<FixedTime>() else {
        return Ok(None);
    };
    let fixed_step = fixed.step().as_nanos() as u64;
    let need = fixed_steps_needed(request.peak_sim_step_ns(), fixed_step);
    if need > u64::from(regia_app::MAX_SUBSTEPS_LIMIT) {
        return Err(RpcError::invalid_params(format!(
            "`time_map` is too fast for the fixed loop: one frame would run {need} fixed steps of {fixed_step} ns, and at most {} run per frame; lower the peak `scale` or raise `fps`",
            regia_app::MAX_SUBSTEPS_LIMIT
        )));
    }
    Ok(Some(need as u32))
}

fn hash_window(hashes: &[u64], params: &Value) -> Result<Value, RpcError> {
    if crate::params::opt_bool(params, "hashes")? != Some(true) {
        return Ok(Value::Null);
    }
    let from = crate::params::opt_u64(params, "hashes_from")?.unwrap_or(0) as usize;
    let list: Vec<String> = hashes
        .iter()
        .skip(from)
        .take(HASHES_PER_CALL)
        .map(|h| hex(*h))
        .collect();
    Ok(json!({ "from": from, "hashes": list }))
}

/// `render.out_start`: validates the request, opens the sink, takes over the clock and arms the job.
pub(crate) fn start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    start_request(world, Request::parse(params)?, None)
}

/// The takes a `render.out_job` hands to `start_request`: the resolved plan and the shot list it
/// plays.
pub(crate) struct PendingTakes {
    /// The resolved job.
    plan: takes::TakePlan,
    /// The `.shots.ron` the job drives the shot player with.
    shots: Option<regia_framework::shots::ShotSequence>,
    /// The sequence player the job's binding table creates (ADR-1880).
    player: Option<takes::JobPlayer>,
    /// The product the job renders instead of the world, and its player (ADR-2757).
    product: Option<(product::ProductRun, product::ProductRig)>,
}

/// Starts a validated request: with `pending` takes (`render.out_job`) the frames go to the takes'
/// outputs instead of one sink (ADR-1616).
fn start_request(
    world: &mut World,
    mut request: Request,
    pending: Option<PendingTakes>,
) -> Result<Value, RpcError> {
    if world
        .get_resource::<RenderOutJobs>()
        .is_some_and(|jobs| jobs.active.is_some())
    {
        return Err(RpcError::new(
            protocol::INVALID_REQUEST,
            "F0018",
            "a render-out is already running",
            "render.out_progress shows it; render.out_cancel stops it",
        ));
    }
    if world
        .get_resource::<crate::playtest::AgentTimeControl>()
        .is_some_and(|control| control.stepping.is_some())
    {
        return Err(RpcError::new(
            protocol::INVALID_REQUEST,
            "F0009",
            "a play.step is running",
            "wait for it to finish before starting a render-out",
        ));
    }
    if crate::loading::offscreen_warm_busy(world) {
        return Err(RpcError::new(
            protocol::INVALID_REQUEST,
            "F0018",
            "the offscreen warm has the renderer (render.pipeline_warm offscreen)",
            "wait until render.pipeline_warm answers offscreen_warm.state \"done\"",
        ));
    }
    // Confinement runs after the busy and play-step checks, before the renderer check and anything is created.
    let root =
        out_root(world).ok_or_else(|| RpcError::internal("no project root to resolve `out` against"))?;
    let extra_roots = world
        .get_resource::<crate::safety::AgentPermissions>()
        .map(|p| p.config.output_roots.clone())
        .unwrap_or_default();
    request.confine_to(&root, &extra_roots)?;
    let substeps = time_map_substeps(world, &request)?;
    // A host that opened no GPU on purpose (the headless editor's `--no-gpu`) refuses here, before
    // a job exists, instead of a device being opened behind its back (ADR-5180). A product job is
    // refused earlier, in `takes::out_job`, before its own device is opened.
    if world.get_resource::<regia_render::RenderDevice>().is_none()
        && let Some(disabled) = world.get_resource::<crate::seeing::GpuDisabled>()
    {
        return Err(disabled.error());
    }
    // A product job draws with its own renderer on its own device, so it needs none of the game's.
    let renders_world = !pending.as_ref().is_some_and(|p| p.product.is_some());
    if renders_world && world.get_resource::<RuntimeRenderer>().is_none() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0011",
            "no RuntimeRenderer",
            "add regia_render::RenderPlugin (headless: true for an offscreen app)",
        ));
    }
    if request.bit_depth == Depth::Sixteen && request.hud && world.get_resource::<RenderOutHud>().is_some() {
        return Err(RpcError::invalid_params(
            "`bit_depth` 16 draws no HUD (the UI overlay is 8-bit); pass `hud: false`",
        ));
    }
    if request.format == plan::Format::Exr && request.hud && world.get_resource::<RenderOutHud>().is_some() {
        return Err(RpcError::invalid_params(
            "format \"exr\" is the scene before the UI is drawn; pass `hud: false`",
        ));
    }
    if let Some(wanted) = &request.audio {
        audio::check(world, wanted)?;
    }
    // A job's takes open their own outputs on their first frames; the timeline only needs its
    // folder (the manifest and the audio go there).
    let sink = if pending.is_some() {
        // The takes' movies open on their first frames; a missing ffmpeg is refused now, as for
        // one movie.
        if request.format.needs_ffmpeg() && sink::find_ffmpeg().is_none() {
            return Err(RpcError::new(
                protocol::UNSUPPORTED,
                "F0019",
                "no `ffmpeg` on PATH (formats ffv1, prores and dnxhr pipe frames to it)",
                "install ffmpeg, or render format \"png\" and encode the sequence later",
            ));
        }
        std::fs::create_dir_all(&request.out_dir)
            .map_err(|e| RpcError::internal(request.scrub(&format!("{}: {e}", request.out_dir.display()))))?;
        None
    } else {
        Some(Sink::open(&request).map_err(|message| {
            let message = request.scrub(&message);
            if request.format.needs_ffmpeg() && message.starts_with("no `ffmpeg`") {
                RpcError::new(
                    protocol::UNSUPPORTED,
                    "F0019",
                    message,
                    "install ffmpeg, or render format \"png\" and encode the sequence later",
                )
            } else {
                RpcError::internal(message)
            }
        })?)
    };
    // The WAVs open and the mixdown starts once the sink is open, so a refused sink leaves the
    // mixer alone.
    let audio_out = match &request.audio {
        Some(wanted) => match audio::AudioOut::start(world, &request, wanted) {
            Ok(out) => Some(out),
            Err(message) => {
                if let Some(sink) = sink {
                    let _ = sink.finish();
                }
                return Err(RpcError::internal(request.scrub(&message)));
            }
        },
        None => None,
    };
    let hud = request
        .hud
        .then(|| world.get_resource::<RenderOutHud>().cloned())
        .flatten();
    let mut saved = {
        let strategy = world
            .get_resource::<TimeUpdateStrategy>()
            .copied()
            .unwrap_or_default();
        let (wait_readback, complete_frames) = match world.get_resource_mut::<RuntimeRenderer>() {
            Some(mut renderer) => {
                let wait = renderer.0.waits_for_exposure_readback();
                // Every frame of the job is complete (ADR-2582): nothing the renderer moved off the
                // frame (graph pipelines, the sky, emissive fits, streamed mips, the SSR sleep) lands
                // on a frame chosen by the CPU's or the GPU's speed. What was queued lands now.
                // Capture the host's switch state before forcing anything: the switch saves the
                // exposure wait it finds, so forcing the wait first would save the forced value
                // and the restore would put back `true` on a host that ran with it off — a job
                // ending during a warm then leaves the wait on for good on the renderer that
                // comes back (audit fix-render6 #1). Turning the switch on forces the wait, but
                // a host that already held the switch on may have turned the wait off since,
                // and an on switch forces nothing again: so the job forces the wait itself, after
                // the capture, for a world-rendering job (review of fixrev-20-k, finding 3).
                let complete = renderer.0.complete_frames();
                renderer.0.set_complete_frames(renders_world || complete);
                if renders_world {
                    renderer.0.set_wait_for_exposure_readback(true);
                }
                (wait, complete)
            }
            None => (false, false),
        };
        // A time map's fast frames run more fixed steps per app frame than a host's default
        // catch-up limit; the job raises it for its own length (ADR-1519).
        let max_substeps = substeps.and_then(|need| {
            let mut fixed = world.get_resource_mut::<FixedTime>()?;
            let before = fixed.max_substeps;
            (need > before).then(|| {
                fixed.max_substeps = need;
                before
            })
        });
        let sim_step = world.get_resource::<SimStep>().copied();
        let mut time = world.resource_mut::<Time>();
        let saved = Saved {
            strategy,
            paused: time.paused,
            time_scale: time.time_scale,
            wait_readback,
            complete_frames,
            max_substeps,
            sim_step,
            clocks: (time.elapsed(), time.sim_elapsed()),
            weather: None,
            effects: None,
            shakes: None,
            effects_start: None,
        };
        time.paused = false;
        time.time_scale = 1.0;
        saved
    };
    job_clocks(world, &request);
    let cuts_before = world
        .get_resource::<RuntimeRenderer>()
        .map_or(0, |renderer| renderer.0.camera_cut_count());
    let mut start_clock = StartClock::of(world);
    saved.weather = restart_job_state(world);
    // A job with the world held leaves the live effects where it found them, so the next held job
    // plays the same frames (ADR-2633). The job's own frames still show them moving (ADR-1524).
    saved.effects = request
        .time_map
        .holds_the_world()
        .then(|| regia_vfx::live_state::LiveEffectsCapture::capture(world));
    saved.shakes = request
        .time_map
        .holds_the_world()
        .then(|| camera_state::HeldShakes::capture(world));
    start_clock.effects_restored = saved.effects.is_some();
    // New effects and shakes of the job look the same in every job: the history that picks their
    // seeds, and an idle particle clock, start over (ADR-2687). After the capture, so a held job
    // hands back what it found.
    let effects_start = regia_vfx::live_state::EffectsJobStart::restart(world);
    start_clock.particle_clock_restarted = effects_start.pool_restarted();
    saved.effects_start = Some(effects_start);
    camera_state::restart_idle_seeds(world);
    // The take outputs live under the confined `out`; the shot player plays the job's list.
    let (mut take_run, job_player, product) = match pending {
        Some(pending) => (
            Some(takes::TakeRun::new(pending.plan, &request, pending.shots)),
            pending.player,
            pending.product,
        ),
        None => (None, None, None),
    };
    let product = product.map(|(run, rig)| {
        world.insert_non_send(rig);
        run
    });
    if let Some(run) = take_run.as_mut() {
        takes::begin_shots(world, run);
        takes::spawn_job_player(world, run, job_player);
        // A product job leaves the world's sequence players alone.
        if product.is_none() {
            takes::begin_sequences(world, run);
        }
    }
    let mut jobs = world.get_resource_or_insert_with(RenderOutJobs::default);
    jobs.next_id += 1;
    let id = jobs.next_id;
    let mut answer = json!({
        "job": id,
        "status": "running",
        "request": request_json(&request),
        "render_size": [request.render_width(), request.render_height()],
        "hud": hud.is_some(),
        "ffmpeg": request
            .format
            .needs_ffmpeg()
            .then(|| sink::find_ffmpeg().map(|_| "ffmpeg".to_string()))
            .flatten(),
        "audio": audio_out.as_ref().map(|out| out.progress_json(&request)),
    });
    if let Some(run) = take_run.as_ref() {
        answer["plan"] = run.plan.to_json();
    }
    jobs.active = Some(Job {
        id,
        request,
        sink,
        hud,
        phase: Phase::Arming,
        delivered: 0,
        hashes: Vec::new(),
        sequence: 0,
        saved,
        cancel: false,
        cuts_before,
        metered_cuts: 0,
        subframe: 0,
        acc: None,
        deep_target: None,
        sim_ns: 0,
        replay_from: None,
        audio: audio_out,
        aov: aov_pass::AovState::default(),
        hdr_sum: None,
        fades: Vec::new(),
        takes: take_run,
        start_clock,
        closing: None,
        pending: None,
        held: None,
        notes_seen: cancel_notes(),
        product,
        render_ms: Vec::new(),
        renders: 0,
    });
    Ok(answer)
}

/// The job's clocks from its first frame on: the render clock steps by the output step, and with a
/// time map the simulation clock is set apart (ADR-1524): warm-up frames and the step into frame 0
/// run at the scale before the first key. Without one the clocks stay one.
fn job_clocks(world: &mut World, request: &Request) {
    world.insert_resource(TimeUpdateStrategy::ManualDuration(request.step()));
    if !request.time_map.is_identity() {
        set_sim_step(world, request.pre_sim_step_ns());
    }
}

/// What a job restarts so that its frames do not depend on what the app did before it (ADR-1881):
/// the game and simulation clocks run from zero (anything animated by `Time::elapsed`, such as the
/// material clock's wind sway, starts at the same phase in every job), the fixed loop has no time
/// banked (the same fixed steps land on the same frames), the weather's cloud clock and drift
/// restart, and every mesh and streamed scatter instance picks its LOD afresh (ADR-2672). Temporal render state restarts at the job's opening
/// camera cut. The world itself is not rewound: game state and anything a host runs on its own
/// timeline are the job's input. Returns the weather clock to hand back at the end.
fn restart_job_state(world: &mut World) -> Option<regia_vfx::WeatherClock> {
    world
        .resource_mut::<Time>()
        .set_clocks(std::time::Duration::ZERO, std::time::Duration::ZERO);
    if let Some(mut fixed) = world.get_resource_mut::<FixedTime>() {
        fixed.clear_accumulator();
    }
    world.insert_resource(regia_render::MeshLodRestart);
    // The streamed scatter keeps its own LOD memory (ADR-2672).
    if let Some(mut scatter) = world.get_resource_mut::<regia_terrain::RuntimeScatter>() {
        scatter.field_mut().restart_lod();
    }
    world
        .get_resource_mut::<regia_vfx::Weather>()
        .map(|mut weather| weather.restart_clock())
}

/// Which render-out a `replay.play { start: "render_out" }` arm starts with: the running job when
/// it has not reached frame 0 yet, else the next one.
pub(crate) fn armed_replay_target(world: &World) -> &'static str {
    let waiting = world
        .get_resource::<RenderOutJobs>()
        .and_then(|jobs| jobs.active.as_ref())
        .is_some_and(|job| job.phase != Phase::Rendering && job.closing.is_none());
    if waiting { "running_job" } else { "next_job" }
}

/// Runs a `.render_job.ron` with a `product` block in this process, with no app: `regia render --job` on a machine
/// with nothing running. The same code as `render.out_job` in a running app, on a world of its own. `on_progress`
/// sees each `render.out_progress` answer while it runs and returns `false` to cancel. Answers the plan for a dry
/// run, else the last `render.out_progress` (status, frames, `manifest`).
///
/// # Errors
/// What `render.out_job` refuses, with the same codes.
pub fn run_product_job(
    project_dir: &std::path::Path,
    job_text: &str,
    date: Option<&str>,
    dry_run: bool,
    on_progress: &mut dyn FnMut(&Value) -> bool,
) -> Result<Value, RpcError> {
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.insert_resource(crate::AgentSettings {
        project_dir: Some(project_dir.to_path_buf()),
    });
    world.insert_resource(crate::safety::AgentPermissions::load(project_dir));
    let started = out_job(
        &mut world,
        &json!({ "job": job_text, "date": date, "dry_run": dry_run }),
    )?;
    if dry_run {
        return Ok(started);
    }
    loop {
        advance_render_out(&mut world);
        let answer = progress(&mut world, &json!({}))?;
        if answer["status"] != "running" && answer["status"] != "cancelling" {
            return Ok(answer);
        }
        if answer["phase"] == "closing" {
            // The closing thread has the files; give it the processor. No timed wait: the render-out path reads no
            // wall clock (`the_render_out_path_reads_no_wall_clock`).
            std::thread::yield_now();
        }
        if !on_progress(&answer) {
            cancel(&mut world, &json!({}))?;
        }
    }
}

/// `render.out_progress { job?, hashes?, hashes_from? }`.
pub(crate) fn progress(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let wanted = crate::params::opt_u64(params, "job")?;
    let Some(jobs) = world.get_resource::<RenderOutJobs>() else {
        return Err(RpcError::not_found(
            "no render-out job",
            "start one with render.out_start",
        ));
    };
    if let Some(job) = jobs
        .active
        .as_ref()
        .filter(|job| wanted.is_none_or(|id| id == job.id))
    {
        let mut answer = running_answer(world, job);
        answer["hashes"] = hash_window(&job.hashes, params)?;
        return Ok(answer);
    }
    let report = jobs
        .finished
        .iter()
        .rev()
        .find(|report| wanted.is_none_or(|id| id == report.id))
        .ok_or_else(|| {
            RpcError::not_found("no such render-out job", "render.out_start returns the job id")
        })?;
    let mut out = json!({
        "job": report.id,
        "status": report.status.name(),
        "frame": report.delivered,
        "frames": report.request.frames,
        "fraction": report.delivered as f64 / report.request.frames as f64,
        "game_time_s": report.request.frame_time(report.delivered).as_secs_f64(),
        "sim_time_s": std::time::Duration::from_nanos(report.sim_ns).as_secs_f64(),
        "replay_from_tick": report.replay_from,
        "sequence_hash": hex(report.sequence),
        "camera_cuts": report.cuts,
        "exposure_metered_cuts": report.metered_cuts,
        "out": report.request.shown(&report.request.out_dir),
        "files": report.files.len(),
        "first_file": report.files.first(),
        "manifest": report.manifest,
        "hashes": hash_window(&report.hashes, params)?,
        "audio": report.audio,
        "takes": report.job.get("takes").cloned(),
    });
    if let Status::Failed(error) = &report.status {
        out["error"] = json!(report.request.scrub(error));
    }
    Ok(out)
}

/// The answer of `render.out_progress` about the running `job`, without `hashes` (the app answers
/// it between frames; the progress board keeps a copy for a poll that comes during a step).
fn running_answer(world: &World, job: &Job) -> Value {
    let cuts = job
        .closing
        .as_ref()
        .map_or_else(|| job_cuts(world, job), |c| c.cuts);
    let frames = job.request.frames;
    let scale_ppm = job.request.time_map.frame_scale_ppm(job.delivered);
    json!({
        "job": job.id,
        "status": if job.cancel { "cancelling" } else { "running" },
        "phase": match job.phase {
            _ if job.closing.is_some() => "closing",
            Phase::Arming => "arming",
            Phase::Warmup(_) => "warmup",
            Phase::Rendering => "rendering",
        },
        "warming": job.renders == 0 && job.closing.is_none(),
        "frame": job.delivered,
        "written": job.sink.as_ref().and_then(Sink::written).unwrap_or(job.delivered),
        "subframe": job.subframe,
        "frames": frames,
        "fraction": job.delivered as f64 / frames as f64,
        "game_time_s": job.request.frame_time(job.delivered).as_secs_f64(),
        "sim_time_s": std::time::Duration::from_nanos(job.sim_ns).as_secs_f64(),
        "sim_step_ns": job.request.sim_step_ns(job.delivered),
        "time_scale": time_map::scale_from_ppm(scale_ppm),
        "replay_from_tick": job.replay_from,
        "sequence_hash": hex(job.sequence),
        "camera_cuts": cuts,
        "exposure_metered_cuts": job.metered_cuts,
        "out": job.request.shown(&job.request.out_dir),
        "hashes": Value::Null,
        "audio": match &job.closing {
            Some(closing) => closing.audio.clone(),
            None => job.audio.as_ref().map(|out| out.progress_json(&job.request)),
        },
        "takes": job.takes.as_ref().map(|run| json!({
            "writing": run.writing(job.delivered),
            "preroll_frames": run.plan.preroll,
            "takes": run.plan.takes.len(),
        })),
        "product": job.product.as_ref().map(product::ProductRun::progress_json),
    })
}

/// `render.out_cancel { job? }`: ends the running job at the next frame; what was delivered stays
/// (a movie is finalized so it plays).
///
/// Idempotent (ADR-1965): a job that is already cancelling answers `cancelling` again; a job that
/// is closing its outputs answers `closing` with the status its frames ended with (`ending`); a
/// finished job answers its final `status` (`done`, `cancelled` or `failed`) with `finished: true`.
/// Without `job` the running job is meant, else the last finished one. `NOT_FOUND` only when no
/// such job exists (an unknown id, or no job at all).
pub(crate) fn cancel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let wanted = crate::params::opt_u64(params, "job")?;
    let Some(jobs) = world.get_resource_mut::<RenderOutJobs>() else {
        return Err(RpcError::not_found(
            "no render-out job",
            "start one with render.out_start",
        ));
    };
    let jobs = jobs.into_inner();
    if let Some(job) = jobs
        .active
        .as_mut()
        .filter(|job| wanted.is_none_or(|w| w == job.id))
    {
        return Ok(match &job.closing {
            Some(closing) => json!({ "job": job.id, "status": "closing", "ending": closing.ending }),
            None => {
                job.cancel = true;
                json!({ "job": job.id, "status": "cancelling" })
            }
        });
    }
    jobs.finished
        .iter()
        .rev()
        .find(|report| wanted.is_none_or(|id| id == report.id))
        .map(|report| json!({ "job": report.id, "status": report.status.name(), "finished": true }))
        .ok_or_else(|| RpcError::not_found("no such render-out job", "render.out_start returns the job id"))
}

enum Step {
    Continue,
    Finish(Status),
}

/// Whether a job holds the app's clock (`play.pause`, `play.resume`, `play.step` and
/// `play.set_time_scale` are refused with `F0018` meanwhile).
pub(crate) fn job_running(world: &World) -> bool {
    world
        .get_resource::<RenderOutJobs>()
        .is_some_and(|jobs| jobs.active.is_some())
}

/// Requests that may cancel a render-out (`render.out_cancel`, `agent.job_cancel`) queued so far
/// by the agent's connection threads.
static CANCEL_NOTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Called by a connection thread after it queued a request that may cancel a render-out: an app
/// thread waiting on a full, stalled output stops waiting, keeps the frame it made and lets the
/// request be handled (ADR-2396). It decides nothing: the request goes through dispatch and its
/// permission checks as usual, and a job it does not cancel hands the frame over next frame.
pub(crate) fn note_cancel_request() {
    CANCEL_NOTES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// How many cancel requests have been noted so far ([`note_cancel_request`]).
pub(crate) fn cancel_notes() -> u64 {
    CANCEL_NOTES.load(std::sync::atomic::Ordering::SeqCst)
}

/// `Last`: advances the running job by one app frame.
pub fn advance_render_out(world: &mut World) {
    let Some(mut job) = world
        .get_resource_mut::<RenderOutJobs>()
        .and_then(|mut jobs| jobs.active.take())
    else {
        return;
    };
    let _span = tracing::debug_span!(target: "regia_agent", "render_out_frame").entered();
    if job
        .closing
        .as_ref()
        .is_some_and(|closing| !closing.thread.is_finished())
    {
        world.resource_mut::<RenderOutJobs>().active = Some(job);
        return;
    }
    if let Some(closing) = job.closing.take() {
        let cuts = closing.cuts;
        let closed = closing
            .thread
            .join()
            .unwrap_or_else(|_| Closed::nothing(Status::Failed("the closing thread panicked".into())));
        finish(world, job, cuts, closed);
        return;
    }
    // Start set these once; a game or a frame budget that writes them mid-job would freeze or speed
    // up the frames while the manifest and the audio keep the request's time.
    if let Some(mut time) = world.get_resource_mut::<Time>() {
        time.paused = false;
        time.time_scale = 1.0;
    }
    // A render-out frame is a capture that must be complete (ADR-1965): with pipeline-warm workers
    // running (any windowed host, and a headless one since its first job's warm) the frame thread
    // would otherwise skip a deferred pipeline variant that is still building and draw without it,
    // so the same request could render differently from run to run.
    let complete = regia_render::pipeline_warm::blocking_scope();
    let busy = publish_step(world, &job);
    let rendering_phase = job.phase != Phase::Arming && job.pending.is_none();
    let stepped = step(world, &mut job);
    drop(busy);
    drop(complete);
    if rendering_phase {
        job.renders += 1;
    }
    // A cancel request noted from now on may arrive after the next frame's dispatch ran; one
    // noted before was or will be dispatched first (a spurious end of a wait only holds a frame).
    job.notes_seen = cancel_notes();
    match stepped {
        Step::Continue => world.resource_mut::<RenderOutJobs>().active = Some(job),
        Step::Finish(status) => begin_close(world, job, status),
    }
}

/// Puts what a poll would get now on the server's progress board for the length of the step that
/// follows (ADR-2582): a step can hold the app thread for many seconds (a cold host's first render
/// compiles pipelines), and a connection answers a poll from the board meanwhile. The guard clears
/// the board when the step ends, however it ends.
fn publish_step(world: &World, job: &Job) -> board::BusyGuard {
    let Some(board) = world.get_resource::<RenderOutBoard>().cloned() else {
        return board::BusyGuard(None);
    };
    // As dispatch: a world without a permission profile uses the default one.
    let read = crate::schema::Permission::Read;
    let read_allowed = world
        .get_resource::<crate::safety::AgentPermissions>()
        .map_or_else(
            || crate::safety::AgentPermissions::default().allows(read),
            |p| p.allows(read),
        );
    board.enter(job.id, running_answer(world, job), &job.hashes, read_allowed);
    board::BusyGuard(Some(board))
}

fn step(world: &mut World, job: &mut Job) -> Step {
    if job.cancel {
        return Step::Finish(Status::Cancelled);
    }
    match job.phase {
        Phase::Arming => {
            if let Err(message) = audio_pre_roll(world, job) {
                return Step::Finish(Status::Failed(message));
            }
            job.phase = if job.request.warmup > 0 {
                Phase::Warmup(job.request.warmup)
            } else {
                enter_rendering(world, job);
                Phase::Rendering
            };
            Step::Continue
        }
        Phase::Warmup(left) => {
            let rendered = if job.request.is_settle(left) {
                // The job's opening cut moves from frame 0 to the first settle frame.
                let cut = if left == job.request.settle {
                    job_cut(world, job)
                } else {
                    Ok(())
                };
                cut.and_then(|()| render_settle(world, job))
            } else {
                render_warmup(world, job)
            };
            if let Err(error) = rendered {
                return Step::Finish(Status::Failed(error.message));
            }
            if let Err(message) = audio_pre_roll(world, job) {
                return Step::Finish(Status::Failed(message));
            }
            job.phase = if left <= 1 {
                enter_rendering(world, job);
                Phase::Rendering
            } else {
                Phase::Warmup(left - 1)
            };
            Step::Continue
        }
        Phase::Rendering => match job.pending.take() {
            Some(pending) => match hand_over_frame(world, job, pending) {
                Ok(true) => Step::Finish(Status::Done),
                Ok(false) => Step::Continue,
                Err(message) => Step::Finish(Status::Failed(message)),
            },
            None => match render_frame(world, job) {
                Ok(true) => Step::Finish(Status::Done),
                Ok(false) => Step::Continue,
                Err(message) => Step::Finish(Status::Failed(message)),
            },
        },
    }
}

/// Hands made frame `pending` to the job's output and books it. A cancel request queued while the
/// output is full interrupts the wait (ADR-2396): the frame is kept, the clocks are held still for
/// the next app frame (whose agent requests are handled), and `Ok(false)` says the job goes on.
fn hand_over_frame(world: &mut World, job: &mut Job, pending: Pending) -> Result<bool, String> {
    let seen = job.notes_seen;
    let interrupted = move || cancel_notes() != seen;
    let Pending {
        index,
        frame,
        aovs,
        hash,
    } = pending;
    let back = match job.takes.as_mut() {
        Some(run) => run.hand_over(index, frame, aovs, &interrupted)?,
        None => {
            job.sink
                .as_mut()
                .ok_or("the output is closed")?
                .hand_over(index, frame, aovs, &interrupted)?
        }
    };
    if let Some((frame, aovs)) = back {
        hold_clocks(world, job);
        job.pending = Some(Pending {
            index,
            frame,
            aovs,
            hash,
        });
        return Ok(false);
    }
    release_clocks(world, job);
    let request = job.request.clone();
    commit_frame(world, job, &request, index, hash)
}

/// Sets both clocks' steps to zero for the next app frame, keeping the steps they had.
fn hold_clocks(world: &mut World, job: &mut Job) {
    if job.held.is_none() {
        job.held = Some((
            world.get_resource::<TimeUpdateStrategy>().copied(),
            world.get_resource::<SimStep>().copied(),
        ));
    }
    set_clock_step(world, 0);
    if world.get_resource::<SimStep>().is_some() {
        set_sim_step(world, 0);
    }
}

/// Puts back the clock steps [`hold_clocks`] kept.
fn release_clocks(world: &mut World, job: &mut Job) {
    let Some((strategy, sim)) = job.held.take() else {
        return;
    };
    match strategy {
        Some(strategy) => world.insert_resource(strategy),
        None => {
            world.remove_resource::<TimeUpdateStrategy>();
        }
    }
    match sim {
        Some(sim) => world.insert_resource(sim),
        None => {
            world.remove_resource::<SimStep>();
        }
    }
}

/// An arming or warm-up frame's audio: one pre-roll block, rendered and dropped, so the mixer's
/// clock reaches frame 0 with the job's (ADR-1520).
fn audio_pre_roll(world: &mut World, job: &mut Job) -> Result<(), String> {
    match job.audio.as_mut() {
        Some(out) => out.pre_roll(world, &job.request),
        None => Ok(()),
    }
}

/// Sets the render clock's step for the next app frame (the shutter timeline).
fn set_clock_step(world: &mut World, nanos: u64) {
    world.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_nanos(nanos),
    ));
}

/// Sets the simulation clock's step for the next app frame (a time-mapped job only, ADR-1524).
fn set_sim_step(world: &mut World, nanos: u64) {
    world.insert_resource(SimStep(std::time::Duration::from_nanos(nanos)));
}

/// Before frame 0. A shutter job's first subframe is `lead` past frame 0's own time, so each
/// clock's next step is its step into frame 0 plus its lead (the render clock's of one output step,
/// the simulation clock's of frame 0's simulation step); without the shutter the clocks are
/// untouched. A replay armed for the render-out starts now at its stated tick, holding the
/// simulation step into frame 0 so frame 0 shows that tick (`replay.play`, ADR-1519).
fn enter_rendering(world: &mut World, job: &mut Job) {
    let request = &job.request;
    let pre = request.pre_sim_step_ns();
    if request.shutter.is_on() {
        let step = request.step_ns();
        set_clock_step(world, step + request.shutter.lead_ns(step));
        if !request.time_map.is_identity() {
            set_sim_step(world, pre + request.shutter.lead_ns(request.sim_step_ns(0)));
        }
    }
    job.replay_from = crate::replay::start_armed_replay(world, std::time::Duration::from_nanos(pre));
    let step = job.request.step_ns();
    if let Some(run) = job.takes.as_mut() {
        takes::set_origin(world, run, step);
        takes::release_sequence_audio(world);
    }
}

/// After frame `index` is delivered: the simulation clock's step to the next frame is this frame's
/// simulation step. The render clock keeps its constant step, and without a time map so does the
/// simulation (the job sets no simulation step).
fn step_to_next_frame(world: &mut World, job: &mut Job, index: u64) {
    let step = job.request.sim_step_ns(index);
    job.sim_ns = job.sim_ns.saturating_add(step);
    if !job.request.time_map.is_identity() && !job.request.shutter.is_on() {
        set_sim_step(world, step);
    }
}

/// The job's camera cuts so far: the renderer's cuts since the job started, a metered cut counted
/// once (its two renderer cuts start one shot).
fn job_cuts(world: &World, job: &Job) -> u64 {
    world
        .get_resource::<RuntimeRenderer>()
        .map_or(0, |r| r.0.camera_cut_count().saturating_sub(job.cuts_before))
        .saturating_sub(job.metered_cuts)
}

/// Whether the view a job renders runs auto exposure. A camera EV, a profile without auto exposure
/// or a disabled post stack holds a fixed exposure, so its cuts need no metering.
fn meters_exposure(world: &World) -> bool {
    world
        .get_resource::<regia_render::runtime::ExtractedFrame>()
        .is_some_and(|frame| {
            let index = frame.window_views().first().map_or(0, |(index, _)| *index);
            frame
                .views
                .get(index)
                .is_some_and(|view| view.post.enabled && view.post.auto_exposure)
        })
}

/// One of the job's camera cuts (frame 0 or a `cuts` frame, ADR-267; the first settle frame,
/// ADR-1449), exposed by the cut frame's own meter (ADR-1455).
///
/// Every temporal history restarts as in a fresh renderer. With auto exposure on, the meter
/// restarts too and reads this game frame in `EXPOSURE_READBACK_FRAMES` undelivered renders at
/// the delivered size (the clock does not step and the particles do not move), and a second cut
/// with `CutExposure::Fade` restarts every history again but keeps the reading. Before, the first
/// frames after a cut printed at the meter's initial EV (the top of the auto-exposure band, a
/// sunny day): one-frame stills of the showcase's hazy morning came out about 1.3 EV darker than
/// settled ones.
fn job_cut(world: &mut World, job: &mut Job) -> Result<(), RpcError> {
    world.resource_mut::<RuntimeRenderer>().0.camera_cut();
    if !meters_exposure(world) {
        return Ok(());
    }
    let _span = tracing::debug_span!(target: "regia_agent", "render_out_meter_cut").entered();
    let request = &job.request;
    let spec =
        RenderOutSpec::new(request.width, request.height, request.layout).supersampled(request.supersample);
    for _ in 0..regia_render::FrameRenderer::EXPOSURE_READBACK_FRAMES {
        with_still_vfx(world, |world| {
            crate::seeing::offscreen_pixels(world, spec.width, spec.height, false, false)
        })?;
    }
    world
        .resource_mut::<RuntimeRenderer>()
        .0
        .camera_cut_with(regia_render::CutExposure::Fade);
    job.metered_cuts += 1;
    Ok(())
}

/// Runs `render` with the particles held: a meter render draws the game frame again without
/// stepping its simulation.
fn with_still_vfx<R>(world: &mut World, render: impl FnOnce(&mut World) -> R) -> R {
    let handle = world.get_resource::<regia_vfx::runtime::VfxGpuHandle>().cloned();
    let saved = handle
        .as_ref()
        .map(regia_vfx::runtime::VfxGpuHandle::step_seconds);
    if let Some(handle) = &handle {
        handle.set_step_seconds(0.0);
    }
    let result = render(world);
    if let (Some(handle), Some(dt)) = (&handle, saved) {
        handle.set_step_seconds(dt);
    }
    result
}

fn render_warmup(world: &mut World, job: &Job) -> Result<(), RpcError> {
    let (w, h) = warmup_size(job.request.width, job.request.height);
    crate::seeing::offscreen_pixels(world, w, h, false, false).map(|_| ())
}

/// A settle frame (ADR-1449): rendered exactly as a delivered frame of the job is (its size and
/// supersampling; the history of temporal AA, hashed alpha and fog is kept only at one size) and
/// not delivered. A warm-up frame renders small, so the first full-size frame had no history:
/// a one-frame still was a single jittered sample, its alpha-hashed foliage a raw stipple.
fn render_settle(world: &mut World, job: &Job) -> Result<(), RpcError> {
    let request = &job.request;
    let spec =
        RenderOutSpec::new(request.width, request.height, request.layout).supersampled(request.supersample);
    crate::seeing::offscreen_pixels(world, spec.width, spec.height, false, false).map(|_| ())
}

/// Renders and delivers one frame (one subframe of a shutter job); `Ok(true)` when it was the
/// last.
fn render_frame(world: &mut World, job: &mut Job) -> Result<bool, String> {
    let index = job.delivered;
    let request = job.request.clone();
    if job.product.is_some() {
        return render_product_frame(world, job, &request);
    }
    if job.subframe == 0 && request.is_cut(index) {
        job_cut(world, job).map_err(|e| e.message)?;
    }
    if request.format == plan::Format::Exr {
        return render_scene_linear(world, job, &request);
    }
    if request.accumulates() {
        return render_accumulated(world, job, &request);
    }
    let spec =
        RenderOutSpec::new(request.width, request.height, request.layout).supersampled(request.supersample);
    if let Some(hud) = &job.hud {
        hud.begin(world, &spec);
    }
    let timer = timing::FrameTimer::start(request.timings);
    let rendered = crate::seeing::offscreen_pixels(world, spec.width, spec.height, false, job.hud.is_some());
    if let Some(ms) = timer.stop_ms() {
        job.render_ms.push(ms);
    }
    if let Some(hud) = &job.hud {
        hud.end(world, &spec);
    }
    let mut rgba = rendered.map_err(|e| e.message)?;
    if request.supersample > 1 {
        rgba = downsample(&rgba, request.width, request.height, request.supersample);
    }
    if !request.neutral
        && let Some(calibration) = world.get_resource::<DisplayCalibration>()
    {
        calibration.apply_sdr_rgba8(&mut rgba);
    }
    let hash = fnv1a(&rgba);
    record_fade(world, &mut job.fades, index);
    job.aov.sample(world, &request, index)?;
    let aovs = job.aov.take();
    hand_over_frame(
        world,
        job,
        Pending {
            index,
            frame: FrameData::Rgba8(rgba),
            aovs,
            hash,
        },
    )
}

/// One frame of a product job (ADR-2757): the player draws it, and from there it is a frame like any other.
fn render_product_frame(world: &mut World, job: &mut Job, request: &Request) -> Result<bool, String> {
    let index = job.delivered;
    let run = job.product.as_mut().ok_or("the job has no product")?;
    let dt_ms = request.step().as_secs_f64() * 1000.0;
    let timer = timing::FrameTimer::start(request.timings);
    let drawn = product::draw_frame(world, run, index, dt_ms);
    if let Some(ms) = timer.stop_ms() {
        job.render_ms.push(ms);
    }
    let mut rgba = drawn?;
    if request.supersample > 1 {
        rgba = downsample(&rgba, request.width, request.height, request.supersample);
    }
    if !request.neutral
        && let Some(calibration) = world.get_resource::<DisplayCalibration>()
    {
        calibration.apply_sdr_rgba8(&mut rgba);
    }
    let hash = fnv1a(&rgba);
    hand_over_frame(
        world,
        job,
        Pending {
            index,
            frame: FrameData::Rgba8(rgba),
            aovs: Vec::new(),
            hash,
        },
    )
}

/// Renders one subframe of a shutter or 16-bit job into the frame's sums, steps the clock to the
/// next subframe, and after the last one resolves, hashes and delivers the frame. `Ok(true)` when
/// that was the last frame.
fn render_accumulated(world: &mut World, job: &mut Job, request: &Request) -> Result<bool, String> {
    let index = job.delivered;
    let k = job.subframe;
    let (width, height) = (request.width, request.height);
    let acc = job
        .acc
        .get_or_insert_with(|| Accumulator::new(width, height, request.supersample, request.bit_depth));
    match request.bit_depth {
        Depth::Eight => {
            let spec = RenderOutSpec::new(width, height, request.layout).supersampled(request.supersample);
            if let Some(hud) = &job.hud {
                hud.begin(world, &spec);
            }
            let rendered =
                crate::seeing::offscreen_pixels(world, spec.width, spec.height, false, job.hud.is_some());
            if let Some(hud) = &job.hud {
                hud.end(world, &spec);
            }
            acc.add_rgba8(&rendered.map_err(|e| e.message)?)?;
        }
        Depth::Sixteen => {
            let target = &mut job.deep_target;
            let rendered =
                deep::offscreen_pixels_deep(world, request.render_width(), request.render_height(), target);
            acc.add_rgba16(&rendered.map_err(|e| e.message)?)?;
        }
    }
    if k == aov::sample_subframe(request.shutter.subframes) {
        record_fade(world, &mut job.fades, index);
        job.aov.sample(world, request, index)?;
    }
    if request.shutter.is_on() {
        set_clock_step(world, request.shutter.delay_after(request.step_ns(), k));
        if !request.time_map.is_identity() {
            let step = request.sim_step_ns(index);
            let next = request.sim_step_ns(index + 1);
            set_sim_step(world, request.shutter.delay_between(step, next, k));
        }
    }
    if k + 1 < request.shutter.subframes {
        job.subframe = k + 1;
        return Ok(false);
    }
    job.subframe = 0;
    let (hash, frame) = match request.bit_depth {
        Depth::Eight => {
            let mut rgba = acc.resolve_rgba8()?;
            if !request.neutral
                && let Some(calibration) = world.get_resource::<DisplayCalibration>()
            {
                calibration.apply_sdr_rgba8(&mut rgba);
            }
            (fnv1a(&rgba), FrameData::Rgba8(rgba))
        }
        Depth::Sixteen => {
            let rgba = acc.resolve_rgba16()?;
            (fnv1a(&accumulate::le_bytes(&rgba)), FrameData::Rgba16(rgba))
        }
    };
    acc.clear();
    let aovs = job.aov.take();
    hand_over_frame(
        world,
        job,
        Pending {
            index,
            frame,
            aovs,
            hash,
        },
    )
}

/// Renders one subframe of a `format: "exr"` job: the frame as usual, plus a copy of the post
/// stack's scene-linear input (`FrameRenderer::request_hdr_capture`), resampled to the delivered
/// size and summed; after the last subframe the average is hashed and delivered. `Ok(true)` when
/// that was the last frame.
fn render_scene_linear(world: &mut World, job: &mut Job, request: &Request) -> Result<bool, String> {
    let index = job.delivered;
    let k = job.subframe;
    let (width, height) = (request.render_width(), request.render_height());
    world.resource_mut::<RuntimeRenderer>().0.request_hdr_capture();
    crate::seeing::offscreen_pixels(world, width, height, false, false).map_err(|e| e.message)?;
    let capture = world.resource_scope(|world, mut runtime: Mut<RuntimeRenderer>| {
        let device = world
            .get_resource::<regia_render::RenderDevice>()
            .ok_or("no render device")?;
        runtime.0.take_hdr_capture(device).map_err(|e| e.to_string())
    })?;
    let capture = capture.ok_or(
        "format \"exr\" needs the scene-linear frame of the post stack, and this view renders without one (post processing off); use format \"png\"",
    )?;
    let rgb = exr_file::resample_rgb(
        &capture.rgb,
        capture.width,
        capture.height,
        request.width,
        request.height,
    );
    match job.hdr_sum.as_mut() {
        Some(sum) if sum.len() == rgb.len() => sum.iter_mut().zip(&rgb).for_each(|(s, v)| *s += v),
        _ => job.hdr_sum = Some(rgb),
    }
    if k == aov::sample_subframe(request.shutter.subframes) {
        record_fade(world, &mut job.fades, index);
        job.aov.sample(world, request, index)?;
    }
    if request.shutter.is_on() {
        set_clock_step(world, request.shutter.delay_after(request.step_ns(), k));
        if !request.time_map.is_identity() {
            let step = request.sim_step_ns(index);
            let next = request.sim_step_ns(index + 1);
            set_sim_step(world, request.shutter.delay_between(step, next, k));
        }
    }
    if k + 1 < request.shutter.subframes {
        job.subframe = k + 1;
        return Ok(false);
    }
    job.subframe = 0;
    let mut rgb = job.hdr_sum.take().ok_or("no scene-linear frame was summed")?;
    if request.shutter.subframes > 1 {
        let inv = 1.0 / request.shutter.subframes as f32;
        rgb.iter_mut().for_each(|v| *v *= inv);
    }
    let bytes: Vec<u8> = rgb.iter().flat_map(|v| v.to_le_bytes()).collect();
    let hash = fnv1a(&bytes);
    let aovs = job.aov.take();
    hand_over_frame(
        world,
        job,
        Pending {
            index,
            frame: FrameData::RgbF32(rgb),
            aovs,
            hash,
        },
    )
}

/// Books delivered frame `index`: its hash, its audio block and the step to the next frame. Every
/// frame path ends here, so the sound keeps up with the frames in every format. `Ok(true)` when
/// that was the last frame.
fn commit_frame(
    world: &mut World,
    job: &mut Job,
    request: &Request,
    index: u64,
    hash: u64,
) -> Result<bool, String> {
    job.hashes.push(hash);
    job.sequence = fold_hash(job.sequence, hash);
    // A plate's sidecar records the camera of the frame it delivered (ADR-5895).
    crate::plates::frame_delivered(world, job.id, index);
    job.delivered += 1;
    if let Some(out) = job.audio.as_mut() {
        out.frame(world, request, index)?;
    }
    step_to_next_frame(world, job, index);
    Ok(job.delivered >= request.frames)
}

/// The layout of a `raw` frame file for an external encoder, or `null` for the other formats.
fn raw_layout(request: &Request) -> Value {
    if request.format != plan::Format::Raw {
        return Value::Null;
    }
    let bytes_per_channel = u64::from(request.bit_depth.bits() / 8);
    let row_stride = u64::from(request.width) * 4 * bytes_per_channel;
    json!({
        "pixel_format": if bytes_per_channel == 1 { "rgba8" } else { "rgba16" },
        "width": request.width,
        "height": request.height,
        "channels": 4,
        "channel_order": "rgba",
        "bit_depth": request.bit_depth.bits(),
        "byte_order": if bytes_per_channel == 1 { "n/a" } else { "little" },
        "row_stride_bytes": row_stride,
        "frame_bytes": row_stride * u64::from(request.height),
        "alpha": "straight, always 255",
        "color": "sRGB, display-referred (the delivered frame)",
        "file_pattern": format!("{}_NNNNNN.raw", request.name),
    })
}

/// The manifest's schema id. Version 2 (ADR-1524): `files` is `{ frames, audio, aov }` (version 1
/// had the frame list there; `audio` lists the WAVs and `aov` each AOV's files, ADR-1554), and
/// `frame_times` is the per-frame time table.
pub const MANIFEST_SCHEMA: &str = "regia.render_out/2";

/// The schema of a product job's manifest (`interfaces.md` § 8): `regia.render_out/2` plus a `product` block.
pub const PRODUCT_MANIFEST_SCHEMA: &str = "jewelry.render_out/1";

/// The manifest's per-frame time table for `delivered` frames, and the simulation time after the
/// last: each frame's render-clock time (`game_time_*`, the output's), simulation time
/// (`sim_time_*`, the time map's running sum), simulation step and scale.
fn frame_time_table(request: &Request, delivered: u64) -> (Vec<Value>, u64) {
    let mut table = Vec::with_capacity(usize::try_from(delivered).unwrap_or(0));
    let mut sim_ns = 0_u64;
    for index in 0..delivered {
        let game = request.frame_time(index);
        let step = request.sim_step_ns(index);
        table.push(json!({
            "frame": index,
            "game_time_ns": game.as_nanos() as u64,
            "game_time_s": game.as_secs_f64(),
            "sim_time_ns": sim_ns,
            "sim_time_s": std::time::Duration::from_nanos(sim_ns).as_secs_f64(),
            "sim_step_ns": step,
            "time_scale": time_map::scale_from_ppm(request.time_map.frame_scale_ppm(index)),
        }));
        sim_ns = sim_ns.saturating_add(step);
    }
    (table, sim_ns)
}

/// Notes the shot fade the compositor draws on delivered frame `index`, if one is visible (the
/// manifest's `fade` block). The AOV pass never draws it.
fn record_fade(world: &World, fades: &mut Vec<(u64, regia_render::FadeState)>, index: u64) {
    let Some(layer) = world.get_resource::<regia_framework::shots::ShotFadeLayer>() else {
        return;
    };
    let state = layer.0.get();
    if state.is_visible() && fades.last().is_none_or(|(frame, _)| *frame != index) {
        fades.push((index, state));
    }
}

/// The manifest's `fade` block (ADR-1554): the frames a shot fade was visible on, and whether the
/// frame files carry it. The compositor draws the fade after the UI on 8-bit display frames; a
/// scene-linear (`exr`) or 16-bit frame is taken before the compositor and carries none, and the
/// AOVs never do (their own pass), so a tool can apply `amount` and `color` itself.
fn fade_json(request: &Request, fades: &[(u64, regia_render::FadeState)]) -> Value {
    let composited = request.format != plan::Format::Exr && request.bit_depth == Depth::Eight;
    json!({
        "in_frames": composited,
        "in_aovs": false,
        "frames": fades.iter().map(|(frame, state)| json!({
            "frame": frame,
            "amount": state.amount.clamp(0.0, 1.0),
            "color": state.color,
        })).collect::<Vec<_>>(),
    })
}

/// The manifest's `files.aov` (ADR-1554): per AOV in request order, its files of the delivered
/// frames, frame `i` at index `i` (the same index as `files.frames` and `frame_times`).
fn aov_files(request: &Request, delivered: u64) -> Value {
    request
        .aovs
        .list
        .iter()
        .map(|aov| {
            let files: Vec<String> = (0..delivered)
                .map(|i| request.shown(&request.aovs.path(&request.out_dir, &request.name, *aov, i)))
                .collect();
            json!({ "aov": aov.name(), "files": files })
        })
        .collect()
}

/// Writes a job's manifest by renaming a finished file over it: a reader never sees half of it, and
/// a re-render that dies here leaves the previous complete manifest instead of a truncated one.
fn write_manifest(path: &std::path::Path, manifest: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    crate::bake_export::replace_file(path, &bytes).map_err(|e| e.to_string())
}

/// A job ended: hands the clocks and the shot player back and ends the mixdown here, on the app
/// thread, and leaves its files to a closing thread ([`close_outputs`]), so a slow disk stalls
/// nothing. The job stays the active one until [`finish`], so a new job is refused meanwhile.
fn begin_close(world: &mut World, mut job: Job, status: Status) {
    let audio_shown = job.audio.as_ref().map(|out| out.progress_json(&job.request));
    let audio = job.audio.take().map(|out| out.end(world));
    if job.product.is_some() {
        product::end(world);
    }
    let mut take_run = job.takes.take();
    if let Some(run) = take_run.as_mut() {
        takes::end_shots(world, run);
        takes::end_job_player(world, run);
        takes::release_sequence_audio(world);
    }
    world.insert_resource(job.saved.strategy);
    {
        let mut time = world.resource_mut::<Time>();
        time.paused = job.saved.paused;
        time.time_scale = job.saved.time_scale;
        let (game, sim) = job.saved.clocks;
        let (game, sim) = (
            game.saturating_add(time.elapsed()),
            sim.saturating_add(time.sim_elapsed()),
        );
        time.set_clocks(game, sim);
    }
    // Before the weather clock resumes: the capture holds the weather as the job's restart left it.
    if let Some(effects) = job.saved.effects.take() {
        effects.restore(world);
    }
    if let Some(shakes) = job.saved.shakes.take() {
        shakes.restore(world);
    }
    if let Some(start) = job.saved.effects_start.take() {
        start.finish(world);
    }
    if let (Some(before), Some(mut weather)) =
        (job.saved.weather, world.get_resource_mut::<regia_vfx::Weather>())
    {
        weather.resume_clock(before);
    }
    if let Some(before) = job.saved.max_substeps
        && let Some(mut fixed) = world.get_resource_mut::<FixedTime>()
    {
        fixed.max_substeps = before;
    }
    if !job.request.time_map.is_identity() {
        match job.saved.sim_step {
            Some(step) => world.insert_resource(step),
            None => {
                world.remove_resource::<SimStep>();
            }
        }
    }
    let cuts = job_cuts(world, &job);
    if let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() {
        // Complete frames first: it restores the exposure wait it forced, then the job's own
        // saved wait value wins (the job may have run while the host already held the switch).
        renderer.0.set_complete_frames(job.saved.complete_frames);
        renderer.0.set_wait_for_exposure_readback(job.saved.wait_readback);
    }
    let ending = status.name();
    let work = CloseWork {
        request: job.request.clone(),
        status,
        delivered: job.delivered,
        sink: job.sink.take(),
        audio,
        takes: take_run,
        sync: world
            .get_resource::<RenderOutFileSync>()
            .cloned()
            .unwrap_or_default(),
    };
    let spawned = std::thread::Builder::new()
        .name(format!("render-out-close-{}", job.id))
        .spawn(move || close_outputs(work));
    match spawned {
        Ok(thread) => {
            job.closing = Some(Closing {
                thread,
                cuts,
                audio: audio_shown,
                ending,
            });
            world.get_resource_or_insert_with(RenderOutJobs::default).active = Some(job);
        }
        Err(error) => {
            let error = format!("cannot start the closing thread: {error}");
            finish(world, job, cuts, Closed::nothing(Status::Failed(error)));
        }
    }
}

/// The closing thread: waits for the frame writers or `ffmpeg`, then closes the WAVs and the takes
/// over exactly the frames that reached the disk. Everything here may block on the disk.
fn close_outputs(work: CloseWork) -> Closed {
    let _span = tracing::debug_span!(target: "regia_agent", "render_out_close").entered();
    let CloseWork {
        request,
        mut status,
        mut delivered,
        sink,
        audio,
        mut takes,
        sync,
    } = work;
    let mut files = Vec::new();
    let mut on_disk = None;
    // A `webm` sink takes the mixdown into its movie: the WAV closes first (over every delivered
    // frame; a movie sink never drops frames), then the sink muxes it (ADR-3369).
    let mut audio = audio;
    let mut audio_closed = None;
    let mut wav = None;
    if let Some(out) = audio.take_if(|_| sink.as_ref().is_some_and(Sink::takes_audio)) {
        let (block, error) = out.close(&request, delivered, &sync);
        wav = Some(audio::mix_path(&request)).filter(|path| error.is_none() && path.is_file());
        audio_closed = Some((block, error));
    }
    if let Some(finished) = sink.map(|sink| sink.finish_with_audio(wav.as_deref())) {
        files = finished.files;
        if let Some(error) = finished.error
            && !matches!(status, Status::Failed(_))
        {
            status = Status::Failed(request.scrub(&error));
        }
        // Frames handed to the writers but not on disk (a write failed) are not delivered.
        on_disk = finished.frames;
        delivered = on_disk.map_or(delivered, |frames| frames.min(delivered));
    }
    // The WAVs keep exactly the delivered frames that are on disk.
    let audio = match (audio, audio_closed) {
        (_, Some((block, error))) => {
            if let Some(error) = error
                && !matches!(status, Status::Failed(_))
            {
                status = Status::Failed(error);
            }
            block
        }
        (Some(out), None) => {
            let (block, error) = out.close(&request, delivered, &sync);
            if let Some(error) = error
                && !matches!(status, Status::Failed(_))
            {
                status = Status::Failed(error);
            }
            block
        }
        (None, None) => Value::Null,
    };
    // Each take keeps exactly its frames on disk.
    let take_results = takes
        .as_mut()
        .map(|run| run.finish(delivered, status.name()))
        .unwrap_or_default();
    if let Some(error) = take_results.iter().find_map(|r| r.error.clone())
        && !matches!(status, Status::Failed(_))
    {
        status = Status::Failed(request.scrub(&error));
    }
    Closed {
        status,
        files,
        on_disk,
        audio,
        takes,
        take_results,
    }
}

/// The outputs are closed: keeps exactly the unbroken run of frames from 0, writes the manifest and
/// files the report.
fn finish(world: &mut World, mut job: Job, cuts: u64, closed: Closed) {
    let Closed {
        status,
        files,
        on_disk,
        audio: audio_block,
        takes: take_run,
        take_results,
    } = closed;
    if let Some(on_disk) = on_disk
        && on_disk < job.delivered
    {
        job.delivered = on_disk;
        job.hashes.truncate(on_disk as usize);
        job.render_ms.truncate(on_disk as usize);
        job.aov.truncate(on_disk);
        job.fades.retain(|(frame, _)| *frame < on_disk);
        job.sequence = job
            .hashes
            .iter()
            .fold(0, |sequence, hash| fold_hash(sequence, *hash));
    }
    let job_block = take_run.as_ref().map_or(Value::Null, |run| {
        takes::job_json(run, &take_results, &job.request, &job.hashes, job.delivered)
    });
    let (frame_times, sim_ns) = frame_time_table(&job.request, job.delivered);
    let files: Vec<String> = files.iter().map(|p| job.request.shown(p)).collect();
    let manifest_path = job
        .request
        .out_dir
        .join(format!("{}_render_out.json", job.request.name));
    let mut manifest = json!({
        "schema": if job.product.is_some() { PRODUCT_MANIFEST_SCHEMA } else { MANIFEST_SCHEMA },
        "engine": env!("CARGO_PKG_VERSION"),
        "status": status.name(),
        "error": match &status { Status::Failed(e) => Some(e.clone()), _ => None },
        "request": request_json(&job.request),
        "delivered": job.delivered,
        "camera_cuts": cuts,
        "exposure_metered_cuts": job.metered_cuts,
        "sequence_hash": hex(job.sequence),
        "game_time_s": job.request.frame_time(job.delivered).as_secs_f64(),
        "sim_time_s": std::time::Duration::from_nanos(sim_ns).as_secs_f64(),
        "frame_times": frame_times,
        "render_ms": job.request.timings.then(|| job.render_ms.clone()),
        "clock": job.start_clock.to_json(),
        "replay_from_tick": job.replay_from,
        "frame_hashes": job.hashes.iter().map(|h| hex(*h)).collect::<Vec<_>>(),
        "files": {
            "frames": files,
            "audio": audio::block_files(&audio_block),
            "aov": aov_files(&job.request, job.delivered),
        },
        "raw": raw_layout(&job.request),
        "aov": job.aov.manifest(&job.request, job.delivered),
        "color": if job.request.format == plan::Format::Exr {
            json!({ "space": "scene-linear Rec. 709 primaries", "unit": "cd/m^2 (before exposure, bloom and the tonemap)", "channels": exr_file::RGB_CHANNELS })
        } else {
            Value::Null
        },
        "ffmpeg_args": job.request.format.needs_ffmpeg().then(|| sink::ffmpeg_args(&job.request)),
        "audio": audio_block,
        "fade": fade_json(&job.request, &job.fades),
        "job": job_block,
    });
    if let Some(run) = &job.product {
        manifest["extends"] = json!(MANIFEST_SCHEMA);
        manifest["product"] = run.manifest_json(job.delivered);
    }
    let manifest_written = write_manifest(&manifest_path, &manifest);
    if let Err(error) = &manifest_written {
        tracing::warn!(target: "regia_agent", "render-out manifest {}: {error}", manifest_path.display());
    }
    tracing::info!(
        target: "regia_agent",
        job = job.id,
        status = status.name(),
        frames = job.delivered,
        "render-out finished"
    );
    let manifest = manifest_written
        .is_ok()
        .then(|| job.request.shown(&manifest_path));
    let report = Report {
        id: job.id,
        status,
        request: job.request,
        delivered: job.delivered,
        hashes: job.hashes,
        sequence: job.sequence,
        files,
        cuts,
        metered_cuts: job.metered_cuts,
        manifest,
        sim_ns,
        replay_from: job.replay_from,
        audio: audio_block,
        job: job_block,
    };
    let mut jobs = world.get_resource_or_insert_with(RenderOutJobs::default);
    jobs.finished.push(report);
    if jobs.finished.len() > KEEP_FINISHED {
        jobs.finished.remove(0);
    }
}

#[cfg(test)]
mod manifest_tests {
    /// Renderout audit M5: the manifest was truncated in place, so a re-render that died there lost
    /// the previous complete file. A hard link to the old file proves it was replaced, not rewritten.
    #[test]
    fn a_manifest_replaces_the_old_file_instead_of_rewriting_it() {
        let dir = regia_test_utils::unique_temp_dir("render_out_manifest");
        let path = dir.join("take_render_out.json");
        let old = dir.join("previous.json");
        std::fs::write(&path, "{\"old\":true}").expect("old");
        std::fs::hard_link(&path, &old).expect("link");
        super::write_manifest(&path, &serde_json::json!({ "new": true })).expect("write");
        assert_eq!(std::fs::read_to_string(&old).expect("old"), "{\"old\":true}");
        let new: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("new")).expect("json");
        assert_eq!(new["new"], true);
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 2, "no temporary file left: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A job whose output folder never appeared reports no manifest; it does not make the folder.
    #[test]
    fn a_manifest_does_not_create_its_folder() {
        let dir = regia_test_utils::unique_temp_dir("render_out_manifest_missing");
        let missing = dir.join("never");
        let path = missing.join("take_render_out.json");
        assert!(super::write_manifest(&path, &serde_json::json!({})).is_err());
        assert!(!missing.exists(), "the manifest made the job's folder");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique relative path under the project root that no test ever creates.
    fn never_path() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Relative, so it resolves under the project root (the working directory in these tests).
        format!("regia-render-out-never-{}-{n}", std::process::id())
    }

    #[test]
    fn the_manifest_clock_block_names_the_fixed_step_clock_and_the_start_state() {
        // ADR-1656: a repeat job in one running app starts from the app clocks the first left.
        let mut world = World::new();
        assert_eq!(StartClock::of(&world), StartClock::default());
        let mut time = Time::default();
        time.advance(std::time::Duration::from_millis(40));
        time.advance(std::time::Duration::from_millis(10));
        world.insert_resource(time);
        let clock = StartClock::of(&world);
        assert_eq!(clock.game_ns, 50_000_000);
        assert_eq!(clock.sim_ns, 50_000_000);
        assert_eq!(clock.app_frames, 2);
        let block = clock.to_json();
        assert_eq!(block["mode"], "fixed_step");
        assert_eq!(block["wall_clock"], false);
        assert_eq!(block["job_time_origin"], "zero");
        assert_eq!(block["live_effects"], "moved_on");
        let held = StartClock {
            effects_restored: true,
            ..clock
        };
        assert_eq!(held.to_json()["live_effects"], "restored");
        // ADR-2687: unseeded effects draw from the job's stream; an idle particle clock restarts.
        assert_eq!(block["effect_seeds"], "job_stream");
        assert_eq!(block["particle_clock"], "continued");
        let restarted = StartClock {
            particle_clock_restarted: true,
            ..clock
        };
        assert_eq!(restarted.to_json()["particle_clock"], "restarted");
        assert_eq!(block["app_game_time_at_start_ns"], 50_000_000);
        assert_eq!(block["app_frame_at_start"], 2);
    }

    #[test]
    fn the_render_out_path_reads_no_wall_clock() {
        // Built from pieces so this test does not match itself.
        let forbidden = [
            ["Instant", "::now"].concat(),
            ["System", "Time"].concat(),
            ["thread::", "sleep"].concat(),
            ["Real", "Time"].concat(),
        ];
        for (name, source) in [
            ("mod.rs", include_str!("mod.rs")),
            ("plan.rs", include_str!("plan.rs")),
            ("sink.rs", include_str!("sink.rs")),
            ("shutter.rs", include_str!("shutter.rs")),
            ("accumulate.rs", include_str!("accumulate.rs")),
            ("deep.rs", include_str!("deep.rs")),
            ("writer.rs", include_str!("writer.rs")),
            ("time_map.rs", include_str!("time_map.rs")),
            ("audio.rs", include_str!("audio.rs")),
            ("aov.rs", include_str!("aov.rs")),
            ("aov_pass.rs", include_str!("aov_pass.rs")),
            ("exr_file.rs", include_str!("exr_file.rs")),
            ("takes.rs", include_str!("takes.rs")),
            ("product.rs", include_str!("product.rs")),
        ] {
            // Only the code before the test module counts.
            let code = source.split("#[cfg(test)]").next().unwrap_or(source);
            for needle in &forbidden {
                assert!(!code.contains(needle.as_str()), "{name} uses `{needle}`");
            }
        }
    }

    #[test]
    fn a_relative_out_without_an_asset_server_joins_the_agents_project_dir() {
        // No AssetServer and no project dir: the working directory, as before.
        let mut world = World::new();
        assert_eq!(out_root(&world), std::env::current_dir().ok());
        // A host without assets (the showcase, a test app) that gave the agent a project dir.
        let project = regia_test_utils::unique_temp_dir("render-out-root");
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(project.clone()),
        });
        assert_eq!(out_root(&world), Some(project.clone()));
        let mut request = Request::parse(&json!({ "out": "renders/take1", "frames": 2 })).expect("request");
        let root = out_root(&world).expect("root");
        request.confine_to(&root, &[]).expect("relative out");
        assert_eq!(request.out_dir, project.join("renders/take1"));
        // An app with no project dir set keeps the working directory.
        world.insert_resource(crate::AgentSettings { project_dir: None });
        assert_eq!(out_root(&world), std::env::current_dir().ok());
        let _ = std::fs::remove_dir_all(&project);
    }

    /// A job of three delivered frames with audio, ready to close, and the WAV it will make.
    fn audio_job(world: &mut World, dir: &std::path::Path) -> (Job, std::path::PathBuf) {
        world.init_resource::<Time>();
        world.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..regia_audio::AudioSettings::default()
        }));
        let mut job = active_job(5, 3);
        job.request = Request::parse(&json!({
            "out": dir.display().to_string(), "name": "take", "frames": 3, "fps": 30,
            "audio": { "format": "pcm16", "stems": ["Master"] }
        }))
        .expect("request");
        let wanted = job.request.audio.clone().expect("audio");
        std::fs::create_dir_all(&job.request.out_dir).expect("out dir");
        let mut out = audio::AudioOut::start(world, &job.request, &wanted).expect("start");
        out.pre_roll(world, &job.request).expect("pre-roll");
        for index in 0..3 {
            out.frame(world, &job.request, index).expect("frame");
        }
        job.audio = Some(out);
        job.delivered = 3;
        job.hashes = vec![1, 2, 3];
        job.phase = Phase::Rendering;
        let wav = job.request.out_dir.join("take_audio.wav");
        (job, wav)
    }

    #[test]
    fn a_stalled_file_sync_never_holds_the_app_thread() {
        // A sync that does not return until the test says so stands in for a busy disk. Every call
        // below runs on this thread, the app thread; if one of them waited for the sync, the test
        // would hang until the sync gave up and failed the job, not pass.
        let dir = regia_test_utils::unique_temp_dir("render-out-slow-sync");
        let mut world = World::new();
        let (job, wav) = audio_job(&mut world, &dir);
        let manifest = dir.join("take_render_out.json");
        let (entered, in_sync) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let held = std::sync::Mutex::new(Some((entered, released)));
        world.insert_resource(RenderOutFileSync(std::sync::Arc::new(move |file| {
            let hold = held.lock().expect("held").take();
            if let Some((entered, released)) = hold {
                entered.send(()).expect("the test waits");
                released
                    .recv_timeout(std::time::Duration::from_secs(600))
                    .map_err(std::io::Error::other)?;
            }
            file.sync_all()
        })));
        begin_close(&mut world, job, Status::Done);
        in_sync.recv().expect("the closing thread reached the sync");
        for _ in 0..1_000 {
            advance_render_out(&mut world);
            let poll = progress(&mut world, &json!({ "job": 5 })).expect("a poll answers");
            assert_eq!(poll["status"], json!("running"), "{poll}");
            assert_eq!(poll["phase"], json!("closing"), "{poll}");
            assert_eq!(poll["frame"], json!(3));
            assert_eq!(poll["audio"]["samples"], json!(3 * 1_600));
        }
        assert!(!wav.exists(), "no final name before the sync returned");
        assert!(!manifest.exists(), "no manifest before the files are in place");
        // A closing job has nothing left to cancel: the cancel answers its state and changes
        // nothing (ADR-1965; it answered NOT_FOUND before).
        let cancelled = cancel(&mut world, &json!({})).expect("a closing job answers");
        assert_eq!(
            cancelled,
            json!({ "job": 5, "status": "closing", "ending": "done" })
        );
        let poll = progress(&mut world, &json!({ "job": 5 })).expect("a poll answers");
        assert_eq!(poll["status"], json!("running"), "{poll}");
        let refused = start(&mut world, &json!({ "out": "x", "frames": 1 })).expect_err("busy");
        assert_eq!(
            refused.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0018")
        );
        release.send(()).expect("the sync waits");
        while world.resource::<RenderOutJobs>().active.is_some() {
            advance_render_out(&mut world);
        }
        let done = progress(&mut world, &json!({ "job": 5 })).expect("report");
        assert_eq!(done["status"], json!("done"), "{done}");
        assert_eq!(done["audio"]["samples"], json!(3 * 1_600));
        assert!(wav.exists() && manifest.exists());
        let parts = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".part"));
        assert_eq!(parts.count(), 0, "every WAV took its final name");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_sync_keeps_its_part_name_and_fails_the_job() {
        let dir = regia_test_utils::unique_temp_dir("render-out-failed-sync");
        let mut world = World::new();
        let (job, wav) = audio_job(&mut world, &dir);
        world.insert_resource(RenderOutFileSync(std::sync::Arc::new(|_| {
            Err(std::io::Error::other("disk gone"))
        })));
        close_now(&mut world, job, Status::Done);
        let done = progress(&mut world, &json!({ "job": 5 })).expect("report");
        assert_eq!(done["status"], json!("failed"), "{done}");
        assert!(done["error"].as_str().is_some_and(|e| e.contains("disk gone")));
        assert!(!wav.exists(), "an unsynced file never takes its final name");
        assert!(dir.join("take_audio.wav.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_out_names_its_reason_on_the_wire() {
        // `render.out_start` end to end, each shape `book/agents/safety.md` lists for the rule; the
        // rule's own table is in plan.rs.
        let base = regia_test_utils::unique_temp_dir("render-out-refused");
        let root = base.join("project");
        std::fs::create_dir_all(&root).expect("root");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        for (out, path_reason) in [
            ("../up", "path_parent"),
            ("renders/../../up", "path_parent"),
            // `C:\up` is absolute on Windows: under no root there, so `path_escapes` (plan.rs).
            (
                "C:\\up",
                if cfg!(windows) {
                    "path_escapes"
                } else {
                    "path_prefix"
                },
            ),
            ("C:up", "path_prefix"),
            ("a\0b", "path_nul"),
            ("CON", "path_device"),
        ] {
            let error = start(&mut world, &json!({ "out": out, "frames": 1 })).expect_err(out);
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{out}");
            let data = error.data.as_ref().expect("data");
            assert_eq!(data["code"], "F0002", "{out}");
            assert_eq!(data["reason"], "path_outside_project", "{out}");
            assert_eq!(data["path_reason"], path_reason, "{out}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn progress_and_cancel_without_a_job_are_clean_errors() {
        let mut world = World::new();
        assert!(progress(&mut world, &json!({})).is_err());
        world.init_resource::<RenderOutJobs>();
        assert!(progress(&mut world, &json!({})).is_err());
        assert!(cancel(&mut world, &json!({})).is_err());
    }

    #[test]
    fn start_validates_before_touching_the_world() {
        let mut world = World::new();
        let error = start(&mut world, &json!({ "frames": 4 })).expect_err("no out");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        let never = never_path();
        let error = start(&mut world, &json!({ "out": never, "frames": 4 })).expect_err("no renderer");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0011")
        );
        assert!(!std::path::Path::new(&never).exists(), "nothing was created");
    }

    /// ADR-5180: a host that opened no GPU on purpose (`GpuDisabled`, the headless editor's
    /// `--no-gpu`) refuses a render-out with its own code before anything is created, and no
    /// device is opened behind its back.
    #[test]
    fn a_host_without_a_gpu_refuses_render_out_and_opens_no_device() {
        let mut world = World::new();
        world.init_resource::<RuntimeRenderer>();
        world.insert_resource(crate::seeing::GpuDisabled {
            code: "editor_headless".into(),
            reason: "the headless editor has no GPU adapter (started with --no-gpu)".into(),
            hint: "start it without --no-gpu".into(),
        });
        let never = never_path();
        let error = start(&mut world, &json!({ "out": never, "frames": 2 })).expect_err("refused");
        assert_eq!(error.code, protocol::UNSUPPORTED);
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("editor_headless")
        );
        assert!(!std::path::Path::new(&never).exists(), "nothing was created");
        assert!(
            world.get_resource::<regia_render::RenderDevice>().is_none(),
            "no device"
        );
        assert!(
            world
                .get_resource::<RenderOutJobs>()
                .is_none_or(|jobs| jobs.active.is_none()),
            "no job started"
        );
        let error = crate::seeing::offscreen_pixels(&mut world, 8, 8, false, false).expect_err("refused");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("editor_headless")
        );
        assert!(
            world.get_resource::<regia_render::RenderDevice>().is_none(),
            "still no device"
        );
    }

    #[test]
    fn a_finished_job_reports_its_hashes_and_failure() {
        let mut world = World::new();
        let request = Request::parse(&json!({ "out": "/tmp/x", "frames": 3 })).expect("request");
        let mut jobs = RenderOutJobs::default();
        jobs.finished.push(Report {
            id: 7,
            status: Status::Failed("boom".into()),
            request,
            delivered: 2,
            hashes: vec![1, 2],
            sequence: 9,
            files: vec!["a.png".into(), "b.png".into()],
            cuts: 1,
            metered_cuts: 1,
            manifest: None,
            sim_ns: 66_666_666,
            replay_from: None,
            audio: Value::Null,
            job: Value::Null,
        });
        world.insert_resource(jobs);
        let out = progress(&mut world, &json!({ "job": 7, "hashes": true })).expect("report");
        assert_eq!(out["status"], json!("failed"));
        assert_eq!(out["error"], json!("boom"));
        assert_eq!(out["frame"], json!(2));
        assert_eq!(
            out["hashes"]["hashes"],
            json!(["0000000000000001", "0000000000000002"])
        );
        assert_eq!(out["sequence_hash"], json!("0000000000000009"));
        assert_eq!(out["camera_cuts"], json!(1));
        assert_eq!(out["exposure_metered_cuts"], json!(1));
        assert!(progress(&mut world, &json!({ "job": 8 })).is_err());
        assert_eq!(progress(&mut world, &json!({})).expect("latest")["job"], json!(7));
    }

    fn active_job(id: u64, frames: u64) -> Job {
        Job {
            id,
            request: Request::parse(&json!({ "out": "/tmp/x", "frames": frames })).expect("request"),
            sink: None,
            hud: None,
            phase: Phase::Arming,
            delivered: 0,
            hashes: Vec::new(),
            sequence: 0,
            saved: Saved {
                strategy: TimeUpdateStrategy::default(),
                paused: false,
                time_scale: 1.0,
                wait_readback: false,
                complete_frames: false,
                max_substeps: None,
                sim_step: None,
                clocks: Default::default(),
                weather: None,
                effects: None,
                shakes: None,
                effects_start: None,
            },
            cancel: false,
            cuts_before: 0,
            metered_cuts: 0,
            subframe: 0,
            acc: None,
            deep_target: None,
            sim_ns: 0,
            replay_from: None,
            audio: None,
            aov: aov_pass::AovState::default(),
            hdr_sum: None,
            fades: Vec::new(),
            takes: None,
            start_clock: StartClock::default(),
            closing: None,
            pending: None,
            held: None,
            notes_seen: 0,
            product: None,
            render_ms: Vec::new(),
            renders: 0,
        }
    }

    /// Ends `job` and waits for its closing thread, as the app does over the next frames.
    /// ADR-2396 (audit 11-03 agent H2): a hand-over interrupted by a cancel request keeps the
    /// made frame for the next app frame and holds both clocks still meanwhile; once it is handed
    /// over the clocks get their steps back and the frame is booked as frame N.
    #[cfg(unix)]
    #[test]
    fn an_interrupted_hand_over_keeps_the_frame_and_holds_the_clocks() {
        let dir = regia_test_utils::unique_temp_dir("render-out-held-frame");
        let script = dir.join("deaf-ffmpeg");
        regia_test_utils::process::write_executable_script(&script, "#!/bin/sh\nexec sleep 120\n")
            .expect("script");
        let request = Request::parse(&json!({
            "out": dir.display().to_string(), "frames": 8, "format": "ffv1", "width": 1024, "height": 1024
        }))
        .expect("request");
        let mut sink = Sink::open_movie(&request, &script).expect("sink");
        let big = || FrameData::Rgba8(vec![0; 1024 * 1024 * 4]);
        for index in 0..3 {
            assert!(
                sink.hand_over(index, big(), Vec::new(), &|| false)
                    .expect("fits")
                    .is_none()
            );
        }
        let mut world = World::new();
        let frame_step = std::time::Duration::from_millis(40);
        world.insert_resource(TimeUpdateStrategy::ManualDuration(frame_step));
        world.insert_resource(SimStep(frame_step));
        let mut job = active_job(9, 8);
        job.request = request;
        job.phase = Phase::Rendering;
        job.sink = Some(sink);
        job.notes_seen = cancel_notes();
        let pending = Pending {
            index: 0,
            frame: big(),
            aovs: Vec::new(),
            hash: 0xab,
        };
        let (done, returned) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let kept = hand_over_frame(&mut world, &mut job, pending);
            let _ = done.send((kept, world, job));
        });
        note_cancel_request();
        let (kept, mut world, mut job) = returned
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the cancel request ends the wait");
        assert_eq!(kept, Ok(false), "the job goes on");
        assert!(
            job.pending
                .as_ref()
                .is_some_and(|p| p.index == 0 && p.hash == 0xab)
        );
        assert_eq!((job.delivered, job.hashes.len()), (0, 0), "nothing is booked yet");
        assert_eq!(
            world.get_resource::<TimeUpdateStrategy>().copied(),
            Some(TimeUpdateStrategy::ManualDuration(std::time::Duration::ZERO))
        );
        assert_eq!(
            world.get_resource::<SimStep>().copied(),
            Some(SimStep(std::time::Duration::ZERO))
        );
        // A cancel the dispatch accepted ends the job before the frame is retried.
        job.cancel = true;
        assert!(matches!(
            step(&mut world, &mut job),
            Step::Finish(Status::Cancelled)
        ));
        release_clocks(&mut world, &mut job);
        assert_eq!(
            world.get_resource::<TimeUpdateStrategy>().copied(),
            Some(TimeUpdateStrategy::ManualDuration(frame_step))
        );
        assert_eq!(
            world.get_resource::<SimStep>().copied(),
            Some(SimStep(frame_step))
        );
        if let Some(sink) = job.sink.take() {
            std::thread::spawn(move || drop(sink.finish()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn close_now(world: &mut World, job: Job, status: Status) {
        begin_close(world, job, status);
        while world.resource::<RenderOutJobs>().active.is_some() {
            advance_render_out(world);
        }
    }

    /// ADR-4894 (the lost-work review's High finding): the output stalled for good and the inbox
    /// holds a full load of polls the progress board already answered, with the cancel behind
    /// them. The cancel's two notes each end one step's wait; the app thread then blocks in the
    /// output reserve again with no further wake coming, so the cancel must dispatch in the
    /// first frame that drains the inbox — however many answered polls sit in front of it.
    #[test]
    fn a_cancel_behind_a_full_inbox_of_board_polls_dispatches_at_once() {
        let mut world = world_with_active(7);
        world.insert_resource(Time::default());
        let (inbox_tx, inbox_rx) = crossbeam_channel::bounded(1100);
        world.insert_resource(crate::dispatch::AgentInbox(inbox_rx));
        // More answered polls than one frame drains (MAX_PER_FRAME is 64), then the cancel.
        for i in 0..1024 {
            let (reply, _rx) = crossbeam_channel::bounded(1);
            crate::server::abandon_board_for_test(reply.clone());
            inbox_tx
                .send(crate::server::Inbound {
                    request: crate::server::parse_request(&format!(
                        r#"{{"jsonrpc":"2.0","id":{i},"method":"render.out_progress","params":{{"job":7}}}}"#
                    ))
                    .expect("well-formed"),
                    reply,
                })
                .expect("queued");
        }
        let (cancel_tx, cancel_rx) = crossbeam_channel::bounded(1);
        inbox_tx
            .send(crate::server::Inbound {
                request: crate::server::parse_request(
                    r#"{"jsonrpc":"2.0","id":2000,"method":"render.out_cancel","params":{}}"#,
                )
                .expect("well-formed"),
                reply: cancel_tx,
            })
            .expect("queued");
        drop(inbox_tx);
        crate::dispatch::process_requests(&mut world);
        let answer = cancel_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the cancel dispatches in the draining frame");
        assert_eq!(
            answer.result.expect("the cancel answer")["status"],
            json!("cancelling")
        );
        assert!(
            world
                .resource::<RenderOutJobs>()
                .active
                .as_ref()
                .is_some_and(|job| job.cancel),
            "the running job took the cancel"
        );
        // The job then reports `cancelled`.
        let job = world
            .resource_mut::<RenderOutJobs>()
            .active
            .take()
            .expect("the active job");
        close_now(&mut world, job, Status::Cancelled);
        let report = progress(&mut world, &json!({ "job": 7 })).expect("the finished report");
        assert_eq!(report["status"], json!("cancelled"));
    }

    fn world_with_active(id: u64) -> World {
        let mut world = World::new();
        world.insert_resource(RenderOutJobs {
            next_id: id,
            active: Some(active_job(id, 10)),
            finished: Vec::new(),
        });
        world
    }

    #[test]
    fn a_second_start_while_one_runs_is_refused_before_anything_else() {
        let mut world = world_with_active(1);
        let never = never_path();
        let error = start(&mut world, &json!({ "out": never, "frames": 4 })).expect_err("busy");
        assert_eq!(error.code, protocol::INVALID_REQUEST);
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0018")
        );
        assert!(!std::path::Path::new(&never).exists());
        // Bad params still win: validation runs first and does not disturb the running job.
        let error = start(&mut world, &json!({ "frames": 4 })).expect_err("invalid");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert!(world.resource::<RenderOutJobs>().active.is_some());
    }

    /// Renderout audit 4: start wrote the clock once, and `play.*` could freeze or scale it for the
    /// rest of the job while the manifest and the audio kept the request's time.
    #[test]
    fn a_running_job_holds_the_clock_against_every_play_control() {
        let mut world = world_with_active(1);
        world.init_resource::<crate::playtest::AgentTimeControl>();
        world.init_resource::<Time>();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let refusals = [
            crate::playtest::set_paused(&mut world, true).map(|_| ()),
            crate::playtest::set_paused(&mut world, false).map(|_| ()),
            crate::playtest::set_time_scale(&mut world, &json!({ "scale": 2.0 })).map(|_| ()),
            crate::playtest::step(&mut world, &json!({ "frames": 1 }), json!(1), tx),
        ];
        for refused in refusals {
            let error = refused.expect_err("the job holds the clock");
            assert_eq!(
                error.data.as_ref().and_then(|d| d["code"].as_str()),
                Some("F0018")
            );
        }
        assert!(!world.resource::<Time>().paused);
        assert_eq!(world.resource::<Time>().time_scale, 1.0);
        // A game or a frame budget writes `Time` directly: the job's own frame puts it back.
        {
            let mut time = world.resource_mut::<Time>();
            time.paused = true;
            time.time_scale = 2.0;
        }
        // The arming frame of a job with a warm-up continues without touching the renderer.
        world
            .resource_mut::<RenderOutJobs>()
            .active
            .as_mut()
            .expect("job")
            .request
            .warmup = 1;
        advance_render_out(&mut world);
        assert!(job_running(&world));
        assert!(!world.resource::<Time>().paused);
        assert_eq!(world.resource::<Time>().time_scale, 1.0);
    }

    #[test]
    fn an_out_path_escaping_the_root_is_refused() {
        let mut world = World::new();
        for out in ["../regia-escape", "/etc/regia-escape"] {
            let error = start(&mut world, &json!({ "out": out, "frames": 4 })).expect_err("escapes");
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{out}");
        }
        assert!(!std::path::Path::new("/etc/regia-escape").exists());
    }

    /// Muse's read-only review of fix-render6 (finding 1): the job forced the exposure wait
    /// before it captured the complete-frames switch, so the switch's saved state held the
    /// forced `true` instead of the host's setting. A job that then ended during a warm left
    /// the wait on for good on the renderer that came back (the stand-in's own restore won,
    /// and `absorb_stand_in` re-applied the polluted saved state with no job restore after
    /// it). The job must save the host's wait, never the forced one.
    #[test]
    fn a_job_saves_the_hosts_exposure_wait_not_the_forced_one() {
        let dir = regia_test_utils::unique_temp_dir("render-out-saved-wait");
        let mut world = World::new();
        world.init_resource::<Time>();
        world.insert_resource(TimeUpdateStrategy::default());
        world.insert_resource(RuntimeRenderer::default());
        let answer = start(
            &mut world,
            &json!({ "out": dir.display().to_string(), "frames": 2, "format": "png" }),
        )
        .expect("start");
        assert_eq!(answer["status"], json!("running"));
        assert!(
            world
                .resource::<RuntimeRenderer>()
                .0
                .waits_for_exposure_readback(),
            "the job meters each histogram on the frame it is due"
        );
        // Review of fixrev-20-k, finding 1: the switch's own saved state is what a warm
        // re-applies (`absorb_stand_in` turns the switch off when the job ended on the
        // stand-in), and `close_now` alone cannot see it: it restores the wait explicitly
        // afterwards. Turn the switch off here as a warm would and read what it put back.
        {
            let mut renderer = world.resource_mut::<RuntimeRenderer>();
            renderer.0.set_complete_frames(false); // what absorb_stand_in does on a warm
            assert!(
                !renderer.0.waits_for_exposure_readback(),
                "the switch saved the host's wait, not the forced one"
            );
            renderer.0.set_complete_frames(true);
        }
        let job = world
            .resource_mut::<RenderOutJobs>()
            .active
            .take()
            .expect("the job runs");
        assert!(
            !job.saved.wait_readback,
            "the job saves the host's wait (off), not the value it forced"
        );
        assert!(!job.saved.complete_frames);
        close_now(&mut world, job, Status::Done);
        assert!(
            !world
                .resource::<RuntimeRenderer>()
                .0
                .waits_for_exposure_readback(),
            "the host's wait (off) is back when the job ends"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review of fixrev-20-k, finding 3: a host that already runs with complete frames on and
    /// then turns the exposure wait off. The switch is already on, so turning it on again
    /// forces nothing; the job must still force the wait itself (a render-out meters each
    /// histogram on its due frame, ADR-1402), and put the host's wait (off) and its switch
    /// (on) back when it ends.
    #[test]
    fn a_job_forces_the_wait_on_a_host_with_complete_frames_and_the_wait_off() {
        let dir = regia_test_utils::unique_temp_dir("render-out-complete-wait-off");
        let mut world = World::new();
        world.init_resource::<Time>();
        world.insert_resource(TimeUpdateStrategy::default());
        world.insert_resource(RuntimeRenderer::default());
        {
            let mut renderer = world.resource_mut::<RuntimeRenderer>();
            renderer.0.set_complete_frames(true);
            renderer.0.set_wait_for_exposure_readback(false);
        }
        let answer = start(
            &mut world,
            &json!({ "out": dir.display().to_string(), "frames": 2, "format": "png" }),
        )
        .expect("start");
        assert_eq!(answer["status"], json!("running"));
        assert!(
            world
                .resource::<RuntimeRenderer>()
                .0
                .waits_for_exposure_readback(),
            "the job meters each histogram on the frame it is due, whatever the host's switch"
        );
        let job = world
            .resource_mut::<RenderOutJobs>()
            .active
            .take()
            .expect("the job runs");
        assert!(!job.saved.wait_readback, "the host's wait (off) is saved");
        assert!(job.saved.complete_frames, "the host's switch (on) is saved");
        close_now(&mut world, job, Status::Done);
        let renderer = &world.resource::<RuntimeRenderer>().0;
        assert!(
            !renderer.waits_for_exposure_readback(),
            "the host's wait (off) is back when the job ends"
        );
        assert!(renderer.complete_frames(), "the host's switch stays on");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_is_refused_while_a_play_step_runs() {
        let mut world = World::new();
        let (tx, _rx) = crossbeam_channel::unbounded();
        world.insert_resource(crate::playtest::AgentTimeControl {
            paused: false,
            stepping: Some((1, json!(1), tx)),
            step_dispatch_frame: None,
            ..Default::default()
        });
        let never = never_path();
        let error = start(&mut world, &json!({ "out": never, "frames": 4 })).expect_err("stepping");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0009")
        );
    }

    #[test]
    fn cancel_is_idempotent_and_progress_reports_cancelling() {
        let mut world = world_with_active(3);
        let first = cancel(&mut world, &json!({})).expect("cancel");
        assert_eq!(first, json!({ "job": 3, "status": "cancelling" }));
        let second = cancel(&mut world, &json!({ "job": 3 })).expect("cancel again");
        assert_eq!(first, second, "a second cancel changes nothing");
        let progress = progress(&mut world, &json!({})).expect("progress");
        assert_eq!(progress["status"], "cancelling");
        assert_eq!(progress["job"], 3);
        assert_eq!(progress["phase"], "arming");
        assert_eq!(progress["frames"], 10);
        assert_eq!(progress["fraction"], 0.0);
        assert_eq!(progress["hashes"], Value::Null, "hashes are opt-in");
    }

    #[test]
    fn cancel_and_progress_with_an_unknown_job_id_are_not_found() {
        let mut world = world_with_active(3);
        let error = cancel(&mut world, &json!({ "job": 99 })).expect_err("unknown job");
        assert_eq!(error.code, protocol::NOT_FOUND);
        // The running job was not cancelled by the stray call.
        assert!(
            !world
                .resource::<RenderOutJobs>()
                .active
                .as_ref()
                .expect("active")
                .cancel
        );
        let error = progress(&mut world, &json!({ "job": 99 })).expect_err("unknown job");
        assert_eq!(error.code, protocol::NOT_FOUND);
        // A finished job cannot be cancelled either.
        let mut world = World::new();
        world.insert_resource(RenderOutJobs::default());
        let error = cancel(&mut world, &json!({ "job": 1 })).expect_err("nothing running");
        assert_eq!(error.code, protocol::NOT_FOUND);
    }

    #[test]
    fn progress_hash_window_pages_and_clamps() {
        let hashes: Vec<u64> = (0..10).collect();
        assert_eq!(hash_window(&hashes, &json!({})).expect("window"), Value::Null);
        assert_eq!(
            hash_window(&hashes, &json!({ "hashes": false })).expect("window"),
            Value::Null
        );
        // ADR-2622: a wrong-typed `hashes` is refused, not read as false.
        let err = hash_window(&hashes, &json!({ "hashes": "true" })).expect_err("refused");
        assert_eq!(
            err.data.as_ref().map(|d| d["param"].clone()),
            Some(json!("hashes"))
        );
        let page = hash_window(&hashes, &json!({ "hashes": true, "hashes_from": 8 })).expect("window");
        assert_eq!(page["hashes"], json!(["0000000000000008", "0000000000000009"]));
        let past = hash_window(&hashes, &json!({ "hashes": true, "hashes_from": 1_000 })).expect("window");
        assert_eq!(past["hashes"], json!([]));
        let many: Vec<u64> = (0..(HASHES_PER_CALL as u64 + 50)).collect();
        let capped = hash_window(&many, &json!({ "hashes": true })).expect("window");
        assert_eq!(capped["hashes"].as_array().map(Vec::len), Some(HASHES_PER_CALL));
        let rest =
            hash_window(&many, &json!({ "hashes": true, "hashes_from": HASHES_PER_CALL })).expect("window");
        assert_eq!(rest["hashes"].as_array().map(Vec::len), Some(50));
    }

    #[test]
    fn finished_reports_select_by_id_and_default_to_the_latest() {
        let mut world = World::new();
        let mut jobs = RenderOutJobs::default();
        for (id, status) in [(1, Status::Done), (2, Status::Cancelled)] {
            jobs.finished.push(Report {
                id,
                status,
                request: Request::parse(&json!({ "out": "/tmp/x", "frames": 4 })).expect("request"),
                delivered: 4,
                hashes: vec![id],
                sequence: id,
                files: vec![],
                cuts: 0,
                metered_cuts: 0,
                manifest: None,
                sim_ns: 4 * 33_333_333,
                replay_from: Some(id as u32),
                audio: Value::Null,
                job: Value::Null,
            });
        }
        world.insert_resource(jobs);
        assert_eq!(
            progress(&mut world, &json!({})).expect("latest")["status"],
            "cancelled"
        );
        let one = progress(&mut world, &json!({ "job": 1 })).expect("job 1");
        assert_eq!(one["status"], "done");
        assert_eq!(one["fraction"], 1.0);
        assert!(one.get("error").is_none());
        // Progress is read-only: asking twice gives the same answer.
        assert_eq!(one, progress(&mut world, &json!({ "job": 1 })).expect("again"));
    }

    #[test]
    fn a_time_map_too_fast_for_the_fixed_loop_is_refused_before_anything_is_created() {
        let mut world = World::new();
        world.insert_resource(FixedTime::default());
        let never = never_path();
        // 30 fps at 40x: 1.33 s of game time per frame, 80 fixed steps of 1/60 s.
        let error = start(
            &mut world,
            &json!({ "out": never, "frames": 4, "fps": 30, "time_map": [{ "frame": 0, "scale": 40 }] }),
        )
        .expect_err("too fast");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert!(error.message.contains("time_map"), "{}", error.message);
        assert!(!std::path::Path::new(&never).exists(), "nothing was created");
        // Within the limit the request gets past the check (to the missing renderer), and the
        // fixed loop is not touched by a refused job.
        let error = start(
            &mut world,
            &json!({ "out": never, "frames": 4, "fps": 30, "time_map": [{ "frame": 0, "scale": 4 }] }),
        )
        .expect_err("no renderer");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0011")
        );
        assert_eq!(world.resource::<FixedTime>().max_substeps, 5);
    }

    /// The shutter doubles the simulation time counted toward the 64-step cap. It does not
    /// raise the cap. At 25 fps with a 25 Hz fixed step, scale 40 needs 41 steps; the same
    /// request with the shutter open needs 81 and is refused.
    #[test]
    fn the_shutter_doubles_demand_and_the_cap_stays_64() {
        let mut world = World::new();
        let mut fixed = FixedTime::default();
        fixed.hz = 25.0;
        world.insert_resource(fixed);
        let plain = Request::parse(&json!({
            "out": "/tmp/x",
            "frames": 2,
            "fps": 25,
            "time_map": [{ "frame": 0, "scale": 40 }]
        }))
        .expect("parse");
        assert_eq!(time_map_substeps(&world, &plain).expect("within cap"), Some(41));
        let blurred = Request::parse(&json!({
            "out": "/tmp/x",
            "frames": 2,
            "fps": 25,
            "subframes": 2,
            "time_map": [{ "frame": 0, "scale": 40 }]
        }))
        .expect("parse");
        let error = time_map_substeps(&world, &blurred).expect_err("shutter doubles the demand");
        assert!(error.message.contains("81"), "{}", error.message);
        assert!(error.message.contains("at most 64"), "{}", error.message);
    }

    #[test]
    fn fixed_steps_needed_bounds_one_frame_of_the_accumulator() {
        let fixed = 16_666_667;
        assert_eq!(fixed_steps_needed(fixed * 8, fixed), 9);
        assert_eq!(fixed_steps_needed(fixed - 1, fixed), 1);
        assert_eq!(fixed_steps_needed(0, fixed), 1);
        assert_eq!(fixed_steps_needed(7, 0), 8, "a zero step counts as 1 ns");
        // The bound holds for the worst banked remainder: just under one step.
        let mut accumulator = FixedTime::default();
        let step = accumulator.step();
        accumulator.max_substeps = 64;
        accumulator.accumulate(step - std::time::Duration::from_nanos(1));
        let peak = step * 5;
        let ran = accumulator.accumulate(peak);
        assert!(u64::from(ran) <= fixed_steps_needed(peak.as_nanos() as u64, step.as_nanos() as u64));
    }

    #[test]
    fn entering_frame_zero_starts_an_armed_replay_holding_the_step_into_it() {
        use regia_framework::replay_sim::{ReplayHeader, ReplaySim, SyntheticReplay};
        let mut world = World::new();
        let header = ReplayHeader {
            version: 3,
            tick_rate: 60,
            ..ReplayHeader::default()
        };
        let mut writer = SyntheticReplay::new(&header);
        for tick in 0..=100u32 {
            writer = writer.inputs(tick, &[]);
        }
        let mut sim = ReplaySim::new();
        sim.load_bytes(&writer.finish()).expect("replay");
        world.insert_resource(sim);
        world.insert_resource(crate::replay::ArmedReplay { from_tick: 42 });
        let mut job = active_job(1, 10);
        job.request = Request::parse(&json!({
            "out": "/tmp/x", "frames": 10, "fps": 25, "time_map": [{ "frame": 0, "scale": 0.5 }]
        }))
        .expect("request");
        enter_rendering(&mut world, &mut job);
        assert_eq!(job.replay_from, Some(42));
        let sim = world.resource::<ReplaySim>();
        assert_eq!((sim.tick(), sim.playing()), (42, true));
        // The step into frame 0 is the pre-roll simulation step: 40 ms at half speed.
        assert_eq!(sim.held(), std::time::Duration::from_millis(20));
        assert!(world.get_resource::<crate::replay::ArmedReplay>().is_none());
        // Without an arm the job starts no replay.
        let mut plain = active_job(2, 10);
        enter_rendering(&mut world, &mut plain);
        assert_eq!(plain.replay_from, None);
    }

    /// The step of each clock the next app frame takes: the render clock's manual step and the
    /// simulation clock's `SimStep` (`None` when the job set none).
    fn clock_steps(world: &World) -> (Option<u64>, Option<u64>) {
        let render = match world.get_resource::<TimeUpdateStrategy>() {
            Some(TimeUpdateStrategy::ManualDuration(d)) => Some(d.as_nanos() as u64),
            _ => None,
        };
        let sim = world.get_resource::<SimStep>().map(|s| s.0.as_nanos() as u64);
        (render, sim)
    }

    #[test]
    fn a_shutter_job_leads_into_frame_zero_on_each_clock() {
        let mut world = World::new();
        let mut job = active_job(1, 10);
        job.request = Request::parse(&json!({
            "out": "/tmp/x", "frames": 10, "fps": 25, "subframes": 2, "shutter_angle": 360,
            "time_map": [{ "frame": 0, "scale": 0.5 }]
        }))
        .expect("request");
        enter_rendering(&mut world, &mut job);
        // Render clock: one 40 ms step plus a quarter of it, as without a map. Simulation clock:
        // the 20 ms pre-roll step plus a quarter of frame 0's 20 ms step.
        assert_eq!(clock_steps(&world), (Some(50_000_000), Some(25_000_000)));
        assert_eq!(job.replay_from, None);
        // Without a map the shutter job sets the render clock only.
        let mut world = World::new();
        let mut plain = active_job(2, 10);
        plain.request = Request::parse(&json!({
            "out": "/tmp/x", "frames": 10, "fps": 25, "subframes": 2, "shutter_angle": 360
        }))
        .expect("request");
        enter_rendering(&mut world, &mut plain);
        assert_eq!(clock_steps(&world), (Some(50_000_000), None));
    }

    #[test]
    fn progress_reports_render_time_and_sim_time_apart() {
        let mut world = World::new();
        let mut job = active_job(4, 10);
        job.request = Request::parse(&json!({
            "out": "/tmp/x", "frames": 10, "fps": 25,
            "time_map": [{ "frame": 0, "scale": 1 }, { "frame": 2, "scale": 0.25, "ease": "hold" }]
        }))
        .expect("request");
        for index in 0..3 {
            step_to_next_frame(&mut world, &mut job, index);
        }
        job.delivered = 3;
        job.replay_from = Some(120);
        world.insert_resource(RenderOutJobs {
            next_id: 4,
            active: Some(job),
            finished: Vec::new(),
        });
        let out = progress(&mut world, &json!({})).expect("progress");
        assert_eq!(out["game_time_s"], json!(0.12), "render time: 3 frames of 40 ms");
        assert_eq!(out["sim_time_s"], json!(0.09), "40 + 40 + 10 ms");
        assert_eq!(out["sim_step_ns"], json!(10_000_000));
        assert_eq!(out["time_scale"], json!(0.25));
        assert_eq!(out["replay_from_tick"], json!(120));
    }

    #[test]
    fn the_simulation_clock_takes_each_frames_own_step_and_the_render_clock_does_not() {
        let mut world = World::new();
        let mut job = active_job(1, 10);
        job.request = Request::parse(&json!({
            "out": "/tmp/x", "frames": 10, "fps": 25,
            "time_map": [{ "frame": 0, "scale": 1 }, { "frame": 4, "scale": 0.25, "ease": "hold" }]
        }))
        .expect("request");
        let mut seen = Vec::new();
        for index in 0..6 {
            step_to_next_frame(&mut world, &mut job, index);
            let (render, sim) = clock_steps(&world);
            assert_eq!(render, None, "the render clock keeps the start step");
            seen.push(sim.expect("a simulation step"));
        }
        assert_eq!(
            seen,
            [
                40_000_000, 40_000_000, 40_000_000, 40_000_000, 10_000_000, 10_000_000
            ]
        );
        assert_eq!(job.sim_ns, 4 * 40_000_000 + 2 * 10_000_000);
        assert_eq!(job.sim_ns, job.request.sim_time(6).as_nanos() as u64);
        // Without a map neither clock is touched (the start step holds), byte for byte as before.
        let mut world = World::new();
        let mut plain = active_job(2, 10);
        step_to_next_frame(&mut world, &mut plain, 0);
        assert_eq!(clock_steps(&world), (None, None));
        assert_eq!(plain.sim_ns, plain.request.step_ns());
    }

    #[test]
    fn finishing_a_mapped_job_hands_the_simulation_clock_back() {
        for host in [None, Some(SimStep(std::time::Duration::from_millis(7)))] {
            let mut world = World::new();
            world.init_resource::<Time>();
            let mut job = active_job(3, 10);
            job.request = Request::parse(&json!({
                "out": never_path(), "frames": 10, "fps": 25, "time_map": [{ "frame": 0, "scale": 0.5 }]
            }))
            .expect("request");
            job.saved.sim_step = host;
            job.delivered = 0;
            set_sim_step(&mut world, 20_000_000);
            close_now(&mut world, job, Status::Cancelled);
            assert_eq!(world.get_resource::<SimStep>().copied(), host);
        }
    }

    #[test]
    fn the_manifest_time_table_has_both_clocks_per_frame() {
        let request = Request::parse(&json!({
            "out": "/tmp/x", "frames": 10, "fps": 25,
            "time_map": [{ "frame": 0, "scale": 1 }, { "frame": 2, "scale": 0.25, "ease": "hold" }]
        }))
        .expect("request");
        let (table, end) = frame_time_table(&request, 4);
        assert_eq!(table.len(), 4);
        let column = |key: &str| table.iter().map(|row| row[key].clone()).collect::<Vec<_>>();
        assert_eq!(column("frame"), [json!(0), json!(1), json!(2), json!(3)]);
        assert_eq!(
            column("game_time_ns"),
            [json!(0), json!(40_000_000), json!(80_000_000), json!(120_000_000)]
        );
        assert_eq!(
            column("sim_time_ns"),
            [json!(0), json!(40_000_000), json!(80_000_000), json!(90_000_000)]
        );
        assert_eq!(
            column("sim_step_ns"),
            [
                json!(40_000_000),
                json!(40_000_000),
                json!(10_000_000),
                json!(10_000_000)
            ]
        );
        assert_eq!(column("time_scale")[3], json!(0.25));
        assert_eq!(table[3]["sim_time_s"], json!(0.09));
        assert_eq!(table[3]["game_time_s"], json!(0.12));
        assert_eq!(end, 100_000_000);
        // No map: the two clocks are one.
        let plain = Request::parse(&json!({ "out": "/tmp/x", "frames": 3, "fps": 25 })).expect("plain");
        let (table, _) = frame_time_table(&plain, 3);
        assert!(table.iter().all(|row| row["game_time_ns"] == row["sim_time_ns"]));
        assert_eq!(MANIFEST_SCHEMA, "regia.render_out/2");
    }

    #[test]
    fn the_fade_block_says_which_outputs_carry_the_fade() {
        let parse = |extra: Value| {
            let mut params = json!({ "out": "/tmp/x", "frames": 4, "fps": 25 });
            params
                .as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            Request::parse(&params).expect("request")
        };
        let half = regia_render::FadeState {
            amount: 0.5,
            color: [1.0, 0.0, 0.0],
        };
        let over = regia_render::FadeState { amount: 3.0, ..half };
        let display = fade_json(&parse(json!({})), &[(2, half), (3, over)]);
        assert_eq!(
            display["in_frames"],
            json!(true),
            "8-bit display frames are composited"
        );
        assert_eq!(display["in_aovs"], json!(false), "the AOVs never carry it");
        assert_eq!(
            display["frames"][0],
            json!({ "frame": 2, "amount": 0.5, "color": [1.0, 0.0, 0.0] })
        );
        assert_eq!(display["frames"][1]["amount"], json!(1.0), "clamped as drawn");
        for before_the_compositor in [
            json!({ "bit_depth": 16, "hud": false }),
            json!({ "format": "exr", "hud": false }),
        ] {
            let block = fade_json(&parse(before_the_compositor.clone()), &[(0, half)]);
            assert_eq!(block["in_frames"], json!(false), "{before_the_compositor}");
            assert_eq!(block["frames"].as_array().map(Vec::len), Some(1));
        }
        assert_eq!(fade_json(&parse(json!({})), &[])["frames"], json!([]));
    }

    #[test]
    fn the_manifest_lists_each_aovs_files_per_delivered_frame() {
        let request = Request::parse(&json!({
            "out": "/tmp/x", "name": "plate", "frames": 10, "fps": 25, "format": "raw",
            "aovs": ["motion", "depth"], "aov_format": "raw"
        }))
        .expect("request");
        let files = aov_files(&request, 3);
        let list = files.as_array().expect("an array");
        assert_eq!(list.len(), 2, "one entry per AOV");
        assert_eq!(list[0]["aov"], "depth", "request order (sorted)");
        assert_eq!(list[1]["aov"], "motion");
        let depth = list[0]["files"].as_array().expect("files");
        assert_eq!(depth.len(), 3, "the delivered frames only");
        assert!(
            depth[2]
                .as_str()
                .is_some_and(|f| f.ends_with("plate_depth_000002.raw")),
            "{depth:?}"
        );
        let plain = Request::parse(&json!({ "out": "/tmp/x", "frames": 3, "fps": 25 })).expect("plain");
        assert_eq!(aov_files(&plain, 3), json!([]), "no AOVs: an empty list");
    }

    #[test]
    fn timings_are_opt_in_and_echoed() {
        let off = Request::parse(&json!({ "out": "/tmp/x", "frames": 2 })).expect("request");
        assert!(!off.timings);
        assert_eq!(request_json(&off)["timings"], false);
        let on = Request::parse(&json!({ "out": "/tmp/x", "frames": 2, "timings": true })).expect("request");
        assert!(on.timings);
        assert_eq!(request_json(&on)["timings"], true);
        assert!(Request::parse(&json!({ "out": "/tmp/x", "frames": 2, "timings": "yes" })).is_err());
    }

    #[test]
    fn request_json_echoes_the_validated_request() {
        let request = Request::parse(&json!({
            "out": "/tmp/x", "frames": 6, "fps": 30000, "fps_den": 1001, "cuts": [3, 1], "name": "s"
        }))
        .expect("request");
        let echoed = request_json(&request);
        assert_eq!(echoed["fps"], 30000);
        assert_eq!(echoed["fps_den"], 1001);
        assert_eq!(echoed["cuts"], json!([1, 3]));
        assert_eq!(echoed["step_ns"], 33_366_667);
        assert_eq!(echoed["format"], "png");
        assert_eq!(echoed["time_map"], json!([]), "no map: an empty list");
        let mapped = Request::parse(&json!({
            "out": "/tmp/x", "frames": 6, "time_map": [{ "frame": 2, "scale": 0.5, "ease": "linear" }]
        }))
        .expect("mapped");
        assert_eq!(
            request_json(&mapped)["time_map"],
            json!([{ "frame": 2, "scale": 0.5, "ease": "linear" }])
        );
    }
}
