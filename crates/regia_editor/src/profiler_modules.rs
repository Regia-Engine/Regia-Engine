// SPDX-License-Identifier: MPL-2.0
//! The Profiler panel's subsystem modules, monitors, loads and marks (`docs/specs/19-editor.md`
//! § 10, ADR-8141; `book/agents/profiler.md`), beside the panel's own tabs
//! ([`crate::profiler_panel`]):
//!
//! - **Percent of frame** (P-304): the flame and timeline views show a span's time in ms or as a
//!   share of the frame ([`frame_shares`]; the top level plus the time outside every span sums to
//!   100 %).
//! - **Modules** (P-307): Audio, Physics, UI and Rendering tabs, each a chart of one metric over
//!   the history and a details table (last, min, max, average of every metric). The metrics are
//!   counters each subsystem already keeps ([`Module::metrics`] names the source of each);
//!   [`collect`] samples them once per repaint from the edit world (the play world through
//!   [`note_play_world`] while the play source is shown) and also lists them as telemetry counters
//!   (`physics.step_ms`, ...), so the Monitors tab graphs them too.
//! - **Monitors** (P-312): any named counter from the telemetry counter listing
//!   ([`regia::telemetry::counters`]: built-in engine counters and a game's own through
//!   `counters::counter`), several on one chart, with min, max and average.
//! - **Loads** (P-314): the asset server's per-asset load times (read, decode, upload), bytes and
//!   live reference counts, sortable by any column ([`load_rows`], [`sort_loads`]).
//! - **Marks** (P-318): trace marks, level loads and hitches on the History graph and the thread
//!   timeline, labelled on hover and listed for the keyboard ([`history_marks`]).
//!
//! Everything shown is computed by pure functions here, so it is tested without a GPU; the window
//! only paints it. Agent twins: `editor.profiler_units`, `editor.profiler_modules`,
//! `editor.profiler_monitors`, `editor.profiler_loads` and `editor.profiler_marks`.

use std::collections::VecDeque;

use regia::telemetry::counters::{self, MarkEntry, Summary, summarize};
use regia::telemetry::profile_capture::ProfileCapture;
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::ecs::message::MessageCursor;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// Samples each module keeps (the frame profiler's history length).
pub const MODULE_HISTORY: usize = crate::profiler_model::HISTORY_FRAMES;

/// Most counters drawn on the Monitors chart at once.
pub const MAX_MONITORS: usize = 8;

/// Height of a module or monitor chart, points.
pub const CHART_HEIGHT: f32 = 90.0;

/// The row name of the time outside every top-level span in [`frame_shares`].
pub const OUTSIDE_SPANS: &str = "(outside spans)";

// ---------------------------------------------------------------------------------------------
// Percent of frame (P-304)

/// `ms` as a percentage of `frame_ms` (0 when the frame time is not positive).
#[must_use]
pub fn percent_of(ms: f64, frame_ms: f64) -> f64 {
    if frame_ms > 0.0 {
        ms / frame_ms * 100.0
    } else {
        0.0
    }
}

/// A time as the panel shows it: `1.234 ms`, or `12.3 %` of `frame_ms` in percent mode.
#[must_use]
pub fn time_text(ms: f64, frame_ms: f64, percent: bool) -> String {
    if percent {
        format!("{:.1} %", percent_of(ms, frame_ms))
    } else {
        format!("{ms:.3} ms")
    }
}

/// One top-level part of a frame and its share.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct FrameShare {
    /// The span (or [`OUTSIDE_SPANS`]).
    pub name: String,
    /// Mean time per frame, ms.
    pub ms: f64,
    /// Share of the frame, percent.
    pub percent: f64,
}

/// The frame time the percentages divide by: the capture's frame time, or the root spans' sum
/// when that is larger (averaged roots can exceed an averaged frame by rounding).
#[must_use]
pub fn percent_base_ms(capture: &ProfileCapture) -> f64 {
    let roots: f64 = capture.cpu.iter().map(|n| n.inclusive_ms).sum();
    capture.frame_ms.max(roots)
}

/// The capture's top-level spans with their share of the frame, plus [`OUTSIDE_SPANS`] for the
/// rest of the frame when there is any: the shares sum to 100 % (for a frame with any time).
#[must_use]
pub fn frame_shares(capture: &ProfileCapture) -> Vec<FrameShare> {
    let base = percent_base_ms(capture);
    let mut out: Vec<FrameShare> = capture
        .cpu
        .iter()
        .map(|n| FrameShare {
            name: n.name.clone(),
            ms: n.inclusive_ms,
            percent: percent_of(n.inclusive_ms, base),
        })
        .collect();
    let roots: f64 = out.iter().map(|s| s.ms).sum();
    let rest = base - roots;
    if rest > 1e-9 {
        out.push(FrameShare {
            name: OUTSIDE_SPANS.to_owned(),
            ms: rest,
            percent: percent_of(rest, base),
        });
    }
    out
}

/// Whether the flame and timeline views show percent of frame ([`ProfilerModulesView::percent`]).
#[must_use]
pub fn percent_mode(world: &World) -> bool {
    world
        .get_resource::<ProfilerModulesView>()
        .is_some_and(|v| v.percent)
}

/// The "% of frame" checkbox of the flame and timeline views (Tab, then Space toggles it). Returns
/// the mode after this frame.
pub fn percent_toggle(ui: &mut egui::Ui, world: &mut World) -> bool {
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    let mut on = view.percent;
    // UI site: agent twin `editor.profiler_units { percent }`.
    if ui
        .checkbox(&mut on, "% of frame")
        .on_hover_text("Show span times as a share of the frame instead of ms")
        .changed()
        && view.act_once(ui.ctx().cumulative_frame_nr())
    {
        view.percent = on;
    }
    view.percent
}

// ---------------------------------------------------------------------------------------------
// Modules (P-307)

/// A subsystem module tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Module {
    /// Voices, streaming.
    Audio,
    /// Bodies, contacts, step time, islands.
    Physics,
    /// Runtime UI nodes, layout time, draw batches.
    Ui,
    /// GPU passes, draws, pipelines.
    Rendering,
}

/// One metric of a module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metric {
    /// Key (`step_ms`); the telemetry counter is `<module>.<key>`.
    pub key: &'static str,
    /// Label in the details table.
    pub label: &'static str,
    /// Where the value comes from.
    pub source: &'static str,
}

const fn metric(key: &'static str, label: &'static str, source: &'static str) -> Metric {
    Metric { key, label, source }
}

