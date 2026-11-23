// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/trace_harness.rs, commit d6ed37677), ADR-8012.
// Mark: adapt for `run_libm_1000`, `pose_vel_hash`, `libm_1000_tick_vector_matches_committed_hash` and
// `libm_wrappers_used_by_hover_basis` (lines 80-163): the loop, the six fields and their hex form are DO27's. Named
// changes: the hover step is `regia_detsim::hover::step`; the hull is the engine's `HoverParams::DEFAULT_HULL`, not
// DO27's measured hull (ADR-8012 A-8, P4; ADR-8164), so the committed `hash=` line is Regia's own vector, and DO27 runs
// the same vector on its own hull in its own repository; the fixture is compiled in with `include_str!` so a
// cross-built test binary (qemu-user, Android, the Mac) needs no file beside it.

//! The libm-1000 cross-OS vector on Regia's hover step (plan row S-07, ADR-8079, ADR-8164).
//!
//! - `libm_1000_tick_vector_matches_committed_hash`: 1000 ticks of full throttle from rest on flat
//!   ground on [`HoverParams::DEFAULT_HULL`]; the final pose and velocity, as the hex of their f32
//!   bits, must equal the committed `hash=` line (`tests/fixtures/hover/libm-1000.txt`). Equal on a
//!   target means that target steps the hover exactly as the x86-64 Linux build does.
//! - `hover_libm_1000_scenario_matches_its_committed_golden`: the same run as an F-02 scenario
//!   (`hover_libm_1000`), checked against its `.detgold` with a hash per tick, so a drift names its
//!   first diverging tick. Prints one `regia-det-golden {json}` line.
//! - `the_scenario_ends_on_the_libm_1000_vector`: the scenario and the vector are the same run.
//! - `integrate_matches_the_libm_1000_loop_bit_for_bit`: the same 1000 ticks with the position moved
//!   by the host kit's `hover::host::integrate` (ADR-8508) end on the committed `hash=` line.
//! - `rate_60_vs_120_on_the_hover_step`: DO27's 60 Hz vs 120 Hz gates on the hover step itself, the
//!   model DO27's `rate_60_vs_120_position_heading_and_flat_shot` was written for (ADR-8046 runs the
//!   same gates on the engine vehicle until this lane).

use regia_detsim::golden::rate_gate::{RateGates, measure};
use regia_detsim::golden::{self, DetScenario, GoldenMode, REPORT_PREFIX};
use regia_detsim::hover::host::{FlatFloor, FloorSampler, HoverBody, integrate};
use regia_detsim::hover::{HoverLibm1000, HoverParams, HoverRateModel, HoverState, step};
use regia_detsim::math;

const LIBM_1000: &str = include_str!("fixtures/hover/libm-1000.txt");
const TRACE_DT: f32 = 1.0 / 60.0;

/// Final pose (x,y,z) and velocity (vx,vy,vz) after 1000 ticks.
/// Pose XZ integrates `velocity * dt`; Y is `floor_y + altitude`.
fn run_libm_1000() -> ([f32; 3], [f32; 3]) {
    let params = HoverParams::DEFAULT_HULL;
    let mut state = HoverState::default();
    let mut pose = [0.0_f32; 3];
    let floor_y = 0.0_f32;
    let input = [1.0_f32, 0.0, 0.0]; // throttle, strafe, steer
    for _ in 0..1000 {
        step(&params, &mut state, &input, floor_y, TRACE_DT);
        // jump stays false — no apply_jump
        pose[0] += state.velocity[0] * TRACE_DT;
        pose[2] += state.velocity[2] * TRACE_DT;
        pose[1] = floor_y + state.altitude;
    }
    (pose, state.velocity)
}

/// Stable fingerprint: eight-digit lowercase hex of each f32 bit pattern,
/// fields `px,py,pz,vx,vy,vz` joined by `_`. Uses libm wrappers only via
/// `step`.
fn pose_vel_hash(pose: [f32; 3], vel: [f32; 3]) -> String {
    format!(
        "{:08x}_{:08x}_{:08x}_{:08x}_{:08x}_{:08x}",
        pose[0].to_bits(),
        pose[1].to_bits(),
        pose[2].to_bits(),
        vel[0].to_bits(),
        vel[1].to_bits(),
        vel[2].to_bits(),
    )
}

