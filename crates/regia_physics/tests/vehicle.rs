// SPDX-License-Identifier: MPL-2.0
//! Ray-cast vehicle wrapper (`regia_physics::vehicle`), headless.
//!
//! Every test builds a bare [`PhysicsWorld`] (or an app with `PhysicsPlugin` and
//! `VehiclePlugin`), steps it a fixed number of times at 60 Hz and checks the vehicle's
//! readout: the suspension settles, throttle pushes along the chassis' forward axis (−Z),
//! steering turns the right way, the brake stops the car, a wheel over a hole reports no
//! ground, and the same inputs give a bit-identical run. No GPU, window or wall clock.

use std::time::Duration;

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::ColliderBuilder;
use regia_app::prelude::*;
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::convert::to_rapier_vec3;
use regia_physics::vehicle::{
    Vehicle, VehicleConfig, VehicleController, VehicleError, VehicleInput, VehiclePlugin, WheelConfig,
    spawn_vehicle,
};
use regia_physics::{PhysicsPlugin, RigidBodyHandle};
use regia_scene::Transform;

/// The fixed step every test runs at.
const DT: f32 = 1.0 / 60.0;

/// Gravity used by the bare-world helper (the engine default).
const GRAVITY: Vec3 = Vec3::new(0.0, -9.81, 0.0);

