// SPDX-License-Identifier: MPL-2.0
//! Rapier insertion follows the order bodies arrive in the world, not their entity ids
//! (ADR-1347).
//!
//! `Commands::spawn` reserves an entity id when the command is recorded. Two systems that spawn
//! bodies in the same pass with no order between them run in parallel on the multi-threaded
//! executor, so which one gets the lower ids depends on thread timing, while their command
//! buffers are still applied in a fixed order. These tests replay that situation with two
//! command queues: the queue applied second reserves its ids first. Rapier must see the bodies in
//! application order, and the simulation must come out bit-identical whichever queue reserved
//! first.

use std::collections::BTreeMap;
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::ecs::world::CommandQueue;
use regia_ecs::prelude::*;
use regia_physics::{
    Collider, ColliderShape, Joint, JointHandle, JointKind, PhysicsPlugin, RigidBody, RigidBodyHandle,
    Velocity,
};
use regia_scene::Transform;

/// Which pile box this is: `lane * 10 + i`.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Tag(u32);

const PER_LANE: u32 = 4;

fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(10.0, 0.5, 10.0),
            },
            ..Collider::default()
        },
    ));
    app
}

/// A box of lane `lane`, stacked so the two lanes interleave and land on each other.
fn pile_box(lane: u32, i: u32) -> impl Bundle {
    let level = 2 * i + lane;
    (
        Tag(lane * 10 + i),
        Transform::from_translation(Vec3::new(
            0.12 * level as f32,
            0.3 + 0.55 * level as f32,
            0.07 * lane as f32,
        )),
        RigidBody::dynamic(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::splat(0.25),
            },
            ..Collider::default()
        },
        Velocity::default(),
    )
}

/// Records lane 0 into one queue and lane 1 into another, reserving lane 1's ids first when
/// `lane1_reserves_first`, then applies lane 0's queue before lane 1's (system order).
/// Returns the entities of each lane.
fn spawn_lanes(app: &mut App, lane1_reserves_first: bool) -> [Vec<Entity>; 2] {
    let mut queues = [CommandQueue::default(), CommandQueue::default()];
    let mut lanes: [Vec<Entity>; 2] = [Vec::new(), Vec::new()];
    {
        let world = app.world();
        let order: [usize; 2] = if lane1_reserves_first { [1, 0] } else { [0, 1] };
        for lane in order {
            let mut commands = Commands::new(&mut queues[lane], world);
            lanes[lane] = (0..PER_LANE)
                .map(|i| commands.spawn(pile_box(lane as u32, i)).id())
                .collect();
        }
    }
    for queue in &mut queues {
        queue.apply(app.world_mut());
    }
    lanes
}

fn handle_of(app: &App, entity: Entity) -> u32 {
    app.world()
        .get::<RigidBodyHandle>(entity)
        .map(|h| h.raw)
        .unwrap_or_else(|| panic!("{entity:?} has a rapier body"))
}

#[test]
fn bodies_enter_rapier_in_arrival_order_when_ids_were_reserved_backwards() {
    let mut app = physics_app();
    let [lane0, lane1] = spawn_lanes(&mut app, true);
    // The setup really is inverted: lane 1 holds the lower ids.
    let max1 = lane1.iter().map(|e| e.index().index()).max();
    let min0 = lane0.iter().map(|e| e.index().index()).min();
    assert!(max1 < min0, "lane 1 reserved first: {lane1:?} vs {lane0:?}");
    app.update();
    let last0 = lane0.iter().map(|e| handle_of(&app, *e)).max();
    let first1 = lane1.iter().map(|e| handle_of(&app, *e)).min();
    assert!(
        last0 < first1,
        "lane 0 was applied first, so its bodies get the lower rapier handles: {last0:?} vs {first1:?}"
    );
    // Within a lane, recorded order.
    let handles0: Vec<u32> = lane0.iter().map(|e| handle_of(&app, *e)).collect();
    let mut sorted = handles0.clone();
    sorted.sort_unstable();
    assert_eq!(handles0, sorted);
}

/// Every pile box's pose and velocity as raw bits, by tag.
fn pile_state(app: &mut App) -> BTreeMap<Tag, Vec<u32>> {
    let world = app.world_mut();
    let mut query = world.query::<(&Tag, &Transform, &Velocity)>();
    query
        .iter(world)
        .map(|(tag, transform, velocity)| {
            let t = transform.translation;
            let r = transform.rotation;
            let (v, w) = (velocity.linear, velocity.angular);
            let bits = [t.x, t.y, t.z, r.x, r.y, r.z, r.w, v.x, v.y, v.z, w.x, w.y, w.z]
                .iter()
                .map(|f| f.to_bits())
                .collect();
            (*tag, bits)
        })
        .collect()
}

fn run_pile(lane1_reserves_first: bool, steps: u32) -> BTreeMap<Tag, Vec<u32>> {
    let mut app = physics_app();
    spawn_lanes(&mut app, lane1_reserves_first);
    for _ in 0..steps {
        app.update();
    }
    pile_state(&mut app)
}

#[test]
fn the_pile_is_bit_identical_whichever_lane_reserved_its_ids_first() {
    let forward = run_pile(false, 150);
    let backward = run_pile(true, 150);
    assert_eq!(forward.len(), (2 * PER_LANE) as usize);
    // The pile moved: the boxes fell and collided.
    let first = run_pile(false, 1);
    assert_ne!(forward, first, "the pile settles over 150 steps");
    let differing: Vec<&Tag> = forward
        .iter()
        .filter(|(tag, bits)| backward.get(*tag) != Some(*bits))
        .map(|(tag, _)| tag)
        .collect();
    assert!(
        differing.is_empty(),
        "boxes that drifted with the id order: {differing:?}"
    );
}

#[test]
fn joints_enter_rapier_in_arrival_order() {
    let mut app = physics_app();
    // Lane 0 reserved (and holds) the lower ids.
    let [lane0, lane1] = spawn_lanes(&mut app, false);
    app.update();
    assert!(lane0[0].index().index() < lane1[0].index().index());
    let joint = |other: Entity| Joint {
        kind: JointKind::Fixed,
        connected: other,
        ..Joint::default()
    };
    // The joint on the higher id arrives first.
    app.world_mut().entity_mut(lane1[0]).insert(joint(lane1[1]));
    app.world_mut().entity_mut(lane0[0]).insert(joint(lane0[1]));
    app.update();
    let raw = |entity: Entity| {
        app.world()
            .get::<JointHandle>(entity)
            .map(|h| h.raw)
            .unwrap_or_else(|| panic!("{entity:?} has a rapier joint"))
    };
    assert!(
        raw(lane1[0]) < raw(lane0[0]),
        "the joint added first gets the lower handle"
    );
}
