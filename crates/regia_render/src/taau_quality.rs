// SPDX-License-Identifier: MPL-2.0
//! Temporal-quality metrics of the TAAU resolve on synthetic moving-camera sequences (ADR-1151,
//! `book/agents/temporal-quality.md`, agent method `render.temporal_quality`).
//!
//! [`run_sequence`] renders a layered 2.5D scene ([`TemporalScene`]) at the render resolution
//! with the Halton(2, 3) jitter, writes the motion and depth that the velocity pass would, and
//! resolves every frame with a CPU copy of `shaders/taau.wgsl` (the same taps, clip, weights and
//! constants; [`ResolveTuning::SHIPPED`]). Each output frame is compared with an unjittered
//! 4 × 4 supersampled ground truth, which gives three numbers ([`TemporalReport`]):
//!
//! - **ghost leak**: how much of a foreground layer's color is left on background it uncovered
//!   in the last six frames (background points that were behind it, followed with their own
//!   layer's motion), as a fraction of the foreground-to-background difference. The baseline is
//!   the same sequence resolved without the foreground layers, and the same statistic on
//!   background the foreground never covered is subtracted, so the upscale's and the
//!   accumulation's ordinary softness does not count as ghosting;
//! - **RMS error** of the luminance against the ground truth;
//! - **peak ratio**: the output over the ground truth on the brightest pixels (ground-truth
//!   luminance at least half the frame's peak, away from the side borders), which drops when
//!   small bright features (stars, a moon's limb, a distant light) fade under motion.
//!
//! The model is a reference for tuning and regression tests, not a capture of the GPU: its jitter
//! is the Halton(2, 3) sequence but not the renderer's phase or y sign (sample `frame + 1`, +y down),
//! the history stays in `f32` (the GPU keeps half floats), and the scene is flat layers that translate
//! on screen (a turning camera moves distant layers by the same amount; parallax is a layer
//! moving faster than the one behind it). The GPU resolve is tested on the same kind of sequence
//! in `taau_tests.rs`.

use regia_core::glam::{IVec2, Vec2, Vec3};

pub use crate::taau::MAX_HISTORY_WEIGHT;

/// The resolve's tunable constants. [`ResolveTuning::SHIPPED`] holds the values compiled into
/// `shaders/taau.wgsl`; the agent method can try others in the model only.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolveTuning {
    /// History weight in motion (the renderer passes 0.9; a held camera climbs toward 0.98).
    pub history_weight: f32,
    /// Half-width of the variance box in standard deviations (Salvi, GDC 2016).
    pub variance_gamma: f32,
    /// Largest cut of the history weight for a fetch halfway between history texels.
    pub fractional_penalty: f32,
    /// Motion disagreement (output pixels) below which the history is fully trusted.
    pub velocity_trust_px: f32,
    /// Motion disagreement (output pixels) at which the history is dropped.
    pub velocity_reject_px: f32,
    /// When upscaling, the motion test takes the largest disagreement of the 2 × 2 render texels
    /// around the history fetch (ADR-1151); `false` reads the nearest texel only (before v0.1.28).
    pub wide_motion_test: bool,
}

impl ResolveTuning {
    /// The constants of `shaders/taau.wgsl` (checked against the shader source by a unit test).
    pub const SHIPPED: Self = Self {
        history_weight: 0.9,
        variance_gamma: 1.25,
        fractional_penalty: 0.35,
        velocity_trust_px: 0.5,
        velocity_reject_px: 2.0,
        wide_motion_test: true,
    };
}

impl Default for ResolveTuning {
    fn default() -> Self {
        Self::SHIPPED
    }
}

/// Color of a layer, in the layer's own pixel coordinates (linear HDR, pre-exposed).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pattern {
    /// One color.
    Flat([f32; 3]),
    /// Smooth two-dimensional waves between the grays `lo` and `hi` (slightly warm), `period`
    /// pixels across: a textured surface whose 3 × 3 range contains mid grays.
    Waves {
        /// Horizontal period in pixels (the vertical one is 1.7 times longer).
        period: f32,
        /// Darkest gray.
        lo: f32,
        /// Brightest gray.
        hi: f32,
    },
    /// A star field on a `background` gray: about one cell in three of an 8 px grid holds a
    /// 1.5 px square of `value`, at a hashed position.
    Stars {
        /// Star brightness.
        value: f32,
        /// Sky brightness between the stars.
        background: f32,
    },
}

/// Where a layer covers the screen, in its own pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Coverage {
    /// The whole plane.
    Everywhere,
    /// The rectangle from the layer's origin, `width` × `height` pixels.
    Rect {
        /// Width in pixels.
        width: f32,
        /// Height in pixels.
        height: f32,
    },
    /// Every row at or below `y` (a ground band under a sky).
    Below {
        /// First covered row, pixels.
        y: f32,
    },
}

/// One flat layer of a [`TemporalScene`]. The nearest layer (smallest depth) wins a pixel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TemporalLayer {
    /// Standard-Z depth (0 near, 1 far). A layer at 1 is sky: before ADR-1151 the velocity pass
    /// wrote no motion there ([`SequenceSettings::sky_motion`]).
    pub depth: f32,
    /// Its color.
    pub pattern: Pattern,
    /// Where it covers the screen.
    pub coverage: Coverage,
    /// Screen position (output pixels) of the layer's origin at frame 0.
    pub start: [f32; 2],
    /// Screen motion in output pixels per frame.
    pub velocity: [f32; 2],
    /// The ghost leak follows this layer (the first flat foreground layer is the reference color).
    pub foreground: bool,
}

/// A synthetic sequence: layers that translate on screen (see the module docs).
#[derive(Clone, Debug, PartialEq)]
pub struct TemporalScene {
    /// Scenario name ([`SCENARIOS`]).
    pub name: String,
    /// Layers, at most 32.
    pub layers: Vec<TemporalLayer>,
}

