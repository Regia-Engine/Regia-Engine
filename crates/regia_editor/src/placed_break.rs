// SPDX-License-Identifier: MPL-2.0
//! *Break* a placed level in the editor (`docs/specs/37-per-entity-levels-layers-instances.md`
//! § 6.5): the instance entity is replaced by plain entities (the derived guids become the
//! permanent guids), the source level is untouched, and the whole change is one undo step whose
//! undo puts the instance back so the level saves to the same bytes as before.
//!
//! The Layers panel, the Outliner's row menu and the agent method `editor.break_placed` share
//! [`break_placed_undoable`] (`book/agents/levels-layers-instances.md`). `level.break_placed`
//! (the runtime method) does the same break without an undo entry.

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_scene::format::SceneFile;
use regia_scene::level::{LevelId, LevelMember};
use regia_scene::placed::{PlacedLevel, PlacedMode, PlacedSpawned};
use regia_scene::prelude::EntityGuid;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack};
use crate::viewport_agent::Handler;

fn refused(reason: &str, kind: i64, message: impl Into<String>) -> RpcError {
    crate::layers_panel::reason(reason, kind, message)
}

/// Whether `entity` is a placed-level instance (any mode).
#[must_use]
pub fn is_placed(world: &World, entity: Entity) -> bool {
    world.get::<PlacedLevel>(entity).is_some()
}

/// Whether `entity` is an inline placed-level instance whose members are spawned: what *Break*
/// needs.
#[must_use]
pub fn can_break(world: &World, entity: Entity) -> bool {
    world
        .get::<PlacedSpawned>(entity)
        .is_some_and(|spawned| spawned.mode == PlacedMode::Inline)
}

/// The undo entry of a *Break*: the instance record (and the non-member entities under it) for
/// the undo, the members' guids for the redo.
pub struct BreakPlaced {
    /// The instance's guid.
    instance: Guid,
    /// The level the instance belongs to.
    level: Option<LevelId>,
    /// The instance's subtree as a level save writes it (members are not in it: a placed level's
    /// members come from its instance).
    record: SceneFile,
    /// The guids of the former members (filled by the break).
    members: Vec<Guid>,
    /// The entities that hung under the instance or a member without being members themselves:
    /// they live through the Break and its undo, which puts each back where it was.
    outsiders: Vec<Outsider>,
}

/// An entity directly under a placed-level instance or one of its members that is not a member
/// (the host level or the user put it there), and its place before the Break.
#[derive(Clone, Debug)]
struct Outsider {
    /// Its guid.
    guid: Guid,
    /// The guid of its parent: the instance, or a member (members keep derived guids).
    parent: Guid,
    /// Its index among the parent's children.
    sibling: usize,
    /// Its local transform under that parent.
    local: Option<regia_scene::prelude::Transform>,
}

/// The [`Outsider`]s of the inline instance `instance`, by parent then sibling index.
fn outsiders_of(world: &mut World, instance: Entity) -> Vec<Outsider> {
    let mut query = world.query::<(Entity, &regia_scene::placed::PlacedMember)>();
    let members: std::collections::HashSet<Entity> = query
        .iter(world)
        .filter(|(_, member)| member.instance == instance)
        .map(|(entity, _)| entity)
        .collect();
    let mut parents: Vec<Entity> = members.iter().copied().collect();
    parents.sort_unstable();
    parents.insert(0, instance);
    let mut out = Vec::new();
    for parent in parents {
        let (Some(parent_guid), Some(children)) = (
            world.get::<EntityGuid>(parent).map(|g| g.0),
            world.get::<Children>(parent).map(|children| children.to_vec()),
        ) else {
            continue;
        };
        for (sibling, child) in children.into_iter().enumerate() {
            if members.contains(&child) {
                continue;
            }
            if let Some(guid) = world.get::<EntityGuid>(child) {
                out.push(Outsider {
                    guid: guid.0,
                    parent: parent_guid,
                    sibling,
                    local: world.get::<regia_scene::prelude::Transform>(child).copied(),
                });
            }
        }
    }
    out
}

/// `records` without the records (nested ones included) whose guid is in `drop`.
fn records_without(
    records: &[regia_scene::format::SceneEntity],
    drop: &std::collections::HashSet<Guid>,
) -> Vec<regia_scene::format::SceneEntity> {
    records
        .iter()
        .filter(|record| !drop.contains(&record.guid))
        .map(|record| {
            let mut record = record.clone();
            if let Some(instance) = record.prefab_instance.as_mut() {
                instance.added_children = records_without(&instance.added_children, drop);
            }
            record
        })
        .collect()
}

impl BreakPlaced {
    fn entity_of(world: &mut World, guid: Guid) -> Option<Entity> {
        crate::layers_panel::entity_of(world, guid)
    }
}

