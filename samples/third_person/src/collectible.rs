// SPDX-License-Identifier: MIT-0
//! Collectibles: sensor spheres with a trigger script. Collecting one despawns it, bumps the
//! counters, and the save system stores which ids are gone.

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::prelude::{Character, SaveableResource, Script, ScriptCtx, ScriptEvent};
use regia_framework::script::Scripts;
use regia_physics::{BodyKind, Collider, ColliderShape, RigidBody};
use regia_render::MeshRenderer;
use regia_scene::Transform;

use crate::level::{box_mesh, solid_material};

/// Marks a collectible entity and identifies it across saves.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct CollectibleId(pub u32);

/// Which collectibles are gone (the saveable half of the world: `SaveGame` stores this
/// resource, and [`apply_progress`] reconciles the live entities to it after a load).
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct CollectProgress {
    /// Ids that have been collected, in collection order.
    pub collected: Vec<u32>,
}

impl SaveableResource for CollectProgress {}

/// Where the singleton [`crate::SampleState`] entity lives (counters the HUD and agent
/// checks read).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct StateHandle(pub Option<Entity>);

/// Trigger-scan occupancy from the previous frame: sensor entity → overlapping entities.
/// (Rapier 0.36 does not emit sensor events for kinematic-vs-static pairs through this
/// stack, so the sample scans sensor volumes with the query API — `book/agents/physics.md`
/// "Overlap" — and raises the same `TriggerEnter`/`TriggerExit` messages the physics flush
/// would. The script layer is unchanged.)
#[derive(Resource, Debug, Default, Clone)]
pub struct TriggerOccupancy(std::collections::BTreeMap<Entity, std::collections::BTreeSet<Entity>>);

/// `Update` (before scripts): overlap-scans every sensor collider and raises
/// `TriggerEnter`/`TriggerExit` messages for occupancy changes — the same messages the
/// physics event flush produces for dynamic bodies, supplied here through the query API
/// (`book/agents/physics.md` "Overlap") because rapier 0.36 does not emit sensor events for
/// kinematic-vs-static pairs through this stack.
#[tracing::instrument(skip_all, target = "third_person", name = "scan_triggers")]
pub fn scan_triggers(world: &mut World) {
    // Snapshot the sensor volumes first (ends the world borrow before queries run).
    let sensors: Vec<(Entity, ColliderShape, Transform)> = {
        let mut query = world.query_filtered::<(Entity, &Collider, &Transform), ()>();
        query
            .iter(world)
            .filter(|(_, collider, _)| collider.sensor)
            .map(|(entity, collider, transform)| (entity, collider.shape.clone(), *transform))
            .collect()
    };
    if sensors.is_empty() {
        return;
    }
    let occupancy: std::collections::BTreeMap<Entity, std::collections::BTreeSet<Entity>> = {
        let Some(physics) = world.get_resource::<regia_physics::PhysicsWorld>() else {
            return;
        };
        let queries = regia_physics::queries::WorldQueries {
            world: physics,
            layers: None,
        };
        let mut occupancy = std::collections::BTreeMap::new();
        for (entity, shape, transform) in &sensors {
            let inside: std::collections::BTreeSet<Entity> = queries
                .overlap(shape, *transform, regia_physics::queries::QueryFilter::new())
                .into_iter()
                .filter(|hit| *hit != *entity)
                .collect();
            occupancy.insert(*entity, inside);
        }
        occupancy
    };
    let previous = world.remove_resource::<TriggerOccupancy>().unwrap_or_default().0;
    for (sensor, inside) in &occupancy {
        let before = previous.get(sensor).cloned().unwrap_or_default();
        for other in inside.difference(&before) {
            world.write_message(regia_physics::TriggerEnter {
                trigger: *sensor,
                other: *other,
            });
        }
        for other in before.difference(inside) {
            world.write_message(regia_physics::TriggerExit {
                trigger: *sensor,
                other: *other,
            });
        }
    }
    world.insert_resource(TriggerOccupancy(occupancy));
}

/// World positions of the level's collectibles, in id order.
pub fn spots() -> &'static [(f32, f32, f32)] {
    &[
        (0.0, 1.0, -3.0),
        (4.0, 1.0, -2.0),
        (8.0, 1.4, -0.5),   // on the stairs
        (12.0, 1.6, 0.5),   // top of the stairs
        (18.0, 1.8, 0.0),   // the slope
        (22.0, 2.2, 0.0),   // terrace
        (22.0, 2.9, -8.0),  // first platform
        (22.0, 3.6, -15.0), // high platform
    ]
}

/// Spawns the level's collectibles and returns how many.
pub fn spawn_collectibles(world: &mut World) -> u32 {
    for (i, &(x, y, z)) in spots().iter().enumerate() {
        let coin = spawn_one(world, i as u32, Vec3::new(x, y, z));
        crate::car::tag_coin(world, coin);
    }
    spots().len() as u32
}

