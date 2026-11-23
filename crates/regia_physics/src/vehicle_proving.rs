// SPDX-License-Identifier: MPL-2.0
//! A proving ground for vehicle tuning (ADR-9760): a scripted run of one car on a flat plane in a
//! private [`PhysicsWorld`], with the metrics a tuner compares (0 to 100 km/h, top speed, skid-pad
//! lateral g) and a telemetry trace.
//!
//! The world has nothing but the car and the plane, steps at a fixed 60 Hz and reads no clock, so a
//! run is a pure function of the configuration, the script and its length: two runs give the same
//! [`ProvingRun::digest`]. The editor's Vehicle Tuning panel records these runs and compares them
//! side by side (`editor.vehicle_tuning_run`, `book/agents/vehicles.md`).
//!
//! The scripts write the same [`VehicleInput`] that `physics.vehicle_input` sets:
//! - [`ProvingScript::Accel`]: full throttle in a straight line from rest.
//! - [`ProvingScript::Skidpad`]: a constant right-hand steer of [`SKIDPAD_STEER`] while the throttle
//!   holds a target speed that rises by [`SKIDPAD_RAMP`] m/s every second, so the car circles faster
//!   and faster until its tires give up; the metric is the highest lateral acceleration it held over
//!   half a second.
//!
//! [`run_proving_course`] drives the same scripts on a [`ProvingTrack`] (a ramp, a slope or a bump
//! strip built from fixed colliders in the lane ahead) and, optionally, with a trailer coupled at
//! [`proving_tow_hitch`] (ADR-7745); it adds the wheel-contact counts and the trailer's readout.

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::ColliderBuilder;
use regia_core::math::{Quat, Vec3};
use tracing::debug_span;

use crate::convert::to_rapier_vec3;
use crate::vehicle::{Vehicle, VehicleConfig, VehicleError, VehicleInput};
use crate::vehicle_trailer::{Trailer, TrailerConfig, TrailerError};
use crate::world::PhysicsWorld;

/// The fixed step of a proving run, seconds.
pub const PROVING_DT: f32 = 1.0 / 60.0;

/// Gravity on the proving ground, m/s² (straight down).
pub const PROVING_GRAVITY: f32 = 9.81;

/// The longest run, seconds.
pub const MAX_PROVING_SECONDS: f32 = 120.0;

/// Seconds the car settles on its springs, input released, before the script starts.
pub const SETTLE_SECONDS: f32 = 1.0;

/// The skid pad's steer, a share of full lock to the right.
pub const SKIDPAD_STEER: f32 = 0.5;

/// How fast the skid pad's target speed rises, m/s per second.
pub const SKIDPAD_RAMP: f32 = 1.0;

/// Telemetry samples per second in [`ProvingRun::samples`].
pub const SAMPLES_PER_SECOND: u32 = 10;

/// 100 km/h in m/s.
const KMH_100: f32 = 100.0 / 3.6;

/// Half the plane's side, meters: a car at 400 km/h stays on it for two minutes.
const GROUND_HALF: f32 = 15_000.0;

/// Steps of the rolling mean the lateral acceleration metric uses (half a second).
const LATERAL_WINDOW: usize = 30;

/// What a proving run does with the car.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProvingScript {
    /// Full throttle in a straight line from rest.
    Accel,
    /// A constant steer at a rising speed until the tires let go.
    Skidpad,
}

impl ProvingScript {
    /// The script's name in the agent protocol (`accel`, `skidpad`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Accel => "accel",
            Self::Skidpad => "skidpad",
        }
    }

    /// The script named `name`, if there is one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "accel" => Some(Self::Accel),
            "skidpad" => Some(Self::Skidpad),
            _ => None,
        }
    }

    /// The input the script gives at `time` seconds into the run with the car at `forward_speed`
    /// m/s.
    #[must_use]
    pub fn input(self, time: f32, forward_speed: f32) -> VehicleInput {
        match self {
            Self::Accel => VehicleInput {
                throttle: 1.0,
                brake: 0.0,
                steer: 0.0,
            },
            Self::Skidpad => {
                let target = 5.0 + SKIDPAD_RAMP * time;
                VehicleInput {
                    throttle: ((target - forward_speed) * 0.5).clamp(0.0, 1.0),
                    brake: 0.0,
                    steer: SKIDPAD_STEER,
                }
            }
        }
    }
}

