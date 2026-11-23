// SPDX-License-Identifier: MPL-2.0
//! Spatio-temporal denoising and accumulation of a noisy per-pixel GI signal (ADR-864).
//!
//! The input is one frame of indirect diffuse irradiance with a few rays per pixel or fewer
//! (the screen-space gather, a stochastic probe lookup, a future per-pixel ray tracer) plus the
//! frame's guides: the shading normal, the linear view depth and the motion of each pixel since
//! the previous frame. The output is a stable, low-noise irradiance image. Three passes run per
//! frame, the same on the CPU ([`GiDenoiser`], the reference) and on the GPU
//! ([`GiDenoiseGpu`], `shaders/gi_denoise.wgsl`):
//!
//! 1. **Temporal accumulation with history validation.** Each pixel reprojects into the
//!    previous frame with its motion vector and reads the history bilinearly. A tap is used only
//!    when its depth is within [`GiDenoiseSettings::depth_tolerance`] (relative) of the pixel's,
//!    its normal within [`GiDenoiseSettings::normal_tolerance`] (cosine), it lies on screen and it
//!    holds history. Disoccluded pixels, pixels revealed at the screen edge, pixels whose surface
//!    changed and every pixel after [`GiDenoiser::reset`] (a camera cut) start a new history.
//!    The new sample blends in with weight `max(1 / n, alpha)` for a history of `n` frames, so a
//!    young history converges as a running mean and a settled one as an exponential average. The
//!    first two luminance moments accumulate beside the colour. **Anti-lag:** per
//!    [`GiDenoiseSettings::antilag_block`]-pixel square, when this frame's mean input and the
//!    reprojected history's mean differ by more than [`GiDenoiseSettings::antilag_sigma`]
//!    standard errors, the square takes the new frame in with at least the relative size of the
//!    change (the history-shortening idea of Schied et al. 2018, from block statistics).
//! 2. **Variance estimate.** The temporal variance `E[l²] − E[l]²` of a settled pixel guides
//!    the spatial filter. A pixel with fewer than
//!    [`GiDenoiseSettings::spatial_variance_frames`] frames of history estimates its moments (and
//!    its colour) over a depth- and normal-weighted window instead, and boosts that variance by
//!    `frames / n`.
//! 3. **Edge-avoiding à-trous wavelet filter.** [`GiDenoiseSettings::atrous_iterations`] passes
//!    of a 5×5 B3-spline kernel with step 1, 2, 4, …, each tap weighted by depth (against the
//!    local depth gradient), normal and luminance. The luminance weight is scaled by the
//!    standard deviation of the pixel, blurred 3×3 over its own surface: where the estimate is
//!    noisy the filter blurs wide, where it has converged it keeps edges. Variance is filtered with the squared
//!    weights so the next pass sees the reduced noise.
//!
//! [`metrics`] scores a sequence (noise against the ground truth, frame-to-frame flicker,
//! bias, edge leak, shadow-edge error, disocclusion error, lag after a lighting change) and
//! [`scenario`] generates the deterministic synthetic sequences the regression tests and the
//! agent method `seeing.gi_denoise_check` (`book/agents/gi-denoise.md`) run through it.
//!
//! ## Provenance
//!
//! Schied, Kaplanyan, Wyman, Patney, Chaitanya, Burgess, Liu, Dachsbacher, Lefohn, Salvi,
//! "Spatiotemporal Variance-Guided Filtering: Real-Time Reconstruction for Path-Traced Global
//! Illumination", HPG 2017: temporal accumulation of colour and luminance moments, reprojection
//! validated by depth and normal, the spatial variance fallback for young histories, the
//! variance-guided luminance edge-stopping weight. Dammertz, Sewtz, Hanika, Lensch,
//! "Edge-Avoiding À-Trous Wavelet Transform for fast Global Illumination Filtering", HPG 2010:
//! the à-trous wavelet with the B3-spline kernel and edge-stopping functions. Schied, Peters,
//! Dachsbacher, "Gradient Estimation for Real-Time Adaptive Temporal Filtering", HPG 2018: a
//! detected change in the shading shortens the temporal history (here detected from per-square
//! means and their standard error instead of re-shaded gradient samples). Written from the
//! papers; the parameter values are the papers' defaults except where ADR-864 records a
//! measured change.

pub(crate) mod gpu;
pub mod metrics;
pub mod scenario;

pub use gpu::GiDenoiseGpu;

/// Most à-trous passes [`GiDenoiseSettings::atrous_iterations`] accepts (step 32 px).
pub const MAX_ATROUS_ITERATIONS: u32 = 6;
/// Largest window radius [`GiDenoiseSettings::variance_radius`] accepts (9×9).
pub const MAX_VARIANCE_RADIUS: u32 = 4;
/// Largest square [`GiDenoiseSettings::antilag_block`] accepts.
pub const MAX_ANTILAG_BLOCK: u32 = 32;
/// Largest input value kept; anything above (and every NaN, infinity or negative) is
/// sanitized before it can enter the history.
pub const MAX_INPUT: f32 = 65_504.0;

/// The 5×5 B3-spline à-trous kernel, by tap distance from the centre (1/16, 1/4, 3/8).
const B3: [f32; 3] = [3.0 / 8.0, 1.0 / 4.0, 1.0 / 16.0];
/// Fewest summed bilinear weight of valid history taps that still counts as history.
const MIN_HISTORY_WEIGHT: f32 = 0.01;
/// Keeps the luminance weight finite where the variance is exactly zero.
const LUMINANCE_EPSILON: f32 = 1e-10;
/// Fewest pixels with history an anti-lag square needs before its means are compared.
const MIN_ANTILAG_SAMPLES: u32 = 8;
/// A neighbour whose depth differs from the pixel's by more than this fraction is across a
/// depth edge and does not count toward the pixel's depth gradient.
const GRADIENT_EDGE: f32 = 0.25;

