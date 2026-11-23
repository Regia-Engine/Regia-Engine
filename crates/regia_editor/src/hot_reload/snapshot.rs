// SPDX-License-Identifier: MPL-2.0
//! Snapshot, teardown, identity check and field-by-field restore (specs/20 § 3).
//!
//! Snapshot values are RON text plus the field's type path. Text holds no pointer into the old
//! library. A field whose type path changed takes `Default` instead of being coerced (an `f32`
//! RON value would otherwise decode as `f64`).
//!
//! Entity keys are [`Entity::to_bits`] (index and generation packed in a `u64`). Restore looks
//! them up with [`Entity::try_from_bits`]. `EntityGuid` is not the snapshot key: a despawned
//! entity is dropped, and a new entity that reused the index does not receive the component.
//!
//! Script slots (`specs/20` § 3 steps 5, 6 and 9): a slot of the engine-owned [`Scripts`]
//! component belongs to the game library when its type path was registered under the library's
//! owner ([`ScriptRegistry::owner_of`]) or its instance's `TypeId` is one of the owner's reflected
//! types. The snapshot keeps the slot's position, flags and reflected fields (read through the
//! registration's `reflect` accessor, ADR-7206); teardown removes the slot and the owner's
//! registry entries; restore constructs the script again through the *new* library's
//! registration and applies the fields like a component's. A type path the new library no
//! longer registers drops its slots with a warning.

use std::any::TypeId;
use std::collections::{HashMap, HashSet};

use regia_framework::script::{ScriptRegistry, ScriptSlot, Scripts};

use serde::{Deserialize, Serialize};

use crate::engine::app::App;
use crate::engine::ecs::ecs::component::ComponentId;
use crate::engine::ecs::ecs::schedule::ScheduleCleanupPolicy;
use crate::engine::ecs::prelude::*;
use crate::engine::ecs::reflect::{AppTypeRegistry, ReflectComponent, ReflectMut, ReflectRef};
use crate::engine::ecs::serialize::ReflectCodec;

/// One reflected field, as RON, with the type path it was written from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldRecord {
    /// Struct field name.
    pub name: String,
    /// Type path at snapshot time (`f32`, `hot_fixture::Inner`, …).
    pub type_path: String,
    /// RON text of the field value. No pointer into the old library.
    pub ron: String,
}

/// Reflected fields of one component or resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldSnap {
    /// A reflected struct, one record per field.
    Struct(Vec<FieldRecord>),
    /// A tuple, enum or opaque value, with the value's type path.
    Whole {
        /// Type path of the whole value.
        type_path: String,
        /// RON text of the whole value.
        ron: String,
    },
}

/// One component instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentSnap {
    /// [`Entity::to_bits`] of the entity that held the component.
    pub entity: u64,
    /// Stable type path (`hot_fixture::Mover`).
    pub type_path: String,
    /// Fields at snapshot time.
    pub fields: FieldSnap,
}

/// One resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSnap {
    /// Stable type path.
    pub type_path: String,
    /// Fields at snapshot time.
    pub fields: FieldSnap,
}

/// One script slot owned by the game plugin (`specs/20` § 3 step 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptSnap {
    /// [`Entity::to_bits`] of the entity whose [`Scripts`] held the slot.
    pub entity: u64,
    /// Position of the slot in that entity's [`Scripts`] at snapshot time.
    pub slot: usize,
    /// Script type path (the [`ScriptRegistry`] key).
    pub type_path: String,
    /// Whether `start` had run (a rebuilt script does not run `start` again).
    pub started: bool,
    /// Whether the slot was enabled.
    pub enabled: bool,
    /// Reflected fields; `None` when the script has no reflected view (a hand-written
    /// `ReflectScript` without the accessors), so the rebuilt script keeps its defaults.
    pub fields: Option<FieldSnap>,
}

/// Reflected state of every component and resource owned by one game plugin, plus its script
/// slots.
///
/// The value is plain data. Dropping the old library does not invalidate it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotReloadSnapshot {
    /// Component instances, keyed by entity bits + type path.
    pub components: Vec<ComponentSnap>,
    /// Resources, keyed by type path.
    pub resources: Vec<ResourceSnap>,
    /// Script slots, keyed by entity bits + slot position + type path. Absent in a snapshot file
    /// written before script slots were kept (read as empty).
    #[serde(default)]
    pub scripts: Vec<ScriptSnap>,
}

impl HotReloadSnapshot {
    /// Component instances plus resources.
    #[must_use]
    pub fn instance_count(&self) -> usize {
        self.components.len() + self.resources.len()
    }
}

/// What [`teardown_owner`] removed, and what it could not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TeardownReport {
    /// Systems removed from schedules.
    pub systems_removed: usize,
    /// Component instances removed.
    pub components_removed: usize,
    /// Resources removed.
    pub resources_removed: usize,
    /// Observer entities despawned.
    pub observers_removed: usize,
    /// Reflect registrations dropped.
    pub registrations_removed: usize,
    /// Script slots of the owner removed from [`Scripts`] components.
    pub script_slots_removed: usize,
    /// [`ScriptRegistry`] entries of the owner removed.
    pub script_registrations_removed: usize,
    /// Owned component instances still in the world afterwards. Specs/20 § 3 requires zero.
    pub leftover_instances: usize,
}

/// What [`restore_snapshot`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Component instances inserted.
    pub components: usize,
    /// Resources whose fields were applied.
    pub resources: usize,
    /// Fields the new type does not have yet (they kept `Default`).
    pub defaulted_fields: usize,
    /// Snapshot fields the new type no longer has.
    pub dropped_fields: usize,
    /// Fields whose type path changed (kept `Default`, warning logged).
    pub retyped_fields: usize,
    /// Snapshot entries whose type is gone.
    pub dropped_values: usize,
    /// Script slots constructed again through the new library's registration.
    pub scripts_rebuilt: usize,
    /// Script slots dropped: the type path is no longer registered, or the entity is gone.
    pub scripts_dropped: usize,
}

/// A game type whose `TypeId` already had a [`ComponentId`] before this register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityClash {
    /// Type path.
    pub type_path: String,
    /// `TypeId` that bevy_ecs already knows.
    pub type_id: TypeId,
}

