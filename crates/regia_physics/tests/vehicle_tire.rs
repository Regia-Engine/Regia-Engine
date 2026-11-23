// SPDX-License-Identifier: MPL-2.0
//! Slip-angle tires (ADR-2719), headless.
//!
//! Every test steps a bare [`PhysicsWorld`] at 60 Hz for a fixed number of steps (no GPU, window or
//! wall clock) and compares against a closed form: the skidpad limit `µg`, the bicycle model's
//! understeer gradient from the authored cornering stiffnesses (Milliken and Milliken, ch. 5), the
//! rigid-body lateral load transfer `m·a·h/t`, the load-sensitivity factor, the friction ellipse,
//! a held car that does not creep, and momentum shared with a free platform.

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder, RigidBodyHandle};
use rapier3d::geometry::ColliderBuilder;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::convert::{from_rapier_vec3, to_rapier_vec3};
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleError, VehicleInput};
use regia_physics::vehicle_drivetrain::{Drivetrain, DrivetrainConfig};
use regia_physics::vehicle_tire::{LateralCurve, SlipTire, TireModel};

const DT: f32 = 1.0 / 60.0;
const G: f32 = 9.81;

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
        to_rapier_vec3(Vec3::new(0.0, -G, 0.0)),
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

/// The default car with these tires front and rear, every wheel driven, no rolling resistance.
fn slip_car(front: SlipTire, rear: SlipTire) -> VehicleConfig {
    let mut config = VehicleConfig {
        rolling_resistance: 0.0,
        ..VehicleConfig::default()
    };
    for (index, wheel) in config.wheels.iter_mut().enumerate() {
        wheel.drive = true;
        wheel.tire_model = TireModel::Slip(if index < 2 { front.clone() } else { rear.clone() });
    }
    config
}

fn spawn(physics: &mut PhysicsWorld, config: VehicleConfig) -> Vehicle {
    Vehicle::spawn(physics, config, Vec3::new(0.0, 0.9, 0.0), Quat::IDENTITY).expect("spawn")
}

fn drive(physics: &mut PhysicsWorld, vehicle: &mut Vehicle, input: VehicleInput, steps: usize) {
    for _ in 0..steps {
        vehicle.update(physics, input, DT).expect("update");
        step(physics);
    }
}

/// A steady-state cornering sample: speed, yaw rate, lateral acceleration and the wheel loads.
#[derive(Debug, Clone, Copy)]
struct Corner {
    speed: f32,
    yaw_rate: f32,
    lateral: f32,
    /// Loads of the wheels on the outside and the inside of the turn (sums).
    outer: f32,
    inner: f32,
    /// Height of the center of mass above the contacts (m).
    height: f32,
    /// Distance between the left and right contacts (m).
    track: f32,
}

/// Holds `speed` m/s with a proportional throttle and the steer input fixed, for `steps` steps,
/// and returns the mean of the last `window` steps. The car turns right.
fn corner(
    physics: &mut PhysicsWorld,
    vehicle: &mut Vehicle,
    steer: f32,
    speed: impl Fn(usize) -> f32,
    steps: usize,
    window: usize,
) -> Corner {
    let mut sum = [0.0_f64; 7];
    for index in 0..steps {
        let state = vehicle.state(physics).expect("state");
        let target = speed(index);
        let throttle = ((target - state.speed) * 0.5).clamp(-1.0, 1.0);
        let input = VehicleInput {
            throttle,
            brake: 0.0,
            steer,
        };
        drive(physics, vehicle, input, 1);
        if index + window >= steps {
            let state = vehicle.state(physics).expect("state");
            let body = &physics.bodies[vehicle.chassis().to_handle()];
            let com = from_rapier_vec3(body.mass_properties().world_com);
            let right = state.rotation * Vec3::X;
            let wheels = &state.wheels;
            let contact = |i: usize| wheels[i].contact_point.unwrap_or(Vec3::ZERO);
            let ground = (0..4).map(|i| contact(i).y).sum::<f32>() / 4.0;
            let track = 0.5 * ((contact(1) - contact(0)).dot(right) + (contact(3) - contact(2)).dot(right));
            // Turning right: the left wheels (0, 2) are outside.
            let load = |i: usize| wheels[i].suspension_force;
            let yaw = -state.angular_velocity.y;
            let values = [
                state.speed,
                yaw,
                state.speed * yaw,
                load(0) + load(2),
                load(1) + load(3),
                com.y - ground,
                track,
            ];
            for (slot, value) in sum.iter_mut().zip(values) {
                *slot += f64::from(value);
            }
        }
    }
    let mean = |i: usize| (sum[i] / window as f64) as f32;
    Corner {
        speed: mean(0),
        yaw_rate: mean(1),
        lateral: mean(2),
        outer: mean(3),
        inner: mean(4),
        height: mean(5),
        track: mean(6),
    }
}

