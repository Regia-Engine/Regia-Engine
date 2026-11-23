// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pilot_collide.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 931-1347 (the tests), mark: adapt (Stage A: asserts unchanged).
// Edits: the seams (`ON_FOOT_PARAMS` for the human-scale constants, `door_local(None, ..)` for an unknown
// stem, a test-local `scrap_blocker` with this file's own box sizes for the host's loose-piece table);
// `det_max` in one floor closure; doc comments (DO27 names dropped).

#![allow(clippy::unwrap_used)]

use super::*;
use crate::compat::fixtures::ON_FOOT_PARAMS;

const P: &OnFootParams = &ON_FOOT_PARAMS;
const PILOT_CAPSULE_RADIUS_M: f32 = ON_FOOT_PARAMS.capsule_radius_m;

/// The test's loose-piece sizes (its own boxes, not a host's table): small is a step, medium is too
/// tall to step over, large blocks from the feet up.
const SIZE_SMALL: u8 = 0;
const SIZE_MEDIUM: u8 = 1;
const SIZE_LARGE: u8 = 2;

fn scrap_blocker(pos: [f32; 3], size: u8) -> Blocker {
    let (radius, height, force) = match size {
        SIZE_LARGE => (4.1, 3.0, true),
        SIZE_MEDIUM => (1.7, 1.5, false),
        _ => (0.4, 0.2, false),
    };
    Blocker {
        min: [pos[0] - radius, pos[1], pos[2] - radius],
        max: [pos[0] + radius, pos[1] + height, pos[2] + radius],
        force_block: force,
    }
}

fn flat(_: f32, _: f32) -> f32 {
    0.0
}

#[test]
fn world_floor_is_a_deck_within_a_step_else_the_base() {
    let mut solids = PilotSolids::default();
    solids.world.stamp_floor_rect(0.0, 4.0, 0.0, 4.0, 0.4);
    // A low deck over plain ground at 0: the foot planter and the host both stand on it.
    assert!((solids.world_floor_y(P, 2.0, 2.0, 0.0, 0.0) - 0.4).abs() < 1e-5);
    // Off the deck: the base.
    assert_eq!(solids.world_floor_y(P, 20.0, 20.0, 0.0, 0.0), 0.0);
    // A deck above one step is not a floor for these feet.
    solids.world.stamp_floor_rect(10.0, 14.0, 10.0, 14.0, 3.0);
    assert_eq!(solids.world_floor_y(P, 12.0, 12.0, 0.0, 0.0), 0.0);
    // A walker standing on the high deck keeps it, and a short drop is accepted.
    assert!((solids.world_floor_y(P, 12.0, 12.0, 3.0, 0.0) - 3.0).abs() < 1e-5);
}

#[test]
fn a_low_thin_lip_over_the_step_is_walked_over_and_a_wall_is_not() {
    // A 0.17 m sliver whose top is 0.57 m over the feet (2 cm over one step).
    let mut lip = PilotGrid::default();
    lip.stamp_wall_box([2.0, 0.40, -2.0], [2.4, 0.57, 2.0]);
    let over = slide_move(P, &lip, &[], 1.0, 0.0, 3.5, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M);
    assert!(over[0] > 3.4, "the lip stops the capsule at {over:?}");
    // The same top on a thick wall still blocks.
    let mut wall = PilotGrid::default();
    wall.stamp_wall_box([2.0, 0.0, -2.0], [2.4, 0.57, 2.0]);
    let stop = slide_move(P, &wall, &[], 1.0, 0.0, 3.5, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M);
    assert!(stop[0] < 2.0, "a thick low wall must block, got {stop:?}");
    // A lip well over the reach still blocks.
    let mut high = PilotGrid::default();
    high.stamp_wall_box([2.0, 1.0, -2.0], [2.4, 1.2, 2.0]);
    let stop = slide_move(P, &high, &[], 1.0, 0.0, 3.5, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M);
    assert!(stop[0] < 2.0, "a lip 1.2 m up must block, got {stop:?}");
}

