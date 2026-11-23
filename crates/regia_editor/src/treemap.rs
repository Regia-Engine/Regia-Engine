// SPDX-License-Identifier: MPL-2.0
//! Squarified treemap layout (Bruls, Huizing, van Wijk, "Squarified Treemaps", 2000).
//!
//! Sizes go in descending order. Rows are laid along the shorter side of the free rectangle: an
//! item joins the current row while that does not worsen the row's worst aspect ratio, else the
//! row is fixed and the rest continues in the rectangle left over. Pure geometry, no egui.

/// A cell of the layout, in the units of the rectangle given to [`squarify`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    /// Index of the size this cell shows.
    pub index: usize,
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Cell {
    /// Whether the point is inside (right and bottom edges excluded, so a point is in one cell).
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// Longer side over shorter side.
    #[must_use]
    pub fn aspect(&self) -> f32 {
        (self.w / self.h).max(self.h / self.w)
    }
}

/// The worst aspect ratio of a row of `areas` laid along a side of length `side`.
fn worst(areas: &[f64], side: f64) -> f64 {
    let sum: f64 = areas.iter().sum();
    let (Some(max), Some(min)) = (
        areas.iter().copied().reduce(f64::max),
        areas.iter().copied().reduce(f64::min),
    ) else {
        return f64::INFINITY;
    };
    if sum <= 0.0 || min <= 0.0 {
        return f64::INFINITY;
    }
    let side2 = side * side;
    let sum2 = sum * sum;
    (side2 * max / sum2).max(sum2 / (side2 * min))
}

