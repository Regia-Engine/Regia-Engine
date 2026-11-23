// SPDX-License-Identifier: MPL-2.0
//! Pixel difference and contact-sheet layout.
//!
//! `|diff|×8` is the per-channel absolute difference multiplied by [`DIFF_GAIN`] and clamped
//! to 255. A missing side is treated as zero, so an added or deleted image still has a diff.
//! Alpha-only changes are folded into the colour channels so they are not a black thumbnail.
//! When the diff is shrunk onto the sheet, each destination pixel keeps the brightest source
//! sample in its block: a one-pixel change stays visible.

use image::imageops::{self, FilterType};
use image::{Rgba, RgbaImage};

use super::font::{self, draw_text};

/// `|channel difference|` is multiplied by this and clamped to 255.
pub(crate) const DIFF_GAIN: u16 = 8;

/// Longest side of one panel (old, new, or diff), in pixels.
pub(crate) const PANEL_MAX_W: u32 = 360;
/// Tallest panel, in pixels.
pub(crate) const PANEL_MAX_H: u32 = 220;

const MARGIN: u32 = 16;
const GAP: u32 = 10;
const SCALE: u32 = 2;
const PAD_TOP: u32 = 8;

const BG: Rgba<u8> = Rgba([22, 24, 28, 255]);
const TEXT: Rgba<u8> = Rgba([236, 236, 232, 255]);
const DIM: Rgba<u8> = Rgba([168, 176, 186, 255]);
const CELL: Rgba<u8> = Rgba([12, 13, 16, 255]);
const FRAME: Rgba<u8> = Rgba([72, 80, 92, 255]);
const MISSING: Rgba<u8> = Rgba([64, 40, 44, 255]);

/// Width of every row and of the contact sheet.
pub(crate) const SHEET_WIDTH: u32 = MARGIN * 2 + PANEL_MAX_W * 3 + GAP * 2;

/// Counts and the amplified difference image for one golden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PixelStats {
    /// Pixels in the union rectangle with any RGBA channel differing.
    pub changed: u64,
    /// Width × height of the union rectangle.
    pub total: u64,
    /// Largest absolute channel delta before [`DIFF_GAIN`], 0–255.
    pub max_diff: u8,
}

/// One comparison row, ready to stack.
pub(crate) struct ComparisonRow {
    /// Full-width row, including the caption.
    pub image: RgbaImage,
    /// Difference counts for the index.
    pub stats: PixelStats,
    /// `old` dimensions, when that revision has a PNG.
    pub old_size: Option<(u32, u32)>,
    /// `new` dimensions, when that revision has a PNG.
    pub new_size: Option<(u32, u32)>,
}

/// Inputs of one row. Images are borrowed; the row owns its rendered pixels.
pub(crate) struct Comparison<'a> {
    /// 1-based index across the whole run, drawn on the row.
    pub index: usize,
    /// Repo-relative path (the new path, or the only path).
    pub path: &'a str,
    /// `added`, `modified`, `deleted`, `renamed`, …
    pub status: &'a str,
    /// Image at `--from`. `None` when the path is absent there.
    pub old: Option<&'a RgbaImage>,
    /// Image at `--to`. `None` when the path is absent there.
    pub new: Option<&'a RgbaImage>,
}

