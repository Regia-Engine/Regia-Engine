// SPDX-License-Identifier: MPL-2.0
//! The Logic Graph panel (`docs/specs/102-logic-graph.md` § 6, phase 2, ADR-7224): an egui-snarl
//! canvas that authors `.logic.ron` rules as event → condition → action node chains, with a palette
//! (events left, conditions middle, actions right), an inspector for the selected node, the live
//! issues list and, while the game plays, the watched graph's activity (which rules fired, matched
//! or were rejected on its latest tick) and its variables.
//!
//! State lives in the [`LogicEditor`] world resource, so the panel and the agent protocol
//! (`editor.logic_*`, [`agent`]) edit the same [`LogicGraphDoc`]. Every structural edit is a
//! [`LogicOp`] list applied by [`edit`] as one [`LogicEdit`] snapshot on the editor's shared
//! [`UndoStack`] (`editor.undo` and Ctrl+Z revert it in order with level edits). The document model,
//! the ops and the validator are headless (`regia_ai::logic_edit`). Positions are saved to the
//! editor-only `x.logic.layout.ron` sidecar; the `.logic.ron` stays the frozen v1 format.
//!
//! A save while the game plays reloads the play world's graphs that ran the file as last saved
//! through `LogicReloads` (spec 102 § 7: locals, inbox and fire counts are kept by
//! rule name); [`LogicEditor::reload`] then shows what the reload did.
//!
//! The debugger (ADR-8021): rule and event [`breakpoints`] kept per user in
//! `.regia/logic_breakpoints.ron`, Step to the next fired rule, the [`watch`] list with hover values
//! and the evaluate box over a scope snapshot of the paused graph, wires that pulse when their rule
//! fires, the [`variables`] list, a fuzzy palette search, and the graph's issues in the editor-wide
//! Problems panel ([`problems`]).

pub mod agent;
pub mod breakpoints;
pub mod debugger_agent;
pub mod panel;
pub mod problems;
pub mod usages;
pub mod variables;
pub mod watch;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ai::Blackboard;
use regia_ai::BtTaskRegistry;
use regia_ai::logic::{LogicGraph, LogicGraphDef, LogicGraphStatus, LogicReloadOutcome, LogicReloads};
use regia_ai::logic_edit::{
    self, LogicBatchError, LogicGraphDoc, LogicIssue, LogicLayoutFile, LogicOp, LogicSeverity, NodeId,
};
use regia_ecs::prelude::*;

use crate::command::{EditorCommand, UndoStack};
use crate::pie::PieSession;

/// What a rule did on the watched graph's latest traced tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleActivity {
    /// Its event arrived and its condition held: its actions started.
    Fired,
    /// Its condition held but it did not run (cooldown or a busy chain).
    Matched,
    /// Its event arrived and its condition was false.
    Rejected,
    /// One of its actions failed.
    Failed,
}

impl RuleActivity {
    /// `fired`, `matched`, `rejected` or `failed`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Fired => "fired",
            Self::Matched => "matched",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }
}

/// A running graph in the play world, as the panel shows it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveView {
    /// The graph entity (bits) in the play world.
    pub entity: u64,
    /// Started and not paused.
    pub running: bool,
    /// Graph ticks so far.
    pub ticks: u64,
    /// The tick the activity is from (the newest traced tick).
    pub activity_tick: Option<u64>,
    /// Events received on that tick, in order.
    pub events: Vec<String>,
    /// What each rule did on that tick (rules not named did nothing).
    pub activity: BTreeMap<String, RuleActivity>,
    /// The index of the last action each rule started on that tick.
    pub actions_started: BTreeMap<String, usize>,
    /// Fires per rule since the graph started.
    pub fires: BTreeMap<String, u64>,
    /// The `local/` scope: name and value.
    pub locals: Vec<(String, String)>,
    /// The entity scope (the entity's `Blackboard`): key and value.
    pub entity_vars: Vec<(String, String)>,
    /// The `world/` scope: the declared world blackboards' entries as `<board>/<key>` and value.
    pub world_vars: Vec<(String, String)>,
    /// The running definition equals the open document.
    pub matches_document: bool,
}

/// A live-view control for the watched graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveControl {
    /// Pause it.
    Pause,
    /// Resume it.
    Resume,
    /// Run one more tick, then pause (`LogicGraph::step`).
    Step,
    /// Run until a tick in which any rule fires, then pause at its end (ADR-8021: the runtime
    /// stops only between ticks, so this is the finest step past one tick). Development builds
    /// only (it rides on `LogicGraphDebug`).
    StepToFire,
}

