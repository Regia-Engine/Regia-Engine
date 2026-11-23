// SPDX-License-Identifier: MPL-2.0
//! The Biome editor (`Panel::Biome`, spec 65 § 3, w11-biome-procgen-tooling): a typed form over a
//! `.biome.ron` (`regia_procgen::biome::def::BiomeDef`): the biome's name, color, map key and
//! priority; its layers (rank, sharing, candidate source with its density noise, channels and their
//! rules, child depth); each layer's entries (spawn, weight, channels, filters with their density
//! window, footprint, placement, render, children); and the grass multipliers. Agent surface:
//! `editor.biome_panel_*` (`book/agents/biome-editor.md`).
//!
//! **Fields are paths** into the definition's serde shape, the same paths `BiomeDef::validate`
//! reports: `name`, `color`, `layers/<layer>/rank`, `layers/<layer>/source/Ground/spacing_m`,
//! `layers/<layer>/entries/<entry>/filters/0/Slope/max_deg`, `grass/<class>/density`. A list element
//! is named by its `name` (layers, channels, entries), its `class` (grass) or its index; `#<index>`
//! is always the index (a layer named `1` is `layers/1`, the second layer `layers/#1`; the form
//! addresses elements this way); an enum's variant is passed through. `set` writes any field or a whole section; `add`, `remove` and `move`
//! edit the lists. A value the definition's types refuse changes nothing (`biome_panel_bad_value`),
//! and so does an edit that would add a rule violation (`biome_invalid`, `biome_entry_unknown`), so
//! every state the panel holds after an edit is one the runtime loader (`BiomeDef::from_ron`)
//! accepts once the opened file was valid. Renaming an entry renames the children that name it;
//! renaming a channel renames it in the layer's entries.
//!
//! **Undo**: one step per edit (128 kept); a drag (`merge: true` on the same path as the step
//! before) is one step however many frames it lasts; in the form a drag ends when the pointer is
//! released. In the editor (a world with the `UndoStack`) the steps are on the shared editor
//! history: Edit ▸ Undo, Ctrl+Z and the Undo History panel list them by file, the panel's
//! Undo and Redo (and `op: undo` / `redo`) step that history, and undoing an edit of a file that is
//! no longer open opens it again. Opening another file or starting a new one over unsaved edits is refused
//! (`biome_panel_unsaved`) unless `discard` (the form asks for a second click). **Save** writes the canonical text
//! (`BiomeDef::to_ron`) atomically; RON comments in the opened file are not kept, and the panel
//! says so before the save (`comments`, `warning`).

use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_procgen::biome::def::{BiomeDef, BiomeIssue, window_factor};
use serde_json::{Value, json};

use crate::data_panels::{PanelError, confined, str_param, write_atomic};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/biome-editor.md";
const PREFIX: &str = "biome_panel";
/// The file suffix the panel edits.
pub const SUFFIX: &str = ".biome.ron";
/// Undo steps kept.
pub const UNDO_DEPTH: usize = 128;

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("{PREFIX}_{code}"), message)
}

