// SPDX-License-Identifier: MPL-2.0
//! The `.regiatrace` timeline of the Diagnostics window (`docs/specs/54-gpu-breadcrumbs-and-trace-markers.md`
//! § 6, ADR-2771): marks as vertical lines, ranges as bars per lane, screenshot thumbnails at
//! their time, zoom and pan, and a record button that drives the process recorder through the
//! same `telemetry.trace` handler an agent calls. The agent twin is `editor.trace_timeline` and
//! `editor.trace_record` (`book/agents/editor-diagnostics.md`).
//!
//! The geometry is [`trace_layout`], a pure function of the trace and the view, so it is tested
//! without a GPU (the pattern of `profiler_panel`); the window only paints rectangles. Times in a
//! trace are clock based, so tests compare names, order and lanes (spec 54 § 7). Sub-pixel bars of
//! a row merge into one bar carrying their count, and the window keeps its layout by (trace
//! revision, view) so a steady repaint does not rebuild it (ADR-7325,
//! [`crate::timeline_geometry`]).
//!
//! Loading never panics: an empty or damaged file gives a one-line message
//! ([`load_trace_bytes`]), and a file that ends inside a chunk loads with a warning.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia::telemetry::trace::{EventKind, Lane, TraceFile, read_trace_bytes};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

/// Narrowest range bar drawn, points (a thinner one is widened so it stays visible).
pub const MIN_BAR_WIDTH: f32 = 2.0;

/// How far past the window's edge, in points, an item still counts as on the edge: far above the
/// rounding of the time-to-points mapping, far below a visible distance (ADR-9601).
const EDGE_SLACK: f64 = 1.0e-3;

/// Highest zoom (visible span = whole trace / zoom).
pub const MAX_ZOOM: f32 = 10_000.0;

/// Height of one timeline row, points.
pub const ROW_HEIGHT: f32 = 18.0;

/// Name of the lane the screenshot thumbnails sit on.
pub const SHOT_LANE: &str = "screenshots";

/// The visible part of the trace: `zoom` 1 shows everything; above 1 the window shows
/// `1 / zoom` of the span, starting `pan` (0 to 1) of the way through the scrollable remainder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelineView {
    /// 1 to [`MAX_ZOOM`].
    pub zoom: f32,
    /// 0 (start) to 1 (end).
    pub pan: f32,
    /// Width of the view, points.
    pub width: f32,
}

impl Default for TimelineView {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: 0.0,
            width: 800.0,
        }
    }
}

impl TimelineView {
    /// The view with `zoom` and `pan` forced into range (NaN becomes the default).
    #[must_use]
    pub fn clamped(self) -> Self {
        let fix =
            |v: f32, lo: f32, hi: f32, default: f32| if v.is_finite() { v.clamp(lo, hi) } else { default };
        Self {
            zoom: fix(self.zoom, 1.0, MAX_ZOOM, 1.0),
            pan: fix(self.pan, 0.0, 1.0, 0.0),
            width: fix(self.width, 0.0, 1.0e6, 800.0),
        }
    }
}

/// What a rectangle stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RectKind {
    /// A mark: a vertical line (width 0).
    Mark,
    /// A range on a CPU lane.
    Range,
    /// A GPU pass (breadcrumb pass or timestamped interval).
    GpuPass,
    /// A screenshot thumbnail.
    Screenshot,
}

impl RectKind {
    /// `mark`, `range`, `gpu_pass` or `screenshot`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Mark => "mark",
            Self::Range => "range",
            Self::GpuPass => "gpu_pass",
            Self::Screenshot => "screenshot",
        }
    }
}

/// One drawn item.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceRect {
    /// What it is.
    pub kind: RectKind,
    /// Mark, range or pass name (the screenshot's name, or `screenshot`).
    pub name: String,
    /// Lane name (`main`, `render`, `worker:<n>`, `gpu`, or [`SHOT_LANE`]).
    pub lane: String,
    /// Timeline row, from the top (a lane takes one row per nesting depth).
    pub row: usize,
    /// Nesting depth on its lane.
    pub depth: u32,
    /// Left edge, points from the view's left (clipped to the view).
    pub x: f32,
    /// Width, points (0 for marks and screenshots).
    pub width: f32,
    /// App frame the item belongs to.
    pub frame: u64,
    /// Start, ns on the trace time base.
    pub t_ns: u64,
    /// End, ns (equal to `t_ns` for instants).
    pub end_ns: u64,
    /// A range closed by an outer range or by stop; or one still open at the end of the file.
    pub forced: bool,
    /// Screenshot PNG path relative to the trace's folder.
    pub file: Option<String>,
    /// Ranges or passes the bar stands for: 1, or more when sub-pixel bars of the row were
    /// merged (the first one's name, the last end); 1 for marks and screenshots.
    pub count: u32,
}

impl TraceRect {
    /// The bar's label: its name, or `<count> ranges` for a merged bar.
    #[must_use]
    pub fn label(&self) -> String {
        if self.count > 1 {
            format!("{} ranges", self.count)
        } else {
            self.name.clone()
        }
    }
}

/// One lane of the timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneRows {
    /// Lane name.
    pub name: String,
    /// First row of the lane.
    pub first_row: usize,
    /// Rows the lane takes (its deepest nesting plus one).
    pub rows: usize,
}

/// A paired event, before it gets a row and an x position.
struct Item {
    kind: RectKind,
    name: String,
    lane: String,
    lane_key: (u8, u32),
    depth: u32,
    frame: u64,
    t_ns: u64,
    end_ns: u64,
    forced: bool,
    file: Option<String>,
    seq: usize,
}

fn lane_key(lane: Lane) -> (u8, u32) {
    match lane {
        Lane::Main => (1, 0),
        Lane::Render => (2, 0),
        Lane::Worker(n) => (3, n),
        Lane::Gpu => (4, 0),
    }
}

/// Pairs the trace's events into drawable items (unplaced).
/// Time bounds of one event. A GPU interval is logged when it finishes, so `t_ns` is after
/// `end_ns`; the bar still occupies `[begin_ns, end_ns]`.
fn event_span(event: &regia::telemetry::trace::Event) -> (u64, u64) {
    match &event.kind {
        EventKind::GpuInterval { begin_ns, end_ns, .. } => {
            let end = (*end_ns).max(*begin_ns);
            (
                event.t_ns.min(*begin_ns).min(end),
                event.t_ns.max(*begin_ns).max(end),
            )
        }
        _ => (event.t_ns, event.t_ns),
    }
}

