// SPDX-License-Identifier: MPL-2.0
//! Stable entity references in save games (ADR-3239, `docs/specs/91-character-save-templates.md` § 10).
//!
//! An `Entity` is an index into one world: written raw, it means nothing after a load. Save
//! format 2 gives every saved entity a **save id**, its position in [`SaveFileRaw::entities`], and
//! keeps the table [`SaveFileRaw::entity_ids`]: one [`SaveEntityId`] per saved entity, then one per
//! *external* entity (a level entity a saved component points at without being saved itself).
//!
//! # Saving
//!
//! - Saved entities are ordered so the file is deterministic: entities with an `EntityGuid` first,
//!   by `(level, guid)` (`LevelMember`'s level, `0` without one), then runtime entities by entity
//!   index and generation. Externals follow, sorted by `(level, guid)`.
//! - A component type *holds references* when reflection reaches an `Entity` through its fields
//!   (structs, tuples, `Option` and other enums, lists, arrays, map values). Such a type must
//!   register `ReflectMapEntities` (`#[reflect(Component, MapEntities)]`); bevy's `ChildOf` is
//!   accepted as is and `Children` is never saved (each child's `ChildOf` rebuilds it). Without it,
//!   or with an `Entity` in a map key or set element, the component is not saved and the note
//!   [`SAVE_ENTITY_REF_UNMAPPED`] says so.
//! - Every reachable `Entity` is written as a marker entity whose index is the target's save id
//!   (generation 1). `Entity::PLACEHOLDER` stays as is ("no entity").
//! - A target that is neither saved nor a level entity becomes `None` in the innermost `Option`
//!   around it ([`SAVE_ENTITY_REF_CLEARED`]); with no `Option` the component is not saved
//!   ([`SAVE_ENTITY_REF_UNSAVED`], with the entity, component and field path).
//!
//! # Loading
//!
//! Every reference is checked against the table before the world changes (an id past the table is
//! [`SAVE_ENTITY_REF_OUT_OF_RANGE`] and refuses the load). Then every saved entity is spawned,
//! externals resolve by guid and level against the live world, and a second pass remaps each
//! reference. A missing target becomes `None` in an `Option` ([`SAVE_ENTITY_REF_CLEARED`]) or skips
//! the component ([`SAVE_ENTITY_REF_MISSING`]): nothing is inserted that points at a dead or
//! unrelated entity.
//!
//! # Format 1
//!
//! An empty table means the references were never converted (a format 1 file, or one
//! `regia upgrade-project` rewrote without the game's types). The loader converts them with the
//! app's registry: every non-placeholder `Entity` becomes [`SaveEntityId::Lost`]. A file without
//! references converts to nothing and loads unchanged.
//!
//! The notes of the last save or load are in [`SaveEntityRefReport`].

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use regia_app::prelude::*;
use regia_ecs::ecs::entity::EntityGeneration;
use regia_ecs::ecs::hierarchy::{ChildOf, Children};
use regia_ecs::reflect::enums::{DynamicEnum, DynamicVariant, VariantInfo};
use regia_ecs::reflect::{ReflectFromReflect, ReflectMapEntities, ReflectMut, TypeInfo, TypeRegistry};
use regia_ecs::serialize::ReflectCodec;
use regia_scene::components::EntityGuid;
use regia_scene::level::LevelMember;
use serde::{Deserialize, Serialize};

use crate::save_game::{SAVE_MAGIC, SaveError, SaveFileRaw, SaveSpec, Saveable, SavedEntity, SavedResource};

/// A saved component points at an entity that is not saved and is not a level entity; the
/// component is not saved (a plain `Entity` field). Warning.
pub const SAVE_ENTITY_REF_UNSAVED: &str = "save_entity_ref_unsaved";
/// A reference's target is missing at load (a level entity a content update removed, or a
/// reference a format 1 save could not keep); the component is skipped. Warning.
pub const SAVE_ENTITY_REF_MISSING: &str = "save_entity_ref_missing";
/// A reference inside an `Option` had no target and was set to `None` (save or load). Debug.
pub const SAVE_ENTITY_REF_CLEARED: &str = "save_entity_ref_cleared";
/// A component type holds an `Entity` but does not register `ReflectMapEntities`, or holds one as
/// a map key or set element; the component is not saved (or not loaded). Warning.
pub const SAVE_ENTITY_REF_UNMAPPED: &str = "save_entity_ref_unmapped";
/// A save holds a reference past its id table (a damaged or hostile file); the load is refused.
pub const SAVE_ENTITY_REF_OUT_OF_RANGE: &str = "save_entity_ref_out_of_range";
/// A save's id table is malformed (shorter than its entity list, or a runtime id among the
/// externals); the load is refused.
pub const SAVE_ENTITY_IDS_INVALID: &str = "save_entity_ids_invalid";

/// The stable identity of an entity in a save file (one [`SaveFileRaw::entity_ids`] entry).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SaveEntityId {
    /// A runtime-spawned saved entity: its ordinal among the runtime entities of the file.
    Runtime(u32),
    /// A level entity: its `EntityGuid` and the level of its `LevelMember` (`0` without one).
    Level {
        /// The `EntityGuid` as a `u128`.
        guid: u128,
        /// The `LevelMember` level id as a `u128`, `0` when the entity has none.
        level: u128,
    },
    /// A reference that could not be kept (converted from format 1): loads as missing.
    Lost,
}

