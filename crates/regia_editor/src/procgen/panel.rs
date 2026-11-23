// SPDX-License-Identifier: MPL-2.0
//! The procedural graph panel (`Panel::ProcGraph`, spec 67 § 2.1): canvas (`egui-snarl`) with pins
//! colored by data kind, the palette, node settings and parameters, the run report strip and the
//! data table. The panel reads the document while drawing and collects edits, applied after the
//! frame through [`super::doc::edit`] (one undo entry each; drags and slider scrubs coalesce).

use std::collections::{BTreeMap, BTreeSet};

use egui_snarl::ui::{AnyPins, SnarlStyle, SnarlViewer, SnarlWidget};
use egui_snarl::{InPin, OutPin, Snarl};
use regia_ecs::prelude::*;
use regia_procgen::nodes::PinSpec;
use regia_procgen::{DataKind, KindSet, ProcGraph, ProcNodeRegistry, SettingValue};
use regia_render::RenderDevice;

use super::debug_view::{ColorBy, ProcPreview, VisForm, VisualizeStyle};
use super::doc::{self, ProcGraphDoc};
use super::inspect::{self, ProcInspect};

/// One color per [`DataKind`] in `DataKind::ALL` order (a colorblind-safe set: Okabe-Ito hues
/// plus two neutrals), linear RGBA.
pub const KIND_COLORS: [[f32; 4]; 10] = [
    [0.90, 0.62, 0.00, 1.0],
    [0.34, 0.71, 0.91, 1.0],
    [0.00, 0.62, 0.45, 1.0],
    [0.94, 0.89, 0.26, 1.0],
    [0.00, 0.45, 0.70, 1.0],
    [0.84, 0.37, 0.00, 1.0],
    [0.80, 0.47, 0.65, 1.0],
    [0.60, 0.60, 0.60, 1.0],
    [0.55, 0.35, 0.20, 1.0],
    [0.35, 0.80, 0.35, 1.0],
];

/// The default pin color of a kind set (the first kind's) from the theme's default set
/// (`crate::theme::DATA_KIND_DEFAULTS`). The canvas draws `super::badges::StripedPin` with the
/// current theme instead: one stripe per kind.
#[must_use]
pub fn kind_color(kinds: KindSet) -> egui::Color32 {
    let i = DataKind::ALL.iter().position(|k| kinds.contains(*k)).unwrap_or(7);
    crate::theme::DATA_KIND_DEFAULTS[i]
}

/// Which part of the panel a capture shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureView {
    /// The canvas with the palette and settings.
    Canvas,
    /// The data table.
    Table,
    /// The run report.
    Report,
}

impl CaptureView {
    /// Parses `canvas`, `table` or `report`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "canvas" => Some(Self::Canvas),
            "table" => Some(Self::Table),
            "report" => Some(Self::Report),
            _ => None,
        }
    }
}

/// Panel UI state (not undoable).
#[derive(Resource, Default)]
pub struct ProcPanelUi {
    /// Nodes on screen (and the selection). Rebuilt when the document revision or the visible set
    /// changes; off-screen nodes are not drawn (spec 67 § 9).
    shown: Option<Snarl<u32>>,
    shown_rev: Option<u64>,
    shown_ids: Vec<u32>,
    /// Scratch for the visibility scan so a steady frame does not allocate a fresh vec.
    visible_buf: Vec<u32>,
    /// Set after the first canvas frame, which forces a 1:1 view of the graph origin.
    canvas_framed: bool,
    /// The canvas transform observed last frame.
    view_tf: Option<egui::emath::TSTransform>,
    /// Draft path for the node menu's "collapse to subgraph".
    collapse_path: String,
    /// The canvas pins, resolved once per document revision (resolving 300 nodes' settings every
    /// frame was most of the panel's frame cost).
    pins: Option<(u64, std::sync::Arc<BTreeMap<u32, PinSpec>>)>,
    search: String,
    side_params: bool,
    bottom_table: bool,
    /// Draft text of RON settings being typed, by `(node, setting)`.
    drafts: BTreeMap<(u32, String), String>,
    /// "New…": the path being typed and the chosen starter.
    new_path: String,
    new_starter: Option<String>,
    /// Nodes the canvas drew in the last frame (the panel budget test counts them, spec 67 § 9).
    nodes_drawn: usize,
    /// Nodes the canvas skipped drawing (off screen) in the last frame.
    nodes_culled: usize,
    /// Cull off-screen nodes and table rows. On by default (`no_cull` false);
    /// `editor.procgen_panel_culling` turns it off to measure.
    no_cull: bool,
    /// Data-table rows laid out and rows available in the last frame.
    rows_drawn: usize,
    rows_total: usize,
    /// The tool row's kind (`None` = Path), preset, Area biome and Paint brush.
    tool_kind: Option<super::tools::ToolKind>,
    tool_preset: String,
    tool_biome: String,
    tool_brush: super::tools::Brush,
    /// The Parameters tab's drafts.
    params_tab: super::params_tab::ParamsTabUi,
    /// The typed setting widgets' drafts.
    widgets: super::widgets::WidgetDrafts,
    /// The last refusal of a setting value, by `(node, setting)`: shown in red under the widget.
    setting_errors: BTreeMap<(u32, String), String>,
    /// The promote prompt's name draft, by `(node, setting)`.
    promote_name: BTreeMap<(u32, String), String>,
    /// The data table's filter field draft and the last refusal.
    filter_draft: Option<String>,
    filter_error: String,
    /// Node badges, rebuilt when their inputs change (`badges_key`).
    badges: std::sync::Arc<BTreeMap<u32, super::badges::NodeBadges>>,
    badges_key: Option<BadgesKey>,
    /// The keyboard route: the Nodes list, its palette and connect form (ADR-7383).
    keys: crate::canvas_keys::GraphKeys,
    /// The node the Nodes list's focus belongs to (rows move when nodes are added or removed).
    keys_node: Option<u32>,
    /// The node Enter in the Nodes list asked to frame, until the culled canvas holds it.
    frame_id: Option<u32>,
}

/// What the cached node badges were built from.
#[derive(Clone, PartialEq)]
struct BadgesKey {
    revision: u64,
    target: Option<Entity>,
    run_id: u64,
    inspected: Option<u32>,
    /// A fold of the visualized node ids.
    visualized: u64,
    tiles: Option<u32>,
}

/// How far past the visible canvas (in graph units) a node still draws in full, so a node
/// scrolling in has its real width and its wires meet its pins.
pub const CULL_MARGIN: f32 = 240.0;

/// Whether a widget laid out at `at` in `ui` is outside the visible area by more than
/// [`CULL_MARGIN`]. The canvas runs in graph space, so the clip rect is the visible graph rect.
fn offscreen(ui: &egui::Ui, at: egui::Pos2) -> bool {
    !ui.clip_rect().expand(CULL_MARGIN).contains(at)
}

/// A change the panel asks for while drawing.
enum PanelEdit {
    Add(String, (f64, f64)),
    AddConnected(String, (f64, f64), (u32, String), String),
    Remove(BTreeSet<u32>),
    Connect((u32, String), (u32, String)),
    Disconnect((u32, String), (u32, String)),
    Move(u32, (f64, f64)),
    /// A keyboard move: its own undo entry (no merge key).
    Nudge(u32, (f64, f64)),
    Set(u32, String, SettingValue),
    Bypass(u32, bool),
    Promote {
        node: u32,
        setting: String,
        name: String,
        existing: bool,
    },
    Unbind(u32, String),
    Param(super::params_tab::ParamAction),
    Table(super::inspect::TableControl),
    Select(BTreeSet<u32>),
    Visualize(u32),
    Inspect(u32),
    Duplicate(BTreeSet<u32>),
    Save,
    Resolve(bool),
    Enter(u32),
    Back,
    New(String, Option<String>),
    SetStarter(bool),
    Clip(String),
    Paste(String),
    Pin(u32, String, bool),
    Collapse(BTreeSet<u32>, String),
    Expand {
        node: u32,
        copy: bool,
    },
    Isolate(Option<u32>),
    Seed {
        entity: Entity,
        action: doc::SeedAction,
    },
    Tier(Option<regia_render::quality::QualityTier>),
    CancelTool,
    Action(PanelAction),
    ArmWith {
        kind: super::tools::ToolKind,
        preset: String,
        biome: Option<String>,
        brush: super::tools::Brush,
    },
    HitFilter(super::tools::ToolKind, super::tools::HitFilter),
    PlantSeed(u32),
    /// A message for the toolbar.
    Toast(String),
    /// The Inspect table's hovered element (`None` clears it): view-only, writes the
    /// viewport-highlight state (spec 67 § 3).
    HoverRow(Option<usize>),
}

/// A graph-panel action the node menu and the preview toolbar perform. Tests apply these directly;
/// the menu pushes the same `PanelEdit` values, which call [`apply_panel_action`].
#[derive(Clone, Debug, PartialEq)]
pub enum PanelAction {
    /// Collapse these nodes into a new graph at `path`.
    Collapse {
        /// Node ids.
        nodes: BTreeSet<u32>,
        /// The new `.pgraph.ron` path.
        path: String,
    },
    /// Expand a `graph.call` node.
    Expand {
        /// The call node.
        node: u32,
        /// Keep the called file and paste a copy.
        copy: bool,
    },
    /// Isolate a node, or `None` to end isolation.
    Isolate(Option<u32>),
    /// A preview-bar seed action on a generator.
    Seed {
        /// The generator.
        entity: Entity,
        /// Pin, unpin, new, set or same-as.
        action: doc::SeedAction,
    },
    /// The preview quality tier (`None` is the running tier).
    Tier(Option<regia_render::quality::QualityTier>),
    /// "Preview on": bind this generator (or none) as an explicit choice.
    PreviewOn(Option<Entity>),
    /// "Preview on: follow selection": the default rule (selection's generator, else last used).
    FollowSelection,
    /// The preview bar's switches; `None` keeps a switch as it is.
    ViewSwitches {
        /// Generate around view (the editor camera is an anchor, spec 66 § 4.1).
        around: Option<bool>,
        /// The tiles overlay (spec 66 § 7).
        tiles: Option<bool>,
    },
}

/// Applies one panel action through the same document functions the agent methods call.
///
/// # Errors
///
/// The document codes of collapse, expand, isolate and seed (`procgen_file_exists` when the
/// collapse path is already a graph).
pub fn apply_panel_action(world: &mut World, action: PanelAction) -> Result<(), doc::DocError> {
    match action {
        PanelAction::Collapse { nodes, path } => {
            if doc::source_text(world, &path).is_some() {
                return Err(doc::DocError::new(
                    "procgen_file_exists",
                    format!("`{path}` already exists"),
                ));
            }
            doc::collapse_selection(world, &nodes, &path).map(|_| ())
        }
        PanelAction::Expand { node, copy } => doc::expand_call(world, node, copy).map(|_| ()),
        PanelAction::Isolate(node) => super::debug_view::isolate(world, node).map(|_| ()),
        PanelAction::Seed { entity, action } => doc::apply_seed_action(world, entity, action).map(|_| ()),
        PanelAction::Tier(tier) => {
            super::debug_view::set_tier(world, tier);
            Ok(())
        }
        PanelAction::PreviewOn(target) => super::debug_view::choose_preview(world, target, false),
        PanelAction::FollowSelection => super::debug_view::choose_preview(world, None, true),
        PanelAction::ViewSwitches { around, tiles } => {
            super::view::set_switches(world, around, tiles, None, None);
            Ok(())
        }
    }
}

