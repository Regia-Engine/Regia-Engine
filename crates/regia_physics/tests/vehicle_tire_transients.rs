// SPDX-License-Identifier: MPL-2.0
//! Tire transients (ADR-3276), headless: relaxation length, self-aligning torque, camber thrust
//! and the slip-weighted combined model on a whole car.
//!
//! Every test steps a bare [`PhysicsWorld`] at 60 Hz for a fixed number of steps (no GPU, window or
//! wall clock). The single-tire closed forms (the σ/v time constant, the trail curve, the camber
//! thrust formula, the µ·Fz bound) are unit tests in `vehicle_tire.rs`; these check that the car
//! carries them: no low-speed oscillation, released steering that comes back, the thrust of a fixed
//! camber on flat ground, and a pinned trajectory.

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::ColliderBuilder;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::convert::to_rapier_vec3;
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleInput};
use regia_physics::vehicle_drivetrain::{Drivetrain, DrivetrainConfig};
use regia_physics::vehicle_tire::{
    CombinedSlip, LateralCurve, SlipTire, TireModel, steering_torques_into, total_steering_torque,
};

const DT: f32 = 1.0 / 60.0;

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
        to_rapier_vec3(Vec3::new(0.0, -9.81, 0.0)),
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

/// A world with a flat plane at y = 0, 4 km across.
fn flat_world() -> PhysicsWorld {
    let mut physics = PhysicsWorld::default();
    let body = physics.bodies.insert(
        RigidBodyBuilder::fixed()
            .translation(to_rapier_vec3(Vec3::new(0.0, -0.5, 0.0)))
            .build(),
    );
    let PhysicsWorld {
        bodies, colliders, ..
    } = &mut physics;
    colliders.insert_with_parent(ColliderBuilder::cuboid(2000.0, 0.5, 2000.0).build(), body, bodies);
    physics
}

/// The default car with these tires front and rear, no rolling resistance.
fn slip_car(front: SlipTire, rear: SlipTire) -> VehicleConfig {
    let mut config = VehicleConfig {
        rolling_resistance: 0.0,
        ..VehicleConfig::default()
    };
    for (index, wheel) in config.wheels.iter_mut().enumerate() {
        wheel.tire_model = TireModel::Slip(if index < 2 { front.clone() } else { rear.clone() });
    }
    config
}

/// A car settled on its springs, then pushed to `speed` m/s straight ahead (−Z).
fn rolling(config: VehicleConfig, speed: f32) -> (PhysicsWorld, Vehicle) {
    let mut physics = flat_world();
    let mut vehicle =
        Vehicle::spawn(&mut physics, config, Vec3::new(0.0, 0.9, 0.0), Quat::IDENTITY).expect("spawn");
    for _ in 0..60 {
        vehicle
            .update(&mut physics, VehicleInput::default(), DT)
            .expect("update");
        step(&mut physics);
    }
    if let Some(body) = physics.bodies.get_mut(vehicle.chassis().to_handle()) {
        body.set_linvel(to_rapier_vec3(Vec3::new(0.0, 0.0, -speed)), true);
    }
    (physics, vehicle)
}

fn drive(physics: &mut PhysicsWorld, vehicle: &mut Vehicle, input: VehicleInput) {
    vehicle.update(physics, input, DT).expect("update");
    step(physics);
}

/// How often the step-to-step change of `values` reverses: a ringing signal reverses nearly every
/// step, a settling one a few times. Changes under `floor` are noise and skipped.
fn reversals(values: &[f32], floor: f32) -> usize {
    let mut last = 0.0_f32;
    let mut count = 0;
    for pair in values.windows(2) {
        let change = pair[1] - pair[0];
        if change.abs() < floor {
            continue;
        }
        if last != 0.0 && change.signum() != last {
            count += 1;
        }
        last = change.signum();
    }
    count
}

