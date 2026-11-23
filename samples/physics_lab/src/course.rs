// SPDX-License-Identifier: MIT-0
//! A small traversal course (`docs/specs/traversal.md`): walk into a pool and swim across, vault a low
//! wall, mantle onto a block, climb a ladder to the top. One character with one scripted input, on fixed
//! steps: [`run_course`] returns a digest that covers every step, so equal runs give equal digests.
//!
//! [`build_course_with`] makes the headless app (the agent test attaches its plugin to it and drives
//! the input itself).

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState, MovementMode,
    use_rapier_character_controller,
};
use regia_framework::state_hash::fnv1a;
use regia_framework::traversal::{
    CharacterTraversal, FlatWater, Ladder, TraversalKind, TraversalPlugin, TraversalRefusal,
    TraversalSettings, TraversalWater,
};
use regia_physics::{CharacterBody, Collider, ColliderShape, PhysicsPlugin, RigidBody};
use regia_scene::Transform;

/// The water's height over the pool.
pub const POOL_SURFACE: f32 = -0.2;
/// The near face of the low wall, a vault.
pub const VAULT_X: f32 = 30.0;
/// The near face of the block, a mantle; its top is [`BLOCK_TOP`].
pub const BLOCK_X: f32 = 36.0;
/// The block's top, meters.
pub const BLOCK_TOP: f32 = 1.5;
/// The far end of the block, where the ladder stands, going up [`LADDER_HEIGHT`].
pub const LADDER_X: f32 = 44.0;
/// The ladder's height, meters.
pub const LADDER_HEIGHT: f32 = 3.0;
/// The ladder's id.
pub const LADDER_ID: u64 = 11;
/// The most fixed steps [`run_course`] takes.
pub const COURSE_STEPS: u32 = 4000;

/// What the course run saw.
#[derive(Clone, Debug, PartialEq)]
pub struct CourseReport {
    /// Every traversal kind the character was in, in the order it first was.
    pub kinds: Vec<TraversalKind>,
    /// The last refusal, if any.
    pub last_refusal: Option<TraversalRefusal>,
    /// The fixed step the character stood on top of the ladder (`None`: it never did).
    pub finished_at: Option<u32>,
    /// Where the character ended.
    pub position: Vec3,
    /// The worst distance from the float line while swimming at the surface over the deep part, meters.
    pub float_error: f32,
    /// Covers position, velocity, mode and move at every step.
    pub digest: u64,
}

fn solid(app: &mut App, half: Vec3, translation: Vec3, rotation: Quat) -> Entity {
    app.world_mut()
        .spawn((
            Transform {
                translation,
                rotation,
                ..Default::default()
            },
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ))
        .id()
}

fn lay_out_pool(app: &mut App) {
    let z = 8.0;
    let slope = (4.0f32 / 8.0).atan();
    let half_len = (16.0f32 + 64.0).sqrt() * 0.5;
    // The basin floor and the ramps down to it and up from it.
    solid(
        app,
        Vec3::new(4.0, 0.5, z),
        Vec3::new(12.0, -4.5, 0.0),
        Quat::IDENTITY,
    );
    let down = Vec3::new(slope.sin(), slope.cos(), 0.0);
    solid(
        app,
        Vec3::new(half_len, 0.5, z),
        Vec3::new(4.0, -2.0, 0.0) - down * 0.5,
        Quat::from_rotation_z(-slope),
    );
    let up = Vec3::new(-slope.sin(), slope.cos(), 0.0);
    solid(
        app,
        Vec3::new(half_len, 0.5, z),
        Vec3::new(20.0, -2.0, 0.0) - up * 0.5,
        Quat::from_rotation_z(slope),
    );
    app.insert_resource(TraversalWater(Box::new(FlatWater {
        height: POOL_SURFACE,
        min: [0.0, -z],
        max: [24.0, z],
    })));
}

