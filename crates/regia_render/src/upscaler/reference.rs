// SPDX-License-Identifier: MPL-2.0
//! The software reference upscaler (ADR-510): the [`Upscaler`] contract run on the CPU, so the
//! conventions every backend must agree on (jitter, motion vectors, the reactive mask, history
//! reset) are tested without a GPU.
//!
//! Per display pixel it:
//!
//! 1. Places the pixel centre in render pixels and moves it by the frame's jitter, converted
//!    from Regia Engine's NDC into render pixels ([`InputConventions::jitter_from_regia`]), to find
//!    where the jittered colour image shows that point.
//! 2. Filters the colour there: [`SpatialFilter::Bilinear`], or [`SpatialFilter::EdgeAdaptive`],
//!    a 16-tap Lanczos-2 kernel stretched along the local luma edge and clamped to the four
//!    nearest texels so it cannot ring (in the spirit of AMD's public FidelityFX Super
//!    Resolution 1 description; original code, `docs/provenance.md`).
//! 3. With temporal accumulation on: reprojects the history with the motion vector (Regia Engine's
//!    `previous uv − current uv`, converted to render pixels; with depth, the nearest surface's
//!    vector in the 3×3 neighbourhood), clamps it to the colour range of the 3×3 render
//!    neighbourhood, and blends in the nearest render sample with a Gaussian weight on its
//!    distance from the pixel centre. The jitter moves that sample around from frame to frame,
//!    which is what turns accumulation into supersampling. The accumulated weight is capped at
//!    [`ReferenceSettings::max_history`] × (1 − reactive): a reactive value of 1 drops the
//!    history and the pixel shows this frame's filtered colour. Reset, a size change and a
//!    reprojection off screen drop it too.
//! 4. Optionally sharpens (`UpscaleParams::sharpness`): an unsharp mask on the 4-neighbour
//!    cross, clamped to the cross's own range so it cannot overshoot (contrast-adaptive
//!    sharpening in the same spirit; original code).
//!
//! The reference is for tests and for the agent's `render.reference_upscale` check, not for
//! shipping frames: it is single-threaded and allocates per call.

use super::UpscaleQuality;
use super::plugin::{
    CpuTarget, CpuUpscaleContext, InputContract, InputConventions, InputNeed, UpscaleError, UpscaleFrame,
    UpscaleParams, Upscaler,
};

/// An RGBA `f32` image on the CPU, row-major, top row first.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuImage {
    width: u32,
    height: u32,
    texels: Vec<[f32; 4]>,
}

impl CpuImage {
    /// A `width` × `height` image of zeros.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            texels: vec![[0.0; 4]; width as usize * height as usize],
        }
    }

    /// An image whose texel at `(x, y)` is `f(x, y)`.
    #[must_use]
    pub fn from_fn(width: u32, height: u32, mut f: impl FnMut(u32, u32) -> [f32; 4]) -> Self {
        let mut texels = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height {
            for x in 0..width {
                texels.push(f(x, y));
            }
        }
        Self {
            width,
            height,
            texels,
        }
    }

    /// Width in texels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in texels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// `(width, height)`.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Every texel, row-major.
    #[must_use]
    pub fn texels(&self) -> &[[f32; 4]] {
        &self.texels
    }

    /// The texel at `(x, y)`, clamped to the edge (zeros for an empty image).
    #[must_use]
    pub fn texel(&self, x: u32, y: u32) -> [f32; 4] {
        self.load(i64::from(x), i64::from(y))
    }

    /// Writes the texel at `(x, y)`; outside the image it does nothing.
    pub fn set(&mut self, x: u32, y: u32, value: [f32; 4]) {
        if x < self.width && y < self.height {
            let index = y as usize * self.width as usize + x as usize;
            self.texels[index] = value;
        }
    }

    fn load(&self, x: i64, y: i64) -> [f32; 4] {
        if self.texels.is_empty() {
            return [0.0; 4];
        }
        let x = x.clamp(0, i64::from(self.width) - 1) as usize;
        let y = y.clamp(0, i64::from(self.height) - 1) as usize;
        self.texels[y * self.width as usize + x]
    }

    /// Bilinear sample at a position in pixels (texel centres at `i + 0.5`), clamped to the
    /// edge.
    #[must_use]
    pub fn sample_bilinear(&self, px: f32, py: f32) -> [f32; 4] {
        let x = px - 0.5;
        let y = py - 0.5;
        let fx0 = x.floor();
        let fy0 = y.floor();
        let (fx, fy) = (x - fx0, y - fy0);
        let (i, j) = (fx0 as i64, fy0 as i64);
        let top = lerp4(self.load(i, j), self.load(i + 1, j), fx);
        let bottom = lerp4(self.load(i, j + 1), self.load(i + 1, j + 1), fx);
        lerp4(top, bottom, fy)
    }
}

fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

fn min4(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2]), a[3].min(b[3])]
}