/// Steps the rapier world once, the way `step_physics` does (gravity, fixed dt).
fn step(physics: &mut PhysicsWorld) {
    let integration = IntegrationParameters {
        dt: DT,
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
        to_rapier_vec3(GRAVITY),
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

/// Adds a fixed box whose top face is at y = 0, centred on (`x`, `z`) with the given
/// half extents in x and z.
fn ground_box(physics: &mut PhysicsWorld, centre_x: f32, centre_z: f32, half_x: f32, half_z: f32) {
    let body = physics.bodies.insert(
        RigidBodyBuilder::fixed()
            .translation(to_rapier_vec3(Vec3::new(centre_x, -0.5, centre_z)))
            .build(),
    );
    let PhysicsWorld {
        bodies, colliders, ..
    } = physics;
    colliders.insert_with_parent(ColliderBuilder::cuboid(half_x, 0.5, half_z).build(), body, bodies);
}

/// A world with a 400 m × 400 m flat plane at y = 0.
fn flat_world() -> PhysicsWorld {
    let mut physics = PhysicsWorld::default();
    ground_box(&mut physics, 0.0, 0.0, 200.0, 200.0);
    physics
}

/// Height the default car's chassis centre rests at: wheel hard point (−0.2 below the
/// centre) plus the settled suspension length plus the wheel radius above the ground.
fn rest_height(config: &VehicleConfig) -> f32 {
    let wheel = &config.wheels[0];
    let wheels = config.wheels.len() as f32;
    let compression = GRAVITY.length() / (wheels * wheel.suspension_stiffness);
    -wheel.position.y + (wheel.suspension_rest_length - compression) + wheel.radius
}

/// Drives `vehicle` for `steps` fixed steps with a constant input.
fn drive(physics: &mut PhysicsWorld, vehicle: &mut Vehicle, input: VehicleInput, steps: usize) {
    for _ in 0..steps {
        vehicle.update(physics, input, DT).expect("vehicle update");
        step(physics);
    }
}

/// A default car spawned just above the plane at the origin facing −Z, settled for 300 steps.
fn settled_car() -> (PhysicsWorld, Vehicle) {
    let mut physics = flat_world();
    let config = VehicleConfig::default();
    let at = Vec3::new(0.0, rest_height(&config) + 0.05, 0.0);
    let mut vehicle = Vehicle::spawn(&mut physics, config, at, Quat::IDENTITY).expect("spawn");
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 300);
    (physics, vehicle)
}

/// Chassis translation in world space.
fn chassis_position(physics: &PhysicsWorld, vehicle: &Vehicle) -> Vec3 {
    vehicle.state(physics).expect("state").translation
}

/// Yaw of the chassis' forward axis (−Z) around +Y, radians; positive turns left.
fn heading_yaw(physics: &PhysicsWorld, vehicle: &Vehicle) -> f32 {
    let forward = vehicle.state(physics).expect("state").forward;
    // forward = (−sin yaw, 0, −cos yaw) for a rotation of −Z by `yaw` around +Y.
    (-forward.x).atan2(-forward.z)
}

// ---------------------------------------------------------------------------
// Configuration and error paths
// ---------------------------------------------------------------------------

#[test]
fn default_config_is_a_valid_four_wheel_car() {
    let config = VehicleConfig::default();
    assert_eq!(config.validate(), Ok(()));
    assert_eq!(config.wheels.len(), 4);
    let steered = config.wheels.iter().filter(|w| w.steer).count();
    let driven = config.wheels.iter().filter(|w| w.drive).count();
    let braked = config.wheels.iter().filter(|w| w.brake).count();
    assert_eq!((steered, driven, braked), (2, 2, 4));
    // The steered wheels are the front ones (forward is −Z).
    assert!(
        config
            .wheels
            .iter()
            .filter(|w| w.steer)
            .all(|w| w.position.z < 0.0)
    );
}

#[test]
fn invalid_configs_are_rejected_with_a_named_error() {
    let mut physics = flat_world();
    let spawn = |physics: &mut PhysicsWorld, config: VehicleConfig| {
        Vehicle::spawn(physics, config, Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY).map(|_| ())
    };

    let no_wheels = VehicleConfig {
        wheels: Vec::new(),
        ..VehicleConfig::default()
    };
    assert_eq!(spawn(&mut physics, no_wheels), Err(VehicleError::NoWheels));

    let too_many = VehicleConfig {
        wheels: vec![WheelConfig::default(); regia_physics::vehicle::MAX_WHEELS + 1],
        ..VehicleConfig::default()
    };
    assert_eq!(
        spawn(&mut physics, too_many),
        Err(VehicleError::TooManyWheels {
            count: regia_physics::vehicle::MAX_WHEELS + 1
        })
    );

    let mut zero_radius = VehicleConfig::default();
    zero_radius.wheels[2].radius = 0.0;
    assert_eq!(
        spawn(&mut physics, zero_radius),
        Err(VehicleError::InvalidWheel {
            index: 2,
            field: "radius"
        })
    );

    let mut nan_position = VehicleConfig::default();
    nan_position.wheels[1].position.x = f32::NAN;
    assert_eq!(
        spawn(&mut physics, nan_position),
        Err(VehicleError::InvalidWheel {
            index: 1,
            field: "position"
        })
    );

    let mut negative_stiffness = VehicleConfig::default();
    negative_stiffness.wheels[0].suspension_stiffness = -1.0;
    assert_eq!(
        spawn(&mut physics, negative_stiffness),
        Err(VehicleError::InvalidWheel {
            index: 0,
            field: "suspension_stiffness"
        })
    );

    let zero_mass = VehicleConfig {
        chassis_mass: 0.0,
        ..VehicleConfig::default()
    };
    assert_eq!(
        spawn(&mut physics, zero_mass),
        Err(VehicleError::InvalidConfig {
            field: "chassis_mass"
        })
    );

    let too_much_lock = VehicleConfig {
        max_steer_angle: std::f32::consts::FRAC_PI_2,
        ..VehicleConfig::default()
    };
    assert_eq!(
        spawn(&mut physics, too_much_lock),
        Err(VehicleError::InvalidConfig {
            field: "max_steer_angle"
        })
    );

    let bad_pose = Vehicle::spawn(
        &mut physics,
        VehicleConfig::default(),
        Vec3::new(0.0, f32::INFINITY, 0.0),
        Quat::IDENTITY,
    );
    assert_eq!(bad_pose.map(|_| ()), Err(VehicleError::InvalidPose));

    // Nothing was added to the world by the failed spawns: only the ground.
    assert_eq!(physics.bodies.len(), 1);
    assert_eq!(physics.colliders.len(), 1);
}

#[test]
fn error_codes_are_stable_lowercase_identifiers() {
    let errors = [
        VehicleError::NoWheels,
        VehicleError::TooManyWheels { count: 99 },
        VehicleError::InvalidWheel {
            index: 0,
            field: "radius",
        },
        VehicleError::InvalidConfig {
            field: "chassis_mass",
        },
        VehicleError::InvalidPose,
        VehicleError::InvalidTimeStep,
        VehicleError::ChassisMissing,
        VehicleError::ChassisNotDynamic,
        VehicleError::ChassisMassless,
        VehicleError::NoPhysicsWorld,
        VehicleError::StateNotFinite,
    ];
    let codes: Vec<&str> = errors.iter().map(VehicleError::code).collect();
    assert_eq!(
        codes,
        [
            "vehicle_no_wheels",
            "vehicle_too_many_wheels",
            "vehicle_invalid_wheel",
            "vehicle_invalid_config",
            "vehicle_invalid_pose",
            "vehicle_invalid_time_step",
            "vehicle_chassis_missing",
            "vehicle_chassis_not_dynamic",
            "vehicle_chassis_massless",
            "vehicle_no_physics_world",
            "vehicle_state_not_finite",
        ]
    );
    for code in codes {
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
}

#[test]
fn a_bad_time_step_is_rejected_without_touching_the_world() {
    let (mut physics, mut vehicle) = settled_car();
    let before = physics.state_digest();
    for dt in [0.0, -DT, f32::NAN, f32::INFINITY] {
        assert_eq!(
            vehicle.update(&mut physics, VehicleInput::default(), dt),
            Err(VehicleError::InvalidTimeStep),
            "dt = {dt}"
        );
    }
    assert_eq!(physics.state_digest(), before);
}

#[test]
fn a_removed_chassis_is_an_error_not_a_panic() {
    let (mut physics, mut vehicle) = settled_car();
    physics.remove_body(vehicle.chassis().to_handle());
    assert_eq!(
        vehicle.update(&mut physics, VehicleInput::default(), DT),
        Err(VehicleError::ChassisMissing)
    );
    assert_eq!(
        vehicle.state(&physics).map(|_| ()),
        Err(VehicleError::ChassisMissing)
    );
    assert_eq!(
        vehicle.wheel_states(&physics).map(|_| ()),
        Err(VehicleError::ChassisMissing)
    );
    assert_eq!(
        vehicle.reset(&mut physics, Vec3::ZERO, Quat::IDENTITY),
        Err(VehicleError::ChassisMissing)
    );
}

#[test]
fn wheel_states_into_reuses_the_caller_buffer_and_matches_wheel_states() {
    let (physics, vehicle) = settled_car();
    let fresh = vehicle.wheel_states(&physics).expect("wheels");
    let mut out: Vec<regia_physics::vehicle::WheelState> = Vec::new();
    vehicle.wheel_states_into(&physics, &mut out).expect("wheels");
    assert_eq!(out, fresh);
    // A second call reuses the buffer (no realloc: same pointer, same capacity) and rewrites
    // the contents (ADR-3271's per-step `Vec` is gone for warm callers).
    let capacity = out.capacity();
    let pointer = out.as_ptr();
    vehicle.wheel_states_into(&physics, &mut out).expect("wheels");
    assert_eq!(out, fresh);
    assert_eq!(out.capacity(), capacity);
    assert_eq!(out.as_ptr(), pointer, "the buffer was reallocated");
    // The error path leaves the buffer empty.
    let (mut physics, vehicle) = settled_car();
    physics.remove_body(vehicle.chassis().to_handle());
    assert_eq!(
        vehicle.wheel_states_into(&physics, &mut out),
        Err(VehicleError::ChassisMissing)
    );
    assert!(out.is_empty());
}

#[test]
fn attach_needs_an_existing_dynamic_body() {
    let mut physics = flat_world();
    let fixed = physics.bodies.insert(RigidBodyBuilder::fixed().build());
    assert_eq!(
        Vehicle::attach(
            &mut physics,
            RigidBodyHandle::from_rapier(fixed),
            VehicleConfig::default()
        )
        .map(|_| ()),
        Err(VehicleError::ChassisNotDynamic)
    );
    let massless = physics.bodies.insert(RigidBodyBuilder::dynamic().build());
    assert_eq!(
        Vehicle::attach(
            &mut physics,
            RigidBodyHandle::from_rapier(massless),
            VehicleConfig::default()
        )
        .map(|_| ()),
        Err(VehicleError::ChassisMassless)
    );
    physics.remove_body(massless);
    let missing = RigidBodyHandle {
        raw: 999,
        generation: 7,
    };
    assert_eq!(
        Vehicle::attach(&mut physics, missing, VehicleConfig::default()).map(|_| ()),
        Err(VehicleError::ChassisMissing)
    );

    // A dynamic body with its own collider drives like a spawned one.
    let config = VehicleConfig::default();
    let at = Vec3::new(0.0, rest_height(&config) + 0.05, 0.0);
    let body = physics.bodies.insert(
        RigidBodyBuilder::dynamic()
            .translation(to_rapier_vec3(at))
            .build(),
    );
    {
        let PhysicsWorld {
            bodies, colliders, ..
        } = &mut physics;
        colliders.insert_with_parent(
            ColliderBuilder::cuboid(0.9, 0.35, 2.1).mass(1200.0).build(),
            body,
            bodies,
        );
    }
    let mut vehicle =
        Vehicle::attach(&mut physics, RigidBodyHandle::from_rapier(body), config).expect("attach");
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 240);
    let wheels = vehicle.wheel_states(&physics).expect("wheels");
    assert!(wheels.iter().all(|w| w.grounded), "{wheels:?}");
    // The chassis' own collider is never ground: the rays start inside it.
    let own = physics_collider_of(&physics, body);
    assert!(
        wheels
            .iter()
            .all(|w| w.ground_collider.is_some() && w.ground_collider != Some(own))
    );
    assert!(vehicle.chassis_collider().is_none());
}

/// The first collider attached to `body`.
fn physics_collider_of(
    physics: &PhysicsWorld,
    body: rapier3d::dynamics::RigidBodyHandle,
) -> rapier3d::geometry::ColliderHandle {
    physics.bodies[body].colliders()[0]
}

// ---------------------------------------------------------------------------
// Behavior
// ---------------------------------------------------------------------------

#[test]
fn a_car_on_a_flat_plane_settles_with_its_suspension_compressed_within_range() {
    let (mut physics, mut vehicle) = settled_car();
    let config = vehicle.config().clone();
    let wheels = vehicle.wheel_states(&physics).expect("wheels");
    assert_eq!(wheels.len(), 4);
    let expected = GRAVITY.length() / (4.0 * config.wheels[0].suspension_stiffness);
    for wheel in &wheels {
        assert!(wheel.grounded, "wheel {} not grounded", wheel.index);
        assert!(
            wheel.compression > 0.0 && wheel.compression < config.wheels[wheel.index].suspension_travel,
            "wheel {} compression {} outside (0, travel)",
            wheel.index,
            wheel.compression
        );
        // Spring force balances the chassis weight: k·Δ·m per wheel = m·g / 4.
        assert!(
            (wheel.compression - expected).abs() < 0.02,
            "wheel {} compression {} vs expected {expected}",
            wheel.index,
            wheel.compression
        );
        assert!(
            (wheel.suspension_length + wheel.compression - config.wheels[wheel.index].suspension_rest_length)
                .abs()
                < 1.0e-5
        );
        let normal = wheel.contact_normal.expect("contact normal");
        assert!(normal.abs_diff_eq(Vec3::Y, 1.0e-4), "{normal}");
        let point = wheel.contact_point.expect("contact point");
        assert!(point.y.abs() < 1.0e-3, "contact on the plane: {point}");
        assert!(wheel.suspension_force > 0.0);
    }
    let state = vehicle.state(&physics).expect("state");
    // At rest: no horizontal drift, no rotation. The vertical reading is rapier's end-of-step
    // velocity: the suspension impulse lands once at the start of the step while gravity acts
    // over the solver's substeps, so a car resting on its springs reads a small downward
    // velocity (about −0.06 m/s at 60 Hz) while its position holds still (checked below).
    let horizontal = Vec3::new(state.linear_velocity.x, 0.0, state.linear_velocity.z);
    assert!(horizontal.length() < 1.0e-3, "{:?}", state.linear_velocity);
    assert!(state.linear_velocity.y.abs() < 0.1, "{:?}", state.linear_velocity);
    assert!(
        state.angular_velocity.length() < 1.0e-3,
        "{:?}",
        state.angular_velocity
    );
    assert!(
        (state.translation.y - rest_height(&config)).abs() < 0.03,
        "chassis at {} vs {}",
        state.translation.y,
        rest_height(&config)
    );
    assert_eq!(state.grounded_wheels, 4);
    assert!(
        state.forward.abs_diff_eq(Vec3::NEG_Z, 1.0e-3),
        "level: {}",
        state.forward
    );
    // Settled: the car stays put without input.
    let before = state.translation;
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    assert!(chassis_position(&physics, &vehicle).distance(before) < 1.0e-3);
}

#[test]
fn throttle_accelerates_along_the_chassis_forward_axis() {
    let (mut physics, mut vehicle) = settled_car();
    let start = chassis_position(&physics, &vehicle);
    let mut previous_speed = vehicle.state(&physics).expect("state").forward_speed;
    for second in 0..2 {
        drive(
            &mut physics,
            &mut vehicle,
            VehicleInput {
                throttle: 1.0,
                ..VehicleInput::default()
            },
            60,
        );
        let speed = vehicle.state(&physics).expect("state").forward_speed;
        assert!(
            speed > previous_speed + 1.0,
            "second {second}: {previous_speed} -> {speed}"
        );
        previous_speed = speed;
    }
    let moved = chassis_position(&physics, &vehicle) - start;
    // Forward is −Z: the car went that way and nowhere else.
    assert!(moved.z < -3.0, "{moved}");
    assert!(moved.x.abs() < 0.02 * moved.z.abs(), "{moved}");
    assert!(moved.y.abs() < 0.1, "{moved}");
    // The wheels rolled forward: spin grows positive on every grounded wheel.
    for wheel in vehicle.wheel_states(&physics).expect("wheels") {
        assert!(
            wheel.spin_angle > 1.0,
            "wheel {} spin {}",
            wheel.index,
            wheel.spin_angle
        );
    }

    // Negative throttle reverses.
    let (mut physics, mut vehicle) = settled_car();
    let start = chassis_position(&physics, &vehicle);
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: -1.0,
            ..VehicleInput::default()
        },
        90,
    );
    let moved = chassis_position(&physics, &vehicle) - start;
    assert!(moved.z > 1.0, "reverse goes +Z: {moved}");
    assert!(vehicle.state(&physics).expect("state").forward_speed < -1.0);
}

