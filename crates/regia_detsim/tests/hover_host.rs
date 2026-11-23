// SPDX-License-Identifier: MPL-2.0
//! The hover host kit (`regia_detsim::hover::host`, ADR-8508): the floor samplers, the ground
//! attitude pass, the fleet and its golden.
//!
//! - `ground_attitude_on_a_plane`: on planes of known slope, at 8 yaws, the four-sample pass reads
//!   the plane's pitch and roll along the hull's axes within 1e-5 rad.
//! - `ground_attitude_is_nan_safe`: a NaN or infinite sample reads the center floor (or 0), bad
//!   footprints read 0, the results are finite and never `-0`.
//! - `a_hull_on_a_side_slope_rolls_with_it`: with the pass, the cushion roll settles at the step's
//!   ground target (`-cushion_terrain_share * reach * ground_roll`; ground rising on the right lifts
//!   the right side, a negative roll); without it the hull stays level.
//! - `fleet_order_is_slot_order`: the inputs' order changes no bit; swapping two slots' hulls and
//!   inputs swaps their results; a duplicate entry reads neutral whatever the order.
//! - `height_grid_*`: the grid's index convention, bilinear blend and edges.
//! - `hover_fleet_terrain_matches_its_committed_golden`: the F-02 golden of the fleet scenario.
//!
//! The allocation test of `HoverFleet::step_all` is `regia_test_utils/tests/hover_fleet_alloc.rs`
//! (it installs the shared counting allocator, ADR-3291).

use regia_detsim::float::det_clamp;
use regia_detsim::golden::{self, GoldenMode, REPORT_PREFIX};
use regia_detsim::hover::host::{
    FlatFloor, FleetHull, FleetInput, FloorFn, FloorSampler, HeightGrid, HeightGridError, HoverBody,
    HoverFleet, ground_attitude,
};
use regia_detsim::hover::{
    ClimbTuning, HoverFleetTerrain, HoverLimits, HoverParams, RollingField, WorldPhysics, cushion_reach,
};
use regia_detsim::math;
use regia_detsim::tick::tick_dt;

const YAWS: [f32; 8] = [0.0, 0.7, 1.5707964, 2.3, 3.1, -0.4, -1.9, -2.8];

#[test]
fn ground_attitude_on_a_plane() {
    // floor = a x + b z + c: along a unit direction (ux, uz) it rises a ux + b uz per metre.
    let planes: [(f32, f32, f32); 5] = [
        (0.0, 0.0, 3.0),
        (0.1, 0.0, 0.0),
        (0.0, -0.25, 7.5),
        (0.3, 0.2, -12.0),
        (-0.6, 0.45, 20.0),
    ];
    for &(a, b, c) in &planes {
        let floor = FloorFn(move |x: f32, z: f32| a * x + b * z + c);
        for &yaw in &YAWS {
            for &pos in &[[0.0, 0.0, 0.0], [13.5, 2.0, -40.25], [-25.0, 0.0, 7.75]] {
                let (pitch, roll) = ground_attitude(&floor, pos, yaw, 2.0, 1.0);
                let (s, co) = (math::sin(yaw), math::cos(yaw));
                let want_pitch = math::atan2(a * s + b * co, 1.0);
                let want_roll = math::atan2(a * co - b * s, 1.0);
                assert!(
                    (pitch - want_pitch).abs() < 1e-5,
                    "plane ({a}, {b}) yaw {yaw} at {pos:?}: pitch {pitch} vs {want_pitch}"
                );
                assert!(
                    (roll - want_roll).abs() < 1e-5,
                    "plane ({a}, {b}) yaw {yaw} at {pos:?}: roll {roll} vs {want_roll}"
                );
            }
        }
    }
    // The signs: ground rising ahead is a positive pitch, rising on the right a positive roll.
    let up_z = FloorFn(|_x: f32, z: f32| 0.2 * z);
    let (pitch, roll) = ground_attitude(&up_z, [0.0; 3], 0.0, 2.0, 1.0);
    assert!(pitch > 0.19 && roll == 0.0, "{pitch} {roll}");
    let up_x = FloorFn(|x: f32, _z: f32| 0.2 * x);
    let (pitch, roll) = ground_attitude(&up_x, [0.0; 3], 0.0, 2.0, 1.0);
    assert!(pitch == 0.0 && roll > 0.19, "{pitch} {roll}");
    // A flat floor reads +0, not -0, at every yaw.
    for &yaw in &YAWS {
        let (pitch, roll) = ground_attitude(&FlatFloor(-3.0), [1.0, 0.0, 1.0], yaw, 2.0, 1.0);
        assert_eq!((pitch.to_bits(), roll.to_bits()), (0, 0), "yaw {yaw}");
    }
}

