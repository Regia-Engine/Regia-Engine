// SPDX-License-Identifier: MPL-2.0
//! The undo history and the structural edits as agent calls (`book/agents/undo-redo.md`):
//! `editor.history` reads the shared undo stack; `editor.spawn`, `editor.delete`,
//! `editor.reparent`, `editor.duplicate`, `editor.add_component` and `editor.remove_component`
//! make the outliner's and inspector's edits through the same commands, one undo entry each.
//! `editor.delete` and `editor.duplicate` also take the Outliner's multi-entity forms
//! (`book/agents/hierarchy.md`, ADR-1354); `editor.reparent` is registered by
//! [`crate::hierarchy_dnd`].
//!
//! Entity ids stay usable across undo: an entity that an undo brought back (or a redo created
//! again) has a new id, and every method here resolves the id the agent holds through
//! [`EntityRemap`](crate::command::EntityRemap). Results report the current id.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect, ReflectComponent};
use regia_scene::prelude::{EntityGuid, Transform};
use serde_json::{Value, json};

use crate::command::{
    AddComponent, CommandGroup, DespawnEntities, EditorCommand, HistoryError, RemoveComponent, Reparent,
    SpawnEntities, UNDO_CAP, UndoStack, resolve_entity,
};
use crate::hierarchy_ops::HierarchyError;
use crate::selection::Selection;
use crate::viewport_agent::Handler;

/// The live entity behind the id parameter `name`, following ids that undo or redo re-spawned.
pub(crate) fn live_entity(world: &World, params: &Value, name: &str) -> Result<Entity, RpcError> {
    let bits = params
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (entity id) is required")))?;
    resolve_entity(world, bits).ok_or_else(|| {
        RpcError::not_found(
            format!("entity {bits} does not exist"),
            "use scene.query_entities; an undo may have removed it (editor.history shows the stack)",
        )
    })
}

/// An optional entity parameter: absent or `null` = `None`.
fn optional_entity(world: &World, params: &Value, name: &str) -> Result<Option<Entity>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => live_entity(world, params, name).map(Some),
    }
}

/// Pushes an applied command as its own entry: sealed on both sides, so an agent edit never
/// merges into the edit before or after it. Returns the undo depth.
pub(crate) fn push_discrete(world: &mut World, command: Box<dyn EditorCommand>) -> usize {
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.seal();
    stack.push(command);
    stack.seal();
    stack.undo_depth()
}

/// The entity carrying `guid`.
fn entity_with_guid(world: &mut World, guid: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(entity, _)| entity)
}

/// The `component` parameter.
fn component_param(params: &Value) -> Result<String, RpcError> {
    params
        .get("component")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RpcError::invalid_params("`component` (type name, e.g. \"PointLight\") is required"))
}

/// Drops deleted entities from the editor selection.
fn retain_selection(world: &mut World) {
    if let Some(mut selection) = world.remove_resource::<Selection>() {
        selection.retain_alive(world);
        world.insert_resource(selection);
    }
}

/// `editor.history`.
fn editor_history(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    crate::procgen::undo::drain(world);
    let remapped = world
        .get_resource::<crate::command::EntityRemap>()
        .map_or(0, crate::command::EntityRemap::len);
    let stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    Ok(json!({
        "undo": stack.undo_descriptions(),
        "redo": stack.redo_descriptions(),
        "undo_depth": stack.undo_depth(),
        "redo_depth": stack.redo_depth(),
        "dirty": stack.is_dirty(),
        "cap": UNDO_CAP,
        "remapped": remapped,
        "entries": stack.history(),
        "index": stack.undo_depth(),
        "transaction": stack.transaction_label(),
    }))
}

#[derive(Resource)]
struct TransactionOwner(Option<regia_agent::server::ConnectionLease>);

/// The dispatcher's drain boundary (`RequestLifecycle::idle`): no agent request runs now, so the
/// editor's own pushes stay out of an agent transaction (review sprint-w12-rev-g13 #3); then
/// [`commit_abandoned`].
pub(crate) fn request_idle(world: &mut World) {
    // The last request's (or this frame's) panel edits join the shared history first (P-322).
    crate::shared_panel_history::flush(world);
    if world
        .get_resource::<UndoStack>()
        .is_some_and(UndoStack::agent_request)
        && let Some(mut stack) = world.get_resource_mut::<UndoStack>()
    {
        stack.set_agent_request(false);
    }
    commit_abandoned(world);
}

