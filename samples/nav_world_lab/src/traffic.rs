// SPDX-License-Identifier: MIT-0
//! Traffic (ADR-3117): four AI cars drive physics vehicles round a closed oval of lanes.
//!
//! [`build_traffic_app`] makes the app (physics, vehicles, `VehicleAiPlugin`, a flat ground, the
//! oval as the `NavLaneGraph` resource) with four default cars spread over the lap, each driving
//! `DriveTarget::Loop`. [`run_traffic`] steps it a fixed number of 60 Hz steps and reports laps,
//! the worst cross-track error, the closest bumper gap and a digest of every chassis position.

use std::fmt;
use std::time::Duration;

use regia_app::prelude::*;
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3, det_atan2, det_quat_from_axis_angle, det_sin_cos};
use regia_framework::vehicle_ai::{DriveTarget, VehicleAiPlugin, VehicleDriver};
use regia_nav::{NavLaneDef, NavLaneGraph};
use regia_physics::vehicle::{VehicleConfig, VehicleController, VehiclePlugin, spawn_vehicle};
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, RigidBody};
use regia_scene::Transform;

use crate::LabError;

/// Cars on the oval.
pub const TRAFFIC_CARS: usize = 4;

/// Radius of the oval's ends, metres.
pub const OVAL_RADIUS: f32 = 25.0;

/// Length of the oval's straights, metres.
pub const OVAL_STRAIGHT: f32 = 80.0;

/// The closed oval: lane 0 from (0, 0) to (80, 0) along +X, a half circle, lane 2 back along −X at
/// z = −50, a half circle home. Lanes 0 → 1 → 2 → 3 → 0, 3.5 m wide, 15 m/s, user ids 1 to 4.
pub fn traffic_oval() -> Result<NavLaneGraph, LabError> {
    let (r, len) = (OVAL_RADIUS, OVAL_STRAIGHT);
    let lane = |points: &[(f32, f32)]| {
        let p: Vec<Vec3> = points.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect();
        let mut def = NavLaneDef::new(&p);
        def.speed = 15.0;
        def.width = 3.5;
        def
    };
    let arc = |cx: f32, cz: f32, sign: f32| arc_points(cx, cz, sign, r);
    let mut lanes = vec![
        lane(&[(0.0, 0.0), (len, 0.0)]),
        lane(&arc(len, -r, 1.0)),
        lane(&[(len, -2.0 * r), (0.0, -2.0 * r)]),
        lane(&arc(0.0, -r, -1.0)),
    ];
    for (i, l) in lanes.iter_mut().enumerate() {
        l.next = vec![((i + 1) % 4) as u32];
        l.user_id = i as u64 + 1;
    }
    NavLaneGraph::build(&lanes, 0.0).map_err(|e| LabError(format!("oval: {e}")))
}

/// The 25 points of a half circle (deterministic kernels, ADR-5128) of radius `r` about (`cx`, `cz`), from (`cx`, `cz + sign·r`)
/// round to (`cx`, `cz − sign·r`).
fn arc_points(cx: f32, cz: f32, sign: f32, r: f32) -> Vec<(f32, f32)> {
    (0..=24)
        .map(|i| {
            let t = std::f32::consts::PI * i as f32 / 24.0;
            let (sin, cos) = det_sin_cos(t);
            (cx + sign * r * sin, cz + sign * r * cos)
        })
        .collect()
}

/// The spawn rotation of a car heading along `dir` (the chassis faces −Z).
fn car_yaw(dir: Vec3) -> Quat {
    det_quat_from_axis_angle(Vec3::Y, det_atan2(-dir.x, -dir.z))
}

/// The traffic app. `prepare` runs first on the empty app (tests add `MinimalPlugins` and the agent
/// there). Returns the app and the four car entities in spawn order.
pub fn build_traffic_app(prepare: impl FnOnce(&mut App)) -> Result<(App, Vec<Entity>), LabError> {
    let mut app = App::new();
    prepare(&mut app);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, VehiclePlugin, VehicleAiPlugin));
    app.world_mut().spawn((
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(200.0, 0.5, 200.0),
            },
            friction: 1.0,
            ..Collider::default()
        },
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
    ));
    let graph = traffic_oval()?;
    let lap = graph.total_length();
    let config = VehicleConfig::default();
    let wheel = &config.wheels[0];
    // Chassis height at rest: wheel mount, rest length less the static compression, radius.
    let compression = 9.81 / (config.wheels.len() as f32 * wheel.suspension_stiffness);
    let height = -wheel.position.y + wheel.suspension_rest_length - compression + wheel.radius + 0.02;
    let mut cars = Vec::with_capacity(TRAFFIC_CARS);
    for k in 0..TRAFFIC_CARS {
        let mut along = k as f32 * lap / TRAFFIC_CARS as f32;
        let mut index = 0u32;
        while let Some(l) = graph.lane(index) {
            if along <= l.length() {
                break;
            }
            along -= l.length();
            index += 1;
        }
        let (p, dir) = graph
            .lane(index)
            .map(|l| l.sample(along))
            .ok_or_else(|| LabError("oval lane".to_owned()))?;
        let yaw = car_yaw(dir);
        let car = spawn_vehicle(app.world_mut(), config.clone(), Vec3::new(p.x, height, p.z), yaw)
            .map_err(|e| LabError(format!("car: {e}")))?;
        app.world_mut()
            .entity_mut(car)
            .insert(VehicleDriver::to(DriveTarget::Loop { lane: index }));
        cars.push(car);
    }
    app.insert_resource(graph);
    Ok((app, cars))
}

