// SPDX-License-Identifier: MPL-2.0
//! 5×7 bitmap font for contact-sheet captions.
//!
//! The glyphs are drawn for this tool (uppercase, digits, and the punctuation that
//! appears in repo paths and the stats line). Captions are folded to ASCII uppercase
//! at draw time; `index.md` keeps the original path and commit text.

use image::{Rgba, RgbaImage};

/// Pixel width of one glyph, not counting the gap before the next glyph.
pub(crate) const GLYPH_W: u32 = 5;
/// Pixel height of one glyph.
pub(crate) const GLYPH_H: u32 = 7;

/// Horizontal step from one character to the next, in pixels, at `scale`.
pub(crate) fn advance(scale: u32) -> u32 {
    (GLYPH_W + 1).saturating_mul(scale.max(1))
}

/// Draw `text` with the top-left of the first glyph at `(x, y)`.
///
/// Pixels that fall outside `img` are skipped. ASCII letters are drawn as uppercase.
/// A character with no glyph is a hollow box, so a missing mark stays visible.
pub(crate) fn draw_text(img: &mut RgbaImage, x: i64, y: i64, text: &str, color: Rgba<u8>, scale: u32) {
    let scale = scale.max(1);
    let step = i64::from(advance(scale));
    let mut cursor = x;
    for ch in text.chars() {
        let rows = glyph(ch);
        for (gy, bits) in rows.iter().enumerate() {
            for gx in 0..GLYPH_W {
                let shift = GLYPH_W - 1 - gx;
                if bits & (1u8 << shift) == 0 {
                    continue;
                }
                let px = cursor + i64::from(gx) * i64::from(scale);
                let py = y + i64::try_from(gy).unwrap_or(0) * i64::from(scale);
                fill_block(img, px, py, scale, color);
            }
        }
        cursor = cursor.saturating_add(step);
    }
}

/// Width of `text` in pixels at `scale`, including the gap after the last glyph.
pub(crate) fn text_width(text: &str, scale: u32) -> u32 {
    let n = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
    n.saturating_mul(advance(scale))
}

fn fill_block(img: &mut RgbaImage, x: i64, y: i64, scale: u32, color: Rgba<u8>) {
    let width = i64::from(img.width());
    let height = i64::from(img.height());
    for dy in 0..i64::from(scale) {
        for dx in 0..i64::from(scale) {
            let px = x + dx;
            let py = y + dy;
            if (0..width).contains(&px) && (0..height).contains(&py) {
                let Ok(px) = u32::try_from(px) else { continue };
                let Ok(py) = u32::try_from(py) else { continue };
                img.put_pixel(px, py, color);
            }
        }
    }
}

fn glyph(ch: char) -> [u8; 7] {
    let ch = ch.to_ascii_uppercase();
    if ch == ' ' {
        return [0; 7];
    }
    for &(name, rows) in GLYPHS {
        if name == ch {
            return pack(rows);
        }
    }
    BOX
}

fn pack(rows: [&str; 7]) -> [u8; 7] {
    let mut out = [0u8; 7];
    for (y, row) in rows.iter().enumerate() {
        let bytes = row.as_bytes();
        let mut bits = 0u8;
        for x in 0..GLYPH_W {
            if bytes.get(usize::try_from(x).unwrap_or(0)).copied() == Some(b'#') {
                bits |= 1u8 << (GLYPH_W - 1 - x);
            }
        }
        out[y] = bits;
    }
    out
}

const BOX: [u8; 7] = [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111];