const AUDIO: &[Metric] = &[
    metric("voices", "Voices playing", "regia_audio::Audio::real_voice_count"),
    metric(
        "virtual_voices",
        "Virtual voices",
        "regia_audio::Audio::virtual_voice_count",
    ),
    metric(
        "streaming",
        "Streaming voices",
        "regia_audio::Audio::stream_stats().streaming_voices",
    ),
    metric(
        "starved",
        "Starved streams",
        "regia_audio::Audio::stream_stats().starved_voices",
    ),
    metric(
        "underruns",
        "Stream underruns",
        "regia_audio::Audio::stream_stats().underruns",
    ),
];

const PHYSICS: &[Metric] = &[
    metric(
        "step_ms",
        "Step time (ms)",
        "regia_physics::PhysicsWorld::stats().last_step_ms",
    ),
    metric("bodies", "Bodies", "regia_physics::PhysicsWorld::bodies"),
    metric(
        "contacts",
        "Contact pairs",
        "regia_physics::PhysicsWorld::stats().contact_pairs",
    ),
    metric(
        "islands",
        "Islands",
        "distinct IslandManager::persistent_island_of over the dynamic bodies",
    ),
    metric(
        "active_bodies",
        "Awake bodies",
        "regia_physics::PhysicsWorld::islands.num_active_bodies()",
    ),
];

const UI: &[Metric] = &[
    metric("nodes", "Nodes laid out", "regia_ui::UiLayoutStats::nodes_walked"),
    metric(
        "layout_ms",
        "Layout time (ms)",
        "the frame profiler's ui_layout span",
    ),
    metric(
        "draw_batches",
        "Draw batches",
        "regia_ui::SharedUiRenderer::batch_count",
    ),
];

const RENDERING: &[Metric] = &[
    metric("gpu_ms", "GPU frame (ms)", "regia_render::GpuTimings::frame_ms"),
    metric(
        "passes",
        "GPU passes",
        "regia_render::GpuTimings::passes (timed, > 0 ms)",
    ),
    metric(
        "draws",
        "Draw calls",
        "regia_render::FrameDrawCounts::total().draws",
    ),
    metric(
        "pipeline_switches",
        "Pipeline switches",
        "regia_render::FrameDrawCounts::total().pipeline_switches",
    ),
    metric("meshes_drawn", "Meshes drawn", "regia_render::DrawStats::drawn"),
    metric(
        "meshes_culled",
        "Meshes culled",
        "regia_render::DrawStats::culled",
    ),
    metric(
        "graph_pipelines",
        "Material pipelines ready",
        "regia_render::GraphPipelineStats::ready",
    ),
    metric(
        "pipelines_created",
        "Pipelines created (total)",
        "regia_render::frame_path_counters_all_threads().pipelines",
    ),
];

impl Module {
    /// Every module, in tab order.
    pub const ALL: [Self; 4] = [Self::Audio, Self::Physics, Self::Ui, Self::Rendering];

    /// The tab label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Audio => "Audio",
            Self::Physics => "Physics",
            Self::Ui => "UI",
            Self::Rendering => "Rendering",
        }
    }

    /// The key agents name it by (`audio`, `physics`, `ui`, `rendering`).
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Physics => "physics",
            Self::Ui => "ui",
            Self::Rendering => "rendering",
        }
    }

    /// The module for `key`.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.key() == key)
    }

    /// Its metrics, the charted one by default first.
    #[must_use]
    pub fn metrics(self) -> &'static [Metric] {
        match self {
            Self::Audio => AUDIO,
            Self::Physics => PHYSICS,
            Self::Ui => UI,
            Self::Rendering => RENDERING,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Audio => 0,
            Self::Physics => 1,
            Self::Ui => 2,
            Self::Rendering => 3,
        }
    }
}

/// One sample of a module: a value per metric (`None` when the subsystem is not in the world).
pub type ModuleSample = Vec<Option<f64>>;

/// Samples `module`'s metrics from `world` (in [`Module::metrics`] order). Reads only counters the
/// subsystems keep; a missing subsystem reads `None`.
#[must_use]
pub fn sample(world: &World, module: Module) -> ModuleSample {
    let n = |v: usize| Some(v as f64);
    match module {
        Module::Audio => match world.get_resource::<regia_audio::Audio>() {
            Some(audio) => {
                let streams = audio.stream_stats();
                vec![
                    n(audio.real_voice_count()),
                    n(audio.virtual_voice_count()),
                    n(streams.streaming_voices),
                    n(streams.starved_voices),
                    Some(streams.underruns as f64),
                ]
            }
            None => vec![None; AUDIO.len()],
        },
        Module::Physics => match world.get_resource::<regia::physics::PhysicsWorld>() {
            Some(physics) => {
                let stats = physics.stats();
                let islands: std::collections::HashSet<u32> = physics
                    .bodies
                    .iter()
                    .filter(|(_, body)| body.is_dynamic())
                    .filter_map(|(handle, _)| physics.islands.persistent_island_of(&physics.bodies, handle))
                    .collect();
                vec![
                    Some(stats.last_step_ms),
                    n(stats.bodies),
                    n(stats.contact_pairs),
                    n(islands.len()),
                    n(physics.islands.num_active_bodies()),
                ]
            }
            None => vec![None; PHYSICS.len()],
        },
        Module::Ui => {
            let nodes = world
                .get_resource::<regia_ui::UiLayoutStats>()
                .map(|s| s.nodes_walked as f64);
            let layout_ms = world.get_resource::<regia::Profiler>().and_then(|p| {
                p.0.last_frame()
                    .into_iter()
                    .find(|(name, _)| *name == "ui_layout")
                    .map(|(_, d)| d.as_secs_f64() * 1000.0)
            });
            let batches = world
                .get_resource::<regia_ui::SharedUiRenderer>()
                .map(|r| r.lock().batch_count() as f64);
            vec![nodes, layout_ms.or(nodes.map(|_| 0.0)), batches]
        }
        Module::Rendering => match world.get_resource::<regia_render::RuntimeRenderer>() {
            Some(renderer) => {
                let timings = renderer.0.gpu_timings();
                let passes = timings.passes().iter().filter(|(_, ms)| *ms > 0.0).count();
                let draws = renderer.0.draw_counts().total();
                let stats = renderer.0.draw_stats();
                vec![
                    Some(f64::from(timings.frame_ms)),
                    n(passes),
                    Some(draws.draws as f64),
                    Some(draws.pipeline_switches as f64),
                    n(stats.drawn),
                    n(stats.culled),
                    n(renderer.0.material_graph_stats().ready),
                    Some(regia_render::frame_path_counters_all_threads().pipelines as f64),
                ]
            }
            None => vec![None; RENDERING.len()],
        },
    }
}