/// The skidpad: a fixed steer and a slowly rising target speed, so the car goes from a steady turn
/// into the tire's limit. Returns the highest one-second mean lateral acceleration.
///
/// `µg` is the limit only when both axles reach their grip together. The drive force that holds the
/// speed against the tires' drag acts at the ground and pitches load off the front axle (about
/// 320 N per front wheel at the limit with the default 0.52 m center-of-mass height), which then
/// saturates first and caps the car near 0.94 g. A low center of mass keeps the axle loads near
/// their static share, as the closed form assumes.
fn skidpad_plateau(mut config: VehicleConfig) -> f32 {
    config.center_of_mass.y = -0.6;
    let mut physics = flat_world();
    let mut vehicle = spawn(&mut physics, config);
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    let steer = 0.24; // 0.12 rad at the wheels: a 22 m circle at low speed
    let mut best = 0.0_f32;
    let mut window = std::collections::VecDeque::new();
    for index in 0..3000 {
        let state = vehicle.state(&physics).expect("state");
        let target = 8.0 + 16.0 * index as f32 / 3000.0;
        let throttle = ((target - state.speed) * 0.5).clamp(-1.0, 1.0);
        drive(
            &mut physics,
            &mut vehicle,
            VehicleInput {
                throttle,
                brake: 0.0,
                steer,
            },
            1,
        );
        let state = vehicle.state(&physics).expect("state");
        assert!(state.translation.is_finite(), "step {index}");
        window.push_back(state.speed * -state.angular_velocity.y);
        if window.len() > 60 {
            window.pop_front();
            best = best.max(window.iter().sum::<f32>() / 60.0);
        }
    }
    best
}

/// A tire with the side force at the center of mass's height: no load transfer, so every wheel
/// keeps a quarter of the weight.
fn level_tire() -> SlipTire {
    SlipTire {
        roll_influence: 0.0,
        ..SlipTire::default()
    }
}

#[test]
fn the_skidpad_lateral_acceleration_plateaus_at_mu_g() {
    for mu in [0.8_f32, 1.0] {
        let tire = SlipTire {
            peak_friction: mu,
            ..level_tire()
        };
        let plateau = skidpad_plateau(slip_car(tire.clone(), tire));
        let want = mu * G;
        assert!(
            (plateau - want).abs() < 0.05 * want,
            "µ {mu}: plateau {plateau} m/s² against µg {want}"
        );
    }
}

#[test]
fn a_raised_load_exponent_lowers_the_plateau_by_the_load_factor() {
    // Every wheel carries a quarter of the weight (no transfer): the grip falls by
    // (load / nominal)^-exponent.
    let load = 1200.0 * G / 4.0;
    let base = SlipTire {
        nominal_load: 1500.0,
        ..level_tire()
    };
    let sensitive = SlipTire {
        load_exponent: 0.3,
        ..base.clone()
    };
    let flat = skidpad_plateau(slip_car(base.clone(), base));
    let lowered = skidpad_plateau(slip_car(sensitive.clone(), sensitive.clone()));
    let predicted = sensitive.load_factor(load);
    assert!((predicted - (load / 1500.0).powf(-0.3)).abs() < 1.0e-5);
    let ratio = lowered / flat;
    assert!(
        (ratio - predicted).abs() < 0.03 * predicted,
        "plateau {flat} → {lowered}: ratio {ratio} against {predicted}"
    );
}

