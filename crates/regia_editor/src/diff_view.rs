// SPDX-License-Identifier: MPL-2.0
//! The read-only diff window (spec addendum § 10.8, P-182, ADR-7799).
//!
//! Opened from a Content Browser asset's menu (**Diff against HEAD…**, **Diff against
//! revision…**) or from a graph panel's file row. It shows what [`crate::asset_diff`] found:
//!
//! - **Graphs** side by side, the old revision on the left and the new one on the right. Added
//!   nodes are drawn in the theme's `ok` accent, removed ones in its `error` color, changed ones in
//!   its `warn` color and unchanged ones dim (`muted`). Both canvases share one pan and zoom: a drag
//!   or a scroll on either moves both. The change list beside them is a keyboard list
//!   ([`crate::canvas_keys::list_rows`]): Up/Down move, Enter (or a click) centers the node in
//!   both canvases. A focused canvas pans with the arrow keys and zooms with `+` and `-`; **Zoom
//!   in**, **Zoom out** and **Fit** do the same from buttons.
//! - **Other assets** as a two-column tree of fields (old | new), only the changed paths by
//!   default or every field with **Show unchanged fields**.
//! - **Diff against revision…** first lists the file's history (`git log --follow`); Enter or a
//!   click on an entry diffs against it.
//!
//! Nothing in the window edits the asset. The window state lives in egui memory (one window at a
//! time) and is drawn once per pass by whichever caller reaches [`show`] first (the Content
//! Browser or a graph panel), so it stays up while either is on screen.

use std::path::{Path, PathBuf};

use crate::asset_diff::{
    self, AssetDiff, ChangeEntry, ChangeKind, DiffBody, DiffError, GraphDiff, Revision, RonValue,
};
use crate::canvas_keys::{self, ListKeys, RowKey};

/// The egui memory key of the window state.
const WINDOW_KEY: &str = "regia.asset_diff.window";

/// How many history entries the picker lists.
pub const PICKER_LIMIT: usize = 100;

/// Zoom limits of the shared graph view.
pub const ZOOM_RANGE: (f32, f32) = (0.2, 3.0);

/// Node box size on the canvas at zoom 1, in points.
pub const NODE_SIZE: [f32; 2] = [150.0, 40.0];

/// Auto-layout pitch (canvas units) for nodes without an authored position.
const AUTO_PITCH: [f32; 2] = [210.0, 70.0];

/// How far one arrow key press pans a focused canvas, in points.
const KEY_PAN: f32 = 40.0;

/// What the window shows.
#[derive(Clone, Debug, Default)]
pub enum DiffContent {
    /// Nothing yet.
    #[default]
    Empty,
    /// The revision picker: the file's history, newest first.
    Picker(Vec<Revision>),
    /// A diff.
    Diff(Box<AssetDiff>),
    /// Why there is no diff ("No revision control", a parse error…).
    Error(DiffError),
}

/// The pan and zoom both graph canvases share.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphView {
    /// The canvas point shown at the middle of each canvas.
    pub center: [f32; 2],
    /// Points per canvas unit.
    pub zoom: f32,
}

impl Default for GraphView {
    fn default() -> Self {
        Self {
            center: [0.0, 0.0],
            zoom: 1.0,
        }
    }
}

impl GraphView {
    /// Pans by `delta` points on screen.
    pub fn pan(&mut self, delta: egui::Vec2) {
        self.center[0] -= delta.x / self.zoom;
        self.center[1] -= delta.y / self.zoom;
    }

    /// Multiplies the zoom by `factor`, inside [`ZOOM_RANGE`].
    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
    }

    /// The screen point of canvas point `p` in a canvas `rect`.
    #[must_use]
    pub fn to_screen(&self, rect: egui::Rect, p: [f32; 2]) -> egui::Pos2 {
        rect.center() + egui::vec2(p[0] - self.center[0], p[1] - self.center[1]) * self.zoom
    }

    /// Centers and zooms on the box spanning `points` in a canvas of `size` points.
    pub fn fit(&mut self, points: &[[f32; 2]], size: egui::Vec2) {
        let Some(first) = points.first() else {
            *self = Self::default();
            return;
        };
        let (mut min, mut max) = (*first, *first);
        for p in points {
            min = [min[0].min(p[0]), min[1].min(p[1])];
            max = [max[0].max(p[0]), max[1].max(p[1])];
        }
        let w = (max[0] - min[0] + NODE_SIZE[0] * 1.5).max(1.0);
        let h = (max[1] - min[1] + NODE_SIZE[1] * 2.0).max(1.0);
        self.center = [(min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5];
        self.zoom = (size.x / w).min(size.y / h).clamp(ZOOM_RANGE.0, 1.0);
    }
}

