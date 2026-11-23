// SPDX-License-Identifier: MIT-0
//! Vehicle lab (ADR-2598): one rear-wheel drive car with a geared drivetrain and one hover craft on
//! a flat track with a ramp, all from primitives. An agent drives both with `physics.vehicle_input`
//! and reads them with `physics.vehicle_telemetry` and `physics.hovercraft`
//! (`book/agents/vehicles.md`). `vehicle_lab` without `--no-script` runs [`LabScript`] so a window
//! shows something moving; the script writes the input every frame, so start with `--no-script` to
//! drive them yourself.

use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::convert::{to_rapier_pose, to_rapier_vec3};
use regia_physics::hover::{HoverConfig, HoverController, spawn_hovercraft};
use regia_physics::vehicle::{VehicleConfig, VehicleController, VehicleInput, VehiclePlugin, spawn_vehicle};
use regia_physics::vehicle_asset::VehicleAsset;
use regia_physics::{BodyKind, Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, RigidBody};
use regia_render::{Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
use regia_scene::Transform;

/// The car's tuning (`assets/rwd_car.vehicle.ron`).
pub const CAR_TEXT: &str = include_str!("../assets/rwd_car.vehicle.ron");

/// The same car on slip-angle tires (`assets/slip_car.vehicle.ron`, `.vehicle.ron` v3, ADR-2719): a
/// config to load in place of [`CAR_TEXT`] or to read with `physics.vehicle_tire_curve`.
pub const SLIP_CAR_TEXT: &str = include_str!("../assets/slip_car.vehicle.ron");
/// Where the car starts (chassis center); it faces −Z, down the track.
pub const CAR_SPAWN: Vec3 = Vec3::new(-6.0, 0.9, 0.0);
/// Where the hover craft starts; the ramp is ahead of it.
pub const CRAFT_SPAWN: Vec3 = Vec3::new(10.0, 1.2, 0.0);
/// Half the track's width, meters.
const TRACK_HALF_WIDTH: f32 = 200.0;
/// Half the track's length, meters.
const TRACK_HALF_LENGTH: f32 = 1500.0;
/// The ramp's center along the track, its half extents and its pitch.
const RAMP_Z: f32 = -40.0;
const RAMP_HALF: Vec3 = Vec3::new(5.0, 0.15, 12.0);
const RAMP_PITCH: f32 = 0.09;

/// The car's configuration from `assets/rwd_car.vehicle.ron`.
///
/// # Errors
/// The asset's stable error code when the text does not load.
pub fn car_config() -> Result<VehicleConfig, String> {
    VehicleAsset::from_ron(CAR_TEXT)
        .map(VehicleAsset::into_config)
        .map_err(|error| format!("{} ({error})", error.code()))
}

/// The lab's entities.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabEntities {
    /// The car's chassis.
    pub car: Entity,
    /// The hover craft's chassis.
    pub craft: Entity,
}

/// When present, the lab drives both vehicles itself: a launch, a weave and a stop in a loop, each
/// loop from the start of the track.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct LabScript;

/// The lab's plugin: physics, vehicles, the track and both vehicles.
pub struct VehicleLabPlugin;

impl Plugin for VehicleLabPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((regia_scene::ScenePlugin, PhysicsPlugin, VehiclePlugin))
            .add_systems(Startup, setup)
            .add_systems(Update, (run_script, follow_car));
    }
}

