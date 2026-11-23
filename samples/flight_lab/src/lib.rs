// SPDX-License-Identifier: MIT-0
//! Flight lab (ADR-3374): a small plane (`assets/trainer.aircraft.ron`) parked at the start of a 3 km
//! runway, all from primitives. An agent flies it with `physics.aircraft_input`, reads it with
//! `physics.aircraft` and levels it off with `physics.aircraft_trim` (`book/agents/aircraft.md`).
//! `flight_lab` without `--no-script` runs [`FlightScript`]: the [`Autopilot`] takes off, climbs and
//! levels off at [`LEVEL_ALTITUDE`], so a window shows something flying; start with `--no-script` to fly
//! it yourself.

use std::sync::Arc;

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::aircraft::{AircraftAssist, AircraftPlugin};
use regia_physics::aero::asset::AircraftAsset;
use regia_physics::aero::trim::{Trim, TrimTarget, solve};
use regia_physics::aero::{AircraftConfig, AircraftController, AircraftInput, AircraftState, spawn_aircraft};
use regia_physics::{BodyKind, Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, RigidBody};
use regia_render::{Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
use regia_scene::Transform;

/// The plane (`assets/trainer.aircraft.ron`).
pub const PLANE_TEXT: &str = include_str!("../assets/trainer.aircraft.ron");
/// Where the plane is spawned (body origin = center of mass), just above its gear's rest height,
/// facing −Z down the runway.
pub const PLANE_SPAWN: Vec3 = Vec3::new(0.0, 1.3, 0.0);
/// The altitude the autopilot levels off at, meters above the runway.
pub const LEVEL_ALTITUDE: f32 = 150.0;
/// The airspeed the autopilot rotates at, m/s.
pub const ROTATE_SPEED: f32 = 27.0;
/// Half the runway's width, meters.
const RUNWAY_HALF_WIDTH: f32 = 30.0;
/// Half the runway's length, meters.
const RUNWAY_HALF_LENGTH: f32 = 1500.0;

/// The plane's configuration from `assets/trainer.aircraft.ron`.
///
/// # Errors
/// The asset's stable error code when the text does not load.
pub fn plane_config() -> Result<AircraftConfig, String> {
    AircraftAsset::from_ron(PLANE_TEXT)
        .map(AircraftAsset::into_config)
        .map_err(|error| format!("{} ({error})", error.code()))
}

/// The lab's plane entity.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabPlane(pub Entity);

/// When present, the [`Autopilot`] flies the plane every fixed step.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct FlightScript;

/// The lab's plugin: physics, aircraft, the runway and the plane.
pub struct FlightLabPlugin;

impl Plugin for FlightLabPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((regia_scene::ScenePlugin, PhysicsPlugin, AircraftPlugin))
            .add_systems(Startup, setup)
            .add_systems(
                FixedUpdate,
                run_script
                    .before(regia_physics::aero::drive_aircraft)
                    .after(regia_framework::aircraft::map_aircraft_input),
            )
            .add_systems(Update, follow_plane);
    }
}

/// Spawns the sun, the camera, the runway and the plane. Logs and leaves the plane out when its asset
/// does not load.
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
        Transform::from_translation(PLANE_SPAWN + CAMERA_OFFSET),
        Camera {
            fov_y: 60.0_f32.to_radians(),
            near: 0.1,
            far: 4000.0,
            exposure_ev: Some(12.7),
            clear: ClearMode::Color([0.45, 0.62, 0.85, 1.0]),
            ..Camera::default()
        },
        LabCamera,
    ));
    let runway_half = Vec3::new(RUNWAY_HALF_WIDTH, 0.5, RUNWAY_HALF_LENGTH);
    world.spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, -RUNWAY_HALF_LENGTH + 100.0)),
        RigidBody {
            kind: BodyKind::Static,
            ..Default::default()
        },
        Collider {
            shape: ColliderShape::Box {
                half_extents: runway_half,
            },
            ..Default::default()
        },
        MeshRenderer::new(
            merged_boxes(&[(Vec3::ZERO, runway_half)]),
            solid_material([0.24, 0.26, 0.28, 1.0]),
        ),
    ));
    let config = match plane_config() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(target: "flight_lab", "trainer.aircraft.ron did not load: {error}");
            return;
        }
    };
    let mesh = plane_mesh(&config);
    let plane = match spawn_aircraft(world, config, PLANE_SPAWN, Quat::IDENTITY, Vec3::ZERO) {
        Ok(plane) => plane,
        Err(error) => {
            tracing::error!(target: "flight_lab", "the plane was not spawned: {error}");
            return;
        }
    };
    world.entity_mut(plane).insert((
        MeshRenderer::new(mesh, solid_material([0.85, 0.85, 0.82, 1.0])),
        AircraftAssist::default(),
        AircraftInput {
            brakes: 1.0,
            ..AircraftInput::default()
        },
    ));
    world.insert_resource(LabPlane(plane));
}