#[test]
fn the_understeer_gradient_matches_the_bicycle_model() {
    // Linear range: a high µ keeps the tires far from their limit at these lateral accelerations.
    let front = SlipTire {
        peak_friction: 3.0,
        cornering_stiffness: 40_000.0,
        ..SlipTire::default()
    };
    let rear = SlipTire {
        cornering_stiffness: 60_000.0,
        ..front.clone()
    };
    let config = slip_car(front.clone(), rear.clone());
    let mass = config.chassis_mass;
    let wheelbase = 2.6_f32;
    let steer_input = 0.08_f32;
    let steer = steer_input * config.max_steer_angle;
    // Each wheel's stiffness grows with its load, so an axle's stiffness is fixed by its weight.
    let axle_weight = mass * G / 2.0;
    let stiffness =
        |tire: &SlipTire| 2.0 * tire.cornering_stiffness * (axle_weight / 2.0) / tire.nominal_load;
    // K = W_f / C_f − W_r / C_r, radians per (m/s²) when divided by g·m/W.
    let gradient = (axle_weight / stiffness(&front) - axle_weight / stiffness(&rear)) / G;
    let mut samples = Vec::new();
    for speed in [10.0_f32, 16.0] {
        let mut physics = flat_world();
        let mut vehicle = spawn(&mut physics, config.clone());
        drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
        let sample = corner(&mut physics, &mut vehicle, steer_input, |_| speed, 900, 120);
        samples.push(sample);
    }
    // δ = L/R + K·a: the slope of (δ − L·r/V) against a between the two speeds.
    let excess = |c: &Corner| steer - wheelbase * c.yaw_rate / c.speed;
    let measured = (excess(&samples[1]) - excess(&samples[0])) / (samples[1].lateral - samples[0].lateral);
    assert!(
        (measured - gradient).abs() < 0.10 * gradient,
        "understeer gradient {measured} rad/(m/s²) against the bicycle model's {gradient} ({samples:?})"
    );
}

#[test]
fn skidpad_load_transfer_matches_the_rigid_body_formula() {
    // Stiff springs keep the roll small, so the center of mass barely moves sideways.
    let stiff = |mut config: VehicleConfig| {
        for wheel in &mut config.wheels {
            wheel.suspension_stiffness = 90.0;
            wheel.suspension_compression_damping = 4.0;
            wheel.suspension_relaxation_damping = 6.0;
        }
        config
    };
    let run = |tire: SlipTire| {
        let mut physics = flat_world();
        let mut vehicle = spawn(&mut physics, stiff(slip_car(tire.clone(), tire)));
        drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
        corner(&mut physics, &mut vehicle, 0.24, |_| 11.0, 900, 120)
    };
    let full = run(SlipTire::default());
    let mass = 1200.0_f32;
    let transfer = 0.5 * (full.outer - full.inner);
    let formula = mass * full.lateral * full.height / full.track;
    assert!(full.lateral > 4.0, "{full:?}");
    assert!(
        (transfer - formula).abs() < 0.10 * formula,
        "transfer {transfer} N against m·a·h/t {formula} N ({full:?})"
    );
    // At the center of mass's height the tires put no roll moment in: no transfer.
    let level = run(level_tire());
    let level_transfer = 0.5 * (level.outer - level.inner);
    assert!(
        level_transfer.abs() < 0.05 * formula,
        "roll_influence 0 still moved {level_transfer} N"
    );
}

