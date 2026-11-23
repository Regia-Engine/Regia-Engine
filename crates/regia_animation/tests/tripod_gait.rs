// SPDX-License-Identifier: MPL-2.0
//! Behavior scenarios adapted from Direct Order 27 ir-sim/src/gait.rs,
//! commit d6ed37677 (regia-absorb-1), ADR-8012. Synthetic settings replace game content.

use regia_animation::gait::{
    GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, leg_array, step_gait,
};
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn settings() -> GaitParams {
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
        // Ground settings (ADR-8792). The band and reach are wide enough that no older
        // scenario's height-only floor triggers them, and the ring search is off: these
        // scenarios pin the pre-search behavior (a refused landing holds its lift-off
        // point). The `ground` module's `ground_settings` turns the search on.
        ground_band: 50.0,
        foothold_candidates: 0,
        foothold_spacing: 0.25,
        max_reach: [5.0; MAX_LEGS],
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
        hang_drop: 0.3,
    }
}

#[test]
fn tripod_keeps_three_feet_planted() {
    let params = settings();
    let mut state = GaitState::at_rest(&params, [0.0; 3], 0.0, |_, _| 0.0).expect("valid gait fixture");
    for _ in 0..600 {
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, |_, _| 0.0).expect("valid gait fixture");
        assert!(state.feet().iter().filter(|f| f.planted).count() >= 3);
    }
    assert!(
        state.position[2] > 1.0,
        "the planted-foot check must exercise walking"
    );
}

// Source scenarios adapted to engine-neutral settings; no content tuning is retained.
#[test]
fn cruise_covers_commanded_distance_in_one_second() {
    let params = settings();
    let mut state = rest(&params);
    state.velocity[2] = params.forward_speed;
    for _ in 0..60 {
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, flat).expect("valid gait fixture");
    }
    assert!((state.position[2] - params.forward_speed).abs() < 1e-5);
    assert_eq!(state.position[0], 0.0);
}

fn flat(_: f32, _: f32) -> f32 {
    0.0
}
fn rest(params: &GaitParams) -> GaitState {
    GaitState::at_rest(params, [0.0; 3], 0.0, flat).expect("valid gait fixture")
}

#[test]
fn step_up_changes_foot_height() {
    let params = settings();
    let floor = |x: f32, _: f32| if x >= 1.0 { 0.2 } else { 0.0 };
    let mut state = GaitState::at_rest(&params, [0.0; 3], core::f32::consts::FRAC_PI_2, floor)
        .expect("valid gait fixture");
    state.velocity[0] = params.forward_speed;
    for _ in 0..120 {
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, floor).expect("valid gait fixture");
    }
    assert!(
        state
            .feet()
            .iter()
            .filter(|f| f.planted)
            .all(|f| (f.position[1] - 0.2).abs() < 1e-6)
    );
    assert!((state.position[1] - params.clearance - 0.2).abs() < 1e-6);
}

#[test]
fn power_loss_freezes_pose() {
    let params = settings();
    let mut state = rest(&params);
    state.velocity[2] = params.forward_speed;
    step_gait(&params, &mut state, [1.0, 0.0, 0.0], 0.1, flat).expect("valid gait fixture");
    state.power_lost = true;
    let before = state;
    step_gait(&params, &mut state, [1.0, 0.5, 0.2], 0.1, |_, _| {
        panic!("frozen gait sampled floor")
    })
    .expect("valid gait fixture");
    assert_eq!(state, before);
}

#[test]
fn identical_inputs_are_deterministic() {
    let params = settings();
    let run = || {
        let mut state = rest(&params);
        for i in 0..1200 {
            let input = if i < 400 {
                [1.0, 0.2, -0.4]
            } else if i < 800 {
                [-1.0, -0.3, 0.5]
            } else {
                [0.0; 3]
            };
            step_gait(&params, &mut state, input, 1.0 / 60.0, |x, z| 0.05 * x + 0.03 * z)
                .expect("valid gait fixture");
        }
        state
    };
    assert_eq!(run(), run());
}

#[test]
fn plants_hold_world_xz_and_tripods_alternate() {
    let params = settings();
    let mut state = rest(&params);
    state.velocity[2] = params.forward_speed;
    let mut flips = 0;
    for _ in 0..120 {
        let before = state;
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, flat).expect("valid gait fixture");
        if before.swing_a != state.swing_a {
            flips += 1;
        }
        let planted: Vec<_> = state
            .feet()
            .iter()
            .enumerate()
            .filter(|(_, f)| f.planted)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            planted,
            if state.swing_a {
                vec![1, 2, 5]
            } else {
                vec![0, 3, 4]
            }
        );
        for (old, new) in before.feet().iter().zip(state.feet()) {
            if old.planted && new.planted {
                assert_eq!(old.position[0], new.position[0]);
                assert_eq!(old.position[2], new.position[2]);
            }
        }
    }
    assert!(flips >= 7);
}

#[test]
fn slope_refusal_survives_touchdown() {
    let mut params = settings();
    params.max_slope = 0.01;
    let mut state = rest(&params);
    let initial = state;
    state.velocity[2] = params.forward_speed;
    // Every forward target encounters a cliff, but the original plants stay level.
    let floor = |x: f32, z: f32| {
        if initial
            .feet()
            .iter()
            .any(|f| f.position[0] == x && f.position[2] == z)
        {
            0.0
        } else {
            100.0
        }
    };
    for _ in 0..90 {
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, floor).expect("valid gait fixture");
        assert_eq!(
            state.feet, initial.feet,
            "refused targets must not teleport at period wrap"
        );
    }
}

#[test]
fn idle_follows_a_changing_floor_without_moving_xz() {
    // Replaces `idle_replants_and_follows_changing_floor` (ADR-8539): idle feet no longer
    // snap to neutral every step. They settle by stepping, then only their height follows
    // the floor; XZ holds.
    let params = settings();
    let mut state = rest(&params);
    for _ in 0..60 {
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, flat).expect("valid gait fixture");
    }
    for _ in 0..120 {
        step_gait(&params, &mut state, [0.0; 3], 1.0 / 60.0, |_, _| 0.3).expect("valid gait fixture");
    }
    assert!(state.settled, "two seconds of idle settle the feet");
    assert!(state.feet().iter().all(|f| f.planted && f.position[1] == 0.3));
    assert_eq!(state.phase, 0.0);
    assert_eq!(state.velocity, [0.0; 3]);
    assert_eq!(state.position[1], params.clearance + 0.3);
    let settled = state.feet;
    for (tick, height) in (0..90).map(|t| (t, if t < 45 { 0.5 } else { -0.2 })) {
        step_gait(&params, &mut state, [0.0; 3], 1.0 / 60.0, |_, _| height).expect("valid gait fixture");
        for (old, new) in settled.iter().zip(state.feet()) {
            assert_eq!(
                old.position[0].to_bits(),
                new.position[0].to_bits(),
                "tick {tick}"
            );
            assert_eq!(
                old.position[2].to_bits(),
                new.position[2].to_bits(),
                "tick {tick}"
            );
            assert!(new.planted && new.position[1] == height, "tick {tick}");
        }
        assert_eq!(state.position[1], params.clearance + height);
        assert!(state.settled);
    }
}

#[test]
fn reverse_strafe_steering_and_control_clamping() {
    let params = settings();
    for (input, expected) in [
        ([-1.0, 0.0, 0.0], [0.0, 0.0, -params.reverse_speed]),
        ([0.0, 1.0, 0.0], [params.strafe_speed, 0.0, 0.0]),
    ] {
        let mut state = rest(&params);
        for _ in 0..120 {
            step_gait(&params, &mut state, input, 1.0 / 60.0, flat).expect("valid gait fixture");
        }
        assert_eq!(state.velocity, expected);
    }
    let mut a = rest(&params);
    let mut b = a;
    for _ in 0..600 {
        step_gait(&params, &mut a, [1.0; 3], 1.0 / 60.0, flat).expect("valid gait fixture");
        step_gait(&params, &mut b, [9.0; 3], 1.0 / 60.0, flat).expect("valid gait fixture");
        assert_eq!(a, b);
        assert!(a.yaw > -core::f32::consts::PI && a.yaw <= core::f32::consts::PI);
    }
}

