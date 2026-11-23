// SPDX-License-Identifier: MPL-2.0
//! Layer edits outside the working toggles (spec 37 §§ 4.1-4.4), shared by the Layers panel and
//! the agent methods:
//!
//! - `editor.layer_delete`: delete a layer from a definition asset, refused with `layer_in_use`
//!   while a level that binds the definition has members in it (ADR-7203). The levels checked are
//!   every level source of the project (`assets/`, either form) plus the open level's unsaved
//!   state, which replaces its file on disk.
//! - `editor.layer_create`: add a layer to a definition asset, creating the `.layers.ron` and its
//!   `.meta` when the file does not exist yet (ADR-7614).
//! - `editor.layer_bind`: bind a definition asset to the open level (or unbind it).
//! - `editor.layer_initial`: set the state a layer starts in when the open level loads (the
//!   binding's `initial`, the saved state; the panel's visible and loaded toggles stay working
//!   state).
//!
//! Definition files are written atomically and are file edits, not undo steps (like
//! `editor.edit_asset`). Binding and initial-state edits change the open level's `LevelLayers`
//! (its header): one undo step each, written by the next level save.

use std::path::{Component, Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::level_assets::LevelSourceIndex;
use regia_scene::format::SceneFile;
use regia_scene::layer_delete::{LayerDeleteError, delete_layer};
use regia_scene::layers::{
    LAYERS_FORMAT_VERSION, LEVEL_LAYERS_PATH, LayerDef, LayerDefs, LayerDefsLibrary, LayerEntry, LayerKind,
    LayerState, LayerTable, LevelLayers, valid_layer_name,
};
use regia_scene::level::{LevelId, LevelLibrary};
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn string_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (string) is required")))
}

/// The project's `assets/` and `defs_rel` under it: refused when no project is open or the path
/// leaves `assets/`.
fn defs_location(world: &World, defs_rel: &str) -> Result<(PathBuf, PathBuf), RpcError> {
    let Some(root) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
    else {
        return Err(RpcError::not_found(
            "no project is open",
            "editor.open_project first",
        ));
    };
    let escapes = Path::new(defs_rel).components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if escapes {
        return Err(refused(
            "path_outside_project",
            format!("`{defs_rel}` leaves assets/"),
            "give the .layers.ron path relative to assets/",
        ));
    }
    let assets = root.join("assets");
    let defs_path = assets.join(defs_rel);
    Ok((assets, defs_path))
}

/// The asset id of the definition file `defs_rel` (its `.meta`), from a scan of `assets/`.
fn defs_id_of(
    index: &LevelSourceIndex,
    defs_path: &Path,
    defs_rel: &str,
) -> Result<regia_core::id::Guid, RpcError> {
    index
        .layer_defs
        .iter()
        .find(|(_, path)| **path == defs_path)
        .map(|(id, _)| *id)
        .ok_or_else(|| {
            refused(
                "layer_defs_not_found",
                format!(
                    "{defs_rel} is not a layer definition asset of the project (no .layers.ron with a .meta)"
                ),
                "editor.layers_panel_state lists the bound definitions",
            )
        })
}

/// `editor.layer_delete { defs, layer }`.
fn editor_layer_delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let defs_rel = string_param(params, "defs")?;
    let layer = string_param(params, "layer")?;
    delete_layer_from(world, defs_rel, layer)
}

/// Deletes `layer` from the definition file `defs_rel` (relative to `assets/`): what
/// `editor.layer_delete` and the Layers panel's *Delete* do. Answers `{ defs, layer, layers }`.
///
/// # Errors
/// The refusals of `editor.layer_delete` (`layer_in_use`, `layer_parent_invalid`,
/// `layer_unknown`, `layer_defs_not_found`, `path_outside_project`, `level_io_error`,
/// `level_parse_error`).
pub fn delete_layer_from(world: &mut World, defs_rel: &str, layer: &str) -> Result<Value, RpcError> {
    let (assets, defs_path) = defs_location(world, defs_rel)?;
    let index = LevelSourceIndex::scan(&assets);
    let defs_id = defs_id_of(&index, &defs_path, defs_rel)?;
    let text = std::fs::read_to_string(&defs_path).map_err(|err| {
        refused(
            "level_io_error",
            format!("cannot read {defs_rel}: {err}"),
            "check the file",
        )
    })?;
    let mut defs = LayerDefs::from_ron_str(&text)
        .map_err(|err| refused("level_parse_error", format!("{defs_rel}: {err}"), "fix the file"))?;

    // Every level source, the open level's unsaved state in place of its file.
    let open = world.get_resource::<crate::level_io::OpenLevel>().cloned();
    let open_scene = open.as_ref().and_then(|open| {
        let mut scene = regia_scene::level::snapshot_level(world, open.id).ok()?.scene;
        // A snapshot carries the settings, not the layer binding: the loaded level's binding.
        crate::level_io::keep_layer_binding(world, open.id, &mut scene);
        Some(scene)
    });
    let mut levels: Vec<(String, SceneFile)> = Vec::new();
    for (id, path) in &index.levels {
        let name = path.strip_prefix(&assets).unwrap_or(path).display().to_string();
        if let (Some(open), Some(scene)) = (&open, &open_scene)
            && open.id.0 == *id
        {
            levels.push((name, scene.clone()));
            continue;
        }
        match regia_scene::level_dir::read_level_source(path) {
            Ok(scene) => levels.push((name, scene)),
            Err(err) => tracing::warn!(target: "regia_editor", "layer delete: {name} not checked: {err}"),
        }
    }
    if let (Some(open), Some(scene)) = (&open, &open_scene)
        && !index.levels.contains_key(&open.id.0)
    {
        levels.push(("(open level)".to_string(), scene.clone()));
    }
    let borrowed: Vec<(&str, &SceneFile)> = levels
        .iter()
        .map(|(name, scene)| (name.as_str(), scene))
        .collect();
    if let Err(err) = delete_layer(&mut defs, defs_id, layer, &borrowed) {
        let mut error = refused(
            err.reason(),
            err.to_string(),
            "take the members out of the layer (editor.layers_panel_set remove_selection) or delete them, then retry",
        );
        if let LayerDeleteError::InUse { members, .. } = &err
            && let Some(Value::Object(data)) = error.data.as_mut()
        {
            data.insert(
                "members".into(),
                members
                    .iter()
                    .map(|m| json!({ "level": m.level, "entity": m.entity.to_string() }))
                    .collect(),
            );
        }
        return Err(error);
    }
    let out = defs
        .to_ron_string()
        .map_err(|err| RpcError::internal(format!("{defs_rel}: {err}")))?;
    crate::atomic_write::write_atomic(&defs_path, out.as_bytes())
        .map_err(|err| refused("level_io_error", err, "check the file's permissions"))?;
    if let Some(mut library) = world.get_resource_mut::<LayerDefsLibrary>() {
        library.insert(defs_id, defs.clone());
    }
    invalidate_table(world);
    Ok(json!({
        "defs": defs_rel,
        "layer": layer,
        "layers": defs.layers.iter().map(|def| def.name.clone()).collect::<Vec<_>>(),
    }))
}

