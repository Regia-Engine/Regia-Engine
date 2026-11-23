// SPDX-License-Identifier: MPL-2.0
//! Labeled JPEG contact sheets (ADR-8036).
//!
//! One sheet is a grid of the input images, each with a label strip. Images keep their aspect
//! and sit centered on black; the sheet is scaled down with nearest-neighbor only when it is
//! wider than the requested maximum. Luma for the CSV is BT.601 on the 8-bit image (or on a
//! fractional region of it). PNG and JPEG go through the workspace `image` crate. PPM and PGM
//! are read here.

use std::path::{Path, PathBuf};

use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, Rgb, RgbImage};

/// Height of the label strip above each cell, in pixels.
pub const STRIP_HEIGHT: u32 = 28;

/// Gap between cells and around the sheet, in pixels.
pub const GAP: u32 = 4;

/// Sheet background, sRGB.
pub const SHEET_BACKGROUND: [u8; 3] = [24, 24, 24];

/// Label-strip background, sRGB.
pub const STRIP_BACKGROUND: [u8; 3] = [40, 40, 40];

/// Letterbox background, sRGB.
pub const LETTERBOX_BACKGROUND: [u8; 3] = [0, 0, 0];

/// Label ink, sRGB.
pub const TEXT_COLOR: [u8; 3] = [255, 255, 255];

/// Default sheet width cap, in pixels.
pub const DEFAULT_MAX_WIDTH: u32 = 3840;

/// Default JPEG quality (1..=100).
pub const DEFAULT_QUALITY: u8 = 90;

const LUMA_CLIP: f32 = 250.0;

/// Why a contact sheet was refused. The display text starts with a stable code.
#[derive(Debug, PartialEq, Eq)]
pub enum ContactSheetError {
    /// The arguments cannot build a sheet.
    Usage(String),
    /// A file could not be read or written.
    Io(String),
    /// The bytes are not a PNG, JPEG, PPM or PGM this tool reads.
    Format(String),
}

impl std::fmt::Display for ContactSheetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "contact_sheet_usage: {message}"),
            Self::Io(message) => write!(f, "contact_sheet_io: {message}"),
            Self::Format(message) => write!(f, "contact_sheet_format: {message}"),
        }
    }
}

impl std::error::Error for ContactSheetError {}

/// A fractional window inside an image. Origin is in `[0, 1]`, size is in `(0, 1]`, and the
/// rectangle stays inside the image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Roi {
    /// Left edge, as a fraction of the width.
    pub x: f32,
    /// Top edge, as a fraction of the height.
    pub y: f32,
    /// Width, as a fraction of the image width.
    pub w: f32,
    /// Height, as a fraction of the image height.
    pub h: f32,
}

/// One cell before layout.
#[derive(Clone, Debug)]
pub struct SheetImage {
    /// File name used for the stem and the CSV `file` column.
    pub file_name: String,
    /// 8-bit sRGB pixels.
    pub image: RgbImage,
    /// Optional role, such as `before` or `after`.
    pub role: Option<String>,
    /// Optional extra label.
    pub label: Option<String>,
    /// Pair index when this cell is one side of a before/after pair.
    pub pair_index: Option<u32>,
}

/// Layout and encode settings.
#[derive(Clone, Debug)]
pub struct SheetOptions {
    /// Columns. Pair sheets have already been widened to an even count.
    pub cols: u32,
    /// Scale the sheet down when it is wider than this.
    pub max_width: u32,
    /// JPEG quality, 1..=100.
    pub quality: u8,
    /// Metric window. The drawn cell is still the whole image.
    pub roi: Option<Roi>,
    /// Write luma columns and the `m=` / `p99=` / `clip=` caption.
    pub metrics: bool,
}

/// The JPEG bytes, the CSV text, and the sheet size.
#[derive(Clone, Debug)]
pub struct SheetOutput {
    /// JPEG file bytes, starting with `FF D8`.
    pub jpeg: Vec<u8>,
    /// CSV text, including the header row.
    pub csv: String,
    /// Sheet width after the max-width scale.
    pub width: u32,
    /// Sheet height after the max-width scale.
    pub height: u32,
}

/// Luma summary of one cell (or of a pair's delta).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LumaStats {
    /// Mean BT.601 luma.
    pub mean: f64,
    /// The value at index `ceil(0.99 * count) - 1` of the sorted lumas.
    pub p99: f64,
    /// Fraction of pixels whose luma is at least 250.
    pub clip: f64,
}

