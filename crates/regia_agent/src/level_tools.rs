// SPDX-License-Identifier: MPL-2.0
//! `level.storage`, `level.convert`, `level.layers`, `level.set_layer_state`,
//! `level.set_membership`, `level.placed`, `level.place_level`, `level.break_placed`: the agent
//! surface of one-file-per-entity levels, data layers and placed levels
//! (`docs/specs/37-per-entity-levels-layers-instances.md` § 8, book/agents/levels-layers-instances.md).
//! Also `level.switch` and `level.switch_status`: the level switch with cancel (ADR-8027,
//! book/agents/level-switch.md).
//!
//! Refusals are `F0002` with `data.reason` (spec 37 § 9). Without the scene plugin the reads
//! answer empty lists.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Quat;
use regia_scene::components::EntityGuid;
use regia_scene::layers::{LayerMembership, LayerState, LayerTable, Side};
use regia_scene::level::{LevelId, LevelLibrary, LevelManager, LevelMember};
use regia_scene::level_dir::{self, LevelDirtyTracker, LevelForm};
use regia_scene::placed::{self, PlacedError, PlacedLevel, PlacedMode, PlacedSpawned};
use regia_scene::transform::Transform;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most entities one `level.set_membership` call may edit.
const MAX_MEMBERSHIP_ENTITIES: usize = 4096;

/// `F0002` with `data.reason`.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn project_root(world: &World) -> PathBuf {
    world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .or_else(|| {
            world
                .get_resource::<crate::AgentSettings>()
                .and_then(|s| s.project_dir.clone())
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// A project-relative level path (no `..`, not absolute), resolved under the project root.
fn level_path(world: &World, params: &Value, key: &str) -> Result<(String, PathBuf), RpcError> {
    let text = params.get(key).and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params(format!("`{key}` (a project-relative level path) is required"))
    })?;
    // Text rules plus symlinks: `level.convert` deletes the source form, so the level's file and
    // directory must both stay inside the project (ADR-1816).
    let root = project_root(world);
    let path = crate::req_path::project_path_in(&root, key, text)?;
    let (file, dir) = level_dir::level_paths(&path);
    crate::req_path::check_derived(&root, key, &file)?;
    crate::req_path::check_derived(&root, key, &dir)?;
    Ok((text.to_owned(), path))
}

fn dir_error(err: &level_dir::LevelDirError) -> RpcError {
    refused(
        err.reason(),
        err.to_string(),
        "see book/agents/levels-layers-instances.md",
    )
}

fn paths_json(root: &Path, paths: &[PathBuf]) -> Value {
    json!(
        paths
            .iter()
            .map(|p| p
                .strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/"))
            .collect::<Vec<_>>()
    )
}

/// The editor's dirty set for a level path (empty without a tracker).
fn dirty_of(world: &World, path: &Path) -> level_dir::LevelDirtySet {
    let (file, dir) = level_dir::level_paths(path);
    world
        .get_resource::<LevelDirtyTracker>()
        .and_then(|t| t.get(&file).or_else(|| t.get(&dir)).cloned())
        .unwrap_or_default()
}

/// `level.storage {level}`: the form a level is stored in and the editor's dirty set.
pub(crate) fn storage(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (text, path) = level_path(world, params, "level")?;
    let form = level_dir::level_form(&path).map_err(|e| dir_error(&e))?;
    let (file, dir) = level_dir::level_paths(&path);
    let (entities, files) = match form {
        LevelForm::Dir => {
            let (scene, report) = level_dir::read_level_dir(&dir).map_err(|e| dir_error(&e))?;
            (scene.entities.len(), report.files_read)
        }
        LevelForm::File => {
            let scene = level_dir::read_level_source(&file).map_err(|e| dir_error(&e))?;
            (scene.entities.len(), 1)
        }
    };
    let dirty = dirty_of(world, &path);
    Ok(json!({
        "level": text,
        "form": form.as_str(),
        "entities": entities,
        "files": files,
        "header_dirty": dirty.header,
        "dirty_entities": dirty.entities.iter().map(|g| g.to_string()).collect::<Vec<_>>(),
    }))
}

/// `level.convert {path, to: "file"|"dir"}`: lossless conversion; the source form is removed.
pub(crate) fn convert(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (_, path) = level_path(world, params, "path")?;
    let to = match crate::params::opt_str(params, "to")? {
        Some("file") => LevelForm::File,
        Some("dir") => LevelForm::Dir,
        _ => return Err(RpcError::invalid_params("`to` must be \"file\" or \"dir\"")),
    };
    let form = level_dir::level_form(&path).map_err(|e| dir_error(&e))?;
    if !dirty_of(world, &path).is_empty() {
        return Err(refused(
            "level_dirty",
            "the editor has unsaved changes to this level",
            "save the level first",
        ));
    }
    let root = project_root(world);
    let (file, dir) = level_dir::level_paths(&path);
    if form == to {
        return Ok(json!({ "written": [], "deleted": [], "unchanged": 0, "form": to.as_str() }));
    }
    match to {
        LevelForm::Dir => {
            let report = level_dir::split_level_file(&file, &dir).map_err(|e| dir_error(&e))?;
            std::fs::remove_file(&file)
                .map_err(|e| RpcError::internal(format!("{}: {e}", file.display())))?;
            let mut deleted = report.deleted.clone();
            deleted.push(file);
            Ok(json!({
                "written": paths_json(&root, &report.written),
                "deleted": paths_json(&root, &deleted),
                "unchanged": report.unchanged,
                "form": "dir",
            }))
        }
        LevelForm::File => {
            level_dir::merge_level_dir(&dir, &file).map_err(|e| dir_error(&e))?;
            std::fs::remove_dir_all(&dir)
                .map_err(|e| RpcError::internal(format!("{}: {e}", dir.display())))?;
            Ok(json!({
                "written": paths_json(&root, std::slice::from_ref(&file)),
                "deleted": paths_json(&root, std::slice::from_ref(&dir)),
                "unchanged": 0,
                "form": "file",
            }))
        }
    }
}

fn layer_json(info: &regia_scene::layers::LayerInfo<'_>) -> Value {
    json!({
        "name": info.name,
        "kind": format!("{:?}", info.kind),
        "parent": info.parent,
        "requested": info.requested.as_str(),
        "effective": info.effective.as_str(),
        "members": info.member_count,
        "peers": format!("{:?}", info.peers),
    })
}

/// `level.layers {layer?}`: every layer (ordered by name), or one, with how many members are
/// despawned (absent, a template record waits) and parked now.
pub(crate) fn layers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let parked = {
        let mut query = world.query::<&regia_scene::layers::LayerParked>();
        query.iter(world).filter(|marker| !marker.inherited).count()
    };
    let wanted = crate::params::opt_str(params, "layer")?;
    let Some(table) = world.get_resource::<LayerTable>() else {
        // No scene plugin: the list is empty, and a named layer is as unknown as it would be in
        // an empty table.
        if let Some(name) = wanted {
            return Err(refused(
                "layer_unknown",
                format!("no layer `{name}` (no layers)"),
                "level.layers lists them",
            ));
        }
        return Ok(json!({ "layers": [], "despawned": 0, "parked": parked }));
    };
    let list: Vec<Value> = table
        .layers()
        .filter(|l| wanted.is_none_or(|w| w == l.name))
        .map(|l| layer_json(&l))
        .collect();
    if let Some(name) = wanted
        && list.is_empty()
    {
        return Err(refused(
            "layer_unknown",
            format!("no layer `{name}`"),
            "level.layers lists them",
        ));
    }
    Ok(json!({ "layers": list, "despawned": table.despawned_count(), "parked": parked }))
}