/// Size, length and options of a [`run_sequence`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SequenceSettings {
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Render resolution over output resolution (0.25-1).
    pub render_scale: f32,
    /// Frames resolved.
    pub frames: u32,
    /// Frames resolved before the metrics start (the history converging).
    pub warmup: u32,
    /// The velocity pass writes the sky's rotation (ADR-1151). `false` reproduces the earlier
    /// pass, which wrote zero motion at depth 1.
    pub sky_motion: bool,
    /// The resolve's constants.
    pub tuning: ResolveTuning,
}

impl Default for SequenceSettings {
    fn default() -> Self {
        Self {
            width: 96,
            height: 48,
            render_scale: 1.0,
            frames: 24,
            warmup: 8,
            sky_motion: true,
            tuning: ResolveTuning::SHIPPED,
        }
    }
}

/// The metrics of one sequence (module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TemporalReport {
    /// Frames the metrics cover (`frames − warmup`).
    pub frames_measured: u32,
    /// Foreground color left on uncovered background, as a fraction of the foreground-to-background
    /// difference, less the same statistic on never-covered background (module docs). 0 without a
    /// foreground. Around 0.02 or less is no visible trail.
    pub ghost_leak: f32,
    /// Uncovered pixels the ghost leak averaged (all measured frames).
    pub disoccluded_pixels: u64,
    /// RMS luminance error against the supersampled ground truth.
    pub rms_error: f32,
    /// Output over ground-truth luminance on the brightest pixels, averaged over the frames
    /// (module docs). 1 is exact; a still star field reads about 0.5, because the resolve's
    /// inverse-luminance blend (the firefly guard) dims sub-pixel HDR points.
    pub peak_ratio: f32,
}

/// The built-in scenarios, by name ([`scenario`]).
pub const SCENARIOS: [&str; 5] = [
    "parallax_pan",
    "moving_object",
    "tracked_object",
    "bright_object",
    "stars_turning",
];

/// Gray of the foreground box: inside the range of the [`Pattern::Waves`] backgrounds, so the
/// neighborhood clip alone cannot reject its trail (the motion test has to).
const MID_GREY: [f32; 3] = [0.5, 0.45, 0.4];

fn layer(
    depth: f32,
    pattern: Pattern,
    coverage: Coverage,
    start: [f32; 2],
    velocity: [f32; 2],
) -> TemporalLayer {
    TemporalLayer {
        depth,
        pattern,
        coverage,
        start,
        velocity,
        foreground: false,
    }
}

fn foreground(pattern: Pattern, start: [f32; 2], velocity: [f32; 2]) -> TemporalLayer {
    TemporalLayer {
        foreground: true,
        ..layer(
            0.3,
            pattern,
            Coverage::Rect {
                width: 14.0,
                height: 18.0,
            },
            start,
            velocity,
        )
    }
}

/// A built-in scenario by name, laid out for the default 96 × 48 output.
///
/// - `parallax_pan`: the camera pans; a textured wall moves 1 px/frame, a mid-gray box in front
///   of it 3 px/frame (parallax: the box uncovers wall every frame).
/// - `moving_object`: a held camera; the box crosses the wall at 3 px/frame.
/// - `tracked_object`: the camera follows the box (still on screen); the wall moves 3 px/frame.
/// - `bright_object`: as `parallax_pan`, with an HDR box (luminance ~3.6) over a dark wall.
/// - `stars_turning`: the camera turns at 3 px/frame over a star sky (depth 1) and a ground band.
#[must_use]
pub fn scenario(name: &str) -> Option<TemporalScene> {
    let waves = Pattern::Waves {
        period: 9.0,
        lo: 0.1,
        hi: 0.9,
    };
    let layers = match name {
        "parallax_pan" => vec![
            layer(0.9, waves, Coverage::Everywhere, [0.0, 0.0], [-1.0, 0.0]),
            foreground(Pattern::Flat(MID_GREY), [80.0, 15.0], [-3.0, 0.0]),
        ],
        "moving_object" => vec![
            layer(0.9, waves, Coverage::Everywhere, [0.0, 0.0], [0.0, 0.0]),
            foreground(Pattern::Flat(MID_GREY), [8.0, 15.0], [3.0, 0.0]),
        ],
        "tracked_object" => vec![
            layer(0.9, waves, Coverage::Everywhere, [0.0, 0.0], [-3.0, 0.0]),
            foreground(Pattern::Flat(MID_GREY), [40.0, 15.0], [0.0, 0.0]),
        ],
        "bright_object" => vec![
            layer(
                0.9,
                Pattern::Waves {
                    period: 9.0,
                    lo: 0.05,
                    hi: 0.3,
                },
                Coverage::Everywhere,
                [0.0, 0.0],
                [-1.0, 0.0],
            ),
            foreground(Pattern::Flat([4.0, 3.5, 3.0]), [80.0, 15.0], [-3.0, 0.0]),
        ],
        "stars_turning" => vec![
            layer(
                1.0,
                Pattern::Stars {
                    value: 3.0,
                    background: 0.02,
                },
                Coverage::Everywhere,
                [0.0, 0.0],
                [-3.0, 0.0],
            ),
            layer(
                0.5,
                Pattern::Waves {
                    period: 11.0,
                    lo: 0.1,
                    hi: 0.5,
                },
                Coverage::Below { y: 34.0 },
                [0.0, 0.0],
                [-3.0, 0.0],
            ),
        ],
        _ => return None,
    };
    Some(TemporalScene {
        name: name.to_string(),
        layers,
    })
}

/// Integer hash of a star cell (a 32-bit mix in the style of MurmurHash2's finaliser,
/// Appleby 2008, public domain).
fn cell_hash(cx: i32, cy: i32) -> u32 {
    let mut h = (cx as u32).wrapping_mul(0x8da6_b343) ^ (cy as u32).wrapping_mul(0xd816_3841);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^ (h >> 15)
}

