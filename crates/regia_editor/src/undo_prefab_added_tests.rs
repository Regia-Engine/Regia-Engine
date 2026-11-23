// SPDX-License-Identifier: MPL-2.0
//! Undo, duplicate and prefab respawn of entities added on top of a prefab instance
//! (v0.5.0-rc1 editor-undo review, findings 1-3; ADR-7412).
//!
//! The level here holds one prefab instance (`Inst`) whose template has a root and a child
//! (`Child`, local id `L_CHILD`). The level added `Added` under that internal child and
//! `AddedKid` under `Added` (both live in the instance record's `added_children`), plus an
//! added component on the internal child. Every test checks the property spec 19 § 1 states:
//! undo returns a byte-identical scene save.

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::prefab::{PrefabLibrary, PrefabLink, PrefabTemplate};
use regia_scene::prelude::{EntityGuid, Name, SceneFile, SpawnOptions, spawn_scene};
use regia_scene::spawn::SnapshotFilter;

use crate::command::{redo_world, undo_world};

/// A component the level adds to a prefab-internal entity (`added_components`).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct AddedMarker {
    value: f32,
}

const ASSET: &str = "5e1f0000-0000-4000-8000-0000000000aa";
const L_ROOT: &str = "5e1f0000-0000-4000-8000-000000000001";
const L_CHILD: &str = "5e1f0000-0000-4000-8000-000000000002";
const INST: &str = "5e1f0000-0000-4000-8000-000000000101";
const ADDED: &str = "5e1f0000-0000-4000-8000-000000000102";
const ADDED_KID: &str = "5e1f0000-0000-4000-8000-000000000103";
const PLAIN: &str = "5e1f0000-0000-4000-8000-000000000104";

fn guid(text: &str) -> Guid {
    text.parse().expect("test guid")
}

/// The prefab template file; `child_y` is the internal child's height (a template edit).
fn template_file(child_y: f32) -> SceneFile {
    SceneFile::from_ron_str(&format!(
        r#"( format_version: 1, entities: [
            ( guid: "{L_ROOT}", name: "Root", components: {{ "regia_scene::transform::Transform": () }} ),
            ( guid: "{L_CHILD}", name: "Child", parent: Some("{L_ROOT}"),
              components: {{ "regia_scene::transform::Transform": ( translation: (0.0, {child_y:?}, 0.0) ) }} ),
        ] )"#
    ))
    .expect("template RON")
}

fn install_template(world: &mut World, child_y: f32) {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let template = PrefabTemplate::from_scene_file(guid(ASSET), &template_file(child_y), &registry.read())
        .expect("bake");
    world
        .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
        .insert(template);
}

/// The open level the fixture spawned into.
#[derive(Resource, Clone, Copy)]
struct TestLevel(regia_scene::prelude::LevelId);

/// An app whose open level holds the instance with its added children, and a plain entity.
fn app() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<AddedMarker>();
    let open = crate::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(TestLevel(open.id));
    install_template(app.world_mut(), 1.0);
    let marker = std::any::type_name::<AddedMarker>();
    let level = SceneFile::from_ron_str(&format!(
        r#"( format_version: 1, entities: [
            ( guid: "{INST}", name: "Inst",
              components: {{ "regia_scene::transform::Transform": ( translation: (5.0, 0.0, 0.0) ) }},
              prefab_instance: Some((
                prefab: "asset:{ASSET}",
                overrides: [],
                added_components: [ ( target: "{L_CHILD}", component: "{marker}", value: ( value: 3.0 ) ) ],
                removed_components: [],
                added_children: [
                  ( guid: "{ADDED}", name: "Added", parent: Some("{L_CHILD}"),
                    components: {{ "regia_scene::transform::Transform": ( translation: (0.0, 0.0, 2.0) ) }} ),
                  ( guid: "{ADDED_KID}", name: "AddedKid", parent: Some("{ADDED}"),
                    components: {{ "regia_scene::transform::Transform": () }} ),
                ],
                removed_children: [],
              )) ),
            ( guid: "{PLAIN}", name: "Plain", components: {{ "regia_scene::transform::Transform": () }} ),
        ] )"#
    ))
    .expect("level RON");
    let opts = SpawnOptions {
        level: Some(open.id),
        ..SpawnOptions::default()
    };
    spawn_scene(app.world_mut(), &level, &opts).expect("spawn level");
    app
}

