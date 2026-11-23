// SPDX-License-Identifier: MPL-2.0
//! Replay soak (`book/agents/replay-soak.md`, ADR-867): record a seeded input stream on a headless
//! scene, replay the recording in a fresh app, and compare the world's state hash step by step.
//!
//! A run is one `(scene, seed)` pair:
//!
//! 1. **Record.** Build the scene for the seed, start an [`InputRecorder`], and drive the scene's
//!    actions from a seeded generator ([`InputStream`]) through [`InputInjection`] for `steps`
//!    fixed steps. Every `every` steps (and on the last) take a [`snapshot`] and keep its hash and
//!    one hash per entity ([`Checkpoint`]).
//! 2. **Replay.** Build the scene again for the recording's seed, play the recording (no
//!    generator), take the same checkpoints and compare each with the recording's. The first
//!    checkpoint that differs is the run's [`Divergence`]: the step and the differing
//!    `<entity>/<Component>` values on both sides.
//!
//! One process runs every `(scene, seed)` of a matrix at one compute-thread count
//! ([`init_threads`]) and one build profile, and writes a [`RunReport`] as JSON
//! ([`RunReport::to_json`], format `regia-replay-soak-run` version 1). `cargo xtask replay-soak`
//! runs the matrix once per thread count and build, then compares the record traces across those
//! runs: a scene whose state depends on the thread count, on the optimizer or on a per-process
//! hash seed diverges there even when each run replays its own recording cleanly.
//!
//! The scenes are CPU-only (no GPU, so the soak runs on lavapipe-only machines):
//!
//! - `smoke`: [`crate::shuffle::smoke_app`] (rigid bodies, a character, spring bones, a clip and a
//!   socket) with the hero on `Move`/`Jump` and the first box on `Shove`.
//! - `pile`: a bin of seeded boxes, spheres and capsules with collision events on, and a striker
//!   sphere on `Shove`; the tally entity folds every `CollisionStarted` pair, in arrival order,
//!   into a hash.
//! - `combat`: a turret on `Aim`/`Fire` that damages seeded targets through the framework's damage
//!   step; deaths are tallied and dead targets respawn after a countdown.
//! - `crowd`: twelve seeded nav agents on a floor with a hole, steered with ORCA avoidance;
//!   `Rally` plans paths to a point around the stick's direction, `Scatter` sends everyone home.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use regia_app::prelude::*;
use regia_core::math::{Quat, Rng, Vec3};
use regia_framework::character::CharacterInput;
use regia_framework::damage::{DamageEvent, DamagePlugin, DamageSet, Died, DiedEvent, Health};
use regia_framework::input_recording::{
    InputRecorder, InputRecording, InputRecordingPlugin, InputRecordingSet,
};
use regia_input::{ActionKind, Actions, InputConfig, InputInjection, InputPlugin};
use regia_nav::{
    DEFAULT_PATH_EXTENTS, NavAgent, NavAgentState, NavMesh, NavMeshBuilder, NavPlugin, NavQuery, NavStep,
};
use regia_physics::{
    ActiveEvents, Collider, ColliderShape, CollisionStarted, PhysicsStep, RigidBody, ShapeAxis, Velocity,
};
use regia_scene::Transform;
use regia_scene::components::Name;

use crate::shuffle::{Snapshot, differing_keys, fnv1a, snapshot, snapshot_hash, snapshot_where};

/// The report format name written by [`RunReport::to_json`].
pub const RUN_FORMAT: &str = "regia-replay-soak-run";
/// The report format version written by [`RunReport::to_json`].
pub const RUN_FORMAT_VERSION: u32 = 1;

/// How many differing values a [`Divergence`] keeps.
pub const MAX_DIFFS: usize = 12;
/// Longest value text a [`ValueDiff`] keeps (characters); longer text is cut and marked.
pub const MAX_VALUE_CHARS: usize = 400;

/// Fixed step of the `pile` and `combat` scenes, in seconds (they run at 60 Hz, one step per frame).
const STEP_SECS: f32 = 1.0 / 60.0;

// ---------------------------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------------------------

/// A headless scene the soak can record and replay.
#[derive(Debug, Clone, Copy)]
pub struct SoakScene {
    /// The scene's name (`smoke`, `pile`, `combat`).
    pub name: &'static str,
    /// Builds the scene for a seed: plugins, entities, the input layer with
    /// [`SoakScene::actions`] and the [`InputRecordingPlugin`]. Every update runs exactly one
    /// fixed step.
    pub build: fn(u64) -> App,
    /// The actions the generator drives, with their kinds, sorted by name.
    pub actions: &'static [(&'static str, ActionKind)],
    /// The component short names the state hash covers; empty hashes every reflected component.
    /// A full game lists its gameplay components here, leaving out asset handles and render state
    /// whose ids come from process-wide counters.
    pub components: &'static [&'static str],
}

impl SoakScene {
    /// The scene's observable state ([`snapshot`], or [`snapshot_where`] over
    /// [`SoakScene::components`]).
    pub fn snapshot(&self, world: &World) -> Snapshot {
        if self.components.is_empty() {
            snapshot(world)
        } else {
            snapshot_where(world, |name| self.components.contains(&name))
        }
    }
}