/// What the last save did to the play world's graphs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReloadView {
    /// Which save this is about (counts from 1).
    pub save: u64,
    /// Graphs the save was queued for.
    pub queued: u64,
    /// Graphs that kept their running state (`kept`).
    pub kept: u64,
    /// Graphs that restarted (`restarted`).
    pub restarted: u64,
    /// Graphs that rejected it (did not compile there).
    pub rejected: u64,
    /// Why the latest rejection happened.
    pub reason: Option<String>,
}

/// The Logic Graph editor's world resource.
#[derive(Resource)]
pub struct LogicEditor {
    /// The file the graph saves to (`None` for a new graph not saved yet).
    pub path: Option<PathBuf>,
    /// The document.
    pub doc: LogicGraphDoc,
    /// The definition as last opened or saved (`None`: never saved).
    saved: Option<LogicGraphDef>,
    /// The layout as last opened or saved.
    saved_layout: BTreeMap<String, [f32; 2]>,
    /// Bumped by every document change.
    pub revision: u64,
    /// Which document is open: undo entries of another document do nothing.
    session: u64,
    /// Last action's status line.
    pub toast: String,
    /// The selected node.
    pub selected: Option<NodeId>,
    /// Graph entities in the play world (bits), refreshed by [`pump`].
    pub agents: Vec<u64>,
    /// The graph entity whose live state to show (`None`: the first running the open file, else the
    /// first one).
    pub watch: Option<u64>,
    /// The watched graph's live state while the game plays.
    pub live: Option<LiveView>,
    /// What the last save did to running graphs.
    pub reload: Option<ReloadView>,
    /// Saves so far.
    pub saves: u64,
    /// A pause / resume / step the next [`pump`] applies to the watched graph (the live view's
    /// buttons; spec 102 § 9).
    pub pending_control: Option<LiveControl>,
    /// A save the next [`pump`] delivers: the path, the new definition and the definition it
    /// replaces (graphs running that one reload).
    pending_reload: Option<(PathBuf, LogicGraphDef, Option<LogicGraphDef>)>,
    /// The play world's reload totals when the last save was delivered, with the graphs it went to.
    reload_baseline: Option<(u64, Vec<u64>)>,
    /// Panel state (snarl canvas, text drafts).
    pub ui: panel::PanelState,
    /// The breakpoints (per user, `.regia/logic_breakpoints.ron`; ADR-8021).
    pub breakpoints: breakpoints::Breakpoints,
    /// The Watch list, its scope snapshot and the evaluate box.
    pub watches: watch::WatchList,
    /// The wires that pulse because their rule fired.
    pub pulse: watch::Pulse,
    /// The graph entity a Step to the next fired rule runs (until a rule fires there).
    pub step_fire: Option<u64>,
    /// The issues of the document revision the Problems panel last read.
    problems: Option<(u64, std::sync::Arc<Vec<LogicIssue>>)>,
}

impl Default for LogicEditor {
    fn default() -> Self {
        Self {
            path: None,
            doc: LogicGraphDoc::new(),
            saved: None,
            saved_layout: BTreeMap::new(),
            revision: 0,
            session: 0,
            toast: String::new(),
            selected: None,
            agents: Vec::new(),
            watch: None,
            live: None,
            reload: None,
            saves: 0,
            pending_control: None,
            pending_reload: None,
            reload_baseline: None,
            ui: panel::PanelState::default(),
            breakpoints: breakpoints::Breakpoints::default(),
            watches: watch::WatchList::default(),
            pulse: watch::Pulse::default(),
            step_fire: None,
            problems: None,
        }
    }
}

