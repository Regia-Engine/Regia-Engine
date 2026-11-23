// SPDX-License-Identifier: MPL-2.0
//! The Profiler panel (`docs/specs/83-prefab-profiler-volumes.md` § 4.3, ADR-2323, ADR-9740): a
//! floating window (View → Profiler) with a source picker (edit world, play, a separate-process
//! PIE child or a dev build, [`crate::profiler_source`]), the frame time, and six tabs: a flame
//! view of the span call tree with the GPU passes; the per-thread Timeline of one frame
//! ([`crate::thread_timeline`], ADR-7325); the 300-frame history with its hitches (click a frame to
//! freeze its spans); entity counts; memory per subsystem, owner, GPU allocation and asset type;
//! and Compare, two opened captures diffed per span with the flame colored by the change. Capture writes `captures/<time>.rprof.json` under the project; Open reads one back. The
//! agent twins are `profiler.*` and `editor.profiler_*` (`book/agents/profiler.md`).
//!
//! The flame view's geometry is [`flame_layout`](crate::profiler_panel::flame_layout), a pure
//! function of the capture, so it is tested without a GPU; the window only paints its rectangles.
//! The panel reads its source once per repaint (egui may run a window's closure twice in one
//! frame), inside the one `profiler_panel` span.

use regia::telemetry::profile_capture::{CaptureNode, DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES, ProfileCapture};
use regia_ecs::prelude::*;

use crate::profiler_source::{
    LinkState, ProfilerSource, ProfilerTab, ProfilerView, SourceData, TimelineData,
};

/// Narrowest bar drawn, points; thinner spans are left out (their parent's width covers them).
pub const MIN_BAR_WIDTH: f32 = 2.0;

/// Height of one flame row, points.
pub const ROW_HEIGHT: f32 = 18.0;

/// Frames the panel averages.
pub const PANEL_FRAMES: usize = 60;

/// The panel's state (a resource; absent = closed).
#[derive(Resource, Clone, Debug, Default)]
pub struct ProfilerPanel {
    /// The window is shown.
    pub open: bool,
    /// The last status line (capture written, errors).
    pub status: String,
    /// The focusable span list beside the flame view (ADR-7383).
    pub keys: crate::canvas_keys::ListKeys,
}

/// One span of the capture's call tree, as the span list shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanRow {
    /// Span name.
    pub name: String,
    /// The parent span's name (`None` for a root).
    pub parent: Option<String>,
    /// Depth in the call tree, 0 = roots.
    pub depth: usize,
    /// Mean inclusive time, ms.
    pub inclusive_ms: f64,
    /// Mean self time, ms.
    pub self_ms: f64,
    /// Mean calls per frame.
    pub calls: f64,
}

