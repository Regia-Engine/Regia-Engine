// SPDX-License-Identifier: MPL-2.0
//! The Parameters panel (w12 P-169, ADR-7851): the graph's parameters, textures and static
//! switches as a typed list (name, type, default, the nodes that read it), in slot order, with
//! add, remove, a typed default editor and reordering (drag a row onto another, or its Up and
//! Down buttons from the keyboard). It replaces the raw RON blocks of the declarations; the
//! shading block keeps its RON editor. Instances list the parent's parameters in this order.
//!
//! The v5 fold (ADR-8105 (b), w13) adds each parameter's category, tooltip and range: the
//! parameters group under their categories (order of first appearance, ungrouped first), keep
//! the slot order inside a group (Up and Down move a parameter past its group's neighbour), clamp
//! every edit to the range (a slider for a ranged `Float`), and show the tooltip under the row and
//! on hover; a parameter's Details section edits the three. Instance tabs draw their overrides
//! with the same groups, ranges and tooltips ([`instance_params_ui`]). Every change is one undo
//! step through [`super::edit`]; the agent twin is `editor.matgraph_params`.

use regia_matgraph::{
    ColorSpace, MaterialGraph, MaterialInstance, Node, ParamDecl, ParamRange, ParamValue, SwitchDecl,
    TextureDecl,
};
use serde_json::{Value, json};

use super::model::valid_node_name;
use super::{DocMode, MatGraphEditor, PendingEdit};

/// Which declaration list a row belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeclKind {
    /// `params`: uniform parameters.
    Param,
    /// `textures`.
    Texture,
    /// `switches`: static switches.
    Switch,
}

impl DeclKind {
    /// Every kind, in panel order.
    pub const ALL: [Self; 3] = [Self::Param, Self::Texture, Self::Switch];

    /// The protocol name: `param`, `texture` or `switch`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Param => "param",
            Self::Texture => "texture",
            Self::Switch => "switch",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == text)
    }

    /// The panel heading.
    #[must_use]
    pub fn heading(self) -> &'static str {
        match self {
            Self::Param => "Parameters",
            Self::Texture => "Textures",
            Self::Switch => "Switches",
        }
    }
}

/// One row of the panel (and of `editor.matgraph_params { op: "list" }`).
#[derive(Clone, Debug, PartialEq)]
pub struct DeclRow {
    /// Its list.
    pub kind: DeclKind,
    /// Slot order within its list (0-based).
    pub index: usize,
    /// The declared name.
    pub name: String,
    /// `Float`, `Vec2`, `Vec3`, `Vec4`, `Color`, `Texture` or `Switch`.
    pub ty: &'static str,
    /// The default as JSON (`{ "Float": 0.5 }`, an image path, `true`).
    pub default: Value,
    /// Nodes of the graph that read it (`Param`, sampling and `Switch` nodes), sorted.
    pub used_by: Vec<String>,
    /// A parameter's category (v5 fold; empty: ungrouped, and always empty for textures and
    /// switches).
    pub category: String,
    /// A parameter's tooltip (v5 fold; empty: none).
    pub tooltip: String,
    /// A parameter's range (v5 fold; `None`: unbounded).
    pub range: Option<ParamRange>,
}

/// The type name of a parameter value.
#[must_use]
pub fn value_type_name(value: ParamValue) -> &'static str {
    match value {
        ParamValue::Float(_) => "Float",
        ParamValue::Vec2(_) => "Vec2",
        ParamValue::Vec3(_) => "Vec3",
        ParamValue::Vec4(_) => "Vec4",
        ParamValue::Color(_) => "Color",
    }
}

/// The zero value of a parameter type name (`Color` is mid gray).
#[must_use]
pub fn zero_value(ty: &str) -> Option<ParamValue> {
    Some(match ty {
        "Float" => ParamValue::Float(0.0),
        "Vec2" => ParamValue::Vec2([0.0; 2]),
        "Vec3" => ParamValue::Vec3([0.0; 3]),
        "Vec4" => ParamValue::Vec4([0.0; 4]),
        "Color" => ParamValue::Color([0.5; 3]),
        _ => return None,
    })
}

/// Whether `node` reads declaration `name` of `kind`.
#[must_use]
pub fn reads(node: &Node, kind: DeclKind, name: &str) -> bool {
    match (kind, node) {
        (DeclKind::Param, Node::Param(p)) => p == name,
        (DeclKind::Texture, Node::Sample { texture, .. })
        | (DeclKind::Texture, Node::Triplanar { texture, .. })
        | (DeclKind::Texture, Node::Parallax { texture, .. }) => texture == name,
        (DeclKind::Switch, Node::Switch { switch, .. }) => switch == name,
        _ => false,
    }
}