/// The built-in scenes, in report order.
pub const SCENES: [SoakScene; 4] = [
    SoakScene {
        name: "smoke",
        build: smoke_scene,
        actions: &[
            ("Jump", ActionKind::Button),
            ("Move", ActionKind::Axis2D),
            ("Shove", ActionKind::Axis2D),
        ],
        components: &[],
    },
    SoakScene {
        name: "pile",
        build: pile_scene,
        actions: &[("Shove", ActionKind::Axis2D)],
        components: &[],
    },
    SoakScene {
        name: "combat",
        build: combat_scene,
        actions: &[("Aim", ActionKind::Axis1D), ("Fire", ActionKind::Button)],
        components: &[],
    },
    SoakScene {
        name: "crowd",
        build: crowd_scene,
        actions: &[("Rally", ActionKind::Axis2D), ("Scatter", ActionKind::Button)],
        components: &[],
    },
];

/// The built-in scene called `name`.
pub fn scene(name: &str) -> Option<SoakScene> {
    SCENES.iter().copied().find(|s| s.name == name)
}

/// Scene bookkeeping on the entity named `tally`: what the scene's own systems counted. It is part
/// of the snapshot, so an event that arrives in a different order changes the state hash.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct SoakTally {
    /// Fixed steps run.
    pub steps: u32,
    /// `CollisionStarted` messages read (`pile`).
    pub collisions: u32,
    /// FNV-1a over the names of every collision pair, in arrival order (`pile`).
    pub collision_order: u64,
    /// Shots fired (`combat`).
    pub shots: u32,
    /// Deaths read from `DiedEvent` (`combat`).
    pub kills: u32,
    /// FNV-1a over the names of every killed target, in arrival order (`combat`).
    pub kill_order: u64,
}

/// Marker: the character that `Move` and `Jump` drive.
#[derive(Component, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Component, Default)]
pub struct SoakHero;

/// Marker: the body whose velocity `Shove` pushes.
#[derive(Component, Reflect, Debug, Clone, Copy, Default)]
#[reflect(Component, Default)]
pub struct SoakStriker;

/// The `combat` turret: `Aim` turns it, `Fire` shoots the target nearest its aim.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct SoakTurret {
    /// Heading around +Y, radians.
    pub yaw: f32,
    /// Steps until the turret can fire again.
    pub cooldown: u32,
}

/// A `combat` target: it respawns `respawn_steps` after dying, at its next ring slot.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct SoakTarget {
    /// Ring slot (the target moves one slot on each respawn).
    pub slot: u32,
    /// Steps a dead target waits before it respawns.
    pub respawn_steps: u32,
    /// Steps left until respawn (0 while alive).
    pub respawn_in: u32,
}