/// How the filter accumulates and smooths. The defaults are Schied et al.'s, except
/// [`Self::history_from_first_pass`] (ADR-864).
#[derive(Clone, Debug, PartialEq)]
pub struct GiDenoiseSettings {
    /// Smallest weight of the new sample in the colour history (0.2: about five frames of
    /// memory once settled). Lower is smoother and slower to follow a lighting change.
    pub alpha_color: f32,
    /// Smallest weight of the new sample in the luminance moments.
    pub alpha_moments: f32,
    /// History length cap, frames (the running mean turns into the exponential average at
    /// `1 / alpha_color` anyway; the cap only bounds the stored count).
    pub max_history: f32,
    /// A history tap is rejected when its depth differs from the pixel's by more than this
    /// fraction of the pixel's depth.
    pub depth_tolerance: f32,
    /// A history tap is rejected when the cosine between its normal and the pixel's is below
    /// this.
    pub normal_tolerance: f32,
    /// À-trous passes (0 = temporal accumulation only), at most [`MAX_ATROUS_ITERATIONS`].
    pub atrous_iterations: u32,
    /// Depth edge-stopping: a tap's depth may differ by this many times what the local depth
    /// gradient predicts over its offset.
    pub sigma_depth: f32,
    /// Depth difference always tolerated, as a fraction of the pixel's depth (keeps flat
    /// surfaces seen head-on, whose gradient is zero, from rejecting their own noise).
    pub depth_epsilon: f32,
    /// Normal edge-stopping exponent on `max(0, n_p · n_q)`.
    pub sigma_normal: f32,
    /// Luminance edge-stopping: a tap may differ by this many standard deviations.
    pub sigma_luminance: f32,
    /// Radius of the spatial variance window for young histories (3 = 7×7), at most
    /// [`MAX_VARIANCE_RADIUS`].
    pub variance_radius: u32,
    /// Histories shorter than this many frames use the spatial variance estimate.
    pub spatial_variance_frames: f32,
    /// Feed the first à-trous pass's output back as the colour history (Schied et al.) instead
    /// of the unfiltered temporal average. Off by default: on the regression scenarios the
    /// recursive blur raised the converged error by about a fifth and blurred shadow edges
    /// (ADR-864).
    pub history_from_first_pass: bool,
    /// Anti-lag: per [`Self::antilag_block`]-pixel square, the mean luminance of this frame's
    /// input is compared with the reprojected history's over the pixels that have history.
    /// When they differ by more than this many standard errors of the input's mean (a change in
    /// the lighting, not noise), every pixel of the square blends the new frame in with at least
    /// the relative size of the change and its history length is cut to match. 0 turns it off.
    /// With 5 a light going from 25 % to 100 % settles in 2 frames instead of 9 (ADR-864).
    pub antilag_sigma: f32,
    /// Side of the anti-lag squares, pixels (2..=[`MAX_ANTILAG_BLOCK`]).
    pub antilag_block: u32,
}

impl Default for GiDenoiseSettings {
    fn default() -> Self {
        Self {
            alpha_color: 0.2,
            alpha_moments: 0.2,
            max_history: 32.0,
            depth_tolerance: 0.1,
            normal_tolerance: 0.9,
            atrous_iterations: 5,
            sigma_depth: 1.0,
            depth_epsilon: 0.01,
            sigma_normal: 128.0,
            sigma_luminance: 4.0,
            variance_radius: 3,
            spatial_variance_frames: 4.0,
            history_from_first_pass: false,
            antilag_sigma: 5.0,
            antilag_block: 8,
        }
    }
}

impl GiDenoiseSettings {
    /// Checks every field's range.
    ///
    /// # Errors
    ///
    /// [`GiDenoiseError::InvalidSetting`] naming the first field out of range.
    pub fn validate(&self) -> Result<(), GiDenoiseError> {
        let unit = |name: &'static str, v: f32| {
            if v.is_finite() && v > 0.0 && v <= 1.0 {
                Ok(())
            } else {
                Err(GiDenoiseError::invalid(name, format!("{v} is not in (0, 1]")))
            }
        };
        let positive = |name: &'static str, v: f32| {
            if v.is_finite() && v > 0.0 {
                Ok(())
            } else {
                Err(GiDenoiseError::invalid(
                    name,
                    format!("{v} is not a finite value above 0"),
                ))
            }
        };
        unit("alpha_color", self.alpha_color)?;
        unit("alpha_moments", self.alpha_moments)?;
        if !(self.max_history.is_finite() && self.max_history >= 1.0) {
            return Err(GiDenoiseError::invalid(
                "max_history",
                format!("{} is below 1 frame", self.max_history),
            ));
        }
        positive("depth_tolerance", self.depth_tolerance)?;
        if !(self.normal_tolerance.is_finite() && (-1.0..=1.0).contains(&self.normal_tolerance)) {
            return Err(GiDenoiseError::invalid(
                "normal_tolerance",
                format!("{} is not a cosine in [-1, 1]", self.normal_tolerance),
            ));
        }
        if self.atrous_iterations > MAX_ATROUS_ITERATIONS {
            return Err(GiDenoiseError::invalid(
                "atrous_iterations",
                format!("{} is above {MAX_ATROUS_ITERATIONS}", self.atrous_iterations),
            ));
        }
        positive("sigma_depth", self.sigma_depth)?;
        positive("depth_epsilon", self.depth_epsilon)?;
        positive("sigma_normal", self.sigma_normal)?;
        positive("sigma_luminance", self.sigma_luminance)?;
        if self.variance_radius > MAX_VARIANCE_RADIUS {
            return Err(GiDenoiseError::invalid(
                "variance_radius",
                format!("{} is above {MAX_VARIANCE_RADIUS}", self.variance_radius),
            ));
        }
        if !(self.spatial_variance_frames.is_finite() && self.spatial_variance_frames >= 0.0) {
            return Err(GiDenoiseError::invalid(
                "spatial_variance_frames",
                format!("{} is not a finite count of frames", self.spatial_variance_frames),
            ));
        }
        if !(self.antilag_sigma.is_finite() && self.antilag_sigma >= 0.0) {
            return Err(GiDenoiseError::invalid(
                "antilag_sigma",
                format!("{} is not a finite value of at least 0", self.antilag_sigma),
            ));
        }
        if !(2..=MAX_ANTILAG_BLOCK).contains(&self.antilag_block) {
            return Err(GiDenoiseError::invalid(
                "antilag_block",
                format!("{} is not in 2..={MAX_ANTILAG_BLOCK}", self.antilag_block),
            ));
        }
        Ok(())
    }
}

