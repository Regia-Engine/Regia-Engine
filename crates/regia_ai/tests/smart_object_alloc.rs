// SPDX-License-Identifier: MPL-2.0
//! Spec 39 §§ 5.1 and 12: without a log a search allocates nothing beyond its result vector, and
//! an unchanged index rebuild allocates nothing at all; a grid search visits only the cells its
//! region overlaps. Counts, never time. Every step runs on this thread, so the thread-local count
//! sees the whole of it.
//!
//! Its own test binary: it installs the counting global allocator (`regia_test_utils::alloc_counter`,
//! ADR-5718).

use regia_ai::smart_object_load::SmartObjectDefs;
use regia_ai::smart_object_v2::{ClaimPriority, QueryRegion, SlotQuery, claim_with};
use regia_ai::{SmartObject, SmartObjectDef, SmartObjectIndex, SmartObjectPlugin, SmartObjectSlot};
use regia_app::prelude::*;
use regia_asset::{AssetId, AssetRef};
use regia_core::math::{Affine3A, Vec3};
use regia_ecs::ecs::schedule::{Schedule, ScheduleLabel, Schedules, SingleThreadedExecutor};
use regia_ecs::ecs::world::World;
use regia_framework::tags::TagQuery;
use regia_scene::{GlobalTransform, Transform};
use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// This thread's heap requests so far.
fn allocations() -> usize {
    alloc_counter::thread_allocs() as usize
}

/// A side of the 1 m lattice of slots: enough slots for the grid index.
const SIDE: usize = 40;

/// `SIDE * SIDE` slots on a 1 m lattice, children of one object, the index built.
fn app() -> App {
    // The state `tracing` has in a host: no `String` per system run for a disabled span, whatever
    // the build's feature set (ADR-5718).
    alloc_counter::quiet_tracing();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(SmartObjectPlugin);
    let object = app.world_mut().spawn(SmartObject::default()).id();
    for x in 0..SIDE {
        for z in 0..SIDE {
            let position = Vec3::new(x as f32, 0.0, z as f32);
            app.world_mut().spawn((
                SmartObjectSlot::new("seat"),
                Transform::from_translation(position),
                GlobalTransform(Affine3A::from_translation(position)),
                regia_ecs::prelude::ChildOf(object),
            ));
        }
    }
    app.update();
    app.update();
    let label = PostUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    app
}

/// What the executor allocates by itself in one run of `label`, whatever the systems do, measured
/// on a schedule of as many no-op systems. In a whole-workspace build `tracing` has its `log`
/// feature (feature unification), and then, in a process where no dispatcher was ever set, the ECS
/// formats each system's name for a span: one `String` per system per run, none under
/// `cargo test -p` (ADR-2012). `app` calls `quiet_tracing` first, so this is 0 in both builds; it
/// stays as the check of that, and the counts in this file are what the engine's code does.
fn executor_allocations(app: &App, label: impl ScheduleLabel) -> usize {
    fn no_op() {}
    let systems = app
        .world()
        .resource::<Schedules>()
        .get(label.intern())
        .map_or(0, Schedule::systems_len);
    let mut schedule = Schedule::default();
    schedule.set_executor(SingleThreadedExecutor::new());
    for _ in 0..systems {
        schedule.add_systems(no_op);
    }
    let mut world = World::new();
    schedule.run(&mut world);
    // The least of three runs: one run that caught a lazy one-off allocation would inflate the
    // allowance subtracted from every measured step (audit-v035-flaky L5).
    let spent = (0..3)
        .map(|_| {
            let before = allocations();
            schedule.run(&mut world);
            allocations() - before
        })
        .min()
        .unwrap_or(0);
    assert!(spent <= systems, "{spent} allocations by {systems} no-op systems");
    spent
}

fn query() -> SlotQuery {
    SlotQuery::new(QueryRegion::Sphere {
        center: Vec3::new(20.0, 0.0, 20.0),
        radius: 3.0,
    })
}

fn index(app: &mut App) -> SmartObjectIndex {
    app.world_mut()
        .remove_resource::<SmartObjectIndex>()
        .expect("index")
}

#[test]
fn the_count_sees_an_allocation() {
    let before = allocations();
    std::hint::black_box(Box::new(1_u64));
    assert!(allocations() > before);
}

/// § 5.1: "with no log passed the path allocates nothing beyond the result vector". The hits are
/// known, so the vector is the only allocation that may remain, and it stays one however many
/// slots the region held.
#[test]
fn a_search_without_a_log_allocates_only_its_result() {
    let mut app = app();
    let index = index(&mut app);
    let world = app.world();
    let wanted = query();
    let before = allocations();
    let hits = index.search(world, &wanted, None);
    let spent = allocations() - before;
    assert_eq!(hits.len(), 29, "the 1 m lattice inside a 3 m sphere");
    assert!(
        spent <= 1 + hits.len().ilog2() as usize,
        "{spent} allocations for {} hits: the vector grows by doubling and nothing else",
        hits.len()
    );
}