fn items(trace: &TraceFile) -> (Vec<Item>, u64, u64) {
    let has_intervals = trace
        .events
        .iter()
        .any(|e| matches!(e.kind, EventKind::GpuInterval { .. }));
    let (t_min, t_max) = trace
        .events
        .iter()
        .map(event_span)
        .fold((u64::MAX, 0u64), |acc, span| {
            (acc.0.min(span.0), acc.1.max(span.1))
        });
    let t_min = if trace.events.is_empty() { 0 } else { t_min };
    let mut out: Vec<Item> = Vec::new();
    let mut open_ranges: HashMap<u64, usize> = HashMap::new();
    let mut open_passes: HashMap<(u16, u64, u64), usize> = HashMap::new();
    for (seq, e) in trace.events.iter().enumerate() {
        let lane = e.lane.name();
        let key = lane_key(e.lane);
        let mut push = |kind, name: String, depth, frame, end_ns, file| {
            out.push(Item {
                kind,
                name,
                lane: lane.clone(),
                lane_key: key,
                depth,
                frame,
                t_ns: e.t_ns,
                end_ns,
                forced: false,
                file,
                seq,
            });
            out.len() - 1
        };
        match &e.kind {
            EventKind::Mark { name, frame, .. } => {
                push(RectKind::Mark, name.clone(), 0, *frame, e.t_ns, None);
            }
            EventKind::RangeOpen {
                id,
                name,
                frame,
                depth,
                ..
            } => {
                let at = push(RectKind::Range, name.clone(), *depth, *frame, u64::MAX, None);
                open_ranges.insert(*id, at);
            }
            EventKind::RangeClose { id, forced, .. } => {
                if let Some(at) = open_ranges.remove(id) {
                    out[at].end_ns = e.t_ns.max(out[at].t_ns);
                    out[at].forced = *forced;
                }
            }
            EventKind::PassOpen {
                tag,
                name,
                frame,
                submission,
            } if !has_intervals => {
                let at = push(RectKind::GpuPass, name.to_string(), 0, *frame, u64::MAX, None);
                open_passes.insert((*tag, *submission, *frame), at);
            }
            EventKind::PassClose {
                tag,
                frame,
                submission,
                ..
            } if !has_intervals => {
                if let Some(at) = open_passes.remove(&(*tag, *submission, *frame)) {
                    out[at].end_ns = e.t_ns.max(out[at].t_ns);
                }
            }
            EventKind::GpuInterval {
                name,
                begin_ns,
                end_ns,
                frame,
                ..
            } => {
                out.push(Item {
                    kind: RectKind::GpuPass,
                    name: name.to_string(),
                    lane: Lane::Gpu.name(),
                    lane_key: lane_key(Lane::Gpu),
                    depth: 0,
                    frame: *frame,
                    t_ns: *begin_ns,
                    end_ns: (*end_ns).max(*begin_ns),
                    forced: false,
                    file: None,
                    seq,
                });
            }
            EventKind::Screenshot {
                name, frame, file, ..
            } => {
                out.push(Item {
                    kind: RectKind::Screenshot,
                    name: name.clone().unwrap_or_else(|| "screenshot".to_owned()),
                    lane: SHOT_LANE.to_owned(),
                    lane_key: (0, 0),
                    depth: 0,
                    frame: *frame,
                    t_ns: e.t_ns,
                    end_ns: e.t_ns,
                    forced: false,
                    file: Some(file.clone()),
                    seq,
                });
            }
            _ => {}
        }
    }
    // A range or pass never closed runs to the end of the file.
    for item in &mut out {
        if item.end_ns == u64::MAX {
            item.end_ns = t_max.max(item.t_ns);
            item.forced = true;
        }
    }
    (out, t_min, t_max.max(t_min))
}

/// The lanes of a trace with their rows, in display order: screenshots, main, render, workers,
/// gpu. Only lanes that have items appear.
#[must_use]
pub fn trace_lanes(trace: &TraceFile) -> Vec<LaneRows> {
    lanes_of(&items(trace).0)
}

fn lanes_of(items: &[Item]) -> Vec<LaneRows> {
    let mut depth: HashMap<(u8, u32), (String, u32)> = HashMap::new();
    for item in items {
        let entry = depth
            .entry(item.lane_key)
            .or_insert_with(|| (item.lane.clone(), 0));
        entry.1 = entry.1.max(item.depth);
    }
    let mut keys: Vec<_> = depth.into_iter().collect();
    keys.sort_by_key(|(k, _)| *k);
    let mut row = 0;
    keys.into_iter()
        .map(|(_, (name, max_depth))| {
            let rows = max_depth as usize + 1;
            let lane = LaneRows {
                name,
                first_row: row,
                rows,
            };
            row += rows;
            lane
        })
        .collect()
}

/// Lays the trace out in a view: ordered by frame, then time, then file order; each item on the
/// row of its lane and depth; x and width from the visible time window. Items wholly outside the
/// window are left out and the rest is clipped to it. Deterministic for a given trace and view.
#[must_use]
pub fn trace_layout(trace: &TraceFile, view: &TimelineView) -> Vec<TraceRect> {
    let view = view.clamped();
    let (mut items, t_min, t_max) = items(trace);
    if items.is_empty() || view.width <= 0.0 {
        return Vec::new();
    }
    let lanes = lanes_of(&items);
    let span = (t_max - t_min).max(1) as f64;
    let visible = span / f64::from(view.zoom);
    let start = t_min as f64 + f64::from(view.pan) * (span - visible);
    let scale = f64::from(view.width) / visible;
    let at = |t: u64| (t as f64 - start) * scale;
    items.sort_by_key(|i| (i.frame, i.t_ns, i.seq));
    let row_of = |item: &Item| {
        lanes
            .iter()
            .find(|l| l.name == item.lane)
            .map(|lane| lane.first_row + item.depth as usize)
    };
    // Sub-pixel bars of a row merge (ADR-7325): `merged[i]` is the merged extent of a group's
    // first item, `absorbed[i]` the other items of a group.
    let mut bars: Vec<crate::timeline_geometry::PixelBar> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let (x0, x1) = (at(item.t_ns), at(item.end_ns));
        if !matches!(item.kind, RectKind::Range | RectKind::GpuPass) || x1 < 0.0 || x0 > f64::from(view.width)
        {
            continue;
        }
        if let Some(row) = row_of(item) {
            bars.push(crate::timeline_geometry::PixelBar {
                row,
                x0,
                x1,
                t0: item.t_ns,
                t1: item.end_ns,
                item: i,
                count: 1,
            });
        }
    }
    bars.sort_by(|a, b| {
        a.row
            .cmp(&b.row)
            .then(a.x0.total_cmp(&b.x0))
            .then(a.item.cmp(&b.item))
    });
    let mut absorbed = vec![false; items.len()];
    for bar in &bars {
        absorbed[bar.item] = true;
    }
    let mut merged: Vec<Option<crate::timeline_geometry::PixelBar>> = vec![None; items.len()];
    for bar in crate::timeline_geometry::merge_subpixel_bars(&bars) {
        absorbed[bar.item] = false;
        merged[bar.item] = Some(bar);
    }
    let mut out = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        if absorbed[i] {
            continue;
        }
        let group = merged[i];
        let (x0, x1) = group.map_or_else(|| (at(item.t_ns), at(item.end_ns)), |g| (g.x0, g.x1));
        let instant = !matches!(item.kind, RectKind::Range | RectKind::GpuPass);
        // ADR-9601: an item on the window's edge maps to the edge only up to rounding (the last
        // event is `span * (width / span)`, one ulp past `width` for about 1 span in 16), so the
        // cull allows `EDGE_SLACK` and an instant is placed back on the edge.
        if x1 < -EDGE_SLACK || x0 > f64::from(view.width) + EDGE_SLACK {
            continue;
        }
        let (x, width) = if instant {
            (x0.clamp(0.0, f64::from(view.width)) as f32, 0.0)
        } else {
            let left = x0.max(0.0);
            let right = x1.min(f64::from(view.width));
            // Widen a sliver so it stays visible, then slide it back inside the view. Applying
            // the minimum width after the clip used to push a right-edge remnant past `width`.
            let limit = view.width.max(0.0);
            let mut width = ((right - left) as f32).max(MIN_BAR_WIDTH);
            if width > limit {
                width = limit;
            }
            let mut x = left as f32;
            if x + width > limit {
                x = (limit - width).max(0.0);
            }
            (x, width)
        };
        let Some(lane) = lanes.iter().find(|l| l.name == item.lane) else {
            continue;
        };
        out.push(TraceRect {
            kind: item.kind,
            name: item.name,
            lane: item.lane,
            row: lane.first_row + item.depth as usize,
            depth: item.depth,
            x,
            width,
            frame: item.frame,
            t_ns: item.t_ns,
            end_ns: group.map_or(item.end_ns, |g| g.t1),
            forced: item.forced,
            file: item.file,
            count: group.map_or(1, |g| g.count),
        });
    }
    out
}