/// Parse `x,y,w,h` fractions.
///
/// # Errors
///
/// The text is not four fractions, or the rectangle is outside the unit square.
pub fn parse_roi(text: &str) -> Result<Roi, ContactSheetError> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(ContactSheetError::Usage(
            "roi must be four fractions x,y,w,h".to_owned(),
        ));
    }
    let mut nums = [0.0_f32; 4];
    for (index, part) in parts.iter().enumerate() {
        nums[index] = part
            .parse::<f32>()
            .map_err(|_| ContactSheetError::Usage(format!("roi component `{part}` is not a number")))?;
        if !nums[index].is_finite() {
            return Err(ContactSheetError::Usage(
                "roi components must be finite".to_owned(),
            ));
        }
    }
    let roi = Roi {
        x: nums[0],
        y: nums[1],
        w: nums[2],
        h: nums[3],
    };
    let origin_ok = (0.0..=1.0).contains(&roi.x) && (0.0..=1.0).contains(&roi.y);
    let size_ok = roi.w > 0.0 && roi.w <= 1.0 && roi.h > 0.0 && roi.h <= 1.0;
    let inside = roi.x + roi.w <= 1.0 + 1.0e-5 && roi.y + roi.h <= 1.0 + 1.0e-5;
    if !origin_ok || !size_ok || !inside {
        return Err(ContactSheetError::Usage(
            "roi origin must be in [0, 1], size in (0, 1], and the rectangle inside the image".to_owned(),
        ));
    }
    Ok(roi)
}

/// Column count. Pair sheets use `max(1, cols / 2) * 2`.
///
/// # Errors
///
/// `cols` is not positive.
pub fn effective_columns(cols: i32, pairs: bool) -> Result<u32, ContactSheetError> {
    if cols <= 0 {
        return Err(ContactSheetError::Usage("cols must be at least 1".to_owned()));
    }
    if pairs {
        Ok(u32::try_from(cols / 2).unwrap_or(0).max(1) * 2)
    } else {
        Ok(u32::try_from(cols).unwrap_or(1))
    }
}

/// The caption drawn in the strip: `{stem}[ {role}][ [{label}]]` plus metrics and an optional delta.
#[must_use]
pub fn cell_caption(
    stem: &str,
    role: Option<&str>,
    label: Option<&str>,
    metrics: Option<LumaStats>,
    delta: Option<LumaStats>,
) -> String {
    let mut text = stem.to_owned();
    if let Some(role) = role.filter(|role| !role.is_empty()) {
        text.push(' ');
        text.push_str(role);
    }
    if let Some(label) = label.filter(|label| !label.is_empty()) {
        text.push_str(" [");
        text.push_str(label);
        text.push(']');
    }
    if let Some(stats) = metrics {
        text.push_str(&format!(
            " m={:.1} p99={:.1} clip={:.3}",
            stats.mean, stats.p99, stats.clip
        ));
    }
    if let Some(stats) = delta {
        text.push_str(&format!(
            " d=({:+.1},{:+.1},{:+.3})",
            stats.mean, stats.p99, stats.clip
        ));
    }
    text
}

/// BT.601 luma of one 8-bit sRGB pixel.
#[must_use]
pub fn luma_bt601(pixel: Rgb<u8>) -> f32 {
    0.299 * f32::from(pixel[0]) + 0.587 * f32::from(pixel[1]) + 0.114 * f32::from(pixel[2])
}

/// Mean, p99 and clip fraction of the pixels in `image` (or in `roi`).
#[must_use]
pub fn luma_stats(image: &RgbImage, roi: Option<Roi>) -> LumaStats {
    let (x0, y0, width, height) = roi.map_or((0, 0, image.width(), image.height()), |roi| {
        roi_pixels(roi, image.width(), image.height())
    });
    let mut values = Vec::with_capacity((width as usize).saturating_mul(height as usize));
    for y in y0..y0 + height {
        for x in x0..x0 + width {
            values.push(luma_bt601(*image.get_pixel(x, y)));
        }
    }
    if values.is_empty() {
        return LumaStats {
            mean: 0.0,
            p99: 0.0,
            clip: 0.0,
        };
    }
    let sum: f64 = values.iter().map(|value| f64::from(*value)).sum();
    let mean = sum / values.len() as f64;
    let clipped = values.iter().filter(|value| **value >= LUMA_CLIP).count();
    values.sort_by(f32::total_cmp);
    let index = percentile_index(values.len());
    LumaStats {
        mean,
        p99: f64::from(values[index]),
        clip: clipped as f64 / values.len() as f64,
    }
}

fn percentile_index(count: usize) -> usize {
    let scaled = (0.99 * count as f64).ceil();
    let index = if scaled <= 1.0 { 0 } else { (scaled as usize) - 1 };
    index.min(count - 1)
}

fn roi_pixels(roi: Roi, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    let mut x0 = (roi.x * width as f32) as u32;
    let mut y0 = (roi.y * height as f32) as u32;
    let mut pw = (roi.w * width as f32) as u32;
    let mut ph = (roi.h * height as f32) as u32;
    if pw == 0 {
        pw = 1;
    }
    if ph == 0 {
        ph = 1;
    }
    x0 = x0.min(width - 1);
    y0 = y0.min(height - 1);
    pw = pw.min(width - x0).max(1);
    ph = ph.min(height - y0).max(1);
    (x0, y0, pw, ph)
}

/// True when `path` has a contact-sheet image suffix.
#[must_use]
pub fn is_image_name(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "ppm" | "pgm")
    )
}

