// SPDX-License-Identifier: MPL-2.0
//! The Look-dev panel (`docs/specs/27-renderer-excellence.md` § 1.1): traces what the editor
//! viewport looks at with the reference path tracer (`regia_pt`), with progress and cancel, and
//! shows the AgX preview beside the files it wrote (a layered linear EXR with every § 1.2 AOV,
//! the preview PNG and the metadata JSON). Agent surface: `editor.lookdev_trace`,
//! `editor.lookdev_status`, `editor.lookdev_cancel` (`book/agents/editor-lookdev.md`).
//!
//! The editor is a desktop tool, so it may link the tracer (§ 1.1 and § 6 keep it out of every
//! runtime crate; `xtask check-layers` enforces that). The trace takes the **same inputs as the
//! real-time path**: the runtime extraction of the open level (`regia_render::extract_frame`,
//! through the editor's own `render::extract`) converted by `regia_pt::extract`, with the
//! editor camera as the view (`viewport`) or the level's own game camera (`game`). Editor-only
//! additions (the preview sun, proxy cubes, the backdrop) are not traced: the reference shows the
//! level as a game would render it.
//!
//! Each trace runs on its own thread with its **own headless device** (ADR-5896), as a bake runs
//! in its own process (ADR-866): the editor's frame never waits on it, and a device loss in the
//! trace cannot take the editor's device with it. The panel and the agent methods share the
//! [`LookDevPanel`] world resource, so an agent sees a trace a user started and the other way
//! round. One trace at a time.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_pt::{Aov, PathTracer, PresetSettings, PtPreset, PtScene, PtSettings};
use serde_json::{Value, json};

use crate::bake_runner::{bool_param, u32_param};
use crate::viewport::EditorCamera;
use crate::viewport_agent::{Handler, str_param};
use crate::views::{ViewCamera, ViewKind};

/// The error code when a trace is started while one is running.
pub const BUSY: &str = "lookdev_busy";
/// The error code when the level has nothing to trace (no mesh, or no view).
pub const NOTHING_TO_TRACE: &str = "lookdev_nothing_to_trace";
/// The error code when `scene: "game"` is asked of a level with no game (window) camera.
pub const NO_GAME_CAMERA: &str = "lookdev_no_game_camera";
/// The error code when a setting is out of range (the panel's own fields).
pub const INVALID_SETTINGS: &str = "lookdev_invalid_settings";
/// The error code when the trace thread cannot be started.
pub const SPAWN_FAILED: &str = "lookdev_spawn_failed";

/// Largest image side the panel traces.
pub const MAX_SIDE: u32 = 4096;
/// Most samples per pixel the panel traces.
pub const MAX_SPP: u32 = 65_536;
/// Longest path the panel traces.
pub const MAX_BOUNCES: u32 = 64;

/// How long dropping a running job waits for its thread before leaving it behind.
const JOIN_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// What the panel traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LookDevScene {
    /// The open level through the editor camera (the perspective viewport's view).
    #[default]
    Viewport,
    /// The open level through its own game camera (the first window camera).
    Game,
    /// The tracer's Cornell box (a check that the tracer runs, no level needed).
    Cornell,
    /// The tracer's white furnace sphere.
    Furnace,
}

impl LookDevScene {
    /// Every scene, in the panel's order.
    pub const ALL: [Self; 4] = [Self::Viewport, Self::Game, Self::Cornell, Self::Furnace];

    /// The stable name: `viewport`, `game`, `cornell`, `furnace`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Viewport => "viewport",
            Self::Game => "game",
            Self::Cornell => "cornell",
            Self::Furnace => "furnace",
        }
    }

    /// The scene of a stable name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// The trace settings the panel sets.
#[derive(Debug, Clone, PartialEq)]
pub struct LookDevSettings {
    /// What to trace.
    pub scene: LookDevScene,
    /// Image width, pixels.
    pub width: u32,
    /// Image height, pixels.
    pub height: u32,
    /// Samples per pixel (the preset's own when a preset is set and this was not changed).
    pub spp: u32,
    /// Maximum path length.
    pub max_bounces: u32,
    /// Trace seed (the same seed traces the same image on one device, § 1.1).
    pub seed: u32,
    /// A quality preset (`draft`, `preview`, `final`, `reference`; ADR-2433), or none for the
    /// plain progressive trace.
    pub preset: Option<PtPreset>,
    /// Preview exposure (EV100); `None`: the scene's own sun-lit level, else 0.
    pub ev100: Option<f32>,
}

impl Default for LookDevSettings {
    fn default() -> Self {
        let base = PtSettings::default();
        Self {
            scene: LookDevScene::Viewport,
            width: 320,
            height: 180,
            spp: 64,
            max_bounces: base.max_bounces,
            seed: base.seed,
            preset: None,
            ev100: None,
        }
    }
}

impl LookDevSettings {
    /// Checks the numbers against the panel's limits.
    ///
    /// # Errors
    ///
    /// The first setting out of range, as a sentence.
    pub fn validate(&self) -> Result<(), String> {
        let check = |name: &str, value: u32, max: u32| {
            if (1..=max).contains(&value) {
                Ok(())
            } else {
                Err(format!("`{name}` must be in 1..={max}, not {value}"))
            }
        };
        check("width", self.width, MAX_SIDE)?;
        check("height", self.height, MAX_SIDE)?;
        check("spp", self.spp, MAX_SPP)?;
        check("max_bounces", self.max_bounces, MAX_BOUNCES)?;
        if self.ev100.is_some_and(|ev| !ev.is_finite()) {
            return Err("`ev100` must be a finite number".into());
        }
        Ok(())
    }

