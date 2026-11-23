// SPDX-License-Identifier: MPL-2.0
//! The UI Inspector panel (`docs/specs/ui-parity-roadmap.md` row E04, ADR-2703, book page
//! `agents/ui-inspector.md`): the runtime UI tree of a play-in-editor world or of a running game,
//! with search, a layout map you hover to pick a node, and a property table that shows which
//! source (default, theme layer, inline, animation, binding) won each property.
//!
//! The panel is a client of the `ui.query`, `ui.inspect`, `ui.pick_at` and `ui.highlight` agent
//! methods. [`InspectorSource`] is the seam: [`WorldSource`] runs them against the PIE world in
//! process, [`RemoteSource`] over the agent socket of a running game
//! ([`regia_agent::client::Client`]). All the state that matters ([`InspectorModel`]: tree
//! flattening, search, selection, the refresh rate cap) is plain data and unit tested without egui.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use regia_ecs::prelude::*;
use serde_json::{Value, json};

/// Fastest and slowest live refresh rate (Hz) the panel allows.
pub const REFRESH_HZ_RANGE: (f64, f64) = (0.5, 10.0);

/// Most nodes the panel asks for in one refresh (the protocol's `ui.query` limit).
const QUERY_LIMIT: u64 = 1000;

/// Where the panel's UI data comes from: it runs one agent method and returns its result.
pub trait InspectorSource {
    /// Runs `method` (`ui.query`, `ui.inspect`, `ui.pick_at`, `ui.highlight`).
    ///
    /// # Errors
    /// The protocol error text, or why the connection failed.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
}

/// The PIE world, in process.
pub struct WorldSource<'a>(pub &'a mut World);

impl InspectorSource for WorldSource<'_> {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let world = &mut *self.0;
        let node = |params: &Value| {
            params
                .get("node")
                .and_then(Value::as_u64)
                .and_then(Entity::try_from_bits)
                .filter(|e| world_has_node(world, *e))
                .ok_or_else(|| "ui_node_not_found".to_owned())
        };
        match method {
            "ui.query" => {
                regia_ui::agent_bridge::call(world, method, &params).map_err(|e| e.reason.to_owned())
            }
            "ui.inspect" => {
                let e = node(&params)?;
                Ok(regia_ui::inspect::inspect(world, e))
            }
            "ui.inspect_events" => {
                let e = node(&params)?;
                Ok(regia_ui::inspect::inspect_events(world, e))
            }
            "ui.set_live_style" => {
                let e = node(&params)?;
                let field = params
                    .get("field")
                    .and_then(Value::as_str)
                    .ok_or("ui_bad_param")?;
                let value = params.get("value").cloned().ok_or("ui_bad_param")?;
                regia_ui::inspect::set_live_style(world, e, field, value)
                    .map_err(|e| format!("{}: {}", e.reason, e.message))
            }
            "ui.pick_at" => {
                let x = params.get("x").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                let y = params.get("y").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                let top = regia_ui::inspect::pick_at(world, [x, y], false);
                Ok(json!({ "node": top.map(Entity::to_bits) }))
            }
            "ui.highlight" => {
                let target = if params.get("clear").and_then(Value::as_bool) == Some(true) {
                    None
                } else {
                    Some(node(&params)?)
                };
                let mut overlay =
                    world.get_resource_or_insert_with(regia_ui::debug_overlay::UiDebugOverlay::default);
                overlay.highlight = target;
                overlay.enabled = overlay.enabled || target.is_some();
                world
                    .get_resource_or_insert_with(regia_ui::extract::UiDamageSet::default)
                    .full = true;
                world
                    .get_resource_or_insert_with(regia_ui::extract::UiDamage::default)
                    .0 = true;
                Ok(json!({ "highlight": target.map(Entity::to_bits) }))
            }
            other => Err(format!("`{other}` is not an inspector method")),
        }
    }
}

fn world_has_node(world: &World, e: Entity) -> bool {
    world.get_entity(e).is_ok() && regia_ui::inspect::is_ui_node(world, e)
}

/// A running game, over its agent socket.
pub struct RemoteSource {
    client: regia_agent::client::Client,
    project_root: std::path::PathBuf,
}

impl RemoteSource {
    /// Connects to the agent endpoint the app in `project_dir` advertises.
    ///
    /// # Errors
    /// The client error text when no app runs there or the connection fails.
    pub fn connect(project_dir: &std::path::Path) -> Result<Self, String> {
        let project_root = project_dir.canonicalize().map_err(|e| e.to_string())?;
        let mut client =
            regia_agent::client::Client::connect_project(&project_root).map_err(|e| e.to_string())?;
        client
            .set_read_timeout(Some(Duration::from_millis(750)))
            .map_err(|e| e.to_string())?;
        Ok(Self { client, project_root })
    }
}

impl InspectorSource for RemoteSource {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.client.call(method, params).map_err(|e| e.to_string())
    }
}

/// One node of the tree, as `ui.query` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeInfo {
    /// Node id (entity bits).
    pub id: u64,
    /// Parent id.
    pub parent: Option<u64>,
    /// `NodeName`, if any.
    pub name: Option<String>,
    /// Widget kind.
    pub kind: String,
    /// Style classes.
    pub classes: Vec<String>,
    /// Text the node shows.
    pub text: Option<String>,
    /// Layout rect `[x, y, w, h]` in physical px.
    pub rect: [f32; 4],
}

impl NodeInfo {
    /// Reads one entry of a `ui.query` result; `None` when it has no id.
    #[must_use]
    pub fn from_query(node: &Value) -> Option<Self> {
        let rect = node
            .get("rect")
            .and_then(Value::as_array)
            .map(|a| {
                let mut r = [0.0_f32; 4];
                for (slot, v) in r.iter_mut().zip(a) {
                    *slot = v.as_f64().unwrap_or(0.0) as f32;
                }
                r
            })
            .unwrap_or_default();
        Some(Self {
            id: node.get("id")?.as_u64()?,
            parent: node.get("parent").and_then(Value::as_u64),
            name: node.get("name").and_then(Value::as_str).map(str::to_owned),
            kind: node
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("Node")
                .to_owned(),
            classes: node
                .get("classes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
            text: node.get("text").and_then(Value::as_str).map(str::to_owned),
            rect,
        })
    }

    /// The label the tree shows: `Kind "name" "text"`.
    #[must_use]
    pub fn label(&self) -> String {
        let mut out = self.kind.clone();
        if let Some(name) = &self.name {
            out.push_str(&format!(" #{name}"));
        }
        if let Some(text) = self.text.as_ref().filter(|t| !t.is_empty()) {
            let short: String = text.chars().take(24).collect();
            out.push_str(&format!(" \"{short}\""));
        }
        out
    }

    fn matches(&self, needle: &str) -> bool {
        let hit = |s: &str| s.to_lowercase().contains(needle);
        hit(&self.kind)
            || self.name.as_deref().is_some_and(hit)
            || self.text.as_deref().is_some_and(hit)
            || self.classes.iter().any(|c| hit(c))
            || self.id.to_string() == needle
    }
}

/// One visible row of the flattened tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Node id.
    pub id: u64,
    /// Indent level.
    pub depth: usize,
    /// Whether the node has children.
    pub has_children: bool,
    /// Whether the children are shown.
    pub expanded: bool,
    /// Text to draw.
    pub label: String,
}