#[test]
fn ground_attitude_is_nan_safe() {
    // A floor with a hole at x > 1.5 (NaN) and a wall at z > 1.5 (infinity), a slope elsewhere.
    let holes = FloorFn(|x: f32, z: f32| {
        if x > 1.5 {
            f32::NAN
        } else if z > 1.5 {
            f32::INFINITY
        } else {
            0.5 * x + 0.25 * z
        }
    });
    // Yaw 0: nose at z = +2 (infinite, reads the center 0), tail at z = -2 (-0.5): pitch from a
    // half-length difference. Right at x = +1 (0.5), left at x = -1 (-0.5): the plane's roll.
    let (pitch, roll) = ground_attitude(&holes, [0.0; 3], 0.0, 2.0, 1.0);
    assert_eq!(pitch, math::atan2(0.5, 4.0));
    assert_eq!(roll, math::atan2(1.0, 2.0));
    // Right sample in the NaN hole (x = 2): reads the center 0.
    let (_, roll) = ground_attitude(&holes, [0.0; 3], 0.0, 2.0, 2.0);
    assert_eq!(roll, math::atan2(1.0, 4.0));
    // A floor that is NaN everywhere: center unusable too, every sample reads 0.
    let nan = FlatFloor(f32::NAN);
    for &yaw in &YAWS {
        let (pitch, roll) = ground_attitude(&nan, [5.0, 0.0, 5.0], yaw, 2.0, 1.0);
        assert_eq!((pitch.to_bits(), roll.to_bits()), (0, 0));
    }
    // Bad footprints and positions.
    let slope = FloorFn(|x: f32, z: f32| 0.3 * x + 0.1 * z);
    for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        let (pitch, roll) = ground_attitude(&slope, [0.0; 3], 0.3, bad, bad);
        assert_eq!((pitch.to_bits(), roll.to_bits()), (0, 0), "footprint {bad}");
    }
    for pos in [[f32::NAN, 0.0, 0.0], [0.0, 0.0, f32::INFINITY]] {
        let (pitch, roll) = ground_attitude(&slope, pos, 0.3, 2.0, 1.0);
        assert!(pitch.is_finite() && roll.is_finite(), "{pos:?}: {pitch} {roll}");
    }
    let (pitch, roll) = ground_attitude(&slope, [0.0; 3], f32::NAN, 2.0, 1.0);
    assert!(pitch.is_finite() && roll.is_finite());
    // A cliff (finite but huge) gives a bounded angle.
    let cliff = FloorFn(|x: f32, _z: f32| if x > 0.0 { 3.0e38 } else { -3.0e38 });
    let (_, roll) = ground_attitude(&cliff, [0.0; 3], 0.0, 2.0, 1.0);
    assert_eq!(roll, math::atan2(f32::INFINITY, 2.0));
}

/// Settle one default hull at rest on `floor` for `ticks`, with or without the attitude pass.
fn settle_on(floor: &dyn FloorSampler, with_pass: bool, ticks: u32) -> HoverBody {
    let params = HoverParams::DEFAULT_HULL;
    let mut body = HoverBody::at_rest([0.0, floor.floor(0.0, 0.0), 0.0], 0.0, &params);
    for _ in 0..ticks {
        if with_pass {
            let (pitch, roll) = ground_attitude(floor, body.pos, body.state.yaw, 2.0, 1.0);
            body.state.ground_pitch = pitch;
            body.state.ground_roll = roll;
        }
        let f = floor.floor(body.pos[0], body.pos[2]);
        let _ = regia_detsim::hover::step_with(
            &params,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            &WorldPhysics::EARTH,
            &mut body.state,
            &[0.0; 3],
            f,
            tick_dt(),
        );
        regia_detsim::hover::host::integrate(&mut body, tick_dt(), f);
    }
    body
}

