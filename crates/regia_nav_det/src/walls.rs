// SPDX-License-Identifier: MPL-2.0
//! Wall segments for the LP avoidance solver from the grid's blocked-cell boundary (ADR-8555).
//!
//! [`segments_from_grid_window`] turns the cells around a point into the straight boundary
//! runs between cells a profile can stand in and cells it cannot (or the map edge), in a fixed
//! order, ready for `regia_detsim::avoid::orca_velocity_obstacles_into` with
//! `OrcaSolver::Lp`. Every peer gets the same segments from the same grid state.

use regia_detsim::avoid::{OrcaSegment, m_to_mm};

use super::grid::{NavGrid, Profile};

/// The merged boundary segments between blocked and passable cells for `profile` in the
/// square window of half-size `r_m` meters around `center` (world XZ), written to `out`
/// (cleared first; its capacity is reused, so a warm vector allocates nothing).
///
/// A cell is blocked when [`NavGrid::passable`] is false or it lies off the grid, so the map
/// edge is a wall. The window covers the cells whose centers' nearest-cell coordinates fall in
/// `center ± r_m`, plus the boundary to the ring just outside it; a window partly or wholly
/// off the grid (including a center or half-size so large that the cell index saturates at the
/// `i32` bounds) gives only the boundaries on the grid and its edge, and none when it lies
/// entirely outside. Edges are cell sides (world
/// `origin + (i - 0.5) * cell_m`), converted with `m_to_mm`. Collinear neighboring edges merge
/// into one segment.
///
/// Order: first the vertical runs (constant X) by column boundary, then by Z; then the
/// horizontal runs (constant Z) by row boundary, then by X. Each segment goes from its lower to
/// its higher coordinate.
pub fn segments_from_grid_window(
    grid: &NavGrid,
    profile: Profile,
    center: [f32; 2],
    r_m: f32,
    out: &mut Vec<OrcaSegment>,
) {
    out.clear();
    let (x0, z0) = grid.cell_coords(center[0] - r_m, center[1] - r_m);
    let (x1, z1) = grid.cell_coords(center[0] + r_m, center[1] + r_m);
    let origin = grid.center(0);
    let cell = grid.cell_m();
    boundary_runs(
        [grid.width(), grid.height()],
        [x0, z0],
        [x1, z1],
        |cx, cz| !grid.passable(profile, grid.index(cx, cz)),
        // World coordinate (mm) of the side between cell k - 1 and cell k on an axis.
        |k| m_to_mm(origin[0] + (k as f32 - 0.5) * cell),
        |k| m_to_mm(origin[1] + (k as f32 - 0.5) * cell),
        out,
    );
}