/// Read one image file.
///
/// # Errors
///
/// The file is missing or the bytes are not a supported image.
pub fn load_image_file(path: &Path) -> Result<RgbImage, ContactSheetError> {
    let bytes = std::fs::read(path)
        .map_err(|err| ContactSheetError::Io(format!("cannot read image {}: {err}", path.display())))?;
    load_image_bytes(&bytes)
}

/// Decode PNG, JPEG, PPM or PGM bytes to 8-bit sRGB.
///
/// # Errors
///
/// The bytes are not one of those formats.
pub fn load_image_bytes(bytes: &[u8]) -> Result<RgbImage, ContactSheetError> {
    if bytes.len() >= 2 && bytes[0] == b'P' && matches!(bytes[1], b'2' | b'3' | b'5' | b'6') {
        return parse_netpbm(bytes);
    }
    image::load_from_memory(bytes)
        .map(|image| image.to_rgb8())
        .map_err(|_| ContactSheetError::Format("unsupported image format".to_owned()))
}

/// Encode `image` as a JPEG.
///
/// # Errors
///
/// `quality` is outside 1..=100, or the encoder refused the buffer.
pub fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>, ContactSheetError> {
    if !(1..=100).contains(&quality) {
        return Err(ContactSheetError::Usage(
            "quality must be from 1 to 100".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
    encoder
        .encode(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|err| ContactSheetError::Format(format!("jpeg encode failed: {err}")))?;
    Ok(bytes)
}

/// Lay the cells out and encode the sheet plus its CSV.
///
/// # Errors
///
/// There are no cells, or the quality is outside 1..=100.
pub fn build_sheet(images: &[SheetImage], options: &SheetOptions) -> Result<SheetOutput, ContactSheetError> {
    let raster = compose_rgb(images, options)?;
    let jpeg = encode_jpeg(&raster, options.quality)?;
    let csv = csv_text(images, options);
    Ok(SheetOutput {
        width: raster.width(),
        height: raster.height(),
        jpeg,
        csv,
    })
}

/// The sheet raster before JPEG encoding.
///
/// # Errors
///
/// There are no cells, or `cols` / `max_width` is zero.
pub fn compose_rgb(images: &[SheetImage], options: &SheetOptions) -> Result<RgbImage, ContactSheetError> {
    if images.is_empty() {
        return Err(ContactSheetError::Usage("no images".to_owned()));
    }
    if options.cols == 0 || options.max_width == 0 {
        return Err(ContactSheetError::Usage(
            "cols and max-width must be at least 1".to_owned(),
        ));
    }
    let cell_w = images
        .iter()
        .map(|image| image.image.width())
        .max()
        .unwrap_or(1)
        .max(1);
    let cell_h = images
        .iter()
        .map(|image| image.image.height())
        .max()
        .unwrap_or(1)
        .max(1);
    let cols = options
        .cols
        .min(u32::try_from(images.len()).unwrap_or(u32::MAX))
        .max(1);
    let rows = u32::try_from(images.len().div_ceil(cols as usize))
        .unwrap_or(1)
        .max(1);
    let block_h = STRIP_HEIGHT + cell_h;
    let sheet_w = cols * cell_w + (cols + 1) * GAP;
    let sheet_h = rows * block_h + (rows + 1) * GAP;
    let mut sheet = RgbImage::from_pixel(sheet_w, sheet_h, Rgb(SHEET_BACKGROUND));
    let captions = captions(images, options.metrics, options.roi);
    for (index, image) in images.iter().enumerate() {
        let col = u32::try_from(index).unwrap_or(0) % cols;
        let row = u32::try_from(index).unwrap_or(0) / cols;
        let origin_x = GAP + col * (cell_w + GAP);
        let origin_y = GAP + row * (block_h + GAP);
        fill_rect(
            &mut sheet,
            origin_x,
            origin_y,
            cell_w,
            STRIP_HEIGHT,
            STRIP_BACKGROUND,
        );
        fill_rect(
            &mut sheet,
            origin_x,
            origin_y + STRIP_HEIGHT,
            cell_w,
            cell_h,
            LETTERBOX_BACKGROUND,
        );
        blit_fit(
            &mut sheet,
            origin_x,
            origin_y + STRIP_HEIGHT,
            cell_w,
            cell_h,
            &image.image,
        );
        draw_text(
            &mut sheet,
            origin_x + 4,
            origin_y + (STRIP_HEIGHT - 7) / 2,
            &captions[index],
        );
    }
    if sheet.width() > options.max_width {
        let height =
            ((u64::from(sheet.height()) * u64::from(options.max_width)) / u64::from(sheet.width())).max(1);
        let height = u32::try_from(height).unwrap_or(u32::MAX);
        sheet = scale_nearest(&sheet, options.max_width, height);
    }
    Ok(sheet)
}

/// CSV beside the JPEG. Mean and p99 use four decimal places; clip uses six.
#[must_use]
pub fn csv_text(images: &[SheetImage], options: &SheetOptions) -> String {
    let mut out = String::from(
        "index,file,stem,label,role,pair_index,mean_luma,p99_luma,clip_fraction,delta_mean_luma,delta_p99_luma,delta_clip_fraction\n",
    );
    let stats: Vec<LumaStats> = images
        .iter()
        .map(|image| luma_stats(&image.image, options.roi))
        .collect();
    for (index, image) in images.iter().enumerate() {
        let delta = image.pair_index.and_then(|pair| {
            let before = images.iter().enumerate().find(|(_, other)| {
                other.pair_index == Some(pair) && other.role.as_deref() == Some("before")
            })?;
            let after = stats.get(index)?;
            let base = stats.get(before.0)?;
            Some(LumaStats {
                mean: after.mean - base.mean,
                p99: after.p99 - base.p99,
                clip: after.clip - base.clip,
            })
        });
        // The before cell of a pair has no delta; only `after` does.
        let delta = if image.role.as_deref() == Some("after") {
            delta
        } else {
            None
        };
        let stats = stats[index];
        let (mean, p99, clip, d_mean, d_p99, d_clip) = if options.metrics {
            (
                format!("{:.4}", stats.mean),
                format!("{:.4}", stats.p99),
                format!("{:.6}", stats.clip),
                delta.map(|d| format!("{:.4}", d.mean)).unwrap_or_default(),
                delta.map(|d| format!("{:.4}", d.p99)).unwrap_or_default(),
                delta.map(|d| format!("{:.6}", d.clip)).unwrap_or_default(),
            )
        } else {
            (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            )
        };
        out.push_str(&csv_row(&[
            index.to_string(),
            image.file_name.clone(),
            stem_of(&image.file_name),
            image.label.clone().unwrap_or_default(),
            image.role.clone().unwrap_or_default(),
            image.pair_index.map(|n| n.to_string()).unwrap_or_default(),
            mean,
            p99,
            clip,
            d_mean,
            d_p99,
            d_clip,
        ]));
        out.push('\n');
    }
    out
}

/// Write `jpeg_path` and the CSV next to it (the suffix replaced with `.csv`). Both writes are
/// a temporary file in the same directory followed by a rename.
///
/// # Errors
///
/// A directory is missing or a write failed.
pub fn write_sheet_files(jpeg_path: &Path, output: &SheetOutput) -> Result<PathBuf, ContactSheetError> {
    write_atomic(jpeg_path, &output.jpeg)?;
    let csv_path = jpeg_path.with_extension("csv");
    write_atomic(&csv_path, output.csv.as_bytes())?;
    Ok(csv_path)
}

fn captions(images: &[SheetImage], metrics: bool, roi: Option<Roi>) -> Vec<String> {
    let stats: Vec<LumaStats> = images.iter().map(|image| luma_stats(&image.image, roi)).collect();
    images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let measured = metrics.then_some(stats[index]);
            let delta = if metrics && image.role.as_deref() == Some("after") {
                image.pair_index.and_then(|pair| {
                    let before = images.iter().position(|other| {
                        other.pair_index == Some(pair) && other.role.as_deref() == Some("before")
                    })?;
                    Some(LumaStats {
                        mean: stats[index].mean - stats[before].mean,
                        p99: stats[index].p99 - stats[before].p99,
                        clip: stats[index].clip - stats[before].clip,
                    })
                })
            } else {
                None
            };
            cell_caption(
                &stem_of(&image.file_name),
                image.role.as_deref(),
                image.label.as_deref(),
                measured,
                delta,
            )
        })
        .collect()
}

fn stem_of(file_name: &str) -> String {
    Path::new(file_name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.to_owned())
}

fn csv_row(fields: &[String]) -> String {
    fields
        .iter()
        .map(|field| {
            if field.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", field.replace('"', "\"\""))
            } else {
                field.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ContactSheetError> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sheet".to_owned());
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)
        .map_err(|err| ContactSheetError::Io(format!("cannot write {}: {err}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        ContactSheetError::Io(format!("cannot write {}: {err}", path.display()))
    })
}

fn fill_rect(image: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) {
    for py in y..y.saturating_add(h).min(image.height()) {
        for px in x..x.saturating_add(w).min(image.width()) {
            image.put_pixel(px, py, Rgb(color));
        }
    }
}

fn blit_fit(dst: &mut RgbImage, ox: u32, oy: u32, cell_w: u32, cell_h: u32, src: &RgbImage) {
    let scale = (f64::from(cell_w) / f64::from(src.width().max(1)))
        .min(f64::from(cell_h) / f64::from(src.height().max(1)))
        .min(1.0);
    let dw = (f64::from(src.width()) * scale)
        .round()
        .clamp(1.0, f64::from(cell_w)) as u32;
    let dh = (f64::from(src.height()) * scale)
        .round()
        .clamp(1.0, f64::from(cell_h)) as u32;
    let dx = ox + (cell_w - dw) / 2;
    let dy = oy + (cell_h - dh) / 2;
    for y in 0..dh {
        let sy = ((u64::from(y) * u64::from(src.height())) / u64::from(dh)).min(u64::from(src.height() - 1))
            as u32;
        for x in 0..dw {
            let sx = ((u64::from(x) * u64::from(src.width())) / u64::from(dw)).min(u64::from(src.width() - 1))
                as u32;
            let px = dx + x;
            let py = dy + y;
            if px < dst.width() && py < dst.height() {
                dst.put_pixel(px, py, *src.get_pixel(sx, sy));
            }
        }
    }
}

fn scale_nearest(src: &RgbImage, width: u32, height: u32) -> RgbImage {
    let mut dst = RgbImage::new(width.max(1), height.max(1));
    for y in 0..dst.height() {
        let sy = ((u64::from(y) * u64::from(src.height())) / u64::from(dst.height()))
            .min(u64::from(src.height() - 1)) as u32;
        for x in 0..dst.width() {
            let sx = ((u64::from(x) * u64::from(src.width())) / u64::from(dst.width()))
                .min(u64::from(src.width() - 1)) as u32;
            dst.put_pixel(x, y, *src.get_pixel(sx, sy));
        }
    }
    dst
}

/// A 5×7 mark for `ch`. Space is blank. Other characters use an original pattern so a strip is
/// not a flat color; the pattern is not a copied glyph table.
fn glyph(ch: char) -> [u8; 7] {
    if ch == ' ' {
        return [0; 7];
    }
    let mut rows = [0_u8; 7];
    let mut state = u32::from(ch);
    for row in &mut rows {
        state = state.wrapping_mul(16_777_619).wrapping_add(0x9E37_79B9);
        let mut bits = ((state >> 8) as u8) & 0b1_1111;
        if bits == 0 {
            bits = 0b1_0001;
        }
        *row = bits;
    }
    rows
}

fn draw_text(image: &mut RgbImage, mut x: u32, y: u32, text: &str) {
    for ch in text.chars() {
        let rows = glyph(ch);
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..5_u32 {
                if (*bits & (1_u8 << (4 - col))) == 0 {
                    continue;
                }
                let px = x + col;
                let py = y + u32::try_from(row).unwrap_or(0);
                if px < image.width() && py < image.height() {
                    image.put_pixel(px, py, Rgb(TEXT_COLOR));
                }
            }
        }
        x = x.saturating_add(6);
        if x >= image.width() {
            break;
        }
    }
}

fn parse_netpbm(bytes: &[u8]) -> Result<RgbImage, ContactSheetError> {
    let kind = bytes[1];
    let mut scan = Scan { bytes, i: 2 };
    let width = scan.number()?;
    let height = scan.number()?;
    let maxval = scan.number()?;
    if width == 0 || height == 0 || maxval != 255 {
        return Err(ContactSheetError::Format("unsupported image format".to_owned()));
    }
    let pixels = (width as usize).saturating_mul(height as usize);
    let gray = matches!(kind, b'2' | b'5');
    let mut image = RgbImage::new(width, height);
    if matches!(kind, b'5' | b'6') {
        if scan.i >= bytes.len() || !bytes[scan.i].is_ascii_whitespace() {
            return Err(ContactSheetError::Format("unsupported image format".to_owned()));
        }
        scan.i += 1;
        let channels = if gray { 1 } else { 3 };
        let need = pixels.saturating_mul(channels);
        let data = bytes.get(scan.i..).unwrap_or_default();
        if data.len() < need {
            return Err(ContactSheetError::Format("unsupported image format".to_owned()));
        }
        for index in 0..pixels {
            let (r, g, b) = if gray {
                let v = data[index];
                (v, v, v)
            } else {
                let at = index * 3;
                (data[at], data[at + 1], data[at + 2])
            };
            image.put_pixel((index as u32) % width, (index as u32) / width, Rgb([r, g, b]));
        }
        return Ok(image);
    }
    let channels = if gray { 1 } else { 3 };
    for index in 0..pixels {
        let sample = |scan: &mut Scan| scan.number().map(|n| u8::try_from(n).unwrap_or(255));
        let (r, g, b) = if channels == 1 {
            let v = sample(&mut scan)?;
            (v, v, v)
        } else {
            (sample(&mut scan)?, sample(&mut scan)?, sample(&mut scan)?)
        };
        image.put_pixel((index as u32) % width, (index as u32) / width, Rgb([r, g, b]));
    }
    Ok(image)
}

struct Scan<'a> {
    bytes: &'a [u8],
    i: usize,
}

impl Scan<'_> {
    fn number(&mut self) -> Result<u32, ContactSheetError> {
        self.skip_ws_and_comments();
        let start = self.i;
        while self.i < self.bytes.len() && self.bytes[self.i].is_ascii_digit() {
            self.i += 1;
        }
        if start == self.i {
            return Err(ContactSheetError::Format("unsupported image format".to_owned()));
        }
        let text = std::str::from_utf8(&self.bytes[start..self.i]).unwrap_or("");
        text.parse::<u32>()
            .map_err(|_| ContactSheetError::Format("unsupported image format".to_owned()))
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while self.i < self.bytes.len() && self.bytes[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            if self.i < self.bytes.len() && self.bytes[self.i] == b'#' {
                while self.i < self.bytes.len() && self.bytes[self.i] != b'\n' {
                    self.i += 1;
                }
                continue;
            }
            break;
        }
    }
}

