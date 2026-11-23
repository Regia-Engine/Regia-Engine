// SPDX-License-Identifier: MPL-2.0
//! The UI designer's model (ADR-1811, ADR-2489, `docs/specs/81-ui-designer.md` § 4): one open
//! `.ui.ron` document with node paths, edits decoded by the format's own serde types, snapshot undo,
//! lint and an atomic save that keeps the source file's comments and key order. Headless: the egui panel ([`crate::ui_designer_panel`]) and the agent
//! methods ([`crate::ui_designer_agent`]) drive the same [`UiDesigner`] resource, so a person and
//! an agent see one document.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_ui::authoring::{
    AnimationBindDef, BindDef, BindTargetDef, RepeatDef, RepeatRowHeight, TextDef, TextSpanDef, UiNodeDef,
    WidgetDef,
};
use regia_ui::ron_merge::merge_ui_ron;
use regia_ui::viewmodel::ViewModelField;
use regia_ui::{
    BindConverter, BindMode, UI_RON_VERSION, UiFile, UiMigrators, ViewModels, parse_ui_ron, upgrade_ui_ron,
    write_ui_ron,
};
use serde_json::Value;

use crate::shared_panel_history::{SharedDoc, SharedHistory, StepKind};
use crate::ui_designer_preview::PreviewSettings;

pub(crate) mod autosave;

/// Undo snapshots kept per document.
pub const UNDO_DEPTH: usize = 200;

/// Node presets the designer adds (`editor.ui_designer_edit {op: "add", preset}`).
pub const PRESETS: [&str; 19] = [
    "panel",
    "text",
    "button",
    "toggle",
    "slider",
    "progress",
    "text_input",
    "dropdown",
    "image",
    "list",
    "scroll",
    "lobby_list",
    "scoreboard",
    "radar",
    "minimap",
    "marker_layer",
    // The v11 containers (ADR-7827).
    "split",
    "foldable",
    "tabs",
];

/// A designer failure: a stable code (`ui_designer_<code>` on the agent surface) and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignerError {
    /// `no_document`, `bad_node`, `bad_op`, `bad_value`, `bad_move`, `nothing_to_undo`,
    /// `nothing_to_redo`, `parse`, `io`, `bad_size`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl DesignerError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DesignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// A node address: child indices from the root (`[]` is the root).
pub type NodePath = Vec<usize>;

/// `"/"` for the root, `"/0/2"` for child 2 of child 0.
pub fn path_string(path: &[usize]) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    path.iter().map(|i| format!("/{i}")).collect()
}

/// The node at `path`.
pub fn node_at<'a>(root: &'a UiNodeDef, path: &[usize]) -> Option<&'a UiNodeDef> {
    path.iter().try_fold(root, |node, &i| node.children.get(i))
}

fn node_at_mut<'a>(root: &'a mut UiNodeDef, path: &[usize]) -> Option<&'a mut UiNodeDef> {
    path.iter().try_fold(root, |node, &i| node.children.get_mut(i))
}

/// Resolves `"/"`, `"/0/2"` (or `"0/2"`) or `"#name"` (first match, depth first) to a path of an
/// existing node.
pub fn resolve_node(root: &UiNodeDef, text: &str) -> Result<NodePath, DesignerError> {
    let text = text.trim();
    if let Some(name) = text.strip_prefix('#') {
        let mut found = None;
        walk(root, &mut Vec::new(), &mut |node, path| {
            if found.is_none() && node.name.as_deref() == Some(name) {
                found = Some(path.to_vec());
            }
        });
        return found.ok_or_else(|| DesignerError::new("bad_node", format!("no node named {name:?}")));
    }
    let mut path = Vec::new();
    for part in text.split('/').filter(|p| !p.is_empty()) {
        let i: usize = part.parse().map_err(|_| {
            DesignerError::new(
                "bad_node",
                format!("{text:?} is not a node path (\"/0/2\" or \"#name\")"),
            )
        })?;
        path.push(i);
    }
    if node_at(root, &path).is_none() {
        return Err(DesignerError::new(
            "bad_node",
            format!("no node at {}", path_string(&path)),
        ));
    }
    Ok(path)
}

/// Visits every node depth first (parent before children) with its path.
pub fn walk(node: &UiNodeDef, path: &mut Vec<usize>, f: &mut dyn FnMut(&UiNodeDef, &[usize])) {
    f(node, path);
    for (i, child) in node.children.iter().enumerate() {
        path.push(i);
        walk(child, path, f);
        path.pop();
    }
}

/// A new node from a preset name ([`PRESETS`]).
pub fn preset(kind: &str) -> Option<UiNodeDef> {
    let text = |s: &str| {
        Some(TextDef {
            spans: vec![TextSpanDef::Lit(s.to_owned())],
            align: Default::default(),
            wrap: Default::default(),
            overflow: Default::default(),
            line_height: 1.2,
            style: Default::default(),
            markup: false,
        })
    };
    let points = |w: f32, h: f32| [regia_ui::Dimension::Points(w), regia_ui::Dimension::Points(h)];
    let mut node = UiNodeDef::default();
    match kind {
        "panel" => {}
        "text" => node.text = text("Text"),
        "button" => {
            node.widget = Some(WidgetDef::Button);
            node.interactable = true;
            node.text = text("Button");
        }
        "toggle" => node.widget = Some(WidgetDef::Toggle(false)),
        "slider" => node.widget = Some(WidgetDef::Slider(0.5, 0.0, 1.0, 0.0)),
        "progress" => node.widget = Some(WidgetDef::Progress(0.5)),
        "text_input" => node.widget = Some(WidgetDef::TextInput(String::new(), false, Default::default())),
        "dropdown" => node.widget = Some(WidgetDef::Dropdown(vec!["One".into(), "Two".into()], Some(0))),
        "image" => {
            node.image = Some(regia_ui::authoring::ImageDef::raw(0));
        }
        "list" => node.widget = Some(WidgetDef::List(0, 32.0)),
        "scroll" => node.widget = Some(WidgetDef::Scroll(0.3, true)),
        "lobby_list" => {
            node.widget = Some(WidgetDef::LobbyList(Default::default()));
            node.style.size = points(480.0, 360.0);
        }
        "scoreboard" => {
            node.widget = Some(WidgetDef::Scoreboard(regia_ui::authoring::ScoreboardDef {
                title: "Scores".to_owned(),
                columns: vec![regia_ui::authoring::ScoreColumnDef {
                    key: "score".to_owned(),
                    title: "Score".to_owned(),
                    width: 0.0,
                }],
                ..Default::default()
            }));
        }
        "radar" => {
            node.widget = Some(WidgetDef::Radar(Default::default()));
            node.style.size = points(200.0, 200.0);
        }
        "minimap" => {
            node.widget = Some(WidgetDef::Radar(regia_ui::authoring::RadarDef {
                world_rect: Some([-50.0, -50.0, 50.0, 50.0]),
                ..Default::default()
            }));
            node.style.size = points(240.0, 240.0);
        }
        "split" => {
            node.widget = Some(WidgetDef::Split(Default::default()));
            node.style.size = points(480.0, 240.0);
            node.children = vec![UiNodeDef::default(), UiNodeDef::default()];
        }
        "foldable" => {
            node.widget = Some(WidgetDef::Foldable(regia_ui::authoring::FoldableDef {
                title: "Section".to_owned(),
                open: true,
                group: None,
            }));
            node.children = vec![UiNodeDef {
                text: text("Content"),
                ..Default::default()
            }];
        }
        "tabs" => {
            node.widget = Some(WidgetDef::Tabs(regia_ui::authoring::TabsDef {
                tabs: vec!["One".to_owned(), "Two".to_owned()],
                active: 0,
            }));
            node.style.size = points(480.0, 240.0);
            node.children = ["One", "Two"]
                .into_iter()
                .map(|t| UiNodeDef {
                    text: text(t),
                    ..Default::default()
                })
                .collect();
        }
        "marker_layer" => {
            node.widget = Some(WidgetDef::MarkerLayer(Default::default()));
            node.style.position = regia_ui::PositionType::Absolute;
            node.style.size = [
                regia_ui::Dimension::Percent(100.0),
                regia_ui::Dimension::Percent(100.0),
            ];
        }
        _ => return None,
    }
    Some(node)
}

/// The short kind name of a node for trees and replies (`button`, `text`, `panel`, …).
pub fn node_kind(node: &UiNodeDef) -> &'static str {
    if node.use_component.is_some() {
        return "instance";
    }
    match &node.widget {
        Some(WidgetDef::Button) => "button",
        Some(WidgetDef::Toggle(_)) => "toggle",
        Some(WidgetDef::Slider(..)) => "slider",
        Some(WidgetDef::Progress(_)) => "progress",
        Some(WidgetDef::TextInput(..)) => "text_input",
        Some(WidgetDef::Dropdown(..)) => "dropdown",
        Some(WidgetDef::TabBar(..)) => "tab_bar",
        Some(WidgetDef::Modal(..)) => "modal",
        Some(WidgetDef::Tooltip(..)) => "tooltip",
        Some(WidgetDef::Scroll(..)) => "scroll",
        Some(WidgetDef::List(..)) => "list",
        Some(WidgetDef::Focusable) => "focusable",
        Some(WidgetDef::Stick(..)) => "stick",
        Some(WidgetDef::VButton(_)) => "v_button",
        Some(WidgetDef::PopupMenu(_)) => "popup_menu",
        Some(WidgetDef::ContextMenu(_)) => "context_menu",
        Some(WidgetDef::MenuBar(_)) => "menu_bar",
        Some(WidgetDef::LobbyList(_)) => "lobby_list",
        Some(WidgetDef::Scoreboard(_)) => "scoreboard",
        Some(WidgetDef::Radar(d)) if d.world_rect.is_some() => "minimap",
        Some(WidgetDef::Radar(_)) => "radar",
        Some(WidgetDef::MarkerLayer(_)) => "marker_layer",
        Some(WidgetDef::Split(_)) => "split",
        Some(WidgetDef::Foldable(_)) => "foldable",
        Some(WidgetDef::Tabs(_)) => "tabs",
        None if node.text.is_some() => "text",
        None if node.image.is_some() => "image",
        None => "panel",
    }
}

/// A tree row's label: the kind, `#name` when named, and `-> <use>` for a component instance.
pub fn node_label(node: &UiNodeDef) -> String {
    let mut label = node_kind(node).to_owned();
    if let Some(name) = &node.name {
        label.push_str(&format!(" #{name}"));
    }
    if let Some(path) = &node.use_component {
        label.push_str(&format!(" -> {path}"));
    }
    label
}

/// `node` as JSON (the sparse form the writer uses: default fields are absent).
pub fn node_json(node: &UiNodeDef) -> Value {
    serde_json::to_value(node).unwrap_or(Value::Null)
}

/// `node` with the field at `key` (dotted, e.g. `style.size`) set to `value`, decoded by the
/// format's own types; `null` resets the field to its default. `children` is not a field here
/// (use add / remove / move). `text` also takes a plain string (one literal span; the node's
/// alignment and style stay).
pub fn set_field(node: &UiNodeDef, key: &str, value: Value) -> Result<UiNodeDef, DesignerError> {
    let key = key.trim();
    if key.is_empty() || key.split('.').any(str::is_empty) {
        return Err(DesignerError::new(
            "bad_value",
            "`key` is a dotted field path such as `style.size`",
        ));
    }
    if key == "children" || key.starts_with("children.") {
        return Err(DesignerError::new(
            "bad_value",
            "children change through add, remove and move",
        ));
    }
    // `widget.<field>` names one typed field of the node's widget (`widget.max` of a slider,
    // `widget.options` of a dropdown, ADR-9730); other `widget.*` keys are plain JSON paths.
    if let Some(field) = key.strip_prefix("widget.")
        && let Some(widget) = &node.widget
        && crate::ui_designer_widget::has_field(widget, field)
    {
        let widget = crate::ui_designer_widget::set_widget_field(widget, field, value)?;
        let mut out = node.clone();
        out.widget = Some(widget);
        return Ok(out);
    }
    let (key, value) = match (key, value) {
        ("text", Value::String(s)) if node.text.is_some() => ("text.spans", serde_json::json!([s])),
        ("text", Value::String(s)) => ("text", serde_json::json!({ "spans": [s] })),
        (_, v) => (key, v),
    };
    let mut json = node_json(node);
    let mut slot = &mut json;
    let parts: Vec<&str> = key.split('.').collect();
    for (i, part) in parts.iter().enumerate() {
        let Value::Object(map) = slot else {
            return Err(DesignerError::new(
                "bad_value",
                format!("{key}: {part} is not inside a struct"),
            ));
        };
        if i + 1 == parts.len() {
            if value.is_null() {
                map.remove(*part);
            } else {
                map.insert((*part).to_owned(), value);
            }
            break;
        }
        slot = map
            .entry((*part).to_owned())
            .or_insert_with(|| Value::Object(Default::default()));
    }
    serde_json::from_value::<UiNodeDef>(json)
        .map_err(|e| DesignerError::new("bad_value", format!("{key}: {e}")))
}

/// One structural or field edit.
#[derive(Debug, Clone, PartialEq)]
pub enum DesignOp {
    /// Insert `node` under `parent` at `index` (`None` appends).
    Add {
        /// Parent node.
        parent: NodePath,
        /// Position among the parent's children.
        index: Option<usize>,
        /// The new node (boxed: a node definition is large).
        node: Box<UiNodeDef>,
    },
    /// Remove a node (not the root).
    Remove(NodePath),
    /// Move a node under `parent` at `index` (`None` appends; the index counts after removal).
    Move {
        /// The node.
        node: NodePath,
        /// New parent (not inside the node).
        parent: NodePath,
        /// Position among the new parent's children.
        index: Option<usize>,
    },
    /// Copy a node right after itself.
    Duplicate(NodePath),
    /// Set one field (see [`set_field`]).
    Set {
        /// The node.
        node: NodePath,
        /// Dotted field path.
        key: String,
        /// JSON value; `null` resets.
        value: Value,
    },
}