/// What the traffic run saw.
#[derive(Clone, Debug, PartialEq)]
pub struct TrafficReport {
    /// Laps each car completed.
    pub laps: [u32; TRAFFIC_CARS],
    /// The largest cross-track error any car had while driving, metres.
    pub max_cross_track: f32,
    /// The closest bumper-to-bumper gap any car saw to its leader, metres.
    pub closest_gap: f32,
    /// Digest over every chassis position at every step.
    pub digest: u64,
}

impl fmt::Display for TrafficReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "traffic: laps {:?}, max cross-track {:.3} m, closest gap {:.2} m, digest {:016x}",
            self.laps, self.max_cross_track, self.closest_gap, self.digest
        )
    }
}

/// Drives the four cars for `steps` fixed steps.
///
/// # Errors
/// A build the lab's fixed inputs should never fail.
pub fn run_traffic(steps: u32) -> Result<TrafficReport, LabError> {
    run_traffic_traced(steps, &mut |_, _, _| {})
}

/// [`run_traffic`], reporting the running digest after every step to `trace` as section
/// `traffic`, plus the physics world's `state_digest` as section `traffic_physics` (see
/// [`crate::DigestTrace`]).
///
/// # Errors
/// As [`run_traffic`].
pub fn run_traffic_traced(steps: u32, trace: &mut crate::DigestTrace<'_>) -> Result<TrafficReport, LabError> {
    let (mut app, cars) = build_traffic_app(|_| {})?;
    let mut digest = 0xcbf2_9ce4_8422_2325u64;
    let mut max_cross_track = 0.0f32;
    let mut closest_gap = f32::INFINITY;
    for step in 0..steps {
        app.update();
        let world = app.world();
        let physics = world.resource::<PhysicsWorld>();
        trace("traffic_physics", step, physics.state_digest());
        for &car in &cars {
            let Some(controller) = world.get::<VehicleController>(car) else {
                continue;
            };
            if let Ok(state) = controller.vehicle.state(physics) {
                for v in [state.translation.x, state.translation.y, state.translation.z] {
                    for b in v.to_bits().to_le_bytes() {
                        digest ^= u64::from(b);
                        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
                    }
                }
            }
            if let Some(driver) = world.get::<VehicleDriver>(car) {
                let status = driver.status();
                max_cross_track = max_cross_track.max(status.cross_track.abs());
                if let Some(gap) = status.gap {
                    closest_gap = closest_gap.min(gap);
                }
            }
        }
        trace("traffic", step, digest);
    }
    let mut laps = [0u32; TRAFFIC_CARS];
    for (slot, &car) in laps.iter_mut().zip(&cars) {
        *slot = app
            .world()
            .get::<VehicleDriver>(car)
            .map_or(0, |d| d.status().laps);
    }
    Ok(TrafficReport {
        laps,
        max_cross_track,
        closest_gap,
        digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADR-5128: the oval and the spawn headings are level setup every host must build to the same
    // bits, so they come from the deterministic kernels; a platform `sin`/`cos`/`atan2` that rounds
    // differently (another OS, or the 1-ulp LD_PRELOAD shim on Linux) fails these checks.

    #[test]
    fn the_oval_arcs_use_the_deterministic_kernels() {
        let points = arc_points(OVAL_STRAIGHT, -OVAL_RADIUS, 1.0, OVAL_RADIUS);
        assert_eq!(points.len(), 25);
        for (i, &(x, z)) in points.iter().enumerate() {
            let t = std::f32::consts::PI * i as f32 / 24.0;
            let (sin, cos) = det_sin_cos(t);
            assert_eq!(
                x.to_bits(),
                (OVAL_STRAIGHT + OVAL_RADIUS * sin).to_bits(),
                "x at {i}"
            );
            assert_eq!(
                z.to_bits(),
                (-OVAL_RADIUS + OVAL_RADIUS * cos).to_bits(),
                "z at {i}"
            );
        }
    }

    #[test]
    fn spawn_headings_use_the_deterministic_kernels() {
        for i in 0..32 {
            let a = i as f32 * 0.2;
            let dir = Vec3::new(a.sin(), 0.0, a.cos());
            let want = det_quat_from_axis_angle(Vec3::Y, det_atan2(-dir.x, -dir.z));
            let got = car_yaw(dir);
            assert_eq!(
                got.to_array().map(f32::to_bits),
                want.to_array().map(f32::to_bits),
                "{dir}"
            );
        }
    }
}