#[test]
fn a_wall_stops_the_capsule() {
    let mut grid = PilotGrid::default();
    grid.stamp_wall_box([2.0, 0.0, -2.0], [2.4, 3.0, 2.0]);
    let feet = resolve_feet(P, [1.2, 0.0, 0.0], [2.0, 0.0], flat, &grid, &[], true);
    assert!(
        feet[0] < 2.0,
        "capsule centre must stay on the near side of the wall, got {feet:?}"
    );
}

#[test]
fn a_step_is_climbed_and_a_taller_face_is_not() {
    let mut low = PilotGrid::default();
    low.stamp_floor_rect(1.0, 3.0, -1.0, 1.0, 0.30);
    let up = resolve_feet(P, [0.4, 0.0, 0.0], [1.2, 0.0], flat, &low, &[], true);
    assert!(
        (up[1] - 0.30).abs() < 0.08,
        "a 0.30 m step is within step height, got {up:?}"
    );
    assert!(up[0] > 1.0, "the capsule should be on the step, got {up:?}");

    let mut high = PilotGrid::default();
    high.stamp_wall_box([1.0, 0.0, -1.0], [3.0, 1.2, 1.0]);
    let blocked = resolve_feet(P, [0.2, 0.0, 0.0], [2.0, 0.0], flat, &high, &[], true);
    assert!(blocked[0] < 1.0, "a 1.2 m face is not a step, got {blocked:?}");
}

#[test]
fn a_ramp_is_climbed_and_descended_without_leaving_it() {
    let floor = |x: f32, _: f32| det_max(x * 0.5, 0.0);
    let mut feet = [0.0, 0.0, 0.0];
    for _ in 0..30 {
        feet = resolve_feet(P, feet, [0.25, 0.0], floor, &PilotGrid::default(), &[], true);
    }
    let top = floor(feet[0], feet[2]);
    assert!(feet[0] > 4.0, "walked up the ramp, got {feet:?}");
    assert!(
        (feet[1] - top).abs() < 0.08,
        "feet left the ramp {feet:?} vs {top}"
    );
    for _ in 0..30 {
        feet = resolve_feet(P, feet, [-0.25, 0.0], floor, &PilotGrid::default(), &[], true);
    }
    let back = floor(feet[0], feet[2]);
    assert!(feet[0] < 1.0, "walked back down, got {feet:?}");
    assert!(
        (feet[1] - back).abs() < 0.08,
        "feet left the ramp on the way down {feet:?}"
    );
}

#[test]
fn a_raised_floor_is_not_tunneled() {
    let mut grid = PilotGrid::default();
    grid.stamp_floor_rect(0.0, 4.0, -1.0, 1.0, 0.35);
    let feet = resolve_feet(P, [1.0, 0.0, 0.0], [0.2, 0.0], flat, &grid, &[], true);
    assert!(
        feet[1] >= 0.30,
        "feet must stand on the slab, not the heightfield under it, got {feet:?}"
    );
}

#[test]
fn the_door_notch_admits_a_pilot_and_the_wall_beside_it_does_not() {
    let center = [0.0, 0.0];
    let door = door_local(None, P);
    assert!(door[1] < -8.0, "unknown buildings keep the spec notch on -Z");
    // In the notch, outside the rim, walking inward.
    let through = push_out_of_wall_ring(
        P,
        [0.0, -9.3],
        center,
        0.0,
        door,
        BUILDING_WALL_M,
        PILOT_CAPSULE_RADIUS_M,
    );
    assert!(
        (through[0] - 0.0).abs() < 0.05 && through[1] < -8.5,
        "the door lets the capsule through, got {through:?}"
    );
    // Beside the door, same radius, must be pushed out.
    let beside = push_out_of_wall_ring(
        P,
        [2.4, -9.1],
        center,
        0.0,
        door,
        BUILDING_WALL_M,
        PILOT_CAPSULE_RADIUS_M,
    );
    let dist = math::sqrt(beside[0] * beside[0] + beside[1] * beside[1]);
    assert!(
        dist > 9.4,
        "the wall beside the door pushes the capsule outside, got {beside:?} dist {dist}"
    );
    // Already inside: the ring does not eject them.
    let inside = push_out_of_wall_ring(
        P,
        [0.0, -2.0],
        center,
        0.0,
        door,
        BUILDING_WALL_M,
        PILOT_CAPSULE_RADIUS_M,
    );
    assert_eq!(inside, [0.0, -2.0]);
}