#[test]
fn a_hull_on_a_side_slope_rolls_with_it() {
    let params = HoverParams::DEFAULT_HULL;
    // Ground rising 0.2 per metre toward +X: the hull's right at yaw 0.
    let side = FloorFn(|x: f32, _z: f32| 0.2 * x);
    let with = settle_on(&side, true, 600);
    let ground_roll = math::atan2(0.2, 1.0);
    assert!((with.state.ground_roll - ground_roll).abs() < 1e-6);
    let reach = cushion_reach(&params, &with.state);
    assert!(reach > 0.99, "the hull rides on its cushion: reach {reach}");
    let want = det_clamp(
        -params.cushion_terrain_share * reach * with.state.ground_roll,
        -HoverLimits::DEFAULT.roll_lean_max,
        HoverLimits::DEFAULT.roll_lean_max,
    );
    assert!(want < -0.05, "a visible tilt: {want}");
    assert!(
        (with.state.roll - want).abs() < 1e-4,
        "the cushion roll converges to share * reach * ground roll: {} vs {want}",
        with.state.roll
    );
    assert!(with.state.roll_rate.abs() < 1e-4);
    // The same slope along the hull (yaw 0, ground rising ahead): the pitch follows instead.
    let ahead = FloorFn(|_x: f32, z: f32| 0.2 * z);
    let pitched = settle_on(&ahead, true, 600);
    assert!(pitched.state.ground_pitch > 0.19);
    assert!(
        pitched.state.cushion_pitch > 0.05,
        "{}",
        pitched.state.cushion_pitch
    );
    assert!(pitched.state.roll.abs() < 1e-6);
    // Without the pass the hull never tilts with the ground.
    let without = settle_on(&side, false, 600);
    assert_eq!(without.state.roll, 0.0);
    assert_eq!(without.state.ground_roll, 0.0);
    let without = settle_on(&ahead, false, 600);
    assert_eq!(without.state.cushion_pitch, 0.0);
}

fn test_fleet(n: u32) -> HoverFleet {
    let mut fleet = HoverFleet::new(HoverFleetTerrain::hull_types().to_vec());
    for i in 0..n {
        let x = (i % 4) as f32 * 9.0;
        let z = (i / 4) as f32 * 9.0;
        let hull = i % 2;
        let params = fleet.hulls()[hull as usize].params;
        fleet.push(
            HoverBody::at_rest([x, RollingField.floor(x, z), z], i as f32 * 0.5, &params),
            hull,
        );
    }
    fleet
}

fn axes_for(slot: u32, tick: u32) -> [f32; 3] {
    let k = (slot * 7 + tick / 30) % 11;
    [
        k as f32 / 10.0,
        (k as f32 - 5.0) / 10.0,
        ((slot + tick / 45) % 5) as f32 / 2.0 - 1.0,
    ]
}

fn bits(fleet: &HoverFleet, slot: u32) -> Vec<u32> {
    let b = fleet.body(slot);
    let s = &b.state;
    [
        b.pos[0],
        b.pos[1],
        b.pos[2],
        s.velocity[0],
        s.velocity[2],
        s.yaw,
        s.altitude,
        s.roll,
        s.cushion_pitch,
        s.ground_pitch,
        s.ground_roll,
    ]
    .iter()
    .map(|v| v.to_bits())
    .collect()
}

fn run_fleet(
    fleet: &mut HoverFleet,
    ticks: u32,
    order: impl Fn(u32, &mut Vec<FleetInput>),
    slot_map: impl Fn(u32) -> u32,
) {
    let mut inputs = Vec::new();
    for tick in 0..ticks {
        inputs.clear();
        order(tick, &mut inputs);
        // The axes follow the hull: slot `s` of this fleet carries the hull `slot_map(s)` of the reference.
        for e in &mut inputs {
            e.axes = axes_for(slot_map(e.slot), tick);
        }
        let report = fleet.step_all(
            &inputs,
            &RollingField,
            &WorldPhysics::EARTH,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            tick_dt(),
        );
        assert_eq!(report.stepped, fleet.len() as u32);
        assert_eq!(
            (report.unknown_slots, report.duplicate_slots, report.recovered),
            (0, 0, 0)
        );
    }
}