/// What went wrong in a denoiser call. [`Self::code`] is the agent protocol's error code.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GiDenoiseError {
    /// A setting is out of range.
    #[error("GI denoise setting `{name}`: {reason}")]
    InvalidSetting {
        /// The field.
        name: &'static str,
        /// Why it was rejected.
        reason: String,
    },
    /// An input buffer's length is not `width × height`.
    #[error("GI denoise input `{buffer}` has {got} pixels, the frame is {width}×{height}")]
    SizeMismatch {
        /// The buffer (`noisy`, `guide` or `motion`).
        buffer: &'static str,
        /// Its length.
        got: usize,
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
    },
    /// A zero-sized frame.
    #[error("GI denoise frame is empty ({width}×{height})")]
    EmptyFrame {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
    },
    /// The GPU path could not run (no adapter, a failed readback).
    #[error("GI denoise GPU: {0}")]
    Gpu(String),
}

impl GiDenoiseError {
    fn invalid(name: &'static str, reason: String) -> Self {
        Self::InvalidSetting { name, reason }
    }

    /// The agent protocol's error code for this error (`book/agents/gi-denoise.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSetting { .. } => "gi_denoise_invalid_setting",
            Self::SizeMismatch { .. } => "gi_denoise_size_mismatch",
            Self::EmptyFrame { .. } => "gi_denoise_empty_frame",
            Self::Gpu(_) => "gi_denoise_gpu",
        }
    }
}

/// One frame of input. Every buffer is row-major, `width × height` long.
#[derive(Clone, Copy, Debug)]
pub struct GiDenoiseInput<'a> {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// The noisy irradiance, linear RGB. NaN, infinite and negative values count as 0; values
    /// above [`MAX_INPUT`] are clamped.
    pub noisy: &'a [[f32; 3]],
    /// `[nx, ny, nz, depth]`: the unit shading normal (any space, the same every frame) and the
    /// linear view depth. A depth that is not finite and above 0 marks a pixel with no surface
    /// (sky): its output is 0 and it holds no history.
    pub guide: &'a [[f32; 4]],
    /// Where the pixel was in the previous frame, as an offset in pixels: the previous position
    /// of pixel `(x, y)`'s centre is `(x + 0.5 + mx, y + 0.5 + my)`.
    pub motion: &'a [[f32; 2]],
}

impl GiDenoiseInput<'_> {
    fn check(&self) -> Result<usize, GiDenoiseError> {
        if self.width == 0 || self.height == 0 {
            return Err(GiDenoiseError::EmptyFrame {
                width: self.width,
                height: self.height,
            });
        }
        let pixels = self.width as usize * self.height as usize;
        for (buffer, got) in [
            ("noisy", self.noisy.len()),
            ("guide", self.guide.len()),
            ("motion", self.motion.len()),
        ] {
            if got != pixels {
                return Err(GiDenoiseError::SizeMismatch {
                    buffer,
                    got,
                    width: self.width,
                    height: self.height,
                });
            }
        }
        Ok(pixels)
    }
}

/// One frame of output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GiDenoiseOutput {
    /// The filtered irradiance, linear RGB.
    pub color: Vec<[f32; 3]>,
    /// Frames of history behind each pixel after this frame (1 = no usable history, 0 = no
    /// surface). Fractional where the history was read across a bilinear footprint.
    pub history_len: Vec<f32>,
    /// The remaining luminance variance estimate after the last pass.
    pub variance: Vec<f32>,
}

/// A denoiser the scenario runner can drive: [`GiDenoiser`] (the CPU reference) or
/// [`GiDenoiseGpu`].
pub trait GiDenoise {
    /// Filters one frame and keeps its history for the next.
    ///
    /// # Errors
    ///
    /// As [`GiDenoiser::denoise`] and [`GiDenoiseGpu::denoise`].
    fn denoise_frame(&mut self, input: &GiDenoiseInput<'_>) -> Result<GiDenoiseOutput, GiDenoiseError>;
}

impl GiDenoise for GiDenoiser {
    fn denoise_frame(&mut self, input: &GiDenoiseInput<'_>) -> Result<GiDenoiseOutput, GiDenoiseError> {
        self.denoise(input)
    }
}

impl GiDenoise for GiDenoiseGpu {
    fn denoise_frame(&mut self, input: &GiDenoiseInput<'_>) -> Result<GiDenoiseOutput, GiDenoiseError> {
        self.denoise(input)
    }
}

/// The state one frame leaves for the next.
#[derive(Clone, Debug)]
struct History {
    color: Vec<[f32; 3]>,
    moments: Vec<[f32; 2]>,
    len: Vec<f32>,
    guide: Vec<[f32; 4]>,
}

/// The CPU reference denoiser: holds the history of one view.
///
/// Deterministic: the same settings and inputs give the same output bit for bit.
#[derive(Clone, Debug)]
pub struct GiDenoiser {
    settings: GiDenoiseSettings,
    size: (u32, u32),
    history: Option<History>,
    frames: u64,
}

impl GiDenoiser {
    /// A denoiser with no history.
    ///
    /// # Errors
    ///
    /// [`GiDenoiseError::InvalidSetting`] when `settings` fails [`GiDenoiseSettings::validate`].
    pub fn new(settings: GiDenoiseSettings) -> Result<Self, GiDenoiseError> {
        settings.validate()?;
        Ok(Self {
            settings,
            size: (0, 0),
            history: None,
            frames: 0,
        })
    }

    /// The settings in use.
    #[must_use]
    pub fn settings(&self) -> &GiDenoiseSettings {
        &self.settings
    }

    /// Replaces the settings. The history is kept: the next frame blends into it with the new
    /// weights.
    ///
    /// # Errors
    ///
    /// [`GiDenoiseError::InvalidSetting`]; the old settings stay.
    pub fn set_settings(&mut self, settings: GiDenoiseSettings) -> Result<(), GiDenoiseError> {
        settings.validate()?;
        self.settings = settings;
        Ok(())
    }

    /// Drops the history (a camera cut, a teleport, a scene switch). The next frame is filtered
    /// spatially only.
    pub fn reset(&mut self) {
        self.history = None;
    }