/// The layer table rebuilds its layers on the next layer switch (a definition or a binding
/// changed); requested states stay. The Layers panel rescans its file list on its next draw, so
/// an agent's create, delete or bind shows at once, as a panel edit does (rev-claude-64 (3)
/// finding 2).
fn invalidate_table(world: &mut World) {
    if let Some(mut table) = world.get_resource_mut::<LayerTable>() {
        table.invalidate();
        table.mark_dirty();
    }
    if let Some(mut form) = world.get_resource_mut::<crate::layers_panel::LayerEditForm>() {
        form.scanned = false;
    }
}

/// Adds `layer` to the definition file `defs_rel` (relative to `assets/`), creating the file and
/// its `.meta` when it does not exist: what `editor.layer_create` and the Layers panel's
/// *Create layer* do. Answers `{ defs, asset, layer, layers, created_file }`.
///
/// # Errors
/// `path_outside_project`, `bad_layer_name` (not `[a-z0-9_]{1,48}`), `layer_exists`,
/// `layer_parent_invalid` (the parent is not a layer of the file), `bad_defs_path` (not a
/// `.layers.ron`), `level_io_error`, `level_parse_error`.
pub fn create_layer_in(
    world: &mut World,
    defs_rel: &str,
    layer: &str,
    kind: LayerKind,
    parent: Option<&str>,
) -> Result<Value, RpcError> {
    let (assets, defs_path) = defs_location(world, defs_rel)?;
    if !defs_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(regia_framework::level_assets::is_layer_defs_name)
    {
        return Err(refused(
            "bad_defs_path",
            format!("{defs_rel} is not a .layers.ron file name"),
            "name the definition file <name>.layers.ron",
        ));
    }
    if !valid_layer_name(layer) {
        return Err(refused(
            "bad_layer_name",
            format!("`{layer}` is not a layer name ([a-z0-9_]{{1,48}})"),
            "use lowercase ascii letters, digits and _",
        ));
    }
    let created_file = !defs_path.exists();
    let mut defs = if created_file {
        LayerDefs {
            format_version: LAYERS_FORMAT_VERSION,
            layers: Vec::new(),
        }
    } else {
        let text = std::fs::read_to_string(&defs_path).map_err(|err| {
            refused(
                "level_io_error",
                format!("cannot read {defs_rel}: {err}"),
                "check the file",
            )
        })?;
        LayerDefs::from_ron_str(&text)
            .map_err(|err| refused("level_parse_error", format!("{defs_rel}: {err}"), "fix the file"))?
    };
    if defs.get(layer).is_some() {
        return Err(refused(
            "layer_exists",
            format!("{defs_rel} already has a layer `{layer}`"),
            "pick another name",
        ));
    }
    if let Some(parent) = parent
        && defs.get(parent).is_none()
    {
        return Err(refused(
            "layer_parent_invalid",
            format!("`{parent}` is not a layer of {defs_rel}"),
            "create the parent layer first",
        ));
    }
    defs.layers.push(LayerDef {
        name: layer.to_owned(),
        kind,
        debug_color: (0.5, 0.5, 0.5),
        parent: parent.map(str::to_owned),
    });
    let out = defs
        .to_ron_string()
        .map_err(|err| RpcError::internal(format!("{defs_rel}: {err}")))?;
    let meta_path = regia_asset::meta_path_for(&defs_path);
    if created_file && let Some(dir) = defs_path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| {
            refused(
                "level_io_error",
                format!("cannot create {}: {err}", dir.display()),
                "check the folder's permissions",
            )
        })?;
    }
    if !meta_path.is_file() {
        let meta = regia_asset::MetaSidecar::new("layer_defs", regia_asset::AssetId::new_random());
        let text = meta
            .to_ron_string()
            .map_err(|err| RpcError::internal(format!("{defs_rel}.meta: {err}")))?;
        crate::atomic_write::write_atomic(&meta_path, text.as_bytes())
            .map_err(|err| refused("level_io_error", err, "check the folder's permissions"))?;
    }
    crate::atomic_write::write_atomic(&defs_path, out.as_bytes())
        .map_err(|err| refused("level_io_error", err, "check the file's permissions"))?;
    let index = LevelSourceIndex::scan(&assets);
    let defs_id = defs_id_of(&index, &defs_path, defs_rel)?;
    world
        .get_resource_or_insert_with(LayerDefsLibrary::default)
        .insert(defs_id, defs.clone());
    invalidate_table(world);
    Ok(json!({
        "defs": defs_rel,
        "asset": defs_id.to_string(),
        "layer": layer,
        "layers": defs.layers.iter().map(|def| def.name.clone()).collect::<Vec<_>>(),
        "created_file": created_file,
    }))
}

