// SPDX-License-Identifier: MPL-2.0
//! Geared drivetrain, anti-roll, aero and driver aids (ADR-2598), headless.
//!
//! The pure tests drive [`DrivetrainState::step`] with chosen contacts. The world tests step a bare
//! [`PhysicsWorld`] at 60 Hz and compare against the closed-form answers from Gillespie (force
//! balance, drag, rolling resistance) and Milliken and Milliken (roll stiffness). Every test runs a
//! fixed number of steps; no GPU, window or wall clock.

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::ColliderBuilder;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::convert::to_rapier_vec3;
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleError, VehicleInput, WheelConfig};
use regia_physics::vehicle_drivetrain::{
    AbsConfig, AeroConfig, AidsConfig, AntiRollBar, ClutchConfig, Differential, DriveLayout, Drivetrain,
    DrivetrainConfig, DrivetrainInput, DrivetrainState, EngineConfig, GearboxConfig, ShiftRequest,
    TireConfig, TractionControlConfig, WheelContact,
};

const DT: f32 = 1.0 / 60.0;
const G: f32 = 9.81;
const RPM_PER_RAD_S: f32 = 60.0 / std::f32::consts::TAU;

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

/// A world with a 30 km flat plane at y = 0, long enough for a top-speed run.
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
    colliders.insert_with_parent(
        ColliderBuilder::cuboid(15_000.0, 0.5, 15_000.0).build(),
        body,
        bodies,
    );
    physics
}

fn geared(drivetrain: DrivetrainConfig) -> VehicleConfig {
    VehicleConfig {
        drivetrain: Drivetrain::Geared(drivetrain),
        ..VehicleConfig::default()
    }
}

/// A car settled on its springs for one second at the origin, facing −Z.
fn settled(config: VehicleConfig) -> (PhysicsWorld, Vehicle) {
    let mut physics = flat_world();
    let mut vehicle =
        Vehicle::spawn(&mut physics, config, Vec3::new(0.0, 0.9, 0.0), Quat::IDENTITY).expect("spawn");
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    (physics, vehicle)
}

fn drive(physics: &mut PhysicsWorld, vehicle: &mut Vehicle, input: VehicleInput, steps: usize) {
    for _ in 0..steps {
        vehicle.update(physics, input, DT).expect("update");
        step(physics);
    }
}

fn throttle(amount: f32) -> VehicleInput {
    VehicleInput {
        throttle: amount,
        ..VehicleInput::default()
    }
}

fn speed(physics: &PhysicsWorld, vehicle: &Vehicle) -> f32 {
    vehicle.state(physics).expect("state").forward_speed
}

/// Drives forward at full throttle until `target` m/s (at most `limit` steps).
fn drive_to_speed(physics: &mut PhysicsWorld, vehicle: &mut Vehicle, target: f32, limit: usize) {
    for _ in 0..limit {
        if speed(physics, vehicle) >= target {
            return;
        }
        drive(physics, vehicle, throttle(1.0), 1);
    }
    panic!("never reached {target} m/s: {}", speed(physics, vehicle));
}

/// An engine with one flat torque.
fn flat_engine(torque: f32) -> EngineConfig {
    EngineConfig {
        torque_curve: vec![(500.0, torque), (20_000.0, torque)],
        idle_rpm: 800.0,
        redline_rpm: 20_000.0,
        ..EngineConfig::default()
    }
}