/// Where a dragged node lands relative to the row it is dropped on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    /// A sibling before the row.
    Before,
    /// The row's last child.
    Into,
    /// A sibling after the row.
    After,
}

impl DropZone {
    /// The zone for a pointer at `fraction` (0 top, 1 bottom) of a row's height.
    pub fn at(fraction: f32) -> Self {
        if fraction < 0.25 {
            Self::Before
        } else if fraction > 0.75 {
            Self::After
        } else {
            Self::Into
        }
    }
}

/// The [`DesignOp::Move`] that drops `node` on the row `onto`; `None` when the drop changes
/// nothing or is not possible (the root, a node's own subtree).
pub fn drop_move(node: &[usize], onto: &[usize], zone: DropZone) -> Option<DesignOp> {
    let (&from, old_parent) = node.split_last()?;
    if onto.starts_with(node) {
        return None;
    }
    let sibling = onto.split_last().filter(|_| zone != DropZone::Into);
    let Some((&at, parent)) = sibling else {
        // Dropped into a row, or beside the root (which has no siblings).
        return (onto != old_parent).then(|| DesignOp::Move {
            node: node.to_vec(),
            parent: onto.to_vec(),
            index: None,
        });
    };
    let mut index = if zone == DropZone::After { at + 1 } else { at };
    // `Move` counts the index after the removal.
    if parent == old_parent && from < index {
        index -= 1;
    }
    (parent != old_parent || index != from).then(|| DesignOp::Move {
        node: node.to_vec(),
        parent: parent.to_vec(),
        index: Some(index),
    })
}

/// The [`DesignOp::Add`] that drops a new node from the palette on the row or preview node `onto`:
/// `Into` makes it the last child, `Before` and `After` a sibling (beside the root means into it).
pub fn drop_add(node: UiNodeDef, onto: &[usize], zone: DropZone) -> DesignOp {
    match onto.split_last().filter(|_| zone != DropZone::Into) {
        Some((&at, parent)) => DesignOp::Add {
            parent: parent.to_vec(),
            index: Some(if zone == DropZone::After { at + 1 } else { at }),
            node: Box::new(node),
        },
        None => DesignOp::Add {
            parent: onto.to_vec(),
            index: None,
            node: Box::new(node),
        },
    }
}

/// The edit that makes `node` repeat over `source`, keyed by `key` (an empty source removes the
/// repeat; its children stay as plain children).
pub fn repeat_op(node: NodePath, source: Option<&str>, key: Option<&str>) -> DesignOp {
    repeat_op_with(node, source, key, RepeatRowHeight::List)
}

/// As [`repeat_op`], with the row height of a virtualized repeat (`list` or `template`, ADR-7751).
pub fn repeat_op_with(
    node: NodePath,
    source: Option<&str>,
    key: Option<&str>,
    row_height: RepeatRowHeight,
) -> DesignOp {
    let value = match source.map(str::trim).filter(|s| !s.is_empty()) {
        Some(source) => serde_json::to_value(RepeatDef {
            source: source.to_owned(),
            key: key.map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned),
            row_height,
        })
        .unwrap_or(Value::Null),
        None => Value::Null,
    };
    DesignOp::Set {
        node,
        key: "repeat".to_owned(),
        value,
    }
}

/// The edit that sets or clears the converter of the node's binding `index`, counted as
/// `ViewModels::status` does: the single `bind` first, then `bindings`.
pub fn converter_op(
    root: &UiNodeDef,
    node: &[usize],
    index: usize,
    converter: Option<BindConverter>,
) -> Result<DesignOp, DesignerError> {
    edit_binding_op(root, node, index, |bind| bind.converter = converter)
}

/// Where a binding reads from, as the `bind_source` edit sets it (ADR-7751).
#[derive(Debug, Clone, PartialEq)]
pub enum BindSourceEdit {
    /// A model or item field (`"Model.field"`, `".field"`): clears `theme` and `animation`.
    Field(String),
    /// A theme token (`"color.accent"`): clears `source` and `animation`.
    Theme(String),
    /// An animation: clears `source` and `theme`.
    Animation(AnimationBindDef),
}

/// The edit that sets the source of the node's binding `index` (counted as [`converter_op`]
/// counts) to a field, a theme token or an animation (ADR-7751).
pub fn bind_source_op(
    root: &UiNodeDef,
    node: &[usize],
    index: usize,
    edit: BindSourceEdit,
) -> Result<DesignOp, DesignerError> {
    edit_binding_op(root, node, index, |bind| match edit {
        BindSourceEdit::Field(source) => {
            bind.source = source;
            bind.theme = None;
            bind.animation = None;
        }
        BindSourceEdit::Theme(token) => {
            bind.source.clear();
            bind.theme = Some(token);
            bind.animation = None;
        }
        BindSourceEdit::Animation(animation) => {
            bind.source.clear();
            bind.theme = None;
            bind.animation = Some(animation);
        }
    })
}

/// The edit that changes the node's binding `index` (the single `bind` first, then `bindings`)
/// with `change`.
fn edit_binding_op(
    root: &UiNodeDef,
    node: &[usize],
    index: usize,
    change: impl FnOnce(&mut BindDef),
) -> Result<DesignOp, DesignerError> {
    let target = node_at(root, node)
        .ok_or_else(|| DesignerError::new("bad_node", format!("no node at {}", path_string(node))))?;
    let (key, value) = match (&target.bind, index) {
        (Some(single), 0) => {
            let mut bind = single.clone();
            change(&mut bind);
            ("bind", serde_json::to_value(bind))
        }
        (single, _) => {
            let slot = index - usize::from(single.is_some());
            let mut bindings = target.bindings.clone();
            let bind = bindings.get_mut(slot).ok_or_else(|| {
                DesignerError::new(
                    "bad_value",
                    format!(
                        "{} has no binding {index} ({} in all)",
                        path_string(node),
                        target.bindings.len() + usize::from(single.is_some())
                    ),
                )
            })?;
            change(bind);
            ("bindings", serde_json::to_value(bindings))
        }
    };
    let value = value.map_err(|e| DesignerError::new("bad_value", e.to_string()))?;
    Ok(DesignOp::Set {
        node: node.to_vec(),
        key: key.to_owned(),
        value,
    })
}

/// The repeat sources enclosing the children of the node at `path`, outermost first, the node's own
/// included.
pub fn enclosing_repeats<'a>(root: &'a UiNodeDef, path: &[usize]) -> Vec<&'a RepeatDef> {
    let mut out = Vec::new();
    let mut node = root;
    out.extend(node.repeat.as_ref());
    for &i in path {
        let Some(next) = node.children.get(i) else {
            break;
        };
        node = next;
        out.extend(node.repeat.as_ref());
    }
    out
}

/// The sample key of a binding `source` on the node at `path`: `".name"` under
/// `repeat: (source: "Vm.items")` is `"Vm.items[].name"`, shared by every sample row; other sources
/// are their own key.
pub fn sample_key(root: &UiNodeDef, path: &[usize], source: &str) -> String {
    if !source.starts_with('.') {
        return source.to_owned();
    }
    // The node's own repeat scopes its children, not its bindings.
    let scope = path.split_last().map_or(&[][..], |(_, parent)| parent);
    let mut prefix = item_prefix(&enclosing_repeats(root, scope));
    prefix.push_str(if source == "." { "" } else { source });
    prefix
}

/// `"Vm.items[]"`, `"Vm.sections[].items[]"`: the sample path of an item under these repeats.
pub fn item_prefix(repeats: &[&RepeatDef]) -> String {
    let mut out = String::new();
    for repeat in repeats {
        let source = repeat.source.as_str();
        // An absolute source starts over; a relative one continues from the item.
        if !source.starts_with('.') {
            out.clear();
        }
        out.push_str(if source == "." { "" } else { source });
        out.push_str("[]");
    }
    out
}

/// One lint finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFinding {
    /// Stable code (`docs/specs/81-ui-designer.md` § 5).
    pub code: &'static str,
    /// `error` or `warning`.
    pub severity: &'static str,
    /// Node path.
    pub node: String,
    /// What is wrong.
    pub message: String,
}

/// The next [`UiDesign::serial`].
static NEXT_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The open document.
#[derive(Debug, Clone)]
pub struct UiDesign {
    /// Where it was opened from / saves to.
    pub path: Option<PathBuf>,
    file: UiFile,
    /// A number no other document opened in this process has ([`Self::serial`]).
    serial: u64,
    revision: u64,
    saved_revision: u64,
    /// The selected node (the primary one of a multi-selection).
    pub selection: Option<NodePath>,
    /// Every selected node when more than one is (ADR-9730): the primary [`Self::selection`] is
    /// one of them. Read it through [`Self::selected_nodes`], which ignores a stale set.
    pub multi: Vec<NodePath>,
    undo: Vec<UiFile>,
    redo: Vec<UiFile>,
    /// The edits on their way to the editor's shared history (P-322).
    shared_history: SharedHistory<UiFile>,
    /// What the running merged edit changes ([`Self::apply_merged`], [`Self::apply_batch_merged`]).
    merging: Option<String>,
    /// The source text at the last open or save (upgraded to the current version) and its tree: a
    /// save merges into it, so comments and key order survive (ADR-2489).
    source: Option<(String, UiFile)>,
    /// The source text has comments.
    pub had_comments: bool,
    /// The last save wrote the canonical form instead of merging into the source (no source yet, the
    /// merge failed, or the caller asked for it); `None` before the first save.
    pub last_save_canonical: Option<bool>,
    /// The last save dropped the comments the source had.
    pub comments_lost: bool,
    /// The version the file declared before it was migrated in memory.
    pub opened_version: u32,
    /// Preview settings.
    pub preview: PreviewSettings,
}

impl UiDesign {
    fn new(
        file: UiFile,
        path: Option<PathBuf>,
        source: Option<String>,
        had_comments: bool,
        opened_version: u32,
    ) -> Self {
        let path_assets_root = path.as_deref().and_then(assets_root_of);
        Self {
            path,
            source: source.map(|text| (text, file.clone())),
            last_save_canonical: None,
            comments_lost: false,
            file,
            serial: NEXT_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            revision: 0,
            saved_revision: 0,
            selection: Some(Vec::new()),
            multi: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            shared_history: SharedHistory::default(),
            merging: None,
            had_comments,
            opened_version,
            preview: PreviewSettings {
                assets_root: path_assets_root,
                ..PreviewSettings::default()
            },
        }
    }

    /// The document.
    pub fn file(&self) -> &UiFile {
        &self.file
    }

    /// Bumped by every change (edits, undo, redo).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// A number unique to this opened document in the process: with [`Self::revision`] it names
    /// one state of one document (a reopened or new document starts again at revision 0).
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// Changed since it was opened or saved.
    pub fn dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// Undo steps available.
    pub fn undo_len(&self) -> usize {
        if self.shared_history.is_shared() {
            return self.shared_history.undo_depth();
        }
        self.undo.len()
    }

    /// Redo steps available.
    pub fn redo_len(&self) -> usize {
        if self.shared_history.is_shared() {
            return self.shared_history.redo_depth();
        }
        self.redo.len()
    }

    /// Applies `op` as one undo step; on error nothing changes. Returns the path of the node the
    /// edit produced or touched (selected afterwards).
    pub fn apply(&mut self, op: DesignOp) -> Result<NodePath, DesignerError> {
        self.merging = None;
        self.apply_step(op, true, false)
    }

    /// Like [`Self::apply`], but consecutive [`DesignOp::Set`]s of one field share an undo step
    /// until [`Self::end_merge`] (a drag on a number field is one edit, however many frames it
    /// runs).
    pub fn apply_merged(&mut self, op: DesignOp) -> Result<NodePath, DesignerError> {
        let key = match &op {
            DesignOp::Set { node, key, .. } => Some(format!("{}|{key}", path_string(node))),
            _ => None,
        };
        let new_step = key.is_none() || key != self.merging;
        let touched = self.apply_step(op, new_step, true)?;
        self.merging = key;
        Ok(touched)
    }

    /// Applies `ops` in order as one undo step (ADR-9730): every op sees the document the ops
    /// before it made, and if any fails nothing changes. The touched nodes become the selection
    /// (the first is the primary). An empty list changes nothing.
    pub fn apply_batch(&mut self, ops: Vec<DesignOp>) -> Result<Vec<NodePath>, DesignerError> {
        self.merging = None;
        self.batch_step(ops, true, false)
    }

    /// [`Self::apply_batch`], sharing one undo step with the previous call of the same `token`
    /// until [`Self::end_merge`] (a drag on a field of several selected nodes is one edit).
    pub fn apply_batch_merged(
        &mut self,
        ops: Vec<DesignOp>,
        token: &str,
    ) -> Result<Vec<NodePath>, DesignerError> {
        let token = format!("batch|{token}");
        let new_step = self.merging.as_deref() != Some(token.as_str());
        let touched = self.batch_step(ops, new_step, true)?;
        self.merging = Some(token);
        Ok(touched)
    }