/// Star grid cell size in pixels.
const STAR_CELL: f32 = 8.0;

impl Pattern {
    fn colour(self, x: f32, y: f32) -> Vec3 {
        match self {
            Self::Flat(c) => Vec3::from(c),
            Self::Waves { period, lo, hi } => {
                let tau = std::f32::consts::TAU;
                let s = 0.5 + 0.5 * (tau * x / period).sin() * (tau * y / (period * 1.7)).cos();
                let v = lo + (hi - lo) * s;
                Vec3::new(v, v * 0.9, v * 0.8)
            }
            Self::Stars { value, background } => {
                let (cx, cy) = ((x / STAR_CELL).floor(), (y / STAR_CELL).floor());
                let h = cell_hash(cx as i32, cy as i32);
                let mut v = background;
                if h.is_multiple_of(3) {
                    let sx = cx * STAR_CELL + 1.0 + ((h >> 8) % 6) as f32;
                    let sy = cy * STAR_CELL + 1.0 + ((h >> 16) % 6) as f32;
                    if (x - sx).abs() < 0.75 && (y - sy).abs() < 0.75 {
                        v = value;
                    }
                }
                Vec3::splat(v)
            }
        }
    }
}

impl Coverage {
    fn covers(self, x: f32, y: f32) -> bool {
        match self {
            Self::Everywhere => true,
            Self::Rect { width, height } => (0.0..width).contains(&x) && (0.0..height).contains(&y),
            Self::Below { y: top } => y >= top,
        }
    }
}

impl TemporalScene {
    /// Color, standard-Z depth and layer index of the nearest layer at screen pixel
    /// coordinates `(x, y)` on `frame` (black sky at depth 1 where no layer covers).
    fn shade(&self, x: f32, y: f32, frame: u32) -> (Vec3, f32, Option<usize>) {
        let mut best = (Vec3::ZERO, f32::INFINITY, None);
        for (i, l) in self.layers.iter().enumerate() {
            let lx = x - (l.start[0] + l.velocity[0] * frame as f32);
            let ly = y - (l.start[1] + l.velocity[1] * frame as f32);
            if l.depth < best.1 && l.coverage.covers(lx, ly) {
                best = (l.pattern.colour(lx, ly), l.depth, Some(i));
            }
        }
        (best.0, best.1.min(1.0), best.2)
    }
}

/// An RGB image with clamp-to-edge fetches.
#[derive(Clone)]
struct Image {
    width: usize,
    height: usize,
    texels: Vec<Vec3>,
}

impl Image {
    fn at(&self, x: i64, y: i64) -> Vec3 {
        let x = x.clamp(0, self.width as i64 - 1) as usize;
        let y = y.clamp(0, self.height as i64 - 1) as usize;
        self.texels[y * self.width + x]
    }

    /// Bilinear fetch at `uv` (a linear-filtering sampler with clamp-to-edge).
    fn bilinear(&self, uv: Vec2) -> Vec3 {
        let x = uv.x * self.width as f32 - 0.5;
        let y = uv.y * self.height as f32 - 0.5;
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (xi, yi) = (x0 as i64, y0 as i64);
        self.at(xi, yi) * ((1.0 - fx) * (1.0 - fy))
            + self.at(xi + 1, yi) * (fx * (1.0 - fy))
            + self.at(xi, yi + 1) * ((1.0 - fx) * fy)
            + self.at(xi + 1, yi + 1) * (fx * fy)
    }
}

/// One rendered frame at the render resolution: color, depth and motion (previous uv − current
/// uv), as the forward and velocity passes leave them.
struct Rendered {
    colour: Image,
    depth: Vec<f32>,
    motion: Vec<Vec2>,
}

/// Renders `frame` at `rw` × `rh` with every sample offset by `jitter` render pixels, in the
/// screen coordinates of an `output`-sized frame. `sky_motion: false` writes no motion at depth 1.
fn render(
    scene: &TemporalScene,
    output: (usize, usize),
    (rw, rh): (usize, usize),
    frame: u32,
    jitter: Vec2,
    sky_motion: bool,
) -> Rendered {
    let (ow, oh) = (output.0 as f32, output.1 as f32);
    let mut colour = Vec::with_capacity(rw * rh);
    let mut depth = Vec::with_capacity(rw * rh);
    let mut motion = Vec::with_capacity(rw * rh);
    for j in 0..rh {
        for i in 0..rw {
            let sx = (i as f32 + 0.5 + jitter.x) * ow / rw as f32;
            let sy = (j as f32 + 0.5 + jitter.y) * oh / rh as f32;
            let (c, d, id) = scene.shade(sx, sy, frame);
            colour.push(c);
            depth.push(d);
            let m = id
                .map(|id| &scene.layers[id])
                .filter(|l| l.depth < 1.0 || sky_motion)
                .map_or(Vec2::ZERO, |l| -Vec2::from(l.velocity) / Vec2::new(ow, oh));
            motion.push(m);
        }
    }
    Rendered {
        colour: Image {
            width: rw,
            height: rh,
            texels: colour,
        },
        depth,
        motion,
    }
}

/// Supersampled ground truth of `frame`: color and, per pixel, a bit per layer seen.
fn ground_truth(scene: &TemporalScene, width: usize, height: usize, frame: u32) -> (Vec<Vec3>, Vec<u32>) {
    const SS: usize = 4;
    let mut colour = Vec::with_capacity(width * height);
    let mut seen = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let mut sum = Vec3::ZERO;
            let mut bits = 0u32;
            for a in 0..SS {
                for b in 0..SS {
                    let sx = x as f32 + (b as f32 + 0.5) / SS as f32;
                    let sy = y as f32 + (a as f32 + 0.5) / SS as f32;
                    let (c, _, id) = scene.shade(sx, sy, frame);
                    sum += c;
                    if let Some(id) = id {
                        bits |= 1 << id.min(31);
                    }
                }
            }
            colour.push(sum / (SS * SS) as f32);
            seen.push(bits);
        }
    }
    (colour, seen)
}