/// The per-metric summary of a module's history (in [`Module::metrics`] order).
#[must_use]
pub fn metric_summaries(history: &VecDeque<ModuleSample>, metrics: usize) -> Vec<Option<Summary>> {
    (0..metrics)
        .map(|i| summarize(history.iter().filter_map(|s| s.get(i).copied().flatten())))
        .collect()
}

/// The points of a line chart of `values` (oldest first, `None` gaps skipped) in a `width` ×
/// `height` box, newest at the right edge: x by sample slot of [`MODULE_HISTORY`], y from the
/// bottom by value over the largest value.
#[must_use]
pub fn chart_points(values: &[Option<f64>], width: f32, height: f32) -> Vec<[f32; 2]> {
    let max = values
        .iter()
        .flatten()
        .copied()
        .filter(|v| v.is_finite())
        .fold(0.0_f64, f64::max);
    let slots = values.len().max(MODULE_HISTORY).max(2);
    let step = width / (slots - 1) as f32;
    let offset = slots - values.len();
    values
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let v = v.filter(|v| v.is_finite())?;
            let y = if max > 0.0 { (v / max) as f32 * height } else { 0.0 };
            Some([(offset + i) as f32 * step, height - y])
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Panel state

/// A tab this module adds to the Profiler window (beside `ProfilerTab`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtraTab {
    /// A subsystem module.
    Module(Module),
    /// Counter monitors.
    Monitors,
    /// Per-asset load times.
    Loads,
}

impl ExtraTab {
    /// Every extra tab, in the window's order.
    pub const ALL: [Self; 6] = [
        Self::Module(Module::Audio),
        Self::Module(Module::Physics),
        Self::Module(Module::Ui),
        Self::Module(Module::Rendering),
        Self::Monitors,
        Self::Loads,
    ];

    /// The tab's label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Module(m) => m.label(),
            Self::Monitors => "Monitors",
            Self::Loads => "Loads",
        }
    }
}

/// A Loads tab column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoadColumn {
    /// The asset path (or id).
    Asset,
    /// The load kind.
    Kind,
    /// Cooked bytes read.
    Size,
    /// Read time.
    Read,
    /// Decode time.
    Decode,
    /// Upload (prepare) time.
    Upload,
    /// Read + decode + upload.
    #[default]
    Total,
    /// Live strong handles.
    Refs,
}

impl LoadColumn {
    /// Every column, in table order.
    pub const ALL: [Self; 8] = [
        Self::Asset,
        Self::Kind,
        Self::Size,
        Self::Read,
        Self::Decode,
        Self::Upload,
        Self::Total,
        Self::Refs,
    ];

    /// The column's key (`editor.profiler_loads { sort }`).
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Asset => "asset",
            Self::Kind => "kind",
            Self::Size => "size",
            Self::Read => "read",
            Self::Decode => "decode",
            Self::Upload => "upload",
            Self::Total => "total",
            Self::Refs => "refs",
        }
    }

    /// The column header.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Asset => "Asset",
            Self::Kind => "Kind",
            Self::Size => "Size",
            Self::Read => "Read ms",
            Self::Decode => "Decode ms",
            Self::Upload => "Upload ms",
            Self::Total => "Total ms",
            Self::Refs => "Refs",
        }
    }

    /// The column for `key`.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key() == key)
    }
}

/// The modules, monitors, loads and marks view state (a resource beside `ProfilerView`).
#[derive(Resource, Debug, Default)]
pub struct ProfilerModulesView {
    /// The extra tab shown instead of the panel's own tab (`None`: the panel's tab).
    pub tab: Option<ExtraTab>,
    /// Ms (false) or percent of frame (true) in the flame and timeline views (P-304).
    pub percent: bool,
    /// The charted metric of each module (index into [`Module::metrics`]).
    pub chart_metric: [usize; 4],
    /// Each module's samples, oldest first (at most [`MODULE_HISTORY`]).
    pub history: [VecDeque<ModuleSample>; 4],
    /// The source the module history was sampled from (`edit` or `pie`); a change clears it.
    pub history_source: &'static str,
    /// The egui frame [`collect`] last sampled at (a second window pass in one frame reuses it).
    pub sampled_at: Option<u64>,
    /// Samples taken (tests count them).
    pub samples_taken: u64,
    /// The counters drawn on the Monitors chart.
    pub monitors: Vec<String>,
    /// The Monitors tab's name filter.
    pub monitor_filter: String,
    /// The Loads tab's sort column and direction.
    pub load_sort: (LoadColumn, bool),
    /// The history's frame numbers while it was last live (marks map onto a frozen history by them).
    pub history_frames: Vec<u64>,
    /// The level-load messages read so far.
    level_cursor: MessageCursor<regia_scene::level::LevelLoaded>,
    /// The focusable mark list under the History graph.
    pub mark_keys: crate::canvas_keys::ListKeys,
    /// The focusable counter list of the Monitors tab.
    pub monitor_keys: crate::canvas_keys::ListKeys,
    /// The egui frame a toggle (percent, a monitor, a sort header) last acted in: egui may run the
    /// window twice in one frame, and a toggle applied twice would undo itself.
    acted_at: Option<u64>,
}

impl ProfilerModulesView {
    /// Whether a toggle may act in egui frame `frame` (once per frame); records it.
    fn act_once(&mut self, frame: u64) -> bool {
        if self.acted_at == Some(frame) {
            return false;
        }
        self.acted_at = Some(frame);
        true
    }
}

/// Samples the modules once per egui frame `frame` (a second window pass reuses the sample):
/// from the edit world for the edit source; the play source's samples come from
/// [`note_play_world`]. Also attaches the frame profiler to the counter listing, notes the
/// built-in engine counters and new level loads, and keeps the live history's frame numbers.
pub fn collect(world: &mut World, frame: u64) {
    let _span = tracing::info_span!("profiler_modules_collect").entered();
    if world
        .get_resource::<ProfilerModulesView>()
        .is_some_and(|v| v.sampled_at == Some(frame))
    {
        return;
    }
    let profiler = world.get_resource::<regia::Profiler>().map(|p| p.0.clone());
    if let Some(profiler) = &profiler {
        counters::attach_frame_profiler(profiler);
    }
    note_builtin_counters();
    note_level_loads(world);
    let source = world
        .get_resource::<crate::profiler_source::ProfilerView>()
        .map_or("edit", |v| v.source().kind());
    let frozen = world
        .get_resource::<crate::profiler_source::ProfilerView>()
        .is_some_and(|v| v.frozen.is_some());
    let samples = (source == "edit").then(|| Module::ALL.map(|m| sample(world, m)));
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    view.sampled_at = Some(frame);
    if !frozen && let Some(profiler) = &profiler {
        view.history_frames = profiler.frame_numbers();
    }
    if let Some(samples) = samples {
        push_samples(&mut view, "edit", samples);
    }
}