fn max4(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

fn clamp4(v: [f32; 4], lo: [f32; 4], hi: [f32; 4]) -> [f32; 4] {
    max4(min4(v, hi), lo)
}

fn luma(c: [f32; 4]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Root-mean-square difference of the RGB channels of two images of the same size; `None` when
/// the sizes differ or the images are empty.
#[must_use]
pub fn rmse(a: &CpuImage, b: &CpuImage) -> Option<f32> {
    if a.size() != b.size() || a.texels.is_empty() {
        return None;
    }
    let sum: f64 = a
        .texels
        .iter()
        .zip(&b.texels)
        .map(|(p, q)| {
            (0..3)
                .map(|c| {
                    let d = f64::from(p[c] - q[c]);
                    d * d
                })
                .sum::<f64>()
        })
        .sum();
    Some((sum / (a.texels.len() as f64 * 3.0)).sqrt() as f32)
}

/// The reference's spatial filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialFilter {
    /// Bilinear between the four nearest texels.
    Bilinear,
    /// 16-tap Lanczos-2 stretched along the local edge, clamped to the four nearest texels.
    EdgeAdaptive,
}

impl SpatialFilter {
    /// Both filters.
    pub const ALL: [Self; 2] = [Self::Bilinear, Self::EdgeAdaptive];

    /// Stable snake_case name: `bilinear`, `edge_adaptive`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bilinear => "bilinear",
            Self::EdgeAdaptive => "edge_adaptive",
        }
    }

    /// Parses [`Self::name`] (also `fsr1`, `easu` for the edge-adaptive filter); case, `_`, `-`
    /// and spaces are ignored.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match super::normalized(name).as_str() {
            "bilinear" => Some(Self::Bilinear),
            "edgeadaptive" | "fsr1" | "easu" => Some(Self::EdgeAdaptive),
            _ => None,
        }
    }

    /// Filters `color` at a position in render pixels.
    #[must_use]
    pub fn sample(self, color: &CpuImage, px: f32, py: f32) -> [f32; 4] {
        match self {
            Self::Bilinear => color.sample_bilinear(px, py),
            Self::EdgeAdaptive => edge_adaptive(color, px, py),
        }
    }
}

fn sinc(x: f32) -> f32 {
    if x.abs() < 1.0e-6 {
        1.0
    } else {
        let px = std::f32::consts::PI * x;
        px.sin() / px
    }
}

/// Lanczos-2: `sinc(x) · sinc(x / 2)` inside |x| < 2.
fn lanczos2(x: f32) -> f32 {
    let x = x.abs();
    if x < 2.0 { sinc(x) * sinc(x * 0.5) } else { 0.0 }
}

/// The edge-adaptive filter at `(px, py)` in render pixels.
fn edge_adaptive(color: &CpuImage, px: f32, py: f32) -> [f32; 4] {
    let x = px - 0.5;
    let y = py - 0.5;
    let fx0 = x.floor();
    let fy0 = y.floor();
    let (fx, fy) = (x - fx0, y - fy0);
    let (i, j) = (fx0 as i64, fy0 as i64);
    let l = |a: i64, b: i64| luma(color.load(a, b));

    // Luma gradient: central differences at the four inner texels, blended bilinearly.
    let mut gx = 0.0;
    let mut gy = 0.0;
    for (oi, oj, w) in [
        (0, 0, (1.0 - fx) * (1.0 - fy)),
        (1, 0, fx * (1.0 - fy)),
        (0, 1, (1.0 - fx) * fy),
        (1, 1, fx * fy),
    ] {
        let (a, b) = (i + oi, j + oj);
        gx += w * (l(a + 1, b) - l(a - 1, b)) * 0.5;
        gy += w * (l(a, b + 1) - l(a, b - 1)) * 0.5;
    }
    let g2 = gx * gx + gy * gy;
    let (nx, ny, anisotropy) = if g2 > 1.0e-12 {
        let g = g2.sqrt();
        (gx / g, gy / g, (g * 4.0).min(1.0))
    } else {
        (1.0, 0.0, 0.0)
    };
    // Distances along the edge shrink, so the kernel reaches further along it than across it.
    let stretch = 1.0 / (1.0 + anisotropy);

    let mut sum = [0.0f32; 4];
    let mut weight = 0.0f32;
    for oj in -1..3 {
        for oi in -1..3 {
            let dx = (i + oi) as f32 - x;
            let dy = (j + oj) as f32 - y;
            let across = dx * nx + dy * ny;
            let along = (-dx * ny + dy * nx) * stretch;
            let w = lanczos2((across * across + along * along).sqrt());
            let c = color.load(i + oi, j + oj);
            for (s, v) in sum.iter_mut().zip(c) {
                *s += w * v;
            }
            weight += w;
        }
    }
    let inner = [
        color.load(i, j),
        color.load(i + 1, j),
        color.load(i, j + 1),
        color.load(i + 1, j + 1),
    ];
    let lo = inner.into_iter().fold([f32::INFINITY; 4], min4);
    let hi = inner.into_iter().fold([f32::NEG_INFINITY; 4], max4);
    if weight.abs() < 1.0e-6 {
        return inner[0];
    }
    let filtered = sum.map(|s| s / weight);
    clamp4(filtered, lo, hi)
}

/// Clamped unsharp mask on the 4-neighbour cross, `strength` 0–1.
#[must_use]
pub fn sharpen(image: &CpuImage, strength: f32) -> CpuImage {
    let strength = strength.clamp(0.0, 1.0);
    CpuImage::from_fn(image.width, image.height, |x, y| {
        let (x, y) = (i64::from(x), i64::from(y));
        let c = image.load(x, y);
        let cross = [
            image.load(x, y - 1),
            image.load(x, y + 1),
            image.load(x - 1, y),
            image.load(x + 1, y),
        ];
        let lo = cross.into_iter().fold(c, min4);
        let hi = cross.into_iter().fold(c, max4);
        let mut out = c;
        for (ch, o) in out.iter_mut().enumerate().take(3) {
            let ring: f32 = cross.iter().map(|n| n[ch]).sum();
            *o = c[ch] + strength * 0.25 * (4.0 * c[ch] - ring);
        }
        clamp4(out, lo, hi)
    })
}

