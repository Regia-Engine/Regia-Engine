// SPDX-License-Identifier: MPL-2.0
//! Gait patterns for any leg count (ADR-8653): presets, duty factor, wave, trot and pace
//! contact sets, speed switching without foot jumps, validation, the static stability margin
//! and allocation-free stepping. Synthetic settings; no game content.

use regia_animation::gait::{
    GaitBody, GaitError, GaitParams, GaitPattern, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS,
    TRIPOD_LEGS, leg_array, step_gait, step_gait_driven, support_margin,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

const DT: f32 = 1.0 / 60.0;

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

/// Settings for `set`, the ring search off and reach unlimited, so feet follow the pattern.
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

/// The six-leg fixture of `tests/tripod_gait.rs` (ADR-8060) with the given patterns.
fn tripod_params(set: GaitPatternSet) -> GaitParams {
    GaitParams {
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
        max_reach: [5.0; MAX_LEGS],
        ..params(set)
    }
}

/// A driven body walking straight ahead (-Z) at `speed`.
struct Walker {
    body: GaitBody,
}

impl Walker {
    fn new() -> Self {
        Self {
            body: GaitBody {
                position: [0.0; 3],
                yaw: 0.0,
                velocity: [0.0; 3],
            },
        }
    }

    fn step(&mut self, params: &GaitParams, state: &mut GaitState, speed: f32) {
        self.body.velocity = [0.0, 0.0, -speed];
        self.body.position[2] -= speed * DT;
        step_gait_driven(params, state, self.body, DT, flat).expect("valid gait fixture");
    }
}

fn planted_set(state: &GaitState) -> Vec<usize> {
    (0..state.leg_count())
        .filter(|&i| state.feet[i].planted)
        .collect()
}

/// The swing bound of `tests/tripod_gait.rs` (animation-02): a foot moves at most
/// `step_length + swing_lift` in one step, and a foot planted before and after keeps its
/// world XZ bit for bit.
fn check_step(params: &GaitParams, before: &GaitState, after: &GaitState, tick: usize) {
    let bound = params.step_length + params.swing_lift;
    for (i, (old, new)) in before.feet().iter().zip(after.feet()).enumerate() {
        let delta = old
            .position
            .iter()
            .zip(new.position)
            .map(|(a, b)| (b - a) * (b - a))
            .sum::<f32>()
            .sqrt();
        assert!(delta <= bound, "tick {tick} foot {i}: moved {delta} m");
        if old.planted && new.planted {
            assert_eq!(
                (old.position[0].to_bits(), old.position[2].to_bits()),
                (new.position[0].to_bits(), new.position[2].to_bits()),
                "tick {tick} foot {i}: a planted foot slid or snapped"
            );
        }
    }
}

/// The `presentation_trace_pin` scenario of `tests/tripod_gait.rs`, hashed the same way.
fn trace_hash(params: &GaitParams) -> u64 {
    let mut state = GaitState::at_rest(params, [0.0; 3], 0.0, flat).expect("valid gait fixture");
    let mut hash = 0xcbf29ce484222325_u64;
    let mut fold = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    };
    for tick in 0..360 {
        let controls = match tick / 90 {
            0 => [1.0, 0.0, 0.0],
            1 => [0.6, 0.2, 0.3],
            2 => [-0.7, -0.2, -0.4],
            _ => [0.0; 3],
        };
        step_gait(params, &mut state, controls, 1.0 / 60.0, |x, z| {
            0.02 * x - 0.03 * z
        })
        .expect("valid gait fixture");
        for value in state
            .position
            .into_iter()
            .chain(state.velocity)
            .chain([state.yaw, state.phase])
        {
            fold(&value.to_bits().to_le_bytes());
        }
        fold(&[u8::from(state.swing_a), u8::from(state.power_lost)]);
        for foot in state.feet() {
            for value in foot.position {
                fold(&value.to_bits().to_le_bytes());
            }
            fold(&[u8::from(foot.planted)]);
        }
    }
    hash
}