impl LogicEditor {
    /// Whether the document differs from the file (graph or layout).
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.doc.to_def()) || self.saved_layout != self.doc.layout()
    }

    /// Whether a graph is open (opened, or started with [`Self::new_graph`]).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.path.is_some() || self.saved.is_some() || self.revision > 0
    }

    /// Whether opening or starting another graph would lose work.
    #[must_use]
    pub fn has_unsaved_work(&self) -> bool {
        self.is_open() && self.dirty() && (self.saved.is_some() || !self.doc.nodes.is_empty())
    }

    /// Starts a new, empty graph (saved to `path` later).
    pub fn new_graph(&mut self, path: Option<PathBuf>) {
        self.doc = LogicGraphDoc::new();
        self.path = path;
        self.saved = None;
        self.saved_layout = BTreeMap::new();
        self.session += 1;
        self.revision += 1;
        self.selected = None;
        self.reload = None;
        self.toast = "new graph".into();
    }

    /// Shows `def` (an entity's graph) as the open document: it has no file, so the panel's Save
    /// asks for a path, and it is not dirty until edited. The layout is the default one.
    pub fn open_def(&mut self, def: &LogicGraphDef) {
        let doc = LogicGraphDoc::from_def(def, &BTreeMap::new());
        self.saved = Some(doc.to_def());
        self.saved_layout = doc.layout();
        self.doc = doc;
        self.path = None;
        self.session += 1;
        self.revision += 1;
        self.selected = None;
        self.reload = None;
        self.toast = "opened the entity's graph".into();
    }

    /// Opens a `.logic.ron` and its layout sidecar (a missing or unreadable sidecar gives the
    /// default layout).
    ///
    /// # Errors
    ///
    /// The file cannot be read or parsed.
    pub fn open(&mut self, path: &Path) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let positions = std::fs::read_to_string(logic_edit::layout_path(path))
            .map(|t| LogicLayoutFile::positions_from_ron(&t))
            .unwrap_or_default();
        let doc = LogicGraphDoc::from_ron(&text, &positions)
            .map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        self.saved = Some(doc.to_def());
        self.saved_layout = doc.layout();
        self.doc = doc;
        self.path = Some(path.to_path_buf());
        self.session += 1;
        self.revision += 1;
        self.selected = None;
        self.reload = None;
        self.toast = format!("opened {}", path.display());
        Ok(())
    }
}

/// Opens `path` in the Logic Graph panel and brings the panel forward (the next shell frame
/// shows it): the prefab's Open graph / New graph for this prefab (`crate::prefab_logic`, P-165).
/// Unsaved work in the panel is replaced; the caller asks first.
///
/// # Errors
///
/// The file cannot be read or parsed.
pub fn open_in_panel(world: &mut World, path: &Path) -> Result<(), String> {
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .open(path)?;
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::LogicGraph),
    );
    Ok(())
}

/// The task registry of a world, when it has any task registered.
fn registry(world: &World) -> Option<&BtTaskRegistry> {
    world.get_resource::<BtTaskRegistry>().filter(|r| !r.is_empty())
}

/// Every problem of the open graph (tasks checked against the world's registry when it has one).
#[must_use]
pub fn diagnose(world: &World) -> Vec<LogicIssue> {
    let empty = LogicGraphDoc::new();
    let doc = world.get_resource::<LogicEditor>().map_or(&empty, |e| &e.doc);
    logic_edit::validate(doc, registry(world))
}

/// The action palette: the built-in `logic::*` tasks, then the registered task paths.
#[must_use]
pub fn palette_tasks(world: &World) -> Vec<String> {
    let mut tasks: Vec<String> = regia_ai::logic::BUILTIN_TASKS
        .iter()
        .map(|t| t.to_string())
        .collect();
    if let Some(r) = world.get_resource::<BtTaskRegistry>() {
        tasks.extend(r.task_paths().map(str::to_string));
    }
    tasks
}

/// A refused save: a stable code and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveError {
    /// `no_graph_open`, `bad_path`, `invalid_graph` or `save_failed`.
    pub code: &'static str,
    /// A sentence for people.
    pub message: String,
}

