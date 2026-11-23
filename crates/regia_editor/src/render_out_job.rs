// SPDX-License-Identifier: MPL-2.0
//! Sequence ▸ Render movie…: a project `.seq.ron` rendered to frames or a movie file as a
//! cancellable editor job (`docs/specs/22-tier2.md` T2.6, `docs/specs/19-editor.md` § 9, ADR-7726,
//! `book/agents/render-movie.md`).
//!
//! The dialog (sequence, output folder, format, frame range, size, frame rate) builds a
//! `.render_job.ron` ([`job_for`]) and starts it with `render.out_job`, called in process
//! (`regia_agent::render_out_host`), on the editor's own renderer. So the dialog, `editor.render_movie`
//! and an agent that sends the same job text to `render.out_job` run one code path and write the
//! same frames: the frame count and every frame hash are the job's, not the editor's.
//!
//! The job is the render-out's: it renders one output frame per editor frame (the UI keeps drawing
//! between them, and the frames are encoded and written by the render-out's writer threads), and a
//! frame file is written as `<name>.part` and renamed, so a file under its final name is always
//! whole. [`tick`] (every editor frame, `crate::editor_jobs::tick`) follows it: it publishes the
//! background-task row [`RENDER_MOVIE_TASK`] with the done fraction, records a `render_movie` event
//! (`events.read`) when the frame count moves and when the job ends, and turns a cancel of the
//! row (`editor.cancel_task { id: "render_movie" }`, the dialog's Cancel) into `render.out_cancel`.
//! A cancelled job keeps the whole frames it delivered and writes no later file.
//!
//! The job renders the saved file: save the sequence first. Its binding table is the Sequence
//! Curves panel's for that file (each binding by its entity's scene guid, else its name), plus
//! every other track target an entity is named after, so the job plays the sequence itself.
//!
//! Refusals (`data.reason`): `render_movie_no_project`, `render_movie_bad_sequence`,
//! `path_outside_project` (the output folder is not a project-relative folder inside the project),
//! `render_movie_bad_format`, `render_movie_bad_range`, `render_movie_bad_size`,
//! `render_movie_busy` (a render-out is already running); the job's own refusals pass through with
//! their `render.out_job` codes (`render_job_*`, `editor_headless`, `F0011`).

use std::collections::BTreeMap;

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::render_job::{
    DEFAULT_PATH, JobBinding, OutputSettings, RENDER_JOB_FORMAT_VERSION, RenderJob, Take, TakeRange,
};
use regia_agent::render_out_host;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::sequencer::{Binding, Sequence, SequenceBindings};
use serde_json::{Value, json};

use crate::tasks::{BackgroundTasks, CancelToken, TaskRow};
use crate::viewport_agent::Handler;

/// The background-task id of a running render (`editor.cancel_task { id: "render_movie" }`).
pub const RENDER_MOVIE_TASK: &str = "render_movie";
/// The `events.read` kind of the job's progress records.
pub const RENDER_MOVIE_EVENT: &str = "render_movie";
/// The output formats the dialog offers (`render.out_start` `format`; the movie formats need the
/// render-out's encoder or `ffmpeg`, and the job refuses them where that is missing).
pub const FORMATS: &[&str] = &["png", "exr", "raw", "webm", "ffv1", "prores", "dnxhr"];
/// The take name of the job.
pub const TAKE: &str = "movie";
/// The largest width or height the dialog accepts, pixels.
pub const MAX_SIZE: u32 = 16_384;

/// What one Render movie run renders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderMovieSettings {
    /// Project-relative `.seq.ron`.
    pub sequence: String,
    /// Project-relative output folder.
    pub out: String,
    /// One of [`FORMATS`].
    pub format: String,
    /// First timeline frame.
    pub start: u64,
    /// The frame after the last one; `None`: to the end of the sequence.
    pub end: Option<u64>,
    /// Delivered width, pixels.
    pub width: u32,
    /// Delivered height, pixels.
    pub height: u32,
    /// Frames per second.
    pub fps: u32,
}

impl Default for RenderMovieSettings {
    fn default() -> Self {
        Self {
            sequence: String::new(),
            out: "renders/movie".into(),
            format: "png".into(),
            start: 0,
            end: None,
            width: 1920,
            height: 1080,
            fps: 30,
        }
    }
}