/// A gearbox with one forward gear.
fn one_gear(ratio: f32) -> GearboxConfig {
    GearboxConfig {
        forward_ratios: vec![ratio],
        final_drive: 1.0,
        efficiency: 1.0,
        upshift_rpm: 19_000.0,
        downshift_rpm: 1000.0,
        ..GearboxConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Acceptance: top speed
// ---------------------------------------------------------------------------

#[test]
fn top_speed_settles_where_the_wheel_force_equals_drag_plus_rolling_resistance() {
    // One gear and a flat torque, so the balance has a closed form:
    // T·G/r = ½ρCdA v² + Crr·m·g.
    let torque = 300.0;
    let ratio = 3.0;
    let radius = WheelConfig::default().radius;
    let aero = AeroConfig {
        air_density: 1.225,
        drag_coefficient: 1.0,
        frontal_area: 2.44,
        ..AeroConfig::default()
    };
    let crr = 0.015;
    let config = VehicleConfig {
        aero: Some(aero.clone()),
        ..geared(DrivetrainConfig {
            engine: flat_engine(torque),
            gearbox: one_gear(ratio),
            tire: TireConfig {
                rolling_coefficient: crr,
                ..TireConfig::default()
            },
            ..DrivetrainConfig::default()
        })
    };
    let mass = config.chassis_mass;
    let wheel_force = torque * ratio / radius;
    let rolling = crr * mass * G;
    let k = 0.5 * aero.air_density * aero.drag_coefficient * aero.frontal_area;
    let analytic = ((wheel_force - rolling) / k).sqrt();
    assert!(analytic > 35.0 && analytic < 45.0, "{analytic}");

    let (mut physics, mut vehicle) = settled(config);
    drive(&mut physics, &mut vehicle, throttle(1.0), 90 * 60);
    let first = speed(&physics, &vehicle);
    drive(&mut physics, &mut vehicle, throttle(1.0), 60);
    let second = speed(&physics, &vehicle);
    assert!(
        (second - first).abs() < 0.02,
        "still accelerating: {first} -> {second}"
    );
    let error = (second - analytic).abs() / analytic;
    assert!(
        error < 0.02,
        "top speed {second} vs {analytic} ({:.2} %)",
        error * 100.0
    );
}

#[test]
fn a_geared_car_tops_out_on_its_own_curve_and_ratio() {
    let aero = AeroConfig {
        drag_coefficient: 1.0,
        frontal_area: 2.0,
        ..AeroConfig::default()
    };
    let drivetrain = DrivetrainConfig::default();
    let config = VehicleConfig {
        aero: Some(aero.clone()),
        ..geared(drivetrain.clone())
    };
    let mass = config.chassis_mass;
    let radius = config.wheels[0].radius;
    let (mut physics, mut vehicle) = settled(config);
    drive(&mut physics, &mut vehicle, throttle(1.0), 80 * 60);
    let state = vehicle.state(&physics).expect("state");
    assert!(state.gear >= 4, "gear {}", state.gear);

    // The balance in the gear it ended in, solved by bisection on the published curve.
    let ratio = drivetrain.gearbox.total_ratio(state.gear);
    let k = 0.5 * aero.air_density * aero.drag_coefficient * aero.frontal_area;
    let rolling = drivetrain.tire.rolling_coefficient * mass * G;
    let surplus = |v: f32| {
        let rpm = v / radius * ratio * RPM_PER_RAD_S;
        drivetrain.engine.torque_at(rpm) * ratio * drivetrain.gearbox.efficiency / radius
            - rolling
            - k * v * v
    };
    let (mut low, mut high) = (1.0_f32, 120.0_f32);
    for _ in 0..60 {
        let middle = 0.5 * (low + high);
        if surplus(middle) > 0.0 {
            low = middle;
        } else {
            high = middle;
        }
    }
    let error = (state.forward_speed - low).abs() / low;
    assert!(
        error < 0.02,
        "top speed {} vs {low} in gear {} ({:.2} %)",
        state.forward_speed,
        state.gear,
        error * 100.0
    );
}

// ---------------------------------------------------------------------------
// Acceptance: the gearbox
// ---------------------------------------------------------------------------

#[test]
fn the_automatic_shifts_up_at_the_threshold_rpm_and_engages_after_the_shift_time() {
    // An engine the tires can take, so the rpm climbs with the ground speed: a spinning wheel holds
    // the gear past the threshold on purpose (next test).
    let drivetrain = DrivetrainConfig {
        engine: EngineConfig {
            torque_curve: vec![(1000.0, 90.0), (4500.0, 150.0), (7000.0, 110.0)],
            ..EngineConfig::default()
        },
        ..DrivetrainConfig::default()
    };
    let threshold = drivetrain.gearbox.upshift_rpm;
    let shift_steps = (drivetrain.gearbox.shift_time / DT).round() as usize;
    let (mut physics, mut vehicle) = settled(geared(drivetrain.clone()));

    let mut shifts = 0;
    let mut peak_rpm_before = 0.0_f32;
    for _ in 0..40 * 60 {
        let before = vehicle.drivetrain().expect("geared").upshifts();
        drive(&mut physics, &mut vehicle, throttle(1.0), 1);
        let state = vehicle.drivetrain().expect("geared");
        peak_rpm_before = peak_rpm_before.max(state.rpm());
        if state.upshifts() == before {
            continue;
        }
        shifts += 1;
        let at = state.last_shift_rpm();
        // One step of engine acceleration past the threshold at the most.
        assert!(
            at >= threshold && at < threshold + 150.0,
            "shift {shifts} began at {at} rpm, threshold {threshold}"
        );
        let from = state.gear();
        let to = state.target_gear();
        assert_eq!(to, from + 1);
        assert!(state.is_shifting());
        // The old gear holds for the shift time, then the new one is in and the engine has dropped
        // towards the new gear's speed.
        drive(&mut physics, &mut vehicle, throttle(1.0), shift_steps - 1);
        assert_eq!(vehicle.drivetrain().expect("geared").gear(), from);
        drive(&mut physics, &mut vehicle, throttle(1.0), 2);
        let after = vehicle.drivetrain().expect("geared");
        assert_eq!(after.gear(), to);
        assert!(!after.is_shifting());
        assert!(after.rpm() < threshold);
        peak_rpm_before = 0.0;
        if shifts == 3 {
            break;
        }
    }
    assert_eq!(shifts, 3, "peak rpm since the last shift: {peak_rpm_before}");
}

#[test]
fn a_spinning_wheel_does_not_run_the_automatic_up_through_the_box() {
    // Far more engine than grip: the rpm reaches the threshold within a second, at walking pace.
    let drivetrain = DrivetrainConfig {
        tire: TireConfig {
            peak_friction: 0.35,
            slide_friction: 0.2,
            ..TireConfig::default()
        },
        ..DrivetrainConfig::default()
    };
    let (mut physics, mut vehicle) = settled(geared(drivetrain));
    drive(&mut physics, &mut vehicle, throttle(1.0), 2 * 60);
    let state = vehicle.drivetrain().expect("geared");
    assert!(state.rpm() > 6200.0, "{}", state.rpm());
    assert_eq!(state.gear(), 1, "it holds first gear");
    assert_eq!(state.upshifts(), 0);
}

#[test]
fn rpm_follows_the_driven_wheels_through_the_ratios() {
    let drivetrain = DrivetrainConfig::default();
    let (mut physics, mut vehicle) = settled(geared(drivetrain.clone()));
    drive_to_speed(&mut physics, &mut vehicle, 12.0, 20 * 60);
    drive(&mut physics, &mut vehicle, throttle(0.3), 60);
    let state = vehicle.drivetrain().expect("geared");
    assert!(!state.is_shifting() && !state.is_clutch_slipping());
    let wheels = state.wheels();
    let carrier = 0.5 * (wheels[2].angular_speed + wheels[3].angular_speed);
    let expected = carrier * drivetrain.gearbox.total_ratio(state.gear()) * RPM_PER_RAD_S;
    assert!(
        (state.rpm() - expected).abs() < 1.0,
        "{} rpm vs {expected} through gear {}",
        state.rpm(),
        state.gear()
    );
    // And the wheels roll with the ground, within the small drive slip.
    let ground = speed(&physics, &vehicle);
    let radius = VehicleConfig::default().wheels[2].radius;
    assert!(
        (wheels[2].angular_speed * radius - ground).abs() < 0.5,
        "{ground}"
    );
    // The readout carries them.
    let readout = vehicle.state(&physics).expect("state");
    assert_eq!(readout.rpm, state.rpm());
    assert_eq!(readout.gear, state.gear());
    assert!(
        (readout.speed - ground).abs() < 0.05,
        "{} vs {ground}",
        readout.speed
    );
}

#[test]
fn braking_steps_the_automatic_back_down() {
    let drivetrain = DrivetrainConfig::default();
    let threshold = drivetrain.gearbox.downshift_rpm;
    let (mut physics, mut vehicle) = settled(geared(drivetrain));
    drive_to_speed(&mut physics, &mut vehicle, 32.0, 30 * 60);
    drive(&mut physics, &mut vehicle, throttle(0.2), 30);
    let top = vehicle.drivetrain().expect("geared").gear();
    assert!(top >= 3, "gear {top}");
    let brake = VehicleInput {
        brake: 0.4,
        ..VehicleInput::default()
    };
    let mut seen = 0;
    for _ in 0..15 * 60 {
        let before = vehicle.drivetrain().expect("geared").downshifts();
        drive(&mut physics, &mut vehicle, brake, 1);
        let state = vehicle.drivetrain().expect("geared");
        if state.downshifts() > before {
            seen += 1;
            assert!(
                state.last_shift_rpm() <= threshold + 1.0,
                "{}",
                state.last_shift_rpm()
            );
        }
    }
    assert!(seen >= 2, "{seen} downshifts");
    assert_eq!(vehicle.drivetrain().expect("geared").gear(), 1, "back in first");
    assert!(speed(&physics, &vehicle).abs() < 0.2);
}

#[test]
fn a_manual_shift_holds_its_gear_and_refuses_gears_that_do_not_exist() {
    let drivetrain = DrivetrainConfig::default();
    let shift_steps = (drivetrain.gearbox.shift_time / DT).round() as usize;
    let (mut physics, mut vehicle) = settled(geared(drivetrain.clone()));
    assert!(vehicle.drivetrain().expect("geared").is_automatic());

    assert_eq!(vehicle.request_shift(ShiftRequest::Gear(3)), Ok(3));
    let state = vehicle.drivetrain().expect("geared");
    assert!(
        !state.is_automatic(),
        "a manual shift takes the gears from the automatic"
    );
    assert_eq!((state.gear(), state.target_gear()), (1, 3));
    drive(&mut physics, &mut vehicle, throttle(1.0), shift_steps + 1);
    assert_eq!(vehicle.drivetrain().expect("geared").gear(), 3);

    assert_eq!(vehicle.request_shift(ShiftRequest::Up), Ok(4));
    assert_eq!(vehicle.request_shift(ShiftRequest::Down), Ok(3));
    assert_eq!(
        vehicle.request_shift(ShiftRequest::Gear(99)),
        Err(VehicleError::InvalidGear { gear: 99 })
    );
    assert_eq!(
        vehicle.request_shift(ShiftRequest::Gear(-2)),
        Err(VehicleError::InvalidGear { gear: -2 })
    );

    // Held in third, the engine passes every threshold without a shift.
    drive(&mut physics, &mut vehicle, throttle(1.0), 25 * 60);
    let held = vehicle.drivetrain().expect("geared");
    assert_eq!(held.gear(), 3);
    assert!(held.rpm() > drivetrain.gearbox.upshift_rpm, "{}", held.rpm());
    assert!(
        held.rpm() < drivetrain.engine.redline_rpm + 50.0,
        "the limiter holds: {}",
        held.rpm()
    );

    // Back to the automatic: it takes over from third.
    vehicle.set_automatic_shifting(true).expect("geared");
    drive(&mut physics, &mut vehicle, throttle(1.0), 2 * 60);
    assert!(vehicle.drivetrain().expect("geared").gear() > 3);

    // The Simple drivetrain has no gearbox.
    let (_physics, mut simple) = settled(VehicleConfig::default());
    assert_eq!(
        simple.request_shift(ShiftRequest::Up),
        Err(VehicleError::NoGearbox)
    );
    assert_eq!(simple.set_automatic_shifting(true), Err(VehicleError::NoGearbox));
    assert_eq!(VehicleError::NoGearbox.code(), "vehicle_no_gearbox");
    assert_eq!(
        VehicleError::InvalidGear { gear: 9 }.code(),
        "vehicle_invalid_gear"
    );
}

#[test]
fn a_negative_throttle_brakes_first_then_reverses() {
    let (mut physics, mut vehicle) = settled(geared(DrivetrainConfig::default()));
    drive_to_speed(&mut physics, &mut vehicle, 10.0, 20 * 60);
    // Against the direction of travel it brakes: no reverse gear while the car rolls forward.
    let mut reversed_at = None;
    for i in 0..10 * 60 {
        drive(&mut physics, &mut vehicle, throttle(-1.0), 1);
        let state = vehicle.state(&physics).expect("state");
        if state.gear == -1 {
            assert!(state.forward_speed < 1.5, "in reverse at {}", state.forward_speed);
            reversed_at = Some(i);
            break;
        }
    }
    assert!(reversed_at.is_some());
    drive(&mut physics, &mut vehicle, throttle(-1.0), 3 * 60);
    let backward = vehicle.state(&physics).expect("state");
    assert!(backward.forward_speed < -2.0, "{}", backward.forward_speed);
    assert!(backward.rpm > 900.0);

    // And forward throttle brakes the backward roll before taking first gear.
    let mut forward_at = None;
    for i in 0..10 * 60 {
        drive(&mut physics, &mut vehicle, throttle(1.0), 1);
        let state = vehicle.state(&physics).expect("state");
        if state.gear == 1 {
            assert!(state.forward_speed > -1.5, "in first at {}", state.forward_speed);
            forward_at = Some(i);
            break;
        }
    }
    assert!(forward_at.is_some());
    drive(&mut physics, &mut vehicle, throttle(1.0), 3 * 60);
    assert!(speed(&physics, &vehicle) > 2.0);
}

#[test]
fn the_clutch_slips_on_a_launch_and_the_idle_car_stays_put() {
    let (mut physics, mut vehicle) = settled(geared(DrivetrainConfig::default()));
    // No throttle: the clutch is open at idle and the car does not creep.
    let start = vehicle.state(&physics).expect("state").translation;
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 120);
    assert!(
        vehicle
            .state(&physics)
            .expect("state")
            .translation
            .distance(start)
            < 0.01
    );
    let idle = vehicle.drivetrain().expect("geared");
    assert!((idle.rpm() - 900.0).abs() < 1.0, "{}", idle.rpm());
    assert!(idle.clutch_torque().abs() < 0.01, "{}", idle.clutch_torque());

    // A launch: the engine runs ahead of the wheels until the clutch locks.
    drive(&mut physics, &mut vehicle, throttle(1.0), 3);
    let launch = vehicle.drivetrain().expect("geared");
    assert!(launch.is_clutch_slipping());
    assert!(launch.rpm() > 1000.0, "{}", launch.rpm());
    let ratio = DrivetrainConfig::default().gearbox.total_ratio(1);
    let wheels = launch.wheels();
    let gear_rpm = 0.5 * (wheels[2].angular_speed + wheels[3].angular_speed) * ratio * RPM_PER_RAD_S;
    assert!(launch.rpm() > gear_rpm, "{} vs {gear_rpm}", launch.rpm());
    let mut locked = false;
    for _ in 0..240 {
        drive(&mut physics, &mut vehicle, throttle(1.0), 1);
        if !vehicle.drivetrain().expect("geared").is_clutch_slipping() {
            locked = true;
            break;
        }
    }
    assert!(locked, "the clutch never locked");
}

#[test]
fn engine_braking_slows_a_coasting_car_more_than_neutral() {
    let coast = |neutral: bool| {
        let (mut physics, mut vehicle) = settled(geared(DrivetrainConfig::default()));
        drive_to_speed(&mut physics, &mut vehicle, 15.0, 20 * 60);
        // Let the launch's wheelspin die away: a spinning engine and wheels first give their energy
        // back to the car.
        drive(&mut physics, &mut vehicle, throttle(0.15), 180);
        if neutral {
            vehicle.request_shift(ShiftRequest::Gear(0)).expect("neutral");
        }
        let from = speed(&physics, &vehicle);
        drive(&mut physics, &mut vehicle, VehicleInput::default(), 120);
        from - speed(&physics, &vehicle)
    };
    let in_gear = coast(false);
    let in_neutral = coast(true);
    assert!(in_gear > in_neutral * 1.3, "{in_gear} vs {in_neutral}");
    assert!(in_neutral > 0.0);
}

// ---------------------------------------------------------------------------
// Acceptance: differentials (pure)
// ---------------------------------------------------------------------------

/// A rear-wheel state on split grip: the left rear has full load, the right rear almost none, both
/// rolling over the ground at `ground` m/s. Returns the state after `steps` full-throttle steps.
fn split_grip(drivetrain: &DrivetrainConfig, ground: f32, steps: usize) -> DrivetrainState {
    let vehicle = VehicleConfig::default();
    let mut state = DrivetrainState::new(drivetrain, &vehicle.wheels, vehicle.max_brake_force);
    let input = DrivetrainInput {
        throttle: 1.0,
        brake: 0.0,
        forward_speed: ground,
        chassis_mass: vehicle.chassis_mass,
    };
    let load = |left_rear: f32, right_rear: f32| {
        [3000.0, 3000.0, left_rear, right_rear].map(|load| WheelContact {
            grounded: true,
            load,
            ground_speed: ground,
        })
    };
    for _ in 0..steps {
        state.step(
            drivetrain,
            &AidsConfig::default(),
            &input,
            &load(3000.0, 200.0),
            DT,
        );
    }
    state
}

/// The default car held in one gear, so a shift does not interrupt the torque being measured.
fn rear_drive(rear: Differential) -> DrivetrainConfig {
    DrivetrainConfig {
        rear_differential: rear,
        engine: flat_engine(300.0),
        gearbox: GearboxConfig {
            automatic: false,
            ..GearboxConfig::default()
        },
        ..DrivetrainConfig::default()
    }
}

#[test]
fn an_open_differential_splits_torque_equally() {
    let state = split_grip(&rear_drive(Differential::Open), 8.0, 60);
    let wheels = state.wheels();
    assert!(wheels[2].drive_torque > 10.0, "{}", wheels[2].drive_torque);
    assert_eq!(
        wheels[2].drive_torque.to_bits(),
        wheels[3].drive_torque.to_bits(),
        "{} vs {}",
        wheels[2].drive_torque,
        wheels[3].drive_torque
    );
    // The light wheel spins up while the torque stays equal: the sides are free.
    assert!(
        wheels[3].angular_speed > wheels[2].angular_speed + 3.0,
        "{} vs {}",
        wheels[3].angular_speed,
        wheels[2].angular_speed
    );
    // The undriven axle gets nothing.
    assert_eq!(wheels[0].drive_torque, 0.0);
    assert_eq!(wheels[1].drive_torque, 0.0);
}

#[test]
fn a_locked_differential_gives_equal_wheel_speeds() {
    let state = split_grip(&rear_drive(Differential::Locked), 8.0, 60);
    let wheels = state.wheels();
    assert!(
        (wheels[2].angular_speed - wheels[3].angular_speed).abs() < 1.0e-3,
        "{} vs {}",
        wheels[2].angular_speed,
        wheels[3].angular_speed
    );
    // The torque went to the wheel with the grip.
    assert!(
        wheels[2].drive_torque > wheels[3].drive_torque + 10.0,
        "{} vs {}",
        wheels[2].drive_torque,
        wheels[3].drive_torque
    );
}

#[test]
fn a_limited_slip_differential_holds_its_torque_bias_ratio() {
    let bias = 2.5;
    let state = split_grip(
        &rear_drive(Differential::LimitedSlip { torque_bias: bias }),
        8.0,
        60,
    );
    let wheels = state.wheels();
    // The light wheel slips, so the differential sits at its limit.
    assert!(
        wheels[3].angular_speed > wheels[2].angular_speed,
        "the sides differ"
    );
    let ratio = wheels[2].drive_torque / wheels[3].drive_torque;
    assert!((ratio - bias).abs() < 0.05 * bias, "ratio {ratio} vs {bias}");
    // Below the limit it stays locked: the grip differs by 1.5, inside the bias of 2.5, so an open
    // differential on the same contacts turns its sides at different speeds and this one does not.
    let uneven = |differential: Differential| {
        let config = rear_drive(differential);
        let mut state = DrivetrainState::new(&config, &VehicleConfig::default().wheels, 3000.0);
        let contacts = [3000.0, 3000.0, 3000.0, 2000.0].map(|load| WheelContact {
            grounded: true,
            load,
            ground_speed: 8.0,
        });
        let input = DrivetrainInput {
            throttle: 0.2,
            brake: 0.0,
            forward_speed: 8.0,
            chassis_mass: 1200.0,
        };
        for _ in 0..60 {
            state.step(&config, &AidsConfig::default(), &input, &contacts, DT);
        }
        state
    };
    let open = uneven(Differential::Open);
    let wheels = open.wheels();
    assert!(
        (wheels[2].angular_speed - wheels[3].angular_speed).abs() > 0.05,
        "the contacts pull the sides apart: {} vs {}",
        wheels[2].angular_speed,
        wheels[3].angular_speed
    );
    let limited = uneven(Differential::LimitedSlip { torque_bias: bias });
    let wheels = limited.wheels();
    assert!(
        (wheels[2].angular_speed - wheels[3].angular_speed).abs() < 1.0e-3,
        "{} vs {}",
        wheels[2].angular_speed,
        wheels[3].angular_speed
    );
    // The torque follows the grip, and stays inside the bias. It falls a little short of the grip's
    // 1.5 because the solve also lets each contact's chassis speed respond to its own force.
    let ratio = wheels[2].drive_torque / wheels[3].drive_torque;
    assert!(ratio > 1.2 && ratio < bias, "{ratio}");
}

#[test]
fn awd_splits_torque_by_the_centre_share_and_a_locked_centre_ties_the_axles() {
    let vehicle = VehicleConfig::default();
    let run = |centre: Differential, rear_load: f32| {
        let drivetrain = DrivetrainConfig {
            layout: DriveLayout::Awd,
            front_torque_share: 0.35,
            centre_differential: centre,
            engine: flat_engine(300.0),
            gearbox: GearboxConfig {
                automatic: false,
                ..GearboxConfig::default()
            },
            ..DrivetrainConfig::default()
        };
        let mut state = DrivetrainState::new(&drivetrain, &vehicle.wheels, vehicle.max_brake_force);
        let input = DrivetrainInput {
            throttle: 1.0,
            brake: 0.0,
            forward_speed: 8.0,
            chassis_mass: vehicle.chassis_mass,
        };
        let contacts = [3000.0, 3000.0, rear_load, rear_load].map(|load| WheelContact {
            grounded: true,
            load,
            ground_speed: 8.0,
        });
        for _ in 0..60 {
            state.step(&drivetrain, &AidsConfig::default(), &input, &contacts, DT);
        }
        state
    };
    let open = run(Differential::Open, 300.0);
    let wheels = open.wheels();
    let front = wheels[0].drive_torque + wheels[1].drive_torque;
    let rear = wheels[2].drive_torque + wheels[3].drive_torque;
    assert!(front > 10.0 && rear > 10.0);
    assert!(
        (front / (front + rear) - 0.35).abs() < 1.0e-3,
        "{front} of {}",
        front + rear
    );

    let locked = run(Differential::Locked, 300.0);
    let wheels = locked.wheels();
    assert!(
        (wheels[0].angular_speed - wheels[2].angular_speed).abs() < 1.0e-3,
        "{} vs {}",
        wheels[0].angular_speed,
        wheels[2].angular_speed
    );
    let front = wheels[0].drive_torque + wheels[1].drive_torque;
    let rear = wheels[2].drive_torque + wheels[3].drive_torque;
    assert!(
        front > rear,
        "the torque goes to the axle with the grip: {front} vs {rear}"
    );
}

#[test]
fn fwd_drives_the_front_wheels() {
    let (mut physics, mut vehicle) = settled(geared(DrivetrainConfig {
        layout: DriveLayout::Fwd,
        ..DrivetrainConfig::default()
    }));
    drive(&mut physics, &mut vehicle, throttle(1.0), 90);
    let state = vehicle.drivetrain().expect("geared");
    assert!(state.wheels()[0].drive_torque.abs() > 1.0);
    assert_eq!(state.wheels()[2].drive_torque, 0.0);
    assert!(speed(&physics, &vehicle) > 3.0);
}

// ---------------------------------------------------------------------------
// Anti-roll, aero, rolling resistance
// ---------------------------------------------------------------------------

/// The chassis' roll in a steady right turn, radians, averaged over a second.
fn steady_turn_roll(anti_roll: Vec<AntiRollBar>, drivetrain: Drivetrain) -> f32 {
    let config = VehicleConfig {
        anti_roll,
        drivetrain,
        ..VehicleConfig::default()
    };
    let mut physics = flat_world();
    let mut vehicle =
        Vehicle::spawn(&mut physics, config, Vec3::new(0.0, 0.9, 0.0), Quat::IDENTITY).expect("spawn");
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    drive_to_speed(&mut physics, &mut vehicle, 8.0, 20 * 60);
    // A small throttle that about holds the speed: a real turn scrubs some off.
    let input = VehicleInput {
        throttle: 0.05,
        brake: 0.0,
        steer: 0.3,
    };
    drive(&mut physics, &mut vehicle, input, 6 * 60);
    let mut sum = 0.0;
    for _ in 0..60 {
        drive(&mut physics, &mut vehicle, input, 1);
        let state = vehicle.state(&physics).expect("state");
        // The chassis' right axis against the horizontal.
        sum += (state.rotation * Vec3::X).y;
    }
    sum / 60.0
}

#[test]
fn an_anti_roll_bar_reduces_the_steady_turn_roll_angle() {
    // Roll stiffness of the springs, per unit of track²: k_s (per wheel, the rate per kg × mass).
    // Each bar adds K. The chassis rolls by moment / stiffness, so the ratio is k_s / (k_s + 2 K).
    let config = VehicleConfig::default();
    let spring = config.wheels[0].suspension_stiffness * config.chassis_mass;
    let k = spring;
    let bars = vec![
        AntiRollBar {
            left: 0,
            right: 1,
            stiffness: k,
        },
        AntiRollBar {
            left: 2,
            right: 3,
            stiffness: k,
        },
    ];
    for drivetrain in [
        Drivetrain::Simple,
        Drivetrain::Geared(DrivetrainConfig::default()),
    ] {
        let free = steady_turn_roll(Vec::new(), drivetrain.clone());
        let barred = steady_turn_roll(bars.clone(), drivetrain.clone());
        assert!(free.abs() > 2.0e-4, "the turn rolls the car: {free}");
        assert_eq!(free.signum(), barred.signum(), "{free} vs {barred}");
        let ratio = barred.abs() / free.abs();
        let expected = spring / (spring + 2.0 * k);
        assert!(
            (ratio - expected).abs() < 0.15,
            "{drivetrain:?}: roll {free} -> {barred}, ratio {ratio} vs {expected}"
        );
    }
}

#[test]
fn a_bar_does_nothing_with_a_level_chassis_and_validates_its_wheels() {
    let bar = AntiRollBar {
        left: 0,
        right: 1,
        stiffness: 20_000.0,
    };
    assert_eq!(bar.force(0.1, 0.1), 0.0);
    assert_eq!(bar.force(0.2, 0.1), 2000.0);
    assert_eq!(bar.force(0.1, 0.2), -2000.0);
    let config = VehicleConfig {
        anti_roll: vec![AntiRollBar {
            left: 0,
            right: 9,
            stiffness: 1.0,
        }],
        ..VehicleConfig::default()
    };
    assert_eq!(
        config.validate(),
        Err(VehicleError::InvalidConfig { field: "anti_roll" })
    );
    // A level car with bars rides like one without, wheel for wheel. The bar's sign shows in the
    // force above and in the steady-turn test, where a flipped bar would add roll.
    let (physics, vehicle) = settled(VehicleConfig {
        anti_roll: vec![bar],
        ..VehicleConfig::default()
    });
    let wheels = vehicle.wheel_states(&physics).expect("wheels");
    let (bare_physics, bare_vehicle) = settled(VehicleConfig::default());
    let bare = bare_vehicle.wheel_states(&bare_physics).expect("wheels");
    for (wheel, bare) in wheels.iter().zip(&bare) {
        assert!(
            (wheel.compression - bare.compression).abs() < 1.0e-3,
            "wheel {}: {} vs {}",
            wheel.index,
            wheel.compression,
            bare.compression
        );
    }
}

// ---------------------------------------------------------------------------
// Edges a driver or a config can reach
// ---------------------------------------------------------------------------

/// How fast a car with a held brake creeps down a slope of `theta` radians, m/s.
fn creep_speed(theta: f32) -> f32 {
    let tilt = Quat::from_rotation_x(theta);
    let mut physics = PhysicsWorld::default();
    let body = physics.bodies.insert(RigidBodyBuilder::fixed().build());
    let PhysicsWorld {
        bodies, colliders, ..
    } = &mut physics;
    colliders.insert_with_parent(
        ColliderBuilder::cuboid(500.0, 0.5, 500.0)
            .translation(to_rapier_vec3(tilt * Vec3::new(0.0, -0.5, 0.0)))
            .rotation(to_rapier_vec3(Vec3::X * theta))
            .build(),
        body,
        bodies,
    );
    let config = geared(DrivetrainConfig::default());
    let mut vehicle =
        Vehicle::spawn(&mut physics, config, tilt * Vec3::new(0.0, 0.9, 0.0), tilt).expect("spawn");
    let held = VehicleInput {
        brake: 1.0,
        ..VehicleInput::default()
    };
    drive(&mut physics, &mut vehicle, held, 5 * 60);
    let before = vehicle.state(&physics).expect("state").translation;
    drive(&mut physics, &mut vehicle, held, 5 * 60);
    let after = vehicle.state(&physics).expect("state").translation;
    (after - before).length() / 5.0
}

#[test]
fn a_held_brake_creeps_down_a_slope_at_the_slip_the_tire_needs() {
    // Below the 1 m/s slip floor a locked tire's force is viscous: µ·N·v/s, with s the peak slip. It
    // balances the slope at v = s·tan(θ)/µ in continuous time. At 60 Hz gravity adds about 4 cm/s per
    // step on 15 degrees, more than the creep itself, so the fixed step reads somewhat above that:
    // the closed form is a floor here, and the speed still scales with tan(θ).
    let tire = TireConfig::default();
    let closed_form = |theta: f32| tire.peak_slip * theta.tan() / tire.peak_friction;
    let (gentle, steep) = (0.26_f32, 0.52_f32);
    let (gentle_speed, steep_speed) = (creep_speed(gentle), creep_speed(steep));
    for (theta, measured) in [(gentle, gentle_speed), (steep, steep_speed)] {
        let expected = closed_form(theta);
        assert!(
            measured >= expected && measured < 1.5 * expected,
            "slope {theta}: creep {measured} m/s vs {expected} m/s"
        );
    }
    let ratio = steep_speed / gentle_speed;
    let expected = steep.tan() / gentle.tan();
    assert!(
        (ratio - expected).abs() < 0.15 * expected,
        "creep {gentle_speed} and {steep_speed}: ratio {ratio} vs {expected}"
    );
    // The user guide's figure for the default tire on a 15 degree hill.
    assert!(
        (0.03..0.04).contains(&gentle_speed),
        "book/user-guide/vehicles.md says about 3.4 cm/s: {gentle_speed}"
    );
}

#[test]
fn a_throttle_against_the_travel_in_neutral_brakes_instead_of_taking_reverse() {
    let config = rear_drive(Differential::Open);
    let wheels = VehicleConfig::default().wheels;
    let mut state = DrivetrainState::new(&config, &wheels, 3000.0);
    state
        .request_shift(&config, ShiftRequest::Gear(0))
        .expect("neutral");
    state.set_automatic(true);
    let at = |ground_speed: f32| {
        [WheelContact {
            grounded: true,
            load: 3000.0,
            ground_speed,
        }; 4]
    };
    let input = |forward_speed: f32, throttle: f32| DrivetrainInput {
        throttle,
        brake: 0.0,
        forward_speed,
        chassis_mass: 1200.0,
    };
    // Rolling forward at speed, the throttle says reverse: the car brakes and stays in neutral.
    let none = AidsConfig::default();
    for _ in 0..10 {
        state.step(&config, &none, &input(30.0, -1.0), &at(30.0), DT);
    }
    assert_eq!(state.target_gear(), 0);
    assert!(
        state.wheels()[2].tire_force < -100.0,
        "{}",
        state.wheels()[2].tire_force
    );
    // Near a stop the automatic takes reverse, and forward throttle takes first.
    state.step(&config, &none, &input(0.5, -1.0), &at(0.5), DT);
    assert_eq!(state.target_gear(), -1);
    let mut forward = DrivetrainState::new(&config, &wheels, 3000.0);
    forward
        .request_shift(&config, ShiftRequest::Gear(0))
        .expect("neutral");
    forward.set_automatic(true);
    forward.step(&config, &none, &input(0.5, 1.0), &at(0.5), DT);
    assert_eq!(forward.target_gear(), 1);
    // A manual box in neutral only revs the engine.
    let mut manual = DrivetrainState::new(&config, &wheels, 3000.0);
    manual
        .request_shift(&config, ShiftRequest::Gear(0))
        .expect("neutral");
    for _ in 0..30 {
        manual.step(&config, &none, &input(0.0, 1.0), &at(0.0), DT);
    }
    assert_eq!(manual.target_gear(), 0);
    assert!(manual.rpm() > 2000.0, "{}", manual.rpm());
}

#[test]
fn a_config_the_automatic_cannot_drive_is_refused() {
    let field = |change: fn(&mut DrivetrainConfig)| {
        let mut drivetrain = DrivetrainConfig::default();
        change(&mut drivetrain);
        match geared(drivetrain).validate() {
            Err(VehicleError::InvalidConfig { field }) => Some(field),
            Ok(()) => None,
            Err(other) => panic!("{other:?}"),
        }
    };
    assert_eq!(field(|_| ()), None);
    // The limiter stops the engine short of the upshift.
    assert_eq!(
        field(|c| c.engine.redline_rpm = 5000.0),
        Some("drivetrain.gearbox.upshift_rpm")
    );
    // The idle floor keeps the engine above the downshift.
    assert_eq!(
        field(|c| c.gearbox.downshift_rpm = 800.0),
        Some("drivetrain.gearbox.downshift_rpm")
    );
    // An upshift would land under the downshift, so the next gear is out of reach.
    assert_eq!(
        field(|c| c.gearbox.forward_ratios = vec![4.0, 1.0]),
        Some("drivetrain.gearbox.forward_ratios")
    );
    // Gears go from short to tall.
    assert_eq!(
        field(|c| c.gearbox.forward_ratios = vec![1.0, 3.0]),
        Some("drivetrain.gearbox.forward_ratios")
    );
    assert_eq!(
        field(|c| c.engine.torque_curve = vec![(1000.0, 0.0), (7000.0, 0.0)]),
        Some("drivetrain.engine.torque_curve")
    );
    // Settings so large their products leave f32, or so small they divide into infinity.
    assert_eq!(
        field(|c| c.wheel_inertia = 1.0e38),
        Some("drivetrain.wheel_inertia")
    );
    assert_eq!(
        field(|c| c.engine.inertia = 1.0e-30),
        Some("drivetrain.engine.inertia")
    );
    let brake = VehicleConfig {
        max_brake_force: 2.0e38,
        ..geared(DrivetrainConfig::default())
    };
    assert_eq!(
        brake.validate(),
        Err(VehicleError::InvalidConfig {
            field: "max_brake_force"
        })
    );
    // The Simple model keeps its v1 rules.
    let simple = VehicleConfig {
        max_brake_force: 2.0e38,
        ..VehicleConfig::default()
    };
    assert_eq!(simple.validate(), Ok(()));
}

#[test]
fn drag_follows_the_square_of_the_speed_in_a_coast_down() {
    // With no rolling resistance and the clutch out, m_eff dv/dt = −k v², so 1/v grows linearly:
    // 1/v(t) = 1/v0 + k t / m_eff, with the wheels' inertia in m_eff (Gillespie).
    let aero = AeroConfig {
        drag_coefficient: 0.9,
        frontal_area: 2.0,
        ..AeroConfig::default()
    };
    assert!(
        aero.drag_force(Vec3::new(0.0, 0.0, -10.0))
            .abs_diff_eq(Vec3::new(0.0, 0.0, 0.5 * 1.225 * 0.9 * 2.0 * 100.0), 1.0e-3)
    );
    let drivetrain = DrivetrainConfig {
        tire: TireConfig {
            rolling_coefficient: 0.0,
            ..TireConfig::default()
        },
        ..DrivetrainConfig::default()
    };
    let radius = VehicleConfig::default().wheels[0].radius;
    let mass = VehicleConfig::default().chassis_mass;
    let m_eff = mass + 4.0 * drivetrain.wheel_inertia / (radius * radius);
    let k = 0.5 * aero.air_density * aero.drag_coefficient * aero.frontal_area;
    let (mut physics, mut vehicle) = settled(VehicleConfig {
        aero: Some(aero),
        ..geared(drivetrain)
    });
    drive_to_speed(&mut physics, &mut vehicle, 35.0, 40 * 60);
    vehicle.request_shift(ShiftRequest::Gear(0)).expect("neutral");
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 120);
    let v0 = speed(&physics, &vehicle);
    let seconds = 20.0;
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput::default(),
        (seconds * 60.0) as usize,
    );
    let v = speed(&physics, &vehicle);
    let analytic = 1.0 / (1.0 / v0 + k * seconds / m_eff);
    assert!(
        (v - analytic).abs() / analytic < 0.02,
        "{v} vs {analytic} from {v0}"
    );
}

