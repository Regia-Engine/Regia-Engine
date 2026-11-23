// SPDX-License-Identifier: MPL-2.0
//! Photosensitivity check for a sequence of sRGB frames.
//!
//! The counting rules follow W3C WCAG 2.2 success criterion 2.3.1
//! (<https://www.w3.org/TR/WCAG22/#three-flashes-or-below-threshold>), the
//! definitions of the general flash and red flash thresholds
//! (<https://www.w3.org/TR/WCAG22/#dfn-general-flash-and-red-flash-thresholds>)
//! and relative luminance
//! (<https://www.w3.org/TR/WCAG22/#dfn-relative-luminance>), and the notes in
//! Understanding SC 2.3.1
//! (<https://www.w3.org/WAI/WCAG22/Understanding/three-flashes-or-below-threshold.html>).
//! The implementation is original. It is a development aid: it is not a
//! certification and it is not a guarantee that a sequence is safe to watch.
//!
//! # What is counted
//!
//! A general-flash transition is a change in relative luminance of at least
//! [`LUMINANCE_DELTA`] where the darker of the two states is strictly below
//! [`DARKER_LIMIT`]. Red transitions differ by more than [`RED_DELTA`] in
//! CIE 1976 UCS chromaticity. At least one transition of a red flash involves
//! a saturated red: `R / (R + G + B) ≥ `[`RED_RATIO`]
//! on the linearized channels. `R`, `G` and `B` are the linear values from the
//! relative-luminance definition, not encoded sRGB bytes. Alpha is ignored.
//!
//! The analyzer tracks peaks and valleys per cell, not the step between
//! consecutive frames. The opening sample is an anchor. Each monotonic leg
//! counts once when it reaches the threshold. A reversal is confirmed only
//! when the step back from the running extreme meets the threshold. Two
//! opposing transitions are one flash, recorded as the second qualifies,
//! even when that transition is the final frame. For chromaticity, opposing
//! directions have a negative dot product in the UCS plane.
//! Pairs do not overlap, so a light that flashes three times in one second
//! stays at the WCAG limit.
//!
//! # Area and the 10 degree field
//!
//! A sequence fails when some one-second window holds more than
//! [`MAX_FLASHES_PER_SECOND`] flashes of one kind and the cells that exceed
//! that count occupy more than [`AREA_LIMIT`] of any 10 degree field. The
//! field is 341×256 px on a 1024×768 screen in the W3C note; [`ten_degree_field_px`]
//! scales each axis with integer division (rounding down) and clamps it to the
//! frame. The search window is then rounded up to the cell grid. Cells that
//! each exceed three flashes in the same one-second window are summed even
//! when their peaks fall on different frames inside that window.
//!
//! The WCAG exception for a balanced pattern of squares smaller than 0.1
//! degree is not applied.
//!
//! # Memory
//!
//! [`FlashAnalyzer::push_frame`] borrows each frame and does not retain the
//! pixels. Live state is the per-cell tracker plus the flash timestamps that
//! still fall inside the latest one-second window.

use std::cmp::Ordering;
use std::collections::VecDeque;

#[cfg(test)]
thread_local! {
    /// 10-degree windows [`observe`] actually sums. Steady frames whose cell
    /// counts did not change must not add to this.
    static WINDOW_EVALS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

use serde::Serialize;

use crate::math::{Color, srgb_to_linear};

/// Maximum general or red flashes in any one-second window (WCAG 2.2 SC 2.3.1).
pub const MAX_FLASHES_PER_SECOND: u32 = 3;

/// Combined flashing area must be no more than this fraction of a 10 degree field.
pub const AREA_LIMIT: f64 = 0.25;

/// A general-flash transition changes relative luminance by at least this much.
pub const LUMINANCE_DELTA: f32 = 0.10;

/// The darker state of a general flash is strictly below this relative luminance.
pub const DARKER_LIMIT: f32 = 0.80;

/// Saturated red: linearized `R / (R + G + B)` is at least this.
pub const RED_RATIO: f32 = 0.8;

/// A red-flash transition differs by more than this in CIE 1976 UCS chromaticity.
pub const RED_DELTA: f32 = 0.2;

/// Largest frame side accepted by [`FlashAnalyzer::new`].
const MAX_SIDE: u32 = 8192;

/// Largest frame area, in pixels, accepted by [`FlashAnalyzer::new`].
const MAX_PIXELS: u64 = 4096 * 4096;

/// Largest cell grid accepted by [`FlashAnalyzer::new`].
const MAX_CELLS: u64 = 2_500_000;

/// How a 10 degree field maps onto the frame being analyzed.
///
/// The defaults are the W3C note: 341×256 px on a 1024×768 screen at a typical
/// viewing distance. Each axis scales by the ratio of the frame to that
/// reference, so a different screen size or a documented viewing distance is a
/// different [`ViewingGeometry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ViewingGeometry {
    /// Reference screen width in pixels. The W3C note uses 1024.
    pub reference_width: u32,
    /// Reference screen height in pixels. The W3C note uses 768.
    pub reference_height: u32,
    /// Width of a 10 degree field on the reference screen. The W3C note uses 341.
    pub ten_degree_width_px: u32,
    /// Height of a 10 degree field on the reference screen. The W3C note uses 256.
    pub ten_degree_height_px: u32,
}

impl Default for ViewingGeometry {
    fn default() -> Self {
        Self {
            reference_width: 1024,
            reference_height: 768,
            ten_degree_width_px: 341,
            ten_degree_height_px: 256,
        }
    }
}

/// Cell size and viewing geometry for a flash analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FlashConfig {
    /// How the 10 degree field scales with the frame.
    pub geometry: ViewingGeometry,
    /// Square cell edge in pixels. The cell is tested as its mean linear color.
    ///
    /// `1` tests every pixel. A larger cell bounds the work. It can also hide a
    /// flash that does not move the cell mean across the threshold, and it
    /// rounds the 10 degree window up to a whole number of cells.
    pub cell_px: u32,
}

impl Default for FlashConfig {
    fn default() -> Self {
        Self {
            geometry: ViewingGeometry::default(),
            cell_px: 8,
        }
    }
}

