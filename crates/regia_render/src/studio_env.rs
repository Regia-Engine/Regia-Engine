// SPDX-License-Identifier: MPL-2.0
//! Studio environments for image-based light: a horizon-to-zenith gradient with a ground colour
//! ([`StudioGradient`]) and a caller-supplied HDR equirect image ([`StudioEquirect`]).
//!
//! Both feed the same split-sum path as the procedural sky and the two-tone studio
//! ([`crate::FrameRenderer::set_studio_sky`]): a cosine-convolved irradiance map (diffuse) and
//! GGX-prefiltered specular mips (Karis, "Real Shading in the legacy engine4", SIGGRAPH 2013 course
//! notes), baked in linear radiance and stored as RGBA16F so a smooth ramp does not band in a
//! mirror and an HDR image is not clipped. The bake is a pure function of the parameters, so a
//! 64-bit key of them ([`StudioGradient::cache_key`], [`StudioEquirect::cache_key`]) says whether
//! a rebuild is needed.
//!
//! The base of the specular chain (the mirror mip and the default background) is
//! [`StudioEnvResolution`]: 256 × 128 by default, larger for a studio frame whose mirrors must keep
//! a small bright source (ADR-2000). The chain halves down to 32 wide, one mip per roughness step
//! (`roughness = mip / last mip`), so a larger base also has finer roughness steps. Every lobe reads
//! a solid-angle-weighted pyramid of the image at the level its sample covers (filtered importance
//! sampling, Křivánek & Colbert 2008): a hot disc neither aliases into speckle nor loses energy.

use crate::SKY_IBL_SCALE;
use crate::ibl::{
    IblHalf, IblImage, IblLower, IblMaps, build_brdf_lut, cosine_hemisphere, dir_from_equirect, dot,
    hammersley, importance_sample_ggx, reflect, rgb_to_rgba16f, to_u8, to_world,
};

/// Base width of [`StudioEnvResolution::Standard`].
pub const STUDIO_ENV_STANDARD_WIDTH: u32 = 256;
/// Base width of [`StudioEnvResolution::Studio`].
pub const STUDIO_ENV_STUDIO_WIDTH: u32 = 1024;
/// Largest base the renderer keeps unless [`crate::FrameRenderer::set_studio_env_budget`] says
/// otherwise (67 MB of mirror mip, 89 MB with the chain); Android keeps
/// [`STUDIO_ENV_STUDIO_WIDTH`].
pub const STUDIO_ENV_DEFAULT_BUDGET: u32 = if cfg!(target_os = "android") {
    STUDIO_ENV_STUDIO_WIDTH
} else {
    4096
};
/// Width of the coarsest prefilter mip (roughness 1).
const LAST_MIP_WIDTH: u32 = 32;
/// Width and height of the irradiance map.
const IRRADIANCE_SIZE: (u32, u32) = (64, 32);
/// Width and height of the lower-hemisphere irradiance map (mip 1 of the irradiance texture).
const LOWER_SIZE: (u32, u32) = (32, 16);
/// Samples per texel of the irradiance integrals and of the GGX integrals up to 256 wide.
const SAMPLES: u32 = 256;
/// Samples per texel of the GGX integrals above 256 wide: their lobes are narrow, and the
/// filtered lookup keeps them smooth.
const SAMPLES_FINE: u32 = 128;
/// Samples of the white plane's irradiance ([`IblLower::plane_feed`]).
const PLANE_FEED_SAMPLES: u32 = 4096;
/// Texels × samples above which a map bakes on several threads.
const PARALLEL_SAMPLES: u64 = 1 << 20;
/// Largest equirect image accepted, in texels (8192 × 4096).
pub const STUDIO_EQUIRECT_MAX_TEXELS: usize = 8192 * 4096;
/// Smallest and largest [`StudioGradient::falloff`].
pub const STUDIO_FALLOFF_RANGE: (f32, f32) = (0.05, 16.0);

/// Why a studio environment was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StudioEnvError {
    /// A radiance value is NaN, infinite or negative.
    #[error("{0} must be finite and >= 0 in every channel")]
    BadRadiance(&'static str),
    /// The gradient falloff is outside [`STUDIO_FALLOFF_RANGE`] or not finite.
    #[error("falloff must be finite and within 0.05 to 16")]
    BadFalloff,
    /// The image is under 2 × 1 texels or over [`STUDIO_EQUIRECT_MAX_TEXELS`].
    #[error("equirect image must be 2x1 or larger and at most 33554432 texels, got {width}x{height}")]
    BadSize {
        /// Width in texels.
        width: u32,
        /// Height in texels.
        height: u32,
    },
    /// The texel count does not match the size.
    #[error("equirect image of {width}x{height} needs {expected} texels, got {got}")]
    TexelCount {
        /// Width in texels.
        width: u32,
        /// Height in texels.
        height: u32,
        /// Texels the size implies.
        expected: usize,
        /// Texels supplied.
        got: usize,
    },
}

/// How wide the base of a studio environment's specular chain should be. The result is a power of
/// two from 256 to the renderer's budget; the height is half of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StudioEnvResolution {
    /// 256 × 128, the default.
    #[default]
    Standard,
    /// 1024 × 512, or the source image's width when that is smaller.
    Studio,
    /// The source image's width (an analytic gradient has no source and takes 1024).
    Source,
    /// This width (no wider than an image's source).
    Width(u32),
}

impl StudioEnvResolution {
    /// The base width kept for a source of `source_width` texels (`None`: analytic, any width is
    /// exact) under a renderer budget of `budget` texels: rounded down to a power of two, at least
    /// 256 even for a smaller source (that one is enlarged, as the standard base always did) and
    /// at most the budget.
    #[must_use]
    pub fn effective_width(self, source_width: Option<u32>, budget: u32) -> u32 {
        let source = source_width.unwrap_or(u32::MAX);
        let wanted = match self {
            Self::Standard => STUDIO_ENV_STANDARD_WIDTH,
            Self::Studio => STUDIO_ENV_STUDIO_WIDTH.min(source),
            Self::Source => source_width.unwrap_or(STUDIO_ENV_STUDIO_WIDTH),
            Self::Width(w) => w.min(source),
        };
        let wanted = wanted.min(budget.max(STUDIO_ENV_STANDARD_WIDTH));
        // Rounded down to a power of two; a smaller source is enlarged to the standard base.
        (1u32 << wanted.max(1).ilog2()).max(STUDIO_ENV_STANDARD_WIDTH)
    }