#[test]
fn rolling_resistance_decelerates_by_its_coefficient() {
    // No air, clutch out: m_eff dv/dt = −Crr m g.
    let crr = 0.03;
    let drivetrain = DrivetrainConfig {
        tire: TireConfig {
            rolling_coefficient: crr,
            ..TireConfig::default()
        },
        ..DrivetrainConfig::default()
    };
    let radius = VehicleConfig::default().wheels[0].radius;
    let mass = VehicleConfig::default().chassis_mass;
    let m_eff = mass + 4.0 * drivetrain.wheel_inertia / (radius * radius);
    let (mut physics, mut vehicle) = settled(geared(drivetrain));
    drive_to_speed(&mut physics, &mut vehicle, 20.0, 30 * 60);
    vehicle.request_shift(ShiftRequest::Gear(0)).expect("neutral");
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 120);
    let v0 = speed(&physics, &vehicle);
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 300);
    let lost = v0 - speed(&physics, &vehicle);
    let analytic = crr * mass * G / m_eff * 5.0;
    assert!((lost - analytic).abs() / analytic < 0.03, "{lost} vs {analytic}");
}

#[test]
fn downforce_loads_the_axle_it_is_set_on() {
    let aero = AeroConfig {
        drag_coefficient: 0.0,
        frontal_area: 0.0,
        front_downforce: 0.0,
        rear_downforce: 1.2,
        ..AeroConfig::default()
    };
    let (mut physics, mut vehicle) = settled(VehicleConfig {
        aero: Some(aero.clone()),
        ..geared(DrivetrainConfig::default())
    });
    let at_rest = vehicle.wheel_states(&physics).expect("wheels");
    let rest_rear = at_rest[2].suspension_force + at_rest[3].suspension_force;
    drive_to_speed(&mut physics, &mut vehicle, 35.0, 40 * 60);
    // Hold a part throttle and average the loads over a second.
    let (mut front, mut rear, mut v) = (0.0, 0.0, 0.0);
    for _ in 0..60 {
        drive(&mut physics, &mut vehicle, throttle(0.05), 1);
        let state = vehicle.state(&physics).expect("state");
        front += state.wheels[0].suspension_force + state.wheels[1].suspension_force;
        rear += state.wheels[2].suspension_force + state.wheels[3].suspension_force;
        v += state.forward_speed;
    }
    let (front, rear, v) = (front / 60.0, rear / 60.0, v / 60.0);
    let expected = aero.downforce(aero.rear_downforce, v);
    assert!(expected > 500.0, "{expected}");
    let weight = VehicleConfig::default().chassis_mass * G;
    assert!(
        ((front + rear) - (weight + expected)).abs() < 0.05 * weight,
        "total {} vs {}",
        front + rear,
        weight + expected
    );
    assert!(
        rear > rest_rear + 0.5 * expected,
        "{rear} vs rest {rest_rear} + {expected}"
    );
}

