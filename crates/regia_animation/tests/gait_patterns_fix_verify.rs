// SPDX-License-Identifier: MPL-2.0
//! Verification proofs for the gait-pattern fixes of ADR-8736 (lane
//! anim-gait-patterns-fixes, head 007dc02b0). The review-c file
//! `gait_patterns_review.rs` is unchanged. These tests are the cases next to
//! those fixes: a late lift carried into a pattern switch, and a switch step
//! that wraps a foot's phase past the next lift-off. Synthetic settings; no
//! game content.

use regia_animation::gait::{
    GaitBody, GaitError, GaitParams, GaitPattern, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS,
    step_gait_driven,
};

fn flat(_: f32, _: f32) -> f32 {
    0.0
}

/// Hips in pairs front to back, left (-X) before right, 0.4 m apart along the body.
fn hips(legs: usize) -> [[f32; 2]; MAX_LEGS] {
    let mut out = [[0.0; 2]; MAX_LEGS];
    let pairs = legs.div_ceil(2);
    for (i, hip) in out.iter_mut().enumerate().take(legs) {
        let pair = i / 2;
        let side = if i % 2 == 0 { -0.3 } else { 0.3 };
        *hip = [side, 0.2 * (pairs as f32 - 1.0) - 0.4 * pair as f32];
    }
    out
}

/// The settings of `tests/gait_patterns.rs`: ring search off, reach unlimited.
fn params(set: GaitPatternSet) -> GaitParams {
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
        hip_offsets: hips(set.leg_count()),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.15,
        turn_step_angle: 0.35,
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [f32::INFINITY; MAX_LEGS],
        hang_drop: 0.3,
        patterns: set,
        posture: GaitPosture::UPRIGHT,
    }
}

/// A driven body walking straight ahead (-Z).
struct Walker {
    body: GaitBody,
    dt: f32,
}

impl Walker {
    fn new(hz: f32) -> Self {
        Self {
            body: GaitBody {
                position: [0.0; 3],
                yaw: 0.0,
                velocity: [0.0; 3],
            },
            dt: 1.0 / hz,
        }
    }

    fn step(&mut self, params: &GaitParams, state: &mut GaitState, speed: f32) -> Result<(), GaitError> {
        self.body.velocity = [0.0, 0.0, -speed];
        self.body.position[2] -= speed * self.dt;
        step_gait_driven(params, state, self.body, self.dt, flat)
    }
}

/// Signed phase change in `(-0.5, 0.5]`, so a wrap from 0.99 to 0.01 is a small
/// forward step and a drop from 0.87 to 0.67 stays negative.
fn phase_delta(before: f32, after: f32) -> f32 {
    let mut delta = after - before;
    if delta > 0.5 {
        delta -= 1.0;
    } else if delta <= -0.5 {
        delta += 1.0;
    }
    delta
}

/// Finding V1. `walk_feet` stores a late lift (a foot the schedule already had in
/// the air at rest) as `swing_start` plus a compressed arc in `swing_progress`.
/// `choose_pattern` then rebuilds that foot's phase as `duty + swing_progress *
/// swing` and clears `swing_start` (`gait.rs` around the late-lift fold). That
/// treats the compressed arc as a full swing, so the reported phase jumps
/// backward and the foot stays in the air until a whole swing has elapsed
/// instead of the remainder the schedule had left.
///
/// Input: `ripple(6)` from rest, one 60 Hz step at 0.5 m/s (the late feet lift),
/// then one more step at the same speed after a used offset is nudged so the
/// set's pattern no longer matches. The nudge is not on the measured foot, and
/// one step of blend moves a phase by about `dt / cycle_time` (~0.01), not by
/// tenths of a cycle.
#[test]
fn verify_v1_late_lift_blend_rewinds_phase_and_lands_late() {
    let ripple = GaitPattern::ripple(6).expect("preset");
    let duty = ripple.duty_factor;
    let set = GaitPatternSet::new(&[ripple, GaitPattern::tripod()], &[1.0], 0.05).expect("valid set");
    let params = params(set);
    let speed = 0.5;
    let mut walker = Walker::new(60.0);
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");
    walker.step(&params, &mut state, speed).expect("first step");

    let late: Vec<usize> = (0..state.leg_count())
        .filter(|&i| state.is_swinging(i) && state.foot_phase[i] > duty + 0.15)
        .collect();
    assert!(
        !late.is_empty(),
        "fixture: ripple(6) from rest leaves a foot mid-swing, duty {duty}, phases {:?}",
        &state.foot_phase[..state.leg_count()]
    );

    // Force a blend back to the set's pattern without changing speed or the
    // measured feet's offsets.
    let nudge = late[0];
    let other = (0..state.leg_count())
        .find(|&i| i != nudge)
        .expect("another foot");
    let mut offset = state.pattern.phase_offset[other] + 0.25;
    if offset >= 1.0 {
        offset -= 1.0;
    }
    state.pattern.phase_offset[other] = offset;

    let before: Vec<f32> = late.iter().map(|&i| state.foot_phase[i]).collect();
    walker.step(&params, &mut state, speed).expect("switch step");
    assert!(state.blending, "the nudged pattern must start a switch");
    let mut rewound = Vec::new();
    for (&i, &phase_before) in late.iter().zip(&before) {
        let delta = phase_delta(phase_before, state.foot_phase[i]);
        if delta < -0.05 {
            rewound.push(format!(
                "foot {i}: phase {phase_before} -> {} (delta {delta})",
                state.foot_phase[i]
            ));
        }
    }

    // Schedule: the phase reported before the switch reaches touchdown in
    // `(1 - phase) * cycle_time` seconds. The blend step already consumed one
    // frame. A correct fold lands inside a few frames of that; a full swing
    // restarted from the compressed arc does not.
    let cycle_time = params.step_period_at_speed(speed) / duty;
    let saved = state;
    let saved_body = walker.body;
    let mut late_landing = Vec::new();
    for (&i, &phase_before) in late.iter().zip(&before) {
        state = saved;
        walker.body = saved_body;
        let remain = (1.0 - phase_before) * cycle_time;
        let limit = (remain / walker.dt).ceil() as usize + 4;
        let mut steps = 0;
        let mut landed = !state.is_swinging(i);
        while !landed && steps < limit {
            walker.step(&params, &mut state, speed).expect("blend continues");
            steps += 1;
            landed = !state.is_swinging(i);
        }
        if !landed {
            late_landing.push(format!(
                "foot {i}: still swinging {steps} steps after the switch (phase was {phase_before}, \
                 schedule left {remain:.3} s, about {} frames)",
                (remain / walker.dt).ceil()
            ));
        }
    }

    assert!(
        rewound.is_empty() && late_landing.is_empty(),
        "a late lift must keep its scheduled phase through a switch.\nrewound: {rewound:#?}\n\
         landed late: {late_landing:#?}"
    );
}