/// Serialises every component and resource owned by `owner` (specs/20 § 3, snapshot step).
#[must_use]
pub fn take_snapshot(world: &World, owner: TypeOwner) -> HotReloadSnapshot {
    let type_ids: Vec<(TypeId, String)> = world
        .get_resource::<TypeOwnership>()
        .map(|ownership| {
            ownership
                .types_of(owner)
                .into_iter()
                .map(|(id, record)| (id, record.type_path.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let registry = world
        .get_resource::<AppTypeRegistry>()
        .cloned()
        .unwrap_or_else(AppTypeRegistry::default);
    let registry = registry.read();
    let mut snapshot = HotReloadSnapshot::default();
    for (type_id, type_path) in type_ids {
        let Some(registration) = registry.get(type_id) else {
            continue;
        };
        let Some(reflect_component) = registration.data::<ReflectComponent>() else {
            continue;
        };
        let Some(component_id) = world.components().get_id(type_id) else {
            continue;
        };
        if let Some(entity) = world.resource_entities().get(component_id) {
            if let Some(value) = reflect_component.reflect(world.entity(entity)) {
                snapshot.resources.push(ResourceSnap {
                    type_path,
                    fields: fields_of(value.as_partial_reflect(), &registry),
                });
            }
            continue;
        }
        for entity in entities_with(world, component_id) {
            if let Some(value) = reflect_component.reflect(world.entity(entity)) {
                snapshot.components.push(ComponentSnap {
                    entity: entity.to_bits(),
                    type_path: type_path.clone(),
                    fields: fields_of(value.as_partial_reflect(), &registry),
                });
            }
        }
    }
    snapshot.scripts = snapshot_scripts(world, owner, &registry);
    snapshot
}

/// Whether `slot` belongs to `owner`: its type path was registered under `owner`, or its
/// instance's `TypeId` is one of `owner`'s reflected types.
fn slot_is_owned(
    slot: &ScriptSlot,
    owner: TypeOwner,
    scripts: Option<&ScriptRegistry>,
    ownership: Option<&TypeOwnership>,
) -> bool {
    if scripts.and_then(|registry| registry.owner_of(&slot.type_path)) == Some(owner) {
        return true;
    }
    slot.instance.as_deref().is_some_and(|instance| {
        // `std::any::Any` is not imported: in scope it would take over `.type_id()` calls on
        // `&TypeRegistration` elsewhere in this module.
        let any: &dyn std::any::Any = instance;
        ownership.and_then(|records| records.type_owner(<dyn std::any::Any>::type_id(any))) == Some(owner)
    })
}

/// Entities that carry [`Scripts`], in archetype order.
fn scripted_entities(world: &World) -> Vec<Entity> {
    world
        .components()
        .get_id(TypeId::of::<Scripts>())
        .map(|id| entities_with(world, id))
        .unwrap_or_default()
}

/// The owner's script slots, with their reflected fields (`specs/20` § 3 step 5).
fn snapshot_scripts(world: &World, owner: TypeOwner, registry: &TypeRegistry) -> Vec<ScriptSnap> {
    let scripts_registry = world.get_resource::<ScriptRegistry>();
    let ownership = world.get_resource::<TypeOwnership>();
    let mut out = Vec::new();
    for entity in scripted_entities(world) {
        let Some(scripts) = world.get::<Scripts>(entity) else {
            continue;
        };
        for (slot_index, slot) in scripts.0.iter().enumerate() {
            if !slot_is_owned(slot, owner, scripts_registry, ownership) {
                continue;
            }
            let fields = slot.instance.as_deref().and_then(|instance| {
                let desc = scripts_registry?.get(&slot.type_path)?;
                (desc.reflect)(instance).map(|value| fields_of(value, registry))
            });
            out.push(ScriptSnap {
                entity: entity.to_bits(),
                slot: slot_index,
                type_path: slot.type_path.clone(),
                started: slot.started,
                enabled: slot.enabled,
                fields,
            });
        }
    }
    out
}

/// Removes the owner's script slots from every [`Scripts`] and the owner's [`ScriptRegistry`]
/// entries (`specs/20` § 3 step 6). The old instances are dropped without `on_destroy`: the
/// script is being rebuilt, not destroyed. Answers `(slots, registrations)` removed.
fn teardown_scripts(world: &mut World, owner: TypeOwner) -> (usize, usize) {
    let entities = scripted_entities(world);
    let mut slots_removed = 0;
    for entity in entities {
        let owned: Vec<usize> = {
            let Some(scripts) = world.get::<Scripts>(entity) else {
                continue;
            };
            let registry = world.get_resource::<ScriptRegistry>();
            let ownership = world.get_resource::<TypeOwnership>();
            scripts
                .0
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot_is_owned(slot, owner, registry, ownership))
                .map(|(index, _)| index)
                .collect()
        };
        if owned.is_empty() {
            continue;
        }
        if let Some(mut scripts) = world.get_mut::<Scripts>(entity) {
            for index in owned.iter().rev() {
                scripts.0.remove(*index);
                slots_removed += 1;
            }
        }
    }
    let registrations_removed = world
        .get_resource_mut::<ScriptRegistry>()
        .map_or(0, |mut registry| registry.remove_owner(owner));
    (slots_removed, registrations_removed)
}

/// Constructs the snapshot's script slots again through the current (new) [`ScriptRegistry`]
/// and applies their fields (`specs/20` § 3 step 9).
fn restore_scripts(
    world: &mut World,
    snapshot: &HotReloadSnapshot,
    registry: &TypeRegistry,
    report: &mut RestoreReport,
) {
    if snapshot.scripts.is_empty() {
        return;
    }
    let scripts_registry = world
        .get_resource::<ScriptRegistry>()
        .cloned()
        .unwrap_or_default();
    let mut dropped_by_type: HashMap<&str, usize> = HashMap::new();
    for snap in &snapshot.scripts {
        let Some(desc) = scripts_registry.get(&snap.type_path).copied() else {
            report.scripts_dropped += 1;
            *dropped_by_type.entry(snap.type_path.as_str()).or_default() += 1;
            continue;
        };
        let Some(entity) =
            Entity::try_from_bits(snap.entity).filter(|entity| world.get_entity(*entity).is_ok())
        else {
            report.scripts_dropped += 1;
            continue;
        };
        let mut instance = (desc.factory)();
        if let Some(fields) = &snap.fields {
            match (desc.reflect_mut)(instance.as_mut()) {
                Some(target) => apply_fields(target, fields, registry, report),
                None => tracing::warn!(
                    target: "regia_editor",
                    type_path = %snap.type_path,
                    "hot reload: the rebuilt script has no reflected view; its fields keep their defaults"
                ),
            }
        }
        let slot = ScriptSlot {
            type_path: desc.type_path.to_string(),
            order: desc.order,
            instance: Some(instance),
            started: snap.started,
            enabled: snap.enabled,
        };
        if let Some(mut scripts) = world.get_mut::<Scripts>(entity) {
            let index = snap.slot.min(scripts.0.len());
            scripts.0.insert(index, slot);
        } else {
            world
                .entity_mut(entity)
                .insert([slot].into_iter().collect::<Scripts>());
        }
        report.scripts_rebuilt += 1;
    }
    for (type_path, count) in dropped_by_type {
        tracing::warn!(
            target: "regia_editor",
            type_path = %type_path,
            count,
            "hot reload dropped script slots whose type disappeared"
        );
    }
}

/// Removes everything `owner` registered.
///
/// Systems are removed with [`Schedules::remove_systems_in_set`] after `Schedules` has been taken
/// out of the world. That call may insert resources; `World::resource_scope` would panic.
///
/// The reflect registry is rebuilt without the owner's registrations (bevy_reflect has no
/// unregister). Engine-owned resources — including an external sim handle (specs/25 § 3) — are
/// not in `owner`'s records and stay.
pub fn teardown_owner(world: &mut World, owner: TypeOwner) -> TeardownReport {
    let mut report = TeardownReport::default();
    let records: Vec<TypeId> = {
        let Some(ownership) = world.get_resource::<TypeOwnership>() else {
            return report;
        };
        ownership.types_of(owner).into_iter().map(|(id, _)| id).collect()
    };
    let reflect_ids: HashSet<TypeId> = records.iter().copied().collect();
    let component_ids: Vec<(TypeId, ComponentId)> = records
        .iter()
        .filter_map(|type_id| world.components().get_id(*type_id).map(|id| (*type_id, id)))
        .collect();

    let schedules: Vec<InternedScheduleLabel> = world
        .get_resource::<TypeOwnership>()
        .map(|ownership| {
            ownership
                .systems_of(owner)
                .map(|system| system.schedule)
                .collect()
        })
        .unwrap_or_default();
    let mut seen_schedules = HashSet::new();
    let schedules: Vec<_> = schedules
        .into_iter()
        .filter(|label| seen_schedules.insert(*label))
        .collect();

    for (_, component_id) in &component_ids {
        if world.resource_entities().get(*component_id).is_some() {
            if world.remove_resource_by_id(*component_id) {
                report.resources_removed += 1;
            }
            continue;
        }
        for entity in entities_with(world, *component_id) {
            if let Ok(mut entity) = world.get_entity_mut(entity) {
                entity.remove_by_id(*component_id);
                report.components_removed += 1;
            }
        }
    }

    let (script_slots, script_registrations) = teardown_scripts(world, owner);
    report.script_slots_removed = script_slots;
    report.script_registrations_removed = script_registrations;

    if let Some(mut schedules_resource) = world.remove_resource::<Schedules>() {
        for label in &schedules {
            match schedules_resource.remove_systems_in_set(
                *label,
                OwnerSet(owner),
                world,
                ScheduleCleanupPolicy::RemoveSetAndSystems,
            ) {
                Ok(count) => report.systems_removed += count,
                Err(error) => {
                    tracing::warn!(target: "regia_editor", "cannot remove systems in {label:?}: {error}");
                }
            }
        }
        // `remove_systems_in_set` may initialise an empty `Schedules` in the world while this
        // one is taken out (`Schedule::initialize` calls `get_resource_or_init`). Drop that
        // empty resource, then put the edited schedules back.
        world.remove_resource::<Schedules>();
        world.insert_resource(schedules_resource);
    }

    let observers: Vec<Entity> = world
        .get_resource::<TypeOwnership>()
        .map(|ownership| {
            ownership
                .observers_of(owner)
                .map(|observer| observer.entity)
                .collect()
        })
        .unwrap_or_default();
    for entity in observers {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
            report.observers_removed += 1;
        }
    }
    world.flush();

    let messages: Vec<String> = world
        .get_resource::<TypeOwnership>()
        .map(|ownership| {
            ownership
                .messages_of(owner)
                .map(|message| message.type_name.to_string())
                .collect()
        })
        .unwrap_or_default();
    remove_message_resources(world, &messages);

    if let Some(registry) = world.get_resource::<AppTypeRegistry>().cloned() {
        let kept: Vec<_> = {
            let registry = registry.read();
            registry
                .iter()
                .filter(|registration| !reflect_ids.contains(&registration.type_id()))
                .cloned()
                .collect::<Vec<_>>()
        };
        let removed = {
            let registry_read = registry.read();
            registry_read.iter().count().saturating_sub(kept.len())
        };
        report.registrations_removed = removed;
        let mut fresh = TypeRegistry::empty();
        for registration in kept {
            fresh.add_registration(registration);
        }
        *registry.write() = fresh;
    }

    for (_, component_id) in &component_ids {
        if world.resource_entities().get(*component_id).is_some() {
            continue;
        }
        report.leftover_instances += entities_with(world, *component_id).len();
    }
    if let Some(mut ownership) = world.get_resource_mut::<TypeOwnership>() {
        ownership.forget(owner);
    }
    if report.leftover_instances != 0 {
        tracing::error!(
            target: "regia_editor",
            leftover = report.leftover_instances,
            "hot reload teardown left owned component instances"
        );
    }
    report
}

/// `TypeId`s that already have a [`ComponentId`]. Captured before `register`.
#[must_use]
pub fn component_type_ids(world: &World) -> HashSet<TypeId> {
    let components = world.components();
    let mut ids = HashSet::new();
    for index in 0..components.len() {
        let id = ComponentId::new(index);
        if let Some(info) = components.get_info(id)
            && let Some(type_id) = info.type_id()
        {
            ids.insert(type_id);
        }
    }
    ids
}

/// Game types registered by `owner` whose `TypeId` was already a component before `register`.
///
/// The metadata salt makes this a backstop. A hit means bevy_ecs would reuse the old column
/// layout (ADR-015). Callers refuse the reload.
#[must_use]
pub fn identity_clashes(world: &World, owner: TypeOwner, before: &HashSet<TypeId>) -> Vec<IdentityClash> {
    let Some(ownership) = world.get_resource::<TypeOwnership>() else {
        return Vec::new();
    };
    ownership
        .types_of(owner)
        .into_iter()
        .filter_map(|(type_id, record)| {
            if !before.contains(&type_id) {
                return None;
            }
            world.components().get_id(type_id)?;
            Some(IdentityClash {
                type_path: record.type_path.to_string(),
                type_id,
            })
        })
        .collect()
}

/// Restores `snapshot` through the world's current registry (the new library's types).
pub fn restore_snapshot(world: &mut World, snapshot: &HotReloadSnapshot) -> RestoreReport {
    let registry = world
        .get_resource::<AppTypeRegistry>()
        .cloned()
        .unwrap_or_else(AppTypeRegistry::default);
    let registry = registry.read();
    let mut report = RestoreReport::default();
    let mut dropped_by_type: HashMap<String, usize> = HashMap::new();

    for component in &snapshot.components {
        let Some(registration) = registry.get_with_type_path(&component.type_path) else {
            report.dropped_values += 1;
            *dropped_by_type.entry(component.type_path.clone()).or_default() += 1;
            continue;
        };
        let (Some(reflect_component), Some(default)) = (
            registration.data::<ReflectComponent>(),
            registration.data::<ReflectDefault>(),
        ) else {
            report.dropped_values += 1;
            *dropped_by_type.entry(component.type_path.clone()).or_default() += 1;
            continue;
        };
        let Some(entity) = Entity::try_from_bits(component.entity) else {
            report.dropped_values += 1;
            continue;
        };
        let Ok(mut entity) = world.get_entity_mut(entity) else {
            report.dropped_values += 1;
            continue;
        };
        let mut value = default.default();
        apply_fields(
            value.as_partial_reflect_mut(),
            &component.fields,
            &registry,
            &mut report,
        );
        reflect_component.insert(&mut entity, value.as_partial_reflect(), &registry);
        report.components += 1;
    }

    for resource in &snapshot.resources {
        let Some(registration) = registry.get_with_type_path(&resource.type_path) else {
            report.dropped_values += 1;
            *dropped_by_type.entry(resource.type_path.clone()).or_default() += 1;
            continue;
        };
        let Some(reflect_component) = registration.data::<ReflectComponent>() else {
            report.dropped_values += 1;
            continue;
        };
        let Some(component_id) = world.components().get_id(registration.type_id()) else {
            report.dropped_values += 1;
            continue;
        };
        let Some(entity) = world.resource_entities().get(component_id) else {
            report.dropped_values += 1;
            continue;
        };
        let Ok(mut entity) = world.get_entity_mut(entity) else {
            report.dropped_values += 1;
            continue;
        };
        if let Some(mut value) = reflect_component.reflect_mut(&mut entity) {
            apply_fields(
                value.as_partial_reflect_mut(),
                &resource.fields,
                &registry,
                &mut report,
            );
            report.resources += 1;
        }
    }

    restore_scripts(world, snapshot, &registry, &mut report);

    for (type_path, count) in dropped_by_type {
        tracing::warn!(
            target: "regia_editor",
            type_path = %type_path,
            count,
            "hot reload dropped values whose type disappeared"
        );
    }
    report
}

/// Number of reflected component/resource types `owner` registered.
#[must_use]
pub fn owned_reflected_types(world: &World, owner: TypeOwner) -> usize {
    world
        .get_resource::<TypeOwnership>()
        .map(|ownership| ownership.types_of(owner).len())
        .unwrap_or(0)
}

/// First numeric value of `field` on a component or resource with `type_path`.
#[must_use]
pub fn reflected_number(world: &World, type_path: &str, field: &str) -> Option<f64> {
    let registry = world.get_resource::<AppTypeRegistry>()?.clone();
    let registry = registry.read();
    let registration = registry.get_with_type_path(type_path)?;
    let reflect_component = registration.data::<ReflectComponent>()?;
    let component_id = world.components().get_id(registration.type_id())?;
    let mut entities = Vec::new();
    if let Some(entity) = world.resource_entities().get(component_id) {
        entities.push(entity);
    } else {
        entities.extend(entities_with(world, component_id));
    }
    for entity in entities {
        let value = reflect_component.reflect(world.entity(entity))?;
        let ReflectRef::Struct(structure) = value.reflect_ref() else {
            return None;
        };
        let field = structure.field(field)?;
        if let Some(number) = as_number(field) {
            return Some(number);
        }
    }
    None
}

/// How many entities carry the component `type_path` (resources count as one).
#[must_use]
pub fn reflected_count(world: &World, type_path: &str) -> usize {
    let Some(registry) = world.get_resource::<AppTypeRegistry>() else {
        return 0;
    };
    let registry = registry.read();
    let Some(registration) = registry.get_with_type_path(type_path) else {
        return 0;
    };
    let Some(component_id) = world.components().get_id(registration.type_id()) else {
        return 0;
    };
    if world.resource_entities().get(component_id).is_some() {
        return 1;
    }
    entities_with(world, component_id).len()
}

/// Writes the snapshot to `path` as RON (the file name specs/20 § 5 asks for is `last_snapshot.bin`).
///
/// # Errors
///
/// Returns the IO or RON error text. Callers log it; a missing file means a later retry has no state.
pub fn write_snapshot(path: &std::path::Path, snapshot: &HotReloadSnapshot) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text =
        ron::ser::to_string_pretty(snapshot, regia_core::ron_pretty()).map_err(|error| error.to_string())?;
    // Temp file + rename: a crash or a full disk mid-write leaves the previous file, never a
    // torn one (audit 11-03 dylib M3). The temp name is unique per process and call.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}-{serial}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        use std::io::Write as _;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    if let Err(error) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.to_string());
    }
    Ok(())
}

