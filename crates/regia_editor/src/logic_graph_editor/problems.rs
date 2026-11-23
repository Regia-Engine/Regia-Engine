// SPDX-License-Identifier: MPL-2.0
//! The open logic graph's issues in the editor-wide Problems panel (ADR-8021): one row per issue
//! with the graph file, the node and the code, whose link opens the Logic Graph panel at the node
//! (selected and framed on the canvas). `editor.problems_panel` answers the same rows as
//! `logic_graph`, and `editor.logic_goto_node` follows a link.

use regia_ai::logic_edit::{LogicSeverity, NodeId};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::LogicEditor;

/// One logic graph row of the Problems panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicProblemRow {
    /// The graph file (`(unsaved graph)` before its first save).
    pub path: String,
    /// The node the issue is about.
    pub node: Option<NodeId>,
    /// `error` or `warning`.
    pub severity: &'static str,
    /// The issue code (spec 102 § 6).
    pub code: &'static str,
    /// The text.
    pub message: String,
}

impl LogicProblemRow {
    /// The row as the Problems panel shows it.
    #[must_use]
    pub fn line(&self) -> String {
        match self.node {
            Some(n) => format!(
                "{} {} {} node #{n}: {}",
                self.severity, self.code, self.path, self.message
            ),
            None => format!("{} {} {}: {}", self.severity, self.code, self.path, self.message),
        }
    }
}

/// The open graph's issues as Problems rows (empty with no graph open). Validation runs once per
/// document revision, shared by every frame that asks.
pub fn rows(world: &mut World) -> Vec<LogicProblemRow> {
    let Some((path, issues)) = super::cached_issues(world) else {
        return Vec::new();
    };
    let path = path.map_or_else(|| "(unsaved graph)".to_string(), |p| p.display().to_string());
    issues
        .iter()
        .map(|i| LogicProblemRow {
            path: path.clone(),
            node: i.node,
            severity: if i.severity == LogicSeverity::Error {
                "error"
            } else {
                "warning"
            },
            code: i.code,
            message: i.message.clone(),
        })
        .collect()
}

/// The rows as `editor.problems_panel` answers them (`logic_graph`).
pub fn rows_json(world: &mut World) -> Value {
    json!(
        rows(world)
            .iter()
            .map(|r| json!({
                "path": r.path,
                "node": r.node,
                "severity": r.severity,
                "code": r.code,
                "message": r.message,
            }))
            .collect::<Vec<_>>()
    )
}

/// Follows a Problems link: brings the Logic Graph panel forward, selects `node` and frames it on
/// the canvas.
///
/// # Errors
///
/// `no_graph_open`, `unknown_node`.
pub fn go_to_node(world: &mut World, node: NodeId) -> Result<(), (&'static str, String)> {
    {
        let mut editor = world
            .get_resource_mut::<LogicEditor>()
            .filter(|e| e.is_open())
            .ok_or(("no_graph_open", "no logic graph is open".to_string()))?;
        if !editor.doc.nodes.contains_key(&node) {
            return Err(("unknown_node", format!("no node {node}")));
        }
        editor.selected = Some(node);
        editor.ui.frame_node(node);
        editor.toast = format!("node #{node}");
    }
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::LogicGraph),
    );
    Ok(())
}

/// The logic graph section of the Problems panel (drawn under the asset findings; nothing with no
/// graph open). A row is a focusable button: Enter or a click opens the graph at the node.
pub fn problems_section(ui: &mut egui::Ui, world: &mut World) {
    let rows = rows(world);
    if rows.is_empty() {
        return;
    }
    let tokens = crate::theme::tokens(world).clone();
    ui.separator();
    ui.strong(format!("Logic graph ({})", rows.len()));
    let mut picked = None;
    for row in &rows {
        let color = if row.severity == "error" {
            tokens.error
        } else {
            tokens.warn
        };
        let response = ui.add(egui::Button::new(egui::RichText::new(row.line()).color(color)).frame(false));
        let response = match row.node {
            Some(_) => response.on_hover_text("Open the graph at this node"),
            None => response,
        };
        if response.clicked()
            && let Some(node) = row.node
        {
            picked = Some(node);
        }
    }
    if let Some(node) = picked
        && let Err((code, message)) = go_to_node(world, node)
    {
        world.get_resource_or_insert_with(LogicEditor::default).toast = format!("{message} ({code})");
    }
}
