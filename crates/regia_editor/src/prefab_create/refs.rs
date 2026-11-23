// SPDX-License-Identifier: MPL-2.0
//! Live entity references across Create prefab (ADR-8593).
//!
//! Create prefab despawns the selected entities and spawns an instance whose prefab-internal
//! entities are new entities (each carries the member's guid as its `PrefabLink` local id). A
//! component field that holds an `Entity` (a joint's `connected`, a chase camera's `target`, a
//! field the Inspector's entity eyedropper set) would still name the despawned member. After the
//! instance is spawned, every reflected `Entity` value in the world that names a member, by any id
//! it has had, is rewritten to the entity that stands for that member now; the undo and the redo
//! do the same in the other direction. A reference to an entity outside the selection is left as
//! it is (that entity did not change).
//!
//! `ChildOf` and `Children` are never rewritten: they are relationships whose hooks keep both
//! sides in step, and the members' hierarchy is the prefab's.

use std::any::TypeId;
use std::collections::{HashMap, HashSet};

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_ecs::reflect::enums::VariantInfo;
use regia_ecs::reflect::{PartialReflect, ReflectMut, TypeInfo, TypeRegistry};
use regia_scene::prefab::PrefabLink;
use regia_scene::prelude::EntityGuid;

/// Every id a member has had, by the member's guid: the plain entity before the prefab, the
/// instance's internal entity after it, and the new ones each undo and redo spawn.
#[derive(Debug, Default, Clone)]
pub(super) struct MemberIds {
    ids: HashMap<Guid, Vec<Entity>>,
}

impl MemberIds {
    /// Notes the current plain entity of each member guid (before Create prefab, after an undo).
    pub(super) fn note_plain(&mut self, world: &mut World, guids: &[Guid]) {
        let wanted: HashSet<Guid> = guids.iter().copied().collect();
        for (entity, guid) in plain_entities(world, &wanted) {
            self.note(guid, entity);
        }
    }

    /// Notes the internal entity of each member guid under the instance `instance`.
    pub(super) fn note_internal(&mut self, world: &World, instance: Entity, guids: &[Guid]) {
        for (guid, entity) in internal_entities(world, instance, guids) {
            self.note(guid, entity);
        }
    }

    fn note(&mut self, guid: Guid, entity: Entity) {
        let ids = self.ids.entry(guid).or_default();
        if !ids.contains(&entity) {
            ids.push(entity);
        }
    }

    /// Every older id of a member → `current` (that member's entity now).
    fn map_to(&self, current: &HashMap<Guid, Entity>) -> HashMap<Entity, Entity> {
        let mut map = HashMap::new();
        for (guid, ids) in &self.ids {
            let Some(target) = current.get(guid) else {
                continue;
            };
            for id in ids {
                if id != target {
                    map.insert(*id, *target);
                }
            }
        }
        map
    }

    /// Points every reference to a member at its plain entity (after an undo).
    pub(super) fn retarget_to_plain(&mut self, world: &mut World, guids: &[Guid]) -> usize {
        self.note_plain(world, guids);
        let wanted: HashSet<Guid> = guids.iter().copied().collect();
        let current: HashMap<Guid, Entity> = plain_entities(world, &wanted)
            .into_iter()
            .map(|(entity, guid)| (guid, entity))
            .collect();
        remap_entity_fields(world, &self.map_to(&current))
    }

    /// Points every reference to a member at its internal entity under `instance`.
    pub(super) fn retarget_to_instance(
        &mut self,
        world: &mut World,
        instance: Entity,
        guids: &[Guid],
    ) -> usize {
        self.note_internal(world, instance, guids);
        let current: HashMap<Guid, Entity> = internal_entities(world, instance, guids).into_iter().collect();
        remap_entity_fields(world, &self.map_to(&current))
    }
}

