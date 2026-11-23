// SPDX-License-Identifier: MPL-2.0
//! Multi-entity hierarchy edits for the Outliner and the agent protocol (`docs/specs/19-editor.md`
//! § 2, ADR-1330): reparent a set of entities (drag and drop), group them under a new parent,
//! duplicate, delete and widen the selection to subtrees, children or parents. Each operation is
//! exactly one entry on the [`UndoStack`].
//!
//! Undo entries address entities through [`EntityKey`]: the scene guid when the entity has one,
//! its id bits otherwise. Undoing a delete re-spawns entities from a snapshot under new ids, so a
//! bits-only command further down the stack would no longer find them; a guid still does.
//!
//! The planning functions (`top_level`, `plan_reparent`, `scope_entities`, `world_affine`) are
//! pure reads of the world and unit-tested; the `*_entities` functions apply and push.

use std::collections::{HashMap, HashSet};

use regia_core::id::Guid;
use regia_core::math::{Affine3A, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_scene::prelude::{EntityGuid, Transform};

use crate::command::{EditorCommand, SpawnEntities, UndoStack, entity_from_bits};
use crate::selection::Selection;

/// Deepest ancestor chain the hierarchy walks follow before giving up (a guard against a
/// malformed `ChildOf` cycle, which bevy_ecs does not prevent on direct inserts).
pub(crate) const MAX_DEPTH: usize = 4096;

/// Why a hierarchy edit was refused. [`code`](Self::code) is the stable protocol error code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HierarchyError {
    /// No entities were given and the selection is empty.
    #[error("no entities given and the selection is empty")]
    NoEntities,
    /// An entity does not exist.
    #[error("entity {0} does not exist")]
    EntityMissing(u64),
    /// An entity is editor-internal (window markers, bookkeeping) and cannot be edited.
    #[error("entity {0} is editor-internal")]
    EntityInternal(u64),
    /// The DCC feed owns the entity until a bake (specs/60 § 2.4).
    #[error("entity {0} is owned by the DCC feed")]
    FeedOwned(u64),
    /// The new parent does not exist.
    #[error("parent {0} does not exist")]
    ParentMissing(u64),
    /// The new parent is editor-internal.
    #[error("parent {0} is editor-internal")]
    ParentInternal(u64),
    /// The new parent is the entity itself or one of its descendants; or, for a delete, duplicate
    /// or group, the entity already sits on a malformed `ChildOf` cycle through `parent`.
    #[error("parenting {entity} under {parent} would create a cycle")]
    Cycle {
        /// The moved entity.
        entity: u64,
        /// The requested parent (inside `entity`'s subtree).
        parent: u64,
    },
    /// The entity has no scene guid, so it cannot be snapshotted for duplicate.
    #[error("entity {0} has no EntityGuid and cannot be duplicated")]
    NoGuid(u64),
    /// The scene snapshot or spawn behind a duplicate or group failed.
    #[error("scene: {0}")]
    Scene(String),
}

impl HierarchyError {
    /// The lowercase protocol error code (documented in `book/agents/hierarchy.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoEntities => "hierarchy_no_entities",
            Self::EntityMissing(_) => "hierarchy_entity_missing",
            Self::EntityInternal(_) => "hierarchy_entity_internal",
            Self::FeedOwned(_) => "hierarchy_feed_owned",
            Self::ParentMissing(_) => "hierarchy_parent_missing",
            Self::ParentInternal(_) => "hierarchy_parent_internal",
            Self::Cycle { .. } => "hierarchy_cycle",
            Self::NoGuid(_) => "hierarchy_no_guid",
            Self::Scene(_) => "hierarchy_scene",
        }
    }
}

/// How an undo entry finds an entity again: by scene guid when it has one (survives a
/// delete + undo, which re-spawns under new ids), else by id bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityKey {
    /// The entity's `EntityGuid`, when it has one.
    pub guid: Option<Guid>,
    /// The entity id bits at capture time.
    pub bits: u64,
}

impl EntityKey {
    /// Captures `entity`'s key now.
    #[must_use]
    pub fn capture(world: &World, entity: Entity) -> Self {
        Self {
            guid: world.get::<EntityGuid>(entity).map(|g| g.0),
            bits: entity.to_bits(),
        }
    }

    /// The live entity this key names, if any.
    #[must_use]
    pub fn resolve(&self, world: &World) -> Option<Entity> {
        let by_bits = entity_from_bits(self.bits).filter(|e| world.get_entity(*e).is_ok());
        let Some(guid) = self.guid else {
            return by_bits;
        };
        // Fast path: the id still names the same guid.
        if let Some(entity) = by_bits
            && world.get::<EntityGuid>(entity).is_some_and(|g| g.0 == guid)
        {
            return Some(entity);
        }
        guid_entity(world, guid)
    }
}

/// The live entity carrying `guid`.
pub(crate) fn guid_entity(world: &World, guid: Guid) -> Option<Entity> {
    world
        .iter_entities()
        .map(|entity_ref| entity_ref.id())
        .find(|entity| world.get::<EntityGuid>(*entity).is_some_and(|g| g.0 == guid))
}

/// `entity`'s ancestors, nearest first (cycle-guarded).
#[must_use]
pub fn ancestors(world: &World, entity: Entity) -> Vec<Entity> {
    let mut out = Vec::new();
    let mut cursor = world.get::<ChildOf>(entity).map(ChildOf::parent);
    while let Some(current) = cursor {
        if current == entity || out.contains(&current) || out.len() >= MAX_DEPTH {
            break;
        }
        out.push(current);
        cursor = world.get::<ChildOf>(current).map(ChildOf::parent);
    }
    out
}

/// `entity`'s world matrix from the local `Transform`s of its ancestor chain (missing
/// `Transform` = identity). Unlike `GlobalTransform` it is never a frame stale: the editor edits
/// between propagation runs.
#[must_use]
pub fn world_affine(world: &World, entity: Entity) -> Affine3A {
    let local = |e: Entity| {
        world
            .get::<Transform>(e)
            .map_or(Affine3A::IDENTITY, Transform::compute_affine)
    };
    ancestors(world, entity)
        .into_iter()
        .fold(local(entity), |affine, ancestor| local(ancestor) * affine)
}

/// The entities of `entities` none of whose ancestors are also in the set, in input order with
/// duplicates removed. Moving, duplicating or deleting an ancestor carries its subtree, so the
/// descendants must not be edited a second time (they would be flattened out of it).
#[must_use]
pub fn top_level(world: &World, entities: &[Entity]) -> Vec<Entity> {
    let set: HashSet<Entity> = entities.iter().copied().collect();
    let mut out: Vec<Entity> = Vec::new();
    for &entity in entities {
        if out.contains(&entity) || ancestors(world, entity).iter().any(|a| set.contains(a)) {
            continue;
        }
        out.push(entity);
    }
    out
}