fn readers(graph: &MaterialGraph, kind: DeclKind, name: &str) -> Vec<String> {
    graph
        .nodes
        .iter()
        .filter(|(_, n)| reads(n, kind, name))
        .map(|(k, _)| k.clone())
        .collect()
}

/// The panel's rows: parameters, then textures, then switches, each in slot order.
#[must_use]
pub fn rows(graph: &MaterialGraph) -> Vec<DeclRow> {
    let mut out = Vec::new();
    for (index, p) in graph.params.iter().enumerate() {
        out.push(DeclRow {
            kind: DeclKind::Param,
            index,
            name: p.name.clone(),
            ty: value_type_name(p.value),
            default: serde_json::to_value(p.value).unwrap_or(Value::Null),
            used_by: readers(graph, DeclKind::Param, &p.name),
            category: p.category.clone(),
            tooltip: p.tooltip.clone(),
            range: p.range,
        });
    }
    for (index, t) in graph.textures.iter().enumerate() {
        out.push(DeclRow {
            kind: DeclKind::Texture,
            index,
            name: t.name.clone(),
            ty: "Texture",
            default: json!(t.path),
            used_by: readers(graph, DeclKind::Texture, &t.name),
            category: String::new(),
            tooltip: String::new(),
            range: None,
        });
    }
    for (index, s) in graph.switches.iter().enumerate() {
        out.push(DeclRow {
            kind: DeclKind::Switch,
            index,
            name: s.name.clone(),
            ty: "Switch",
            default: json!(s.default),
            used_by: readers(graph, DeclKind::Switch, &s.name),
            category: String::new(),
            tooltip: String::new(),
            range: None,
        });
    }
    out
}

/// A row as protocol JSON.
#[must_use]
pub fn row_json(row: &DeclRow) -> Value {
    json!({
        "kind": row.kind.name(),
        "index": row.index,
        "name": row.name,
        "type": row.ty,
        "default": row.default,
        "used_by": row.used_by,
        "category": row.category,
        "tooltip": row.tooltip,
        "range": row.range.map(|r| json!({ "min": r.min, "max": r.max, "step": r.step })),
    })
}

/// The parameter groups as protocol JSON: `[{ category, params: [names in slot order] }]`, in
/// the panel's order (ungrouped first, then categories by first appearance).
#[must_use]
pub fn groups_json(graph: &MaterialGraph) -> Value {
    Value::Array(
        graph
            .param_groups()
            .into_iter()
            .map(|(category, members)| {
                let names: Vec<&str> = members
                    .iter()
                    .filter_map(|&i| graph.params.get(i).map(|p| p.name.as_str()))
                    .collect();
                json!({ "category": category, "params": names })
            })
            .collect(),
    )
}

/// Longest category name (characters).
pub const MAX_CATEGORY_CHARS: usize = 64;
/// Longest tooltip (characters).
pub const MAX_TOOLTIP_CHARS: usize = 512;

/// A change to a parameter's metadata for [`set_meta`]: each `Some` field replaces that part.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParamMeta {
    /// The new category (trimmed; empty: ungrouped).
    pub category: Option<String>,
    /// The new tooltip (trimmed; empty: none).
    pub tooltip: Option<String>,
    /// The new range (`Some(None)` removes it).
    pub range: Option<Option<ParamRange>>,
}

/// Changes the metadata of parameter `name`; a new range clamps the default into it. Returns the
/// default after the change.
///
/// # Errors
///
/// No such parameter, a category or tooltip too long or with a control character, or an invalid
/// range (non-finite, `min >= max`, a step outside `0..=max - min`).
pub fn set_meta(graph: &mut MaterialGraph, name: &str, meta: ParamMeta) -> Result<ParamValue, String> {
    let at = position(graph, DeclKind::Param, name).ok_or_else(|| format!("no param `{name}`"))?;
    let check = |what: &str, text: &str, max: usize| -> Result<String, String> {
        let text = text.trim();
        if text.chars().count() > max {
            return Err(format!(
                "parameter `{name}`: the {what} is longer than {max} characters"
            ));
        }
        if text.chars().any(char::is_control) {
            return Err(format!("parameter `{name}`: the {what} has a control character"));
        }
        Ok(text.to_string())
    };
    let category = meta
        .category
        .as_deref()
        .map(|c| check("category", c, MAX_CATEGORY_CHARS))
        .transpose()?;
    let tooltip = meta
        .tooltip
        .as_deref()
        .map(|t| check("tooltip", t, MAX_TOOLTIP_CHARS))
        .transpose()?;
    if let Some(Some(range)) = meta.range
        && let Some(why) = range.error()
    {
        return Err(format!("parameter `{name}`: {why}"));
    }
    let decl = &mut graph.params[at];
    if let Some(category) = category {
        decl.category = category;
    }
    if let Some(tooltip) = tooltip {
        decl.tooltip = tooltip;
    }
    if let Some(range) = meta.range {
        decl.range = range;
        decl.value = decl.clamp(decl.value);
    }
    Ok(decl.value)
}

