// SPDX-License-Identifier: MPL-2.0
//! Editor commands and the undo stack (`docs/specs/19-editor.md` § 1).
//!
//! Every edit the editor (human or agent) makes goes through here: a command captures enough
//! state to [`apply`](EditorCommand::apply) and [`revert`](EditorCommand::revert) itself against
//! the world, and the [`UndoStack`] keeps the ordered history with coalescing (continuous edits —
//! gizmo drags, slider scrubs — merge into one entry) and a cap of 500 entries.
//!
//! Reflection makes the generic commands possible: [`SetProperty`] clones the old and new field
//! values as reflected data, so no per-type edit code exists.

use std::any::Any;
use std::collections::{BTreeMap, HashMap, HashSet};

use regia_core::id::Guid;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, GetPath, PartialReflect, ReflectComponent};
use regia_ecs::serialize::ReflectCodec;
use regia_scene::level::{LevelMember, Persistent};
use regia_scene::prefab::OverrideEntry;
use regia_scene::prelude::{
    EntityGuid, PrefabInstance, SceneEntity, SceneFile, SpawnOptions, Transform, snapshot_scene, spawn_scene,
};
use regia_scene::spawn::{SceneSpawnError, SnapshotFilter};

/// Resolves entity bits to an entity (`None` when the bits are not a valid id in this
/// bevy_ecs version's generation scheme).
pub(crate) fn entity_from_bits(bits: u64) -> Option<Entity> {
    Entity::try_from_bits(bits)
}

/// Where the entities an undo entry recorded live now.
///
/// Undoing a delete (and redoing a create) spawns the entities again from a snapshot: same
/// guids, new entity ids. Every entry recorded before that still holds the old ids, so each
/// command resolves its ids through this map ([`resolve_entity`]) instead of using them raw.
/// The commands that despawn and re-spawn fill it; it also remembers, per despawned guid, the
/// level membership, the `Persistent` marker, its parent and its place among its siblings, so a
/// restored entity is saved with its level and comes back where it was in the outliner.
#[derive(Resource, Default, Debug)]
pub struct EntityRemap {
    /// Old entity bits → current entity bits (path-compressed: never a chain).
    forward: HashMap<u64, u64>,
    /// Current entity bits → the old bits `forward` maps to them: the reverse index that lets
    /// [`Self::record`] move an entity's old ids without scanning every entry.
    backward: HashMap<u64, Vec<u64>>,
    /// What a despawn by an undo entry removed, by guid, until the entity is spawned again.
    buried: HashMap<Guid, Buried>,
}

/// One entity an undo entry despawned.
#[derive(Clone, Copy, Debug)]
struct Buried {
    /// Its entity bits when it was despawned.
    bits: u64,
    /// Its `LevelMember` level, if any.
    level: Option<u128>,
    /// Whether it had `Persistent`.
    persistent: bool,
    /// Its parent's entity bits (`None` for a root). The snapshot of a deleted subtree re-roots
    /// a root whose parent stays alive; this puts it back under that parent.
    parent: Option<u64>,
    /// Its index among its parent's children (`None` for a root).
    sibling: Option<usize>,
}

impl EntityRemap {
    /// The current bits of an entity an entry recorded as `bits`: `bits` itself unless an undo
    /// or redo spawned that entity again, then the bits of the new one.
    #[must_use]
    pub fn resolve(&self, bits: u64) -> u64 {
        self.forward.get(&bits).copied().unwrap_or(bits)
    }

    /// Number of remapped ids (diagnostics, tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    /// Whether no id was remapped yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Records that the entity once `old` is `new` now. Older ids that pointed at `old` move to
    /// `new` as well, so a lookup is always one step. The cost is the number of old ids of that
    /// one entity, not the size of the map (rc2 review, editor-undo finding 3: the scan of every
    /// entry made each live reload slower than the last). A stale mapping from `new` (an id
    /// that is live as itself again) is dropped with its reverse entry.
    fn record(&mut self, old: u64, new: u64) {
        if old == new {
            return;
        }
        if let Some(target) = self.forward.remove(&new) {
            self.unlink(target, new);
        }
        if let Some(target) = self.forward.remove(&old) {
            self.unlink(target, old);
        }
        let mut moved = self.backward.remove(&old).unwrap_or_default();
        moved.retain(|bits| *bits != new);
        for bits in &moved {
            self.forward.insert(*bits, new);
        }
        moved.push(old);
        self.forward.insert(old, new);
        self.backward.entry(new).or_default().extend(moved);
    }

    /// Drops `old` from the reverse entry of `target`.
    fn unlink(&mut self, target: u64, old: u64) {
        if let Some(olds) = self.backward.get_mut(&target) {
            olds.retain(|bits| *bits != old);
            if olds.is_empty() {
                self.backward.remove(&target);
            }
        }
    }
}

/// The live entity an undo entry recorded as `bits`, following [`EntityRemap`]; `None` when it
/// does not exist (any more). Host commands implementing [`EditorCommand`] should resolve their
/// stored ids through this too.
#[must_use]
pub fn resolve_entity(world: &World, bits: u64) -> Option<Entity> {
    let bits = world
        .get_resource::<EntityRemap>()
        .map_or(bits, |remap| remap.resolve(bits));
    entity_from_bits(bits).filter(|entity| world.get_entity(*entity).is_ok())
}

/// Despawns `roots` (with their subtrees) for an undo entry, remembering every despawned guid's
/// id, level membership and sibling place in [`EntityRemap`] so [`exhume`] can restore them.
pub(crate) fn bury_and_despawn(world: &mut World, roots: &[Entity], what: &str) {
    bury_records(world, roots);
    for root in roots {
        // A root may sit inside another root's subtree and be gone already.
        if world.get_entity(*root).is_ok()
            && let Err(err) = world.get_entity_mut(*root).map(|e| e.despawn())
        {
            tracing::warn!(target: "regia_editor", "{what}: {err}");
        }
    }
}

/// The bookkeeping half of [`bury_and_despawn`]: remembers every guid of `roots`' subtrees (id,
/// level membership, `Persistent`, parent and sibling place) in [`EntityRemap`] without despawning
/// anything. For an undo entry whose despawn happens elsewhere (`regia_scene::placed::break_placed`
/// despawns a placed-level instance itself), so [`spawn_and_exhume`] can still put the entities
/// back where they were and map their old ids. Returns the guids it noted.
pub(crate) fn bury_records(world: &mut World, roots: &[Entity]) -> Vec<Guid> {
    let mut found: Vec<(Guid, Buried)> = Vec::new();
    let mut stack: Vec<Entity> = roots.to_vec();
    while let Some(entity) = stack.pop() {
        if world.get_entity(entity).is_err() {
            continue;
        }
        if let Some(guid) = world.get::<EntityGuid>(entity) {
            let parent = world.get::<ChildOf>(entity).map(ChildOf::parent);
            let sibling = parent
                .and_then(|parent| world.get::<Children>(parent))
                .and_then(|children| {
                    let siblings: &[Entity] = children;
                    siblings.iter().position(|c| *c == entity)
                });
            found.push((
                guid.0,
                Buried {
                    bits: entity.to_bits(),
                    level: world.get::<LevelMember>(entity).map(|member| member.0),
                    persistent: world.get::<Persistent>(entity).is_some(),
                    parent: parent.map(Entity::to_bits),
                    sibling,
                },
            ));
        }
        if let Some(children) = world.get::<Children>(entity) {
            let children: &[Entity] = children;
            stack.extend_from_slice(children);
        }
    }
    let guids = found.iter().map(|(guid, _)| *guid).collect();
    let mut remap = world.get_resource_or_insert_with::<EntityRemap>(EntityRemap::default);
    for (guid, buried) in found {
        remap.buried.insert(guid, buried);
    }
    guids
}

/// Forgets the burial records of those `guids` whose entity is still alive: what
/// [`bury_records`] noted that the despawn kept after all (a placed level's former members,
/// and what sits under them, stay when its instance goes).
pub(crate) fn unbury_live(world: &mut World, guids: &[Guid]) {
    let alive: HashSet<Guid> = {
        let mut query = regia_ecs::ecs::query::QueryState::<&EntityGuid>::new(world);
        query.iter(world).map(|guid| guid.0).collect()
    };
    if let Some(mut remap) = world.get_resource_mut::<EntityRemap>() {
        for guid in guids.iter().filter(|guid| alive.contains(guid)) {
            remap.buried.remove(guid);
        }
    }
}

/// Records in [`EntityRemap`] that the entity an undo entry knew as `old` is `new` now.
pub(crate) fn record_respawn(world: &mut World, old: Entity, new: Entity) {
    world
        .get_resource_or_insert_with::<EntityRemap>(EntityRemap::default)
        .record(old.to_bits(), new.to_bits());
}

/// Spawns `file` for an undo entry and maps the entities it brings back to the ids they had
/// when [`bury_and_despawn`] removed them; restores their level membership, `Persistent` marker
/// and sibling order. Returns whether the spawn succeeded.
///
/// Component types the registry does not know are kept as raw RON (the editor's policy, the
/// same as a level open), so a component the snapshot carried that way comes back for the next
/// save instead of being dropped.
pub(crate) fn spawn_and_exhume(
    world: &mut World,
    file: &SceneFile,
    level: Option<regia_scene::prelude::LevelId>,
    what: &str,
) -> bool {
    match try_spawn_and_exhume(world, file, level) {
        Ok(()) => true,
        Err(err) => {
            tracing::error!(target: "regia_editor", "{what}: {err}");
            false
        }
    }
}

/// [`spawn_and_exhume`] that returns the spawn error instead of logging it. A failed spawn
/// leaves nothing behind (`spawn_scene` despawns its rows) and keeps the burial records, so the
/// same snapshot can be spawned again later.
pub(crate) fn try_spawn_and_exhume(
    world: &mut World,
    file: &SceneFile,
    level: Option<regia_scene::prelude::LevelId>,
) -> Result<(), regia_scene::spawn::SceneSpawnError> {
    let opts = SpawnOptions {
        level,
        persistent: false,
        unknown_types: regia_scene::spawn::UnknownTypePolicyOption::Preserve,
    };
    spawn_scene(world, file, &opts)?;
    exhume(world, file);
    Ok(())
}

/// The second half of [`spawn_and_exhume`], after the spawn.
fn exhume(world: &mut World, file: &SceneFile) {
    let Some(mut remap) = world.remove_resource::<EntityRemap>() else {
        return;
    };
    // The file's records, and everything the spawn put under them: the entities nested in an
    // instance's `added_children` and the instance's prefab-internal entities are buried too,
    // and need their level membership back (a level save keeps only level members).
    let roots: HashSet<Guid> = file.entities.iter().map(|record| record.guid).collect();
    let mut live: Vec<(Entity, Buried)> = Vec::new();
    if !roots.is_empty() && !remap.buried.is_empty() {
        let mut stack: Vec<Entity> = Vec::new();
        let mut query = regia_ecs::ecs::query::QueryState::<(Entity, &EntityGuid)>::new(world);
        for (entity, guid) in query.iter(world) {
            if roots.contains(&guid.0) {
                stack.push(entity);
            }
        }
        let mut seen: HashSet<Entity> = HashSet::new();
        while let Some(entity) = stack.pop() {
            if !seen.insert(entity) {
                continue;
            }
            if let Some(guid) = world.get::<EntityGuid>(entity)
                && let Some(buried) = remap.buried.remove(&guid.0)
            {
                live.push((entity, buried));
            }
            if let Some(children) = world.get::<Children>(entity) {
                let children: &[Entity] = children;
                stack.extend_from_slice(children);
            }
        }
    }
    let mut places: Vec<(usize, Entity, Entity)> = Vec::new();
    for (entity, buried) in live {
        remap.record(buried.bits, entity.to_bits());
        // The parent the spawn gave it (inside the restored subtree), else the live parent it
        // had outside the subtree.
        let parent = world.get::<ChildOf>(entity).map(ChildOf::parent).or_else(|| {
            buried
                .parent
                .and_then(|bits| entity_from_bits(remap.resolve(bits)))
                .filter(|parent| world.get_entity(*parent).is_ok())
        });
        let mut entity_mut = world.entity_mut(entity);
        match buried.level {
            Some(level) => {
                entity_mut.insert(LevelMember(level));
            }
            None => {
                entity_mut.remove::<LevelMember>();
            }
        }
        if buried.persistent {
            entity_mut.insert(Persistent);
        } else {
            entity_mut.remove::<Persistent>();
        }
        // A deferred prefab carries its spawn context separately from the root's components.
        // Restore that context too, so children resolved after this undo keep their document.
        if let Some(mut pending) = entity_mut.get_mut::<regia_scene::prefab::PendingPrefab>() {
            pending.level = buried.level.unwrap_or(0);
            pending.persistent = buried.persistent;
        }
        if let (Some(parent), Some(index)) = (parent, buried.sibling) {
            places.push((index, parent, entity));
        }
    }
    world.insert_resource(remap);
    // Ascending index per parent rebuilds the original order (a restored parent's children
    // are all restored; a live parent's other children are as they were before the despawn).
    places.sort_by_key(|(index, _, _)| *index);
    for (index, parent, child) in places {
        if world.get_entity(parent).is_ok() {
            world.entity_mut(parent).insert_child(index, child);
        }
    }
}