/// One wheel in a telemetry sample.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct ProvingWheel {
    /// Slip ratio (0 with the `Simple` drivetrain).
    pub slip_ratio: f32,
    /// Slip angle, radians.
    pub slip_angle: f32,
    /// Suspension load, newtons.
    pub load: f32,
    /// Suspension compression, meters.
    pub compression: f32,
}

/// The car at one moment of a run.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ProvingSample {
    /// Seconds since the script started.
    pub time: f32,
    /// Speed along the chassis' forward axis, km/h.
    pub speed_kmh: f32,
    /// Engine speed, rpm (0 with the `Simple` drivetrain).
    pub rpm: f32,
    /// The engaged gear (0 with the `Simple` drivetrain).
    pub gear: i32,
    /// Lateral acceleration, in g: yaw rate times speed.
    pub lateral_g: f32,
    /// Every wheel, in configuration order.
    pub wheels: Vec<ProvingWheel>,
}

/// A finished proving run.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ProvingRun {
    /// The script.
    pub script: ProvingScript,
    /// The script's length, seconds.
    pub seconds: f32,
    /// Seconds from the start to 100 km/h, interpolated within the step; `None` if never reached.
    pub zero_to_100_s: Option<f32>,
    /// The highest forward speed, km/h.
    pub top_speed_kmh: f32,
    /// The highest lateral acceleration held over half a second, g.
    pub max_lateral_g: f32,
    /// Distance driven along the plane, meters.
    pub distance_m: f32,
    /// Upshifts during the run (0 with the `Simple` drivetrain).
    pub upshifts: u32,
    /// Telemetry at [`SAMPLES_PER_SECOND`].
    pub samples: Vec<ProvingSample>,
    /// The world's and the vehicle's state digests at the end, combined: equal runs have equal
    /// digests.
    pub digest: u64,
}

/// A mid-run retune: at `step` (counted from the script's start) the car gets `config` through
/// [`Vehicle::set_config`].
#[derive(Debug, Clone, PartialEq)]
pub struct ProvingRetune {
    /// The script step before which the configuration changes.
    pub step: u32,
    /// The new configuration.
    pub config: VehicleConfig,
}

/// Adds the proving ground's plane (top face at y = 0, 30 km square) to `physics`.
pub fn add_proving_ground(physics: &mut PhysicsWorld) {
    let body = physics.bodies.insert(
        RigidBodyBuilder::fixed()
            .translation(to_rapier_vec3(Vec3::new(0.0, -0.5, 0.0)))
            .build(),
    );
    let PhysicsWorld {
        bodies, colliders, ..
    } = physics;
    colliders.insert_with_parent(
        ColliderBuilder::cuboid(GROUND_HALF, 0.5, GROUND_HALF).build(),
        body,
        bodies,
    );
}

/// Where a track's feature starts: meters down the lane (the car drives toward −Z from z = 0).
pub const TRACK_START_Z: f32 = -20.0;

/// Half the width of a track feature across the lane (x), meters.
pub const TRACK_HALF_WIDTH: f32 = 4.0;

/// The ramp: rise and length of the wedge, meters (about 14°).
pub const RAMP_RISE: f32 = 1.5;
/// See [`RAMP_RISE`].
pub const RAMP_LENGTH: f32 = 6.0;

/// The slope: rise over its length (a 10 % grade), then a plateau of [`SLOPE_PLATEAU`] at the top.
pub const SLOPE_RISE: f32 = 6.0;
/// See [`SLOPE_RISE`].
pub const SLOPE_LENGTH: f32 = 60.0;
/// The plateau at the top of the slope, meters long.
pub const SLOPE_PLATEAU: f32 = 400.0;

/// The bump strip: bumps alternate between the left and the right half of the lane, one every
/// [`BUMP_PITCH`] meters, each [`BUMP_HEIGHT`] tall and [`BUMP_LENGTH`] long, [`BUMP_COUNT`] of them.
pub const BUMP_HEIGHT: f32 = 0.12;
/// See [`BUMP_HEIGHT`].
pub const BUMP_LENGTH: f32 = 0.5;
/// See [`BUMP_HEIGHT`].
pub const BUMP_PITCH: f32 = 2.0;
/// See [`BUMP_HEIGHT`].
pub const BUMP_COUNT: u32 = 150;

