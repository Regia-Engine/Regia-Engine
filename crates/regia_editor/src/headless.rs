// SPDX-License-Identifier: MPL-2.0
//! The headless editor (`regia-editor --headless`, `regia editor --headless`; `docs/specs/21` § 1,
//! `docs/specs/23` § 2, ADR-2412): the edit world, the asset DB with its live cooker, the hot-reload
//! host and the agent protocol server, with no window and no surface. CI and cloud agents drive it.
//!
//! [`run`] steps the edit world the way the windowed editor's redraw does (hot-reload poll, asset
//! watch, play-in-editor requests, the panel pumps, one edit-world update), minus the egui pass.
//! Views render offscreen on the adapter: `editor.capture_viewport` and the other view methods use
//! the device opened at start, and a `seeing.screenshot` of the editor window is answered with the
//! perspective viewport. Without an adapter (`--no-gpu`, or none found) the view methods answer
//! `editor_headless`, and a window screenshot fails with a message naming it.
//!
//! On start the process prints one JSON line to stdout once the agent server listens:
//! `{"event":"ready","endpoint":...,"url":...,"pid":...,"adapter":...}`, so a supervisor waits on
//! a line, never on a timer. It ends cleanly on `agent.shutdown` (dirty documents saved first),
//! `quit.request` (dirty level autosaved, as in the windowed editor), SIGTERM, SIGINT or SIGHUP
//! (Ctrl+C, Ctrl+Break or console close on Windows; dirty documents saved), and with
//! `--exit-on-stdin-eof` when stdin reaches its end, which is how `regia mcp --headless` leaves no
//! editor behind however it ends (ADR-2821). It prints `{"event":"exit",...}` before it returns.
//!
//! [`install`] adds the methods this module answers to the edit world in both modes:
//! `editor.headless_status`, `editor.open_level`, `agent.shutdown`, `gen.list`, `gen.run` and
//! `gen.verify` (the generators of `regia_asset_pipeline::generators`). A generator runs on a
//! worker as job `gen:<n>`; the call waits for it up to `wait_ms`, so a short one still answers its
//! report and a long one answers the job id without holding the edit world (ADR-2821).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;

use crate::HotReloadHost;
use crate::command::UndoStack;
use crate::level_io::{self, OpenLevel};
use crate::pie::{PieRequests, PieSession, PlayMode};

/// The error code of a view method on a headless editor without an adapter.
pub const EDITOR_HEADLESS: &str = "editor_headless";

/// The size of a headless `seeing.screenshot` of the editor window (the perspective viewport).
pub const WINDOW_CAPTURE_SIZE: [f32; 2] = [1280.0, 720.0];

/// How long an ending editor waits for its jobs to stop (each kills its process tree when asked).
const JOB_STOP_GRACE: Duration = Duration::from_secs(10);

/// How long the loop keeps the agent server up after `agent.shutdown` answered, so the answer
/// leaves the socket before the server goes (the reply is written by the server's own thread).
const SHUTDOWN_REPLY_GRACE: Duration = Duration::from_millis(200);

/// How a headless editor runs.
#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    /// Open a GPU device for offscreen views (`false` with `--no-gpu`).
    pub gpu: bool,
    /// Exit after this many frames (`--exit-after-frames`), saving like a SIGTERM.
    pub exit_after: Option<u64>,
    /// Time between edit-world frames when idle.
    pub frame_interval: Duration,
    /// End, saving like a SIGTERM, when stdin reaches its end (`--exit-on-stdin-eof`): whoever
    /// started the editor holds the pipe, and the kernel closes it however that process ends.
    pub exit_on_stdin_eof: bool,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            gpu: true,
            exit_after: None,
            frame_interval: Duration::from_millis(10),
            exit_on_stdin_eof: false,
        }
    }
}

/// The adapter a headless editor renders views on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HeadlessAdapter {
    /// The adapter's name.
    pub name: String,
    /// The backend (`vulkan`, `dx12`, `metal`, `gl`).
    pub backend: String,
    /// `discrete_gpu`, `integrated_gpu`, `virtual_gpu`, `cpu` or `other`.
    pub device_type: String,
}

/// Present in the edit world while the editor runs headless: what `editor.headless_status`
/// reports and what the view methods check.
#[derive(Resource, Debug, Clone, Default)]
pub struct HeadlessEditor {
    /// The adapter views render on (`None`: views answer `editor_headless`).
    pub adapter: Option<HeadlessAdapter>,
    /// Why there is no adapter, when there is none.
    pub no_adapter_reason: Option<String>,
    /// Edit-world frames stepped so far.
    pub frames: u64,
    /// Set by `agent.shutdown`: the loop ends after this frame.
    pub shutdown_requested: bool,
    /// What `agent.shutdown` saved (the exit line reports it; the loop does not save again).
    pub shutdown_saved: Vec<String>,
}

/// Inserted by `agent.shutdown` in the windowed editor: the event loop saves the shell's own
/// documents (the Material panel, Project Settings), then quits like `quit.request`.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct ShutdownRequest;

