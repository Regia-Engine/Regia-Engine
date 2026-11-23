// SPDX-License-Identifier: MPL-2.0
//! Raymarched volumetric clouds (`specs/26` § 4).
//!
//! A weather map (R coverage, G cloud type) says where clouds may form. The GPU march
//! (`shaders/clouds.wgsl`) erodes a tiling 3D Perlin–Worley base shape with a smaller
//! Worley detail texture inside that envelope, lights it with a short march toward the sun,
//! and composites the reduced-resolution result over the frame. A pixel's ray stops at the
//! farthest surface in its low-resolution footprint, and the upsample picks the low-resolution
//! texel whose scene distance matches that pixel, so a cloud in front of a mountain draws over
//! it and a closer peak stays in front. Cloud shadows project the weather column along the sun
//! onto the ground.
//!
//! - Schneider, "The Real-Time Volumetric Cloudscapes of Horizon: Zero Dawn", SIGGRAPH 2015,
//!   and GPU Pro 7 ch. 4: weather map, Perlin–Worley shape, Worley detail erosion,
//!   Beer–Powder lighting.
//! - Schneider & Vos, "Nubis: Authoring Real-Time Volumetric Cloudscapes", SIGGRAPH 2017:
//!   per-type height profiles (stratus, cumulus, towering).
//! - Worley, "A Cellular Texture Basis Function", SIGGRAPH 1996; Perlin, "Improving Noise",
//!   SIGGRAPH 2002: the noise bases, made tileable by wrapping the lattice.

use std::sync::OnceLock;

use regia_app::prelude::*;

/// Edge of the base-shape noise volume, texels.
pub const SHAPE_NOISE_SIZE: u32 = 32;
/// Edge of the detail noise volume, texels.
pub const DETAIL_NOISE_SIZE: u32 = 32;
/// Fraction of a weather column's optical depth that shadows the ground (the 3D noise fills
/// only part of a cloudy column and multiple scattering lightens the shadow). Matches
/// `COLUMN_FILL` in `clouds.wgsl`.
pub const COLUMN_FILL: f32 = 0.05;

/// View step at the start of a ray while the camera is inside the cloud layer (metres).
///
/// The step grows from here so the fade span finishes in the step budget. Matches the
/// `20.0` near-field cap of `small_step` in `clouds.wgsl`.
pub const CLOUD_NEAR_STEP_M: f32 = 20.0;

/// Aerial fade distance (metres). Written into `CloudGpu.ambient_top.w`.
///
/// The march clamps a shell to [`CLOUD_FADE_MULTIPLE`] times this (`t0 + fade * 1.5`
/// in `clouds.wgsl`). 45 km × 1.5 is the 67.5 km span the inside step schedule covers.
pub const CLOUD_FADE_M: f32 = 45_000.0;

/// Edge of the tileable curl field ([`cloud_curl`]), texels.
pub const CURL_SIZE: u32 = 128;

/// Curl displacement of the detail erosion, as a multiple of the detail tile
/// ([`CloudSettings::detail_scale_m`]). Cone light only.
pub const CLOUD_CURL_STRENGTH: f32 = 0.35;

/// World size of one curl tile, as a multiple of the detail tile.
pub const CLOUD_CURL_TILE: f32 = 4.0;

/// How strongly the in-scatter probability darkens thin edges and bases (0 off, 1 the
/// full Schneider & Vos 2017 term). Cone light only.
pub const CLOUD_DARK_EDGE: f32 = 0.5;

/// Length of the cone light march toward the sun as a fraction of the layer thickness; one
/// long tap reaches on to three times this. Cone light only.
pub const CLOUD_CONE_SPAN: f32 = 0.5;

/// How many fade distances the view march will walk. Matches `fade * 1.5` in `march`.
pub const CLOUD_FADE_MULTIPLE: f32 = 1.5;

/// Big-step length over the small step. Matches `clouds.wgsl` (`ss * 4.0`).
pub const CLOUD_BIG_STEP_RATIO: f32 = 4.0;

/// Empty small steps before the march returns to the big step. Matches `clouds.wgsl`.
pub const CLOUD_EMPTY_STEPS: u32 = 8;

/// Scene-distance sentinel for sky, and the clamp on stored distances (metres).
///
/// Half floats saturate at 65504, so a hit farther than this is stored as sky.
/// Matches `SKY_M` in `clouds.wgsl`.
pub const CLOUD_SKY_DISTANCE_M: f32 = 60_000.0;

/// Smallest view step when the camera is outside the layer (metres). Matches the `4.0`
/// floor of `small_step` in `clouds.wgsl`.
pub const CLOUD_FAR_STEP_FLOOR_M: f32 = 4.0;

/// Nearer and farther distances along a unit `dir` from `eye` to the sphere.
///
/// `None` only when the ray misses. Roots behind the ray are returned negative:
/// the sphere is there, and the ray does not meet it in front.
///
/// `center` is the planet centre `(0, −planet, 0)` and `radius` is the shell.
/// `|o|² − radius²` is expanded about the planet radius, so f32 never squares a
/// multi-megametre length (the product `(length(o) − r)(length(o) + r)` still
/// amplifies that length's half-metre ulp into metres of error on a long chord).
/// The quadratic uses the stable root from Haines, Günther and Akenine-Möller,
/// *Ray Tracing Gems*, chapter 7 (2019). `dir` is unit length. This is the CPU
/// mirror of `shell` in `clouds.wgsl`.
#[must_use]
pub fn shell_intersections(
    eye: [f32; 3],
    dir: [f32; 3],
    center: [f32; 3],
    radius: f32,
) -> Option<(f32, f32)> {
    let o = [eye[0] - center[0], eye[1] - center[1], eye[2] - center[2]];
    let b = o[0] * dir[0] + o[1] * dir[1] + o[2] * dir[2];
    // `planet = −center.y`. `excess` is the camera height above the origin.
    let planet = -center[1];
    let shell_alt = radius - planet;
    let excess = o[1] - planet;
    let horiz2 = o[0] * o[0] + o[2] * o[2];
    let c = horiz2 + 2.0 * planet * (excess - shell_alt) + excess * excess - shell_alt * shell_alt;
    let disc = b * b - c;
    if !disc.is_finite() || disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let sign_b = if b >= 0.0 { 1.0 } else { -1.0 };
    let q = -(b + sign_b * s);
    let (t_a, t_b) = if q.abs() < 1.0e-4 {
        (-b - s, -b + s)
    } else {
        (q, c / q)
    };
    Some((t_a.min(t_b), t_a.max(t_b)))
}

/// Sum of `1 + r + … + r^(n−1)`. The march's geometric schedule uses this, not
/// `(r^n − 1) / (r − 1)`, so a ratio near 1 does not cancel. `n` is at most
/// [`crate::CloudMarch::STEP_LIMIT`], the shader loop bound.
fn geometric_series_sum(r: f32, n: u32) -> f32 {
    let n = n.min(crate::CloudMarch::STEP_LIMIT);
    let mut sum = 0.0;
    let mut term = 1.0;
    for _ in 0..n {
        sum += term;
        term *= r;
    }
    sum
}

