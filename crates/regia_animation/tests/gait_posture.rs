// SPDX-License-Identifier: MPL-2.0
//! Body posture of the procedural gait (ADR-8706): a body height target from every foot,
//! eased and slew-limited so it stays continuous across lift-off group swaps, and pitch and
//! roll from a least-squares plane through the planted feet. Synthetic settings; no game
//! content.

use regia_animation::gait::{
    GaitBody, GaitError, GaitParams, GaitPattern, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS,
    leg_array, step_gait, step_gait_driven,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

const DT: f32 = 1.0 / 60.0;

/// The six-leg fixture of `tests/tripod_gait.rs` (ADR-8060) with `posture`.
fn params(posture: GaitPosture) -> GaitParams {
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
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [5.0; MAX_LEGS],
        hang_drop: 0.3,
        patterns: GaitPatternSet::tripod(),
        posture,
    }
}

/// Posture on: half-weight feet in the air, 80 ms height ease at most 0.6 m/s, full
/// attitude clamped at 25 degrees pitch and 20 degrees roll, 100 ms attitude ease.
fn following() -> GaitPosture {
    GaitPosture {
        swing_weight: 0.5,
        height_tau: 0.08,
        height_slew: 0.6,
        attitude_weight: 1.0,
        max_pitch: 25.0_f32.to_radians(),
        max_roll: 20.0_f32.to_radians(),
        attitude_tau: 0.1,
    }
}

/// A seeded rubble field: heights in `[-0.15, 0.15]` m on a 0.4 m grid, bilinear between.
#[derive(Clone, Copy)]
struct Rubble {
    seed: u64,
}