#[test]
fn errors_leave_state_unchanged_and_large_steps_are_bounded() {
    use regia_animation::gait::GaitError;
    let params = settings();
    let original = rest(&params);
    let mut state = original;
    for dt in [f32::NAN, f32::INFINITY] {
        assert_eq!(
            step_gait(&params, &mut state, [0.0; 3], dt, flat),
            Err(GaitError::Input)
        );
        assert_eq!(state, original);
    }
    assert_eq!(
        step_gait(&params, &mut state, [f32::NAN; 3], 0.1, flat),
        Err(GaitError::Input)
    );
    let calls = std::cell::Cell::new(0);
    assert_eq!(
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 0.1, |_, _| {
            calls.set(calls.get() + 1);
            if calls.get() == 4 { f32::NAN } else { 0.0 }
        }),
        Err(GaitError::Floor)
    );
    assert_eq!(calls.get(), 4);
    assert_eq!(state, original);
    assert_eq!(
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 10.0, flat),
        Err(GaitError::StepTooLarge)
    );
    assert_eq!(state, original);
    for dt in [0.0, -1.0] {
        step_gait(&params, &mut state, [f32::NAN; 3], dt, |_, _| {
            panic!("no-op sampled floor")
        })
        .expect("valid gait fixture");
        assert_eq!(state, original);
    }
    let mut invalid = params;
    invalid.step_length = 0.0;
    assert_eq!(
        step_gait(&invalid, &mut state, [1.0; 3], 0.1, flat),
        Err(GaitError::Settings)
    );
    assert_eq!(state, original);
    assert!(GaitState::at_rest(&params, [f32::NAN; 3], 0.0, flat).is_err());
}

#[test]
fn origin_shift_carries_swing_history() {
    let params = settings();
    let mut a = rest(&params);
    for _ in 0..10 {
        step_gait(&params, &mut a, [1.0, 0.0, 0.0], 1.0 / 60.0, flat).expect("valid gait fixture");
    }
    let mut b = a;
    b.shift_origin([2.0, 3.0, 4.0]).expect("valid gait fixture");
    for _ in 0..30 {
        step_gait(&params, &mut a, [1.0, 0.0, 0.0], 1.0 / 60.0, flat).expect("valid gait fixture");
        step_gait(&params, &mut b, [1.0, 0.0, 0.0], 1.0 / 60.0, |_, _| -3.0).expect("valid gait fixture");
        for (fa, fb) in a.feet().iter().zip(b.feet()) {
            for (axis, shift) in [2.0, 3.0, 4.0].into_iter().enumerate() {
                assert!((fa.position[axis] - fb.position[axis] - shift).abs() < 2e-5);
            }
            assert_eq!(fa.planted, fb.planted);
        }
    }
}

#[test]
fn swing_arc_peaks_halfway_through_period() {
    let params = settings();
    let mut state = rest(&params);
    state.velocity[2] = params.forward_speed;
    let dt = params.step_period_at_speed(params.forward_speed) * 0.5;
    step_gait(&params, &mut state, [1.0, 0.0, 0.0], dt, flat).expect("valid gait fixture");
    assert_eq!(state.phase, 0.5);
    for foot in state.feet().iter().filter(|f| !f.planted) {
        assert_eq!(foot.position[1], params.swing_lift);
    }
    assert_eq!(state.position[1], params.clearance);
}

#[test]
fn presentation_trace_pin() {
    // FNV-1a over little-endian f32 bits and contact flags at every step, not a sim checksum.
    let params = settings();
    let mut state = rest(&params);
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
        step_gait(&params, &mut state, controls, 1.0 / 60.0, |x, z| {
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
    assert_eq!(hash, 0x8e1c_5c05_2242_34f3, "presentation trace FNV-1a");
}

#[test]
fn fixed_step_uses_no_heap_allocations() {
    let params = settings();
    let mut state = rest(&params);
    step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, flat).expect("valid gait fixture");
    let ((), counted) = alloc_counter::measure(|| {
        for _ in 0..600 {
            step_gait(&params, &mut state, [1.0, 0.2, 0.4], 1.0 / 60.0, flat).expect("valid gait fixture");
        }
    });
    assert_eq!(counted.allocs, 0, "fixed-array stepping should not allocate");
}

#[test]
fn coincident_valid_landing_still_swings() {
    // A host body correction can align a legal landing with its previous plant.
    // Equality of XZ is not evidence that the slope test refused the footstep.
    let mut params = settings();
    params.forward_speed = 1.0;
    params.step_length = 1.0;
    let mut state = rest(&params);
    state.position[2] = -0.75;
    state.velocity[2] = 1.0;
    step_gait(&params, &mut state, [1.0, 0.0, 0.0], 0.25, flat).expect("valid gait fixture");
    assert_eq!(state.feet().iter().filter(|foot| !foot.planted).count(), 3);
    assert!(state.feet[0].position[1] > 0.0);
}

#[test]
fn heading_rotates_hips_and_forward_motion() {
    let params = settings();
    let mut state = GaitState::at_rest(&params, [0.0; 3], core::f32::consts::FRAC_PI_2, flat)
        .expect("valid gait fixture");
    for (foot, offset) in state.feet().iter().zip(params.hip_offsets) {
        assert!((foot.position[0] - offset[1]).abs() < 1e-6);
        assert!((foot.position[2] + offset[0]).abs() < 1e-6);
    }
    state.velocity = [params.forward_speed, 0.0, 0.0];
    for _ in 0..60 {
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], 1.0 / 60.0, flat).expect("valid gait fixture");
    }
    assert!((state.position[0] - params.forward_speed).abs() < 1e-5);
    assert!(state.position[2].abs() < 1e-5);
}

// Driven mode (G1): the host's simulation owns the body; the stepper places feet only.

mod driven {
    use super::{flat, settings};
    use regia_animation::gait::{
        GaitBody, GaitError, GaitMode, GaitParams, GaitState, step_gait, step_gait_driven,
    };
    use regia_test_utils::alloc_counter;

    const DT: f32 = 1.0 / 60.0;

    fn bumpy(x: f32, z: f32) -> f32 {
        0.04 * (1.3 * x).sin() + 0.03 * (0.9 * z).cos() + 0.01 * x
    }

    /// Hip offsets authored for `step_gait` (+Z forward) in the object convention (-Z forward).
    fn object_convention(params: &GaitParams) -> GaitParams {
        let mut out = *params;
        for offset in &mut out.hip_offsets {
            offset[1] = -offset[1];
        }
        out
    }

    /// World XZ of a hip in the object convention: +X right, -Z forward, yaw about +Y.
    fn object_hip(body: &GaitBody, offset: [f32; 2]) -> [f32; 2] {
        let (sin, cos) = body.yaw.sin_cos();
        [
            body.position[0] + cos * offset[0] - sin * offset[1],
            body.position[2] - sin * offset[0] - cos * offset[1],
        ]
    }

    fn stance_height(params: &GaitParams, state: &GaitState) -> f32 {
        let mut sum = 0.0;
        let mut count = 0.0;
        for foot in state.feet().iter().filter(|f| f.planted) {
            sum += foot.position[1];
            count += 1.0;
        }
        sum / count + params.clearance
    }

    /// A scripted sim mover in the object convention: accelerate, curve, stop, reverse.
    struct Mover {
        body: GaitBody,
        speed: f32,
    }

    impl Mover {
        fn new() -> Self {
            Self {
                body: GaitBody {
                    position: [0.5, 9.0, -0.25],
                    yaw: 0.3,
                    velocity: [0.0; 3],
                },
                speed: 0.0,
            }
        }

        fn tick(&mut self, tick: usize) -> GaitBody {
            let (target, turn) = match tick {
                0..150 => (2.0, 0.0),
                150..300 => (1.6, 0.9),
                300..380 => (0.0, 0.0),
                380..520 => (-1.0, -0.5),
                _ => (0.0, 0.0),
            };
            let step = 3.0 * DT;
            self.speed += (target - self.speed).clamp(-step, step);
            self.body.yaw += turn * DT;
            let (sin, cos) = self.body.yaw.sin_cos();
            // The sim's own vertical motion is carried bit for bit, never used for feet.
            self.body.velocity = [-sin * self.speed, 0.25, -cos * self.speed];
            self.body.position[0] += self.body.velocity[0] * DT;
            self.body.position[1] += self.body.velocity[1] * DT;
            self.body.position[2] += self.body.velocity[2] * DT;
            self.body
        }
    }