/// Largest value the resolve writes (`MAX_HDR` in the shader).
const MAX_HDR: f32 = 6.0e4;
/// Keys' cubic parameter of the history fetch (`HISTORY_CUBIC_A` in the shader).
const HISTORY_CUBIC_A: f32 = -1.0;

fn luma(c: Vec3) -> f32 {
    c.dot(Vec3::new(0.2126, 0.7152, 0.0722))
}

/// YCoCg with the halved Co of the shader (Malvar & Sullivan 2003).
fn ycocg(c: Vec3) -> Vec3 {
    let y = c.dot(Vec3::new(0.25, 0.5, 0.25));
    Vec3::new(y, (c.x - c.z) * 0.5, c.y - y)
}

fn rgb_from_ycocg(c: Vec3) -> Vec3 {
    let tmp = c.x - c.z;
    Vec3::new(tmp + c.y, c.x + c.z, tmp - c.y)
}

/// WGSL `fract`: `x − floor(x)` (also for negative `x`).
fn fract(v: Vec2) -> Vec2 {
    v - v.floor()
}

/// The shader's five-tap cubic history fetch.
fn sample_history(history: &Image, uv: Vec2) -> Vec3 {
    let size = Vec2::new(history.width as f32, history.height as f32);
    let pos = uv * size;
    let p1 = (pos - 0.5).floor() + 0.5;
    let f = pos - p1;
    let a = HISTORY_CUBIC_A;
    let g = Vec2::ONE - f;
    let w0 = a * f * g * g;
    let w1 = ((a + 2.0) * f - (a + 3.0)) * f * f + 1.0;
    let w2 = ((a + 2.0) * g - (a + 3.0)) * g * g + 1.0;
    let w3 = a * g * f * f;
    let w12 = w1 + w2;
    let p0 = (p1 - 1.0) / size;
    let p3 = (p1 + 2.0) / size;
    let p12 = (p1 + w2 / w12) / size;
    let sum = history.bilinear(Vec2::new(p12.x, p0.y)) * (w12.x * w0.y)
        + history.bilinear(Vec2::new(p0.x, p12.y)) * (w0.x * w12.y)
        + history.bilinear(p12) * (w12.x * w12.y)
        + history.bilinear(Vec2::new(p3.x, p12.y)) * (w3.x * w12.y)
        + history.bilinear(Vec2::new(p12.x, p3.y)) * (w12.x * w3.y);
    let weight = w12.x * w0.y + w0.x * w12.y + w12.x * w12.y + w3.x * w12.y + w12.x * w3.y;
    (sum / weight).max(Vec3::ZERO)
}

/// Smallest half-width of the clip box per channel, as a share of the box's top luma
/// (`CLIP_EXTENT_FLOOR` in the shader): above the half-float rounding of the history.
const CLIP_EXTENT_FLOOR: f32 = 1.0 / 256.0;

fn clip_to_box(history: Vec3, lo: Vec3, hi: Vec3) -> Vec3 {
    let floor_extent = (CLIP_EXTENT_FLOOR * hi.x).max(1.0e-6);
    let extent = ((hi - lo) * 0.5).max(Vec3::splat(floor_extent));
    let mid = (hi + lo) * 0.5;
    let offset = history - mid;
    let scale = (offset / extent).abs().max_element();
    if scale > 1.0 {
        mid + offset / scale
    } else {
        history
    }
}

/// Last frame's state the resolve reads.
struct History<'a> {
    image: &'a Image,
    /// Last frame's motion target (render resolution).
    motion: &'a [Vec2],
}

