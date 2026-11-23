// SPDX-License-Identifier: MPL-2.0
//! The in-game performance overlay (book `user-guide/performance-overlay.md`, agent guide
//! topic `perf_overlay`): a compact HUD in the top-left corner that alpha testers turn on from
//! the game's settings menu to see and report performance themselves.
//!
//! **Compact** shows FPS and frame time, a frame-time graph of the last ~5 s (lines at 16.7 and
//! 33.3 ms, spikes over 2 × the median in red), renderer CPU against GPU frame time, the most
//! expensive GPU passes, material texture memory against its budget, the stutter counter
//! (frames over 2 × median, plus pipelines and bind groups created after warm-up), the quality
//! preset, the lighting (GI) mode and the output/internal resolution with the upscaler.
//! **Detailed** adds the frame-time percentiles, GPU memory, the overlay's own cost and the full
//! per-pass GPU table.
//!
//! Every number comes from an existing engine source, never a parallel measurement: the frame
//! interval is [`RealTime`]'s delta (what `play.hitch_report` records), CPU and GPU times are the
//! runtime renderer's [`FrameRenderer::cpu_timings`](regia_render::FrameRenderer::cpu_timings)
//! and [`GpuTimings`], the stutter maths is [`HitchReport`] over the window, device calls are the
//! frame-path counters, texture memory is
//! [`FrameRenderer::texture_memory_summary`](regia_render::FrameRenderer::texture_memory_summary)
//! and GPU memory [`regia_render::memory_report`].
//!
//! **Turning it on.** The mode is a setting, [`GameSettings::performance_overlay`]: `None`
//! follows the host's default ([`PerformanceOverlay::default_mode`], engine default
//! [`PerformanceOverlayMode::Off`]); a player's saved choice always wins. A settings menu adds
//! the option with [`spawn_settings_entry`] (a ready-made labelled dropdown) or builds its own
//! control from [`settings_menu_entry`]. There is no default key: a game that wants one binds the
//! [`PERF_OVERLAY_ACTION`] action in its [`regia_input::InputConfig`] (each press cycles Off →
//! Compact → Detailed). Agents use `perf_overlay.toggle` / `perf_overlay.get`.
//!
//! **Cost.** Hidden, the plugin does a few comparisons per frame. Visible, the graph redraws at
//! 20 Hz and the text at 4 Hz through the runtime UI; the UI pipeline is compiled on the first
//! frame (`UiRenderer::draw` warms its target format), so showing the overlay creates no
//! pipelines. Detailed mode's GPU memory report (every 2 s) runs on a short-lived worker thread
//! since v0.1.14, never on the frame thread; the overlay shows it from the next refresh after it
//! finishes.
//!
//! ```
//! use regia_app::prelude::*;
//! use regia_framework::perf_overlay::{PerformanceOverlay, PerformanceOverlayMode};
//!
//! let mut app = App::new();
//! // DO27's alpha: on (Compact) unless the player turned it off in the settings menu.
//! app.insert_resource(PerformanceOverlay::default_mode(PerformanceOverlayMode::Compact));
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use regia_app::prelude::*;
use regia_app::{Diagnostics, RealTime};
use regia_render::{
    FramePathCounters, GameFrameWarm, GpuTimings, HitchFrame, HitchReport, HitchTargets, RenderDevice,
    RenderSettings, RuntimeRenderer, WindowSurfaces,
};
use regia_ui::node::{
    Background, BackgroundKind, Border, CanvasRect, Interactable, NodeStyle, UiCanvas, UiNode, UiRoot,
};
use regia_ui::style::{Align, Dimension, Display, Edges, FlexDirection, Style};
use regia_ui::text::{Text, TextAlign, TextSpan, TextStyle, WrapMode};
use regia_ui::{Dropdown, Focusable, ScaleMode, UiColor};
use serde::{Deserialize, Serialize};

use crate::settings::GameSettings;

/// The input action that cycles the overlay (Off → Compact → Detailed). It has **no default
/// binding**: a game that wants a key declares and binds it in its `InputConfig`.
pub const PERF_OVERLAY_ACTION: &str = "PerformanceOverlay";

/// The [`GameSettings`] field (reflection path for `settings.get` / `settings.set`).
pub const SETTINGS_PATH: &str = "performance_overlay";

/// Seconds of frame history the graph and the window statistics cover.
pub const WINDOW_SECONDS: f32 = 5.0;

/// Graph columns (each the slowest frame of its slice of the window).
const GRAPH_COLUMNS: usize = 120;
/// Graph top, ms.
const GRAPH_TOP_MS: f32 = 50.0;
/// Graph redraw interval, s.
const GRAPH_EVERY_S: f64 = 0.05;
/// Text refresh interval, s.
const TEXT_EVERY_S: f64 = 0.25;
/// GPU memory report interval (Detailed only), s.
const VRAM_EVERY_S: f64 = 2.0;
/// Frames after start (and after a level warm) before device creates count as stutters.
const WARMUP_FRAMES: u32 = 300;
/// Frames after a [`GameFrameWarm`] finishes before counting again.
const REWARM_FRAMES: u32 = 60;
/// History cap (frames) whatever the frame rate.
const RING_CAP: usize = 4096;

/// What the overlay shows.
#[derive(Reflect, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Default)]
pub enum PerformanceOverlayMode {
    /// Hidden (engine default).
    #[default]
    Off,
    /// FPS, graph, CPU/GPU, top passes, memory, stutters, quality and resolution.
    Compact,
    /// Compact plus percentiles, GPU memory and the full per-pass GPU table.
    Detailed,
}

impl PerformanceOverlayMode {
    /// Every mode in menu order.
    pub const ALL: [Self; 3] = [Self::Off, Self::Compact, Self::Detailed];

    /// The stable name (`"Off"`, `"Compact"`, `"Detailed"`): the settings file, the agent
    /// protocol.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Compact => "Compact",
            Self::Detailed => "Detailed",
        }
    }

    /// Parses [`Self::name`] (ASCII case-insensitive).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|m| m.name().eq_ignore_ascii_case(name))
    }

    /// The next mode in the cycle Off → Compact → Detailed → Off.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Compact,
            Self::Compact => Self::Detailed,
            Self::Detailed => Self::Off,
        }
    }

    /// Index in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Off => 0,
            Self::Compact => 1,
            Self::Detailed => 2,
        }
    }
}

/// Where the active mode came from ([`PerformanceOverlay::resolve`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeSource {
    /// A runtime override (`perf_overlay.toggle`, or the action key without [`GameSettings`]).
    Override,
    /// The player's saved choice ([`GameSettings::performance_overlay`]).
    Player,
    /// The host's default ([`PerformanceOverlay::default_mode`]).
    HostDefault,
}

impl ModeSource {
    /// `"override"`, `"player"` or `"host_default"`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Player => "player",
            Self::HostDefault => "host_default",
        }
    }
}