/// Why a flash analysis stopped.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FlashError {
    /// [`FlashAnalyzer::finish`] was called before any frame.
    #[error("no frames were pushed")]
    NoFrames,
    /// Width or height is zero.
    #[error("frame width and height must be non-zero")]
    EmptyFrame,
    /// A frame's byte length does not match `width × height × 4`.
    #[error("frame has {actual_bytes} bytes, expected {expected_bytes}")]
    FrameSizeMismatch {
        /// Bytes `width × height × 4` requires.
        expected_bytes: usize,
        /// Bytes the caller passed.
        actual_bytes: usize,
    },
    /// A timestamp was NaN, infinite, or not strictly later than the previous one.
    #[error("frame time {time_s} is not after {previous_s}")]
    TimeNotMonotonic {
        /// Timestamp of the previous accepted frame, or the rejected time when it was the first.
        previous_s: f64,
        /// Timestamp that was rejected.
        time_s: f64,
    },
    /// A viewing-geometry or cell size was zero.
    #[error("viewing geometry or cell size is zero")]
    Geometry,
    /// The frame or the cell grid is above the analyzer's memory cap.
    #[error("frame {width}x{height} at cell {cell_px}px exceeds the analysis cap")]
    FrameTooLarge {
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
        /// Requested cell edge in pixels.
        cell_px: u32,
    },
}

impl FlashError {
    /// Stable lowercase code for the agent protocol and the offline tool.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoFrames => "no_frames",
            Self::EmptyFrame => "empty_frame",
            Self::FrameSizeMismatch { .. } => "frame_size_mismatch",
            Self::TimeNotMonotonic { .. } => "time_not_monotonic",
            Self::Geometry => "geometry_out_of_range",
            Self::FrameTooLarge { .. } => "frame_too_large",
        }
    }
}

/// Which threshold a reported window crossed, or the louder of the two when neither did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FlashKind {
    /// Relative-luminance flashes.
    General,
    /// Saturated-red flashes.
    Red,
}

/// A pixel rectangle on the analyzed frame. Origin is the top-left, y grows downward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct FlashRect {
    /// Left edge, in pixels.
    pub x: u32,
    /// Top edge, in pixels.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Result of a finished flash analysis.
///
/// `passed` means neither kind had more than [`MAX_FLASHES_PER_SECOND`] flashes
/// inside a one-second window whose flashing cells exceed [`AREA_LIMIT`] of a
/// 10 degree field. A pass is not a certification.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FlashReport {
    /// `false` when either kind fails the count and area tests together.
    pub passed: bool,
    /// Flash count of [`Self::kind`] in the reported window.
    pub max_flashes: u32,
    /// Start of the reported one-second window, in seconds (`end − 1`, floored at 0).
    pub window_start_s: f64,
    /// Cell-aligned 10 degree window the area fraction was measured in.
    pub region: FlashRect,
    /// Fraction of [`Self::region`] occupied by cells with more than three flashes.
    pub area_fraction: f64,
    /// Kind the headline fields describe.
    pub kind: FlashKind,
    /// Highest general-flash count in any one-second window.
    pub general_flashes: u32,
    /// Highest red-flash count in any one-second window.
    pub red_flashes: u32,
    /// Area fraction of the general-flash window that was kept.
    pub general_area_fraction: f64,
    /// Area fraction of the red-flash window that was kept.
    pub red_area_fraction: f64,
    /// Start of the kept general-flash window, in seconds.
    pub general_window_start_s: f64,
    /// Start of the kept red-flash window, in seconds.
    pub red_window_start_s: f64,
    /// Frames accepted by [`FlashAnalyzer::push_frame`].
    pub frames: u32,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Cell edge in pixels used for this analysis.
    pub cell_px: u32,
    /// 10 degree field width in pixels, before rounding up to the cell grid.
    pub field_width: u32,
    /// 10 degree field height in pixels, before rounding up to the cell grid.
    pub field_height: u32,
}

/// Pixel size of a 10 degree field on a frame of `frame_w` by `frame_h`.
///
/// Each axis is `(ten_degree_px * frame) / reference`, rounded down, then
/// raised to at least 1 and clamped to the frame. Rounding down makes the
/// field slightly smaller, so the area test is slightly stricter.
///
/// # Errors
///
/// Returns [`FlashError::EmptyFrame`] when a frame side is 0, and
/// [`FlashError::Geometry`] when a geometry field is 0.
#[must_use = "the field size is the result of this function"]
pub fn ten_degree_field_px(
    frame_w: u32,
    frame_h: u32,
    geometry: &ViewingGeometry,
) -> Result<(u32, u32), FlashError> {
    if frame_w == 0 || frame_h == 0 {
        return Err(FlashError::EmptyFrame);
    }
    if geometry.reference_width == 0
        || geometry.reference_height == 0
        || geometry.ten_degree_width_px == 0
        || geometry.ten_degree_height_px == 0
    {
        return Err(FlashError::Geometry);
    }
    let w =
        u64::from(geometry.ten_degree_width_px) * u64::from(frame_w) / u64::from(geometry.reference_width);
    let h =
        u64::from(geometry.ten_degree_height_px) * u64::from(frame_h) / u64::from(geometry.reference_height);
    let w = u32::try_from(w).unwrap_or(u32::MAX).clamp(1, frame_w);
    let h = u32::try_from(h).unwrap_or(u32::MAX).clamp(1, frame_h);
    Ok((w, h))
}

/// Streaming flash analyzer. Pixels are borrowed per frame and then dropped.
pub struct FlashAnalyzer {
    grid: Grid,
    byte_len: usize,
    lut: [f32; 256],
    cells: Vec<CellTrack>,
    luma_counts: Vec<u32>,
    red_counts: Vec<u32>,
    cell_area: Vec<u32>,
    values: Vec<u32>,
    integral_total: Vec<u32>,
    integral_flash: Vec<u32>,
    previous_s: Option<f64>,
    frames: u32,
    general: Observation,
    red: Observation,
}

#[derive(Clone, Copy)]
struct Grid {
    width: u32,
    height: u32,
    cell_px: u32,
    cols: u32,
    rows: u32,
    win_w: u32,
    win_h: u32,
    field_w: u32,
    field_h: u32,
    origin: FlashRect,
}

#[derive(Default)]
struct Tracker {
    started: bool,
    from: [f32; 2],
    extreme: [f32; 2],
    from_sat: bool,
    extreme_sat: bool,
    counted: bool,
    armed: Option<([f32; 2], bool)>,
    flashes: VecDeque<f64>,
}

#[derive(Default)]
struct CellTrack {
    luma: Tracker,
    red: Tracker,
}

#[derive(Clone, Copy, Default)]
struct Observation {
    seen: bool,
    max_flashes: u32,
    window_start_s: f64,
    region: FlashRect,
    area_fraction: f64,
    failing: bool,
    flashing_px: u32,
}

impl std::fmt::Debug for FlashAnalyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlashAnalyzer")
            .field("width", &self.grid.width)
            .field("height", &self.grid.height)
            .field("cell_px", &self.grid.cell_px)
            .field("frames", &self.frames)
            .finish()
    }
}

struct AreaMaps<'a> {
    cell_area: &'a [u32],
    values: &'a mut [u32],
    total: &'a [u32],
    flash: &'a mut [u32],
}