/// The open level and its layer binding as loaded (its `LevelLibrary` copy, which a save
/// writes back; ADR-7203).
fn open_binding(world: &World) -> Result<(LevelId, Option<LevelLayers>), RpcError> {
    let level = world
        .get_resource::<crate::level_io::OpenLevel>()
        .map(|open| open.id)
        .ok_or_else(|| refused("level_not_found", "no level is open", "editor.open_level first"))?;
    let binding = world
        .get_resource::<LevelLibrary>()
        .and_then(|library| library.get(level))
        .and_then(|loaded| LevelLayers::from_scene(&loaded.scene))
        .transpose()
        .map_err(|err| {
            refused(
                "level_parse_error",
                format!("the open level's LevelLayers does not parse: {err}"),
                "fix the level file",
            )
        })?;
    Ok((level, binding))
}

/// Writes `binding` into the open level's `LevelLibrary` copy (what a save writes) and the world
/// resource (which marks the level header dirty), and rebuilds the layer table.
fn write_binding(
    world: &mut World,
    level: LevelId,
    binding: Option<&LevelLayers>,
    resource: Option<&LevelLayers>,
) {
    let Some(current) = world
        .get_resource::<LevelLibrary>()
        .and_then(|library| library.get(level))
        .cloned()
    else {
        return;
    };
    let mut next = (*current).clone();
    match binding {
        Some(binding) => match ron::to_string(binding)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                ron::value::RawValue::from_boxed_ron(text.into_boxed_str()).map_err(|e| e.to_string())
            }) {
            Ok(raw) => {
                next.scene.resources.insert(LEVEL_LAYERS_PATH.to_owned(), raw);
            }
            Err(err) => {
                tracing::error!(target: "regia_editor", "layer binding not written: {err}");
                return;
            }
        },
        None => {
            next.scene.resources.remove(LEVEL_LAYERS_PATH);
        }
    }
    world.resource_mut::<LevelLibrary>().insert(next);
    match resource {
        Some(resource) => world.insert_resource(resource.clone()),
        None => {
            world.remove_resource::<LevelLayers>();
        }
    }
    invalidate_table(world);
}

/// One edit of the open level's layer binding: one undo step.
#[derive(Clone, Debug, PartialEq)]
pub struct SetLayerBinding {
    level: LevelId,
    before: Option<LevelLayers>,
    before_resource: Option<LevelLayers>,
    after: Option<LevelLayers>,
    label: String,
}

impl crate::command::EditorCommand for SetLayerBinding {
    fn apply(&mut self, world: &mut World) {
        write_binding(world, self.level, self.after.as_ref(), self.after.as_ref());
    }

