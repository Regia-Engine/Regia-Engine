// SPDX-License-Identifier: MPL-2.0
//! Prefab "break link" and the file-backed "apply override" (spec 14 § A3, ADR-7203).
//!
//! Break link turns a prefab instance into plain scene entities: the root's [`PrefabInstance`]
//! and the [`PrefabLink`] (and nested [`PrefabInstance`]) of every prefab-internal entity under
//! it go, the entities stay with their current values (overrides included), and the next save
//! writes them as ordinary entities. A scene entity under the instance (an added child) is not
//! internal and keeps whatever it is, including its own prefab instance. One undo step puts the
//! link back.
//!
//! The file-backed Apply lives with the inspector (`apply_prefab_override`); this module holds
//! its echo record so the asset watcher does not rebuild instances after the editor's own
//! write ([`PrefabWriteEcho`]), and its agent method `editor.prefab_apply_override` (a plain
//! prefab's `.prefab.ron` or, for an instance of a variant, its `.variant.ron`, ADR-7263).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_scene::prefab::{PrefabInstance, PrefabLink};
use serde_json::{Value, json};

use crate::command::{EditorCommand, resolve_entity};
use crate::inspector::EditError;
use crate::viewport_agent::Handler;

/// What [`break_prefab_link`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakLinkReport {
    /// The instance root, as entity bits.
    pub root: u64,
    /// Prefab-internal entities that became plain entities.
    pub unlinked: usize,
    /// Undo entries afterwards.
    pub undo_depth: usize,
}

fn edit_error(code: &'static str, message: impl Into<String>) -> EditError {
    EditError {
        code,
        message: message.into(),
    }
}

/// The outermost instance root that `entity` belongs to: itself when it is a root that is not
/// prefab-internal, else the first ancestor with [`PrefabInstance`] and no [`PrefabLink`].
fn outer_root(world: &World, entity: Entity) -> Option<Entity> {
    let mut cursor = entity;
    for _ in 0..256 {
        let internal = world.get::<PrefabLink>(cursor).is_some();
        if !internal {
            return world.get::<PrefabInstance>(cursor).is_some().then_some(cursor);
        }
        cursor = world.get::<ChildOf>(cursor)?.parent();
    }
    None
}

/// The prefab-internal entities under `root` (depth first, entity order within a level): the
/// walk does not enter an entity that is not internal.
fn internal_members(world: &World, root: Entity) -> Vec<Entity> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(at) = stack.pop() {
        let Some(children) = world.get::<Children>(at) else {
            continue;
        };
        let mut internal: Vec<Entity> = children.to_vec();
        internal.retain(|child| world.get::<PrefabLink>(*child).is_some());
        internal.sort_unstable();
        for child in internal.into_iter().rev() {
            found.push(child);
            stack.push(child);
        }
    }
    found.sort_unstable();
    found
}

/// One entity's prefab bookkeeping, removed by [`BreakPrefabLink`].
#[derive(Clone)]
struct Unlinked {
    entity: u64,
    link: Option<PrefabLink>,
    instance: Option<PrefabInstance>,
}

/// Undo record of a break: the components it removed, put back on revert.
struct BreakPrefabLink {
    removed: Vec<Unlinked>,
}

impl EditorCommand for BreakPrefabLink {
    fn apply(&mut self, world: &mut World) {
        for item in &self.removed {
            if let Some(entity) = resolve_entity(world, item.entity)
                && let Ok(mut entity_mut) = world.get_entity_mut(entity)
            {
                entity_mut.remove::<(PrefabLink, PrefabInstance)>();
            }
        }
    }

    fn revert(&mut self, world: &mut World) {
        for item in &self.removed {
            let Some(entity) = resolve_entity(world, item.entity) else {
                continue;
            };
            let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
                continue;
            };
            if let Some(link) = item.link {
                entity_mut.insert(link);
            }
            if let Some(instance) = &item.instance {
                entity_mut.insert(instance.clone());
            }
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Break prefab link".to_string()
    }
}