/// `r ≥ 1` such that `n` terms starting at 1 sum to `sum_units`.
///
/// A binary search of the partial sums, matching `inside_ratio` in `clouds.wgsl`.
/// Returns 1 when `n` steps of length 1 already cover `sum_units`.
fn geometric_ratio(sum_units: f32, n: u32) -> f32 {
    let n = n.clamp(1, crate::CloudMarch::STEP_LIMIT);
    if n <= 1 || sum_units <= n as f32 {
        return 1.0;
    }
    let mut lo = 1.0_f32;
    let mut hi = 2.0_f32;
    for _ in 0..8 {
        if geometric_series_sum(hi, n) >= sum_units {
            break;
        }
        hi *= 2.0;
    }
    for _ in 0..20 {
        let mid = 0.5 * (lo + hi);
        if geometric_series_sum(mid, n) < sum_units {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// View-march small step in metres. The big step is [`CLOUD_BIG_STEP_RATIO`] times this.
///
/// Outside the layer the step is a fraction of `(t1 − t0) / steps` that grows toward
/// the end of the segment and floors at [`CLOUD_FAR_STEP_FLOOR_M`]. Inside the layer
/// it starts at [`CLOUD_NEAR_STEP_M`] and grows geometrically so `steps` small steps
/// sum to the segment. A 67.5 km fade span (`fade * 1.5`) therefore finishes in 64
/// steps, and a short segment stays at the 20 m cap. Matches `small_step` in
/// `clouds.wgsl`.
#[must_use]
pub fn cloud_small_step(t: f32, t0: f32, t1: f32, steps: u32, eye_inside: bool) -> f32 {
    let span = (t1 - t0).max(1.0);
    let n = steps.max(1);
    let along = (t - t0).clamp(0.0, span);
    if eye_inside {
        let r = if span <= CLOUD_NEAR_STEP_M * n as f32 {
            1.0
        } else {
            geometric_ratio(span / CLOUD_NEAR_STEP_M, n)
        };
        return CLOUD_NEAR_STEP_M + (r - 1.0) * along;
    }
    let cover = span / n as f32;
    let u = along / span;
    let ramp = 0.35 + (1.8 - 0.35) * (u * u);
    let grown = cover * ramp;
    let cap = (cover * 2.0).max(CLOUD_FAR_STEP_FLOOR_M);
    grown.clamp(CLOUD_FAR_STEP_FLOOR_M, cap)
}

/// Edge of the reprojection block, in low-resolution texels. Each frame marches one texel
/// of every `CLOUD_BLOCK × CLOUD_BLOCK` block, so a texel refreshes every 16 frames.
pub const CLOUD_BLOCK: u32 = 4;

/// Weight of a fresh march sample against its history on a still texel (the texel the frame
/// just marched): one half averages the start jitter over refreshes. A texel that moved, was
/// resampled or reads as stale takes the fresh sample whole. Matches `FRESH_WEIGHT` in
/// `clouds.wgsl`.
pub const CLOUD_FRESH_WEIGHT: f32 = 0.5;
const _: () = assert!(CLOUD_FRESH_WEIGHT > 0.0 && CLOUD_FRESH_WEIGHT <= 1.0);

/// 4×4 Bayer matrix: the order in which a block's texels are marched.
const BAYER_4X4: [[u32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Sub-position `[x, y]` inside a [`CLOUD_BLOCK`] block that `frame` marches.
///
/// Frame `k` marches the texel whose 4×4 Bayer rank is `k % 16`, so consecutive frames
/// are spread across the block rather than marching a row at a time.
#[must_use]
pub fn cloud_bayer_offset(frame: u32) -> [u32; 2] {
    let rank = frame % 16;
    for (y, row) in BAYER_4X4.iter().enumerate() {
        for (x, value) in row.iter().enumerate() {
            if *value == rank {
                return [x as u32, y as u32];
            }
        }
    }
    [0, 0]
}

/// Fresh-target size for a `low` march target: one texel per [`CLOUD_BLOCK`] block.
#[must_use]
pub fn cloud_fresh_extent(low: [u32; 2]) -> [u32; 2] {
    [
        low[0].div_ceil(CLOUD_BLOCK).max(1),
        low[1].div_ceil(CLOUD_BLOCK).max(1),
    ]
}

/// The low-resolution texel that fresh texel `block` marches with sub-position `sub`.
///
/// A partial block at the right or bottom edge clamps to its last texel, so every texel
/// is still marched at least once per 16 frames. Matches `fresh_low` in `clouds.wgsl`.
#[must_use]
pub fn cloud_fresh_texel(block: [u32; 2], sub: [u32; 2], low: [u32; 2]) -> [u32; 2] {
    [
        (block[0] * CLOUD_BLOCK + sub[0]).min(low[0].max(1) - 1),
        (block[1] * CLOUD_BLOCK + sub[1]).min(low[1].max(1) - 1),
    ]
}

/// Single-scattering albedo of the view march (`ambient_bottom.w` in `clouds.wgsl`).
pub const CLOUD_MARCH_ALBEDO: f32 = 0.99;

/// An upper bound of the normalised density the layer of `settings` can reach anywhere (a majorant,
/// ADR-2127): `density_base` in `regia::cloud_density` is at most the column's cover and the
/// detail erosion only lowers it, and the cover is at most what the largest weather texel, raised
/// by the full edge breakup, gives. 0 when the map is empty.
#[must_use]
pub fn cloud_density_bound(settings: &CloudSettings, weather: &WeatherMap) -> f32 {
    let peak = weather
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|t| t[0])
        .max()
        .map_or(0.0, |r| f32::from(r) / 255.0);
    if peak <= 0.0 {
        return 0.0;
    }
    let raised = (peak + settings.look.sanitized().breakup).clamp(0.0, 1.0);
    (raised * (0.75 + 0.4 * settings.coverage.clamp(0.0, 1.0))).clamp(0.0, 1.0)
}

/// How the weather map is filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloudPattern {
    /// Tileable cellular noise thresholded so exactly `coverage` of the texels are cloudy.
    #[default]
    Coverage,
    /// `x >= 0` cloudy and `x < 0` clear, for a shadow that splits the ground in half.
    SplitHemisphere,
    /// Every texel stores the same coverage. The weather has no edges.
    Constant,
}

/// One volumetric cloud layer.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct CloudSettings {
    /// Fraction of the sky the layer covers, `0..1`. Zero skips the march.
    pub coverage: f32,
    /// Cloud type: 0 stratus, 0.5 cumulus, 1 towering cumulus.
    pub cloud_type: f32,
    /// Extinction inside a full-density cloud (1/metres). Cumulus ≈ 0.02–0.1.
    pub density: f32,
    /// Bottom of the layer above the ground (metres).
    pub altitude_bottom: f32,
    /// Top of the layer (metres). Must be above the bottom.
    pub altitude_top: f32,
    /// Horizontal wind (metres/second) on X and Z.
    pub wind_m_per_s: [f32; 2],
    /// Seconds of wind applied to the weather and noise.
    pub time_s: f32,
    /// Wind drift (metres, world X and Z) added to `wind_m_per_s × time_s` (ADR-429). A weather
    /// driver that changes the wind integrates the drift and keeps it here, so a new wind
    /// carries the clouds on from where they are instead of jumping by `Δwind × time_s`.
    /// Zero (the default) leaves the drift `wind × time` exactly as before.
    pub drift_offset_m: [f32; 2],
    /// World XZ extent one weather tile covers (metres).
    pub domain_m: f32,
    /// Weather texels on each side.
    pub map_resolution: u32,
    /// World size of one tile of the base-shape noise (metres).
    pub shape_scale_m: f32,
    /// World size of one tile of the detail noise (metres).
    pub detail_scale_m: f32,
    /// View-ray march steps.
    pub steps: u32,
    /// The march runs at `1 / resolution_divisor` of the frame on each axis.
    pub resolution_divisor: u32,
    /// Fill pattern.
    pub pattern: CloudPattern,
    /// Seed of the weather noise.
    pub seed: u32,
    /// The cloud transmittance map darkens the sun term of meshes and terrain (ADR-320).
    /// Off keeps the screen-space shadow multiply of the whole shaded pixel.
    pub shadow_map: bool,
    /// Froxel fog reads the cloud transmittance map, so sunlit air shows god rays through
    /// gaps in the layer (ADR-320). Builds the map even with [`Self::shadow_map`] off.
    pub fog_coupling: bool,
    /// The sky light (IBL) dims and greys with the layer's mean coverage and column depth
    /// (ADR-321). Earth only: airless and Mars skies are unchanged.
    pub overcast_ibl: bool,
    /// Texels on each side of the cloud transmittance map.
    pub shadow_map_size: u32,
    /// World edge of the cloud transmittance map (metres), centred on the camera.
    pub shadow_map_extent_m: f32,
    /// Draw the cloud transmittance map over the frame: the sun's transmittance at every
    /// surface and, on the sky, where the view ray meets the layer base. The
    /// `DebugViewMode::CloudShadowMap` debug view sets it for its frames. Builds the map.
    pub shadow_map_debug: bool,
    /// Diffuse multiple scattering, erosion, edge breakup and the high cirrus sheet
    /// (ADR-1408). [`CloudLook::LEGACY`](crate::CloudLook::LEGACY), the default, draws the layer
    /// as before; the [`CloudGenus`](crate::CloudGenus) presets set it.
    pub look: crate::cloud_types::CloudLook,
}

impl Default for CloudSettings {
    fn default() -> Self {
        Self {
            coverage: 0.0,
            cloud_type: 0.5,
            density: 0.04,
            altitude_bottom: 1500.0,
            altitude_top: 4000.0,
            wind_m_per_s: [6.0, 2.0],
            time_s: 0.0,
            drift_offset_m: [0.0, 0.0],
            domain_m: 24_000.0,
            map_resolution: 128,
            shape_scale_m: 2600.0,
            detail_scale_m: 520.0,
            steps: 64,
            resolution_divisor: 2,
            pattern: CloudPattern::Coverage,
            seed: 7,
            shadow_map: false,
            fog_coupling: false,
            overcast_ibl: false,
            shadow_map_size: CLOUD_SHADOW_MAP_SIZE,
            shadow_map_extent_m: CLOUD_SHADOW_MAP_EXTENT_M,
            shadow_map_debug: false,
            look: crate::cloud_types::CloudLook::LEGACY,
        }
    }
}

impl CloudSettings {
    /// Wind drift of the weather and noise (metres, world X and Z):
    /// `wind_m_per_s × time_s + drift_offset_m` (ADR-429).
    #[must_use]
    pub fn drift_m(&self) -> [f32; 2] {
        let [ox, oz] = self.drift_offset_m;
        let x = self.wind_m_per_s[0] * self.time_s;
        let z = self.wind_m_per_s[1] * self.time_s;
        if ox == 0.0 && oz == 0.0 {
            return [x, z];
        }
        [x + ox, z + oz]
    }

    /// The march uniform's `wind` row: `[wind x, wind z, time, weather tile]`, which the shader
    /// multiplies into the drift. With a [`Self::drift_offset_m`] the row carries the whole drift
    /// premultiplied (`[drift x, drift z, 1, tile]`); without one it is the row the shader always
    /// read, so frames without an offset are unchanged bit for bit (ADR-429).
    #[must_use]
    pub fn wind_uniform(&self) -> [f32; 4] {
        let domain = self.domain_m.max(1.0);
        if self.drift_offset_m == [0.0, 0.0] {
            return [self.wind_m_per_s[0], self.wind_m_per_s[1], self.time_s, domain];
        }
        let [x, z] = self.drift_m();
        [x, z, 1.0, domain]
    }

    /// The values a weather change moves ([`CloudWeatherSample`]).
    #[must_use]
    pub fn weather_sample(&self) -> CloudWeatherSample {
        CloudWeatherSample {
            coverage: self.coverage,
            cloud_type: self.cloud_type,
            density: self.density,
        }
    }

    /// The cloud transmittance map is built: [`Self::shadow_map`], [`Self::fog_coupling`] or
    /// [`Self::shadow_map_debug`].
    #[must_use]
    pub fn wants_shadow_map(&self) -> bool {
        self.shadow_map || self.fog_coupling || self.shadow_map_debug
    }

    /// The map's texels per side, clamped to `64..=2048`.
    #[must_use]
    pub fn shadow_map_texels(&self) -> u32 {
        self.shadow_map_size.clamp(64, 2048)
    }

    /// The map's world edge (metres), clamped to `1 km..=200 km`.
    #[must_use]
    pub fn shadow_map_extent(&self) -> f32 {
        if self.shadow_map_extent_m.is_finite() {
            self.shadow_map_extent_m.clamp(1_000.0, 200_000.0)
        } else {
            CLOUD_SHADOW_MAP_EXTENT_M
        }
    }

    /// A still cumulus layer with a known coverage, for the goldens.
    #[must_use]
    pub fn coverage_layer(coverage: f32) -> Self {
        Self {
            coverage: coverage.clamp(0.0, 1.0),
            wind_m_per_s: [0.0, 0.0],
            ..Self::default()
        }
    }

    /// Half the ground in cloud shadow, half clear.
    #[must_use]
    pub fn split_shadow() -> Self {
        Self {
            coverage: 0.5,
            pattern: CloudPattern::SplitHemisphere,
            ..Self::coverage_layer(0.5)
        }
    }
}

/// RGBA8 weather map. R is coverage, G cloud type, A 255.
#[derive(Clone, Debug)]
pub struct WeatherMap {
    /// Width and height in texels.
    pub resolution: u32,
    /// Row-major RGBA8.
    pub rgba: Vec<u8>,
    /// Fraction of texels whose coverage is at least one half.
    pub filled: f32,
}

impl WeatherMap {
    /// Bilinear coverage at a world XZ position, including wind scroll and tile wrap (the GPU
    /// sampler's filtering).
    #[must_use]
    pub fn sample(&self, xz: [f32; 2], settings: &CloudSettings) -> f32 {
        let n = self.resolution;
        if n == 0 {
            return 0.0;
        }
        let domain = settings.domain_m.max(1.0);
        let drift = settings.drift_m();
        let u = (xz[0] - drift[0]) / domain + 0.5;
        let v = (xz[1] - drift[1]) / domain + 0.5;
        let fx = u * n as f32 - 0.5;
        let fy = v * n as f32 - 0.5;
        let x0 = fx.floor();
        let y0 = fy.floor();
        let tx = fx - x0;
        let ty = fy - y0;
        let at = |x: f32, y: f32| {
            let xi = (x as i64).rem_euclid(i64::from(n)) as u32;
            let yi = (y as i64).rem_euclid(i64::from(n)) as u32;
            f32::from(self.rgba[((yi * n + xi) * 4) as usize]) / 255.0
        };
        let top = at(x0, y0) * (1.0 - tx) + at(x0 + 1.0, y0) * tx;
        let bottom = at(x0, y0 + 1.0) * (1.0 - tx) + at(x0 + 1.0, y0 + 1.0) * tx;
        top * (1.0 - ty) + bottom * ty
    }
}

/// Builds the weather map. [`CloudPattern::Coverage`] thresholds a tileable cellular noise
/// field at the coverage percentile, so exactly that fraction of texels is at least half
/// covered, with soft edges either side.
#[must_use]
pub fn build_weather_map(settings: &CloudSettings) -> WeatherMap {
    let n = map_n(settings);
    let field = match settings.pattern {
        CloudPattern::Coverage => coverage_field(n, settings.seed),
        CloudPattern::SplitHemisphere | CloudPattern::Constant => Vec::new(),
    };
    pack_weather_map(settings, &field, None, &type_wobble(n, settings.seed))
}

/// The coverage-independent part of a [`CloudPattern::Coverage`] weather map (ADR-429): the
/// cellular noise field, its sorted copy (the coverage percentile is one index into it) and the
/// cloud-type wobble. It depends only on the map resolution and seed, so a weather change that
/// moves coverage or cloud type re-thresholds it in one pass over the texels instead of
/// rebuilding the noise and sorting it again.
#[derive(Debug, Clone)]
pub struct WeatherField {
    resolution: u32,
    seed: u32,
    field: Vec<f32>,
    sorted: Vec<f32>,
    wobble: Vec<f32>,
}

impl WeatherField {
    /// Builds the field for `settings`' map resolution and seed.
    #[must_use]
    pub fn new(settings: &CloudSettings) -> Self {
        let _span = tracing::info_span!("weather_field_build").entered();
        let n = map_n(settings);
        let field = coverage_field(n, settings.seed);
        let mut sorted = field.clone();
        sorted.sort_by(f32::total_cmp);
        count_cloud_work(|c| c.weather_fields += 1);
        Self {
            resolution: n,
            seed: settings.seed,
            field,
            sorted,
            wobble: type_wobble(n, settings.seed),
        }
    }

    /// Whether this field is the one `settings` would build (same resolution and seed).
    #[must_use]
    pub fn matches(&self, settings: &CloudSettings) -> bool {
        self.resolution == map_n(settings) && self.seed == settings.seed
    }

    /// The weather map for `settings`, equal byte for byte to [`build_weather_map`]. Patterns
    /// other than [`CloudPattern::Coverage`], or settings the field does not
    /// [match](Self::matches), build it from scratch.
    #[must_use]
    pub fn weather_map(&self, settings: &CloudSettings) -> WeatherMap {
        if settings.pattern != CloudPattern::Coverage || !self.matches(settings) {
            return build_weather_map(settings);
        }
        pack_weather_map(settings, &self.field, Some(&self.sorted), &self.wobble)
    }
}

/// Largest weather-map edge in texels; a larger `map_resolution` is clamped (a `u32` square
/// would wrap and index past an empty buffer).
const MAX_MAP_RESOLUTION: u32 = 4096;

/// The clamped weather-map edge for `settings`.
fn map_n(settings: &CloudSettings) -> u32 {
    settings.map_resolution.clamp(1, MAX_MAP_RESOLUTION)
}

/// Texels in an `n`×`n` map, counted in `usize`.
fn texel_count(n: u32) -> usize {
    n as usize * n as usize
}

/// The tileable cellular noise field the [`CloudPattern::Coverage`] map thresholds.
fn coverage_field(n: u32, seed: u32) -> Vec<f32> {
    let mut field = vec![0.0_f32; texel_count(n)];
    for y in 0..n {
        for x in 0..n {
            let u = (x as f32 + 0.5) / n as f32;
            let v = (y as f32 + 0.5) / n as f32;
            let fbm = fbm_2d(u, v, 7.0, 4, seed);
            let cells = 1.0 - worley_2d(u, v, 9, seed ^ 0x9e37).min(1.0);
            field[y as usize * n as usize + x as usize] = 0.6 * fbm + 0.4 * cells;
        }
    }
    field
}

/// Per-texel cloud-type wobble, `−0.5..0.5`.
fn type_wobble(n: u32, seed: u32) -> Vec<f32> {
    let mut wobble = vec![0.0_f32; texel_count(n)];
    for y in 0..n {
        for x in 0..n {
            let u = (x as f32 + 0.5) / n as f32;
            let v = (y as f32 + 0.5) / n as f32;
            wobble[y as usize * n as usize + x as usize] =
                value_noise_2d(u * 3.0, v * 3.0, 3, seed ^ 0x5bd1) - 0.5;
        }
    }
    wobble
}

/// Thresholds `field` (Coverage pattern; `sorted` is its sorted copy when cached) or fills the
/// other patterns, and packs the RGBA8 map.
fn pack_weather_map(
    settings: &CloudSettings,
    field: &[f32],
    sorted: Option<&[f32]>,
    wobble: &[f32],
) -> WeatherMap {
    let n = map_n(settings);
    let count = texel_count(n);
    let coverage = settings.coverage.clamp(0.0, 1.0);
    let mut cover = vec![0.0_f32; count];
    match settings.pattern {
        CloudPattern::Constant => cover.fill(coverage),
        CloudPattern::SplitHemisphere => {
            let domain = settings.domain_m.max(1.0);
            for y in 0..n {
                for x in 0..n {
                    let world_x = ((x as f32 + 0.5) / n as f32 - 0.5) * domain;
                    cover[(y * n + x) as usize] = if world_x >= 0.0 { 1.0 } else { 0.0 };
                }
            }
        }
        CloudPattern::Coverage => {
            if coverage >= 1.0 {
                cover.fill(1.0);
            } else if coverage > 0.0 && field.len() == count {
                let sorted: std::borrow::Cow<'_, [f32]> = match sorted.filter(|s| s.len() == count) {
                    Some(sorted) => std::borrow::Cow::Borrowed(sorted),
                    None => {
                        let mut copy = field.to_vec();
                        copy.sort_by(f32::total_cmp);
                        std::borrow::Cow::Owned(copy)
                    }
                };
                let want = (coverage * count as f32).round() as usize;
                let threshold = if want == 0 {
                    f32::INFINITY
                } else {
                    sorted[count - want]
                };
                let soft = 0.08;
                for (c, f) in cover.iter_mut().zip(field) {
                    // At least 0.5 exactly where `f >= threshold`.
                    *c = if *f >= threshold {
                        0.5 + 0.5 * ((*f - threshold) / soft).min(1.0)
                    } else {
                        0.5 * (1.0 - ((threshold - *f) / soft).min(1.0))
                    };
                }
            }
        }
    }
    let mut rgba = vec![0u8; count * 4];
    let mut filled = 0usize;
    for (i, (c, w)) in cover.iter().zip(wobble).enumerate() {
        let r = to_u8(*c);
        if r >= 128 {
            filled += 1;
        }
        rgba[i * 4] = r;
        rgba[i * 4 + 1] = to_u8((settings.cloud_type + 0.35 * w).clamp(0.0, 1.0));
        rgba[i * 4 + 2] = r;
        rgba[i * 4 + 3] = 255;
    }
    count_cloud_work(|c| c.weather_maps += 1);
    WeatherMap {
        resolution: n,
        rgba,
        filled: filled as f32 / count as f32,
    }
}

/// Tiling RGBA8 noise volumes for the march, built once per process.
#[derive(Debug)]
pub struct CloudNoise {
    /// `SHAPE_NOISE_SIZE³` texels: R Perlin–Worley, GBA Worley FBM at rising frequency.
    pub shape: Vec<u8>,
    /// `DETAIL_NOISE_SIZE³` texels: RGB Worley FBM at rising frequency.
    pub detail: Vec<u8>,
}

/// The shared [`CloudNoise`] volumes.
pub fn cloud_noise() -> &'static CloudNoise {
    static NOISE: OnceLock<CloudNoise> = OnceLock::new();
    NOISE.get_or_init(|| {
        let _span = tracing::info_span!("cloud_noise_build").entered();
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::CLOUD_NOISE);
        let n = SHAPE_NOISE_SIZE;
        let mut shape = vec![0u8; (n * n * n * 4) as usize];
        for z in 0..n {
            for y in 0..n {
                for x in 0..n {
                    let p = [
                        (x as f32 + 0.5) / n as f32,
                        (y as f32 + 0.5) / n as f32,
                        (z as f32 + 0.5) / n as f32,
                    ];
                    let perlin = (perlin_fbm_3d(p, 4, 3, 11) * 0.5 + 0.5).clamp(0.0, 1.0);
                    let w4 = worley_fbm_3d(p, 4, 21);
                    // Perlin–Worley: Perlin remapped into the Worley floor (Schneider 2015).
                    let pw = remap(perlin, 0.0, 1.0, w4, 1.0).clamp(0.0, 1.0);
                    let i = (((z * n + y) * n + x) * 4) as usize;
                    shape[i] = to_u8(pw);
                    shape[i + 1] = to_u8(w4);
                    shape[i + 2] = to_u8(worley_fbm_3d(p, 8, 31));
                    shape[i + 3] = to_u8(worley_fbm_3d(p, 16, 41));
                }
            }
        }
        let n = DETAIL_NOISE_SIZE;
        let mut detail = vec![0u8; (n * n * n * 4) as usize];
        for z in 0..n {
            for y in 0..n {
                for x in 0..n {
                    let p = [
                        (x as f32 + 0.5) / n as f32,
                        (y as f32 + 0.5) / n as f32,
                        (z as f32 + 0.5) / n as f32,
                    ];
                    let i = (((z * n + y) * n + x) * 4) as usize;
                    detail[i] = to_u8(worley_fbm_3d(p, 2, 51));
                    detail[i + 1] = to_u8(worley_fbm_3d(p, 4, 61));
                    detail[i + 2] = to_u8(worley_fbm_3d(p, 8, 71));
                    detail[i + 3] = 255;
                }
            }
        }
        CloudNoise { shape, detail }
    })
}

/// Tileable 2D curl field: `CURL_SIZE²` RGBA8 texels, RG the curl of a periodic scalar
/// potential (Bridson, Hourihan, Nordenstam, "Curl-Noise for Procedural Fluid Flow",
/// SIGGRAPH 2007) mapped from −1..1 to 0..255, B and A unused. `curl = (∂ψ/∂y, −∂ψ/∂x)` by
/// central differences, so the field is divergence-free and swirls without bunching.
pub fn cloud_curl() -> &'static [u8] {
    static CURL: OnceLock<Vec<u8>> = OnceLock::new();
    CURL.get_or_init(|| {
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::CLOUD_NOISE);
        let field = curl_field(CURL_SIZE);
        let peak = field
            .iter()
            .fold(1.0e-6_f32, |m, v| m.max(v[0].abs()).max(v[1].abs()));
        let mut rgba = vec![0u8; field.len() * 4];
        for (i, v) in field.iter().enumerate() {
            rgba[i * 4] = to_u8(v[0] / peak * 0.5 + 0.5);
            rgba[i * 4 + 1] = to_u8(v[1] / peak * 0.5 + 0.5);
            rgba[i * 4 + 3] = 255;
        }
        rgba
    })
}