/// Absolute per-channel difference over the union rectangle.
///
/// A side that has no pixel at `(x, y)` contributes `(0, 0, 0, 0)`. `max_diff` is the raw
/// delta; the returned image is the gain-amplified colour, with alpha folded in and stored
/// opaque.
pub(crate) fn abs_diff(old: Option<&RgbaImage>, new: Option<&RgbaImage>) -> (RgbaImage, PixelStats) {
    let (width, height) = union_size(old, new);
    let mut diff = RgbaImage::new(width, height);
    let mut changed = 0u64;
    let mut max_diff = 0u8;
    for y in 0..height {
        for x in 0..width {
            let a = sample(old, x, y);
            let b = sample(new, x, y);
            let delta = [
                a[0].abs_diff(b[0]),
                a[1].abs_diff(b[1]),
                a[2].abs_diff(b[2]),
                a[3].abs_diff(b[3]),
            ];
            let mut peak = 0u8;
            for channel in delta {
                peak = peak.max(channel);
            }
            if peak > 0 {
                changed += 1;
            }
            max_diff = max_diff.max(peak);
            let gained = [
                amplify(delta[0]),
                amplify(delta[1]),
                amplify(delta[2]),
                amplify(delta[3]),
            ];
            // An alpha-only edit would otherwise be a black pixel on the sheet.
            let alpha = gained[3];
            diff.put_pixel(
                x,
                y,
                Rgba([
                    gained[0].max(alpha),
                    gained[1].max(alpha),
                    gained[2].max(alpha),
                    255,
                ]),
            );
        }
    }
    let total = u64::from(width) * u64::from(height);
    (
        diff,
        PixelStats {
            changed,
            total,
            max_diff,
        },
    )
}