/// Every span of the capture's CPU tree, depth first (parents before their children), including
/// the spans too thin for a flame bar.
#[must_use]
pub fn span_rows(capture: &ProfileCapture) -> Vec<SpanRow> {
    fn walk(node: &CaptureNode, parent: Option<&str>, depth: usize, out: &mut Vec<SpanRow>) {
        out.push(SpanRow {
            name: node.name.clone(),
            parent: parent.map(str::to_string),
            depth,
            inclusive_ms: node.inclusive_ms,
            self_ms: node.self_ms,
            calls: node.calls,
        });
        for child in &node.children {
            walk(child, Some(&node.name), depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for root in &capture.cpu {
        walk(root, None, 0, &mut out);
    }
    out
}

/// The row (and accessible) name of a span: name, parent, inclusive and self ms, calls. The frame
/// profiler's call tree is the main thread's.
#[must_use]
pub fn span_row_name(row: &SpanRow) -> String {
    let parent = row
        .parent
        .as_ref()
        .map_or_else(String::new, |p| format!(" in {p}"));
    format!(
        "{}{parent}, main thread, {:.3} ms inclusive, {:.3} ms self, {:.1} calls",
        row.name, row.inclusive_ms, row.self_ms, row.calls
    )
}

/// [`span_row_name`] in percent mode (P-304): the inclusive and self times as percentages of
/// `frame_ms` (`crate::profiler_modules::percent_base_ms`) instead of ms.
#[must_use]
pub fn span_row_name_in(row: &SpanRow, frame_ms: f64, percent: bool) -> String {
    if !percent {
        return span_row_name(row);
    }
    let parent = row
        .parent
        .as_ref()
        .map_or_else(String::new, |p| format!(" in {p}"));
    format!(
        "{}{parent}, main thread, {:.1} % of frame inclusive, {:.1} % self, {:.1} calls",
        row.name,
        crate::profiler_modules::percent_of(row.inclusive_ms, frame_ms),
        crate::profiler_modules::percent_of(row.self_ms, frame_ms),
        row.calls
    )
}

/// The flame view of `capture` and its span list (the list carries what hovering a bar shows), with
/// times as percent of frame when `percent` (P-304).
pub(crate) fn flame_ui_in(
    ui: &mut egui::Ui,
    capture: &ProfileCapture,
    keys: &mut crate::canvas_keys::ListKeys,
    percent: bool,
) {
    let base = crate::profiler_modules::percent_base_ms(capture);
    draw_flame_in(ui, capture, None, percent.then_some(base));
    let spans = span_rows(capture);
    ui.strong(format!("Spans ({})", spans.len()));
    let name = |i: usize| {
        spans
            .get(i)
            .map(|row| span_row_name_in(row, base, percent))
            .unwrap_or_default()
    };
    let current = keys.current;
    let _ = crate::canvas_keys::list_rows(
        ui,
        "profiler-spans",
        keys,
        spans.len(),
        crate::canvas_keys::LIST_HEIGHT,
        current,
        &name,
    );
}

/// One bar of the flame view.
#[derive(Clone, Debug, PartialEq)]
pub struct FlameRect {
    /// Span name.
    pub name: String,
    /// The names from the root to this span joined with `;` (the key Compare colors by).
    pub path: String,
    /// Depth (row) from the top, 0 = roots.
    pub depth: usize,
    /// Left edge, points from the view's left.
    pub x: f32,
    /// Width, points.
    pub width: f32,
    /// Mean inclusive time, ms.
    pub inclusive_ms: f64,
    /// Mean self time, ms.
    pub self_ms: f64,
    /// Mean calls per frame.
    pub calls: f64,
}

/// Lays the capture's CPU tree out in a view `width` points wide: roots side by side across the
/// whole width in proportion to their inclusive time, each child inside its parent from the
/// parent's left edge, one row per depth. Bars narrower than `min_width` are dropped with their
/// subtrees. Deterministic for a given capture.
#[must_use]
pub fn flame_layout(capture: &ProfileCapture, width: f32, min_width: f32) -> Vec<FlameRect> {
    let total: f64 = capture.cpu.iter().map(|n| n.inclusive_ms).sum();
    let mut out = Vec::new();
    if !(total > 0.0 && width > 0.0) {
        return out;
    }
    let scale = f64::from(width) / total;
    let mut x = 0.0_f64;
    for root in &capture.cpu {
        place(root, "", 0, x, scale, min_width, &mut out);
        x += root.inclusive_ms * scale;
    }
    out
}

fn place(
    node: &CaptureNode,
    parent: &str,
    depth: usize,
    x: f64,
    scale: f64,
    min_width: f32,
    out: &mut Vec<FlameRect>,
) {
    let width = (node.inclusive_ms * scale) as f32;
    if width < min_width {
        return;
    }
    let path = if parent.is_empty() {
        node.name.clone()
    } else {
        format!("{parent};{}", node.name)
    };
    out.push(FlameRect {
        name: node.name.clone(),
        path: path.clone(),
        depth,
        x: x as f32,
        width,
        inclusive_ms: node.inclusive_ms,
        self_ms: node.self_ms,
        calls: node.calls,
    });
    let mut child_x = x;
    for child in &node.children {
        place(child, &path, depth + 1, child_x, scale, min_width, out);
        child_x += child.inclusive_ms * scale;
    }
}

/// The capture the panel shows: the process profiler over [`PANEL_FRAMES`] frames.
#[must_use]
pub fn current_capture(world: &World) -> Option<ProfileCapture> {
    let profiler = world.get_resource::<regia::Profiler>()?;
    let graph = profiler.0.flame_graph(PANEL_FRAMES);
    let gpu: Vec<(&'static str, f32)> = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map(|r| r.0.gpu_timings())
        .filter(|g| g.frame_ms > 0.0)
        .map(|g| g.passes().to_vec())
        .unwrap_or_default();
    Some(ProfileCapture::from_flame(
        &graph,
        &gpu,
        DEFAULT_MAX_DEPTH,
        DEFAULT_MAX_NODES,
    ))
}

/// Writes the current capture to `captures/<unix seconds>.rprof.json` under the project.
///
/// # Errors
///
/// No project, no profiler, or the file cannot be written.
pub fn capture_to_project(world: &World) -> Result<std::path::PathBuf, String> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
        .ok_or("no project is open")?;
    let capture = current_capture(world).ok_or("no frame profiler is installed")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = root.join("captures").join(format!("{stamp}.rprof.json"));
    capture.write(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Frames between two in-process `memory.report` reads of the Memory tab.
pub const MEMORY_EVERY_FRAMES: u64 = 30;

/// Height of the history graph, points.
pub const HISTORY_HEIGHT: f32 = 80.0;

/// Reads the source for this repaint: once per egui frame (a second pass of the window in the same
/// frame reuses the data), counted in [`ProfilerView::layout_builds`].
pub fn frame_data(world: &mut World, frame: u64) -> SourceData {
    if let Some((at, data)) = world
        .get_resource::<ProfilerView>()
        .and_then(|v| v.frame_data.clone())
        && at == frame
    {
        return data;
    }
    let memory = world
        .get_resource::<ProfilerView>()
        .is_some_and(|v| v.tab == ProfilerTab::Memory || v.tab == ProfilerTab::Entities);
    let data = crate::profiler_source::read_source(world, frame, memory, MEMORY_EVERY_FRAMES);
    let mut view = world.get_resource_or_insert_with(ProfilerView::default);
    view.frame_data = Some((frame, data.clone()));
    view.layout_builds += 1;
    data
}

/// Reads the Timeline tab's frame once per egui frame (a second pass of the window reuses it).
pub fn timeline_data(world: &mut World, frame: u64) -> TimelineData {
    if let Some((at, data)) = world
        .get_resource::<ProfilerView>()
        .and_then(|v| v.timeline_data.clone())
        && at == frame
    {
        return data;
    }
    let data = crate::profiler_source::read_timeline(world);
    world
        .get_resource_or_insert_with(ProfilerView::default)
        .timeline_data = Some((frame, data.clone()));
    data
}

/// Fill of a timeline bar: by depth, merged runs gray.
fn timeline_fill(rect: &crate::thread_timeline::ThreadRect) -> egui::Color32 {
    if rect.count > 1 {
        return egui::Color32::from_gray(150);
    }
    let shade = 90 + ((usize::from(rect.depth) * 37) % 120) as u8;
    egui::Color32::from_rgb(70, shade, 210)
}

fn timeline_tab(ui: &mut egui::Ui, world: &mut World, frame_nr: u64) {
    use crate::thread_timeline::{ROW_HEIGHT, cached_layout, frame_at_offset, rect_at};
    use crate::timeline_geometry::{pan_by, zoom_about};
    let data = timeline_data(world, frame_nr);
    let local = matches!(data.source, "edit" | "pie");
    ui.horizontal(|ui| {
        if local && let Some(profiler) = world.get_resource::<regia::Profiler>().map(|p| p.0.clone()) {
            let mut on = profiler.timeline_enabled();
            // UI site: agent twin `profiler.set_capture { timeline }`.
            if ui.checkbox(&mut on, "Record timeline").changed() {
                profiler.set_timeline_enabled(on);
            }
        }
        let held = world
            .get_resource::<ProfilerView>()
            .and_then(|v| v.timeline_frame);
        let shown = data.frame.as_ref().map(|f| f.number);
        // View-only controls; `editor.profiler_timeline` reads the same layout.
        if ui.small_button("◀").clicked()
            && let Some(n) = held.or(shown)
        {
            world
                .get_resource_or_insert_with(ProfilerView::default)
                .timeline_frame = Some(n.saturating_sub(1));
        }
        if ui.small_button("▶").clicked()
            && let Some(n) = held.or(shown)
        {
            world
                .get_resource_or_insert_with(ProfilerView::default)
                .timeline_frame = Some(n + 1);
        }
        if held.is_some() && ui.small_button("Live").clicked() {
            world
                .get_resource_or_insert_with(ProfilerView::default)
                .timeline_frame = None;
        }
        if ui.small_button("Reset view").clicked() {
            let mut view = world.get_resource_or_insert_with(ProfilerView::default);
            view.timeline_view.zoom = 1.0;
            view.timeline_view.pan = 0.0;
        }
        // The strip's frame count (ADR-7409): N consecutive frames on one axis.
        let mut frames = world
            .get_resource::<ProfilerView>()
            .map_or(1, |v| v.strip_frames());
        // UI site: agent twin `editor.profiler_timeline { frames }`.
        let picked = egui::DragValue::new(&mut frames)
            .range(1..=crate::profiler_source::MAX_TIMELINE_FRAMES)
            .prefix("x");
        if ui
            .add(picked)
            .on_hover_text("Consecutive frames on one time axis")
            .changed()
        {
            let mut view = world.get_resource_or_insert_with(ProfilerView::default);
            view.timeline_frames = frames;
            view.timeline_data = None;
        }
        let _ = crate::profiler_modules::percent_toggle(ui, world);
        ui.weak(match (held, shown) {
            (Some(n), _) => format!("frame {n} (held)"),
            (None, Some(n)) => format!("frame {n} (live)"),
            (None, None) => String::new(),
        });
    });
    let Some(frame) = data.frame else {
        ui.label(match &data.message {
            Some(m) => format!("{}: {m}", data.status),
            None => data.status.clone(),
        });
        return;
    };
    let width = ui.available_width().max(100.0);
    let layout = {
        let mut view = world.get_resource_or_insert_with(ProfilerView::default);
        view.timeline_view.width = width;
        let v = view.timeline_view;
        cached_layout(&mut view.timeline_layout, data.source, &frame, &v)
    };
    ui.weak(format!(
        "{:.3} ms · {} spans on {} threads · {} bars{}",
        frame.frame_ns as f64 / 1e6,
        frame.span_count(),
        layout.threads.len(),
        layout.rects.len(),
        if frame.dropped > 0 {
            format!(" · {} dropped at the cap", frame.dropped)
        } else {
            String::new()
        }
    ));
    let height = layout.rows.max(1) as f32 * ROW_HEIGHT;
    let (area, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());
    let painter = ui.painter_at(area);
    for rect in &layout.rects {
        let bar = egui::Rect::from_min_size(
            area.min + egui::vec2(rect.x, rect.row as f32 * ROW_HEIGHT),
            egui::vec2(rect.width.max(1.0), ROW_HEIGHT - 1.0),
        );
        painter.rect_filled(bar, 1.0, timeline_fill(rect));
        if rect.width > 40.0 {
            painter.text(
                bar.left_center() + egui::vec2(2.0, 0.0),
                egui::Align2::LEFT_CENTER,
                rect.label(),
                egui::FontId::monospace(10.0),
                egui::Color32::BLACK,
            );
        }
    }
    for (i, thread) in layout.threads.iter().enumerate() {
        let y = area.min.y + thread.first_row as f32 * ROW_HEIGHT;
        if i > 0 {
            painter.hline(
                area.x_range(),
                y,
                egui::Stroke::new(1.0, egui::Color32::DARK_GRAY),
            );
        }
        painter.text(
            egui::pos2(area.max.x - 2.0, y + 1.0),
            egui::Align2::RIGHT_TOP,
            &thread.name,
            egui::FontId::monospace(10.0),
            egui::Color32::GRAY,
        );
    }
    // The strip's frame boundaries (ADR-7409): a line and the frame number at each frame start.
    let view = world
        .get_resource::<ProfilerView>()
        .map(|v| v.timeline_view)
        .unwrap_or_default()
        .clamped();
    if data.frames.len() > 1 {
        let span = (layout.window_ns.1.saturating_sub(layout.window_ns.0)).max(1) as f32;
        let at = |t: u64| {
            (t.saturating_sub(layout.window_ns.0) as f32 / span * view.width.max(1.0)).clamp(0.0, view.width)
        };
        for strip_frame in &data.frames {
            let x = area.min.x + at(strip_frame.start_ns);
            painter.vline(
                x,
                area.y_range(),
                egui::Stroke::new(1.0, egui::Color32::from_rgb(200, 140, 40)),
            );
            painter.text(
                egui::pos2(x + 2.0, area.min.y + 1.0),
                egui::Align2::LEFT_TOP,
                strip_frame.number.to_string(),
                egui::FontId::monospace(10.0),
                egui::Color32::from_rgb(230, 170, 80),
            );
        }
    }
    // Marks recorded in the strip's frames (P-318): a line at their frame's start, hover names them.
    let marks =
        crate::profiler_modules::timeline_marks(&regia::telemetry::counters::marks_since(0), &data.frames);
    let mark_color = crate::theme::ui_tokens(ui.ctx()).accents.select;
    let mut mark_hover = Vec::new();
    for (x, label) in crate::thread_timeline::mark_xs(&layout, &data.frames, &marks, view.width) {
        let x = area.min.x + x;
        painter.vline(x, area.y_range(), egui::Stroke::new(1.5, mark_color));
        if response.hover_pos().is_some_and(|p| (p.x - x).abs() <= 3.0) {
            mark_hover.push(label);
        }
    }
    if !mark_hover.is_empty() {
        let _ = response.clone().on_hover_text(mark_hover.join("\n"));
    }
    let hover = response.hover_pos();
    if let Some(pos) = hover {
        let row = ((pos.y - area.min.y) / ROW_HEIGHT) as usize;
        if let Some(rect) = rect_at(&layout, pos.x - area.min.x, row) {
            let thread = layout.threads.get(rect.thread).map_or("?", |t| t.name.as_str());
            let frame_of = frame_at_offset(&data.frames, rect.start_ns)
                .map_or_else(String::new, |n| format!("frame {n} · "));
            let what = if rect.count > 1 {
                format!("{} (first: {})", rect.label(), rect.name)
            } else {
                rect.name.clone()
            };
            // Percent mode (P-304): the bar's share of the frame it belongs to.
            let duration = if crate::profiler_modules::percent_mode(world) {
                let frame_ns = frame_at_offset(&data.frames, rect.start_ns)
                    .and_then(|n| data.frames.iter().find(|f| f.number == n))
                    .map_or(frame.frame_ns, |f| f.frame_ns);
                format!(
                    "{:.1} % of frame",
                    crate::profiler_modules::percent_of(rect.duration_ms(), frame_ns as f64 / 1e6)
                )
            } else {
                format!("{:.3} ms", rect.duration_ms())
            };
            let _ = response.clone().on_hover_text(format!(
                "{what}\n{frame_of}{thread} · {duration} · {:.3} to {:.3} ms",
                rect.start_ns as f64 / 1e6,
                rect.end_ns as f64 / 1e6,
            ));
        }
        // Zoom about the pointer: the wheel or a pinch.
        let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta().y, i.zoom_delta()));
        let factor = if (pinch - 1.0).abs() > f32::EPSILON {
            pinch
        } else {
            (scroll * 0.01).exp()
        };
        if (factor - 1.0).abs() > f32::EPSILON {
            let mut view = world.get_resource_or_insert_with(ProfilerView::default);
            view.timeline_view = zoom_about(view.timeline_view, factor, pos.x - area.min.x);
        }
    }
    if response.dragged() {
        let dx = response.drag_delta().x;
        let mut view = world.get_resource_or_insert_with(ProfilerView::default);
        view.timeline_view = pan_by(view.timeline_view, dx);
    }
    // UI site (click holds the frame under the pointer: in a strip the frame drawn there, else the
    // shown frame): view state; the agent reads it as `held` from `editor.profiler_timeline` and
    // holds a frame with its `frame` parameter.
    if response.clicked() {
        let clicked = response
            .interact_pointer_pos()
            .and_then(|pos| {
                crate::thread_timeline::frame_at_x(&layout, &data.frames, pos.x - area.min.x, view.width)
            })
            .unwrap_or(frame.number);
        world
            .get_resource_or_insert_with(ProfilerView::default)
            .timeline_frame = Some(clicked);
    }
}

/// Paints a flame view; `colors` maps a span path to its delta (ms) for Compare.
fn draw_flame(
    ui: &mut egui::Ui,
    capture: &ProfileCapture,
    colors: Option<(&std::collections::HashMap<String, f64>, f64)>,
) {
    draw_flame_in(ui, capture, colors, None);
}

/// [`draw_flame`]; with `percent_base` (the frame's ms) bar labels and hover show percent of frame.
fn draw_flame_in(
    ui: &mut egui::Ui,
    capture: &ProfileCapture,
    colors: Option<(&std::collections::HashMap<String, f64>, f64)>,
    percent_base: Option<f64>,
) {
    let width = ui.available_width();
    let rects = flame_layout(capture, width, MIN_BAR_WIDTH);
    let rows = rects.iter().map(|r| r.depth + 1).max().unwrap_or(1);
    let (area, response) =
        ui.allocate_exact_size(egui::vec2(width, rows as f32 * ROW_HEIGHT), egui::Sense::hover());
    // A named canvas for screen readers; the span list beside it carries the bars (ADR-7383).
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("Flame view, {} spans", capture.node_count()),
    );
    let painter = ui.painter_at(area);
    let hover = response.hover_pos();
    for rect in &rects {
        let bar = egui::Rect::from_min_size(
            area.min + egui::vec2(rect.x, rect.depth as f32 * ROW_HEIGHT),
            egui::vec2(rect.width - 1.0, ROW_HEIGHT - 1.0),
        );
        let (fill, delta) = match colors {
            Some((deltas, max)) => {
                let d = deltas.get(&rect.path).copied().unwrap_or(0.0);
                let [r, g, b] = crate::profiler_model::delta_color(d, max);
                (egui::Color32::from_rgb(r, g, b), Some(d))
            }
            None => {
                let shade = 60 + ((rect.depth * 37) % 120) as u8;
                (egui::Color32::from_rgb(200, shade, 40), None)
            }
        };
        painter.rect_filled(bar, 2.0, fill);
        if rect.width > 40.0 {
            let label = match percent_base {
                Some(base) => format!(
                    "{} {:.1} %",
                    rect.name,
                    crate::profiler_modules::percent_of(rect.inclusive_ms, base)
                ),
                None => rect.name.clone(),
            };
            painter.text(
                bar.left_center() + egui::vec2(3.0, 0.0),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::monospace(11.0),
                egui::Color32::BLACK,
            );
        }
        if hover.is_some_and(|p| bar.contains(p)) {
            let delta = delta.map(|d| format!(" · change {d:+.3} ms")).unwrap_or_default();
            let times = match percent_base {
                Some(base) => format!(
                    "inclusive {:.1} % of frame · self {:.1} %",
                    crate::profiler_modules::percent_of(rect.inclusive_ms, base),
                    crate::profiler_modules::percent_of(rect.self_ms, base)
                ),
                None => format!(
                    "inclusive {:.3} ms · self {:.3} ms",
                    rect.inclusive_ms, rect.self_ms
                ),
            };
            let _ = response
                .clone()
                .on_hover_text(format!("{}\n{times} · {:.1} calls{delta}", rect.name, rect.calls));
        }
    }
}

fn source_picker(ui: &mut egui::Ui, world: &mut World) {
    let current = world
        .get_resource::<ProfilerView>()
        .map(|v| v.source().clone())
        .unwrap_or_default();
    let mut choices = vec![ProfilerSource::Edit, ProfilerSource::Pie];
    if let Some(procs) = world.get_resource::<crate::pie_process::PieProcesses>() {
        for p in procs.processes() {
            if p.exit.is_none() {
                choices.push(ProfilerSource::Process(p.role));
            }
        }
    }
    let remote_label = "Dev build…";
    let mut picked: Option<ProfilerSource> = None;
    let mut remote = matches!(current, ProfilerSource::Remote { .. });
    // UI site: agent twin `editor.profiler_source { kind }`.
    egui::ComboBox::from_label("Source")
        .selected_text(current.label())
        .show_ui(ui, |ui| {
            for choice in choices {
                let selected = choice == current;
                let clicked = ui.selectable_label(selected, choice.label()).clicked();
                // A click on the selected entry picks it again while its worker's link is
                // disconnected, restarting the worker (ADR-7493, the gesture ADR-7427's
                // panicked-worker reason names); a healthy selected entry stays a no-op.
                if clicked && (!selected || selected_link_disconnected(world)) {
                    picked = Some(choice);
                }
            }
            if ui.selectable_label(remote, remote_label).clicked() {
                remote = true;
            }
        });
    if let Some(source) = picked {
        let result = crate::profiler_source::set_source(world, source);
        let mut view = world.get_resource_or_insert_with(ProfilerView::default);
        view.error = result.err().map(|(_, message)| message);
        return;
    }
    if remote {
        let mut view = world.get_resource_or_insert_with(ProfilerView::default);
        let (mut address, mut token) = (view.remote_address.clone(), view.remote_token.clone());
        ui.add(
            egui::TextEdit::singleline(&mut address)
                .hint_text("ws://127.0.0.1:<port>")
                .desired_width(170.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut token)
                .hint_text("token")
                .password(true)
                .desired_width(120.0),
        );
        view.remote_address.clone_from(&address);
        view.remote_token.clone_from(&token);
        // UI site: agent twin `editor.profiler_source { kind: "remote", address, token }`.
        if ui.button("Connect").clicked() {
            let result = crate::profiler_source::set_source(world, ProfilerSource::Remote { address, token });
            let mut view = world.get_resource_or_insert_with(ProfilerView::default);
            view.error = result.err().map(|(_, message)| message);
        }
    }
}

/// Whether the selected source's worker link is `disconnected` (a poisoned or dead worker):
/// picking the already-selected entry again then restarts the worker, as ADR-7427's
/// panicked-worker reason tells the user to. `false` for a healthy link, and for the in-process
/// sources, which have no worker.
fn selected_link_disconnected(world: &World) -> bool {
    world
        .get_resource::<ProfilerView>()
        .and_then(ProfilerView::worker)
        .is_some_and(|w| matches!(w.snapshot().link, LinkState::Disconnected(_)))
}

fn capture_row(ui: &mut egui::Ui, world: &mut World) {
    let mut view = world.get_resource_or_insert_with(ProfilerView::default);
    let mut path = view.path_field.clone();
    ui.add(
        egui::TextEdit::singleline(&mut path)
            .hint_text("captures/….rprof.json")
            .desired_width(200.0),
    );
    view.path_field.clone_from(&path);
    let mut slot: Option<&str> = None;
    // UI sites: agent twin `editor.profiler_capture_slot { slot: view | a | b, path? }`.
    if ui.button("Open").clicked() {
        slot = Some("view");
    }
    if ui.button("Set A").clicked() {
        slot = Some("a");
    }
    if ui.button("Set B").clicked() {
        slot = Some("b");
    }
    let clear = ui.button("Clear").clicked();
    if clear {
        let mut view = world.get_resource_or_insert_with(ProfilerView::default);
        view.opened = None;
        view.compare_a = None;
        view.compare_b = None;
        view.error = None;
    }
    if let Some(slot) = slot {
        let loaded = crate::profiler_source::read_capture_file(world, &path);
        let mut view = world.get_resource_or_insert_with(ProfilerView::default);
        match loaded {
            Ok(capture) => {
                let entry = Some((path, capture));
                match slot {
                    "view" => view.opened = entry,
                    "a" => view.compare_a = entry,
                    _ => view.compare_b = entry,
                }
                view.error = None;
            }
            Err(e) => view.error = Some(e),
        }
    }
}

fn flame_tab(
    ui: &mut egui::Ui,
    world: &mut World,
    data: &SourceData,
    keys: &mut crate::canvas_keys::ListKeys,
) {
    let percent = crate::profiler_modules::percent_toggle(ui, world);
    let opened = world
        .get_resource::<ProfilerView>()
        .and_then(|v| v.opened.clone());
    let capture = match (&opened, &data.capture) {
        (Some((path, capture)), _) => {
            ui.weak(format!("opened {path}"));
            capture
        }
        (None, Some(capture)) => capture,
        (None, None) => {
            ui.label("No frame profiler is installed.");
            return;
        }
    };
    ui.label(format!(
        "{:.2} ms per frame over {} frames · {} spans",
        capture.frame_ms,
        capture.frames,
        capture.node_count(),
    ));
    flame_ui_in(ui, capture, keys, percent);
    if !capture.gpu.is_empty() {
        ui.separator();
        ui.label("GPU passes (last frame)");
        for pass in &capture.gpu {
            ui.horizontal(|ui| {
                ui.monospace(format!("{:>16}", pass.pass));
                ui.monospace(format!("{:.3} ms", pass.ms));
            });
        }
    }
}

/// The History tab's bar color: the frozen frame highlighted white, a hitch red, a normal
/// frame blue (ADR-9740; the freeze agent twin `editor.profiler_freeze`, ADR-7409).
#[must_use]
pub fn history_bar_color(
    frozen: Option<usize>,
    index: usize,
    ms: f64,
    hitch_threshold_ms: f64,
) -> egui::Color32 {
    if frozen == Some(index) {
        egui::Color32::WHITE
    } else if ms > hitch_threshold_ms {
        egui::Color32::from_rgb(230, 70, 50)
    } else {
        egui::Color32::from_rgb(90, 160, 220)
    }
}

fn history_tab(ui: &mut egui::Ui, world: &mut World, data: &SourceData) {
    use crate::profiler_model::{hitch_threshold_ms, hitches, median_ms};
    let samples = &data.history;
    if samples.is_empty() {
        ui.label("No frames recorded yet.");
        return;
    }
    let threshold = hitch_threshold_ms(samples);
    ui.label(format!(
        "{} frames · median {:.2} ms · hitch over {:.2} ms",
        samples.len(),
        median_ms(samples),
        threshold
    ));
    let width = ui.available_width();
    let (area, response) = ui.allocate_exact_size(egui::vec2(width, HISTORY_HEIGHT), egui::Sense::click());
    let painter = ui.painter_at(area);
    let max = samples.iter().map(|s| s.ms).fold(threshold, f64::max).max(1e-3);
    let step = width / crate::profiler_model::HISTORY_FRAMES as f32;
    let frozen = world.get_resource::<ProfilerView>().and_then(|v| v.frozen);
    for (i, sample) in samples.iter().enumerate() {
        let h = (sample.ms / max) as f32 * HISTORY_HEIGHT;
        let x = area.min.x + i as f32 * step;
        let bar = egui::Rect::from_min_max(
            egui::pos2(x, area.max.y - h),
            egui::pos2(x + step.max(1.0), area.max.y),
        );
        painter.rect_filled(bar, 0.0, history_bar_color(frozen, i, sample.ms, threshold));
    }
    let y = area.max.y - (threshold / max) as f32 * HISTORY_HEIGHT;
    painter.hline(area.x_range(), y, egui::Stroke::new(1.0, egui::Color32::YELLOW));
    // Trace marks, level loads and hitches (P-318): drawn on the graph, named on hover and in the
    // focusable list below it.
    let marks = crate::profiler_modules::current_history_marks(world, samples);
    let tokens = crate::theme::ui_tokens(ui.ctx());
    if let Some(label) = crate::profiler_modules::paint_history_marks(
        &painter,
        &tokens,
        area,
        step,
        &marks,
        response.hover_pos(),
    ) {
        let _ = response.clone().on_hover_text(label);
    }
    // UI site (click a frame to freeze it): agent twin `editor.profiler_freeze { frame? }`
    // (ADR-7409); `editor.profiler_state` reports the frozen index.
    if response.clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let index = ((pos.x - area.min.x) / step) as usize;
        crate::profiler_source::freeze_history(world, Some(index), samples);
    }
    let frozen = world.get_resource::<ProfilerView>().and_then(|v| v.frozen);
    if let Some(i) = frozen.filter(|i| *i < samples.len()) {
        ui.horizontal(|ui| {
            ui.strong(format!(
                "frame {i}: {:.3} ms (frozen; new frames paused)",
                samples[i].ms
            ));
            // UI site: agent twin `editor.profiler_freeze {}` (unfreeze resumes the history).
            if ui.small_button("Unfreeze").clicked() {
                crate::profiler_source::freeze_history(world, None, samples);
            }
        });
        for (name, ms) in &samples[i].spans {
            ui.monospace(format!("{ms:>9.3} ms  {name}"));
        }
    }
    crate::profiler_modules::mark_list(ui, world, &marks);
    ui.separator();
    let rows = hitches(samples, threshold);
    ui.label(format!("{} hitches", rows.len()));
    egui::ScrollArea::vertical()
        .id_salt("profiler_hitches")
        .max_height(160.0)
        .show(ui, |ui| {
            for i in rows {
                let spans: Vec<String> = samples[i]
                    .spans
                    .iter()
                    .map(|(n, ms)| format!("{n} {ms:.2}"))
                    .collect();
                if ui
                    .selectable_label(
                        frozen == Some(i),
                        format!("frame {i}: {:.2} ms · {}", samples[i].ms, spans.join(", ")),
                    )
                    .clicked()
                {
                    // UI site: agent twin `editor.profiler_freeze { frame }`.
                    crate::profiler_source::freeze_history(world, Some(i), samples);
                }
            }
        });
}

fn entities_tab(ui: &mut egui::Ui, data: &SourceData) {
    let Some(counts) = &data.entities else {
        ui.label("No entity counts for this source yet (play stopped, or no answer from the process).");
        return;
    };
    ui.label(format!(
        "{} entities · {} archetypes",
        counts.entities, counts.archetypes
    ));
    if counts.components.is_empty() {
        ui.weak(
            "No component ranking from this source (a build older than profiler.frame's entities field).",
        );
        return;
    }
    egui::Grid::new("profiler_components")
        .striped(true)
        .show(ui, |ui| {
            ui.strong("component");
            ui.strong("entities");
            ui.end_row();
            for (name, n) in &counts.components {
                let _ = ui
                    .monospace(name.rsplit("::").next().unwrap_or(name))
                    .on_hover_text(name);
                ui.monospace(n.to_string());
                ui.end_row();
            }
        });
}

fn compare_tab(ui: &mut egui::Ui, world: &World) {
    let Some(view) = world.get_resource::<ProfilerView>() else {
        return;
    };
    ui.label(format!(
        "A: {} · B: {}",
        view.compare_a.as_ref().map_or("-", |(p, _)| p.as_str()),
        view.compare_b.as_ref().map_or("-", |(p, _)| p.as_str())
    ));
    let (Some(diff), Some((_, b))) = (view.diff(), view.compare_b.as_ref()) else {
        ui.weak("Open two captures with Set A and Set B.");
        return;
    };
    ui.label(format!(
        "frame {:.3} → {:.3} ms ({:+.3})",
        diff.frame_ms_a, diff.frame_ms_b, diff.frame_ms_delta
    ));
    let deltas: std::collections::HashMap<String, f64> =
        diff.spans.iter().map(|s| (s.key(), s.inclusive_delta)).collect();
    draw_flame(ui, b, Some((&deltas, diff.max_abs_inclusive_delta())));
    egui::ScrollArea::vertical()
        .id_salt("profiler_compare")
        .show(ui, |ui| {
            egui::Grid::new("profiler_compare_rows")
                .striped(true)
                .show(ui, |ui| {
                    for h in ["span", "incl. A", "incl. B", "Δ incl.", "Δ self"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for s in &diff.spans {
                        ui.monospace(s.key());
                        ui.monospace(format!("{:.3}", s.inclusive_a));
                        ui.monospace(format!("{:.3}", s.inclusive_b));
                        ui.monospace(format!("{:+.3}", s.inclusive_delta));
                        ui.monospace(format!("{:+.3}", s.self_delta));
                        ui.end_row();
                    }
                });
        });
}

/// Draws the window when the panel is open (called every editor frame). The source is read once
/// per repaint inside the `profiler_panel` span ([`frame_data`]).
pub fn window(ctx: &egui::Context, world: &mut World) {
    let Some(mut panel) = world.get_resource::<ProfilerPanel>().cloned() else {
        return;
    };
    if !panel.open {
        return;
    }
    let _span = tracing::info_span!("profiler_panel").entered();
    let data = frame_data(world, ctx.cumulative_frame_nr());
    crate::profiler_modules::collect(world, ctx.cumulative_frame_nr());
    let mut open = panel.open;
    egui::Window::new("Profiler")
        .default_pos(crate::tool_windows::default_pos(ctx, "Profiler"))
        .open(&mut open)
        .default_width(760.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                source_picker(ui, world);
                ui.weak(match &data.message {
                    Some(m) => format!("{}: {m}", data.status),
                    None => data.status.clone(),
                });
            });
            ui.horizontal(|ui| {
                let profiler = world.get_resource::<regia::Profiler>().map(|p| p.0.clone());
                if let Some(profiler) = &profiler {
                    let mut flame = profiler.flame_enabled();
                    if ui.checkbox(&mut flame, "Record call tree").changed() {
                        profiler.set_flame_enabled(flame);
                    }
                }
                // UI site: agent twin `profiler.capture { path }`.
                if ui.button("Capture").clicked() {
                    panel.status = match capture_to_project(world) {
                        Ok(path) => format!("wrote {}", path.display()),
                        Err(err) => err,
                    };
                }
                capture_row(ui, world);
            });
            if !panel.status.is_empty() {
                ui.weak(&panel.status);
            }
            if let Some(error) = world.get_resource::<ProfilerView>().and_then(|v| v.error.clone()) {
                ui.colored_label(egui::Color32::from_rgb(230, 90, 70), error);
            }
            let tab = world
                .get_resource::<ProfilerView>()
                .map(|v| v.tab)
                .unwrap_or_default();
            let extra = world
                .get_resource::<crate::profiler_modules::ProfilerModulesView>()
                .and_then(|v| v.tab);
            ui.horizontal_wrapped(|ui| {
                for t in ProfilerTab::ALL {
                    // View-only tabs; `editor.profiler_state` returns every tab's data at once.
                    if ui
                        .selectable_label(extra.is_none() && tab == t, t.label())
                        .clicked()
                    {
                        world.get_resource_or_insert_with(ProfilerView::default).tab = t;
                        world
                            .get_resource_or_insert_with(
                                crate::profiler_modules::ProfilerModulesView::default,
                            )
                            .tab = None;
                    }
                }
                ui.separator();
                for t in crate::profiler_modules::ExtraTab::ALL {
                    // View-only tabs; agent twins `editor.profiler_modules`, `_monitors`, `_loads`.
                    if ui.selectable_label(extra == Some(t), t.label()).clicked() {
                        world
                            .get_resource_or_insert_with(
                                crate::profiler_modules::ProfilerModulesView::default,
                            )
                            .tab = Some(t);
                    }
                }
            });
            ui.separator();
            if tab != ProfilerTab::Timeline || extra.is_some() {
                crate::profiler_source::stop_timeline_polls(world);
            }
            if let Some(extra) = extra {
                use crate::profiler_modules::ExtraTab;
                match extra {
                    ExtraTab::Module(module) => crate::profiler_modules::module_tab(ui, world, module),
                    ExtraTab::Monitors => crate::profiler_modules::monitors_tab(ui, world),
                    ExtraTab::Loads => crate::profiler_modules::loads_tab(ui, world),
                }
                return;
            }
            match tab {
                ProfilerTab::Flame => flame_tab(ui, world, &data, &mut panel.keys),
                ProfilerTab::Timeline => timeline_tab(ui, world, ctx.cumulative_frame_nr()),
                ProfilerTab::History => history_tab(ui, world, &data),
                ProfilerTab::Entities => entities_tab(ui, &data),
                ProfilerTab::Memory => crate::memory_browser::memory_tab(ui, world, data.memory.as_ref()),
                ProfilerTab::Compare => compare_tab(ui, world),
                ProfilerTab::Lockstep => lockstep_tab(ui, world),
            }
        });
    panel.open = open;
    world.insert_resource(panel);
}

