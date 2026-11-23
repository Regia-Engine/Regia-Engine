// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/hover.rs, commit d6ed37677), ADR-8012.
// Portions copied from Direct Order 27 (ir-sim/src/math.rs, commit d6ed37677), ADR-8012: the hover call-site test
// (lines 55-75) as `hover_tick_path_uses_math_module`.
// Mark: adapt. Test bodies (lines 991-2260): inputs, tick counts and messages are DO27's. Named changes: ADR-8012 A-8
// names; the handling-file test reads `FlightMode` (Regia's match setting) instead of DO27's file; DO27 hull and map
// names are gone from test names and labels. ADR-8164 (no baseline-game numbers, A-8 P4): the tests step
// `HoverParams::DEFAULT_HULL` (engine-authored) instead of DO27's measured hull, the plain-model regression steps the
// synthetic `plain_test_hull` instead of DO27's v8 row, and every expected value that came from DO27's measurements is
// now derived from the hull the test steps (the share, spring and drop fields), with the tolerance unchanged.

use super::*;

const DT: f32 = crate::tick::tick_dt(); // matches TICK_RATE_HZ

/// A synthetic hull on the plain model (no taper, release drop, deadband, slip grip, lookahead or
/// slope pull): round numbers chosen for this test file, so `plain_model_dynamics` pins the plain
/// model's moment-to-moment feel with hand-checkable arithmetic. Not a game hull.
fn plain_test_hull() -> HoverParams {
    HoverParams {
        ride_height: 1.0,
        coast_drag: 5.0,
        thrust_drag: 1.0,
        height_spring: 16.0,
        height_damper: 6.0,
        lean_pitch_per_speed: 0.2,
        lean_pitch_per_throttle: 0.1,
        lean_roll_per_strafe: 0.1,
        lean_roll_per_yaw_rate: 0.1,
        max_forward_speed: 24.0,
        max_reverse_speed: 12.0,
        max_strafe_speed: 12.0,
        thrust_accel: 25.0,
        brake_accel: 60.0,
        yaw_rate_at_rest: 4.0,
        yaw_rate_at_speed: 2.0,
        yaw_response: 5.0,
        jump_speed: 10.0,
        lateral_drag: 0.0,
        thrust_taper: 0.0,
        yaw_release_response: 0.0,
        yaw_response_at_speed: 0.0,
        yaw_deadband: 0.0,
        roll_slip: 0.0,
        hover_lead: 0.0,
        slope_pull: 0.0,
        accel_drag_high: 0.0,
        drag_knee: 0.0,
        turbo_scale: 0.0,
        lat_grip: 0.0,
        turn_bleed: 0.0,
        strafe_thrust_share: 0.0,
        reverse_thrust_share: 0.0,
        coast_drop: 0.0,
        coast_drop_share: 0.0,
        roll_bank: 0.0,
        ..HoverParams::DEFAULT_HULL
    }
}

fn forward_speed(state: &HoverState) -> f32 {
    state.velocity[0] * math::sin(state.yaw) + state.velocity[2] * math::cos(state.yaw)
}

fn lateral_speed(state: &HoverState) -> f32 {
    state.velocity[0] * math::cos(state.yaw) - state.velocity[2] * math::sin(state.yaw)
}

/// Unit step response of an underdamped second-order spring (natural frequency `omega`, damping ratio
/// `zeta` < 1) at `t` seconds: the share of the way to the target. The expected values of the spring
/// tests are derived from it, so they follow the hull's own spring fields (ADR-8164).
fn second_order_step(omega: f32, zeta: f32, t: f32) -> f32 {
    let root = math::sqrt(1.0 - zeta * zeta);
    let wd = omega * root;
    1.0 - math::exp(-zeta * omega * t) * (math::cos(wd * t) + zeta / root * math::sin(wd * t))
}