/// The panel's state, apart from egui: the tree, search, selection, expansion and refresh clock.
#[derive(Debug, Clone)]
pub struct InspectorModel {
    nodes: Vec<NodeInfo>,
    children: HashMap<Option<u64>, Vec<usize>>,
    /// Search text (case-insensitive; matches kind, name, text, class or id).
    pub search: String,
    /// The selected node.
    pub selected: Option<u64>,
    collapsed: HashSet<u64>,
    refresh_hz: f64,
    last_refresh: Option<f64>,
}

impl Default for InspectorModel {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            children: HashMap::new(),
            search: String::new(),
            selected: None,
            collapsed: HashSet::new(),
            refresh_hz: 4.0,
            last_refresh: None,
        }
    }
}

impl InspectorModel {
    /// The live refresh rate in Hz.
    #[must_use]
    pub fn refresh_hz(&self) -> f64 {
        self.refresh_hz
    }

    /// Sets the refresh rate, clamped to [`REFRESH_HZ_RANGE`] (a NaN keeps the old rate).
    pub fn set_refresh_hz(&mut self, hz: f64) {
        if hz.is_finite() {
            self.refresh_hz = hz.clamp(REFRESH_HZ_RANGE.0, REFRESH_HZ_RANGE.1);
        }
    }

    /// Whether a refresh is due at `now` (seconds on any steady clock). The first call is always
    /// due; later ones wait `1 / refresh_hz`.
    #[must_use]
    pub fn refresh_due(&self, now: f64) -> bool {
        self.last_refresh
            .is_none_or(|t| now - t >= 1.0 / self.refresh_hz || now < t)
    }

    /// Records that a refresh ran at `now`.
    pub fn mark_refreshed(&mut self, now: f64) {
        self.last_refresh = Some(now);
    }

    /// Replaces the tree from a `ui.query` result. The selection survives when its node still
    /// exists; otherwise it clears. Collapsed state is kept per node id.
    pub fn set_nodes(&mut self, nodes: Vec<NodeInfo>) {
        let ids: HashSet<u64> = nodes.iter().map(|n| n.id).collect();
        self.children.clear();
        for (i, n) in nodes.iter().enumerate() {
            // A parent outside the list (a truncated query) makes the node a root.
            let key = n.parent.filter(|p| ids.contains(p));
            self.children.entry(key).or_default().push(i);
        }
        if self.selected.is_some_and(|s| !ids.contains(&s)) {
            self.selected = None;
        }
        self.collapsed.retain(|id| ids.contains(id));
        self.nodes = nodes;
    }

    /// Number of nodes in the tree.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the tree is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// One node's data.
    #[must_use]
    pub fn node(&self, id: u64) -> Option<&NodeInfo> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Toggles whether a node's children show.
    pub fn toggle(&mut self, id: u64) {
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
    }

    /// The visible rows, preorder. With a search, only matching nodes and their ancestors show
    /// (and everything above a match is open); without, collapsed nodes hide their subtree.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let needle = self.search.trim().to_lowercase();
        let keep: Option<HashSet<u64>> = (!needle.is_empty()).then(|| self.matching_with_ancestors(&needle));
        let mut out = Vec::new();
        let mut stack: Vec<(usize, usize)> = self
            .children
            .get(&None)
            .map(|roots| roots.iter().rev().map(|i| (*i, 0)).collect())
            .unwrap_or_default();
        while let Some((i, depth)) = stack.pop() {
            let n = &self.nodes[i];
            if keep.as_ref().is_some_and(|k| !k.contains(&n.id)) {
                continue;
            }
            let kids = self.children.get(&Some(n.id));
            let visible_kids = kids.map_or(0, |k| {
                k.iter()
                    .filter(|c| {
                        keep.as_ref()
                            .is_none_or(|keep| keep.contains(&self.nodes[**c].id))
                    })
                    .count()
            });
            let expanded = keep.is_some() || !self.collapsed.contains(&n.id);
            out.push(Row {
                id: n.id,
                depth,
                has_children: visible_kids > 0,
                expanded: expanded && visible_kids > 0,
                label: n.label(),
            });
            if expanded && let Some(kids) = kids {
                stack.extend(kids.iter().rev().map(|c| (*c, depth + 1)));
            }
            if out.len() >= 100_000 {
                break;
            }
        }
        out
    }

    fn matching_with_ancestors(&self, needle: &str) -> HashSet<u64> {
        let by_id: HashMap<u64, &NodeInfo> = self.nodes.iter().map(|n| (n.id, n)).collect();
        let mut keep = HashSet::new();
        for n in self.nodes.iter().filter(|n| n.matches(needle)) {
            let mut cur = Some(n.id);
            while let Some(id) = cur {
                if !keep.insert(id) {
                    break;
                }
                cur = by_id.get(&id).and_then(|n| n.parent);
            }
        }
        keep
    }

    /// The overall size of the UI: the largest right and bottom edge of any root node.
    #[must_use]
    pub fn screen_size(&self) -> [f32; 2] {
        self.children
            .get(&None)
            .into_iter()
            .flatten()
            .map(|i| self.nodes[*i].rect)
            .fold([0.0, 0.0], |acc: [f32; 2], r| {
                [acc[0].max(r[0] + r[2]), acc[1].max(r[1] + r[3])]
            })
    }
}

/// Maps a point in the layout map (`origin`, `size` in egui points) to a screen position of a UI
/// that is `screen` px big. `None` outside the map or for an empty UI.
#[must_use]
pub fn map_to_screen(
    point: [f32; 2],
    origin: [f32; 2],
    size: [f32; 2],
    screen: [f32; 2],
) -> Option<[f32; 2]> {
    if size[0] <= 0.0 || size[1] <= 0.0 || screen[0] <= 0.0 || screen[1] <= 0.0 {
        return None;
    }
    let u = (point[0] - origin[0]) / size[0];
    let v = (point[1] - origin[1]) / size[1];
    ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v)).then(|| [u * screen[0], v * screen[1]])
}

/// What the panel is attached to.
#[derive(Default)]
pub enum Attachment {
    /// Nothing yet.
    #[default]
    Detached,
    /// The play-in-editor world.
    Pie,
    /// A running game's agent socket.
    Remote(Box<RemoteSource>),
}