/// The plane's boxes: the fuselage collider and a thin slab per surface.
fn plane_mesh(config: &AircraftConfig) -> Arc<CpuMesh> {
    let mut boxes = vec![(Vec3::ZERO, config.fuselage_half_extents)];
    for surface in &config.surfaces {
        let (_, normal, span) = surface.axes();
        let thickness = 0.04;
        // An axis-aligned slab: wide along the span axis, thin along the normal.
        let half =
            span.abs() * (surface.span * 0.5) + normal.abs() * thickness + Vec3::Z * (surface.chord * 0.5);
        boxes.push((surface.position + Vec3::Z * (surface.chord * 0.25), half));
    }
    merged_boxes(&boxes)
}

/// The camera that follows the plane.
#[derive(Component)]
struct LabCamera;

/// Where the camera sits against the plane.
const CAMERA_OFFSET: Vec3 = Vec3::new(8.0, 4.0, 18.0);

/// Keeps the camera behind the plane.
fn follow_plane(
    plane: Option<Res<LabPlane>>,
    bodies: Query<&Transform, Without<LabCamera>>,
    mut cameras: Query<&mut Transform, With<LabCamera>>,
) {
    let Some(plane) = plane else {
        return;
    };
    let Ok(body) = bodies.get(plane.0) else {
        return;
    };
    for mut camera in &mut cameras {
        camera.translation = body.translation + CAMERA_OFFSET;
        camera.rotation = Quat::from_rotation_x(-0.2);
    }
}

/// The speed the autopilot cruises at after leveling off, m/s.
pub const CRUISE_SPEED: f32 = 45.0;
/// The pitch attitude the autopilot climbs at, radians.
const CLIMB_PITCH: f32 = 0.17;

/// The lab's autopilot. It trims the plane once (level flight at [`CRUISE_SPEED`] and
/// [`LEVEL_ALTITUDE`], `regia_physics::aero::trim`) and flies around that trim: full throttle and brakes
/// off on the ground, rotate at [`ROTATE_SPEED`], climb at about 10° nose up, then hold
/// [`LEVEL_ALTITUDE`] at the trim's throttle with a climb-rate loop on the pitch attitude. The pitch input
/// is the trim's plus an attitude loop damped by the pitch rate; the roll input holds the wings level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Autopilot {
    /// The level trim the autopilot flies around.
    pub trim: Trim,
}

impl Autopilot {
    /// Trims `config` for the autopilot.
    ///
    /// # Errors
    /// The trim's stable error code.
    pub fn new(config: &AircraftConfig) -> Result<Self, String> {
        solve(config, &TrimTarget::level(CRUISE_SPEED, LEVEL_ALTITUDE))
            .map(|trim| Self { trim })
            .map_err(|error| format!("{} ({error})", error.code()))
    }

    /// The input for one fixed step.
    #[must_use]
    pub fn input(&self, state: &AircraftState) -> AircraftInput {
        let trim = &self.trim;
        let altitude = state.translation.y;
        let speed = state.air.airspeed.max(1.0);
        // Right roll is rotation about −Z, so the right-roll rate is −body_rates.z.
        let roll = (-1.5 * state.bank + 0.3 * state.body_rates.z).clamp(-1.0, 1.0);
        if state.grounded_gear > 0 && state.air.airspeed < ROTATE_SPEED {
            return AircraftInput {
                throttle: 1.0,
                roll,
                ..AircraftInput::default()
            };
        }
        let climb_angle = (state.linear_velocity.y / speed).clamp(-1.0, 1.0);
        let (target_pitch, throttle) = if altitude < LEVEL_ALTITUDE - 20.0 {
            (CLIMB_PITCH, 1.0)
        } else {
            let wanted = (0.15 * (LEVEL_ALTITUDE - altitude)).clamp(-4.0, 4.0) / speed;
            (
                (trim.pitch_attitude + wanted + 1.5 * (wanted - climb_angle)).clamp(-0.1, 0.2),
                trim.throttle,
            )
        };
        let pitch =
            (trim.pitch_input + 3.0 * (target_pitch - state.pitch) - state.body_rates.x).clamp(-1.0, 1.0);
        AircraftInput {
            pitch,
            roll,
            throttle,
            ..AircraftInput::default()
        }
    }
}

/// Flies the plane with the [`Autopilot`] when [`FlightScript`] is present (the autopilot trims the
/// plane on its first step).
fn run_script(
    script: Option<Res<FlightScript>>,
    physics: Option<Res<PhysicsWorld>>,
    mut pilot: Local<Option<Autopilot>>,
    mut planes: Query<(&AircraftController, &mut AircraftInput)>,
) {
    let (Some(_), Some(physics)) = (script, physics) else {
        return;
    };
    for (controller, mut input) in &mut planes {
        if pilot.is_none() {
            match Autopilot::new(controller.aircraft.config()) {
                Ok(autopilot) => *pilot = Some(autopilot),
                Err(error) => {
                    tracing::error!(target: "flight_lab", "the autopilot could not trim the plane: {error}");
                    return;
                }
            }
        }
        let (Some(autopilot), Ok(state)) = (pilot.as_ref(), controller.aircraft.state(&physics)) else {
            continue;
        };
        let next = autopilot.input(&state);
        if *input != next {
            *input = next;
        }
    }
}