// ---------------------------------------------------------------------------
// Acceptance: driver aids
// ---------------------------------------------------------------------------

/// The distance a car takes to stop from 25 m/s under a full brake that can lock every wheel.
fn stopping_distance(abs: Option<AbsConfig>) -> f32 {
    let mut config = geared(DrivetrainConfig::default());
    // Brakes past the tires' limit: without help every wheel locks.
    config.max_brake_force = 8000.0;
    config.aids.abs = abs;
    let (mut physics, mut vehicle) = settled(config);
    drive_to_speed(&mut physics, &mut vehicle, 25.0, 30 * 60);
    let start = vehicle.state(&physics).expect("state").translation;
    let v0 = speed(&physics, &vehicle);
    let brake = VehicleInput {
        brake: 1.0,
        ..VehicleInput::default()
    };
    let mut locked_steps = 0;
    for _ in 0..30 * 60 {
        drive(&mut physics, &mut vehicle, brake, 1);
        let state = vehicle.state(&physics).expect("state");
        if state.wheels.iter().all(|wheel| wheel.slip_ratio < -0.9) {
            locked_steps += 1;
        }
        if state.forward_speed.abs() < 0.2 {
            let distance = state.translation.distance(start);
            // Scale to exactly 25 m/s (the launch ends a little past it).
            let scaled = distance * (25.0 / v0) * (25.0 / v0);
            if vehicle.config().aids.abs.is_none() {
                assert!(locked_steps > 60, "the wheels locked: {locked_steps} steps");
            } else {
                assert!(locked_steps < 30, "ABS kept them turning: {locked_steps} steps");
            }
            return scaled;
        }
    }
    panic!("the car never stopped");
}

