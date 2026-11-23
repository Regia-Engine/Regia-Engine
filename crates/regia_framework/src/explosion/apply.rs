// SPDX-License-Identifier: MPL-2.0
//! The fixed-step system of [`super`]: impulses, destruction damage, the cloth gust and the
//! counters.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_physics::cloth::{ClothSet, MAX_WIND_SPEED, WindField, WindFieldKind};
use regia_physics::convert::{from_rapier_vec3, to_rapier_vec3};
use regia_physics::destruction::{
    Authority, ChunkState, Damage, DamageKind, DestructibleId, DestructibleRoot, Destruction,
    DestructionChunk, follow_root_transforms, step_destruction,
};
use regia_physics::{
    BodyKind, ColliderHandle, ExternalImpulse, JointBroken, PhysicsQuery, PhysicsStep, PhysicsWorld,
    QueryFilter, RigidBody, RigidBodyHandle,
};
use regia_scene::{GlobalTransform, Transform};

use super::{Explosion, ExplosionError, ExplosionEvent, OCCLUSION_MARGIN, Occlusion};

/// Fixed steps after an explosion in which joint break reports are credited to it in
/// [`ExplosionStats`] (the solver breaks a joint on the step after the impulse).
const CREDIT_WINDOW_STEPS: u32 = 8;
/// Fixed steps a gust lives.
const GUST_STEPS: u32 = 30;

/// Who decides when there is no `Destruction` resource.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExplosionSettings {
    /// A [`Authority::Replica`] pushes no gameplay body and queues no damage; it does push its own
    /// flying debris, which each process simulates for itself. With a `Destruction` resource its
    /// own authority wins.
    pub authority: Authority,
}

/// What the explosions did since the plugin was added (agent `gameplay.explosion_stats`).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct ExplosionStats {
    /// Explosions processed (replicas count theirs too).
    pub explosions: u64,
    /// Explosions skipped for force because this world is a replica.
    pub skipped_replica: u64,
    /// Bodies that received an impulse (a body hit twice counts twice).
    pub bodies_hit: u64,
    /// Bodies in range that the ray cast shielded.
    pub bodies_occluded: u64,
    /// Sum of the linear impulses' magnitudes (newton-seconds).
    pub impulse_total: f64,
    /// Joints broken within `CREDIT_WINDOW_STEPS` fixed steps of an explosion.
    pub joints_broken: u64,
    /// Destruction damage requests queued.
    pub damage_queued: u64,
    /// Chunks the blasts reached when they went off: attached chunks whose center is inside the
    /// radius (a replica's world takes no damage, so it counts none), plus
    /// [`Self::chunks_pushed`]. A chunk hit by two blasts counts twice.
    pub chunks_hit: u64,
    /// Flying debris chunks that received an impulse (a chunk pushed twice counts twice).
    pub chunks_pushed: u64,
    /// Cloth gusts started.
    pub gusts: u64,
    /// Effect layers played (explosion-fx).
    pub fx_played: u64,
    /// Camera shakes scheduled (explosion-fx).
    pub shakes: u64,
    /// Fixed steps left in which breaks are credited.
    pub credit_steps: u32,
}

/// A one-shot radial wind field that stirs cloth for `GUST_STEPS` fixed steps.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExplosionGust {
    /// Fixed steps left.
    pub steps_left: u32,
}

/// Validates `explosion` and queues it for the next fixed step.
///
/// # Errors
/// The first [`ExplosionError`] of [`Explosion::validate`].
pub fn explode(world: &mut World, explosion: Explosion) -> Result<(), ExplosionError> {
    explosion.validate()?;
    world.write_message(ExplosionEvent(explosion));
    Ok(())
}

/// Adds the explosion messages, resources and systems. Needs `regia_physics::PhysicsPlugin`;
/// `DestructionPlugin`, the cloth plugin and (with `explosion-fx`) `fx::ExplosionFxPlugin`
/// make the other consequences happen when present.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExplosionPlugin;