    #[test]
    fn driven_body_is_the_host_body_bit_for_bit() {
        let params = object_convention(&settings());
        let mut mover = Mover::new();
        let mut state = GaitState::at_rest_driven(&params, mover.body, bumpy).expect("valid gait fixture");
        assert_eq!(state.mode, GaitMode::Driven);
        let (mut walked, mut flips) = (false, 0);
        for tick in 0..600 {
            let body = mover.tick(tick);
            let swing_before = state.swing_a;
            step_gait_driven(&params, &mut state, body, DT, bumpy).expect("valid gait fixture");
            assert_eq!(
                state.position[0].to_bits(),
                body.position[0].to_bits(),
                "tick {tick}"
            );
            assert_eq!(
                state.position[2].to_bits(),
                body.position[2].to_bits(),
                "tick {tick}"
            );
            assert_eq!(state.yaw.to_bits(), body.yaw.to_bits(), "tick {tick}");
            assert_eq!(
                state.velocity.map(f32::to_bits),
                body.velocity.map(f32::to_bits),
                "tick {tick}"
            );
            assert_eq!(
                state.position[1].to_bits(),
                stance_height(&params, &state).to_bits(),
                "tick {tick}"
            );
            assert_eq!(state.mode, GaitMode::Driven);
            assert_eq!(state.replants, 0, "a continuous body never replants");
            assert!(state.feet().iter().filter(|f| f.planted).count() >= 3);
            walked |= mover.speed.abs() > 1.5;
            flips += usize::from(swing_before != state.swing_a);
        }
        assert!(
            walked && flips >= 10,
            "the script must exercise walking: {flips} flips"
        );
    }

    #[test]
    fn driven_and_integrating_modes_place_the_same_feet() {
        // Both modes rotate by yaw about +Y; they differ in the local forward axis, so the
        // conversion is the hip offsets' forward component. The body is fed as given.
        let params = settings();
        let driven_params = object_convention(&params);
        let mut integrating =
            GaitState::at_rest(&params, [0.2, 0.0, -0.1], 0.4, bumpy).expect("valid gait fixture");
        let body = |s: &GaitState| GaitBody {
            position: s.position,
            yaw: s.yaw,
            velocity: s.velocity,
        };
        let mut driven =
            GaitState::at_rest_driven(&driven_params, body(&integrating), bumpy).expect("valid gait fixture");
        assert_eq!(driven.feet, integrating.feet);
        for tick in 0..720 {
            let controls = match tick / 120 {
                0 => [1.0, 0.0, 0.0],
                1 => [0.7, 0.3, 0.5],
                2 => [-0.8, -0.2, -0.6],
                3 => [0.0, 1.0, 0.2],
                4 => [1.0, 0.0, -1.0],
                _ => [0.0; 3],
            };
            step_gait(&params, &mut integrating, controls, DT, bumpy).expect("valid gait fixture");
            step_gait_driven(&driven_params, &mut driven, body(&integrating), DT, bumpy)
                .expect("valid gait fixture");
            for (a, b) in integrating.feet().iter().zip(driven.feet()) {
                assert_eq!(
                    a.position.map(f32::to_bits),
                    b.position.map(f32::to_bits),
                    "tick {tick}"
                );
                assert_eq!(a.planted, b.planted, "tick {tick}");
            }
            assert_eq!(
                integrating.position.map(f32::to_bits),
                driven.position.map(f32::to_bits)
            );
            assert_eq!(
                (integrating.phase.to_bits(), integrating.swing_a),
                (driven.phase.to_bits(), driven.swing_a)
            );
        }
        assert_eq!(integrating.mode, GaitMode::Integrating);
        assert_eq!((integrating.replants, driven.replants), (0, 0));
    }

    #[test]
    fn planted_feet_hold_world_xz_under_a_driven_body() {
        let params = object_convention(&settings());
        let mut mover = Mover::new();
        let mut state = GaitState::at_rest_driven(&params, mover.body, bumpy).expect("valid gait fixture");
        let mut held = 0;
        for tick in 0..300 {
            let before = state;
            step_gait_driven(&params, &mut state, mover.tick(tick), DT, bumpy).expect("valid gait fixture");
            // Tripod swaps land feet; idle (phase 0) replants at neutral by design.
            if before.swing_a != state.swing_a || before.phase == 0.0 || state.phase == 0.0 {
                continue;
            }
            for (old, new) in before.feet().iter().zip(state.feet()) {
                if old.planted && new.planted {
                    assert_eq!(
                        old.position[0].to_bits(),
                        new.position[0].to_bits(),
                        "tick {tick}"
                    );
                    assert_eq!(
                        old.position[2].to_bits(),
                        new.position[2].to_bits(),
                        "tick {tick}"
                    );
                    assert_eq!(new.position[1], bumpy(new.position[0], new.position[2]));
                    held += 1;
                }
            }
        }
        assert!(held > 600, "planted feet must be checked while walking: {held}");
    }

    #[test]
    fn a_driven_teleport_replants_and_counts() {
        let params = object_convention(&settings());
        let mut mover = Mover::new();
        let mut state = GaitState::at_rest_driven(&params, mover.body, bumpy).expect("valid gait fixture");
        for tick in 0..120 {
            step_gait_driven(&params, &mut state, mover.tick(tick), DT, bumpy).expect("valid gait fixture");
        }
        assert!(
            state.feet().iter().any(|f| !f.planted),
            "teleport must interrupt a swing"
        );
        let mut jumped = mover.body;
        jumped.position[0] += 5.0 * params.step_length;
        step_gait_driven(&params, &mut state, jumped, DT, bumpy).expect("valid gait fixture");
        let neutral = GaitState::at_rest_driven(&params, jumped, bumpy).expect("valid gait fixture");
        assert_eq!(state.feet, neutral.feet, "a teleport replants at neutral");
        assert_eq!(
            state.position.map(f32::to_bits),
            neutral.position.map(f32::to_bits)
        );
        assert_eq!((state.phase, state.swing_a, state.replants), (0.0, true, 1));
        assert_eq!(state.velocity, jumped.velocity);
        mover.body = jumped;
        // Reach bound: a tripod planted at neutral trails its hip by one full period of travel
        // (one step length) before it swings; later plants land half a step ahead and trail
        // half a step. Without the replant the legs would stretch 5 step lengths.
        let reach_limit = params.step_length + 1e-4;
        for tick in 120..360 {
            let body = mover.tick(tick);
            step_gait_driven(&params, &mut state, body, DT, bumpy).expect("valid gait fixture");
            for (foot, offset) in state.feet().iter().zip(params.hip_offsets) {
                let [hx, hz] = object_hip(&body, offset);
                let reach = ((foot.position[0] - hx).powi(2) + (foot.position[2] - hz).powi(2)).sqrt();
                assert!(reach <= reach_limit, "tick {tick}: foot {reach} m from its hip");
            }
        }
        assert_eq!(
            state.replants, 1,
            "continuous motion after the jump does not replant"
        );
        // A jump just under one step length is still continuous motion.
        let mut edge = state;
        let mut body = mover.body;
        body.position[2] += params.step_length * 0.999;
        step_gait_driven(&params, &mut edge, body, DT, bumpy).expect("valid gait fixture");
        assert_eq!(edge.replants, 1);
    }

    #[test]
    fn driven_hips_follow_the_object_convention() {
        // -Z forward, +X right: at zero yaw the front feet are at -Z and the right feet at +X.
        // The fixture's offsets read as [right, forward] in the object convention here.
        let params = settings();
        let body = GaitBody {
            position: [1.0, 0.0, 2.0],
            yaw: 0.0,
            velocity: [0.0; 3],
        };
        let state = GaitState::at_rest_driven(&params, body, flat).expect("valid gait fixture");
        for (foot, expected) in [(1, [1.4, 0.0, 1.5]), (4, [0.6, 0.0, 2.5])] {
            for (got, want) in state.feet[foot].position.into_iter().zip(expected) {
                assert!((got - want).abs() < 1e-6, "foot {foot}: {got} vs {want}");
            }
        }
        let turned = GaitBody {
            yaw: core::f32::consts::FRAC_PI_2,
            ..body
        };
        let state = GaitState::at_rest_driven(&params, turned, flat).expect("valid gait fixture");
        for (foot, offset) in state.feet().iter().zip(params.hip_offsets) {
            let [x, z] = object_hip(&turned, offset);
            assert!((foot.position[0] - x).abs() < 1e-6 && (foot.position[2] - z).abs() < 1e-6);
        }
        // Facing -X after a quarter turn: front feet have the smallest X.
        assert!(state.feet[0].position[0] < state.feet[4].position[0]);
    }