/// `.` is off, `#` is on. Each row is five columns.
const GLYPHS: &[(char, [&str; 7])] = &[
    (
        'A',
        [".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
    ),
    (
        'B',
        ["####.", "#...#", "#...#", "####.", "#...#", "#...#", "####."],
    ),
    (
        'C',
        [".###.", "#...#", "#....", "#....", "#....", "#...#", ".###."],
    ),
    (
        'D',
        ["####.", "#...#", "#...#", "#...#", "#...#", "#...#", "####."],
    ),
    (
        'E',
        ["#####", "#....", "#....", "####.", "#....", "#....", "#####"],
    ),
    (
        'F',
        ["#####", "#....", "#....", "####.", "#....", "#....", "#...."],
    ),
    (
        'G',
        [".###.", "#...#", "#....", "#.###", "#...#", "#...#", ".###."],
    ),
    (
        'H',
        ["#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
    ),
    (
        'I',
        ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "#####"],
    ),
    (
        'J',
        ["..###", "...#.", "...#.", "...#.", "#..#.", "#..#.", ".##.."],
    ),
    (
        'K',
        ["#...#", "#..#.", "#.#..", "##...", "#.#..", "#..#.", "#...#"],
    ),
    (
        'L',
        ["#....", "#....", "#....", "#....", "#....", "#....", "#####"],
    ),
    (
        'M',
        ["#...#", "##.##", "#.#.#", "#...#", "#...#", "#...#", "#...#"],
    ),
    (
        'N',
        ["#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#"],
    ),
    (
        'O',
        [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
    ),
    (
        'P',
        ["####.", "#...#", "#...#", "####.", "#....", "#....", "#...."],
    ),
    (
        'Q',
        [".###.", "#...#", "#...#", "#...#", "#.#.#", "#..#.", ".##.#"],
    ),
    (
        'R',
        ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
    ),
    (
        'S',
        [".####", "#....", "#....", ".###.", "....#", "....#", "####."],
    ),
    (
        'T',
        ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."],
    ),
    (
        'U',
        ["#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
    ),
    (
        'V',
        ["#...#", "#...#", "#...#", "#...#", "#...#", ".#.#.", "..#.."],
    ),
    (
        'W',
        ["#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#"],
    ),
    (
        'X',
        ["#...#", "#...#", ".#.#.", "..#..", ".#.#.", "#...#", "#...#"],
    ),
    (
        'Y',
        ["#...#", "#...#", ".#.#.", "..#..", "..#..", "..#..", "..#.."],
    ),
    (
        'Z',
        ["#####", "....#", "...#.", "..#..", ".#...", "#....", "#####"],
    ),
    (
        '0',
        [".###.", "#..##", "#.#.#", "#.#.#", "##..#", "#...#", ".###."],
    ),
    (
        '1',
        ["..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###."],
    ),
    (
        '2',
        [".###.", "#...#", "....#", "..##.", ".#...", "#....", "#####"],
    ),
    (
        '3',
        [".###.", "#...#", "....#", "..##.", "....#", "#...#", ".###."],
    ),
    (
        '4',
        ["...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#."],
    ),
    (
        '5',
        ["#####", "#....", "####.", "....#", "....#", "#...#", ".###."],
    ),
    (
        '6',
        ["..##.", ".#...", "#....", "####.", "#...#", "#...#", ".###."],
    ),
    (
        '7',
        ["#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#..."],
    ),
    (
        '8',
        [".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###."],
    ),
    (
        '9',
        [".###.", "#...#", "#...#", ".####", "....#", "...#.", ".##.."],
    ),
    (
        '.',
        [".....", ".....", ".....", ".....", ".....", "..#..", "..#.."],
    ),
    (
        '_',
        [".....", ".....", ".....", ".....", ".....", ".....", "#####"],
    ),
    (
        '-',
        [".....", ".....", ".....", "#####", ".....", ".....", "....."],
    ),
    (
        '/',
        ["....#", "...#.", "...#.", "..#..", ".#...", ".#...", "#...."],
    ),
    (
        '\\',
        ["#....", ".#...", ".#...", "..#..", "...#.", "...#.", "....#"],
    ),
    (
        '|',
        ["..#..", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."],
    ),
    (
        ':',
        [".....", "..#..", "..#..", ".....", "..#..", "..#..", "....."],
    ),
    (
        '+',
        [".....", "..#..", "..#..", "#####", "..#..", "..#..", "....."],
    ),
    (
        '(',
        ["..##.", ".#...", "#....", "#....", "#....", ".#...", "..##."],
    ),
    (
        ')',
        [".##..", "...#.", "....#", "....#", "....#", "...#.", ".##.."],
    ),
    (
        '%',
        ["#...#", "#..#.", "...#.", "..#..", ".#...", ".#..#", "#...#"],
    ),
    (
        '#',
        [".#.#.", "#####", ".#.#.", ".#.#.", "#####", ".#.#.", "....."],
    ),
    (
        ',',
        [".....", ".....", ".....", ".....", "..#..", "..#..", ".#..."],
    ),
    (
        '>',
        ["....#", "...#.", "..#..", ".#...", "..#..", "...#.", "....#"],
    ),
    (
        '=',
        [".....", "#####", ".....", "#####", ".....", ".....", "....."],
    ),
    (
        '\'',
        ["..#..", "..#..", ".#...", ".....", ".....", ".....", "....."],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_rows_are_five_marks() {
        for (ch, rows) in GLYPHS {
            for row in rows {
                assert_eq!(row.chars().count(), 5, "{ch} row {row}");
                assert!(row.chars().all(|c| c == '.' || c == '#'), "{ch} {row}");
            }
        }
    }

    #[test]
    fn uppercase_t_spans_the_top_row_and_lowercase_matches() {
        let mut img = RgbaImage::new(16, 16);
        draw_text(&mut img, 0, 0, "T", Rgba([9, 0, 0, 255]), 1);
        for x in 0..GLYPH_W {
            assert_eq!(img.get_pixel(x, 0).0[0], 9, "x={x}");
        }
        let mut lower = RgbaImage::new(16, 16);
        draw_text(&mut lower, 0, 0, "t", Rgba([9, 0, 0, 255]), 1);
        assert_eq!(img.as_raw(), lower.as_raw());
    }

    #[test]
    fn text_clips_instead_of_panicking() {
        let mut img = RgbaImage::new(4, 4);
        draw_text(&mut img, -8, -8, "HELLO", Rgba([255, 255, 255, 255]), 2);
        draw_text(&mut img, 100, 100, "HELLO", Rgba([255, 255, 255, 255]), 2);
    }
}