/// Keeps one sample of the play world's modules (the editor loop calls it while the play source is
/// shown and the panel is open).
pub fn note_play_world(world: &mut World, play: &World) {
    let samples = Module::ALL.map(|m| sample(play, m));
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    push_samples(&mut view, "pie", samples);
}

fn push_samples(view: &mut ProfilerModulesView, source: &'static str, samples: [ModuleSample; 4]) {
    if view.history_source != source {
        view.history_source = source;
        for h in &mut view.history {
            h.clear();
        }
    }
    for (module, values) in Module::ALL.into_iter().zip(samples) {
        for (metric, value) in module.metrics().iter().zip(&values) {
            if let Some(v) = value {
                counters::note_counter(&format!("{}.{}", module.key(), metric.key), *v);
            }
        }
        let history = &mut view.history[module.index()];
        if history.len() >= MODULE_HISTORY {
            history.pop_front();
        }
        history.push_back(values);
    }
    view.samples_taken += 1;
}

/// Lists the engine's built-in counters and gauges (`regia_core::telemetry`) as `core.<name>`.
pub fn note_builtin_counters() {
    use regia_core::telemetry::{Counter, Gauge};
    const COUNTERS: [Counter; Counter::COUNT] = [
        Counter::SurfaceOutdated,
        Counter::SurfaceLost,
        Counter::SurfaceTimeout,
        Counter::SurfaceOther,
        Counter::TextureEvictions,
        Counter::AssetLoads,
        Counter::GpuErrors,
        Counter::Panics,
        Counter::DeviceLost,
        Counter::AudioSuspends,
        Counter::AudioThrottleSuspends,
    ];
    for c in COUNTERS {
        counters::note_counter(
            &format!("core.{}", c.name()),
            regia_core::telemetry::counter(c) as f64,
        );
    }
    for g in [Gauge::ScatterEmitted, Gauge::ParticlesLive] {
        counters::note_counter(
            &format!("core.{}", g.name()),
            regia_core::telemetry::gauge(g) as f64,
        );
    }
}

/// Notes each new `LevelLoaded` message of `world` as a `level_load` mark.
fn note_level_loads(world: &mut World) {
    let Some(messages) = world.get_resource::<Messages<regia_scene::level::LevelLoaded>>() else {
        return;
    };
    let mut cursor = world
        .get_resource::<ProfilerModulesView>()
        .map(|v| v.level_cursor.clone())
        .unwrap_or_default();
    let loaded: Vec<String> = cursor
        .read(messages)
        .map(|m| format!("level {} loaded ({} entities)", m.level, m.entities))
        .collect();
    for name in &loaded {
        counters::note_mark(name, "level_load");
    }
    world
        .get_resource_or_insert_with(ProfilerModulesView::default)
        .level_cursor = cursor;
}

// ---------------------------------------------------------------------------------------------
// Monitors (P-312)

/// One counter as the Monitors table shows it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct MonitorRow {
    /// The counter.
    pub name: String,
    /// Its samples' values, oldest first.
    pub values: Vec<f64>,
    /// Last, min, max and average (`None` before a finite sample).
    pub summary: Option<Summary>,
    /// Samples ever reported.
    pub total: u64,
}

/// The rows of the counters `names` (unknown names are left out).
#[must_use]
pub fn monitor_rows(names: &[String]) -> Vec<MonitorRow> {
    names
        .iter()
        .filter_map(|name| counters::series(name))
        .map(|series| MonitorRow {
            summary: series.summary(),
            values: series.samples.iter().map(|s| s.value).collect(),
            name: series.name,
            total: series.total,
        })
        .collect()
}

/// The accessible name and text of a monitor's table row.
#[must_use]
pub fn monitor_text(row: &MonitorRow) -> String {
    match row.summary {
        Some(s) => format!(
            "{}: last {}, min {}, max {}, avg {}",
            row.name,
            number(s.last),
            number(s.min),
            number(s.max),
            number(s.avg)
        ),
        None => format!("{}: no samples", row.name),
    }
}

fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

// ---------------------------------------------------------------------------------------------
// Loads (P-314)

/// One Loads tab row.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LoadRow {
    /// The asset id.
    pub id: String,
    /// Its project path, when the manifest names it.
    pub path: Option<String>,
    /// The load kind.
    pub kind: &'static str,
    /// Cooked bytes read.
    pub bytes: u64,
    /// Read time, ms.
    pub read_ms: f64,
    /// Decode time, ms.
    pub decode_ms: f64,
    /// Upload (prepare) time, ms.
    pub upload_ms: f64,
    /// Read + decode + upload, ms.
    pub total_ms: f64,
    /// Live strong handles.
    pub refs: usize,
    /// `pak` or `cache`.
    pub source: &'static str,
    /// Whether the load succeeded.
    pub ok: bool,
}

impl LoadRow {
    /// The asset's name in the table: its path, or its id.
    #[must_use]
    pub fn asset(&self) -> &str {
        self.path.as_deref().unwrap_or(&self.id)
    }
}

/// The asset server's load times as rows (empty without a server), in load order.
#[must_use]
pub fn load_rows(world: &World) -> Vec<LoadRow> {
    let Some(server) = world.get_resource::<regia_asset::AssetServer>() else {
        return Vec::new();
    };
    let ms = |ns: u64| ns as f64 / 1e6;
    server
        .load_times()
        .rows()
        .into_iter()
        .map(|r| LoadRow {
            id: r.id.to_string(),
            path: server.path_for_id(r.id).map(str::to_owned),
            kind: r.kind,
            bytes: r.bytes,
            read_ms: ms(r.read_ns),
            decode_ms: ms(r.decode_ns),
            upload_ms: ms(r.upload_ns),
            total_ms: ms(r.total_ns()),
            refs: server.strong_count(r.id),
            source: r.source,
            ok: r.ok,
        })
        .collect()
}