struct Viewer<'a> {
    graph: &'a ProcGraph,
    registry: &'a ProcNodeRegistry,
    pins: &'a BTreeMap<u32, PinSpec>,
    errors: &'a BTreeMap<u32, String>,
    report: &'a BTreeMap<u32, (f64, usize, bool, bool)>,
    /// Node badges (spec 67 § 2.2).
    badges: &'a BTreeMap<u32, super::badges::NodeBadges>,
    /// The theme the pins take their colors from.
    tokens: &'a crate::theme::ThemeTokens,
    selection: &'a BTreeSet<u32>,
    edits: &'a mut Vec<PanelEdit>,
    /// Headers shown this frame: one per node drawn.
    drawn: usize,
    /// Nodes whose text was skipped this frame because they are off screen.
    culled: usize,
    /// Cull off-screen nodes.
    cull: bool,
    /// Draft subgraph path edited from the node menu.
    collapse_path: &'a mut String,
    /// Forced on the first frame so the default view is 1:1, not a fit of every node.
    force_tf: Option<egui::emath::TSTransform>,
    /// The transform snarl used this frame, after the force.
    seen_tf: &'a mut Option<egui::emath::TSTransform>,
}

impl Viewer<'_> {
    fn input_name(&self, node: u32, i: usize) -> &str {
        match self.graph.nodes.get(&node).map(|n| n.ty.as_str()) {
            Some("reroute") => "in",
            _ => self
                .pins
                .get(&node)
                .and_then(|p| p.inputs.get(i))
                .map(|p| p.name.as_str())
                .unwrap_or(""),
        }
    }

    fn output_name(&self, node: u32, i: usize) -> &str {
        match self.graph.nodes.get(&node).map(|n| n.ty.as_str()) {
            Some("reroute") => "out",
            _ => self
                .pins
                .get(&node)
                .and_then(|p| p.outputs.get(i))
                .map(|p| p.name.as_str())
                .unwrap_or(""),
        }
    }
}

impl SnarlViewer<u32> for Viewer<'_> {
    fn title(&mut self, node: &u32) -> String {
        self.graph
            .nodes
            .get(node)
            .map_or_else(|| "?".into(), |n| n.ty.clone())
    }

    fn show_header(
        &mut self,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<u32>,
    ) {
        let Some(id) = snarl.get_node(node).copied() else {
            return;
        };
        let title = self.title(&id);
        if self.cull && offscreen(ui, ui.next_widget_position()) {
            // Off screen: keep the node's slot (so wires still reach its pins) without laying
            // out its text.
            self.culled += 1;
            ui.allocate_space(egui::vec2(8.0 * (title.len() + 6) as f32, 18.0));
            return;
        }
        self.drawn += 1;
        let r = ui.selectable_label(self.selection.contains(&id), format!("{title}  #{id}"));
        if r.double_clicked() && regia_procgen::graph::CALL_NODES.contains(&title.as_str()) {
            self.edits.push(PanelEdit::Enter(id));
        }
        if r.clicked() {
            let mut sel = if ui.input(|i| i.modifiers.shift) {
                self.selection.clone()
            } else {
                BTreeSet::new()
            };
            sel.insert(id);
            self.edits.push(PanelEdit::Select(sel));
        }
        if let Some(b) = self.badges.get(&id) {
            badge_row(ui, b);
        } else if let Some(err) = self.errors.get(&id) {
            // The glyph alone carries no meaning: the spoken name is the error text (FU-7).
            crate::a11y_audit::warning_glyph(ui, egui::Color32::from_rgb(230, 80, 70), err);
        }
    }

    fn inputs(&mut self, node: &u32) -> usize {
        match self.graph.nodes.get(node).map(|n| n.ty.as_str()) {
            Some("reroute") => 1,
            Some("note") => 0,
            _ => self.pins.get(node).map_or(0, |p| p.inputs.len()),
        }
    }

    fn outputs(&mut self, node: &u32) -> usize {
        match self.graph.nodes.get(node).map(|n| n.ty.as_str()) {
            Some("reroute") => 1,
            Some("note") => 0,
            _ => self.pins.get(node).map_or(0, |p| p.outputs.len()),
        }
    }

    fn show_input(
        &mut self,
        pin: &InPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<u32>,
    ) -> impl egui_snarl::ui::SnarlPin + 'static {
        let id = snarl.get_node(pin.id.node).copied().unwrap_or(0);
        if self.cull && offscreen(ui, ui.next_widget_position()) {
            ui.allocate_space(egui::vec2(40.0, 16.0));
        } else {
            ui.label(self.input_name(id, pin.id.input));
        }
        let kinds = self
            .pins
            .get(&id)
            .and_then(|p| p.inputs.get(pin.id.input))
            .map_or(KindSet::of(&DataKind::ALL), |p| p.kinds);
        super::badges::StripedPin::new(self.tokens, kinds)
    }

    fn show_output(
        &mut self,
        pin: &OutPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<u32>,
    ) -> impl egui_snarl::ui::SnarlPin + 'static {
        let id = snarl.get_node(pin.id.node).copied().unwrap_or(0);
        if self.cull && offscreen(ui, ui.next_widget_position()) {
            ui.allocate_space(egui::vec2(40.0, 16.0));
        } else {
            ui.label(self.output_name(id, pin.id.output));
        }
        let kinds = self
            .pins
            .get(&id)
            .and_then(|p| p.outputs.get(pin.id.output))
            .map_or(KindSet::of(&DataKind::ALL), |p| p.kinds);
        super::badges::StripedPin::new(self.tokens, kinds)
    }

    fn has_body(&mut self, node: &u32) -> bool {
        self.report.contains_key(node) || self.graph.nodes.get(node).is_some_and(|n| n.ty == "note")
    }

    fn show_body(
        &mut self,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<u32>,
    ) {
        let Some(id) = snarl.get_node(node).copied() else {
            return;
        };
        if self.cull && offscreen(ui, ui.next_widget_position()) {
            ui.allocate_space(egui::vec2(60.0, 16.0));
            return;
        }
        if let Some(n) = self.graph.nodes.get(&id)
            && n.ty == "note"
        {
            ui.weak(
                n.settings
                    .get("text")
                    .and_then(SettingValue::as_text)
                    .unwrap_or(""),
            );
            return;
        }
        if let Some((ms, elements, _, inactive)) = self.report.get(&id) {
            let text = if *inactive {
                "inactive".to_string()
            } else {
                format!("{elements} el · {ms:.2} ms")
            };
            ui.weak(text);
        }
    }

    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<u32>) {
        let (Some(a), Some(b)) = (
            snarl.get_node(from.id.node).copied(),
            snarl.get_node(to.id.node).copied(),
        ) else {
            return;
        };
        let out = self.output_name(a, from.id.output).to_string();
        let inp = self.input_name(b, to.id.input).to_string();
        self.edits.push(PanelEdit::Connect((a, out), (b, inp)));
        snarl.connect(from.id, to.id);
    }

    fn disconnect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<u32>) {
        let (Some(a), Some(b)) = (
            snarl.get_node(from.id.node).copied(),
            snarl.get_node(to.id.node).copied(),
        ) else {
            return;
        };
        let out = self.output_name(a, from.id.output).to_string();
        let inp = self.input_name(b, to.id.input).to_string();
        self.edits.push(PanelEdit::Disconnect((a, out), (b, inp)));
        snarl.disconnect(from.id, to.id);
    }

    fn has_graph_menu(&mut self, _pos: egui::Pos2, _snarl: &mut Snarl<u32>) -> bool {
        true
    }

    fn show_graph_menu(&mut self, pos: egui::Pos2, ui: &mut egui::Ui, _snarl: &mut Snarl<u32>) {
        ui.label("Add node");
        let mut by_cat: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for k in self.registry.iter() {
            by_cat.entry(k.category()).or_default().push(k.id());
        }
        for (cat, ids) in by_cat {
            ui.menu_button(cat, |ui| {
                for id in ids {
                    if ui.button(id).clicked() {
                        self.edits.push(PanelEdit::Add(
                            id.to_string(),
                            (f64::from(pos.x), f64::from(pos.y)),
                        ));
                        ui.close();
                    }
                }
            });
        }
        for aid in ["note", "reroute"] {
            if ui.button(aid).clicked() {
                self.edits.push(PanelEdit::Add(
                    aid.to_string(),
                    (f64::from(pos.x), f64::from(pos.y)),
                ));
                ui.close();
            }
        }
    }

    fn has_dropped_wire_menu(&mut self, src_pins: AnyPins, _snarl: &mut Snarl<u32>) -> bool {
        matches!(src_pins, AnyPins::Out(_))
    }

    fn show_dropped_wire_menu(
        &mut self,
        pos: egui::Pos2,
        ui: &mut egui::Ui,
        src_pins: AnyPins,
        snarl: &mut Snarl<u32>,
    ) {
        // Dropping a wire from an output pin on empty canvas: the palette filtered to types with a
        // compatible input; choosing one adds and connects it (spec 67 § 2.1).
        let AnyPins::Out(pins) = src_pins else {
            return;
        };
        let Some(first) = pins.first() else {
            return;
        };
        let Some(node) = snarl.get_node(first.node).copied() else {
            return;
        };
        let pin = self.output_name(node, first.output).to_string();
        let kinds = self
            .pins
            .get(&node)
            .and_then(|p| p.outputs.get(first.output))
            .map(|p| p.kinds);
        ui.label("Add and connect");
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            for e in doc::palette(self.registry, "", kinds) {
                if let Some(input) = e.pin
                    && ui.button(&e.ty).on_hover_text(&e.doc).clicked()
                {
                    self.edits.push(PanelEdit::AddConnected(
                        e.ty,
                        (f64::from(pos.x), f64::from(pos.y)),
                        (node, pin.clone()),
                        input,
                    ));
                    ui.close();
                }
            }
        });
    }

    fn has_node_menu(&mut self, _node: &u32) -> bool {
        true
    }

    fn show_node_menu(
        &mut self,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<u32>,
    ) {
        let Some(id) = snarl.get_node(node).copied() else {
            return;
        };
        let enabled = self.graph.nodes.get(&id).is_some_and(|n| n.enabled);
        if ui
            .button(if enabled { "Bypass (B)" } else { "Enable (B)" })
            .clicked()
        {
            self.edits.push(PanelEdit::Bypass(id, !enabled));
            ui.close();
        }
        if ui.button("Visualize (V)").clicked() {
            self.edits.push(PanelEdit::Visualize(id));
            ui.close();
        }
        if ui.button("Inspect (I)").clicked() {
            self.edits.push(PanelEdit::Inspect(id));
            ui.close();
        }
        if ui.button("Duplicate").clicked() {
            self.edits.push(PanelEdit::Duplicate(BTreeSet::from([id])));
            ui.close();
        }
        if ui.button("Delete").clicked() {
            self.edits.push(PanelEdit::Remove(BTreeSet::from([id])));
            ui.close();
        }
        ui.separator();
        ui.label("Collapse to subgraph");
        ui.text_edit_singleline(self.collapse_path);
        let ids = if self.selection.contains(&id) {
            self.selection.clone()
        } else {
            BTreeSet::from([id])
        };
        // The boundary check runs before the click, so the button says why it is off.
        let check = doc::collapse_check(self.graph, &ids);
        let r = ui.add_enabled(
            check.is_ok() && !self.collapse_path.is_empty(),
            egui::Button::new("Collapse"),
        );
        let r = match &check {
            Err(e) => r.on_disabled_hover_text(&e.message),
            Ok(()) => r,
        };
        if r.clicked() {
            self.edits
                .push(PanelEdit::Collapse(ids, self.collapse_path.clone()));
            ui.close();
        }
        if let Err(e) = &check {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 70), &e.message);
        }
        if self.graph.nodes.get(&id).is_some_and(|n| n.ty == "graph.call") {
            if ui.button("Expand").clicked() {
                self.edits.push(PanelEdit::Expand {
                    node: id,
                    copy: false,
                });
                ui.close();
            }
            if ui.button("Expand as copy").clicked() {
                self.edits.push(PanelEdit::Expand { node: id, copy: true });
                ui.close();
            }
        }
        if ui.button("Isolate").clicked() {
            self.edits.push(PanelEdit::Isolate(Some(id)));
            ui.close();
        }
    }

    fn current_transform(&mut self, to_global: &mut egui::emath::TSTransform, _snarl: &mut Snarl<u32>) {
        if let Some(tf) = self.force_tf {
            *to_global = tf;
        }
        *self.seen_tf = Some(*to_global);
    }
}

