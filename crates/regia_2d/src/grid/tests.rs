// SPDX-License-Identifier: MPL-2.0
//! Geometry tests: cell/world round trips for every orientation, hit tests against the drawn
//! outlines, map size, draw order and culling ranges.

use super::*;
use crate::tilemap::{Orientation, StaggerAxis, StaggerIndex};

fn g() -> GridSize {
    GridSize {
        width: 7,
        height: 6,
        tile_width: 32,
        tile_height: 16,
    }
}

/// Every orientation under test.
fn all() -> Vec<Orientation> {
    let mut out = vec![Orientation::Orthogonal, Orientation::Isometric];
    for axis in [StaggerAxis::X, StaggerAxis::Y] {
        for index in [StaggerIndex::Odd, StaggerIndex::Even] {
            out.push(Orientation::Staggered { axis, index });
            for side_length in [0, 6, 10] {
                out.push(Orientation::Hexagonal {
                    axis,
                    index,
                    side_length,
                });
            }
        }
    }
    out
}

/// Strict point-in-convex-polygon (clockwise or counter-clockwise), `eps` inside the edges.
fn strictly_inside(poly: &[[f64; 2]], p: [f64; 2], eps: f64) -> bool {
    let mut sign = 0.0f64;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        let cross = (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
        let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        if len < 1e-12 {
            // A degenerate edge (a hexagon of side length 0): nothing to test against.
            continue;
        }
        let d = cross / len;
        if d.abs() <= eps {
            return false;
        }
        if sign == 0.0 {
            sign = d.signum();
        } else if d.signum() != sign {
            return false;
        }
    }
    true
}

#[test]
fn stack_outlines_match_the_allocated_outline() {
    for o in all() {
        for y in 0..g().height {
            for x in 0..g().width {
                let stack = o.cell_outline_points(g(), x, y);
                let heap = o.cell_outline(g(), x, y);
                assert!(
                    stack.len == 4 || stack.len == 6,
                    "{o:?} outline length {}",
                    stack.len
                );
                assert_eq!(
                    &stack.points[..stack.len],
                    heap.as_slice(),
                    "{o:?} cell ({x}, {y})"
                );
            }
        }
    }
}

#[test]
fn cell_center_round_trips_for_every_orientation() {
    for o in all() {
        for y in 0..g().height {
            for x in 0..g().width {
                let c = o.cell_center(g(), x, y);
                assert_eq!(
                    o.cell_at(g(), c),
                    Some((x, y)),
                    "{o:?} cell ({x}, {y}) center {c:?}"
                );
            }
        }
    }
}

#[test]
fn points_inside_an_outline_pick_that_cell_and_none_pick_two() {
    for o in all() {
        let [w, h] = o.pixel_size(g());
        // A lattice of sample points over the map box (step coprime with the tile sizes).
        let mut y = 0.37;
        while y < h {
            let mut x = 0.41;
            while x < w {
                let picked = o.cell_at(g(), [x, y]);
                let mut containing = Vec::new();
                for cy in 0..g().height {
                    for cx in 0..g().width {
                        if strictly_inside(&o.cell_outline(g(), cx, cy), [x, y], 1e-6) {
                            containing.push((cx, cy));
                        }
                    }
                }
                assert!(containing.len() <= 1, "{o:?}: outlines overlap at ({x}, {y})");
                if let Some(&cell) = containing.first() {
                    assert_eq!(picked, Some(cell), "{o:?} at ({x}, {y})");
                }
                if let Some(cell) = picked {
                    // The pick is inside the cell's outline (edges count).
                    let loose = strictly_inside(&o.cell_outline(g(), cell.0, cell.1), [x, y], -1e-6);
                    assert!(
                        loose,
                        "{o:?}: pick {cell:?} for ({x}, {y}) is outside its outline"
                    );
                }
                x += 3.1;
            }
            y += 2.7;
        }
    }
}