/// The diff window's state (egui memory, one window at a time).
#[derive(Clone, Debug)]
pub struct DiffWindow {
    /// Whether the window is open.
    pub open: bool,
    /// The project root the path is relative to.
    pub root: PathBuf,
    /// The project-relative asset path.
    pub path: String,
    /// What the window shows.
    pub content: DiffContent,
    /// The shared pan and zoom of both graph canvases.
    pub view: GraphView,
    /// The selected change list entry.
    pub selected: Option<usize>,
    /// The tree view lists every field, not only changed ones.
    pub show_unchanged: bool,
    /// Keyboard state of the change list.
    pub keys: ListKeys,
    /// Keyboard state of the revision picker.
    pub picker_keys: ListKeys,
    /// The canvas size of the last frame (for **Fit**).
    canvas_size: egui::Vec2,
    /// Fit the view on the next frame (a new diff).
    fit_pending: bool,
    /// The pass this window was last drawn in (one draw per pass, ADR-7614's dialog rule).
    drawn_pass: u64,
    /// The egui frame whose pan and zoom input each source (toolbar, old canvas, new canvas)
    /// already applied (review sprint-w12-rev-g7 #3).
    view_frames: [Option<u64>; 3],
}

/// Where a pan or zoom comes from: one gate per source and egui frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewSource {
    /// The Zoom in / Zoom out buttons.
    Toolbar,
    /// A graph canvas.
    Canvas(Side),
}

impl DiffWindow {
    fn new(root: &Path, path: &str) -> Self {
        Self {
            open: true,
            root: root.to_path_buf(),
            path: path.to_string(),
            content: DiffContent::Empty,
            view: GraphView::default(),
            selected: None,
            show_unchanged: false,
            keys: ListKeys::default(),
            picker_keys: ListKeys::default(),
            canvas_size: egui::vec2(400.0, 300.0),
            fit_pending: true,
            drawn_pass: u64::MAX,
            view_frames: [None; 3],
        }
    }

    /// Whether `source`'s pan and zoom input may apply in egui frame `frame` (it then stamps it):
    /// egui's discarded second pass of a frame sees the same drag, scroll and click, which must
    /// move the view once.
    pub fn view_input_once(&mut self, source: ViewSource, frame: u64) -> bool {
        let slot = match source {
            ViewSource::Toolbar => 0,
            ViewSource::Canvas(Side::Old) => 1,
            ViewSource::Canvas(Side::New) => 2,
        };
        if self.view_frames[slot] == Some(frame) {
            return false;
        }
        self.view_frames[slot] = Some(frame);
        true
    }

    /// A window diffing `path` between `rev` (default `HEAD`) and the working copy.
    #[must_use]
    pub fn for_diff(root: &Path, path: &str, rev: Option<&str>) -> Self {
        let mut w = Self::new(root, path);
        w.run_diff(rev.unwrap_or("HEAD"));
        w
    }

    /// A window listing the history of `path` to pick a revision from.
    #[must_use]
    pub fn for_picker(root: &Path, path: &str) -> Self {
        let mut w = Self::new(root, path);
        w.content = match asset_diff::asset_revisions(root, path, PICKER_LIMIT) {
            Ok(list) => DiffContent::Picker(list),
            Err(e) => DiffContent::Error(e),
        };
        w
    }

    /// A window over an already computed diff (the revision control panel, tests).
    #[must_use]
    pub fn for_result(root: &Path, diff: AssetDiff) -> Self {
        let mut w = Self::new(root, &diff.path);
        w.content = DiffContent::Diff(Box::new(diff));
        w
    }

    /// Diffs against `rev` (and the working copy) and shows the result.
    pub fn run_diff(&mut self, rev: &str) {
        self.content = match asset_diff::diff_asset(&self.root, &self.path, Some(rev), None) {
            Ok(diff) => DiffContent::Diff(Box::new(diff)),
            Err(e) => DiffContent::Error(e),
        };
        self.selected = None;
        self.keys = ListKeys::default();
        self.fit_pending = true;
    }

    /// The diff shown, if any.
    #[must_use]
    pub fn diff(&self) -> Option<&AssetDiff> {
        match &self.content {
            DiffContent::Diff(d) => Some(d),
            _ => None,
        }
    }

    /// The change list entries of the shown diff.
    #[must_use]
    pub fn entries(&self) -> Vec<ChangeEntry> {
        self.diff().map(|d| d.body.entries()).unwrap_or_default()
    }