/// The badges after a node's title (spec 67 § 2.2): a gray stripe when bypassed, `eye`
/// (visualized), `table` (inspected), the tier's cell size, a dot for a cache hit, the warning
/// count and a red error mark, each with a hover text.
fn badge_row(ui: &mut egui::Ui, b: &super::badges::NodeBadges) {
    use super::badges::Badge;
    use crate::graph_kit::badges::Severity;
    for badge in &b.list {
        match badge {
            Badge::Bypassed => {
                ui.colored_label(egui::Color32::from_gray(120), "▮")
                    .on_hover_text("bypassed (B)");
            }
            Badge::Visualized => {
                ui.small("eye").on_hover_text("visualized in the viewport (V)");
            }
            Badge::Inspected => {
                ui.small("table").on_hover_text("shown in the data table (I)");
            }
            Badge::Tier(t) if t.shared => {
                let tiers: Vec<String> = t.feeds_list().iter().map(u8::to_string).collect();
                ui.small(t.label()).on_hover_text(format!(
                    "shared: runs in tiers {} (a tier-neutral node, spec 66)",
                    tiers.join(", ")
                ));
            }
            Badge::Tier(t) if t.neutral => {
                ui.small(t.label()).on_hover_text(format!(
                    "tier-neutral: runs in tier {}, the tier of its consumers (spec 66)",
                    t.index
                ));
            }
            Badge::Tier(t) => {
                ui.small(t.label())
                    .on_hover_text(format!("tier {} (spec 66)", t.index));
            }
            Badge::CacheHit => {
                ui.small("•").on_hover_text("served from the cache");
            }
            Badge::Warnings(n) => {
                // Spoken as the count and what it counts (graph_kit::badges).
                let spoken = format!("{n} warnings in the last run (run report)");
                crate::graph_kit::badges::count_badge(ui, Severity::Warning, *n, &spoken);
            }
            Badge::Error { code, message } => {
                // Spoken as the error text, not the glyph alone (FU-7).
                crate::graph_kit::badges::badge(ui, Severity::Error, &format!("{code}: {message}"));
            }
        }
    }
}

/// `only` is a sorted list of graph ids. `None` inserts every node. `pos_override` keeps a drag
/// that has not been written back to the document yet (snarl has no position setter after insert).
fn build_snarl(
    graph: &ProcGraph,
    pins: &BTreeMap<u32, PinSpec>,
    only: Option<&[u32]>,
    pos_override: &BTreeMap<u32, egui::Pos2>,
) -> Snarl<u32> {
    let mut snarl = Snarl::new();
    let mut map = BTreeMap::new();
    let want = |id: u32| only.is_none_or(|ids| ids.binary_search(&id).is_ok());
    for (id, n) in &graph.nodes {
        if !want(*id) {
            continue;
        }
        let pos = pos_override
            .get(id)
            .copied()
            .unwrap_or_else(|| egui::pos2(n.at.0 as f32, n.at.1 as f32));
        let sid = snarl.insert_node(pos, *id);
        map.insert(*id, sid);
    }
    let out_index = |id: u32, name: &str| -> Option<usize> {
        match graph.nodes.get(&id).map(|n| n.ty.as_str()) {
            Some("reroute") => Some(0),
            _ => pins.get(&id)?.output(name),
        }
    };
    let in_index = |id: u32, name: &str| -> Option<usize> {
        match graph.nodes.get(&id).map(|n| n.ty.as_str()) {
            Some("reroute") => Some(0),
            _ => pins.get(&id)?.input(name),
        }
    };
    for e in &graph.edges {
        if let (Some(&a), Some(&b), Some(o), Some(i)) = (
            map.get(&e.from.0),
            map.get(&e.to.0),
            out_index(e.from.0, &e.from.1),
            in_index(e.to.0, &e.to.1),
        ) {
            snarl.connect(
                egui_snarl::OutPinId { node: a, output: o },
                egui_snarl::InPinId { node: b, input: i },
            );
        }
    }
    snarl
}

/// The per-node pins of a graph (what the canvas draws): the compiler's pins plus the override
/// pins shown on request.
#[must_use]
pub fn graph_pins(world: &World, graph: &ProcGraph) -> BTreeMap<u32, PinSpec> {
    let registry = doc::registry(world);
    let load = |p: &str| doc::graph_text(world, p);
    let shown = world
        .get_resource::<ProcGraphDoc>()
        .map(|d| d.shown_pins.clone())
        .unwrap_or_default();
    graph
        .nodes
        .keys()
        .filter_map(|id| {
            let mut pins = graph.node_pins(*id, &registry, &load)?;
            for (_, setting) in shown.iter().filter(|(n, _)| n == id) {
                let name = format!("@{setting}");
                if pins.input(&name).is_none() {
                    pins.inputs.push(regia_procgen::nodes::PinDesc {
                        name,
                        kinds: KindSet::TABLE,
                        multi: false,
                        required: false,
                    });
                }
            }
            Some((*id, pins))
        })
        .collect()
}

/// Nodes the canvas drew in the last panel frame (0 before the first frame or with no graph open).
#[must_use]
pub fn nodes_drawn(world: &World) -> usize {
    world.get_resource::<ProcPanelUi>().map_or(0, |p| p.nodes_drawn)
}

/// Nodes the canvas skipped drawing (off screen) in the last panel frame.
#[must_use]
pub fn nodes_culled(world: &World) -> usize {
    world.get_resource::<ProcPanelUi>().map_or(0, |p| p.nodes_culled)
}

/// `(rows laid out, rows available)` of the data table in the last panel frame.
#[must_use]
pub fn table_rows(world: &World) -> (usize, usize) {
    world
        .get_resource::<ProcPanelUi>()
        .map_or((0, 0), |p| (p.rows_drawn, p.rows_total))
}

/// Shows the side Parameters tab (else the Node tab) and the bottom Data table tab (else the run
/// report), as the panel's tab buttons do.
pub fn set_tabs(world: &mut World, parameters: bool, table: bool) {
    let mut panel = world.remove_resource::<ProcPanelUi>().unwrap_or_default();
    panel.side_params = parameters;
    panel.bottom_table = table;
    world.insert_resource(panel);
}

/// Parameter rows the Parameters tab drew in the last panel frame.
#[must_use]
pub fn param_rows_drawn(world: &World) -> usize {
    world
        .get_resource::<ProcPanelUi>()
        .map_or(0, |p| p.params_tab.rows_drawn)
}

/// Whether the panel culls off-screen nodes and table rows (default true).
#[must_use]
pub fn culling(world: &World) -> bool {
    world.get_resource::<ProcPanelUi>().is_none_or(|p| !p.no_cull)
}

/// Turns culling of off-screen nodes and table rows on or off (for measuring).
pub fn set_culling(world: &mut World, on: bool) {
    let mut panel = world.remove_resource::<ProcPanelUi>().unwrap_or_default();
    panel.no_cull = !on;
    world.insert_resource(panel);
}

/// Draws the panel against `world` and applies its edits.
#[tracing::instrument(target = "regia_editor", skip_all, name = "editor.procgen_panel")]
pub fn procgen_graph_panel(ui: &mut egui::Ui, world: &mut World) {
    super::install_if_missing(world);
    let edits = draw(ui, world, CaptureView::Canvas);
    let pointer_down = ui.input(|i| i.pointer.any_down());
    // egui may run the UI twice in one frame (a discarded pass): apply one pass's edits per frame.
    let frame = ui.ctx().cumulative_frame_nr();
    let mut applied = world.get_resource_or_insert_with(PanelAppliedFrame::default);
    if applied.0 == Some(frame) {
        return;
    }
    applied.0 = Some(frame);
    apply(world, edits, pointer_down);
}

/// The egui frame whose panel edits were last applied.
#[derive(Resource, Default)]
struct PanelAppliedFrame(Option<u64>);

fn draw(ui: &mut egui::Ui, world: &mut World, view: CaptureView) -> Vec<PanelEdit> {
    let mut edits = Vec::new();
    let mut panel = world.remove_resource::<ProcPanelUi>().unwrap_or_default();
    {
        let world: &World = world;
        ui.horizontal(|ui| new_graph_ui(ui, world, &mut panel, &mut edits));
        if let Some(d) = world.get_resource::<ProcGraphDoc>() {
            toolbar(ui, world, d, &mut edits);
            tool_row(ui, world, &mut panel, &mut edits);
            if d.path.is_none() {
                // Wraps in a narrow dock (bug 19, ADR-8586).
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Open a .pgraph.ron from the Content Browser to edit it here.")
                            .weak(),
                    )
                    .wrap(),
                );
            } else {
                match view {
                    CaptureView::Canvas => {
                        body(ui, world, d, &mut panel, &mut edits);
                    }
                    CaptureView::Table => table_ui(ui, world, &mut panel, &mut edits),
                    CaptureView::Report => report_ui(ui, world, d),
                }
            }
        } else {
            ui.weak("Procedural graphs are not available.");
        }
    }
    world.insert_resource(panel);
    edits
}

fn new_graph_ui(ui: &mut egui::Ui, world: &World, panel: &mut ProcPanelUi, edits: &mut Vec<PanelEdit>) {
    ui.menu_button("New…", |ui| {
        let label = ui.label("Path (.pgraph.ron)");
        ui.text_edit_singleline(&mut panel.new_path).labelled_by(label.id);
        let picker = world
            .get_resource::<super::ProcEditorPrefs>()
            .is_none_or(|p| p.starter_picker);
        if picker {
            let current = panel.new_starter.clone().unwrap_or_else(|| "(blank)".into());
            let combo = egui::ComboBox::from_id_salt("procgen-new-starter")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(panel.new_starter.is_none(), "(blank)")
                        .clicked()
                    {
                        panel.new_starter = None;
                    }
                    for s in doc::starters(world) {
                        let label = match &s.tool {
                            Some(t) => format!("{} ({t} tool)", s.path),
                            None => s.path.clone(),
                        };
                        if ui
                            .selectable_label(panel.new_starter.as_deref() == Some(s.path.as_str()), label)
                            .on_hover_text(&s.description)
                            .clicked()
                        {
                            panel.new_starter = Some(s.path);
                        }
                    }
                });
            crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "New graph starter");
        }
        if ui.button("Create").clicked() && !panel.new_path.is_empty() {
            edits.push(PanelEdit::New(panel.new_path.clone(), panel.new_starter.clone()));
            ui.close();
        }
    });
}