#[test]
fn tripod_preset_matches_the_six_leg_stepper_bit_for_bit() {
    // The pin of `presentation_trace_pin` (ADR-8060, unchanged through ADR-8792).
    const PIN: u64 = 0x8e1c_5c05_2242_34f3;
    let preset = GaitPattern::tripod();
    assert_eq!(preset.leg_count(), TRIPOD_LEGS);
    assert_eq!(preset.duty_factor, 0.5);
    assert_eq!(trace_hash(&tripod_params(GaitPatternSet::tripod())), PIN);
    // The same gait spelled out as explicit offsets goes through the same pattern path.
    let explicit = GaitPattern::new(&[0.5, 0.0, 0.0, 0.5, 0.5, 0.0], 0.5).expect("valid pattern");
    assert_eq!(explicit, preset);
    let set = GaitPatternSet::new(&[explicit], &[], 0.0).expect("valid set");
    assert_eq!(trace_hash(&tripod_params(set)), PIN);
}

#[test]
fn duty_factor_sets_the_stance_fraction() {
    let speed = 1.2;
    let cases = [
        GaitPattern::tripod(),
        GaitPattern::quadruped_walk(),
        GaitPattern::wave(6).expect("valid preset"),
        GaitPattern::biped_walk(),
        GaitPattern::bound(),
        GaitPattern::ripple(6).expect("valid preset"),
        GaitPattern::new(&[0.0, 0.3, 0.55], 0.7).expect("valid pattern"),
    ];
    for pattern in cases {
        let params = params(GaitPatternSet::single(pattern));
        let mut walker = Walker::new();
        let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
        // One stance lasts step_length / speed; the cycle is that over the duty factor.
        let stance = params.step_length / speed;
        let cycle = stance / pattern.duty_factor;
        let warmup = (2.0 * cycle / DT).ceil() as usize;
        let legs = pattern.leg_count();
        let mut run = vec![0_usize; legs];
        let mut stances = vec![Vec::new(); legs];
        for tick in 0..warmup + (4.0 * cycle / DT) as usize {
            walker.step(&params, &mut state, speed);
            for ((foot, run), stances) in state.feet().iter().zip(&mut run).zip(&mut stances) {
                if foot.planted {
                    *run += 1;
                } else {
                    if tick > warmup && *run > 0 {
                        stances.push(*run);
                    }
                    *run = 0;
                }
            }
        }
        for (i, runs) in stances.iter().enumerate() {
            assert!(runs.len() >= 3, "{pattern:?} foot {i}: {runs:?}");
            for &steps in runs {
                let planted = steps as f32 * DT;
                assert!(
                    (planted - pattern.duty_factor * cycle).abs() <= DT * 1.01,
                    "{pattern:?} foot {i}: planted {planted} s, duty x period {} s",
                    pattern.duty_factor * cycle
                );
            }
        }
    }
}

#[test]
fn wave_gait_keeps_n_minus_one_feet_planted() {
    for n in [4, 6, 8] {
        let pattern = GaitPattern::wave(n).expect("valid preset");
        assert!((pattern.duty_factor - (n as f32 - 1.0) / n as f32).abs() < 1e-6);
        let params = params(GaitPatternSet::single(pattern));
        let mut walker = Walker::new();
        let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
        let mut lifts = vec![0_usize; n];
        let mut exactly = 0;
        for tick in 0..900 {
            let before = state;
            walker.step(&params, &mut state, 1.2);
            check_step(&params, &before, &state, tick);
            let planted = planted_set(&state).len();
            assert!(planted >= n - 1, "wave({n}) tick {tick}: {planted} planted");
            exactly += usize::from(planted == n - 1);
            for ((lift, old), new) in lifts.iter_mut().zip(before.feet()).zip(state.feet()) {
                *lift += usize::from(old.planted && !new.planted);
            }
        }
        assert!(exactly > 600, "wave({n}): one foot swings most of the time");
        assert!(
            lifts.iter().all(|&k| k >= 4),
            "wave({n}): every foot steps: {lifts:?}"
        );
    }
}