/// `level.set_layer_state {layer, state, recursive?, side?}`.
pub(crate) fn set_layer_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let layer = params
        .get("layer")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`layer` is required"))?;
    let state = crate::params::opt_str(params, "state")?
        .and_then(LayerState::parse)
        .ok_or_else(|| RpcError::invalid_params("`state` must be \"absent\", \"parked\" or \"live\""))?;
    let recursive = crate::params::opt_bool(params, "recursive")?.unwrap_or(false);
    let side = match crate::params::opt_str(params, "side")? {
        None | Some("local") => Side::Local,
        Some("server") => Side::Server,
        Some("client") => Side::Client,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`side` must be local, server or client, not {other}"
            )));
        }
    };
    let Some(mut table) = world.get_resource_mut::<LayerTable>() else {
        return Err(refused(
            "layer_unknown",
            format!("no layer `{layer}` (no layers)"),
            "add ScenePlugin",
        ));
    };
    let owner = table.peers(layer).is_some_and(|p| side.owns(p));
    let changed = table
        .request(layer, state, recursive, side)
        .map_err(|e| refused(e.reason(), e.to_string(), "level.layers lists them"))?;
    let effective = table.effective(layer).map(LayerState::as_str);
    let mut out = json!({ "layer": layer, "changed": changed, "effective": effective });
    if !owner {
        out["reason"] = json!("side_not_owner");
    }
    Ok(out)
}

fn entities_param(params: &Value) -> Result<Vec<Entity>, RpcError> {
    let ids: Vec<Value> = match (params.get("entity"), params.get("entities")) {
        (Some(one), None) => vec![one.clone()],
        (None, Some(Value::Array(many))) => many.clone(),
        _ => return Err(RpcError::invalid_params("pass `entity` or `entities` (an array)")),
    };
    if ids.len() > MAX_MEMBERSHIP_ENTITIES {
        return Err(RpcError::invalid_params(format!(
            "at most {MAX_MEMBERSHIP_ENTITIES} entities per call"
        )));
    }
    ids.iter()
        .map(|v| {
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params(format!("{v} is not an entity id")))
        })
        .collect()
}

fn names_param(params: &Value, key: &str) -> Result<Vec<String>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_owned).ok_or_else(|| {
                    RpcError::invalid_params(format!("`{key}` must be an array of layer names"))
                })
            })
            .collect(),
        Some(_) => Err(RpcError::invalid_params(format!(
            "`{key}` must be an array of layer names"
        ))),
    }
}

/// `level.set_membership {entity|entities, add?, remove?}`: edits `LayerMembership`.
pub(crate) fn set_membership(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = entities_param(params)?;
    let add = names_param(params, "add")?;
    let remove = names_param(params, "remove")?;
    if let Some(bad) = add.iter().find(|n| !regia_scene::layers::valid_layer_name(n)) {
        return Err(RpcError::invalid_params(format!(
            "`{bad}` is not a layer name ([a-z0-9_]{{1,48}})"
        )));
    }
    for entity in &entities {
        if world.get_entity(*entity).is_err() {
            return Err(RpcError::not_found(
                format!("entity {} does not exist", entity.to_bits()),
                "use scene.query_entities",
            ));
        }
    }
    let mut updated = 0;
    for entity in entities {
        let mut membership = world.get::<LayerMembership>(entity).cloned().unwrap_or_default();
        let before = membership.clone();
        membership.layers.retain(|l| !remove.contains(l));
        for name in &add {
            if !membership.layers.contains(name) {
                membership.layers.push(name.clone());
            }
        }
        if membership != before
            && let Ok(mut e) = world.get_entity_mut(entity)
        {
            e.insert(membership);
            updated += 1;
        }
    }
    if let Some(mut table) = world.get_resource_mut::<LayerTable>() {
        table.mark_dirty();
    }
    Ok(json!({ "updated": updated }))
}