fn toolbar(ui: &mut egui::Ui, world: &World, d: &ProcGraphDoc, edits: &mut Vec<PanelEdit>) {
    // Wraps in a narrow dock instead of cutting the tier picker off (bug 19, ADR-8586).
    ui.horizontal_wrapped(|ui| {
        let mut starter = d.graph.flags.starter;
        if d.path.is_some()
            && !d.read_only()
            && ui
                .checkbox(&mut starter, "starter")
                .on_hover_text("List this graph in the New procedural graph picker (flags.starter)")
                .changed()
        {
            edits.push(PanelEdit::SetStarter(starter));
        }
        ui.strong(d.path.as_deref().unwrap_or("(no graph)"));
        if d.dirty() {
            ui.weak("● unsaved");
        }
        if d.read_only() {
            ui.weak("variant: parent read-only");
        }
        if ui.button("Save").clicked() {
            edits.push(PanelEdit::Save);
        }
        if !d.errors.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(230, 80, 70),
                format!("{} compile error(s)", d.errors.len()),
            );
        }
        if !d.breadcrumb.is_empty() && ui.link(format!("‹ {}", d.breadcrumb.join(" › "))).clicked() {
            edits.push(PanelEdit::Back);
        }
        ui.weak(&d.toast);
        preview_bar(ui, world, edits);
    });
    if d.conflict.is_some() {
        ui.horizontal(|ui| {
            ui.colored_label(egui::Color32::from_rgb(240, 180, 60), "The file changed on disk.");
            if ui.button("Keep mine").clicked() {
                edits.push(PanelEdit::Resolve(false));
            }
            if ui.button("Take theirs").clicked() {
                edits.push(PanelEdit::Resolve(true));
            }
        });
    }
}

/// The viewport tool mode "Procedural" (spec 67 § 5): the kind, a preset whose `flags.tool` fits it
/// (shipped presets and the project's starters), the Area biome, the Paint brush, the kind's hit
/// filter (saved in the editor preferences), and Arm / Cancel.
fn tool_row(ui: &mut egui::Ui, world: &World, panel: &mut ProcPanelUi, edits: &mut Vec<PanelEdit>) {
    use super::tools::{ALL_KINDS, ToolKind};
    let armed = world.get_resource::<super::tools::ProcViewportMode>();
    ui.horizontal(|ui| {
        ui.label("Procedural tool");
        let kind = panel.tool_kind.unwrap_or(ToolKind::Path);
        for k in ALL_KINDS {
            if ui.selectable_label(kind == k, k.name()).clicked() && kind != k {
                panel.tool_kind = Some(k);
                panel.tool_preset.clear();
            }
        }
        let kind = panel.tool_kind.unwrap_or(ToolKind::Path);
        if panel.tool_preset.is_empty()
            && let Some(first) = super::presets::for_kind(kind).next()
        {
            panel.tool_preset = first.to_string();
        }
        let combo = egui::ComboBox::from_id_salt("procgen-tool-preset")
            .selected_text(panel.tool_preset.as_str())
            .show_ui(ui, |ui| {
                // Listed only while the list is open: the project's starters are read from disk.
                let mut presets: Vec<String> = super::presets::for_kind(kind).map(str::to_string).collect();
                for s in doc::starters(world) {
                    if s.tool.as_deref() == Some(kind.name()) && !presets.contains(&s.path) {
                        presets.push(s.path);
                    }
                }
                for p in &presets {
                    if ui.selectable_label(panel.tool_preset == *p, p).clicked() {
                        panel.tool_preset.clone_from(p);
                    }
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Tool preset");
        if kind == ToolKind::Area {
            // The project's `.biome.ron` files (w11-biome-procgen-tooling item 1): listed only while
            // the list is open, since they are read from disk; the Biome editor edits them.
            let label = if panel.tool_biome.is_empty() {
                "(the preset's biome)"
            } else {
                panel.tool_biome.as_str()
            };
            let combo = egui::ComboBox::from_id_salt("procgen-tool-biome")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(panel.tool_biome.is_empty(), "(the preset's biome)")
                        .clicked()
                    {
                        panel.tool_biome.clear();
                    }
                    for path in crate::biome_panel::biome_files(world) {
                        if ui
                            .selectable_label(panel.tool_biome == path, path.as_str())
                            .clicked()
                        {
                            panel.tool_biome = path;
                        }
                    }
                });
            crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Area biome");
        }
        if kind == ToolKind::Paint {
            let b = &mut panel.tool_brush;
            ui.add(
                egui::DragValue::new(&mut b.radius_m)
                    .range(0.05..=500.0)
                    .prefix("r ")
                    .suffix(" m"),
            );
            ui.add(
                egui::DragValue::new(&mut b.spacing_m)
                    .range(0.05..=50.0)
                    .prefix("spacing ")
                    .suffix(" m"),
            );
            ui.add(
                egui::DragValue::new(&mut b.density)
                    .range(0.0..=1.0)
                    .speed(0.01)
                    .prefix("density "),
            );
            let mut layer = b.layer + 1;
            ui.add(egui::DragValue::new(&mut layer).range(1..=8).prefix("layer "));
            b.layer = layer.saturating_sub(1);
            ui.checkbox(&mut b.random_yaw, "random yaw");
            ui.add(egui::DragValue::new(&mut b.seed).prefix("seed "));
        }
        let is_armed = armed.is_some_and(|m| m.kind == kind && m.preset == panel.tool_preset);
        let label = if is_armed { "Update" } else { "Arm" };
        if !panel.tool_preset.is_empty()
            && ui
                .button(label)
                .on_hover_text("Draw with this tool in the viewport (Enter applies, Esc cancels)")
                .clicked()
        {
            edits.push(PanelEdit::ArmWith {
                kind,
                preset: panel.tool_preset.clone(),
                biome: (!panel.tool_biome.is_empty()).then(|| panel.tool_biome.clone()),
                brush: panel.tool_brush,
            });
        }
        if armed.is_some() && ui.button("Cancel tool").clicked() {
            edits.push(PanelEdit::CancelTool);
        }
        ui.separator();
        let current = super::tools::hit_filter(world, kind);
        let mut f = current.clone();
        ui.label("lands on");
        ui.checkbox(&mut f.terrain, "terrain");
        ui.checkbox(&mut f.meshes, "meshes");
        ui.checkbox(&mut f.skip_generated, "skip generated");
        if f != current {
            edits.push(PanelEdit::HitFilter(kind, f));
        }
        if let Some(m) = armed.filter(|m| !m.last_error.is_empty()) {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 70), &m.last_error);
        }
        let pending = super::tools::pending_strokes(world);
        if pending > 0 {
            ui.weak(format!("{pending} stroke points stamping"));
        }
    });
}

/// Extra graph-space margin around the canvas, pixels, so a node that straddles the edge still draws.
const CULL_MARGIN_PX: f32 = 320.0;

fn empty_errors() -> &'static BTreeMap<u32, String> {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<BTreeMap<u32, String>> = OnceLock::new();
    EMPTY.get_or_init(BTreeMap::new)
}

fn empty_report() -> &'static BTreeMap<u32, (f64, usize, bool, bool)> {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<BTreeMap<u32, (f64, usize, bool, bool)>> = OnceLock::new();
    EMPTY.get_or_init(BTreeMap::new)
}