/// How many RON comments (`// ...` lines and `/* ... */` blocks) `text` holds outside string
/// literals: what a canonical save loses.
#[must_use]
pub fn ron_comment_count(text: &str) -> usize {
    let mut count = 0;
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek().copied()) {
            ('"', _) => in_string = true,
            ('/', Some('/')) => {
                count += 1;
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                count += 1;
                chars.next();
                let mut depth = 1;
                while depth > 0 {
                    match (chars.next(), chars.peek().copied()) {
                        (None, _) => break,
                        (Some('*'), Some('/')) => {
                            chars.next();
                            depth -= 1;
                        }
                        (Some('/'), Some('*')) => {
                            chars.next();
                            depth += 1;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    count
}

/// The Biome editor's state (an edit-world resource).
#[derive(Resource, Default)]
pub struct BiomePanel {
    /// The open file, project-relative.
    pub path: Option<String>,
    /// The definition being edited.
    pub def: Option<BiomeDef>,
    /// The definition as last opened or saved (`None` for a new, unsaved one).
    saved: Option<BiomeDef>,
    /// Bumped by every applied edit, undo and redo.
    pub generation: u64,
    /// The last failed operation.
    pub error: Option<PanelError>,
    /// RON comments in the opened file (a save writes the canonical text without them).
    pub comments: usize,
    /// The opened text was the canonical text already (a save rewrites nothing else).
    pub canonical: bool,
    /// The definition before each edit, oldest first.
    undo: Vec<BiomeDef>,
    redo: Vec<BiomeDef>,
    /// The merge path of the last edit while it may still merge (a drag in progress).
    merging: Option<String>,
    /// `merging` was set by the form (a pointer drag), so the frame that releases the pointer
    /// ends it.
    form_drag: bool,
    /// An Open (`false`) or New (`true`) of a path the form refused over unsaved edits, waiting
    /// for the second click that discards them.
    pending_discard: Option<(String, bool)>,
    path_input: String,
    /// Hosted in an editor world with an `UndoStack`: edits, undo and redo use the shared editor
    /// history instead of `undo` / `redo`.
    shared: bool,
    /// The shared history's (undo, redo) depths when the panel last saw it.
    shared_depths: (usize, usize),
    /// Edits of this call, moved to the shared history when the call ends.
    pending: Vec<BiomeEdit>,
    /// A shared-history undo (`false`) or redo (`true`) to run when the call ends.
    history_request: Option<bool>,
    /// Counts `merge` chains: an edit of the same chain joins the shared entry.
    gesture: u64,
}

/// One biome edit on the shared editor history: the definition before and after.
struct BiomeEdit {
    path: String,
    field: String,
    op: String,
    before: BiomeDef,
    after: BiomeDef,
    gesture: Option<u64>,
}

/// What the shared history needs of each biome document it opened, to reactivate one by path.
#[derive(Clone)]
struct BiomeDocument {
    saved: Option<BiomeDef>,
    comments: usize,
    canonical: bool,
}

/// The biome documents the shared history may reactivate, by project-relative path.
#[derive(Resource, Default)]
struct BiomeDocuments(std::collections::BTreeMap<String, BiomeDocument>);

fn remember_biome(world: &mut World, panel: &BiomePanel) {
    if let Some(path) = &panel.path {
        world
            .get_resource_or_insert_with::<BiomeDocuments>(Default::default)
            .0
            .insert(
                path.clone(),
                BiomeDocument {
                    saved: panel.saved.clone(),
                    comments: panel.comments,
                    canonical: panel.canonical,
                },
            );
    }
}

impl BiomeEdit {
    /// Puts `def` into the panel, reactivating this edit's document when another one is open.
    fn restore(&self, world: &mut World, def: &BiomeDef) {
        let mut panel = world.remove_resource::<BiomePanel>().unwrap_or_default();
        remember_biome(world, &panel);
        if panel.path.as_deref() != Some(&self.path) {
            let doc = world
                .get_resource::<BiomeDocuments>()
                .and_then(|docs| docs.0.get(&self.path))
                .cloned();
            panel.path = Some(self.path.clone());
            self.path.clone_into(&mut panel.path_input);
            panel.saved = doc.as_ref().and_then(|d| d.saved.clone());
            panel.comments = doc.as_ref().map_or(0, |d| d.comments);
            panel.canonical = doc.as_ref().is_none_or(|d| d.canonical);
            panel.pending_discard = None;
        }
        panel.def = Some(def.clone());
        panel.merging = None;
        panel.form_drag = false;
        panel.error = None;
        panel.generation += 1;
        remember_biome(world, &panel);
        world.insert_resource(panel);
    }
}

impl crate::command::EditorCommand for BiomeEdit {
    fn apply(&mut self, world: &mut World) {
        self.restore(world, &self.after);
    }
    fn revert(&mut self, world: &mut World) {
        self.restore(world, &self.before);
    }
    fn merge(&mut self, next: &dyn crate::command::EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<Self>() else {
            return false;
        };
        if self.gesture.is_some()
            && self.gesture == next.gesture
            && self.path == next.path
            && self.field == next.field
        {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        vec![crate::command::CommandTouch::asset(
            self.path.clone(),
            (!self.field.is_empty()).then(|| self.field.clone()),
        )]
    }
    fn description(&self) -> String {
        format!("Biome: {} {} in {}", self.op, self.field, self.path)
    }
}

/// Runs a panel operation; in an editor world with an `UndoStack` its edits join the shared
/// history and its undo or redo runs there (once per egui `frame` when given).
fn with_shared<T>(
    world: &mut World,
    frame: Option<u64>,
    f: impl FnOnce(&mut BiomePanel, Option<&Path>) -> T,
) -> T {
    let _span = tracing::trace_span!(target: "regia_editor", "biome_shared_history").entered();
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<BiomePanel>().unwrap_or_default();
    let depths = shared_depths(world);
    panel.shared = depths.is_some();
    panel.shared_depths = depths.unwrap_or_default();
    let out = f(&mut panel, project.as_deref());
    if panel.shared {
        remember_biome(world, &panel);
        let merging = panel.merging.is_some();
        let mut stack = world.resource_mut::<crate::command::UndoStack>();
        for command in panel.pending.drain(..) {
            if command.gesture.is_none() {
                stack.seal();
            }
            stack.push(Box::new(command));
            if !merging {
                stack.seal();
            }
        }
    }
    let request = panel.history_request.take();
    world.insert_resource(panel);
    if let Some(redo) = request {
        if let Some(frame) = frame {
            crate::command::navigate_world_once(world, frame, redo);
        } else if redo {
            crate::command::redo_world(world);
        } else {
            crate::command::undo_world(world);
        }
    }
    if let Some(depths) = shared_depths(world)
        && let Some(mut panel) = world.get_resource_mut::<BiomePanel>()
    {
        panel.shared_depths = depths;
    }
    out
}

fn shared_depths(world: &World) -> Option<(usize, usize)> {
    world
        .get_resource::<crate::command::UndoStack>()
        .map(|s| (s.undo_depth(), s.redo_depth()))
}

/// The JSON form of a definition (`None` when it holds a number JSON cannot carry: NaN or infinity).
fn to_json(def: &BiomeDef) -> Option<Value> {
    let value = serde_json::to_value(def).ok()?;
    let back: BiomeDef = serde_json::from_value(value.clone()).ok()?;
    (back == *def).then_some(value)
}

/// The path's segments (`a/b/c`; empty segments ignored).
fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// The segment naming list element `i` by index alone (`#<i>`): never taken for a name.
fn index_seg(i: usize) -> String {
    format!("#{i}")
}

/// The element of a `len`-long list a segment names: `#<i>` is index `i`; otherwise the element
/// `named` finds (by `name` or `class`), else a plain index.
fn element_by(len: usize, seg: &str, named: impl Fn(&str) -> Option<usize>) -> Option<usize> {
    if let Some(i) = seg.strip_prefix('#') {
        return i.parse::<usize>().ok().filter(|i| *i < len);
    }
    named(seg).or_else(|| seg.parse::<usize>().ok().filter(|i| *i < len))
}

/// The element of a list a segment names: `#<index>`, else by `name`, by `class`, else by index.
fn element_index(list: &[Value], seg: &str) -> Option<usize> {
    element_by(list.len(), seg, |seg| {
        list.iter().position(|v| {
            v.get("name").and_then(Value::as_str) == Some(seg)
                || v.get("class").and_then(Value::as_str) == Some(seg)
        })
    })
}

/// The value at `segs` under `root`.
fn locate<'a>(root: &'a mut Value, segs: &[&str], path: &str) -> Result<&'a mut Value, PanelError> {
    let mut at = root;
    for seg in segs {
        at = match at {
            Value::Object(map) => map.get_mut(*seg),
            Value::Array(list) => {
                let i = element_index(list, seg);
                i.and_then(move |i| list.get_mut(i))
            }
            _ => None,
        }
        .ok_or_else(|| err("bad_field", format!("no field `{path}` (at `{seg}`)")))?;
    }
    Ok(at)
}

/// How many issues of each code `issues` holds. Edits compare these counts, not paths: renaming
/// the element an issue sits in, or moving it, changes the issue's path but adds no issue.
fn issue_counts(issues: &[BiomeIssue]) -> std::collections::BTreeMap<&str, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for issue in issues {
        *counts.entry(issue.code.as_str()).or_insert(0) += 1;
    }
    counts
}

/// Renames the references to an entry or channel after a `set` of its name (`path` is
/// `layers/<l>/entries/<e>/name` or `layers/<l>/channels/<c>/name`).
fn cascade_rename(old: &BiomeDef, new: &mut BiomeDef, segs: &[&str]) {
    let [_, layer, list, item, "name"] = segs else {
        return;
    };
    let Some(li) = element_by(old.layers.len(), layer, |seg| {
        old.layers.iter().position(|l| l.name == seg)
    }) else {
        return;
    };
    let (Some(old_layer), Some(new_layer)) = (old.layers.get(li), new.layers.get_mut(li)) else {
        return;
    };
    match *list {
        "entries" => {
            let Some(ei) = element_by(old_layer.entries.len(), item, |seg| {
                old_layer.entries.iter().position(|e| e.name == seg)
            }) else {
                return;
            };
            let (Some(from), Some(to)) = (
                old_layer.entries.get(ei).map(|e| e.name.clone()),
                new_layer.entries.get(ei).map(|e| e.name.clone()),
            ) else {
                return;
            };
            for e in &mut new_layer.entries {
                for c in &mut e.children {
                    if c.entry == from {
                        c.entry.clone_from(&to);
                    }
                }
            }
        }
        "channels" => {
            let Some(ci) = element_by(old_layer.channels.len(), item, |seg| {
                old_layer.channels.iter().position(|c| c.name == seg)
            }) else {
                return;
            };
            let (Some(from), Some(to)) = (
                old_layer.channels.get(ci).map(|c| c.name.clone()),
                new_layer.channels.get(ci).map(|c| c.name.clone()),
            ) else {
                return;
            };
            for e in &mut new_layer.entries {
                for c in &mut e.channels {
                    if *c == from {
                        c.clone_from(&to);
                    }
                }
            }
        }
        _ => {}
    }
}

impl BiomePanel {
    /// Whether the definition differs from the file (a new definition is always dirty).
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.def.is_some() && self.def != self.saved
    }

    /// Undo steps available: the shared editor history's when hosted, else the panel's own.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        if self.shared {
            self.shared_depths.0 + self.pending.len()
        } else {
            self.undo.len()
        }
    }

    /// Redo steps available: the shared editor history's when hosted, else the panel's own.
    #[must_use]
    pub fn redo_depth(&self) -> usize {
        if self.shared {
            if self.pending.is_empty() {
                self.shared_depths.1
            } else {
                0
            }
        } else {
            self.redo.len()
        }
    }

    fn reset(&mut self, path: &str, def: BiomeDef, saved: Option<BiomeDef>) {
        self.path = Some(path.to_owned());
        self.def = Some(def);
        self.saved = saved;
        self.generation = 0;
        self.undo.clear();
        self.redo.clear();
        self.merging = None;
        self.form_drag = false;
        self.pending_discard = None;
        path.clone_into(&mut self.path_input);
    }

    /// Refuses to replace unsaved edits unless `discard`.
    fn check_unsaved(&self, discard: bool) -> Result<(), PanelError> {
        if discard || !self.dirty() {
            return Ok(());
        }
        Err(err(
            "unsaved",
            format!(
                "{} has unsaved edits: save them first, or pass discard: true to drop them",
                self.path.as_deref().unwrap_or("the open biome")
            ),
        ))
    }

    /// Opens `path` (project-relative `.biome.ron`). A definition that breaks a rule opens too, with
    /// its issues listed. Unsaved edits of the open definition are dropped only with `discard`.
    ///
    /// # Errors
    /// `biome_panel_unsaved` (nothing changes), `biome_panel_no_project`, `biome_panel_bad_path`,
    /// `biome_panel_file_not_found`, or the file's `biome_invalid` (not RON, or newer than this
    /// build).
    pub fn open(&mut self, project: Option<&Path>, path: &str, discard: bool) -> Result<(), PanelError> {
        self.check_unsaved(discard)?;
        let root = project.ok_or_else(|| err("no_project", "open a project first"))?;
        let file = confined(root, path, &[SUFFIX], PREFIX)?;
        let text =
            std::fs::read_to_string(&file).map_err(|e| err("file_not_found", format!("{path}: {e}")))?;
        let def = BiomeDef::parse_unchecked(&text).map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        self.comments = ron_comment_count(&text);
        self.canonical = text == def.to_ron();
        self.reset(path, def.clone(), Some(def));
        Ok(())
    }

    /// Starts a new definition named after the file that `save` writes to `path`. Unsaved edits of
    /// the open definition are dropped only with `discard`.
    ///
    /// # Errors
    /// `biome_panel_unsaved` (nothing changes), `biome_panel_bad_path`.
    pub fn new_biome(&mut self, path: &str, discard: bool) -> Result<(), PanelError> {
        self.check_unsaved(discard)?;
        confined(Path::new("."), path, &[SUFFIX], PREFIX)?;
        let stem = path
            .rsplit('/')
            .next()
            .and_then(|n| n.strip_suffix(SUFFIX))
            .unwrap_or("biome");
        let def = BiomeDef {
            name: stem.to_owned(),
            ..BiomeDef::default()
        };
        self.comments = 0;
        self.canonical = true;
        self.reset(path, def, None);
        Ok(())
    }

    /// Applies one edit (`params.op`: `set`, `add`, `remove`, `move`, `undo`, `redo`). An edit clears
    /// the redo steps.
    ///
    /// - `set { path, value, merge? }`: writes `value` at `path` (`merge: true` on the path of the
    ///   step before joins that step: a drag).
    /// - `add { path, value, index? }`: inserts `value` into the list at `path` (at the end, or at
    ///   `index`); missing fields take the format's defaults.
    /// - `remove { path }`: removes the list element `path` names.
    /// - `move { path, to }`: moves the list element `path` names to index `to`.
    ///
    /// # Errors
    /// `biome_panel_no_biome`, `biome_panel_bad_op`, `biome_panel_bad_field`, `biome_panel_bad_value`,
    /// `biome_panel_read_only`, `biome_panel_not_a_list`, `biome_panel_non_finite`,
    /// `biome_panel_nothing_to_undo`, `biome_panel_nothing_to_redo`, `biome_invalid`,
    /// `biome_entry_unknown`; nothing changes.
    pub fn edit(&mut self, params: &Value) -> Result<(), PanelError> {
        let op = params.get("op").and_then(Value::as_str).unwrap_or_default();
        let def = self
            .def
            .as_ref()
            .ok_or_else(|| err("no_biome", "open or create a .biome.ron first"))?;
        match op {
            "undo" | "redo" if self.shared => {
                let redo = op == "redo";
                let depth = if redo {
                    self.redo_depth()
                } else {
                    self.undo_depth()
                };
                if depth == 0 {
                    return Err(if redo {
                        err("nothing_to_redo", "no undone edit to redo")
                    } else {
                        err("nothing_to_undo", "no edit to undo")
                    });
                }
                self.history_request = Some(redo);
                self.merging = None;
                return Ok(());
            }
            "undo" => {
                let step = self
                    .undo
                    .pop()
                    .ok_or_else(|| err("nothing_to_undo", "no edit to undo"))?;
                let current = self.def.replace(step);
                self.redo.extend(current);
                self.merging = None;
                self.generation += 1;
                return Ok(());
            }
            "redo" => {
                let next = self
                    .redo
                    .pop()
                    .ok_or_else(|| err("nothing_to_redo", "no undone edit to redo"))?;
                let current = self.def.replace(next);
                self.undo.extend(current);
                self.merging = None;
                self.generation += 1;
                return Ok(());
            }
            "set" | "add" | "remove" | "move" => {}
            other => {
                return Err(err(
                    "bad_op",
                    format!("unknown op `{other}` (set, add, remove, move, undo, redo)"),
                ));
            }
        }
        let path = params.get("path").and_then(Value::as_str).unwrap_or_default();
        let segs = segments(path);
        if segs.first() == Some(&"format_version") {
            return Err(err("read_only", "format_version is written by the editor"));
        }
        let mut root = to_json(def).ok_or_else(|| {
            err(
                "non_finite",
                "the definition holds a NaN or infinite number, which the form cannot carry: edit the file's text",
            )
        })?;
        match op {
            "set" => {
                let value = params
                    .get("value")
                    .cloned()
                    .ok_or_else(|| err("bad_value", "`set` needs a `value`"))?;
                if segs.is_empty() {
                    return Err(err("bad_field", "`set` needs a field `path`"));
                }
                *locate(&mut root, &segs, path)? = value;
            }
            "add" => {
                let value = params.get("value").cloned().ok_or_else(|| {
                    err(
                        "bad_value",
                        "`add` needs a `value` (missing fields take defaults)",
                    )
                })?;
                let list = locate(&mut root, &segs, path)?
                    .as_array_mut()
                    .ok_or_else(|| err("not_a_list", format!("`{path}` is not a list")))?;
                let index = match params.get("index") {
                    None | Some(Value::Null) => list.len(),
                    Some(v) => v
                        .as_u64()
                        .and_then(|i| usize::try_from(i).ok())
                        .filter(|i| *i <= list.len())
                        .ok_or_else(|| err("bad_value", format!("`index` must be 0..={}", list.len())))?,
                };
                list.insert(index, value);
            }
            _ => {
                let (last, parent) = segs
                    .split_last()
                    .ok_or_else(|| err("bad_field", format!("`{op}` needs the path of a list element")))?;
                let list = locate(&mut root, parent, path)?
                    .as_array_mut()
                    .ok_or_else(|| err("not_a_list", format!("`{path}` is not a list element")))?;
                let i = element_index(list, last)
                    .ok_or_else(|| err("bad_field", format!("no element `{last}` in `{path}`")))?;
                let item = list.remove(i);
                if op == "move" {
                    let to = params
                        .get("to")
                        .and_then(Value::as_u64)
                        .and_then(|t| usize::try_from(t).ok())
                        .filter(|t| *t <= list.len())
                        .ok_or_else(|| err("bad_value", format!("`to` must be 0..={}", list.len())))?;
                    list.insert(to, item);
                }
            }
        }
        let mut next: BiomeDef =
            serde_json::from_value(root).map_err(|e| err("bad_value", format!("`{path}`: {e}")))?;
        if op == "set" {
            cascade_rename(def, &mut next, &segs);
        }
        let before_issues = def.validate();
        let before = issue_counts(&before_issues);
        let after_issues = next.validate();
        let after = issue_counts(&after_issues);
        let grown = after
            .iter()
            .find(|(code, n)| before.get(*code).is_none_or(|b| *n > b))
            .map(|(code, _)| *code);
        if let Some(code) = grown {
            // Name an issue of that code the definition did not have, else the first of the code.
            let first = after_issues
                .iter()
                .filter(|i| i.code == code)
                .find(|i| !before_issues.contains(*i))
                .or_else(|| after_issues.iter().find(|i| i.code == code));
            if let Some(first) = first {
                return Err(PanelError::new(
                    &first.code,
                    format!("{}: {}", first.path, first.message),
                ));
            }
        }
        if next == *def {
            return Ok(());
        }
        let merge = (op == "set" && params.get("merge").and_then(Value::as_bool) == Some(true))
            .then(|| path.to_owned());
        let joins = merge.is_some() && merge == self.merging && (self.shared || !self.undo.is_empty());
        let previous = self.def.replace(next);
        if self.shared {
            if !joins {
                self.gesture = self.gesture.wrapping_add(1);
            }
            if let (Some(doc), Some(before), Some(after)) = (&self.path, previous, &self.def) {
                self.pending.push(BiomeEdit {
                    path: doc.clone(),
                    field: path.to_owned(),
                    op: op.to_owned(),
                    before,
                    after: after.clone(),
                    gesture: merge.is_some().then_some(self.gesture),
                });
            }
        } else if !joins && let Some(previous) = previous {
            self.undo.push(previous);
            if self.undo.len() > UNDO_DEPTH {
                self.undo.remove(0);
            }
        }
        self.merging = merge;
        self.redo.clear();
        self.generation += 1;
        Ok(())
    }

    /// Writes the definition to its file (atomically, canonical text). Returns `(path, bytes)`.
    ///
    /// # Errors
    /// `biome_panel_no_project`, `biome_panel_no_biome`, `biome_panel_bad_path`,
    /// `biome_panel_io_failed`, or the definition's `biome_invalid` / `biome_entry_unknown` (a file
    /// opened with issues saves only once they are fixed).
    pub fn save(&mut self, project: Option<&Path>) -> Result<(String, usize), PanelError> {
        let root = project.ok_or_else(|| err("no_project", "open a project first"))?;
        let (Some(path), Some(def)) = (self.path.clone(), self.def.as_ref()) else {
            return Err(err("no_biome", "open or create a .biome.ron first"));
        };
        if let Some(first) = def.validate().first() {
            return Err(PanelError::new(
                &first.code,
                format!("{}: {} (fix it before saving)", first.path, first.message),
            ));
        }
        let text = def.to_ron();
        let file = confined(root, &path, &[SUFFIX], PREFIX)?;
        write_atomic(&file, &text, PREFIX)?;
        self.saved = Some(def.clone());
        self.comments = 0;
        self.canonical = true;
        self.merging = None;
        Ok((path, text.len()))
    }

    /// What a save would lose, for the warning line: `None` when nothing.
    #[must_use]
    pub fn warning(&self) -> Option<String> {
        (self.comments > 0).then(|| {
            format!(
                "Saving rewrites the file in the canonical form: its {} RON comment{} will be lost.",
                self.comments,
                if self.comments == 1 { "" } else { "s" }
            )
        })
    }

    /// The panel as the agent reads it.
    #[must_use]
    pub fn json(&self) -> Value {
        let issues: Vec<Value> = self
            .def
            .as_ref()
            .map(|d| {
                d.validate()
                    .into_iter()
                    .map(|i| json!({ "code": i.code, "path": i.path, "message": i.message }))
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "path": self.path,
            "dirty": self.dirty(),
            "generation": self.generation,
            "undo_depth": self.undo_depth(),
            "redo_depth": self.redo_depth(),
            "comments": self.comments,
            "canonical": self.canonical,
            "warning": self.warning(),
            "def": self.def.as_ref().and_then(to_json),
            "issues": issues,
            "error": self.error.as_ref().map(PanelError::json),
        })
    }
}