/// The slot a parameter's Up (`up`) or Down button moves it to: the slot of its group's
/// previous or next member, or `None` at the group's edge.
#[must_use]
pub fn group_neighbour_slot(graph: &MaterialGraph, index: usize, up: bool) -> Option<usize> {
    let category = &graph.params.get(index)?.category;
    let members: Vec<usize> = graph
        .params
        .iter()
        .enumerate()
        .filter(|(_, p)| p.category == *category)
        .map(|(i, _)| i)
        .collect();
    let at = members.iter().position(|&i| i == index)?;
    if up {
        at.checked_sub(1).map(|j| members[j])
    } else {
        members.get(at + 1).copied()
    }
}

/// A new default for [`set`].
#[derive(Clone, Debug, PartialEq)]
pub enum DeclValue {
    /// A parameter default (its variant is the type).
    Param(ParamValue),
    /// A texture's default image (project-relative).
    Texture(String),
    /// A switch default.
    Switch(bool),
}

impl DeclValue {
    /// The list it belongs to.
    #[must_use]
    pub fn kind(&self) -> DeclKind {
        match self {
            Self::Param(_) => DeclKind::Param,
            Self::Texture(_) => DeclKind::Texture,
            Self::Switch(_) => DeclKind::Switch,
        }
    }
}

fn position(graph: &MaterialGraph, kind: DeclKind, name: &str) -> Option<usize> {
    match kind {
        DeclKind::Param => graph.params.iter().position(|p| p.name == name),
        DeclKind::Texture => graph.textures.iter().position(|t| t.name == name),
        DeclKind::Switch => graph.switches.iter().position(|s| s.name == name),
    }
}

/// Sets the default of declaration `name` (a parameter may change type; a ranged one is clamped
/// to its range), or adds it at the end of its list. Returns whether it was added.
///
/// # Errors
///
/// A bad name, a non-finite value, or a texture path outside the project.
pub fn set(graph: &mut MaterialGraph, name: &str, value: DeclValue) -> Result<bool, String> {
    if !valid_node_name(name) {
        return Err(format!(
            "bad name `{name}`: 1-64 characters of letters, digits and `_`"
        ));
    }
    let kind = value.kind();
    let at = position(graph, kind, name);
    match value {
        DeclValue::Param(value) => {
            if !value.is_finite() {
                return Err(format!("parameter `{name}`: the default is not finite"));
            }
            match at {
                Some(i) => {
                    let clamped = graph.params[i].clamp(value);
                    graph.params[i].value = clamped;
                }
                None => graph.params.push(ParamDecl::new(name, value)),
            }
        }
        DeclValue::Texture(path) => {
            if !path.is_empty() {
                super::model::graph_file_path(None, None, &path)?;
            }
            match at {
                Some(i) => graph.textures[i].path = path,
                None => graph.textures.push(TextureDecl {
                    name: name.to_string(),
                    path,
                    color_space: ColorSpace::Srgb,
                }),
            }
        }
        DeclValue::Switch(on) => match at {
            Some(i) => graph.switches[i].default = on,
            None => graph.switches.push(SwitchDecl {
                name: name.to_string(),
                default: on,
            }),
        },
    }
    Ok(at.is_none())
}

/// Removes declaration `name` of `kind`; nodes that read it stay (the compiler reports them).
/// Returns those nodes.
///
/// # Errors
///
/// No such declaration.
pub fn remove(graph: &mut MaterialGraph, kind: DeclKind, name: &str) -> Result<Vec<String>, String> {
    let at = position(graph, kind, name).ok_or_else(|| format!("no {} `{name}`", kind.name()))?;
    match kind {
        DeclKind::Param => {
            graph.params.remove(at);
        }
        DeclKind::Texture => {
            graph.textures.remove(at);
        }
        DeclKind::Switch => {
            graph.switches.remove(at);
        }
    }
    Ok(readers(graph, kind, name))
}

/// Moves declaration `name` of `kind` to slot `to` (clamped to the list), keeping the others in
/// order. The slot order is the uniform layout, so a moved parameter changes the generated code
/// (instances follow by name).
///
/// # Errors
///
/// No such declaration.
pub fn reorder(graph: &mut MaterialGraph, kind: DeclKind, name: &str, to: usize) -> Result<usize, String> {
    let from = position(graph, kind, name).ok_or_else(|| format!("no {} `{name}`", kind.name()))?;
    fn shift<T>(list: &mut Vec<T>, from: usize, to: usize) -> usize {
        let to = to.min(list.len().saturating_sub(1));
        let item = list.remove(from);
        list.insert(to, item);
        to
    }
    Ok(match kind {
        DeclKind::Param => shift(&mut graph.params, from, to),
        DeclKind::Texture => shift(&mut graph.textures, from, to),
        DeclKind::Switch => shift(&mut graph.switches, from, to),
    })
}