/// The panel's persistent state (a resource in the editor world).
#[derive(Resource, Default)]
pub struct UiInspectorPanel {
    /// Tree, search, selection.
    pub model: InspectorModel,
    /// What the panel reads from.
    pub attachment: Attachment,
    /// Project directory typed for a remote attach.
    pub project_dir: String,
    /// Hover-to-pick mode on the layout map.
    pub pick_mode: bool,
    /// The last inspection of the selected node.
    pub inspection: Option<Value>,
    /// The last error, shown in the panel.
    pub error: Option<String>,
    last_highlight: Option<u64>,
    clock: f64,
    events_tab: bool,
    live_field: String,
    live_value: String,
    last_action_frame: Option<u64>,
}

impl UiInspectorPanel {
    /// Reloads the tree and the selected node's inspection from `source` when a refresh is due.
    /// Returns whether it ran.
    pub fn refresh(&mut self, source: &mut dyn InspectorSource, now: f64, force: bool) -> bool {
        if !force && !self.model.refresh_due(now) {
            return false;
        }
        self.model.mark_refreshed(now);
        match source.call("ui.query", json!({ "limit": QUERY_LIMIT })) {
            Ok(result) => {
                let nodes = result
                    .get("nodes")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(NodeInfo::from_query).collect())
                    .unwrap_or_default();
                self.model.set_nodes(nodes);
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e);
                return true;
            }
        }
        self.inspection = match self.model.selected {
            Some(id) => match source.call("ui.inspect", json!({ "node": id })) {
                Ok(v) => Some(v),
                Err(e) => {
                    self.error = Some(e);
                    None
                }
            },
            None => None,
        };
        true
    }

    /// Selects `id`, reads its inspection and highlights it in the game.
    pub fn select(&mut self, source: &mut dyn InspectorSource, id: Option<u64>) {
        if self.model.selected != id {
            self.live_field.clear();
            self.live_value.clear();
        }
        self.model.selected = id;
        self.inspection = None;
        if let Some(id) = id {
            match source.call("ui.inspect", json!({ "node": id })) {
                Ok(v) => self.inspection = Some(v),
                Err(e) => self.error = Some(e),
            }
        }
        self.highlight(source, id);
    }

    /// Outlines `id` in the game (or clears), once per change.
    pub fn highlight(&mut self, source: &mut dyn InspectorSource, id: Option<u64>) {
        if self.last_highlight == id {
            return;
        }
        self.last_highlight = id;
        let params = match id {
            Some(id) => json!({ "node": id }),
            None => json!({ "clear": true }),
        };
        if let Err(e) = source.call("ui.highlight", params) {
            self.error = Some(e);
        }
    }

    /// Picks the topmost node at the map point `point` and selects it. `None` when the point is
    /// outside the map.
    pub fn pick_on_map(
        &mut self,
        source: &mut dyn InspectorSource,
        point: [f32; 2],
        origin: [f32; 2],
        size: [f32; 2],
    ) -> Option<u64> {
        let at = map_to_screen(point, origin, size, self.model.screen_size())?;
        let picked = source
            .call("ui.pick_at", json!({ "x": at[0], "y": at[1] }))
            .ok()?
            .get("node")
            .and_then(Value::as_u64);
        if picked != self.model.selected {
            self.select(source, picked);
        }
        picked
    }
}

