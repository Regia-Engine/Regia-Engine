// SPDX-License-Identifier: MPL-2.0
//! CPU-only north-up shaded terrain overviews (ADR-8063).
//!
//! Behavior requirements: DO27 `tools/ops/gen_map_overviews.py`, d6ed37677
//! (regia-absorb-1), owner-authorized 2026-11-16. Original Rust implementation;
//! no Python, map presets or game assets. Area averaging, a separable five-tap
//! binomial Gaussian approximation, finite-difference Lambert shading, percentile
//! elevation and contour bands operate only on this preview, never the source.
//!
//! ADR-8596 adds an explicit fit ([`OverviewFit`]: the legacy stretch, or preserve-aspect with a
//! letterbox), the image-to-world mapping of every bake ([`OverviewMapping`]), legend and scale
//! metadata ([`OverviewLegend`]), a marker overlay ([`draw_markers`]) and a cancel/progress
//! observer ([`bake_overview_registered`]). The registered artifact (descriptor, cache, atomic
//! publication) is `crate::overview_artifact`. Overview images are presentation-only: nothing in
//! the simulation reads them.

use std::ops::ControlFlow;

use crate::source::{SourceError, TerrainSource};

/// How a source's world rectangle fills the output image (ADR-8596).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OverviewFit {
    /// The source rectangle fills the whole image, one scale per axis (the legacy mode: a square
    /// output stretches a rectangular source). Images are byte-identical to earlier releases.
    #[default]
    Stretch,
    /// One scale on both axes: the source keeps its world aspect ratio and is centered, the rest
    /// of the image is [`LETTERBOX_RGB`].
    PreserveAspect,
}

impl OverviewFit {
    /// The stable name (`"stretch"`, `"preserve_aspect"`) the agent, CLI and descriptor use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stretch => "stretch",
            Self::PreserveAspect => "preserve_aspect",
        }
    }

    /// Parses [`Self::as_str`]'s names.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "stretch" => Some(Self::Stretch),
            "preserve_aspect" => Some(Self::PreserveAspect),
            _ => None,
        }
    }
}

/// Appearance and output budgets of a terrain preview.
#[derive(Clone, Debug)]
pub struct OverviewSettings {
    /// Output `[width, height]`; with [`OverviewFit::Stretch`] a square intentionally stretches
    /// rectangular sources.
    pub size: [u32; 2],
    /// Maximum output pixels (checked before allocation).
    pub max_pixels: usize,
    /// Nonnegative vertical exaggeration used only for shading.
    pub relief: f32,
    /// Contour spacing in meters, or `None` to disable contours.
    pub contour_step_m: Option<f32>,
    /// Separable five-tap blur iterations, 0..=8 (0 preserves the area average).
    pub blur_passes: u8,
    /// Direction from the terrain to the light, `[X, up, Z]`; need not be normalized.
    pub light: [f32; 3],
    /// Brightness on flat ground, 0..=1, before elevation modulation.
    pub ambient: f32,
    /// Shading contrast, finite and nonnegative.
    pub contrast: f32,
    /// Optional gray border width in pixels; zero disables it. With
    /// [`OverviewFit::PreserveAspect`] it frames the source rectangle, not the letterbox.
    pub frame_pixels: u32,
    /// How the source rectangle fills `size` (ADR-8596); the default is the legacy stretch.
    pub fit: OverviewFit,
}

impl Default for OverviewSettings {
    fn default() -> Self {
        Self {
            size: [512, 512],
            max_pixels: 4 << 20,
            relief: 1.0,
            contour_step_m: None,
            blur_passes: 1,
            light: [-1.0, 1.0, 1.0],
            ambient: 0.5,
            contrast: 1.0,
            frame_pixels: 0,
            fit: OverviewFit::Stretch,
        }
    }
}

/// The color of an overview pixel whose footprint holds no source data (ADR-8542): magenta,
/// never a shade of the gray relief.
pub const MISSING_RGB: [u8; 3] = [255, 0, 255];

/// The color of the letterbox around a [`OverviewFit::PreserveAspect`] source rectangle: navy,
/// never a shade of the gray relief and never [`MISSING_RGB`].
pub const LETTERBOX_RGB: [u8; 3] = [0, 0, 128];