/// The overlay's configuration. Insert it (before or after `DefaultPlugins`) to change the host
/// default; the player's saved choice in [`GameSettings::performance_overlay`] always wins.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct PerformanceOverlay {
    /// The mode when the player has not chosen one (engine default [`PerformanceOverlayMode::Off`]).
    pub default: PerformanceOverlayMode,
    /// A runtime override above the setting (agent `perf_overlay.toggle`); not persisted, and
    /// cleared when the player changes the setting.
    pub override_mode: Option<PerformanceOverlayMode>,
}

impl PerformanceOverlay {
    /// A configuration whose host default is `mode` (DO27's alpha ships `Compact`).
    #[must_use]
    pub fn default_mode(mode: PerformanceOverlayMode) -> Self {
        Self {
            default: mode,
            override_mode: None,
        }
    }

    /// The active mode and where it came from: override, else the player's setting, else the
    /// host default.
    #[must_use]
    pub fn resolve(&self, settings: Option<&GameSettings>) -> (PerformanceOverlayMode, ModeSource) {
        if let Some(mode) = self.override_mode {
            return (mode, ModeSource::Override);
        }
        match settings.and_then(|s| s.performance_overlay) {
            Some(mode) => (mode, ModeSource::Player),
            None => (self.default, ModeSource::HostDefault),
        }
    }
}

/// The active mode of `world` ([`PerformanceOverlay::resolve`]); `Off` without the plugin.
#[must_use]
pub fn active_mode(world: &World) -> (PerformanceOverlayMode, ModeSource) {
    world
        .get_resource::<PerformanceOverlay>()
        .map_or((PerformanceOverlayMode::Off, ModeSource::HostDefault), |o| {
            o.resolve(world.get_resource::<GameSettings>())
        })
}

/// Sets the mode as the player would: writes [`GameSettings::performance_overlay`] (persisted
/// by the settings plugin) and clears any override; without `GameSettings` it becomes the
/// override.
pub fn set_player_mode(world: &mut World, mode: PerformanceOverlayMode) {
    if let Some(mut settings) = world.get_resource_mut::<GameSettings>() {
        if settings.performance_overlay != Some(mode) {
            settings.performance_overlay = Some(mode);
        }
        world.get_resource_or_init::<PerformanceOverlay>().override_mode = None;
    } else {
        world.get_resource_or_init::<PerformanceOverlay>().override_mode = Some(mode);
    }
}

/// Sets (or with `None` clears) the runtime override: shown or hidden now, nothing persisted.
pub fn set_override(world: &mut World, mode: Option<PerformanceOverlayMode>) {
    world.get_resource_or_init::<PerformanceOverlay>().override_mode = mode;
}

/// What a host's settings menu needs to show the option itself ([`settings_menu_entry`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsMenuEntry {
    /// The [`GameSettings`] reflection path ([`SETTINGS_PATH`]).
    pub path: &'static str,
    /// The English label (`"Performance overlay"`); localise with key `settings-performance-overlay`.
    pub label: &'static str,
    /// `(value, English label)` per option, in menu order; the value is
    /// [`PerformanceOverlayMode::name`].
    pub options: [(&'static str, &'static str); 3],
    /// One-line help text.
    pub help: &'static str,
}

/// The settings-menu descriptor for hosts that build their own menu controls. Write the choice
/// with [`set_player_mode`] (or `GameSettings::performance_overlay = Some(mode)`).
#[must_use]
pub const fn settings_menu_entry() -> SettingsMenuEntry {
    SettingsMenuEntry {
        path: SETTINGS_PATH,
        label: "Performance overlay",
        options: [("Off", "Off"), ("Compact", "Compact"), ("Detailed", "Detailed")],
        help: "Shows frame rate, frame time, GPU passes, memory and stutters in the top-left corner.",
    }
}

/// Marks a [`Dropdown`] spawned by [`spawn_settings_entry`]: its selection is the player's
/// overlay mode, kept in sync both ways.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct PerformanceOverlayOption;

/// Spawns a ready-made settings-menu row: the label and a dropdown (Off / Compact / Detailed)
/// that writes the player's choice. Returns the row; with `parent` it is added as its child.
pub fn spawn_settings_entry(world: &mut World, parent: Option<Entity>) -> Entity {
    let entry = settings_menu_entry();
    let (mode, _) = active_mode(world);
    let label = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                flex_grow: 1.0,
                ..Default::default()
            }),
            Text {
                spans: vec![TextSpan {
                    style: TextStyle {
                        font_px: 18.0,
                        ..Default::default()
                    },
                    ..TextSpan::lit(entry.label)
                }],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    let dropdown = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(200.0), Dimension::Points(36.0)],
                padding: Edges::px(8.0),
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            Dropdown {
                options: entry.options.iter().map(|(_, l)| (*l).to_owned()).collect(),
                selected: Some(mode.index()),
                open: false,
            },
            regia_ui::Classes(vec!["button".into()]),
            regia_ui::Themed::default(),
            PerformanceOverlayOption,
        ))
        .id();
    let row = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                align_items: Align::Center,
                gap: [Dimension::Points(16.0), Dimension::Points(16.0)],
                ..Default::default()
            }),
        ))
        .id();
    world.entity_mut(row).add_child(label).add_child(dropdown);
    if let Some(parent) = parent {
        world.entity_mut(parent).add_child(row);
    }
    row
}