/// The level save (the bytes undo must give back): the open level's scene, which keeps only
/// entities that are members of the level.
fn save(world: &mut World) -> String {
    let level = world.resource::<TestLevel>().0;
    regia_scene::level::snapshot_level(world, level)
        .expect("snapshot")
        .scene
        .to_ron_string()
        .expect("ron")
}

fn by_guid(world: &mut World, wanted: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == wanted)
        .map(|(e, _)| e)
}

fn named(world: &mut World, name: &str) -> Vec<Entity> {
    world
        .query::<(Entity, &Name)>()
        .iter(world)
        .filter(|(_, n)| n.as_str() == name)
        .map(|(e, _)| e)
        .collect()
}

fn parent_of(world: &World, entity: Entity) -> Option<Entity> {
    world.get::<ChildOf>(entity).map(ChildOf::parent)
}

/// The prefab-internal entity with `local` id under the instance root `root`.
fn internal_under(world: &mut World, root: Entity, local: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &PrefabLink)>()
        .iter(world)
        .filter(|(_, link)| link.guid() == local)
        .map(|(e, _)| e)
        .find(|e| {
            let mut cursor = *e;
            while let Some(parent) = world.get::<ChildOf>(cursor).map(ChildOf::parent) {
                if parent == root {
                    return true;
                }
                cursor = parent;
            }
            false
        })
}

#[test]
fn deleting_an_added_child_of_an_instance_undoes_to_identical_bytes() {
    let mut app = app();
    let world = app.world_mut();
    let before = save(world);
    assert!(
        before.contains("AddedKid"),
        "the fixture nests the added children: {before}"
    );

    let added = by_guid(world, guid(ADDED)).expect("Added");
    crate::hierarchy_ops::delete_entities(world, &[added]).expect("delete");
    assert!(by_guid(world, guid(ADDED)).is_none(), "Added is gone");
    assert!(
        by_guid(world, guid(ADDED_KID)).is_none(),
        "its subtree went with it"
    );

    assert!(undo_world(world), "the delete is on the stack");
    let added = by_guid(world, guid(ADDED)).expect("undo restores Added");
    let kid = by_guid(world, guid(ADDED_KID)).expect("undo restores AddedKid");
    let inst = by_guid(world, guid(INST)).expect("instance");
    let child = internal_under(world, inst, guid(L_CHILD)).expect("internal child");
    assert_eq!(
        parent_of(world, added),
        Some(child),
        "back under the internal child"
    );
    assert_eq!(parent_of(world, kid), Some(added), "AddedKid back under Added");
    assert_eq!(save(world), before, "undo gives back the same scene save");

    // Redo deletes again; a second undo restores again.
    assert!(redo_world(world));
    assert!(by_guid(world, guid(ADDED)).is_none());
    assert!(undo_world(world));
    assert_eq!(
        save(world),
        before,
        "undo after redo gives back the same scene save"
    );
}

#[test]
fn deleting_a_nested_added_child_undoes_to_identical_bytes() {
    let mut app = app();
    let world = app.world_mut();
    let before = save(world);

    let kid = by_guid(world, guid(ADDED_KID)).expect("AddedKid");
    crate::hierarchy_ops::delete_entities(world, &[kid]).expect("delete");
    assert!(by_guid(world, guid(ADDED_KID)).is_none());
    assert!(undo_world(world));
    let kid = by_guid(world, guid(ADDED_KID)).expect("undo restores AddedKid");
    let added = by_guid(world, guid(ADDED)).expect("Added");
    assert_eq!(parent_of(world, kid), Some(added));
    assert_eq!(save(world), before);
}

#[test]
fn duplicating_an_instance_with_added_children_keeps_guids_unique() {
    let mut app = app();
    let world = app.world_mut();
    let before = save(world);

    let inst = by_guid(world, guid(INST)).expect("instance");
    let copies = crate::hierarchy_ops::duplicate_entities(world, &[inst], regia_core::math::Vec3::X)
        .expect("duplicate");
    assert_eq!(copies.len(), 1);
    // The level can still be saved: no guid is in the world twice.
    let after = regia_scene::prelude::snapshot_scene(world, &SnapshotFilter::default())
        .expect("a level with a duplicated instance still saves");
    let mut all: Vec<Guid> = world.query::<&EntityGuid>().iter(world).map(|g| g.0).collect();
    let count = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), count, "every EntityGuid in the world is unique");
    assert_eq!(named(world, "Added").len(), 2, "the copy has its own added child");
    assert_eq!(
        named(world, "AddedKid").len(),
        2,
        "and its own nested added child"
    );
    // The copy's added children hang under the copy's internal child.
    let copy_child = internal_under(world, copies[0], guid(L_CHILD)).expect("copy's internal child");
    let copy_added = named(world, "Added")
        .into_iter()
        .find(|e| parent_of(world, *e) == Some(copy_child))
        .expect("the copy's Added sits under the copy's internal child");
    assert_ne!(
        world.get::<EntityGuid>(copy_added).map(|g| g.0),
        Some(guid(ADDED))
    );
    assert_eq!(
        after.entities.len(),
        3,
        "Inst, its copy and Plain are the top-level records"
    );

    assert!(undo_world(world));
    assert_eq!(
        save(world),
        before,
        "undoing the duplicate gives back the same scene save"
    );
}

