// SPDX-License-Identifier: MPL-2.0
//! Gait follow-ups of the v0.7 plan item L-05 (ADR-10130): the swing bound on stepped rubble,
//! driven bodies below `idle_speed`, and a body yaw that changes on only some steps.
//!
//! The rubble ground is the agent's synthetic `rubble` (`anim.gait_simulate`, ADR-8708):
//! square cells of constant height, a hash of the seed and the cell.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_animation::gait::{
    GaitBody, GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, leg_array, step_gait,
    step_gait_driven,
};

const DT: f32 = 1.0 / 60.0;

/// The six-leg walker `anim.gait_simulate` steps by default.
fn walker() -> GaitParams {
    GaitParams {
        forward_speed: 2.4,
        reverse_speed: 1.1,
        strafe_speed: 0.8,
        acceleration: 3.0,
        step_length: 0.6,
        clearance: 0.7,
        swing_lift: 0.18,
        max_slope: 0.9,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        hip_offsets: leg_array(
            [
                [-0.4, 0.5],
                [0.4, 0.5],
                [-0.5, 0.0],
                [0.5, 0.0],
                [-0.4, -0.5],
                [0.4, -0.5],
            ],
            [0.0; 2],
        ),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.15,
        turn_step_angle: 0.35,
        ground_band: 2.0,
        foothold_candidates: 16,
        foothold_spacing: 0.25,
        max_reach: [1.2; MAX_LEGS],
        hang_drop: 0.3,
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
    }
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The agent's `rubble` ground.
#[derive(Clone, Copy)]
struct Rubble {
    amplitude: f32,
    cell: f32,
    seed: u64,
}

impl Rubble {
    fn height(&self, x: f32, z: f32) -> f32 {
        let ix = (x / self.cell).floor() as i64 as u64;
        let iz = (z / self.cell).floor() as i64 as u64;
        let bits = mix(self.seed ^ mix(ix ^ mix(iz)));
        let unit = (bits >> 40) as f32 / (1u64 << 24) as f32;
        self.amplitude * unit
    }

    fn ground(self) -> impl Fn(f32, f32) -> f32 + Copy {
        move |x, z| self.height(x, z)
    }
}

fn flat(_x: f32, _z: f32) -> f32 {
    0.0
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2) + (b[2] - a[2]).powi(2)).sqrt()
}

/// `anim.gait_simulate`'s `swing_bound` for the tripod walker at `max_speed`.
fn swing_bound(params: &GaitParams, max_speed: f32, dt: f32) -> f32 {
    let duty = params.patterns.patterns[0].duty_factor.max(1.0e-3);
    let rings = params.foothold_candidates.div_ceil(8) as f32;
    let longest = params.step_length / duty + 2.0 * params.step_length + rings * params.foothold_spacing;
    let speed = max_speed.max(1.0e-3);
    let walking = (1.0 - duty) / duty * params.step_length / speed;
    let shortest = params.settle_period.min(walking).max(1.0e-4);
    let radius = params.hip_offsets[..params.legs()]
        .iter()
        .map(|[r, f]| (r * r + f * f).sqrt())
        .fold(0.0_f32, f32::max)
        + params.step_length;
    dt * ((longest + 4.0 * params.swing_lift) / shortest + max_speed + params.max_yaw_rate * radius)
}

/// One step's worst foot move: (distance, foot, swinging before, swinging after, before, after).
type Jump = (f32, usize, bool, bool, [f32; 3], [f32; 3]);

fn worst_move(before: &GaitState, after: &GaitState) -> Jump {
    let mut worst: Jump = (0.0, 0, false, false, [0.0; 3], [0.0; 3]);
    for (i, (a, b)) in before.feet().iter().zip(after.feet()).enumerate() {
        let d = distance(a.position, b.position);
        if d > worst.0 {
            worst = (
                d,
                i,
                before.is_swinging(i),
                after.is_swinging(i),
                a.position,
                b.position,
            );
        }
    }
    worst
}