/// The proving ground's layout: the flat plane, or the plane with one generated feature in the lane
/// ahead of the car (ADR-7745). Every feature is built from fixed colliders, no asset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProvingTrack {
    /// The plane alone.
    #[default]
    Flat,
    /// A wedge [`RAMP_RISE`] high and [`RAMP_LENGTH`] long at [`TRACK_START_Z`]: the car jumps off
    /// its top edge.
    Ramp,
    /// A [`SLOPE_LENGTH`] incline rising [`SLOPE_RISE`], then a flat plateau.
    Slope,
    /// Bumps across alternate halves of the lane (a twist strip): the wheels lose and regain
    /// contact.
    BumpStrip,
}

impl ProvingTrack {
    /// Every track, in the panel's order.
    pub const ALL: [Self; 4] = [Self::Flat, Self::Ramp, Self::Slope, Self::BumpStrip];

    /// The track's name in the agent protocol (`flat`, `ramp`, `slope`, `bump_strip`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Ramp => "ramp",
            Self::Slope => "slope",
            Self::BumpStrip => "bump_strip",
        }
    }

    /// The track named `name`, if there is one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|track| track.name() == name)
    }

    /// The feature's solids in world space: each a list of convex hull points.
    #[must_use]
    pub fn solids(self) -> Vec<Vec<Vec3>> {
        let w = TRACK_HALF_WIDTH;
        let z0 = TRACK_START_Z;
        // A wedge from `z0` (height 0) to `z0 - length` (height `rise`), x in `[x0, x1]`.
        let wedge = |x0: f32, x1: f32, z0: f32, length: f32, rise: f32| {
            let z1 = z0 - length;
            vec![
                Vec3::new(x0, 0.0, z0),
                Vec3::new(x1, 0.0, z0),
                Vec3::new(x0, 0.0, z1),
                Vec3::new(x1, 0.0, z1),
                Vec3::new(x0, rise, z1),
                Vec3::new(x1, rise, z1),
            ]
        };
        // An axis-aligned block, x in `[x0, x1]`, y in `[0, top]`, z in `[z1, z0]`.
        let block = |x0: f32, x1: f32, z0: f32, z1: f32, top: f32| {
            let mut points = Vec::with_capacity(8);
            for x in [x0, x1] {
                for y in [0.0, top] {
                    for z in [z0, z1] {
                        points.push(Vec3::new(x, y, z));
                    }
                }
            }
            points
        };
        match self {
            Self::Flat => Vec::new(),
            Self::Ramp => vec![wedge(-w, w, z0, RAMP_LENGTH, RAMP_RISE)],
            Self::Slope => vec![
                wedge(-w, w, z0, SLOPE_LENGTH, SLOPE_RISE),
                block(
                    -w,
                    w,
                    z0 - SLOPE_LENGTH,
                    z0 - SLOPE_LENGTH - SLOPE_PLATEAU,
                    SLOPE_RISE,
                ),
            ],
            Self::BumpStrip => (0..BUMP_COUNT)
                .map(|i| {
                    let front = z0 - i as f32 * BUMP_PITCH;
                    let (x0, x1) = if i.is_multiple_of(2) { (-w, 0.0) } else { (0.0, w) };
                    // A bump is a low ridge: a ramp up and down across its length.
                    let half = BUMP_LENGTH * 0.5;
                    let mut points = Vec::with_capacity(6);
                    for x in [x0, x1] {
                        points.push(Vec3::new(x, 0.0, front));
                        points.push(Vec3::new(x, BUMP_HEIGHT, front - half));
                        points.push(Vec3::new(x, 0.0, front - BUMP_LENGTH));
                    }
                    points
                })
                .collect(),
        }
    }
}

/// Adds the proving ground's plane and `track`'s feature (fixed convex colliders) to `physics`.
/// Returns how many feature colliders it added. [`ProvingTrack::Flat`] adds exactly what
/// [`add_proving_ground`] adds.
pub fn add_proving_track(physics: &mut PhysicsWorld, track: ProvingTrack) -> usize {
    add_proving_ground(physics);
    let solids = track.solids();
    if solids.is_empty() {
        return 0;
    }
    let body = physics.bodies.insert(RigidBodyBuilder::fixed().build());
    let PhysicsWorld {
        bodies, colliders, ..
    } = physics;
    let mut added = 0;
    for points in solids {
        let points: Vec<rapier3d::math::Vector> = points.into_iter().map(to_rapier_vec3).collect();
        if let Some(shape) = rapier3d::geometry::SharedShape::convex_hull(&points) {
            colliders.insert_with_parent(ColliderBuilder::new(shape).friction(1.0).build(), body, bodies);
            added += 1;
        }
    }
    added
}