fn input_config(actions: &[(&'static str, ActionKind)]) -> InputConfig {
    actions.iter().fold(InputConfig::new(), |config, (name, kind)| {
        config.action(*name, *kind)
    })
}

/// Adds the input layer, the recorder and the soak's reflected types.
fn add_soak_input(app: &mut App, actions: &[(&'static str, ActionKind)]) {
    app.add_plugins((InputPlugin::new(input_config(actions)), InputRecordingPlugin))
        .register_type::<SoakTally>()
        .register_type::<SoakHero>()
        .register_type::<SoakStriker>()
        .register_type::<SoakTurret>()
        .register_type::<SoakTarget>()
        .register_type::<SoakRally>()
        .register_type::<SoakWalker>();
    app.world_mut().spawn((Name::new("tally"), SoakTally::default()));
}

fn find_named(app: &mut App, name: &str) -> Option<Entity> {
    let mut query = app.world_mut().query::<(Entity, &Name)>();
    query
        .iter(app.world())
        .find(|(_, n)| n.as_str() == name)
        .map(|(entity, _)| entity)
}

/// `FixedUpdate`, after the recorder and before physics: actions to the hero and the striker.
fn drive_bodies(
    actions: Res<Actions>,
    mut heroes: Query<&mut CharacterInput, With<SoakHero>>,
    mut strikers: Query<&mut Velocity, With<SoakStriker>>,
    mut tally: Query<&mut SoakTally>,
) {
    let movement = actions.value("Move");
    for mut input in &mut heroes {
        input.move_dir = Vec3::new(movement.x, 0.0, -movement.y).clamp_length_max(1.0);
        input.jump = actions.just_pressed("Jump");
    }
    let shove = actions.value("Shove");
    if shove != Vec3::ZERO {
        for mut velocity in &mut strikers {
            velocity.linear += Vec3::new(shove.x, 0.0, -shove.y) * 0.75;
        }
    }
    for mut tally in &mut tally {
        tally.steps += 1;
    }
}

/// `FixedUpdate`, after physics: folds every collision pair into the tally, in arrival order.
fn tally_collisions(
    mut collisions: MessageReader<CollisionStarted>,
    names: Query<&Name>,
    mut tally: Query<&mut SoakTally>,
) {
    let mut order = Vec::new();
    for event in collisions.read() {
        let name = |entity: Entity| names.get(entity).map_or("?", |n| n.as_str()).to_string();
        let (a, b) = (name(event.entity_a), name(event.entity_b));
        // The pair, not who is `a`: only the order of the events matters here.
        let pair = if a <= b {
            format!("{a}|{b};")
        } else {
            format!("{b}|{a};")
        };
        order.push(pair);
    }
    if order.is_empty() {
        return;
    }
    for mut tally in &mut tally {
        tally.collisions += u32::try_from(order.len()).unwrap_or(u32::MAX);
        let mut bytes = tally.collision_order.to_le_bytes().to_vec();
        bytes.extend(order.concat().bytes());
        tally.collision_order = fnv1a(&bytes);
    }
}

/// The `smoke` scene: [`crate::shuffle::smoke_app`] on the input layer. `Move`/`Jump` drive the
/// hero, `Shove` pushes `box0`. The seed only steers the input stream.
pub fn smoke_scene(_seed: u64) -> App {
    let mut app = crate::shuffle::smoke_app();
    add_soak_input(&mut app, SCENES[0].actions);
    if let Some(hero) = find_named(&mut app, "hero") {
        app.world_mut().entity_mut(hero).insert(SoakHero);
    }
    if let Some(striker) = find_named(&mut app, "box0") {
        app.world_mut().entity_mut(striker).insert(SoakStriker);
    }
    app.add_systems(
        FixedUpdate,
        drive_bodies.after(InputRecordingSet).before(PhysicsStep),
    );
    app
}

/// The `pile` scene: a walled bin, 36 seeded boxes, spheres and capsules dropped into it with
/// collision events on, and a striker sphere on `Shove`.
pub fn pile_scene(seed: u64) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        regia_scene::ScenePlugin,
        regia_physics::PhysicsPlugin,
    ));
    enable_fixed_step(&mut app, 60.0);
    add_soak_input(&mut app, SCENES[1].actions);
    let mut rng = Rng::new(seed);
    let wall = |half: Vec3| Collider {
        shape: ColliderShape::Box { half_extents: half },
        ..Collider::default()
    };
    let statics = [
        ("floor", Vec3::new(0.0, -0.5, 0.0), Vec3::new(4.0, 0.5, 4.0)),
        ("wall_n", Vec3::new(0.0, 1.0, -3.0), Vec3::new(3.0, 1.5, 0.2)),
        ("wall_s", Vec3::new(0.0, 1.0, 3.0), Vec3::new(3.0, 1.5, 0.2)),
        ("wall_e", Vec3::new(3.0, 1.0, 0.0), Vec3::new(0.2, 1.5, 3.0)),
        ("wall_w", Vec3::new(-3.0, 1.0, 0.0), Vec3::new(0.2, 1.5, 3.0)),
    ];
    for (name, at, half) in statics {
        app.world_mut().spawn((
            Name::new(name),
            Transform::from_translation(at),
            RigidBody::static_body(),
            wall(half),
        ));
    }
    let events = ActiveEvents {
        collision: true,
        contact_force: false,
    };
    for i in 0..36u32 {
        let size = 0.18 + rng.f32() * 0.17;
        let shape = match i % 3 {
            0 => ColliderShape::Box {
                half_extents: Vec3::new(size, size * (0.6 + rng.f32() * 0.8), size),
            },
            1 => ColliderShape::Sphere { radius: size },
            _ => ColliderShape::Capsule {
                half_height: size,
                radius: size * 0.6,
                axis: ShapeAxis::Y,
            },
        };
        let at = Vec3::new(
            rng.f32() * 4.0 - 2.0,
            0.8 + i as f32 * 0.32,
            rng.f32() * 4.0 - 2.0,
        );
        let spin = Quat::from_rotation_y(rng.f32() * std::f32::consts::TAU);
        app.world_mut().spawn((
            Name::new(format!("body{i:02}")),
            Transform {
                translation: at,
                rotation: spin,
                ..Transform::IDENTITY
            },
            RigidBody::dynamic(),
            Collider {
                shape,
                friction: 0.3 + rng.f32() * 0.6,
                restitution: rng.f32() * 0.4,
                active_events: events,
                ..Collider::default()
            },
            Velocity::default(),
        ));
    }
    app.world_mut().spawn((
        Name::new("striker"),
        Transform::from_translation(Vec3::new(0.0, 0.45, 2.3)),
        RigidBody {
            can_sleep: false,
            ..RigidBody::dynamic()
        },
        Collider {
            shape: ColliderShape::Sphere { radius: 0.4 },
            density: 4.0,
            active_events: events,
            ..Collider::default()
        },
        Velocity::default(),
        SoakStriker,
    ));
    app.add_systems(
        FixedUpdate,
        (
            drive_bodies.after(InputRecordingSet).before(PhysicsStep),
            tally_collisions.after(PhysicsStep),
        ),
    );
    app
}

/// The `combat` targets as the turret sees them.
type TargetView<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static Name, &'static Transform, &'static Health),
    (With<SoakTarget>, Without<SoakTurret>),
>;

/// Dead `combat` targets waiting to respawn.
type DeadTargets<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut SoakTarget,
        &'static mut Health,
        &'static mut Transform,
    ),
    With<Died>,
>;

/// `FixedUpdate`, before the damage step: `Aim` turns the turret, `Fire` damages the living
/// target nearest its aim (ties broken by name).
fn turret_step(
    actions: Res<Actions>,
    mut turrets: Query<(Entity, &mut Transform, &mut SoakTurret)>,
    targets: TargetView<'_, '_>,
    mut damage: MessageWriter<DamageEvent>,
    mut tally: Query<&mut SoakTally>,
) {
    for mut tally in &mut tally {
        tally.steps += 1;
    }
    for (turret, mut transform, mut state) in &mut turrets {
        state.yaw += actions.axis1d("Aim") * 2.5 * STEP_SECS;
        transform.rotation = Quat::from_rotation_y(state.yaw);
        state.cooldown = state.cooldown.saturating_sub(1);
        if !actions.just_pressed("Fire") || state.cooldown > 0 {
            continue;
        }
        state.cooldown = 6;
        let forward = transform.rotation * Vec3::NEG_Z;
        let best = targets
            .iter()
            .filter(|(_, _, _, health)| !health.is_dead())
            .map(|(entity, name, at, _)| {
                let to = (at.translation - transform.translation).normalize_or_zero();
                (forward.dot(to), name.as_str(), entity, at.translation)
            })
            .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.cmp(a.1)));
        for mut tally in &mut tally {
            tally.shots += 1;
        }
        if let Some((facing, _, target, at)) = best {
            damage.write(DamageEvent {
                target,
                amount: 20.0 + 25.0 * facing.max(0.0),
                source: Some(turret),
                position: Some(at),
            });
        }
    }
}