/// The same pattern with every used offset moved `by` cycles (wrapped into `[0, 1)`).
fn shifted(pattern: GaitPattern, by: f32) -> GaitPattern {
    let n = pattern.leg_count();
    let mut offsets = [0.0_f32; MAX_LEGS];
    for (slot, &offset) in offsets[..n].iter_mut().zip(&pattern.phase_offset[..n]) {
        let mut value = offset + by;
        if value >= 1.0 {
            value -= 1.0;
        }
        *slot = value;
    }
    GaitPattern::new(&offsets[..n], pattern.duty_factor).expect("shifted pattern")
}

/// Finding V2. `blend_feet` lands a swinging foot whose phase reaches 1, and the
/// ADR-8736 wrap branch lifts and lands a foot that was planted. Neither branch
/// starts the next swing when the wrapped phase is still past the duty factor, so
/// the foot is planted while `foot_phase` says swing.
///
/// Input: quadruped walk (duty 0.75) switching to the same duty with every offset
/// `+0.5` (the short-way maximum). Walk at 60 Hz until a foot has just lifted,
/// then one 30 Hz step at 95% of one stance (`speed = 0.95 * step_length / dt`,
/// progress 0.95; progress above 1 is `gait_step_too_large`). The offset term
/// adds about `0.5 * duty * progress` of phase on top of the stance advance, so
/// the phase wraps and lands inside the next swing.
#[test]
fn verify_v2_max_step_wrap_leaves_foot_planted_in_swing() {
    let walk = GaitPattern::quadruped_walk();
    let duty = walk.duty_factor;
    let set = GaitPatternSet::new(&[walk, shifted(walk, 0.5)], &[1.0], 0.05).expect("valid set");
    let step_length = 0.5;
    let params = GaitParams {
        step_length,
        ..params(set)
    };
    let mut walker = Walker::new(60.0);
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");

    let mut lifted = None;
    for _ in 0..600 {
        let was: Vec<bool> = (0..state.leg_count()).map(|i| state.is_swinging(i)).collect();
        walker.step(&params, &mut state, 0.4).expect("slow walk");
        for (i, was_swinging) in was.iter().enumerate() {
            let phase = state.foot_phase[i];
            if !was_swinging && state.is_swinging(i) && phase >= duty && phase < duty + 0.08 {
                lifted = Some(i);
                break;
            }
        }
        if lifted.is_some() {
            break;
        }
    }
    let foot = lifted.expect("a foot lifts within 10 s of quadruped walk");
    let phase_before = state.foot_phase[foot];

    // 95% of one stance at 30 Hz. Exactly 1.0 can round to a refusal; 0.95 still
    // wraps a just-lifted foot (duty 0.75, offset delta +0.5) into the next swing.
    let dt = 1.0 / 30.0;
    let speed = step_length / dt * 0.95;
    walker.dt = dt;
    walker.step(&params, &mut state, speed).expect("max switch step");

    let phase = state.foot_phase[foot];
    assert!(
        state.blending,
        "crossing the switch speed must blend (foot {foot} phase {phase_before} -> {phase})"
    );
    assert!(
        phase > duty + 0.02 && phase < 1.0 - 1e-4,
        "fixture: foot {foot} wrapped phase {phase} (from {phase_before}, duty {duty}) should sit \
         inside the next swing; the step was progress 0.95 with offset delta +0.5"
    );
    assert!(
        state.is_swinging(foot) && !state.feet[foot].planted,
        "foot {foot} phase {phase} is past duty {duty} (was {phase_before}) but planted {}, \
         swinging {}",
        state.feet[foot].planted,
        state.is_swinging(foot)
    );
}