/// The tow hitch the proving ground puts on a tuned car: a ball 0.3 m behind the chassis box and
/// 0.25 m below its center line (for the default car, `vehicle_trailer::car_tow_hitch`).
#[must_use]
pub fn proving_tow_hitch(config: &VehicleConfig) -> Vec3 {
    Vec3::new(0.0, -0.25, config.chassis_half_extents.z + 0.3)
}

/// What a course run sets up beyond [`run_proving`]: the track, and a trailer coupled to the car.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProvingCourse {
    /// The proving ground's layout.
    pub track: ProvingTrack,
    /// A trailer on the car's [`proving_tow_hitch`], spawned coupled; `None` drives solo.
    pub trailer: Option<TrailerConfig>,
}

/// What a trailer did on a course run.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct ProvingTrailerRun {
    /// The trailer stayed coupled to the end.
    pub coupled: bool,
    /// The largest articulation angle, radians (absolute).
    pub max_articulation: f32,
    /// The largest coupling gap, meters.
    pub max_coupling_gap: f32,
    /// The trailer's chassis mass, kilograms (the value the run read).
    pub chassis_mass: f32,
}

/// A course run: the [`ProvingRun`] metrics plus the wheel contacts and the trailer.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ProvingCourseRun {
    /// The car's metrics, as [`run_proving`] measures them.
    pub run: ProvingRun,
    /// The track it ran on.
    pub track: ProvingTrack,
    /// The fewest of the car's wheels touching the ground on one step of the script.
    pub min_contacts: u32,
    /// The most of the car's wheels touching the ground on one step of the script.
    pub max_contacts: u32,
    /// Steps of the script on which the number of grounded wheels changed from the step before.
    pub contact_changes: u32,
    /// The highest the chassis' center rose above its start, meters.
    pub max_rise: f32,
    /// The trailer's run, when the course had one.
    pub trailer: Option<ProvingTrailerRun>,
}

/// Runs `script` for `seconds` with `config` on `course`: its track and, if it has one, a trailer
/// coupled at [`proving_tow_hitch`] (stepped after the car each fixed step, braking with it).
/// [`ProvingTrack::Flat`] without a trailer drives the same world as [`run_proving`], so its
/// `run` is bit-for-bit [`run_proving`]'s.
///
/// # Errors
/// As [`run_proving`]; a trailer the car cannot couple reports its [`TrailerError`] as
/// [`VehicleError::InvalidConfig`] on `trailer`.
pub fn run_proving_course(
    config: &VehicleConfig,
    script: ProvingScript,
    seconds: f32,
    course: &ProvingCourse,
) -> Result<ProvingCourseRun, VehicleError> {
    let mut extra = CourseState {
        track: course.track,
        trailer_config: course.trailer.clone(),
        ..CourseState::default()
    };
    let run = run_inner(config, script, seconds, None, Some(&mut extra))?;
    Ok(ProvingCourseRun {
        run,
        track: course.track,
        min_contacts: extra.min_contacts.unwrap_or(0),
        max_contacts: extra.max_contacts,
        contact_changes: extra.contact_changes,
        max_rise: extra.max_rise,
        trailer: extra.trailer_run,
    })
}

/// A course run's bookkeeping inside [`run_inner`].
#[derive(Default)]
struct CourseState {
    track: ProvingTrack,
    trailer_config: Option<TrailerConfig>,
    trailer: Option<Trailer>,
    min_contacts: Option<u32>,
    max_contacts: u32,
    contact_changes: u32,
    last_contacts: Option<u32>,
    start_y: f32,
    max_rise: f32,
    trailer_run: Option<ProvingTrailerRun>,
}

/// Steps `physics` once by [`PROVING_DT`] under [`PROVING_GRAVITY`].
pub fn step_proving(physics: &mut PhysicsWorld) {
    let integration = IntegrationParameters {
        dt: PROVING_DT,
        ..Default::default()
    };
    let PhysicsWorld {
        pipeline,
        islands,
        broad_phase,
        narrow_phase,
        bodies,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        ccd_solver,
        ..
    } = physics;
    pipeline.step(
        to_rapier_vec3(Vec3::new(0.0, -PROVING_GRAVITY, 0.0)),
        &integration,
        islands,
        broad_phase,
        narrow_phase,
        bodies,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        ccd_solver,
        &(),
        &(),
    );
}