/// Item 1 of L-05: the reported hexapod run (rubble amplitude 0.08, cell 0.4, seed 11; walk
/// and steer 0.2 for 300 steps, then stop; 480 steps). No foot moves farther in one step than
/// the swing bound the agent publishes for the run.
#[test]
fn rubble_seed_11_no_foot_jumps_past_the_swing_bound() {
    let params = walker();
    let rubble = Rubble {
        amplitude: 0.08,
        cell: 0.4,
        seed: 11,
    };
    let ground = rubble.ground();
    let y = rubble.height(0.0, 0.0) + params.clearance;
    let mut state = GaitState::at_rest(&params, [0.0, y, 0.0], 0.0, ground).expect("stance");
    let (mut worst, mut worst_step, mut max_speed) = ((0.0, 0, false, false, [0.0; 3], [0.0; 3]), 0, 0.0_f32);
    for step in 0..480 {
        let control = if step < 300 { [1.0, 0.0, 0.2] } else { [0.0; 3] };
        let before = state;
        step_gait(&params, &mut state, control, DT, ground).expect("step");
        let v = state.velocity;
        max_speed = max_speed.max((v[0] * v[0] + v[2] * v[2]).sqrt());
        let jump = worst_move(&before, &state);
        if jump.0 > worst.0 {
            worst = jump;
            worst_step = step;
        }
    }
    let bound = swing_bound(&params, max_speed, DT);
    assert!(
        worst.0 <= bound + 1.0e-4,
        "step {worst_step} foot {} moved {:.4} > swing_bound {bound:.4} (swinging {} -> {}, {:?} -> {:?})",
        worst.1,
        worst.0,
        worst.2,
        worst.3,
        worst.4,
        worst.5
    );
}