/// The CPU copy of `shaders/taau.wgsl`'s `fs_main` for every output pixel.
fn resolve(
    current: &Rendered,
    history: Option<&History<'_>>,
    width: usize,
    height: usize,
    tuning: &ResolveTuning,
) -> Image {
    let (rw, rh) = (current.colour.width, current.colour.height);
    let texel = Vec2::new(1.0 / rw as f32, 1.0 / rh as f32);
    let out_size = Vec2::new(width as f32, height as f32);
    // The shader's test: a render texel larger than an output pixel on either axis.
    let upscaling = rw < width || rh < height;
    let sample_current = |uv: Vec2| {
        current
            .colour
            .bilinear(uv)
            .clamp(Vec3::ZERO, Vec3::splat(MAX_HDR))
    };
    let mut texels = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let uv = Vec2::new((x as f32 + 0.5) / width as f32, (y as f32 + 0.5) / height as f32);
            let (mut min_c, mut max_c) = (Vec3::splat(1.0e6), Vec3::splat(-1.0e6));
            let (mut m1, mut m2) = (Vec3::ZERO, Vec3::ZERO);
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let ycc = ycocg(sample_current(uv + Vec2::new(dx as f32, dy as f32) * texel));
                    min_c = min_c.min(ycc);
                    max_c = max_c.max(ycc);
                    m1 += ycc;
                    m2 += ycc * ycc;
                }
            }
            let centre_rgb = sample_current(uv);
            let Some(history) = history else {
                texels.push(centre_rgb);
                continue;
            };
            let mean = m1 / 9.0;
            let sigma = (m2 / 9.0 - mean * mean).max(Vec3::ZERO).powf(0.5);
            let centre = ycocg(centre_rgb);
            let lo = min_c.max((mean - tuning.variance_gamma * sigma).min(centre));
            let hi = max_c.min((mean + tuning.variance_gamma * sigma).max(centre));
            // The motion of the closest surface in the 3 × 3 neighborhood.
            let size = Vec2::new(rw as f32, rh as f32);
            let base = (uv * size)
                .as_ivec2()
                .clamp(IVec2::ZERO, IVec2::new(rw as i32 - 1, rh as i32 - 1));
            let (mut best, mut best_depth) = (base, 2.0f32);
            for oy in -1..=1 {
                for ox in -1..=1 {
                    let c = (base + IVec2::new(ox, oy))
                        .clamp(IVec2::ZERO, IVec2::new(rw as i32 - 1, rh as i32 - 1));
                    let d = current.depth[c.y as usize * rw + c.x as usize];
                    if d < best_depth {
                        best_depth = d;
                        best = c;
                    }
                }
            }
            let motion = current.motion[best.y as usize * rw + best.x as usize];
            let history_uv = uv + motion;
            if history_uv.cmplt(Vec2::ZERO).any() || history_uv.cmpgt(Vec2::ONE).any() {
                texels.push(centre_rgb);
                continue;
            }
            let fetched = sample_history(history.image, history_uv);
            let clipped = rgb_from_ycocg(clip_to_box(ycocg(fetched), lo, hi));
            let mut weight = tuning.history_weight.clamp(0.0, MAX_HISTORY_WEIGHT);
            let hsize = Vec2::new(history.image.width as f32, history.image.height as f32);
            let f = fract(history_uv * hsize - 0.5);
            let between = 4.0 * f * (Vec2::ONE - f);
            weight *= 1.0 - tuning.fractional_penalty * between.max_element();
            if history.motion.len() == rw * rh {
                let last = IVec2::new(rw as i32 - 1, rh as i32 - 1);
                let disagree_at = |p: IVec2| {
                    let p = p.clamp(IVec2::ZERO, last);
                    ((history.motion[p.y as usize * rw + p.x as usize] - motion) * out_size).length()
                };
                let disagree = if upscaling && tuning.wide_motion_test {
                    let base = (history_uv * size - 0.5).floor().as_ivec2();
                    [IVec2::ZERO, IVec2::X, IVec2::Y, IVec2::ONE]
                        .iter()
                        .map(|&o| disagree_at(base + o))
                        .fold(0.0f32, f32::max)
                } else {
                    disagree_at((history_uv * size).as_ivec2())
                };
                let span = (tuning.velocity_reject_px - tuning.velocity_trust_px).max(1.0e-3);
                weight *= (1.0 - (disagree - tuning.velocity_trust_px) / span).clamp(0.0, 1.0);
            }
            let wc = (1.0 - weight) / (1.0 + luma(centre_rgb));
            let wh = weight / (1.0 + luma(clipped.max(Vec3::ZERO)));
            let blended = (centre_rgb * wc + clipped * wh) / (wc + wh).max(1.0e-6);
            texels.push(blended.clamp(Vec3::ZERO, Vec3::splat(MAX_HDR)));
        }
    }
    Image {
        width,
        height,
        texels,
    }
}

/// Frames back the ghost leak looks for a foreground that hid a background point.
const GHOST_FRAMES: u32 = 6;
/// Pixels within this distance (along each axis) of a foreground pixel are its edge, not its trail.
const GHOST_EDGE_PX: usize = 2;
/// Smallest foreground-to-background luminance difference the leak is measured on.
const GHOST_MIN_CONTRAST: f32 = 0.15;
/// Columns at each side left out of the control set and the peak ratio (new content entering
/// the frame has no history yet).
const BORDER_PX: usize = 8;

impl TemporalScene {
    /// The background point at the centre of pixel `(x, y)` on `frame` was behind a foreground
    /// layer on one of the `GHOST_FRAMES` frames before: the resolve uncovered it since.
    fn was_hidden(&self, x: usize, y: usize, frame: u32) -> bool {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let Some(layer) = self.shade(px, py, frame).2.map(|id| &self.layers[id]) else {
            return false;
        };
        if layer.foreground {
            return false;
        }
        (1..=GHOST_FRAMES.min(frame)).any(|k| {
            let sx = px - layer.velocity[0] * k as f32;
            let sy = py - layer.velocity[1] * k as f32;
            self.shade(sx, sy, frame - k)
                .2
                .is_some_and(|id| self.layers[id].foreground)
        })
    }

    /// The same scene without its foreground layers (the ghost leak's baseline).
    fn without_foreground(&self) -> Self {
        Self {
            name: self.name.clone(),
            layers: self.layers.iter().filter(|l| !l.foreground).copied().collect(),
        }
    }
}

/// One measured frame and what it is compared with.
struct Measured<'a> {
    scene: &'a TemporalScene,
    frame: u32,
    out: &'a Image,
    truth: &'a [Vec3],
    /// Per pixel, a bit per layer the supersampled ground truth saw.
    seen: &'a [u32],
    /// The same frame of the sequence without its foreground layers.
    clean: Option<&'a Image>,
    fg_bits: u32,
    fg_luma: f32,
}

/// Running sums of the metrics.
#[derive(Default)]
struct Totals {
    frames: u32,
    rms: f64,
    peak: f64,
    leak_sum: f64,
    leak_count: u64,
    control_sum: f64,
    control_count: u64,
}

