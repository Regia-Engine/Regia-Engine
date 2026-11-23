// SPDX-License-Identifier: MPL-2.0
//! The Cameras panel (`book/agents/editor-gameplay-panels.md`, ADR-2030): the asset page of every
//! `.camera.ron` of the project (status, issues, parameters, variables, input slots, director), its
//! rig graph ([`crate::camera_graph`]), and the drivers of a running game with their active rigs,
//! blend, layers and variables.
//!
//! The assets on the page are the edit world's, in its own [`CameraAssets`] with no driver running.
//! The page loads files from the project; a changed file reloads by itself
//! ([`regia_framework::camera::load`], in the edit world and in the play world alike). Edits go to the
//! working copy: **Apply** validates it and makes it the active version, **Save** writes it to its
//! file, which is what reaches a running game. The state is in [`CameraPanel`], which the panel and
//! `editor.camera_panel_*` share.
//!
//! The working copy is edited through typed forms and structure edits ([`edit`], ADR-7734), each
//! one undo step of the asset's own history ([`history`]); `editor.camera_panel_edit` takes the same
//! edits as ops.

pub mod edit;
#[cfg(test)]
mod edit_tests;
mod form_ui;
pub mod history;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::camera::asset::{
    CameraAsset, CameraBuildStatus, CameraIssue, Node, ParamValue, Value as NodeValue, VarValue,
};
use regia_framework::camera::director::CameraHooks;
use regia_framework::camera::driver::{CameraAssets, CameraDriver};
use regia_framework::camera::load::{self, CameraFiles};
use serde_json::{Value, json};

use crate::camera_graph::{self, GraphLayout, NODE_H, RigGraph};
use crate::gameplay_panel::{
    CallRecord, LiveSource, PanelCall, entity_param, header, play_running, reason_error,
};
use crate::pie::PieSession;
use crate::viewport_agent::Handler;

/// Folders a scan walks at most, so a huge or looping tree cannot stall the editor.
const MAX_SCAN_DIRS: usize = 4096;

/// Which part of the page the panel shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CameraTab {
    /// The asset's status, issues, parameters, variables, slots.
    #[default]
    Asset,
    /// The rig graph.
    Graph,
    /// The drivers of the running game.
    Live,
}

/// One driver of the world being watched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverRow {
    /// Its entity.
    pub entity: Entity,
    /// Its display name.
    pub name: String,
    /// The asset it names.
    pub asset: String,
    /// Evaluating.
    pub enabled: bool,
    /// Its asset was valid and it runs.
    pub bound: bool,
    /// The primary rig.
    pub primary: Option<String>,
}

/// An asset of the play world, which may differ from the page's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayAssetRow {
    /// The asset's name.
    pub name: String,
    /// Its build status there.
    pub status: &'static str,
    /// Its active revision there.
    pub revision: u32,
}

/// The panel's model.
#[derive(Resource, Debug, Default)]
pub struct CameraPanel {
    /// The world the driver rows come from.
    pub source: LiveSource,
    /// The tab.
    pub tab: CameraTab,
    /// The asset on the page.
    pub selected_asset: Option<String>,
    /// The path field of the Load row (relative to `assets/`).
    pub load_path: String,
    /// The drivers, sorted by entity.
    pub drivers: Vec<DriverRow>,
    /// The driver the live view and the graph's colors follow.
    pub selected_driver: Option<Entity>,
    /// `camera.inspect` of the selected driver.
    pub live: Option<Value>,
    /// The play world's assets; empty when nothing plays or it has no camera framework.
    pub play_assets: Vec<PlayAssetRow>,
    /// The play world has a camera framework.
    pub play_framework: bool,
    /// The selected rig and node path in the graph, for the details under it.
    pub selected_node: Option<(usize, Vec<usize>)>,
    /// What the last call did.
    pub last_call: Option<CallRecord>,
    /// Snapshots of the play world taken so far.
    pub snapshots: u64,
    /// The undo history of each asset edited on the page, by asset name.
    pub docs: BTreeMap<String, history::CameraDoc>,
    /// The node kind the Add node buttons make (an index into [`edit::NODE_KINDS`]).
    pub add_kind: usize,
    /// The rename field: the rig it is for and the text typed.
    pub rename: Option<(String, String)>,
    /// The project root the first scan ran for.
    scanned: Option<PathBuf>,
    pending: Vec<PanelCall>,
    /// The egui pass whose Ctrl+S was handled (egui may run a panel twice in one frame).
    shortcut_pass: Option<u64>,
}

fn status_name(assets: &CameraAssets, name: &str) -> &'static str {
    assets.status(name).map_or("unknown", CameraBuildStatus::name)
}

fn issue_json(issue: &CameraIssue) -> Value {
    json!({
        "code": issue.code,
        "severity": issue.severity.name(),
        "rig": issue.rig,
        "node": issue.node,
        "message": issue.message,
    })
}

fn param_json(value: &ParamValue) -> Value {
    match value {
        ParamValue::Bool(v) => json!(v),
        ParamValue::Int(v) => json!(v),
        ParamValue::Float(v) => json!(v),
        ParamValue::Vec3(v) => json!(v),
        ParamValue::Name(v) => json!(v),
    }
}

fn var_json(value: &VarValue) -> Value {
    match value {
        VarValue::Bool(v) => json!(v),
        VarValue::Float(v) => json!(v),
        VarValue::Vec3(v) => json!(v),
        VarValue::YawPitch(yaw, pitch) => json!({ "yaw": yaw, "pitch": pitch }),
    }
}

/// A new default for a parameter, of the same type as the old one.
fn param_from_json(like: &ParamValue, value: &Value) -> Option<ParamValue> {
    match like {
        ParamValue::Bool(_) => value.as_bool().map(ParamValue::Bool),
        ParamValue::Int(_) => value.as_i64().map(ParamValue::Int),
        ParamValue::Float(_) => value
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| ParamValue::Float(v as f32)),
        ParamValue::Vec3(_) => {
            let items = value.as_array().filter(|a| a.len() == 3)?;
            let mut out = [0.0f32; 3];
            for (slot, item) in out.iter_mut().zip(items) {
                *slot = item.as_f64().filter(|v| v.is_finite())? as f32;
            }
            Some(ParamValue::Vec3(out))
        }
        ParamValue::Name(_) => value.as_str().map(|s| ParamValue::Name(s.to_owned())),
    }
}

fn asset_row_json(world: &World, name: &str) -> Value {
    let Some(assets) = world.get_resource::<CameraAssets>() else {
        return Value::Null;
    };
    let files = world.get_resource::<CameraFiles>();
    let path = assets.path(name);
    let file = path.and_then(|path| files.and_then(|files| files.get(path)));
    let working = assets.working(name);
    json!({
        "name": name,
        "path": path,
        "status": status_name(assets, name),
        "issue_count": assets.issues(name).map_or(0, <[CameraIssue]>::len),
        "active": assets.active(name).is_some(),
        "revision": assets.revision(name),
        "rigs": working.map(|a| a.rigs.iter().map(|r| r.name.clone()).collect::<Vec<_>>()).unwrap_or_default(),
        "reads": file.map_or(0, |file| file.generation),
        "cooked": file.is_some_and(|file| file.cooked),
        "file_error": file
            .and_then(|file| file.error.as_ref())
            .map(|e| json!({ "reason": e.reason, "detail": e.detail })),
    })
}

fn page_json(world: &World, name: &str) -> Value {
    let Some(assets) = world.get_resource::<CameraAssets>() else {
        return Value::Null;
    };
    let Some(asset) = assets.working(name) else {
        return Value::Null;
    };
    json!({
        "row": asset_row_json(world, name),
        "issues": assets.issues(name).unwrap_or(&[]).iter().map(issue_json).collect::<Vec<_>>(),
        "params": asset.params.iter().map(|(key, value)| json!({
            "name": key,
            "type": value.kind(),
            "value": param_json(value),
        })).collect::<Vec<_>>(),
        "variables": asset.variables.iter().map(|var| json!({
            "name": var.name,
            "type": var.default.kind(),
            "default": var_json(&var.default),
            "local_only": var.local_only,
        })).collect::<Vec<_>>(),
        "inputs": asset.inputs.iter().map(|slot| json!({
            "name": slot.name,
            "kind": format!("{:?}", slot.kind),
            "speed": [slot.speed.0, slot.speed.1],
            "pitch_clamp": [slot.pitch_clamp.0, slot.pitch_clamp.1],
        })).collect::<Vec<_>>(),
        "director": camera_graph::rig_graph(asset).director,
        "rigs": asset.rigs.iter().enumerate().map(|(index, rig)| json!({
            "name": rig.name,
            "index": index,
            "fields": edit::rig_form(rig).map(|form| form.fields).unwrap_or(Value::Null),
            "nodes": rig.root.len(),
            "transitions": rig.transitions.len(),
            "references": edit::rig_references(asset, &rig.name),
        })).collect::<Vec<_>>(),
        "transitions": transition_rows(asset),
    })
}

/// Every transition of `asset` with where it is and its form: the shared list, then each rig's.
fn transition_rows(asset: &CameraAsset) -> Vec<Value> {
    let shared = asset.transitions.iter().map(|t| (None, t));
    let own = asset
        .rigs
        .iter()
        .flat_map(|rig| rig.transitions.iter().map(move |t| (Some(rig.name.as_str()), t)));
    let mut index: BTreeMap<Option<&str>, usize> = BTreeMap::new();
    shared
        .chain(own)
        .map(|(owner, transition)| {
            let at = index.entry(owner).or_default();
            let row = transition_json(owner, *at, transition);
            *at += 1;
            row
        })
        .collect()
}

fn form_json(form: &edit::Form) -> Value {
    json!({ "kind": form.kind, "fields": form.fields, "variants": form.enums })
}

fn transition_json(
    owner: Option<&str>,
    index: usize,
    transition: &regia_framework::camera::asset::TransitionDef,
) -> Value {
    let form = edit::transition_form(transition).map_or(Value::Null, |form| form_json(&form));
    json!({ "owner": owner, "index": index, "form": form })
}