/// Spawns the sun, the camera, the track, the ramp and both vehicles. Logs and leaves a vehicle out
/// when it does not spawn.
fn setup(world: &mut World) {
    world.spawn((
        DirectionalLight {
            direction: [-0.4, -1.0, -0.3],
            color: [1.0, 0.96, 0.9],
            illuminance: 20000.0,
            cast_shadows: true,
            static_shadow: false,
        },
        Transform::default(),
    ));
    world.spawn((
        Transform::from_translation(CAR_SPAWN + CAMERA_OFFSET),
        Camera {
            fov_y: 60.0_f32.to_radians(),
            near: 0.1,
            far: 2000.0,
            exposure_ev: Some(12.7),
            clear: ClearMode::Color([0.45, 0.62, 0.85, 1.0]),
            ..Camera::default()
        },
        LabCamera,
    ));
    spawn_static(
        world,
        Vec3::new(0.0, -0.25, -TRACK_HALF_LENGTH + 50.0),
        Vec3::new(TRACK_HALF_WIDTH, 0.25, TRACK_HALF_LENGTH),
        Quat::IDENTITY,
        [0.24, 0.26, 0.28, 1.0],
    );
    spawn_static(
        world,
        Vec3::new(CRAFT_SPAWN.x, 1.0, RAMP_Z),
        RAMP_HALF,
        Quat::from_rotation_x(RAMP_PITCH),
        [0.55, 0.42, 0.22, 1.0],
    );

    let config = match car_config() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(target: "vehicle_lab", "rwd_car.vehicle.ron did not load: {error}");
            return;
        }
    };
    let half = config.chassis_half_extents;
    let car = match spawn_vehicle(world, config, CAR_SPAWN, Quat::IDENTITY) {
        Ok(car) => car,
        Err(error) => {
            tracing::error!(target: "vehicle_lab", "the car was not spawned: {error}");
            return;
        }
    };
    world.entity_mut(car).insert(MeshRenderer::new(
        box_mesh(half),
        solid_material([0.78, 0.14, 0.10, 1.0]),
    ));
    let craft_config = HoverConfig::default();
    let half = craft_config.chassis_half_extents;
    let craft = match spawn_hovercraft(world, craft_config, CRAFT_SPAWN, Quat::IDENTITY) {
        Ok(craft) => craft,
        Err(error) => {
            tracing::error!(target: "vehicle_lab", "the hover craft was not spawned: {error}");
            return;
        }
    };
    world.entity_mut(craft).insert(MeshRenderer::new(
        box_mesh(half),
        solid_material([0.12, 0.35, 0.72, 1.0]),
    ));
    world.insert_resource(LabEntities { car, craft });
}

/// The camera that follows the car.
#[derive(Component)]
struct LabCamera;

/// Where the camera sits against the car.
const CAMERA_OFFSET: Vec3 = Vec3::new(5.0, 4.0, 13.0);

/// Keeps the camera behind the car, looking down the track.
fn follow_car(
    entities: Option<Res<LabEntities>>,
    bodies: Query<&Transform, Without<LabCamera>>,
    mut cameras: Query<&mut Transform, With<LabCamera>>,
) {
    let Some(entities) = entities else {
        return;
    };
    let Ok(car) = bodies.get(entities.car) else {
        return;
    };
    for mut camera in &mut cameras {
        camera.translation = car.translation + CAMERA_OFFSET;
        camera.rotation = Quat::from_rotation_x(-0.25);
    }
}

/// Puts both vehicles back at their spawn poses, at rest. Logs when one has gone missing.
fn back_to_the_start(
    physics: &mut PhysicsWorld,
    cars: &mut Query<&mut VehicleController>,
    craft: &Query<&HoverController>,
) {
    for mut car in cars.iter_mut() {
        if let Err(error) = car.vehicle.reset_to_spawn(physics) {
            tracing::warn!(target: "vehicle_lab", "the car did not reset: {error}");
        }
    }
    for controller in craft.iter() {
        if let Some(body) = physics.bodies.get_mut(controller.craft.chassis().to_handle()) {
            body.set_position(to_rapier_pose(CRAFT_SPAWN, Quat::IDENTITY), true);
            body.set_linvel(to_rapier_vec3(Vec3::ZERO), true);
            body.set_angvel(to_rapier_vec3(Vec3::ZERO), true);
        }
    }
}

/// Drives both vehicles when [`LabScript`] is present: 12 s of launch, 3 s of a weave, 5 s of braking,
/// and back to the start, so the loop never runs out of track.
fn run_script(
    script: Option<Res<LabScript>>,
    time: Res<Time>,
    mut finished: Local<u32>,
    mut physics: ResMut<PhysicsWorld>,
    mut inputs: Query<&mut VehicleInput>,
    mut cars: Query<&mut VehicleController>,
    craft: Query<&HoverController>,
) {
    if script.is_none() {
        return;
    }
    let loops = (time.elapsed_secs() / LOOP_SECONDS) as u32;
    if loops > *finished {
        *finished = loops;
        back_to_the_start(&mut physics, &mut cars, &craft);
    }
    let phase = time.elapsed_secs() % LOOP_SECONDS;
    let input = VehicleInput {
        throttle: if phase < 15.0 { 1.0 } else { 0.0 },
        brake: if phase < 15.0 { 0.0 } else { 1.0 },
        steer: if (12.0..15.0).contains(&phase) {
            0.15 * (phase * 3.0).sin()
        } else {
            0.0
        },
    };
    for mut vehicle in &mut inputs {
        if *vehicle != input {
            *vehicle = input;
        }
    }
}