#[test]
fn fleet_order_is_slot_order() {
    const N: u32 = 12;
    let ascending = |_t: u32, v: &mut Vec<FleetInput>| v.extend((0..N).map(|s| FleetInput::new(s, [0.0; 3])));
    let shuffled = |t: u32, v: &mut Vec<FleetInput>| {
        // A different permutation every tick: stride 5 (coprime with 12) from a moving start.
        v.extend((0..N).map(|i| FleetInput::new((t + i * 5) % N, [0.0; 3])));
    };
    let mut a = test_fleet(N);
    run_fleet(&mut a, 400, ascending, |s| s);
    let mut b = test_fleet(N);
    run_fleet(&mut b, 400, shuffled, |s| s);
    for slot in 0..N {
        assert_eq!(
            bits(&a, slot),
            bits(&b, slot),
            "slot {slot}: the input order moved a bit"
        );
    }
    // The hulls moved and tilted.
    assert!(a.body(3).pos[2] != test_fleet(N).body(3).pos[2]);
    assert!(a.bodies().iter().any(|b| b.state.ground_roll.abs() > 0.01));

    // Swap the hulls of slots 2 and 7 (each keeps its hull type) and their inputs: their results swap.
    let mut c = HoverFleet::new(HoverFleetTerrain::hull_types().to_vec());
    let reference = test_fleet(N);
    let swap = |s: u32| match s {
        2 => 7,
        7 => 2,
        s => s,
    };
    for slot in 0..N {
        let from = swap(slot);
        c.push(*reference.body(from), reference.hull_of(from).unwrap_or(0));
    }
    run_fleet(&mut c, 400, ascending, swap);
    for slot in 0..N {
        assert_eq!(bits(&c, slot), bits(&a, swap(slot)), "slot {slot}");
    }

    // A duplicate slot reads neutral input whatever the order; an unknown slot is ignored.
    let mut d1 = test_fleet(4);
    let mut d2 = test_fleet(4);
    let mut neutral = test_fleet(4);
    let dup_a = [
        FleetInput::new(1, [1.0, 0.0, 0.5]),
        FleetInput::new(1, [-1.0, 0.3, 0.0]),
        FleetInput::new(9, [1.0, 1.0, 1.0]),
    ];
    let dup_b = [dup_a[2], dup_a[1], dup_a[0]];
    for _ in 0..120 {
        let ra = d1.step_all(
            &dup_a,
            &RollingField,
            &WorldPhysics::EARTH,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            tick_dt(),
        );
        let _ = d2.step_all(
            &dup_b,
            &RollingField,
            &WorldPhysics::EARTH,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            tick_dt(),
        );
        let _ = neutral.step_all(
            &[],
            &RollingField,
            &WorldPhysics::EARTH,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            tick_dt(),
        );
        assert_eq!((ra.duplicate_slots, ra.unknown_slots), (1, 1));
    }
    for slot in 0..4 {
        assert_eq!(bits(&d1, slot), bits(&d2, slot));
        assert_eq!(bits(&d1, slot), bits(&neutral, slot));
    }
    // A bad dt steps nothing.
    let before = bits(&d1, 0);
    let r = d1.step_all(
        &dup_a,
        &RollingField,
        &WorldPhysics::EARTH,
        &HoverLimits::DEFAULT,
        &ClimbTuning::AIR_TIME,
        f32::NAN,
    );
    assert!(r.skipped && r.stepped == 0);
    assert_eq!(bits(&d1, 0), before);
}

#[test]
fn height_grid_index_convention_and_bilinear() {
    // 3 columns along +X, 2 rows along +Z, 2 m cells, post (0, 0) at (10, -4).
    let heights = vec![
        0.0, 1.0, 4.0, // row 0 (z = -4): x = 10, 12, 14
        2.0, 3.0, 8.0, // row 1 (z = -2)
    ];
    let grid = HeightGrid::new([10.0, -4.0], 2.0, 3, 2, heights).expect("a valid grid");
    assert_eq!(grid.post(2, 0), Some(4.0));
    assert_eq!(grid.post(0, 1), Some(2.0));
    assert_eq!(grid.post(3, 0), None);
    // Posts read back bit for bit.
    assert_eq!(grid.floor(10.0, -4.0), 0.0);
    assert_eq!(grid.floor(14.0, -4.0), 4.0);
    assert_eq!(grid.floor(12.0, -2.0), 3.0);
    assert_eq!(grid.floor(14.0, -2.0), 8.0);
    // Mid-cell: the four posts' mean.
    assert_eq!(grid.floor(11.0, -3.0), (0.0 + 1.0 + 2.0 + 3.0) / 4.0);
    assert_eq!(grid.floor(13.0, -3.0), (1.0 + 4.0 + 3.0 + 8.0) / 4.0);
    // Off the grid: the nearest edge.
    assert_eq!(grid.floor(-100.0, -100.0), 0.0);
    assert_eq!(grid.floor(1.0e9, 1.0e9), 8.0);
    assert_eq!(grid.floor(f32::NAN, f32::NAN), 0.0);
    assert_eq!(grid.floor(f32::INFINITY, f32::NEG_INFINITY), 4.0);
    // A grid of a plane reads the plane inside it (bilinear is exact on a plane).
    let plane = HeightGrid::from_fn([-8.0, -8.0], 0.5, 33, 33, |x, z| 0.25 * x - 0.5 * z).expect("plane");
    for &(x, z) in &[(0.1_f32, 0.2_f32), (-3.3, 4.75), (7.9, -7.9)] {
        assert!(
            (plane.floor(x, z) - (0.25 * x - 0.5 * z)).abs() < 1e-5,
            "({x}, {z})"
        );
    }
    // The attitude pass works on a grid as on the function.
    let (pitch, roll) = ground_attitude(&plane, [0.0; 3], 0.0, 2.0, 1.0);
    assert!((pitch - math::atan2(-0.5, 1.0)).abs() < 1e-5);
    assert!((roll - math::atan2(0.25, 1.0)).abs() < 1e-5);
}

