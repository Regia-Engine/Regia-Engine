// SPDX-License-Identifier: MPL-2.0
//! Ground-vehicle tests (ADR-8551). The comparison with `regia_physics::vehicle_track` lives in that
//! crate (`tracked_defaults_match_vehicle_track`): this crate does not depend on it.

use super::tracked::{self, TrackedInput, TrackedParams, TrackedState};
use super::wheeled::{self, WheeledInput, WheeledParams, WheeledState};
use super::*;
use crate::golden::{self, DetScenario, GoldenMode, REPORT_PREFIX};
use crate::hover::host::{FlatFloor, FloorFn};

/// 1/64 s is exact in f32.
const DT: f32 = 1.0 / 64.0;
const EARTH: WorldPhysics = WorldPhysics::EARTH;

fn tank_run<S: FloorSampler>(
    params: &TrackedParams,
    floor: &S,
    state: &mut TrackedState,
    input: TrackedInput,
    steps: u32,
) -> u32 {
    let mut limited = 0;
    for _ in 0..steps {
        let report = tracked::step(params, &EARTH, floor, state, &input, DT);
        assert!(!report.recovered);
        limited += u32::from(report.grade_limited);
    }
    limited
}

fn car_run<S: FloorSampler>(
    params: &WheeledParams,
    floor: &S,
    state: &mut WheeledState,
    input: WheeledInput,
    steps: u32,
) -> u32 {
    let mut limited = 0;
    for _ in 0..steps {
        let report = wheeled::step(params, &EARTH, floor, state, &input, DT);
        assert!(!report.recovered);
        limited += u32::from(report.grade_limited);
    }
    limited
}

fn throttle(t: f32) -> WheeledInput {
    WheeledInput {
        throttle: t,
        ..WheeledInput::default()
    }
}

fn tracks(left: f32, right: f32) -> TrackedInput {
    TrackedInput {
        left_throttle: left,
        right_throttle: right,
    }
}

/// A plane rising `rise` meters per meter toward −Z (ahead of a nose at yaw 0).
fn ramp(rise: f32) -> FloorFn<impl Fn(f32, f32) -> f32> {
    FloorFn(move |_x: f32, z: f32| -z * rise)
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dz) = (a[0] - b[0], a[2] - b[2]);
    math::sqrt(dx * dx + dz * dz)
}

#[test]
fn default_params_are_valid_and_tracked_defaults_are_vehicle_track_constants() {
    assert!(TrackedParams::DEFAULT.validate().is_ok());
    assert!(WheeledParams::DEFAULT.validate().is_ok());
    let p = TrackedParams::DEFAULT;
    assert_eq!(
        (
            p.separation_m,
            p.max_track_speed_m_s,
            p.friction_accel_m_s2,
            p.coast_drag_m_s2,
            p.pivot_turn
        ),
        (2.0, 8.0, 4.0, 1.0, true)
    );
    for t in GroundVehicleMix::tracked_types() {
        assert!(t.validate().is_ok());
    }
    for w in GroundVehicleMix::wheeled_types() {
        assert!(w.validate().is_ok());
    }
}