/// The entities whose `EntityGuid` is in `wanted` (a member's plain entity; an internal entity of
/// an instance has an instance-derived guid, never the member's).
fn plain_entities(world: &mut World, wanted: &HashSet<Guid>) -> Vec<(Entity, Guid)> {
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut query = world.query::<(Entity, &EntityGuid)>();
    query
        .iter(world)
        .filter(|(_, guid)| wanted.contains(&guid.0))
        .map(|(entity, guid)| (entity, guid.0))
        .collect()
}

/// The prefab-internal entities of `instance` whose local id is one of `guids`.
fn internal_entities(world: &World, instance: Entity, guids: &[Guid]) -> Vec<(Guid, Entity)> {
    let wanted: HashSet<u128> = guids.iter().map(|g| g.as_u128()).collect();
    crate::outliner::subtree_of(world, instance)
        .into_iter()
        .filter_map(|entity| {
            let link = world.get::<PrefabLink>(entity)?;
            wanted
                .contains(&link.local_id)
                .then(|| (Guid::from_u128(link.local_id), entity))
        })
        .collect()
}

/// Whether a value of `type_id` can hold an `Entity` (memoized; a recursive type adds nothing the
/// outer scan does not see).
fn holds_entity(registry: &TypeRegistry, type_id: TypeId, memo: &mut HashMap<TypeId, bool>) -> bool {
    if type_id == TypeId::of::<Entity>() {
        return true;
    }
    if let Some(known) = memo.get(&type_id) {
        return *known;
    }
    memo.insert(type_id, false);
    let Some(info) = registry.get_type_info(type_id) else {
        return false;
    };
    let fields: Vec<TypeId> = match info {
        TypeInfo::Struct(s) => s.iter().map(|f| f.type_id()).collect(),
        TypeInfo::TupleStruct(s) => s.iter().map(|f| f.type_id()).collect(),
        TypeInfo::Tuple(t) => t.iter().map(|f| f.type_id()).collect(),
        TypeInfo::List(l) => vec![l.item_ty().id()],
        TypeInfo::Array(a) => vec![a.item_ty().id()],
        TypeInfo::Map(m) => vec![m.value_ty().id()],
        TypeInfo::Set(_) | TypeInfo::Opaque(_) => Vec::new(),
        TypeInfo::Enum(e) => e
            .iter()
            .flat_map(|variant| match variant {
                VariantInfo::Struct(v) => v.iter().map(|f| f.type_id()).collect::<Vec<_>>(),
                VariantInfo::Tuple(v) => v.iter().map(|f| f.type_id()).collect(),
                VariantInfo::Unit(_) => Vec::new(),
            })
            .collect(),
    };
    let holds = fields
        .into_iter()
        .any(|field| holds_entity(registry, field, memo));
    memo.insert(type_id, holds);
    holds
}

/// Rewrites every `Entity` in `value` found in `map`. Returns how many it rewrote.
fn rewrite(value: &mut dyn PartialReflect, map: &HashMap<Entity, Entity>) -> usize {
    match value.reflect_mut() {
        ReflectMut::Opaque(v) => match v.try_downcast_mut::<Entity>() {
            Some(entity) => match map.get(entity) {
                Some(to) => {
                    *entity = *to;
                    1
                }
                None => 0,
            },
            None => 0,
        },
        ReflectMut::Struct(s) => (0..s.field_len())
            .filter_map(|i| s.field_at_mut(i).map(|f| rewrite(f, map)))
            .sum(),
        ReflectMut::TupleStruct(s) => (0..s.field_len())
            .filter_map(|i| s.field_mut(i).map(|f| rewrite(f, map)))
            .sum(),
        ReflectMut::Tuple(t) => (0..t.field_len())
            .filter_map(|i| t.field_mut(i).map(|f| rewrite(f, map)))
            .sum(),
        ReflectMut::List(l) => (0..l.len())
            .filter_map(|i| l.get_mut(i).map(|f| rewrite(f, map)))
            .sum(),
        ReflectMut::Array(a) => (0..a.len())
            .filter_map(|i| a.get_mut(i).map(|f| rewrite(f, map)))
            .sum(),
        ReflectMut::Map(m) => {
            let mut count = 0;
            m.retain(&mut |_, v| {
                count += rewrite(v, map);
                true
            });
            count
        }
        ReflectMut::Set(_) => 0,
        ReflectMut::Enum(e) => (0..e.field_len())
            .filter_map(|i| e.field_at_mut(i).map(|f| rewrite(f, map)))
            .sum(),
    }
}