/// The error a view method answers on a headless editor without an adapter, or `None` when the
/// world is windowed or has one.
#[must_use]
pub fn no_adapter_error(world: &World) -> Option<RpcError> {
    let headless = world.get_resource::<HeadlessEditor>()?;
    if headless.adapter.is_some() {
        return None;
    }
    let hint = if headless.no_adapter_reason.as_deref() == Some(NO_GPU_REASON) {
        "start it without --no-gpu on a machine with a Vulkan, DX12 or Metal adapter (lavapipe works)"
    } else {
        "this machine offered no Vulkan, DX12 or Metal adapter: install lavapipe (Vulkan software) or a GPU driver"
    };
    Some(RpcError::new(
        protocol::UNSUPPORTED,
        EDITOR_HEADLESS,
        format!(
            "the headless editor has no GPU adapter ({})",
            headless.no_adapter_reason.as_deref().unwrap_or("none found")
        ),
        hint,
    ))
}

/// `HeadlessEditor::no_adapter_reason` when the editor was started with `--no-gpu`.
const NO_GPU_REASON: &str = "started with --no-gpu";

fn adapter_of(device: &regia_render::RenderDevice) -> HeadlessAdapter {
    let info = device.adapter.get_info();
    HeadlessAdapter {
        name: info.name.clone(),
        backend: format!("{:?}", info.backend).to_ascii_lowercase(),
        device_type: match info.device_type {
            wgpu::DeviceType::DiscreteGpu => "discrete_gpu",
            wgpu::DeviceType::IntegratedGpu => "integrated_gpu",
            wgpu::DeviceType::VirtualGpu => "virtual_gpu",
            wgpu::DeviceType::Cpu => "cpu",
            wgpu::DeviceType::Other => "other",
        }
        .to_string(),
    }
}

/// Opens the offscreen device (when `gpu`) and marks the world headless. The device becomes the
/// world's [`regia_render::RenderDevice`], which every view method shares.
pub fn prepare(world: &mut World, gpu: bool) {
    let mut state = HeadlessEditor::default();
    if gpu {
        match crate::render::headless_device() {
            Ok(device) => {
                state.adapter = Some(adapter_of(&device));
                world.insert_resource(device);
            }
            Err(err) => {
                tracing::warn!(target: "regia_editor", "headless editor without views: {err}");
                state.no_adapter_reason = Some(err.to_string());
            }
        }
    } else {
        state.no_adapter_reason = Some(NO_GPU_REASON.to_string());
    }
    world.insert_resource(state);
    // The agent's offscreen captures and render-out must not open a device behind the flag
    // (ADR-5180): they answer the same `editor_headless` as the view methods, word for word.
    if let Some(err) = no_adapter_error(world) {
        let hint = err
            .data
            .as_ref()
            .and_then(|data| data.get("hint"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        world.insert_resource(regia_agent::GpuDisabled {
            code: EDITOR_HEADLESS.to_owned(),
            reason: err.message,
            hint,
        });
    }
}

/// What saving the dirty documents did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SaveSummary {
    /// Documents written (project-relative paths when inside the project).
    pub saved: Vec<String>,
    /// Documents that could not be written, with why.
    pub failed: Vec<String>,
}

fn shown(root: Option<&Path>, path: &Path) -> String {
    root.and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Saves the documents with edits the edit world holds: the open level (an untitled one goes to
/// the first free `assets/unsaved*.level.ron`) with its terrain and tilemaps, and an open UI
/// designer document. The windowed shell's own panel documents (the Material panel, Project
/// Settings) are never dirty headless: no panel draws.
pub fn save_dirty_state(world: &mut World) -> SaveSummary {
    let mut summary = SaveSummary::default();
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    if world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty) {
        let mut open = world
            .get_resource::<OpenLevel>()
            .cloned()
            .unwrap_or_else(|| level_io::new_level(world));
        let before = open.clone();
        if open.path.is_none()
            && let Some(root) = &root
        {
            open.path = Some(level_io::free_untitled_level_path(&root.join("assets")));
        }
        match open.path.clone() {
            None => summary
                .failed
                .push("the level: untitled and no project is open".to_string()),
            Some(path) => match level_io::save_level_and_terrain(world, &open, &path) {
                Ok(_) => {
                    if let Some(root) = &root {
                        crate::autosave::clear_autosave_after_save(root, &before, &path);
                    }
                    open.path = Some(path.clone());
                    world.insert_resource(open);
                    summary.saved.push(shown(root.as_deref(), &path));
                    // The tile strokes are on the same undo stack: it is clean only once the
                    // tile map is written too, so a failed write keeps the session dirty.
                    match crate::mode2d::save_dirty_tilemap(world) {
                        Ok(map) => {
                            if let Some((map, _)) = map {
                                summary.saved.push(shown(root.as_deref(), &map));
                            }
                            crate::terrain_tool::commit_terrain_save(world);
                            world
                                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                                .mark_saved();
                        }
                        Err(err) => summary.failed.push(format!("the tile map: {err}")),
                    }
                }
                Err(err) => summary
                    .failed
                    .push(format!("{}: {err}", shown(root.as_deref(), &path))),
            },
        }
    }
    if let Some(mut designer) = world.get_resource_mut::<crate::ui_designer::UiDesigner>()
        && designer.doc().is_some_and(crate::ui_designer::UiDesign::dirty)
    {
        let path = designer.doc().and_then(|doc| doc.path.clone());
        match designer.doc_mut().and_then(|doc| doc.save(None)) {
            Ok(_) => summary.saved.push(
                path.as_deref()
                    .map_or_else(|| "the UI document".to_string(), |p| shown(root.as_deref(), p)),
            ),
            Err(err) => summary.failed.push(format!("UI document: {err}")),
        }
    }
    summary
}

/// The JSON line printed when the agent server listens.
#[must_use]
pub fn ready_line(world: &World) -> Value {
    let endpoint = world.get_resource::<regia_agent::AgentEndpoint>();
    let headless = world.get_resource::<HeadlessEditor>();
    json!({
        "event": "ready",
        "pid": std::process::id(),
        "endpoint": endpoint.map(|e| e.dir.join("endpoint.json").display().to_string()),
        "url": endpoint.map(|e| e.url.clone()),
        "project": world
            .get_resource::<crate::agent_ext::EditorProject>()
            .and_then(|p| p.root.as_ref().map(|r| r.display().to_string())),
        "adapter": headless.and_then(|h| h.adapter.clone()),
    })
}

fn print_line(value: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Why the loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitCause {
    /// `agent.shutdown`: documents already saved by the call.
    Shutdown,
    /// `quit.request` or an `AppExit`: the windowed editor's agent quit (dirty level autosaved).
    Quit,
    /// SIGTERM / SIGINT / a console control event: save, then exit.
    Signal,
    /// `--exit-after-frames`: save, then exit.
    Frames,
    /// `--exit-on-stdin-eof`: whoever started the editor is gone. Save, then exit.
    ParentExit,
}

impl ExitCause {
    fn name(self) -> &'static str {
        match self {
            Self::Shutdown => "agent.shutdown",
            Self::Quit => "quit.request",
            Self::Signal => "signal",
            Self::Frames => "exit_after_frames",
            Self::ParentExit => "parent_exit",
        }
    }
}

/// The OS shutdown requests as flags the loop reads each frame.
fn install_signals() -> regia_net::server_host::HostSignals {
    let signals = regia_net::server_host::HostSignals::new();
    match signals.install_os_handlers() {
        Ok(names) => tracing::info!(target: "regia_editor", "headless editor stops on {}", names.join(", ")),
        Err(err) => tracing::warn!(target: "regia_editor", "no signal handlers: {err}"),
    }
    signals
}

/// A flag set when stdin reaches its end (or cannot be read).
fn watch_stdin() -> Arc<AtomicBool> {
    let closed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&closed);
    let spawned = std::thread::Builder::new()
        .name("regia-stdin-watch".into())
        .spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            flag.store(true, Ordering::Release);
        });
    if let Err(err) = spawned {
        tracing::warn!(target: "regia_editor", "--exit-on-stdin-eof not watched: {err}");
    }
    closed
}