    /// `"standard"`, `"studio"`, `"source"` or `"width"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Studio => "studio",
            Self::Source => "source",
            Self::Width(_) => "width",
        }
    }
}

/// Which of the two draws the sky and gives the image light of a frame that has both an
/// installed studio environment and the physical atmosphere (Desktop High and Ultra, or any
/// planet other than Earth): `FrameRenderer::set_environment_authority`. Frames without the
/// atmosphere, or without a studio environment, are the same under both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EnvironmentAuthority {
    /// The default, and the behaviour of every release up to v0.2.8: the atmosphere's LUT sky is
    /// drawn and its IBL bake is written over the installed environment's maps, so the frame is
    /// lit by a mix of the two. Kept as the default so a project's exposure, fitted on such a
    /// frame, holds (ADR-2016).
    #[default]
    Atmosphere,
    /// Opt-in (ADR-2000 (0)): the installed studio environment is the frame's sky and image
    /// light: the atmosphere's LUT sky is not drawn and its bake does not touch the maps. The
    /// studio background (`FrameRenderer::set_studio_background`) shows on such a frame.
    Studio,
}

impl EnvironmentAuthority {
    /// `"atmosphere"` or `"studio"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Atmosphere => "atmosphere",
            Self::Studio => "studio",
        }
    }

    /// The authority named `name` ([`Self::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "atmosphere" => Some(Self::Atmosphere),
            "studio" => Some(Self::Studio),
            _ => None,
        }
    }
}

/// What the sky draw shows behind a studio frame (`FrameRenderer::set_studio_background`). The
/// lighting never reads it.
#[derive(Clone, Debug, Default)]
pub enum StudioBackground {
    /// The lighting environment itself.
    #[default]
    Environment,
    /// One radiance, cd/m² per channel.
    Solid([f32; 3]),
    /// A ramp over the view direction.
    Gradient(StudioGradient),
    /// A second equirect image, cd/m².
    Image(StudioEquirect),
}

impl StudioBackground {
    /// `"environment"`, `"solid"`, `"gradient"` or `"image"`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Solid(_) => "solid",
            Self::Gradient(_) => "gradient",
            Self::Image(_) => "image",
        }
    }

    /// Checks the background's values.
    ///
    /// # Errors
    /// [`StudioEnvError`] for a bad radiance or falloff.
    pub fn validate(&self) -> Result<(), StudioEnvError> {
        match self {
            Self::Solid(color) => check_radiance("color", *color),
            Self::Gradient(gradient) => gradient.validate(),
            Self::Environment | Self::Image(_) => Ok(()),
        }
    }
}

/// Prefilter mips of a chain whose base is `base_width` wide: 256 keeps the four it always had.
fn mip_count(base_width: u32) -> usize {
    (base_width.max(LAST_MIP_WIDTH) / LAST_MIP_WIDTH).ilog2() as usize + 1
}

/// What the renderer kept of a studio environment (`render.get_studio_environment`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StudioEnvInfo {
    /// Base width of the specular chain: the mirror's resolution.
    pub base_width: u32,
    /// Base height of the specular chain.
    pub base_height: u32,
    /// Mips of the chain.
    pub mips: u32,
    /// Source image size (`None` for an analytic environment).
    pub source: Option<(u32, u32)>,
    /// Bytes of the chain and the irradiance maps on the GPU.
    pub gpu_bytes: u64,
}

fn check_radiance(name: &'static str, v: [f32; 3]) -> Result<(), StudioEnvError> {
    if v.iter().all(|c| c.is_finite() && *c >= 0.0) {
        Ok(())
    } else {
        Err(StudioEnvError::BadRadiance(name))
    }
}

/// 64-bit FNV-1a over a stream of words; stable across runs and platforms (unlike `DefaultHasher`).
struct Fnv(u64);

impl Fnv {
    fn new(tag: u8) -> Self {
        let mut h = Self(0xcbf2_9ce4_8422_2325);
        h.bytes(&[tag]);
        h
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn f32s(&mut self, values: &[f32]) {
        for v in values {
            // -0.0 and 0.0 bake identically.
            let v = if *v == 0.0 { 0.0 } else { *v };
            self.bytes(&v.to_bits().to_le_bytes());
        }
    }
}

/// A sun-disc-free studio sky: radiance ramps from `horizon` at the horizon to `zenith` straight
/// up, and everything below the horizon is `ground`. Radiances are cd/m² per channel, linear.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StudioGradient {
    /// Radiance straight up.
    pub zenith: [f32; 3],
    /// Radiance at the horizon (elevation 0, upper side).
    pub horizon: [f32; 3],
    /// Radiance of the whole lower hemisphere (the first bounce off a large plane).
    pub ground: [f32; 3],
    /// Ramp shape: the blend toward the zenith is `sin(elevation) ^ falloff`. 1 is linear in
    /// `sin(elevation)`; above 1 holds the horizon colour longer, below 1 reaches the zenith
    /// colour sooner. Within [`STUDIO_FALLOFF_RANGE`].
    pub falloff: f32,
}

impl StudioGradient {
    /// A gradient with the linear ramp (`falloff` 1).
    #[must_use]
    pub fn new(zenith: [f32; 3], horizon: [f32; 3], ground: [f32; 3]) -> Self {
        Self {
            zenith,
            horizon,
            ground,
            falloff: 1.0,
        }
    }

    /// Checks every radiance is finite and non-negative and the falloff is in range.
    ///
    /// # Errors
    /// [`StudioEnvError::BadRadiance`] or [`StudioEnvError::BadFalloff`].
    pub fn validate(&self) -> Result<(), StudioEnvError> {
        check_radiance("zenith", self.zenith)?;
        check_radiance("horizon", self.horizon)?;
        check_radiance("ground", self.ground)?;
        if !(self.falloff.is_finite()
            && (STUDIO_FALLOFF_RANGE.0..=STUDIO_FALLOFF_RANGE.1).contains(&self.falloff))
        {
            return Err(StudioEnvError::BadFalloff);
        }
        Ok(())
    }

