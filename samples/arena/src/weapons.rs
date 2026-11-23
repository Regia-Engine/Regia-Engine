// SPDX-License-Identifier: MIT-0
//! Weapons (`docs/specs/14` M20 `samples/arena`): hitscan rifles and pooled rocket projectiles,
//! plus the explosion that damages pawns and shoves dynamic props.
//!
//! * **Hitscan** ([`Weapon::Rifle`]): one raycast per shot; the first collider hit takes damage.
//! * **Pooled projectiles** ([`Weapon::Rocket`]): spawned through [`PrefabPool`]; each fixed step
//!   the projectile sweeps its path with a [`WorldQueries::shapecast`] and explodes on impact.
//! * **Explosions**: an [`ExplosionRequest`] damages every pawn in a radius (linear falloff) and
//!   applies an [`ExternalImpulse`] to every dynamic body in range (the "explosion impulse").
//!
//! Impact VFX, decals and spatial audio are written as messages (`SpawnEffect`, `SpawnDecal`)
//! and one-shot `Audio` plays — consumed by the VFX/audio plugins when installed, no-ops
//! otherwise, so headless combat is still deterministic.
//!
//! The combat systems are exclusive (`&mut World`) so they can both borrow the physics world for
//! queries (through [`WorldQueries`]) and mutate the [`PrefabPool`] / damage / impulse state.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::damage::Health;
use regia_framework::damage::damage;
use regia_framework::pool::PrefabPool;
use regia_physics::queries::{QueryFilter, WorldQueries};
use regia_physics::{BodyKind, ColliderShape, ExternalImpulse, RigidBody};
use regia_scene::Transform;

use crate::audio::{ArenaAudio, SoundKind};

/// The weapon a [`FireRequest`] asks to fire.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Default)]
pub enum Weapon {
    /// Hitscan rifle.
    #[default]
    Rifle,
    /// Pooled rocket projectile.
    Rocket,
}

/// A request to fire a weapon (written by the player-input and bot systems).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct FireRequest {
    /// The firing pawn.
    pub shooter: Entity,
    /// Muzzle position (world space).
    pub position: Vec3,
    /// Fire direction (world space, need not be normalised).
    pub direction: Vec3,
    /// Which weapon.
    pub weapon: Weapon,
}

/// A live pooled projectile.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component)]
pub struct Projectile {
    /// Velocity (m/s).
    pub velocity: Vec3,
    /// Damage on a direct hit.
    pub damage: f32,
    /// Explosion radius (m).
    pub radius: f32,
    /// Who fired it.
    pub source: Entity,
    /// Seconds left before it fizzles.
    pub lifetime: f32,
}

/// A request to detonate an explosion at a point (written on projectile impact, grenades, etc.).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct ExplosionRequest {
    /// Centre of the explosion.
    pub position: Vec3,
    /// Blast radius (m).
    pub radius: f32,
    /// Maximum damage at the centre (falls off linearly to 0 at the radius).
    pub damage: f32,
    /// Who caused it (for kill attribution).
    pub source: Entity,
    /// Impulse magnitude applied to dynamic bodies (kg·m/s).
    pub impulse: f32,
}

/// Rifle damage per hit.
pub const RIFLE_DAMAGE: f32 = 25.0;
/// Rocket direct/explosion damage.
pub const ROCKET_DAMAGE: f32 = 100.0;
/// Rocket explosion radius.
pub const ROCKET_RADIUS: f32 = 4.0;
/// Rocket speed (m/s).
pub const ROCKET_SPEED: f32 = 30.0;
/// Projectile lifetime (s).
pub const PROJECTILE_LIFE: f32 = 3.0;
/// Maximum hitscan range (m).
pub const HITSCAN_RANGE: f32 = 100.0;
/// The [`PrefabPool`] key for rockets.
pub const PROJECTILE_POOL: &str = "rocket";

/// How many shots the match has resolved, split by weapon.
///
/// Bots fire a pooled rocket every fourth shot. Tests (and the bench) read this instead of
/// racing the projectile's one-frame lifetime.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct ShotLog {
    /// Hitscan rifle shots.
    pub rifles: u32,
    /// Pooled rocket shots.
    pub rockets: u32,
}

