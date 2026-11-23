// SPDX-License-Identifier: MIT-0
//! Pawns: the player and the bots, plus their controllers.
//!
//! A pawn is a kinematic capsule (sensor collider) with [`Health`]. Its `Transform` is driven
//! directly by the player-input and bot-AI systems (steering — no navmesh, no character
//! controller; the arena floor is flat). The sensor capsule makes the pawn hittable by the
//! line-of-sight and hitscan raycasts without pushing other bodies.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::damage::Health;
use regia_framework::player::{AiController, Player, PlayerController, PlayerKind};
use regia_framework::pool::PooledInactive;
use regia_physics::{BodyKind, Collider, ColliderShape, RigidBody};
use regia_scene::Transform;

use crate::config::MatchRng;

/// Marker: this entity is a combatant (a player or a bot).
#[derive(Component, Reflect, Debug, Clone, PartialEq, Eq)]
#[reflect(Component)]
pub struct Pawn {
    /// Player slot index (0 = local player, 1.. = bots).
    pub slot: u8,
    /// Whether a bot drives this pawn.
    pub is_bot: bool,
}

/// Per-bot combat state, driven by `drive_bots`.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct BotBrain {
    /// Firing cooldown remaining (seconds).
    pub cooldown: f32,
    /// Current steering target (the pawn this bot hunts).
    pub target: Option<Entity>,
    /// Seconds until the bot re-picks a target (avoids thrashing).
    pub repick: f32,
    /// Shots fired. Every fourth shot is a pooled rocket.
    pub shots: u32,
}

/// The pawn's standing height above the floor (the capsule centre y).
pub const PAWN_EYE_HEIGHT: f32 = 1.6;
/// Capsule radius for hit detection.
pub const PAWN_RADIUS: f32 = 0.35;

/// Spawns a pawn (no controller) at `position` and returns it.
///
/// The pawn is a kinematic body with a sensor capsule collider (for raycast hits) and full
/// [`Health`]. It does not yet have a [`PlayerController`] or [`AiController`]; the caller wires
/// those.
pub fn spawn_pawn(world: &mut World, slot: u8, is_bot: bool, position: Vec3) -> Entity {
    world
        .spawn((
            Transform {
                translation: Vec3::new(position.x, PAWN_EYE_HEIGHT, position.z),
                ..Default::default()
            },
            regia_scene::components::Name::new(if is_bot {
                format!("bot_{slot}")
            } else {
                "player".to_string()
            }),
            RigidBody {
                kind: BodyKind::Kinematic,
                gravity_scale: 0.0,
                can_sleep: false,
                ..RigidBody::default()
            },
            Collider {
                shape: ColliderShape::Capsule {
                    half_height: 0.5,
                    radius: PAWN_RADIUS,
                    axis: regia_physics::ShapeAxis::Y,
                },
                sensor: true,
                friction: 0.0,
                restitution: 0.0,
                density: 0.0,
                ..Collider::default()
            },
            Health::new(100.0),
            Pawn { slot, is_bot },
            // The slot's seat in the hosted match session (slot `s` is team `s + 1`, seat slot 0):
            // the session reads its position, and the damage step credits its kills.
            regia_framework::rules::TeamId(crate::match_flow::slot_team(slot)),
            regia_framework::rules::LedgerSlot(0),
        ))
        .id()
}

/// Spawns the local player's [`Player`] + [`PlayerController`] and makes it possess `pawn`.
pub fn spawn_player(world: &mut World, pawn: Entity) -> (Entity, Entity) {
    let controller = world
        .spawn((
            PlayerController {
                player_index: 0,
                possessed: pawn,
                view_target: pawn,
                ..PlayerController::default()
            },
            Transform::default(),
        ))
        .id();
    let player = world
        .spawn(Player {
            index: 0,
            kind: PlayerKind::Local,
            controller,
            device: None,
        })
        .id();
    (player, controller)
}

/// Spawns an [`AiController`] driving `pawn` for bot slot `slot`, and attaches a [`BotBrain`] to
/// the pawn (the per-bot combat state the AI system reads).
pub fn spawn_bot(world: &mut World, pawn: Entity, slot: u8) -> Entity {
    world.entity_mut(pawn).insert(BotBrain::default());
    world
        .spawn((
            AiController::new(pawn),
            regia_scene::components::Name::new(format!("bot_{slot}_brain")),
        ))
        .id()
}

/// Whether a pawn entity is currently active (not pooled/despawned).
pub fn pawn_is_alive(world: &World, pawn: Entity) -> bool {
    world.get_entity(pawn).is_ok()
        && world.get::<PooledInactive>(pawn).is_none()
        && world.get::<Health>(pawn).is_some_and(|h| !h.is_dead())
}

/// Draws a uniformly distributed random point inside the arena (bots wander between targets).
pub fn random_arena_point(rng: &mut MatchRng, half_extent: f32) -> Vec3 {
    let x = (rng.rng.f32() * 2.0 - 1.0) * half_extent;
    let z = (rng.rng.f32() * 2.0 - 1.0) * half_extent;
    Vec3::new(x, 0.0, z)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pawn_is_spawned_with_health_and_a_sensor_capsule() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let pawn = spawn_pawn(app.world_mut(), 1, true, Vec3::new(2.0, 0.0, 3.0));
        let world = app.world_mut();
        assert_eq!(world.get::<Pawn>(pawn).map(|p| p.slot), Some(1));
        assert_eq!(world.get::<Pawn>(pawn).map(|p| p.is_bot), Some(true));
        assert_eq!(world.get::<Health>(pawn).map(|h| h.current), Some(100.0));
        let collider = world.get::<Collider>(pawn).expect("collider");
        assert!(collider.sensor, "pawn capsule is a sensor");
        let body = world.get::<RigidBody>(pawn).expect("body");
        assert_eq!(body.kind, BodyKind::Kinematic);
        // The pawn rests at eye height.
        assert_eq!(
            world.get::<Transform>(pawn).expect("transform").translation.y,
            PAWN_EYE_HEIGHT
        );
    }

    #[test]
    fn pawn_is_alive_until_it_dies_or_is_pooled() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let pawn = spawn_pawn(app.world_mut(), 0, false, Vec3::ZERO);
        assert!(pawn_is_alive(app.world(), pawn));
        app.world_mut().get_mut::<Health>(pawn).expect("health").current = 0.0;
        assert!(!pawn_is_alive(app.world(), pawn));
        app.world_mut().get_mut::<Health>(pawn).expect("health").current = 100.0;
        app.world_mut().entity_mut(pawn).insert(PooledInactive);
        assert!(!pawn_is_alive(app.world(), pawn));
    }

    #[test]
    fn random_arena_point_stays_in_bounds() {
        let mut rng = MatchRng::new(1);
        for _ in 0..1000 {
            let p = random_arena_point(&mut rng, 8.0);
            assert!(p.x >= -8.0 && p.x <= 8.0);
            assert!(p.z >= -8.0 && p.z <= 8.0);
        }
    }
}