/// Maximum entries kept in the undo history (older entries fall off, `specs/19` § 1).
pub const UNDO_CAP: usize = 500;

/// One reversible edit. Object-safe; stored boxed on the [`UndoStack`].
pub trait EditorCommand: Any + Send + Sync {
    /// Applies the edit. Implementations may record what they did (spawned entity guids) so
    /// [`revert`](EditorCommand::revert) can undo it.
    fn apply(&mut self, world: &mut World);
    /// Reverts the last [`apply`](EditorCommand::apply).
    fn revert(&mut self, world: &mut World);
    /// Coalesces `next` into this entry: returns `true` when `next` was absorbed (continuous
    /// edits like slider scrubs and gizmo drags), `false` when it must become its own entry.
    fn merge(&mut self, next: &dyn EditorCommand) -> bool;
    /// Whether applying or reverting this command changes the level document's state (drives
    /// dirty tracking, close prompts and autosave). File-only commands (for example an
    /// input-map rewrite that is already on disk) return `false`: they stay undoable without
    /// marking a saved level dirty.
    fn changes_level_state(&self) -> bool {
        true
    }
    /// Optional change details for the history panel and agent. Empty means the command has
    /// not supplied details; it does not mean the command changed nothing.
    fn touched(&self) -> Vec<CommandTouch> {
        Vec::new()
    }
    /// What the entry does, for the UI ("Set Transform.translation").
    fn description(&self) -> String;
    /// Entity ids and asset paths touched by a host-specific command, for the Activity browser.
    /// Standard scene commands are recognized by the editor; custom commands may provide metadata here.
    fn activity_targets(&self) -> (Vec<u64>, Vec<String>) {
        (Vec::new(), Vec::new())
    }
}

/// One document, entity or asset property affected by an undo command.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CommandTouch {
    /// Document path, or `level` for the edit world's current level.
    pub document: String,
    /// Entity id at edit time; resolve through [`resolve_entity`] after a respawn.
    pub entity: Option<u64>,
    /// Stable entity guid when the command stores scene records instead of entity ids.
    pub guid: Option<String>,
    /// Asset path, for document edits.
    pub asset: Option<String>,
    /// Component and field path; `None` denotes a structural edit.
    pub property: Option<String>,
}

impl CommandTouch {
    /// Details for an entity edit in the current level.
    #[must_use]
    pub fn entity(entity: u64, property: Option<String>) -> Self {
        Self {
            document: "level".into(),
            entity: Some(entity),
            guid: None,
            asset: None,
            property,
        }
    }

    /// Details for an asset document edit.
    #[must_use]
    pub fn asset(path: impl Into<String>, property: Option<String>) -> Self {
        let path = path.into();
        Self {
            document: path.clone(),
            entity: None,
            guid: None,
            asset: Some(path),
            property,
        }
    }
}

/// One entry in chronological history order. `index` is its after-entry boundary.
#[derive(Clone, Debug, serde::Serialize)]
pub struct HistoryEntry {
    /// Boundary after this entry, starting at one; zero means before all retained entries.
    pub index: usize,
    /// Human-readable action label.
    pub description: String,
    /// Whether this entry is currently applied.
    pub applied: bool,
    /// Documents named by the command's details, without duplicates.
    pub documents: Vec<String>,
    /// Optional entity, asset and property details.
    pub touched: Vec<CommandTouch>,
}

/// A refused history or transaction operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    /// Begin was called while a transaction is open.
    #[error("transaction_nested")]
    Nested,
    /// Commit or abort was called with no open transaction.
    #[error("transaction_missing")]
    Missing,
    /// Navigation cannot run while edits are grouped.
    #[error("transaction_active")]
    Active,
    /// The requested boundary is outside retained history.
    #[error("history_index")]
    Index,
}

struct Transaction {
    group: CommandGroup,
    state: u64,
    /// An agent's transaction (`editor.transaction_begin`): only pushes made while an agent request
    /// runs ([`UndoStack::set_agent_request`]) join it; any other push (a gizmo drag, a panel
    /// edit) is its own entry (review sprint-w12-rev-g13 #3).
    agent: bool,
}

/// Errors from building or applying editor commands.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// The entity does not exist.
    #[error("entity {0} does not exist")]
    NoSuchEntity(u64),
    /// The component type is not registered for reflection.
    #[error("no reflected component {0}")]
    NoSuchComponent(String),
    /// The entity lacks the component.
    #[error("entity {0} has no {1}")]
    MissingComponent(u64, String),
    /// The reflected path does not exist.
    #[error("no field {1} in {0}")]
    NoSuchPath(String, String),
    /// A reflected value could not be cloned or applied.
    #[error("reflection error on {0}: {1}")]
    Reflect(String, String),
    /// A scene snapshot/spawn round trip failed.
    #[error("scene: {0}")]
    Scene(#[from] SceneSpawnError),
}

/// The reflected-component data of a type, resolved by short or full type name.
fn component_data(
    registry: &regia_ecs::reflect::TypeRegistry,
    name: &str,
) -> Result<ReflectComponent, CommandError> {
    registry
        .get_with_short_type_path(name)
        .or_else(|| regia_core::brand::lookup_type_path(name, |p| registry.get_with_type_path(p)))
        .and_then(|r| r.data::<ReflectComponent>().cloned())
        .ok_or_else(|| CommandError::NoSuchComponent(name.to_string()))
}

/// Reads a reflected field (or whole component) of one entity as a cloned value.
fn read_field(
    world: &World,
    entity: Entity,
    component: &str,
    path: &str,
) -> Result<Box<dyn PartialReflect>, CommandError> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let data = component_data(&registry, component)?;
    let Ok(entity_ref) = world.get_entity(entity) else {
        return Err(CommandError::NoSuchEntity(entity.to_bits()));
    };
    let Some(value) = data.reflect(entity_ref) else {
        return Err(CommandError::MissingComponent(
            entity.to_bits(),
            component.to_string(),
        ));
    };
    let target: &str = if path.is_empty() { component } else { path };
    let field = value
        .reflect_path(path)
        .map_err(|_| CommandError::NoSuchPath(component.to_string(), path.to_string()))?;
    clone_partial(field).map_err(|e| CommandError::Reflect(target.to_string(), e.to_string()))
}

/// A concrete clone where the type supports it, otherwise a dynamic one: bevy_reflect 0.20 has
/// no `reflect_clone` for fixed-size arrays or dynamic values, so `[f32; 3]`, any struct holding
/// one and the inspector's edited copies come back dynamic. `try_apply` accepts either.
pub(crate) fn clone_partial(
    value: &dyn PartialReflect,
) -> Result<Box<dyn PartialReflect>, regia_ecs::reflect::ReflectCloneError> {
    match value.reflect_clone() {
        Ok(concrete) => Ok(concrete.into_partial_reflect()),
        Err(clone_err) => value.to_dynamic().map_err(|_| clone_err),
    }
}

/// Reads a reflected field (`""` = the whole component) of one entity as a cloned value — the
/// value a [`SetProperty`] on the same path would record as `old`.
///
/// # Errors
/// When the entity, component or path does not resolve.
pub fn read_property(
    world: &World,
    entity: Entity,
    component: &str,
    path: &str,
) -> Result<Box<dyn PartialReflect>, CommandError> {
    read_field(world, entity, component, path)
}

/// Applies a reflected value to a field (or whole component) of one entity.
fn write_field(
    world: &mut World,
    entity: Entity,
    component: &str,
    path: &str,
    value: &dyn PartialReflect,
) -> Result<(), CommandError> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let data = component_data(&registry, component)?;
    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        return Err(CommandError::NoSuchEntity(entity.to_bits()));
    };
    let Some(mut target) = data.reflect_mut(&mut entity_mut) else {
        return Err(CommandError::MissingComponent(
            entity.to_bits(),
            component.to_string(),
        ));
    };
    let field: &mut dyn PartialReflect = if path.is_empty() {
        target.as_partial_reflect_mut()
    } else {
        target
            .reflect_path_mut(path)
            .map_err(|_| CommandError::NoSuchPath(component.to_string(), path.to_string()))?
    };
    field
        .try_apply(value)
        .map_err(|e| CommandError::Reflect(format!("{component}.{path}"), e.to_string()))?;
    prune_to(field, value);
    Ok(())
}

/// Makes `target` structurally equal to `value` after a `try_apply`, which only ever grows
/// collections: drops list elements past `value`'s length, map keys and set elements `value`
/// lacks, recursively through structs, tuples, arrays and same-variant enums. Without it, undoing
/// an "add element" (or applying a shorter list) would leave the extra elements in place.
fn prune_to(target: &mut dyn PartialReflect, value: &dyn PartialReflect) {
    use regia_ecs::reflect::{ReflectMut, ReflectRef};
    match (target.reflect_mut(), value.reflect_ref()) {
        (ReflectMut::List(list), ReflectRef::List(other)) => {
            while list.len() > other.len() {
                list.pop();
            }
            for i in 0..list.len() {
                if let (Some(t), Some(v)) = (list.get_mut(i), other.get(i)) {
                    prune_to(t, v);
                }
            }
        }
        (ReflectMut::Array(array), ReflectRef::Array(other)) => {
            for i in 0..array.len() {
                if let (Some(t), Some(v)) = (array.get_mut(i), other.get(i)) {
                    prune_to(t, v);
                }
            }
        }
        (ReflectMut::Map(map), ReflectRef::Map(other)) => {
            map.retain(&mut |key, entry| match other.get(key) {
                Some(v) => {
                    prune_to(entry, v);
                    true
                }
                None => false,
            });
        }
        (ReflectMut::Set(set), ReflectRef::Set(other)) => {
            set.retain(&mut |element| other.contains(element));
        }
        (ReflectMut::Struct(fields), ReflectRef::Struct(other)) => {
            for i in 0..fields.field_len() {
                let name = fields.name_at(i).map(str::to_string);
                if let Some(name) = name
                    && let (Some(t), Some(v)) = (fields.field_at_mut(i), other.field(&name))
                {
                    prune_to(t, v);
                }
            }
        }
        (ReflectMut::TupleStruct(fields), ReflectRef::TupleStruct(other)) => {
            for i in 0..fields.field_len() {
                if let (Some(t), Some(v)) = (fields.field_mut(i), other.field(i)) {
                    prune_to(t, v);
                }
            }
        }
        (ReflectMut::Tuple(fields), ReflectRef::Tuple(other)) => {
            for i in 0..fields.field_len() {
                if let (Some(t), Some(v)) = (fields.field_mut(i), other.field(i)) {
                    prune_to(t, v);
                }
            }
        }
        (ReflectMut::Enum(variant), ReflectRef::Enum(other))
            if variant.variant_name() == other.variant_name() =>
        {
            for i in 0..variant.field_len() {
                if let (Some(t), Some(v)) = (variant.field_at_mut(i), other.field_at(i)) {
                    prune_to(t, v);
                }
            }
        }
        _ => {}
    }
}

/// The concrete type of `other` when it is `T` (for [`EditorCommand::merge`] downcasts).
pub(crate) fn downcast_to<T: 'static>(other: &dyn EditorCommand) -> Option<&T> {
    (other as &dyn Any).downcast_ref::<T>()
}

/// Sets a reflected field of a component: `SetProperty { entity, component, path, old, new }`
/// (`specs/19` § 1).
pub struct SetProperty {
    /// Target entity (bits; entities are not `Copy` across command storage).
    pub entity: u64,
    /// Component type name.
    pub component: String,
    /// Reflected field path ("" = the whole component).
    pub path: String,
    /// Value before the edit.
    pub old: Box<dyn PartialReflect>,
    /// Value after the edit.
    pub new: Box<dyn PartialReflect>,
    /// Whether a following edit of the same field merges into this entry (slider scrubs, typing).
    /// Structural inspector edits (list add/remove/move, variant switches) set it to `false` so
    /// each is its own undo step.
    pub coalesce: bool,
}

impl SetProperty {
    /// Captures the current value and the new one.
    ///
    /// # Errors
    /// When the entity, component or path does not resolve ([`CommandError`]).
    pub fn capture(
        world: &World,
        entity: Entity,
        component: &str,
        path: &str,
        new: Box<dyn PartialReflect>,
    ) -> Result<Self, CommandError> {
        let old = read_field(world, entity, component, path)?;
        Ok(Self {
            entity: entity.to_bits(),
            component: component.to_string(),
            path: path.to_string(),
            old,
            new,
            coalesce: true,
        })
    }

    fn write(&self, world: &mut World, value: &dyn PartialReflect) {
        let Some(entity) = resolve_entity(world, self.entity) else {
            tracing::warn!(target: "regia_editor", "SetProperty: entity {} is gone", self.entity);
            return;
        };
        if let Err(err) = write_field(world, entity, &self.component, &self.path, value) {
            tracing::warn!(target: "regia_editor", "SetProperty: {err}");
        }
    }
}

impl EditorCommand for SetProperty {
    fn touched(&self) -> Vec<CommandTouch> {
        let property = if self.path.is_empty() {
            self.component.clone()
        } else {
            format!("{}.{}", self.component, self.path)
        };
        vec![CommandTouch::entity(self.entity, Some(property))]
    }