/// Settings of the software reference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceSettings {
    /// The spatial filter.
    pub filter: SpatialFilter,
    /// Accumulate over frames (off: every frame is the spatial filter alone).
    pub temporal: bool,
    /// Cap of the accumulated sample weight, roughly the number of frames the history
    /// remembers (16).
    pub max_history: f32,
}

impl Default for ReferenceSettings {
    fn default() -> Self {
        Self {
            filter: SpatialFilter::EdgeAdaptive,
            temporal: true,
            max_history: 16.0,
        }
    }
}

/// Spread of the Gaussian that weights a render sample by its distance from the display pixel
/// centre, in render pixels.
const SAMPLE_SIGMA: f32 = 0.5;

/// The software reference upscaler (see the module docs).
#[derive(Debug, Clone)]
pub struct ReferenceUpscaler {
    settings: ReferenceSettings,
    /// RGB plus the accumulated weight in alpha, at the display size; empty before the first
    /// frame.
    history: CpuImage,
}

impl ReferenceUpscaler {
    /// A reference upscaler with no history.
    #[must_use]
    pub fn new(settings: ReferenceSettings) -> Self {
        Self {
            settings,
            history: CpuImage::new(0, 0),
        }
    }

    /// Its settings.
    #[must_use]
    pub const fn settings(&self) -> &ReferenceSettings {
        &self.settings
    }

    /// The accumulated weight at a display pixel (0 with no history).
    #[must_use]
    pub fn history_weight(&self, x: u32, y: u32) -> f32 {
        if self.history.texels.is_empty() {
            0.0
        } else {
            self.history.texel(x, y)[3]
        }
    }

    /// Upscales `frame` and returns the output (the [`Upscaler`] impl writes it into the
    /// context instead).
    ///
    /// # Errors
    ///
    /// The frame fails [`UpscaleFrame::check`], or an image is not the render size
    /// ([`UpscaleError::SizeMismatch`]).
    pub fn run(&mut self, frame: &UpscaleFrame<'_, CpuImage>) -> Result<CpuImage, UpscaleError> {
        let _span = tracing::info_span!("reference_upscale").entered();
        let _crumb = crate::breadcrumbs::frame_pass("reference_upscale");
        frame.check(&self.contract_for())?;
        let p = frame.params;
        for (input, image) in [
            ("color", Some(frame.color)),
            ("depth", frame.depth),
            ("motion", frame.motion),
            ("reactive", frame.reactive),
            ("transparency", frame.transparency),
        ] {
            if let Some(image) = image
                && image.size() != p.render_size
            {
                return Err(UpscaleError::SizeMismatch {
                    input,
                    got: image.size(),
                    expected: p.render_size,
                });
            }
        }
        let out = self.accumulate(frame, &p);
        if self.settings.temporal {
            self.history = out.clone();
        }
        let out = if p.sharpness > 0.0 {
            sharpen(&out, p.sharpness)
        } else {
            out
        };
        // The output carries alpha 1, not the history weight.
        Ok(CpuImage::from_fn(out.width, out.height, |x, y| {
            let t = out.texel(x, y);
            [t[0], t[1], t[2], 1.0]
        }))
    }

    fn contract_for(&self) -> InputContract {
        InputContract {
            depth: InputNeed::Optional,
            motion: InputNeed::Optional,
            reactive: InputNeed::Optional,
            transparency: InputNeed::Optional,
            conventions: InputConventions::RENDER_PIXELS,
            vulkan_device_hook: false,
            max_ratio_permille: 3000,
        }
    }

    fn accumulate(&self, frame: &UpscaleFrame<'_, CpuImage>, p: &UpscaleParams) -> CpuImage {
        let conventions = InputConventions::RENDER_PIXELS;
        let (rw, rh) = (p.render_size.0 as f32, p.render_size.1 as f32);
        let (dw, dh) = (p.display_size.0 as f32, p.display_size.1 as f32);
        let jitter = conventions.jitter_from_regia(p.jitter, p.render_size);
        let history_valid = self.settings.temporal && !p.reset && self.history.size() == p.display_size;
        let history_scale = if p.history_scale > 0.0 {
            p.history_scale
        } else {
            1.0
        };
        let color = frame.color;
        let max_history = self.settings.max_history.max(0.0);
        CpuImage::from_fn(p.display_size.0, p.display_size.1, |ox, oy| {
            // The display pixel centre in render pixels, and where the jittered image shows it.
            let prx = (ox as f32 + 0.5) / dw * rw;
            let pry = (oy as f32 + 0.5) / dh * rh;
            let (ix, iy) = (prx + jitter[0], pry + jitter[1]);
            let spatial = self.settings.filter.sample(color, ix, iy);
            if !self.settings.temporal {
                return [spatial[0], spatial[1], spatial[2], 1.0];
            }
            let ti = (ix.floor() as i64).clamp(0, i64::from(p.render_size.0) - 1);
            let tj = (iy.floor() as i64).clamp(0, i64::from(p.render_size.1) - 1);
            // The scene point that texel sampled, and its distance from the pixel centre.
            let qx = ti as f32 + 0.5 - jitter[0];
            let qy = tj as f32 + 0.5 - jitter[1];
            let d2 = (qx - prx) * (qx - prx) + (qy - pry) * (qy - pry);
            let w = (-d2 / (2.0 * SAMPLE_SIGMA * SAMPLE_SIGMA)).exp();
            let sample = color.load(ti, tj);
            let reactive = frame
                .reactive
                .map_or(0.0, |m| m.load(ti, tj)[0])
                .max(frame.transparency.map_or(0.0, |m| m.load(ti, tj)[0]))
                .clamp(0.0, 1.0);

            let mut count = 0.0f32;
            let mut history = [0.0f32; 4];
            if history_valid {
                let (mx, my) = match frame.motion {
                    Some(motion) => {
                        let (si, sj) = nearest_surface(frame.depth, ti, tj);
                        let m = motion.load(si, sj);
                        let px = conventions.motion_from_regia([m[0], m[1]], p.render_size);
                        (px[0], px[1])
                    }
                    None => (0.0, 0.0),
                };
                let hx = (prx + mx) / rw;
                let hy = (pry + my) / rh;
                if (0.0..=1.0).contains(&hx) && (0.0..=1.0).contains(&hy) {
                    let h = self.history.sample_bilinear(hx * dw, hy * dh);
                    let mut lo = [f32::INFINITY; 4];
                    let mut hi = [f32::NEG_INFINITY; 4];
                    for b in -1..=1 {
                        for a in -1..=1 {
                            let n = color.load(ti + a, tj + b);
                            lo = min4(lo, n);
                            hi = max4(hi, n);
                        }
                    }
                    let scaled = [
                        h[0] * history_scale,
                        h[1] * history_scale,
                        h[2] * history_scale,
                        0.0,
                    ];
                    history = clamp4(scaled, lo, hi);
                    count = h[3];
                }
            }
            let cap = max_history * (1.0 - reactive);
            let count = count.min(cap);
            if count <= 0.0 {
                [spatial[0], spatial[1], spatial[2], 1.0]
            } else {
                let total = count + w;
                let mut out = [0.0f32; 4];
                for ((o, h), s) in out.iter_mut().zip(history).zip(sample).take(3) {
                    *o = (h * count + s * w) / total;
                }
                out[3] = total.min(cap.max(1.0));
                out
            }
        })
    }
}