// ------------------------------------------------------------------------------------------- drawing

/// The variants of the definition's enums, by field name: `(label, the value it starts as)`.
fn variants(key: &str) -> Option<Vec<(&'static str, Value)>> {
    let unit = |names: &[&'static str]| names.iter().map(|n| (*n, json!(n))).collect::<Vec<_>>();
    Some(match key {
        "sharing" => unit(&["Exclusive", "Shared"]),
        "collision" => unit(&["None", "Simple", "Mesh"]),
        "keep_clear_of_parent" => unit(&["Core", "Spread", "None"]),
        "outside" => unit(&["Keep", "Clear"]),
        "source" => vec![
            ("Ground", json!({ "Ground": {} })),
            ("Poisson", json!({ "Poisson": {} })),
            (
                "ScatterClass",
                json!({ "ScatterClass": { "set": "", "class": "" } }),
            ),
            ("Graph", json!({ "Graph": { "graph": "" } })),
        ],
        "rule" => vec![
            ("Slope", json!({ "Slope": {} })),
            ("Altitude", json!({ "Altitude": {} })),
            ("TerrainLayer", json!({ "TerrainLayer": { "name": "" } })),
            ("Mask", json!({ "Mask": { "asset": "" } })),
            ("Attr", json!({ "Attr": { "sel": "" } })),
            ("Random", json!({ "Random": {} })),
            ("Rest", json!("Rest")),
        ],
        "filter" => vec![
            ("Altitude", json!({ "Altitude": {} })),
            ("Slope", json!({ "Slope": {} })),
            ("Density", json!({ "Density": {} })),
            ("WaterDistance", json!({ "WaterDistance": {} })),
            ("TerrainLayer", json!({ "TerrainLayer": { "name": "" } })),
            ("Mask", json!({ "Mask": { "asset": "" } })),
            ("Attr", json!({ "Attr": { "sel": "" } })),
            ("Graph", json!({ "Graph": { "graph": "", "attr": "" } })),
        ],
        "spawn" => vec![
            ("None", json!("None")),
            ("Mesh", json!({ "Mesh": "" })),
            ("Prefab", json!({ "Prefab": "" })),
        ],
        "yaw" => vec![
            ("Random", json!("Random")),
            ("Fixed", json!({ "Fixed": 0.0 })),
            ("AlongSlope", json!("AlongSlope")),
        ],
        "scale" => vec![
            ("Keep", json!("Keep")),
            ("Range", json!({ "Range": [0.8, 1.2] })),
            ("Fixed", json!({ "Fixed": 1.0 })),
        ],
        _ => return None,
    })
}

/// The value an optional field at `path` starts as when switched on (`None`: not optional).
fn option_default(path: &str, key: &str) -> Option<Value> {
    Some(match key {
        "map_key" => json!([128, 128, 128]),
        "noise" => json!({}),
        "core_m" | "spread_m" => json!(1.0),
        "offset" => json!([[0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]]),
        // A child rule's graph is optional; a source's or a filter's is not.
        "graph" if path.contains("/children/") => json!(""),
        _ => return None,
    })
}

/// A new element of the list `key` (`siblings`: the list as it is, for a free name).
fn new_element(key: &str, siblings: &[Value], first_entry: Option<&str>) -> Option<Value> {
    let free = |stem: &str, field: &str| {
        (1..)
            .map(|k| format!("{stem} {k}"))
            .find(|n| {
                !siblings
                    .iter()
                    .any(|v| v.get(field).and_then(Value::as_str) == Some(n))
            })
            .unwrap_or_default()
    };
    Some(match key {
        "layers" => json!({ "name": free("layer", "name") }),
        "entries" => json!({ "name": free("entry", "name") }),
        "channels" => json!({ "name": free("channel", "name"), "rule": "Rest" }),
        "filters" => json!({ "Altitude": {} }),
        "children" => json!({ "entry": first_entry? }),
        "grass" => json!({ "class": free("grass", "class") }),
        _ => return None,
    })
}

/// The label of a list element: its name or class, else its variant, else its index.
fn element_label(v: &Value, i: usize) -> String {
    v.get("name")
        .or_else(|| v.get("class"))
        .or_else(|| v.get("entry"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| v.as_object().and_then(|m| m.keys().next().cloned()))
        .unwrap_or_else(|| i.to_string())
}

/// A `set` edit.
fn set(path: &str, value: Value, merge: bool) -> Value {
    json!({ "op": "set", "path": path, "value": value, "merge": merge })
}

/// The form's draw context: the edits a frame produced and the open layer's channel and entry
/// names (for the channel checkboxes and the children's entry picker).
struct Form<'a> {
    edits: &'a mut Vec<Value>,
    channels: Vec<String>,
    entries: Vec<String>,
}

/// A text field that writes on Enter or when it loses focus (one undo step per change).
fn text_field(ui: &mut egui::Ui, form: &mut Form<'_>, path: &str, current: &str) {
    let id = ui.make_persistent_id(("biome-text", path));
    let mut buffer = ui
        .data_mut(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| current.to_owned());
    let response = ui.add(egui::TextEdit::singleline(&mut buffer).desired_width(180.0));
    if response.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, buffer));
    } else {
        if response.lost_focus() && buffer != current {
            form.edits.push(set(path, json!(buffer), false));
        }
        ui.data_mut(|d| d.remove::<String>(id));
    }
}