#[derive(Clone, Copy)]
enum Mode {
    Luma,
    Red,
}

impl FlashAnalyzer {
    /// Begin an analysis for frames of `width` by `height`.
    ///
    /// # Errors
    ///
    /// Returns [`FlashError::EmptyFrame`], [`FlashError::Geometry`], or
    /// [`FlashError::FrameTooLarge`]. A frame side above 8192 pixels, an area
    /// above 4096×4096, or a cell grid above 2,500,000 cells is refused.
    pub fn new(width: u32, height: u32, config: FlashConfig) -> Result<Self, FlashError> {
        if width == 0 || height == 0 {
            return Err(FlashError::EmptyFrame);
        }
        if config.cell_px == 0 {
            return Err(FlashError::Geometry);
        }
        let (field_w, field_h) = ten_degree_field_px(width, height, &config.geometry)?;
        let cols = width.div_ceil(config.cell_px);
        let rows = height.div_ceil(config.cell_px);
        let pixels = u64::from(width) * u64::from(height);
        let cell_count = u64::from(cols) * u64::from(rows);
        if width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS || cell_count > MAX_CELLS {
            return Err(FlashError::FrameTooLarge {
                width,
                height,
                cell_px: config.cell_px,
            });
        }
        let n = fit_usize(cell_count, width, height, config.cell_px)?;
        let stride = fit_usize(u64::from(cols) + 1, width, height, config.cell_px)?;
        let row_stride = fit_usize(u64::from(rows) + 1, width, height, config.cell_px)?;
        let integral_len = stride.checked_mul(row_stride).ok_or(FlashError::FrameTooLarge {
            width,
            height,
            cell_px: config.cell_px,
        })?;
        let byte_len = fit_usize(pixels.saturating_mul(4), width, height, config.cell_px)?;
        let win_w = field_w.div_ceil(config.cell_px).clamp(1, cols);
        let win_h = field_h.div_ceil(config.cell_px).clamp(1, rows);
        let grid = Grid {
            width,
            height,
            cell_px: config.cell_px,
            cols,
            rows,
            win_w,
            win_h,
            field_w,
            field_h,
            origin: FlashRect {
                x: 0,
                y: 0,
                width: win_w.saturating_mul(config.cell_px).min(width),
                height: win_h.saturating_mul(config.cell_px).min(height),
            },
        };
        let mut cell_area = vec![0u32; n];
        fill_cell_area(&mut cell_area, &grid);
        let mut integral_total = vec![0u32; integral_len];
        build_integral(&mut integral_total, cols, rows, &cell_area);
        let mut lut = [0.0f32; 256];
        for byte in 0u8..=255 {
            lut[usize::from(byte)] = srgb_to_linear(f32::from(byte) / 255.0);
        }
        Ok(Self {
            grid,
            byte_len,
            lut,
            cells: std::iter::repeat_with(CellTrack::default).take(n).collect(),
            luma_counts: vec![0; n],
            red_counts: vec![0; n],
            cell_area,
            values: vec![0; n],
            integral_total,
            integral_flash: vec![0; integral_len],
            previous_s: None,
            frames: 0,
            general: Observation::default(),
            red: Observation::default(),
        })
    }

    /// Fold one RGBA8 sRGB frame into the trackers.
    ///
    /// `time_s` is the frame's position on the caller's clock. Capture at a
    /// fixed rate uses `index / fps`. The pixels are not stored.
    ///
    /// # Errors
    ///
    /// Returns [`FlashError::FrameSizeMismatch`] or [`FlashError::TimeNotMonotonic`].
    /// A rejected frame leaves the analyzer unchanged.
    pub fn push_frame(&mut self, rgba: &[u8], time_s: f64) -> Result<(), FlashError> {
        if rgba.len() != self.byte_len {
            return Err(FlashError::FrameSizeMismatch {
                expected_bytes: self.byte_len,
                actual_bytes: rgba.len(),
            });
        }
        if let Some(previous) = self.previous_s {
            if !(time_s.is_finite() && time_s > previous) {
                return Err(FlashError::TimeNotMonotonic {
                    previous_s: previous,
                    time_s,
                });
            }
        } else if !time_s.is_finite() {
            return Err(FlashError::TimeNotMonotonic {
                previous_s: time_s,
                time_s,
            });
        }
        let _span = tracing::debug_span!(
            target: "regia_core",
            "flash_frame",
            width = self.grid.width,
            height = self.grid.height,
            time_s,
        )
        .entered();
        let first = self.frames == 0;
        let (luma_changed, red_changed) = self.sample(rgba, time_s);
        self.previous_s = Some(time_s);
        self.frames += 1;
        // A quiet frame leaves every cell's one-second count where it was. The
        // stored window is the first one that reached the current maximum
        // (`prefer` does not replace an equal maximum), so searching again
        // cannot change the report.
        if first || luma_changed || red_changed {
            self.note(time_s, first || luma_changed, first || red_changed);
        }
        Ok(())
    }

    /// Finish the analysis.
    ///
    /// # Errors
    ///
    /// Returns [`FlashError::NoFrames`] when nothing was pushed.
    pub fn finish(self) -> Result<FlashReport, FlashError> {
        if self.frames == 0 {
            return Err(FlashError::NoFrames);
        }
        let kind = headline(&self.general, &self.red);
        let chosen = match kind {
            FlashKind::General => &self.general,
            FlashKind::Red => &self.red,
        };
        let (max_flashes, window_start_s, region, area_fraction) = published(chosen, self.grid.origin);
        let (general_flashes, general_window_start_s, _, general_area_fraction) =
            published(&self.general, self.grid.origin);
        let (red_flashes, red_window_start_s, _, red_area_fraction) = published(&self.red, self.grid.origin);
        Ok(FlashReport {
            passed: !self.general.failing && !self.red.failing,
            max_flashes,
            window_start_s,
            region,
            area_fraction,
            kind,
            general_flashes,
            red_flashes,
            general_area_fraction,
            red_area_fraction,
            general_window_start_s,
            red_window_start_s,
            frames: self.frames,
            width: self.grid.width,
            height: self.grid.height,
            cell_px: self.grid.cell_px,
            field_width: self.grid.field_w,
            field_height: self.grid.field_h,
        })
    }