    /// Radiance toward the unit vector `dir` (Y up), cd/m².
    #[must_use]
    pub fn radiance(&self, dir: [f32; 3]) -> [f32; 3] {
        let y = dir[1];
        if y < 0.0 {
            return self.ground;
        }
        let t = y.clamp(0.0, 1.0).powf(self.falloff);
        [0, 1, 2].map(|c| self.horizon[c] + (self.zenith[c] - self.horizon[c]) * t)
    }

    /// The rebuild key: equal keys bake identical maps. Distinct from every
    /// [`StudioEquirect::cache_key`] by construction (a tag byte), up to hash collisions.
    #[must_use]
    pub fn cache_key(&self) -> u64 {
        let mut h = Fnv::new(1);
        h.f32s(&self.zenith);
        h.f32s(&self.horizon);
        h.f32s(&self.ground);
        h.f32s(&[self.falloff]);
        h.0
    }

    /// Bakes the IBL maps of this gradient with a base `base_width` wide.
    #[must_use]
    pub(crate) fn bake(&self, base_width: u32) -> IblMaps {
        bake(&Analytic(self), base_width)
    }
}

/// The radiance a bake reads.
trait BakeSource: Sync {
    /// Radiance toward the unit vector `dir`, from a copy of the image blurred to pyramid level
    /// `lod` (0 = the base; analytic sources are exact at every level).
    fn radiance(&self, dir: [f32; 3], lod: f32) -> [f32; 3];
    /// Pyramid levels above the base (0: analytic).
    fn top_lod(&self) -> f32;
    /// Whether mip `m`'s lobes and the irradiance read fixed levels (`m`, the last) instead of the
    /// filtered lookup: [`BoxChain`], the standard bake of v0.2.8 and earlier.
    fn per_mip(&self) -> bool {
        false
    }
}

struct Analytic<'a>(&'a StudioGradient);

impl BakeSource for Analytic<'_> {
    fn radiance(&self, dir: [f32; 3], _lod: f32) -> [f32; 3] {
        self.0.radiance(dir)
    }

    fn top_lod(&self) -> f32 {
        0.0
    }
}

/// An image as v0.2.8 and earlier baked it at the standard 256 base, kept so a default studio
/// environment's maps are the same bytes (ADR-2016): the image box-averaged to 256 × 128, then
/// each level the plain box half of the one before it, down to 32 wide; prefilter mip `m` reads
/// level `m`, the irradiance the last level.
struct BoxChain(Vec<Level>);

impl BoxChain {
    fn new(source: &Level) -> Self {
        let base_width = STUDIO_ENV_STANDARD_WIDTH;
        let mips = mip_count(base_width);
        let mut levels = vec![resample_box(source, base_width, base_width / 2)];
        for m in 1..mips {
            let prev = &levels[m - 1];
            let next = resample_box(prev, (base_width >> m).max(1), ((base_width / 2) >> m).max(1));
            levels.push(next);
        }
        Self(levels)
    }
}

impl BakeSource for BoxChain {
    fn radiance(&self, dir: [f32; 3], lod: f32) -> [f32; 3] {
        let level = (lod.max(0.0) as usize).min(self.0.len() - 1);
        self.0[level].sample(dir)
    }

    fn top_lod(&self) -> f32 {
        (self.0.len() - 1) as f32
    }

    fn per_mip(&self) -> bool {
        true
    }
}

/// An image as a pyramid from the base down: each level the solid-angle-weighted half of the one
/// before it, so a level holds the same energy as the base.
struct Pyramid(Vec<Level>);

impl Pyramid {
    fn new(base: Level) -> Self {
        let mut levels = vec![base];
        while let Some(last) = levels.last()
            && last.width > 4
        {
            let next = resample(last, last.width / 2, (last.height / 2).max(1));
            levels.push(next);
        }
        Self(levels)
    }
}

impl BakeSource for Pyramid {
    fn radiance(&self, dir: [f32; 3], lod: f32) -> [f32; 3] {
        let lod = lod.clamp(0.0, self.top_lod());
        let low = lod.floor() as usize;
        let a = self.0[low].sample(dir);
        let t = lod - low as f32;
        if t <= 0.0 {
            return a;
        }
        let b = self.0[low + 1].sample(dir);
        [0, 1, 2].map(|c| a[c] + (b[c] - a[c]) * t)
    }

    fn top_lod(&self) -> f32 {
        (self.0.len() - 1) as f32
    }
}

/// An HDR environment image in the equirectangular layout the renderer's sky maps use: column
/// `x` of `width` is azimuth `atan2(z, x)` from -π to π (the image centre looks along +X), row
/// `y` of `height` runs from straight up (+Y) at the top to straight down at the bottom.
/// Texels are linear radiance, cd/m².
#[derive(Clone, Debug)]
pub struct StudioEquirect {
    width: u32,
    height: u32,
    texels: Vec<[f32; 3]>,
    key: u64,
}

impl StudioEquirect {
    /// Wraps `texels` (row-major, `width * height`, top row first).
    ///
    /// # Errors
    /// [`StudioEnvError::BadSize`], [`StudioEnvError::TexelCount`] or
    /// [`StudioEnvError::BadRadiance`] (a NaN, infinite or negative channel).
    pub fn new(width: u32, height: u32, texels: Vec<[f32; 3]>) -> Result<Self, StudioEnvError> {
        let count = (width as usize).saturating_mul(height as usize);
        if width < 2 || height < 1 || count > STUDIO_EQUIRECT_MAX_TEXELS {
            return Err(StudioEnvError::BadSize { width, height });
        }
        if texels.len() != count {
            return Err(StudioEnvError::TexelCount {
                width,
                height,
                expected: count,
                got: texels.len(),
            });
        }
        if texels
            .iter()
            .any(|t| !t.iter().all(|c| c.is_finite() && *c >= 0.0))
        {
            return Err(StudioEnvError::BadRadiance("equirect texel"));
        }
        let mut h = Fnv::new(2);
        h.bytes(&width.to_le_bytes());
        h.bytes(&height.to_le_bytes());
        for t in &texels {
            h.f32s(t);
        }
        Ok(Self {
            width,
            height,
            texels,
            key: h.0,
        })
    }

