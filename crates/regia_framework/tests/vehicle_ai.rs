// SPDX-License-Identifier: MPL-2.0
//! ADR-3117 acceptance: AI drivers drive physical vehicles along lane graphs.
//!
//! Every test builds an app with `PhysicsPlugin`, `VehiclePlugin` and `VehicleAiPlugin` on a flat
//! static ground, steps it a fixed number of 60 Hz steps with deterministic inputs (no wall clock)
//! and reads each driver's status. The controller tests check thresholds only; the trajectory hash
//! is pinned in its own test (`vehicle_ai_trajectory_pin`), which moves whenever the vehicle physics
//! changes (the lead re-pins it).

use std::time::Duration;

use regia_app::prelude::*;
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_framework::vehicle_ai::{
    DriveTarget, VehicleAiPlugin, VehicleDriver, VehicleDriverParams, VehicleDriverState,
};
use regia_nav::{NavLaneDef, NavLaneGraph};
use regia_physics::vehicle::{VehicleConfig, VehicleController, VehicleInput, VehiclePlugin, spawn_vehicle};
use regia_physics::{BodyKind, Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, RigidBody};
use regia_scene::Transform;

/// Chassis height at rest for the default car (wheel mount −0.2, rest 0.35 less the static
/// compression g / (4 · 30), radius 0.35), plus 2 cm.
const SPAWN_HEIGHT: f32 = 0.2 + 0.35 - 9.81 / 120.0 + 0.35 + 0.02;

fn app(with_ai: bool) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, VehiclePlugin));
    if with_ai {
        app.add_plugins(VehicleAiPlugin);
    }
    static_box(&mut app, Vec3::new(0.0, -0.5, 0.0), Vec3::new(600.0, 0.5, 600.0));
    app
}

fn static_box(app: &mut App, center: Vec3, half_extents: Vec3) {
    app.world_mut().spawn((
        RigidBody {
            kind: BodyKind::Static,
            ..RigidBody::static_body()
        },
        Collider {
            shape: ColliderShape::Box { half_extents },
            friction: 1.0,
            ..Collider::default()
        },
        Transform {
            translation: center,
            ..Transform::IDENTITY
        },
    ));
}

/// The yaw that points the chassis' forward (−Z) along `dir`.
fn facing(dir: Vec3) -> Quat {
    Quat::from_rotation_y((-dir.x).atan2(-dir.z))
}

fn car(app: &mut App, at: Vec3, dir: Vec3) -> Entity {
    spawn_vehicle(
        app.world_mut(),
        VehicleConfig::default(),
        Vec3::new(at.x, SPAWN_HEIGHT, at.z),
        facing(dir),
    )
    .expect("spawn_vehicle")
}

fn lane(points: &[(f32, f32)], speed: f32) -> NavLaneDef {
    let p: Vec<Vec3> = points.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect();
    let mut def = NavLaneDef::new(&p);
    def.speed = speed;
    def.width = 3.5;
    def
}

/// A closed oval: two 80 m straights and two 25 m radius half circles, lanes 0 → 1 → 2 → 3 → 0.
/// Lane 0 runs from (0, 0) to (80, 0) along +X.
fn oval(radius: f32, straight: f32, speed: f32) -> NavLaneGraph {
    let arc = |cx: f32, cz: f32, sign: f32| {
        (0..=24)
            .map(|i| {
                let t = std::f32::consts::PI * i as f32 / 24.0;
                (cx + sign * radius * t.sin(), cz + sign * radius * t.cos())
            })
            .collect::<Vec<_>>()
    };
    let mut lanes = vec![
        lane(&[(0.0, 0.0), (straight, 0.0)], speed),
        lane(&arc(straight, -radius, 1.0), speed),
        lane(&[(straight, -2.0 * radius), (0.0, -2.0 * radius)], speed),
        lane(&arc(0.0, -radius, -1.0), speed),
    ];
    for (i, l) in lanes.iter_mut().enumerate() {
        l.next = vec![((i + 1) % 4) as u32];
        l.user_id = i as u64 + 1;
    }
    NavLaneGraph::build(&lanes, 0.0).expect("oval")
}

fn chassis_position(app: &App, entity: Entity) -> Vec3 {
    let controller = app.world().get::<VehicleController>(entity).expect("controller");
    let physics = app.world().resource::<PhysicsWorld>();
    controller
        .vehicle
        .state(physics)
        .map(|s| s.translation)
        .unwrap_or(Vec3::NAN)
}

