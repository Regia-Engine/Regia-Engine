// SPDX-License-Identifier: MPL-2.0
//! Fixed-wing aircraft (`regia_physics::aero`, ADR-3374), headless in a raw rapier world: a trimmed
//! plane holds level flight with the stick fixed, its glide matches its own polar, a right roll
//! input rolls right, the take-off run is deterministic and inside its stated range, a parked plane
//! settles without drifting, still air gives no NaN, and a high angle of attack sets the stall flag.

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::ColliderBuilder;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::aero::trim::{
    TrimTarget, max_lift_to_drag, max_trimmed_lift_to_drag, solve, trimmed_polar,
};
use regia_physics::aero::{AeroEnvironment, Aircraft, AircraftInput, isa_density, trainer};
use regia_physics::convert::to_rapier_vec3;

const DT: f32 = 1.0 / 60.0;
const GRAVITY: Vec3 = Vec3::new(0.0, -9.81, 0.0);

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

/// A world with a 4 km × 200 m runway whose top is at y = 0 (none when `runway` is false).
fn world(runway: bool) -> PhysicsWorld {
    let mut physics = PhysicsWorld::default();
    if runway {
        let body = physics.bodies.insert(
            RigidBodyBuilder::fixed()
                .translation(to_rapier_vec3(Vec3::new(0.0, -0.5, -1900.0)))
                .build(),
        );
        let PhysicsWorld {
            bodies, colliders, ..
        } = &mut physics;
        colliders.insert_with_parent(ColliderBuilder::cuboid(100.0, 0.5, 2000.0).build(), body, bodies);
    }
    physics
}

fn fly(physics: &mut PhysicsWorld, plane: &mut Aircraft, input: AircraftInput, steps: usize) {
    let env = AeroEnvironment::default();
    for _ in 0..steps {
        plane
            .update_with_layers(physics, input, &env, DT, None)
            .expect("update");
        step(physics);
    }
}

#[test]
fn the_trim_holds_level_flight_for_3600_steps() {
    let speed = 50.0;
    let altitude = 1000.0;
    let trim = solve(&trainer(), &TrimTarget::level(speed, altitude)).expect("trim");
    let mut physics = world(false);
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, altitude, 0.0),
        trim.rotation(),
        trim.velocity(),
    )
    .expect("spawn");
    let env = AeroEnvironment::default();
    let (mut worst_dy, mut worst_dv) = (0.0_f32, 0.0_f32);
    for _ in 0..3600 {
        plane
            .update_with_layers(&mut physics, trim.input(), &env, DT, None)
            .expect("update");
        step(&mut physics);
        let state = plane.state(&physics).expect("state");
        worst_dy = worst_dy.max((state.translation.y - altitude).abs());
        worst_dv = worst_dv.max((state.linear_velocity.length() - speed).abs() / speed);
    }
    let state = plane.state(&physics).expect("state");
    println!(
        "trim {trim:?}: worst altitude change {worst_dy} m, worst speed change {} %, end {:?}",
        worst_dv * 100.0,
        state.translation
    );
    assert!(worst_dy < 2.0, "altitude moved {worst_dy} m");
    assert!(worst_dv < 0.02, "speed moved {} %", worst_dv * 100.0);
    assert!(state.bank.abs() < 0.01 && state.heading.abs() < 0.01, "{state:?}");
}

#[test]
fn the_glide_ratio_matches_the_polar() {
    let config = trainer();
    // The glide flies the trimmed polar (elevator set for zero pitching moment); the neutral-elevator
    // polar's best is printed for comparison: the gap is the trim drag of the sample's large static
    // margin (ADR-3374).
    let (neutral, _) = max_lift_to_drag(&config);
    let (best, alpha) = max_trimmed_lift_to_drag(&config);
    let (cl, _, _) = trimmed_polar(&config, alpha).expect("trims");
    let altitude = 1500.0;
    let weight = config.mass * 9.81;
    let speed = (2.0 * weight / (isa_density(altitude) * config.reference_area() * cl)).sqrt();
    let trim = solve(&config, &TrimTarget::glide(speed, altitude)).expect("glide trim");
    let mut physics = world(false);
    let start = Vec3::new(0.0, altitude, 0.0);
    let mut plane =
        Aircraft::spawn(&mut physics, config, start, trim.rotation(), trim.velocity()).expect("spawn");
    fly(&mut physics, &mut plane, trim.input(), 1800);
    let state = plane.state(&physics).expect("state");
    let moved = state.translation - start;
    let ratio = Vec3::new(moved.x, 0.0, moved.z).length() / -moved.y;
    println!(
        "best L/D trimmed {best} (neutral elevator {neutral}) at alpha {} deg, speed {speed}, trim {trim:?}, \
         simulated glide ratio {ratio}",
        alpha.to_degrees()
    );
    assert!(
        (ratio - best).abs() / best < 0.10,
        "glide {ratio} vs trimmed polar {best}"
    );
}