    fn sample(&mut self, rgba: &[u8], time_s: f64) -> (bool, bool) {
        let cols = self.grid.cols;
        let rows = self.grid.rows;
        let mut luma_changed = false;
        let mut red_changed = false;
        for cy in 0..rows {
            for cx in 0..cols {
                let (luma, red, saturated) = self.mean_cell(rgba, cx, cy);
                let idx = (cy * cols + cx) as usize;
                let (luma_n, red_n) = {
                    let cell = &mut self.cells[idx];
                    push_tracker(&mut cell.luma, [luma, 0.0], false, time_s, Mode::Luma);
                    push_tracker(&mut cell.red, red, saturated, time_s, Mode::Red);
                    (cell.luma.flashes.len(), cell.red.flashes.len())
                };
                let luma_u = u32::try_from(luma_n).unwrap_or(u32::MAX);
                let red_u = u32::try_from(red_n).unwrap_or(u32::MAX);
                if self.luma_counts[idx] != luma_u {
                    self.luma_counts[idx] = luma_u;
                    luma_changed = true;
                }
                if self.red_counts[idx] != red_u {
                    self.red_counts[idx] = red_u;
                    red_changed = true;
                }
            }
        }
        (luma_changed, red_changed)
    }

    fn mean_cell(&self, rgba: &[u8], cx: u32, cy: u32) -> (f32, [f32; 2], bool) {
        let x0 = cx.saturating_mul(self.grid.cell_px);
        let y0 = cy.saturating_mul(self.grid.cell_px);
        let x1 = x0.saturating_add(self.grid.cell_px).min(self.grid.width);
        let y1 = y0.saturating_add(self.grid.cell_px).min(self.grid.height);
        let mut sr = 0.0f64;
        let mut sg = 0.0f64;
        let mut sb = 0.0f64;
        let mut n = 0u32;
        for y in y0..y1 {
            let row = (y * self.grid.width) as usize;
            for x in x0..x1 {
                let o = (row + x as usize) * 4;
                sr += f64::from(self.lut[rgba[o] as usize]);
                sg += f64::from(self.lut[rgba[o + 1] as usize]);
                sb += f64::from(self.lut[rgba[o + 2] as usize]);
                n += 1;
            }
        }
        if n == 0 {
            return (0.0, [0.0, 0.0], false);
        }
        let scale = f64::from(n);
        let r = (sr / scale) as f32;
        let g = (sg / scale) as f32;
        let b = (sb / scale) as f32;
        let sum = r + g + b;
        let saturated = sum > 0.0 && r >= RED_RATIO * sum;
        let red = chromaticity(r, g, b);
        let luma = Color::rgb(r, g, b).luminance();
        (luma, red, saturated)
    }

    fn note(&mut self, time_s: f64, luma: bool, red: bool) {
        let grid = self.grid;
        if luma {
            let general = observe(
                grid,
                &self.luma_counts,
                AreaMaps {
                    cell_area: &self.cell_area,
                    values: &mut self.values,
                    total: &self.integral_total,
                    flash: &mut self.integral_flash,
                },
                time_s,
            );
            if prefer(&general, &self.general) {
                self.general = general;
            }
        }
        if red {
            let red_obs = observe(
                grid,
                &self.red_counts,
                AreaMaps {
                    cell_area: &self.cell_area,
                    values: &mut self.values,
                    total: &self.integral_total,
                    flash: &mut self.integral_flash,
                },
                time_s,
            );
            if prefer(&red_obs, &self.red) {
                self.red = red_obs;
            }
        }
    }
}

fn fit_usize(n: u64, width: u32, height: u32, cell_px: u32) -> Result<usize, FlashError> {
    usize::try_from(n).map_err(|_| FlashError::FrameTooLarge {
        width,
        height,
        cell_px,
    })
}

fn fill_cell_area(area: &mut [u32], grid: &Grid) {
    for cy in 0..grid.rows {
        for cx in 0..grid.cols {
            let x0 = cx.saturating_mul(grid.cell_px);
            let y0 = cy.saturating_mul(grid.cell_px);
            let x1 = x0.saturating_add(grid.cell_px).min(grid.width);
            let y1 = y0.saturating_add(grid.cell_px).min(grid.height);
            area[(cy * grid.cols + cx) as usize] = x1.saturating_sub(x0) * y1.saturating_sub(y0);
        }
    }
}

fn build_integral(dst: &mut [u32], cols: u32, rows: u32, values: &[u32]) {
    let stride = cols as usize + 1;
    dst.fill(0);
    for y in 0..rows as usize {
        let mut row_sum = 0u32;
        for x in 0..cols as usize {
            row_sum += values[y * cols as usize + x];
            let above = dst[y * stride + x + 1];
            dst[(y + 1) * stride + x + 1] = row_sum + above;
        }
    }
}