#[test]
fn full_braking_in_a_turn_cuts_the_side_force_as_the_ellipse_says() {
    let tire = SlipTire::default();
    let mut config = slip_car(tire.clone(), tire.clone());
    let drivetrain = DrivetrainConfig::default();
    let longitudinal_friction = drivetrain.tire.peak_friction;
    config.drivetrain = Drivetrain::Geared(drivetrain);
    let mut physics = flat_world();
    let mut vehicle = spawn(&mut physics, config);
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    corner(&mut physics, &mut vehicle, 0.2, |_| 14.0, 600, 1);
    let cornering: f32 = vehicle.tire_forces().iter().map(|t| t.lateral_force.abs()).sum();
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: 0.0,
            brake: 1.0,
            steer: 0.2,
        },
        3,
    );
    let mut braked = 0.0;
    for (index, forces) in vehicle.tire_forces().iter().enumerate() {
        assert!(forces.slip && !forces.stuck, "wheel {index}: {forces:?}");
        let along_cap = longitudinal_friction * tire.load_factor(forces.load) * forces.load;
        let used = (forces.longitudinal_force.abs() / along_cap).min(1.0);
        assert!(used > 0.5, "wheel {index} brakes hard: {forces:?}");
        let pure = tire.lateral_force(forces.slip_angle, forces.load);
        let ellipse = pure * (1.0 - used * used).sqrt();
        assert!(
            (forces.lateral_force - ellipse).abs() <= 0.01 * pure.abs() + 1.0,
            "wheel {index}: side force {} against the ellipse's {ellipse} (pure {pure}, used {used})",
            forces.lateral_force
        );
        braked += forces.lateral_force.abs();
    }
    assert!(
        braked < 0.8 * cornering,
        "braked {braked} N against cornering {cornering} N"
    );
}

/// A world tilted by `tilt` and a held car on it, facing −Z in the slope's frame.
fn held_on_slope(config: VehicleConfig, tilt: Quat) -> (PhysicsWorld, Vehicle) {
    let mut physics = PhysicsWorld::default();
    let body = physics.bodies.insert(RigidBodyBuilder::fixed().build());
    let PhysicsWorld {
        bodies, colliders, ..
    } = &mut physics;
    colliders.insert_with_parent(
        ColliderBuilder::cuboid(500.0, 0.5, 500.0)
            .translation(to_rapier_vec3(tilt * Vec3::new(0.0, -0.5, 0.0)))
            .rotation(to_rapier_vec3(tilt.to_scaled_axis()))
            .build(),
        body,
        bodies,
    );
    let vehicle = Vehicle::spawn(&mut physics, config, tilt * Vec3::new(0.0, 0.9, 0.0), tilt).expect("spawn");
    (physics, vehicle)
}

fn held() -> VehicleInput {
    VehicleInput {
        brake: 1.0,
        ..VehicleInput::default()
    }
}

/// How far a held car moves over 600 steps after two seconds to settle.
fn held_drift(config: VehicleConfig, tilt: Quat) -> f32 {
    let (mut physics, mut vehicle) = held_on_slope(config, tilt);
    drive(&mut physics, &mut vehicle, held(), 120);
    let before = vehicle.state(&physics).expect("state").translation;
    drive(&mut physics, &mut vehicle, held(), 600);
    let after = vehicle.state(&physics).expect("state");
    assert!(after.translation.is_finite());
    (after.translation - before).length()
}