/// `level.placed`: every placed-level instance.
pub(crate) fn placed_list(world: &mut World) -> Result<Value, RpcError> {
    let mut rows: Vec<(u64, Value)> = Vec::new();
    let mut query = world.query::<(Entity, &PlacedLevel, Option<&PlacedSpawned>, Option<&PlacedError>)>();
    for (entity, placed, spawned, error) in query.iter(world) {
        let mut row = json!({
            "entity": entity.to_bits(),
            "level": placed.level,
            "mode": format!("{:?}", placed.mode).to_lowercase(),
            "spawned": spawned.map_or(0, |s| s.entities),
        });
        if let Some(error) = error {
            row["error"] = json!({ "reason": error.reason, "detail": error.detail });
        }
        rows.push((entity.to_bits(), row));
    }
    rows.sort_by_key(|(bits, _)| *bits);
    Ok(json!({ "placed": rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>() }))
}

fn vec3(value: Option<&Value>, default: [f32; 3]) -> Result<[f32; 3], RpcError> {
    let Some(value) = value else { return Ok(default) };
    let items = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params("vectors are [x, y, z]"))?;
    let mut out = [0.0; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let n = item
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params("vector items are numbers"))? as f32;
        if !n.is_finite() {
            return Err(RpcError::invalid_params("vector items must be finite"));
        }
        *slot = n;
    }
    Ok(out)
}

/// A rotation `[x, y, z, w]` (finite, non-zero length; normalized here), or `Quat::IDENTITY`.
fn quat4(value: Option<&Value>) -> Result<Quat, RpcError> {
    let Some(value) = value else {
        return Ok(Quat::IDENTITY);
    };
    let items = value
        .as_array()
        .filter(|a| a.len() == 4)
        .ok_or_else(|| RpcError::invalid_params("`rotation` is a quaternion [x, y, z, w]"))?;
    let mut out = [0.0_f32; 4];
    for (slot, item) in out.iter_mut().zip(items) {
        let n = item
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params("rotation items are numbers"))? as f32;
        if !n.is_finite() {
            return Err(RpcError::invalid_params("rotation items must be finite"));
        }
        *slot = n;
    }
    let q = Quat::from_array(out);
    let len = q.length();
    if !len.is_finite() || len < 1e-6 {
        return Err(RpcError::invalid_params("`rotation` must have a non-zero length"));
    }
    Ok(q / len)
}

/// `level.place_level {level, transform?, mode?, parent?}`: spawns an instance entity and fills
/// it at once. `transform` is `{ translation?, rotation?, scale? }`.
pub(crate) fn place_level(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("level")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`level` (asset:<guid>) is required"))?;
    let guid: Guid = text
        .strip_prefix("asset:")
        .unwrap_or(text)
        .parse()
        .map_err(|_| RpcError::invalid_params(format!("`{text}` is not a level asset id")))?;
    let level = LevelId(guid);
    let mode = match crate::params::opt_str(params, "mode")? {
        None | Some("inline") => PlacedMode::Inline,
        Some("streamed") => PlacedMode::Streamed,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`mode` must be inline or streamed, not {other}"
            )));
        }
    };
    let transform = params.get("transform").cloned().unwrap_or(Value::Null);
    let translation = vec3(transform.get("translation"), [0.0; 3])?;
    let rotation = quat4(transform.get("rotation"))?;
    let scale = vec3(transform.get("scale"), [1.0; 3])?;
    let parent = match params.get("parent") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let entity = v
                .as_u64()
                .and_then(Entity::try_from_bits)
                .filter(|e| world.get_entity(*e).is_ok())
                .ok_or_else(|| {
                    RpcError::not_found(format!("parent {v} does not exist"), "use scene.query_entities")
                })?;
            Some(entity)
        }
    };
    let library = world.get_resource::<LevelLibrary>().cloned().unwrap_or_default();
    if !library.contains(level) {
        return Err(refused(
            "level_not_found",
            format!("level {text} is not loaded"),
            "load the level asset first",
        ));
    }
    let host = world
        .get_resource::<LevelManager>()
        .and_then(LevelManager::current);
    let chain: Vec<LevelId> = host.into_iter().collect();
    // The same check the fill makes: a prefab-instance record counts what it instantiates, so a
    // placement the fill would refuse (`placed_limit`) is refused here, before anything spawns.
    let check = match world.get_resource::<regia_scene::prefab::PrefabLibrary>() {
        Some(prefabs) => placed::check_placement_with_prefabs(&library, prefabs, level, &chain),
        None => placed::check_placement(&library, level, &chain),
    };
    check.map_err(|e| {
        refused(
            e.reason(),
            e.to_string(),
            "a level cannot contain itself; nest at most 8 deep",
        )
    })?;
    let mut placed = PlacedLevel::inline(level);
    placed.mode = mode;
    let mut entity = world.spawn((
        EntityGuid(Guid::new_random()),
        Transform {
            rotation,
            scale: scale.into(),
            ..Transform::from_translation(translation.into())
        },
        placed,
    ));
    if let Some(host) = host {
        entity.insert(LevelMember::from_level(host));
    }
    if let Some(parent) = parent {
        entity.insert(ChildOf(parent));
    }
    let id = entity.id();
    placed::spawn_placed_levels(world);
    if let Some(error) = world.get::<PlacedError>(id) {
        return Err(refused(error.reason, error.detail.clone(), "see level.placed"));
    }
    let spawned = world.get::<PlacedSpawned>(id).map_or(0, |s| s.entities);
    Ok(json!({ "entity": id.to_bits(), "spawned": spawned }))
}