    /// Selects change list entry `i` and, for a node or a wire, centers it in both canvases.
    pub fn activate(&mut self, i: usize) {
        self.selected = Some(i);
        let Some(diff) = self.diff() else {
            return;
        };
        let DiffBody::Graph(g) = &diff.body else {
            return;
        };
        let layout = Layout::of(g);
        let target = match self.entries().get(i) {
            Some(ChangeEntry::Node(n)) => layout.node_pos(*n, Side::New),
            Some(ChangeEntry::Wire(w)) => g.wires.get(*w).and_then(|w| {
                let side = if w.status == ChangeKind::Removed {
                    Side::Old
                } else {
                    Side::New
                };
                let a = layout.endpoint(g, &w.wire.from, side)?;
                let b = layout.endpoint(g, &w.wire.to, side)?;
                Some([(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5])
            }),
            _ => None,
        };
        if let Some(p) = target {
            self.view.center = [p[0] + NODE_SIZE[0] * 0.5, p[1] + NODE_SIZE[1] * 0.5];
        }
    }
}

/// The window state, if a diff window was opened.
#[must_use]
pub fn window_state(ctx: &egui::Context) -> Option<DiffWindow> {
    ctx.data(|d| d.get_temp(egui::Id::new(WINDOW_KEY)))
}

/// Replaces the window state (opens it when `w.open`).
pub fn set_window(ctx: &egui::Context, w: DiffWindow) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(WINDOW_KEY), w));
}

/// Opens the window on a diff of `path` against `rev` (default `HEAD`).
pub fn open_diff(ctx: &egui::Context, root: &Path, path: &str, rev: Option<&str>) {
    set_window(ctx, DiffWindow::for_diff(root, path, rev));
}

/// Opens the window on the revision picker of `path`.
pub fn open_picker(ctx: &egui::Context, root: &Path, path: &str) {
    set_window(ctx, DiffWindow::for_picker(root, path));
}

/// Opens the window with "No project" for a file that is not inside a project.
fn open_error(ctx: &egui::Context, path: &str, error: DiffError) {
    let mut w = DiffWindow::new(Path::new(""), path);
    w.content = DiffContent::Error(error);
    set_window(ctx, w);
}

/// The Content Browser menu items of asset `rel` (inside its context menu). The window opens on
/// a RON asset only; other files get no items.
pub fn content_menu_items(ui: &mut egui::Ui, root: Option<&Path>, rel: &str) {
    if asset_diff::AssetKind::of_path(rel).is_none() {
        return;
    }
    let head = crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_DIFF_HEAD).clicked();
    let pick =
        crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_DIFF_REVISION).clicked();
    if head || pick {
        match root {
            Some(root) if head => open_diff(ui.ctx(), root, rel, None),
            Some(root) => open_picker(ui.ctx(), root, rel),
            None => open_error(ui.ctx(), rel, DiffError::new("no_project", "No project is open")),
        }
        ui.close();
    }
}

/// A graph panel's diff buttons for its open file (`None`: unsaved, the buttons are greyed out),
/// plus the window itself. One call in the panel's file row.
pub fn graph_file_buttons(ui: &mut egui::Ui, file: Option<&Path>) {
    let enabled = file.is_some();
    let head = crate::agent_parity::action_enabled(ui, enabled, &crate::agent_parity::GRAPH_DIFF_HEAD, None)
        .on_hover_text("Compare the open graph's saved file with HEAD")
        .clicked();
    let pick =
        crate::agent_parity::action_enabled(ui, enabled, &crate::agent_parity::GRAPH_DIFF_REVISION, None)
            .on_hover_text("Pick a revision of the open graph's saved file to compare with")
            .clicked();
    if let Some(file) = file
        && (head || pick)
    {
        let found = asset_diff::project_root_of(file).map(|root| {
            let rel = asset_diff::project_relative(&root, &file.to_string_lossy());
            (root, rel)
        });
        let shown = file.display().to_string();
        match found {
            Some((root, Ok(rel))) if head => open_diff(ui.ctx(), &root, &rel, None),
            Some((root, Ok(rel))) => open_picker(ui.ctx(), &root, &rel),
            Some((_, Err(e))) => open_error(ui.ctx(), &shown, e),
            None => open_error(
                ui.ctx(),
                &shown,
                DiffError::new(
                    "no_project",
                    "The file is not inside a project (no Regia.toml above it)",
                ),
            ),
        }
    }
    show(ui.ctx());
}

/// Draws the diff window if it is open (once per pass, whoever calls first).
pub fn show(ctx: &egui::Context) {
    let Some(mut w) = window_state(ctx) else {
        return;
    };
    if !w.open {
        return;
    }
    let pass = ctx.cumulative_pass_nr();
    if w.drawn_pass == pass {
        return;
    }
    w.drawn_pass = pass;
    let mut open = true;
    egui::Window::new(format!("Diff: {}", w.path))
        .id(egui::Id::new(WINDOW_KEY))
        .open(&mut open)
        .default_size([960.0, 560.0])
        .resizable(true)
        .show(ctx, |ui| window_ui(ui, &mut w));
    w.open = w.open && open;
    set_window(ctx, w);
}