/// The unnormalised curl of a periodic potential on an `n × n` grid (row-major).
fn curl_field(n: u32) -> Vec<[f32; 2]> {
    let n_i = n as i32;
    let mut psi = vec![0.0f32; texel_count(n)];
    for y in 0..n {
        for x in 0..n {
            let (u, v) = ((x as f32 + 0.5) / n as f32, (y as f32 + 0.5) / n as f32);
            psi[(y * n + x) as usize] = fbm_2d(u, v, 4.0, 3, 811);
        }
    }
    let at = |x: i32, y: i32| psi[(y.rem_euclid(n_i) * n_i + x.rem_euclid(n_i)) as usize];
    let mut field = Vec::with_capacity(texel_count(n));
    for y in 0..n_i {
        for x in 0..n_i {
            let d_dx = 0.5 * (at(x + 1, y) - at(x - 1, y));
            let d_dy = 0.5 * (at(x, y + 1) - at(x, y - 1));
            field.push([d_dy, -d_dx]);
        }
    }
    field
}

/// Nubis height profile for a layer fraction `h` (0 base, 1 top) and type `t`
/// (0 stratus, 0.5 cumulus, 1 towering). Matches `type_gradient` in `clouds.wgsl`.
#[must_use]
pub fn type_gradient(h: f32, t: f32) -> f32 {
    let sat = |v: f32| v.clamp(0.0, 1.0);
    let stratus = sat(remap(h, 0.0, 0.08, 0.0, 1.0)) * sat(remap(h, 0.18, 0.3, 1.0, 0.0));
    let cumulus = sat(remap(h, 0.0, 0.2, 0.0, 1.0)) * sat(remap(h, 0.35, 0.7, 1.0, 0.0));
    let tower = sat(remap(h, 0.0, 0.1, 0.0, 1.0)) * sat(remap(h, 0.7, 1.0, 1.0, 0.0));
    let a = stratus + (cumulus - stratus) * sat(t * 2.0);
    a + (tower - a) * sat(t * 2.0 - 1.0)
}

/// Vertical profile of a cumulus layer at height `y` between `bottom` and `top` (metres).
#[must_use]
pub fn height_gradient(y: f32, bottom: f32, top: f32) -> f32 {
    let span = (top - bottom).max(0.1);
    type_gradient(((y - bottom) / span).clamp(0.0, 1.0), 0.5)
}

/// Optical depth of the weather column at `xz` (dimensionless), before the `COLUMN_FILL` shadow fraction.
#[must_use]
pub fn column_optical_depth(xz: [f32; 2], settings: &CloudSettings, weather: &WeatherMap) -> f32 {
    let raw = weather.sample(xz, settings);
    let cover = raw * (0.75 + 0.4 * settings.coverage.clamp(0.0, 1.0));
    if cover <= 0.0 {
        return 0.0;
    }
    let thickness = (settings.altitude_top - settings.altitude_bottom).max(0.0);
    let steps = 8;
    let mut sum = 0.0;
    for i in 0..steps {
        sum += type_gradient((i as f32 + 0.5) / steps as f32, settings.cloud_type);
    }
    // Thin weather erodes to sparse wisps, so a column's mean density grows with the square of
    // its coverage; the raw weather value weights the depth once more (ADR-1443). 1 where the
    // weather is full. Matches `column_depth` in `clouds.wgsl`.
    cover * raw.clamp(0.0, 1.0) * settings.density.max(0.0) * thickness * sum / steps as f32
}

/// Share of the lit colour a cloud shadow cannot remove (the sky light that still reaches the
/// ground); the screen-space shadow never goes darker than this. Matches the `draw.w` the
/// outdoor pass uploads to `clouds.wgsl`.
pub const CLOUD_SHADOW_FLOOR: f32 = 0.35;

/// Multiplier a cloud shadow applies to a lit pixel, from the sun's transmittance `transmit`
/// (`exp(-COLUMN_FILL · τ)`, Beer–Lambert on the column along the sun ray): `floor` where the
/// sun is blocked, 1 where the column is clear, linear in the transmittance between. Matches
/// `mix(transmit, 1.0, floor)` in `clouds.wgsl`.
#[must_use]
pub fn cloud_shadow_shade(transmit: f32, floor: f32) -> f32 {
    let floor = floor.clamp(0.0, 1.0);
    let t = if transmit.is_nan() {
        1.0
    } else {
        transmit.clamp(0.0, 1.0)
    };
    t + (1.0 - t) * floor
}

/// [`cloud_shadow_shade`] for a column of optical depth `tau` (before `COLUMN_FILL`, the private
/// fill factor the shadow pass applies).
#[must_use]
pub fn cloud_shadow_shade_for_depth(tau: f32, floor: f32) -> f32 {
    cloud_shadow_shade((-tau.max(0.0) * COLUMN_FILL).exp(), floor)
}

/// Sun transmittance through the cloud column, projected along the sun onto the ground.
///
/// `sun_travel` points from the sun toward the scene. At noon the shadow sits under the
/// cloud; a low sun offsets it.
#[must_use]
pub fn cloud_shadow_transmittance(
    ground_xz: [f32; 2],
    ground_y: f32,
    sun_travel: [f32; 3],
    settings: &CloudSettings,
    weather: &WeatherMap,
) -> f32 {
    let mid = 0.5 * (settings.altitude_bottom + settings.altitude_top);
    let rise = mid - ground_y;
    let travel_y = sun_travel[1];
    let xz = if travel_y.abs() < 1.0e-3 {
        ground_xz
    } else {
        // Step from the ground back toward the sun (opposite travel) up to the layer.
        let scale = rise / -travel_y;
        [
            ground_xz[0] + (-sun_travel[0]) * scale,
            ground_xz[1] + (-sun_travel[2]) * scale,
        ]
    };
    (-column_optical_depth(xz, settings, weather) * COLUMN_FILL).exp()
}

/// Fraction of weather texels that are at least half covered. For [`CloudPattern::Coverage`]
/// this matches the setting within a texel.
#[must_use]
pub fn measured_coverage(weather: &WeatherMap) -> f32 {
    weather.filled
}

/// The layer values a weather change moves (ADR-429).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CloudWeatherSample {
    /// [`CloudSettings::coverage`].
    pub coverage: f32,
    /// [`CloudSettings::cloud_type`].
    pub cloud_type: f32,
    /// [`CloudSettings::density`].
    pub density: f32,
}

/// Largest one-frame coverage change that is still a transition (ADR-429). A 1 s transition
/// from clear to overcast moves 1/60 a frame at 60 fps; a larger step is a weather cut.
pub const CLOUD_WEATHER_CUT_COVERAGE: f32 = 0.05;

/// Largest one-frame cloud-type change that is still a transition.
pub const CLOUD_WEATHER_CUT_TYPE: f32 = 0.1;

/// Largest one-frame density ratio (larger over smaller) that is still a transition.
pub const CLOUD_WEATHER_CUT_DENSITY_RATIO: f32 = 1.25;

/// Whether the layer went from `prev` to `now` in one frame by more than a transition moves it:
/// a weather cut (ADR-429). After a cut the cloud history and the cloud transmittance map are
/// marched whole, like after a camera cut, instead of letting the reprojection pull the old sky
/// toward the new one over 16 frames. Smaller changes are left to the reconstruction's
/// staleness clamp and the map's band refresh. A non-finite value is a cut.
#[must_use]
pub fn cloud_weather_cut(prev: CloudWeatherSample, now: CloudWeatherSample) -> bool {
    let coverage = (now.coverage.clamp(0.0, 1.0) - prev.coverage.clamp(0.0, 1.0)).abs();
    let cloud_type = (now.cloud_type.clamp(0.0, 1.0) - prev.cloud_type.clamp(0.0, 1.0)).abs();
    if coverage.is_nan()
        || cloud_type.is_nan()
        || coverage > CLOUD_WEATHER_CUT_COVERAGE
        || cloud_type > CLOUD_WEATHER_CUT_TYPE
    {
        return true;
    }
    let (a, b) = (prev.density.max(0.0), now.density.max(0.0));
    let (lo, hi) = (a.min(b), a.max(b));
    if !hi.is_finite() {
        return true;
    }
    if hi <= 0.0 {
        return false;
    }
    lo <= 0.0 || hi > lo * CLOUD_WEATHER_CUT_DENSITY_RATIO
}

/// How far the layer's weather moved from `prev` to `now`, as a fraction `0..1` of a weather cut
/// ([`cloud_weather_cut`]'s thresholds; 1 at or past a cut). The reconstruction clamps the cloud
/// history at least this much toward the fresh samples (`reproject.w` in `clouds.wgsl`), so a
/// transition does not trail its old sky for the 16 frames a refresh takes. Exactly 0 when the
/// weather did not change, which leaves the reconstruction as it was (ADR-429).
#[must_use]
pub fn cloud_weather_motion(prev: CloudWeatherSample, now: CloudWeatherSample) -> f32 {
    if prev == now {
        return 0.0;
    }
    if cloud_weather_cut(prev, now) {
        return 1.0;
    }
    let coverage =
        (now.coverage.clamp(0.0, 1.0) - prev.coverage.clamp(0.0, 1.0)).abs() / CLOUD_WEATHER_CUT_COVERAGE;
    let cloud_type =
        (now.cloud_type.clamp(0.0, 1.0) - prev.cloud_type.clamp(0.0, 1.0)).abs() / CLOUD_WEATHER_CUT_TYPE;
    let (a, b) = (prev.density.max(0.0), now.density.max(0.0));
    let density = if a > 0.0 && b > 0.0 {
        (b / a).ln().abs() / CLOUD_WEATHER_CUT_DENSITY_RATIO.ln()
    } else {
        0.0
    };
    coverage.max(cloud_type).max(density).clamp(0.0, 1.0)
}

/// Cloud work the renderer did on this thread since start (ADR-429): a test or bench takes the
/// difference across its frames ([`Self::since`]) and checks it against the steady-state budget
/// ([`cloud_steady_work`]) without timing anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CloudWorkCounters {
    /// [`WeatherField`] builds (noise field and sort; once per resolution and seed).
    pub weather_fields: u64,
    /// Weather maps packed (a threshold pass over the texels per coverage or type change).
    pub weather_maps: u64,
    /// Weather textures created (the first, and a new map resolution).
    pub weather_textures: u64,
    /// Weather map uploads into the existing texture.
    pub weather_uploads: u64,
    /// View frames that marched every low-resolution texel (no usable history).
    pub view_full_marches: u64,
    /// View frames that marched one texel per 4×4 block and reprojected the rest.
    pub view_reprojected: u64,
    /// Low-resolution texels the view march marched (fresh texels on a reprojected frame).
    pub view_texels: u64,
    /// Weather cuts ([`cloud_weather_cut`]) that dropped the cloud history.
    pub weather_cuts: u64,
    /// Cloud transmittance map frames that marched the whole map.
    pub map_whole: u64,
    /// Cloud transmittance map texels marched.
    pub map_texels: u64,
}

impl CloudWorkCounters {
    const ZERO: Self = Self {
        weather_fields: 0,
        weather_maps: 0,
        weather_textures: 0,
        weather_uploads: 0,
        view_full_marches: 0,
        view_reprojected: 0,
        view_texels: 0,
        weather_cuts: 0,
        map_whole: 0,
        map_texels: 0,
    };

    /// `self − earlier`, per counter.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            weather_fields: self.weather_fields.saturating_sub(earlier.weather_fields),
            weather_maps: self.weather_maps.saturating_sub(earlier.weather_maps),
            weather_textures: self.weather_textures.saturating_sub(earlier.weather_textures),
            weather_uploads: self.weather_uploads.saturating_sub(earlier.weather_uploads),
            view_full_marches: self.view_full_marches.saturating_sub(earlier.view_full_marches),
            view_reprojected: self.view_reprojected.saturating_sub(earlier.view_reprojected),
            view_texels: self.view_texels.saturating_sub(earlier.view_texels),
            weather_cuts: self.weather_cuts.saturating_sub(earlier.weather_cuts),
            map_whole: self.map_whole.saturating_sub(earlier.map_whole),
            map_texels: self.map_texels.saturating_sub(earlier.map_texels),
        }
    }
}

thread_local! {
    static CLOUD_WORK: std::cell::Cell<CloudWorkCounters> =
        const { std::cell::Cell::new(CloudWorkCounters::ZERO) };
}

/// Adds to this thread's [`CloudWorkCounters`].
pub(crate) fn count_cloud_work(update: impl FnOnce(&mut CloudWorkCounters)) {
    CLOUD_WORK.with(|cell| {
        let mut counters = cell.get();
        update(&mut counters);
        cell.set(counters);
    });
}

/// This thread's [`CloudWorkCounters`].
#[must_use]
pub fn cloud_work_counters() -> CloudWorkCounters {
    CLOUD_WORK.with(std::cell::Cell::get)
}

/// Per-frame cloud work at steady state (ADR-429), in density-sample upper bounds: the view
/// march's texels × steps and the cloud transmittance map's texels × [`CLOUD_SHADOW_MAP_STEPS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudFrameWork {
    /// Low-resolution texels the view march marches.
    pub view_texels: u64,
    /// View-march density samples at most (`view_texels × steps`).
    pub view_samples: u64,
    /// Map texels marched (one refresh band; 0 when the map is off).
    pub map_texels: u64,
    /// Map density samples (`map_texels × CLOUD_SHADOW_MAP_STEPS`).
    pub map_samples: u64,
}

impl CloudFrameWork {
    /// View and map samples together.
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.view_samples + self.map_samples
    }
}

/// Steady-state work of one frame of `settings` on a `width × height` frame (ADR-429): with
/// `reprojection` one texel per [`CLOUD_BLOCK`]² block of the low-resolution target is marched
/// (else all of them), `steps` per texel (the preset's share of [`CloudSettings::steps`],
/// at most 96); the map, when [`CloudSettings::wants_shadow_map`], marches one band of
/// `size / CLOUD_SHADOW_REFRESH_FRAMES` rows. A held camera with no weather cut does exactly
/// this; camera cuts, sun steps and window jumps cost more for one frame.
#[must_use]
pub fn cloud_steady_work(
    settings: &CloudSettings,
    width: u32,
    height: u32,
    steps: u32,
    reprojection: bool,
) -> CloudFrameWork {
    let div = settings.resolution_divisor.clamp(1, 8);
    let low = [width.div_ceil(div).max(1), height.div_ceil(div).max(1)];
    let view_texels = if reprojection {
        let fresh = cloud_fresh_extent(low);
        u64::from(fresh[0]) * u64::from(fresh[1])
    } else {
        u64::from(low[0]) * u64::from(low[1])
    };
    let map_texels = if settings.wants_shadow_map() {
        let size = settings.shadow_map_texels();
        u64::from(size) * u64::from(size.div_ceil(CLOUD_SHADOW_REFRESH_FRAMES))
    } else {
        0
    };
    CloudFrameWork {
        view_texels,
        view_samples: view_texels * u64::from(steps.clamp(1, 96)),
        map_texels,
        map_samples: map_texels * u64::from(CLOUD_SHADOW_MAP_STEPS),
    }
}

/// Steady-state density-sample budget of one cloud frame at 2560×1440 on Desktop High (64
/// steps, half resolution, reprojection, a 512² map): 57 600 fresh texels × 64 steps plus one
/// 32-row map band × 16 steps, about 4.0 M samples (ADR-429). [`cloud_steady_work`] of the
/// shipped defaults must stay inside it; a change that raises the per-frame work past it needs
/// a measured bench and a new ADR.
pub const CLOUD_FRAME_SAMPLE_BUDGET: u64 = 4_000_000;

