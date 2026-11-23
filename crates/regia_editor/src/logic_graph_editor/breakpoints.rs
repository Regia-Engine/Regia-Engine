// SPDX-License-Identifier: MPL-2.0
//! Logic graph breakpoints in the panel (ADR-8021, spec 102 § 9): rule breakpoints (the gutter
//! dot on a rule's event node, F9 on the selected rule) and event breakpoints (an event node's
//! menu, for the arms that arrive through the inbox), listed in the Breakpoints list where each can
//! be enabled, disabled, removed or gone to.
//!
//! Breakpoints belong to the user, not the asset: they persist per project in
//! `.regia/logic_breakpoints.ron` (keyed by the graph's project-relative path) and never touch the
//! `.logic.ron`. The pump applies the enabled ones to every play-world graph that runs the open
//! file through its `LogicGraphDebug` component, next to the entries the agent's `logic.debug` set
//! (the editor removes only what it added). `LogicGraphDebug` exists in development builds only,
//! so a shipping build lists and saves breakpoints but never applies them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ai::logic::LogicEvent;
use regia_ai::logic_edit::{LogicGraphDoc, LogicNodeKind, NodeId};
use serde::{Deserialize, Serialize};

/// The per-user breakpoint file, relative to the project root.
pub const BREAKPOINTS_FILE: &str = ".regia/logic_breakpoints.ron";

/// The version [`BreakpointFile`] writes.
pub const BREAKPOINTS_VERSION: u32 = 1;

/// What a breakpoint stops on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakpointKind {
    /// A rule: the graph pauses at the end of a tick in which the rule fires.
    Rule,
    /// An inbox event: the graph pauses at the end of the tick the event arrives in.
    Event,
}

impl BreakpointKind {
    /// `rule` or `event`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Event => "event",
        }
    }
}

/// One breakpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breakpoint {
    /// The rule name or inbox event name.
    pub name: String,
    /// Applied while playing (a disabled one stays in the list).
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

/// The breakpoints of one graph file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphBreakpoints {
    /// Rule breakpoints, in the order they were set.
    #[serde(default)]
    pub rules: Vec<Breakpoint>,
    /// Event breakpoints, in the order they were set.
    #[serde(default)]
    pub events: Vec<Breakpoint>,
}

impl GraphBreakpoints {
    /// The list of `kind`.
    #[must_use]
    pub fn list(&self, kind: BreakpointKind) -> &[Breakpoint] {
        match kind {
            BreakpointKind::Rule => &self.rules,
            BreakpointKind::Event => &self.events,
        }
    }

    fn list_mut(&mut self, kind: BreakpointKind) -> &mut Vec<Breakpoint> {
        match kind {
            BreakpointKind::Rule => &mut self.rules,
            BreakpointKind::Event => &mut self.events,
        }
    }

    /// The enabled names of `kind`.
    #[must_use]
    pub fn enabled(&self, kind: BreakpointKind) -> Vec<String> {
        self.list(kind)
            .iter()
            .filter(|b| b.enabled)
            .map(|b| b.name.clone())
            .collect()
    }

    /// Whether `name` has a breakpoint of `kind` (enabled or not).
    #[must_use]
    pub fn get(&self, kind: BreakpointKind, name: &str) -> Option<&Breakpoint> {
        self.list(kind).iter().find(|b| b.name == name)
    }

    fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.events.is_empty()
    }
}

/// `.regia/logic_breakpoints.ron`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakpointFile {
    /// [`BREAKPOINTS_VERSION`].
    pub format_version: u32,
    /// Graph path (project-relative, `/` separators) → its breakpoints.
    #[serde(default)]
    pub graphs: BTreeMap<String, GraphBreakpoints>,
}

impl Default for BreakpointFile {
    fn default() -> Self {
        Self {
            format_version: BREAKPOINTS_VERSION,
            graphs: BTreeMap::new(),
        }
    }
}

/// A breakpoint change (the panel's controls and `editor.logic_breakpoint_set` share it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakpointChange {
    /// Add it when missing, remove it when present (the gutter dot, F9).
    Toggle,
    /// Add it (enabled) or enable / disable it.
    Enable(bool),
    /// Remove it.
    Remove,
}

/// A refused breakpoint change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointError {
    /// `no_graph_open`, `no_graph_path`, `rule_not_found`, `bad_value`, `not_found` or
    /// `save_failed`.
    pub code: &'static str,
    /// A sentence for people.
    pub message: String,
}

