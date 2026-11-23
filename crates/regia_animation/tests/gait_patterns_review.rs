// SPDX-License-Identifier: MPL-2.0
//! Review proofs for the gait patterns of ADR-8653 (lane anim-gait-patterns-review-c): edge
//! inputs, frame-rate independence of pattern switches, and state validation. Each test
//! names the finding it proves in the review report
//! (`<scratch>`). Synthetic settings; no
//! game content.

use regia_animation::gait::{
    FootMode, GaitBody, GaitError, GaitParams, GaitPattern, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS,
    step_gait_driven,
};

fn flat(_: f32, _: f32) -> f32 {
    0.0
}

/// Hips in pairs front to back, left (-X) before right, 0.4 m apart along the body (the
/// fixture of `tests/gait_patterns.rs`).
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

/// A driven body walking straight ahead (-Z), stepped at a fixed `dt`.
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

/// Finding F1: `GaitPattern` documents that `phase_offset` entries past `legs` are ignored,
/// and `validate` accepts any value there. A NaN in an unused slot makes the derived
/// `PartialEq` false for the pattern against its own copy, so `choose_pattern` starts a new
/// blend every time the previous one ends: the state blends forever on a steady walk with a
/// one-pattern set.
#[test]
fn review_f1_unused_nan_offset_must_not_blend_forever() {
    let mut trot = GaitPattern::trot();
    trot.phase_offset[10] = f32::NAN;
    assert_eq!(
        trot.validate(),
        Ok(()),
        "unused offsets are ignored by validation"
    );
    let set = GaitPatternSet::single(trot);
    assert_eq!(set.validate(), Ok(()));
    let params = params(set);
    let mut walker = Walker::new(60.0);
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");
    let mut blending_steps = 0;
    for tick in 0..600 {
        walker.step(&params, &mut state, 1.2).expect("valid walk");
        if tick >= 120 {
            blending_steps += usize::from(state.blending);
        }
    }
    assert_eq!(
        blending_steps, 0,
        "a one-pattern set at steady speed never switches; it blended on {blending_steps} of 480 steps"
    );
}

/// Lift-offs (planted -> not planted) of a 4 s walk at 1.2 m/s from `state`, refusals, and
/// the first refusal's error.
fn walk_lifts(params: &GaitParams, mut state: GaitState) -> (usize, usize, Option<GaitError>) {
    let mut walker = Walker::new(60.0);
    let (mut lifts, mut refused, mut first_error) = (0, 0, None);
    for _ in 0..240 {
        let before = state;
        match walker.step(params, &mut state, 1.2) {
            Ok(()) => {
                lifts += before
                    .feet()
                    .iter()
                    .zip(state.feet())
                    .filter(|(a, b)| a.planted && !b.planted)
                    .count();
            }
            Err(error) => {
                assert_eq!(state, before, "refused atomically");
                refused += 1;
                first_error.get_or_insert(error);
            }
        }
    }
    (lifts, refused, first_error)
}

/// Corrupts one field of a pattern.
type Spoil = fn(&mut GaitPattern);