/// The Stats strip's cache: the (code key, profile name) it was counted for, and the count.
pub type StatsCache = (
    (Option<u64>, &'static str),
    Result<super::cost::ProfileStats, String>,
);

/// The panel's w12 state (parameters form, tier preview, pinned node, stats, usages,
/// hierarchy): one field of `PanelState`, so the panel's own struct changes in one line.
#[derive(Default)]
pub struct ParityState {
    /// The render profile the preview draws with (`None`: the editor's own quality).
    pub profile: Option<regia_render::RenderProfilePreset>,
    /// The node pinned in the preview (its value drawn on the shape instead of the material).
    pub pinned: Option<String>,
    /// The Stats strip: (code key, profile name) it was counted for, and the count.
    pub stats: Option<StatsCache>,
    /// The last Find usages answer: (what was searched, the usages or the reason).
    pub usages: Option<(
        super::usages::UsageQuery,
        Result<Vec<super::usages::Usage>, String>,
    )>,
    /// The Hierarchy view (scanned on request).
    pub hierarchy: Option<Result<super::hierarchy::Hierarchy, String>>,
    /// The instance tab's reparent picker: the chosen graph (project-relative).
    pub reparent_pick: String,
    /// The Add row: name and type index of [`ADD_TYPES`].
    pub add_name: String,
    /// See [`Self::add_name`].
    pub add_type: usize,
}

impl ParityState {
    /// A hash of what the preview draws besides the graph (tier and pinned node).
    #[must_use]
    pub fn preview_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.profile
            .map(regia_render::RenderProfilePreset::name)
            .hash(&mut h);
        self.pinned.hash(&mut h);
        h.finish()
    }
}

/// The types the Add row offers.
pub const ADD_TYPES: [&str; 7] = ["Float", "Vec2", "Vec3", "Vec4", "Color", "Texture", "Switch"];

/// The default a new declaration of type `ty` starts with.
#[must_use]
pub fn new_value(ty: &str) -> Option<DeclValue> {
    match ty {
        "Texture" => Some(DeclValue::Texture(String::new())),
        "Switch" => Some(DeclValue::Switch(false)),
        other => zero_value(other).map(DeclValue::Param),
    }
}

fn set_edit(name: String, value: DeclValue, merge: Option<String>) -> PendingEdit {
    PendingEdit::new("declaration default", merge, move |d| {
        set(&mut d.graph, &name, value).map(|_| ())
    })
}

/// The Parameters panel (graph documents). An instance tab shows the parent's declarations in
/// the same order through its overrides list; a function has no declarations. Parameters group
/// under their categories (ungrouped first, then categories in order of first appearance).
pub fn params_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor, pending: &mut Vec<PendingEdit>) {
    if !matches!(editor.doc.mode, DocMode::Graph) {
        return;
    }
    let all = rows(&editor.doc.graph);
    for kind in DeclKind::ALL {
        let list: Vec<&DeclRow> = all.iter().filter(|r| r.kind == kind).collect();
        let last = list.len().saturating_sub(1);
        egui::CollapsingHeader::new(format!("{} ({})", kind.heading(), list.len()))
            .id_salt(("matgraph-params", kind.name()))
            .default_open(true)
            .show(ui, |ui| {
                if list.is_empty() {
                    ui.weak("None.");
                }
                if kind != DeclKind::Param {
                    for row in list {
                        let up = row.index.checked_sub(1);
                        let down = (row.index < last).then_some(row.index + 1);
                        decl_row_ui(ui, editor, row, up, down, pending);
                    }
                    return;
                }
                for (category, members) in editor.doc.graph.param_groups() {
                    let group: Vec<&DeclRow> = members.iter().filter_map(|&i| list.get(i).copied()).collect();
                    let mut body = |ui: &mut egui::Ui| {
                        for row in &group {
                            let up = group_neighbour_slot(&editor.doc.graph, row.index, true);
                            let down = group_neighbour_slot(&editor.doc.graph, row.index, false);
                            decl_row_ui(ui, editor, row, up, down, pending);
                        }
                    };
                    if category.is_empty() {
                        body(ui);
                    } else {
                        egui::CollapsingHeader::new(format!("{category} ({})", group.len()))
                            .id_salt(("matgraph-param-category", category.as_str()))
                            .default_open(true)
                            .show(ui, body);
                    }
                }
            });
    }
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(&mut editor.ui.parity.add_name)
                .hint_text("name")
                .desired_width(110.0),
        );
        crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "New declaration name");
        let index = editor.ui.parity.add_type.min(ADD_TYPES.len() - 1);
        let combo = egui::ComboBox::from_id_salt("matgraph-param-add-type")
            .selected_text(ADD_TYPES[index])
            .show_ui(ui, |ui| {
                for (i, ty) in ADD_TYPES.iter().enumerate() {
                    ui.selectable_value(&mut editor.ui.parity.add_type, i, *ty);
                }
            })
            .response;
        crate::a11y_audit::name(combo, egui::WidgetType::ComboBox, "New declaration type");
        let name = editor.ui.parity.add_name.trim().to_string();
        let add = crate::a11y_audit::name(
            ui.add_enabled(!name.is_empty(), egui::Button::new("Add")),
            egui::WidgetType::Button,
            "Add declaration",
        );
        if add.clicked()
            && let Some(value) = new_value(ADD_TYPES[index])
        {
            pending.push(PendingEdit::new("add declaration", None, move |d| {
                if position(&d.graph, value.kind(), &name).is_some() {
                    return Err(format!("`{name}` is already declared"));
                }
                set(&mut d.graph, &name, value).map(|_| ())
            }));
            editor.ui.parity.add_name.clear();
        }
    });
}