/// Reads a snapshot written by [`write_snapshot`].
///
/// # Errors
///
/// Returns the IO or RON error text.
pub fn read_snapshot(path: &std::path::Path) -> Result<HotReloadSnapshot, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    ron::from_str(&text).map_err(|error| error.to_string())
}

fn entities_with(world: &World, id: ComponentId) -> Vec<Entity> {
    world
        .archetypes()
        .iter()
        .filter(|archetype| archetype.contains(id))
        .flat_map(|archetype| archetype.entities().iter().map(|entity| entity.id()))
        .collect()
}

fn fields_of(value: &dyn PartialReflect, registry: &TypeRegistry) -> FieldSnap {
    match value.reflect_ref() {
        ReflectRef::Struct(structure) => FieldSnap::Struct(
            (0..structure.field_len())
                .filter_map(|index| {
                    let name = structure.name_at(index)?.to_string();
                    let field = structure.field_at(index)?;
                    let type_path = field
                        .get_represented_type_info()
                        .map(|info| info.type_path().to_string())
                        .unwrap_or_default();
                    Some(FieldRecord {
                        name,
                        type_path,
                        ron: to_ron(field, registry),
                    })
                })
                .collect(),
        ),
        _ => FieldSnap::Whole {
            type_path: value
                .get_represented_type_info()
                .map(|info| info.type_path().to_string())
                .unwrap_or_default(),
            ron: to_ron(value, registry),
        },
    }
}