impl SaveEntityId {
    /// Text form for logs and the agent protocol: `runtime:3`, `level:<32 hex digits>` or `lost`.
    pub fn key(&self) -> String {
        match self {
            Self::Runtime(n) => format!("runtime:{n}"),
            Self::Level { guid, .. } => format!("level:{guid:032x}"),
            Self::Lost => "lost".to_string(),
        }
    }
}

/// Which operation wrote a [`SaveEntityRefReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveEntityRefOp {
    /// A save (or `SaveGame::encode`).
    Save,
    /// A load (or `SaveGame::decode`).
    Load,
}

impl SaveEntityRefOp {
    /// `save` or `load`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::Load => "load",
        }
    }
}

/// One thing the reference pass reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveEntityRefNote {
    /// One of the `SAVE_ENTITY_REF_*` codes.
    pub code: &'static str,
    /// The save id of the entity whose component it concerns.
    pub entity: u32,
    /// The component's short type path.
    pub component: String,
    /// The field path inside the component (`target`, `riders[1]`, `0`); empty for the whole value.
    pub field: String,
}

/// The notes of the last save or load (spec 91 § 10.3). Inserted by every save and load.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct SaveEntityRefReport {
    /// The operation that wrote it.
    pub op: SaveEntityRefOp,
    /// Its notes, in entity order.
    pub notes: Vec<SaveEntityRefNote>,
}

/// One reference found by [`inspect_refs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedEntityRef {
    /// The component's short type path.
    pub component: String,
    /// The field path inside the component.
    pub field: String,
    /// The target's save id; `None` when the payload's value is not a well-formed marker.
    pub target: Option<u32>,
}

/// One saved entity as [`inspect_refs`] sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedEntityRefs {
    /// The save id (position in the file).
    pub id: u32,
    /// Its table entry (`None` in a file without a table).
    pub key: Option<SaveEntityId>,
    /// Its component short type paths.
    pub components: Vec<String>,
    /// Every reference its components hold.
    pub refs: Vec<SavedEntityRef>,
}

// ---------------------------------------------------------------------------
// Type scan
// ---------------------------------------------------------------------------

/// What reflection reaches inside a type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Scan {
    /// An `Entity` reachable through fields the walk can rewrite.
    refs: bool,
    /// An `Entity` in a map key or set element (cannot be rewritten in place).
    unsupported: bool,
}

impl Scan {
    fn any(self) -> bool {
        self.refs || self.unsupported
    }

    fn merge(&mut self, other: Scan) {
        self.refs |= other.refs;
        self.unsupported |= other.unsupported;
    }
}

/// Per-call cache of component type scans.
#[derive(Default)]
struct Scans(HashMap<TypeId, Scan>);

impl Scans {
    fn get(&mut self, registry: &TypeRegistry, type_id: TypeId) -> Scan {
        *self
            .0
            .entry(type_id)
            .or_insert_with(|| scan_type(registry, type_id, None, &mut HashSet::new()))
    }
}

/// Whether `type_id` reaches an `Entity`. A type already being scanned (a recursive type) adds
/// nothing the outer scan does not already see.
fn scan_type(
    registry: &TypeRegistry,
    type_id: TypeId,
    info: Option<&'static TypeInfo>,
    visiting: &mut HashSet<TypeId>,
) -> Scan {
    if type_id == TypeId::of::<Entity>() {
        return Scan {
            refs: true,
            unsupported: false,
        };
    }
    if !visiting.insert(type_id) {
        return Scan::default();
    }
    let mut out = Scan::default();
    if let Some(info) = info.or_else(|| registry.get_type_info(type_id)) {
        match info {
            TypeInfo::Struct(s) => {
                for f in s.iter() {
                    out.merge(scan_type(registry, f.type_id(), f.type_info(), visiting));
                }
            }
            TypeInfo::TupleStruct(s) => {
                for f in s.iter() {
                    out.merge(scan_type(registry, f.type_id(), f.type_info(), visiting));
                }
            }
            TypeInfo::Tuple(t) => {
                for f in t.iter() {
                    out.merge(scan_type(registry, f.type_id(), f.type_info(), visiting));
                }
            }
            TypeInfo::List(l) => out.merge(scan_type(registry, l.item_ty().id(), l.item_info(), visiting)),
            TypeInfo::Array(a) => out.merge(scan_type(registry, a.item_ty().id(), a.item_info(), visiting)),
            TypeInfo::Map(m) => {
                if scan_type(registry, m.key_ty().id(), m.key_info(), visiting).any() {
                    out.unsupported = true;
                }
                out.merge(scan_type(registry, m.value_ty().id(), m.value_info(), visiting));
            }
            TypeInfo::Set(s) => {
                if scan_type(registry, s.value_ty().id(), None, visiting).any() {
                    out.unsupported = true;
                }
            }
            TypeInfo::Enum(e) => {
                for variant in e.iter() {
                    match variant {
                        VariantInfo::Struct(v) => {
                            for f in v.iter() {
                                out.merge(scan_type(registry, f.type_id(), f.type_info(), visiting));
                            }
                        }
                        VariantInfo::Tuple(v) => {
                            for f in v.iter() {
                                out.merge(scan_type(registry, f.type_id(), f.type_info(), visiting));
                            }
                        }
                        VariantInfo::Unit(_) => {}
                    }
                }
            }
            TypeInfo::Opaque(_) => {}
        }
    }
    visiting.remove(&type_id);
    out
}