/// The texel in the 3×3 neighbourhood of `(ti, tj)` nearest the camera (largest reverse-Z
/// depth); `(ti, tj)` itself without depth. Thin foreground edges then carry their own motion.
fn nearest_surface(depth: Option<&CpuImage>, ti: i64, tj: i64) -> (i64, i64) {
    let Some(depth) = depth else {
        return (ti, tj);
    };
    let mut best = (ti, tj);
    let mut best_depth = depth.load(ti, tj)[0];
    for b in -1..=1 {
        for a in -1..=1 {
            let (x, y) = (
                (ti + a).clamp(0, i64::from(depth.width) - 1),
                (tj + b).clamp(0, i64::from(depth.height) - 1),
            );
            let d = depth.load(x, y)[0];
            if d > best_depth {
                best_depth = d;
                best = (x, y);
            }
        }
    }
    best
}

impl Upscaler<CpuTarget> for ReferenceUpscaler {
    fn name(&self) -> &'static str {
        match self.settings.filter {
            SpatialFilter::Bilinear => "reference_bilinear",
            SpatialFilter::EdgeAdaptive => "reference_edge_adaptive",
        }
    }

    fn contract(&self) -> InputContract {
        self.contract_for()
    }

    fn upscale(
        &mut self,
        ctx: &mut CpuUpscaleContext<'_>,
        frame: &UpscaleFrame<'_, CpuImage>,
    ) -> Result<(), UpscaleError> {
        *ctx.output = self.run(frame)?;
        Ok(())
    }
}

/// The synthetic scene of [`run_check`]: a disc (0.7) over diagonal stripes (0.3) on black, in
/// UV, grey. Thin features and curved edges alias at a low render size, so supersampling and
/// motion errors show in the RMSE.
#[must_use]
pub fn check_scene(u: f32, v: f32) -> f32 {
    let (cx, cy) = (u - 0.5, v - 0.5);
    let disc = if cx * cx + cy * cy < 0.09 { 0.7 } else { 0.0 };
    let band = ((u + 0.37 * v) * 9.0).floor().rem_euclid(2.0);
    let stripe = if band < 0.5 { 0.3 } else { 0.0 };
    disc + stripe
}

/// Renders `scene` the way the frame does: each render texel centre sees the scene at its
/// position minus the jitter, with the scene moved by `shift` (UV).
#[must_use]
pub fn render_scene(
    scene: impl Fn(f32, f32) -> f32,
    render_size: (u32, u32),
    jitter_ndc: [f32; 2],
    shift: [f32; 2],
) -> CpuImage {
    let (rw, rh) = (render_size.0 as f32, render_size.1 as f32);
    let jitter = InputConventions::RENDER_PIXELS.jitter_from_regia(jitter_ndc, render_size);
    CpuImage::from_fn(render_size.0, render_size.1, |i, j| {
        let u = (i as f32 + 0.5 - jitter[0]) / rw - shift[0];
        let v = (j as f32 + 0.5 - jitter[1]) / rh - shift[1];
        let s = scene(u, v);
        [s, s, s, 1.0]
    })
}

/// The ground truth of `scene` at a display size: a box filter of `supersample`² samples per
/// pixel, with the scene moved by `shift` (UV).
#[must_use]
pub fn scene_truth(
    scene: impl Fn(f32, f32) -> f32,
    display_size: (u32, u32),
    shift: [f32; 2],
    supersample: u32,
) -> CpuImage {
    let n = supersample.max(1);
    let (dw, dh) = (display_size.0 as f32, display_size.1 as f32);
    CpuImage::from_fn(display_size.0, display_size.1, |x, y| {
        let mut acc = 0.0;
        for sy in 0..n {
            for sx in 0..n {
                let u = (x as f32 + (sx as f32 + 0.5) / n as f32) / dw - shift[0];
                let v = (y as f32 + (sy as f32 + 0.5) / n as f32) / dh - shift[1];
                acc += scene(u, v);
            }
        }
        let s = acc / (n * n) as f32;
        [s, s, s, 1.0]
    })
}