/// Breaks `entity` with burial records for its subtree, so the undo's `spawn_and_exhume` puts
/// the instance back under its parent at its sibling place and maps its old id
/// (`regia_scene::placed::break_placed` despawns the instance itself, never through
/// `bury_and_despawn`). The records of what the break keeps (the former members and what sits
/// under them) are dropped again.
fn break_with_burial(
    world: &mut World,
    entity: Entity,
) -> Result<Vec<Guid>, regia_scene::placed::PlacedProblem> {
    let noted = crate::command::bury_records(world, &[entity]);
    let result = regia_scene::placed::break_placed(world, entity);
    crate::command::unbury_live(world, &noted);
    result
}

impl EditorCommand for BreakPlaced {
    fn apply(&mut self, world: &mut World) {
        let Some(entity) = Self::entity_of(world, self.instance) else {
            tracing::error!(target: "regia_editor", "redo break: instance {} is gone", self.instance);
            return;
        };
        match break_with_burial(world, entity) {
            Ok(members) => self.members = members,
            Err(err) => tracing::error!(target: "regia_editor", "redo break: {err}"),
        }
    }

    fn revert(&mut self, world: &mut World) {
        // The outsiders stay alive (their ids, and the undo entries that name them, hold): each
        // leaves its parent first so a former member's despawn does not take it, and its record
        // is not spawned again.
        let mut kept: std::collections::HashSet<Guid> = std::collections::HashSet::new();
        let mut alive: Vec<(Entity, Outsider)> = Vec::new();
        for outsider in &self.outsiders {
            let Some(entity) = Self::entity_of(world, outsider.guid) else {
                continue;
            };
            world.entity_mut(entity).remove::<ChildOf>();
            for below in crate::outliner::subtree_of(world, entity) {
                if let Some(guid) = world.get::<EntityGuid>(below) {
                    kept.insert(guid.0);
                }
            }
            alive.push((entity, outsider.clone()));
        }
        // The former members go (their placed copies come back with the instance), then the
        // instance is spawned from its record and filled again: the members get the same derived
        // guids, so selection and references by guid survive.
        let doomed: Vec<(Entity, Guid)> = {
            let mut query = world.query::<(Entity, &EntityGuid)>();
            query
                .iter(world)
                .filter(|(_, guid)| self.members.binary_search(&guid.0).is_ok())
                .map(|(entity, guid)| (entity, guid.0))
                .collect()
        };
        for (entity, _) in &doomed {
            if let Ok(entity) = world.get_entity_mut(*entity) {
                entity.despawn();
            }
        }
        let mut record = self.record.clone();
        record.entities = records_without(&record.entities, &kept);
        crate::command::spawn_and_exhume(world, &record, self.level, "undo break placed level");
        regia_scene::placed::spawn_placed_levels(world);
        // The refilled members are new entities under the same guids: the entries recorded
        // against the former members (a move after the Break) follow them.
        for (old, guid) in doomed {
            if let Some(new) = Self::entity_of(world, guid) {
                crate::command::record_respawn(world, old, new);
            }
        }
        // Each outsider goes back under the restored instance or the refilled member, at its old
        // place among the siblings and with its old local transform.
        for (entity, outsider) in alive {
            let Some(parent) = Self::entity_of(world, outsider.parent) else {
                tracing::warn!(target: "regia_editor", "undo break: the parent of {} is gone; it stays a root", outsider.guid);
                continue;
            };
            if let Some(local) = outsider.local {
                world.entity_mut(entity).insert(local);
            }
            let len = world.get::<Children>(parent).map_or(0, |children| children.len());
            world
                .entity_mut(parent)
                .insert_child(outsider.sibling.min(len), entity);
        }
        if let Some(mut table) = world.get_resource_mut::<regia_scene::layers::LayerTable>() {
            table.mark_dirty();
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Break placed level".to_string()
    }
}

/// Breaks the inline placed level `entity` as one undo step: what the Layers panel's and the
/// Outliner's *Break placed level* and `editor.break_placed` do. Returns the former members'
/// guids, sorted.
///
/// # Errors
/// `placed_not_found` (no such entity), `not_placed` (not an inline instance with spawned
/// members).
pub fn break_placed_undoable(world: &mut World, entity: Entity) -> Result<Vec<Guid>, RpcError> {
    if world.get_entity(entity).is_err() {
        return Err(refused(
            "placed_not_found",
            NOT_FOUND,
            format!("entity {} does not exist", entity.to_bits()),
        ));
    }
    if !can_break(world, entity) {
        return Err(refused(
            "not_placed",
            INVALID_PARAMS,
            format!(
                "entity {} is not an inline placed level with spawned members",
                entity.to_bits()
            ),
        ));
    }
    let Some(instance) = world.get::<EntityGuid>(entity).map(|guid| guid.0) else {
        return Err(refused(
            "not_placed",
            INVALID_PARAMS,
            "the instance has no guid; save and reopen the level",
        ));
    };
    let level = world.get::<LevelMember>(entity).map(LevelMember::level);
    let record = crate::command::subtree_snapshot(world, &[entity.to_bits()]);
    let outsiders = outsiders_of(world, entity);
    let members = break_with_burial(world, entity)
        .map_err(|err| refused(err.reason(), INVALID_PARAMS, err.to_string()))?;
    if let Some(mut table) = world.get_resource_mut::<regia_scene::layers::LayerTable>() {
        table.mark_dirty();
    }
    let command = BreakPlaced {
        instance,
        level,
        record,
        members: members.clone(),
        outsiders,
    };
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(members)
}

/// The egui memory id of a row-menu edit waiting for the Outliner's `&mut World`.
const PENDING_ROW_EDIT: &str = "outliner-placed-row-edit";

/// The Outliner row menu's placed-level and prefab-variant items for one row (`entity`):
/// *Edit placed level* and *Break placed level* on a placed-level instance, *Create variant…* on a
/// prefab instance. The row menu only reads the world: an edit waits in egui memory until
/// [`outliner_frame`] runs it.
pub fn outliner_row_menu(ui: &mut egui::Ui, world: &World, entity: Entity) {
    if is_placed(world, entity) {
        if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::OUTLINER_EDIT_PLACED).clicked() {
            ui.ctx().data_mut(|d| {
                d.insert_temp(
                    egui::Id::new(PENDING_ROW_EDIT),
                    crate::layers_panel::LayerEdit::EditPlaced(entity),
                );
            });
            ui.close();
        }
        if crate::agent_parity::action_enabled(
            ui,
            can_break(world, entity),
            &crate::agent_parity::OUTLINER_BREAK_PLACED,
            None,
        )
        .clicked()
        {
            ui.ctx().data_mut(|d| {
                d.insert_temp(
                    egui::Id::new(PENDING_ROW_EDIT),
                    crate::layers_panel::LayerEdit::BreakPlaced(entity),
                );
            });
            ui.close();
        }
    }
    if world.get::<regia_scene::prefab::PrefabInstance>(entity).is_some()
        && crate::agent_parity::action_menu_item(ui, &crate::agent_parity::OUTLINER_CREATE_VARIANT).clicked()
    {
        match crate::prefab_variant_edit::instance_base_path(world, entity) {
            Some(base) => crate::prefab_variant_edit::open_variant_dialog(ui.ctx(), &base, Some(entity)),
            None => tracing::warn!(
                target: "regia_editor",
                "create variant: the instance's prefab file is not in the project"
            ),
        }
        ui.close();
    }
}