/// The preview bar (spec 67 § 3): the "Preview on" picker, the seed (field, pin, new seed, same
/// seed as), the quality tier, isolate, and the generate-around-view and tiles switches. Every
/// control pushes an idempotent edit (a value, not a toggle), so a UI pass egui runs twice in one
/// frame cannot apply it twice.
fn preview_bar(ui: &mut egui::Ui, world: &World, edits: &mut Vec<PanelEdit>) {
    let preview = world.get_resource::<ProcPreview>();
    let target = preview.and_then(|p| p.target);
    let pinned = preview.is_some_and(|p| p.pinned_target);
    let tier = preview.and_then(|p| p.tier);
    let isolated = preview.and_then(|p| p.isolate.as_ref().map(|i| i.node));
    let (around, tiles) = preview.map_or((false, false), |p| (p.generate_around_view, p.tiles_overlay));
    ui.separator();
    let name_of = |e: Entity| {
        world
            .get::<regia_scene::components::Name>(e)
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|| format!("{}", e.to_bits()))
    };
    // Listed only while a list is open (a level can hold many generators).
    let generators = || -> Vec<(Entity, String)> {
        world
            .iter_entities()
            .filter(|entity| entity.get::<regia_procgen::world::ProcGenerator>().is_some())
            .map(|entity| (entity.id(), name_of(entity.id())))
            .collect()
    };
    let shown = match (target, pinned) {
        (Some(e), true) => name_of(e),
        (Some(e), false) => format!("{} (selection)", name_of(e)),
        (None, true) => "none".to_string(),
        (None, false) => "follow selection".to_string(),
    };
    let combo = egui::ComboBox::from_id_salt("procgen-preview-on")
        .selected_text(format!("Preview on: {shown}"))
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(!pinned, "Follow selection")
                .on_hover_text("The selected entity's generator, else the last one used")
                .clicked()
            {
                edits.push(PanelEdit::Action(PanelAction::FollowSelection));
            }
            if ui.selectable_label(pinned && target.is_none(), "None").clicked() {
                edits.push(PanelEdit::Action(PanelAction::PreviewOn(None)));
            }
            for (e, name) in &generators() {
                if ui.selectable_label(pinned && target == Some(*e), name).clicked() {
                    edits.push(PanelEdit::Action(PanelAction::PreviewOn(Some(*e))));
                }
            }
        });
    crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Preview on");
    let tier_label = tier.map(|t| t.name()).unwrap_or("Running");
    let combo = egui::ComboBox::from_id_salt("procgen-tier")
        .selected_text(tier_label)
        .show_ui(ui, |ui| {
            if ui.selectable_label(tier.is_none(), "Running").clicked() {
                edits.push(PanelEdit::Tier(None));
            }
            for t in regia_render::quality::QualityTier::ALL {
                if ui.selectable_label(tier == Some(t), t.name()).clicked() {
                    edits.push(PanelEdit::Tier(Some(t)));
                }
            }
        });
    crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Quality tier");
    let doc_selection = world.get_resource::<ProcGraphDoc>().and_then(|d| {
        (d.selection.len() == 1)
            .then(|| d.selection.iter().next().copied())
            .flatten()
    });
    match (isolated, doc_selection) {
        (Some(node), _) => {
            if ui.button(format!("End isolate #{node}")).clicked() {
                edits.push(PanelEdit::Isolate(None));
            }
        }
        (None, Some(node)) => {
            if ui
                .button("Isolate")
                .on_hover_text("Preview up to the selected node; the generator's output is kept")
                .clicked()
            {
                edits.push(PanelEdit::Isolate(Some(node)));
            }
        }
        (None, None) => {}
    }
    let mut around_now = around;
    if ui
        .checkbox(&mut around_now, "Around view")
        .on_hover_text("The editor camera is an anchor for Anchored generators")
        .changed()
    {
        edits.push(PanelEdit::Action(PanelAction::ViewSwitches {
            around: Some(around_now),
            tiles: None,
        }));
    }
    let mut tiles_now = tiles;
    if ui
        .checkbox(&mut tiles_now, "Tiles")
        .on_hover_text("Outline every cell of the tiled generators, colored by state")
        .changed()
    {
        edits.push(PanelEdit::Action(PanelAction::ViewSwitches {
            around: None,
            tiles: Some(tiles_now),
        }));
    }
    let Some(target) = target else {
        ui.weak("Bind a preview to set its seed");
        return;
    };
    let current = regia_procgen::world::generator_seed(world, target);
    let mut seed = current;
    ui.label("Seed");
    if ui
        .add(egui::DragValue::new(&mut seed).speed(1.0))
        .on_hover_text("The generator's seed (pins it)")
        .changed()
        && seed != current
    {
        edits.push(PanelEdit::Seed {
            entity: target,
            action: doc::SeedAction::Set(seed),
        });
    }
    if ui.button("Pin").clicked() {
        edits.push(PanelEdit::Seed {
            entity: target,
            action: doc::SeedAction::Pin,
        });
    }
    if ui.button("New seed").clicked() {
        // Idempotent within a frame: the value is computed from the seed drawn this frame.
        edits.push(PanelEdit::Seed {
            entity: target,
            action: doc::SeedAction::Set(regia_core::math::hash::pcg_hash(current ^ 0x9e37_79b9)),
        });
    }
    let has_others = world
        .iter_entities()
        .any(|entity| entity.id() != target && entity.get::<regia_procgen::world::ProcGenerator>().is_some());
    if has_others {
        let combo = egui::ComboBox::from_id_salt("procgen-same-seed")
            .selected_text("Same seed as")
            .show_ui(ui, |ui| {
                for (e, name) in generators().iter().filter(|(e, _)| *e != target) {
                    if ui.button(name).clicked() {
                        edits.push(PanelEdit::Seed {
                            entity: target,
                            action: doc::SeedAction::SameAs(*e),
                        });
                    }
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Same seed as");
    }
}

fn body(
    ui: &mut egui::Ui,
    world: &World,
    d: &ProcGraphDoc,
    panel: &mut ProcPanelUi,
    edits: &mut Vec<PanelEdit>,
) {
    let registry = doc::registry(world);
    let pins = match &panel.pins {
        Some((rev, pins)) if *rev == d.revision => pins.clone(),
        _ => {
            let pins = std::sync::Arc::new(graph_pins(world, &d.graph));
            panel.pins = Some((d.revision, pins.clone()));
            pins
        }
    };
    let errors_owned;
    let errors: &BTreeMap<u32, String> = if d.errors.is_empty() {
        empty_errors()
    } else {
        errors_owned = d
            .errors
            .iter()
            .filter_map(|e| e.node.map(|n| (n, format!("{}: {}", e.code, e.message))))
            .collect();
        &errors_owned
    };
    let target = world.get_resource::<ProcPreview>().and_then(|p| p.target);
    let report_owned;
    let report: &BTreeMap<u32, (f64, usize, bool, bool)> = match target
        .and_then(|t| world.get::<regia_procgen::world::ProcGenState>(t))
        .and_then(|s| s.report.as_ref())
    {
        Some(r) if !r.nodes.is_empty() => {
            report_owned = r
                .nodes
                .iter()
                .map(|n| (n.id, (n.ms, n.elements, n.cache_hit, n.inactive)))
                .collect();
            &report_owned
        }
        _ => empty_report(),
    };
    // Node badges: rebuilt only when what they read changes (a compile per rebuild).
    let key = {
        let preview = world.get_resource::<ProcPreview>();
        BadgesKey {
            revision: d.revision,
            target,
            run_id: target
                .and_then(|t| world.get::<regia_procgen::world::ProcGenState>(t))
                .map_or(0, |s| s.run_id),
            inspected: world.get_resource::<ProcInspect>().and_then(|i| i.node),
            visualized: preview.map_or(0, |p| {
                p.visualize.keys().fold(0_u64, |h, id| {
                    h.wrapping_mul(0x9e37_79b9).wrapping_add(u64::from(*id) + 1)
                })
            }),
            tiles: target
                .and_then(|t| world.get::<regia_procgen::world::ProcGenerator>(t))
                .and_then(|g| g.tiles.as_ref())
                .map(|t| t.cell_m.to_bits()),
        }
    };
    if panel.badges_key.as_ref() != Some(&key) {
        panel.badges = std::sync::Arc::new(super::badges::node_badges(world, None));
        panel.badges_key = Some(key);
    }
    let badges = panel.badges.clone();
    let tokens = crate::theme::ui_tokens(ui.ctx());

    // Keyboard: V visualize, I inspect, B bypass, Delete removes the selection (with a row of the
    // Nodes list focused, Delete removes that row's node instead).
    if !d.selection.is_empty() && !ui.ctx().egui_wants_keyboard_input() {
        let (v, i, b, del) = ui.input(|inp| {
            (
                inp.key_pressed(egui::Key::V),
                inp.key_pressed(egui::Key::I),
                inp.key_pressed(egui::Key::B),
                inp.key_pressed(egui::Key::Delete),
            )
        });
        for id in &d.selection {
            if v {
                edits.push(PanelEdit::Visualize(*id));
            }
            if b {
                let on = d.graph.nodes.get(id).is_some_and(|n| n.enabled);
                edits.push(PanelEdit::Bypass(*id, !on));
            }
        }
        if i && let Some(id) = d.selection.iter().next() {
            edits.push(PanelEdit::Inspect(*id));
        }
        if del && !panel.keys.list.row_focused {
            edits.push(PanelEdit::Remove(d.selection.clone()));
        }
    }
    // Clipboard: copy the selection as a RON fragment, paste one, duplicate (Ctrl+D).
    if !ui.ctx().egui_wants_keyboard_input() {
        let (copy, paste, dup) = ui.input(|i| {
            let copy = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            let paste = i.events.iter().find_map(|e| match e {
                egui::Event::Paste(t) => Some(t.clone()),
                _ => None,
            });
            (copy, paste, i.modifiers.command && i.key_pressed(egui::Key::D))
        });
        if copy && !d.selection.is_empty() {
            let frag = doc::copy_fragment(&d.graph, &d.selection);
            ui.ctx().copy_text(frag.clone());
            edits.push(PanelEdit::Clip(frag));
        }
        if let Some(text) = paste {
            edits.push(PanelEdit::Paste(text));
        }
        if dup && !d.selection.is_empty() {
            edits.push(PanelEdit::Duplicate(d.selection.clone()));
        }
    }

    egui::Panel::left("procgen-nodes")
        .resizable(true)
        .default_size(200.0)
        .show(ui, |ui| nodes_ui(ui, d, &registry, &pins, panel, edits));
    egui::Panel::left("procgen-palette")
        .resizable(true)
        .default_size(180.0)
        .show(ui, |ui| palette_ui(ui, &registry, panel, edits));
    egui::Panel::right("procgen-settings")
        .resizable(true)
        .default_size(260.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut panel.side_params, false, "Node");
                ui.selectable_value(&mut panel.side_params, true, "Parameters");
            });
            ui.separator();
            if panel.side_params {
                let mut actions = Vec::new();
                super::params_tab::params_ui(ui, d, &mut panel.params_tab, &mut actions);
                edits.extend(actions.into_iter().map(PanelEdit::Param));
            } else {
                settings_ui(ui, world, d, &registry, panel, edits);
            }
        });
    if d.graph.flags.domain == regia_procgen::nodes::Domain::Plant {
        // Spec 68 § 8: a Plant graph previews in an asset viewport with a six-seed variants strip.
        egui::Panel::bottom("procgen-plant")
            .resizable(true)
            .default_size(250.0)
            .show(ui, |ui| {
                let mut clicked = None;
                super::plant_preview::plant_ui(ui, world, &mut clicked);
                if let Some(seed) = clicked {
                    edits.push(PanelEdit::PlantSeed(seed));
                }
            });
    }
    egui::Panel::bottom("procgen-bottom")
        .resizable(true)
        .default_size(140.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut panel.bottom_table, false, "Run report");
                ui.selectable_value(&mut panel.bottom_table, true, "Data table");
            });
            if panel.bottom_table {
                table_ui(ui, world, panel, edits);
            } else {
                report_ui(ui, world, d);
            }
        });

    // Graph space visible through the canvas, plus a margin so a node that straddles the edge
    // still draws. The selection always draws, even off screen.
    let canvas = ui.available_rect_before_wrap();
    let graph_rect = panel
        .view_tf
        .unwrap_or(egui::emath::TSTransform::IDENTITY)
        .inverse()
        .mul_rect(canvas)
        .expand(CULL_MARGIN_PX);
    panel.visible_buf.clear();
    for (id, n) in &d.graph.nodes {
        let p = egui::pos2(n.at.0 as f32, n.at.1 as f32);
        // With culling off (`editor.procgen_panel_culling`), every node is in the canvas.
        if panel.no_cull || d.selection.contains(id) || graph_rect.contains(p) {
            panel.visible_buf.push(*id);
        }
    }
    let reuse =
        panel.shown.is_some() && panel.shown_rev == Some(d.revision) && panel.shown_ids == panel.visible_buf;
    let mut carried = BTreeMap::new();
    let snarl = if reuse {
        panel.shown.take()
    } else {
        if panel.shown_rev == Some(d.revision)
            && let Some(old) = panel.shown.take()
        {
            for (_, pos, id) in old.nodes_pos_ids() {
                if panel.visible_buf.binary_search(id).is_ok() {
                    carried.insert(*id, pos);
                }
            }
        } else {
            panel.shown = None;
        }
        panel.shown_ids.clone_from(&panel.visible_buf);
        panel.shown_rev = Some(d.revision);
        Some(build_snarl(
            &d.graph,
            &pins,
            Some(panel.visible_buf.as_slice()),
            &carried,
        ))
    };
    let Some(mut snarl) = snarl else {
        return;
    };
    // The first frame is 1:1 on the graph origin. Fitting every node would pull the off-screen
    // ones into view and defeat the cull (spec 67 § 9).
    let force_tf =
        (!panel.canvas_framed).then(|| egui::emath::TSTransform::from_translation(canvas.min.to_vec2()));
    let mut seen = panel.view_tf;
    // Enter in the Nodes list: frame the node under a fresh canvas id once it is in the canvas
    // (ADR-7383).
    if let Some(id) = panel.frame_id
        && let Some(node) = snarl.node_ids().find(|(_, n)| **n == id).map(|(n, _)| n)
    {
        panel.frame_id = None;
        panel.keys.frame = Some(node);
        panel.keys.frame_epoch += 1;
    }
    let salt = (
        "procgen-canvas",
        d.path.as_deref().unwrap_or(""),
        panel.keys.frame_epoch,
    );
    let drawn = {
        let mut viewer = Viewer {
            graph: &d.graph,
            registry: &registry,
            pins: &pins,
            errors,
            report,
            badges: &badges,
            tokens: &tokens,
            selection: &d.selection,
            edits,
            drawn: 0,
            culled: 0,
            cull: !panel.no_cull,
            collapse_path: &mut panel.collapse_path,
            force_tf,
            seen_tf: &mut seen,
        };
        let style = SnarlStyle {
            collapsible: Some(false),
            ..SnarlStyle::new()
        };
        // Enter in the Nodes list frames its node (graph_kit::canvas).
        crate::graph_kit::canvas::show(
            ui,
            &SnarlWidget::new().id_salt(salt).style(style),
            &mut snarl,
            &mut viewer,
            panel.keys.frame.take(),
            &format!("Procedural graph canvas, {} nodes", d.graph.nodes.len()),
        );
        (viewer.drawn, viewer.culled)
    };
    panel.nodes_drawn = drawn.0;
    // Culled: nodes outside the visible set (never put in the canvas) plus those the canvas skipped.
    panel.nodes_culled = drawn.1 + d.graph.nodes.len().saturating_sub(panel.visible_buf.len());
    if let Some(tf) = seen {
        panel.view_tf = Some(tf);
    }
    panel.canvas_framed = true;
    // Box select on the canvas becomes the document's selection.
    let boxed: BTreeSet<u32> = SnarlWidget::new()
        .id_salt(salt)
        .get_selected_nodes(ui)
        .into_iter()
        .filter_map(|n| snarl.get_node(n).copied())
        .collect();
    if boxed.len() > 1 && boxed != d.selection {
        edits.push(PanelEdit::Select(boxed));
    }
    for (_, pos, id) in snarl.nodes_pos_ids() {
        if let Some(n) = d.graph.nodes.get(id)
            && ((n.at.0 as f32 - pos.x).abs() > 0.5 || (n.at.1 as f32 - pos.y).abs() > 0.5)
        {
            edits.push(PanelEdit::Move(*id, (f64::from(pos.x), f64::from(pos.y))));
        }
    }
    panel.shown = Some(snarl);
}