/// Installs the weapon systems and the projectile pool.
pub struct WeaponsPlugin;

impl Plugin for WeaponsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<FireRequest>()
            .add_message::<ExplosionRequest>()
            .init_resource::<PrefabPool>()
            .init_resource::<ShotLog>()
            .add_systems(
                FixedUpdate,
                (resolve_fire_requests, advance_projectiles, apply_explosions)
                    .chain()
                    .in_set(crate::ArenaSimSet::Weapons),
            );
    }
}

/// The read position of [`drain`] for messages of type `M`, kept between calls.
#[derive(Resource)]
struct DrainCursor<M: Message>(MessageCursor<M>);

/// Every message of type `M` written since the last call, each exactly once.
///
/// `Messages` keeps two frames of messages, so a fresh cursor would read last frame's messages a
/// second time: a fire request, an explosion or a death was handled twice, and with several
/// fixed steps in one frame more often (the outcome then depended on the frame rate, ADR-537).
/// The cursor is kept in a resource; other readers of the same messages are not affected.
pub(crate) fn drain<M: Message + Clone>(world: &mut World) -> Vec<M> {
    let mut cursor = world
        .remove_resource::<DrainCursor<M>>()
        .map_or_else(MessageCursor::default, |held| held.0);
    let out = world
        .get_resource::<Messages<M>>()
        .map(|messages| cursor.read(messages).cloned().collect())
        .unwrap_or_default();
    world.insert_resource(DrainCursor(cursor));
    out
}

/// Builds a [`WorldQueries`] over the world's physics resources.
fn queries(world: &World) -> WorldQueries<'_> {
    WorldQueries {
        world: world.resource::<regia_physics::PhysicsWorld>(),
        layers: world.get_resource::<regia_physics::PhysicsLayers>(),
    }
}

/// `FixedUpdate`: fires rifles (hitscan) and spawns rockets (pooled).
#[tracing::instrument(skip_all, target = "arena", name = "weapons_fire")]
fn resolve_fire_requests(world: &mut World) {
    let requests = drain::<FireRequest>(world);
    for req in requests {
        if let Some(mut log) = world.get_resource_mut::<ShotLog>() {
            match req.weapon {
                Weapon::Rifle => log.rifles = log.rifles.saturating_add(1),
                Weapon::Rocket => log.rockets = log.rockets.saturating_add(1),
            }
        }
        match req.weapon {
            Weapon::Rifle => fire_hitscan(world, req),
            Weapon::Rocket => spawn_rocket(world, req),
        }
    }
}

/// Resolves one rifle shot: raycast, damage, impact VFX/decals and a spatial gunshot.
fn fire_hitscan(world: &mut World, req: FireRequest) {
    let dir = req.direction.normalize_or_zero();
    let hit = queries(world).raycast(
        req.position,
        dir,
        HITSCAN_RANGE,
        QueryFilter::new().exclude_entities(&[req.shooter]),
    );
    let Some(hit) = hit else {
        return;
    };

    if world.get::<Health>(hit.entity).is_some() {
        damage(
            world,
            hit.entity,
            RIFLE_DAMAGE,
            Some(req.shooter),
            Some(hit.point),
        );
    }
    spawn_impact(world, hit.point, hit.normal, false);
}

/// Spawns (or reuses) a pooled rocket for `req`.
fn spawn_rocket(world: &mut World, req: FireRequest) {
    let dir = req.direction.normalize_or_zero();
    let velocity = dir * ROCKET_SPEED;

    let mut pool = world.remove_resource::<PrefabPool>().unwrap_or_default();
    let entity = pool.acquire(world, PROJECTILE_POOL, |world| {
        world.spawn(Transform::default()).id()
    });
    world.insert_resource(pool);

    world.entity_mut(entity).insert((
        Transform {
            translation: req.position,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        },
        Projectile {
            velocity,
            damage: ROCKET_DAMAGE,
            radius: ROCKET_RADIUS,
            source: req.shooter,
            lifetime: PROJECTILE_LIFE,
        },
    ));
    world.write_message(regia_vfx::SpawnEffect {
        effect: "muzzle_flash".into(),
        position: req.position,
        forward: dir,
        seed: None,
    });
    play_sound(world, req.position, SoundKind::Gunshot);
}