/// Why a Render movie request was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovieError {
    /// The lowercase protocol reason (`data.reason`).
    pub reason: &'static str,
    /// What to show the user.
    pub message: String,
}

impl MovieError {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }

    /// The protocol error of this refusal (`F0002`, or `F0003` without a project).
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let (code, regia) = match self.reason {
            "render_movie_no_project" => (NOT_FOUND, "F0003"),
            _ => (INVALID_PARAMS, "F0002"),
        };
        let mut error = RpcError::new(
            code,
            regia,
            self.message.clone(),
            "see book/agents/render-movie.md",
        );
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert("reason".into(), Value::String(self.reason.to_string()));
        }
        error
    }
}

impl std::fmt::Display for MovieError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for MovieError {}

/// How a run ended, or that it still runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovieState {
    /// Frames are being rendered (or the outputs closed).
    Running,
    /// Every frame was written.
    Done,
    /// Cancelled: the whole frames delivered so far stay.
    Cancelled,
    /// The job failed.
    Failed,
}

impl MovieState {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// The current (or last) Render movie run.
#[derive(Resource, Clone, Debug)]
pub struct RenderMovieJob {
    /// The render-out job id (`render.out_progress { job }`).
    pub job: u64,
    /// The `.render_job.ron` text the run started with.
    pub job_text: String,
    /// What the dialog asked for.
    pub settings: RenderMovieSettings,
    /// Running or how it ended.
    pub state: MovieState,
    /// Frames delivered so far.
    pub frame: u64,
    /// Frames the job renders (pre-roll included).
    pub frames: u64,
    /// The hash of the first delivered frame (hex), once known.
    pub first_hash: Option<String>,
    /// The manifest path the job wrote (as `render.out_progress` shows it).
    pub manifest: Option<String>,
    /// The job's error text, when it failed.
    pub error: Option<String>,
    /// The background-task row's cancel flag.
    cancel: CancelToken,
    /// `render.out_cancel` was sent.
    cancel_sent: bool,
    /// The frame count of the last `render_movie` event.
    reported: Option<u64>,
}

impl RenderMovieJob {
    /// Whether the run still renders.
    #[must_use]
    pub fn running(&self) -> bool {
        self.state == MovieState::Running
    }

    /// The `editor.render_movie_status` answer.
    #[must_use]
    pub fn status_json(&self) -> Value {
        json!({
            "task": RENDER_MOVIE_TASK,
            "job": self.job,
            "state": self.state.name(),
            "frame": self.frame,
            "frames": self.frames,
            "first_hash": self.first_hash,
            "manifest": self.manifest,
            "error": self.error,
            "sequence": self.settings.sequence,
            "out": self.settings.out,
            "format": self.settings.format,
        })
    }
}

/// The dialog: open flag, its fields and the last refusal.
#[derive(Resource, Clone, Debug, Default)]
pub struct RenderMovieDialog {
    /// Drawn this frame.
    pub open: bool,
    /// The fields.
    pub settings: RenderMovieSettings,
    /// Whether the range field runs to the end of the sequence.
    pub whole: bool,
    /// The last refusal, shown under the fields.
    pub error: Option<String>,
}

/// The project root, or `render_movie_no_project`.
fn root(world: &World) -> Result<std::path::PathBuf, MovieError> {
    crate::mesh_source::project_root(world)
        .ok_or_else(|| MovieError::new("render_movie_no_project", "open a project first"))
}

/// The binding table of the job: the Sequence Curves panel's table of the player that has
/// `sequence` open (each entry by its entity's scene guid, else its `Name`), then every other
/// track target some entity is named after.
fn bindings_for(world: &mut World, path: &str, sequence: &Sequence) -> BTreeMap<String, JobBinding> {
    use regia_scene::components::{EntityGuid, Name};
    let mut out = BTreeMap::new();
    let player = world
        .get_resource::<crate::seq_curve_panel::SeqCurvePanel>()
        .and_then(|panel| {
            panel
                .paths
                .iter()
                .filter(|(_, p)| p.as_str() == path)
                .map(|(e, _)| *e)
                .min_by_key(|e| e.to_bits())
        });
    if let Some(table) = player.and_then(|e| world.get::<SequenceBindings>(e)).cloned() {
        for (name, binding) in table.iter() {
            let bound = match binding {
                Binding::Guid(guid) => Some(JobBinding::Guid(guid.to_string())),
                Binding::Entity(entity) => world
                    .get::<EntityGuid>(entity)
                    .map(|g| JobBinding::Guid(g.0.to_string()))
                    .or_else(|| {
                        world
                            .get::<Name>(entity)
                            .map(|n| JobBinding::Name(n.as_str().to_string()))
                    }),
            };
            if let Some(bound) = bound {
                out.insert(name.to_string(), bound);
            }
        }
    }
    let mut named = world.query::<&Name>();
    let names: std::collections::BTreeSet<String> =
        named.iter(world).map(|n| n.as_str().to_string()).collect();
    for track in &sequence.tracks {
        let target = track.target.trim();
        if !target.is_empty() && !out.contains_key(target) && names.contains(target) {
            out.insert(target.to_string(), JobBinding::Name(target.to_string()));
        }
    }
    out
}

/// A job name from the sequence file's stem: letters, digits, `_`, `-` and `.` (others become `_`).
fn job_name(path: &str) -> String {
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".seq.ron");
    let name: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let name = name.trim_start_matches('.').to_string();
    if name.is_empty() { "movie".into() } else { name }
}

