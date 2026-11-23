// SPDX-License-Identifier: MPL-2.0
//! Spec 39 § 12: a frame where nothing the index reads changed does not rebuild it, and every kind
//! of change that matters does (ADR-2012). Each test states what the index must show afterward,
//! so a skip that hid a change fails on the data, not only on the counter. The random walk at the
//! end compares the index with a list derived from the world after every frame.

use regia_ai::smart_object_v2::{ClaimPriority, QueryRegion, SlotQuery, claim_with, release_handle};
use regia_ai::{SmartObject, SmartObjectIndex, SmartObjectPlugin, SmartObjectSlot};
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_ecs::prelude::ChildOf;
use regia_scene::{GlobalTransform, ScenePlugin, Transform};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SmartObjectPlugin));
    app
}

fn spawn_slot(app: &mut App, parent: Entity, role: &str, position: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            SmartObjectSlot::new(role),
            GlobalTransform(Affine3A::from_translation(position)),
            ChildOf(parent),
        ))
        .id()
}

fn frame(app: &mut App) {
    app.world_mut().run_schedule(PostUpdate);
    app.world_mut().clear_trackers();
}

fn rebuilds(app: &App) -> u64 {
    app.world().resource::<SmartObjectIndex>().rebuilds()
}

/// One object with one slot, indexed.
fn one_slot() -> (App, Entity, Entity) {
    let mut app = app();
    let object = app.world_mut().spawn(SmartObject::default()).id();
    let slot = spawn_slot(&mut app, object, "seat", Vec3::X);
    frame(&mut app);
    (app, object, slot)
}

/// Runs one frame and asserts that it rebuilt the index exactly once.
fn frame_rebuilds(app: &mut App) {
    let before = rebuilds(app);
    frame(app);
    assert_eq!(rebuilds(app), before + 1, "this change must rebuild the index");
}

#[test]
fn unchanged_frames_do_not_rebuild() {
    let (mut app, _, slot) = one_slot();
    let settled = rebuilds(&app);
    assert!(settled >= 1, "the first frame builds the index");
    for _ in 0..10 {
        frame(&mut app);
    }
    assert_eq!(rebuilds(&app), settled);
    let index = app.world().resource::<SmartObjectIndex>();
    assert_eq!(index.len(), 1);
    assert!(index.slot(slot).is_some());
}