/// A number field: integers keep their type, floats drag (a drag is one undo step).
fn number_field(ui: &mut egui::Ui, form: &mut Form<'_>, path: &str, n: &serde_json::Number) {
    if let Some(i) = n.as_i64() {
        let mut v = i;
        if ui.add(egui::DragValue::new(&mut v)).changed() {
            form.edits.push(set(path, json!(v), true));
        }
    } else if let Some(f) = n.as_f64() {
        let mut v = f;
        let speed = (f.abs() * 0.01).clamp(0.001, 10.0);
        if ui.add(egui::DragValue::new(&mut v).speed(speed)).changed() && v.is_finite() {
            form.edits.push(set(path, json!(v), true));
        }
    }
}

/// The density window of a filter as a small curve: 1 inside `[min, max]`, falling to 0 over
/// `feather` on each side (`window_factor`, spec 65 § 3.3).
fn density_curve(ui: &mut egui::Ui, min: f64, max: f64, feather: f64) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(160.0, 28.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(70)),
        egui::StrokeKind::Inside,
    );
    if !(min.is_finite() && max.is_finite() && feather.is_finite())
        || min > max
        || min < -1.0e6
        || max > 1.0e6
    {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "open window",
            egui::FontId::proportional(10.0),
            egui::Color32::from_gray(140),
        );
        return;
    }
    let pad = (max - min).max(feather).max(1.0) * 0.25 + feather;
    let (lo, hi) = (min - pad, max + pad);
    let points: Vec<egui::Pos2> = (0..=64)
        .map(|k| {
            let v = lo + (hi - lo) * f64::from(k) / 64.0;
            let y = window_factor(v, min, max, feather);
            egui::pos2(
                rect.left() + rect.width() * k as f32 / 64.0,
                rect.bottom() - 3.0 - (rect.height() - 6.0) * y as f32,
            )
        })
        .collect();
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 200, 120)),
    ));
}