/// Default marker color: pure red, never a shade of the gray relief.
pub const MARKER_RGB: [u8; 3] = [255, 0, 0];

/// Pixels of each arm of a marker's plus sign (the sign is `2 * arm + 1` pixels across).
pub const MARKER_ARM_PIXELS: u32 = 3;

/// RGB8 north-up image, with column zero at minimum X.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverviewImage {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
    missing_pixels: usize,
}
impl OverviewImage {
    /// Pixels painted [`MISSING_RGB`] because no sample under them holds data (0 for a source
    /// without missing samples).
    pub fn missing_pixels(&self) -> usize {
        self.missing_pixels
    }
    /// Width and height in pixels.
    pub fn dimensions(&self) -> [u32; 2] {
        [self.width, self.height]
    }
    /// Row-major RGB bytes; row zero is maximum Z (north).
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }
    /// Encode a lossless RGB8 PNG without timestamps or source identifiers.
    ///
    /// # Errors
    /// Returns the PNG encoder's error as a source error.
    pub fn encode_png(&self) -> Result<Vec<u8>, SourceError> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, self.width, self.height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(|e| SourceError(e.to_string()))?;
            writer
                .write_image_data(&self.rgb)
                .map_err(|e| SourceError(e.to_string()))?;
            writer.finish().map_err(|e| SourceError(e.to_string()))?;
        }
        Ok(bytes)
    }
}

/// Bake an overview without a GPU or filesystem access.
///
/// Per-axis spacing is retained for slope calculations even when the output is square.
/// Samples without data ([`TerrainSource::valid_mask`]) never contribute a height: a pixel with
/// no valid sample under it is painted [`MISSING_RGB`], and the others average their valid
/// samples only.
/// Pixel heights are area averages; only the preview is blurred. Cook-side f64
/// accumulators avoid overflow for all validated f32 source mappings.
/// [`OverviewSettings::fit`] chooses stretch (legacy) or preserve-aspect with a letterbox.
///
/// # Errors
/// Refuses invalid settings or an output larger than the caller's pixel budget.
#[tracing::instrument(skip_all, target = "regia_terrain")]
pub fn bake_overview(
    source: &TerrainSource,
    settings: &OverviewSettings,
) -> Result<OverviewImage, SourceError> {
    bake_overview_registered(source, settings, |_| ControlFlow::Continue(())).map(|baked| baked.image)
}

/// Where a bake is between its start and its end (ADR-8596): `step` of `steps`, one step per
/// output row of the area average, one per blur pass and one per output row of the shading.
/// The count depends only on the settings, so a test cancels at an exact point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverviewProgress {
    /// Steps finished.
    pub step: u32,
    /// Steps in the bake.
    pub steps: u32,
}

/// The image-to-world mapping of an overview (ADR-8596).
///
/// Image coordinates are continuous pixels: `(0, 0)` is the top-left corner of pixel `(0, 0)`,
/// `u` grows east (+X) and `v` grows south (-Z); the center of pixel `(px, py)` is
/// `(px + 0.5, py + 0.5)`. The source rectangle covers `content_px` (`[x, y, width, height]`)
/// and spans the world from `world_min` to `world_max` (X, Z): the outer edges of the source's
/// cells, half a spacing beyond the first and last sample centers. Row zero of the image is north
/// (maximum Z); column zero is west (minimum X).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverviewMapping {
    /// Output `[width, height]`.
    pub image_size: [u32; 2],
    /// The source rectangle in the image, `[x, y, width, height]` pixels; the whole image for
    /// [`OverviewFit::Stretch`].
    pub content_px: [u32; 4],
    /// World X/Z of the source rectangle's west/south edges, meters.
    pub world_min: [f64; 2],
    /// World X/Z of the source rectangle's east/north edges, meters.
    pub world_max: [f64; 2],
}