#[test]
fn forward_follows_the_chassis_rotation() {
    // Yawed 90° left: the chassis' −Z now points along world −X.
    let mut physics = flat_world();
    let config = VehicleConfig::default();
    let at = Vec3::new(0.0, rest_height(&config) + 0.05, 0.0);
    let rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    let mut vehicle = Vehicle::spawn(&mut physics, config, at, rotation).expect("spawn");
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 120);
    let start = chassis_position(&physics, &vehicle);
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: 1.0,
            ..VehicleInput::default()
        },
        90,
    );
    let moved = chassis_position(&physics, &vehicle) - start;
    assert!(moved.x < -2.0, "{moved}");
    assert!(moved.z.abs() < 0.02 * moved.x.abs(), "{moved}");
    // The heading (the chassis pitches a little under acceleration, so compare it flat).
    let forward = vehicle.state(&physics).expect("state").forward;
    let heading = Vec3::new(forward.x, 0.0, forward.z).normalize();
    assert!(heading.abs_diff_eq(Vec3::NEG_X, 1.0e-3), "{forward}");
}

#[test]
fn steering_turns_right_and_left_with_the_right_sign() {
    let turn = |steer: f32| {
        let (mut physics, mut vehicle) = settled_car();
        // Get rolling straight first, then hold the turn at part throttle.
        drive(
            &mut physics,
            &mut vehicle,
            VehicleInput {
                throttle: 0.6,
                ..VehicleInput::default()
            },
            45,
        );
        let start = chassis_position(&physics, &vehicle);
        let input = VehicleInput {
            throttle: 0.3,
            steer,
            ..VehicleInput::default()
        };
        drive(&mut physics, &mut vehicle, input, 90);
        let wheels = vehicle.wheel_states(&physics).expect("wheels");
        let state = vehicle.state(&physics).expect("state");
        (
            chassis_position(&physics, &vehicle) - start,
            heading_yaw(&physics, &vehicle),
            state.angular_velocity.y,
            wheels,
        )
    };

    let max = VehicleConfig::default().max_steer_angle;

    // Steer +1 = right: the heading turns clockwise seen from above (negative yaw) and the
    // car drifts towards +X (right of −Z).
    let (moved, yaw, yaw_rate, wheels) = turn(1.0);
    assert!(yaw < -0.2, "yaw {yaw}");
    assert!(yaw_rate < 0.0, "yaw rate {yaw_rate}");
    assert!(moved.x > 0.5, "{moved}");
    for wheel in &wheels {
        let expected = if wheel.index < 2 { -max } else { 0.0 };
        assert!(
            (wheel.steer_angle - expected).abs() < 1.0e-6,
            "wheel {} steer {}",
            wheel.index,
            wheel.steer_angle
        );
    }

    // Steer −1 = left: the mirror image.
    let (moved_left, yaw_left, yaw_rate_left, wheels_left) = turn(-1.0);
    assert!(yaw_left > 0.2, "yaw {yaw_left}");
    assert!(yaw_rate_left > 0.0, "yaw rate {yaw_rate_left}");
    assert!(moved_left.x < -0.5, "{moved_left}");
    assert!(wheels_left[0].steer_angle > 0.0 && wheels_left[1].steer_angle > 0.0);

    // The car is left/right symmetric, so the two turns mirror each other.
    assert!((yaw + yaw_left).abs() < 0.05 * yaw.abs(), "{yaw} vs {yaw_left}");
    assert!(
        (moved.x + moved_left.x).abs() < 0.05 * moved.x.abs(),
        "{moved} vs {moved_left}"
    );
    assert!(
        (moved.z - moved_left.z).abs() < 0.05 * moved.z.abs(),
        "{moved} vs {moved_left}"
    );

    // Half input is half the lock.
    let (_, half_yaw, _, half_wheels) = turn(0.5);
    assert!((half_wheels[0].steer_angle + 0.5 * max).abs() < 1.0e-6);
    assert!(half_yaw < 0.0 && half_yaw > yaw, "half {half_yaw} vs full {yaw}");
}