/// Commits commands whose originating transport closed, even on an otherwise idle frame.
pub(crate) fn commit_abandoned(world: &mut World) {
    let closed = world
        .get_resource::<TransactionOwner>()
        .and_then(|owner| owner.0.as_ref())
        .is_some_and(|connection| {
            !connection.is_open()
                && !connection.has_pending_requests()
                && !world
                    .get_resource::<regia_agent::server::ConnectionLease>()
                    .is_some_and(|current| connection.same_connection(current))
        });
    if closed {
        crate::procgen::undo::drain(world);
        let label = world
            .get_resource::<UndoStack>()
            .and_then(|stack| stack.transaction_label())
            .map(str::to_owned);
        if let Some(mut stack) = world.get_resource_mut::<UndoStack>() {
            let _ = stack.transaction_commit();
        }
        world.remove_resource::<TransactionOwner>();
        tracing::warn!(target: "regia_editor", ?label, "committed abandoned agent transaction after connection close");
    }
}

pub(crate) fn transaction_guard(
    world: &mut World,
    method: &str,
    params: &Value,
    mutating: bool,
) -> Result<(), RpcError> {
    commit_abandoned(world);
    // Panel edits recorded since the last boundary join the shared history before this request
    // reads or steps it; a panel's own undo route runs here (P-322, `shared_panel_history`).
    crate::shared_panel_history::before_request(world, method, params)?;
    // An agent request runs until the next drain boundary: its pushes join an agent transaction.
    if let Some(mut stack) = world.get_resource_mut::<UndoStack>() {
        stack.set_agent_request(true);
    }
    if !world
        .get_resource::<UndoStack>()
        .is_some_and(|stack| stack.transaction_label().is_some())
    {
        return Ok(());
    }
    let command_replaces_world = method == "editor.command"
        && matches!(
            params.get("run").and_then(Value::as_str),
            Some("file.new_level" | "file.new_level_discard")
        );
    let menu_replaces_world = method == "editor.menu_invoke"
        && params
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| path.starts_with("File/Recent/"));
    if command_replaces_world
        || menu_replaces_world
        || matches!(
            method,
            "editor.undo"
                | "editor.redo"
                | "editor.history_jump"
                | "editor.tables_panel_undo"
                | "editor.vehicle_tuning_undo"
                | "editor.vehicle_tuning_redo"
                | "editor.open_level"
                | "editor.open_project"
                | "editor.new_project"
                | "editor.open_prefab"
                | "editor.close_prefab"
                | "editor.placed_edit"
                | "editor.recovery_restore"
        )
    {
        return Err(history_error(HistoryError::Active));
    }
    if mutating && let Some(owner) = world.get_resource::<TransactionOwner>() {
        let current = world.get_resource::<regia_agent::server::ConnectionLease>();
        let same = match (&owner.0, current) {
            (Some(owner), Some(current)) => owner.same_connection(current),
            (None, None) => true,
            _ => false,
        };
        if !same {
            let mut error = RpcError::invalid_params("another connection owns the open transaction");
            if let Some(data) = &mut error.data {
                data["reason"] = json!("transaction_owner");
            }
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn history_error(error: HistoryError) -> RpcError {
    let mut result = RpcError::invalid_params(error.to_string());
    if let Some(data) = &mut result.data {
        data["reason"] = json!(error.to_string());
    }
    result
}

fn history_jump(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let index = params
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| history_error(HistoryError::Index))?;
    crate::command::jump_world(world, index).map_err(history_error)?;
    editor_history(world, &Value::Null)
}

fn transaction_begin(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .filter(|label| !label.trim().is_empty())
        .ok_or_else(|| RpcError::invalid_params("label must be a nonempty string"))?;
    crate::procgen::undo::drain(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .transaction_begin_agent(label)
        .map_err(history_error)?;
    let owner = world
        .get_resource::<regia_agent::server::ConnectionLease>()
        .cloned();
    world.insert_resource(TransactionOwner(owner));
    editor_history(world, &Value::Null)
}

fn transaction_commit(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    crate::procgen::undo::drain(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .transaction_commit()
        .map_err(history_error)?;
    world.remove_resource::<TransactionOwner>();
    editor_history(world, &Value::Null)
}

fn transaction_abort(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    crate::procgen::undo::drain(world);
    let mut stack = world.remove_resource::<UndoStack>().unwrap_or_default();
    let result = stack.transaction_abort(world);
    world.insert_resource(stack);
    result.map_err(history_error)?;
    world.remove_resource::<TransactionOwner>();
    retain_selection(world);
    editor_history(world, &Value::Null)
}

/// `editor.spawn`.
fn editor_spawn(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("New entity")
        .to_string();
    let transform = Transform {
        translation: crate::agent_ext::vec3_param(params, "translation")?.unwrap_or(Vec3::ZERO),
        rotation: crate::agent_ext::quat_param(params, "rotation")?.unwrap_or(Quat::IDENTITY),
        scale: crate::agent_ext::vec3_param(params, "scale")?.unwrap_or(Vec3::ONE),
    };
    let parent = optional_entity(world, params, "parent")?;
    let guid = Guid::new_random();
    let mut spawn = SpawnEntities::new_entity(world, guid, &name, transform)
        .map_err(|err| RpcError::internal(format!("spawn: {err}")))?;
    spawn.apply(world);
    let Some(entity) = entity_with_guid(world, guid) else {
        return Err(RpcError::internal("the spawned entity did not appear"));
    };
    let undoable = match parent {
        Some(parent) => {
            let mut group = CommandGroup::new(format!("Create {name}"));
            group.push(Box::new(spawn));
            let mut reparent = Reparent::capture(world, entity, Some(parent));
            reparent.apply(world);
            group.push(Box::new(reparent));
            push_discrete(world, Box::new(group))
        }
        None => push_discrete(world, Box::new(spawn)),
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "guid": guid.to_string(),
        "name": name,
        "parent": parent.map(Entity::to_bits),
        "undoable": undoable,
    }))
}

/// `editor.delete`. `entities` absent = the selection (the Outliner's Delete key, ADR-1354).
fn editor_delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let requested: Vec<u64> = match params.get("entities") {
        None | Some(Value::Null) => world
            .get_resource::<Selection>()
            .map(|selection| selection.entities().iter().map(|e| e.to_bits()).collect())
            .unwrap_or_default(),
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|id| {
                id.as_u64()
                    .ok_or_else(|| RpcError::invalid_params("`entities` holds entity ids (numbers)"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`entities` must be an array of entity ids",
            ));
        }
    };
    let mut entities: Vec<u64> = Vec::with_capacity(requested.len());
    for bits in requested {
        let entity = resolve_entity(world, bits).ok_or_else(|| {
            crate::hierarchy_agent::hierarchy_rpc_error(&HierarchyError::EntityMissing(bits))
        })?;
        if !entities.contains(&entity.to_bits()) {
            entities.push(entity.to_bits());
        }
    }
    if entities.is_empty() {
        return Err(crate::hierarchy_agent::hierarchy_rpc_error(
            &HierarchyError::NoEntities,
        ));
    }
    let deleted = json!(entities);
    let mut command = DespawnEntities {
        entities,
        snapshot: None,
    };
    command.apply(world);
    let undoable = push_discrete(world, Box::new(command));
    retain_selection(world);
    Ok(json!({ "deleted": deleted, "undoable": undoable }))
}