    #[test]
    fn switching_modes_replants_and_records_the_mode() {
        let params = settings();
        let mut state = GaitState::at_rest(&params, [0.0; 3], 0.0, flat).expect("valid gait fixture");
        assert_eq!((state.mode, state.replants), (GaitMode::Integrating, 0));
        let body = GaitBody {
            position: [0.0, 0.0, 0.01],
            yaw: 0.0,
            velocity: [0.0, 0.0, -0.6],
        };
        step_gait_driven(&params, &mut state, body, DT, flat).expect("valid gait fixture");
        let neutral = GaitState::at_rest_driven(&params, body, flat).expect("valid gait fixture");
        assert_eq!((state.mode, state.replants), (GaitMode::Driven, 1));
        assert_eq!(state.feet, neutral.feet);
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], DT, flat).expect("valid gait fixture");
        assert_eq!((state.mode, state.replants), (GaitMode::Integrating, 2));
        step_gait(&params, &mut state, [1.0, 0.0, 0.0], DT, flat).expect("valid gait fixture");
        assert_eq!(state.replants, 2);
    }

    #[test]
    fn driven_errors_leave_state_unchanged() {
        let params = settings();
        let body = GaitBody {
            position: [0.0; 3],
            yaw: 0.0,
            velocity: [0.0, 0.0, -1.0],
        };
        let original = GaitState::at_rest_driven(&params, body, flat).expect("valid gait fixture");
        let mut state = original;
        for bad in [
            GaitBody {
                yaw: f32::NAN,
                ..body
            },
            GaitBody {
                position: [f32::INFINITY, 0.0, 0.0],
                ..body
            },
            GaitBody {
                velocity: [0.0, f32::NAN, 0.0],
                ..body
            },
        ] {
            assert_eq!(
                step_gait_driven(&params, &mut state, bad, DT, flat),
                Err(GaitError::Input)
            );
            assert_eq!(state, original);
        }
        assert_eq!(
            step_gait_driven(&params, &mut state, body, f32::NAN, flat),
            Err(GaitError::Input)
        );
        let fast = GaitBody {
            velocity: [0.0, 0.0, -100.0],
            ..body
        };
        assert_eq!(
            step_gait_driven(&params, &mut state, fast, 1.0, flat),
            Err(GaitError::StepTooLarge)
        );
        assert_eq!(
            step_gait_driven(&params, &mut state, body, DT, |_, _| f32::NAN),
            Err(GaitError::Floor)
        );
        assert_eq!(state, original);
        for dt in [0.0, -1.0] {
            step_gait_driven(
                &params,
                &mut state,
                GaitBody {
                    yaw: f32::NAN,
                    ..body
                },
                dt,
                |_, _| panic!("no-op sampled floor"),
            )
            .expect("valid gait fixture");
            assert_eq!(state, original);
        }
        state.power_lost = true;
        let frozen = state;
        let far = GaitBody {
            position: [50.0, 0.0, 0.0],
            ..body
        };
        step_gait_driven(&params, &mut state, far, DT, |_, _| {
            panic!("frozen gait sampled floor")
        })
        .expect("valid gait fixture");
        assert_eq!(state, frozen);
        assert!(
            GaitState::at_rest_driven(
                &params,
                GaitBody {
                    yaw: f32::INFINITY,
                    ..body
                },
                flat
            )
            .is_err()
        );
    }

    #[test]
    fn driven_step_allocates_nothing() {
        let params = object_convention(&settings());
        let mut mover = Mover::new();
        let mut state = GaitState::at_rest_driven(&params, mover.body, bumpy).expect("valid gait fixture");
        step_gait_driven(&params, &mut state, mover.tick(0), DT, bumpy).expect("valid gait fixture");
        let ((), counted) = alloc_counter::measure(|| {
            for tick in 1..600 {
                step_gait_driven(&params, &mut state, mover.tick(tick), DT, bumpy)
                    .expect("valid gait fixture");
            }
        });
        assert_eq!(counted.allocs, 0, "driven stepping should not allocate");
        assert!(state.position[2] < -1.0, "the counted run must walk");
    }
}

// Idle settling and turning in place (G2, ADR-8539): feet step to neutral instead of snapping.

mod settle {
    use super::{flat, settings};
    use regia_animation::gait::{
        GaitBody, GaitError, GaitParams, GaitState, TRIPOD_LEGS, step_gait, step_gait_driven,
    };
    use regia_test_utils::alloc_counter;

    const DT: f32 = 1.0 / 60.0;

    fn driven_settings() -> GaitParams {
        // The shared fixture's offsets read as [right, forward] in the object convention.
        settings()
    }

    /// World XZ of a hip in the object convention (-Z forward), plain std math.
    fn neutral(body: &GaitBody, offset: [f32; 2]) -> [f32; 2] {
        let (sin, cos) = body.yaw.sin_cos();
        [
            body.position[0] + cos * offset[0] - sin * offset[1],
            body.position[2] - sin * offset[0] - cos * offset[1],
        ]
    }

    fn xz_distance(foot: [f32; 3], point: [f32; 2]) -> f32 {
        ((foot[0] - point[0]).powi(2) + (foot[2] - point[1]).powi(2)).sqrt()
    }