#[test]
fn the_brake_stops_the_car_and_holds_it() {
    let (mut physics, mut vehicle) = settled_car();
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: 1.0,
            ..VehicleInput::default()
        },
        120,
    );
    let speed = vehicle.state(&physics).expect("state").forward_speed;
    assert!(speed > 5.0, "{speed}");

    let brake = VehicleInput {
        brake: 1.0,
        ..VehicleInput::default()
    };
    let mut stopped_after = None;
    for step_index in 0..240 {
        drive(&mut physics, &mut vehicle, brake, 1);
        let speed = vehicle.state(&physics).expect("state").forward_speed;
        if speed.abs() < 0.05 {
            stopped_after = Some(step_index);
            break;
        }
    }
    let stopped_after = stopped_after.expect("the brake stops the car within 4 s");
    // Full brake on the default car is about 1 g: from > 5 m/s that takes more than 0.4 s.
    assert!(stopped_after > 20, "stopped after {stopped_after} steps");

    // The body pitches back after the stop (a few centimeters at the center of mass) and
    // settles; held on the brake it then stays put (no creep, no reverse).
    let stopped_at = chassis_position(&physics, &vehicle);
    drive(&mut physics, &mut vehicle, brake, 90);
    let held = chassis_position(&physics, &vehicle);
    assert!(
        held.distance(stopped_at) < 0.05,
        "rebound {}",
        held.distance(stopped_at)
    );
    drive(&mut physics, &mut vehicle, brake, 60);
    assert!(chassis_position(&physics, &vehicle).distance(held) < 1.0e-3);
    assert!(vehicle.state(&physics).expect("state").forward_speed.abs() < 1.0e-3);

    // Throttle and brake together: the brake wins on wheels that can brake, so the car does
    // not pull away.
    let start = chassis_position(&physics, &vehicle);
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: 1.0,
            brake: 1.0,
            ..VehicleInput::default()
        },
        60,
    );
    assert!(chassis_position(&physics, &vehicle).distance(start) < 1.0e-3);
}

#[test]
fn coasting_slows_down_through_rolling_resistance_only() {
    let (mut physics, mut vehicle) = settled_car();
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: 1.0,
            ..VehicleInput::default()
        },
        90,
    );
    let fast = vehicle.state(&physics).expect("state").forward_speed;
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    let coasted = vehicle.state(&physics).expect("state").forward_speed;
    // Slower, but nowhere near a brake stop.
    assert!(coasted < fast, "{fast} -> {coasted}");
    assert!(coasted > 0.8 * fast, "{fast} -> {coasted}");
}

#[test]
fn a_wheel_over_a_hole_reports_not_grounded() {
    // Ground under the left half only (x < 0): the right wheels (x = +0.8) hang over nothing.
    let mut physics = PhysicsWorld::default();
    ground_box(&mut physics, -100.0, 0.0, 100.0, 100.0);
    let config = VehicleConfig::default();
    let at = Vec3::new(0.0, rest_height(&config), 0.0);
    let mut vehicle = Vehicle::spawn(&mut physics, config.clone(), at, Quat::IDENTITY).expect("spawn");

    // Before the first update nothing has been ray-cast yet: no wheel is grounded and the
    // suspension reads its rest length.
    for wheel in vehicle.wheel_states(&physics).expect("wheels") {
        assert!(!wheel.grounded);
        assert_eq!(wheel.compression, 0.0);
        assert_eq!(
            wheel.suspension_length,
            config.wheels[wheel.index].suspension_rest_length
        );
    }

    // One step builds the broad phase; the next update casts the wheel rays against it.
    step(&mut physics);
    vehicle
        .update(&mut physics, VehicleInput::default(), DT)
        .expect("update");
    let wheels = vehicle.wheel_states(&physics).expect("wheels");
    for wheel in &wheels {
        let over_ground = config.wheels[wheel.index].position.x < 0.0;
        assert_eq!(wheel.grounded, over_ground, "wheel {}", wheel.index);
        if over_ground {
            assert!(wheel.contact_point.is_some() && wheel.ground_collider.is_some());
        } else {
            assert_eq!(wheel.contact_point, None);
            assert_eq!(wheel.contact_normal, None);
            assert_eq!(wheel.ground_collider, None);
            assert_eq!(wheel.ground_entity, None);
            assert_eq!(wheel.suspension_force, 0.0);
            assert_eq!(wheel.compression, 0.0);
            assert_eq!(
                wheel.suspension_length,
                config.wheels[wheel.index].suspension_rest_length
            );
        }
    }
    assert_eq!(vehicle.state(&physics).expect("state").grounded_wheels, 2);
}

#[test]
fn a_falling_car_has_no_grounded_wheel_until_it_lands() {
    let mut physics = flat_world();
    let mut vehicle = Vehicle::spawn(
        &mut physics,
        VehicleConfig::default(),
        Vec3::new(0.0, 4.0, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 20);
    let airborne = vehicle.state(&physics).expect("state");
    assert_eq!(airborne.grounded_wheels, 0);
    assert!(airborne.linear_velocity.y < -2.0);
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 280);
    assert_eq!(vehicle.state(&physics).expect("state").grounded_wheels, 4);
}

#[test]
fn wheel_poses_follow_the_chassis_and_the_suspension() {
    let (mut physics, mut vehicle) = settled_car();
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: 0.5,
            steer: 0.5,
            ..VehicleInput::default()
        },
        30,
    );
    let config = vehicle.config().clone();
    let state = vehicle.state(&physics).expect("state");
    for wheel in &state.wheels {
        let cfg = &config.wheels[wheel.index];
        // Chassis space: hard point straight down by the suspension length.
        let local = cfg.position + Vec3::NEG_Y * wheel.suspension_length;
        assert!(
            wheel.local_translation.abs_diff_eq(local, 1.0e-6),
            "{}",
            wheel.index
        );
        let expected_local =
            Quat::from_rotation_y(wheel.steer_angle) * Quat::from_rotation_x(-wheel.spin_angle);
        assert!(wheel.local_rotation.abs_diff_eq(expected_local, 1.0e-6));
        // World space = chassis pose × chassis space.
        let world = state.translation + state.rotation * local;
        assert!(
            wheel.world_translation.abs_diff_eq(world, 1.0e-4),
            "{}",
            wheel.index
        );
        assert!(
            wheel
                .world_rotation
                .abs_diff_eq(state.rotation * wheel.local_rotation, 1.0e-5)
        );
        // The wheel centre sits one radius above its contact point.
        if let Some(contact) = wheel.contact_point {
            let gap = wheel.world_translation.distance(contact);
            assert!((gap - cfg.radius).abs() < 0.05, "wheel {} gap {gap}", wheel.index);
        }
    }
    // Steered wheels' axles turned with the steering; the rear axles did not.
    let right = state.rotation * Vec3::X;
    let front_axle = state.wheels[0].world_rotation * Vec3::X;
    let rear_axle = state.wheels[2].world_rotation * Vec3::X;
    assert!(rear_axle.abs_diff_eq(right, 1.0e-4));
    assert!((front_axle.dot(right) - state.wheels[0].steer_angle.cos()).abs() < 1.0e-4);
}