/// Default edge of the cloud transmittance map ([`CloudSettings::shadow_map_size`]), texels.
pub const CLOUD_SHADOW_MAP_SIZE: u32 = 512;

/// Default world edge of the cloud transmittance map ([`CloudSettings::shadow_map_extent_m`]),
/// metres: 20 km, about 39 m a texel at 512².
pub const CLOUD_SHADOW_MAP_EXTENT_M: f32 = 20_000.0;

/// Density samples along the sun per map texel. Matches `SHADOW_MAP_STEPS` in `clouds.wgsl`.
pub const CLOUD_SHADOW_MAP_STEPS: u32 = 16;

/// The map refreshes one band of `size / 16` texel rows per frame, so every texel is marched
/// again within this many frames.
pub const CLOUD_SHADOW_REFRESH_FRAMES: u32 = 16;

/// Lowest sun (`to_sun.y`, the sine of its elevation, about 0.6°) the map is built and read
/// for. Matches `cloud_shadow.wgsl`.
pub const CLOUD_SHADOW_MIN_SUN_Y: f32 = 0.01;

/// Where the map starts fading to unshadowed, as a fraction of its half extent. Matches
/// `cloud_shadow.wgsl`.
pub const CLOUD_SHADOW_EDGE_FADE: f32 = 0.85;

/// Strongest dimming of the sky light under a full, opaque overcast (the fraction removed).
pub const CLOUD_OVERCAST_DIM: f32 = 0.6;

/// Where the sun line through `p` crosses the plane `y = base_y` (the layer base), the
/// coordinate the cloud transmittance map is indexed by. `None` when the sun is lower than
/// [`CLOUD_SHADOW_MIN_SUN_Y`]. `to_sun` is unit length and points toward the sun.
#[must_use]
pub fn cloud_shadow_crossing(p: [f32; 3], to_sun: [f32; 3], base_y: f32) -> Option<[f32; 2]> {
    if to_sun[1].is_nan() || to_sun[1] < CLOUD_SHADOW_MIN_SUN_Y {
        return None;
    }
    let k = (base_y - p[1]) / to_sun[1];
    Some([p[0] + to_sun[0] * k, p[2] + to_sun[2] * k])
}

/// The world cell (texel units) `xz` on the base plane falls in, for `texel_m` metre texels.
#[must_use]
pub fn cloud_shadow_cell(xz: [f32; 2], texel_m: f32) -> [i32; 2] {
    let t = texel_m.max(1.0e-3);
    [(xz[0] / t).floor() as i32, (xz[1] / t).floor() as i32]
}

/// The map texel that stores world cell `cell`. Addressing is toroidal: a cell keeps its texel
/// while the window moves, so only the cells a move brings in need marching.
#[must_use]
pub fn cloud_shadow_texel(cell: [i32; 2], size: u32) -> [u32; 2] {
    let n = i64::from(size.max(1));
    [
        i64::from(cell[0]).rem_euclid(n) as u32,
        i64::from(cell[1]).rem_euclid(n) as u32,
    ]
}

/// The world cell texel `texel` holds for a window whose first cell is `start`. The inverse
/// of [`cloud_shadow_texel`] inside the window; matches `shadow_map_cell` in `clouds.wgsl`.
#[must_use]
pub fn cloud_shadow_texel_cell(texel: [u32; 2], start: [i32; 2], size: u32) -> [i32; 2] {
    let n = i64::from(size.max(1));
    let along = |t: u32, s: i32| {
        let rel = (i64::from(t) - i64::from(s)).rem_euclid(n);
        (i64::from(s) + rel) as i32
    };
    [along(texel[0], start[0]), along(texel[1], start[1])]
}

/// First cell of the map window: `size` cells centred on the base-plane crossing of the
/// camera (taken at the layer base when it is above it, so a camera above the clouds keeps the
/// ground below it in the window), snapped to whole texels so the window does not swim.
/// `None` when the sun is too low ([`CLOUD_SHADOW_MIN_SUN_Y`]).
#[must_use]
pub fn cloud_shadow_window(
    eye: [f32; 3],
    to_sun: [f32; 3],
    base_y: f32,
    size: u32,
    texel_m: f32,
) -> Option<[i32; 2]> {
    let center = cloud_shadow_crossing([eye[0], eye[1].min(base_y), eye[2]], to_sun, base_y)?;
    let cell = cloud_shadow_cell(center, texel_m);
    let half = (size / 2) as i32;
    Some([cell[0] - half, cell[1] - half])
}

/// Texel rectangles `[x, y, width, height]` the map pass marches this frame.
///
/// Every frame refreshes one band of rows (`size / 16`, [`CLOUD_SHADOW_REFRESH_FRAMES`]), so
/// the whole map is marched again every 16 frames. A window that moved by a few cells also
/// marches the columns and rows it brought in (they still hold cells from the far side). No
/// previous window (first frame, camera cut, a sun or wind change, a resize) or a jump of a
/// quarter of the map or more marches it whole.
#[must_use]
pub fn cloud_shadow_refresh_rects(
    frame: u32,
    size: u32,
    prev: Option<[i32; 2]>,
    start: [i32; 2],
) -> Vec<[u32; 4]> {
    let n = size.max(1);
    let full = vec![[0, 0, n, n]];
    let Some(prev) = prev else {
        return full;
    };
    let dx = i64::from(start[0]) - i64::from(prev[0]);
    let dz = i64::from(start[1]) - i64::from(prev[1]);
    let limit = i64::from((n / 4).max(1));
    if dx.abs() >= limit || dz.abs() >= limit {
        return full;
    }
    let mut rects = Vec::with_capacity(5);
    let band = n.div_ceil(CLOUD_SHADOW_REFRESH_FRAMES);
    let y0 = (frame % CLOUD_SHADOW_REFRESH_FRAMES) * band;
    if y0 < n {
        rects.push([0, y0, n, band.min(n - y0)]);
    }
    // The cells a move brought in: `count` consecutive cells from `first`, as texel spans.
    let spans = |first: i64, count: i64| {
        let start = first.rem_euclid(i64::from(n)) as u32;
        let count = count as u32;
        if start + count <= n {
            vec![(start, count)]
        } else {
            vec![(start, n - start), (0, count - (n - start))]
        }
    };
    let entered = |delta: i64, prev: i32, start: i32| -> Vec<(u32, u32)> {
        match delta {
            0 => Vec::new(),
            d if d > 0 => spans(i64::from(prev) + i64::from(n), d),
            d => spans(i64::from(start), -d),
        }
    };
    for (x, w) in entered(dx, prev[0], start[0]) {
        rects.push([x, 0, w, n]);
    }
    for (y, h) in entered(dz, prev[1], start[1]) {
        rects.push([0, y, n, h]);
    }
    rects
}

/// Sun transmittance the cloud transmittance map stores for base-plane point `xz`: the line
/// along `to_sun` through `(x, altitude_bottom, z)`, from where it leaves the layer-base shell
/// to where it leaves the top shell (both curved with the planet, `planet_radius_m`), sampled
/// at [`CLOUD_SHADOW_MAP_STEPS`] midpoints of `density` (normalised 0..1, the march's
/// `density_at`) times [`CloudSettings::density`] (Beer–Lambert).
///
/// The CPU mirror of `shadow_map_transmittance` in `clouds.wgsl`: same shells (a missed shell
/// is `(−1, −1)` there too), same step positions. Tests pass an analytic density.
#[must_use]
pub fn cloud_shadow_march(
    xz: [f32; 2],
    to_sun: [f32; 3],
    settings: &CloudSettings,
    planet_radius_m: f32,
    density: impl Fn([f32; 3]) -> f32,
) -> f32 {
    let r = planet_radius_m.max(1000.0);
    let center = [0.0, -r, 0.0];
    let top_alt = settings.altitude_top.max(settings.altitude_bottom + 1.0);
    let o = [xz[0], settings.altitude_bottom, xz[1]];
    let (_, bottom_far) =
        shell_intersections(o, to_sun, center, r + settings.altitude_bottom).unwrap_or((-1.0, -1.0));
    let (_, t1) = shell_intersections(o, to_sun, center, r + top_alt).unwrap_or((-1.0, -1.0));
    let t0 = bottom_far.min(0.0);
    if t1 <= t0 {
        return 1.0;
    }
    let steps = CLOUD_SHADOW_MAP_STEPS;
    let dt = (t1 - t0) / steps as f32;
    let mut od = 0.0;
    for i in 0..steps {
        let t = t0 + (i as f32 + 0.5) * dt;
        let p = [o[0] + to_sun[0] * t, o[1] + to_sun[1] * t, o[2] + to_sun[2] * t];
        od += density(p) * dt;
    }
    (-od * settings.density.max(0.0)).exp()
}

/// Map uniform the shading passes read (`cloud_shadow.wgsl`): `[extent m, layer base y,
/// layer top y, 1]`. All zero (map off) when `enabled` is false.
#[must_use]
pub fn cloud_shadow_params(settings: &CloudSettings, enabled: bool) -> [f32; 4] {
    if !enabled {
        return [0.0; 4];
    }
    [
        settings.shadow_map_extent(),
        settings.altitude_bottom,
        settings.altitude_top.max(settings.altitude_bottom + 1.0),
        1.0,
    ]
}

/// CPU mirror of `cloud_sun_transmittance` in `cloud_shadow.wgsl` over a map held in
/// `texels` (row-major, `size × size`): bilinear with toroidal wrap at the point's base-plane
/// crossing, the part of the layer above a point inside it (`T^fraction`), and a fade to 1 over
/// the outer [`CLOUD_SHADOW_EDGE_FADE`] of the window around the camera.
#[must_use]
pub fn cloud_shadow_lookup(
    texels: &[f32],
    size: u32,
    params: [f32; 4],
    p: [f32; 3],
    to_sun: [f32; 3],
    eye: [f32; 3],
) -> f32 {
    let [extent, base, top, on] = params;
    // The shader's switch, `!(params.w > 0.5)` (ADR-5971): anything that is not above 0.5 is off.
    let off = on.partial_cmp(&0.5) != Some(std::cmp::Ordering::Greater);
    // In u64: a `size` past 65 535 squares past `u32` (rc1-rev-render-k 5).
    if off || p[1] >= top || size == 0 || (texels.len() as u64) < u64::from(size) * u64::from(size) {
        return 1.0;
    }
    let Some(xz) = cloud_shadow_crossing(p, to_sun, base) else {
        return 1.0;
    };
    let Some(center) = cloud_shadow_crossing([eye[0], eye[1].min(base), eye[2]], to_sun, base) else {
        return 1.0;
    };
    let extent = extent.max(1.0);
    let d = (xz[0] - center[0]).abs().max((xz[1] - center[1]).abs()) / (0.5 * extent);
    if d >= 1.0 {
        return 1.0;
    }
    let n = size as f32;
    let fx = xz[0] / extent * n - 0.5;
    let fz = xz[1] / extent * n - 0.5;
    let (x0, z0) = (fx.floor(), fz.floor());
    let (tx, tz) = (fx - x0, fz - z0);
    let at = |x: f32, z: f32| {
        let [i, j] = cloud_shadow_texel([x as i32, z as i32], size);
        texels[(j * size + i) as usize]
    };
    let row0 = at(x0, z0) + (at(x0 + 1.0, z0) - at(x0, z0)) * tx;
    let row1 = at(x0, z0 + 1.0) + (at(x0 + 1.0, z0 + 1.0) - at(x0, z0 + 1.0)) * tx;
    let mut t = row0 + (row1 - row0) * tz;
    if p[1] > base {
        let frac = ((top - p[1]) / (top - base).max(1.0)).clamp(0.0, 1.0);
        t = t.max(1.0e-6).powf(frac);
    }
    let edge = smoothstep(CLOUD_SHADOW_EDGE_FADE, 1.0, d);
    t + (1.0 - t) * edge
}

fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The part of a view ray a cloud layer can shadow ([`cloud_air_span`], ADR-5944).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudAirSpan {
    /// The stretch of the ray, metres from the eye, that is below the layer's top and inside
    /// the cloud transmittance map's window: the only air [`cloud_shadow_lookup`] can darken.
    /// Empty (`[0, 0]`) when there is none.
    pub span_m: [f32; 2],
    /// Share of the ray's length below the layer's top, `0..=1` (the window does not count).
    pub under_top: f32,
}

impl CloudAirSpan {
    /// No air under the layer.
    pub const NONE: Self = Self {
        span_m: [0.0, 0.0],
        under_top: 0.0,
    };
}

/// Where the air between `eye` and the point `distance_m` along the unit direction `view` can
/// lie in the cloud layer's shadow: the ray clipped to the half space below the layer's top
/// and to the cloud transmittance map's window (a square of `extent` metres on the base plane
/// around the camera's sun-line crossing; a point's place in it is linear in the distance along
/// the ray, so the window cuts the ray to one interval, a slab test per axis, Kay & Kajiya
/// 1986). `params` is [`cloud_shadow_params`]; an off map or a sun below
/// [`CLOUD_SHADOW_MIN_SUN_Y`] gives [`CloudAirSpan::NONE`].
///
/// The CPU mirror of `cloud_air_span` in `cloud_shadow.wgsl`.
#[must_use]
pub fn cloud_air_span(
    params: [f32; 4],
    eye: [f32; 3],
    view: [f32; 3],
    distance_m: f32,
    to_sun: [f32; 3],
) -> CloudAirSpan {
    let [extent, base, top, on] = params;
    if on < 0.5
        || to_sun[1].is_nan()
        || to_sun[1] < CLOUD_SHADOW_MIN_SUN_Y
        || distance_m.is_nan()
        || distance_m <= 0.0
    {
        return CloudAirSpan::NONE;
    }
    let (mut t0, mut t1) = (0.0_f32, distance_m);
    if view[1].abs() < 1.0e-6 {
        if eye[1] >= top {
            return CloudAirSpan::NONE;
        }
    } else {
        let cross = (top - eye[1]) / view[1];
        if view[1] > 0.0 {
            t1 = t1.min(cross);
        } else {
            t0 = t0.max(cross);
        }
    }
    if t1 <= t0 {
        return CloudAirSpan::NONE;
    }
    let under_top = ((t1 - t0) / distance_m).clamp(0.0, 1.0);
    let none = CloudAirSpan {
        span_m: [0.0, 0.0],
        under_top,
    };
    // A point's offset from the window's centre on the base plane, `off0 + slope · t`.
    let half = 0.5 * extent.max(1.0);
    let lift = ((base - eye[1]) - (base - eye[1].min(base))) / to_sun[1];
    let rise = view[1] / to_sun[1];
    for axis in [0, 2] {
        let off0 = to_sun[axis] * lift;
        let slope = view[axis] - to_sun[axis] * rise;
        if slope.abs() < 1.0e-9 {
            if off0.abs() >= half {
                return none;
            }
        } else {
            let (a, b) = ((-half - off0) / slope, (half - off0) / slope);
            t0 = t0.max(a.min(b));
            t1 = t1.min(a.max(b));
        }
    }
    if t1 <= t0 {
        return none;
    }
    CloudAirSpan {
        span_m: [t0, t1],
        under_top,
    }
}

/// Midpoint samples of the cloud transmittance along a view ray for the analytic haze
/// (ADR-5944). Matches `HAZE_CLOUD_STEPS` in `cloud_shadow.wgsl`.
pub const HAZE_CLOUD_STEPS: u32 = 4;

/// Mean of [`cloud_shadow_lookup`] over [`HAZE_CLOUD_STEPS`] midpoints of the segment from
/// `eye` to `world`: the share of the sun the air on that view ray keeps under the layer
/// (ADR-5944). 1 with the map off. The CPU mirror of `cloud_sun_on_ray` in `cloud_shadow.wgsl`.
#[must_use]
pub fn cloud_shadow_on_ray(
    texels: &[f32],
    size: u32,
    params: [f32; 4],
    eye: [f32; 3],
    world: [f32; 3],
    to_sun: [f32; 3],
) -> f32 {
    if params[3] < 0.5 {
        return 1.0;
    }
    let n = HAZE_CLOUD_STEPS;
    let mut sum = 0.0;
    for i in 0..n {
        let f = (i as f32 + 0.5) / n as f32;
        let p = [
            eye[0] + (world[0] - eye[0]) * f,
            eye[1] + (world[1] - eye[1]) * f,
            eye[2] + (world[2] - eye[2]) * f,
        ];
        sum += cloud_shadow_lookup(texels, size, params, p, to_sun, eye);
    }
    sum / n as f32
}

