// SPDX-License-Identifier: MPL-2.0
//! In-game feedback and bug reports (book `user-guide/feedback.md`, agent page
//! `agents/feedback.md`).
//!
//! The player opens a small prompt from the game's menu (a "Send feedback" entry the host adds,
//! [`spawn_feedback_button`] or [`open_prompt`]), or from a key the game binds to the
//! [`FEEDBACK_ACTION`] input action (no default binding: DO27 uses the F-keys). The prompt has
//! one text field and a category (bug / performance / visual / other). On Send, one bundle file
//! (a `.zip`, see [`bundle`]) is written to the feedback folder (`user://feedback`): the
//! screenshot taken when the prompt opened, the note, the last 30 s of frame times and the hitch
//! report, GPU/driver/OS/CPU/RAM, the active settings and preset, `render.problems`, the log
//! tail, the build identity and whatever the game added through [`FeedbackHost`] (match id, map,
//! player count, position). The game keeps running: nothing is paused, networking is untouched.
//!
//! On a panic the same bundle is written automatically, without a screenshot ([`crash`]); on a
//! native crash (a segfault, a driver crash) too, with a minidump ([`native`], Linux, needs
//! [`run_crash_monitor_if_requested`] first in `main`). The next launch shows where it is.
//!
//! **Privacy:** the engine writes local files only and uploads nothing. A game that wants to
//! send bundles reads [`FeedbackWritten`] / [`pending_bundles`] and uploads them
//! itself, after asking the player.
//!
//! Cost when unused: one frame sample pushed into a preallocated ring per frame (no allocation),
//! a change check on the settings, the renderer's problem list and the host fields, and a run
//! condition that keeps the prompt system idle until something opens it.
//!
//! ```
//! use regia_app::prelude::*;
//! use regia_framework::feedback::{FeedbackHost, FeedbackPlugin};
//!
//! let mut app = App::new();
//! app.add_plugins(FeedbackPlugin::default());
//! // The game adds its state when it changes (a crash bundle carries these too):
//! let mut host = app.world_mut().resource_mut::<FeedbackHost>();
//! host.set("match_id", "m-1234");
//! host.set("map", "mars_outpost");
//! host.set("player_count", "6");
//! ```

pub mod bundle;
pub mod crash;
pub mod native;
pub mod pipeline_cache;
pub mod prompt;
pub mod zip;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use regia_app::RealTime;
use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_input::{Actions, Keyboard};
use regia_platform::{ButtonState, KeyboardInput, MountPoint, Vfs, VfsPath};
use regia_render::{CapturedImage, FramePathCounters, RuntimeRenderer, ScreenshotRequests};
use regia_ui::interaction::{UiEvent, UiEventKind};
use regia_ui::widgets::{TextInput, spawn_button};

pub use bundle::{
    BundleFile, BundleInfo, BundleInput, CrashDetails, FRAME_RING_CAPACITY, FeedbackCategory, FrameRing,
    FrameSample, NativeCrash, RenderTimingAbsence, RenderTimingSample, SystemInfo, build_bundle,
    hitch_report, list_bundles, read_bundle, write_bundle,
};
pub use crash::{
    install_crash_hook, mark_announced, set_summary_source, unannounced_crashes, write_crash_bundle,
    write_device_lost_bundle, write_validation_breadcrumb_bundle,
};
pub use native::{NativeCrashStatus, native_crash_status, run_crash_monitor_if_requested};
pub use pipeline_cache::PipelineCacheAttachment;
pub use prompt::{FeedbackMenuButton, PromptButton, PromptNote};

use crate::settings::GameSettings;

/// The input action that opens the prompt. It has **no default binding**: a game that wants a
/// key binds it in its `InputConfig` (and the player can rebind it like any action).
pub const FEEDBACK_ACTION: &str = "Feedback";

/// Frames to wait for the screenshot before opening the prompt without one.
pub const SCREENSHOT_TIMEOUT_FRAMES: u32 = 90;

/// Seconds a "saved" toast stays up.
pub const TOAST_SECONDS: f64 = 8.0;
/// Seconds the next-launch crash notice stays up.
pub const CRASH_NOTICE_SECONDS: f64 = 15.0;

/// A menu entry descriptor for the game's pause or settings menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackMenuItem {
    /// Stable id (for `.ui.ron` menus and agent `scene.find`).
    pub id: &'static str,
    /// The label to show.
    pub label: &'static str,
}

/// The "Send feedback" entry: put it in the pause or settings menu and call [`open_prompt`]
/// when it is chosen (or spawn [`spawn_feedback_button`], which does that itself).
pub const FEEDBACK_MENU_ITEM: FeedbackMenuItem = FeedbackMenuItem {
    id: "send-feedback",
    label: "Send feedback",
};