/// One snapshot of everything the overlay shows (`perf_overlay.get` returns it as JSON).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PerfOverlayStats {
    /// Average FPS over the diagnostics window ([`Diagnostics::fps`]).
    pub fps: f64,
    /// Average frame time over the diagnostics window, ms.
    pub frame_avg_ms: f64,
    /// The last frame's interval ([`RealTime`] delta), ms.
    pub frame_ms: f32,
    /// Frames in the overlay's window (0 while hidden).
    pub window_frames: usize,
    /// Window median frame time, ms ([`HitchReport::p50_ms`]).
    pub p50_ms: f32,
    /// Window 99th percentile, ms.
    pub p99_ms: f32,
    /// Window longest frame, ms.
    pub longest_ms: f32,
    /// Window frames over 2 × median ([`HitchReport::over_2x`]).
    pub stutters_window: usize,
    /// Frames over 2 × median since warm-up while the overlay was visible.
    pub stutters_total: u64,
    /// The renderer's CPU time of its last frame ([`regia_render::FrameCpuTimings::total_ms`]),
    /// ms: `FrameRenderer::render_into` only (culling, recording, submit), not the sim, extract,
    /// the swapchain acquire or the present. The frame's wall time is [`Self::frame_ms`].
    pub cpu_ms: f32,
    /// The GPU frame ([`GpuTimings::frame_ms`]), ms; 0 without timestamp queries.
    pub gpu_ms: f32,
    /// Timestamp queries run (the device has them and timing is on).
    pub gpu_timing: bool,
    /// The major GPU passes by name ([`GpuTimings::passes`]), ms.
    pub gpu_passes: Vec<(String, f32)>,
    /// GPU scene section, ms.
    pub gpu_scene_ms: f32,
    /// Warm-up has ended (creates after it count).
    pub warmed_up: bool,
    /// Pipelines created after warm-up (frame-path counters).
    pub pipelines_after_warmup: u64,
    /// Bind groups created after warm-up.
    pub bind_groups_after_warmup: u64,
    /// Resident material texture bytes ([`regia_render::TextureMemorySummary::resident_bytes`]).
    pub texture_bytes: u64,
    /// The budget they are held to ([`regia_render::TextureMemorySummary::checked_budget`]).
    pub texture_budget_bytes: u64,
    /// Texture streaming on.
    pub texture_streaming: bool,
    /// GPU memory allocated, bytes (Detailed only, every 2 s, computed off the frame thread;
    /// `None` until the first report and where the backend has no allocator report).
    pub vram_allocated_bytes: Option<u64>,
    /// GPU memory reserved, bytes.
    pub vram_reserved_bytes: Option<u64>,
    /// Process resident set, bytes (Linux/Android).
    pub rss_bytes: Option<u64>,
    /// Render profile preset name, else the quality tier it matches (`"Medium"`), or `"Custom"`.
    pub preset: String,
    /// The player's lighting option ([`regia_render::GiLook`]).
    pub gi_look: String,
    /// The frame's GI mode ([`regia_render::GiMode`]) and whether probes trace.
    pub gi_mode: String,
    /// Output (window surface) size, px.
    pub output: (u32, u32),
    /// Internal render size, px.
    pub internal: (u32, u32),
    /// Render scale.
    pub render_scale: f32,
    /// `"native"`, `"TAAU"`, `"Sgsr"` or `"Bilinear"`.
    pub upscaler: String,
    /// The overlay's own CPU time last frame (its systems), µs.
    pub overlay_cpu_us: f32,
    /// Uncaptured wgpu errors since start ([`regia_render::gpu_health::gpu_error_count`]);
    /// shown as a "GPU errors" line when non-zero.
    pub gpu_errors: u64,
}

impl PerfOverlayStats {
    /// The passes sorted most expensive first (non-zero only).
    #[must_use]
    pub fn top_passes(&self, n: usize) -> Vec<(String, f32)> {
        let mut passes: Vec<(String, f32)> = self
            .gpu_passes
            .iter()
            .filter(|(_, ms)| *ms > 0.0)
            .cloned()
            .collect();
        passes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        passes.truncate(n);
        passes
    }
}

/// The overlay's runtime state (frame window, warm-up, entities).
#[derive(Resource, Default)]
pub struct PerfOverlayState {
    ring: VecDeque<HitchFrame>,
    ring_ms: f32,
    last_counters: Option<FramePathCounters>,
    warm_left: u32,
    warm_started: bool,
    baseline: Option<FramePathCounters>,
    stutters_total: u64,
    p50_ms: f32,
    root: Option<Entity>,
    panel: Option<Entity>,
    header: Option<Entity>,
    graph: Option<Entity>,
    labels: Option<Entity>,
    values: Option<Entity>,
    passes_row: Option<Entity>,
    pass_names: Option<Entity>,
    pass_values: Option<Entity>,
    shown: PerformanceOverlayMode,
    last_graph_s: f64,
    last_text_s: f64,
    last_vram_s: f64,
    vram: Option<(u64, u64)>,
    rss: Option<u64>,
    /// The GPU memory report in flight on its worker.
    memory: MemoryPoll,
    /// A report has come back (the first one is asked for at once, then every 2 s).
    memory_seen: bool,
    /// Tests: a stand-in for [`regia_render::memory_report`] (runs without a device).
    #[cfg(test)]
    memory_job: Option<fn() -> MemorySample>,
    timing_enabled_by_us: bool,
    last_settings_choice: Option<Option<PerformanceOverlayMode>>,
    overlay_cpu_us: f32,
    stats: PerfOverlayStats,
}

impl PerfOverlayState {
    /// The last refreshed snapshot (what the overlay shows; refreshed at 4 Hz while visible).
    #[must_use]
    pub fn stats(&self) -> &PerfOverlayStats {
        &self.stats
    }

    /// The overlay's root UI entity once it has been shown.
    #[must_use]
    pub fn root(&self) -> Option<Entity> {
        self.root
    }

    /// The frame-time graph node.
    #[must_use]
    pub fn graph(&self) -> Option<Entity> {
        self.graph
    }

    /// The mode currently drawn.
    #[must_use]
    pub fn shown(&self) -> PerformanceOverlayMode {
        self.shown
    }
}

/// Adds the overlay (`regia::DefaultPlugins` adds it with the `ui` feature).
#[derive(Default)]
pub struct PerfOverlayPlugin;

impl Plugin for PerfOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PerformanceOverlay>()
            .init_resource::<PerfOverlayState>()
            .init_resource::<PerformanceOverlayPlacement>()
            .register_type::<PerformanceOverlayMode>()
            .add_systems(Update, (perf_overlay_action, sync_settings_entries))
            .add_systems(PostUpdate, update_perf_overlay.before(PostUpdateSet::UiLayout));
    }
}

/// `Update`: the optional [`PERF_OVERLAY_ACTION`] cycles the mode as a player choice.
fn perf_overlay_action(world: &mut World) {
    let pressed = world
        .get_resource::<regia_input::Actions>()
        .is_some_and(|a| a.just_pressed_this_frame(PERF_OVERLAY_ACTION));
    if pressed {
        let (mode, _) = active_mode(world);
        set_player_mode(world, mode.next());
    }
}

/// `Update`: keeps [`PerformanceOverlayOption`] dropdowns and the setting in step.
fn sync_settings_entries(world: &mut World) {
    let mut q = world.query_filtered::<(Entity, Ref<Dropdown>), With<PerformanceOverlayOption>>();
    let entries: Vec<(Entity, bool, Option<usize>)> = q
        .iter(world)
        .map(|(e, d)| (e, d.is_changed() && !d.is_added(), d.selected))
        .collect();
    if entries.is_empty() {
        return;
    }
    let (mode, _) = active_mode(world);
    for (entity, changed, selected) in entries {
        let chosen = selected.and_then(|i| PerformanceOverlayMode::ALL.get(i).copied());
        if changed && let Some(chosen) = chosen.filter(|c| *c != mode) {
            set_player_mode(world, chosen);
            continue;
        }
        if chosen != Some(mode)
            && let Some(mut dropdown) = world.get_mut::<Dropdown>(entity)
        {
            dropdown.selected = Some(mode.index());
        }
    }
}