#[test]
fn positive_roll_input_rolls_right() {
    let trim = solve(&trainer(), &TrimTarget::level(50.0, 1000.0)).expect("trim");
    let mut physics = world(false);
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 1000.0, 0.0),
        trim.rotation(),
        trim.velocity(),
    )
    .expect("spawn");
    let input = AircraftInput {
        roll: 1.0,
        ..trim.input()
    };
    fly(&mut physics, &mut plane, input, 30);
    let state = plane.state(&physics).expect("state");
    let right_tip = state.rotation * Vec3::new(5.0, 0.0, 0.0);
    let left_tip = state.rotation * Vec3::new(-5.0, 0.0, 0.0);
    println!("bank {} rad, rates {:?}", state.bank, state.body_rates);
    assert!(state.bank > 0.05, "bank {}", state.bank);
    assert!(right_tip.y < left_tip.y);
    // Right roll is rotation about the forward axis (−Z): body rate z negative.
    assert!(state.body_rates.z < 0.0);
    // And left.
    let mut physics = world(false);
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 1000.0, 0.0),
        trim.rotation(),
        trim.velocity(),
    )
    .expect("spawn");
    let input = AircraftInput {
        roll: -1.0,
        ..trim.input()
    };
    fly(&mut physics, &mut plane, input, 30);
    assert!(plane.state(&physics).expect("state").bank < -0.05);
}

#[test]
fn positive_pitch_and_yaw_inputs_raise_and_turn_the_nose_right() {
    let trim = solve(&trainer(), &TrimTarget::level(50.0, 1000.0)).expect("trim");
    let run = |input: AircraftInput| {
        let mut physics = world(false);
        let mut plane = Aircraft::spawn(
            &mut physics,
            trainer(),
            Vec3::new(0.0, 1000.0, 0.0),
            trim.rotation(),
            trim.velocity(),
        )
        .expect("spawn");
        fly(&mut physics, &mut plane, input, 20);
        plane.state(&physics).expect("state")
    };
    let pulled = run(AircraftInput {
        pitch: (trim.pitch_input + 0.3).min(1.0),
        ..trim.input()
    });
    assert!(pulled.pitch > trim.pitch_attitude + 0.01, "{pulled:?}");
    let yawed = run(AircraftInput {
        yaw: 1.0,
        ..trim.input()
    });
    assert!(yawed.heading > 0.005, "{yawed:?}");
}

/// Settles the trainer on the runway with the brakes set, then runs the take-off: full throttle,
/// brakes off, pitch 0.5 above 27 m/s. Returns the lift-off distance (m) from brake release, the
/// lift-off step, and the world digest after `steps` steps.
fn take_off(steps: usize) -> (Option<f32>, Option<usize>, u64) {
    let mut physics = world(true);
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 1.25, 0.0),
        Quat::IDENTITY,
        Vec3::ZERO,
    )
    .expect("spawn");
    fly(
        &mut physics,
        &mut plane,
        AircraftInput {
            brakes: 1.0,
            ..AircraftInput::default()
        },
        120,
    );
    let start = plane.state(&physics).expect("state").translation;
    let env = AeroEnvironment::default();
    let (mut lift_off, mut lift_step, mut airborne) = (None, None, 0);
    for i in 0..steps {
        let state = plane.state(&physics).expect("state");
        let pitch = if state.air.airspeed > 27.0 { 0.5 } else { 0.0 };
        let input = AircraftInput {
            throttle: 1.0,
            pitch,
            ..AircraftInput::default()
        };
        plane
            .update_with_layers(&mut physics, input, &env, DT, None)
            .expect("update");
        step(&mut physics);
        let state = plane.state(&physics).expect("state");
        if plane.gear().iter().all(|g| !g.grounded) {
            airborne += 1;
            if airborne == 30 && lift_off.is_none() {
                lift_step = Some(i - 29);
                lift_off = Some((start.z - state.translation.z) - 29.0 * DT * state.linear_velocity.length());
            }
        } else {
            airborne = 0;
        }
    }
    (lift_off, lift_step, physics.state_digest())
}