/// Feedback settings.
#[derive(Resource, Debug, Clone)]
pub struct FeedbackConfig {
    /// A physical key that opens the prompt (`"F8"`, `"KeyB"`…). `None` (the default): no key;
    /// the game opens the prompt from its menu or binds [`Self::action`].
    pub hotkey: Option<String>,
    /// The input action that opens the prompt when the game's input config binds it
    /// ([`FEEDBACK_ACTION`], unbound by default).
    pub action: String,
    /// The feedback folder. `None`: `user://feedback` (on Windows `%LOCALAPPDATA%`, see
    /// [`feedback_folder`]).
    pub dir: Option<PathBuf>,
    /// Seconds of frame times in a bundle.
    pub window_s: f64,
    /// Log records in a bundle.
    pub log_lines: usize,
    /// Bundles kept in the folder (oldest removed first; `0` keeps all).
    pub keep: usize,
    /// Write a bundle on a panic.
    pub crash_bundles: bool,
    /// Also write one on a native crash (SIGSEGV and friends, driver crashes included) with a
    /// minidump, through an out-of-process monitor ([`native`]). Linux, and Windows when opted in
    /// ([`Self::native_crashes_windows`]); needs
    /// [`run_crash_monitor_if_requested`] first in `main`. Off when [`Self::crash_bundles`] is.
    /// Players can turn it off for a run with `REGIA_NO_NATIVE_CRASH=1`.
    pub native_crashes: bool,
    /// Windows only: native crash capture is **opt-in** there until it has run on real Windows
    /// (see the Windows-parity ADR in docs/decisions.md). Set this, or run with `REGIA_NATIVE_CRASH=1`, to enable it. Ignored on
    /// other targets, where [`Self::native_crashes`] alone decides.
    pub native_crashes_windows: bool,
    /// Tell the player about the last session's crash bundle on launch (a toast and a log line).
    pub notify_crash_on_launch: bool,
    /// Maximum saved pipeline-cache attachment bytes (default 32 MiB; 0 disables).
    /// Also capped to half `bundle::MAX_BUNDLE_BYTES`. Only normal feedback captures
    /// attach the last saved cache; crash handlers never read it or call the driver.
    pub pipeline_cache_max_bytes: u64,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            hotkey: None,
            action: FEEDBACK_ACTION.to_owned(),
            dir: None,
            window_s: 30.0,
            log_lines: 400,
            keep: 50,
            crash_bundles: true,
            native_crashes: true,
            native_crashes_windows: false,
            notify_crash_on_launch: true,
            pipeline_cache_max_bytes: 32 * 1024 * 1024,
        }
    }
}

/// Computes game fields at capture time (for values that change every frame, like position).
pub type HostProvider = Arc<dyn Fn(&World) -> Vec<(String, String)> + Send + Sync>;

/// The game's hook into bundles: fields it sets when they change (match id, map, player count;
/// crash bundles carry these too) and an optional [`HostProvider`] evaluated only when a bundle
/// is captured. Keys `map` and `position` are also copied into `feedback.json`.
#[derive(Resource, Default, Clone)]
pub struct FeedbackHost {
    fields: BTreeMap<String, String>,
    provider: Option<HostProvider>,
}

impl std::fmt::Debug for FeedbackHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedbackHost")
            .field("fields", &self.fields)
            .field("provider", &self.provider.is_some())
            .finish()
    }
}

impl FeedbackHost {
    /// Sets one field.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.fields.insert(key.into(), value.into());
    }

    /// Removes one field.
    pub fn remove(&mut self, key: &str) {
        self.fields.remove(key);
    }

    /// Removes every field.
    pub fn clear(&mut self) {
        self.fields.clear();
    }

    /// The fields.
    #[must_use]
    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.fields
    }

    /// Sets the capture-time provider (replaces any previous one).
    pub fn set_provider(
        &mut self,
        provider: impl Fn(&World) -> Vec<(String, String)> + Send + Sync + 'static,
    ) {
        self.provider = Some(Arc::new(provider));
    }
}

/// Sent when a bundle was written (prompt, agent or API capture; not crash bundles).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct FeedbackWritten {
    /// The bundle.
    pub info: BundleInfo,
    /// Written from the player's prompt.
    pub from_prompt: bool,
}

/// Called once with the written bundle or an error, on the writer thread.
pub type FeedbackDone = Box<dyn FnOnce(Result<BundleInfo, String>) + Send + Sync>;

/// A capture without the prompt (agent, tests, a game's own UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOptions {
    /// Category.
    pub category: FeedbackCategory,
    /// Note.
    pub note: String,
    /// Include a screenshot of the next presented frame.
    pub screenshot: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            category: FeedbackCategory::Other,
            note: String::new(),
            screenshot: true,
        }
    }
}

/// A write job shared with its thread, so a failed spawn can still run it inline.
type SharedJob = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

type ShotSlot = Arc<Mutex<Option<Result<CapturedImage, String>>>>;

enum Request {
    Prompt,
    Direct(CaptureOptions, Option<FeedbackDone>),
}

enum Destination {
    Prompt,
    Direct(Option<FeedbackDone>),
}

struct Pending {
    input: BundleInput,
    shot: Option<ShotSlot>,
    waited: u32,
    destination: Destination,
}

struct PromptState {
    input: BundleInput,
    category: FeedbackCategory,
    root: Entity,
    field: Entity,
}

struct WriteResult {
    result: Result<BundleInfo, String>,
    from_prompt: bool,
}

/// The feedback state: open the prompt, capture directly, read what was written.
#[derive(Resource, Default)]
pub struct Feedback {
    requests: Vec<Request>,
    pending: Vec<Pending>,
    prompt: Option<PromptState>,
    results: Arc<Mutex<Vec<WriteResult>>>,
    writing: usize,
    /// The `regia-feedback` writer threads still running, joined on [`AppExit`].
    writers: Vec<std::thread::JoinHandle<()>>,
    last: Option<BundleInfo>,
    crash_notice: Vec<BundleFile>,
    toast: Option<(Entity, f64)>,
    ui_cursor: MessageCursor<UiEvent>,
    key_cursor: MessageCursor<KeyboardInput>,
}