impl SaveError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Saves the open graph as canonical `.logic.ron` to `path` (or where it was opened) and its layout
/// sidecar next to it, atomically. A graph with errors is refused (`invalid_graph`) and nothing is
/// written. While a play session runs, the next [`pump`] reloads the graphs that ran the file as
/// last opened or saved.
///
/// # Errors
///
/// No graph open, no path, a path that is not a `.logic.ron`, validation errors, or a failed write.
pub fn save(world: &mut World, path: Option<&Path>) -> Result<PathBuf, SaveError> {
    let issues = diagnose(world);
    let mut editor = world
        .get_resource_mut::<LogicEditor>()
        .filter(|e| e.is_open())
        .ok_or_else(|| SaveError::new("no_graph_open", "no logic graph is open"))?;
    let path = path
        .map(Path::to_path_buf)
        .or_else(|| editor.path.clone())
        .ok_or_else(|| SaveError::new("bad_path", "no graph path: pass one to save as"))?;
    if !path.to_string_lossy().ends_with(".logic.ron") {
        return Err(SaveError::new(
            "bad_path",
            format!("{} is not a .logic.ron path", path.display()),
        ));
    }
    if !logic_edit::is_valid(&issues) {
        let first = issues
            .iter()
            .find(|i| i.severity == LogicSeverity::Error)
            .map(|i| match i.node {
                Some(n) => format!("{} at node {n}", i.code),
                None => i.code.to_string(),
            })
            .unwrap_or_default();
        editor.toast = format!("not saved: the graph has errors ({first})");
        return Err(SaveError::new(
            "invalid_graph",
            format!("the graph has errors and was not saved (first: {first})"),
        ));
    }
    let def = editor.doc.to_def();
    let text = def
        .to_ron()
        .map_err(|e| SaveError::new("save_failed", e.to_string()))?;
    let layout = LogicLayoutFile::of(&editor.doc);
    let layout_text = layout.to_ron().map_err(|e| SaveError::new("save_failed", e))?;
    crate::atomic_write::write_atomic(&path, text.as_bytes())
        .map_err(|e| SaveError::new("save_failed", e))?;
    crate::atomic_write::write_atomic(&logic_edit::layout_path(&path), layout_text.as_bytes())
        .map_err(|e| SaveError::new("save_failed", e))?;
    let previous = editor.saved.replace(def.clone());
    editor.saved_layout = layout.positions;
    editor.path = Some(path.clone());
    editor.saves += 1;
    editor.reload = Some(ReloadView {
        save: editor.saves,
        ..ReloadView::default()
    });
    editor.pending_reload = Some((path.clone(), def, previous));
    editor.reload_baseline = None;
    editor.toast = format!("saved {}", path.display());
    Ok(path)
}

/// A snapshot edit of the document on the shared undo stack. It belongs to the document it was made
/// in: once another graph is open it does nothing.
pub struct LogicEdit {
    label: String,
    merge_key: Option<String>,
    session: u64,
    before: LogicGraphDoc,
    after: LogicGraphDoc,
    selected_before: Option<NodeId>,
    selected_after: Option<NodeId>,
}

impl LogicEdit {
    fn set(world: &mut World, session: u64, doc: &LogicGraphDoc, selected: Option<NodeId>) {
        let mut editor = world.get_resource_or_insert_with(LogicEditor::default);
        if editor.session != session {
            editor.toast = "that undo step belongs to a graph that is no longer open".into();
            return;
        }
        editor.doc = doc.clone();
        editor.revision += 1;
        editor.selected = selected.filter(|s| doc.nodes.contains_key(s));
    }
}

impl EditorCommand for LogicEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.after, self.selected_after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.before, self.selected_before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<LogicEdit>() else {
            return false;
        };
        if self.merge_key.is_some() && self.merge_key == next.merge_key && self.session == next.session {
            self.after = next.after.clone();
            self.selected_after = next.selected_after;
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("Logic graph: {}", self.label)
    }
}

/// Applies `ops` to a copy of the document as one undo step (merged with the previous step when
/// both carry the same `merge_key`, e.g. dragging one node). A node the ops add becomes selected.
/// On error, or when nothing changed, the document and the stack stay. Returns the added node ids.
///
/// # Errors
///
/// The first operation that does not fit the graph: its position, code and message.
pub fn edit(
    world: &mut World,
    label: &str,
    merge_key: Option<String>,
    ops: &[LogicOp],
) -> Result<Vec<NodeId>, LogicBatchError> {
    let (before, session, selected_before) = {
        let editor = world.get_resource_or_insert_with(LogicEditor::default);
        (editor.doc.clone(), editor.session, editor.selected)
    };
    let mut after = before.clone();
    let created = logic_edit::apply_ops(&mut after, ops)?;
    if after == before {
        return Ok(created);
    }
    let selected_after = created
        .last()
        .copied()
        .or(selected_before)
        .filter(|s| after.nodes.contains_key(s));
    let mut command = LogicEdit {
        label: label.to_string(),
        merge_key,
        session,
        before,
        after,
        selected_before,
        selected_after,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(command));
    Ok(created)
}

/// Editor-loop hook, once per frame: while the game plays, delivers a pending save to the play
/// world, lists its graph entities and copies the watched one's activity and variables into
/// [`LogicEditor::live`]; with no play session it clears them. Does nothing until the editor
/// resource exists (the panel or `editor.logic_open` was used).
pub fn pump(edit: &mut World, session: Option<&mut PieSession>) {
    pump_world(edit, session.map(|s| s.app.world_mut()));
}