    /// The tracer settings, and the preset table when a preset is set (its own sample count and
    /// path length unless `spp_given` / `bounces_given`).
    fn tracer_settings(&self, spp_given: bool, bounces_given: bool) -> (PtSettings, Option<PresetSettings>) {
        let base = PtSettings {
            spp: self.spp,
            max_bounces: self.max_bounces,
            seed: self.seed,
            ..PtSettings::default()
        };
        let Some(preset) = self.preset else {
            return (base, None);
        };
        let mut table = preset.settings_for(&base);
        if spp_given {
            table.spp = self.spp;
        }
        if bounces_given {
            table.max_bounces = self.max_bounces;
        }
        if table.spp < 2 {
            // One sample has no variance to judge.
            table.adaptive.enabled = false;
        }
        if table.adaptive.enabled {
            table.adaptive.min_spp = table.adaptive.min_spp.min(table.spp);
        }
        let settings = PtSettings {
            spp: table.spp,
            max_bounces: table.max_bounces,
            rr_start: table.rr_start,
            seed: self.seed,
            ..PtSettings::default()
        };
        (settings, Some(table))
    }
}

/// A failure the panel and the agent methods report: a stable code and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookDevError {
    /// One of [`BUSY`], [`NOTHING_TO_TRACE`], [`NO_GAME_CAMERA`], [`INVALID_SETTINGS`], [`SPAWN_FAILED`].
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl LookDevError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn rpc(&self) -> RpcError {
        let (rpc_code, regia_code, hint) = match self.code {
            BUSY => (
                protocol::INVALID_PARAMS,
                "F0002",
                "wait for it (editor.lookdev_status) or stop it (editor.lookdev_cancel)",
            ),
            NOTHING_TO_TRACE => (
                protocol::UNSUPPORTED,
                "F0011",
                "add a mesh to the level, or trace a named scene (cornell, furnace)",
            ),
            NO_GAME_CAMERA => (
                protocol::UNSUPPORTED,
                "F0011",
                "add a Camera to the level, or trace scene viewport (the editor camera)",
            ),
            INVALID_SETTINGS => (
                protocol::INVALID_PARAMS,
                "F0002",
                "see the limits in book/agents/editor-lookdev.md",
            ),
            _ => (protocol::INTERNAL_ERROR, "F0002", "see the editor log"),
        };
        let mut error = RpcError::new(rpc_code, regia_code, self.message.clone(), hint);
        if let Some(data) = error.data.as_mut() {
            data["lookdev_error"] = json!(self.code);
        }
        error
    }
}

/// The extraction options of a level trace: the runtime renderer's material clock, foliage wind
/// and floating origin when the world has one (as `seeing.reference_trace` takes them, ADR-2725).
fn extract_options(world: &World) -> regia_pt::extract::ExtractOptions {
    let runtime = world.get_resource::<regia_render::RuntimeRenderer>();
    regia_pt::extract::ExtractOptions {
        material_clock: runtime.and_then(|r| r.0.material_clock()),
        foliage_wind: runtime.and_then(|r| r.0.foliage_wind()),
        origin_offset: runtime.map_or([0.0; 3], |r| r.0.origin_offset()),
        ..regia_pt::extract::ExtractOptions::default()
    }
}

/// The scene `settings` asks for, with what it contains (`scene_info`).
///
/// # Errors
///
/// [`NOTHING_TO_TRACE`] for a level with no mesh; [`NO_GAME_CAMERA`] for `game` without a game
/// camera.
pub fn scene_for(world: &mut World, settings: &LookDevSettings) -> Result<(PtScene, Value), LookDevError> {
    let (width, height) = (settings.width, settings.height);
    let mut scene = match settings.scene {
        LookDevScene::Cornell => regia_pt::scenes::cornell_box(width, height, true),
        LookDevScene::Furnace => {
            regia_pt::scenes::furnace_sphere(regia_pt::PtMaterial::lambert([1.0; 3]), width.max(height))
        }
        LookDevScene::Viewport | LookDevScene::Game => {
            regia_scene::prelude::propagate_transforms(world);
            crate::mesh_source::resolve_mesh_sources(world);
            let mut frame = crate::render::extract(world);
            if settings.scene == LookDevScene::Viewport {
                let camera = world.get_resource::<EditorCamera>().cloned().unwrap_or_default();
                let view = ViewCamera::of(ViewKind::Perspective, &camera);
                frame.views = vec![regia_render::ExtractedView {
                    camera: regia_render::ExtractedCamera {
                        eye: view.eye.to_array(),
                        target: view.target.to_array(),
                        fov_y: view.fov_y,
                        near: view.near,
                        far: view.far,
                    },
                    viewport: [0.0, 0.0, 1.0, 1.0],
                    order: 0,
                    clear: true,
                    target: regia_render::ExtractedTarget::Window,
                    layer_mask: u32::MAX,
                    post: regia_render::PostSettings::default(),
                }];
            }
            let Some(view) = frame.window_views().first().map(|(index, _)| *index) else {
                return Err(LookDevError::new(
                    NO_GAME_CAMERA,
                    "the level has no game camera to trace through",
                ));
            };
            if frame.meshes.is_empty() {
                return Err(LookDevError::new(
                    NOTHING_TO_TRACE,
                    "the level has no mesh to trace",
                ));
            }
            regia_pt::extract::from_extracted_with(&frame, view, width, height, &extract_options(world))
        }
    };
    scene.camera.width = width;
    scene.camera.height = height;
    let info = json!({
        "scene": settings.scene.name(),
        "instances": scene.instances.len(),
        "meshes": scene.meshes.len(),
        "triangles": scene.meshes.iter().map(regia_pt::PtMesh::triangle_count).sum::<usize>(),
        "materials": scene.materials.len(),
        "terrain": scene.terrain.is_some(),
        "sun_lux": scene.sun.map(|s| s.illuminance),
        "local_lights": scene.lights.len(),
    });
    Ok((scene, info))
}