impl BreakpointError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The editor's breakpoint state (inside [`super::LogicEditor`]).
#[derive(Debug, Clone, Default)]
pub struct Breakpoints {
    /// Every graph's breakpoints, as in the file.
    pub file: BreakpointFile,
    /// The file `file` was read from (`None`: not read yet).
    loaded_from: Option<PathBuf>,
    /// Why the file could not be read or written, if it could not.
    pub error: Option<String>,
    /// The file exists but could not be read (unreadable, malformed or another format version):
    /// it is never overwritten (review 3616 L3).
    keep_file: bool,
    /// Bumped by every change.
    pub revision: u64,
    /// What the editor added to each play-world graph's debug lists (entity bits → rules, events).
    pub(crate) applied: BTreeMap<u64, (Vec<String>, Vec<String>)>,
    /// The breakpoints that paused the watched graph on its newest traced tick (`rule <name>` or
    /// `event <name>`).
    pub hits: Vec<String>,
}

impl Breakpoints {
    /// Reads the file under `root` once (again when the project root changes). A missing file is
    /// an empty list; an unreadable one is an empty list and [`Self::error`].
    pub fn ensure_loaded(&mut self, root: &Path) {
        let path = root.join(BREAKPOINTS_FILE);
        if self.loaded_from.as_deref() == Some(path.as_path()) {
            return;
        }
        self.file = BreakpointFile::default();
        self.error = None;
        self.keep_file = false;
        match std::fs::read_to_string(&path) {
            Ok(text) => match ron::from_str::<BreakpointFile>(&text) {
                Ok(file) if file.format_version == BREAKPOINTS_VERSION => self.file = file,
                Ok(file) => {
                    self.error = Some(format!(
                        "{} has format_version {} (this editor reads {BREAKPOINTS_VERSION}); it is ignored",
                        path.display(),
                        file.format_version
                    ));
                }
                Err(e) => self.error = Some(format!("cannot read {}: {e}", path.display())),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => self.error = Some(format!("cannot read {}: {e}", path.display())),
        }
        self.keep_file = self.error.is_some();
        self.loaded_from = Some(path);
        self.revision += 1;
    }

    /// The breakpoints of graph `key` (empty when it has none).
    #[must_use]
    pub fn of(&self, key: &str) -> GraphBreakpoints {
        self.file.graphs.get(key).cloned().unwrap_or_default()
    }

    /// Applies `change` to the breakpoint `kind`/`name` of graph `key` and writes the file
    /// atomically. Returns whether the breakpoint exists afterwards.
    ///
    /// # Errors
    ///
    /// `not_found` (enable or remove a breakpoint that does not exist), `save_failed` (also while
    /// the file exists but could not be read: it is never overwritten).
    pub fn change(
        &mut self,
        key: &str,
        kind: BreakpointKind,
        name: &str,
        change: BreakpointChange,
    ) -> Result<bool, BreakpointError> {
        if self.keep_file {
            return Err(BreakpointError::new(
                "save_failed",
                format!(
                    "{}; fix or remove it first (it is not overwritten)",
                    self.error
                        .as_deref()
                        .unwrap_or("the breakpoint file could not be read")
                ),
            ));
        }
        let mut graph = self.of(key);
        let list = graph.list_mut(kind);
        let at = list.iter().position(|b| b.name == name);
        let exists = match (change, at) {
            (BreakpointChange::Toggle, Some(i)) | (BreakpointChange::Remove, Some(i)) => {
                list.remove(i);
                false
            }
            (BreakpointChange::Toggle, None) | (BreakpointChange::Enable(true), None) => {
                list.push(Breakpoint {
                    name: name.to_string(),
                    enabled: true,
                });
                true
            }
            (BreakpointChange::Enable(on), Some(i)) => {
                list[i].enabled = on;
                true
            }
            (BreakpointChange::Enable(false), None) | (BreakpointChange::Remove, None) => {
                return Err(BreakpointError::new(
                    "not_found",
                    format!("no {} breakpoint on {name:?}", kind.name()),
                ));
            }
        };
        if graph.is_empty() {
            self.file.graphs.remove(key);
        } else {
            self.file.graphs.insert(key.to_string(), graph);
        }
        self.revision += 1;
        self.save()?;
        Ok(exists)
    }

    /// Writes the file where it was read from (nothing when it was never read).
    fn save(&mut self) -> Result<(), BreakpointError> {
        let Some(path) = self.loaded_from.clone() else {
            return Ok(());
        };
        if self.keep_file {
            // A newer editor's file (or a damaged one) keeps every other graph's breakpoints.
            return Err(BreakpointError::new(
                "save_failed",
                format!(
                    "{} could not be read, so it is not overwritten: fix or delete it, then reopen the project",
                    path.display()
                ),
            ));
        }
        let text = ron::ser::to_string_pretty(&self.file, regia_core::ron_pretty())
            .map_err(|e| BreakpointError::new("save_failed", e.to_string()))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| BreakpointError::new("save_failed", format!("{}: {e}", dir.display())))?;
        }
        crate::atomic_write::write_atomic(&path, text.as_bytes())
            .map_err(|e| BreakpointError::new("save_failed", e))?;
        self.error = None;
        Ok(())
    }
}