/// With real transform propagation: it must leave a still slot's `GlobalTransform` alone, and a
/// moved parent must reach the index through it.
#[test]
fn propagation_rebuilds_only_for_what_moved() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ScenePlugin, SmartObjectPlugin));
    let object = app
        .world_mut()
        .spawn((
            SmartObject::default(),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    let slot = app
        .world_mut()
        .spawn((
            SmartObjectSlot::new("seat"),
            Transform::from_translation(Vec3::X),
            GlobalTransform::default(),
            ChildOf(object),
        ))
        .id();
    app.update();
    app.update();
    let settled = rebuilds(&app);
    let position = |app: &App| {
        let index = app.world().resource::<SmartObjectIndex>();
        index.slot(slot).expect("indexed").position
    };
    assert_eq!(position(&app), Vec3::X);
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(rebuilds(&app), settled);

    app.world_mut()
        .get_mut::<Transform>(object)
        .expect("transform")
        .translation = Vec3::Y;
    app.update();
    assert_eq!(rebuilds(&app), settled + 1);
    assert_eq!(position(&app), Vec3::X + Vec3::Y);
}

#[test]
fn a_new_slot_appears() {
    let (mut app, object, _) = one_slot();
    let added = spawn_slot(&mut app, object, "bed", Vec3::Y);
    frame_rebuilds(&mut app);
    let index = app.world().resource::<SmartObjectIndex>();
    assert_eq!(index.len(), 2);
    assert_eq!(index.slot(added).expect("indexed").role, "bed");
}

#[test]
fn a_moved_slot_moves() {
    let (mut app, _, slot) = one_slot();
    let target = Vec3::new(4.0, 5.0, 6.0);
    app.world_mut()
        .entity_mut(slot)
        .insert(GlobalTransform(Affine3A::from_translation(target)));
    frame_rebuilds(&mut app);
    let index = app.world().resource::<SmartObjectIndex>();
    assert_eq!(index.slot(slot).expect("indexed").position, target);
}

#[test]
fn a_claim_and_its_release_show() {
    let (mut app, _, slot) = one_slot();
    let user = app.world_mut().spawn_empty().id();
    let handle = claim_with(app.world_mut(), slot, user, ClaimPriority::Normal).expect("claim");
    frame_rebuilds(&mut app);
    let index = app.world().resource::<SmartObjectIndex>();
    assert!(index.slot(slot).expect("indexed").claimed);

    release_handle(app.world_mut(), handle).expect("release");
    frame_rebuilds(&mut app);
    let index = app.world().resource::<SmartObjectIndex>();
    assert!(!index.slot(slot).expect("indexed").claimed);
}

#[test]
fn a_disabled_object_disables_its_slots() {
    let (mut app, object, slot) = one_slot();
    app.world_mut()
        .get_mut::<SmartObject>(object)
        .expect("object")
        .enabled = false;
    frame_rebuilds(&mut app);
    let index = app.world().resource::<SmartObjectIndex>();
    assert!(!index.slot(slot).expect("indexed").enabled);
}

#[test]
fn a_despawned_slot_goes() {
    let (mut app, object, slot) = one_slot();
    let kept = spawn_slot(&mut app, object, "bed", Vec3::Y);
    frame(&mut app);
    app.world_mut().despawn(slot);
    frame_rebuilds(&mut app);
    let index = app.world().resource::<SmartObjectIndex>();
    assert!(index.slot(slot).is_none());
    assert!(index.slot(kept).is_some());
}

#[test]
fn a_removed_slot_component_goes() {
    let (mut app, _, slot) = one_slot();
    app.world_mut().entity_mut(slot).remove::<SmartObjectSlot>();
    frame_rebuilds(&mut app);
    assert!(app.world().resource::<SmartObjectIndex>().is_empty());
}

#[test]
fn a_slot_that_loses_its_transform_goes() {
    let (mut app, _, slot) = one_slot();
    app.world_mut().entity_mut(slot).remove::<GlobalTransform>();
    frame_rebuilds(&mut app);
    assert!(app.world().resource::<SmartObjectIndex>().is_empty());
}

#[test]
fn losing_a_transform_elsewhere_does_not_rebuild() {
    let (mut app, ..) = one_slot();
    let other = app.world_mut().spawn(GlobalTransform::default()).id();
    frame(&mut app);
    let settled = rebuilds(&app);
    app.world_mut().entity_mut(other).remove::<GlobalTransform>();
    frame(&mut app);
    assert_eq!(rebuilds(&app), settled);
}

/// The slot keeps its world transform, so only the parent link says it changed owner.
#[test]
fn a_slot_moved_to_another_object_changes_owner() {
    let (mut app, _, slot) = one_slot();
    let other = app.world_mut().spawn(SmartObject::default()).id();
    frame(&mut app);
    app.world_mut().entity_mut(slot).insert(ChildOf(other));
    frame_rebuilds(&mut app);
    let index = app.world().resource::<SmartObjectIndex>();
    assert_eq!(index.slot(slot).expect("indexed").object, other);
}

/// The link that changes sits above the slot: nothing on the slot itself moves.
#[test]
fn a_group_moved_to_another_object_changes_the_owner_of_its_slots() {
    let mut app = app();
    let first = app.world_mut().spawn(SmartObject::default()).id();
    let second = app.world_mut().spawn(SmartObject::default()).id();
    let group = app.world_mut().spawn(ChildOf(first)).id();
    let slot = spawn_slot(&mut app, group, "seat", Vec3::X);
    frame(&mut app);
    assert_eq!(
        app.world()
            .resource::<SmartObjectIndex>()
            .slot(slot)
            .expect("indexed")
            .object,
        first
    );
    app.world_mut().entity_mut(group).insert(ChildOf(second));
    frame_rebuilds(&mut app);
    assert_eq!(
        app.world()
            .resource::<SmartObjectIndex>()
            .slot(slot)
            .expect("indexed")
            .object,
        second
    );
}

#[test]
fn a_group_cut_from_its_object_orphans_its_slots() {
    let mut app = app();
    let object = app.world_mut().spawn(SmartObject::default()).id();
    let group = app.world_mut().spawn(ChildOf(object)).id();
    let slot = spawn_slot(&mut app, group, "seat", Vec3::X);
    frame(&mut app);
    app.world_mut().entity_mut(group).remove::<ChildOf>();
    frame_rebuilds(&mut app);
    assert_eq!(
        app.world()
            .resource::<SmartObjectIndex>()
            .slot(slot)
            .expect("indexed")
            .object,
        Entity::PLACEHOLDER
    );
}

#[test]
fn a_removed_object_orphans_its_slots() {
    let (mut app, object, slot) = one_slot();
    app.world_mut().entity_mut(object).remove::<SmartObject>();
    frame_rebuilds(&mut app);
    assert_eq!(
        app.world()
            .resource::<SmartObjectIndex>()
            .slot(slot)
            .expect("indexed")
            .object,
        Entity::PLACEHOLDER
    );
}

/// A host's `cell_size` is read on the next frame: the regridded index answers as the old one.
#[test]
fn a_new_cell_size_regrids() {
    let mut app = app();
    let object = app.world_mut().spawn(SmartObject::default()).id();
    for x in 0..300 {
        spawn_slot(&mut app, object, "seat", Vec3::new(x as f32, 0.0, 0.0));
    }
    frame(&mut app);
    let query = SlotQuery::new(QueryRegion::Sphere {
        center: Vec3::new(100.0, 0.0, 0.0),
        radius: 3.5,
    });
    let search = |app: &mut App| {
        let index = app
            .world_mut()
            .remove_resource::<SmartObjectIndex>()
            .expect("index");
        let hits = index.search(app.world(), &query, None);
        app.world_mut().insert_resource(index);
        hits.iter().map(|hit| hit.slot).collect::<Vec<_>>()
    };
    let before = search(&mut app);
    assert_eq!(before.len(), 7);
    app.world_mut().resource_mut::<SmartObjectIndex>().cell_size = 2.0;
    frame(&mut app);
    assert_eq!(search(&mut app), before);
}

/// A zero, negative or non-finite `cell_size` must not make the grid path return fewer hits than
/// the linear scan (spec 39 § 9).
#[test]
fn a_bad_cell_size_still_matches_the_scan() {
    let mut app = app();
    let object = app.world_mut().spawn(SmartObject::default()).id();
    for x in 0..300 {
        spawn_slot(&mut app, object, "seat", Vec3::new(x as f32 - 150.0, 0.0, 0.0));
    }
    frame(&mut app);
    let query = SlotQuery::new(QueryRegion::Sphere {
        center: Vec3::ZERO,
        radius: 20.0,
    });
    let search = |app: &mut App| {
        let index = app
            .world_mut()
            .remove_resource::<SmartObjectIndex>()
            .expect("index");
        let hits = index.search(app.world(), &query, None);
        app.world_mut().insert_resource(index);
        hits.len()
    };
    let good = search(&mut app);
    assert_eq!(good, 41);
    for size in [-8.0, -0.0, 0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        app.world_mut().resource_mut::<SmartObjectIndex>().cell_size = size;
        frame(&mut app);
        assert_eq!(search(&mut app), good, "cell_size {size}");
    }
}

/// A replaced resource is empty until the plugin fills it.
#[test]
fn a_replaced_index_is_refilled() {
    let (mut app, ..) = one_slot();
    app.world_mut().insert_resource(SmartObjectIndex::default());
    frame(&mut app);
    assert_eq!(app.world().resource::<SmartObjectIndex>().len(), 1);
}

/// xorshift64: the walk below is one fixed sequence.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// `(slot, owner, position, enabled, claimed)` of every slot, derived from the world alone.
fn derived(world: &mut World) -> Vec<(Entity, Entity, Vec3, bool, bool)> {
    let mut query = world.query::<(Entity, &SmartObjectSlot, &GlobalTransform)>();
    let mut objects = world.query::<&SmartObject>();
    let mut parents = world.query::<&ChildOf>();
    let mut rows: Vec<_> = query
        .iter(world)
        .map(|(entity, slot, transform)| {
            let mut cursor = Some(entity);
            let mut owner = (Entity::PLACEHOLDER, true);
            while let Some(at) = cursor {
                if let Ok(object) = objects.get(world, at) {
                    owner = (at, object.enabled && object.off_reasons.is_empty());
                    break;
                }
                cursor = parents.get(world, at).ok().map(ChildOf::parent);
            }
            (
                entity,
                owner.0,
                transform.translation(),
                slot.enabled && slot.off_reasons.is_empty() && owner.1,
                slot.claimed_by.is_some(),
            )
        })
        .collect();
    rows.sort_by_key(|row| row.0.to_bits());
    rows
}

fn indexed(world: &World) -> Vec<(Entity, Entity, Vec3, bool, bool)> {
    let mut rows: Vec<_> = world
        .resource::<SmartObjectIndex>()
        .slots()
        .map(|s| (s.entity, s.object, s.position, s.enabled, s.claimed))
        .collect();
    rows.sort_by_key(|row| row.0.to_bits());
    rows
}

/// Every operation of the tests above, in one fixed random order, with quiet frames mixed in: after
/// each frame the index equals what the world says, and the quiet frames did rebuild nothing.
#[test]
fn a_random_walk_never_leaves_the_index_stale() {
    let mut app = app();
    let mut objects: Vec<Entity> = (0..4)
        .map(|_| app.world_mut().spawn(SmartObject::default()).id())
        .collect();
    let mut groups: Vec<Entity> = objects
        .iter()
        .map(|object| app.world_mut().spawn(ChildOf(*object)).id())
        .collect();
    let mut slots: Vec<Entity> = Vec::new();
    let user = app.world_mut().spawn_empty().id();
    let mut random = 0x9E37_79B9_7F4A_7C15_u64;
    let mut quiet = 0;
    for step in 0..2_000 {
        let pick = next(&mut random);
        let at = (pick >> 8) as usize;
        let position = Vec3::new(((pick >> 20) & 63) as f32, 0.0, ((pick >> 26) & 63) as f32);
        let before = rebuilds(&app);
        let mut touched = true;
        match (pick % 12, slots.is_empty()) {
            (0, _) | (_, true) => {
                let parent = groups[at % groups.len()];
                slots.push(spawn_slot(&mut app, parent, "seat", position));
            }
            (1, _) => {
                let slot = slots.swap_remove(at % slots.len());
                app.world_mut().despawn(slot);
            }
            (2, _) => {
                let slot = slots[at % slots.len()];
                app.world_mut()
                    .entity_mut(slot)
                    .insert(GlobalTransform(Affine3A::from_translation(position)));
            }
            (3, _) => {
                let group = groups[at % groups.len()];
                let object = objects[(at >> 4) % objects.len()];
                app.world_mut().entity_mut(group).insert(ChildOf(object));
            }
            (4, _) => {
                let object = objects[at % objects.len()];
                let mut object = app.world_mut().get_mut::<SmartObject>(object).expect("object");
                object.enabled = !object.enabled;
            }
            (5, _) => {
                let slot = slots[at % slots.len()];
                if app
                    .world()
                    .get::<SmartObjectSlot>(slot)
                    .expect("slot")
                    .claimed_by
                    .is_none()
                {
                    // A disabled slot refuses: that changes nothing, which the walk also checks.
                    let _ = claim_with(app.world_mut(), slot, user, ClaimPriority::Normal);
                } else {
                    regia_ai::release_slot(app.world_mut(), slot, user).expect("release");
                }
            }
            (6, _) => {
                let slot = slots[at % slots.len()];
                let group = groups[(at >> 4) % groups.len()];
                app.world_mut().entity_mut(slot).insert(ChildOf(group));
            }
            (7, _) if objects.len() > 1 && step % 50 == 7 => {
                // Slots under it end without an owner; the groups under it stay, unparented.
                let object = objects.swap_remove(at % objects.len());
                app.world_mut().entity_mut(object).remove::<SmartObject>();
                let replacement = app.world_mut().spawn(SmartObject::default()).id();
                objects.push(replacement);
                groups.push(app.world_mut().spawn(ChildOf(replacement)).id());
            }
            _ => touched = false,
        }
        frame(&mut app);
        let expected = derived(app.world_mut());
        assert_eq!(indexed(app.world()), expected, "stale after step {step}");
        if !touched {
            quiet += 1;
            assert_eq!(rebuilds(&app), before, "a quiet frame rebuilt at step {step}");
        }
    }
    assert!(quiet > 500, "the walk must hold quiet frames, had {quiet}");
    assert!(slots.len() > 3, "the walk must keep slots, had {}", slots.len());
}

/// Finding 11 (audit G): the index was a `PostUpdate` snapshot of the propagated
/// `GlobalTransform`, so on a frame whose second fixed step ran without propagation, a claim read
/// a pose a frame old — the frame rate leaked into the simulation. The fixed-step refresh must
/// see this step's local transforms.
#[test]
fn second_fixed_step_sees_a_slot_that_moved_on_the_first() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ScenePlugin, SmartObjectPlugin));
    let object = app
        .world_mut()
        .spawn((
            SmartObject::default(),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    let slot = app
        .world_mut()
        .spawn((
            SmartObjectSlot::new("seat"),
            Transform::from_translation(Vec3::X),
            GlobalTransform::default(),
            ChildOf(object),
        ))
        .id();
    let position = |app: &App| {
        app.world()
            .resource::<SmartObjectIndex>()
            .slot(slot)
            .expect("indexed")
            .position
    };
    // Settle: the frame snapshot builds.
    app.world_mut().run_schedule(PostUpdate);
    assert_eq!(position(&app), Vec3::X);
    // The slot's parent moves on the first fixed step; no propagation runs before the next step.
    app.world_mut()
        .get_mut::<Transform>(object)
        .expect("transform")
        .translation = Vec3::Y;
    app.world_mut().run_schedule(FixedUpdate);
    assert_eq!(
        position(&app),
        Vec3::X + Vec3::Y,
        "the fixed-step refresh reads this step's local transforms"
    );
}

/// fixrev-05-k (audit-kimirev-fix-ai-determinism-muse, Med 3): the fixed-step watch saw local
/// `Transform` changes only, but the rebuild falls back to `GlobalTransform` for a slot that has
/// no local `Transform`. A `GlobalTransform`-only slot moved between fixed steps must refresh
/// the step index, not wait for `PostUpdate`.
#[test]
fn a_global_transform_only_move_rebuilds_the_step_index() {
    let (mut app, _object, slot) = one_slot();
    // Move the slot between fixed steps with no `Transform` anywhere: only its
    // `GlobalTransform` changes.
    app.world_mut()
        .get_mut::<GlobalTransform>(slot)
        .expect("global transform")
        .0 = Affine3A::from_translation(Vec3::new(50.0, 0.0, 0.0));
    app.world_mut().run_schedule(FixedUpdate);
    let position = app
        .world()
        .resource::<SmartObjectIndex>()
        .slot(slot)
        .expect("indexed")
        .position;
    assert_eq!(
        position,
        Vec3::new(50.0, 0.0, 0.0),
        "the step refresh saw the GlobalTransform-only move"
    );
}

/// fixrev-05-k (audit-kimirev-fix-ai-determinism-muse, Med 4): a non-slot `Transform` that moves
/// every fixed step (a player, a camera) rebuilds the step index each step; the step refresh
/// must not then also trigger an identical rebuild in `PostUpdate` through its own write.
#[test]
fn a_step_rebuild_is_not_rebuilt_again_in_post_update() {
    let mut app = app();
    let object = app.world_mut().spawn(SmartObject::default()).id();
    for i in 0..4 {
        spawn_slot(&mut app, object, "seat", Vec3::new(i as f32, 0.0, 0.0));
    }
    let mover = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::ZERO))
        .id();
    app.update();
    assert!(rebuilds(&app) >= 1, "the first frame builds the index");
    for step in 0..3u32 {
        app.world_mut()
            .get_mut::<Transform>(mover)
            .expect("transform")
            .translation = Vec3::new(step as f32, 0.0, 0.0);
        let before = rebuilds(&app);
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            rebuilds(&app),
            before + 1,
            "the moving transform rebuilds the step index at step {step}"
        );
        app.world_mut().run_schedule(PostUpdate);
        assert_eq!(
            rebuilds(&app),
            before + 1,
            "PostUpdate must not rebuild the identical index again at step {step}"
        );
        app.world_mut().clear_trackers();
    }
}