/// `FixedUpdate`, after the damage step: tallies deaths and respawns dead targets.
fn respawn_step(
    mut commands: Commands,
    mut deaths: MessageReader<DiedEvent>,
    names: Query<&Name>,
    mut dead: DeadTargets<'_, '_>,
    mut tally: Query<&mut SoakTally>,
) {
    for death in deaths.read() {
        let name = names.get(death.entity).map_or("?", |n| n.as_str());
        for mut tally in &mut tally {
            tally.kills += 1;
            let mut bytes = tally.kill_order.to_le_bytes().to_vec();
            bytes.extend(name.bytes());
            tally.kill_order = fnv1a(&bytes);
        }
    }
    for (entity, mut target, mut health, mut transform) in &mut dead {
        if target.respawn_in == 0 {
            target.respawn_in = target.respawn_steps;
            continue;
        }
        target.respawn_in -= 1;
        if target.respawn_in == 0 {
            target.slot += 1;
            transform.translation = ring_position(target.slot);
            health.current = health.max;
            commands.entity(entity).remove::<Died>();
        }
    }
}

/// Ring slot `slot` of the `combat` scene (golden-angle spacing, radius 8).
fn ring_position(slot: u32) -> Vec3 {
    let angle = slot as f32 * 2.399_963_2;
    Vec3::new(angle.cos() * 8.0, 0.0, angle.sin() * 8.0)
}

/// The `combat` scene: a turret on `Aim`/`Fire`, six seeded targets with framework [`Health`],
/// the framework damage step, death tally and respawns.
pub fn combat_scene(seed: u64) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, DamagePlugin));
    enable_fixed_step(&mut app, 60.0);
    add_soak_input(&mut app, SCENES[2].actions);
    let mut rng = Rng::new(seed);
    app.world_mut()
        .spawn((Name::new("turret"), Transform::default(), SoakTurret::default()));
    for i in 0..6u32 {
        let slot = i * 7 + (rng.next_u32() % 5);
        app.world_mut().spawn((
            Name::new(format!("target{i}")),
            Transform::from_translation(ring_position(slot)),
            Health::new(60.0 + (rng.next_u32() % 81) as f32),
            SoakTarget {
                slot,
                respawn_steps: 20 + rng.next_u32() % 60,
                respawn_in: 0,
            },
        ));
    }
    app.add_systems(
        FixedUpdate,
        (
            turret_step.after(InputRecordingSet).before(DamageSet),
            respawn_step.after(DamageSet),
        ),
    );
    app
}

/// The `crowd` scene's rally state, on the entity named `rally`.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct SoakRally {
    /// Where the agents gather when `active`.
    pub point: Vec3,
    /// Whether the agents are gathering (false: they walk home).
    pub active: bool,
    /// Paths planned so far (one per agent per change of plan).
    pub plans: u32,
}

/// A `crowd` agent's home and its place around the rally point.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct SoakWalker {
    /// Where the agent walks on `Scatter`.
    pub home: Vec3,
    /// Its offset from the rally point.
    pub offset: Vec3,
}

/// `FixedUpdate`, after the recorder and before the nav step: `Rally` (a direction) plans every
/// agent a path to its place around a point 6 m out that way; `Scatter` plans every agent home.
/// Paths are only replanned when the plan changes.
fn crowd_plan(
    actions: Res<Actions>,
    mesh: Option<Res<NavMesh>>,
    mut rally: Query<&mut SoakRally>,
    mut walkers: Query<(&SoakWalker, &Transform, &mut NavAgentState)>,
    mut tally: Query<&mut SoakTally>,
) {
    for mut tally in &mut tally {
        tally.steps += 1;
    }
    let (Some(mesh), Ok(mut rally)) = (mesh, rally.single_mut()) else {
        return;
    };
    let aim = actions.value("Rally");
    let (point, active) = if actions.just_pressed("Scatter") {
        (rally.point, false)
    } else if aim != Vec3::ZERO {
        (Vec3::new(aim.x * 6.0, 0.0, -aim.y * 6.0), true)
    } else {
        return;
    };
    if point == rally.point && active == rally.active {
        return;
    }
    rally.point = point;
    rally.active = active;
    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    for (walker, transform, mut state) in &mut walkers {
        let goal = if active {
            point + walker.offset
        } else {
            walker.home
        };
        match query.find_path(transform.translation, goal, DEFAULT_PATH_EXTENTS) {
            Some(path) => state.set_path(path),
            None => state.clear_path(),
        }
        rally.plans += 1;
    }
}

/// Two floor triangles over the rectangle `x0..x1` by `z0..z1` (y = 0).
fn floor_rect(x0: f32, z0: f32, x1: f32, z1: f32) -> [[Vec3; 3]; 2] {
    [
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
        ],
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
            Vec3::new(x0, 0.0, z1),
        ],
    ]
}