#[test]
fn a_held_car_on_15_degrees_does_not_creep() {
    let slope = 15.0_f32.to_radians();
    let tire = SlipTire::default();
    let geared = VehicleConfig {
        drivetrain: Drivetrain::Geared(DrivetrainConfig::default()),
        ..slip_car(tire.clone(), tire.clone())
    };
    let simple = slip_car(tire.clone(), tire.clone());
    // Nose down the hill, and side on to it.
    for (name, config) in [("geared", geared), ("simple", simple)] {
        for (facing, tilt) in [
            ("down", Quat::from_rotation_x(slope)),
            ("across", Quat::from_rotation_z(slope)),
        ] {
            let drift = held_drift(config.clone(), tilt);
            assert!(drift < 1.0e-3, "{name}, {facing}: moved {drift} m in 600 steps");
        }
    }
    // The damper tire with the geared drivetrain creeps (ADR-2598's 3.4 cm/s): the hold is new.
    let damper = VehicleConfig {
        drivetrain: Drivetrain::Geared(DrivetrainConfig::default()),
        ..VehicleConfig::default()
    };
    let creep = held_drift(damper, Quat::from_rotation_x(slope));
    assert!(creep > 0.1, "the damper car moved only {creep} m");
}

#[test]
fn a_held_car_past_its_grip_still_slides() {
    // tan(40°) = 0.84 is beyond µ = 0.6: the hold must let go.
    let tire = SlipTire {
        peak_friction: 0.6,
        ..SlipTire::default()
    };
    let config = VehicleConfig {
        drivetrain: Drivetrain::Geared(DrivetrainConfig {
            tire: regia_physics::vehicle_drivetrain::TireConfig {
                peak_friction: 0.6,
                slide_friction: 0.5,
                ..Default::default()
            },
            ..DrivetrainConfig::default()
        }),
        ..slip_car(tire.clone(), tire)
    };
    let drift = held_drift(config, Quat::from_rotation_x(40.0_f32.to_radians()));
    assert!(drift > 1.0, "moved only {drift} m");
}

/// A 6 t platform on a frictionless floor, free to slide, and its handle.
fn platform_world() -> (PhysicsWorld, RigidBodyHandle) {
    let mut physics = PhysicsWorld::default();
    let floor = physics.bodies.insert(
        RigidBodyBuilder::fixed()
            .translation(to_rapier_vec3(Vec3::new(0.0, -1.5, 0.0)))
            .build(),
    );
    let platform = physics.bodies.insert(
        RigidBodyBuilder::dynamic()
            .translation(to_rapier_vec3(Vec3::new(0.0, -0.5, 0.0)))
            .can_sleep(false)
            .build(),
    );
    let PhysicsWorld {
        bodies, colliders, ..
    } = &mut physics;
    colliders.insert_with_parent(
        ColliderBuilder::cuboid(500.0, 0.5, 500.0).friction(0.0).build(),
        floor,
        bodies,
    );
    colliders.insert_with_parent(
        ColliderBuilder::cuboid(60.0, 0.5, 60.0)
            .friction(0.0)
            .mass(6000.0)
            .build(),
        platform,
        bodies,
    );
    (physics, platform)
}

/// Horizontal momentum of a body (kg·m/s).
fn horizontal_momentum(physics: &PhysicsWorld, body: RigidBodyHandle) -> Vec3 {
    let body = &physics.bodies[body];
    let momentum = from_rapier_vec3(body.linvel()) * body.mass();
    Vec3::new(momentum.x, 0.0, momentum.z)
}

/// Drives a car in a circle on the free platform; returns the momentum changes of the car and of
/// the platform.
fn circle_on_platform(config: VehicleConfig) -> (Vec3, Vec3) {
    let (mut physics, platform) = platform_world();
    let mut vehicle = spawn(&mut physics, config);
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    let chassis = vehicle.chassis().to_handle();
    let car_before = horizontal_momentum(&physics, chassis);
    let platform_before = horizontal_momentum(&physics, platform);
    corner(&mut physics, &mut vehicle, 0.3, |i| 3.0 + i as f32 * 0.02, 300, 1);
    (
        horizontal_momentum(&physics, chassis) - car_before,
        horizontal_momentum(&physics, platform) - platform_before,
    )
}