/// `(rows laid out, rows)` of the Nodes list in the last panel frame (it lays out only the rows in
/// view).
#[must_use]
pub fn node_rows(world: &World) -> (usize, usize) {
    world
        .get_resource::<ProcPanelUi>()
        .map_or((0, 0), |p| (p.keys.list.drawn, p.keys.list.total))
}

/// The row (and accessible) name of node `id`: its type and id.
#[must_use]
pub fn node_row_name(graph: &ProcGraph, id: u32) -> String {
    graph
        .nodes
        .get(&id)
        .map_or_else(|| format!("#{id}"), |n| format!("{} #{id}", n.ty))
}

/// The Nodes list beside the canvas: the keyboard route to add, wire, move, delete and select
/// nodes (ADR-7383). Its edits are the ones `procgen.graph_edit` makes.
fn nodes_ui(
    ui: &mut egui::Ui,
    d: &ProcGraphDoc,
    registry: &ProcNodeRegistry,
    pins: &BTreeMap<u32, PinSpec>,
    panel: &mut ProcPanelUi,
    edits: &mut Vec<PanelEdit>,
) {
    use crate::canvas_keys::{GraphAction, GraphRows, graph_keys_ui};
    let ids: Vec<u32> = d.graph.nodes.keys().copied().collect();
    crate::graph_kit::canvas::follow_anchor(&mut panel.keys, panel.keys_node.as_ref(), &ids);
    let selected = (d.selection.len() == 1)
        .then(|| d.selection.iter().next())
        .flatten()
        .and_then(|s| ids.iter().position(|n| n == s));
    let first_output = |id: u32| -> Option<(String, KindSet)> {
        if d.graph.nodes.get(&id).is_some_and(|n| n.ty == "reroute") {
            return Some(("out".into(), KindSet::ANY));
        }
        pins.get(&id)
            .and_then(|p| p.outputs.first())
            .map(|p| (p.name.clone(), p.kinds))
    };
    let entries = |row: usize, filter: &str| -> Vec<doc::PaletteEntry> {
        let Some((_, kinds)) = ids.get(row).and_then(|id| first_output(*id)) else {
            return Vec::new();
        };
        doc::palette(registry, filter, Some(kinds))
            .into_iter()
            .filter(|e| e.pin.is_some())
            .collect()
    };
    let name = |i: usize| {
        ids.get(i)
            .map(|id| node_row_name(&d.graph, *id))
            .unwrap_or_default()
    };
    let kinds = |row: usize, filter: &str| entries(row, filter).into_iter().map(|e| e.ty).collect();
    let rows = GraphRows {
        len: ids.len(),
        name: &name,
        selected,
        kinds: &kinds,
        named_outputs: true,
        read_only: d.read_only(),
        input_pins: true,
        deletable: true,
        heading: "Nodes",
        noun: "node",
    };
    let actions = graph_keys_ui(ui, "procgen-nodes", &mut panel.keys, &rows);
    panel.keys_node = crate::graph_kit::canvas::anchor_of(&panel.keys, &ids);
    for action in actions {
        match action {
            GraphAction::Activate(row) => {
                let Some(&id) = ids.get(row) else { continue };
                // Framed once the canvas holds the node: the canvas is culled, and the selection
                // (made here) is always in it from the next frame.
                panel.frame_id = Some(id);
                edits.push(PanelEdit::Select(BTreeSet::from([id])));
            }
            GraphAction::Delete(row) => {
                if let Some(&id) = ids.get(row) {
                    edits.push(PanelEdit::Remove(BTreeSet::from([id])));
                }
            }
            GraphAction::Nudge(row, delta) => {
                let Some((&id, n)) = ids.get(row).and_then(|id| d.graph.nodes.get_key_value(id)) else {
                    continue;
                };
                edits.push(PanelEdit::Nudge(
                    id,
                    (n.at.0 + f64::from(delta[0]), n.at.1 + f64::from(delta[1])),
                ));
            }
            GraphAction::AddAfter(row, ty) => {
                let Some((&id, n)) = ids.get(row).and_then(|id| d.graph.nodes.get_key_value(id)) else {
                    continue;
                };
                let output = first_output(id).map(|(o, _)| o);
                let input = entries(row, "")
                    .into_iter()
                    .find(|e| e.ty == ty)
                    .and_then(|e| e.pin);
                if let (Some(output), Some(input)) = (output, input) {
                    edits.push(PanelEdit::AddConnected(
                        ty,
                        (n.at.0 + 220.0, n.at.1),
                        (id, output),
                        input,
                    ));
                }
            }
            GraphAction::Connect(row, form) => {
                let Some(&id) = ids.get(row) else { continue };
                let output = match form.output.trim() {
                    "" => first_output(id).map(|(o, _)| o).unwrap_or_default(),
                    named => named.to_string(),
                };
                let target = form.target.trim();
                let to = target
                    .rsplit_once('#')
                    .map_or(target, |(_, n)| n)
                    .parse::<u32>()
                    .ok()
                    .or_else(|| {
                        ids.iter()
                            .copied()
                            .find(|i| node_row_name(&d.graph, *i) == target)
                    });
                match to {
                    Some(to) => edits.push(PanelEdit::Connect(
                        (id, output),
                        (to, form.input.trim().to_string()),
                    )),
                    None => edits.push(PanelEdit::Toast(format!(
                        "no node `{target}`: type its id (12 or #12) or its row name"
                    ))),
                }
            }
        }
    }
}

fn palette_ui(
    ui: &mut egui::Ui,
    registry: &ProcNodeRegistry,
    panel: &mut ProcPanelUi,
    edits: &mut Vec<PanelEdit>,
) {
    ui.strong("Palette");
    let search = ui.text_edit_singleline(&mut panel.search);
    crate::a11y_audit::name(search, egui::WidgetType::TextEdit, "Palette filter");
    let searching = !panel.search.is_empty();
    let mut by_cat: BTreeMap<String, Vec<doc::PaletteEntry>> = BTreeMap::new();
    for e in doc::palette(registry, &panel.search, None) {
        by_cat.entry(e.category.clone()).or_default().push(e);
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (cat, items) in by_cat {
            egui::CollapsingHeader::new(cat)
                .default_open(searching)
                .show(ui, |ui| {
                    for e in items {
                        if ui.button(&e.ty).on_hover_text(&e.doc).clicked() {
                            edits.push(PanelEdit::Add(e.ty, (0.0, 0.0)));
                        }
                    }
                });
        }
    });
}

fn settings_ui(
    ui: &mut egui::Ui,
    world: &World,
    d: &ProcGraphDoc,
    registry: &ProcNodeRegistry,
    panel: &mut ProcPanelUi,
    edits: &mut Vec<PanelEdit>,
) {
    let Some(id) = d.selection.iter().next().copied() else {
        ui.weak("Select a node to edit its settings.");
        return;
    };
    let Some(n) = d.graph.nodes.get(&id) else {
        return;
    };
    ui.strong(format!("{}  #{id}", n.ty));
    let Some(kind) = registry.get(&n.ty) else {
        ui.weak("Editor aid or unknown node type.");
        return;
    };
    ui.weak(kind.doc());
    egui::ScrollArea::vertical().show(ui, |ui| {
        for desc in kind.settings() {
            let current = n
                .settings
                .get(desc.name)
                .cloned()
                .or_else(|| SettingValue::parse(desc.default).ok())
                .unwrap_or(SettingValue::Ident("None".into()));
            let key = (id, desc.name.to_string());
            ui.horizontal(|ui| {
                ui.label(desc.name).on_hover_text(desc.doc);
                if let Some(param) = current.as_bind() {
                    ui.weak(format!("= {param}"));
                    if ui.small_button("unbind").clicked() {
                        edits.push(PanelEdit::Unbind(id, desc.name.to_string()));
                    }
                    return;
                }
                let text = panel.drafts.entry(key.clone()).or_insert_with(|| current.write());
                if let Some(v) =
                    super::widgets::setting_ui(ui, world, id, desc, &current, &mut panel.widgets, text)
                {
                    edits.push(PanelEdit::Set(id, desc.name.to_string(), v));
                }
                let pin = format!("@{}", desc.name);
                let wired = d.graph.edges.iter().any(|e| e.to.0 == id && e.to.1 == pin);
                let shown = wired || d.shown_pins.contains(&(id, desc.name.to_string()));
                if ui
                    .selectable_label(shown, "pin")
                    .on_hover_text("Show as an override pin (a Table input wins over this value)")
                    .clicked()
                {
                    edits.push(PanelEdit::Pin(id, desc.name.to_string(), !shown));
                }
                if let Some(ty) = doc::promote_type(&d.graph, registry, id, desc.name) {
                    promote_menu(ui, d, id, desc.name, ty, panel, edits);
                }
            });
            if let Some(err) = panel.setting_errors.get(&key) {
                ui.colored_label(egui::Color32::from_rgb(230, 80, 70), err);
            }
        }
    });
}

/// "Promote to parameter" with the name prompt (spec 67 § 2.4): a name field that starts at the
/// suggested free name (red while it is invalid or taken), Promote, and the existing parameters of
/// the setting's type to pick instead.
fn promote_menu(
    ui: &mut egui::Ui,
    d: &ProcGraphDoc,
    id: u32,
    setting: &str,
    ty: regia_procgen::AttrType,
    panel: &mut ProcPanelUi,
    edits: &mut Vec<PanelEdit>,
) {
    ui.menu_button("bind…", |ui| {
        ui.label("Promote to a new parameter");
        let name = panel
            .promote_name
            .entry((id, setting.to_string()))
            .or_insert_with(|| doc::suggest_param_name(&d.graph, setting));
        let valid = regia_procgen::graph::valid_param_name(name) && d.graph.param(name).is_none();
        let color = if valid {
            ui.visuals().text_color()
        } else {
            egui::Color32::from_rgb(230, 80, 70)
        };
        let r = ui.add(egui::TextEdit::singleline(name).text_color(color));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if !valid {
            ui.colored_label(color, "not a new valid parameter name");
        }
        if (ui.add_enabled(valid, egui::Button::new("Promote")).clicked() || (enter && valid)) && valid {
            edits.push(PanelEdit::Promote {
                node: id,
                setting: setting.to_string(),
                name: name.clone(),
                existing: false,
            });
            ui.close();
        }
        let existing: Vec<&str> = d
            .graph
            .params
            .iter()
            .filter(|p| p.ty == ty)
            .map(|p| p.name.as_str())
            .collect();
        if !existing.is_empty() {
            ui.separator();
            ui.label("Bind to a parameter");
            for p in existing {
                if ui.button(p).clicked() {
                    edits.push(PanelEdit::Promote {
                        node: id,
                        setting: setting.to_string(),
                        name: p.to_string(),
                        existing: true,
                    });
                    ui.close();
                }
            }
        }
    });
}