/// Preview exposure when none is given: the scene's own sun-lit level (as `seeing.reference_trace`).
fn default_ev100(scene: &PtScene) -> f32 {
    match scene.sun {
        Some(sun) if sun.illuminance > 1000.0 => {
            regia_render::ev100_from_luminance(sun.illuminance * 0.18 / std::f32::consts::PI)
        }
        _ => 0.0,
    }
}

/// What the trace thread and the job share.
#[derive(Default)]
struct Shared {
    done: AtomicU32,
    cancel: AtomicBool,
    result: Mutex<Option<Result<Value, String>>>,
}

impl Shared {
    fn result(&self) -> Option<Result<Value, String>> {
        self.result.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// One running (or finished) trace.
pub struct LookDevJob {
    id: u64,
    settings: LookDevSettings,
    spp: u32,
    out_dir: PathBuf,
    info: Value,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl LookDevJob {
    /// The job's number (counts up per editor session).
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The settings it traces.
    #[must_use]
    pub fn settings(&self) -> &LookDevSettings {
        &self.settings
    }

    /// Samples per pixel it traces (the preset's when one is set).
    #[must_use]
    pub fn spp(&self) -> u32 {
        self.spp
    }

    /// Samples per pixel done so far.
    #[must_use]
    pub fn spp_done(&self) -> u32 {
        self.shared.done.load(Ordering::Relaxed)
    }

    /// Where it writes.
    #[must_use]
    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    /// Whether the trace has its result (done, cancelled or failed).
    #[must_use]
    pub fn finished(&self) -> bool {
        self.shared.result().is_some()
    }

    /// `running`, `done`, `cancelled` or `failed`.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self.shared.result() {
            None => "running",
            Some(Ok(_)) if self.shared.cancel.load(Ordering::Relaxed) => "cancelled",
            Some(Ok(_)) => "done",
            Some(Err(_)) => "failed",
        }
    }

    /// The preview PNG of a finished trace.
    #[must_use]
    pub fn preview_png(&self) -> Option<PathBuf> {
        match self.shared.result() {
            Some(Ok(out)) => out["preview_png"].as_str().map(PathBuf::from),
            _ => None,
        }
    }

    /// Asks the trace to stop after the sample in flight (what was traced is still written).
    /// Returns `false` when it had already finished.
    pub fn cancel(&self) -> bool {
        if self.finished() {
            return false;
        }
        self.shared.cancel.store(true, Ordering::Relaxed);
        true
    }

    /// Blocks until the trace thread returns (agents' `wait: true`, tests).
    pub fn wait(&mut self) {
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }

    /// The status object of `editor.lookdev_status`.
    #[must_use]
    pub fn status_json(&self) -> Value {
        let spp_done = self.spp_done();
        let fraction = if self.spp == 0 {
            0.0
        } else {
            f64::from(spp_done.min(self.spp)) / f64::from(self.spp)
        };
        let mut out = json!({
            "job": self.id,
            "state": self.state(),
            "running": !self.finished(),
            "scene": self.settings.scene.name(),
            "width": self.settings.width,
            "height": self.settings.height,
            "preset": self.settings.preset.map(PtPreset::name),
            "seed": self.settings.seed,
            "spp": self.spp,
            "spp_done": spp_done,
            "fraction": fraction,
            "out_dir": self.out_dir.display().to_string(),
            "scene_info": self.info,
        });
        match self.shared.result() {
            Some(Ok(done)) => {
                if let (Some(o), Some(d)) = (out.as_object_mut(), done.as_object()) {
                    o.extend(d.clone());
                }
            }
            Some(Err(error)) => out["error"] = json!(error),
            None => {}
        }
        out
    }
}

impl Drop for LookDevJob {
    /// A trace still running when the editor drops it (the editor closing) is cancelled and its
    /// thread joined within `JOIN_WAIT`, so its device does not outlive the editor's exit.
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        let Some(handle) = self.thread.take() else {
            return;
        };
        let deadline = std::time::Instant::now() + JOIN_WAIT;
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            tracing::warn!(target: "regia_editor", "a look-dev trace thread is still running at exit");
        }
    }
}