/// Sorts `rows` by `column`, largest (or last name) first when `descending`; ties keep load order.
pub fn sort_loads(rows: &mut [LoadRow], column: LoadColumn, descending: bool) {
    rows.sort_by(|a, b| {
        let order = match column {
            LoadColumn::Asset => a.asset().cmp(b.asset()),
            LoadColumn::Kind => a.kind.cmp(b.kind),
            LoadColumn::Size => a.bytes.cmp(&b.bytes),
            LoadColumn::Read => a.read_ms.total_cmp(&b.read_ms),
            LoadColumn::Decode => a.decode_ms.total_cmp(&b.decode_ms),
            LoadColumn::Upload => a.upload_ms.total_cmp(&b.upload_ms),
            LoadColumn::Total => a.total_ms.total_cmp(&b.total_ms),
            LoadColumn::Refs => a.refs.cmp(&b.refs),
        };
        if descending { order.reverse() } else { order }
    });
}

/// The accessible name and text of a load row.
#[must_use]
pub fn load_text(row: &LoadRow) -> String {
    format!(
        "{}, {}, {} bytes, read {:.3} ms, decode {:.3} ms, upload {:.3} ms, total {:.3} ms, {} refs{}",
        row.asset(),
        row.kind,
        row.bytes,
        row.read_ms,
        row.decode_ms,
        row.upload_ms,
        row.total_ms,
        row.refs,
        if row.ok { "" } else { ", failed" }
    )
}

// ---------------------------------------------------------------------------------------------
// Marks (P-318)

/// A mark placed on the History graph.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct HistoryMark {
    /// Index into the shown history.
    pub index: usize,
    /// `trace`, `level_load`, `hitch` or a host's kind.
    pub kind: String,
    /// What hover shows.
    pub label: String,
    /// The frame profiler's frame number (`None` for a history without numbers).
    pub frame: Option<u64>,
}

/// The marks of the shown history: each listed mark whose frame is in `frame_numbers` (the
/// history's frame numbers, oldest first) at that frame's index, then a `hitch` mark per hitch
/// index of `history` (`hitches`), in index order.
#[must_use]
pub fn history_marks(
    marks: &[MarkEntry],
    frame_numbers: &[u64],
    history: &[crate::profiler_model::FrameSample],
    hitches: &[usize],
) -> Vec<HistoryMark> {
    let mut out: Vec<HistoryMark> = marks
        .iter()
        .filter_map(|m| {
            let index = frame_numbers.iter().position(|n| *n == m.frame)?;
            (index < history.len()).then(|| HistoryMark {
                index,
                kind: m.kind.clone(),
                label: format!("{} ({}, frame {})", m.name, m.kind.replace('_', " "), m.frame),
                frame: Some(m.frame),
            })
        })
        .collect();
    out.extend(hitches.iter().filter_map(|&i| {
        let sample = history.get(i)?;
        Some(HistoryMark {
            index: i,
            kind: "hitch".to_owned(),
            label: format!("hitch: {:.2} ms", sample.ms),
            frame: frame_numbers.get(i).copied(),
        })
    }));
    out.sort_by_key(|m| m.index);
    out
}

/// The marks of the in-process history `history` shown now (the frozen copy's numbers while
/// frozen): trace and level-load marks from the listing, and its hitches.
#[must_use]
pub fn current_history_marks(
    world: &World,
    history: &[crate::profiler_model::FrameSample],
) -> Vec<HistoryMark> {
    let local = world
        .get_resource::<crate::profiler_source::ProfilerView>()
        .is_none_or(|v| matches!(v.source().kind(), "edit" | "pie"));
    let threshold = crate::profiler_model::hitch_threshold_ms(history);
    let hitches = crate::profiler_model::hitches(history, threshold);
    let numbers: Vec<u64> = if local {
        world
            .get_resource::<ProfilerModulesView>()
            .map(|v| v.history_frames.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    history_marks(&counters::marks_since(0), &numbers, history, &hitches)
}

/// The marks on a thread-timeline strip: each listed mark whose frame is one of `frames`.
#[must_use]
pub fn timeline_marks(
    marks: &[MarkEntry],
    frames: &[regia::telemetry::profile_capture::CaptureStripFrame],
) -> Vec<(u64, String)> {
    marks
        .iter()
        .filter(|m| frames.iter().any(|f| f.number == m.frame))
        .map(|m| (m.frame, format!("{} ({})", m.name, m.kind.replace('_', " "))))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Drawing

fn chart(ui: &mut egui::Ui, name: &str, series: &[(egui::Color32, Vec<Option<f64>>)]) -> egui::Response {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let width = ui.available_width().max(100.0);
    let (area, response) = ui.allocate_exact_size(egui::vec2(width, CHART_HEIGHT), egui::Sense::hover());
    crate::canvas_keys::name_canvas(ui.ctx(), response.id, name);
    let painter = ui.painter_at(area);
    painter.rect_filled(area, 0.0, tokens.well);
    for (color, values) in series {
        let points: Vec<egui::Pos2> = chart_points(values, area.width(), area.height())
            .into_iter()
            .map(|[x, y]| area.min + egui::vec2(x, y))
            .collect();
        painter.add(egui::Shape::line(points, egui::Stroke::new(1.5, *color)));
    }
    response
}

fn summary_cells(ui: &mut egui::Ui, s: Option<Summary>) {
    match s {
        Some(s) => {
            for v in [s.last, s.min, s.max, s.avg] {
                ui.monospace(number(v));
            }
        }
        None => {
            for _ in 0..4 {
                ui.weak("n/a");
            }
        }
    }
}

/// Draws a module tab: the metric picker, its chart over the history and the details table.
pub fn module_tab(ui: &mut egui::Ui, world: &mut World, module: Module) {
    let source = world
        .get_resource::<crate::profiler_source::ProfilerView>()
        .map_or("edit", |v| v.source().kind());
    if !matches!(source, "edit" | "pie") {
        ui.label("Modules read the in-process sources (Edit world, Play); pick one of them.");
        return;
    }
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    let metrics = module.metrics();
    let mut picked = view.chart_metric[module.index()].min(metrics.len() - 1);
    // UI site: agent twin `editor.profiler_modules { module, metric }`.
    // One focusable button per metric (Tab, then Space or Enter charts it).
    ui.horizontal_wrapped(|ui| {
        ui.label("Chart");
        for (i, m) in metrics.iter().enumerate() {
            ui.selectable_value(&mut picked, i, m.label);
        }
    });
    view.chart_metric[module.index()] = picked;
    let history = &view.history[module.index()];
    let values: Vec<Option<f64>> = history.iter().map(|s| s.get(picked).copied().flatten()).collect();
    if values.iter().all(Option::is_none) {
        ui.weak(format!(
            "No {} counters in this world yet ({}).",
            module.label(),
            metrics[picked].source
        ));
    }
    chart(
        ui,
        &format!("{} chart: {}", module.label(), metrics[picked].label),
        &[(tokens.signal, values)],
    );
    let summaries = metric_summaries(history, metrics.len());
    egui::Grid::new(("profiler_module", module.key()))
        .striped(true)
        .show(ui, |ui| {
            for h in ["metric", "last", "min", "max", "avg"] {
                ui.strong(h);
            }
            ui.end_row();
            for (m, s) in metrics.iter().zip(summaries) {
                let _ = ui.label(m.label).on_hover_text(m.source);
                summary_cells(ui, s);
                ui.end_row();
            }
        });
}

/// Draws the Monitors tab: the filtered counter list (Space or a click toggles a counter onto the
/// chart), the chart of the shown counters and their table.
pub fn monitors_tab(ui: &mut egui::Ui, world: &mut World) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let listing = counters::listing();
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    ui.horizontal(|ui| {
        let label = ui.label("Filter counters");
        let _ = ui
            .add(egui::TextEdit::singleline(&mut view.monitor_filter).desired_width(160.0))
            .labelled_by(label.id);
        ui.weak(format!("{} counters", listing.names.len()));
    });
    let filter = view.monitor_filter.to_lowercase();
    let names: Vec<String> = listing
        .names
        .into_iter()
        .filter(|n| filter.is_empty() || n.to_lowercase().contains(&filter))
        .collect();
    let shown = view.monitors.clone();
    let name = |i: usize| {
        names
            .get(i)
            .map(|n| {
                let on = if shown.contains(n) { "shown" } else { "not shown" };
                format!("{n}, {on}")
            })
            .unwrap_or_default()
    };
    let current = view.monitor_keys.current;
    // UI site: agent twin `editor.profiler_monitors { add, remove }`.
    let keys = crate::canvas_keys::list_rows(
        ui,
        "profiler-monitor-counters",
        &mut view.monitor_keys,
        names.len(),
        crate::canvas_keys::LIST_HEIGHT,
        current,
        &name,
    );
    let frame = ui.ctx().cumulative_frame_nr();
    for key in keys {
        if let crate::canvas_keys::RowKey::Activate(i) = key
            && let Some(n) = names.get(i)
            && view.act_once(frame)
        {
            toggle_monitor(&mut view, n);
        }
    }
    let rows = monitor_rows(&view.monitors);
    ui.separator();
    let series: Vec<(egui::Color32, Vec<Option<f64>>)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            (
                tokens.data_kinds[i % tokens.data_kinds.len()],
                r.values.iter().copied().map(Some).collect(),
            )
        })
        .collect();
    chart(ui, &format!("Monitors chart, {} counters", rows.len()), &series);
    egui::Grid::new("profiler_monitors").striped(true).show(ui, |ui| {
        for h in ["counter", "last", "min", "max", "avg"] {
            ui.strong(h);
        }
        ui.end_row();
        for (i, row) in rows.iter().enumerate() {
            let color = tokens.data_kinds[i % tokens.data_kinds.len()];
            let _ = ui
                .colored_label(color, &row.name)
                .on_hover_text(monitor_text(row));
            summary_cells(ui, row.summary);
            ui.end_row();
        }
    });
}