/// [`pump`] against any play world (the tests drive one directly).
pub fn pump_world(edit: &mut World, play: Option<&mut World>) {
    if !edit.contains_resource::<LogicEditor>() {
        return;
    }
    let root = crate::agent_ext::agent_root(edit);
    let Some(mut editor) = edit.remove_resource::<LogicEditor>() else {
        return;
    };
    editor.breakpoints.ensure_loaded(&root);
    match play {
        Some(play) => {
            let _span = tracing::debug_span!(target: "regia_editor", "logic_editor_pump").entered();
            pump_play(&mut editor, play, &root);
        }
        None => {
            editor.agents.clear();
            editor.live = None;
            editor.step_fire = None;
            editor.pulse.clear();
            editor.breakpoints.hits.clear();
            editor.breakpoints.applied.clear();
            if editor.watches.snapshot.take().is_some() {
                editor.watches.recompute(&editor.doc);
            }
        }
    }
    edit.insert_resource(editor);
}

/// The open graph's key in the breakpoint file (`None`: not saved anywhere yet).
#[must_use]
pub fn breakpoint_key(editor: &LogicEditor, root: &Path) -> Option<String> {
    editor.path.as_deref().map(|p| breakpoints::graph_key(root, p))
}

/// Applies one breakpoint change to the open graph (the gutter dot, F9, the event menu, the
/// Breakpoints list and `editor.logic_breakpoint_set` all come here). Returns whether the
/// breakpoint exists afterwards.
///
/// # Errors
///
/// `no_graph_open`, `no_graph_path` (save the graph first), `rule_not_found`, `bad_value` (an
/// empty event name), `not_found`, `save_failed`.
pub fn set_breakpoint(
    world: &mut World,
    kind: breakpoints::BreakpointKind,
    name: &str,
    change: breakpoints::BreakpointChange,
) -> Result<bool, breakpoints::BreakpointError> {
    use breakpoints::{BreakpointError, BreakpointKind};
    let root = crate::agent_ext::agent_root(world);
    let mut editor = world
        .get_resource_mut::<LogicEditor>()
        .filter(|e| e.is_open())
        .ok_or_else(|| BreakpointError {
            code: "no_graph_open",
            message: "no logic graph is open".into(),
        })?;
    let key = breakpoint_key(&editor, &root).ok_or_else(|| BreakpointError {
        code: "no_graph_path",
        message: "breakpoints belong to a graph file: save the graph first".into(),
    })?;
    editor.breakpoints.ensure_loaded(&root);
    let adding = matches!(
        change,
        breakpoints::BreakpointChange::Toggle | breakpoints::BreakpointChange::Enable(true)
    );
    let listed = editor.breakpoints.of(&key).get(kind, name).is_some();
    match kind {
        // A breakpoint still listed for a rule since renamed can be removed or disabled.
        BreakpointKind::Rule if adding && !listed && editor.doc.find_rule(name).is_none() => {
            return Err(BreakpointError {
                code: "rule_not_found",
                message: format!("the graph has no rule named {name:?}"),
            });
        }
        BreakpointKind::Event if name.is_empty() => {
            return Err(BreakpointError {
                code: "bad_value",
                message: "an event name is empty".into(),
            });
        }
        _ => {}
    }
    let out = editor.breakpoints.change(&key, kind, name, change);
    editor.toast = match &out {
        Ok(true) => format!("breakpoint on {} {name}", kind.name()),
        Ok(false) => format!("removed the breakpoint on {} {name}", kind.name()),
        Err(e) => format!("breakpoint not changed: {} ({})", e.message, e.code),
    };
    out
}

fn reload_total(play: &World) -> u64 {
    play.get_resource::<LogicReloads>()
        .map_or(0, |r| r.applied + r.rejected)
}