fn to_ron(value: &dyn PartialReflect, registry: &TypeRegistry) -> String {
    ReflectCodec::new(registry)
        .to_ron_typed(value)
        .unwrap_or_default()
}

fn apply_fields(
    target: &mut dyn PartialReflect,
    fields: &FieldSnap,
    registry: &TypeRegistry,
    report: &mut RestoreReport,
) {
    match fields {
        FieldSnap::Struct(list) => {
            let ReflectMut::Struct(structure) = target.reflect_mut() else {
                report.dropped_fields += list.len();
                return;
            };
            let present: HashSet<&str> = list.iter().map(|record| record.name.as_str()).collect();
            // Count new fields before taking a mutable field borrow.
            let defaulted = (0..structure.field_len())
                .filter_map(|index| structure.name_at(index))
                .filter(|name| !present.contains(name))
                .count();
            report.defaulted_fields += defaulted;
            for record in list {
                apply_one_field(target, record, registry, report);
            }
        }
        FieldSnap::Whole { type_path, ron } => {
            let current = target
                .get_represented_type_info()
                .map(|info| info.type_path().to_string())
                .unwrap_or_default();
            if current != *type_path {
                tracing::warn!(
                    target: "regia_editor",
                    from = %type_path,
                    to = %current,
                    "hot reload: value changed type; using the default"
                );
                report.retyped_fields += 1;
                return;
            }
            let Some(type_id) = target.get_represented_type_info().map(|info| info.type_id()) else {
                report.retyped_fields += 1;
                return;
            };
            match deserialize_ron(ron, type_id, registry) {
                Some(value) if target.try_apply(value.as_ref()).is_ok() => {}
                _ => {
                    tracing::warn!(target: "regia_editor", "hot reload: could not apply value; using the default");
                    report.retyped_fields += 1;
                }
            }
        }
    }
}