    /// Width in texels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in texels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The rebuild key: a hash of the size and every texel, computed once at construction.
    #[must_use]
    pub fn cache_key(&self) -> u64 {
        self.key
    }

    /// Bakes the IBL maps of this image with a base `base_width` wide. The standard 256 base
    /// bakes as v0.2.8 did ([`BoxChain`], ADR-2016); a larger one filters a solid-angle-weighted
    /// pyramid (ADR-2000 (1)).
    #[must_use]
    pub(crate) fn bake(&self, base_width: u32) -> IblMaps {
        if base_width == STUDIO_ENV_STANDARD_WIDTH {
            let source = Level {
                width: self.width,
                height: self.height,
                texels: self.texels.clone(),
            };
            return bake(&BoxChain::new(&source), base_width);
        }
        bake(&Pyramid::new(self.level(base_width)), base_width)
    }

    /// This image at `width × width / 2`.
    fn level(&self, width: u32) -> Level {
        let source = Level {
            width: self.width,
            height: self.height,
            texels: self.texels.clone(),
        };
        if (self.width, self.height) == (width, width / 2) {
            source
        } else {
            resample(&source, width, width / 2)
        }
    }

    /// RGBA16F bytes of this image at `width × width / 2`, radiance over `SKY_IBL_SCALE`.
    pub(crate) fn encoded(&self, width: u32) -> Vec<u8> {
        encode_maps(width, width / 2, &self.level(width).texels).1
    }
}

/// Premultiplied radiance and coverage of an equirect capture: `texels[..]` are RGB, `coverage`
/// the share of each texel a surface fills (1) rather than the environment (0).
pub(crate) struct CaptureImage {
    /// Width in texels; the height is half of it.
    pub(crate) width: u32,
    /// Premultiplied radiance, cd/m², top row first.
    pub(crate) texels: Vec<[f32; 3]>,
    /// Coverage, top row first.
    pub(crate) coverage: Vec<f32>,
}

/// One RGBA16F level per prefilter mip of `image`: `width >> mip` wide. RGB is premultiplied
/// radiance over `SKY_IBL_SCALE`, alpha the coverage, both GGX-filtered as the environment is, so a
/// lobe that is half surface blends half with the environment.
pub(crate) fn capture_chain(image: &CaptureImage) -> Vec<Vec<u8>> {
    let (width, height) = (image.width, image.width / 2);
    let radiance = Pyramid::new(Level {
        width,
        height,
        texels: image.texels.clone(),
    });
    let coverage = Pyramid::new(Level {
        width,
        height,
        texels: image.coverage.iter().map(|c| [*c; 3]).collect(),
    });
    let k = 1.0 / SKY_IBL_SCALE;
    let colour = prefilter_chain(&radiance, width);
    let alpha = prefilter_chain(&coverage, width);
    colour
        .iter()
        .zip(&alpha)
        .map(|(rgb, a)| {
            let mut out = Vec::with_capacity(rgb.len() * 8);
            for (t, c) in rgb.iter().zip(a) {
                for v in [t[0] * k, t[1] * k, t[2] * k, c[0]] {
                    out.extend_from_slice(&crate::ibl::f16_bits(v.max(0.0)).to_le_bytes());
                }
            }
            out
        })
        .collect()
}

/// Mips of [`capture_chain`] for a capture `width` wide.
pub(crate) fn capture_mips(width: u32) -> u32 {
    mip_count(width) as u32
}

/// A float equirect image used as a pyramid level.
struct Level {
    width: u32,
    height: u32,
    texels: Vec<[f32; 3]>,
}

impl Level {
    fn texel(&self, x: i64, y: i64) -> [f32; 3] {
        let w = i64::from(self.width);
        let h = i64::from(self.height);
        let x = x.rem_euclid(w);
        let y = y.clamp(0, h - 1);
        self.texels[(y * w + x) as usize]
    }

    /// Bilinear radiance toward the unit vector `dir`: wraps in azimuth, clamps at the poles.
    fn sample(&self, dir: [f32; 3]) -> [f32; 3] {
        let u = dir[2].atan2(dir[0]) / (2.0 * std::f32::consts::PI) + 0.5;
        let v = dir[1].clamp(-1.0, 1.0).acos() / std::f32::consts::PI;
        self.sample_uv(u, v)
    }

    fn sample_uv(&self, u: f32, v: f32) -> [f32; 3] {
        let x = u * self.width as f32 - 0.5;
        let y = v * self.height as f32 - 0.5;
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let (a, b) = (self.texel(x0, y0), self.texel(x0 + 1, y0));
        let (c, d) = (self.texel(x0, y0 + 1), self.texel(x0 + 1, y0 + 1));
        [0, 1, 2].map(|k| {
            let top = a[k] + (b[k] - a[k]) * fx;
            let bottom = c[k] + (d[k] - c[k]) * fx;
            top + (bottom - top) * fy
        })
    }
}

/// `src` at `width × height`: an average of the covered source texels weighted by their solid
/// angle (a row's `sin θ`, so a hot source keeps its energy at any latitude) when shrinking,
/// bilinear when enlarging. Deterministic (fixed iteration and summation order).
fn resample(src: &Level, width: u32, height: u32) -> Level {
    let mut texels = Vec::with_capacity((width * height) as usize);
    let shrink = src.width > width || src.height > height;
    let row_weight: Vec<f32> = (0..src.height)
        .map(|y| ((y as f32 + 0.5) / src.height as f32 * std::f32::consts::PI).sin())
        .collect();
    for y in 0..height {
        for x in 0..width {
            if shrink {
                let range = |i: u32, n: u32, s: u32| {
                    let lo = (u64::from(i) * u64::from(s) / u64::from(n)) as u32;
                    // Exclusive end: the next bin's `lo`, so adjacent bins meet without overlap.
                    let hi = if i + 1 == n {
                        s
                    } else {
                        ((u64::from(i) + 1) * u64::from(s) / u64::from(n)) as u32
                    };
                    (lo.min(s - 1), hi.clamp(lo + 1, s))
                };
                let (x0, x1) = range(x, width, src.width);
                let (y0, y1) = range(y, height, src.height);
                let mut sum = [0.0f32; 3];
                let mut weight = 0.0f32;
                for sy in y0..y1 {
                    let w = row_weight[sy as usize];
                    for sx in x0..x1 {
                        let t = src.texels[(sy * src.width + sx) as usize];
                        for (s, v) in sum.iter_mut().zip(t) {
                            *s += v * w;
                        }
                        weight += w;
                    }
                }
                texels.push(sum.map(|v| v / weight));
            } else {
                texels.push(src.sample_uv((x as f32 + 0.5) / width as f32, (y as f32 + 0.5) / height as f32));
            }
        }
    }
    Level {
        width,
        height,
        texels,
    }
}