impl Feedback {
    /// Opens the prompt (takes the screenshot first, then shows the panel). No-op while open.
    pub fn open_prompt(&mut self) {
        if !self.is_prompt_open() && !self.requests.iter().any(|r| matches!(r, Request::Prompt)) {
            self.requests.push(Request::Prompt);
        }
    }

    /// Captures a bundle without the prompt. `done` runs on the writer thread.
    pub fn capture(&mut self, options: CaptureOptions, done: Option<FeedbackDone>) {
        self.requests.push(Request::Direct(options, done));
    }

    /// Whether the prompt is showing or about to (games skip gameplay input while it is).
    #[must_use]
    pub fn is_prompt_open(&self) -> bool {
        self.prompt.is_some()
            || self
                .pending
                .iter()
                .any(|p| matches!(p.destination, Destination::Prompt))
    }

    /// The last bundle written this session.
    #[must_use]
    pub fn last_written(&self) -> Option<&BundleInfo> {
        self.last.as_ref()
    }

    /// Crash bundles from earlier sessions the player was told about at this launch.
    #[must_use]
    pub fn crash_notice(&self) -> &[BundleFile] {
        &self.crash_notice
    }

    /// Whether anything is in flight (a request, a screenshot wait, the prompt, a write, a toast).
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.requests.is_empty()
            || !self.pending.is_empty()
            || self.prompt.is_some()
            || self.writing > 0
            || self.toast.is_some()
    }
}

/// Opens the feedback prompt (what the menu's "Send feedback" entry calls).
pub fn open_prompt(world: &mut World) {
    if let Some(mut feedback) = world.get_resource_mut::<Feedback>() {
        feedback.open_prompt();
    }
}

/// Spawns a "Send feedback" button under `parent` in the game's menu; clicking it opens the
/// prompt.
pub fn spawn_feedback_button(commands: &mut Commands, parent: Entity) -> Entity {
    let entity = spawn_button(commands, parent, FEEDBACK_MENU_ITEM.label, &[]);
    commands
        .entity(entity)
        .insert((FeedbackMenuButton, Name::new(FEEDBACK_MENU_ITEM.id)));
    entity
}

/// The feedback folder: [`FeedbackConfig::dir`], else `user://feedback` on the host. On Windows,
/// when `user://` is the standard roaming `%APPDATA%` directory, the default is the non-roaming
/// `%LOCALAPPDATA%\RegiaEngine\<exe>\feedback` instead, so crash bundles and minidumps stay on the
/// machine (`regia_platform::paths::default_feedback_dir`, ADR-525).
#[must_use]
pub fn feedback_folder(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<FeedbackConfig>()
        .and_then(|c| c.dir.clone())
        .or_else(|| {
            regia_platform::paths::feedback_dir_for(
                world
                    .get_resource::<Vfs>()
                    .and_then(|vfs| vfs.host_path(&VfsPath::root(MountPoint::User))),
            )
        })
}

/// The bundles in the feedback folder, oldest first (what a game would upload later).
#[must_use]
pub fn pending_bundles(world: &World) -> Vec<BundleFile> {
    feedback_folder(world).map_or_else(Vec::new, |d| list_bundles(&d))
}

/// Per-frame recorder state.
#[derive(Resource, Default)]
pub struct FeedbackRecorder {
    sampler: FrameSampler,
    problems_key: Option<(usize, usize)>,
    /// The native crash monitor has the context as of the first frame (GPU facts included).
    native_context_sent: bool,
}

/// Builds one [`FrameSample`] per frame: the frame interval, the renderer's CPU and GPU frame
/// times and the frame-path device calls since the previous sample. The feedback ring and
/// `regia_telemetry` share it, so both read a frame the same way. Never allocates.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameSampler {
    last_counters: Option<FramePathCounters>,
    last_cpu_sample: u64,
    last_gpu_sample: u64,
    timing: RenderTimingSample,
}

impl FrameSampler {
    fn timing_values(
        &mut self,
        cpu: regia_render::FrameCpuTimings,
        gpu: regia_render::GpuTimings,
    ) -> (f32, f32) {
        let cpu_fresh = cpu.sample_id > self.last_cpu_sample;
        let gpu_fresh = gpu.sample_id > self.last_gpu_sample;
        if cpu_fresh {
            self.last_cpu_sample = cpu.sample_id;
        }
        if gpu_fresh {
            self.last_gpu_sample = gpu.sample_id;
        }
        self.timing = RenderTimingSample {
            cpu_frame_id: cpu.frame_id,
            gpu_frame_id: gpu.frame_id,
            cpu_fresh,
            gpu_fresh,
            cpu_absent: (!cpu_fresh).then_some(RenderTimingAbsence::RendererIdle),
            gpu_absent: (!gpu_fresh).then_some(if cpu_fresh {
                RenderTimingAbsence::Pending
            } else {
                RenderTimingAbsence::RendererIdle
            }),
        };
        (
            if cpu_fresh { cpu.total_ms } else { 0.0 },
            if gpu_fresh { gpu.frame_ms } else { 0.0 },
        )
    }