impl Rubble {
    fn cell(&self, ix: i64, iz: i64) -> f32 {
        let mut h = self.seed ^ (ix as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^= (iz as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
        h ^= h >> 31;
        h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h ^= h >> 27;
        let unit = (h >> 40) as f32 / (1u64 << 24) as f32;
        (unit * 2.0 - 1.0) * 0.15
    }

    fn height(&self, x: f32, z: f32) -> f32 {
        let (gx, gz) = (x / 0.4, z / 0.4);
        let (fx, fz) = (gx.floor(), gz.floor());
        let (tx, tz) = (gx - fx, gz - fz);
        let (ix, iz) = (fx as i64, fz as i64);
        let near = self.cell(ix, iz) + (self.cell(ix + 1, iz) - self.cell(ix, iz)) * tx;
        let far = self.cell(ix, iz + 1) + (self.cell(ix + 1, iz + 1) - self.cell(ix, iz + 1)) * tx;
        near + (far - near) * tz
    }
}

/// Walk 1200 integrating steps over `ground`, steering gently; returns the largest
/// per-step body height change and the number of lift-off group swaps.
fn rubble_walk(params: &GaitParams, ground: Rubble) -> (f32, u32, GaitState) {
    let height = |x: f32, z: f32| ground.height(x, z);
    let mut state = GaitState::at_rest(params, [0.0, 0.7, 0.0], 0.0, height).expect("valid gait fixture");
    let mut largest = 0.0_f32;
    let mut swaps = 0;
    for tick in 0..1200 {
        let steer = if (tick / 300) % 2 == 0 { 0.25 } else { -0.25 };
        let before = state;
        step_gait(params, &mut state, [1.0, 0.0, steer], DT, height).expect("valid gait fixture");
        largest = largest.max((state.position[1] - before.position[1]).abs());
        swaps += u32::from(state.swing_a != before.swing_a);
        assert!(state.attitude.iter().all(|v| v.is_finite()), "tick {tick}");
    }
    (largest, swaps, state)
}

#[test]
fn body_height_is_continuous_across_group_swaps() {
    let ground = Rubble { seed: 0x5eed_8706 };
    let posture = following();
    let bound = posture.height_slew * DT;
    // The upright stepper (the plain mean of the planted feet) jumps at group swaps here.
    let (upright_jump, upright_swaps, _) = rubble_walk(&params(GaitPosture::UPRIGHT), ground);
    assert!(
        upright_swaps > 50,
        "the walk must swap groups often: {upright_swaps}"
    );
    assert!(
        upright_jump > 2.0 * bound,
        "the scenario must make the upright body jump: {upright_jump} m vs {bound} m"
    );
    let (largest, swaps, state) = rubble_walk(&params(posture), ground);
    assert!(swaps > 50, "{swaps}");
    // One rounding of `start + limit` on top of the slew bound.
    assert!(
        largest <= bound * (1.0 + 1e-5) + 1e-6,
        "body height moved {largest} m in one step (slew bound {bound} m)"
    );
    // It still follows the ground: the target is near.
    assert!(
        (state.position[1] - state.body_height_target).abs() < 0.1,
        "{} vs {}",
        state.position[1],
        state.body_height_target
    );
    assert!(
        state.position[2] > 10.0,
        "the walk must cover rubble: {:?}",
        state.position
    );
}

/// Rotate `v` by the unit quaternion `q = [x, y, z, w]`.
fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let (u, w) = ([q[0], q[1], q[2]], q[3]);
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let t = cross(u, v).map(|c| 2.0 * c);
    let ut = cross(u, t);
    [0, 1, 2].map(|i| v[i] + w * t[i] + ut[i])
}

#[test]
fn body_pitches_with_a_slope_up_to_the_limit() {
    let posture = following();
    let params = params(posture);
    let tolerance = 0.5_f32.to_radians();
    for degrees in [10.0_f32, 40.0] {
        let slope = degrees.to_radians().tan();
        let expect = degrees.to_radians().min(posture.max_pitch);
        // Integrating mode walks +Z uphill.
        let ramp = move |_: f32, z: f32| slope * z;
        let mut state = GaitState::at_rest(&params, [0.0, 0.7, 0.0], 0.0, ramp).expect("valid gait fixture");
        assert!(
            (state.pitch - expect).abs() < tolerance,
            "at rest on {degrees}: {}",
            state.pitch
        );
        for tick in 0..600 {
            step_gait(&params, &mut state, [0.6, 0.0, 0.0], DT, ramp).expect("valid gait fixture");
            assert!(
                (state.pitch - expect).abs() < tolerance,
                "{degrees} degrees, tick {tick}: pitch {} vs {expect}",
                state.pitch.to_degrees()
            );
            assert!(state.roll.abs() < tolerance, "tick {tick}: roll {}", state.roll);
            // Integrating mode: local +Z is forward; the attitude raises it by the pitch.
            let nose = rotate(state.attitude, [0.0, 0.0, 1.0]);
            assert!(
                (nose[1].asin() - state.pitch).abs() < 1e-4,
                "tick {tick}: {nose:?}"
            );
        }
        assert!(
            state.position[2] > 3.0,
            "the walk must climb: {:?}",
            state.position
        );
        // Driven mode walks -Z uphill on the mirrored ramp: same pitch, local -Z forward.
        let ramp = move |_: f32, z: f32| -slope * z;
        let mut body = GaitBody {
            position: [0.0, 0.7, 0.0],
            yaw: 0.0,
            velocity: [0.0, 0.0, -1.0],
        };
        let mut state = GaitState::at_rest_driven(&params, body, ramp).expect("valid gait fixture");
        for tick in 0..600 {
            body.position[2] -= DT;
            step_gait_driven(&params, &mut state, body, DT, ramp).expect("valid gait fixture");
            assert!(
                (state.pitch - expect).abs() < tolerance,
                "driven {degrees}, tick {tick}: pitch {}",
                state.pitch.to_degrees()
            );
            let nose = rotate(state.attitude, [0.0, 0.0, -1.0]);
            assert!(
                (nose[1].asin() - state.pitch).abs() < 1e-4,
                "tick {tick}: {nose:?}"
            );
        }
    }
    // A side slope rising toward local +X rolls the body by the slope (within the limit).
    let side = move |x: f32, _: f32| 10.0_f32.to_radians().tan() * x;
    let mut state = GaitState::at_rest(&params, [0.0, 0.7, 0.0], 0.0, side).expect("valid gait fixture");
    for _ in 0..300 {
        step_gait(&params, &mut state, [0.6, 0.0, 0.0], DT, side).expect("valid gait fixture");
    }
    assert!(
        (state.roll - 10.0_f32.to_radians()).abs() < tolerance,
        "{}",
        state.roll
    );
    assert!(state.pitch.abs() < tolerance, "{}", state.pitch);
    let right = rotate(state.attitude, [1.0, 0.0, 0.0]);
    assert!((right[1].asin() - state.roll).abs() < 1e-4, "{right:?}");
}

#[test]
fn flat_ground_attitude_is_identity_bit_for_bit() {
    const IDENTITY: [u32; 4] = [0, 0, 0, 1.0_f32.to_bits()];
    let bits = |q: [f32; 4]| q.map(f32::to_bits);
    for floor in [0.0_f32, 0.37, -2.5] {
        let level = move |_: f32, _: f32| floor;
        for set in [
            GaitPatternSet::tripod(),
            GaitPatternSet::single(GaitPattern::ripple(6).expect("valid preset")),
        ] {
            let params = GaitParams {
                patterns: set,
                ..params(following())
            };
            let mut state =
                GaitState::at_rest(&params, [0.0, floor + 0.7, 0.0], 0.3, level).expect("valid gait fixture");
            assert_eq!(bits(state.attitude), IDENTITY);
            for tick in 0..720 {
                let controls = match tick / 180 {
                    0 => [1.0, 0.0, 0.0],
                    1 => [0.6, 0.3, 0.5],
                    2 => [-0.5, -0.2, -0.6],
                    _ => [0.0, 0.0, 0.4],
                };
                step_gait(&params, &mut state, controls, DT, level).expect("valid gait fixture");
                assert_eq!(bits(state.attitude), IDENTITY, "floor {floor} tick {tick}");
                assert_eq!(
                    (state.pitch.to_bits(), state.roll.to_bits()),
                    (0, 0),
                    "tick {tick}"
                );
            }
        }
        // Driven, with a trot: the planted pair is collinear, the fallback is level too.
        let params = GaitParams {
            hip_offsets: leg_array([[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]], [0.0; 2]),
            patterns: GaitPatternSet::single(GaitPattern::trot()),
            ..params(following())
        };
        let mut body = GaitBody {
            position: [0.0, floor + 0.7, 0.0],
            yaw: 1.0,
            velocity: [0.0; 3],
        };
        let mut state = GaitState::at_rest_driven(&params, body, level).expect("valid gait fixture");
        for tick in 0..600 {
            body.yaw += 0.004;
            body.velocity = [-1.2 * body.yaw.sin(), 0.0, -1.2 * body.yaw.cos()];
            body.position[0] += body.velocity[0] * DT;
            body.position[2] += body.velocity[2] * DT;
            step_gait_driven(&params, &mut state, body, DT, level).expect("valid gait fixture");
            assert_eq!(bits(state.attitude), IDENTITY, "driven floor {floor} tick {tick}");
        }
    }
}

/// The `presentation_trace_pin` scenario of `tests/tripod_gait.rs`, hashed the same way.
/// Also returns the largest attitude angle seen.
fn trace_hash(params: &GaitParams) -> (u64, f32) {
    let ground = |x: f32, z: f32| 0.02 * x - 0.03 * z;
    let mut state = GaitState::at_rest(params, [0.0; 3], 0.0, |_, _| 0.0).expect("valid gait fixture");
    let mut hash = 0xcbf29ce484222325_u64;
    let mut tilt = 0.0_f32;
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
        step_gait(params, &mut state, controls, 1.0 / 60.0, ground).expect("valid gait fixture");
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
        tilt = tilt.max(state.pitch.abs()).max(state.roll.abs());
    }
    (hash, tilt)
}