/// Walk `pattern` on four legs and return every planted set after the first step, plus the
/// largest stability margin seen while walking.
fn quadruped_contacts(pattern: GaitPattern) -> (Vec<Vec<usize>>, f32) {
    let params = params(GaitPatternSet::single(pattern));
    let mut walker = Walker::new();
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
    let mut sets = Vec::new();
    let mut most = f32::NEG_INFINITY;
    for tick in 0..600 {
        let before = state;
        walker.step(&params, &mut state, 1.5);
        check_step(&params, &before, &state, tick);
        sets.push(planted_set(&state));
        most = most.max(state.stability_margin);
    }
    (sets, most)
}

#[test]
fn trot_plants_diagonal_pairs() {
    let (sets, most) = quadruped_contacts(GaitPattern::trot());
    // 0 left fore, 1 right fore, 2 left hind, 3 right hind.
    let diagonals = [vec![0, 3], vec![1, 2]];
    for (tick, set) in sets.iter().enumerate() {
        assert!(diagonals.contains(set), "tick {tick}: planted {set:?}");
    }
    assert!(diagonals.iter().all(|d| sets.contains(d)));
    assert!(most <= 0.0, "a trot is statically unstable: margin {most}");
    let (_, walk) = quadruped_contacts(GaitPattern::quadruped_walk());
    assert!(walk > 0.0, "a walk has statically stable moments: margin {walk}");
}

#[test]
fn pace_plants_lateral_pairs() {
    let (sets, most) = quadruped_contacts(GaitPattern::pace());
    let laterals = [vec![0, 2], vec![1, 3]];
    for (tick, set) in sets.iter().enumerate() {
        assert!(laterals.contains(set), "tick {tick}: planted {set:?}");
    }
    assert!(laterals.iter().all(|l| sets.contains(l)));
    assert!(most <= 0.0, "a pace is statically unstable: margin {most}");
    let (_, bound) = quadruped_contacts(GaitPattern::bound());
    assert!(bound <= 0.0, "a bound is statically unstable: margin {bound}");
}

#[test]
fn pattern_switch_keeps_every_foot_continuous() {
    let set = GaitPatternSet::new(
        &[
            GaitPattern::quadruped_walk(),
            GaitPattern::trot(),
            GaitPattern::bound(),
        ],
        &[1.0, 2.0],
        0.1,
    )
    .expect("valid set");
    let params = params(set);
    let mut walker = Walker::new();
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
    let mut seen = Vec::new();
    let mut blending = 0;
    // Speed up from a walk through a trot to a bound, hold, then slow back down and stop.
    let profile = |tick: usize| -> f32 {
        match tick {
            0..300 => 0.4 + tick as f32 * 0.009,
            300..420 => 3.1,
            420..720 => 3.1 - (tick - 420) as f32 * 0.009,
            720..800 => 0.4,
            _ => 0.0,
        }
    };
    for tick in 0..1000 {
        let before = state;
        walker.step(&params, &mut state, profile(tick));
        check_step(&params, &before, &state, tick);
        blending += usize::from(state.blending);
        let index = usize::from(state.pattern_index);
        if seen.last() != Some(&index) {
            seen.push(index);
        }
        for i in 0..4 {
            // A foot the pattern has in stance is planted; a swinging one is in the air.
            if !state.blending && state.foot_modes[i] == regia_animation::gait::FootMode::Tripod {
                assert_eq!(
                    state.feet[i].planted,
                    !state.is_swinging(i),
                    "tick {tick} foot {i}"
                );
            }
        }
    }
    assert_eq!(seen, [0, 1, 2, 1, 0], "walk, trot, bound and back");
    assert!(
        blending > 20,
        "switches blend over a cycle: {blending} blending steps"
    );
    assert!(state.settled, "the walk settles after the stop");
    assert!(state.feet().iter().all(|f| f.planted));
    assert_eq!(state.pattern, GaitPattern::quadruped_walk());
}