    /// The frame that just ended, or `None` when real time did not advance (a paused or
    /// stepped app). The device-call counters advance either way.
    pub fn sample(
        &mut self,
        real: Option<&RealTime>,
        renderer: Option<&RuntimeRenderer>,
    ) -> Option<FrameSample> {
        let Some(real) = real.filter(|r| !r.delta().is_zero()) else {
            self.last_counters = Some(regia_render::frame_path_counters_all_threads());
            return None;
        };
        // The real interval, not gameplay's clamped delta: a multi-second main-loop stall
        // must read as one (it read as 250 ms before v0.1.8, and `GAP_MS` could never fire).
        Some(self.sample_at(
            real.elapsed().as_secs_f64(),
            real.raw_delta().as_secs_f32() * 1000.0,
            renderer.map(|r| &r.0),
        ))
    }

    /// The frame that just ended, with its time and interval given by the caller (a harness
    /// that renders without the ECS app).
    pub fn sample_at(
        &mut self,
        t_s: f64,
        frame_ms: f32,
        renderer: Option<&regia_render::FrameRenderer>,
    ) -> FrameSample {
        let now = regia_render::frame_path_counters_all_threads();
        let delta = self
            .last_counters
            .replace(now)
            .map(|last| now.since(last))
            .unwrap_or_default();
        let (cpu_ms, gpu_ms) = if let Some(r) = renderer {
            let gpu = if r.gpu_timing_enabled() {
                r.gpu_timings()
            } else {
                Default::default()
            };
            let values = self.timing_values(r.cpu_timings(), gpu);
            if !self.timing.gpu_fresh && !r.gpu_timing_enabled() {
                self.timing.gpu_absent = Some(RenderTimingAbsence::NoGpuTiming);
            }
            values
        } else {
            self.timing = RenderTimingSample {
                cpu_absent: Some(RenderTimingAbsence::NoRenderer),
                gpu_absent: Some(RenderTimingAbsence::NoRenderer),
                ..Default::default()
            };
            (0.0, 0.0)
        };
        FrameSample {
            timing: Some(self.timing),
            t_s,
            frame_ms,
            cpu_ms,
            gpu_ms,
            pipelines: delta.pipelines.min(u64::from(u32::MAX)) as u32,
            bind_groups: delta.bind_groups.min(u64::from(u32::MAX)) as u32,
            buffers: delta.buffers.min(u64::from(u32::MAX)) as u32,
            textures: delta.textures.min(u64::from(u32::MAX)) as u32,
            blocking_polls: delta.blocking_polls.min(u64::from(u16::MAX)) as u16,
            inline_bakes: delta.inline_bakes.min(u64::from(u16::MAX)) as u16,
        }
    }
}

/// Adds in-game feedback: the prompt, captures, the frame ring and the crash bundle hook.
/// Works without the runtime UI (no prompt or toasts then; [`Feedback::capture`] still works)
/// and without a renderer (no screenshot, frame CPU/GPU times or problems then).
#[derive(Debug, Default, Clone)]
pub struct FeedbackPlugin {
    /// Initial settings.
    pub config: FeedbackConfig,
}

impl Plugin for FeedbackPlugin {
    fn build(&self, app: &mut App) {
        // Allocate the ring now, not in the first frame.
        drop(crash::frames().lock());
        crash::update_context(|c| {
            c.window_s = self.config.window_s;
            c.log_lines = self.config.log_lines;
            c.keep = self.config.keep;
            c.enabled = self.config.crash_bundles;
        });
        app.insert_resource(self.config.clone())
            .init_resource::<Feedback>()
            .init_resource::<FeedbackHost>()
            .init_resource::<FeedbackRecorder>()
            .add_message::<FeedbackWritten>()
            .add_systems(Startup, feedback_startup)
            .add_systems(
                Update,
                feedback_update
                    .run_if(feedback_should_run)
                    .after(regia_ui::widgets::ui_widgets),
            )
            .add_systems(Last, (record_feedback_frame, join_feedback_writers_on_exit));
        crash::set_crash_dir(feedback_folder(app.world()));
        if self.config.crash_bundles {
            install_crash_hook();
        }
        native::start(
            self.config.crash_bundles && self.config.native_crashes,
            self.config.native_crashes_windows,
        );
        // A lost GPU device ends the game with a crash bundle (ADR-171).
        app.world_mut()
            .get_resource_or_init::<regia_render::DeviceLostHooks>()
            .add(device_lost_bundle);
        // An uncaptured validation or internal error writes breadcrumbs at most once a minute
        // (spec 54 § 4.1). The hook does not need the ECS world.
        regia_render::gpu_health::set_validation_bundle_hook(validation_breadcrumb_bundle);
    }
}

/// The uncaptured-error hook: one bug bundle with `gpu_breadcrumbs.json` (`lost` null).
fn validation_breadcrumb_bundle(kind: &'static str) {
    match crash::write_validation_breadcrumb_bundle(kind) {
        Some(bundle) => tracing::info!(
            target: "regia_framework",
            "validation breadcrumb bundle written: {}",
            bundle.name
        ),
        None => tracing::debug!(
            target: "regia_framework",
            "no validation breadcrumb bundle (bundles off, no folder, or one in the last minute)"
        ),
    }
}