/// Adds `name` to the chart, or removes it when shown (at most [`MAX_MONITORS`] shown; the oldest
/// leaves for a new one).
pub fn toggle_monitor(view: &mut ProfilerModulesView, name: &str) {
    if let Some(i) = view.monitors.iter().position(|n| n == name) {
        view.monitors.remove(i);
    } else {
        if view.monitors.len() >= MAX_MONITORS {
            view.monitors.remove(0);
        }
        view.monitors.push(name.to_owned());
    }
}

fn bytes_text(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.2} MiB", n as f64 / f64::from(1u32 << 20))
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// Draws the Loads tab: sortable header buttons over the asset server's load-time rows.
pub fn loads_tab(ui: &mut egui::Ui, world: &mut World) {
    let mut rows = load_rows(world);
    let Some(server) = world.get_resource::<regia_asset::AssetServer>() else {
        ui.label("No asset server in this world.");
        return;
    };
    ui.weak(format!(
        "{} loads recorded · {} kept · {} dropped (editor process)",
        server.load_times().recorded(),
        rows.len(),
        server.load_times().dropped()
    ));
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    let (column, descending) = view.load_sort;
    sort_loads(&mut rows, column, descending);
    egui::ScrollArea::vertical()
        .id_salt("profiler_loads")
        .max_height(320.0)
        .show(ui, |ui| {
            egui::Grid::new("profiler_loads_rows")
                .striped(true)
                .show(ui, |ui| {
                    for c in LoadColumn::ALL {
                        let arrow = match (c == column, descending) {
                            (true, true) => " ▼",
                            (true, false) => " ▲",
                            (false, _) => "",
                        };
                        // View-only sort; agent twin `editor.profiler_loads { sort, descending }`.
                        if ui
                            .button(format!("{}{arrow}", c.label()))
                            .on_hover_text(format!("Sort by {}", c.label()))
                            .clicked()
                            && view.act_once(ui.ctx().cumulative_frame_nr())
                        {
                            view.load_sort = if c == column { (c, !descending) } else { (c, true) };
                        }
                    }
                    ui.end_row();
                    for row in &rows {
                        let _ = ui.monospace(row.asset()).on_hover_text(load_text(row));
                        ui.monospace(row.kind);
                        ui.monospace(bytes_text(row.bytes));
                        for v in [row.read_ms, row.decode_ms, row.upload_ms, row.total_ms] {
                            ui.monospace(format!("{v:.3}"));
                        }
                        ui.monospace(row.refs.to_string());
                        ui.end_row();
                    }
                });
        });
}

/// The color a History mark is drawn with (theme accents).
#[must_use]
pub fn mark_color(tokens: &crate::theme::ThemeTokens, kind: &str) -> egui::Color32 {
    match kind {
        "hitch" => tokens.accents.alert,
        "level_load" => tokens.accents.ok,
        _ => tokens.accents.select,
    }
}