/// Render `old | new | |diff|×8` with the path, the changed-pixel count, and the max delta.
pub(crate) fn render_comparison(cmp: &Comparison<'_>) -> ComparisonRow {
    let (diff, stats) = abs_diff(cmp.old, cmp.new);
    let old_size = cmp.old.map(|img| img.dimensions());
    let new_size = cmp.new.map(|img| img.dimensions());
    let (union_w, union_h) = union_size(cmp.old, cmp.new);
    let (_, fit_h) = fitted_size(union_w, union_h, PANEL_MAX_W, PANEL_MAX_H);
    let panel_h = fit_h.max(font::GLYPH_H * SCALE + 8);

    let line = font::GLYPH_H * SCALE + 6;
    let height = PAD_TOP + line * 3 + panel_h + 12;
    let mut row = RgbaImage::from_pixel(SHEET_WIDTH, height, BG);

    let path = ellipsize(
        &format!("{:02}  {}", cmp.index, cmp.path),
        max_text_chars(SHEET_WIDTH),
    );
    draw_text(
        &mut row,
        i64::from(MARGIN),
        i64::from(PAD_TOP),
        &path,
        TEXT,
        SCALE,
    );
    let stats_text = ellipsize(
        &stats_line(cmp.status, old_size, new_size, stats.changed, stats.max_diff),
        max_text_chars(SHEET_WIDTH),
    );
    draw_text(
        &mut row,
        i64::from(MARGIN),
        i64::from(PAD_TOP + line),
        &stats_text,
        DIM,
        SCALE,
    );

    let panels_y = PAD_TOP + line * 3;
    let slots: [(&str, Panel<'_>); 3] = [
        ("old", panel_source(cmp.old)),
        ("new", panel_source(cmp.new)),
        ("|diff|x8", Panel::Diff(&diff)),
    ];
    for (i, (label, source)) in slots.into_iter().enumerate() {
        let x = MARGIN + (PANEL_MAX_W + GAP) * u32::try_from(i).unwrap_or(0);
        let label_w = font::text_width(label, SCALE);
        let label_x = i64::from(x) + i64::from(PANEL_MAX_W.saturating_sub(label_w)) / 2;
        draw_text(
            &mut row,
            label_x,
            i64::from(PAD_TOP + line * 2),
            label,
            DIM,
            SCALE,
        );
        let cell = match source {
            Panel::Image(img) => place_in_cell(img, PANEL_MAX_W, panel_h, false),
            Panel::Diff(img) => place_in_cell(img, PANEL_MAX_W, panel_h, true),
            Panel::Missing => missing_cell(PANEL_MAX_W, panel_h),
        };
        imageops::replace(&mut row, &cell, i64::from(x), i64::from(panels_y));
        stroke_rect(&mut row, x, panels_y, PANEL_MAX_W, panel_h, FRAME);
    }
    stroke_rect(&mut row, 0, height.saturating_sub(1), SHEET_WIDTH, 1, FRAME);
    ComparisonRow {
        image: row,
        stats,
        old_size,
        new_size,
    }
}

enum Panel<'a> {
    Image(&'a RgbaImage),
    Diff(&'a RgbaImage),
    Missing,
}

fn panel_source(img: Option<&RgbaImage>) -> Panel<'_> {
    match img {
        Some(img) => Panel::Image(img),
        None => Panel::Missing,
    }
}

/// Stack `rows` under `header`. Rows are already [`SHEET_WIDTH`] wide.
pub(crate) fn compose_sheet(header: &str, rows: &[RgbaImage]) -> RgbaImage {
    let line = font::GLYPH_H * SCALE + 6;
    let header_h = PAD_TOP + line + 4;
    let rows_h = rows
        .iter()
        .fold(0u32, |acc, row| acc.saturating_add(row.height()));
    let mut sheet = RgbaImage::from_pixel(SHEET_WIDTH, header_h.saturating_add(rows_h), BG);
    let shown = ellipsize(header, max_text_chars(SHEET_WIDTH));
    draw_text(
        &mut sheet,
        i64::from(MARGIN),
        i64::from(PAD_TOP),
        &shown,
        TEXT,
        SCALE,
    );
    let mut y = header_h;
    for row in rows {
        imageops::replace(&mut sheet, row, 0, i64::from(y));
        y = y.saturating_add(row.height());
    }
    sheet
}

/// Half-open `[start, end)` ranges of at most `per` rows. `per == 0` yields nothing.
pub(crate) fn page_bounds(n: usize, per: usize) -> Vec<(usize, usize)> {
    if n == 0 || per == 0 {
        return Vec::new();
    }
    let mut pages = Vec::new();
    let mut start = 0;
    while start < n {
        let end = (start + per).min(n);
        pages.push((start, end));
        start = end;
    }
    pages
}

/// Keep the tail of `text` so a long path still shows its file name.
pub(crate) fn ellipsize(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 2 {
        return ".".repeat(max_chars);
    }
    let keep = max_chars - 2;
    let tail: String = text
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("..{tail}")
}

/// `changed / total` as a percentage with one decimal place, truncating.
pub(crate) fn percent(changed: u64, total: u64) -> String {
    if total == 0 {
        return "0.0%".to_string();
    }
    let tenths = changed.saturating_mul(1000) / total;
    format!("{}.{}%", tenths / 10, tenths % 10)
}

fn stats_line(
    status: &str,
    old: Option<(u32, u32)>,
    new: Option<(u32, u32)>,
    changed: u64,
    max_diff: u8,
) -> String {
    let dims = match (old, new) {
        (Some(a), Some(b)) if a == b => format!("{}x{}", a.0, a.1),
        (Some(a), Some(b)) => format!("{}x{} -> {}x{}", a.0, a.1, b.0, b.1),
        (Some(a), None) | (None, Some(a)) => format!("{}x{}", a.0, a.1),
        (None, None) => "none".to_string(),
    };
    format!("{status}  {dims}  changed {changed}  max {max_diff}")
}

fn union_size(old: Option<&RgbaImage>, new: Option<&RgbaImage>) -> (u32, u32) {
    match (old, new) {
        (Some(a), Some(b)) => (a.width().max(b.width()), a.height().max(b.height())),
        (Some(img), None) | (None, Some(img)) => img.dimensions(),
        (None, None) => (1, 1),
    }
}

fn sample(img: Option<&RgbaImage>, x: u32, y: u32) -> [u8; 4] {
    match img {
        Some(img) if x < img.width() && y < img.height() => img.get_pixel(x, y).0,
        _ => [0, 0, 0, 0],
    }
}

fn amplify(delta: u8) -> u8 {
    let scaled = u16::from(delta).saturating_mul(DIFF_GAIN);
    if scaled > 255 {
        255
    } else {
        u8::try_from(scaled).unwrap_or(255)
    }
}

/// Fit `(w, h)` inside the box. Images that already fit scale up by an integer factor
/// (nearest-neighbour, so a tiny golden stays crisp). Larger images scale down uniformly.
pub(crate) fn fitted_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 || max_w == 0 || max_h == 0 {
        return (1, 1);
    }
    if w <= max_w && h <= max_h {
        let factor = (max_w / w).max(1).min((max_h / h).max(1));
        return (
            w.saturating_mul(factor).min(max_w),
            h.saturating_mul(factor).min(max_h),
        );
    }
    let scale = (f64::from(max_w) / f64::from(w)).min(f64::from(max_h) / f64::from(h));
    let nw = round_px(f64::from(w) * scale, max_w);
    let nh = round_px(f64::from(h) * scale, max_h);
    (nw, nh)
}