/// `editor.duplicate`: `entity` (one), `entities` or neither (the selection), each subtree
/// copied under its original's parent and moved by `offset`, as one undo entry; the copies
/// become the selection (`crate::hierarchy_ops::duplicate_entities`, the Outliner's Ctrl+D).
fn editor_duplicate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let sources = match (params.get("entities"), params.get("entity")) {
        (None | Some(Value::Null), Some(id)) if !id.is_null() => vec![live_entity(world, params, "entity")?],
        _ => crate::hierarchy_agent::entities_param(world, params)?,
    };
    let offset = crate::hierarchy_agent::offset_param(params)?;
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .seal();
    let copies = crate::hierarchy_ops::duplicate_entities(world, &sources, offset)
        .map_err(|err| crate::hierarchy_agent::hierarchy_rpc_error(&err))?;
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.seal();
    let undoable = stack.undo_depth();
    let first = copies.first().copied();
    Ok(json!({
        "entity": first.map(Entity::to_bits),
        "source": sources.first().map(|e| e.to_bits()),
        "parent": first
            .and_then(|copy| world.get::<ChildOf>(copy))
            .map(|c| c.parent().to_bits()),
        "entities": copies.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
        "undoable": undoable,
    }))
}

/// `editor.add_component`.
fn editor_add_component(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_ecs::reflect::std_traits::ReflectDefault;
    use serde::de::DeserializeSeed as _;
    let entity = live_entity(world, params, "entity")?;
    let component = component_param(params)?;
    // Before taking the registry lock (`read_property` takes it too).
    if crate::command::read_property(world, entity, &component, "").is_ok() {
        return Err(RpcError::invalid_params(format!(
            "entity {} already has {component}; change it with editor.set_property",
            entity.to_bits()
        )));
    }
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let registration = registry
        .get_with_short_type_path(&component)
        .or_else(|| regia_core::brand::lookup_type_path(&component, |p| registry.get_with_type_path(p)))
        .filter(|registration| registration.data::<ReflectComponent>().is_some())
        .ok_or_else(|| {
            RpcError::not_found(
                format!("no reflected component `{component}`"),
                "types.list shows the registered component types",
            )
        })?;
    let short = registration
        .type_info()
        .type_path_table()
        .short_path()
        .to_string();
    let value: Box<dyn PartialReflect> = match params.get("value") {
        None | Some(Value::Null) => registration
            .data::<ReflectDefault>()
            .map(|default| default.default().into_partial_reflect())
            .ok_or_else(|| {
                RpcError::invalid_params(format!("{short} has no reflected default; pass `value`"))
            })?,
        Some(value) => {
            if let Some(missing) =
                crate::agent_ext::first_missing_field(value, registration.type_info(), &registry, "")
            {
                return Err(RpcError::invalid_params(format!(
                    "value does not fit {short}: `{missing}` is missing; give every field"
                )));
            }
            regia_ecs::reflect::serde::TypedReflectDeserializer::new(registration, &registry)
                .deserialize(value)
                .map_err(|err| RpcError::invalid_params(format!("value does not fit {short}: {err}")))?
        }
    };
    drop(registry);
    let mut command = AddComponent::capture(world, entity, &short, value)
        .map_err(|err| RpcError::invalid_params(err.to_string()))?;
    command.apply(world);
    if crate::command::read_property(world, entity, &short, "").is_err() {
        return Err(RpcError::internal(format!("{short} could not be inserted")));
    }
    let undoable = push_discrete(world, Box::new(command));
    Ok(json!({ "entity": entity.to_bits(), "component": short, "undoable": undoable }))
}