/// Breaks the link of the prefab instance `entity` belongs to (the instance root, or any
/// prefab-internal entity of it): see the module docs. One undo step.
///
/// # Errors
/// [`EditError`] `not_found` (no such entity) or `not_prefab_instance`.
pub fn break_prefab_link(world: &mut World, entity: Entity) -> Result<BreakLinkReport, EditError> {
    if world.get_entity(entity).is_err() {
        return Err(edit_error(
            "not_found",
            format!("entity {} does not exist", entity.to_bits()),
        ));
    }
    let root = outer_root(world, entity).ok_or_else(|| {
        edit_error(
            "not_prefab_instance",
            "the entity is not a prefab instance or inside one",
        )
    })?;
    let members = internal_members(world, root);
    let mut removed = Vec::with_capacity(members.len() + 1);
    removed.push(Unlinked {
        entity: root.to_bits(),
        link: None,
        instance: world.get::<PrefabInstance>(root).cloned(),
    });
    for member in &members {
        removed.push(Unlinked {
            entity: member.to_bits(),
            link: world.get::<PrefabLink>(*member).copied(),
            instance: world.get::<PrefabInstance>(*member).cloned(),
        });
    }
    let mut command = BreakPrefabLink { removed };
    command.apply(world);
    let undo_depth = crate::undo_agent::push_discrete(world, Box::new(command));
    Ok(BreakLinkReport {
        root: root.to_bits(),
        unlinked: members.len(),
        undo_depth,
    })
}

/// The text the editor last wrote to each prefab file (Apply override). The asset watcher's
/// reload of a file whose bytes are exactly that text is the editor's own echo, not an outside
/// edit: the live instances already match it, so nothing respawns (and the undo history, whose
/// entries address those entities, stays valid).
#[derive(Resource, Debug, Default)]
pub struct PrefabWriteEcho {
    written: HashMap<PathBuf, String>,
}

impl PrefabWriteEcho {
    /// Records that the editor wrote `text` to `path`.
    pub fn note(&mut self, path: &Path, text: &str) {
        self.written.insert(path.to_path_buf(), text.to_string());
    }

    /// Whether `text` at `path` is the editor's own last write.
    #[must_use]
    pub fn is_echo(&self, path: &Path, text: &str) -> bool {
        self.written.get(path).is_some_and(|written| written == text)
    }
}

/// `editor.prefab_break_link { entity }`.
fn editor_prefab_break_link(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (integer entity id) is required"))?;
    let Some(entity) = resolve_entity(world, bits) else {
        return Err(RpcError::not_found(
            format!("entity {bits} does not exist"),
            "use scene.query_entities; an undo may have removed it",
        ));
    };
    match break_prefab_link(world, entity) {
        Ok(report) => Ok(json!({
            "root": report.root,
            "unlinked": report.unlinked,
            "undo_depth": report.undo_depth,
        })),
        Err(err) if err.code == "not_found" => Err(RpcError::not_found(
            err.message,
            "use scene.query_entities; an undo may have removed it",
        )),
        Err(err) => {
            let mut rpc = RpcError::not_found(
                err.message,
                "select an instance root or an entity inside one (scene.query_entities shows PrefabInstance)",
            );
            if let Some(Value::Object(data)) = rpc.data.as_mut() {
                data.insert("reason".into(), Value::String(err.code.to_string()));
            }
            Err(rpc)
        }
    }
}