fn round_px(value: f64, max: u32) -> u32 {
    if !value.is_finite() || value <= 1.0 {
        return 1;
    }
    let capped = value.min(f64::from(max));
    if capped >= f64::from(max) {
        return max;
    }
    let rounded = capped.round();
    if rounded <= 1.0 {
        1
    } else if rounded >= f64::from(max) {
        max
    } else {
        // `rounded` is in (1, max). Truncation toward zero matches that range.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let px = rounded as u32;
        px.clamp(1, max)
    }
}

fn place_in_cell(src: &RgbaImage, cell_w: u32, cell_h: u32, max_pool: bool) -> RgbaImage {
    let flat;
    let source = if max_pool {
        src
    } else {
        flat = flatten_checker(src);
        &flat
    };
    let (fw, fh) = fitted_size(source.width(), source.height(), cell_w, cell_h);
    let scaled = scale_image(source, fw, fh, max_pool);
    let mut cell = RgbaImage::from_pixel(cell_w, cell_h, CELL);
    let x = i64::from(cell_w.saturating_sub(scaled.width())) / 2;
    let y = i64::from(cell_h.saturating_sub(scaled.height())) / 2;
    imageops::replace(&mut cell, &scaled, x, y);
    cell
}

fn missing_cell(cell_w: u32, cell_h: u32) -> RgbaImage {
    let mut cell = RgbaImage::from_pixel(cell_w, cell_h, MISSING);
    let label = "missing";
    let tw = font::text_width(label, SCALE);
    let x = i64::from(cell_w.saturating_sub(tw)) / 2;
    let y = i64::from(cell_h.saturating_sub(font::GLYPH_H * SCALE)) / 2;
    draw_text(&mut cell, x, y, label, TEXT, SCALE);
    cell
}

fn flatten_checker(src: &RgbaImage) -> RgbaImage {
    let mut out = RgbaImage::new(src.width(), src.height());
    for (x, y, pixel) in src.enumerate_pixels() {
        let bg: u8 = if (x / 8 + y / 8).is_multiple_of(2) { 42 } else { 68 };
        let alpha = u32::from(pixel[3]);
        let mix = |channel: u8| -> u8 {
            let blended = u32::from(channel) * alpha + u32::from(bg) * (255 - alpha);
            u8::try_from((blended + 127) / 255).unwrap_or(255)
        };
        out.put_pixel(x, y, Rgba([mix(pixel[0]), mix(pixel[1]), mix(pixel[2]), 255]));
    }
    out
}

fn scale_image(src: &RgbaImage, width: u32, height: u32, max_pool: bool) -> RgbaImage {
    if src.width() == width && src.height() == height {
        return src.clone();
    }
    let shrinking = width < src.width() || height < src.height();
    if max_pool && shrinking {
        return downsample_max(src, width, height);
    }
    let filter = if shrinking {
        FilterType::Triangle
    } else {
        FilterType::Nearest
    };
    imageops::resize(src, width, height, filter)
}