/// Draws one field (and its sub-fields) of the definition at `path`.
fn field(ui: &mut egui::Ui, form: &mut Form<'_>, path: &str, key: &str, value: &Value) {
    if key == "format_version" {
        return;
    }
    let enum_key = if key.parse::<usize>().is_ok() && path.contains("/filters/") {
        "filter"
    } else {
        key
    };
    if let Some(choices) = variants(enum_key) {
        let current = match value {
            Value::String(s) => s.clone(),
            Value::Object(m) => m.keys().next().cloned().unwrap_or_default(),
            _ => String::new(),
        };
        ui.horizontal(|ui| {
            ui.label(key);
            egui::ComboBox::from_id_salt(("biome-enum", path))
                .selected_text(current.as_str())
                .show_ui(ui, |ui| {
                    for (label, start) in &choices {
                        if ui.selectable_label(current == *label, *label).clicked() && current != *label {
                            form.edits.push(set(path, start.clone(), false));
                        }
                    }
                });
        });
        if let Value::Object(m) = value
            && let Some((variant, payload)) = m.iter().next()
        {
            let inner = format!("{path}/{variant}");
            ui.indent(("biome-variant", path), |ui| match payload {
                Value::Object(fields) => {
                    for (k, v) in fields {
                        field(ui, form, &format!("{inner}/{k}"), k, v);
                    }
                    if enum_key == "filter"
                        && let (Some(lo), Some(hi), Some(f)) = (
                            fields
                                .get("min")
                                .or_else(|| fields.get("min_deg"))
                                .or_else(|| fields.get("min_m"))
                                .and_then(Value::as_f64),
                            fields
                                .get("max")
                                .or_else(|| fields.get("max_deg"))
                                .or_else(|| fields.get("max_m"))
                                .and_then(Value::as_f64),
                            fields.get("feather").and_then(Value::as_f64),
                        )
                    {
                        ui.horizontal(|ui| {
                            ui.weak("density");
                            density_curve(ui, lo, hi, f);
                        });
                    }
                }
                other => field(ui, form, &inner, variant, other),
            });
        }
        return;
    }
    if !value.is_null() && !value.is_object() && option_default(path, key).is_some() {
        ui.horizontal(|ui| {
            let mut on = true;
            if ui
                .checkbox(&mut on, "")
                .on_hover_text("Off: back to the default")
                .changed()
            {
                form.edits.push(set(path, Value::Null, false));
            }
            ui.vertical(|ui| plain_field(ui, form, path, key, value));
        });
        return;
    }
    plain_field(ui, form, path, key, value);
}