/// The `.render_job.ron` a run of `settings` starts: the checks of the dialog, then the job.
///
/// # Errors
/// The refusals listed in the module docs (not the job's own; those come when it starts).
pub fn job_for(world: &mut World, settings: &RenderMovieSettings) -> Result<RenderJob, MovieError> {
    let root = root(world)?;
    let path = settings.sequence.trim();
    if !path.ends_with(".seq.ron") {
        return Err(MovieError::new(
            "render_movie_bad_sequence",
            format!("`{path}` must be a project path ending in .seq.ron"),
        ));
    }
    let file = regia_core::project_path::resolve_read(&root, path).map_err(|e| {
        MovieError::new(
            "render_movie_bad_sequence",
            format!("{path}: must be a project-relative path inside the project: {e}"),
        )
    })?;
    let text = std::fs::read_to_string(&file)
        .map_err(|e| MovieError::new("render_movie_bad_sequence", format!("cannot read {path}: {e}")))?;
    let sequence = Sequence::from_ron(&text)
        .map_err(|e| MovieError::new("render_movie_bad_sequence", format!("{path}: {e}")))?;
    let out = settings.out.trim().trim_end_matches('/');
    if out.is_empty() {
        return Err(MovieError::new(
            "path_outside_project",
            "the output folder must be a project-relative folder",
        ));
    }
    regia_core::project_path::resolve_write(&root, out).map_err(|e| {
        MovieError::new(
            "path_outside_project",
            format!("{out}: the output folder must be a project-relative folder inside the project: {e}"),
        )
    })?;
    if !FORMATS.contains(&settings.format.as_str()) {
        return Err(MovieError::new(
            "render_movie_bad_format",
            format!("`{}` is not one of {}", settings.format, FORMATS.join(", ")),
        ));
    }
    if settings.width == 0 || settings.height == 0 || settings.width > MAX_SIZE || settings.height > MAX_SIZE
    {
        return Err(MovieError::new(
            "render_movie_bad_size",
            format!(
                "{}x{} must be 1..={MAX_SIZE} pixels each way",
                settings.width, settings.height
            ),
        ));
    }
    if !(1..=1000).contains(&settings.fps) {
        return Err(MovieError::new(
            "render_movie_bad_size",
            format!("{} fps must be 1..=1000", settings.fps),
        ));
    }
    let range = match settings.end {
        None if settings.start == 0 => TakeRange::Whole,
        None => {
            let seconds = sequence.duration();
            let end = (seconds * f64::from(settings.fps)).ceil() as u64;
            TakeRange::Frames {
                start: settings.start,
                end: end.max(settings.start + 1),
            }
        }
        Some(end) => {
            if end <= settings.start {
                return Err(MovieError::new(
                    "render_movie_bad_range",
                    format!(
                        "frames {}..{end}: the end must come after the start",
                        settings.start
                    ),
                ));
            }
            TakeRange::Frames {
                start: settings.start,
                end,
            }
        }
    };
    Ok(RenderJob {
        version: RENDER_JOB_FORMAT_VERSION,
        name: job_name(path),
        shots: None,
        sequence: Some(path.to_string()),
        bindings: bindings_for(world, path, &sequence),
        out: out.to_string(),
        path: DEFAULT_PATH.to_string(),
        output: OutputSettings {
            width: Some(settings.width),
            height: Some(settings.height),
            fps: Some(settings.fps),
            format: Some(settings.format.clone()),
            hud: Some(false),
            ..OutputSettings::default()
        },
        takes: vec![Take {
            name: TAKE.into(),
            range,
            path: None,
        }],
        cuts: Vec::new(),
        max_preroll_frames: None,
        product: None,
    })
}