impl OverviewMapping {
    /// The mapping of `source` into a `content_px` rectangle of an `image_size` image.
    #[must_use]
    pub fn of_source(source: &TerrainSource, image_size: [u32; 2], content_px: [u32; 4]) -> Self {
        let [cols, rows] = source.dimensions();
        let [sx, sz] = source.spacing_m().map(f64::from);
        let [x0, z0] = source.sample0().map(f64::from);
        Self {
            image_size,
            content_px,
            world_min: [x0 - 0.5 * sx, z0 - 0.5 * sz],
            world_max: [
                x0 + (f64::from(cols) - 0.5) * sx,
                z0 + (f64::from(rows) - 0.5) * sz,
            ],
        }
    }

    /// Meters per pixel on X (east) and Z (north). Equal up to the rounding of the content size for
    /// [`OverviewFit::PreserveAspect`].
    #[must_use]
    pub fn meters_per_pixel(&self) -> [f64; 2] {
        [
            (self.world_max[0] - self.world_min[0]) / f64::from(self.content_px[2]),
            (self.world_max[1] - self.world_min[1]) / f64::from(self.content_px[3]),
        ]
    }

    /// World X/Z of a continuous image position (it may lie in the letterbox or off the image:
    /// the mapping extends linearly).
    #[must_use]
    pub fn pixel_to_world(&self, [u, v]: [f64; 2]) -> [f64; 2] {
        let [mx, mz] = self.meters_per_pixel();
        [
            self.world_min[0] + (u - f64::from(self.content_px[0])) * mx,
            self.world_max[1] - (v - f64::from(self.content_px[1])) * mz,
        ]
    }

    /// The continuous image position of world X/Z (the inverse of [`Self::pixel_to_world`]).
    #[must_use]
    pub fn world_to_pixel(&self, [x, z]: [f64; 2]) -> [f64; 2] {
        let [mx, mz] = self.meters_per_pixel();
        [
            f64::from(self.content_px[0]) + (x - self.world_min[0]) / mx,
            f64::from(self.content_px[1]) + (self.world_max[1] - z) / mz,
        ]
    }

    /// The pixel holding world X/Z, or `None` outside the source rectangle. A point on a shared
    /// edge belongs to the pixel east/south of it; the east and south outer edges are inside.
    #[must_use]
    pub fn world_pixel(&self, xz: [f64; 2]) -> Option<[u32; 2]> {
        let [u, v] = self.world_to_pixel(xz);
        let [cx, cy, cw, ch] = self.content_px.map(f64::from);
        if !(u >= cx && u <= cx + cw && v >= cy && v <= cy + ch) {
            return None;
        }
        let px = (u.floor() as u32).min(self.content_px[0] + self.content_px[2] - 1);
        let py = (v.floor() as u32).min(self.content_px[1] + self.content_px[3] - 1);
        Some([px, py])
    }

    /// Whether pixel `(px, py)` shows the source (false in the letterbox).
    #[must_use]
    pub fn in_content(&self, px: u32, py: u32) -> bool {
        let [cx, cy, cw, ch] = self.content_px;
        px >= cx && py >= cy && px < cx + cw && py < cy + ch
    }
}

/// Reading aids of an overview (ADR-8596): what a brightness and a color mean, and a scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverviewLegend {
    /// Heights at the 1st and 99th percentile of the (blurred) pixel heights, meters: brightness
    /// is modulated from 70 % at the first to 100 % at the second.
    pub elevation_range_m: [f64; 2],
    /// A round scale-bar length (1, 2 or 5 times a power of ten meters), at most a quarter of the
    /// source rectangle's width.
    pub scale_bar_m: f64,
    /// That length in pixels on X.
    pub scale_bar_px: f64,
}

/// A point of interest drawn over an overview (ADR-8596).
#[derive(Clone, Debug, PartialEq)]
pub struct OverviewMarker {
    /// What it is (an entity name, a spawn, ...); recorded, not drawn.
    pub label: String,
    /// World X/Z, meters.
    pub world_xz: [f64; 2],
    /// Plus-sign color.
    pub rgb: [u8; 3],
}

/// A marker after drawing: where it landed.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedMarker {
    /// The marker's label.
    pub label: String,
    /// The marker's world X/Z.
    pub world_xz: [f64; 2],
    /// The pixel at its center ([`OverviewMapping::world_pixel`]); `None` outside the source
    /// rectangle (then nothing is drawn).
    pub pixel: Option<[u32; 2]>,
}