/// The front-left lateral force and the yaw rate of a car rolling at 0.5 m/s with the wheels
/// turned, over 600 steps.
fn walking_turn(tire: SlipTire) -> (Vec<f32>, Vec<f32>) {
    let (mut physics, mut vehicle) = rolling(slip_car(tire.clone(), tire), 0.5);
    let input = VehicleInput {
        throttle: 0.0,
        brake: 0.0,
        steer: 0.6,
    };
    let (mut force, mut yaw) = (Vec::new(), Vec::new());
    for _ in 0..600 {
        drive(&mut physics, &mut vehicle, input);
        let state = vehicle.state(&physics).expect("state");
        force.push(vehicle.tire_forces()[0].lateral_force);
        yaw.push(state.angular_velocity.y);
        assert!(state.translation.is_finite() && state.angular_velocity.is_finite());
    }
    (force, yaw)
}

#[test]
fn a_relaxed_car_turning_at_walking_speed_neither_rings_nor_grows() {
    let no_lag = SlipTire {
        stick_speed: 0.0,
        ..SlipTire::default()
    };
    let (control_force, control_yaw) = walking_turn(no_lag.clone());
    for (sigma, stiffness) in [(0.3_f32, 45_000.0_f32), (1.0, 45_000.0), (0.3, 400_000.0)] {
        let tire = SlipTire {
            relaxation_length: sigma,
            cornering_stiffness: stiffness,
            ..no_lag.clone()
        };
        let (force, yaw) = walking_turn(tire);
        let case = format!("σ {sigma} C {stiffness}");
        let early = force[..300].iter().fold(0.0_f32, |m, f| m.max(f.abs()));
        let late = force[300..].iter().fold(0.0_f32, |m, f| m.max(f.abs()));
        assert!(
            late <= early * 1.05 + 1.0,
            "{case}: the force grew {early} -> {late}"
        );
        let yaw_peak = yaw.iter().fold(0.0_f32, |m, y| m.max(y.abs()));
        let yaw_late = yaw[300..].iter().fold(0.0_f32, |m, y| m.max(y.abs()));
        assert!(yaw_late <= yaw_peak, "{case}");
        // No more ringing than the car without a lag (the suspension settles in both).
        let ring = reversals(&force[60..], 1.0);
        let control = reversals(&control_force[60..], 1.0);
        assert!(
            ring <= control + 6,
            "{case}: {ring} reversals, the no-lag car {control}"
        );
        let yaw_ring = reversals(&yaw[60..], 1.0e-4);
        let yaw_control = reversals(&control_yaw[60..], 1.0e-4);
        assert!(
            yaw_ring <= yaw_control + 6,
            "{case}: yaw {yaw_ring} vs {yaw_control}"
        );
    }
}

#[test]
fn released_steering_returns_toward_center() {
    for curve in [
        LateralCurve::Brush,
        LateralCurve::MagicFormula {
            shape: 1.3,
            curvature: -0.3,
        },
    ] {
        for relaxation_length in [0.0_f32, 0.5] {
            let front = SlipTire {
                curve,
                pneumatic_trail: 0.04,
                relaxation_length,
                ..SlipTire::default()
            };
            let config = slip_car(front, SlipTire::default());
            let max_steer = config.max_steer_angle;
            let (mut physics, mut vehicle) = rolling(config, 10.0);
            let held = 0.12_f32; // radians at the wheels, left
            let input = |angle: f32| VehicleInput {
                throttle: 0.0,
                brake: 0.0,
                steer: -angle / max_steer,
            };
            for _ in 0..90 {
                drive(&mut physics, &mut vehicle, input(held));
            }
            let torque = total_steering_torque(vehicle.config(), &vehicle.tire_forces());
            let case = format!("{curve:?} σ {relaxation_length}");
            // In a left turn the front contacts slide right: the torque turns the wheels right.
            assert!(torque < -1.0, "{case}: held torque {torque}");
            let mut axles = Vec::new();
            steering_torques_into(vehicle.config(), &vehicle.tire_forces(), &mut axles);
            assert_eq!(axles.len(), 1, "{case}");
            assert!((axles[0].torque - torque).abs() < 1.0e-3, "{case}");
            // A free steering model: inertia and damping about the steering axis, no driver.
            let (inertia, damping) = (5.0_f32, 60.0_f32);
            let (mut angle, mut rate) = (held, 0.0_f32);
            let mut smallest = held;
            for _ in 0..120 {
                drive(&mut physics, &mut vehicle, input(angle));
                let torque = total_steering_torque(vehicle.config(), &vehicle.tire_forces());
                rate += (torque - damping * rate) / inertia * DT;
                angle = (angle + rate * DT).clamp(-max_steer, max_steer);
                smallest = smallest.min(angle.abs());
            }
            assert!(smallest < 0.5 * held, "{case}: the wheels stayed at {smallest}");
            assert!(angle.abs() < 0.5 * held, "{case}: they ended at {angle}");
        }
    }
}