/// Turns the runtime renderer's GPU timestamps on or off; returns whether it was on before.
fn set_gpu_timing(world: &mut World, enabled: bool) -> Option<bool> {
    if !world.contains_resource::<RenderDevice>() || !world.contains_resource::<RuntimeRenderer>() {
        return None;
    }
    world.resource_scope(|world, mut renderer: Mut<RuntimeRenderer>| {
        let device = world.get_resource::<RenderDevice>()?;
        let was = renderer.0.gpu_timing_enabled();
        if was != enabled {
            renderer.0.set_gpu_timing(device, enabled);
        }
        Some(was)
    })
}

/// `PostUpdate` (before UI layout): warm-up accounting always; while visible, records the
/// frame, redraws the graph (20 Hz) and the text (4 Hz).
pub fn update_perf_overlay(world: &mut World) {
    let _span = tracing::trace_span!("perf_overlay").entered();
    let started = Instant::now();
    let Some(mut state) = world.remove_resource::<PerfOverlayState>() else {
        return;
    };
    // A changed player choice beats an override.
    let choice = world
        .get_resource::<GameSettings>()
        .map(|s| s.performance_overlay);
    if state.last_settings_choice.is_some() && state.last_settings_choice != choice {
        set_override(world, None);
    }
    state.last_settings_choice = choice;
    tick_warmup(world, &mut state);
    let (mode, _) = active_mode(world);
    if mode == PerformanceOverlayMode::Off {
        if state.shown != PerformanceOverlayMode::Off {
            hide(world, &mut state);
        }
        world.insert_resource(state);
        return;
    }
    if state.shown != mode {
        show(world, &mut state, mode);
    } else if world
        .get_resource::<RenderDevice>()
        .is_some_and(|d| d.capabilities.timestamp_query)
        && world
            .get_resource::<RuntimeRenderer>()
            .is_some_and(|r| !r.0.gpu_timing_enabled())
    {
        // Someone else turned the timestamps off under the visible overlay (the first-launch
        // quality probe restoring its "was off"): take them back, and own them from now on.
        if set_gpu_timing(world, true) == Some(false) {
            state.timing_enabled_by_us = true;
        }
    }
    apply_placement(world, &state);
    record_frame(world, &mut state);
    let now = world
        .get_resource::<RealTime>()
        .map_or(0.0, |t| t.elapsed().as_secs_f64());
    if now - state.last_graph_s >= GRAPH_EVERY_S || state.last_graph_s == 0.0 {
        state.last_graph_s = now;
        draw_graph(world, &state);
    }
    if now - state.last_text_s >= TEXT_EVERY_S || state.last_text_s == 0.0 {
        state.last_text_s = now;
        if let Some(sample) = state.memory.take() {
            state.vram = sample.vram;
            state.rss = sample.rss;
            state.memory_seen = true;
        }
        if mode == PerformanceOverlayMode::Detailed
            && (now - state.last_vram_s >= VRAM_EVERY_S || !state.memory_seen)
            && !state.memory.busy()
            && let Some(job) = memory_job(world, &state)
        {
            state.last_vram_s = now;
            state.memory.start(job);
        }
        state.stats = collect(world, &state);
        state.p50_ms = state.stats.p50_ms;
        fit_scale(world, &state);
        write_text(world, &state, mode);
    }
    state.overlay_cpu_us = started.elapsed().as_secs_f32() * 1.0e6;
    world.insert_resource(state);
}

/// One GPU memory report as the overlay shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MemorySample {
    /// Allocated and reserved bytes, where the backend reports them.
    vram: Option<(u64, u64)>,
    /// Process RSS.
    rss: Option<u64>,
}

impl MemorySample {
    fn of(report: &regia_render::MemoryReport) -> Self {
        Self {
            vram: report
                .available
                .then_some((report.allocated_bytes, report.reserved_bytes)),
            rss: report.rss_bytes,
        }
    }
}

type MemoryJob = Box<dyn FnOnce() -> MemorySample + Send>;

/// The report job for this frame: [`regia_render::memory_report`] on a device clone, `None`
/// without a device.
fn memory_job(world: &World, state: &PerfOverlayState) -> Option<MemoryJob> {
    #[cfg(test)]
    if let Some(job) = state.memory_job {
        return Some(Box::new(job));
    }
    let _ = state;
    let device = world.get_resource::<RenderDevice>()?.clone();
    Some(Box::new(move || {
        MemorySample::of(&regia_render::memory_report(&device))
    }))
}

/// One report at a time on a short-lived worker thread (`regia-perf-memory`): the allocator
/// walk and `/proc` read never run on the frame thread. The result is picked up by a later
/// overlay refresh.
#[derive(Default)]
struct MemoryPoll {
    done: Arc<Mutex<Option<MemorySample>>>,
    busy: Arc<AtomicBool>,
}

/// Clears [`MemoryPoll::busy`] when the worker ends, also by unwinding.
struct NotBusy(Arc<AtomicBool>);

impl Drop for NotBusy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl MemoryPoll {
    /// A report is running.
    fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// Starts `job` on a worker unless one is running; `false` when it did not start.
    fn start(&self, job: MemoryJob) -> bool {
        if self.busy.swap(true, Ordering::AcqRel) {
            return false;
        }
        let done = self.done.clone();
        let guard = NotBusy(self.busy.clone());
        let spawned = std::thread::Builder::new()
            .name("regia-perf-memory".into())
            .spawn(move || {
                let sample = job();
                *done.lock().unwrap_or_else(PoisonError::into_inner) = Some(sample);
                drop(guard);
            });
        if let Err(err) = spawned {
            // The guard went down with the closure: `busy` is clear again.
            tracing::debug!(target: "regia_framework", "perf overlay: memory report thread: {err}");
            return false;
        }
        true
    }