    /// The swing bound, checked for every foot on every step: a foot moves at most
    /// `step_length + swing_lift` (3D) in one step, and a foot planted before and after the
    /// step keeps its world XZ bit for bit (only a swing moves a foot across the ground).
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
            assert!(
                delta <= bound,
                "tick {tick} foot {i}: moved {delta} m in one step"
            );
            if old.planted && new.planted {
                assert_eq!(
                    (old.position[0].to_bits(), old.position[2].to_bits()),
                    (new.position[0].to_bits(), new.position[2].to_bits()),
                    "tick {tick} foot {i}: a planted foot slid or snapped"
                );
            }
        }
        assert!(
            after.feet().iter().filter(|f| f.planted).count() >= 3,
            "tick {tick}: a tripod must stay planted"
        );
    }

    #[test]
    fn no_foot_jumps_more_than_its_swing_bound() {
        let params = settings();
        let mut state = GaitState::at_rest(&params, [0.0; 3], 0.0, flat).expect("valid gait fixture");
        let script: [([f32; 3], usize); 6] = [
            ([1.0, 0.0, 0.0], 120), // walk
            ([0.0; 3], 120),        // stop and wait
            ([1.0, 0.0, 0.0], 90),  // walk again
            ([0.0; 3], 90),         // stop
            ([0.0, 0.0, 1.0], 480), // turn in place: 0.8 rad/s for 8 s, more than a full turn
            ([0.0; 3], 120),        // stop turning
        ];
        let (mut tick, mut turned, mut settled_at_rest) = (0, 0.0_f32, 0);
        for (controls, steps) in script {
            for _ in 0..steps {
                let before = state;
                step_gait(&params, &mut state, controls, DT, flat).expect("valid gait fixture");
                check_step(&params, &before, &state, tick);
                if controls[2] != 0.0 {
                    assert_eq!(state.velocity, [0.0; 3], "tick {tick}: the turn is in place");
                    turned += controls[2] * params.max_yaw_rate * DT;
                }
                settled_at_rest += usize::from(state.settled);
                tick += 1;
            }
        }
        assert!(turned >= core::f32::consts::TAU, "turned {turned} rad");
        assert!(settled_at_rest > 60, "the script must reach rest between moves");
        assert!(state.settled);
    }

    #[test]
    fn no_foot_jumps_when_a_driven_body_stops_dead() {
        let params = driven_settings();
        for stop in 100..=130 {
            let mut body = GaitBody {
                position: [0.0; 3],
                yaw: 0.4,
                velocity: [0.0; 3],
            };
            let mut state = GaitState::at_rest_driven(&params, body, flat).expect("valid gait fixture");
            let (sin, cos) = body.yaw.sin_cos();
            for tick in 0..stop + 90 {
                body.velocity = if tick < stop {
                    [-sin * 2.0, 0.0, -cos * 2.0]
                } else {
                    [0.0; 3]
                };
                body.position[0] += body.velocity[0] * DT;
                body.position[2] += body.velocity[2] * DT;
                let before = state;
                step_gait_driven(&params, &mut state, body, DT, flat).expect("valid gait fixture");
                check_step(&params, &before, &state, tick);
            }
            assert!(state.settled, "stop at tick {stop}");
        }
    }

    /// Walk a driven body forward, stop it dead at `stop`, and return the state on the first
    /// step at which it reports `settled` plus the number of steps after the stop. Every step
    /// after the stop is held to the swing bound (`check_step`): settling steps, never snaps.
    fn settle_after_stop(params: &GaitParams, stop: usize) -> (GaitState, GaitBody, usize) {
        let mut body = GaitBody {
            position: [1.0, 0.0, -2.0],
            yaw: -0.7,
            velocity: [0.0; 3],
        };
        let mut state = GaitState::at_rest_driven(params, body, flat).expect("valid gait fixture");
        let (sin, cos) = body.yaw.sin_cos();
        for tick in 0..stop {
            let speed = (tick as f32 * 0.05).min(2.0);
            body.velocity = [-sin * speed, 0.0, -cos * speed];
            body.position[0] += body.velocity[0] * DT;
            body.position[2] += body.velocity[2] * DT;
            step_gait_driven(params, &mut state, body, DT, flat).expect("valid gait fixture");
        }
        assert!(!state.settled, "stop {stop}: walking is never settled");
        body.velocity = [0.0; 3];
        for after in 1..=600 {
            let before = state;
            step_gait_driven(params, &mut state, body, DT, flat).expect("valid gait fixture");
            check_step(params, &before, &state, stop + after);
            if state.settled {
                return (state, body, after);
            }
        }
        panic!("stop {stop}: never settled");
    }

    #[test]
    fn stop_settles_to_neutral_within_two_periods() {
        let params = driven_settings();
        // Two idle swings at most: the swing in progress finishes (bending its landing to
        // neutral), then the other tripod steps. One extra step absorbs phase rounding.
        let limit = (2.0 * params.settle_period / DT).ceil() as usize + 1;
        for stop in 80..=120 {
            let (state, body, steps) = settle_after_stop(&params, stop);
            assert!(
                steps <= limit,
                "stop {stop}: settled after {steps} steps (limit {limit})"
            );
            assert_eq!(state.phase, 0.0);
            for (i, (foot, offset)) in state.feet().iter().zip(params.hip_offsets).enumerate() {
                let distance = xz_distance(foot.position, neutral(&body, offset));
                assert!(foot.planted, "stop {stop} foot {i}");
                assert!(
                    distance <= params.settle_distance + 1e-5,
                    "stop {stop} foot {i}: {distance} m from neutral"
                );
            }
        }
    }

    #[test]
    fn settled_feet_never_move() {
        let params = driven_settings();
        let (mut state, body, _) = settle_after_stop(&params, 97);
        let settled = state;
        for tick in 0..1000 {
            step_gait_driven(&params, &mut state, body, DT, flat).expect("valid gait fixture");
            assert!(state.settled, "tick {tick}");
            for (old, new) in settled.feet().iter().zip(state.feet()) {
                assert_eq!(
                    old.position.map(f32::to_bits),
                    new.position.map(f32::to_bits),
                    "tick {tick}"
                );
                assert!(new.planted);
            }
        }
        assert_eq!(state, settled, "settled idle is a fixed point");
        // A body creeping below idle_speed by less than settle_distance leaves settled feet
        // where they are (the hips move, the feet do not).
        let mut creeping = body;
        creeping.velocity = [0.0018, 0.0, 0.0];
        for tick in 0..1000 {
            creeping.position[0] += creeping.velocity[0] * DT;
            step_gait_driven(&params, &mut state, creeping, DT, flat).expect("valid gait fixture");
            assert!(state.settled, "creep tick {tick}");
            for (old, new) in settled.feet().iter().zip(state.feet()) {
                assert_eq!(
                    old.position.map(f32::to_bits),
                    new.position.map(f32::to_bits),
                    "creep tick {tick}"
                );
            }
        }
        assert!(creeping.position[0] - body.position[0] > 0.02, "the body crept");
        // The integrating mode settles the same way once its body has braked.
        let mut state = GaitState::at_rest(&params, [0.0; 3], 0.0, flat).expect("valid gait fixture");
        for tick in 0..240 {
            let controls = if tick < 90 { [1.0, 0.0, 0.0] } else { [0.0; 3] };
            step_gait(&params, &mut state, controls, DT, flat).expect("valid gait fixture");
        }
        assert!(state.settled);
        let settled = state;
        for _ in 0..1000 {
            step_gait(&params, &mut state, [0.0; 3], DT, flat).expect("valid gait fixture");
        }
        assert_eq!(state, settled);
    }

    #[test]
    fn turn_in_place_steps_instead_of_sliding() {
        let params = driven_settings();
        let mut body = GaitBody {
            position: [3.0, 0.0, 1.0],
            yaw: 0.2,
            velocity: [0.0; 3],
        };
        let mut state = GaitState::at_rest_driven(&params, body, flat).expect("valid gait fixture");
        let mut lifts = [0_usize; TRIPOD_LEGS];
        let mut held = 0;
        // 0.8 rad/s for 8 s: more than one full turn, then 1 s without turning.
        for tick in 0..540 {
            if tick < 480 {
                body.yaw += params.max_yaw_rate * DT;
            }
            let before = state;
            step_gait_driven(&params, &mut state, body, DT, flat).expect("valid gait fixture");
            check_step(&params, &before, &state, tick);
            for (i, (old, new)) in before.feet().iter().zip(state.feet()).enumerate() {
                lifts[i] += usize::from(old.planted && !new.planted);
                held += usize::from(old.planted && new.planted);
                // No foot is left behind: each stays within one step length of its neutral.
                let reach = xz_distance(new.position, neutral(&body, params.hip_offsets[i]));
                assert!(
                    reach <= params.step_length,
                    "tick {tick} foot {i}: {reach} m from its hip"
                );
            }
        }
        assert!(held > 1500, "planted feet must be checked while turning: {held}");
        assert!(
            lifts.iter().all(|&n| n >= 4),
            "every foot must step while turning: {lifts:?}"
        );
        assert!(state.settled, "the feet settle once the turn stops");
        for (foot, offset) in state.feet().iter().zip(params.hip_offsets) {
            assert!(xz_distance(foot.position, neutral(&body, offset)) <= params.settle_distance + 1e-5);
        }
    }

    #[test]
    fn idle_drift_steps_once_past_the_settle_distance() {
        // A sim body creeping below idle_speed: feet hold until one passes settle_distance.
        let params = driven_settings();
        let mut body = GaitBody {
            position: [0.0; 3],
            yaw: 0.0,
            velocity: [0.01, 0.0, 0.0],
        };
        let mut state = GaitState::at_rest_driven(&params, body, flat).expect("valid gait fixture");
        let start = state.feet;
        let mut first_lift = None;
        for tick in 0..900 {
            body.position[0] += body.velocity[0] * DT;
            let before = state;
            step_gait_driven(&params, &mut state, body, DT, flat).expect("valid gait fixture");
            check_step(&params, &before, &state, tick);
            if first_lift.is_none() && state.feet().iter().any(|f| !f.planted) {
                first_lift = Some(body.position[0]);
            }
        }
        let first_lift = first_lift.expect("the drift must trigger a step");
        assert!(
            (first_lift - params.settle_distance).abs() < 0.01 * DT + 1e-4,
            "stepped at {first_lift} m of drift"
        );
        assert_ne!(state.feet, start, "the feet followed the drift by stepping");
    }

    #[test]
    fn new_settings_are_validated() {
        let params = settings();
        let state = GaitState::at_rest(&params, [0.0; 3], 0.0, flat).expect("valid gait fixture");
        let bad: [fn(&mut GaitParams); 8] = [
            |p| p.settle_distance = -0.1,
            |p| p.settle_distance = f32::NAN,
            |p| p.settle_period = 0.0,
            |p| p.settle_period = f32::INFINITY,
            |p| p.turn_step_distance = -1.0,
            |p| p.turn_step_angle = -0.1,
            |p| p.turn_step_angle = 3.2,
            |p| p.turn_step_angle = f32::NAN,
        ];
        for (n, edit) in bad.into_iter().enumerate() {
            let mut invalid = params;
            edit(&mut invalid);
            assert_eq!(invalid.validate(), Err(GaitError::Settings), "case {n}");
            let mut copy = state;
            assert_eq!(
                step_gait(&invalid, &mut copy, [0.0; 3], DT, flat),
                Err(GaitError::Settings),
                "case {n}"
            );
            assert_eq!(copy, state);
        }
        let mut edge = params;
        edge.settle_distance = 0.0;
        edge.turn_step_distance = 0.0;
        edge.turn_step_angle = core::f32::consts::PI;
        assert_eq!(edge.validate(), Ok(()));
        // An idle swing longer than one settle period per step is refused like a walking one.
        let mut walking = state;
        step_gait(&params, &mut walking, [1.0, 0.0, 0.0], 0.1, flat).expect("valid gait fixture");
        walking.velocity = [0.0; 3];
        let before = walking;
        assert_eq!(
            step_gait(&params, &mut walking, [0.0; 3], 2.0 * params.settle_period, flat),
            Err(GaitError::StepTooLarge)
        );
        assert_eq!(walking, before);
    }

    #[test]
    fn settle_and_turn_allocate_nothing() {
        let params = driven_settings();
        let mut body = GaitBody {
            position: [0.0; 3],
            yaw: 0.0,
            velocity: [0.0; 3],
        };
        let mut state = GaitState::at_rest_driven(&params, body, flat).expect("valid gait fixture");
        step_gait_driven(&params, &mut state, body, DT, flat).expect("valid gait fixture");
        let ((), counted) = alloc_counter::measure(|| {
            for tick in 0..720 {
                let speed = match tick {
                    0..120 => 1.5,
                    _ => 0.0,
                };
                if (240..480).contains(&tick) {
                    body.yaw += params.max_yaw_rate * DT;
                }
                body.velocity = [-body.yaw.sin() * speed, 0.0, -body.yaw.cos() * speed];
                body.position[0] += body.velocity[0] * DT;
                body.position[2] += body.velocity[2] * DT;
                step_gait_driven(&params, &mut state, body, DT, flat).expect("valid gait fixture");
            }
        });
        assert_eq!(
            counted.allocs, 0,
            "settling and turning in place should not allocate"
        );
        assert!(state.settled);
    }
}