/// What the window keeps per (trace revision, view): the bars and the lanes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceLayout {
    /// [`trace_layout`] of the view.
    pub rects: Vec<TraceRect>,
    /// [`trace_lanes`] of the trace.
    pub lanes: Vec<LaneRows>,
}

/// The cache key of a trace layout: the trace's revision and the view.
pub type TraceLayoutKey = (u64, (u32, u32, u32));

/// A trace read from disk, with a note when it is damaged.
#[derive(Clone, Debug)]
pub struct LoadedTrace {
    /// The decoded trace.
    pub trace: Arc<TraceFile>,
    /// The file ended inside a chunk; what came before it is shown.
    pub warning: Option<String>,
}

/// Decodes a `.regiatrace`; an empty or damaged file gives a clear message, never a panic.
///
/// # Errors
/// The text to show: empty file, not a trace, or a newer format version.
pub fn load_trace_bytes(bytes: &[u8]) -> Result<LoadedTrace, String> {
    if bytes.is_empty() {
        return Err("the file is empty (a trace that was never closed, or the wrong file)".to_owned());
    }
    let trace = read_trace_bytes(bytes).map_err(|e| e.to_string())?;
    let warning = trace.truncated.then(|| {
        format!(
            "the file ends inside a chunk (cut short or damaged); showing the {} events before it",
            trace.events.len()
        )
    });
    Ok(LoadedTrace {
        trace: Arc::new(trace),
        warning,
    })
}

/// Reads and decodes a trace file.
///
/// # Errors
/// The text to show; see [`load_trace_bytes`].
pub fn load_trace_file(path: &Path) -> Result<LoadedTrace, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    load_trace_bytes(&bytes)
}

/// The timeline's state (a resource; the Diagnostics window owns it).
#[derive(Resource, Clone, Default)]
pub struct TraceTimeline {
    /// The file shown.
    pub path: Option<PathBuf>,
    /// Its decoded trace.
    pub loaded: Option<LoadedTrace>,
    /// Zoom and pan (the width follows the window).
    pub view: TimelineView,
    /// The last status or error line.
    pub status: String,
    /// Screenshot textures by absolute path (`None` = could not be decoded).
    pub textures: HashMap<PathBuf, Option<egui::TextureHandle>>,
    /// Bumped when another trace is loaded (the layout cache's data revision).
    pub revision: u64,
    /// The layout of the current trace and view (ADR-7325).
    pub layout_cache: crate::timeline_geometry::LayoutCache<TraceLayoutKey, TraceLayout>,
    /// The focusable list of the items in view (ADR-7383).
    pub keys: crate::canvas_keys::ListKeys,
}

/// The row (and accessible) name of a timeline item: name, kind, lane (thread), frame and, for a
/// range or GPU pass, its duration.
#[must_use]
pub fn trace_row_name(rect: &TraceRect) -> String {
    match rect.kind {
        RectKind::Range | RectKind::GpuPass => format!(
            "{}, {} lane, frame {}, {:.3} ms{}",
            rect.name,
            rect.lane,
            rect.frame,
            (rect.end_ns - rect.t_ns) as f64 / 1.0e6,
            if rect.forced { ", forced close" } else { "" }
        ),
        RectKind::Mark => format!("{}, mark, frame {}", rect.name, rect.frame),
        RectKind::Screenshot => format!("{}, screenshot, frame {}", rect.name, rect.frame),
    }
}

impl std::fmt::Debug for TraceTimeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceTimeline")
            .field("path", &self.path)
            .field("loaded", &self.loaded.is_some())
            .field("view", &self.view)
            .field("status", &self.status)
            .field("textures", &self.textures.len())
            .field("revision", &self.revision)
            .field("layout_cache", &self.layout_cache)
            .finish()
    }
}

impl TraceTimeline {
    /// Loads `path` into the timeline and resets the view; on failure the old trace stays and
    /// `status` says why.
    ///
    /// # Errors
    /// The message (also in `status`).
    pub fn open(&mut self, path: &Path) -> Result<(), String> {
        match load_trace_file(path) {
            Ok(loaded) => {
                self.status = loaded.warning.clone().unwrap_or_default();
                self.path = Some(path.to_path_buf());
                self.loaded = Some(loaded);
                self.view = TimelineView::default();
                self.textures.clear();
                self.revision += 1;
                Ok(())
            }
            Err(e) => {
                self.status = e.clone();
                Err(e)
            }
        }
    }

