// SPDX-License-Identifier: MPL-2.0
//! The per-view contact sheet: header lines, then a grid of labelled tiles of one size, in the
//! `golden-sheets` style (dark background, 5×7 captions). [`layout`] is the pure geometry;
//! [`compose`] draws it.

use image::{Rgba, RgbaImage, imageops};

use crate::golden_sheets::font;

/// Outer margin, pixels.
pub const MARGIN: u32 = 16;
/// Gap between tiles, pixels.
pub const GAP: u32 = 10;
/// Caption scale (5×7 glyphs drawn at 2×).
pub const SCALE: u32 = 2;
/// Height of one caption line, pixels.
pub const LINE: u32 = font::GLYPH_H * SCALE + 6;

const BG: Rgba<u8> = Rgba([22, 24, 28, 255]);
const TEXT: Rgba<u8> = Rgba([236, 236, 232, 255]);
const DIM: Rgba<u8> = Rgba([168, 176, 186, 255]);

/// Where one tile and its caption go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// Left edge of the tile and its caption.
    pub x: u32,
    /// Top of the caption.
    pub label_y: u32,
    /// Top of the tile.
    pub y: u32,
}

/// The geometry of a sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SheetLayout {
    /// Sheet width.
    pub width: u32,
    /// Sheet height.
    pub height: u32,
    /// Size every tile is drawn at (the image scaled down to the column width, aspect kept,
    /// never scaled up).
    pub tile: (u32, u32),
    /// Height of the header band.
    pub header_h: u32,
    /// Cells, row-major.
    pub cells: Vec<Cell>,
}

/// Lays out `cols × rows` tiles of an `image`-sized picture under `header_lines` lines of text,
/// each tile at most `max_tile_w` wide. `None` when any input is zero.
#[must_use]
pub fn layout(
    image: (u32, u32),
    cols: u32,
    rows: u32,
    max_tile_w: u32,
    header_lines: u32,
) -> Option<SheetLayout> {
    let (w, h) = image;
    if w == 0 || h == 0 || cols == 0 || rows == 0 || max_tile_w == 0 {
        return None;
    }
    let tile_w = w.min(max_tile_w);
    let tile_h = ((u64::from(h) * u64::from(tile_w) + u64::from(w) / 2) / u64::from(w)).max(1) as u32;
    let header_h = MARGIN / 2 + header_lines * LINE + 4;
    let row_h = LINE + tile_h + GAP;
    let width = 2 * MARGIN + cols * tile_w + (cols - 1) * GAP;
    let height = header_h + rows * row_h + MARGIN / 2;
    let mut cells = Vec::with_capacity((cols * rows) as usize);
    for r in 0..rows {
        for c in 0..cols {
            let label_y = header_h + r * row_h;
            cells.push(Cell {
                x: MARGIN + c * (tile_w + GAP),
                label_y,
                y: label_y + LINE,
            });
        }
    }
    Some(SheetLayout {
        width,
        height,
        tile: (tile_w, tile_h),
        header_h,
        cells,
    })
}

/// One tile: a caption and tightly packed RGBA8 pixels.
#[derive(Clone, Copy, Debug)]
pub struct Tile<'a> {
    /// Caption (drawn uppercase).
    pub label: &'a str,
    /// Pixels, `size.0 × size.1 × 4` bytes.
    pub rgba: &'a [u8],
    /// Width and height.
    pub size: (u32, u32),
}

fn fit(text: &str, width: u32) -> String {
    let max = (width / font::advance(SCALE)).max(1) as usize;
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars()
            .take(max.saturating_sub(1))
            .chain(std::iter::once('~'))
            .collect()
    }
}