/// Starts a run of `settings` (the dialog's Render, `editor.render_movie`). Answers the job id,
/// the job text and the frame count.
///
/// # Errors
/// The refusals of [`job_for`], `render_movie_busy`, and the job's own (`render.out_job`).
pub fn start(world: &mut World, settings: &RenderMovieSettings) -> Result<Value, RpcError> {
    if render_out_host::job_running(world) {
        return Err(MovieError::new(
            "render_movie_busy",
            "a render-out is already running: cancel it or wait for it to end",
        )
        .rpc());
    }
    let job = job_for(world, settings).map_err(|e| e.rpc())?;
    // The Sequence Curves panel's edit-world preview writes the sequence at its playhead every
    // frame; it would fight the job's own player, so it stops first (and puts its values back).
    crate::seq_tracks::stop_preview(world);
    let text = job
        .to_ron()
        .map_err(|e| MovieError::new("render_movie_bad_sequence", e.to_string()).rpc())?;
    let started = render_out_host::out_job(world, &json!({ "job": text }))?;
    let id = started
        .get("job")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::internal("render.out_job answered no job id"))?;
    let frames = started
        .get("plan")
        .and_then(|p| p.get("frames"))
        .and_then(Value::as_u64)
        .or_else(|| started.pointer("/request/frames").and_then(Value::as_u64))
        .unwrap_or(0);
    let run = RenderMovieJob {
        job: id,
        job_text: text.clone(),
        settings: settings.clone(),
        state: MovieState::Running,
        frame: 0,
        frames,
        first_hash: None,
        manifest: None,
        error: None,
        cancel: CancelToken::new(),
        cancel_sent: false,
        reported: None,
    };
    publish_row(world, &run);
    record_event(world, &run);
    let mut run = run;
    run.reported = Some(0);
    world.insert_resource(run);
    Ok(json!({
        "task": RENDER_MOVIE_TASK,
        "job": id,
        "job_text": text,
        "frames": frames,
        "out": settings.out.trim().trim_end_matches('/'),
        "state": "running",
    }))
}

fn publish_row(world: &mut World, run: &RenderMovieJob) {
    let fraction = if run.frames == 0 {
        None
    } else {
        Some(run.frame as f32 / run.frames as f32)
    };
    world
        .get_resource_or_insert_with(BackgroundTasks::default)
        .publish(TaskRow {
            id: RENDER_MOVIE_TASK.to_string(),
            label: format!(
                "rendering {} ({}/{})",
                run.settings.sequence, run.frame, run.frames
            ),
            progress: fraction,
            cancel: run.cancel.clone(),
        });
}

fn record_event(world: &mut World, run: &RenderMovieJob) {
    let frame = world
        .get_resource::<regia_app::prelude::Time>()
        .map_or(0, regia_app::prelude::Time::frame_count);
    if let Some(mut log) = world.get_resource_mut::<regia_agent::AgentEventLog>() {
        log.push_host(frame, RENDER_MOVIE_EVENT, run.status_json());
    }
}

