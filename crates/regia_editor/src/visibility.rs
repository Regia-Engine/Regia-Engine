// SPDX-License-Identifier: MPL-2.0
//! Editor-internal entities: which entities the outliner, picking and selection treat as
//! bookkeeping rather than level content (`docs/specs/19-editor.md` § 2).
//!
//! The edit world hosts a few entities that exist for plumbing (the primary-window marker,
//! protocol bookkeeping). They must never appear in the Outliner and never become selected.
//! The rule is a marker component, [`EditorHidden`]; [`mark_internal_entities`] applies it to
//! every entity that carries no user-visible content (no guid, no name, no `Transform`), so the
//! outliner can simply list "everything not hidden" — user entities created through any path
//! (editor spawn, level load, agent `scene.spawn`) always carry at least one of those.

use regia_ecs::ecs::archetype::Archetype;
use regia_ecs::ecs::component::ComponentId;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;

use regia_scene::prelude::{EntityGuid, Transform};

thread_local! {
    static VISITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn count_visit() {
    VISITS.with(|visits| visits.set(visits.get() + 1));
}

/// How many entities the identity walks ([`mark_internal_entities`], [`ensure_scene_identity`],
/// [`ensure_snapshot_identity`]) visited on the calling thread: a counter for tests (a steady
/// editor frame must visit none, ADR-7326).
#[must_use]
pub fn identity_walk_visits() -> u64 {
    VISITS.with(std::cell::Cell::get)
}

/// Marker for editor-internal entities (windows, protocol bookkeeping): hidden from the
/// outliner, skipped by picking, never selectable.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EditorHidden;

/// Whether `entity` is editor-internal (hidden from the outliner and selection).
#[must_use]
pub fn is_hidden(world: &World, entity: Entity) -> bool {
    world.get::<EditorHidden>(entity).is_some()
}

/// Whether `entity` carries any user-visible content (a name, a guid, or a transform).
///
/// Editor-internal entities — window markers, protocol bookkeeping — carry none of these, which
/// is what [`mark_internal_entities`] keys off.
#[must_use]
pub fn is_user_entity(world: &World, entity: Entity) -> bool {
    world.get::<EntityGuid>(entity).is_some()
        || world.get::<regia_scene::prelude::Name>(entity).is_some()
        || world.get::<regia_ecs::ecs::name::Name>(entity).is_some()
        || world.get::<Transform>(entity).is_some()
}

/// Marks every content-less entity as [`EditorHidden`] and returns how many entities were
/// marked. Idempotent: already-hidden entities are left alone. Run it whenever bookkeeping
/// entities may have appeared (once after startup is enough for M10; the editor calls it from
/// the shell's per-frame effects to stay correct against future protocol spawns).
///
/// The rule depends only on which components an entity has, so it is decided per archetype and
/// only the entities of matching archetypes are visited: a frame where nothing new appeared
/// visits none (ADR-7326; the cost is one look at each archetype).
pub fn mark_internal_entities(world: &mut World) -> usize {
    let ids = ContentIds::of(world);
    let to_mark = entities_where(world, false, |archetype| {
        !ids.user(archetype) && !ContentIds::has(archetype, ids.hidden)
    });
    let mut marked = 0usize;
    for entity in to_mark {
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            entry.insert(EditorHidden);
            marked += 1;
        }
    }
    marked
}

/// The ids of the components the identity rules look at (`None`: no entity has one yet).
struct ContentIds {
    guid: Option<ComponentId>,
    scene_name: Option<ComponentId>,
    ecs_name: Option<ComponentId>,
    transform: Option<ComponentId>,
    hidden: Option<ComponentId>,
}

impl ContentIds {
    fn of(world: &World) -> Self {
        Self {
            guid: world.component_id::<EntityGuid>(),
            scene_name: world.component_id::<regia_scene::prelude::Name>(),
            ecs_name: world.component_id::<regia_ecs::ecs::name::Name>(),
            transform: world.component_id::<Transform>(),
            hidden: world.component_id::<EditorHidden>(),
        }
    }

