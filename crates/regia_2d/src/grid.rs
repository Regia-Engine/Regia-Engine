// SPDX-License-Identifier: MPL-2.0
//! Map-orientation geometry (`docs/specs/86-2d-toolkit.md` § 12, ADR-2597): where a cell of an
//! [`Orientation`] sits in the map, which cell is under a point, how big the map is and in which
//! order cells draw.
//!
//! Everything here works in **map pixels, +Y down, origin at the top-left of the map's bounding
//! box** (Tiled's convention); [`crate::tilemap::TileMapAsset`] converts to world space (+Y up,
//! origin bottom-left). Arithmetic is `f64`, so every column of a legal map is exact.
//!
//! - Orthogonal: cell `(x, y)` has its box at `(x·tw, y·th)`.
//! - Isometric (diamond): the box is at `((x − y + h − 1)·tw/2, (x + y)·th/2)`; the map is
//!   `(w + h)·tw/2` by `(w + h)·th/2`.
//! - Staggered and hexagonal, stagger axis Y (rows): box at `(x·tw + shift·tw/2, y·(th + s)/2)`
//!   where `s` is the hexagon's side length (0 for staggered diamonds) and `shift` is 1 on the
//!   staggered rows. Axis X (columns) is the same with the roles of x and y swapped.
//!
//! The geometry follows the public Tiled map format documentation (orientation, `staggeraxis`,
//! `staggerindex`, `hexsidelength`); the hit test is original (see `docs/provenance.md`).

use crate::tilemap::{Orientation, StaggerAxis, StaggerIndex};

/// The size parameters every conversion needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// Cell box width in pixels.
    pub tile_width: u32,
    /// Cell box height in pixels.
    pub tile_height: u32,
}

/// A cell outline in map pixels (+Y down). `len` is 4 (rectangle or diamond) or 6 (hexagon);
/// only the first `len` entries of `points` are live. The rest are zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellOutline {
    /// Clockwise vertices, starting at the top (or top-left for a rectangle).
    pub points: [[f64; 2]; 6],
    /// 4 or 6.
    pub len: usize,
}

/// True when index `i` is one of the shifted rows or columns.
fn shifted(index: StaggerIndex, i: u32) -> bool {
    match index {
        StaggerIndex::Odd => !i.is_multiple_of(2),
        StaggerIndex::Even => i.is_multiple_of(2),
    }
}

impl Orientation {
    /// True for the plain square grid.
    #[must_use]
    pub fn is_orthogonal(self) -> bool {
        matches!(self, Self::Orthogonal)
    }

    /// The stagger parameters `(axis, index, side_length)` of a staggered or hexagonal map; a
    /// staggered isometric map is a hexagonal one with side length 0.
    fn stagger(self) -> Option<(StaggerAxis, StaggerIndex, f64)> {
        match self {
            Self::Staggered { axis, index } => Some((axis, index, 0.0)),
            Self::Hexagonal {
                axis,
                index,
                side_length,
            } => Some((axis, index, f64::from(side_length))),
            Self::Orthogonal | Self::Isometric => None,
        }
    }