/// `editor.remove_component`.
fn editor_remove_component(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = live_entity(world, params, "entity")?;
    let component = component_param(params)?;
    let mut command = RemoveComponent::capture(world, entity, &component).map_err(|err| {
        RpcError::not_found(err.to_string(), "scene.get_entity lists the entity's components")
    })?;
    command.apply(world);
    let undoable = push_discrete(world, Box::new(command));
    Ok(json!({ "entity": entity.to_bits(), "component": component, "undoable": undoable }))
}

/// Wraps native scene validation and behavior with edit-world commands. The hook is never
/// installed in a play world. Batch edits use this recursively and share one implicit group.
pub(crate) fn scene_edit(
    world: &mut World,
    method: &str,
    params: &Value,
    native: regia_agent::SceneEditHandler,
) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("hosted_scene_edit", method).entered();
    let mut remapped = params.clone();
    fn remap(world: &World, params: &mut Value) {
        for key in ["entity", "parent", "controller"] {
            if let Some(bits) = params.get(key).and_then(Value::as_u64)
                && let Some(entity) = resolve_entity(world, bits)
            {
                params[key] = json!(entity.to_bits());
            }
        }
    }
    remap(world, &mut remapped);
    if let Some(edits) = remapped.get_mut("edits").and_then(Value::as_array_mut) {
        for edit in edits {
            remap(world, edit);
        }
    }
    let params = &remapped;

    if method == "scene.season" {
        use regia_framework::season::SeasonState;
        let before = world.get_resource::<SeasonState>().copied();
        let result = native(world, params)?;
        if let (Some(before), Some(after)) = (before, world.get_resource::<SeasonState>().copied())
            && before != after
        {
            push_discrete(world, Box::new(SeasonEdit { before, after }));
        }
        return Ok(result);
    }
    if method == "scene.possess" {
        use regia_framework::player::PlayerController;
        let controller = params
            .get("controller")
            .and_then(Value::as_u64)
            .and_then(Entity::try_from_bits);
        let targets = |world: &World| {
            controller
                .and_then(|entity| world.get::<PlayerController>(entity))
                .map(|pc| (pc.possessed.to_bits(), pc.view_target.to_bits()))
        };
        let before = targets(world);
        let result = native(world, params)?;
        if let (Some(controller), Some(before), Some(after)) = (controller, before, targets(world))
            && before != after
        {
            push_discrete(
                world,
                Box::new(PossessionEdit {
                    controller: controller.to_bits(),
                    before,
                    after,
                }),
            );
        }
        return Ok(result);
    }

    if matches!(method, "scene.set_property" | "scene.apply_edits")
        && params.get("dry_run").and_then(Value::as_bool) == Some(true)
    {
        return native(world, params);
    }
    if method == "scene.apply_edits" {
        let owned = world
            .get_resource::<UndoStack>()
            .is_none_or(|stack| stack.transaction_label().is_none());
        if owned {
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .transaction_begin("Scene edits")
                .map_err(history_error)?;
        }
        let result = native(world, params);
        if owned {
            world
                .resource_mut::<UndoStack>()
                .transaction_commit()
                .map_err(history_error)?;
        }
        return result;
    }
    let entity = params
        .get("entity")
        .and_then(Value::as_u64)
        .and_then(|bits| resolve_entity(world, bits));
    let component = params.get("component").and_then(Value::as_str).unwrap_or("");
    let path = params.get("path").and_then(Value::as_str).unwrap_or("");
    let old = if matches!(
        method,
        "scene.set_property" | "scene.add_component" | "scene.remove_component"
    ) {
        entity.and_then(|entity| {
            crate::command::read_property(
                world,
                entity,
                component,
                if method == "scene.set_property" { path } else { "" },
            )
            .ok()
        })
    } else {
        None
    };
    let mut reparent = entity.map(|entity| Reparent::capture(world, entity, None));
    let mut buried = Vec::new();
    let snapshot = if method == "scene.despawn" {
        entity.map(|entity| {
            let snapshot = crate::command::subtree_snapshot(world, &[entity.to_bits()]);
            buried = crate::command::bury_records(world, &[entity]);
            snapshot
        })
    } else {
        None
    };
    let result = match native(world, params) {
        Ok(result) => result,
        Err(error) => {
            crate::command::unbury_live(world, &buried);
            return Err(error);
        }
    };
    let command: Option<Box<dyn EditorCommand>> = match method {
        "scene.spawn" | "scene.duplicate" | "scene.spawn_primitive" | "scene.spawn_prefab" => {
            if let Some(entity) = result
                .get("entity")
                .and_then(Value::as_u64)
                .and_then(Entity::try_from_bits)
            {
                // Native duplicate copies reflected identity too; each authored copy needs its own guid.
                if method == "scene.duplicate" || world.get::<EntityGuid>(entity).is_none() {
                    world.entity_mut(entity).insert(EntityGuid::new_random());
                }
                if let Some(name) = world.get::<Name>(entity).map(|name| name.as_str().to_owned()) {
                    world
                        .entity_mut(entity)
                        .insert(regia_scene::prelude::Name::new(name))
                        .remove::<Name>();
                }
                if let Some(level) = world
                    .get_resource::<regia_scene::prelude::LevelManager>()
                    .and_then(|levels| levels.current())
                {
                    let mut descendants = vec![entity];
                    let mut seen = std::collections::HashSet::new();
                    while let Some(current) = descendants.pop() {
                        if !seen.insert(current) {
                            continue;
                        }
                        if let Some(children) = world.get::<Children>(current) {
                            descendants.extend_from_slice(children);
                        }
                        let mut current = world.entity_mut(current);
                        current.insert(regia_scene::prelude::LevelMember::from_level(level));
                        if let Some(mut pending) = current.get_mut::<regia_scene::prefab::PendingPrefab>() {
                            pending.level = level.0.as_u128();
                        }
                    }
                }
                let mut command = crate::command::SpawnedRoots {
                    entities: vec![entity.to_bits()],
                    snapshot: None,
                };
                command.apply(world);
                Some(Box::new(command))
            } else {
                None
            }
        }
        "scene.despawn" => entity.zip(snapshot).map(|(entity, snapshot)| {
            Box::new(DespawnEntities {
                entities: vec![entity.to_bits()],
                snapshot: Some(snapshot),
            }) as Box<dyn EditorCommand>
        }),
        "scene.reparent" => {
            if let Some(command) = &mut reparent {
                command.new_parent = entity
                    .and_then(|entity| world.get::<ChildOf>(entity))
                    .map(|parent| parent.parent().to_bits());
            }
            reparent.map(|command| Box::new(command) as Box<dyn EditorCommand>)
        }
        "scene.set_property" => match (entity, old) {
            (Some(entity), Some(old)) => {
                match crate::command::read_property(world, entity, component, path) {
                    Ok(new) => Some(Box::new(crate::command::SetProperty {
                        entity: entity.to_bits(),
                        component: component.into(),
                        path: path.into(),
                        old,
                        new,
                        coalesce: false,
                    })),
                    Err(error) => {
                        revert_unrecorded(world, method, entity, component, path, Some(old));
                        return Err(unrecorded_error(&error));
                    }
                }
            }
            _ => None,
        },
        "scene.add_component" => {
            if let Some(entity) = entity {
                match crate::command::read_property(world, entity, component, "") {
                    Ok(value) => Some(Box::new(AddComponent {
                        entity: entity.to_bits(),
                        component: component.into(),
                        value,
                        previous: old,
                    })),
                    Err(error) => {
                        revert_unrecorded(world, method, entity, component, "", old);
                        return Err(unrecorded_error(&error));
                    }
                }
            } else {
                None
            }
        }
        "scene.remove_component" => entity.zip(old).map(|(entity, old)| {
            Box::new(RemoveComponent {
                entity: entity.to_bits(),
                component: component.into(),
                old: Some(old),
            }) as Box<dyn EditorCommand>
        }),
        _ => None,
    };
    if let Some(command) = command {
        push_discrete(world, command);
    }
    Ok(result)
}