#[test]
fn two_and_sixteen_legs_work() {
    for pattern in [
        GaitPattern::biped_walk(),
        GaitPattern::wave(16).expect("valid preset"),
        GaitPattern::ripple(16).expect("valid preset"),
    ] {
        let legs = pattern.leg_count();
        let params = params(GaitPatternSet::single(pattern));
        let mut walker = Walker::new();
        let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
        assert_eq!(state.feet().len(), legs);
        let mut lifts = vec![0_usize; legs];
        for tick in 0..1200 {
            let before = state;
            walker.step(&params, &mut state, if tick < 600 { 1.0 } else { 0.0 });
            check_step(&params, &before, &state, tick);
            for ((lift, old), new) in lifts.iter_mut().zip(before.feet()).zip(state.feet()) {
                *lift += usize::from(old.planted && !new.planted);
            }
            assert!(
                state.feet[legs..].iter().all(|f| *f == state.feet[MAX_LEGS - 1]),
                "unused feet stay untouched"
            );
        }
        assert!(lifts.iter().all(|&k| k >= 2), "{legs} legs: {lifts:?}");
        assert!(state.settled, "{legs} legs settle after the stop");
        assert!(state.feet().iter().all(|f| f.planted));
    }
}

#[test]
fn invalid_pattern_is_refused() {
    let mut offsets = [0.0; MAX_LEGS + 1];
    offsets[1] = 0.5;
    assert_eq!(GaitPattern::new(&[], 0.5), Err(GaitError::Pattern));
    assert_eq!(GaitPattern::new(&offsets, 0.5), Err(GaitError::Pattern));
    for bad in [f32::NAN, f32::INFINITY, -0.1, 1.0] {
        assert_eq!(
            GaitPattern::new(&[0.0, bad], 0.5),
            Err(GaitError::Pattern),
            "{bad}"
        );
    }
    for duty in [0.0, 1.0, -0.5, 1.5, f32::NAN] {
        assert_eq!(
            GaitPattern::new(&[0.0, 0.5], duty),
            Err(GaitError::Pattern),
            "{duty}"
        );
    }
    let mut zero = GaitPattern::biped_walk();
    zero.legs = 0;
    assert_eq!(zero.validate(), Err(GaitError::Pattern));
    let mut many = GaitPattern::biped_walk();
    many.legs = MAX_LEGS as u8 + 1;
    assert_eq!(many.validate(), Err(GaitError::Pattern));
    for legs in [0, 3, 18] {
        assert_eq!(GaitPattern::wave(legs), Err(GaitError::Pattern));
        assert_eq!(GaitPattern::ripple(legs), Err(GaitError::Pattern));
    }
    assert_eq!(GaitPattern::ripple(2), Err(GaitError::Pattern));
    // Sets: one leg count, increasing speeds outside the hysteresis, small duty steps.
    let walk = GaitPattern::quadruped_walk();
    let trot = GaitPattern::trot();
    for (patterns, speeds, hysteresis) in [
        (vec![walk, GaitPattern::tripod()], vec![1.0], 0.0),
        (vec![walk, trot], vec![], 0.0),
        (vec![walk, trot, GaitPattern::bound()], vec![2.0, 1.0], 0.0),
        (vec![walk, trot, GaitPattern::bound()], vec![1.0, 1.15], 0.1),
        (vec![walk, trot], vec![f32::NAN], 0.0),
        (vec![walk, trot], vec![1.0], -0.1),
        (vec![], vec![], 0.0),
        (
            vec![
                GaitPattern::new(&[0.0, 0.5, 0.25, 0.75], 0.9).expect("valid pattern"),
                GaitPattern::new(&[0.0, 0.5, 0.5, 0.0], 0.3).expect("valid pattern"),
            ],
            vec![1.0],
            0.0,
        ),
    ] {
        assert_eq!(
            GaitPatternSet::new(&patterns, &speeds, hysteresis),
            Err(GaitError::Pattern),
            "{patterns:?} {speeds:?} {hysteresis}"
        );
    }
    // Settings and stepping refuse a bad pattern atomically, with the documented code.
    let good = params(GaitPatternSet::single(walk));
    let mut bad = good;
    bad.patterns.patterns[0].duty_factor = 1.0;
    assert_eq!(bad.validate(), Err(GaitError::Pattern));
    assert!(GaitError::Pattern.to_string().starts_with("gait_pattern:"));
    assert_eq!(
        GaitState::at_rest(&bad, [0.0; 3], 0.0, flat),
        Err(GaitError::Pattern)
    );
    let original = GaitState::at_rest(&good, [0.0; 3], 0.0, flat).expect("valid gait fixture");
    let mut state = original;
    assert_eq!(
        step_gait(&bad, &mut state, [1.0, 0.0, 0.0], DT, flat),
        Err(GaitError::Pattern)
    );
    assert_eq!(state, original);
    // A state built for four legs refuses six-leg settings.
    let six = tripod_params(GaitPatternSet::tripod());
    assert_eq!(
        step_gait(&six, &mut state, [1.0, 0.0, 0.0], DT, flat),
        Err(GaitError::Pattern)
    );
    assert_eq!(state, original);
}