/// The trace thread's body: its own device, the progressive trace (or the preset's pipeline),
/// then `reference.exr` (every AOV), `reference_preview.png`, `reference.json` and, for a preset
/// that post-processed, `preview.png`.
fn run_trace(
    scene: &PtScene,
    settings: &PtSettings,
    preset: Option<&PresetSettings>,
    out_dir: &Path,
    ev100: f32,
    shared: &Shared,
) -> Result<Value, String> {
    let _span = tracing::info_span!(target: "regia_editor", "lookdev_trace", spp = settings.spp).entered();
    std::fs::create_dir_all(out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    let cancelled = || shared.cancel.load(Ordering::Relaxed);
    let started = std::time::Instant::now();
    let device = crate::render::headless_device().map_err(|e| format!("no GPU for the trace: {e}"))?;
    // A cancel before the first sample is still `cancelled` (not `failed`), with no files.
    let nothing_traced = || Ok(json!({ "traced": false }));
    if cancelled() {
        return nothing_traced();
    }
    let tracer = PathTracer::new(&device).map_err(|e| e.to_string())?;
    if cancelled() {
        return nothing_traced();
    }
    let mut progress = |done: u32| {
        shared.done.store(done, Ordering::Relaxed);
        !shared.cancel.load(Ordering::Relaxed)
    };
    let (image, post) = match preset {
        Some(table) => {
            let mut session = tracer
                .begin_with_quality(scene, settings, &table.quality)
                .map_err(|e| e.to_string())?;
            let result =
                regia_pt::run_preset(&mut session, table, &mut progress).map_err(|e| e.to_string())?;
            match result.raw {
                Some(raw) => (raw, Some(result.image)),
                None => (result.image, None),
            }
        }
        None => {
            let mut session = tracer.begin(scene, settings).map_err(|e| e.to_string())?;
            session
                .run_with(settings.spp, &mut progress)
                .map_err(|e| e.to_string())?;
            (session.image().map_err(|e| e.to_string())?, None)
        }
    };
    let exr = out_dir.join("reference.exr");
    let raw_png = out_dir.join("reference_preview.png");
    let meta = out_dir.join("reference.json");
    image.write_exr(&exr, &[]).map_err(|e| e.to_string())?;
    image
        .write_preview_png(&raw_png, Aov::Beauty, ev100)
        .map_err(|e| e.to_string())?;
    image.write_metadata_json(&meta).map_err(|e| e.to_string())?;
    let preview_png = match post {
        Some(post) => {
            let png = out_dir.join("preview.png");
            post.write_preview_png(&png, Aov::Beauty, ev100)
                .map_err(|e| e.to_string())?;
            png
        }
        None => raw_png.clone(),
    };
    Ok(json!({
        "exr": exr.display().to_string(),
        "preview_png": preview_png.display().to_string(),
        "raw_preview_png": raw_png.display().to_string(),
        "metadata_json": meta.display().to_string(),
        "ev100": ev100,
        "wall_ms": started.elapsed().as_secs_f64() * 1000.0,
        "traced": true,
    }))
}

/// The Look-dev panel's state (a world resource shared by the panel and the agent methods).
#[derive(Resource, Default)]
pub struct LookDevPanel {
    /// The settings the panel shows (and the next trace uses).
    pub settings: LookDevSettings,
    /// The settings' `spp` was set by hand (a preset keeps it rather than its own).
    pub spp_set: bool,
    /// The settings' `max_bounces` was set by hand.
    pub bounces_set: bool,
    /// The output directory typed in the panel (`""`: [`Self::default_out_dir`]).
    pub out: String,
    job: Option<LookDevJob>,
    next: u64,
    last_error: Option<LookDevError>,
    preview: Option<(PathBuf, egui::TextureHandle)>,
}

impl LookDevPanel {
    /// The current or last trace.
    #[must_use]
    pub fn job(&self) -> Option<&LookDevJob> {
        self.job.as_ref()
    }

    /// Whether a trace is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.job.as_ref().is_some_and(|job| !job.finished())
    }

    /// Where trace `id` writes by default: `<project>/.regia/lookdev/trace-<pid>-<id>`, or the
    /// temp directory without a project.
    #[must_use]
    pub fn default_out_dir(project: Option<&Path>, id: u64) -> PathBuf {
        let name = format!("trace-{}-{id}", std::process::id());
        match project {
            Some(root) => root.join(".regia").join("lookdev").join(name),
            None => std::env::temp_dir().join("regia_lookdev").join(name),
        }
    }

    /// Starts a trace of `settings` (the panel's own unless the agent passed others).
    ///
    /// # Errors
    ///
    /// [`BUSY`] while a trace runs; [`INVALID_SETTINGS`] for a setting out of range; what
    /// [`scene_for`] refuses; [`SPAWN_FAILED`] when the thread cannot start.
    pub fn start(
        world: &mut World,
        settings: LookDevSettings,
        out: Option<PathBuf>,
    ) -> Result<u64, LookDevError> {
        if world.get_resource::<Self>().is_some_and(Self::is_running) {
            return Err(LookDevError::new(BUSY, "a look-dev trace is already running"));
        }
        settings
            .validate()
            .map_err(|message| LookDevError::new(INVALID_SETTINGS, message))?;
        let project = crate::mesh_source::project_root(world);
        let (scene, info) = scene_for(world, &settings)?;
        let mut panel = world.get_resource_or_insert_with(Self::default);
        let (pt_settings, preset) = settings.tracer_settings(panel.spp_set, panel.bounces_set);
        panel.next += 1;
        let id = panel.next;
        let out_dir = out.unwrap_or_else(|| Self::default_out_dir(project.as_deref(), id));
        let ev100 = settings.ev100.unwrap_or_else(|| default_ev100(&scene));
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        let dir = out_dir.clone();
        let table = preset;
        let thread_settings = pt_settings;
        let handle = std::thread::Builder::new()
            .name(format!("regia_lookdev {id}"))
            .spawn(move || {
                // A panic still ends the job (`failed`), so the panel never shows `running` for ever.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_trace(&scene, &thread_settings, table.as_ref(), &dir, ev100, &worker)
                }))
                .unwrap_or_else(|_| Err("the trace thread panicked".to_string()));
                *worker.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
            })
            .map_err(|e| LookDevError::new(SPAWN_FAILED, format!("could not start the trace thread: {e}")))?;
        panel.job = Some(LookDevJob {
            id,
            spp: preset.as_ref().map_or(pt_settings.spp, |t| t.spp),
            settings,
            out_dir,
            info,
            shared,
            thread: Some(handle),
        });
        panel.last_error = None;
        Ok(id)
    }

    /// Asks the running trace to stop. Returns `false` when none was running.
    pub fn cancel(&mut self) -> bool {
        self.job.as_ref().is_some_and(LookDevJob::cancel)
    }

    /// The status of `editor.lookdev_status`: the current or last job (`state: "idle"` before the
    /// first) and the panel's settings.
    #[must_use]
    pub fn status_json(&self) -> Value {
        let mut out = match &self.job {
            Some(job) => job.status_json(),
            None => json!({ "state": "idle", "running": false }),
        };
        let s = &self.settings;
        out["settings"] = json!({
            "scene": s.scene.name(),
            "width": s.width,
            "height": s.height,
            "spp": s.spp,
            "max_bounces": s.max_bounces,
            "seed": s.seed,
            "preset": s.preset.map(PtPreset::name),
            "ev100": s.ev100,
        });
        if let Some(error) = &self.last_error {
            out["last_error"] = json!({ "code": error.code, "message": error.message });
        }
        out
    }
}