/// The height at which the chassis' center puts the lowest wheel on the plane at rest length.
#[must_use]
pub fn spawn_height(config: &VehicleConfig) -> f32 {
    config
        .wheels
        .iter()
        .map(|wheel| wheel.suspension_rest_length + wheel.radius - wheel.position.y)
        .fold(0.0_f32, f32::max)
}

/// Runs `script` for `seconds` with `config` on a fresh proving ground.
///
/// # Errors
/// The [`VehicleError`] of [`Vehicle::spawn`] or of an update; [`VehicleError::InvalidTimeStep`]
/// when `seconds` is not in (0, [`MAX_PROVING_SECONDS`]].
pub fn run_proving(
    config: &VehicleConfig,
    script: ProvingScript,
    seconds: f32,
) -> Result<ProvingRun, VehicleError> {
    run_proving_retuned(config, script, seconds, None)
}

/// [`run_proving`] with an optional [`ProvingRetune`] partway through.
///
/// # Errors
/// As [`run_proving`], and the [`VehicleError`] the retune's [`Vehicle::set_config`] returns.
pub fn run_proving_retuned(
    config: &VehicleConfig,
    script: ProvingScript,
    seconds: f32,
    retune: Option<&ProvingRetune>,
) -> Result<ProvingRun, VehicleError> {
    run_inner(config, script, seconds, retune, None)
}

/// The trailer's step on a course run: after the car's update, before the world steps.
fn update_trailer(
    course: &mut Option<&mut CourseState>,
    physics: &mut PhysicsWorld,
    brake: f32,
) -> Result<(), VehicleError> {
    if let Some(trailer) = course.as_mut().and_then(|c| c.trailer.as_mut()) {
        trailer
            .update(physics, brake, PROVING_DT, None)
            .map_err(|e| trailer_error(&e))?;
    }
    Ok(())
}

/// A trailer problem as the car's error.
fn trailer_error(error: &TrailerError) -> VehicleError {
    match error {
        TrailerError::Chassis(e) => e.clone(),
        _ => VehicleError::InvalidConfig { field: "trailer" },
    }
}