fn report_ui(ui: &mut egui::Ui, world: &World, d: &ProcGraphDoc) {
    let Some(target) = world.get_resource::<ProcPreview>().and_then(|p| p.target) else {
        ui.weak("No preview target: pick a generator.");
        return;
    };
    let Some(st) = world.get::<regia_procgen::world::ProcGenState>(target) else {
        ui.weak("The preview target has not run yet.");
        return;
    };
    ui.horizontal(|ui| {
        ui.label(format!("state: {}", st.state.name()));
        if let Some(r) = &st.report {
            ui.label(format!("{:.2} ms, {} cache hit(s)", r.total_ms, r.cache_hits));
        }
        if let Some((node, code, msg)) = &st.last_error {
            ui.colored_label(
                egui::Color32::from_rgb(230, 80, 70),
                format!("node {node:?}: {code}: {msg}"),
            );
        }
    });
    let Some(r) = &st.report else {
        return;
    };
    let max = r.nodes.iter().map(|n| n.ms).fold(0.0_f64, f64::max).max(1e-6);
    egui::ScrollArea::vertical().show(ui, |ui| {
        for n in &r.nodes {
            ui.horizontal(|ui| {
                let label = format!("#{} {}", n.id, n.ty);
                if n.inactive || d.graph.nodes.get(&n.id).is_some_and(|x| !x.enabled) {
                    ui.weak(label);
                } else {
                    ui.label(label);
                }
                ui.add(
                    egui::ProgressBar::new((n.ms / max) as f32)
                        .desired_width(120.0)
                        .text(format!(
                            "{:.2} ms · {} el{}",
                            n.ms,
                            n.elements,
                            if n.cache_hit { " · cached" } else { "" }
                        )),
                );
            });
        }
    });
}

/// The table rows `[first, last)` to lay out for a scroll viewport spanning `top..bottom` (points
/// of the scrolled content): the visible rows plus one on each side; every row with `cull` off.
fn row_window(top: f32, bottom: f32, row_height: f32, len: usize, cull: bool) -> (usize, usize) {
    if !cull || row_height.is_nan() || row_height <= 0.0 {
        return (0, len);
    }
    let first = ((top / row_height).floor().max(0.0) as usize).saturating_sub(1);
    let last = ((bottom / row_height).ceil().max(0.0) as usize + 1).min(len);
    (first.min(len), last)
}

/// The data table's controls (spec 67 § 3): pin tabs, the item picker, the filter field (red with
/// the refusal), the pager. Each pushes a `TableControl`.
fn table_controls(
    ui: &mut egui::Ui,
    table: &inspect::DataTable,
    ins: &ProcInspect,
    panel: &mut ProcPanelUi,
    edits: &mut Vec<PanelEdit>,
) {
    use super::inspect::TableControl;
    ui.horizontal(|ui| {
        for pin in 0..table.pins {
            if ui
                .selectable_label(ins.pin == pin, format!("pin {pin}"))
                .clicked()
                && ins.pin != pin
            {
                edits.push(PanelEdit::Table(TableControl::Pin(pin)));
            }
        }
        if table.items > 1 {
            let mut item = ins.item;
            if ui
                .add(
                    egui::DragValue::new(&mut item)
                        .range(0..=table.items - 1)
                        .prefix("item "),
                )
                .changed()
            {
                edits.push(PanelEdit::Table(TableControl::Item(item)));
            }
        }
        ui.separator();
        let draft = panel
            .filter_draft
            .get_or_insert_with(|| ins.filter.clone().unwrap_or_default());
        let color = if panel.filter_error.is_empty() {
            ui.visuals().text_color()
        } else {
            egui::Color32::from_rgb(230, 80, 70)
        };
        let r = ui.add(
            egui::TextEdit::singleline(draft)
                .hint_text("filter: attr op value")
                .text_color(color)
                .desired_width(180.0),
        );
        if r.lost_focus() && Some(draft.as_str()) != ins.filter.as_deref().or(Some("")) {
            edits.push(PanelEdit::Table(TableControl::Filter(draft.clone())));
        } else if !r.has_focus() && panel.filter_error.is_empty() {
            // Follow a filter set elsewhere (the agent) while the field is idle.
            let current = ins.filter.as_deref().unwrap_or("");
            if draft != current {
                current.clone_into(draft);
            }
        }
        if !panel.filter_error.is_empty() {
            ui.colored_label(color, &panel.filter_error);
        }
        ui.separator();
        let pages = table.pages();
        let page = ins.page.min(pages - 1);
        if ui.add_enabled(page > 0, egui::Button::new("◀")).clicked() {
            edits.push(PanelEdit::Table(TableControl::Page(page - 1)));
        }
        let mut shown = page + 1;
        if ui
            .add(
                egui::DragValue::new(&mut shown)
                    .range(1..=pages)
                    .suffix(format!(" / {pages}")),
            )
            .changed()
        {
            edits.push(PanelEdit::Table(TableControl::Page(shown.saturating_sub(1))));
        }
        if ui.add_enabled(page + 1 < pages, egui::Button::new("▶")).clicked() {
            edits.push(PanelEdit::Table(TableControl::Page(page + 1)));
        }
    });
}

fn table_ui(ui: &mut egui::Ui, world: &World, panel: &mut ProcPanelUi, edits: &mut Vec<PanelEdit>) {
    let table = match inspect::build_table(world) {
        Ok(t) => t,
        Err(e) => {
            // No table this frame: report none, not the previous table's counts.
            panel.rows_drawn = 0;
            panel.rows_total = 0;
            ui.weak(e.message);
            return;
        }
    };
    let ins = world.get_resource::<ProcInspect>().cloned().unwrap_or_default();
    table_controls(ui, &table, &ins, panel, edits);
    // The cell picker (spec 67 § 3): a tiled generator's Inspect shows one cell's data; three
    // drag values over the run's cells `(tier, i, j)`. `editor.procgen_table { cell }` is the
    // agent path.
    let cells = inspect::inspect_cells(world);
    if !cells.is_empty() {
        let mut current = ins.cell.unwrap_or(cells[0]);
        if !cells.contains(&current) {
            current = cells[0];
        }
        let (mut tier, mut i, mut j) = current;
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("cell");
            changed |= ui.add(egui::DragValue::new(&mut tier)).changed();
            changed |= ui.add(egui::DragValue::new(&mut i)).changed();
            changed |= ui.add(egui::DragValue::new(&mut j)).changed();
        });
        if changed {
            edits.push(PanelEdit::Table(super::inspect::TableControl::Cell(tier, i, j)));
        }
    }
    ui.weak(format!(
        "{} · {} item(s) on pin {} · {} row(s) · page {}/{}",
        table.kind,
        table.items,
        ins.pin,
        table.order.len(),
        ins.page + 1,
        table.pages()
    ));
    let page = table.page(ins.page);
    panel.rows_total = page.len();
    // Match the pitch `egui::Grid` actually lays out: each row is at least `interact_size.y`
    // tall, plus `item_spacing.y`. The Grid draws the header row only when the window starts
    // at row 0; otherwise the spacer above the window stands in for it, so the content height
    // is always `header + len * row_height` and the scroll bar does not drift.
    let cull = !panel.no_cull;
    let row_height = ui
        .text_style_height(&egui::TextStyle::Body)
        .max(ui.spacing().interact_size.y)
        + ui.spacing().item_spacing.y;
    let mut drawn = 0;
    egui::ScrollArea::both().show_viewport(ui, |ui, viewport| {
        // Culling: only the rows in the scroll viewport (plus one row each side) are laid out; the
        // rest is one spacer, so the scroll bar still spans the page.
        let (first, last) = row_window(viewport.min.y, viewport.max.y, row_height, page.len(), cull);
        let header = if first == 0 { 0.0 } else { row_height };
        ui.add_space(first as f32 * row_height + header);
        egui::Grid::new("procgen-table").striped(true).show(ui, |ui| {
            if first == 0 {
                // A header click sorts: ascending, descending, off.
                for c in &table.columns {
                    let mark = match &ins.sort {
                        Some((s, false)) if s == c => " ▲",
                        Some((s, true)) if s == c => " ▼",
                        _ => "",
                    };
                    if ui
                        .button(egui::RichText::new(format!("{c}{mark}")).strong())
                        .clicked()
                    {
                        edits.push(PanelEdit::Table(super::inspect::TableControl::ToggleSort(
                            c.clone(),
                        )));
                    }
                }
                ui.end_row();
            }
            for (k, e) in page[first..last].iter().enumerate() {
                drawn += 1;
                let selected = ins.selected == Some(*e);
                for (c, cell) in table.rows[*e].iter().enumerate() {
                    if c == 0 {
                        // The first cell selects the row's element; hovering it highlights the
                        // element in the viewport (spec 67 § 3, view-only).
                        let row = ui.selectable_label(selected, cell.text());
                        if row.hovered() {
                            edits.push(PanelEdit::HoverRow(Some(*e)));
                        }
                        if row.clicked() {
                            edits.push(PanelEdit::Table(super::inspect::TableControl::SelectRow(
                                first + k,
                            )));
                        }
                    } else if selected {
                        ui.colored_label(egui::Color32::from_rgb(255, 210, 60), cell.text());
                    } else {
                        ui.label(cell.text());
                    }
                }
                ui.end_row();
            }
        });
        ui.add_space((page.len() - last) as f32 * row_height);
    });
    // The pointer left every row: clear the viewport hover (view-only).
    if !edits.iter().any(|e| matches!(e, PanelEdit::HoverRow(Some(_)))) {
        edits.push(PanelEdit::HoverRow(None));
    }
    panel.rows_drawn = drawn;
}