    fn batch_step(
        &mut self,
        ops: Vec<DesignOp>,
        new_step: bool,
        merged: bool,
    ) -> Result<Vec<NodePath>, DesignerError> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let mut next = self.file.clone();
        let mut touched = Vec::with_capacity(ops.len());
        // While a layout variant is selected, field edits write its overrides only (ADR-7858).
        let variant = crate::ui_designer_variants::active(&self.file, &self.preview);
        for op in ops {
            touched.push(match variant {
                Some(v) => crate::ui_designer_variants::apply_in_variant(&mut next, v, op)?,
                None => apply_op(&mut next.root, op)?,
            });
        }
        self.commit_file(next, new_step, merged);
        touched.dedup();
        self.set_selection(touched.clone());
        Ok(touched)
    }

    /// Replaces the whole document as one undo step and selects `selection` (group, duplicate of
    /// several nodes, paste; ADR-9730).
    pub(crate) fn replace_selecting(&mut self, file: UiFile, selection: Vec<NodePath>) {
        self.merging = None;
        self.commit_file(file, true, false);
        self.set_selection(selection);
    }

    fn set_selection(&mut self, mut nodes: Vec<NodePath>) {
        let mut seen = std::collections::HashSet::new();
        nodes.retain(|p| seen.insert(p.clone()));
        self.selection = nodes.first().cloned();
        self.multi = if nodes.len() > 1 { nodes } else { Vec::new() };
    }

    /// Every selected node, the primary first: the multi-selection when it holds the primary,
    /// else the primary alone (nothing when no node is selected).
    pub fn selected_nodes(&self) -> Vec<NodePath> {
        match &self.selection {
            Some(primary) if self.multi.len() > 1 && self.multi.contains(primary) => {
                let mut out = vec![primary.clone()];
                out.extend(self.multi.iter().filter(|p| *p != primary).cloned());
                out
            }
            Some(primary) => vec![primary.clone()],
            None => Vec::new(),
        }
    }

    /// Selects several nodes, the first the primary (no undo step). Every path must exist.
    pub fn select_many(&mut self, nodes: Vec<NodePath>) -> Result<(), DesignerError> {
        if let Some(missing) = nodes.iter().find(|p| node_at(&self.file.root, p).is_none()) {
            return Err(DesignerError::new(
                "bad_node",
                format!("no node at {}", path_string(missing)),
            ));
        }
        self.set_selection(nodes);
        Ok(())
    }

    /// Adds `node` to the selection, or takes it out when it is already selected (Ctrl+click).
    /// The primary stays unless it is the one taken out.
    pub fn toggle_selected(&mut self, node: NodePath) -> Result<(), DesignerError> {
        let mut nodes = self.selected_nodes();
        if let Some(at) = nodes.iter().position(|p| *p == node) {
            nodes.remove(at);
        } else {
            nodes.push(node);
        }
        self.select_many(nodes)
    }

    /// Ends a merged edit: the next [`Self::apply_merged`] starts its own undo step.
    pub fn end_merge(&mut self) {
        self.merging = None;
    }

    fn apply_step(&mut self, op: DesignOp, new_step: bool, merged: bool) -> Result<NodePath, DesignerError> {
        let mut next = self.file.clone();
        // While a layout variant is selected, field edits write its overrides only (ADR-7858).
        let touched = match crate::ui_designer_variants::active(&self.file, &self.preview) {
            Some(v) => crate::ui_designer_variants::apply_in_variant(&mut next, v, op)?,
            None => apply_op(&mut next.root, op)?,
        };
        self.commit_file(next, new_step, merged);
        self.set_selection(vec![touched.clone()]);
        Ok(touched)
    }

    /// Replaces the whole document as one undo step (make-component, spec 111 § 6); `touched` is
    /// selected afterwards.
    pub(crate) fn replace_as_step(&mut self, file: UiFile, touched: NodePath) {
        self.merging = None;
        self.commit_file(file, true, false);
        self.set_selection(vec![touched]);
    }

    /// Makes `next` the document: a new undo step when `new_step` (a drag's first frame when
    /// `merged`), else part of the running merged step; recorded for the shared history.
    fn commit_file(&mut self, next: UiFile, new_step: bool, merged: bool) {
        let kind = match (new_step, merged) {
            (false, _) => StepKind::Continue,
            (true, true) => StepKind::Start,
            (true, false) => StepKind::Discrete,
        };
        let path = self.history_path();
        self.shared_history
            .record(self.serial, &path, &self.file, &next, kind, "document");
        if new_step && !self.shared_history.is_shared() {
            self.push_undo();
        }
        self.file = next;
        self.redo.clear();
        self.revision += 1;
    }

    /// What the shared history names the document by.
    fn history_path(&self) -> String {
        self.path.as_ref().map_or_else(
            || "untitled .ui.ron".to_owned(),
            |path| path.display().to_string().replace('\\', "/"),
        )
    }

    fn push_undo(&mut self) {
        self.undo.push(self.file.clone());
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
    }

    /// Restores the state before the last edit.
    pub fn undo(&mut self) -> Result<(), DesignerError> {
        if self.shared_history.is_shared() {
            self.merging = None;
            return if self.shared_history.navigate(false) {
                Ok(())
            } else {
                Err(DesignerError::new("nothing_to_undo", "the undo history is empty"))
            };
        }
        let prev = self
            .undo
            .pop()
            .ok_or_else(|| DesignerError::new("nothing_to_undo", "the undo history is empty"))?;
        self.redo.push(std::mem::replace(&mut self.file, prev));
        self.after_history();
        Ok(())
    }

    /// Re-applies the last undone edit.
    pub fn redo(&mut self) -> Result<(), DesignerError> {
        if self.shared_history.is_shared() {
            self.merging = None;
            return if self.shared_history.navigate(true) {
                Ok(())
            } else {
                Err(DesignerError::new("nothing_to_redo", "nothing was undone"))
            };
        }
        let next = self
            .redo
            .pop()
            .ok_or_else(|| DesignerError::new("nothing_to_redo", "nothing was undone"))?;
        self.undo.push(std::mem::replace(&mut self.file, next));
        self.after_history();
        Ok(())
    }

    fn after_history(&mut self) {
        self.merging = None;
        self.revision += 1;
        // An undone `add` can take the selected layout variant away (ADR-7858).
        if self
            .preview
            .variant
            .is_some_and(|v| v >= self.file.variants.len())
        {
            self.preview.variant = None;
        }
        if let Some(sel) = &self.selection
            && node_at(&self.file.root, sel).is_none()
        {
            self.selection = Some(Vec::new());
        }
        let root = &self.file.root;
        self.multi.retain(|p| node_at(root, p).is_some());
    }

    /// Selects a node (or nothing).
    pub fn select(&mut self, node: Option<NodePath>) -> Result<(), DesignerError> {
        if let Some(path) = &node
            && node_at(&self.file.root, path).is_none()
        {
            return Err(DesignerError::new(
                "bad_node",
                format!("no node at {}", path_string(path)),
            ));
        }
        self.selection = node;
        self.multi.clear();
        Ok(())
    }

    /// The canonical text of the document ([`write_ui_ron`]).
    pub fn text(&self) -> Result<String, DesignerError> {
        write_ui_ron(&self.file).map_err(|e| DesignerError::new("parse", e.to_string()))
    }

    /// The text a save writes: the source with only the changed fields rewritten, comments and key
    /// order kept. Falls back on the canonical form when the document has no source or the merge
    /// cannot produce this exact tree; the second value is `true` then.
    pub fn save_text(&self) -> Result<(String, bool), DesignerError> {
        if let Some((source, tree)) = &self.source {
            match merge_ui_ron(source, tree, &self.file) {
                Ok(text) => return Ok((text, false)),
                Err(e) => {
                    tracing::warn!(target: "regia_editor", "ui designer save: {e}; writing the canonical form");
                }
            }
        }
        Ok((self.text()?, true))
    }

    /// Writes the document to `path` (or its own path) through a temp file and a rename, merged into
    /// its source ([`Self::save_text`]); the text is parsed back first, so a file the loader would
    /// refuse is never written.
    pub fn save(&mut self, path: Option<PathBuf>) -> Result<PathBuf, DesignerError> {
        self.save_with(path, false)
    }

    /// [`Self::save`], or with `canonical` the canonical form (comments of the source are dropped).
    pub fn save_with(&mut self, path: Option<PathBuf>, canonical: bool) -> Result<PathBuf, DesignerError> {
        let path = path
            .or_else(|| self.path.clone())
            .ok_or_else(|| DesignerError::new("io", "the document has no path yet: pass one"))?;
        let (text, was_canonical) = if canonical {
            (self.text()?, true)
        } else {
            self.save_text()?
        };
        let back = parse_ui_ron(&text, &path.display().to_string(), &UiMigrators::default())
            .map_err(|e| DesignerError::new("parse", format!("written text does not load: {e}")))?;
        if back != self.file {
            return Err(DesignerError::new(
                "parse",
                "written text loads as a different tree",
            ));
        }
        write_atomic(&path, &text)?;
        self.path = Some(path.clone());
        self.saved_revision = self.revision;
        self.comments_lost = was_canonical && self.had_comments;
        self.had_comments = has_comments(&text);
        self.opened_version = UI_RON_VERSION;
        self.last_save_canonical = Some(was_canonical);
        self.source = Some((text, self.file.clone()));
        Ok(path)
    }
}

pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), DesignerError> {
    let io = |e: std::io::Error| DesignerError::new("io", format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("ui.ron");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}

/// Applies one op to a scratch copy of the root; returns the touched node's path.
fn apply_op(root: &mut UiNodeDef, op: DesignOp) -> Result<NodePath, DesignerError> {
    let bad_node = |p: &[usize]| DesignerError::new("bad_node", format!("no node at {}", path_string(p)));
    match op {
        DesignOp::Add { parent, index, node } => {
            let p = node_at_mut(root, &parent).ok_or_else(|| bad_node(&parent))?;
            let at = index.unwrap_or(p.children.len());
            if at > p.children.len() {
                return Err(DesignerError::new(
                    "bad_value",
                    format!(
                        "index {at} is past the {} children of {}",
                        p.children.len(),
                        path_string(&parent)
                    ),
                ));
            }
            p.children.insert(at, *node);
            let mut out = parent;
            out.push(at);
            Ok(out)
        }
        DesignOp::Remove(path) => {
            let Some((&last, parent)) = path.split_last() else {
                return Err(DesignerError::new("bad_node", "the root cannot be removed"));
            };
            let p = node_at_mut(root, parent).ok_or_else(|| bad_node(&path))?;
            if last >= p.children.len() {
                return Err(bad_node(&path));
            }
            p.children.remove(last);
            Ok(parent.to_vec())
        }
        DesignOp::Duplicate(path) => {
            let Some((&last, parent)) = path.split_last() else {
                return Err(DesignerError::new("bad_node", "the root cannot be duplicated"));
            };
            let p = node_at_mut(root, parent).ok_or_else(|| bad_node(&path))?;
            let copy = p.children.get(last).cloned().ok_or_else(|| bad_node(&path))?;
            p.children.insert(last + 1, copy);
            let mut out = parent.to_vec();
            out.push(last + 1);
            Ok(out)
        }
        DesignOp::Move { node, parent, index } => {
            let Some((&last, old_parent)) = node.split_last() else {
                return Err(DesignerError::new("bad_move", "the root cannot move"));
            };
            if parent.starts_with(&node) {
                return Err(DesignerError::new(
                    "bad_move",
                    format!("{} cannot move into its own subtree", path_string(&node)),
                ));
            }
            if node_at(root, &parent).is_none() {
                return Err(bad_node(&parent));
            }
            let moved = {
                let p = node_at_mut(root, old_parent).ok_or_else(|| bad_node(&node))?;
                if last >= p.children.len() {
                    return Err(bad_node(&node));
                }
                p.children.remove(last)
            };
            // The new parent's path shifts when it sat after the node under the same parent.
            let mut parent = parent;
            if parent.len() > old_parent.len()
                && parent.starts_with(old_parent)
                && parent[old_parent.len()] > last
            {
                parent[old_parent.len()] -= 1;
            }
            let p = node_at_mut(root, &parent).ok_or_else(|| bad_node(&parent))?;
            let at = index.unwrap_or(p.children.len());
            if at > p.children.len() {
                return Err(DesignerError::new(
                    "bad_move",
                    format!(
                        "index {at} is past the {} children of {}",
                        p.children.len(),
                        path_string(&parent)
                    ),
                ));
            }
            p.children.insert(at, moved);
            let mut out = parent;
            out.push(at);
            Ok(out)
        }
        DesignOp::Set { node, key, value } => {
            let target = node_at_mut(root, &node).ok_or_else(|| bad_node(&node))?;
            let mut updated = set_field(target, &key, value)?;
            updated.children = std::mem::take(&mut target.children);
            *target = updated;
            Ok(node)
        }
    }
}

/// The UI designer's documents on the editor's shared history (P-322,
/// `crate::shared_panel_history`): the open one and those an entered component returns to.
pub(crate) struct DesignHistory;

impl SharedDoc for DesignHistory {
    type State = UiFile;
    const PANEL: &'static str = "UI designer";

    fn histories(world: &mut World) -> Vec<&mut SharedHistory<UiFile>> {
        let Some(designer) = world.get_resource_mut::<UiDesigner>() else {
            return Vec::new();
        };
        let designer = designer.into_inner();
        designer
            .doc
            .iter_mut()
            .chain(designer.parents.iter_mut())
            .map(|doc| &mut doc.shared_history)
            .collect()
    }

    fn restore(world: &mut World, doc: u64, path: &str, state: &UiFile) {
        let found = world.get_resource_mut::<UiDesigner>().and_then(|designer| {
            let designer = designer.into_inner();
            designer
                .doc
                .iter_mut()
                .chain(designer.parents.iter_mut())
                .find(|open| open.serial == doc)
        });
        let Some(open) = found else {
            tracing::warn!(target: "regia_editor", path, "UI designer history step for a document that is no longer open");
            return;
        };
        open.file = state.clone();
        open.after_history();
    }

    fn open_doc(world: &World) -> Option<(u64, String)> {
        world
            .get_resource::<UiDesigner>()
            .and_then(UiDesigner::doc)
            .map(|doc| (doc.serial, doc.history_path()))
    }
}

/// The designer: at most one open document, plus the documents an entered component returns to.
#[derive(Resource, Debug, Default)]
pub struct UiDesigner {
    doc: Option<UiDesign>,
    /// The last error the panel shows.
    pub error: Option<String>,
    /// The copied nodes as the format's node JSON (a JSON array, ADR-9730); kept across documents,
    /// so a subtree copied in one document pastes into the next.
    pub clipboard: Option<String>,
    /// Documents waiting for a return from an entered component (spec 111 § 6), outermost first.
    parents: Vec<UiDesign>,
    /// The preview size, insets and theme the editor remembers (`.regia/editor_view.ron`,
    /// [`crate::view_state`]); a document opened later starts with them.
    pub preview_view: Option<crate::ui_designer_preview::PreviewView>,
}

impl UiDesigner {
    /// The open document.
    pub fn doc(&self) -> Option<&UiDesign> {
        self.doc.as_ref()
    }

    /// The open document, mutably.
    pub fn doc_mut(&mut self) -> Result<&mut UiDesign, DesignerError> {
        self.doc
            .as_mut()
            .ok_or_else(|| DesignerError::new("no_document", "no document is open"))
    }

    /// Starts an empty document (a full-size column root).
    pub fn new_document(&mut self) {
        let mut root = UiNodeDef::default();
        root.style.size = [
            regia_ui::Dimension::Percent(100.0),
            regia_ui::Dimension::Percent(100.0),
        ];
        root.style.direction = regia_ui::style::FlexDirection::Column;
        self.doc = Some(UiDesign::new(
            UiFile {
                version: UI_RON_VERSION,
                component: None,
                root,
                variants: Vec::new(),
                back: None,
            },
            None,
            None,
            false,
            UI_RON_VERSION,
        ));
        self.error = None;
        self.parents.clear();
        self.apply_preview_view();
    }

    /// Gives the open document the remembered preview view ([`Self::preview_view`]).
    pub fn apply_preview_view(&mut self) {
        if let (Some(view), Some(doc)) = (self.preview_view.clone(), self.doc.as_mut()) {
            doc.preview.apply_view(&view);
        }
    }

    /// Opens `.ui.ron` text (older versions are migrated in memory; the file is not touched).
    pub fn open_text(&mut self, text: &str, path: Option<PathBuf>) -> Result<(), DesignerError> {
        let shown = path
            .as_ref()
            .map_or_else(|| "<text>".to_owned(), |p| p.display().to_string());
        let file = parse_ui_ron(text, &shown, &UiMigrators::default())
            .map_err(|e| DesignerError::new("parse", e.to_string()))?;
        let opened_version = declared_version(text).unwrap_or(UI_RON_VERSION);
        // Renumbered only, so a save of an older file changes its version line and what was edited.
        let source = upgrade_ui_ron(text, &shown).ok();
        self.doc = Some(UiDesign::new(
            file,
            path,
            source,
            has_comments(text),
            opened_version,
        ));
        self.error = None;
        self.parents.clear();
        self.apply_preview_view();
        Ok(())
    }

    /// Opens a `.ui.ron` file.
    pub fn open(&mut self, path: &Path) -> Result<(), DesignerError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| DesignerError::new("io", format!("{}: {e}", path.display())))?;
        self.open_text(&text, Some(path.to_path_buf()))
    }

    /// Opens `path` as an entered component: the open document waits until [`Self::leave`].
    pub fn enter(&mut self, path: &Path) -> Result<(), DesignerError> {
        let parent = self
            .doc
            .take()
            .ok_or_else(|| DesignerError::new("no_document", "no document is open"))?;
        let mut stack = std::mem::take(&mut self.parents);
        let opened = self.open(path);
        if opened.is_ok() {
            stack.push(parent);
        } else {
            self.doc = Some(parent);
        }
        self.parents = stack;
        opened
    }

    /// Returns from an entered component to the document that entered it (the component must be
    /// saved first). The returned document's preview re-reads its components.
    pub fn leave(&mut self) -> Result<(), DesignerError> {
        if self.parents.is_empty() {
            return Err(DesignerError::new("not_entered", "no component was entered"));
        }
        if self.doc.as_ref().is_some_and(UiDesign::dirty) {
            return Err(DesignerError::new(
                "dirty",
                "the component has unsaved edits: save it (or undo them) before returning",
            ));
        }
        self.doc = self.parents.pop();
        if let Some(doc) = &mut self.doc {
            doc.revision += 1;
        }
        Ok(())
    }

    /// How many documents wait for a return (0 when no component is entered).
    pub fn depth(&self) -> usize {
        self.parents.len()
    }

    /// The paths of the documents waiting for a return, outermost first.
    pub fn parent_paths(&self) -> Vec<Option<PathBuf>> {
        self.parents.iter().map(|d| d.path.clone()).collect()
    }
}