/// Draws a field that is not an enum (and, for an optional one, not its on/off switch).
fn plain_field(ui: &mut egui::Ui, form: &mut Form<'_>, path: &str, key: &str, value: &Value) {
    match value {
        Value::Null => {
            ui.horizontal(|ui| {
                let mut on = false;
                if ui.checkbox(&mut on, key).changed()
                    && let Some(start) = option_default(path, key)
                {
                    form.edits.push(set(path, start, false));
                }
                ui.weak("(auto)");
            });
        }
        Value::Bool(b) => {
            let mut v = *b;
            if ui.checkbox(&mut v, key).changed() {
                form.edits.push(set(path, json!(v), false));
            }
        }
        Value::Number(n) => {
            ui.horizontal(|ui| {
                ui.label(key);
                number_field(ui, form, path, n);
            });
        }
        Value::String(s) => {
            ui.horizontal(|ui| {
                ui.label(key);
                text_field(ui, form, path, s);
            });
        }
        Value::Array(items) if key == "color" && items.len() == 3 => {
            let mut rgb = [0.0_f32; 3];
            for (slot, v) in rgb.iter_mut().zip(items) {
                *slot = v.as_f64().unwrap_or(0.0) as f32;
            }
            ui.horizontal(|ui| {
                ui.label(key);
                if ui.color_edit_button_rgb(&mut rgb).changed() {
                    form.edits.push(set(path, json!(rgb), true));
                }
            });
        }
        Value::Array(items) if key == "channels" && path.contains("/entries/") => {
            ui.horizontal_wrapped(|ui| {
                ui.label("channels");
                if form.channels.is_empty() {
                    ui.weak("(the layer's one channel `all`)");
                }
                let names = form.channels.clone();
                for name in names {
                    let mut on = items.iter().any(|v| v.as_str() == Some(name.as_str()));
                    if ui.checkbox(&mut on, name.as_str()).changed() {
                        let mut next: Vec<String> = items
                            .iter()
                            .filter_map(Value::as_str)
                            .filter(|c| *c != name)
                            .map(str::to_owned)
                            .collect();
                        if on {
                            next.push(name.clone());
                        }
                        form.edits.push(set(path, json!(next), false));
                    }
                }
            });
        }
        Value::Array(items) if items.iter().all(|v| v.is_number() || v.is_array()) && !items.is_empty() => {
            ui.horizontal(|ui| {
                ui.label(key);
                for (i, v) in items.iter().enumerate() {
                    match v {
                        Value::Number(n) => number_field(ui, form, &format!("{path}/{i}"), n),
                        other => field(ui, form, &format!("{path}/{i}"), &i.to_string(), other),
                    }
                }
            });
        }
        Value::Array(items) => list(ui, form, path, key, items),
        Value::Object(fields) => {
            let optional = option_default(path, key).is_some();
            egui::CollapsingHeader::new(key)
                .id_salt(("biome-obj", path))
                .show(ui, |ui| {
                    if optional
                        && ui
                            .small_button("Off")
                            .on_hover_text("Back to the default")
                            .clicked()
                    {
                        form.edits.push(set(path, Value::Null, false));
                    }
                    for (k, v) in fields {
                        field(ui, form, &format!("{path}/{k}"), k, v);
                    }
                });
        }
    }
}