    fn revert(&mut self, world: &mut World) {
        write_binding(
            world,
            self.level,
            self.before.as_ref(),
            self.before_resource.as_ref(),
        );
    }

    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

/// Applies `after` as one undo step (nothing when it equals `before`). Answers whether it changed.
fn push_binding(
    world: &mut World,
    level: LevelId,
    before: Option<LevelLayers>,
    after: Option<LevelLayers>,
    label: String,
) -> bool {
    if before == after {
        return false;
    }
    let mut command = SetLayerBinding {
        before_resource: world.get_resource::<LevelLayers>().cloned(),
        level,
        before,
        after,
        label,
    };
    crate::command::EditorCommand::apply(&mut command, world);
    world
        .get_resource_or_insert_with(crate::command::UndoStack::new)
        .push(Box::new(command));
    true
}

/// Binds the definition file `defs_rel` to the open level (`bind`), or unbinds it: what
/// `editor.layer_bind` and the Layers panel's *Bind* and *Unbind* do. One undo step; the next
/// level save writes it. Unbinding also drops the open level's entries for the layers only that
/// file defines. Answers `{ defs, asset, bound, changed }`.
///
/// # Errors
/// `level_not_found` (no open level), `path_outside_project`, `layer_defs_not_found`,
/// `level_io_error`, `level_parse_error`.
pub fn bind_defs(world: &mut World, defs_rel: &str, bind: bool) -> Result<Value, RpcError> {
    let (assets, defs_path) = defs_location(world, defs_rel)?;
    let index = LevelSourceIndex::scan(&assets);
    let defs_id = defs_id_of(&index, &defs_path, defs_rel)?;
    let (level, before) = open_binding(world)?;
    let loaded = world
        .get_resource::<LayerDefsLibrary>()
        .and_then(|library| library.get(defs_id).cloned());
    let defs = match loaded {
        Some(defs) => defs,
        None => {
            let defs = regia_framework::level_assets::read_layer_defs(&defs_path)
                .map_err(|err| refused("level_parse_error", err.to_string(), "fix the file"))?;
            world
                .get_resource_or_insert_with(LayerDefsLibrary::default)
                .insert(defs_id, defs.clone());
            defs
        }
    };
    let mut after = before.clone().unwrap_or_default();
    let is_ref = |text: &String| {
        text.strip_prefix("asset:")
            .unwrap_or(text)
            .parse::<regia_core::id::Guid>()
            .ok()
            == Some(defs_id)
    };
    if bind {
        if !after.defs.iter().any(is_ref) {
            after.defs.push(format!("asset:{defs_id}"));
        }
    } else {
        after.defs.retain(|text| !is_ref(text));
        let library = world.get_resource::<LayerDefsLibrary>();
        let still_bound = |name: &str| {
            after
                .def_ids()
                .iter()
                .filter_map(|id| library.and_then(|library| library.get(*id)))
                .chain(after.embedded.iter())
                .any(|other| other.get(name).is_some())
        };
        let entries: Vec<LayerEntry> = after
            .entries
            .iter()
            .filter(|entry| defs.get(&entry.layer).is_none() || still_bound(&entry.layer))
            .cloned()
            .collect();
        after.entries = entries;
    }
    let after = (before.is_some() || after != LevelLayers::default()).then_some(after);
    let label = if bind {
        format!("Bind layers {defs_rel}")
    } else {
        format!("Unbind layers {defs_rel}")
    };
    let changed = push_binding(world, level, before, after, label);
    Ok(json!({
        "defs": defs_rel,
        "asset": defs_id.to_string(),
        "bound": bind,
        "changed": changed,
    }))
}

/// Sets the state `layer` starts in when the open level loads (the binding entry's `initial`):
/// what `editor.layer_initial` and the Layers panel's *initial* column do. One undo step; the next
/// level save writes it; the working toggles stay where they are. Answers
/// `{ layer, initial, changed }`.
///
/// # Errors
/// `level_not_found`, `layer_unknown` (not a layer the open level binds), `level_parse_error`.
pub fn set_initial_state(world: &mut World, layer: &str, initial: LayerState) -> Result<Value, RpcError> {
    let (level, before) = open_binding(world)?;
    // A layer of a bound definition: checked against the binding itself, so a bind (or an
    // unbind) earlier in the same frame counts before the layer table rebuilds.
    let library = world.get_resource::<LayerDefsLibrary>();
    let defines = |binding: &LevelLayers| {
        binding.embedded.iter().any(|defs| defs.get(layer).is_some())
            || binding
                .def_ids()
                .into_iter()
                .filter_map(|id| library.and_then(|library| library.get(id)))
                .any(|defs| defs.get(layer).is_some())
    };
    let Some(binding) = before.clone().filter(|binding| defines(binding)) else {
        return Err(refused(
            "layer_unknown",
            format!("`{layer}` is not a layer the open level binds"),
            "editor.layers_panel_state lists the layers",
        ));
    };
    let mut after = binding;
    match after.entries.iter_mut().find(|entry| entry.layer == layer) {
        Some(entry) => entry.initial = initial,
        None => after.entries.push(LayerEntry {
            layer: layer.to_owned(),
            initial,
            ..LayerEntry::default()
        }),
    }
    let changed = push_binding(
        world,
        level,
        before,
        Some(after),
        format!("Set initial state of {layer}"),
    );
    Ok(json!({ "layer": layer, "initial": initial.as_str(), "changed": changed }))
}

/// The definition files the open level binds, as `(asset id, path relative to assets/)`, in
/// binding order (ids without a file in the project are skipped).
#[must_use]
pub fn bound_defs(world: &World) -> Vec<(regia_core::id::Guid, String)> {
    let Some(assets) = crate::layers_panel::assets_dir(world) else {
        return Vec::new();
    };
    let Ok((_, Some(binding))) = open_binding(world) else {
        return Vec::new();
    };
    let index = LevelSourceIndex::scan(&assets);
    binding
        .def_ids()
        .into_iter()
        .filter_map(|id| {
            let path = index.layer_defs.get(&id)?;
            Some((
                id,
                path.strip_prefix(&assets).unwrap_or(path).display().to_string(),
            ))
        })
        .collect()
}

fn layer_create(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let defs_rel = string_param(params, "defs")?;
    let layer = string_param(params, "layer")?;
    let kind = match params.get("kind").and_then(Value::as_str) {
        None | Some("runtime") => LayerKind::Runtime,
        Some("editor_only") => LayerKind::EditorOnly,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`kind` is runtime or editor_only, not `{other}`"
            )));
        }
    };
    let parent = params
        .get("parent")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    create_layer_in(world, defs_rel, layer, kind, parent)
}

fn layer_bind(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let defs_rel = string_param(params, "defs")?;
    let bind = match params.get("bind") {
        None | Some(Value::Null) => true,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`bind` is a boolean"))?,
    };
    bind_defs(world, defs_rel, bind)
}

fn layer_initial(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let layer = string_param(params, "layer")?;
    let text = string_param(params, "initial")?;
    let initial = LayerState::parse(text).ok_or_else(|| {
        RpcError::invalid_params(format!(
            "`initial` is absent, parked or live (or unloaded, loaded, activated), not `{text}`"
        ))
    })?;
    set_initial_state(world, layer, initial)
}