/// The `crowd` scene: a 16 m floor with a 4 m hole in the middle, a navmesh built from it, and
/// twelve seeded nav agents on a ring around the hole. `Rally` and `Scatter` drive their plans.
pub fn crowd_scene(seed: u64) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, NavPlugin));
    enable_fixed_step(&mut app, 60.0);
    add_soak_input(&mut app, SCENES[3].actions);
    let floor: Vec<[Vec3; 3]> = [
        floor_rect(-8.0, 2.0, 8.0, 8.0),
        floor_rect(-8.0, -8.0, 8.0, -2.0),
        floor_rect(-8.0, -2.0, -2.0, 2.0),
        floor_rect(2.0, -2.0, 8.0, 2.0),
    ]
    .into_iter()
    .flatten()
    .collect();
    match NavMeshBuilder::default().build(&floor) {
        Ok(mesh) => {
            app.insert_resource(mesh);
        }
        // `crowd_plan` and the nav step skip without a mesh (the soak test checks it is there).
        Err(err) => eprintln!("replay-soak: the crowd navmesh did not build: {err}"),
    }
    let mut rng = Rng::new(seed);
    app.world_mut().spawn((Name::new("rally"), SoakRally::default()));
    for i in 0..12u32 {
        let angle = i as f32 * std::f32::consts::TAU / 12.0 + (rng.f32() - 0.5) * 0.3;
        let home = Vec3::new(angle.cos() * 6.0, 0.0, angle.sin() * 6.0);
        let around = i as f32 * std::f32::consts::TAU / 12.0;
        app.world_mut().spawn((
            Name::new(format!("agent{i:02}")),
            Transform::from_translation(home),
            NavAgent {
                max_speed: 2.5 + rng.f32() * 2.0,
                ..NavAgent::default()
            },
            NavAgentState::default(),
            SoakWalker {
                home,
                offset: Vec3::new(around.cos() * 1.2, 0.0, around.sin() * 1.2),
            },
        ));
    }
    app.add_systems(FixedUpdate, crowd_plan.after(InputRecordingSet).before(NavStep));
    app
}

// ---------------------------------------------------------------------------------------------
// Input generator
// ---------------------------------------------------------------------------------------------

/// One action's generated value and how many more steps it is held.
#[derive(Debug, Clone)]
struct HeldValue {
    kind: ActionKind,
    value: Vec3,
    hold: u32,
}

/// A seeded input stream for a scene's actions: buttons alternate presses of 1 to 8 steps with
/// releases of 3 to 40; axes hold a random value (or zero, 30 % of the time) for 2 to 30 steps.
/// The same seed always gives the same stream.
#[derive(Debug, Clone)]
pub struct InputStream {
    rng: Rng,
    actions: Vec<(String, HeldValue)>,
}