#[test]
fn the_take_off_run_is_deterministic_and_in_range() {
    let (distance, step_a, digest_a) = take_off(1500);
    let (distance_b, step_b, digest_b) = take_off(1500);
    println!("lift-off after {distance:?} m at step {step_a:?}; digest {digest_a:#018x}");
    assert_eq!(digest_a, digest_b);
    assert_eq!(step_a, step_b);
    assert_eq!(distance.map(f32::to_bits), distance_b.map(f32::to_bits));
    let distance = distance.expect("the plane left the ground");
    assert!(
        (120.0..=500.0).contains(&distance),
        "lift-off distance {distance} m"
    );
}

/// The take-off scenario's world digest after 900 steps, captured on Linux x86_64 (ADR-3374). Rapier's
/// floats go through libm, so the number is per target, like the determinism matrix.
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    not(feature = "physics-cross-platform-determinism")
))]
const TAKE_OFF_PIN: u64 = 0xb828_c99b_6e9f_ce06;

#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    not(feature = "physics-cross-platform-determinism")
))]
#[test]
fn the_take_off_digest_is_pinned() {
    let (_, _, digest) = take_off(900);
    assert_eq!(digest, TAKE_OFF_PIN, "take-off digest is {digest:#018x}");
}

#[test]
fn a_parked_plane_settles_without_drifting() {
    for brakes in [1.0, 0.0] {
        let mut physics = world(true);
        let mut plane = Aircraft::spawn(
            &mut physics,
            trainer(),
            Vec3::new(0.0, 1.3, 0.0),
            Quat::IDENTITY,
            Vec3::ZERO,
        )
        .expect("spawn");
        let input = AircraftInput {
            brakes,
            ..AircraftInput::default()
        };
        fly(&mut physics, &mut plane, input, 300);
        let settled = plane.state(&physics).expect("state");
        fly(&mut physics, &mut plane, input, 600);
        let later = plane.state(&physics).expect("state");
        let drift = later.translation - settled.translation;
        println!(
            "brakes {brakes}: settled at {:?}, drift {drift:?}",
            settled.translation
        );
        assert_eq!(later.grounded_gear, 3);
        assert!(drift.length() < 2.0e-3, "drifted {drift:?}");
        assert!(later.linear_velocity.length() < 1.0e-2, "{later:?}");
        assert!(settled.pitch.abs() < 0.05, "{settled:?}");
    }
}

#[test]
fn zero_airspeed_gives_no_nan() {
    let mut physics = world(false);
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 100.0, 0.0),
        Quat::IDENTITY,
        Vec3::ZERO,
    )
    .expect("spawn");
    let input = AircraftInput {
        throttle: 1.0,
        pitch: 1.0,
        roll: -1.0,
        yaw: 1.0,
        flaps: 1.0,
        brakes: 1.0,
    };
    let env = AeroEnvironment::default();
    plane
        .update_with_layers(&mut physics, input, &env, DT, None)
        .expect("update");
    let state = plane.state(&physics).expect("state");
    assert_eq!(state.air.alpha, 0.0);
    assert_eq!(state.air.airspeed, 0.0);
    assert!(state.cl == 0.0 && state.cd == 0.0);
    // Moving with the wind: zero airspeed again.
    let wind = AeroEnvironment {
        wind: Vec3::new(0.0, 0.0, -20.0),
        ..AeroEnvironment::default()
    };
    let mut physics = world(false);
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 100.0, 0.0),
        Quat::IDENTITY,
        Vec3::new(0.0, 0.0, -20.0),
    )
    .expect("spawn");
    for _ in 0..120 {
        plane
            .update_with_layers(&mut physics, input, &wind, DT, None)
            .expect("update");
        step(&mut physics);
        let state = plane.state(&physics).expect("state");
        assert!(
            state.translation.is_finite() && state.linear_velocity.is_finite(),
            "{state:?}"
        );
        assert!(state.air.alpha.is_finite() && state.air.beta.is_finite() && state.cl.is_finite());
        assert!(plane.surfaces().iter().all(|s| s.force.is_finite()));
    }
}