#[test]
fn with_abs_a_car_stops_shorter_than_with_locked_wheels_on_the_same_grip() {
    let tire = TireConfig::default();
    let locked = stopping_distance(None);
    let with_abs = stopping_distance(Some(AbsConfig::default()));
    // d = v² / (2 µ g), plus what engine braking and the first instants move it by.
    let on_slide = 25.0 * 25.0 / (2.0 * tire.slide_friction * G);
    let on_peak = 25.0 * 25.0 / (2.0 * tire.peak_friction * G);
    assert!(
        (locked - on_slide).abs() / on_slide < 0.08,
        "locked {locked} vs {on_slide}"
    );
    assert!(with_abs < locked * 0.9, "ABS {with_abs} vs locked {locked}");
    assert!(
        with_abs > on_peak * 0.95,
        "ABS {with_abs} beats the peak's {on_peak}"
    );
    assert!(
        with_abs < on_peak * 1.15,
        "ABS {with_abs} vs the peak's {on_peak}"
    );
}

#[test]
fn traction_control_cuts_the_throttle_on_wheelspin_and_launches_harder() {
    let launch = |tc: Option<TractionControlConfig>| {
        let mut config = geared(DrivetrainConfig {
            // A slippery surface: the engine overpowers the tires in first gear.
            tire: TireConfig {
                peak_friction: 0.35,
                slide_friction: 0.2,
                ..TireConfig::default()
            },
            ..DrivetrainConfig::default()
        });
        config.aids.traction_control = tc;
        let (mut physics, mut vehicle) = settled(config);
        let (mut most_slip, mut least_factor) = (0.0_f32, 1.0_f32);
        for _ in 0..6 * 60 {
            drive(&mut physics, &mut vehicle, throttle(1.0), 1);
            let state = vehicle.drivetrain().expect("geared");
            most_slip = most_slip.max(state.wheels()[2].slip_ratio);
            least_factor = least_factor.min(state.traction_factor());
        }
        (speed(&physics, &vehicle), most_slip, least_factor)
    };
    let (free_speed, free_slip, free_factor) = launch(None);
    let (tc_speed, tc_slip, tc_factor) = launch(Some(TractionControlConfig::default()));
    assert_eq!(free_factor, 1.0);
    assert!(tc_factor < 0.9, "the throttle was cut: {tc_factor}");
    assert!(free_slip > 2.0 * tc_slip, "slip {free_slip} vs {tc_slip}");
    assert!(tc_speed > free_speed, "{tc_speed} vs {free_speed}");
}