/// `f` of every texel direction of a `width × height` equirect map, top row first; on several
/// threads when `cost` (samples per texel) makes the map worth it. Each texel is a pure function of
/// its direction, so the result does not depend on the thread count.
fn equirect_texels(
    width: u32,
    height: u32,
    cost: u32,
    f: &(dyn Fn([f32; 3]) -> [f32; 3] + Sync),
) -> Vec<[f32; 3]> {
    let row = |y: u32, out: &mut [[f32; 3]]| {
        for (x, t) in out.iter_mut().enumerate() {
            let u = (x as f32 + 0.5) / width as f32;
            let v = (y as f32 + 0.5) / height as f32;
            *t = f(dir_from_equirect(u, v));
        }
    };
    let mut texels = vec![[0.0f32; 3]; (width * height) as usize];
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    if threads <= 1 || u64::from(width) * u64::from(height) * u64::from(cost) < PARALLEL_SAMPLES {
        for (y, out) in texels.chunks_mut(width as usize).enumerate() {
            row(y as u32, out);
        }
        return texels;
    }
    let rows_per_job = (height as usize).div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (job, out) in texels.chunks_mut(rows_per_job * width as usize).enumerate() {
            let row = &row;
            scope.spawn(move || {
                for (i, line) in out.chunks_mut(width as usize).enumerate() {
                    row((job * rows_per_job + i) as u32, line);
                }
            });
        }
    });
    texels
}

/// RGBA8 (clipped, for hosts that read the images) and RGBA16F (the upload) of `texels`, radiance
/// divided by `SKY_IBL_SCALE`.
fn encode_maps(width: u32, height: u32, texels: &[[f32; 3]]) -> (IblImage, Vec<u8>) {
    let k = 1.0 / SKY_IBL_SCALE;
    let scaled: Vec<[f32; 3]> = texels.iter().map(|t| t.map(|v| v * k)).collect();
    let mut rgba = vec![255u8; scaled.len() * 4];
    for (out, t) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&scaled) {
        for (o, v) in out.iter_mut().zip(t) {
            *o = to_u8(*v);
        }
    }
    (IblImage { width, height, rgba }, rgb_to_rgba16f(&scaled))
}

/// `src` at `width × height` as v0.2.8 resampled it: a plain box average of the covered source
/// texels when shrinking, bilinear when enlarging ([`BoxChain`]). [`resample`] weighs the rows by
/// their solid angle instead.
fn resample_box(src: &Level, width: u32, height: u32) -> Level {
    let mut texels = Vec::with_capacity((width * height) as usize);
    let shrink = src.width > width || src.height > height;
    for y in 0..height {
        for x in 0..width {
            if shrink {
                let range = |i: u32, n: u32, s: u32| {
                    let lo = (u64::from(i) * u64::from(s) / u64::from(n)) as u32;
                    let hi = ((u64::from(i) + 1) * u64::from(s)).div_ceil(u64::from(n)) as u32;
                    (lo.min(s - 1), hi.clamp(lo + 1, s))
                };
                let (x0, x1) = range(x, width, src.width);
                let (y0, y1) = range(y, height, src.height);
                let mut sum = [0.0f32; 3];
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let t = src.texels[(sy * src.width + sx) as usize];
                        for (s, v) in sum.iter_mut().zip(t) {
                            *s += v;
                        }
                    }
                }
                let inv = 1.0 / ((x1 - x0) * (y1 - y0)) as f32;
                texels.push(sum.map(|v| v * inv));
            } else {
                texels.push(src.sample_uv((x as f32 + 0.5) / width as f32, (y as f32 + 0.5) / height as f32));
            }
        }
    }
    Level {
        width,
        height,
        texels,
    }
}

/// Solid angle of a base texel of `width × width / 2` toward `dir`.
fn texel_solid_angle(dir: [f32; 3], width: u32) -> f32 {
    let height = (width / 2).max(1) as f32;
    let sin_theta = (1.0 - dir[1] * dir[1]).max(0.0).sqrt().max(1e-3);
    2.0 * std::f32::consts::PI * std::f32::consts::PI * sin_theta / (width as f32 * height)
}

/// The GGX density of a half vector at `n_dot_h` (N = V = R: also the density of the light
/// direction, up to the factor 1/4 this returns it with).
fn ggx_light_pdf(n_dot_h: f32, roughness: f32) -> f32 {
    let a = roughness * roughness;
    let a2 = a * a;
    let d = n_dot_h * n_dot_h * (a2 - 1.0) + 1.0;
    (a2 / (std::f32::consts::PI * d * d)).max(1e-6) * 0.25
}

/// The GGX-prefiltered radiance around `r` at `roughness`. Each sample reads the pyramid at the
/// level whose texel covers the sample's share of the lobe, one level blurrier than the match
/// (Křivánek & Colbert 2008, "Real-time Shading with Filtered Importance Sampling", § 4).
/// `fixed_lod` replaces the filtered lookup with one level for every sample ([`BoxChain`]).
fn prefilter_texel(
    env: &dyn BakeSource,
    r: [f32; 3],
    roughness: f32,
    samples: u32,
    base_width: u32,
    fixed_lod: Option<f32>,
) -> [f32; 3] {
    let (mut sum, mut weight) = ([0.0f32; 3], 0.0f32);
    for i in 0..samples {
        let h = importance_sample_ggx(hammersley(i, samples), roughness, r);
        let l = reflect(r, h);
        let n_dot_l = dot(r, l);
        if n_dot_l > 0.0 {
            let lod = if let Some(lod) = fixed_lod {
                lod
            } else if env.top_lod() > 0.0 {
                let sample_angle = 1.0 / (samples as f32 * ggx_light_pdf(dot(r, h), roughness));
                0.5 * (sample_angle / texel_solid_angle(l, base_width)).log2() + 1.0
            } else {
                0.0
            };
            let e = env.radiance(l, lod);
            for (s, v) in sum.iter_mut().zip(e) {
                *s += v * n_dot_l;
            }
            weight += n_dot_l;
        }
    }
    if weight <= 1e-5 {
        env.radiance(r, fixed_lod.unwrap_or(0.0))
    } else {
        sum.map(|v| v / weight)
    }
}