#[test]
fn a_car_cornering_on_a_free_platform_pushes_it_back() {
    let tire = SlipTire::default();
    let (car, platform) = circle_on_platform(slip_car(tire.clone(), tire));
    assert!(car.length() > 1000.0, "the car's momentum changed by only {car}");
    let sum = car + platform;
    assert!(
        sum.length() < 0.05 * car.length(),
        "car {car} and platform {platform}: not equal and opposite"
    );
    // The damper tire gives the platform nothing (rapier's own limit, unchanged).
    let (damper_car, damper_platform) = circle_on_platform(VehicleConfig::default());
    assert!(damper_car.length() > 1000.0);
    assert!(
        damper_platform.length() < 0.05 * damper_car.length(),
        "{damper_platform}"
    );
}

#[test]
fn a_slip_car_steers_the_right_way_and_reports_its_tires() {
    let tire = SlipTire {
        curve: LateralCurve::MagicFormula {
            shape: 1.3,
            curvature: -0.3,
        },
        ..SlipTire::default()
    };
    let mut physics = flat_world();
    let mut vehicle = spawn(&mut physics, slip_car(tire.clone(), tire));
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    let sample = corner(&mut physics, &mut vehicle, 0.3, |_| 10.0, 600, 60);
    let state = vehicle.state(&physics).expect("state");
    assert!(sample.yaw_rate > 0.1, "a right turn: {sample:?}");
    // Turning right moves the car toward +X from −Z.
    assert!(state.translation.x > 5.0, "{}", state.translation);
    let forces = vehicle.tire_forces();
    assert_eq!(forces.len(), 4);
    for (index, tire) in forces.iter().enumerate() {
        assert!(tire.slip && tire.load > 1000.0, "wheel {index}: {tire:?}");
        // The side force pulls toward the turn's center, the wheel's right.
        assert!(tire.lateral_force > 100.0, "wheel {index}: {tire:?}");
        assert!(tire.slip_angle < 0.0, "wheel {index}: {tire:?}");
    }
    // In the air: no load, no force, no NaN.
    let mut physics = flat_world();
    let tire = SlipTire::default();
    let mut vehicle = Vehicle::spawn(
        &mut physics,
        slip_car(tire.clone(), tire),
        Vec3::new(0.0, 30.0, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: f32::NAN,
            brake: 0.0,
            steer: 1.0,
        },
        5,
    );
    for tire in vehicle.tire_forces() {
        assert_eq!(
            (tire.lateral_force, tire.longitudinal_force, tire.load),
            (0.0, 0.0, 0.0)
        );
    }
}

#[test]
fn damper_cars_report_rapier_s_side_force_and_keep_their_digest_rules() {
    let mut physics = flat_world();
    let mut vehicle = spawn(&mut physics, VehicleConfig::default());
    assert!(vehicle.tire_forces().iter().all(|t| *t == Default::default()));
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    corner(&mut physics, &mut vehicle, 0.4, |_| 8.0, 300, 1);
    let forces = vehicle.tire_forces();
    assert!(forces.iter().all(|t| !t.slip && t.load > 0.0));
    assert!(forces.iter().map(|t| t.lateral_force.abs()).sum::<f32>() > 1000.0);
    assert!(!vehicle.config().has_slip_tires());
}

#[test]
fn a_bad_slip_tire_is_refused_with_its_field() {
    let mut config = slip_car(SlipTire::default(), SlipTire::default());
    if let TireModel::Slip(tire) = &mut config.wheels[2].tire_model {
        tire.cornering_stiffness = f32::NAN;
    }
    assert_eq!(
        config.validate(),
        Err(VehicleError::InvalidWheel {
            index: 2,
            field: "tire_model.cornering_stiffness"
        })
    );
}