/// `FixedUpdate`: advances pooled projectiles, sweeping their path for impacts.
#[tracing::instrument(skip_all, target = "arena", name = "weapons_projectiles")]
fn advance_projectiles(world: &mut World) {
    // One fixed step (`FixedUpdate`), not the frame delta.
    let dt = world
        .get_resource::<FixedTime>()
        .map_or(1.0 / 60.0, |fixed| fixed.step().as_secs_f32());

    let projectiles: Vec<(Entity, Projectile, Transform)> = {
        let mut q = world.query::<(Entity, &Projectile, &Transform)>();
        q.iter(world).map(|(e, p, t)| (e, p.clone(), *t)).collect()
    };

    for (entity, projectile, transform) in projectiles {
        let lifetime = projectile.lifetime - dt;
        let dir = projectile.velocity.normalize_or_zero();
        let distance = projectile.velocity.length() * dt;
        let shape = ColliderShape::Sphere { radius: 0.2 };
        let sweep = if distance > 0.0 {
            queries(world).shapecast(
                &shape,
                transform,
                dir,
                distance,
                QueryFilter::new().exclude_entities(&[projectile.source, entity]),
            )
        } else {
            None
        };

        if let Some(hit) = sweep {
            // Impact: explode and recycle the projectile.
            let impact = transform.translation + dir * hit.time_of_impact;
            let req = ExplosionRequest {
                position: impact,
                radius: projectile.radius,
                damage: projectile.damage,
                source: projectile.source,
                impulse: 8.0,
            };
            explode(world, req);
            recycle_projectile(world, entity);
            continue;
        }

        if lifetime <= 0.0 {
            // Fizzled out: recycle without an explosion.
            recycle_projectile(world, entity);
            continue;
        }

        // Advance.
        if let Some(mut t) = world.get_mut::<Transform>(entity) {
            t.translation += projectile.velocity * dt;
        }
        if let Some(mut p) = world.get_mut::<Projectile>(entity) {
            p.lifetime = lifetime;
        }
    }
}

/// Recycles a pooled projectile: removes its live marker and hides it.
fn recycle_projectile(world: &mut World, entity: Entity) {
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.remove::<Projectile>();
    }
    let mut pool = world.remove_resource::<PrefabPool>().unwrap_or_default();
    pool.recycle(world, PROJECTILE_POOL, entity);
    world.insert_resource(pool);
    // Park it below the floor so it does not render or collide.
    if let Some(mut t) = world.get_mut::<Transform>(entity) {
        t.translation = Vec3::new(0.0, -1000.0, 0.0);
    }
}

/// `FixedUpdate`: detonates explosions — damage in a radius + impulse to dynamic bodies.
#[tracing::instrument(skip_all, target = "arena", name = "weapons_explosions")]
fn apply_explosions(world: &mut World) {
    let requests = drain::<ExplosionRequest>(world);
    for req in requests {
        explode(world, req);
    }
}

/// Applies one explosion: radius damage to pawns and an outward impulse to dynamic props.
fn explode(world: &mut World, explosion: ExplosionRequest) {
    // Visual + audio first.
    spawn_impact(world, explosion.position, Vec3::Y, true);

    let shape = ColliderShape::Sphere {
        radius: explosion.radius,
    };
    let pose = Transform::from_translation(explosion.position);
    let in_radius = queries(world).overlap(&shape, pose, QueryFilter::new());

    for entity in in_radius {
        // Damage pawns with linear falloff.
        if world.get::<Health>(entity).is_some() {
            let dist = world
                .get::<Transform>(entity)
                .map(|t| (t.translation - explosion.position).length())
                .unwrap_or(0.0);
            let falloff = (1.0 - dist / explosion.radius.max(1.0e-3)).clamp(0.0, 1.0);
            damage(
                world,
                entity,
                explosion.damage * falloff,
                Some(explosion.source),
                Some(explosion.position),
            );
        }
        // Shove dynamic props.
        if let Some(body) = world.get::<RigidBody>(entity)
            && body.kind == BodyKind::Dynamic
        {
            let dir = world
                .get::<Transform>(entity)
                .map(|t| (t.translation - explosion.position).normalize_or_zero())
                .unwrap_or(Vec3::Y);
            if let Ok(mut e) = world.get_entity_mut(entity) {
                e.insert(ExternalImpulse {
                    impulse: dir * explosion.impulse,
                    torque_impulse: Vec3::ZERO,
                });
            }
        }
    }
}