/// Reads one trace line (`tick N: kind rest`) as its tick, kind and the rest.
fn trace_parts(line: &str) -> Option<(u64, &str, &str)> {
    let rest = line.strip_prefix("tick ")?;
    let (tick, rest) = rest.split_once(": ")?;
    let tick = tick.parse().ok()?;
    let (kind, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    Some((tick, kind, rest))
}

/// The activity of the newest traced tick (spec 102 § 9: the panel colors nodes by it).
fn activity(status: &LogicGraphStatus, live: &mut LiveView) {
    let newest = status
        .trace
        .iter()
        .filter_map(|l| trace_parts(l))
        .map(|(t, ..)| t)
        .max();
    live.activity_tick = newest;
    let Some(newest) = newest else {
        return;
    };
    for (tick, kind, rest) in status.trace.iter().filter_map(|l| trace_parts(l)) {
        if tick != newest {
            continue;
        }
        let rule = rest.split(' ').next().unwrap_or_default().to_string();
        match kind {
            "event_received" => live.events.push(rest.to_string()),
            "rule_rejected" => {
                live.activity.insert(rule, RuleActivity::Rejected);
            }
            "rule_matched" => {
                // A match during a cooldown or a busy chain stays `matched`; `action_started`
                // (below) upgrades a real fire.
                live.activity.entry(rule).or_insert(RuleActivity::Matched);
            }
            "action_started" => {
                let index = rest.split(' ').nth(1).and_then(|i| i.parse().ok()).unwrap_or(0);
                live.actions_started.insert(rule.clone(), index);
                if live.activity.get(&rule) != Some(&RuleActivity::Failed) {
                    live.activity.insert(rule, RuleActivity::Fired);
                }
            }
            "action_failed" => {
                live.activity.insert(rule, RuleActivity::Failed);
            }
            _ => {}
        }
    }
}

fn pump_play(editor: &mut LogicEditor, play: &mut World, root: &Path) {
    let current = editor.doc.to_def();
    // The graph entities, in id order.
    let mut rows: Vec<(u64, bool)> = {
        let mut query = play.query::<(Entity, &LogicGraph)>();
        query
            .iter(play)
            .map(|(e, g)| {
                (
                    e.to_bits(),
                    g.def() == &current || editor.saved.as_ref() == Some(g.def()),
                )
            })
            .collect()
    };
    rows.sort_unstable();
    editor.agents = rows.iter().map(|(e, _)| *e).collect();
    let watch = editor
        .watch
        .filter(|w| editor.agents.contains(w))
        .or_else(|| rows.iter().find(|(_, uses)| *uses).map(|(e, _)| *e))
        .or_else(|| editor.agents.first().copied());

    // A save waiting for this session: queue the new definition for every graph that ran the
    // definition it replaces (a graph running another file is never touched; a first save of a new
    // graph reloads nothing).
    if let Some((path, def, previous)) = editor.pending_reload.take() {
        let source = path.display().to_string();
        let targets: Vec<u64> = {
            let mut query = play.query::<(Entity, &LogicGraph)>();
            let mut t: Vec<u64> = query
                .iter(play)
                .filter(|(_, g)| previous.as_ref() == Some(g.def()))
                .map(|(e, _)| e.to_bits())
                .collect();
            t.sort_unstable();
            t
        };
        let baseline = reload_total(play);
        let empty = BtTaskRegistry::default();
        let mut reloads = play.remove_resource::<LogicReloads>().unwrap_or_default();
        for bits in &targets {
            let Some(entity) = Entity::try_from_bits(*bits) else {
                continue;
            };
            // `LogicGraph` owns its task instances, so each target gets its own compile.
            let registry = play.get_resource::<BtTaskRegistry>().unwrap_or(&empty);
            match LogicGraph::compile(&def, registry) {
                Ok(graph) => reloads.queue(entity, source.clone(), graph),
                Err(e) => reloads.reject(entity, source.clone(), e.to_string()),
            }
        }
        play.insert_resource(reloads);
        if let Some(view) = editor.reload.as_mut() {
            view.queued = targets.len() as u64;
        }
        editor.reload_baseline = Some((baseline, targets));
    }

    // A Step to the next fired rule ends once its graph paused (a rule fired) or left the world.
    if let Some(bits) = editor.step_fire
        && Entity::try_from_bits(bits)
            .and_then(|e| play.get::<LogicGraph>(e))
            .is_none_or(LogicGraph::is_paused)
    {
        editor.step_fire = None;
    }
    // The live view's pause / resume / step buttons act on the watched graph.
    if let Some(control) = editor.pending_control.take()
        && let Some(entity) = watch.and_then(Entity::try_from_bits)
        && let Some(mut graph) = play.get_mut::<LogicGraph>(entity)
    {
        match control {
            LiveControl::Pause => graph.pause(),
            LiveControl::Resume => graph.resume(),
            LiveControl::Step => graph.step(),
            LiveControl::StepToFire => {
                if cfg!(debug_assertions) {
                    graph.resume();
                    editor.step_fire = Some(entity.to_bits());
                } else {
                    editor.toast = "Step to the next fired rule needs a development build".into();
                }
            }
        }
    }
    // The breakpoints of the open file, applied to every graph that runs it (development builds).
    let key = breakpoint_key(editor, root);
    #[cfg(debug_assertions)]
    apply_breakpoints(editor, play, &rows, key.as_deref());
    #[cfg(not(debug_assertions))]
    let _ = (&rows, &key);

    let mut hits = Vec::new();
    editor.live = watch.and_then(|bits| {
        let entity = Entity::try_from_bits(bits)?;
        let graph = play.get::<LogicGraph>(entity)?;
        let status = play.get::<LogicGraphStatus>(entity).cloned().unwrap_or_else(|| {
            LogicGraphStatus::of_in(
                graph,
                play.get_resource::<regia_ai::logic::LogicWorldBlackboards>(),
            )
        });
        let mut live = LiveView {
            entity: bits,
            running: !graph.is_paused() && graph.is_started(),
            ticks: status.ticks,
            matches_document: graph.def() == &current,
            ..LiveView::default()
        };
        activity(&status, &mut live);
        hits = breakpoints::hits_of(&status.trace, live.activity_tick);
        live.fires = status.rules.iter().map(|r| (r.name.clone(), r.fires)).collect();
        live.locals = graph
            .locals()
            .values
            .iter()
            .map(|(k, v)| (k.clone(), format!("{v:?}")))
            .collect();
        // The declared world boards (spec 102 § 5, phase 3), as `<board>/<key>`.
        live.world_vars = status
            .world
            .iter()
            .map(|w| (w.name.clone(), format!("{:?}", w.value)))
            .collect();
        if let Some(bb) = play.get::<Blackboard>(entity) {
            live.entity_vars = bb
                .values
                .iter()
                .map(|(k, v)| (k.clone(), format!("{v:?}")))
                .collect();
        }
        Some(live)
    });
    editor.breakpoints.hits = hits;
    pump_watch(editor, play, watch);

    // The outcomes of the last delivered save: the reload records added since it was delivered that
    // name the saved file.
    if let (Some((baseline, targets)), Some(path), Some(view)) = (
        editor.reload_baseline.as_ref(),
        editor.path.clone(),
        editor.reload.as_mut(),
    ) && let Some(reloads) = play.get_resource::<LogicReloads>()
    {
        let source = path.display().to_string();
        let added = (reloads.applied + reloads.rejected).saturating_sub(*baseline);
        let log: Vec<_> = reloads.log().collect();
        let fresh = usize::try_from(added).unwrap_or(usize::MAX).min(log.len());
        let (mut kept, mut restarted, mut rejected) = (0, 0, 0);
        let mut reason = None;
        for record in &log[log.len() - fresh..] {
            if record.source != source || !targets.contains(&record.entity.to_bits()) {
                continue;
            }
            match &record.result {
                Ok(LogicReloadOutcome::Kept) => kept += 1,
                Ok(LogicReloadOutcome::Restarted) => restarted += 1,
                Err(why) => {
                    rejected += 1;
                    reason = Some(why.clone());
                }
            }
        }
        view.kept = kept;
        view.restarted = restarted;
        view.rejected = rejected;
        view.reason = reason;
    }
}

/// The breakpoint lists the editor wants on graph entity `bits` (`uses`: it runs the open file).
#[cfg(debug_assertions)]
fn wanted_lists(
    editor: &LogicEditor,
    graph: &LogicGraph,
    bits: u64,
    uses: bool,
    key: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    use breakpoints::BreakpointKind;
    let mut rules = Vec::new();
    let mut events = Vec::new();
    // The debug-instance filter (w13 P-181, ADR-8565): while the pick names a graph in the play
    // world, the open file's breakpoints go to that instance only.
    let picked = editor.watch.filter(|w| editor.agents.contains(w));
    let uses = uses && picked.is_none_or(|w| w == bits);
    if uses && let Some(set) = key.and_then(|key| editor.breakpoints.file.graphs.get(key)) {
        rules.extend(
            set.list(BreakpointKind::Rule)
                .iter()
                .filter(|b| b.enabled && graph.has_rule(&b.name))
                .map(|b| b.name.clone()),
        );
        events.extend(set.enabled(BreakpointKind::Event));
    }
    if editor.step_fire == Some(bits) {
        for (name, ..) in graph.rule_stats() {
            if !rules.iter().any(|r| r == name) {
                rules.push(name.to_string());
            }
        }
    }
    (rules, events)
}

/// Brings every graph's `LogicGraphDebug` in line with the editor's breakpoints: what the editor
/// added before is taken out, what it wants now goes in, and entries the agent's `logic.debug` set
/// stay. A graph whose wanted lists did not change is not touched.
#[cfg(debug_assertions)]
fn apply_breakpoints(editor: &mut LogicEditor, play: &mut World, rows: &[(u64, bool)], key: Option<&str>) {
    use regia_ai::logic::LogicGraphDebug;
    editor
        .breakpoints
        .applied
        .retain(|bits, _| rows.iter().any(|(b, _)| b == bits));
    for &(bits, uses) in rows {
        let Some(entity) = Entity::try_from_bits(bits) else {
            continue;
        };
        let Some(graph) = play.get::<LogicGraph>(entity) else {
            continue;
        };
        let wanted = wanted_lists(editor, graph, bits, uses, key);
        let before = editor.breakpoints.applied.get(&bits).cloned().unwrap_or_default();
        if wanted == before {
            continue;
        }
        let mut debug = play.get::<LogicGraphDebug>(entity).cloned().unwrap_or_default();
        debug.pause_on_rule.retain(|r| !before.0.contains(r));
        debug.pause_on_event.retain(|e| !before.1.contains(e));
        for r in &wanted.0 {
            if !debug.pause_on_rule.contains(r) {
                debug.pause_on_rule.push(r.clone());
            }
        }
        for e in &wanted.1 {
            if !debug.pause_on_event.contains(e) {
                debug.pause_on_event.push(e.clone());
            }
        }
        if let Ok(mut host) = play.get_entity_mut(entity) {
            host.insert(debug);
        }
        if wanted.0.is_empty() && wanted.1.is_empty() {
            editor.breakpoints.applied.remove(&bits);
        } else {
            editor.breakpoints.applied.insert(bits, wanted);
        }
    }
}

/// The watched graph's scope snapshot, Watch values and wire pulses, after the live view: the
/// snapshot is copied every pump with Live on, else only when the graph is paused on a tick it
/// does not hold yet; pulses run only while a graph is watched.
fn pump_watch(editor: &mut LogicEditor, play: &World, watch: Option<u64>) {
    let graph = watch
        .and_then(Entity::try_from_bits)
        .and_then(|e| Some((e, play.get::<LogicGraph>(e)?)));
    let Some((entity, graph)) = graph else {
        editor.pulse.clear();
        if editor.watches.snapshot.take().is_some() {
            editor.watches.recompute(&editor.doc);
        }
        return;
    };
    let held = editor.watches.snapshot.as_ref().is_some_and(|s| {
        s.entity == entity.to_bits() && s.ticks == graph.ticks() && s.paused == graph.is_paused()
    });
    let refresh = editor.watches.live || (graph.is_paused() && !held);
    if refresh {
        editor.watches.snapshot = Some(watch::ScopeSnapshot::of(graph, play, entity));
        editor.watches.refreshes += 1;
        editor.watches.recompute(&editor.doc);
    } else if editor
        .watches
        .snapshot
        .as_ref()
        .is_some_and(|s| s.entity != entity.to_bits() || (s.paused && !graph.is_paused()))
    {
        // A snapshot of another graph, or of a pause that ended, no longer answers.
        editor.watches.snapshot = None;
        editor.watches.recompute(&editor.doc);
    }
    let fired: Vec<&str> = editor
        .live
        .as_ref()
        .map(|l| {
            l.activity
                .iter()
                .filter(|(_, a)| **a == RuleActivity::Fired)
                .map(|(r, _)| r.as_str())
                .collect()
        })
        .unwrap_or_default();
    let tick = editor.live.as_ref().and_then(|l| l.activity_tick);
    editor.pulse.frame(&editor.doc, &fired, tick);
}

/// The open graph's issues for the Problems panel, computed once per document revision.
pub(crate) fn cached_issues(world: &mut World) -> Option<(Option<PathBuf>, std::sync::Arc<Vec<LogicIssue>>)> {
    let (revision, cached) = {
        let editor = world.get_resource::<LogicEditor>().filter(|e| e.is_open())?;
        (
            editor.revision,
            editor.problems.clone().filter(|(rev, _)| *rev == editor.revision),
        )
    };
    let issues = match cached {
        Some((_, issues)) => issues,
        None => {
            let issues = std::sync::Arc::new(diagnose(world));
            world.resource_mut::<LogicEditor>().problems = Some((revision, issues.clone()));
            issues
        }
    };
    Some((world.resource::<LogicEditor>().path.clone(), issues))
}

#[cfg(test)]
mod debugger_tests;
#[cfg(test)]
mod keys_tests;
#[cfg(test)]
mod tests;
