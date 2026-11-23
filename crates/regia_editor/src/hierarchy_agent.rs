// SPDX-License-Identifier: MPL-2.0
//! The agent-protocol surface of the Outliner's hierarchy edits (`book/agents/hierarchy.md`,
//! ADR-1330): read the tree, group and widen the selection, each edit one undo entry on the
//! shared [`crate::command::UndoStack`].
//!
//! The handlers call the same [`crate::hierarchy_ops`] functions the panel calls. The other
//! three edits keep one method each (ADR-1354): `editor.reparent` lives in
//! [`crate::hierarchy_dnd`], `editor.duplicate` and `editor.delete` in [`crate::undo_agent`];
//! all three take an `entities` list (absent = the selection) and answer refusals with the
//! `hierarchy_*` codes below in `data.hierarchy_error`.

use regia_agent::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::command::{UndoStack, entity_from_bits};
use crate::hierarchy_ops::{self, HierarchyError, SelectScope};
use crate::outliner::{HierarchyNode, display_name, hierarchy_tree};
use crate::selection::{SelectMode, Selection};
use crate::viewport_agent::Handler;

const P_ENTITIES: ParamSchema = ParamSchema {
    name: "entities",
    kind: "array",
    required: false,
    description: "Entity ids; absent = the editor selection.",
};

/// The protocol error for a refused hierarchy edit: the catalog code (`F0002` bad parameters,
/// `F0003` not found, `F0001` internal) in `data.code` and [`HierarchyError::code`] in
/// `data.hierarchy_error`.
#[must_use]
pub fn hierarchy_rpc_error(err: &HierarchyError) -> RpcError {
    let (code, regia_code, hint) = match err {
        HierarchyError::NoEntities => (
            INVALID_PARAMS,
            "F0002",
            "pass `entities` or select some with editor.select",
        ),
        HierarchyError::EntityMissing(_) | HierarchyError::ParentMissing(_) => (
            NOT_FOUND,
            "F0003",
            "take ids from editor.hierarchy or scene.query_entities",
        ),
        HierarchyError::EntityInternal(_) | HierarchyError::ParentInternal(_) => (
            INVALID_PARAMS,
            "F0002",
            "only entities editor.hierarchy lists can be edited",
        ),
        HierarchyError::FeedOwned(_) => (
            INVALID_PARAMS,
            "F0002",
            "bake it with dcc_feed.bake, or change it in the content tool",
        ),
        HierarchyError::Cycle { .. } => (
            INVALID_PARAMS,
            "F0002",
            "the parent is inside a moved subtree; move the parent out first, or pick another parent",
        ),
        HierarchyError::NoGuid(_) => (
            INVALID_PARAMS,
            "F0002",
            "only scene entities (with an EntityGuid) can be duplicated; scene.duplicate copies components",
        ),
        HierarchyError::Scene(_) => (INTERNAL_ERROR, "F0001", "see logs.read for details"),
    };
    let mut error = RpcError::new(code, regia_code, err.to_string(), hint);
    if let Some(data) = error.data.as_mut() {
        data["hierarchy_error"] = json!(err.code());
        if matches!(err, HierarchyError::FeedOwned(_)) {
            data["reason"] = json!("feed_owned");
        }
    }
    error
}

/// The `entities` parameter, or the selection when absent. Ids from before an undo follow
/// their re-spawned entity (`crate::command::resolve_entity`).
pub(crate) fn entities_param(world: &mut World, params: &Value) -> Result<Vec<Entity>, RpcError> {
    match params.get("entities") {
        None | Some(Value::Null) => Ok(world
            .get_resource_or_insert_with::<Selection>(Selection::default)
            .entities()
            .to_vec()),
        Some(Value::Array(items)) => {
            let world: &World = world;
            items
                .iter()
                .map(|item| {
                    let bits = item
                        .as_u64()
                        .ok_or_else(|| RpcError::invalid_params("`entities` must be entity ids"))?;
                    let entity = entity_from_bits(bits)
                        .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
                    Ok(crate::command::resolve_entity(world, bits).unwrap_or(entity))
                })
                .collect()
        }
        Some(_) => Err(RpcError::invalid_params(
            "`entities` must be an array of entity ids",
        )),
    }
}

/// An optional entity parameter (`null`/absent = `None`).
fn optional_entity(params: &Value, name: &str) -> Result<Option<Entity>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let bits = value
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be an entity id or null")))?;
            entity_from_bits(bits)
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
        }
    }
}