/// The settings of an `editor.lookdev_trace` call: the panel's, with what the call gives.
/// Returns them with whether `spp` and `max_bounces` were given.
fn settings_from_params(
    base: &LookDevSettings,
    params: &Value,
) -> Result<(LookDevSettings, bool, bool), RpcError> {
    let mut settings = base.clone();
    if let Some(name) = str_param(params, "scene") {
        settings.scene = LookDevScene::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown `scene` {name:?}: viewport, game, cornell or furnace"
            ))
        })?;
    }
    if let Some(v) = u32_param(params, "width")? {
        settings.width = v;
    }
    if let Some(v) = u32_param(params, "height")? {
        settings.height = v;
    }
    let spp = u32_param(params, "spp")?;
    if let Some(v) = spp {
        settings.spp = v;
    }
    let bounces = u32_param(params, "max_bounces")?;
    if let Some(v) = bounces {
        settings.max_bounces = v;
    }
    if let Some(v) = u32_param(params, "seed")? {
        settings.seed = v;
    }
    match params.get("preset") {
        None | Some(Value::Null) => {}
        Some(Value::String(name)) if name == "none" => settings.preset = None,
        Some(Value::String(name)) => {
            settings.preset = Some(PtPreset::from_name(name).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "unknown `preset` {name:?}: none, draft, preview, final or reference"
                ))
            })?);
        }
        Some(_) => return Err(RpcError::invalid_params("`preset` must be a string")),
    }
    match params.get("ev100") {
        None | Some(Value::Null) => {}
        Some(v) => {
            settings.ev100 = Some(
                v.as_f64()
                    .filter(|n| n.is_finite())
                    .map(|n| n as f32)
                    .ok_or_else(|| RpcError::invalid_params("`ev100` must be a finite number"))?,
            );
        }
    }
    settings.validate().map_err(RpcError::invalid_params)?;
    Ok((settings, spp.is_some(), bounces.is_some()))
}

/// `editor.lookdev_trace`.
fn lookdev_trace_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let base = world
        .get_resource::<LookDevPanel>()
        .map(|p| p.settings.clone())
        .unwrap_or_default();
    let (settings, spp_given, bounces_given) = settings_from_params(&base, params)?;
    let wait = bool_param(params, "wait");
    // The panel's Output dir field (ADR-7747): absent or blank = the default directory; a relative
    // path is under the project. Every other path goes through the output-path rule (ADR-1816,
    // `allowed_request_path`: under the project, the temp or working directory or an
    // `output_roots` entry, never `..`; review sprint-w11-rev-e-k #2).
    let out_text = match params.get("out") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.trim().to_string()).filter(|t| !t.is_empty()),
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`out` must be a directory path (string), got {other}"
            )));
        }
    };
    let out = match out_text.as_deref() {
        Some(text) => Some(crate::agent_ext::allowed_request_path(world, "out", text)?),
        None => None,
    };
    {
        let mut panel = world.get_resource_or_insert_with(LookDevPanel::default);
        // What an agent asked for is what the panel shows next (one shared state).
        panel.settings = settings.clone();
        panel.spp_set = spp_given;
        panel.bounces_set = bounces_given;
        if let Some(out) = &out {
            panel.out = out.display().to_string();
        }
    }
    if let Err(error) = LookDevPanel::start(world, settings, out) {
        world.resource_mut::<LookDevPanel>().last_error = Some(error.clone());
        return Err(error.rpc());
    }
    let mut panel = world.resource_mut::<LookDevPanel>();
    if wait && let Some(job) = panel.job.as_mut() {
        job.wait();
    }
    Ok(panel.status_json())
}

/// `editor.lookdev_status`.
fn lookdev_status_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(world
        .get_resource_or_insert_with(LookDevPanel::default)
        .status_json())
}

/// `editor.lookdev_cancel`.
fn lookdev_cancel_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.get_resource_or_insert_with(LookDevPanel::default);
    let cancelled = panel.cancel();
    if bool_param(params, "wait")
        && let Some(job) = panel.job.as_mut()
    {
        job.wait();
    }
    let mut status = panel.status_json();
    status["cancelled"] = json!(cancelled);
    Ok(status)
}