/// Rewrites, in every reflected component of every entity, each `Entity` value that is a key of
/// `map` to its value (`ChildOf` and `Children` excepted). A component is marked changed only when
/// one of its values moved. Returns how many values it rewrote.
pub(super) fn remap_entity_fields(world: &mut World, map: &HashMap<Entity, Entity>) -> usize {
    if map.is_empty() {
        return 0;
    }
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let skip = [TypeId::of::<ChildOf>(), TypeId::of::<Children>()];
    // Component types that can hold an entity, and the entities that have one of them.
    let mut memo = HashMap::new();
    let mut work: Vec<(Entity, ReflectComponent)> = Vec::new();
    for archetype in world.archetypes().iter() {
        for &component in archetype.components() {
            let Some(type_id) = world
                .components()
                .get_info(component)
                .and_then(|info| info.type_id())
            else {
                continue;
            };
            if skip.contains(&type_id) || !holds_entity(&registry, type_id, &mut memo) {
                continue;
            }
            let Some(reflect) = registry.get(type_id).and_then(|r| r.data::<ReflectComponent>()) else {
                continue;
            };
            for entity in archetype.entities() {
                work.push((entity.id(), reflect.clone()));
            }
        }
    }
    drop(registry);
    let mut count = 0;
    for (entity, reflect) in work {
        let Ok(entity_mut) = world.get_entity_mut(entity) else {
            continue;
        };
        let Some(mut value) = reflect.reflect_mut(entity_mut) else {
            continue;
        };
        let moved = rewrite(value.bypass_change_detection().as_partial_reflect_mut(), map);
        if moved > 0 {
            value.set_changed();
            count += moved;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Component, Reflect, Default)]
    #[reflect(Component, Default)]
    struct Holder {
        one: Option<Entity>,
        many: Vec<Entity>,
        plain: f32,
    }

    #[derive(Component, Reflect, Default)]
    #[reflect(Component, Default)]
    struct NoRefs {
        value: f32,
    }

    #[test]
    fn only_types_that_reach_an_entity_are_walked_and_mapped_values_move() {
        let mut world = World::new();
        world.init_resource::<AppTypeRegistry>();
        {
            let registry = world.resource::<AppTypeRegistry>().clone();
            let mut registry = registry.write();
            registry.register::<Holder>();
            registry.register::<NoRefs>();
        }
        let registry = world.resource::<AppTypeRegistry>().clone();
        let mut memo = HashMap::new();
        assert!(holds_entity(&registry.read(), TypeId::of::<Holder>(), &mut memo));
        assert!(!holds_entity(&registry.read(), TypeId::of::<NoRefs>(), &mut memo));

        let old = world.spawn_empty().id();
        let new = world.spawn_empty().id();
        let other = world.spawn_empty().id();
        let parent = world.spawn_empty().id();
        let holder = world
            .spawn((
                Holder {
                    one: Some(old),
                    many: vec![other, old],
                    plain: 1.0,
                },
                NoRefs { value: 2.0 },
                ChildOf(parent),
            ))
            .id();
        let map = HashMap::from([(old, new), (parent, other)]);
        assert_eq!(remap_entity_fields(&mut world, &map), 2);
        let h = world.get::<Holder>(holder).expect("holder");
        assert_eq!(
            (h.one, h.many.clone(), h.plain),
            (Some(new), vec![other, new], 1.0)
        );
        assert_eq!(
            world.get::<ChildOf>(holder).map(ChildOf::parent),
            Some(parent),
            "relationships are left alone"
        );
    }
}