fn committed_hash() -> String {
    LIBM_1000
        .lines()
        .find_map(|line| line.strip_prefix("hash=").map(str::to_string))
        .expect("libm-1000.txt must contain hash=...")
}

#[test]
fn libm_1000_tick_vector_matches_committed_hash() {
    let (pose, vel) = run_libm_1000();
    let hash = pose_vel_hash(pose, vel);
    let expected = committed_hash();
    println!(
        "regia-hover-libm-1000 target={} hash={hash} expected={expected}",
        golden::current_target()
    );
    assert_eq!(
        hash, expected,
        "libm-1000 cross-OS vector drifted; pose={pose:?} vel={vel:?}"
    );
}

#[test]
fn integrate_matches_the_libm_1000_loop_bit_for_bit() {
    let params = HoverParams::DEFAULT_HULL;
    let mut body = HoverBody::new([0.0; 3]);
    let floor = FlatFloor(0.0);
    for _ in 0..1000 {
        let floor_y = floor.floor(body.pos[0], body.pos[2]);
        step(&params, &mut body.state, &[1.0, 0.0, 0.0], floor_y, TRACE_DT);
        integrate(&mut body, TRACE_DT, floor_y);
    }
    let hash = pose_vel_hash(body.pos, body.state.velocity);
    assert_eq!(
        hash,
        committed_hash(),
        "the host kit's integrate left the libm-1000 vector"
    );
    // And the hand-written loop agrees word for word.
    let (pose, vel) = run_libm_1000();
    assert_eq!(pose_vel_hash(pose, vel), hash);
}

#[test]
fn libm_wrappers_used_by_hover_basis() {
    // Compile-time coupling: hover facing uses math::sin/cos (see hover.rs).
    let yaw = 1.2_f32;
    let s = math::sin(yaw);
    let c = math::cos(yaw);
    assert_ne!(s.to_bits(), c.to_bits());
}

#[test]
fn the_scenario_ends_on_the_libm_1000_vector() {
    let mut scenario = HoverLibm1000::new();
    let run = scenario.default_run();
    assert_eq!(run.ticks, 1000);
    assert_eq!(run.rate.hz(), 60);
    scenario.reset(&run);
    for tick in 0..run.ticks {
        scenario.step(tick);
    }
    let (pose, vel) = (scenario.pose(), scenario.state().velocity);
    assert_eq!(pose_vel_hash(pose, vel), committed_hash());
}

#[test]
fn hover_libm_1000_scenario_matches_its_committed_golden() {
    let mut scenario = HoverLibm1000::new();
    let report = golden::check_golden(
        &mut scenario,
        &golden::engine_golden_dir(),
        GoldenMode::from_env(),
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");
}

#[test]
fn rate_60_vs_120_on_the_hover_step() {
    let gates = RateGates::default();
    let mut model = HoverRateModel::new(HoverParams::DEFAULT_HULL);
    let report = measure(&mut model, &gates);
    let failures = report.failures(&gates);
    assert!(failures.is_empty(), "{report:?}: {failures:?}");
    // The gates measured a hull that moved and turned.
    let (_, z, _, _) = regia_detsim::golden::rate_gate::integrate_pose(&mut model, 60, 5.0, [1.0, 0.0, 0.0]);
    assert!(z > 50.0, "5 s of full throttle covers ground: {z}");
    let _ = regia_detsim::golden::rate_gate::integrate_pose(&mut model, 60, 2.0, [0.0, 0.0, 1.0]);
    // The yaw wraps to (-pi, pi] (the hull turns more than half a circle), so read the rate.
    assert!(
        model.state.yaw_rate > 2.0,
        "2 s of full steer turns: {}",
        model.state.yaw_rate
    );
}