    fn apply(&mut self, world: &mut World) {
        self.write(world, self.new.as_ref());
    }

    fn revert(&mut self, world: &mut World) {
        self.write(world, self.old.as_ref());
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = downcast_to::<SetProperty>(next) else {
            return false;
        };
        if self.coalesce
            && next.coalesce
            && next.entity == self.entity
            && next.component == self.component
            && next.path == self.path
            && let Ok(new) = clone_partial(next.new.as_ref())
        {
            self.new = new;
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        let field = if self.path.is_empty() {
            self.component.clone()
        } else {
            format!("{}.{}", self.component, self.path)
        };
        format!("Set {field}")
    }
}

/// Sets a whole `Transform` (gizmo drags): translation, rotation and scale in one undo entry,
/// with drag coalescing.
pub struct SetTransform {
    /// Target entity (bits).
    pub entity: u64,
    /// The transform before the edit.
    pub old: Transform,
    /// The transform after the edit.
    pub new: Transform,
}

impl SetTransform {
    /// Captures the entity's current `Transform` (identity when absent).
    #[must_use]
    pub fn capture(world: &World, entity: Entity, new: Transform) -> Self {
        Self {
            entity: entity.to_bits(),
            old: world
                .get::<Transform>(entity)
                .copied()
                .unwrap_or(Transform::IDENTITY),
            new,
        }
    }

    fn write(&self, world: &mut World, value: &Transform) {
        let Some(entity) = resolve_entity(world, self.entity) else {
            tracing::warn!(target: "regia_editor", "SetTransform: entity {} is gone", self.entity);
            return;
        };
        world.entity_mut(entity).insert(*value);
    }
}

impl EditorCommand for SetTransform {
    fn touched(&self) -> Vec<CommandTouch> {
        vec![CommandTouch::entity(self.entity, Some("Transform".into()))]
    }

    fn apply(&mut self, world: &mut World) {
        self.write(world, &self.new);
    }

    fn revert(&mut self, world: &mut World) {
        self.write(world, &self.old);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = downcast_to::<SetTransform>(next) else {
            return false;
        };
        if next.entity == self.entity {
            self.new = next.new;
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        "Transform gizmo".to_string()
    }
}

/// One SpaceMouse Object-tool gesture (`docs/specs/29-spacemouse.md` § 2.3b): the transform
/// before and after, already applied live while the gesture ran. Never merges, so every gesture
/// is exactly one undo entry (a [`SetTransform`] would fold two gestures on one entity together).
pub struct TransformGesture(pub SetTransform);

impl EditorCommand for TransformGesture {
    fn touched(&self) -> Vec<CommandTouch> {
        self.0.touched()
    }

    fn apply(&mut self, world: &mut World) {
        self.0.apply(world);
    }

    fn revert(&mut self, world: &mut World) {
        self.0.revert(world);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "SpaceMouse object move".to_string()
    }
}

/// Spawns a serialized entity subtree; reverts by despawning the entities it created (tracked by
/// their stable scene guids).
pub struct SpawnEntities {
    /// The serialized subtree (guids are stable identity).
    pub file: SceneFile,
    /// Guids of every spawned entity (filled by the first `apply`).
    pub spawned_guids: Vec<Guid>,
    /// Entity bits from the latest apply, for Activity metadata. Initialize empty for a new command.
    pub spawned_entities: Vec<u64>,
}

impl SpawnEntities {
    /// Builds the command for one new entity with the given name, guid and transform.
    ///
    /// # Errors
    /// When the transform cannot be serialized through reflection.
    pub fn new_entity(
        world: &World,
        guid: Guid,
        name: &str,
        transform: Transform,
    ) -> Result<Self, CommandError> {
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let mut codec = ReflectCodec::new(&registry);
        let text = codec
            .to_ron_typed(transform.as_partial_reflect())
            .map_err(|e| CommandError::Reflect("Transform".to_string(), e.to_string()))?;
        let raw = ron::value::RawValue::from_ron(text.trim_end())
            .map_err(|e| CommandError::Reflect("Transform".to_string(), e.to_string()))?;
        let mut file = SceneFile::new();
        file.entities.push(SceneEntity {
            guid,
            name: Some(name.to_string()),
            parent: None,
            components: BTreeMap::from([("regia_scene::transform::Transform".to_string(), raw.to_owned())]),
            prefab_instance: None,
        });
        Ok(Self {
            file,
            spawned_guids: Vec::new(),
            spawned_entities: Vec::new(),
        })
    }

    /// Adds a reflected component to the (first) entity this command spawns, keyed by its type
    /// path like a scene file stores it.
    ///
    /// # Errors
    /// When the component's type is not registered or cannot be serialized through reflection.
    pub fn with_component(mut self, world: &World, value: &dyn PartialReflect) -> Result<Self, CommandError> {
        let type_path = value.reflect_type_path().to_string();
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let mut codec = ReflectCodec::new(&registry);
        let text = codec
            .to_ron_typed(value)
            .map_err(|e| CommandError::Reflect(type_path.clone(), e.to_string()))?;
        let raw = ron::value::RawValue::from_ron(text.trim_end())
            .map_err(|e| CommandError::Reflect(type_path.clone(), e.to_string()))?;
        if let Some(entity) = self.file.entities.first_mut() {
            entity.components.insert(type_path, raw.to_owned());
        }
        Ok(self)
    }

    fn despawn_guids(world: &mut World, guids: &[Guid]) {
        let mut doomed: Vec<Entity> = Vec::new();
        {
            let mut query = regia_ecs::ecs::query::QueryState::<(Entity, &EntityGuid)>::new(world);
            for (entity, guid) in query.iter(world) {
                if guids.contains(&guid.0) {
                    doomed.push(entity);
                }
            }
        }
        bury_and_despawn(world, &doomed, "revert spawn");
    }
}

impl EditorCommand for SpawnEntities {
    fn touched(&self) -> Vec<CommandTouch> {
        record_guids(&self.file.entities)
            .into_iter()
            .map(|guid| CommandTouch {
                document: "level".into(),
                entity: None,
                guid: Some(guid.to_string()),
                asset: None,
                property: None,
            })
            .collect()
    }

    fn activity_targets(&self) -> (Vec<u64>, Vec<String>) {
        (self.spawned_entities.clone(), Vec::new())
    }

    fn apply(&mut self, world: &mut World) {
        if self.spawned_guids.is_empty() {
            self.spawned_guids = self.file.entities.iter().map(|e| e.guid).collect();
            // What is placed joins the current layer set (spec 37 § 4.4); the file keeps it, so a
            // redo brings the same membership back.
            crate::layers_panel::stamp_current_set(world, &mut self.file);
        }
        // New entities land in the active level when several are open (ADR-7792).
        let level = crate::level_io::spawn_target_level(world);
        // A redo maps the re-created entities onto the ids later entries recorded.
        spawn_and_exhume(world, &self.file, level, "spawn command failed");
        let guids: HashSet<_> = self.spawned_guids.iter().copied().collect();
        self.spawned_entities = world
            .query::<(Entity, &EntityGuid)>()
            .iter(world)
            .filter_map(|(entity, guid)| guids.contains(&guid.0).then_some(entity.to_bits()))
            .collect();
        if let Some(mut table) = world.get_resource_mut::<regia_scene::layers::LayerTable>() {
            table.mark_dirty();
        }
    }

    fn revert(&mut self, world: &mut World) {
        Self::despawn_guids(world, &self.spawned_guids);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        let name = self
            .file
            .entities
            .first()
            .and_then(|e| e.name.clone())
            .unwrap_or_else(|| "entity".to_string());
        format!("Create {name}")
    }
}

/// Snapshots the subtrees of `entities` (descendants included) before they change, using the
/// scene crate's canonical snapshotter so undo data is format-identical to level saves.
pub(crate) fn subtree_snapshot(world: &mut World, entities: &[u64]) -> SceneFile {
    // Collect the subtree of each target (descendants through `Children`).
    // A set, not a `Vec::contains` scan: a live reload snapshots every instance of a prefab at
    // once (rc2 review, editor-undo finding 3).
    let mut keep: HashSet<Entity> = HashSet::new();
    for bits in entities {
        let Some(root) = resolve_entity(world, *bits) else {
            continue;
        };
        let mut stack = vec![root];
        while let Some(current) = stack.pop() {
            if !keep.insert(current) {
                continue;
            }
            if let Some(children) = world.get::<Children>(current) {
                let children: &[Entity] = children;
                stack.extend_from_slice(children);
            }
        }
    }
    let filter = SnapshotFilter {
        level: None,
        resources: Vec::new(),
    };
    let Ok(file) = snapshot_scene(world, &filter) else {
        return SceneFile::new();
    };
    let guids_of = |entity: Entity| world.get::<EntityGuid>(entity).map(|g| g.0);
    let kept: HashSet<Guid> = keep.iter().filter_map(|e| guids_of(*e)).collect();
    let mut out = SceneFile::new();
    out.entities = kept_records(file.entities, &kept);
    // Parents outside the kept set would fail `spawn_scene`'s validation; those entities
    // re-root instead (the same policy `snapshot_scene` applies for out-of-filter parents).
    // An added child of a prefab instance whose instance stays names its parent by the
    // internal entity's local id, which is never in `kept`: it re-roots too, and the undo
    // (`exhume`) puts it back under that live internal entity at its old sibling place.
    for record in &mut out.entities {
        if record.parent.is_some_and(|parent| !kept.contains(&parent)) {
            record.parent = None;
        }
    }
    out
}

/// The records of `records` whose guid is in `kept`, as top-level records. `snapshot_scene`
/// writes an entity added on top of a prefab instance inside the instance record's
/// `added_children`, never at the top level: a kept added child of an instance that is not kept
/// is lifted out of it (with whatever it nests), so deleting or duplicating it alone still has
/// its record (ADR-7412). A kept instance keeps its own added children nested.
pub(crate) fn kept_records(records: Vec<SceneEntity>, kept: &HashSet<Guid>) -> Vec<SceneEntity> {
    let mut out = Vec::new();
    // Depth-first in file order, without recursion (instances nest inside added children).
    let mut stack: Vec<SceneEntity> = records.into_iter().rev().collect();
    while let Some(record) = stack.pop() {
        if kept.contains(&record.guid) {
            out.push(record);
        } else if let Some(instance) = record.prefab_instance {
            stack.extend(instance.added_children.into_iter().rev());
        }
    }
    out
}

/// Every guid in `records`, the records nested in instances' `added_children` included.
pub(crate) fn record_guids(records: &[SceneEntity]) -> Vec<Guid> {
    let mut guids = Vec::new();
    let mut stack: Vec<&SceneEntity> = records.iter().collect();
    while let Some(record) = stack.pop() {
        guids.push(record.guid);
        if let Some(instance) = &record.prefab_instance {
            stack.extend(instance.added_children.iter());
        }
    }
    guids
}

/// Undo entry for entities spawned by a caller that owns the spawn itself (prefab instances):
/// the first [`apply`](EditorCommand::apply) sees them alive and just snapshots them for redo;
/// after a revert (despawn) it re-spawns them from that snapshot.
pub struct SpawnedRoots {
    /// The spawned roots (bits).
    pub entities: Vec<u64>,
    /// Serialized snapshot for redo (captured on the first apply).
    pub snapshot: Option<SceneFile>,
}

impl EditorCommand for SpawnedRoots {
    fn touched(&self) -> Vec<CommandTouch> {
        self.entities
            .iter()
            .map(|id| CommandTouch::entity(*id, None))
            .collect()
    }

    fn apply(&mut self, world: &mut World) {
        let all_alive = self
            .entities
            .iter()
            .all(|bits| resolve_entity(world, *bits).is_some());
        if all_alive {
            if self.snapshot.is_none() {
                self.snapshot = Some(subtree_snapshot(world, &self.entities));
            }
            return;
        }
        // Redo: re-spawn from the snapshot, back into the level and onto the ids the revert
        // recorded (later entries refer to them).
        if let Some(file) = &self.snapshot {
            spawn_and_exhume(world, file, None, "redo spawn");
        }
    }

    fn revert(&mut self, world: &mut World) {
        let roots: Vec<Entity> = self
            .entities
            .iter()
            .filter_map(|bits| resolve_entity(world, *bits))
            .collect();
        bury_and_despawn(world, &roots, "revert spawn");
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Spawn entity".to_string()
    }
}

/// Despawns entities (with their subtrees); reverts by re-spawning the serialized snapshot.
pub struct DespawnEntities {
    /// Entities to remove (bits).
    pub entities: Vec<u64>,
    /// Serialized snapshot of the removed subtrees (filled by the first `apply`).
    pub snapshot: Option<SceneFile>,
}

impl DespawnEntities {
    /// Snapshots the subtrees of `entities` now, before anything is removed — see
    /// [`subtree_snapshot`].
    fn take_snapshot(world: &mut World, entities: &[u64]) -> SceneFile {
        subtree_snapshot(world, entities)
    }
}

impl EditorCommand for DespawnEntities {
    fn touched(&self) -> Vec<CommandTouch> {
        self.entities
            .iter()
            .map(|id| CommandTouch::entity(*id, None))
            .collect()
    }