#[test]
fn stability_margin_matches_a_hand_computed_case() {
    // A 2 m x 1 m rectangle of feet: inside, the margin is the distance to the nearest edge.
    let rectangle = [[0.0, 0.0], [2.0, 0.0], [2.0, 1.0], [0.0, 1.0]];
    assert_eq!(support_margin(&rectangle, [0.5, 0.5]), 0.5);
    assert_eq!(support_margin(&rectangle, [1.0, 0.25]), 0.25);
    // Outside, minus the distance to the polygon (here to the corner [2, 1]: a 3-4-5 case).
    assert_eq!(support_margin(&rectangle, [5.0, 5.0]), -5.0);
    // A triangle (three planted feet of a walk): (0,0), (4,0), (0,3); the incenter (1,1)
    // is 1 m from every edge.
    let triangle = [[0.0, 0.0], [4.0, 0.0], [0.0, 3.0]];
    assert!((support_margin(&triangle, [1.0, 1.0]) - 1.0).abs() < 1e-6);
    // Two feet (a trot diagonal): never positive; on the line it is zero.
    let diagonal = [[-1.0, -1.0], [1.0, 1.0]];
    assert_eq!(support_margin(&diagonal, [0.0, 0.0]), 0.0);
    assert!((support_margin(&diagonal, [1.0, -1.0]) + 2.0_f32.sqrt()).abs() < 1e-6);
    // A quadruped standing on hips at +-0.3 m right, +-0.5 m forward: margin 0.3 m.
    let params = params(GaitPatternSet::single(GaitPattern::trot()));
    let mut quad = params;
    quad.hip_offsets = leg_array([[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]], [0.0; 2]);
    let body = GaitBody {
        position: [2.0, 0.0, -1.0],
        yaw: 0.0,
        velocity: [0.0; 3],
    };
    let state = GaitState::at_rest_driven(&quad, body, flat).expect("valid gait fixture");
    assert!(
        (state.stability_margin - 0.3).abs() < 1e-6,
        "{}",
        state.stability_margin
    );
}

#[test]
fn foot_phase_follows_the_pattern() {
    let pattern = GaitPattern::quadruped_walk();
    let params = params(GaitPatternSet::single(pattern));
    let mut walker = Walker::new();
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
    for tick in 0..400 {
        walker.step(&params, &mut state, 1.2);
        for i in 0..4 {
            let phase = state.foot_phase[i];
            assert!((0.0..1.0).contains(&phase));
            if tick > 60 {
                // Stance below the duty factor, swing above it.
                assert_eq!(
                    state.feet[i].planted,
                    phase < pattern.duty_factor,
                    "tick {tick} foot {i}: phase {phase}"
                );
            }
        }
        // Offsets hold between feet: each foot's phase minus its offset is one cycle clock.
        let clock = |i: usize| (state.foot_phase[i] - pattern.phase_offset[i]).rem_euclid(1.0);
        for i in 1..4 {
            let gap = (clock(i) - clock(0)).abs();
            assert!(gap.min(1.0 - gap) < 1e-4, "tick {tick} foot {i}");
        }
    }
}

#[test]
fn pattern_steps_allocate_nothing() {
    let set = GaitPatternSet::new(
        &[
            GaitPattern::quadruped_walk(),
            GaitPattern::trot(),
            GaitPattern::bound(),
        ],
        &[1.0, 2.0],
        0.1,
    )
    .expect("valid set");
    let params = params(set);
    let mut walker = Walker::new();
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
    walker.step(&params, &mut state, 0.5);
    let ((), counted) = alloc_counter::measure(|| {
        for tick in 0..900 {
            let speed = match tick {
                0..300 => 0.5 + tick as f32 * 0.009,
                300..600 => 3.2 - (tick - 300) as f32 * 0.009,
                _ => 0.0,
            };
            walker.step(&params, &mut state, speed);
        }
    });
    assert_eq!(counted.allocs, 0, "pattern stepping should not allocate");
    assert!(state.settled);
}