#[test]
fn a_pilot_cannot_leave_the_interior_except_by_the_door() {
    let door = door_local(None, P);
    // Inner half of the wall, off the door ray. The nearer free side is
    // back inside, so the step does not complete the exit.
    let leaving = push_out_of_wall_ring(
        P,
        [2.2, -8.2],
        [0.0, 0.0],
        0.0,
        door,
        BUILDING_WALL_M,
        PILOT_CAPSULE_RADIUS_M,
    );
    let dist = math::sqrt(leaving[0] * leaving[0] + leaving[1] * leaving[1]);
    assert!(
        dist <= 8.1,
        "a wall exit pushes the capsule back inside, got {leaving:?}"
    );
    let by_door = push_out_of_wall_ring(
        P,
        [0.0, -8.6],
        [0.0, 0.0],
        0.0,
        door,
        BUILDING_WALL_M,
        PILOT_CAPSULE_RADIUS_M,
    );
    assert!(by_door[1] < -8.0, "the door is the way out, got {by_door:?}");
}

#[test]
fn large_scrap_blocks_and_small_scrap_is_a_step() {
    let large = scrap_blocker([0.0, 0.0, 0.0], SIZE_LARGE);
    assert!(large.force_block);
    assert!(
        large.max[0] >= 4.0,
        "the box covers the 8 m heap, half-extent {}",
        large.max[0]
    );
    let stopped = resolve_feet(
        P,
        [-8.0, 0.0, 0.0],
        [2.0, 0.0],
        flat,
        &PilotGrid::default(),
        &[large],
        true,
    );
    assert!(
        stopped[0] < -4.2,
        "a large heap stops the capsule at the mesh edge, got {stopped:?}"
    );
    let small = scrap_blocker([1.5, 0.0, 0.0], SIZE_SMALL);
    assert!(!small.force_block);
    let over = resolve_feet(
        P,
        [0.2, 0.0, 0.0],
        [2.5, 0.0],
        flat,
        &PilotGrid::default(),
        &[small],
        true,
    );
    assert!(
        over[0] > 2.0,
        "a small piece is a step, the capsule reaches the far side, got {over:?}"
    );
}

#[test]
fn medium_scrap_is_too_tall_to_step_over() {
    let medium = scrap_blocker([2.0, 0.0, 0.0], SIZE_MEDIUM);
    let stopped = resolve_feet(
        P,
        [0.2, 0.0, 0.0],
        [3.0, 0.0],
        flat,
        &PilotGrid::default(),
        &[medium],
        true,
    );
    assert!(
        stopped[0] < 1.6,
        "medium scrap stands above step height, got {stopped:?}"
    );
}

#[test]
fn a_door_mouth_opens_a_gridded_wall_and_the_jamb_stays() {
    let mut grid = PilotGrid::default();
    grid.stamp_wall_box([-4.0, 0.0, -0.3], [4.0, 2.4, 0.3]);
    let shut = grid.slide(P, 0.0, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M);
    assert!(
        shut[1].abs() > 0.15 || shut[0].abs() > 0.15,
        "the wall is shut before the cut, got {shut:?}"
    );
    grid.clear_door_mouth(P, [-0.55, 0.0, -0.08], [0.55, 2.1, 0.08]);
    let through = grid.slide(P, 0.0, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M);
    assert!(
        through[0].abs() < 0.15 && through[1].abs() < 0.15,
        "the mouth lets the capsule stand in the door, got {through:?}"
    );
    let jamb = grid.slide(P, 2.4, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M);
    assert!(
        (jamb[0] - 2.4).abs() > 0.15 || jamb[1].abs() > 0.15,
        "the jamb beside the mouth still blocks, got {jamb:?}"
    );
}