    /// The finished report, once.
    fn take(&self) -> Option<MemorySample> {
        self.done.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

fn tick_warmup(world: &World, state: &mut PerfOverlayState) {
    if !state.warm_started {
        state.warm_started = true;
        state.warm_left = WARMUP_FRAMES;
    }
    if world
        .get_resource::<GameFrameWarm>()
        .is_some_and(GameFrameWarm::is_pending)
    {
        state.warm_left = state.warm_left.max(REWARM_FRAMES);
        state.baseline = None;
    }
    if state.warm_left > 0 {
        state.warm_left -= 1;
        if state.warm_left == 0 {
            state.baseline = Some(regia_render::frame_path_counters_all_threads());
        }
    }
}

fn record_frame(world: &World, state: &mut PerfOverlayState) {
    let now = regia_render::frame_path_counters_all_threads();
    let last = state.last_counters.replace(now);
    let frame_ms = world
        .get_resource::<RealTime>()
        .map_or(0.0, |t| t.delta().as_secs_f32() * 1000.0);
    let Some(last) = last else {
        return;
    };
    if frame_ms <= 0.0 {
        return;
    }
    let (cpu_ms, gpu_ms) = renderer_sample(world).map_or((0.0, 0.0), |r| (r.cpu.total_ms, r.gpu.frame_ms));
    if state.baseline.is_some() && state.p50_ms > 0.0 && frame_ms > 2.0 * state.p50_ms {
        state.stutters_total += 1;
    }
    state.ring.push_back(HitchFrame {
        frame_ms,
        cpu_ms,
        gpu_ms,
        counters: now.since(last),
    });
    state.ring_ms += frame_ms;
    while state.ring.len() > RING_CAP || (state.ring_ms > WINDOW_SECONDS * 1000.0 && state.ring.len() > 1) {
        if let Some(old) = state.ring.pop_front() {
            state.ring_ms -= old.frame_ms;
        }
    }
}

/// What the overlay reads from a renderer each frame: all `Copy` values from
/// [`FrameRenderer`](regia_render::FrameRenderer)'s own accessors.
#[derive(Clone, Copy, Debug, Default)]
pub struct RendererSample {
    /// [`FrameRenderer::cpu_timings`](regia_render::FrameRenderer::cpu_timings).
    pub cpu: regia_render::FrameCpuTimings,
    /// [`FrameRenderer::gpu_timings`](regia_render::FrameRenderer::gpu_timings).
    pub gpu: GpuTimings,
    /// [`FrameRenderer::gpu_timing_enabled`](regia_render::FrameRenderer::gpu_timing_enabled).
    pub gpu_timing: bool,
    /// [`FrameRenderer::texture_memory_summary`](regia_render::FrameRenderer::texture_memory_summary).
    pub texture: regia_render::TextureMemorySummary,
    /// [`FrameRenderer::last_extent`](regia_render::FrameRenderer::last_extent).
    pub extent: Option<regia_render::FrameExtent>,
}

impl RendererSample {
    /// Reads `renderer`.
    #[must_use]
    pub fn of(renderer: &regia_render::FrameRenderer) -> Self {
        Self {
            cpu: renderer.cpu_timings(),
            gpu: renderer.gpu_timings(),
            gpu_timing: renderer.gpu_timing_enabled(),
            texture: renderer.texture_memory_summary(),
            extent: renderer.last_extent(),
        }
    }
}

/// For hosts that drive their own [`FrameRenderer`](regia_render::FrameRenderer) instead of
/// the runtime's [`RuntimeRenderer`]: insert `PerfOverlayRendererSample(RendererSample::of(&r))`
/// after each frame and the overlay reads it instead.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct PerfOverlayRendererSample(pub RendererSample);

/// The renderer the overlay reads: a host's [`PerfOverlayRendererSample`], else the
/// [`RuntimeRenderer`].
fn renderer_sample(world: &World) -> Option<RendererSample> {
    world
        .get_resource::<PerfOverlayRendererSample>()
        .map(|s| s.0)
        .or_else(|| {
            world
                .get_resource::<RuntimeRenderer>()
                .map(|r| RendererSample::of(&r.0))
        })
}

/// A fresh snapshot from the engine sources (and the overlay's window, empty while hidden).
#[must_use]
pub fn snapshot(world: &World) -> PerfOverlayStats {
    match world.get_resource::<PerfOverlayState>() {
        Some(state) => collect(world, state),
        None => collect(world, &PerfOverlayState::default()),
    }
}

fn collect(world: &World, state: &PerfOverlayState) -> PerfOverlayStats {
    let mut s = PerfOverlayStats::default();
    if let Some(d) = world.get_resource::<Diagnostics>() {
        s.fps = d.fps;
        s.frame_avg_ms = d.frame_time_avg.as_secs_f64() * 1000.0;
    }
    s.frame_ms = world
        .get_resource::<RealTime>()
        .map_or(0.0, |t| t.delta().as_secs_f32() * 1000.0);
    let frames: Vec<HitchFrame> = state.ring.iter().copied().collect();
    if !frames.is_empty() {
        let report = HitchReport::new(&frames, HitchTargets::SPEC);
        s.window_frames = report.frames;
        s.p50_ms = report.p50_ms;
        s.p99_ms = report.p99_ms;
        s.longest_ms = report.longest_ms;
        s.stutters_window = report.over_2x;
    }
    s.stutters_total = state.stutters_total;
    let sample = renderer_sample(world);
    if let Some(r) = sample {
        let gpu: GpuTimings = r.gpu;
        s.cpu_ms = r.cpu.total_ms;
        s.gpu_ms = gpu.frame_ms;
        s.gpu_scene_ms = gpu.scene_ms;
        s.gpu_timing = r.gpu_timing;
        s.gpu_passes = gpu
            .passes()
            .iter()
            .map(|(n, ms)| ((*n).to_owned(), *ms))
            .collect();
        s.texture_bytes = r.texture.resident_bytes;
        s.texture_budget_bytes = r.texture.checked_budget;
        s.texture_streaming = r.texture.streaming;
    }
    s.warmed_up = state.baseline.is_some();
    if let Some(base) = state.baseline {
        let since = regia_render::frame_path_counters_all_threads().since(base);
        s.pipelines_after_warmup = since.pipelines;
        s.bind_groups_after_warmup = since.bind_groups;
    }
    s.vram_allocated_bytes = state.vram.map(|v| v.0);
    s.vram_reserved_bytes = state.vram.map(|v| v.1);
    s.rss_bytes = state.rss;
    s.output = world
        .get_resource::<WindowSurfaces>()
        .and_then(WindowSurfaces::primary_size)
        .unwrap_or((0, 0));
    if let Some(settings) = world.get_resource::<RenderSettings>() {
        // Medium is Desktop High at render scale 0.67, not a preset row (ADR-157): name the tier.
        s.preset = settings
            .preset()
            .map(|p| p.name().to_owned())
            .or_else(|| {
                regia_render::quality::QualityTier::of_settings(settings).map(|t| t.name().to_owned())
            })
            .unwrap_or_else(|| "Custom".to_owned());
        let quality = settings.quality();
        s.gi_look = format!("{:?}", settings.effective_gi_look());
        s.gi_mode = format!(
            "{:?}{}",
            quality.gi,
            if quality.screen_space_gi { " + SSGI" } else { "" }
        );
        s.render_scale = quality.render_scale();
        s.internal = quality.scaled_extent(s.output.0, s.output.1);
        s.upscaler = if s.render_scale >= 0.999 {
            "native".to_owned()
        } else if quality.taau {
            "TAAU".to_owned()
        } else {
            format!("{:?}", quality.spatial_upscaler)
        };
    }
    // What the renderer actually drew last (offscreen captures included) beats the settings.
    if let Some(extent) = sample.and_then(|r| r.extent) {
        s.output = extent.output;
        s.internal = extent.internal;
        s.render_scale = extent.internal.0 as f32 / extent.output.0.max(1) as f32;
        s.upscaler = extent.upscaler.to_owned();
    }
    s.overlay_cpu_us = state.overlay_cpu_us;
    s.gpu_errors = regia_render::gpu_health::gpu_error_count();
    s
}

// ---- drawing ----

const PANEL_BG: UiColor = UiColor::linear(0.0, 0.0, 0.0, 0.62);
const GRAPH_BG: UiColor = UiColor::linear(0.02, 0.02, 0.02, 0.55);
const TEXT: UiColor = UiColor::linear(0.85, 0.85, 0.85, 1.0);
const DIM: UiColor = UiColor::linear(0.45, 0.45, 0.45, 1.0);
const GOOD: UiColor = UiColor::linear(0.25, 0.8, 0.3, 1.0);
const WARN: UiColor = UiColor::linear(0.95, 0.7, 0.1, 1.0);
const BAD: UiColor = UiColor::linear(0.95, 0.15, 0.1, 1.0);
const LINE: UiColor = UiColor::linear(1.0, 1.0, 1.0, 0.28);

fn span(text: impl Into<String>, px: f32, color: UiColor) -> TextSpan {
    TextSpan {
        style: TextStyle {
            font_px: px,
            color,
            ..Default::default()
        },
        ..TextSpan::lit(text)
    }
}

fn text_node(world: &mut World, align: TextAlign, grow: bool) -> Entity {
    world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                flex_grow: if grow { 1.0 } else { 0.0 },
                ..Default::default()
            }),
            Text {
                spans: vec![span(" ", 13.0, TEXT)],
                align,
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id()
}

fn spawn(world: &mut World, state: &mut PerfOverlayState) {
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPhysicalSize,
                // Over every game UI.
                sort_order: i32::MAX - 1,
                ..Default::default()
            },
            UiNode,
            NodeStyle::new(Style {
                align_items: Align::Start,
                ..Default::default()
            }),
        ))
        .id();
    let panel = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                margin: Edges::px(10.0),
                padding: Edges::px(8.0),
                gap: [Dimension::Points(5.0), Dimension::Points(5.0)],
                size: [Dimension::Points(360.0), Dimension::Auto],
                align_self: Align::Start,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(PANEL_BG),
            },
            Border {
                color: UiColor::TRANSPARENT,
                widths: [0.0; 4],
                radius: [5.0; 4],
            },
        ))
        .id();
    let header = text_node(world, TextAlign::Start, false);
    let graph = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(56.0)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(GRAPH_BG),
            },
            UiCanvas::default(),
        ))
        .id();
    let row = |world: &mut World| {
        world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    direction: FlexDirection::Row,
                    gap: [Dimension::Points(10.0), Dimension::Points(10.0)],
                    ..Default::default()
                }),
            ))
            .id()
    };
    let stats_row = row(world);
    let labels = text_node(world, TextAlign::Start, false);
    let values = text_node(world, TextAlign::End, true);
    world.entity_mut(stats_row).add_child(labels).add_child(values);
    let passes_row = row(world);
    let pass_names = text_node(world, TextAlign::Start, false);
    let pass_values = text_node(world, TextAlign::End, true);
    world
        .entity_mut(passes_row)
        .add_child(pass_names)
        .add_child(pass_values);
    world
        .entity_mut(panel)
        .add_child(header)
        .add_child(graph)
        .add_child(stats_row)
        .add_child(passes_row);
    world.entity_mut(root).add_child(panel);
    state.root = Some(root);
    state.panel = Some(panel);
    state.header = Some(header);
    state.graph = Some(graph);
    state.labels = Some(labels);
    state.values = Some(values);
    state.passes_row = Some(passes_row);
    state.pass_names = Some(pass_names);
    state.pass_values = Some(pass_values);
}