/// Puts back a write the native scene handler applied when its new value cannot be read for the
/// undo entry (review sprint-w12-rev-g13 #4): no applied write is left without an undo entry.
/// `scene.set_property` writes `old` back; `scene.add_component` restores the previous component
/// (`old`) or removes the added one.
fn revert_unrecorded(
    world: &mut World,
    method: &str,
    entity: Entity,
    component: &str,
    path: &str,
    old: Option<Box<dyn PartialReflect>>,
) {
    match method {
        "scene.set_property" => {
            if let Some(old) = old {
                let mut undo = crate::command::SetProperty {
                    entity: entity.to_bits(),
                    component: component.into(),
                    path: path.into(),
                    old,
                    // Never written: only the revert runs.
                    new: Box::new(0u8),
                    coalesce: false,
                };
                undo.revert(world);
            }
        }
        "scene.add_component" => {
            let mut undo = AddComponent {
                entity: entity.to_bits(),
                component: component.into(),
                // Never written: only the revert runs.
                value: Box::new(0u8),
                previous: old,
            };
            undo.revert(world);
        }
        _ => {}
    }
}

/// The error of an edit [`revert_unrecorded`] put back.
fn unrecorded_error(error: &crate::command::CommandError) -> RpcError {
    RpcError::internal(format!(
        "{error}; the value could not be read for undo, so the edit was put back"
    ))
}