impl Totals {
    fn add_frame(&mut self, m: &Measured<'_>) {
        let (w, h) = (m.out.width, m.out.height);
        let truth_luma: Vec<f32> = m.truth.iter().map(|&c| luma(c)).collect();
        let mut sq = 0.0f64;
        for (o, &lt) in m.out.texels.iter().zip(&truth_luma) {
            let d = luma(*o) - lt;
            sq += f64::from(d * d);
        }
        self.frames += 1;
        self.rms += (sq / (w * h) as f64).sqrt();
        // Brightest features: ground-truth luminance at least half the frame's peak.
        let truth_peak = truth_luma.iter().copied().fold(0.0f32, f32::max);
        let (mut out_sum, mut truth_sum) = (0.0f64, 0.0f64);
        for y in 0..h {
            for x in BORDER_PX..w.saturating_sub(BORDER_PX) {
                let i = y * w + x;
                if truth_luma[i] >= 0.5 * truth_peak {
                    out_sum += f64::from(luma(m.out.texels[i]));
                    truth_sum += f64::from(truth_luma[i]);
                }
            }
        }
        if truth_sum > 0.0 {
            self.peak += out_sum / truth_sum;
        }
        let Some(clean) = m.clean.filter(|_| m.fg_bits != 0) else {
            return;
        };
        let fg_free = |x: usize, y: usize| m.seen[y * w + x] & m.fg_bits == 0;
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                // Pure background: no foreground here or within the edge distance on either axis.
                let pure = (0..=GHOST_EDGE_PX).all(|d| {
                    (x < d || fg_free(x - d, y))
                        && (x + d >= w || fg_free(x + d, y))
                        && (y < d || fg_free(x, y - d))
                        && (y + d >= h || fg_free(x, y + d))
                });
                let baseline = luma(clean.texels[i]);
                let contrast = m.fg_luma - baseline;
                if !pure || contrast.abs() <= GHOST_MIN_CONTRAST {
                    continue;
                }
                let r = f64::from((luma(m.out.texels[i]) - baseline) / contrast);
                if m.scene.was_hidden(x, y, m.frame) {
                    self.leak_sum += r;
                    self.leak_count += 1;
                } else if x >= BORDER_PX && x + BORDER_PX < w {
                    self.control_sum += r;
                    self.control_count += 1;
                }
            }
        }
    }

    fn report(&self) -> TemporalReport {
        let frames = f64::from(self.frames.max(1));
        let ghost_leak = if self.leak_count > 0 && self.control_count > 0 {
            self.leak_sum / self.leak_count as f64 - self.control_sum / self.control_count as f64
        } else {
            0.0
        };
        TemporalReport {
            frames_measured: self.frames,
            ghost_leak: ghost_leak as f32,
            disoccluded_pixels: self.leak_count,
            rms_error: (self.rms / frames) as f32,
            peak_ratio: (self.peak / frames) as f32,
        }
    }
}

/// Last frame's resolve and motion target, for the next frame's history.
type Carried = Option<(Image, Vec<Vec2>)>;

/// Renders `frame` and resolves it against `previous`; returns the output and keeps it (with
/// this frame's motion) in `previous`.
fn step(
    scene: &TemporalScene,
    settings: &SequenceSettings,
    render_size: (usize, usize),
    frame: u32,
    previous: &mut Carried,
) -> Image {
    let (w, h) = (settings.width as usize, settings.height as usize);
    // The renderer's Halton(2, 3) jitter of this frame, in render pixels.
    let n = frame + 1;
    let jitter = Vec2::new(crate::taau::halton(n, 2) - 0.5, crate::taau::halton(n, 3) - 0.5);
    let current = render(scene, (w, h), render_size, frame, jitter, settings.sky_motion);
    let out = {
        let history = previous.as_ref().map(|(image, motion)| History { image, motion });
        resolve(&current, history.as_ref(), w, h, &settings.tuning)
    };
    *previous = Some((out.clone(), current.motion));
    out
}

/// Renders and resolves `scene` for `settings.frames` frames and measures the frames after the
/// warm-up (module docs). Sizes are clamped to 8-512 pixels, the frame count to 2-256, the
/// render scale to 0.25-1 and the warm-up to fewer than the frames. With a foreground layer the
/// sequence also runs without it, as the ghost leak's baseline.
#[must_use]
pub fn run_sequence(scene: &TemporalScene, settings: &SequenceSettings) -> TemporalReport {
    let _span = tracing::info_span!("taau_quality_sequence").entered();
    let mut settings = *settings;
    settings.width = settings.width.clamp(8, 512);
    settings.height = settings.height.clamp(8, 512);
    settings.frames = settings.frames.clamp(2, 256);
    settings.warmup = settings.warmup.min(settings.frames - 1);
    settings.render_scale = if settings.render_scale.is_finite() {
        settings.render_scale.clamp(0.25, 1.0)
    } else {
        1.0
    };
    let (w, h) = (settings.width as usize, settings.height as usize);
    let render_size = (
        ((w as f32 * settings.render_scale).round() as usize).max(1),
        ((h as f32 * settings.render_scale).round() as usize).max(1),
    );
    let fg_bits = scene
        .layers
        .iter()
        .take(32)
        .enumerate()
        .filter(|(_, l)| l.foreground)
        .fold(0u32, |bits, (i, _)| bits | (1 << i));
    let fg_luma = scene
        .layers
        .iter()
        .find(|l| l.foreground)
        .map_or(0.0, |l| luma(l.pattern.colour(0.0, 0.0)));
    let clean_scene = (fg_bits != 0).then(|| scene.without_foreground());
    let mut totals = Totals::default();
    let (mut previous, mut clean_previous): (Carried, Carried) = (None, None);
    for frame in 0..settings.frames {
        let out = step(scene, &settings, render_size, frame, &mut previous);
        let clean = clean_scene
            .as_ref()
            .map(|clean| step(clean, &settings, render_size, frame, &mut clean_previous));
        if frame >= settings.warmup {
            let (truth, seen) = ground_truth(scene, w, h, frame);
            totals.add_frame(&Measured {
                scene,
                frame,
                out: &out,
                truth: &truth,
                seen: &seen,
                clean: clean.as_ref(),
                fg_bits,
                fg_luma,
            });
        }
    }
    totals.report()
}

/// One frame of [`TemporalScene::unjittered_frames`] (the GPU parity test in `taau_tests.rs`).
#[cfg(test)]
pub(crate) struct ParityFrame {
    /// Rendered color at the render size.
    pub(crate) colour: Vec<Vec3>,
    /// Rendered standard-Z depth (the layers' `depth`) at the render size.
    pub(crate) depth: Vec<f32>,
    /// The model's resolve at the output size.
    pub(crate) out: Vec<Vec3>,
}