/// Timing evidence, not an assertion: nanoseconds per driven step for the tripod, a 16-leg
/// wave and a walk/trot/bound set switching every few strides. Run in release with
/// `cargo test --release -p regia_animation --test gait_patterns -- --ignored --nocapture`.
#[test]
#[ignore = "timing evidence; run by hand in release"]
fn pattern_step_cost_evidence() {
    let switching = GaitPatternSet::new(
        &[
            GaitPattern::quadruped_walk(),
            GaitPattern::trot(),
            GaitPattern::bound(),
        ],
        &[1.0, 2.0],
        0.1,
    )
    .expect("valid set");
    let cases = [
        ("tripod", tripod_params(GaitPatternSet::tripod())),
        (
            "wave16",
            params(GaitPatternSet::single(
                GaitPattern::wave(16).expect("valid preset"),
            )),
        ),
        ("switching4", params(switching)),
    ];
    const STEPS: usize = 100_000;
    for (name, params) in cases {
        let mut walker = Walker::new();
        let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
        let start = std::time::Instant::now();
        for tick in 0..STEPS {
            // A 10 s speed sawtooth between 0.5 and 3 m/s: every switch, both ways.
            let speed = 0.5 + 2.5 * ((tick % 600) as f32 / 600.0);
            walker.step(&params, &mut state, speed);
        }
        let elapsed = start.elapsed();
        println!(
            "gait_step_cost {name}: {:.0} ns/step over {STEPS} steps ({} legs)",
            elapsed.as_nanos() as f64 / STEPS as f64,
            state.leg_count()
        );
        assert!(state.position[2].is_finite());
    }
}

/// ADR-8736 (review F1): equality compares only what the pattern uses. A NaN in an ignored
/// offset slot, or different ignored patterns and switch speeds in a set, compare equal;
/// a used offset, the duty or the leg count still tell patterns apart.
#[test]
fn pattern_equality_ignores_unused_entries() {
    let trot = GaitPattern::trot();
    let mut spoiled = trot;
    spoiled.phase_offset[10] = f32::NAN;
    assert_eq!(spoiled, trot);
    assert_eq!(spoiled, spoiled, "a pattern equals its own copy");
    // Bit for bit: a refused invalid pattern (NaN duty) still equals its unchanged copy, so
    // `assert_eq!(state, before)` can prove atomicity.
    let mut invalid = trot;
    invalid.duty_factor = f32::NAN;
    assert_eq!(invalid, invalid);
    let mut negative_zero = trot;
    assert_eq!(trot.phase_offset[1].to_bits(), 0.0_f32.to_bits());
    negative_zero.phase_offset[1] = -0.0;
    assert_ne!(negative_zero, trot);
    let mut moved = trot;
    moved.phase_offset[3] = 0.25;
    assert_ne!(moved, trot);
    let mut duty = trot;
    duty.duty_factor = 0.6;
    assert_ne!(duty, trot);
    assert_ne!(
        GaitPattern::biped_walk(),
        GaitPattern::new(&[0.0, 0.5, 0.0], 0.6).expect("valid pattern")
    );
    let mut set = GaitPatternSet::single(trot);
    let reference = set;
    set.patterns[2] = GaitPattern::bound();
    set.patterns[1].phase_offset[0] = f32::NAN;
    set.switch_speeds = [f32::NAN; 3];
    assert_eq!(set, reference);
    set.hysteresis = 0.1;
    assert_ne!(set, reference);
    // Presets carry no offset a rounding step under a whole cycle (review N1).
    for preset in [
        GaitPattern::ripple(6).expect("valid preset"),
        GaitPattern::ripple(16).expect("valid preset"),
        GaitPattern::wave(16).expect("valid preset"),
        GaitPattern::bound(),
        GaitPattern::biped_walk(),
    ] {
        let used = &preset.phase_offset[..preset.leg_count()];
        assert!(used.iter().all(|o| *o < 1.0 - 1e-5), "{preset:?}");
    }
    assert_eq!(GaitPattern::ripple(6).expect("valid preset").phase_offset[0], 0.0);
}