fn ids(entities: &[Entity]) -> Vec<u64> {
    entities.iter().map(|e| e.to_bits()).collect()
}

fn undo_depth(world: &mut World) -> usize {
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .undo_depth()
}

/// Every hierarchy method with its handler.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.hierarchy",
                tool: "hierarchy",
                description: "The Outliner's tree: every user entity (editor-internal ones hidden) with its \
                    children sorted by name, as the panel shows it.",
                params: &[
                    ParamSchema {
                        name: "root",
                        kind: "number",
                        required: false,
                        description: "Only this entity's subtree; absent = the whole level.",
                    },
                    ParamSchema {
                        name: "max_depth",
                        kind: "number",
                        required: false,
                        description: "Levels below the roots to include (0 = roots only); absent = all.",
                    },
                    ParamSchema {
                        name: "filter",
                        kind: "string",
                        required: false,
                        description: "Keep the entities this Outliner query matches and their ancestors, like \
                            the panel's search box: name words, kind:, type:, tag:, has:, under:, parent:, \
                            Component.field comparisons, and/or/not, parentheses (book/agents/outliner.md). A \
                            malformed query fails with data.query_error and data.column.",
                    },
                    crate::pie_worlds::P_CLIENT,
                ],
                result: "{ roots: [{ entity, name, guid, parent, selected, kind, role, unclassified, category, children: [..], child_count }], count, client? }",
                mutating: false,
                permission: Permission::Read,
            },
            hierarchy as Handler,
        ),
        (
            MethodSchema {
                name: "editor.group",
                tool: "group",
                description: "Create an empty entity at the centroid of the entities, under their common \
                    parent (the root when they differ), and move them under it keeping their world poses. \
                    One undo entry; the group becomes the selection.",
                params: &[
                    P_ENTITIES,
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "The group's name (default \"Group\").",
                    },
                ],
                result: "{ group, members, parent, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            group as Handler,
        ),
        (
            MethodSchema {
                name: "editor.select_hierarchy",
                tool: "select_hierarchy",
                description: "Select the relatives of entities: their whole subtrees, their direct children \
                    or their parents. An empty entities array is not an error: mode set clears the \
                    selection and mode add leaves it.",
                params: &[
                    P_ENTITIES,
                    ParamSchema {
                        name: "scope",
                        kind: "string",
                        required: true,
                        description: "\"subtree\" | \"children\" | \"parent\".",
                    },
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "\"set\" (default) replaces the selection, \"add\" extends it.",
                    },
                ],
                result: "{ entities: [{entity, name}], primary }",
                mutating: true,
                permission: Permission::EditScene,
            },
            select_hierarchy as Handler,
        ),
    ]
}

/// One tree node as JSON, cut at `depth_left` levels below.
fn node_json(world: &World, selection: &Selection, node: &HierarchyNode, depth_left: Option<u64>) -> Value {
    let children: Vec<Value> = match depth_left {
        Some(0) => Vec::new(),
        _ => node
            .children
            .iter()
            .map(|child| node_json(world, selection, child, depth_left.map(|d| d - 1)))
            .collect(),
    };
    json!({
        "entity": node.entity.to_bits(),
        "name": node.label,
        "guid": world
            .get::<regia_scene::prelude::EntityGuid>(node.entity)
            .map(|g| g.0.to_string()),
        "parent": world.get::<ChildOf>(node.entity).map(|c| c.parent().to_bits()),
        "selected": selection.contains(node.entity),
        // Spec 101 § 4: the row's kind, derived role, unclassified marker and grouping category.
        "kind": node.icon.kind,
        "role": node.icon.role.map(|r| r.name()),
        "unclassified": node.icon.unclassified,
        "category": node.icon.category,
        "child_count": node.children.len(),
        "children": children,
    })
}

fn count_nodes(node: &HierarchyNode) -> usize {
    1 + node.children.iter().map(count_nodes).sum::<usize>()
}

/// Finds `entity`'s node in the tree.
fn find_node(nodes: &[HierarchyNode], entity: Entity) -> Option<&HierarchyNode> {
    nodes.iter().find_map(|node| {
        if node.entity == entity {
            Some(node)
        } else {
            find_node(&node.children, entity)
        }
    })
}