fn source_color(source: &str) -> egui::Color32 {
    match source {
        "theme" => egui::Color32::from_rgb(110, 170, 240),
        "inline" => egui::Color32::from_rgb(240, 180, 90),
        "animation" => egui::Color32::from_rgb(200, 130, 230),
        "binding" => egui::Color32::from_rgb(120, 210, 140),
        _ => egui::Color32::GRAY,
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Null => "-".to_owned(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Draws the UI Inspector. `play` is the PIE world while a session runs.
#[tracing::instrument(target = "regia_editor", skip_all, name = "ui_inspector_panel")]
pub fn ui_inspector_panel(ui: &mut egui::Ui, world: &mut World, play: Option<&mut World>) {
    let mut panel = world.remove_resource::<UiInspectorPanel>().unwrap_or_default();
    panel.clock = ui.input(|i| i.time);
    let now = panel.clock;
    ui.horizontal(|ui| {
        ui.strong("UI Inspector");
        let pie = matches!(panel.attachment, Attachment::Pie);
        if ui.selectable_label(pie, "PIE world").clicked() {
            panel.attachment = Attachment::Pie;
        }
        ui.add(
            egui::TextEdit::singleline(&mut panel.project_dir)
                .hint_text("project dir of a running game")
                .desired_width(180.0),
        );
        if ui.button("Attach").clicked() {
            match RemoteSource::connect(std::path::Path::new(&panel.project_dir)) {
                Ok(remote) => {
                    panel.attachment = Attachment::Remote(Box::new(remote));
                    panel.error = None;
                }
                Err(e) => panel.error = Some(e),
            }
        }
        let mut hz = panel.model.refresh_hz();
        if ui
            .add(
                egui::DragValue::new(&mut hz)
                    .range(REFRESH_HZ_RANGE.0..=REFRESH_HZ_RANGE.1)
                    .suffix(" Hz"),
            )
            .on_hover_text("live refresh rate")
            .changed()
        {
            panel.model.set_refresh_hz(hz);
        }
        ui.toggle_value(&mut panel.pick_mode, "Pick")
            .on_hover_text("hover the layout map to pick a node");
    });
    if let Some(e) = &panel.error {
        ui.colored_label(egui::Color32::LIGHT_RED, e);
    }
    let mut pie_source;
    let source: Option<&mut dyn InspectorSource> = match &mut panel.attachment {
        Attachment::Detached => None,
        Attachment::Pie => match play {
            Some(w) => {
                pie_source = WorldSource(w);
                Some(&mut pie_source as &mut dyn InspectorSource)
            }
            None => None,
        },
        Attachment::Remote(r) => Some(r.as_mut() as &mut dyn InspectorSource),
    };
    let Some(source) = source else {
        ui.label("Press Play, then choose \"PIE world\", or attach to a running game.");
        world.insert_resource(panel);
        return;
    };
    // Work on the panel's data apart from `attachment`, which `source` borrows.
    let mut work = UiInspectorPanel {
        model: std::mem::take(&mut panel.model),
        attachment: Attachment::Detached,
        project_dir: String::new(),
        pick_mode: panel.pick_mode,
        inspection: panel.inspection.take(),
        error: panel.error.take(),
        last_highlight: panel.last_highlight,
        clock: now,
        events_tab: panel.events_tab,
        live_field: std::mem::take(&mut panel.live_field),
        live_value: std::mem::take(&mut panel.live_value),
        last_action_frame: panel.last_action_frame,
    };
    work.refresh(source, now, false);
    ui.ctx()
        .request_repaint_after(Duration::from_secs_f64(1.0 / work.model.refresh_hz()));
    ui.add(
        egui::TextEdit::singleline(&mut work.model.search).hint_text("search kind, name, text, class, id"),
    );
    let mut clicked = None;
    let mut toggled = None;
    let mut picked_on_map = None;
    let mut copy = None;
    // The rows are flattened once per frame; only the ones in view are laid out (ADR-7306): a live
    // UI tree can hold thousands of nodes (inventory grids).
    let rows = work.model.rows();
    let row_height = ui.spacing().interact_size.y;
    ui.columns(2, |cols| {
        egui::ScrollArea::vertical()
            .id_salt("ui_inspector_tree")
            .show_rows(&mut cols[0], row_height, rows.len(), |ui, range| {
                for row in &rows[range] {
                    ui.horizontal(|ui| {
                        ui.add_space(row.depth as f32 * 12.0);
                        if row.has_children {
                            if ui.small_button(if row.expanded { "v" } else { ">" }).clicked() {
                                toggled = Some(row.id);
                            }
                        } else {
                            ui.add_space(18.0);
                        }
                        if ui
                            .selectable_label(work.model.selected == Some(row.id), &row.label)
                            .clicked()
                        {
                            clicked = Some(row.id);
                        }
                    });
                }
            });
        let ui = &mut cols[1];
        // The layout map: every node's rect, scaled to fit; hover picks the topmost node.
        let screen = work.model.screen_size();
        if screen[0] > 0.0 && screen[1] > 0.0 {
            let width = ui.available_width().min(320.0);
            let size = egui::vec2(width, width * screen[1] / screen[0]);
            let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, egui::Color32::from_gray(24));
            let scale = [rect.width() / screen[0], rect.height() / screen[1]];
            for row in &rows {
                if let Some(n) = work.model.node(row.id) {
                    let r = egui::Rect::from_min_size(
                        egui::pos2(
                            rect.min.x + n.rect[0] * scale[0],
                            rect.min.y + n.rect[1] * scale[1],
                        ),
                        egui::vec2(n.rect[2] * scale[0], n.rect[3] * scale[1]),
                    );
                    let selected = work.model.selected == Some(n.id);
                    let stroke = egui::Stroke::new(
                        if selected { 2.0 } else { 0.5 },
                        if selected {
                            egui::Color32::YELLOW
                        } else {
                            egui::Color32::from_gray(90)
                        },
                    );
                    painter.rect_stroke(r, 0.0, stroke, egui::StrokeKind::Inside);
                }
            }
            if let Some(pos) = response.hover_pos()
                && (work.pick_mode || response.clicked())
            {
                picked_on_map = Some((pos, rect));
            }
        }
        inspector_tabs(ui, &mut work.events_tab);
        if work.events_tab {
            events_table(ui, work.inspection.as_ref());
        } else if work.inspection.is_some() {
            copy = live_controls(ui, &mut work, source);
            if let Some(inspection) = &work.inspection {
                property_table(ui, inspection);
            }
        } else {
            ui.label("Select a node to inspect it.");
        }
    });
    if let Some(id) = toggled {
        work.model.toggle(id);
    }
    if let Some(id) = clicked {
        work.select(source, Some(id));
    }
    if let Some((pos, rect)) = picked_on_map {
        work.pick_on_map(
            source,
            [pos.x, pos.y],
            [rect.min.x, rect.min.y],
            [rect.width(), rect.height()],
        );
    }
    panel.model = work.model;
    panel.inspection = work.inspection;
    panel.error = work.error;
    panel.last_highlight = work.last_highlight;
    panel.events_tab = work.events_tab;
    panel.live_field = work.live_field;
    panel.live_value = work.live_value;
    panel.last_action_frame = work.last_action_frame;
    if let Some(params) = copy {
        if let Attachment::Remote(remote) = &panel.attachment
            && !copy_project_matches(world, &remote.project_root)
        {
            panel.error = Some("ui_copy_origin: the attached game belongs to another project; open that project before copying".into());
        } else if let Err(e) = copy_to_document(world, &params) {
            panel.error = Some(e.message);
        }
    }
    world.insert_resource(panel);
}

fn property_table(ui: &mut egui::Ui, inspection: &Value) {
    egui::ScrollArea::vertical()
        .id_salt("ui_inspector_props")
        .show(ui, |ui| {
            ui.strong(format!(
                "{} {}",
                inspection["type"].as_str().unwrap_or("Node"),
                inspection["name"].as_str().unwrap_or("")
            ));
            let rect = &inspection["layout"]["rect"];
            ui.label(format!(
                "rect {:.0},{:.0}  {:.0} x {:.0}   z {}",
                rect["x"].as_f64().unwrap_or(0.0),
                rect["y"].as_f64().unwrap_or(0.0),
                rect["w"].as_f64().unwrap_or(0.0),
                rect["h"].as_f64().unwrap_or(0.0),
                inspection["layout"]["z"]
            ));
            if let Some(name) = inspection["accessibility"]["name"]
                .as_str()
                .filter(|n| !n.is_empty())
            {
                ui.label(format!(
                    "accessible name: {name} ({})",
                    value_text(&inspection["accessibility"]["role"])
                ));
            }
            let focus = &inspection["focus"];
            if focus["focusable"] == true {
                ui.label(format!(
                    "focusable{}",
                    if focus["focused"] == true { ", focused" } else { "" }
                ));
            }
            for group in ["visual", "content", "layout"] {
                ui.collapsing(group, |ui| {
                    egui::Grid::new(format!("ui_inspector_{group}"))
                        .striped(true)
                        .show(ui, |ui| {
                            let props = inspection["properties"]
                                .as_array()
                                .map(Vec::as_slice)
                                .unwrap_or_default();
                            for p in props.iter().filter(|p| p["group"] == group) {
                                // Plain defaults are noise in the layout group; show what is set.
                                if group == "layout" && p["source"] == "default" {
                                    continue;
                                }
                                ui.label(p["name"].as_str().unwrap_or(""));
                                ui.label(value_text(&p["value"]));
                                let source = p["source"].as_str().unwrap_or("default");
                                ui.colored_label(source_color(source), source);
                                ui.label(p["detail"].as_str().unwrap_or(""));
                                ui.end_row();
                            }
                        });
                });
            }
            if let Some(binds) = inspection["bindings"].as_array().filter(|b| !b.is_empty()) {
                ui.collapsing("bindings", |ui| {
                    for b in binds {
                        ui.label(format!(
                            "{} -> {} ({}) = {}",
                            b["source"].as_str().unwrap_or(""),
                            b["target"].as_str().unwrap_or(""),
                            b["mode"].as_str().unwrap_or(""),
                            value_text(&b["value"])
                        ));
                    }
                });
            }
        });
}

fn copy_project_matches(world: &World, attached_root: &std::path::Path) -> bool {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.as_deref())
        .and_then(|root| root.canonicalize().ok())
        .is_some_and(|root| root == attached_root)
}

fn events_table(ui: &mut egui::Ui, inspection: Option<&Value>) {
    ui.label("Recent events (simulation frames)");
    egui::ScrollArea::vertical()
        .id_salt("ui-inspector-events")
        .show(ui, |ui| {
            let events = inspection.and_then(|i| i["events"].as_array());
            if events.is_none_or(Vec::is_empty) {
                ui.label("No recorded events for this node.");
            }
            for event in events.into_iter().flatten() {
                ui.label(format!(
                    "Frame {} · {} · {}",
                    event["frame"],
                    event["kind"].as_str().unwrap_or_default(),
                    value_text(&event["value"])
                ));
            }
        });
}

fn live_controls(
    ui: &mut egui::Ui,
    panel: &mut UiInspectorPanel,
    source: &mut dyn InspectorSource,
) -> Option<Value> {
    let node = panel.model.selected?;
    ui.label("Live edits — running game only");
    let inspection = panel.inspection.as_ref()?;
    let mut options = vec![("text".to_owned(), Value::String(String::new()))];
    for property in inspection["properties"].as_array().into_iter().flatten() {
        if property["group"] == "layout" {
            options.push((
                format!("style.{}", property["name"].as_str().unwrap_or_default()),
                property["value"].clone(),
            ));
        } else if property["name"] == "text" {
            options[0].1 = property["value"].clone();
        }
    }
    let before = panel.live_field.clone();
    egui::ComboBox::from_id_salt("ui-inspector-live-field")
        .selected_text(if before.is_empty() {
            "Choose a field"
        } else {
            &before
        })
        .show_ui(ui, |ui| {
            for (field, _) in &options {
                ui.selectable_value(&mut panel.live_field, field.clone(), field);
            }
        });
    if panel.live_field != before
        && let Some((_, value)) = options.iter().find(|(field, _)| *field == panel.live_field)
    {
        panel.live_value = value.to_string();
    }
    ui.add(egui::TextEdit::singleline(&mut panel.live_value).hint_text("JSON value for the live field"));
    let apply = ui.button("Apply live edit");
    #[cfg(test)]
    ui.data_mut(|d| d.insert_temp(egui::Id::new("ui-inspector-apply-id"), apply.id));
    let frame = ui.ctx().cumulative_frame_nr();
    if apply.clicked() && panel.last_action_frame != Some(frame) {
        panel.last_action_frame = Some(frame);
        match serde_json::from_str::<Value>(&panel.live_value) {
            Ok(value) => match source.call(
                "ui.set_live_style",
                json!({"node":node,"field":panel.live_field,"value":value}),
            ) {
                Ok(inspection) => {
                    panel.inspection = Some(inspection);
                    panel.error = None;
                }
                Err(e) => panel.error = Some(e),
            },
            Err(e) => panel.error = Some(format!("ui_live_value: {e}")),
        }
    }
    let inspection = panel.inspection.as_ref()?;
    let can_copy = inspection["document"].is_object()
        && inspection["live_edits"]
            .as_object()
            .is_some_and(|e| !e.is_empty());
    let copy = ui.add_enabled(can_copy, egui::Button::new("Copy to document"));
    #[cfg(test)]
    ui.data_mut(|d| d.insert_temp(egui::Id::new("ui-inspector-copy-id"), copy.id));
    if !inspection["document"].is_object() {
        ui.weak("No exact document origin: generated nodes cannot be copied back.");
    }
    if copy.clicked() && panel.last_action_frame != Some(frame) {
        panel.last_action_frame = Some(frame);
        return Some(json!({"document":inspection["document"],"edits":inspection["live_edits"]}));
    }
    None
}

/// The explicit copy-back route; runtime edit methods never save files.
pub(crate) fn methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    vec![(
        MethodSchema {
            name: "editor.ui_inspector_copy_to_document",
            tool: "ui_inspector_copy_to_document",
            description: "Copy a live UI patch into its exact source .ui.ron through the designer's undoable edit and atomic save. Rejects a changed source, an unsaved open document, or a generated node without an exact origin.",
            params: &[
                ParamSchema {
                    name: "document",
                    kind: "object",
                    required: true,
                    description: "The document object returned by ui.inspect: path under assets, node path, expected source fields.",
                },
                ParamSchema {
                    name: "edits",
                    kind: "object",
                    required: true,
                    description: "The live_edits object from ui.inspect, mapping text or style.<field> to JSON values.",
                },
            ],
            result: "{path, node, copied, undo}",
            mutating: true,
            permission: Permission::Assets,
        },
        copy_to_document,
    )]
}