/// Draws `marks` over the History graph in `area` (`step` points per bar) and returns the label of
/// the marks under `hover` (within half a bar or three points).
pub fn paint_history_marks(
    painter: &egui::Painter,
    tokens: &crate::theme::ThemeTokens,
    area: egui::Rect,
    step: f32,
    marks: &[HistoryMark],
    hover: Option<egui::Pos2>,
) -> Option<String> {
    let mut hovered = Vec::new();
    for m in marks {
        let x = area.min.x + (m.index as f32 + 0.5) * step;
        let color = mark_color(tokens, &m.kind);
        painter.vline(x, area.y_range(), egui::Stroke::new(1.0, color));
        let tip = egui::pos2(x, area.min.y);
        painter.add(egui::Shape::convex_polygon(
            vec![
                tip + egui::vec2(-4.0, 0.0),
                tip + egui::vec2(4.0, 0.0),
                tip + egui::vec2(0.0, 6.0),
            ],
            color,
            egui::Stroke::NONE,
        ));
        if hover.is_some_and(|p| area.contains(p) && (p.x - x).abs() <= (step / 2.0).max(3.0)) {
            hovered.push(m.label.clone());
        }
    }
    (!hovered.is_empty()).then(|| hovered.join("\n"))
}

/// The focusable list of the History graph's marks (the keyboard path to what hovering shows).
pub fn mark_list(ui: &mut egui::Ui, world: &mut World, marks: &[HistoryMark]) {
    if marks.is_empty() {
        return;
    }
    ui.strong(format!("Marks ({})", marks.len()));
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    let name = |i: usize| {
        marks
            .get(i)
            .map(|m| format!("frame {}: {}", m.index, m.label))
            .unwrap_or_default()
    };
    let current = view.mark_keys.current;
    let _ = crate::canvas_keys::list_rows(
        ui,
        "profiler-history-marks",
        &mut view.mark_keys,
        marks.len(),
        crate::canvas_keys::LIST_HEIGHT.min(120.0),
        current,
        &name,
    );
}

// ---------------------------------------------------------------------------------------------
// Agent methods

fn refused(code: &'static str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, "see book/agents/profiler.md")
}

fn summary_json(s: Option<Summary>) -> Value {
    match s {
        Some(s) => json!({ "last": s.last, "min": s.min, "max": s.max, "avg": s.avg, "count": s.count }),
        None => Value::Null,
    }
}

/// `editor.profiler_units { percent? }`: sets (or reads) the ms / percent-of-frame toggle and
/// answers the current frame's top-level shares (they sum to 100).
fn profiler_units(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let percent = match params.get("percent") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| refused("profiler_bad_percent", "`percent` must be true or false"))?,
        ),
    };
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    if let Some(p) = percent {
        view.percent = p;
    }
    let percent = view.percent;
    let data = crate::profiler_source::read_source(world, u64::MAX, false, 0);
    let shares = data.capture.as_ref().map(frame_shares).unwrap_or_default();
    let total: f64 = shares.iter().map(|s| s.percent).sum();
    Ok(json!({
        "percent": percent,
        "frame_ms": data.capture.as_ref().map(percent_base_ms),
        "shares": shares,
        "total_percent": total,
    }))
}

/// `editor.profiler_modules { module, metric? }`.
fn profiler_modules(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let key = params.get("module").and_then(Value::as_str).unwrap_or("");
    let module = Module::from_key(key).ok_or_else(|| {
        refused(
            "profiler_bad_module",
            format!("`module` must be audio, physics, ui or rendering, not `{key}`"),
        )
    })?;
    let metrics = module.metrics();
    let picked = match params.get("metric") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let k = v.as_str().unwrap_or("");
            Some(metrics.iter().position(|m| m.key == k).ok_or_else(|| {
                let keys: Vec<&str> = metrics.iter().map(|m| m.key).collect();
                refused(
                    "profiler_bad_metric",
                    format!("`metric` must be one of {} for {}", keys.join(", "), module.key()),
                )
            })?)
        }
    };
    // An agent read samples once more, so a call right after a change sees it.
    let frame = world
        .get_resource::<ProfilerModulesView>()
        .and_then(|v| v.sampled_at)
        .map_or(0, |f| f.wrapping_add(1));
    collect(world, frame);
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    view.tab = Some(ExtraTab::Module(module));
    if let Some(i) = picked {
        view.chart_metric[module.index()] = i;
    }
    let charted = view.chart_metric[module.index()].min(metrics.len() - 1);
    let history = &view.history[module.index()];
    let summaries = metric_summaries(history, metrics.len());
    let rows: Vec<Value> = metrics
        .iter()
        .zip(summaries)
        .map(|(m, s)| json!({ "key": m.key, "label": m.label, "source": m.source, "summary": summary_json(s) }))
        .collect();
    let values: Vec<Option<f64>> = history
        .iter()
        .map(|s| s.get(charted).copied().flatten())
        .collect();
    Ok(json!({
        "module": module.key(),
        "source": view.history_source,
        "samples": history.len(),
        "metrics": rows,
        "chart": {
            "metric": metrics[charted].key,
            "values": values,
            "points": chart_points(&values, 400.0, CHART_HEIGHT).len(),
        },
    }))
}

/// `editor.profiler_monitors { add?, remove?, filter? }`.
fn profiler_monitors(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    note_builtin_counters();
    let listing = counters::listing();
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    view.tab = Some(ExtraTab::Monitors);
    for (key, add) in [("add", true), ("remove", false)] {
        let Some(v) = params.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        let name = v
            .as_str()
            .ok_or_else(|| refused("profiler_bad_counter", format!("`{key}` must be a counter name")))?;
        if add && !listing.names.iter().any(|n| n == name) {
            return Err(refused(
                "profiler_unknown_counter",
                format!("no counter `{name}` has reported (see `counters` of an answer)"),
            ));
        }
        let shown = view.monitors.iter().any(|n| n == name);
        if shown != add {
            toggle_monitor(&mut view, name);
        }
    }
    if let Some(f) = params.get("filter").and_then(Value::as_str) {
        f.clone_into(&mut view.monitor_filter);
    }
    let filter = view.monitor_filter.to_lowercase();
    let names: Vec<&String> = listing
        .names
        .iter()
        .filter(|n| filter.is_empty() || n.to_lowercase().contains(&filter))
        .collect();
    let shown: Vec<Value> = monitor_rows(&view.monitors)
        .into_iter()
        .map(|r| json!({ "name": r.name, "summary": summary_json(r.summary), "samples": r.values.len(), "total": r.total }))
        .collect();
    Ok(json!({
        "counters": names,
        "dropped_names": listing.dropped_names,
        "shown": shown,
    }))
}