/// An overview with its mapping and legend.
#[derive(Clone, Debug, PartialEq)]
pub struct RegisteredOverview {
    /// The image.
    pub image: OverviewImage,
    /// Image-to-world mapping.
    pub mapping: OverviewMapping,
    /// Legend and scale.
    pub legend: OverviewLegend,
}

/// [`bake_overview`] with its image-to-world mapping and legend, reporting progress to
/// `observer`; [`ControlFlow::Break`] cancels the bake (`terrain_overview_cancelled`).
///
/// # Errors
/// [`bake_overview`]'s refusals and the cancel.
pub fn bake_overview_registered(
    source: &TerrainSource,
    settings: &OverviewSettings,
    mut observer: impl FnMut(OverviewProgress) -> ControlFlow<()>,
) -> Result<RegisteredOverview, SourceError> {
    let [width, height] = settings.size;
    let count = u64::from(width) * u64::from(height);
    if width < 2 || height < 2 || count > settings.max_pixels as u64 || count > (usize::MAX / 24) as u64 {
        return Err(SourceError(
            "overview size must be at least 2x2 and fit max_pixels".into(),
        ));
    }
    let content_px = match settings.fit {
        OverviewFit::Stretch => [0, 0, width, height],
        OverviewFit::PreserveAspect => fit_content(source, settings.size),
    };
    let mapping = OverviewMapping::of_source(source, settings.size, content_px);
    let mut inner = settings.clone();
    inner.size = [content_px[2], content_px[3]];
    let (content, elevation_range_m) = bake_content(source, &inner, &mut observer)?;
    let image = if content_px == [0, 0, width, height] {
        content
    } else {
        let mut rgb = Vec::with_capacity(count as usize * 3);
        for py in 0..height {
            for px in 0..width {
                if mapping.in_content(px, py) {
                    let i = ((py - content_px[1]) * content_px[2] + (px - content_px[0])) as usize * 3;
                    rgb.extend_from_slice(&content.rgb[i..i + 3]);
                } else {
                    rgb.extend_from_slice(&LETTERBOX_RGB);
                }
            }
        }
        OverviewImage {
            width,
            height,
            rgb,
            missing_pixels: content.missing_pixels,
        }
    };
    let [mx, _] = mapping.meters_per_pixel();
    let scale_bar_m = round_scale(f64::from(content_px[2]) * mx / 4.0);
    Ok(RegisteredOverview {
        image,
        mapping,
        legend: OverviewLegend {
            elevation_range_m,
            scale_bar_m,
            scale_bar_px: scale_bar_m / mx,
        },
    })
}

/// The centered source rectangle that keeps the world aspect ratio inside `[width, height]`.
fn fit_content(source: &TerrainSource, [width, height]: [u32; 2]) -> [u32; 4] {
    let [cols, rows] = source.dimensions();
    let [sx, sz] = source.spacing_m().map(f64::from);
    let aspect = f64::from(cols) * sx / (f64::from(rows) * sz);
    let (w, h) = (f64::from(width), f64::from(height));
    let (cw, ch) = if w / h >= aspect {
        (((h * aspect).round() as u32).clamp(2, width), height)
    } else {
        (width, ((w / aspect).round() as u32).clamp(2, height))
    };
    [(width - cw) / 2, (height - ch) / 2, cw, ch]
}

/// The largest 1, 2 or 5 times a power of ten that is at most `limit` (`limit` itself when it is
/// not positive and finite).
fn round_scale(limit: f64) -> f64 {
    if !(limit.is_finite() && limit > 0.0) {
        return limit;
    }
    let decade = 10f64.powf(limit.log10().floor());
    [5.0, 2.0, 1.0]
        .into_iter()
        .map(|m| m * decade)
        .find(|v| *v <= limit * (1.0 + 1e-12))
        .unwrap_or(decade)
}

