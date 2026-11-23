// SPDX-License-Identifier: MPL-2.0
//! Independent boundary contracts for circular streaming-cell selection.

use regia_scene::stream_grid::{CellCoord, StreamError, StreamGrid};

fn expected_by_rectangles(grid_size: f64, x: f64, z: f64, radius: f64) -> Vec<CellCoord> {
    let low_x = ((x - radius) / grid_size).floor() as i64 - 1;
    let high_x = ((x + radius) / grid_size).floor() as i64 + 1;
    let low_z = ((z - radius) / grid_size).floor() as i64 - 1;
    let high_z = ((z + radius) / grid_size).floor() as i64 + 1;
    let radius_squared = radius * radius;
    let mut cells = Vec::new();
    for cx in low_x..=high_x {
        for cz in low_z..=high_z {
            let left = cx as f64 * grid_size;
            let right = (cx + 1) as f64 * grid_size;
            let bottom = cz as f64 * grid_size;
            let top = (cz + 1) as f64 * grid_size;
            let dx = if x < left {
                left - x
            } else if x > right {
                x - right
            } else {
                0.0
            };
            let dz = if z < bottom {
                bottom - z
            } else if z > top {
                z - top
            } else {
                0.0
            };
            if dx * dx + dz * dz <= radius_squared {
                cells.push(CellCoord::new(cx, cz));
            }
        }
    }
    cells
}

fn check_case(grid: &StreamGrid, size: f64, x: f64, z: f64, radius: f64, hand: &[CellCoord]) {
    let got = grid
        .cells_within(x, z, radius, 10_000)
        .expect("small boundary query");
    assert_eq!(got, hand, "hand-derived set for ({x}, {z}), radius {radius}");
    assert_eq!(
        got,
        expected_by_rectangles(size, x, z, radius),
        "independent rectangle oracle"
    );
    assert!(
        got.windows(2).all(|pair| pair[0] < pair[1]),
        "X-major/Z-minor order"
    );
    assert_eq!(
        got.iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        got.len()
    );
}

#[test]
fn zero_radius_selects_one_two_or_four_closed_rectangles() {
    let grid = StreamGrid::new(8.0).expect("grid");
    check_case(&grid, 8.0, 3.0, 5.0, 0.0, &[CellCoord::new(0, 0)]);
    check_case(
        &grid,
        8.0,
        8.0,
        5.0,
        0.0,
        &[CellCoord::new(0, 0), CellCoord::new(1, 0)],
    );
    check_case(
        &grid,
        8.0,
        8.0,
        8.0,
        0.0,
        &[
            CellCoord::new(0, 0),
            CellCoord::new(0, 1),
            CellCoord::new(1, 0),
            CellCoord::new(1, 1),
        ],
    );
    check_case(
        &grid,
        8.0,
        -8.0,
        -8.0,
        0.0,
        &[
            CellCoord::new(-2, -2),
            CellCoord::new(-2, -1),
            CellCoord::new(-1, -2),
            CellCoord::new(-1, -1),
        ],
    );
}