/// Fill of a Lockstep bar under its budget, and over it.
const LOCKSTEP_OK: egui::Color32 = egui::Color32::from_rgb(90, 170, 110);
const LOCKSTEP_OVER: egui::Color32 = egui::Color32::from_rgb(230, 90, 70);

/// One Lockstep row: name, p99 bar against the whole-tick budget line (at 80 % of the bar's
/// width) and the numbers. The row's accessible name is its label.
fn lockstep_row_ui(ui: &mut egui::Ui, row: &crate::profiler_model::LockstepRow) {
    let label = crate::profiler_model::lockstep_row_label(row);
    ui.horizontal(|ui| {
        let (rect, response) = ui.allocate_exact_size(egui::vec2(220.0, 14.0), egui::Sense::hover());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
        let budget_x = rect.left() + rect.width() * 0.8;
        let fill = (rect.width() * 0.8 * row.bar.clamp(0.0, 1.25)).max(1.0);
        let bar = egui::Rect::from_min_size(rect.min, egui::vec2(fill, rect.height()));
        painter.rect_filled(
            bar,
            0.0,
            if row.over_budget {
                LOCKSTEP_OVER
            } else {
                LOCKSTEP_OK
            },
        );
        painter.line_segment(
            [
                egui::pos2(budget_x, rect.top()),
                egui::pos2(budget_x, rect.bottom()),
            ],
            egui::Stroke::new(1.5, ui.visuals().strong_text_color()),
        );
        let text = format!(
            "{:<16} p50 {:>7.3}  p99 {:>7.3}  max {:>7.3} ms",
            row.system, row.p50_ms, row.p99_ms, row.max_ms
        );
        if row.over_budget {
            ui.colored_label(LOCKSTEP_OVER, egui::RichText::new(text).monospace());
        } else {
            ui.label(egui::RichText::new(text).monospace());
        }
    });
}