impl Plugin for ExplosionPlugin {
    fn build(&self, app: &mut App) {
        // The joint messages are read for the counters; registering them again is a no-op when
        // the physics plugin already did.
        app.add_message::<ExplosionEvent>()
            .add_message::<JointBroken>()
            .init_resource::<ExplosionStats>()
            .init_resource::<ExplosionSettings>()
            // Damage queued here breaks in this very tick, at the pose the structure has now.
            .add_systems(
                FixedUpdate,
                (apply_explosions, expire_gusts)
                    .chain()
                    .after(follow_root_transforms)
                    .before(step_destruction)
                    .before(PhysicsStep),
            );
    }
}

/// Query of the bodies an explosion pushes.
type BodyQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static RigidBodyHandle,
        &'static RigidBody,
        Option<&'static ExternalImpulse>,
    ),
    (Without<DestructionChunk>, Without<DestructibleRoot>),
>;

/// The place, mass and bounds of one rapier body.
struct BodyFacts {
    com: Vec3,
    mass: f32,
    lo: Vec3,
    hi: Vec3,
}

/// `body_facts` calls in one app (tests insert it; app-local, so parallel tests cannot add to
/// it). An explosion counts one per body whose collider meets the blast.
#[cfg(test)]
#[derive(Resource, Default)]
pub struct BodyFactsCalls {
    /// Calls so far.
    pub calls: u64,
}

/// Reads a body; `None` when it is gone.
fn body_facts(world: &PhysicsWorld, handle: RigidBodyHandle) -> Option<BodyFacts> {
    let rb = world.bodies.get(handle.to_handle())?;
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for collider in rb.colliders().iter().filter_map(|h| world.colliders.get(*h)) {
        let aabb = collider.compute_aabb();
        lo = lo.min(from_rapier_vec3(aabb.mins));
        hi = hi.max(from_rapier_vec3(aabb.maxs));
    }
    Some(BodyFacts {
        com: from_rapier_vec3(rb.center_of_mass()),
        mass: rb.mass(),
        lo,
        hi,
    })
}

/// What the blast gives a body with those facts at falloff `gain`: linear and torque impulses.
/// `None` for a body without a usable mass.
fn push_of(explosion: &Explosion, body: &BodyFacts, gain: f32) -> Option<(Vec3, Vec3)> {
    if !(body.mass.is_finite() && body.mass > 0.0) {
        return None;
    }
    let dir = explosion.direction_to(body.com);
    let linear = dir * (body.mass * explosion.peak_delta_v() * gain);
    let hit = hit_point(body.lo, body.hi, body.com, explosion.center);
    Some((linear, (hit - body.com).cross(linear) * explosion.torque))
}

/// Attached chunks whose center lies inside the radius: the chunks the damage path loads.
fn attached_in_reach(destruction: &Destruction, explosion: &Explosion) -> u64 {
    destruction
        .world
        .instances()
        .map(|(_, inst)| {
            (0u32..)
                .zip(inst.chunk_states())
                .filter(|(c, state)| {
                    **state == ChunkState::Attached
                        && inst.world_centroid(*c).is_some_and(|p| {
                            Vec3::from_array(p).distance(explosion.center) < explosion.radius
                        })
                })
                .count() as u64
        })
        .sum()
}

/// The flying chunks a blast reaches: their impulses add up here and go to rapier together (ADR-2573).
type FlyingPushes = BTreeMap<(DestructibleId, u32), (RigidBodyHandle, Vec3, Vec3)>;