/// The project-relative files named `*.camera.ron` under `assets/`, sorted, relative to `assets/`.
#[must_use]
pub fn find_camera_files(project: &Path) -> Vec<String> {
    let root = project.join("assets");
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    let mut walked = 0usize;
    while let Some(dir) = stack.pop() {
        walked += 1;
        if walked > MAX_SCAN_DIRS {
            tracing::warn!(target: "regia_editor", "camera file scan stopped after {MAX_SCAN_DIRS} folders");
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `file_type` does not follow links, so a link cycle cannot trap the walk.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_ascii_lowercase().ends_with(".camera.ron"))
                && let Ok(rel) = path.strip_prefix(&root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

fn ensure_registry(world: &mut World) {
    if !world.contains_resource::<CameraAssets>() {
        world.init_resource::<CameraAssets>();
    }
    if !world.contains_resource::<CameraHooks>() {
        // The edit world has no game code, so it cannot know a game's hook names (see
        // `CameraHooks::accept_any_name`).
        let mut hooks = CameraHooks::default();
        hooks.accept_any_name();
        world.insert_resource(hooks);
    }
}

/// Reads the file `path` of the open project into the edit world's registry.
fn load_project_file(world: &mut World, path: &str) -> Result<CameraBuildStatus, RpcError> {
    ensure_registry(world);
    let rel = load::normalize_camera_path(path).map_err(|e| reason_error(e.reason, e.detail))?;
    let Some(root) = crate::mesh_source::project_root(world) else {
        // No project: the VFS or the asset server may still know the file.
        return load::load_camera_file(world, &rel).map_err(|e| reason_error(e.reason, e.detail));
    };
    let file = root.join("assets").join(&rel);
    let text = std::fs::read_to_string(&file)
        .map_err(|e| reason_error(load::FILE_NOT_FOUND, format!("{}: {e}", file.display())))?;
    load::load_camera_text(world, &rel, &text, false).map_err(|e| reason_error(e.reason, e.detail))
}

/// Loads every `.camera.ron` of the project the registry has no record of. Returns how many it read
/// and the failures, which also stay in [`CameraFiles`].
fn scan_project(world: &mut World) -> (usize, Vec<Value>) {
    ensure_registry(world);
    let Some(root) = crate::mesh_source::project_root(world) else {
        return (0, Vec::new());
    };
    let mut read = 0;
    let mut failures = Vec::new();
    for rel in find_camera_files(&root) {
        let known = world
            .get_resource::<CameraFiles>()
            .is_some_and(|files| files.get(&rel).is_some());
        if known {
            continue;
        }
        read += 1;
        if let Err(error) = load_project_file(world, &rel) {
            failures.push(json!({ "path": rel, "message": error.message }));
        }
    }
    (read, failures)
}

impl CameraPanel {
    /// Copies the drivers of `world` into the panel; the selection falls back to the first driver.
    pub fn snapshot_drivers(&mut self, world: &mut World, source: LiveSource) {
        let _span = tracing::debug_span!(target: "regia_editor", "camera_panel_snapshot").entered();
        if self.source != source {
            self.selected_driver = None;
        }
        self.source = source;
        let mut query = world.query::<(Entity, &CameraDriver)>();
        let mut rows: Vec<DriverRow> = query
            .iter(world)
            .map(|(entity, driver)| DriverRow {
                entity,
                name: crate::outliner::display_name(world, entity),
                asset: driver.asset.clone(),
                enabled: driver.enabled,
                bound: driver.core().is_some(),
                primary: driver
                    .core()
                    .and_then(|core| core.primary_name().map(str::to_owned)),
            })
            .collect();
        rows.sort_by_key(|row| row.entity.to_bits());
        self.selected_driver = self
            .selected_driver
            .filter(|entity| rows.iter().any(|row| row.entity == *entity))
            .or_else(|| rows.first().map(|row| row.entity));
        self.drivers = rows;
        self.live = self.selected_driver.and_then(|entity| {
            regia_agent::panel_calls::call(world, "camera.inspect", &json!({ "entity": entity.to_bits() }))
                .ok()
        });
        self.play_framework = source == LiveSource::Play && world.contains_resource::<CameraAssets>();
        self.play_assets = match (source, world.get_resource::<CameraAssets>()) {
            (LiveSource::Play, Some(assets)) => assets
                .names()
                .map(|name| PlayAssetRow {
                    name: name.to_owned(),
                    status: status_name(assets, name),
                    revision: assets.revision(name),
                })
                .collect(),
            _ => Vec::new(),
        };
        self.snapshots += 1;
    }

    /// Runs the queued driver calls on `world`.
    pub fn run_pending(&mut self, world: &mut World, source: LiveSource) {
        for call in std::mem::take(&mut self.pending) {
            let result = regia_agent::panel_calls::call(world, &call.method, &call.params);
            self.last_call = Some(CallRecord::of(&call.method, source, result));
        }
    }

    fn json(&self, world: &World) -> Value {
        let names: Vec<String> = world
            .get_resource::<CameraAssets>()
            .map(|assets| assets.names().map(str::to_owned).collect())
            .unwrap_or_default();
        let files: Vec<Value> = world
            .get_resource::<CameraFiles>()
            .into_iter()
            .flat_map(CameraFiles::iter)
            .map(|(path, file)| {
                json!({
                    "path": path,
                    "asset": file.asset,
                    "reads": file.generation,
                    "error": file.error.as_ref().map(|e| json!({ "reason": e.reason, "detail": e.detail })),
                })
            })
            .collect();
        json!({
            "source": self.source.name(),
            "assets": names.iter().map(|name| asset_row_json(world, name)).collect::<Vec<_>>(),
            "files": files,
            "selected_asset": self.selected_asset,
            "page": self.selected_asset.as_deref().map_or(Value::Null, |name| page_json(world, name)),
            "drivers": self.drivers.iter().map(|row| json!({
                "entity": row.entity.to_bits(),
                "name": row.name,
                "asset": row.asset,
                "enabled": row.enabled,
                "bound": row.bound,
                "primary": row.primary,
            })).collect::<Vec<_>>(),
            "selected_driver": self.selected_driver.map(Entity::to_bits),
            "live": self.live,
            "play_framework": self.play_framework,
            "play_assets": self.play_assets.iter().map(|row| json!({
                "name": row.name,
                "status": row.status,
                "revision": row.revision,
            })).collect::<Vec<_>>(),
            "snapshots": self.snapshots,
            "last_call": self.last_call.as_ref().map(CallRecord::json),
            "history": self.selected_asset.as_deref().map_or(Value::Null, |name| self.history_json(world, name)),
        })
    }

    /// The history of `name` as the agent reads it: a clean empty one when it has none yet or its
    /// working copy was reloaded since.
    fn history_json(&self, world: &World, name: &str) -> Value {
        let working = world
            .get_resource::<CameraAssets>()
            .and_then(|assets| assets.working(name));
        match self.docs.get(name) {
            Some(doc) if working == Some(&doc.last) => doc.json(),
            _ => json!({
                "dirty": false, "undo_depth": 0, "redo_depth": 0, "next_undo": null, "next_redo": null,
                "comments_warning": file_has_comments(world, name).then_some(history::COMMENTS_WARNING),
            }),
        }
    }
}

/// Whether the file of `name` on disk has RON comments (a save writes the canonical text).
fn file_has_comments(world: &World, name: &str) -> bool {
    let path = world
        .get_resource::<CameraAssets>()
        .and_then(|assets| assets.path(name))
        .map(str::to_owned);
    let (Some(path), Some(root)) = (path, crate::mesh_source::project_root(world)) else {
        return false;
    };
    std::fs::read_to_string(root.join("assets").join(path))
        .is_ok_and(|text| crate::abilities_panel::form::has_comments(&text))
}

/// The history of `name`, made (clean) when it has none or the working copy changed outside it.
fn sync_doc<'a>(panel: &'a mut CameraPanel, world: &World, name: &str) -> Option<&'a mut history::CameraDoc> {
    let working = world.get_resource::<CameraAssets>()?.working(name)?.clone();
    let fresh = panel.docs.get(name).is_none_or(|doc| doc.last != working);
    if fresh {
        let comments = file_has_comments(world, name);
        panel.docs.insert(
            name.to_owned(),
            history::CameraDoc::new(working.clone(), comments),
        );
    }
    Some(
        panel
            .docs
            .entry(name.to_owned())
            .or_insert_with(|| history::CameraDoc::new(working, false)),
    )
}

fn edit_error(e: edit::EditError) -> RpcError {
    reason_error(e.code, e.message)
}

/// What a committed edit reports: the asset's status and history, and whether the save will drop
/// the file's comments.
fn edit_reply(panel: &CameraPanel, world: &World, name: &str) -> Value {
    let history = panel.history_json(world, name);
    json!({
        "status": status_name(world.resource::<CameraAssets>(), name),
        "comments_dropped": !history["comments_warning"].is_null(),
        "history": history,
    })
}

/// Runs `edit` on a copy of the working copy of `name`; when it changed something, the copy becomes
/// the working copy (status stale) and one undo step. A `merge_key` lets consecutive edits of one
/// field merge while the history is not sealed (GUI drags); `None` is always its own step.
fn commit<T>(
    panel: &mut CameraPanel,
    world: &mut World,
    name: &str,
    label: &str,
    merge_key: Option<String>,
    edit: impl FnOnce(&mut CameraAsset) -> Result<T, edit::EditError>,
) -> Result<T, RpcError> {
    require_asset(world, name)?;
    let Some(before) = world
        .get_resource::<CameraAssets>()
        .and_then(|assets| assets.working(name))
        .cloned()
    else {
        return Err(reason_error("camera_unknown_asset", name.to_owned()));
    };
    let mut after = before.clone();
    let out = edit(&mut after).map_err(edit_error)?;
    if after == before {
        return Ok(out);
    }
    let Some(doc) = sync_doc(panel, world, name) else {
        return Err(reason_error("camera_unknown_asset", name.to_owned()));
    };
    if merge_key.is_none() {
        doc.undo.seal();
    }
    world
        .resource_mut::<CameraAssets>()
        .edit(name, |working| *working = after.clone());
    doc.undo.push(Box::new(history::AssetEdit {
        asset: name.to_owned(),
        label: label.to_owned(),
        before,
        after: after.clone(),
        merge_key: merge_key.clone(),
    }));
    if merge_key.is_none() {
        doc.undo.seal();
    }
    doc.last = after;
    Ok(out)
}

/// Undoes (or redoes) the last step of `name`'s history.
fn history_step(
    panel: &mut CameraPanel,
    world: &mut World,
    name: &str,
    redo: bool,
) -> Result<Value, RpcError> {
    require_asset(world, name)?;
    let Some(doc) = sync_doc(panel, world, name) else {
        return Err(reason_error("camera_unknown_asset", name.to_owned()));
    };
    let done = if redo {
        doc.undo.redo(world)
    } else {
        doc.undo.undo(world)
    };
    if !done {
        let (code, what) = if redo {
            ("camera_panel_nothing_to_redo", "redo")
        } else {
            ("camera_panel_nothing_to_undo", "undo")
        };
        return Err(reason_error(code, format!("{name} has nothing to {what}")));
    }
    if let Some(working) = world.resource::<CameraAssets>().working(name) {
        doc.last = working.clone();
    }
    panel.selected_node = None;
    Ok(edit_reply(panel, world, name))
}

/// Saves `name` and marks its history saved.
fn save_from_panel(panel: &mut CameraPanel, world: &mut World, name: &str) -> Result<Value, RpcError> {
    let saved = save_asset(world, name)?;
    if let Some(doc) = sync_doc(panel, world, name) {
        doc.undo.mark_saved();
        doc.comments = false;
    }
    Ok(saved)
}

/// Keeps the page's selection on an asset that exists.
fn settle_selection(panel: &mut CameraPanel, world: &World) {
    let Some(assets) = world.get_resource::<CameraAssets>() else {
        panel.selected_asset = None;
        return;
    };
    if panel
        .selected_asset
        .as_deref()
        .is_none_or(|name| assets.working(name).is_none())
    {
        panel.selected_asset = assets.names().next().map(str::to_owned);
    }
}

/// Draws the panel. Its first draw with a project open scans the project for camera files.
pub fn camera_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<CameraPanel>().unwrap_or_default();
    let root = crate::mesh_source::project_root(world);
    if root.is_some() && panel.scanned != root {
        panel.scanned = root;
        scan_project(world);
    }
    if !play_running(world) {
        panel.snapshot_drivers(world, LiveSource::Edit);
    }
    settle_selection(&mut panel, world);
    draw(ui, &mut panel, world);
    world.insert_resource(panel);
}

/// Editor-loop hook, once per frame: runs the driver calls on the play world and copies its drivers,
/// or runs the calls on the edit world when nothing plays.
pub fn pump(edit: &mut World, session: Option<&mut PieSession>) {
    let _span = tracing::debug_span!(target: "regia_editor", "camera_panel_pump").entered();
    let Some(mut panel) = edit.remove_resource::<CameraPanel>() else {
        return;
    };
    match session {
        Some(session) => {
            let play = session.app.world_mut();
            panel.run_pending(play, LiveSource::Play);
            panel.snapshot_drivers(play, LiveSource::Play);
        }
        None => panel.run_pending(edit, LiveSource::Edit),
    }
    edit.insert_resource(panel);
}

fn draw(ui: &mut egui::Ui, panel: &mut CameraPanel, world: &mut World) {
    let count = world
        .get_resource::<CameraAssets>()
        .map_or(0, |assets| assets.names().count());
    let detail = format!("{count} assets, {} drivers", panel.drivers.len());
    header(ui, "Cameras", panel.source, &detail);
    ui.horizontal(|ui| {
        ui.selectable_value(&mut panel.tab, CameraTab::Asset, "Asset");
        ui.selectable_value(&mut panel.tab, CameraTab::Graph, "Rig graph");
        ui.selectable_value(&mut panel.tab, CameraTab::Live, "Live");
    });
    ui.horizontal(|ui| {
        ui.label("Load");
        ui.text_edit_singleline(&mut panel.load_path);
        if ui.button("Load").clicked() {
            let path = panel.load_path.clone();
            let result = load_project_file(world, &path).map(|status| json!({ "status": status.name() }));
            panel.last_call = Some(CallRecord::of("camera.load", LiveSource::Edit, result));
        }
        if ui.button("Scan project").clicked() {
            let (read, failures) = scan_project(world);
            panel.last_call = Some(CallRecord::of(
                "scan",
                LiveSource::Edit,
                Ok(json!({ "read": read, "failures": failures })),
            ));
        }
    });
    asset_picker(ui, panel, world);
    if let Some(record) = &panel.last_call {
        record.draw(ui);
    }
    ui.separator();
    match panel.tab {
        CameraTab::Asset => asset_page(ui, panel, world),
        CameraTab::Graph => graph_page(ui, panel, world),
        CameraTab::Live => live_page(ui, panel),
    }
}

fn asset_picker(ui: &mut egui::Ui, panel: &mut CameraPanel, world: &World) {
    let Some(assets) = world.get_resource::<CameraAssets>() else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        for name in assets.names() {
            let selected = panel.selected_asset.as_deref() == Some(name);
            let text = format!("{name} [{}]", status_name(assets, name));
            if ui.selectable_label(selected, text).clicked() {
                panel.selected_asset = Some(name.to_owned());
                panel.selected_node = None;
            }
        }
    });
}