fn copy_to_document(world: &mut World, params: &Value) -> Result<Value, regia_agent::protocol::RpcError> {
    use crate::ui_designer::{DesignOp, UiDesigner, node_at, resolve_node};
    use regia_agent::protocol::{INVALID_PARAMS, RpcError};
    let error = |code: &str, message: &str| {
        RpcError::new(
            INVALID_PARAMS,
            code,
            message,
            "reload the runtime UI from its source to refresh the origin; save or close pending designer edits",
        )
    };
    let origin = &params["document"];
    let path = origin["path"]
        .as_str()
        .filter(|s| s.ends_with(".ui.ron"))
        .ok_or_else(|| error("ui_copy_origin", "an exact .ui.ron document origin is required"))?;
    regia_ui::ui_component::check_use_path(path).map_err(|e| error("ui_copy_origin", &e))?;
    let request_path = format!("assets/{path}");
    let path = crate::agent_ext::document_request_path(world, "document.path", &request_path)?;
    let node = origin["node"]
        .as_str()
        .ok_or_else(|| error("ui_copy_origin", "source node path is required"))?;
    let edits = params["edits"]
        .as_object()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| error("ui_copy_value", "edits must be a nonempty live patch"))?;
    let mut fresh = UiDesigner::default();
    fresh
        .open(&path)
        .map_err(|e| error("ui_copy_io", &e.to_string()))?;
    let file = fresh
        .doc()
        .ok_or_else(|| error("ui_copy_origin", "source did not open"))?
        .file();
    let at = resolve_node(&file.root, node).map_err(|e| error("ui_copy_origin", &e.to_string()))?;
    let def = node_at(&file.root, &at).ok_or_else(|| error("ui_copy_origin", "source node disappeared"))?;
    let current = json!({"style":def.style,"text":def.text,"name":def.name,"widget":def.widget});
    if current["name"] != origin["expected"]["name"] || current["widget"] != origin["expected"]["widget"] {
        return Err(error("ui_copy_conflict", "the source node's identity changed"));
    }
    let mut ops = Vec::new();
    for (field, value) in edits {
        if let Some(key) = field.strip_prefix("style.") {
            if current["style"].get(key).is_none() {
                return Err(error("ui_copy_value", "unknown layout property"));
            }
            if current["style"][key] != origin["expected"]["style"][key] {
                return Err(error(
                    "ui_copy_conflict",
                    "an edited source property changed since the UI spawned",
                ));
            }
        } else if field == "text" {
            if matches!(def.widget, Some(regia_ui::authoring::WidgetDef::TextInput(..))) {
                return Err(error(
                    "ui_copy_value",
                    "text input values belong to a model; the frozen document format has no initial input value field",
                ));
            }
            if !value.is_string() {
                return Err(error("ui_copy_value", "text is a string"));
            }
            if current["text"] != origin["expected"]["text"] {
                return Err(error(
                    "ui_copy_conflict",
                    "the source text changed since the UI spawned",
                ));
            }
        } else {
            return Err(error(
                "ui_copy_value",
                "only text and style.<field> can be copied",
            ));
        }
        ops.push(DesignOp::Set {
            node: at.clone(),
            key: field.clone(),
            value: value.clone(),
        });
    }
    // Preserve an existing undo history, and never save somebody else's pending changes.
    let current = world.get_resource::<UiDesigner>();
    if current.is_some_and(|d| d.doc().is_some_and(|doc| doc.dirty())) {
        return Err(error("ui_copy_dirty", "the designer has unsaved changes"));
    }
    let same = current
        .and_then(UiDesigner::doc)
        .is_some_and(|d| d.path.as_ref() == Some(&path));
    if !same && current.is_some_and(|d| d.depth() > 0) {
        return Err(error(
            "ui_copy_dirty",
            "return from the entered component before copying another document",
        ));
    }
    if same {
        let current = current
            .and_then(UiDesigner::doc)
            .ok_or_else(|| error("ui_copy_origin", "open document disappeared"))?;
        if current.file() != file {
            return Err(error(
                "ui_copy_conflict",
                "the open document differs from its file",
            ));
        }
        *fresh
            .doc_mut()
            .map_err(|e| error("ui_copy_origin", &e.to_string()))? = current.clone();
    }
    let doc = fresh
        .doc_mut()
        .map_err(|e| error("ui_copy_origin", &e.to_string()))?;
    doc.apply_batch(ops)
        .map_err(|e| error("ui_copy_value", &e.to_string()))?;
    doc.save(None).map_err(|e| error("ui_copy_io", &e.to_string()))?;
    let out =
        json!({"path":path.display().to_string(),"node":node,"copied":edits.len(),"undo":doc.undo_len()});
    if same {
        let new_doc = doc.clone();
        *world
            .resource_mut::<UiDesigner>()
            .doc_mut()
            .map_err(|e| error("ui_copy_origin", &e.to_string()))? = new_doc;
    } else {
        world.insert_resource(fresh);
    }
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::UiDesigner),
    );
    Ok(out)
}