/// The [`regia_render::DeviceLostHooks`] entry: refreshes the render problems the bundle
/// carries, then writes the device-lost crash bundle.
fn device_lost_bundle(world: &mut World, info: &regia_render::DeviceLostInfo) {
    if let Some(renderer) = world.get_resource::<RuntimeRenderer>() {
        let list = problems_of(renderer);
        crash::update_context(|c| c.render_problems = Some(list));
    }
    match crash::write_device_lost_bundle(info) {
        Some(bundle) => tracing::info!(
            target: "regia_framework",
            "device-lost crash bundle written: {}",
            bundle.name
        ),
        None => tracing::warn!(
            target: "regia_framework",
            "no device-lost crash bundle written (crash bundles off, no folder, or one in the last 5 s)"
        ),
    }
}

/// `Last`: records the frame into the ring and refreshes what a crash bundle would carry when it
/// changed. Allocates only when the settings, problems or host fields change.
pub fn record_feedback_frame(
    real: Option<Res<RealTime>>,
    renderer: Option<Res<RuntimeRenderer>>,
    settings: Option<Res<GameSettings>>,
    host: Option<Res<FeedbackHost>>,
    mut recorder: ResMut<FeedbackRecorder>,
    (hidden, runner): (
        Option<Res<regia_platform::SurfacesHidden>>,
        Option<Res<regia_app::RunnerState>>,
    ),
    (extracted, warm): (
        Option<Res<regia_render::ExtractedFrame>>,
        Option<Res<regia_render::GameFrameWarm>>,
    ),
) {
    if let Some(mut sample) = recorder.sampler.sample(real.as_deref(), renderer.as_deref()) {
        if let Some(timing) = sample.timing.as_mut() {
            timing.inactive(
                hidden.is_some_and(|h| h.0)
                    || runner.is_some_and(|r| r.occluded || r.stalled || r.surface_refused),
                extracted.is_some_and(|x| x.views.is_empty()) || warm.is_some_and(|w| w.is_warming()),
            );
        }
        crash::frames()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(sample);
    }
    if let Some(settings) = settings.as_ref()
        && settings.is_changed()
    {
        let ron = settings.to_ron().ok();
        crash::update_context(|c| c.settings_ron = ron);
    }
    if let Some(host) = host.as_ref()
        && host.is_changed()
    {
        let fields = host.fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        crash::update_context(|c| c.host = fields);
    }
    if let Some(renderer) = renderer.as_ref() {
        let problems = renderer.0.frame_problems();
        let key = (
            problems.len(),
            problems.iter().map(|p| p.code.len() + p.message.len()).sum(),
        );
        if recorder.problems_key != Some(key) {
            recorder.problems_key = Some(key);
            let list = problems_of(renderer);
            crash::update_context(|c| c.render_problems = Some(list));
        }
    }
    // Once, after the monitor connected: by now the renderer has reported the GPU.
    if !recorder.native_context_sent {
        match native::native_crash_status() {
            NativeCrashStatus::Starting => {}
            NativeCrashStatus::Armed => {
                recorder.native_context_sent = true;
                native::publish_context();
            }
            _ => recorder.native_context_sent = true,
        }
    }
}

fn problems_of(renderer: &RuntimeRenderer) -> Vec<(String, String)> {
    renderer
        .0
        .frame_problems()
        .iter()
        .map(|p| (p.code.to_owned(), p.message.clone()))
        .collect()
}

fn hotkey_pressed(config: &FeedbackConfig, keyboard: Option<&Keyboard>, actions: Option<&Actions>) -> bool {
    config
        .hotkey
        .as_deref()
        .is_some_and(|k| keyboard.is_some_and(|kb| kb.just_pressed(k)))
        || actions.is_some_and(|a| a.just_pressed_this_frame(&config.action))
}

/// Run condition of [`feedback_update`]: something is in flight, the key or action fired, or a
/// UI event arrived (a menu entry may have been clicked).
pub fn feedback_should_run(
    feedback: Res<Feedback>,
    config: Res<FeedbackConfig>,
    keyboard: Option<Res<Keyboard>>,
    actions: Option<Res<Actions>>,
    ui_events: Option<Res<Messages<UiEvent>>>,
) -> bool {
    feedback.is_active()
        || hotkey_pressed(&config, keyboard.as_deref(), actions.as_deref())
        || ui_events.is_some_and(|m| !m.is_empty())
}

enum UiAction {
    Open,
    Button(PromptButton),
}