fn asset_page(ui: &mut egui::Ui, panel: &mut CameraPanel, world: &mut World) {
    let Some(name) = panel.selected_asset.clone() else {
        ui.weak("No camera asset is loaded. Load a .camera.ron, scan the project, or make one with Content > New > Camera rig.");
        return;
    };
    // A continuous edit (a drag, typing) ends when the pointer is up and no field has focus.
    let editing = ui.input(|i| i.pointer.any_down()) || ui.ctx().text_edit_focused();
    if let Some(doc) = sync_doc(panel, world, &name)
        && !editing
    {
        doc.undo.seal();
    }
    let history = panel.history_json(world, &name);
    let mut record = None;
    ui.horizontal(|ui| {
        let can_undo = history["undo_depth"].as_u64().unwrap_or(0) > 0;
        let can_redo = history["redo_depth"].as_u64().unwrap_or(0) > 0;
        if ui.add_enabled(can_undo, egui::Button::new("Undo")).clicked() {
            record = Some(CallRecord::of(
                "undo",
                LiveSource::Edit,
                history_step(panel, world, &name, false),
            ));
        }
        if ui.add_enabled(can_redo, egui::Button::new("Redo")).clicked() {
            record = Some(CallRecord::of(
                "redo",
                LiveSource::Edit,
                history_step(panel, world, &name, true),
            ));
        }
        if ui.button("Apply").clicked() {
            record = Some(CallRecord::of(
                "apply",
                LiveSource::Edit,
                apply_asset(world, &name),
            ));
        }
        let dirty = history["dirty"] == true;
        if ui
            .button(if dirty { "Save*" } else { "Save" })
            .on_hover_text("Ctrl+S")
            .clicked()
        {
            record = Some(CallRecord::of(
                "save",
                LiveSource::Edit,
                save_from_panel(panel, world, &name),
            ));
        }
        if ui.button("Reload file").clicked() {
            let path = world
                .get_resource::<CameraAssets>()
                .and_then(|assets| assets.path(&name))
                .map(str::to_owned);
            let result = match path {
                Some(path) => {
                    load_project_file(world, &path).map(|status| json!({ "status": status.name() }))
                }
                None => Err(reason_error("camera_panel_no_path", "this asset has no file")),
            };
            record = Some(CallRecord::of("reload", LiveSource::Edit, result));
        }
    });
    save_shortcut(ui, panel, world, &name, &mut record);
    if record.is_some() {
        panel.last_call = record;
    }
    if let Some(warning) = history["comments_warning"].as_str() {
        // Wraps in a narrow dock (bug 19, ADR-8586).
        ui.add(
            egui::Label::new(egui::RichText::new(warning).color(egui::Color32::from_rgb(235, 170, 80)))
                .wrap(),
        );
    }
    let page = page_json(world, &name);
    let row = &page["row"];
    ui.label(format!(
        "{}  revision {}  read {} times",
        row["path"].as_str().unwrap_or("(no file)"),
        row["revision"],
        row["reads"]
    ));
    ui.monospace(format!("status: {}", row["status"].as_str().unwrap_or("?")));
    ui.monospace(format!("director: {}", page["director"].as_str().unwrap_or("?")));
    if let Some(detail) = row["file_error"]["detail"].as_str() {
        ui.colored_label(egui::Color32::from_rgb(230, 90, 90), detail);
    }
    egui::ScrollArea::vertical()
        .id_salt("camera-asset-page")
        .show(ui, |ui| {
            for issue in page["issues"].as_array().into_iter().flatten() {
                let color = if issue["severity"] == "error" {
                    egui::Color32::from_rgb(230, 90, 90)
                } else {
                    egui::Color32::from_rgb(235, 170, 80)
                };
                ui.colored_label(
                    color,
                    format!(
                        "{}: {}",
                        issue["code"].as_str().unwrap_or("?"),
                        issue["message"].as_str().unwrap_or("?")
                    ),
                );
            }
            egui::CollapsingHeader::new("Parameters")
                .default_open(true)
                .show(ui, |ui| params_editor(ui, panel, world, &name));
            egui::CollapsingHeader::new("Rigs")
                .default_open(true)
                .show(ui, |ui| rigs_editor(ui, panel, world, &name, &page));
            egui::CollapsingHeader::new("Transitions")
                .default_open(true)
                .show(ui, |ui| transitions_editor(ui, panel, world, &name, &page));
            egui::CollapsingHeader::new("Variables")
                .default_open(true)
                .show(ui, |ui| {
                    for var in page["variables"].as_array().into_iter().flatten() {
                        ui.monospace(format!(
                            "{} ({}) = {}",
                            var["name"].as_str().unwrap_or("?"),
                            var["type"].as_str().unwrap_or("?"),
                            var["default"]
                        ));
                    }
                });
            egui::CollapsingHeader::new("Input slots")
                .default_open(true)
                .show(ui, |ui| {
                    for slot in page["inputs"].as_array().into_iter().flatten() {
                        ui.monospace(format!(
                            "{} ({})",
                            slot["name"].as_str().unwrap_or("?"),
                            slot["kind"].as_str().unwrap_or("?")
                        ));
                    }
                });
        });
}

/// Ctrl+S (Cmd+S on macOS) saves the page's asset while the pointer is over the panel and no text
/// field has focus; once per egui pass (egui may run the panel twice in one frame).
fn save_shortcut(
    ui: &egui::Ui,
    panel: &mut CameraPanel,
    world: &mut World,
    name: &str,
    record: &mut Option<CallRecord>,
) {
    if !ui.rect_contains_pointer(ui.max_rect()) || ui.ctx().text_edit_focused() {
        return;
    }
    let pass = ui.ctx().cumulative_frame_nr();
    if panel.shortcut_pass == Some(pass) {
        return;
    }
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
        panel.shortcut_pass = Some(pass);
        *record = Some(CallRecord::of(
            "save",
            LiveSource::Edit,
            save_from_panel(panel, world, name),
        ));
    }
}

/// Runs one panel edit as the agent op `op` with `params` (one route for the GUI and the agent).
fn gui_edit(
    panel: &mut CameraPanel,
    world: &mut World,
    name: &str,
    mut params: Value,
    merge_key: Option<String>,
) {
    let op = params["op"].as_str().unwrap_or_default().to_owned();
    if let Some(map) = params.as_object_mut() {
        map.insert("asset".to_owned(), json!(name));
    }
    let result = run_edit_op(panel, world, name, &op, &params, merge_key);
    if result.is_err()
        || !matches!(
            op.as_str(),
            "set_param" | "node_field" | "rig_field" | "transition_field"
        )
    {
        panel.last_call = Some(CallRecord::of(&op, LiveSource::Edit, result));
    }
}

fn params_editor(ui: &mut egui::Ui, panel: &mut CameraPanel, world: &mut World, name: &str) {
    let Some(params) = world
        .get_resource::<CameraAssets>()
        .and_then(|assets| assets.working(name))
        .map(|working| working.params.clone())
    else {
        return;
    };
    let mut changed = None;
    egui::Grid::new("camera-params").num_columns(2).show(ui, |ui| {
        for (key, mut value) in params {
            ui.monospace(&key);
            let edited = match &mut value {
                ParamValue::Bool(v) => ui.checkbox(v, "").changed(),
                ParamValue::Int(v) => ui.add(egui::DragValue::new(v)).changed(),
                ParamValue::Float(v) => ui.add(egui::DragValue::new(v).speed(0.01)).changed(),
                ParamValue::Vec3(v) => {
                    let mut any = false;
                    ui.horizontal(|ui| {
                        for c in v.iter_mut() {
                            any |= ui.add(egui::DragValue::new(c).speed(0.01)).changed();
                        }
                    });
                    any
                }
                ParamValue::Name(v) => ui.text_edit_singleline(v).changed(),
            };
            if edited {
                changed = Some((key, value));
            }
            ui.end_row();
        }
    });
    if let Some((key, value)) = changed {
        let merge = Some(format!("param/{key}"));
        gui_edit(
            panel,
            world,
            name,
            json!({ "op": "set_param", "param": key, "value": param_json(&value) }),
            merge,
        );
    }
}

fn rigs_editor(ui: &mut egui::Ui, panel: &mut CameraPanel, world: &mut World, name: &str, page: &Value) {
    let rigs = page["rigs"].as_array().cloned().unwrap_or_default();
    let count = rigs.len();
    let mut action: Option<(Value, Option<String>)> = None;
    for rig in &rigs {
        let rig_name = rig["name"].as_str().unwrap_or_default().to_owned();
        let index = rig["index"].as_u64().unwrap_or(0) as usize;
        egui::CollapsingHeader::new(format!("{rig_name}  ({} nodes)", rig["nodes"]))
            .id_salt(("camera-rig", index))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.add_enabled(index > 0, egui::Button::new("Up")).clicked() {
                        action = Some((
                            json!({ "op": "move_rig", "rig": rig_name, "to": index - 1 }),
                            None,
                        ));
                    }
                    if ui
                        .add_enabled(index + 1 < count, egui::Button::new("Down"))
                        .clicked()
                    {
                        action = Some((
                            json!({ "op": "move_rig", "rig": rig_name, "to": index + 1 }),
                            None,
                        ));
                    }
                    if ui.button("Duplicate").clicked() {
                        action = Some((json!({ "op": "duplicate_rig", "rig": rig_name }), None));
                    }
                    let refs = rig["references"].as_array().map_or(0, Vec::len);
                    let remove = ui.add_enabled(refs == 0, egui::Button::new("Remove"));
                    let remove = if refs > 0 {
                        remove.on_disabled_hover_text(format!("still named by {}", rig["references"]))
                    } else {
                        remove
                    };
                    if remove.clicked() {
                        action = Some((json!({ "op": "remove_rig", "rig": rig_name }), None));
                    }
                });
                ui.horizontal(|ui| {
                    let mut text = match &panel.rename {
                        Some((rig, text)) if *rig == rig_name => text.clone(),
                        _ => rig_name.clone(),
                    };
                    if ui.text_edit_singleline(&mut text).changed() {
                        panel.rename = Some((rig_name.clone(), text.clone()));
                    }
                    if ui
                        .add_enabled(text != rig_name, egui::Button::new("Rename"))
                        .clicked()
                    {
                        action = Some((json!({ "op": "rename_rig", "rig": rig_name, "name": text }), None));
                        panel.rename = None;
                    }
                });
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt(("camera-add-kind", index))
                        .selected_text(edit::NODE_KINDS.get(panel.add_kind).copied().unwrap_or("Fov"))
                        .show_ui(ui, |ui| {
                            for (i, kind) in edit::NODE_KINDS.iter().enumerate() {
                                ui.selectable_value(&mut panel.add_kind, i, *kind);
                            }
                        });
                    if ui.button("Add node").clicked() {
                        let kind = edit::NODE_KINDS.get(panel.add_kind).copied().unwrap_or("Fov");
                        action = Some((json!({ "op": "add_node", "rig": rig_name, "kind": kind }), None));
                    }
                    if ui.button("Add own transition").clicked() {
                        action = Some((json!({ "op": "add_transition", "owner": rig_name }), None));
                    }
                });
                let empty = BTreeMap::new();
                if let Some(form_edit) =
                    form_ui::form_rows(ui, &format!("rig-{index}"), &rig["fields"], &empty, &[])
                {
                    let (params, merge) = form_op(form_edit, json!({ "rig": rig_name }), "rig", &rig_name);
                    let params = match params {
                        Some(params) => Some(params),
                        None => list_entry_params(world, name, "rig", &json!({ "rig": rig_name }), &merge),
                    };
                    action = params.map(|params| (params, Some(merge)));
                }
            });
    }
    if ui.button("Add rig").clicked() {
        action = Some((json!({ "op": "add_rig" }), None));
    }
    if let Some((params, merge)) = action {
        gui_edit(panel, world, name, params, merge);
    }
}

/// The op params of one form edit of a node, rig or transition (`what`), or `None` for an Add entry
/// (its list is computed by [`list_entry_params`]); and the merge key of the field.
fn form_op(form_edit: form_ui::FormEdit, target: Value, what: &str, id: &str) -> (Option<Value>, String) {
    let mut params = target;
    let (field, op, extra) = match form_edit {
        form_ui::FormEdit::Set(field, value) => (field, format!("{what}_field"), Some(("value", value))),
        form_ui::FormEdit::Variant(field, variant) => (
            field,
            format!("{what}_variant"),
            Some(("variant", Value::String(variant))),
        ),
        form_ui::FormEdit::Add(field) => {
            let merge = format!("{what}/{id}/{field}");
            if let Some(map) = params.as_object_mut() {
                map.insert("field".to_owned(), json!(field));
            }
            return (None, merge);
        }
    };
    let merge = format!("{what}/{id}/{field}");
    if let Some(map) = params.as_object_mut() {
        map.insert("op".to_owned(), json!(op));
        map.insert("field".to_owned(), json!(field));
        if let Some((key, value)) = extra {
            map.insert(key.to_owned(), value);
        }
    }
    (Some(params), merge)
}