/// Applies every [`ExplosionEvent`] of the step, in order: flying debris in (instance, chunk) order
/// (any process), then damage and the bodies in ascending entity order (an authoritative one only)
/// (span `apply_explosions`).
#[allow(clippy::too_many_arguments)]
pub fn apply_explosions(
    mut events: MessageReader<ExplosionEvent>,
    mut joints: MessageReader<JointBroken>,
    mut commands: Commands,
    world: Res<PhysicsWorld>,
    query: PhysicsQuery,
    bodies: BodyQuery,
    kinds: KindQuery,
    debris: Query<(), With<DestructionChunk>>,
    destruction: Option<ResMut<Destruction>>,
    cloth: Option<Res<ClothSet>>,
    settings: Res<ExplosionSettings>,
    mut stats: ResMut<ExplosionStats>,
    #[cfg(test)] mut facts_calls: Option<ResMut<BodyFactsCalls>>,
) {
    let joint_count = joints.read().count() as u64;
    if stats.credit_steps > 0 {
        stats.credit_steps -= 1;
        stats.joints_broken += joint_count;
    }
    if events.is_empty() {
        return;
    }
    let _span = tracing::debug_span!("apply_explosions").entered();

    let authority = destruction
        .as_ref()
        .map_or(settings.authority, |d| d.world.authority());
    let mut destruction = destruction;
    // Entity bits order: the same set of bodies gives the same sequence on every run.
    let mut order: Vec<(u64, Entity, RigidBodyHandle)> = bodies
        .iter()
        .filter(|(_, _, body, _)| body.kind == BodyKind::Dynamic)
        .map(|(entity, handle, _, _)| (entity.to_bits(), entity, *handle))
        .collect();
    order.sort_unstable_by_key(|(bits, _, _)| *bits);
    let mut pending: BTreeMap<u64, (Entity, ExternalImpulse)> = BTreeMap::new();
    let mut flying: FlyingPushes = BTreeMap::new();
    let mut source: Vec<Entity> = Vec::new();
    let mut skip: Vec<Entity> = Vec::new();

    for ExplosionEvent(explosion) in events.read() {
        if explosion.validate().is_err() {
            tracing::warn!(target: "regia_framework", "explosion rejected: {:?}", explosion.validate());
            continue;
        }
        stats.explosions += 1;
        // The source the blast ignores, with its child colliders: none of them shields a body.
        source.clear();
        if let Some(ignored) = explosion.ignore {
            match bodies.get(ignored) {
                Ok((_, handle, _, _)) => own_entities(&world, ignored, *handle, &[], &mut source),
                Err(_) => source.push(ignored),
            }
        }
        if explosion.gust && cloth_in_range(cloth.as_deref(), explosion) {
            spawn_gust(&mut commands, explosion);
            stats.gusts += 1;
        }
        // Debris is cosmetic and each process simulates its own, so a replica pushes it too.
        if let Some(d) = destruction.as_deref() {
            for chunk in d.physics.active() {
                let meets = world.colliders.get(chunk.collider).is_some_and(|collider| {
                    let aabb = collider.compute_aabb();
                    let mins = from_rapier_vec3(aabb.mins);
                    let maxs = from_rapier_vec3(aabb.maxs);
                    let closest = explosion.center.clamp(mins, maxs);
                    closest.distance_squared(explosion.center) <= explosion.radius * explosion.radius
                });
                if !meets {
                    continue;
                }
                let handle = RigidBodyHandle::from_rapier(chunk.body);
                #[cfg(test)]
                if let Some(counter) = facts_calls.as_mut() {
                    counter.calls += 1;
                }
                let Some(body) = body_facts(&world, handle) else {
                    continue;
                };
                let distance = (body.com - explosion.center).length();
                let gain = explosion.falloff.gain(distance, explosion.radius);
                if gain <= 0.0 {
                    continue;
                }
                let entity = d
                    .chunk_entity(chunk.destructible, chunk.chunk)
                    .unwrap_or(Entity::PLACEHOLDER);
                if explosion.occlusion == Occlusion::RayCast && distance > OCCLUSION_MARGIN {
                    own_entities(&world, entity, handle, &source, &mut skip);
                    if occluded(&query, &world, &kinds, &debris, explosion, &skip, body.com) {
                        stats.bodies_occluded += 1;
                        continue;
                    }
                }
                let Some((linear, torque)) = push_of(explosion, &body, gain) else {
                    continue;
                };
                let slot = flying.entry((chunk.destructible, chunk.chunk)).or_insert((
                    handle,
                    Vec3::ZERO,
                    Vec3::ZERO,
                ));
                slot.1 += linear;
                slot.2 += torque;
                stats.chunks_pushed += 1;
                stats.chunks_hit += 1;
                stats.impulse_total += f64::from(linear.length());
            }
        }
        if authority == Authority::Replica {
            stats.skipped_replica += 1;
            continue;
        }
        stats.credit_steps = CREDIT_WINDOW_STEPS;
        if explosion.destruction
            && let Some(d) = destruction.as_deref_mut()
        {
            let damage = Damage {
                target: None,
                kind: DamageKind::Explosion {
                    center: explosion.center.to_array(),
                    radius: explosion.radius,
                    impulse: explosion.impulse,
                },
            };
            if d.world.damage(damage).is_ok() {
                stats.damage_queued += 1;
                stats.chunks_hit += attached_in_reach(d, explosion);
            }
        }
        for (bits, entity, handle) in &order {
            if explosion.ignore == Some(*entity) {
                continue;
            }
            // Radius pre-filter: the falloff is zero from the radius on, measured to the center of
            // mass, so read only that before the full facts (the colliders' bounds). It decides by
            // the body, whatever its colliders are: on child entities, sensors, filtered layers or
            // none at all.
            let Some(com) = world
                .bodies
                .get(handle.to_handle())
                .map(|rb| from_rapier_vec3(rb.center_of_mass()))
            else {
                continue;
            };
            if explosion
                .falloff
                .gain((com - explosion.center).length(), explosion.radius)
                <= 0.0
            {
                continue;
            }
            #[cfg(test)]
            if let Some(counter) = facts_calls.as_mut() {
                counter.calls += 1;
            }
            let Some(body) = body_facts(&world, *handle) else {
                continue;
            };
            let distance = (body.com - explosion.center).length();
            let gain = explosion.falloff.gain(distance, explosion.radius);
            if gain <= 0.0 {
                continue;
            }
            if explosion.occlusion == Occlusion::RayCast && distance > OCCLUSION_MARGIN {
                own_entities(&world, *entity, *handle, &source, &mut skip);
                if occluded(&query, &world, &kinds, &debris, explosion, &skip, body.com) {
                    stats.bodies_occluded += 1;
                    continue;
                }
            }
            let Some((linear, torque)) = push_of(explosion, &body, gain) else {
                continue;
            };
            let slot = pending.entry(*bits).or_insert_with(|| {
                let existing = bodies
                    .get(*entity)
                    .ok()
                    .and_then(|(_, _, _, e)| e.cloned())
                    .unwrap_or_default();
                (*entity, existing)
            });
            slot.1.impulse += linear;
            slot.1.torque_impulse += torque;
            stats.bodies_hit += 1;
            stats.impulse_total += f64::from(linear.length());
        }
    }
    for (_, (entity, impulse)) in pending {
        commands.entity(entity).insert(impulse);
    }
    if !flying.is_empty() {
        // Debris bodies have no entity with a body handle, so the impulses go straight to rapier,
        // before the physics step of this tick.
        commands.queue(move |world: &mut World| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            for (handle, linear, torque) in flying.into_values() {
                if let Some(rb) = physics.bodies.get_mut(handle.to_handle()) {
                    rb.apply_impulse(to_rapier_vec3(linear), true);
                    rb.apply_torque_impulse(to_rapier_vec3(torque), true);
                }
            }
        });
    }
}

