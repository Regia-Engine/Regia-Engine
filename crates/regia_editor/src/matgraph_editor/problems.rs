// SPDX-License-Identifier: MPL-2.0
//! The material graph editor's compile errors as a Problems panel source (w13, P-185, the
//! material part): every open document (the active tab, compiled live, and each parked tab, as it
//! was parked) lists its errors with the tab, the target (a node, a declaration, an output) and
//! the message; [`problems::open_problem`] activates the tab and selects the node, the link the Problems
//! panel and `editor.problems_panel { open_graph_error }` follow.
//!
//! A parked tab's diagnostics are cached by the document itself, so the per-frame cost is one
//! comparison per parked tab; the active tab reuses [`MatGraphEditor::diagnostics`].

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::model::{Diagnostics, MatGraphDoc, Target};
use super::{DocMode, MatGraphEditor};

/// One compile error of an open material graph document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphProblem {
    /// The document's tab index ([`MatGraphEditor::tabs`] order).
    pub tab: usize,
    /// The tab's label (file name, or `untitled graph`).
    pub label: String,
    /// The document's file, when saved.
    pub path: Option<PathBuf>,
    /// `graph`, `function` or `instance`.
    pub kind: &'static str,
    /// What the error is on: `graph`, `node:<name>`, `param:<name>`, `output:<slot>`, ….
    pub target: String,
    /// The node the link opens (`None`: the error is not on a node).
    pub node: Option<String>,
    /// What is wrong.
    pub message: String,
}

impl GraphProblem {
    /// The Problems panel line: `error <label> <target>: <message>`.
    #[must_use]
    pub fn line(&self) -> String {
        format!("error {} {}: {}", self.label, self.target, self.message)
    }

    /// As protocol JSON (`index` is its place in the list).
    #[must_use]
    pub fn to_json(&self, index: usize) -> Value {
        json!({
            "index": index,
            "tab": self.tab,
            "label": self.label,
            "path": self.path.as_ref().map(|p| p.display().to_string()),
            "kind": self.kind,
            "target": self.target,
            "node": self.node,
            "message": self.message,
        })
    }
}

/// The parked tabs' diagnostics, each with the document it was computed for.
#[derive(Default)]
pub struct ProblemsCache {
    parked: Vec<(MatGraphDoc, Option<PathBuf>, Diagnostics)>,
}

fn tab_dir(path: Option<&Path>, parent: Option<&Path>, doc: &MatGraphDoc) -> Option<PathBuf> {
    let own = path.and_then(Path::parent).map(Path::to_path_buf);
    match doc.mode {
        DocMode::Instance(_) => parent.and_then(Path::parent).map(Path::to_path_buf).or(own),
        _ => own,
    }
}

fn rows_of(
    out: &mut Vec<GraphProblem>,
    tab: usize,
    label: &str,
    path: Option<&Path>,
    doc: &MatGraphDoc,
    diagnostics: &Diagnostics,
) {
    for error in &diagnostics.errors {
        let node = match &error.target {
            Target::Node(name) => Some(name.clone()),
            _ => None,
        };
        out.push(GraphProblem {
            tab,
            label: label.to_string(),
            path: path.map(Path::to_path_buf),
            kind: super::doc_kind(doc),
            target: error.target.label(),
            node,
            message: error.message.clone(),
        });
    }
}

impl MatGraphEditor {
    /// The compile errors of every open document, in tab order (compiler order inside a tab).
    pub fn problems(&mut self) -> Vec<GraphProblem> {
        let _span = tracing::info_span!(target: "regia_editor", "matgraph_problems").entered();
        let root = self.root.clone();
        // Parked tabs: recomputed only when the parked document is not the one cached.
        let mut cache = std::mem::take(&mut self.problems_cache.parked);
        cache.truncate(self.parked.len());
        for (i, tab) in self.parked.iter().enumerate() {
            let dir = tab_dir(tab.path.as_deref(), tab.parent_path.as_deref(), &tab.doc);
            let fresh = cache
                .get(i)
                .is_some_and(|(doc, d, _)| *doc == tab.doc && *d == dir);
            if fresh {
                continue;
            }
            let context = tab.context.as_ref().map(|c| &c.graph);
            let diagnostics = super::model::diagnose_doc(&tab.doc, dir.as_deref(), root.as_deref(), context);
            let entry = (tab.doc.clone(), dir, diagnostics);
            if i < cache.len() {
                cache[i] = entry;
            } else {
                cache.push(entry);
            }
        }
        let tabs = self.tabs();
        let mut out = Vec::new();
        let active = self.is_open().then(|| self.active_tab.min(self.parked.len()));
        let mut parked = cache.iter().zip(&self.parked);
        for (index, info) in tabs.iter().enumerate() {
            if Some(index) == active {
                continue;
            }
            if let Some(((_, _, diagnostics), tab)) = parked.next() {
                rows_of(
                    &mut out,
                    index,
                    &info.label,
                    tab.path.as_deref(),
                    &tab.doc,
                    diagnostics,
                );
            }
        }
        if let Some(index) = active {
            let label = tabs.get(index).map(|t| t.label.clone()).unwrap_or_default();
            let path = self.path.clone();
            let diagnostics = self.diagnostics().clone();
            let mut rows = Vec::new();
            rows_of(&mut rows, index, &label, path.as_deref(), &self.doc, &diagnostics);
            let at = out.iter().position(|p| p.tab > index).unwrap_or(out.len());
            let tail = out.split_off(at);
            out.extend(rows);
            out.extend(tail);
        }
        self.problems_cache.parked = cache;
        out
    }
}

/// Follows a problem's link: activates its tab and selects its node (the inspector shows it).
/// Returns the selected node.
///
/// # Errors
///
/// The tab is gone, or the node no longer exists.
pub fn open_problem(editor: &mut MatGraphEditor, problem: &GraphProblem) -> Result<Option<String>, String> {
    editor.select_tab(problem.tab)?;
    if let Some(node) = &problem.node {
        if !editor.doc.graph.nodes.contains_key(node) && !matches!(editor.doc.mode, DocMode::Function(_)) {
            return Err(format!("node `{node}` is no longer in {}", problem.label));
        }
        editor.ui.selected = Some(node.clone());
    }
    Ok(problem.node.clone())
}

/// The world-level problems: the open editor's compile errors, or none without an editor.
pub fn world_problems(world: &mut regia_ecs::prelude::World) -> Vec<GraphProblem> {
    world
        .get_resource_mut::<MatGraphEditor>()
        .map(|mut editor| editor.problems())
        .unwrap_or_default()
}

/// Opens problem `index` of [`world_problems`]: its tab, its node, and the Material Graph panel.
///
/// # Errors
///
/// No such problem, or [`open_problem`] refused.
pub fn open_world_problem(
    world: &mut regia_ecs::prelude::World,
    index: usize,
) -> Result<GraphProblem, String> {
    let problems = world_problems(world);
    let problem = problems.get(index).cloned().ok_or_else(|| {
        format!(
            "there is no material graph error {index} ({} listed)",
            problems.len()
        )
    })?;
    let mut editor = world
        .get_resource_mut::<MatGraphEditor>()
        .ok_or_else(|| "the material graph editor is not open".to_string())?;
    open_problem(&mut editor, &problem)?;
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::MaterialGraph),
    );
    Ok(problem)
}
