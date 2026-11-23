// SPDX-License-Identifier: MIT-0
//! AI bots (`docs/specs/14` M20 `samples/arena`): steering plus line-of-sight raycasts — no
//! navmesh (Tier 2).
//!
//! Each bot ([`BotBrain`] on an `AiController`-driven pawn) picks the nearest alive enemy,
//! steers toward it with a little seeded jitter, and fires when a line-of-sight raycast to the
//! enemy's chest is unobstructed. The raycast uses the [`PhysicsQuery`] system parameter, so a
//! wall or prop between the bot and its target blocks the shot.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::queries::{PhysicsQuery, QueryFilter};
use regia_scene::Transform;

use crate::config::MatchRng;
use crate::match_flow::Phase;
use crate::pawn::{BotBrain, Pawn};
use crate::weapons::FireRequest;

/// Bot steering speed (m/s).
const BOT_SPEED: f32 = 3.5;
/// Maximum distance a bot will engage an enemy (m).
const ENGAGE_RANGE: f32 = 30.0;
/// Firing cooldown between shots (s).
const FIRE_COOLDOWN: f32 = 0.6;
/// Every Nth shot is a pooled rocket; the rest are hitscan. Deterministic (no extra RNG).
const ROCKET_EVERY: u32 = 4;
/// Seconds between target re-picks.
const REPICK_SECS: f32 = 1.0;

/// Installs the bot AI system.
pub struct BotsPlugin;

impl Plugin for BotsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(FixedUpdate, drive_bots.in_set(crate::ArenaSimSet::Bots));
    }
}