// ---------------------------------------------------------------------------
// Walk
// ---------------------------------------------------------------------------

/// What a reference becomes.
enum Leaf {
    /// Write this entity.
    To(Entity),
    /// No target.
    Missing,
}

/// Visits every `Entity` reflection reaches in a value and rewrites it through `map`.
struct Walk<'f> {
    /// Called with each non-placeholder entity and its field path.
    map: &'f mut dyn FnMut(Entity, &str) -> Leaf,
    /// Field paths of `Option`s set to `None` because their target was missing.
    cleared: Vec<String>,
}

fn join(path: &str, part: &str) -> String {
    if path.is_empty() {
        part.to_string()
    } else {
        format!("{path}.{part}")
    }
}

fn is_option(value: &dyn PartialReflect) -> bool {
    let path = value
        .get_represented_type_info()
        .map_or_else(|| value.reflect_type_path(), |info| info.type_path());
    path.starts_with("core::option::Option<")
}

impl Walk<'_> {
    /// `Err(path)` names a plain reference without a target.
    fn value(&mut self, value: &mut dyn PartialReflect, path: &str) -> Result<(), String> {
        if is_option(value) {
            let some = match value.reflect_mut() {
                ReflectMut::Enum(e) if e.variant_name() == "Some" => match e.field_at_mut(0) {
                    Some(inner) => self.value(inner, path),
                    None => Ok(()),
                },
                _ => Ok(()),
            };
            if let Err(missing) = some {
                value
                    .try_apply(&DynamicEnum::new("None", DynamicVariant::Unit))
                    .map_err(|_| missing.clone())?;
                self.cleared.push(missing);
            }
            return Ok(());
        }
        match value.reflect_mut() {
            ReflectMut::Opaque(v) => {
                if let Some(entity) = v.try_downcast_mut::<Entity>()
                    && *entity != Entity::PLACEHOLDER
                {
                    match (self.map)(*entity, path) {
                        Leaf::To(to) => *entity = to,
                        Leaf::Missing => return Err(path.to_string()),
                    }
                }
                Ok(())
            }
            ReflectMut::Struct(s) => {
                for i in 0..s.field_len() {
                    let name = s.name_at(i).map_or_else(|| i.to_string(), str::to_string);
                    if let Some(field) = s.field_at_mut(i) {
                        self.value(field, &join(path, &name))?;
                    }
                }
                Ok(())
            }
            ReflectMut::TupleStruct(s) => {
                for i in 0..s.field_len() {
                    if let Some(field) = s.field_mut(i) {
                        self.value(field, &join(path, &i.to_string()))?;
                    }
                }
                Ok(())
            }
            ReflectMut::Tuple(t) => {
                for i in 0..t.field_len() {
                    if let Some(field) = t.field_mut(i) {
                        self.value(field, &join(path, &i.to_string()))?;
                    }
                }
                Ok(())
            }
            ReflectMut::List(l) => {
                for i in 0..l.len() {
                    if let Some(item) = l.get_mut(i) {
                        self.value(item, &format!("{path}[{i}]"))?;
                    }
                }
                Ok(())
            }
            ReflectMut::Array(a) => {
                for i in 0..a.len() {
                    if let Some(item) = a.get_mut(i) {
                        self.value(item, &format!("{path}[{i}]"))?;
                    }
                }
                Ok(())
            }
            ReflectMut::Map(m) => {
                let mut failed = None;
                let item = format!("{path}[..]");
                m.retain(&mut |_, v| {
                    if failed.is_none()
                        && let Err(p) = self.value(v, &item)
                    {
                        failed = Some(p);
                    }
                    true
                });
                failed.map_or(Ok(()), Err)
            }
            ReflectMut::Set(_) => Ok(()),
            ReflectMut::Enum(e) => {
                let variant = e.variant_name().to_string();
                for i in 0..e.field_len() {
                    let name = e.name_at(i).map_or_else(|| i.to_string(), str::to_string);
                    if let Some(field) = e.field_at_mut(i) {
                        self.value(field, &join(path, &format!("{variant}.{name}")))?;
                    }
                }
                Ok(())
            }
        }
    }
}

/// Walks `value` with `map`. Returns the paths of `Option`s cleared, or the path of a plain
/// reference without a target.
fn walk(
    value: &mut dyn PartialReflect,
    map: &mut dyn FnMut(Entity, &str) -> Leaf,
) -> Result<Vec<String>, String> {
    let mut walker = Walk {
        map,
        cleared: Vec::new(),
    };
    walker.value(value, "")?;
    Ok(walker.cleared)
}