fn rect_sum(integral: &[u32], cols: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> u32 {
    let stride = cols as usize + 1;
    let x0 = x0 as usize;
    let x1 = x1 as usize;
    let y0 = y0 as usize;
    let y1 = y1 as usize;
    let a = integral[y0 * stride + x0];
    let b = integral[y0 * stride + x1];
    let c = integral[y1 * stride + x0];
    let d = integral[y1 * stride + x1];
    d + a - b - c
}

// D65 sRGB primaries from W3C CSS Color 4, CIE e-ILV 17-23-073.
// Black has undefined chromaticity; (0, 0) is a conservative analysis convention.
fn chromaticity(r: f32, g: f32, b: f32) -> [f32; 2] {
    let x = 0.412_390_8 * r + 0.357_584_33 * g + 0.180_480_8 * b;
    let y = 0.212_639 * r + 0.715_168_65 * g + 0.072_192_32 * b;
    let z = 0.019_330_82 * r + 0.119_194_78 * g + 0.950_532_14 * b;
    let denominator = x + 15.0 * y + 3.0 * z;
    if denominator > 0.0 {
        [4.0 * x / denominator, 9.0 * y / denominator]
    } else {
        [0.0, 0.0]
    }
}

fn subtract(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

fn dot(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

fn push_tracker(tracker: &mut Tracker, metric: [f32; 2], saturated: bool, time_s: f64, mode: Mode) {
    if !tracker.started {
        tracker.started = true;
        tracker.from = metric;
        tracker.extreme = metric;
        tracker.from_sat = saturated;
        tracker.extreme_sat = saturated;
    } else {
        let step = subtract(metric, tracker.extreme);
        let direction = subtract(tracker.extreme, tracker.from);
        if dot(step, direction) >= 0.0 {
            tracker.extreme = metric;
            tracker.extreme_sat = saturated;
        } else if qualifies(mode, tracker.extreme, metric) {
            tracker.from = tracker.extreme;
            tracker.from_sat = tracker.extreme_sat;
            tracker.extreme = metric;
            tracker.extreme_sat = saturated;
            tracker.counted = false;
        }
        if !tracker.counted && qualifies(mode, tracker.from, tracker.extreme) {
            let transition = subtract(tracker.extreme, tracker.from);
            let red = tracker.from_sat || tracker.extreme_sat;
            if let Some((previous, previous_red)) = tracker.armed {
                if dot(previous, transition) < 0.0 && (matches!(mode, Mode::Luma) || previous_red || red) {
                    tracker.flashes.push_back(time_s);
                    tracker.armed = None;
                } else {
                    tracker.armed = Some((transition, red));
                }
            } else {
                tracker.armed = Some((transition, red));
            }
            tracker.counted = true;
        }
    }
    let keep_after = time_s - 1.0;
    while tracker.flashes.front().is_some_and(|t| *t <= keep_after) {
        tracker.flashes.pop_front();
    }
}

fn qualifies(mode: Mode, a: [f32; 2], b: [f32; 2]) -> bool {
    match mode {
        Mode::Luma => (a[0] - b[0]).abs() >= LUMINANCE_DELTA && a[0].min(b[0]) < DARKER_LIMIT,
        Mode::Red => {
            let delta = subtract(a, b);
            dot(delta, delta) > RED_DELTA * RED_DELTA
        }
    }
}

fn observe(grid: Grid, counts: &[u32], maps: AreaMaps<'_>, time_s: f64) -> Observation {
    let max_flashes = counts.iter().copied().max().unwrap_or(0);
    let window_start_s = (time_s - 1.0).max(0.0);
    if max_flashes <= MAX_FLASHES_PER_SECOND {
        return Observation {
            seen: true,
            max_flashes,
            window_start_s,
            region: grid.origin,
            area_fraction: 0.0,
            failing: false,
            flashing_px: 0,
        };
    }
    let mut total_flash = 0u32;
    for (i, area) in maps.cell_area.iter().copied().enumerate() {
        let value = if counts[i] > MAX_FLASHES_PER_SECOND {
            area
        } else {
            0
        };
        maps.values[i] = value;
        total_flash = total_flash.saturating_add(value);
    }
    build_integral(maps.flash, grid.cols, grid.rows, maps.values);
    let mut best_flash = 0u32;
    let mut best_total = 0u32;
    let mut best_x = 0u32;
    let mut best_y = 0u32;
    let y_last = grid.rows - grid.win_h;
    let x_last = grid.cols - grid.win_w;
    // No window can hold more flashing pixels than the frame does. The first
    // window that already holds all of them is the maximum (`>` keeps the
    // earliest tie).
    'windows: for y in 0..=y_last {
        for x in 0..=x_last {
            #[cfg(test)]
            WINDOW_EVALS.with(|evals| evals.set(evals.get() + 1));
            let flash = rect_sum(maps.flash, grid.cols, x, y, x + grid.win_w, y + grid.win_h);
            if flash > best_flash {
                best_flash = flash;
                best_total = rect_sum(maps.total, grid.cols, x, y, x + grid.win_w, y + grid.win_h);
                best_x = x;
                best_y = y;
            }
            if best_flash == total_flash {
                break 'windows;
            }
        }
    }
    let region = cell_rect(grid, best_x, best_y);
    let total = if best_total == 0 {
        region.width.saturating_mul(region.height)
    } else {
        best_total
    };
    let failing = total > 0 && u64::from(best_flash) * 4 > u64::from(total);
    let area_fraction = if total == 0 {
        0.0
    } else {
        f64::from(best_flash) / f64::from(total)
    };
    Observation {
        seen: true,
        max_flashes,
        window_start_s,
        region,
        area_fraction,
        failing,
        flashing_px: best_flash,
    }
}

fn cell_rect(grid: Grid, cx: u32, cy: u32) -> FlashRect {
    let x = cx.saturating_mul(grid.cell_px).min(grid.width);
    let y = cy.saturating_mul(grid.cell_px).min(grid.height);
    let x1 = cx
        .saturating_add(grid.win_w)
        .saturating_mul(grid.cell_px)
        .min(grid.width);
    let y1 = cy
        .saturating_add(grid.win_h)
        .saturating_mul(grid.cell_px)
        .min(grid.height);
    FlashRect {
        x,
        y,
        width: x1.saturating_sub(x),
        height: y1.saturating_sub(y),
    }
}

fn prefer(candidate: &Observation, current: &Observation) -> bool {
    if !current.seen {
        return true;
    }
    match (candidate.failing, current.failing) {
        (true, false) => true,
        (false, true) => false,
        _ => match candidate.max_flashes.cmp(&current.max_flashes) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => candidate.flashing_px > current.flashing_px,
        },
    }
}

fn headline(general: &Observation, red: &Observation) -> FlashKind {
    if general.failing != red.failing {
        if red.failing {
            FlashKind::Red
        } else {
            FlashKind::General
        }
    } else if red.max_flashes > general.max_flashes {
        FlashKind::Red
    } else {
        FlashKind::General
    }
}

fn published(obs: &Observation, origin: FlashRect) -> (u32, f64, FlashRect, f64) {
    if obs.seen {
        (obs.max_flashes, obs.window_start_s, obs.region, obs.area_fraction)
    } else {
        (0, 0.0, origin, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{linear_to_srgb, srgb_to_linear};

    fn paint(buf: &mut [u8], color: [u8; 4]) {
        let (pixels, _) = buf.as_chunks_mut::<4>();
        for pixel in pixels {
            *pixel = color;
        }
    }

    fn paint_rect(buf: &mut [u8], width: u32, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
        for row in y..(y + h) {
            for col in x..(x + w) {
                let i = ((row * width + col) as usize) * 4;
                buf[i] = color[0];
                buf[i + 1] = color[1];
                buf[i + 2] = color[2];
                buf[i + 3] = color[3];
            }
        }
    }

    fn quantize_linear(linear: f32) -> u8 {
        let encoded = linear_to_srgb(linear).clamp(0.0, 1.0) * 255.0;
        let byte = encoded.round();
        if byte <= 0.0 {
            0
        } else if byte >= 255.0 {
            255
        } else {
            byte as u8
        }
    }

    fn decode(byte: u8) -> f32 {
        srgb_to_linear(f32::from(byte) / 255.0)
    }

    fn is_high(time_s: f64, hz: f64) -> bool {
        let halves = (time_s * hz * 2.0).floor();
        (halves * 0.5).fract() == 0.0
    }

    struct Wave {
        width: u32,
        height: u32,
        fps: f64,
        hz: f64,
        seconds: f64,
        hi: [u8; 4],
        lo: [u8; 4],
        cell_px: u32,
        rect: Option<(u32, u32, u32, u32)>,
    }

    fn run_wave(wave: &Wave, keep: impl Fn(f64) -> bool) -> FlashReport {
        let mut analyzer = FlashAnalyzer::new(
            wave.width,
            wave.height,
            FlashConfig {
                geometry: ViewingGeometry::default(),
                cell_px: wave.cell_px,
            },
        )
        .expect("analyzer");
        let mut buf = vec![0u8; wave.width as usize * wave.height as usize * 4];
        let frames = (wave.fps * wave.seconds).round() as u32;
        let mut pushed = 0u32;
        for i in 0..frames {
            let t = f64::from(i) / wave.fps;
            if !keep(t) {
                continue;
            }
            let color = if is_high(t, wave.hz) { wave.hi } else { wave.lo };
            if let Some((x, y, w, h)) = wave.rect {
                paint(&mut buf, wave.lo);
                paint_rect(&mut buf, wave.width, x, y, w, h, color);
            } else {
                paint(&mut buf, color);
            }
            analyzer.push_frame(&buf, t).expect("frame");
            pushed += 1;
        }
        assert!(pushed > 0, "the wave kept no frames");
        analyzer.finish().expect("report")
    }

    fn full_frame(fps: f64, hz: f64, seconds: f64, hi: [u8; 4], lo: [u8; 4]) -> FlashReport {
        run_wave(
            &Wave {
                width: 16,
                height: 12,
                fps,
                hz,
                seconds,
                hi,
                lo,
                cell_px: 8,
                rect: None,
            },
            |_| true,
        )
    }

    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    #[test]
    fn ten_degree_field_matches_the_w3c_reference_screen() {
        let geometry = ViewingGeometry::default();
        let reference = ten_degree_field_px(1024, 768, &geometry).expect("reference");
        assert_eq!(reference, (341, 256));
        let doubled = ten_degree_field_px(2048, 1536, &geometry).expect("doubled");
        assert_eq!(doubled, (682, 512));
        let area = ten_degree_field_px(64, 48, &geometry).expect("area fixture");
        assert_eq!(area, (21, 16));
    }

    #[test]
    fn four_complete_flashes_fail_without_a_following_transition() {
        let mut analyzer = FlashAnalyzer::new(1, 1, FlashConfig::default()).expect("analyzer");
        for tick in 0..=8u32 {
            let color = if tick.is_multiple_of(2) { BLACK } else { WHITE };
            analyzer
                .push_frame(&color, f64::from(tick) / 10.0)
                .expect("frame");
        }
        let report = analyzer.finish().expect("report");
        assert_eq!(report.general_flashes, 4);
        assert!(!report.passed);
    }

    #[test]
    fn full_screen_black_white_at_2_hz_passes_and_at_4_hz_fails() {
        let slow = full_frame(60.0, 2.0, 2.0, WHITE, BLACK);
        let fast = full_frame(60.0, 4.0, 2.0, WHITE, BLACK);
        assert!(
            slow.passed,
            "2 Hz max {} area {}",
            slow.max_flashes, slow.area_fraction
        );
        assert!(slow.max_flashes <= MAX_FLASHES_PER_SECOND);
        assert_eq!(slow.kind, FlashKind::General);
        assert!(
            !fast.passed,
            "4 Hz max {} area {}",
            fast.max_flashes, fast.area_fraction
        );
        assert!(fast.max_flashes > MAX_FLASHES_PER_SECOND);
        assert!(fast.area_fraction > AREA_LIMIT);
        assert_eq!(fast.kind, FlashKind::General);
    }

    #[test]
    fn a_region_below_the_area_threshold_passes_and_above_it_fails() {
        let (field_w, field_h) = ten_degree_field_px(64, 48, &ViewingGeometry::default()).expect("field");
        assert_eq!((field_w, field_h), (21, 16));
        let window = f64::from(field_w) * f64::from(field_h);
        let small = run_wave(
            &Wave {
                width: 64,
                height: 48,
                fps: 60.0,
                hz: 10.0,
                seconds: 1.5,
                hi: WHITE,
                lo: BLACK,
                cell_px: 1,
                rect: Some((0, 0, 8, 8)),
            },
            |_| true,
        );
        let large = run_wave(
            &Wave {
                width: 64,
                height: 48,
                fps: 60.0,
                hz: 10.0,
                seconds: 1.5,
                hi: WHITE,
                lo: BLACK,
                cell_px: 1,
                rect: Some((0, 0, 12, 12)),
            },
            |_| true,
        );
        assert!(
            small.max_flashes > MAX_FLASHES_PER_SECOND,
            "small max {}",
            small.max_flashes
        );
        assert!(small.passed, "small area {}", small.area_fraction);
        assert!(
            (small.area_fraction - (64.0 / window)).abs() < 1e-9,
            "{}",
            small.area_fraction
        );
        assert!(small.area_fraction <= AREA_LIMIT);
        assert!(
            !large.passed,
            "large area {} max {}",
            large.area_fraction, large.max_flashes
        );
        assert!(
            (large.area_fraction - (144.0 / window)).abs() < 1e-9,
            "{}",
            large.area_fraction
        );
        assert!(large.area_fraction > AREA_LIMIT);
        assert_eq!(
            large.region,
            FlashRect {
                x: 0,
                y: 0,
                width: field_w,
                height: field_h
            }
        );
    }

    #[test]
    fn an_area_of_exactly_one_quarter_of_the_field_passes() {
        let exact = run_wave(
            &Wave {
                width: 64,
                height: 48,
                fps: 60.0,
                hz: 10.0,
                seconds: 1.5,
                hi: WHITE,
                lo: BLACK,
                cell_px: 1,
                rect: Some((0, 0, 12, 7)),
            },
            |_| true,
        );
        let over = run_wave(
            &Wave {
                width: 64,
                height: 48,
                fps: 60.0,
                hz: 10.0,
                seconds: 1.5,
                hi: WHITE,
                lo: BLACK,
                cell_px: 1,
                rect: Some((0, 0, 17, 5)),
            },
            |_| true,
        );
        assert!(exact.max_flashes > MAX_FLASHES_PER_SECOND);
        assert!(
            (exact.area_fraction - AREA_LIMIT).abs() < 1e-9,
            "{}",
            exact.area_fraction
        );
        assert!(exact.passed);
        assert!(over.area_fraction > AREA_LIMIT, "{}", over.area_fraction);
        assert!(!over.passed);
    }

    #[test]
    fn dark_flicker_below_the_luminance_change_threshold_passes() {
        let gray = quantize_linear(0.09);
        assert!(decode(gray) < LUMINANCE_DELTA);
        let report = full_frame(60.0, 10.0, 2.0, [gray, gray, gray, 255], BLACK);
        assert!(report.passed);
        assert_eq!(report.general_flashes, 0);
        assert_eq!(report.red_flashes, 0);
    }

    #[test]
    fn flicker_between_two_bright_levels_passes_the_darker_image_rule() {
        let hi_l = decode(255);
        let mut lo = 255u8;
        for byte in (0..255).rev() {
            let luma = decode(byte);
            if luma >= DARKER_LIMIT && hi_l - luma >= LUMINANCE_DELTA {
                lo = byte;
                break;
            }
        }
        assert!(lo < 255, "no pair of bright sRGB grays differs by 0.10");
        let lo_l = decode(lo);
        assert!(lo_l >= DARKER_LIMIT, "decoded darker image {lo_l} from byte {lo}");
        assert!(hi_l - lo_l >= LUMINANCE_DELTA);
        let report = full_frame(60.0, 4.0, 2.0, WHITE, [lo, lo, lo, 255]);
        assert_eq!(report.general_flashes, 0, "bright pair was counted");
        assert_eq!(report.red_flashes, 0);
        assert!(report.passed);
    }

    #[test]
    fn the_darker_image_rule_is_strictly_below_0_80() {
        let mut at_limit = 0u8;
        for byte in 0..=255 {
            if decode(byte) >= DARKER_LIMIT {
                at_limit = byte;
                break;
            }
        }
        assert!(at_limit > 0);
        let below = at_limit - 1;
        assert!(decode(at_limit) >= DARKER_LIMIT);
        assert!(decode(below) < DARKER_LIMIT);
        assert!(decode(255) - decode(below) >= LUMINANCE_DELTA);
        let at = full_frame(60.0, 4.0, 2.0, WHITE, [at_limit, at_limit, at_limit, 255]);
        let under = full_frame(60.0, 4.0, 2.0, WHITE, [below, below, below, 255]);
        assert!(at.passed, "level at 0.80 was counted, max {}", at.general_flashes);
        assert_eq!(at.general_flashes, 0);
        assert!(
            !under.passed,
            "level below 0.80 was ignored, max {}",
            under.general_flashes
        );
        assert!(under.general_flashes > MAX_FLASHES_PER_SECOND);
    }

    #[test]
    fn a_saturated_red_flash_fails_while_the_same_luminance_change_in_gray_passes() {
        let red_byte = quantize_linear(0.15);
        let red_linear = decode(red_byte);
        let red_luma = 0.2126 * red_linear;
        let red_uv = chromaticity(red_linear, 0.0, 0.0);
        assert!(dot(red_uv, red_uv) > RED_DELTA * RED_DELTA);
        assert!(
            red_luma < LUMINANCE_DELTA,
            "red luminance {red_luma} would also be a general flash"
        );
        let mut gray = 0u8;
        let mut best_err = f32::MAX;
        for byte in 0..=255 {
            let err = (decode(byte) - red_luma).abs();
            if err < best_err {
                best_err = err;
                gray = byte;
            }
        }
        assert!(best_err < 0.005, "gray luma missed the red luma by {best_err}");
        let red = full_frame(60.0, 5.0, 2.0, [red_byte, 0, 0, 255], BLACK);
        let neutral = full_frame(60.0, 5.0, 2.0, [gray, gray, gray, 255], BLACK);
        assert!(
            !red.passed,
            "red max {} area {}",
            red.red_flashes, red.area_fraction
        );
        assert!(red.red_flashes > MAX_FLASHES_PER_SECOND);
        assert_eq!(red.general_flashes, 0);
        assert_eq!(red.kind, FlashKind::Red);
        assert!(
            neutral.passed,
            "gray general {} red {}",
            neutral.general_flashes, neutral.red_flashes
        );
        assert_eq!(neutral.general_flashes, 0);
        assert_eq!(neutral.red_flashes, 0);
    }

    #[test]
    fn dim_red_green_flashes_use_chromaticity_not_the_legacy_red_metric() {
        let red = quantize_linear(0.01);
        let green = quantize_linear(0.003);
        let report = full_frame(60.0, 5.0, 2.0, [red, 0, 0, 255], [0, green, 0, 255]);
        assert_eq!(report.general_flashes, 0);
        assert!(report.red_flashes > MAX_FLASHES_PER_SECOND);
        assert!(!report.passed);
    }

    #[test]
    fn red_intensity_changes_with_constant_chromaticity_are_not_red_flashes() {
        let lo = quantize_linear(0.1);
        let hi = quantize_linear(0.4);
        let report = full_frame(60.0, 5.0, 2.0, [hi, 0, 0, 255], [lo, 0, 0, 255]);
        assert_eq!(report.general_flashes, 0);
        assert_eq!(report.red_flashes, 0);
        assert!(report.passed);
    }

    #[test]
    fn a_smooth_fade_over_one_second_passes() {
        let mut analyzer = FlashAnalyzer::new(8, 8, FlashConfig::default()).expect("analyzer");
        let mut buf = vec![0u8; 8 * 8 * 4];
        for i in 0u32..=30 {
            let byte = u8::try_from(i * 255 / 30).expect("byte");
            paint(&mut buf, [byte, byte, byte, 255]);
            analyzer.push_frame(&buf, f64::from(i) / 30.0).expect("frame");
        }
        let report = analyzer.finish().expect("report");
        assert_eq!(report.frames, 31);
        assert_eq!(report.general_flashes, 0);
        assert_eq!(report.red_flashes, 0);
        assert!(report.passed);
    }

    #[test]
    fn the_same_square_wave_at_30_and_60_fps_has_the_same_verdict() {
        for hz in [2.0, 4.0] {
            let at_30 = full_frame(30.0, hz, 2.0, WHITE, BLACK);
            let at_60 = full_frame(60.0, hz, 2.0, WHITE, BLACK);
            assert_eq!(
                at_30.passed, at_60.passed,
                "{hz} Hz 30 fps passed={} max {} ; 60 fps passed={} max {}",
                at_30.passed, at_30.max_flashes, at_60.passed, at_60.max_flashes
            );
            if hz <= 3.0 {
                assert!(at_30.passed && at_60.passed, "{hz} Hz should pass");
            } else {
                assert!(!at_30.passed && !at_60.passed, "{hz} Hz should fail");
            }
        }
    }

    #[test]
    fn a_sliding_window_catches_a_burst_that_straddles_two_seconds() {
        let wave = Wave {
            width: 16,
            height: 12,
            fps: 40.0,
            hz: 4.0,
            seconds: 2.0,
            hi: WHITE,
            lo: BLACK,
            cell_px: 1,
            rect: None,
        };
        let full = run_wave(&wave, |_| true);
        let opening = run_wave(&wave, |t| t < 1.0);
        let closing = run_wave(&wave, |t| t >= 1.0);
        assert!(
            !full.passed,
            "straddle max {} start {}",
            full.max_flashes, full.window_start_s
        );
        assert!(full.max_flashes > MAX_FLASHES_PER_SECOND);
        assert!(
            opening.passed,
            "opening second max {} should pass on its own",
            opening.max_flashes
        );
        assert!(opening.max_flashes <= MAX_FLASHES_PER_SECOND);
        assert!(
            closing.passed,
            "closing second max {} should pass on its own",
            closing.max_flashes
        );
        assert!(closing.max_flashes <= MAX_FLASHES_PER_SECOND);
    }

    #[test]
    fn a_long_sequence_is_streamed_from_one_buffer() {
        let mut analyzer = FlashAnalyzer::new(
            8,
            8,
            FlashConfig {
                cell_px: 1,
                ..FlashConfig::default()
            },
        )
        .expect("analyzer");
        let mut buf = vec![0u8; 8 * 8 * 4];
        for i in 0..3_000u32 {
            let t = f64::from(i) / 30.0;
            let color = if is_high(t, 2.0) { WHITE } else { BLACK };
            paint(&mut buf, color);
            analyzer.push_frame(&buf, t).expect("frame");
        }
        let report = analyzer.finish().expect("report");
        assert_eq!(report.frames, 3_000);
        assert!(report.passed, "max {}", report.max_flashes);
        assert!(report.max_flashes <= MAX_FLASHES_PER_SECOND);
    }

    #[test]
    fn second_frame_of_a_different_size_is_rejected() {
        let mut analyzer = FlashAnalyzer::new(2, 2, FlashConfig::default()).expect("analyzer");
        let frame = vec![0u8; 16];
        analyzer.push_frame(&frame, 0.0).expect("first");
        let err = analyzer.push_frame(&[0u8; 8], 0.1).expect_err("size");
        assert_eq!(
            err,
            FlashError::FrameSizeMismatch {
                expected_bytes: 16,
                actual_bytes: 8
            }
        );
        assert_eq!(err.code(), "frame_size_mismatch");
        analyzer
            .push_frame(&frame, 0.2)
            .expect("unchanged after the rejection");
    }

    #[test]
    fn frame_times_must_increase_and_be_finite() {
        let mut analyzer = FlashAnalyzer::new(2, 2, FlashConfig::default()).expect("analyzer");
        let frame = vec![0u8; 16];
        let nan = analyzer.push_frame(&frame, f64::NAN).expect_err("nan");
        assert_eq!(nan.code(), "time_not_monotonic");
        analyzer.push_frame(&frame, 0.0).expect("zero");
        let equal = analyzer.push_frame(&frame, 0.0).expect_err("equal");
        assert_eq!(equal.code(), "time_not_monotonic");
        let back = analyzer.push_frame(&frame, -0.1).expect_err("backwards");
        assert_eq!(back.code(), "time_not_monotonic");
        analyzer.push_frame(&frame, 0.05).expect("advanced");
    }

    #[test]
    fn empty_input_geometry_and_oversized_frames_are_refused() {
        let empty = FlashAnalyzer::new(0, 4, FlashConfig::default()).expect_err("empty");
        assert_eq!(empty.code(), "empty_frame");
        let zero_cell = FlashAnalyzer::new(
            4,
            4,
            FlashConfig {
                cell_px: 0,
                ..FlashConfig::default()
            },
        )
        .expect_err("cell");
        assert_eq!(zero_cell.code(), "geometry_out_of_range");
        let geometry = ViewingGeometry {
            reference_width: 0,
            ..ViewingGeometry::default()
        };
        let zero_geometry = ten_degree_field_px(8, 8, &geometry).expect_err("geometry");
        assert_eq!(zero_geometry.code(), "geometry_out_of_range");
        let huge = FlashAnalyzer::new(8193, 1, FlashConfig::default()).expect_err("side");
        assert_eq!(huge.code(), "frame_too_large");
        let analyzer = FlashAnalyzer::new(2, 2, FlashConfig::default()).expect("analyzer");
        let err = analyzer.finish().expect_err("no frames");
        assert_eq!(err.code(), "no_frames");
    }

    #[test]
    fn a_single_frame_passes_and_the_report_serializes() {
        let mut analyzer = FlashAnalyzer::new(4, 4, FlashConfig::default()).expect("analyzer");
        analyzer.push_frame(&[0u8; 64], 0.0).expect("frame");
        let report = analyzer.finish().expect("report");
        assert!(report.passed);
        assert_eq!(report.max_flashes, 0);
        assert_eq!(report.kind, FlashKind::General);
        assert_eq!(report.frames, 1);
        let json = serde_json::to_value(&report).expect("json");
        assert_eq!(json["passed"], true);
        assert_eq!(json["kind"], "general");
        assert_eq!(json["max_flashes"], 0);
        assert!(json["region"]["width"].as_u64().is_some_and(|w| w >= 1));
    }

    /// A corner that is already over three flashes must not have its 10-degree
    /// field walked again on frames that do not change any cell's count.
    #[test]
    fn steady_frames_do_not_rescan_flash_windows() {
        let geometry = ViewingGeometry {
            reference_width: 80,
            reference_height: 80,
            ten_degree_width_px: 16,
            ten_degree_height_px: 16,
        };
        let mut analyzer =
            FlashAnalyzer::new(80, 80, FlashConfig { geometry, cell_px: 8 }).expect("analyzer");
        // 10×10 cells, 2×2 cell field: 9×9 window positions per search.
        let positions = 9u64 * 9;
        let mut buf = vec![0u8; 80 * 80 * 4];
        WINDOW_EVALS.with(|evals| evals.set(0));
        for tick in 0..=8u32 {
            let color = if tick.is_multiple_of(2) { WHITE } else { BLACK };
            paint(&mut buf, BLACK);
            paint_rect(&mut buf, 80, 0, 0, 8, 8, color);
            analyzer.push_frame(&buf, f64::from(tick) / 10.0).expect("frame");
        }
        let prefix = WINDOW_EVALS.with(|evals| evals.get());
        WINDOW_EVALS.with(|evals| evals.set(0));
        // Same white corner, still inside the one-second window, so no timestamp
        // expires and no new transition is armed.
        for step in 0..10u32 {
            analyzer
                .push_frame(&buf, 0.81 + f64::from(step) * 0.01)
                .expect("hold");
        }
        let hold = WINDOW_EVALS.with(|evals| evals.get());
        let report = analyzer.finish().expect("report");
        assert_eq!(report.general_flashes, 4, "{report:?}");
        assert!(report.passed, "{report:?}");
        assert!(
            (report.general_area_fraction - 0.25).abs() < 1e-9,
            "{}",
            report.general_area_fraction
        );
        assert_eq!(
            report.region,
            FlashRect {
                x: 0,
                y: 0,
                width: 16,
                height: 16
            }
        );
        assert_eq!(
            hold, 0,
            "held frames rescanned {hold} windows (prefix {prefix}, field {positions})"
        );
        assert!(
            prefix < positions,
            "the corner flash walked {prefix} windows; the field has {positions}"
        );
    }
}