    fn has(archetype: &Archetype, id: Option<ComponentId>) -> bool {
        id.is_some_and(|id| archetype.contains(id))
    }

    /// [`is_user_entity`] for every entity of `archetype`.
    fn user(&self, archetype: &Archetype) -> bool {
        Self::has(archetype, self.guid)
            || Self::has(archetype, self.scene_name)
            || Self::has(archetype, self.ecs_name)
            || Self::has(archetype, self.transform)
    }
}

/// The entities of every non-empty archetype `wanted` accepts, each counted as visited. With
/// `skip_disabled`, archetypes holding a disabling component (`Disabled`) are left out, as a
/// query leaves them out.
fn entities_where(world: &World, skip_disabled: bool, wanted: impl Fn(&Archetype) -> bool) -> Vec<Entity> {
    let disabling: Vec<ComponentId> = if skip_disabled {
        world
            .get_resource::<regia_ecs::ecs::entity_disabling::DefaultQueryFilters>()
            .map(|filters| filters.disabling_ids().collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut out = Vec::new();
    for archetype in world.archetypes().iter() {
        if archetype.is_empty() || disabling.iter().any(|id| archetype.contains(*id)) || !wanted(archetype) {
            continue;
        }
        for entity in archetype.entities() {
            count_visit();
            out.push(entity.id());
        }
    }
    out
}

/// Gives every *user* entity that lacks one a fresh [`EntityGuid`] (`specs/14` A1: stable scene
/// identity), and returns how many were added.
///
/// Saving and play-in-editor snapshot through `regia_scene::snapshot_scene`, which addresses
/// entities by their guid — a guid-less entity would silently vanish from saves and the play
/// world. The runtime is free to spawn transient entities without guids; the *editor* guarantees
/// that authored content always carries identity, whichever path created it (panel actions,
/// drag-and-drop, the agent protocol's `scene.spawn`). Editor-internal entities
/// ([`is_user_entity`]) are never touched.
///
/// Also copies names between `regia_scene`'s `Name` (what level and prefab files store, and
/// what the snapshot writes) and the ECS `Name` (what `scene.spawn` inserts and what
/// `scene.query_entities` reads). Filling only the missing side keeps both views of one entity
/// addressable without overwriting a name the user set.
pub fn ensure_scene_identity(world: &mut World) -> usize {
    let tagged = tag_guids(world);
    sync_names(world, true);
    tagged
}

/// What a snapshot needs and nothing more: a guid on every user entity and the scene `Name`
/// (what the snapshot writes) where only the ECS `Name` is set. Unlike
/// [`ensure_scene_identity`] it does not add the ECS `Name`: play-in-editor calls this before
/// its snapshot, and every component it inserts moves an edit entity to another table. That
/// changes the order the next save writes, so stopping play would not give back the same scene
/// (`tests/import_flow.rs`: the swap appeared once one more resource existed).
pub fn ensure_snapshot_identity(world: &mut World) -> usize {
    let tagged = tag_guids(world);
    sync_names(world, false);
    tagged
}

/// Whether `entity` or one of its ancestors is something a scene file never holds as an
/// ordinary record: editor-internal ([`EditorHidden`]), kept across levels (`Persistent`), a
/// prefab-internal part (`PrefabLink`), procedural output (`Generated`), DCC feed data
/// (`FeedOwned`) or a placed level's member (`PlacedMember`).
fn outside_scene_content(world: &World, entity: Entity) -> bool {
    use regia_scene::components::{FeedOwned, Generated};
    let mut at = Some(entity);
    // Bounded: a hierarchy cycle (never valid) cannot loop forever.
    for _ in 0..4096 {
        let Some(current) = at else {
            return false;
        };
        if world.get::<EditorHidden>(current).is_some()
            || world.get::<regia_scene::level::Persistent>(current).is_some()
            || world.get::<regia_scene::prefab::PrefabLink>(current).is_some()
            || world.get::<Generated>(current).is_some()
            || world.get::<FeedOwned>(current).is_some()
            || world.get::<regia_scene::placed::PlacedMember>(current).is_some()
        {
            return true;
        }
        at = world.get::<ChildOf>(current).map(ChildOf::parent);
    }
    true
}

/// Adopts the user entities that belong to no level into `level` before a level save or the
/// level autosave snapshots it (ADR-7382), and returns how many were adopted.
///
/// The runtime methods (`scene.spawn`, `scene.spawn_primitive`) spawn entities with neither a
/// guid nor a `LevelMember`, and `regia_scene::level::snapshot_level` writes only the open
/// level's members with a guid, so such an entity was queryable and shown but never saved. This
/// gives every user entity its identity ([`ensure_snapshot_identity`], the snapshot-only variant:
/// a save then a second save stay byte-identical, see its table-order note) and makes the ones
/// outside every level members of `level`. Skipped: entities of another level, everything a scene
/// file never holds as a record (editor-internal, persistent, prefab-internal, generated, feed,
/// placed-level members, and their descendants) and `keep` (documents of other editors that live
/// in the edit world, such as the sequence players the Sequence Curves panel opened).
pub fn adopt_into_level(world: &mut World, level: regia_scene::prelude::LevelId, keep: &[Entity]) -> usize {
    ensure_snapshot_identity(world);
    let mut query =
        world.query_filtered::<Entity, (With<EntityGuid>, Without<regia_scene::prelude::LevelMember>)>();
    let candidates: Vec<Entity> = query.iter(world).collect();
    let orphans: Vec<Entity> = candidates
        .into_iter()
        .filter(|entity| {
            is_user_entity(world, *entity) && !keep.contains(entity) && !outside_scene_content(world, *entity)
        })
        .collect();
    for entity in &orphans {
        if let Ok(mut entry) = world.get_entity_mut(*entity) {
            entry.insert(regia_scene::prelude::LevelMember::from_level(level));
        }
    }
    orphans.len()
}

/// The entities a whole-scene snapshot (`regia_scene::snapshot_scene` with no level filter)
/// writes as records: every entity with a guid except prefab-internal parts, procedural output,
/// DCC feed data and placed-level members (the same rules), minus `keep`. Prefab edit mode clears
/// exactly these when it switches scenes, so nothing its stash captured survives beside the
/// restore, and nothing of a prefab session survives its close.
#[must_use]
pub fn scene_content(world: &mut World, keep: &[Entity]) -> Vec<Entity> {
    use regia_scene::components::{FeedOwned, Generated};
    let mut query = world.query_filtered::<Entity, (
        With<EntityGuid>,
        Without<regia_scene::prefab::PrefabLink>,
        Without<Generated>,
        Without<FeedOwned>,
        Without<regia_scene::placed::PlacedMember>,
    )>();
    let all: Vec<Entity> = query.iter(world).collect();
    all.into_iter().filter(|entity| !keep.contains(entity)).collect()
}

fn tag_guids(world: &mut World) -> usize {
    let ids = ContentIds::of(world);
    let to_tag = entities_where(world, false, |archetype| {
        ids.user(archetype) && !ContentIds::has(archetype, ids.guid)
    });
    let mut tagged = 0usize;
    for entity in to_tag {
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            entry.insert(EntityGuid::new_random());
            tagged += 1;
        }
    }
    tagged
}

/// Fills a missing scene `Name` from the ECS `Name`, and (with `fill_ecs`) the other way around,
/// for user entities.
fn sync_names(world: &mut World, fill_ecs: bool) {
    // A named entity is a user entity, so only the two names decide; a query would skip
    // disabled entities, and so does this.
    let ids = ContentIds::of(world);
    let scene_only: Vec<(Entity, String)> = if fill_ecs {
        entities_where(world, true, |archetype| {
            ContentIds::has(archetype, ids.scene_name) && !ContentIds::has(archetype, ids.ecs_name)
        })
        .into_iter()
        .filter_map(|entity| {
            let name = world.get::<regia_scene::prelude::Name>(entity)?;
            Some((entity, name.as_str().to_string()))
        })
        .collect()
    } else {
        Vec::new()
    };
    let ecs_only: Vec<(Entity, String)> = entities_where(world, true, |archetype| {
        ContentIds::has(archetype, ids.ecs_name) && !ContentIds::has(archetype, ids.scene_name)
    })
    .into_iter()
    .filter_map(|entity| {
        let name = world.get::<regia_ecs::ecs::name::Name>(entity)?;
        Some((entity, name.as_str().to_string()))
    })
    .collect();
    for (entity, name) in scene_only {
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            entry.insert(regia_ecs::ecs::name::Name::new(name));
        }
    }
    for (entity, name) in ecs_only {
        if let Ok(mut entry) = world.get_entity_mut(entity) {
            entry.insert(regia_scene::prelude::Name::new(name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::prelude::Name;

    #[test]
    fn contentless_entities_get_marked_content_entities_do_not() {
        let mut world = World::new();
        let window = world.spawn(regia_platform::PrimaryWindow).id();
        let named = world
            .spawn((
                regia_ecs::ecs::name::Name::new("Agent spawn"),
                Transform::default(),
            ))
            .id();
        let guided_guid = EntityGuid::new_random().0;
        let guided = world
            .spawn((Name::new("Level content"), EntityGuid(guided_guid)))
            .id();

        let marked = mark_internal_entities(&mut world);
        assert!(
            marked >= 1,
            "the content-less window entity (plus any ECS bookkeeping) is marked"
        );
        assert!(is_hidden(&world, window));
        assert!(!is_hidden(&world, named), "agent-spawned entities stay visible");
        assert!(!is_hidden(&world, guided), "level entities stay visible");

        // Idempotent.
        assert_eq!(mark_internal_entities(&mut world), 0);
    }

    #[test]
    fn user_entities_get_scene_identity_internal_entities_do_not() {
        use regia_ecs::ecs::name::Name as EcsName;

        let mut world = World::new();
        let window = world.spawn(regia_platform::PrimaryWindow).id();
        // An agent `scene.spawn` entity: a name but no guid.
        let agent_spawned = world.spawn(EcsName::new("Agent spawn")).id();
        let guided_guid = EntityGuid::new_random().0;
        let guided = world
            .spawn((Name::new("Level content"), EntityGuid(guided_guid)))
            .id();

        assert_eq!(
            ensure_scene_identity(&mut world),
            1,
            "only the guid-less user entity"
        );
        assert!(world.get::<EntityGuid>(agent_spawned).is_some(), "got identity");
        let first = world.get::<EntityGuid>(agent_spawned).expect("guid").0;
        assert!(
            world.get::<EntityGuid>(window).is_none(),
            "editor-internal entities never get identity"
        );
        assert_eq!(
            world.get::<EntityGuid>(guided).expect("guid").0,
            guided_guid,
            "existing guids are untouched"
        );

        // Idempotent.
        assert_eq!(ensure_scene_identity(&mut world), 0);
        assert_eq!(
            world.get::<EntityGuid>(agent_spawned).expect("guid").0,
            first,
            "the guid is stable across sweeps"
        );
        // The sweep also mirrors names so the agent query and the scene file agree.
        assert_eq!(
            world.get::<Name>(agent_spawned).expect("scene name").as_str(),
            "Agent spawn"
        );
        assert_eq!(
            world.get::<EcsName>(guided).expect("ecs name").as_str(),
            "Level content"
        );
    }

    #[test]
    fn user_entity_check_covers_all_creation_paths() {
        let mut world = World::new();
        let empty = world.spawn_empty().id();
        let ecs_named = world.spawn(regia_ecs::ecs::name::Name::new("x")).id();
        let scene_named = world.spawn(Name::new("x")).id();
        let transformed = world.spawn(Transform::default()).id();
        let guided = world.spawn(EntityGuid::new_random()).id();

        assert!(!is_user_entity(&world, empty));
        assert!(is_user_entity(&world, ecs_named), "agent scene.spawn path");
        assert!(is_user_entity(&world, scene_named), "editor spawn path");
        assert!(is_user_entity(&world, transformed), "prefab drop path");
        assert!(is_user_entity(&world, guided), "level load path");
    }
}