/// The boundary runs of [`segments_from_grid_window`] on a `size[0]` x `size[1]` grid, for
/// the window of cell coordinates `lo..=hi` (as `NavGrid::cell_coords` gives them, possibly
/// saturated at the `i32` bounds). `blocked(cx, cz)` is asked only for on-grid cells; every
/// off-grid cell is blocked. `side_x(k)` / `side_z(k)` give the coordinate (mm) of the side
/// between cell `k - 1` and cell `k`. Appends to `out` in the documented order.
fn boundary_runs(
    size: [usize; 2],
    lo: [i32; 2],
    hi: [i32; 2],
    blocked: impl Fn(usize, usize) -> bool,
    side_x: impl Fn(i64) -> i64,
    side_z: impl Fn(i64) -> i64,
    out: &mut Vec<OrcaSegment>,
) {
    let ([x0, z0], [x1, z1]) = (lo, hi);
    if x1 < x0 || z1 < z0 {
        return;
    }
    // Window arithmetic is in `i64` (ADR-8804): a legal side reaches `i32::MAX`, so the ring
    // `w + 1` and the loop ends `x1 + 1` / `z1 + 1` need one more bit than `i32` has. A side
    // never exceeds `u32::MAX` (`validate` caps the cell count), so every value here is
    // exact, and the output for any window that fit `i32` is unchanged.
    let (w, h) = (size[0] as i64, size[1] as i64);
    let (x0, x1, z0, z1) = (i64::from(x0), i64::from(x1), i64::from(z0), i64::from(z1));
    // Clamp the window to the grid plus its one-cell ring (ADR-8703). Off the grid every
    // cell is blocked, so no boundary outside `0..=w` x `0..=h` is an edge and the output is
    // unchanged; the clamp bounds the work by the grid size for any window.
    let (x0, x1) = (x0.clamp(-1, w + 1), x1.clamp(-2, w));
    let (z0, z1) = (z0.clamp(-1, h + 1), z1.clamp(-2, h));
    let blocked = |cx: i64, cz: i64| {
        if cx < 0 || cz < 0 || cx >= w || cz >= h {
            return true;
        }
        blocked(cx as usize, cz as usize)
    };

    // Vertical runs: the boundary between columns i - 1 and i, along Z.
    for i in x0..=x1 + 1 {
        let x = side_x(i);
        let mut run: Option<i64> = None;
        for cz in z0..=z1 + 1 {
            let edge = cz <= z1 && blocked(i - 1, cz) != blocked(i, cz);
            match (edge, run) {
                (true, None) => run = Some(cz),
                (false, Some(start)) => {
                    out.push(OrcaSegment {
                        a_mm: [x, side_z(start)],
                        b_mm: [x, side_z(cz)],
                    });
                    run = None;
                }
                _ => {}
            }
        }
    }
    // Horizontal runs: the boundary between rows j - 1 and j, along X.
    for j in z0..=z1 + 1 {
        let z = side_z(j);
        let mut run: Option<i64> = None;
        for cx in x0..=x1 + 1 {
            let edge = cx <= x1 && blocked(cx, j - 1) != blocked(cx, j);
            match (edge, run) {
                (true, None) => run = Some(cx),
                (false, Some(start)) => {
                    out.push(OrcaSegment {
                        a_mm: [side_x(start), z],
                        b_mm: [side_x(cx), z],
                    });
                    run = None;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::grid::{GridSettings, NavStatic};

    /// A 16 x 16 grid of 1 m cells centered on the origin (cell `i` centered at `i - 8` m), with
    /// the given cells closed.
    fn grid(closed: &[(usize, usize)]) -> NavGrid {
        let settings = GridSettings {
            cell_m: 1.0,
            canvas_cells: [16, 16],
            ..GridSettings::hover_ground_jet()
        };
        let stat = Arc::new(NavStatic::build(None, Arc::new(settings)).expect("flat grid"));
        let mut mask = vec![0u8; 256];
        for &(x, z) in closed {
            mask[z * 16 + x] = 1;
        }
        NavGrid::with_closed_cells(stat, mask).expect("grid")
    }

    #[test]
    fn open_window_has_no_segments() {
        let g = grid(&[]);
        let mut out = vec![OrcaSegment {
            a_mm: [1, 1],
            b_mm: [2, 2],
        }];
        segments_from_grid_window(&g, Profile::FOOT, [0.0, 0.0], 3.0, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn a_wall_of_cells_becomes_two_merged_sides() {
        // Column 10 (center x = 2 m) closed for rows 6..=9 (centers z = -2..=1 m).
        let g = grid(&[(10, 6), (10, 7), (10, 8), (10, 9)]);
        let mut out = Vec::new();
        segments_from_grid_window(&g, Profile::FOOT, [0.0, 0.0], 4.0, &mut out);
        assert_eq!(
            out,
            vec![
                // West and east sides, each one merged run from z = -2.5 to 1.5 m.
                OrcaSegment {
                    a_mm: [1_500, -2_500],
                    b_mm: [1_500, 1_500]
                },
                OrcaSegment {
                    a_mm: [2_500, -2_500],
                    b_mm: [2_500, 1_500]
                },
                // South and north caps.
                OrcaSegment {
                    a_mm: [1_500, -2_500],
                    b_mm: [2_500, -2_500]
                },
                OrcaSegment {
                    a_mm: [1_500, 1_500],
                    b_mm: [2_500, 1_500]
                },
            ]
        );
    }

    #[test]
    fn grid_walls_keep_a_body_out_of_blocked_cells() {
        use regia_detsim::avoid::{
            OrcaBody, OrcaScratch, OrcaSettings, OrcaSolver, orca_velocity_obstacles_into,
        };
        // A wall of closed cells across x = 2 m (column 10), rows 4..=11.
        let closed: Vec<(usize, usize)> = (4..=11).map(|z| (10, z)).collect();
        let g = grid(&closed);
        let settings = OrcaSettings {
            solver: OrcaSolver::Lp,
            ..OrcaSettings::default()
        };
        let mut body = OrcaBody {
            id: 1,
            x_mm: -3_000,
            z_mm: 0,
            vx_mm_s: 0,
            vz_mm_s: 0,
            r_mm: 400,
        };
        let (mut scratch, mut segments) = (OrcaScratch::new(), Vec::new());
        let mut um = [body.x_mm * 1_000, body.z_mm * 1_000];
        for _ in 0..300 {
            let at = [body.x_mm as f32 / 1000.0, body.z_mm as f32 / 1000.0];
            segments_from_grid_window(&g, Profile::FOOT, at, 4.0, &mut segments);
            let v = orca_velocity_obstacles_into(&mut scratch, &body, [1_500, 0], &[], &segments, &settings);
            body.vx_mm_s = v[0];
            body.vz_mm_s = v[1];
            um[0] += v[0] * 20;
            um[1] += v[1] * 20;
            body.x_mm = um[0] / 1_000;
            body.z_mm = um[1] / 1_000;
            // The west face of the closed column is at x = 1.5 m.
            assert!(body.x_mm + body.r_mm <= 1_500 + 1, "into the wall: {body:?}");
        }
        assert!(body.x_mm > 0, "it still walked up to the wall: {body:?}");
    }

    #[test]
    fn the_map_edge_is_a_wall_and_capacity_is_reused() {
        let g = grid(&[]);
        let mut out = Vec::with_capacity(64);
        // Near the -X edge (cell 0 centered at -8 m, its west side at -8.5 m).
        segments_from_grid_window(&g, Profile::FOOT, [-7.0, 0.0], 2.0, &mut out);
        assert_eq!(
            out,
            vec![OrcaSegment {
                a_mm: [-8_500, -2_500],
                b_mm: [-8_500, 2_500]
            }]
        );
        let cap = out.capacity();
        segments_from_grid_window(&g, Profile::FOOT, [-7.0, 0.0], 2.0, &mut out);
        assert_eq!(out.capacity(), cap);
    }

    /// An L of blocked cells. Order is vertical runs by column then Z, then horizontal
    /// runs by row then X, and the closed-cell slice order does not change it (the
    /// helper reads a mask, not the insertion sequence).
    #[test]
    fn boundary_runs_follow_column_then_row_and_ignore_insertion_order() {
        let cells = [(10, 8), (11, 8), (10, 9)];
        let reversed = [cells[2], cells[1], cells[0]];
        let mut a = Vec::new();
        let mut b = Vec::new();
        segments_from_grid_window(&grid(&cells), Profile::FOOT, [0.0, 0.0], 4.0, &mut a);
        segments_from_grid_window(&grid(&reversed), Profile::FOOT, [0.0, 0.0], 4.0, &mut b);
        assert_eq!(a, b, "closed-cell insertion order changed the segments");
        assert_eq!(
            a,
            vec![
                // West face of column 10, rows 8 and 9 merged.
                OrcaSegment {
                    a_mm: [1_500, -500],
                    b_mm: [1_500, 1_500]
                },
                // East face of column 10 / west of the open cell beside row 9 only.
                OrcaSegment {
                    a_mm: [2_500, 500],
                    b_mm: [2_500, 1_500]
                },
                // East face of column 11, row 8 only.
                OrcaSegment {
                    a_mm: [3_500, -500],
                    b_mm: [3_500, 500]
                },
                // South of row 8, both columns merged.
                OrcaSegment {
                    a_mm: [1_500, -500],
                    b_mm: [3_500, -500]
                },
                // North of row 8 where row 9 is open.
                OrcaSegment {
                    a_mm: [2_500, 500],
                    b_mm: [3_500, 500]
                },
                // North of row 9.
                OrcaSegment {
                    a_mm: [1_500, 1_500],
                    b_mm: [2_500, 1_500]
                },
            ]
        );
    }

    /// Centers whose nearest cell saturates at the `i32` bounds (review finding 1). The
    /// window is clamped to the grid plus its one-cell ring, so a window entirely off the
    /// grid gives no segments and a window larger than the grid gives exactly the map edge
    /// and the blocked-cell boundary, the same as a window that just covers the grid.
    #[test]
    fn saturating_cell_index_does_not_panic() {
        let g = grid(&[(10, 8)]);
        let mut far = Vec::new();
        for center in [
            [1.0e20, 0.0],
            [-1.0e20, -1.0e20],
            [0.0, 1.0e20],
            [1.0e20, -1.0e20],
            [f32::INFINITY, 0.0],
            [0.0, f32::NEG_INFINITY],
        ] {
            far.push(OrcaSegment {
                a_mm: [9, 9],
                b_mm: [9, 9],
            });
            segments_from_grid_window(&g, Profile::FOOT, center, 2.0, &mut far);
            assert!(far.is_empty(), "off-grid window {center:?} gave {far:?}");
        }
        let mut covering = Vec::new();
        segments_from_grid_window(&g, Profile::FOOT, [0.0, 0.0], 20.0, &mut covering);
        // The map edge (four sides) plus the closed cell's four sides.
        assert_eq!(covering.len(), 8, "{covering:?}");
        let mut huge = Vec::new();
        for r_m in [1.0e20, f32::MAX, f32::INFINITY] {
            segments_from_grid_window(&g, Profile::FOOT, [0.0, 0.0], r_m, &mut huge);
            assert_eq!(huge, covering, "window half-size {r_m}");
        }
    }

    /// The largest legal canvas side is `i32::MAX` (`GridSettings::validate`: each side is at
    /// least 2 and the product fits `u32`). A window whose far bound saturates at
    /// `i32::MAX` there reaches the map edge `w` and the off-grid ring `w + 1`, so the clamp
    /// bound `w + 1` and the loop ends `x1 + 1` / `z1 + 1` must not overflow (det-orca-walls
    /// verification). Abstract all-open grids, so no `i32::MAX`-cell allocation: the far
    /// edge is the only wall, and the near rows/columns of the window close their caps.
    #[test]
    fn east_and_north_windows_on_a_max_canvas_reach_the_edge() {
        let max = i32::MAX;
        let open = |_: usize, _: usize| false;
        let mut out = Vec::new();
        // Width i32::MAX, height 2; window columns i32::MAX - 1 ..= i32::MAX (saturated).
        boundary_runs(
            [max as usize, 2],
            [max - 1, 0],
            [max, 1],
            open,
            |k| (k - i64::from(max)) * 1000,
            |k| k * 1000,
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                // The east map edge between column w - 1 and the off-grid column w.
                OrcaSegment {
                    a_mm: [0, 0],
                    b_mm: [0, 2_000]
                },
                // South and north map edges under the window's on-grid column w - 1.
                OrcaSegment {
                    a_mm: [-1_000, 0],
                    b_mm: [0, 0]
                },
                OrcaSegment {
                    a_mm: [-1_000, 2_000],
                    b_mm: [0, 2_000]
                },
            ]
        );
        // Width 2, height i32::MAX; window rows i32::MAX - 1 ..= i32::MAX (saturated).
        boundary_runs(
            [2, max as usize],
            [0, max - 1],
            [1, max],
            open,
            |k| k * 1000,
            |k| (k - i64::from(max)) * 1000,
            &mut out,
        );
        assert_eq!(
            out[3..],
            [
                // West and east map edges beside the window's on-grid row h - 1.
                OrcaSegment {
                    a_mm: [0, -1_000],
                    b_mm: [0, 0]
                },
                OrcaSegment {
                    a_mm: [2_000, -1_000],
                    b_mm: [2_000, 0]
                },
                // The north map edge between row h - 1 and the off-grid row h.
                OrcaSegment {
                    a_mm: [0, 0],
                    b_mm: [2_000, 0]
                },
            ]
        );
    }
}