fn apply_one_field(
    target: &mut dyn PartialReflect,
    record: &FieldRecord,
    registry: &TypeRegistry,
    report: &mut RestoreReport,
) {
    let ReflectMut::Struct(structure) = target.reflect_mut() else {
        report.dropped_fields += 1;
        return;
    };
    let Some(field) = structure.field_mut(&record.name) else {
        report.dropped_fields += 1;
        return;
    };
    let current = field
        .get_represented_type_info()
        .map(|info| info.type_path().to_string())
        .unwrap_or_default();
    if current != record.type_path {
        tracing::warn!(
            target: "regia_editor",
            field = %record.name,
            from = %record.type_path,
            to = %current,
            "hot reload: field changed type; using the default"
        );
        report.retyped_fields += 1;
        return;
    }
    let Some(info) = field.get_represented_type_info() else {
        report.retyped_fields += 1;
        return;
    };
    let type_id = info.type_id();
    match deserialize_ron(&record.ron, type_id, registry) {
        Some(value) if field.try_apply(value.as_ref()).is_ok() => {}
        _ => {
            tracing::warn!(
                target: "regia_editor",
                field = %record.name,
                "hot reload: field changed type; using the default"
            );
            report.retyped_fields += 1;
        }
    }
}

fn deserialize_ron(
    ron_text: &str,
    type_id: TypeId,
    registry: &TypeRegistry,
) -> Option<Box<dyn PartialReflect>> {
    let registration = registry.get(type_id)?;
    let value = ReflectCodec::new(registry)
        .from_ron_typed(ron_text, registration)
        .ok()?;
    Some(value.into_partial_reflect())
}

fn as_number(value: &dyn PartialReflect) -> Option<f64> {
    if let Some(number) = value.try_downcast_ref::<f32>() {
        return Some(f64::from(*number));
    }
    if let Some(number) = value.try_downcast_ref::<f64>() {
        return Some(*number);
    }
    if let Some(number) = value.try_downcast_ref::<u32>() {
        return Some(f64::from(*number));
    }
    if let Some(number) = value.try_downcast_ref::<u64>() {
        return Some(*number as f64);
    }
    if let Some(flag) = value.try_downcast_ref::<bool>() {
        return Some(if *flag { 1.0 } else { 0.0 });
    }
    None
}

fn remove_message_resources(world: &mut World, type_names: &[String]) {
    if type_names.is_empty() {
        return;
    }
    let mut ids = Vec::new();
    let components = world.components();
    for index in 0..components.len() {
        let id = ComponentId::new(index);
        let Some(info) = components.get_info(id) else {
            continue;
        };
        let name = format!("{}", info.name());
        if type_names
            .iter()
            .any(|type_name| name.contains("Messages<") && name.contains(type_name.as_str()))
        {
            ids.push(id);
        }
    }
    for id in ids {
        world.remove_resource_by_id(id);
    }
}