/// `editor.hierarchy`.
fn hierarchy(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // `client`: the tree of that play-in-editor client's world (ADR-2623).
    if let Some(result) = crate::pie_worlds::route_to_client(world, params, hierarchy) {
        return result;
    }
    let root = optional_entity(params, "root")?;
    let max_depth = params.get("max_depth").and_then(Value::as_u64);
    let filter = params.get("filter").and_then(Value::as_str).unwrap_or("");
    // The Outliner's query language (ADR-7757): a malformed filter is an error with its code and
    // column, not an unfiltered tree.
    let query = crate::outliner_query::Query::parse(filter).map_err(|err| {
        let mut error = RpcError::new(
            INVALID_PARAMS,
            "F0002",
            format!("filter: {err}"),
            "see the query grammar in book/agents/outliner.md",
        );
        if let Some(data) = error.data.as_mut() {
            data["query_error"] = json!(err.code);
            data["column"] = json!(err.column);
        }
        error
    })?;
    // Read-only: inserting a resource here would spawn a (resource) entity into the tree.
    let selection = world.get_resource::<Selection>().cloned().unwrap_or_default();
    let tree = crate::outliner::filter_tree_query(&hierarchy_tree(world), &query, &*world);
    let roots: Vec<&HierarchyNode> = match root {
        Some(root) => vec![find_node(&tree, root).ok_or_else(|| {
            let mut error = RpcError::new(
                NOT_FOUND,
                "F0003",
                format!(
                    "entity {} is not in the hierarchy (or `filter` hides it)",
                    root.to_bits()
                ),
                "take ids from editor.hierarchy without `root` or `filter`",
            );
            if let Some(data) = error.data.as_mut() {
                data["hierarchy_error"] = json!("hierarchy_entity_missing");
            }
            error
        })?],
        None => tree.iter().collect(),
    };
    Ok(json!({
        "roots": roots.iter().map(|node| node_json(world, &selection, node, max_depth)).collect::<Vec<_>>(),
        "count": roots.iter().map(|node| count_nodes(node)).sum::<usize>(),
    }))
}

/// `editor.group`.
fn group(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = entities_param(world, params)?;
    let name = params.get("name").and_then(Value::as_str).unwrap_or("Group");
    if name.is_empty() {
        return Err(RpcError::invalid_params("`name` must not be empty"));
    }
    let outcome =
        hierarchy_ops::group_entities(world, &entities, name).map_err(|err| hierarchy_rpc_error(&err))?;
    Ok(json!({
        "group": outcome.group.to_bits(),
        "members": ids(&outcome.members),
        "parent": outcome.parent.map(Entity::to_bits),
        "undoable": undo_depth(world),
    }))
}

/// The `offset` parameter of `editor.duplicate`: `[x, y, z]`, default `[1, 0, 0]`.
pub(crate) fn offset_param(params: &Value) -> Result<Vec3, RpcError> {
    match params.get("offset") {
        None | Some(Value::Null) => Ok(Vec3::X),
        Some(Value::Array(items)) if items.len() == 3 => {
            let mut out = [0.0f32; 3];
            for (slot, item) in out.iter_mut().zip(items) {
                *slot = item
                    .as_f64()
                    .ok_or_else(|| RpcError::invalid_params("`offset` must be numbers"))?
                    as f32;
            }
            Ok(Vec3::from(out))
        }
        Some(_) => Err(RpcError::invalid_params("`offset` must be [x, y, z]")),
    }
}

/// `editor.select_hierarchy`.
fn select_hierarchy(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = entities_param(world, params)?;
    let scope_name = params.get("scope").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`scope` (\"subtree\", \"children\" or \"parent\") is required")
    })?;
    let scope = SelectScope::from_name(scope_name).ok_or_else(|| {
        RpcError::invalid_params(format!("scope {scope_name:?} (subtree, children, parent)"))
    })?;
    let add = match params.get("mode").and_then(Value::as_str).unwrap_or("set") {
        "set" => false,
        "add" => true,
        other => return Err(RpcError::invalid_params(format!("mode {other:?} (set, add)"))),
    };
    for &entity in &entities {
        if world.get_entity(entity).is_err() {
            return Err(hierarchy_rpc_error(&HierarchyError::EntityMissing(
                entity.to_bits(),
            )));
        }
    }
    let picked = hierarchy_ops::scope_entities(world, &entities, scope);
    let mut selection = world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .clone();
    if !add {
        selection.clear();
    }
    for &entity in &picked {
        selection.select(entity, SelectMode::Add);
    }
    let out = json!({
        "entities": selection
            .entities()
            .iter()
            .map(|e| json!({ "entity": e.to_bits(), "name": display_name(world, *e) }))
            .collect::<Vec<_>>(),
        "primary": selection.primary().map(Entity::to_bits),
    });
    world.insert_resource(selection);
    Ok(out)
}