#[test]
fn duplicating_an_added_child_copies_it_under_the_same_parent() {
    let mut app = app();
    let world = app.world_mut();
    let before = save(world);

    let added = by_guid(world, guid(ADDED)).expect("Added");
    let parent = parent_of(world, added).expect("internal parent");
    let copies = crate::hierarchy_ops::duplicate_entities(world, &[added], regia_core::math::Vec3::X)
        .expect("duplicate");
    assert_eq!(copies.len(), 1, "the added child was copied");
    assert_eq!(parent_of(world, copies[0]), Some(parent), "next to its original");
    assert_ne!(world.get::<EntityGuid>(copies[0]).map(|g| g.0), Some(guid(ADDED)));
    assert_eq!(named(world, "AddedKid").len(), 2, "with its subtree");
    regia_scene::prelude::snapshot_scene(world, &SnapshotFilter::default()).expect("the level still saves");

    assert!(undo_world(world));
    assert_eq!(save(world), before);
}

/// Respawns the instances of `ASSET` from the library as it is.
fn respawn(world: &mut World) -> usize {
    let before = crate::prefab_edit::library_checkpoint(world);
    crate::prefab_edit::respawn_instances(world, &[guid(ASSET)], &before).expect("the instances respawn")
}

#[test]
fn respawning_instances_keeps_added_children_and_components() {
    let mut app = app();
    let world = app.world_mut();
    let before = save(world);

    // Same template: a respawn (live reload of an unchanged file) is invisible in the save.
    let updated = respawn(world);
    assert_eq!(updated, 1);
    assert_eq!(
        save(world),
        before,
        "a respawn from the same template changes nothing"
    );

    // A template edit: the instance updates and keeps what the level added on top of it.
    install_template(world, 4.0);
    assert_eq!(respawn(world), 1);
    let inst = by_guid(world, guid(INST)).expect("the instance kept its guid");
    let child = internal_under(world, inst, guid(L_CHILD)).expect("internal child");
    assert_eq!(
        world
            .get::<regia_scene::prelude::Transform>(child)
            .map(|t| t.translation.y),
        Some(4.0),
        "the instance updated from the new template"
    );
    assert_eq!(
        world.get::<AddedMarker>(child),
        Some(&AddedMarker { value: 3.0 }),
        "the added component survived"
    );
    let added = by_guid(world, guid(ADDED)).expect("Added survived the respawn");
    assert_eq!(parent_of(world, added), Some(child));
    let kid = by_guid(world, guid(ADDED_KID)).expect("AddedKid survived the respawn");
    assert_eq!(parent_of(world, kid), Some(added));
}

#[test]
fn respawning_an_instance_keeps_its_parent_and_undo_entries_find_it() {
    let mut app = app();
    let world = app.world_mut();
    let inst = by_guid(world, guid(INST)).expect("instance");
    let plain = by_guid(world, guid(PLAIN)).expect("Plain");
    crate::hierarchy_ops::reparent_entities(world, &[inst], Some(plain), true).expect("reparent");
    let before = save(world);

    assert_eq!(respawn(world), 1);
    let inst = by_guid(world, guid(INST)).expect("the instance kept its guid");
    assert_eq!(
        parent_of(world, inst),
        Some(plain),
        "the instance kept its parent"
    );
    assert_eq!(save(world), before);

    // The reparent entry recorded the old instance id; undo still moves the rebuilt instance.
    assert!(undo_world(world));
    let inst = by_guid(world, guid(INST)).expect("instance");
    assert_eq!(
        parent_of(world, inst),
        None,
        "undo reached the respawned instance"
    );
}