/// `editor.prefab_apply_override { entity, component, field }`: the inspector's Apply button.
fn editor_prefab_apply_override(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (integer entity id) is required"))?;
    let component = params
        .get("component")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`component` (a type name or path) is required"))?;
    let field = params.get("field").and_then(Value::as_str).unwrap_or("");
    // The Overrides window's "Apply to" (ADR-7790): omitted is the default target.
    let target = match params.get("target").and_then(Value::as_str) {
        None | Some("" | "default") => None,
        Some(text) => Some(crate::inspector::multi::ApplyTarget::parse(text).ok_or_else(|| {
            let mut rpc = RpcError::invalid_params(format!(
                "unknown target `{text}`: one of nested, outer, variant, base"
            ));
            let data = rpc.data.get_or_insert_with(|| json!({}));
            if let Value::Object(data) = data {
                data.insert("reason".into(), Value::String("apply_target_unknown".into()));
            }
            rpc
        })?),
    };
    let Some(entity) = resolve_entity(world, bits) else {
        return Err(RpcError::not_found(
            format!("entity {bits} does not exist"),
            "use scene.query_entities; an undo may have removed it",
        ));
    };
    let file = match target {
        None => crate::inspector::multi::prefab_apply_file(world, entity),
        Some(target) => crate::inspector::multi::prefab_apply_targets(world, entity, component, field)
            .0
            .into_iter()
            .find(|choice| choice.target == target)
            .map(|choice| choice.file),
    };
    match crate::inspector::multi::apply_prefab_override_to(world, entity, component, field, target) {
        Ok(report) => {
            let root = world
                .get_resource::<crate::agent_ext::EditorProject>()
                .and_then(|project| project.root.clone());
            let file = file.map(|path| {
                root.as_deref()
                    .and_then(|root| path.strip_prefix(root).ok())
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/")
            });
            Ok(json!({
                "edited": report.edited,
                "undo_depth": report.undo_depth,
                "file": file,
            }))
        }
        Err(err) => {
            let mut rpc = if matches!(err.code, "not_found" | "not_prefab_instance" | "override_absent") {
                RpcError::not_found(
                    err.message,
                    "pick an entity inside a prefab instance and a field it overrides (bold rows in the inspector)",
                )
            } else {
                RpcError::invalid_params(err.message)
            };
            let data = rpc.data.get_or_insert_with(|| json!({}));
            if let Value::Object(data) = data {
                data.insert("reason".into(), Value::String(err.code.to_string()));
            }
            Err(rpc)
        }
    }
}

/// The methods of this module.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.prefab_break_link",
                tool: "prefab_break_link",
                description: "Break the link of a prefab instance: the root's PrefabInstance and every prefab-internal entity's PrefabLink are removed, the entities stay with their current values (overrides included) and save as plain entities. Pass the root or any entity inside the instance. One undo step.",
                params: &[ParamSchema {
                    name: "entity",
                    kind: "integer",
                    required: true,
                    description: "The instance root or an entity inside the instance.",
                }],
                result: "{ root, unlinked, undo_depth }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_prefab_break_link as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefab_apply_override",
                tool: "prefab_apply_override",
                description: "Apply one override of a prefab instance (the inspector's Apply button): the value goes into the prefab's .prefab.ron, or for an instance of a prefab variant into the variant's .variant.ron (the base file is untouched), written atomically in the current format; this instance's override goes and every other instance that does not override the field takes the value. A prefab with no file in the project gets the session-only Apply. One undo step.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "integer",
                        required: true,
                        description: "The prefab-internal entity whose field is overridden.",
                    },
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: true,
                        description: "Component type name or path, e.g. Transform.",
                    },
                    ParamSchema {
                        name: "field",
                        kind: "string",
                        required: false,
                        description: "Field path inside the component (translation); empty or omitted for the whole component.",
                    },
                    ParamSchema {
                        name: "target",
                        kind: "string",
                        required: false,
                        description: "Where the value goes: nested (the nested prefab's own file), outer (the outer prefab's file), variant (the variant file) or base (the variant chain's base file); omitted: the default the inspector's Apply picks. editor.prefab_overrides lists each override's valid targets.",
                    },
                ],
                result: "{ edited, undo_depth, file }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_prefab_apply_override as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