#[test]
fn a_cut_mouth_is_passable_across_its_full_depth() {
    // One leaf in a wall. The capsule's whole band must be clear through
    // the cut depth: a wall cell or a sliver left inside the opening
    // seals the mouth (an inset once left one across a shelter leaf).
    let mut grid = PilotGrid::default();
    grid.stamp_wall_box([-4.0, 0.0, -0.3], [4.0, 2.4, 0.3]);
    grid.clear_door_mouth(P, [-0.55, 0.0, -0.08], [0.55, 2.1, 0.08]);
    let radius = PILOT_CAPSULE_RADIUS_M;
    for step in 0..=24 {
        let z = -1.20 + step as f32 * 0.10;
        for band in 0..=6 {
            let x = -0.15 + band as f32 * 0.05;
            let slid = grid.slide(P, x, z, 0.0, radius);
            assert!(
                (slid[0] - x).abs() < 0.02 && (slid[1] - z).abs() < 0.02,
                "the mouth must not push the capsule at ({x}, {z}), got {slid:?}"
            );
        }
    }
    // The wall cells beside the cleared columns still funnel a wide
    // approach into the opening instead of letting it through.
    let beside = grid.slide(P, -0.90, 0.0, 0.0, radius);
    assert!(
        beside[0] > -0.45,
        "the wall beside the mouth funnels the capsule to the opening, got {beside:?}"
    );
}

#[test]
fn a_nested_panel_merges_into_one_mouth() {
    // A moving leaf and a static panel a meter in, offset on the wide
    // axis. The passable span is the union of the two openings, not a
    // slit narrower than the body.
    let mut grid = PilotGrid::default();
    grid.stamp_wall_box([-4.0, 0.0, -0.3], [4.0, 2.4, 0.3]);
    grid.stamp_wall_box([-4.0, 0.0, -1.4], [4.0, 2.4, -1.0]);
    grid.clear_door_mouth(P, [-0.55, 0.0, -0.08], [0.55, 2.1, 0.08]);
    grid.clear_door_mouth(P, [-0.25, 0.0, -1.28], [0.85, 2.1, -1.12]);
    assert_eq!(grid.door_cuts.len(), 1, "the panels merge into one mouth");
    let radius = PILOT_CAPSULE_RADIUS_M;
    // The shared corridor passes both planes and stays off the walls.
    for z in [-0.05, -0.70, -1.20] {
        for x in [0.05, 0.20] {
            let slid = grid.slide(P, x, z, 0.0, radius);
            assert!(
                (slid[0] - x).abs() < 0.02 && (slid[1] - z).abs() < 0.02,
                "the merged mouth passes the capsule at ({x}, {z}), got {slid:?}"
            );
        }
    }
    // The wall beside the union still blocks.
    let beside = grid.slide(P, 1.6, -1.2, 0.0, radius);
    assert!(
        (beside[0] - 1.6).abs() > 0.15,
        "the wall beside the merged mouth blocks, got {beside:?}"
    );
}

#[test]
fn an_airborne_body_is_not_glued_to_the_floor() {
    let feet = resolve_feet(
        P,
        [0.0, 3.0, 0.0],
        [0.5, 0.0],
        flat,
        &PilotGrid::default(),
        &[],
        false,
    );
    assert!((feet[1] - 3.0).abs() < 0.01, "jump height sticks, got {feet:?}");
    assert!((feet[0] - 0.5).abs() < 0.01);
}