/// ADR-8736 (review F2): the reflected pattern a switch blends from is validated like the
/// pattern in effect, so a host or agent write of `blend_from.duty_factor` (a private but
/// reflected field) is refused atomically with `gait_pattern`, never stepped into a frozen
/// switch.
#[test]
fn reflected_blend_patterns_are_validated() {
    use regia_ecs::prelude::bevy_reflect::GetPath;
    let params = params(GaitPatternSet::single(GaitPattern::trot()));
    let mut walker = Walker::new();
    let fresh = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
    for path in ["blend_from.duty_factor", "pattern.duty_factor"] {
        let mut state = fresh;
        *state.path_mut::<f32>(path).expect("reflected path") = 0.0;
        let before = state;
        walker.body.velocity = [0.0, 0.0, -1.2];
        walker.body.position[2] -= 1.2 * DT;
        assert_eq!(
            step_gait_driven(&params, &mut state, walker.body, DT, flat),
            Err(GaitError::Pattern),
            "{path}"
        );
        assert_eq!(state, before, "{path}: refused atomically");
        assert_eq!(
            state.shift_origin([1.0, 0.0, 0.0]),
            Err(GaitError::Pattern),
            "{path}: every entry point validates"
        );
    }
}

/// No ground at all: every foot hangs.
struct NoGround;

impl regia_animation::gait::GaitGround for NoGround {
    fn sample(&self, _: f32, _: f32, _: f32, _: f32) -> Option<regia_animation::gait::GroundHit> {
        None
    }
}

/// ADR-8736 (review F4): with nothing planted (a bound's flight phases, every foot hanging)
/// the reported margin is the finite `NO_SUPPORT_MARGIN`, below every real margin, so it
/// serializes as a number and `lt`/`lte` checks hold; `support_margin` still returns
/// negative infinity for no points.
#[test]
fn no_support_margin_is_finite_and_below_every_margin() {
    use regia_animation::gait::NO_SUPPORT_MARGIN;
    assert!(NO_SUPPORT_MARGIN.is_finite() && NO_SUPPORT_MARGIN < -1e30);
    assert_eq!(support_margin(&[], [0.0, 0.0]), f32::NEG_INFINITY);
    let params = params(GaitPatternSet::single(GaitPattern::bound()));
    let mut walker = Walker::new();
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
    let mut flights = 0;
    for _ in 0..300 {
        walker.step(&params, &mut state, 2.0);
        assert!(state.stability_margin.is_finite());
        if planted_set(&state).is_empty() {
            flights += 1;
            assert_eq!(state.stability_margin, NO_SUPPORT_MARGIN);
        } else {
            assert!(state.stability_margin > NO_SUPPORT_MARGIN);
        }
    }
    assert!(flights > 0, "a bound at 2 m/s has flight phases");
    let hanging = GaitState::at_rest_driven(&params, walker.body, NoGround).expect("valid gait fixture");
    assert!(hanging.feet().iter().all(|f| !f.planted));
    assert_eq!(hanging.stability_margin, NO_SUPPORT_MARGIN);
}