    /// Zooms by `factor` keeping the centre of the window where it is.
    pub fn zoom_by(&mut self, factor: f32) {
        let v = self.view.clamped();
        let centre_visible = 0.5 / v.zoom;
        let zoom = (v.zoom * factor).clamp(1.0, MAX_ZOOM);
        // Fraction of the whole span at the window's centre, before and after.
        let centre = v.pan * (1.0 - 1.0 / v.zoom) + centre_visible;
        let denominator = 1.0 - 1.0 / zoom;
        let pan = if denominator > 0.0 {
            ((centre - 0.5 / zoom) / denominator).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.view = TimelineView { zoom, pan, ..v };
    }

    /// The layout in the current view.
    #[must_use]
    pub fn layout(&self) -> Vec<TraceRect> {
        self.loaded
            .as_ref()
            .map(|l| trace_layout(&l.trace, &self.view))
            .unwrap_or_default()
    }

    /// The layout and lanes in the current view, kept while the trace and the view are unchanged
    /// (a steady repaint is a cache hit, counted in [`Self::layout_cache`]). `None` without a trace.
    pub fn cached_layout(&mut self) -> Option<std::sync::Arc<TraceLayout>> {
        let trace = Arc::clone(&self.loaded.as_ref()?.trace);
        let view = self.view.clamped();
        let key = (self.revision, crate::timeline_geometry::view_key(&view));
        Some(self.layout_cache.get_or_build(key, || TraceLayout {
            rects: trace_layout(&trace, &view),
            lanes: trace_lanes(&trace),
        }))
    }
}

/// The result of one record action.
fn record_error(code: &str, message: String) -> String {
    format!("{code}: {message}")
}

/// Starts, stops or reads the process recorder through the registered `telemetry.trace` handler,
/// the one an agent calls (no second implementation).
///
/// # Errors
/// The handler's message, or a note that the agent extension is missing.
pub fn record(world: &mut World, action: &str, file: Option<&str>) -> Result<Value, String> {
    let handler = world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .and_then(|ext| ext.method("telemetry.trace"))
        .map(|(_, handler)| handler)
        .ok_or_else(|| {
            record_error(
                "no_recorder",
                "telemetry.trace is not registered in this app".to_owned(),
            )
        })?;
    let mut params = json!({ "action": action });
    if let (Some(file), Some(map)) = (file, params.as_object_mut()) {
        map.insert("file".into(), json!(file));
    }
    handler(world, &params).map_err(|e| {
        let reason = e
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("error");
        record_error(reason, e.message)
    })
}

/// A default file stem for a recording started from the button (unix seconds).
#[must_use]
pub fn default_record_name(now_unix_s: u64) -> String {
    format!("editor-{now_unix_s}")
}

/// Decodes a PNG into an egui image (`None` when it cannot be read).
fn load_shot(path: &Path) -> Option<egui::ColorImage> {
    let image = image::open(path).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()))
}

/// The fill of a CPU range bar at nesting `depth`. The depth is read from the file, so a damaged
/// trace can carry any `u32`: reduce it first so the hue step cannot overflow (`(d % 80) * 37 % 80`
/// equals `d * 37 % 80` for every depth that did not overflow).
fn range_colour(depth: u32) -> egui::Color32 {
    let step = ((depth % 80) * 37) % 80;
    egui::Color32::from_rgb(200, 150 + step as u8, 40)
}

/// Stops into `state`: opens the file the recorder just wrote and names it in `status`. A file
/// that cannot be loaded keeps its load error instead of a stale or empty line.
fn open_recorded(state: &mut TraceTimeline, file: &Path) {
    if let Err(e) = state.open(file) {
        state.status = e;
    } else {
        state.status = format!("recorded {}", file.display());
    }
}