    /// Frames denoised since construction.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Whether the next frame has history to reproject.
    #[must_use]
    pub fn has_history(&self) -> bool {
        self.history.is_some()
    }

    /// Filters one frame and keeps its history for the next. A frame of a different size than
    /// the last drops the history first.
    ///
    /// # Errors
    ///
    /// [`GiDenoiseError::EmptyFrame`] or [`GiDenoiseError::SizeMismatch`]; the history is left
    /// as it was.
    pub fn denoise(&mut self, input: &GiDenoiseInput<'_>) -> Result<GiDenoiseOutput, GiDenoiseError> {
        let _span = tracing::trace_span!("gi_denoise_cpu").entered();
        input.check()?;
        let size = (input.width, input.height);
        if size != self.size {
            self.history = None;
            self.size = size;
        }
        let frame = Frame::new(input);
        let temporal = temporal_pass(&self.settings, &frame, self.history.as_ref());
        let (mut color, mut variance) = variance_pass(&self.settings, &frame, &temporal);
        let mut first_pass = None;
        for iteration in 0..self.settings.atrous_iterations {
            let (c, v) = atrous_pass(&self.settings, &frame, &color, &variance, 1 << iteration);
            color = c;
            variance = v;
            if iteration == 0 && self.settings.history_from_first_pass {
                first_pass = Some(color.clone());
            }
        }
        let history_len = temporal.len.clone();
        self.history = Some(History {
            color: first_pass.unwrap_or(temporal.color),
            moments: temporal.moments,
            len: temporal.len,
            guide: input.guide.to_vec(),
        });
        self.frames += 1;
        Ok(GiDenoiseOutput {
            color,
            history_len,
            variance,
        })
    }
}

/// The checked input with the per-frame derived data.
struct Frame<'a> {
    w: usize,
    h: usize,
    noisy: Vec<[f32; 3]>,
    guide: &'a [[f32; 4]],
    motion: &'a [[f32; 2]],
    gradient: Vec<[f32; 2]>,
}

impl<'a> Frame<'a> {
    fn new(input: &GiDenoiseInput<'a>) -> Self {
        let w = input.width as usize;
        let h = input.height as usize;
        Self {
            w,
            h,
            noisy: input.noisy.iter().map(|c| c.map(sanitize)).collect(),
            guide: input.guide,
            motion: input.motion,
            gradient: depth_gradients(w, h, input.guide),
        }
    }

    /// The pixel at `(x + dx, y + dy)` when it is on screen.
    fn at(&self, x: usize, y: usize, dx: i64, dy: i64) -> Option<usize> {
        let qx = x as i64 + dx;
        let qy = y as i64 + dy;
        if qx < 0 || qy < 0 || qx >= self.w as i64 || qy >= self.h as i64 {
            None
        } else {
            Some(qy as usize * self.w + qx as usize)
        }
    }

    /// Depth and normal edge-stopping weight between pixel `i` and tap `j` at offset
    /// `(dx, dy)` pixels.
    fn geometry_weight(&self, s: &GiDenoiseSettings, i: usize, j: usize, dx: f32, dy: f32) -> f32 {
        let gp = self.guide[i];
        let gq = self.guide[j];
        let g = self.gradient[i];
        let expected = (g[0] * dx + g[1] * dy).abs();
        let wz = (-(gq[3] - gp[3]).abs() / (s.sigma_depth * expected + s.depth_epsilon * gp[3])).exp();
        let d = gp[0] * gq[0] + gp[1] * gq[1] + gp[2] * gq[2];
        let wn = if d > 0.0 { d.powf(s.sigma_normal) } else { 0.0 };
        wz * wn
    }
}

/// The temporal pass's output.
struct Temporal {
    color: Vec<[f32; 3]>,
    moments: Vec<[f32; 2]>,
    len: Vec<f32>,
}

/// Rec. 709 luminance of linear RGB.
#[must_use]
pub fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn sanitize(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 {
        v.min(MAX_INPUT)
    } else {
        0.0
    }
}

fn has_surface(depth: f32) -> bool {
    depth.is_finite() && depth > 0.0
}

/// Per-pixel screen-space depth gradient `[dz/dx, dz/dy]`: per axis the smaller in magnitude
/// of the forward and backward differences to a neighbour with a surface, so the gradient
/// next to a depth edge is the surface's own slope, not the edge. A difference above
/// `GRADIENT_EDGE` of the depth is an edge and is ignored even when it is the only one (a
/// pixel at the screen border beside a nearer object would otherwise take the edge as its
/// slope and let the filter cross it).
fn depth_gradients(w: usize, h: usize, guide: &[[f32; 4]]) -> Vec<[f32; 2]> {
    let mut out = vec![[0.0; 2]; w * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let z = guide[i][3];
            if !has_surface(z) {
                continue;
            }
            let side = |j: Option<usize>, sign: f32| {
                j.map(|j| guide[j][3])
                    .filter(|zq| has_surface(*zq) && (zq - z).abs() <= GRADIENT_EDGE * z)
                    .map(|zq| sign * (zq - z))
            };
            let pick = |a: Option<f32>, b: Option<f32>| match (a, b) {
                (Some(a), Some(b)) => {
                    if a.abs() <= b.abs() {
                        a
                    } else {
                        b
                    }
                }
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => 0.0,
            };
            let right = if x + 1 < w { Some(i + 1) } else { None };
            let left = if x > 0 { Some(i - 1) } else { None };
            let down = if y + 1 < h { Some(i + w) } else { None };
            let up = if y > 0 { Some(i - w) } else { None };
            out[i] = [
                pick(side(right, 1.0), side(left, -1.0)),
                pick(side(down, 1.0), side(up, -1.0)),
            ];
        }
    }
    out
}

/// One pixel's history after reprojection (normalised by the valid taps' weight).
#[derive(Clone, Copy, Default)]
struct Reprojected {
    color: [f32; 3],
    moments: [f32; 2],
    len: f32,
    valid: bool,
}

