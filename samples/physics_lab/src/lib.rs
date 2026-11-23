// SPDX-License-Identifier: MIT-0
//! Headless physics lab (spec 15 § 6, ADR-2424): the physics features in one deterministic run.
//!
//! - **Stack**: a tower of boxes on a floor stays standing.
//! - **Joints**: a block held out to the side of a fixed joint with a `JointBreakTorque` breaks at a
//!   fixed step; an identical lever without the limit holds. A four-link spherical chain hangs.
//! - **Materials**: two boxes on a 20 degree ramp, one default and one with the `ice` `.physmat.ron`
//!   material (`PhysicsMaterialRef`): the default holds (friction 0.5 > tan 20 degrees), the ice slides.
//! - **Ragdoll drop**: a limited humanoid profile is dropped limp until it settles
//!   (`regia_framework::ragdoll::preview`).
//! - **Debug draw**: with [`LabConfig::debug_draw`] on, the contact, joint and center-of-mass lines of the
//!   last frame are counted.
//! - **Traversal course** ([`course`]): a character swims a pool, vaults a wall, mantles a block and
//!   climbs a ladder.
//!
//! [`build_app`] makes the headless app (agent tests attach the agent plugin to it); [`run`] steps it a
//! fixed number of 60 Hz steps and returns a [`LabReport`] whose `digest` covers the physics state: equal
//! runs give equal digests.

use std::fmt;
use std::time::Duration;

pub mod course;

use regia_animation::demo_rig::{DemoRig, demo_humanoid};
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_ecs::prelude::Entity;
use regia_framework::ragdoll::RagdollProfile;
use regia_framework::ragdoll::preview::{RagdollPreviewOptions, preview_drop};
use regia_physics::debug_draw::PhysicsDebugDraw;
use regia_physics::{
    Collider, ColliderShape, Joint, JointBreakTorque, JointBroken, JointKind, PhysicsMaterial,
    PhysicsMaterialRef, PhysicsMaterials, PhysicsPlugin, PhysicsPose, PhysicsWorld, RigidBody, Velocity,
};
use regia_scene::Transform;

/// The `.physmat.ron` text of the ice material the ramp box uses.
pub const ICE_RON: &str =
    r#"(format_version: 1, name: "ice", friction: 0.02, restitution: 0.0, friction_combine: Min)"#;

/// Tilt of the ramp, radians (20 degrees).
const RAMP_TILT: f32 = 0.349_065_85;

/// What to run.
#[derive(Clone, Debug, PartialEq)]
pub struct LabConfig {
    /// Boxes in the tower.
    pub stack_height: usize,
    /// Fixed steps of the main run.
    pub steps: u32,
    /// Fixed steps of the ragdoll drop test.
    pub ragdoll_steps: u32,
    /// Torque limit of the lever (N·m); the block's load is about 0.63 N·m.
    pub break_torque: f32,
    /// Turn the physics debug draw on for the run.
    pub debug_draw: bool,
}

impl Default for LabConfig {
    fn default() -> Self {
        Self {
            stack_height: 8,
            steps: 240,
            ragdoll_steps: 600,
            break_torque: 0.2,
            debug_draw: false,
        }
    }
}

/// The entities the lab spawned, for tests and agents.
#[derive(Clone, Debug)]
pub struct LabEntities {
    /// The boxes of the tower, bottom first.
    pub stack: Vec<Entity>,
    /// The anchor carrying the torque-limited lever joint.
    pub lever: Entity,
    /// The anchor of the lever without a limit.
    pub control_lever: Entity,
    /// The chain's last link.
    pub chain_tail: Entity,
    /// The default-material box on the ramp.
    pub plain_box: Entity,
    /// The ice-material box on the ramp.
    pub ice_box: Entity,
}

/// What the lab saw.
#[derive(Clone, Debug, PartialEq)]
pub struct LabReport {
    /// The tower's top box stayed within 5 cm of its start.
    pub stack_standing: bool,
    /// The step at which the torque-limited lever broke (`None`: it held).
    pub lever_broke_at: Option<u32>,
    /// The lever without a limit still holds.
    pub control_lever_held: bool,
    /// How far under its anchor the chain's last link hangs, metres.
    pub chain_drop: f32,
    /// How far the default box slid down the ramp, metres.
    pub plain_slide: f32,
    /// How far the ice box slid down the ramp, metres.
    pub ice_slide: f32,
    /// The ragdoll drop's settle step.
    pub ragdoll_settled_at: Option<u32>,
    /// The most debug-draw lines one frame held (0 with the draw off).
    pub debug_lines: usize,
    /// Debug-draw collections run (0 with the draw off).
    pub debug_collections: u64,
    /// The physics state digest at the end.
    pub digest: u64,
}