/// What [`run_check`] gives the motion-vector input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionVectorMode {
    /// The true motion, in Regia Engine's convention.
    Correct,
    /// No motion vectors (the history smears under motion).
    Zero,
    /// The true motion with the sign flipped (a convention mistake).
    Negated,
}

impl MotionVectorMode {
    /// Stable snake_case name: `correct`, `zero`, `negated`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Correct => "correct",
            Self::Zero => "zero",
            Self::Negated => "negated",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match super::normalized(name).as_str() {
            "correct" => Some(Self::Correct),
            "zero" | "none" => Some(Self::Zero),
            "negated" => Some(Self::Negated),
            _ => None,
        }
    }
}

/// A run of the reference on [`check_scene`] (the `render.reference_upscale` agent check).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceCheck {
    /// The spatial filter.
    pub filter: SpatialFilter,
    /// The quality mode (sets the render size from the display size).
    pub quality: UpscaleQuality,
    /// Display size.
    pub display_size: (u32, u32),
    /// Frames to run (Halton jitter, frame indices 1..=frames).
    pub frames: u32,
    /// Scene motion per frame, in display pixels.
    pub motion_px: [f32; 2],
    /// What the motion-vector input carries.
    pub motion_vectors: MotionVectorMode,
    /// Mark the whole frame reactive (1.0): no history is kept.
    pub reactive: bool,
    /// Post-upscale sharpening, 0–1.
    pub sharpness: f32,
}

impl Default for ReferenceCheck {
    fn default() -> Self {
        Self {
            filter: SpatialFilter::EdgeAdaptive,
            quality: UpscaleQuality::Performance,
            display_size: (64, 64),
            frames: 32,
            motion_px: [0.0, 0.0],
            motion_vectors: MotionVectorMode::Correct,
            reactive: false,
            sharpness: 0.0,
        }
    }
}

/// What [`run_check`] measured against [`scene_truth`] (8 × 8 supersampling).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceReport {
    /// The render size the quality mode gave.
    pub render_size: (u32, u32),
    /// Frames run.
    pub frames: u32,
    /// RMSE of the first frame's output.
    pub first_frame_rmse: f32,
    /// RMSE of the last frame's output.
    pub final_rmse: f32,
    /// RMSE of the spatial filter alone on the last frame (no history): what accumulation is
    /// measured against.
    pub spatial_only_rmse: f32,
}