/// Finding F2: `GaitState::pattern` is a pub, reflected field (writable by the host and by
/// `scene.set_property`), but `check` never validates it (nor the private reflected
/// `blend_from`). A state whose pattern has duty factor 0 is stepped without error and the
/// gait freezes: the switch back to the set's pattern advances by `progress * duty == 0`
/// per step, so the blend never ends and no foot lifts again while the body walks away
/// from the feet. The documented contract is "invalid ... state, settings, pattern ...
/// refused atomically". The other corrupt values below happen to heal in one step (the
/// NaN or out-of-range blend ends at once); the test accepts healing, so only the freeze
/// fails it.
#[test]
fn review_f2_invalid_state_pattern_is_refused_or_heals() {
    let params = params(GaitPatternSet::single(GaitPattern::trot()));
    let walker = Walker::new(60.0);
    let fresh = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");
    let (healthy, refused, _) = walk_lifts(&params, fresh);
    assert_eq!(refused, 0);
    let corrupt: [(&str, Spoil); 5] = [
        ("duty 0", |p| p.duty_factor = 0.0),
        ("duty 1", |p| p.duty_factor = 1.0),
        ("duty NaN", |p| p.duty_factor = f32::NAN),
        ("offset 2.5", |p| p.phase_offset[0] = 2.5),
        ("offset -0.75", |p| p.phase_offset[1] = -0.75),
    ];
    let mut frozen = Vec::new();
    for (name, spoil) in corrupt {
        let mut state = fresh;
        spoil(&mut state.pattern);
        assert!(
            state.pattern.validate().is_err(),
            "{name}: the fixture is invalid"
        );
        let (lifts, refused, error) = walk_lifts(&params, state);
        // Refused every step (gait_pattern / gait_state), or stepped and walking normally.
        let ok = refused == 240 || (refused == 0 && 5 * lifts >= 4 * healthy);
        if !ok {
            frozen.push(format!(
                "{name}: {refused} refused ({error:?}), {lifts} lift-offs in 4 s (healthy walk: {healthy})"
            ));
        }
    }
    assert!(
        frozen.is_empty(),
        "an invalid GaitState::pattern must be refused, not stepped into a frozen gait: {frozen:#?}"
    );
}

/// What one switch walk measured.
struct SwitchWalk {
    /// Planted feet whose pattern phase wrapped past touchdown with no swing: each stays on
    /// its old plant for another stride.
    skipped: usize,
    /// Largest XZ distance of a planted foot from its hip, in step lengths.
    stretch: f32,
    /// The first few skipped steps.
    examples: Vec<String>,
}

/// Walk `set` driven at `hz`: `slow` m/s for 2 s, `fast` for 2 s, `slow` for 2 s, `fast`
/// for 1 s, `slow` for 1 s (each speed change is one switch). The same profile in seconds at
/// every rate.
fn switch_walk(set: GaitPatternSet, step_length: f32, slow: f32, fast: f32, hz: f32) -> SwitchWalk {
    let params = GaitParams {
        step_length,
        ..params(set)
    };
    let legs = set.leg_count();
    let mut walker = Walker::new(hz);
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");
    let mut out = SwitchWalk {
        skipped: 0,
        stretch: 0.0,
        examples: Vec::new(),
    };
    for tick in 0..(8.0 * hz).round() as usize {
        let t = tick as f32 / hz;
        let speed = if (2.0..4.0).contains(&t) || (6.0..7.0).contains(&t) {
            fast
        } else {
            slow
        };
        let before = state;
        walker.step(&params, &mut state, speed).expect("valid walk");
        for i in 0..legs {
            let (old, new) = (before.feet[i], state.feet[i]);
            let same_xz = old.position[0].to_bits() == new.position[0].to_bits()
                && old.position[2].to_bits() == new.position[2].to_bits();
            // A phase within the event tolerance of 1 is touchdown (f32 rounding of offsets).
            let touchdown = |p: f32| if p >= 1.0 - 1e-5 { 0.0 } else { p };
            let (was, now) = (touchdown(before.foot_phase[i]), touchdown(state.foot_phase[i]));
            if old.planted
                && new.planted
                && same_xz
                && was - now > 0.5
                && !before.is_swinging(i)
                && !state.is_swinging(i)
            {
                out.skipped += 1;
                if out.examples.len() < 4 {
                    out.examples.push(format!(
                        "{hz} Hz tick {tick} foot {i}: phase {} -> {} planted throughout (blending {} -> {})",
                        before.foot_phase[i], state.foot_phase[i], before.blending, state.blending
                    ));
                }
            }
            if new.planted {
                // Driven mode at yaw 0: right is +X, forward is -Z.
                let hip = params.hip_offsets[i];
                let (hx, hz_) = (state.position[0] + hip[0], state.position[2] - hip[1]);
                let d = ((new.position[0] - hx).powi(2) + (new.position[2] - hz_).powi(2)).sqrt();
                out.stretch = out.stretch.max(d / step_length);
            }
        }
    }
    out
}