/// One declaration row: drag handle and name (the tooltip on hover), type, the typed default
/// editor, Up / Down (to `up` / `down`, the neighbour's slot) and Remove, then what reads it,
/// the tooltip, and a parameter's Details (category, tooltip, range).
fn decl_row_ui(
    ui: &mut egui::Ui,
    editor: &MatGraphEditor,
    row: &DeclRow,
    up: Option<usize>,
    down: Option<usize>,
    pending: &mut Vec<PendingEdit>,
) {
    let kind = row.kind;
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    let drag_id = egui::Id::new(("matgraph-param-row", kind.name(), &row.name));
    let frame = ui.dnd_drop_zone::<(DeclKind, String), _>(egui::Frame::new(), |ui| {
        ui.horizontal(|ui| {
            ui.dnd_drag_source(drag_id, (kind, row.name.clone()), |ui| {
                let label = ui.label(egui::RichText::new(&row.name).strong());
                if !row.tooltip.is_empty() {
                    let _ = label.on_hover_text(&row.tooltip);
                }
            });
            ui.weak(row.ty);
            row_value_ui(ui, editor, row, pending);
            let up_button = ui.add_enabled(up.is_some(), egui::Button::new("Up").small());
            let up_button = crate::a11y_audit::name(
                up_button,
                egui::WidgetType::Button,
                &format!("Move {} up", row.name),
            );
            if up_button.clicked()
                && let Some(to) = up
            {
                pending.push(reorder_edit(kind, &row.name, to));
            }
            let down_button = ui.add_enabled(down.is_some(), egui::Button::new("Down").small());
            let down_button = crate::a11y_audit::name(
                down_button,
                egui::WidgetType::Button,
                &format!("Move {} down", row.name),
            );
            if down_button.clicked()
                && let Some(to) = down
            {
                pending.push(reorder_edit(kind, &row.name, to));
            }
            let remove_button = crate::a11y_audit::name(
                ui.small_button("Remove"),
                egui::WidgetType::Button,
                &format!("Remove {}", row.name),
            );
            if remove_button.clicked() {
                let name = row.name.clone();
                pending.push(PendingEdit::new("remove declaration", None, move |d| {
                    remove(&mut d.graph, kind, &name).map(|_| ())
                }));
            }
        });
        if row.used_by.is_empty() {
            ui.colored_label(accents.log_warn, "unused");
        } else {
            ui.weak(format!("read by {}", row.used_by.join(", ")));
        }
        if !row.tooltip.is_empty() {
            ui.weak(&row.tooltip);
        }
        if kind == DeclKind::Param {
            meta_ui(ui, row, pending);
        }
    });
    if let Some(payload) = frame.1
        && payload.0 == kind
        && payload.1 != row.name
    {
        pending.push(reorder_edit(kind, &payload.1, row.index));
    }
}

/// The text a parameter's Details fields hold while the user types (committed on focus loss
/// or Enter), keyed by the parameter.
#[derive(Clone, Default)]
struct MetaDraft {
    category: String,
    tooltip: String,
    min: f32,
    max: f32,
    step: f32,
}