/// The parts of the editor loop that the headless loop owns.
struct Loop {
    app: App,
    window: Entity,
    host: Option<HotReloadHost>,
    pie: Option<PieSession>,
}

impl Loop {
    fn current_game(&self) -> Option<crate::pie::CurrentGame> {
        self.app
            .world()
            .get_resource::<crate::pie::CurrentGame>()
            .cloned()
    }

    fn refresh_current_game(&mut self) {
        let current = self.host.as_ref().and_then(|host| {
            host.current_exports()
                .map(|(exports, generation)| crate::pie::CurrentGame {
                    exports,
                    generation,
                    package: host.package_name().to_string(),
                })
        });
        if let Some(current) = current {
            self.app.world_mut().insert_resource(current);
        }
    }

    /// One edit-world frame, in the windowed editor's redraw order.
    fn frame(&mut self) {
        let _span = tracing::info_span!(target: "regia_editor", "headless_frame").entered();
        if let Some(host) = self.host.as_mut() {
            host.frame();
        }
        let pie_mode = self.pie.as_mut().map(PieSession::pause_for_reload);
        let compile = self
            .app
            .world_mut()
            .get_resource_mut::<crate::code_reload::CompileRequest>()
            .is_some_and(|mut request| std::mem::take(&mut request.0));
        if compile && let Some(report) = self.host.as_mut().and_then(HotReloadHost::request_compile) {
            tracing::info!(target: "regia_editor", "{}", report.toast);
        }
        if let Some(report) = self.host.as_mut().and_then(HotReloadHost::poll) {
            tracing::info!(target: "regia_editor", "{}", report.toast);
            if report.accepted {
                self.refresh_current_game();
            }
        }
        // The game build's task row (`editor.cancel_task`, ADR-9770).
        crate::tasks::sync_game_build(
            self.app.world_mut(),
            self.host.as_ref().and_then(HotReloadHost::compile_task),
        );
        let code = crate::code_reload::CodeReloadStatus {
            game: self.host.as_ref().map(|host| host.package_name().to_string()),
            state: self.host.as_ref().map(HotReloadHost::compile_state),
            report_serial: self.host.as_ref().map_or(0, HotReloadHost::report_serial),
        };
        self.app.world_mut().insert_resource(code);
        crate::code_reload::service_hot_reload_job(self.app.world_mut(), self.host.as_ref());
        if let (Some(session), Some(previous)) = (&mut self.pie, pie_mode) {
            session.resume_after_reload(previous);
        }
        crate::asset_watch::pump_asset_watch(
            self.app.world_mut(),
            self.pie.as_mut().map(|session| session.app.world_mut()),
        );
        // Edits and undos feed the open level's dirty set (ADR-7203).
        crate::level_dirty::feed_on_history_change(self.app.world_mut());
        // Play-in-editor: queued requests, then one play-world frame.
        let requests = std::mem::take(
            &mut self
                .app
                .world_mut()
                .get_resource_or_insert_with::<PieRequests>(Default::default)
                .queue,
        );
        for request in requests {
            let game = self.current_game();
            let toast = crate::pie::apply_request(self.app.world_mut(), &mut self.pie, request, game);
            tracing::info!(target: "regia_editor", "{toast}");
        }
        // PIE client worlds (ADR-2623): built before the play world's first frame, stepped in lockstep.
        crate::pie_worlds::pump(self.app.world_mut(), self.pie.as_mut());
        if let Some(session) = self.pie.as_mut()
            && let Some(panic) = session.frame()
        {
            tracing::error!(target: "regia_editor", "stopping play after a game panic: {panic}");
            if let Some(session) = self.pie.take() {
                crate::pie::stop_after_panic(self.app.world_mut(), session);
            }
        }
        crate::pie::sync_status(self.app.world_mut(), self.pie.as_ref());
        // Live link: last frame's edit-world edits into the session (ADR-7833).
        crate::live_link::pump(
            self.app.world_mut(),
            self.pie.as_mut().map(|session| session.app.world_mut()),
        );
        let paused = self.pie.as_ref().is_some_and(|s| s.mode == PlayMode::Paused);
        crate::pie_net::sync_with_play(self.app.world_mut(), self.pie.is_some());
        crate::pie_net::step_frame(self.app.world_mut(), paused);
        crate::abilities_panel::pump(self.app.world_mut(), self.pie.as_mut());
        crate::xr_simulator::pump(self.app.world_mut(), self.pie.as_mut());
        crate::tests_panel::pump(self.app.world_mut(), self.pie.as_mut());
        crate::state_tree_panel::pump(self.app.world_mut(), self.pie.as_mut());
        crate::debug_overlay_selected::pump(self.app.world_mut(), self.pie.as_mut());
        crate::visual_log_panel::pump(self.app.world_mut(), self.pie.as_mut());
        crate::state_tree_editor::pump(self.app.world_mut(), self.pie.as_mut());
        crate::logic_graph_editor::pump(self.app.world_mut(), self.pie.as_mut());
        crate::smart_object_panel::pump(self.app.world_mut(), self.pie.as_mut());
        crate::camera_panel::pump(self.app.world_mut(), self.pie.as_mut());
        // The Match strip's queued controls and the live match view (ADR-8738).
        crate::match_rules_play::pump(self.app.world_mut(), self.pie.as_mut());
        crate::vehicle_tuning::pump(self.app.world_mut(), self.pie.as_mut());
        // A finished DCC bake becomes ordinary content on the frame, not inside the read-only
        // `dcc_feed.bake_status` poll.
        crate::dcc_feed::pump_bakes(self.app.world_mut());
        // The edit world advances (agent requests, level lifecycle, propagation).
        self.app.update();
        crate::collab::tick(self.app.world_mut());
        // These CPU effects also run after Update in the windowed editor. They load the
        // level terrain, record agent strokes in history and finish queued terrain edits.
        {
            let _span = tracing::info_span!(target: "regia_editor", "headless_terrain").entered();
            crate::terrain_tool::frame_effects(self.app.world_mut());
        }
        crate::visibility::mark_internal_entities(self.app.world_mut());
        // Auto key, as the windowed shell runs it once per frame: an agent edit made this frame
        // is keyed at this frame, not only at the next `editor.seq_*` call (ADR-8730; rev-claude-57
        // seq-shot finding 3, ADR-7495).
        crate::seq_tracks::auto_key_frame(self.app.world_mut());
        // Tools ▸ Bake reflection captures advances here too, one capture per frame (ADR-7592).
        crate::capture_bake::tick(self.app.world_mut());
        self.serve_window_screenshots();
        if let Some(mut state) = self.app.world_mut().get_resource_mut::<HeadlessEditor>() {
            state.frames += 1;
        }
    }

