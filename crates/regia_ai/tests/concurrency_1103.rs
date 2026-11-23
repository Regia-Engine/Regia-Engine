// SPDX-License-Identifier: MPL-2.0
//! Ordering regressions from the 11-03 concurrency audit (ADR-2396): spatial queries run after
//! physics, parking a smart-object slot updates the index, behavior trees tick in spawn order, and
//! a leaf that replaces its own agent's tree keeps the replacement.

use regia_ai::{
    AiPlugin, BehaviorTree, BehaviorTreeDef, BtContext, BtStatus, BtTask, BtTaskRegistration, BtTaskRegistry,
    SmartObject, SmartObjectIndex, SmartObjectPlugin, SmartObjectSlot, SpatialQueryPlugin,
};
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_ecs::prelude::ChildOf;
use regia_physics::PhysicsStep;
use regia_scene::GlobalTransform;
use regia_scene::layers::LayerParked;
use regia_scene::prelude::Visibility;

#[derive(Resource, Default)]
struct Marks(u32);

fn physics_marker(mut marks: ResMut<Marks>) {
    marks.0 += 1;
}

/// Audit H1: the spatial-query tick reads `PhysicsWorld`, so it must have an order after the
/// physics step instead of only `before(AiStep)`.
#[test]
fn spatial_query_tick_is_ordered_after_physics() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AiPlugin, SpatialQueryPlugin));
    app.init_resource::<Marks>();
    app.add_systems(FixedUpdate, physics_marker.in_set(PhysicsStep));
    let report = app.ambiguity_report();
    let unordered: Vec<_> = report
        .ranked()
        .into_iter()
        .filter(|(_, pair)| {
            let names = [pair.a.as_str(), pair.b.as_str()];
            names.contains(&"physics_marker")
                && names
                    .iter()
                    .any(|n| n.contains("tick_spatial_queries") || n.contains("run_probes"))
        })
        .map(|(_, pair)| (pair.a.clone(), pair.b.clone()))
        .collect();
    assert!(unordered.is_empty(), "unordered against physics: {unordered:?}");
}

fn frame(app: &mut App) {
    app.world_mut().run_schedule(PostUpdate);
    app.world_mut().clear_trackers();
}

fn parked() -> LayerParked {
    LayerParked {
        restore: Visibility::Inherited,
        inherited: true,
    }
}

/// Audit H2: parking or unparking an otherwise unchanged slot reaches the index.
#[test]
fn parking_and_unparking_a_still_slot_updates_the_index() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SmartObjectPlugin));
    let object = app.world_mut().spawn(SmartObject::default()).id();
    let slot = app
        .world_mut()
        .spawn((
            SmartObjectSlot::new("seat"),
            GlobalTransform(Affine3A::from_translation(Vec3::X)),
            ChildOf(object),
        ))
        .id();
    frame(&mut app);
    assert!(app.world().resource::<SmartObjectIndex>().slot(slot).is_some());

    app.world_mut().entity_mut(slot).insert(parked());
    frame(&mut app);
    assert!(
        app.world().resource::<SmartObjectIndex>().slot(slot).is_none(),
        "a parked slot leaves the index"
    );

    app.world_mut().entity_mut(slot).remove::<LayerParked>();
    frame(&mut app);
    assert!(
        app.world().resource::<SmartObjectIndex>().slot(slot).is_some(),
        "an unparked slot comes back"
    );
}

#[derive(Resource, Default)]
struct Ticked(Vec<Entity>);

#[derive(Default)]
struct RecordAgent;
impl BtTask for RecordAgent {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let entity = ctx.entity;
        ctx.world.resource_mut::<Ticked>().0.push(entity);
        BtStatus::Success
    }
}
impl BtTaskRegistration for RecordAgent {
    fn construct() -> Box<dyn BtTask> {
        Box::new(RecordAgent)
    }
    fn task_type_path() -> &'static str {
        "test::RecordAgent"
    }
}

fn record_tree(app: &App) -> BehaviorTree {
    let registry = app.world().resource::<BtTaskRegistry>();
    BehaviorTree::compile(&BehaviorTreeDef::leaf("r", "test::RecordAgent"), registry).expect("compiles")
}

/// ecs audit M2: trees tick in the order they arrived, not in entity-id order. A late agent with
/// a lower entity id (a reused index, or here an entity reserved earlier that gets its tree
/// later) must not tick (and win first-come claims) before an older agent.
#[test]
fn behavior_trees_tick_in_spawn_order_not_entity_id_order() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AiPlugin));
    app.init_resource::<Ticked>();
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register::<RecordAgent>();
    // The late agent's entity exists first (lower id) but gets its tree after the older agent.
    let late = app.world_mut().spawn_empty().id();
    let tree = record_tree(&app);
    let older = app.world_mut().spawn(tree).id();
    let tree = record_tree(&app);
    app.world_mut().entity_mut(late).insert(tree);
    assert!(
        late.index().index() < older.index().index(),
        "the late agent has the lower entity index (late {late:?}, older {older:?})"
    );
    for _ in 0..2 {
        app.world_mut().run_schedule(FixedUpdate);
    }
    assert_eq!(app.world().resource::<Ticked>().0, vec![older, late, older, late]);
}

#[derive(Resource, Default)]
struct NextTree(Option<BehaviorTree>);

#[derive(Default)]
struct SwitchTree;
impl BtTask for SwitchTree {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let entity = ctx.entity;
        if let Some(tree) = ctx.world.resource_mut::<NextTree>().0.take() {
            ctx.world.entity_mut(entity).insert(tree);
        }
        BtStatus::Success
    }
}
impl BtTaskRegistration for SwitchTree {
    fn construct() -> Box<dyn BtTask> {
        Box::new(SwitchTree)
    }
    fn task_type_path() -> &'static str {
        "test::SwitchTree"
    }
}

/// ai audit L2: a leaf that installs a new tree on its own agent keeps it; the tick does not
/// re-insert the old tree over it.
#[test]
fn a_leaf_that_replaces_its_own_tree_keeps_the_replacement() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AiPlugin));
    app.init_resource::<Ticked>();
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register::<RecordAgent>();
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register::<SwitchTree>();
    let next = record_tree(&app);
    app.insert_resource(NextTree(Some(next)));
    let switch = {
        let registry = app.world().resource::<BtTaskRegistry>();
        BehaviorTree::compile(&BehaviorTreeDef::leaf("s", "test::SwitchTree"), registry).expect("compiles")
    };
    let agent = app.world_mut().spawn(switch).id();
    app.world_mut().run_schedule(FixedUpdate);
    assert!(app.world().resource::<Ticked>().0.is_empty());
    app.world_mut().run_schedule(FixedUpdate);
    assert_eq!(
        app.world().resource::<Ticked>().0,
        vec![agent],
        "the second tick runs the replacement tree"
    );
}