/// The marker entity a payload holds for save id `id` (`None` past the encodable range).
fn marker(id: usize) -> Option<Entity> {
    let id = u32::try_from(id).ok()?;
    // Index `u32::MAX - 1` is the placeholder's; never hand it out as a save id.
    if id >= u32::MAX - 1 {
        return None;
    }
    Entity::from_raw_u32(id)
}

/// The save id a payload's marker entity names, if it is a well-formed marker.
fn marker_id(entity: Entity) -> Option<usize> {
    (entity.generation() == EntityGeneration::FIRST).then(|| entity.index_u32() as usize)
}

/// Spawn-slot order of an entity: index, then generation.
fn spawn_slot(entity: Entity) -> (u32, u32) {
    (entity.index_u32(), entity.generation().to_bits())
}

/// Whether the type's references can be written: it registers `ReflectMapEntities` or is `ChildOf`.
fn mappable(registry: &TypeRegistry, type_id: TypeId) -> bool {
    type_id == TypeId::of::<ChildOf>()
        || registry
            .get(type_id)
            .is_some_and(|registration| registration.data::<ReflectMapEntities>().is_some())
}

fn note(
    notes: &mut Vec<SaveEntityRefNote>,
    op: SaveEntityRefOp,
    code: &'static str,
    entity: usize,
    component: &str,
    field: &str,
) {
    let entity = u32::try_from(entity).unwrap_or(u32::MAX);
    if code == SAVE_ENTITY_REF_CLEARED {
        tracing::debug!(
            target: "regia_framework",
            "{code}: {} of saved entity {entity}: {component}.{field} set to None",
            op.as_str()
        );
    } else {
        tracing::warn!(
            target: "regia_framework",
            "{code}: {} of saved entity {entity}: component {component}, field {field:?}",
            op.as_str()
        );
    }
    notes.push(SaveEntityRefNote {
        code,
        entity,
        component: component.to_string(),
        field: field.to_string(),
    });
}

// ---------------------------------------------------------------------------
// Save
// ---------------------------------------------------------------------------

/// The entity part of a save: the entities in file order and their id table.
pub(crate) struct SavedEntities {
    pub(crate) entities: Vec<SavedEntity>,
    pub(crate) entity_ids: Vec<SaveEntityId>,
}

/// One component waiting for the reference pass.
struct Pending {
    entity: usize,
    short_path: String,
    value: Box<dyn PartialReflect>,
}