/// The key of graph file `path` in the breakpoint file: project-relative with `/` separators, or the
/// whole path when it is outside the project.
#[must_use]
pub fn graph_key(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The inbox name an event node's arm arrives under (`None` for `Start`, `Timer` and
/// `BlackboardChanged`, which never come through the inbox, so they take no event breakpoint).
#[must_use]
pub fn event_breakpoint_name(event: &LogicEvent) -> Option<String> {
    event.inbox_name()
}

/// The rule a node belongs to (its own rule for an event node; the rule whose chain reaches it
/// otherwise).
#[must_use]
pub fn rule_of_node(doc: &LogicGraphDoc, node: NodeId) -> Option<String> {
    if let Some(LogicNodeKind::Event { rule, .. }) = doc.nodes.get(&node).map(|n| &n.kind) {
        return Some(rule.clone());
    }
    doc.chains().into_iter().find_map(|c| {
        (c.conditions.contains(&node) || c.actions.contains(&node))
            .then(|| doc.rule_name(c.event).map(str::to_string))
            .flatten()
    })
}

/// The breakpoints that paused a graph on its newest traced tick, from its trace lines
/// (`tick N: breakpoint rule <name>` / `tick N: breakpoint event <name>`).
#[must_use]
pub fn hits_of(trace: &[String], newest: Option<u64>) -> Vec<String> {
    let Some(newest) = newest else {
        return Vec::new();
    };
    trace
        .iter()
        .filter_map(|line| super::trace_parts(line))
        .filter(|(tick, kind, _)| *tick == newest && *kind == "breakpoint")
        .map(|(_, _, rest)| rest.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review 3616 L3: a file this editor cannot read (here a newer format version) is reported
    /// and never overwritten by the next breakpoint change.
    #[test]
    fn an_unreadable_breakpoint_file_is_never_overwritten() {
        let root = regia_test_utils::unique_temp_dir("logic-breakpoints-keep");
        let path = root.join(BREAKPOINTS_FILE);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("dir");
        }
        let newer = "(format_version: 2, graphs: {})";
        std::fs::write(&path, newer).expect("write");
        let mut bps = Breakpoints::default();
        bps.ensure_loaded(&root);
        assert!(
            bps.error
                .as_deref()
                .is_some_and(|e| e.contains("format_version 2")),
            "{:?}",
            bps.error
        );
        let err = bps
            .change(
                "logic/a.logic.ron",
                BreakpointKind::Rule,
                "r",
                BreakpointChange::Toggle,
            )
            .expect_err("refused");
        assert_eq!(err.code, "save_failed");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            newer,
            "the file is kept"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod keep_file_tests {
    use super::*;

    /// Review 3616 L3: a newer editor's breakpoint file is ignored, and a toggle in this editor
    /// must not write this session's list over it.
    #[test]
    fn an_unreadable_breakpoint_file_is_never_overwritten() {
        let root = regia_test_utils::unique_temp_dir("logic-breakpoints-keep");
        let path = root.join(BREAKPOINTS_FILE);
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        let newer = format!("(format_version: {}, graphs: {{}})", BREAKPOINTS_VERSION + 1);
        std::fs::write(&path, &newer).expect("write");
        let mut breakpoints = Breakpoints::default();
        breakpoints.ensure_loaded(&root);
        assert!(breakpoints.error.is_some(), "the newer file is reported");
        let err = breakpoints
            .change(
                "assets/a.logic.ron",
                BreakpointKind::Rule,
                "open",
                BreakpointChange::Toggle,
            )
            .expect_err("the save is refused");
        assert_eq!(err.code, "save_failed");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            newer,
            "the file is unchanged"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