/// The window's contents (also what the panel tests draw directly).
pub fn window_ui(ui: &mut egui::Ui, w: &mut DiffWindow) {
    let _span = tracing::info_span!("diff_view.ui").entered();
    let tokens = crate::theme::ui_tokens(ui.ctx());
    match &w.content {
        DiffContent::Empty => {
            ui.weak("Nothing to show");
        }
        DiffContent::Error(e) => {
            ui.colored_label(tokens.error, crate::data_panels::sentence(&e.message))
                .on_hover_text(format!("Agent error code: {}", e.code));
        }
        DiffContent::Picker(list) => {
            let list = list.clone();
            picker_ui(ui, w, &list);
        }
        DiffContent::Diff(_) => {
            // Moved out for the draw (no per-frame clone); back unless the draw replaced it.
            let DiffContent::Diff(diff) = std::mem::take(&mut w.content) else {
                return;
            };
            let activated = diff_ui(ui, w, &diff);
            if matches!(w.content, DiffContent::Empty) {
                w.content = DiffContent::Diff(diff);
                if let Some(i) = activated {
                    w.activate(i);
                }
            }
        }
    }
}

fn picker_ui(ui: &mut egui::Ui, w: &mut DiffWindow, list: &[Revision]) {
    ui.label(format!(
        "Pick a revision of {} to compare with the working copy",
        w.path
    ));
    if list.is_empty() {
        ui.weak("This file has no committed history yet");
        return;
    }
    let names: Vec<String> = list
        .iter()
        .map(|r| {
            let renamed = if r.path == w.path {
                String::new()
            } else {
                format!(" (as {})", r.path)
            };
            format!("{} {} {}: {}{renamed}", r.short, r.date, r.author, r.subject)
        })
        .collect();
    let height = ui.available_height().max(120.0);
    let current = w.picker_keys.current;
    let events = canvas_keys::list_rows(
        ui,
        "asset-diff-picker",
        &mut w.picker_keys,
        names.len(),
        height,
        current,
        &|i| names[i].clone(),
    );
    for event in events {
        if let RowKey::Activate(i) = event
            && let Some(rev) = list.get(i)
        {
            let hash = rev.hash.clone();
            w.run_diff(&hash);
            return;
        }
    }
}

fn status_color(tokens: &crate::theme::ThemeTokens, status: ChangeKind) -> egui::Color32 {
    match status {
        ChangeKind::Added => tokens.accents.ok,
        ChangeKind::Removed => tokens.error,
        ChangeKind::Changed => tokens.warn,
        ChangeKind::Unchanged => tokens.muted,
    }
}

/// Draws a diff; returns the change list entry activated this frame.
fn diff_ui(ui: &mut egui::Ui, w: &mut DiffWindow, diff: &AssetDiff) -> Option<usize> {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let entries = diff.body.entries();
    ui.horizontal_wrapped(|ui| {
        ui.strong(diff.old.label());
        if diff.old.path != diff.path {
            ui.weak(format!("({})", diff.old.path));
        }
        ui.label("→");
        ui.strong(diff.new.label());
        ui.separator();
        let count = |k: ChangeKind| entries.iter().filter(|e| diff.body.status(e) == k).count();
        ui.colored_label(tokens.accents.ok, format!("{} added", count(ChangeKind::Added)));
        ui.colored_label(tokens.error, format!("{} removed", count(ChangeKind::Removed)));
        ui.colored_label(tokens.warn, format!("{} changed", count(ChangeKind::Changed)));
        ui.separator();
        if crate::agent_parity::action_button(ui, &crate::agent_parity::DIFF_PICK_ANOTHER).clicked() {
            let picker = DiffWindow::for_picker(&w.root, &w.path);
            w.content = picker.content;
            w.picker_keys = ListKeys::default();
        }
        ui.weak("Read-only");
    });
    ui.separator();
    let list_width = (ui.available_width() * 0.3).clamp(180.0, 360.0);
    let names: Vec<String> = entries.iter().map(|e| diff.body.describe(e)).collect();
    let mut activated = None;
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(list_width);
            ui.label(format!("Changes ({})", entries.len()));
            if entries.is_empty() {
                ui.weak("No differences");
            }
            let height = ui.available_height().max(120.0);
            let events = canvas_keys::list_rows(
                ui,
                "asset-diff-changes",
                &mut w.keys,
                names.len(),
                height,
                w.selected,
                &|i| names[i].clone(),
            );
            for event in events {
                if let RowKey::Activate(i) = event {
                    activated = Some(i);
                }
            }
        });
        ui.separator();
        ui.vertical(|ui| match &diff.body {
            DiffBody::Graph(g) => graph_pane(ui, w, diff, g, &entries),
            DiffBody::Tree { changes, old, new } => tree_pane(ui, w, diff, changes, old, new),
        });
    });
    activated
}