/// Draws each marker as a plus sign ([`MARKER_ARM_PIXELS`]) clipped to the source rectangle, in
/// the given order (a later marker paints over an earlier one), and reports where each landed.
pub fn draw_markers(
    image: &mut OverviewImage,
    mapping: &OverviewMapping,
    markers: &[OverviewMarker],
) -> Vec<PlacedMarker> {
    let arm = MARKER_ARM_PIXELS as i64;
    markers
        .iter()
        .map(|marker| {
            let pixel = mapping.world_pixel(marker.world_xz);
            if let Some([px, py]) = pixel {
                for d in -arm..=arm {
                    for (x, y) in [
                        (i64::from(px) + d, i64::from(py)),
                        (i64::from(px), i64::from(py) + d),
                    ] {
                        let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
                            continue;
                        };
                        if mapping.in_content(x, y) && x < image.width && y < image.height {
                            let i = (y * image.width + x) as usize * 3;
                            image.rgb[i..i + 3].copy_from_slice(&marker.rgb);
                        }
                    }
                }
            }
            PlacedMarker {
                label: marker.label.clone(),
                world_xz: marker.world_xz,
                pixel,
            }
        })
        .collect()
}

/// The bake of the source rectangle at `settings.size`, and the elevation range of its brightness.
fn bake_content(
    source: &TerrainSource,
    settings: &OverviewSettings,
    observer: &mut dyn FnMut(OverviewProgress) -> ControlFlow<()>,
) -> Result<(OverviewImage, [f64; 2]), SourceError> {
    let [width, height] = settings.size;
    let count = u64::from(width) * u64::from(height);
    let light = settings.light.map(f64::from);
    let light_len = light.iter().map(|v| v * v).sum::<f64>().sqrt();
    if width < 2 || height < 2 || count > settings.max_pixels as u64 || count > (usize::MAX / 24) as u64 {
        return Err(SourceError(
            "overview size must be at least 2x2 and fit max_pixels".into(),
        ));
    }
    if !settings.relief.is_finite()
        || settings.relief < 0.0
        || settings.blur_passes > 8
        || !light_len.is_finite()
        || light_len == 0.0
        || !settings.ambient.is_finite()
        || !(0.0..=1.0).contains(&settings.ambient)
        || !settings.contrast.is_finite()
        || settings.contrast < 0.0
        || settings
            .contour_step_m
            .is_some_and(|v| !v.is_finite() || v <= 0.0)
        || settings.frame_pixels > width.min(height) / 2
    {
        return Err(SourceError(
            "invalid overview relief, blur, light, ambient, contrast, contour or frame".into(),
        ));
    }
    let light = light.map(|v| v / light_len);
    let masked = source.missing_samples() > 0;
    let steps = 2 * height + u32::from(settings.blur_passes);
    let mut step = 0;
    let mut tick = || {
        step += 1;
        match observer(OverviewProgress { step, steps }) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => Err(SourceError(format!(
                "terrain_overview_cancelled: the overview bake was cancelled after step {step} of {steps}"
            ))),
        }
    };
    let mut heights = if masked {
        area_average_masked(source, settings.size, &mut tick)?
    } else {
        area_average(source, settings.size, &mut tick)?
    };
    for _ in 0..settings.blur_passes {
        heights = if masked {
            blur_masked(&heights, width as usize, height as usize)
        } else {
            blur(&heights, width as usize, height as usize)
        };
        tick()?;
    }
    let mut sorted: Vec<f64> = heights.iter().copied().filter(|v| !v.is_nan()).collect();
    if sorted.is_empty() {
        return Err(SourceError(
            "terrain_raster_missing_data: no overview pixel holds data".into(),
        ));
    }
    sorted.sort_unstable_by(f64::total_cmp);
    let low = sorted[(sorted.len() - 1) / 100];
    let high = sorted[(sorted.len() - 1) * 99 / 100];
    drop(sorted);
    let [cols, rows] = source.dimensions();
    let [spacing_x, spacing_z] = source.spacing_m();
    let dx = f64::from(cols) * f64::from(spacing_x) / f64::from(width);
    let dz = f64::from(rows) * f64::from(spacing_z) / f64::from(height);
    // A missing neighbor contributes the center's own height (no slope toward the gap).
    let at = |i: usize, center: f64| {
        let v = heights[i];
        if v.is_nan() { center } else { v }
    };
    let mut missing_pixels = 0;
    let (w, h) = (width as usize, height as usize);
    let mut rgb = Vec::with_capacity(count as usize * 3);
    for y in 0..h {
        tick()?;
        for x in 0..w {
            let border = settings.frame_pixels as usize;
            let framed = x < border || y < border || x >= w - border || y >= h - border;
            let value = heights[y * w + x];
            if value.is_nan() && !framed {
                missing_pixels += 1;
                rgb.extend_from_slice(&MISSING_RGB);
                continue;
            }
            let (left, right) = (x.saturating_sub(1), (x + 1).min(w - 1));
            let (top, bottom) = (y.saturating_sub(1), (y + 1).min(h - 1));
            let gx = (at(y * w + right, value) - at(y * w + left, value)) * f64::from(settings.relief)
                / ((right - left) as f64 * dx);
            let gy = (at(bottom * w + x, value) - at(top * w + x, value)) * f64::from(settings.relief)
                / ((bottom - top) as f64 * dz);
            let normal_len = (gx * gx + gy * gy + 1.0).sqrt();
            let shade = ((-gx * light[0] + light[1] + gy * light[2]) / normal_len).clamp(0.0, 1.0);
            let flat = light[1].clamp(0.0, 1.0);
            let elevation = if high > low {
                ((value - low) / (high - low)).clamp(0.0, 1.0)
            } else {
                0.5
            };
            let mut lum =
                (f64::from(settings.ambient) + (shade - flat) * f64::from(settings.contrast)).clamp(0.0, 1.0);
            lum *= 0.7 + 0.3 * elevation;
            if let Some(step) = settings.contour_step_m {
                let band = |v: f64| (v / f64::from(step)).floor();
                if (x > 0 && band(value) != band(at(y * w + x - 1, value)))
                    || (y > 0 && band(value) != band(at((y - 1) * w + x, value)))
                {
                    lum *= 0.7;
                }
            }
            if framed {
                lum = 0.5;
            }
            let v = (lum * 255.0).round() as u8;
            rgb.extend_from_slice(&[v, v, v]);
        }
    }
    Ok((
        OverviewImage {
            width,
            height,
            rgb,
            missing_pixels,
        },
        [low, high],
    ))
}