/// ADR-8736 (review F5): walking from rest with overlapping swings (a bound: the hind pair
/// is mid-swing at the fore pair's lift-off), a foot the schedule has in the air lifts on
/// the first step and lands on its scheduled touchdown; its arc starts at its plant (no
/// pop to mid-arc) and contact agrees with `foot_phase` from the first step on.
#[test]
fn walk_from_rest_lifts_feet_the_schedule_has_in_the_air() {
    let pattern = GaitPattern::bound();
    let params = params(GaitPatternSet::single(pattern));
    let speed = 1.0;
    let mut walker = Walker::new();
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
    let rest = state;
    walker.step(&params, &mut state, speed);
    // Hind feet 2 and 3 lift with the fore pair: their swing is 5/6 done at cycle 0.
    for (i, foot) in state.feet().iter().enumerate() {
        assert!(state.is_swinging(i), "foot {i} swings from the first step");
        assert!(!foot.planted, "foot {i}");
    }
    let xz = |a: [f32; 3], b: [f32; 3]| ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    let first = xz(state.feet[2].position, rest.feet[2].position);
    // The rest of the hind swing: (1 - 5/6) of the swing, at cycle = stance / duty.
    let cycle = params.step_length / speed / pattern.duty_factor;
    let remaining = (1.0 - pattern.duty_factor) * cycle * (1.0 - 5.0 / 6.0);
    let mut touchdown = None;
    for tick in 1..(2.0 * cycle / DT) as usize {
        walker.step(&params, &mut state, speed);
        if touchdown.is_none() && state.feet[2].planted {
            touchdown = Some((tick, xz(state.feet[2].position, rest.feet[2].position)));
        }
        for i in 0..4 {
            let phase = state.foot_phase[i];
            if phase < 1.0 - 1e-5 && phase > pattern.duty_factor + speed * DT / params.step_length {
                assert!(
                    !state.feet[i].planted,
                    "tick {tick} foot {i}: planted at phase {phase}"
                );
            }
        }
    }
    let (tick, stride) = touchdown.expect("the hind foot lands");
    assert!(
        ((tick + 1) as f32 * DT - remaining).abs() <= DT * 1.01,
        "landed after {} s, scheduled {remaining} s",
        (tick + 1) as f32 * DT
    );
    assert!(
        stride > 0.0 && first <= 0.25 * stride,
        "first step moved {first} m of {stride} m"
    );
}

/// ADR-8736 (review N3): a finite driven velocity whose ground speed overflows is the
/// host's input (`gait_input`), refused atomically at construction and at every step.
#[test]
fn overflowing_body_speed_is_an_input_error() {
    let params = params(GaitPatternSet::single(GaitPattern::trot()));
    let mut walker = Walker::new();
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
    let mut body = walker.body;
    body.velocity = [0.0, 0.0, -1e20];
    assert_eq!(
        GaitState::at_rest_driven(&params, body, flat),
        Err(GaitError::Input)
    );
    let before = state;
    assert_eq!(
        step_gait_driven(&params, &mut state, body, DT, flat),
        Err(GaitError::Input)
    );
    assert_eq!(state, before);
    walker.step(&params, &mut state, 1.0);
}

/// ADR-8736 (review F3 sweep): a first step from rest that crosses a whole beat (wave(16)
/// has 1/16-cycle beats, shorter than one 60 Hz step at 2 m/s with 0.3 m steps) still
/// steps the starting group: each foot due to lift at rest lifts and lands in that step
/// (or is still in the air), never keeping its plant for another stride. Contact matches
/// `foot_phase` after the step.
#[test]
fn first_step_crossing_a_beat_steps_the_starting_group() {
    let pattern = GaitPattern::wave(16).expect("valid preset");
    let params = GaitParams {
        step_length: 0.3,
        ..params(GaitPatternSet::single(pattern))
    };
    let mut walker = Walker::new();
    let mut state = GaitState::at_rest_driven(&params, walker.body, flat).expect("valid gait fixture");
    let rest = state;
    walker.step(&params, &mut state, 2.0);
    assert!(state.beat != rest.beat, "the step crossed a beat");
    let due: Vec<usize> = (0..16)
        .filter(|&i| (rest.foot_phase[i] - pattern.duty_factor).abs() < 1e-5)
        .collect();
    assert!(!due.is_empty(), "{:?}", rest.foot_phase);
    for i in due {
        let moved = rest.feet[i].position[0].to_bits() != state.feet[i].position[0].to_bits()
            || rest.feet[i].position[2].to_bits() != state.feet[i].position[2].to_bits();
        assert!(moved || state.is_swinging(i), "foot {i} kept its rest plant");
    }
    for (i, foot) in state.feet().iter().enumerate() {
        assert_eq!(
            foot.planted,
            state.foot_phase[i] < pattern.duty_factor,
            "foot {i}: phase {}",
            state.foot_phase[i]
        );
    }
}