// ---------------------------------------------------------------------------
// Graph canvases
// ---------------------------------------------------------------------------

/// Which side of the diff a canvas shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The old revision (left).
    Old,
    /// The new revision (right).
    New,
}

/// Where each node of a graph diff sits on each side: its authored position when it has one
/// (the other side's when only that has one), else an automatic layout by wire depth shared by
/// both sides (so an unmoved node is in the same place left and right).
#[derive(Clone, Debug)]
pub struct Layout {
    auto: Vec<[f32; 2]>,
    old: Vec<Option<[f32; 2]>>,
    new: Vec<Option<[f32; 2]>>,
    /// Endpoints that are not nodes (a material graph's `output`), with their place.
    pseudo: Vec<(String, [f32; 2])>,
}

impl Layout {
    /// The layout of `g`.
    #[must_use]
    pub fn of(g: &GraphDiff) -> Self {
        let n = g.nodes.len();
        let index = |id: &str, side: Side| {
            g.nodes.iter().position(|node| {
                let own = match side {
                    Side::Old => node.old_id.as_deref(),
                    Side::New => node.new_id.as_deref(),
                };
                own == Some(id)
            })
        };
        // Edges between node indices over both sides' wires; endpoints that are not nodes go to
        // a pseudo list.
        let mut edges = Vec::new();
        let mut pseudo_names: Vec<String> = Vec::new();
        for w in &g.wires {
            let side = if w.status == ChangeKind::Removed {
                Side::Old
            } else {
                Side::New
            };
            match (index(&w.wire.from, side), index(&w.wire.to, side)) {
                (Some(a), Some(b)) if a != b => edges.push((a, b)),
                (Some(_), None) if !pseudo_names.contains(&w.wire.to) => pseudo_names.push(w.wire.to.clone()),
                _ => {}
            }
        }
        // Longest-path depth, relaxed at most n times (a cycle cannot loop forever).
        let mut depth = vec![0usize; n];
        for _ in 0..n {
            let mut moved = false;
            for &(a, b) in &edges {
                if depth[b] < depth[a] + 1 && depth[a] < n {
                    depth[b] = depth[a] + 1;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        let mut rows = vec![0usize; n + 1];
        let mut auto = Vec::with_capacity(n);
        for d in &depth {
            let r = rows[(*d).min(n)];
            rows[(*d).min(n)] += 1;
            auto.push([*d as f32 * AUTO_PITCH[0], r as f32 * AUTO_PITCH[1]]);
        }
        let max_depth = depth.iter().copied().max().unwrap_or(0);
        let pseudo = pseudo_names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                (
                    name,
                    [(max_depth + 1) as f32 * AUTO_PITCH[0], i as f32 * AUTO_PITCH[1]],
                )
            })
            .collect();
        Self {
            auto,
            old: g.nodes.iter().map(|x| x.old_pos.or(x.new_pos)).collect(),
            new: g.nodes.iter().map(|x| x.new_pos.or(x.old_pos)).collect(),
            pseudo,
        }
    }

    /// The top-left canvas point of node `i` on `side`.
    #[must_use]
    pub fn node_pos(&self, i: usize, side: Side) -> Option<[f32; 2]> {
        let authored = match side {
            Side::Old => self.old.get(i).copied().flatten(),
            Side::New => self.new.get(i).copied().flatten(),
        };
        authored.or_else(|| self.auto.get(i).copied())
    }

    /// The top-left canvas point of wire endpoint `id` on `side` (a node or a pseudo endpoint).
    #[must_use]
    pub fn endpoint(&self, g: &GraphDiff, id: &str, side: Side) -> Option<[f32; 2]> {
        let i = g.nodes.iter().position(|n| {
            let own = match side {
                Side::Old => n.old_id.as_deref(),
                Side::New => n.new_id.as_deref(),
            };
            own == Some(id)
        });
        match i {
            Some(i) => self.node_pos(i, side),
            None => self.pseudo.iter().find(|(n, _)| n == id).map(|(_, p)| *p),
        }
    }

    /// Every placed point (for **Fit**).
    #[must_use]
    pub fn points(&self) -> Vec<[f32; 2]> {
        let mut out: Vec<[f32; 2]> = (0..self.auto.len())
            .filter_map(|i| self.node_pos(i, Side::New))
            .collect();
        out.extend(self.pseudo.iter().map(|(_, p)| *p));
        out
    }
}

fn graph_pane(
    ui: &mut egui::Ui,
    w: &mut DiffWindow,
    diff: &AssetDiff,
    g: &GraphDiff,
    entries: &[ChangeEntry],
) {
    let layout = Layout::of(g);
    ui.horizontal(|ui| {
        let zoom_in = crate::agent_parity::action_button(ui, &crate::agent_parity::DIFF_ZOOM_IN).clicked();
        let zoom_out = crate::agent_parity::action_button(ui, &crate::agent_parity::DIFF_ZOOM_OUT).clicked();
        if (zoom_in || zoom_out) && w.view_input_once(ViewSource::Toolbar, ui.ctx().cumulative_frame_nr()) {
            w.view.zoom_by(if zoom_in { 1.25 } else { 0.8 });
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::DIFF_FIT).clicked() {
            w.fit_pending = true;
        }
        ui.weak(format!("Zoom {:.0} %", w.view.zoom * 100.0));
    });
    let gap = ui.spacing().item_spacing.x;
    let width = ((ui.available_width() - gap) * 0.5).max(80.0);
    let height = ui.available_height().max(160.0);
    let size = egui::vec2(width, height);
    w.canvas_size = size;
    if w.fit_pending {
        w.view.fit(&layout.points(), size);
        w.fit_pending = false;
    }
    let selected_node = w.selected.and_then(|i| match entries.get(i) {
        Some(ChangeEntry::Node(n)) => Some(*n),
        _ => None,
    });
    let mut clicked = None;
    ui.horizontal_top(|ui| {
        for side in [Side::Old, Side::New] {
            ui.vertical(|ui| {
                let label = match side {
                    Side::Old => diff.old.label(),
                    Side::New => diff.new.label(),
                };
                ui.label(label.clone());
                let pane = CanvasPane {
                    side,
                    size,
                    selected_node,
                    label: &label,
                };
                if let Some(node) = canvas(ui, w, g, &layout, &pane) {
                    clicked = Some(node);
                }
            });
        }
    });
    if let Some(node) = clicked
        && let Some(i) = entries.iter().position(|e| *e == ChangeEntry::Node(node))
    {
        w.selected = Some(i);
        w.keys.retarget(Some(i));
    }
}

/// What one canvas shows: its side, size, the selected node and its revision label.
struct CanvasPane<'a> {
    side: Side,
    size: egui::Vec2,
    selected_node: Option<usize>,
    label: &'a str,
}