/// Whether static or kinematic geometry lies between the center and `com`. Flying debris shields
/// nothing: it is dynamic, though its entity has no [`RigidBody`]. Colliders owned by `skip` (the
/// body itself with its child colliders, and the ignored source with its own) shield nothing.
fn occluded(
    query: &PhysicsQuery,
    world: &PhysicsWorld,
    kinds: &KindQuery,
    debris: &Query<(), With<DestructionChunk>>,
    explosion: &Explosion,
    skip: &[Entity],
    com: Vec3,
) -> bool {
    // The same distance the caller measured the falloff with.
    let distance = (com - explosion.center).length();
    // A collider on a child entity takes the kind of the body it belongs to.
    let solid = |e: Entity| {
        !debris.contains(e)
            && match kinds.get(e) {
                Ok((Some(body), _)) => body.kind != BodyKind::Dynamic,
                Ok((None, Some(collider))) => !world
                    .colliders
                    .get(collider.to_handle())
                    .and_then(|c| c.parent())
                    .and_then(|parent| world.bodies.get(parent))
                    .is_some_and(|rb| rb.is_dynamic()),
                _ => true,
            }
    };
    let filter = QueryFilter::new()
        .exclude_sensors()
        .exclude_entities(skip)
        .predicate(&solid);
    query
        .raycast(
            explosion.center,
            com - explosion.center,
            distance - OCCLUSION_MARGIN,
            filter,
        )
        .is_some()
}