/// Reads pixel `(x, y)`'s history through its motion vector, tap by tap validated.
fn reproject(s: &GiDenoiseSettings, f: &Frame<'_>, hist: &History, x: usize, y: usize) -> Reprojected {
    let i = y * f.w + x;
    let g = f.guide[i];
    let m = f.motion[i];
    let qx = x as f32 + m[0];
    let qy = y as f32 + m[1];
    // Outside this range (or not finite) every tap is off screen.
    let near = qx > -2.0 && qy > -2.0 && qx < f.w as f32 + 1.0 && qy < f.h as f32 + 1.0;
    if !near {
        return Reprojected::default();
    }
    let x0 = qx.floor();
    let y0 = qy.floor();
    let fx = qx - x0;
    let fy = qy - y0;
    let taps = [
        (0, 0, (1.0 - fx) * (1.0 - fy)),
        (1, 0, fx * (1.0 - fy)),
        (0, 1, (1.0 - fx) * fy),
        (1, 1, fx * fy),
    ];
    let mut hc = [0.0f32; 3];
    let mut hm = [0.0f32; 2];
    let mut hl = 0.0f32;
    let mut ws = 0.0f32;
    for (ox, oy, wt) in taps {
        let tx = x0 as i64 + ox;
        let ty = y0 as i64 + oy;
        if wt <= 0.0 || tx < 0 || ty < 0 || tx >= f.w as i64 || ty >= f.h as i64 {
            continue;
        }
        let j = ty as usize * f.w + tx as usize;
        let pg = hist.guide[j];
        let normal_dot = pg[0] * g[0] + pg[1] * g[1] + pg[2] * g[2];
        if hist.len[j] <= 0.0
            || !has_surface(pg[3])
            || (pg[3] - g[3]).abs() > s.depth_tolerance * g[3]
            || normal_dot < s.normal_tolerance
        {
            continue;
        }
        for (a, b) in hc.iter_mut().zip(hist.color[j]) {
            *a += wt * b;
        }
        hm[0] += wt * hist.moments[j][0];
        hm[1] += wt * hist.moments[j][1];
        hl += wt * hist.len[j];
        ws += wt;
    }
    if ws < MIN_HISTORY_WEIGHT {
        return Reprojected::default();
    }
    let inv = 1.0 / ws;
    Reprojected {
        color: hc.map(|v| v * inv),
        moments: hm.map(|v| v * inv),
        len: hl * inv,
        valid: true,
    }
}

/// Anti-lag weight per square (row-major squares of `antilag_block` pixels): the relative
/// change between this frame's mean input luminance and the reprojected history's, where it
/// exceeds `antilag_sigma` standard errors of the input mean; 0 elsewhere.
fn antilag_weights(s: &GiDenoiseSettings, f: &Frame<'_>, reprojected: &[Reprojected]) -> Vec<f32> {
    let b = s.antilag_block as usize;
    let bw = f.w.div_ceil(b);
    let bh = f.h.div_ceil(b);
    let mut out = vec![0.0f32; bw * bh];
    if s.antilag_sigma <= 0.0 {
        return out;
    }
    for by in 0..bh {
        for bx in 0..bw {
            let mut count = 0u32;
            let mut sum_c = 0.0f32;
            let mut sum_cc = 0.0f32;
            let mut sum_h = 0.0f32;
            for y in by * b..((by + 1) * b).min(f.h) {
                for x in bx * b..((bx + 1) * b).min(f.w) {
                    let i = y * f.w + x;
                    let r = reprojected[i];
                    if !r.valid || !has_surface(f.guide[i][3]) {
                        continue;
                    }
                    let lc = luminance(f.noisy[i]);
                    sum_c += lc;
                    sum_cc += lc * lc;
                    sum_h += luminance(r.color);
                    count += 1;
                }
            }
            if count < MIN_ANTILAG_SAMPLES {
                continue;
            }
            let n = count as f32;
            let mc = sum_c / n;
            let mh = sum_h / n;
            let se = ((sum_cc / n - mc * mc).max(0.0) / n).sqrt();
            let d = (mc - mh).abs();
            let top = mc.max(mh);
            if d > s.antilag_sigma * se && top > 0.0 {
                out[by * bw + bx] = (d / top).min(1.0);
            }
        }
    }
    out
}

fn temporal_pass(s: &GiDenoiseSettings, f: &Frame<'_>, history: Option<&History>) -> Temporal {
    let n = f.w * f.h;
    let mut out = Temporal {
        color: vec![[0.0; 3]; n],
        moments: vec![[0.0; 2]; n],
        len: vec![0.0; n],
    };
    let mut reprojected = vec![Reprojected::default(); n];
    if let Some(hist) = history {
        for y in 0..f.h {
            for x in 0..f.w {
                let i = y * f.w + x;
                if has_surface(f.guide[i][3]) {
                    reprojected[i] = reproject(s, f, hist, x, y);
                }
            }
        }
    }
    let lambdas = antilag_weights(s, f, &reprojected);
    let b = s.antilag_block as usize;
    let bw = f.w.div_ceil(b);
    for y in 0..f.h {
        for x in 0..f.w {
            let i = y * f.w + x;
            if !has_surface(f.guide[i][3]) {
                continue;
            }
            let c = f.noisy[i];
            let l = luminance(c);
            let r = reprojected[i];
            if r.valid {
                let lambda = lambdas[(y / b) * bw + x / b];
                let mut len = (r.len + 1.0).min(s.max_history);
                if lambda > 0.0 {
                    len = len.min((1.0 / lambda).max(1.0));
                }
                let a = (1.0 / len).max(s.alpha_color).max(lambda);
                let am = (1.0 / len).max(s.alpha_moments).max(lambda);
                let hc = r.color;
                let hm = r.moments;
                out.color[i] = [
                    hc[0] + (c[0] - hc[0]) * a,
                    hc[1] + (c[1] - hc[1]) * a,
                    hc[2] + (c[2] - hc[2]) * a,
                ];
                out.moments[i] = [hm[0] + (l - hm[0]) * am, hm[1] + (l * l - hm[1]) * am];
                out.len[i] = len;
            } else {
                out.color[i] = c;
                out.moments[i] = [l, l * l];
                out.len[i] = 1.0;
            }
        }
    }
    out
}