/// Cancels the running job (the dialog's Cancel): the next [`tick`] sends `render.out_cancel`.
/// Answers whether a job was running.
pub fn cancel(world: &mut World) -> bool {
    let Some(run) = world.get_resource::<RenderMovieJob>() else {
        return false;
    };
    if !run.running() {
        return false;
    }
    run.cancel.cancel();
    tick(world);
    true
}

/// Follows the running job (every editor frame): a cancelled task row becomes
/// `render.out_cancel`, the row and the `render_movie` events follow the delivered frames, and
/// the job's end is recorded. Does nothing without a running job.
pub fn tick(world: &mut World) {
    let Some(mut run) = world.get_resource::<RenderMovieJob>().cloned() else {
        return;
    };
    if !run.running() {
        return;
    }
    let _span = tracing::info_span!("editor_render_movie_tick", frame = run.frame).entered();
    if run.cancel.is_cancelled() && !run.cancel_sent {
        run.cancel_sent = true;
        if let Err(e) = render_out_host::cancel(world, &json!({ "job": run.job })) {
            tracing::warn!(target: "regia_editor", "render movie: cancel: {}", e.message);
        }
    }
    // The hash window (up to 4096 hex strings) only until the first frame's hash is known: this
    // runs every editor frame of a long render.
    let params = json!({ "job": run.job, "hashes": run.first_hash.is_none() });
    let progress = match render_out_host::progress(world, &params) {
        Ok(progress) => progress,
        Err(e) => {
            run.state = MovieState::Failed;
            run.error = Some(e.message);
            end(world, run);
            return;
        }
    };
    run.frame = progress.get("frame").and_then(Value::as_u64).unwrap_or(run.frame);
    run.frames = progress
        .get("frames")
        .and_then(Value::as_u64)
        .unwrap_or(run.frames);
    if run.first_hash.is_none() {
        run.first_hash = progress
            .pointer("/hashes/hashes/0")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let state = match progress.get("status").and_then(Value::as_str) {
        Some("done") => MovieState::Done,
        Some("cancelled") => MovieState::Cancelled,
        Some("failed") => MovieState::Failed,
        _ => MovieState::Running,
    };
    run.state = state;
    if state == MovieState::Running {
        publish_row(world, &run);
        if run.reported != Some(run.frame) {
            record_event(world, &run);
            run.reported = Some(run.frame);
        }
        world.insert_resource(run);
        return;
    }
    run.manifest = progress
        .get("manifest")
        .and_then(Value::as_str)
        .map(str::to_string);
    run.error = progress.get("error").and_then(Value::as_str).map(str::to_string);
    end(world, run);
}

fn end(world: &mut World, run: RenderMovieJob) {
    if let Some(mut tasks) = world.get_resource_mut::<BackgroundTasks>() {
        tasks.finish(RENDER_MOVIE_TASK);
    }
    record_event(world, &run);
    world.insert_resource(run);
}

// ---------------------------------------------------------------------------------------------
// Menu and dialog

/// Sequence ▸ Render movie…: opens the dialog, its sequence field set to the Sequence Curves
/// panel's file when it has one. Answers the toast line.
pub fn menu_item(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let project = crate::mesh_source::project_root(world).is_some();
    if crate::agent_parity::action_enabled(ui, project, &crate::agent_parity::SEQUENCE_RENDER_MOVIE, None)
        .on_hover_text("Render a saved .seq.ron to frames or a movie file in the project (cancellable)")
        .clicked()
    {
        ui.close();
        let mut dialog = world
            .remove_resource::<RenderMovieDialog>()
            .unwrap_or_else(|| RenderMovieDialog {
                whole: true,
                ..RenderMovieDialog::default()
            });
        if dialog.settings.sequence.is_empty()
            && let Some(panel) = world.get_resource::<crate::seq_curve_panel::SeqCurvePanel>()
            && let Some(path) = panel.paths.values().next()
        {
            dialog.settings.sequence = path.clone();
        }
        dialog.open = true;
        dialog.error = None;
        world.insert_resource(dialog);
    }
    None
}

/// The dialog, and the running job's progress and Cancel. The shell calls it every frame
/// (`crate::menus::windows`).
pub fn window(ctx: &egui::Context, world: &mut World) {
    let running = world.get_resource::<RenderMovieJob>().cloned();
    let Some(mut dialog) = world.remove_resource::<RenderMovieDialog>() else {
        return;
    };
    if !dialog.open {
        world.insert_resource(dialog);
        return;
    }
    let mut open = true;
    egui::Window::new("Render movie")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            let busy = running.as_ref().is_some_and(RenderMovieJob::running);
            ui.add_enabled_ui(!busy, |ui| {
                fields(ui, &mut dialog);
            });
            match &running {
                Some(run) if run.running() => {
                    ui.label(format!("Rendering frame {} of {}", run.frame, run.frames));
                    let fraction = if run.frames == 0 {
                        0.0
                    } else {
                        run.frame as f32 / run.frames as f32
                    };
                    ui.add(egui::ProgressBar::new(fraction).show_percentage());
                    if crate::agent_parity::action_button(ui, &crate::agent_parity::RENDER_MOVIE_CANCEL)
                        .clicked()
                    {
                        crate::tasks::cancel(world, RENDER_MOVIE_TASK);
                    }
                }
                _ => {
                    if let Some(run) = &running {
                        ui.label(format!(
                            "Last run: {} ({} of {} frames){}",
                            run.state.name(),
                            run.frame,
                            run.frames,
                            run.error.as_ref().map_or(String::new(), |e| format!(": {e}"))
                        ));
                    }
                    if crate::agent_parity::action_button(ui, &crate::agent_parity::RENDER_MOVIE_START)
                        .clicked()
                    {
                        let mut settings = dialog.settings.clone();
                        if dialog.whole {
                            settings.end = None;
                        }
                        dialog.error = start(world, &settings).err().map(|e| e.message);
                    }
                }
            }
            if let Some(error) = &dialog.error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        });
    dialog.open = open;
    world.insert_resource(dialog);
}