/// Lays `sizes` out in the `w` × `h` rectangle at the origin: one [`Cell`] per positive size, areas
/// proportional to the sizes. Zero and non-finite sizes get no cell. The result only depends on
/// the input: ties keep the input order.
#[must_use]
pub fn squarify(sizes: &[u64], w: f32, h: f32) -> Vec<Cell> {
    let total: f64 = sizes.iter().map(|s| *s as f64).sum();
    if total <= 0.0 || !(w > 0.0 && h > 0.0) || !w.is_finite() || !h.is_finite() {
        return Vec::new();
    }
    let scale = f64::from(w) * f64::from(h) / total;
    let mut order: Vec<usize> = (0..sizes.len()).filter(|i| sizes[*i] > 0).collect();
    order.sort_by_key(|i| (std::cmp::Reverse(sizes[*i]), *i));

    let (mut x, mut y, mut free_w, mut free_h) = (0.0_f64, 0.0_f64, f64::from(w), f64::from(h));
    let mut cells = Vec::with_capacity(order.len());
    let mut next = 0;
    while next < order.len() {
        let side = free_w.min(free_h);
        let mut row: Vec<f64> = vec![sizes[order[next]] as f64 * scale];
        let mut end = next + 1;
        while end < order.len() {
            let area = sizes[order[end]] as f64 * scale;
            let mut grown = row.clone();
            grown.push(area);
            if worst(&grown, side) > worst(&row, side) {
                break;
            }
            row = grown;
            end += 1;
        }
        // Fix the row: it takes a strip of the free rectangle across its shorter side.
        let sum: f64 = row.iter().sum();
        let thickness = sum / side;
        let along_width = free_w <= free_h;
        let mut offset = 0.0;
        for (slot, area) in order[next..end].iter().zip(&row) {
            let length = area / thickness;
            let (cx, cy, cw, ch) = if along_width {
                (x + offset, y, length, thickness)
            } else {
                (x, y + offset, thickness, length)
            };
            cells.push(Cell {
                index: *slot,
                x: cx as f32,
                y: cy as f32,
                w: cw as f32,
                h: ch as f32,
            });
            offset += length;
        }
        if along_width {
            y += thickness;
            free_h = (free_h - thickness).max(0.0);
        } else {
            x += thickness;
            free_w = (free_w - thickness).max(0.0);
        }
        next = end;
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example of the paper: 6, 6, 4, 3, 2, 2, 1 in a 6 × 4 rectangle.
    #[test]
    fn the_paper_example_lays_out_in_its_rows() {
        let cells = squarify(&[6, 6, 4, 3, 2, 2, 1], 6.0, 4.0);
        assert_eq!(cells.len(), 7);
        // The first row stacks the two sixes along the short side: 2 wide, 2 and 2 high... of 3.
        assert!(
            (cells[0].w - 3.0).abs() < 1e-4 && (cells[0].h - 2.0).abs() < 1e-4,
            "{:?}",
            cells[0]
        );
        assert!(
            (cells[1].w - 3.0).abs() < 1e-4 && (cells[1].h - 2.0).abs() < 1e-4,
            "{:?}",
            cells[1]
        );
        assert_eq!((cells[0].x, cells[1].x), (0.0, 0.0));
        assert!(cells.iter().all(|c| c.aspect() < 3.0), "{cells:?}");
    }

    #[test]
    fn areas_are_proportional_inside_the_rectangle_without_overlap() {
        let sizes = [1000, 500, 250, 125, 60, 30, 15, 7, 3, 1, 1, 1];
        let (w, h) = (640.0_f32, 360.0_f32);
        let cells = squarify(&sizes, w, h);
        assert_eq!(cells.len(), sizes.len());
        let total: u64 = sizes.iter().sum();
        for cell in &cells {
            let want = sizes[cell.index] as f32 / total as f32 * w * h;
            assert!((cell.w * cell.h - want).abs() / want < 1e-3, "{cell:?}");
            assert!(cell.x >= -1e-3 && cell.y >= -1e-3);
            assert!(
                cell.x + cell.w <= w + 1e-2 && cell.y + cell.h <= h + 1e-2,
                "{cell:?}"
            );
        }
        for (i, a) in cells.iter().enumerate() {
            for b in &cells[i + 1..] {
                let overlap_x = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
                let overlap_y = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
                assert!(overlap_x < 1e-2 || overlap_y < 1e-2, "{a:?} {b:?}");
            }
        }
    }

    #[test]
    fn every_point_is_in_one_cell() {
        let cells = squarify(&[8, 4, 2, 1, 1], 100.0, 100.0);
        for (x, y) in [(1.0, 1.0), (50.0, 50.0), (99.0, 99.0), (75.0, 25.0)] {
            assert_eq!(cells.iter().filter(|c| c.contains(x, y)).count(), 1, "({x}, {y})");
        }
    }

    #[test]
    fn empty_and_zero_sizes_get_no_cell() {
        assert!(squarify(&[], 10.0, 10.0).is_empty());
        assert!(squarify(&[0, 0], 10.0, 10.0).is_empty());
        assert!(squarify(&[5], 0.0, 10.0).is_empty());
        assert!(squarify(&[5], f32::NAN, 10.0).is_empty());
        let cells = squarify(&[0, 5, 0], 10.0, 10.0);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].index, 1);
        assert_eq!((cells[0].w, cells[0].h), (10.0, 10.0));
    }

    #[test]
    fn it_beats_a_plain_slice_on_the_worst_aspect_ratio() {
        let sizes: Vec<u64> = (1..=40).map(|n| n * n).collect();
        let (w, h) = (800.0, 300.0);
        let worst = squarify(&sizes, w, h)
            .iter()
            .map(Cell::aspect)
            .fold(0.0_f32, f32::max);
        // Slicing the long side gives the smallest item this ratio.
        let total: u64 = sizes.iter().sum();
        let sliced = h / (w * sizes[0] as f32 / total as f32);
        assert!(worst < sliced / 10.0, "{worst} against {sliced}");
    }

    #[test]
    fn equal_input_gives_equal_output() {
        let sizes = [5, 5, 5, 5, 3, 3];
        assert_eq!(squarify(&sizes, 90.0, 40.0), squarify(&sizes, 90.0, 40.0));
    }
}