#[test]
fn height_grid_rejects_bad_shapes() {
    assert_eq!(
        HeightGrid::new([0.0; 2], 1.0, 1, 4, vec![0.0; 4]),
        Err(HeightGridError::TooSmall)
    );
    assert_eq!(
        HeightGrid::new([0.0; 2], 0.0, 2, 2, vec![0.0; 4]),
        Err(HeightGridError::BadCell)
    );
    assert_eq!(
        HeightGrid::new([0.0; 2], f32::NAN, 2, 2, vec![0.0; 4]),
        Err(HeightGridError::BadCell)
    );
    assert_eq!(
        HeightGrid::new([f32::INFINITY, 0.0], 1.0, 2, 2, vec![0.0; 4]),
        Err(HeightGridError::BadCell)
    );
    assert_eq!(
        HeightGrid::new([0.0; 2], 1.0, 2, 2, vec![0.0; 5]),
        Err(HeightGridError::WrongLength)
    );
    assert!(HeightGridError::WrongLength.to_string().contains("cols * rows"));
}

#[test]
fn hover_fleet_terrain_matches_its_committed_golden() {
    let mut scenario = HoverFleetTerrain::new();
    let report = golden::check_golden(
        &mut scenario,
        &golden::engine_golden_dir(),
        GoldenMode::from_env(),
    );
    println!("{REPORT_PREFIX}{}", report.to_json());
    assert!(report.passed(), "{report}");
    // The run exercised the pass: hulls tilted with the ground and none was reset.
    let fleet = scenario.fleet();
    assert_eq!(fleet.len(), 64);
    assert!(
        fleet
            .bodies()
            .iter()
            .filter(|b| b.state.roll.abs() > 0.01)
            .count()
            > 8
    );
    assert!(
        fleet
            .bodies()
            .iter()
            .filter(|b| b.state.cushion_pitch.abs() > 0.01)
            .count()
            > 8
    );
    assert_eq!(scenario.report().recovered, 0);
    assert!(fleet.bodies().iter().all(|b| b.pos.iter().all(|v| v.is_finite())));
}

#[test]
fn fleet_hull_types_are_valid() {
    for hull in HoverFleetTerrain::hull_types() {
        assert!(hull.params.validate().is_ok());
    }
    let _ = FleetHull::new(HoverParams::DEFAULT_HULL, 2.0, 1.0);
}

#[test]
fn a_fleet_over_holes_in_the_floor_stays_finite() {
    // NaN in a band of X, infinity in a band of Z, rolling hills elsewhere: hulls cross both.
    let holes = FloorFn(|x: f32, z: f32| {
        if (10.0..14.0).contains(&x) {
            f32::NAN
        } else if (20.0..22.0).contains(&z) {
            f32::INFINITY
        } else {
            RollingField.floor(x, z)
        }
    });
    let mut fleet = test_fleet(12);
    let mut inputs = Vec::new();
    for tick in 0..900 {
        inputs.clear();
        inputs.extend((0..12).map(|s| FleetInput::new(s, axes_for(s, tick))));
        let report = fleet.step_all(
            &inputs,
            &holes,
            &WorldPhysics::EARTH,
            &HoverLimits::DEFAULT,
            &ClimbTuning::AIR_TIME,
            tick_dt(),
        );
        assert_eq!(
            report.recovered, 0,
            "tick {tick}: a bad floor sample never poisons a hull"
        );
        for b in fleet.bodies() {
            assert!(b.pos.iter().all(|v| v.is_finite()), "tick {tick}: {:?}", b.pos);
            assert!(b.state.ground_pitch.is_finite() && b.state.ground_roll.is_finite());
        }
    }
}