impl InputStream {
    /// A stream for `actions` under `seed`.
    pub fn new(seed: u64, actions: &[(&'static str, ActionKind)]) -> Self {
        Self {
            // A stream of its own so the input does not follow the scene layout's draws.
            rng: Rng::with_stream(seed, 0x50ac_1e5e_ed00_0001),
            actions: actions
                .iter()
                .map(|(name, kind)| {
                    (
                        (*name).to_string(),
                        HeldValue {
                            kind: *kind,
                            value: Vec3::ZERO,
                            hold: 0,
                        },
                    )
                })
                .collect(),
        }
    }

    /// The next step's value for every action, in the order given to [`InputStream::new`].
    pub fn next_step(&mut self) -> Vec<(String, Vec3)> {
        let rng = &mut self.rng;
        self.actions
            .iter_mut()
            .map(|(name, held)| {
                if held.hold == 0 {
                    let pressed = held.value != Vec3::ZERO;
                    let (value, hold) = match held.kind {
                        ActionKind::Button if pressed => (Vec3::ZERO, 3 + rng.next_u32() % 38),
                        ActionKind::Button => (Vec3::X, 1 + rng.next_u32() % 8),
                        kind => {
                            let value = if rng.f32() < 0.3 {
                                Vec3::ZERO
                            } else {
                                let x = rng.f32() * 2.0 - 1.0;
                                let y = rng.f32() * 2.0 - 1.0;
                                let z = rng.f32() * 2.0 - 1.0;
                                match kind {
                                    ActionKind::Axis1D => Vec3::new(x, 0.0, 0.0),
                                    ActionKind::Axis2D => Vec3::new(x, y, 0.0),
                                    _ => Vec3::new(x, y, z),
                                }
                            };
                            (value, 2 + rng.next_u32() % 29)
                        }
                    };
                    held.value = value;
                    held.hold = hold;
                }
                held.hold -= 1;
                (name.clone(), held.value)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Record, replay, compare
// ---------------------------------------------------------------------------------------------

/// Run size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakConfig {
    /// Fixed steps per run.
    pub steps: u32,
    /// Steps between checkpoints (the last step is always a checkpoint). 1 hashes every step.
    pub every: u32,
}

impl Default for SoakConfig {
    fn default() -> Self {
        Self {
            steps: 600,
            every: 10,
        }
    }
}

impl SoakConfig {
    fn is_checkpoint(&self, step: u32) -> bool {
        step == self.steps || step.is_multiple_of(self.every.max(1))
    }
}

/// The state at one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Fixed steps run when the snapshot was taken.
    pub step: u32,
    /// [`snapshot_hash`] of the whole world.
    pub hash: u64,
    /// One hash per entity key (empty for replay checkpoints, which are compared in process).
    pub entities: BTreeMap<String, u64>,
}

/// One `<entity>/<Component>` value that differs between the recording and the replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueDiff {
    /// `<entity>/<Component>`.
    pub key: String,
    /// The recording's value (`<absent>` when the key is missing), cut at [`MAX_VALUE_CHARS`].
    pub record: String,
    /// The replay's value (`<absent>` when the key is missing), cut at [`MAX_VALUE_CHARS`].
    pub replay: String,
}

/// Where a replay first left its recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// The first checkpoint step whose hash differs.
    pub step: u32,
    /// Up to [`MAX_DIFFS`] differing values at that step, sorted by key.
    pub differing: Vec<ValueDiff>,
}

/// What [`record`] produced: the recording, its checkpoints and their snapshots.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The scene.
    pub scene: &'static str,
    /// The seed.
    pub seed: u64,
    /// The input recording (seed and one value set per step).
    pub recording: InputRecording,
    /// Checkpoints, in step order.
    pub checkpoints: Vec<Checkpoint>,
    /// The snapshot of each checkpoint (parallel to `checkpoints`).
    pub snapshots: Vec<Snapshot>,
    /// Step-action pairs whose generated value was non-zero.
    pub active_inputs: u32,
}

/// What [`replay`] produced.
#[derive(Debug, Clone)]
pub struct Replayed {
    /// Checkpoints, in step order (whole-world hashes only).
    pub checkpoints: Vec<Checkpoint>,
    /// The first checkpoint that differs from the recording's, if any.
    pub divergence: Option<Divergence>,
}

/// Groups a snapshot's values by entity key and hashes each group.
pub fn entity_hashes(snapshot: &Snapshot) -> BTreeMap<String, u64> {
    let mut text: BTreeMap<&str, String> = BTreeMap::new();
    for (key, value) in snapshot {
        let (entity, component) = key.rsplit_once('/').unwrap_or((key.as_str(), ""));
        let _ = writeln!(text.entry(entity).or_default(), "{component}={value}");
    }
    text.into_iter()
        .map(|(entity, text)| (entity.to_string(), fnv1a(text.as_bytes())))
        .collect()
}

fn cut(text: Option<&String>) -> String {
    match text {
        None => "<absent>".to_string(),
        Some(text) if text.chars().count() > MAX_VALUE_CHARS => {
            let mut short: String = text.chars().take(MAX_VALUE_CHARS).collect();
            short.push_str("...");
            short
        }
        Some(text) => text.clone(),
    }
}

/// The differing values between two snapshots (at most [`MAX_DIFFS`]).
pub fn value_diffs(record: &Snapshot, replay: &Snapshot) -> Vec<ValueDiff> {
    differing_keys(record, replay, MAX_DIFFS)
        .into_iter()
        .map(|key| ValueDiff {
            record: cut(record.get(&key)),
            replay: cut(replay.get(&key)),
            key,
        })
        .collect()
}

/// Records `config.steps` steps of `scene` under `seed`, driven by an [`InputStream`].
pub fn record(scene: &SoakScene, seed: u64, config: &SoakConfig) -> Recorded {
    let mut app = (scene.build)(seed);
    let names: Vec<String> = scene.actions.iter().map(|(n, _)| (*n).to_string()).collect();
    app.world_mut()
        .resource_mut::<InputRecorder>()
        .record_with(seed, names);
    let mut stream = InputStream::new(seed, scene.actions);
    let mut checkpoints = Vec::new();
    let mut snapshots = Vec::new();
    let mut active_inputs = 0u32;
    for step in 1..=config.steps {
        // Scheduled for the frame about to run: `update_time` advances the counter first.
        let frame = app.world().resource::<Time>().frame_count() + 1;
        let values = stream.next_step();
        {
            let mut injection = app.world_mut().resource_mut::<InputInjection>();
            for (name, value) in values {
                if value != Vec3::ZERO {
                    active_inputs += 1;
                }
                injection.schedule(name, value, frame, 1);
            }
        }
        app.update();
        if config.is_checkpoint(step) {
            let snap = scene.snapshot(app.world());
            checkpoints.push(Checkpoint {
                step,
                hash: snapshot_hash(&snap),
                entities: entity_hashes(&snap),
            });
            snapshots.push(snap);
        }
    }
    app.world_mut().resource_mut::<InputRecorder>().stop();
    let recording = app.world().resource::<InputRecorder>().recording.clone();
    Recorded {
        scene: scene.name,
        seed,
        recording,
        checkpoints,
        snapshots,
        active_inputs,
    }
}

/// Replays `recording` on a fresh `scene` (built for the recording's seed) and compares every
/// checkpoint with `against` (normally [`record`]'s output for the same recording).
pub fn replay(
    scene: &SoakScene,
    recording: &InputRecording,
    against: &Recorded,
    config: &SoakConfig,
) -> Replayed {
    let mut app = (scene.build)(recording.seed);
    app.world_mut()
        .resource_mut::<InputRecorder>()
        .play(recording.clone());
    let mut checkpoints = Vec::new();
    let mut divergence = None;
    let mut index = 0;
    for step in 1..=config.steps {
        app.update();
        if !config.is_checkpoint(step) {
            continue;
        }
        let snap = scene.snapshot(app.world());
        let hash = snapshot_hash(&snap);
        if divergence.is_none() {
            match (against.checkpoints.get(index), against.snapshots.get(index)) {
                (Some(expected), Some(expected_snap)) if expected.step == step => {
                    if expected.hash != hash {
                        divergence = Some(Divergence {
                            step,
                            differing: value_diffs(expected_snap, &snap),
                        });
                    }
                }
                _ => {
                    divergence = Some(Divergence {
                        step,
                        differing: vec![ValueDiff {
                            key: "<checkpoint>".to_string(),
                            record: "<absent>".to_string(),
                            replay: format!("step {step}"),
                        }],
                    });
                }
            }
        }
        index += 1;
        checkpoints.push(Checkpoint {
            step,
            hash,
            entities: BTreeMap::new(),
        });
    }
    Replayed {
        checkpoints,
        divergence,
    }
}

/// One `(scene, seed)` result.
#[derive(Debug, Clone)]
pub struct SeedReport {
    /// The scene.
    pub scene: String,
    /// The seed.
    pub seed: u64,
    /// Entities in the last record snapshot.
    pub entities: usize,
    /// Values (`<entity>/<Component>` keys) in the last record snapshot.
    pub values: usize,
    /// Step-action pairs whose recorded value was non-zero.
    pub active_inputs: u32,
    /// The record run's checkpoints (with per-entity hashes).
    pub record: Vec<Checkpoint>,
    /// The replay run's checkpoints.
    pub replay: Vec<Checkpoint>,
    /// Where the replay first left the recording.
    pub replay_divergence: Option<Divergence>,
}

impl SeedReport {
    /// True when the replay matched the recording at every checkpoint.
    pub fn replay_matches(&self) -> bool {
        self.replay_divergence.is_none()
    }
}

/// Records and replays one `(scene, seed)`.
pub fn soak_seed(scene: &SoakScene, seed: u64, config: &SoakConfig) -> SeedReport {
    let recorded = record(scene, seed, config);
    let replayed = replay(scene, &recorded.recording, &recorded, config);
    let last = recorded.snapshots.last();
    SeedReport {
        scene: scene.name.to_string(),
        seed,
        entities: recorded
            .checkpoints
            .last()
            .map_or(0, |checkpoint| checkpoint.entities.len()),
        values: last.map_or(0, BTreeMap::len),
        active_inputs: recorded.active_inputs,
        record: recorded.checkpoints,
        replay: replayed.checkpoints,
        replay_divergence: replayed.divergence,
    }
}

/// Sizes the global compute pools to `requested` threads (all three pools; `None` keeps the
/// default formula) and returns the compute pool's actual size. Only the first call in a process
/// sizes the pools, so call it before building any app.
pub fn init_threads(requested: Option<usize>) -> usize {
    regia_core::tasks::init(regia_core::tasks::TaskPoolOptions {
        compute_threads: requested,
        async_compute_threads: requested,
        io_threads: requested,
        ..regia_core::tasks::TaskPoolOptions::default()
    })
    .compute
}

/// One process's soak: every `(scene, seed)` at one thread count and build.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// Run label (`<build>-t<threads>` from `cargo xtask replay-soak`, or `local`).
    pub label: String,
    /// Build profile the test binary was compiled with (`debug`, `release`, ...).
    pub build: String,
    /// Compute threads asked for (`None`: the default formula).
    pub threads_requested: Option<usize>,
    /// Compute threads the pool actually has.
    pub threads: usize,
    /// The run size.
    pub config: SoakConfig,
    /// Per `(scene, seed)` results, scene-major in [`SCENES`] order.
    pub runs: Vec<SeedReport>,
}