/// Colour and variance after the variance estimate (young histories filtered spatially).
fn variance_pass(s: &GiDenoiseSettings, f: &Frame<'_>, t: &Temporal) -> (Vec<[f32; 3]>, Vec<f32>) {
    let n = f.w * f.h;
    let mut color = t.color.clone();
    let mut variance = vec![0.0f32; n];
    let r = i64::from(s.variance_radius);
    for y in 0..f.h {
        for x in 0..f.w {
            let i = y * f.w + x;
            if !has_surface(f.guide[i][3]) {
                continue;
            }
            let [m1, m2] = t.moments[i];
            let len = t.len[i];
            if len >= s.spatial_variance_frames {
                variance[i] = (m2 - m1 * m1).max(0.0);
                continue;
            }
            let mut sc = [0.0f32; 3];
            let mut s1 = 0.0f32;
            let mut s2 = 0.0f32;
            let mut sw = 0.0f32;
            for dy in -r..=r {
                for dx in -r..=r {
                    let Some(j) = f.at(x, y, dx, dy) else { continue };
                    if !has_surface(f.guide[j][3]) {
                        continue;
                    }
                    let w = f.geometry_weight(s, i, j, dx as f32, dy as f32);
                    for (a, b) in sc.iter_mut().zip(t.color[j]) {
                        *a += w * b;
                    }
                    s1 += w * t.moments[j][0];
                    s2 += w * t.moments[j][1];
                    sw += w;
                }
            }
            // The centre tap weighs 1 for a unit normal; a degenerate normal keeps the temporal
            // estimate.
            if sw <= 0.0 {
                variance[i] = (m2 - m1 * m1).max(0.0);
                continue;
            }
            let inv = 1.0 / sw;
            let mean = s1 * inv;
            variance[i] = (s2 * inv - mean * mean).max(0.0) * (s.spatial_variance_frames / len.max(1.0));
            color[i] = sc.map(|v| v * inv);
        }
    }
    (color, variance)
}