// Ground queries (G3, ADR-8792): layers, holes, cliff edges, reach. Host adapters are
// written here over code-built ground; the crate never queries physics or terrain.

mod ground {
    use super::flat;
    use regia_animation::gait::{
        FootMode, GaitBody, GaitError, GaitGround, GaitParams, GaitState, GroundHit, MAX_FOOTHOLD_CANDIDATES,
        MAX_LEGS, step_gait, step_gait_driven,
    };
    use regia_test_utils::alloc_counter;

    const DT: f32 = 1.0 / 60.0;
    const UP: [f32; 3] = [0.0, 1.0, 0.0];

    /// The shared fixture with the ring search on: 16 points, rings 0.25 m apart (0.5 m reach).
    fn settings() -> GaitParams {
        GaitParams {
            foothold_candidates: 16,
            ..super::settings()
        }
    }

    /// Terrain at +6 m over a tube whose floor is at 0 m (its ceiling at 3 m is not ground
    /// for a walker): the layer nearest `expected_y` inside the band answers.
    #[derive(Clone, Copy)]
    struct Tunnel;

    impl GaitGround for Tunnel {
        fn sample(&self, _x: f32, _z: f32, expected_y: f32, band: f32) -> Option<GroundHit> {
            [0.0_f32, 6.0]
                .into_iter()
                .filter(|y| (y - expected_y).abs() <= band)
                .min_by(|a, b| (a - expected_y).abs().total_cmp(&(b - expected_y).abs()))
                .map(|y| GroundHit { y, normal: UP })
        }
    }

    /// Flat ground with a square hole: no ground inside.
    #[derive(Clone, Copy)]
    struct Hole {
        center: [f32; 2],
        half: f32,
    }

    impl Hole {
        fn contains(&self, x: f32, z: f32) -> bool {
            (x - self.center[0]).abs() < self.half && (z - self.center[1]).abs() < self.half
        }
    }

    impl GaitGround for Hole {
        fn sample(&self, x: f32, z: f32, _: f32, _: f32) -> Option<GroundHit> {
            (!self.contains(x, z)).then_some(GroundHit { y: 0.0, normal: UP })
        }
    }