fn inspector_tabs(ui: &mut egui::Ui, events_tab: &mut bool) {
    ui.horizontal(|ui| {
        ui.selectable_value(events_tab, false, "Properties");
        let events = ui.selectable_value(events_tab, true, "Events");
        #[cfg(test)]
        ui.data_mut(|d| d.insert_temp(egui::Id::new("ui-inspector-events-id"), events.id));
        #[cfg(not(test))]
        let _ = events;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u64, parent: Option<u64>, kind: &str, name: Option<&str>, text: Option<&str>) -> NodeInfo {
        NodeInfo {
            id,
            parent,
            name: name.map(str::to_owned),
            kind: kind.to_owned(),
            classes: Vec::new(),
            text: text.map(str::to_owned),
            rect: [0.0, 0.0, 100.0, 50.0],
        }
    }

    fn sample() -> Vec<NodeInfo> {
        vec![
            node(1, None, "Root", Some("hud"), None),
            node(2, Some(1), "Node", Some("bar"), None),
            node(3, Some(2), "Button", Some("save"), Some("Save game")),
            node(4, Some(2), "Button", Some("quit"), Some("Quit")),
            node(5, Some(1), "Text", None, Some("Score")),
        ]
    }

    fn ids(model: &InspectorModel) -> Vec<(u64, usize)> {
        model.rows().iter().map(|r| (r.id, r.depth)).collect()
    }

    #[test]
    fn the_tree_flattens_in_preorder_with_depths() {
        let mut m = InspectorModel::default();
        m.set_nodes(sample());
        assert_eq!(ids(&m), vec![(1, 0), (2, 1), (3, 2), (4, 2), (5, 1)]);
        let rows = m.rows();
        assert!(rows[0].has_children && rows[0].expanded);
        assert!(!rows[2].has_children);
    }

    #[test]
    fn collapsing_hides_a_subtree_and_survives_a_refresh() {
        let mut m = InspectorModel::default();
        m.set_nodes(sample());
        m.toggle(2);
        assert_eq!(ids(&m), vec![(1, 0), (2, 1), (5, 1)]);
        m.set_nodes(sample());
        assert_eq!(
            ids(&m),
            vec![(1, 0), (2, 1), (5, 1)],
            "collapsed state is kept per node"
        );
        m.toggle(2);
        assert_eq!(m.rows().len(), 5);
    }

    #[test]
    fn search_keeps_matches_and_their_ancestors_and_opens_collapsed_nodes() {
        let mut m = InspectorModel::default();
        m.set_nodes(sample());
        m.toggle(2);
        m.search = "SAVE".into();
        assert_eq!(ids(&m), vec![(1, 0), (2, 1), (3, 2)]);
        m.search = "score".into();
        assert_eq!(ids(&m), vec![(1, 0), (5, 1)]);
        m.search = "4".into();
        assert_eq!(ids(&m), vec![(1, 0), (2, 1), (4, 2)], "an id matches too");
        m.search = "zzz".into();
        assert!(m.rows().is_empty());
        m.search.clear();
        assert_eq!(
            m.rows().len(),
            3,
            "clearing the search restores the collapsed view"
        );
    }

    #[test]
    fn the_selection_survives_a_refresh_and_clears_when_the_node_is_gone() {
        let mut m = InspectorModel::default();
        m.set_nodes(sample());
        m.selected = Some(3);
        let mut moved = sample();
        moved.reverse();
        m.set_nodes(moved);
        assert_eq!(m.selected, Some(3));
        let mut fewer = sample();
        fewer.retain(|n| n.id != 3);
        m.set_nodes(fewer);
        assert_eq!(m.selected, None);
    }

    #[test]
    fn a_node_whose_parent_is_missing_becomes_a_root() {
        let mut m = InspectorModel::default();
        m.set_nodes(vec![node(7, Some(99), "Node", None, None)]);
        assert_eq!(ids(&m), vec![(7, 0)]);
    }

    #[test]
    fn refresh_is_rate_capped_and_clamped() {
        let mut m = InspectorModel::default();
        assert!(m.refresh_due(10.0), "the first refresh is always due");
        m.mark_refreshed(10.0);
        assert!(!m.refresh_due(10.1));
        assert!(m.refresh_due(10.25));
        m.set_refresh_hz(1000.0);
        assert_eq!(m.refresh_hz(), REFRESH_HZ_RANGE.1);
        m.set_refresh_hz(0.0);
        assert_eq!(m.refresh_hz(), REFRESH_HZ_RANGE.0);
        m.set_refresh_hz(f64::NAN);
        assert_eq!(m.refresh_hz(), REFRESH_HZ_RANGE.0);
        assert!(m.refresh_due(5.0), "a clock that went backwards refreshes");
    }

    #[test]
    fn map_points_scale_to_the_screen() {
        assert_eq!(
            map_to_screen([60.0, 30.0], [10.0, 10.0], [100.0, 50.0], [1000.0, 500.0]),
            Some([500.0, 200.0])
        );
        assert_eq!(
            map_to_screen([5.0, 30.0], [10.0, 10.0], [100.0, 50.0], [1000.0, 500.0]),
            None
        );
        assert_eq!(
            map_to_screen([20.0, 20.0], [10.0, 10.0], [100.0, 50.0], [0.0, 0.0]),
            None
        );
    }

    /// A scripted source: answers by method, records the calls.
    struct Fake {
        nodes: Vec<Value>,
        pick: Option<u64>,
        calls: Vec<(String, Value)>,
    }

    impl InspectorSource for Fake {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.push((method.to_owned(), params.clone()));
            match method {
                "ui.query" => Ok(json!({ "nodes": self.nodes })),
                "ui.inspect" => Ok(json!({ "node": params["node"], "properties": [] })),
                "ui.pick_at" => Ok(json!({ "node": self.pick })),
                "ui.highlight" => Ok(json!({})),
                _ => Err("nope".into()),
            }
        }
    }

    fn fake() -> Fake {
        Fake {
            nodes: vec![
                json!({ "id": 1, "type": "Root", "rect": [0, 0, 800, 600] }),
                json!({ "id": 2, "parent": 1, "type": "Button", "name": "ok", "rect": [10, 10, 100, 40] }),
            ],
            pick: Some(2),
            calls: Vec::new(),
        }
    }

    #[test]
    fn refresh_reads_the_tree_and_the_selected_inspection_at_the_capped_rate() {
        let mut panel = UiInspectorPanel::default();
        let mut src = fake();
        assert!(panel.refresh(&mut src, 1.0, false));
        assert_eq!(panel.model.len(), 2);
        panel.select(&mut src, Some(2));
        assert_eq!(
            panel.inspection.as_ref().map(|v| v["node"].clone()),
            Some(json!(2))
        );
        let before = src.calls.len();
        assert!(!panel.refresh(&mut src, 1.05, false), "too soon");
        assert_eq!(src.calls.len(), before);
        assert!(panel.refresh(&mut src, 1.3, false));
        assert!(
            panel.inspection.is_some(),
            "the inspection is re-read for the selection"
        );
        assert_eq!(panel.model.selected, Some(2));
    }

    #[test]
    fn selecting_highlights_once_per_change_and_clearing_sends_clear() {
        let mut panel = UiInspectorPanel::default();
        let mut src = fake();
        panel.refresh(&mut src, 0.0, true);
        panel.select(&mut src, Some(2));
        panel.select(&mut src, Some(2));
        let highlights: Vec<_> = src.calls.iter().filter(|(m, _)| m == "ui.highlight").collect();
        assert_eq!(highlights.len(), 1);
        panel.select(&mut src, None);
        let last = src
            .calls
            .iter()
            .rfind(|(m, _)| m == "ui.highlight")
            .map(|(_, p)| p.clone());
        assert_eq!(last, Some(json!({ "clear": true })));
    }

    #[test]
    fn hovering_the_map_picks_through_ui_pick_at_in_screen_coordinates() {
        let mut panel = UiInspectorPanel::default();
        let mut src = fake();
        panel.refresh(&mut src, 0.0, true);
        // The map is 400 x 300 at (50, 50); its centre is the middle of the 800 x 600 screen.
        let picked = panel.pick_on_map(&mut src, [250.0, 200.0], [50.0, 50.0], [400.0, 300.0]);
        assert_eq!(picked, Some(2));
        assert_eq!(panel.model.selected, Some(2));
        let call = src
            .calls
            .iter()
            .rfind(|(m, _)| m == "ui.pick_at")
            .expect("pick_at called");
        assert_eq!(call.1, json!({ "x": 400.0, "y": 300.0 }));
        assert_eq!(
            panel.pick_on_map(&mut src, [0.0, 0.0], [50.0, 50.0], [400.0, 300.0]),
            None
        );
    }

    #[test]
    fn a_failed_query_shows_the_error_and_keeps_the_old_tree() {
        struct Down;
        impl InspectorSource for Down {
            fn call(&mut self, _: &str, _: Value) -> Result<Value, String> {
                Err("connection failed".into())
            }
        }
        let mut panel = UiInspectorPanel::default();
        panel.model.set_nodes(sample());
        panel.refresh(&mut Down, 0.0, true);
        assert_eq!(panel.error.as_deref(), Some("connection failed"));
        assert_eq!(panel.model.len(), 5);
    }

    #[test]
    fn ui_inspector_live_style_and_events_use_the_runtime_source() {
        let mut world = World::new();
        let node = world
            .spawn((regia_ui::UiNode, regia_ui::NodeStyle::default()))
            .id();
        let mut source = WorldSource(&mut world);
        source
            .call(
                "ui.set_live_style",
                json!({"node":node.to_bits(), "field":"style.flex_grow", "value":2.0}),
            )
            .expect("live edit");
        let out = source
            .call("ui.inspect", json!({"node":node.to_bits()}))
            .expect("inspect");
        assert!(
            out["properties"]
                .as_array()
                .expect("properties")
                .iter()
                .any(|p| p["name"] == "flex_grow" && p["value"] == 2.0)
        );
        let events = source
            .call("ui.inspect_events", json!({"node":node.to_bits()}))
            .expect("events");
        assert!(events["events"].is_array());
    }

    /// sprint-w13-ui-live-bugs (4): one real UI event (a click the UI plugin's recorder saw) reaches
    /// the inspector's runtime source, `ui.inspect_events`' answer, and the events table.
    #[test]
    fn a_recorded_ui_event_reaches_the_events_table() {
        let mut app = regia_app::App::new();
        app.insert_resource(regia_ui::text::TextSystem::embedded_only());
        app.add_plugins(regia_ui::UiPlugin::code_only());
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        let node = app
            .world_mut()
            .spawn((regia_ui::UiNode, regia_ui::NodeStyle::default()))
            .id();
        let other = app.world_mut().spawn(regia_ui::UiNode).id();
        let events_of = |world: &mut World, node: Entity| {
            WorldSource(world)
                .call("ui.inspect_events", json!({ "node": node.to_bits() }))
                .expect("events")
        };
        assert!(
            events_of(app.world_mut(), node)["events"]
                .as_array()
                .expect("events")
                .is_empty(),
            "nothing recorded before the click"
        );
        app.world_mut()
            .write_message(regia_ui::UiEvent::new(node, regia_ui::UiEventKind::Click));
        app.update();
        let answer = events_of(app.world_mut(), node);
        let recorded = answer["events"].as_array().expect("events");
        assert_eq!(recorded.len(), 1, "{answer}");
        assert_eq!(recorded[0]["kind"], "click");
        assert_eq!(recorded[0]["node"], node.to_bits());
        assert!(
            events_of(app.world_mut(), other)["events"]
                .as_array()
                .expect("events")
                .is_empty(),
            "the event belongs to the clicked node only"
        );
        // The panel's refresh carries the answer into the events table.
        let mut panel = UiInspectorPanel::default();
        panel.model.selected = Some(node.to_bits());
        panel.inspection = Some(
            WorldSource(app.world_mut())
                .call("ui.inspect", json!({ "node": node.to_bits() }))
                .expect("inspect"),
        );
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |ui| events_table(ui, panel.inspection.as_ref()),
        );
        output.textures_delta.clear();
        let shown: Vec<String> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect();
        assert!(
            shown.iter().any(|line| line.contains("· click ·")),
            "the events table lists the click: {shown:?}"
        );
        assert!(
            !shown.iter().any(|line| line.contains("No recorded events")),
            "{shown:?}"
        );
    }

    #[test]
    fn world_source_reports_an_unknown_node_with_the_error_code() {
        let mut world = World::new();
        let mut src = WorldSource(&mut world);
        let err = src
            .call("ui.inspect", json!({ "node": 12345 }))
            .expect_err("unknown");
        assert_eq!(err, "ui_node_not_found");
    }
}