/// The methods of this module.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.layer_delete",
                tool: "layer_delete",
                description: "Delete a layer from a layer definition asset (.layers.ron). Refused with layer_in_use (and the members in data.members) while any level of the project that binds the definition, the open level's unsaved state included, has members in it; layer_parent_invalid while another layer names it as parent. Writes the file; not an undo step.",
                params: &[
                    ParamSchema {
                        name: "defs",
                        kind: "string",
                        required: true,
                        description: "The definition file, relative to assets/ (e.g. layers/town.layers.ron).",
                    },
                    ParamSchema {
                        name: "layer",
                        kind: "string",
                        required: true,
                        description: "The layer name.",
                    },
                ],
                result: "{ defs, layer, layers }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_layer_delete as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layer_create",
                tool: "layer_create",
                description: "Add a layer to a layer definition asset (.layers.ron), creating the file and its .meta when it does not exist. Refused with bad_layer_name, layer_exists, layer_parent_invalid or bad_defs_path. Writes the file; not an undo step. The Layers panel's Create layer.",
                params: &[
                    ParamSchema {
                        name: "defs",
                        kind: "string",
                        required: true,
                        description: "The definition file, relative to assets/ (e.g. layers/town.layers.ron).",
                    },
                    ParamSchema {
                        name: "layer",
                        kind: "string",
                        required: true,
                        description: "The new layer's name ([a-z0-9_]{1,48}).",
                    },
                    ParamSchema {
                        name: "kind",
                        kind: "string",
                        required: false,
                        description: "runtime (default) or editor_only.",
                    },
                    ParamSchema {
                        name: "parent",
                        kind: "string",
                        required: false,
                        description: "A layer of the same file the new layer is never more active than.",
                    },
                ],
                result: "{ defs, asset, layer, layers, created_file }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layer_create as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layer_bind",
                tool: "layer_bind",
                description: "Bind a layer definition asset to the open level (its LevelLayers), or unbind it with bind: false (which also drops the level's entries for layers only that file defines). One undo step; editor.save_level writes it. The Layers panel's Bind and Unbind.",
                params: &[
                    ParamSchema {
                        name: "defs",
                        kind: "string",
                        required: true,
                        description: "The definition file, relative to assets/.",
                    },
                    ParamSchema {
                        name: "bind",
                        kind: "boolean",
                        required: false,
                        description: "true (default) binds, false unbinds.",
                    },
                ],
                result: "{ defs, asset, bound, changed }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layer_bind as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layer_initial",
                tool: "layer_initial",
                description: "Set the state a layer of the open level starts in when the level loads (the saved initial state of its binding entry; the working visible and loaded toggles stay). One undo step; editor.save_level writes it. The Layers panel's initial column.",
                params: &[
                    ParamSchema {
                        name: "layer",
                        kind: "string",
                        required: true,
                        description: "A layer the open level binds.",
                    },
                    ParamSchema {
                        name: "initial",
                        kind: "string",
                        required: true,
                        description: "absent, parked or live (unloaded, loaded, activated are accepted too).",
                    },
                ],
                result: "{ layer, initial, changed }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layer_initial as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::App;

    /// The dispatcher path: a member in a level binding the definition refuses the delete.
    #[test]
    fn layer_delete_through_the_agent_method() {
        let root = regia_test_utils::unique_temp_dir("layer-delete-agent");
        std::fs::create_dir_all(root.join("assets/levels")).expect("dir");
        let defs_path = root.join("assets/town.layers.ron");
        std::fs::write(
            &defs_path,
            "LayerDefs(format_version: 1, layers: [(name: \"night\"), (name: \"day\")])\n",
        )
        .expect("defs");
        let id = regia_core::id::Guid::from_u128(0x1a7e_0000_0000_4000_8000_0000_0000_0001);
        let meta = regia_asset::MetaSidecar::new("layer_defs", regia_asset::AssetId(id));
        std::fs::write(
            regia_asset::meta_path_for(&defs_path),
            meta.to_ron_string().expect("meta"),
        )
        .expect("meta");
        let level = root.join("assets/levels/town.level.ron");
        std::fs::write(
            &level,
            format!(
                r#"SceneFile(format_version: {v}, entities: [(guid: "5a00000a-0000-4000-8000-00000000000a", components: {{ "regia_scene::layers::LayerMembership": (layers: ["night"]) }})], resources: {{ "regia_scene::layers::LevelLayers": (defs: ["asset:{id}"], entries: []) }})"#,
                v = regia_scene::format::SCENE_FORMAT_VERSION
            ),
        )
        .expect("level");
        let level_meta = regia_asset::MetaSidecar::new("level_ron", regia_asset::AssetId::new_random());
        std::fs::write(
            regia_asset::meta_path_for(&level),
            level_meta.to_ron_string().expect("meta"),
        )
        .expect("level meta");

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        crate::agent_ext::install(app.world_mut());
        let (_, handler) = app
            .world()
            .resource::<regia_agent::extension::AgentExtension>()
            .method("editor.layer_delete")
            .expect("registered");
        let before = std::fs::read(&defs_path).expect("defs");
        let err = handler(
            app.world_mut(),
            &json!({ "defs": "town.layers.ron", "layer": "night" }),
        )
        .expect_err("in use");
        let data = err.data.clone().expect("data");
        assert_eq!(data["reason"], "layer_in_use", "{err:?}");
        assert_eq!(data["members"][0]["level"], "levels/town.level.ron");
        assert_eq!(
            std::fs::read(&defs_path).expect("defs"),
            before,
            "nothing written"
        );

        // The open level's unsaved state counts instead of its file: a member added in the
        // editor (not saved) refuses the delete of `day`.
        let open = crate::level_io::open_level_file(app.world_mut(), &level).expect("open");
        app.world_mut().insert_resource(open);
        let member = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &regia_scene::prelude::EntityGuid)>();
            query
                .iter(world)
                .map(|(entity, _)| entity)
                .next()
                .expect("the level's entity")
        };
        app.world_mut()
            .entity_mut(member)
            .insert(regia_scene::layers::LayerMembership {
                layers: vec!["night".into(), "day".into()],
            });
        let err = handler(
            app.world_mut(),
            &json!({ "defs": "town.layers.ron", "layer": "day" }),
        )
        .expect_err("unsaved member");
        assert_eq!(err.data.clone().expect("data")["reason"], "layer_in_use");
        app.world_mut()
            .entity_mut(member)
            .insert(regia_scene::layers::LayerMembership {
                layers: vec!["night".into()],
            });

        let out = handler(
            app.world_mut(),
            &json!({ "defs": "town.layers.ron", "layer": "day" }),
        )
        .expect("deleted");
        assert_eq!(out["layers"], json!(["night"]));
        let written = std::fs::read_to_string(&defs_path).expect("defs");
        assert!(!written.contains("\"day\""), "{written}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A project with `layers/town.layers.ron` (layer `night`, a fixed id) and a level that binds
    /// nothing, opened in the editor.
    fn bind_project(tag: &str) -> (App, std::path::PathBuf, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets/layers")).expect("dir");
        std::fs::create_dir_all(root.join("assets/levels")).expect("dir");
        let defs_path = root.join("assets/layers/town.layers.ron");
        std::fs::write(
            &defs_path,
            "LayerDefs(format_version: 1, layers: [(name: \"night\")])\n",
        )
        .expect("defs");
        let id = regia_core::id::Guid::from_u128(0x1a7e_0000_0000_4000_8000_0000_0000_0002);
        let meta = regia_asset::MetaSidecar::new("layer_defs", regia_asset::AssetId(id));
        std::fs::write(
            regia_asset::meta_path_for(&defs_path),
            meta.to_ron_string().expect("meta"),
        )
        .expect("meta");
        let level = root.join("assets/levels/town.level.ron");
        std::fs::write(
            &level,
            format!(
                r#"SceneFile(format_version: {v}, entities: [(guid: "5a00000a-0000-4000-8000-00000000000a", components: {{ "regia_scene::layers::LayerMembership": (layers: ["night"]) }})], resources: {{}})"#,
                v = regia_scene::format::SCENE_FORMAT_VERSION
            ),
        )
        .expect("level");
        let level_meta = regia_asset::MetaSidecar::new(
            "level_ron",
            regia_asset::AssetId(regia_core::id::Guid::from_u128(0x1e7e1)),
        );
        std::fs::write(
            regia_asset::meta_path_for(&level),
            level_meta.to_ron_string().expect("meta"),
        )
        .expect("level meta");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        app.world_mut().insert_resource(crate::command::UndoStack::new());
        let open = crate::level_io::open_level_file(app.world_mut(), &level).expect("open");
        app.world_mut().insert_resource(open);
        app.update();
        (app, root, level)
    }

    fn handler(name: &str) -> Handler {
        methods()
            .into_iter()
            .find(|(schema, _)| schema.name == name)
            .map(|(_, handler)| handler)
            .expect("registered")
    }

    fn save(app: &mut App, path: &std::path::Path) -> String {
        let open = app.world().resource::<crate::level_io::OpenLevel>().clone();
        crate::level_io::save_level_file(app.world_mut(), &open, path).expect("save");
        std::fs::read_to_string(path).expect("read")
    }

    fn depth(app: &App) -> usize {
        app.world().resource::<crate::command::UndoStack>().undo_depth()
    }

    #[test]
    fn smoke_layer_bind_undo_restores_absent_resource() {
        let (mut app, root, _) = bind_project("smoke-layer-bind-undo");
        let before = app.world().get_resource::<LevelLayers>().cloned();
        let before_binding = open_binding(app.world()).expect("binding");
        handler("editor.layer_bind")(app.world_mut(), &json!({ "defs": "layers/town.layers.ron" }))
            .expect("bind");
        assert_eq!(depth(&app), 1);
        assert!(crate::command::undo_world(app.world_mut()));
        assert_eq!(open_binding(app.world()).expect("binding"), before_binding);
        assert_eq!(app.world().get_resource::<LevelLayers>(), before.as_ref());
        assert!(crate::command::redo_world(app.world_mut()));
        assert_eq!(bound_defs(app.world()).len(), 1);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn smoke_layer_bind_undo_preserves_existing_world_resource() {
        let (mut app, root, _) = bind_project("smoke-layer-bind-existing");
        // The world resource is independent of the LevelLibrary header (e.g. another loaded
        // level supplied it). Restore exactly what the command replaced, not a default.
        let before = LevelLayers {
            entries: vec![LayerEntry {
                layer: "other_level".into(),
                initial: LayerState::Parked,
                ..LayerEntry::default()
            }],
            ..LevelLayers::default()
        };
        app.insert_resource(before.clone());
        for _ in 0..3 {
            handler("editor.layer_bind")(app.world_mut(), &json!({ "defs": "layers/town.layers.ron" }))
                .expect("bind");
            assert!(crate::command::undo_world(app.world_mut()));
            assert_eq!(app.world().get_resource::<LevelLayers>(), Some(&before));
            assert!(open_binding(app.world()).expect("binding").1.is_none());
        }
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    /// ADR-7614 acceptance: create, bind, set the initial state and delete from the Layers panel
    /// give the same definition and level bytes as the agent methods; bind and initial are one
    /// undo step each, and undoing them gives the unbound level's bytes back.
    #[test]
    fn panel_layer_edits_write_what_the_agent_methods_write() {
        use crate::layers_panel::{LayerEdit, apply_layer_edit};
        let (mut agent, agent_root, _) = bind_project("layer-edits-agent");
        let (mut panel, panel_root, _) = bind_project("layer-edits-panel");
        let panel_out = panel_root.join("out.level.ron");
        let unbound = save(&mut panel, &panel_out);

        let world = agent.world_mut();
        handler("editor.layer_create")(
            world,
            &json!({ "defs": "layers/town.layers.ron", "layer": "day", "kind": "editor_only" }),
        )
        .expect("create");
        let out =
            handler("editor.layer_bind")(world, &json!({ "defs": "layers/town.layers.ron" })).expect("bind");
        assert_eq!(out["changed"], true);
        agent.update();
        let out = handler("editor.layer_initial")(
            agent.world_mut(),
            &json!({ "layer": "night", "initial": "parked" }),
        )
        .expect("initial");
        assert_eq!(out["initial"], "parked");

        apply_layer_edit(
            panel.world_mut(),
            LayerEdit::Create {
                defs: "layers/town.layers.ron".into(),
                layer: "day".into(),
                kind: regia_scene::layers::LayerKind::EditorOnly,
            },
        )
        .expect("create");
        let before = depth(&panel);
        apply_layer_edit(
            panel.world_mut(),
            LayerEdit::Bind {
                defs: "layers/town.layers.ron".into(),
                bind: true,
            },
        )
        .expect("bind");
        assert_eq!(depth(&panel), before + 1, "bind is one undo step");
        panel.update();
        assert!(
            crate::layers_panel::rows(panel.world())
                .iter()
                .any(|row| row.name == "night" && row.members == 1),
            "the bound layer shows in the panel"
        );
        apply_layer_edit(
            panel.world_mut(),
            LayerEdit::Initial {
                layer: "night".into(),
                initial: regia_scene::layers::LayerState::Parked,
            },
        )
        .expect("initial");
        assert_eq!(depth(&panel), before + 2, "initial is one undo step");

        let defs = |root: &std::path::Path| {
            std::fs::read_to_string(root.join("assets/layers/town.layers.ron")).expect("defs")
        };
        assert_eq!(
            defs(&panel_root),
            defs(&agent_root),
            "create: the same definition bytes"
        );
        assert!(defs(&panel_root).contains("\"day\""));
        let agent_level = save(&mut agent, &agent_root.join("out.level.ron"));
        let panel_level = save(&mut panel, &panel_out);
        assert_eq!(panel_level, agent_level, "bind and initial: the same level bytes");
        assert!(
            panel_level.contains("asset:1a7e0000-0000-4000-8000-000000000002"),
            "{panel_level}"
        );
        assert!(panel_level.contains("Parked"), "{panel_level}");

        // Undo the initial state, then the binding: the level saves as it was.
        assert!(crate::command::undo_world(panel.world_mut()));
        assert!(!save(&mut panel, &panel_out).contains("Parked"));
        assert!(crate::command::undo_world(panel.world_mut()));
        assert_eq!(
            save(&mut panel, &panel_out),
            unbound,
            "undo restores the level bytes"
        );
        assert!(crate::command::redo_world(panel.world_mut()));
        assert!(crate::command::redo_world(panel.world_mut()));
        assert_eq!(save(&mut panel, &panel_out), agent_level, "redo");

        // Delete: the same definition bytes again (`day` has no members).
        handler("editor.layer_delete")(
            agent.world_mut(),
            &json!({ "defs": "layers/town.layers.ron", "layer": "day" }),
        )
        .expect("delete");
        apply_layer_edit(
            panel.world_mut(),
            LayerEdit::Delete {
                defs: "layers/town.layers.ron".into(),
                layer: "day".into(),
            },
        )
        .expect("delete");
        assert_eq!(
            defs(&panel_root),
            defs(&agent_root),
            "delete: the same definition bytes"
        );
        assert!(!defs(&panel_root).contains("\"day\""));
        // `night` has a member: the panel's delete is refused like the agent's.
        let err = apply_layer_edit(
            panel.world_mut(),
            LayerEdit::Delete {
                defs: "layers/town.layers.ron".into(),
                layer: "night".into(),
            },
        )
        .expect_err("in use");
        assert_eq!(err.data.expect("data")["reason"], "layer_in_use");

        // Unbind drops the binding's entries for the file's layers.
        apply_layer_edit(
            panel.world_mut(),
            LayerEdit::Bind {
                defs: "layers/town.layers.ron".into(),
                bind: false,
            },
        )
        .expect("unbind");
        let unbound_again = save(&mut panel, &panel_out);
        assert!(!unbound_again.contains("asset:1a7e0000"), "{unbound_again}");
        assert!(!unbound_again.contains("Parked"), "{unbound_again}");
        let _ = std::fs::remove_dir_all(&agent_root);
        let _ = std::fs::remove_dir_all(&panel_root);
    }

    /// `editor.layer_create` makes a new definition file with its `.meta` and refuses bad input
    /// with documented reasons; `editor.layer_initial` refuses a layer the level does not bind.
    #[test]
    fn layer_create_makes_the_file_and_names_its_refusals() {
        let (mut app, root, _) = bind_project("layer-create-new");
        let world = app.world_mut();
        let create = handler("editor.layer_create");
        let reason = |err: RpcError| err.data.expect("data")["reason"].as_str().map(str::to_owned);
        let out = create(
            world,
            &json!({ "defs": "layers/new/quest.layers.ron", "layer": "quest_a" }),
        )
        .expect("new file");
        assert_eq!(out["created_file"], true);
        let path = root.join("assets/layers/new/quest.layers.ron");
        assert!(regia_asset::meta_path_for(&path).is_file(), "a .meta");
        let out = create(
            world,
            &json!({ "defs": "layers/new/quest.layers.ron", "layer": "quest_a_boss", "parent": "quest_a" }),
        )
        .expect("child");
        assert_eq!(out["created_file"], false);
        assert_eq!(out["layers"], json!(["quest_a", "quest_a_boss"]));
        let defs = LayerDefs::from_ron_str(&std::fs::read_to_string(&path).expect("read")).expect("parses");
        assert_eq!(
            defs.get("quest_a_boss").and_then(|d| d.parent.as_deref()),
            Some("quest_a")
        );
        for (params, expected) in [
            (
                json!({ "defs": "layers/new/quest.layers.ron", "layer": "Bad Name" }),
                "bad_layer_name",
            ),
            (
                json!({ "defs": "layers/new/quest.layers.ron", "layer": "quest_a" }),
                "layer_exists",
            ),
            (
                json!({ "defs": "layers/new/quest.layers.ron", "layer": "x", "parent": "nope" }),
                "layer_parent_invalid",
            ),
            (
                json!({ "defs": "layers/quest.ron", "layer": "x" }),
                "bad_defs_path",
            ),
            (
                json!({ "defs": "../quest.layers.ron", "layer": "x" }),
                "path_outside_project",
            ),
        ] {
            let err = create(world, &params).expect_err(expected);
            assert_eq!(reason(err).as_deref(), Some(expected), "{params}");
        }
        let err = handler("editor.layer_bind")(world, &json!({ "defs": "layers/none.layers.ron" }))
            .expect_err("no such file");
        assert_eq!(reason(err).as_deref(), Some("layer_defs_not_found"));
        let err = handler("editor.layer_initial")(world, &json!({ "layer": "night", "initial": "parked" }))
            .expect_err("not bound yet");
        assert_eq!(reason(err).as_deref(), Some("layer_unknown"));
        // Bind and set the initial state in the same frame (one agent batch): no update between.
        handler("editor.layer_bind")(world, &json!({ "defs": "layers/town.layers.ron" })).expect("bind");
        let out = handler("editor.layer_initial")(world, &json!({ "layer": "night", "initial": "absent" }))
            .expect("the bound layer counts before the table rebuilds");
        assert_eq!(out["changed"], true);
        assert!(
            handler("editor.layer_initial")(world, &json!({ "layer": "night", "initial": "on" })).is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// rev-claude-64 (3) finding 2: a definition file an agent creates shows in the Layers
    /// panel's file list at once (the cached list is rescanned after `editor.layer_create`, as
    /// after a panel edit).
    #[test]
    fn an_agent_created_definition_file_shows_in_the_panel() {
        let (mut app, root, _) = bind_project("layer-create-panel");
        let ctx = egui::Context::default();
        let draw = |app: &mut App| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                crate::layers_panel::layers_panel(ui, app.world_mut());
            });
            output.textures_delta.clear();
        };
        let files = |app: &App| -> Vec<String> {
            app.world()
                .get_resource::<crate::layers_panel::LayerEditForm>()
                .map(|form| form.files.iter().map(|(_, path)| path.clone()).collect())
                .unwrap_or_default()
        };
        draw(&mut app);
        assert!(!files(&app).iter().any(|path| path.ends_with("new.layers.ron")));
        handler("editor.layer_create")(
            app.world_mut(),
            &json!({ "defs": "layers/new.layers.ron", "layer": "fresh" }),
        )
        .expect("create");
        draw(&mut app);
        assert!(
            files(&app).iter().any(|path| path.ends_with("new.layers.ron")),
            "the panel lists the new file: {:?}",
            files(&app)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// rev-claude-64 (3) finding 3: in one agent batch, unbinding a file and then setting the
    /// initial state of one of its layers refuses `layer_unknown` (the layer table, not rebuilt
    /// yet, still lists the layer; the binding does not).
    #[test]
    fn layer_initial_after_an_unbind_in_the_same_frame_is_refused() {
        let (mut app, root, _) = bind_project("layer-unbind-initial");
        handler("editor.layer_bind")(app.world_mut(), &json!({ "defs": "layers/town.layers.ron" }))
            .expect("bind");
        app.update();
        let world = app.world_mut();
        handler("editor.layer_initial")(world, &json!({ "layer": "night", "initial": "parked" }))
            .expect("a bound layer");
        handler("editor.layer_bind")(world, &json!({ "defs": "layers/town.layers.ron", "bind": false }))
            .expect("unbind");
        let err = handler("editor.layer_initial")(world, &json!({ "layer": "night", "initial": "absent" }))
            .expect_err("the file is no longer bound");
        assert_eq!(err.data.expect("data")["reason"], "layer_unknown");
        let _ = std::fs::remove_dir_all(&root);
    }
}
