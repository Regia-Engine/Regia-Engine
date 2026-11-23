// SPDX-License-Identifier: MPL-2.0
//! Logic for the open prefab (v0.6.0 parity row P-165, ADR-8566): in prefab edit mode the banner's
//! **Open graph** opens the `.logic.ron` the prefab's [`LogicGraphRef`](regia_ai::logic::LogicGraphRef) names in the Logic Graph
//! panel, and **New graph for this prefab** writes an empty `.logic.ron` next to the prefab file
//! (`door.prefab.ron` → `door.logic.ron`, then `door_2.logic.ron`, … while the name is taken),
//! adds a [`LogicGraphRef`](regia_ai::logic::LogicGraphRef) naming it to the prefab's root as one undo step, and opens it. Undo
//! takes the component off; the file stays (as Make unique leaves its copy). At runtime
//! `regia_ai::AiPlugin` compiles the reference into the entity's graph, so every instance runs it.
//!
//! The agent route is `editor.prefab_logic { op: "open" | "new" }` (`book/agents/prefab-logic.md`).

use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::logic::{LogicGraphDef, LogicGraphRef, LogicLocalOverride, LogicOverrideValue};
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;
use serde_json::{Value, json};

use crate::command::{AddComponent, EditorCommand, UndoStack};
use crate::data_panels::PanelError;
use crate::logic_graph_editor::LogicEditor;
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/prefab-logic.md";

/// What the banner's prefab-logic actions do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefabLogicOp {
    /// Open the graph the prefab references.
    Open,
    /// Create a graph next to the prefab, reference it from the root and open it.
    New,
}

impl PrefabLogicOp {
    /// The agent spelling (`open`, `new`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::New => "new",
        }
    }
}

/// What a prefab-logic action did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefabLogic {
    /// The action.
    pub op: PrefabLogicOp,
    /// The graph, project-relative (what the component holds).
    pub path: String,
    /// The graph file on disk.
    pub file: PathBuf,
    /// The entity that carries the reference (bits).
    pub entity: u64,
    /// A new file was written and the component added (`new`).
    pub created: bool,
    /// The open Logic Graph document had unsaved work that this replaced (`discard: true`).
    pub discarded_unsaved: bool,
    /// Undo entries after the action.
    pub undoable: usize,
}

/// Registers the component's types in the edit world (the editor's edit world runs no
/// `regia_ai::AiPlugin`, so they are not there unless the host added them).
fn ensure_registered(world: &mut World) {
    let registry = world
        .get_resource_or_insert_with(AppTypeRegistry::default)
        .clone();
    let mut registry = registry.write();
    if registry.get(std::any::TypeId::of::<LogicGraphRef>()).is_none() {
        registry.register::<LogicGraphRef>();
        registry.register::<LogicLocalOverride>();
        registry.register::<LogicOverrideValue>();
    }
}

/// `door.prefab.ron` → `door`; any other name loses its last extension.
fn prefab_stem(file: &str) -> &str {
    file.strip_suffix(".prefab.ron")
        .or_else(|| file.rsplit_once('.').map(|(stem, _)| stem))
        .unwrap_or(file)
}

/// The project-relative path of a new graph next to the prefab `rel` (project-relative, `/`
/// separators): `<stem>.logic.ron`, then `<stem>_2.logic.ron`, … the first name `exists` says is
/// free.
#[must_use]
pub fn new_graph_path(rel: &str, exists: impl Fn(&str) -> bool) -> String {
    let (folder, file) = rel.rsplit_once('/').map_or(("", rel), |(f, n)| (f, n));
    let stem = prefab_stem(file);
    let prefix = if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    };
    (1..)
        .map(|n| {
            if n == 1 {
                format!("{prefix}{stem}.logic.ron")
            } else {
                format!("{prefix}{stem}_{n}.logic.ron")
            }
        })
        .find(|candidate| !exists(candidate))
        .unwrap_or_default()
}

/// `path` under `root` as a project-relative path with `/` separators.
fn project_rel(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// The open prefab's file, the project root, and the prefab's root entity.
fn session(world: &mut World) -> Result<(PathBuf, PathBuf, Entity), PanelError> {
    let prefab = world
        .get_resource::<crate::prefab_edit::PrefabEditMode>()
        .and_then(|mode| mode.editing().map(Path::to_path_buf))
        .ok_or_else(|| {
            PanelError::new(
                "prefab_logic_not_editing",
                "no prefab is open in prefab edit mode",
            )
        })?;
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| PanelError::new("prefab_logic_no_project", "no project is open"))?;
    let entity = crate::prefab_edit::prefab_root(world).ok_or_else(|| {
        PanelError::new(
            "prefab_logic_no_root",
            "the prefab has no root entity to hold its graph",
        )
    })?;
    Ok((prefab, root, entity))
}

