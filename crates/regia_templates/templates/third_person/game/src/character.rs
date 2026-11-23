// SPDX-License-Identifier: MIT-0
//! The player: a rapier-controlled capsule character with a procedural body (torso, head, two
//! arms, two feet) that walks with a swing cycle and plants its feet on the ground under them.
//!
//! The foot placement is a small ray-cast IK: each frame a ray from above each foot finds the
//! surface below, and the foot is moved to rest on it, so feet land on stairs and slopes. To use
//! your own animated model instead, replace `attach_rig` and drive your skeleton from
//! `CharacterState` (the engine's third-person sample does this with a skinned fox and the
//! `regia_animation` foot-IK planner).

use std::f32::consts::TAU;

use regia::physics::{CharacterBody, PhysicsQuery, QueryFilter};
use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::camera::{CameraMode, CameraRig, ChaseCamera, ThirdPerson};
use regia_framework::character::{Character, CharacterInput, CharacterMovement, CharacterState};
use regia_render::{Camera, ClearMode, MeshRenderer};
use regia_scene::{GlobalTransform, Transform, components::Name};

use crate::combat::{Fighter, Health, PLAYER_HEALTH, Sword};
use crate::level::{cube_mesh, solid};

/// Capsule half-height of `CharacterMovement::default` (meters); the rig hangs below it.
const HALF_HEIGHT: f32 = 0.9;
/// Half-height of a foot box.
const FOOT_HALF: f32 = 0.06;
/// Local height of a foot at rest (its center), relative to the capsule center.
const FOOT_REST_Y: f32 = -HALF_HEIGHT + FOOT_HALF;
/// How far a foot may move up or down to meet the ground (meters).
const FOOT_REACH: f32 = 0.35;

/// One animated body part of the rig.
#[derive(Component, Debug, Clone, Copy)]
pub struct RigPart {
    /// What it is.
    pub kind: RigKind,
    /// +1 right, -1 left (the two sides swing in opposite phase).
    pub side: f32,
}

/// The parts the walk cycle moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RigKind {
    /// An arm: swings against the same-side foot.
    Arm,
    /// A foot: swings forward and back, lifts, and plants on the ground below.
    Foot,
}

/// Walk-cycle phase of a character (radians, advances with ground speed).
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct WalkPhase(pub f32);

/// `Startup`: the player character with its rig.
pub fn setup_character(mut commands: Commands) {
    let start = Transform::from_translation(crate::respawn::SPAWN_POINT);
    let player = commands
        .spawn((
            Name::new("Player1"),
            start,
            GlobalTransform(start.compute_affine()),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState::default(),
            CharacterBody::default(),
            WalkPhase::default(),
            Health::new(PLAYER_HEALTH),
            Fighter::default(),
        ))
        .id();
    attach_rig(&mut commands, player);
}

/// Adds the body parts as children of `player`. Replace it with your model.
fn attach_rig(commands: &mut Commands, player: Entity) {
    let skin = [0.9, 0.5, 0.2, 1.0];
    let cloth = [0.25, 0.35, 0.75, 1.0];
    let part =
        |commands: &mut Commands, name: &str, at: Vec3, half: Vec3, color: [f32; 4], rig: Option<RigPart>| {
            let mut entity = commands.spawn((
                Name::new(name),
                Transform::from_translation(at),
                GlobalTransform(Transform::from_translation(at).compute_affine()),
                MeshRenderer::new(std::sync::Arc::new(cube_mesh(half)), solid(color)),
                ChildOf(player),
            ));
            if let Some(rig) = rig {
                entity.insert(rig);
            }
        };
    // The sword: a blade in the right hand; `combat::animate_sword` swings it.
    commands.spawn((
        Name::new("Sword"),
        Transform::from_translation(Vec3::new(0.4, 0.1, -0.35)),
        GlobalTransform(Transform::from_translation(Vec3::new(0.4, 0.1, -0.35)).compute_affine()),
        MeshRenderer::new(
            std::sync::Arc::new(cube_mesh(Vec3::new(0.03, 0.03, 0.45))),
            solid([0.8, 0.82, 0.88, 1.0]),
        ),
        Sword,
        ChildOf(player),
    ));
    part(
        commands,
        "Torso",
        Vec3::new(0.0, 0.15, 0.0),
        Vec3::new(0.25, 0.4, 0.14),
        cloth,
        None,
    );
    part(
        commands,
        "Head",
        Vec3::new(0.0, 0.76, 0.0),
        Vec3::new(0.15, 0.15, 0.15),
        skin,
        None,
    );
    for side in [-1.0_f32, 1.0] {
        part(
            commands,
            "Arm",
            Vec3::new(0.34 * side, 0.2, 0.0),
            Vec3::new(0.07, 0.3, 0.07),
            skin,
            Some(RigPart {
                kind: RigKind::Arm,
                side,
            }),
        );
        part(
            commands,
            "Foot",
            Vec3::new(0.14 * side, FOOT_REST_Y, 0.0),
            Vec3::new(0.09, FOOT_HALF, 0.16),
            cloth,
            Some(RigPart {
                kind: RigKind::Foot,
                side,
            }),
        );
    }
}