    /// Answers `seeing.screenshot` requests for the editor window with the perspective viewport
    /// rendered offscreen (the play world's, while playing), or `editor_headless`.
    fn serve_window_screenshots(&mut self) {
        let window = self.window;
        let callbacks = match self
            .app
            .world_mut()
            .get_resource_mut::<regia_render::ScreenshotRequests>()
        {
            Some(mut requests) => requests.take_for(window),
            None => return,
        };
        if callbacks.is_empty() {
            return;
        }
        let _span = tracing::info_span!(target: "regia_editor", "headless_window_capture").entered();
        let result = self.capture_window();
        for callback in callbacks {
            callback(result.clone());
        }
    }

    fn capture_window(&mut self) -> Result<regia_render::CapturedImage, String> {
        if let Some(err) = no_adapter_error(self.app.world()) {
            return Err(format!("{EDITOR_HEADLESS}: {}", err.message));
        }
        let device = self
            .app
            .world()
            .get_resource::<regia_render::RenderDevice>()
            .cloned()
            .ok_or_else(|| format!("{EDITOR_HEADLESS}: no render device"))?;
        let camera = self
            .app
            .world()
            .get_resource::<crate::viewport::EditorCamera>()
            .cloned()
            .unwrap_or_default();
        let world = match self.pie.as_mut() {
            Some(session) => session.app.world_mut(),
            None => self.app.world_mut(),
        };
        let image = crate::render::capture_viewport(&device, world, &camera, WINDOW_CAPTURE_SIZE)
            .ok_or_else(|| "the offscreen viewport render failed (see logs.read)".to_string())?;
        let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
        for pixel in &image.pixels {
            rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
        }
        Ok(regia_render::CapturedImage {
            width: image.width() as u32,
            height: image.height() as u32,
            rgba,
        })
    }