/// A list of elements: each in its own section with up, down and remove; `Add` below.
fn list(ui: &mut egui::Ui, form: &mut Form<'_>, path: &str, key: &str, items: &[Value]) {
    egui::CollapsingHeader::new(format!("{key} ({})", items.len()))
        .id_salt(("biome-list", path))
        .default_open(key == "layers" || key == "entries")
        .show(ui, |ui| {
            for (i, item) in items.iter().enumerate() {
                let label = element_label(item, i);
                // By index alone: an element whose name is a number is not taken for another.
                let item_path = format!("{path}/{}", index_seg(i));
                // A layer opens its channels' and entries' names for the forms inside it.
                let saved = (form.channels.clone(), form.entries.clone());
                if key == "layers" {
                    form.channels = names_of(item.get("channels"));
                    form.entries = names_of(item.get("entries"));
                }
                egui::CollapsingHeader::new(label)
                    .id_salt(("biome-item", &item_path))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui.small_button("▲").on_hover_text("Move up").clicked() && i > 0 {
                                form.edits
                                    .push(json!({ "op": "move", "path": item_path, "to": i - 1 }));
                            }
                            if ui.small_button("▼").on_hover_text("Move down").clicked()
                                && i + 1 < items.len()
                            {
                                form.edits
                                    .push(json!({ "op": "move", "path": item_path, "to": i + 1 }));
                            }
                            if ui.small_button("Remove").clicked() {
                                form.edits.push(json!({ "op": "remove", "path": item_path }));
                            }
                        });
                        match item {
                            Value::Object(fields) if key == "children" => {
                                for (k, v) in fields {
                                    if k == "entry" {
                                        entry_picker(
                                            ui,
                                            form,
                                            &format!("{item_path}/entry"),
                                            v.as_str().unwrap_or(""),
                                        );
                                    } else {
                                        field(ui, form, &format!("{item_path}/{k}"), k, v);
                                    }
                                }
                            }
                            Value::Object(fields) if key != "filters" => {
                                for (k, v) in fields {
                                    field(ui, form, &format!("{item_path}/{k}"), k, v);
                                }
                            }
                            other => field(ui, form, &item_path, &i.to_string(), other),
                        }
                    });
                if key == "layers" {
                    (form.channels, form.entries) = saved;
                }
            }
            let first = form.entries.first().cloned();
            match new_element(key, items, first.as_deref()) {
                Some(value) => {
                    let noun = match key {
                        "layers" => "layer",
                        "entries" => "entry",
                        "channels" => "channel",
                        "filters" => "filter",
                        "children" => "child",
                        _ => "grass class",
                    };
                    if ui.button(format!("Add {noun}")).clicked() {
                        form.edits
                            .push(json!({ "op": "add", "path": path, "value": value }));
                    }
                }
                None if key == "children" => {
                    ui.weak("Add an entry to the layer first.");
                }
                None => {}
            }
        });
}

/// The `name`s of a list value.
fn names_of(list: Option<&Value>) -> Vec<String> {
    list.and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .filter_map(|v| v.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A child rule's entry: one of the layer's entries.
fn entry_picker(ui: &mut egui::Ui, form: &mut Form<'_>, path: &str, current: &str) {
    ui.horizontal(|ui| {
        ui.label("entry");
        egui::ComboBox::from_id_salt(("biome-child", path))
            .selected_text(current)
            .show_ui(ui, |ui| {
                for name in form.entries.clone() {
                    if ui.selectable_label(current == name, name.as_str()).clicked() && current != name {
                        form.edits.push(set(path, json!(name), false));
                    }
                }
            });
    });
}

/// The `.biome.ron` files of the project (the asset picker of this panel and the procgen Area tool).
#[must_use]
pub fn biome_files(world: &World) -> Vec<String> {
    crate::procgen::widgets::asset_choices(world, SUFFIX)
        .into_iter()
        .filter(|p| p.ends_with(SUFFIX))
        .collect()
}

/// What the form's header row asked for in a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FormAction {
    /// Nothing.
    None,
    /// Open a path (`true`: the second click that discards unsaved edits).
    Open(String, bool),
    /// Start a new biome at a path (`true`: discard unsaved edits).
    New(String, bool),
    /// Save.
    Save,
    /// Keep the unsaved edits: forget the refused Open or New.
    KeepEdits,
}

impl BiomePanel {
    /// Applies one frame of the form: its header `action`, then its `edits` in order (the first
    /// refused edit stops the rest). An Open or New refused over unsaved edits waits for the
    /// second click (`FormAction::Open(path, true)`) or `KeepEdits`. `pointer_down`: whether a
    /// pointer button is held at the end of the frame; once it is released a form drag
    /// (`merge`) ends, so the next drag on the same field is its own undo step.
    pub(crate) fn apply_form_frame(
        &mut self,
        project: Option<&Path>,
        action: FormAction,
        edits: &[Value],
        pointer_down: bool,
    ) {
        let (result, asked) = match action {
            FormAction::None => (Ok(()), None),
            FormAction::Open(path, discard) => (self.open(project, &path, discard), Some((path, false))),
            FormAction::New(path, discard) => (self.new_biome(&path, discard), Some((path, true))),
            FormAction::Save => (self.save(project).map(|_| ()), None),
            FormAction::KeepEdits => {
                self.pending_discard = None;
                self.error = None;
                // The file drop-down already showed the refused path: show the open file again.
                if let Some(path) = &self.path {
                    path.clone_into(&mut self.path_input);
                }
                (Ok(()), None)
            }
        };
        match result {
            Ok(()) if asked.is_some() => self.error = None,
            Ok(()) => {}
            Err(e) => {
                if asked.is_some() {
                    // Only a refusal over unsaved edits waits for the second click.
                    self.pending_discard = asked.filter(|_| e.code.strip_prefix(PREFIX) == Some("_unsaved"));
                }
                self.error = Some(e);
            }
        }
        for edit in edits {
            let merges = edit.get("merge").and_then(Value::as_bool) == Some(true);
            if let Err(e) = self.edit(edit) {
                self.error = Some(e);
                break;
            }
            self.form_drag = merges;
            self.error = None;
        }
        if self.form_drag && !pointer_down {
            self.merging = None;
            self.form_drag = false;
        }
    }
}

/// Draws the panel.
pub fn biome_panel(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::info_span!(target: "regia_editor", "biome_panel").entered();
    let mut panel = world.remove_resource::<BiomePanel>().unwrap_or_default();
    let mut edits: Vec<Value> = Vec::new();
    let mut action = FormAction::None;
    draw(ui, &mut panel, world, &mut edits, &mut action);
    world.insert_resource(panel);
    let pointer_down = ui.input(|i| i.pointer.any_down());
    let frame = ui.ctx().cumulative_frame_nr();
    with_shared(world, Some(frame), |panel, project| {
        panel.apply_form_frame(project, action, &edits, pointer_down);
    });
}

fn draw(
    ui: &mut egui::Ui,
    panel: &mut BiomePanel,
    world: &World,
    edits: &mut Vec<Value>,
    action: &mut FormAction,
) {
    crate::data_panels::header(ui, "Biome", "layers · entries · filters · grass (.biome.ron)");
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("biome-panel-files")
            .selected_text(if panel.path_input.is_empty() {
                "pick a .biome.ron"
            } else {
                panel.path_input.as_str()
            })
            .show_ui(ui, |ui| {
                // Listed only while the list is open: the project's files are read from disk.
                for f in &biome_files(world) {
                    if ui.selectable_label(panel.path_input == *f, f.as_str()).clicked() {
                        panel.path_input.clone_from(f);
                        *action = FormAction::Open(f.clone(), false);
                    }
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut panel.path_input)
                .hint_text("assets/biomes/forest.biome.ron")
                .desired_width(200.0),
        );
        if ui.button("Open").clicked() {
            *action = FormAction::Open(panel.path_input.trim().to_owned(), false);
        }
        if ui.button("New").clicked() {
            *action = FormAction::New(panel.path_input.trim().to_owned(), false);
        }
        if ui.add_enabled(panel.dirty(), egui::Button::new("Save")).clicked() {
            *action = FormAction::Save;
        }
        if ui
            .add_enabled(panel.undo_depth() > 0, egui::Button::new("Undo"))
            .clicked()
        {
            edits.push(json!({ "op": "undo" }));
        }
        if ui
            .add_enabled(panel.redo_depth() > 0, egui::Button::new("Redo"))
            .clicked()
        {
            edits.push(json!({ "op": "redo" }));
        }
    });
    if let Some(e) = &panel.error {
        e.draw(ui);
    }
    if let Some((path, new)) = panel.pending_discard.clone() {
        // The second click: Open or New was refused over unsaved edits (`biome_panel_unsaved`).
        ui.horizontal(|ui| {
            let verb = if new { "start" } else { "open" };
            if ui.button(format!("Discard edits and {verb} {path}")).clicked() {
                *action = if new {
                    FormAction::New(path.clone(), true)
                } else {
                    FormAction::Open(path.clone(), true)
                };
            }
            if ui.button("Keep editing").clicked() {
                *action = FormAction::KeepEdits;
            }
        });
    }
    if let Some(w) = panel.warning() {
        ui.colored_label(egui::Color32::from_rgb(230, 170, 80), w);
    }
    let Some(def) = panel.def.as_ref() else {
        ui.weak("Open a .biome.ron or start a new one.");
        return;
    };
    ui.label(format!(
        "{} · {} layer(s) · {} entr{}{}",
        panel.path.as_deref().unwrap_or("unsaved"),
        def.layers.len(),
        def.layers.iter().map(|l| l.entries.len()).sum::<usize>(),
        if def.layers.iter().map(|l| l.entries.len()).sum::<usize>() == 1 {
            "y"
        } else {
            "ies"
        },
        if panel.dirty() { " *" } else { "" }
    ));
    for issue in def.validate() {
        ui.colored_label(
            egui::Color32::from_rgb(230, 170, 80),
            format!("{} {}: {}", issue.code, issue.path, issue.message),
        );
    }
    let Some(Value::Object(root)) = to_json(def) else {
        ui.colored_label(
            egui::Color32::from_rgb(230, 90, 90),
            "This file holds a NaN or infinite number: edit its text.",
        );
        return;
    };
    let mut form = Form {
        edits,
        channels: Vec::new(),
        entries: Vec::new(),
    };
    egui::ScrollArea::vertical()
        .id_salt("biome-panel-form")
        .show(ui, |ui| {
            for (k, v) in &root {
                field(ui, &mut form, k, k, v);
            }
        });
}