#[test]
fn validate_names_the_first_bad_field() {
    let bad = WheeledParams {
        max_steer_rad: f32::NAN,
        lateral_grip_m_s2: -1.0,
        ..WheeledParams::DEFAULT
    };
    let error = bad.validate().expect_err("NaN steer");
    assert_eq!((error.field, error.kind.code()), ("max_steer_rad", "not_finite"));
    let bad = WheeledParams {
        lateral_grip_m_s2: 0.0,
        ..WheeledParams::DEFAULT
    };
    let error = bad.validate().expect_err("zero grip");
    assert_eq!(
        (error.field, error.kind.code()),
        ("lateral_grip_m_s2", "out_of_range")
    );
    let bad = TrackedParams {
        grade_speed_loss: 1.5,
        ..TrackedParams::DEFAULT
    };
    let error = bad.validate().expect_err("loss above 1");
    assert_eq!(
        (error.field, error.kind),
        ("grade_speed_loss", GroundParamsErrorKind::OutOfRange)
    );
    assert!(
        error
            .to_string()
            .starts_with("out_of_range: field `grade_speed_loss`")
    );
    let ok = TrackedParams {
        coast_drag_m_s2: 0.0,
        ..TrackedParams::DEFAULT
    };
    assert!(ok.validate().is_ok(), "a zero coast drag is in range");
    for kind in GroundParamsErrorKind::ALL {
        assert!(kind.code().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
    }
}

#[test]
fn pivot_turn_spins_in_place() {
    // Tracked with pivot turns: opposite full throttles spin about the center.
    let mut tank = TrackedState::default();
    tank_run(
        &TrackedParams::DEFAULT,
        &FlatFloor(0.0),
        &mut tank,
        tracks(1.0, -1.0),
        64,
    );
    assert!(
        tank.center_speed().abs() < 1e-3,
        "center speed {}",
        tank.center_speed()
    );
    assert!(
        tank.pos[0].abs() < 1e-3 && tank.pos[2].abs() < 1e-3,
        "the center did not translate"
    );
    assert!(
        tank.yaw < -0.5,
        "a faster left track yaws right, yaw {}",
        tank.yaw
    );

    // Without pivot turns: equal and opposite throttles brake both tracks, no spin.
    let no_pivot = TrackedParams {
        pivot_turn: false,
        ..TrackedParams::DEFAULT
    };
    let mut held = TrackedState::default();
    tank_run(&no_pivot, &FlatFloor(0.0), &mut held, tracks(1.0, -1.0), 64);
    assert_eq!(
        (held.yaw, held.pos, held.left_speed, held.right_speed),
        (0.0, [0.0; 3], 0.0, 0.0)
    );
    // An opposing weaker track is braked: the body turns about it, never backing that track.
    let mut about = TrackedState::default();
    tank_run(&no_pivot, &FlatFloor(0.0), &mut about, tracks(1.0, -0.5), 64);
    assert_eq!(about.right_speed, 0.0);
    assert!(about.left_speed > 3.0 && about.yaw < -0.5 && about.center_speed() > 1.0);

    // Wheeled: the bicycle model cannot pivot. Full lock at rest yaws nothing and moves nothing.
    let mut car = WheeledState::default();
    let lock = WheeledInput {
        steer: 1.0,
        ..WheeledInput::default()
    };
    car_run(&WheeledParams::DEFAULT, &FlatFloor(0.0), &mut car, lock, 64);
    assert_eq!(car.steer, WheeledParams::DEFAULT.max_steer_rad);
    assert_eq!((car.yaw, car.pos, car.speed), (0.0, [0.0; 3], 0.0));
}

#[test]
fn grade_limit_stops_the_climb() {
    // 45° is past both default limits (35° tracked, 30° wheeled); 15° is under both.
    let steep = ramp(1.0);
    let gentle = ramp(0.268);
    let seconds = 5 * 64;

    let mut tank = TrackedState::default();
    let limited = tank_run(
        &TrackedParams::DEFAULT,
        &steep,
        &mut tank,
        tracks(1.0, 1.0),
        seconds,
    );
    assert_eq!(limited, seconds, "every step refused the climb");
    assert_eq!((tank.pos[2], tank.left_speed), (0.0, 0.0));
    assert!(
        (tank.pitch - core::f32::consts::FRAC_PI_4).abs() < 1e-5,
        "pitch {}",
        tank.pitch
    );
    let mut climber = TrackedState::default();
    assert_eq!(
        tank_run(
            &TrackedParams::DEFAULT,
            &gentle,
            &mut climber,
            tracks(1.0, 1.0),
            seconds
        ),
        0
    );
    assert!(
        climber.pos[2] < -5.0 && climber.pos[1] > 1.0,
        "climbed to {:?}",
        climber.pos
    );
    // The grade costs speed below the limit: 15° of 35° at a 50 % loss is about 79 %.
    let expected = 8.0 * (1.0 - 0.5 * (math::atan2(0.268, 1.0) / (35.0 * DEG_TO_RAD)));
    assert!(climber.left_speed <= expected + 1e-4 && climber.left_speed > expected - 0.5);
    // Reversing down the steep ramp is not a climb.
    let mut down = TrackedState::default();
    assert_eq!(
        tank_run(&TrackedParams::DEFAULT, &steep, &mut down, tracks(-1.0, -1.0), 64),
        0
    );
    assert!(down.pos[2] > 0.5);

    let mut car = WheeledState::default();
    let limited = car_run(&WheeledParams::DEFAULT, &steep, &mut car, throttle(1.0), seconds);
    assert_eq!(limited, seconds);
    assert_eq!((car.pos[2], car.speed), (0.0, 0.0));
    let mut climber = WheeledState::default();
    assert_eq!(
        car_run(
            &WheeledParams::DEFAULT,
            &gentle,
            &mut climber,
            throttle(1.0),
            seconds
        ),
        0
    );
    assert!(climber.pos[2] < -20.0, "climbed to {:?}", climber.pos);
    // A car that reaches a steep ramp at speed brakes to a stop on it.
    let knee = FloorFn(|_x: f32, z: f32| if z < -20.0 { -(z + 20.0) * 1.5 } else { 0.0 });
    let mut runner = WheeledState {
        speed: 15.0,
        ..WheeledState::default()
    };
    car_run(
        &WheeledParams::DEFAULT,
        &knee,
        &mut runner,
        throttle(1.0),
        10 * 64,
    );
    assert_eq!(runner.speed, 0.0, "stopped on the ramp");
    assert!(
        runner.pos[2] < -15.0 && runner.pos[2] > -40.0,
        "stopped near the knee {:?}",
        runner.pos
    );
}

#[test]
fn coasting_on_a_slope_rolls_back_and_drag_holds_a_gentle_one() {
    // 20°: g sin(20°) = 3.4 m/s² beats the 1 m/s² tracked drag and the 1.5 m/s² car coast.
    let slope = ramp(0.364);
    let mut tank = TrackedState::default();
    tank_run(&TrackedParams::DEFAULT, &slope, &mut tank, tracks(0.0, 0.0), 64);
    assert!(
        tank.center_speed() < -1.0 && tank.pos[2] > 0.5,
        "rolled back down: {tank:?}"
    );
    let mut car = WheeledState::default();
    car_run(&WheeledParams::DEFAULT, &slope, &mut car, throttle(0.0), 64);
    assert!(car.speed < -1.0 && car.pos[2] > 0.5);
    // 3°: g sin(3°) = 0.51 m/s², under both drags: the vehicles hold.
    let flatish = ramp(0.0524);
    let mut tank = TrackedState::default();
    tank_run(&TrackedParams::DEFAULT, &flatish, &mut tank, tracks(0.0, 0.0), 64);
    assert_eq!((tank.left_speed, tank.pos[2]), (0.0, 0.0));
    // Lower gravity rolls slower.
    let moon = WorldPhysics {
        gravity_m_s2: 1.62,
        ..WorldPhysics::EARTH
    };
    let mut light = WheeledState::default();
    for _ in 0..64 {
        wheeled::step(
            &WheeledParams::DEFAULT,
            &moon,
            &slope,
            &mut light,
            &throttle(0.0),
            DT,
        );
    }
    assert_eq!(light.speed, 0.0, "1.62 sin(20°) = 0.55 m/s² is under the coast");
}

#[test]
fn bicycle_turn_radius_is_wheelbase_over_tan_steer() {
    let params = WheeledParams {
        coast_m_s2: 0.0,
        ..WheeledParams::DEFAULT
    };
    for steer in [0.1_f32, 0.3, 0.5] {
        // 2 m/s held (no coast drag on flat ground): lateral 4 tan(steer) / 2.7 is under the grip.
        let mut car = WheeledState {
            speed: 2.0,
            steer,
            ..WheeledState::default()
        };
        let input = WheeledInput {
            steer: steer / params.max_steer_rad,
            ..WheeledInput::default()
        };
        let expected = wheeled::turn_radius(params.wheelbase_m, steer);
        // One full circle and a bit: the widest distance from the start is the diameter.
        let steps = (2.2 * core::f32::consts::PI * expected / (2.0 * DT)) as u32;
        let start = car.pos;
        let mut widest = 0.0_f32;
        // The published yaw is wrapped to (-PI, PI] (ADR-10062): sum the per-tick turn instead.
        let mut turned = 0.0_f32;
        for _ in 0..steps {
            let before = car.yaw;
            wheeled::step(&params, &EARTH, &FlatFloor(0.0), &mut car, &input, DT);
            turned += wrap_delta(car.yaw, before);
            assert_eq!(car.slip, 0.0);
            let d = distance(start, car.pos);
            if d > widest {
                widest = d;
            }
        }
        let measured = 0.5 * widest;
        assert!(
            (measured - expected).abs() <= 0.01 * expected,
            "steer {steer}: radius {measured} m, L / tan(steer) = {expected} m"
        );
        assert!(
            turned < -6.0,
            "positive steer turns right (negative yaw), turned {turned}"
        );
    }
}

#[test]
fn lateral_grip_caps_yaw_rate_at_speed() {
    let params = WheeledParams {
        coast_m_s2: 0.0,
        ..WheeledParams::DEFAULT
    };
    let lock = WheeledInput {
        steer: 1.0,
        ..WheeledInput::default()
    };
    let mut fast = WheeledState {
        speed: 30.0,
        steer: params.max_steer_rad,
        ..WheeledState::default()
    };
    for _ in 0..64 {
        let yaw_before = fast.yaw;
        wheeled::step(&params, &EARTH, &FlatFloor(0.0), &mut fast, &lock, DT);
        let yaw_rate = (fast.yaw - yaw_before) / DT;
        let lateral = (fast.speed * yaw_rate).abs();
        assert!(
            lateral <= params.lateral_grip_m_s2 * 1.001,
            "lateral {lateral} m/s² over the {} m/s² grip",
            params.lateral_grip_m_s2
        );
        assert!(
            lateral >= params.lateral_grip_m_s2 * 0.999,
            "capped at the grip, not under it"
        );
    }
    // Kinematic: 30 tan(0.6) / 2.7 = 7.6 rad/s; the grip allows 8 / 30 = 0.27: most of it slips.
    assert!(fast.slip > 0.9, "slip {}", fast.slip);
    // The handbrake doubles the cap while it brakes.
    let mut skid = WheeledState {
        speed: 30.0,
        steer: params.max_steer_rad,
        ..WheeledState::default()
    };
    let yank = WheeledInput {
        handbrake: true,
        ..lock
    };
    wheeled::step(&params, &EARTH, &FlatFloor(0.0), &mut skid, &yank, DT);
    let v_mid = 0.5 * (30.0 + skid.speed);
    let yaw_rate = skid.yaw / DT;
    assert!(((v_mid * yaw_rate).abs() - 2.0 * params.lateral_grip_m_s2).abs() < 0.05);
    assert!(skid.speed < 30.0);
}

#[test]
fn reverse_speed_cap() {
    let params = WheeledParams::DEFAULT;
    let mut car = WheeledState::default();
    for _ in 0..(10 * 64) {
        wheeled::step(&params, &EARTH, &FlatFloor(0.0), &mut car, &throttle(-1.0), DT);
        assert!(car.speed >= -params.reverse_speed_m_s, "speed {}", car.speed);
    }
    assert_eq!(car.speed, -params.reverse_speed_m_s, "settles on the cap");
    assert!(car.pos[2] > 30.0, "reverse backs toward +Z");
    // Forward cap, and a reverse request at speed brakes first (brake rate, not accel).
    let mut fwd = WheeledState::default();
    car_run(&params, &FlatFloor(0.0), &mut fwd, throttle(1.0), 20 * 64);
    assert_eq!(fwd.speed, params.max_speed_m_s);
    let before = fwd.speed;
    wheeled::step(&params, &EARTH, &FlatFloor(0.0), &mut fwd, &throttle(-1.0), DT);
    assert!((before - fwd.speed - params.brake_m_s2 * DT).abs() < 1e-4);
    // Half throttle eases off at the coast rate.
    let mut ease = WheeledState {
        speed: 20.0,
        ..WheeledState::default()
    };
    wheeled::step(&params, &EARTH, &FlatFloor(0.0), &mut ease, &throttle(0.25), DT);
    assert!((20.0 - ease.speed - params.coast_m_s2 * DT).abs() < 1e-5);
}

#[test]
fn ground_vehicles_ignore_nan_input() {
    let start_tank = TrackedState {
        left_speed: 3.0,
        right_speed: 2.0,
        yaw: 0.4,
        ..TrackedState::default()
    };
    let start_car = WheeledState {
        speed: 6.0,
        steer: 0.1,
        yaw: -0.3,
        ..WheeledState::default()
    };
    let field = GroundField;
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        // A NaN axis reads as 0, bit for bit.
        let (mut a, mut b) = (start_tank, start_tank);
        tracked::step(
            &TrackedParams::DEFAULT,
            &EARTH,
            &field,
            &mut a,
            &tracks(bad, 0.5),
            DT,
        );
        tracked::step(
            &TrackedParams::DEFAULT,
            &EARTH,
            &field,
            &mut b,
            &tracks(0.0, 0.5),
            DT,
        );
        assert_eq!(a.words().map(f32::to_bits), b.words().map(f32::to_bits));
        let (mut a, mut b) = (start_car, start_car);
        let nan_in = WheeledInput {
            throttle: bad,
            brake: bad,
            steer: bad,
            handbrake: false,
        };
        wheeled::step(&WheeledParams::DEFAULT, &EARTH, &field, &mut a, &nan_in, DT);
        wheeled::step(
            &WheeledParams::DEFAULT,
            &EARTH,
            &field,
            &mut b,
            &WheeledInput::default(),
            DT,
        );
        assert_eq!(a.words().map(f32::to_bits), b.words().map(f32::to_bits));
        // A bad world gravity reads as Earth's.
        let bad_world = WorldPhysics {
            gravity_m_s2: bad,
            ..WorldPhysics::EARTH
        };
        let (mut c, mut d) = (start_car, start_car);
        wheeled::step(
            &WheeledParams::DEFAULT,
            &bad_world,
            &field,
            &mut c,
            &WheeledInput::default(),
            DT,
        );
        wheeled::step(
            &WheeledParams::DEFAULT,
            &EARTH,
            &field,
            &mut d,
            &WheeledInput::default(),
            DT,
        );
        assert_eq!(c.words().map(f32::to_bits), d.words().map(f32::to_bits));
    }
    // A NaN floor keeps the previous height and stays finite.
    let holes = FloorFn(|x: f32, _z: f32| if x > 0.5 { f32::NAN } else { 2.0 });
    let mut car = WheeledState::at_rest([0.0, 2.0, 0.0], -core::f32::consts::FRAC_PI_2);
    car_run(&WheeledParams::DEFAULT, &holes, &mut car, throttle(1.0), 128);
    assert!(car.pos[0] > 1.0 && car.pos[1] == 2.0 && car.words().iter().all(|w| w.is_finite()));
    let mut tank = TrackedState::at_rest([0.0, 2.0, 0.0], -core::f32::consts::FRAC_PI_2);
    tank_run(&TrackedParams::DEFAULT, &holes, &mut tank, tracks(1.0, 1.0), 128);
    assert!(tank.pos[0] > 1.0 && tank.pos[1] == 2.0 && tank.words().iter().all(|w| w.is_finite()));
    // A poisoned state resets to rest in place and says so.
    let mut poisoned = WheeledState {
        pos: [3.0, 0.0, f32::NAN],
        speed: f32::INFINITY,
        yaw: 1.0,
        ..WheeledState::default()
    };
    let report = wheeled::step(
        &WheeledParams::DEFAULT,
        &EARTH,
        &FlatFloor(1.5),
        &mut poisoned,
        &throttle(1.0),
        DT,
    );
    assert!(report.recovered);
    assert_eq!(poisoned, WheeledState::at_rest([3.0, 1.5, 0.0], 1.0));
    let mut poisoned = TrackedState {
        left_speed: f32::NAN,
        ..start_tank
    };
    let report = tracked::step(
        &TrackedParams::DEFAULT,
        &EARTH,
        &FlatFloor(0.0),
        &mut poisoned,
        &tracks(1.0, 1.0),
        DT,
    );
    assert!(report.recovered);
    assert_eq!(poisoned, TrackedState::at_rest([0.0; 3], 0.4));
}