/// The Look-dev panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.lookdev_trace",
                tool: "lookdev_trace",
                description: "Trace the open level with the reference path tracer from the Look-dev panel \
                    (specs/27 § 1.1): the editor camera's view (`viewport`), the level's game camera \
                    (`game`) or a tracer test scene, on its own device in the background. Returns the \
                    status (editor.lookdev_status's shape). Unset settings keep the panel's. One trace \
                    at a time (data.lookdev_error `lookdev_busy`).",
                params: &[
                    ParamSchema {
                        name: "scene",
                        kind: "string",
                        required: false,
                        description: "`viewport` (the editor camera, default), `game` (the level's game camera), `cornell` or `furnace`.",
                    },
                    ParamSchema {
                        name: "width",
                        kind: "number",
                        required: false,
                        description: "Image width, 1..=4096 (panel default 320).",
                    },
                    ParamSchema {
                        name: "height",
                        kind: "number",
                        required: false,
                        description: "Image height, 1..=4096 (panel default 180).",
                    },
                    ParamSchema {
                        name: "spp",
                        kind: "number",
                        required: false,
                        description: "Samples per pixel, 1..=65536 (default 64; a preset's own unless given).",
                    },
                    ParamSchema {
                        name: "max_bounces",
                        kind: "number",
                        required: false,
                        description: "Maximum path length, 1..=64 (default 12).",
                    },
                    ParamSchema {
                        name: "seed",
                        kind: "number",
                        required: false,
                        description: "Trace seed (same seed, same image on one device).",
                    },
                    ParamSchema {
                        name: "preset",
                        kind: "string",
                        required: false,
                        description: "`none`, `draft`, `preview`, `final` or `reference` (book/agents/pt-presets.md).",
                    },
                    ParamSchema {
                        name: "ev100",
                        kind: "number",
                        required: false,
                        description: "Preview exposure; default the scene's sun-lit level, else 0.",
                    },
                    ParamSchema {
                        name: "wait",
                        kind: "boolean",
                        required: false,
                        description: "Block the editor until the trace finishes (small traces only). Default false.",
                    },
                    ParamSchema {
                        name: "out",
                        kind: "string",
                        required: false,
                        description: "Output directory (the panel's Output dir); relative = under the project; \
                            absent or blank = .regia/lookdev/trace-<pid>-<n>.",
                    },
                ],
                result: "{ job, state, running, scene, width, height, preset, seed, spp, spp_done, fraction, out_dir, scene_info, exr?, preview_png?, raw_preview_png?, metadata_json?, ev100?, wall_ms?, traced?, error?, settings }",
                mutating: true,
                permission: Permission::Assets,
            },
            lookdev_trace_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.lookdev_status",
                tool: "lookdev_status",
                description: "The Look-dev panel's current or last trace: state (`idle`, `running`, `done`, \
                    `cancelled`, `failed`), spp_done of spp, the files (exr with every specs/27 § 1.2 AOV, \
                    preview_png, metadata_json) or the error, scene_info, and the panel's settings.",
                params: &[],
                result: "{ job?, state, running, spp?, spp_done?, fraction?, out_dir?, exr?, preview_png?, error?, settings, last_error? }",
                mutating: false,
                permission: Permission::Read,
            },
            lookdev_status_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.lookdev_cancel",
                tool: "lookdev_cancel",
                description: "Stop the running look-dev trace after the sample in flight; what was traced is \
                    still written (state `cancelled`). `cancelled` is false when no trace was running.",
                params: &[ParamSchema {
                    name: "wait",
                    kind: "boolean",
                    required: false,
                    description: "Block until the trace thread has stopped. Default false.",
                }],
                result: "{ cancelled, job, state, ... }",
                mutating: true,
                permission: Permission::Assets,
            },
            lookdev_cancel_method as Handler,
        ),
    ]
}

const GOOD: egui::Color32 = egui::Color32::from_rgb(110, 200, 120);
const BAD: egui::Color32 = egui::Color32::from_rgb(230, 100, 90);

/// What a click in the panel asks for (run after the panel's borrow ends).
enum Action {
    Trace,
    Cancel,
}

/// The Look-dev panel: scene, size, samples, bounces, seed and preset; Trace / Cancel; progress;
/// the preview and the written files.
pub fn lookdev_panel(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::info_span!(target: "regia_editor", "lookdev_panel").entered();
    let mut action = None;
    {
        let mut panel = world.get_resource_or_insert_with(LookDevPanel::default);
        draw(ui, &mut panel, &mut action);
    }
    match action {
        Some(Action::Trace) => {
            let (settings, out) = {
                let panel = world.resource::<LookDevPanel>();
                let out = (!panel.out.trim().is_empty()).then(|| PathBuf::from(panel.out.trim()));
                (panel.settings.clone(), out)
            };
            if let Err(error) = LookDevPanel::start(world, settings, out) {
                world.resource_mut::<LookDevPanel>().last_error = Some(error);
            }
        }
        Some(Action::Cancel) => {
            world.resource_mut::<LookDevPanel>().cancel();
        }
        None => {}
    }
}