    /// Ends the session: saves (or autosaves, for `quit.request`) and returns the exit status.
    fn finish(mut self, cause: ExitCause) -> i32 {
        if cause == ExitCause::Shutdown {
            std::thread::sleep(SHUTDOWN_REPLY_GRACE);
        }
        if let Some(session) = self.pie.take() {
            crate::pie::stop_play(session);
        }
        let world = self.app.world_mut();
        // The view state's last changes (ADR-7275).
        crate::view_state::flush(world);
        let root = world
            .get_resource::<crate::agent_ext::EditorProject>()
            .and_then(|p| p.root.clone());
        let mut summary = SaveSummary::default();
        let mut autosaved = None;
        match cause {
            ExitCause::Quit => {
                if let Some(root) = &root {
                    match crate::autosave::autosave_unsaved_level(world, root) {
                        Some(Ok(path)) => autosaved = Some(shown(Some(root), &path)),
                        Some(Err(err)) => summary.failed.push(format!("the level autosave: {err}")),
                        None => {}
                    }
                }
            }
            // `agent.shutdown` already saved (or was told not to).
            ExitCause::Shutdown => {
                summary.saved = world
                    .get_resource::<HeadlessEditor>()
                    .map(|h| h.shutdown_saved.clone())
                    .unwrap_or_default();
            }
            ExitCause::Signal | ExitCause::Frames | ExitCause::ParentExit => {
                summary = save_dirty_state(world);
            }
        }
        // A generator or test run still going is stopped with its process tree, never left behind.
        // After the save: documents with edits come first whatever a job does.
        if let Some(mut worker) = world.remove_resource::<GenWorker>() {
            worker.stop();
        }
        if let Some(jobs) = world.get_resource::<regia_agent::jobs::JobRegistry>() {
            let left = jobs.stop_running(JOB_STOP_GRACE);
            if left > 0 {
                tracing::warn!(target: "regia_editor", "{left} job(s) still running at exit");
            }
        }
        // The autosaves of this session go with a clean end; a crashed session's stay on offer.
        if let Some(root) = &root
            && autosaved.is_none()
            && summary.failed.is_empty()
        {
            let keep: Vec<PathBuf> = crate::autosave::recovery_candidates(root)
                .into_iter()
                .filter(crate::autosave::should_offer_recovery)
                .map(|c| c.autosave)
                .collect();
            crate::autosave::clear_autosaves_except(root, &keep);
        }
        let frames = world.get_resource::<HeadlessEditor>().map_or(0, |h| h.frames);
        if let Some(endpoint) = world.get_resource::<regia_agent::AgentEndpoint>() {
            regia_agent::endpoint::remove(&endpoint.dir);
        }
        let status = i32::from(!summary.failed.is_empty());
        print_line(&json!({
            "event": "exit",
            "cause": cause.name(),
            "frames": frames,
            "saved": summary.saved,
            "failed": summary.failed,
            "autosaved": autosaved,
            "status": status,
        }));
        status
    }
}

/// Runs the headless editor until a shutdown, returning the process exit status (0 clean, 1
/// when a dirty document could not be saved). `app` is the edit world (agent server started,
/// see `main.rs`), `window` the entity screenshots of the editor window address, `host` the
/// hot-reload host of `--game`.
pub fn run(mut app: App, window: Entity, host: Option<HotReloadHost>, options: &HeadlessOptions) -> i32 {
    if !app.world().contains_resource::<HeadlessEditor>() {
        prepare(app.world_mut(), options.gpu);
    }
    let signals = install_signals();
    let mut state = Loop {
        app,
        window,
        host,
        pie: None,
    };
    state.refresh_current_game();
    let stdin_closed = options.exit_on_stdin_eof.then(watch_stdin);
    print_line(&ready_line(state.app.world()));
    let mut frames = 0u64;
    let cause = loop {
        state.frame();
        frames += 1;
        if state
            .app
            .world()
            .get_resource::<HeadlessEditor>()
            .is_some_and(|h| h.shutdown_requested)
        {
            break ExitCause::Shutdown;
        }
        if state
            .app
            .world_mut()
            .get_resource_mut::<crate::quit::EditorQuit>()
            .is_some_and(|mut quit| quit.take())
        {
            break ExitCause::Quit;
        }
        // SIGHUP (the terminal or ssh session went away) ends a headless editor too.
        if signals.take_shutdown() || signals.take_reload() {
            // A second signal while saving ends the process at once (HostSignals).
            signals.mark_draining();
            break ExitCause::Signal;
        }
        if stdin_closed.as_ref().is_some_and(|c| c.load(Ordering::Acquire)) {
            break ExitCause::ParentExit;
        }
        if options.exit_after.is_some_and(|limit| frames >= limit) {
            break ExitCause::Frames;
        }
        std::thread::sleep(options.frame_interval);
    };
    let status = state.finish(cause);
    signals.mark_stopped();
    status
}

// ---------------------------------------------------------------------------------------------
// Agent methods.

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