/// Checks that every entity exists and is user content.
pub(crate) fn validate_entities(world: &World, entities: &[Entity]) -> Result<(), HierarchyError> {
    if entities.is_empty() {
        return Err(HierarchyError::NoEntities);
    }
    for &entity in entities {
        if world.get_entity(entity).is_err() {
            return Err(HierarchyError::EntityMissing(entity.to_bits()));
        }
        if crate::visibility::is_hidden(world, entity) {
            return Err(HierarchyError::EntityInternal(entity.to_bits()));
        }
        if world.get::<regia_scene::prelude::FeedOwned>(entity).is_some() {
            return Err(HierarchyError::FeedOwned(entity.to_bits()));
        }
    }
    Ok(())
}

/// Refuses a set holding an entity that sits on a `ChildOf` cycle (its parent chain comes back to
/// it). [`top_level`] drops every member of such a cycle, so a delete, duplicate or group of it
/// would quietly act on nothing (a group would also spawn at a NaN centroid).
pub(crate) fn refuse_parent_cycles(world: &World, entities: &[Entity]) -> Result<(), HierarchyError> {
    for &entity in entities {
        let Some(parent) = world.get::<ChildOf>(entity).map(ChildOf::parent) else {
            continue;
        };
        let mut cursor = Some(parent);
        let mut steps = 0;
        while let Some(current) = cursor {
            if current == entity {
                return Err(HierarchyError::Cycle {
                    entity: entity.to_bits(),
                    parent: parent.to_bits(),
                });
            }
            steps += 1;
            if steps > MAX_DEPTH {
                break;
            }
            cursor = world.get::<ChildOf>(current).map(ChildOf::parent);
        }
    }
    Ok(())
}

/// One entity's move inside a [`ReparentEntities`] command.
#[derive(Debug, Clone)]
pub struct ReparentMove {
    /// The moved entity.
    pub entity: EntityKey,
    /// Parent before the move (`None` = root).
    pub old_parent: Option<EntityKey>,
    /// Parent after the move (`None` = root).
    pub new_parent: Option<EntityKey>,
    /// Local transform before the move (`None` = the entity has no `Transform`).
    pub old_local: Option<Transform>,
    /// Local transform after the move that keeps the world pose (`None` = keep the local
    /// transform: plain reparent, no `Transform`, or a singular new parent).
    pub new_local: Option<Transform>,
}

/// Moves several entities under one new parent (or to the root) as one undo entry, optionally
/// keeping each entity's world pose. Built by [`plan_reparent`].
#[derive(Debug, Clone, Default)]
pub struct ReparentEntities {
    /// The moves, applied in order and reverted in reverse.
    pub moves: Vec<ReparentMove>,
    /// The Outliner folders (`EditorFolder`, ADR-7757) the last apply took off moved entities:
    /// a folder belongs to a top-level entity only, so an entity that gets a parent, or leaves one,
    /// drops it (filled by `apply`, restored by `revert`; leave it empty when building).
    pub cleared_folders: Vec<(EntityKey, regia_scene::components::EditorFolder)>,
}

/// Takes the Outliner folder off `entity` when a move gives it a parent or takes one away
/// (review sprint-w12-rev-early-k row 10): a folder kept through such a move would reappear on its
/// own once the entity is at the root again, even after the folder was renamed or deleted.
pub(crate) fn take_stale_folder(
    world: &mut World,
    entity: Entity,
    had_parent: bool,
    gets_parent: bool,
) -> Option<regia_scene::components::EditorFolder> {
    if !(had_parent || gets_parent) {
        return None;
    }
    world
        .entity_mut(entity)
        .take::<regia_scene::components::EditorFolder>()
}

/// Sets or clears `entity`'s parent; `false` when the parent key no longer resolves.
fn set_parent(world: &mut World, entity: Entity, parent: Option<&EntityKey>) -> bool {
    match parent {
        Some(key) => {
            let Some(parent) = key.resolve(world) else {
                return false;
            };
            world.entity_mut(entity).insert(ChildOf(parent));
        }
        None => {
            world.entity_mut(entity).remove::<ChildOf>();
        }
    }
    true
}

impl EditorCommand for ReparentEntities {
    fn apply(&mut self, world: &mut World) {
        self.cleared_folders.clear();
        for step in &self.moves {
            let Some(entity) = step.entity.resolve(world) else {
                tracing::warn!(target: "regia_editor", "reparent: entity gone");
                continue;
            };
            let had_parent = world.get::<ChildOf>(entity).is_some();
            if !set_parent(world, entity, step.new_parent.as_ref()) {
                tracing::warn!(target: "regia_editor", "reparent: parent gone");
                continue;
            }
            if let Some(folder) = take_stale_folder(world, entity, had_parent, step.new_parent.is_some()) {
                self.cleared_folders.push((step.entity, folder));
            }
            if let Some(local) = step.new_local {
                world.entity_mut(entity).insert(local);
            }
        }
    }