/// Collects the [`Saveable`] entities in save-id order and writes their components, references
/// as save ids. Inserts the [`SaveEntityRefReport`].
#[tracing::instrument(skip_all, target = "regia_framework", name = "save_entity_refs_write")]
pub(crate) fn serialize_entities(
    world: &mut World,
    registry: &TypeRegistry,
    spec: &SaveSpec,
) -> SavedEntities {
    let op = SaveEntityRefOp::Save;
    let mut notes = Vec::new();
    let mut scans = Scans::default();

    // Candidates: every saveable entity with at least one registered component to write.
    let mut level: Vec<((u128, u128), Entity)> = Vec::new();
    let mut runtime: Vec<Entity> = Vec::new();
    let mut candidates: HashMap<Entity, Vec<(TypeId, String)>> = HashMap::new();
    let mut query =
        world.query_filtered::<(Entity, Option<&EntityGuid>, Option<&LevelMember>), With<Saveable>>();
    for (entity, guid, member) in query.iter(world) {
        let Ok(infos) = world.inspect_entity(entity) else {
            continue;
        };
        let mut types = Vec::new();
        for (_, info) in infos {
            let Some(type_id) = info.type_id() else {
                continue;
            };
            if type_id == TypeId::of::<Saveable>() || type_id == TypeId::of::<Children>() {
                continue;
            }
            let Some(reg) = registry.get(type_id) else {
                continue;
            };
            let short_path = reg.type_info().type_path_table().short_path().to_string();
            if short_path.is_empty() {
                continue;
            }
            if !spec.component_filter.is_empty()
                && !spec
                    .component_filter
                    .iter()
                    .any(|f| short_path.contains(f.as_str()))
            {
                continue;
            }
            types.push((type_id, short_path));
        }
        if types.is_empty() {
            continue;
        }
        candidates.insert(entity, types);
        match guid {
            Some(guid) => level.push(((member.map_or(0, |m| m.0), guid.0.0), entity)),
            None => runtime.push(entity),
        }
    }
    let spawn_order = |e: &Entity| spawn_slot(*e);
    level.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| spawn_order(&a.1).cmp(&spawn_order(&b.1)))
    });
    runtime.sort_by_key(spawn_order);

    let mut order: Vec<Entity> = Vec::with_capacity(level.len() + runtime.len());
    let mut entity_ids: Vec<SaveEntityId> = Vec::with_capacity(order.capacity());
    for ((level_id, guid), entity) in &level {
        order.push(*entity);
        entity_ids.push(SaveEntityId::Level {
            guid: *guid,
            level: *level_id,
        });
    }
    for (n, entity) in runtime.iter().enumerate() {
        order.push(*entity);
        entity_ids.push(SaveEntityId::Runtime(u32::try_from(n).unwrap_or(u32::MAX)));
    }
    let position: HashMap<Entity, usize> = order.iter().enumerate().map(|(i, e)| (*e, i)).collect();

    // Components: plain ones encode now; ones holding references wait for the id table.
    let mut codec = ReflectCodec::new(registry);
    let mut entities: Vec<BTreeMap<String, Vec<u8>>> = vec![BTreeMap::new(); order.len()];
    let mut pending: Vec<Pending> = Vec::new();
    for (index, entity) in order.iter().enumerate() {
        let Some(types) = candidates.remove(entity) else {
            continue;
        };
        for (type_id, short_path) in types {
            let Ok(reflected) = world.get_reflect(*entity, type_id) else {
                continue;
            };
            let scan = scans.get(registry, type_id);
            if !scan.any() {
                match codec.to_postcard_typed(reflected.as_partial_reflect()) {
                    Ok(bytes) => {
                        entities[index].insert(short_path, bytes);
                    }
                    Err(e) => tracing::warn!(
                        target: "regia_framework",
                        "cannot serialize component {short_path} on entity {}: {e}",
                        entity.to_bits()
                    ),
                }
                continue;
            }
            if scan.unsupported || !mappable(registry, type_id) {
                note(&mut notes, op, SAVE_ENTITY_REF_UNMAPPED, index, &short_path, "");
                continue;
            }
            // A copy to rewrite: `reflect_clone`, else the type's `FromReflect`.
            let copy = reflected.reflect_clone().ok().or_else(|| {
                registry
                    .get_type_data::<ReflectFromReflect>(type_id)
                    .and_then(|from| from.from_reflect(reflected.as_partial_reflect()))
            });
            match copy {
                Some(value) => pending.push(Pending {
                    entity: index,
                    short_path,
                    value: value.into_partial_reflect(),
                }),
                None => tracing::warn!(
                    target: "regia_framework",
                    "cannot copy component {short_path} on entity {} to rewrite its entity references",
                    entity.to_bits()
                ),
            }
        }
    }

    // Externals: level entities the pending components point at, sorted by (level, guid).
    let mut external_keys: HashMap<Entity, (u128, u128)> = HashMap::new();
    for item in &mut pending {
        let _ = walk(item.value.as_mut(), &mut |target, _| {
            if !position.contains_key(&target)
                && !external_keys.contains_key(&target)
                && let Ok(entity_ref) = world.get_entity(target)
                && let Some(guid) = entity_ref.get::<EntityGuid>()
            {
                let level_id = entity_ref.get::<LevelMember>().map_or(0, |m| m.0);
                external_keys.insert(target, (level_id, guid.0.0));
            }
            Leaf::To(target)
        });
    }
    let sorted: BTreeSet<(u128, u128)> = external_keys.values().copied().collect();
    let mut external_index: HashMap<(u128, u128), usize> = HashMap::new();
    for (level_id, guid) in sorted {
        external_index.insert((level_id, guid), entity_ids.len());
        entity_ids.push(SaveEntityId::Level {
            guid,
            level: level_id,
        });
    }

    // Rewrite every reference as a marker and encode.
    for mut item in pending {
        let result = walk(item.value.as_mut(), &mut |target, _| {
            let id = position.get(&target).copied().or_else(|| {
                external_keys
                    .get(&target)
                    .and_then(|key| external_index.get(key).copied())
            });
            match id.and_then(marker) {
                Some(m) => Leaf::To(m),
                None => Leaf::Missing,
            }
        });
        match result {
            Err(field) => note(
                &mut notes,
                op,
                SAVE_ENTITY_REF_UNSAVED,
                item.entity,
                &item.short_path,
                &field,
            ),
            Ok(cleared) => {
                for field in cleared {
                    note(
                        &mut notes,
                        op,
                        SAVE_ENTITY_REF_CLEARED,
                        item.entity,
                        &item.short_path,
                        &field,
                    );
                }
                match codec.to_postcard_typed(item.value.as_ref()) {
                    Ok(bytes) => {
                        entities[item.entity].insert(item.short_path, bytes);
                    }
                    Err(e) => tracing::warn!(
                        target: "regia_framework",
                        "cannot serialize component {} on saved entity {}: {e}",
                        item.short_path,
                        item.entity
                    ),
                }
            }
        }
    }
    notes.sort_by_key(|n| n.entity);
    world.insert_resource(SaveEntityRefReport { op, notes });
    SavedEntities {
        entities: entities
            .into_iter()
            .map(|components| SavedEntity { components })
            .collect(),
        entity_ids,
    }
}

// ---------------------------------------------------------------------------
// Load
// ---------------------------------------------------------------------------

/// The checked id table of a save being loaded (spec 91 § 10.3).
pub(crate) struct LoadRefs {
    entity_ids: Vec<SaveEntityId>,
    scans: Scans,
}

/// The type id of a decoded value.
fn value_type(value: &dyn Reflect) -> TypeId {
    value
        .get_represented_type_info()
        .map_or_else(|| value.as_any().type_id(), TypeInfo::type_id)
}