    fn apply(&mut self, world: &mut World) {
        if self.snapshot.is_none() {
            let alive: Vec<u64> = self
                .entities
                .iter()
                .copied()
                .filter(|bits| resolve_entity(world, *bits).is_some())
                .collect();
            self.snapshot = Some(Self::take_snapshot(world, &alive));
        }
        // A redo after an undo finds the restored entities under their new ids.
        let roots: Vec<Entity> = self
            .entities
            .iter()
            .filter_map(|bits| resolve_entity(world, *bits))
            .collect();
        bury_and_despawn(world, &roots, "despawn");
    }

    fn revert(&mut self, world: &mut World) {
        let Some(file) = &self.snapshot else { return };
        // Level membership, `Persistent` and sibling order come back from the burial record:
        // without them a restored entity would be dropped by the next level save.
        spawn_and_exhume(world, file, None, "revert despawn");
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Delete {} entities", self.entities.len())
    }
}

/// Moves an entity under a new parent (or detaches it). Plain reparent: the local `Transform` is
/// kept, so the world pose moves with the new parent. Callers wanting pose preservation adjust
/// the transform before constructing the command.
pub struct Reparent {
    /// The moved entity (bits).
    pub entity: u64,
    /// Parent before the move (`None` = root).
    pub old_parent: Option<u64>,
    /// Parent after the move (`None` = root).
    pub new_parent: Option<u64>,
    /// The Outliner folder (`EditorFolder`, ADR-7757) the last apply took off the entity: a move
    /// that gives it a parent or takes one away drops it (restored by revert).
    pub cleared_folder: Option<regia_scene::components::EditorFolder>,
}

impl Reparent {
    /// Captures the current parent.
    #[must_use]
    pub fn capture(world: &World, entity: Entity, new_parent: Option<Entity>) -> Self {
        Self {
            entity: entity.to_bits(),
            old_parent: world.get::<ChildOf>(entity).map(|c| c.parent().to_bits()),
            new_parent: new_parent.map(Entity::to_bits),
            cleared_folder: None,
        }
    }

    fn set_parent(&self, world: &mut World, parent: Option<u64>) {
        let Some(entity) = resolve_entity(world, self.entity) else {
            tracing::warn!(target: "regia_editor", "reparent: entity gone");
            return;
        };
        match parent {
            Some(bits) => {
                let Some(parent) = resolve_entity(world, bits) else {
                    tracing::warn!(target: "regia_editor", "reparent: parent gone");
                    return;
                };
                world.entity_mut(entity).insert(ChildOf(parent));
            }
            None => {
                world.entity_mut(entity).remove::<ChildOf>();
            }
        }
    }
}

impl EditorCommand for Reparent {
    fn touched(&self) -> Vec<CommandTouch> {
        vec![CommandTouch::entity(self.entity, Some("ChildOf".into()))]
    }

    fn apply(&mut self, world: &mut World) {
        let had_parent =
            resolve_entity(world, self.entity).is_some_and(|e| world.get::<ChildOf>(e).is_some());
        self.set_parent(world, self.new_parent);
        self.cleared_folder = resolve_entity(world, self.entity).and_then(|entity| {
            crate::hierarchy_ops::take_stale_folder(world, entity, had_parent, self.new_parent.is_some())
        });
    }

    fn revert(&mut self, world: &mut World) {
        self.set_parent(world, self.old_parent);
        if let Some(folder) = self.cleared_folder.take()
            && let Some(entity) = resolve_entity(world, self.entity)
        {
            world.entity_mut(entity).insert(folder);
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Reparent".to_string()
    }
}

/// Adds a reflected component to an entity; reverts by removing it (or restoring the previous
/// value when the component already existed).
pub struct AddComponent {
    /// Target entity (bits).
    pub entity: u64,
    /// Component type name.
    pub component: String,
    /// The value to insert.
    pub value: Box<dyn PartialReflect>,
    /// The previous value when the component already existed (then revert restores, not removes).
    pub previous: Option<Box<dyn PartialReflect>>,
}

impl AddComponent {
    /// Captures the previous value (if the entity already has the component).
    ///
    /// # Errors
    /// When the component type is not registered ([`CommandError`]).
    pub fn capture(
        world: &World,
        entity: Entity,
        component: &str,
        value: Box<dyn PartialReflect>,
    ) -> Result<Self, CommandError> {
        let previous = read_field(world, entity, component, "").ok();
        Ok(Self {
            entity: entity.to_bits(),
            component: component.to_string(),
            value,
            previous,
        })
    }
}

impl EditorCommand for AddComponent {
    fn touched(&self) -> Vec<CommandTouch> {
        vec![CommandTouch::entity(self.entity, Some(self.component.clone()))]
    }

    fn apply(&mut self, world: &mut World) {
        let Some(entity) = resolve_entity(world, self.entity) else {
            return;
        };
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let Ok(data) = component_data(&registry, &self.component) else {
            tracing::warn!(target: "regia_editor", "add: no component {}", self.component);
            return;
        };
        if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
            data.insert(&mut entity_mut, self.value.as_ref(), &registry);
        }
    }

    fn revert(&mut self, world: &mut World) {
        let Some(entity) = resolve_entity(world, self.entity) else {
            return;
        };
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let Ok(data) = component_data(&registry, &self.component) else {
            return;
        };
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            return;
        };
        match &self.previous {
            Some(old) => data.insert(&mut entity_mut, old.as_ref(), &registry),
            None => data.remove(&mut entity_mut),
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Add {}", self.component)
    }
}

/// Removes a reflected component; reverts by re-inserting the captured value.
pub struct RemoveComponent {
    /// Target entity (bits).
    pub entity: u64,
    /// Component type name.
    pub component: String,
    /// The removed value (captured at construction).
    pub old: Option<Box<dyn PartialReflect>>,
}

impl RemoveComponent {
    /// Captures the component's current value, so the removal can be undone. Unlike a plain
    /// `reflect_clone`, this also captures components holding fixed-size arrays (`[f32; 3]`)
    /// that bevy_reflect can only clone dynamically.
    ///
    /// # Errors
    /// When the entity or component does not resolve, or the entity lacks the component
    /// ([`CommandError`]).
    pub fn capture(world: &World, entity: Entity, component: &str) -> Result<Self, CommandError> {
        let old = read_field(world, entity, component, "")?;
        Ok(Self {
            entity: entity.to_bits(),
            component: component.to_string(),
            old: Some(old),
        })
    }
}

impl EditorCommand for RemoveComponent {
    fn touched(&self) -> Vec<CommandTouch> {
        vec![CommandTouch::entity(self.entity, Some(self.component.clone()))]
    }

    fn apply(&mut self, world: &mut World) {
        let Some(entity) = resolve_entity(world, self.entity) else {
            return;
        };
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let Ok(data) = component_data(&registry, &self.component) else {
            return;
        };
        if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
            data.remove(&mut entity_mut);
        }
    }

    fn revert(&mut self, world: &mut World) {
        let (Some(entity), Some(old)) = (resolve_entity(world, self.entity), &self.old) else {
            return;
        };
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let Ok(data) = component_data(&registry, &self.component) else {
            return;
        };
        if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
            data.insert(&mut entity_mut, old.as_ref(), &registry);
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Remove {}", self.component)
    }
}

/// Several commands as one undo entry (`specs/19` § 1 "transaction groups"): applied in order,
/// reverted in reverse order, never merged. One user action that takes several commands (create
/// a child = spawn + reparent) is one Ctrl+Z.
pub struct CommandGroup {
    /// What the group does, for the UI ("Create child").
    pub label: String,
    /// The commands, in apply order.
    pub commands: Vec<Box<dyn EditorCommand>>,
}

impl CommandGroup {
    /// An empty group with a label.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            commands: Vec::new(),
        }
    }

    /// Appends a command that is already applied (the group's first `apply` on the stack is a
    /// redo).
    pub fn push(&mut self, command: Box<dyn EditorCommand>) {
        self.commands.push(command);
    }
}

impl EditorCommand for CommandGroup {
    fn changes_level_state(&self) -> bool {
        self.commands.iter().any(|command| command.changes_level_state())
    }

    fn touched(&self) -> Vec<CommandTouch> {
        let mut touched = Vec::new();
        for detail in self.commands.iter().flat_map(|command| command.touched()) {
            if !touched.contains(&detail) {
                touched.push(detail);
            }
        }
        touched
    }

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

/// Replaces the override list of one [`PrefabInstance`].
///
/// A multi-edit appends this after the property writes in a [`CommandGroup`], so one undo restores
/// the markers and the values (revert runs last-to-first). Applying writes `new`; reverting writes
/// `old`. It never merges.
pub struct SetPrefabOverrides {
    /// Instance root (bits).
    pub entity: u64,
    /// Overrides before the edit.
    pub old: Vec<OverrideEntry>,
    /// Overrides after the edit.
    pub new: Vec<OverrideEntry>,
}

impl SetPrefabOverrides {
    fn write(world: &mut World, bits: u64, overrides: &[OverrideEntry]) {
        let Some(entity) = resolve_entity(world, bits) else {
            tracing::warn!(target: "regia_editor", "prefab overrides: entity {bits} is gone");
            return;
        };
        let Some(mut instance) = world.get_mut::<PrefabInstance>(entity) else {
            tracing::warn!(target: "regia_editor", "prefab overrides: entity {bits} has no PrefabInstance");
            return;
        };
        instance.overrides = overrides.to_vec();
    }
}

impl EditorCommand for SetPrefabOverrides {
    fn touched(&self) -> Vec<CommandTouch> {
        vec![CommandTouch::entity(
            self.entity,
            Some("PrefabInstance.overrides".into()),
        )]
    }

    fn apply(&mut self, world: &mut World) {
        Self::write(world, self.entity, &self.new);
    }

    fn revert(&mut self, world: &mut World) {
        Self::write(world, self.entity, &self.old);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Prefab overrides".to_string()
    }
}

/// One undo entry and the id of the world state it leads to.
struct Entry {
    command: Box<dyn EditorCommand>,
    /// The state after this entry (a fresh id whenever the entry changes by merging).
    state: u64,
    /// The open documents (levels and layer files, ADR-7792) this entry changed; empty when no
    /// document feed tagged it ([`UndoStack::tag_top`]).
    documents: Vec<u128>,
}

/// The editor's undo/redo history (`specs/19` § 1): capped, coalescing, dirty-tracking.
///
/// Dirty tracking compares states, not depths: every entry carries the id of the world state it
/// leads to, a save remembers the current id, and the level is dirty exactly when the current
/// id differs. Undoing back to the saved state makes the level clean again; redoing past it,
/// merging into the saved entry, or editing after undoing below it makes it dirty.
#[derive(Resource)]
pub struct UndoStack {
    pub(crate) activity: crate::activity_panel::CommandActivity,
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    /// The last state id handed out.
    last_state: u64,
    /// The state with no entry left to undo (moves up when the cap drops the oldest entry).
    base_state: u64,
    /// The state at the last save; `None` when no reachable state is saved (restored autosave).
    saved_state: Option<u64>,
    /// The next push must not merge into the top entry (an undo, redo, save or [`seal`]
    /// happened since the top was pushed).
    ///
    /// [`seal`]: Self::seal
    sealed: bool,
    /// Per open document (ADR-7792): the document's state at its last save. A document missing
    /// here was never edited since it opened, and is saved at state 0.
    document_saved: HashMap<u128, u64>,
    /// Per document: its state with none of its entries left to undo (moves up when the cap drops
    /// the oldest entry that changed it).
    document_base: HashMap<u128, u64>,
    /// The newest state id the document feed has seen ([`take_new_entry`](Self::take_new_entry)).
    fed_state: u64,
    transaction: Option<Transaction>,
    /// An agent request is running (set by the editor's request hook, cleared at each dispatcher
    /// drain boundary): its pushes join an agent transaction.
    agent_request: bool,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self {
            activity: Default::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            last_state: 0,
            base_state: 0,
            saved_state: Some(0),
            sealed: false,
            document_saved: HashMap::new(),
            document_base: HashMap::new(),
            fed_state: 0,
            transaction: None,
            agent_request: false,
        }
    }
}