/// One loop of [`LabScript`], seconds.
const LOOP_SECONDS: f32 = 20.0;

/// How many fixed steps [`run_headless`] drives by.
pub const HEADLESS_STEPS: usize = 1200;

/// What [`run_headless`] saw.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessReport {
    /// Upshifts the car made.
    pub upshifts: u32,
    /// The car's speed at the end, m/s.
    pub car_speed: f32,
    /// The hover craft's forward speed at the end, m/s.
    pub craft_speed: f32,
    /// The physics world's digest at the end.
    pub world_digest: u64,
    /// The car's drivetrain digest at the end.
    pub car_digest: u64,
}

/// Runs the lab's physics without a renderer for [`HEADLESS_STEPS`] fixed steps at full throttle.
///
/// # Errors
/// When the car's asset does not load, or a vehicle went missing.
pub fn run_headless() -> Result<HeadlessReport, String> {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, VehicleLabPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
    // Startup and the sync chain that creates the track's colliders.
    for _ in 0..3 {
        app.update();
    }
    let entities = *app
        .world()
        .get_resource::<LabEntities>()
        .ok_or_else(|| "the lab has no vehicles".to_string())?;
    for _ in 0..60 {
        app.update();
    }
    for entity in [entities.car, entities.craft] {
        if let Some(mut input) = app.world_mut().get_mut::<VehicleInput>(entity) {
            input.throttle = 1.0;
        }
    }
    for _ in 0..HEADLESS_STEPS {
        app.update();
    }
    let world = app.world();
    let physics = world
        .get_resource::<regia_physics::PhysicsWorld>()
        .ok_or_else(|| "no physics world".to_string())?;
    let controller = world
        .get::<VehicleController>(entities.car)
        .ok_or_else(|| "the car went missing".to_string())?;
    let drivetrain = controller
        .vehicle
        .drivetrain()
        .ok_or_else(|| "the car has no drivetrain".to_string())?;
    let car = controller.vehicle.state(physics).map_err(|e| e.to_string())?;
    let craft = world
        .get::<regia_physics::hover::HoverController>(entities.craft)
        .ok_or_else(|| "the hover craft went missing".to_string())?
        .craft
        .state(physics)
        .map_err(|e| e.to_string())?;
    Ok(HeadlessReport {
        upshifts: drivetrain.upshifts(),
        car_speed: car.speed,
        craft_speed: craft.forward_speed,
        world_digest: physics.state_digest(),
        car_digest: controller.vehicle.state_digest(),
    })
}

/// Spawns a static box with a visible mesh.
fn spawn_static(world: &mut World, center: Vec3, half_extents: Vec3, rotation: Quat, colour: [f32; 4]) {
    world.spawn((
        Transform {
            translation: center,
            rotation,
            ..Default::default()
        },
        RigidBody {
            kind: BodyKind::Static,
            ..Default::default()
        },
        Collider {
            shape: ColliderShape::Box { half_extents },
            ..Default::default()
        },
        MeshRenderer::new(box_mesh(half_extents), solid_material(colour)),
    ));
}

fn solid_material(colour: [f32; 4]) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color: colour,
        metallic: 0.0,
        roughness: 0.9,
        ..Default::default()
    })
}

/// A box mesh with per-face normals.
fn box_mesh(half: Vec3) -> Arc<CpuMesh> {
    /// A face's normal and the two in-plane axes whose cross product is that normal.
    const FACES: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices: Vec<u32> = Vec::with_capacity(36);
    for (normal, u, v) in FACES {
        let base = vertices.len() as u32;
        let (normal, u, v) = (Vec3::from(normal), Vec3::from(u), Vec3::from(v));
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let position = (normal + u * su + v * sv) * half;
            vertices.push(LabVertex {
                position: position.to_array(),
                normal: normal.to_array(),
                uv: [(su + 1.0) * 0.5, (sv + 1.0) * 0.5],
                tangent: [u.x, u.y, u.z, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Arc::new(CpuMesh { vertices, indices })
}