/// One à-trous pass with tap spacing `step`.
fn atrous_pass(
    s: &GiDenoiseSettings,
    f: &Frame<'_>,
    color: &[[f32; 3]],
    variance: &[f32],
    step: i64,
) -> (Vec<[f32; 3]>, Vec<f32>) {
    const GAUSS: [f32; 3] = [0.25, 0.125, 0.0625];
    let n = f.w * f.h;
    let mut out_c = vec![[0.0f32; 3]; n];
    let mut out_v = vec![0.0f32; n];
    for y in 0..f.h {
        for x in 0..f.w {
            let i = y * f.w + x;
            if !has_surface(f.guide[i][3]) {
                continue;
            }
            // 3×3 Gaussian of the variance over the neighbours on the same surface (depth and
            // normal weights): a bright object's noise must not loosen the luminance weight of
            // the wall beside it (ADR-864).
            let mut gv = 0.0f32;
            let mut gw = 0.0f32;
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    let Some(j) = f.at(x, y, dx, dy) else { continue };
                    if !has_surface(f.guide[j][3]) {
                        continue;
                    }
                    let k = GAUSS[(dx.unsigned_abs() + dy.unsigned_abs()) as usize]
                        * f.geometry_weight(s, i, j, dx as f32, dy as f32);
                    gv += k * variance[j];
                    gw += k;
                }
            }
            let blurred = if gw > 0.0 { gv / gw } else { variance[i] };
            let lp = luminance(color[i]);
            let dl = s.sigma_luminance * blurred.max(0.0).sqrt() + LUMINANCE_EPSILON;
            let mut sc = [0.0f32; 3];
            let mut sv = 0.0f32;
            let mut sw = 0.0f32;
            for dy in -2i64..=2 {
                for dx in -2i64..=2 {
                    let Some(j) = f.at(x, y, dx * step, dy * step) else {
                        continue;
                    };
                    if !has_surface(f.guide[j][3]) {
                        continue;
                    }
                    let h = B3[dx.unsigned_abs() as usize] * B3[dy.unsigned_abs() as usize];
                    let wg = f.geometry_weight(s, i, j, (dx * step) as f32, (dy * step) as f32);
                    let wl = (-(luminance(color[j]) - lp).abs() / dl).exp();
                    let w = h * wg * wl;
                    for (a, b) in sc.iter_mut().zip(color[j]) {
                        *a += w * b;
                    }
                    sv += w * w * variance[j];
                    sw += w;
                }
            }
            // The centre tap weighs 9/64 for a unit normal; a degenerate normal passes through.
            if sw <= 0.0 {
                out_c[i] = color[i];
                out_v[i] = variance[i];
                continue;
            }
            let inv = 1.0 / sw;
            out_c[i] = sc.map(|v| v * inv);
            out_v[i] = sv * inv * inv;
        }
    }
    (out_c, out_v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noisy colors, guides and motion vectors of a frame.
    type Frame = (Vec<[f32; 3]>, Vec<[f32; 4]>, Vec<[f32; 2]>);

    fn flat(w: u32, h: u32, value: [f32; 3]) -> Frame {
        let n = (w * h) as usize;
        (vec![value; n], vec![[0.0, 0.0, 1.0, 10.0]; n], vec![[0.0; 2]; n])
    }

    #[test]
    fn defaults_validate() {
        GiDenoiseSettings::default()
            .validate()
            .expect("defaults are valid");
    }

    #[test]
    fn out_of_range_settings_are_rejected_with_their_name() {
        let cases: [(&str, GiDenoiseSettings); 8] = [
            (
                "antilag_sigma",
                GiDenoiseSettings {
                    antilag_sigma: -1.0,
                    ..GiDenoiseSettings::default()
                },
            ),
            (
                "antilag_block",
                GiDenoiseSettings {
                    antilag_block: 1,
                    ..GiDenoiseSettings::default()
                },
            ),
            (
                "alpha_color",
                GiDenoiseSettings {
                    alpha_color: 0.0,
                    ..GiDenoiseSettings::default()
                },
            ),
            (
                "alpha_moments",
                GiDenoiseSettings {
                    alpha_moments: f32::NAN,
                    ..GiDenoiseSettings::default()
                },
            ),
            (
                "max_history",
                GiDenoiseSettings {
                    max_history: 0.5,
                    ..GiDenoiseSettings::default()
                },
            ),
            (
                "atrous_iterations",
                GiDenoiseSettings {
                    atrous_iterations: MAX_ATROUS_ITERATIONS + 1,
                    ..GiDenoiseSettings::default()
                },
            ),
            (
                "variance_radius",
                GiDenoiseSettings {
                    variance_radius: MAX_VARIANCE_RADIUS + 1,
                    ..GiDenoiseSettings::default()
                },
            ),
            (
                "depth_epsilon",
                GiDenoiseSettings {
                    depth_epsilon: 0.0,
                    ..GiDenoiseSettings::default()
                },
            ),
        ];
        for (name, settings) in cases {
            let err = GiDenoiser::new(settings).expect_err(name);
            assert_eq!(err.code(), "gi_denoise_invalid_setting");
            assert!(
                matches!(err, GiDenoiseError::InvalidSetting { name: n, .. } if n == name),
                "{err}"
            );
        }
    }

    #[test]
    fn wrong_buffer_sizes_and_empty_frames_are_errors_and_keep_history() {
        let mut d = GiDenoiser::new(GiDenoiseSettings::default()).expect("denoiser");
        let (noisy, guide, motion) = flat(4, 4, [1.0; 3]);
        let ok = GiDenoiseInput {
            width: 4,
            height: 4,
            noisy: &noisy,
            guide: &guide,
            motion: &motion,
        };
        d.denoise(&ok).expect("first frame");
        let short = GiDenoiseInput {
            motion: &motion[..3],
            ..ok
        };
        let err = d.denoise(&short).expect_err("short motion");
        assert_eq!(err.code(), "gi_denoise_size_mismatch");
        assert!(matches!(
            err,
            GiDenoiseError::SizeMismatch {
                buffer: "motion",
                got: 3,
                ..
            }
        ));
        let empty = GiDenoiseInput {
            width: 0,
            noisy: &[],
            guide: &[],
            motion: &[],
            ..ok
        };
        assert_eq!(
            d.denoise(&empty).expect_err("empty").code(),
            "gi_denoise_empty_frame"
        );
        assert!(d.has_history(), "a rejected frame leaves the history alone");
        assert_eq!(d.frames(), 1);
    }

    #[test]
    fn clean_constant_input_passes_through_and_history_grows() {
        let mut d = GiDenoiser::new(GiDenoiseSettings::default()).expect("denoiser");
        let (noisy, guide, motion) = flat(16, 12, [0.5, 1.0, 2.0]);
        let input = GiDenoiseInput {
            width: 16,
            height: 12,
            noisy: &noisy,
            guide: &guide,
            motion: &motion,
        };
        for frame in 1..=40u32 {
            let out = d.denoise(&input).expect("frame");
            for c in &out.color {
                for (got, want) in c.iter().zip([0.5, 1.0, 2.0]) {
                    assert!((got - want).abs() <= 1e-5 * want, "frame {frame}: {c:?}");
                }
            }
            let want_len = (frame as f32).min(32.0);
            assert!(
                out.history_len.iter().all(|l| (l - want_len).abs() < 1e-4),
                "frame {frame}"
            );
        }
    }

    #[test]
    fn sky_pixels_output_zero_and_hold_no_history() {
        let mut d = GiDenoiser::new(GiDenoiseSettings::default()).expect("denoiser");
        let (noisy, mut guide, motion) = flat(8, 8, [1.0; 3]);
        for g in guide.iter_mut().take(8) {
            g[3] = 0.0;
        }
        guide[9][3] = f32::INFINITY;
        let input = GiDenoiseInput {
            width: 8,
            height: 8,
            noisy: &noisy,
            guide: &guide,
            motion: &motion,
        };
        d.denoise(&input).expect("frame 1");
        let out = d.denoise(&input).expect("frame 2");
        for i in (0..8).chain([9]) {
            assert_eq!(out.color[i], [0.0; 3]);
            assert_eq!(out.history_len[i], 0.0);
        }
        assert!((out.color[10][0] - 1.0).abs() < 1e-5, "{:?}", out.color[10]);
    }

    #[test]
    fn nan_inf_and_negative_input_never_reach_the_output() {
        let mut d = GiDenoiser::new(GiDenoiseSettings::default()).expect("denoiser");
        let (mut noisy, guide, motion) = flat(8, 8, [1.0; 3]);
        noisy[3] = [f32::NAN, f32::INFINITY, -4.0];
        noisy[20] = [f32::NEG_INFINITY; 3];
        let input = GiDenoiseInput {
            width: 8,
            height: 8,
            noisy: &noisy,
            guide: &guide,
            motion: &motion,
        };
        for _ in 0..4 {
            let out = d.denoise(&input).expect("frame");
            assert!(out.color.iter().flatten().all(|v| v.is_finite() && *v >= 0.0));
            assert!(out.variance.iter().all(|v| v.is_finite() && *v >= 0.0));
        }
    }

    #[test]
    fn reset_and_resize_drop_the_history() {
        let mut d = GiDenoiser::new(GiDenoiseSettings::default()).expect("denoiser");
        let (noisy, guide, motion) = flat(8, 8, [1.0; 3]);
        let input = GiDenoiseInput {
            width: 8,
            height: 8,
            noisy: &noisy,
            guide: &guide,
            motion: &motion,
        };
        d.denoise(&input).expect("frame");
        let out = d.denoise(&input).expect("frame");
        assert!(out.history_len.iter().all(|l| *l == 2.0));
        d.reset();
        assert!(!d.has_history());
        let out = d.denoise(&input).expect("after reset");
        assert!(
            out.history_len.iter().all(|l| *l == 1.0),
            "a cut restarts every history"
        );
        let (noisy, guide, motion) = flat(4, 8, [1.0; 3]);
        let resized = GiDenoiseInput {
            width: 4,
            height: 8,
            noisy: &noisy,
            guide: &guide,
            motion: &motion,
        };
        let out = d.denoise(&resized).expect("resized");
        assert!(
            out.history_len.iter().all(|l| *l == 1.0),
            "a resize restarts every history"
        );
    }

    #[test]
    fn a_surface_change_rejects_the_history() {
        let mut d = GiDenoiser::new(GiDenoiseSettings::default()).expect("denoiser");
        let (noisy, guide, motion) = flat(8, 8, [1.0; 3]);
        let input = GiDenoiseInput {
            width: 8,
            height: 8,
            noisy: &noisy,
            guide: &guide,
            motion: &motion,
        };
        d.denoise(&input).expect("frame");
        d.denoise(&input).expect("frame");
        // Pixel 0 moves 50 % farther, pixel 1 turns 90°, pixel 2 moves 5 % (inside tolerance).
        let mut moved_guide = guide.clone();
        moved_guide[0][3] = 15.0;
        moved_guide[1] = [1.0, 0.0, 0.0, 10.0];
        moved_guide[2][3] = 10.5;
        let changed = GiDenoiseInput {
            guide: &moved_guide,
            ..input
        };
        let out = d.denoise(&changed).expect("frame");
        assert_eq!(out.history_len[0], 1.0, "depth change rejects the history");
        assert_eq!(out.history_len[1], 1.0, "normal change rejects the history");
        assert_eq!(out.history_len[2], 3.0, "a small depth change keeps it");
        assert_eq!(out.history_len[3], 3.0);
    }

    #[test]
    fn subpixel_motion_reads_the_history_bilinearly() {
        // No spatial pass of any kind and no anti-lag (the zero frame below would read as the
        // light going out): the output is the temporal blend alone.
        let settings = GiDenoiseSettings {
            atrous_iterations: 0,
            spatial_variance_frames: 0.0,
            antilag_sigma: 0.0,
            ..GiDenoiseSettings::default()
        };
        let mut d = GiDenoiser::new(settings).expect("denoiser");
        let (mut noisy, guide, motion) = flat(8, 1, [0.0; 3]);
        // Frame 1: a ramp 0, 1, 2, … along x.
        for (x, c) in noisy.iter_mut().enumerate() {
            *c = [x as f32; 3];
        }
        let input = GiDenoiseInput {
            width: 8,
            height: 1,
            noisy: &noisy,
            guide: &guide,
            motion: &motion,
        };
        d.denoise(&input).expect("frame 1");
        // Frame 2: zero input, history read half a pixel to the right. With a history of one
        // frame the new sample weighs 1/2: out = (ramp(x + 0.5)) / 2.
        let zeros = vec![[0.0f32; 3]; 8];
        let shifted = vec![[0.5f32, 0.0]; motion.len()];
        let moved = GiDenoiseInput {
            noisy: &zeros,
            motion: &shifted,
            ..input
        };
        let out = d.denoise(&moved).expect("frame 2");
        for x in 0..7 {
            let want = (x as f32 + 0.5) * 0.5;
            assert!((out.color[x][0] - want).abs() < 1e-5, "x {x}: {:?}", out.color[x]);
            assert!((out.history_len[x] - 2.0).abs() < 1e-6);
        }
        // The last pixel's right tap is off screen: only the left tap (weight 1/2) counts.
        assert!((out.color[7][0] - 3.5).abs() < 1e-5, "{:?}", out.color[7]);
    }

    #[test]
    fn denoising_is_deterministic() {
        let run = || {
            let mut d = GiDenoiser::new(GiDenoiseSettings::default()).expect("denoiser");
            let sc = scenario::GiScenario::new(scenario::GiScenarioKind::Disocclusion);
            let mut last = GiDenoiseOutput::default();
            for t in 0..6 {
                let f = sc.frame(t);
                last = d.denoise(&f.input()).expect("frame");
            }
            last
        };
        let a = run();
        let b = run();
        assert!(
            a.color
                .iter()
                .flatten()
                .zip(b.color.iter().flatten())
                .all(|(x, y)| x.to_bits() == y.to_bits())
        );
    }

    #[test]
    fn anti_lag_lets_a_lighting_change_through() {
        let run = |antilag_sigma: f32| {
            let settings = GiDenoiseSettings {
                antilag_sigma,
                ..GiDenoiseSettings::default()
            };
            let mut d = GiDenoiser::new(settings).expect("denoiser");
            let (dim, guide, motion) = flat(16, 12, [1.0; 3]);
            let bright = vec![[4.0f32; 3]; dim.len()];
            let dim_input = GiDenoiseInput {
                width: 16,
                height: 12,
                noisy: &dim,
                guide: &guide,
                motion: &motion,
            };
            let bright_input = GiDenoiseInput {
                noisy: &bright,
                ..dim_input
            };
            for _ in 0..10 {
                d.denoise(&dim_input).expect("dim frame");
            }
            d.denoise(&bright_input).expect("bright frame")
        };
        // A clean input has no noise to explain any change: the squares see 1 -> 4 and blend
        // the new frame in at 3/4, cutting the history to 4/3 frames.
        let on = run(5.0);
        assert!(
            on.color.iter().all(|c| (c[0] - 3.25).abs() < 1e-3),
            "{:?}",
            on.color[0]
        );
        assert!(on.history_len.iter().all(|l| (l - 4.0 / 3.0).abs() < 1e-4));
        // Off, the settled history takes the new frame at alpha 0.2: 1 + 0.2 * 3.
        let off = run(0.0);
        assert!(
            off.color.iter().all(|c| (c[0] - 1.6).abs() < 1e-3),
            "{:?}",
            off.color[0]
        );
        assert!(off.history_len.iter().all(|l| (l - 11.0).abs() < 1e-4));
    }

    #[test]
    fn a_clean_shadow_edge_is_not_blurred() {
        // Zero temporal variance (the spatial estimate off): the luminance weight stops the
        // à-trous filter at any step in brightness, however wide its step.
        let settings = GiDenoiseSettings {
            spatial_variance_frames: 0.0,
            ..GiDenoiseSettings::default()
        };
        let mut d = GiDenoiser::new(settings).expect("denoiser");
        let (mut noisy, guide, motion) = flat(16, 8, [1.0; 3]);
        for (i, c) in noisy.iter_mut().enumerate() {
            if i % 16 < 8 {
                *c = [0.25; 3];
            }
        }
        let input = GiDenoiseInput {
            width: 16,
            height: 8,
            noisy: &noisy,
            guide: &guide,
            motion: &motion,
        };
        for _ in 0..8 {
            let out = d.denoise(&input).expect("frame");
            for (o, n) in out.color.iter().zip(&noisy) {
                assert!((o[0] - n[0]).abs() <= 1e-4 * n[0], "{o:?} vs {n:?}");
            }
        }
    }
}