const P_PATH: ParamSchema = ParamSchema {
    name: "path",
    kind: "string",
    required: true,
    description: "Project-relative level file (`.level.ron`) or level folder (`.level.d`).",
};
const P_DISCARD: ParamSchema = ParamSchema {
    name: "discard",
    kind: "boolean",
    required: false,
    description: "Drop unsaved edits of the open level (default false: a dirty level answers `level_dirty`).",
};
const P_SAVE: ParamSchema = ParamSchema {
    name: "save",
    kind: "boolean",
    required: false,
    description: "Save the documents with edits first (default true). false exits without saving.",
};
const P_NAME: ParamSchema = ParamSchema {
    name: "name",
    kind: "string",
    required: true,
    description: "The generator: generators/<name>.toml.",
};
const P_NAMES: ParamSchema = ParamSchema {
    name: "names",
    kind: "array",
    required: false,
    description: "Generators to verify (default: every generators/*.toml).",
};
const P_WAIT: ParamSchema = ParamSchema {
    name: "wait_ms",
    kind: "integer",
    required: false,
    description: "How long the call waits for the generator (default 5000, at most 25000). A generator \
        still running then answers its job id; 0 answers the job id at once.",
};

/// The methods of this module with their handlers, plus `crate::code_reload::methods`
/// (`editor.code_status`; the reload itself is `agent.hot_reload`, ADR-5993).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    let mut list = vec![
        (
            MethodSchema {
                name: "editor.headless_status",
                tool: "headless_status",
                description: "Whether the editor runs headless, the adapter views render on (null: view methods \
                    answer editor_headless), frames stepped, the open level and whether it is dirty, the project, \
                    live asset reload and the hot-reload game.",
                params: &[],
                result: "{ headless, adapter, no_adapter_reason, frames, project, level, dirty, asset_watch, game }",
                mutating: false,
                permission: Permission::Read,
            },
            headless_status as Handler,
        ),
        (
            MethodSchema {
                name: "editor.open_level",
                tool: "open_level",
                description: "Open a level file of the project in the editor (replacing the open level; undo history \
                    and selection reset). A dirty level answers level_dirty unless discard is true; a prefab edit \
                    session answers prefab_open.",
                params: &[P_PATH, P_DISCARD],
                result: "{ path, level, entities }",
                mutating: true,
                permission: Permission::EditScene,
            },
            open_level as Handler,
        ),
        (
            MethodSchema {
                name: "agent.shutdown",
                tool: "shutdown",
                description: "End the editor: save every document with edits (unless save is false), then exit at the \
                    end of the frame. A failed save answers shutdown_save_failed and the editor keeps running.",
                params: &[P_SAVE],
                result: "{ exiting, saved, headless }",
                mutating: true,
                permission: Permission::Play,
            },
            shutdown as Handler,
        ),
        (
            MethodSchema {
                name: "gen.list",
                tool: "gen_list",
                description: "Every generator declared in generators/*.toml: kind, seed, inputs, outputs, description, \
                    or the error that keeps it from loading.",
                params: &[],
                result: "{ generators: [{ name, ok, kind, seed, description, inputs, outputs, error }] }",
                mutating: false,
                permission: Permission::Read,
            },
            gen_list as Handler,
        ),
        (
            MethodSchema {
                name: "gen.run",
                tool: "gen_run",
                description: "Run a generator: stage its outputs, install the changed files and record generator + seed \
                    in each output's .meta. It runs on a worker as job gen:<n>: within wait_ms the call answers \
                    the report, after that the job id (agent.job_status streams stderr lines as output events). \
                    Runs project code (build permission).",
                params: &[P_NAME, P_WAIT],
                result: "{ generator, kind, seed, outputs: [{ path, bytes, hash, written, provenance_written }], removed } | { job, state: running }",
                mutating: true,
                permission: Permission::Build,
            },
            gen_run as Handler,
        ),
        (
            MethodSchema {
                name: "gen.verify",
                tool: "gen_verify",
                description: "Re-run generators into staging folders and compare every byte with the project: ok, \
                    and a diff list (changed, missing, extra). Never writes the project. It runs on a worker as job \
                    gen:<n>: within wait_ms the call answers the report, after that the job id. Runs project code \
                    (build permission).",
                params: &[P_NAMES, P_WAIT],
                result: "{ ok, generators: [{ generator, files, ok, error }], diffs: [{ generator, path, kind, expected_hash, found_hash, first_difference }] } | { job, state: running }",
                mutating: false,
                permission: Permission::Build,
            },
            gen_verify as Handler,
        ),
    ];
    // `editor.code_status` (specs/23 § 3.6, ADR-5627); the reload is `agent.hot_reload` (ADR-5993).
    list.extend(crate::code_reload::methods());
    list
}

/// Adds this module's methods and the `generators` capability to the world's agent extension.
pub fn install(world: &mut World) {
    let mut extension = world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .cloned()
        .unwrap_or_default();
    for (method, handler) in methods() {
        extension.register(method, handler);
    }
    extension.register_param_shape(
        "gen.verify",
        "names",
        json!({ "type": "array", "items": { "type": "string" } }),
    );
    extension.add_capability(
        "generators",
        "generators/<name>.toml declarations: gen.list, gen.run, gen.verify (byte-identical re-runs, provenance generator + seed).",
    );
    extension.add_capability(
        "editor_headless",
        "editor.headless_status says whether the editor runs without a window; agent.shutdown saves and exits.",
    );
    extension.add_capability(
        "hot_reload",
        "agent.hot_reload rebuilds the hosted game crate and swaps it in with reflected state kept (a job); editor.code_status reports idle, compiling, error or reloaded.",
    );
    world.insert_resource(extension);
}

fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            RpcError::new(
                protocol::NOT_FOUND,
                "no_project",
                "no project is open in the editor",
                "start the editor with --project <dir>",
            )
        })
}