#[test]
fn attitude_weight_zero_matches_the_upright_trace() {
    // The pin of `presentation_trace_pin` (ADR-8060, unchanged through ADR-8653).
    const PIN: u64 = 0x8e1c_5c05_2242_34f3;
    assert_eq!(trace_hash(&params(GaitPosture::UPRIGHT)), (PIN, 0.0));
    // Attitude limits and ease set, but weight 0: upright, the same trace.
    let weightless = GaitPosture {
        attitude_weight: 0.0,
        ..GaitPosture {
            swing_weight: 0.0,
            height_tau: 0.0,
            height_slew: f32::INFINITY,
            ..following()
        }
    };
    assert_eq!(trace_hash(&params(weightless)), (PIN, 0.0));
    // Attitude is output only: with full weight the body tilts on the trace's sloped
    // ground, and the body and feet are still the upright trace bit for bit.
    let (hash, tilt) = trace_hash(&params(GaitPosture {
        attitude_weight: 1.0,
        ..weightless
    }));
    assert_eq!(hash, PIN);
    assert!(tilt > 0.01, "the trace ground is sloped: {tilt}");
}

#[test]
fn collinear_feet_fall_back_without_nan() {
    let posture = following();
    let ramp_degrees = 10.0_f32;
    let slope = ramp_degrees.to_radians().tan();
    let ramp = move |x: f32, z: f32| slope * z + 0.05 * x;
    let check = |state: &GaitState, what: &str, tick: usize| {
        let values = state
            .attitude
            .iter()
            .chain([&state.pitch, &state.roll, &state.position[1]]);
        assert!(
            values.copied().all(f32::is_finite),
            "{what} tick {tick}: {state:?}"
        );
        assert!(state.pitch.abs() <= posture.max_pitch && state.roll.abs() <= posture.max_roll);
    };
    let four = leg_array([[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]], [0.0; 2]);
    let cases = [
        // Every planted pair of a trot is one diagonal: collinear every step.
        ("trot", GaitPattern::trot(), four),
        // Two legs: one or two planted feet.
        (
            "biped",
            GaitPattern::biped_walk(),
            leg_array([[-0.2, 0.0], [0.2, 0.0]], [0.0; 2]),
        ),
        // One leg: never more than one point.
        (
            "one leg",
            GaitPattern::new(&[0.0], 0.5).expect("valid pattern"),
            leg_array([[0.0, 0.3]], [0.0; 2]),
        ),
        // Every hip at the body center: all feet at one spot.
        ("one spot", GaitPattern::trot(), [[0.0; 2]; MAX_LEGS]),
    ];
    for (what, pattern, hips) in cases {
        let params = GaitParams {
            hip_offsets: hips,
            max_reach: [f32::INFINITY; MAX_LEGS],
            patterns: GaitPatternSet::single(pattern),
            ..params(posture)
        };
        let mut state = GaitState::at_rest(&params, [0.0, 0.7, 0.0], 0.0, ramp).expect("valid gait fixture");
        check(&state, what, 0);
        for tick in 0..600 {
            let controls = if tick < 400 { [0.7, 0.0, 0.0] } else { [0.0; 3] };
            step_gait(&params, &mut state, controls, DT, ramp).expect("valid gait fixture");
            check(&state, what, tick);
            if what == "trot" && (100..400).contains(&tick) {
                // The planted diagonal is a line; with the feet in the air at their ground
                // line the four feet span the ramp.
                assert!(
                    (state.pitch - ramp_degrees.to_radians()).abs() < 1.0_f32.to_radians(),
                    "trot tick {tick}: pitch {}",
                    state.pitch.to_degrees()
                );
            }
            if what == "one leg" {
                // No plane is ever known: the attitude holds level.
                assert_eq!(state.attitude, [0.0, 0.0, 0.0, 1.0], "{what} tick {tick}");
            }
        }
    }
}