#[test]
fn reset_returns_the_car_to_its_spawn_pose_at_rest() {
    let mut physics = flat_world();
    let config = VehicleConfig::default();
    let spawn_at = Vec3::new(3.0, rest_height(&config) + 0.05, -2.0);
    let spawn_rotation = Quat::from_rotation_y(0.3);
    let mut vehicle = Vehicle::spawn(&mut physics, config, spawn_at, spawn_rotation).expect("spawn");
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: 1.0,
            steer: 0.7,
            ..VehicleInput::default()
        },
        200,
    );
    assert!(chassis_position(&physics, &vehicle).distance(spawn_at) > 5.0);

    vehicle.reset_to_spawn(&mut physics).expect("reset");
    let state = vehicle.state(&physics).expect("state");
    assert!(
        state.translation.abs_diff_eq(spawn_at, 1.0e-5),
        "{}",
        state.translation
    );
    assert!(state.rotation.abs_diff_eq(spawn_rotation, 1.0e-5));
    assert_eq!(state.linear_velocity, Vec3::ZERO);
    assert_eq!(state.angular_velocity, Vec3::ZERO);
    for wheel in &state.wheels {
        assert_eq!(wheel.spin_angle, 0.0);
        assert_eq!(wheel.steer_angle, 0.0);
        assert!(!wheel.grounded);
    }
    assert_eq!(vehicle.steps(), 0);

    // It settles and drives again after the reset.
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 240);
    assert_eq!(vehicle.state(&physics).expect("state").grounded_wheels, 4);

    // Reset to an explicit pose; a non-finite pose is refused and changes nothing.
    let elsewhere = Vec3::new(-10.0, 2.0, 5.0);
    vehicle
        .reset(&mut physics, elsewhere, Quat::IDENTITY)
        .expect("reset");
    assert!(chassis_position(&physics, &vehicle).abs_diff_eq(elsewhere, 1.0e-5));
    assert_eq!(
        vehicle.reset(&mut physics, Vec3::splat(f32::NAN), Quat::IDENTITY),
        Err(VehicleError::InvalidPose)
    );
    assert!(chassis_position(&physics, &vehicle).abs_diff_eq(elsewhere, 1.0e-5));
}

#[test]
fn non_finite_and_out_of_range_input_is_sanitized() {
    let input = VehicleInput {
        throttle: f32::NAN,
        brake: 7.0,
        steer: -f32::INFINITY,
    }
    .sanitized();
    assert_eq!(
        input,
        VehicleInput {
            throttle: 0.0,
            brake: 1.0,
            steer: 0.0,
        }
    );
    assert_eq!(
        VehicleInput {
            throttle: -3.0,
            brake: -1.0,
            steer: 2.0,
        }
        .sanitized(),
        VehicleInput {
            throttle: -1.0,
            brake: 0.0,
            steer: 1.0,
        }
    );

    // A NaN throttle drives exactly like no throttle.
    let (mut a, mut va) = settled_car();
    let (mut b, mut vb) = settled_car();
    let nan = VehicleInput {
        throttle: f32::NAN,
        ..VehicleInput::default()
    };
    drive(&mut a, &mut va, nan, 30);
    drive(&mut b, &mut vb, VehicleInput::default(), 30);
    assert_eq!(a.state_digest(), b.state_digest());
}

#[test]
fn the_same_inputs_give_a_bit_identical_run_over_300_steps() {
    /// Scripted input for step `i`: launch, weave, brake, reverse.
    fn script(i: usize) -> VehicleInput {
        let t = i as f32 / 60.0;
        VehicleInput {
            throttle: match i {
                0..=119 => 1.0,
                120..=179 => 0.4,
                180..=239 => 0.0,
                _ => -0.6,
            },
            brake: if (180..240).contains(&i) { 0.8 } else { 0.0 },
            steer: (t * 2.1).sin() * if i < 180 { 0.8 } else { 0.3 },
        }
    }

    fn run() -> (Vec<u64>, Vec<u32>) {
        let mut physics = flat_world();
        let config = VehicleConfig::default();
        let at = Vec3::new(1.5, rest_height(&config) + 0.2, -4.0);
        let mut vehicle =
            Vehicle::spawn(&mut physics, config, at, Quat::from_rotation_y(0.4)).expect("spawn");
        let mut digests = Vec::with_capacity(300);
        let mut wheel_bits = Vec::new();
        for i in 0..300 {
            vehicle.update(&mut physics, script(i), DT).expect("update");
            step(&mut physics);
            digests.push(physics.state_digest());
            for wheel in vehicle.wheel_states(&physics).expect("wheels") {
                wheel_bits.push(wheel.spin_angle.to_bits());
                wheel_bits.push(wheel.suspension_length.to_bits());
                wheel_bits.push(u32::from(wheel.grounded));
            }
        }
        (digests, wheel_bits)
    }

    let (digests_a, wheels_a) = run();
    let (digests_b, wheels_b) = run();
    assert_eq!(digests_a.len(), 300);
    if let Some(first) = digests_a.iter().zip(&digests_b).position(|(a, b)| a != b) {
        panic!("runs diverged at step {first}");
    }
    assert_eq!(wheels_a, wheels_b);
    // The script really moved the car (a parked car would be trivially deterministic).
    assert!(digests_a.windows(2).filter(|w| w[0] != w[1]).count() > 250);
}

// ---------------------------------------------------------------------------
// ADR-4773 determinism pins (audit G finding 7): a slip-tire run's digest and wheel readouts
// are bit-identical on every host, so every angle and rotation behind them comes from the
// deterministic kernels, never the platform libm.
// ---------------------------------------------------------------------------

/// `vehicle.state_digest()` after the scripted run below, pinned. Linux/x86_64 only: the run
/// crosses 120 rapier steps, and rapier is deterministic per platform, not across platforms
/// (spec 15 § 5) — on another target a last-ulp rapier difference would diverge the digest for
/// reasons unrelated to the kernels this pin guards.
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    not(feature = "physics-cross-platform-determinism")
))]
const SLIP_RUN_DIGEST: u64 = 14_357_029_193_857_743_748;
/// `wheel_states()[0]` after the scripted run below: `(slip_angle, local_rotation.xyzw)`.
/// Gated like [`SLIP_RUN_DIGEST`].
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    not(feature = "physics-cross-platform-determinism")
))]
const SLIP_RUN_WHEEL0_BITS: [u32; 5] = [
    3_166_716_703,
    1_063_709_619,
    1_026_332_564,
    1_035_557_590,
    3_201_762_624,
];

#[test]
fn slip_tire_run_pins_digest_and_wheel_readout_bits() {
    use regia_physics::vehicle_tire::{SlipTire, TireModel};

    let mut physics = flat_world();
    let mut config = VehicleConfig::default();
    for wheel in &mut config.wheels {
        wheel.tire_model = TireModel::Slip(SlipTire::default());
    }
    let at = Vec3::new(0.0, rest_height(&config) + 0.05, 0.0);
    let mut vehicle = Vehicle::spawn(&mut physics, config, at, Quat::IDENTITY).expect("spawn");
    // Settle, then drive and steer so the slip angles and readout quaternions are exercised.
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: 0.3,
            steer: 0.4,
            ..VehicleInput::default()
        },
        120,
    );
    let wheels = vehicle.wheel_states(&physics).expect("wheels");
    assert!(
        wheels.iter().all(|wheel| wheel.grounded),
        "the run must load the tires for the digest to cover them"
    );
    // The bit pins cross 120 rapier steps, so they hold on Linux x86_64 only (spec 15 § 5).
    #[cfg(all(
        target_os = "linux",
        target_arch = "x86_64",
        not(feature = "physics-cross-platform-determinism")
    ))]
    {
        let w = &wheels[0];
        assert_eq!(
            vehicle.state_digest(),
            SLIP_RUN_DIGEST,
            "the vehicle digest folds the tire words; both must be platform-independent"
        );
        assert_eq!(
            [
                w.slip_angle.to_bits(),
                w.local_rotation.x.to_bits(),
                w.local_rotation.y.to_bits(),
                w.local_rotation.z.to_bits(),
                w.local_rotation.w.to_bits(),
            ],
            SLIP_RUN_WHEEL0_BITS,
            "steer/spin quaternions and the contact slip angle use the deterministic kernels"
        );
    }
}