fn fields(ui: &mut egui::Ui, dialog: &mut RenderMovieDialog) {
    let s = &mut dialog.settings;
    egui::Grid::new("render_movie_fields")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Sequence");
            let field = ui.add(egui::TextEdit::singleline(&mut s.sequence).hint_text("cine/intro.seq.ron"));
            crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Sequence file");
            ui.end_row();
            ui.label("Output folder");
            let field = ui.add(egui::TextEdit::singleline(&mut s.out).hint_text("renders/movie"));
            crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Output folder");
            ui.end_row();
            ui.label("Format");
            egui::ComboBox::from_id_salt("render_movie_format")
                .selected_text(s.format.clone())
                .show_ui(ui, |ui| {
                    for format in FORMATS {
                        ui.selectable_value(&mut s.format, (*format).to_string(), *format);
                    }
                });
            ui.end_row();
            ui.label("Frames");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut s.start).prefix("from "));
                let mut end = s.end.unwrap_or(s.start + 1);
                ui.add_enabled_ui(!dialog.whole, |ui| {
                    ui.add(
                        egui::DragValue::new(&mut end)
                            .prefix("to ")
                            .suffix(" (not included)"),
                    );
                });
                if !dialog.whole {
                    s.end = Some(end);
                }
                ui.checkbox(&mut dialog.whole, "to the end");
            });
            ui.end_row();
            ui.label("Size");
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut s.width)
                        .range(1..=MAX_SIZE)
                        .suffix(" px"),
                );
                ui.label("×");
                ui.add(
                    egui::DragValue::new(&mut s.height)
                        .range(1..=MAX_SIZE)
                        .suffix(" px"),
                );
                ui.add(egui::DragValue::new(&mut s.fps).range(1..=1000).suffix(" fps"));
            });
            ui.end_row();
        });
}

// ---------------------------------------------------------------------------------------------
// Agent methods