/// `FixedUpdate`: steers every bot toward its target and fires when it has a clear shot.
#[tracing::instrument(skip_all, target = "arena", name = "bots_drive")]
#[allow(clippy::type_complexity)]
fn drive_bots(
    mut param: ParamSet<(
        Query<(Entity, &mut BotBrain, &Pawn, &mut Transform)>,
        Query<(Entity, &Pawn, &Transform)>,
    )>,
    queries: PhysicsQuery,
    mut rng: ResMut<MatchRng>,
    fixed: Res<FixedTime>,
    (config, mode): (
        Option<Res<crate::config::ArenaConfig>>,
        Option<Res<crate::modes::ArenaMatchMode>>,
    ),
    state: Option<Single<&crate::match_flow::ArenaState>>,
    mut fire: MessageWriter<FireRequest>,
) {
    // Team modes: the match core's mode-bot orders drive the bots (`crate::modes::drive_mode_bots`).
    if mode.is_some_and(|m| m.mode.is_team()) {
        return;
    }
    // Navmesh pathing: bots are driven by a behaviour tree instead (specs/22 § T2.2); the
    // default brain is the state tree (`crate::brain`, ADR-2134).
    if config.is_some_and(|c| c.use_navmesh || c.brain != crate::config::BotBrainKind::Steering) {
        return;
    }
    // Bots only fight during the round.
    if state.is_some_and(|s| s.phase != Phase::Round) {
        return;
    }
    // The fixed step, not the frame delta: this runs once per fixed step (ADR-867).
    let dt = fixed.step().as_secs_f32().max(1.0e-4);

    // Snapshot the alive pawns once (bots query them without aliasing the bot query).
    let mut alive: Vec<(Entity, u8, Vec3)> = param
        .p1()
        .iter()
        .map(|(entity, pawn, transform)| (entity, pawn.slot, transform.translation))
        .collect();
    // Entity order is deterministic (spawn order), which keeps target selection stable.
    alive.sort_by_key(|(entity, _, _)| entity.index());

    for (entity, mut brain, own, mut transform) in &mut param.p0() {
        brain.cooldown = (brain.cooldown - dt).max(0.0);
        brain.repick = (brain.repick - dt).max(0.0);

        // Pick a target: the nearest alive pawn of a different slot.
        let self_pos = transform.translation;
        let mut best: Option<(f32, Entity, Vec3)> = None;
        for (other, other_pawn, other_pos) in &alive {
            if *other_pawn == own.slot || *other == entity {
                continue;
            }
            let dist = (*other_pos - self_pos).length();
            if dist <= ENGAGE_RANGE && best.is_none_or(|(d, _, _)| dist < d) {
                best = Some((dist, *other, *other_pos));
            }
        }

        if brain.repick <= 0.0 {
            brain.target = best.map(|(_, e, _)| e);
            brain.repick = REPICK_SECS;
        }
        let target_pos = brain
            .target
            .and_then(|t| alive.iter().find(|(e, _, _)| *e == t).map(|(_, _, p)| *p));

        // Steering: move toward the target (or a wander point) with seeded jitter.
        let destination = match target_pos {
            Some(target) => target,
            None => {
                // Wander toward a random point; pick a new one occasionally.
                if brain.repick <= 0.0 {
                    brain.repick = REPICK_SECS;
                }
                let jitter = Vec3::new(rng.rng.f32() - 0.5, 0.0, rng.rng.f32() - 0.5) * 2.0;
                self_pos + jitter
            }
        };
        let dir = (destination - self_pos).normalize_or_zero();
        if dir.length_squared() > 1.0e-6 {
            let step = dir * BOT_SPEED * dt;
            let next = clamp_to_arena(self_pos + step);
            transform.translation = next;
        }

        // Fire when the target is in sight.
        if let (Some(target_entity), Some(target_pos)) = (brain.target, target_pos) {
            let eye = transform.translation;
            let dir = (target_pos - eye).normalize_or_zero();
            let hit = queries.raycast(
                eye,
                dir,
                ENGAGE_RANGE,
                QueryFilter::new().exclude_entities(&[entity]),
            );
            let clear = hit.is_some_and(|h| h.entity == target_entity);
            if clear && brain.cooldown <= 0.0 {
                brain.shots = brain.shots.saturating_add(1);
                let weapon = if brain.shots % ROCKET_EVERY == 0 {
                    crate::weapons::Weapon::Rocket
                } else {
                    crate::weapons::Weapon::Rifle
                };
                // Seeded spread on rifles. Rockets fly straight; the RNG is still drawn so
                // the stream does not depend on which weapon was chosen.
                let spread = 0.02;
                let jitter = Vec3::new(rng.rng.f32() - 0.5, rng.rng.f32() - 0.5, 0.0) * spread;
                let aim = if weapon == crate::weapons::Weapon::Rifle {
                    (dir + jitter).normalize_or_zero()
                } else {
                    dir
                };
                fire.write(FireRequest {
                    shooter: entity,
                    position: eye,
                    direction: aim,
                    weapon,
                });
                brain.cooldown = FIRE_COOLDOWN;
            }
        }
    }
}