/// `Update`: the key/action and menu entry, the prompt's buttons, screenshot waits, finished
/// writes and toasts.
pub fn feedback_update(world: &mut World) {
    let _span = tracing::trace_span!("feedback").entered();
    let pressed = hotkey_pressed(
        world.resource::<FeedbackConfig>(),
        world.get_resource::<Keyboard>(),
        world.get_resource::<Actions>(),
    );
    if pressed {
        if world.resource::<Feedback>().prompt.is_some() {
            close_prompt(world, false);
        } else {
            world.resource_mut::<Feedback>().open_prompt();
        }
    }
    for action in ui_actions(world) {
        match action {
            UiAction::Open => world.resource_mut::<Feedback>().open_prompt(),
            UiAction::Button(PromptButton::Category(c)) => set_category(world, c),
            UiAction::Button(PromptButton::Send) => close_prompt(world, true),
            UiAction::Button(PromptButton::Cancel) => close_prompt(world, false),
        }
    }
    let requests = std::mem::take(&mut world.resource_mut::<Feedback>().requests);
    for request in requests {
        let (options, destination) = match request {
            Request::Prompt => (
                CaptureOptions {
                    category: FeedbackCategory::Bug,
                    note: String::new(),
                    screenshot: true,
                },
                Destination::Prompt,
            ),
            Request::Direct(options, done) => (options, Destination::Direct(done)),
        };
        let pending = capture_moment(world, &options, destination);
        world.resource_mut::<Feedback>().pending.push(pending);
    }
    advance_pending(world);
    finish_writes(world);
    let now = world
        .get_resource::<RealTime>()
        .map_or(0.0, |t| t.elapsed().as_secs_f64());
    let expired = world
        .resource::<Feedback>()
        .toast
        .filter(|(_, until)| now >= *until)
        .map(|(e, _)| e);
    if let Some(entity) = expired {
        world.resource_mut::<Feedback>().toast = None;
        let _ = world.try_despawn(entity);
        prompt::mark_layout(world);
    }
}

/// Reads this frame's UI events and Escape presses that concern feedback.
fn ui_actions(world: &mut World) -> Vec<UiAction> {
    let mut actions = Vec::new();
    world.resource_scope(|world, mut feedback: Mut<Feedback>| {
        let open = feedback.prompt.is_some();
        let feedback = &mut *feedback;
        if let Some(messages) = world.get_resource::<Messages<UiEvent>>() {
            for event in feedback.ui_cursor.read(messages) {
                match &event.kind {
                    UiEventKind::Click => {
                        if let Some(b) = world.get::<PromptButton>(event.target) {
                            actions.push(UiAction::Button(*b));
                        } else if world.get::<FeedbackMenuButton>(event.target).is_some() {
                            actions.push(UiAction::Open);
                        }
                    }
                    UiEventKind::Submitted(_) if world.get::<PromptNote>(event.target).is_some() => {
                        actions.push(UiAction::Button(PromptButton::Send));
                    }
                    _ => {}
                }
            }
        }
        if let Some(keys) = world.get_resource::<Messages<KeyboardInput>>() {
            for key in feedback.key_cursor.read(keys) {
                if open
                    && key.state == ButtonState::Pressed
                    && !key.repeat
                    && (key.logical_key == "Escape" || key.physical_key == "Escape")
                {
                    actions.push(UiAction::Button(PromptButton::Cancel));
                }
            }
        }
    });
    actions
}

/// Takes everything a bundle needs at this moment, and asks for a screenshot.
fn capture_moment(world: &mut World, options: &CaptureOptions, destination: Destination) -> Pending {
    let config = world.resource::<FeedbackConfig>().clone();
    let mut input = BundleInput::new(options.category, options.note.clone());
    if let Some(os) = world.get_resource::<regia_platform::OsInfo>() {
        input.system.os = os.clone();
    }
    input.window_s = config.window_s;
    input.pipeline_cache_max_bytes = config.pipeline_cache_max_bytes;
    if config.pipeline_cache_max_bytes > 0 {
        input.pipeline_cache = world
            .get_resource::<PipelineCacheAttachment>()
            .cloned()
            .or_else(|| {
                regia_render::pipeline_cache::installed().map(|cache| PipelineCacheAttachment {
                    path: cache.path().to_path_buf(),
                    key: cache.key().clone(),
                })
            });
    }
    input.frames = crash::frames()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .last_window(config.window_s);
    input.settings_ron = world.get_resource::<GameSettings>().and_then(|s| s.to_ron().ok());
    input.render_problems = world.get_resource::<RuntimeRenderer>().map(problems_of);
    input.log_tail = crash::log_tail(config.log_lines);
    if let Some(host) = world.get_resource::<FeedbackHost>().cloned() {
        input.host = host.fields.into_iter().collect();
        if let Some(provider) = host.provider {
            for (k, v) in provider(world) {
                input.host.retain(|(key, _)| *key != k);
                input.host.push((k, v));
            }
        }
    }
    let shot = if !options.screenshot {
        input.screenshot_missing = Some("screenshot not requested".into());
        None
    } else if let Some(mut requests) = world.get_resource_mut::<ScreenshotRequests>() {
        let slot: ShotSlot = Arc::new(Mutex::new(None));
        let writer = slot.clone();
        requests.request(
            None,
            Box::new(move |result| {
                *writer.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
            }),
        );
        Some(slot)
    } else {
        input.screenshot_missing = Some("no renderer in this app".into());
        None
    };
    Pending {
        input,
        shot,
        waited: 0,
        destination,
    }
}

