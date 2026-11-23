// SPDX-License-Identifier: MPL-2.0
//! Transactional behavior when one coordinate axis of a focus cannot be represented.

use regia_scene::floating_origin::{FloatingOrigin, LargePosition, LargeSpace, OriginError, Rebase};
use regia_scene::stream_grid::MAX_CELL_COORD;

const CELL_SIZE: f64 = 1024.0;
const THRESHOLD: f32 = 512.0;

fn space() -> LargeSpace {
    LargeSpace::new(CELL_SIZE).expect("valid test space")
}

fn invalid_focus(axis: usize, out_of_range: bool, negative: bool) -> LargePosition {
    let mut focus = LargePosition {
        cell: [7, -9, 11],
        local: [0.0; 3],
    };
    if out_of_range {
        focus.cell[axis] = if negative {
            -MAX_CELL_COORD - 1
        } else {
            MAX_CELL_COORD + 1
        };
    } else {
        focus.local[axis] = if negative {
            f32::NEG_INFINITY
        } else {
            f32::INFINITY
        };
    }
    focus
}

#[test]
fn rejected_axis_updates_are_atomic_and_recover_like_a_clean_origin() {
    let valid_following_focus = LargePosition {
        cell: [6, -8, 10],
        local: [0.0; 3],
    };

    // Each axis is independently the failing axis, with both out-of-range signs and both
    // infinity signs. Other axes remain valid, displaced focus coordinates.
    for axis in 0..3 {
        for out_of_range in [true, false] {
            for negative in [true, false] {
                let mut origin = FloatingOrigin::new(space(), [0; 3], THRESHOLD).expect("valid origin");
                let mut clean = origin;
                let snapshot_cell = origin.cell();
                let snapshot_space = *origin.space();

                let error = origin
                    .update(&invalid_focus(axis, out_of_range, negative))
                    .expect_err("one invalid axis must reject the whole focus");
                assert_eq!(origin.cell(), snapshot_cell, "axis={axis}");
                assert_eq!(*origin.space(), snapshot_space, "axis={axis}");
                assert!(matches!(
                    error,
                    OriginError::OutOfRange | OriginError::NonFinite("focus")
                ));

                let recovered = origin.update(&valid_following_focus).expect("valid recovery");
                let control = clean
                    .update(&valid_following_focus)
                    .expect("valid control update");
                assert_eq!(recovered, control, "axis={axis}");
                assert_eq!(origin.cell(), clean.cell(), "axis={axis}");
                assert_eq!(*origin.space(), *clean.space(), "axis={axis}");
            }
        }
    }
}

#[test]
fn exact_half_cell_boundaries_are_stable_on_each_axis_and_sign() {
    let s = space();
    for axis in 0..3 {
        for world_coordinate in [-CELL_SIZE / 2.0, CELL_SIZE / 2.0] {
            let mut world = [0.0; 3];
            world[axis] = world_coordinate;
            let focus = s.from_world(world).expect("half-cell position is representable");
            let mut origin = FloatingOrigin::new(s, [0; 3], THRESHOLD).expect("valid origin");
            assert_eq!(origin.update(&focus).expect("boundary update"), None);
            assert_eq!(origin.cell(), [0; 3]);
        }
    }
}

#[test]
fn opposite_multiaxis_rebases_preserve_independent_world_points() {
    let s = space();
    let mut origin = FloatingOrigin::new(s, [0; 3], THRESHOLD).expect("valid origin");
    let points = [
        s.from_world([1300.25, -2250.5, 3075.75]).expect("point one"),
        s.from_world([-3000.5, 4200.25, -750.125]).expect("point two"),
        s.from_world([512.0, -512.0, 1536.0]).expect("point three"),
    ];
    let world_truth = points.map(|point| s.to_world(&point));
    let mut locals = points.map(|point| origin.to_local(&point));
    let focuses = [
        [3072.0, -3072.0, 4096.0],
        [-3072.0, 3072.0, -4096.0],
        [4096.0, 2048.0, -3072.0],
        [-4096.0, -2048.0, 3072.0],
    ];

    for world_focus in focuses {
        let focus = s.from_world(world_focus).expect("focus in range");
        let rebase = origin
            .update(&focus)
            .expect("accepted focus")
            .expect("focus lies beyond threshold");
        assert!(rebase.cell_delta.iter().any(|delta| *delta != 0));
        rebase.apply(&mut locals[0]);
        rebase.apply(&mut locals[1]);
        rebase.apply(&mut locals[2]);

        for (local, truth) in locals.iter().zip(world_truth) {
            let recovered = s.to_world(&origin.from_local(*local).expect("local point"));
            for axis in 0..3 {
                let scale = f64::from(local[axis].abs().max(rebase.shift[axis].abs())).max(1.0);
                let tolerance = 8.0 * f64::from(f32::EPSILON) * scale;
                assert!(
                    (recovered[axis] - truth[axis]).abs() <= tolerance,
                    "axis={axis}, recovered={}, truth={}, tolerance={tolerance}",
                    recovered[axis],
                    truth[axis]
                );
            }
        }
    }
}

#[test]
fn out_of_range_rebase_to_is_a_documented_no_op() {
    let mut origin = FloatingOrigin::new(space(), [3, -2, 1], THRESHOLD).expect("valid origin");
    let snapshot_cell = origin.cell();
    let snapshot_space = *origin.space();
    let rebase: Rebase = origin.rebase_to([MAX_CELL_COORD + 1, -2, 1]);
    assert_eq!(rebase.cell_delta, [0; 3]);
    assert_eq!(rebase.shift, [0.0; 3]);
    assert_eq!(origin.cell(), snapshot_cell);
    assert_eq!(*origin.space(), snapshot_space);
}