/// Share of the analytic haze's horizon sky term a closed cloud layer leaves (ADR-5944): the
/// sky's share of the light on flat ground under a clear sky (0.08 to 0.16 by the tables for a
/// sun 60° to 20° up), which is what the scene under the cover keeps, so the haze and the ground
/// behind it fall together. Matches `HAZE_SKY_FLOOR` in
/// `aerial.wgsl` and the terrain's copy.
pub const HAZE_SKY_FLOOR: f32 = 0.12;

/// Mean coverage and the type profile of one packed weather map (ADR-321).
///
/// Altitude and density are not inputs. [`overcast_factor_from_terms`] applies them, so a
/// layer that only moves keeps this value.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OvercastMapTerms {
    /// Mean of the coverage channel after the march's coverage boost.
    pub mean_cover: f32,
    /// Mean of the cloud-type height profile over eight samples.
    pub profile: f32,
}

/// The map-dependent half of [`cloud_overcast_factor`]. Zero when the layer or the map is empty.
#[must_use]
pub(crate) fn overcast_map_terms(settings: &CloudSettings, weather: &WeatherMap) -> OvercastMapTerms {
    let coverage = settings.coverage.clamp(0.0, 1.0);
    if coverage <= 0.0 || weather.rgba.is_empty() {
        return OvercastMapTerms {
            mean_cover: 0.0,
            profile: 0.0,
        };
    }
    let boost = 0.75 + 0.4 * coverage;
    let texels = weather.rgba.as_chunks::<4>().0;
    let sum: f32 = texels
        .iter()
        .map(|px| (f32::from(px[0]) / 255.0 * boost).min(1.0))
        .sum();
    let mean_cover = sum / texels.len().max(1) as f32;
    let steps = 8;
    let profile: f32 = (0..steps)
        .map(|i| type_gradient((i as f32 + 0.5) / steps as f32, settings.cloud_type))
        .sum::<f32>()
        / steps as f32;
    OvercastMapTerms { mean_cover, profile }
}

/// The scalar half of [`cloud_overcast_factor`]: column opacity from density and thickness.
#[must_use]
pub(crate) fn overcast_factor_from_terms(settings: &CloudSettings, terms: OvercastMapTerms) -> f32 {
    let coverage = settings.coverage.clamp(0.0, 1.0);
    if coverage <= 0.0 {
        return 0.0;
    }
    let thickness = (settings.altitude_top - settings.altitude_bottom).max(0.0);
    let tau = settings.density.max(0.0) * thickness * terms.profile;
    let opacity = 1.0 - (-COLUMN_FILL * tau).exp();
    (terms.mean_cover * opacity).clamp(0.0, 1.0)
}

/// Share of a covered column's optical depth the eroded 3D shape keeps for a column of weather
/// coverage 1: the shape `remap(base, 1 − cover, 1) · cover` has mean `cover² / 2` for uniform
/// noise (the cloud-shadow-depth row of `docs/provenance.md`), so the sun crosses about half the
/// full column there. [`sun_cover_from_terms`] scales it by `cover²`.
pub(crate) const SUN_COLUMN_FILL: f32 = 0.5;

/// The share of the sun a layer takes from the ground on average (ADR-5944): the weather map's
/// mean coverage times the opacity to the sun of a column of that coverage,
/// `1 − exp(−SUN_COLUMN_FILL · cover² · τ)` with `τ` the optical depth of a fully covered
/// column. An estimate of one minus the mean of the cloud transmittance map, from the same map
/// terms as the overcast factor; a closed deck gives about 1, a fair-weather sky its covered
/// share. Not [`cloud_overcast_factor`]: that is how much a cover dims the sky light
/// (`COLUMN_FILL` 0.05), and it leaves a fifth of the sun under a deck that lets none through.
#[must_use]
pub(crate) fn sun_cover_from_terms(settings: &CloudSettings, terms: OvercastMapTerms) -> f32 {
    if settings.coverage.clamp(0.0, 1.0) <= 0.0 {
        return 0.0;
    }
    let thickness = (settings.altitude_top - settings.altitude_bottom).max(0.0);
    let density = if settings.density.is_finite() {
        settings.density.max(0.0)
    } else {
        0.0
    };
    let tau = density * thickness * terms.profile;
    let cover = terms.mean_cover.clamp(0.0, 1.0);
    (cover * (1.0 - (-SUN_COLUMN_FILL * cover * cover * tau).exp())).clamp(0.0, 1.0)
}

/// Overcast factor `0..1` of a layer (ADR-321): the weather map's mean coverage (with the
/// march's coverage boost) times the opacity of a covered column
/// (`1 − exp(−COLUMN_FILL · τ)`, `τ` the column's optical depth at full cover). Zero without
/// clouds.
#[must_use]
pub fn cloud_overcast_factor(settings: &CloudSettings, weather: &WeatherMap) -> f32 {
    overcast_factor_from_terms(settings, overcast_map_terms(settings, weather))
}

/// Sky radiance under an overcast factor (ADR-321): greyed toward its luminance and dimmed by
/// up to [`CLOUD_OVERCAST_DIM`]. Factor 0 returns `rgb` unchanged, bit for bit.
#[must_use]
pub fn overcast_sky(rgb: [f32; 3], overcast: f32) -> [f32; 3] {
    if overcast.is_nan() || overcast <= 0.0 {
        return rgb;
    }
    let o = overcast.min(1.0);
    let luma = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    let keep = 1.0 - CLOUD_OVERCAST_DIM * o;
    rgb.map(|c| (c + (luma - c) * o) * keep)
}

fn remap(v: f32, lo: f32, hi: f32, new_lo: f32, new_hi: f32) -> f32 {
    if (hi - lo).abs() < 1.0e-6 {
        return new_lo;
    }
    new_lo + (v - lo) / (hi - lo) * (new_hi - new_lo)
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn hash_u32(mut x: u32) -> u32 {
    x = x.wrapping_mul(0x9E37_79B1);
    x ^= x >> 16;
    x = x.wrapping_mul(0x85EB_CA77);
    x ^= x >> 13;
    x = x.wrapping_mul(0xC2B2_AE3D);
    x ^= x >> 16;
    x
}

fn hash3(x: i32, y: i32, z: i32, seed: u32) -> u32 {
    hash_u32(
        (x as u32).wrapping_mul(73_856_093)
            ^ (y as u32).wrapping_mul(19_349_663)
            ^ (z as u32).wrapping_mul(83_492_791)
            ^ seed.wrapping_mul(2_654_435_761),
    )
}

fn unit_f(h: u32) -> f32 {
    (h >> 8) as f32 / 16_777_216.0
}

fn fade(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Tileable 2D value noise with `period` cells, `0..1`.
fn value_noise_2d(x: f32, y: f32, period: i32, seed: u32) -> f32 {
    let (x0, y0) = (x.floor(), y.floor());
    let (tx, ty) = (fade(x - x0), fade(y - y0));
    let at = |i: f32, j: f32| {
        let a = (i as i32).rem_euclid(period);
        let b = (j as i32).rem_euclid(period);
        unit_f(hash3(a, b, 0, seed))
    };
    let top = at(x0, y0) + (at(x0 + 1.0, y0) - at(x0, y0)) * tx;
    let bottom = at(x0, y0 + 1.0) + (at(x0 + 1.0, y0 + 1.0) - at(x0, y0 + 1.0)) * tx;
    top + (bottom - top) * ty
}

/// Tileable 2D FBM over the unit square, base frequency `freq`.
fn fbm_2d(u: f32, v: f32, freq: f32, octaves: u32, seed: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 0.5;
    let mut total = 0.0;
    let mut f = freq;
    for o in 0..octaves {
        sum += amp * value_noise_2d(u * f, v * f, f as i32, seed.wrapping_add(o * 977));
        total += amp;
        amp *= 0.5;
        f *= 2.0;
    }
    sum / total
}

/// Tileable 2D Worley F1 (in cell units) over the unit square with `cells` per side.
fn worley_2d(u: f32, v: f32, cells: i32, seed: u32) -> f32 {
    let x = u * cells as f32;
    let y = v * cells as f32;
    let (cx, cy) = (x.floor() as i32, y.floor() as i32);
    let mut best = f32::MAX;
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (gx, gy) = (cx + dx, cy + dy);
            let h = hash3(gx.rem_euclid(cells), gy.rem_euclid(cells), 0, seed);
            let px = gx as f32 + unit_f(h);
            let py = gy as f32 + unit_f(hash_u32(h));
            let d = ((px - x).powi(2) + (py - y).powi(2)).sqrt();
            best = best.min(d);
        }
    }
    best
}

/// Tileable 3D Worley F1 (cell units) over the unit cube with `cells` per side.
fn worley_3d(p: [f32; 3], cells: i32, seed: u32) -> f32 {
    let q = [p[0] * cells as f32, p[1] * cells as f32, p[2] * cells as f32];
    let c = [q[0].floor() as i32, q[1].floor() as i32, q[2].floor() as i32];
    let mut best = f32::MAX;
    for dz in -1..=1 {
        for dy in -1..=1 {
            for dx in -1..=1 {
                let g = [c[0] + dx, c[1] + dy, c[2] + dz];
                let h = hash3(
                    g[0].rem_euclid(cells),
                    g[1].rem_euclid(cells),
                    g[2].rem_euclid(cells),
                    seed,
                );
                let h2 = hash_u32(h);
                let h3 = hash_u32(h2);
                let f = [
                    g[0] as f32 + unit_f(h),
                    g[1] as f32 + unit_f(h2),
                    g[2] as f32 + unit_f(h3),
                ];
                let d2 = (f[0] - q[0]).powi(2) + (f[1] - q[1]).powi(2) + (f[2] - q[2]).powi(2);
                best = best.min(d2);
            }
        }
    }
    best.sqrt()
}

/// Inverted Worley FBM (1 at feature points), three octaves from `cells`.
fn worley_fbm_3d(p: [f32; 3], cells: i32, seed: u32) -> f32 {
    let w = |c: i32, s: u32| 1.0 - worley_3d(p, c, s).min(1.0);
    (w(cells, seed) * 0.625 + w(cells * 2, seed + 1) * 0.25 + w(cells * 4, seed + 2) * 0.125).clamp(0.0, 1.0)
}

fn grad3(h: u32, x: f32, y: f32, z: f32) -> f32 {
    // Perlin 2002's twelve edge gradients.
    match h % 12 {
        0 => x + y,
        1 => -x + y,
        2 => x - y,
        3 => -x - y,
        4 => x + z,
        5 => -x + z,
        6 => x - z,
        7 => -x - z,
        8 => y + z,
        9 => -y + z,
        10 => y - z,
        _ => -y - z,
    }
}

/// Tileable 3D gradient noise with `period` cells over the unit cube, about `-1..1`.
fn perlin_3d(p: [f32; 3], period: i32, seed: u32) -> f32 {
    let q = [p[0] * period as f32, p[1] * period as f32, p[2] * period as f32];
    let c = [q[0].floor(), q[1].floor(), q[2].floor()];
    let f = [q[0] - c[0], q[1] - c[1], q[2] - c[2]];
    let ci = [c[0] as i32, c[1] as i32, c[2] as i32];
    let corner = |dx: i32, dy: i32, dz: i32| {
        let h = hash3(
            (ci[0] + dx).rem_euclid(period),
            (ci[1] + dy).rem_euclid(period),
            (ci[2] + dz).rem_euclid(period),
            seed,
        );
        grad3(h, f[0] - dx as f32, f[1] - dy as f32, f[2] - dz as f32)
    };
    let (u, v, w) = (fade(f[0]), fade(f[1]), fade(f[2]));
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let x00 = lerp(corner(0, 0, 0), corner(1, 0, 0), u);
    let x10 = lerp(corner(0, 1, 0), corner(1, 1, 0), u);
    let x01 = lerp(corner(0, 0, 1), corner(1, 0, 1), u);
    let x11 = lerp(corner(0, 1, 1), corner(1, 1, 1), u);
    lerp(lerp(x00, x10, v), lerp(x01, x11, v), w)
}

fn perlin_fbm_3d(p: [f32; 3], period: i32, octaves: u32, seed: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 1.0;
    let mut total = 0.0;
    let mut f = period;
    for o in 0..octaves {
        sum += amp * perlin_3d(p, f, seed + o);
        total += amp;
        amp *= 0.5;
        f *= 2;
    }
    sum / total
}

#[cfg(test)]
mod tests {
    #[test]
    fn curl_field_tiles_and_is_divergence_free() {
        let n = 32;
        let field = super::curl_field(n);
        let n_i = n as i32;
        let at = |x: i32, y: i32| field[(y.rem_euclid(n_i) * n_i + x.rem_euclid(n_i)) as usize];
        let peak = field
            .iter()
            .fold(0.0f32, |m, v| m.max(v[0].abs()).max(v[1].abs()));
        assert!(peak > 1.0e-3, "flat curl field: {peak}");
        // Central differences of a central-difference curl cancel exactly, up to rounding.
        let mut worst = 0.0f32;
        for y in 0..n_i {
            for x in 0..n_i {
                let div =
                    0.5 * (at(x + 1, y)[0] - at(x - 1, y)[0]) + 0.5 * (at(x, y + 1)[1] - at(x, y - 1)[1]);
                worst = worst.max(div.abs());
            }
        }
        assert!(worst < peak * 1.0e-4, "divergence {worst} vs peak {peak}");
        // The potential tiles over the unit square, so the curl (and its texture) does too.
        for (u, v) in [(0.03, 0.4), (0.51, 0.97), (0.88, 0.12)] {
            let a = super::fbm_2d(u, v, 4.0, 3, 811);
            let b = super::fbm_2d(u + 1.0, v, 4.0, 3, 811);
            let c = super::fbm_2d(u, v + 1.0, 4.0, 3, 811);
            assert!(
                (a - b).abs() < 1.0e-4 && (a - c).abs() < 1.0e-4,
                "potential does not tile at {u},{v}"
            );
        }
        let rgba = super::cloud_curl();
        assert_eq!(rgba.len(), (super::CURL_SIZE * super::CURL_SIZE * 4) as usize);
    }

    use super::*;

    #[test]
    fn coverage_map_matches_the_setting() {
        for coverage in [0.0_f32, 0.25, 0.5, 0.8, 1.0] {
            let settings = CloudSettings::coverage_layer(coverage);
            let map = build_weather_map(&settings);
            let err = (map.filled - coverage).abs();
            assert!(err < 0.02, "coverage {coverage} filled {}", map.filled);
        }
    }