/// One golden orb with a sensor collider and the collect script.
fn spawn_one(world: &mut World, id: u32, position: Vec3) -> Entity {
    world
        .spawn((
            Transform {
                translation: position,
                ..Default::default()
            },
            CollectibleId(id),
            RigidBody {
                kind: BodyKind::Static,
                ..Default::default()
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.45 },
                sensor: true,
                // Sensors only raise TriggerEnter/Exit with collision events active.
                active_events: regia_physics::ActiveEvents {
                    collision: true,
                    contact_force: false,
                },
                ..Default::default()
            },
            MeshRenderer::new(
                box_mesh(Vec3::new(0.22, 0.22, 0.22)),
                solid_material([0.95, 0.75, 0.15, 1.0]),
            ),
            Scripts::with::<CollectScript>(),
        ))
        .id()
}

/// The collectible behaviour: collect on trigger overlap with a character.
#[derive(regia_macros::Script, Reflect, Default, Clone)]
#[script(order = 0)]
pub struct CollectScript {
    /// How many trigger events this instance has seen (script state; also proves the
    /// take-run-reinsert instance survives between events).
    pub events_seen: u32,
}

impl Script for CollectScript {
    fn on_event(&mut self, ctx: &mut ScriptCtx, event: &ScriptEvent) {
        self.events_seen += 1;
        let ScriptEvent::TriggerEnter { entity, other } = event else {
            return;
        };
        if *entity != ctx.entity() {
            return; // targeted delivery guarantees this, but stay defensive
        }
        // Only characters collect.
        if ctx.world().get::<Character>(*other).is_none() {
            return;
        }
        let id = ctx.get::<CollectibleId>().map(|c| c.0).unwrap_or(u32::MAX);
        collect(ctx.world(), id);
        tracing::info!(target: "third_person", "collected {id}");
        ctx.despawn();
    }
}

/// Records a collection in the progress resource and the sample state.
pub fn collect(world: &mut World, id: u32) {
    let mut already = false;
    if let Some(mut progress) = world.get_resource_mut::<CollectProgress>() {
        already = progress.collected.contains(&id);
        if !already {
            progress.collected.push(id);
        }
    }
    if already {
        return;
    }
    let handle = world.resource::<StateHandle>().0;
    if let Some(entity) = handle
        && let Some(mut state) = world.get_mut::<crate::SampleState>(entity)
    {
        state.collected += 1;
    }
}

/// `Update` (after scripts): the same reconciliation as [`apply_progress`], so an agent
/// `save.load` — which restores [`CollectProgress`] but does not know about coins — drops
/// the collected entities on the next frame.
#[tracing::instrument(skip_all, target = "third_person", name = "reconcile_collectibles")]
pub fn reconcile(world: &mut World) {
    apply_progress(world);
}

/// Reconciles live collectibles with the (possibly just loaded) progress resource.
///
/// Collected ids are despawned. Ids that are not collected but missing — a coin picked
/// up after the save, for example — are spawned again, so a load restores the world and
/// not only the counter.
pub fn apply_progress(world: &mut World) {
    let Some(progress) = world.get_resource::<CollectProgress>() else {
        return;
    };
    let collected: Vec<u32> = progress.collected.clone();
    let to_despawn: Vec<Entity> = world
        .query_filtered::<Entity, With<CollectibleId>>()
        .iter(world)
        .filter(|entity| {
            world
                .get::<CollectibleId>(*entity)
                .is_some_and(|id| collected.contains(&id.0))
        })
        .collect();
    for entity in to_despawn {
        world.despawn(entity);
    }
    let present: Vec<u32> = world
        .query_filtered::<&CollectibleId, ()>()
        .iter(world)
        .map(|id| id.0)
        .collect();
    for (index, &(x, y, z)) in spots().iter().enumerate() {
        let id = index as u32;
        if collected.contains(&id) || present.contains(&id) {
            continue;
        }
        spawn_one(world, id, Vec3::new(x, y, z));
    }
    let handle = world.resource::<StateHandle>().0;
    if let Some(entity) = handle
        && let Some(mut state) = world.get_mut::<crate::SampleState>(entity)
    {
        state.collected = collected.len() as u32;
    }
}

/// Spins the remaining collectibles (visual only).
#[tracing::instrument(skip_all, target = "third_person", name = "collectibles_animate")]
pub fn animate(time: Res<Time>, mut query: Query<(&mut Transform,), With<CollectibleId>>) {
    let t = time.elapsed_secs();
    for (mut transform,) in &mut query {
        transform.rotation = regia_core::math::Quat::from_rotation_y(t * 1.5);
    }
}