#[test]
fn the_aids_need_the_geared_drivetrain() {
    let config = VehicleConfig {
        aids: AidsConfig {
            abs: Some(AbsConfig::default()),
            traction_control: None,
        },
        ..VehicleConfig::default()
    };
    assert_eq!(
        config.validate(),
        Err(VehicleError::InvalidConfig { field: "aids" })
    );
}

// ---------------------------------------------------------------------------
// Readout, reset, determinism
// ---------------------------------------------------------------------------

#[test]
fn slip_ratio_and_slip_angle_read_true() {
    let (mut physics, mut vehicle) = settled(geared(DrivetrainConfig::default()));
    // A hard launch spins the driven wheels: positive slip on the rear, none on the front.
    drive(&mut physics, &mut vehicle, throttle(1.0), 60);
    let launch = vehicle.state(&physics).expect("state");
    assert!(
        launch.wheels[2].slip_ratio > 0.02,
        "{}",
        launch.wheels[2].slip_ratio
    );
    assert!(
        launch.wheels[0].slip_ratio.abs() < 0.05,
        "{}",
        launch.wheels[0].slip_ratio
    );

    // Coming up to speed, then a turn: the front contacts run off to the wheels' inside.
    drive_to_speed(&mut physics, &mut vehicle, 10.0, 20 * 60);
    let turn = |steer: f32, physics: &mut PhysicsWorld, vehicle: &mut Vehicle| {
        drive(
            physics,
            vehicle,
            VehicleInput {
                throttle: 0.2,
                brake: 0.0,
                steer,
            },
            120,
        );
        vehicle.state(physics).expect("state")
    };
    let right = turn(0.4, &mut physics, &mut vehicle);
    let angle = right.wheels[0].slip_angle;
    assert!(angle < -0.005 && angle > -0.6, "right turn: {angle}");
    let left = turn(-0.4, &mut physics, &mut vehicle);
    let angle = left.wheels[1].slip_angle;
    assert!(angle > 0.005 && angle < 0.6, "left turn: {angle}");

    // A hard stop: negative slip.
    let brake = VehicleInput {
        brake: 1.0,
        ..VehicleInput::default()
    };
    drive(&mut physics, &mut vehicle, brake, 10);
    let braking = vehicle.state(&physics).expect("state");
    assert!(
        braking.wheels[0].slip_ratio < -0.01,
        "{}",
        braking.wheels[0].slip_ratio
    );
}