/// One area-average or shading row finished: `Err` when the observer cancels.
type Tick<'a> = dyn FnMut() -> Result<(), SourceError> + 'a;

fn area_average(
    source: &TerrainSource,
    [width, height]: [u32; 2],
    tick: &mut Tick<'_>,
) -> Result<Vec<f64>, SourceError> {
    let [cols, rows] = source.dimensions();
    let sx = f64::from(cols) / f64::from(width);
    let sy = f64::from(rows) / f64::from(height);
    let mut out = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        let (y0, y1) = (f64::from(y) * sy, f64::from(y + 1) * sy);
        for x in 0..width {
            let (x0, x1) = (f64::from(x) * sx, f64::from(x + 1) * sx);
            // Accumulate deviations from a local reference, preserving constant fields
            // exactly even with fractional overlap weights (no percentile amplification).
            let reference = f64::from(source.height_m(x0.floor() as u32, rows - 1 - y0.floor() as u32));
            let mut sum = 0.0;
            for iy in y0.floor() as u32..(y1.ceil() as u32).min(rows) {
                let wy = (y1.min(f64::from(iy + 1)) - y0.max(f64::from(iy))).max(0.0);
                for ix in x0.floor() as u32..(x1.ceil() as u32).min(cols) {
                    let wx = (x1.min(f64::from(ix + 1)) - x0.max(f64::from(ix))).max(0.0);
                    sum += (f64::from(source.height_m(ix, rows - 1 - iy)) - reference) * wx * wy;
                }
            }
            out.push(reference + sum / (sx * sy));
        }
        tick()?;
    }
    Ok(out)
}