/// Draws the timeline into `ui` (called by the Diagnostics window each frame it is shown).
pub fn show(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::info_span!("trace_timeline").entered();
    let mut state = world.get_resource::<TraceTimeline>().cloned().unwrap_or_default();
    let recording =
        regia::telemetry::trace::recorder().state() == regia::telemetry::trace::RecorderState::Writing;
    ui.horizontal(|ui| {
        let label = if recording { "Stop recording" } else { "Record" };
        if ui.button(label).clicked() {
            let result = if recording {
                record(world, "stop", None).map(|answer| answer["file"].as_str().map(PathBuf::from))
            } else {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                record(world, "start", Some(&default_record_name(stamp))).map(|_| None)
            };
            match result {
                Ok(Some(file)) => open_recorded(&mut state, &file),
                Ok(None) => {
                    state.status = if recording {
                        "stopped".into()
                    } else {
                        "recording".into()
                    }
                }
                Err(e) => state.status = e,
            }
        }
        if let Some(root) = world
            .get_resource::<crate::agent_ext::EditorProject>()
            .and_then(|p| p.root.clone())
            && ui.button("Open newest").clicked()
        {
            let found =
                newest_trace(&root.join("profiling")).or_else(|| newest_trace(Path::new("profiling")));
            match found {
                Some(path) => {
                    let _ = state.open(&path);
                }
                None => state.status = "no .regiatrace file in the project's profiling folder".into(),
            }
        }
        if crate::a11y_audit::name(ui.button("−"), egui::WidgetType::Button, "Zoom out").clicked() {
            state.zoom_by(0.5);
        }
        if crate::a11y_audit::name(ui.button("+"), egui::WidgetType::Button, "Zoom in").clicked() {
            state.zoom_by(2.0);
        }
        ui.label(format!("zoom {:.1}x", state.view.zoom));
        if recording {
            ui.colored_label(egui::Color32::LIGHT_RED, "● recording");
        }
    });
    if state.view.zoom > 1.0 {
        ui.add(egui::Slider::new(&mut state.view.pan, 0.0..=1.0).text("pan"));
    }
    if !state.status.is_empty() {
        ui.weak(&state.status);
    }
    if state.loaded.is_some() {
        state.view.width = ui.available_width().max(100.0);
    }
    let cached = state.cached_layout();
    if let (Some(cached), Some(loaded)) = (cached, state.loaded.clone()) {
        let (rects, lanes) = (&cached.rects, &cached.lanes);
        let rows: usize = lanes.iter().map(|l| l.rows).sum::<usize>().max(1);
        let height = rows as f32 * ROW_HEIGHT * 2.0;
        let (area, response) =
            ui.allocate_exact_size(egui::vec2(state.view.width, height), egui::Sense::hover());
        crate::canvas_keys::name_canvas(
            ui.ctx(),
            response.id,
            &format!("Trace timeline, {} items in view", rects.len()),
        );
        let painter = ui.painter_at(area);
        let row_h = ROW_HEIGHT * 2.0;
        let dir = state
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let hover = response.hover_pos();
        let mut hovered = None;
        for rect in rects {
            let top = area.min.y + rect.row as f32 * row_h;
            let left = area.min.x + rect.x;
            match rect.kind {
                RectKind::Mark => {
                    painter.line_segment(
                        [egui::pos2(left, area.min.y), egui::pos2(left, area.max.y)],
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 160, 240)),
                    );
                }
                RectKind::Range | RectKind::GpuPass => {
                    let bar = egui::Rect::from_min_size(
                        egui::pos2(left, top + 2.0),
                        egui::vec2(rect.width, row_h - 4.0),
                    );
                    let colour = if rect.kind == RectKind::GpuPass {
                        egui::Color32::from_rgb(120, 190, 110)
                    } else {
                        range_colour(rect.depth)
                    };
                    painter.rect_filled(bar, 2.0, colour);
                    if rect.width > 40.0 {
                        painter.text(
                            bar.left_center() + egui::vec2(3.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            rect.label(),
                            egui::FontId::monospace(11.0),
                            egui::Color32::BLACK,
                        );
                    }
                    if hover.is_some_and(|p| bar.contains(p)) {
                        hovered = Some(format!(
                            "{}\nlane {} · frame {} · {:.3} ms{}",
                            if rect.count > 1 {
                                format!("{} (first: {})", rect.label(), rect.name)
                            } else {
                                rect.name.clone()
                            },
                            rect.lane,
                            rect.frame,
                            (rect.end_ns - rect.t_ns) as f64 / 1.0e6,
                            if rect.forced { " (forced close)" } else { "" }
                        ));
                    }
                }
                RectKind::Screenshot => {
                    let thumb = egui::Rect::from_min_size(
                        egui::pos2(left, top + 1.0),
                        egui::vec2(row_h - 2.0, row_h - 2.0),
                    );
                    let path = dir.as_ref().zip(rect.file.as_ref()).map(|(d, f)| d.join(f));
                    let texture = path.as_ref().and_then(|p| {
                        state
                            .textures
                            .entry(p.clone())
                            .or_insert_with(|| {
                                load_shot(p).map(|img| {
                                    ui.ctx().load_texture(
                                        p.display().to_string(),
                                        img,
                                        egui::TextureOptions::LINEAR,
                                    )
                                })
                            })
                            .clone()
                    });
                    match texture {
                        Some(tex) => {
                            painter.image(
                                tex.id(),
                                thumb,
                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                egui::Color32::WHITE,
                            );
                        }
                        None => {
                            painter.rect_stroke(
                                thumb,
                                2.0,
                                egui::Stroke::new(1.0, egui::Color32::GRAY),
                                egui::StrokeKind::Inside,
                            );
                        }
                    }
                    if hover.is_some_and(|p| thumb.contains(p)) {
                        hovered = Some(format!("{} · frame {}", rect.name, rect.frame));
                    }
                }
            }
        }
        for lane in lanes {
            painter.text(
                area.min + egui::vec2(2.0, lane.first_row as f32 * row_h + 1.0),
                egui::Align2::LEFT_TOP,
                &lane.name,
                egui::FontId::monospace(10.0),
                egui::Color32::GRAY,
            );
        }
        if let Some(text) = hovered {
            let _ = response.on_hover_text(text);
        }
        ui.strong(format!("Items in view ({})", rects.len()));
        let name = |i: usize| rects.get(i).map(trace_row_name).unwrap_or_default();
        let current = state.keys.current;
        let _ = crate::canvas_keys::list_rows(
            ui,
            "trace-items",
            &mut state.keys,
            rects.len(),
            crate::canvas_keys::LIST_HEIGHT,
            current,
            &name,
        );
        ui.weak(format!(
            "{} events · {} items in view{}",
            loaded.trace.events.len(),
            rects.len(),
            if loaded.trace.header.adapter.is_empty() {
                String::new()
            } else {
                format!(" · {}", loaded.trace.header.adapter)
            }
        ));
    } else if state.status.is_empty() {
        ui.weak("No trace open. Record one, or open the newest in the project's profiling folder.");
    }
    world.insert_resource(state);
}