/// Runs the reference on the synthetic scene and measures it.
///
/// # Errors
///
/// The sizes are invalid for the mode ([`UpscaleError::InvalidSize`],
/// [`UpscaleError::UnsupportedRatio`]).
pub fn run_check(check: &ReferenceCheck) -> Result<ReferenceReport, UpscaleError> {
    let display = check.display_size;
    let render = check.quality.render_size(display);
    let frames = check.frames.max(1);
    let velocity = [
        check.motion_px[0] / display.0.max(1) as f32,
        check.motion_px[1] / display.1.max(1) as f32,
    ];
    let mv = match check.motion_vectors {
        // A point at uv now was at uv − velocity last frame: previous − current = −velocity.
        MotionVectorMode::Correct => [-velocity[0], -velocity[1]],
        MotionVectorMode::Zero => [0.0, 0.0],
        MotionVectorMode::Negated => velocity,
    };
    let motion = CpuImage::from_fn(render.0, render.1, |_, _| [mv[0], mv[1], 0.0, 1.0]);
    let reactive = CpuImage::from_fn(render.0, render.1, |_, _| [1.0, 0.0, 0.0, 1.0]);
    let settings = ReferenceSettings {
        filter: check.filter,
        ..ReferenceSettings::default()
    };
    let mut upscaler = ReferenceUpscaler::new(settings);
    let mut spatial = ReferenceUpscaler::new(ReferenceSettings {
        temporal: false,
        ..settings
    });
    let mut first = None;
    let mut last = None;
    for index in 1..=frames {
        let jitter = crate::taau::halton_jitter(u64::from(index), render.0, render.1);
        let shift = [velocity[0] * index as f32, velocity[1] * index as f32];
        let color = render_scene(check_scene, render, jitter, shift);
        let frame = UpscaleFrame {
            color: &color,
            depth: None,
            motion: Some(&motion),
            reactive: check.reactive.then_some(&reactive),
            transparency: None,
            params: UpscaleParams {
                jitter,
                reset: index == 1,
                sharpness: check.sharpness,
                ..UpscaleParams::new(render, display)
            },
        };
        let out = upscaler.run(&frame)?;
        // Only the first and the last frame are measured.
        if index != 1 && index != frames {
            continue;
        }
        let truth = scene_truth(check_scene, display, shift, 8);
        let error = rmse(&out, &truth).unwrap_or(f32::NAN);
        if index == 1 {
            first = Some(error);
        }
        if index == frames {
            let alone = spatial.run(&frame)?;
            last = Some((error, rmse(&alone, &truth).unwrap_or(f32::NAN)));
        }
    }
    let (final_rmse, spatial_only_rmse) = last.unwrap_or((f32::NAN, f32::NAN));
    Ok(ReferenceReport {
        render_size: render,
        frames,
        first_frame_rmse: first.unwrap_or(f32::NAN),
        final_rmse,
        spatial_only_rmse,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grey(v: f32) -> [f32; 4] {
        [v, v, v, 1.0]
    }

    fn spatial_only(filter: SpatialFilter) -> ReferenceUpscaler {
        ReferenceUpscaler::new(ReferenceSettings {
            filter,
            temporal: false,
            ..ReferenceSettings::default()
        })
    }

    fn frame_of(color: &CpuImage, display: (u32, u32), jitter: [f32; 2]) -> UpscaleFrame<'_, CpuImage> {
        UpscaleFrame {
            color,
            depth: None,
            motion: None,
            reactive: None,
            transparency: None,
            params: UpscaleParams {
                jitter,
                ..UpscaleParams::new(color.size(), display)
            },
        }
    }

    fn step_scene(u: f32, v: f32) -> f32 {
        if u + 0.3 * v < 0.52 { 1.0 } else { 0.0 }
    }

    /// Display pixels strictly inside an edge's ramp (between 5 % and 95 %).
    fn ramp_pixels(image: &CpuImage) -> usize {
        image
            .texels()
            .iter()
            .filter(|t| t[0] > 0.05 && t[0] < 0.95)
            .count()
    }

    #[test]
    fn bilinear_is_exact_at_texel_centres_midpoints_and_edges() {
        let image = CpuImage::from_fn(2, 2, |x, y| grey((x + 2 * y) as f32));
        assert_eq!(image.sample_bilinear(0.5, 0.5), grey(0.0));
        assert_eq!(image.sample_bilinear(1.5, 1.5), grey(3.0));
        assert_eq!(image.sample_bilinear(1.0, 0.5)[0], 0.5);
        assert_eq!(image.sample_bilinear(1.0, 1.0)[0], 1.5);
        // Clamped to the edge outside the image.
        assert_eq!(image.sample_bilinear(-4.0, 0.5), grey(0.0));
        assert_eq!(image.texel(9, 9), grey(3.0));
        assert_eq!(CpuImage::new(0, 0).sample_bilinear(0.5, 0.5), [0.0; 4]);
    }

    #[test]
    fn a_2x_bilinear_upscale_matches_the_hand_computed_row() -> Result<(), UpscaleError> {
        // Render 2×2 (0 left, 1 right) to 4×4: display centres sit at 0.25, 0.75, 1.25 and 1.75
        // render pixels, so the row reads 0, 0.25, 0.75, 1 (the ends clamp).
        let color = CpuImage::from_fn(2, 2, |x, _| grey(x as f32));
        let out = spatial_only(SpatialFilter::Bilinear).run(&frame_of(&color, (4, 4), [0.0, 0.0]))?;
        assert_eq!(out.size(), (4, 4));
        let row: Vec<f32> = (0..4).map(|x| out.texel(x, 1)[0]).collect();
        for (got, want) in row.iter().zip([0.0, 0.25, 0.75, 1.0]) {
            assert!((got - want).abs() < 1.0e-6, "{row:?}");
        }
        // A jitter of half a render pixel to the right (1 / width in NDC) moves the image right,
        // so each scene point is found half a texel further right in it.
        let jitter = [1.0 / 2.0, 0.0];
        let out = spatial_only(SpatialFilter::Bilinear).run(&frame_of(&color, (4, 4), jitter))?;
        let row: Vec<f32> = (0..4).map(|x| out.texel(x, 1)[0]).collect();
        for (got, want) in row.iter().zip([0.25, 0.75, 1.0, 1.0]) {
            assert!((got - want).abs() < 1.0e-6, "{row:?}");
        }
        Ok(())
    }

    #[test]
    fn the_reference_runs_through_the_boundary() -> Result<(), UpscaleError> {
        let color = render_scene(check_scene, (32, 32), [0.0, 0.0], [0.0, 0.0]);
        let mut output = CpuImage::new(1, 1);
        let mut upscaler = ReferenceUpscaler::new(ReferenceSettings::default());
        let backend: &mut dyn Upscaler<CpuTarget> = &mut upscaler;
        assert_eq!(backend.name(), "reference_edge_adaptive");
        assert_eq!(backend.contract().conventions, InputConventions::RENDER_PIXELS);
        let frame = frame_of(&color, (64, 64), [0.0, 0.0]);
        frame.check(&backend.contract())?;
        backend.upscale(&mut CpuUpscaleContext { output: &mut output }, &frame)?;
        assert_eq!(output.size(), (64, 64));
        assert!(
            output.texels().iter().all(|t| t[3] == 1.0),
            "alpha is 1, not the weight"
        );
        // A wrong-size input is refused with its name.
        let small = CpuImage::new(16, 16);
        let bad = UpscaleFrame {
            reactive: Some(&small),
            ..frame_of(&color, (64, 64), [0.0, 0.0])
        };
        let error = backend
            .upscale(&mut CpuUpscaleContext { output: &mut output }, &bad)
            .err();
        assert_eq!(
            error,
            Some(UpscaleError::SizeMismatch {
                input: "reactive",
                got: (16, 16),
                expected: (32, 32)
            })
        );
        assert_eq!(error.map(|e| e.code()), Some("size_mismatch"));
        Ok(())
    }

    #[test]
    fn jittered_accumulation_converges_toward_the_supersampled_truth() -> Result<(), UpscaleError> {
        for filter in SpatialFilter::ALL {
            let report = run_check(&ReferenceCheck {
                filter,
                ..ReferenceCheck::default()
            })?;
            assert_eq!(report.render_size, (32, 32), "Performance at 64×64");
            // Measured on the prototype: bilinear 0.067 → 0.035, edge-adaptive 0.054 → 0.035.
            assert!(
                report.final_rmse < 0.75 * report.spatial_only_rmse,
                "{filter:?}: {report:?}"
            );
            assert!(
                report.final_rmse < 0.75 * report.first_frame_rmse,
                "{filter:?}: {report:?}"
            );
        }
        // With one frame, the first and the final measurement are the same frame.
        let still = run_check(&ReferenceCheck {
            frames: 1,
            ..ReferenceCheck::default()
        })?;
        assert_eq!(still.first_frame_rmse.to_bits(), still.final_rmse.to_bits());
        Ok(())
    }

    #[test]
    fn the_edge_adaptive_filter_beats_bilinear_and_never_rings() -> Result<(), UpscaleError> {
        let one = |filter| {
            run_check(&ReferenceCheck {
                filter,
                frames: 1,
                ..ReferenceCheck::default()
            })
        };
        let bilinear = one(SpatialFilter::Bilinear)?;
        let edge = one(SpatialFilter::EdgeAdaptive)?;
        // Prototype: 0.067 vs 0.054.
        assert!(
            edge.spatial_only_rmse < 0.9 * bilinear.spatial_only_rmse,
            "{edge:?} vs {bilinear:?}"
        );
        // A hard diagonal edge: a narrower ramp, a smaller error, nothing outside 0–1.
        let color = render_scene(step_scene, (32, 32), [0.0, 0.0], [0.0, 0.0]);
        let truth = scene_truth(step_scene, (64, 64), [0.0, 0.0], 8);
        let b = spatial_only(SpatialFilter::Bilinear).run(&frame_of(&color, (64, 64), [0.0, 0.0]))?;
        let e = spatial_only(SpatialFilter::EdgeAdaptive).run(&frame_of(&color, (64, 64), [0.0, 0.0]))?;
        // Prototype: ramps 164 vs 128 pixels, RMSE 0.052 vs 0.037.
        assert!(
            ramp_pixels(&e) < ramp_pixels(&b),
            "{} vs {}",
            ramp_pixels(&e),
            ramp_pixels(&b)
        );
        let (re, rb) = (rmse(&e, &truth), rmse(&b, &truth));
        assert!(
            matches!((re, rb), (Some(re), Some(rb)) if re < 0.85 * rb),
            "{re:?} vs {rb:?}"
        );
        assert!(
            e.texels().iter().all(|t| (0.0..=1.0).contains(&t[0])),
            "the clamp keeps Lanczos lobes inside the texel range"
        );
        Ok(())
    }

    #[test]
    fn motion_vectors_in_regia_convention_keep_moving_content_registered() -> Result<(), UpscaleError> {
        let run = |motion_vectors| {
            run_check(&ReferenceCheck {
                frames: 24,
                motion_px: [1.5, 0.5],
                motion_vectors,
                ..ReferenceCheck::default()
            })
        };
        let correct = run(MotionVectorMode::Correct)?;
        let zero = run(MotionVectorMode::Zero)?;
        let negated = run(MotionVectorMode::Negated)?;
        // Prototype: 0.064 correct, 0.182 without vectors, 0.165 with the sign flipped.
        assert!(
            correct.final_rmse < 0.5 * zero.final_rmse,
            "{correct:?} vs {zero:?}"
        );
        assert!(
            correct.final_rmse < 0.5 * negated.final_rmse,
            "{correct:?} vs {negated:?}"
        );
        assert!(
            correct.final_rmse < 2.0 * correct.spatial_only_rmse,
            "the history stays usable under motion: {correct:?}"
        );
        Ok(())
    }

    /// Runs `frames` jittered frames of the check scene and returns the last output, the
    /// spatial-only output of the last frame, and the upscaler.
    fn accumulate(
        frames: u32,
        reactive: Option<&CpuImage>,
        transparency: Option<&CpuImage>,
    ) -> Result<(CpuImage, CpuImage, ReferenceUpscaler), UpscaleError> {
        let render = (32, 32);
        let display = (64, 64);
        let mut upscaler = ReferenceUpscaler::new(ReferenceSettings::default());
        let mut last = (CpuImage::new(0, 0), CpuImage::new(0, 0));
        for index in 1..=frames {
            let jitter = crate::taau::halton_jitter(u64::from(index), render.0, render.1);
            let color = render_scene(check_scene, render, jitter, [0.0, 0.0]);
            let frame = UpscaleFrame {
                reactive,
                transparency,
                ..frame_of(&color, display, jitter)
            };
            let out = upscaler.run(&frame)?;
            let alone = spatial_only(SpatialFilter::EdgeAdaptive).run(&frame)?;
            last = (out, alone);
        }
        Ok((last.0, last.1, upscaler))
    }

    #[test]
    fn a_reactive_mask_of_one_shows_this_frame_and_zero_keeps_the_history() -> Result<(), UpscaleError> {
        // Left half reactive (render x < 16), right half not.
        let mask = CpuImage::from_fn(32, 32, |x, _| grey(if x < 16 { 1.0 } else { 0.0 }));
        let (out, alone, upscaler) = accumulate(12, Some(&mask), None)?;
        let mut history_pixels = 0;
        for y in 0..64 {
            for x in 0..64 {
                if x < 30 {
                    // Display x < 30 reads render texels < 16 under any Halton jitter.
                    assert_eq!(out.texel(x, y), alone.texel(x, y), "({x}, {y})");
                    assert_eq!(upscaler.history_weight(x, y), 1.0);
                } else if x > 33 && upscaler.history_weight(x, y) > 1.0 {
                    history_pixels += 1;
                }
            }
        }
        assert!(
            history_pixels > 25 * 64,
            "the right half accumulated: {history_pixels}"
        );
        // The whole-frame check: a reactive frame is the spatial filter exactly.
        let report = run_check(&ReferenceCheck {
            frames: 8,
            reactive: true,
            ..ReferenceCheck::default()
        })?;
        assert_eq!(report.final_rmse.to_bits(), report.spatial_only_rmse.to_bits());
        // A transparency mask folds into the reactive mask.
        let (folded, _, _) = accumulate(12, None, Some(&mask))?;
        assert_eq!(folded, out);
        Ok(())
    }

    #[test]
    fn reset_and_a_size_change_drop_the_history() -> Result<(), UpscaleError> {
        let (_, _, mut upscaler) = accumulate(10, None, None)?;
        assert!(upscaler.history_weight(32, 32) > 1.0);
        let jitter = crate::taau::halton_jitter(11, 32, 32);
        let color = render_scene(check_scene, (32, 32), jitter, [0.0, 0.0]);
        let reset = UpscaleFrame {
            params: UpscaleParams {
                reset: true,
                jitter,
                ..UpscaleParams::new((32, 32), (64, 64))
            },
            ..frame_of(&color, (64, 64), jitter)
        };
        let out = upscaler.run(&reset)?;
        let alone = spatial_only(SpatialFilter::EdgeAdaptive).run(&reset)?;
        assert_eq!(out, alone);
        assert_eq!(upscaler.history_weight(32, 32), 1.0);
        // A new display size starts over too.
        let (_, _, mut upscaler) = accumulate(10, None, None)?;
        let bigger = frame_of(&color, (96, 96), jitter);
        let out = upscaler.run(&bigger)?;
        assert_eq!(out, spatial_only(SpatialFilter::EdgeAdaptive).run(&bigger)?);
        Ok(())
    }

    #[test]
    fn depth_picks_the_nearest_surface_for_motion() {
        // Reverse-Z: larger is nearer. The nearest texel around (2, 2) is (3, 1).
        let depth = CpuImage::from_fn(5, 5, |x, y| grey(if (x, y) == (3, 1) { 0.9 } else { 0.1 }));
        assert_eq!(nearest_surface(Some(&depth), 2, 2), (3, 1));
        assert_eq!(nearest_surface(Some(&depth), 0, 4), (0, 4), "out of reach");
        assert_eq!(nearest_surface(None, 2, 2), (2, 2));
        // At the image edge the neighbourhood is clamped, not wrapped.
        let corner = CpuImage::from_fn(3, 3, |x, y| grey((x + y) as f32));
        assert_eq!(nearest_surface(Some(&corner), 2, 2), (2, 2));
    }

    #[test]
    fn sharpening_keeps_flat_fields_and_never_overshoots() -> Result<(), UpscaleError> {
        let flat = CpuImage::from_fn(8, 8, |_, _| grey(0.25));
        assert_eq!(sharpen(&flat, 1.0), flat);
        let color = render_scene(step_scene, (32, 32), [0.0, 0.0], [0.0, 0.0]);
        let soft = spatial_only(SpatialFilter::EdgeAdaptive).run(&frame_of(&color, (64, 64), [0.0, 0.0]))?;
        let sharp = sharpen(&soft, 1.0);
        // Prototype: the ramp shrinks from 128 to 90 pixels.
        assert!(ramp_pixels(&sharp) < ramp_pixels(&soft));
        for y in 0..64u32 {
            for x in 0..64u32 {
                let (xi, yi) = (i64::from(x), i64::from(y));
                let cross = [
                    soft.load(xi, yi),
                    soft.load(xi, yi - 1),
                    soft.load(xi, yi + 1),
                    soft.load(xi - 1, yi),
                    soft.load(xi + 1, yi),
                ];
                let lo = cross.iter().map(|t| t[0]).fold(f32::INFINITY, f32::min);
                let hi = cross.iter().map(|t| t[0]).fold(f32::NEG_INFINITY, f32::max);
                let v = sharp.texel(x, y)[0];
                assert!((lo..=hi).contains(&v), "({x}, {y}): {v} outside {lo}..{hi}");
            }
        }
        // Through the boundary: `sharpness` applies after accumulation.
        let mut upscaler = spatial_only(SpatialFilter::EdgeAdaptive);
        let frame = UpscaleFrame {
            params: UpscaleParams {
                sharpness: 1.0,
                ..UpscaleParams::new((32, 32), (64, 64))
            },
            ..frame_of(&color, (64, 64), [0.0, 0.0])
        };
        assert_eq!(upscaler.run(&frame)?, sharp);
        Ok(())
    }

    #[test]
    fn the_check_is_deterministic_and_names_round_trip() -> Result<(), UpscaleError> {
        let check = ReferenceCheck {
            frames: 6,
            motion_px: [0.75, -0.25],
            ..ReferenceCheck::default()
        };
        assert_eq!(run_check(&check)?, run_check(&check)?);
        for filter in SpatialFilter::ALL {
            assert_eq!(SpatialFilter::from_name(filter.name()), Some(filter));
        }
        assert_eq!(
            SpatialFilter::from_name("FSR1"),
            Some(SpatialFilter::EdgeAdaptive)
        );
        assert_eq!(SpatialFilter::from_name("lanczos"), None);
        for mode in [
            MotionVectorMode::Correct,
            MotionVectorMode::Zero,
            MotionVectorMode::Negated,
        ] {
            assert_eq!(MotionVectorMode::from_name(mode.name()), Some(mode));
        }
        // An impossible size is refused, not panicked on.
        let tiny = run_check(&ReferenceCheck {
            display_size: (0, 0),
            ..ReferenceCheck::default()
        });
        assert_eq!(tiny.err().map(|e| e.code()), Some("invalid_size"));
        Ok(())
    }
}