    #[test]
    fn coverage_map_has_soft_edges_not_a_binary_mask() {
        let map = build_weather_map(&CloudSettings::coverage_layer(0.5));
        let mid = map
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| (20..235).contains(&px[0]))
            .count();
        assert!(mid > map.rgba.len() / 4 / 20, "only {mid} partial texels");
    }

    #[test]
    fn wind_scrolls_the_sample() {
        let mut settings = CloudSettings::coverage_layer(0.5);
        settings.wind_m_per_s = [settings.domain_m, 0.0];
        let map = build_weather_map(&settings);
        let mut differ = false;
        for x in [0.0, 3000.0, -8000.0, 12000.0, 20000.0] {
            settings.time_s = 0.0;
            let p = map.sample([x, 1.0], &settings);
            settings.time_s = 0.37;
            if (p - map.sample([x, 1.0], &settings)).abs() > 1.0e-3 {
                differ = true;
            }
        }
        assert!(differ, "wind must move the weather sample");
    }

    #[test]
    fn split_shadow_darkens_only_the_cloudy_half() {
        let settings = CloudSettings::split_shadow();
        let map = build_weather_map(&settings);
        let sun = [0.0, -1.0, 0.0];
        let quarter = settings.domain_m * 0.25;
        let clear = cloud_shadow_transmittance([-quarter, 0.0], 0.0, sun, &settings, &map);
        let cloudy = cloud_shadow_transmittance([quarter, 0.0], 0.0, sun, &settings, &map);
        assert!(clear > 0.85, "clear half stays lit, {clear}");
        assert!(cloudy < 0.35, "cloudy half shadows the ground, {cloudy}");
        assert!(clear > cloudy * 2.0);
    }

    #[test]
    fn shadow_shade_follows_optical_depth() {
        let f = CLOUD_SHADOW_FLOOR;
        assert_eq!(cloud_shadow_shade_for_depth(0.0, f), 1.0, "clear sky is lit");
        // Thin cloud (tau 0.2): a few percent, not the floor.
        let thin = cloud_shadow_shade_for_depth(0.2, f);
        assert!(thin > 0.99, "thin cloud casts a faint shadow, {thin}");
        // Monotonic in depth, bounded below by the floor.
        let mut last = 1.0;
        for i in 1..=60 {
            let s = cloud_shadow_shade_for_depth(i as f32, f);
            assert!(s <= last && s >= f, "depth {i}: {s}");
            last = s;
        }
        assert!((cloud_shadow_shade_for_depth(1.0e4, f) - f).abs() < 1.0e-4);
        // Beer-Lambert: transmittance multiplies for stacked columns.
        let a = (-0.5 * COLUMN_FILL).exp();
        let b = (-1.5 * COLUMN_FILL).exp();
        let both = (-2.0 * COLUMN_FILL).exp();
        assert!((a * b - both).abs() < 1.0e-6);
        assert_eq!(cloud_shadow_shade(f32::NAN, f), 1.0);
    }

    #[test]
    fn thin_weather_casts_a_fainter_shadow_than_its_coverage() {
        let settings = CloudSettings {
            pattern: CloudPattern::Constant,
            density: 0.04,
            ..CloudSettings::coverage_layer(0.08)
        };
        let thin = build_weather_map(&settings);
        let full = build_weather_map(&CloudSettings {
            coverage: 1.0,
            ..settings
        });
        // Same boost for both so only the weather value differs.
        let thin_tau = column_optical_depth([0.0, 0.0], &settings, &thin);
        let full_tau = column_optical_depth(
            [0.0, 0.0],
            &CloudSettings {
                coverage: 0.08,
                ..settings
            },
            &full,
        );
        let ratio = thin_tau / full_tau;
        let raw = thin.sample([0.0, 0.0], &settings);
        assert!(raw < 0.2 && ratio > 0.0);
        assert!(
            (ratio - raw * raw).abs() < 1.0e-3,
            "depth follows the square of the weather value: {ratio} vs {}",
            raw * raw
        );
    }

    #[test]
    fn height_gradient_vanishes_at_the_layer_edges() {
        assert!(height_gradient(1500.0, 1500.0, 4000.0) < 0.01);
        assert!(height_gradient(4000.0, 1500.0, 4000.0) < 0.01);
        assert!(height_gradient(2400.0, 1500.0, 4000.0) > 0.5);
    }

    #[test]
    fn cloud_types_have_distinct_heights() {
        // Stratus is gone by mid-layer; a tower still stands there.
        assert!(type_gradient(0.5, 0.0) < 0.01);
        assert!(type_gradient(0.5, 1.0) > 0.9);
        assert!(type_gradient(0.25, 0.5) > 0.9);
    }

    #[test]
    fn constant_weather_is_the_same_value_everywhere() {
        let settings = CloudSettings {
            pattern: CloudPattern::Constant,
            ..CloudSettings::coverage_layer(0.5)
        };
        let map = build_weather_map(&settings);
        let sample = map.sample([0.0, 0.0], &settings);
        assert!((sample - 128.0 / 255.0).abs() < 1.0e-6);
        for xz in [[3000.0, -2000.0], [10_000.0, 10_000.0], [-8000.0, 4000.0]] {
            assert!((map.sample(xz, &settings) - sample).abs() < 1.0e-6, "{xz:?}");
        }
    }

    #[test]
    fn noise_volumes_tile_and_vary() {
        let noise = cloud_noise();
        let n = SHAPE_NOISE_SIZE as usize;
        assert_eq!(noise.shape.len(), n * n * n * 4);
        let r: Vec<u8> = noise.shape.iter().step_by(4).copied().collect();
        let min = *r.iter().min().unwrap_or(&0);
        let max = *r.iter().max().unwrap_or(&0);
        assert!(max - min > 120, "Perlin–Worley spans a range, {min}..{max}");
        // Wrapping: the noise is periodic, so opposite faces are close.
        let at = |x: usize, y: usize, z: usize| i32::from(noise.shape[((z * n + y) * n + x) * 4 + 1]);
        let mut seam = 0;
        for y in 0..n {
            for z in 0..n {
                seam += (at(0, y, z) - at(n - 1, y, z)).abs();
            }
        }
        let mean_seam = seam as f32 / (n * n) as f32;
        assert!(
            mean_seam < 40.0,
            "Worley channel does not wrap, mean seam {mean_seam}"
        );
    }

    fn shell_f64(eye: [f64; 3], dir: [f64; 3], center: [f64; 3], radius: f64) -> Option<(f64, f64)> {
        let o = [eye[0] - center[0], eye[1] - center[1], eye[2] - center[2]];
        let b = o[0] * dir[0] + o[1] * dir[1] + o[2] * dir[2];
        let rad = (o[0] * o[0] + o[1] * o[1] + o[2] * o[2]).sqrt();
        let c = (rad - radius) * (rad + radius);
        let disc = b * b - c;
        if !disc.is_finite() || disc < 0.0 {
            return None;
        }
        let s = disc.sqrt();
        let q = -(b + s.copysign(b));
        let (t_a, t_b) = if q.abs() < 1.0e-8 {
            (-b - s, -b + s)
        } else {
            (q, c / q)
        };
        Some((t_a.min(t_b), t_a.max(t_b)))
    }

    fn unit(d: [f64; 3]) -> [f64; 3] {
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        [d[0] / len, d[1] / len, d[2] / len]
    }

    /// The f32 shell stays within a few metres of the f64 shell on a planet-radius sphere.
    #[test]
    fn shell_matches_f64_on_a_planet_sphere() {
        let planet = 6_371_000.0_f64;
        let center = [0.0, -planet, 0.0];
        let center_f = [0.0_f32, -planet as f32, 0.0];
        let shells = [planet + 1_500.0, planet + 4_000.0];
        let eyes: [[f64; 3]; 5] = [
            [0.0, 2.0, 0.0],
            [0.0, 2_500.0, 0.0],
            [4_000.0, 2.0, 0.0],
            [80_000.0, 100.0, -20_000.0],
            [0.0, 6_000.0, 0.0],
        ];
        let dirs = [
            unit([0.0, 1.0, 0.0]),
            unit([0.0, 0.0, 1.0]),
            unit([0.0, (0.2_f64.to_radians()).sin(), (0.2_f64.to_radians()).cos()]),
            unit([1.0, -0.4, 0.2]),
            unit([0.3, -1.0, 0.1]),
        ];
        for eye in eyes {
            let horiz = (eye[0] * eye[0] + eye[2] * eye[2]).sqrt();
            let tol = if horiz < 100.0 && eye[1].abs() < 100.0 {
                0.05
            } else {
                5.0
            };
            for &radius in &shells {
                for &dir in &dirs {
                    let got = shell_intersections(
                        [eye[0] as f32, eye[1] as f32, eye[2] as f32],
                        [dir[0] as f32, dir[1] as f32, dir[2] as f32],
                        center_f,
                        radius as f32,
                    );
                    let expect = shell_f64(eye, dir, center, radius);
                    match (got, expect) {
                        (None, None) => {}
                        (Some((a, b)), Some((ea, eb))) => {
                            // A root on the far side of the planet is millions of metres away.
                            // Half a metre there is still far tighter than the old cancelling form.
                            let bound = |expect: f64| if expect.abs() > 200_000.0 { 2.0 } else { tol };
                            let err_a = (f64::from(a) - ea).abs();
                            let err_b = (f64::from(b) - eb).abs();
                            assert!(
                                err_a < bound(ea) && err_b < bound(eb),
                                "eye {eye:?} r {radius} dir {dir:?}: f32 ({a}, {b}) f64 ({ea}, {eb}) err ({err_a}, {err_b}) tol {tol}"
                            );
                        }
                        (got, expect) => panic!("miss disagrees: f32 {got:?} f64 {expect:?} eye {eye:?}"),
                    }
                }
            }
        }
        // Above the outer shell, looking up: both roots are behind the camera, not a miss.
        let above = shell_intersections(
            [0.0, 6_000.0, 0.0],
            [0.0, 1.0, 0.0],
            center_f,
            (planet + 4_000.0) as f32,
        );
        let (near, far) = above.expect("the shell is behind an upward ray");
        assert!(
            far < 0.0,
            "upward from above the shell should be behind, {near} {far}"
        );
        // Above the shell, looking sideways: a true miss.
        assert!(
            shell_intersections(
                [0.0, 6_000.0, 0.0],
                [1.0, 0.0, 0.0],
                center_f,
                (planet + 4_000.0) as f32
            )
            .is_none(),
            "a horizontal ray from above the shell misses it"
        );
    }

    #[test]
    fn small_step_starts_at_20m_inside_and_covers_the_fade_span() {
        let t0 = 0.0;
        let fade_span = CLOUD_FADE_M * CLOUD_FADE_MULTIPLE;
        let t1 = t0 + fade_span;
        let steps = 64;
        let near = cloud_small_step(t0, t0, t1, steps, true);
        assert!(
            (near - CLOUD_NEAR_STEP_M).abs() < 1.0e-3,
            "inside the layer the step starts at {CLOUD_NEAR_STEP_M}, got {near}"
        );
        let far = cloud_small_step(t1, t0, t1, steps, true);
        assert!(
            far > near * 4.0,
            "inside step should grow along the ray, {near} -> {far}"
        );
        // The same recurrence the march uses: each step is evaluated at the
        // start of the interval, then the ray advances by that step.
        let mut t = t0;
        for _ in 0..steps {
            t += cloud_small_step(t, t0, t1, steps, true);
        }
        let err = (t - t1).abs();
        assert!(
            err < 150.0,
            "64 inside steps end at {t}, fade span ends at {t1}, err {err}"
        );
        // A short inside segment stays at the cap. It does not inherit the
        // hundreds-of-metres step a long shell would have had.
        let short = cloud_small_step(t0, t0, t0 + 32.0, steps, true);
        assert!(
            (short - CLOUD_NEAR_STEP_M).abs() < 1.0e-3,
            "a short inside segment stays at the cap, got {short}"
        );
        let outside_near = cloud_small_step(t0, t0, t1, steps, false);
        let outside_far = cloud_small_step(t1, t0, t1, steps, false);
        assert!(
            outside_far > outside_near * 2.0,
            "outside step should grow, {outside_near} -> {outside_far}"
        );
        let outside_short = cloud_small_step(t0, t0, t0 + 32.0, steps, false);
        assert!(
            (outside_short - CLOUD_FAR_STEP_FLOOR_M).abs() < 1.0e-3,
            "outside the layer a short segment floors at {CLOUD_FAR_STEP_FLOOR_M}, got {outside_short}"
        );
        assert!((CLOUD_BIG_STEP_RATIO - 4.0).abs() < 1.0e-6);
        assert_eq!(CLOUD_EMPTY_STEPS, 8);
        assert!((CLOUD_SKY_DISTANCE_M - 60_000.0).abs() < 1.0e-3);
        assert!((CLOUD_FADE_M - 45_000.0).abs() < 1.0e-3);
        assert!((CLOUD_FADE_MULTIPLE - 1.5).abs() < 1.0e-6);
    }

    /// Cinematic asks for 192 or 256 inside-layer steps. The ratio must use that count,
    /// not the old 128-term clamp, or the march stops short of the fade span.
    #[test]
    fn inside_steps_above_128_cover_the_fade_span() {
        let t0 = 0.0;
        let t1 = CLOUD_FADE_M * CLOUD_FADE_MULTIPLE;
        let march = |steps: u32| {
            let mut t = t0;
            for _ in 0..steps {
                t += cloud_small_step(t, t0, t1, steps, true);
            }
            t
        };
        for steps in [192, crate::CloudMarch::STEP_LIMIT] {
            let end = march(steps);
            let err = (end - t1).abs();
            assert!(
                err < 500.0,
                "{steps} inside steps end at {end}, fade span ends at {t1}, err {err}"
            );
        }
        let at_end = |steps: u32| cloud_small_step(t1, t0, t1, steps, true);
        let gap = (at_end(192) - at_end(128)).abs();
        assert!(
            gap > 100.0,
            "a 192-step end step collapsed onto the 128-step schedule ({gap} m)"
        );
        let wgsl = include_str!("../shaders/clouds.wgsl");
        let cap = crate::CloudMarch::STEP_LIMIT;
        assert!(
            wgsl.contains(&format!("let count = min(max(n, 0), {cap});")),
            "the shader series sum must accept {cap} terms"
        );
        assert!(wgsl.contains(&format!(
            "for (var i = 0; i < {cap}; i++) {{\n        if (i >= count)"
        )));
    }

    #[test]
    fn bayer_order_visits_every_block_texel_once() {
        let mut seen = [[false; 4]; 4];
        for frame in 0..16 {
            let [x, y] = cloud_bayer_offset(frame);
            assert!(!seen[y as usize][x as usize], "frame {frame} repeats ({x}, {y})");
            seen[y as usize][x as usize] = true;
            assert_eq!(
                cloud_bayer_offset(frame + 16),
                [x, y],
                "the order repeats every 16 frames"
            );
        }
        // Consecutive frames land in different quadrants of the block.
        for frame in 0..16 {
            let a = cloud_bayer_offset(frame);
            let b = cloud_bayer_offset(frame + 1);
            assert!(
                a[0] / 2 != b[0] / 2 || a[1] / 2 != b[1] / 2,
                "frames {frame} and {} share a quadrant",
                frame + 1
            );
        }
    }

    #[test]
    fn fresh_texels_cover_every_low_texel_in_16_frames() {
        for low in [[640_u32, 360], [641, 362], [3, 2], [1, 1]] {
            let fresh = cloud_fresh_extent(low);
            assert_eq!(fresh, [low[0].div_ceil(4), low[1].div_ceil(4)]);
            let mut count = vec![0_u32; (low[0] * low[1]) as usize];
            for frame in 0..16 {
                let sub = cloud_bayer_offset(frame);
                for by in 0..fresh[1] {
                    for bx in 0..fresh[0] {
                        let [x, y] = cloud_fresh_texel([bx, by], sub, low);
                        assert!(x < low[0] && y < low[1]);
                        assert_eq!([x / CLOUD_BLOCK, y / CLOUD_BLOCK], [bx, by], "stays in its block");
                        count[(y * low[0] + x) as usize] += 1;
                    }
                }
            }
            assert!(
                count.iter().all(|&c| c >= 1),
                "{low:?}: a texel was never marched"
            );
            if low[0] % 4 == 0 && low[1] % 4 == 0 {
                assert!(
                    count.iter().all(|&c| c == 1),
                    "{low:?}: whole blocks march each texel once"
                );
            }
        }
    }

    /// The new switches are off by default, so a host that never sets them renders as before.
    #[test]
    fn cloud_shadow_switches_are_off_by_default() {
        let settings = CloudSettings::default();
        assert!(!settings.shadow_map && !settings.fog_coupling && !settings.overcast_ibl);
        assert!(!settings.wants_shadow_map());
        assert_eq!(settings.shadow_map_texels(), CLOUD_SHADOW_MAP_SIZE);
        assert!((settings.shadow_map_extent() - CLOUD_SHADOW_MAP_EXTENT_M).abs() < 1.0e-3);
        assert_eq!(cloud_shadow_params(&settings, false), [0.0; 4]);
        let fog_only = CloudSettings {
            fog_coupling: true,
            ..settings
        };
        assert!(fog_only.wants_shadow_map(), "fog coupling builds the map");
    }

    fn unit_f32(d: [f32; 3]) -> [f32; 3] {
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        [d[0] / len, d[1] / len, d[2] / len]
    }

    /// A smooth stand-in for `density_at`: the cumulus height profile of the (curved) layer
    /// times a slow horizontal pattern. f64 in, so the reference evaluates it in f64.
    fn test_density(p: [f64; 3], planet: f64, settings: &CloudSettings) -> f64 {
        let alt = (p[0] * p[0] + (p[1] + planet) * (p[1] + planet) + p[2] * p[2]).sqrt() - planet;
        let bottom = f64::from(settings.altitude_bottom);
        let top = f64::from(settings.altitude_top);
        if alt < bottom || alt > top {
            return 0.0;
        }
        let h = ((alt - bottom) / (top - bottom)) as f32;
        let pattern = 0.5 + 0.5 * (p[0] / 3000.0).sin() * (p[2] / 2300.0).cos();
        f64::from(type_gradient(h, 0.5)) * pattern
    }

    /// [`cloud_shadow_march`] in f64: the same shells and step positions with `steps` steps.
    fn march_f64(xz: [f64; 2], to_sun: [f64; 3], settings: &CloudSettings, planet: f64, steps: u32) -> f64 {
        let center = [0.0, -planet, 0.0];
        let o = [xz[0], f64::from(settings.altitude_bottom), xz[1]];
        let bottom = shell_f64(o, to_sun, center, planet + f64::from(settings.altitude_bottom))
            .unwrap_or((-1.0, -1.0));
        let top =
            shell_f64(o, to_sun, center, planet + f64::from(settings.altitude_top)).unwrap_or((-1.0, -1.0));
        let t0 = bottom.1.min(0.0);
        let t1 = top.1;
        if t1 <= t0 {
            return 1.0;
        }
        let dt = (t1 - t0) / f64::from(steps);
        let mut od = 0.0;
        for i in 0..steps {
            let t = t0 + (f64::from(i) + 0.5) * dt;
            let p = [o[0] + to_sun[0] * t, o[1] + to_sun[1] * t, o[2] + to_sun[2] * t];
            od += test_density(p, planet, settings) * dt;
        }
        (-od * f64::from(settings.density)).exp()
    }

    /// The f32 map march (the shader's) matches the same 16-step march in f64 within 5e-3 of
    /// transmittance (about 1.3 levels of 255), near the origin and 75 km out (where the curved
    /// base sits 440 m below the plane), and a 4096-step f64 integral within 0.02:
    /// sixteen midpoints are enough for the smooth column the map stores.
    #[test]
    fn the_density_bound_covers_the_weather_and_the_breakup() {
        let empty = CloudSettings::coverage_layer(0.0);
        assert_eq!(cloud_density_bound(&empty, &build_weather_map(&empty)), 0.0);
        let layer = CloudSettings::coverage_layer(0.45);
        let map = build_weather_map(&layer);
        let bound = cloud_density_bound(&layer, &map);
        // A texel of full weather gives a cover of 0.75 + 0.4 × 0.45.
        assert!((bound - 0.93).abs() < 0.01, "{bound}");
        // The breakup can raise a texel to full weather; the cover is clamped at 1.
        let full = CloudSettings {
            coverage: 1.0,
            ..CloudSettings::default().with_genus(crate::CloudGenus::Cumulus)
        };
        assert_eq!(cloud_density_bound(&full, &build_weather_map(&full)), 1.0);
    }

    #[test]
    fn cloud_shadow_march_matches_an_f64_reference() {
        let planet = 6_371_000.0_f64;
        // Extinction low enough that the shadows are partial (T well inside 0..1).
        let settings = CloudSettings {
            density: 0.0008,
            ..CloudSettings::coverage_layer(0.5)
        };
        let suns = [
            unit_f32([0.0, 1.0, 0.0]),
            unit_f32([0.4, 1.0, 0.2]),
            unit_f32([0.8, 0.5, -0.3]),
            unit_f32([-0.9, 0.25, 0.1]),
        ];
        let points = [
            [0.0_f32, 0.0],
            [1200.0, -3400.0],
            [60_000.0, -45_000.0],
            [-8000.0, 9000.0],
        ];
        let mut partial = 0;
        for sun in suns {
            let sun64 = sun.map(f64::from);
            for xz in points {
                let got = cloud_shadow_march(xz, sun, &settings, planet as f32, |p| {
                    test_density(p.map(f64::from), planet, &settings) as f32
                });
                let same = march_f64(
                    xz.map(f64::from),
                    sun64,
                    &settings,
                    planet,
                    CLOUD_SHADOW_MAP_STEPS,
                );
                let fine = march_f64(xz.map(f64::from), sun64, &settings, planet, 4096);
                assert!(
                    (f64::from(got) - same).abs() < 5.0e-3,
                    "f32 march {got} vs f64 {same} (sun {sun:?}, xz {xz:?})"
                );
                assert!(
                    (f64::from(got) - fine).abs() < 0.02,
                    "16 steps {got} vs 4096 steps {fine} (sun {sun:?}, xz {xz:?})"
                );
                if (0.05..0.95).contains(&fine) {
                    partial += 1;
                }
            }
        }
        assert!(
            partial >= 8,
            "the test field should shadow partially, {partial} partial points"
        );
    }

    /// Sub-texel camera moves keep the window, and a world point keeps its texel; a whole-texel
    /// move shifts the window by one cell and the point still keeps its texel (toroidal).
    #[test]
    fn cloud_shadow_window_snaps_to_texels() {
        let size = CLOUD_SHADOW_MAP_SIZE;
        let texel = CLOUD_SHADOW_MAP_EXTENT_M / size as f32;
        let sun = unit_f32([0.3, 0.8, -0.2]);
        let base = 1500.0;
        let point = [2345.6, 0.0, -987.6];
        let point_texel = |start: [i32; 2]| {
            let xz = cloud_shadow_crossing(point, sun, base).expect("sun is up");
            let cell = cloud_shadow_cell(xz, texel);
            assert!(
                cell[0] >= start[0]
                    && cell[0] < start[0] + size as i32
                    && cell[1] >= start[1]
                    && cell[1] < start[1] + size as i32,
                "the point stays inside the window"
            );
            let t = cloud_shadow_texel(cell, size);
            assert_eq!(
                cloud_shadow_texel_cell(t, start, size),
                cell,
                "texel → cell round trip"
            );
            t
        };
        // An eye whose base-plane crossing sits in the middle of a cell.
        let k = (base - 3.0) / sun[1];
        let eye0 = [100.5 * texel - sun[0] * k, 3.0, -39.5 * texel - sun[2] * k];
        let start0 = cloud_shadow_window(eye0, sun, base, size, texel).expect("sun is up");
        let texel0 = point_texel(start0);
        for step in [0.05_f32, 0.2, -0.3, 0.45] {
            let eye = [eye0[0] + step * texel, eye0[1], eye0[2] - step * texel];
            let start = cloud_shadow_window(eye, sun, base, size, texel).expect("sun is up");
            assert_eq!(start, start0, "a {step} texel move keeps the window");
            assert_eq!(point_texel(start), texel0, "and the point's texel");
        }
        let eye = [eye0[0] + texel, eye0[1], eye0[2]];
        let start = cloud_shadow_window(eye, sun, base, size, texel).expect("sun is up");
        assert_eq!(
            start,
            [start0[0] + 1, start0[1]],
            "a whole-texel move shifts the window one cell"
        );
        assert_eq!(
            point_texel(start),
            texel0,
            "toroidal addressing keeps the point's texel"
        );
        // A camera above the layer centres the window on its own column at the base.
        let high =
            cloud_shadow_window([eye0[0], 6000.0, eye0[2]], sun, base, size, texel).expect("sun is up");
        let at_base =
            cloud_shadow_window([eye0[0], base, eye0[2]], sun, base, size, texel).expect("sun is up");
        assert_eq!(high, at_base);
        assert!(cloud_shadow_window(eye0, [1.0, 0.0, 0.0], base, size, texel).is_none());
    }

    /// Sixteen frames of the refresh schedule march every texel exactly once (counted per
    /// texel, not timed); a moved window also marches the strips it brought in.
    #[test]
    fn cloud_shadow_refresh_covers_the_map_in_16_frames() {
        for size in [512_u32, 64, 100] {
            let start = [-7, 13];
            let mut count = vec![0_u32; (size * size) as usize];
            for frame in 0..CLOUD_SHADOW_REFRESH_FRAMES {
                let rects = cloud_shadow_refresh_rects(frame, size, Some(start), start);
                // A still window marches one band (none on the 16th frame of a map whose rows
                // the 15 bands before already covered).
                assert!(rects.len() <= 1, "a still window marches one band, {rects:?}");
                for [x, y, w, h] in rects {
                    for j in y..y + h {
                        for i in x..x + w {
                            count[(j * size + i) as usize] += 1;
                        }
                    }
                }
            }
            if size.is_multiple_of(CLOUD_SHADOW_REFRESH_FRAMES) {
                assert!(
                    count.iter().all(|&c| c == 1),
                    "{size}: each texel once in 16 frames"
                );
            } else {
                assert!(
                    count.iter().all(|&c| c >= 1),
                    "{size}: every texel within 16 frames"
                );
            }
        }
        let size = 64;
        assert_eq!(
            cloud_shadow_refresh_rects(3, size, None, [0, 0]),
            vec![[0, 0, size, size]]
        );
        assert_eq!(
            cloud_shadow_refresh_rects(3, size, Some([0, 0]), [16, 0]),
            vec![[0, 0, size, size]],
            "a quarter-map jump marches it whole"
        );
        // Moving +3 cells in x brings in cells 64..67, which live in texel columns 0..3; moving
        // −2 in z brings in cells −2..0, texel rows 62..64.
        let rects = cloud_shadow_refresh_rects(0, size, Some([0, 0]), [3, -2]);
        assert!(rects.contains(&[0, 0, 3, size]), "{rects:?}");
        assert!(rects.contains(&[0, 62, size, 2]), "{rects:?}");
        // A strip that wraps the texel edge splits in two.
        let rects = cloud_shadow_refresh_rects(0, size, Some([62, 0]), [66, 0]);
        assert!(
            rects.contains(&[62, 0, 2, size]) && rects.contains(&[0, 0, 2, size]),
            "{rects:?}"
        );
        // Every entered cell's texel is covered.
        for (prev, start) in [([0, 0], [5, 0]), ([10, 10], [7, 13]), ([-3, 60], [-1, 58])] {
            let rects = cloud_shadow_refresh_rects(9, size, Some(prev), start);
            for cx in start[0]..start[0] + size as i32 {
                for cz in start[1]..start[1] + size as i32 {
                    let old = cx >= prev[0]
                        && cx < prev[0] + size as i32
                        && cz >= prev[1]
                        && cz < prev[1] + size as i32;
                    if old {
                        continue;
                    }
                    let [tx, tz] = cloud_shadow_texel([cx, cz], size);
                    assert!(
                        rects
                            .iter()
                            .any(|&[x, y, w, h]| tx >= x && tx < x + w && tz >= y && tz < y + h),
                        "entered cell ({cx}, {cz}) not marched ({prev:?} → {start:?})"
                    );
                }
            }
        }
    }

    /// rc1-rev-render-k 5: a map edge whose square passes `u32` (65 536 squared wraps to 0) is
    /// lit, not a wrapped size check that reads past the slice.
    #[test]
    fn cloud_shadow_lookup_refuses_a_size_whose_square_overflows() {
        let settings = CloudSettings::coverage_layer(0.5);
        let params = cloud_shadow_params(&settings, true);
        let texels = vec![0.2f32; 64];
        let (sun, eye) = ([0.0, 1.0, 0.0], [0.0, 2.0, 0.0]);
        for size in [65_536u32, 65_537, 100_000, u32::MAX] {
            assert_eq!(
                cloud_shadow_lookup(&texels, size, params, [1.0, 0.0, 1.0], sun, eye),
                1.0,
                "{size}"
            );
        }
        assert!(cloud_shadow_lookup(&texels, 8, params, [1.0, 0.0, 1.0], sun, eye) < 1.0);
    }

    /// The lookup reads the texel under a point's base-plane crossing, leaves points above the
    /// layer lit, keeps only the part of the column above a point inside the layer, and fades
    /// to lit at the window edge.
    #[test]
    fn cloud_shadow_lookup_follows_the_sun_line() {
        let size = 64;
        let settings = CloudSettings::coverage_layer(0.5);
        let params = cloud_shadow_params(&settings, true);
        let extent = params[0];
        let texel = extent / size as f32;
        // Half the map shadowed: texel columns 0..32 hold 0.2, the rest 1.
        let texels: Vec<f32> = (0..size * size)
            .map(|i| if i % size < 32 { 0.2 } else { 1.0 })
            .collect();
        let sun = [0.0, 1.0, 0.0];
        let eye = [0.0, 2.0, 0.0];
        let shadowed = [10.5 * texel, 0.0, 3.5 * texel];
        // Cell −24 lives in texel column 40 (toroidal), inside the window around the eye.
        let lit = [-23.5 * texel, 0.0, 3.5 * texel];
        assert!((cloud_shadow_lookup(&texels, size, params, shadowed, sun, eye) - 0.2).abs() < 1.0e-5);
        assert!((cloud_shadow_lookup(&texels, size, params, lit, sun, eye) - 1.0).abs() < 1.0e-5);
        let above = [shadowed[0], settings.altitude_top + 10.0, shadowed[2]];
        assert_eq!(cloud_shadow_lookup(&texels, size, params, above, sun, eye), 1.0);
        let mid = [
            shadowed[0],
            0.5 * (settings.altitude_bottom + settings.altitude_top),
            shadowed[2],
        ];
        let half = cloud_shadow_lookup(&texels, size, params, mid, sun, eye);
        assert!(
            (half - 0.2_f32.sqrt()).abs() < 1.0e-4,
            "mid-layer keeps half the depth, {half}"
        );
        assert_eq!(
            cloud_shadow_lookup(&texels, size, [0.0; 4], shadowed, sun, eye),
            1.0,
            "off"
        );
        // Past the window edge: lit.
        let far = [0.6 * extent, 0.0, 0.0];
        assert_eq!(cloud_shadow_lookup(&texels, size, params, far, sun, eye), 1.0);
        // A slanted sun reads the texel the line meets at the base, not the one overhead.
        let slant = unit_f32([1.0, 1.0, 0.0]);
        let base = settings.altitude_bottom;
        // Column 2 (shadowed) along the sun line; straight overhead it would read column 61 (lit).
        let under_shadow = [2.5 * texel - base, 0.0, 3.5 * texel];
        let slanted = cloud_shadow_lookup(&texels, size, params, under_shadow, slant, eye);
        assert!(
            (slanted - 0.2).abs() < 1.0e-4,
            "the sun-line texel, not the overhead one: {slanted}"
        );
    }

    /// ADR-5944: the span is the part of the ray the lookup can darken. With a map that holds 0
    /// everywhere the lookup is below 1 exactly inside the window and under the layer's top, so
    /// walking the ray finds the same stretch, for suns overhead and slanted and eyes under, in
    /// and over the layer.
    #[test]
    fn cloud_air_span_is_where_the_lookup_can_shadow() {
        let size = 64;
        let settings = CloudSettings::coverage_layer(0.5);
        let params = cloud_shadow_params(&settings, true);
        let texels = vec![0.0_f32; (size * size) as usize];
        let top = settings.altitude_top;
        let mut checked = 0;
        for to_sun in [
            [0.0, 1.0, 0.0],
            unit_f32([0.0, 0.574, 0.819]),
            unit_f32([-0.6, 0.3, 0.2]),
        ] {
            for eye in [
                [0.0, 1200.0, 0.0],
                [300.0, 2.0, -40.0],
                [0.0, 2600.0, 0.0],
                [0.0, top + 900.0, 0.0],
            ] {
                for view in [
                    unit_f32([0.0, -0.555, 0.832]),
                    unit_f32([0.7, -0.1, 0.7]),
                    unit_f32([-1.0, 0.0, 0.0]),
                    unit_f32([0.3, 0.25, -0.9]),
                    unit_f32([0.0, -1.0, 0.0]),
                ] {
                    let dist = 26_000.0;
                    let got = cloud_air_span(params, eye, view, dist, to_sun);
                    let [a, b] = got.span_m;
                    let (mut under, steps) = (0u32, 2600u32);
                    for i in 0..steps {
                        let t = (i as f32 + 0.5) * dist / steps as f32;
                        let p = [eye[0] + view[0] * t, eye[1] + view[1] * t, eye[2] + view[2] * t];
                        under += u32::from(p[1] < top);
                        let shadowed = cloud_shadow_lookup(&texels, size, params, p, to_sun, eye) < 1.0;
                        // The walk's samples are 10 m apart: leave the ends of the span alone.
                        if (t - a).abs() > 15.0 && (t - b).abs() > 15.0 {
                            let inside = t > a && t < b;
                            assert_eq!(
                                shadowed, inside,
                                "sun {to_sun:?} eye {eye:?} view {view:?} t {t}: span {a}..{b}"
                            );
                            checked += 1;
                        }
                    }
                    let share = under as f32 / steps as f32;
                    assert!(
                        (got.under_top - share).abs() < 2.0e-3,
                        "under_top {} against {share} walked (eye {eye:?} view {view:?})",
                        got.under_top
                    );
                }
            }
        }
        assert!(checked > 100_000);
        // No span with the map off, a sun at the horizon, an eye above the layer looking up or
        // a point at the eye.
        let down = unit_f32([0.0, -0.555, 0.832]);
        let eye = [0.0, 1200.0, 0.0];
        let sun = [0.0, 1.0, 0.0];
        assert_eq!(
            cloud_air_span([0.0; 4], eye, down, 2000.0, sun),
            CloudAirSpan::NONE
        );
        assert_eq!(
            cloud_air_span(params, eye, down, 2000.0, unit_f32([1.0, 0.001, 0.0])),
            CloudAirSpan::NONE
        );
        assert_eq!(
            cloud_air_span(params, [0.0, top + 5.0, 0.0], [0.0, 1.0, 0.0], 2000.0, sun),
            CloudAirSpan::NONE
        );
        assert_eq!(cloud_air_span(params, eye, down, 0.0, sun), CloudAirSpan::NONE);
        assert_eq!(
            cloud_air_span(params, eye, down, f32::NAN, sun),
            CloudAirSpan::NONE
        );
        // A ground view under the layer, sun overhead: the whole ray, all of it under the top.
        let whole = cloud_air_span(params, eye, down, 2000.0, sun);
        assert_eq!(whole.span_m, [0.0, 2000.0]);
        assert_eq!(whole.under_top, 1.0);
    }

    /// The haze's mean over the ray: 1 under a clear map or with the map off, the map's value
    /// under a full cover, and the lit share of a ray that climbs out of the layer.
    #[test]
    fn cloud_shadow_on_ray_is_the_mean_of_the_lookup() {
        let size = 64;
        let settings = CloudSettings::coverage_layer(0.5);
        let params = cloud_shadow_params(&settings, true);
        let sun = [0.0, 1.0, 0.0];
        let eye = [0.0, 2.0, 0.0];
        let ground = [900.0, 0.0, 1200.0];
        let clear = vec![1.0_f32; (size * size) as usize];
        let closed = vec![0.25_f32; (size * size) as usize];
        assert_eq!(cloud_shadow_on_ray(&clear, size, params, eye, ground, sun), 1.0);
        assert_eq!(
            cloud_shadow_on_ray(&closed, size, [0.0; 4], eye, ground, sun),
            1.0
        );
        let under = cloud_shadow_on_ray(&closed, size, params, eye, ground, sun);
        assert!((under - 0.25).abs() < 1.0e-5, "under a 0.25 cover: {under}");
        // Straight up to twice the layer's top: the two midpoints above the top are lit, one is
        // under the base (0.25) and one inside the layer (between 0.25 and 1).
        let high = [0.0, 2.0 * settings.altitude_top, 0.0];
        let climbing = cloud_shadow_on_ray(&closed, size, params, eye, high, sun);
        let inside = cloud_shadow_lookup(
            &closed,
            size,
            params,
            [0.0, 2.0 + (high[1] - 2.0) * 0.375, 0.0],
            sun,
            eye,
        );
        assert!(inside > 0.25 && inside < 1.0);
        assert!(
            (climbing - (0.25 + inside + 2.0) / 4.0).abs() < 1.0e-5,
            "a ray out of the layer: {climbing}"
        );
        assert_eq!(HAZE_CLOUD_STEPS, 4);
    }

    /// ADR-5944: the sun's mean cover of the ground: nothing without a layer, the covered share
    /// of a fair sky, all of the sun under a closed deck (where the overcast factor, a measure
    /// of the sky light's dimming, still leaves a share of it).
    #[test]
    fn sun_cover_is_the_covered_share_of_the_sun() {
        let terms = |settings: &CloudSettings| overcast_map_terms(settings, &build_weather_map(settings));
        let clear = CloudSettings::default();
        assert_eq!(sun_cover_from_terms(&clear, terms(&clear)), 0.0);
        let deck = CloudSettings {
            pattern: CloudPattern::Constant,
            ..CloudSettings::coverage_layer(0.95)
        };
        let closed = sun_cover_from_terms(&deck, terms(&deck));
        assert!(closed > 0.99, "a closed deck takes the sun: {closed}");
        assert!(
            cloud_overcast_factor(&deck, &build_weather_map(&deck)) < closed,
            "the sky light's factor is the weaker one"
        );
        let fair = CloudSettings::coverage_layer(0.3);
        let some = sun_cover_from_terms(&fair, terms(&fair));
        assert!(
            some > 0.02 && some < 0.6,
            "a fair sky shadows part of the ground: {some}"
        );
        let cloudy = CloudSettings::coverage_layer(0.7);
        assert!(
            sun_cover_from_terms(&cloudy, terms(&cloudy)) > some,
            "more cover, less sun"
        );
        let thin = CloudSettings { density: 0.0, ..deck };
        assert_eq!(
            sun_cover_from_terms(&thin, terms(&thin)),
            0.0,
            "no density, no shadow"
        );
    }

    /// No clouds, no overcast; a fuller layer is more overcast; factor 0 leaves the sky's bits.
    #[test]
    fn overcast_factor_grows_with_coverage() {
        let clear = CloudSettings::default();
        assert_eq!(cloud_overcast_factor(&clear, &build_weather_map(&clear)), 0.0);
        let half = CloudSettings::coverage_layer(0.5);
        let full = CloudSettings::coverage_layer(1.0);
        let o_half = cloud_overcast_factor(&half, &build_weather_map(&half));
        let o_full = cloud_overcast_factor(&full, &build_weather_map(&full));
        assert!(
            o_half > 0.1 && o_half < o_full && o_full <= 1.0,
            "{o_half} {o_full}"
        );
        let sky = [0.31, 0.47, 0.93];
        assert_eq!(overcast_sky(sky, 0.0), sky);
        let grey = overcast_sky(sky, o_full);
        let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        assert!(luma(grey) < luma(sky), "overcast dims the sky");
        assert!((grey[2] - grey[0]) < (sky[2] - sky[0]), "and greys it");
    }
}