/// Deep-zai lower note (`smart_object.rs` staleness observers): the index chain-reads an
/// ancestor's `GlobalTransform` when the ancestor has no local `Transform`, but the removal
/// observer only fired for the slot itself. An ancestor losing its `GlobalTransform` must
/// rebuild the index the same frame, or every claim until an unrelated rebuild sees a pose
/// the world no longer has.
#[test]
fn an_ancestor_losing_its_global_transform_rebuilds_the_index() {
    let mut app = app();
    // A `GlobalTransform`-only object: the chain read below falls back to its global.
    let object = app
        .world_mut()
        .spawn((
            SmartObject::default(),
            GlobalTransform(Affine3A::from_translation(Vec3::Y * 10.0)),
        ))
        .id();
    let slot = app
        .world_mut()
        .spawn((
            SmartObjectSlot::new("seat"),
            Transform::from_translation(Vec3::X),
            GlobalTransform(Affine3A::from_translation(Vec3::X + Vec3::Y * 10.0)),
            ChildOf(object),
        ))
        .id();
    frame(&mut app);
    let position = |app: &App| {
        app.world()
            .resource::<SmartObjectIndex>()
            .slot(slot)
            .expect("indexed")
            .position
    };
    assert_eq!(position(&app), Vec3::X + Vec3::Y * 10.0);

    // The object keeps its `ChildOf` children but loses its world transform: the slot's
    // chain-resolved pose drops the object's contribution.
    app.world_mut().entity_mut(object).remove::<GlobalTransform>();
    frame_rebuilds(&mut app);
    assert_eq!(
        position(&app),
        Vec3::X,
        "the index must show the pose without the ancestor's global transform"
    );
}