/// Builds the course and its character, with `prepare` run on the new app first.
pub fn build_course_with(prepare: impl FnOnce(&mut App)) -> (App, Entity) {
    let mut app = App::new();
    prepare(&mut app);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins(PhysicsPlugin)
        .add_plugins(CharacterPlugin)
        .add_plugins(TraversalPlugin);
    use_rapier_character_controller(&mut app);

    let z = 8.0;
    // Ground before the pool and after it.
    solid(
        &mut app,
        Vec3::new(5.0, 0.5, z),
        Vec3::new(-5.0, -0.5, 0.0),
        Quat::IDENTITY,
    );
    solid(
        &mut app,
        Vec3::new(20.0, 0.5, z),
        Vec3::new(44.0, -0.5, 0.0),
        Quat::IDENTITY,
    );
    lay_out_pool(&mut app);
    // The low wall.
    solid(
        &mut app,
        Vec3::new(0.15, 0.5, z),
        Vec3::new(VAULT_X + 0.15, 0.5, 0.0),
        Quat::IDENTITY,
    );
    // The block, then the upper floor beyond the ladder's wall.
    let block = Vec3::new((BLOCK_X + LADDER_X) * 0.5, BLOCK_TOP * 0.5, 0.0);
    solid(
        &mut app,
        Vec3::new((LADDER_X - BLOCK_X) * 0.5, BLOCK_TOP * 0.5, z),
        block,
        Quat::IDENTITY,
    );
    let top = BLOCK_TOP + LADDER_HEIGHT;
    solid(
        &mut app,
        Vec3::new(10.0, top * 0.5, z),
        Vec3::new(LADDER_X + 10.0, top * 0.5, 0.0),
        Quat::IDENTITY,
    );
    app.world_mut().spawn((
        // Local -Z (into the ladder) must point +X.
        Transform {
            translation: Vec3::new(LADDER_X, BLOCK_TOP, 0.0),
            rotation: Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2),
            ..Default::default()
        },
        Ladder {
            height: LADDER_HEIGHT,
            user_id: LADDER_ID,
            ..Ladder::default()
        },
    ));
    let character = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(-3.0, 1.22, 0.0)),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState {
                grounded: true,
                ..CharacterState::default()
            },
            CharacterBody::default(),
            TraversalSettings::default(),
        ))
        .id();
    (app, character)
}

/// The scripted input at `x`: always forward, and jump in front of the two walls.
fn scripted_input(x: f32) -> CharacterInput {
    let near = |wall: f32| (wall - 0.8..wall).contains(&x);
    CharacterInput {
        move_dir: Vec3::X,
        jump: near(VAULT_X) || near(BLOCK_X),
        ..CharacterInput::default()
    }
}

/// Runs the whole course once.
pub fn run_course() -> CourseReport {
    let (mut app, character) = build_course_with(|_| {});
    let mut report = CourseReport {
        kinds: Vec::new(),
        last_refusal: None,
        finished_at: None,
        position: Vec3::ZERO,
        float_error: 0.0,
        digest: 0,
    };
    let float_y = POOL_SURFACE - TraversalSettings::default().swim.chest_offset;
    let finish_y = BLOCK_TOP + LADDER_HEIGHT;
    let mut bytes = Vec::new();
    for step in 0..COURSE_STEPS {
        let at = app
            .world()
            .get::<Transform>(character)
            .map_or(Vec3::ZERO, |t| t.translation);
        if let Some(mut input) = app.world_mut().get_mut::<CharacterInput>(character) {
            *input = scripted_input(at.x);
        }
        app.update();
        let at = app
            .world()
            .get::<Transform>(character)
            .map_or(Vec3::ZERO, |t| t.translation);
        let state = app
            .world()
            .get::<CharacterState>(character)
            .cloned()
            .unwrap_or_default();
        let traversal = app
            .world()
            .get::<CharacterTraversal>(character)
            .copied()
            .unwrap_or_default();
        if traversal.kind != TraversalKind::None && !report.kinds.contains(&traversal.kind) {
            report.kinds.push(traversal.kind);
        }
        if traversal.last_refusal.is_some() {
            report.last_refusal = traversal.last_refusal;
        }
        if traversal.kind == TraversalKind::Swim && (10.0..14.0).contains(&at.x) {
            report.float_error = report.float_error.max((at.y - float_y).abs());
        }
        for value in [
            at.x,
            at.y,
            at.z,
            state.velocity.x,
            state.velocity.y,
            state.velocity.z,
        ] {
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        bytes.extend_from_slice(traversal.kind.name().as_bytes());
        bytes.extend_from_slice(format!("{:?}", state.mode).as_bytes());
        bytes.push(u8::from(state.grounded));
        report.position = at;
        if state.grounded
            && traversal.kind == TraversalKind::None
            && state.mode == MovementMode::Walking
            && at.x > LADDER_X
            && at.y > finish_y
        {
            report.finished_at = Some(step);
            break;
        }
    }
    report.digest = fnv1a(&bytes);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_jumps_only_in_front_of_the_walls() {
        assert!(!scripted_input(0.0).jump);
        assert!(scripted_input(VAULT_X - 0.5).jump);
        assert!(!scripted_input(VAULT_X + 0.5).jump);
        assert!(scripted_input(BLOCK_X - 0.5).jump);
    }
}