/// Draws a sheet: `header` lines, then `tiles` row-major in `cols` columns. Every tile is scaled
/// to the first tile's layout size; a tile whose pixel count does not match its size is drawn as
/// an empty cell. `None` when there is no tile or a size is zero.
#[must_use]
pub fn compose(header: &[String], tiles: &[Tile<'_>], cols: u32, max_tile_w: u32) -> Option<RgbaImage> {
    let first = tiles.first()?;
    let n = u32::try_from(tiles.len()).ok()?;
    let cols = cols.min(n).max(1);
    let rows = n.div_ceil(cols);
    let lay = layout(
        first.size,
        cols,
        rows,
        max_tile_w,
        u32::try_from(header.len()).ok()?,
    )?;
    let mut sheet = RgbaImage::from_pixel(lay.width, lay.height, BG);
    for (i, line) in header.iter().enumerate() {
        let y = MARGIN / 2 + u32::try_from(i).unwrap_or(0) * LINE;
        let color = if i == 0 { TEXT } else { DIM };
        font::draw_text(
            &mut sheet,
            i64::from(MARGIN),
            i64::from(y),
            &fit(line, lay.width - 2 * MARGIN),
            color,
            SCALE,
        );
    }
    for (tile, cell) in tiles.iter().zip(&lay.cells) {
        font::draw_text(
            &mut sheet,
            i64::from(cell.x),
            i64::from(cell.label_y),
            &fit(tile.label, lay.tile.0),
            TEXT,
            SCALE,
        );
        let Some(img) = RgbaImage::from_raw(tile.size.0, tile.size.1, tile.rgba.to_vec()) else {
            continue;
        };
        let scaled = if img.dimensions() == lay.tile {
            img
        } else {
            imageops::resize(&img, lay.tile.0, lay.tile.1, imageops::FilterType::Triangle)
        };
        imageops::replace(&mut sheet, &scaled, i64::from(cell.x), i64::from(cell.y));
    }
    Some(sheet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_shrink_to_the_column_width_and_keep_their_aspect() {
        let l = layout((640, 360), 4, 2, 320, 2).expect("layout");
        assert_eq!(l.tile, (320, 180));
        assert_eq!(l.width, 2 * MARGIN + 4 * 320 + 3 * GAP);
        assert_eq!(l.cells.len(), 8);
        let l = layout((333, 200), 3, 1, 320, 1).expect("layout");
        assert_eq!(l.tile, (320, 192));
        let l = layout((100, 50), 2, 1, 320, 0).expect("layout");
        assert_eq!(l.tile, (100, 50), "never scaled up");
    }

    #[test]
    fn cells_are_inside_the_sheet_and_do_not_overlap() {
        for (size, cols, rows, header) in [((640, 360), 4, 2, 2), ((64, 48), 3, 3, 0), ((1, 1000), 5, 1, 4)] {
            let l = layout(size, cols, rows, 320, header).expect("layout");
            let (tw, th) = l.tile;
            for c in &l.cells {
                assert!(c.label_y >= l.header_h, "captions below the header");
                assert_eq!(c.y, c.label_y + LINE);
                assert!(c.x + tw <= l.width - MARGIN + 1 && c.y + th <= l.height, "{l:?}");
            }
            for (i, a) in l.cells.iter().enumerate() {
                for b in &l.cells[i + 1..] {
                    let apart_x = a.x + tw <= b.x || b.x + tw <= a.x;
                    let apart_y = a.label_y + LINE + th <= b.label_y || b.label_y + LINE + th <= a.label_y;
                    assert!(apart_x || apart_y, "{a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn zero_sizes_have_no_layout() {
        assert_eq!(layout((0, 10), 1, 1, 320, 0), None);
        assert_eq!(layout((10, 10), 0, 1, 320, 0), None);
        assert_eq!(layout((10, 10), 1, 0, 320, 0), None);
        assert_eq!(layout((10, 10), 1, 1, 0, 0), None);
    }

    #[test]
    fn compose_draws_a_sheet_of_the_layout_size() {
        let px = vec![200u8; 8 * 6 * 4];
        let tiles = [
            Tile {
                label: "baked",
                rgba: &px,
                size: (8, 6),
            },
            Tile {
                label: "reference",
                rgba: &px,
                size: (8, 6),
            },
            Tile {
                label: "short buffer",
                rgba: &px[..10],
                size: (8, 6),
            },
        ];
        let header = vec!["cornell / front".to_string()];
        let sheet = compose(&header, &tiles, 2, 320).expect("sheet");
        let l = layout((8, 6), 2, 2, 320, 1).expect("layout");
        assert_eq!(sheet.dimensions(), (l.width, l.height));
        let c = l.cells[1];
        assert_eq!(sheet.get_pixel(c.x + 1, c.y + 1).0, [200, 200, 200, 200]);
        assert_eq!(compose(&header, &[], 2, 320), None);
    }
}