/// Above 1300 px of output height the panel scales with the screen (1.33× at 1440p, 2× at
/// 2160p) so it stays readable; below it keeps one dp per UI px (the Deck, laptops).
fn fit_scale(world: &mut World, state: &PerfOverlayState) {
    let want = if state.stats.output.1 >= 1300 {
        ScaleMode::ScaleWithScreen {
            reference: [1920.0, 1080.0],
            match_width: 0.0,
        }
    } else {
        ScaleMode::ConstantPhysicalSize
    };
    if let Some(root) = state.root
        && let Some(mut ui_root) = world.get_mut::<UiRoot>(root)
        && ui_root.scale_mode != want
    {
        ui_root.scale_mode = want;
    }
}

fn set_display(world: &mut World, entity: Option<Entity>, display: Display) {
    if let Some(e) = entity
        && let Some(mut style) = world.get_mut::<NodeStyle>(e)
        && style.0.display != display
    {
        style.0.display = display;
    }
}

fn show(world: &mut World, state: &mut PerfOverlayState, mode: PerformanceOverlayMode) {
    if state.root.is_none_or(|r| world.get_entity(r).is_err()) {
        spawn(world, state);
    }
    set_display(world, state.root, Display::Flex);
    let passes = if mode == PerformanceOverlayMode::Detailed {
        Display::Flex
    } else {
        Display::None
    };
    set_display(world, state.passes_row, passes);
    if state.shown == PerformanceOverlayMode::Off {
        state.timing_enabled_by_us = set_gpu_timing(world, true) == Some(false);
        state.ring.clear();
        state.ring_ms = 0.0;
        state.last_counters = None;
    }
    state.last_graph_s = 0.0;
    state.last_text_s = 0.0;
    state.shown = mode;
}

fn hide(world: &mut World, state: &mut PerfOverlayState) {
    set_display(world, state.root, Display::None);
    if state.timing_enabled_by_us {
        set_gpu_timing(world, false);
        state.timing_enabled_by_us = false;
    }
    state.ring.clear();
    state.ring_ms = 0.0;
    state.last_counters = None;
    state.shown = PerformanceOverlayMode::Off;
}

fn frame_color(ms: f32) -> UiColor {
    if ms <= 16.7 {
        GOOD
    } else if ms <= 33.3 {
        WARN
    } else {
        BAD
    }
}

fn draw_graph(world: &mut World, state: &PerfOverlayState) {
    let Some(graph) = state.graph else {
        return;
    };
    let mut rects = Vec::with_capacity(GRAPH_COLUMNS + 2);
    let y = |ms: f32| 1.0 - (ms / GRAPH_TOP_MS).clamp(0.0, 1.0);
    let n = state.ring.len();
    let p50 = {
        let mut v: Vec<f32> = state.ring.iter().map(|f| f.frame_ms).collect();
        v.sort_by(f32::total_cmp);
        regia_render::hitch::percentile_sorted(&v, 0.5)
    };
    if n > 0 {
        let columns = n.min(GRAPH_COLUMNS);
        let w = 1.0 / GRAPH_COLUMNS as f32;
        for c in 0..columns {
            // Newest frames at the right edge.
            let lo = c * n / columns;
            let hi = ((c + 1) * n / columns).max(lo + 1);
            let worst = state
                .ring
                .range(lo..hi.min(n))
                .map(|f| f.frame_ms)
                .fold(0.0f32, f32::max);
            let spike = p50 > 0.0 && worst > 2.0 * p50;
            let color = if spike {
                BAD
            } else {
                frame_color(worst).with_alpha(0.85)
            };
            let x0 = 1.0 - (columns - c) as f32 * w;
            rects.push(CanvasRect {
                min: [x0, y(worst)],
                max: [x0 + w * 0.8, 1.0],
                color,
            });
        }
    }
    for line in [16.7, 33.3] {
        let at = y(line);
        rects.push(CanvasRect {
            min: [0.0, at - 0.012],
            max: [1.0, at + 0.012],
            color: LINE,
        });
    }
    if let Some(mut canvas) = world.get_mut::<UiCanvas>(graph) {
        canvas.rects = rects;
    }
}