impl RunReport {
    /// Runs every `(scene, seed)` pair.
    pub fn run(
        scenes: &[SoakScene],
        seeds: impl IntoIterator<Item = u64> + Clone,
        config: SoakConfig,
        label: impl Into<String>,
        build: impl Into<String>,
        threads_requested: Option<usize>,
        threads: usize,
    ) -> Self {
        let mut runs = Vec::new();
        for scene in scenes {
            for seed in seeds.clone() {
                runs.push(soak_seed(scene, seed, &config));
            }
        }
        Self {
            label: label.into(),
            build: build.into(),
            threads_requested,
            threads,
            config,
            runs,
        }
    }

    /// The runs whose replay left the recording.
    pub fn replay_divergences(&self) -> Vec<&SeedReport> {
        self.runs.iter().filter(|r| !r.replay_matches()).collect()
    }

    /// True when every replay matched its recording.
    pub fn is_clean(&self) -> bool {
        self.replay_divergences().is_empty()
    }

    /// A short markdown summary (one line per run, divergences in full).
    pub fn to_markdown(&self) -> String {
        let mut out = format!(
            "replay-soak {}: build {}, {} compute threads, {} steps (checkpoint every {}), {} runs\n",
            self.label,
            self.build,
            self.threads,
            self.config.steps,
            self.config.every,
            self.runs.len()
        );
        for run in &self.runs {
            let last = run.record.last().map_or(0, |c| c.hash);
            let verdict = match &run.replay_divergence {
                None => "replay identical".to_string(),
                Some(d) => format!("REPLAY DIVERGES at step {}", d.step),
            };
            let _ = writeln!(
                out,
                "- {} seed {}: {} entities, {} values, {} active inputs, final {last:016x}: {verdict}",
                run.scene, run.seed, run.entities, run.values, run.active_inputs
            );
            if let Some(divergence) = &run.replay_divergence {
                for diff in &divergence.differing {
                    let _ = writeln!(
                        out,
                        "  - {}\n    record: {}\n    replay: {}",
                        diff.key, diff.record, diff.replay
                    );
                }
            }
        }
        out
    }