/// Resolves screenshot waits: opens the prompt or starts the write.
fn advance_pending(world: &mut World) {
    if world.resource::<Feedback>().pending.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut world.resource_mut::<Feedback>().pending);
    let mut waiting = Vec::new();
    for mut p in pending {
        let ready = match &p.shot {
            None => true,
            Some(slot) => {
                let taken = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
                match taken {
                    Some(Ok(image)) => {
                        p.input.screenshot = Some(image);
                        true
                    }
                    Some(Err(e)) => {
                        p.input.screenshot_missing = Some(format!("screenshot failed: {e}"));
                        true
                    }
                    None => {
                        p.waited += 1;
                        if p.waited > SCREENSHOT_TIMEOUT_FRAMES {
                            p.input.screenshot_missing = Some(format!(
                                "no frame presented within {SCREENSHOT_TIMEOUT_FRAMES} frames"
                            ));
                            true
                        } else {
                            false
                        }
                    }
                }
            }
        };
        if !ready {
            waiting.push(p);
            continue;
        }
        match p.destination {
            Destination::Prompt if prompt::ui_available(world) => show_prompt(world, p.input),
            Destination::Prompt => {
                tracing::warn!(
                    target: "regia_framework::feedback",
                    "feedback prompt needs regia_ui::UiPlugin; writing the bundle without a note"
                );
                spawn_write(world, p.input, None, true);
            }
            Destination::Direct(done) => spawn_write(world, p.input, done, false),
        }
    }
    world.resource_mut::<Feedback>().pending.extend(waiting);
}

fn folder_label(world: &World) -> String {
    feedback_folder(world).map_or_else(
        || "the game's feedback folder".to_owned(),
        |p| p.display().to_string(),
    )
}

fn show_prompt(world: &mut World, input: BundleInput) {
    let category = input.category;
    let folder = folder_label(world);
    let (root, field) = prompt::build_prompt(world, category, &input.note, &folder);
    world.resource_mut::<Feedback>().prompt = Some(PromptState {
        input,
        category,
        root,
        field,
    });
}

fn set_category(world: &mut World, category: FeedbackCategory) {
    let Some(mut state) = world.resource_mut::<Feedback>().prompt.take() else {
        return;
    };
    if let Some(input) = world.get::<TextInput>(state.field) {
        state.input.note = input.text.clone();
    }
    let _ = world.try_despawn(state.root);
    state.category = category;
    state.input.category = category;
    let folder = folder_label(world);
    let (root, field) = prompt::build_prompt(world, category, &state.input.note, &folder);
    state.root = root;
    state.field = field;
    world.resource_mut::<Feedback>().prompt = Some(state);
}

fn close_prompt(world: &mut World, send: bool) {
    let Some(mut state) = world.resource_mut::<Feedback>().prompt.take() else {
        return;
    };
    if let Some(input) = world.get::<TextInput>(state.field) {
        state.input.note = input.text.clone();
    }
    let _ = world.try_despawn(state.root);
    prompt::mark_layout(world);
    if send {
        state.input.category = state.category;
        spawn_write(world, state.input, None, true);
    }
}

/// Encodes and writes the bundle on a worker thread (PNG encoding of a 4K frame takes a while).
fn spawn_write(world: &mut World, input: BundleInput, done: Option<FeedbackDone>, from_prompt: bool) {
    let dir = feedback_folder(world);
    let keep = world.resource::<FeedbackConfig>().keep;
    let mut feedback = world.resource_mut::<Feedback>();
    feedback.writing += 1;
    let results = feedback.results.clone();
    let job = move || {
        let result = match dir {
            Some(dir) => write_bundle(&dir, &input, keep).map_err(|e| format!("{}: {e}", dir.display())),
            None => Err("no feedback folder (no user:// mount and no FeedbackConfig::dir)".to_owned()),
        };
        results
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(WriteResult {
                result: result.clone(),
                from_prompt,
            });
        if let Some(done) = done {
            done(result);
        }
    };
    let job: SharedJob = Arc::new(Mutex::new(Some(Box::new(job))));
    let thread_job = job.clone();
    let spawned = std::thread::Builder::new()
        .name("regia-feedback".into())
        .spawn(move || {
            if let Some(job) = thread_job.lock().unwrap_or_else(PoisonError::into_inner).take() {
                job();
            }
        });
    match spawned {
        Ok(handle) => {
            let mut feedback = world.resource_mut::<Feedback>();
            feedback.writers.retain(|w| !w.is_finished());
            feedback.writers.push(handle);
        }
        Err(_) => {
            if let Some(job) = job.lock().unwrap_or_else(PoisonError::into_inner).take() {
                job();
            }
        }
    }
}

/// Waits for every running writer thread, so quitting right after a send does not kill one
/// inside the file write.
fn join_writers(feedback: &mut Feedback) {
    for writer in std::mem::take(&mut feedback.writers) {
        let _ = writer.join();
    }
}

/// `Last`: on [`AppExit`], lets in-flight feedback bundles finish writing.
fn join_feedback_writers_on_exit(mut exit: MessageReader<AppExit>, mut feedback: ResMut<Feedback>) {
    if exit.read().next().is_some() {
        join_writers(&mut feedback);
    }
}