/// The texels of every prefilter mip of `env` from `base_width` down to 32 wide.
fn prefilter_chain(env: &dyn BakeSource, base_width: u32) -> Vec<Vec<[f32; 3]>> {
    let base_height = (base_width / 2).max(1);
    let mips = mip_count(base_width);
    (0..mips)
        .map(|m| {
            let roughness = m as f32 / (mips - 1) as f32;
            let width = (base_width >> m).max(1);
            let height = (base_height >> m).max(1);
            let samples = if m == 0 {
                1
            } else if width > STUDIO_ENV_STANDARD_WIDTH {
                SAMPLES_FINE
            } else {
                SAMPLES
            };
            equirect_texels(width, height, samples, &|r| {
                if m == 0 {
                    env.radiance(r, 0.0)
                } else {
                    prefilter_texel(
                        env,
                        r,
                        roughness,
                        samples,
                        base_width,
                        env.per_mip().then_some(m as f32),
                    )
                }
            })
        })
        .collect()
}

/// The split-sum maps of `env`, the prefilter chain starting at `base_width`.
fn bake(env: &dyn BakeSource, base_width: u32) -> IblMaps {
    let base_height = (base_width / 2).max(1);
    let mips = mip_count(base_width);
    // The diffuse integrals read the pyramid at about the irradiance map's own width (the last
    // level of a `BoxChain`).
    let irr_lod = if env.per_mip() {
        (mips - 1) as f32
    } else {
        (base_width as f32 / LAST_MIP_WIDTH as f32).log2()
    };
    let cosine_mean = |n: [f32; 3], keep: &(dyn Fn([f32; 3]) -> bool + Sync), samples: u32| {
        let mut sum = [0.0f32; 3];
        for i in 0..samples {
            let l = to_world(cosine_hemisphere(hammersley(i, samples)), n);
            if keep(l) {
                let e = env.radiance(l, irr_lod);
                for (s, v) in sum.iter_mut().zip(e) {
                    *s += v;
                }
            }
        }
        sum.map(|v| v / samples as f32)
    };
    let (irr_w, irr_h) = IRRADIANCE_SIZE;
    let irr_texels = equirect_texels(irr_w, irr_h, SAMPLES, &|n| cosine_mean(n, &|_| true, SAMPLES));
    let (irradiance, irr_half) = encode_maps(irr_w, irr_h, &irr_texels);
    let (lower_w, lower_h) = LOWER_SIZE;
    let lower_texels = equirect_texels(lower_w, lower_h, SAMPLES, &|n| {
        cosine_mean(n, &|l| l[1] < 0.0, SAMPLES)
    });
    let (_, lower_half) = encode_maps(lower_w, lower_h, &lower_texels);
    let plane_feed = cosine_mean([0.0, 1.0, 0.0], &|l| l[1] > 0.0, PLANE_FEED_SAMPLES);
    let mut prefilter = Vec::with_capacity(mips);
    let mut prefilter_half = Vec::with_capacity(mips);
    for (m, texels) in prefilter_chain(env, base_width).iter().enumerate() {
        let (image, half) = encode_maps((base_width >> m).max(1), (base_height >> m).max(1), texels);
        prefilter.push(image);
        prefilter_half.push(half);
    }
    IblMaps {
        irradiance,
        prefilter,
        brdf: build_brdf_lut(),
        half: Some(IblHalf {
            irradiance: irr_half,
            prefilter: prefilter_half,
        }),
        lower: Some(IblLower {
            width: lower_w,
            height: lower_h,
            half: lower_half,
            plane_feed,
        }),
    }
}