// ---------------------------------------------------------------------------
// ECS: VehiclePlugin
// ---------------------------------------------------------------------------

fn vehicle_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, VehiclePlugin));
    app
}

#[test]
fn the_plugin_drives_a_spawned_vehicle_entity_from_its_input_component() {
    let mut app = vehicle_app();
    {
        let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
        ground_box(&mut physics, 0.0, 0.0, 200.0, 200.0);
    }
    let config = VehicleConfig::default();
    let at = Vec3::new(0.0, rest_height(&config) + 0.05, 0.0);
    let entity = spawn_vehicle(app.world_mut(), config, at, Quat::IDENTITY).expect("spawn_vehicle");
    // The chassis is registered to the entity, so queries and events name it.
    {
        let physics = app.world().resource::<PhysicsWorld>();
        let handle = app
            .world()
            .get::<RigidBodyHandle>(entity)
            .expect("handle")
            .to_handle();
        assert_eq!(physics.entity_of_body(handle), Some(entity));
    }
    for _ in 0..240 {
        app.update();
    }
    let settled = app
        .world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation;
    {
        let controller = app.world().get::<VehicleController>(entity).expect("controller");
        assert_eq!(controller.last_error, None);
        let physics = app.world().resource::<PhysicsWorld>();
        let state = controller.vehicle.state(physics).expect("state");
        assert_eq!(state.grounded_wheels, 4);
        assert!(
            state.wheels.iter().all(|w| w.ground_entity.is_none()),
            "the ground has no entity"
        );
    }

    app.world_mut()
        .get_mut::<VehicleInput>(entity)
        .expect("input")
        .throttle = 1.0;
    for _ in 0..90 {
        app.update();
    }
    let driven = app
        .world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation;
    assert!(driven.z < settled.z - 2.0, "{settled} -> {driven}");

    // Despawning the entity takes the chassis out of the world; the plugin keeps running.
    let bodies_before = app.world().resource::<PhysicsWorld>().bodies.len();
    app.world_mut().despawn(entity);
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<PhysicsWorld>().bodies.len(),
        bodies_before - 1
    );
}

#[test]
fn spawn_vehicle_without_a_physics_world_is_an_error() {
    let mut app = App::new();
    let result = spawn_vehicle(
        app.world_mut(),
        VehicleConfig::default(),
        Vec3::ZERO,
        Quat::IDENTITY,
    );
    assert_eq!(result, Err(VehicleError::NoPhysicsWorld));
    assert_eq!(
        app.world_mut()
            .query::<&VehicleController>()
            .iter(app.world())
            .count(),
        0
    );
}