#[test]
fn camber_thrust_on_flat_ground_matches_the_closed_form() {
    let tire = SlipTire {
        camber_stiffness: 4500.0,
        static_camber: -0.03,
        load_exponent: 0.1,
        ..SlipTire::default()
    };
    let (mut physics, mut vehicle) = rolling(slip_car(tire.clone(), tire.clone()), 10.0);
    for _ in 0..30 {
        drive(&mut physics, &mut vehicle, VehicleInput::default());
    }
    let forces = vehicle.tire_forces();
    for (index, force) in forces.iter().enumerate() {
        // Left wheels (0, 2) lean right, toward the center line; right wheels lean left.
        let want_camber = if index % 2 == 0 { 0.03 } else { -0.03 };
        assert!(
            (force.camber - want_camber).abs() < 2.0e-3,
            "wheel {index}: camber {} (roll and pitch stay small on flat ground)",
            force.camber
        );
        let want = tire.camber_stiffness_at(force.load) * force.camber;
        assert!(
            (force.camber_thrust - want).abs() < 1.0e-3 * want.abs(),
            "wheel {index}: thrust {} vs {want}",
            force.camber_thrust
        );
        assert!(
            force.camber_thrust * want_camber > 0.0,
            "wheel {index}: toward the lean"
        );
    }
    // The two sides cancel: the car keeps going straight.
    let state = vehicle.state(&physics).expect("state");
    assert!(state.translation.x.abs() < 0.05, "{}", state.translation);
}

#[test]
fn every_transient_at_its_default_changes_no_bit() {
    let explicit = SlipTire {
        relaxation_length: 0.0,
        pneumatic_trail: 0.0,
        camber_stiffness: 0.0,
        static_camber: 0.0,
        combined_slip: CombinedSlip::Ellipse,
        ..SlipTire::default()
    };
    assert_eq!(explicit, SlipTire::default());
    // Static camber alone (no camber stiffness) and a trail alone (a readout) move nothing either.
    let readout_only = SlipTire {
        static_camber: -0.04,
        pneumatic_trail: 0.03,
        ..SlipTire::default()
    };
    let run = |tire: SlipTire| {
        let (mut physics, mut vehicle) = rolling(slip_car(tire.clone(), tire), 8.0);
        for index in 0..240 {
            let steer = if index < 120 { 0.4 } else { -0.2 };
            drive(
                &mut physics,
                &mut vehicle,
                VehicleInput {
                    throttle: 0.3,
                    brake: 0.0,
                    steer,
                },
            );
        }
        let state = vehicle.state(&physics).expect("state");
        (
            state.translation.to_array().map(f32::to_bits),
            state.rotation.to_array().map(f32::to_bits),
            vehicle.state_digest(),
        )
    };
    let base = run(SlipTire::default());
    assert_eq!(run(readout_only), base);
}