/// The `Slip` trajectory digest: a scripted run of a geared car with slip tires.
fn slip_trace() -> u64 {
    let tire = SlipTire {
        load_exponent: 0.15,
        ..SlipTire::default()
    };
    let mut config = slip_car(
        tire.clone(),
        SlipTire {
            curve: LateralCurve::MagicFormula {
                shape: 1.35,
                curvature: -0.2,
            },
            ..tire
        },
    );
    config.drivetrain = Drivetrain::Geared(DrivetrainConfig::default());
    let mut physics = flat_world();
    let mut vehicle = spawn(&mut physics, config);
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for index in 0..600_usize {
        let phase = index / 100;
        let input = VehicleInput {
            throttle: if phase % 3 == 2 { 0.0 } else { 1.0 },
            brake: if phase == 5 { 1.0 } else { 0.0 },
            steer: [0.0, 0.5, -0.7, 0.2, 1.0, 0.3][phase],
        };
        drive(&mut physics, &mut vehicle, input, 1);
        let state = vehicle.state(&physics).expect("state");
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
fn two_slip_runs_give_the_same_trajectory() {
    assert_eq!(slip_trace(), slip_trace());
}

/// Captured on Linux x86_64 at the commit that added slip tires (ADR-2719), re-pinned when the
/// tire math moved to the platform-independent kernels (ADR-4773, audit G finding 7): the
/// angles and powers now come from the deterministic kernels, so the trajectory bits are the
/// same on every host instead of the glibc bits.
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    not(feature = "physics-cross-platform-determinism")
))]
const PIN_SLIP: u64 = 0xb1c5_7498_5338_8708;

#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    not(feature = "physics-cross-platform-determinism")
))]
#[test]
fn the_slip_trajectory_keeps_its_pinned_bits() {
    let trace = slip_trace();
    assert_eq!(trace, PIN_SLIP, "slip trajectory digest {trace:#018x}");
}

#[test]
fn a_held_car_keeps_its_hold_through_a_floating_origin_rebase() {
    // Ground with no body: the stick anchors live in world space and must move with the rebase.
    let slope = 10.0_f32.to_radians();
    let tilt = Quat::from_rotation_x(slope);
    let mut physics = PhysicsWorld::default();
    let ground = physics.colliders.insert(
        ColliderBuilder::cuboid(500.0, 0.5, 500.0)
            .translation(to_rapier_vec3(tilt * Vec3::new(0.0, -0.5, 0.0)))
            .rotation(to_rapier_vec3(tilt.to_scaled_axis()))
            .build(),
    );
    let tire = SlipTire::default();
    let mut vehicle = Vehicle::spawn(
        &mut physics,
        slip_car(tire.clone(), tire),
        tilt * Vec3::new(0.0, 0.9, 0.0),
        tilt,
    )
    .expect("spawn");
    drive(&mut physics, &mut vehicle, held(), 120);
    assert!(vehicle.tire_forces().iter().all(|t| t.stuck));
    // Small enough that the broad phase, refreshed only by a world step, still holds the ground
    // where the first ray after the rebase looks; far enough that a stale anchor breaks the hold.
    let shift = Vec3::new(64.0, 0.0, -32.0);
    let chassis = vehicle.chassis().to_handle();
    let before = vehicle.state(&physics).expect("state").translation - shift;
    {
        let body = &mut physics.bodies[chassis];
        let moved = from_rapier_vec3(body.translation()) - shift;
        body.set_translation(to_rapier_vec3(moved), true);
        let collider = &mut physics.colliders[ground];
        let moved = from_rapier_vec3(collider.translation()) - shift;
        collider.set_translation(to_rapier_vec3(moved));
    }
    vehicle.shift_origin(shift);
    // The very next step still holds: a stale anchor 71 m away would have broken every hold.
    drive(&mut physics, &mut vehicle, held(), 1);
    assert!(
        vehicle.tire_forces().iter().all(|t| t.stuck),
        "{:?}",
        vehicle.tire_forces()
    );
    drive(&mut physics, &mut vehicle, held(), 299);
    let after = vehicle.state(&physics).expect("state").translation;
    assert!(
        (after - before).length() < 1.0e-3,
        "moved {} m across the rebase",
        (after - before).length()
    );
    assert!(vehicle.tire_forces().iter().all(|t| t.stuck));
}