    fn revert(&mut self, world: &mut World) {
        for step in self.moves.iter().rev() {
            let Some(entity) = step.entity.resolve(world) else {
                tracing::warn!(target: "regia_editor", "undo reparent: entity gone");
                continue;
            };
            if !set_parent(world, entity, step.old_parent.as_ref()) {
                tracing::warn!(target: "regia_editor", "undo reparent: old parent gone");
                continue;
            }
            if step.new_local.is_some()
                && let Some(local) = step.old_local
            {
                world.entity_mut(entity).insert(local);
            }
        }
        for (key, folder) in self.cleared_folders.drain(..) {
            if let Some(entity) = key.resolve(world) {
                world.entity_mut(entity).insert(folder);
            }
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        match self.moves.len() {
            1 => "Reparent".to_string(),
            n => format!("Reparent {n} entities"),
        }
    }
}

/// What [`plan_reparent`] decided.
#[derive(Debug, Clone)]
pub struct ReparentPlan {
    /// The command (not yet applied). Empty when nothing moves.
    pub command: ReparentEntities,
    /// Entities that move, in input order.
    pub moved: Vec<Entity>,
    /// Entities already under the requested parent (no-op).
    pub unchanged: Vec<Entity>,
    /// Entities skipped because an ancestor of theirs is also moved (they travel with it).
    pub carried: Vec<Entity>,
    /// Moved entities whose world pose could not be kept (singular new parent): their local
    /// transform stays.
    pub pose_not_kept: Vec<Entity>,
}

/// Plans moving `entities` under `parent` (`None` = root). With `keep_world` each moved entity
/// gets the local transform that keeps its world pose under the new parent.
///
/// # Errors
/// A [`HierarchyError`] when an entity or the parent is missing or internal, or when the parent
/// is inside a moved entity's subtree (the whole call is refused; nothing is half-applied).
pub fn plan_reparent(
    world: &World,
    entities: &[Entity],
    parent: Option<Entity>,
    keep_world: bool,
) -> Result<ReparentPlan, HierarchyError> {
    validate_entities(world, entities)?;
    if let Some(parent) = parent {
        if world.get_entity(parent).is_err() {
            return Err(HierarchyError::ParentMissing(parent.to_bits()));
        }
        if crate::visibility::is_hidden(world, parent) {
            return Err(HierarchyError::ParentInternal(parent.to_bits()));
        }
        for &entity in entities {
            if crate::outliner::reparent_creates_cycle(world, entity, parent) {
                return Err(HierarchyError::Cycle {
                    entity: entity.to_bits(),
                    parent: parent.to_bits(),
                });
            }
        }
    }
    let top = top_level(world, entities);
    let carried: Vec<Entity> =
        entities
            .iter()
            .copied()
            .filter(|e| !top.contains(e))
            .fold(Vec::new(), |mut acc, e| {
                if !acc.contains(&e) {
                    acc.push(e);
                }
                acc
            });
    // The parent is outside every moved subtree (cycle check), so its world matrix is the same
    // before and after the moves: compute it once.
    let parent_inverse = parent.and_then(|p| {
        let affine = world_affine(world, p);
        (affine.matrix3.determinant().abs() > 1e-12).then(|| affine.inverse())
    });
    let parent_key = parent.map(|p| EntityKey::capture(world, p));

    let mut plan = ReparentPlan {
        command: ReparentEntities::default(),
        moved: Vec::new(),
        unchanged: Vec::new(),
        carried,
        pose_not_kept: Vec::new(),
    };
    for entity in top {
        let current = world.get::<ChildOf>(entity).map(ChildOf::parent);
        if current == parent {
            plan.unchanged.push(entity);
            continue;
        }
        let old_local = world.get::<Transform>(entity).copied();
        let new_local = match (keep_world, old_local, parent) {
            (true, Some(_), None) => Some(Transform::from_affine(world_affine(world, entity))),
            (true, Some(_), Some(_)) => match parent_inverse {
                Some(inverse) => Some(Transform::from_affine(inverse * world_affine(world, entity))),
                None => {
                    plan.pose_not_kept.push(entity);
                    None
                }
            },
            _ => None,
        };
        plan.command.moves.push(ReparentMove {
            entity: EntityKey::capture(world, entity),
            old_parent: current.map(|p| EntityKey::capture(world, p)),
            new_parent: parent_key,
            old_local,
            new_local,
        });
        plan.moved.push(entity);
    }
    Ok(plan)
}

/// Pushes an already-applied command on the world's undo stack.
fn push_undo(world: &mut World, command: Box<dyn EditorCommand>) {
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(command);
}

/// Plans, applies and pushes a reparent (one undo entry; nothing is pushed when nothing moves).
///
/// # Errors
/// See [`plan_reparent`].
pub fn reparent_entities(
    world: &mut World,
    entities: &[Entity],
    parent: Option<Entity>,
    keep_world: bool,
) -> Result<ReparentPlan, HierarchyError> {
    let plan = plan_reparent(world, entities, parent, keep_world)?;
    if !plan.command.moves.is_empty() {
        let mut command = plan.command.clone();
        command.apply(world);
        push_undo(world, Box::new(command));
    }
    Ok(plan)
}

/// Several commands as one undo entry: applied in order, reverted in reverse.
pub struct CommandBatch {
    /// What the entry does, for the Edit menu.
    pub label: String,
    /// The commands.
    pub commands: Vec<Box<dyn EditorCommand>>,
}

impl EditorCommand for CommandBatch {
    fn apply(&mut self, world: &mut World) {
        for command in &mut self.commands {
            command.apply(world);
        }
    }