/// A parameter's Details section: Category and Tooltip fields, a Range checkbox and the Min,
/// Max and Step values. Each committed change is one undo step.
fn meta_ui(ui: &mut egui::Ui, row: &DeclRow, pending: &mut Vec<PendingEdit>) {
    egui::CollapsingHeader::new(format!("{} details", row.name))
        .id_salt(("matgraph-param-details", &row.name))
        .default_open(false)
        .show(ui, |ui| {
            let id = egui::Id::new(("matgraph-param-meta", &row.name));
            let current = MetaDraft {
                category: row.category.clone(),
                tooltip: row.tooltip.clone(),
                min: row.range.map_or(0.0, |r| r.min),
                max: row.range.map_or(1.0, |r| r.max),
                step: row.range.map_or(0.0, |r| r.step),
            };
            let mut draft: MetaDraft = ui.data(|d| d.get_temp(id)).unwrap_or_else(|| current.clone());
            let name = row.name.clone();
            let mut commit: Option<ParamMeta> = None;
            ui.horizontal(|ui| {
                let label = ui.label("Category");
                let field = ui
                    .add(egui::TextEdit::singleline(&mut draft.category).desired_width(140.0))
                    .labelled_by(label.id);
                let field = crate::a11y_audit::name(
                    field,
                    egui::WidgetType::TextEdit,
                    &format!("Category of {name}"),
                );
                if field.lost_focus() && draft.category.trim() != row.category {
                    commit = Some(ParamMeta {
                        category: Some(draft.category.clone()),
                        ..ParamMeta::default()
                    });
                }
            });
            ui.horizontal(|ui| {
                let label = ui.label("Tooltip");
                let field = ui
                    .add(egui::TextEdit::singleline(&mut draft.tooltip).desired_width(200.0))
                    .labelled_by(label.id);
                let field =
                    crate::a11y_audit::name(field, egui::WidgetType::TextEdit, &format!("Tooltip of {name}"));
                if field.lost_focus() && draft.tooltip.trim() != row.tooltip {
                    commit = Some(ParamMeta {
                        tooltip: Some(draft.tooltip.clone()),
                        ..ParamMeta::default()
                    });
                }
            });
            let mut ranged = row.range.is_some();
            let check = ui
                .horizontal(|ui| {
                    ui.label("Range");
                    crate::a11y_audit::bare_checkbox(ui, &mut ranged, &format!("Range of {name}"))
                })
                .inner;
            if check.changed() {
                commit = Some(ParamMeta {
                    range: Some(ranged.then_some(ParamRange {
                        min: draft.min,
                        max: draft.max.max(draft.min + 1.0),
                        step: 0.0,
                    })),
                    ..ParamMeta::default()
                });
            }
            if row.range.is_some() {
                ui.horizontal(|ui| {
                    let mut changed = false;
                    for (what, value) in [
                        ("Min", &mut draft.min),
                        ("Max", &mut draft.max),
                        ("Step", &mut draft.step),
                    ] {
                        let label = ui.label(what);
                        let field = ui
                            .add(egui::DragValue::new(value).speed(0.01))
                            .labelled_by(label.id);
                        let shown = f64::from(*value);
                        let field = crate::a11y_audit::named_drag_value(
                            field,
                            &format!("Range {} of {name}", what.to_lowercase()),
                            shown,
                        );
                        changed |= field.drag_stopped() || field.lost_focus();
                    }
                    let next = ParamRange {
                        min: draft.min,
                        max: draft.max,
                        step: draft.step,
                    };
                    if changed && Some(next) != row.range {
                        commit = Some(ParamMeta {
                            range: Some(Some(next)),
                            ..ParamMeta::default()
                        });
                    }
                });
            }
            if let Some(meta) = commit {
                ui.data_mut(|d| d.remove::<MetaDraft>(id));
                pending.push(meta_edit(&row.name, meta));
            } else if draft.category != current.category
                || draft.tooltip != current.tooltip
                || (draft.min, draft.max, draft.step) != (current.min, current.max, current.step)
            {
                ui.data_mut(|d| d.insert_temp(id, draft));
            } else {
                ui.data_mut(|d| d.remove::<MetaDraft>(id));
            }
        });
}

fn meta_edit(name: &str, meta: ParamMeta) -> PendingEdit {
    let name = name.to_string();
    PendingEdit::new("parameter details", None, move |d| {
        set_meta(&mut d.graph, &name, meta).map(|_| ())
    })
}