/// `Update`: advances each character's walk phase with its ground speed.
pub fn advance_phase(time: Res<Time>, mut characters: Query<(&CharacterState, &mut WalkPhase)>) {
    for (state, mut phase) in &mut characters {
        let speed = state.velocity.with_y(0.0).length();
        if state.grounded && speed > 0.1 {
            phase.0 = (phase.0 + speed * 1.6 * time.delta_secs()) % TAU;
        }
    }
}

/// `Update` (after [`advance_phase`]): swings arms and feet, and plants each foot on the ground
/// found by a ray below it (foot IK). In the air the feet tuck up instead.
#[tracing::instrument(skip_all, name = "animate_rig")]
pub fn animate_rig(
    physics: PhysicsQuery,
    characters: Query<(&Transform, &CharacterState, &WalkPhase, &Children), With<Character>>,
    mut parts: Query<(&RigPart, &mut Transform), Without<Character>>,
) {
    for (character, state, phase, children) in &characters {
        let moving = state.velocity.with_y(0.0).length() > 0.1;
        for &child in children {
            let Ok((part, mut transform)) = parts.get_mut(child) else {
                continue;
            };
            let swing = if moving {
                (phase.0 + part.side * std::f32::consts::FRAC_PI_2).sin()
            } else {
                0.0
            };
            match part.kind {
                RigKind::Arm => {
                    transform.rotation = Quat::from_rotation_x(-swing * 0.6);
                }
                RigKind::Foot => {
                    let stride = swing * 0.28;
                    let lift = if moving { swing.abs() * 0.1 } else { 0.0 };
                    let mut local = Vec3::new(0.14 * part.side, FOOT_REST_Y + lift, -stride);
                    if state.grounded {
                        // Foot IK: the ground height below the foot's world position.
                        let world = character.translation + character.rotation * local;
                        let origin = world + Vec3::Y * 0.6;
                        if let Some(hit) = physics.raycast(origin, Vec3::NEG_Y, 1.4, QueryFilter::new()) {
                            let wanted = hit.point.y + FOOT_HALF + lift - character.translation.y;
                            local.y = wanted.clamp(FOOT_REST_Y - FOOT_REACH, FOOT_REST_Y + FOOT_REACH);
                        }
                    } else {
                        local.y = FOOT_REST_Y + 0.25;
                    }
                    transform.translation = local;
                }
            }
        }
    }
}

/// `Startup` (after the character): the boom camera, a rig chasing the character, with the
/// render camera on the same entity.
pub fn setup_camera(mut commands: Commands, characters: Query<Entity, With<Character>>) {
    let Some(target) = characters.iter().next() else {
        warn!("third_person template: no character to chase");
        return;
    };
    let mode = ThirdPerson {
        boom_length: 4.5,
        socket_offset: Vec3::new(0.0, 1.6, 0.0),
        target_offset: Vec3::new(0.0, 1.2, 0.0),
        ..ThirdPerson::default()
    };
    commands.spawn((
        Name::new("Player1Camera"),
        Transform::default(),
        CameraRig {
            mode: CameraMode::ThirdPerson(mode),
            ..CameraRig::default()
        },
        ChaseCamera { target },
        Camera {
            clear: ClearMode::Color([0.05, 0.08, 0.12, 1.0]),
            ..Camera::default()
        },
    ));
}