#[cfg(test)]
mod tests {
    /// The standard bake is v0.2.8's, byte for byte (ADR-2016): the same image and gradient
    /// baked at v0.2.8 (18bfbe1bb) hash to these values on x86_64 Linux. Other targets may round
    /// the trigonometry differently, so the compare runs there only.
    #[test]
    fn the_standard_bake_keeps_the_bytes_of_v0_2_8() {
        let (w, h) = (512u32, 256u32);
        let mut texels = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
                let hot = if (x as i32 - 300).abs() < 3 && (y as i32 - 70).abs() < 3 {
                    60_000.0
                } else {
                    0.0
                };
                texels.push([
                    500.0 + 1500.0 * (1.0 - fy) + 300.0 * fx + hot,
                    700.0 + 1200.0 * (1.0 - fy) + hot,
                    900.0 + 2500.0 * (1.0 - fy) * (1.0 - fy) + hot,
                ]);
            }
        }
        let image = super::StudioEquirect::new(w, h, texels).expect("image");
        let gradient = super::StudioGradient::new(
            [2400.0, 4200.0, 8000.0],
            [2600.0, 2800.0, 3000.0],
            [600.0, 600.0, 600.0],
        );
        let hash = |maps: &super::IblMaps| {
            let half = maps.half.as_ref().expect("half");
            let mut h = 0xcbf2_9ce4_8422_2325u64;
            for bytes in std::iter::once(&half.irradiance).chain(half.prefilter.iter()) {
                for b in bytes {
                    h ^= u64::from(*b);
                    h = h.wrapping_mul(0x0100_0000_01b3);
                }
            }
            h
        };
        let standard = super::STUDIO_ENV_STANDARD_WIDTH;
        let (equirect, analytic) = (hash(&image.bake(standard)), hash(&gradient.bake(standard)));
        eprintln!("standard bake hashes: equirect {equirect:016x}, gradient {analytic:016x}");
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(
                equirect, 0x3d81_0c12_822b_f30e,
                "the equirect bake moved from v0.2.8's"
            );
            assert_eq!(
                analytic, 0x532b_5164_aa04_f74c,
                "the gradient bake moved from v0.2.8's"
            );
        }
        // A larger base takes the filtered pyramid, a different chain.
        assert_ne!(hash(&image.bake(2 * standard)), equirect);
    }

    #[test]
    fn the_environment_authority_defaults_to_the_atmosphere_and_round_trips_its_name() {
        use super::EnvironmentAuthority;
        assert_eq!(EnvironmentAuthority::default(), EnvironmentAuthority::Atmosphere);
        for a in [EnvironmentAuthority::Atmosphere, EnvironmentAuthority::Studio] {
            assert_eq!(EnvironmentAuthority::from_name(a.name()), Some(a));
        }
        assert_eq!(EnvironmentAuthority::from_name("Studio"), None);
    }

    use super::*;

    /// Studio-sky magnitudes (cd/m²): the half-float maps hold radiance / `SKY_IBL_SCALE`.
    const S: f32 = 4000.0;
    const BASE_WIDTH: u32 = STUDIO_ENV_STANDARD_WIDTH;
    /// Prefilter mips of the standard chain: roughness 0, 1/3, 2/3, 1.
    const MIPS: usize = 4;

    fn gradient() -> StudioGradient {
        StudioGradient::new(
            [0.2 * S, 0.4 * S, 0.9 * S],
            [S, 0.9 * S, 0.8 * S],
            [0.1 * S, 0.1 * S, 0.1 * S],
        )
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() <= 2e-3 * b.abs().max(1.0)
    }

    #[test]
    fn the_ramp_runs_horizon_to_zenith_with_a_flat_ground() {
        let g = gradient();
        assert_eq!(g.radiance([1.0, 0.0, 0.0]), g.horizon);
        let top = g.radiance([0.0, 1.0, 0.0]);
        assert!((0..3).all(|c| near(top[c], g.zenith[c])), "{top:?}");
        let mid = g.radiance([0.0, 0.5, 0.866]);
        assert!(near(mid[0], 0.6 * S) && near(mid[2], 0.85 * S), "{mid:?}");
        assert_eq!(g.radiance([0.0, -0.01, 1.0]), g.ground);
        assert_eq!(g.radiance([0.0, -1.0, 0.0]), g.ground);
    }

    #[test]
    fn the_ramp_is_monotonic_and_falloff_bends_it() {
        let g = gradient();
        let mut last = g.horizon[2];
        for i in 1..=20 {
            let y = i as f32 / 20.0;
            let b = g.radiance([(1.0 - y * y).sqrt(), y, 0.0])[2];
            assert!(b >= last, "y {y}");
            last = b;
        }
        let slow = StudioGradient { falloff: 4.0, ..g }.radiance([0.0, 0.5, 0.0]);
        let fast = StudioGradient { falloff: 0.25, ..g }.radiance([0.0, 0.5, 0.0]);
        // The blue channel rises toward the zenith: a slow ramp is still near the horizon value.
        assert!(slow[2] < g.radiance([0.0, 0.5, 0.0])[2] && g.radiance([0.0, 0.5, 0.0])[2] < fast[2]);
    }

    #[test]
    fn validation_refuses_bad_values() {
        let mut g = gradient();
        assert!(g.validate().is_ok());
        g.zenith[1] = f32::NAN;
        assert_eq!(g.validate(), Err(StudioEnvError::BadRadiance("zenith")));
        let mut g = gradient();
        g.ground[0] = -0.1;
        assert_eq!(g.validate(), Err(StudioEnvError::BadRadiance("ground")));
        for falloff in [0.0, 100.0, f32::INFINITY, f32::NAN] {
            assert_eq!(
                StudioGradient {
                    falloff,
                    ..gradient()
                }
                .validate(),
                Err(StudioEnvError::BadFalloff)
            );
        }
    }

    #[test]
    fn the_gradient_key_follows_every_parameter() {
        let g = gradient();
        assert_eq!(g.cache_key(), gradient().cache_key());
        // The sign of zero does not change the bake, so it does not change the key.
        let z = StudioGradient::new([0.0, 0.4, 0.9], g.horizon, g.ground);
        let nz = StudioGradient::new([-0.0, 0.4, 0.9], g.horizon, g.ground);
        assert_eq!(z.cache_key(), nz.cache_key());
        let mut keys = vec![g.cache_key()];
        for tweak in 0..4 {
            let mut t = g;
            match tweak {
                0 => t.zenith[2] += 0.01,
                1 => t.horizon[0] -= 0.01,
                2 => t.ground[1] += 0.01,
                _ => t.falloff = 2.0,
            }
            keys.push(t.cache_key());
        }
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 5);
    }

    #[test]
    fn the_equirect_key_follows_size_and_texels_and_differs_from_a_gradient_key() {
        let texels = vec![[0.5, 0.5, 0.5]; 8];
        let a = StudioEquirect::new(4, 2, texels.clone()).expect("image");
        let b = StudioEquirect::new(4, 2, texels.clone()).expect("image");
        assert_eq!(a.cache_key(), b.cache_key());
        let wide = StudioEquirect::new(8, 1, texels.clone()).expect("image");
        assert_ne!(a.cache_key(), wide.cache_key());
        let mut changed = texels;
        changed[7][2] = 0.51;
        let c = StudioEquirect::new(4, 2, changed).expect("image");
        assert_ne!(a.cache_key(), c.cache_key());
        assert_ne!(a.cache_key(), gradient().cache_key());
    }

    #[test]
    fn equirect_validation_refuses_bad_images() {
        let ok = vec![[1.0; 3]; 8];
        assert!(matches!(
            StudioEquirect::new(1, 8, ok.clone()),
            Err(StudioEnvError::BadSize { .. })
        ));
        assert!(matches!(
            StudioEquirect::new(4, 0, vec![]),
            Err(StudioEnvError::BadSize { .. })
        ));
        assert!(matches!(
            StudioEquirect::new(u32::MAX, u32::MAX, vec![]),
            Err(StudioEnvError::BadSize { .. })
        ));
        assert!(matches!(
            StudioEquirect::new(4, 3, ok.clone()),
            Err(StudioEnvError::TexelCount {
                expected: 12,
                got: 8,
                ..
            })
        ));
        let mut bad = ok;
        bad[3][1] = f32::INFINITY;
        assert!(matches!(
            StudioEquirect::new(4, 2, bad),
            Err(StudioEnvError::BadRadiance(_))
        ));
    }

    fn texel16(bytes: &[u8], index: usize) -> f32 {
        // Decodes the first (red) half float of texel `index`.
        let bits = u16::from_le_bytes([bytes[index * 8], bytes[index * 8 + 1]]);
        half_to_f32(bits)
    }

    fn half_to_f32(bits: u16) -> f32 {
        let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
        let exp = i32::from((bits >> 10) & 0x1f);
        let mant = f32::from(bits & 0x3ff);
        match exp {
            0 => sign * mant * 2.0f32.powi(-24),
            0x1f => sign * f32::INFINITY,
            _ => sign * (1.0 + mant / 1024.0) * 2.0f32.powi(exp - 15),
        }
    }

    #[test]
    fn the_gradient_bake_is_deterministic_and_keeps_the_mirror_mip_exact() {
        let g = gradient();
        let a = g.bake(BASE_WIDTH);
        let b = g.bake(BASE_WIDTH);
        let (ha, hb) = (a.half.as_ref().expect("half"), b.half.as_ref().expect("half"));
        assert_eq!(ha.irradiance, hb.irradiance);
        assert_eq!(ha.prefilter, hb.prefilter);
        assert_eq!(a.prefilter.len(), MIPS);
        // Mirror mip: texel (0, 0) faces nearly straight up, the last row nearly straight down.
        let top = texel16(&ha.prefilter[0], 0) * SKY_IBL_SCALE;
        let bottom = texel16(&ha.prefilter[0], (BASE_WIDTH * (BASE_WIDTH / 2 - 1)) as usize) * SKY_IBL_SCALE;
        assert!((top - g.zenith[0]).abs() < 0.01 * g.horizon[0], "top {top}");
        assert!(near(bottom, g.ground[0]), "bottom {bottom}");
    }

    #[test]
    fn irradiance_of_a_gradient_lies_between_its_extremes_and_rises_with_the_normal() {
        let g = gradient();
        let maps = g.bake(BASE_WIDTH);
        let half = maps.half.as_ref().expect("half");
        let w = 64usize;
        let up = texel16(&half.irradiance, 0) * SKY_IBL_SCALE;
        let down = texel16(&half.irradiance, w * 31) * SKY_IBL_SCALE;
        let side = texel16(&half.irradiance, w * 16) * SKY_IBL_SCALE;
        assert!(down < side && side < up, "down {down} side {side} up {up}");
        assert!(down >= g.ground[0] * 0.998 && up <= g.horizon[0] * 1.002);
    }

    #[test]
    fn a_uniform_equirect_bakes_to_itself_above_the_8_bit_clip() {
        // 3 × the IBL scale in every channel: an 8-bit map would clip at 1; the half path holds it.
        let v = 3.0 * SKY_IBL_SCALE;
        let image = StudioEquirect::new(16, 8, vec![[v, v, v]; 128]).expect("image");
        let maps = image.bake(BASE_WIDTH);
        let half = maps.half.as_ref().expect("half");
        for bytes in std::iter::once(&half.irradiance).chain(half.prefilter.iter()) {
            for i in [0usize, 5, bytes.len() / 16] {
                let got = texel16(bytes, i) * SKY_IBL_SCALE;
                assert!((got - v).abs() < 0.002 * v, "{got} vs {v}");
            }
        }
    }

    #[test]
    fn an_equirect_gradient_matches_the_analytic_gradient_at_the_mirror_mip() {
        // Rasterize the gradient into a 128 × 64 image and bake it: the mirror mip should land
        // within a few percent of the analytic bake away from the horizon step.
        let g = StudioGradient::new([0.1 * S, 0.3 * S, 0.8 * S], [0.8 * S; 3], [0.2 * S; 3]);
        let (w, h) = (128u32, 64u32);
        let mut texels = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let d = dir_from_equirect((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
                texels.push(g.radiance(d));
            }
        }
        let from_image = StudioEquirect::new(w, h, texels).expect("image").bake(BASE_WIDTH);
        let analytic = g.bake(BASE_WIDTH);
        let (fi, an) = (
            from_image.half.as_ref().expect("half"),
            analytic.half.as_ref().expect("half"),
        );
        let wide = BASE_WIDTH as usize;
        for row in [8usize, 20, 40, 100, 120] {
            let i = row * wide + 17;
            let (a, b) = (texel16(&fi.prefilter[0], i), texel16(&an.prefilter[0], i));
            assert!((a - b).abs() <= 0.06 * b, "row {row}: image {a} analytic {b}");
        }
    }

    #[test]
    fn resample_averages_when_shrinking_and_keeps_a_constant() {
        let src = Level {
            width: 4,
            height: 2,
            texels: vec![
                [1.0; 3], [3.0; 3], [5.0; 3], [7.0; 3], [1.0; 3], [3.0; 3], [5.0; 3], [7.0; 3],
            ],
        };
        let out = resample(&src, 2, 1);
        assert_eq!(out.texels, vec![[2.0; 3], [6.0; 3]]);
        let flat = Level {
            width: 2,
            height: 1,
            texels: vec![[0.7; 3]; 2],
        };
        let up = resample(&flat, 8, 4);
        assert!(up.texels.iter().all(|t| (t[0] - 0.7).abs() < 1e-6));
    }

    #[test]
    fn shrinking_bins_do_not_overlap() {
        // 5 -> 2: bins are [0,2) and [2,5). A hot texel at 2 belongs to the second bin only.
        let mut texels = vec![[0.0; 3]; 5];
        texels[2] = [10.0; 3];
        let src = Level {
            width: 5,
            height: 1,
            texels,
        };
        let out = resample(&src, 2, 1);
        assert_eq!(out.texels[0], [0.0; 3]);
        assert!(
            (out.texels[1][0] - 10.0 / 3.0).abs() < 1e-5,
            "{:?}",
            out.texels[1]
        );
    }
}