/// One painted graph canvas; returns the node clicked, if any.
fn canvas(
    ui: &mut egui::Ui,
    w: &mut DiffWindow,
    g: &GraphDiff,
    layout: &Layout,
    pane: &CanvasPane<'_>,
) -> Option<usize> {
    let CanvasPane {
        side,
        size,
        selected_node,
        label,
    } = *pane;
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let shown = g
        .nodes
        .iter()
        .filter(|n| match side {
            Side::Old => n.status != ChangeKind::Added,
            Side::New => n.status != ChangeKind::Removed,
        })
        .count();
    let side_name = match side {
        Side::Old => "Old",
        Side::New => "New",
    };
    canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("{side_name} graph ({label}), {shown} nodes"),
    );
    // Shared pan and zoom: either canvas moves both, once per egui frame (review
    // sprint-w12-rev-g7 #3: a discarded second pass repeats the gesture).
    let apply = w.view_input_once(ViewSource::Canvas(side), ui.ctx().cumulative_frame_nr());
    if apply && response.dragged() {
        w.view.pan(response.drag_delta());
    }
    if apply && response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            w.view.zoom_by((scroll * 0.002).exp());
        }
    }
    if apply && response.has_focus() {
        use egui::{Key, Modifiers};
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            );
        });
        let mut pan = egui::Vec2::ZERO;
        let mut zoom = 1.0;
        ui.input_mut(|input| {
            for (key, d) in [
                (Key::ArrowLeft, egui::vec2(KEY_PAN, 0.0)),
                (Key::ArrowRight, egui::vec2(-KEY_PAN, 0.0)),
                (Key::ArrowUp, egui::vec2(0.0, KEY_PAN)),
                (Key::ArrowDown, egui::vec2(0.0, -KEY_PAN)),
            ] {
                if input.consume_key(Modifiers::NONE, key) {
                    pan += d;
                }
            }
            if input.consume_key(Modifiers::NONE, Key::Plus)
                || input.consume_key(Modifiers::NONE, Key::Equals)
            {
                zoom *= 1.25;
            }
            if input.consume_key(Modifiers::NONE, Key::Minus) {
                zoom *= 0.8;
            }
        });
        w.view.pan(pan);
        w.view.zoom_by(zoom);
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, tokens.well);
    let stroke_w = if response.has_focus() { 2.0 } else { 1.0 };
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(
            stroke_w,
            if response.has_focus() {
                tokens.accents.select
            } else {
                tokens.line
            },
        ),
        egui::StrokeKind::Inside,
    );
    let view = w.view;
    let node_size = egui::vec2(NODE_SIZE[0], NODE_SIZE[1]) * view.zoom;
    let body_font = egui::TextStyle::Body.resolve(ui.style());
    let font = egui::FontId::new(
        (body_font.size * view.zoom).clamp(4.0, 40.0),
        body_font.family.clone(),
    );
    let small = egui::FontId::new(
        (body_font.size * 0.8 * view.zoom).clamp(4.0, 32.0),
        body_font.family,
    );
    // Wires first, under the nodes.
    for wire in &g.wires {
        let on_side = match side {
            Side::Old => wire.status != ChangeKind::Added,
            Side::New => wire.status != ChangeKind::Removed,
        };
        if !on_side {
            continue;
        }
        // Unchanged wires carry new ids; place them by the new side's ids on both canvases.
        let id_side = if wire.status == ChangeKind::Removed {
            Side::Old
        } else {
            Side::New
        };
        let (Some(a), Some(b)) = (
            layout.endpoint(g, &wire.wire.from, id_side),
            layout.endpoint(g, &wire.wire.to, id_side),
        ) else {
            continue;
        };
        // Draw at this side's positions: re-resolve each endpoint's node on this side.
        let place = |id: &str, fallback: [f32; 2]| {
            g.nodes
                .iter()
                .position(|n| match id_side {
                    Side::Old => n.old_id.as_deref() == Some(id),
                    Side::New => n.new_id.as_deref() == Some(id),
                })
                .and_then(|i| layout.node_pos(i, side))
                .unwrap_or(fallback)
        };
        let a = place(&wire.wire.from, a);
        let b = place(&wire.wire.to, b);
        let from = view.to_screen(rect, a) + egui::vec2(node_size.x, node_size.y * 0.5);
        let to = view.to_screen(rect, b) + egui::vec2(0.0, node_size.y * 0.5);
        let width = if wire.status == ChangeKind::Unchanged {
            1.0
        } else {
            2.0
        };
        painter.line_segment(
            [from, to],
            egui::Stroke::new(width, status_color(&tokens, wire.status)),
        );
    }
    for (name, p) in &layout.pseudo {
        let r = egui::Rect::from_min_size(view.to_screen(rect, *p), node_size);
        painter.rect_stroke(
            r,
            4.0,
            egui::Stroke::new(1.0, tokens.muted),
            egui::StrokeKind::Inside,
        );
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            name,
            font.clone(),
            tokens.muted,
        );
    }
    let mut hit = None;
    let pointer = response.interact_pointer_pos();
    for (i, node) in g.nodes.iter().enumerate() {
        let visible = match side {
            Side::Old => node.status != ChangeKind::Added,
            Side::New => node.status != ChangeKind::Removed,
        };
        let Some(p) = layout.node_pos(i, side).filter(|_| visible) else {
            continue;
        };
        let r = egui::Rect::from_min_size(view.to_screen(rect, p), node_size);
        if !rect.intersects(r) {
            continue;
        }
        let color = status_color(&tokens, node.status);
        let selected = selected_node == Some(i);
        painter.rect_filled(r, 4.0, tokens.raised);
        painter.rect_stroke(
            r,
            4.0,
            egui::Stroke::new(
                if selected { 3.0 } else { 1.5 },
                if selected { tokens.accents.select } else { color },
            ),
            egui::StrokeKind::Inside,
        );
        let text_color = if node.status == ChangeKind::Unchanged {
            tokens.muted
        } else {
            tokens.text
        };
        let id = match side {
            Side::Old => node.old_id.as_deref().unwrap_or(""),
            Side::New => node.new_id.as_deref().unwrap_or(""),
        };
        painter.text(
            r.left_top() + egui::vec2(6.0, 4.0) * view.zoom,
            egui::Align2::LEFT_TOP,
            id,
            font.clone(),
            text_color,
        );
        painter.text(
            r.left_bottom() + egui::vec2(6.0, -4.0) * view.zoom,
            egui::Align2::LEFT_BOTTOM,
            &node.kind,
            small.clone(),
            color,
        );
        if response.clicked() && pointer.is_some_and(|q| r.contains(q)) {
            hit = Some(i);
        }
    }
    hit
}