#[test]
fn invalid_posture_is_refused() {
    let good = params(following());
    let mut state = GaitState::at_rest(&good, [0.0, 0.7, 0.0], 0.0, |_, _| 0.0).expect("valid gait fixture");
    let base = following();
    let bad = [
        GaitPosture {
            swing_weight: 1.5,
            ..base
        },
        GaitPosture {
            swing_weight: -0.1,
            ..base
        },
        GaitPosture {
            swing_weight: f32::NAN,
            ..base
        },
        GaitPosture {
            height_tau: -0.1,
            ..base
        },
        GaitPosture {
            height_tau: f32::INFINITY,
            ..base
        },
        GaitPosture {
            height_slew: 0.0,
            ..base
        },
        GaitPosture {
            height_slew: f32::NAN,
            ..base
        },
        GaitPosture {
            attitude_weight: 2.0,
            ..base
        },
        GaitPosture {
            max_pitch: core::f32::consts::FRAC_PI_2,
            ..base
        },
        GaitPosture {
            max_roll: -0.1,
            ..base
        },
        GaitPosture {
            attitude_tau: f32::NAN,
            ..base
        },
    ];
    for posture in bad {
        let params = params(posture);
        assert_eq!(params.validate(), Err(GaitError::Settings), "{posture:?}");
        let before = state;
        assert_eq!(
            step_gait(&params, &mut state, [1.0, 0.0, 0.0], DT, |_, _| 0.0),
            Err(GaitError::Settings)
        );
        assert_eq!(state, before, "a refused step leaves the state unchanged");
    }
    assert!(GaitPosture::UPRIGHT.is_valid() && base.is_valid());
}