/// Spawns the impact/explosion VFX, a scorch decal, and a spatial sound cue.
fn spawn_impact(world: &mut World, position: Vec3, normal: Vec3, explosion: bool) {
    let effect = if explosion { "explosion" } else { "impact" };
    world.write_message(regia_vfx::SpawnEffect {
        effect: effect.into(),
        position,
        forward: normal,
        seed: None,
    });
    let half = if explosion { 1.5 } else { 0.4 };
    world.write_message(regia_vfx::SpawnDecal {
        position,
        rotation: Quat::from_rotation_arc(Vec3::Z, normal),
        half_size: Vec3::new(half, half, 0.1),
        color: [0.1, 0.1, 0.12, 1.0],
        lifetime: Some(if explosion { 6.0 } else { 3.0 }),
    });
    let kind = if explosion {
        SoundKind::Explosion
    } else {
        SoundKind::Gunshot
    };
    play_sound(world, position, kind);
}

/// Plays a spatial one-shot if the audio backend and arena clips exist.
fn play_sound(world: &mut World, position: Vec3, kind: SoundKind) {
    let clip = match world.get_resource::<ArenaAudio>() {
        Some(clips) => match kind {
            SoundKind::Gunshot => clips.gunshot.clone(),
            SoundKind::Explosion => clips.explosion.clone(),
        },
        None => return,
    };
    if let Some(mut audio) = world.get_resource_mut::<regia_audio::Audio>() {
        let _ = audio.play_at(&clip, position);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weapon_defaults_to_rifle() {
        assert_eq!(Weapon::default(), Weapon::Rifle);
    }

    #[test]
    fn weapon_kinds_differ() {
        assert_ne!(Weapon::Rifle, Weapon::Rocket);
    }

    #[test]
    fn a_pooled_rocket_is_acquired_from_the_prefab_pool() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, WeaponsPlugin));
        enable_fixed_step(&mut app, 60.0);
        let shooter = app.world_mut().spawn(Transform::default()).id();
        app.world_mut().write_message(FireRequest {
            shooter,
            position: Vec3::new(0.0, 1.6, 0.0),
            direction: Vec3::NEG_Z,
            weapon: Weapon::Rocket,
        });
        app.update();
        let rockets = {
            let world = app.world_mut();
            let mut projectiles = world.query::<&Projectile>();
            projectiles.iter(world).count()
        };
        assert_eq!(rockets, 1, "the rocket is live");
        let world = app.world();
        assert_eq!(world.resource::<ShotLog>().rockets, 1);
        assert_eq!(
            world.resource::<PrefabPool>().live_len(PROJECTILE_POOL),
            1,
            "the projectile came from the rocket pool"
        );
    }

    #[test]
    fn explosion_writes_external_impulse_on_a_dynamic_crate() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, WeaponsPlugin));
        enable_fixed_step(&mut app, 60.0);
        let crate_entity = crate::props::spawn_crate(app.world_mut(), 0, 0.0, 0.0, None);
        // One step registers the crate's collider with the query pipeline.
        app.update();
        explode(
            app.world_mut(),
            ExplosionRequest {
                position: Vec3::new(0.0, 0.5, 1.2),
                radius: ROCKET_RADIUS,
                damage: ROCKET_DAMAGE,
                source: Entity::PLACEHOLDER,
                impulse: 8.0,
            },
        );
        let impulse = app
            .world()
            .get::<ExternalImpulse>(crate_entity)
            .expect("the explosion inserts ExternalImpulse before physics consumes it");
        assert!(impulse.impulse.length() > 0.1, "impulse {:?}", impulse.impulse);
    }
}