/// Weather coupling (ADR-429): drift integration, the cached weather field, weather cuts and the
/// steady-state work budget.
#[cfg(test)]
mod weather_tests {
    use super::*;

    /// A `map_resolution` whose `u32` square wraps to 0 is clamped, not an empty buffer.
    #[test]
    fn oversized_map_resolution_is_clamped_not_wrapped() {
        let settings = CloudSettings {
            map_resolution: 65_536,
            pattern: CloudPattern::Constant,
            ..CloudSettings::default()
        };
        assert_eq!(map_n(&settings), MAX_MAP_RESOLUTION);
        assert_eq!(texel_count(map_n(&settings)), 4096 * 4096);
        assert_eq!(texel_count(65_536), 1 << 32);
    }

    /// Without a drift offset the march's wind row is the one the shader always read (frames
    /// are unchanged bit for bit); with one it carries the whole drift premultiplied.
    #[test]
    fn drift_offset_zero_keeps_the_wind_row() {
        let settings = CloudSettings {
            wind_m_per_s: [6.0, -2.0],
            time_s: 12.5,
            ..CloudSettings::coverage_layer(0.5)
        };
        assert_eq!(settings.wind_uniform(), [6.0, -2.0, 12.5, settings.domain_m]);
        assert_eq!(settings.drift_m(), [75.0, -25.0]);
        let offset = CloudSettings {
            drift_offset_m: [-30.0, 4.0],
            ..settings
        };
        assert_eq!(offset.drift_m(), [45.0, -21.0]);
        assert_eq!(offset.wind_uniform(), [45.0, -21.0, 1.0, settings.domain_m]);
    }