#[test]
fn origin_shift_moves_the_height_target() {
    let params = params(following());
    let ground = Rubble { seed: 7 };
    let height = |x: f32, z: f32| ground.height(x, z);
    let mut state = GaitState::at_rest(&params, [0.0, 0.7, 0.0], 0.0, height).expect("valid gait fixture");
    for _ in 0..50 {
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], DT, height).expect("valid gait fixture");
    }
    let before = state;
    state.shift_origin([10.0, 2.0, -4.0]).expect("finite shift");
    assert_eq!(state.body_height_target, before.body_height_target - 2.0);
    assert_eq!(state.position[1], before.position[1] - 2.0);
    assert_eq!(state.attitude, before.attitude);
}

#[test]
fn posture_steps_allocate_nothing() {
    let params = params(following());
    let ground = Rubble { seed: 11 };
    let height = |x: f32, z: f32| ground.height(x, z);
    let mut state = GaitState::at_rest(&params, [0.0, 0.7, 0.0], 0.0, height).expect("valid gait fixture");
    step_gait(&params, &mut state, [1.0, 0.0, 0.0], DT, height).expect("valid gait fixture");
    let mut body = GaitBody {
        position: [0.0, 0.7, 0.0],
        yaw: 0.0,
        velocity: [0.0, 0.0, -1.0],
    };
    let mut driven = GaitState::at_rest_driven(&params, body, height).expect("valid gait fixture");
    let ((), counted) = alloc_counter::measure(|| {
        for tick in 0..900 {
            let controls = if tick < 700 { [1.0, 0.2, 0.3] } else { [0.0; 3] };
            step_gait(&params, &mut state, controls, DT, height).expect("valid gait fixture");
            body.position[2] -= DT;
            step_gait_driven(&params, &mut driven, body, DT, height).expect("valid gait fixture");
        }
    });
    assert_eq!(counted.allocs, 0, "posture stepping should not allocate");
}

/// Timing evidence, not an assertion: nanoseconds per integrating step on rubble with the
/// upright posture and with height and tilt on. Run in release with
/// `cargo test --release -p regia_animation --test gait_posture -- --ignored --nocapture`.
#[test]
#[ignore = "timing evidence; run by hand in release"]
fn posture_step_cost_evidence() {
    const STEPS: usize = 100_000;
    let ground = Rubble { seed: 0x5eed_8706 };
    let height = |x: f32, z: f32| ground.height(x, z);
    for (name, posture) in [("upright", GaitPosture::UPRIGHT), ("posture", following())] {
        let params = params(posture);
        let mut state =
            GaitState::at_rest(&params, [0.0, 0.7, 0.0], 0.0, height).expect("valid gait fixture");
        let start = std::time::Instant::now();
        for tick in 0..STEPS {
            // Walk in a slow circle so the rubble keeps changing under the feet.
            let steer = if (tick / 600) % 2 == 0 { 0.3 } else { -0.3 };
            step_gait(&params, &mut state, [1.0, 0.0, steer], DT, height).expect("valid gait fixture");
        }
        let elapsed = start.elapsed();
        println!(
            "gait_posture_step_cost {name}: {:.0} ns/step over {STEPS} steps",
            elapsed.as_nanos() as f64 / STEPS as f64
        );
        assert!(state.position[1].is_finite());
    }
}