fn set_spans(world: &mut World, entity: Option<Entity>, spans: Vec<TextSpan>) {
    if let Some(e) = entity
        && let Some(mut text) = world.get_mut::<Text>(e)
        && text.spans != spans
    {
        text.spans = spans;
    }
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn write_text(world: &mut World, state: &PerfOverlayState, mode: PerformanceOverlayMode) {
    let s = &state.stats;
    const PX: f32 = 13.0;
    let frame = if s.p50_ms > 0.0 {
        s.p50_ms
    } else {
        s.frame_avg_ms as f32
    };
    set_spans(
        world,
        state.header,
        vec![
            span(format!("{:.0} FPS", s.fps), 20.0, frame_color(frame)),
            span(format!("   {frame:.1} ms"), 15.0, TEXT),
            span(format!("   {}", s.preset), 13.0, DIM),
        ],
    );
    let mut labels = Vec::new();
    let mut values: Vec<TextSpan> = Vec::new();
    let mut line = |label: &str, value: String, color: UiColor| {
        let nl = if labels.is_empty() { "" } else { "\n" };
        labels.push(span(format!("{nl}{label}"), PX, DIM));
        values.push(span(format!("{nl}{value}"), PX, color));
    };
    // The larger of the two limits the frame when it fills at least 90 % of it.
    let bound = if frame <= 0.0 || s.cpu_ms.max(s.gpu_ms) < 0.9 * frame {
        ""
    } else if s.gpu_ms >= s.cpu_ms {
        "  GPU-bound"
    } else {
        "  CPU-bound"
    };
    let gpu = if s.gpu_timing && s.gpu_ms > 0.0 {
        format!("{:.2}", s.gpu_ms)
    } else {
        "n/a".to_owned()
    };
    line(
        "Render CPU / GPU",
        format!("{:.2} / {gpu} ms{bound}", s.cpu_ms),
        TEXT,
    );
    let top = s.top_passes(3);
    if !top.is_empty() {
        let list: Vec<String> = top.iter().map(|(n, ms)| format!("{n} {ms:.2}")).collect();
        line("GPU top", list.join("  "), TEXT);
    }
    if s.texture_budget_bytes > 0 {
        let pct = s.texture_bytes as f64 * 100.0 / s.texture_budget_bytes as f64;
        let color = if pct > 100.0 {
            BAD
        } else if pct > 90.0 {
            WARN
        } else {
            TEXT
        };
        line(
            "Textures",
            format!(
                "{:.2} / {:.2} GiB ({pct:.0}%)",
                gib(s.texture_bytes),
                gib(s.texture_budget_bytes)
            ),
            color,
        );
    }
    let stutter_color = if s.stutters_window > 0 { WARN } else { TEXT };
    line(
        "Stutters",
        format!(
            "{} in {WINDOW_SECONDS:.0} s  ·  {} total",
            s.stutters_window, s.stutters_total
        ),
        stutter_color,
    );
    let creates = if s.warmed_up {
        format!(
            "{} pipelines  ·  {} bind groups",
            s.pipelines_after_warmup, s.bind_groups_after_warmup
        )
    } else {
        "warming up".to_owned()
    };
    let creates_color = if s.pipelines_after_warmup + s.bind_groups_after_warmup > 0 {
        WARN
    } else {
        TEXT
    };
    line("Creates", creates, creates_color);
    line("Lighting", format!("{}  ({})", s.gi_look, s.gi_mode), TEXT);
    let resolution = if s.output.0 == 0 {
        "n/a (nothing rendered yet)".to_owned()
    } else {
        format!(
            "{}x{} > {}x{} ({:.0}%) {}",
            s.output.0,
            s.output.1,
            s.internal.0,
            s.internal.1,
            s.render_scale * 100.0,
            s.upscaler
        )
    };
    line("Resolution", resolution, TEXT);
    if s.gpu_errors > 0 {
        // Release builds log wgpu errors and keep drawing (ADR-159): say so on screen.
        line("GPU errors", format!("{} (see log)", s.gpu_errors), BAD);
    }
    if mode == PerformanceOverlayMode::Detailed {
        line(
            "p50 / p99 / max",
            format!("{:.1} / {:.1} / {:.1} ms", s.p50_ms, s.p99_ms, s.longest_ms),
            TEXT,
        );
        if let (Some(a), Some(r)) = (s.vram_allocated_bytes, s.vram_reserved_bytes) {
            // Reserved is the device memory the allocator holds (its blocks); allocated is what
            // its sub-allocation list names, which leaves out dedicated allocations.
            line(
                "GPU memory",
                format!("{:.2} GiB reserved  ({:.2} sub-alloc)", gib(r), gib(a)),
                TEXT,
            );
        }
        if let Some(rss) = s.rss_bytes {
            line("Process RSS", format!("{:.2} GiB", gib(rss)), TEXT);
        }
        line("Overlay systems", format!("{:.0} us", s.overlay_cpu_us), DIM);
    }
    set_spans(world, state.labels, labels);
    set_spans(world, state.values, values);
    if mode == PerformanceOverlayMode::Detailed {
        let mut names = vec![span("GPU pass", PX, DIM)];
        let mut ms = vec![span("ms", PX, DIM)];
        for (name, v) in &s.gpu_passes {
            names.push(span(format!("\n{name}"), PX, TEXT));
            ms.push(span(format!("\n{v:.2}"), PX, if *v > 0.0 { TEXT } else { DIM }));
        }
        names.push(span("\nscene / frame", PX, TEXT));
        ms.push(span(
            format!("\n{:.2} / {:.2}", s.gpu_scene_ms, s.gpu_ms),
            PX,
            TEXT,
        ));
        set_spans(world, state.pass_names, names);
        set_spans(world, state.pass_values, ms);
    }
}

/// Screen corner anchoring a performance overlay. Offsets point inward from this corner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayCorner {
    /// Top left (the existing default).
    #[default]
    TopLeft,
    /// Top right.
    TopRight,
    /// Bottom left.
    BottomLeft,
    /// Bottom right.
    BottomRight,
}