// --------------------------------------------------------------------------------------- agent surface

fn with_panel<T>(
    world: &mut World,
    f: impl FnOnce(&mut BiomePanel, Option<&Path>) -> Result<T, PanelError>,
) -> Result<T, RpcError> {
    let result = with_shared(world, None, |panel, project| {
        let result = f(panel, project);
        match &result {
            Ok(_) => panel.error = None,
            Err(e) => panel.error = Some(e.clone()),
        }
        result
    });
    result.map_err(|e| e.rpc(HINT))
}

fn state(world: &World) -> Value {
    match world.get_resource::<BiomePanel>() {
        Some(panel) => panel.json(),
        None => BiomePanel::default().json(),
    }
}

fn panel_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let create = params.get("new").and_then(Value::as_bool).unwrap_or(false);
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    with_panel(world, |panel, project| {
        if create {
            panel.new_biome(&path, discard)
        } else {
            panel.open(project, &path, discard)
        }
    })?;
    Ok(state(world))
}

fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(state(world))
}

fn panel_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, _| {
        panel.edit(params)?;
        // An agent `merge` chain is not a form drag: a released pointer does not end it.
        panel.form_drag = false;
        Ok(())
    })?;
    Ok(state(world))
}

fn panel_save(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let (written, bytes) = with_panel(world, |panel, project| panel.save(project))?;
    Ok(json!({ "written": written, "bytes": bytes }))
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const STATE: &str = "{ path, dirty, generation, undo_depth, redo_depth, comments, canonical, warning, \
    def: <the BiomeDef as JSON: { format_version, name, color, map_key, priority, layers: [{ name, rank, sharing, source, \
    channels, entries: [{ name, spawn, weight, channels, enabled, filters, filters_off, footprint, placement, render, \
    children }], max_child_depth }], grass }>, issues: [{ code, path, message }], error }";

const OPEN_PARAMS: &[ParamSchema] = &[
    param("path", "string", true, "Project-relative .biome.ron."),
    param(
        "new",
        "boolean",
        false,
        "Start a new biome named after the file that save writes to path.",
    ),
    param(
        "discard",
        "boolean",
        false,
        "Drop the open biome's unsaved edits (without it, biome_panel_unsaved refuses and nothing changes).",
    ),
];
const EDIT_PARAMS: &[ParamSchema] = &[
    param("op", "string", true, "set | add | remove | move | undo | redo"),
    param(
        "path",
        "string",
        false,
        "The field: `/`-separated serde path (`layers/<layer>/entries/<entry>/weight`, \
         `layers/<layer>/source/Ground/spacing_m`, `grass/<class>/density`); a list element by name, class or index \
         (`#<index>` is always the index).",
    ),
    param(
        "value",
        "any",
        false,
        "set: the field's new value (an enum as `\"Rest\"` or `{ \"Slope\": { \"max_deg\": 30 } }`); add: the new \
         element (missing fields take the format's defaults).",
    ),
    param(
        "index",
        "number",
        false,
        "add: insert at this index (default: the end).",
    ),
    param("to", "number", false, "move: the element's new index."),
    param(
        "merge",
        "boolean",
        false,
        "set: join the previous step when it set the same path with merge (a drag).",
    ),
];

/// The Biome editor's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.biome_panel_open",
                tool: "biome_panel_open",
                description: "Open a .biome.ron in the Biome editor (a file with rule issues opens too), or start a \
                    new one. Refused over unsaved edits (biome_panel_unsaved) unless discard.",
                params: OPEN_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.biome_panel_get",
                tool: "biome_panel_get",
                description: "The Biome editor: the definition as JSON, its rule issues, dirty, undo depth and the \
                    comment-loss warning.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.biome_panel_edit",
                tool: "biome_panel_edit",
                description: "Edit the open biome by field path (in memory, undoable; editor.biome_panel_save writes \
                    it). A value the format refuses or an edit that adds a rule issue changes nothing.",
                params: EDIT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.biome_panel_save",
                tool: "biome_panel_save",
                description: "Write the open biome to its .biome.ron in the canonical form, atomically (RON comments \
                    are not kept).",
                params: &[],
                result: "{ written, bytes }",
                mutating: true,
                permission: Permission::Assets,
            },
            panel_save as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