#[test]
fn a_replant_snaps_height_and_tilt() {
    // A slow ease, so only a snap reaches the new ground in one step.
    let posture = GaitPosture {
        height_tau: 2.0,
        attitude_tau: 2.0,
        ..following()
    };
    let params = params(posture);
    let level = |_: f32, _: f32| 0.0;
    let mut body = GaitBody {
        position: [0.0, 0.7, 0.0],
        yaw: 0.0,
        velocity: [0.0; 3],
    };
    let mut state = GaitState::at_rest_driven(&params, body, level).expect("valid gait fixture");
    assert_eq!(state.attitude, [0.0, 0.0, 0.0, 1.0]);
    // Teleport 20 m onto a 10 degree ramp (rising toward -Z, driven forward): one replant.
    let slope = 10.0_f32.to_radians().tan();
    let ramp = move |_: f32, z: f32| -slope * z;
    body.position = [0.0, 20.0 * slope + 0.7, -20.0];
    step_gait_driven(&params, &mut state, body, DT, ramp).expect("valid gait fixture");
    assert_eq!(state.replants, 1);
    assert!(
        (state.pitch - 10.0_f32.to_radians()).abs() < 1e-4,
        "{}",
        state.pitch
    );
    assert_eq!(state.position[1], state.body_height_target);
    assert!((state.body_height_target - (20.0 * slope + 0.7)).abs() < 1e-3);
}

/// Review probe (anim-gait-attitude-review). `expected_floor` is the visual body height minus
/// clearance. On an 8 degree ramp the stance spread fits in a 0.35 m band, so `UPRIGHT`
/// (infinite slew) keeps its feet and its height target on the ramp. The same settings with
/// `height_slew` 0.04 m/s fall behind that band, the feet hang, and the height target freezes.
/// The visual body is allowed to lag; the target and the planted feet are not.
#[test]
fn eased_height_keeps_a_ramp_inside_a_finite_ground_band() {
    let slope = 8.0_f32.to_radians().tan();
    let ramp = move |_: f32, z: f32| -slope * z;
    let speed = 1.2_f32;
    let steps = 480;
    let climb = |posture: GaitPosture| {
        let mut settings = params(posture);
        settings.ground_band = 0.35;
        let mut body = GaitBody {
            position: [0.0, 0.7, 0.0],
            yaw: 0.0,
            velocity: [0.0, 0.0, -speed],
        };
        let mut state = GaitState::at_rest_driven(&settings, body, ramp).expect("valid gait fixture");
        for tick in 0..steps {
            body.position[2] -= speed * DT;
            let result = step_gait_driven(&settings, &mut state, body, DT, ramp);
            assert!(result.is_ok(), "{posture:?} tick {tick}: {result:?}");
        }
        let floor = ramp(body.position[0], body.position[2]);
        let planted = state.feet().iter().filter(|foot| foot.planted).count();
        (
            state.position[1],
            state.body_height_target,
            floor,
            planted,
            state.refused,
            state.position[2],
        )
    };
    // Same as `UPRIGHT` except the slew. The snap is what keeps the band on the ramp.
    let slow = GaitPosture {
        height_slew: 0.04,
        ..GaitPosture::UPRIGHT
    };
    let (up_y, up_target, up_floor, up_planted, up_refused, up_z) = climb(GaitPosture::UPRIGHT);
    assert!(
        up_z < -3.0 && up_planted >= 1,
        "control must climb the ramp with a foot down: y {up_y} target {up_target} floor {up_floor} planted {up_planted} refused {up_refused} z {up_z}"
    );
    assert!(
        (up_y - (up_floor + 0.7)).abs() < 0.2 && (up_target - (up_floor + 0.7)).abs() < 0.2,
        "control body left the ramp: y {up_y} target {up_target} floor {up_floor} planted {up_planted} refused {up_refused}"
    );
    let (y, target, floor, planted, refused, z) = climb(slow);
    assert!(
        planted >= 1 && (target - (floor + 0.7)).abs() < 0.35,
        "slewed body lost the ramp: y {y} target {target} floor {floor} clearance 0.7 planted {planted} refused {refused} z {z}"
    );
}