// The season driver is session state, not a serialized level setting. Undo restores it without
// marking the level file dirty or introducing a content-format field.
struct SeasonEdit {
    before: regia_framework::season::SeasonState,
    after: regia_framework::season::SeasonState,
}

impl EditorCommand for SeasonEdit {
    fn apply(&mut self, world: &mut World) {
        world.insert_resource(self.after);
    }
    fn revert(&mut self, world: &mut World) {
        world.insert_resource(self.before);
    }
    fn merge(&mut self, _: &dyn EditorCommand) -> bool {
        false
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn description(&self) -> String {
        "Set scene season".into()
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        [
            (
                "year_fraction",
                self.before.year_fraction() != self.after.year_fraction(),
            ),
            ("rate_per_s", self.before.rate_per_s != self.after.rate_per_s),
            ("hold_summer", self.before.hold_summer != self.after.hold_summer),
        ]
        .into_iter()
        .filter(|(_, changed)| *changed)
        .map(|(field, _)| crate::command::CommandTouch {
            document: "session".into(),
            entity: None,
            guid: None,
            asset: None,
            property: Some(format!("SeasonState.{field}")),
        })
        .collect()
    }
}

struct PossessionEdit {
    controller: u64,
    before: (u64, u64),
    after: (u64, u64),
}

impl PossessionEdit {
    fn write(&self, world: &mut World, targets: (u64, u64)) {
        let Some(controller) = resolve_entity(world, self.controller) else {
            return;
        };
        let target = |bits| {
            resolve_entity(world, bits)
                .or_else(|| Entity::try_from_bits(bits))
                .unwrap_or(Entity::PLACEHOLDER)
        };
        let (possessed, view_target) = (target(targets.0), target(targets.1));
        if let Some(mut pc) = world.get_mut::<regia_framework::player::PlayerController>(controller) {
            pc.possessed = possessed;
            pc.view_target = view_target;
        }
    }
}

impl EditorCommand for PossessionEdit {
    fn apply(&mut self, world: &mut World) {
        self.write(world, self.after);
    }
    fn revert(&mut self, world: &mut World) {
        self.write(world, self.before);
    }
    fn merge(&mut self, _: &dyn EditorCommand) -> bool {
        false
    }
    fn description(&self) -> String {
        "Set controller possession".into()
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        ["PlayerController.possessed", "PlayerController.view_target"]
            .into_iter()
            .map(|field| crate::command::CommandTouch::entity(self.controller, Some(field.into())))
            .collect()
    }
}

const ENTITY: ParamSchema = ParamSchema {
    name: "entity",
    kind: "number",
    required: true,
    description: "Entity id (from scene.query_entities; ids from before an undo still work).",
};

const COMPONENT: ParamSchema = ParamSchema {
    name: "component",
    kind: "string",
    required: true,
    description: "Component type name, short or full (\"PointLight\").",
};

/// The undo-history and structural-edit methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.history_jump",
                tool: "undo_history_jump",
                description: "Move to a history boundary, running each undo or redo. Zero is before the oldest retained entry. Refuses transaction_active or history_index.",
                params: &[ParamSchema {
                    name: "index",
                    kind: "integer",
                    required: true,
                    description: "Number of retained entries to leave applied (0..=entries.length).",
                }],
                result: "{ undo, redo, entries, index, transaction, undo_depth, redo_depth, dirty, cap, remapped }",
                mutating: true,
                permission: Permission::EditScene,
            },
            history_jump as Handler,
        ),
        (
            MethodSchema {
                name: "editor.transaction_begin",
                tool: "editor_transaction_begin",
                description: "Collect edits on the shared history as one entry. Nesting is refused with transaction_nested.",
                params: &[ParamSchema {
                    name: "label",
                    kind: "string",
                    required: true,
                    description: "Nonempty description for the grouped undo entry.",
                }],
                result: "{ undo, redo, entries, index, transaction, undo_depth, redo_depth, dirty, cap, remapped }",
                mutating: true,
                permission: Permission::EditScene,
            },
            transaction_begin as Handler,
        ),
        (
            MethodSchema {
                name: "editor.transaction_commit",
                tool: "editor_transaction_commit",
                description: "Commit the open transaction as one undo step; empty transactions add no entry. Refuses transaction_missing.",
                params: &[],
                result: "{ undo, redo, entries, index, transaction, undo_depth, redo_depth, dirty, cap, remapped }",
                mutating: true,
                permission: Permission::EditScene,
            },
            transaction_commit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.transaction_abort",
                tool: "editor_transaction_abort",
                description: "Revert the open transaction in reverse order and preserve the prior redo branch. Refuses transaction_missing.",
                params: &[],
                result: "{ undo, redo, entries, index, transaction, undo_depth, redo_depth, dirty, cap, remapped }",
                mutating: true,
                permission: Permission::EditScene,
            },
            transaction_abort as Handler,
        ),
        (
            MethodSchema {
                name: "editor.history",
                tool: "undo_history",
                description: "The editor's undo stack: what undo and redo would do next, depths, \
                    and whether the level has unsaved edits.",
                params: &[],
                result: "{ undo: [description], redo: [description], undo_depth, redo_depth, dirty, cap, \
                    remapped, entries: [{ index, description, applied, documents, touched }], index, transaction }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_history as Handler,
        ),
        (
            MethodSchema {
                name: "editor.spawn",
                tool: "editor_spawn",
                description: "Create an entity (name, transform, optional parent) as one undoable \
                    entry, like the outliner's create.",
                params: &[
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "Name (default \"New entity\").",
                    },
                    ParamSchema {
                        name: "translation",
                        kind: "array",
                        required: false,
                        description: "[x, y, z] (default origin).",
                    },
                    ParamSchema {
                        name: "rotation",
                        kind: "array",
                        required: false,
                        description: "[x, y, z, w] quaternion (default identity).",
                    },
                    ParamSchema {
                        name: "scale",
                        kind: "array",
                        required: false,
                        description: "[x, y, z] (default 1).",
                    },
                    ParamSchema {
                        name: "parent",
                        kind: "number",
                        required: false,
                        description: "Parent entity id; absent or null = a root.",
                    },
                ],
                result: "{ entity, guid, name, parent, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_spawn as Handler,
        ),
        (
            MethodSchema {
                name: "editor.delete",
                tool: "editor_delete",
                description: "Delete entities with their subtrees as one undoable entry; undo brings \
                    them back with their guids, level, components and place among their siblings. \
                    Refusals carry data.hierarchy_error (hierarchy_no_entities, hierarchy_entity_missing).",
                params: &[ParamSchema {
                    name: "entities",
                    kind: "array",
                    required: false,
                    description: "Entity ids; absent = the editor selection.",
                }],
                result: "{ deleted, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_delete as Handler,
        ),
        (
            MethodSchema {
                name: "editor.duplicate",
                tool: "editor_duplicate",
                description: "Duplicate entities with their subtrees (fresh guids, moved by `offset`, \
                    default 1 m along +x) under their originals' parents, as one undoable entry; the \
                    copies become the selection. Refusals carry data.hierarchy_error.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "One entity id (ids from before an undo still work).",
                    },
                    ParamSchema {
                        name: "entities",
                        kind: "array",
                        required: false,
                        description: "Entity ids; with `entity` also absent = the editor selection.",
                    },
                    ParamSchema {
                        name: "offset",
                        kind: "array",
                        required: false,
                        description: "[x, y, z] added to each copy's local translation (default [1, 0, 0]).",
                    },
                ],
                result: "{ entity, source, parent, entities, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_duplicate as Handler,
        ),
        (
            MethodSchema {
                name: "editor.add_component",
                tool: "editor_add_component",
                description: "Add a reflected component (its default, or `value`), undoable. An \
                    entity that already has it is an error: use editor.set_property.",
                params: &[
                    ENTITY,
                    COMPONENT,
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: false,
                        description: "The whole component in the reflect JSON form scene.get_entity \
                            returns (every field); absent = the type's default.",
                    },
                ],
                result: "{ entity, component, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_add_component as Handler,
        ),
        (
            MethodSchema {
                name: "editor.remove_component",
                tool: "editor_remove_component",
                description: "Remove a component, undoable (undo restores its value).",
                params: &[ENTITY, COMPONENT],
                result: "{ entity, component, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_remove_component as Handler,
        ),
    ]
}