#[test]
fn tangent_axis_contacts_and_adjacent_radii_match_closed_rectangles() {
    let grid = StreamGrid::new(8.0).expect("grid");
    for (x, z, hand_at_tangency) in [
        (
            0.0,
            4.0,
            vec![
                CellCoord::new(-2, 0),
                CellCoord::new(-1, -1),
                CellCoord::new(-1, 0),
                CellCoord::new(-1, 1),
                CellCoord::new(0, -1),
                CellCoord::new(0, 0),
                CellCoord::new(0, 1),
                CellCoord::new(1, 0),
            ],
        ),
        (
            -8.0,
            4.0,
            vec![
                CellCoord::new(-3, 0),
                CellCoord::new(-2, -1),
                CellCoord::new(-2, 0),
                CellCoord::new(-2, 1),
                CellCoord::new(-1, -1),
                CellCoord::new(-1, 0),
                CellCoord::new(-1, 1),
                CellCoord::new(0, 0),
            ],
        ),
    ] {
        let radius: f64 = 8.0;
        let below = f64::from_bits(radius.to_bits() - 1);
        let above = f64::from_bits(radius.to_bits() + 1);
        assert_eq!(
            grid.cells_within(x, z, radius, 10_000).expect("tangent query"),
            hand_at_tangency,
            "complete hand-derived membership at ({x}, {z})"
        );
        for r in [below, radius, above] {
            let got = grid.cells_within(x, z, r, 10_000).expect("small query");
            assert_eq!(got, expected_by_rectangles(8.0, x, z, r));
            assert!(got.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }
}

#[test]
fn tangent_corner_contact_and_adjacent_radii_are_observable() {
    let grid = StreamGrid::new(1.0).expect("grid");
    let radius: f64 = 5.0;
    let below = f64::from_bits(radius.to_bits() - 1);
    let above = f64::from_bits(radius.to_bits() + 1);
    let target = CellCoord::new(3, 4); // Its lower corner (3, 4) is exactly 3-4-5 from the center.
    for (x, z, cell) in [(0.0, 0.0, target), (-10.0, -10.0, CellCoord::new(-7, -6))] {
        let at = grid.cells_within(x, z, radius, 10_000).expect("corner tangent");
        assert!(at.contains(&cell));
        assert!(
            !grid
                .cells_within(x, z, below, 10_000)
                .expect("below")
                .contains(&cell)
        );
        assert!(
            grid.cells_within(x, z, above, 10_000)
                .expect("above")
                .contains(&cell)
        );
        for r in [below, radius, above] {
            let got = grid.cells_within(x, z, r, 10_000).expect("small query");
            assert_eq!(got, expected_by_rectangles(1.0, x, z, r));
            assert!(got.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }
}

#[test]
fn integer_cell_translations_preserve_the_complete_membership_set() {
    let grid = StreamGrid::new(8.0).expect("grid");
    for (x, z, radius, shift_x, shift_z) in [(7.0, -9.0, 8.0, 13_i64, -21_i64), (-16.0, 24.0, 12.0, -7, 11)] {
        let original = grid.cells_within(x, z, radius, 10_000).expect("original");
        let translated = grid
            .cells_within(x + shift_x as f64 * 8.0, z + shift_z as f64 * 8.0, radius, 10_000)
            .expect("translated");
        let expected: Vec<_> = original
            .iter()
            .map(|cell| CellCoord::new(cell.x + shift_x, cell.z + shift_z))
            .collect();
        assert_eq!(translated, expected);
    }
}

#[test]
fn bounding_box_work_cap_counts_candidates_before_circle_filtering() {
    let grid = StreamGrid::new(8.0).expect("grid");
    // Interior point, zero radius: the documented extra ring visits a 3 × 3 candidate box;
    // only one rectangle contains the point.
    assert_eq!(
        grid.cells_within(3.0, 5.0, 0.0, 8),
        Err(StreamError::TooManyCells { cells: 9, limit: 8 })
    );
    assert_eq!(
        grid.cells_within(3.0, 5.0, 0.0, 9),
        Ok(vec![CellCoord::new(0, 0)])
    );
    assert_eq!(
        grid.cells_within(3.0, 5.0, 0.0, 10),
        Ok(vec![CellCoord::new(0, 0)])
    );
}

#[test]
fn circle_query_keeps_nonfinite_and_out_of_range_errors() {
    let grid = StreamGrid::new(8.0).expect("grid");
    assert_eq!(
        grid.cells_within(f64::NAN, 0.0, 0.0, 10),
        Err(StreamError::NonFinite("x"))
    );
    assert_eq!(
        grid.cells_within(0.0, f64::INFINITY, 0.0, 10),
        Err(StreamError::NonFinite("z"))
    );
    assert!(matches!(
        grid.cells_within(1.0e30, 0.0, 0.0, 10),
        Err(StreamError::OutOfRange(_))
    ));
}