fn apply(world: &mut World, edits: Vec<PanelEdit>, pointer_down: bool) {
    for e in edits {
        let result = match e {
            PanelEdit::Add(ty, at) => {
                doc::edit(world, &format!("add {ty}"), None, |c| doc::add_node(c, &ty, at)).map(|_| ())
            }
            PanelEdit::AddConnected(ty, at, from, input) => {
                doc::edit(world, &format!("add {ty}"), None, |c| {
                    let id = doc::add_node(c, &ty, at)?;
                    doc::connect(c, (from.0, &from.1), (id, &input))
                })
            }
            PanelEdit::Remove(ids) => doc::edit(world, "remove nodes", None, |c| {
                doc::remove_nodes(c.graph, &ids);
                Ok(())
            }),
            PanelEdit::Connect(a, b) => doc::edit(world, "connect", None, |c| {
                doc::connect(c, (a.0, &a.1), (b.0, &b.1))
            }),
            PanelEdit::Disconnect(a, b) => doc::edit(world, "disconnect", None, |c| {
                doc::disconnect(c.graph, (a.0, &a.1), (b.0, &b.1));
                Ok(())
            }),
            PanelEdit::Move(id, at) => doc::edit(world, "move node", Some(format!("move:{id}")), |c| {
                doc::move_node(c.graph, id, at)
            }),
            PanelEdit::Nudge(id, at) => {
                doc::edit(world, "move node", None, |c| doc::move_node(c.graph, id, at))
            }
            PanelEdit::Set(id, name, v) => {
                let key = format!("set:{id}:{name}");
                let r = doc::edit(world, &format!("set {name}"), Some(key), |c| {
                    doc::set_setting(c, id, &name, v)
                });
                // An invalid value is shown in red under its widget and not written.
                if let Some(mut p) = world.get_resource_mut::<ProcPanelUi>() {
                    match &r {
                        Ok(()) => {
                            p.setting_errors.remove(&(id, name));
                        }
                        Err(e) => {
                            p.setting_errors
                                .insert((id, name), format!("{}: {}", e.code, e.message));
                        }
                    }
                }
                r
            }
            PanelEdit::Bypass(id, on) => {
                doc::edit(world, "bypass", None, |c| doc::set_enabled(c.graph, id, on))
            }
            PanelEdit::Promote {
                node,
                setting,
                name,
                existing,
            } => {
                let r = doc::promote_setting(world, node, &setting, Some(&name), existing).map(|_| ());
                if r.is_ok()
                    && let Some(mut p) = world.get_resource_mut::<ProcPanelUi>()
                {
                    p.promote_name.remove(&(node, setting));
                }
                r
            }
            PanelEdit::Unbind(id, name) => {
                doc::edit(world, "unbind", None, |c| doc::unbind_setting(c.graph, id, &name))
            }
            PanelEdit::Param(action) => super::params_tab::apply_param_action(world, action),
            PanelEdit::Table(control) => {
                let filter = match &control {
                    super::inspect::TableControl::Filter(text) => Some(text.clone()),
                    _ => None,
                };
                let r = super::inspect::apply_table_control(world, control);
                if let Some(text) = filter
                    && let Some(mut p) = world.get_resource_mut::<ProcPanelUi>()
                {
                    match &r {
                        Ok(()) => {
                            p.filter_error.clear();
                            p.filter_draft = Some(text);
                        }
                        Err(e) => p.filter_error = e.message.clone(),
                    }
                }
                // The filter field shows its own error; no toast.
                if matches!(&r, Err(e) if matches!(e.code.as_str(), "procgen_filter_invalid" | "procgen_unknown_column"))
                {
                    Ok(())
                } else {
                    r
                }
            }
            PanelEdit::Select(ids) => {
                if let Some(mut d) = world.get_resource_mut::<ProcGraphDoc>() {
                    d.selection = ids;
                }
                Ok(())
            }
            PanelEdit::Visualize(id) => {
                let on = world
                    .get_resource::<ProcPreview>()
                    .is_some_and(|p| p.visualize.contains_key(&id));
                let n = world
                    .get_resource::<ProcPreview>()
                    .map_or(0, |p| p.visualize.len());
                let style = (!on).then(|| VisualizeStyle {
                    color_by: ColorBy::Density,
                    form: VisForm::Points,
                    color: super::debug_view::categorical(n),
                });
                super::debug_view::visualize(world, id, style);
                Ok(())
            }
            PanelEdit::Inspect(id) => {
                *world.get_resource_or_insert_with(ProcInspect::default) = ProcInspect {
                    node: Some(id),
                    ..ProcInspect::default()
                };
                if let Some(mut p) = world.get_resource_mut::<ProcPanelUi>() {
                    p.bottom_table = true;
                }
                Ok(())
            }
            PanelEdit::Duplicate(ids) => doc::edit(world, "duplicate", None, |c| {
                let frag = doc::copy_fragment(c.graph, &ids);
                doc::paste_fragment(c.graph, &frag, (40.0, 40.0)).map(|_| ())
            }),
            PanelEdit::Save => doc::save(world).map(|_| ()),
            PanelEdit::Enter(id) => doc::enter_call(world, id).map(|_| ()),
            PanelEdit::Back => doc::back(world).map(|_| ()),
            PanelEdit::New(path, starter) => doc::new_graph(world, &path, starter.as_deref()),
            PanelEdit::SetStarter(on) => doc::set_starter(world, on),
            PanelEdit::Clip(frag) => {
                if let Some(mut d) = world.get_resource_mut::<ProcGraphDoc>() {
                    d.clipboard = Some(frag);
                }
                Ok(())
            }
            PanelEdit::Pin(id, name, show) => doc::show_pin(world, id, &name, show),
            PanelEdit::Paste(text) => doc::edit(world, "paste", None, |c| {
                doc::paste_fragment(c.graph, &text, (40.0, 40.0)).map(|_| ())
            }),
            PanelEdit::Resolve(theirs) => doc::resolve_conflict(world, theirs),
            PanelEdit::Collapse(nodes, path) => {
                apply_panel_action(world, PanelAction::Collapse { nodes, path })
            }
            PanelEdit::Expand { node, copy } => apply_panel_action(world, PanelAction::Expand { node, copy }),
            PanelEdit::Isolate(node) => apply_panel_action(world, PanelAction::Isolate(node)),
            PanelEdit::Seed { entity, action } => {
                apply_panel_action(world, PanelAction::Seed { entity, action })
            }
            PanelEdit::Tier(tier) => apply_panel_action(world, PanelAction::Tier(tier)),
            PanelEdit::CancelTool => {
                super::tools::disarm(world);
                Ok(())
            }
            PanelEdit::Action(action) => apply_panel_action(world, action),
            PanelEdit::ArmWith {
                kind,
                preset,
                biome,
                brush,
            } => {
                super::tools::arm_with(world, kind, &preset, biome, Some(brush));
                Ok(())
            }
            PanelEdit::HitFilter(kind, filter) => {
                super::tools::set_hit_filter(world, kind, filter);
                Ok(())
            }
            PanelEdit::PlantSeed(seed) => {
                super::plant_preview::set_seed(world, seed);
                Ok(())
            }
            PanelEdit::Toast(message) => Err(doc::DocError::new("procgen_no_node", message)),
            PanelEdit::HoverRow(element) => {
                super::inspect::set_hover(world, element);
                Ok(())
            }
        };
        if let Err(err) = result
            && let Some(mut d) = world.get_resource_mut::<ProcGraphDoc>()
        {
            d.toast = err.to_string();
        }
    }
    if !pointer_down && let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        // The pointer is up: a drag or slider scrub ended, the next one is its own undo entry.
        stack.seal();
    }
}

/// Captures the panel (canvas, table or report) drawn against `world`.
///
/// # Errors
///
/// GPU readback failures.
pub fn capture_panel(
    device: &RenderDevice,
    world: &mut World,
    view: CaptureView,
    size: [u32; 2],
) -> Result<egui::ColorImage, String> {
    super::install_if_missing(world);
    crate::anim::capture::render_ui_offscreen(device, size, 3, |ui| {
        let _ = draw(ui, world, view);
    })
}

#[cfg(test)]
mod cull_tests {
    use super::*;

    #[test]
    fn the_row_window_covers_the_viewport_and_one_row_each_side() {
        // 20-point rows, a 100-point viewport scrolled to 400: rows 20..25 visible.
        assert_eq!(row_window(400.0, 500.0, 20.0, 1000, true), (19, 26));
    }

    #[test]
    fn the_row_window_clamps_to_the_page() {
        assert_eq!(row_window(0.0, 100.0, 20.0, 1000, true), (0, 6));
        assert_eq!(row_window(0.0, 100.0, 20.0, 3, true), (0, 3));
        assert_eq!(row_window(9_000.0, 9_100.0, 20.0, 10, true), (10, 10));
    }

    #[test]
    fn culling_off_lays_out_every_row() {
        assert_eq!(row_window(400.0, 500.0, 20.0, 1000, false), (0, 1000));
        assert_eq!(
            row_window(400.0, 500.0, 0.0, 7, true),
            (0, 7),
            "no row height: all rows"
        );
    }

    #[test]
    fn a_failed_table_reports_no_rows() {
        // No ProcInspect resource: build_table fails. The panel shows the error text and the
        // agent method must report no rows, not the previous table's counts.
        let world = World::new();
        let mut panel = ProcPanelUi {
            rows_drawn: 42,
            rows_total: 1000,
            ..ProcPanelUi::default()
        };
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            table_ui(ui, &world, &mut panel, &mut Vec::new());
        });
        out.textures_delta.clear();
        assert_eq!(
            (panel.rows_drawn, panel.rows_total),
            (0, 0),
            "a failed table clears the previous counts"
        );
    }

    #[test]
    fn the_row_pitch_matches_a_real_grid() {
        // The culling spacers assume one pitch per row; lay out a real Grid and check the
        // formula (max(Body, interact_size) + item_spacing) predicts its height, header
        // included, so the scroll content does not drift from the drawn rows.
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let pitch = ui
                .text_style_height(&egui::TextStyle::Body)
                .max(ui.spacing().interact_size.y)
                + ui.spacing().item_spacing.y;
            let rows = 20;
            let grid = egui::Grid::new("pitch-probe").show(ui, |ui| {
                ui.strong("header");
                ui.end_row();
                for i in 0..rows {
                    ui.label(format!("row {i}"));
                    ui.end_row();
                }
            });
            // The rect covers every row's height and the spacing between rows; the spacing
            // after the last row is not part of it.
            let expected = pitch * (rows as f32 + 1.0) - ui.spacing().item_spacing.y;
            let actual = grid.response.rect.height();
            assert!(
                (actual - expected).abs() <= 1.0,
                "grid height {actual} ≈ header + {rows} rows at pitch {pitch} = {expected}"
            );
        });
        out.textures_delta.clear();
    }

    /// FU-7 (`docs/evidence/editor-a11y-2026-11.md`): a node error is drawn as a "⚠" whose
    /// meaning lived only in hover text; the spoken name now carries the error text.
    #[test]
    fn a_node_error_carries_its_text_as_a_spoken_name() {
        let mut world = World::new();
        let mut d = ProcGraphDoc {
            path: Some("fixture.pgraph.ron".to_string()),
            ..ProcGraphDoc::default()
        };
        let mut node = regia_procgen::graph::NodeEntry::new("reroute");
        // Well inside the canvas area that is left between the palette, settings and report
        // panels, so the node is drawn and not culled as off screen.
        node.at = (200.0, 100.0);
        d.graph.nodes.insert(1, node);
        d.errors.push(regia_procgen::graph::GraphError {
            node: Some(1),
            code: "fixture".to_string(),
            message: "the fixture warning".to_string(),
        });
        world.insert_resource(d);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| procgen_graph_panel(ui, &mut world));
        output.textures_delta.clear();
        let update = output.platform_output.accesskit_update.expect("accesskit is on");
        // The glyph is static text read in place (a label), never an operable button that does
        // nothing (ADR-8870, WCAG 4.1.2).
        let labels = crate::a11y_audit::label_names(&update);
        assert!(
            labels.iter().any(|name| name.contains("the fixture warning")),
            "the error text in the labels {labels:?}"
        );
        let names: Vec<String> = crate::a11y_audit::widget_names(&update)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert!(
            !names.iter().any(|name| name.contains("the fixture warning")),
            "the error glyph is not announced as an interactive widget: {names:?}"
        );
    }
}