// ---------------------------------------------------------------------------
// Tree view
// ---------------------------------------------------------------------------

/// Every leaf of `v` as `(path, value)`, with the paths [`asset_diff::diff_values`] uses.
#[must_use]
pub fn leaves(v: &RonValue) -> Vec<(String, String)> {
    let mut out = Vec::new();
    collect_leaves("", v, &mut out);
    out
}

fn collect_leaves(path: &str, v: &RonValue, out: &mut Vec<(String, String)>) {
    let join = |seg: &str| {
        if path.is_empty() {
            seg.to_string()
        } else {
            format!("{path}.{seg}")
        }
    };
    if v.is_leaf() {
        out.push((path.to_string(), v.summary(160)));
        return;
    }
    match v {
        RonValue::Struct { fields, .. } => {
            for (k, x) in fields {
                collect_leaves(&join(k), x, out);
            }
        }
        RonValue::Tuple { items, .. } => {
            for (i, x) in items.iter().enumerate() {
                collect_leaves(&join(&i.to_string()), x, out);
            }
        }
        RonValue::Seq(items) => {
            for (i, x) in items.iter().enumerate() {
                collect_leaves(&format!("{path}[{i}]"), x, out);
            }
        }
        RonValue::Map(entries) => {
            for (k, x) in entries {
                collect_leaves(&format!("{path}[{}]", k.summary(60)), x, out);
            }
        }
        _ => out.push((path.to_string(), v.summary(160))),
    }
}