/// `level.break_placed {entity}`: plain entities in place of an inline instance.
pub(crate) fn break_placed(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(refused(
            "placed_not_found",
            format!("entity {} does not exist", entity.to_bits()),
            "level.placed lists instances",
        ));
    }
    let guids = placed::break_placed(world, entity)
        .map_err(|e| refused(e.reason(), e.to_string(), "level.placed lists inline instances"))?;
    Ok(json!({ "entities": guids.iter().map(Guid::to_string).collect::<Vec<_>>() }))
}

fn level_switch_bad_params(detail: &str) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "level_switch_bad_params",
        detail,
        "pass { \"path\": \"levels/next.level.ron\" } to switch, or { \"cancel\": true } to stop the read in \
         flight (book/agents/level-switch.md)",
    )
}

fn no_level_switch() -> RpcError {
    RpcError::not_found(
        "no LevelSwitch: the level switch plugin is not installed",
        "add regia_framework::level_switch::LevelSwitchPlugin to the app (the regia default plugins do)",
    )
}

/// The `level.switch_status` answer.
fn switch_status_json(world: &World) -> Result<Value, RpcError> {
    use regia_framework::level_switch::{LevelSwitch, LevelSwitchOutcome};
    let switch = world.get_resource::<LevelSwitch>().ok_or_else(no_level_switch)?;
    let current = world
        .get_resource::<LevelManager>()
        .and_then(LevelManager::current)
        .map(|id| format!("asset:{}", id.0));
    let last = switch.last().map(|outcome| match outcome {
        LevelSwitchOutcome::Opened {
            path,
            level,
            replaced,
        } => json!({
            "outcome": "opened",
            "path": path.to_string_lossy().replace('\\', "/"),
            "level": format!("asset:{}", level.0),
            "replaced": replaced.map(|id| format!("asset:{}", id.0)),
        }),
        LevelSwitchOutcome::Failed { path, error } => {
            json!({ "outcome": "failed", "path": path.replace('\\', "/"), "error": error })
        }
        LevelSwitchOutcome::Cancelled { path } => {
            json!({ "outcome": "cancelled", "path": path.replace('\\', "/") })
        }
        _ => json!({ "outcome": "other" }),
    });
    let stats = switch.stats();
    Ok(json!({
        "loading": switch.loading().map(|path| path.replace('\\', "/")),
        "current": current,
        "last": last,
        "retiring": switch.retiring(),
        "requests": switch.requests(),
        "stats": {
            "loads_started": stats.loads_started,
            "loads_skipped": stats.loads_skipped,
            "loads_cancelled": stats.loads_cancelled,
            "loads_landed": stats.loads_landed,
            "loads_failed": stats.loads_failed,
            "retires": stats.retires,
            "max_wait_ms": stats.max_wait_ms,
            "last_wait_ms": stats.last_wait_ms,
        },
    }))
}

/// `level.switch {path} | {cancel: true}`: starts reading a project level on a worker and opens it
/// with `Replace` when it lands (a read in flight is cancelled), or cancels the read in flight and
/// keeps the open level (ADR-8027, book/agents/level-switch.md). Answers `level.switch_status`.
pub(crate) fn switch(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_framework::level_switch::LevelSwitch;
    let cancel = crate::params::opt_bool(params, "cancel")
        .map_err(|_| level_switch_bad_params("`cancel` must be true or false"))?
        .unwrap_or(false);
    let has_path = params.get("path").is_some();
    if cancel == has_path {
        return Err(level_switch_bad_params(if cancel {
            "give either `path` or `cancel`, not both"
        } else {
            "give `path` (a project-relative level) or `cancel: true`"
        }));
    }
    if !world.contains_resource::<LevelSwitch>() {
        return Err(no_level_switch());
    }
    if cancel {
        world.resource_mut::<LevelSwitch>().cancel();
    } else {
        if !world.contains_resource::<LevelManager>() {
            return Err(RpcError::not_found(
                "no LevelManager: the scene plugin is not installed",
                "add regia_scene::ScenePlugin to the app",
            ));
        }
        let (_, path) = level_path(world, params, "path")?;
        let (file, dir) = level_dir::level_paths(&path);
        if !file.exists() && !dir.exists() {
            return Err(refused(
                "level_not_found",
                format!("no level at {}", path.display()),
                "pass a project-relative .level.ron or .level.d path",
            ));
        }
        world.resource_mut::<LevelSwitch>().request(path);
    }
    switch_status_json(world)
}