/// `entity` and the entities of `handle`'s colliders into `skip` (cleared first, then `extra`): a
/// compound body's child colliders register under the child entities, not the body's.
fn own_entities(
    world: &PhysicsWorld,
    entity: Entity,
    handle: RigidBodyHandle,
    extra: &[Entity],
    skip: &mut Vec<Entity>,
) {
    skip.clear();
    skip.extend_from_slice(extra);
    skip.push(entity);
    if let Some(rb) = world.bodies.get(handle.to_handle()) {
        skip.extend(rb.colliders().iter().filter_map(|h| world.entity_of_collider(*h)));
    }
}

/// Every entity's body kind, or the collider handle of a collider on a child entity: what decides
/// whether it shields.
type KindQuery<'w, 's> = Query<'w, 's, (Option<&'static RigidBody>, Option<&'static ColliderHandle>)>;

/// Where the blast reaches a body with bounds `lo..hi`: the point of the box on the line from
/// the center of mass toward the center of the explosion.
fn hit_point(lo: Vec3, hi: Vec3, com: Vec3, center: Vec3) -> Vec3 {
    let to_center = center - com;
    let Some(dir) = to_center.try_normalize() else {
        return com;
    };
    if !(lo.is_finite() && hi.is_finite()) {
        return com;
    }
    // Exit distance of the ray from the center of mass through the box (slab method).
    let mut t = f32::INFINITY;
    for axis in 0..3 {
        let d = dir[axis];
        if d.abs() > 1e-6 {
            let edge = if d > 0.0 { hi[axis] } else { lo[axis] };
            t = t.min((edge - com[axis]) / d);
        }
    }
    if !t.is_finite() || t < 0.0 {
        return com;
    }
    com + dir * t.min(to_center.length())
}

/// Whether any cloth solver's centroid lies inside the blast radius (no cloth set: no gust).
fn cloth_in_range(cloth: Option<&ClothSet>, explosion: &Explosion) -> bool {
    cloth.is_some_and(|set| {
        set.iter()
            .any(|(_, solver)| solver.centroid().distance(explosion.center) < explosion.radius)
    })
}

fn spawn_gust(commands: &mut Commands, explosion: &Explosion) {
    let strength = (explosion.peak_delta_v() * 0.5).clamp(1.0, MAX_WIND_SPEED);
    commands.spawn((
        Transform::from_translation(explosion.center),
        GlobalTransform(Affine3A::from_translation(explosion.center)),
        WindField {
            kind: WindFieldKind::Radial {
                strength,
                falloff: 1.0,
            },
            radius: Some(explosion.radius),
            gust: None,
        },
        ExplosionGust {
            steps_left: GUST_STEPS,
        },
    ));
}

/// Counts the gusts down and removes the spent ones.
fn expire_gusts(mut commands: Commands, mut gusts: Query<(Entity, &mut ExplosionGust)>) {
    for (entity, mut gust) in &mut gusts {
        if gust.steps_left == 0 {
            commands.entity(entity).despawn();
        } else {
            gust.steps_left -= 1;
        }
    }
}