/// Review probe. `attitude_weight` 0 is documented to keep `attitude` at the identity.
/// The weight is applied to the ease target only, so a tilted pose eases toward upright
/// and is not the identity on the next step.
#[test]
fn weight_zero_does_not_snap_attitude_to_identity() {
    let slope = 20.0_f32.to_radians().tan();
    let ramp = move |_: f32, z: f32| -slope * z;
    let tilted = GaitPosture {
        swing_weight: 0.0,
        height_tau: 0.0,
        height_slew: f32::INFINITY,
        attitude_weight: 1.0,
        max_pitch: 25.0_f32.to_radians(),
        max_roll: 25.0_f32.to_radians(),
        attitude_tau: 2.0,
    };
    let settings = params(tilted);
    let body = GaitBody {
        position: [0.0, 0.7, 0.0],
        yaw: 0.0,
        velocity: [0.0; 3],
    };
    let mut state = GaitState::at_rest_driven(&settings, body, ramp).expect("valid gait fixture");
    assert!(
        (state.pitch - 20.0_f32.to_radians()).abs() < 1.0_f32.to_radians(),
        "setup pitch {}",
        state.pitch
    );

    let upright = GaitPosture {
        attitude_weight: 0.0,
        ..tilted
    };
    let upright = params(upright);
    let result = step_gait_driven(&upright, &mut state, body, DT, ramp);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        state.attitude.map(f32::to_bits),
        [0.0_f32, 0.0, 0.0, 1.0].map(f32::to_bits),
        "attitude_weight 0 left pitch {} roll {} attitude {:?}",
        state.pitch,
        state.roll,
        state.attitude
    );
}

/// Review probe. `max_pitch` and `max_roll` are documented as the largest angles. They clamp
/// the ease target only, so a pitch stored by a scene file or `scene.set_property` stays
/// past the live limit after one step.
#[test]
fn a_lowered_pitch_limit_holds_on_the_next_step() {
    let limited = GaitPosture {
        swing_weight: 0.0,
        height_tau: 0.0,
        height_slew: f32::INFINITY,
        attitude_weight: 1.0,
        max_pitch: 5.0_f32.to_radians(),
        max_roll: 5.0_f32.to_radians(),
        attitude_tau: 2.0,
    };
    let settings = params(limited);
    let body = GaitBody {
        position: [0.0, 0.7, 0.0],
        yaw: 0.0,
        velocity: [0.0; 3],
    };
    let mut state = GaitState::at_rest_driven(&settings, body, |_, _| 0.0).expect("valid gait fixture");
    state.pitch = 1.0;
    state.roll = 0.8;
    let result = step_gait_driven(&settings, &mut state, body, DT, |_, _| 0.0);
    assert!(result.is_ok(), "{result:?}");
    assert!(
        state.pitch.abs() <= limited.max_pitch + 1.0e-4
            && state.roll.abs() <= limited.max_roll + 1.0e-4
            && state.attitude.iter().all(|v| v.is_finite()),
        "pose past the limit: pitch {} roll {} attitude {:?} (limits {}, {})",
        state.pitch,
        state.roll,
        state.attitude,
        limited.max_pitch,
        limited.max_roll
    );
}