fn draw(ui: &mut egui::Ui, panel: &mut LookDevPanel, action: &mut Option<Action>) {
    ui.heading("Look-dev");
    ui.label("Reference path trace of the level. Editor-only helpers (preview sun, proxies) are not traced.");
    let running = panel.is_running();
    ui.add_enabled_ui(!running, |ui| {
        egui::Grid::new("lookdev_settings").num_columns(2).show(ui, |ui| {
            ui.label("Scene");
            egui::ComboBox::from_id_salt("lookdev_scene")
                .selected_text(panel.settings.scene.name())
                .show_ui(ui, |ui| {
                    for scene in LookDevScene::ALL {
                        ui.selectable_value(&mut panel.settings.scene, scene, scene.name());
                    }
                });
            ui.end_row();
            ui.label("Size");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut panel.settings.width).range(1..=MAX_SIDE));
                ui.label("×");
                ui.add(egui::DragValue::new(&mut panel.settings.height).range(1..=MAX_SIDE));
            });
            ui.end_row();
            ui.label("Preset");
            egui::ComboBox::from_id_salt("lookdev_preset")
                .selected_text(panel.settings.preset.map_or("none", PtPreset::name))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut panel.settings.preset, None, "none");
                    for name in ["draft", "preview", "final", "reference"] {
                        if let Some(preset) = PtPreset::from_name(name) {
                            ui.selectable_value(&mut panel.settings.preset, Some(preset), name);
                        }
                    }
                });
            ui.end_row();
            ui.label("Samples / pixel");
            if ui
                .add(egui::DragValue::new(&mut panel.settings.spp).range(1..=MAX_SPP))
                .changed()
            {
                panel.spp_set = true;
            }
            ui.end_row();
            ui.label("Max bounces");
            if ui
                .add(egui::DragValue::new(&mut panel.settings.max_bounces).range(1..=MAX_BOUNCES))
                .changed()
            {
                panel.bounces_set = true;
            }
            ui.end_row();
            ui.label("Seed");
            ui.add(egui::DragValue::new(&mut panel.settings.seed));
            ui.end_row();
            ui.label("Output dir");
            ui.add(egui::TextEdit::singleline(&mut panel.out).hint_text(".regia/lookdev/trace-<pid>-<n>"));
            ui.end_row();
        });
    });
    ui.horizontal(|ui| {
        if ui.add_enabled(!running, egui::Button::new("Trace")).clicked() {
            *action = Some(Action::Trace);
        }
        if ui.add_enabled(running, egui::Button::new("Cancel")).clicked() {
            *action = Some(Action::Cancel);
        }
    });
    if let Some(error) = &panel.last_error {
        ui.colored_label(BAD, format!("{}: {}", error.code, error.message));
    }
    let Some(job) = panel.job.as_ref() else {
        return;
    };
    let status = job.status_json();
    if running {
        let fraction = status["fraction"].as_f64().unwrap_or(0.0) as f32;
        ui.add(egui::ProgressBar::new(fraction).text(format!("{} / {} spp", job.spp_done(), job.spp())));
        // Keep the progress moving while nothing else repaints.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
        return;
    }
    match job.state() {
        "failed" => {
            ui.colored_label(BAD, format!("failed: {}", status["error"].as_str().unwrap_or("")));
        }
        state => {
            let wall = status["wall_ms"].as_f64().unwrap_or(0.0);
            ui.colored_label(
                GOOD,
                format!("{state}: {} spp in {:.1} s", job.spp_done(), wall / 1000.0),
            );
        }
    }
    for key in ["exr", "preview_png", "metadata_json"] {
        if let Some(path) = status[key].as_str() {
            ui.horizontal(|ui| {
                ui.label(key);
                if ui.link(path).on_hover_text("Copy the path").clicked() {
                    ui.ctx().copy_text(path.to_string());
                }
            });
        }
    }
    let Some(png) = job.preview_png() else {
        return;
    };
    if panel.preview.as_ref().is_none_or(|(path, _)| *path != png) {
        match std::fs::read(&png)
            .map_err(|e| e.to_string())
            .and_then(|bytes| crate::reference::decode_image(&bytes))
        {
            Ok(image) => {
                let texture = ui
                    .ctx()
                    .load_texture("lookdev_preview", image, egui::TextureOptions::LINEAR);
                panel.preview = Some((png, texture));
            }
            Err(error) => {
                ui.colored_label(BAD, format!("preview: {error}"));
                return;
            }
        }
    }
    if let Some((_, texture)) = &panel.preview {
        let size = texture.size_vec2();
        let scale = (ui.available_width() / size.x.max(1.0)).min(1.0);
        ui.image((texture.id(), size * scale));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_gpu(job: &LookDevJob) -> bool {
        job.status_json()["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("no GPU"))
    }

    #[test]
    fn spec27_1_1_scene_names_round_trip_and_settings_are_checked() {
        for scene in LookDevScene::ALL {
            assert_eq!(LookDevScene::from_name(scene.name()), Some(scene));
        }
        assert_eq!(LookDevScene::from_name("nope"), None);
        let ok = LookDevSettings::default();
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(ok.max_bounces, 12, "specs/27 § 1.1: 12 bounces by default");
        for bad in [
            LookDevSettings {
                width: 0,
                ..ok.clone()
            },
            LookDevSettings {
                height: MAX_SIDE + 1,
                ..ok.clone()
            },
            LookDevSettings { spp: 0, ..ok.clone() },
            LookDevSettings {
                max_bounces: MAX_BOUNCES + 1,
                ..ok.clone()
            },
            LookDevSettings {
                ev100: Some(f32::NAN),
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn spec27_1_1_agent_params_override_the_panel_and_are_validated() {
        let base = LookDevSettings::default();
        let (s, spp, bounces) = settings_from_params(
            &base,
            &json!({ "scene": "cornell", "width": 64, "spp": 8, "preset": "draft", "ev100": 2.5 }),
        )
        .expect("valid");
        assert_eq!(s.scene, LookDevScene::Cornell);
        assert_eq!((s.width, s.height, s.spp), (64, 180, 8));
        assert_eq!(s.preset, PtPreset::from_name("draft"));
        assert_eq!(s.ev100, Some(2.5));
        assert!(spp && !bounces);
        // A preset keeps its own sample count unless spp was given.
        let (pt, table) = s.tracer_settings(true, false);
        assert_eq!((pt.spp, table.as_ref().map(|t| t.spp)), (8, Some(8)));
        let (_, table) = s.tracer_settings(false, false);
        let own = PtPreset::from_name("draft").map(|p| p.settings_for(&PtSettings::default()).spp);
        assert_eq!(table.map(|t| t.spp), own);
        for bad in [
            json!({ "scene": "moon" }),
            json!({ "width": 0 }),
            json!({ "spp": -1 }),
            json!({ "preset": "ultra" }),
            json!({ "preset": 3 }),
            json!({ "ev100": "bright" }),
        ] {
            assert!(settings_from_params(&base, &bad).is_err(), "{bad}");
        }
        let (none, ..) = settings_from_params(&s, &json!({ "preset": "none" })).expect("none");
        assert_eq!(none.preset, None);
    }

    #[test]
    fn spec27_1_1_an_empty_level_has_nothing_to_trace() {
        let mut world = World::new();
        let error = scene_for(&mut world, &LookDevSettings::default()).expect_err("no mesh");
        assert_eq!(error.code, NOTHING_TO_TRACE);
        let game = LookDevSettings {
            scene: LookDevScene::Game,
            ..LookDevSettings::default()
        };
        let error = scene_for(&mut world, &game).expect_err("no camera");
        assert_eq!(error.code, NO_GAME_CAMERA);
        // Through the agent method: the stable code in data, nothing started.
        let error = lookdev_trace_method(&mut world, &json!({})).expect_err("refused");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["lookdev_error"].as_str()),
            Some(NOTHING_TO_TRACE)
        );
        let status = lookdev_status_method(&mut world, &json!({})).expect("status");
        assert_eq!(status["state"], "idle");
        assert_eq!(status["last_error"]["code"], NOTHING_TO_TRACE);
    }

    #[test]
    fn spec27_1_1_the_viewport_trace_looks_through_the_editor_camera() {
        let mut world = World::new();
        let camera = EditorCamera {
            target: regia_core::math::Vec3::new(1.0, 2.0, 3.0),
            distance: 5.0,
            ..EditorCamera::default()
        };
        world.insert_resource(camera.clone());
        let (vertices, indices) = regia_render::box_mesh([0.5; 3]);
        let mesh = Arc::new(regia_render::CpuMesh { vertices, indices });
        world.spawn((
            regia_render::MeshRenderer::new(mesh, Arc::new(regia_render::LabMaterial::default())),
            regia_scene::GlobalTransform(regia_core::glam::Affine3A::IDENTITY),
        ));
        let settings = LookDevSettings {
            width: 32,
            height: 18,
            ..LookDevSettings::default()
        };
        let (scene, info) = scene_for(&mut world, &settings).expect("a level with a cube");
        let eye = camera.eye();
        let traced = scene.camera.eye;
        assert!(
            (traced[0] - eye.x).abs() < 1e-4
                && (traced[1] - eye.y).abs() < 1e-4
                && (traced[2] - eye.z).abs() < 1e-4,
            "traced from {traced:?}, the editor camera is at {eye:?}"
        );
        assert_eq!((scene.camera.width, scene.camera.height), (32, 18));
        assert_eq!(info["scene"], "viewport");
        assert!(info["instances"].as_u64().unwrap_or(0) >= 1, "{info}");
    }

    #[test]
    fn spec27_1_1_a_named_trace_runs_writes_its_files_and_refuses_a_second_while_running() {
        let mut world = World::new();
        let status = lookdev_trace_method(
            &mut world,
            &json!({ "scene": "cornell", "width": 16, "height": 16, "spp": 4, "max_bounces": 3, "wait": true }),
        )
        .expect("started");
        let panel = world.resource::<LookDevPanel>();
        let job = panel.job().expect("a job");
        if no_gpu(job) {
            // Review rc1-rev-editor-k finding 9: without a GPU this test returned green having
            // checked nothing. The GPU-missing path is checked instead (the GPU half runs on the
            // card hosts): the job fails with the stable error, writes nothing, and a finished
            // job does not hold the panel busy or cancel.
            eprintln!(
                "no GPU: checking the failure path of spec27_1_1_a_named_trace_runs_writes_its_files: {}",
                status["error"]
            );
            assert_eq!(status["state"], "failed", "{status}");
            assert_eq!(status["running"], false, "{status}");
            assert_eq!(status["spp_done"], 0, "{status}");
            assert!(
                status["error"]
                    .as_str()
                    .is_some_and(|e| e.starts_with("no GPU for the trace")),
                "{status}"
            );
            for key in ["exr", "preview_png", "metadata_json"] {
                assert!(status.get(key).is_none(), "{key} without a trace: {status}");
            }
            let out = job.out_dir().to_path_buf();
            let again = lookdev_trace_method(&mut world, &json!({ "scene": "furnace", "wait": true }))
                .expect("a failed job does not hold the panel busy");
            assert_eq!(again["job"], 2, "{again}");
            assert_eq!(again["state"], "failed", "{again}");
            let cancel = lookdev_cancel_method(&mut world, &json!({})).expect("cancel");
            assert_eq!(cancel["cancelled"], false, "a finished job has nothing to cancel");
            let _ = std::fs::remove_dir_all(out);
            if let Some(dir) = world
                .resource::<LookDevPanel>()
                .job()
                .map(|j| j.out_dir().to_path_buf())
            {
                let _ = std::fs::remove_dir_all(dir);
            }
            return;
        }
        assert_eq!(status["state"], "done", "{status}");
        assert_eq!(status["spp_done"], 4, "{status}");
        for key in ["exr", "preview_png", "metadata_json"] {
            let path = status[key].as_str().unwrap_or_default();
            assert!(Path::new(path).is_file(), "{key}: {path}");
        }
        assert_eq!(
            panel.settings.scene,
            LookDevScene::Cornell,
            "the panel shows what the agent ran"
        );
        let out = job.out_dir().to_path_buf();

        // A long trace, then a second start: refused with lookdev_busy; cancel stops it.
        let started = lookdev_trace_method(
            &mut world,
            &json!({ "scene": "cornell", "width": 16, "height": 16, "spp": 65536 }),
        )
        .expect("long trace");
        assert_eq!(started["job"], 2);
        let busy = lookdev_trace_method(&mut world, &json!({ "scene": "furnace" })).expect_err("busy");
        assert_eq!(
            busy.data.as_ref().and_then(|d| d["lookdev_error"].as_str()),
            Some(BUSY)
        );
        let cancelled = lookdev_cancel_method(&mut world, &json!({ "wait": true })).expect("cancel");
        assert_eq!(cancelled["cancelled"], true, "{cancelled}");
        assert_eq!(cancelled["state"], "cancelled", "{cancelled}");
        assert!(
            cancelled["spp_done"].as_u64().unwrap_or(0) < 65_536,
            "{cancelled}"
        );
        let again = lookdev_cancel_method(&mut world, &json!({})).expect("cancel again");
        assert_eq!(again["cancelled"], false);
        let _ = std::fs::remove_dir_all(out);
        if let Some(dir) = world
            .resource::<LookDevPanel>()
            .job()
            .map(|j| j.out_dir().to_path_buf())
        {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn the_three_methods_are_registered_with_documented_params() {
        let names: Vec<&str> = methods().iter().map(|(s, _)| s.name).collect();
        assert_eq!(
            names,
            [
                "editor.lookdev_trace",
                "editor.lookdev_status",
                "editor.lookdev_cancel"
            ]
        );
        for (schema, _) in methods() {
            assert!(!schema.description.is_empty());
            for param in schema.params {
                assert!(!param.description.is_empty(), "{}.{}", schema.name, param.name);
            }
        }
    }
}