    /// The CPU weather sample follows the integrated drift: an offset moves the field exactly
    /// like the same distance of `wind × time`.
    #[test]
    fn weather_sample_follows_the_integrated_drift() {
        let settings = CloudSettings {
            map_resolution: 32,
            ..CloudSettings::coverage_layer(0.5)
        };
        let map = build_weather_map(&settings);
        let by_offset = CloudSettings {
            drift_offset_m: [700.0, -300.0],
            ..settings
        };
        let by_wind = CloudSettings {
            wind_m_per_s: [7.0, -3.0],
            time_s: 100.0,
            ..settings
        };
        for xz in [[0.0, 0.0], [1234.0, -987.0], [-5000.0, 220.0]] {
            let a = map.sample(xz, &by_offset);
            let b = map.sample(xz, &by_wind);
            assert!((a - b).abs() < 1.0e-5, "{xz:?}: {a} vs {b}");
            let moved = map.sample([xz[0] - 700.0, xz[1] + 300.0], &settings);
            assert!((a - moved).abs() < 1.0e-5, "{xz:?}: {a} vs {moved}");
        }
    }

    /// The cached field gives the same map, byte for byte, as a full build at every coverage and
    /// type, so a transition re-thresholds it in one pass and the frames match a fresh renderer.
    /// Another seed or resolution falls back to a full build.
    #[test]
    fn weather_field_maps_equal_the_full_build() {
        let base = CloudSettings {
            map_resolution: 48,
            ..CloudSettings::default()
        };
        let before = cloud_work_counters();
        let field = WeatherField::new(&base);
        assert_eq!(cloud_work_counters().since(before).weather_fields, 1);
        for coverage in [0.0, 0.004, 0.1, 0.37, 0.5, 0.93, 1.0] {
            for cloud_type in [0.0, 0.5, 0.85] {
                let settings = CloudSettings {
                    coverage,
                    cloud_type,
                    ..base
                };
                let cached = field.weather_map(&settings);
                let full = build_weather_map(&settings);
                assert_eq!(cached.rgba, full.rgba, "coverage {coverage} type {cloud_type}");
                assert_eq!(cached.filled.to_bits(), full.filled.to_bits());
            }
        }
        let reseeded = CloudSettings {
            seed: 99,
            coverage: 0.4,
            ..base
        };
        assert!(!field.matches(&reseeded));
        assert_eq!(
            field.weather_map(&reseeded).rgba,
            build_weather_map(&reseeded).rgba
        );
        let split = CloudSettings {
            pattern: CloudPattern::SplitHemisphere,
            ..base
        };
        assert_eq!(field.weather_map(&split).rgba, build_weather_map(&split).rgba);
        assert_eq!(
            cloud_work_counters().since(before).weather_fields,
            1,
            "maps from the field build no new field"
        );
    }

    /// One frame of a transition is not a cut; a jump is.
    #[test]
    fn weather_cuts_split_jumps_from_transitions() {
        let at = |coverage: f32, cloud_type: f32, density: f32| CloudWeatherSample {
            coverage,
            cloud_type,
            density,
        };
        let base = at(0.4, 0.5, 0.04);
        assert!(!cloud_weather_cut(base, base));
        // A one-second clear-to-overcast transition at 60 fps.
        assert!(!cloud_weather_cut(base, at(0.4 + 1.0 / 60.0, 0.5, 0.04)));
        assert!(!cloud_weather_cut(base, at(0.4, 0.5 + 1.0 / 60.0, 0.04 * 1.02)));
        assert!(cloud_weather_cut(base, at(0.5, 0.5, 0.04)), "coverage jump");
        assert!(cloud_weather_cut(base, at(0.4, 0.7, 0.04)), "type jump");
        assert!(cloud_weather_cut(base, at(0.4, 0.5, 0.06)), "density jump");
        assert!(cloud_weather_cut(base, at(0.4, 0.5, 0.0)), "density to zero");
        assert!(cloud_weather_cut(base, at(f32::NAN, 0.5, 0.04)), "NaN coverage");
        assert!(
            cloud_weather_cut(base, at(0.4, 0.5, f32::INFINITY)),
            "infinite density"
        );
        assert!(!cloud_weather_cut(at(0.4, 0.5, 0.0), at(0.4, 0.5, 0.0)));
        // Out-of-range values clamp like the march does.
        assert!(!cloud_weather_cut(at(1.0, 0.5, 0.04), at(1.3, 0.5, 0.04)));
    }

    /// The weather motion the reconstruction clamps by: 0 for steady weather (the frame is
    /// unchanged), growing with the step, 1 at a cut.
    #[test]
    fn weather_motion_is_zero_when_still_and_one_at_a_cut() {
        let base = CloudSettings::coverage_layer(0.4).weather_sample();
        assert_eq!(cloud_weather_motion(base, base).to_bits(), 0.0_f32.to_bits());
        let small = CloudWeatherSample {
            coverage: 0.41,
            ..base
        };
        let larger = CloudWeatherSample {
            coverage: 0.43,
            ..base
        };
        let a = cloud_weather_motion(base, small);
        let b = cloud_weather_motion(base, larger);
        assert!(a > 0.15 && a < 0.25, "0.01 of coverage is a fifth of a cut: {a}");
        assert!(b > a && b < 1.0, "{a} then {b}");
        let thicker = CloudWeatherSample {
            density: base.density * 1.1,
            ..base
        };
        let d = cloud_weather_motion(base, thicker);
        assert!(d > 0.3 && d < 0.6, "a 10 % density step: {d}");
        let cut = CloudWeatherSample {
            coverage: 0.9,
            ..base
        };
        assert_eq!(cloud_weather_motion(base, cut), 1.0);
        assert_eq!(
            cloud_weather_motion(
                base,
                CloudWeatherSample {
                    density: f32::NAN,
                    ..base
                }
            ),
            1.0
        );
    }

    /// The shipped Desktop High layer at 1440p fits the per-frame sample budget with the map on;
    /// without reprojection the same frame is far over it (the budget is the reprojection's).
    #[test]
    fn steady_cloud_work_fits_the_budget() {
        let settings = CloudSettings {
            shadow_map: true,
            fog_coupling: true,
            ..CloudSettings::coverage_layer(0.5)
        };
        let high = cloud_steady_work(&settings, 2560, 1440, settings.steps, true);
        assert_eq!(high.view_texels, 320 * 180);
        assert_eq!(high.map_texels, 512 * 32);
        assert_eq!(high.map_samples, 512 * 32 * u64::from(CLOUD_SHADOW_MAP_STEPS));
        assert!(
            high.samples() <= CLOUD_FRAME_SAMPLE_BUDGET,
            "Desktop High 1440p: {} samples over the budget {CLOUD_FRAME_SAMPLE_BUDGET}",
            high.samples()
        );
        let hd = cloud_steady_work(&settings, 1920, 1080, settings.steps, true);
        assert!(hd.samples() < high.samples());
        let off = CloudSettings {
            shadow_map: false,
            fog_coupling: false,
            ..settings
        };
        assert_eq!(cloud_steady_work(&off, 2560, 1440, 64, true).map_texels, 0);
        let full = cloud_steady_work(&settings, 2560, 1440, settings.steps, false);
        assert_eq!(full.view_texels, 1280 * 720);
        assert!(full.samples() > 8 * CLOUD_FRAME_SAMPLE_BUDGET);
        // 64² at half resolution is 32² low texels, 8² fresh ones; the step count is the
        // shader's loop bound at most.
        assert_eq!(
            cloud_steady_work(&settings, 64, 64, 1000, true).view_samples,
            8 * 8 * 96
        );
    }
}