fn headless_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let headless = world.get_resource::<HeadlessEditor>();
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    let level = world
        .get_resource::<OpenLevel>()
        .and_then(|open| open.path.as_deref().map(|p| shown(root.as_deref(), p)));
    let watch = world.get_resource::<regia_asset::AssetWatchStatus>();
    let code = world.get_resource::<crate::code_reload::CodeReloadStatus>();
    Ok(json!({
        "headless": headless.is_some(),
        "adapter": headless.and_then(|h| h.adapter.clone()),
        "no_adapter_reason": headless.and_then(|h| h.no_adapter_reason.clone()),
        "frames": headless.map(|h| h.frames),
        "project": root.as_ref().map(|r| r.display().to_string()),
        "level": level,
        "dirty": world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty),
        "asset_watch": watch.is_some_and(|w| w.watching),
        "game": code.and_then(|c| c.game.clone()),
    }))
}

fn open_level(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let rel = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("path (string) is required"))?;
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    let root = project_root(world)?;
    let full = regia_agent::req_path::project_path_in(&root, "path", rel)?;
    if !full.exists() {
        return Err(RpcError::new(
            protocol::NOT_FOUND,
            "level_not_found",
            format!("no level {rel}"),
            "a project-relative .level.ron file or .level.d folder",
        ));
    }
    if !discard && world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty) {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "level_dirty",
            "the open level has unsaved edits",
            "save it with editor.save_level, or pass discard: true",
        ));
    }
    if world
        .get_resource::<crate::prefab_edit::PrefabEditMode>()
        .is_some_and(|mode| mode.editing().is_some())
    {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "prefab_open",
            "a prefab is being edited",
            "close it with editor.close_prefab before opening a level",
        ));
    }
    let open = level_io::open_level_file(world, &full).map_err(|e| {
        RpcError::new(
            protocol::INVALID_PARAMS,
            "level_unreadable",
            e,
            "check the file with assets.validate",
        )
    })?;
    level_io::reset_edit_state(world);
    world.insert_resource(open.clone());
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .mark_saved();
    let entities = world
        .query::<&regia_scene::prelude::EntityGuid>()
        .iter(world)
        .count();
    Ok(json!({
        "path": shown(Some(&root), &full),
        "level": open.id.0.to_string(),
        "entities": entities,
    }))
}

fn shutdown(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let save = params.get("save").and_then(Value::as_bool).unwrap_or(true);
    let summary = if save {
        save_dirty_state(world)
    } else {
        // Exiting without saving: the edits are dropped deliberately, so nothing is dirty.
        if let Some(mut undo) = world.get_resource_mut::<UndoStack>() {
            undo.mark_saved();
        }
        SaveSummary::default()
    };
    if !summary.failed.is_empty() {
        return Err(RpcError::new(
            protocol::INTERNAL_ERROR,
            "shutdown_save_failed",
            format!("not saved: {}", summary.failed.join("; ")),
            "fix the cause and call agent.shutdown again, or pass save: false to exit without saving",
        ));
    }
    let headless = match world.get_resource_mut::<HeadlessEditor>() {
        Some(mut state) => {
            state.shutdown_requested = true;
            state.shutdown_saved.clone_from(&summary.saved);
            true
        }
        None => {
            // The windowed editor saves its shell documents, then exits through its quit path.
            world.insert_resource(ShutdownRequest);
            false
        }
    };
    Ok(json!({ "exiting": true, "saved": summary.saved, "headless": headless }))
}

fn gen_error(err: &regia_asset_pipeline::generators::GenError) -> RpcError {
    use regia_asset_pipeline::generators::GenError;
    let (code, hint) = match err {
        GenError::NotFound(_) => (protocol::NOT_FOUND, "gen.list shows the declared generators"),
        GenError::Invalid { .. } | GenError::UnsupportedVersion { .. } => (
            protocol::INVALID_PARAMS,
            "fix generators/<name>.toml (see the generator-toml format page)",
        ),
        GenError::InputMissing { .. } => (protocol::NOT_FOUND, "create the input or fix inputs = [...]"),
        GenError::PathDenied { .. } => (
            protocol::INVALID_PARAMS,
            "outputs must be real folders inside the project, not symlinks",
        ),
        _ => (
            protocol::INTERNAL_ERROR,
            "run the generator's command by hand to see its output",
        ),
    };
    RpcError::new(code, err.code(), err.to_string(), hint)
}

fn gen_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let generators: Vec<Value> = regia_asset_pipeline::generators::list(&root)
        .into_iter()
        .map(|(name, loaded)| match loaded {
            Ok(def) => json!({
                "name": name,
                "ok": true,
                "kind": def.kind.name(),
                "seed": def.seed,
                "description": def.description,
                "inputs": def.inputs,
                "outputs": def.outputs,
            }),
            Err(err) => json!({ "name": name, "ok": false, "error": format!("{}: {err}", err.code()) }),
        })
        .collect();
    Ok(json!({ "generators": generators }))
}

/// The default of `wait_ms`: long enough for the generators people run inline, short enough that a
/// long one holds the edit world only this once.
const DEFAULT_GEN_WAIT_MS: u64 = 5000;

/// The largest `wait_ms`: below the protocol client's 30 s default call timeout.
const MAX_GEN_WAIT_MS: u64 = 25_000;

/// The worker of the generator job, if one was started. Dropping it stops the job, so a generator
/// never outlives the editor, also in the windowed editor.
#[derive(Resource, Default)]
struct GenWorker {
    current: Option<(regia_agent::jobs::JobHandle, JoinHandle<()>)>,
}