/// The prefab member whose [`LogicGraphRef`] **Open graph** opens: the root's, else the first
/// member (in the order a save writes them) that has one.
fn referencing_member(world: &mut World, root: Entity) -> Option<(Entity, String)> {
    if let Some(r) = world.get::<LogicGraphRef>(root) {
        return Some((root, r.graph.clone()));
    }
    let members = crate::prefab_edit::prefab_members(world);
    members
        .into_iter()
        .find_map(|e| world.get::<LogicGraphRef>(e).map(|r| (e, r.graph.clone())))
}

/// Opens `file` in the Logic Graph panel (refusing to drop unsaved work unless `discard`).
fn open_graph(world: &mut World, file: &Path, discard: bool) -> Result<bool, PanelError> {
    let unsaved = world
        .get_resource::<LogicEditor>()
        .is_some_and(LogicEditor::has_unsaved_work);
    if unsaved && !discard {
        return Err(PanelError::new(
            "prefab_logic_unsaved",
            "the Logic Graph panel has unsaved changes: save them first (or pass discard: true)",
        ));
    }
    crate::logic_graph_editor::open_in_panel(world, file)
        .map_err(|e| PanelError::new("prefab_logic_open_failed", e))?;
    Ok(unsaved)
}

/// Runs `op` on the open prefab (see the module docs).
///
/// # Errors
///
/// `prefab_logic_not_editing`, `prefab_logic_no_project`, `prefab_logic_outside_project`,
/// `prefab_logic_no_root`, `prefab_logic_none` (open: no member references a graph),
/// `prefab_logic_exists` (new: the root already references one), `prefab_logic_unsaved`,
/// `prefab_logic_io` or `prefab_logic_open_failed`.
pub fn prefab_logic(world: &mut World, op: PrefabLogicOp, discard: bool) -> Result<PrefabLogic, PanelError> {
    let (prefab, root, entity) = session(world)?;
    match op {
        PrefabLogicOp::Open => {
            let (holder, path) = referencing_member(world, entity).ok_or_else(|| {
                PanelError::new(
                    "prefab_logic_none",
                    "no entity of the prefab references a logic graph: use New graph for this prefab",
                )
            })?;
            let reference = LogicGraphRef::new(path.clone());
            let file = reference
                .file(&root)
                .map_err(|e| PanelError::new("prefab_logic_open_failed", e.to_string()))?;
            let discarded_unsaved = open_graph(world, &file, discard)?;
            Ok(PrefabLogic {
                op,
                path,
                file,
                entity: holder.to_bits(),
                created: false,
                discarded_unsaved,
                undoable: world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
            })
        }
        PrefabLogicOp::New => {
            if let Some(r) = world.get::<LogicGraphRef>(entity) {
                return Err(PanelError::new(
                    "prefab_logic_exists",
                    format!("the prefab's root already references {}: use Open graph", r.graph),
                ));
            }
            let rel_prefab = project_rel(&root, &prefab).ok_or_else(|| {
                PanelError::new(
                    "prefab_logic_outside_project",
                    format!("{} is not inside the project", prefab.display()),
                )
            })?;
            let unsaved = world
                .get_resource::<LogicEditor>()
                .is_some_and(LogicEditor::has_unsaved_work);
            if unsaved && !discard {
                return Err(PanelError::new(
                    "prefab_logic_unsaved",
                    "the Logic Graph panel has unsaved changes: save them first (or pass discard: true)",
                ));
            }
            let path = new_graph_path(&rel_prefab, |candidate| root.join(candidate).exists());
            let file = root.join(&path);
            let text = LogicGraphDef::new()
                .to_ron()
                .map_err(|e| PanelError::new("prefab_logic_io", e.to_string()))?;
            crate::atomic_write::write_atomic(&file, text.as_bytes())
                .map_err(|e| PanelError::new("prefab_logic_io", e))?;
            ensure_registered(world);
            // One undo step: the component on the root (the file stays on undo).
            let mut command = AddComponent::capture(
                world,
                entity,
                "LogicGraphRef",
                Box::new(LogicGraphRef::new(path.clone())),
            )
            .map_err(|e| {
                let _ = std::fs::remove_file(&file);
                PanelError::new("prefab_logic_io", e.to_string())
            })?;
            command.apply(world);
            world
                .get_resource_or_insert_with(UndoStack::new)
                .push(Box::new(command));
            crate::content_index::note_changed(world, std::slice::from_ref(&file));
            let discarded_unsaved = open_graph(world, &file, true)?;
            if let Some(mut mode) = world.get_resource_mut::<crate::prefab_edit::PrefabEditMode>() {
                mode.toast = format!("new logic graph {path}");
            }
            Ok(PrefabLogic {
                op,
                path,
                file,
                entity: entity.to_bits(),
                created: true,
                discarded_unsaved,
                undoable: world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
            })
        }
    }
}