/// A lining shell's hidden floor-slab end face (from under the floor up to it) and a roof facet 13 m
/// overhead share the cells of a seam. Fused into one span they made a floor-to-roof wall nothing
/// drew; kept apart, neither stands in the body band and the walker crosses.
#[test]
fn a_slab_end_under_the_floor_and_a_roof_face_overhead_do_not_wall_the_body_band() {
    let mut grid = PilotGrid::default();
    // A seam line across the walk at x 2.0..2.75, the way the tube seams cross the floor.
    for k in -8..=8 {
        let z = k as f32 * 0.375;
        grid.stamp_wall(2.2, z, -1.4, 0.0);
        grid.stamp_wall(2.2, z, 13.0, 14.0);
    }
    let cell = grid.cells[&(cell_index(2.2), cell_index(0.0))];
    assert_eq!(
        cell.spans(),
        &[WallSpan { lo: -1.4, hi: 0.0 }, WallSpan { lo: 13.0, hi: 14.0 }]
    );
    assert_eq!(cell.wall_envelope(), Some((-1.4, 14.0)));
    let walked = slide_move(P, &grid, &[], 1.0, 0.0, 3.5, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M);
    assert!(walked[0] > 3.4, "the seam stops the capsule at {walked:?}");
    let feet = resolve_feet(P, [1.0, 0.0, 0.0], [2.5, 0.0], flat, &grid, &[], true);
    assert!(
        feet[0] > 3.4 && feet[1].abs() < 1e-6,
        "walked over the seam on the floor, got {feet:?}"
    );
    assert_eq!(
        grid.slide(P, 2.3, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M),
        [2.3, 0.0],
        "standing on the seam is not inside a wall"
    );
    // The camera ray at eye height crosses the seam too; the roof still stops a ray up there.
    assert!(
        grid.wall_hit_along([0.0, 1.6, 0.0], [4.0, 1.6, 0.0], 0.25, 0.3)
            .is_none()
    );
    assert!(
        grid.wall_hit_along([0.0, 13.5, 0.0], [4.0, 13.5, 0.0], 0.25, 0.3)
            .is_some()
    );
    // A real wall in the body band in the same cells still blocks, as a third span.
    for k in -8..=8 {
        grid.stamp_wall(2.2, k as f32 * 0.375, 0.4, 2.6);
    }
    assert_eq!(grid.cells[&(cell_index(2.2), cell_index(0.0))].spans().len(), 3);
    let stopped = slide_move(P, &grid, &[], 1.0, 0.0, 3.5, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M);
    assert!(
        stopped[0] < 2.0,
        "a body-height wall still blocks, got {stopped:?}"
    );
}

#[test]
fn wall_spans_join_within_the_tolerance_and_do_not_depend_on_the_stamp_order() {
    let stamps = [
        (0.0, 1.0),
        (1.08, 2.0),
        (2.05, 3.0),
        (6.0, 7.0),
        (-4.0, -3.0),
        (6.5, 6.6),
    ];
    let mut want: Option<Vec<WallSpan>> = None;
    // Every rotation and its reverse.
    for start in 0..stamps.len() {
        for reverse in [false, true] {
            let mut order: Vec<(f32, f32)> = (0..stamps.len())
                .map(|i| stamps[(start + i) % stamps.len()])
                .collect();
            if reverse {
                order.reverse();
            }
            let mut cell = PilotCell::default();
            for (lo, hi) in order {
                cell.add_wall(lo, hi);
            }
            let got = cell.spans().to_vec();
            match &want {
                None => want = Some(got),
                Some(w) => assert_eq!(&got, w, "stamp order changed the spans"),
            }
        }
    }
    assert_eq!(
        want.unwrap(),
        vec![
            WallSpan { lo: -4.0, hi: -3.0 },
            WallSpan { lo: 0.0, hi: 3.0 },
            WallSpan { lo: 6.0, hi: 7.0 }
        ]
    );
    // A gap over the tolerance stays open; a bad span is ignored.
    let mut cell = PilotCell::default();
    cell.add_wall(0.0, 1.0);
    cell.add_wall(1.2, 2.0);
    cell.add_wall(f32::NAN, 3.0);
    cell.add_wall(5.0, 4.0);
    assert_eq!(cell.spans().len(), 2);
}