impl UndoStack {
    /// An empty stack.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn fresh_state(&mut self) -> u64 {
        self.last_state += 1;
        self.last_state
    }

    /// The id of the current world state.
    fn current_state(&self) -> u64 {
        self.transaction.as_ref().map_or_else(
            || self.undo.last().map_or(self.base_state, |entry| entry.state),
            |transaction| transaction.state,
        )
    }

    /// Pushes an applied command; coalesces with the top entry when it merges, unless the stack
    /// is sealed. Pushing after an undo drops the redo branch (standard semantics).
    ///
    /// While a transaction is open the command joins it, except in an agent transaction
    /// ([`transaction_begin_agent`](Self::transaction_begin_agent)) when no agent request runs:
    /// then it is its own entry below the transaction (a user's edit during an agent script is not
    /// reverted by its abort nor folded into its commit).
    pub fn push(&mut self, command: Box<dyn EditorCommand>) {
        if let Some(transaction) = &self.transaction {
            if !transaction.agent || self.agent_request {
                let state = if command.changes_level_state() {
                    self.fresh_state()
                } else {
                    self.current_state()
                };
                if let Some(transaction) = &mut self.transaction {
                    transaction.state = state;
                    transaction.group.push(command);
                }
                return;
            }
            let level = command.changes_level_state();
            self.push_entry(command);
            // The transaction's pending state stays the newest: it holds this edit and the
            // agent's commands.
            if level {
                let state = self.fresh_state();
                if let Some(transaction) = &mut self.transaction {
                    transaction.state = state;
                }
            }
            return;
        }
        self.push_entry(command);
    }

    /// Whether an agent request is running (its pushes join an agent transaction). The editor's
    /// request hook sets it before each agent request and clears it at each drain boundary.
    pub fn set_agent_request(&mut self, running: bool) {
        self.agent_request = running;
    }

    /// Whether [`set_agent_request`](Self::set_agent_request) last said an agent request runs.
    #[must_use]
    pub fn agent_request(&self) -> bool {
        self.agent_request
    }

    /// Pushes `command` as an entry of the undo list (no transaction involved).
    fn push_entry(&mut self, command: Box<dyn EditorCommand>) {
        self.activity.record(command.as_ref(), "edit");
        self.redo.clear();
        let sealed = std::mem::take(&mut self.sealed);
        if !sealed
            && let Some(top) = self.undo.last_mut()
            && top.command.merge(command.as_ref())
        {
            // A file-only command (a material-graph drag, an input-map rewrite) reuses the
            // current state id, the same way a non-merging push does, so coalescing it never
            // marks a saved level dirty.
            if command.changes_level_state() {
                let state = self.fresh_state();
                if let Some(top) = self.undo.last_mut() {
                    top.state = state;
                }
            }
            return;
        }
        if self.undo.len() >= UNDO_CAP {
            let dropped = self.undo.remove(0);
            self.base_state = dropped.state;
            for document in dropped.documents {
                self.document_base.insert(document, dropped.state);
            }
        }
        // A command that leaves the level state untouched (a file-only undo entry) reuses the
        // current state id, so pushing, undoing or redoing it never changes `is_dirty`.
        let state = if command.changes_level_state() {
            self.fresh_state()
        } else {
            self.current_state()
        };
        self.undo.push(Entry {
            command,
            state,
            documents: Vec::new(),
        });
    }

    /// Ends coalescing: the next [`push`](Self::push) becomes its own entry even when it would
    /// merge into the top one. Call when a continuous edit ends (pointer released after a gizmo
    /// drag or slider scrub) and after discrete edits that must stay single steps.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    /// The open transaction's label, if any.
    #[must_use]
    pub fn transaction_label(&self) -> Option<&str> {
        self.transaction.as_ref().map(|t| t.group.label.as_str())
    }

    /// Begins collecting applied commands. An empty commit preserves the redo branch.
    ///
    /// # Errors
    /// [`HistoryError::Nested`] if already collecting commands.
    pub fn transaction_begin(&mut self, label: impl Into<String>) -> Result<(), HistoryError> {
        if self.transaction.is_some() {
            return Err(HistoryError::Nested);
        }
        self.seal();
        self.transaction = Some(Transaction {
            group: CommandGroup::new(label),
            state: self.current_state(),
            agent: false,
        });
        Ok(())
    }

    /// Begins an agent's transaction (`editor.transaction_begin`): like
    /// [`transaction_begin`](Self::transaction_begin), but only commands pushed while an agent
    /// request runs join it; the user's own edits in between stay separate entries.
    ///
    /// # Errors
    /// [`HistoryError::Nested`] if already collecting commands.
    pub fn transaction_begin_agent(&mut self, label: impl Into<String>) -> Result<(), HistoryError> {
        self.transaction_begin(label)?;
        if let Some(transaction) = &mut self.transaction {
            transaction.agent = true;
        }
        Ok(())
    }

    /// Commits the collected commands as one entry; returns whether it contained any edits.
    ///
    /// # Errors
    /// [`HistoryError::Missing`] without a transaction.
    pub fn transaction_commit(&mut self) -> Result<bool, HistoryError> {
        let transaction = self.transaction.take().ok_or(HistoryError::Missing)?;
        if transaction.group.commands.is_empty() {
            return Ok(false);
        }
        // Keep the pending state id: a save during the transaction must remain the baseline.
        self.redo.clear();
        if self.undo.len() >= UNDO_CAP {
            let dropped = self.undo.remove(0);
            self.base_state = dropped.state;
            for document in dropped.documents {
                self.document_base.insert(document, dropped.state);
            }
        }
        self.undo.push(Entry {
            command: Box::new(transaction.group),
            state: transaction.state,
            documents: Vec::new(),
        });
        self.seal();
        Ok(true)
    }

    /// Reverts collected commands in reverse order and keeps the previous redo branch.
    ///
    /// # Errors
    /// [`HistoryError::Missing`] without a transaction.
    pub fn transaction_abort(&mut self, world: &mut World) -> Result<(), HistoryError> {
        let mut transaction = self.transaction.take().ok_or(HistoryError::Missing)?;
        transaction.group.revert(world);
        self.seal();
        Ok(())
    }

    /// Runs extension edits against the world, grouping commands they push into one entry.
    /// A panic commits the applied commands before resuming unwinding, so they remain undoable.
    /// The closure must use command-based editing APIs; raw world writes cannot be captured.
    ///
    /// # Errors
    /// [`HistoryError::Nested`] if an agent or extension already has a transaction open.
    pub fn transaction<R>(
        world: &mut World,
        label: impl Into<String>,
        edit: impl FnOnce(&mut World) -> R,
    ) -> Result<R, HistoryError> {
        world
            .get_resource_or_insert_with::<Self>(Self::new)
            .transaction_begin(label)?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| edit(world)));
        let commit = world
            .get_resource_or_insert_with::<Self>(Self::new)
            .transaction_commit();
        match result {
            Ok(value) => {
                commit?;
                Ok(value)
            }
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Chronological entries, including the redo branch. No command is applied by this read.
    #[must_use]
    pub fn history(&self) -> Vec<HistoryEntry> {
        self.undo
            .iter()
            .chain(self.redo.iter().rev())
            .enumerate()
            .map(|(index, entry)| {
                let touched = entry.command.touched();
                let mut documents: Vec<String> = touched.iter().map(|touch| touch.document.clone()).collect();
                if entry.command.changes_level_state() {
                    documents.push("level".into());
                }
                documents.sort();
                documents.dedup();
                HistoryEntry {
                    index: index + 1,
                    description: entry.command.description(),
                    applied: index < self.undo.len(),
                    documents,
                    touched,
                }
            })
            .collect()
    }

    /// Moves to a retained history boundary, invoking every intervening apply/revert once.
    ///
    /// # Errors
    /// [`HistoryError::Active`] during a transaction; [`HistoryError::Index`] out of range.
    pub fn jump(&mut self, world: &mut World, index: usize) -> Result<(), HistoryError> {
        if self.transaction.is_some() {
            return Err(HistoryError::Active);
        }
        if index > self.undo.len() + self.redo.len() {
            return Err(HistoryError::Index);
        }
        while self.undo.len() > index {
            self.undo(world);
        }
        while self.undo.len() < index {
            self.redo(world);
        }
        Ok(())
    }

    /// Removes the top undo entry and returns its command, still applied, so a caller can fold it
    /// into a larger entry (an Alt+drag duplicate becomes part of the move, ADR-7204). The redo
    /// branch is untouched; the stack is sealed.
    pub fn take_top(&mut self) -> Option<Box<dyn EditorCommand>> {
        self.sealed = true;
        if let Some(transaction) = &mut self.transaction {
            return transaction.group.commands.pop();
        }
        self.undo.pop().map(|entry| entry.command)
    }

    /// The id of the current world state: a fresh, larger id after every new edit or merge, an
    /// older one after an undo (the sequencer's auto key watches it, ADR-9710).
    #[must_use]
    pub fn state_id(&self) -> u64 {
        self.current_state()
    }

    /// `true` when the next [`push`](Self::push) starts a new entry (a continuous edit ended).
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// The command of the top undo entry.
    #[must_use]
    pub fn top(&self) -> Option<&dyn EditorCommand> {
        if let Some(transaction) = &self.transaction {
            return transaction.group.commands.last().map(|command| command.as_ref());
        }
        self.undo.last().map(|entry| entry.command.as_ref())
    }

    /// Joins an already applied `command` into the top entry, so one undo reverts both (a
    /// [`CommandGroup`] named after the top entry; with no top entry it is pushed alone). The
    /// stack stays sealed: the next push is its own entry. Used by the sequencer's auto key to make
    /// a scene edit and its key one step (ADR-9710).
    pub fn join_top(&mut self, command: Box<dyn EditorCommand>) {
        if self.transaction.is_some() {
            self.push(command);
            return;
        }
        let Some(top) = self.undo.pop() else {
            self.push(command);
            self.sealed = true;
            return;
        };
        self.activity.record(command.as_ref(), "edit");
        self.redo.clear();
        let changes_level = command.changes_level_state();
        let mut group = CommandGroup::new(top.command.description());
        group.push(top.command);
        group.push(command);
        let state = if changes_level {
            self.fresh_state()
        } else {
            top.state
        };
        self.undo.push(Entry {
            command: Box::new(group),
            state,
            documents: top.documents,
        });
        self.sealed = true;
    }

    /// Undoes the top entry; `false` when there is nothing to undo.
    pub fn undo(&mut self, world: &mut World) -> bool {
        if self.transaction.is_some() {
            return false;
        }
        if let Some(mut entry) = self.undo.pop() {
            self.activity.record(entry.command.as_ref(), "undo");
            entry.command.revert(world);
            self.redo.push(entry);
            self.sealed = true;
            true
        } else {
            false
        }
    }

    /// Redoes the last undone entry; `false` when there is nothing to redo.
    pub fn redo(&mut self, world: &mut World) -> bool {
        if self.transaction.is_some() {
            return false;
        }
        if let Some(mut entry) = self.redo.pop() {
            self.activity.record(entry.command.as_ref(), "redo");
            entry.command.apply(world);
            self.undo.push(entry);
            self.sealed = true;
            true
        } else {
            false
        }
    }

    /// Number of entries that can be undone.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// Number of entries that can be redone.
    #[must_use]
    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    /// Description of the next undone action (for the Edit menu).
    #[must_use]
    pub fn next_undo(&self) -> Option<String> {
        self.undo.last().map(|entry| entry.command.description())
    }

    /// Description of the next redone action (for the Edit menu).
    #[must_use]
    pub fn next_redo(&self) -> Option<String> {
        self.redo.last().map(|entry| entry.command.description())
    }

    /// Descriptions of every undoable entry, next to undo first.
    #[must_use]
    pub fn undo_descriptions(&self) -> Vec<String> {
        self.undo
            .iter()
            .rev()
            .map(|entry| entry.command.description())
            .collect()
    }

    /// Descriptions of every redoable entry, next to redo first.
    #[must_use]
    pub fn redo_descriptions(&self) -> Vec<String> {
        self.redo
            .iter()
            .rev()
            .map(|entry| entry.command.description())
            .collect()
    }

    /// Whether the world differs from the state at the last [`mark_saved`](Self::mark_saved).
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.saved_state != Some(self.current_state())
    }

    /// Marks the current state as the saved baseline (and seals: an edit after a save is its
    /// own entry, so undo can return exactly to the saved state).
    pub fn mark_saved(&mut self) {
        self.saved_state = Some(self.current_state());
        self.sealed = true;
    }

    /// Tags the top undo entry with the open documents it changed (ADR-7792); the document feed
    /// (`crate::level_dirty::feed_documents`) calls it after a push. Tags accumulate (a merged
    /// gesture that later reaches another document keeps both). `false` without an entry.
    pub fn tag_top(&mut self, documents: &[u128]) -> bool {
        let Some(top) = self.undo.last_mut() else {
            return false;
        };
        for document in documents {
            if !top.documents.contains(document) {
                top.documents.push(*document);
            }
        }
        true
    }

    /// Whether the top undo entry is new since the last call (a push or a merge made it; an undo
    /// or a redo only moves between entries the feed already saw), and marks everything up to now
    /// seen. The document feed asks before it tags ([`tag_top`](Self::tag_top)).
    pub fn take_new_entry(&mut self) -> bool {
        let new = !self.undo.is_empty() && self.current_state() > self.fed_state;
        self.fed_state = self.last_state;
        new
    }

    /// The documents the top undo entry is tagged with.
    #[must_use]
    pub fn top_documents(&self) -> &[u128] {
        self.undo
            .last()
            .map(|entry| entry.documents.as_slice())
            .unwrap_or(&[])
    }

    /// The state of one open document: the state after the newest undoable entry that changed it.
    #[must_use]
    pub fn document_state(&self, document: u128) -> u64 {
        self.undo
            .iter()
            .rev()
            .find(|entry| entry.documents.contains(&document))
            .map_or_else(
                || self.document_base.get(&document).copied().unwrap_or(0),
                |entry| entry.state,
            )
    }

    /// Whether `document` differs from its last save: an undo back to the save makes it clean
    /// again, exactly like [`is_dirty`](Self::is_dirty) for the whole history.
    #[must_use]
    pub fn is_document_dirty(&self, document: u128) -> bool {
        self.document_saved.get(&document).copied().unwrap_or(0) != self.document_state(document)
    }

    /// Marks `document`'s current state as saved (and seals). The whole history's dirty flag is
    /// left alone: other documents may still have unsaved edits.
    pub fn mark_document_saved(&mut self, document: u128) {
        let state = self.document_state(document);
        self.document_saved.insert(document, state);
        self.sealed = true;
    }

    /// Forgets a closed document's save state (its entries stay; they find nothing to change).
    pub fn forget_document(&mut self, document: u128) {
        self.document_saved.remove(&document);
        self.document_base.remove(&document);
    }

    /// Marks the level unsaved without pushing a command.
    ///
    /// A restored autosave is the work the user had not saved; closing must still ask
    /// (`specs/19` § 7) instead of treating the world as clean and deleting the autosave. No
    /// undo or redo makes it clean again; only a save does.
    pub fn mark_dirty(&mut self) {
        self.saved_state = None;
    }
}