/// The newest `*.regiatrace` in `dir` (by modification time).
#[must_use]
pub fn newest_trace(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "regiatrace"))
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use regia::telemetry::trace::{Recorder, TraceConfig};

    use super::*;

    const MS: u64 = 1_000_000;

    /// Records: mark `boot` (frame 1), range `load` on main (10-30 ms) with `parse` inside it
    /// (12-18 ms), range `draw` on render (20-40 ms), mark `done` at 50 ms; returns the file bytes.
    fn scripted_bytes() -> Vec<u8> {
        let now = Arc::new(AtomicU64::new(1000 * MS));
        let probe = now.clone();
        let dir = regia_test_utils::unique_temp_dir("trace-timeline");
        let r = Recorder::with_clock(
            TraceConfig {
                dir: dir.clone(),
                ..TraceConfig::default()
            },
            Box::new(move || probe.load(Ordering::Relaxed)),
        );
        let tick = |ms: u64| now.fetch_add(ms * MS, Ordering::Relaxed);
        r.start(Some("t"), None).expect("start");
        r.set_frame(1);
        r.mark("boot", None);
        tick(10);
        let load = r.open_range("load", Lane::Main).id;
        tick(2);
        let parse = r.open_range("parse", Lane::Main).id;
        tick(6);
        let _ = r.close_range(parse);
        tick(2);
        r.set_frame(2);
        let draw = r.open_range("draw", Lane::Render).id;
        tick(0);
        let _ = r.close_range(load);
        tick(20);
        let _ = r.close_range(draw);
        tick(10);
        r.mark("done", None);
        r.stop().expect("stop");
        let bytes = std::fs::read(dir.join("t.regiatrace")).expect("file");
        let _ = std::fs::remove_dir_all(&dir);
        bytes
    }

    /// w11 item 2 (ADR-7325 kept): the window's draw path lays the trace out once per trace and
    /// width. Repaints of the same trace at the same width are cache hits, a new width builds once,
    /// and loading another trace (a new revision) builds once more.
    #[test]
    fn the_window_lays_out_once_per_trace_and_width() {
        use crate::canvas_keys::test_input::frame;
        let mut world = World::new();
        world.insert_resource(TraceTimeline {
            loaded: Some(load_trace_bytes(&scripted_bytes()).expect("loads")),
            ..TraceTimeline::default()
        });
        let ctx = egui::Context::default();
        let builds = |world: &World| world.resource::<TraceTimeline>().layout_cache.builds();
        for _ in 0..4 {
            frame(&ctx, [900.0, 700.0], vec![], |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| show(ui, &mut world));
            });
        }
        assert_eq!(
            builds(&world),
            1,
            "four repaints of one trace at one width lay out once"
        );
        for _ in 0..3 {
            frame(&ctx, [1200.0, 700.0], vec![], |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| show(ui, &mut world));
            });
        }
        assert_eq!(builds(&world), 2, "a new width lays out once");
        {
            let mut state = world.resource_mut::<TraceTimeline>();
            state.loaded = Some(load_trace_bytes(&scripted_bytes()).expect("loads"));
            state.revision += 1;
        }
        for _ in 0..3 {
            frame(&ctx, [1200.0, 700.0], vec![], |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| show(ui, &mut world));
            });
        }
        assert_eq!(builds(&world), 3, "another trace lays out once");
    }

    /// FU-5 (ADR-7383): the timeline is a named canvas and its item list names each range with
    /// its lane, frame and duration (span details no longer need a hover), keys only.
    #[test]
    fn the_item_list_names_the_spans_of_a_fixture_trace() {
        use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to};
        let mut world = World::new();
        let loaded = load_trace_bytes(&scripted_bytes()).expect("loads");
        world.insert_resource(TraceTimeline {
            loaded: Some(loaded),
            ..TraceTimeline::default()
        });
        let ctx = egui::Context::default();
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| show(ui, &mut world));
        };
        frame(&ctx, [900.0, 700.0], vec![], &mut draw);
        let update = frame(&ctx, [900.0, 700.0], vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        assert_eq!(
            labels_of(&update, egui::accesskit::Role::Canvas),
            ["Trace timeline, 5 items in view"]
        );
        let names: Vec<String> = crate::a11y_audit::widget_names(&update)
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        for want in [
            "boot, mark, frame 1",
            "load, main lane, frame 1, 10.000 ms",
            "parse, main lane, frame 1, 6.000 ms",
            "draw, render lane, frame 2, 20.000 ms",
            "done, mark, frame 2",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} not in {names:?}");
        }
        tab_to(&ctx, [900.0, 700.0], "boot, mark, frame 1", false, 20, &mut draw);
        frame(
            &ctx,
            [900.0, 700.0],
            vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)],
            &mut draw,
        );
        let down = frame(&ctx, [900.0, 700.0], vec![], &mut draw);
        assert!(
            focused_name(&down).is_some_and(|n| n != "boot, mark, frame 1"),
            "Down moved on"
        );
    }

    fn names(rects: &[TraceRect]) -> Vec<(String, String)> {
        rects.iter().map(|r| (r.name.clone(), r.lane.clone())).collect()
    }

    #[test]
    fn marks_ranges_and_lanes_lay_out_in_frame_order() {
        let loaded = load_trace_bytes(&scripted_bytes()).expect("loads");
        assert!(loaded.warning.is_none());
        let rects = trace_layout(&loaded.trace, &TimelineView::default());
        let by = |n: &str| rects.iter().find(|r| r.name == n).cloned().expect(n);
        assert_eq!(by("boot").kind, RectKind::Mark);
        assert_eq!(by("boot").width, 0.0);
        assert_eq!(by("load").kind, RectKind::Range);
        // Frame order: frame 1 items before frame 2 items.
        let frames: Vec<u64> = rects.iter().map(|r| r.frame).collect();
        assert!(frames.windows(2).all(|w| w[0] <= w[1]), "{frames:?}");
        assert_eq!(rects.first().map(|r| r.name.as_str()), Some("boot"));
        // Lanes: main has the nested range one row lower; render sits below main.
        assert_eq!(
            (by("load").lane.as_str(), by("load").row, by("load").depth),
            ("main", 0, 0)
        );
        assert_eq!(
            (by("parse").lane.as_str(), by("parse").row, by("parse").depth),
            ("main", 1, 1)
        );
        assert_eq!((by("draw").lane.as_str(), by("draw").row), ("render", 2));
        let lanes = trace_lanes(&loaded.trace);
        assert_eq!(
            lanes
                .iter()
                .map(|l| (l.name.as_str(), l.rows))
                .collect::<Vec<_>>(),
            [("main", 2), ("render", 1)]
        );
        // The inner range sits inside the outer one.
        let (load, parse) = (by("load"), by("parse"));
        assert!(parse.x >= load.x && parse.x + parse.width <= load.x + load.width);
        // The whole trace fits the view.
        assert!(rects.iter().all(|r| r.x >= 0.0 && r.x + r.width <= 800.0 + 0.01));
        assert_eq!(
            trace_layout(&loaded.trace, &TimelineView::default()),
            rects,
            "deterministic"
        );
    }

    /// ADR-9601: the last event of a trace sits on the view's right edge. Mapped to pixels as
    /// `span * (width / span)`, about 1 span in 16 rounds one ulp past `width` (11 ns, 22 ns and
    /// 39 ns do at 800 points), and the edge cull used to drop it: `editor_diagnostics` lost its
    /// `done` mark on a card run. Every span here keeps both marks, the last one at `width`.
    #[test]
    fn the_last_mark_stays_on_the_right_edge_for_every_span() {
        for span_ns in [11u64, 22, 39, 44, 78, 1_000, 49_999_917] {
            let now = Arc::new(AtomicU64::new(1000 * MS));
            let probe = now.clone();
            let dir = regia_test_utils::unique_temp_dir("trace-timeline-edge");
            let r = Recorder::with_clock(
                TraceConfig {
                    dir: dir.clone(),
                    ..TraceConfig::default()
                },
                Box::new(move || probe.load(Ordering::Relaxed)),
            );
            r.start(Some("t"), None).expect("start");
            r.set_frame(1);
            r.mark("first", None);
            now.fetch_add(span_ns, Ordering::Relaxed);
            r.mark("last", None);
            r.stop().expect("stop");
            let bytes = std::fs::read(dir.join("t.regiatrace")).expect("file");
            let _ = std::fs::remove_dir_all(&dir);
            let loaded = load_trace_bytes(&bytes).expect("loads");
            let rects = trace_layout(&loaded.trace, &TimelineView::default());
            let names: Vec<&str> = rects.iter().map(|r| r.name.as_str()).collect();
            assert_eq!(names, ["first", "last"], "span {span_ns} ns");
            assert_eq!(rects[0].x, 0.0, "span {span_ns} ns");
            assert_eq!(rects[1].x, 800.0, "span {span_ns} ns");
        }
    }

    #[test]
    fn zooming_in_clips_and_culls_and_panning_moves_the_window() {
        let loaded = load_trace_bytes(&scripted_bytes()).expect("loads");
        let whole = trace_layout(&loaded.trace, &TimelineView::default());
        let width_of = |rects: &[TraceRect], n: &str| rects.iter().find(|r| r.name == n).map(|r| r.width);
        // 4x zoom at the start: only the first quarter of the span (0-12.5 ms) is in view.
        let start = trace_layout(
            &loaded.trace,
            &TimelineView {
                zoom: 4.0,
                pan: 0.0,
                width: 800.0,
            },
        );
        assert!(names(&start).iter().any(|(n, _)| n == "boot"));
        assert!(
            names(&start).iter().all(|(n, _)| n != "done" && n != "draw"),
            "culled: {:?}",
            names(&start)
        );
        // At 2x the first half (0-25 ms) fills the view, so `load` (10-30 ms) is wider than in the whole.
        let half = trace_layout(
            &loaded.trace,
            &TimelineView {
                zoom: 2.0,
                pan: 0.0,
                width: 800.0,
            },
        );
        assert!(
            width_of(&half, "load").expect("load in view") > width_of(&whole, "load").expect("load whole"),
            "zoomed bars are wider"
        );
        // Panned to the end the window shows the tail instead.
        let end = trace_layout(
            &loaded.trace,
            &TimelineView {
                zoom: 4.0,
                pan: 1.0,
                width: 800.0,
            },
        );
        assert!(names(&end).iter().any(|(n, _)| n == "done"));
        assert!(names(&end).iter().all(|(n, _)| n != "boot"));
        // A clipped bar never leaves the view.
        assert!(
            start
                .iter()
                .chain(&end)
                .all(|r| r.x >= 0.0 && r.x + r.width <= 800.0 + 0.01)
        );
        // Out-of-range values clamp instead of misbehaving.
        let wild = trace_layout(
            &loaded.trace,
            &TimelineView {
                zoom: f32::NAN,
                pan: -3.0,
                width: 800.0,
            },
        );
        assert_eq!(wild, whole);
    }

    #[test]
    fn zoom_by_keeps_the_window_centre() {
        let mut t = TraceTimeline::default();
        t.zoom_by(4.0);
        assert!((t.view.zoom - 4.0).abs() < 1e-6);
        // Centre of the whole span stays the centre: pan 0.5.
        assert!((t.view.pan - 0.5).abs() < 1e-5, "{}", t.view.pan);
        t.zoom_by(0.01);
        assert_eq!(t.view.zoom, 1.0);
        assert_eq!(t.view.pan, 0.0);
    }

    #[test]
    fn an_empty_trace_lays_out_nothing_and_bad_files_say_why() {
        let empty = TraceFile::default();
        assert!(trace_layout(&empty, &TimelineView::default()).is_empty());
        assert!(trace_lanes(&empty).is_empty());
        let message = load_trace_bytes(&[]).expect_err("empty");
        assert!(message.contains("empty"), "{message}");
        let message = load_trace_bytes(b"this is not a trace").expect_err("garbage");
        assert!(message.contains("not a regiatrace"), "{message}");
        let message = load_trace_bytes(b"RG").expect_err("short");
        assert!(!message.is_empty());
        let mut newer = scripted_bytes();
        newer[4] = 99; // the version field follows the magic
        // Either rejected as newer or still readable; never a panic.
        let _ = load_trace_bytes(&newer);
    }

    #[test]
    fn a_truncated_or_damaged_file_loads_what_it_can_with_a_warning() {
        let bytes = scripted_bytes();
        let cut = &bytes[..bytes.len() - 7];
        match load_trace_bytes(cut) {
            Ok(loaded) => {
                assert!(loaded.warning.is_some(), "a cut file warns");
                let _ = trace_layout(&loaded.trace, &TimelineView::default());
            }
            Err(message) => assert!(!message.is_empty()),
        }
        // Every prefix and every single-byte corruption is survivable.
        for end in 0..bytes.len() {
            if let Ok(loaded) = load_trace_bytes(&bytes[..end]) {
                let _ = trace_layout(&loaded.trace, &TimelineView::default());
            }
        }
        for at in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[at] ^= 0xFF;
            if let Ok(loaded) = load_trace_bytes(&damaged) {
                let _ = trace_layout(
                    &loaded.trace,
                    &TimelineView {
                        zoom: 8.0,
                        pan: 0.5,
                        width: 640.0,
                    },
                );
            }
        }
    }

    #[test]
    fn stopping_into_a_file_says_recorded_and_a_failed_open_says_why() {
        let dir = regia_test_utils::unique_temp_dir("trace-timeline-recorded");
        std::fs::write(dir.join("a.regiatrace"), scripted_bytes()).expect("write");
        let mut t = TraceTimeline::default();
        open_recorded(&mut t, &dir.join("a.regiatrace"));
        assert!(
            t.status.starts_with("recorded "),
            "the stop names the file it wrote, not the load warning or an empty line: {:?}",
            t.status
        );
        // A file that cannot be loaded keeps the load error, not a stale "recorded".
        open_recorded(&mut t, &dir.join("missing.regiatrace"));
        assert!(
            !t.status.starts_with("recorded"),
            "a failed open says why: {:?}",
            t.status
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_replaces_the_view_and_keeps_the_old_trace_on_failure() {
        let dir = regia_test_utils::unique_temp_dir("trace-timeline-open");
        std::fs::write(dir.join("a.regiatrace"), scripted_bytes()).expect("write");
        std::fs::write(dir.join("empty.regiatrace"), []).expect("write");
        let mut t = TraceTimeline::default();
        t.zoom_by(2.0);
        t.open(&dir.join("a.regiatrace")).expect("opens");
        assert_eq!(t.view.zoom, 1.0, "opening resets the view");
        assert!(!t.layout().is_empty());
        let message = t.open(&dir.join("empty.regiatrace")).expect_err("empty");
        assert_eq!(t.status, message);
        assert!(!t.layout().is_empty(), "the old trace stays");
        assert!(t.open(&dir.join("missing.regiatrace")).is_err());
        assert_eq!(newest_trace(&dir.join("nowhere")), None);
        assert!(newest_trace(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn screenshots_sit_on_their_own_lane_at_the_top() {
        let mut trace = load_trace_bytes(&scripted_bytes())
            .expect("loads")
            .trace
            .as_ref()
            .clone();
        trace.events.push(regia::telemetry::trace::Event {
            t_ns: trace.events.last().map_or(0, |e| e.t_ns),
            lane: Lane::Main,
            kind: EventKind::Screenshot {
                name: Some("door".into()),
                frame: 2,
                width: 64,
                height: 64,
                file: "t.shots/1.png".into(),
            },
        });
        let lanes = trace_lanes(&trace);
        assert_eq!(lanes.first().map(|l| l.name.as_str()), Some(SHOT_LANE));
        let rects = trace_layout(&trace, &TimelineView::default());
        let shot = rects
            .iter()
            .find(|r| r.kind == RectKind::Screenshot)
            .expect("shot");
        assert_eq!(
            (shot.row, shot.frame, shot.file.as_deref()),
            (0, 2, Some("t.shots/1.png"))
        );
        let main = rects.iter().find(|r| r.name == "load").expect("load");
        assert!(main.row > shot.row);
    }

    #[test]
    fn record_without_the_extension_says_so() {
        let mut world = World::new();
        let message = record(&mut world, "status", None).expect_err("not registered");
        assert!(message.starts_with("no_recorder"), "{message}");
    }

    /// A range whose visible remnant is thinner than [`MIN_BAR_WIDTH`] still has to stay inside
    /// the view. Widening it after the clip used to push the right edge past `view.width`.
    #[test]
    fn a_right_edge_sliver_stays_inside_the_view() {
        let trace = TraceFile {
            events: vec![
                regia::telemetry::trace::Event {
                    t_ns: 0,
                    lane: Lane::Main,
                    kind: EventKind::Mark {
                        name: "start".into(),
                        data: None,
                        frame: 1,
                    },
                },
                regia::telemetry::trace::Event {
                    t_ns: 990,
                    lane: Lane::Main,
                    kind: EventKind::RangeOpen {
                        id: 1,
                        name: "thin".into(),
                        frame: 1,
                        depth: 0,
                        parent: None,
                    },
                },
                regia::telemetry::trace::Event {
                    t_ns: 1000,
                    lane: Lane::Main,
                    kind: EventKind::RangeClose {
                        id: 1,
                        frame: 1,
                        forced: false,
                    },
                },
            ],
            ..TraceFile::default()
        };
        let view = TimelineView {
            zoom: 1.0,
            pan: 0.0,
            width: 100.0,
        };
        let rects = trace_layout(&trace, &view);
        let thin = rects.iter().find(|r| r.name == "thin").expect("thin bar");
        assert!(thin.width > 0.0, "the sliver stays visible");
        assert!(
            thin.x >= 0.0 && thin.x + thin.width <= view.width + 0.01,
            "x {} width {} leaves the {} pt view",
            thin.x,
            thin.width,
            view.width
        );
    }

    /// A GPU interval is logged when it finishes, so `t_ns` is after `end_ns`. The window has to
    /// include `begin_ns`, or the bar sits entirely left of the view and is culled.
    #[test]
    fn a_gpu_interval_logged_after_it_ran_stays_in_the_window() {
        let trace = TraceFile {
            events: vec![regia::telemetry::trace::Event {
                t_ns: 2_000_000,
                lane: Lane::Gpu,
                kind: EventKind::GpuInterval {
                    tag: 1,
                    name: "shadow".into(),
                    begin_ns: 100_000,
                    end_ns: 1_100_000,
                    frame: 1,
                },
            }],
            ..TraceFile::default()
        };
        let view = TimelineView {
            zoom: 1.0,
            pan: 0.0,
            width: 800.0,
        };
        let rects = trace_layout(&trace, &view);
        let bar = rects
            .iter()
            .find(|r| r.name == "shadow")
            .expect("gpu interval is in the window");
        assert_eq!(bar.kind, RectKind::GpuPass);
        assert!(bar.width > 0.0, "width {}", bar.width);
        assert!(
            bar.x >= 0.0 && bar.x + bar.width <= view.width + 0.01,
            "x {} width {} leaves the view",
            bar.x,
            bar.width
        );
    }

    #[test]
    fn a_damaged_range_depth_still_gets_a_colour() {
        // The depth comes from the file (a varint), so a damaged trace can name any u32; the old
        // `depth * 37` overflowed (a panic in debug builds) for depths past 116 million.
        for depth in [116_076_204, 200_000_000, u32::MAX] {
            let colour = range_colour(depth);
            assert_eq!(colour.r(), 200);
            assert!((150..230).contains(&colour.g()), "{colour:?}");
        }
        // Small depths keep the colours they always had.
        for depth in 0..400u32 {
            let expected = 150 + ((depth * 37) % 80) as u8;
            assert_eq!(range_colour(depth).g(), expected, "depth {depth}");
        }
    }

    /// ADR-7325 acceptance: 100 000 ranges (two depths on main, a third of them on render) lay out
    /// with at most one bar per pixel column per row and every range counted in exactly one bar;
    /// the window's second identical layout call is a cache hit, a new view builds again.
    #[test]
    fn a_hundred_thousand_ranges_lay_out_one_bar_per_pixel_column_and_cache() {
        let mut events = Vec::new();
        let range = |events: &mut Vec<regia::telemetry::trace::Event>,
                     id: u64,
                     lane: Lane,
                     depth: u32,
                     t: u64,
                     end: u64| {
            events.push(regia::telemetry::trace::Event {
                t_ns: t,
                lane,
                kind: EventKind::RangeOpen {
                    id,
                    name: format!("r{depth}"),
                    frame: 1,
                    depth,
                    parent: None,
                },
            });
            events.push(regia::telemetry::trace::Event {
                t_ns: end,
                lane,
                kind: EventKind::RangeClose {
                    id,
                    frame: 1,
                    forced: false,
                },
            });
        };
        let mut id = 0;
        for i in 0..50_000u64 {
            let t = i * 2_000;
            let lane = if i % 3 == 0 { Lane::Render } else { Lane::Main };
            id += 1;
            range(&mut events, id, lane, 0, t, t + 1_500);
            id += 1;
            range(&mut events, id, lane, 1, t + 100, t + 900);
        }
        let trace = TraceFile {
            events,
            ..TraceFile::default()
        };
        let view = TimelineView {
            zoom: 1.0,
            pan: 0.0,
            width: 800.0,
        };
        let rects = trace_layout(&trace, &view);
        let total: u64 = rects.iter().map(|r| u64::from(r.count)).sum();
        assert_eq!(total, 100_000, "every range is in exactly one bar");
        let mut per_row: std::collections::BTreeMap<usize, Vec<i64>> = std::collections::BTreeMap::new();
        for r in &rects {
            per_row.entry(r.row).or_default().push(r.x.floor() as i64);
        }
        assert_eq!(per_row.len(), 4, "main and render, two depths each");
        for (row, mut columns) in per_row {
            let n = columns.len();
            assert!(n <= 800, "row {row}: {n} bars for 800 columns");
            columns.sort_unstable();
            columns.dedup();
            assert_eq!(columns.len(), n, "row {row}: two bars start in one pixel column");
        }
        assert!(
            rects
                .iter()
                .any(|r| r.count > 1 && r.label().ends_with(" ranges"))
        );
        // Zoomed far in, nothing is merged.
        let near = trace_layout(
            &trace,
            &TimelineView {
                zoom: MAX_ZOOM,
                pan: 0.0,
                width: 800.0,
            },
        );
        assert!(!near.is_empty() && near.iter().all(|r| r.count == 1));

        let mut state = TraceTimeline {
            loaded: Some(LoadedTrace {
                trace: Arc::new(trace),
                warning: None,
            }),
            view,
            ..TraceTimeline::default()
        };
        let first = state.cached_layout().expect("layout");
        let second = state.cached_layout().expect("layout");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            (state.layout_cache.builds(), state.layout_cache.hits()),
            (1, 1),
            "a steady repaint is a cache hit"
        );
        assert_eq!(first.rects, rects);
        state.zoom_by(2.0);
        let _ = state.cached_layout();
        assert_eq!(state.layout_cache.builds(), 2, "a new view builds");
        assert!(TraceTimeline::default().cached_layout().is_none());
    }
}