/// Load a directory of images, sorted by file name. Non-images are skipped.
///
/// # Errors
///
/// The directory cannot be read, or a chosen image cannot be decoded.
pub fn load_dir(dir: &Path) -> Result<Vec<SheetImage>, ContactSheetError> {
    let mut paths = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|err| ContactSheetError::Io(format!("cannot read image {}: {err}", dir.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|err| ContactSheetError::Io(format!("cannot read image {}: {err}", dir.display())))?;
        let path = entry.path();
        if path.is_file() && is_image_name(&path) {
            paths.push(path);
        }
    }
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    paths
        .iter()
        .map(|path| load_named(path, None, None, None))
        .collect()
}

/// Pair two directories by file name. Names that are in only one directory are warned on stderr
/// and left out. Cells alternate `before`, `after`.
///
/// # Errors
///
/// A directory or a paired image cannot be read.
pub fn load_pair_dirs(before: &Path, after: &Path) -> Result<Vec<SheetImage>, ContactSheetError> {
    let left = image_paths(before)?;
    let right = image_paths(after)?;
    let right_names: std::collections::HashMap<std::ffi::OsString, PathBuf> = right
        .iter()
        .filter_map(|path| path.file_name().map(|name| (name.to_owned(), path.clone())))
        .collect();
    let mut used = std::collections::HashSet::new();
    let mut cells = Vec::new();
    let mut pair_index = 0_u32;
    for path in &left {
        let Some(name) = path.file_name() else { continue };
        if let Some(other) = right_names.get(name) {
            used.insert(name.to_owned());
            cells.push(load_named(
                path,
                Some("before".to_owned()),
                None,
                Some(pair_index),
            )?);
            cells.push(load_named(
                other,
                Some("after".to_owned()),
                None,
                Some(pair_index),
            )?);
            pair_index += 1;
        } else {
            eprintln!("warning: unmatched name {}", path.display());
        }
    }
    for path in &right {
        if let Some(name) = path.file_name()
            && !used.contains(name)
        {
            eprintln!("warning: unmatched name {}", path.display());
        }
    }
    Ok(cells)
}