fn driver(app: &App, entity: Entity) -> &VehicleDriver {
    app.world().get::<VehicleDriver>(entity).expect("driver")
}

fn hash_positions(hash: &mut u64, app: &App, entities: &[Entity]) {
    for &e in entities {
        let p = chassis_position(app, e);
        for v in [p.x, p.y, p.z] {
            for b in v.to_bits().to_le_bytes() {
                *hash ^= u64::from(b);
                *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
}

/// Laps: drive once around the oval from 5 m into lane 0 back to 2 m into it. Returns the trajectory
/// hash, the cross-track RMS and max, the worst speed / curve-speed ratio, the stop error and the
/// steps taken.
fn lap() -> (u64, f32, f32, f32, f32, usize) {
    let mut app = app(true);
    app.insert_resource(oval(25.0, 80.0, 15.0));
    let e = car(&mut app, Vec3::new(5.0, 0.0, 0.0), Vec3::X);
    app.world_mut()
        .entity_mut(e)
        .insert(VehicleDriver::to(DriveTarget::Lane {
            lane: 0,
            distance: 2.0,
        }));
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let (mut sum2, mut n, mut max_ct, mut worst_ratio) = (0.0f64, 0usize, 0.0f32, 0.0f32);
    let mut steps = 0;
    let mut worst_at = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for step in 0..4000 {
        app.update();
        hash_positions(&mut hash, &app, &[e]);
        let status = driver(&app, e).status().clone();
        assert!(status.last_error.is_none(), "{:?}", status.last_error);
        if matches!(
            status.state,
            VehicleDriverState::Driving | VehicleDriverState::Following
        ) {
            sum2 += f64::from(status.cross_track * status.cross_track);
            n += 1;
            max_ct = max_ct.max(status.cross_track.abs());
            if status.speed > 2.0 && status.speed / status.curve_speed > worst_ratio {
                worst_ratio = status.speed / status.curve_speed;
                worst_at = (
                    status.station,
                    status.speed,
                    status.curve_speed,
                    status.target_speed,
                );
            }
        }
        if status.state == VehicleDriverState::Arrived {
            steps = step + 1;
            break;
        }
    }
    assert!(steps > 0, "the car arrived ({:?})", driver(&app, e).status());
    assert_eq!(driver(&app, e).route(), &[0, 1, 2, 3, 0]);
    let end = *driver(&app, e).path().last().expect("path");
    let p = chassis_position(&app, e);
    let stop_error = Vec3::new(p.x - end.x, 0.0, p.z - end.z).length();
    let rms = (sum2 / n.max(1) as f64).sqrt() as f32;
    println!("worst speed/curve at (station, speed, curve, target) {worst_at:?}");
    (hash, rms, max_ct, worst_ratio, stop_error, steps)
}

#[test]
fn a_default_car_laps_the_oval_on_line_under_the_curve_speed_and_stops_at_the_end() {
    let (_, rms, max_ct, worst_ratio, stop_error, steps) = lap();
    println!(
        "lap: {steps} steps, cross-track rms {rms:.3} m max {max_ct:.3} m, speed/curve {worst_ratio:.3}, stop {stop_error:.3} m"
    );
    assert!(rms <= 0.5, "cross-track RMS {rms} m");
    assert!(max_ct <= 1.5, "cross-track max {max_ct} m");
    assert!(worst_ratio <= 1.05, "speed over the curve speed by {worst_ratio}");
    assert!(stop_error <= 1.0, "stopped {stop_error} m from the route end");
}

#[test]
fn two_runs_give_identical_trajectories() {
    let first = lap();
    let second = lap();
    assert_eq!(first.0, second.0, "trajectory hashes differ");
    assert_eq!(first.5, second.5);
}

/// The trajectory pin (ADR-3117). It moves whenever the vehicle physics, the driver or the schedule
/// changes; re-pin it with the value this test prints, and say why in the commit.
#[test]
fn vehicle_ai_trajectory_pin() {
    let (hash, .., steps) = lap();
    println!("vehicle_ai trajectory pin: hash {hash:#018x}, {steps} steps");
    // Pinned 2026-11-06 on x86_64 Linux (lane vehicle-ai-lanes): 1898 steps. The track and the
    // car are built with the platform's `sin`/`cos`/`atan2` and run through the default (per
    // instruction set, ADR-5176) physics build, so the hash is one value per platform: macOS on
    // Apple silicon measured the same value on the M5 at integ-v047 and integ-v048, also 1898
    // steps (ADR-5550, ADR-5860).
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    const PIN: Option<u64> = Some(0xad7f_2d98_41bd_08b7);
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    const PIN: Option<u64> = Some(0x3f31_d79e_4717_af00);
    if let Some(pin) = PIN {
        assert_eq!(hash, pin, "trajectory pin moved: {hash:#018x}");
    }
}

#[test]
fn a_follower_keeps_its_gap_when_the_leader_brakes_at_its_limit() {
    let mut app = app(true);
    app.insert_resource(NavLaneGraph::build(&[lane(&[(0.0, 0.0), (0.0, -900.0)], 20.0)], 0.0).expect("road"));
    let leader = car(&mut app, Vec3::new(0.0, 0.0, -40.0), Vec3::NEG_Z);
    let follower = car(&mut app, Vec3::new(0.0, 0.0, -5.0), Vec3::NEG_Z);
    for e in [leader, follower] {
        app.world_mut()
            .entity_mut(e)
            .insert(VehicleDriver::to(DriveTarget::Lane {
                lane: 0,
                distance: 880.0,
            }));
    }
    let collider = |app: &App, e: Entity| {
        app.world()
            .get::<VehicleController>(e)
            .and_then(|c| c.vehicle.chassis_collider())
            .expect("collider")
            .to_handle()
    };
    let (a, b) = (collider(&app, leader), collider(&app, follower));
    let min_gap = VehicleDriverParams::default().min_gap;
    let (mut contacts, mut min_seen, mut braked_at) = (0usize, f32::INFINITY, None);
    for step in 0..2400 {
        // Cruise until both are at the limit, then the leader stops with a full brake.
        if braked_at.is_none() && step > 600 {
            let speed = driver(&app, leader).status().speed;
            if speed > 18.0 {
                braked_at = Some(step);
                app.world_mut().entity_mut(leader).remove::<VehicleDriver>();
            }
        }
        if braked_at.is_some() {
            *app.world_mut().get_mut::<VehicleInput>(leader).expect("input") = VehicleInput {
                throttle: 0.0,
                brake: 1.0,
                steer: 0.0,
            };
        }
        app.update();
        let physics = app.world().resource::<PhysicsWorld>();
        if physics
            .narrow_phase
            .contact_pairs_with(a)
            .any(|p| (p.collider1 == b || p.collider2 == b) && p.has_any_active_contact())
        {
            contacts += 1;
        }
        if let Some(gap) = driver(&app, follower).status().gap {
            min_seen = min_seen.min(gap);
        }
    }
    let status = driver(&app, follower).status().clone();
    println!("platoon: braked at {braked_at:?}, min gap {min_seen:.2} m, final {status:?}");
    assert!(braked_at.is_some(), "the platoon reached speed");
    assert_eq!(contacts, 0, "the chassis touched");
    assert!(
        min_seen > min_gap,
        "gap {min_seen} m under the IDM minimum {min_gap} m"
    );
    assert!(status.speed.abs() < 0.5, "the follower stopped behind the leader");
}

#[test]
fn a_car_against_a_wall_recovers_and_finishes_its_route() {
    let mut app = app(true);
    app.insert_resource(NavLaneGraph::build(&[lane(&[(0.0, 0.0), (0.0, -80.0)], 12.0)], 0.0).expect("road"));
    // The car faces +X with its nose 0.4 m from a wall; the lane runs along −Z.
    static_box(
        &mut app,
        Vec3::new(2.1 + 0.4 + 0.25, 1.0, 0.0),
        Vec3::new(0.25, 1.0, 12.0),
    );
    let e = car(&mut app, Vec3::ZERO, Vec3::X);
    app.world_mut()
        .entity_mut(e)
        .insert(VehicleDriver::to(DriveTarget::Lane {
            lane: 0,
            distance: 60.0,
        }));
    let mut arrived = false;
    for _ in 0..6000 {
        app.update();
        let state = driver(&app, e).state();
        assert_ne!(state, VehicleDriverState::Stuck, "{:?}", driver(&app, e).status());
        if state == VehicleDriverState::Arrived {
            arrived = true;
            break;
        }
    }
    let status = driver(&app, e).status().clone();
    println!("wall: {status:?}");
    assert!(arrived, "the car finished its route");
    assert!(status.recoveries >= 1, "it needed a recovery");
}

#[test]
fn a_car_boxed_in_gives_up_after_its_recoveries() {
    let mut app = app(true);
    app.insert_resource(NavLaneGraph::build(&[lane(&[(0.0, 0.0), (0.0, -80.0)], 12.0)], 0.0).expect("road"));
    // Walls ahead and behind, 0.4 m from each bumper.
    for z in [-(2.1 + 0.4 + 0.25), 2.1 + 0.4 + 0.25] {
        static_box(&mut app, Vec3::new(0.0, 1.0, z), Vec3::new(3.0, 1.0, 0.25));
    }
    let e = car(&mut app, Vec3::ZERO, Vec3::NEG_Z);
    let params = VehicleDriverParams {
        max_recoveries: 2,
        ..VehicleDriverParams::default()
    };
    let mut d = VehicleDriver::new(params);
    d.drive_to(DriveTarget::Lane {
        lane: 0,
        distance: 60.0,
    });
    app.world_mut().entity_mut(e).insert(d);
    for _ in 0..3000 {
        app.update();
    }
    let status = driver(&app, e).status().clone();
    println!("boxed in: {status:?}");
    assert_eq!(status.state, VehicleDriverState::Stuck);
    assert_eq!(status.failed_recoveries, 2);
    assert_eq!(status.input.brake, 1.0, "a stuck driver holds the brake");
}

#[test]
fn a_disabled_lane_stops_the_car_until_it_reopens() {
    let mut app = app(true);
    let mut first = lane(&[(0.0, 0.0), (0.0, -100.0)], 12.0);
    first.next = vec![1];
    let mut second = lane(&[(0.0, -100.0), (0.0, -200.0)], 12.0);
    second.user_id = 7;
    let mut graph = NavLaneGraph::build(&[first, second], 0.0).expect("road");
    let e = car(&mut app, Vec3::new(0.0, 0.0, -5.0), Vec3::NEG_Z);
    // Lane 1 closes after the route was planned through it: the driver must stop before it.
    graph.set_lane_enabled(7, false);
    app.insert_resource(graph);
    let mut d = VehicleDriver::to(DriveTarget::Lane {
        lane: 1,
        distance: 50.0,
    });
    let open = {
        let mut g = app.world().resource::<NavLaneGraph>().clone();
        g.set_lane_enabled(7, true);
        g
    };
    d.plan(&open, Vec3::new(0.0, 0.0, -5.0), 0.0)
        .expect("plan on the open graph");
    app.world_mut().entity_mut(e).insert(d);
    for _ in 0..1500 {
        app.update();
    }
    let status = driver(&app, e).status().clone();
    let z = chassis_position(&app, e).z;
    println!("blocked: z {z:.2}, {status:?}");
    assert_eq!(status.state, VehicleDriverState::Blocked);
    assert!(
        z > -100.0 + 2.1 && z < -100.0 + 2.1 + 6.0,
        "stopped in front of the closed lane: z {z}"
    );
    app.world_mut()
        .resource_mut::<NavLaneGraph>()
        .set_lane_enabled(7, true);
    let mut arrived = false;
    for _ in 0..2000 {
        app.update();
        if driver(&app, e).state() == VehicleDriverState::Arrived {
            arrived = true;
            break;
        }
    }
    let z = chassis_position(&app, e).z;
    assert!(
        arrived,
        "drove on after the lane reopened ({:?})",
        driver(&app, e).status()
    );
    assert!((z - -150.0).abs() <= 1.0, "stopped at the route end: z {z}");
}

#[test]
fn errors_and_stop_leave_the_car_braked() {
    let mut app = app(true);
    let e = car(&mut app, Vec3::ZERO, Vec3::NEG_Z);
    // No lane graph: the driver says so and holds the brake.
    app.world_mut()
        .entity_mut(e)
        .insert(VehicleDriver::to(DriveTarget::Loop { lane: 0 }));
    for _ in 0..3 {
        app.update();
    }
    let status = driver(&app, e).status().clone();
    assert_eq!(status.state, VehicleDriverState::Idle);
    assert_eq!(
        status.last_error.as_ref().map(|e| e.code()),
        Some("vehicle_ai_no_lane_graph")
    );
    assert_eq!(
        app.world().get::<VehicleInput>(e).copied().map(|i| i.brake),
        Some(1.0)
    );
    // With the oval it drives; `stop` brakes it to a halt.
    app.insert_resource(oval(25.0, 80.0, 15.0));
    app.world_mut()
        .entity_mut(e)
        .insert(VehicleDriver::to(DriveTarget::Point {
            point: Vec3::new(80.0, 0.0, -50.0),
            reach: 5.0,
        }));
    for _ in 0..240 {
        app.update();
    }
    assert!(
        driver(&app, e).status().speed > 3.0,
        "{:?}",
        driver(&app, e).status()
    );
    app.world_mut()
        .get_mut::<VehicleDriver>(e)
        .expect("driver")
        .stop();
    for _ in 0..300 {
        app.update();
    }
    let status = driver(&app, e).status().clone();
    assert_eq!(status.state, VehicleDriverState::Idle);
    assert!(status.speed.abs() < 0.2, "{status:?}");
}

/// Spec § 2 (`VehicleAiError` codes): a driver whose chassis left the physics world records
/// `vehicle_ai_no_chassis`, and the other drivers keep driving.
#[test]
fn a_driver_without_a_chassis_reports_it() {
    let mut app = app(true);
    app.insert_resource(oval(25.0, 80.0, 15.0));
    let gone = car(&mut app, Vec3::new(5.0, 0.0, 0.0), Vec3::X);
    let other = car(&mut app, Vec3::new(75.0, 0.0, -50.0), Vec3::NEG_X);
    for e in [gone, other] {
        app.world_mut()
            .entity_mut(e)
            .insert(VehicleDriver::to(DriveTarget::Loop { lane: 0 }));
    }
    for _ in 0..30 {
        app.update();
    }
    assert!(driver(&app, gone).status().last_error.is_none());
    let chassis = app
        .world()
        .get::<VehicleController>(gone)
        .expect("controller")
        .vehicle
        .chassis()
        .to_handle();
    app.world_mut()
        .resource_mut::<PhysicsWorld>()
        .remove_body(chassis);
    for _ in 0..30 {
        app.update();
    }
    assert_eq!(
        driver(&app, gone).status().last_error.as_ref().map(|e| e.code()),
        Some("vehicle_ai_no_chassis")
    );
    let status = driver(&app, other).status().clone();
    assert!(status.last_error.is_none(), "{status:?}");
    assert!(
        matches!(
            status.state,
            VehicleDriverState::Driving | VehicleDriverState::Following
        ),
        "{status:?}"
    );
    assert!(status.steps >= 50, "the other car kept driving: {status:?}");
}

#[test]
fn four_cars_loop_the_oval_without_touching() {
    let mut app = app(true);
    let graph = oval(25.0, 80.0, 15.0);
    let lap = graph.total_length();
    let mut cars = Vec::new();
    for k in 0..4 {
        // Evenly spaced over the lap, each on its lane facing along it.
        let mut along = k as f32 * lap / 4.0;
        let mut index = 0u32;
        while let Some(l) = graph.lane(index) {
            if along <= l.length() {
                break;
            }
            along -= l.length();
            index += 1;
        }
        let (p, dir) = graph.lane(index).expect("lane").sample(along);
        let e = car(&mut app, p, dir);
        app.world_mut()
            .entity_mut(e)
            .insert(VehicleDriver::to(DriveTarget::Loop { lane: index }));
        cars.push(e);
    }
    app.insert_resource(graph);
    let colliders: Vec<_> = cars
        .iter()
        .map(|&e| {
            app.world()
                .get::<VehicleController>(e)
                .and_then(|c| c.vehicle.chassis_collider())
                .expect("collider")
                .to_handle()
        })
        .collect();
    let mut contacts = 0;
    for _ in 0..3000 {
        app.update();
        let physics = app.world().resource::<PhysicsWorld>();
        for (i, &a) in colliders.iter().enumerate() {
            for &b in &colliders[i + 1..] {
                if physics
                    .narrow_phase
                    .contact_pairs_with(a)
                    .any(|p| (p.collider1 == b || p.collider2 == b) && p.has_any_active_contact())
                {
                    contacts += 1;
                }
            }
        }
    }
    for &e in &cars {
        let status = driver(&app, e).status();
        assert!(status.laps >= 1, "every car lapped: {status:?}");
        assert!(status.cross_track.abs() < 1.5, "{status:?}");
    }
    assert_eq!(contacts, 0, "no chassis contacts");
}