/// The Lockstep tab (ADR-8562): the deterministic tick profile per system (`sim.tick_profile`),
/// the budget line, the over-budget ticks, and the det debug draw switch (`sim.det_debug_draw`).
/// Reads the edit world's sim, else the play world's copy the editor loop leaves.
fn lockstep_tab(ui: &mut egui::Ui, world: &mut World) {
    use crate::agent_parity::{
        PROFILER_LOCKSTEP_CANDIDATES, PROFILER_LOCKSTEP_CONTACTS, PROFILER_LOCKSTEP_GRID,
        PROFILER_LOCKSTEP_OCCUPANCY, PROFILER_LOCKSTEP_QUERIES,
    };
    use regia_framework::det_debug::DebugWhat;
    let (report, draw) = crate::profiler_source::lockstep_source(world);
    match report {
        None => {
            ui.weak("No tick profile: the game has no sim host that publishes tick_profile() and no DetTickProfile resource.");
        }
        Some(report) => {
            let view = crate::profiler_model::lockstep_view(&report);
            ui.label(format!(
                "Clock {} · {} ticks · budget {:.3} ms per tick (the line on each bar)",
                view.clock, view.samples, view.tick_budget_ms
            ));
            for row in &view.rows {
                lockstep_row_ui(ui, row);
            }
            ui.separator();
            lockstep_row_ui(ui, &view.tick);
            ui.separator();
            if view.over_budget.is_empty() {
                ui.weak("No over-budget ticks in the ring.");
            } else {
                ui.label(format!(
                    "Over budget: {} ticks (newest first)",
                    view.over_budget.len()
                ));
                egui::ScrollArea::vertical()
                    .id_salt("lockstep_over_budget")
                    .max_height(140.0)
                    .show(ui, |ui| {
                        for tick in &view.over_budget {
                            ui.colored_label(
                                LOCKSTEP_OVER,
                                format!(
                                    "tick {}: {:.3} ms, slowest {} {:.3} ms",
                                    tick.tick, tick.total_ms, tick.worst.0, tick.worst.1
                                ),
                            );
                        }
                    });
            }
        }
    }
    ui.separator();
    let Some(draw) = draw else {
        ui.weak("Debug draw: the game has no DetDebugDraw resource and no sim host whose sim draws.");
        return;
    };
    ui.horizontal(|ui| {
        ui.label("Debug draw");
        let mut what = draw.what;
        for (action, flag) in [
            (&PROFILER_LOCKSTEP_GRID, DebugWhat::GRID),
            (&PROFILER_LOCKSTEP_OCCUPANCY, DebugWhat::OCCUPANCY),
            (&PROFILER_LOCKSTEP_CANDIDATES, DebugWhat::CANDIDATES),
            (&PROFILER_LOCKSTEP_CONTACTS, DebugWhat::CONTACTS),
            (&PROFILER_LOCKSTEP_QUERIES, DebugWhat::QUERIES),
        ] {
            let mut on = what.contains(flag);
            if crate::agent_parity::action_toggle(ui, action, &mut on).changed() {
                what = if on {
                    what.union(flag)
                } else {
                    DebugWhat::from_bits(what.bits() & !flag.bits())
                };
            }
        }
        // Idempotent (a set, not a flip), so the second UI pass of a frame changes nothing.
        if what != draw.what {
            crate::profiler_source::request_det_debug(world, what);
        }
        ui.weak(match draw.tick {
            Some(tick) => format!("{} primitives at tick {tick}", draw.primitives),
            None => "no frame yet".to_owned(),
        });
    });
    ui.weak("Show ▸ Lockstep draws the frame over play in the viewport.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str, inclusive: f64, children: Vec<CaptureNode>) -> CaptureNode {
        let child_sum: f64 = children.iter().map(|c| c.inclusive_ms).sum();
        CaptureNode {
            name: name.into(),
            inclusive_ms: inclusive,
            self_ms: inclusive - child_sum,
            calls: 1.0,
            children,
        }
    }

    fn capture() -> ProfileCapture {
        ProfileCapture {
            format_version: 1,
            frames: 1,
            frame_ms: 10.0,
            cpu: vec![
                n(
                    "render",
                    6.0,
                    vec![
                        n("forward", 3.0, vec![n("sort", 0.01, vec![])]),
                        n("shadows", 2.0, vec![]),
                    ],
                ),
                n("update", 2.0, vec![]),
            ],
            truncated: 0,
            gpu: Vec::new(),
            folded: String::new(),
            timeline: Vec::new(),
            timeline_truncated: 0,
        }
    }

    #[test]
    fn bars_are_proportional_and_children_sit_inside_their_parents() {
        let rects = flame_layout(&capture(), 800.0, MIN_BAR_WIDTH);
        let by = |name: &str| rects.iter().find(|r| r.name == name).cloned().expect(name);
        let (render, update) = (by("render"), by("update"));
        assert_eq!((render.x, render.width), (0.0, 600.0));
        assert_eq!((update.x, update.width), (600.0, 200.0), "roots fill the width");
        let (forward, shadows) = (by("forward"), by("shadows"));
        assert_eq!((forward.depth, forward.x, forward.width), (1, 0.0, 300.0));
        assert_eq!(
            (shadows.x, shadows.width),
            (300.0, 200.0),
            "siblings follow each other"
        );
        for child in [&forward, &shadows] {
            assert!(child.x >= render.x && child.x + child.width <= render.x + render.width);
        }
        // `sort` is 1 point wide: below the minimum, dropped.
        assert!(rects.iter().all(|r| r.name != "sort"));
        assert_eq!(rects.len(), 4);
        assert_eq!(
            flame_layout(&capture(), 800.0, MIN_BAR_WIDTH),
            rects,
            "deterministic"
        );
    }

    /// FU-5 (ADR-7383): the flame view is a named canvas and its span list names every span of
    /// the frame (the thin `sort` too), reachable by Tab and Down with no pointer.
    #[test]
    fn the_span_list_names_every_span_of_the_capture() {
        use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to};
        let ctx = egui::Context::default();
        let capture = capture();
        let mut keys = crate::canvas_keys::ListKeys::default();
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| flame_ui_in(ui, &capture, &mut keys, false));
        };
        frame(&ctx, [900.0, 600.0], vec![], &mut draw);
        let update = frame(&ctx, [900.0, 600.0], vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        assert_eq!(
            labels_of(&update, egui::accesskit::Role::Canvas),
            ["Flame view, 5 spans"]
        );
        let names: Vec<String> = crate::a11y_audit::widget_names(&update)
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        for want in [
            "render, main thread, 6.000 ms inclusive, 1.000 ms self, 1.0 calls",
            "forward in render, main thread, 3.000 ms inclusive, 2.990 ms self, 1.0 calls",
            "sort in forward, main thread, 0.010 ms inclusive, 0.010 ms self, 1.0 calls",
            "shadows in render, main thread, 2.000 ms inclusive, 2.000 ms self, 1.0 calls",
            "update, main thread, 2.000 ms inclusive, 2.000 ms self, 1.0 calls",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} not in {names:?}");
        }
        let render = "render, main thread, 6.000 ms inclusive, 1.000 ms self, 1.0 calls";
        tab_to(&ctx, [900.0, 600.0], render, false, 10, &mut draw);
        frame(
            &ctx,
            [900.0, 600.0],
            vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)],
            &mut draw,
        );
        let down = frame(&ctx, [900.0, 600.0], vec![], &mut draw);
        assert_eq!(
            focused_name(&down).as_deref(),
            Some("forward in render, main thread, 3.000 ms inclusive, 2.990 ms self, 1.0 calls"),
            "Down moves to the next span in tree order"
        );
    }

    #[test]
    fn an_empty_capture_or_view_lays_out_nothing() {
        let mut empty = capture();
        empty.cpu.clear();
        assert!(flame_layout(&empty, 800.0, MIN_BAR_WIDTH).is_empty());
        assert!(flame_layout(&capture(), 0.0, MIN_BAR_WIDTH).is_empty());
        // A zero minimum keeps every bar.
        assert_eq!(flame_layout(&capture(), 800.0, 0.0).len(), 5);
    }

    #[test]
    fn flame_rects_carry_their_path() {
        let rects = flame_layout(&capture(), 800.0, MIN_BAR_WIDTH);
        let forward = rects.iter().find(|r| r.name == "forward").expect("forward");
        assert_eq!(forward.path, "render;forward");
    }

    #[test]
    fn capture_to_project_needs_a_project_and_a_profiler() {
        let mut world = World::new();
        assert!(capture_to_project(&world).is_err());
        let dir = regia_test_utils::unique_temp_dir("profiler-panel");
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        assert_eq!(
            capture_to_project(&world),
            Err("no frame profiler is installed".to_string())
        );
        world.insert_resource(regia::Profiler(regia_core::profiling::FrameProfiler::new()));
        let path = capture_to_project(&world).expect("written");
        assert!(path.starts_with(dir.join("captures")));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(ProfileCapture::from_json(&text).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The source picker's click tests (ADR-7427's review, w8): headless egui frames whose
    // widgets are found by accessible name and clicked with real pointer events.

    use crate::pie_process::ProcessRole;
    use crate::profiler_source::{LinkState, WORKER_PANICKED};

    /// One headless frame of the picker row; returns its AccessKit tree, so a test finds widgets
    /// by name and clicks them without a pointer device.
    fn picker_frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        world: &mut World,
    ) -> egui::accesskit::TreeUpdate {
        crate::canvas_keys::test_input::frame(ctx, [400.0, 120.0], events, &mut |ui: &mut egui::Ui| {
            ui.horizontal(|ui| source_picker(ui, world));
        })
    }

    /// The node id and center of the widget labelled `label`, skipping `skip` (the combo box's
    /// own button when its popup entry of the same text is wanted), from an AccessKit tree. A
    /// combo box carries its selected text as the node's value (not its label), a popup entry is
    /// a labelled button.
    fn labeled(
        update: &egui::accesskit::TreeUpdate,
        label: &str,
        skip: Option<egui::accesskit::NodeId>,
    ) -> Option<(egui::accesskit::NodeId, egui::Pos2)> {
        fn text_of(node: &egui::accesskit::Node) -> Option<&str> {
            node.label().or_else(|| node.value())
        }
        update
            .nodes
            .iter()
            .filter(|(id, _)| Some(*id) != skip)
            .find_map(|(id, node)| {
                let bounds = node.bounds()?;
                (text_of(node) == Some(label)).then(|| {
                    let center = |a: f64, b: f64| ((a + b) / 2.0) as f32;
                    (
                        *id,
                        egui::pos2(center(bounds.x0, bounds.x1), center(bounds.y0, bounds.y1)),
                    )
                })
            })
    }

    /// Clicks `at` through pointer events: hover, press, release, one frame each.
    fn click(ctx: &egui::Context, world: &mut World, at: egui::Pos2) {
        let press = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        picker_frame(ctx, vec![egui::Event::PointerMoved(at)], world);
        picker_frame(ctx, vec![press(at, true)], world);
        picker_frame(ctx, vec![press(at, false)], world);
    }

    /// Opens the picker's popup and clicks its entry labelled `label` (the closed combo box's
    /// button carries the selected entry's label, so the entry is told apart by node id).
    fn pick_entry(ctx: &egui::Context, world: &mut World, label: &str) {
        let _ = picker_frame(ctx, vec![], world);
        let closed = picker_frame(ctx, vec![], world);
        let (button, at) = labeled(&closed, label, None)
            .unwrap_or_else(|| panic!("the picker shows a button labelled {label}"));
        click(ctx, world, at);
        let open = picker_frame(ctx, vec![], world);
        let (_, entry) =
            labeled(&open, label, Some(button)).unwrap_or_else(|| panic!("the picker's popup lists {label}"));
        click(ctx, world, entry);
    }

    /// ADR-7427's review (w8): the reason a panicked worker leaves says to select the source
    /// again to restart it, so a click on the already-selected entry does that while its link is
    /// disconnected: `set_source` runs and the poison (polls = 7) is gone with a fresh worker.
    #[test]
    fn a_click_on_a_disconnected_selected_entry_restarts_the_worker() {
        let dir = regia_test_utils::TempDir::new("profiler-click-restart");
        let mut world = World::new();
        world.insert_resource(crate::pie_process::test_hooks::dedicated_session(&dir));
        crate::profiler_source::set_source(
            &mut world,
            crate::profiler_source::ProfilerSource::Process(ProcessRole::Server),
        )
        .expect("the fake session accepts a process source");
        {
            let view = world.resource::<ProfilerView>();
            let worker = view.worker().expect("the process source started a worker");
            crate::profiler_source::test_hooks::poison(worker, 7);
            let snapshot = worker.snapshot();
            match &snapshot.link {
                LinkState::Disconnected(reason) => {
                    assert!(reason.contains(WORKER_PANICKED), "{reason}");
                }
                other => panic!("a poisoned worker reads disconnected, not {other:?}"),
            }
            assert_eq!(snapshot.polls, 7);
        }
        let ctx = egui::Context::default();
        pick_entry(&ctx, &mut world, "PIE process server");
        let view = world.resource::<ProfilerView>();
        assert_eq!(
            *view.source(),
            crate::profiler_source::ProfilerSource::Process(ProcessRole::Server)
        );
        let snapshot = view.worker().expect("the restarted worker").snapshot();
        assert_eq!(
            snapshot.polls, 0,
            "a fresh worker (the poisoned one held polls = 7)"
        );
        if let LinkState::Disconnected(reason) = &snapshot.link {
            assert!(!reason.contains(WORKER_PANICKED), "{reason}");
        }
    }

    /// A click on a selected entry whose link is healthy leaves the worker alone: `set_source`
    /// would clear `ProfilerView::error`, so it is kept. The worker is held `connecting` against a
    /// listener that accepts and never answers the WebSocket handshake (the worker blocks in
    /// `connect`, so it never publishes: deterministic, no waits).
    #[test]
    fn a_click_on_a_healthy_selected_entry_leaves_the_worker_alone() {
        let dir = regia_test_utils::TempDir::new("profiler-click-healthy");
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind a listener");
        let port = listener.local_addr().expect("address").port();
        let held = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&held);
        std::thread::Builder::new()
            .name("silent-listener".into())
            .spawn(move || {
                while let Ok(stream) = listener.accept() {
                    if let Ok(mut held) = sink.lock() {
                        held.push(stream);
                    }
                }
            })
            .expect("the silent listener thread");
        regia_agent::endpoint::write(
            &dir.join("agent").join("server"),
            &regia_agent::protocol::Endpoint {
                url: format!("ws://127.0.0.1:{port}"),
                pid: 0,
                protocol: "1".to_owned(),
                token_file: "token".to_owned(),
                app: "panel-test".to_owned(),
            },
            "panel-test-token",
        )
        .expect("write the endpoint");
        let mut world = World::new();
        world.insert_resource(crate::pie_process::test_hooks::dedicated_session(&dir));
        crate::profiler_source::set_source(
            &mut world,
            crate::profiler_source::ProfilerSource::Process(ProcessRole::Server),
        )
        .expect("the fake session accepts a process source");
        {
            let view = world.resource::<ProfilerView>();
            let snapshot = view.worker().expect("worker").snapshot();
            assert_eq!(snapshot.link, LinkState::Connecting);
            assert_eq!(snapshot.polls, 0);
        }
        world.resource_mut::<ProfilerView>().error = Some("kept unless the source is set".into());
        let ctx = egui::Context::default();
        pick_entry(&ctx, &mut world, "PIE process server");
        let view = world.resource::<ProfilerView>();
        assert_eq!(
            view.error.as_deref(),
            Some("kept unless the source is set"),
            "set_source did not run"
        );
        let snapshot = view.worker().expect("the same worker").snapshot();
        assert_eq!(snapshot.link, LinkState::Connecting);
        assert_eq!(snapshot.polls, 0);
    }

    /// The in-process sources have no worker to restart: a click on the selected `edit` entry is
    /// a no-op there too.
    #[test]
    fn a_click_on_the_selected_edit_entry_is_a_no_op() {
        let mut world = World::new();
        world.insert_resource(ProfilerView::default());
        world.resource_mut::<ProfilerView>().error = Some("kept unless the source is set".into());
        let ctx = egui::Context::default();
        pick_entry(&ctx, &mut world, "Edit world");
        assert_eq!(
            world.resource::<ProfilerView>().error.as_deref(),
            Some("kept unless the source is set")
        );
    }

    /// The frozen history frame's bar is drawn highlighted (white), over hitch red and normal
    /// blue; every other bar keeps its own color (ADR-9740, the freeze agent twin).
    #[test]
    fn the_frozen_history_bar_is_drawn_highlighted() {
        assert_eq!(history_bar_color(Some(3), 3, 5.0, 4.0), egui::Color32::WHITE);
        assert_eq!(
            history_bar_color(Some(3), 2, 5.0, 4.0),
            egui::Color32::from_rgb(230, 70, 50),
            "a hitch keeps its red unless it is the frozen frame"
        );
        assert_eq!(
            history_bar_color(None, 3, 3.0, 4.0),
            egui::Color32::from_rgb(90, 160, 220)
        );
        assert_eq!(
            history_bar_color(Some(3), 4, 5.0, 4.0),
            egui::Color32::from_rgb(230, 70, 50),
            "unfreezing shows the hitch color again"
        );
    }

    /// P-318 acceptance: a trace mark shows on the History graph: it lands on the bar of the frame
    /// it was recorded in, hovering that bar's x names it, and the graph's mark list (the keyboard
    /// path) has a focusable row for it.
    #[test]
    fn a_mark_shows_on_the_history_graph() {
        use crate::canvas_keys::test_input::{frame, tab_to};
        let profiler = regia_core::profiling::FrameProfiler::new();
        for _ in 0..5 {
            profiler.end_frame();
        }
        regia::telemetry::counters::attach_frame_profiler(&profiler);
        let name = format!("history_mark_test_{}", std::process::id());
        let _ = regia::telemetry::trace::recorder().mark(&name, None);
        let marked = profiler.newest_frame_number().expect("five frames ended") + 1;
        for _ in 0..3 {
            profiler.end_frame();
        }
        let mut world = World::new();
        world.insert_resource(regia::Profiler(profiler.clone()));
        world.insert_resource(ProfilerView::default());
        crate::profiler_modules::collect(&mut world, 1);
        let data = crate::profiler_source::read_source(&mut world, 1, false, 0);
        let index = profiler
            .frame_numbers()
            .iter()
            .position(|n| *n == marked)
            .expect("the marked frame is in the history");
        let marks = crate::profiler_modules::current_history_marks(&world, &data.history);
        let mark = marks
            .iter()
            .find(|m| m.label.starts_with(&name))
            .unwrap_or_else(|| panic!("{name} is on the graph: {marks:?}"))
            .clone();
        assert_eq!(
            (mark.index, mark.kind.as_str(), mark.frame),
            (index, "trace", Some(marked))
        );
        // Hovering the mark's bar names it; another bar's x does not.
        let ctx = egui::Context::default();
        let _ = frame(&ctx, [600.0, 200.0], vec![], |ui| {
            let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(600.0, HISTORY_HEIGHT));
            let step = 600.0 / crate::profiler_model::HISTORY_FRAMES as f32;
            let tokens = crate::theme::ThemeTokens::regia_default();
            let at = |i: usize| egui::pos2((i as f32 + 0.5) * step, 40.0);
            let painter = ui.painter().clone();
            let hovered = crate::profiler_modules::paint_history_marks(
                &painter,
                &tokens,
                area,
                step,
                std::slice::from_ref(&mark),
                Some(at(index)),
            );
            assert_eq!(hovered.as_deref(), Some(mark.label.as_str()));
            let away = crate::profiler_modules::paint_history_marks(
                &painter,
                &tokens,
                area,
                step,
                std::slice::from_ref(&mark),
                Some(at(index + 40)),
            );
            assert_eq!(away, None);
        });
        // The History tab lists the mark as a focusable row (Tab reaches it).
        let row = format!("frame {index}: {}", mark.label);
        let mut draw = |ui: &mut egui::Ui| history_tab(ui, &mut world, &data);
        frame(&ctx, [900.0, 700.0], vec![], &mut draw);
        let update = frame(&ctx, [900.0, 700.0], vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        tab_to(&ctx, [900.0, 700.0], &row, false, 20, &mut draw);
    }

    /// P-304: the span list names spans by percent of frame in percent mode.
    #[test]
    fn percent_mode_names_spans_by_their_share_of_the_frame() {
        let rows = span_rows(&capture());
        let base = crate::profiler_modules::percent_base_ms(&capture());
        assert_eq!(
            span_row_name_in(&rows[0], base, true),
            "render, main thread, 60.0 % of frame inclusive, 10.0 % self, 1.0 calls"
        );
        assert_eq!(span_row_name_in(&rows[0], base, false), span_row_name(&rows[0]));
    }

    /// Item 1 (w11-profiler-finish): the Timeline tab shows a three-frame strip, and a real click
    /// on the oldest frame's part of it holds that frame (before, a click held the strip's
    /// newest frame wherever it landed).
    #[test]
    fn a_click_on_a_strip_frame_holds_that_frame() {
        let profiler = regia_core::profiling::FrameProfiler::new();
        profiler.set_timeline_enabled(true);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::registry(),
            profiler.clone(),
        ));
        tracing::dispatcher::with_default(&dispatch, || {
            for _ in 0..5 {
                {
                    let _span = tracing::info_span!("strip_click_work").entered();
                }
                profiler.end_frame();
            }
        });
        let mut world = World::new();
        world.insert_resource(regia::Profiler(profiler));
        world.insert_resource(ProfilerView::default());
        world.resource_mut::<ProfilerView>().timeline_frames = 3;
        let ctx = egui::Context::default();
        let size = [900.0, 400.0];
        let draw = |world: &mut World, events: Vec<egui::Event>| -> egui::Rect {
            let mut drawn = egui::Rect::NOTHING;
            let _ = crate::canvas_keys::test_input::frame(&ctx, size, events, |ui| {
                let nr = ui.ctx().cumulative_frame_nr();
                timeline_tab(ui, world, nr);
                drawn = ui.min_rect();
            });
            drawn
        };
        let drawn = draw(&mut world, Vec::new());
        let (frames, total, width) = {
            let view = world.resource::<ProfilerView>();
            let (_, data) = view.timeline_data.clone().expect("the tab read its timeline");
            let total = data.frame.as_ref().map_or(1, |f| f.frame_ns.max(1));
            (data.frames, total, view.timeline_view.width)
        };
        let numbers: Vec<u64> = frames.iter().map(|f| f.number).collect();
        assert_eq!(numbers, [2, 3, 4], "the strip shows the three newest frames");
        assert_eq!(
            world.resource::<ProfilerView>().timeline_frame,
            None,
            "live before the click"
        );
        // The middle of the oldest frame's extent, on the bottom row of the timeline area.
        let oldest = frames[0];
        let t = oldest.start_ns + oldest.span_ns / 2;
        let at = egui::pos2(
            drawn.min.x + (t as f64 / total as f64 * f64::from(width)) as f32,
            drawn.max.y - crate::thread_timeline::ROW_HEIGHT / 2.0,
        );
        let press = |pressed: bool| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        draw(&mut world, vec![egui::Event::PointerMoved(at)]);
        draw(&mut world, vec![press(true)]);
        draw(&mut world, vec![press(false)]);
        assert_eq!(
            world.resource::<ProfilerView>().timeline_frame,
            Some(oldest.number),
            "the click holds the frame drawn under the pointer"
        );
    }
}