#[cfg(test)]
impl TemporalScene {
    /// Test hook: `frames` frames rendered without jitter at `render` size and resolved by the
    /// model to `output` size with `tuning` (frame 0 has no history), for a GPU resolve to be
    /// compared with on the same inputs.
    pub(crate) fn unjittered_frames(
        &self,
        render_size: (usize, usize),
        output: (usize, usize),
        frames: u32,
        tuning: &ResolveTuning,
    ) -> Vec<ParityFrame> {
        let mut previous: Carried = None;
        (0..frames)
            .map(|frame| {
                let current = render(self, output, render_size, frame, Vec2::ZERO, true);
                let out = {
                    let history = previous.as_ref().map(|(image, motion)| History { image, motion });
                    resolve(&current, history.as_ref(), output.0, output.1, tuning)
                };
                let frame_out = ParityFrame {
                    colour: current.colour.texels.clone(),
                    depth: current.depth.clone(),
                    out: out.texels.clone(),
                };
                previous = Some((out, current.motion));
                frame_out
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, settings: SequenceSettings) -> TemporalReport {
        let scene = scenario(name).expect("built-in scenario");
        let report = run_sequence(&scene, &settings);
        eprintln!("{name} {settings:?}: {report:?}");
        report
    }

    #[test]
    fn every_listed_scenario_exists_and_unknown_names_do_not() {
        for name in SCENARIOS {
            let scene = scenario(name).expect("listed scenario");
            assert_eq!(scene.name, name);
            assert!(!scene.layers.is_empty());
        }
        assert!(scenario("nope").is_none());
    }

    /// The model must use the shader's constants: a change to one without the other makes the
    /// metrics describe a resolve that does not ship.
    #[test]
    fn shipped_tuning_matches_the_shader_source() {
        let shader = include_str!("../shaders/taau.wgsl");
        let t = ResolveTuning::SHIPPED;
        assert_eq!(t.variance_gamma, 1.25);
        assert!(shader.contains("mean - 1.25 * sigma") && shader.contains("mean + 1.25 * sigma"));
        assert_eq!(t.fractional_penalty, 0.35);
        assert!(shader.contains("1.0 - 0.35 * max(between.x, between.y)"));
        assert_eq!((t.velocity_trust_px, t.velocity_reject_px), (0.5, 2.0));
        assert!(shader.contains("(disagree - 0.5) / 1.5"));
        assert!(shader.contains("const HISTORY_CUBIC_A: f32 = -1.0;"));
        assert!(shader.contains("const MAX_HDR: f32 = 6.0e4;"));
        assert_eq!(CLIP_EXTENT_FLOOR, 1.0 / 256.0);
        assert!(shader.contains("const CLIP_EXTENT_FLOOR: f32 = 1.0 / 256.0;"));
        assert!(shader.contains("let floor_extent = max(CLIP_EXTENT_FLOOR * hi.x, 1.0e-6);"));
        assert!(t.wide_motion_test);
        assert!(shader.contains("if (any(params.inv_size.zw > params.inv_size.xy * 1.0001))"));
        assert!(shader.contains("let base = vec2<i32>(floor(history_uv * vec2<f32>(psize) - 0.5));"));
        assert_eq!(t.history_weight, 0.9);
    }

    /// v0.1.29: the GPU keeps its history in half floats. A history that is the neighbourhood's
    /// own grey colour, rounded to half floats, lies inside the clip box: its chroma rounding
    /// (the box's chroma range is 0) does not count as the history lying outside, which pulled
    /// the whole colour toward the box centre every frame. A real chroma change still clips.
    #[test]
    fn half_float_rounding_of_a_grey_history_is_not_clipped() {
        // Round to a half float's 11 significant bits (normal range, ties up).
        let half = |v: f32| f32::from_bits((v.to_bits() + 0x1000) & !0x1fff);
        for v in [0.137f32, 0.41, 0.72, 0.9, 3.3] {
            let grey = Vec3::new(v, 0.9 * v, 0.8 * v);
            // A neighbourhood of one warm grey with a luma spread: its Cg range is 0 (to f32
            // rounding), as on the GPU parity test's wave wall.
            let (a, b) = (ycocg(grey * 0.8), ycocg(grey * 1.2));
            let (lo, hi) = (a.min(b), a.max(b));
            let history = Vec3::new(half(grey.x), half(grey.y), half(grey.z));
            let kept = rgb_from_ycocg(clip_to_box(ycocg(history), lo, hi));
            assert!(
                (kept - history).abs().max_element() < 1.0e-6,
                "grey {v}: the half-float history moved from {history} to {kept}"
            );
            // A history tinted green (Cg 3 % of luma off the box) is still pulled in.
            let tinted = history + Vec3::new(-0.02 * v, 0.04 * v, -0.02 * v);
            let pulled = rgb_from_ycocg(clip_to_box(ycocg(tinted), lo, hi));
            assert!(
                (pulled - tinted).abs().max_element() > 1.0e-3 * v,
                "grey {v}: a tinted history was not clipped"
            );
        }
    }

    /// A still view converges: the resolve of a static textured wall approaches the ground truth
    /// (the metrics are not dominated by a broken accumulation).
    #[test]
    fn a_still_view_converges_close_to_the_ground_truth() {
        let scene = TemporalScene {
            name: "still".into(),
            layers: vec![layer(
                0.9,
                Pattern::Waves {
                    period: 12.0,
                    lo: 0.2,
                    hi: 0.8,
                },
                Coverage::Everywhere,
                [0.0, 0.0],
                [0.0, 0.0],
            )],
        };
        let report = run_sequence(&scene, &SequenceSettings::default());
        assert_eq!(report.frames_measured, 16);
        assert_eq!(report.ghost_leak, 0.0, "no foreground, no leak");
        assert!(report.rms_error < 0.03, "{report:?}");
        assert!((report.peak_ratio - 1.0).abs() < 0.05, "{report:?}");
    }

    /// History rejection (numpy port of this model, 2026-10-21: |leak| ≤ 0.017 at 1.0 and 0.67):
    /// a mid-gray box inside the wall's color range (the clip cannot tell it from the wall)
    /// leaves no trail where it uncovers the wall, for a pan with parallax, a box crossing a held
    /// view and a box the camera follows, at the native and the 0.67 render scale.
    #[test]
    fn uncovered_background_keeps_no_foreground_trail() {
        for name in ["parallax_pan", "moving_object", "tracked_object"] {
            for render_scale in [1.0, 0.67] {
                let report = run(
                    name,
                    SequenceSettings {
                        render_scale,
                        ..SequenceSettings::default()
                    },
                );
                assert!(
                    report.disoccluded_pixels > 100,
                    "{name}: nothing uncovered: {report:?}"
                );
                assert!(
                    report.ghost_leak.abs() < 0.05,
                    "{name} at {render_scale}: {:.3} of the box's color left on the uncovered wall",
                    report.ghost_leak
                );
            }
        }
    }

    /// The second artifact fixed by this lane (ADR-1151): upscaling, the motion test read only
    /// the nearest render texel of last frame's motion and kept 10-15 % of the box's color on the
    /// uncovered wall (numpy port: 0.151 for `moving_object` at 0.67). The 2 × 2 test removes it.
    #[test]
    fn the_wide_motion_test_removes_the_upscaled_trail() {
        let narrow = run(
            "moving_object",
            SequenceSettings {
                render_scale: 0.67,
                tuning: ResolveTuning {
                    wide_motion_test: false,
                    ..ResolveTuning::SHIPPED
                },
                ..SequenceSettings::default()
            },
        );
        let wide = run(
            "moving_object",
            SequenceSettings {
                render_scale: 0.67,
                ..SequenceSettings::default()
            },
        );
        assert!(
            narrow.ghost_leak > 0.08,
            "the nearest-texel test should leave a trail: {narrow:?}"
        );
        assert!(wide.ghost_leak < 0.05, "{wide:?}");
        assert!(wide.rms_error < narrow.rms_error, "{wide:?} vs {narrow:?}");
    }

    /// At the native scale the wide test is off: the resolve is the one before ADR-1151.
    #[test]
    fn the_wide_motion_test_changes_nothing_at_native_scale() {
        for name in SCENARIOS {
            let narrow = run(
                name,
                SequenceSettings {
                    tuning: ResolveTuning {
                        wide_motion_test: false,
                        ..ResolveTuning::SHIPPED
                    },
                    ..SequenceSettings::default()
                },
            );
            assert_eq!(narrow, run(name, SequenceSettings::default()), "{name}");
        }
    }

    /// The leak metric is not vacuous: without the motion test (trust kept for any disagreement)
    /// and with a clip box too wide to reject, the box's trail shows (numpy port: 0.42).
    #[test]
    fn the_leak_metric_sees_a_trail_when_rejection_is_off() {
        let report = run(
            "moving_object",
            SequenceSettings {
                tuning: ResolveTuning {
                    variance_gamma: 100.0,
                    velocity_trust_px: 1.0e3,
                    velocity_reject_px: 2.0e3,
                    ..ResolveTuning::SHIPPED
                },
                ..SequenceSettings::default()
            },
        );
        assert!(report.ghost_leak > 0.1, "{report:?}");
    }

    /// The worst artifact found by this lane (ADR-1151): with the sky written as motionless, a
    /// turning camera fetched the sky's history where it had been and stars faded to a few
    /// percent of their brightness (numpy port: peak ratio 0.061). With the sky's rotation they
    /// keep what a still camera keeps (0.507 turning, 0.501 still).
    #[test]
    fn stars_keep_their_brightness_while_the_camera_turns() {
        let fixed = run("stars_turning", SequenceSettings::default());
        let before = run(
            "stars_turning",
            SequenceSettings {
                sky_motion: false,
                ..SequenceSettings::default()
            },
        );
        let mut still = scenario("stars_turning").expect("scenario");
        for l in &mut still.layers {
            l.velocity = [0.0, 0.0];
        }
        let still = run_sequence(&still, &SequenceSettings::default());
        assert!(
            before.peak_ratio < 0.15,
            "the old sky motion should fade stars: {before:?}"
        );
        assert!(
            fixed.peak_ratio > 0.4,
            "stars fade with the sky's motion: {fixed:?}"
        );
        assert!(
            fixed.peak_ratio > 0.9 * still.peak_ratio,
            "turning {fixed:?} vs still {still:?}"
        );
        assert!(fixed.rms_error < before.rms_error, "{fixed:?} vs {before:?}");
    }

    /// An HDR object over a dark wall leaves no trail, and the frame's features keep their level.
    #[test]
    fn bright_objects_leave_no_trail() {
        for render_scale in [1.0, 0.67] {
            let report = run(
                "bright_object",
                SequenceSettings {
                    render_scale,
                    ..SequenceSettings::default()
                },
            );
            assert!(report.ghost_leak.abs() < 0.05, "{report:?}");
            assert!(report.peak_ratio > 0.8, "{report:?}");
        }
    }

    #[test]
    fn settings_are_clamped() {
        let scene = scenario("parallax_pan").expect("scenario");
        let report = run_sequence(
            &scene,
            &SequenceSettings {
                width: 1,
                height: 100_000,
                render_scale: f32::NAN,
                frames: 0,
                warmup: 50,
                ..SequenceSettings::default()
            },
        );
        assert_eq!(report.frames_measured, 1);
        assert!(report.rms_error.is_finite() && report.peak_ratio.is_finite());
    }
}