#[test]
fn the_simple_drivetrain_reads_no_engine_and_no_slip_ratio() {
    let (mut physics, mut vehicle) = settled(VehicleConfig::default());
    drive(&mut physics, &mut vehicle, throttle(1.0), 120);
    let state = vehicle.state(&physics).expect("state");
    assert_eq!(state.rpm, 0.0);
    assert_eq!(state.gear, 0);
    assert!(vehicle.drivetrain().is_none());
    assert!(state.wheels.iter().all(|wheel| wheel.slip_ratio == 0.0));
    assert!(
        (state.speed - state.forward_speed).abs() < 0.01,
        "{}",
        state.speed
    );
    // Slip angle is measured for either model: dead straight here.
    assert!(state.wheels.iter().all(|wheel| wheel.slip_angle.abs() < 0.01));
}

#[test]
fn reset_puts_the_drivetrain_back_at_idle_in_first() {
    let (mut physics, mut vehicle) = settled(geared(DrivetrainConfig::default()));
    drive(&mut physics, &mut vehicle, throttle(1.0), 8 * 60);
    assert!(vehicle.drivetrain().expect("geared").gear() > 1);
    vehicle.reset_to_spawn(&mut physics).expect("reset");
    let state = vehicle.drivetrain().expect("geared");
    assert_eq!(state.gear(), 1);
    assert_eq!(state.upshifts(), 0);
    assert!((state.rpm() - 900.0).abs() < 0.01, "{}", state.rpm());
    assert!(state.wheels().iter().all(|wheel| wheel.angular_speed == 0.0));
    // It drives again after the reset.
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    drive(&mut physics, &mut vehicle, throttle(1.0), 120);
    assert!(speed(&physics, &vehicle) > 3.0);
    // The wheel readout takes the drivetrain's spin, not rapier's kinematic one.
    let wheels = vehicle.wheel_states(&physics).expect("wheels");
    let spin = vehicle.drivetrain().expect("geared").wheels()[2].spin_angle;
    assert_eq!(wheels[2].spin_angle, spin);
    assert!(spin > 1.0);
}