impl GenWorker {
    /// Asks the job to stop and waits for its worker, which kills the command's process tree.
    fn stop(&mut self) {
        if let Some((handle, thread)) = self.current.take() {
            handle.request_cancel();
            let _ = thread.join();
        }
    }
}

impl Drop for GenWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wait_of(params: &Value) -> Result<Duration, RpcError> {
    let ms = match params.get("wait_ms") {
        None | Some(Value::Null) => DEFAULT_GEN_WAIT_MS,
        Some(value) => value
            .as_u64()
            .filter(|ms| *ms <= MAX_GEN_WAIT_MS)
            .ok_or_else(|| {
                RpcError::invalid_params(format!("wait_ms: an integer in 0..={MAX_GEN_WAIT_MS}"))
            })?,
    };
    Ok(Duration::from_millis(ms))
}

/// Runs `work` as job `gen:<n>` and waits for it up to `wait`: the outcome when it ends in time,
/// else `{ job, state: "running" }`. One generator job at a time (`gen_busy`).
fn gen_job(
    world: &mut World,
    label: String,
    wait: Duration,
    work: impl FnOnce(
        &regia_asset_pipeline::generators::GenControl,
    ) -> Result<Value, regia_asset_pipeline::generators::GenError>
    + Send
    + 'static,
) -> Result<Value, RpcError> {
    use regia_agent::jobs::JobRegistry;
    use regia_asset_pipeline::generators::{GenControl, GenError};
    let mut registry = world.get_resource_or_insert_with::<JobRegistry>(Default::default);
    if let Some(running) = registry.running("gen") {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "gen_busy",
            format!("{running} is still running"),
            "poll it with agent.job_status or stop it with agent.job_cancel",
        ));
    }
    let (job, handle) = registry.start("gen", label, true);
    let (answer, outcome) = mpsc::channel();
    let worker = handle.clone();
    let spawned = std::thread::Builder::new()
        .name("regia-gen-job".into())
        .spawn(move || {
            let _span = tracing::info_span!(target: "regia_editor", "gen_job").entered();
            worker.progress(None, "running");
            let (for_cancel, for_lines) = (worker.clone(), worker.clone());
            let control = GenControl {
                cancel: Some(Arc::new(move || for_cancel.cancel_requested())),
                on_line: Some(Arc::new(move |line: &str| {
                    for_lines.event(json!({ "kind": "output", "line": line }));
                })),
                // A generator under the protocol cannot bless a golden by running a test, or by
                // writing or removing one through a folder output (ADR-2821).
                env_remove: regia_agent::golden_tools::scrubbed_env_names(),
                deny_path: Some(Arc::new(regia_agent::golden_tools::names_golden)),
            };
            let result = work(&control);
            match &result {
                Ok(value) => worker.finish(value.clone()),
                Err(GenError::Cancelled { .. }) => worker.finish(json!({ "cancelled": true })),
                Err(err) => worker.fail(err.code(), err.to_string()),
            }
            let _ = answer.send(result.map_err(|err| gen_error(&err)));
        });
    let thread = match spawned {
        Ok(thread) => thread,
        Err(err) => {
            handle.fail("gen_io", format!("no worker thread: {err}"));
            return Err(RpcError::internal(format!("no worker thread: {err}")));
        }
    };
    let mut slot = world.get_resource_or_insert_with::<GenWorker>(Default::default);
    // The job before this one has ended (`gen_busy` above), so this join does not wait.
    if let Some((_, finished)) = slot.current.replace((handle, thread)) {
        let _ = finished.join();
    }
    match outcome.recv_timeout(wait) {
        Ok(result) => result,
        Err(_) => Ok(json!({ "job": job, "state": "running" })),
    }
}

fn gen_run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("name (string) is required"))?
        .to_string();
    let wait = wait_of(params)?;
    let root = project_root(world)?;
    // ADR-2821: a generator's declared outputs are installed here, so they cannot name a golden.
    let def = regia_asset_pipeline::generators::load(&root, &name).map_err(|e| gen_error(&e))?;
    if let Some((rel, _)) = def
        .output_paths()
        .into_iter()
        .find(|(rel, _)| regia_agent::golden_tools::names_golden(rel))
    {
        return Err(regia_agent::golden_tools::golden_path_denied(&rel));
    }
    gen_job(world, name.clone(), wait, move |control| {
        let report = regia_asset_pipeline::generators::run_observed(&root, &name, control)?;
        Ok(serde_json::to_value(report).unwrap_or(Value::Null))
    })
}

fn gen_verify(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let names: Vec<String> = match params.get("names") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| RpcError::invalid_params("names: an array of strings"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(RpcError::invalid_params("names: an array of strings")),
    };
    let wait = wait_of(params)?;
    let root = project_root(world)?;
    let label = if names.is_empty() {
        "verify all".to_string()
    } else {
        format!("verify {}", names.join(" "))
    };
    gen_job(world, label, wait, move |control| {
        let report = regia_asset_pipeline::generators::verify_observed(&root, &names, control)?;
        let ok = report.ok();
        let mut value = serde_json::to_value(report).unwrap_or(Value::Null);
        if let Some(map) = value.as_object_mut() {
            map.insert("ok".to_string(), Value::Bool(ok));
        }
        Ok(value)
    })
}

#[cfg(test)]
mod tests;