    fn revert(&mut self, world: &mut World) {
        for command in self.commands.iter_mut().rev() {
            command.revert(world);
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

/// Deletes `entities` with their subtrees as one undo entry (a
/// [`crate::command::DespawnEntities`]); undo re-spawns them under their old parents and redo
/// deletes them again. Returns the deleted roots. Prunes the selection.
///
/// Entities without an `EntityGuid` are deleted but cannot come back on undo (the undo snapshot
/// is a scene file, keyed by guid).
///
/// # Errors
/// When an entity is missing or internal, or none is given, or one sits on a `ChildOf` cycle.
pub fn delete_entities(world: &mut World, entities: &[Entity]) -> Result<Vec<Entity>, HierarchyError> {
    validate_entities(world, entities)?;
    refuse_parent_cycles(world, entities)?;
    let top = top_level(world, entities);
    // The undo stack's own delete (ADR-540): undo restores each root's parent, level and
    // sibling place and records the new ids in `EntityRemap`, so entries keyed by id bits
    // (`hierarchy_dnd::MoveEntities`, the inspector's edits) still find them (ADR-1354).
    let mut command = crate::command::DespawnEntities {
        entities: top.iter().map(|e| e.to_bits()).collect(),
        snapshot: None,
    };
    command.apply(world);
    push_undo(world, Box::new(command));
    let mut selection = world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .clone();
    selection.retain_alive(world);
    world.insert_resource(selection);
    Ok(top)
}

/// Serializes `transform` the way scene files store it.
fn transform_raw(world: &World, transform: &Transform) -> Option<Box<ron::value::RawValue>> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut codec = regia_ecs::serialize::ReflectCodec::new(&registry);
    let text = codec.to_ron_typed(transform.as_partial_reflect()).ok()?;
    ron::value::RawValue::from_ron(text.trim_end())
        .ok()
        .map(|raw| raw.to_owned())
}

/// The scene type path of `Transform` in scene files.
const TRANSFORM_KEY: &str = "regia_scene::transform::Transform";

/// Duplicates `entities` (with their subtrees) as one undo entry: fresh guids, each copy's local
/// translation moved by `offset`, under the same parent as its original. Returns the copies'
/// roots in input order; the copies become the selection.
///
/// # Errors
/// When an entity is missing, internal or has no `EntityGuid`, or none is given, or one sits on a
/// `ChildOf` cycle.
pub fn duplicate_entities(
    world: &mut World,
    entities: &[Entity],
    offset: Vec3,
) -> Result<Vec<Entity>, HierarchyError> {
    validate_entities(world, entities)?;
    refuse_parent_cycles(world, entities)?;
    let top = top_level(world, entities);
    let mut source_guids: Vec<Guid> = Vec::with_capacity(top.len());
    for &entity in &top {
        let guid = world
            .get::<EntityGuid>(entity)
            .map(|g| g.0)
            .ok_or_else(|| HierarchyError::NoGuid(entity.to_bits()))?;
        source_guids.push(guid);
    }
    let mut subtree_guids: HashSet<Guid> = HashSet::new();
    for &entity in &top {
        for member in crate::outliner::subtree_of(world, entity) {
            if let Some(guid) = world.get::<EntityGuid>(member) {
                subtree_guids.insert(guid.0);
            }
        }
    }
    let filter = regia_scene::spawn::SnapshotFilter {
        level: None,
        resources: Vec::new(),
    };
    let mut file = regia_scene::prelude::snapshot_scene(world, &filter)
        .map_err(|err| HierarchyError::Scene(err.to_string()))?;
    // Entities added on top of a prefab instance sit in its record's `added_children`: a copied
    // added child is lifted out of an instance that is not copied (ADR-7412).
    file.entities = crate::command::kept_records(std::mem::take(&mut file.entities), &subtree_guids);

    // Fresh guids for every copied record, nested added children included: a copied instance
    // must not bring its added children back under their original guids.
    let remap: HashMap<Guid, Guid> = crate::command::record_guids(&file.entities)
        .into_iter()
        .map(|guid| (guid, Guid::new_random()))
        .collect();
    // Copy names in input order, so several copies take consecutive numbers (ADR-7757).
    let mut copy_names = CopyNames::of_world(world);
    let copy_name: HashMap<Guid, String> = source_guids
        .iter()
        .filter_map(|guid| {
            let original = guid_entity(world, *guid)?;
            let name = world.get::<regia_scene::prelude::Name>(original)?;
            Some((*guid, copy_names.next_name(name.as_str())))
        })
        .collect();
    for record in &mut file.entities {
        if let Some(instance) = &mut record.prefab_instance {
            remap_added_children(&mut instance.added_children, &remap);
        }
        let source = record.guid;
        record.guid = remap.get(&source).copied().unwrap_or(source);
        record.parent = record.parent.and_then(|parent| remap.get(&parent).copied());
        // A copy root takes the next free numbered name ("Lamp" -> "Lamp 2", ADR-7757).
        if let Some(name) = copy_name.get(&source) {
            record.name = Some(name.clone());
        }
        // Offset the copy roots in their parent's space.
        if source_guids.contains(&source)
            && let Some(original) = guid_entity(world, source)
            && let Some(transform) = world.get::<Transform>(original).copied()
            && let Some(raw) = transform_raw(
                world,
                &Transform {
                    translation: transform.translation + offset,
                    ..transform
                },
            )
        {
            record.components.insert(TRANSFORM_KEY.to_string(), raw);
        }
    }
    // A copied reflection capture takes an unused id, or the next bake refuses the level
    // (ADR-7663). In the file, so redo brings the copy back with the same id.
    crate::shapes::fresh_capture_ids(world, &mut file.entities);

    let mut spawn = SpawnEntities {
        file,
        spawned_guids: Vec::new(),
        spawned_entities: Vec::new(),
    };
    spawn.apply(world);
    // Copies of non-root originals go under the original's parent (the scene file cannot
    // name a parent outside itself, so the spawn put them at the root).
    let mut relink = ReparentEntities::default();
    let mut copies: Vec<Entity> = Vec::with_capacity(top.len());
    for (&original, source) in top.iter().zip(&source_guids) {
        let Some(copy_guid) = remap.get(source).copied() else {
            continue;
        };
        let Some(copy) = guid_entity(world, copy_guid) else {
            continue;
        };
        copies.push(copy);
        if let Some(parent) = world.get::<ChildOf>(original).map(ChildOf::parent) {
            relink.moves.push(ReparentMove {
                entity: EntityKey::capture(world, copy),
                old_parent: None,
                new_parent: Some(EntityKey::capture(world, parent)),
                old_local: None,
                new_local: None,
            });
        }
    }
    relink.apply(world);
    let batch = CommandBatch {
        label: match top.len() {
            1 => "Duplicate entity".to_string(),
            n => format!("Duplicate {n} entities"),
        },
        commands: vec![Box::new(spawn), Box::new(relink)],
    };
    push_undo(world, Box::new(batch));
    select_exactly(world, &copies);
    Ok(copies)
}

/// The duplicate naming rule (ADR-7757): a name splits into a stem and an optional trailing
/// number after a space ("Lamp 3" is stem "Lamp", number 3; a bare "Lamp" counts as number 1).
/// A copy is named "`<stem> <n>`", where n is one more than the highest number any entity of the
/// level uses with that stem, so duplicating "Lamp" gives "Lamp 2", then "Lamp 3", and
/// duplicating "Lamp 2" also gives the next free "`Lamp <n>`". Several copies made at once take
/// consecutive numbers. Only the copied roots are renamed; their children keep their names.
#[derive(Debug, Clone, Default)]
pub struct CopyNames {
    /// The highest number in use per stem.
    highest: HashMap<String, u64>,
}

impl CopyNames {
    /// The numbers in use by every named entity of `world`.
    #[must_use]
    pub fn of_world(world: &World) -> Self {
        let mut names = Self::default();
        for entity in world.iter_entities() {
            if let Some(name) = entity.get::<regia_scene::prelude::Name>() {
                names.note(name.as_str());
            }
        }
        names
    }

    /// Splits `name` into its stem and number ("Lamp 12" -> ("Lamp", 12), "Lamp" -> ("Lamp", 1)).
    #[must_use]
    pub fn split(name: &str) -> (&str, u64) {
        if let Some((stem, digits)) = name.rsplit_once(' ')
            && !stem.trim().is_empty()
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(number) = digits.parse::<u64>()
        {
            return (stem, number);
        }
        (name, 1)
    }

    /// Records a name in use.
    pub fn note(&mut self, name: &str) {
        let (stem, number) = Self::split(name);
        let highest = self.highest.entry(stem.to_string()).or_insert(0);
        *highest = (*highest).max(number);
    }

    /// The copy name for `name`, recorded as in use.
    pub fn next_name(&mut self, name: &str) -> String {
        let (stem, _) = Self::split(name);
        let highest = self.highest.entry(stem.to_string()).or_insert(1);
        *highest = highest.saturating_add(1).max(2);
        format!("{stem} {}", *highest)
    }
}

/// Gives nested `added_children` records (and the instances nested in them) their copies' guids.
/// A parent reference to another added child moves with it; one to a prefab-internal local id
/// stays (the copy's instance spawns its own internal entities under the same local ids).
fn remap_added_children(children: &mut [regia_scene::prelude::SceneEntity], remap: &HashMap<Guid, Guid>) {
    let mut stack: Vec<&mut regia_scene::prelude::SceneEntity> = children.iter_mut().collect();
    while let Some(record) = stack.pop() {
        record.guid = remap.get(&record.guid).copied().unwrap_or(record.guid);
        record.parent = record
            .parent
            .map(|parent| remap.get(&parent).copied().unwrap_or(parent));
        if let Some(instance) = &mut record.prefab_instance {
            stack.extend(instance.added_children.iter_mut());
        }
    }
}

/// Replaces the selection with `entities` (the last one primary).
fn select_exactly(world: &mut World, entities: &[Entity]) {
    let mut selection = world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .clone();
    selection.clear();
    for &entity in entities {
        selection.select(entity, crate::selection::SelectMode::Add);
    }
    world.insert_resource(selection);
}

/// What [`group_entities`] made.
#[derive(Debug, Clone)]
pub struct GroupOutcome {
    /// The new group entity.
    pub group: Entity,
    /// The entities moved under it (top-level members, input order).
    pub members: Vec<Entity>,
    /// The group's parent (the members' common parent, or `None` = root).
    pub parent: Option<Entity>,
}

/// Creates an empty entity named `name` at the centroid of `entities`' world positions, under
/// their common parent (root when they differ), and moves them under it keeping their world
/// poses: one undo entry. The group becomes the selection.
///
/// # Errors
/// When an entity is missing or internal, or none is given, or one sits on a `ChildOf` cycle.
pub fn group_entities(
    world: &mut World,
    entities: &[Entity],
    name: &str,
) -> Result<GroupOutcome, HierarchyError> {
    validate_entities(world, entities)?;
    refuse_parent_cycles(world, entities)?;
    let members = top_level(world, entities);
    if members.is_empty() {
        return Err(HierarchyError::NoEntities);
    }
    let parents: Vec<Option<Entity>> = members
        .iter()
        .map(|e| world.get::<ChildOf>(*e).map(ChildOf::parent))
        .collect();
    let parent = parents
        .first()
        .copied()
        .flatten()
        .filter(|first| parents.iter().all(|p| *p == Some(*first)));
    let centroid = members
        .iter()
        .map(|e| Vec3::from(world_affine(world, *e).translation))
        .fold(Vec3::ZERO, |sum, p| sum + p)
        / members.len() as f32;
    let local_translation = match parent {
        Some(parent) => {
            let affine = world_affine(world, parent);
            if affine.matrix3.determinant().abs() > 1e-12 {
                affine.inverse().transform_point3(centroid)
            } else {
                Vec3::ZERO
            }
        }
        None => centroid,
    };
    let group_guid = Guid::new_random();
    let mut spawn = SpawnEntities::new_entity(
        world,
        group_guid,
        name,
        Transform::from_translation(local_translation),
    )
    .map_err(|err| HierarchyError::Scene(err.to_string()))?;
    spawn.apply(world);
    let group = guid_entity(world, group_guid)
        .ok_or_else(|| HierarchyError::Scene("the group entity did not spawn".to_string()))?;
    let mut commands: Vec<Box<dyn EditorCommand>> = vec![Box::new(spawn)];
    if let Some(parent) = parent {
        let mut place = ReparentEntities {
            moves: vec![ReparentMove {
                entity: EntityKey::capture(world, group),
                old_parent: None,
                new_parent: Some(EntityKey::capture(world, parent)),
                old_local: None,
                new_local: None,
            }],
            ..ReparentEntities::default()
        };
        place.apply(world);
        commands.push(Box::new(place));
    }
    // The group is fresh and outside every member's subtree: this cannot fail validation.
    let plan = plan_reparent(world, &members, Some(group), true)?;
    let mut adopt = plan.command;
    adopt.apply(world);
    commands.push(Box::new(adopt));
    push_undo(
        world,
        Box::new(CommandBatch {
            label: format!("Group {} entities", members.len()),
            commands,
        }),
    );
    select_exactly(world, &[group]);
    Ok(GroupOutcome {
        group,
        members,
        parent,
    })
}

/// Which relatives [`scope_entities`] collects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectScope {
    /// Each entity and all its descendants.
    Subtree,
    /// The direct children of each entity.
    Children,
    /// The parent of each entity (roots contribute nothing).
    Parent,
}

impl SelectScope {
    /// Parses the protocol name (`subtree`, `children`, `parent`).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "subtree" => Some(Self::Subtree),
            "children" => Some(Self::Children),
            "parent" => Some(Self::Parent),
            _ => None,
        }
    }
}