/// Edits one parameter value in place, clamped to `range` (each component a value box with the
/// range's bounds and step; a ranged `Float` is a slider). True when it changed. Without a range
/// it is the panel's plain editor.
pub fn ranged_value_ui(
    ui: &mut egui::Ui,
    value: &mut ParamValue,
    range: Option<ParamRange>,
    name: &str,
) -> bool {
    let Some(range) = range else {
        return super::panel::param_value_ui(ui, value);
    };
    let speed = if range.step > 0.0 {
        f64::from(range.step)
    } else {
        f64::from(range.max - range.min) / 200.0
    };
    let bounds = range.min..=range.max;
    let changed = ui
        .horizontal(|ui| {
            let component = |ui: &mut egui::Ui, x: &mut f32, label: String| -> bool {
                let response = ui.add(egui::DragValue::new(x).range(bounds.clone()).speed(speed));
                let shown = f64::from(*x);
                crate::a11y_audit::named_drag_value(response, &label, shown).changed()
            };
            match value {
                ParamValue::Float(x) => {
                    let mut slider = egui::Slider::new(x, bounds.clone());
                    if range.step > 0.0 {
                        slider = slider.step_by(f64::from(range.step));
                    }
                    let response = ui.add(slider);
                    crate::a11y_audit::name(response, egui::WidgetType::Slider, name).changed()
                }
                ParamValue::Vec2(v) => v.iter_mut().enumerate().fold(false, |c, (i, x)| {
                    component(ui, x, format!("{name} {}", ["x", "y"][i])) | c
                }),
                ParamValue::Vec3(v) => v.iter_mut().enumerate().fold(false, |c, (i, x)| {
                    component(ui, x, format!("{name} {}", ["x", "y", "z"][i])) | c
                }),
                ParamValue::Vec4(v) => v.iter_mut().enumerate().fold(false, |c, (i, x)| {
                    component(ui, x, format!("{name} {}", ["x", "y", "z", "w"][i])) | c
                }),
                ParamValue::Color(c) => ui.color_edit_button_rgb(c).changed(),
            }
        })
        .inner;
    if changed {
        *value = range.clamp_value(*value);
    }
    changed
}

/// An instance tab's parameter overrides, inheriting the parent's metadata: grouped by the
/// parent's categories, each override edited within the parent's range, the tooltip under the
/// row. Each change is one undo step ([`super::MatGraphDoc::set_param_override`] clamps too).
pub fn instance_params_ui(
    ui: &mut egui::Ui,
    graph: &MaterialGraph,
    instance: &MaterialInstance,
    pending: &mut Vec<PendingEdit>,
) {
    for (category, members) in graph.param_groups() {
        let mut body = |ui: &mut egui::Ui| {
            for decl in members.iter().filter_map(|&i| graph.params.get(i)) {
                instance_param_row(ui, decl, instance, pending);
            }
        };
        if category.is_empty() {
            body(ui);
        } else {
            egui::CollapsingHeader::new(format!("{category} ({})", members.len()))
                .id_salt(("matgraph-instance-category", category.as_str()))
                .default_open(true)
                .show(ui, body);
        }
    }
}

fn instance_param_row(
    ui: &mut egui::Ui,
    decl: &ParamDecl,
    instance: &MaterialInstance,
    pending: &mut Vec<PendingEdit>,
) {
    ui.horizontal(|ui| {
        let mut on = instance.params.contains_key(&decl.name);
        let check = ui.checkbox(&mut on, decl.name.as_str());
        let check = if decl.tooltip.is_empty() {
            check
        } else {
            check.on_hover_text(&decl.tooltip)
        };
        if check.changed() {
            let name = decl.name.clone();
            let value = on.then_some(decl.value);
            pending.push(PendingEdit::new("instance parameter", None, move |d| {
                d.set_param_override(&name, value)
            }));
        }
        match instance.params.get(&decl.name) {
            Some(value) => {
                let mut edited = decl.clamp(super::model::as_declared(decl.value, *value));
                if ranged_value_ui(ui, &mut edited, decl.range, &decl.name) && edited.is_finite() {
                    let name = decl.name.clone();
                    pending.push(PendingEdit::new(
                        "instance parameter",
                        Some(format!("override:{name}")),
                        move |d| d.set_param_override(&name, Some(edited)),
                    ));
                }
            }
            None => {
                ui.weak(format!("{:?}", decl.value));
            }
        }
    });
    if !decl.tooltip.is_empty() {
        ui.weak(&decl.tooltip);
    }
}

fn reorder_edit(kind: DeclKind, name: &str, to: usize) -> PendingEdit {
    let name = name.to_string();
    PendingEdit::new("reorder declaration", None, move |d| {
        reorder(&mut d.graph, kind, &name, to).map(|_| ())
    })
}

