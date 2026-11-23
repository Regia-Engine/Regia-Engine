// SPDX-License-Identifier: MPL-2.0
//! A load replaces the saved entities and nothing else (ADR-7601, spec 91 § 10.3 step 2).
//!
//! `World::despawn` takes an entity's whole `Children` subtree with it, so before ADR-7601 a load
//! destroyed every non-`Saveable` child of a `Saveable` entity (an emitter, a camera rig, a pooled
//! attachment) although the save never recorded it. Now such a child is detached before its
//! parent goes and re-parented to the respawned entity with the same stable id (`EntityGuid` and
//! level), keeping its local transform; without a match it stays at the root where it was.
//!
//! Every test saves the live world and loads the save back into the same world, the way a game's
//! quick-load does.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::hierarchy::ChildOf;
use regia_framework::save_game::{SaveGame, SavePlugin, SaveSlot, SaveSpec, Saveable};
use regia_scene::components::EntityGuid;
use regia_scene::level::LevelMember;
use regia_scene::propagate::propagate_transforms;
use regia_scene::transform::{GlobalTransform, Transform};
use regia_test_utils::unique_temp_dir;

/// A name to find an entity again after a load.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Tag(String);

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SavePlugin::default()))
        .register_type::<Tag>()
        .register_type::<EntityGuid>()
        .register_type::<LevelMember>()
        .register_type::<Transform>()
        .register_type::<ChildOf>();
    app
}

fn tag(s: &str) -> Tag {
    Tag(s.to_string())
}

fn tagged(world: &mut World, tag: &str) -> Entity {
    let mut query = world.query::<(Entity, &Tag)>();
    let found: Vec<Entity> = query
        .iter(world)
        .filter(|(_, t)| t.0 == tag)
        .map(|(entity, _)| entity)
        .collect();
    assert_eq!(found.len(), 1, "exactly one entity tagged {tag:?}: {found:?}");
    found[0]
}

fn world_translation(world: &World, entity: Entity) -> Vec3 {
    world
        .get::<GlobalTransform>(entity)
        .map(GlobalTransform::translation)
        .expect("propagated")
}

fn parent_guid(world: &World, entity: Entity) -> Option<EntityGuid> {
    let parent = world.get::<ChildOf>(entity)?.parent();
    world.get::<EntityGuid>(parent).copied()
}

fn close(a: Vec3, b: Vec3) -> bool {
    (a - b).length() < 1e-4
}

/// Saves `app`'s world to a fresh slot, runs `between` on the world, then loads the save back.
fn save_then_load(app: &mut App, name: &str, between: impl FnOnce(&mut World)) {
    let dir = unique_temp_dir(&format!("save-unsaved-children-{name}"));
    let slot = SaveSlot {
        name: name.into(),
        path: dir.join(format!("{name}.save")),
        header: None,
    };
    SaveGame::save(&slot, app.world_mut(), &SaveSpec::default()).expect("save");
    between(app.world_mut());
    SaveGame::load(&slot, app.world_mut(), &SaveSpec::default()).expect("load");
    let _ = std::fs::remove_dir_all(&dir);
    propagate_transforms(app.world_mut());
}

const TORCH: EntityGuid = EntityGuid(Guid(0x7041_c400));

fn torch_with_emitter(world: &mut World) -> (Entity, Entity) {
    let torch = world
        .spawn((
            Saveable,
            tag("torch"),
            TORCH,
            Transform {
                translation: Vec3::new(10.0, 0.0, -4.0),
                rotation: Quat::from_rotation_y(0.5),
                ..Transform::IDENTITY
            },
        ))
        .id();
    let emitter = world
        .spawn((
            tag("emitter"),
            Transform::from_translation(Vec3::new(0.0, 1.5, 0.25)),
            ChildOf(torch),
        ))
        .id();
    propagate_transforms(world);
    (torch, emitter)
}

/// The acceptance case: a `Saveable` parent with a non-`Saveable` child survives save then load;
/// the child is alive, at the same world transform, under the parent with the same stable id.
#[test]
fn a_non_saveable_child_survives_a_load_under_its_respawned_parent() {
    let mut app = app();
    let (old_torch, emitter) = torch_with_emitter(app.world_mut());
    let before = world_translation(app.world(), emitter);

    save_then_load(&mut app, "acceptance", |_| {});

    let world = app.world_mut();
    assert!(
        world.get_entity(emitter).is_ok(),
        "the emitter is alive after the load"
    );
    let torch = tagged(world, "torch");
    assert_ne!(torch, old_torch, "the load respawned the saved torch");
    assert_eq!(world.get::<ChildOf>(emitter).map(ChildOf::parent), Some(torch));
    assert_eq!(parent_guid(world, emitter), Some(TORCH), "same parent id");
    let after = world_translation(world, emitter);
    assert!(close(before, after), "same world transform: {before} vs {after}");
    assert!(world.get_entity(old_torch).is_err(), "the old torch is gone");
}