/// The transient trajectory digest: a scripted run of a geared car whose tires use every
/// transient (relaxation, trail, camber, the slip-weighted rear).
fn transient_trace() -> u64 {
    let front = SlipTire {
        relaxation_length: 0.6,
        pneumatic_trail: 0.04,
        camber_stiffness: 4000.0,
        static_camber: -0.02,
        load_exponent: 0.1,
        ..SlipTire::default()
    };
    let rear = SlipTire {
        curve: LateralCurve::MagicFormula {
            shape: 1.35,
            curvature: -0.2,
        },
        combined_slip: CombinedSlip::SlipWeighted {
            shape: 0.9,
            stiffness: 8.0,
        },
        ..front.clone()
    };
    let mut config = slip_car(front, rear);
    for wheel in &mut config.wheels {
        wheel.drive = true;
    }
    config.drivetrain = Drivetrain::Geared(DrivetrainConfig::default());
    let mut physics = flat_world();
    let mut vehicle =
        Vehicle::spawn(&mut physics, config, Vec3::new(0.0, 0.9, 0.0), Quat::IDENTITY).expect("spawn");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for index in 0..600_usize {
        let phase = index / 100;
        let input = VehicleInput {
            throttle: if phase % 3 == 2 { 0.0 } else { 1.0 },
            brake: if phase == 5 { 1.0 } else { 0.0 },
            steer: [0.0, 0.5, -0.7, 0.2, 1.0, 0.3][phase],
        };
        drive(&mut physics, &mut vehicle, input);
        let state = vehicle.state(&physics).expect("state");
        let torque = total_steering_torque(vehicle.config(), &vehicle.tire_forces());
        let words = [
            state.translation.x,
            state.translation.y,
            state.translation.z,
            state.rotation.x,
            state.rotation.y,
            state.rotation.z,
            state.rotation.w,
            state.linear_velocity.x,
            state.linear_velocity.z,
            torque,
        ]
        .map(f32::to_bits)
        .into_iter()
        .chain([
            vehicle.state_digest() as u32,
            (vehicle.state_digest() >> 32) as u32,
        ]);
        for word in words {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash
}

#[test]
fn two_transient_runs_give_the_same_trajectory() {
    assert_eq!(transient_trace(), transient_trace());
}

/// Linux x86_64 after Physics1 deterministic math (ADR-4773).
///
/// Was `0x7b6a_597b_8cb2_230d` while the v4 tire path called the platform `tan`, `atan`, `cos`,
/// `asin` and `exp`. Those calls now go through the deterministic kernels, and the relaxation
/// gain is `1 − e^(−x)` in f64 (ADR-5128). audfix-physics-g measured `0x79ff_136f_98e0_d47c` on an
/// older base without the f64 gain. integ-v047d measured `0xd8f4_e36a_0659_a01e` on the build server
/// (glibc x86_64) at integ-v046 `908e4e926`, before the audfix-physics-g merge and after it:
/// the same value at all three, so the integ-v046 tire code moved it and the merges did not.
///
/// Now `0xdbbe_45de_a132_27b8`: corr-low-anim-phys's tire fixes (58e6e3c59: a grip-clamped side
/// force scales its camber part with the clamp; a slow sliding tire uses the brush trail at its
/// slip angle) moved it. integ-v049-dx12 traced it on the build server: integ-v049 `f0e274281` (before the
/// corr-low merge) `0xd8f4_e36a_0659_a01e`; the merge `66629fa08` and this line's head
/// `0xdbbe_45de_a132_27b8`; the head with only those two tire hunks reverted
/// `0xd8f4_e36a_0659_a01e` again (ADR-7298).
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    not(feature = "physics-cross-platform-determinism")
))]
const PIN_TRANSIENT: u64 = 0xdbbe_45de_a132_27b8;

#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    not(feature = "physics-cross-platform-determinism")
))]
#[test]
fn the_transient_trajectory_keeps_its_pinned_bits() {
    let trace = transient_trace();
    assert_eq!(trace, PIN_TRANSIENT, "transient trajectory digest {trace:#018x}");
}