#[cfg(test)]
mod live_panel_tests {
    use super::*;

    #[test]
    fn ui_inspector_copy_refuses_to_save_an_input_value_as_a_duplicate_label() {
        let dir = regia_test_utils::unique_temp_dir("ui-inspector-input-copy");
        let path = dir.join("assets/ui/input.ui.ron");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        let file = regia_ui::UiFile {
            version: regia_ui::UI_RON_VERSION,
            component: None,
            variants: Vec::new(),
            root: crate::ui_designer::preset("text_input").expect("input"),
            back: None,
        };
        let before = regia_ui::write_ui_ron(&file).expect("ron");
        regia_test_utils::write_atomic(&path, before.as_bytes()).expect("write");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        let origin = json!({"path":"ui/input.ui.ron","node":"/","expected":{"style":file.root.style,"text":file.root.text,"name":file.root.name,"widget":file.root.widget}});
        let error = copy_to_document(
            &mut world,
            &json!({"document":origin,"edits":{"text":"Transient"}}),
        )
        .expect_err("no source field");
        assert_eq!(error.data.as_ref().expect("data")["code"], "ui_copy_value");
        assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), before);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn ui_inspector_keyboard_applies_once_and_opens_events() {
        let ctx = egui::Context::default();
        let mut world = World::new();
        let node = world
            .spawn((regia_ui::UiNode, regia_ui::NodeStyle::default()))
            .id();
        let mut panel = UiInspectorPanel::default();
        panel.model.selected = Some(node.to_bits());
        panel.inspection = Some(regia_ui::inspect::inspect(&mut world, node));
        panel.live_field = "style.flex_grow".into();
        panel.live_value = "3".into();
        let frame = |events: Vec<egui::Event>, panel: &mut UiInspectorPanel, world: &mut World| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                inspector_tabs(ui, &mut panel.events_tab);
                if panel.events_tab {
                    events_table(ui, panel.inspection.as_ref());
                } else {
                    let _ = live_controls(ui, panel, &mut WorldSource(world));
                }
            });
            output.textures_delta.clear();
        };
        frame(vec![], &mut panel, &mut world);
        let apply = ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-inspector-apply-id")))
            .expect("apply");
        ctx.memory_mut(|m| m.request_focus(apply));
        let enter = || egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![enter()], &mut panel, &mut world);
        assert_eq!(
            world.get::<regia_ui::NodeStyle>(node).expect("style").0.flex_grow,
            3.0
        );
        let tab = ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-inspector-events-id")))
            .expect("events tab");
        ctx.memory_mut(|m| m.request_focus(tab));
        frame(vec![enter()], &mut panel, &mut world);
        assert!(panel.events_tab);
    }

    #[test]
    fn ui_inspector_copy_control_is_keyboard_operable() {
        let ctx = egui::Context::default();
        let mut world = World::new();
        let node = world
            .spawn((regia_ui::UiNode, regia_ui::NodeStyle::default()))
            .id();
        let mut panel = UiInspectorPanel::default();
        panel.model.selected = Some(node.to_bits());
        let mut inspection = regia_ui::inspect::inspect(&mut world, node);
        inspection["document"] = json!({"path":"ui/test.ui.ron","node":"/"});
        inspection["live_edits"] = json!({"style.flex_grow":2.0});
        panel.inspection = Some(inspection);
        let mut copied = None;
        let mut draw = |events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                if let Some(value) = live_controls(ui, &mut panel, &mut WorldSource(&mut world)) {
                    copied = Some(value);
                }
            });
            output.textures_delta.clear();
        };
        draw(vec![]);
        let copy = ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-inspector-copy-id")))
            .expect("copy id");
        ctx.memory_mut(|m| m.request_focus(copy));
        draw(vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert_eq!(copied.expect("copy action")["edits"]["style.flex_grow"], 2.0);
    }

    #[test]
    fn ui_inspector_copy_requires_the_attached_games_project() {
        let dir = regia_test_utils::unique_temp_dir("ui-inspector-project");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).expect("projects");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        assert!(copy_project_matches(&world, &dir.canonicalize().expect("root")));
        assert!(!copy_project_matches(
            &world,
            &other.canonicalize().expect("other")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_inspector_copy_checks_source_and_keeps_an_undo_step() {
        let dir = regia_test_utils::unique_temp_dir("ui-inspector-copy");
        let path = dir.join("assets/ui/live.ui.ron");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        let file = regia_ui::UiFile {
            version: regia_ui::UI_RON_VERSION,
            component: None,
            variants: Vec::new(),
            root: regia_ui::authoring::UiNodeDef {
                name: Some("root".into()),
                text: Some(regia_ui::authoring::TextDef::literal("Original")),
                ..Default::default()
            },
            back: None,
        };
        regia_test_utils::write_atomic(&path, regia_ui::write_ui_ron(&file).expect("ron").as_bytes())
            .expect("write");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        let origin = json!({"path":"ui/live.ui.ron","node":"/","expected":{"style":file.root.style,"text":file.root.text,"name":file.root.name,"widget":file.root.widget}});
        let out = copy_to_document(
            &mut world,
            &json!({"document":origin,"edits":{"text":"Copied","style.flex_grow":2.0}}),
        )
        .expect("copy");
        assert_eq!(out["copied"], 2);
        assert_eq!(out["undo"], 1);
        let saved = std::fs::read(&path).expect("saved");
        let err = copy_to_document(&mut world, &json!({"document":origin,"edits":{"text":"Stale"}}))
            .expect_err("conflict");
        assert_eq!(err.data.as_ref().expect("data")["code"], "ui_copy_conflict");
        assert_eq!(std::fs::read(&path).expect("unchanged"), saved);
        let mut designer = world.resource_mut::<crate::ui_designer::UiDesigner>();
        designer.doc_mut().expect("document").undo().expect("undo");
        assert_eq!(designer.doc().expect("document").file(), &file);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