#[test]
fn outlines_stay_inside_the_map_box_and_touch_it() {
    for o in all() {
        let [w, h] = o.pixel_size(g());
        let (mut min, mut max) = ([f64::MAX; 2], [f64::MIN; 2]);
        for cy in 0..g().height {
            for cx in 0..g().width {
                // The bounding box of every cell, not only the drawn shape.
                let [ox, oy] = o.cell_origin(g(), cx, cy);
                for p in [[ox, oy], [ox + 32.0, oy + 16.0]] {
                    for i in 0..2 {
                        min[i] = min[i].min(p[i]);
                        max[i] = max[i].max(p[i]);
                    }
                }
            }
        }
        assert!(min[0] >= -1e-9 && min[1] >= -1e-9, "{o:?} min {min:?}");
        assert!(
            (max[0] - w).abs() < 1e-9 && (max[1] - h).abs() < 1e-9,
            "{o:?} max {max:?} vs {w}x{h}"
        );
        assert!(
            min[0].abs() < 1e-9 && min[1].abs() < 1e-9,
            "{o:?} min {min:?} should touch 0"
        );
    }
}

#[test]
fn pinned_origins_and_sizes() {
    let g = GridSize {
        width: 3,
        height: 2,
        tile_width: 64,
        tile_height: 32,
    };
    assert_eq!(Orientation::Orthogonal.pixel_size(g), [192.0, 64.0]);
    assert_eq!(Orientation::Isometric.pixel_size(g), [160.0, 80.0]);
    // Diamond: (0,0) sits at the top, (0,1) at the left edge, (2,0) at the right.
    assert_eq!(Orientation::Isometric.cell_origin(g, 0, 0), [32.0, 0.0]);
    assert_eq!(Orientation::Isometric.cell_origin(g, 0, 1), [0.0, 16.0]);
    assert_eq!(Orientation::Isometric.cell_origin(g, 2, 0), [96.0, 32.0]);
    let stag = Orientation::Staggered {
        axis: StaggerAxis::Y,
        index: StaggerIndex::Odd,
    };
    assert_eq!(stag.pixel_size(g), [224.0, 48.0]);
    assert_eq!(stag.cell_origin(g, 1, 1), [96.0, 16.0]);
    assert_eq!(stag.cell_origin(g, 1, 0), [64.0, 0.0]);
    let flat = Orientation::Hexagonal {
        axis: StaggerAxis::X,
        index: StaggerIndex::Even,
        side_length: 16,
    };
    // Columns 0 and 2 are shifted down by half a cell; columns step (64 + 16) / 2.
    assert_eq!(flat.cell_origin(g, 0, 0), [0.0, 16.0]);
    assert_eq!(flat.cell_origin(g, 1, 0), [40.0, 0.0]);
    assert_eq!(flat.cell_origin(g, 2, 1), [80.0, 48.0]);
    assert_eq!(flat.pixel_size(g), [144.0, 80.0]);
}

#[test]
fn a_one_row_odd_stagger_adds_no_half_tile() {
    let g = GridSize {
        width: 4,
        height: 1,
        tile_width: 32,
        tile_height: 32,
    };
    let odd = Orientation::Staggered {
        axis: StaggerAxis::Y,
        index: StaggerIndex::Odd,
    };
    let even = Orientation::Staggered {
        axis: StaggerAxis::Y,
        index: StaggerIndex::Even,
    };
    // Odd shifts a row only when height > 1. Even shifts column 0 of a one-row map.
    assert_eq!(odd.pixel_size(g), [128.0, 32.0]);
    assert_eq!(even.pixel_size(g), [144.0, 32.0]);
}