fn settings_of(params: &Value) -> Result<RenderMovieSettings, RpcError> {
    let defaults = RenderMovieSettings::default();
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
    let whole = |key: &str| -> Result<Option<u64>, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a whole number"))),
        }
    };
    let size = |key: &str, default: u32| -> Result<u32, RpcError> {
        whole(key)?.map_or(Ok(default), |n| {
            u32::try_from(n).map_err(|_| RpcError::invalid_params(format!("`{key}` is too large")))
        })
    };
    Ok(RenderMovieSettings {
        sequence: text("sequence")
            .ok_or_else(|| RpcError::invalid_params("`sequence` (a project .seq.ron path) is required"))?,
        out: text("out").unwrap_or(defaults.out),
        format: text("format").unwrap_or(defaults.format),
        start: whole("start")?.unwrap_or(0),
        end: whole("end")?,
        width: size("width", defaults.width)?,
        height: size("height", defaults.height)?,
        fps: size("fps", defaults.fps)?,
    })
}

/// `editor.render_movie`.
fn render_movie(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let settings = settings_of(params)?;
    if params.get("dry_run").and_then(Value::as_bool) == Some(true) {
        let job = job_for(world, &settings).map_err(|e| e.rpc())?;
        let text = job
            .to_ron()
            .map_err(|e| MovieError::new("render_movie_bad_sequence", e.to_string()).rpc())?;
        return Ok(json!({ "dry_run": true, "job_text": text }));
    }
    start(world, &settings)
}

/// `editor.render_movie_status`.
fn render_movie_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    tick(world);
    Ok(world.get_resource::<RenderMovieJob>().map_or_else(
        || json!({ "task": RENDER_MOVIE_TASK, "state": "idle" }),
        RenderMovieJob::status_json,
    ))
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.render_movie",
                tool: "render_movie",
                description: "Sequence ▸ Render movie…: render a saved project .seq.ron with the editor's renderer as \
                    a cancellable editor job (background task render_movie): builds the .render_job.ron the \
                    dialog builds (one take `movie`, the Sequence Curves panel's bindings, hud off) and starts it \
                    with render.out_job, so the frames and hashes are that job's. Progress: `render_movie` records \
                    in events.read and editor.render_movie_status; cancel with editor.cancel_task { id: \
                    \"render_movie\" } (the frames delivered stay, each written whole). Refusals (data.reason): \
                    render_movie_no_project, render_movie_bad_sequence, path_outside_project, \
                    render_movie_bad_format, render_movie_bad_range, render_movie_bad_size, render_movie_busy, \
                    and the render.out_job codes.",
                params: &[
                    ParamSchema {
                        name: "sequence",
                        kind: "string",
                        required: true,
                        description: "Project-relative path ending in .seq.ron (the saved file is rendered).",
                    },
                    ParamSchema {
                        name: "out",
                        kind: "string",
                        required: false,
                        description: "Project-relative output folder inside the project (default renders/movie).",
                    },
                    ParamSchema {
                        name: "format",
                        kind: "string",
                        required: false,
                        description: "png (default), exr, raw, webm, ffv1, prores or dnxhr.",
                    },
                    ParamSchema {
                        name: "start",
                        kind: "number",
                        required: false,
                        description: "First timeline frame (default 0).",
                    },
                    ParamSchema {
                        name: "end",
                        kind: "number",
                        required: false,
                        description: "The frame after the last (default: the end of the sequence).",
                    },
                    ParamSchema {
                        name: "width",
                        kind: "number",
                        required: false,
                        description: "Width in pixels (default 1920).",
                    },
                    ParamSchema {
                        name: "height",
                        kind: "number",
                        required: false,
                        description: "Height in pixels (default 1080).",
                    },
                    ParamSchema {
                        name: "fps",
                        kind: "number",
                        required: false,
                        description: "Frames per second (default 30).",
                    },
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "Check the settings and answer the job text without starting it.",
                    },
                ],
                result: "{ task, job, job_text, frames, out, state } (dry run: { dry_run, job_text })",
                mutating: true,
                permission: Permission::EditScene,
            },
            render_movie as Handler,
        ),
        (
            MethodSchema {
                name: "editor.render_movie_status",
                tool: "render_movie_status",
                description: "The Render movie run: state (idle, running, done, cancelled, failed), the render-out \
                    job id, frames delivered and planned, the first frame's hash, the manifest path, the error \
                    of a failed run, and its sequence, folder and format.",
                params: &[],
                result: "{ task, job, state, frame, frames, first_hash, manifest, error, sequence, out, format }",
                mutating: false,
                permission: Permission::Read,
            },
            render_movie_status as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