/// Runs `body` while `owner` is the app's current registration owner, then restores the previous one.
///
/// A panic inside `body` still restores the previous owner, then continues unwinding.
pub fn with_game_owner(app: &mut App, owner: TypeOwner, body: impl FnOnce(&mut App)) {
    let previous = app.set_current_owner(owner);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(app)));
    app.set_current_owner(previous);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::app::prelude::*;

    #[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
    #[reflect(Component, Default)]
    struct Bag {
        speed: f32,
        tag: u32,
    }

    #[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
    #[reflect(Resource, Default)]
    struct BagStore {
        speed: f32,
        tag: u32,
    }

    #[derive(Resource, Default, Debug, PartialEq)]
    struct EngineSim {
        token: u64,
    }

    #[derive(Reflect, Default, Debug, PartialEq)]
    struct Retyped {
        speed: f64,
        tag: u32,
        heading: f32,
    }

    #[test]
    fn snapshot_restore_and_teardown_keep_engine_resources() {
        let mut app = App::new();
        app.insert_resource(EngineSim { token: 7 });
        let owner = TypeOwner::Game(1);
        with_game_owner(&mut app, owner, |app| {
            app.register_type::<Bag>();
            app.register_type::<BagStore>();
            app.insert_resource(BagStore { speed: 4.5, tag: 9 });
            app.world_mut().spawn(Bag { speed: 1.5, tag: 3 });
            app.add_systems(Update, |mut bag: ResMut<BagStore>| bag.tag += 1);
        });
        app.update();
        let snapshot = take_snapshot(app.world(), owner);
        assert!(
            snapshot
                .resources
                .iter()
                .any(|resource| resource.type_path.ends_with("BagStore")),
            "{snapshot:?}"
        );
        assert_eq!(snapshot.components.len(), 1);

        let report = teardown_owner(app.world_mut(), owner);
        assert!(report.systems_removed > 0, "{report:?}");
        assert_eq!(report.leftover_instances, 0, "{report:?}");
        assert_eq!(app.world().resource::<TypeOwnership>().owned_count(owner), 0);
        assert_eq!(app.world().resource::<EngineSim>().token, 7);

        with_game_owner(&mut app, owner, |app| {
            app.register_type::<Bag>();
            app.register_type::<BagStore>();
            app.insert_resource(BagStore::default());
        });
        let restored = restore_snapshot(app.world_mut(), &snapshot);
        assert!(restored.components >= 1, "{restored:?}");
        assert!(restored.resources >= 1, "{restored:?}");
        let number = reflected_number(app.world(), snapshot.components[0].type_path.as_str(), "speed");
        assert_eq!(number, Some(1.5));
        assert_eq!(app.world().resource::<EngineSim>().token, 7);
    }

    /// Component rows store `Entity::to_bits`. A later entity, even one that reused the index,
    /// does not receive the component, and the miss counts in `dropped_values`.
    #[test]
    fn component_keys_are_entity_bits_and_a_new_entity_is_not_restored() {
        let mut app = App::new();
        let owner = TypeOwner::Game(4);
        let mut spawned = None;
        with_game_owner(&mut app, owner, |app| {
            app.register_type::<Bag>();
            spawned = Some(app.world_mut().spawn(Bag { speed: 1.5, tag: 3 }).id());
        });
        let entity = spawned.expect("spawned");
        let snapshot = take_snapshot(app.world(), owner);
        assert_eq!(snapshot.components.len(), 1);
        assert_eq!(snapshot.components[0].entity, entity.to_bits());

        app.world_mut().despawn(entity);
        let reused = app.world_mut().spawn(Bag { speed: 0.0, tag: 9 }).id();
        assert_ne!(reused.to_bits(), entity.to_bits());
        let report = restore_snapshot(app.world_mut(), &snapshot);
        assert!(report.dropped_values >= 1, "{report:?}");
        let bag = app.world().get::<Bag>(reused).expect("new entity");
        assert_eq!(bag.speed, 0.0);
        assert_eq!(bag.tag, 9);
    }

    /// A field added to a quick model between two loads takes its type's `Default`; the fields the
    /// snapshot has keep their values. The "new" field is simulated by removing it from the RON.
    #[cfg(not(feature = "hot-dylib"))]
    #[test]
    fn a_field_added_to_a_quick_model_takes_its_default() {
        use regia::quick::{Model, Quick};
        #[derive(Reflect, Default, Clone, Debug, PartialEq)]
        struct Score {
            points: u32,
            name: String,
        }
        fn register(game: &mut App, owner: TypeOwner) {
            with_game_owner(game, owner, |app| {
                app.add_plugins(regia::quick::app(|_: &Quick| Score {
                    points: 0,
                    name: "fresh".into(),
                }));
            });
        }
        let mut game = App::new();
        register(&mut game, TypeOwner::Game(30));
        game.world_mut().resource_mut::<Model<Score>>().model.points = 5;
        let mut snapshot = take_snapshot(game.world(), TypeOwner::Game(30));
        for r in &mut snapshot.resources {
            if let FieldSnap::Struct(list) = &mut r.fields {
                for f in list.iter_mut().filter(|f| f.name == "model") {
                    assert!(f.ron.contains("name: \"fresh\","), "{}", f.ron);
                    f.ron = f.ron.replace("name: \"fresh\",", "");
                }
            }
        }
        teardown_owner(game.world_mut(), TypeOwner::Game(30));
        register(&mut game, TypeOwner::Game(31));
        let report = restore_snapshot(game.world_mut(), &snapshot);
        assert!(report.resources >= 1, "{report:?}");
        let score = &game.world().resource::<Model<Score>>().model;
        assert_eq!(score.points, 5, "kept");
        assert_eq!(
            score.name, "",
            "the field missing from the snapshot takes Default"
        );
    }

    /// Quick-app lane acceptance: a quick app's reflected model (`regia::quick::Model<M>`)
    /// survives the snapshot → teardown → re-register → restore sequence, and the re-registered
    /// `update` keeps advancing it.
    #[cfg(not(feature = "hot-dylib"))]
    #[test]
    fn a_quick_app_model_survives_a_reload() {
        use regia::quick::{Model, Quick};

        #[derive(Reflect, Default, Clone, Debug, PartialEq)]
        struct Score {
            points: u32,
            name: String,
        }

        // What the game library's `register` does, once per load.
        fn register(game: &mut App, owner: TypeOwner) {
            with_game_owner(game, owner, |app| {
                app.add_plugins(
                    regia::quick::app(|_: &Quick| Score {
                        points: 0,
                        name: "fresh".into(),
                    })
                    .update(|_: &Quick, score: &mut Score| score.points += 1),
                );
            });
        }

        let mut game = App::new();
        let owner = TypeOwner::Game(3);
        register(&mut game, owner);
        for _ in 0..5 {
            game.update();
        }
        game.world_mut().resource_mut::<Model<Score>>().model.name = "edited".into();

        let snapshot = take_snapshot(game.world(), owner);
        assert!(
            snapshot.resources.iter().any(|r| r.type_path.contains("Model<")),
            "{snapshot:?}"
        );
        let report = teardown_owner(game.world_mut(), owner);
        assert_eq!(report.leftover_instances, 0, "{report:?}");
        assert!(game.world().get_resource::<Model<Score>>().is_none());

        // The host registers each load under a new owner (`TypeOwner::Game(generation)`).
        register(&mut game, TypeOwner::Game(4));
        assert_eq!(
            game.world().resource::<Model<Score>>().model.points,
            0,
            "model() ran again"
        );
        let restored = restore_snapshot(game.world_mut(), &snapshot);
        assert!(restored.resources >= 1, "{restored:?}");
        let score = &game.world().resource::<Model<Score>>().model;
        assert_eq!((score.points, score.name.as_str()), (5, "edited"));
        game.update();
        assert_eq!(
            game.world().resource::<Model<Score>>().model.points,
            6,
            "the new update runs"
        );
    }

    #[test]
    fn identity_check_flags_a_type_id_that_already_has_a_component() {
        let mut app = App::new();
        let owner = TypeOwner::Game(2);
        let before = component_type_ids(app.world());
        with_game_owner(&mut app, owner, |app| {
            app.register_type::<Bag>();
            app.world_mut().spawn(Bag::default());
        });
        assert!(
            identity_clashes(app.world(), owner, &before).is_empty(),
            "the first registration is new"
        );
        let after = component_type_ids(app.world());
        let again = identity_clashes(app.world(), owner, &after);
        assert!(
            again.iter().any(|clash| clash.type_path.ends_with("Bag")),
            "{again:?}"
        );
    }

    #[test]
    fn retyped_field_keeps_default_and_new_field_defaults() {
        let mut registry = TypeRegistry::new();
        registry.register::<f64>();
        registry.register::<u32>();
        registry.register::<f32>();
        registry.register::<Retyped>();
        let fields = FieldSnap::Struct(vec![
            FieldRecord {
                name: "speed".to_string(),
                type_path: "f32".to_string(),
                ron: "4.5".to_string(),
            },
            FieldRecord {
                name: "tag".to_string(),
                type_path: "u32".to_string(),
                ron: "9".to_string(),
            },
            FieldRecord {
                name: "gone".to_string(),
                type_path: "u32".to_string(),
                ron: "1".to_string(),
            },
        ]);
        let mut value = Retyped::default();
        let mut report = RestoreReport::default();
        apply_fields(value.as_partial_reflect_mut(), &fields, &registry, &mut report);
        assert_eq!(value.speed, 0.0, "retyped field stays at Default");
        assert_eq!(value.tag, 9);
        assert_eq!(value.heading, 0.0, "new field stays at Default");
        assert_eq!(report.retyped_fields, 1);
        assert_eq!(report.dropped_fields, 1);
        assert_eq!(report.defaulted_fields, 1);
    }

    /// Specs/20 § 3 step 9: a type that disappeared from the new library is dropped (one
    /// `dropped_values` per instance), and the types that are still there restore normally.
    #[test]
    fn a_type_that_disappeared_is_dropped_and_counted() {
        let mut app = App::new();
        let old = TypeOwner::Game(6);
        with_game_owner(&mut app, old, |app| {
            app.register_type::<Bag>();
            app.register_type::<BagStore>();
            app.insert_resource(BagStore { speed: 2.5, tag: 4 });
            app.world_mut().spawn(Bag { speed: 1.0, tag: 1 });
            app.world_mut().spawn(Bag { speed: 2.0, tag: 2 });
        });
        let snapshot = take_snapshot(app.world(), old);
        assert_eq!(snapshot.components.len(), 2, "{snapshot:?}");
        assert_eq!(snapshot.resources.len(), 1, "{snapshot:?}");
        teardown_owner(app.world_mut(), old);

        // The new library still has `BagStore` but no longer defines `Bag`.
        with_game_owner(&mut app, TypeOwner::Game(7), |app| {
            app.register_type::<BagStore>();
            app.insert_resource(BagStore::default());
        });
        let report = restore_snapshot(app.world_mut(), &snapshot);
        assert_eq!(report.dropped_values, 2, "both Bag instances dropped: {report:?}");
        assert_eq!(report.components, 0, "{report:?}");
        assert_eq!(report.resources, 1, "{report:?}");
        assert_eq!(
            *app.world().resource::<BagStore>(),
            BagStore { speed: 2.5, tag: 4 }
        );
    }

    /// Specs/20 § 6: the identity check refuses an unsalted retype. Without the metadata salt
    /// the rebuilt type keeps its `TypeId`, which bevy_ecs already mapped to a `ComponentId`;
    /// registering it again under the next owner is a clash, while a type new to this world
    /// (what the salt produces) is not.
    #[test]
    fn identity_check_refuses_an_unsalted_type_across_owners() {
        #[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
        #[reflect(Component, Default)]
        struct Salted {
            speed: f64,
        }
        let mut app = App::new();
        let first = TypeOwner::Game(8);
        with_game_owner(&mut app, first, |app| {
            app.register_type::<Bag>();
            app.world_mut().spawn(Bag::default());
        });
        teardown_owner(app.world_mut(), first);

        let second = TypeOwner::Game(9);
        let before = component_type_ids(app.world());
        with_game_owner(&mut app, second, |app| {
            app.register_type::<Bag>();
            app.register_type::<Salted>();
            app.world_mut().spawn((Bag::default(), Salted::default()));
        });
        let clashes = identity_clashes(app.world(), second, &before);
        assert_eq!(clashes.len(), 1, "{clashes:?}");
        assert!(clashes[0].type_path.ends_with("Bag"), "{clashes:?}");
    }

    /// Script stand-ins for one game library built twice. `BlinkV1` and `BlinkV2` share the type
    /// path `game::Blink`, as a salted rebuild does (same path, new `TypeId`); only the body of
    /// `update` differs (`stamp` 1 versus 2). `GoneV1` exists only in the first build.
    mod scripts_fixture {
        use crate::engine::app::prelude::*;
        use regia_framework::script::{
            ReflectScript, Script, ScriptCtx, downcast_reflect, downcast_reflect_mut,
        };

        /// Engine-owned probe the scripts write each tick, so a test sees which code ran.
        #[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Probe {
            pub stamp: u32,
            pub count: u32,
            pub gone_ticks: u32,
        }

        #[derive(Reflect, Default, Clone, Debug)]
        pub struct BlinkV1 {
            pub count: u32,
            pub stamp: u32,
        }

        #[derive(Reflect, Default, Clone, Debug)]
        pub struct BlinkV2 {
            pub count: u32,
            pub stamp: u32,
        }

        #[derive(Reflect, Default, Clone, Debug)]
        pub struct GoneV1 {
            pub ticks: u32,
        }

        impl Script for BlinkV1 {
            fn update(&mut self, ctx: &mut ScriptCtx) {
                self.count += 1;
                self.stamp = 1;
                let (count, stamp) = (self.count, self.stamp);
                let mut probe = ctx.world().get_resource_or_init::<Probe>();
                probe.count = count;
                probe.stamp = stamp;
            }
        }

        impl Script for BlinkV2 {
            fn update(&mut self, ctx: &mut ScriptCtx) {
                self.count += 1;
                self.stamp = 2;
                let (count, stamp) = (self.count, self.stamp);
                let mut probe = ctx.world().get_resource_or_init::<Probe>();
                probe.count = count;
                probe.stamp = stamp;
            }
        }

        impl Script for GoneV1 {
            fn update(&mut self, ctx: &mut ScriptCtx) {
                self.ticks += 1;
                ctx.world().get_resource_or_init::<Probe>().gone_ticks += 1;
            }
        }

        impl ReflectScript for BlinkV1 {
            fn construct() -> Box<dyn Script> {
                Box::new(Self::default())
            }
            fn order() -> i32 {
                0
            }
            fn script_type_path() -> &'static str {
                "game::Blink"
            }
            fn reflect_script(script: &dyn Script) -> Option<&dyn PartialReflect> {
                downcast_reflect::<Self>(script)
            }
            fn reflect_script_mut(script: &mut dyn Script) -> Option<&mut dyn PartialReflect> {
                downcast_reflect_mut::<Self>(script)
            }
        }

        impl ReflectScript for BlinkV2 {
            fn construct() -> Box<dyn Script> {
                Box::new(Self::default())
            }
            fn order() -> i32 {
                0
            }
            fn script_type_path() -> &'static str {
                "game::Blink"
            }
            fn reflect_script(script: &dyn Script) -> Option<&dyn PartialReflect> {
                downcast_reflect::<Self>(script)
            }
            fn reflect_script_mut(script: &mut dyn Script) -> Option<&mut dyn PartialReflect> {
                downcast_reflect_mut::<Self>(script)
            }
        }

        impl ReflectScript for GoneV1 {
            fn construct() -> Box<dyn Script> {
                Box::new(Self::default())
            }
            fn order() -> i32 {
                0
            }
            fn script_type_path() -> &'static str {
                "game::Gone"
            }
            fn reflect_script(script: &dyn Script) -> Option<&dyn PartialReflect> {
                downcast_reflect::<Self>(script)
            }
            fn reflect_script_mut(script: &mut dyn Script) -> Option<&mut dyn PartialReflect> {
                downcast_reflect_mut::<Self>(script)
            }
        }
    }

    /// Spec 20 § 3.5/3.6/3.9 acceptance in process: after snapshot → teardown → register (new
    /// build) → restore, the script's next tick runs the new body, its reflected fields carry
    /// over, the old build's registry entries are gone, and a slot whose type disappeared is
    /// dropped. Outcomes are compared as one tuple so a failure prints all of them.
    #[test]
    fn a_reload_rebuilds_script_slots_from_the_new_build() {
        use regia_framework::script::{RegisterScriptExt, ScriptPlugin, ScriptRegistry, Scripts};
        use scripts_fixture::{BlinkV1, BlinkV2, GoneV1, Probe};

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ScriptPlugin));
        let first = TypeOwner::Game(1);
        with_game_owner(&mut app, first, |app| {
            app.register_type::<BlinkV1>()
                .register_type::<GoneV1>()
                .register_script::<BlinkV1>()
                .register_script::<GoneV1>();
        });
        let (blink, gone) = {
            let registry = app.world().resource::<ScriptRegistry>().clone();
            let mut blink = Scripts::default();
            assert!(blink.push_registered("game::Blink", &registry));
            let mut gone = Scripts::default();
            assert!(gone.push_registered("game::Gone", &registry));
            (
                app.world_mut().spawn(blink).id(),
                app.world_mut().spawn(gone).id(),
            )
        };
        for _ in 0..3 {
            app.update();
        }
        let before = *app.world().resource::<Probe>();
        assert_eq!((before.stamp, before.count, before.gone_ticks), (1, 3, 3));
        let entries_before = app.world().resource::<ScriptRegistry>().iter().count();

        let snapshot = take_snapshot(app.world(), first);
        let teardown = teardown_owner(app.world_mut(), first);
        let entries_after_teardown = app.world().resource::<ScriptRegistry>().iter().count();

        let second = TypeOwner::Game(2);
        with_game_owner(&mut app, second, |app| {
            app.register_type::<BlinkV2>().register_script::<BlinkV2>();
        });
        let restored = restore_snapshot(app.world_mut(), &snapshot);
        app.update();
        let after = *app.world().resource::<Probe>();
        let gone_slots = app.world().get::<Scripts>(gone).map_or(0, |s| s.0.len());
        let blink_slots = app.world().get::<Scripts>(blink).map_or(0, |s| s.0.len());
        let entries_after = app.world().resource::<ScriptRegistry>().iter().count();
        assert_eq!(
            (
                entries_before,
                entries_after_teardown,
                entries_after,
                after.stamp,
                after.count,
                after.gone_ticks,
                blink_slots,
                gone_slots,
            ),
            (2, 0, 1, 2, 4, 3, 1, 0),
            "(registry entries before / after teardown / after register, stamp, count, gone ticks, \
             blink slots, gone slots)"
        );
        assert_eq!(snapshot.scripts.len(), 2, "{snapshot:?}");
        assert_eq!(
            (
                teardown.script_slots_removed,
                teardown.script_registrations_removed
            ),
            (2, 2),
            "{teardown:?}"
        );
        assert_eq!(
            (restored.scripts_rebuilt, restored.scripts_dropped),
            (1, 1),
            "{restored:?}"
        );
    }

    /// A reload whose new build has the same script types leaves the script state as it was:
    /// same registry size, same slot count, and the fields continue from where they were.
    #[test]
    fn a_reload_with_the_same_scripts_is_a_no_op_for_script_state() {
        use regia_framework::script::{RegisterScriptExt, ScriptPlugin, ScriptRegistry, Scripts};
        use scripts_fixture::{BlinkV1, Probe};

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ScriptPlugin));
        let first = TypeOwner::Game(1);
        with_game_owner(&mut app, first, |app| {
            app.register_type::<BlinkV1>().register_script::<BlinkV1>();
        });
        let entity = app.world_mut().spawn(Scripts::with::<BlinkV1>()).id();
        for _ in 0..2 {
            app.update();
        }
        let entries_before = app.world().resource::<ScriptRegistry>().iter().count();
        let snapshot = take_snapshot(app.world(), first);
        teardown_owner(app.world_mut(), first);
        let second = TypeOwner::Game(2);
        with_game_owner(&mut app, second, |app| {
            app.register_type::<BlinkV1>().register_script::<BlinkV1>();
        });
        let restored = restore_snapshot(app.world_mut(), &snapshot);
        app.update();
        let probe = *app.world().resource::<Probe>();
        let slots = app.world().get::<Scripts>(entity).map_or(0, |s| s.0.len());
        let entries_after = app.world().resource::<ScriptRegistry>().iter().count();
        assert_eq!(
            (entries_before, entries_after, slots, probe.stamp, probe.count),
            (1, 1, 1, 1, 3)
        );
        assert_eq!(
            (restored.scripts_rebuilt, restored.scripts_dropped),
            (1, 0),
            "{restored:?}"
        );
    }

    #[test]
    fn snapshot_ron_round_trips_without_pointers() {
        let snapshot = HotReloadSnapshot {
            components: vec![ComponentSnap {
                entity: 11,
                type_path: "hot_fixture::Mover".to_string(),
                fields: FieldSnap::Struct(vec![FieldRecord {
                    name: "tag".to_string(),
                    type_path: "u32".to_string(),
                    ron: "9".to_string(),
                }]),
            }],
            resources: Vec::new(),
            scripts: vec![ScriptSnap {
                entity: 12,
                slot: 1,
                type_path: "hot_fixture::Blinker".to_string(),
                started: true,
                enabled: false,
                fields: Some(FieldSnap::Struct(vec![FieldRecord {
                    name: "count".to_string(),
                    type_path: "u32".to_string(),
                    ron: "4".to_string(),
                }])),
            }],
        };
        let dir = std::env::temp_dir().join(format!("regia-hot-snap-{}", std::process::id()));
        let path = dir.join("last_snapshot.bin");
        write_snapshot(&path, &snapshot).expect("write");
        let loaded = read_snapshot(&path).expect("read");
        assert_eq!(loaded, snapshot);
        let _ = std::fs::remove_dir_all(dir);
    }
}