#[test]
fn draw_order_visits_every_cell_once_back_to_front() {
    for o in all() {
        let mut seen = Vec::new();
        o.draw_order(g(), 0, g().width, 0, g().height, |x, y| {
            seen.push((x, y));
            true
        });
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            (g().width * g().height) as usize,
            "{o:?} visits each cell once"
        );
        // The screen height of the cell's top edge never decreases along a column-staggered
        // map's order; every other orientation is row-major.
        let column_staggered = matches!(
            o,
            Orientation::Staggered {
                axis: StaggerAxis::X,
                ..
            } | Orientation::Hexagonal {
                axis: StaggerAxis::X,
                ..
            }
        );
        if column_staggered {
            let tops: Vec<f64> = seen.iter().map(|&(x, y)| o.cell_origin(g(), x, y)[1]).collect();
            assert!(tops.windows(2).all(|w| w[0] <= w[1]), "{o:?} tops {tops:?}");
        } else {
            let mut row_major = seen.clone();
            row_major.sort_by_key(|&(x, y)| (y, x));
            assert_eq!(seen, row_major, "{o:?}");
        }
    }
}

#[test]
fn draw_order_stops_when_asked() {
    let mut n = 0;
    Orientation::Isometric.draw_order(g(), 0, 7, 0, 6, |_, _| {
        n += 1;
        n < 5
    });
    assert_eq!(n, 5);
}