/// When the second-order step response first reaches `share` (bisection, to a tenth of a millisecond).
fn second_order_time_to(omega: f32, zeta: f32, share: f32) -> f32 {
    let (mut lo, mut hi) = (
        0.0_f32,
        core::f32::consts::PI / (omega * math::sqrt(1.0 - zeta * zeta)),
    );
    while hi - lo > 1.0e-4 {
        let mid = 0.5 * (lo + hi);
        if second_order_step(omega, zeta, mid) < share {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

#[test]
fn default_params_are_the_engine_default_hull() {
    let p = HoverParams::default();
    assert_eq!(p, HoverParams::DEFAULT_HULL);
    assert_eq!(p.ride_height, 1.0);
    // The cushion: a 5 rad/s altitude spring (omega squared) at damping ratio 0.8 (2 zeta omega).
    assert_eq!(p.height_spring, 5.0 * 5.0);
    assert_eq!(p.height_damper, 2.0 * 0.8 * 5.0);
    // 30 m/s forward, a third of that in reverse, half of it sideways.
    assert_eq!(p.max_forward_speed, 30.0);
    assert_eq!(p.max_reverse_speed, p.max_forward_speed / 3.0);
    assert_eq!(p.max_strafe_speed, p.max_forward_speed / 2.0);
    // 1.5 g of thrust; braking stronger than thrust; drag smaller under thrust than released.
    assert_eq!(p.thrust_accel, 15.0);
    assert!(p.brake_accel > p.thrust_accel);
    assert!(p.thrust_drag < p.coast_drag);
    // Pivot at rest twice the turn rate at full speed; the yaw answers faster at speed.
    assert_eq!(p.yaw_rate_at_rest, 2.0 * p.yaw_rate_at_speed);
    assert!(p.yaw_response_at_speed > p.yaw_response);
    assert_eq!(p.jump_speed, 10.0);
    // Every optional path the engine's tests exercise is on in the default hull.
    for (name, v) in [
        ("thrust_taper", p.thrust_taper),
        ("yaw_deadband", p.yaw_deadband),
        ("coast_drop", p.coast_drop),
        ("coast_drop_share", p.coast_drop_share),
        ("lat_grip", p.lat_grip),
        ("turn_bleed", p.turn_bleed),
        ("slope_pull", p.slope_pull),
        ("hover_lead", p.hover_lead),
        ("roll_bank", p.roll_bank),
        ("roll_slip", p.roll_slip),
        ("strafe_thrust_share", p.strafe_thrust_share),
        ("reverse_thrust_share", p.reverse_thrust_share),
        ("cushion_terrain_share", p.cushion_terrain_share),
    ] {
        assert!(v > 0.0, "{name} is on in the default hull");
    }
    // The spring and drop fields the step divides by or squares are positive.
    for v in [
        p.coast_drop_tau,
        p.roll_spring_omega,
        p.roll_spring_zeta,
        p.cushion_pitch_omega,
        p.cushion_pitch_zeta,
        p.cushion_reach_band_m,
    ] {
        assert!(v > 0.0);
    }
}

#[test]
fn hover_state_default_is_idle() {
    let state = HoverState::default();
    assert_eq!(state.velocity, [0.0; 3]);
    assert!(!state.airborne);
    assert_eq!(state.altitude, 0.0);
}

#[test]
fn speed_cap_forward() {
    let params = HoverParams::default();
    let mut state = HoverState::default();
    for _ in 0..200 {
        step(&params, &mut state, &[1.0, 0.0, 0.0], 0.0, DT);
        assert!(
            forward_speed(&state) <= params.max_forward_speed + 0.001,
            "cap exceeded: {}",
            forward_speed(&state)
        );
    }
    let f = forward_speed(&state);
    assert!(
        (params.max_forward_speed - 0.5..=params.max_forward_speed + 0.001).contains(&f),
        "terminal forward speed {f} outside cap band"
    );
}

#[test]
fn speed_cap_reverse() {
    let params = HoverParams::default();
    let mut state = HoverState::default();
    for _ in 0..200 {
        step(&params, &mut state, &[-1.0, 0.0, 0.0], 0.0, DT);
        assert!(
            forward_speed(&state) >= -params.max_reverse_speed - 0.001,
            "reverse cap exceeded: {}",
            forward_speed(&state)
        );
    }
    let r = -forward_speed(&state);
    assert!(
        (params.max_reverse_speed - 0.5..=params.max_reverse_speed + 0.001).contains(&r),
        "terminal reverse speed {r} outside cap band"
    );
}

#[test]
fn strafe_speed_cap() {
    let params = HoverParams::default();
    let mut state = HoverState::default();
    for _ in 0..200 {
        step(&params, &mut state, &[0.0, 1.0, 0.0], 0.0, DT);
        assert!(
            lateral_speed(&state).abs() <= params.max_strafe_speed + 0.001,
            "strafe cap exceeded: {}",
            lateral_speed(&state)
        );
    }
    assert!(
        lateral_speed(&state).abs() >= params.max_strafe_speed - 0.5,
        "strafe never approached its cap: {}",
        lateral_speed(&state)
    );
}

#[test]
fn brake_stronger_than_thrust() {
    let params = HoverParams::default();
    let mut state = HoverState {
        velocity: [0.0, 0.0, 15.0], // 15 m/s along facing (yaw = 0)
        ..HoverState::default()
    };
    let v0 = 15.0_f32;
    let thrust_bound = (v0 / (params.thrust_accel * DT)).ceil() as usize; // 15 ticks
    let brake_bound = (v0 / (params.brake_accel * DT)).ceil() as usize; // 4 ticks

    let mut ticks_to_stop = None;
    for tick in 1..=thrust_bound + 5 {
        step(&params, &mut state, &[-1.0, 0.0, 0.0], 0.0, DT);
        if forward_speed(&state) <= 0.0 {
            ticks_to_stop = Some(tick);
            break;
        }
    }
    let ticks = ticks_to_stop.expect("never stopped under full brake");
    assert!(
        ticks <= brake_bound,
        "braking took {ticks} ticks, brake_accel bound is {brake_bound}"
    );
    assert!(
        ticks < thrust_bound,
        "braking ({ticks} ticks) not faster than thrust_accel bound ({thrust_bound})"
    );
}

#[test]
fn drag_decay_curve() {
    // Constant deceleration (coast drag m/s²): the coast bleeds off
    // LINEARLY — 10 m/s at 4 m/s² reaches zero at exactly 2.5 s
    // and stays there. This linear drift is the plain model's feel
    // (and what lets fast classes reach caps above thrust-accel/drag).
    // The linear drift of the plain drag model: no first-drop stage, no knee.
    let params = HoverParams {
        accel_drag_high: 0.0,
        coast_drag: 4.0,
        ..HoverParams::default()
    };
    let mut state = HoverState {
        velocity: [0.0, 0.0, 10.0],
        ..HoverState::default()
    };
    let ticks = ((2.5 / DT).ceil() as usize) + 10;
    for k in 1..=ticks {
        step(&params, &mut state, &[0.0, 0.0, 0.0], 0.0, DT);
        let expected = det_max(10.0 - params.coast_drag * DT * k as f32, 0.0);
        let actual = forward_speed(&state);
        assert!(
            (actual - expected).abs() < 1e-3,
            "tick {k}: expected {expected}, got {actual}"
        );
    }
    assert_eq!(forward_speed(&state), 0.0, "coast ends fully stopped");
}

#[test]
fn fast_class_reaches_cap_above_thrust_over_drag() {
    // A light hull: cap 25 with thrust 20 and thrust drag 1.0.
    // Proportional drag would terminal-limit at 20; constant drag must
    // let the light hull reach its real forward speed.
    let params = HoverParams {
        max_forward_speed: 25.0,
        thrust_accel: 20.0,
        thrust_drag: 1.0,
        ..HoverParams::default()
    };
    let mut state = HoverState::default();
    for _ in 0..400 {
        step(&params, &mut state, &[1.0, 0.0, 0.0], 0.0, DT);
    }
    let speed = forward_speed(&state);
    assert!(
        speed > 24.5 && speed <= 25.0 + 1e-3,
        "a light hull must reach its 25 m/s cap (got {speed})"
    );
}

#[test]
fn yaw_rate_lerp_clamped() {
    let params = HoverParams::default();
    // Diagonal over-speed: forward + lateral components both at cap, so
    // total speed exceeds max_forward_speed and an unclamped lerp would
    // shrink (or invert) the commanded rate below yaw_rate_at_speed.
    let mut state = HoverState {
        velocity: [20.0, 0.0, 20.0], // yaw = 0: fwd = 20 (z), lateral = 20 (x)
        ..HoverState::default()
    };
    step(&params, &mut state, &[0.0, 0.0, 1.0], 0.0, DT);
    // After one tick from yaw_rate = 0, yaw_rate = commanded * yaw_response * dt.
    let implied_commanded = state.yaw_rate / (params.yaw_response * DT);
    assert!(
        implied_commanded >= params.yaw_rate_at_speed - 1e-3,
        "commanded rate {implied_commanded} fell below yaw_rate_at_speed"
    );

    // Extreme over-speed (4x cap) must never invert the turn direction.
    let mut state = HoverState {
        velocity: [0.0, 0.0, 80.0],
        ..HoverState::default()
    };
    step(&params, &mut state, &[0.0, 0.0, 1.0], 0.0, DT);
    assert!(
        state.yaw_rate > 0.0,
        "turn inverted at high speed: {}",
        state.yaw_rate
    );
    let implied_commanded = state.yaw_rate / (params.yaw_response * DT);
    assert!(implied_commanded >= params.yaw_rate_at_speed - 1e-3);
}

#[test]
fn pd_altitude_convergence() {
    let params = HoverParams::default();
    let mut state = HoverState::default(); // altitude 0 (on ground)
    let mut max_altitude = 0.0_f32;
    for _ in 0..((3.0 / DT).ceil() as usize) {
        // 3 seconds
        step(&params, &mut state, &[0.0, 0.0, 0.0], 0.0, DT);
        max_altitude = det_max(max_altitude, state.altitude);
    }
    assert!(
        (state.altitude - params.ride_height).abs() <= 0.05 * params.ride_height,
        "altitude {} not within 5% of ride_height after 3 s",
        state.altitude
    );
    assert!(
        max_altitude < 1.5 * params.ride_height,
        "overshoot {max_altitude} exceeded 50%"
    );
}

#[test]
fn terrain_following_bob() {
    // Hover bob: as the ground rises under the moving craft, its
    // AGL height dips (the hull lags), then the spring restores
    // ride_height; the WORLD Y (terrain + altitude) moves smoothly,
    // not rigidly with the terrain.
    let params = HoverParams {
        hover_lead: 0.0,
        ..HoverParams::default()
    };
    let mut state = HoverState::default();
    // Settle on flat ground (seeds prev_terrain, altitude -> ~1.0).
    for _ in 0..80 {
        step(&params, &mut state, &[0.0, 0.0, 0.0], 0.0, DT);
    }
    assert!(
        (state.altitude - params.ride_height).abs() < 0.05,
        "settled at ride_height"
    );
    let settled_world_y = 0.0 + state.altitude;

    // Terrain ramps up 0.2/tick under the craft (steady climb).
    let mut terrain = 0.0f32;
    let mut min_agl = f32::MAX;
    for _ in 0..20 {
        terrain += 0.2;
        step(&params, &mut state, &[0.0, 0.0, 0.0], terrain, DT);
        min_agl = det_min(min_agl, state.altitude);
    }
    // The hull lagged: AGL dipped below ride_height while climbing.
    assert!(
        min_agl < params.ride_height - 0.02,
        "hull lags the rising ground (bob), min_agl={min_agl}"
    );
    // World Y climbed but by LESS than the terrain (it's catching up).
    let climbing_world_y = terrain + state.altitude;
    assert!(climbing_world_y > settled_world_y, "world Y rose");
    assert!(
        climbing_world_y < terrain + params.ride_height + 0.001,
        "world Y still catching up to the ground"
    );

    // Terrain goes flat again; the spring restores ride_height.
    for _ in 0..120 {
        step(&params, &mut state, &[0.0, 0.0, 0.0], terrain, DT);
    }
    assert!(
        (state.altitude - params.ride_height).abs() < 0.02,
        "settles back to ride_height on flat ground"
    );

    // Flat terrain at any height still settles to the same AGL (the
    // disturbance is only the terrain CHANGE, not its absolute value).
    let mut high = HoverState::default();
    for _ in 0..200 {
        step(&params, &mut high, &[0.0, 0.0, 0.0], 57.3, DT);
    }
    assert!((high.altitude - params.ride_height).abs() < 0.02);
}

/// Pins the moment-to-moment feel of the plain model on [`plain_test_hull`]: acceleration time, brake
/// distance, coast/drift distance, pivot rate at rest, and turn rate at speed. If any of these drift
/// the plain model has changed.
#[test]
fn plain_model_dynamics() {
    let p = plain_test_hull();

    // Accel from rest: net accel = thrust-accel - thrust-drag =
    // 25 - 1 = 24 m/s², so 23 m/s (96% of the 24 cap) in ~0.96 s.
    let mut st = HoverState::default();
    let mut ticks_to_23 = None;
    let accel_window = (1.2 / DT).ceil() as usize;
    for t in 1..=accel_window {
        step(&p, &mut st, &[1.0, 0.0, 0.0], 0.0, DT);
        if ticks_to_23.is_none() && forward_speed(&st) >= 23.0 {
            ticks_to_23 = Some(t);
        }
    }
    let ta = ticks_to_23.expect("reaches 23 m/s");
    let t_secs = ta as f32 * DT;
    assert!(
        (0.9..=1.3).contains(&t_secs),
        "accel to 23 m/s in ~1 s, got {t_secs:.3}s ({ta} ticks)"
    );
    assert!(
        forward_speed(&st) <= p.max_forward_speed + 1e-3,
        "caps at forward-speed"
    );

    // Brake distance from cruise: brake-accel = 60 m/s² → stops in
    // ~0.4 s over a short distance (~5 m), MUCH shorter than the
    // coast below.
    let mut st = HoverState::default();
    for _ in 0..(3.0 / DT) as usize {
        step(&p, &mut st, &[1.0, 0.0, 0.0], 0.0, DT);
    }
    let mut brake_dist = 0.0f32;
    let mut brake_ticks = 0;
    let brake_limit = (2.0 / DT) as usize;
    loop {
        step(&p, &mut st, &[-1.0, 0.0, 0.0], 0.0, DT);
        brake_dist += forward_speed(&st).abs() * DT;
        brake_ticks += 1;
        if forward_speed(&st) <= 0.05 || brake_ticks > brake_limit {
            break;
        }
    }
    assert!(
        brake_ticks as f32 * DT <= 0.6,
        "hard brake stops fast, {} ticks ({:.3}s)",
        brake_ticks,
        brake_ticks as f32 * DT
    );
    assert!(
        brake_dist < 6.0,
        "brake distance short (~5 m), got {brake_dist:.1}"
    );

    // Coast/drift on release: coast drag = 5 m/s² from 24 m/s → ~4.8 s,
    // ~58 m — the signature long drift, an order of magnitude past the
    // brake distance.
    let mut st = HoverState::default();
    for _ in 0..(3.0 / DT) as usize {
        step(&p, &mut st, &[1.0, 0.0, 0.0], 0.0, DT);
    }
    let mut coast_dist = 0.0f32;
    let mut coast_ticks = 0;
    let coast_limit = (10.0 / DT) as usize;
    loop {
        step(&p, &mut st, &[0.0, 0.0, 0.0], 0.0, DT);
        coast_dist += forward_speed(&st).abs() * DT;
        coast_ticks += 1;
        if forward_speed(&st) <= 0.05 || coast_ticks > coast_limit {
            break;
        }
    }
    let coast_secs = coast_ticks as f32 * DT;
    assert!(
        (4.5..=5.75).contains(&coast_secs),
        "coast ~5 s, got {coast_secs:.3}s ({coast_ticks} ticks)"
    );
    assert!(
        (40.0..60.0).contains(&coast_dist),
        "drift ~58 m, got {coast_dist:.1}"
    );

    // Pivot at rest: full steer settles to the rest yaw rate = 4 rad/s.
    let mut st = HoverState::default();
    for _ in 0..(2.0 / DT) as usize {
        step(&p, &mut st, &[0.0, 0.0, 1.0], 0.0, DT);
    }
    assert!(
        (st.yaw_rate - p.yaw_rate_at_rest).abs() < 0.05,
        "pivot rate = yaw_rate_at_rest, got {}",
        st.yaw_rate
    );

    // Turn at cruise: the rate cap lerps toward the at-speed rate = 2 rad/s
    // (wider turn under speed). Drive forward + full steer.
    let mut st = HoverState::default();
    for _ in 0..(6.0 / DT) as usize {
        step(&p, &mut st, &[1.0, 0.0, 1.0], 0.0, DT);
    }
    assert!(
        st.yaw_rate < p.yaw_rate_at_rest - 0.3 && st.yaw_rate > p.yaw_rate_at_speed - 0.3,
        "turn rate at speed near the at-speed rate, got {}",
        st.yaw_rate
    );
}

#[test]
fn determinism_bit_identical() {
    let params = HoverParams::default();
    let scripted_input = |tick: usize| -> [f32; 3] {
        let a = ((tick * 7) % 13) as f32 / 13.0 * 2.0 - 1.0;
        let b = ((tick * 5) % 11) as f32 / 11.0 * 2.0 - 1.0;
        let c = ((tick * 3) % 7) as f32 / 7.0 * 2.0 - 1.0;
        [a, b, c]
    };
    let run = || {
        let mut state = HoverState {
            velocity: [1.0, 0.0, -2.0],
            yaw: 0.4,
            ..HoverState::default()
        };
        for tick in 0..500 {
            if tick == 100 {
                apply_jump(&mut state, &params);
            }
            step(&params, &mut state, &scripted_input(tick), 12.5, DT);
        }
        state
    };
    let a = run();
    let b = run();
    for i in 0..3 {
        assert_eq!(a.velocity[i].to_bits(), b.velocity[i].to_bits());
    }
    assert_eq!(a.vertical_vel.to_bits(), b.vertical_vel.to_bits());
    assert_eq!(a.yaw.to_bits(), b.yaw.to_bits());
    assert_eq!(a.yaw_rate.to_bits(), b.yaw_rate.to_bits());
    assert_eq!(a.altitude.to_bits(), b.altitude.to_bits());
    assert_eq!(a.airborne, b.airborne);
}

#[test]
fn steer_reduced_when_airborne() {
    let params = HoverParams::default();
    let mut grounded = HoverState::default();
    let mut airborne = HoverState {
        airborne: true,
        altitude: 5.0, // well above ride_height + eps
        ..HoverState::default()
    };
    step(&params, &mut grounded, &[0.0, 0.0, 1.0], 0.0, DT);
    step(&params, &mut airborne, &[0.0, 0.0, 1.0], 0.0, DT);
    assert!(airborne.airborne, "should still be airborne");
    let ratio = airborne.yaw_rate / grounded.yaw_rate;
    assert!(
        (ratio - 0.4).abs() < 1e-5,
        "airborne steer ratio {ratio}, expected 0.4"
    );
}

#[test]
fn velocity_y_mirrors_vertical_vel() {
    let params = HoverParams::default();
    let mut state = HoverState::default();
    for _ in 0..10 {
        step(&params, &mut state, &[0.5, 0.0, 0.0], 0.0, DT);
        assert_eq!(state.velocity[1].to_bits(), state.vertical_vel.to_bits());
    }
}

#[test]
fn jump_applies_impulse_and_lands() {
    let params = HoverParams::default();
    let mut state = HoverState {
        altitude: 1.0,
        ..HoverState::default()
    };
    apply_jump(&mut state, &params);
    assert!(state.airborne);
    assert_eq!(state.vertical_vel, params.jump_speed);
    // Double jump while airborne is a no-op.
    apply_jump(&mut state, &params);
    assert_eq!(state.vertical_vel, params.jump_speed);

    let mut peak = state.altitude;
    let mut landed_tick = None;
    for tick in 1..=400 {
        step(&params, &mut state, &[0.0, 0.0, 0.0], 0.0, DT);
        peak = det_max(peak, state.altitude);
        if !state.airborne {
            landed_tick = Some(tick);
            break;
        }
    }
    assert!(landed_tick.is_some(), "never landed after jump");
    assert!(peak > params.ride_height + 1.0, "jump barely left hover");
    // After landing, the spring settles back to hover altitude.
    for _ in 0..((3.0 / DT).ceil() as usize) {
        step(&params, &mut state, &[0.0, 0.0, 0.0], 0.0, DT);
    }
    assert!(
        (state.altitude - params.ride_height).abs() <= 0.05 * params.ride_height,
        "did not resettle to hover altitude: {}",
        state.altitude
    );
}

#[test]
fn lean_pitch_uses_forward_component() {
    let params = HoverParams::default();
    // Same forward speed at two different headings must produce the
    // same pitch (a world-X-based bug would differ).
    let north = HoverState {
        velocity: [0.0, 0.0, 10.0], // yaw 0 faces +Z
        ..HoverState::default()
    };
    let east = HoverState {
        velocity: [10.0, 0.0, 0.0],
        yaw: core::f32::consts::FRAC_PI_2, // faces +X
        ..HoverState::default()
    };
    let input = [0.5, 0.0, 0.0];
    let (pitch_n, _) = compute_lean(&params, &north, &input);
    let (pitch_e, _) = compute_lean(&params, &east, &input);
    assert!(
        (pitch_n - pitch_e).abs() < 1e-5,
        "pitch differs by heading: {pitch_n} vs {pitch_e}"
    );
    // pitch = throttle * lean_pitch_per_throttle + (fwd/max_forward_speed) * lean_pitch_per_speed
    let expected = 0.5 * params.lean_pitch_per_throttle
        + (10.0 / params.max_forward_speed) * params.lean_pitch_per_speed;
    assert!((pitch_n - expected).abs() < 1e-5);

    // Roll: strafe input and yaw rate contributions.
    let turning = HoverState {
        yaw_rate: 2.0,
        ..HoverState::default()
    };
    let (_, roll) = compute_lean(&params, &turning, &[0.0, 1.0, 0.0]);
    let expected_roll = params.lean_roll_per_strafe + 2.0 * params.lean_roll_per_yaw_rate;
    assert!((roll - expected_roll).abs() < 1e-5);
}

/// Fly one hop from rest with the real step. Returns (apex rise m, max |lean| rad, ticks).
fn fly_hop(gravity: f32, air: f32, forward: f32) -> (f32, f32, usize) {
    let flight = fly_hop_with(&HoverLimits::DEFAULT, gravity, air, forward);
    (flight.apex_m, flight.lean_max, flight.ticks)
}

/// What [`fly_hop_with`] measured.
#[derive(Debug, Clone, Copy)]
pub(super) struct HopFlight {
    /// Apex rise over the launch altitude, metres.
    pub(super) apex_m: f32,
    /// Largest |pitch| or |roll| lean, radians.
    pub(super) lean_max: f32,
    /// Ticks until the hull was back on its cushion.
    pub(super) ticks: usize,
    /// Ticks after the press until the hop cooldown had run out (0 if it never ran out in flight).
    pub(super) cooldown_ticks: usize,
}

/// [`fly_hop`] under a game's own model limits (ADR-8349): the hop's launch speed, cooldown and
/// gravity floor come from `limits`, and the step is [`step_with`] (bit for bit [`step_world`] under
/// [`HoverLimits::DEFAULT`]).
pub(super) fn fly_hop_with(limits: &HoverLimits, gravity: f32, air: f32, forward: f32) -> HopFlight {
    let params = HoverParams::default();
    let world = WorldPhysics {
        gravity_m_s2: gravity,
        air_density_kg_m3: air,
        vacuum: false,
        low_g: crate::ballistic::LowG::Governor,
    };
    let mut state = HoverState {
        altitude: params.ride_height,
        ..HoverState::default()
    };
    let launch = hop_launch_speed_for(limits, &world, DT);
    assert!(apply_hull_hop_with(limits, &mut state, launch));
    let start = state.altitude;
    let (mut peak, mut lean_max, mut ticks, mut cooldown_ticks) = (0.0_f32, 0.0_f32, 0usize, 0usize);
    let input = [forward, 0.0, 0.0];
    for _ in 0..6000 {
        step_with(
            &params,
            limits,
            &ClimbTuning::AIR_TIME,
            &world,
            &mut state,
            &input,
            0.0,
            DT,
        );
        ticks += 1;
        if cooldown_ticks == 0 && state.hop_cooldown == 0.0 {
            cooldown_ticks = ticks;
        }
        peak = det_max(peak, state.altitude - start);
        // The hop adds no attitude of its own: only the throttle lean.
        let (pitch, roll) = compute_lean(&params, &state, &[0.0, 0.0, 0.0]);
        lean_max = det_max(det_max(lean_max, pitch.abs()), roll.abs());
        if !state.airborne && ticks > 10 {
            break;
        }
    }
    HopFlight {
        apex_m: peak,
        lean_max,
        ticks,
        cooldown_ticks,
    }
}

#[test]
fn hull_hop_apex_is_two_to_four_metres_on_every_world() {
    for (name, g, air) in [
        ("earth", 9.81_f32, 1.2_f32),
        ("mars", 3.71, 0.02),
        ("europa", 1.315, 0.0),
        ("titan", 1.35, 5.3),
    ] {
        let (apex, _, ticks) = fly_hop(g, air, 0.0);
        assert!(
            (2.0..=4.0).contains(&apex),
            "{name}: hop apex {apex} m outside 2-4 m"
        );
        assert!(ticks < 6000, "{name}: hop never settled");
    }
}

#[test]
fn feedback_jump_apex_and_hang_time_on_owner_worlds() {
    for (name, g, air) in [
        ("europa_thin_air", 1.315, 0.30),
        ("mars_thin_air", 3.71, 0.60),
        ("earth", 9.81, 0.99),
    ] {
        let (apex, _, ticks) = fly_hop(g, air, 0.0);
        let hang = ticks as f32 * DT;
        // The previous launch solver targeted exactly 3 m on every world.
        assert!(
            (apex / HoverLimits::DEFAULT.hop_apex_m - 1.0).abs() <= 0.02,
            "{name}: {apex}"
        );
        assert!(hang <= 2.1, "{name}: hull hang {hang}");
        eprintln!("jump feel {name}: hull apex={apex:.5} m hang={hang:.5} s");
    }
}

#[test]
fn jump_floor_leaves_earth_and_non_jump_ballistics_unchanged() {
    let params = HoverParams::default();
    for g in [1.315, 3.71, 9.81] {
        let mut falling = HoverState {
            airborne: true,
            altitude: 20.0,
            vertical_vel: -2.0,
            ..HoverState::default()
        };
        let expected = crate::ballistic::step_debris([0.0, -2.0, 0.0], g, 0.3, DT);
        step_world(&params, &mut falling, &[0.0; 3], 0.0, DT, g, 0.3);
        assert_eq!(falling.vertical_vel, expected[1], "free fall uses map g");
    }
    let mut hop = HoverState {
        altitude: 1.0,
        ..HoverState::default()
    };
    apply_hull_hop(&mut hop, hop_launch_speed(9.81, 0.99, DT));
    let mut old = hop;
    old.jump_active = false; // Old arc: unmodified map gravity.
    for _ in 0..150 {
        step_world(&params, &mut hop, &[0.0; 3], 0.0, DT, 9.81, 0.99);
        step_world(&params, &mut old, &[0.0; 3], 0.0, DT, 9.81, 0.99);
        assert_eq!(hop.altitude.to_bits(), old.altitude.to_bits());
        assert_eq!(hop.vertical_vel.to_bits(), old.vertical_vel.to_bits());
        assert_eq!(hop.airborne, old.airborne);
    }
    assert!(!hop.jump_active, "landing clears the jump arc");
}

#[test]
fn hull_hop_keeps_pitch_and_roll_level() {
    for (g, air) in [(9.81_f32, 1.2_f32), (1.315, 0.0)] {
        let (_, lean, _) = fly_hop(g, air, 0.0);
        // From rest the jump adds no pitch or roll at all (limit: 3 deg).
        assert!(lean.to_degrees() < 3.0, "lean {} deg", lean.to_degrees());
    }
}

#[test]
fn hull_hop_is_one_pulse_per_press_with_a_cooldown() {
    let mut state = HoverState::default();
    let launch = hop_launch_speed(1.315, 0.0, DT);
    assert!(apply_hull_hop(&mut state, launch));
    let v = state.vertical_vel;
    assert!(!apply_hull_hop(&mut state, launch), "no second pulse in the air");
    assert_eq!(state.vertical_vel, v);
    state.airborne = false;
    assert!(!apply_hull_hop(&mut state, launch), "cooldown refuses a re-press");
    state.hop_cooldown = 0.0;
    assert!(apply_hull_hop(&mut state, launch));
}

// --- Crest and wall rules: a hull hugs the ground, small hops only ---

/// Drive at full throttle along +Z (yaw 0) over `ground(z)`, the way a host steps a hull: z advances by the
/// forward velocity, the terrain delta feeds `step`. Rows are `(z, height over ground, forward speed)`.
fn drive_profile(
    params: &HoverParams,
    start_speed: f32,
    seconds: f32,
    ground: &dyn Fn(f32) -> f32,
) -> Vec<(f32, f32, f32)> {
    drive_profile_in(&ClimbTuning::CLAMPED_1014, params, start_speed, seconds, ground)
}

/// [`drive_profile`] under the given flight rules (the clamped tests keep the clamped rules they were written for).
fn drive_profile_in(
    climb: &ClimbTuning,
    params: &HoverParams,
    start_speed: f32,
    seconds: f32,
    ground: &dyn Fn(f32) -> f32,
) -> Vec<(f32, f32, f32)> {
    let mut state = HoverState {
        velocity: [0.0, 0.0, start_speed],
        ..HoverState::default()
    };
    let mut z = 0.0_f32;
    // Settle on the first ground height so the run starts at hover height.
    for _ in 0..90 {
        step_tuned(params, &mut state, &[0.0; 3], ground(z), DT, GRAVITY, 0.0, climb);
    }
    state.velocity = [0.0, state.vertical_vel, start_speed];
    let mut rows = Vec::new();
    for _ in 0..(seconds / DT) as usize {
        z += state.velocity[2] * DT;
        step_tuned(
            params,
            &mut state,
            &[1.0, 0.0, 0.0],
            ground(z),
            DT,
            GRAVITY,
            0.0,
            climb,
        );
        rows.push((z, state.altitude, state.velocity[2]));
    }
    rows
}

/// A ramp of `rise` metres over `run` metres starting at z = 20, then flat plain.
fn ramp_then_plain(rise: f32, run: f32) -> impl Fn(f32) -> f32 {
    move |z: f32| det_clamp((z - 20.0) / run, 0.0, 1.0) * rise
}

fn peak_over_ground(rows: &[(f32, f32, f32)]) -> f32 {
    rows.iter().map(|r| r.1).fold(0.0, det_max)
}

#[test]
fn leaving_a_steep_ramp_at_speed_does_not_loft_the_hull() {
    let params = HoverParams::default();
    // A tunnel exit: 30 degrees up (rise 20 m over 35 m), then the plain, at cruise and at full speed.
    for start in [20.0_f32, 40.0] {
        let rows = drive_profile(&params, start, 6.0, &ramp_then_plain(20.0, 35.0));
        let peak = peak_over_ground(&rows);
        assert!(
            peak <= params.ride_height + HOVER_RIDE_MAX_M + 0.35,
            "start {start} m/s: hull reached {peak} m over the ground"
        );
    }
}

#[test]
fn a_falling_crest_never_throws_the_hull_up() {
    let params = HoverParams::default();
    // A long climb then a drop-off: the ground falls 12 m over 10 m at full speed.
    let ground = |z: f32| {
        let up = det_clamp((z - 20.0) / 120.0, 0.0, 1.0) * 18.0;
        let down = det_clamp((z - 140.0) / 10.0, 0.0, 1.0) * 12.0;
        up - down
    };
    let rows = drive_profile(&params, 40.0, 8.0, &ground);
    let after: Vec<_> = rows.iter().filter(|r| r.0 > 140.0).collect();
    // Over the drop the hull falls at least as fast as the ground; it never rises over the plain below.
    let mut prev = f32::MAX;
    for r in &after {
        assert!(
            r.1 <= params.ride_height + HOVER_RIDE_MAX_M + 12.5,
            "hull {} m over the ground",
            r.1
        );
        prev = det_min(prev, r.1);
    }
    assert!(
        prev <= params.ride_height + 0.3,
        "the hull settled onto the lower ground"
    );
}

#[test]
fn a_steep_grade_is_still_climbable_at_speed() {
    // The ride ceiling must not take away the climb: a 25 degree slope at cruise is still driven to the top. Faces past the
    // hover step are the host's wall rule, tested on its own maps.
    let params = HoverParams::default();
    let slope = |z: f32| det_clamp((z - 20.0) * 0.47, 0.0, 25.0);
    let rows = drive_profile(&params, 20.0, 4.0, &slope);
    let top = rows.iter().map(|r| slope(r.0)).fold(0.0, det_max);
    assert!(
        top > 20.0,
        "a 25 degree grade is still climbable, reached {top} m"
    );
}

#[test]
fn flat_ground_feel_is_unchanged_by_the_crest_rules() {
    // On flat ground nothing above engages: the hull settles, holds its altitude and reaches its cap as before.
    let params = HoverParams::default();
    for climb in [ClimbTuning::CLAMPED_1014, ClimbTuning::AIR_TIME] {
        let rows = drive_profile_in(&climb, &params, 0.0, 6.0, &|_| 0.0);
        for r in &rows {
            assert!(
                (r.1 - params.ride_height).abs() < 0.01,
                "altitude drifted to {}",
                r.1
            );
        }
        let last = rows.last().expect("rows");
        assert!(last.2 > params.max_forward_speed - 0.5);
    }
}

// --- Air-time flight (the default flight mode, "air_time") ---

/// Seconds spent more than 1 m above hover height once the hull is past `from_z`.
fn air_time_after(rows: &[(f32, f32, f32)], params: &HoverParams, from_z: f32) -> f32 {
    rows.iter()
        .filter(|r| r.0 > from_z && r.1 > params.ride_height + 1.0)
        .count() as f32
        * DT
}

/// DO27 read the flight mode from its handling file; in Regia it is a match setting with the same
/// names, air time by default, and a typo is refused.
#[test]
fn the_flight_mode_flies_air_time_by_default_and_parses_the_switch() {
    assert_eq!(FlightMode::default(), FlightMode::AirTime);
    assert_eq!(ClimbTuning::default().flight, FlightRules::AIR_TIME);
    assert_eq!(FlightMode::default().climb().flight, FlightRules::AIR_TIME);
    let clamped = FlightMode::from_name("clamped").expect("clamped parses");
    assert_eq!(clamped.climb(), &ClimbTuning::CLAMPED_1014);
    assert_eq!(FlightMode::from_name("air_time"), Some(FlightMode::AirTime));
    assert!(FlightMode::from_name("floaty").is_none());
    for mode in [FlightMode::AirTime, FlightMode::Clamped] {
        assert_eq!(FlightMode::from_name(mode.name()), Some(mode));
    }
}

#[test]
fn air_time_flight_leaves_a_ramp_top_with_air_time_and_comes_back_down() {
    // A tunnel exit profile (30 degrees, 20 m) at cruise and full speed. Air time keeps the upward speed over the top
    // and falls at about 6 m/s2: seconds of air, a bounded arc, and the cushion catches it on the plain.
    let params = HoverParams::default();
    for start in [20.0_f32, 40.0] {
        let rows = drive_profile_in(
            &ClimbTuning::AIR_TIME,
            &params,
            start,
            12.0,
            &ramp_then_plain(20.0, 35.0),
        );
        let air = air_time_after(&rows, &params, 55.0);
        let peak = peak_over_ground(&rows);
        assert!(
            air >= 1.5,
            "start {start} m/s: only {air:.2} s of air off the ramp top"
        );
        assert!(
            peak <= 30.0,
            "start {start} m/s: the arc reached {peak} m over the ground"
        );
        let end = rows.last().expect("rows").1;
        assert!(
            (end - params.ride_height).abs() < 0.3,
            "start {start} m/s: still {end} m up after 12 s"
        );
    }
    // The clamped rules gave about 1 s and 3 m: "no air time".
    let rows = drive_profile_in(
        &ClimbTuning::CLAMPED_1014,
        &params,
        40.0,
        12.0,
        &ramp_then_plain(20.0, 35.0),
    );
    assert!(peak_over_ground(&rows) <= params.ride_height + HOVER_RIDE_MAX_M + 0.35);
}

#[test]
fn air_time_flight_falls_no_faster_than_the_fall_floor_and_lands_softly() {
    // A 12 m drop-off at full speed: free of the cushion the hull's downward acceleration never passes the fall floor, and
    // the damper catches it so it lands slower than a free fall from that height would.
    let params = HoverParams::default();
    let ground = |z: f32| if z < 20.0 { 0.0 } else { -12.0 };
    let mut state = HoverState {
        altitude: params.ride_height,
        prev_terrain: Some(0.0),
        ..HoverState::default()
    };
    state.velocity = [0.0, 0.0, 40.0];
    let (mut z, mut worst, mut land) = (0.0_f32, 0.0_f32, 0.0_f32);
    let mut was_high = false;
    for _ in 0..600 {
        let vy0 = state.vertical_vel;
        step_tuned(
            &params,
            &mut state,
            &[1.0, 0.0, 0.0],
            ground(z),
            DT,
            3.71,
            0.6,
            &ClimbTuning::AIR_TIME,
        );
        z += state.velocity[2] * DT;
        if state.altitude > params.ride_height + 1.0 {
            worst = det_min(worst, (state.vertical_vel - vy0) / DT);
            was_high = true;
        } else if was_high {
            land = det_min(land, state.vertical_vel);
        }
    }
    assert!(
        worst >= -(HoverLimits::DEFAULT.crest_fall_min_m_s2 + 0.01),
        "fell at {worst} m/s2"
    );
    assert!(
        land > -8.0,
        "landed at {land} m/s (free fall from 12 m at 6 m/s2 is -12)"
    );
    assert!(
        (state.altitude - params.ride_height).abs() < 0.1,
        "settled at {}",
        state.altitude
    );
}

#[test]
fn air_time_flight_floats_off_a_downhill_and_rejoins_it() {
    // 35 degrees down for 60 m at full speed, then flat: the hull flies off the slope (it does not hug it) and comes back.
    let params = HoverParams::default();
    let ground = |z: f32| -(det_clamp(z - 20.0, 0.0, 60.0)) * 0.7;
    let rows = drive_profile_in(&ClimbTuning::AIR_TIME, &params, 40.0, 8.0, &ground);
    let air = air_time_after(&rows, &params, 20.0);
    let peak = peak_over_ground(&rows);
    assert!(
        air >= 1.0,
        "only {air:.2} s off a 35 degree downhill at full speed"
    );
    // The slope falls 42 m in 1.5 s; the hull falls behind it but never rises: at most the slope's depth over the ground.
    assert!(peak <= 42.0 + params.ride_height + 1.0, "{peak} m over the slope");
    assert!((rows.last().expect("rows").1 - params.ride_height).abs() < 0.3);
}

#[test]
fn air_time_flight_keeps_the_hop_and_the_rest_height() {
    // The hop (3 m apex) and the hull at rest are the same under both rules.
    for climb in [ClimbTuning::CLAMPED_1014, ClimbTuning::AIR_TIME] {
        let params = HoverParams::default();
        let mut state = HoverState {
            altitude: params.ride_height,
            prev_terrain: Some(0.0),
            ..HoverState::default()
        };
        for _ in 0..120 {
            step_tuned(&params, &mut state, &[0.0; 3], 0.0, DT, 3.71, 0.6, &climb);
        }
        assert!((state.altitude - params.ride_height).abs() < 1.0e-3);
        assert!(apply_hull_hop(&mut state, hop_launch_speed(3.71, 0.6, DT)));
        let mut apex = 0.0_f32;
        for _ in 0..600 {
            step_tuned(&params, &mut state, &[0.0; 3], 0.0, DT, 3.71, 0.6, &climb);
            apex = det_max(apex, state.altitude - params.ride_height);
        }
        assert!((2.9..=3.1).contains(&apex), "hop apex {apex}");
    }
}

#[test]
fn a_hull_hugs_a_long_downhill_instead_of_flying_off_it() {
    let params = HoverParams::default();
    // 35 degrees down for 60 m at full speed (an approach dip), then flat.
    let ground = |z: f32| -(det_clamp(z - 20.0, 0.0, 60.0)) * 0.7;
    let rows = drive_profile(&params, 40.0, 4.0, &ground);
    let peak = peak_over_ground(&rows);
    assert!(
        peak <= params.ride_height + HOVER_RIDE_MAX_M + 1.0,
        "hull stood {peak} m over a downhill"
    );
}

// ---- Auto-level and the hull's real roll ----

/// A hull high in the air (a long fall ahead), tilted, with the given auto-level setting.
fn tilted_in_the_air(level_off: bool, roll: f32) -> (HoverParams, HoverState) {
    let params = HoverParams::default();
    let state = HoverState {
        // About 4 s of fall before the cushion catches it.
        altitude: params.ride_height + 80.0,
        airborne: true,
        roll,
        level_off,
        ..HoverState::default()
    };
    (params, state)
}

#[test]
fn auto_level_off_gives_no_righting_torque_in_the_air() {
    let (params, mut st) = tilted_in_the_air(true, 0.6);
    for _ in 0..(1.5 / DT) as usize {
        step(&params, &mut st, &[0.0; 3], 0.0, DT);
        assert!(st.airborne, "still falling");
    }
    assert!(
        (st.roll - 0.6).abs() < 1.0e-5,
        "nothing rights a free hull: roll {}",
        st.roll
    );
    assert!(
        st.roll_rate.abs() < 1.0e-5,
        "and nothing spins it: {}",
        st.roll_rate
    );
}

/// Auto-level ON levels the hull through its roll spring (`roll_spring_omega`, `roll_spring_zeta`): within 5% of level
/// inside the spring's 5% envelope time `ln 20 / (zeta omega)`, without flipping past level by more than 5%, and at 0.65 s
/// as far down as the spring's step response says.
#[test]
fn auto_level_on_levels_the_hull_in_the_spring_time() {
    let (params, mut st) = tilted_in_the_air(false, 0.6);
    let mut settled_at = None;
    let mut overshoot = 0.0_f32;
    for i in 0..(2.5 / DT) as usize {
        step(&params, &mut st, &[0.0; 3], 0.0, DT);
        overshoot = det_max(overshoot, -st.roll);
        if settled_at.is_none() && st.roll.abs() <= 0.03 {
            settled_at = Some((i + 1) as f32 * DT);
        }
    }
    let t = settled_at.expect("levels");
    let envelope = 2.995_732_3 / (params.roll_spring_zeta * params.roll_spring_omega); // ln 20
    assert!(t <= envelope, "levelled in {t} s (envelope {envelope} s)");
    assert!(overshoot <= 0.03, "overshot level by {overshoot}");
    // At 0.65 s the hull has come the spring's step-response share of the way.
    let (params, mut st) = tilted_in_the_air(false, 0.6);
    for _ in 0..(0.65 / DT) as usize {
        step(&params, &mut st, &[0.0; 3], 0.0, DT);
    }
    let want = 0.6 * (1.0 - second_order_step(params.roll_spring_omega, params.roll_spring_zeta, 0.65));
    assert!(
        (st.roll - want).abs() < 0.05,
        "0.65 s: roll {} (the spring says {want})",
        st.roll
    );
}

#[test]
fn auto_level_off_lets_the_strafe_keys_roll_a_hull_all_the_way_round_in_the_air() {
    let (params, mut st) = tilted_in_the_air(true, 0.0);
    let mut turned = 0.0_f32;
    let mut prev = st.roll;
    for _ in 0..(3.0 / DT) as usize {
        step(&params, &mut st, &[0.0, 1.0, 0.0], 0.0, DT);
        assert!(hull_in_air(&params, &st), "still in the air");
        turned += wrap_pi(st.roll - prev);
        prev = st.roll;
    }
    assert!(
        turned > core::f32::consts::TAU,
        "a full roll in 3 s, turned {turned}"
    );
    assert!(
        st.roll > -core::f32::consts::PI && st.roll <= core::f32::consts::PI,
        "wrapped"
    );
    // ON in the air: the same keys only lean it.
    let (params, mut st) = tilted_in_the_air(false, 0.0);
    for _ in 0..(3.0 / DT) as usize {
        step(&params, &mut st, &[0.0, 1.0, 0.0], 0.0, DT);
    }
    assert!(
        (st.roll - params.lean_roll_per_strafe).abs() < 0.01,
        "ON holds the strafe lean: {}",
        st.roll
    );
}

/// On the cushion the hover fans hold the hull at its lean whatever the setting: `lean_roll_per_strafe` into a full
/// strafe (right side down for a right strafe), at 0.65 s the roll spring's step-response share of it.
#[test]
fn strafe_rolls_the_hull_into_the_strafe_on_the_cushion_either_way() {
    for level_off in [false, true] {
        let params = HoverParams::default();
        let mut st = HoverState {
            altitude: params.ride_height,
            level_off,
            ..HoverState::default()
        };
        let mut at_065 = 0.0;
        for i in 0..(3.0 / DT) as usize {
            step(&params, &mut st, &[0.0, 1.0, 0.0], 0.0, DT);
            if i + 1 == (0.65 / DT) as usize {
                at_065 = st.roll;
            }
        }
        assert!(!hull_in_air(&params, &st));
        let want = params.lean_roll_per_strafe;
        assert!(
            (st.roll - want).abs() < 0.1 * want,
            "level_off {level_off}: strafe roll {} deg",
            st.roll.to_degrees()
        );
        let share = second_order_step(params.roll_spring_omega, params.roll_spring_zeta, 0.65);
        assert!(
            ((share - 0.085) * want..(share + 0.085) * want).contains(&at_065),
            "level_off {level_off}: {} at 0.65 s (the spring says {})",
            at_065,
            share * want
        );
    }
}

/// A held turn at speed banks the hull into the turn (right turn, right side down) by `roll_bank` per m/s2 of
/// centripetal acceleration; turning at rest banks nothing.
#[test]
fn a_turn_banks_into_the_turn_and_a_spin_at_rest_does_not() {
    let params = HoverParams::default();
    let mut st = HoverState {
        altitude: params.ride_height,
        velocity: [0.0, 0.0, 20.0],
        thrusting: true,
        ..HoverState::default()
    };
    let mut bank = 0.0_f32;
    let mut want = 0.0_f32;
    for _ in 0..(3.0 / DT) as usize {
        step(&params, &mut st, &[1.0, 0.0, 1.0], 0.0, DT);
        let fwd = forward_speed(&st);
        want = params.roll_bank * st.yaw_rate * fwd;
        bank = st.roll;
    }
    assert!(bank > 0.02, "banks into a right turn: {bank}");
    assert!(
        (bank - det_min(want, HoverLimits::DEFAULT.roll_lean_max)).abs() < 0.25 * want.abs() + 0.01,
        "bank {bank} vs {want}"
    );
    let mut st = HoverState {
        altitude: params.ride_height,
        ..HoverState::default()
    };
    for _ in 0..(3.0 / DT) as usize {
        step(&params, &mut st, &[0.0, 0.0, 1.0], 0.0, DT);
    }
    assert!(st.roll.abs() < 0.01, "a spin at rest banks nothing: {}", st.roll);
}

/// Auto-level OFF in the air, nose up: part of the thrust pushes the hull up (a stretched jump), never more than
/// `AIR_LIFT_MAX_G` of gravity, so it still comes down. ON, or nose level, the thrust is all horizontal.
#[test]
fn auto_level_off_thrust_follows_the_hull_in_the_air_but_never_flies() {
    let fall = |level_off: bool, pitch: f32| {
        let params = HoverParams::default();
        let mut st = HoverState {
            altitude: params.ride_height + 20.0,
            airborne: true,
            level_off,
            pitch,
            ..HoverState::default()
        };
        let mut t = 0.0_f32;
        while st.airborne && t < 30.0 {
            step(&params, &mut st, &[1.0, 0.0, 0.0], 0.0, DT);
            t += DT;
        }
        (t, forward_speed(&st))
    };
    let (t_on, _) = fall(false, 1.0);
    let (t_level, _) = fall(true, 0.0);
    let (t_up, v_up) = fall(true, 1.0);
    let (t_down, _) = fall(true, -1.0);
    assert!(
        (t_on - t_level).abs() < 2.0 * DT,
        "ON ignores the pitch: {t_on} vs {t_level}"
    );
    assert!(
        t_up > t_level + 0.2,
        "nose up stretches the fall: {t_up} vs {t_level}"
    );
    assert!(t_up < 30.0, "but it lands");
    assert!(t_down < t_level, "nose down dives: {t_down} vs {t_level}");
    assert!(v_up > 0.0, "and still drives forward");
}

#[test]
fn auto_level_and_roll_are_deterministic() {
    let run = || {
        let params = HoverParams::default();
        let mut st = HoverState {
            altitude: params.ride_height,
            ..HoverState::default()
        };
        let mut h = 0u32;
        for i in 0..900u32 {
            st.level_off = (i / 120) % 2 == 1;
            st.pitch = 0.9 * math::sin(i as f32 * 0.01);
            if i % 200 == 10 {
                apply_hull_hop(&mut st, 7.7);
            }
            let input = [
                1.0,
                if (i / 45) % 2 == 0 { 1.0 } else { -0.5 },
                math::sin(i as f32 * 0.03),
            ];
            step(&params, &mut st, &input, 0.0, DT);
            for v in [
                st.roll,
                st.roll_rate,
                st.velocity[0],
                st.velocity[2],
                st.vertical_vel,
                st.yaw,
                st.coast_drop_left,
            ] {
                h = h.rotate_left(7) ^ v.to_bits();
            }
        }
        h
    };
    assert_eq!(run(), run());
}

/// The release drop: a hull sheds `coast_drop + coast_drop_share * speed` in the first few `coast_drop_tau` after
/// letting go, then coasts at `coast_drag`. The expected speeds are the drop's own discrete decay (a share `dt / tau`
/// of what is left each tick) plus the linear coast. Re-pressing the throttle cancels what is left of the drop.
#[test]
fn letting_go_drops_speed_quickly_then_coasts() {
    let params = HoverParams::default();
    let v0 = params.max_forward_speed;
    let mut st = HoverState {
        altitude: params.ride_height,
        velocity: [0.0, 0.0, v0],
        thrusting: true,
        ..HoverState::default()
    };
    let mut at = Vec::new();
    for i in 0..(2.1 / DT) as usize {
        step(&params, &mut st, &[0.0; 3], 0.0, DT);
        at.push((((i + 1) as f32) * DT, forward_speed(&st)));
    }
    let speed_at = |t: f32| {
        at.iter()
            .find(|(s, _)| *s >= t - 1.0e-4)
            .map(|(_, v)| *v)
            .expect("sample at t")
    };
    let drop = params.coast_drop + params.coast_drop_share * v0;
    let keep = 1.0 - DT / params.coast_drop_tau;
    for t in [0.25_f32, 0.5, 1.0, 2.0] {
        let ticks = (t / DT).round();
        let want = v0 - params.coast_drag * DT * ticks - drop * (1.0 - math::powf(keep, ticks));
        let got = speed_at(t);
        assert!(
            (got - want).abs() < 0.02 * want,
            "{t} s: {got} m/s, the drop and coast say {want}"
        );
    }
    // The drop is quick: most of it is gone within 0.4 s, far more than the coast alone takes.
    assert!(speed_at(0.4) < v0 - 0.8 * drop, "{} m/s at 0.4 s", speed_at(0.4));
    let mut st2 = HoverState {
        altitude: params.ride_height,
        velocity: [0.0, 0.0, v0],
        thrusting: true,
        ..HoverState::default()
    };
    step(&params, &mut st2, &[0.0; 3], 0.0, DT);
    assert!(st2.coast_drop_left > 0.0);
    step(&params, &mut st2, &[1.0, 0.0, 0.0], 0.0, DT);
    assert_eq!(st2.coast_drop_left, 0.0, "throttle again cancels the drop");
}

// ---------------------------------------------------------------------------------------------------------------------
// The cushion ("stuck at the same height, the movement isnt natural at all"). The expected values follow the hull's own
// spring and lookahead fields (ADR-8164), on a synthetic hill.

/// Rise of the synthetic hill, metres.
const HILL_RISE_M: f32 = 18.0;
/// Seconds the synthetic hill takes to climb (and to descend) at the hull's ground speed.
const HILL_HALF_S: f32 = 5.0;
/// Seconds of flat ground before the synthetic hill.
const HILL_START_S: f32 = 2.0;

/// Ground height under a hull crossing a synthetic hill, metres over the start, at `t` seconds: flat, then a cosine
/// climb of [`HILL_RISE_M`] over [`HILL_HALF_S`], then the same descent. The steepest climb rate is
/// `pi / 2 * rise / half`.
fn crest_ground_at(t: f32) -> f32 {
    let u = (t - HILL_START_S) / HILL_HALF_S;
    if u <= 0.0 || u >= 2.0 {
        0.0
    } else {
        0.5 * HILL_RISE_M * (1.0 - math::cos(core::f32::consts::PI * u))
    }
}

/// The cushion catches a small hop softly: released 0.405 m over its ride height, the hull falls back on the spring
/// with the compression damper (`height_damper * cushion_compress_damp_share`) and dips under its ride height by the
/// spring's first undershoot, `0.405 * exp(-zeta omega pi / omega_d)` at `pi / omega_d` seconds, then settles. With the
/// full damper all the way down it would dip far less and look locked to its ride height.
#[test]
fn the_cushion_dips_on_landing_by_its_spring_undershoot() {
    let params = HoverParams::default();
    let mut st = HoverState {
        altitude: params.ride_height + 0.405,
        prev_terrain: Some(0.0),
        ..HoverState::default()
    };
    let (mut low, mut low_t) = (f32::MAX, 0.0);
    for i in 0..(2.0 / DT) as usize {
        step(&params, &mut st, &[0.0; 3], 0.0, DT);
        if st.altitude < low {
            low = st.altitude;
            low_t = (i + 1) as f32 * DT;
        }
    }
    let omega = math::sqrt(params.height_spring);
    let undershoot = |damper: f32| {
        let zeta = damper / (2.0 * omega);
        let wd = omega * math::sqrt(1.0 - zeta * zeta);
        (
            0.405 * math::exp(-zeta * omega * core::f32::consts::PI / wd),
            core::f32::consts::PI / wd,
        )
    };
    let (dip, dip_t) = undershoot(params.height_damper * params.cushion_compress_damp_share);
    let want = params.ride_height - dip;
    assert!(
        (low - want).abs() < 0.015,
        "dips to {low:.3} m, the spring says {want:.3}"
    );
    assert!(
        (low_t - dip_t).abs() < 0.15,
        "lowest at {low_t:.2} s, the spring says {dip_t:.2}"
    );
    let (stiff_dip, _) = undershoot(params.height_damper);
    assert!(
        dip > 2.0 * stiff_dip,
        "the compression damper dips the hull ({dip:.3} m), the full damper would not ({stiff_dip:.3} m)"
    );
    assert!(
        (st.altitude - params.ride_height).abs() < 0.01,
        "back at ride height after 2 s: {:.3}",
        st.altitude
    );
}

/// Drop from 3 m over the ride height onto the cushion (a hull put down on the spring): it falls, the cushion takes the
/// fall, dips under the ride height and settles, never touching the ground.
#[test]
fn a_drop_from_3_m_is_caught_by_the_cushion_with_one_dip() {
    let params = HoverParams::default();
    let mut st = HoverState {
        altitude: 4.0,
        prev_terrain: Some(0.0),
        ..HoverState::default()
    };
    let (mut low, mut settle) = (f32::MAX, 0.0);
    for i in 0..(4.0 / DT) as usize {
        step(&params, &mut st, &[0.0; 3], 0.0, DT);
        low = det_min(low, st.altitude);
        if (st.altitude - 1.0).abs() > 0.05 {
            settle = (i + 1) as f32 * DT;
        }
    }
    assert!(
        low > 0.6 && low < 0.9,
        "lowest {low:.2} m: the cushion compresses but never grounds"
    );
    assert!(settle < 2.0, "within 5 cm after {settle:.2} s");
}

/// A hill crossed at speed, fed to the hull as the ground under it ([`crest_ground_at`]): the lookahead aims
/// `hover_lead * climb rate` over the ride height, so the hull rides high up the climb (at least half that aim at the
/// steepest climb, never more than the aim plus a metre), floats off the crest and sags under its ride height as the
/// ground falls away, then follows the descent without touching the ground.
#[test]
fn the_crest_run_rides_high_floats_off_and_sags() {
    let params = HoverParams::default();
    let mut st = HoverState {
        altitude: params.ride_height,
        ..HoverState::default()
    };
    let crest_t = HILL_START_S + HILL_HALF_S;
    let (mut peak, mut sag) = (0.0_f32, f32::MAX);
    for i in 0..((HILL_START_S + 2.0 * HILL_HALF_S + 1.0) / DT) as usize {
        let t = i as f32 * DT;
        step(&params, &mut st, &[0.0; 3], crest_ground_at(t), DT);
        if t < crest_t {
            peak = det_max(peak, st.altitude);
        } else {
            sag = det_min(sag, st.altitude);
        }
    }
    let steepest = 0.5 * core::f32::consts::PI * HILL_RISE_M / HILL_HALF_S;
    let aim = params.hover_lead * steepest;
    assert!(
        peak > params.ride_height + 0.5 * aim && peak < params.ride_height + aim + 1.0,
        "rides {peak:.2} m high up the climb (the lookahead aims {aim:.2} m over the ride height)"
    );
    assert!(
        sag < params.ride_height - 0.05 && sag > 0.3 * params.ride_height,
        "sags to {sag:.2} m past the crest, never to the ground"
    );
}

/// A ramp taken at speed launches the hull: the ground climbs 3 m in 1 s (a 3 m/s rise, a short ramp at 20 m/s) and drops
/// away. The hull keeps its upward speed off the end (air time), comes down and lands on the cushion softly (slower than
/// 8 m/s, about a 3 m free fall at 1 g, and not a snap back), dips under its ride height and settles, without touching
/// the ground.
#[test]
fn a_ramp_at_speed_launches_the_hull_and_it_lands() {
    let params = HoverParams::default();
    let mut st = HoverState {
        altitude: 1.0,
        ..HoverState::default()
    };
    let ground = |t: f32| {
        if t < 1.0 {
            0.0
        } else if t < 2.0 {
            3.0 * (t - 1.0)
        } else {
            0.0
        }
    };
    let (mut top, mut low, mut landing) = (0.0_f32, f32::MAX, None);
    let mut prev = st.altitude;
    for i in 0..(6.0 / DT) as usize {
        let t = i as f32 * DT;
        step(&params, &mut st, &[0.0; 3], ground(t), DT);
        if t > 2.0 {
            top = det_max(top, st.altitude);
            low = det_min(low, st.altitude);
            if landing.is_none() && prev >= 1.0 && st.altitude < 1.0 {
                landing = Some((t, st.vertical_vel));
            }
        }
        prev = st.altitude;
    }
    assert!(
        top > 3.0 + params.ride_height,
        "in the air off the ramp: {top:.2} m over the ground"
    );
    let (landed, vy) = landing.expect("lands within 4 s");
    assert!(
        landed > 2.8,
        "a flight, not a snap back: back on the cushion at {landed:.2} s"
    );
    // A soft landing: under 8 m/s, and a real fall (over 1.5 m/s).
    assert!((-8.0..=-1.5).contains(&vy), "lands at {vy:.2} m/s");
    assert!(
        low > 0.3 && low < 0.95,
        "the cushion dips and catches it before the ground: lowest {low:.2} m"
    );
    assert!((st.altitude - 1.0).abs() < 0.05, "settled at {:.2}", st.altitude);
}

/// A hull resting on flat ground holds its ride height dead still (no bob over 3 s).
#[test]
fn a_hull_at_rest_holds_its_ride_height() {
    let params = HoverParams::default();
    let mut st = HoverState {
        altitude: 1.0,
        prev_terrain: Some(0.0),
        ..HoverState::default()
    };
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for _ in 0..(3.0 / DT) as usize {
        step(&params, &mut st, &[0.0; 3], 0.0, DT);
        lo = det_min(lo, st.altitude);
        hi = det_max(hi, st.altitude);
    }
    assert!(hi - lo < 1.0e-3, "bob {:.4} m", hi - lo);
}

/// On a slope the hull pitches and rolls toward `cushion_terrain_share` of the ground's tilt, reaching 63% of it when
/// the cushion's pitch spring (`cushion_pitch_omega`, `cushion_pitch_zeta`) says, without overshooting much.
#[test]
fn the_hull_follows_a_share_of_the_ground_slope_with_a_lag() {
    let params = HoverParams::default();
    let slope = 0.235_f32; // 13.5 degrees
    let mut st = HoverState {
        altitude: 1.0,
        prev_terrain: Some(0.0),
        ..HoverState::default()
    };
    let (mut t63, mut peak) = (None, 0.0_f32);
    for i in 0..(3.0 / DT) as usize {
        st.ground_pitch = slope;
        st.ground_roll = slope;
        step(&params, &mut st, &[0.0; 3], 0.0, DT);
        peak = det_max(peak, st.cushion_pitch);
        if t63.is_none() && st.cushion_pitch >= 0.632 * params.cushion_terrain_share * slope {
            t63 = Some((i + 1) as f32 * DT);
        }
    }
    let want = params.cushion_terrain_share * slope;
    assert!(
        (st.cushion_pitch - want).abs() < 0.03 * want,
        "pitch {:.4} rad, want {want:.4}",
        st.cushion_pitch
    );
    assert!(
        (st.cushion_pitch.to_degrees() - want.to_degrees()).abs() < 0.3,
        "{:.2} degrees on 13.5",
        st.cushion_pitch.to_degrees()
    );
    let t63 = t63.expect("rises");
    let spring_t63 = second_order_time_to(params.cushion_pitch_omega, params.cushion_pitch_zeta, 0.632);
    assert!(
        (t63 - spring_t63).abs() < 0.15,
        "63% at {t63:.2} s, the spring says {spring_t63:.2}"
    );
    assert!(
        peak < 1.05 * want,
        "overshoot {:.1}%",
        (peak / want - 1.0) * 100.0
    );
    // Ground rising on the right lifts the right side: a negative roll (right side down positive).
    assert!(
        (st.roll + want).abs() < 0.03 * want,
        "roll {:.4} rad, want {:.4}",
        st.roll,
        -want
    );
}

/// Riding high on the lookahead is not being in the air: a hull held just inside its cushion reach (0.98 of
/// `cushion_reach_full_m` over its ride height) still pitches its full share of the slope. Far out of the fans' reach
/// (12 m over, past `cushion_reach_full_m + cushion_reach_band_m`) the ground no longer pitches the hull.
#[test]
fn a_hull_riding_high_up_a_climb_still_follows_the_slope() {
    let params = HoverParams::default();
    let slope = 0.235_f32;
    assert!(params.cushion_reach_full_m + params.cushion_reach_band_m < 12.0);
    for (over, share) in [(0.98 * params.cushion_reach_full_m, 1.0_f32), (12.0, 0.0)] {
        let mut st = HoverState {
            altitude: params.ride_height + over,
            prev_terrain: Some(0.0),
            ..HoverState::default()
        };
        for _ in 0..(1.5 / DT) as usize {
            st.altitude = params.ride_height + over; // held there, as the lookahead holds it on the climb
            st.vertical_vel = 0.0;
            st.ground_pitch = slope;
            step(&params, &mut st, &[0.0; 3], 0.0, DT);
        }
        let want = share * params.cushion_terrain_share * slope;
        assert!(
            (st.cushion_pitch - want).abs() < 0.05 * params.cushion_terrain_share * slope,
            "{over} m over: {} want {want}",
            st.cushion_pitch
        );
    }
}

/// In the air the ground does not hold the hull: auto-level ON brings the cushion pitch back to level, auto-level OFF
/// leaves it where it left the ground (no righting torque).
#[test]
fn in_the_air_auto_level_on_levels_the_cushion_pitch_and_off_leaves_it() {
    let params = HoverParams::default();
    for level_off in [false, true] {
        let mut st = HoverState {
            altitude: 1.0,
            prev_terrain: Some(0.0),
            ..HoverState::default()
        };
        for _ in 0..(2.0 / DT) as usize {
            st.ground_pitch = 0.4;
            step(&params, &mut st, &[0.0; 3], 0.0, DT);
        }
        let left_with = st.cushion_pitch;
        assert!(left_with > 0.1);
        st.level_off = level_off;
        apply_hull_hop(&mut st, 12.0);
        for _ in 0..(0.9 / DT) as usize {
            st.ground_pitch = 0.4;
            step(&params, &mut st, &[0.0; 3], 0.0, DT);
            assert!(st.airborne);
        }
        if level_off {
            assert!(
                (st.cushion_pitch - left_with).abs() < 0.01,
                "OFF: {:.3} kept from {left_with:.3}",
                st.cushion_pitch
            );
        } else {
            assert!(
                st.cushion_pitch.abs() < 0.1 * left_with,
                "ON: {:.3} levelled from {left_with:.3}",
                st.cushion_pitch
            );
        }
    }
}

#[test]
fn the_cushion_attitude_is_deterministic() {
    let run = || {
        let params = HoverParams::default();
        let mut st = HoverState {
            altitude: params.ride_height,
            ..HoverState::default()
        };
        let mut h = 0u32;
        for i in 0..900u32 {
            let t = i as f32 * DT;
            st.ground_pitch = 0.3 * math::sin(t * 0.7);
            st.ground_roll = 0.2 * math::cos(t * 1.1);
            st.level_off = (i / 150) % 2 == 1;
            if i % 240 == 30 {
                apply_hull_hop(&mut st, 7.7);
            }
            step(
                &params,
                &mut st,
                &[1.0, math::sin(t), 0.3],
                crest_ground_at(det_min(t, 13.0)),
                DT,
            );
            for v in [
                st.cushion_pitch,
                st.cushion_pitch_rate,
                st.roll,
                st.altitude,
                st.vertical_vel,
            ] {
                h = h.rotate_left(7) ^ v.to_bits();
            }
        }
        h
    };
    assert_eq!(run(), run());
}

/// The modern aim preset: the modern flag speeds the yaw tracking up, classic is the old curve, and the same inputs (flag
/// included) replay to the same bits.
#[test]
fn modern_yaw_tracks_fast_classic_is_unchanged_and_both_replay_exactly() {
    let params = HoverParams::DEFAULT_HULL;
    let run = |modern: bool| {
        let mut st = HoverState {
            altitude: params.ride_height,
            yaw_modern: modern,
            ..HoverState::default()
        };
        let mut rates = Vec::new();
        for i in 0..240u32 {
            let steer = if (i / 80) % 2 == 0 { 1.0 } else { 0.0 };
            step(&params, &mut st, &[1.0, 0.0, steer], 0.0, DT);
            rates.push(st.yaw_rate);
        }
        rates
    };
    let (classic, modern) = (run(false), run(true));
    assert_eq!(classic, run(false));
    assert_eq!(modern, run(true));
    // 90% of the rate cap within 0.2 s for modern; classic is still ramping then.
    let cap = modern[70];
    assert!(modern[11] > 0.9 * cap, "modern 12 ticks: {} of {cap}", modern[11]);
    assert!(
        classic[11] < 0.5 * cap,
        "classic 12 ticks: {} of {cap}",
        classic[11]
    );
    // Modern lets go fast as well: half a second after the key comes up it has stopped.
    assert!(modern[80 + 12].abs() < 0.1 * cap, "{}", modern[80 + 12]);
}

/// DO27's math-module call-site test (ir-sim/src/math.rs lines 55-75): the hover tick path runs
/// through `crate::math`.
#[test]
fn hover_tick_path_uses_math_module() {
    // Compile-time use of the math module from a hover-shaped call site:
    // calling math::* here mirrors what step does with the yaw basis.
    let yaw = 0.4_f32;
    let (s, c) = (math::sin(yaw), math::cos(yaw));
    let speed = math::sqrt(s * s + c * c);
    assert!(speed > 0.0);

    let params = HoverParams::default();
    let mut state = HoverState {
        yaw,
        ..HoverState::default()
    };
    step(&params, &mut state, &[1.0, 0.0, 0.0], 0.0, 1.0 / 60.0);
    let _ = (math::powf(2.0, 2.0), math::exp(0.0), math::atan2(1.0, 1.0));
}

/// A -0.0 axis input (a host's axis that reads "negative zero") steps to the same bits as +0.0. The steer's zero reaches a
/// `signum` in the deadband mapping (a DO27 DET_FLOAT site): `det_clamp` and the canonical zero at the sink make it +0, so
/// the commanded yaw rate is never -0. (Before them the -0 still cancelled in the yaw-rate update, so this is a guard, not
/// a fail-first test.)
#[test]
fn a_negative_zero_input_steps_like_positive_zero() {
    let params = HoverParams::default();
    assert!(
        params.yaw_deadband > 0.0,
        "the deadband mapping is the path with the signum"
    );
    for (neg, pos) in [
        ([1.0_f32, 0.0, -0.0], [1.0_f32, 0.0, 0.0]),
        ([0.0, -0.0, -0.0], [0.0, 0.0, 0.0]),
        ([-0.0, 1.0, -0.0], [0.0, 1.0, 0.0]),
    ] {
        let mut a = HoverState {
            altitude: params.ride_height,
            ..HoverState::default()
        };
        let mut b = a;
        for tick in 0..120 {
            step(&params, &mut a, &neg, 0.0, DT);
            step(&params, &mut b, &pos, 0.0, DT);
            let words = |s: &HoverState| {
                [
                    s.velocity[0].to_bits(),
                    s.velocity[1].to_bits(),
                    s.velocity[2].to_bits(),
                    s.vertical_vel.to_bits(),
                    s.yaw.to_bits(),
                    s.yaw_rate.to_bits(),
                    s.altitude.to_bits(),
                    s.roll.to_bits(),
                    s.roll_rate.to_bits(),
                    s.cushion_pitch.to_bits(),
                    s.coast_drop_left.to_bits(),
                ]
            };
            assert_eq!(words(&a), words(&b), "input {neg:?} vs {pos:?}, tick {tick}");
        }
    }
}

// ---- Bad input (lane sprint-buildout-mp-hover-robust-inputs, audit gap 4.1 rows 1-2) ----

/// Every state word, hashed in a fixed order with the golden [`crate::golden::StateHasher`].
fn state_hash(s: &HoverState) -> u64 {
    let mut h = crate::golden::StateHasher::new();
    h.f32("vel_x", s.velocity[0]);
    h.f32("vel_y", s.velocity[1]);
    h.f32("vel_z", s.velocity[2]);
    h.f32("vertical_vel", s.vertical_vel);
    h.f32("yaw", s.yaw);
    h.f32("yaw_rate", s.yaw_rate);
    h.bool("airborne", s.airborne);
    h.bool("jump_active", s.jump_active);
    h.f32("altitude", s.altitude);
    h.bool("prev_terrain_set", s.prev_terrain.is_some());
    h.f32("prev_terrain", s.prev_terrain.unwrap_or(0.0));
    h.f32("hop_cooldown", s.hop_cooldown);
    h.f32("ground_climb", s.ground_climb);
    h.f32("slope", s.slope);
    h.f32("roll", s.roll);
    h.f32("roll_rate", s.roll_rate);
    h.f32("pitch", s.pitch);
    h.bool("level_off", s.level_off);
    h.bool("yaw_modern", s.yaw_modern);
    h.f32("coast_drop_left", s.coast_drop_left);
    h.bool("thrusting", s.thrusting);
    h.f32("ground_pitch", s.ground_pitch);
    h.f32("ground_roll", s.ground_roll);
    h.f32("cushion_pitch", s.cushion_pitch);
    h.f32("cushion_pitch_rate", s.cushion_pitch_rate);
    h.finish()
}

/// Every f32 word of the state, for finiteness checks.
fn state_words(s: &HoverState) -> [f32; 19] {
    [
        s.velocity[0],
        s.velocity[1],
        s.velocity[2],
        s.vertical_vel,
        s.yaw,
        s.yaw_rate,
        s.altitude,
        s.prev_terrain.unwrap_or(0.0),
        s.hop_cooldown,
        s.ground_climb,
        s.slope,
        s.roll,
        s.roll_rate,
        s.pitch,
        s.coast_drop_left,
        s.ground_pitch,
        s.ground_roll,
        s.cushion_pitch,
        s.cushion_pitch_rate,
    ]
}

fn all_finite(s: &HoverState) -> bool {
    state_words(s).iter().all(|w| w.is_finite())
}

/// A hull mid-manoeuvre: moving, turning, rolled, a cooldown running, a seeded floor.
fn moving_state(params: &HoverParams) -> HoverState {
    let mut s = HoverState {
        altitude: params.ride_height,
        ..HoverState::default()
    };
    for tick in 0..90 {
        let floor = 0.02 * tick as f32;
        step(params, &mut s, &[1.0, 0.3, 0.6], floor, DT);
    }
    s.hop_cooldown = 0.5;
    s
}

#[test]
fn a_zero_or_nan_dt_leaves_the_state_unchanged() {
    let params = HoverParams::default();
    let before = moving_state(&params);
    assert!(all_finite(&before) && before.prev_terrain.is_some());
    for dt in [
        0.0_f32,
        -0.0,
        -1.0 / 60.0,
        f32::NAN,
        -f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ] {
        for climb in [&ClimbTuning::AIR_TIME, &ClimbTuning::CLAMPED_1014] {
            let mut s = before;
            step(&params, &mut s, &[1.0, -0.5, 0.25], 1.5, dt);
            assert_eq!(state_hash(&s), state_hash(&before), "step, dt {dt}");
            step_world(&params, &mut s, &[1.0, -0.5, 0.25], 1.5, dt, 1.6, 0.0);
            step_tuned(&params, &mut s, &[1.0, -0.5, 0.25], 1.5, dt, GRAVITY, 0.0, climb);
            assert_eq!(state_hash(&s), state_hash(&before), "step_tuned, dt {dt}");
            let report = try_step_tuned(&params, &mut s, &[1.0, -0.5, 0.25], 1.5, dt, GRAVITY, 0.0, climb);
            assert!(!report.recovered);
            assert_eq!(state_hash(&s), state_hash(&before), "try_step_tuned, dt {dt}");
        }
    }
}

#[test]
fn a_nan_input_or_floor_never_reaches_the_state() {
    let params = HoverParams::default();
    let poisons = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -f32::NAN];
    // Lane 0..3: the three input axes; lane 3: the floor.
    for lane in 0..4 {
        for climb in [&ClimbTuning::AIR_TIME, &ClimbTuning::CLAMPED_1014] {
            let mut rng = crate::golden::scenarios::SplitMix64::new(0x0BAD_1000 + lane as u64);
            let mut poisoned = HoverState::default();
            let mut reference = HoverState::default();
            let mut prev_floor = 0.0_f32;
            let mut floor = 0.0_f32;
            for tick in 0..1000_u32 {
                let input = [rng.signed(), rng.signed(), rng.signed()];
                // A rolling floor with a step now and then (crests, dips, ledges).
                floor += 0.05 * rng.signed();
                if tick % 97 == 13 {
                    floor += 2.0;
                }
                let (mut bad_input, mut good_input) = (input, input);
                let (mut bad_floor, mut good_floor) = (floor, floor);
                if tick % 7 == 6 {
                    let poison = poisons[(tick / 7) as usize % poisons.len()];
                    if lane < 3 {
                        bad_input[lane] = poison;
                        good_input[lane] = 0.0;
                    } else {
                        bad_floor = poison;
                        good_floor = prev_floor;
                    }
                }
                if tick % 150 == 40 {
                    apply_jump(&mut poisoned, &params);
                    apply_jump(&mut reference, &params);
                }
                let report = try_step_tuned(
                    &params,
                    &mut poisoned,
                    &bad_input,
                    bad_floor,
                    DT,
                    GRAVITY,
                    0.0,
                    climb,
                );
                step_tuned(
                    &params,
                    &mut reference,
                    &good_input,
                    good_floor,
                    DT,
                    GRAVITY,
                    0.0,
                    climb,
                );
                assert!(!report.recovered, "lane {lane}, tick {tick}");
                assert!(all_finite(&poisoned), "lane {lane}, tick {tick}: {poisoned:?}");
                assert_eq!(
                    state_hash(&poisoned),
                    state_hash(&reference),
                    "lane {lane}, tick {tick}"
                );
                prev_floor = good_floor;
            }
        }
    }
}

#[test]
fn a_poisoned_state_recovers_to_rest_at_the_floor() {
    let params = HoverParams::default();
    let base = moving_state(&params);
    // Poison each f32 word in turn (NaN, then infinity).
    let setters: [fn(&mut HoverState, f32); 19] = [
        |s, v| s.velocity[0] = v,
        |s, v| s.velocity[1] = v,
        |s, v| s.velocity[2] = v,
        |s, v| s.vertical_vel = v,
        |s, v| s.yaw = v,
        |s, v| s.yaw_rate = v,
        |s, v| s.altitude = v,
        |s, v| s.prev_terrain = Some(v),
        |s, v| s.hop_cooldown = v,
        |s, v| s.ground_climb = v,
        |s, v| s.slope = v,
        |s, v| s.roll = v,
        |s, v| s.roll_rate = v,
        |s, v| s.pitch = v,
        |s, v| s.coast_drop_left = v,
        |s, v| s.ground_pitch = v,
        |s, v| s.ground_roll = v,
        |s, v| s.cushion_pitch = v,
        |s, v| s.cushion_pitch_rate = v,
    ];
    for (word, set) in setters.iter().enumerate() {
        for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for floor in [2.5_f32, f32::NAN] {
                let mut s = base;
                s.level_off = true;
                s.yaw_modern = true;
                set(&mut s, poison);
                let report = try_step_tuned(
                    &params,
                    &mut s,
                    &[1.0, 0.0, 0.0],
                    floor,
                    DT,
                    GRAVITY,
                    0.0,
                    &ClimbTuning::AIR_TIME,
                );
                assert!(report.recovered, "word {word}, {poison}, floor {floor}");
                // A NaN floor reads as the previous floor; a poisoned previous floor reads as 0.
                let rest_floor = if floor.is_finite() {
                    floor
                } else if word == 7 {
                    0.0
                } else {
                    base.prev_terrain.unwrap_or(0.0)
                };
                let rest = HoverState {
                    altitude: params.ride_height,
                    prev_terrain: Some(rest_floor),
                    level_off: true,
                    yaw_modern: true,
                    ..HoverState::default()
                };
                assert_eq!(
                    state_hash(&s),
                    state_hash(&rest),
                    "word {word}, {poison}, floor {floor}"
                );
                // The next tick steps normally from rest.
                let next = try_step_tuned(
                    &params,
                    &mut s,
                    &[1.0, 0.0, 0.0],
                    2.5,
                    DT,
                    GRAVITY,
                    0.0,
                    &ClimbTuning::AIR_TIME,
                );
                assert!(!next.recovered && all_finite(&s));
            }
        }
    }
}

#[test]
fn validate_names_the_bad_field() {
    let good = HoverParams::DEFAULT_HULL;
    assert_eq!(good.validate(), Ok(()));
    assert_eq!(
        good.sanitized(),
        good,
        "a valid hull sanitizes to itself, bit for bit"
    );
    assert!(!HoverParams::FIELD_RANGES.is_empty());
    for field in HoverParams::FIELD_RANGES {
        let mut cases = vec![
            (f32::NAN, HoverParamsErrorKind::NotFinite),
            (f32::INFINITY, HoverParamsErrorKind::NotFinite),
            (f32::NEG_INFINITY, HoverParamsErrorKind::NotFinite),
            (field.max * 2.0 + 1.0, HoverParamsErrorKind::OutOfRange),
        ];
        if field.min >= 0.0 {
            cases.push((-1.0, HoverParamsErrorKind::Negative));
        } else {
            cases.push((field.min * 2.0 - 1.0, HoverParamsErrorKind::OutOfRange));
        }
        if field.rate {
            cases.push((0.0, HoverParamsErrorKind::ZeroRate));
        }
        for (value, kind) in cases {
            let mut p = good;
            (field.set)(&mut p, value);
            assert_eq!(
                (field.get)(&p).to_bits(),
                value.to_bits(),
                "{}: get/set pair",
                field.name
            );
            let err = p.validate().expect_err(field.name);
            assert_eq!(
                (err.field, err.kind),
                (field.name, kind),
                "{} = {value}",
                field.name
            );
            assert!(err.to_string().contains(field.name) && err.to_string().contains(kind.code()));
            let fixed = p.sanitized();
            assert_eq!(fixed.validate(), Ok(()), "{} = {value} sanitized", field.name);
            // Only the bad field moved (and the deadband with the steer range it must fit in).
            let pair = |name: &str| field.name == "yaw_rate_at_rest" && name == "yaw_deadband";
            for other in HoverParams::FIELD_RANGES
                .iter()
                .filter(|o| o.name != field.name && !pair(o.name))
            {
                assert_eq!(
                    (other.get)(&fixed).to_bits(),
                    (other.get)(&good).to_bits(),
                    "{}",
                    other.name
                );
            }
        }
    }
    // The one pair rule: a deadband wider than the steer range leaves full lock at speed with no turn.
    let p = HoverParams {
        yaw_deadband: good.yaw_rate_at_rest + 1.0,
        ..good
    };
    let err = p.validate().expect_err("inverted");
    assert_eq!((err.field, err.kind.code()), ("yaw_deadband", "inverted_range"));
    assert_eq!(p.sanitized().yaw_deadband, good.yaw_rate_at_rest);
    assert_eq!(p.sanitized().validate(), Ok(()));
    // Climb tuning and flight rules: the same rules over their own field lists.
    for climb in [ClimbTuning::AIR_TIME, ClimbTuning::CLAMPED_1014] {
        assert_eq!(climb.validate(), Ok(()));
        assert_eq!(climb.sanitized(), climb);
        assert_eq!(climb.flight.validate(), Ok(()));
        for field in ClimbTuning::FIELD_RANGES {
            for (value, kind) in [
                (f32::NAN, HoverParamsErrorKind::NotFinite),
                (-1.0, HoverParamsErrorKind::Negative),
                (field.max * 2.0 + 1.0, HoverParamsErrorKind::OutOfRange),
            ] {
                let mut c = climb;
                (field.set)(&mut c, value);
                let err = c.validate().expect_err(field.name);
                assert_eq!(
                    (err.field, err.kind),
                    (field.name, kind),
                    "{} = {value}",
                    field.name
                );
                assert_eq!(
                    c.sanitized().validate(),
                    Ok(()),
                    "{} = {value} sanitized",
                    field.name
                );
            }
        }
        for field in FlightRules::FIELD_RANGES {
            let mut f = climb.flight;
            (field.set)(&mut f, f32::NAN);
            let err = f.validate().expect_err(field.name);
            assert_eq!((err.field, err.kind.code()), (field.name, "not_finite"));
            assert_eq!(f.sanitized().validate(), Ok(()));
        }
    }
    // Every error code is lowercase ascii, digits and `_`.
    for kind in HoverParamsErrorKind::ALL {
        assert!(
            kind.code()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        );
    }
}

/// A random f32 for a hull field: mostly in a wide range, sometimes NaN, infinite, negative, zero or huge.
fn wild_value(rng: &mut crate::golden::scenarios::SplitMix64) -> f32 {
    match rng.next_u64() % 10 {
        0 => f32::NAN,
        1 => {
            if rng.next_u64().is_multiple_of(2) {
                f32::INFINITY
            } else {
                f32::NEG_INFINITY
            }
        }
        2 => 0.0,
        3 => -rng.unit() * 1.0e6,
        4 => rng.unit() * 1.0e30,
        5 => f32::from_bits(rng.next_u64() as u32),
        _ => rng.unit() * 50.0,
    }
}

#[test]
fn sanitized_hull_steps_finite_for_random_params() {
    let mut rng = crate::golden::scenarios::SplitMix64::new(0x5A7E_0001);
    for hull in 0..10_000_u32 {
        let mut raw = HoverParams::DEFAULT_HULL;
        for field in HoverParams::FIELD_RANGES {
            if !rng.next_u64().is_multiple_of(3) {
                (field.set)(&mut raw, wild_value(&mut rng));
            }
        }
        let params = raw.sanitized();
        assert_eq!(params.validate(), Ok(()), "hull {hull}: {raw:?}");
        let mut climb = if hull.is_multiple_of(2) {
            ClimbTuning::AIR_TIME
        } else {
            ClimbTuning::CLAMPED_1014
        };
        for field in ClimbTuning::FIELD_RANGES {
            if rng.next_u64().is_multiple_of(4) {
                (field.set)(&mut climb, wild_value(&mut rng));
            }
        }
        let climb = climb.sanitized();
        assert_eq!(climb.validate(), Ok(()), "hull {hull}");
        // The two tick rates the engine runs (60 Hz and the 30 Hz floor of the documented domain).
        let dt = if hull.is_multiple_of(3) { 1.0 / 30.0 } else { DT };
        let gravity = if hull.is_multiple_of(5) { 1.6 } else { GRAVITY };
        let mut s = HoverState::default();
        let mut floor = 0.0_f32;
        for tick in 0..120_u32 {
            let mut input = [rng.signed(), rng.signed(), rng.signed()];
            if rng.next_u64().is_multiple_of(16) {
                input[(rng.next_u64() % 3) as usize] = wild_value(&mut rng);
            }
            floor += match rng.next_u64() % 8 {
                0 => 50.0 * rng.signed(),
                1 => 1.0e6 * rng.signed(),
                _ => 0.3 * rng.signed(),
            };
            let floor_in = if rng.next_u64().is_multiple_of(32) {
                f32::NAN
            } else {
                floor
            };
            s.level_off = tick % 40 < 10;
            s.yaw_modern = hull.is_multiple_of(7);
            s.pitch = 0.5 * rng.signed();
            s.ground_pitch = 0.3 * rng.signed();
            s.ground_roll = 0.3 * rng.signed();
            if rng.next_u64().is_multiple_of(40) {
                apply_jump(&mut s, &params);
            }
            if rng.next_u64().is_multiple_of(40) {
                apply_hull_hop(&mut s, hop_launch_speed(gravity, 0.0, dt));
            }
            let report = try_step_tuned(&params, &mut s, &input, floor_in, dt, gravity, 0.0, &climb);
            assert!(
                !report.recovered && all_finite(&s),
                "hull {hull}, tick {tick}: {params:?} {climb:?} -> {s:?}"
            );
            // The lean is presentation: it reads the axes as given, so a host passes the clamped ones.
            let axes = input.map(|x| {
                if x.is_finite() {
                    det_clamp(x, -1.0, 1.0)
                } else {
                    0.0
                }
            });
            let (lean_pitch, lean_roll) = compute_lean(&params, &s, &axes);
            assert!(
                lean_pitch.is_finite() && lean_roll.is_finite(),
                "hull {hull}, tick {tick}"
            );
        }
    }
}

/// An unsanitized hull with absurd spring numbers used to hang `wrap_pi` (an infinite or 1e30 roll
/// never leaves its `while` loop). The step now terminates and the next tick recovers.
#[test]
fn an_absurd_roll_spring_terminates_and_recovers() {
    for omega in [1.0e20_f32, f32::MAX, 1.0e10] {
        let params = HoverParams {
            roll_spring_omega: omega,
            ..HoverParams::DEFAULT_HULL
        };
        assert!(params.validate().is_err());
        let mut s = HoverState {
            altitude: params.ride_height,
            ..HoverState::default()
        };
        let mut recovered = 0;
        for _ in 0..240 {
            if try_step_tuned(
                &params,
                &mut s,
                &[1.0, 1.0, 1.0],
                0.0,
                DT,
                GRAVITY,
                0.0,
                &ClimbTuning::AIR_TIME,
            )
            .recovered
            {
                recovered += 1;
            }
        }
        assert!(
            recovered > 0,
            "omega {omega}: the absurd spring did poison the state"
        );
        let mut s = HoverState::default();
        let params = params.sanitized();
        for _ in 0..240 {
            let r = try_step_tuned(
                &params,
                &mut s,
                &[1.0, 1.0, 1.0],
                0.0,
                DT,
                GRAVITY,
                0.0,
                &ClimbTuning::AIR_TIME,
            );
            assert!(!r.recovered && all_finite(&s));
        }
    }
}

/// The lockstep settings handshake compares a hull as raw little-endian `f32` bits in
/// [`HoverParams::FIELD_RANGES`] order (`HoverParams::to_le_bytes`), never as text. `-0.0` keeps
/// its sign bit, and two hulls one ulp apart do not compare equal.
#[test]
fn hull_bytes_are_raw_bits_in_field_order() {
    let mut hull = HoverParams::DEFAULT_HULL;
    hull.ride_height = -0.0;
    let bytes = hull.to_le_bytes();
    assert_eq!(bytes.len(), HoverParams::BYTES);
    assert_eq!(HoverParams::BYTES, HoverParams::FIELD_RANGES.len() * 4);
    for (i, field) in HoverParams::FIELD_RANGES.iter().enumerate() {
        let at = i * 4;
        let word = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        assert_eq!(word, (field.get)(&hull).to_bits(), "{}", field.name);
    }
    assert_eq!(
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        (-0.0_f32).to_bits()
    );
    assert_ne!((-0.0_f32).to_bits(), 0.0_f32.to_bits());

    let mut other = hull;
    let bits = other.max_forward_speed.to_bits();
    other.max_forward_speed = f32::from_bits(bits + 1);
    let a = hull.to_le_bytes();
    let b = other.to_le_bytes();
    assert_ne!(a, b);
    let (words_a, rest_a) = a.as_chunks::<4>();
    let (words_b, rest_b) = b.as_chunks::<4>();
    assert!(rest_a.is_empty() && rest_b.is_empty());
    let diffs = words_a.iter().zip(words_b).filter(|(x, y)| x != y).count();
    assert_eq!(diffs, 1);
}