impl LoadRefs {
    /// Checks the table and every reference in the decoded components before the world changes.
    pub(crate) fn check<T>(
        raw: &SaveFileRaw,
        staged: &mut [Vec<(T, Box<dyn Reflect>)>],
        registry: &TypeRegistry,
    ) -> Result<Self, SaveError> {
        let ids = &raw.entity_ids;
        let n = raw.entities.len();
        if !ids.is_empty() && ids.len() < n {
            return Err(SaveError::Corrupt(format!(
                "{SAVE_ENTITY_IDS_INVALID}: the id table has {} entries for {n} saved entities",
                ids.len()
            )));
        }
        if let Some(bad) = ids
            .iter()
            .skip(n)
            .position(|id| matches!(id, SaveEntityId::Runtime(_)))
        {
            return Err(SaveError::Corrupt(format!(
                "{SAVE_ENTITY_IDS_INVALID}: external entry {} is a runtime id",
                n + bad
            )));
        }
        let mut scans = Scans::default();
        for (entity, components) in staged.iter_mut().enumerate() {
            for (_, value) in components.iter_mut() {
                if !scans.get(registry, value_type(value.as_ref())).refs {
                    continue;
                }
                let component = value.reflect_short_type_path().to_string();
                let mut bad: Option<String> = None;
                let _ = walk(value.as_partial_reflect_mut(), &mut |target, field| {
                    if bad.is_none() && !marker_id(target).is_some_and(|id| id < ids.len()) {
                        bad = Some(format!(
                            "{SAVE_ENTITY_REF_OUT_OF_RANGE}: saved entity {entity}, component {component}, field {field:?} \
                             holds entity index {} (generation {:?}), the id table has {} entries",
                            target.index_u32(),
                            target.generation(),
                            ids.len()
                        ));
                    }
                    Leaf::To(target)
                });
                if let Some(message) = bad {
                    return Err(SaveError::Corrupt(message));
                }
            }
        }
        Ok(Self {
            entity_ids: ids.clone(),
            scans,
        })
    }

    /// Resolves externals against the live world and remaps every reference to the `spawned`
    /// entities (one per saved entity, in file order). Components that cannot keep a reference are
    /// removed from `staged`. Inserts the [`SaveEntityRefReport`].
    #[tracing::instrument(skip_all, target = "regia_framework", name = "save_entity_refs_remap")]
    pub(crate) fn remap<T>(
        mut self,
        world: &mut World,
        spawned: &[Entity],
        staged: &mut [Vec<(T, Box<dyn Reflect>)>],
        registry: &TypeRegistry,
    ) {
        let op = SaveEntityRefOp::Load;
        let mut notes = Vec::new();
        let n = spawned.len();

        // Level entities of the live world by (level, guid); the lowest entity wins a tie.
        let mut live: HashMap<(u128, u128), Entity> = HashMap::new();
        if self.entity_ids.len() > n {
            let fresh: HashSet<Entity> = spawned.iter().copied().collect();
            let mut query = world.query::<(Entity, &EntityGuid, Option<&LevelMember>)>();
            for (entity, guid, member) in query.iter(world) {
                if fresh.contains(&entity) {
                    continue;
                }
                let key = (member.map_or(0, |m| m.0), guid.0.0);
                live.entry(key)
                    .and_modify(|kept| {
                        if spawn_slot(entity) < spawn_slot(*kept) {
                            *kept = entity;
                        }
                    })
                    .or_insert(entity);
            }
        }
        let targets: Vec<Option<Entity>> = self
            .entity_ids
            .iter()
            .enumerate()
            .map(|(i, id)| match id {
                _ if i < n => spawned.get(i).copied(),
                SaveEntityId::Level { guid, level } => live.get(&(*level, *guid)).copied(),
                SaveEntityId::Runtime(_) | SaveEntityId::Lost => None,
            })
            .collect();

        for (entity, components) in staged.iter_mut().enumerate() {
            components.retain_mut(|(_, value)| {
                let scan = self.scans.get(registry, value_type(value.as_ref()));
                if !scan.any() {
                    return true;
                }
                let component = value.reflect_short_type_path().to_string();
                if scan.unsupported {
                    note(&mut notes, op, SAVE_ENTITY_REF_UNMAPPED, entity, &component, "");
                    return false;
                }
                let result = walk(
                    value.as_partial_reflect_mut(),
                    &mut |target, _| match marker_id(target).and_then(|id| targets.get(id).copied().flatten())
                    {
                        Some(to) => Leaf::To(to),
                        None => Leaf::Missing,
                    },
                );
                match result {
                    Ok(cleared) => {
                        for field in cleared {
                            note(
                                &mut notes,
                                op,
                                SAVE_ENTITY_REF_CLEARED,
                                entity,
                                &component,
                                &field,
                            );
                        }
                        true
                    }
                    Err(field) => {
                        note(
                            &mut notes,
                            op,
                            SAVE_ENTITY_REF_MISSING,
                            entity,
                            &component,
                            &field,
                        );
                        false
                    }
                }
            });
        }
        world.insert_resource(SaveEntityRefReport { op, notes });
    }
}

// ---------------------------------------------------------------------------
// Format 1
// ---------------------------------------------------------------------------