/// The `<what>_field` op that sets a list field to itself plus one default-filled entry (the form's
/// Add entry: the agent sets the same list).
fn list_entry_params(world: &World, name: &str, what: &str, target: &Value, merge: &str) -> Option<Value> {
    let field = merge.rsplit('/').next()?.to_owned();
    let working = world.get_resource::<CameraAssets>()?.working(name)?;
    let value = match what {
        "rig" => edit::rig_list_entry(working, target["rig"].as_str()?, &field),
        "node" => {
            let path = node_path_param(target, "node").ok()?;
            edit::node_list_entry(working, target["rig"].as_str()?, &path, &field)
        }
        _ => edit::transition_list_entry(working, &transition_at(target).ok()?, &field),
    }
    .ok()?;
    let mut params = target.clone();
    let map = params.as_object_mut()?;
    map.insert("op".to_owned(), json!(format!("{what}_field")));
    map.insert("field".to_owned(), json!(field));
    map.insert("value".to_owned(), value);
    Some(params)
}

fn transitions_editor(
    ui: &mut egui::Ui,
    panel: &mut CameraPanel,
    world: &mut World,
    name: &str,
    page: &Value,
) {
    let rows = page["transitions"].as_array().cloned().unwrap_or_default();
    let Some(working) = world
        .get_resource::<CameraAssets>()
        .and_then(|assets| assets.working(name))
        .cloned()
    else {
        return;
    };
    let mut action: Option<(Value, Option<String>)> = None;
    for row in &rows {
        let owner = row["owner"].clone();
        let index = row["index"].as_u64().unwrap_or(0) as usize;
        let len = rows.iter().filter(|r| r["owner"] == owner).count();
        let typed = match owner.as_str() {
            Some(rig) => working.rig(rig).and_then(|def| def.transitions.get(index)),
            None => working.transitions.get(index),
        };
        let Some(form) = typed.and_then(|t| edit::transition_form(t).ok()) else {
            continue;
        };
        let list = owner
            .as_str()
            .map_or_else(|| "shared".to_owned(), |rig| format!("rig {rig}"));
        let target = json!({ "owner": owner, "index": index });
        let id = format!("{list}-{index}");
        egui::CollapsingHeader::new(format!("{list} #{index}"))
            .id_salt(("camera-transition", &id))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let op = |name: &str, to: Option<usize>| {
                        let mut params = target.clone();
                        params["op"] = json!(name);
                        if let Some(to) = to {
                            params["to"] = json!(to);
                        }
                        Some((params, None))
                    };
                    if ui.add_enabled(index > 0, egui::Button::new("Up")).clicked() {
                        action = op("move_transition", Some(index - 1));
                    }
                    if ui
                        .add_enabled(index + 1 < len, egui::Button::new("Down"))
                        .clicked()
                    {
                        action = op("move_transition", Some(index + 1));
                    }
                    if ui.button("Duplicate").clicked() {
                        action = op("duplicate_transition", None);
                    }
                    if ui.button("Remove").clicked() {
                        action = op("remove_transition", None);
                    }
                });
                if let Some(form_edit) = form_ui::form_rows(ui, &id, &form.fields, &form.enums, &[]) {
                    let (params, merge) = form_op(form_edit, target.clone(), "transition", &id);
                    let params = match params {
                        Some(params) => Some(params),
                        None => list_entry_params(world, name, "transition", &target, &merge),
                    };
                    action = params.map(|params| (params, Some(merge)));
                }
            });
    }
    if ui.button("Add shared transition").clicked() {
        action = Some((json!({ "op": "add_transition" }), None));
    }
    if let Some((params, merge)) = action {
        gui_edit(panel, world, name, params, merge);
    }
}

/// Colors of a rig graph box: the node's state in the running driver, else its kind of box.
fn node_color(switched_off: bool, live: Option<bool>) -> egui::Color32 {
    match (switched_off, live) {
        (true, _) => egui::Color32::from_gray(70),
        (_, Some(true)) => egui::Color32::from_rgb(150, 60, 60),
        (_, Some(false)) => egui::Color32::from_rgb(50, 110, 70),
        (_, None) => egui::Color32::from_rgb(60, 70, 90),
    }
}

fn graph_page(ui: &mut egui::Ui, panel: &mut CameraPanel, world: &mut World) {
    let Some(name) = panel.selected_asset.clone() else {
        ui.weak("No camera asset is loaded.");
        return;
    };
    let graph = world
        .get_resource::<CameraAssets>()
        .and_then(|assets| assets.working(&name))
        .map(camera_graph::rig_graph);
    let Some(graph) = graph else {
        return;
    };
    let layout = camera_graph::layout_graph(&graph);
    ui.monospace(&graph.director);
    egui::ScrollArea::both()
        .id_salt("camera-rig-graph")
        .max_height((ui.available_height() - 140.0).max(80.0))
        .show(ui, |ui| paint_graph(ui, panel, &graph, &layout));
    // FU-5: the keyboard route of the graph canvas (camera_graph::node_list_ui).
    camera_graph::node_list_ui(ui, &graph, &mut panel.selected_node);
    ui.separator();
    node_details(ui, panel, world, &name, &graph);
}

fn paint_graph(ui: &mut egui::Ui, panel: &mut CameraPanel, graph: &RigGraph, layout: &GraphLayout) {
    let size = egui::vec2(layout.width.max(1.0), layout.height.max(1.0));
    let (response, painter) = ui.allocate_painter(size, egui::Sense::click());
    let origin = response.rect.min.to_vec2();
    let to_rect =
        |r: [f32; 4]| egui::Rect::from_min_size(egui::pos2(r[0], r[1]) + origin, egui::vec2(r[2], r[3]));
    let font = egui::FontId::proportional(11.0);
    let text = ui.visuals().text_color();
    let live = panel.live.as_ref();
    for arc in &layout.arcs {
        let edge = &graph.edges[arc.edge];
        let color = if edge.blend == "cut" {
            egui::Color32::from_rgb(200, 120, 120)
        } else {
            egui::Color32::from_rgb(120, 170, 220)
        };
        let stroke = egui::Stroke::new(1.0, color);
        let from = egui::pos2(arc.x, arc.from_y) + origin;
        let to = egui::pos2(arc.x, arc.to_y) + origin;
        let lane_end = origin.x + layout.rig_labels.first().map_or(arc.x, |rect| rect[0]);
        painter.line_segment([egui::pos2(lane_end, from.y), from], stroke);
        painter.line_segment([from, to], stroke);
        painter.arrow(to, egui::vec2(lane_end - to.x, 0.0), stroke);
    }
    for (index, rig) in graph.rigs.iter().enumerate() {
        let Some(rect) = layout.rig_labels.get(index).copied() else {
            continue;
        };
        let weight = camera_graph::rig_weight(live, &rig.name);
        let label = match weight {
            Some(weight) => format!("{}  weight {weight:.2}", rig.name),
            None if rig.director_choice => format!("{}  (director)", rig.name),
            None => rig.name.clone(),
        };
        let color = if weight.is_some() {
            egui::Color32::from_rgb(120, 220, 120)
        } else {
            text
        };
        painter.text(
            to_rect(rect).left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.0),
            color,
        );
    }
    let click = response.interact_pointer_pos().filter(|_| response.clicked());
    let focused = camera_graph::focused_node(ui.ctx(), graph);
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("Camera rig graph canvas, {} nodes", layout.nodes.len()),
    );
    for placed in &layout.nodes {
        let rig = &graph.rigs[placed.rig];
        let node = &rig.nodes[placed.node];
        let rect = to_rect(placed.rect);
        let state = camera_graph::node_live(live, &rig.name, node.index);
        painter.rect_filled(rect, 4.0, node_color(node.switched_off, state));
        let chosen = panel
            .selected_node
            .as_ref()
            .is_some_and(|(r, path)| *r == placed.rig && *path == node.path);
        if chosen {
            painter.rect_stroke(
                rect,
                4.0,
                egui::Stroke::new(2.0, egui::Color32::WHITE),
                egui::StrokeKind::Outside,
            );
        }
        if focused
            .as_ref()
            .is_some_and(|(r, path)| *r == placed.rig && *path == node.path)
        {
            crate::canvas_keys::focus_ring(&painter, rect);
        }
        painter.text(
            rect.left_top() + egui::vec2(4.0, 3.0),
            egui::Align2::LEFT_TOP,
            node.kind,
            egui::FontId::proportional(12.0),
            text,
        );
        let summary: String = node.summary.chars().take(22).collect();
        painter.text(
            rect.left_top() + egui::vec2(4.0, NODE_H / 2.0 + 1.0),
            egui::Align2::LEFT_TOP,
            summary,
            font.clone(),
            text,
        );
        if click.is_some_and(|pos| rect.contains(pos)) {
            panel.selected_node = Some((placed.rig, node.path.clone()));
        }
    }
}

fn node_details(ui: &mut egui::Ui, panel: &mut CameraPanel, world: &mut World, name: &str, graph: &RigGraph) {
    let Some((rig, path)) = panel.selected_node.clone() else {
        ui.weak("Click a node for its settings.");
        return;
    };
    let node = world
        .get_resource::<CameraAssets>()
        .and_then(|assets| assets.working(name))
        .and_then(|asset| asset.rigs.get(rig))
        .and_then(|def| camera_graph::node_at(def, &path))
        .cloned();
    let Some(node) = node else {
        panel.selected_node = None;
        return;
    };
    let rig_name = graph.rigs.get(rig).map_or("?", |r| r.name.as_str()).to_owned();
    let editing = ui.input(|i| i.pointer.any_down()) || ui.ctx().text_edit_focused();
    if let Some(doc) = sync_doc(panel, world, name)
        && !editing
    {
        doc.undo.seal();
    }
    ui.strong(format!("{rig_name}: {}  at {path:?}", node.kind()));
    let siblings = world
        .get_resource::<CameraAssets>()
        .and_then(|assets| assets.working(name))
        .and_then(|asset| asset.rigs.get(rig))
        .map_or(0, |def| match path.split_last() {
            Some((_, [])) | None => def.root.len(),
            Some((_, parent)) => camera_graph::node_at(def, parent).map_or(0, |p| p.children().len()),
        });
    let index = path.last().copied().unwrap_or(0);
    let target = json!({ "rig": rig_name, "node": path });
    let mut action: Option<(Value, Option<String>)> = None;
    let op = |name: &str, extra: Value| {
        let mut params = target.clone();
        params["op"] = json!(name);
        for (key, value) in extra.as_object().into_iter().flatten() {
            params[key] = value.clone();
        }
        Some((params, None))
    };
    ui.horizontal(|ui| {
        if ui.add_enabled(index > 0, egui::Button::new("Up")).clicked() {
            action = op("move_node", json!({ "to": index.saturating_sub(1) }));
        }
        if ui
            .add_enabled(index + 1 < siblings, egui::Button::new("Down"))
            .clicked()
        {
            action = op("move_node", json!({ "to": index + 1 }));
        }
        if ui.button("Duplicate").clicked() {
            action = op("duplicate_node", json!({}));
        }
        if ui.button("Remove").clicked() {
            action = op("remove_node", json!({}));
        }
    });
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("camera-node-add-kind")
            .selected_text(edit::NODE_KINDS.get(panel.add_kind).copied().unwrap_or("Fov"))
            .show_ui(ui, |ui| {
                for (i, kind) in edit::NODE_KINDS.iter().enumerate() {
                    ui.selectable_value(&mut panel.add_kind, i, *kind);
                }
            });
        let kind = edit::NODE_KINDS.get(panel.add_kind).copied().unwrap_or("Fov");
        let parent: Vec<usize> = path.split_last().map(|(_, p)| p.to_vec()).unwrap_or_default();
        if ui.button("Add after").clicked() {
            action = Some((
                json!({ "op": "add_node", "rig": rig_name, "parent": parent, "index": index + 1, "kind": kind }),
                None,
            ));
        }
        let container = matches!(node, Node::Group { .. } | Node::When { .. });
        if ui.add_enabled(container, egui::Button::new("Add inside")).clicked() {
            action = Some((
                json!({ "op": "add_node", "rig": rig_name, "parent": path, "kind": kind }),
                None,
            ));
        }
    });
    if let Node::Group { enabled, .. } = &node {
        let mut on = !matches!(enabled, NodeValue::Literal(false));
        if ui.checkbox(&mut on, "Group on").changed() {
            action = Some((
                json!({ "op": "set_group", "rig": rig_name, "node": path, "enabled": on }),
                None,
            ));
        }
    }
    egui::ScrollArea::vertical()
        .id_salt("camera-node-details")
        .show(ui, |ui| match edit::node_form(&node) {
            Ok(form) => {
                let _span = tracing::trace_span!(target: "regia_editor", "camera_node_form").entered();
                let id = format!("node-{rig}-{path:?}");
                if let Some(form_edit) = form_ui::form_rows(ui, &id, &form.fields, &form.enums, &["nodes"]) {
                    let (params, merge) = form_op(form_edit, target.clone(), "node", &id);
                    let params = match params {
                        Some(params) => Some(params),
                        None => list_entry_params(world, name, "node", &target, &merge),
                    };
                    action = params.map(|params| (params, Some(merge)));
                }
            }
            Err(e) => {
                ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e.message);
            }
        });
    if let Some((params, merge)) = action {
        gui_edit(panel, world, name, params, merge);
    }
}