#[test]
fn past_the_span_cap_the_closest_pair_merges() {
    let mut cell = PilotCell::default();
    for (lo, hi) in [(0.0, 1.0), (3.0, 4.0), (10.0, 11.0), (20.0, 21.0)] {
        cell.add_wall(lo, hi);
    }
    assert_eq!(cell.spans().len(), MAX_WALL_SPANS);
    // A fifth span 0.5 m over the second: that gap is the smallest, so those two merge.
    cell.add_wall(4.5, 5.0);
    assert_eq!(
        cell.spans(),
        &[
            WallSpan { lo: 0.0, hi: 1.0 },
            WallSpan { lo: 3.0, hi: 5.0 },
            WallSpan { lo: 10.0, hi: 11.0 },
            WallSpan { lo: 20.0, hi: 21.0 }
        ]
    );
}

#[test]
fn a_door_cut_clears_only_the_spans_it_crosses_and_a_thin_span_is_a_step() {
    let mut cell = PilotCell::default();
    cell.add_wall(-2.0, -1.0);
    cell.add_wall(0.0, 2.2);
    cell.add_wall(3.0, 4.0);
    cell.clear_walls_overlapping(-0.25, 2.35);
    assert_eq!(
        cell.spans(),
        &[WallSpan { lo: -2.0, hi: -1.0 }, WallSpan { lo: 3.0, hi: 4.0 }]
    );
    // A curb top under a roof span in the same cell is still a step (the fused span was not).
    let mut grid = PilotGrid::default();
    grid.stamp_wall(1.0, 0.0, 0.0, 0.3);
    grid.stamp_wall(1.0, 0.0, 9.0, 12.0);
    assert_eq!(grid.support(1.0, 0.0, 0.0, PILOT_CAPSULE_RADIUS_M), Some(0.3));
}

#[test]
fn a_segment_through_a_drawn_wall_reports_the_first_blocked_sample() {
    let mut grid = PilotGrid::default();
    // A wall column across x = 3 (one cell wide) from y 0 to 4.
    for k in -4..=4 {
        grid.stamp_wall(3.2, k as f32 * 0.5, 0.0, 4.0);
    }
    let hit = grid
        .wall_hit_along([0.0, 1.5, 0.0], [6.0, 1.5, 0.0], 0.25, 0.3)
        .expect("blocked");
    assert!(
        (0.3..=0.55).contains(&hit),
        "hit at t {hit} (one cell of slack before the wall)"
    );
    assert!(
        grid.wall_hit_along([0.0, 6.0, 0.0], [6.0, 6.0, 0.0], 0.25, 0.3)
            .is_none(),
        "over the wall"
    );
    assert!(
        grid.wall_hit_along([0.0, 1.5, 0.0], [1.5, 1.5, 0.0], 0.25, 0.3)
            .is_none(),
        "short of it"
    );
    assert!(
        PilotGrid::default()
            .wall_hit_along([0.0, 1.5, 0.0], [6.0, 1.5, 0.0], 0.25, 0.3)
            .is_none()
    );
}

/// Original (plan row W-03): `building_key` keeps the stem for the first art set and marks the
/// second with an opaque suffix; `door_local` passes the host's socket through.
#[test]
fn building_keys_and_door_sockets_are_the_hosts() {
    assert_eq!(building_key("hall", false), "hall");
    assert_ne!(building_key("hall", true), building_key("hall", false));
    assert_eq!(door_local(Some([1.5, 2.5]), P), [1.5, 2.5]);
    assert_eq!(door_local(None, P), [0.0, -P.building_radius_m]);
}