/// `level.switch_status {}`: the read in flight, the open level, how the last switch ended, and
/// the switch's counters.
pub(crate) fn switch_status(world: &mut World) -> Result<Value, RpcError> {
    switch_status_json(world)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::format::{SCENE_FORMAT_VERSION, SceneFile};
    use regia_scene::layers::LayerPeers;
    use regia_scene::level::Level;

    fn reason(err: &RpcError) -> Option<&str> {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
    }

    fn world_in(project: &Path) -> World {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        std::mem::take(app.world_mut())
    }

    const LEVEL: &str = r#"SceneFile(format_version: 10, entities: [
        (guid: "00000000-0000-0000-0000-0000000000a1", components: {}),
        (guid: "00000000-0000-0000-0000-0000000000a2", parent: Some("00000000-0000-0000-0000-0000000000a1"), components: {}),
    ])"#;

    #[test]
    fn storage_and_convert_round_trip() {
        let project = regia_test_utils::unique_temp_dir("agent-level-storage");
        std::fs::create_dir_all(project.join("levels")).expect("mkdir");
        std::fs::write(project.join("levels/yard.level.ron"), LEVEL).expect("write");
        let mut world = world_in(&project);

        let out = storage(&mut world, &json!({ "level": "levels/yard.level.ron" })).expect("storage");
        assert_eq!(out["form"], "file");
        assert_eq!(out["entities"], 2);

        let out = convert(
            &mut world,
            &json!({ "path": "levels/yard.level.ron", "to": "dir" }),
        )
        .expect("to dir");
        assert_eq!(out["written"].as_array().map(Vec::len), Some(3));
        let out = storage(&mut world, &json!({ "level": "levels/yard" })).expect("storage dir");
        assert_eq!(
            (out["form"].as_str(), out["files"].as_u64()),
            (Some("dir"), Some(3))
        );

        // A dirty level is refused.
        let mut tracker = LevelDirtyTracker::default();
        tracker.mark_header(project.join("levels/yard.level.d"));
        world.insert_resource(tracker);
        let err = convert(
            &mut world,
            &json!({ "path": "levels/yard.level.d", "to": "file" }),
        )
        .expect_err("dirty");
        assert_eq!(reason(&err), Some("level_dirty"));
        world.remove_resource::<LevelDirtyTracker>();

        convert(
            &mut world,
            &json!({ "path": "levels/yard.level.d", "to": "file" }),
        )
        .expect("to file");
        let text = std::fs::read_to_string(project.join("levels/yard.level.ron")).expect("merged");
        assert!(SceneFile::from_ron_str(&text).is_ok());

        // Reasons and parameters.
        let err = storage(&mut world, &json!({ "level": "levels/none.level.ron" })).expect_err("missing");
        assert_eq!(reason(&err), Some("level_not_found"));
        std::fs::create_dir_all(project.join("levels/yard.level.d")).expect("mkdir");
        std::fs::write(project.join("levels/yard.level.d/level.ron"), "x").expect("w");
        let err = storage(&mut world, &json!({ "level": "levels/yard.level.ron" })).expect_err("ambiguous");
        assert_eq!(reason(&err), Some("ambiguous_level_form"));
        assert!(storage(&mut world, &json!({})).is_err());
        assert!(storage(&mut world, &json!({ "level": "../x.level.ron" })).is_err());
        assert!(
            convert(
                &mut world,
                &json!({ "path": "levels/yard.level.ron", "to": "zip" })
            )
            .is_err()
        );
    }

    /// `level.convert` deletes the source form: a level reached through a symlink out of the
    /// project, or an absolute or drive path, is refused before anything is read or deleted
    /// (ADR-1816).
    #[test]
    fn convert_refuses_levels_outside_the_project() {
        let base = regia_test_utils::unique_temp_dir("agent-level-escape");
        let (project, outside) = (base.join("project"), base.join("outside"));
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("yard.level.ron"), LEVEL).expect("write");
        let mut world = world_in(&project);
        let abs = outside.join("yard.level.ron").display().to_string();
        for p in [
            abs.as_str(),
            "C:/yard.level.ron",
            "\\\\host\\share\\yard.level.ron",
        ] {
            let err = convert(&mut world, &json!({ "path": p, "to": "dir" })).expect_err(p);
            assert_eq!(reason(&err), Some("path_outside_project"), "{p}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, project.join("levels")).expect("link");
            let err = convert(
                &mut world,
                &json!({ "path": "levels/yard.level.ron", "to": "dir" }),
            )
            .expect_err("symlinked");
            assert_eq!(reason(&err), Some("path_outside_project"));
            let err = storage(&mut world, &json!({ "level": "levels/yard.level.ron" })).expect_err("read");
            assert_eq!(reason(&err), Some("path_outside_project"));
        }
        assert!(
            outside.join("yard.level.ron").is_file(),
            "nothing deleted outside"
        );
        assert!(!outside.join("yard.level.d").exists(), "nothing written outside");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn layer_reads_and_writes() {
        let project = regia_test_utils::unique_temp_dir("agent-level-layers");
        let mut world = world_in(&project);
        assert_eq!(
            layers(&mut world, &json!({})).expect("empty")["layers"],
            json!([])
        );
        {
            let mut table = world.resource_mut::<LayerTable>();
            table.insert_layer("quest_a", None, LayerState::Live, LayerPeers::Everyone);
            table.insert_layer("server_only", None, LayerState::Live, LayerPeers::ServerOnly);
        }
        let all = layers(&mut world, &json!({})).expect("layers");
        assert_eq!(all["layers"][0]["name"], "quest_a");
        assert_eq!(all["layers"][1]["effective"], "live");
        let err = layers(&mut world, &json!({ "layer": "nope" })).expect_err("unknown");
        assert_eq!(reason(&err), Some("layer_unknown"));

        let out =
            set_layer_state(&mut world, &json!({ "layer": "quest_a", "state": "parked" })).expect("set");
        assert_eq!(
            (out["changed"].as_bool(), out["effective"].as_str()),
            (Some(true), Some("parked"))
        );
        let out =
            set_layer_state(&mut world, &json!({ "layer": "quest_a", "state": "loaded" })).expect("alias");
        assert_eq!(out["changed"], false);
        let out = set_layer_state(
            &mut world,
            &json!({ "layer": "server_only", "state": "absent", "side": "client" }),
        )
        .expect("not owner");
        assert_eq!(
            (out["changed"].as_bool(), out["reason"].as_str()),
            (Some(false), Some("side_not_owner"))
        );
        let err =
            set_layer_state(&mut world, &json!({ "layer": "x", "state": "live" })).expect_err("unknown");
        assert_eq!(reason(&err), Some("layer_unknown"));
        assert!(set_layer_state(&mut world, &json!({ "layer": "quest_a" })).is_err());
        assert!(set_layer_state(&mut world, &json!({ "layer": "quest_a", "state": "on" })).is_err());

        let e = world.spawn(EntityGuid(Guid::from_u128(5))).id();
        let out =
            set_membership(&mut world, &json!({ "entity": e.to_bits(), "add": ["quest_a"] })).expect("add");
        assert_eq!(out["updated"], 1);
        assert_eq!(
            world.get::<LayerMembership>(e).map(|m| m.layers.clone()),
            Some(vec!["quest_a".to_owned()])
        );
        let out = set_membership(
            &mut world,
            &json!({ "entities": [e.to_bits()], "remove": ["quest_a"] }),
        )
        .expect("rm");
        assert_eq!(out["updated"], 1);
        assert!(set_membership(&mut world, &json!({ "add": ["a"] })).is_err());
        assert!(set_membership(&mut world, &json!({ "entity": e.to_bits(), "add": ["Bad Name"] })).is_err());
    }

    #[test]
    fn layers_count_despawned_and_parked_members() {
        let project = regia_test_utils::unique_temp_dir("agent-level-layer-counts");
        let mut world = world_in(&project);
        let text = format!(
            r#"SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [
                (guid: "00000000-0000-0000-0000-0000000000b1", components: {{
                    "regia_scene::layers::LayerMembership": (layers: ["night"]),
                }}),
                (guid: "00000000-0000-0000-0000-0000000000b2", components: {{
                    "regia_scene::layers::LayerMembership": (layers: ["night"]),
                }}),
            ], resources: {{
                "regia_scene::layers::LevelLayers": (
                    embedded: [(format_version: 1, layers: [(name: "night", kind: Runtime)])],
                    entries: [(layer: "night", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone)],
                ),
            }})"#
        );
        world.resource_mut::<LevelLibrary>().insert(level(0xb0, &text));
        regia_scene::level::open_level(
            &mut world,
            LevelId(Guid::from_u128(0xb0)),
            regia_scene::level::LoadMode::Replace,
        )
        .expect("open");
        regia_scene::layers::apply_layer_switches(&mut world);
        let counts = |world: &mut World| {
            let out = layers(world, &json!({})).expect("layers");
            (
                out["despawned"].as_u64(),
                out["parked"].as_u64(),
                out["layers"][0]["members"].as_u64(),
            )
        };
        assert_eq!(counts(&mut world), (Some(0), Some(0), Some(2)));
        for (state, expected) in [
            ("parked", (Some(0), Some(2), Some(2))),
            ("absent", (Some(2), Some(0), Some(2))),
            ("live", (Some(0), Some(0), Some(2))),
        ] {
            set_layer_state(&mut world, &json!({ "layer": "night", "state": state })).expect("set");
            regia_scene::layers::apply_layer_switches(&mut world);
            assert_eq!(counts(&mut world), expected, "{state}");
        }
        // One layer asked for: the counts are still the world's.
        let one = layers(&mut world, &json!({ "layer": "night" })).expect("one");
        assert_eq!(one["despawned"], 0);
        let _ = std::fs::remove_dir_all(&project);
    }

    fn level(id: u128, text: &str) -> Level {
        Level::from_ron_str(LevelId(Guid::from_u128(id)), text).expect("level")
    }

    #[test]
    fn place_list_and_break() {
        let project = regia_test_utils::unique_temp_dir("agent-level-placed");
        let mut world = world_in(&project);
        assert_eq!(placed_list(&mut world).expect("empty")["placed"], json!([]));
        world.resource_mut::<LevelLibrary>().insert(level(0xa0, LEVEL));
        let self_placing = format!(
            r#"SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [
                (guid: "00000000-0000-0000-0000-0000000000c1", components: {{
                    "regia_scene::placed::PlacedLevel": (level: "asset:{}", mode: Inline, filter: (layers: [])),
                }}),
            ])"#,
            Guid::from_u128(0xc0)
        );
        world
            .resource_mut::<LevelLibrary>()
            .insert(level(0xc0, &self_placing));

        let out = place_level(
            &mut world,
            &json!({ "level": format!("asset:{}", Guid::from_u128(0xa0)), "transform": { "translation": [1, 2, 3] } }),
        )
        .expect("place");
        assert_eq!(out["spawned"], 2);
        let list = placed_list(&mut world).expect("list");
        assert_eq!(list["placed"][0]["spawned"], 2);
        assert_eq!(list["placed"][0]["mode"], "inline");

        let entity = out["entity"].as_u64().expect("id");
        let broken = break_placed(&mut world, &json!({ "entity": entity })).expect("break");
        assert_eq!(broken["entities"].as_array().map(Vec::len), Some(2));
        let err = break_placed(&mut world, &json!({ "entity": entity })).expect_err("gone");
        assert_eq!(reason(&err), Some("placed_not_found"));
        let plain = world.spawn(EntityGuid(Guid::from_u128(9))).id();
        let err = break_placed(&mut world, &json!({ "entity": plain.to_bits() })).expect_err("plain");
        assert_eq!(reason(&err), Some("not_placed"));

        let err = place_level(
            &mut world,
            &json!({ "level": format!("asset:{}", Guid::from_u128(0xc0)) }),
        )
        .expect_err("cycle");
        assert_eq!(reason(&err), Some("placed_cycle"));
        let err = place_level(
            &mut world,
            &json!({ "level": format!("asset:{}", Guid::from_u128(0xdd)) }),
        )
        .expect_err("missing");
        assert_eq!(reason(&err), Some("level_not_found"));
        assert!(place_level(&mut world, &json!({})).is_err());
        assert!(place_level(&mut world, &json!({ "level": "asset:x" })).is_err());
    }

    /// Spec 37 § 10 test 8: without the scene plugin the reads answer empty lists, a named layer
    /// is `layer_unknown` (as in an empty table), and a write is `layer_unknown`, never a panic.
    #[test]
    fn without_the_scene_plugin_reads_are_empty_and_writes_refused() {
        let project = regia_test_utils::unique_temp_dir("agent-level-noplugin");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(project.clone()),
        });
        let out = layers(&mut world, &json!({})).expect("empty");
        assert_eq!(out["layers"], json!([]));
        assert_eq!(
            (out["despawned"].as_u64(), out["parked"].as_u64()),
            (Some(0), Some(0))
        );
        let err = layers(&mut world, &json!({ "layer": "night" })).expect_err("named");
        assert_eq!(reason(&err), Some("layer_unknown"));
        let err =
            set_layer_state(&mut world, &json!({ "layer": "night", "state": "live" })).expect_err("no table");
        assert_eq!(reason(&err), Some("layer_unknown"));
        assert_eq!(placed_list(&mut world).expect("placed")["placed"], json!([]));
        let err = place_level(
            &mut world,
            &json!({ "level": format!("asset:{}", Guid::from_u128(0xa0)) }),
        )
        .expect_err("no library");
        assert_eq!(reason(&err), Some("level_not_found"));
        let _ = std::fs::remove_dir_all(&project);
    }

    /// The spec 37 § 8 state words are aliases of the built names (ADR-1667 (j)); `recursive`
    /// reaches child layers; a bad `side` and a missing `layer` are parameter errors.
    #[test]
    fn set_layer_state_takes_the_aliases_and_recursion() {
        let project = regia_test_utils::unique_temp_dir("agent-level-aliases");
        let mut world = world_in(&project);
        {
            let mut table = world.resource_mut::<LayerTable>();
            table.insert_layer("quest", None, LayerState::Live, LayerPeers::Everyone);
            table.insert_layer(
                "quest_boss",
                Some("quest"),
                LayerState::Live,
                LayerPeers::Everyone,
            );
        }
        for (word, effective) in [
            ("unloaded", "absent"),
            ("loaded", "parked"),
            ("activated", "live"),
        ] {
            let out = set_layer_state(&mut world, &json!({ "layer": "quest", "state": word })).expect(word);
            assert_eq!(out["effective"], effective, "{word}");
        }
        // Not recursive: the child keeps its requested state; parked parent clamps its effective.
        set_layer_state(&mut world, &json!({ "layer": "quest", "state": "parked" })).expect("parent");
        {
            let table = world.resource::<LayerTable>();
            assert_eq!(table.requested("quest_boss"), Some(LayerState::Live));
            assert_eq!(table.effective("quest_boss"), Some(LayerState::Parked));
        }
        // Recursive: the child is requested too.
        set_layer_state(
            &mut world,
            &json!({ "layer": "quest", "state": "absent", "recursive": true }),
        )
        .expect("recursive");
        assert_eq!(
            world.resource::<LayerTable>().requested("quest_boss"),
            Some(LayerState::Absent)
        );
        assert!(set_layer_state(&mut world, &json!({ "state": "live" })).is_err());
        assert!(
            set_layer_state(
                &mut world,
                &json!({ "layer": "quest", "state": "live", "side": "peer" })
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    /// `transform.rotation` reaches the instance (it was read as translation and scale only, so
    /// a rotated placement came out unrotated); `parent` parents the instance; a placement whose
    /// level cannot be found lists its error in `level.placed`; `level.break_placed` needs
    /// `entity`.
    #[test]
    fn place_level_takes_rotation_and_parent_and_lists_errors() {
        let project = regia_test_utils::unique_temp_dir("agent-level-place-rot");
        let mut world = world_in(&project);
        world.resource_mut::<LevelLibrary>().insert(level(0xa0, LEVEL));
        let holder = world.spawn(EntityGuid(Guid::from_u128(0x77))).id();
        let half = std::f32::consts::FRAC_1_SQRT_2;
        let out = place_level(
            &mut world,
            &json!({
                "level": format!("asset:{}", Guid::from_u128(0xa0)),
                "transform": { "translation": [1, 0, 0], "rotation": [0, 2.0 * half, 0, 2.0 * half] },
                "parent": holder.to_bits(),
            }),
        )
        .expect("place");
        let entity = Entity::try_from_bits(out["entity"].as_u64().expect("id")).expect("entity");
        let transform = *world.get::<Transform>(entity).expect("transform");
        let expected = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        assert!(
            transform.rotation.angle_between(expected) < 1e-4,
            "rotation applied and normalized: {:?}",
            transform.rotation
        );
        assert_eq!(world.get::<ChildOf>(entity).map(ChildOf::parent), Some(holder));
        for bad in [json!([0, 0, 0, 0]), json!([0, 1, 0]), json!([0, "y", 0, 1])] {
            let err = place_level(
                &mut world,
                &json!({
                    "level": format!("asset:{}", Guid::from_u128(0xa0)),
                    "transform": { "rotation": bad },
                }),
            )
            .expect_err("bad rotation");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
        let err = place_level(
            &mut world,
            &json!({ "level": format!("asset:{}", Guid::from_u128(0xa0)), "parent": 999_999_999_u64 }),
        )
        .expect_err("no parent");
        assert!(
            reason(&err).is_none(),
            "a missing parent is not_found, not a level reason"
        );

        // A placement of a level the library does not hold: the row carries its error.
        let broken = world
            .spawn((
                EntityGuid(Guid::from_u128(0x99)),
                Transform::default(),
                PlacedLevel::inline(LevelId(Guid::from_u128(0xdd))),
            ))
            .id();
        placed::spawn_placed_levels(&mut world);
        let list = placed_list(&mut world).expect("list");
        let row = list["placed"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["entity"] == broken.to_bits()))
            .cloned()
            .expect("the broken row");
        assert_eq!(row["error"]["reason"], "level_not_found");
        assert_eq!(row["spawned"], 0);
        assert!(break_placed(&mut world, &json!({})).is_err());
        let _ = std::fs::remove_dir_all(&project);
    }

    /// w10-v051-smalls 3: the `level.place` preview counted records only, so it accepted a level
    /// whose one record instantiates a prefab over the placement cap; the fill then refused it
    /// and the instance stayed behind carrying a `PlacedError`. The preview counts the prefab's
    /// expansion too, refuses with `placed_limit` and spawns nothing.
    #[test]
    fn place_level_refuses_a_prefab_expansion_over_the_cap_before_spawning() {
        use regia_scene::prefab::{PrefabLibrary, PrefabTemplate, TemplateEntity};

        let project = regia_test_utils::unique_temp_dir("agent-level-place-prefab-cap");
        let mut world = world_in(&project);
        let prefab = Guid::from_u128(0xfab_0001);
        let entities = (0..=placed::MAX_PLACED_ENTITIES as u32)
            .map(|i| TemplateEntity {
                local_id: Guid::from_u128(((prefab.as_u128()) << 32) + u128::from(i) + 1),
                name: None,
                parent: (i > 0).then_some(0),
                components: Vec::new(),
                nested: None,
            })
            .collect();
        world.resource_mut::<PrefabLibrary>().insert(PrefabTemplate {
            asset: prefab,
            entities,
            warnings: Vec::new(),
        });
        let text = format!(
            "SceneFile(format_version: {v}, entities: [(guid: \"{record}\", components: {{}}, prefab_instance: (prefab: \"asset:{prefab}\"))], resources: {{}})",
            v = SCENE_FORMAT_VERSION,
            record = Guid::from_u128(0xf000),
        );
        world.resource_mut::<LevelLibrary>().insert(level(0xa0, &text));
        let err = place_level(
            &mut world,
            &json!({ "level": format!("asset:{}", Guid::from_u128(0xa0)) }),
        )
        .expect_err("the expansion is over the cap");
        assert_eq!(reason(&err), Some("placed_limit"), "{err:?}");
        let left = world.query::<&PlacedLevel>().iter(&world).count();
        assert_eq!(left, 0, "the refused preview spawned no placement");
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn switch_opens_cancels_and_reports() {
        use regia_framework::level_switch::{LevelSwitch, LevelSwitchPlugin};
        let project = regia_test_utils::unique_temp_dir("agent-level-switch");
        std::fs::create_dir_all(project.join("levels")).expect("mkdir");
        std::fs::write(project.join("levels/yard.level.ron"), LEVEL).expect("write");
        std::fs::write(project.join("levels/pier.level.ron"), LEVEL).expect("write");
        let mut app = App::new();
        app.add_plugins((regia_scene::ScenePlugin, LevelSwitchPlugin));
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(project.clone()),
        });
        let world = app.world_mut();

        let code = |err: &RpcError| {
            err.data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .map(str::to_owned)
        };
        let err = switch(world, &json!({})).expect_err("no params");
        assert_eq!(code(&err).as_deref(), Some("level_switch_bad_params"));
        let err =
            switch(world, &json!({ "path": "levels/yard.level.ron", "cancel": true })).expect_err("both");
        assert_eq!(code(&err).as_deref(), Some("level_switch_bad_params"));
        let err = switch(world, &json!({ "cancel": "yes" })).expect_err("not a bool");
        assert_eq!(code(&err).as_deref(), Some("level_switch_bad_params"));
        let err = switch(world, &json!({ "path": "levels/none.level.ron" })).expect_err("missing");
        assert_eq!(reason(&err), Some("level_not_found"));
        let err = switch(world, &json!({ "path": "../x.level.ron" })).expect_err("outside");
        assert_eq!(reason(&err), Some("path_outside_project"));

        let out = switch(world, &json!({ "path": "levels/yard.level.ron" })).expect("switch");
        assert!(
            out["loading"]
                .as_str()
                .is_some_and(|p| p.ends_with("levels/yard.level.ron")),
            "{out}"
        );
        let opened = LevelSwitch::wait_and_open(world, std::time::Duration::from_secs(60));
        assert!(opened.is_some());
        let out = switch_status(world).expect("status");
        assert_eq!(out["last"]["outcome"], "opened", "{out}");
        assert_eq!(out["current"], out["last"]["level"]);
        assert!(out["loading"].is_null());

        switch(world, &json!({ "path": "levels/pier.level.ron" })).expect("switch");
        let out = switch(world, &json!({ "cancel": true })).expect("cancel");
        assert_eq!(out["last"]["outcome"], "cancelled", "{out}");
        assert_eq!(out["stats"]["loads_cancelled"], 1);
        assert_eq!(out["stats"]["loads_landed"], 1);
        assert!(out["current"].is_string());

        world.remove_resource::<LevelSwitch>();
        let err = switch_status(world).expect_err("no plugin");
        assert_eq!(code(&err).as_deref(), Some("F0003"));
        let _ = std::fs::remove_dir_all(&project);
    }
}