/// Runs the row-menu edit [`outliner_row_menu`] left (errors go to the Layers panel's error line)
/// and draws the Create variant dialog. Called once per Outliner draw.
pub fn outliner_frame(ctx: &egui::Context, world: &mut World) {
    let id = egui::Id::new(PENDING_ROW_EDIT);
    let pending = ctx.data_mut(|d| {
        let edit = d.get_temp::<crate::layers_panel::LayerEdit>(id);
        d.remove::<crate::layers_panel::LayerEdit>(id);
        edit
    });
    if let Some(edit) = pending {
        let error = crate::layers_panel::apply_layer_edit(world, edit)
            .err()
            .map(|e| e.message);
        world
            .get_resource_or_insert_with(crate::layers_panel::LayersPanel::default)
            .error = error;
    }
    crate::prefab_variant_edit::variant_dialog(ctx, world);
}

/// `editor.break_placed { entity }`.
fn editor_break_placed(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (integer) is required"))?;
    let entity = crate::command::resolve_entity(world, bits).ok_or_else(|| {
        refused(
            "placed_not_found",
            NOT_FOUND,
            format!("entity {bits} does not exist"),
        )
    })?;
    let guids = break_placed_undoable(world, entity)?;
    Ok(json!({ "entities": guids.iter().map(Guid::to_string).collect::<Vec<_>>() }))
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.break_placed",
            tool: "break_placed",
            description: "Break an inline placed level as one undo step: its members become plain entities (derived guids kept), the instance entity goes, the source level is untouched. editor.undo puts the instance back. The Layers panel's and the Outliner's Break placed level.",
            params: &[ParamSchema {
                name: "entity",
                kind: "integer",
                required: true,
                description: "The instance entity (level.placed lists them).",
            }],
            result: "{ entities: [guid] }",
            mutating: true,
            permission: Permission::EditScene,
        },
        editor_break_placed as Handler,
    )]
}

#[cfg(test)]
mod tests;
