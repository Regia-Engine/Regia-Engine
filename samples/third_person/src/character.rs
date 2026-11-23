// SPDX-License-Identifier: MIT-0
//! The player character: a rapier-controller capsule plus the animated Fox rig that follows
//! it. One character per joined player.

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::character::{Character, CharacterInput, CharacterMovement, CharacterState};
use regia_framework::player::{Player, PlayerController, PlayerManager};
use regia_framework::prelude::PlayerStart;
use regia_physics::{BodyKind, CharacterBody, Collider, ColliderShape, RigidBody};
use regia_scene::Transform;

use crate::anim;

/// Uniform scale applied to the Fox (cooked in centimetres).
pub const FOX_SCALE: f32 = 0.017;

/// Capsule half-height (metres). Matches the body this module spawns.
pub const CAPSULE_HALF_HEIGHT: f32 = 0.5;

/// Capsule radius (metres). Matches the body this module spawns.
pub const CAPSULE_RADIUS: f32 = 0.3;

/// How far below the capsule centre the rig origin must sit so the bind-pose
/// mesh's lowest vertex rests on the ground (metres).
///
/// `mesh_min_y` is the cooked mesh's minimum Y in skeleton units.
#[must_use]
pub fn rig_drop(mesh_min_y: f32) -> f32 {
    CAPSULE_HALF_HEIGHT + CAPSULE_RADIUS + mesh_min_y * FOX_SCALE
}

/// Rotation aligning the Fox's +X forward with Regia Engine's −Z forward.
pub fn fox_to_regia_rotation() -> Quat {
    // +X → −Z is a −90° yaw.
    Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2)
}

/// Marker on characters that already have their visual rig + camera (idempotent spawning).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct HasRig;

/// Spawns the player 0 character at startup (creating the player + controller first), and
/// characters for players who join later. Idempotent: a player with a living character is
/// skipped.
pub fn spawn_for_players(world: &mut World) {
    // Player 0 (keyboard + mouse) exists from the first frame.
    if world
        .get_resource::<PlayerManager>()
        .is_none_or(|m| m.count() == 0)
    {
        let _ = PlayerManager::spawn_player_static(world, regia_framework::player::PlayerKind::Local);
    }
    let players: Vec<u8> = world
        .get_resource::<PlayerManager>()
        .map(|m| m.iter().map(|(index, _)| index).collect())
        .unwrap_or_default();
    for index in players {
        if !character_alive(world, index) {
            let _ = spawn_player_character(world, index);
        }
    }
}

/// Whether `player`'s controller possesses a live character.
fn character_alive(world: &World, player: u8) -> bool {
    let Some(manager) = world.get_resource::<PlayerManager>() else {
        return false;
    };
    let Some(player_entity) = manager.player_entity(player) else {
        return false;
    };
    let Some(player_data) = world.get::<Player>(player_entity) else {
        return false;
    };
    world
        .get::<PlayerController>(player_data.controller)
        .is_some_and(|pc| world.get::<Character>(pc.possessed).is_some())
}

/// Spawns (or respawns) the character for `player`, possessing it through the player's
/// controller and attaching the animated rig + camera. Returns the character entity.
pub fn spawn_player_character(world: &mut World, player: u8) -> Option<Entity> {
    let character = spawn_capsule(world, player)?;
    crate::car::tag_player(world, character);
    crate::demo_fx::track_character(world, character, player);
    crate::cloth_fx::attach_cape(world, character, player);

    // The visual rig (best effort — the level stays playable without it).
    if let Some(fox) = world.remove_resource::<crate::assets::FoxAssets>() {
        anim::attach_rig(world, character, player, &fox);
        world.insert_resource(fox);
    } else {
        tracing::warn!(target: "third_person", "Fox assets unavailable; capsule-only character");
    }

    // Possess through the player's controller when one exists.
    if let Some(manager) = world.get_resource::<PlayerManager>()
        && let Some(player_entity) = manager.player_entity(player)
        && let Some(player_data) = world.get::<Player>(player_entity)
    {
        let controller = player_data.controller;
        if let Some(mut pc) = world.get_mut::<PlayerController>(controller) {
            pc.possess(character);
        }
    }
    Some(character)
}

/// The capsule character body at the player's `PlayerStart`.
fn spawn_capsule(world: &mut World, player: u8) -> Option<Entity> {
    let mut starts: Vec<Vec3> = world
        .query_filtered::<&Transform, With<PlayerStart>>()
        .iter(world)
        .map(|t| t.translation)
        .collect();
    starts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.z.total_cmp(&b.z)));
    let position = starts
        .get(usize::from(player))
        .copied()
        .unwrap_or(Vec3::new(0.0, 1.5, 0.0));
    let movement = CharacterMovement {
        radius: CAPSULE_RADIUS,
        half_height: CAPSULE_HALF_HEIGHT,
        max_walk_speed: 4.5,
        sprint_speed_multiplier: 1.8,
        jump_velocity: 6.5,
        max_jumps: 2,
        step_height: 0.35,
        ..CharacterMovement::default()
    };
    Some(
        world
            .spawn((
                Transform {
                    translation: position,
                    ..Default::default()
                },
                Character,
                movement,
                CharacterInput::default(),
                CharacterState::default(),
                // A kinematic body + capsule collider so sensor volumes see the character;
                // the controller never sweeps against it (excluded by body handle).
                RigidBody {
                    kind: BodyKind::Kinematic,
                    ..Default::default()
                },
                Collider {
                    shape: ColliderShape::Capsule {
                        half_height: CAPSULE_HALF_HEIGHT,
                        radius: CAPSULE_RADIUS,
                        axis: regia_physics::ShapeAxis::Y,
                    },
                    ..Default::default()
                },
                // The ECS-side controller owns this transform; the physics interpolation
                // would fight it (see `regia_physics::sync::interpolate_transforms`).
                regia_physics::NoInterpolation,
                CharacterBody::default(),
            ))
            .id(),
    )
}