/// Finding F3: during a pattern switch, `blend_feet` lifts a planted foot only when its new
/// phase is at or past the blended duty *and did not wrap* in this step. When one step moves
/// a foot's phase across its whole swing window (`1 - duty`, 1/16 of a cycle for wave(16)),
/// the foot never lifts and stays on its old plant for another stride. Steady walking
/// (`walk_feet`) lifts and lands such a foot within the step, so only switches skip steps,
/// and the count depends on the host's timestep. Measured at the reviewed head (step 0.3 m,
/// 0.5 <-> 1.5 m/s): 9 / 1 / 0 skipped steps at 30 / 60 / 144 Hz, planted feet up to 1.83 /
/// 1.64 / 1.00 step lengths from their hips.
#[test]
fn review_f3_switch_skips_no_step_at_30_60_144_hz() {
    let set = GaitPatternSet::new(
        &[
            GaitPattern::wave(16).expect("preset"),
            GaitPattern::ripple(16).expect("preset"),
        ],
        &[1.0],
        0.05,
    )
    .expect("valid set");
    let mut failures = Vec::new();
    for hz in [30.0, 60.0, 144.0] {
        let walk = switch_walk(set, 0.3, 0.5, 1.5, hz);
        if walk.skipped > 0 {
            failures.push(format!(
                "{hz} Hz: {} skipped steps, stretch {:.2} step lengths; {:#?}",
                walk.skipped, walk.stretch, walk.examples
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Finding F5 (hazard): walking from rest with a pattern whose swings overlap (ripple(n),
/// bound), the beat clock starts at beat 0 with every foot planted, so a foot the schedule
/// already has mid-swing stays planted through that whole swing (one long first stance, up
/// to `2 - duty` cycles), and its reported `foot_phase` says swing (phase >= duty) while it
/// is planted. `choose_pattern` lifts such feet at once on a switch ("a planted foot whose
/// turn the clock skipped ... lifts at once"), but `walk_feet` from rest does not. Measured:
/// ripple(6) foot 3 reports phase 0.837 (duty 0.667) planted from the first step.
#[test]
fn review_f5_walk_from_rest_keeps_phase_and_contact_in_step() {
    for pattern in [GaitPattern::ripple(6).expect("preset"), GaitPattern::bound()] {
        let params = params(GaitPatternSet::single(pattern));
        for hz in [30.0, 60.0, 144.0] {
            let mut walker = Walker::new(hz);
            let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");
            let mut late = Vec::new();
            for tick in 0..(2.0 * hz) as usize {
                walker.step(&params, &mut state, 1.0).expect("valid walk");
                for i in 0..state.leg_count() {
                    // Planted deeper into the swing window than one step of phase can explain.
                    if state.feet[i].planted
                        && state.foot_modes[i] == FootMode::Tripod
                        && state.foot_phase[i] > pattern.duty_factor + 0.05
                        && state.foot_phase[i] < 1.0 - 1e-5
                        && late.len() < 4
                    {
                        late.push(format!(
                            "tick {tick} foot {i}: planted at phase {} (duty {})",
                            state.foot_phase[i], pattern.duty_factor
                        ));
                    }
                }
            }
            assert!(late.is_empty(), "{pattern:?} at {hz} Hz: {late:#?}");
        }
    }
}

/// Finding F4 (hazard): with no foot planted (the flight phases of a bound, duty 0.4) the
/// margin is negative infinity. Agents read it as JSON `null`, and `play.expect` with
/// `lt`/`lte` (the book's own "statically unstable" example) then fails with "op needs
/// numbers" exactly during flight. A finite sentinel (or a separate `supported` flag) keeps
/// the documented assertion usable.
#[test]
fn review_f4_bound_margin_stays_finite_for_agents() {
    let params = params(GaitPatternSet::single(GaitPattern::bound()));
    let mut walker = Walker::new(60.0);
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");
    let mut nonfinite = Vec::new();
    for tick in 0..300 {
        walker.step(&params, &mut state, 2.0).expect("valid walk");
        if !state.stability_margin.is_finite() && nonfinite.len() < 4 {
            let planted = state.feet().iter().filter(|f| f.planted).count();
            nonfinite.push(format!(
                "tick {tick}: margin {} with {planted} planted",
                state.stability_margin
            ));
        }
    }
    assert!(
        nonfinite.is_empty(),
        "stability_margin is serialized for agents and compared with lt/lte: {nonfinite:#?}"
    );
}

/// Check (expected to pass): steady walking is frame-rate independent within one step of
/// travel. Same trot, same speed, same 2 s of walking at 30, 60 and 144 Hz: the body ends
/// at the same place, every foot steps the same number of times, and each planted foot is
/// within `speed / 30` m of the 144 Hz plant (a landing is placed when the step that ends
/// the swing runs, so it can be late by at most one step of travel).
#[test]
fn review_check_steady_walk_is_frame_rate_independent_within_one_step() {
    let speed = 1.5;
    for pattern in [
        GaitPattern::trot(),
        GaitPattern::quadruped_walk(),
        GaitPattern::wave(6).expect("preset"),
        GaitPattern::tripod(),
    ] {
        let params = params(GaitPatternSet::single(pattern));
        let legs = pattern.leg_count();
        let run = |hz: f32| -> (GaitState, Vec<usize>) {
            let mut walker = Walker::new(hz);
            let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");
            let mut lifts = vec![0; legs];
            for _ in 0..(2.0 * hz).round() as usize {
                let before = state;
                walker.step(&params, &mut state, speed).expect("valid walk");
                for (i, lift) in lifts.iter_mut().enumerate() {
                    *lift += usize::from(!before.is_swinging(i) && state.is_swinging(i));
                }
            }
            (state, lifts)
        };
        let (reference, reference_lifts) = run(144.0);
        for hz in [30.0, 60.0] {
            let (state, lifts) = run(hz);
            assert!(
                (state.position[2] - reference.position[2]).abs() < 1e-3,
                "{pattern:?} {hz} Hz: body z"
            );
            let lift_gap = lifts
                .iter()
                .zip(&reference_lifts)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                lift_gap <= 1,
                "{hz} Hz lifts {lifts:?} vs 144 Hz {reference_lifts:?}"
            );
            for i in 0..legs {
                let (a, b) = (state.feet[i], reference.feet[i]);
                if a.planted && b.planted {
                    let d = ((a.position[0] - b.position[0]).powi(2)
                        + (a.position[2] - b.position[2]).powi(2))
                    .sqrt();
                    assert!(
                        d <= speed / 30.0 + 1e-4,
                        "{hz} Hz foot {i}: plant {d} m from the 144 Hz plant"
                    );
                }
            }
        }
    }
}

/// Check (expected to pass): edge speeds. Speed exactly at `idle_speed` is idle; a huge
/// finite velocity is refused atomically (no NaN reaches the feet); a velocity that
/// overflows when squared is refused too.
#[test]
fn review_check_edge_speeds_refuse_atomically() {
    let params = params(GaitPatternSet::single(GaitPattern::trot()));
    let mut walker = Walker::new(60.0);
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");
    walker
        .step(&params, &mut state, params.idle_speed)
        .expect("idle speed is idle");
    assert!(state.settled);
    for speed in [1e3_f32, 1e20, f32::MAX] {
        let before = state;
        let mut body = walker.body;
        body.velocity = [0.0, 0.0, -speed];
        let result = step_gait_driven(&params, &mut state, body, 1.0 / 60.0, flat);
        assert!(result.is_err(), "speed {speed}: {result:?}");
        assert_eq!(state, before, "speed {speed}: atomic");
    }
    assert!(
        state
            .feet()
            .iter()
            .all(|f| f.position.iter().all(|v| v.is_finite()))
    );
}

/// Exploration for F3 (ignored): switch sweeps over step length, speed profile and rate,
/// printing skipped steps, phase/contact mismatches and the worst planted-foot stretch (XZ
/// distance from its hip, in step lengths).
#[test]
#[ignore = "exploration; run with --ignored --nocapture"]
fn review_f3_sweep() {
    let sets = [
        (
            "wave16-ripple16",
            vec![
                GaitPattern::wave(16).expect("preset"),
                GaitPattern::ripple(16).expect("preset"),
            ],
        ),
        (
            "wave6-tripod",
            vec![GaitPattern::wave(6).expect("preset"), GaitPattern::tripod()],
        ),
        (
            "ripple6-tripod",
            vec![GaitPattern::ripple(6).expect("preset"), GaitPattern::tripod()],
        ),
        (
            "walk-trot",
            vec![GaitPattern::quadruped_walk(), GaitPattern::trot()],
        ),
        ("trot-bound", vec![GaitPattern::trot(), GaitPattern::bound()]),
        (
            "wave4-pace",
            vec![GaitPattern::wave(4).expect("preset"), GaitPattern::pace()],
        ),
    ];
    for (name, patterns) in &sets {
        for step_length in [0.3_f32, 0.6] {
            for (slow, fast) in [(0.5_f32, 1.5_f32), (1.0, 3.0), (2.0, 5.0)] {
                let switch = 0.5 * (slow + fast);
                let set = GaitPatternSet::new(patterns, &[switch], 0.05).expect("valid set");
                let params = GaitParams {
                    step_length,
                    ..params(set)
                };
                let legs = set.leg_count();
                for hz in [30.0_f32, 60.0, 144.0] {
                    let mut walker = Walker::new(hz);
                    let mut state =
                        GaitState::at_rest_driven(&params, walker.body, flat).expect("valid fixture");
                    let (mut skipped, mut mismatched, mut stretch) = (0, 0, 0.0_f32);
                    let mut error = None;
                    for tick in 0..(8.0 * hz) as usize {
                        let t = tick as f32 / hz;
                        let speed = if (2.0..4.0).contains(&t) || (6.0..7.0).contains(&t) {
                            fast
                        } else {
                            slow
                        };
                        let before = state;
                        if let Err(e) = walker.step(&params, &mut state, speed) {
                            error = Some((tick, e));
                            break;
                        }
                        for i in 0..legs {
                            let (old, new) = (before.feet[i], state.feet[i]);
                            let same = old.position[0].to_bits() == new.position[0].to_bits()
                                && old.position[2].to_bits() == new.position[2].to_bits();
                            if old.planted
                                && new.planted
                                && same
                                && before.foot_phase[i] - state.foot_phase[i] > 0.5
                                && !before.is_swinging(i)
                                && !state.is_swinging(i)
                            {
                                skipped += 1;
                            }
                            if !state.blending
                                && state.foot_modes[i] == FootMode::Tripod
                                && (state.foot_phase[i] < state.pattern.duty_factor) == state.is_swinging(i)
                            {
                                mismatched += 1;
                            }
                            if new.planted {
                                let hip = params.hip_offsets[i];
                                let (hx, hz_) = (state.position[0] + hip[0], state.position[2] - hip[1]);
                                let d =
                                    ((new.position[0] - hx).powi(2) + (new.position[2] - hz_).powi(2)).sqrt();
                                stretch = stretch.max(d / step_length);
                            }
                        }
                    }
                    println!(
                        "{name} step {step_length} speeds {slow}/{fast} {hz} Hz: skipped {skipped} mismatched {mismatched} stretch {stretch:.2} error {error:?}"
                    );
                }
            }
        }
    }
}