/// The relatives of `entities` in `scope`, deduplicated in walk order, editor-internal entities
/// left out.
#[must_use]
pub fn scope_entities(world: &World, entities: &[Entity], scope: SelectScope) -> Vec<Entity> {
    let mut out: Vec<Entity> = Vec::new();
    let push = |entity: Entity, out: &mut Vec<Entity>| {
        if !out.contains(&entity) && !crate::visibility::is_hidden(world, entity) {
            out.push(entity);
        }
    };
    for &entity in entities {
        if world.get_entity(entity).is_err() {
            continue;
        }
        match scope {
            SelectScope::Subtree => {
                for member in crate::outliner::subtree_of(world, entity) {
                    push(member, &mut out);
                }
            }
            SelectScope::Children => {
                let children: Vec<Entity> = world
                    .get::<Children>(entity)
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                for child in children {
                    push(child, &mut out);
                }
            }
            SelectScope::Parent => {
                if let Some(parent) = world.get::<ChildOf>(entity).map(ChildOf::parent) {
                    push(parent, &mut out);
                }
            }
        }
    }
    out
}

/// The entities a drag of `dragged` carries: the whole selection when `dragged` is selected
/// (multi-select drag), otherwise `dragged` alone.
#[must_use]
pub fn drag_set(selection: &Selection, dragged: Entity) -> Vec<Entity> {
    if selection.contains(dragged) {
        selection.entities().to_vec()
    } else {
        vec![dragged]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{redo_world, undo_world};
    use regia_core::math::Quat;
    use regia_scene::prelude::Name;

    fn app() -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app
    }

    fn spawn(world: &mut World, name: &str, transform: Transform, parent: Option<Entity>) -> Entity {
        let entity = world
            .spawn((Name::new(name.to_string()), transform, EntityGuid::new_random()))
            .id();
        if let Some(parent) = parent {
            world.entity_mut(entity).insert(ChildOf(parent));
        }
        entity
    }

    fn by_name(world: &World, name: &str) -> Entity {
        world
            .iter_entities()
            .map(|e| e.id())
            .find(|e| world.get::<Name>(*e).is_some_and(|n| n.as_str() == name))
            .unwrap_or_else(|| panic!("no entity {name:?}"))
    }

    fn parent_of(world: &World, entity: Entity) -> Option<Entity> {
        world.get::<ChildOf>(entity).map(ChildOf::parent)
    }

    fn world_pos(world: &World, entity: Entity) -> Vec3 {
        Vec3::from(world_affine(world, entity).translation)
    }

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    /// Review sprint-w12-rev-early-k row 10: an entity in an Outliner folder that gets a parent
    /// loses the folder, and dropping it back at the root does not bring the old folder back;
    /// undo restores the folder with the parent.
    #[test]
    fn a_reparent_drops_the_outliner_folder_and_undo_restores_it() {
        use regia_scene::components::EditorFolder;
        let mut app = app();
        let world = app.world_mut();
        world.insert_resource(crate::command::UndoStack::new());
        let lamp = spawn(world, "Lamp", Transform::default(), None);
        let post = spawn(world, "Post", Transform::default(), None);
        let lights = EditorFolder {
            path: "Lights".to_string(),
        };
        world.entity_mut(lamp).insert(lights.clone());
        let push = |world: &mut World, parent: Option<Entity>| {
            let mut command = plan_reparent(world, &[lamp], parent, true).expect("plan").command;
            command.apply(world);
            world
                .resource_mut::<crate::command::UndoStack>()
                .push(Box::new(command));
        };
        push(world, Some(post));
        assert_eq!(parent_of(world, lamp), Some(post));
        assert!(
            world.get::<EditorFolder>(lamp).is_none(),
            "a child keeps no folder"
        );
        push(world, None);
        assert_eq!(parent_of(world, lamp), None);
        assert!(
            world.get::<EditorFolder>(lamp).is_none(),
            "back at the root, the old folder does not come back"
        );
        assert!(undo_world(world));
        assert!(undo_world(world));
        assert_eq!(parent_of(world, lamp), None);
        assert_eq!(
            world.get::<EditorFolder>(lamp),
            Some(&lights),
            "undo restores the folder"
        );
        assert!(redo_world(world));
        assert!(world.get::<EditorFolder>(lamp).is_none(), "redo drops it again");
        // The single-entity command (`editor.reparent`'s) does the same.
        assert!(undo_world(world));
        let mut single = crate::command::Reparent::capture(world, lamp, Some(post));
        single.apply(world);
        assert!(world.get::<EditorFolder>(lamp).is_none());
        single.revert(world);
        assert_eq!(world.get::<EditorFolder>(lamp), Some(&lights));
    }

    /// A (at x=10, rotated 90° about y, scale 2) with child A1; B at the root; C at the root.
    fn scene(world: &mut World) -> (Entity, Entity, Entity, Entity) {
        let a = spawn(
            world,
            "A",
            Transform {
                translation: Vec3::new(10.0, 0.0, 0.0),
                rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                scale: Vec3::splat(2.0),
            },
            None,
        );
        let a1 = spawn(
            world,
            "A1",
            Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
            Some(a),
        );
        let b = spawn(
            world,
            "B",
            Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
            None,
        );
        let c = spawn(
            world,
            "C",
            Transform::from_translation(Vec3::new(0.0, 0.0, 3.0)),
            None,
        );
        (a, a1, b, c)
    }

    #[test]
    fn top_level_drops_descendants_and_duplicates() {
        let mut app = app();
        let (a, a1, b, _) = scene(app.world_mut());
        let world = app.world();
        assert_eq!(top_level(world, &[a1, a, b, a]), vec![a, b]);
        assert_eq!(top_level(world, &[a1, b]), vec![a1, b]);
    }

    #[test]
    fn world_affine_follows_the_chain() {
        let mut app = app();
        let (_, a1, _, _) = scene(app.world_mut());
        // A1's local x=1 is scaled by 2 and rotated 90° about y: (0, 0, -2) from A at x=10.
        assert!(close(world_pos(app.world(), a1), Vec3::new(10.0, 0.0, -2.0)));
    }

    #[test]
    fn reparent_many_keeps_world_pose_and_undoes_as_one_entry() {
        let mut app = app();
        let (a, a1, b, c) = scene(app.world_mut());
        let world = app.world_mut();
        let before_b = world_pos(world, b);
        let before_c = world_pos(world, c);

        let plan = reparent_entities(world, &[b, c], Some(a), true).expect("reparent");
        assert_eq!(plan.moved, vec![b, c]);
        assert_eq!(parent_of(world, b), Some(a));
        assert_eq!(parent_of(world, c), Some(a));
        assert!(close(world_pos(world, b), before_b), "B kept its world pose");
        assert!(close(world_pos(world, c), before_c), "C kept its world pose");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "two moves, one undo entry"
        );

        assert!(undo_world(world));
        assert_eq!(parent_of(world, b), None);
        assert_eq!(parent_of(world, c), None);
        assert_eq!(world.get::<Transform>(b).map(|t| t.translation), Some(before_b));
        assert!(redo_world(world));
        assert_eq!(parent_of(world, b), Some(a));
        assert!(close(world_pos(world, c), before_c));
        // A1 was not touched.
        assert_eq!(parent_of(world, a1), Some(a));
    }

    #[test]
    fn plain_reparent_keeps_the_local_transform() {
        let mut app = app();
        let (a, _, b, _) = scene(app.world_mut());
        let world = app.world_mut();
        let local = *world.get::<Transform>(b).expect("transform");
        reparent_entities(world, &[b], Some(a), false).expect("reparent");
        assert_eq!(*world.get::<Transform>(b).expect("transform"), local);
    }

    #[test]
    fn reparent_refuses_cycles_without_side_effects() {
        let mut app = app();
        let (a, a1, b, _) = scene(app.world_mut());
        let world = app.world_mut();
        let err = reparent_entities(world, &[b, a], Some(a1), true).expect_err("cycle");
        assert_eq!(err.code(), "hierarchy_cycle");
        assert_eq!(parent_of(world, b), None, "nothing half-applied");
        let err = reparent_entities(world, &[a], Some(a), true).expect_err("self");
        assert_eq!(err.code(), "hierarchy_cycle");
        assert_eq!(
            world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
            0
        );
    }

    #[test]
    fn reparent_reports_unchanged_and_carried() {
        let mut app = app();
        let (a, a1, b, _) = scene(app.world_mut());
        let world = app.world_mut();
        // A1 already under A; B moves; A is not given.
        let plan = reparent_entities(world, &[a1, b], Some(a), true).expect("reparent");
        assert_eq!(plan.unchanged, vec![a1]);
        assert_eq!(plan.moved, vec![b]);
        // Moving A and A1 to the root: A1 travels with A.
        let plan = plan_reparent(world, &[a, a1], None, true).expect("plan");
        assert_eq!(plan.carried, vec![a1]);
        assert_eq!(plan.unchanged, vec![a], "A is already a root");
    }

    #[test]
    fn unparent_to_root_keeps_world_pose() {
        let mut app = app();
        let (_, a1, _, _) = scene(app.world_mut());
        let world = app.world_mut();
        let before = world_pos(world, a1);
        reparent_entities(world, &[a1], None, true).expect("unparent");
        assert_eq!(parent_of(world, a1), None);
        assert!(close(world_pos(world, a1), before));
        let t = world.get::<Transform>(a1).expect("transform");
        assert!(
            (t.scale - Vec3::splat(2.0)).length() < 1e-4,
            "inherited scale baked in"
        );
    }

    #[test]
    fn errors_have_stable_codes() {
        let mut app = app();
        let (a, _, b, _) = scene(app.world_mut());
        let world = app.world_mut();
        assert_eq!(
            plan_reparent(world, &[], Some(a), true)
                .expect_err("empty")
                .code(),
            "hierarchy_no_entities"
        );
        let gone = spawn(world, "Gone", Transform::default(), None);
        assert!(world.despawn(gone));
        assert_eq!(
            plan_reparent(world, &[b], Some(gone), true)
                .expect_err("parent")
                .code(),
            "hierarchy_parent_missing"
        );
        assert_eq!(
            plan_reparent(world, &[gone], Some(a), true)
                .expect_err("entity")
                .code(),
            "hierarchy_entity_missing"
        );
        let internal = world.spawn(crate::visibility::EditorHidden).id();
        assert_eq!(
            plan_reparent(world, &[internal], None, true)
                .expect_err("internal")
                .code(),
            "hierarchy_entity_internal"
        );
        assert_eq!(
            plan_reparent(world, &[b], Some(internal), true)
                .expect_err("internal parent")
                .code(),
            "hierarchy_parent_internal"
        );
        let guidless = world.spawn(Transform::default()).id();
        assert_eq!(
            duplicate_entities(world, &[guidless], Vec3::X)
                .expect_err("no guid")
                .code(),
            "hierarchy_no_guid"
        );
    }

    #[test]
    fn undo_survives_delete_and_respawn_by_guid() {
        let mut app = app();
        let (a, _, b, _) = scene(app.world_mut());
        let world = app.world_mut();
        reparent_entities(world, &[b], Some(a), true).expect("reparent");
        // Delete A (B goes with it), undo the delete: A and B come back under new ids.
        delete_entities(world, &[a]).expect("delete");
        assert!(world.get_entity(b).is_err());
        assert!(undo_world(world), "undo delete");
        let a2 = by_name(world, "A");
        let b2 = by_name(world, "B");
        assert_eq!(parent_of(world, b2), Some(a2), "subtree restored");
        // The reparent entry still finds B through its guid.
        assert!(undo_world(world), "undo reparent");
        assert_eq!(parent_of(world, b2), None);
        assert!(close(world_pos(world, b2), Vec3::new(0.0, 5.0, 0.0)));
    }

    #[test]
    fn delete_child_undo_restores_its_parent_link() {
        let mut app = app();
        let (a, a1, _, c) = scene(app.world_mut());
        let world = app.world_mut();
        let deleted = delete_entities(world, &[a1, c]).expect("delete");
        assert_eq!(deleted, vec![a1, c]);
        assert!(world.get_entity(a1).is_err() && world.get_entity(c).is_err());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        assert!(undo_world(world));
        let a1 = by_name(world, "A1");
        assert_eq!(
            parent_of(world, a1),
            Some(a),
            "A1 is back under A, not at the root"
        );
        assert_eq!(parent_of(world, by_name(world, "C")), None);
        assert!(redo_world(world));
        assert!(
            world
                .iter_entities()
                .all(|e| world.get::<Name>(e.id()).is_none_or(|n| n.as_str() != "A1"))
        );
    }

    #[test]
    fn duplicate_many_keeps_parents_offsets_and_selects_copies() {
        let mut app = app();
        let (a, a1, _, c) = scene(app.world_mut());
        let world = app.world_mut();
        let copies = duplicate_entities(world, &[a1, c], Vec3::new(0.0, 1.0, 0.0)).expect("duplicate");
        assert_eq!(copies.len(), 2);
        assert_eq!(parent_of(world, copies[0]), Some(a), "A1's copy stays under A");
        assert_eq!(parent_of(world, copies[1]), None);
        assert_eq!(
            world.get::<Transform>(copies[0]).map(|t| t.translation),
            Some(Vec3::new(1.0, 1.0, 0.0)),
            "offset in the parent's space"
        );
        assert_ne!(
            world.get::<EntityGuid>(copies[0]).map(|g| g.0),
            world.get::<EntityGuid>(a1).map(|g| g.0),
            "fresh guid"
        );
        assert_eq!(world.resource::<Selection>().entities(), copies.as_slice());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        assert!(undo_world(world));
        assert!(copies.iter().all(|e| world.get_entity(*e).is_err()));
        assert!(redo_world(world));
        // The copy is named "A1 2" (ADR-7757's duplicate naming rule).
        let a1_copies = world
            .iter_entities()
            .filter(|e| world.get::<Name>(e.id()).is_some_and(|n| n.as_str() == "A1 2"))
            .count();
        assert_eq!(a1_copies, 1, "redo re-creates the copy");
        let restored: Vec<Entity> = world
            .iter_entities()
            .map(|e| e.id())
            .filter(|e| *e != a1 && world.get::<Name>(*e).is_some_and(|n| n.as_str() == "A1 2"))
            .collect();
        assert_eq!(parent_of(world, restored[0]), Some(a), "redo re-links the copy");
    }

    #[test]
    fn group_wraps_members_at_their_centroid() {
        let mut app = app();
        let (_, _, b, c) = scene(app.world_mut());
        let world = app.world_mut();
        let before = (world_pos(world, b), world_pos(world, c));
        let outcome = group_entities(world, &[b, c], "Group").expect("group");
        assert_eq!(outcome.parent, None);
        assert!(close(
            world_pos(world, outcome.group),
            (before.0 + before.1) * 0.5
        ));
        assert_eq!(parent_of(world, b), Some(outcome.group));
        assert!(close(world_pos(world, b), before.0) && close(world_pos(world, c), before.1));
        assert_eq!(world.resource::<Selection>().entities(), &[outcome.group]);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);

        assert!(undo_world(world));
        assert!(world.get_entity(outcome.group).is_err(), "group removed");
        assert_eq!(parent_of(world, b), None);
        assert!(redo_world(world));
        let group = by_name(world, "Group");
        assert_eq!(
            parent_of(world, b),
            Some(group),
            "redo finds the new group by guid"
        );
        assert!(close(world_pos(world, c), before.1));
    }

    #[test]
    fn group_under_common_parent() {
        let mut app = app();
        let (a, a1, _, _) = scene(app.world_mut());
        let world = app.world_mut();
        let a2 = spawn(
            world,
            "A2",
            Transform::from_translation(Vec3::new(3.0, 0.0, 0.0)),
            Some(a),
        );
        let before = (world_pos(world, a1), world_pos(world, a2));
        let outcome = group_entities(world, &[a1, a2], "Pair").expect("group");
        assert_eq!(outcome.parent, Some(a));
        assert_eq!(parent_of(world, outcome.group), Some(a));
        assert!(close(world_pos(world, a1), before.0) && close(world_pos(world, a2), before.1));
        assert!(close(
            world_pos(world, outcome.group),
            (before.0 + before.1) * 0.5
        ));
    }

    /// Two entities that are each other's `ChildOf` parent (bevy_ecs accepts the direct inserts).
    fn parent_cycle(world: &mut World) -> (Entity, Entity) {
        let p = spawn(world, "P", Transform::from_translation(Vec3::X), None);
        let q = spawn(world, "Q", Transform::from_translation(Vec3::Y), Some(p));
        world.entity_mut(p).insert(ChildOf(q));
        assert_eq!(parent_of(world, p), Some(q), "the cycle is in place");
        assert_eq!(parent_of(world, q), Some(p), "the cycle is in place");
        (p, q)
    }

    #[test]
    fn group_of_a_parent_cycle_spawns_nothing() {
        let mut app = app();
        let world = app.world_mut();
        let (p, q) = parent_cycle(world);
        world.init_resource::<UndoStack>();
        let count = world.iter_entities().count();
        let err = group_entities(world, &[p, q], "Loop").expect_err("a cycle cannot be grouped");
        assert_eq!(err.code(), "hierarchy_cycle", "{err}");
        assert_eq!(
            world.iter_entities().count(),
            count,
            "no group entity was left behind"
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);

        let err = delete_entities(world, &[p, q]).expect_err("a cycle is not silently kept");
        assert_eq!(err.code(), "hierarchy_cycle", "{err}");
        assert!(world.get_entity(p).is_ok() && world.get_entity(q).is_ok());
        let err = duplicate_entities(world, &[q, p], Vec3::X).expect_err("nor duplicated as nothing");
        assert_eq!(err.code(), "hierarchy_cycle", "{err}");
        assert_eq!(world.iter_entities().count(), count);

        // The way out the error names: move one of them to the root, then the pair deletes.
        reparent_entities(world, &[p], None, true).expect("unparent breaks the cycle");
        assert_eq!(parent_of(world, p), None);
        let deleted = delete_entities(world, &[p, q]).expect("delete");
        assert_eq!(deleted, vec![p], "q goes with its parent");
        assert!(world.get_entity(p).is_err() && world.get_entity(q).is_err());
    }

    #[test]
    fn group_below_a_parent_cycle_finishes() {
        let mut app = app();
        let world = app.world_mut();
        let (p, _) = parent_cycle(world);
        let child = spawn(world, "Child", Transform::default(), Some(p));
        // The child is not on the cycle; its group goes under P, and the cycle check of the
        // reparent walks P's looping chain.
        let outcome = group_entities(world, &[child], "Below").expect("group");
        assert_eq!(outcome.parent, Some(p));
        assert_eq!(parent_of(world, child), Some(outcome.group));
    }

    #[test]
    fn scopes_and_drag_sets() {
        let mut app = app();
        let (a, a1, b, _) = scene(app.world_mut());
        let world = app.world();
        assert_eq!(scope_entities(world, &[a], SelectScope::Subtree), vec![a, a1]);
        assert_eq!(scope_entities(world, &[a, b], SelectScope::Children), vec![a1]);
        assert_eq!(scope_entities(world, &[a1, b], SelectScope::Parent), vec![a]);
        assert_eq!(SelectScope::from_name("children"), Some(SelectScope::Children));
        assert_eq!(SelectScope::from_name("siblings"), None);

        let mut selection = Selection::default();
        selection.select(a1, crate::selection::SelectMode::Replace);
        selection.select(b, crate::selection::SelectMode::Add);
        assert_eq!(
            drag_set(&selection, b),
            vec![a1, b],
            "dragging a selected row drags all"
        );
        assert_eq!(
            drag_set(&selection, a),
            vec![a],
            "dragging an unselected row drags it alone"
        );
    }

    /// ADR-7757: duplicates take auto-incremented names ("Lamp", "Lamp 2", "Lamp 3"); several
    /// copies at once take consecutive numbers; children keep their names.
    #[test]
    fn duplicates_take_the_next_numbered_name() {
        let mut app = app();
        let world = app.world_mut();
        let lamp = world
            .spawn((Name::new("Lamp"), Transform::default(), EntityGuid::new_random()))
            .id();
        let bulb = world
            .spawn((
                Name::new("Bulb"),
                Transform::default(),
                EntityGuid::new_random(),
                ChildOf(lamp),
            ))
            .id();
        let _ = bulb;
        let name = |world: &World, e: Entity| world.get::<Name>(e).map(|n| n.as_str().to_string());
        let first = duplicate_entities(world, &[lamp], Vec3::X).expect("copy")[0];
        assert_eq!(name(world, first).as_deref(), Some("Lamp 2"));
        let second = duplicate_entities(world, &[lamp], Vec3::X).expect("copy")[0];
        assert_eq!(name(world, second).as_deref(), Some("Lamp 3"));
        let third = duplicate_entities(world, &[first], Vec3::X).expect("copy")[0];
        assert_eq!(
            name(world, third).as_deref(),
            Some("Lamp 4"),
            "a numbered name continues its stem"
        );
        let both = duplicate_entities(world, &[lamp, second], Vec3::X).expect("copies");
        let names: Vec<_> = both.iter().map(|e| name(world, *e)).collect();
        assert_eq!(names, [Some("Lamp 5".to_string()), Some("Lamp 6".to_string())]);
        let bulbs = world
            .iter_entities()
            .filter(|e| world.get::<Name>(e.id()).is_some_and(|n| n.as_str() == "Bulb"))
            .count();
        assert_eq!(bulbs, 6, "children keep their names");
        assert_eq!(CopyNames::split("Lamp 07"), ("Lamp", 7));
        assert_eq!(CopyNames::split("2"), ("2", 1), "a bare number is a stem");
        assert_eq!(CopyNames::split("Room 1A"), ("Room 1A", 1));
        assert_eq!(CopyNames::split(" 5"), (" 5", 1), "an empty stem is no stem");
        let mut names = CopyNames::default();
        assert_eq!(
            names.next_name("Door 99999999999999999999"),
            "Door 99999999999999999999 2"
        );
    }
}