/// Each destination pixel is the per-channel max of the source rectangle it covers.
pub(crate) fn downsample_max(src: &RgbaImage, dst_w: u32, dst_h: u32) -> RgbaImage {
    let mut dst = RgbaImage::new(dst_w.max(1), dst_h.max(1));
    let src_w = src.width().max(1);
    let src_h = src.height().max(1);
    let dst_w = dst.width();
    let dst_h = dst.height();
    for y in 0..dst_h {
        let y0 = u32::try_from(u64::from(y) * u64::from(src_h) / u64::from(dst_h)).unwrap_or(0);
        let y1 = u32::try_from(u64::from(y + 1) * u64::from(src_h) / u64::from(dst_h))
            .unwrap_or(src_h)
            .max(y0 + 1)
            .min(src_h);
        for x in 0..dst_w {
            let x0 = u32::try_from(u64::from(x) * u64::from(src_w) / u64::from(dst_w)).unwrap_or(0);
            let x1 = u32::try_from(u64::from(x + 1) * u64::from(src_w) / u64::from(dst_w))
                .unwrap_or(src_w)
                .max(x0 + 1)
                .min(src_w);
            let mut max = [0u8; 4];
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let pixel = src.get_pixel(xx, yy).0;
                    for channel in 0..4 {
                        max[channel] = max[channel].max(pixel[channel]);
                    }
                }
            }
            dst.put_pixel(x, y, Rgba(max));
        }
    }
    dst
}

fn stroke_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    if w == 0 || h == 0 {
        return;
    }
    let x1 = x.saturating_add(w - 1);
    let y1 = y.saturating_add(h - 1);
    for px in x..=x1 {
        put(img, px, y, color);
        put(img, px, y1, color);
    }
    for py in y..=y1 {
        put(img, x, py, color);
        put(img, x1, py, color);
    }
}

fn put(img: &mut RgbaImage, x: u32, y: u32, color: Rgba<u8>) {
    if x < img.width() && y < img.height() {
        img.put_pixel(x, y, color);
    }
}