#[test]
fn zero_dt_changes_nothing() {
    let tank = TrackedState {
        pos: [1.0, 2.0, 3.0],
        left_speed: 3.0,
        right_speed: -2.0,
        yaw: 0.4,
        pitch: 0.1,
        roll: -0.1,
    };
    let car = WheeledState {
        pos: [1.0, 2.0, 3.0],
        speed: 6.0,
        steer: 0.1,
        yaw: -0.3,
        slip: 0.2,
        pitch: 0.05,
        roll: 0.01,
    };
    for dt in [0.0, -0.0, -DT, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut t = tank;
        let report = tracked::step(
            &TrackedParams::DEFAULT,
            &EARTH,
            &GroundField,
            &mut t,
            &tracks(1.0, -1.0),
            dt,
        );
        assert_eq!(report, GroundReport::default());
        assert_eq!(
            t.words().map(f32::to_bits),
            tank.words().map(f32::to_bits),
            "dt {dt}"
        );
        let mut c = car;
        let full = WheeledInput {
            throttle: 1.0,
            brake: 0.0,
            steer: 1.0,
            handbrake: true,
        };
        let report = wheeled::step(&WheeledParams::DEFAULT, &EARTH, &GroundField, &mut c, &full, dt);
        assert_eq!(report, GroundReport::default());
        assert_eq!(
            c.words().map(f32::to_bits),
            car.words().map(f32::to_bits),
            "dt {dt}"
        );
    }
    // A poisoned state is not touched either when dt is bad (the dt guard runs first).
    let mut poisoned = WheeledState {
        speed: f32::NAN,
        ..car
    };
    let report = wheeled::step(
        &WheeledParams::DEFAULT,
        &EARTH,
        &GroundField,
        &mut poisoned,
        &throttle(1.0),
        0.0,
    );
    assert!(!report.recovered && poisoned.speed.is_nan());
}