/// [`run_proving_retuned`], and [`run_proving_course`] when `course` is given.
fn run_inner(
    config: &VehicleConfig,
    script: ProvingScript,
    seconds: f32,
    retune: Option<&ProvingRetune>,
    mut course: Option<&mut CourseState>,
) -> Result<ProvingRun, VehicleError> {
    let _span = debug_span!("vehicle_proving_run", script = script.name()).entered();
    if !(seconds.is_finite() && seconds > 0.0 && seconds <= MAX_PROVING_SECONDS) {
        return Err(VehicleError::InvalidTimeStep);
    }
    let mut physics = PhysicsWorld::default();
    match course.as_deref() {
        Some(c) => {
            add_proving_track(&mut physics, c.track);
        }
        None => add_proving_ground(&mut physics),
    }
    let height = spawn_height(config) + 0.02;
    let mut vehicle = Vehicle::spawn(
        &mut physics,
        config.clone(),
        Vec3::new(0.0, height, 0.0),
        Quat::IDENTITY,
    )?;
    if let Some(c) = course.as_deref_mut()
        && let Some(trailer) = c.trailer_config.clone()
    {
        let chassis_mass = trailer.chassis.chassis_mass;
        let spawned = Trailer::spawn_coupled(
            &mut physics,
            trailer,
            vehicle.chassis(),
            proving_tow_hitch(config),
        )
        .map_err(|e| trailer_error(&e))?;
        c.trailer = Some(spawned);
        c.trailer_run = Some(ProvingTrailerRun {
            chassis_mass,
            ..ProvingTrailerRun::default()
        });
    }
    let settle = (SETTLE_SECONDS / PROVING_DT).round() as u32;
    for _ in 0..settle {
        vehicle.update(&mut physics, VehicleInput::default(), PROVING_DT)?;
        update_trailer(&mut course, &mut physics, 0.0)?;
        step_proving(&mut physics);
    }
    let start = vehicle.state(&physics)?.translation;
    if let Some(c) = course.as_deref_mut() {
        c.start_y = start.y;
    }
    let steps = (seconds / PROVING_DT).round().max(1.0) as u32;
    let sample_every = (60 / SAMPLES_PER_SECOND).max(1);
    let upshifts_before = vehicle.drivetrain().map_or(0, |d| d.upshifts());
    let mut run = ProvingRun {
        script,
        seconds,
        zero_to_100_s: None,
        top_speed_kmh: 0.0,
        max_lateral_g: 0.0,
        distance_m: 0.0,
        upshifts: 0,
        samples: Vec::new(),
        digest: 0,
    };
    let mut lateral = [0.0_f32; LATERAL_WINDOW];
    let mut previous_speed = vehicle.state(&physics)?.forward_speed;
    for step in 0..steps {
        if let Some(retune) = retune.filter(|r| r.step == step) {
            vehicle.set_config(&mut physics, retune.config.clone())?;
        }
        let time = step as f32 * PROVING_DT;
        let input = script.input(time, previous_speed);
        vehicle.update(&mut physics, input, PROVING_DT)?;
        update_trailer(&mut course, &mut physics, input.brake)?;
        step_proving(&mut physics);
        let state = vehicle.state(&physics)?;
        if let Some(c) = course.as_deref_mut() {
            c.observe(&physics, &state)?;
        }
        let speed = state.forward_speed;
        if run.zero_to_100_s.is_none() && speed >= KMH_100 {
            let share = if speed > previous_speed {
                ((KMH_100 - previous_speed) / (speed - previous_speed)).clamp(0.0, 1.0)
            } else {
                1.0
            };
            run.zero_to_100_s = Some(time + share * PROVING_DT);
        }
        run.top_speed_kmh = run.top_speed_kmh.max(speed * 3.6);
        let up = state.rotation * Vec3::Y;
        let lateral_g = (state.angular_velocity.dot(up) * speed).abs() / PROVING_GRAVITY;
        let slot = step as usize % LATERAL_WINDOW;
        lateral[slot] = lateral_g;
        if step as usize + 1 >= LATERAL_WINDOW {
            let mean = lateral.iter().sum::<f32>() / LATERAL_WINDOW as f32;
            run.max_lateral_g = run.max_lateral_g.max(mean);
        }
        if (step + 1).is_multiple_of(sample_every) {
            run.samples.push(ProvingSample {
                time: (step + 1) as f32 * PROVING_DT,
                speed_kmh: speed * 3.6,
                rpm: state.rpm,
                gear: state.gear,
                lateral_g,
                wheels: state
                    .wheels
                    .iter()
                    .map(|wheel| ProvingWheel {
                        slip_ratio: wheel.slip_ratio,
                        slip_angle: wheel.slip_angle,
                        load: wheel.suspension_force,
                        compression: wheel.compression,
                    })
                    .collect(),
            });
        }
        previous_speed = speed;
    }
    let end = vehicle.state(&physics)?.translation;
    let flat = |v: Vec3| Vec3::new(v.x, 0.0, v.z);
    run.distance_m = (flat(end) - flat(start)).length();
    run.upshifts = vehicle
        .drivetrain()
        .map_or(0, |d| d.upshifts())
        .saturating_sub(upshifts_before);
    run.digest = physics.state_digest() ^ vehicle.state_digest().rotate_left(17);
    if let Some(trailer) = course.as_deref().and_then(|c| c.trailer.as_ref()) {
        run.digest ^= trailer.state_digest().rotate_left(31);
    }
    Ok(run)
}

impl CourseState {
    /// Counts the car's grounded wheels and reads the trailer after one script step.
    fn observe(
        &mut self,
        physics: &PhysicsWorld,
        state: &crate::vehicle::VehicleState,
    ) -> Result<(), VehicleError> {
        let contacts = state.wheels.iter().filter(|wheel| wheel.grounded).count() as u32;
        self.min_contacts = Some(self.min_contacts.map_or(contacts, |m| m.min(contacts)));
        self.max_contacts = self.max_contacts.max(contacts);
        if self.last_contacts.is_some_and(|last| last != contacts) {
            self.contact_changes += 1;
        }
        self.last_contacts = Some(contacts);
        self.max_rise = self.max_rise.max(state.translation.y - self.start_y);
        if let (Some(trailer), Some(out)) = (self.trailer.as_ref(), self.trailer_run.as_mut()) {
            let readout = trailer.readout(physics).map_err(|e| trailer_error(&e))?;
            out.coupled = readout.coupled;
            out.max_articulation = out.max_articulation.max(readout.articulation_angle.abs());
            out.max_coupling_gap = out.max_coupling_gap.max(readout.coupling_gap);
        }
        Ok(())
    }
}