/// Undoes through the world's [`UndoStack`]. The stack is removed from the world for the call
/// (it edits the world while reverting) and reinserted after; standard Bevy resource dance.
/// Entities the undo removed leave the editor selection.
pub fn undo_world(world: &mut World) -> bool {
    crate::procgen::undo::drain(world);
    let Some(mut stack) = world.remove_resource::<UndoStack>() else {
        return false;
    };
    let done = stack.undo(world);
    world.insert_resource(stack);
    retain_selection(world);
    done
}

/// Redoes through the world's [`UndoStack`]; see [`undo_world`].
pub fn redo_world(world: &mut World) -> bool {
    crate::procgen::undo::drain(world);
    let Some(mut stack) = world.remove_resource::<UndoStack>() else {
        return false;
    };
    let done = stack.redo(world);
    world.insert_resource(stack);
    retain_selection(world);
    done
}

/// Jumps through the world's shared history and drops dead entities from the selection.
///
/// # Errors
/// A transaction is active or `index` is outside the retained history.
pub fn jump_world(world: &mut World, index: usize) -> Result<(), HistoryError> {
    crate::procgen::undo::drain(world);
    let mut stack = world.remove_resource::<UndoStack>().unwrap_or_default();
    let result = stack.jump(world, index);
    world.insert_resource(stack);
    retain_selection(world);
    result
}

/// Drops despawned entities from the selection (an undone create, a redone delete).
fn retain_selection(world: &mut World) {
    if let Some(mut selection) = world.remove_resource::<crate::selection::Selection>() {
        selection.retain_alive(world);
        world.insert_resource(selection);
    }
}

#[derive(Resource, Default)]
struct HistoryNavigationFrame(Option<u64>);