/// Picks up finished writes: toast, log line, [`FeedbackWritten`].
fn finish_writes(world: &mut World) {
    if world.resource::<Feedback>().writing == 0 {
        return;
    }
    let results = {
        let feedback = world.resource::<Feedback>();
        std::mem::take(&mut *feedback.results.lock().unwrap_or_else(PoisonError::into_inner))
    };
    for WriteResult { result, from_prompt } in results {
        {
            let mut feedback = world.resource_mut::<Feedback>();
            feedback.writing = feedback.writing.saturating_sub(1);
        }
        match result {
            Ok(info) => {
                tracing::info!(
                    target: "regia_framework::feedback",
                    "feedback bundle written: {} ({} bytes)",
                    info.path.display(),
                    info.bytes
                );
                if from_prompt {
                    let text = format!(
                        "Feedback saved to {}. Nothing was sent: share this file to report it.",
                        info.path.display()
                    );
                    show_toast(world, &text, TOAST_SECONDS);
                }
                world.resource_mut::<Feedback>().last = Some(info.clone());
                world.write_message(FeedbackWritten { info, from_prompt });
            }
            Err(e) => {
                tracing::error!(target: "regia_framework::feedback", "feedback bundle not written: {e}");
                if from_prompt {
                    show_toast(world, &format!("Could not save feedback: {e}"), TOAST_SECONDS);
                }
            }
        }
    }
}

fn show_toast(world: &mut World, text: &str, seconds: f64) {
    if !prompt::ui_available(world) {
        return;
    }
    if let Some((old, _)) = world.resource_mut::<Feedback>().toast.take() {
        let _ = world.try_despawn(old);
    }
    let entity = prompt::build_toast(world, text);
    let now = world
        .get_resource::<RealTime>()
        .map_or(0.0, |t| t.elapsed().as_secs_f64());
    world.resource_mut::<Feedback>().toast = Some((entity, now + seconds));
}

/// `Startup`: points crash bundles at the folder and tells the player about the last session's
/// crash bundle.
fn feedback_startup(world: &mut World) {
    let dir = feedback_folder(world);
    crash::set_crash_dir(dir.clone());
    let Some(dir) = dir else {
        return;
    };
    if !world.resource::<FeedbackConfig>().notify_crash_on_launch {
        return;
    }
    let crashes = unannounced_crashes(&dir);
    let Some(newest) = crashes.last() else {
        return;
    };
    tracing::warn!(
        target: "regia_framework::feedback",
        "the last session crashed; its report is {} (nothing was sent)",
        newest.path.display()
    );
    let text = format!(
        "The game closed unexpectedly last time. A crash report was saved to {}. Nothing was sent: \
         share this file to report it.",
        newest.path.display()
    );
    show_toast(world, &text, CRASH_NOTICE_SECONDS);
    if let Err(e) = mark_announced(&dir, &crashes) {
        tracing::warn!(target: "regia_framework::feedback", "could not record the crash notice: {e}");
    }
    world.resource_mut::<Feedback>().crash_notice = crashes;
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod stall_attribution_tests {
    use super::*;

    #[test]
    fn delayed_gpu_and_replaced_renderer_publications_are_independent() {
        let mut sampler = FrameSampler::default();
        let cpu = regia_render::FrameCpuTimings {
            frame_id: Some(9),
            sample_id: 10,
            total_ms: 25.0,
            ..Default::default()
        };
        assert_eq!(sampler.timing_values(cpu, Default::default()), (25.0, 0.0));
        assert_eq!(sampler.timing.gpu_absent, Some(RenderTimingAbsence::Pending));
        let gpu = regia_render::GpuTimings {
            frame_id: Some(7),
            sample_id: 11,
            frame_ms: 3.0,
            ..Default::default()
        };
        assert_eq!(sampler.timing_values(cpu, gpu), (0.0, 3.0));
        assert_eq!(sampler.timing.gpu_frame_id, Some(7));
        assert!(!sampler.timing.cpu_fresh && sampler.timing.gpu_fresh);
        let replaced = regia_render::FrameCpuTimings {
            frame_id: Some(1),
            sample_id: 12,
            total_ms: 2.0,
            ..Default::default()
        };
        assert_eq!(sampler.timing_values(replaced, gpu), (2.0, 0.0));
        assert_eq!(
            sampler.timing_values(cpu, gpu),
            (0.0, 0.0),
            "returning to an older renderer must not repeat its publication"
        );
        assert_eq!(
            sampler
                .sample_at(1.0, 100.0, None)
                .timing
                .expect("metadata")
                .cpu_absent,
            Some(RenderTimingAbsence::NoRenderer)
        );
        assert_eq!(
            sampler.timing_values(replaced, gpu),
            (0.0, 0.0),
            "a renderer temporarily removed for warming must not reset the sampler"
        );
    }

    #[test]
    fn one_slow_render_then_update_only_ticks_consumes_timings_once() {
        let mut sampler = FrameSampler::default();
        let cpu = regia_render::FrameCpuTimings {
            frame_id: Some(1),
            sample_id: 1,
            total_ms: 4218.53,
            ..Default::default()
        };
        let gpu = regia_render::GpuTimings {
            frame_id: Some(1),
            sample_id: 2,
            frame_ms: 0.52,
            ..Default::default()
        };
        assert_eq!(sampler.timing_values(cpu, gpu), (4218.53, 0.52));
        for _ in 0..8 {
            assert_eq!(
                sampler.timing_values(cpu, gpu),
                (0.0, 0.0),
                "an update-only tick must not repeat the last render"
            );
            assert_eq!(sampler.timing.cpu_frame_id, Some(1));
            assert_eq!(sampler.timing.cpu_absent, Some(RenderTimingAbsence::RendererIdle));
            assert_eq!(sampler.timing.gpu_absent, Some(RenderTimingAbsence::RendererIdle));
        }
    }
}