/// A load moves the parent back to where the save found it; the attached child follows with its
/// local offset (it is part of the parent, not of the world).
#[test]
fn a_reattached_child_keeps_its_local_offset_when_the_parent_moves_back() {
    let mut app = app();
    let (torch, emitter) = torch_with_emitter(app.world_mut());
    let local = *app.world().get::<Transform>(emitter).expect("transform");
    let saved_world = world_translation(app.world(), emitter);

    save_then_load(&mut app, "moved", |world| {
        if let Some(mut t) = world.get_mut::<Transform>(torch) {
            t.translation = Vec3::new(-50.0, 3.0, 8.0);
        }
        propagate_transforms(world);
    });

    let world = app.world_mut();
    assert_eq!(parent_guid(world, emitter), Some(TORCH));
    assert_eq!(
        world.get::<Transform>(emitter).copied(),
        Some(local),
        "local offset kept"
    );
    let after = world_translation(world, emitter);
    assert!(
        close(saved_world, after),
        "back where the save had it: {saved_world} vs {after}"
    );
}

/// A runtime parent (no `EntityGuid`) has no stable id to find again: its non-saveable child stays
/// alive at the root, where it was in the world.
#[test]
fn a_child_of_a_runtime_parent_stays_at_the_root_where_it_was() {
    let mut app = app();
    let world = app.world_mut();
    let parent = world
        .spawn((
            Saveable,
            tag("pawn"),
            Transform::from_translation(Vec3::new(3.0, 0.0, 7.0)),
        ))
        .id();
    let rig = world
        .spawn((
            tag("camera rig"),
            Transform::from_translation(Vec3::new(0.0, 2.0, -5.0)),
            ChildOf(parent),
        ))
        .id();
    propagate_transforms(world);
    let before = world_translation(world, rig);

    save_then_load(&mut app, "runtime", |_| {});

    let world = app.world_mut();
    assert!(world.get_entity(rig).is_ok(), "the rig is alive after the load");
    assert_eq!(
        world.get::<ChildOf>(rig),
        None,
        "no stable id: the rig stays a root"
    );
    let after = world_translation(world, rig);
    assert!(close(before, after), "same world transform: {before} vs {after}");
    let pawn = tagged(world, "pawn");
    assert!(world.get_entity(pawn).is_ok(), "the pawn was respawned");
}

/// A saved hierarchy (a `Saveable` child under a `Saveable` parent) with a non-saveable grandchild:
/// the grandchild goes back under the respawned child, and the non-saveable child's own subtree
/// rides along untouched.
#[test]
fn nested_children_go_back_under_the_right_respawned_entity() {
    const ARM: EntityGuid = EntityGuid(Guid(0xa4));
    let mut app = app();
    let world = app.world_mut();
    let (torch, _) = torch_with_emitter(world);
    let arm = world
        .spawn((
            Saveable,
            tag("arm"),
            ARM,
            Transform::from_translation(Vec3::X),
            ChildOf(torch),
        ))
        .id();
    let light = world
        .spawn((tag("light"), Transform::from_translation(Vec3::Y), ChildOf(arm)))
        .id();
    let spark = world
        .spawn((tag("spark"), Transform::from_translation(Vec3::Z), ChildOf(light)))
        .id();
    propagate_transforms(world);
    let light_before = world_translation(world, light);
    let spark_before = world_translation(world, spark);

    save_then_load(&mut app, "nested", |_| {});

    let world = app.world_mut();
    let arm = tagged(world, "arm");
    assert_eq!(
        parent_guid(world, arm),
        Some(TORCH),
        "the saved hierarchy is rebuilt"
    );
    assert_eq!(world.get::<ChildOf>(light).map(ChildOf::parent), Some(arm));
    assert_eq!(world.get::<ChildOf>(spark).map(ChildOf::parent), Some(light));
    assert!(close(light_before, world_translation(world, light)));
    assert!(close(spark_before, world_translation(world, spark)));
    let emitter = tagged(world, "emitter");
    assert_eq!(parent_guid(world, emitter), Some(TORCH));
}

/// The stable id includes the level: a parent of another level with the same guid is not taken.
#[test]
fn the_parent_match_includes_the_level() {
    let level_a = LevelMember(0xaaaa);
    let level_b = LevelMember(0xbbbb);
    let mut app = app();
    let world = app.world_mut();
    let a = world
        .spawn((Saveable, tag("a"), TORCH, level_a, Transform::IDENTITY))
        .id();
    world.spawn((Saveable, tag("b"), TORCH, level_b, Transform::IDENTITY));
    let child = world
        .spawn((tag("child"), Transform::from_translation(Vec3::Y), ChildOf(a)))
        .id();
    propagate_transforms(world);

    save_then_load(&mut app, "level", |_| {});

    let world = app.world_mut();
    let a = tagged(world, "a");
    assert_eq!(world.get::<ChildOf>(child).map(ChildOf::parent), Some(a));
}