#[test]
fn visible_cells_cover_every_intersecting_cell() {
    let views = [
        ([0.0, 0.0], [40.0, 30.0]),
        ([60.0, 20.0], [150.0, 70.0]),
        ([100.0, 40.0], [101.0, 41.0]),
        ([-50.0, -50.0], [10.0, 10.0]),
        ([0.0, 0.0], [1000.0, 1000.0]),
    ];
    for o in all() {
        for (lo, hi) in views {
            let (x0, x1, y0, y1) = o.visible_cells(g(), lo, hi);
            for cy in 0..g().height {
                for cx in 0..g().width {
                    let [ox, oy] = o.cell_origin(g(), cx, cy);
                    let hit = ox < hi[0] && ox + 32.0 > lo[0] && oy < hi[1] && oy + 16.0 > lo[1];
                    if hit {
                        assert!(
                            cx >= x0 && cx < x1 && cy >= y0 && cy < y1,
                            "{o:?} view {lo:?}..{hi:?}: cell ({cx}, {cy}) outside {x0}..{x1} x {y0}..{y1}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn hostile_inputs_give_none() {
    for o in all() {
        assert_eq!(o.cell_at(g(), [f64::NAN, 0.0]), None);
        assert_eq!(o.cell_at(g(), [f64::INFINITY, 0.0]), None);
        assert_eq!(o.cell_at(g(), [-5.0, -5.0]), None);
        assert_eq!(o.cell_at(g(), [1e12, 1e12]), None);
        let empty = GridSize { width: 0, ..g() };
        assert_eq!(o.cell_at(empty, [1.0, 1.0]), None);
    }
}

/// The cells reachable through `neighbor_cells`, as `(slot, x, y)` for the slots that exist.
fn slots(o: Orientation, x: u32, y: u32) -> Vec<(usize, i64, i64)> {
    o.neighbor_cells(x, y)
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.map(|(nx, ny)| (i, nx, ny)))
        .collect()
}

#[test]
fn neighbor_sets_table_for_odd_and_even_rows_and_columns() {
    let sy = |index| Orientation::Hexagonal {
        axis: StaggerAxis::Y,
        index,
        side_length: 8,
    };
    let sx = |index| Orientation::Hexagonal {
        axis: StaggerAxis::X,
        index,
        side_length: 8,
    };
    // Pointy hexagons, odd rows shifted right: no N or S; slots NW NE W E SW SE.
    let pointy_odd = |x: i64, y: i64, shifted: bool| {
        let (w, e) = if shifted { (0, 1) } else { (-1, 0) };
        vec![
            (0, x + w, y - 1),
            (2, x + e, y - 1),
            (3, x - 1, y),
            (4, x, y),
            (5, x + 1, y),
            (6, x + w, y + 1),
            (8, x + e, y + 1),
        ]
    };
    assert_eq!(
        slots(sy(StaggerIndex::Odd), 3, 3),
        pointy_odd(3, 3, true),
        "odd row"
    );
    assert_eq!(
        slots(sy(StaggerIndex::Odd), 3, 2),
        pointy_odd(3, 2, false),
        "even row"
    );
    assert_eq!(
        slots(sy(StaggerIndex::Even), 3, 2),
        pointy_odd(3, 2, true),
        "even index, even row"
    );
    assert_eq!(
        slots(sy(StaggerIndex::Even), 3, 3),
        pointy_odd(3, 3, false),
        "even index, odd row"
    );
    // Flat hexagons, odd columns shifted down: no W or E; slots NW N NE SW S SE.
    let flat = |x: i64, y: i64, shifted: bool| {
        let (u, d) = if shifted { (0, 1) } else { (-1, 0) };
        vec![
            (0, x - 1, y + u),
            (1, x, y - 1),
            (2, x + 1, y + u),
            (4, x, y),
            (6, x - 1, y + d),
            (7, x, y + 1),
            (8, x + 1, y + d),
        ]
    };
    assert_eq!(slots(sx(StaggerIndex::Odd), 3, 3), flat(3, 3, true), "odd column");
    assert_eq!(
        slots(sx(StaggerIndex::Odd), 2, 3),
        flat(2, 3, false),
        "even column"
    );
    assert_eq!(slots(sx(StaggerIndex::Even), 2, 3), flat(2, 3, true));
    // Orthogonal: the 3 x 3 block.
    assert_eq!(slots(Orientation::Orthogonal, 2, 2).len(), 9);
    // Staggered diamonds (axis Y, odd): 8 neighbors, N and S two rows away.
    let st = Orientation::Staggered {
        axis: StaggerAxis::Y,
        index: StaggerIndex::Odd,
    };
    let n = st.neighbor_cells(3, 3);
    assert_eq!(n[1], Some((3, 1)));
    assert_eq!(n[7], Some((3, 5)));
    assert_eq!(n[0], Some((3, 2)));
    assert_eq!(n[2], Some((4, 2)));
    assert_eq!(n[3], Some((2, 3)));
    // Axis X: W and E two columns away.
    let st = Orientation::Staggered {
        axis: StaggerAxis::X,
        index: StaggerIndex::Even,
    };
    let n = st.neighbor_cells(2, 3);
    assert_eq!((n[3], n[5]), (Some((0, 3)), Some((4, 3))));
    assert_eq!((n[0], n[6]), (Some((1, 3)), Some((1, 4))));
}

#[test]
fn neighbor_sets_are_symmetric_and_touch_on_screen() {
    for o in all() {
        for y in 0..g().height {
            for x in 0..g().width {
                for (slot, nx, ny) in slots(o, x, y) {
                    if slot == 4
                        || nx < 0
                        || ny < 0
                        || nx >= i64::from(g().width)
                        || ny >= i64::from(g().height)
                    {
                        continue;
                    }
                    // Symmetric: the neighbor lists this cell back, in the opposite slot.
                    let back = o.neighbor_cells(nx as u32, ny as u32);
                    assert_eq!(
                        back[8 - slot],
                        Some((i64::from(x), i64::from(y))),
                        "{o:?} ({x},{y}) slot {slot}"
                    );
                    if o.is_orthogonal() || o == Orientation::Isometric {
                        continue;
                    }
                    // On screen the centers are close: at most one cell box away on each axis
                    // (two cells for the straight diamond neighbors).
                    let a = o.cell_center(g(), x, y);
                    let b = o.cell_center(g(), nx as u32, ny as u32);
                    assert!(
                        (a[0] - b[0]).abs() <= 2.0 * 32.0 && (a[1] - b[1]).abs() <= 2.0 * 16.0,
                        "{o:?}"
                    );
                    assert!(a != b);
                }
            }
        }
    }
}