fn live_page(ui: &mut egui::Ui, panel: &mut CameraPanel) {
    if panel.source == LiveSource::Play && !panel.play_framework {
        ui.weak("This play session has no camera framework: the game adds CameraPlugin.");
        return;
    }
    if panel.drivers.is_empty() {
        ui.weak("No CameraDriver here. Drivers run in play: press Play.");
        return;
    }
    let mut chosen = panel.selected_driver;
    for row in &panel.drivers {
        let text = format!(
            "{}  {}  rig: {}{}",
            row.name,
            row.asset,
            row.primary.as_deref().unwrap_or("-"),
            if row.bound { "" } else { "  (not bound)" }
        );
        if ui.selectable_label(chosen == Some(row.entity), text).clicked() {
            chosen = Some(row.entity);
        }
    }
    panel.selected_driver = chosen;
    for asset in &panel.play_assets {
        ui.weak(format!(
            "in play: {} [{}] revision {}",
            asset.name, asset.status, asset.revision
        ));
    }
    let Some(live) = panel.live.clone() else {
        return;
    };
    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt("camera-live")
        .show(ui, |ui| {
            if let Some(blend) = live.get("transition").filter(|t| !t.is_null()) {
                ui.monospace(format!(
                    "blend {} -> {}: {:.2} of {:.2} s",
                    blend["from"].as_str().unwrap_or("-"),
                    blend["to"].as_str().unwrap_or("-"),
                    blend["elapsed"].as_f64().unwrap_or(0.0),
                    blend["duration"].as_f64().unwrap_or(0.0)
                ));
            }
            for rig in live["rigs"].as_array().into_iter().flatten() {
                ui.monospace(format!(
                    "{}  weight {:.2}  skipped {}",
                    rig["name"].as_str().unwrap_or("?"),
                    rig["weight"].as_f64().unwrap_or(0.0),
                    rig["skipped"]
                ));
            }
            for layer in live["layers"].as_array().into_iter().flatten() {
                ui.monospace(format!(
                    "layer {} {} ({}): {} weight {:.2}",
                    layer["id"],
                    layer["rig"].as_str().unwrap_or("?"),
                    layer["kind"].as_str().unwrap_or("?"),
                    layer["phase"].as_str().unwrap_or("?"),
                    layer["weight"].as_f64().unwrap_or(0.0)
                ));
            }
            if let Some(vars) = live["variables"].as_object() {
                for (name, value) in vars {
                    ui.monospace(format!("var {name} = {value}"));
                }
            }
            for event in live["events"].as_array().into_iter().flatten() {
                ui.weak(format!(
                    "{} {} {}",
                    event["id"],
                    event["rig"].as_str().unwrap_or("?"),
                    event["kind"].as_str().unwrap_or("?")
                ));
            }
        });
}

fn require_asset(world: &World, name: &str) -> Result<(), RpcError> {
    let known = world
        .get_resource::<CameraAssets>()
        .is_some_and(|assets| assets.working(name).is_some());
    if known {
        Ok(())
    } else {
        Err(reason_error(
            "camera_unknown_asset",
            format!("no camera asset named {name:?} on the page"),
        ))
    }
}

/// Validates the working copy; it becomes the active version when it has no errors.
fn apply_asset(world: &mut World, name: &str) -> Result<Value, RpcError> {
    require_asset(world, name)?;
    ensure_registry(world);
    let status = world.resource_scope(|world, mut assets: Mut<CameraAssets>| {
        assets.revalidate(name, world.resource::<CameraHooks>())
    });
    let assets = world.resource::<CameraAssets>();
    Ok(json!({
        "status": status.map_or("unknown", CameraBuildStatus::name),
        "revision": assets.revision(name),
        "issues": assets.issues(name).unwrap_or(&[]).iter().map(issue_json).collect::<Vec<_>>(),
    }))
}

/// Applies the working copy and writes it to its file through a temporary file and a rename.
/// Comments and formatting of the old file do not survive: the text is the canonical form.
fn save_asset(world: &mut World, name: &str) -> Result<Value, RpcError> {
    let applied = apply_asset(world, name)?;
    if applied["status"] == CameraBuildStatus::Errors.name() {
        return Err(reason_error(
            "camera_panel_has_errors",
            format!("{name} has validation errors; the file was not written"),
        ));
    }
    let assets = world.resource::<CameraAssets>();
    let path = assets
        .path(name)
        .map(str::to_owned)
        .ok_or_else(|| reason_error("camera_panel_no_path", format!("{name} has no file to save to")))?;
    let rel = load::normalize_camera_path(&path).map_err(|e| reason_error(e.reason, e.detail))?;
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| reason_error("camera_panel_no_project", "no project is open"))?;
    let text = assets
        .working(name)
        .ok_or_else(|| reason_error("camera_unknown_asset", name.to_owned()))?
        .to_ron()
        .map_err(|e| reason_error("camera_panel_io", e.to_string()))?;
    let file = root.join("assets").join(&rel);
    write_atomic(&file, &text)
        .map_err(|e| reason_error("camera_panel_io", format!("{}: {e}", file.display())))?;
    Ok(json!({ "path": rel, "status": applied["status"], "revision": applied["revision"] }))
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The ops of `editor.camera_panel_edit` (`op`; with none, `param` or `rig` + `node` + `enabled` pick
/// `set_param` or `set_group`).
pub const EDIT_OPS: [&str; 22] = [
    "set_param",
    "set_group",
    "node_field",
    "node_variant",
    "add_node",
    "remove_node",
    "duplicate_node",
    "move_node",
    "add_rig",
    "remove_rig",
    "duplicate_rig",
    "move_rig",
    "rename_rig",
    "rig_field",
    "add_transition",
    "remove_transition",
    "duplicate_transition",
    "move_transition",
    "transition_field",
    "transition_variant",
    "undo",
    "redo",
];

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (a string) is required")))
}

fn index_param(params: &Value, key: &str) -> Result<usize, RpcError> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (an index) is required")))
}

