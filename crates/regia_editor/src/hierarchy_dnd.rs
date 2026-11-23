// SPDX-License-Identifier: MPL-2.0
//! Outliner drag and drop: moving entities in the hierarchy as one undoable edit
//! (`book/agents/editor-hierarchy.md`, `docs/specs/19-editor.md` § 2 "drag to reparent").
//!
//! A drag starts on one row. When that row is selected, the whole selection moves with it;
//! otherwise only the dragged entity moves. Only the top-most entities of the set move (a
//! selected child of a selected parent rides along with its parent). Dropping on a row parents
//! under it; dropping on the "move to the root" strip under the tree unparents.
//!
//! By default the world pose is kept: each moved entity gets the local [`Transform`] that puts
//! it where it was, under the new parent (`new_local = parent_world⁻¹ × world`). Holding Alt
//! keeps the local transform instead, so the entity moves with its new parent. A parent with
//! non-uniform scale and a rotation cannot always be undone exactly into a TRS transform
//! (the shear is dropped), as in any engine that stores TRS.
//!
//! A drop that would put an entity under itself or its own descendant is refused, as is a drop
//! that changes nothing. A refused drop pushes nothing on the undo stack. The whole move is one
//! [`MoveEntities`] entry: one Ctrl+Z restores every parent and local transform.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Affine3A;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack, entity_from_bits};
use crate::outliner::reparent_creates_cycle;
use crate::selection::Selection;
use crate::viewport_agent::Handler;

/// Most ancestors followed when composing a world transform (a guard against corrupt cycles).
const MAX_DEPTH: usize = 1024;

/// Why a drop was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DropRefusal {
    /// The drop would parent an entity under itself or one of its descendants.
    #[error("entity {0:#x} cannot move under itself or its own descendant")]
    Cycle(u64),
    /// Every entity is already under the target.
    #[error("nothing to move: every entity is already there")]
    NothingToMove,
    /// The target parent does not exist.
    #[error("the target entity {0:#x} does not exist")]
    TargetGone(u64),
    /// A moved entity does not exist.
    #[error("entity {0:#x} does not exist")]
    EntityGone(u64),
}

impl DropRefusal {
    /// The agent error code: `reparent_cycle`, `reparent_nothing_to_move`,
    /// `reparent_target_gone` or `reparent_entity_gone`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cycle(_) => "reparent_cycle",
            Self::NothingToMove => "reparent_nothing_to_move",
            Self::TargetGone(_) => "reparent_target_gone",
            Self::EntityGone(_) => "reparent_entity_gone",
        }
    }
}

/// One entity of a [`MoveEntities`] edit.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityMove {
    /// The moved entity (bits).
    pub entity: u64,
    /// Parent before the move (`None` = root).
    pub old_parent: Option<u64>,
    /// Parent after the move (`None` = root).
    pub new_parent: Option<u64>,
    /// Local transform before the move (`None`: the entity has none).
    pub old_local: Option<Transform>,
    /// Local transform after the move (`None`: left as it is).
    pub new_local: Option<Transform>,
}

/// Moves entities under a new parent (or to the root) as one undo entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveEntities {
    /// The moves, in the order they apply (reverted in reverse).
    pub moves: Vec<EntityMove>,
    /// The world pose was kept (the local transforms were recomputed).
    pub keep_world: bool,
}

// Ids resolve through `command::resolve_entity`, so a move still undoes after the moved entities
// (or their parent) were deleted and restored by undo, which re-spawns them under new ids.
fn set_parent(world: &mut World, entity: u64, parent: Option<u64>) {
    let Some(entity) = crate::command::resolve_entity(world, entity) else {
        tracing::warn!(target: "regia_editor", "move: entity gone");
        return;
    };
    match parent {
        Some(bits) => match crate::command::resolve_entity(world, bits) {
            Some(parent) => {
                world.entity_mut(entity).insert(ChildOf(parent));
            }
            None => tracing::warn!(target: "regia_editor", "move: parent gone"),
        },
        None => {
            world.entity_mut(entity).remove::<ChildOf>();
        }
    }
}