#[cfg(test)]
mod parity_rows {
    /// The body of a click site: the call-site line and the lines after it, up to the first `}`
    /// line indented no deeper than the call site (the end of the `if ... { }` it opens).
    fn click_body<'a>(lines: &[&'a str], at: usize) -> Vec<&'a str> {
        let indent = |l: &str| l.len() - l.trim_start().len();
        let base = indent(lines[at]);
        let mut body = vec![lines[at]];
        for line in &lines[at + 1..] {
            body.push(line);
            if line.trim() == "}" && indent(line) <= base {
                break;
            }
        }
        body
    }

    /// Review g16 finding 3: every profiler-panel control whose click freezes or unfreezes the
    /// History tab (`freeze_history`) is classed `method` via `editor.profiler_freeze` in the
    /// agent-parity allowlist, not `view_only` (the scan matches rows by text, so a stale class
    /// stays green there).
    #[test]
    fn freeze_clicks_are_classed_as_the_freeze_method() {
        let allow = crate::agent_parity::parse_allowlist(include_str!("../agent-parity-allow.ron"))
            .unwrap_or_else(|err| panic!("{err}"));
        let source = include_str!("profiler_panel.rs");
        // Only the non-test part of the file: the scan skips test modules too.
        let source = source.split("#[cfg(test)]").next().unwrap_or(source);
        let lines: Vec<&str> = source.lines().collect();
        let mut freezing = 0;
        for entry in allow.iter().filter(|e| e.file == "profiler_panel.rs") {
            let sites: Vec<usize> = (0..lines.len())
                .filter(|&i| lines[i].trim() == entry.text)
                .collect();
            assert!(!sites.is_empty(), "row `{}` names no line", entry.text);
            for at in sites {
                if click_body(&lines, at)
                    .iter()
                    .any(|l| l.contains("freeze_history("))
                {
                    freezing += 1;
                    assert_eq!(
                        (entry.class, entry.via.as_str()),
                        (crate::agent_parity::SiteClass::Method, "editor.profiler_freeze"),
                        "line {}: `{}` freezes the history",
                        at + 1,
                        entry.text
                    );
                }
            }
        }
        assert!(
            freezing >= 2,
            "the Unfreeze button and the hitch row both freeze: {freezing}"
        );
    }

    /// Review g16 finding 4: `book/COVERAGE.md` has one Profiler per-thread Timeline row, the
    /// strip's (it names `frames?` and `systems_over_cap`), not the older one-frame row too.
    #[test]
    fn coverage_has_one_timeline_row_and_it_is_the_strip() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book/COVERAGE.md");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        let rows: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("| Profiler per-thread **Timeline**"))
            .collect();
        assert_eq!(rows.len(), 1, "one Timeline row: {rows:#?}");
        assert!(
            rows[0].contains("frames?") && rows[0].contains("systems_over_cap"),
            "the strip row stays: {}",
            rows[0]
        );
    }
}