/// Review probe. A scene file (reflected `GaitState`) can store a non-finite posture field.
/// The step must not panic and must not leave a NaN or an infinity in the pose.
#[test]
fn non_finite_posture_fields_do_not_stay_in_the_pose() {
    let settings = params(following());
    let body = GaitBody {
        position: [0.0, 0.7, 0.0],
        yaw: 0.0,
        velocity: [0.0; 3],
    };
    let fresh = || GaitState::at_rest_driven(&settings, body, |_, _| 0.0).expect("valid gait fixture");
    let poison = [
        ("pitch NaN", f32::NAN, 0.0, 0.7, [0.0, 0.0, 0.0, 1.0]),
        ("roll inf", 0.0, f32::INFINITY, 0.7, [0.0, 0.0, 0.0, 1.0]),
        ("height NaN", 0.0, 0.0, f32::NAN, [0.0, 0.0, 0.0, 1.0]),
        ("attitude NaN", 0.0, 0.0, 0.7, [f32::NAN, 0.0, 0.0, 1.0]),
    ];
    let mut failed = String::new();
    for (name, pitch, roll, height, attitude) in poison {
        let mut state = fresh();
        state.pitch = pitch;
        state.roll = roll;
        state.body_height_target = height;
        state.attitude = attitude;
        let result = step_gait_driven(&settings, &mut state, body, DT, |_, _| 0.0);
        let finite = state.attitude.iter().copied().all(f32::is_finite)
            && state.pitch.is_finite()
            && state.roll.is_finite()
            && state.body_height_target.is_finite()
            && state.position[1].is_finite();
        if !(result.is_ok() && finite) {
            failed.push_str(&format!(
                "{name}: result {result:?} pitch {} roll {} height {} attitude {:?} body {}\n",
                state.pitch, state.roll, state.body_height_target, state.attitude, state.position[1]
            ));
        }
    }
    assert!(failed.is_empty(), "{failed}");
}

/// A finite zero-length attitude is not a NaN. The next step rebuilds it from pitch and roll.
#[test]
fn zero_length_attitude_is_rebuilt() {
    let settings = params(following());
    let body = GaitBody {
        position: [0.0, 0.7, 0.0],
        yaw: 0.0,
        velocity: [0.0; 3],
    };
    let mut state = GaitState::at_rest_driven(&settings, body, |_, _| 0.0).expect("valid gait fixture");
    state.attitude = [0.0, 0.0, 0.0, 0.0];
    step_gait_driven(&settings, &mut state, body, DT, |_, _| 0.0).expect("valid gait fixture");
    let length = state.attitude.iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!((length - 1.0).abs() < 1.0e-4, "attitude length {length}");
    assert!(state.pitch.is_finite() && state.roll.is_finite() && state.position[1].is_finite());
}

/// Review probe. The same walk at 30, 60 and 144 Hz for eight seconds on a 10 degree ramp.
/// On this fixture the heights stay within about 1.2 cm and the pitch matches, so the bound
/// is 2 cm and 0.01 rad. A larger gap would be a frame-rate bug.
#[test]
fn posture_matches_across_30_60_and_144_hz() {
    let slope = 10.0_f32.to_radians().tan();
    let ramp = move |_: f32, z: f32| -slope * z;
    let posture = GaitPosture {
        height_slew: 4.0,
        ..following()
    };
    let settings = params(posture);
    let run = |hz: f32| {
        let dt = 1.0 / hz;
        let steps = (8.0 * hz).round() as i32;
        let speed = 1.2_f32;
        let mut body = GaitBody {
            position: [0.0, 0.7, 0.0],
            yaw: 0.0,
            velocity: [0.0, 0.0, -speed],
        };
        let mut state = GaitState::at_rest_driven(&settings, body, ramp).expect("valid gait fixture");
        for _ in 0..steps {
            body.position[2] -= speed * dt;
            step_gait_driven(&settings, &mut state, body, dt, ramp).expect("valid gait fixture");
        }
        (state.position[1], state.pitch, state.roll, state.position[2])
    };
    let a = run(30.0);
    let b = run(60.0);
    let c = run(144.0);
    for (name, left, right) in [("30 vs 60", a, b), ("60 vs 144", b, c), ("30 vs 144", a, c)] {
        assert!(
            (left.0 - right.0).abs() < 0.02 && (left.1 - right.1).abs() < 0.01,
            "{name}: height {} vs {} pitch {} vs {} (z {} vs {})",
            left.0,
            right.0,
            left.1,
            right.1,
            left.3,
            right.3
        );
    }
}