    /// Stable lowercase name for the agent protocol and Tiled: `orthogonal`, `isometric`,
    /// `staggered`, `hexagonal`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Orthogonal => "orthogonal",
            Self::Isometric => "isometric",
            Self::Staggered { .. } => "staggered",
            Self::Hexagonal { .. } => "hexagonal",
        }
    }

    /// Map size in pixels.
    #[must_use]
    pub fn pixel_size(self, g: GridSize) -> [f64; 2] {
        let (w, h) = (f64::from(g.width), f64::from(g.height));
        let (tw, th) = (f64::from(g.tile_width), f64::from(g.tile_height));
        match self {
            Self::Orthogonal => [w * tw, h * th],
            Self::Isometric => [(w + h) * tw / 2.0, (w + h) * th / 2.0],
            _ => {
                let Some((axis, index, side)) = self.stagger() else {
                    return [w * tw, h * th];
                };
                // The half-cell overhang exists only when a shifted row or column exists.
                let any_shifted = |count: u32| match index {
                    StaggerIndex::Odd => count > 1,
                    StaggerIndex::Even => count > 0,
                };
                match axis {
                    StaggerAxis::Y => [
                        w * tw + if any_shifted(g.height) { tw / 2.0 } else { 0.0 },
                        (h - 1.0).max(0.0) * (th + side) / 2.0 + th,
                    ],
                    StaggerAxis::X => [
                        (w - 1.0).max(0.0) * (tw + side) / 2.0 + tw,
                        h * th + if any_shifted(g.width) { th / 2.0 } else { 0.0 },
                    ],
                }
            }
        }
    }

    /// Top-left corner of the bounding box of cell `(x, y)` in map pixels (+Y down).
    #[must_use]
    pub fn cell_origin(self, g: GridSize, x: u32, y: u32) -> [f64; 2] {
        let (fx, fy) = (f64::from(x), f64::from(y));
        let (tw, th) = (f64::from(g.tile_width), f64::from(g.tile_height));
        match self {
            Self::Orthogonal => [fx * tw, fy * th],
            Self::Isometric => [
                (fx - fy + f64::from(g.height) - 1.0) * tw / 2.0,
                (fx + fy) * th / 2.0,
            ],
            _ => {
                let Some((axis, index, side)) = self.stagger() else {
                    return [fx * tw, fy * th];
                };
                match axis {
                    StaggerAxis::Y => [
                        fx * tw + if shifted(index, y) { tw / 2.0 } else { 0.0 },
                        fy * (th + side) / 2.0,
                    ],
                    StaggerAxis::X => [
                        fx * (tw + side) / 2.0,
                        fy * th + if shifted(index, x) { th / 2.0 } else { 0.0 },
                    ],
                }
            }
        }
    }

    /// Center of cell `(x, y)` in map pixels (+Y down).
    #[must_use]
    pub fn cell_center(self, g: GridSize, x: u32, y: u32) -> [f64; 2] {
        let [ox, oy] = self.cell_origin(g, x, y);
        [
            ox + f64::from(g.tile_width) / 2.0,
            oy + f64::from(g.tile_height) / 2.0,
        ]
    }

    /// The outline of cell `(x, y)` in map pixels (+Y down), clockwise from the top-left, for
    /// drawing the grid and the hover cell: 4 points for a rectangle or diamond, 6 for a hexagon.
    ///
    /// The points live on the stack ([`CellOutline`]). [`Self::cell_outline`] copies them into a
    /// `Vec` for callers that want an owned list.
    #[must_use]
    pub fn cell_outline_points(self, g: GridSize, x: u32, y: u32) -> CellOutline {
        let [ox, oy] = self.cell_origin(g, x, y);
        let (tw, th) = (f64::from(g.tile_width), f64::from(g.tile_height));
        let mut points = [[0.0; 2]; 6];
        let len = match self {
            Self::Orthogonal => {
                points[0] = [ox, oy];
                points[1] = [ox + tw, oy];
                points[2] = [ox + tw, oy + th];
                points[3] = [ox, oy + th];
                4
            }
            Self::Isometric | Self::Staggered { .. } => {
                points[0] = [ox + tw / 2.0, oy];
                points[1] = [ox + tw, oy + th / 2.0];
                points[2] = [ox + tw / 2.0, oy + th];
                points[3] = [ox, oy + th / 2.0];
                4
            }
            Self::Hexagonal {
                axis, side_length, ..
            } => {
                let s = f64::from(side_length);
                match axis {
                    // Pointy: top and bottom vertices, vertical side edges of length `s`.
                    StaggerAxis::Y => {
                        let cut = ((th - s) / 2.0).max(0.0);
                        points[0] = [ox + tw / 2.0, oy];
                        points[1] = [ox + tw, oy + cut];
                        points[2] = [ox + tw, oy + cut + s];
                        points[3] = [ox + tw / 2.0, oy + th];
                        points[4] = [ox, oy + cut + s];
                        points[5] = [ox, oy + cut];
                    }
                    // Flat: left and right vertices, horizontal top and bottom edges.
                    StaggerAxis::X => {
                        let cut = ((tw - s) / 2.0).max(0.0);
                        points[0] = [ox + cut, oy];
                        points[1] = [ox + cut + s, oy];
                        points[2] = [ox + tw, oy + th / 2.0];
                        points[3] = [ox + cut + s, oy + th];
                        points[4] = [ox + cut, oy + th];
                        points[5] = [ox, oy + th / 2.0];
                    }
                }
                6
            }
        };
        CellOutline { points, len }
    }

    /// The outline of cell `(x, y)` in map pixels (+Y down), clockwise from the top-left: 4 points
    /// for a rectangle or diamond, 6 for a hexagon.
    ///
    /// Prefer [`Self::cell_outline_points`] on a per-cell loop. This allocates a `Vec`.
    #[must_use]
    pub fn cell_outline(self, g: GridSize, x: u32, y: u32) -> Vec<[f64; 2]> {
        let outline = self.cell_outline_points(g, x, y);
        outline.points[..outline.len].to_vec()
    }

    /// Margin by which `p` lies inside the cell shape centered at `c` (>= 0 inside, < 0 outside;
    /// larger is deeper). Only meaningful for the staggered and hexagonal shapes.
    fn shape_margin(self, g: GridSize, c: [f64; 2], p: [f64; 2]) -> f64 {
        let (tw, th) = (f64::from(g.tile_width), f64::from(g.tile_height));
        let (dx, dy) = ((p[0] - c[0]).abs(), (p[1] - c[1]).abs());
        let Some((axis, _, side)) = self.stagger() else {
            return (tw / 2.0 - dx).min(th / 2.0 - dy);
        };
        match axis {
            StaggerAxis::Y => {
                let ymax = side.min(th) / 2.0 + (th - side.min(th)) / 2.0 * (1.0 - dx / (tw / 2.0));
                ((tw / 2.0 - dx) / tw).min((ymax - dy) / th)
            }
            StaggerAxis::X => {
                let xmax = side.min(tw) / 2.0 + (tw - side.min(tw)) / 2.0 * (1.0 - dy / (th / 2.0));
                ((th / 2.0 - dy) / th).min((xmax - dx) / tw)
            }
        }
    }

    /// The cell under map-pixel point `p` (+Y down, origin top-left of the map box), or `None`
    /// outside the map or in a gap between cells (the corners of a staggered or hexagonal map's
    /// bounding box). Points on a shared edge go to one of the two cells, always the same one.
    #[must_use]
    pub fn cell_at(self, g: GridSize, p: [f64; 2]) -> Option<(u32, u32)> {
        // Far past any legal map (at most 2^32 pixels): refuse before the i64 row math.
        const FAR: f64 = 1.0e13;
        if !(p[0].abs() <= FAR && p[1].abs() <= FAR) || g.width == 0 || g.height == 0 {
            return None;
        }
        let (tw, th) = (f64::from(g.tile_width), f64::from(g.tile_height));
        let inside =
            |x: f64, y: f64| x >= 0.0 && y >= 0.0 && x < f64::from(g.width) && y < f64::from(g.height);
        match self {
            Self::Orthogonal => {
                let (cx, cy) = ((p[0] / tw).floor(), (p[1] / th).floor());
                inside(cx, cy).then_some((cx as u32, cy as u32))
            }
            Self::Isometric => {
                // Rotate into the diamond lattice: u runs along +x − y, v along +x + y.
                let u = (p[0] - f64::from(g.height) * tw / 2.0) / (tw / 2.0);
                let v = p[1] / (th / 2.0);
                let (cx, cy) = (((u + v) / 2.0).floor(), ((v - u) / 2.0).floor());
                inside(cx, cy).then_some((cx as u32, cy as u32))
            }
            _ => {
                let (axis, index, side) = self.stagger()?;
                // Candidate cells: the few rows (or columns) whose boxes can reach `p`, and the
                // neighbouring columns (or rows) of each; the deepest containing one wins.
                let (step_main, size_main) = match axis {
                    StaggerAxis::Y => ((th + side) / 2.0, th),
                    StaggerAxis::X => ((tw + side) / 2.0, tw),
                };
                let main_p = match axis {
                    StaggerAxis::Y => p[1],
                    StaggerAxis::X => p[0],
                };
                let hi = (main_p / step_main).floor() as i64;
                let lo = ((main_p - size_main) / step_main).floor() as i64;
                let mut best: Option<((u32, u32), f64)> = None;
                for main in lo..=hi + 1 {
                    let Ok(main_u) = u32::try_from(main) else { continue };
                    let cross_size = match axis {
                        StaggerAxis::Y => tw,
                        StaggerAxis::X => th,
                    };
                    let cross_p = match axis {
                        StaggerAxis::Y => p[0],
                        StaggerAxis::X => p[1],
                    };
                    let off = if shifted(index, main_u) {
                        cross_size / 2.0
                    } else {
                        0.0
                    };
                    let guess = ((cross_p - off) / cross_size).floor() as i64;
                    for cross in guess - 1..=guess + 1 {
                        let Ok(cross_u) = u32::try_from(cross) else {
                            continue;
                        };
                        let (cx, cy) = match axis {
                            StaggerAxis::Y => (cross_u, main_u),
                            StaggerAxis::X => (main_u, cross_u),
                        };
                        if cx >= g.width || cy >= g.height {
                            continue;
                        }
                        let margin = self.shape_margin(g, self.cell_center(g, cx, cy), p);
                        if best.is_none_or(|(_, m)| margin > m) {
                            best = Some(((cx, cy), margin));
                        }
                    }
                }
                best.and_then(|(cell, margin)| (margin >= -1e-9).then_some(cell))
            }
        }
    }

    /// Visits the cells of the rectangle `x0..x1` by `y0..y1` back to front, so a tile image
    /// taller or wider than its cell overlaps the cells behind it correctly. Orthogonal,
    /// isometric and row-staggered maps go row by row, left to right (cell `(x, y)` is behind
    /// `(x + 1, y)` and `(x, y + 1)`); a column-staggered map goes by the screen height of the
    /// cell's top edge, then left to right. `visit` returns `false` to stop.
    pub fn draw_order(
        self,
        g: GridSize,
        x0: u32,
        x1: u32,
        y0: u32,
        y1: u32,
        mut visit: impl FnMut(u32, u32) -> bool,
    ) {
        let (x1, y1) = (x1.min(g.width), y1.min(g.height));
        match self.stagger() {
            Some((StaggerAxis::X, index, _)) => {
                // Key 2y + shift is the cell's top edge in half-rows; ascending key is back to front.
                let lo = y0.saturating_mul(2);
                let hi = y1.saturating_mul(2).saturating_add(1);
                for key in lo..hi {
                    for x in x0..x1 {
                        let shift = u32::from(shifted(index, x));
                        if key < shift || !(key - shift).is_multiple_of(2) {
                            continue;
                        }
                        let y = (key - shift) / 2;
                        if y >= y0 && y < y1 && !visit(x, y) {
                            return;
                        }
                    }
                }
            }
            _ => {
                for y in y0..y1 {
                    for x in x0..x1 {
                        if !visit(x, y) {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Converts a point in the map's **object space** (the coordinates Tiled stores for objects and
    /// that `MapObject::position` keeps) to map pixels (+Y down, origin at the top-left of the map's
    /// box, the space of [`Orientation::cell_origin`]).
    ///
    /// Orthogonal, staggered and hexagonal maps store objects in map pixels already. An isometric
    /// map stores them in the unrotated grid, measured in pixels of the tile *height* per cell:
    /// `(u, v)` is the grid point `(u / th, v / th)` and lands on the screen at
    /// `((gx − gy)·tw/2 + h·tw/2, (gx + gy)·th/2)`. The mapping is affine, so a polygon's absolute
    /// points convert one by one.
    #[must_use]
    pub fn object_to_map_pixels(self, g: GridSize, p: [f64; 2]) -> [f64; 2] {
        match self {
            Self::Isometric => {
                let (tw, th) = (f64::from(g.tile_width), f64::from(g.tile_height));
                if th <= 0.0 {
                    return p;
                }
                let (gx, gy) = (p[0] / th, p[1] / th);
                [
                    (gx - gy) * tw / 2.0 + f64::from(g.height) * tw / 2.0,
                    (gx + gy) * th / 2.0,
                ]
            }
            _ => p,
        }
    }

    /// The inverse of [`Orientation::object_to_map_pixels`].
    #[must_use]
    pub fn map_pixels_to_object(self, g: GridSize, p: [f64; 2]) -> [f64; 2] {
        match self {
            Self::Isometric => {
                let (tw, th) = (f64::from(g.tile_width), f64::from(g.tile_height));
                if tw <= 0.0 || th <= 0.0 {
                    return p;
                }
                // sx = (gx − gy)·tw/2, sy = (gx + gy)·th/2 with the origin shift removed.
                let sx = (p[0] - f64::from(g.height) * tw / 2.0) * 2.0 / tw;
                let sy = p[1] * 2.0 / th;
                [(sy + sx) / 2.0 * th, (sy - sx) / 2.0 * th]
            }
            _ => p,
        }
    }

    /// The cells that touch cell `(x, y)` on screen, in the 3 × 3 pattern order used by the
    /// auto-tile rules (`NW N NE / W C E / SW S SE`, index 4 is the cell itself), as signed grid
    /// coordinates (they can fall outside the map). `None` marks a slot with no neighbor.
    ///
    /// - Orthogonal and isometric: the 8 grid neighbors (on an isometric map the compass is the
    ///   grid's, which is rotated 45° on screen).
    /// - Staggered (and hexagonal with side length 0): the four screen-diagonal neighbors are the
    ///   cells that share an edge (their grid offset depends on whether the row, or column for axis
    ///   X, is shifted); the four straight neighbors are two rows (axis Y: `N`, `S`) or two columns
    ///   (axis X: `W`, `E`) away; the other two straight ones are the adjacent cell in the row (or
    ///   column).
    /// - Hexagonal: the six cells that share an edge. Axis Y (pointy-topped) has no `N` or `S`; axis
    ///   X (flat-topped) has no `W` or `E`.
    #[must_use]
    pub fn neighbor_cells(self, x: u32, y: u32) -> [Option<(i64, i64)>; 9] {
        let (cx, cy) = (i64::from(x), i64::from(y));
        let at = |dx: i64, dy: i64| Some((cx + dx, cy + dy));
        let Some((axis, index, side)) = self.stagger() else {
            return [
                at(-1, -1),
                at(0, -1),
                at(1, -1),
                at(-1, 0),
                at(0, 0),
                at(1, 0),
                at(-1, 1),
                at(0, 1),
                at(1, 1),
            ];
        };
        let diamond = side <= 0.0;
        match axis {
            StaggerAxis::Y => {
                // A shifted row sits half a cell to the right of its neighbors above and below.
                let (west, east) = if shifted(index, y) { (0, 1) } else { (-1, 0) };
                [
                    at(west, -1),
                    if diamond { at(0, -2) } else { None },
                    at(east, -1),
                    at(-1, 0),
                    at(0, 0),
                    at(1, 0),
                    at(west, 1),
                    if diamond { at(0, 2) } else { None },
                    at(east, 1),
                ]
            }
            StaggerAxis::X => {
                // A shifted column sits half a cell lower than its neighbors left and right.
                let (up, down) = if shifted(index, x) { (0, 1) } else { (-1, 0) };
                [
                    at(-1, up),
                    at(0, -1),
                    at(1, up),
                    if diamond { at(-2, 0) } else { None },
                    at(0, 0),
                    if diamond { at(2, 0) } else { None },
                    at(-1, down),
                    at(0, 1),
                    at(1, down),
                ]
            }
        }
    }

    /// A conservative cell rectangle `(x0, x1, y0, y1)` (end exclusive, clamped to the map) that
    /// covers every cell whose box intersects the map-pixel rectangle `lo..hi` (+Y down).
    #[must_use]
    pub fn visible_cells(self, g: GridSize, lo: [f64; 2], hi: [f64; 2]) -> (u32, u32, u32, u32) {
        let (tw, th) = (f64::from(g.tile_width), f64::from(g.tile_height));
        let clamp = |v: f64, n: u32| -> u32 {
            if v.is_nan() {
                0
            } else {
                (v.clamp(0.0, f64::from(n)) as u32).min(n)
            }
        };
        match self {
            Self::Orthogonal => (
                clamp((lo[0] / tw).floor(), g.width),
                clamp((hi[0] / tw).ceil(), g.width),
                clamp((lo[1] / th).floor(), g.height),
                clamp((hi[1] / th).ceil(), g.height),
            ),
            Self::Isometric => {
                // The inverse of the lattice is linear: the rectangle's corners bound the cells.
                let corners = [[lo[0], lo[1]], [hi[0], lo[1]], [lo[0], hi[1]], [hi[0], hi[1]]].map(|c| {
                    let u = (c[0] - f64::from(g.height) * tw / 2.0) / (tw / 2.0);
                    let v = c[1] / (th / 2.0);
                    [(u + v) / 2.0, (v - u) / 2.0]
                });
                let fold = |i: usize, f: fn(f64, f64) -> f64, init: f64| {
                    corners.iter().fold(init, |a, c| f(a, c[i]))
                };
                // A cell's own box reaches one cell further than its anchor point.
                (
                    clamp((fold(0, f64::min, f64::MAX) - 1.0).floor(), g.width),
                    clamp((fold(0, f64::max, f64::MIN) + 1.0).ceil(), g.width),
                    clamp((fold(1, f64::min, f64::MAX) - 1.0).floor(), g.height),
                    clamp((fold(1, f64::max, f64::MIN) + 1.0).ceil(), g.height),
                )
            }
            _ => {
                let (axis, _, side) = self.stagger().unwrap_or((StaggerAxis::Y, StaggerIndex::Odd, 0.0));
                let (sx, sy) = match axis {
                    StaggerAxis::Y => (tw, (th + side) / 2.0),
                    StaggerAxis::X => ((tw + side) / 2.0, th),
                };
                (
                    clamp(((lo[0] - 2.0 * tw) / sx).floor(), g.width),
                    clamp(((hi[0]) / sx).ceil() + 1.0, g.width),
                    clamp(((lo[1] - 2.0 * th) / sy).floor(), g.height),
                    clamp(((hi[1]) / sy).ceil() + 1.0, g.height),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests;