/// The prefab banner's logic buttons (Open graph, New graph for this prefab). Returns the status
/// line of a click. A click is handled once per frame (egui may run the banner twice).
pub fn banner_buttons(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let frame = ui.ctx().cumulative_frame_nr();
    let mut clicked = None;
    if crate::agent_parity::action_button(ui, &crate::agent_parity::PREFAB_LOGIC_NEW)
        .on_hover_text(
            "Create a logic graph next to this prefab and attach it to the prefab's root (one undo step)",
        )
        .clicked()
    {
        clicked = Some(PrefabLogicOp::New);
    }
    if crate::agent_parity::action_button(ui, &crate::agent_parity::PREFAB_LOGIC_OPEN)
        .on_hover_text("Open the logic graph this prefab references in the Logic Graph panel")
        .clicked()
    {
        clicked = Some(PrefabLogicOp::Open);
    }
    let op = clicked?;
    {
        let mut guard = world.get_resource_or_insert_with(PrefabLogicUi::default);
        if guard.handled_frame == Some(frame) {
            return None;
        }
        guard.handled_frame = Some(frame);
    }
    Some(match prefab_logic(world, op, false) {
        Ok(done) if done.created => format!("new logic graph {} on the prefab's root", done.path),
        Ok(done) => format!("opened {}", done.path),
        Err(e) => e.message,
    })
}

/// The frame the banner last handled a prefab-logic click in.
#[derive(Resource, Debug, Default)]
pub struct PrefabLogicUi {
    handled_frame: Option<u64>,
}

/// `editor.prefab_logic`.
fn prefab_logic_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = match params.get("op").and_then(Value::as_str) {
        Some("open") => PrefabLogicOp::Open,
        Some("new") => PrefabLogicOp::New,
        other => {
            return Err(PanelError::new(
                "prefab_logic_bad_op",
                format!("`op` must be \"open\" or \"new\", not {other:?}"),
            )
            .rpc(HINT));
        }
    };
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    let done = prefab_logic(world, op, discard).map_err(|e| e.rpc(HINT))?;
    Ok(json!({
        "op": done.op.name(),
        "path": done.path,
        "file": done.file.display().to_string(),
        "entity": done.entity,
        "created": done.created,
        "discarded_unsaved": done.discarded_unsaved,
        "undoable": done.undoable,
    }))
}

const PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "op",
        kind: "string",
        required: true,
        description: "\"open\": open the graph the prefab references; \"new\": create one next to the prefab, \
            reference it from the root (one undo step) and open it.",
    },
    ParamSchema {
        name: "discard",
        kind: "boolean",
        required: false,
        description: "Replace unsaved work in the Logic Graph panel (default false: refused).",
    },
];

/// The agent methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.prefab_logic",
            tool: "prefab_logic",
            description: "Prefab edit mode's logic actions (P-165): `open` opens the .logic.ron the prefab's \
                LogicGraphRef names in the Logic Graph panel; `new` writes an empty .logic.ron next to the prefab \
                (<stem>.logic.ron, then <stem>_2.logic.ron, …), adds a LogicGraphRef naming it to the prefab's root \
                as one undo step (the file stays on undo) and opens it.",
            params: PARAMS,
            result: "{ op, path, file, entity, created, discarded_unsaved, undoable }",
            mutating: true,
            permission: Permission::EditScene,
        },
        prefab_logic_method as Handler,
    )]
}

#[cfg(test)]
mod tests;