/// Launch, weave, brake and a manual shift: the whole system at once.
fn scripted_run(config: VehicleConfig) -> (Vec<u64>, Vec<u64>) {
    let mut physics = flat_world();
    let mut vehicle = Vehicle::spawn(
        &mut physics,
        config,
        Vec3::new(2.0, 1.0, -3.0),
        Quat::from_rotation_y(0.3),
    )
    .expect("spawn");
    let (mut worlds, mut cars) = (Vec::new(), Vec::new());
    for i in 0..1200_usize {
        let t = i as f32 / 60.0;
        if i == 700 {
            vehicle.request_shift(ShiftRequest::Down).expect("shift");
        }
        if i == 800 {
            vehicle.set_automatic_shifting(true).expect("automatic");
        }
        let input = VehicleInput {
            throttle: match i {
                0..=599 => 1.0,
                600..=699 => 0.0,
                700..=999 => 0.7,
                _ => -0.5,
            },
            brake: if (600..700).contains(&i) { 0.9 } else { 0.0 },
            steer: (t * 1.3).sin() * 0.6,
        };
        vehicle.update(&mut physics, input, DT).expect("update");
        step(&mut physics);
        worlds.push(physics.state_digest());
        cars.push(vehicle.state_digest());
    }
    (worlds, cars)
}

#[test]
fn two_runs_give_the_same_digest() {
    let config = VehicleConfig {
        anti_roll: vec![
            AntiRollBar {
                left: 0,
                right: 1,
                stiffness: 15_000.0,
            },
            AntiRollBar {
                left: 2,
                right: 3,
                stiffness: 9_000.0,
            },
        ],
        aero: Some(AeroConfig {
            front_downforce: 0.2,
            rear_downforce: 0.3,
            ..AeroConfig::default()
        }),
        aids: AidsConfig {
            abs: Some(AbsConfig::default()),
            traction_control: Some(TractionControlConfig::default()),
        },
        ..geared(DrivetrainConfig {
            layout: DriveLayout::Awd,
            rear_differential: Differential::LimitedSlip { torque_bias: 2.5 },
            centre_differential: Differential::Locked,
            clutch: ClutchConfig::default(),
            ..DrivetrainConfig::default()
        })
    };
    let (worlds_a, cars_a) = scripted_run(config.clone());
    let (worlds_b, cars_b) = scripted_run(config);
    if let Some(first) = worlds_a.iter().zip(&worlds_b).position(|(a, b)| a != b) {
        panic!("the worlds diverged at step {first}");
    }
    if let Some(first) = cars_a.iter().zip(&cars_b).position(|(a, b)| a != b) {
        panic!("the drivetrains diverged at step {first}");
    }
    // The run really moved (a parked car would be trivially deterministic).
    assert!(cars_a.windows(2).filter(|w| w[0] != w[1]).count() > 1100);
    assert!(worlds_a.windows(2).filter(|w| w[0] != w[1]).count() > 1000);
}

#[test]
fn a_geared_car_steers_the_right_way_and_stays_finite_through_a_rough_script() {
    let (mut physics, mut vehicle) = settled(geared(DrivetrainConfig::default()));
    drive_to_speed(&mut physics, &mut vehicle, 10.0, 20 * 60);
    drive(
        &mut physics,
        &mut vehicle,
        VehicleInput {
            throttle: 0.2,
            brake: 0.0,
            steer: 0.5,
        },
        120,
    );
    let state = vehicle.state(&physics).expect("state");
    assert!(
        state.angular_velocity.y < -0.1,
        "right turn: {}",
        state.angular_velocity.y
    );

    // Flip through everything at once; nothing goes non-finite.
    for i in 0..1200_usize {
        let phase = i % 240;
        let input = VehicleInput {
            throttle: if phase < 120 { 1.0 } else { -1.0 },
            brake: if phase % 90 < 15 { 1.0 } else { 0.0 },
            steer: if phase < 60 { 1.0 } else { -1.0 },
        };
        drive(&mut physics, &mut vehicle, input, 1);
        let readout = vehicle.state(&physics).expect("state");
        assert!(
            readout.linear_velocity.is_finite() && readout.rpm.is_finite(),
            "step {i}"
        );
        for wheel in &readout.wheels {
            assert!(
                wheel.slip_ratio.is_finite() && wheel.slip_angle.is_finite(),
                "step {i}"
            );
        }
    }
    // A car in the air keeps a valid state: the driven wheels and the engine run free.
    let mut physics = flat_world();
    let mut vehicle = Vehicle::spawn(
        &mut physics,
        geared(DrivetrainConfig::default()),
        Vec3::new(0.0, 8.0, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    drive(&mut physics, &mut vehicle, throttle(1.0), 30);
    let air = vehicle.state(&physics).expect("state");
    assert_eq!(air.grounded_wheels, 0);
    assert!(
        air.wheels
            .iter()
            .all(|wheel| wheel.slip_ratio == 0.0 && wheel.slip_angle == 0.0)
    );
    assert!(vehicle.drivetrain().expect("geared").wheels()[2].angular_speed > 1.0);
    assert_eq!(vehicle.drivetrain().expect("geared").wheels()[2].tire_force, 0.0);
}