#[test]
fn a_vehicle_whose_chassis_vanished_records_the_error_instead_of_panicking() {
    let mut app = vehicle_app();
    let entity = spawn_vehicle(
        app.world_mut(),
        VehicleConfig::default(),
        Vec3::new(0.0, 2.0, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn_vehicle");
    let handle = app
        .world()
        .get::<RigidBodyHandle>(entity)
        .expect("handle")
        .to_handle();
    app.world_mut().resource_mut::<PhysicsWorld>().remove_body(handle);
    app.update();
    let controller = app.world().get::<VehicleController>(entity).expect("controller");
    assert_eq!(controller.last_error, Some(VehicleError::ChassisMissing));
}

#[test]
fn wheel_world_poses_are_finite_for_rendering_every_step() {
    let (mut physics, mut vehicle) = settled_car();
    for i in 0..120 {
        let input = VehicleInput {
            throttle: 1.0,
            steer: if i < 60 { 1.0 } else { -1.0 },
            brake: 0.0,
        };
        drive(&mut physics, &mut vehicle, input, 1);
        for wheel in vehicle.wheel_states(&physics).expect("wheels") {
            assert!(wheel.world_translation.is_finite());
            assert!(wheel.world_rotation.is_finite() && wheel.world_rotation.is_normalized());
        }
    }
}

/// A fixed plane (top at y = 0) whose collider is a member of `layer` only.
fn ground_on_layer(physics: &mut PhysicsWorld, layer: u32) {
    use rapier3d::geometry::{Group, InteractionGroups, InteractionTestMode};
    let body = physics.bodies.insert(
        RigidBodyBuilder::fixed()
            .translation(to_rapier_vec3(Vec3::new(0.0, -0.5, 0.0)))
            .build(),
    );
    let PhysicsWorld {
        bodies, colliders, ..
    } = physics;
    let groups = InteractionGroups::new(
        Group::from_bits_retain(1 << layer),
        Group::all(),
        InteractionTestMode::And,
    );
    colliders.insert_with_parent(
        ColliderBuilder::cuboid(100.0, 0.5, 100.0)
            .collision_groups(groups)
            .build(),
        body,
        bodies,
    );
}

/// Grounded wheel count after one ray cast of a default car over the layer-2 plane.
fn grounded_over_layer_two(config: VehicleConfig, layers: Option<&regia_physics::PhysicsLayers>) -> usize {
    let mut physics = PhysicsWorld::default();
    ground_on_layer(&mut physics, 2);
    let at = Vec3::new(0.0, rest_height(&config), 0.0);
    let mut vehicle = Vehicle::spawn(&mut physics, config, at, Quat::IDENTITY).expect("spawn");
    step(&mut physics);
    vehicle
        .update_with_layers(&mut physics, VehicleInput::default(), DT, layers)
        .expect("update");
    vehicle
        .wheel_states(&physics)
        .expect("wheels")
        .iter()
        .filter(|w| w.grounded)
        .count()
}

#[test]
fn wheel_rays_only_hit_the_ground_layers_of_the_config() {
    let with = |ground_layers| VehicleConfig {
        ground_layers,
        ..VehicleConfig::default()
    };
    assert_eq!(
        grounded_over_layer_two(with(None), None),
        4,
        "every layer by default"
    );
    assert_eq!(grounded_over_layer_two(with(Some(1 << 2)), None), 4);
    assert_eq!(
        grounded_over_layer_two(with(Some(1 << 0)), None),
        0,
        "the plane is not on layer 0"
    );
    assert_eq!(grounded_over_layer_two(with(Some((1 << 0) | (1 << 2))), None), 4);
    assert_eq!(
        grounded_over_layer_two(with(Some(0)), None),
        0,
        "an empty mask sees nothing"
    );
}

#[test]
fn the_query_layer_row_of_the_matrix_filters_the_wheel_rays() {
    let mut layers = regia_physics::PhysicsLayers::default();
    layers.query_rows[1] = 1 << 2;
    layers.query_rows[3] = 1 << 0;
    let from = |layer| VehicleConfig {
        query_layer: Some(layer),
        ..VehicleConfig::default()
    };
    assert_eq!(grounded_over_layer_two(from(1), Some(&layers)), 4);
    assert_eq!(grounded_over_layer_two(from(3), Some(&layers)), 0);
    // The row and the mask intersect.
    let both = VehicleConfig {
        query_layer: Some(1),
        ground_layers: Some(1 << 0),
        ..VehicleConfig::default()
    };
    assert_eq!(grounded_over_layer_two(both, Some(&layers)), 0);
    // Without a matrix the query layer has nothing to read.
    assert_eq!(grounded_over_layer_two(from(3), None), 4);
    // A layer past the matrix is a config error.
    let bad = VehicleConfig {
        query_layer: Some(regia_physics::MAX_LAYERS as u16),
        ..VehicleConfig::default()
    };
    assert_eq!(
        bad.validate(),
        Err(VehicleError::InvalidConfig { field: "query_layer" })
    );
}

// ---------------------------------------------------------------------------
// Floating origin (ADR-2435)
// ---------------------------------------------------------------------------

/// The suspension force and compression of every wheel.
fn wheel_loads(app: &App, entity: Entity) -> Vec<(f32, f32)> {
    let controller = app.world().get::<VehicleController>(entity).expect("controller");
    let physics = app.world().resource::<PhysicsWorld>();
    controller
        .vehicle
        .wheel_states(physics)
        .expect("wheels")
        .iter()
        .map(|w| (w.suspension_force, w.compression))
        .collect()
}

/// A rebase pending for the next fixed step (what `regia_world`'s `rebase_step` does).
#[derive(Resource, Default)]
struct PendingRebase(Option<[f32; 3]>);

/// `StreamSet::Rebase` for the test: shifts root transforms and writes `OriginShifted`.
fn apply_pending_rebase(
    mut pending: ResMut<PendingRebase>,
    mut roots: Query<&mut Transform, Without<ChildOf>>,
    mut shifted: MessageWriter<regia_scene::streaming::OriginShifted>,
) {
    let Some(shift) = pending.0.take() else {
        return;
    };
    for mut transform in &mut roots {
        transform.translation -= Vec3::from_array(shift);
    }
    shifted.write(regia_scene::streaming::OriginShifted {
        rebase: regia_scene::floating_origin::Rebase {
            cell_delta: [10, 0, 0],
            shift,
        },
        origin_cell: [10, 0, 0],
    });
}

/// A car resting 10 km out is rebased to the origin between two fixed steps, the way the streaming
/// runtime does it (roots' `Transform` plus `OriginShifted` in `StreamSet::Rebase`, after the physics
/// step). The wheels' contact record moves with the shift at once, every wheel stays grounded, and
/// no suspension force or compression jumps on the steps after it.
#[test]
fn wheel_contacts_survive_a_rebase_without_a_suspension_spike() {
    use regia_scene::streaming::{RebaseBlockers, RebaseHolds, StreamSet};

    let mut app = vehicle_app();
    app.configure_sets(
        FixedUpdate,
        (StreamSet::Sources, StreamSet::Rebase, StreamSet::Plan).chain(),
    );
    app.init_resource::<RebaseBlockers>()
        .init_resource::<RebaseHolds>()
        .init_resource::<PendingRebase>()
        .add_systems(FixedUpdate, apply_pending_rebase.in_set(StreamSet::Rebase));
    let far = 10_240.0;
    {
        let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
        ground_box(&mut physics, far, 0.0, 200.0, 200.0);
    }
    let config = VehicleConfig::default();
    let at = Vec3::new(far + 3.0, rest_height(&config) + 0.05, 0.0);
    let entity = spawn_vehicle(app.world_mut(), config, at, Quat::IDENTITY).expect("spawn_vehicle");
    for _ in 0..240 {
        app.update();
    }
    // The settled loads and their step-to-step jitter.
    let mut settled = Vec::new();
    for _ in 0..30 {
        app.update();
        settled.push(wheel_loads(&app, entity));
    }
    let wheels = settled[0].len();
    let mean: Vec<(f32, f32)> = (0..wheels)
        .map(|w| {
            let n = settled.len() as f32;
            (
                settled.iter().map(|s| s[w].0).sum::<f32>() / n,
                settled.iter().map(|s| s[w].1).sum::<f32>() / n,
            )
        })
        .collect();
    let jitter: Vec<(f32, f32)> = (0..wheels)
        .map(|w| {
            settled.iter().fold((0.0f32, 0.0f32), |(f, c), s| {
                (
                    f.max((s[w].0 - mean[w].0).abs()),
                    c.max((s[w].1 - mean[w].1).abs()),
                )
            })
        })
        .collect();
    assert!(
        mean.iter().all(|(force, _)| *force > 0.0),
        "settled on four wheels: {mean:?}"
    );
    let contacts_before: Vec<Vec3> = {
        let controller = app.world().get::<VehicleController>(entity).expect("controller");
        let physics = app.world().resource::<PhysicsWorld>();
        controller
            .vehicle
            .wheel_states(physics)
            .expect("wheels")
            .iter()
            .map(|w| w.contact_point.expect("grounded"))
            .collect()
    };

    // The rebase, in the next step's `StreamSet::Rebase`.
    let shift = [far, 0.0, 0.0];
    app.world_mut().resource_mut::<PendingRebase>().0 = Some(shift);
    // One step: the solve runs before the rebase in it, so the contact record the readout shows
    // after it is the one cast before the shift.
    app.update();
    {
        let controller = app.world().get::<VehicleController>(entity).expect("controller");
        let physics = app.world().resource::<PhysicsWorld>();
        let state = controller.vehicle.state(physics).expect("state");
        assert!(
            state.translation.x.abs() < 10.0,
            "the chassis moved: {}",
            state.translation
        );
        for (wheel, before) in state.wheels.iter().zip(&contacts_before) {
            let contact = wheel.contact_point.expect("still grounded");
            assert!(
                (contact - (*before - Vec3::from_array(shift))).length() < 0.05,
                "contact {contact} vs {before} - shift"
            );
            assert!(
                (contact - wheel.world_translation).length() < 1.0,
                "the contact sits under its wheel"
            );
        }
        // The solve's hold is scoped to the solve: nothing blocks the next rebase.
        assert!(app.world().resource::<RebaseBlockers>().0.is_empty());
        assert_eq!(app.world().resource::<RebaseHolds>().iter().count(), 0);
    }
    for step in 0..60 {
        app.update();
        let loads = wheel_loads(&app, entity);
        for (w, (force, compression)) in loads.iter().enumerate() {
            let force_limit = jitter[w].0 + 0.02 * mean[w].0;
            let compression_limit = jitter[w].1 + 0.002;
            assert!(
                (force - mean[w].0).abs() <= force_limit,
                "step {step} wheel {w}: force {force} vs settled {} (limit {force_limit})",
                mean[w].0
            );
            assert!(
                (compression - mean[w].1).abs() <= compression_limit,
                "step {step} wheel {w}: compression {compression} vs settled {}",
                mean[w].1
            );
        }
    }
}

/// `Vehicle::shift_origin` moves the readout's contacts at once; the next update casts afresh.
#[test]
fn shift_origin_moves_the_wheel_contacts_until_the_next_update() {
    let (mut physics, mut vehicle) = settled_car();
    let before: Vec<Vec3> = vehicle
        .wheel_states(&physics)
        .expect("wheels")
        .iter()
        .map(|w| w.contact_point.expect("grounded"))
        .collect();
    let shift = Vec3::new(1_024.0, 0.0, -2_048.0);
    vehicle.shift_origin(shift);
    let after: Vec<Vec3> = vehicle
        .wheel_states(&physics)
        .expect("wheels")
        .iter()
        .map(|w| w.contact_point.expect("grounded"))
        .collect();
    for (a, b) in after.iter().zip(&before) {
        assert_eq!(*a, *b - shift);
    }
    // The world did not move here, so the next cast lands where the contacts were: the shift is
    // dropped once the rays are cast again.
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 1);
    for (wheel, b) in vehicle
        .wheel_states(&physics)
        .expect("wheels")
        .iter()
        .zip(&before)
    {
        assert!((wheel.contact_point.expect("grounded") - *b).length() < 0.01);
    }
}

// ---------------------------------------------------------------------------
// Floating-origin rebase (gap-audit-1102 row 655)
// ---------------------------------------------------------------------------

/// What `shift_physics_origin` does to the bare world's bodies: every body moves by `-shift`.
fn rebase_bodies(physics: &mut PhysicsWorld, shift: Vec3) {
    for (_, body) in physics.bodies.iter_mut() {
        let mut pose = *body.position();
        pose.translation -= to_rapier_vec3(shift);
        body.set_position(pose, false);
    }
    physics
        .bodies
        .propagate_modified_body_positions_to_colliders(&mut physics.colliders);
}

#[test]
fn a_rebase_moves_the_wheel_contacts_with_the_chassis() {
    let (mut physics, mut vehicle) = settled_car();
    let before = vehicle.state(&physics).expect("state");
    let contact_before = before.wheels[0].contact_point.expect("grounded");
    let shift = Vec3::new(1024.0, 0.0, -512.0);

    rebase_bodies(&mut physics, shift);
    vehicle.shift_origin(shift);

    // Read before the next update: the contact is in the new frame, under the wheel.
    let after = vehicle.state(&physics).expect("state");
    for (wheel, old) in after.wheels.iter().zip(&before.wheels) {
        let contact = wheel.contact_point.expect("still grounded");
        let moved = old.contact_point.expect("grounded") - shift;
        assert!(
            contact.abs_diff_eq(moved, 1.0e-2),
            "wheel {}: {contact} vs {moved}",
            wheel.index
        );
        let radius_gap = wheel.world_translation.distance(contact);
        assert!(
            radius_gap < 1.0,
            "wheel {}: contact {radius_gap} m from the wheel",
            wheel.index
        );
    }
    assert!(
        after.wheels[0]
            .contact_point
            .expect("grounded")
            .distance(contact_before)
            > 1000.0
    );

    // A second rebase before any update adds up.
    rebase_bodies(&mut physics, shift);
    vehicle.shift_origin(shift);
    let twice = vehicle.state(&physics).expect("state");
    assert!(
        twice.wheels[0]
            .contact_point
            .expect("grounded")
            .abs_diff_eq(contact_before - shift * 2.0, 1.0e-2),
        "{:?}",
        twice.wheels[0].contact_point
    );
}

/// ADR-7431 (5): every wheel setting has a maximum. An anti-roll bar of 1e30 N/m was already
/// refused (the drivetrain module's 1e9 limit); the wheel settings took any finite value before
/// (a suspension of 1e30 N/m, a wheel radius of 1e30 m), whose products leave `f32` in the first
/// step. Each is refused with `vehicle_invalid_wheel` naming the field. The `Simple` model's
/// vehicle-wide fields keep their v1 rules (`vehicle_drivetrain.rs` pins that).
#[test]
fn settings_past_the_maximum_are_refused_with_their_field() {
    use regia_physics::vehicle_drivetrain::AntiRollBar;
    let huge = 1.0e30_f32;
    let bar = VehicleConfig {
        anti_roll: vec![AntiRollBar {
            left: 0,
            right: 1,
            stiffness: huge,
        }],
        ..VehicleConfig::default()
    };
    let refused = bar.validate().expect_err("1e30 N/m bar");
    assert_eq!(refused, VehicleError::InvalidConfig { field: "anti_roll" });
    assert_eq!(refused.code(), "vehicle_invalid_config");

    for field in [
        "suspension_stiffness",
        "suspension_rest_length",
        "max_suspension_force",
        "friction_slip",
        "radius",
    ] {
        let mut config = VehicleConfig::default();
        let wheel = &mut config.wheels[2];
        match field {
            "suspension_stiffness" => wheel.suspension_stiffness = huge,
            "suspension_rest_length" => wheel.suspension_rest_length = huge,
            "max_suspension_force" => wheel.max_suspension_force = huge,
            "friction_slip" => wheel.friction_slip = huge,
            _ => wheel.radius = huge,
        }
        let refused = config.validate().expect_err(field);
        assert_eq!(
            refused,
            VehicleError::InvalidWheel { index: 2, field },
            "{field} = 1e30"
        );
        assert_eq!(refused.code(), "vehicle_invalid_wheel");
    }
}

/// ADR-7431 (5): the limit itself is a valid setting; one step past it is not.
#[test]
fn the_vehicle_setting_limit_is_inclusive() {
    use regia_physics::vehicle::MAX_VEHICLE_SETTING;
    let mut edge = VehicleConfig::default();
    edge.wheels[0].max_suspension_force = MAX_VEHICLE_SETTING;
    edge.wheels[1].suspension_stiffness = MAX_VEHICLE_SETTING;
    assert_eq!(edge.validate(), Ok(()));
    edge.wheels[1].suspension_stiffness = MAX_VEHICLE_SETTING * 2.0;
    assert_eq!(
        edge.validate(),
        Err(VehicleError::InvalidWheel {
            index: 1,
            field: "suspension_stiffness"
        })
    );
}

/// ADR-7431 (5), rc1 physics review finding 13 (PLAUSIBLE): the slip-angle readout between a
/// rebase and the next update. The contact point is corrected by the pending shift, and the
/// chassis (and ground) moved with the world, so a car sliding and yawing reads the same slip
/// angles right after a rebase as right before it. A wrong correction would put the contact
/// 1 km from the chassis' centre of mass, and the yaw rate times that lever arm would swamp the
/// sideways velocity.
#[test]
fn slip_angles_read_the_same_across_a_rebase_before_the_next_update() {
    let (mut physics, mut vehicle) = settled_car();
    let chassis = vehicle.chassis().to_handle();
    let body = physics.bodies.get_mut(chassis).expect("chassis");
    body.set_linvel(to_rapier_vec3(Vec3::new(2.0, 0.0, -8.0)), true);
    body.set_angvel(to_rapier_vec3(Vec3::new(0.0, 0.6, 0.0)), true);
    let slips = |physics: &PhysicsWorld, vehicle: &Vehicle| -> Vec<f32> {
        vehicle
            .wheel_states(physics)
            .expect("wheels")
            .iter()
            .map(|w| {
                assert!(w.grounded, "wheel {} grounded", w.index);
                w.slip_angle
            })
            .collect()
    };
    let before = slips(&physics, &vehicle);
    assert!(
        before.iter().all(|s| s.abs() > 0.05),
        "the car slides: {before:?}"
    );
    let shift = Vec3::new(1024.0, 0.0, -512.0);
    rebase_bodies(&mut physics, shift);
    vehicle.shift_origin(shift);
    let after = slips(&physics, &vehicle);
    for (wheel, (a, b)) in after.iter().zip(&before).enumerate() {
        assert!((a - b).abs() < 1.0e-3, "wheel {wheel}: {a} after vs {b} before");
    }
}