/// The typed default editor of one row.
fn row_value_ui(ui: &mut egui::Ui, editor: &MatGraphEditor, row: &DeclRow, pending: &mut Vec<PendingEdit>) {
    let graph = &editor.doc.graph;
    match row.kind {
        DeclKind::Param => {
            let Some(mut value) = graph.params.get(row.index).map(|p| p.value) else {
                return;
            };
            if ranged_value_ui(ui, &mut value, row.range, &row.name) && value.is_finite() {
                pending.push(set_edit(
                    row.name.clone(),
                    DeclValue::Param(value),
                    Some(format!("param_default:{}", row.name)),
                ));
            }
        }
        DeclKind::Texture => {
            let Some(mut path) = graph.textures.get(row.index).map(|t| t.path.clone()) else {
                return;
            };
            let before = path.clone();
            let response = ui.add(
                egui::TextEdit::singleline(&mut path)
                    .hint_text("assets/….png")
                    .desired_width(140.0),
            );
            let response = crate::a11y_audit::name(
                response,
                egui::WidgetType::TextEdit,
                &format!("Texture {}", row.name),
            );
            if response.lost_focus() && path != before {
                pending.push(set_edit(row.name.clone(), DeclValue::Texture(path), None));
            }
        }
        DeclKind::Switch => {
            let Some(mut on) = graph.switches.get(row.index).map(|s| s.default) else {
                return;
            };
            if ui.checkbox(&mut on, "on").changed() {
                pending.push(set_edit(row.name.clone(), DeclValue::Switch(on), None));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> MaterialGraph {
        MaterialGraph::from_ron(
            r#"(
                format_version: 4,
                params: [
                    (name: "tint", value: Color((0.8, 0.2, 0.1))),
                    (name: "rough", value: Float(0.5)),
                    (name: "spare", value: Vec2((1.0, 2.0))),
                ],
                textures: [(name: "albedo", path: "a.png", color_space: Srgb)],
                switches: [(name: "wet", default: false)],
                nodes: {
                    "tint": Param("tint"),
                    "r": Param("rough"),
                    "uv": TexCoord,
                    "s": Sample(texture: "albedo", uv: "uv"),
                    "rgb": Swizzle(input: "s", channels: "rgb"),
                    "dry": Mul("tint", "rgb"),
                    "base": Switch(switch: "wet", on: "tint", off: "dry"),
                },
                output: (base_color: Some("base"), roughness: Some("r")),
            )"#,
        )
        .expect("fixture parses")
    }

    #[test]
    fn rows_list_types_defaults_and_readers_in_slot_order() {
        let rows = rows(&graph());
        let names: Vec<_> = rows
            .iter()
            .map(|r| (r.kind.name(), r.name.as_str(), r.ty))
            .collect();
        assert_eq!(
            names,
            vec![
                ("param", "tint", "Color"),
                ("param", "rough", "Float"),
                ("param", "spare", "Vec2"),
                ("texture", "albedo", "Texture"),
                ("switch", "wet", "Switch"),
            ]
        );
        assert_eq!(rows[0].used_by, vec!["tint".to_string()]);
        assert!(rows[2].used_by.is_empty());
        assert_eq!(rows[3].used_by, vec!["s".to_string()]);
        assert_eq!(rows[4].used_by, vec!["base".to_string()]);
        assert_eq!(rows[1].default, json!({ "Float": 0.5 }));
    }

    #[test]
    fn set_adds_or_changes_and_reorder_moves_one_slot() {
        let mut g = graph();
        assert!(!set(&mut g, "rough", DeclValue::Param(ParamValue::Float(0.25))).expect("set"));
        assert_eq!(g.params[1].value, ParamValue::Float(0.25));
        assert!(set(&mut g, "metal", DeclValue::Param(ParamValue::Float(1.0))).expect("add"));
        assert_eq!(g.params.last().map(|p| p.name.as_str()), Some("metal"));
        assert!(set(&mut g, "bad name", DeclValue::Switch(true)).is_err());
        assert!(set(&mut g, "x", DeclValue::Param(ParamValue::Float(f32::NAN))).is_err());
        assert!(set(&mut g, "t", DeclValue::Texture("../outside.png".into())).is_err());
        assert_eq!(reorder(&mut g, DeclKind::Param, "metal", 0).expect("move"), 0);
        let order: Vec<_> = g.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(order, ["metal", "tint", "rough", "spare"]);
        assert_eq!(reorder(&mut g, DeclKind::Param, "metal", 99).expect("clamped"), 3);
        assert!(reorder(&mut g, DeclKind::Switch, "nope", 0).is_err());
        let left = remove(&mut g, DeclKind::Texture, "albedo").expect("remove");
        assert_eq!(
            left,
            vec!["s".to_string()],
            "the reader stays for the compiler to report"
        );
        assert!(g.textures.is_empty());
    }

    #[test]
    fn a_reorder_keeps_instances_reading_by_name() {
        let mut g = graph();
        let mut instance = regia_matgraph::MaterialInstance::new("p.matgraph.ron");
        instance.set("rough", ParamValue::Float(0.9));
        let before = instance.param_block(&g, 0.0).expect("block");
        reorder(&mut g, DeclKind::Param, "rough", 0).expect("move");
        let after = instance.param_block(&g, 0.0).expect("block");
        // Slot 0 is the clock; `rough` moved from slot 2 to slot 1 and keeps the override.
        assert_eq!(before[2][0], 0.9);
        assert_eq!(after[1][0], 0.9);
        assert!(super::super::model::diagnose(&g, None, None).ok);
    }
}