#[test]
fn a_filtered_search_without_a_log_allocates_only_its_result() {
    let mut app = app();
    let index = index(&mut app);
    let world = app.world();
    let mut wanted = query();
    wanted.activity = Some(TagQuery::from_ron(r#"NoneOf([Has("Activity.Sit")])"#).expect("query"));
    wanted.families = Vec::new();
    let before = allocations();
    let hits = index.search(world, &wanted, None);
    let spent = allocations() - before;
    assert_eq!(hits.len(), 29);
    assert!(spent <= 1 + hits.len().ilog2() as usize, "{spent} allocations");
}

/// Nothing passes: the result vector never grows, so the search allocates nothing at all.
#[test]
fn a_search_with_no_hits_allocates_nothing() {
    let mut app = app();
    let index = index(&mut app);
    let world = app.world();
    let mut wanted = query();
    wanted.region = QueryRegion::Sphere {
        center: Vec3::new(-500.0, 0.0, 0.0),
        radius: 1.0,
    };
    let before = allocations();
    let hits = index.search(world, &wanted, None);
    assert_eq!((hits.len(), allocations() - before), (0, 0));
}

/// A claimed slot in the region is filtered, not an extra allocation.
#[test]
fn claimed_slots_do_not_change_what_a_search_allocates() {
    let mut app = app();
    let user = app.world_mut().spawn_empty().id();
    let first = app
        .world()
        .resource::<SmartObjectIndex>()
        .search(app.world(), &query(), None)[0]
        .slot;
    claim_with(app.world_mut(), first, user, ClaimPriority::Normal).expect("claim");
    let index = index(&mut app);
    let world = app.world();
    let wanted = query();
    let before = allocations();
    let hits = index.search(world, &wanted, None);
    let spent = allocations() - before;
    assert_eq!(hits.len(), 28);
    assert!(spent <= 1 + hits.len().ilog2() as usize, "{spent} allocations");
}

/// A reference that resolved costs nothing per frame: the loader only looks at it.
#[test]
fn a_loaded_definition_reference_allocates_nothing_per_frame() {
    let mut app = app();
    let id = AssetId::new_random();
    let def = SmartObjectDef::from_ron(r#"(format_version: 1, slots: [(name: "a"), (name: "b")])"#)
        .expect("definition");
    app.world_mut().resource_mut::<SmartObjectDefs>().insert(id, def);
    for _ in 0..200 {
        app.world_mut().spawn(SmartObject {
            definition: Some(AssetRef::new(id)),
            ..SmartObject::default()
        });
    }
    for _ in 0..4 {
        app.world_mut().run_schedule(PostUpdate);
        app.world_mut().clear_trackers();
    }
    let executor = executor_allocations(&app, PostUpdate);
    let mut total = 0;
    for _ in 0..60 {
        let before = allocations();
        app.world_mut().run_schedule(PostUpdate);
        total += (allocations() - before).abs_diff(executor);
        app.world_mut().clear_trackers();
    }
    assert_eq!(total, 0);
}

/// § 12's no-change rebuild as a count: `PostUpdate` with nothing moved allocates nothing.
#[test]
fn an_unchanged_index_rebuild_allocates_nothing() {
    let mut app = app();
    for _ in 0..4 {
        app.world_mut().run_schedule(PostUpdate);
    }
    let executor = executor_allocations(&app, PostUpdate);
    let mut total = 0;
    for _ in 0..60 {
        let before = allocations();
        app.world_mut().run_schedule(PostUpdate);
        total += (allocations() - before).abs_diff(executor);
    }
    assert_eq!(total, 0);
    assert_eq!(
        app.world().resource::<SmartObjectIndex>().slots().count(),
        SIDE * SIDE
    );
}

fn one_slot(app: &App) -> (Entity, Vec3) {
    let index = app.world().resource::<SmartObjectIndex>();
    let slot = index
        .slots()
        .find(|slot| slot.position == Vec3::ZERO)
        .expect("the origin slot");
    (slot.entity, slot.position)
}

fn move_slot(app: &mut App, entity: Entity, position: Vec3) {
    // The index stores the local-transform chain, and the PostUpdate rebuild runs only when a
    // slot's `GlobalTransform` changed. `ScenePlugin` (which owns `TransformPropagate`) is not in
    // this app, so the test writes both, the same pair a propagated move leaves behind. Writing
    // `Transform` alone never reaches the index.
    app.world_mut()
        .get_mut::<Transform>(entity)
        .expect("transform")
        .translation = position;
    app.world_mut()
        .get_mut::<GlobalTransform>(entity)
        .expect("global transform")
        .0 = Affine3A::from_translation(position);
}

fn warm_post_update(app: &mut App) {
    for _ in 0..4 {
        app.world_mut().run_schedule(PostUpdate);
        app.world_mut().clear_trackers();
    }
}

/// One slot moves inside its 8 m cell. The grid's membership does not change, so the rebuild
/// allocates nothing.
#[test]
fn a_move_inside_one_cell_allocates_nothing() {
    let mut app = app();
    warm_post_update(&mut app);
    let executor = executor_allocations(&app, PostUpdate);
    let (entity, origin) = one_slot(&app);
    move_slot(&mut app, entity, origin + Vec3::new(0.05, 0.0, 0.0));
    let before = allocations();
    app.world_mut().run_schedule(PostUpdate);
    let spent = (allocations() - before).abs_diff(executor);
    if spent != 0 {
        // Name the site: the same step again, traced.
        app.world_mut().clear_trackers();
        move_slot(&mut app, entity, origin + Vec3::new(0.1, 0.0, 0.0));
        let ((), traced) = alloc_counter::trace(|| app.world_mut().run_schedule(PostUpdate));
        panic!(
            "{spent} allocations moving one slot by 0.05 m; the same step traced:\n{}",
            traced.report(8)
        );
    }
    let index = app.world().resource::<SmartObjectIndex>();
    let moved = index.slots().find(|slot| slot.entity == entity).expect("slot");
    assert_eq!(moved.position, origin + Vec3::new(0.05, 0.0, 0.0));
    assert!(index.has_grid());
}

/// Crossing into a cell that already has slots may grow that cell once. Oscillating between the
/// two cells after that allocates nothing, and both cells still answer a search.
#[test]
fn oscillating_across_a_cell_boundary_allocates_nothing() {
    let mut app = app();
    warm_post_update(&mut app);
    let (entity, _) = one_slot(&app);
    // (0, 0, 0) is cell 0. x = 8 is the next cell, which the 1 m lattice already fills.
    move_slot(&mut app, entity, Vec3::new(8.05, 0.0, 0.0));
    app.world_mut().run_schedule(PostUpdate);
    app.world_mut().clear_trackers();
    move_slot(&mut app, entity, Vec3::new(0.05, 0.0, 0.0));
    app.world_mut().run_schedule(PostUpdate);
    app.world_mut().clear_trackers();
    let mut total = 0;
    let executor = executor_allocations(&app, PostUpdate);
    for step in 0..8 {
        let x = if step % 2 == 0 { 8.05 } else { 0.05 };
        move_slot(&mut app, entity, Vec3::new(x, 0.0, 0.0));
        let before = allocations();
        app.world_mut().run_schedule(PostUpdate);
        total += (allocations() - before).abs_diff(executor);
        app.world_mut().clear_trackers();
    }
    assert_eq!(total, 0, "{total} allocations oscillating across a cell boundary");
    let index = app.world().resource::<SmartObjectIndex>();
    assert!(index.has_grid());
    assert_eq!(index.slots().filter(|slot| slot.entity == entity).count(), 1);
}

/// The index lists its slots by role, then in the order they arrived, also when the roles arrive
/// interleaved and the list is long (ADR-5718: one unstable sort on the pair replaced an
/// arrival sort followed by a stable sort by role, which allocated a scratch buffer per rebuild
/// at this size; the order must be the one the two passes gave).
#[test]
fn the_index_lists_slots_by_role_then_arrival() {
    const ROLES: [&str; 3] = ["seat", "bench", "perch"];
    alloc_counter::quiet_tracing();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(SmartObjectPlugin);
    let object = app.world_mut().spawn(SmartObject::default()).id();
    let mut arrived: Vec<(&str, regia_ecs::prelude::Entity)> = Vec::new();
    for i in 0..SIDE * SIDE {
        // 7 and 3 are coprime: the roles interleave without a period of three.
        let role = ROLES[(i * 7 + i / 5) % 3];
        let position = Vec3::new((i % SIDE) as f32, 0.0, (i / SIDE) as f32);
        let slot = app
            .world_mut()
            .spawn((
                SmartObjectSlot::new(role),
                Transform::from_translation(position),
                GlobalTransform(Affine3A::from_translation(position)),
                regia_ecs::prelude::ChildOf(object),
            ))
            .id();
        arrived.push((role, slot));
    }
    app.update();
    app.update();
    // A stable sort by role of the arrival order: the order the index documents.
    let mut expected = arrived;
    expected.sort_by(|a, b| a.0.cmp(b.0));
    let index = app.world().resource::<SmartObjectIndex>();
    let listed: Vec<(&str, regia_ecs::prelude::Entity)> =
        index.slots().map(|slot| (slot.role, slot.entity)).collect();
    assert_eq!(listed.len(), SIDE * SIDE);
    assert!(listed == expected, "the index is not in role-then-arrival order");
}