/// Format 1 payload layout: [`SaveFileRaw`] without `entity_ids`.
#[derive(Serialize, Deserialize)]
struct SaveFileRawV1 {
    version: u32,
    game_version: String,
    engine_version: String,
    timestamp_secs: f64,
    thumbnail: Option<Vec<u8>>,
    entities: Vec<SavedEntity>,
    resources: Vec<SavedResource>,
}

/// Borrowed format 1 layout, for writing.
#[derive(Serialize)]
struct SaveFileRawV1Ref<'a> {
    version: u32,
    game_version: &'a str,
    engine_version: &'a str,
    timestamp_secs: f64,
    thumbnail: &'a Option<Vec<u8>>,
    entities: &'a [SavedEntity],
    resources: &'a [SavedResource],
}

/// The first format whose payload carries `entity_ids`.
pub(crate) const ENTITY_IDS_SINCE: u32 = 2;

/// Decodes a payload written in format `version` (format 1 has no `entity_ids`).
pub(crate) fn decode_payload(version: u32, payload: &[u8]) -> Result<SaveFileRaw, postcard::Error> {
    if version >= ENTITY_IDS_SINCE {
        return postcard::from_bytes(payload);
    }
    let old: SaveFileRawV1 = postcard::from_bytes(payload)?;
    Ok(SaveFileRaw {
        version: old.version,
        game_version: old.game_version,
        engine_version: old.engine_version,
        timestamp_secs: old.timestamp_secs,
        thumbnail: old.thumbnail,
        entities: old.entities,
        resources: old.resources,
        entity_ids: Vec::new(),
    })
}

/// Encodes `raw` in the layout of `raw.version` (format 1 drops `entity_ids`).
pub(crate) fn encode_payload(raw: &SaveFileRaw) -> Result<Vec<u8>, postcard::Error> {
    if raw.version >= ENTITY_IDS_SINCE {
        return postcard::to_allocvec(raw);
    }
    postcard::to_allocvec(&SaveFileRawV1Ref {
        version: raw.version,
        game_version: &raw.game_version,
        engine_version: &raw.engine_version,
        timestamp_secs: raw.timestamp_secs,
        thumbnail: &raw.thumbnail,
        entities: &raw.entities,
        resources: &raw.resources,
    })
}

/// The header fields every format shares (format 1's layout is a prefix of format 2's).
pub(crate) fn decode_header_fields(payload: &[u8]) -> Option<(String, String, f64, bool)> {
    #[derive(Deserialize)]
    struct Prefix {
        _version: u32,
        game_version: String,
        engine_version: String,
        timestamp_secs: f64,
        thumbnail: Option<Vec<u8>>,
    }
    let prefix: Prefix = postcard::from_bytes(payload).ok()?;
    Some((
        prefix.game_version,
        prefix.engine_version,
        prefix.timestamp_secs,
        prefix.thumbnail.is_some(),
    ))
}

/// Whether the engine handles the step from `version` to `version + 1` itself (format 1 → 2).
pub(crate) fn engine_step(version: u32) -> bool {
    version + 1 == ENTITY_IDS_SINCE
}

/// Converts the references of a payload with an empty id table: every non-placeholder `Entity`
/// in a registered component becomes [`SaveEntityId::Lost`]. A payload without references stays
/// as it is.
pub(crate) fn convert_legacy_refs(raw: &mut SaveFileRaw, registry: &TypeRegistry) {
    if !raw.entity_ids.is_empty() || raw.entities.is_empty() {
        return;
    }
    let Some(lost) = marker(raw.entities.len()) else {
        return;
    };
    let mut scans = Scans::default();
    let mut codec = ReflectCodec::new(registry);
    let mut converted = 0usize;
    for entity in &mut raw.entities {
        for (short_path, bytes) in &mut entity.components {
            let Some(reg) = registry.get_with_short_type_path(short_path) else {
                continue;
            };
            if !scans.get(registry, reg.type_id()).refs {
                continue;
            }
            let Ok(mut value) = codec.from_postcard_typed(bytes, reg) else {
                continue;
            };
            let mut changed = false;
            let _ = walk(value.as_partial_reflect_mut(), &mut |_, _| {
                changed = true;
                Leaf::To(lost)
            });
            if changed && let Ok(rewritten) = codec.to_postcard_typed(value.as_partial_reflect()) {
                *bytes = rewritten;
                converted += 1;
            }
        }
    }
    if converted > 0 {
        tracing::info!(
            target: "regia_framework",
            "save written before format {ENTITY_IDS_SINCE}: {converted} component(s) held entity references, now lost"
        );
        raw.entity_ids = (0..raw.entities.len())
            .map(|n| SaveEntityId::Runtime(u32::try_from(n).unwrap_or(u32::MAX)))
            .chain(std::iter::once(SaveEntityId::Lost))
            .collect();
    }
}