/// Clamps `p` into the arena's horizontal bounds.
pub fn clamp_to_arena(p: Vec3) -> Vec3 {
    let half = crate::match_flow::ARENA_HALF - 0.5;
    Vec3::new(p.x.clamp(-half, half), p.y, p.z.clamp(-half, half))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_blocks_a_bots_line_of_sight_shot() {
        use regia_framework::damage::DamagePlugin;
        use regia_physics::queries::{QueryFilter, WorldQueries};
        use regia_physics::{Collider, ColliderShape, RigidBody};

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_physics::PhysicsPlugin,
            DamagePlugin,
            BotsPlugin,
            crate::weapons::WeaponsPlugin,
        ));
        app.insert_resource(crate::config::MatchRng::new(7));
        app.world_mut().spawn(crate::match_flow::ArenaState {
            phase: Phase::Round,
            scores: vec![0, 0, 0],
            ..crate::match_flow::ArenaState::default()
        });
        enable_fixed_step(&mut app, 60.0);

        let world = app.world_mut();
        let shooter = crate::pawn::spawn_pawn(world, 1, true, Vec3::new(0.0, 0.0, -3.0));
        crate::pawn::spawn_bot(world, shooter, 1);
        let target = crate::pawn::spawn_pawn(world, 2, true, Vec3::new(0.0, 0.0, 3.0));
        crate::pawn::spawn_bot(world, target, 2);
        let wall = world
            .spawn((
                regia_scene::components::Name::new("blocker"),
                Transform::from_translation(Vec3::new(0.0, 1.5, 0.0)),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::new(3.0, 2.0, 0.35),
                    },
                    ..Collider::default()
                },
            ))
            .id();

        // Let rapier register the colliders, then confirm the wall is what the ray hits.
        for _ in 0..3 {
            app.update();
        }
        let eye = app
            .world()
            .get::<Transform>(shooter)
            .expect("shooter")
            .translation;
        let aim = app.world().get::<Transform>(target).expect("target").translation;
        let hit = {
            let world = app.world();
            WorldQueries {
                world: world.resource::<regia_physics::PhysicsWorld>(),
                layers: world.get_resource::<regia_physics::PhysicsLayers>(),
            }
            .raycast(
                eye,
                (aim - eye).normalize_or_zero(),
                30.0,
                QueryFilter::new().exclude_entities(&[shooter]),
            )
        };
        let hit = hit.expect("the ray hits something");
        assert_eq!(hit.entity, wall, "the wall, not the target, blocks the ray");
        let health = app
            .world()
            .get::<regia_framework::damage::Health>(target)
            .expect("hp");
        assert!(
            (health.current - health.max).abs() < 1.0e-3,
            "a blocked bot does not damage its target, health {}",
            health.current
        );

        // Remove the wall. The same bots now have a clear shot and the rifle lands.
        app.world_mut().despawn(wall);
        for _ in 0..90 {
            app.update();
        }
        let health = app
            .world()
            .get::<regia_framework::damage::Health>(target)
            .expect("hp");
        assert!(
            health.current < health.max,
            "without the wall the bot's shot deals damage, health {}",
            health.current
        );
    }

    #[test]
    fn bots_fire_pooled_rockets_during_the_round() {
        use regia_framework::damage::DamagePlugin;

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_physics::PhysicsPlugin,
            DamagePlugin,
            BotsPlugin,
            crate::weapons::WeaponsPlugin,
        ));
        app.insert_resource(crate::config::MatchRng::new(3));
        app.world_mut().spawn(crate::match_flow::ArenaState {
            phase: Phase::Round,
            scores: vec![0, 0, 0],
            ..crate::match_flow::ArenaState::default()
        });
        enable_fixed_step(&mut app, 60.0);

        let world = app.world_mut();
        let a = crate::pawn::spawn_pawn(world, 1, true, Vec3::new(0.0, 0.0, -2.0));
        crate::pawn::spawn_bot(world, a, 1);
        let b = crate::pawn::spawn_pawn(world, 2, true, Vec3::new(0.0, 0.0, 2.0));
        crate::pawn::spawn_bot(world, b, 2);

        // Cooldown is 0.6 s and every fourth shot is a rocket, so five seconds is enough.
        for _ in 0..300 {
            app.update();
        }
        let log = app.world().resource::<crate::weapons::ShotLog>();
        assert!(
            log.rockets > 0,
            "bots fired pooled rockets (rifles {}, rockets {})",
            log.rifles,
            log.rockets
        );
    }

    #[test]
    fn clamp_keeps_points_inside_the_arena() {
        assert_eq!(
            clamp_to_arena(Vec3::new(100.0, 1.6, 0.0)).x,
            crate::match_flow::ARENA_HALF - 0.5
        );
        assert_eq!(
            clamp_to_arena(Vec3::new(-100.0, 1.6, 0.0)).x,
            -(crate::match_flow::ARENA_HALF - 0.5)
        );
        let inside = clamp_to_arena(Vec3::new(0.0, 1.6, 0.0));
        assert_eq!(inside, Vec3::new(0.0, 1.6, 0.0));
    }
}