/// Live placement in logical UI pixels; insert before or after the overlay plugin. It is
/// separate from [`PerformanceOverlay`] so existing host struct literals remain valid.
/// Invalid offsets (negative or nonfinite) use the default 10 px on that axis.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerformanceOverlayPlacement {
    /// Screen corner.
    pub corner: OverlayCorner,
    /// Inward `[horizontal, vertical]` offset in UI pixels, following the overlay's DPI scale.
    pub offset: [f32; 2],
}

impl Default for PerformanceOverlayPlacement {
    fn default() -> Self {
        Self {
            corner: OverlayCorner::TopLeft,
            offset: [10.0; 2],
        }
    }
}

impl PerformanceOverlayPlacement {
    /// Effective placement, also returned by the agent. Direct resource writes with invalid
    /// offsets are sanitized before reaching layout.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            offset: self
                .offset
                .map(|v| if v.is_finite() && v >= 0.0 { v } else { 10.0 }),
            ..self
        }
    }
}

fn apply_placement(world: &mut World, state: &PerfOverlayState) {
    let placement = world
        .get_resource::<PerformanceOverlayPlacement>()
        .copied()
        .unwrap_or_default()
        .normalized();
    let (right, bottom) = match placement.corner {
        OverlayCorner::TopLeft => (false, false),
        OverlayCorner::TopRight => (true, false),
        OverlayCorner::BottomLeft => (false, true),
        OverlayCorner::BottomRight => (true, true),
    };
    let [x, y] = placement.offset.map(Dimension::Points);
    let inset = Edges {
        left: if right { Dimension::Auto } else { x },
        right: if right { x } else { Dimension::Auto },
        top: if bottom { Dimension::Auto } else { y },
        bottom: if bottom { y } else { Dimension::Auto },
    };
    if let Some(panel) = state.panel
        && let Some(mut style) = world.get_mut::<NodeStyle>(panel)
    {
        let absolute = regia_ui::style::PositionType::Absolute;
        if style.0.inset != inset || style.0.position != absolute || style.0.margin != Edges::px(0.0) {
            style.0.position = absolute;
            style.0.inset = inset;
            style.0.margin = Edges::px(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_choice_beats_host_default_and_override_beats_both() {
        let mut overlay = PerformanceOverlay::default_mode(PerformanceOverlayMode::Compact);
        let mut settings = GameSettings::default();
        assert_eq!(
            overlay.resolve(Some(&settings)),
            (PerformanceOverlayMode::Compact, ModeSource::HostDefault)
        );
        settings.performance_overlay = Some(PerformanceOverlayMode::Off);
        assert_eq!(
            overlay.resolve(Some(&settings)),
            (PerformanceOverlayMode::Off, ModeSource::Player)
        );
        overlay.override_mode = Some(PerformanceOverlayMode::Detailed);
        assert_eq!(
            overlay.resolve(Some(&settings)).0,
            PerformanceOverlayMode::Detailed
        );
        assert_eq!(
            PerformanceOverlay::default().resolve(None).0,
            PerformanceOverlayMode::Off,
            "engine default is off"
        );
    }

    /// A memory report that takes far longer than a frame.
    fn slow_report() -> MemorySample {
        std::thread::sleep(std::time::Duration::from_millis(400));
        MemorySample {
            vram: Some((1 << 30, 2 << 30)),
            rss: Some(3 << 30),
        }
    }

    /// v0.1.14 (DO27 10-17): Detailed mode ran `memory_report` on the frame thread every 2 s. Now
    /// the overlay's update never waits for it; the numbers appear at a later refresh.
    #[test]
    fn a_slow_memory_report_does_not_block_the_overlay() {
        let mut app = App::new();
        app.add_plugins(regia_ui::UiPlugin::code_only())
            .add_plugins(regia_app::DiagnosticsPlugin::default())
            .add_plugins(PerfOverlayPlugin);
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
        app.insert_resource(GameSettings::default())
            .insert_resource(RenderSettings::default());
        set_override(app.world_mut(), Some(PerformanceOverlayMode::Detailed));
        app.world_mut().resource_mut::<PerfOverlayState>().memory_job = Some(slow_report);
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        let mut worst_us = 0.0f32;
        let mut frames = 0;
        loop {
            app.update();
            frames += 1;
            let state = app.world().resource::<PerfOverlayState>();
            worst_us = worst_us.max(state.overlay_cpu_us);
            if state.stats().vram_allocated_bytes.is_some() || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let state = app.world().resource::<PerfOverlayState>();
        assert_eq!(
            state.stats().vram_allocated_bytes,
            Some(1 << 30),
            "after {frames} frames"
        );
        assert_eq!(state.stats().vram_reserved_bytes, Some(2 << 30));
        assert_eq!(state.stats().rss_bytes, Some(3 << 30));
        assert!(
            frames > 1,
            "the report arrived at a later refresh, not in the frame that asked"
        );
        // A blocking report would cost the overlay 400 ms in one frame; the budget is a quarter
        // of that so a slow debug build or a loaded runner still passes.
        assert!(
            worst_us < 100_000.0,
            "overlay update took {worst_us:.0} us while the report ran"
        );
        // One report at a time: a second start while one runs is refused.
        let poll = MemoryPoll::default();
        assert!(poll.start(Box::new(slow_report)));
        assert!(poll.busy() && !poll.start(Box::new(slow_report)));
    }

    #[test]
    fn modes_cycle_and_parse() {
        let mut m = PerformanceOverlayMode::Off;
        for want in [
            PerformanceOverlayMode::Compact,
            PerformanceOverlayMode::Detailed,
            PerformanceOverlayMode::Off,
        ] {
            m = m.next();
            assert_eq!(m, want);
        }
        assert_eq!(
            PerformanceOverlayMode::from_name("detailed"),
            Some(PerformanceOverlayMode::Detailed)
        );
        assert_eq!(
            settings_menu_entry().options[1].0,
            PerformanceOverlayMode::Compact.name()
        );
    }

    #[test]
    fn setting_round_trips_and_old_files_load() {
        let settings = GameSettings {
            performance_overlay: Some(PerformanceOverlayMode::Detailed),
            ..Default::default()
        };
        let text = settings.to_ron().expect("serializes");
        assert_eq!(
            GameSettings::from_ron_strict(&text).expect("strict parse"),
            settings
        );
        let old = "(graphics:(preset:Medium,shadow_quality:None,render_scale:None),display:(resolution:None,vsync:true,frame_cap:None),audio:(master:1.0,music:1.0,sfx:1.0),input:(sensitivity:1.0,invert_y:false),language:\"\",accessibility:(subtitles:true,text_scale:1.0))";
        assert_eq!(
            GameSettings::from_ron(old).expect("old file").performance_overlay,
            None
        );
    }
}