fn set_local(world: &mut World, entity: u64, local: Option<Transform>) {
    if let (Some(local), Some(entity)) = (local, crate::command::resolve_entity(world, entity))
        && let Ok(mut entity_mut) = world.get_entity_mut(entity)
    {
        entity_mut.insert(local);
    }
}

impl EditorCommand for MoveEntities {
    fn apply(&mut self, world: &mut World) {
        for step in &self.moves {
            set_parent(world, step.entity, step.new_parent);
            set_local(world, step.entity, step.new_local);
        }
    }

    fn revert(&mut self, world: &mut World) {
        for step in self.moves.iter().rev() {
            set_parent(world, step.entity, step.old_parent);
            if step.new_local.is_some() {
                set_local(world, step.entity, step.old_local);
            }
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        match self.moves.len() {
            1 => "Move in hierarchy".to_string(),
            n => format!("Move {n} entities in hierarchy"),
        }
    }
}

/// `entity`'s world transform composed from the local [`Transform`]s up its parent chain
/// (independent of transform propagation, which the edit world may not have run yet).
#[must_use]
pub fn world_affine(world: &World, entity: Entity) -> Affine3A {
    let mut affine = Affine3A::IDENTITY;
    let mut cursor = Some(entity);
    let mut depth = 0;
    while let Some(current) = cursor {
        if depth == MAX_DEPTH {
            break;
        }
        depth += 1;
        if let Some(local) = world.get::<Transform>(current) {
            affine = local.compute_affine() * affine;
        }
        cursor = world.get::<ChildOf>(current).map(ChildOf::parent);
    }
    affine
}

/// The entities a drag of `dragged` moves: the selection when `dragged` is selected (in
/// selection order), else `dragged` alone.
#[must_use]
pub fn drag_set(dragged: Entity, selection: &Selection) -> Vec<Entity> {
    if selection.contains(dragged) {
        selection.entities().to_vec()
    } else {
        vec![dragged]
    }
}

fn has_ancestor_in(world: &World, entity: Entity, set: &[Entity]) -> bool {
    let mut cursor = world.get::<ChildOf>(entity).map(ChildOf::parent);
    let mut depth = 0;
    while let Some(current) = cursor {
        if set.contains(&current) || depth == MAX_DEPTH {
            return set.contains(&current);
        }
        depth += 1;
        cursor = world.get::<ChildOf>(current).map(ChildOf::parent);
    }
    false
}

/// Plans moving `entities` under `target` (`None` = the root).
///
/// # Errors
///
/// A missing entity or target, a cycle, or nothing to move (see [`DropRefusal`]).
pub fn plan_move(
    world: &World,
    entities: &[Entity],
    target: Option<Entity>,
    keep_world: bool,
) -> Result<MoveEntities, DropRefusal> {
    if let Some(target) = target
        && world.get_entity(target).is_err()
    {
        return Err(DropRefusal::TargetGone(target.to_bits()));
    }
    let mut unique: Vec<Entity> = Vec::with_capacity(entities.len());
    for &entity in entities {
        if world.get_entity(entity).is_err() {
            return Err(DropRefusal::EntityGone(entity.to_bits()));
        }
        if !unique.contains(&entity) {
            unique.push(entity);
        }
    }
    let top: Vec<Entity> = unique
        .iter()
        .copied()
        .filter(|&entity| !has_ancestor_in(world, entity, &unique))
        .collect();
    if let Some(target) = target
        && let Some(&bad) = top
            .iter()
            .find(|&&entity| reparent_creates_cycle(world, entity, target))
    {
        return Err(DropRefusal::Cycle(bad.to_bits()));
    }
    // A singular target (a zero scale) has no inverse: its children keep their local transform
    // (`new_local: None`), which the agent reports as `pose_not_kept`.
    let target_inverse = match target {
        None => Some(Affine3A::IDENTITY),
        Some(target) => {
            let affine = world_affine(world, target);
            (affine.matrix3.determinant().abs() > 1e-12).then(|| affine.inverse())
        }
    };
    let moves: Vec<EntityMove> = top
        .into_iter()
        .filter(|&entity| world.get::<ChildOf>(entity).map(ChildOf::parent) != target)
        .map(|entity| {
            let old_local = world.get::<Transform>(entity).copied();
            let new_local = old_local.filter(|_| keep_world).and_then(|_| {
                target_inverse.map(|inverse| Transform::from_affine(inverse * world_affine(world, entity)))
            });
            EntityMove {
                entity: entity.to_bits(),
                old_parent: world.get::<ChildOf>(entity).map(|c| c.parent().to_bits()),
                new_parent: target.map(Entity::to_bits),
                old_local,
                new_local,
            }
        })
        .collect();
    if moves.is_empty() {
        return Err(DropRefusal::NothingToMove);
    }
    Ok(MoveEntities { moves, keep_world })
}

/// Applies a planned move and pushes it on the undo stack. Returns how many entities moved.
///
/// # Errors
///
/// What [`plan_move`] refuses; nothing changes then.
pub fn move_entities(
    world: &mut World,
    entities: &[Entity],
    target: Option<Entity>,
    keep_world: bool,
) -> Result<usize, DropRefusal> {
    let mut command = plan_move(world, entities, target, keep_world)?;
    let moved = command.moves.len();
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(moved)
}

/// The outliner's drop: moves [`drag_set`] of `dragged` under `target`.
///
/// # Errors
///
/// What [`plan_move`] refuses.
pub fn drop_dragged(
    world: &mut World,
    dragged: Entity,
    target: Option<Entity>,
    keep_world: bool,
) -> Result<usize, DropRefusal> {
    let selection = world.get_resource::<Selection>().cloned().unwrap_or_default();
    let set = drag_set(dragged, &selection);
    move_entities(world, &set, target, keep_world)
}

/// Whether dropping `dragged` on `target` would be accepted (the outliner colors the row).
pub fn drop_allowed(world: &World, dragged: Entity, target: Option<Entity>) -> Result<(), DropRefusal> {
    let selection = world.get_resource::<Selection>().cloned().unwrap_or_default();
    plan_move(world, &drag_set(dragged, &selection), target, true).map(|_| ())
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

fn refusal_error(refusal: &DropRefusal) -> RpcError {
    let (code, hint) = match refusal {
        DropRefusal::TargetGone(_) | DropRefusal::EntityGone(_) => (
            regia_agent::protocol::NOT_FOUND,
            "take entity ids from scene.query_entities",
        ),
        DropRefusal::Cycle(_) => (
            regia_agent::protocol::INVALID_PARAMS,
            "pick a parent outside the moved entities' subtrees",
        ),
        DropRefusal::NothingToMove => (
            regia_agent::protocol::INVALID_PARAMS,
            "the entities are already under that parent",
        ),
    };
    let regia_code = if code == regia_agent::protocol::NOT_FOUND {
        "F0003"
    } else {
        "F0002"
    };
    let mut error = RpcError::new(code, regia_code, refusal.to_string(), hint);
    if let Some(data) = error.data.as_mut() {
        data["reparent_error"] = json!(refusal.code());
        // The Outliner surface's code for the same refusal (`book/agents/hierarchy.md`).
        let hierarchy = match refusal {
            DropRefusal::Cycle(_) => Some("hierarchy_cycle"),
            DropRefusal::TargetGone(_) => Some("hierarchy_parent_missing"),
            DropRefusal::EntityGone(_) => Some("hierarchy_entity_missing"),
            DropRefusal::NothingToMove => None,
        };
        if let Some(hierarchy) = hierarchy {
            data["hierarchy_error"] = json!(hierarchy);
        }
    }
    error
}

fn entity_param(value: &Value, what: &str) -> Result<Entity, RpcError> {
    value
        .as_u64()
        .and_then(entity_from_bits)
        .ok_or_else(|| RpcError::invalid_params(format!("{what} must be an entity id")))
}

/// `editor.reparent`. Takes `entities` (an outliner drop), `entity` (one id, the form of the
/// undo agent surface, ADR-540) or neither (the selection, ADR-1354); ids from before an undo
/// follow their re-spawned entity. `dry_run` validates and reports without applying.
fn reparent_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = match (params.get("entities"), params.get("entity")) {
        (None | Some(Value::Null), Some(id)) if !id.is_null() => {
            let entity = entity_param(id, "`entity`")?;
            vec![crate::command::resolve_entity(world, entity.to_bits()).unwrap_or(entity)]
        }
        _ => crate::hierarchy_agent::entities_param(world, params)?,
    };
    if entities.is_empty() {
        return Err(crate::hierarchy_agent::hierarchy_rpc_error(
            &crate::hierarchy_ops::HierarchyError::NoEntities,
        ));
    }
    let target = match params.get("parent") {
        None | Some(Value::Null) => None,
        Some(value) => Some(entity_param(value, "`parent`")?),
    };
    let keep_world = params.get("keep_world").and_then(Value::as_bool).unwrap_or(true);
    let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let plan = plan_move(world, &entities, target, keep_world).map_err(|refusal| refusal_error(&refusal))?;
    let moved: Vec<u64> = plan.moves.iter().map(|step| step.entity).collect();
    let pose_not_kept: Vec<u64> = plan
        .moves
        .iter()
        .filter(|step| keep_world && step.old_local.is_some() && step.new_local.is_none())
        .map(|step| step.entity)
        .collect();
    let mut unique: Vec<Entity> = Vec::with_capacity(entities.len());
    for &entity in &entities {
        if !unique.contains(&entity) {
            unique.push(entity);
        }
    }
    let carried: Vec<u64> = unique
        .iter()
        .filter(|&&entity| has_ancestor_in(world, entity, &unique))
        .map(|entity| entity.to_bits())
        .collect();
    let unchanged: Vec<u64> = unique
        .iter()
        .map(|entity| entity.to_bits())
        .filter(|bits| !moved.contains(bits) && !carried.contains(bits))
        .collect();
    if !dry_run {
        // A discrete agent edit is its own undo entry (ADR-540): seal on both sides.
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .seal();
        let mut command = plan;
        command.apply(world);
        let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
        stack.push(Box::new(command));
        stack.seal();
    }
    let rows: Vec<Value> = entities
        .iter()
        .map(|&entity| {
            let local = world.get::<Transform>(entity).copied().unwrap_or_default();
            let world_pose = world_affine(world, entity);
            json!({
                "entity": entity.to_bits(),
                "parent": world.get::<ChildOf>(entity).map(|c| c.parent().to_bits()),
                "translation": local.translation.to_array(),
                "rotation": local.rotation.to_array(),
                "scale": local.scale.to_array(),
                "world_translation": world_pose.translation.to_array(),
            })
        })
        .collect();
    let undoable = world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .undo_depth();
    Ok(json!({
        "moved": moved,
        "unchanged": unchanged,
        "carried": carried,
        "pose_not_kept": pose_not_kept,
        "parent": target.map(Entity::to_bits),
        "applied": !dry_run,
        "keep_world": keep_world,
        "entities": rows,
        "undoable": undoable,
    }))
}

/// The hierarchy-move agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.reparent",
            tool: "hierarchy_move",
            description: "Move entities under a new parent (or to the root) the way an outliner drop does: \
                one undo entry, the world pose kept by default (keep_world: false keeps the local \
                transforms), children of moved entities ride along (`carried`), and a drop under an \
                entity's own subtree is refused (data.reparent_error, data.hierarchy_error). `entity` \
                moves one entity; neither moves the selection; ids from before an undo follow the \
                re-spawned entity. dry_run: true validates and reports without applying.",
            params: &[
                ParamSchema {
                    name: "entities",
                    kind: "array",
                    required: false,
                    description: "Entity ids to move (from editor.hierarchy or scene.query_entities); \
                        absent with `entity` absent = the selection.",
                },
                ParamSchema {
                    name: "entity",
                    kind: "number",
                    required: false,
                    description: "One entity id to move (the same as `entities: [id]`).",
                },
                ParamSchema {
                    name: "parent",
                    kind: "number",
                    required: false,
                    description: "The new parent's id; null or absent moves to the root.",
                },
                ParamSchema {
                    name: "keep_world",
                    kind: "boolean",
                    required: false,
                    description: "Keep each entity's world pose (default true).",
                },
                ParamSchema {
                    name: "dry_run",
                    kind: "boolean",
                    required: false,
                    description: "Validate and report without applying (default false).",
                },
            ],
            result: "{ moved, unchanged, carried, pose_not_kept, parent, applied, keep_world, entities: [{entity, parent, translation, rotation, scale, world_translation}], undoable }",
            mutating: true,
            permission: Permission::EditScene,
        },
        reparent_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::SelectMode;
    use regia_core::math::{Quat, Vec3};
    use regia_scene::prelude::Name;

    fn spawn(world: &mut World, name: &str, transform: Transform) -> Entity {
        world.spawn((Name::new(name), transform)).id()
    }

    fn parent_of(world: &World, entity: Entity) -> Option<Entity> {
        world.get::<ChildOf>(entity).map(ChildOf::parent)
    }

    fn near(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    /// `Holder` at x = 10, scaled 2, turned 90° about Y; `Rock` at the root, x = 1.
    fn scene() -> (World, Entity, Entity) {
        let mut world = World::new();
        let holder = spawn(
            &mut world,
            "Holder",
            Transform {
                translation: Vec3::new(10.0, 0.0, 0.0),
                rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                scale: Vec3::splat(2.0),
            },
        );
        let rock = spawn(
            &mut world,
            "Rock",
            Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
        );
        (world, holder, rock)
    }

    #[test]
    fn a_drop_keeps_the_world_pose_and_undoes_in_one_step() {
        let (mut world, holder, rock) = scene();
        let before = world_affine(&world, rock).translation;
        assert_eq!(move_entities(&mut world, &[rock], Some(holder), true), Ok(1));
        assert_eq!(parent_of(&world, rock), Some(holder));
        assert!(
            near(
                Vec3::from(world_affine(&world, rock).translation),
                Vec3::from(before)
            ),
            "the rock did not move in the world"
        );
        let local = world.get::<Transform>(rock).copied().expect("local");
        assert!((local.scale - Vec3::splat(0.5)).length() < 1e-4, "{local:?}");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);

        assert!(crate::command::undo_world(&mut world));
        assert_eq!(parent_of(&world, rock), None);
        assert_eq!(
            world.get::<Transform>(rock).map(|t| t.translation),
            Some(Vec3::new(1.0, 0.0, 0.0)),
            "undo restores the local transform exactly"
        );
        assert!(crate::command::redo_world(&mut world));
        assert_eq!(parent_of(&world, rock), Some(holder));
        assert!(near(
            Vec3::from(world_affine(&world, rock).translation),
            Vec3::new(1.0, 0.0, 0.0)
        ));
    }

    #[test]
    fn alt_keeps_the_local_transform() {
        let (mut world, holder, rock) = scene();
        assert_eq!(move_entities(&mut world, &[rock], Some(holder), false), Ok(1));
        assert_eq!(
            world.get::<Transform>(rock).map(|t| t.translation),
            Some(Vec3::new(1.0, 0.0, 0.0))
        );
        assert!(
            !near(
                Vec3::from(world_affine(&world, rock).translation),
                Vec3::new(1.0, 0.0, 0.0)
            ),
            "the rock follows its new parent"
        );
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(parent_of(&world, rock), None);
    }

    #[test]
    fn dropping_to_the_root_unparents_and_keeps_the_pose() {
        let (mut world, holder, rock) = scene();
        world.entity_mut(rock).insert(ChildOf(holder));
        let before = Vec3::from(world_affine(&world, rock).translation);
        assert_eq!(move_entities(&mut world, &[rock], None, true), Ok(1));
        assert_eq!(parent_of(&world, rock), None);
        let local = world.get::<Transform>(rock).copied().expect("local");
        assert!(near(local.translation, before), "{local:?} vs {before:?}");
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(parent_of(&world, rock), Some(holder));
    }

    #[test]
    fn a_selected_drag_moves_the_top_of_the_selection_as_one_entry() {
        let (mut world, holder, rock) = scene();
        let pebble = spawn(&mut world, "Pebble", Transform::default());
        world.entity_mut(pebble).insert(ChildOf(rock));
        let stick = spawn(&mut world, "Stick", Transform::default());
        let mut selection = Selection::default();
        selection.select(rock, SelectMode::Replace);
        selection.select(pebble, SelectMode::Add);
        selection.select(stick, SelectMode::Add);
        world.insert_resource(selection);

        assert_eq!(
            drop_dragged(&mut world, stick, Some(holder), true),
            Ok(2),
            "rock and stick move; the pebble rides along under the rock"
        );
        assert_eq!(parent_of(&world, rock), Some(holder));
        assert_eq!(parent_of(&world, stick), Some(holder));
        assert_eq!(
            parent_of(&world, pebble),
            Some(rock),
            "the child keeps its parent"
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "one entry");
        assert!(crate::command::undo_world(&mut world));
        assert_eq!((parent_of(&world, rock), parent_of(&world, stick)), (None, None));

        // An unselected row drags alone.
        let loose = spawn(&mut world, "Loose", Transform::default());
        assert_eq!(drop_dragged(&mut world, loose, Some(holder), true), Ok(1));
        assert_eq!(parent_of(&world, rock), None, "the selection stayed put");
    }

    #[test]
    fn cycles_and_no_ops_are_refused_without_an_undo_entry() {
        let (mut world, holder, rock) = scene();
        world.entity_mut(rock).insert(ChildOf(holder));
        assert_eq!(
            move_entities(&mut world, &[holder], Some(rock), true),
            Err(DropRefusal::Cycle(holder.to_bits()))
        );
        assert_eq!(
            move_entities(&mut world, &[holder], Some(holder), true).map_err(|r| r.code()),
            Err("reparent_cycle")
        );
        assert_eq!(
            move_entities(&mut world, &[rock], Some(holder), true),
            Err(DropRefusal::NothingToMove)
        );
        assert_eq!(parent_of(&world, holder), None);
        assert_eq!(parent_of(&world, rock), Some(holder));
        assert!(
            world
                .get_resource::<UndoStack>()
                .is_none_or(|stack| stack.undo_depth() == 0)
        );
        assert!(drop_allowed(&world, rock, None).is_ok());
        assert_eq!(
            drop_allowed(&world, holder, Some(rock)).map_err(|r| r.code()),
            Err("reparent_cycle")
        );
    }

    #[test]
    fn the_agent_method_moves_undoably_and_reports_codes() {
        let (mut world, holder, rock) = scene();
        let reply = reparent_method(
            &mut world,
            &json!({ "entities": [rock.to_bits()], "parent": holder.to_bits() }),
        )
        .expect("editor.reparent");
        assert_eq!(reply["moved"], json!([rock.to_bits()]));
        assert_eq!(reply["keep_world"], true);
        assert_eq!(reply["entities"][0]["parent"], holder.to_bits());
        assert_eq!(reply["undoable"], 1);
        let x = reply["entities"][0]["world_translation"][0].as_f64().expect("x");
        assert!((x - 1.0).abs() < 1e-4, "world pose kept: {x}");

        let code = |err: RpcError| err.data.and_then(|d| d.get("reparent_error").cloned());
        let err = reparent_method(
            &mut world,
            &json!({ "entities": [holder.to_bits()], "parent": rock.to_bits() }),
        )
        .expect_err("cycle");
        assert_eq!(code(err), Some(json!("reparent_cycle")));
        let gone = world.spawn_empty().id();
        assert!(world.despawn(gone));
        let err = reparent_method(
            &mut world,
            &json!({ "entities": [rock.to_bits()], "parent": gone.to_bits() }),
        )
        .expect_err("a parent that does not exist is refused");
        assert_eq!(code(err), Some(json!("reparent_target_gone")));
        let err = reparent_method(&mut world, &json!({ "entities": [] })).expect_err("empty");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);

        let back = reparent_method(
            &mut world,
            &json!({ "entities": [rock.to_bits()], "parent": null, "keep_world": false }),
        )
        .expect("to the root");
        assert_eq!(back["entities"][0]["parent"], Value::Null);
        assert_eq!(back["undoable"], 2);
    }
}