/// Runs a panel's undo or redo once per egui frame, even across repeated layout passes.
pub(crate) fn navigate_world_once(world: &mut World, frame: u64, redo: bool) -> bool {
    let mut last = world.get_resource_or_insert_with::<HistoryNavigationFrame>(Default::default);
    if last.0 == Some(frame) {
        return false;
    }
    last.0 = Some(frame);
    if redo {
        redo_world(world)
    } else {
        undo_world(world)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::App;

    /// The old `EntityRemap::record`: scan every entry. The reverse-indexed one must give the
    /// same map for any sequence of records.
    fn naive_record(forward: &mut HashMap<u64, u64>, old: u64, new: u64) {
        if old == new {
            return;
        }
        for target in forward.values_mut() {
            if *target == old {
                *target = new;
            }
        }
        forward.remove(&new);
        forward.insert(old, new);
    }

    /// rc2 review (editor-undo finding 3): the reverse index keeps `record` equal to the full
    /// scan on a fixed pseudo-random sequence (ids drawn from a small range, so chains, cycles
    /// and ids coming back all occur), and its reverse entries match the forward map.
    #[test]
    fn entity_remap_record_matches_the_full_scan() {
        let mut remap = EntityRemap::default();
        let mut naive: HashMap<u64, u64> = HashMap::new();
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % 24
        };
        for _ in 0..4_000 {
            let (old, new) = (next(), next());
            remap.record(old, new);
            naive_record(&mut naive, old, new);
            assert_eq!(remap.forward, naive, "after record({old}, {new})");
        }
        let mut pairs: Vec<(u64, u64)> = remap
            .backward
            .iter()
            .flat_map(|(target, olds)| olds.iter().map(move |old| (*old, *target)))
            .collect();
        pairs.sort_unstable();
        let mut expected: Vec<(u64, u64)> = remap.forward.iter().map(|(o, t)| (*o, *t)).collect();
        expected.sort_unstable();
        assert_eq!(
            pairs, expected,
            "the reverse index is exactly the forward map inverted"
        );
    }
    use regia_ecs::reflect::Reflect;
    // The scene `Name` is the one a level saves (the editor fills it from the ECS `Name` for user
    // entities, `visibility::sync_names`); an ECS-only name does not survive a delete and undo.
    use regia_scene::prelude::Name;

    /// A reflected component for property edits.
    #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Marker {
        value: f32,
    }

    /// A component holding a fixed-size array (bevy_reflect clones it only dynamically).
    #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Tint {
        rgb: [f32; 3],
    }

    fn app() -> App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<Marker>();
        app.register_type::<Tint>();
        app
    }

    fn by_guid(world: &mut World, guid: Guid) -> Option<Entity> {
        world
            .query::<(Entity, &EntityGuid)>()
            .iter(world)
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e)
    }

    fn guid_of(world: &World, entity: Entity) -> Guid {
        world.get::<EntityGuid>(entity).expect("guid").0
    }

    fn spawn_named(world: &mut World, name: &str) -> Entity {
        world
            .spawn((
                Name::new(name.to_string()),
                Transform::default(),
                EntityGuid::new_random(),
            ))
            .id()
    }

    #[test]
    fn set_property_undo_redo_round_trip() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Mover");
        app.world_mut().entity_mut(entity).insert(Marker { value: 1.0 });

        let mut undo = UndoStack::new();
        let mut command =
            SetProperty::capture(app.world(), entity, "Marker", "value", Box::new(5.0f32)).expect("capture");
        command.apply(app.world_mut());
        undo.push(Box::new(command));
        assert_eq!(app.world().get::<Marker>(entity).expect("marker").value, 5.0);

        assert!(undo.undo(app.world_mut()));
        assert_eq!(app.world().get::<Marker>(entity).expect("marker").value, 1.0);
        assert!(undo.redo(app.world_mut()));
        assert_eq!(app.world().get::<Marker>(entity).expect("marker").value, 5.0);
    }

    #[test]
    fn capture_reports_missing_targets() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Plain");
        assert!(
            SetProperty::capture(app.world(), entity, "Marker", "value", Box::new(0.0f32)).is_err(),
            "no Marker on the entity yet"
        );
        assert!(
            SetProperty::capture(app.world(), entity, "Nope", "", Box::new(0.0f32)).is_err(),
            "unregistered component"
        );
    }

    #[test]
    fn spawn_despawn_reparent_undo() {
        let mut app = app();
        let parent = spawn_named(app.world_mut(), "Parent");
        let mut undo = UndoStack::new();

        let guid = Guid::new_random();
        let mut spawn =
            SpawnEntities::new_entity(app.world(), guid, "Child", Transform::default()).expect("build");
        spawn.apply(app.world_mut());
        undo.push(Box::new(spawn));
        let child = app
            .world_mut()
            .query::<(Entity, &EntityGuid)>()
            .iter(app.world())
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e)
            .expect("child spawned");
        assert_eq!(
            app.world().get::<ChildOf>(child).map(|c| c.parent()),
            None,
            "spawned as root (the editor parents after spawn)"
        );

        // Parent it, undo restores root.
        let reparent = Reparent::capture(app.world(), child, Some(parent));
        let mut reparent = reparent;
        reparent.apply(app.world_mut());
        undo.push(Box::new(reparent));
        assert_eq!(
            app.world().get::<ChildOf>(child).map(|c| c.parent()),
            Some(parent)
        );
        assert!(undo.undo(app.world_mut()), "undo reparent");
        assert!(app.world().get::<ChildOf>(child).is_none());
        assert!(undo.redo(app.world_mut()), "redo reparent");
        assert_eq!(
            app.world().get::<ChildOf>(child).map(|c| c.parent()),
            Some(parent)
        );

        // Despawn the child, undo brings it back with the same guid.
        let mut despawn = DespawnEntities {
            entities: vec![child.to_bits()],
            snapshot: None,
        };
        despawn.apply(app.world_mut());
        undo.push(Box::new(despawn));
        assert!(app.world().get_entity(child).is_err());
        assert!(undo.undo(app.world_mut()), "undo despawn");
        let restored = app
            .world_mut()
            .query::<(Entity, &EntityGuid)>()
            .iter(app.world())
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e);
        assert!(restored.is_some(), "undo respawned the entity with its guid");
        let _ = restored;

        // Undo everything: reparent undone, spawn undone.
        assert!(undo.undo(app.world_mut()), "undo reparent");
        assert!(undo.undo(app.world_mut()), "undo spawn");
        assert!(
            app.world_mut()
                .query::<(Entity, &EntityGuid)>()
                .iter(app.world())
                .find(|(_, g)| g.0 == guid)
                .is_none(),
            "created entity is gone after full undo"
        );
        assert!(
            app.world().get_entity(parent).is_ok(),
            "parent predates the stack"
        );
    }

    #[test]
    fn add_remove_component_undo() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Blank");
        let mut undo = UndoStack::new();

        let mut add = AddComponent::capture(app.world(), entity, "Marker", Box::new(Marker { value: 2.0 }))
            .expect("capture add");
        add.apply(app.world_mut());
        undo.push(Box::new(add));
        assert_eq!(app.world().get::<Marker>(entity).expect("added").value, 2.0);
        assert!(undo.undo(app.world_mut()));
        assert!(app.world().get::<Marker>(entity).is_none());
        assert!(undo.redo(app.world_mut()));
        assert_eq!(app.world().get::<Marker>(entity).expect("re-added").value, 2.0);

        // Remove with a captured old value, undo restores it.
        let old = app.world().get::<Marker>(entity).cloned();
        let mut remove = RemoveComponent {
            entity: entity.to_bits(),
            component: "Marker".to_string(),
            old: old.map(|m| Box::new(m) as Box<dyn PartialReflect>),
        };
        remove.apply(app.world_mut());
        undo.push(Box::new(remove));
        assert!(app.world().get::<Marker>(entity).is_none());
        assert!(undo.undo(app.world_mut()));
        assert_eq!(app.world().get::<Marker>(entity).expect("restored").value, 2.0);
    }

    #[test]
    fn continuous_edits_coalesce() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Scrub");
        app.world_mut().entity_mut(entity).insert(Marker { value: 0.0 });

        let mut undo = UndoStack::new();
        for value in [0.1f32, 0.2, 0.3, 0.4] {
            let mut command = SetProperty::capture(app.world(), entity, "Marker", "value", Box::new(value))
                .expect("capture");
            command.apply(app.world_mut());
            undo.push(Box::new(command));
        }
        assert_eq!(undo.undo_depth(), 1, "scrubs merged into one entry");
        assert_eq!(app.world().get::<Marker>(entity).expect("m").value, 0.4);
        undo.undo(app.world_mut());
        assert_eq!(
            app.world().get::<Marker>(entity).expect("m").value,
            0.0,
            "back to start"
        );
    }

    /// `specs/19` § 1: the undo stack keeps at most 500 entries.
    #[test]
    fn the_undo_cap_is_the_spec_500_entries() {
        assert_eq!(UNDO_CAP, 500);
    }

    #[test]
    fn stack_is_capped_and_dirty_tracks() {
        let mut app = app();
        let mut undo = UndoStack::new();
        assert!(!undo.is_dirty());
        for _ in 0..UNDO_CAP + 20 {
            undo.push(Box::new(Dummy));
        }
        assert_eq!(undo.undo_depth(), UNDO_CAP);
        assert!(undo.is_dirty());
        undo.mark_saved();
        assert!(!undo.is_dirty());
        // Undoing after mark_saved makes it dirty again.
        assert!(undo.undo(app.world_mut()));
        assert!(undo.is_dirty());
    }

    #[test]
    fn dirty_follows_the_saved_state_not_the_depth() {
        let mut app = app();
        let world = app.world_mut();
        let mut undo = UndoStack::new();
        assert!(!undo.is_dirty(), "a fresh stack is clean");
        undo.push(Box::new(Dummy));
        assert!(undo.is_dirty());
        assert!(undo.undo(world));
        assert!(
            !undo.is_dirty(),
            "undoing the only edit returns to the opened state"
        );
        assert!(undo.redo(world));
        undo.push(Box::new(Dummy));
        undo.mark_saved();
        assert!(!undo.is_dirty());
        assert!(undo.undo(world));
        assert!(undo.is_dirty(), "one step below the save");
        assert!(undo.undo(world));
        assert!(
            undo.is_dirty(),
            "nothing left to undo, but the save was two edits later"
        );
        assert!(undo.redo(world));
        assert!(undo.redo(world));
        assert!(!undo.is_dirty(), "redone back to the saved state");
        assert!(undo.undo(world));
        undo.push(Box::new(Dummy));
        assert!(undo.is_dirty(), "a new branch never reaches the saved state");
        assert!(undo.undo(world));
        assert!(undo.is_dirty(), "the saved state was on the dropped redo branch");
    }

    #[test]
    fn a_save_seals_and_the_cap_moves_the_opened_state() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Scrub");
        app.world_mut().entity_mut(entity).insert(Marker { value: 0.0 });
        fn scrub(world: &mut World, undo: &mut UndoStack, entity: Entity, value: f32) {
            let mut command =
                SetProperty::capture(world, entity, "Marker", "value", Box::new(value)).expect("capture");
            command.apply(world);
            undo.push(Box::new(command));
        }
        let mut undo = UndoStack::new();
        scrub(app.world_mut(), &mut undo, entity, 0.5);
        undo.mark_saved();
        scrub(app.world_mut(), &mut undo, entity, 0.7);
        assert_eq!(undo.undo_depth(), 2, "the scrub after the save is its own entry");
        assert!(undo.is_dirty());
        assert!(undo.undo(app.world_mut()));
        assert!(!undo.is_dirty(), "undo returned exactly to the saved state");
        assert_eq!(app.world().get::<Marker>(entity).expect("m").value, 0.5);

        let mut capped = UndoStack::new();
        for _ in 0..UNDO_CAP + 3 {
            capped.push(Box::new(Dummy));
        }
        while capped.undo(app.world_mut()) {}
        assert!(
            capped.is_dirty(),
            "three edits fell off the bottom: the opened state is out of reach"
        );

        let mut restored = UndoStack::new();
        restored.mark_dirty();
        restored.push(Box::new(Dummy));
        assert!(restored.undo(app.world_mut()));
        assert!(restored.is_dirty(), "a restored autosave stays unsaved");
        restored.mark_saved();
        assert!(!restored.is_dirty());
    }

    #[test]
    fn coalescing_stops_at_seals_undos_and_redos() {
        let mut app = app();
        let a = spawn_named(app.world_mut(), "A");
        let b = spawn_named(app.world_mut(), "B");
        fn x(world: &World, entity: Entity) -> f32 {
            world.get::<Transform>(entity).expect("t").translation.x
        }
        fn drag(world: &mut World, undo: &mut UndoStack, entity: Entity, to: f32) {
            let target = Transform::from_translation(regia_core::math::Vec3::new(to, 0.0, 0.0));
            let mut command = SetTransform::capture(world, entity, target);
            command.apply(world);
            undo.push(Box::new(command));
        }
        let mut undo = UndoStack::new();
        let world = app.world_mut();
        drag(world, &mut undo, a, 1.0);
        drag(world, &mut undo, a, 2.0);
        assert_eq!(undo.undo_depth(), 1, "one drag, one entry");
        undo.seal();
        drag(world, &mut undo, a, 3.0);
        assert_eq!(undo.undo_depth(), 2, "a seal ends the drag");
        drag(world, &mut undo, b, 1.0);
        assert!(undo.undo(world));
        assert_eq!(x(world, b), 0.0);
        drag(world, &mut undo, a, 4.0);
        assert_eq!(
            undo.undo_depth(),
            3,
            "an edit after an undo never merges into the entry below"
        );
        assert!(undo.undo(world));
        assert_eq!(x(world, a), 3.0, "undo stops at the start of the last drag");
        assert!(undo.redo(world));
        drag(world, &mut undo, a, 5.0);
        assert_eq!(undo.undo_depth(), 4, "an edit after a redo is its own entry");
    }

    #[test]
    fn entries_follow_entities_that_undo_spawned_again() {
        let mut app = app();
        let world = app.world_mut();
        let rock = spawn_named(world, "Rock");
        world.entity_mut(rock).insert(Marker { value: 1.0 });
        let guid = guid_of(world, rock);
        let mut undo = UndoStack::new();
        let mut edit =
            SetProperty::capture(world, rock, "Marker", "value", Box::new(2.0f32)).expect("capture");
        edit.apply(world);
        undo.push(Box::new(edit));
        let mut delete = DespawnEntities {
            entities: vec![rock.to_bits()],
            snapshot: None,
        };
        delete.apply(world);
        undo.push(Box::new(delete));
        assert!(by_guid(world, guid).is_none());

        assert!(undo.undo(world), "undo delete");
        let restored = by_guid(world, guid).expect("restored");
        assert_ne!(restored, rock, "a new entity id with the same guid");
        assert_eq!(resolve_entity(world, rock.to_bits()), Some(restored));
        assert!(undo.undo(world), "undo the edit made before the delete");
        assert_eq!(
            world.get::<Marker>(restored).expect("m").value,
            1.0,
            "the older entry reached the restored entity"
        );
        assert!(undo.redo(world));
        assert_eq!(world.get::<Marker>(restored).expect("m").value, 2.0);
        assert!(undo.redo(world), "redo delete");
        assert!(by_guid(world, guid).is_none(), "redo deleted the restored entity");
        assert!(undo.undo(world));
        let again = by_guid(world, guid).expect("restored twice");
        assert_eq!(
            resolve_entity(world, rock.to_bits()),
            Some(again),
            "the first id maps straight to the newest entity"
        );
    }

    #[test]
    fn restored_entities_keep_level_persistence_and_sibling_place() {
        let mut app = app();
        let world = app.world_mut();
        let level = LevelMember::from_level(regia_scene::prelude::LevelId(Guid::new_random()));
        let parent = spawn_named(world, "Parent");
        world.entity_mut(parent).insert(level);
        let kids: Vec<Entity> = ["A", "B", "C"].iter().map(|n| spawn_named(world, n)).collect();
        for kid in &kids {
            world.entity_mut(*kid).insert((ChildOf(parent), level));
        }
        world.entity_mut(kids[1]).insert(Persistent);
        let guid_b = guid_of(world, kids[1]);
        let mut undo = UndoStack::new();

        let mut delete = DespawnEntities {
            entities: vec![kids[1].to_bits()],
            snapshot: None,
        };
        delete.apply(world);
        undo.push(Box::new(delete));
        assert!(undo.undo(world));
        let b = by_guid(world, guid_b).expect("B restored");
        assert_eq!(
            world.get::<LevelMember>(b).copied(),
            Some(level),
            "still in its level, so the next save keeps it"
        );
        assert!(world.get::<Persistent>(b).is_some());
        fn order(world: &World, parent: Entity) -> Vec<String> {
            let Some(children) = world.get::<Children>(parent) else {
                return Vec::new();
            };
            let children: &[Entity] = children;
            children
                .iter()
                .filter_map(|c| world.get::<Name>(*c).map(|n| n.as_str().to_string()))
                .collect()
        }
        assert_eq!(
            order(world, parent),
            ["A", "B", "C"],
            "back in the middle, not appended (B's parent {:?}, name {:?}; the parent's children {:?})",
            world.get::<ChildOf>(b).map(ChildOf::parent),
            world.get::<Name>(b),
            world.get::<Children>(parent).map(|c| c.to_vec())
        );

        // The whole subtree: children come back in their order whatever the snapshot order.
        let mut delete = DespawnEntities {
            entities: vec![parent.to_bits()],
            snapshot: None,
        };
        delete.apply(world);
        undo.push(Box::new(delete));
        assert!(
            by_guid(world, guid_b).is_none(),
            "the subtree went with the parent"
        );
        assert!(undo.undo(world));
        let parent_now = resolve_entity(world, parent.to_bits()).expect("parent restored");
        assert_eq!(order(world, parent_now), ["A", "B", "C"]);
        assert_eq!(world.get::<LevelMember>(parent_now).copied(), Some(level));
        let b = by_guid(world, guid_b).expect("B restored with the parent");
        assert!(world.get::<Persistent>(b).is_some());
    }

    #[test]
    fn spawned_roots_undo_redo_undo_removes_the_instance() {
        let mut app = app();
        let world = app.world_mut();
        let root = spawn_named(world, "Instance");
        let guid = guid_of(world, root);
        let mut undo = UndoStack::new();
        let mut command = SpawnedRoots {
            entities: vec![root.to_bits()],
            snapshot: None,
        };
        command.apply(world);
        undo.push(Box::new(command));
        assert!(undo.undo(world));
        assert!(by_guid(world, guid).is_none());
        assert!(undo.redo(world));
        assert!(by_guid(world, guid).is_some());
        assert!(undo.undo(world));
        assert!(
            by_guid(world, guid).is_none(),
            "the second undo found the re-spawned instance"
        );
    }

    #[test]
    fn a_group_is_one_step_and_its_redo_keeps_the_parent() {
        let mut app = app();
        let world = app.world_mut();
        let parent = spawn_named(world, "Parent");
        let guid = Guid::new_random();
        let mut group = CommandGroup::new("Create child");
        let mut spawn = SpawnEntities::new_entity(world, guid, "Child", Transform::default()).expect("build");
        spawn.apply(world);
        group.push(Box::new(spawn));
        let child = by_guid(world, guid).expect("spawned");
        let mut reparent = Reparent::capture(world, child, Some(parent));
        reparent.apply(world);
        group.push(Box::new(reparent));
        let mut undo = UndoStack::new();
        undo.push(Box::new(group));
        assert_eq!(undo.undo_depth(), 1);
        assert_eq!(undo.next_undo().as_deref(), Some("Create child"));
        assert!(undo.undo(world));
        assert!(by_guid(world, guid).is_none(), "one undo removed the child");
        assert!(undo.redo(world));
        let child = by_guid(world, guid).expect("redone");
        assert_eq!(
            world.get::<ChildOf>(child).map(ChildOf::parent),
            Some(parent),
            "the redone child is parented again"
        );
    }

    #[test]
    fn remove_capture_keeps_array_components_undoable() {
        let mut app = app();
        let world = app.world_mut();
        let lamp = spawn_named(world, "Lamp");
        let tint = Tint { rgb: [0.2, 0.4, 0.6] };
        world.entity_mut(lamp).insert(tint.clone());
        let mut remove = RemoveComponent::capture(world, lamp, "Tint").expect("capture");
        assert!(remove.old.is_some());
        remove.apply(world);
        let mut undo = UndoStack::new();
        undo.push(Box::new(remove));
        assert!(world.get::<Tint>(lamp).is_none());
        assert!(undo.undo(world));
        assert_eq!(world.get::<Tint>(lamp), Some(&tint));
        assert!(
            RemoveComponent::capture(world, lamp, "Marker").is_err(),
            "no Marker to remove"
        );
    }

    /// A command that merges every successor, standing in for a coalescing drag.
    struct Merging;
    impl EditorCommand for Merging {
        fn apply(&mut self, _world: &mut World) {}
        fn revert(&mut self, _world: &mut World) {}
        fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
            true
        }
        fn description(&self) -> String {
            "merging".to_string()
        }
    }

    #[test]
    fn undoing_past_the_saved_state_is_dirty() {
        let mut app = app();
        let mut undo = UndoStack::new();
        undo.push(Box::new(Dummy));
        undo.mark_saved();
        assert!(!undo.is_dirty());
        // The only edit was saved; undoing it leaves the world different from the file.
        assert!(undo.undo(app.world_mut()));
        assert_eq!(undo.undo_depth(), 0);
        assert!(undo.is_dirty(), "the saved edit was undone");
        // Redo returns to the saved state.
        assert!(undo.redo(app.world_mut()));
        assert!(!undo.is_dirty(), "back at the saved state");
    }

    #[test]
    fn undo_back_to_the_saved_state_is_clean() {
        let mut app = app();
        let mut undo = UndoStack::new();
        undo.push(Box::new(Dummy));
        undo.mark_saved();
        undo.push(Box::new(Dummy));
        assert!(undo.is_dirty());
        assert!(undo.undo(app.world_mut()));
        assert!(!undo.is_dirty(), "the world equals the saved file again");
    }

    #[test]
    fn a_fresh_stack_is_clean_after_an_edit_and_its_undo() {
        let mut app = app();
        let mut undo = UndoStack::new();
        assert!(!undo.is_dirty());
        undo.push(Box::new(Dummy));
        assert!(undo.is_dirty());
        assert!(undo.undo(app.world_mut()));
        assert!(!undo.is_dirty());
    }

    #[test]
    fn a_new_edit_after_undo_drops_the_redo_branch_and_the_saved_state_on_it() {
        let mut app = app();
        let mut undo = UndoStack::new();
        undo.push(Box::new(Dummy));
        undo.mark_saved();
        assert!(undo.undo(app.world_mut()));
        assert_eq!(undo.redo_depth(), 1);
        undo.push(Box::new(Dummy));
        assert_eq!(undo.redo_depth(), 0, "a new edit drops the redo branch");
        // The saved state is gone with the branch: undoing to depth 1 must not read clean.
        undo.push(Box::new(Dummy));
        assert!(undo.undo(app.world_mut()));
        assert_eq!(undo.undo_depth(), 1);
        assert!(undo.is_dirty());
    }

    #[test]
    fn an_edit_after_a_save_never_merges_into_the_saved_entry() {
        // ADR-540: a save seals the stack, so a coalescing edit after it is its own entry and
        // undo can return exactly to the saved state (ADR-381's saved-depth fix merged into it).
        let mut app = app();
        let mut undo = UndoStack::new();
        undo.push(Box::new(Merging));
        undo.mark_saved();
        undo.push(Box::new(Merging));
        assert_eq!(undo.undo_depth(), 2, "the edit after the save is its own entry");
        assert!(undo.is_dirty());
        assert!(undo.undo(app.world_mut()));
        assert!(!undo.is_dirty(), "undo is back at the saved state");
        assert!(undo.redo(app.world_mut()));
        assert!(undo.is_dirty());
        // Without a save in between, coalescing edits still merge into one entry.
        undo.push(Box::new(Merging));
        undo.push(Box::new(Merging));
        assert_eq!(undo.undo_depth(), 3);
    }

    #[test]
    fn mark_dirty_survives_undo_and_redo() {
        let mut app = app();
        let mut undo = UndoStack::new();
        undo.push(Box::new(Dummy));
        undo.mark_saved();
        undo.mark_dirty();
        assert!(undo.undo(app.world_mut()));
        assert!(undo.redo(app.world_mut()));
        assert!(
            undo.is_dirty(),
            "a restored autosave never reads clean by undo alone"
        );
    }

    #[test]
    fn undo_and_redo_on_an_empty_stack_do_nothing() {
        let mut app = app();
        let mut undo = UndoStack::new();
        assert!(!undo.undo(app.world_mut()));
        assert!(!undo.redo(app.world_mut()));
        assert_eq!(undo.next_undo(), None);
        assert_eq!(undo.next_redo(), None);
        assert!(!undo.is_dirty());
    }

    /// A no-op command for capacity tests.
    struct Dummy;
    impl EditorCommand for Dummy {
        fn apply(&mut self, _world: &mut World) {}
        fn revert(&mut self, _world: &mut World) {}
        fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
            false
        }
        fn description(&self) -> String {
            "dummy".to_string()
        }
    }

    #[test]
    fn undo_to_empty_after_save_is_dirty() {
        let mut app = app();
        let mut undo = UndoStack::new();
        undo.push(Box::new(Dummy));
        undo.mark_saved();
        assert!(!undo.is_dirty());
        assert!(undo.undo(app.world_mut()));
        assert!(
            undo.is_dirty(),
            "stack is empty but the file still has the command's effect"
        );
        assert!(undo.redo(app.world_mut()));
        assert!(!undo.is_dirty(), "back on the saved depth");
        // A push that drops the saved state's redo branch can never be clean again by undo alone.
        assert!(undo.undo(app.world_mut()));
        undo.push(Box::new(Dummy));
        assert!(undo.undo(app.world_mut()));
        assert!(undo.is_dirty());
        // A never-saved stack is clean again at depth zero.
        let mut fresh = UndoStack::new();
        fresh.push(Box::new(Dummy));
        assert!(fresh.undo(app.world_mut()));
        assert!(!fresh.is_dirty());
    }

    fn test_guid_entity(world: &mut World, guid: Guid) -> Option<Entity> {
        world
            .query::<(Entity, &EntityGuid)>()
            .iter(world)
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e)
    }

    #[test]
    fn despawn_undo_redo_keeps_level_membership_and_tracks_new_ids() {
        use regia_scene::prelude::LevelMember;
        let mut app = app();
        let open = crate::level_io::new_level(app.world_mut());
        let entity = spawn_named(app.world_mut(), "Rock");
        let guid = app.world().get::<EntityGuid>(entity).expect("guid").0;
        app.world_mut()
            .entity_mut(entity)
            .insert(LevelMember::from_level(open.id));

        let mut undo = UndoStack::new();
        let mut despawn = DespawnEntities {
            entities: vec![entity.to_bits()],
            snapshot: None,
        };
        despawn.apply(app.world_mut());
        undo.push(Box::new(despawn));

        assert!(undo.undo(app.world_mut()));
        let restored = test_guid_entity(app.world_mut(), guid).expect("respawned");
        assert_eq!(
            app.world().get::<LevelMember>(restored).map(LevelMember::level),
            Some(open.id),
            "restored entity belongs to the open level, so a save keeps it"
        );
        let saved = regia_scene::level::snapshot_level(app.world_mut(), open.id).expect("snapshot");
        assert!(saved.scene.entities.iter().any(|e| e.guid == guid));

        assert!(undo.redo(app.world_mut()));
        assert!(
            test_guid_entity(app.world_mut(), guid).is_none(),
            "redo removes the respawned copy"
        );
        assert!(undo.undo(app.world_mut()));
        assert!(test_guid_entity(app.world_mut(), guid).is_some());
    }

    #[test]
    fn spawned_roots_redo_undo_removes_the_respawned_copy() {
        let mut app = app();
        let _open = crate::level_io::new_level(app.world_mut());
        let entity = spawn_named(app.world_mut(), "Prefab");
        let guid = app.world().get::<EntityGuid>(entity).expect("guid").0;
        let mut undo = UndoStack::new();
        let mut spawned = SpawnedRoots {
            entities: vec![entity.to_bits()],
            snapshot: None,
        };
        spawned.apply(app.world_mut());
        undo.push(Box::new(spawned));
        assert!(undo.undo(app.world_mut()));
        assert!(test_guid_entity(app.world_mut(), guid).is_none());
        assert!(undo.redo(app.world_mut()));
        assert!(test_guid_entity(app.world_mut(), guid).is_some());
        assert!(undo.undo(app.world_mut()));
        assert!(
            test_guid_entity(app.world_mut(), guid).is_none(),
            "second undo finds the copy"
        );
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    #[derive(Resource, Default)]
    struct Counter {
        value: i32,
        applied: usize,
        reverted: usize,
    }
    struct Step(i32, bool);
    impl EditorCommand for Step {
        fn apply(&mut self, world: &mut World) {
            let mut counter = world.resource_mut::<Counter>();
            counter.value += self.0;
            counter.applied += 1;
        }
        fn revert(&mut self, world: &mut World) {
            let mut counter = world.resource_mut::<Counter>();
            counter.value -= self.0;
            counter.reverted += 1;
        }
        fn merge(&mut self, _: &dyn EditorCommand) -> bool {
            false
        }
        fn changes_level_state(&self) -> bool {
            self.1
        }
        fn description(&self) -> String {
            "Step".into()
        }
    }
    fn step(world: &mut World, value: i32, level: bool) {
        let mut step = Step(value, level);
        step.apply(world);
        world.resource_mut::<UndoStack>().push(Box::new(step));
    }
    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<Counter>();
        world.init_resource::<UndoStack>();
        world
    }

    #[test]
    fn random_commands_full_undo_and_jumps_run_every_revert() {
        let mut world = world();
        let mut rng = regia_core::math::Rng::new(7826);
        let mut expected = 0;
        for _ in 0..400 {
            let value = (rng.next_u32() % 101) as i32 - 50;
            expected += value;
            step(&mut world, value, true);
        }
        assert_eq!(world.resource::<Counter>().value, expected);
        jump_world(&mut world, 0).expect("jump to start");
        assert_eq!(world.resource::<Counter>().value, 0);
        assert_eq!(world.resource::<Counter>().reverted, 400);
        jump_world(&mut world, 400).expect("jump to end");
        assert_eq!(world.resource::<Counter>().value, expected);
        assert_eq!(world.resource::<Counter>().applied, 800);
        assert_eq!(jump_world(&mut world, 401), Err(HistoryError::Index));
        assert_eq!(world.resource::<Counter>().applied, 800);
    }

    #[test]
    fn abort_and_empty_commit_preserve_redo_and_file_groups_stay_clean() {
        let mut world = world();
        step(&mut world, 3, true);
        undo_world(&mut world);
        world
            .resource_mut::<UndoStack>()
            .transaction_begin("Discard")
            .expect("begin");
        step(&mut world, 7, true);
        assert_eq!(jump_world(&mut world, 0), Err(HistoryError::Active));
        let mut stack = world.remove_resource::<UndoStack>().expect("stack");
        stack.transaction_abort(&mut world).expect("abort");
        assert_eq!(stack.redo_depth(), 1);
        assert!(!stack.is_dirty());
        stack.transaction_begin("Empty").expect("begin");
        assert!(!stack.transaction_commit().expect("empty commit"));
        assert_eq!(stack.redo_depth(), 1);
        world.insert_resource(stack);
        UndoStack::transaction(&mut world, "Asset pair", |world| {
            step(world, 2, false);
            step(world, 5, false);
        })
        .expect("Rust transaction");
        assert!(!world.resource::<UndoStack>().is_dirty());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        assert_eq!(world.resource::<UndoStack>().redo_depth(), 0);
        undo_world(&mut world);
        assert_eq!(world.resource::<Counter>().value, 0);
    }
    /// Review sprint-w12-rev-g13 #3: in an agent transaction, a push made outside an agent request
    /// (a gizmo drag between two script calls) is its own entry: the abort leaves it applied and
    /// the commit does not fold it into the agent's group.
    #[test]
    fn a_user_edit_during_an_agent_transaction_stays_out_of_it() {
        let mut world = world();
        let agent = |world: &mut World, value: i32| {
            world.resource_mut::<UndoStack>().set_agent_request(true);
            step(world, value, true);
            world.resource_mut::<UndoStack>().set_agent_request(false);
        };
        world
            .resource_mut::<UndoStack>()
            .transaction_begin_agent("Script")
            .expect("begin");
        agent(&mut world, 3);
        step(&mut world, 10, true);
        let mut stack = world.remove_resource::<UndoStack>().expect("stack");
        stack.transaction_abort(&mut world).expect("abort");
        world.insert_resource(stack);
        assert_eq!(
            world.resource::<Counter>().value,
            10,
            "the user's edit is not reverted"
        );
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "and it is one undo step"
        );
        world
            .resource_mut::<UndoStack>()
            .transaction_begin_agent("Script")
            .expect("begin");
        agent(&mut world, 5);
        step(&mut world, 20, true);
        agent(&mut world, 7);
        assert!(
            world
                .resource_mut::<UndoStack>()
                .transaction_commit()
                .expect("commit")
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 3);
        assert!(world.resource::<UndoStack>().is_dirty());
        undo_world(&mut world);
        assert_eq!(
            world.resource::<Counter>().value,
            30,
            "the agent's group: 5 and 7"
        );
        undo_world(&mut world);
        assert_eq!(world.resource::<Counter>().value, 10, "then the user's own step");
        // A plain (Rust or extension) transaction still takes every push.
        world
            .resource_mut::<UndoStack>()
            .transaction_begin("Plain")
            .expect("begin");
        step(&mut world, 1, true);
        step(&mut world, 1, true);
        assert!(
            world
                .resource_mut::<UndoStack>()
                .transaction_commit()
                .expect("commit")
        );
        undo_world(&mut world);
        assert_eq!(world.resource::<Counter>().value, 10);
    }

    #[test]
    fn transaction_save_baseline_and_panic_keep_applied_commands_recoverable() {
        let mut world = world();
        world
            .resource_mut::<UndoStack>()
            .transaction_begin("Save inside")
            .expect("begin");
        step(&mut world, 4, true);
        world.resource_mut::<UndoStack>().mark_saved();
        assert!(!world.resource::<UndoStack>().is_dirty());
        world
            .resource_mut::<UndoStack>()
            .transaction_commit()
            .expect("commit");
        assert!(!world.resource::<UndoStack>().is_dirty());
        undo_world(&mut world);
        assert!(world.resource::<UndoStack>().is_dirty());
        redo_world(&mut world);
        assert!(!world.resource::<UndoStack>().is_dirty());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = UndoStack::transaction(&mut world, "Interrupted extension", |world| {
                step(world, 9, true);
                panic!("extension interrupted");
            });
        }));
        assert!(panic.is_err());
        assert!(world.resource::<UndoStack>().transaction_label().is_none());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
        undo_world(&mut world);
        assert_eq!(world.resource::<Counter>().value, 4);
    }
    #[test]
    fn repeated_panel_passes_do_not_repeat_history_navigation() {
        let mut world = world();
        for _ in 0..3 {
            step(&mut world, 1, true);
        }
        assert!(navigate_world_once(&mut world, 10, false));
        assert!(!navigate_world_once(&mut world, 10, false));
        assert_eq!(world.resource::<Counter>().value, 2);
        assert!(navigate_world_once(&mut world, 11, false));
        assert_eq!(world.resource::<Counter>().value, 1);
    }
    #[test]
    fn attaching_a_file_command_to_a_saved_scene_entry_keeps_the_level_clean() {
        let mut world = world();
        step(&mut world, 2, true);
        world.resource_mut::<UndoStack>().mark_saved();
        let mut file = Step(3, false);
        file.apply(&mut world);
        world.resource_mut::<UndoStack>().join_top(Box::new(file));
        assert!(!world.resource::<UndoStack>().is_dirty());
        undo_world(&mut world);
        assert!(world.resource::<UndoStack>().is_dirty());
        assert_eq!(world.resource::<Counter>().value, 0);
        redo_world(&mut world);
        assert!(!world.resource::<UndoStack>().is_dirty());
    }
}