/// `editor.profiler_loads { sort?, descending?, limit? }`.
fn profiler_loads(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let current = world
        .get_resource::<ProfilerModulesView>()
        .map(|v| v.load_sort)
        .unwrap_or_default();
    let column = match params.get("sort").and_then(Value::as_str) {
        None => current.0,
        Some(k) => LoadColumn::from_key(k).ok_or_else(|| {
            let keys: Vec<&str> = LoadColumn::ALL.iter().map(|c| c.key()).collect();
            refused(
                "profiler_bad_sort",
                format!("`sort` must be one of {}", keys.join(", ")),
            )
        })?,
    };
    let descending = params
        .get("descending")
        .and_then(Value::as_bool)
        .unwrap_or(params.get("sort").is_some() || current.1);
    let limit = match params.get("limit") {
        None | Some(Value::Null) => 100,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=10_000).contains(n))
            .ok_or_else(|| refused("profiler_bad_limit", "`limit` must be an integer from 1 to 10000"))?
            as usize,
    };
    let mut view = world.get_resource_or_insert_with(ProfilerModulesView::default);
    view.tab = Some(ExtraTab::Loads);
    view.load_sort = (column, descending);
    let mut rows = load_rows(world);
    sort_loads(&mut rows, column, descending);
    let (recorded, dropped) = world
        .get_resource::<regia_asset::AssetServer>()
        .map_or((0, 0), |s| (s.load_times().recorded(), s.load_times().dropped()));
    let total = rows.len();
    rows.truncate(limit);
    Ok(json!({
        "sort": column.key(),
        "descending": descending,
        "recorded": recorded,
        "dropped": dropped,
        "rows_total": total,
        "rows": rows,
    }))
}

/// `editor.profiler_marks {}`.
fn profiler_marks(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    collect(
        world,
        world
            .get_resource::<ProfilerModulesView>()
            .and_then(|v| v.sampled_at)
            .map_or(0, |f| f.wrapping_add(1)),
    );
    let data = crate::profiler_source::read_source(world, u64::MAX, false, 0);
    let marks = current_history_marks(world, &data.history);
    Ok(json!({
        "history": { "frames": data.history.len() },
        "marks": marks,
    }))
}

/// The modules, monitors, loads and marks agent methods (ADR-8141).
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.profiler_units",
                tool: "profiler_units",
                description: "Set or read how the editor Profiler panel's flame and timeline views show time: ms (percent false) or percent of the frame (percent true). Answers the source's top-level spans with their ms and share of the frame plus the time outside every span; the shares sum to 100.",
                params: &[ParamSchema {
                    name: "percent",
                    kind: "boolean",
                    required: false,
                    description: "true: percent of frame; false: ms; absent keeps the toggle.",
                }],
                result: "{ percent, frame_ms, shares: [{ name, ms, percent }], total_percent }",
                mutating: true,
                permission: Permission::EditScene,
            },
            profiler_units as Handler,
        ),
        (
            MethodSchema {
                name: "editor.profiler_modules",
                tool: "profiler_modules",
                description: "Show and read one subsystem module tab of the editor Profiler panel: audio (voices, streaming), physics (step time, bodies, contacts, islands, awake bodies), ui (nodes, layout time, draw batches) or rendering (GPU time, passes, draws, pipelines). Each metric comes from a counter the subsystem keeps (its source is listed) and is sampled once per repaint from the in-process source's world; the answer has every metric's last, min, max and average over the history and the charted metric's values.",
                params: &[
                    ParamSchema {
                        name: "module",
                        kind: "string",
                        required: true,
                        description: "audio, physics, ui or rendering.",
                    },
                    ParamSchema {
                        name: "metric",
                        kind: "string",
                        required: false,
                        description: "The metric key to chart (from metrics[].key); absent keeps the tab's.",
                    },
                ],
                result: "{ module, source, samples, metrics: [{ key, label, source, summary: { last, min, max, avg, count } | null }], chart: { metric, values, points } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            profiler_modules as Handler,
        ),
        (
            MethodSchema {
                name: "editor.profiler_monitors",
                tool: "profiler_monitors",
                description: "Show and read the editor Profiler panel's Monitors tab: every named counter in the telemetry counter listing (the engine's built-in counters as core.*, the module metrics as <module>.<metric>, and a game's own counters from regia::telemetry::counters::counter) and the counters drawn on its chart with last, min, max and average. add puts a counter on the chart, remove takes it off (at most 8 shown).",
                params: &[
                    ParamSchema {
                        name: "add",
                        kind: "string",
                        required: false,
                        description: "A counter name to chart.",
                    },
                    ParamSchema {
                        name: "remove",
                        kind: "string",
                        required: false,
                        description: "A counter name to take off the chart.",
                    },
                    ParamSchema {
                        name: "filter",
                        kind: "string",
                        required: false,
                        description: "Case-insensitive name filter of the counter list (empty: all).",
                    },
                ],
                result: "{ counters: [name], dropped_names, shown: [{ name, summary: { last, min, max, avg, count } | null, samples, total }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            profiler_monitors as Handler,
        ),
        (
            MethodSchema {
                name: "editor.profiler_loads",
                tool: "profiler_loads",
                description: "Show and read the editor Profiler panel's Loads tab: the editor asset server's per-asset load times (read, decode and upload ms), cooked bytes, source, live reference count and outcome, one row per asset (its newest load), sorted by a column.",
                params: &[
                    ParamSchema {
                        name: "sort",
                        kind: "string",
                        required: false,
                        description: "asset, kind, size, read, decode, upload, total or refs (default the tab's, initially total).",
                    },
                    ParamSchema {
                        name: "descending",
                        kind: "boolean",
                        required: false,
                        description: "Largest first (default true with sort, else the tab's).",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "integer",
                        required: false,
                        description: "Rows listed, 1-10000 (default 100); rows_total counts them all.",
                    },
                ],
                result: "{ sort, descending, recorded, dropped, rows_total, rows: [{ id, path, kind, bytes, read_ms, decode_ms, upload_ms, total_ms, refs, source, ok }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            profiler_loads as Handler,
        ),
        (
            MethodSchema {
                name: "editor.profiler_marks",
                tool: "profiler_marks",
                description: "Read the marks the editor Profiler panel draws on its History graph (and, for the frames in a strip, on the thread timeline): trace marks, level loads and hitches, each at its history index with the label hovering shows. Marks are mapped for the in-process sources (edit world, play); a process source lists its hitches only.",
                params: &[],
                result: "{ history: { frames }, marks: [{ index, kind, label, frame }] }",
                mutating: false,
                permission: Permission::Read,
            },
            profiler_marks as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