#[test]
fn ground_vehicle_mix_is_repeatable_and_exercises_the_models() {
    let mut a = GroundVehicleMix::new();
    let mut b = GroundVehicleMix::new();
    let run = a.default_run();
    let ta = golden::record(&mut a, &run);
    let tb = golden::record(&mut b, &run);
    assert_eq!(ta.tick_hashes, tb.tick_hashes);
    assert_eq!(a.recovered_steps(), 0);
    assert!(
        a.grade_limited_steps() > 20,
        "grade limits hit {} times",
        a.grade_limited_steps()
    );
    for w in 0..4 {
        assert_eq!(a.tanks(w).len(), 8);
        assert!(a.tanks(w).iter().all(|s| s.words().iter().all(|v| v.is_finite())));
        assert!(a.cars(w).iter().all(|s| s.words().iter().all(|v| v.is_finite())));
        assert!(a.cars(w).iter().any(|s| s.slip > 0.0 || s.pitch.abs() > 0.05));
    }
    // The worlds differ: gravity reaches the state.
    assert_ne!(
        a.cars(0)[0].words().map(f32::to_bits),
        a.cars(3)[0].words().map(f32::to_bits)
    );
}

/// Re-recorded 2026-11-20 (ADR-10062): the published yaw is wrapped to (-PI, PI] after every step,
/// so the hashed words differ from tick 22 on, where the first vehicle turns past PI (was
/// 4987f794ad0b2650 on x86_64-linux, aarch64-macos and aarch64-android).
#[test]
fn ground_vehicle_mix_matches_its_committed_golden() {
    let mut scenario = GroundVehicleMix::new();
    let report = golden::check_golden(
        &mut scenario,
        &golden::engine_golden_dir(),
        GoldenMode::from_env(),
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");
}

#[test]
fn digest_words_folds_zero_and_nan() {
    assert_eq!(digest_words(&[0.0, 1.0]), digest_words(&[-0.0, 1.0]));
    assert_eq!(digest_words(&[f32::NAN]), digest_words(&[-f32::NAN]));
    assert_ne!(digest_words(&[1.0, 2.0]), digest_words(&[2.0, 1.0]));
}

/// `params` with the field named `name` set to `value`.
fn tracked_with(mut params: TrackedParams, name: &str, value: f32) -> TrackedParams {
    match name {
        "separation_m" => params.separation_m = value,
        "max_track_speed_m_s" => params.max_track_speed_m_s = value,
        "friction_accel_m_s2" => params.friction_accel_m_s2 = value,
        "coast_drag_m_s2" => params.coast_drag_m_s2 = value,
        "grade_limit_deg" => params.grade_limit_deg = value,
        "grade_speed_loss" => params.grade_speed_loss = value,
        "half_length_m" => params.half_length_m = value,
        other => panic!("no tracked field {other}"),
    }
    params
}

/// `params` with the field named `name` set to `value`.
fn wheeled_with(mut params: WheeledParams, name: &str, value: f32) -> WheeledParams {
    match name {
        "wheelbase_m" => params.wheelbase_m = value,
        "max_steer_rad" => params.max_steer_rad = value,
        "steer_rate_rad_s" => params.steer_rate_rad_s = value,
        "max_speed_m_s" => params.max_speed_m_s = value,
        "reverse_speed_m_s" => params.reverse_speed_m_s = value,
        "accel_m_s2" => params.accel_m_s2 = value,
        "brake_m_s2" => params.brake_m_s2 = value,
        "coast_m_s2" => params.coast_m_s2 = value,
        "lateral_grip_m_s2" => params.lateral_grip_m_s2 = value,
        "grade_limit_deg" => params.grade_limit_deg = value,
        "track_width_m" => params.track_width_m = value,
        other => panic!("no wheeled field {other}"),
    }
    params
}

/// The smallest and largest value `range` accepts.
fn range_edges(range: &FieldRange) -> [f32; 2] {
    let low = if range.min_exclusive {
        range.min.next_up()
    } else {
        range.min
    };
    [low, range.max]
}

/// Hard driving on three floors at the agent's shortest and longest `dt`: every step publishes only
/// finite words and never takes the recovery.
fn assert_tracked_finite(params: &TrackedParams, label: &str) {
    assert!(params.validate().is_ok(), "{label}: {params:?}");
    let plane = FloorFn(|x: f32, z: f32| 0.3 * x - 0.7 * z);
    let inputs = [
        tracks(1.0, -1.0),
        tracks(1.0, 1.0),
        tracks(-1.0, -1.0),
        tracks(1.0, 0.0),
        tracks(0.0, 0.0),
    ];
    for dt in [DT, 1.0] {
        for floor in [&FlatFloor(0.0) as &dyn FloorSampler, &plane, &GroundField] {
            let mut tank = TrackedState::default();
            for (i, input) in inputs.iter().cycle().take(5 * 48).enumerate() {
                let report = tracked::step(params, &EARTH, floor, &mut tank, input, dt);
                assert!(
                    !report.recovered && tank.words().iter().all(|w| w.is_finite()),
                    "{label}: step {i} at dt {dt} published {:?} (recovered {})",
                    tank.words().map(f32::to_bits),
                    report.recovered
                );
            }
        }
    }
}

/// [`assert_tracked_finite`] for the wheeled model.
fn assert_wheeled_finite(params: &WheeledParams, label: &str) {
    assert!(params.validate().is_ok(), "{label}: {params:?}");
    let plane = FloorFn(|x: f32, z: f32| 0.3 * x - 0.7 * z);
    let inputs = [
        WheeledInput {
            throttle: 1.0,
            steer: 1.0,
            ..WheeledInput::default()
        },
        WheeledInput {
            throttle: -1.0,
            steer: -1.0,
            ..WheeledInput::default()
        },
        WheeledInput {
            throttle: 1.0,
            steer: 1.0,
            handbrake: true,
            ..WheeledInput::default()
        },
        WheeledInput {
            brake: 1.0,
            steer: -1.0,
            ..WheeledInput::default()
        },
        WheeledInput {
            steer: 1.0,
            ..WheeledInput::default()
        },
    ];
    for dt in [DT, 1.0] {
        for floor in [&FlatFloor(0.0) as &dyn FloorSampler, &plane, &GroundField] {
            let mut car = WheeledState::default();
            for (i, input) in inputs.iter().cycle().take(5 * 48).enumerate() {
                let report = wheeled::step(params, &EARTH, floor, &mut car, input, dt);
                assert!(
                    !report.recovered && car.words().iter().all(|w| w.is_finite()),
                    "{label}: step {i} at dt {dt} published {:?} (recovered {})",
                    car.words().map(f32::to_bits),
                    report.recovered
                );
            }
        }
    }
}

/// ADR-8638 (review finding A): every parameter at both ends of its documented range, alone and
/// all at once, steps finite words. x86-64 and aarch64 differ only in the payload of a hardware
/// NaN, so a step that publishes only finite words is bit-identical on both. Before the fix the
/// smallest separation (`next_up(0)`, the old open range) published a NaN X and Z and an
/// infinite yaw with `recovered` false, and the smallest wheelbase a NaN X, Z and yaw.
#[test]
fn every_documented_parameter_edge_steps_finite_words() {
    for range in TrackedParams::RANGES {
        for value in range_edges(&range) {
            let params = tracked_with(TrackedParams::DEFAULT, range.name, value);
            assert_tracked_finite(&params, &format!("tracked {} = {value:e}", range.name));
        }
    }
    for range in WheeledParams::RANGES {
        for value in range_edges(&range) {
            let params = wheeled_with(WheeledParams::DEFAULT, range.name, value);
            assert_wheeled_finite(&params, &format!("wheeled {} = {value:e}", range.name));
        }
    }
    // Every field at once: all low, all high, then seed-stable mixes of the two edges.
    let mut rng = crate::golden::scenarios::SplitMix64::new(0x8638_ED6E);
    for mix in 0..66_u32 {
        let mut tank = TrackedParams::DEFAULT;
        for range in TrackedParams::RANGES {
            let side = match mix {
                0 => 0,
                1 => 1,
                _ => (rng.next_u64() & 1) as usize,
            };
            tank = tracked_with(tank, range.name, range_edges(&range)[side]);
        }
        tank.pivot_turn = !mix.is_multiple_of(3);
        assert_tracked_finite(&tank, &format!("tracked mix {mix}: {tank:?}"));
        let mut car = WheeledParams::DEFAULT;
        for range in WheeledParams::RANGES {
            let side = match mix {
                0 => 0,
                1 => 1,
                _ => (rng.next_u64() & 1) as usize,
            };
            car = wheeled_with(car, range.name, range_edges(&range)[side]);
        }
        assert_wheeled_finite(&car, &format!("wheeled mix {mix}: {car:?}"));
    }
}

/// ADR-8638 (review finding A): lengths below 1 cm are invalid; 1 cm is the smallest valid one.
/// Before the fix `validate` accepted any length above 0 (`1e-40` passed).
#[test]
fn validate_refuses_a_length_below_one_centimeter() {
    for name in ["separation_m", "half_length_m"] {
        for bad in [1.0e-45, 1.0e-40, 1.0e-3, 0.0099] {
            let error = tracked_with(TrackedParams::DEFAULT, name, bad)
                .validate()
                .expect_err("refused");
            assert_eq!((error.field, error.kind.code()), (name, "out_of_range"));
        }
        assert!(
            tracked_with(TrackedParams::DEFAULT, name, 0.01)
                .validate()
                .is_ok()
        );
    }
    for name in ["wheelbase_m", "track_width_m"] {
        for bad in [1.0e-45, 1.0e-40, 1.0e-3, 0.0099] {
            let error = wheeled_with(WheeledParams::DEFAULT, name, bad)
                .validate()
                .expect_err("refused");
            assert_eq!((error.field, error.kind.code()), (name, "out_of_range"));
        }
        assert!(
            wheeled_with(WheeledParams::DEFAULT, name, 0.01)
                .validate()
                .is_ok()
        );
    }
}

/// ADR-8638 (review finding A): a step whose result overflows (here a host-written speed at
/// `f32::MAX`, or a length `validate` refuses) rests the vehicle at its pose before the step on the
/// same tick and says `recovered`. Before the fix it published NaN and infinite words with
/// `recovered` false, and only the next tick reset.
#[test]
fn an_overflowing_step_rests_on_the_same_tick() {
    let floor = FlatFloor(0.5);
    let mut tank = TrackedState {
        pos: [5.0, 0.0, -3.0],
        yaw: 0.7,
        left_speed: f32::MAX,
        right_speed: -f32::MAX,
        ..TrackedState::default()
    };
    let report = tracked::step(
        &TrackedParams::DEFAULT,
        &EARTH,
        &floor,
        &mut tank,
        &tracks(1.0, -1.0),
        DT,
    );
    assert!(report.recovered, "{:?}", tank.words());
    assert_eq!(tank, TrackedState::at_rest([5.0, 0.5, -3.0], 0.7));

    let mut car = WheeledState {
        pos: [-2.0, 0.0, 4.0],
        yaw: -0.3,
        speed: f32::MAX,
        steer: 0.5,
        ..WheeledState::default()
    };
    let report = wheeled::step(
        &WheeledParams::DEFAULT,
        &EARTH,
        &floor,
        &mut car,
        &throttle(1.0),
        DT,
    );
    assert!(report.recovered, "{:?}", car.words());
    assert_eq!(car, WheeledState::at_rest([-2.0, 0.5, 4.0], -0.3));

    // The review's proof inputs: lengths `validate` now refuses, stepped anyway (a host that skips
    // `validate`). The step still never publishes a non-finite word.
    let tiny_tank = TrackedParams {
        separation_m: 1.0e-40,
        ..TrackedParams::DEFAULT
    };
    let mut tank = TrackedState::default();
    let report = tracked::step(
        &tiny_tank,
        &EARTH,
        &FlatFloor(0.0),
        &mut tank,
        &tracks(1.0, -1.0),
        1.0 / 60.0,
    );
    assert!(
        report.recovered && tank.words().iter().all(|w| w.is_finite()),
        "{:?}",
        tank.words()
    );
    let tiny_car = WheeledParams {
        wheelbase_m: 1.0e-40,
        ..WheeledParams::DEFAULT
    };
    let mut car = WheeledState {
        speed: 10.0,
        steer: 0.2,
        ..WheeledState::default()
    };
    let steer = WheeledInput {
        steer: 1.0,
        ..WheeledInput::default()
    };
    let report = wheeled::step(&tiny_car, &EARTH, &FlatFloor(0.0), &mut car, &steer, 1.0 / 60.0);
    assert!(
        report.recovered && car.words().iter().all(|w| w.is_finite()),
        "{:?}",
        car.words()
    );
}

/// Yaw wrap follow-up (ADR-8638 known issue, ADR-10062): a vehicle that keeps turning publishes a
/// yaw in `(-PI, PI]` on every tick, and the heading it integrates still turns at the step's own
/// rate (the sum of the wrapped per-tick changes matches the unwrapped sum a long pivot builds up).
/// Before the fix the yaw grew without bound (about -30 rad after 60 s of pivot here), losing f32
/// precision as it grew.
#[test]
fn a_long_turn_keeps_the_yaw_wrapped() {
    use core::f32::consts::{PI, TAU};
    let mut tank = TrackedState::default();
    let mut turned = 0.0f32;
    for tick in 0..(60 * 64) {
        let before = tank.yaw;
        let report = tracked::step(
            &TrackedParams::DEFAULT,
            &EARTH,
            &FlatFloor(0.0),
            &mut tank,
            &tracks(1.0, -1.0),
            DT,
        );
        assert!(!report.recovered);
        assert!(
            tank.yaw > -PI && tank.yaw <= PI,
            "tracked tick {tick}: yaw {} outside (-PI, PI]",
            tank.yaw
        );
        turned += wrap_delta(tank.yaw, before);
    }
    assert!(turned < -4.0 * TAU, "the pivot kept turning: {turned} rad");
    let mut car = WheeledState {
        speed: 10.0,
        ..WheeledState::default()
    };
    let lock = WheeledInput {
        throttle: 0.3,
        steer: 1.0,
        ..WheeledInput::default()
    };
    let mut car_turned = 0.0f32;
    for tick in 0..(60 * 64) {
        let before = car.yaw;
        wheeled::step(
            &WheeledParams::DEFAULT,
            &EARTH,
            &FlatFloor(0.0),
            &mut car,
            &lock,
            DT,
        );
        assert!(
            car.yaw > -PI && car.yaw <= PI,
            "wheeled tick {tick}: yaw {} outside (-PI, PI]",
            car.yaw
        );
        car_turned += wrap_delta(car.yaw, before);
    }
    assert!(car_turned < -4.0 * TAU, "the car kept circling: {car_turned} rad");
}

/// The change from `before` to `after` of a yaw wrapped to (-PI, PI], as a turn in (-PI, PI].
fn wrap_delta(after: f32, before: f32) -> f32 {
    use core::f32::consts::{PI, TAU};
    let mut d = after - before;
    if d > PI {
        d -= TAU;
    } else if d <= -PI {
        d += TAU;
    }
    d
}