fn value_param<'a>(params: &'a Value, key: &str) -> Result<&'a Value, RpcError> {
    params
        .get(key)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` is required")))
}

/// A node path (`node` or `parent`): an array of indices as `editor.camera_rig_graph` gives them.
fn node_path_param(params: &Value, key: &str) -> Result<Vec<usize>, RpcError> {
    params
        .get(key)
        .and_then(Value::as_array)
        .and_then(|items| items.iter().map(|v| v.as_u64().map(|n| n as usize)).collect())
        .ok_or_else(|| {
            RpcError::invalid_params(format!(
                "`{key}` must be a node path (an array of indices) from editor.camera_rig_graph"
            ))
        })
}

/// `owner` (a rig name, or absent/null for the shared list) and `index`.
fn transition_at(params: &Value) -> Result<edit::TransitionAt, RpcError> {
    let owner = match params.get("owner") {
        None | Some(Value::Null) => None,
        Some(Value::String(rig)) => Some(rig.clone()),
        Some(_) => return Err(RpcError::invalid_params("`owner` is a rig name or null")),
    };
    Ok(edit::TransitionAt {
        owner,
        index: index_param(params, "index")?,
    })
}

/// A transition end as written in the graph: `any`, `tag:<tag>`, or a rig name.
fn rig_match_param(
    params: &Value,
    key: &str,
) -> Result<Option<regia_framework::camera::asset::RigMatch>, RpcError> {
    use regia_framework::camera::asset::RigMatch;
    Ok(match params.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text == "any" => Some(RigMatch::Any),
        Some(Value::String(text)) => Some(match text.strip_prefix("tag:") {
            Some(tag) => RigMatch::Tag(tag.to_owned()),
            None => RigMatch::Rig(text.clone()),
        }),
        Some(_) => {
            return Err(RpcError::invalid_params(format!(
                "`{key}` is `any`, `tag:<tag>` or a rig name"
            )));
        }
    })
}

/// The op a call names: `op`, else the two forms that came before ops.
fn op_of(params: &Value) -> Result<&str, RpcError> {
    if let Some(op) = params.get("op") {
        let op = op.as_str().unwrap_or_default();
        if !EDIT_OPS.contains(&op) {
            return Err(reason_error(
                "bad_op",
                format!("unknown op {op:?}: {}", EDIT_OPS.join(", ")),
            ));
        }
        return Ok(op);
    }
    if params.get("param").is_some() {
        Ok("set_param")
    } else if params.get("rig").is_some() {
        Ok("set_group")
    } else {
        Err(RpcError::invalid_params(
            "give `op` (one of the edit ops), or `param` and `value`, or `rig`, `node` and `enabled`",
        ))
    }
}

/// Runs one edit op on `name`: one undo step when it changes the working copy.
fn run_edit_op(
    panel: &mut CameraPanel,
    world: &mut World,
    name: &str,
    op: &str,
    params: &Value,
    merge_key: Option<String>,
) -> Result<Value, RpcError> {
    let mut extra = serde_json::Map::new();
    match op {
        "undo" | "redo" => return history_step(panel, world, name, op == "redo"),
        "set_param" => {
            let key = str_param(params, "param")?.to_owned();
            let value = value_param(params, "value")?;
            let like = world
                .resource::<CameraAssets>()
                .working(name)
                .and_then(|asset| asset.params.get(&key))
                .cloned()
                .ok_or_else(|| {
                    reason_error("camera_unknown_name", format!("{name} has no parameter {key:?}"))
                })?;
            let next = param_from_json(&like, value).ok_or_else(|| {
                reason_error("camera_bad_value", format!("expected a {} value", like.kind()))
            })?;
            commit(panel, world, name, &format!("Set {key}"), merge_key, |asset| {
                asset.params.insert(key.clone(), next);
                Ok(())
            })?;
        }
        "set_group" => {
            let rig = str_param(params, "rig")?.to_owned();
            let path = node_path_param(params, "node")?;
            let on = params
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| RpcError::invalid_params("`enabled` (a boolean) is required"))?;
            commit(panel, world, name, "Switch group", merge_key, |asset| {
                let group = asset
                    .rigs
                    .iter_mut()
                    .find(|def| def.name == rig)
                    .and_then(|def| camera_graph::node_at_mut(def, &path));
                match group {
                    Some(Node::Group { enabled, .. }) => {
                        *enabled = NodeValue::Literal(on);
                        Ok(())
                    }
                    _ => Err(edit::EditError {
                        code: "camera_panel_unknown_node",
                        message: format!("{rig} has no Group at {path:?}"),
                    }),
                }
            })?;
        }
        "node_field" | "node_variant" => {
            let rig = str_param(params, "rig")?.to_owned();
            let path = node_path_param(params, "node")?;
            let field = str_param(params, "field")?.to_owned();
            if op == "node_field" {
                let value = value_param(params, "value")?.clone();
                commit(panel, world, name, &format!("Set {field}"), merge_key, |asset| {
                    edit::set_node_field(asset, &rig, &path, &field, &value)
                })?;
            } else {
                let variant = str_param(params, "variant")?.to_owned();
                commit(
                    panel,
                    world,
                    name,
                    &format!("{field} as {variant}"),
                    merge_key,
                    |asset| edit::set_node_variant(asset, &rig, &path, &field, &variant),
                )?;
            }
            panel.selected_node = rig_index(world, name, &rig).map(|r| (r, path));
        }
        "add_node" => {
            let rig = str_param(params, "rig")?.to_owned();
            let parent = match params.get("parent") {
                None | Some(Value::Null) => Vec::new(),
                Some(_) => node_path_param(params, "parent")?,
            };
            let index = match params.get("index") {
                None | Some(Value::Null) => None,
                Some(_) => Some(index_param(params, "index")?),
            };
            let kind = str_param(params, "kind")?.to_owned();
            let path = commit(panel, world, name, &format!("Add {kind}"), None, |asset| {
                edit::add_node(asset, &rig, &parent, index, &kind)
            })?;
            extra.insert("node".to_owned(), json!(path));
            panel.selected_node = rig_index(world, name, &rig).map(|r| (r, path));
        }
        "remove_node" | "duplicate_node" | "move_node" => {
            let rig = str_param(params, "rig")?.to_owned();
            let path = node_path_param(params, "node")?;
            let to = if op == "move_node" {
                Some(index_param(params, "to")?)
            } else {
                None
            };
            let label = match op {
                "remove_node" => "Remove node",
                "duplicate_node" => "Duplicate node",
                _ => "Move node",
            };
            let now = commit(panel, world, name, label, None, |asset| match (op, to) {
                ("remove_node", _) => edit::remove_node(asset, &rig, &path).map(|_| None),
                ("duplicate_node", _) => edit::duplicate_node(asset, &rig, &path).map(Some),
                (_, Some(to)) => edit::move_node(asset, &rig, &path, to).map(Some),
                _ => Ok(None),
            })?;
            extra.insert("node".to_owned(), json!(now));
            panel.selected_node = now.and_then(|path| rig_index(world, name, &rig).map(|r| (r, path)));
        }
        "add_rig" | "duplicate_rig" => {
            let new_name = params.get("name").and_then(Value::as_str).map(str::to_owned);
            let rig = if op == "duplicate_rig" {
                Some(str_param(params, "rig")?.to_owned())
            } else {
                None
            };
            let made = commit(
                panel,
                world,
                name,
                if rig.is_some() { "Duplicate rig" } else { "Add rig" },
                None,
                |asset| match &rig {
                    Some(rig) => edit::duplicate_rig(asset, rig, new_name.as_deref()),
                    None => edit::add_rig(asset, new_name.as_deref()),
                },
            )?;
            extra.insert("rig".to_owned(), json!(made));
            panel.selected_node = None;
        }
        "remove_rig" | "move_rig" | "rename_rig" => {
            let rig = str_param(params, "rig")?.to_owned();
            let label = match op {
                "remove_rig" => "Remove rig",
                "move_rig" => "Move rig",
                _ => "Rename rig",
            };
            let to = if op == "move_rig" {
                Some(index_param(params, "to")?)
            } else {
                None
            };
            let new_name = if op == "rename_rig" {
                Some(str_param(params, "name")?.to_owned())
            } else {
                None
            };
            commit(panel, world, name, label, None, |asset| match op {
                "remove_rig" => edit::remove_rig(asset, &rig).map(|_| ()),
                "move_rig" => edit::move_rig(asset, &rig, to.unwrap_or_default()),
                _ => edit::rename_rig(asset, &rig, new_name.as_deref().unwrap_or_default()),
            })?;
            if let Some(new_name) = new_name {
                extra.insert("rig".to_owned(), json!(new_name));
            }
            panel.selected_node = None;
        }
        "rig_field" => {
            let rig = str_param(params, "rig")?.to_owned();
            let field = str_param(params, "field")?.to_owned();
            let value = value_param(params, "value")?.clone();
            commit(panel, world, name, &format!("Set {field}"), merge_key, |asset| {
                edit::set_rig_field(asset, &rig, &field, &value)
            })?;
        }
        "add_transition" => {
            let owner = params.get("owner").and_then(Value::as_str).map(str::to_owned);
            let from = rig_match_param(params, "from_rig")?;
            let to = rig_match_param(params, "to_rig")?;
            let at = commit(panel, world, name, "Add transition", None, |asset| {
                edit::add_transition(asset, owner.as_deref(), from, to)
            })?;
            extra.insert(
                "transition".to_owned(),
                json!({ "owner": at.owner, "index": at.index }),
            );
        }
        "remove_transition" | "duplicate_transition" | "move_transition" => {
            let at = transition_at(params)?;
            let to = if op == "move_transition" {
                Some(index_param(params, "to")?)
            } else {
                None
            };
            let label = match op {
                "remove_transition" => "Remove transition",
                "duplicate_transition" => "Duplicate transition",
                _ => "Move transition",
            };
            let now = commit(panel, world, name, label, None, |asset| match (op, to) {
                ("remove_transition", _) => edit::remove_transition(asset, &at).map(|_| None),
                ("duplicate_transition", _) => edit::duplicate_transition(asset, &at).map(Some),
                (_, Some(to)) => edit::move_transition(asset, &at, to).map(Some),
                _ => Ok(None),
            })?;
            extra.insert(
                "transition".to_owned(),
                json!(now.map(|at| json!({ "owner": at.owner, "index": at.index }))),
            );
        }
        "transition_field" | "transition_variant" => {
            let at = transition_at(params)?;
            let field = str_param(params, "field")?.to_owned();
            if op == "transition_field" {
                let value = value_param(params, "value")?.clone();
                commit(panel, world, name, &format!("Set {field}"), merge_key, |asset| {
                    edit::set_transition_field(asset, &at, &field, &value)
                })?;
            } else {
                let variant = str_param(params, "variant")?.to_owned();
                commit(
                    panel,
                    world,
                    name,
                    &format!("{field} as {variant}"),
                    merge_key,
                    |asset| edit::set_transition_variant(asset, &at, &field, &variant),
                )?;
            }
        }
        other => {
            return Err(reason_error(
                "bad_op",
                format!("unknown op {other:?}: {}", EDIT_OPS.join(", ")),
            ));
        }
    }
    let mut reply = edit_reply(panel, world, name);
    if let Some(map) = reply.as_object_mut() {
        map.extend(extra);
    }
    Ok(reply)
}

fn rig_index(world: &World, name: &str, rig: &str) -> Option<usize> {
    world.resource::<CameraAssets>().working(name)?.rig_index(rig)
}

fn with_panel<T>(world: &mut World, f: impl FnOnce(&mut CameraPanel, &mut World) -> T) -> T {
    let mut panel = world.remove_resource::<CameraPanel>().unwrap_or_default();
    let out = f(&mut panel, world);
    world.insert_resource(panel);
    out
}

fn asset_name(panel: &CameraPanel, params: &Value) -> Result<String, RpcError> {
    params
        .get("asset")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| panel.selected_asset.clone())
        .ok_or_else(|| {
            reason_error(
                "camera_unknown_asset",
                "no asset is selected and no `asset` was given",
            )
        })
}

/// `editor.camera_panel_get`.
fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(with_panel(world, |panel, world| {
        ensure_registry(world);
        if !play_running(world) {
            panel.snapshot_drivers(world, LiveSource::Edit);
        }
        settle_selection(panel, world);
        panel.json(world)
    }))
}

/// `editor.camera_panel_select { asset?, driver? }`.
fn panel_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let driver = entity_param(params, "driver")?;
    with_panel(world, |panel, world| {
        ensure_registry(world);
        if let Some(name) = params.get("asset").and_then(Value::as_str) {
            require_asset(world, name)?;
            panel.selected_asset = Some(name.to_owned());
            panel.selected_node = None;
        }
        if !play_running(world) {
            panel.snapshot_drivers(world, LiveSource::Edit);
        }
        if let Some(entity) = driver {
            if !panel.drivers.iter().any(|row| row.entity == entity) {
                return Err(reason_error(
                    "camera_no_driver",
                    "no listed driver has that entity",
                ));
            }
            panel.selected_driver = Some(entity);
        }
        settle_selection(panel, world);
        Ok(panel.json(world))
    })
}

/// `editor.camera_panel_load { path? }`: one file, or a scan of the project with no path.
fn panel_load(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world| {
        ensure_registry(world);
        let answer = match params.get("path").and_then(Value::as_str) {
            Some(path) => {
                let status = load_project_file(world, path)?;
                let rel = load::normalize_camera_path(path).map_err(|e| reason_error(e.reason, e.detail))?;
                let asset = world
                    .get_resource::<CameraFiles>()
                    .and_then(|files| files.get(&rel))
                    .and_then(|file| file.asset.clone());
                panel.selected_asset.clone_from(&asset);
                json!({ "path": rel, "asset": asset, "status": status.name() })
            }
            None => {
                let (read, failures) = scan_project(world);
                panel.scanned = crate::mesh_source::project_root(world);
                json!({ "read": read, "failures": failures })
            }
        };
        settle_selection(panel, world);
        Ok(answer)
    })
}

/// `editor.camera_rig_graph { asset? }`: the graph, with the selected driver's weights if it runs it.
fn panel_graph(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world| {
        ensure_registry(world);
        settle_selection(panel, world);
        let name = asset_name(panel, params)?;
        require_asset(world, &name)?;
        let assets = world.resource::<CameraAssets>();
        let asset: Option<&CameraAsset> = assets.working(&name);
        let graph = asset.map(camera_graph::rig_graph).unwrap_or_default();
        let layout = camera_graph::layout_graph(&graph);
        let mut out = camera_graph::graph_json(&graph, &layout);
        let live = panel.live.as_ref().filter(|live| live["asset"] == name.as_str());
        if let Some(rigs) = out["rigs"].as_array_mut() {
            for rig in rigs {
                let rig_name = rig["name"].as_str().unwrap_or_default().to_owned();
                rig["live_weight"] = json!(camera_graph::rig_weight(live, &rig_name));
                let def = asset.and_then(|asset| asset.rig(&rig_name));
                for node in rig["nodes"].as_array_mut().into_iter().flatten() {
                    let index = node["index"].as_u64().unwrap_or(u64::MAX) as usize;
                    node["live_skipped"] = json!(camera_graph::node_live(live, &rig_name, index));
                    let path: Option<Vec<usize>> = node["path"]
                        .as_array()
                        .and_then(|items| items.iter().map(|v| v.as_u64().map(|n| n as usize)).collect());
                    let form = path
                        .zip(def)
                        .and_then(|(path, def)| camera_graph::node_at(def, &path))
                        .and_then(|typed| edit::node_form(typed).ok());
                    node["form"] = form.map_or(Value::Null, |form| form_json(&form));
                }
            }
        }
        out["asset"] = json!(name);
        Ok(out)
    })
}

/// `editor.camera_panel_edit { asset?, op, ... }`: one edit op (`EDIT_OPS`), or the older
/// `{ param, value }` / `{ rig, node, enabled }` forms.
fn panel_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world| {
        ensure_registry(world);
        settle_selection(panel, world);
        let op = op_of(params)?.to_owned();
        let name = asset_name(panel, params)?;
        require_asset(world, &name)?;
        run_edit_op(panel, world, &name, &op, params, None)
    })
}

/// `editor.camera_panel_apply { asset? }`.
fn panel_apply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world| {
        settle_selection(panel, world);
        let name = asset_name(panel, params)?;
        apply_asset(world, &name)
    })
}

/// `editor.camera_panel_save { asset? }`.
fn panel_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world| {
        settle_selection(panel, world);
        let name = asset_name(panel, params)?;
        save_from_panel(panel, world, &name)
    })
}

const DRIVER_OPS: &[(&str, &str)] = &[
    ("set_var", "camera.set_var"),
    ("set_param", "camera.set_param"),
    ("set_input", "camera.set_input"),
    ("reset_input", "camera.reset_input"),
    ("push_overlay", "camera.push_overlay"),
    ("add_additive", "camera.add_additive"),
    ("release_layer", "camera.release_layer"),
    ("hold_overlay", "camera.hold_overlay"),
];

/// `editor.camera_panel_driver { op, entity?, ... }`: a driver method on the watched world.
fn panel_driver(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = params.get("op").and_then(Value::as_str).unwrap_or_default();
    let Some((_, method)) = DRIVER_OPS.iter().find(|(name, _)| *name == op) else {
        let names: Vec<&str> = DRIVER_OPS.iter().map(|(name, _)| *name).collect();
        return Err(reason_error(
            "bad_op",
            format!("unknown op {op:?}: {}", names.join(", ")),
        ));
    };
    with_panel(world, |panel, world| {
        ensure_registry(world);
        let playing = play_running(world);
        if !playing {
            panel.snapshot_drivers(world, LiveSource::Edit);
        }
        let entity = entity_param(params, "entity")?
            .or(panel.selected_driver)
            .ok_or_else(|| {
                reason_error(
                    "camera_no_driver",
                    "no driver is selected and no `entity` was given",
                )
            })?;
        let mut call_params = params.clone();
        if let Some(map) = call_params.as_object_mut() {
            map.remove("op");
            map.insert("entity".to_owned(), json!(entity.to_bits()));
        }
        if playing {
            panel.pending.push(PanelCall::new(method, call_params));
            return Ok(json!({
                "queued": true,
                "hint": "applied to the play world on the next frame; read last_call from editor.camera_panel_get",
            }));
        }
        let result = regia_agent::panel_calls::call(world, method, &call_params);
        panel.last_call = Some(CallRecord::of(method, LiveSource::Edit, result.clone()));
        panel.snapshot_drivers(world, LiveSource::Edit);
        Ok(json!({ "queued": false, "answer": result? }))
    })
}

const fn param(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: false,
        description,
    }
}

const ASSET_PARAMS: &[ParamSchema] = &[param("asset", "string", "The asset; default the selected one.")];

const SELECT_PARAMS: &[ParamSchema] = &[
    param("asset", "string", "The asset the page shows."),
    param("driver", "integer", "The driver the live view follows."),
];

const LOAD_PARAMS: &[ParamSchema] = &[param(
    "path",
    "string",
    "A .camera.ron relative to assets/; with none, scan the project for new files.",
)];

const EDIT_PARAMS: &[ParamSchema] = &[
    param("asset", "string", "The asset; default the selected one."),
    param(
        "op",
        "string",
        "set_param, set_group, node_field, node_variant, add_node, remove_node, duplicate_node, move_node, \
            add_rig, remove_rig, duplicate_rig, move_rig, rename_rig, rig_field, add_transition, remove_transition, \
            duplicate_transition, move_transition, transition_field, transition_variant, undo or redo. Without op: \
            `param` means set_param, `rig` means set_group.",
    ),
    param(
        "param",
        "string",
        "set_param: the parameter whose default to change.",
    ),
    param(
        "value",
        "any",
        "set_param: the new default, of the same type; node_field, rig_field, transition_field: the form value.",
    ),
    param(
        "rig",
        "string",
        "The rig: of the node (set_group, node_*, add_node, remove/duplicate/move_node) or the rig op.",
    ),
    param(
        "node",
        "array",
        "The node's path from editor.camera_rig_graph (set_group, node_field, node_variant, remove/duplicate/move_node).",
    ),
    param("enabled", "boolean", "set_group: switch the Group."),
    param(
        "field",
        "string",
        "node_field, node_variant, rig_field, transition_field, transition_variant: the field path \
            (`length`, `length.Literal`, `offset[1]`, `blend.Smooth`, `tags`).",
    ),
    param(
        "variant",
        "string",
        "node_variant, transition_variant: the enum variant to switch to.",
    ),
    param(
        "kind",
        "string",
        "add_node: Pivot, LookInput, Boom, Collision, Fov, Offset, SmoothPosition, SmoothRotation, Lean, Frame, \
            Shake, Fly, Rail, Crane, Group or When.",
    ),
    param(
        "parent",
        "array",
        "add_node: the Group or When to add into; default the rig's root.",
    ),
    param(
        "index",
        "integer",
        "add_node: where in the list (default the end); transition ops: the transition's index.",
    ),
    param(
        "to",
        "integer",
        "move_node, move_rig, move_transition: the new index in the same list.",
    ),
    param(
        "name",
        "string",
        "add_rig, duplicate_rig: the new rig's name (default a free one); rename_rig: the new name.",
    ),
    param(
        "owner",
        "string",
        "Transition ops: the rig whose own list holds it; absent or null for the shared list.",
    ),
    param(
        "from_rig",
        "string",
        "add_transition: the outgoing end, `any`, `tag:<tag>` or a rig name (default any).",
    ),
    param(
        "to_rig",
        "string",
        "add_transition: the incoming end, as from_rig (default any).",
    ),
];

const DRIVER_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "op",
        kind: "string",
        required: true,
        description: "set_var, set_param, set_input, reset_input, push_overlay, add_additive, release_layer or \
            hold_overlay; the other parameters are that camera.* method's.",
    },
    param("entity", "integer", "The driver; default the selected one."),
    param("name", "string", "set_var, set_param: the name."),
    param("value", "any", "set_var, set_param, set_input: the value."),
    param("slot", "string", "set_input, reset_input: the slot."),
    param(
        "rig",
        "string",
        "push_overlay, add_additive, release_layer: the rig.",
    ),
    param("order", "integer", "add_additive: the sort order."),
    param("id", "integer", "release_layer, hold_overlay: the layer id."),
];

const STATE: &str = "{ source, assets: [{ name, path, status, issue_count, active, revision, rigs, reads, cooked, file_error }], \
    files: [{ path, asset, reads, error }], selected_asset, page: { row, issues, params, variables, inputs, director, \
    rigs: [{ name, index, fields, nodes, transitions, references }], transitions: [{ owner, index, form }] }, \
    drivers: [{ entity, name, asset, enabled, bound, primary }], selected_driver, live, play_framework, play_assets, \
    snapshots, last_call, history: { dirty, undo_depth, redo_depth, next_undo, next_redo, comments_warning } }";

const GRAPH: &str = "{ asset, director, rigs: [{ name, tags, director_choice, blend_out, label_rect, live_weight, nodes: [{ index, \
    path, depth, kind, summary, switched_off, rect, live_skipped, form: { kind, fields, variants } }] }], edges: [{ from, to, from_rigs, to_rigs, owner, blend, \
    seconds, start_facing, hold_outgoing, share_state, interruptible, conditions }], width, height }";

/// The Cameras panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.camera_panel_get",
                tool: "camera_panel_get",
                description: "The Cameras panel: the project's camera assets with status, reads and load errors, the \
                    selected asset's page, and the drivers of the play world with camera.inspect of the selected one.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.camera_panel_select",
                tool: "camera_panel_select",
                description: "Choose the asset on the Cameras panel's page and the driver its live view follows.",
                params: SELECT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::Play,
            },
            panel_select as Handler,
        ),
        (
            MethodSchema {
                name: "editor.camera_panel_load",
                tool: "camera_panel_load",
                description: "Read a .camera.ron of the project into the page, or scan the project for files it has \
                    not read. A changed file reloads by itself afterwards.",
                params: LOAD_PARAMS,
                result: "{ path, asset, status } | { read, failures: [{ path, message }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_load as Handler,
        ),
        (
            MethodSchema {
                name: "editor.camera_rig_graph",
                tool: "camera_rig_graph",
                description: "A camera asset as a graph: rigs with their nodes in evaluation order, transitions with \
                    the rigs each end matches, the canvas layout, and the running driver's rig weights and node states.",
                params: ASSET_PARAMS,
                result: GRAPH,
                mutating: false,
                permission: Permission::Read,
            },
            panel_graph as Handler,
        ),
        (
            MethodSchema {
                name: "editor.camera_panel_edit",
                tool: "camera_panel_edit",
                description: "Change the working copy of an asset, one undo step per change: a parameter's default, \
                    a Group's switch, a node, rig or transition field through its typed form, the structure (add, remove, \
                    duplicate, reorder and rename nodes, rigs and transitions; a rig something still names is not \
                    removed), or undo and redo. Its status is stale until editor.camera_panel_apply.",
                params: EDIT_PARAMS,
                result: "{ status, comments_dropped, history: { dirty, undo_depth, redo_depth, next_undo, next_redo, \
                    comments_warning }, node?, rig?, transition? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.camera_panel_apply",
                tool: "camera_panel_apply",
                description: "Validate the working copy; without errors it becomes the active version.",
                params: ASSET_PARAMS,
                result: "{ status, revision, issues }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_apply as Handler,
        ),
        (
            MethodSchema {
                name: "editor.camera_panel_save",
                tool: "camera_panel_save",
                description: "Apply the working copy and write it to its .camera.ron in canonical form, which reloads \
                    it in a running game. Comments in the old file do not survive.",
                params: ASSET_PARAMS,
                result: "{ path, status, revision }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.camera_panel_driver",
                tool: "camera_panel_driver",
                description: "Call a camera driver method on the world being watched. While playing the call runs on \
                    the play world on the next frame (queued: true) and its result is last_call.",
                params: DRIVER_PARAMS,
                result: "{ queued, answer?, hint? }",
                mutating: true,
                permission: Permission::Play,
            },
            panel_driver as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use regia_framework::camera::CameraPlugin;

    use super::*;
    use crate::gameplay_panel::reason_of;

    const GOOD: &str = r#"(format_version: 1, name: "player",
        params: { "len": Float(4.0), "shoulder": Vec3((0.0, 1.0, 0.0)) },
        variables: [(name: "aiming", default: Bool(false))],
        inputs: [(name: "look", kind: Axis2, speed: (120.0, 90.0), pitch_clamp: (-80.0, 30.0))],
        rigs: [
            (name: "walk", root: [
                Boom(length: Param("len")),
                Group(enabled: Literal(true), nodes: [Fov(degrees: Literal(70.0))]),
            ], transitions: [(from: Rig("walk"), to: Rig("aim"), blend: Smooth(0.25))]),
            (name: "aim", root: [Fov(degrees: Literal(40.0))]),
        ],
        director: Rules(default: "walk", rules: [(when: VarBool("aiming", true), rig: "aim")]))"#;

    fn project(tag: &str) -> (World, PathBuf) {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        (world, dir)
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let file = dir.join("assets").join(rel);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
        regia_test_utils::write_atomic(&file, text.as_bytes()).expect("write");
    }

    fn get(world: &mut World) -> Value {
        panel_get(world, &json!({})).expect("get")
    }

    fn reason(result: Result<Value, RpcError>) -> String {
        reason_of(&result.expect_err("refused")).unwrap_or_default()
    }

    #[test]
    fn load_shows_the_page_and_a_scan_finds_the_rest() {
        let (mut world, dir) = project("camera-panel-load");
        write(&dir, "camera/player.camera.ron", GOOD);
        write(
            &dir,
            "camera/deep/other.camera.ron",
            &GOOD.replace("\"player\"", "\"other\""),
        );
        write(&dir, "camera/bad.camera.ron", "(format_version: 1, nope");
        write(&dir, "camera/readme.txt", "not a camera");
        assert_eq!(
            find_camera_files(&dir),
            [
                "camera/bad.camera.ron",
                "camera/deep/other.camera.ron",
                "camera/player.camera.ron",
            ]
        );
        let loaded = panel_load(&mut world, &json!({ "path": "camera/player.camera.ron" })).expect("load");
        assert_eq!(loaded["asset"], "player");
        assert_eq!(loaded["status"], "warnings", "`shoulder` is declared and unused");
        let v = get(&mut world);
        assert_eq!(v["selected_asset"], "player");
        let page = &v["page"];
        assert_eq!(page["row"]["path"], "camera/player.camera.ron");
        assert_eq!(page["row"]["reads"], 1);
        assert_eq!(page["row"]["revision"], 1);
        assert_eq!(page["params"][0]["name"], "len");
        assert_eq!(page["params"][0]["type"], "Float");
        assert_eq!(page["variables"][0]["name"], "aiming");
        assert_eq!(page["inputs"][0]["name"], "look");
        assert!(page["director"].as_str().is_some_and(|d| d.starts_with("rules:")));

        let scan = panel_load(&mut world, &json!({})).expect("scan");
        assert_eq!(scan["read"], 2, "the file already read is left alone");
        assert_eq!(scan["failures"][0]["path"], "camera/bad.camera.ron");
        let v = get(&mut world);
        assert_eq!(v["assets"].as_array().expect("assets").len(), 2);
        let bad = v["files"]
            .as_array()
            .expect("files")
            .iter()
            .find(|f| f["path"] == "camera/bad.camera.ron")
            .cloned()
            .expect("the failure is on record");
        assert_eq!(bad["error"]["reason"], "camera_asset_invalid");
        assert!(
            bad["error"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("line") || d.contains(':'))
        );
        assert_eq!(panel_load(&mut world, &json!({})).expect("scan")["read"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_refuses_what_it_cannot_read() {
        let (mut world, dir) = project("camera-panel-refuse");
        for (path, code) in [
            ("../x.camera.ron", "camera_bad_path"),
            ("x.ron", "camera_bad_path"),
            ("none.camera.ron", "camera_file_not_found"),
        ] {
            assert_eq!(
                reason(panel_load(&mut world, &json!({ "path": path }))),
                code,
                "{path}"
            );
        }
        write(&dir, "b.camera.ron", "(");
        assert_eq!(
            reason(panel_load(&mut world, &json!({ "path": "b.camera.ron" }))),
            "camera_asset_invalid"
        );
        let mut none = World::new();
        assert_eq!(
            reason(panel_load(&mut none, &json!({ "path": "a.camera.ron" }))),
            "camera_file_not_found"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hook_director_is_valid_in_the_edit_world() {
        let (mut world, dir) = project("camera-panel-hook");
        write(
            &dir,
            "h.camera.ron",
            r#"(format_version: 1, name: "h", rigs: [(name: "a", root: [])], director: Hook("game_hook"))"#,
        );
        let loaded = panel_load(&mut world, &json!({ "path": "h.camera.ron" })).expect("load");
        assert_ne!(loaded["status"], "errors", "the game's code is not loaded here");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_graph_method_reports_rigs_nodes_edges_and_their_places() {
        let (mut world, dir) = project("camera-panel-graph");
        write(&dir, "p.camera.ron", GOOD);
        assert_eq!(
            reason(panel_graph(&mut world, &json!({}))),
            "camera_unknown_asset"
        );
        panel_load(&mut world, &json!({ "path": "p.camera.ron" })).expect("load");
        let g = panel_graph(&mut world, &json!({})).expect("graph");
        assert_eq!(g["asset"], "player");
        assert_eq!(g["rigs"][0]["name"], "walk");
        assert_eq!(g["rigs"][0]["director_choice"], true);
        assert_eq!(g["rigs"][0]["nodes"][1]["kind"], "group");
        assert_eq!(g["rigs"][0]["nodes"][2]["path"], json!([1, 0]));
        assert_eq!(g["rigs"][0]["nodes"][0]["live_skipped"], Value::Null);
        assert!(g["rigs"][0]["nodes"][0]["rect"].is_array());
        assert_eq!(g["edges"][0]["blend"], "smooth");
        assert_eq!(g["edges"][0]["to_rigs"], json!([1]));
        assert!(g["width"].as_f64().is_some_and(|w| w > 0.0));
        assert_eq!(
            reason(panel_graph(&mut world, &json!({ "asset": "none" }))),
            "camera_unknown_asset"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edits_go_stale_apply_activates_and_save_writes_a_file_that_loads() {
        let (mut world, dir) = project("camera-panel-edit");
        write(&dir, "p.camera.ron", GOOD);
        panel_load(&mut world, &json!({ "path": "p.camera.ron" })).expect("load");
        let edited = panel_edit(&mut world, &json!({ "param": "len", "value": 9.5 })).expect("edit");
        assert_eq!(edited["status"], "stale");
        assert_eq!(get(&mut world)["assets"][0]["status"], "stale");
        let active_len = |world: &World| {
            let assets = world.resource::<CameraAssets>();
            assets.active("player").and_then(|a| a.params["len"].as_f32())
        };
        assert_eq!(active_len(&world), Some(4.0), "drivers keep the active version");
        let applied = panel_apply(&mut world, &json!({})).expect("apply");
        assert_eq!(
            (applied["status"].clone(), applied["revision"].clone()),
            (json!("warnings"), json!(2))
        );
        assert_eq!(active_len(&world), Some(9.5));

        let group = panel_edit(
            &mut world,
            &json!({ "rig": "walk", "node": [1], "enabled": false }),
        )
        .expect("switch");
        assert_eq!(group["status"], "stale");
        let g = panel_graph(&mut world, &json!({})).expect("graph");
        assert_eq!(g["rigs"][0]["nodes"][1]["switched_off"], true);

        let saved = panel_save(&mut world, &json!({})).expect("save");
        assert_eq!(saved["path"], "p.camera.ron");
        let text = std::fs::read_to_string(dir.join("assets/p.camera.ron")).expect("read");
        let back = CameraAsset::from_ron(&text).expect("the file loads");
        assert_eq!(back.params["len"].as_f32(), Some(9.5));
        assert!(matches!(
            camera_graph::node_at(&back.rigs[0], &[1]),
            Some(Node::Group {
                enabled: NodeValue::Literal(false),
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_edits_and_saves_are_refused_with_codes() {
        let (mut world, dir) = project("camera-panel-bad-edit");
        write(&dir, "p.camera.ron", GOOD);
        panel_load(&mut world, &json!({ "path": "p.camera.ron" })).expect("load");
        for (params, code) in [
            (json!({ "param": "nope", "value": 1.0 }), "camera_unknown_name"),
            (json!({ "param": "len", "value": "text" }), "camera_bad_value"),
            (json!({ "param": "shoulder", "value": [1.0] }), "camera_bad_value"),
            (
                json!({ "rig": "walk", "node": [0], "enabled": false }),
                "camera_panel_unknown_node",
            ),
            (
                json!({ "rig": "none", "node": [0], "enabled": false }),
                "camera_panel_unknown_node",
            ),
            (
                json!({ "asset": "none", "param": "len", "value": 1.0 }),
                "camera_unknown_asset",
            ),
        ] {
            assert_eq!(reason(panel_edit(&mut world, &params)), code, "{params}");
        }
        for params in [json!({}), json!({ "rig": "walk" }), json!({ "param": "len" })] {
            let e = panel_edit(&mut world, &params).expect_err("refused");
            assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS, "{params}");
        }
        // A rig that names a missing rig is a validation error: nothing is written.
        world.resource_mut::<CameraAssets>().edit("player", |asset| {
            asset.director = regia_framework::camera::asset::DirectorDef::Fixed("missing".into());
        });
        let before = std::fs::read_to_string(dir.join("assets/p.camera.ron")).expect("read");
        assert_eq!(
            reason(panel_save(&mut world, &json!({}))),
            "camera_panel_has_errors"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("assets/p.camera.ron")).expect("read"),
            before
        );
        assert_eq!(
            world.resource::<CameraAssets>().revision("player"),
            1,
            "the last good stays"
        );
        // No file to save to.
        let hooks = CameraHooks::default();
        let asset = CameraAsset::from_ron(&GOOD.replace("\"player\"", "\"loose\"")).expect("parses");
        world.resource_mut::<CameraAssets>().insert(asset, None, &hooks);
        assert_eq!(
            reason(panel_save(&mut world, &json!({ "asset": "loose" }))),
            "camera_panel_no_path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A play world with the framework and one driver, stepped by hand.
    fn play_app() -> (regia_app::prelude::App, Entity) {
        use regia_app::prelude::*;
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin));
        app.world_mut().resource_mut::<CameraAssets>().insert(
            CameraAsset::from_ron(GOOD).expect("parses"),
            None,
            &CameraHooks::default(),
        );
        let entity = app
            .world_mut()
            .spawn((
                regia_scene::Transform::default(),
                regia_scene::components::Name::new("Cam"),
                CameraDriver::new("player"),
            ))
            .id();
        app.update();
        (app, entity)
    }

    #[test]
    fn while_playing_drivers_are_live_and_calls_queue() {
        let (mut app, entity) = play_app();
        let (mut edit, dir) = project("camera-panel-live");
        write(&dir, "p.camera.ron", GOOD);
        panel_load(&mut edit, &json!({ "path": "p.camera.ron" })).expect("load");
        edit.insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            ..Default::default()
        });
        let pump_once = |edit: &mut World, play: &mut World| {
            let mut panel = edit.remove_resource::<CameraPanel>().unwrap_or_default();
            panel.run_pending(play, LiveSource::Play);
            panel.snapshot_drivers(play, LiveSource::Play);
            edit.insert_resource(panel);
        };
        pump_once(&mut edit, app.world_mut());
        let v = get(&mut edit);
        assert_eq!(v["source"], "play");
        assert_eq!(v["play_framework"], true);
        assert_eq!(v["play_assets"][0]["name"], "player");
        assert_eq!(v["drivers"][0]["name"], "Cam");
        assert_eq!(v["drivers"][0]["bound"], true);
        assert_eq!(v["drivers"][0]["primary"], "walk");
        assert_eq!(v["selected_driver"], entity.to_bits());
        assert_eq!(v["live"]["primary"], "walk");

        // The graph takes its colors from the driver running the asset on the page.
        let g = panel_graph(&mut edit, &json!({})).expect("graph");
        assert_eq!(g["rigs"][0]["live_weight"], 1.0);
        assert_eq!(g["rigs"][0]["nodes"][0]["live_skipped"], false);
        assert_eq!(g["rigs"][1]["live_weight"], Value::Null);

        let queued = panel_driver(
            &mut edit,
            &json!({ "op": "set_var", "name": "aiming", "value": true }),
        )
        .expect("queued");
        assert_eq!(queued["queued"], true);
        pump_once(&mut edit, app.world_mut());
        let v = get(&mut edit);
        assert_eq!(v["last_call"]["ok"], true);
        assert_eq!(v["live"]["variables"]["aiming"], true);
        app.update();
        pump_once(&mut edit, app.world_mut());
        assert_eq!(
            get(&mut edit)["live"]["primary"],
            "aim",
            "the director follows the variable"
        );

        // A driver call's failure comes back through the record.
        panel_driver(
            &mut edit,
            &json!({ "op": "set_var", "name": "nope", "value": true }),
        )
        .expect("queued");
        pump_once(&mut edit, app.world_mut());
        let v = get(&mut edit);
        assert_eq!(v["last_call"]["reason"], "camera_unknown_name");

        let select = panel_select(&mut edit, &json!({ "driver": entity.to_bits() })).expect("select");
        assert_eq!(select["selected_driver"], entity.to_bits());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn driver_calls_and_selection_refuse_with_codes_outside_play() {
        let mut world = World::new();
        assert_eq!(
            reason(panel_driver(&mut world, &json!({ "op": "set_var" }))),
            "camera_no_driver"
        );
        assert_eq!(reason(panel_driver(&mut world, &json!({ "op": "x" }))), "bad_op");
        assert_eq!(
            reason(panel_select(&mut world, &json!({ "asset": "none" }))),
            "camera_unknown_asset"
        );
        let plain = world.spawn_empty().id();
        assert_eq!(
            reason(panel_select(&mut world, &json!({ "driver": plain.to_bits() }))),
            "camera_no_driver"
        );
        assert_eq!(
            reason(panel_apply(&mut world, &json!({}))),
            "camera_unknown_asset"
        );
        assert_eq!(reason(panel_save(&mut world, &json!({}))), "camera_unknown_asset");
    }

    fn draw_frame(world: &mut World) -> usize {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| camera_panel(ui, world));
        output.textures_delta.clear();
        ctx.tessellate(output.shapes, output.pixels_per_point).len()
    }

    #[test]
    fn the_panel_draws_headless_on_every_tab_and_its_first_draw_scans() {
        let mut empty = World::new();
        for tab in [CameraTab::Asset, CameraTab::Graph, CameraTab::Live] {
            empty.resource_mut_or_panel().tab = tab;
            assert!(draw_frame(&mut empty) > 0, "{tab:?}, nothing loaded");
        }
        let (mut world, dir) = project("camera-panel-draw");
        write(&dir, "p.camera.ron", GOOD);
        write(&dir, "bad.camera.ron", "(");
        assert!(draw_frame(&mut world) > 0);
        assert_eq!(
            world.resource::<CameraAssets>().names().collect::<Vec<_>>(),
            ["player"],
            "the first draw scanned the project"
        );
        for tab in [CameraTab::Asset, CameraTab::Graph, CameraTab::Live] {
            world.resource_mut::<CameraPanel>().tab = tab;
            assert!(draw_frame(&mut world) > 0, "{tab:?}");
        }
        // The graph's details, with a Group chosen and with a stale path.
        {
            let mut panel = world.resource_mut::<CameraPanel>();
            panel.tab = CameraTab::Graph;
            panel.selected_node = Some((0, vec![1]));
        }
        assert!(draw_frame(&mut world) > 0);
        world.resource_mut::<CameraPanel>().selected_node = Some((0, vec![9]));
        assert!(draw_frame(&mut world) > 0);
        assert_eq!(world.resource::<CameraPanel>().selected_node, None);
        // The live tab with a running driver on the panel.
        let (mut app, _) = play_app();
        world.resource_mut::<CameraPanel>().tab = CameraTab::Live;
        world.insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            ..Default::default()
        });
        let mut panel = world.remove_resource::<CameraPanel>().expect("panel");
        panel.snapshot_drivers(app.world_mut(), LiveSource::Play);
        world.insert_resource(panel);
        assert!(draw_frame(&mut world) > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_method_answers_by_its_registered_name() {
        let list = methods();
        assert_eq!(list.len(), 8);
        let (mut world, dir) = project("camera-panel-names");
        write(&dir, "p.camera.ron", GOOD);
        for (schema, handler) in &list {
            let params = match schema.name {
                "editor.camera_panel_load" => json!({ "path": "p.camera.ron" }),
                "editor.camera_panel_edit" => json!({ "param": "len", "value": 2.0 }),
                _ => json!({}),
            };
            if schema.name == "editor.camera_panel_driver" {
                // No driver in the edit world: an answer with a code is a real dispatch.
                let e = handler(&mut world, &json!({ "op": "set_var" })).expect_err("no driver");
                assert_eq!(reason_of(&e).as_deref(), Some("camera_no_driver"));
                continue;
            }
            handler(&mut world, &params).unwrap_or_else(|e| panic!("{}: {e:?}", schema.name));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `resource_mut` that makes the panel first (the empty-world draw above).
    trait PanelOrDefault {
        fn resource_mut_or_panel(&mut self) -> Mut<'_, CameraPanel>;
    }

    impl PanelOrDefault for World {
        fn resource_mut_or_panel(&mut self) -> Mut<'_, CameraPanel> {
            if !self.contains_resource::<CameraPanel>() {
                self.init_resource::<CameraPanel>();
            }
            self.resource_mut::<CameraPanel>()
        }
    }
}