#[test]
fn a_high_angle_of_attack_sets_the_stall_flag() {
    let mut physics = world(false);
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 500.0, 0.0),
        Quat::from_rotation_x(25.0_f32.to_radians()),
        Vec3::new(0.0, 0.0, -30.0),
    )
    .expect("spawn");
    fly(&mut physics, &mut plane, AircraftInput::default(), 1);
    let stalled = plane.state(&physics).expect("state");
    assert!(stalled.stalled, "{stalled:?}");
    assert!(stalled.air.alpha > 20.0_f32.to_radians());
    let mut physics = world(false);
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 500.0, 0.0),
        Quat::from_rotation_x(10.0_f32.to_radians()),
        Vec3::new(0.0, 0.0, -30.0),
    )
    .expect("spawn");
    fly(&mut physics, &mut plane, AircraftInput::default(), 1);
    let flying = plane.state(&physics).expect("state");
    assert!(!flying.stalled);
    assert!(stalled.cl < flying.cl, "{} vs {}", stalled.cl, flying.cl);
}

#[test]
fn bad_steps_and_poses_are_named() {
    let mut physics = world(false);
    assert_eq!(
        Aircraft::spawn(
            &mut physics,
            trainer(),
            Vec3::new(f32::NAN, 0.0, 0.0),
            Quat::IDENTITY,
            Vec3::ZERO
        )
        .map(|_| ())
        .map_err(|e| e.code()),
        Err("aircraft_invalid_pose")
    );
    let mut plane =
        Aircraft::spawn(&mut physics, trainer(), Vec3::ZERO, Quat::IDENTITY, Vec3::ZERO).expect("spawn");
    let env = AeroEnvironment::default();
    assert_eq!(
        plane
            .update_with_layers(&mut physics, AircraftInput::default(), &env, 0.0, None)
            .map_err(|e| e.code()),
        Err("aircraft_invalid_time_step")
    );
    let body = plane.chassis().to_handle();
    physics.remove_body(body);
    assert_eq!(
        plane
            .update_with_layers(&mut physics, AircraftInput::default(), &env, DT, None)
            .map_err(|e| e.code()),
        Err("aircraft_chassis_missing")
    );
}

/// Prints a trimmed level flight every 2 s (hand-run: `--ignored --nocapture`).
#[test]
#[ignore = "diagnostic print, no assertions"]
fn print_the_trimmed_flight() {
    let trim = solve(&trainer(), &TrimTarget::level(50.0, 1000.0)).expect("trim");
    let mut physics = world(false);
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 1000.0, 0.0),
        trim.rotation(),
        trim.velocity(),
    )
    .expect("spawn");
    let env = AeroEnvironment::default();
    for i in 0..3600 {
        plane
            .update_with_layers(&mut physics, trim.input(), &env, DT, None)
            .expect("update");
        if i < 3 || i % 120 == 0 {
            let s = plane.state(&physics).expect("state");
            let l = plane.loads();
            let net = l.aero_force + l.thrust_force + GRAVITY * 1000.0;
            println!(
                "trace {i:4} y {:9.4} vy {:8.5} V {:8.4} pitch {:8.5} alpha {:8.5} q {:9.6} net {:?} torque {:?}",
                s.translation.y,
                s.linear_velocity.y,
                s.linear_velocity.length(),
                s.pitch.to_degrees(),
                s.air.alpha.to_degrees(),
                s.body_rates.x,
                net,
                l.torque
            );
        }
        step(&mut physics);
    }
}