/// The same over more seeds, steer signs and rubble heights.
#[test]
fn rubble_sweep_no_foot_jumps_past_the_swing_bound() {
    let params = walker();
    let mut failures = Vec::new();
    for seed in [1_u64, 5, 11, 23, 42] {
        for steer in [-0.4_f32, 0.2, 0.6] {
            for amplitude in [0.04_f32, 0.08, 0.15] {
                let rubble = Rubble {
                    amplitude,
                    cell: 0.4,
                    seed,
                };
                let ground = rubble.ground();
                let y = rubble.height(0.0, 0.0) + params.clearance;
                let mut state = GaitState::at_rest(&params, [0.0, y, 0.0], 0.0, ground).expect("stance");
                let (mut worst, mut max_speed) = (0.0_f32, 0.0_f32);
                for step in 0..480 {
                    let control = if step < 300 { [1.0, 0.0, steer] } else { [0.0; 3] };
                    let before = state;
                    step_gait(&params, &mut state, control, DT, ground).expect("step");
                    let v = state.velocity;
                    max_speed = max_speed.max((v[0] * v[0] + v[2] * v[2]).sqrt());
                    worst = worst.max(worst_move(&before, &state).0);
                }
                let bound = swing_bound(&params, max_speed, DT);
                if worst > bound + 1.0e-4 {
                    failures.push(format!(
                        "seed {seed} steer {steer} amplitude {amplitude}: {worst:.4} > {bound:.4}"
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Item 5 of L-05: a driven body creeping below `idle_speed` (a host that nudges its body a
/// little every step) never replants; planted feet keep their XZ bits and step only when the
/// body has drifted past `settle_distance`.
#[test]
fn a_driven_body_below_idle_speed_never_replants() {
    let params = walker();
    let speed = params.idle_speed * 0.5;
    let mut body = GaitBody {
        position: [0.0, params.clearance, 0.0],
        yaw: 0.3,
        velocity: [0.0; 3],
    };
    let mut state = GaitState::at_rest_driven(&params, body, flat).expect("stance");
    let (sin, cos) = body.yaw.sin_cos();
    let mut lifts = 0;
    for tick in 0..1200 {
        body.velocity = [-sin * speed, 0.0, -cos * speed];
        body.position[0] += body.velocity[0] * DT;
        body.position[2] += body.velocity[2] * DT;
        let before = state;
        step_gait_driven(&params, &mut state, body, DT, flat).expect("step");
        assert_eq!(state.replants, 0, "tick {tick}: a creeping body replanted");
        for (i, (a, b)) in before.feet().iter().zip(state.feet()).enumerate() {
            if a.planted && b.planted && !before.is_swinging(i) && !state.is_swinging(i) {
                assert_eq!(
                    (a.position[0].to_bits(), a.position[2].to_bits()),
                    (b.position[0].to_bits(), b.position[2].to_bits()),
                    "tick {tick} foot {i}: a planted foot snapped"
                );
            }
            if a.planted && !b.planted {
                lifts += 1;
            }
        }
    }
    // 1200 steps at 0.01 m/s is 0.2 m of drift: a few settle steps, not one per frame.
    assert!(lifts > 0, "the feet followed the drifting body");
    assert!(lifts <= 6 * 12, "{lifts} lift-offs for 0.2 m of drift");
}

/// Lift-offs of a driven body turning in place at `rate` rad/s for `ticks` steps, its yaw
/// handed to the stepper every `every` steps (a host that updates heading at a lower rate).
fn turn_lifts(params: &GaitParams, rate: f32, every: usize, ticks: usize) -> usize {
    let mut body = GaitBody {
        position: [0.0, params.clearance, 0.0],
        yaw: 0.0,
        velocity: [0.0; 3],
    };
    let mut state = GaitState::at_rest_driven(params, body, flat).expect("stance");
    let mut lifts = 0;
    for tick in 0..ticks {
        if (tick + 1).is_multiple_of(every) {
            body.yaw = rate * (tick + 1) as f32 * DT;
        }
        let before = state;
        step_gait_driven(params, &mut state, body, DT, flat).expect("step");
        lifts += before
            .feet()
            .iter()
            .zip(state.feet())
            .filter(|(a, b)| a.planted && !b.planted)
            .count();
    }
    lifts
}

/// KI-AN-005 (ADR-8539 known issue): a body yaw updated on only some steps (every third
/// step, a 20 Hz heading on a 60 Hz presentation) steps the feet no more often than the same
/// turn updated every step.
#[test]
fn a_yaw_updated_every_third_step_steps_no_more_often() {
    let params = walker();
    for rate in [0.3_f32, 0.6, 0.8] {
        let every_step = turn_lifts(&params, rate, 1, 600);
        let every_third = turn_lifts(&params, rate, 3, 600);
        assert!(every_step > 0, "{rate} rad/s: the feet stepped while turning");
        assert!(
            every_third <= every_step + 6,
            "{rate} rad/s: {every_third} lift-offs with a 20 Hz yaw against {every_step} at 60 Hz"
        );
    }
}

/// A driven body walked up to 2 m/s and stopped dead at tick `stop`: the number of steps after
/// the stop until the state reports `settled`, the worst one-step foot move (3D) in those
/// steps, and the settled state and body.
fn dead_stop(
    params: &GaitParams,
    ground: impl Fn(f32, f32) -> f32 + Copy,
    stop: usize,
) -> (usize, f32, GaitState, GaitBody) {
    let mut body = GaitBody {
        position: [1.0, ground(1.0, -2.0) + params.clearance, -2.0],
        yaw: -0.7,
        velocity: [0.0; 3],
    };
    let mut state = GaitState::at_rest_driven(params, body, ground).expect("stance");
    let (sin, cos) = body.yaw.sin_cos();
    for tick in 0..stop {
        let speed = (tick as f32 * 0.05).min(2.0);
        body.velocity = [-sin * speed, 0.0, -cos * speed];
        body.position[0] += body.velocity[0] * DT;
        body.position[2] += body.velocity[2] * DT;
        step_gait_driven(params, &mut state, body, DT, ground).expect("step");
    }
    assert!(!state.settled, "stop {stop}: walking is never settled");
    body.velocity = [0.0; 3];
    let mut worst = 0.0_f32;
    for after in 1..=600 {
        let before = state;
        step_gait_driven(params, &mut state, body, DT, ground).expect("step");
        worst = worst.max(worst_move(&before, &state).0);
        if state.settled {
            return (after, worst, state, body);
        }
    }
    panic!("stop {stop}: never settled");
}

/// World XZ of a hip in the driven (object) convention.
fn driven_neutral(body: &GaitBody, offset: [f32; 2]) -> [f32; 2] {
    let (sin, cos) = body.yaw.sin_cos();
    [
        body.position[0] + cos * offset[0] - sin * offset[1],
        body.position[2] - sin * offset[0] - cos * offset[1],
    ]
}

/// ADR-10162: a dead stop that catches a swing late in its arc keeps both properties of the
/// idle settle. (1) It settles within two `settle_period`s (one step absorbs phase rounding):
/// the swing in progress stretches so its landing still reaches neutral at touchdown, and
/// only the other tripod steps after it (without the stretch, stops past about three quarters
/// of a swing needed a third idle swing: 38-39 steps against a limit of 37). (2) No foot
/// moves more in one step than the bend limit allows the landing plus the arc's own travel:
/// the landing never drops its walking heading (`step_length / 2`, 0.3 m here) in one step.
/// Stops over three full walking cycles, so every arc progress at the stop is covered.
#[test]
fn a_late_dead_stop_stretches_its_swing_and_settles_within_two_periods() {
    let params = walker();
    let limit = (2.0 * params.settle_period / DT).ceil() as usize + 1;
    // One idle step moves a swinging foot by its arc's own travel plus at most the bend limit
    // (`2 * step_length * DT / settle_period`, 0.067 m here): measured 0.061 m at worst
    // (ADR-10162). 0.1 m is a third of the old one-step drop of `step_length / 2` (0.3 m).
    let jump_limit = 0.1_f32;
    let (mut slowest, mut worst) = ((0, 0), (0.0_f32, 0));
    let mut failures = Vec::new();
    for stop in 60..=170 {
        let (steps, jump, state, body) = dead_stop(&params, flat, stop);
        if steps > slowest.0 {
            slowest = (steps, stop);
        }
        if jump > worst.0 {
            worst = (jump, stop);
        }
        if steps > limit {
            failures.push(format!(
                "stop {stop}: settled after {steps} steps (limit {limit})"
            ));
        }
        if jump > jump_limit {
            failures.push(format!("stop {stop}: a foot moved {jump:.4} m in one step"));
        }
        assert_eq!(state.phase, 0.0, "stop {stop}");
        for (i, (foot, offset)) in state.feet().iter().zip(params.hip_offsets).enumerate() {
            let [nx, nz] = driven_neutral(&body, offset);
            let d = ((foot.position[0] - nx).powi(2) + (foot.position[2] - nz).powi(2)).sqrt();
            assert!(foot.planted, "stop {stop} foot {i}");
            assert!(
                d <= params.settle_distance + 1e-5,
                "stop {stop} foot {i}: {d} m from neutral"
            );
        }
    }
    eprintln!(
        "slowest settle {} steps (stop {}), worst one-step foot move {:.4} m (stop {})",
        slowest.0, slowest.1, worst.0, worst.1
    );
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The same dead stops on the agent's rubble (seed 11, amplitude 0.08): every stop settles,
/// and no foot moves farther in one step than flat ground's limit plus the rubble's height
/// range (measured 0.081 m at worst, ADR-10162). Refused footholds may add settle steps on
/// rubble, so only the one-step move is held here.
#[test]
fn a_late_dead_stop_on_rubble_never_jumps() {
    let params = walker();
    let rubble = Rubble {
        amplitude: 0.08,
        cell: 0.4,
        seed: 11,
    };
    let mut worst = (0.0_f32, 0);
    for stop in 60..=170 {
        let (_, jump, _, _) = dead_stop(&params, rubble.ground(), stop);
        if jump > worst.0 {
            worst = (jump, stop);
        }
    }
    eprintln!(
        "rubble: worst one-step foot move {:.4} m (stop {})",
        worst.0, worst.1
    );
    assert!(
        worst.0 <= 0.1 + 0.08,
        "stop {}: a foot moved {:.4} m in one step",
        worst.1,
        worst.0
    );
}