/// The project's `assets/` directory for a document at `path`: the nearest ancestor named `assets`.
pub fn assets_root_of(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|a| a.file_name().is_some_and(|n| n == "assets"))
        .map(Path::to_path_buf)
}

/// The root `version` a text declares (best effort; other fields are ignored).
fn declared_version(text: &str) -> Option<u32> {
    #[derive(serde::Deserialize)]
    struct VersionOnly {
        version: u32,
    }
    let opts = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
    opts.from_str::<VersionOnly>(text).ok().map(|v| v.version)
}

/// Whether RON text has a `//` or `/* */` comment outside strings.
fn has_comments(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'/' if matches!(b.get(i + 1), Some(b'/' | b'*')) => return true,
            _ => i += 1,
        }
    }
    false
}

/// Which bind targets a node can take (`binding_target_mismatch`).
fn target_fits(node: &UiNodeDef, target: BindTargetDef) -> bool {
    let w = node.widget.as_ref();
    match target {
        BindTargetDef::Text => node.text.is_some(),
        BindTargetDef::Progress => matches!(w, Some(WidgetDef::Progress(_))),
        BindTargetDef::Checked => matches!(w, Some(WidgetDef::Toggle(_))),
        BindTargetDef::Value => matches!(w, Some(WidgetDef::Slider(..))),
        BindTargetDef::Input => matches!(w, Some(WidgetDef::TextInput(..))),
        BindTargetDef::Selected => matches!(w, Some(WidgetDef::Dropdown(..) | WidgetDef::TabBar(..))),
        BindTargetDef::Opacity | BindTargetDef::Visible | BindTargetDef::Enabled => true,
    }
}

fn editable(target: BindTargetDef) -> bool {
    matches!(
        target,
        BindTargetDef::Checked | BindTargetDef::Value | BindTargetDef::Input | BindTargetDef::Selected
    )
}

/// Whether the node gets clicks (a command needs them).
fn interactive(node: &UiNodeDef) -> bool {
    node.interactable
        || matches!(
            node.widget,
            Some(
                WidgetDef::Button
                    | WidgetDef::Toggle(_)
                    | WidgetDef::Slider(..)
                    | WidgetDef::TextInput(..)
                    | WidgetDef::Dropdown(..)
                    | WidgetDef::VButton(_)
                    | WidgetDef::PopupMenu(_)
                    | WidgetDef::ContextMenu(_)
            )
        )
}

/// Lints a document; `models` (the registered view models, when known) enables the model and
/// field checks. Findings are in tree order.
pub fn lint(file: &UiFile, models: Option<&ViewModels>) -> Vec<LintFinding> {
    let mut out = Vec::new();
    let mut names: std::collections::BTreeMap<String, String> = Default::default();
    let root = &file.root;
    let ignored: Vec<&str> = [
        ("name", root.name.is_some()),
        ("classes", !root.classes.is_empty()),
        ("background", root.background.is_some()),
        ("border", root.border.is_some()),
        ("text", root.text.is_some()),
        ("widget", root.widget.is_some()),
        ("bind", root.bind.is_some()),
        ("bindings", !root.bindings.is_empty()),
        ("command", root.command.is_some()),
        ("repeat", root.repeat.is_some()),
        ("safe_area", root.safe_area),
    ]
    .into_iter()
    .filter_map(|(k, set)| set.then_some(k))
    .collect();
    if !ignored.is_empty() {
        out.push(LintFinding {
            code: "root_field_ignored",
            severity: "warning",
            node: "/".to_owned(),
            message: format!(
                "spawn_ui uses only the root's style and children; these root fields have no effect: {}",
                ignored.join(", ")
            ),
        });
    }
    // Focus neighbors name nodes anywhere in the document (ADR-7827): first match by name.
    let mut named: std::collections::BTreeMap<String, bool> = Default::default();
    walk(root, &mut Vec::new(), &mut |node, _| {
        if let Some(name) = &node.name {
            named.entry(name.clone()).or_insert_with(|| focusable(node));
        }
    });
    walk(root, &mut Vec::new(), &mut |node, path| {
        let at = path_string(path);
        let parent = path.split_last().map_or(&[][..], |(_, parent)| parent);
        let scope = enclosing_repeats(root, parent);
        let item_fields = item_schema(&scope, models);
        let mut push = |code, severity, message: String| {
            out.push(LintFinding {
                code,
                severity,
                node: at.clone(),
                message,
            });
        };
        if let Some(name) = &node.name {
            if let Some(first) = names.get(name) {
                push(
                    "duplicate_name",
                    "warning",
                    format!("name {name:?} is also used by {first}; `#{name}` and ui.query find the first"),
                );
            } else {
                names.insert(name.clone(), at.clone());
            }
        }
        if node.command.is_some() && !interactive(node) {
            push(
                "command_not_interactable",
                "error",
                "a command needs a node that gets clicks: set `interactable` or use a button".to_owned(),
            );
        }
        if let Some(repeat) = &node.repeat {
            lint_repeat(node, repeat, &scope, models, item_fields.as_deref(), &mut push);
        }
        for bind in node.bind.iter().chain(&node.bindings) {
            lint_bind(node, bind, &scope, models, item_fields.as_deref(), &mut push);
            lint_converter(bind, &mut push);
        }
        lint_container(node, &mut push);
        lint_nav(node, &named, &mut push);
    });
    out
}

/// Whether the runtime makes the node focusable (a navigation target): the widgets that spawn
/// with `Focusable`.
pub fn focusable(node: &UiNodeDef) -> bool {
    matches!(
        node.widget,
        Some(
            WidgetDef::Button
                | WidgetDef::Toggle(_)
                | WidgetDef::Slider(..)
                | WidgetDef::TextInput(..)
                | WidgetDef::Dropdown(..)
                | WidgetDef::Focusable
                | WidgetDef::PopupMenu(_)
        )
    )
}

/// The v11 containers' shape checks (ADR-7827): `split_panes`, `split_ratio`, `tabs_pages`,
/// `tabs_active`.
fn lint_container(node: &UiNodeDef, push: &mut dyn FnMut(&'static str, &'static str, String)) {
    match &node.widget {
        Some(WidgetDef::Split(d)) => {
            if node.repeat.is_none() && node.children.len() != 2 {
                push(
                    "split_panes",
                    "warning",
                    format!(
                        "a split lays out two panes; this one has {} child(ren) (a third and later child gets no share)",
                        node.children.len()
                    ),
                );
            }
            let ok = |v: f32| v.is_finite() && (0.0..=1.0).contains(&v);
            if !(ok(d.ratio) && ok(d.min_ratio) && ok(d.max_ratio) && d.min_ratio <= d.max_ratio) {
                push(
                    "split_ratio",
                    "warning",
                    format!(
                        "ratio {}, min_ratio {} and max_ratio {} must lie in 0..1 with min_ratio <= max_ratio (the runtime clamps)",
                        d.ratio, d.min_ratio, d.max_ratio
                    ),
                );
            }
        }
        Some(WidgetDef::Tabs(d)) => {
            if node.repeat.is_none() && d.tabs.len() != node.children.len() {
                push(
                    "tabs_pages",
                    "warning",
                    format!(
                        "{} tab(s) and {} page(s): child i is the page of tab i",
                        d.tabs.len(),
                        node.children.len()
                    ),
                );
            }
            if !d.tabs.is_empty() && d.active >= d.tabs.len() {
                push(
                    "tabs_active",
                    "error",
                    format!(
                        "active tab {} is past the last tab ({})",
                        d.active,
                        d.tabs.len() - 1
                    ),
                );
            }
        }
        _ => {}
    }
}

/// Focus-neighbor checks (ADR-7827): `nav_unknown_target`, `nav_target_not_focusable`,
/// `nav_not_focusable`, `nav_self`. `named` maps each name (first match) to whether that node is
/// focusable.
fn lint_nav(
    node: &UiNodeDef,
    named: &std::collections::BTreeMap<String, bool>,
    push: &mut dyn FnMut(&'static str, &'static str, String),
) {
    if node.nav.is_empty() {
        return;
    }
    if !focusable(node) {
        push(
            "nav_not_focusable",
            "warning",
            "focus neighbors apply only to a focusable node (button, toggle, slider, text input, dropdown, popup menu or focusable)".to_owned(),
        );
    }
    for (dir, target) in node.nav.entries() {
        let Some(target) = target else {
            continue;
        };
        if node.name.as_deref() == Some(target) {
            push("nav_self", "warning", format!("nav.{dir} names the node itself"));
            continue;
        }
        match named.get(target) {
            None => push(
                "nav_unknown_target",
                "error",
                format!("nav.{dir} names {target:?}; no node has that name"),
            ),
            Some(false) => push(
                "nav_target_not_focusable",
                "warning",
                format!(
                    "nav.{dir} names {target:?}, which is not focusable; navigation falls back to geometry"
                ),
            ),
            Some(true) => {}
        }
    }
}

/// The fields of the collection item a binding on the node at `path` reads, for the designer's
/// picker; `None` outside a repeat template or when the schema does not know them.
pub fn item_fields_at(
    root: &UiNodeDef,
    path: &[usize],
    models: Option<&ViewModels>,
) -> Option<Vec<ViewModelField>> {
    let parent = path.split_last().map_or(&[][..], |(_, parent)| parent);
    item_schema(&enclosing_repeats(root, parent), models)
}

/// The fields of the items under `repeats`, when the view-model schema knows them; `None` when it
/// does not (no schema, or a list whose item is not a struct).
fn item_schema(repeats: &[&RepeatDef], models: Option<&ViewModels>) -> Option<Vec<ViewModelField>> {
    let models = models?;
    let mut fields: Option<Vec<ViewModelField>> = None;
    for repeat in repeats {
        fields = list_field(&repeat.source, fields.as_deref(), models).map(|f| f.item_fields.clone());
    }
    fields.filter(|f| !f.is_empty())
}

