// SPDX-License-Identifier: MPL-2.0
//! `.vehicle.ron` v1 compatibility pin (ADR-2598): a v1 file migrates to the `Simple` drivetrain,
//! which must reproduce `max_engine_force` exactly. The digests below were captured on the commit
//! before the drivetrain existed (integ/v035 94d4d501e), so a change in the bits of a v1 car's run
//! fails here. The numbers are per target (rapier's floats go through libm), like the determinism
//! matrix; the second test compares against the plain controller mapping on every target.

use rapier3d::control::{DynamicRayCastVehicleController, WheelTuning};
use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::ColliderBuilder;
use rapier3d::pipeline::QueryFilter;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::convert::to_rapier_vec3;
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleInput};
use regia_physics::vehicle_asset::VehicleAsset;

const DT: f32 = 1.0 / 60.0;
const STEPS: usize = 600;

/// `samples/third_person`'s car, which stays a v1 file.
const THIRD_PERSON_CAR: &str = include_str!("../../../samples/third_person/assets/car.vehicle.ron");
/// A v1 file with every drive flag, a layer mask and tuned limits.
const RALLY_CAR: &str = r#"(
    format_version: 1,
    name: "rally car",
    config: (
        chassis_mass: 1100.0,
        max_engine_force: 4200.0,
        max_steer_angle: 0.45,
        ground_layers: Some(3),
        query_layer: None,
        wheels: [
            (position: (-0.8, -0.2, -1.3), steer: true, drive: true, brake: true),
            (position: (0.8, -0.2, -1.3), steer: true, drive: true, brake: true),
            (position: (-0.8, -0.2, 1.3), drive: true, brake: true),
            (position: (0.8, -0.2, 1.3), drive: true, brake: true),
        ],
    ),
)"#;

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

/// A world with a 2 km flat plane at y = 0.
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
    colliders.insert_with_parent(ColliderBuilder::cuboid(1000.0, 0.5, 1000.0).build(), body, bodies);
    physics
}

/// Launch, weave, brake, reverse, then coast.
fn script(i: usize) -> VehicleInput {
    let t = i as f32 / 60.0;
    VehicleInput {
        throttle: match i {
            0..=239 => 1.0,
            240..=299 => 0.4,
            300..=379 => 0.0,
            380..=479 => -0.6,
            _ => 0.0,
        },
        brake: if (300..380).contains(&i) { 0.8 } else { 0.0 },
        steer: (t * 2.1).sin() * if i < 300 { 0.8 } else { 0.3 },
    }
}

/// FNV-1a over a run: the world digest of every step and each wheel's spin, suspension length and
/// grounded flag.
fn trace(config: VehicleConfig) -> u64 {
    let mut physics = flat_world();
    let at = Vec3::new(1.5, 1.0, -4.0);
    let mut vehicle = Vehicle::spawn(&mut physics, config, at, Quat::from_rotation_y(0.4)).expect("spawn");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut write = |word: u64| {
        for byte in word.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for i in 0..STEPS {
        vehicle.update(&mut physics, script(i), DT).expect("update");
        step(&mut physics);
        write(physics.state_digest());
        for wheel in vehicle.wheel_states(&physics).expect("wheels") {
            write(u64::from(wheel.spin_angle.to_bits()));
            write(u64::from(wheel.suspension_length.to_bits()));
            write(u64::from(wheel.grounded));
        }
    }
    hash
}

fn load(text: &str) -> VehicleConfig {
    VehicleAsset::from_ron(text).expect("v1 file loads").into_config()
}

#[test]
fn v1_cars_keep_their_exact_bits() {
    let default_car = trace(VehicleConfig::default());
    let third_person = trace(load(THIRD_PERSON_CAR));
    let rally = trace(load(RALLY_CAR));
    println!("digests default {default_car:#x} third_person {third_person:#x} rally {rally:#x}");
    // The third-person car is the default car.
    assert_eq!(default_car, third_person);
    assert_ne!(default_car, rally, "the tuned car drives differently");
    #[cfg(all(
        target_arch = "x86_64",
        target_os = "linux",
        not(feature = "physics-cross-platform-determinism")
    ))]
    {
        assert_eq!(default_car, PIN_DEFAULT, "the default car's bits moved");
        assert_eq!(rally, PIN_RALLY, "the v1 rally car's bits moved");
    }
}

/// Captured at integ/v035 94d4d501e, before the drivetrain.
#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    not(feature = "physics-cross-platform-determinism")
))]
const PIN_DEFAULT: u64 = 0xf854_46ba_dc0d_b755;
/// Captured at integ/v035 94d4d501e, before the drivetrain.
#[cfg(all(
    target_arch = "x86_64",
    target_os = "linux",
    not(feature = "physics-cross-platform-determinism")
))]
const PIN_RALLY: u64 = 0x6faa_fb90_d6c5_7328;

/// The mapping `max_engine_force` always had, written against rapier directly: the migrated car
/// must match it step for step on every target.
#[test]
fn the_simple_drivetrain_is_the_plain_controller_mapping() {
    let config = VehicleConfig::default();
    let at = Vec3::new(1.5, 1.0, -4.0);
    let rotation = Quat::from_rotation_y(0.4);

    let mut wrapped_world = flat_world();
    let mut vehicle = Vehicle::spawn(&mut wrapped_world, config.clone(), at, rotation).expect("spawn");
    let mut plain_world = flat_world();
    let plain = Vehicle::spawn(&mut plain_world, config.clone(), at, rotation).expect("spawn");
    let chassis = plain.chassis().to_handle();
    let mut controller = DynamicRayCastVehicleController::new(chassis);
    controller.index_up_axis = 1;
    controller.index_forward_axis = 2;
    for wheel in &config.wheels {
        controller.add_wheel(
            to_rapier_vec3(wheel.position),
            to_rapier_vec3(Vec3::NEG_Y),
            to_rapier_vec3(Vec3::X),
            wheel.suspension_rest_length,
            wheel.radius,
            &WheelTuning {
                suspension_stiffness: wheel.suspension_stiffness,
                suspension_compression: wheel.suspension_compression_damping,
                suspension_damping: wheel.suspension_relaxation_damping,
                max_suspension_travel: wheel.suspension_travel,
                side_friction_stiffness: wheel.side_friction_stiffness,
                friction_slip: wheel.friction_slip,
                max_suspension_force: wheel.max_suspension_force,
            },
        );
    }
    for i in 0..STEPS {
        let input = script(i).sanitized();
        vehicle.update(&mut wrapped_world, input, DT).expect("update");
        step(&mut wrapped_world);

        let steering = -input.steer * config.max_steer_angle;
        let engine = input.throttle * config.max_engine_force;
        let brake = input.brake * config.max_brake_force * DT;
        let rolling = config.rolling_resistance * DT;
        for (wheel, tuned) in controller.wheels_mut().iter_mut().zip(&config.wheels) {
            let braking = tuned.brake && brake > 0.0;
            let driving = tuned.drive && !braking && engine != 0.0;
            wheel.steering = if tuned.steer { steering } else { 0.0 };
            wheel.engine_force = if driving { engine } else { 0.0 };
            wheel.brake = if braking {
                brake
            } else if driving {
                0.0
            } else {
                rolling
            };
        }
        let filter = QueryFilter::default()
            .exclude_sensors()
            .exclude_rigid_body(chassis);
        let queries = plain_world.query_pipeline_mut(filter);
        controller.update_vehicle(DT, queries);
        step(&mut plain_world);
        assert_eq!(
            wrapped_world.state_digest(),
            plain_world.state_digest(),
            "diverged at step {i}"
        );
    }
}