/// One row of the two-column tree view.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    /// The field path.
    pub path: String,
    /// The old value (empty when added).
    pub old: String,
    /// The new value (empty when removed).
    pub new: String,
    /// What happened to it.
    pub status: ChangeKind,
}

/// The rows of the tree view: the changed paths, or (with `all`) every leaf of both sides with
/// the changed ones marked.
#[must_use]
pub fn tree_rows(
    changes: &[asset_diff::PathChange],
    old: &RonValue,
    new: &RonValue,
    all: bool,
) -> Vec<TreeRow> {
    let summary = |v: &Option<RonValue>| v.as_ref().map(|x| x.summary(160)).unwrap_or_default();
    if !all {
        return changes
            .iter()
            .map(|c| TreeRow {
                path: c.path.clone(),
                old: summary(&c.old),
                new: summary(&c.new),
                status: c.kind,
            })
            .collect();
    }
    let status_of = |path: &str| {
        changes
            .iter()
            .find(|c| {
                c.path == path
                    || c.path.is_empty()
                    || path.starts_with(&format!("{}.", c.path))
                    || path.starts_with(&format!("{}[", c.path))
            })
            .map_or(ChangeKind::Unchanged, |c| c.kind)
    };
    let a = leaves(old);
    let b = leaves(new);
    let mut rows: Vec<TreeRow> = b
        .iter()
        .map(|(path, value)| TreeRow {
            path: path.clone(),
            old: a
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, v)| v.clone())
                .unwrap_or_default(),
            new: value.clone(),
            status: status_of(path),
        })
        .collect();
    for (path, value) in &a {
        if !b.iter().any(|(p, _)| p == path) {
            rows.push(TreeRow {
                path: path.clone(),
                old: value.clone(),
                new: String::new(),
                status: status_of(path),
            });
        }
    }
    rows
}

fn tree_pane(
    ui: &mut egui::Ui,
    w: &mut DiffWindow,
    diff: &AssetDiff,
    changes: &[asset_diff::PathChange],
    old: &RonValue,
    new: &RonValue,
) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    crate::agent_parity::action_toggle(
        ui,
        &crate::agent_parity::DIFF_SHOW_UNCHANGED,
        &mut w.show_unchanged,
    );
    let rows = tree_rows(changes, old, new, w.show_unchanged);
    let selected_path = w.selected.and_then(|i| changes.get(i)).map(|c| c.path.clone());
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y;
    let col = ((ui.available_width() - 16.0) / 3.0).max(80.0);
    egui::Grid::new("asset-diff-tree-head")
        .num_columns(3)
        .min_col_width(col)
        .show(ui, |ui| {
            ui.strong("Field");
            ui.strong(diff.old.label());
            ui.strong(diff.new.label());
            ui.end_row();
        });
    egui::ScrollArea::both()
        .id_salt("asset-diff-tree")
        .auto_shrink([false, false])
        .show_rows(ui, row_h, rows.len(), |ui, range| {
            egui::Grid::new("asset-diff-tree-rows")
                .num_columns(3)
                .min_col_width(col)
                .striped(true)
                .show(ui, |ui| {
                    for row in &rows[range] {
                        let color = status_color(&tokens, row.status);
                        let path = if row.path.is_empty() {
                            "(document)"
                        } else {
                            row.path.as_str()
                        };
                        let label = ui.colored_label(color, path);
                        if selected_path.as_deref() == Some(row.path.as_str()) {
                            label.scroll_to_me(Some(egui::Align::Center));
                        }
                        let text = |s: &str| {
                            if s.is_empty() {
                                "—".to_string()
                            } else {
                                s.to_string()
                            }
                        };
                        let (old_color, new_color) = match row.status {
                            ChangeKind::Unchanged => (tokens.muted, tokens.muted),
                            ChangeKind::Removed => (tokens.error, tokens.muted),
                            ChangeKind::Added => (tokens.muted, tokens.accents.ok),
                            ChangeKind::Changed => (tokens.warn, tokens.warn),
                        };
                        ui.colored_label(old_color, text(&row.old));
                        ui.colored_label(new_color, text(&row.new));
                        ui.end_row();
                    }
                });
        });
}

#[cfg(test)]
mod tests;