/// The list field a repeat source names: `.field` in the given item fields, else `Model.field`.
fn list_field<'a>(
    source: &str,
    item: Option<&'a [ViewModelField]>,
    models: &'a ViewModels,
) -> Option<&'a ViewModelField> {
    if let Some(rel) = source.strip_prefix('.') {
        let first = rel.split('.').next().unwrap_or(rel);
        item?.iter().find(|f| f.name == first)
    } else {
        let (model, field) = source.split_once('.')?;
        models.get(model)?.field(field.split('.').next().unwrap_or(field))
    }
}

fn lint_repeat(
    node: &UiNodeDef,
    repeat: &RepeatDef,
    scope: &[&RepeatDef],
    models: Option<&ViewModels>,
    item_fields: Option<&[ViewModelField]>,
    push: &mut dyn FnMut(&'static str, &'static str, String),
) {
    let source = repeat.source.as_str();
    let relative = source.starts_with('.');
    let well_formed = if relative {
        true
    } else {
        source
            .split_once('.')
            .is_some_and(|(m, f)| !m.is_empty() && !f.is_empty())
    };
    if !well_formed {
        push(
            "repeat_bad_source",
            "error",
            format!("repeat source {source:?} is not `Model.field` or `.field`"),
        );
    } else if relative && scope.is_empty() {
        push(
            "repeat_outside_template",
            "error",
            format!("repeat source {source:?} reads an item, but no enclosing repeat gives one"),
        );
    } else if let Some(models) = models {
        match list_field(source, item_fields, models) {
            Some(field) if field.kind != "list" => push(
                "repeat_not_a_list",
                "error",
                format!("{source} is a {}, not a list", field.type_path),
            ),
            None if !relative
                && source
                    .split_once('.')
                    .is_some_and(|(m, _)| models.get(m).is_some()) =>
            {
                push(
                    "repeat_unknown_field",
                    "error",
                    format!("no list field for repeat source {source:?}"),
                )
            }
            None if relative && item_fields.is_some() => push(
                "repeat_unknown_field",
                "error",
                format!("the item has no field for repeat source {source:?}"),
            ),
            _ => {}
        }
    }
    if node.children.is_empty() {
        push(
            "repeat_empty_template",
            "warning",
            "a repeat needs at least one child: the template spawned per item".to_owned(),
        );
    }
    if repeat.key.is_none() {
        push(
            "repeat_no_key",
            "warning",
            "without `key`, items follow by position: a removal in the middle rewrites the rows after it and shifts their state".to_owned(),
        );
    }
    // A repeat on a list is virtualized (ADR-2715): every row is a box `row height` tall, so a
    // template root of another fixed height overlaps or leaves gaps. `row_height: template`
    // (ADR-7751) takes the first template node's fixed height instead of the list's.
    let from_template = repeat.row_height == RepeatRowHeight::Template;
    if let Some(WidgetDef::List(_, list_row)) = node.widget {
        let row_height = match (from_template, RepeatDef::template_row_height(&node.children)) {
            (true, Some(h)) => h,
            (true, None) => {
                push(
                    "repeat_template_height",
                    "warning",
                    format!(
                        "row_height: template needs a fixed height (px) on the first template node; the list keeps {list_row} px"
                    ),
                );
                list_row
            }
            (false, _) => list_row,
        };
        for (i, child) in node.children.iter().enumerate() {
            if let regia_ui::Dimension::Points(h) = child.style.size[1]
                && (h - row_height).abs() > 0.5
            {
                push(
                    "repeat_on_list",
                    "warning",
                    format!(
                        "template root {i} is {h} px tall, but the list's rows are {row_height} px: size it Percent(100) or set the list's row height"
                    ),
                );
            }
        }
    } else if from_template {
        push(
            "repeat_template_height",
            "warning",
            "row_height: template only applies to a repeat on a list widget".to_owned(),
        );
    }
}

/// Targets a boolean converter result fits.
fn bool_target(target: BindTargetDef) -> bool {
    matches!(
        target,
        BindTargetDef::Visible | BindTargetDef::Enabled | BindTargetDef::Checked
    )
}

fn lint_converter(bind: &BindDef, push: &mut dyn FnMut(&'static str, &'static str, String)) {
    let Some(converter) = &bind.converter else {
        return;
    };
    if let Some(problem) = converter.problem() {
        push("converter_bad_settings", "warning", problem);
    }
    let fits = match converter {
        BindConverter::BoolToVisibility => bool_target(bind.target),
        BindConverter::LocKey { .. } => bind.target == BindTargetDef::Text,
        BindConverter::EnumText { .. } | BindConverter::Number { .. } => {
            matches!(bind.target, BindTargetDef::Text | BindTargetDef::Input)
        }
        BindConverter::Clamp { .. } | BindConverter::MapRange { .. } => !bool_target(bind.target),
    };
    if !fits {
        push(
            "converter_target_mismatch",
            "warning",
            format!(
                "converter `{}` does not fit target `{}`",
                converter.name(),
                bind.target.name()
            ),
        );
    }
    if bind.mode == BindMode::TwoWay && !converter.is_invertible() {
        push(
            "converter_one_way",
            "error",
            format!(
                "converter `{}` has no way back; a two_way edit writes nothing and reports binding_converter_one_way",
                converter.name()
            ),
        );
    }
}

fn lint_bind(
    node: &UiNodeDef,
    bind: &BindDef,
    scope: &[&RepeatDef],
    models: Option<&ViewModels>,
    item_fields: Option<&[ViewModelField]>,
    push: &mut dyn FnMut(&'static str, &'static str, String),
) {
    // Theme-token and animation bindings (ADR-7751) read no model field.
    if bind.theme.is_some() || bind.animation.is_some() {
        lint_dynamic_bind(node, bind, push);
        return;
    }
    if let Some(rel) = bind.source.strip_prefix('.') {
        if scope.is_empty() {
            push(
                "binding_item_outside_repeat",
                "error",
                format!(
                    "source {:?} reads a collection item, but this node is not in a repeat template",
                    bind.source
                ),
            );
            return;
        }
        lint_bind_target(node, bind, push);
        let first = rel.split('.').next().unwrap_or(rel);
        if let Some(fields) = item_fields
            && !first.is_empty()
            && !fields.iter().any(|f| f.name == first)
        {
            push(
                "binding_unknown_field",
                "error",
                format!("the item has no field {first:?}"),
            );
        }
        return;
    }
    let Some((model, field)) = bind
        .source
        .split_once('.')
        .filter(|(m, f)| !m.is_empty() && !f.is_empty())
    else {
        push(
            "binding_empty_source",
            "error",
            format!("source {:?} is not `Model.field`", bind.source),
        );
        return;
    };
    lint_bind_target(node, bind, push);
    if let Some(models) = models {
        match models.get(model) {
            None => push(
                "binding_unknown_model",
                "warning",
                format!("{model:?} is not a registered view model"),
            ),
            Some(info) => {
                let first = field.split('.').next().unwrap_or(field);
                if info.field(first).is_none() {
                    push(
                        "binding_unknown_field",
                        "error",
                        format!("view model {model} has no field {first:?}"),
                    );
                }
            }
        }
    }
}

/// A theme-token or animation binding (ADR-7751): its target, an empty token, a broken animation,
/// a two-way mode (nothing to write back) and the fields the source kind makes unused.
fn lint_dynamic_bind(
    node: &UiNodeDef,
    bind: &BindDef,
    push: &mut dyn FnMut(&'static str, &'static str, String),
) {
    if !target_fits(node, bind.target) {
        push(
            "binding_target_mismatch",
            "error",
            format!(
                "target `{}` does not fit a {} node",
                bind.target.name(),
                node_kind(node)
            ),
        );
    }
    if bind.theme.as_deref().is_some_and(|t| t.trim().is_empty()) {
        push(
            "binding_empty_source",
            "error",
            "the theme token is empty: name one such as color.accent".to_owned(),
        );
    }
    if bind.theme.is_none()
        && let Some(problem) = bind.animation.as_ref().and_then(animation_problem)
    {
        push("binding_bad_animation", "warning", problem);
    }
    if bind.mode == BindMode::TwoWay {
        push(
            "binding_two_way_read_only",
            "warning",
            "a theme token or an animation has no field to write back; two_way acts as one_way".to_owned(),
        );
    }
    let ignored = match (&bind.theme, &bind.animation, bind.source.is_empty()) {
        (Some(_), Some(_), _) => Some("`theme` wins: `animation` is ignored"),
        (_, _, false) => Some("`theme` or `animation` wins: `source` is ignored"),
        _ => None,
    };
    if let Some(why) = ignored {
        push("binding_source_ignored", "warning", why.to_owned());
    }
}

/// Why an animation binding does not play as written (`None` when it does): an unknown ease, a
/// duration that is not a positive number, a negative or non-finite delay, non-finite ends.
pub fn animation_problem(animation: &AnimationBindDef) -> Option<String> {
    if let Some(ease) = &animation.ease
        && regia_ui::tween::Ease::parse(ease).is_none()
    {
        return Some(format!("unknown ease {ease:?}: plays linear"));
    }
    if !(animation.duration.is_finite() && animation.duration > 0.0) {
        return Some(format!(
            "duration {} is not a positive number of seconds: the value jumps to `to`",
            animation.duration
        ));
    }
    if !(animation.delay.is_finite() && animation.delay >= 0.0) {
        return Some(format!(
            "delay {} is not a finite number of seconds >= 0",
            animation.delay
        ));
    }
    if !(animation.from.is_finite() && animation.to.is_finite()) {
        return Some("`from` and `to` must be finite numbers".to_owned());
    }
    None
}