/// Rewrites format 1 save-file bytes as format 2 with an empty id table (`regia upgrade-project`):
/// the game converts the references on its next load, with its own types. Current bytes come back
/// unchanged.
///
/// The layout follows the CRC-covered payload version (the payload's first field), as the loader
/// does, never the header word, which is outside the CRC: a current payload under a damaged older
/// header only gets its header word repaired, so its id table is kept. A payload that needs a
/// migration step the game registers (any step but format 1 to 2) is refused and left for the
/// game's own load, since the tool cannot run the game's migrations.
///
/// # Errors
/// [`SaveError::Corrupt`] for a bad magic, length or CRC; [`SaveError::Deserialize`] for a payload
/// that does not decode, a version newer than this engine, or a step only the game can migrate.
pub fn upgrade_save_bytes(bytes: &[u8]) -> Result<Vec<u8>, SaveError> {
    use crate::save_game::SAVE_FORMAT_VERSION;
    if bytes.len() < 12 || &bytes[..4] != SAVE_MAGIC {
        return Err(SaveError::Corrupt("not a save file (magic or length)".into()));
    }
    let mut version = [0u8; 4];
    version.copy_from_slice(&bytes[4..8]);
    let version = u32::from_le_bytes(version);
    if version > SAVE_FORMAT_VERSION {
        return Err(SaveError::Deserialize(format!(
            "save file version {version} is newer than this engine supports ({SAVE_FORMAT_VERSION})"
        )));
    }
    if version == SAVE_FORMAT_VERSION {
        return Ok(bytes.to_vec());
    }
    let payload = &bytes[8..bytes.len() - 4];
    let mut crc = [0u8; 4];
    crc.copy_from_slice(&bytes[bytes.len() - 4..]);
    if crate::save_game::crc32(payload) != u32::from_le_bytes(crc) {
        return Err(SaveError::Corrupt("CRC mismatch".into()));
    }
    // The payload's first field is its version varint, and it is what the CRC covers: a payload
    // that does not begin with a decodable one (only a crafted file: every version this engine
    // writes encodes as one byte under 0x80) is refused by name, never read through the header
    // word, which the CRC does not cover.
    let layout = crate::save_game::leading_varint_u32(payload).ok_or_else(|| {
        SaveError::Deserialize("payload does not begin with a decodable version varint".into())
    })?;
    if layout > SAVE_FORMAT_VERSION {
        return Err(SaveError::Deserialize(format!(
            "save payload version {layout} is newer than this engine supports ({SAVE_FORMAT_VERSION})"
        )));
    }
    if layout == SAVE_FORMAT_VERSION {
        // Damage to the header word only: repair it and keep the payload byte for byte.
        let mut out = bytes.to_vec();
        out[4..8].copy_from_slice(&SAVE_FORMAT_VERSION.to_le_bytes());
        return Ok(out);
    }
    if let Some(step) = (layout..SAVE_FORMAT_VERSION).find(|v| !engine_step(*v)) {
        return Err(SaveError::Deserialize(format!(
            "save payload version {layout} needs the game's migration from version {step} to {}; \
             left as it is for the game to load",
            step + 1
        )));
    }
    let mut raw = decode_payload(layout, payload).map_err(|e| SaveError::Deserialize(e.to_string()))?;
    raw.version = SAVE_FORMAT_VERSION;
    let payload = encode_payload(&raw).map_err(|e| SaveError::Serialize(e.to_string()))?;
    let mut out = Vec::with_capacity(payload.len() + 12);
    out.extend_from_slice(SAVE_MAGIC);
    out.extend_from_slice(&SAVE_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&crate::save_game::crc32(&payload).to_le_bytes());
    Ok(out)
}

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

/// Every saved entity of `raw` with its id, components and references (agent `save.inspect`).
/// A component that does not decode with `registry` is listed without references.
pub fn inspect_refs(raw: &SaveFileRaw, registry: &TypeRegistry) -> Vec<SavedEntityRefs> {
    let mut scans = Scans::default();
    let mut codec = ReflectCodec::new(registry);
    raw.entities
        .iter()
        .enumerate()
        .map(|(i, saved)| {
            let mut refs = Vec::new();
            for (short_path, bytes) in &saved.components {
                let Some(reg) = registry.get_with_short_type_path(short_path) else {
                    continue;
                };
                if !scans.get(registry, reg.type_id()).refs {
                    continue;
                }
                let Ok(mut value) = codec.from_postcard_typed(bytes, reg) else {
                    continue;
                };
                // `Entity::PLACEHOLDER` ("no entity") is not a reference and is not listed.
                let _ = walk(value.as_partial_reflect_mut(), &mut |target, field| {
                    refs.push(SavedEntityRef {
                        component: short_path.clone(),
                        field: field.to_string(),
                        target: marker_id(target).and_then(|id| u32::try_from(id).ok()),
                    });
                    Leaf::To(target)
                });
            }
            SavedEntityRefs {
                id: u32::try_from(i).unwrap_or(u32::MAX),
                key: raw.entity_ids.get(i).copied(),
                components: saved.components.keys().cloned().collect(),
                refs,
            }
        })
        .collect()
}

/// Parses, migrates and converts save-file `bytes` the way a load does, without touching the
/// world's entities (agent `save.inspect`).
///
/// # Errors
/// The load's parse errors.
pub fn parse_save_bytes(world: &mut World, bytes: &[u8]) -> Result<SaveFileRaw, SaveError> {
    crate::save_game::parse_save_file(bytes, world).map(|(raw, _)| raw)
}

#[cfg(test)]
mod tests;