    /// The test adapter over a code-built heightfield: a regular grid, bilinear height,
    /// normal from the cell's gradient, `None` outside the grid (the cliff edge). A host's
    /// terrain adapter has this shape; it borrows the samples, so it is `Copy`.
    #[derive(Clone, Copy)]
    struct Heightfield<'a> {
        origin: [f32; 2],
        cell: f32,
        columns: usize,
        rows: usize,
        heights: &'a [f32],
    }

    impl<'a> Heightfield<'a> {
        fn build(origin: [f32; 2], cell: f32, columns: usize, rows: usize) -> Vec<f32> {
            let mut heights = Vec::with_capacity(columns * rows);
            for row in 0..rows {
                for column in 0..columns {
                    let x = origin[0] + column as f32 * cell;
                    let z = origin[1] + row as f32 * cell;
                    heights.push(0.06 * (0.7 * x).sin() + 0.04 * (0.5 * z).cos());
                }
            }
            heights
        }

        fn new(origin: [f32; 2], cell: f32, columns: usize, heights: &'a [f32]) -> Self {
            Self {
                origin,
                cell,
                columns,
                rows: heights.len() / columns,
                heights,
            }
        }
    }

    impl GaitGround for Heightfield<'_> {
        fn sample(&self, x: f32, z: f32, _: f32, _: f32) -> Option<GroundHit> {
            let u = (x - self.origin[0]) / self.cell;
            let v = (z - self.origin[1]) / self.cell;
            let (last_u, last_v) = ((self.columns - 1) as f32, (self.rows - 1) as f32);
            if !(0.0..=last_u).contains(&u) || !(0.0..=last_v).contains(&v) {
                return None;
            }
            let (c, r) = (
                (u as usize).min(self.columns - 2),
                (v as usize).min(self.rows - 2),
            );
            let (fu, fv) = (u - c as f32, v - r as f32);
            let at = |c: usize, r: usize| self.heights[r * self.columns + c];
            let (h00, h10, h01, h11) = (at(c, r), at(c + 1, r), at(c, r + 1), at(c + 1, r + 1));
            let y = (h00 * (1.0 - fu) + h10 * fu) * (1.0 - fv) + (h01 * (1.0 - fu) + h11 * fu) * fv;
            let dx = ((h10 - h00) * (1.0 - fv) + (h11 - h01) * fv) / self.cell;
            let dz = ((h01 - h00) * (1.0 - fu) + (h11 - h10) * fu) / self.cell;
            Some(GroundHit {
                y,
                normal: [-dx, 1.0, -dz],
            })
        }
    }

    /// Flat ground at 0 with a steep ramp band across the path (z in [-4.5, -3], rising
    /// 2.5 m per meter toward -Z, then a drop back to 0): every landing on it is refused.
    #[derive(Clone, Copy)]
    struct Ramp;

    impl GaitGround for Ramp {
        fn sample(&self, _x: f32, z: f32, _: f32, _: f32) -> Option<GroundHit> {
            if (-4.5..=-3.0).contains(&z) {
                Some(GroundHit {
                    y: 2.5 * (-3.0 - z),
                    normal: [0.0, 1.0, 2.5],
                })
            } else {
                Some(GroundHit { y: 0.0, normal: UP })
            }
        }
    }

    /// World XZ of a hip in the object convention (-Z forward), plain std math.
    fn hip(body: &GaitBody, offset: [f32; 2]) -> [f32; 2] {
        let (sin, cos) = body.yaw.sin_cos();
        [
            body.position[0] + cos * offset[0] - sin * offset[1],
            body.position[2] - sin * offset[0] - cos * offset[1],
        ]
    }

    /// A driven body walking straight along its forward axis at `speed`.
    fn walk(body: &mut GaitBody, speed: f32) {
        let (sin, cos) = body.yaw.sin_cos();
        body.velocity = [-sin * speed, 0.0, -cos * speed];
        body.position[0] += body.velocity[0] * DT;
        body.position[2] += body.velocity[2] * DT;
    }

    #[test]
    fn tunnel_floor_is_chosen_by_expected_height() {
        let mut params = settings();
        params.ground_band = 2.0;
        // In the tube: the body stands clearance above the tube floor.
        let mut body = GaitBody {
            position: [0.0, params.clearance, 0.0],
            yaw: 0.3,
            velocity: [0.0; 3],
        };
        let mut state = GaitState::at_rest_driven(&params, body, Tunnel).expect("valid gait fixture");
        let (mut planted, mut flips) = (0, 0);
        for tick in 0..600 {
            let speed = match tick {
                0..200 => 1.8,
                200..260 => 0.0,
                _ => 1.2,
            };
            if (300..420).contains(&tick) {
                body.yaw += 0.6 * DT;
            }
            walk(&mut body, speed);
            let swing = state.swing_a;
            step_gait_driven(&params, &mut state, body, DT, Tunnel).expect("valid gait fixture");
            flips += usize::from(swing != state.swing_a);
            for (i, foot) in state.feet().iter().enumerate() {
                if foot.planted {
                    assert_eq!(
                        foot.position[1], 0.0,
                        "tick {tick} foot {i}: planted off the tube floor"
                    );
                    planted += 1;
                }
            }
            assert_eq!(state.position[1], params.clearance, "tick {tick}");
            assert_eq!((state.refused, state.forced_steps), (0, 0), "tick {tick}");
        }
        assert!(
            planted >= 1800 && flips > 15,
            "the walk must exercise stepping: {flips} flips"
        );
        // The same ground under a body on the terrain plants at +6 m, and a sim teleport into
        // the tube replants on the tube floor (the replant asks near the host body's Y).
        let mut top = GaitBody {
            position: [20.0, 6.0 + params.clearance, 0.0],
            yaw: 0.0,
            velocity: [0.0; 3],
        };
        let mut state = GaitState::at_rest_driven(&params, top, Tunnel).expect("valid gait fixture");
        assert!(state.feet().iter().all(|f| f.planted && f.position[1] == 6.0));
        top.position = [0.0, params.clearance, 0.0];
        step_gait_driven(&params, &mut state, top, DT, Tunnel).expect("valid gait fixture");
        assert_eq!(state.replants, 1);
        assert!(state.feet().iter().all(|f| f.planted && f.position[1] == 0.0));
    }

    /// Run the integrating walk, recording foot 0 at each of its touchdowns.
    fn touchdowns(params: &GaitParams, ground: impl GaitGround + Copy) -> Vec<(usize, GaitState)> {
        let mut state = GaitState::at_rest(params, [0.0; 3], 0.0, ground).expect("valid gait fixture");
        let mut out = Vec::new();
        for tick in 0..240 {
            let before = state.feet[0].planted;
            step_gait(params, &mut state, [1.0, 0.0, 0.0], DT, ground).expect("valid gait fixture");
            if !before && state.feet[0].planted {
                out.push((tick, state));
            }
        }
        out
    }

    #[test]
    fn a_hole_is_stepped_around() {
        let params = settings();
        let reference = touchdowns(&params, flat);
        let (tick, landed) = reference[2];
        let nominal = landed.feet[0].position;
        let hole = Hole {
            center: [nominal[0], nominal[2]],
            half: 0.2,
        };
        // Same body path, so the same touchdown tick; foot 0 lands on the first valid point
        // of the nearest ring: the axis points at 0.25 m clear the 0.4 m square, the
        // diagonals at 0.177 m per axis do not, and +right (+X at zero yaw) comes first.
        let mut state = GaitState::at_rest(&params, [0.0; 3], 0.0, hole).expect("valid gait fixture");
        for step in 0..240 {
            let before = state.feet[0].planted;
            step_gait(&params, &mut state, [1.0, 0.0, 0.0], DT, hole).expect("valid gait fixture");
            for (i, foot) in state.feet().iter().enumerate() {
                assert!(
                    !(foot.planted && hole.contains(foot.position[0], foot.position[2])),
                    "tick {step} foot {i} planted in the hole"
                );
            }
            if step == tick {
                assert!(
                    !before && state.feet[0].planted,
                    "foot 0 touches down at tick {tick}"
                );
                let got = state.feet[0].position;
                assert!(
                    (got[0] - (nominal[0] + params.foothold_spacing)).abs() < 1e-6,
                    "{got:?} vs {nominal:?}"
                );
                assert!(
                    got[1] == 0.0 && (got[2] - nominal[2]).abs() < 1e-6,
                    "{got:?} vs {nominal:?}"
                );
            }
        }
        assert_eq!(
            (state.refused, state.forced_steps),
            (0, 0),
            "a ring foothold is not a refusal"
        );
        assert!(state.position[2] > 3.0, "the walk must cross the hole");
    }

    #[test]
    fn cliff_edge_foot_hangs_and_reports() {
        let params = settings();
        // Ground only for x <= -0.2: the right feet (+X in the object convention at zero
        // yaw) stand past the edge, beyond the 0.5 m reach of the 16-point ring search.
        let samples = Heightfield::build([-4.0, -8.0], 0.1, 39, 161);
        let field = Heightfield::new([-4.0, -8.0], 0.1, 39, &samples);
        let mut body = GaitBody {
            position: [0.0, params.clearance, 6.0],
            yaw: 0.0,
            velocity: [0.0; 3],
        };
        let mut state = GaitState::at_rest_driven(&params, body, field).expect("no ground is not an error");
        for i in [1, 3, 5] {
            assert_eq!(state.foot_modes[i], FootMode::Hanging, "foot {i}");
            assert!(!state.feet[i].planted, "foot {i}");
            assert_eq!(state.feet[i].normal, [0.0, 1.0, 0.0]);
        }
        assert_eq!(state.refused, 3, "one refusal per hanging foot");
        for i in [0, 2, 4] {
            assert!(
                state.feet[i].planted && state.foot_modes[i] == FootMode::Tripod,
                "foot {i}"
            );
        }
        for tick in 0..400 {
            walk(&mut body, if tick < 300 { 1.5 } else { 0.0 });
            step_gait_driven(&params, &mut state, body, DT, field).expect("no ground is not an error");
            for (i, foot) in state.feet().iter().enumerate() {
                assert!(
                    foot.position.iter().chain(&foot.normal).all(|v| v.is_finite()),
                    "tick {tick} foot {i}"
                );
                if i % 2 == 1 {
                    assert!(!foot.planted, "tick {tick} foot {i} planted past the edge");
                    assert!(
                        foot.position[1] < state.position[1] - params.clearance,
                        "tick {tick} foot {i} hangs below the floor line"
                    );
                }
            }
            assert!(state.position.iter().all(|v| v.is_finite()));
        }
        assert!(
            state.refused > 3 + 10,
            "every touchdown past the edge counts: {}",
            state.refused
        );
        assert_eq!(state.forced_steps, 0);
        assert!(body.position[2] < 0.0, "the walk must travel along the edge");
    }

    #[test]
    fn no_planted_foot_ever_exceeds_reach() {
        let mut params = settings();
        params.max_reach = [1.1; MAX_LEGS];
        let mut body = GaitBody {
            position: [0.0, params.clearance, 0.0],
            yaw: 0.0,
            velocity: [0.0; 3],
        };
        let mut state = GaitState::at_rest_driven(&params, body, Ramp).expect("valid gait fixture");
        let (mut checked, mut thin, mut late) = (0, 0, 0);
        for tick in 0..240 {
            if tick == 180 {
                late = state.forced_steps;
            }
            walk(&mut body, 4.0);
            step_gait_driven(&params, &mut state, body, DT, Ramp).expect("valid gait fixture");
            for (i, foot) in state.feet().iter().enumerate().filter(|(_, f)| f.planted) {
                let [hx, hz] = hip(&body, params.hip_offsets[i]);
                let p = foot.position;
                let reach =
                    ((p[0] - hx).powi(2) + (state.position[1] - p[1]).powi(2) + (p[2] - hz).powi(2)).sqrt();
                assert!(
                    reach <= params.max_reach[i] + 1e-4,
                    "tick {tick} foot {i}: {reach} m from its hip"
                );
                checked += 1;
            }
            // The three-planted invariant drops only while a forced step is in progress or a
            // foot hangs (no valid ground in reach): never on a plain walk.
            let planted = state.feet().iter().filter(|f| f.planted).count();
            if planted < 3 {
                thin += 1;
                assert!(
                    state
                        .foot_modes
                        .iter()
                        .any(|m| matches!(m, FootMode::Forced | FootMode::Hanging)),
                    "tick {tick}: {planted} planted with no forced or hanging foot: {:?}",
                    state.foot_modes
                );
            }
            if body.position[2] > -2.0 {
                assert_eq!(
                    (state.forced_steps, state.refused),
                    (0, 0),
                    "tick {tick}: flat walk"
                );
            }
        }
        assert!(checked > 600, "{checked}");
        assert!(
            state.forced_steps > 0 && state.refused > 0,
            "the ramp must force steps: {} forced, {} refused",
            state.forced_steps,
            state.refused
        );
        assert!(thin > 0, "the scenario must exercise a dropped tripod");
        assert!(
            !state
                .foot_modes
                .iter()
                .any(|m| matches!(m, FootMode::Forced | FootMode::Hanging)),
            "every foot recovers on the flat past the ramp: {:?}",
            state.foot_modes
        );
        assert_eq!(
            state.forced_steps, late,
            "no forced steps on the flat past the ramp"
        );
        assert!(body.position[2] < -12.0);
    }

    /// A struct with exactly a closure's answers (normal +Y, scaled to show normalization).
    #[derive(Clone, Copy)]
    struct Same<F>(F);

    impl<F: Fn(f32, f32) -> f32> GaitGround for Same<F> {
        fn sample(&self, x: f32, z: f32, _: f32, _: f32) -> Option<GroundHit> {
            Some(GroundHit {
                y: (self.0)(x, z),
                normal: [0.0, 2.0, 0.0],
            })
        }
    }

    #[test]
    fn closure_floors_still_work() {
        // Every test above this module passes closures and `fn` items unchanged through the
        // blanket impl; here a closure and a struct ground give bit-identical walks.
        let params = settings();
        let floor = |x: f32, z: f32| 0.02 * x - 0.03 * z;
        let mut a = GaitState::at_rest(&params, [0.0; 3], 0.0, floor).expect("valid gait fixture");
        let mut b = GaitState::at_rest(&params, [0.0; 3], 0.0, Same(floor)).expect("valid gait fixture");
        assert_eq!(a, b);
        for tick in 0..480 {
            let controls = match tick / 120 {
                0 => [1.0, 0.0, 0.0],
                1 => [0.6, 0.2, 0.3],
                2 => [0.0, 0.0, 1.0],
                _ => [0.0; 3],
            };
            step_gait(&params, &mut a, controls, DT, floor).expect("valid gait fixture");
            step_gait(&params, &mut b, controls, DT, Same(floor)).expect("valid gait fixture");
            assert_eq!(a, b, "tick {tick}");
            assert!(a.feet().iter().all(|f| f.normal == UP));
        }
        assert_eq!((a.refused, a.forced_steps), (0, 0));
        assert!(a.foot_modes.iter().all(|m| *m == FootMode::Tripod));
    }

    #[test]
    fn foot_normals_follow_the_ground() {
        let params = settings();
        let samples = Heightfield::build([-16.0, -16.0], 0.25, 129, 129);
        let field = Heightfield::new([-16.0, -16.0], 0.25, 129, &samples);
        let mut state = GaitState::at_rest(&params, [0.0; 3], 0.0, field).expect("valid gait fixture");
        for tick in 0..300 {
            step_gait(&params, &mut state, [1.0, 0.0, 0.2], DT, field).expect("valid gait fixture");
            for (i, foot) in state.feet().iter().enumerate() {
                let length = foot.normal.iter().map(|v| v * v).sum::<f32>().sqrt();
                assert!(
                    (length - 1.0).abs() < 1e-5,
                    "tick {tick} foot {i}: |n| = {length}"
                );
                if foot.planted {
                    let hit = field
                        .sample(foot.position[0], foot.position[2], 0.0, 1.0)
                        .expect("on the field");
                    let n = hit.normal;
                    let scale = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                    for (got, want) in foot.normal.iter().zip(n) {
                        assert!((got - want / scale).abs() < 1e-6, "tick {tick} foot {i}");
                    }
                }
            }
        }
        assert!(
            state.feet().iter().any(|f| f.normal != UP),
            "the field must tilt some feet"
        );
    }

    #[test]
    fn ground_errors_and_settings() {
        #[derive(Clone, Copy)]
        struct Bad(GroundHit);
        impl GaitGround for Bad {
            fn sample(&self, _: f32, _: f32, _: f32, _: f32) -> Option<GroundHit> {
                Some(self.0)
            }
        }
        let params = settings();
        let original = GaitState::at_rest(&params, [0.0; 3], 0.0, flat).expect("valid gait fixture");
        for bad in [
            GroundHit {
                y: f32::NAN,
                normal: UP,
            },
            GroundHit {
                y: 0.0,
                normal: [0.0; 3],
            },
            GroundHit {
                y: 0.0,
                normal: [0.0, f32::INFINITY, 0.0],
            },
            GroundHit {
                y: 0.0,
                normal: [f32::NAN, 1.0, 0.0],
            },
        ] {
            let mut state = original;
            assert_eq!(
                step_gait(&params, &mut state, [1.0, 0.0, 0.0], DT, Bad(bad)),
                Err(GaitError::Floor)
            );
            assert_eq!(state, original, "{bad:?}");
            assert!(GaitState::at_rest(&params, [0.0; 3], 0.0, Bad(bad)).is_err());
        }
        let bad: [fn(&mut GaitParams); 10] = [
            |p| p.ground_band = 0.0,
            |p| p.ground_band = f32::NAN,
            |p| p.foothold_candidates = MAX_FOOTHOLD_CANDIDATES + 1,
            |p| p.foothold_spacing = 0.0,
            |p| p.foothold_spacing = f32::INFINITY,
            |p| p.max_reach[3] = 0.0,
            |p| p.max_reach[0] = f32::NAN,
            |p| p.hang_drop = -0.1,
            |p| p.hang_drop = f32::INFINITY,
            |p| p.ground_band = -1.0,
        ];
        for (n, edit) in bad.into_iter().enumerate() {
            let mut invalid = params;
            edit(&mut invalid);
            assert_eq!(invalid.validate(), Err(GaitError::Settings), "case {n}");
        }
        let mut edge = params;
        edge.ground_band = f32::INFINITY;
        edge.max_reach = [f32::INFINITY; MAX_LEGS];
        edge.foothold_candidates = 0;
        edge.hang_drop = 0.0;
        assert_eq!(edge.validate(), Ok(()));
        edge.foothold_candidates = MAX_FOOTHOLD_CANDIDATES;
        assert_eq!(edge.validate(), Ok(()));
        // A ground with nothing at all: every foot hangs, the body holds its height, no error.
        let nothing = |_: f32, _: f32, _: f32, _: f32| None::<GroundHit>;
        #[derive(Clone, Copy)]
        struct Void<F>(F);
        impl<F: Fn(f32, f32, f32, f32) -> Option<GroundHit>> GaitGround for Void<F> {
            fn sample(&self, x: f32, z: f32, e: f32, b: f32) -> Option<GroundHit> {
                (self.0)(x, z, e, b)
            }
        }
        let mut state = GaitState::at_rest(&params, [0.0, 2.0, 0.0], 0.0, Void(nothing))
            .expect("no ground is not an error");
        assert!(state.feet().iter().all(|f| !f.planted));
        assert_eq!((state.position[1], state.refused), (2.0, 6));
        for _ in 0..120 {
            step_gait(&params, &mut state, [1.0, 0.0, 0.0], DT, Void(nothing))
                .expect("no ground is not an error");
        }
        assert_eq!(state.position[1], 2.0);
        assert!(state.foot_modes[..6].iter().all(|m| *m == FootMode::Hanging));
        let hang = 2.0 - params.clearance - params.hang_drop;
        assert!(
            state
                .feet()
                .iter()
                .all(|f| !f.planted && (f.position[1] - hang).abs() <= params.swing_lift)
        );
    }

    #[test]
    fn ground_walks_are_deterministic() {
        let params = settings();
        let samples = Heightfield::build([-16.0, -16.0], 0.25, 129, 129);
        let field = Heightfield::new([-16.0, -16.0], 0.25, 129, &samples);
        let run = || {
            let mut reach_params = params;
            reach_params.max_reach = [1.1; MAX_LEGS];
            let mut body = GaitBody {
                position: [0.0, 0.7, 0.0],
                yaw: 0.0,
                velocity: [0.0; 3],
            };
            let mut state = GaitState::at_rest_driven(&reach_params, body, Ramp).expect("valid gait fixture");
            let mut other = GaitState::at_rest(&params, [0.0; 3], 0.0, field).expect("valid gait fixture");
            for tick in 0..240 {
                walk(&mut body, 4.0);
                step_gait_driven(&reach_params, &mut state, body, DT, Ramp).expect("valid gait fixture");
                step_gait(
                    &params,
                    &mut other,
                    [1.0, 0.3, if tick < 120 { 0.5 } else { -0.5 }],
                    DT,
                    field,
                )
                .expect("valid gait fixture");
            }
            (state, other)
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn ground_queries_allocate_nothing() {
        let mut params = settings();
        params.max_reach = [1.1; MAX_LEGS];
        let samples = Heightfield::build([-4.0, -8.0], 0.1, 39, 161);
        let field = Heightfield::new([-4.0, -8.0], 0.1, 39, &samples);
        let mut ramp_body = GaitBody {
            position: [0.0, 0.7, 0.0],
            yaw: 0.0,
            velocity: [0.0; 3],
        };
        let mut cliff_body = GaitBody {
            position: [0.0, 0.7, 6.0],
            yaw: 0.0,
            velocity: [0.0; 3],
        };
        let mut tube_body = GaitBody {
            position: [0.0, 0.7, 0.0],
            yaw: 0.4,
            velocity: [0.0; 3],
        };
        let mut ramp = GaitState::at_rest_driven(&params, ramp_body, Ramp).expect("valid gait fixture");
        let mut cliff = GaitState::at_rest_driven(&params, cliff_body, field).expect("valid gait fixture");
        let mut tube = GaitState::at_rest_driven(&params, tube_body, Tunnel).expect("valid gait fixture");
        let mut holed = GaitState::at_rest(&params, [0.0; 3], 0.0, flat).expect("valid gait fixture");
        let hole = Hole {
            center: [-0.4, 1.0],
            half: 0.2,
        };
        let ((), counted) = alloc_counter::measure(|| {
            for tick in 0..240 {
                walk(&mut ramp_body, 4.0);
                walk(&mut cliff_body, if tick < 180 { 1.5 } else { 0.0 });
                walk(&mut tube_body, 1.5);
                step_gait_driven(&params, &mut ramp, ramp_body, DT, Ramp).expect("valid gait fixture");
                step_gait_driven(&params, &mut cliff, cliff_body, DT, field).expect("valid gait fixture");
                step_gait_driven(&params, &mut tube, tube_body, DT, Tunnel).expect("valid gait fixture");
                step_gait(&params, &mut holed, [1.0, 0.0, 0.0], DT, hole).expect("valid gait fixture");
            }
        });
        assert_eq!(
            counted.allocs, 0,
            "ground queries, foothold search and forced steps should not allocate"
        );
        assert!(
            ramp.forced_steps > 0 && cliff.refused > 0,
            "the counted run must search, force and hang"
        );
    }
}