/// Typed entry points over the handlers above, for the editor integration tests. Not a stable
/// API.
#[doc(hidden)]
pub mod test_support {
    use super::*;

    /// Calls the undo method named `method` (`editor.history`, `editor.spawn`, …) directly.
    ///
    /// # Errors
    /// The handler's error; an unknown name is `invalid_params`.
    pub fn call(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
        let handler = methods()
            .into_iter()
            .find(|(schema, _)| schema.name == method)
            .map(|(_, handler)| handler)
            .ok_or_else(|| RpcError::invalid_params(format!("no undo method {method}")))?;
        handler(world, params)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use regia_ecs::reflect::Reflect;

    #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Marker {
        value: f32,
    }

    fn app_with_marker() -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<Marker>();
        app
    }

    /// Review sprint-w12-rev-g13 #4: when the new value cannot be read for the undo entry, the
    /// native write is put back (a property gets its old value; an added component goes, or the
    /// replaced one comes back), so no applied edit is left without an undo entry.
    #[test]
    fn an_edit_whose_value_cannot_be_recorded_is_put_back() {
        let mut app = app_with_marker();
        let world = app.world_mut();
        let entity = world.spawn(Marker { value: 1.0 }).id();
        let old = crate::command::read_property(world, entity, "Marker", "value").unwrap();
        // The native handler wrote 5.0; recording failed.
        world.get_mut::<Marker>(entity).unwrap().value = 5.0;
        revert_unrecorded(world, "scene.set_property", entity, "Marker", "value", Some(old));
        assert_eq!(world.get::<Marker>(entity).unwrap().value, 1.0);

        // An added component that cannot be recorded is removed again.
        let bare = world.spawn_empty().id();
        world.entity_mut(bare).insert(Marker { value: 3.0 });
        revert_unrecorded(world, "scene.add_component", bare, "Marker", "", None);
        assert!(world.get::<Marker>(bare).is_none());

        // A component that replaced an existing one gets the previous value back.
        let previous = crate::command::read_property(world, entity, "Marker", "").unwrap();
        world.entity_mut(entity).insert(Marker { value: 9.0 });
        revert_unrecorded(world, "scene.add_component", entity, "Marker", "", Some(previous));
        assert_eq!(world.get::<Marker>(entity).unwrap().value, 1.0);
        assert_eq!(
            world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
            0,
            "nothing recorded"
        );
    }
}