/// What [`run_headless`] saw.
#[derive(Debug, Clone, PartialEq)]
pub struct FlightReport {
    /// Distance from the start of the roll to the last step on the ground, meters.
    pub lift_off_distance: Option<f32>,
    /// Fixed step of the lift-off.
    pub lift_off_step: Option<usize>,
    /// Altitude at the end, meters.
    pub altitude: f32,
    /// Highest altitude reached, meters.
    pub max_altitude: f32,
    /// Climb rate at the end, m/s.
    pub climb_rate: f32,
    /// Airspeed at the end, m/s.
    pub airspeed: f32,
    /// The plane stalled at some step.
    pub stalled: bool,
    /// The physics world's digest at the end.
    pub world_digest: u64,
}

/// Runs the lab without a renderer: 120 fixed steps parked with the brakes set, then `steps` steps
/// under the [`Autopilot`].
///
/// # Errors
/// When the plane's asset does not load, or the plane went missing.
pub fn run_headless(steps: usize) -> Result<FlightReport, String> {
    trace_headless(steps, 0)
}

/// [`run_headless`] that also prints the plane's state every `every` steps (0: never), for tuning.
///
/// # Errors
/// As [`run_headless`].
pub fn trace_headless(steps: usize, every: usize) -> Result<FlightReport, String> {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, FlightLabPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
    for _ in 0..120 {
        app.update();
    }
    let plane = app
        .world()
        .get_resource::<LabPlane>()
        .ok_or_else(|| "the lab has no plane".to_string())?
        .0;
    let start = plane_state(&app, plane)?.translation;
    app.insert_resource(FlightScript);
    let mut report = FlightReport {
        lift_off_distance: None,
        lift_off_step: None,
        altitude: 0.0,
        max_altitude: 0.0,
        climb_rate: 0.0,
        airspeed: 0.0,
        stalled: false,
        world_digest: 0,
    };
    let mut was_grounded = true;
    for step in 0..steps {
        app.update();
        let state = plane_state(&app, plane)?;
        let grounded = state.grounded_gear > 0;
        if was_grounded && !grounded && report.lift_off_step.is_none() && state.air.airspeed > ROTATE_SPEED {
            report.lift_off_step = Some(step);
            report.lift_off_distance = Some(start.z - state.translation.z);
        }
        if grounded && report.lift_off_step.is_some() && state.translation.y < 3.0 {
            // Bounced back onto the runway: count the lift-off again.
            report.lift_off_step = None;
        }
        was_grounded = grounded;
        report.max_altitude = report.max_altitude.max(state.translation.y);
        report.stalled |= state.stalled;
        report.altitude = state.translation.y;
        report.climb_rate = state.linear_velocity.y;
        report.airspeed = state.air.airspeed;
        if every > 0 && step.is_multiple_of(every) {
            let input = plane_input(&app, plane);
            println!(
                "step {step:5} z {:8.1} y {:6.1} vy {:5.2} V {:5.1} alpha {:5.1} pitch {:5.1} q {:5.2} cl {:5.2} \
                 stall {} gear {} input p {:5.2} t {:4.2}",
                state.translation.z,
                state.translation.y,
                state.linear_velocity.y,
                state.air.airspeed,
                state.air.alpha.to_degrees(),
                state.pitch.to_degrees(),
                state.body_rates.x,
                state.cl,
                state.stalled,
                state.grounded_gear,
                input.pitch,
                input.throttle,
            );
        }
    }
    report.world_digest = app
        .world()
        .get_resource::<PhysicsWorld>()
        .ok_or_else(|| "no physics world".to_string())?
        .state_digest();
    Ok(report)
}

fn plane_input(app: &App, plane: Entity) -> AircraftInput {
    app.world()
        .get::<AircraftInput>(plane)
        .copied()
        .unwrap_or_default()
}

fn plane_state(app: &App, plane: Entity) -> Result<AircraftState, String> {
    let world = app.world();
    let physics = world
        .get_resource::<PhysicsWorld>()
        .ok_or_else(|| "no physics world".to_string())?;
    world
        .get::<AircraftController>(plane)
        .ok_or_else(|| "the plane went missing".to_string())?
        .aircraft
        .state(physics)
        .map_err(|e| e.to_string())
}

fn solid_material(colour: [f32; 4]) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color: colour,
        metallic: 0.0,
        roughness: 0.9,
        ..Default::default()
    })
}

/// Box meshes (center, half extents) merged into one mesh with per-face normals.
fn merged_boxes(boxes: &[(Vec3, Vec3)]) -> Arc<CpuMesh> {
    /// A face's normal and the two in-plane axes whose cross product is that normal.
    const FACES: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    let mut vertices = Vec::with_capacity(24 * boxes.len());
    let mut indices: Vec<u32> = Vec::with_capacity(36 * boxes.len());
    for &(center, half) in boxes {
        for (normal, u, v) in FACES {
            let base = vertices.len() as u32;
            let (normal, u, v) = (Vec3::from(normal), Vec3::from(u), Vec3::from(v));
            for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let position = center + (normal + u * su + v * sv) * half;
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
    }
    Arc::new(CpuMesh { vertices, indices })
}