fn image_paths(dir: &Path) -> Result<Vec<PathBuf>, ContactSheetError> {
    let mut paths = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|err| ContactSheetError::Io(format!("cannot read image {}: {err}", dir.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|err| ContactSheetError::Io(format!("cannot read image {}: {err}", dir.display())))?;
        let path = entry.path();
        if path.is_file() && is_image_name(&path) {
            paths.push(path);
        }
    }
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(paths)
}

/// Load one file as a cell.
///
/// # Errors
///
/// The file cannot be read or decoded.
pub fn load_named(
    path: &Path,
    role: Option<String>,
    label: Option<String>,
    pair_index: Option<u32>,
) -> Result<SheetImage, ContactSheetError> {
    let image = load_image_file(path)?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_owned());
    Ok(SheetImage {
        file_name,
        image,
        role,
        label,
        pair_index,
    })
}

/// Apply labels: one label for every cell, one label per pair (shared by both sides), or one
/// label per cell.
///
/// # Errors
///
/// The label count matches none of those shapes.
pub fn assign_labels(images: &mut [SheetImage], labels: &[String]) -> Result<(), ContactSheetError> {
    if labels.is_empty() {
        return Ok(());
    }
    if labels.len() == 1 {
        for image in images.iter_mut() {
            image.label = Some(labels[0].clone());
        }
        return Ok(());
    }
    let pairs = images
        .iter()
        .filter_map(|image| image.pair_index)
        .max()
        .map(|n| n as usize + 1);
    if let Some(pair_count) = pairs
        && labels.len() == pair_count
        && images.len() == pair_count * 2
    {
        for image in images.iter_mut() {
            if let Some(index) = image.pair_index {
                image.label = labels.get(index as usize).cloned();
            }
        }
        return Ok(());
    }
    if labels.len() == images.len() {
        for (image, label) in images.iter_mut().zip(labels.iter()) {
            image.label = Some(label.clone());
        }
        return Ok(());
    }
    Err(ContactSheetError::Usage(format!(
        "got {} labels for {} cells",
        labels.len(),
        images.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, Luma, Rgba, RgbaImage};

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb(rgb))
    }

    fn cell(name: &str, image: RgbImage) -> SheetImage {
        SheetImage {
            file_name: name.to_owned(),
            image,
            role: None,
            label: None,
            pair_index: None,
        }
    }

    fn options(cols: u32) -> SheetOptions {
        SheetOptions {
            cols,
            max_width: DEFAULT_MAX_WIDTH,
            quality: 95,
            roi: None,
            metrics: true,
        }
    }

    #[test]
    fn solid_gray_white_and_red_luma_and_the_gradient_p99() {
        let gray = luma_stats(&solid(4, 4, [100, 100, 100]), None);
        assert!((gray.mean - 100.0).abs() < 1e-6, "{}", gray.mean);
        assert!((gray.p99 - 100.0).abs() < 1e-6);
        assert_eq!(gray.clip, 0.0);

        let white = luma_stats(&solid(2, 2, [255, 255, 255]), None);
        assert!((white.clip - 1.0).abs() < 1e-9);
        let red = luma_bt601(Rgb([255, 0, 0]));
        assert!((red - 76.245).abs() < 0.01, "{red}");

        let mut gradient = RgbImage::new(10, 10);
        for y in 0..10 {
            for x in 0..10 {
                let v = u8::try_from(y * 10 + x).expect("gradient sample fits in a byte");
                gradient.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        let stats = luma_stats(&gradient, None);
        assert!((stats.mean - 49.5).abs() < 1e-6, "{}", stats.mean);
        assert!((stats.p99 - 98.0).abs() < 1e-6, "{}", stats.p99);
    }

    #[test]
    fn roi_rejects_a_rectangle_outside_the_image_and_limits_the_mean() {
        assert!(parse_roi("0,0,1,1").is_ok());
        assert!(parse_roi("-0.1,0,1,1").is_err());
        assert!(parse_roi("0,0,1.1,1").is_err());
        assert!(parse_roi("0.5,0.5,0.6,0.6").is_err());
        assert!(parse_roi("0,0,0,1").is_err());

        let mut image = solid(10, 10, [0, 0, 0]);
        for y in 0..5 {
            for x in 0..5 {
                image.put_pixel(x, y, Rgb([100, 100, 100]));
            }
        }
        let roi = parse_roi("0,0,0.5,0.5").expect("roi");
        let stats = luma_stats(&image, Some(roi));
        assert!((stats.mean - 100.0).abs() < 1.0, "{}", stats.mean);
    }

    #[test]
    fn png_ppm_and_jpeg_round_trip() {
        let rgba = RgbaImage::from_pixel(2, 2, Rgba([10, 20, 30, 255]));
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .expect("png");
        let loaded = load_image_bytes(&png).expect("rgba png");
        assert_eq!(loaded.get_pixel(0, 0).0, [10, 20, 30]);

        let gray = image::GrayImage::from_pixel(2, 2, Luma([40]));
        let mut png = Vec::new();
        DynamicImage::ImageLuma8(gray)
            .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .expect("gray png");
        let loaded = load_image_bytes(&png).expect("gray png");
        assert_eq!(loaded.get_pixel(0, 0).0, [40, 40, 40]);

        let wide: image::ImageBuffer<Luma<u16>, Vec<u16>> =
            image::ImageBuffer::from_pixel(2, 2, Luma([200 * 257]));
        let mut png = Vec::new();
        DynamicImage::ImageLuma16(wide)
            .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .expect("16-bit png");
        let loaded = load_image_bytes(&png).expect("16-bit");
        assert_eq!(loaded.get_pixel(0, 0).0, [200, 200, 200]);

        let mut ppm = b"P6\n2 2\n255\n".to_vec();
        ppm.extend_from_slice(&[9, 8, 7, 9, 8, 7, 9, 8, 7, 9, 8, 7]);
        let loaded = load_image_bytes(&ppm).expect("p6");
        assert_eq!(loaded.get_pixel(1, 1).0, [9, 8, 7]);
        let p3 = b"P3\n2 1\n255\n1 2 3 4 5 6\n";
        let loaded = load_image_bytes(p3).expect("p3");
        assert_eq!(loaded.get_pixel(1, 0).0, [4, 5, 6]);
        let pgm = b"P5\n2 1\n255\n\x10\x20";
        let loaded = load_image_bytes(pgm).expect("p5");
        assert_eq!(loaded.get_pixel(1, 0).0, [0x20, 0x20, 0x20]);

        assert!(
            load_image_bytes(b"NOTANIMAGE")
                .expect_err("bad magic")
                .to_string()
                .contains("unsupported image format")
        );
        let missing = load_image_file(Path::new("/no/such/contact-sheet-image.png"));
        assert!(
            missing
                .expect_err("missing file")
                .to_string()
                .contains("cannot read image")
        );

        let solid_image = solid(16, 16, [200, 30, 80]);
        let jpeg = encode_jpeg(&solid_image, 95).expect("jpeg");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        let back = image::load_from_memory(&jpeg).expect("decode").to_rgb8();
        for pixel in back.pixels() {
            for (got, want) in pixel.0.iter().zip([200, 30, 80]) {
                assert!((*got as i32 - want).abs() <= 4, "{got} vs {want}");
            }
        }
    }

    #[test]
    fn the_grid_letterboxes_and_scales_down_and_the_strip_is_not_flat() {
        let mut images = Vec::new();
        for index in 0..4 {
            images.push(cell(&format!("f{index}.png"), solid(20, 10, [index * 20, 0, 0])));
        }
        let raster = compose_rgb(&images, &options(2)).expect("sheet");
        let width = 2 * 20 + 3 * GAP;
        let height = 2 * (STRIP_HEIGHT + 10) + 3 * GAP;
        assert_eq!((raster.width(), raster.height()), (width, height));

        let mut pair = vec![
            cell("small.png", solid(10, 10, [255, 0, 0])),
            cell("big.png", solid(20, 20, [0, 0, 255])),
        ];
        pair[0].label = Some("hero".to_owned());
        let raster = compose_rgb(&pair, &options(2)).expect("letterbox");
        // The small image is centered in the 20×20 content box, so the content corner is black
        // and the content centre is the small image's red.
        let content_x = GAP;
        let content_y = GAP + STRIP_HEIGHT;
        assert_eq!(raster.get_pixel(content_x, content_y).0, LETTERBOX_BACKGROUND);
        assert_eq!(raster.get_pixel(content_x + 10, content_y + 10).0, [255, 0, 0]);

        let mut colors = std::collections::HashSet::new();
        let mut saw_ink = false;
        for y in content_y - STRIP_HEIGHT..content_y {
            for x in content_x..content_x + 20 {
                let pixel = raster.get_pixel(x, y).0;
                if pixel == TEXT_COLOR {
                    saw_ink = true;
                }
                colors.insert(pixel);
            }
        }
        assert!(saw_ink);
        assert!(colors.len() > 1);

        let natural_w = 4 * 20 + 5 * GAP;
        let wide = compose_rgb(
            &images,
            &SheetOptions {
                max_width: natural_w + 50,
                ..options(4)
            },
        )
        .expect("wide");
        assert_eq!(
            wide.width(),
            natural_w,
            "a sheet narrower than max-width is not scaled up"
        );
        let scaled = compose_rgb(
            &images,
            &SheetOptions {
                max_width: 30,
                cols: 4,
                ..options(4)
            },
        )
        .expect("scaled");
        assert_eq!(scaled.width(), 30);
    }

    #[test]
    fn captions_metrics_and_pair_deltas_land_in_the_csv() {
        let caption = cell_caption(
            "shot",
            None,
            Some("hero"),
            Some(LumaStats {
                mean: 100.0,
                p99: 100.0,
                clip: 0.0,
            }),
            Some(LumaStats {
                mean: 5.0,
                p99: 5.0,
                clip: 0.25,
            }),
        );
        assert_eq!(
            caption,
            "shot [hero] m=100.0 p99=100.0 clip=0.000 d=(+5.0,+5.0,+0.250)"
        );

        let before = SheetImage {
            file_name: "shot.png".into(),
            image: solid(8, 8, [100, 100, 100]),
            role: Some("before".into()),
            label: Some("hero".into()),
            pair_index: Some(0),
        };
        let after = SheetImage {
            file_name: "shot.png".into(),
            image: solid(8, 8, [150, 150, 150]),
            role: Some("after".into()),
            label: Some("hero".into()),
            pair_index: Some(0),
        };
        let sheet = build_sheet(&[before, after], &options(2)).expect("sheet");
        assert!(!sheet.csv.contains("m="));
        assert!(sheet.csv.lines().nth(2).expect("csv row").contains("50.0000"));
        assert!(
            sheet
                .csv
                .starts_with("index,file,stem,label,role,pair_index,mean_luma")
        );
        let cols = effective_columns(5, true).expect("cols");
        assert_eq!(cols, 4);
        assert_eq!(effective_columns(1, true).expect("cols"), 2);
        assert!(effective_columns(0, false).is_err());
        assert!(encode_jpeg(&solid(1, 1, [0, 0, 0]), 0).is_err());
    }

    #[test]
    fn dirs_are_sorted_and_pairs_warn_on_unmatched_names() {
        let dir = regia_test_utils::unique_temp_dir("contact-dirs");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("b.ppm"), b"P6\n1 1\n255\n\x01\x02\x03").expect("ppm");
        std::fs::write(dir.join("a.ppm"), b"P6\n1 1\n255\n\x04\x05\x06").expect("ppm");
        std::fs::write(dir.join("notes.txt"), b"skip").expect("txt");
        let loaded = load_dir(&dir).expect("dir");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].file_name, "a.ppm");
        assert_eq!(loaded[1].file_name, "b.ppm");

        let before = dir.join("before");
        let after = dir.join("after");
        std::fs::create_dir_all(&before).expect("dir");
        std::fs::create_dir_all(&after).expect("dir");
        std::fs::write(before.join("shot.ppm"), b"P6\n1 1\n255\n\x0a\x0a\x0a").expect("ppm");
        std::fs::write(before.join("only-before.ppm"), b"P6\n1 1\n255\n\x01\x01\x01").expect("ppm");
        std::fs::write(after.join("shot.ppm"), b"P6\n1 1\n255\n\x14\x14\x14").expect("ppm");
        let paired = load_pair_dirs(&before, &after).expect("pairs");
        assert_eq!(
            paired.len(),
            2,
            "{:?}",
            paired.iter().map(|c| &c.file_name).collect::<Vec<_>>()
        );
        assert_eq!(paired[0].role.as_deref(), Some("before"));
        assert_eq!(paired[1].role.as_deref(), Some("after"));
        let mut labeled = paired;
        assign_labels(&mut labeled, &["hero".to_owned()]).expect("one label");
        assert_eq!(labeled[0].label.as_deref(), Some("hero"));
        assign_labels(&mut labeled, &["one".into(), "two".into()])
            .expect("per pair is one label shared, or per cell");
    }
}