/// [`area_average`] over valid samples only; a pixel whose footprint holds no valid sample is NaN.
fn area_average_masked(
    source: &TerrainSource,
    [width, height]: [u32; 2],
    tick: &mut Tick<'_>,
) -> Result<Vec<f64>, SourceError> {
    let [cols, rows] = source.dimensions();
    let sx = f64::from(cols) / f64::from(width);
    let sy = f64::from(rows) / f64::from(height);
    let mut out = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        let (y0, y1) = (f64::from(y) * sy, f64::from(y + 1) * sy);
        for x in 0..width {
            let (x0, x1) = (f64::from(x) * sx, f64::from(x + 1) * sx);
            let (mut reference, mut sum, mut weight) = (None, 0.0, 0.0);
            for iy in y0.floor() as u32..(y1.ceil() as u32).min(rows) {
                let wy = (y1.min(f64::from(iy + 1)) - y0.max(f64::from(iy))).max(0.0);
                for ix in x0.floor() as u32..(x1.ceil() as u32).min(cols) {
                    let wx = (x1.min(f64::from(ix + 1)) - x0.max(f64::from(ix))).max(0.0);
                    let Some(h) = source.sample_height_m(ix, rows - 1 - iy) else {
                        continue;
                    };
                    if wx * wy <= 0.0 {
                        continue;
                    }
                    let r = *reference.get_or_insert(f64::from(h));
                    sum += (f64::from(h) - r) * wx * wy;
                    weight += wx * wy;
                }
            }
            out.push(reference.map_or(f64::NAN, |r| r + sum / weight));
        }
        tick()?;
    }
    Ok(out)
}

/// [`blur`] that skips NaN (missing) taps and renormalizes; a missing pixel stays missing.
fn blur_masked(input: &[f64], width: usize, height: usize) -> Vec<f64> {
    let weights = [1.0, 4.0, 6.0, 4.0, 1.0];
    let pass = |src: &[f64], horizontal: bool| -> Vec<f64> {
        let mut out = vec![f64::NAN; src.len()];
        for y in 0..height {
            for x in 0..width {
                let center = src[y * width + x];
                if center.is_nan() {
                    continue;
                }
                let (mut sum, mut total) = (0.0, 0.0);
                for (k, weight) in weights.iter().enumerate() {
                    let i = if horizontal {
                        y * width + (x + k).saturating_sub(2).min(width - 1)
                    } else {
                        (y + k).saturating_sub(2).min(height - 1) * width + x
                    };
                    if !src[i].is_nan() {
                        sum += (src[i] - center) * weight;
                        total += weight;
                    }
                }
                out[y * width + x] = center + sum / total;
            }
        }
        out
    };
    pass(&pass(input, true), false)
}

// Pascal's five-tap [1, 4, 6, 4, 1] / 16 approximates a unit-sigma Gaussian.
// Clamped edges retain constant fields, including rectangular and two-pixel grids.
fn blur(input: &[f64], width: usize, height: usize) -> Vec<f64> {
    let weights = [1.0, 4.0, 6.0, 4.0, 1.0];
    let mut tmp = vec![0.0; input.len()];
    let mut out = vec![0.0; input.len()];
    for y in 0..height {
        for x in 0..width {
            for (k, weight) in weights.iter().enumerate() {
                let col = (x + k).saturating_sub(2).min(width - 1);
                tmp[y * width + x] += (input[y * width + col] - input[y * width + x]) * weight / 16.0;
            }
            tmp[y * width + x] += input[y * width + x];
        }
    }
    for y in 0..height {
        for x in 0..width {
            for (k, weight) in weights.iter().enumerate() {
                let row = (y + k).saturating_sub(2).min(height - 1);
                out[y * width + x] += (tmp[row * width + x] - tmp[y * width + x]) * weight / 16.0;
            }
            out[y * width + x] += tmp[y * width + x];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blur_preserves_a_constant_rectangular_field() {
        assert_eq!(blur(&[3.0; 15], 5, 3), vec![3.0; 15]);
    }

    #[test]
    fn scale_bars_are_one_two_or_five_times_a_power_of_ten() {
        for (limit, bar) in [
            (3.0, 2.0),
            (1.0, 1.0),
            (9.99, 5.0),
            (10.0, 10.0),
            (2500.0, 2000.0),
        ] {
            assert_eq!(round_scale(limit), bar, "{limit}");
        }
    }
}