    /// The report as JSON (format [`RUN_FORMAT`], version [`RUN_FORMAT_VERSION`];
    /// `book/agents/replay-soak.md` documents every field). Hashes are 16-digit lowercase hex
    /// strings.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n  \"format\": ");
        json_string(&mut out, RUN_FORMAT);
        let _ = write!(out, ",\n  \"version\": {RUN_FORMAT_VERSION},\n  \"label\": ");
        json_string(&mut out, &self.label);
        out.push_str(",\n  \"build\": ");
        json_string(&mut out, &self.build);
        out.push_str(",\n  \"threads_requested\": ");
        match self.threads_requested {
            Some(n) => {
                let _ = write!(out, "{n}");
            }
            None => out.push_str("null"),
        }
        let _ = write!(out, ",\n  \"threads\": {},\n  \"os\": ", self.threads);
        json_string(&mut out, std::env::consts::OS);
        out.push_str(",\n  \"arch\": ");
        json_string(&mut out, std::env::consts::ARCH);
        let _ = write!(
            out,
            ",\n  \"steps\": {},\n  \"every\": {},\n  \"runs\": [",
            self.config.steps, self.config.every
        );
        for (i, run) in self.runs.iter().enumerate() {
            out.push_str(if i == 0 { "\n    {" } else { ",\n    {" });
            out.push_str("\"scene\": ");
            json_string(&mut out, &run.scene);
            let _ = write!(
                out,
                ", \"seed\": {}, \"entities\": {}, \"values\": {}, \"active_inputs\": {},\n      \"record\": ",
                run.seed, run.entities, run.values, run.active_inputs
            );
            checkpoints_json(&mut out, &run.record);
            out.push_str(",\n      \"replay\": ");
            checkpoints_json(&mut out, &run.replay);
            out.push_str(",\n      \"replay_divergence\": ");
            match &run.replay_divergence {
                None => out.push_str("null"),
                Some(divergence) => {
                    let _ = write!(out, "{{\"step\": {}, \"differing\": [", divergence.step);
                    for (j, diff) in divergence.differing.iter().enumerate() {
                        if j > 0 {
                            out.push_str(", ");
                        }
                        out.push_str("{\"key\": ");
                        json_string(&mut out, &diff.key);
                        out.push_str(", \"record\": ");
                        json_string(&mut out, &diff.record);
                        out.push_str(", \"replay\": ");
                        json_string(&mut out, &diff.replay);
                        out.push('}');
                    }
                    out.push_str("]}");
                }
            }
            out.push('}');
        }
        out.push_str("\n  ]\n}\n");
        out
    }
}

fn checkpoints_json(out: &mut String, checkpoints: &[Checkpoint]) {
    out.push('[');
    for (i, checkpoint) in checkpoints.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(
            out,
            "{{\"step\": {}, \"hash\": \"{:016x}\"",
            checkpoint.step, checkpoint.hash
        );
        if !checkpoint.entities.is_empty() {
            out.push_str(", \"entities\": {");
            for (j, (entity, hash)) in checkpoint.entities.iter().enumerate() {
                if j > 0 {
                    out.push_str(", ");
                }
                json_string(out, entity);
                let _ = write!(out, ": \"{hash:016x}\"");
            }
            out.push('}');
        }
        out.push('}');
    }
    out.push(']');
}

/// Appends `text` as a JSON string literal (quotes, backslashes and control characters escaped).
pub fn json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_escape_quotes_backslashes_and_controls() {
        let mut out = String::new();
        json_string(&mut out, "a\"b\\c\nd\u{1}é");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\u0001é\"");
    }

    #[test]
    fn the_input_stream_is_a_pure_function_of_the_seed() {
        let actions = SCENES[0].actions;
        let take = |seed: u64| {
            let mut stream = InputStream::new(seed, actions);
            (0..200).map(|_| stream.next_step()).collect::<Vec<_>>()
        };
        assert_eq!(take(3), take(3));
        assert_ne!(take(3), take(4));
        let steps = take(3);
        let jumps = steps
            .iter()
            .filter(|step| step.iter().any(|(n, v)| n == "Jump" && *v == Vec3::X))
            .count();
        assert!(
            jumps > 10 && jumps < 150,
            "Jump is pressed some of the time: {jumps}"
        );
        let moving = steps
            .iter()
            .filter(|step| step.iter().any(|(n, v)| n == "Move" && *v != Vec3::ZERO))
            .count();
        assert!(moving > 50, "Move is non-zero most of the time: {moving}");
    }

    #[test]
    fn checkpoints_land_on_multiples_and_the_last_step() {
        let config = SoakConfig { steps: 25, every: 10 };
        let steps: Vec<u32> = (1..=25).filter(|s| config.is_checkpoint(*s)).collect();
        assert_eq!(steps, vec![10, 20, 25]);
        let every_step = SoakConfig { steps: 3, every: 0 };
        assert_eq!((1..=3).filter(|s| every_step.is_checkpoint(*s)).count(), 3);
    }

    #[test]
    fn entity_hashes_group_values_by_entity() {
        let mut snap = Snapshot::new();
        snap.insert("hero/Transform".into(), "a".into());
        snap.insert("hero/Velocity".into(), "b".into());
        snap.insert("box0/Transform".into(), "a".into());
        let hashes = entity_hashes(&snap);
        assert_eq!(hashes.len(), 2);
        let mut changed = snap.clone();
        changed.insert("hero/Velocity".into(), "c".into());
        let after = entity_hashes(&changed);
        assert_eq!(hashes["box0"], after["box0"]);
        assert_ne!(hashes["hero"], after["hero"]);
    }

    #[test]
    fn value_diffs_cut_long_values_and_mark_missing_keys() {
        let mut a = Snapshot::new();
        a.insert("x/Long".into(), "y".repeat(MAX_VALUE_CHARS + 50));
        a.insert("x/Gone".into(), "1".into());
        let mut b = Snapshot::new();
        b.insert("x/Long".into(), "z".into());
        let diffs = value_diffs(&a, &b);
        assert_eq!(diffs.len(), 2);
        let gone = diffs
            .iter()
            .find(|d| d.key == "x/Gone")
            .expect("missing key listed");
        assert_eq!(gone.replay, "<absent>");
        let long = diffs
            .iter()
            .find(|d| d.key == "x/Long")
            .expect("changed key listed");
        assert_eq!(long.record.chars().count(), MAX_VALUE_CHARS + 3);
        assert!(long.record.ends_with("..."));
    }
}