fn lint_bind_target(
    node: &UiNodeDef,
    bind: &BindDef,
    push: &mut dyn FnMut(&'static str, &'static str, String),
) {
    if !target_fits(node, bind.target) {
        push(
            "binding_target_mismatch",
            "error",
            format!(
                "target `{}` does not fit a {} node",
                bind.target.name(),
                node_kind(node)
            ),
        );
    }
    if bind.mode == BindMode::TwoWay && !editable(bind.target) {
        push(
            "binding_two_way_read_only",
            "warning",
            format!(
                "target `{}` never changes from the UI; two_way acts as one_way",
                bind.target.name()
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::{Reflect, ReflectDefault, ReflectResource};
    use serde_json::json;

    /// A view model for the lint test.
    #[derive(Resource, Reflect, Default)]
    #[reflect(Resource, Default)]
    struct HudVm {
        hp: f32,
    }

    const DOC: &str = r#"// a comment
(
    version: 2,
    root: (
        style: (direction: column),
        children: [
            (name: "title", text: (spans: ["Hello"])),
            (name: "row", children: [
                (name: "ok", widget: button, text: (spans: ["OK"])),
                (name: "cancel", widget: button),
            ]),
        ],
    ),
)"#;

    fn designer() -> UiDesigner {
        let mut d = UiDesigner::default();
        d.open_text(DOC, None).expect("open");
        d
    }

    /// The game widgets are in the palette, are named back correctly, and a document holding them
    /// survives the writer (ADR-2958).
    #[test]
    fn every_preset_makes_a_node_and_the_game_widgets_round_trip() {
        for name in PRESETS {
            assert!(preset(name).is_some(), "{name}");
        }
        assert!(preset("no such preset").is_none());
        let mut root = UiNodeDef::default();
        for (name, kind) in [
            ("lobby_list", "lobby_list"),
            ("scoreboard", "scoreboard"),
            ("radar", "radar"),
            ("minimap", "minimap"),
            ("marker_layer", "marker_layer"),
        ] {
            let node = preset(name).expect("preset");
            assert_eq!(node_kind(&node), kind);
            root.children.push(node);
        }
        let file = UiFile {
            version: UI_RON_VERSION,
            component: None,
            root,
            variants: Vec::new(),
            back: None,
        };
        let text = write_ui_ron(&file).expect("writes");
        assert_eq!(
            parse_ui_ron(&text, "presets", &UiMigrators::default()).expect("reads back"),
            file,
            "{text}"
        );
    }

    #[test]
    fn opens_migrates_in_memory_and_notes_comments() {
        let d = designer();
        let doc = d.doc().expect("doc");
        assert_eq!(doc.file().version, UI_RON_VERSION);
        assert_eq!(doc.opened_version, 2);
        assert!(doc.had_comments);
        assert!(!doc.dirty());
        assert!(!has_comments("(name: \"a // b\")"));
    }

    #[test]
    fn resolves_paths_and_names() {
        let d = designer();
        let root = &d.doc().expect("open document").file().root;
        assert_eq!(resolve_node(root, "/").expect("resolves"), Vec::<usize>::new());
        assert_eq!(resolve_node(root, "/1/0").expect("resolves"), vec![1, 0]);
        assert_eq!(resolve_node(root, "1/1").expect("resolves"), vec![1, 1]);
        assert_eq!(resolve_node(root, "#cancel").expect("resolves"), vec![1, 1]);
        assert_eq!(resolve_node(root, "/9").expect_err("must fail").code, "bad_node");
        assert_eq!(
            resolve_node(root, "#nope").expect_err("must fail").code,
            "bad_node"
        );
        assert_eq!(resolve_node(root, "/x").expect_err("must fail").code, "bad_node");
        assert_eq!(path_string(&[1, 0]), "/1/0");
    }

    #[test]
    fn every_edit_undoes_to_the_exact_document() {
        let mut d = designer();
        let original = d.doc().expect("open document").file().clone();
        let ops = vec![
            DesignOp::Add {
                parent: vec![1],
                index: Some(0),
                node: Box::new(preset("slider").expect("slider preset")),
            },
            DesignOp::Remove(vec![0]),
            DesignOp::Duplicate(vec![0, 1]),
            DesignOp::Move {
                node: vec![0, 0],
                parent: vec![],
                index: Some(0),
            },
            DesignOp::Set {
                node: vec![0],
                key: "style.size".into(),
                value: json!([200, "Auto"]),
            },
            DesignOp::Set {
                node: vec![0],
                key: "bindings".into(),
                value: json!([{ "source": "SettingsVm.volume", "target": "value", "mode": "two_way" }]),
            },
        ];
        let doc = d.doc_mut().expect("open document");
        let mut states = vec![doc.file().clone()];
        for op in ops {
            doc.apply(op.clone()).unwrap_or_else(|e| panic!("{op:?}: {e}"));
            states.push(doc.file().clone());
        }
        assert!(doc.dirty());
        let slider = &doc.file().root.children[0];
        assert_eq!(slider.bindings[0].mode, BindMode::TwoWay);
        assert_eq!(slider.style.size[0], regia_ui::Dimension::Points(200.0));
        for expected in states.iter().rev().skip(1) {
            doc.undo().expect("undo");
            assert_eq!(doc.file(), expected);
        }
        assert_eq!(doc.file(), &original);
        assert_eq!(doc.undo().expect_err("must fail").code, "nothing_to_undo");
        for expected in states.iter().skip(1) {
            doc.redo().expect("redo");
            assert_eq!(doc.file(), expected);
        }
        assert_eq!(doc.redo().expect_err("must fail").code, "nothing_to_redo");
    }

    #[test]
    fn invalid_edits_change_nothing() {
        let mut d = designer();
        let doc = d.doc_mut().expect("open document");
        let before = (doc.file().clone(), doc.revision(), doc.undo_len());
        let bad = [
            (DesignOp::Remove(vec![]), "bad_node"),
            (DesignOp::Remove(vec![7]), "bad_node"),
            (
                DesignOp::Move {
                    node: vec![1],
                    parent: vec![1, 0],
                    index: None,
                },
                "bad_move",
            ),
            (
                DesignOp::Set {
                    node: vec![0],
                    key: "widget".into(),
                    value: json!("rocket"),
                },
                "bad_value",
            ),
            (
                DesignOp::Set {
                    node: vec![0],
                    key: "colour".into(),
                    value: json!(1),
                },
                "bad_value",
            ),
            (
                DesignOp::Set {
                    node: vec![0],
                    key: "children".into(),
                    value: json!([]),
                },
                "bad_value",
            ),
            (
                DesignOp::Add {
                    parent: vec![0],
                    index: Some(3),
                    node: Box::default(),
                },
                "bad_value",
            ),
        ];
        for (op, code) in bad {
            assert_eq!(doc.apply(op.clone()).expect_err("must fail").code, code, "{op:?}");
        }
        assert_eq!((doc.file().clone(), doc.revision(), doc.undo_len()), before);
    }

    #[test]
    fn move_after_a_sibling_lands_where_asked() {
        let mut d = designer();
        let doc = d.doc_mut().expect("open document");
        // Move "title" (/0) into "row" (/1): the row becomes /0 once title leaves.
        let at = doc
            .apply(DesignOp::Move {
                node: vec![0],
                parent: vec![1],
                index: Some(1),
            })
            .expect("ok");
        assert_eq!(at, vec![0, 1]);
        let row = &doc.file().root.children[0];
        let names: Vec<_> = row
            .children
            .iter()
            .map(|c| c.name.clone().unwrap_or_default())
            .collect();
        assert_eq!(names, ["ok", "title", "cancel"]);
    }

    #[test]
    fn set_accepts_text_shorthand_and_null_resets() {
        let node = UiNodeDef::default();
        let with_text = set_field(&node, "text", json!("Hi")).expect("set_field");
        assert_eq!(
            with_text.text.as_ref().expect("ok").spans,
            vec![TextSpanDef::Lit("Hi".into())]
        );
        let padded = set_field(&with_text, "style.padding", json!(8)).expect("set_field");
        assert_eq!(padded.style.padding, regia_ui::Edges::px(8.0));
        let reset = set_field(&padded, "style.padding", Value::Null).expect("set_field");
        assert_eq!(reset.style.padding, regia_ui::Edges::default());
        let widget =
            set_field(&node, "widget", json!({ "slider": [0.5, 0.0, 1.0, 0.1] })).expect("set_field");
        assert!(matches!(widget.widget, Some(WidgetDef::Slider(..))));
    }

    #[test]
    fn set_text_keeps_alignment_and_style() {
        let mut d = designer();
        let doc = d.doc_mut().expect("doc");
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "text.line_height".to_owned(),
            value: json!(1.75),
        })
        .expect("line height");
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "text".to_owned(),
            value: json!("Bye"),
        })
        .expect("text");
        let text = doc.file().root.children[0].text.as_ref().expect("text");
        assert_eq!(text.spans, vec![TextSpanDef::Lit("Bye".to_owned())]);
        assert_eq!(text.line_height, 1.75);
    }

    #[test]
    fn merged_sets_share_one_undo_step_until_ended() {
        let mut d = designer();
        let doc = d.doc_mut().expect("doc");
        let before = doc.file().clone();
        let set = |key: &str, value: f32| DesignOp::Set {
            node: vec![0],
            key: key.to_owned(),
            value: json!(value),
        };
        for v in [1.0, 2.0, 3.0] {
            doc.apply_merged(set("style.flex_grow", v)).expect("drag");
        }
        assert_eq!(doc.undo_len(), 1, "one drag is one step");
        doc.apply_merged(set("style.flex_shrink", 2.0))
            .expect("other field");
        assert_eq!(doc.undo_len(), 2, "another field is its own step");
        doc.end_merge();
        doc.apply_merged(set("style.flex_shrink", 4.0))
            .expect("second drag");
        assert_eq!(doc.undo_len(), 3, "a new drag is its own step");
        doc.apply_merged(set("style.flex_shrink", 5.0))
            .expect("second drag");
        assert_eq!(doc.undo_len(), 3);
        for _ in 0..3 {
            doc.undo().expect("undo");
        }
        assert_eq!(doc.file(), &before, "undo restores the exact document");
    }

    /// Spec 81 § 4.1: the undo stack keeps the last 200 snapshots; the oldest falls off, and
    /// undoing all 200 lands on the document after the first edit, not before it.
    #[test]
    fn undo_keeps_the_last_two_hundred_steps() {
        assert_eq!(UNDO_DEPTH, 200);
        let mut d = designer();
        let doc = d.doc_mut().expect("doc");
        let set = |value: f32| DesignOp::Set {
            node: vec![0],
            key: "style.flex_grow".to_owned(),
            value: json!(value),
        };
        doc.apply(set(1.0)).expect("first edit");
        let after_first = doc.file().clone();
        for v in 2..=(UNDO_DEPTH + 1) {
            doc.apply(set(v as f32)).expect("edit");
        }
        assert_eq!(doc.undo_len(), UNDO_DEPTH, "201 edits keep 200 steps");
        for _ in 0..UNDO_DEPTH {
            doc.undo().expect("undo");
        }
        assert_eq!(doc.file(), &after_first, "the oldest step fell off");
        assert_eq!(
            doc.undo().expect_err("nothing left past the depth").code,
            "nothing_to_undo"
        );
    }

    #[test]
    fn drop_zones_map_to_moves() {
        assert_eq!(DropZone::at(0.1), DropZone::Before);
        assert_eq!(DropZone::at(0.5), DropZone::Into);
        assert_eq!(DropZone::at(0.9), DropZone::After);
        // title (/0) dropped after row (/1): the index counts after the removal.
        let mut d = designer();
        let doc = d.doc_mut().expect("doc");
        let op = drop_move(&[0], &[1], DropZone::After).expect("a move");
        assert_eq!(doc.apply(op).expect("move"), vec![1]);
        assert_eq!(doc.file().root.children[1].name.as_deref(), Some("title"));
        doc.undo().expect("undo");
        // cancel (/1/1) before ok (/1/0).
        let op = drop_move(&[1, 1], &[1, 0], DropZone::Before).expect("a move");
        doc.apply(op).expect("move");
        let row = &doc.file().root.children[1];
        assert_eq!(row.children[0].name.as_deref(), Some("cancel"));
        doc.undo().expect("undo");
        // ok (/1/0) out of row, before title; then title is at /1.
        let op = drop_move(&[1, 0], &[0], DropZone::Before).expect("a move");
        assert_eq!(doc.apply(op).expect("move"), vec![0]);
        assert_eq!(doc.file().root.children[0].name.as_deref(), Some("ok"));
        doc.undo().expect("undo");
        // title into cancel (a later sibling's subtree; the parent path shifts).
        let op = drop_move(&[0], &[1, 1], DropZone::Into).expect("a move");
        assert_eq!(doc.apply(op).expect("move"), vec![0, 1, 0]);
        doc.undo().expect("undo");
        // Beside the root means into it.
        let op = drop_move(&[1, 0], &[], DropZone::After).expect("a move");
        assert_eq!(doc.apply(op).expect("move"), vec![2]);
        // No-ops and impossible drops.
        assert_eq!(drop_move(&[], &[0], DropZone::Into), None, "the root stays");
        assert_eq!(drop_move(&[1], &[1, 0], DropZone::Into), None, "own subtree");
        assert_eq!(drop_move(&[1], &[1], DropZone::After), None, "on itself");
        assert_eq!(drop_move(&[0], &[1], DropZone::Before), None, "already there");
        assert_eq!(drop_move(&[1], &[0], DropZone::After), None, "already there");
        assert_eq!(
            drop_move(&[0], &[], DropZone::Into),
            None,
            "already a child there"
        );
    }

    #[test]
    fn lint_reports_each_code() {
        let src = r#"(version: 3, root: (safe_area: true, children: [
            (name: "a", text: (spans: ["x"]), bindings: [(source: "Vm", target: text)]),
            (name: "a", widget: progress(0.5), bindings: [(source: "Vm.hp", target: value)]),
            (name: "c", command: "go"),
            (name: "d", widget: progress(0.5), bindings: [(source: "Vm.hp", target: progress, mode: two_way)]),
            (name: "e", text: (spans: ["x"]), bindings: [(source: "Other.x"), (source: "HudVm.nope")]),
        ]))"#;
        let file = parse_ui_ron(src, "t", &UiMigrators::default()).expect("ok");
        let mut app = regia_app::App::new();
        app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
        regia_ui::ViewModelAppExt::register_view_model::<HudVm>(&mut app);
        let models = app.world().resource::<ViewModels>();
        let codes: Vec<(&str, String)> = lint(&file, Some(models))
            .into_iter()
            .map(|f| (f.code, f.node))
            .collect();
        let expect = [
            ("root_field_ignored", "/"),
            ("binding_empty_source", "/0"),
            ("duplicate_name", "/1"),
            ("binding_target_mismatch", "/1"),
            ("binding_unknown_model", "/1"),
            ("command_not_interactable", "/2"),
            ("binding_two_way_read_only", "/3"),
            ("binding_unknown_model", "/3"),
            ("binding_unknown_model", "/4"),
            ("binding_unknown_field", "/4"),
        ];
        let expect: Vec<(&str, String)> = expect.iter().map(|(c, n)| (*c, (*n).to_owned())).collect();
        assert_eq!(codes, expect);
        // Without a schema the model checks are skipped.
        assert!(
            lint(&file, None)
                .iter()
                .all(|f| !f.code.starts_with("binding_unknown"))
        );
    }

    #[test]
    fn save_writes_atomically_and_reloads_equal() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-save");
        let path = dir.join("ui").join("menu.ui.ron");
        let mut d = designer();
        let doc = d.doc_mut().expect("open document");
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "command".into(),
            value: json!("menu.title"),
        })
        .expect("ok");
        assert_eq!(doc.save(None).expect_err("must fail").code, "io");
        let saved = doc.save(Some(path.clone())).expect("ok");
        assert_eq!(saved, path);
        assert!(!doc.dirty());
        let mut again = UiDesigner::default();
        again.open(&path).expect("ok");
        assert_eq!(again.doc().expect("open document").file(), doc.file());
        assert!(
            again.doc().expect("open document").had_comments,
            "a save merges into the source and keeps its comment"
        );
        assert_eq!(doc.last_save_canonical, Some(false));
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("ok"))
            .expect("ok")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-9980: a hand-written v10 file with a text input; setting `widget.max_chars` and saving
    /// changes only the version line (v10 → v11) and the text input's own line, comments kept, and the
    /// saved file loads with the limit.
    #[test]
    fn a_text_input_option_edit_keeps_every_other_line() {
        const SOURCE: &str = r#"// Sign-in form, hand-written.
(
    version: 10, // v10
    root: (
        children: [
            // The PIN field.
            (name: "pin", widget: text_input("PIN", false)), // four digits
            (name: "logo", image: (id: 3)),
        ],
    ),
)
"#;
        let dir = regia_test_utils::unique_temp_dir("ui-designer-text-input-option");
        let path = dir.join("form.ui.ron");
        std::fs::write(&path, SOURCE).expect("write");
        let mut d = UiDesigner::default();
        d.open(&path).expect("open");
        let doc = d.doc_mut().expect("doc");
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "widget.max_chars".to_owned(),
            value: json!(4),
        })
        .expect("edit");
        doc.save(None).expect("save");
        assert_eq!(
            doc.last_save_canonical,
            Some(false),
            "a merge, not the canonical writer"
        );
        let saved = std::fs::read_to_string(&path).expect("read");
        let (old, new): (Vec<&str>, Vec<&str>) = (SOURCE.lines().collect(), saved.lines().collect());
        assert_eq!(old.len(), new.len(), "{saved}");
        let changed: Vec<usize> = (0..old.len()).filter(|&i| old[i] != new[i]).collect();
        assert_eq!(
            changed,
            vec![2, 6],
            "only the version and the text input lines: {saved}"
        );
        assert_eq!(new[2], "    version: 11, // v10");
        assert!(
            new[6].contains("max_chars: 4") && new[6].ends_with("// four digits"),
            "{}",
            new[6]
        );
        let back = regia_ui::parse_ui_ron(&saved, "form", &UiMigrators::default()).expect("loads");
        let Some(WidgetDef::TextInput(placeholder, _, options)) = &back.root.children[0].widget else {
            panic!("a text input");
        };
        assert_eq!((placeholder.as_str(), options.max_chars), ("PIN", 4));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The acceptance round trip: open a hand-written file, edit one property, save; every other
    /// line is byte-identical, comments and key order included.
    #[test]
    fn saving_one_edit_changes_one_line() {
        const SOURCE: &str = r#"// Inventory screen, hand-written.
(
    version: 11,
    root: (
        // key order here is not the canonical order
        children: [
            ( // the title row
                text: (spans: ["Items"]),   /* big */
                name: "title",
                style: (padding: 8),
            ),
            (name: "list", repeat: (source: "Vm.items", key: "id"), children: [
                // one per item
                (name: "row", text: (spans: ["-"]), bindings: [(source: ".name")]),
            ]),
        ],
        style: (direction: column),
    ),
)
// the end
"#;
        let dir = regia_test_utils::unique_temp_dir("ui-designer-roundtrip");
        let path = dir.join("inv.ui.ron");
        std::fs::write(&path, SOURCE).expect("write");
        let mut d = UiDesigner::default();
        d.open(&path).expect("open");
        let doc = d.doc_mut().expect("doc");
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "style.padding".to_owned(),
            value: json!(12),
        })
        .expect("edit");
        doc.save(None).expect("save");
        assert_eq!(doc.last_save_canonical, Some(false));
        let saved = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            saved,
            SOURCE.replace("style: (padding: 8),", "style: (padding: 12.0),"),
            "exactly one property changed"
        );
        // A second edit and save merges against what the first wrote.
        let doc = d.doc_mut().expect("doc");
        doc.apply(DesignOp::Set {
            node: vec![1, 0],
            key: "name".to_owned(),
            value: json!("line"),
        })
        .expect("edit");
        doc.save(None).expect("save");
        let again = std::fs::read_to_string(&path).expect("read");
        assert_eq!(again, saved.replace("name: \"row\"", "name: \"line\""));
        // Undo back to the opened tree writes the opened bytes again.
        let doc = d.doc_mut().expect("doc");
        doc.undo().expect("undo");
        doc.undo().expect("undo");
        doc.save(None).expect("save");
        let reverted = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            reverted,
            SOURCE.replace("style: (padding: 8),", "style: (padding: 8.0),"),
            "the values round trip; only the spelling of a number written back can differ"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_an_older_file_changes_its_version_line_and_the_edit() {
        let mut d = designer();
        let dir = regia_test_utils::unique_temp_dir("ui-designer-older");
        let path = dir.join("old.ui.ron");
        let doc = d.doc_mut().expect("doc");
        doc.apply(DesignOp::Set {
            node: vec![1, 1],
            key: "command".to_owned(),
            value: json!("menu.cancel"),
        })
        .expect("edit");
        doc.save(Some(path.clone())).expect("save");
        let saved = std::fs::read_to_string(&path).expect("read");
        let expected = DOC
            .replace("version: 2", &format!("version: {UI_RON_VERSION}"))
            .replace(
                "(name: \"cancel\", widget: button)",
                "(name: \"cancel\", widget: button, command: \"menu.cancel\")",
            );
        assert_eq!(saved, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    const HISTORY_FIRST: &str = r#"        // First sibling must keep its spelling and comments.
        (name: "first", text: (spans: [(text: "loc:menu-title"), "Café \"été\" \\ 東京"])), // first tail
"#;
    const HISTORY_LABEL: &str = r#"        // Localized label, deliberately identical text to first.
        (text: (spans: [(text: "loc:menu-title"), "Café \"été\" \\ 東京"]), name: "label"), // label tail
"#;
    const HISTORY_BRANCH: &str = r#"        // Move this whole subtree with its bindings.
        (name: "branch", bindings: [(source: "Vm.visible", target: visible)], children: [
            // Bound child stays with branch.
            (name: "bound", text: (spans: ["Café \"été\" \\ 東京"]), bindings: [(source: "Vm.title")]),
        ]), // branch tail
"#;
    const HISTORY_LAST: &str = r#"        /* Untouched last sibling. */
        (name: "last", text: (spans: ["Café \"été\" \\ 東京"])), // last tail
"#;

    fn history_source(version: u32) -> String {
        format!(
            "// History fixture.\n(version: {version}, root: (\n    children: [\n{HISTORY_FIRST}{HISTORY_LABEL}{HISTORY_BRANCH}{HISTORY_LAST}    ],\n    style: (direction: column), // keep this order\n))\n// End of history fixture.\n"
        )
    }

    fn history_set(doc: &mut UiDesign, name: &str, key: &str, value: Value) {
        let node = resolve_node(&doc.file().root, name).expect("named history node");
        doc.apply(DesignOp::Set {
            node,
            key: key.to_owned(),
            value,
        })
        .expect("history edit");
    }

    /// Compare both the public document and every source byte; a canonical fallback is not a pass.
    fn assert_history_save(doc: &mut UiDesign, path: &Path, expected_text: &str) {
        let expected_file = doc.file().clone();
        assert_eq!(doc.save(None).expect("history save"), path);
        assert!(!doc.dirty());
        assert_eq!(doc.last_save_canonical, Some(false));
        assert!(!doc.comments_lost);
        assert_eq!(std::fs::read_to_string(path).expect("saved bytes"), expected_text);
        let mut reopened = UiDesigner::default();
        reopened.open(path).expect("saved document is readable");
        assert_eq!(reopened.doc().expect("reopened").file(), &expected_file);
    }

    #[test]
    fn save_history_localization_move_reset_undo_reopen_and_second_save() {
        for version in [2, UI_RON_VERSION] {
            let dir = regia_test_utils::unique_temp_dir("ui-designer-save-history");
            let path = dir.join("history.ui.ron");
            let mut designer = UiDesigner::default();
            designer
                .open_text(&history_source(version), Some(path.clone()))
                .expect("open");
            let doc = designer.doc_mut().expect("doc");
            let original = doc.file().clone();
            history_set(
                doc,
                "#label",
                "text",
                json!({"spans": [
                    {"text": "loc:menu-renamed"}, "Café \"été\" \\ 東京"
                ]}),
            );
            doc.apply(DesignOp::Move {
                node: vec![2],
                parent: vec![],
                index: Some(1),
            })
            .expect("move bound subtree");
            let moved = doc.file().clone();
            assert_eq!(moved.root.children[1], original.root.children[2]);
            history_set(doc, "#branch", "style.padding", json!(17));
            let padded = doc.file().clone();
            history_set(doc, "#branch", "style.padding", Value::Null);
            assert_eq!(doc.file(), &moved, "reset restores the field default");
            doc.undo().expect("undo reset");
            assert_eq!(doc.file(), &padded);
            doc.redo().expect("redo reset");
            assert_eq!(doc.file(), &moved);
            let changed_label = HISTORY_LABEL.replace("loc:menu-title", "loc:menu-renamed");
            let expected_first = history_source(UI_RON_VERSION).replace(
                &format!("{HISTORY_LABEL}{HISTORY_BRANCH}"),
                &format!("{HISTORY_BRANCH}{changed_label}"),
            );
            assert_history_save(doc, &path, &expected_first);

            designer.open(&path).expect("reopen first save");
            let doc = designer.doc_mut().expect("doc");
            assert_eq!(doc.file(), &moved);
            assert_eq!((doc.undo_len(), doc.redo_len()), (0, 0));
            // The label moved from /1 to /2, and the earlier sibling still has the same literal.
            history_set(
                doc,
                "#label",
                "text",
                json!({"spans": [
                    {"text": "loc:menu-renamed"}, "第二 \"save\" \\ café"
                ]}),
            );
            let second = doc.file().clone();
            assert_eq!(second.root.children[0], original.root.children[0]);
            assert_eq!(second.root.children[1], original.root.children[2]);
            let changed_again = changed_label.replace(r#"Café \"été\" \\ 東京"#, r#"第二 \"save\" \\ café"#);
            let expected_second = expected_first.replace(&changed_label, &changed_again);
            assert_history_save(doc, &path, &expected_second);
            doc.undo().expect("undo edit made after reopen");
            assert_eq!(doc.file(), &moved, "undo returns to the reopened document");
            assert_history_save(doc, &path, &expected_first);
            doc.redo().expect("save keeps redo history");
            assert_eq!(doc.file(), &second);
            assert_history_save(doc, &path, &expected_second);
            std::fs::remove_dir_all(dir).expect("cleanup");
        }
    }

    #[test]
    fn save_history_repeated_sibling_text_uses_the_named_and_indexed_spans() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-save-spans");
        let path = dir.join("spans.ui.ron");
        let source = history_source(UI_RON_VERSION);
        let mut designer = UiDesigner::default();
        designer.open_text(&source, Some(path.clone())).expect("open");
        let doc = designer.doc_mut().expect("doc");
        history_set(doc, "#last", "text", json!("最後 \"quote\" \\ fin"));
        let last = HISTORY_LAST.replace(r#"Café \"été\" \\ 東京"#, r#"最後 \"quote\" \\ fin"#);
        let first_save = source.replace(HISTORY_LAST, &last);
        assert_history_save(doc, &path, &first_save);
        designer.open(&path).expect("reopen");
        let doc = designer.doc_mut().expect("doc");
        history_set(
            doc,
            "/0",
            "text",
            json!({"spans": [
                {"text": "loc:menu-title"}, "déjà \\ \"vu\""
            ]}),
        );
        let first = HISTORY_FIRST.replace(r#"Café \"été\" \\ 東京"#, r#"déjà \\ \"vu\""#);
        assert_history_save(doc, &path, &first_save.replace(HISTORY_FIRST, &first));
        doc.undo().expect("undo after reopen");
        assert_history_save(doc, &path, &first_save);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn save_history_directory_destination_keeps_saved_bytes_dirty_state_and_history() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-save-failure-history");
        let path = dir.join("saved.ui.ron");
        let blocked = dir.join("occupied.ui.ron");
        std::fs::create_dir_all(&blocked).expect("directory destination");
        let source = history_source(UI_RON_VERSION);
        let mut designer = UiDesigner::default();
        designer.open_text(&source, Some(path.clone())).expect("open");
        let doc = designer.doc_mut().expect("doc");
        history_set(doc, "#branch", "command", json!("menu.open"));
        // Save a field that will later be reset, so the retry must remove its source span.
        let saved_text = doc.save_text().expect("text").0;
        assert_history_save(doc, &path, &saved_text);
        let saved_file = doc.file().clone();
        let saved_bytes = std::fs::read(&path).expect("saved baseline");
        history_set(doc, "#branch", "command", Value::Null);
        history_set(doc, "#last", "text", json!("変更 \"retry\" \\ fin"));
        let edited = doc.file().clone();
        doc.undo().expect("undo pending label edit");
        doc.redo().expect("redo pending label edit");
        let revision = doc.revision();
        let history = (doc.undo_len(), doc.redo_len());
        let save_flags = (doc.last_save_canonical, doc.comments_lost, doc.had_comments);
        for canonical in [false, true] {
            assert_eq!(
                doc.save_with(Some(blocked.clone()), canonical)
                    .expect_err("directory refuses rename")
                    .code,
                "io"
            );
            assert!(blocked.is_dir());
            assert_eq!(std::fs::read_dir(&blocked).expect("directory").count(), 0);
            assert_eq!(
                doc.path.as_ref(),
                Some(&path),
                "failed save-as keeps the old path"
            );
            assert!(doc.dirty(), "failed save must not mark pending edits clean");
            assert_eq!(doc.file(), &edited);
            assert_eq!(doc.revision(), revision);
            assert_eq!((doc.undo_len(), doc.redo_len()), history);
            assert_eq!(
                (doc.last_save_canonical, doc.comments_lost, doc.had_comments),
                save_flags
            );
            assert_eq!(std::fs::read(&path).expect("previous save"), saved_bytes);
            let mut reopened = UiDesigner::default();
            reopened.open(&path).expect("previous save still loads");
            assert_eq!(reopened.doc().expect("reopened").file(), &saved_file);
            assert_eq!(
                std::fs::read_dir(&dir).expect("parent").count(),
                2,
                "failed rename cleans its temporary file"
            );
        }
        doc.undo().expect("undo survives failed saves");
        doc.undo().expect("undo field reset survives failed saves");
        assert_eq!(doc.file(), &saved_file);
        doc.redo().expect("redo reset");
        doc.redo().expect("redo label");
        assert_eq!(doc.file(), &edited);
        let last = HISTORY_LAST.replace(r#"Café \"été\" \\ 東京"#, r#"変更 \"retry\" \\ fin"#);
        assert_history_save(doc, &path, &source.replace(HISTORY_LAST, &last));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    /// Exercise the existing wire methods through regia_agent's dispatcher, including error data.
    #[test]
    fn save_history_agent_dispatch_preserves_dirty_state_after_io_failure() {
        use regia_app::prelude::MinimalPlugins;

        let dir = regia_test_utils::unique_temp_dir("ui-designer-save-history-dispatch");
        let path = dir.join("saved.ui.ron");
        let blocked = dir.join("occupied.ui.ron");
        std::fs::create_dir_all(&blocked).expect("directory destination");
        let source = history_source(UI_RON_VERSION);
        std::fs::write(&path, &source).expect("fixture");
        let mut app = regia_app::App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        crate::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut id = 0;
        let mut call = |method: &str, params: Value| {
            id += 1;
            let (reply, answer) = crossbeam_channel::bounded(1);
            inbox
                .send(regia_agent::server::Inbound {
                    request: regia_agent::protocol::Request {
                        jsonrpc: "2.0".into(),
                        id: Some(json!(id)),
                        method: method.into(),
                        params,
                    },
                    reply,
                })
                .expect("request");
            app.update();
            let response = answer.try_recv().expect("dispatcher answers within this frame");
            match (response.result, response.error) {
                (_, Some(error)) => Err(error),
                (Some(result), None) => Ok(result),
                (None, None) => panic!("empty response for {method}"),
            }
        };
        call("editor.ui_designer_open", json!({"path": path})).expect("open");
        call(
            "editor.ui_designer_edit",
            json!({"op": "set", "node": "#last", "key": "text", "value": "再試行 \\ \"save\""}),
        )
        .expect("edit");
        let error = call("editor.ui_designer_save", json!({"path": blocked})).expect_err("directory failure");
        assert_eq!(error.data.expect("error data")["code"], "ui_designer_io");
        let state = call("editor.ui_designer_state", json!({})).expect("state");
        assert_eq!(state["dirty"], true);
        assert_eq!(state["undo"], 1);
        assert_eq!(std::fs::read_to_string(&path).expect("old file"), source);
        let saved = call("editor.ui_designer_save", json!({})).expect("retry at original path");
        assert_eq!(saved["dirty"], false);
        assert_eq!(saved["last_save_canonical"], false);
        let last = HISTORY_LAST.replace(r#"Café \"été\" \\ 東京"#, r#"再試行 \\ \"save\""#);
        assert_eq!(
            std::fs::read_to_string(&path).expect("retry bytes"),
            source.replace(HISTORY_LAST, &last)
        );
        call("editor.ui_designer_open", json!({"path": path})).expect("reopen");
        call(
            "editor.ui_designer_edit",
            json!({"op": "set", "node": "/3", "key": "text", "value": "temporary"}),
        )
        .expect("second edit");
        call("editor.ui_designer_edit", json!({"op": "undo"})).expect("undo after reopen");
        call("editor.ui_designer_save", json!({})).expect("save restored document");
        assert_eq!(
            std::fs::read_to_string(&path).expect("undo bytes"),
            source.replace(HISTORY_LAST, &last)
        );
        drop(app);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    /// `canonical: true` writes the normal form; a new document has no source and writes it too.
    #[test]
    fn canonical_saves_are_still_available() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-canonical");
        let path = dir.join("c.ui.ron");
        let mut d = designer();
        let doc = d.doc_mut().expect("doc");
        doc.save_with(Some(path.clone()), true).expect("save");
        assert_eq!(doc.last_save_canonical, Some(true));
        assert!(doc.comments_lost, "the caller is told");
        assert!(!doc.had_comments);
        assert!(
            !std::fs::read_to_string(&path)
                .expect("read")
                .contains("a comment")
        );
        let mut fresh = UiDesigner::default();
        fresh.new_document();
        let doc = fresh.doc_mut().expect("doc");
        doc.save(Some(path.clone())).expect("save");
        assert_eq!(doc.last_save_canonical, Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeat_and_converter_ops_edit_the_document() {
        let mut d = designer();
        let doc = d.doc_mut().expect("doc");
        doc.apply(repeat_op(vec![1], Some("Vm.items"), Some("id")))
            .expect("repeat");
        let row = &doc.file().root.children[1];
        assert_eq!(
            row.repeat,
            Some(RepeatDef {
                source: "Vm.items".to_owned(),
                key: Some("id".to_owned()),
                ..RepeatDef::default()
            })
        );
        doc.apply(repeat_op(vec![1], None, None)).expect("clear");
        assert_eq!(doc.file().root.children[1].repeat, None);

        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "bindings".to_owned(),
            value: json!([{ "source": "Vm.a" }, { "source": "Vm.b" }]),
        })
        .expect("bind");
        let converter = BindConverter::Clamp { min: 0.0, max: 1.0 };
        let op = converter_op(&doc.file().root, &[0], 1, Some(converter.clone())).expect("op");
        doc.apply(op).expect("converter");
        assert_eq!(doc.file().root.children[0].bindings[1].converter, Some(converter));
        assert_eq!(doc.file().root.children[0].bindings[0].converter, None);
        let op = converter_op(&doc.file().root, &[0], 1, None).expect("op");
        doc.apply(op).expect("clear");
        assert_eq!(doc.file().root.children[0].bindings[1].converter, None);
        let err = converter_op(&doc.file().root, &[0], 2, None).expect_err("past the end");
        assert_eq!(err.code, "bad_value");
        let err = converter_op(&doc.file().root, &[9], 0, None).expect_err("no node");
        assert_eq!(err.code, "bad_node");

        // The single `bind` counts first.
        doc.apply(DesignOp::Set {
            node: vec![0],
            key: "bind".to_owned(),
            value: json!({ "source": "Vm.c" }),
        })
        .expect("bind");
        let op = converter_op(&doc.file().root, &[0], 0, Some(BindConverter::BoolToVisibility)).expect("op");
        doc.apply(op).expect("converter");
        let node = &doc.file().root.children[0];
        assert_eq!(
            node.bind.as_ref().and_then(|b| b.converter.clone()),
            Some(BindConverter::BoolToVisibility)
        );
    }

    #[test]
    fn drop_add_places_a_palette_node() {
        let node = || preset("button").expect("preset");
        assert_eq!(
            drop_add(node(), &[1], DropZone::Into),
            DesignOp::Add {
                parent: vec![1],
                index: None,
                node: Box::new(node()),
            }
        );
        assert_eq!(
            drop_add(node(), &[1, 2], DropZone::Before),
            DesignOp::Add {
                parent: vec![1],
                index: Some(2),
                node: Box::new(node()),
            }
        );
        assert_eq!(
            drop_add(node(), &[1, 2], DropZone::After),
            DesignOp::Add {
                parent: vec![1],
                index: Some(3),
                node: Box::new(node()),
            }
        );
        assert_eq!(
            drop_add(node(), &[], DropZone::After),
            DesignOp::Add {
                parent: vec![],
                index: None,
                node: Box::new(node()),
            },
            "beside the root is into it"
        );
    }

    /// ADR-7751 lint: a theme or animation binding reads no model field (no `binding_empty_source`
    /// or `binding_unknown_model`), but an empty token, a broken animation, a two-way mode and a
    /// `source` the kind ignores are reported; `row_height: template` needs a fixed template
    /// height on a list, and the template height is then the reference for `repeat_on_list`.
    #[test]
    fn lint_reports_the_v11_fold_additions() {
        let src = r#"(version: 11, root: (children: [
            (text: (spans: ["-"]), bindings: [(theme: "color.accent")]),
            (text: (spans: ["-"]), bindings: [(theme: " ")]),
            (opacity: 1, bindings: [(target: opacity, animation: (from: 0, to: 1, duration: 0, ease: "wobble"))]),
            (text: (spans: ["-"]), bindings: [(source: "Vm.x", theme: "color.accent", mode: two_way)]),
            (widget: list(0, 32), repeat: (source: "ListVm.rows", key: "id", row_height: template),
                children: [(style: (size: (Auto, Percent(100))))]),
            (repeat: (source: "ListVm.rows", key: "id", row_height: template), children: [(name: "x")]),
            (widget: list(0, 32), repeat: (source: "ListVm.rows", key: "id", row_height: template),
                children: [(style: (size: (Auto, 48)))]),
        ]))"#;
        let file = parse_ui_ron(src, "t", &UiMigrators::default()).expect("parse");
        let mut app = regia_app::App::new();
        app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
        regia_ui::ViewModelAppExt::register_view_model::<ListVm>(&mut app);
        let models = app.world().resource::<ViewModels>();
        let found: Vec<(&str, String)> = lint(&file, Some(models))
            .into_iter()
            .map(|f| (f.code, f.node))
            .collect();
        let has = |code: &str, node: &str| found.iter().any(|(c, n)| *c == code && n == node);
        assert!(
            !found.iter().any(|(_, n)| n == "/0"),
            "a theme token lints clean: {found:?}"
        );
        assert!(has("binding_empty_source", "/1"), "{found:?}");
        assert!(has("binding_bad_animation", "/2"), "{found:?}");
        assert!(has("binding_two_way_read_only", "/3"), "{found:?}");
        assert!(has("binding_source_ignored", "/3"), "{found:?}");
        assert!(!has("binding_unknown_model", "/3"), "{found:?}");
        assert!(has("repeat_template_height", "/4"), "{found:?}");
        assert!(has("repeat_template_height", "/5"), "{found:?}");
        assert!(
            !found.iter().any(|(_, n)| n == "/6"),
            "48 px template, 48 px rows: {found:?}"
        );
        assert_eq!(
            animation_problem(&AnimationBindDef::default()),
            None,
            "the designer's default animation plays"
        );
    }

    #[test]
    fn sample_keys_follow_the_enclosing_repeats() {
        let src = r#"(version: 9, root: (children: [
            (repeat: (source: "Vm.sections"), children: [
                (name: "title", bindings: [(source: ".title")]),
                (repeat: (source: ".items", key: "id"), bindings: [(source: "Vm.flag")], children: [
                    (name: "cell", bindings: [(source: ".name"), (source: ".")]),
                ]),
            ]),
        ]))"#;
        let file = parse_ui_ron(src, "t", &UiMigrators::default()).expect("parse");
        let root = &file.root;
        assert_eq!(sample_key(root, &[0, 0], ".title"), "Vm.sections[].title");
        assert_eq!(
            sample_key(root, &[0, 1, 0], ".name"),
            "Vm.sections[].items[].name"
        );
        assert_eq!(sample_key(root, &[0, 1, 0], "."), "Vm.sections[].items[]");
        assert_eq!(sample_key(root, &[0, 1], "Vm.flag"), "Vm.flag");
    }

    /// A view model with a list, for the repeat lint.
    #[derive(Reflect, Default, Clone)]
    #[reflect(Default)]
    struct Row {
        id: u32,
        name: String,
    }

    #[derive(Resource, Reflect, Default)]
    #[reflect(Resource, Default)]
    struct ListVm {
        rows: Vec<Row>,
        total: u32,
    }

    #[test]
    fn lint_reports_repeat_and_converter_codes() {
        let src = r#"(version: 9, root: (children: [
            (repeat: (source: "nope"), children: [(name: "a")]),
            (repeat: (source: ".rows", key: "id"), children: [(name: "b")]),
            (repeat: (source: "ListVm.total", key: "id"), children: [(name: "c")]),
            (repeat: (source: "ListVm.missing", key: "id"), children: [(name: "d")]),
            (repeat: (source: "ListVm.rows", key: "id")),
            (repeat: (source: "ListVm.rows"), children: [
                (name: "e", text: (spans: ["-"]), bindings: [(source: ".name"), (source: ".wrong")]),
            ]),
            (repeat: (source: "ListVm.rows", key: "id"), widget: list(0, 32.0), children: [(name: "f", style: (size: (Percent(100), 50)))]),
            (name: "g", text: (spans: ["-"]), bindings: [(source: ".name")]),
            (name: "h", text: (spans: ["-"]), bindings: [(source: "ListVm.total", converter: clamp(min: 2, max: 1))]),
            (name: "i", text: (spans: ["-"]), bindings: [(source: "ListVm.total", converter: bool_to_visibility)]),
            (name: "j", widget: toggle(false), bindings: [(source: "ListVm.total", target: checked, mode: two_way, converter: bool_to_visibility)]),
        ]))"#;
        let file = parse_ui_ron(src, "t", &UiMigrators::default()).expect("parse");
        let mut app = regia_app::App::new();
        app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
        regia_ui::ViewModelAppExt::register_view_model::<ListVm>(&mut app);
        let models = app.world().resource::<ViewModels>();
        let found: Vec<(&str, String)> = lint(&file, Some(models))
            .into_iter()
            .filter(|f| {
                f.code.starts_with("repeat_")
                    || f.code.starts_with("converter_")
                    || f.code == "binding_item_outside_repeat"
                    || f.code == "binding_unknown_field"
            })
            .map(|f| (f.code, f.node))
            .collect();
        let expected = [
            ("repeat_bad_source", "/0"),
            ("repeat_no_key", "/0"),
            ("repeat_outside_template", "/1"),
            ("repeat_not_a_list", "/2"),
            ("repeat_unknown_field", "/3"),
            ("repeat_empty_template", "/4"),
            ("repeat_no_key", "/5"),
            ("binding_unknown_field", "/5/0"),
            ("repeat_on_list", "/6"),
            ("binding_item_outside_repeat", "/7"),
            ("converter_bad_settings", "/8"),
            ("converter_target_mismatch", "/9"),
            ("converter_one_way", "/10"),
        ];
        let expected: Vec<(&str, String)> = expected.iter().map(|(c, n)| (*c, (*n).to_owned())).collect();
        assert_eq!(found, expected);
        // Without a schema only what the file alone shows is reported.
        let without: Vec<&str> = lint(&file, None).iter().map(|f| f.code).collect();
        assert!(!without.contains(&"repeat_unknown_field"));
        assert!(!without.contains(&"repeat_not_a_list"));
    }

    /// A repeat on a list is virtualized (ADR-2715): it lints clean unless a template root has a
    /// fixed height other than the list's row height.
    #[test]
    fn a_repeat_on_a_list_lints_only_a_row_height_mismatch() {
        let src = r#"(version: 6, root: (children: [
            (repeat: (source: "ListVm.rows", key: "id"), widget: list(0, 32.0), children: [
                (name: "a", style: (size: (Percent(100), Percent(100)))),
            ]),
            (repeat: (source: "ListVm.rows", key: "id"), widget: list(0, 32.0), children: [
                (name: "b", style: (size: (Percent(100), 32))),
            ]),
            (repeat: (source: "ListVm.rows", key: "id"), widget: list(0, 32.0), children: [
                (name: "c", style: (size: (Percent(100), 31))),
            ]),
        ]))"#;
        let file = parse_ui_ron(src, "t", &UiMigrators::default()).expect("parse");
        let found: Vec<(&str, String)> = lint(&file, None)
            .into_iter()
            .filter(|f| f.code.starts_with("repeat_"))
            .map(|f| (f.code, f.node))
            .collect();
        assert_eq!(found, [("repeat_on_list", "/2".to_owned())]);
    }
}