impl fmt::Display for LabReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "stack standing: {}", self.stack_standing)?;
        writeln!(
            f,
            "lever broke at step {:?}; control held: {}",
            self.lever_broke_at, self.control_lever_held
        )?;
        writeln!(f, "chain hangs {:.2} m", self.chain_drop)?;
        writeln!(
            f,
            "ramp slide: default {:.2} m, ice {:.2} m",
            self.plain_slide, self.ice_slide
        )?;
        writeln!(f, "ragdoll drop: settled at {:?}", self.ragdoll_settled_at)?;
        writeln!(
            f,
            "debug draw: {} lines, {} collections",
            self.debug_lines, self.debug_collections
        )?;
        write!(f, "digest {:016x}", self.digest)
    }
}

/// Why the lab could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabError(pub String);

impl fmt::Display for LabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LabError {}

fn solid(app: &mut App, body: RigidBody, shape: ColliderShape, transform: Transform) -> Entity {
    app.world_mut()
        .spawn((
            transform,
            body,
            Collider {
                shape,
                friction: 0.5,
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id()
}

fn cube(half: f32) -> ColliderShape {
    ColliderShape::Box {
        half_extents: Vec3::splat(half),
    }
}

/// Builds the headless lab app (physics only, one fixed 60 Hz step per update) and the entities in it.
pub fn build_app(config: &LabConfig) -> (App, LabEntities) {
    build_app_with(config, |_| {})
}

/// [`build_app`] with `prepare` run on the new app first (an agent test adds its plugins there).
pub fn build_app_with(config: &LabConfig, prepare: impl FnOnce(&mut App)) -> (App, LabEntities) {
    let mut app = App::new();
    prepare(&mut app);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    let mut materials = PhysicsMaterials::default();
    materials.insert(PhysicsMaterial::default());
    if let Err(e) = materials.insert_ron(ICE_RON) {
        tracing::warn!(target: "physics_lab", "ice material: {e}");
    }
    app.insert_resource(materials);

    // The floor: top face at y = 0.
    solid(
        &mut app,
        RigidBody::static_body(),
        ColliderShape::Box {
            half_extents: Vec3::new(30.0, 0.5, 30.0),
        },
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
    );
    // The tower, at x = -6.
    let stack: Vec<Entity> = (0..config.stack_height)
        .map(|i| {
            solid(
                &mut app,
                RigidBody::dynamic(),
                cube(0.25),
                Transform::from_translation(Vec3::new(-6.0, 0.25 + 0.5 * i as f32, 0.0)),
            )
        })
        .collect();
    // Two levers, side by side at y = 5: a block 1 m out from the joint point.
    let lever = lever(&mut app, -2.0, config.break_torque);
    let control_lever = lever_without_limit(&mut app, 0.0);
    // A hanging chain of four spherical links at x = 3.
    let anchor = solid(
        &mut app,
        RigidBody::static_body(),
        cube(0.1),
        Transform::from_translation(Vec3::new(3.0, 6.0, 0.0)),
    );
    let mut parent = anchor;
    let mut chain_tail = anchor;
    for i in 0..4 {
        let link = solid(
            &mut app,
            RigidBody::dynamic(),
            cube(0.1),
            Transform::from_translation(Vec3::new(3.0, 5.5 - 0.5 * i as f32, 0.0)),
        );
        app.world_mut().entity_mut(parent).insert(Joint {
            kind: JointKind::Spherical { cone_limit: None },
            connected: link,
            anchor: Vec3::new(0.0, -0.25, 0.0),
            connected_anchor: Vec3::new(0.0, 0.25, 0.0),
            break_force: 0.0,
        });
        parent = link;
        chain_tail = link;
    }
    // The ramp at x = 8, tilted about Z, a default box and an ice box on it.
    let ramp = Quat::from_rotation_z(RAMP_TILT);
    solid(
        &mut app,
        RigidBody::static_body(),
        ColliderShape::Box {
            half_extents: Vec3::new(3.0, 0.1, 4.0),
        },
        Transform {
            translation: Vec3::new(8.0, 1.0, 0.0),
            rotation: ramp,
            ..Transform::IDENTITY
        },
    );
    let on_ramp = |z: f32| Transform {
        translation: Vec3::new(8.0, 1.0, z) + ramp * Vec3::new(0.0, 0.1 + 0.25, 0.0),
        rotation: ramp,
        ..Transform::IDENTITY
    };
    let plain_box = solid(&mut app, RigidBody::dynamic(), cube(0.25), on_ramp(-2.0));
    let ice_box = solid(&mut app, RigidBody::dynamic(), cube(0.25), on_ramp(2.0));
    app.world_mut()
        .entity_mut(ice_box)
        .insert(PhysicsMaterialRef("ice".to_owned()));
    if config.debug_draw {
        app.world_mut().resource_mut::<PhysicsDebugDraw>().set_on(true);
    }
    (
        app,
        LabEntities {
            stack,
            lever,
            control_lever,
            chain_tail,
            plain_box,
            ice_box,
        },
    )
}

/// A static anchor at `(x, 5, 0)` and a block 1 m out along +X held by a fixed joint whose joint point
/// is at the anchor: the block's weight is a steady torque on the joint.
fn lever_base(app: &mut App, x: f32) -> (Entity, Entity) {
    let anchor = solid(
        app,
        RigidBody::static_body(),
        cube(0.1),
        Transform::from_translation(Vec3::new(x, 5.0, 0.0)),
    );
    let block = solid(
        app,
        RigidBody::dynamic(),
        cube(0.2),
        Transform::from_translation(Vec3::new(x + 1.0, 5.0, 0.0)),
    );
    app.world_mut().entity_mut(anchor).insert(Joint {
        kind: JointKind::Fixed,
        connected: block,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::new(-1.0, 0.0, 0.0),
        break_force: 0.0,
    });
    (anchor, block)
}

fn lever(app: &mut App, x: f32, torque: f32) -> Entity {
    let (anchor, _) = lever_base(app, x);
    app.world_mut()
        .entity_mut(anchor)
        .insert(JointBreakTorque(torque));
    anchor
}

fn lever_without_limit(app: &mut App, x: f32) -> Entity {
    lever_base(app, x).0
}

fn position(app: &App, entity: Entity) -> Vec3 {
    if let Some(p) = app.world().get::<PhysicsPose>(entity) {
        return p.translation;
    }
    app.world()
        .get::<Transform>(entity)
        .map_or(Vec3::ZERO, |t| t.translation)
}

/// Steps the lab `config.steps` times and reports.
///
/// # Errors
/// [`LabError`] when the ragdoll drop test cannot run.
pub fn run(config: &LabConfig) -> Result<LabReport, LabError> {
    let (mut app, ids) = build_app(config);
    app.update();
    let top_start = position(&app, *ids.stack.last().unwrap_or(&ids.lever));
    let plain_start = position(&app, ids.plain_box);
    let ice_start = position(&app, ids.ice_box);
    let mut broken = MessageCursor::<JointBroken>::default();
    let mut lever_broke_at = None;
    let mut control_broke = false;
    let mut debug_lines = 0;
    for step in 1..=config.steps {
        app.update();
        let messages = app.world().resource::<Messages<JointBroken>>();
        for m in broken.read(messages) {
            if m.entity == ids.lever {
                lever_broke_at.get_or_insert(step);
            } else if m.entity == ids.control_lever {
                control_broke = true;
            }
        }
        debug_lines = debug_lines.max(app.world().resource::<PhysicsDebugDraw>().lines().len());
    }
    let top_end = position(&app, *ids.stack.last().unwrap_or(&ids.lever));
    let slide = |now: Vec3, start: Vec3| (now - start).length();
    let skeleton = demo_humanoid(&DemoRig::default()).ok_or_else(|| LabError("demo rig".to_owned()))?;
    let profile = RagdollProfile::humanoid_limited(&skeleton, 75.0, Vec3::NEG_Z);
    let drop = preview_drop(
        std::sync::Arc::new(skeleton),
        &profile,
        &RagdollPreviewOptions {
            steps: config.ragdoll_steps,
            ..Default::default()
        },
    )
    .map_err(|e| LabError(format!("ragdoll drop: {e}")))?;
    Ok(LabReport {
        stack_standing: (top_end - top_start).length() < 0.05,
        lever_broke_at,
        control_lever_held: !control_broke,
        chain_drop: 6.0 - position(&app, ids.chain_tail).y,
        plain_slide: slide(position(&app, ids.plain_box), plain_start),
        ice_slide: slide(position(&app, ids.ice_box), ice_start),
        ragdoll_settled_at: drop.settled_at,
        debug_lines,
        debug_collections: app.world().resource::<PhysicsDebugDraw>().stats().collections,
        digest: app.world().resource::<PhysicsWorld>().state_digest() ^ drop.digest.rotate_left(17),
    })
}