fn max_text_chars(width: u32) -> usize {
    let step = font::advance(SCALE).max(1);
    let inner = width.saturating_sub(MARGIN * 2);
    usize::try_from(inner / step).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, pixel: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(pixel))
    }

    #[test]
    fn gain_is_eight_and_saturates() {
        assert_eq!(DIFF_GAIN, 8);
        let old = solid(2, 1, [0, 0, 0, 255]);
        let mut new = solid(2, 1, [0, 0, 0, 255]);
        new.put_pixel(0, 0, Rgba([1, 0, 0, 255]));
        new.put_pixel(1, 0, Rgba([40, 0, 0, 255]));
        let (diff, stats) = abs_diff(Some(&old), Some(&new));
        assert_eq!(stats.changed, 2);
        assert_eq!(stats.max_diff, 40);
        assert_eq!(stats.total, 2);
        assert_eq!(diff.get_pixel(0, 0).0, [8, 0, 0, 255]);
        assert_eq!(diff.get_pixel(1, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn identical_images_have_a_black_diff() {
        let img = solid(2, 2, [10, 20, 30, 255]);
        let (diff, stats) = abs_diff(Some(&img), Some(&img));
        assert_eq!(stats.changed, 0);
        assert_eq!(stats.max_diff, 0);
        assert!(diff.pixels().all(|p| p.0 == [0, 0, 0, 255]));
    }

    #[test]
    fn alpha_only_change_is_visible() {
        let old = solid(1, 1, [0, 0, 0, 255]);
        let new = solid(1, 1, [0, 0, 0, 250]);
        let (diff, stats) = abs_diff(Some(&old), Some(&new));
        assert_eq!(stats.changed, 1);
        assert_eq!(stats.max_diff, 5);
        assert_eq!(diff.get_pixel(0, 0).0, [40, 40, 40, 255]);
    }

    #[test]
    fn size_mismatch_counts_pixels_the_other_side_lacks() {
        let old = solid(1, 1, [0, 0, 0, 255]);
        let new = solid(2, 1, [0, 0, 0, 255]);
        let (diff, stats) = abs_diff(Some(&old), Some(&new));
        assert_eq!(diff.dimensions(), (2, 1));
        assert_eq!(stats.total, 2);
        assert_eq!(stats.changed, 1);
        assert_eq!(stats.max_diff, 255);
    }

    #[test]
    fn added_image_diffs_against_zero() {
        let new = solid(2, 1, [255, 0, 0, 255]);
        let (_, stats) = abs_diff(None, Some(&new));
        assert_eq!(stats.changed, 2);
        assert_eq!(stats.max_diff, 255);
    }

    #[test]
    fn max_pool_keeps_a_single_bright_pixel() {
        let mut src = solid(4, 2, [0, 0, 0, 255]);
        src.put_pixel(3, 1, Rgba([0, 8, 0, 255]));
        let dst = downsample_max(&src, 2, 1);
        assert_eq!(dst.dimensions(), (2, 1));
        assert_eq!(dst.get_pixel(0, 0).0[1], 0);
        assert_eq!(dst.get_pixel(1, 0).0[1], 8);
    }

    #[test]
    fn fitted_size_upsamples_by_an_integer_and_downsamples_inside_the_box() {
        assert_eq!(fitted_size(64, 36, 360, 220), (320, 180));
        let (w, h) = fitted_size(640, 360, 360, 220);
        assert_eq!(w, 360);
        assert!(h <= 220);
        assert_eq!(fitted_size(512, 512, 360, 220), (220, 220));
    }

    #[test]
    fn triptych_is_a_full_width_row_with_the_diff_stats() {
        let old = solid(2, 2, [255, 0, 0, 255]);
        let mut new = old.clone();
        new.put_pixel(1, 1, Rgba([255, 1, 0, 255]));
        let row = render_comparison(&Comparison {
            index: 3,
            path: "crates/demo/goldens/box.png",
            status: "modified",
            old: Some(&old),
            new: Some(&new),
        });
        assert_eq!(row.image.width(), SHEET_WIDTH);
        assert!(row.image.height() > PANEL_MAX_H);
        assert_eq!(row.stats.changed, 1);
        assert_eq!(row.stats.max_diff, 1);
        assert_eq!(row.old_size, Some((2, 2)));
    }

    #[test]
    fn missing_side_does_not_panic_and_counts_every_pixel() {
        let new = solid(2, 2, [0, 0, 255, 255]);
        let row = render_comparison(&Comparison {
            index: 1,
            path: "a/goldens/a.png",
            status: "added",
            old: None,
            new: Some(&new),
        });
        assert_eq!(row.stats.changed, 4);
        assert_eq!(row.old_size, None);
        assert_eq!(row.new_size, Some((2, 2)));
        assert!(row.image.pixels().any(|p| p.0[0] > 200));
    }

    #[test]
    fn thirteen_rows_fill_two_sheets_of_at_most_twelve() {
        let pages = page_bounds(13, 12);
        assert_eq!(pages, vec![(0, 12), (12, 13)]);
        assert_eq!(page_bounds(12, 12), vec![(0, 12)]);
        assert!(page_bounds(0, 12).is_empty());
        assert!(page_bounds(4, 0).is_empty());
        let row = RgbaImage::from_pixel(SHEET_WIDTH, 10, Rgba([1, 2, 3, 255]));
        let rows = vec![row; 13];
        let first = compose_sheet("h", &rows[pages[0].0..pages[0].1]);
        let second = compose_sheet("h", &rows[pages[1].0..pages[1].1]);
        assert!(first.height() > second.height());
        assert_eq!(first.width(), SHEET_WIDTH);
    }

    #[test]
    fn ellipsize_keeps_the_tail_and_percent_truncates() {
        assert_eq!(ellipsize("abcdefghij", 6), "..ghij");
        assert_eq!(ellipsize("abc", 6), "abc");
        assert_eq!(percent(1, 64), "1.5%");
        assert_eq!(percent(0, 10), "0.0%");
        assert_eq!(percent(10, 10), "100.0%");
        assert_eq!(percent(1, 0), "0.0%");
    }
}
