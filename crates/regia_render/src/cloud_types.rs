// SPDX-License-Identifier: MPL-2.0
//! Cloud genera for the volumetric layer (ADR-1408): physically motivated presets and the look
//! controls they need beyond the weather values.
//!
//! A [`CloudGenus`] is one kind of cloud from the WMO International Cloud Atlas (2017): fair
//! cumulus (humilis), heavier cumulus (mediocris to congestus), broken stratocumulus, and cirrus.
//! [`CloudGenus::layer`] gives its base and top heights, extinction, coverage, shape scales and a
//! [`CloudLook`]; [`CloudGenusLayer::apply`] writes that into a [`CloudSettings`].
//!
//! [`CloudLook`] holds what the presets change in the march (`clouds.wgsl`), each defaulting to
//! the layer as it was drawn before, so a host that never sets it renders the same pixels:
//!
//! - **Diffusion.** The multiple-scattered sunlight inside a cloud as a two-stream diffuse field.
//!   For conservative scattering the diffuse transmission of a slab of optical depth `τ` falls
//!   off as `1 / (1 + ¾ (1 − g) τ)` (Bohren, "Multiple scattering of light and some of its
//!   observable consequences", Am. J. Phys. 55, 1987), not as `exp(−τ)`. With a physical
//!   extinction (0.03–0.1 /m) the sun-ward optical depth inside a cumulus is tens, so the
//!   exponential octaves leave the core lit by the sky alone and every cloud reads as one flat
//!   grey. The diffuse term keeps the shadowed side a third or so of the sunlit side, as in
//!   photographs. 0 keeps the two Wrenninge octaves.
//! - **Erosion.** How deep the detail noise bites into the base shape (0.5 before).
//! - **Breakup.** A second, finer coverage noise added to the weather map, so the edges of a
//!   cloud field break into lobes and cells instead of following the smooth weather blobs.
//! - **Cirrus.** A thin high ice sheet ([`CirrusSheet`]): fibrous streaks along the wind, single
//!   scattering with a strongly forward ice phase function, aerial perspective in front.
//!
//! Sources for the numbers are in each preset's comment and in `docs/provenance.md`.

use crate::clouds::CloudSettings;

/// Asymmetry of the water-droplet phase function in the visible (Mie, droplet radius about
/// 10 µm; Bohren 1987 and Hansen & Travis 1974 give 0.85–0.87). Used by the diffusion term.
pub const CLOUD_DROPLET_ASYMMETRY: f32 = 0.85;

/// Forward asymmetry of the cirrus ice phase function (hexagonal columns and plates, Macke,
/// Mueller & Raschke 1996 and Baran 2012: g ≈ 0.75–0.8).
pub const CIRRUS_ICE_ASYMMETRY: f32 = 0.75;

/// The diffuse (multiply scattered) transmission of a conservative-scattering slab of optical
/// depth `tau` and phase asymmetry `g`: `1 / (1 + ¾ (1 − g) τ)` (two-stream, Bohren 1987).
/// 1 at `tau = 0`, falling as `1 / τ` for a thick cloud. Mirrors `diffuse_transmission` in
/// `clouds.wgsl`.
#[must_use]
pub fn diffuse_transmission(tau: f32, g: f32) -> f32 {
    let tau = if tau.is_finite() { tau.max(0.0) } else { 0.0 };
    1.0 / (1.0 + 0.75 * (1.0 - g.clamp(-1.0, 1.0)) * tau)
}

/// A thin high ice-cloud sheet above the volumetric layer. Coverage 0 (the default) draws none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CirrusSheet {
    /// Fraction of the sky the streaks cover, `0..1`. 0 draws no cirrus.
    pub coverage: f32,
    /// Height of the sheet above the ground (metres). Cirrus sits in the high étage, 5–13 km in
    /// temperate latitudes (WMO International Cloud Atlas 2017).
    pub altitude_m: f32,
    /// Vertical optical depth of a full streak. Thin cirrus is 0.03–0.3 (Sassen & Cho 1992).
    pub optical_depth: f32,
    /// Width of one streak tile across the wind (metres).
    pub scale_m: f32,
    /// How many times longer than wide the streaks are, along the wind.
    pub stretch: f32,
    /// Strength of the combed fiber structure, `0..1` (ADR-1509): strands that follow each
    /// fallstreak and fan out with it, and faint patches of fibers between the trails. 0 (the
    /// default) keeps the ADR-1470 trails.
    pub fiber: f32,
    /// Spacing of the fibers across the shear at a trail's head (metres; at least 10).
    pub fiber_scale_m: f32,
    /// Direction of the wind shear the trails and fibers lie along, in degrees from the wind
    /// direction (counterclockwise seen from above, `-180..180`).
    pub shear_deg: f32,
    /// Amount of wind shear, `0..1` (ADR-1509): trails grow longer downwind, and their tails fall
    /// up to [`CIRRUS_FALL_DEPTH_M`] below the sheet, so seen at a slant they hang toward the
    /// horizon. 0 keeps the flat trails.
    pub shear: f32,
    /// Patchy breakup of the sheet, `0..1` (ADR-1509): broken patches instead of continuous
    /// bands. 0 keeps the bands whole.
    pub breakup: f32,
    /// Uncinus hooks, `0..1` (ADR-1509): a dense tuft at each head and a tail that bends as the
    /// wind veers with depth. 0 keeps the random gentle bends.
    pub hook: f32,
    /// Crisp, photographic strands, `0..1` (ADR-1516): above 0 the sheet is supersampled
    /// ([`CIRRUS_CRISP_SAMPLES`] per march texel, a cost step) and its warp and bands
    /// cross-fade from the bilinear reads to a smooth cubic B-spline (no saw-tooth kinks) by
    /// the amount, so small values change the sheet continuously; strands sharpen as far as
    /// the sample footprint allows, with less ice between them and their peaks unclipped, at
    /// about the same mean optical depth. 0 (the default) keeps the ADR-1509 sheet bit-for-bit.
    pub crisp: f32,
}

impl CirrusSheet {
    /// No cirrus.
    pub const NONE: Self = Self {
        coverage: 0.0,
        altitude_m: 9_000.0,
        optical_depth: 0.2,
        scale_m: 2_500.0,
        stretch: 6.0,
        fiber: 0.0,
        fiber_scale_m: 150.0,
        shear_deg: 0.0,
        shear: 0.0,
        breakup: 0.0,
        hook: 0.0,
        crisp: 0.0,
    };

    /// The fibrous values the look round tuned by eye on the showcase shot s1v t11 (ADR-1509),
    /// to set over a sheet with [`CirrusSheet::with_fibrous_look`]. Not a default: a sheet keeps
    /// its ADR-1470 trails until a fibrous value is set.
    pub const FIBROUS: Self = Self {
        fiber: 1.0,
        fiber_scale_m: 120.0,
        shear_deg: 0.0,
        shear: 0.6,
        breakup: 0.6,
        hook: 0.45,
        ..Self::NONE
    };

    /// This sheet with the fibrous values of [`CirrusSheet::FIBROUS`] (coverage, height,
    /// optical depth, tile and stretch kept).
    #[must_use]
    pub fn with_fibrous_look(self) -> Self {
        let f = Self::FIBROUS;
        Self {
            fiber: f.fiber,
            fiber_scale_m: f.fiber_scale_m,
            shear_deg: f.shear_deg,
            shear: f.shear,
            breakup: f.breakup,
            hook: f.hook,
            ..self
        }
    }

    /// Whether any fibrous value (fiber, shear, breakup, hook, crisp) is set, so the sheet
    /// draws the ADR-1509 field ([`cirrus_density_fibrous`]) instead of the ADR-1470 trails.
    #[must_use]
    pub fn is_fibrous(&self) -> bool {
        let s = self.sanitized();
        s.fiber > 0.0 || s.shear > 0.0 || s.breakup > 0.0 || s.hook > 0.0 || s.crisp > 0.0
    }

    /// This sheet with crisp strands at `crisp` (`0..1`, ADR-1516); every other value kept.
    #[must_use]
    pub fn with_crisp(self, crisp: f32) -> Self {
        Self { crisp, ..self }
    }

    /// The sheet with every value finite and in range (coverage `0..1`, altitude 2–20 km,
    /// optical depth `0..4`, scale at least 100 m, stretch `1..32`; fiber, shear, breakup, hook
    /// and crisp `0..1`, fiber scale at least 10 m, shear direction `-180..180`°).
    #[must_use]
    pub fn sanitized(self) -> Self {
        let pick = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
        let none = Self::NONE;
        Self {
            coverage: pick(self.coverage, 0.0).clamp(0.0, 1.0),
            altitude_m: pick(self.altitude_m, none.altitude_m).clamp(2_000.0, 20_000.0),
            optical_depth: pick(self.optical_depth, none.optical_depth).clamp(0.0, 4.0),
            scale_m: pick(self.scale_m, none.scale_m).max(100.0),
            stretch: pick(self.stretch, none.stretch).clamp(1.0, 32.0),
            fiber: pick(self.fiber, 0.0).clamp(0.0, 1.0),
            fiber_scale_m: pick(self.fiber_scale_m, none.fiber_scale_m).max(10.0),
            shear_deg: pick(self.shear_deg, 0.0).clamp(-180.0, 180.0),
            shear: pick(self.shear, 0.0).clamp(0.0, 1.0),
            breakup: pick(self.breakup, 0.0).clamp(0.0, 1.0),
            hook: pick(self.hook, 0.0).clamp(0.0, 1.0),
            crisp: pick(self.crisp, 0.0).clamp(0.0, 1.0),
        }
    }

    /// `a` blended toward `b` by `t` (`0..1`), every field linearly; `shear_deg` blends across
    /// the ±180° wrap (the shortest arc), so directions straddling it do not swing through 0.
    #[must_use]
    pub fn lerp(a: Self, b: Self, t: f32) -> Self {
        if !t.is_finite() {
            return a;
        }
        let t = t.clamp(0.0, 1.0);
        // The ends are exact: the wrap below rounds (a shear of -18.183216 came back as
        // -18.183228 at `t = 1`, and 180 as -180).
        if t <= 0.0 {
            return a;
        }
        if t >= 1.0 {
            return b;
        }
        let mix = |x: f32, y: f32| x + (y - x) * t;
        // Shortest-arc delta on the circle, result wrapped back into -180..180.
        let delta = (b.shear_deg - a.shear_deg).rem_euclid(360.0);
        let delta = if delta > 180.0 { delta - 360.0 } else { delta };
        let shear_deg = (a.shear_deg + delta * t + 180.0).rem_euclid(360.0) - 180.0;
        Self {
            coverage: mix(a.coverage, b.coverage),
            altitude_m: mix(a.altitude_m, b.altitude_m),
            optical_depth: mix(a.optical_depth, b.optical_depth),
            scale_m: mix(a.scale_m, b.scale_m),
            stretch: mix(a.stretch, b.stretch),
            fiber: mix(a.fiber, b.fiber),
            fiber_scale_m: mix(a.fiber_scale_m, b.fiber_scale_m),
            shear_deg,
            shear: mix(a.shear, b.shear),
            breakup: mix(a.breakup, b.breakup),
            hook: mix(a.hook, b.hook),
            crisp: mix(a.crisp, b.crisp),
        }
    }
}

impl Default for CirrusSheet {
    fn default() -> Self {
        Self::NONE
    }
}

/// How the volumetric layer is lit and shaped beyond coverage, type and density (ADR-1408).
/// [`CloudLook::LEGACY`] (the default) draws the layer exactly as before it existed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CloudLook {
    /// Strength of the two-stream diffuse sunlight inside the cloud ([`diffuse_transmission`]);
    /// about 2 is physical (the diffuse field's mean radiance is about `2 E T / 4π`). 0 keeps
    /// the two exponential multiple-scattering octaves.
    pub diffusion: f32,
    /// Depth of the detail erosion into the base shape, `0..1` (0.5 before ADR-1408).
    pub erosion: f32,
    /// Amplitude of the fine coverage noise that breaks the cloud field's edges, `0..1`
    /// (0: the weather map alone).
    pub breakup: f32,
    /// World size of one tile of that noise (metres).
    pub breakup_scale_m: f32,
    /// The high cirrus sheet.
    pub cirrus: CirrusSheet,
}

impl CloudLook {
    /// The look before ADR-1408: exponential octaves, erosion 0.5, no breakup, no cirrus.
    pub const LEGACY: Self = Self {
        diffusion: 0.0,
        erosion: 0.5,
        breakup: 0.0,
        breakup_scale_m: 3_000.0,
        cirrus: CirrusSheet::NONE,
    };

    /// Every value finite and in range.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let pick = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
        let legacy = Self::LEGACY;
        Self {
            diffusion: pick(self.diffusion, 0.0).clamp(0.0, 8.0),
            erosion: pick(self.erosion, legacy.erosion).clamp(0.0, 1.0),
            breakup: pick(self.breakup, 0.0).clamp(0.0, 1.0),
            breakup_scale_m: pick(self.breakup_scale_m, legacy.breakup_scale_m).max(100.0),
            cirrus: self.cirrus.sanitized(),
        }
    }

    /// `a` blended toward `b` by `t` (`0..1`), every field linearly.
    #[must_use]
    pub fn lerp(a: Self, b: Self, t: f32) -> Self {
        if !t.is_finite() {
            return a;
        }
        let t = t.clamp(0.0, 1.0);
        let mix = |x: f32, y: f32| x + (y - x) * t;
        Self {
            diffusion: mix(a.diffusion, b.diffusion),
            erosion: mix(a.erosion, b.erosion),
            breakup: mix(a.breakup, b.breakup),
            breakup_scale_m: mix(a.breakup_scale_m, b.breakup_scale_m),
            cirrus: CirrusSheet::lerp(a.cirrus, b.cirrus, t),
        }
    }

    /// The march uniform's `look` row: diffusion, erosion, breakup, breakup tile (m).
    #[must_use]
    pub fn look_row(&self) -> [f32; 4] {
        let look = self.sanitized();
        [look.diffusion, look.erosion, look.breakup, look.breakup_scale_m]
    }

    /// The march uniform's `cirrus` row: coverage, altitude (m), optical depth, streak tile (m).
    #[must_use]
    pub fn cirrus_row(&self) -> [f32; 4] {
        let c = self.cirrus.sanitized();
        [c.coverage, c.altitude_m, c.optical_depth, c.scale_m]
    }

    /// The march uniform's `cirrus_fiber` row (ADR-1509): fiber strength, fiber scale (m),
    /// shear amount, hook amount.
    #[must_use]
    pub fn cirrus_fiber_row(&self) -> [f32; 4] {
        let c = self.cirrus.sanitized();
        [c.fiber, c.fiber_scale_m, c.shear, c.hook]
    }

    /// The march uniform's `cirrus_shear` row (ADR-1509): cosine and sine of the shear
    /// direction from the wind, breakup, and 1 when the sheet is fibrous
    /// ([`CirrusSheet::is_fibrous`]; 0 draws the ADR-1470 trails, pixel for pixel).
    #[must_use]
    pub fn cirrus_shear_row(&self) -> [f32; 4] {
        let c = self.cirrus.sanitized();
        let (sin, cos) = c.shear_deg.to_radians().sin_cos();
        [cos, sin, c.breakup, if c.is_fibrous() { 1.0 } else { 0.0 }]
    }

    /// The march uniform's `cirrus_crisp` row (ADR-1516): the crisp amount and the sheet
    /// samples per march texel ([`CIRRUS_CRISP_SAMPLES`] when crisp, else 1: one centred sample,
    /// pixel for pixel the ADR-1509 sheet), then two zeros.
    #[must_use]
    pub fn cirrus_crisp_row(&self) -> [f32; 4] {
        let c = self.cirrus.sanitized();
        let samples = if c.crisp > 0.0 {
            CIRRUS_CRISP_SAMPLES as f32
        } else {
            1.0
        };
        [c.crisp, samples, 0.0, 0.0]
    }
}

impl Default for CloudLook {
    fn default() -> Self {
        Self::LEGACY
    }
}

/// The march uniform's `cirrus_dir` row: the unit streak direction on world X and Z (the wind's,
/// +X when there is no wind) and the sheet's drift (metres). Winds at cirrus height run about
/// 2.5 times the low-level wind in mid latitudes, so the sheet drifts that much faster.
#[must_use]
pub fn cirrus_direction_row(settings: &CloudSettings) -> [f32; 4] {
    let [wx, wz] = settings.wind_m_per_s;
    let len = (wx * wx + wz * wz).sqrt();
    let dir = if len.is_finite() && len > 1.0e-3 {
        [wx / len, wz / len]
    } else {
        [1.0, 0.0]
    };
    let drift = settings.drift_m();
    [
        dir[0],
        dir[1],
        drift[0] * CIRRUS_WIND_RATIO,
        drift[1] * CIRRUS_WIND_RATIO,
    ]
}

/// How much faster the cirrus sheet drifts than the cloud layer (see [`cirrus_direction_row`]).
pub const CIRRUS_WIND_RATIO: f32 = 2.5;

/// Rayleigh scale height of the Earth's air (metres; U.S. Standard Atmosphere 1976). In a
/// stratified atmosphere the fraction of the air along any upward ray that lies above height
/// `h` is `exp(−h / H)` whatever the ray's elevation, so a cirrus sheet at 9 km hides only about
/// a third of the sky's in-scatter (ADR-1425). Mirrors `CIRRUS_AIR_SCALE_HEIGHT_M` in
/// `clouds.wgsl`.
pub const CIRRUS_AIR_SCALE_HEIGHT_M: f32 = 8_000.0;

/// The cirrus ice phase function (per steradian, ADR-1425): two Henyey–Greenstein terms
/// (Kattawar 1975), a sharp forward lobe for the diffraction peak (g 0.9, weight 0.7) and a broad
/// one for refracted and side-scattered light (g 0.4). Mean asymmetry 0.75, as measured for
/// cirrus ice ([`CIRRUS_ICE_ASYMMETRY`]). Mirrors `cirrus_phase` in `clouds.wgsl`.
#[must_use]
pub fn cirrus_phase(cos_t: f32) -> f32 {
    0.7 * crate::henyey_greenstein(cos_t, 0.9) + 0.3 * crate::henyey_greenstein(cos_t, 0.4)
}

/// A thin ridge where a `0..1` noise crosses its middle: 1 on the crossing, 0 a quarter of the
/// range away, cubed so the filament is thin. The cirrus sheet drew its filaments as this of a
/// noise stretched along the wind until ADR-1470 replaced them with fallstreak trails
/// ([`cirrus_density`]); kept for hosts that call it.
#[must_use]
pub fn cirrus_ridge(n: f32) -> f32 {
    let r = 1.0 - ((n - 0.5).abs() * 4.0).clamp(0.0, 1.0);
    r * r * r
}

/// Length of one cirrus generating cell along the wind, in streak tiles per unit of
/// [`CirrusSheet::stretch`] (ADR-1470). A cell is one tile across. Mirrors `CIRRUS_CELL_ALONG`.
pub const CIRRUS_CELL_ALONG: f32 = 1.0;
/// Fraction of cirrus generating cells that hold a head (ADR-1470). Mirrors `CIRRUS_CELL_ACTIVE`.
pub const CIRRUS_CELL_ACTIVE: f32 = 0.92;
/// Normalised density of the faint ice veil between the trails, inside a band (ADR-1470).
/// Mirrors `CIRRUS_VEIL`.
pub const CIRRUS_VEIL: f32 = 0.07;
/// Area mean of the fallstreak trails (fibres included) where the band is full: the value a
/// pixel footprint much wider than a trail fades to (ADR-1470). Mirrors `CIRRUS_TRAIL_MEAN`.
pub const CIRRUS_TRAIL_MEAN: f32 = 0.078;
/// Peak density of the strongest cirrus trail heads; overlaps clip at 1 (ADR-1470). Mirrors
/// `CIRRUS_TRAIL_GAIN`.
pub const CIRRUS_TRAIL_GAIN: f32 = 1.6;
/// Depth of the fibre modulation inside a trail, `0..1` (ADR-1470). Mirrors
/// `CIRRUS_FIBRE_CONTRAST`.
pub const CIRRUS_FIBRE_CONTRAST: f32 = 0.85;
/// Gain that keeps the fibre modulation's area mean near 1 (ADR-1470). Mirrors
/// `CIRRUS_FIBRE_GAIN`.
pub const CIRRUS_FIBRE_GAIN: f32 = 2.4;
/// Fibre-noise cells per streak tile across a cirrus trail, one bright fibre per one to two
/// cells (ADR-1470). Mirrors `CIRRUS_FIBRE_CELLS`.
pub const CIRRUS_FIBRE_CELLS: f32 = 14.0;
/// A cirrus trail's cross profile is widened to at least this many pixel footprints, its
/// amplitude lowered to keep its area, so it never narrows below the texel (ADR-1470). Mirrors
/// `CIRRUS_TRAIL_FILTER`.
pub const CIRRUS_TRAIL_FILTER: f32 = 4.0;

/// pcg2d (Jarzynski & Olano, "Hash Functions for GPU Rendering", JCGT 9(3), 2020) of an integer
/// cell: eight bytes of hash. Mirrors `cirrus_pcg2d` in `clouds.wgsl`.
fn cirrus_pcg2d(cx: i32, cy: i32) -> [u32; 2] {
    const M: u32 = 1_664_525;
    let mut x = (cx as u32).wrapping_mul(M).wrapping_add(1_013_904_223);
    let mut y = (cy as u32).wrapping_mul(M).wrapping_add(1_013_904_223);
    x = x.wrapping_add(y.wrapping_mul(M));
    y = y.wrapping_add(x.wrapping_mul(M));
    x ^= x >> 16;
    y ^= y >> 16;
    x = x.wrapping_add(y.wrapping_mul(M));
    y = y.wrapping_add(x.wrapping_mul(M));
    x ^= x >> 16;
    y ^= y >> 16;
    [x, y]
}

/// Byte `k` (0..3) of `h` as `0..1`. Mirrors `cirrus_byte`.
fn cirrus_byte(h: u32, k: u32) -> f32 {
    ((h >> (8 * k)) & 255) as f32 / 255.0
}

/// The PCG output hash of one integer (Jarzynski & Olano 2020), `0..1` in 24 bits. Mirrors
/// `cirrus_hash1`.
fn cirrus_hash1(n: i32) -> f32 {
    let mut h = (n as u32).wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    h = ((h >> ((h >> 28) + 4)) ^ h).wrapping_mul(277_803_737);
    h ^= h >> 22;
    (h >> 8) as f32 / 16_777_216.0
}

/// 1D gradient noise with the quintic fade (Perlin, "Improving Noise", SIGGRAPH 2002), about
/// `−0.5..0.5`. Mirrors `cirrus_noise1`.
fn cirrus_noise1(x: f32) -> f32 {
    let i = x.floor();
    let f = x - i;
    let n = i as i32;
    let g0 = cirrus_hash1(n) * 2.0 - 1.0;
    let g1 = cirrus_hash1(n.wrapping_add(1)) * 2.0 - 1.0;
    let w = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let a = g0 * f;
    a + (g1 * (f - 1.0) - a) * w
}

/// WGSL `smoothstep` for `e0 < e1`.
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Normalised ice density (`0..1`) of the cirrus sheet at `x` streak tiles along the wind and `y`
/// across it (after the shader's curl warp), inside a band of strength `band` (`0..1`), for a
/// sheet of streak `stretch` and a pixel footprint of `footprint` tiles across the trails
/// (ADR-1470). Mirrors `cirrus_density` in `clouds.wgsl`, which feeds it the band and the warp
/// from its noise textures.
///
/// Cirrus uncinus and fibratus are fallstreaks: ice crystals grow in small generating cells and
/// fall out of them, and the wind shear lays each trail out downwind as a hook-headed streak
/// that widens and thins as crystals of different sizes fall at different speeds (Marshall
/// 1953; Heymsfield 1975). Generating cells sit at random points of a staggered grid; each
/// draws a soft head and a tapering trail of random length, width, hook and lateral drift, and
/// overlapping trails add. Soft fibres run along the strongest trail. A footprint wider than a
/// fibre fades the fibres to their mean, and one wider than a trail fades the field to
/// [`CIRRUS_VEIL`] + [`CIRRUS_TRAIL_MEAN`], so the sheet does not alias into hair.
#[must_use]
pub fn cirrus_density(x: f32, y: f32, band: f32, stretch: f32, footprint: f32) -> f32 {
    let finite = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
    let (x, y) = (finite(x, 0.0), finite(y, 0.0));
    let b = finite(band, 0.0).clamp(0.0, 1.0);
    if b <= 0.0 {
        return 0.0;
    }
    let footprint = finite(footprint, 0.0).max(0.0);
    let far = b * (CIRRUS_VEIL + CIRRUS_TRAIL_MEAN);
    let blur = smoothstep(0.2, 0.5, footprint);
    if blur >= 1.0 {
        return far.min(1.0);
    }
    let cx = CIRRUS_CELL_ALONG * finite(stretch, 1.0).clamp(1.0, 32.0);
    let mut total = 0.0f32;
    let mut best = 0.0f32;
    let (mut best_n, mut best_u, mut best_seed) = (0.0f32, 0.0f32, 0.0f32);
    let iy = y.floor() as i32;
    for dj in -1..=1 {
        let j = iy.wrapping_add(dj);
        // Rows staggered by the golden ratio so heads never line up across the wind.
        let stagger = j as f32 * 0.618_034;
        let xr = x + (stagger - stagger.floor()) * cx;
        let ix = (xr / cx).floor() as i32;
        for k in 0..2 {
            let c = ix.wrapping_sub(k);
            let [hx_bits, hy_bits] = cirrus_pcg2d(c, j);
            if cirrus_byte(hx_bits, 0) >= CIRRUS_CELL_ACTIVE {
                continue;
            }
            let w0 = 0.12 + 0.28 * cirrus_byte(hy_bits, 0);
            let hx = (c as f32 + 0.1 + 0.8 * cirrus_byte(hx_bits, 1)) * cx;
            let hy = j as f32 + 0.25 + 0.5 * cirrus_byte(hx_bits, 2);
            let len = cx * (0.4 + 0.55 * cirrus_byte(hx_bits, 3));
            let drift = (cirrus_byte(hy_bits, 1) - 0.5) * 0.5;
            let hook = (cirrus_byte(hy_bits, 2) - 0.5) * 1.2;
            let seed = cirrus_byte(hy_bits, 3);
            let s = xr - hx;
            let u_raw = s / len;
            let front = (0.8 * w0).min(0.1 * cx);
            if s <= -front || u_raw >= 1.0 {
                continue;
            }
            let us = u_raw.clamp(0.0, 1.0);
            let head = smoothstep(-front, front, s);
            let tail = (1.0 - smoothstep(0.0, 1.0, u_raw)) * (1.0 - 0.5 * us);
            let n = y - hy - drift * us - hook * w0 * us * us;
            let w = w0 * (0.6 + 1.2 * us);
            // The profile convolved with the footprint keeps its area: wider and fainter.
            let fp = CIRRUS_TRAIL_FILTER * footprint;
            // Capped so a trail never reaches past the neighbouring rows the loop visits.
            let we = (w * w + fp * fp).sqrt().min(w.max(0.72));
            let a = n / we;
            let across = (1.0 - a * a).clamp(0.0, 1.0);
            let d = (0.45 + 0.55 * seed) * CIRRUS_TRAIL_GAIN * head * tail * across * across * (w / we);
            total += d;
            if d > best {
                best = d;
                best_n = n;
                best_u = us;
                best_seed = seed;
            }
        }
    }
    let mut fibre = 1.0f32;
    // Where two trails overlap evenly the fibres fade, so the switch of frame is unseen.
    let dom = (2.0 * best / total.max(1e-6) - 1.0).clamp(0.0, 1.0);
    // Each fibre octave fades before a texel covers a third of one of its bright fibres.
    let cells = footprint * CIRRUS_FIBRE_CELLS;
    let pre = 1.0 - smoothstep(0.12, 0.3, cells);
    if best > 0.0 && dom * pre > 0.0 {
        let fine = 1.0 - smoothstep(0.05, 0.13, cells);
        let fc = best_n * CIRRUS_FIBRE_CELLS + best_seed * 37.0 + best_u * 0.8;
        let f = 0.65 * cirrus_noise1(fc) + 0.35 * fine * cirrus_noise1(fc * 2.3 + 11.0);
        let fib = smoothstep(-0.4, 0.4, f);
        let brk = smoothstep(-0.3, 0.25, cirrus_noise1(best_u * 3.0 + best_seed * 53.0));
        let contrast = CIRRUS_FIBRE_CONTRAST * dom * pre;
        fibre = 1.0 + (fib * (0.5 + 0.5 * brk) * CIRRUS_FIBRE_GAIN - 1.0) * contrast;
    }
    let near = b * (CIRRUS_VEIL + (total * fibre).min(1.0));
    (near + (far - near) * blur).clamp(0.0, 1.0)
}

/// Fall depth of the trails' tails below the sheet at full shear (metres, ADR-1509). Ice
/// crystals falling out of cirrus generating cells form trails 1–3 km deep before they
/// sublimate (Heymsfield 1975; Sassen 1991). Mirrors `CIRRUS_FALL_DEPTH_M`.
pub const CIRRUS_FALL_DEPTH_M: f32 = 2_500.0;
/// Largest apparent slant of a fibrous trail on the sheet, in streak tiles per unit of fall
/// (ADR-1509): near the horizon the parallax of a hanging tail grows without bound, so it is
/// clamped here (the footprint blur takes over there). Mirrors `CIRRUS_SLANT_MAX`.
pub const CIRRUS_SLANT_MAX: f32 = 3.0;
/// Share of the sheet's broad curl warp (the bend `cirrus_sheet` in `clouds.wgsl` applies before
/// the density) that a fibrous sheet keeps, so its streaks run along the shear instead of curling
/// into S-strokes (ADR-1509). Mirrors `CIRRUS_FIBROUS_WARP`.
pub const CIRRUS_FIBROUS_WARP: f32 = 0.4;
/// A fibrous trail's cross profile is widened to at least this many pixel footprints (ADR-1509;
/// the ADR-1470 trails use [`CIRRUS_TRAIL_FILTER`], whose wider profile reads as airbrushed).
/// Mirrors `CIRRUS_FIBROUS_FILTER`.
pub const CIRRUS_FIBROUS_FILTER: f32 = 2.0;
/// Extra density of an uncinus head tuft at full hook (ADR-1509). Mirrors `CIRRUS_HOOK_TUFT`.
pub const CIRRUS_HOOK_TUFT: f32 = 1.2;
/// Sharpness of the strands combed along a fibrous trail: the strand is `1 − |noise| · this`,
/// squared (ADR-1509). Mirrors `CIRRUS_STRAND_RIDGE`.
pub const CIRRUS_STRAND_RIDGE: f32 = 3.2;
/// Density left between the strands of a fibrous trail (ADR-1509). Mirrors `CIRRUS_STRAND_BASE`.
pub const CIRRUS_STRAND_BASE: f32 = 0.15;
/// Area mean of the strand modulation before normalising, so a trail keeps its mean density
/// whatever the fiber strength (ADR-1509). Mirrors `CIRRUS_STRAND_MEAN`.
pub const CIRRUS_STRAND_MEAN: f32 = 0.415;
/// Breaks per trail length along each strand (ADR-1509). Mirrors `CIRRUS_STRAND_BREAK`.
pub const CIRRUS_STRAND_BREAK: f32 = 4.0;
/// Peak share of the faint fiber patches between the trails at full fiber strength
/// (ADR-1509). Mirrors `CIRRUS_SHEET_FIBERS`.
pub const CIRRUS_SHEET_FIBERS: f32 = 0.12;
/// Area mean of the envelope of those patches (ADR-1509). Mirrors `CIRRUS_FIBER_PATCH_MEAN`.
pub const CIRRUS_FIBER_PATCH_MEAN: f32 = 0.18;
/// How many times longer than wide a sheet fiber is (ADR-1509). Mirrors `CIRRUS_FIBER_ANISO`.
pub const CIRRUS_FIBER_ANISO: f32 = 18.0;
/// Area mean of one sheet fiber octave, for the normalisation (ADR-1509). Mirrors
/// `CIRRUS_FIBER_OCTAVE_MEAN`.
pub const CIRRUS_FIBER_OCTAVE_MEAN: f32 = 0.157;

/// Cirrus sheet samples per march texel for a crisp sheet (ADR-1516): the sheet is the mean of
/// this many samples through the texel's R2 points, each prefiltered over `1 / √samples` of the
/// texel, so strands resolve about half a texel wide without aliasing.
pub const CIRRUS_CRISP_SAMPLES: u32 = 4;
/// A crisp trail's cross profile is widened to at least this many sample footprints (ADR-1516;
/// [`CIRRUS_FIBROUS_FILTER`] otherwise). Mirrors `CIRRUS_CRISP_FILTER`.
pub const CIRRUS_CRISP_FILTER: f32 = 1.0;
/// Sharpest strand ridge at full crisp (`1 − |noise| · this`, squared; ADR-1516; the ADR-1509
/// strands use [`CIRRUS_STRAND_RIDGE`]). Mirrors `CIRRUS_CRISP_RIDGE`.
pub const CIRRUS_CRISP_RIDGE: f32 = 7.0;
/// A crisp ridge is never sharper than this many strand spacings over the footprint, so a
/// strand never narrows below about one sample (ADR-1516). Mirrors `CIRRUS_CRISP_RIDGE_WIDTH`.
pub const CIRRUS_CRISP_RIDGE_WIDTH: f32 = 0.45;
/// Density left between the strands at full crisp (ADR-1516; [`CIRRUS_STRAND_BASE`]
/// otherwise). Mirrors `CIRRUS_CRISP_BASE`.
pub const CIRRUS_CRISP_BASE: f32 = 0.04;
/// Area mean of the strand modulation for a ridge sharpness `k`: `base + A / (k + B)`, fitted
/// to the strand noise for `k` 2..15 within 3 % (ADR-1516). Mirrors `CIRRUS_CRISP_MEAN_A`.
pub const CIRRUS_CRISP_MEAN_A: f32 = 1.48;
/// The `B` of [`CIRRUS_CRISP_MEAN_A`]. Mirrors `CIRRUS_CRISP_MEAN_B`.
pub const CIRRUS_CRISP_MEAN_B: f32 = 2.3;
/// Share of the veil between the trails a fully crisp sheet keeps (ADR-1516). Mirrors
/// `CIRRUS_CRISP_VEIL`.
pub const CIRRUS_CRISP_VEIL: f32 = 0.4;
/// Largest normalised density of a fully crisp sheet (ADR-1516; 1 otherwise): the strand peaks
/// are not clipped, so they keep the optical depth taken from the gaps. Mirrors
/// `CIRRUS_CRISP_CAP`.
pub const CIRRUS_CRISP_CAP: f32 = 3.0;

/// One axis of a cubic B-spline texture lookup as two linear taps (Sigg & Hadwiger, "Fast
/// Third-Order Texture Filtering", GPU Gems 2 ch. 20, 2005; ADR-1516): for texture coordinate
/// `uv` on a `size`-texel axis, the coordinates of the two taps and the weight of the first.
/// The B-spline is C2, so a warp read through it bends smoothly where a bilinear read kinks at
/// every texel edge. Mirrors `cirrus_bspline_axis` in `clouds.wgsl`.
#[must_use]
pub fn cirrus_bspline_axis(uv: f32, size: f32) -> [f32; 3] {
    let size = if size.is_finite() { size.max(1.0) } else { 1.0 };
    let x = if uv.is_finite() { uv * size - 0.5 } else { -0.5 };
    let i = x.floor();
    let f = x - i;
    let (f2, f3) = (f * f, f * f * f);
    let w0 = (1.0 - 3.0 * f + 3.0 * f2 - f3) / 6.0;
    let w1 = (4.0 - 6.0 * f2 + 3.0 * f3) / 6.0;
    let w2 = (1.0 + 3.0 * f + 3.0 * f2 - 3.0 * f3) / 6.0;
    let w3 = f3 / 6.0;
    let g0 = w0 + w1;
    [(i - 0.5 + w1 / g0) / size, (i + 1.5 + w3 / (w2 + w3)) / size, g0]
}

/// 2D gradient noise with the quintic fade (Perlin 2002) on the pcg2d hash, about `−0.7..0.7`.
/// Mirrors `cirrus_noise2`.
fn cirrus_noise2(x: f32, y: f32) -> f32 {
    let (fx0, fy0) = (x.floor(), y.floor());
    let (fx, fy) = (x - fx0, y - fy0);
    let (ix, iy) = (fx0 as i32, fy0 as i32);
    let corner = |a: i32, b: i32| {
        let [h, _] = cirrus_pcg2d(ix.wrapping_add(a), iy.wrapping_add(b));
        (cirrus_byte(h, 0) * 2.0 - 1.0) * (fx - a as f32) + (cirrus_byte(h, 1) * 2.0 - 1.0) * (fy - b as f32)
    };
    let fade = |t: f32| t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
    let (wx, wy) = (fade(fx), fade(fy));
    let (c00, c10, c01, c11) = (corner(0, 0), corner(1, 0), corner(0, 1), corner(1, 1));
    let a = c00 + (c10 - c00) * wx;
    let b = c01 + (c11 - c01) * wx;
    a + (b - a) * wy
}

/// One octave of the sheet fibers: thin ridges where an anisotropic gradient noise crosses
/// zero, broken along their length. Mirrors `cirrus_fiber_octave`.
fn cirrus_fiber_octave(a: f32, b: f32, octave: f32, freq: f32) -> f32 {
    let g = cirrus_noise2(
        a * freq / CIRRUS_FIBER_ANISO + 17.3 * octave,
        b * freq + 5.1 * octave,
    );
    let ridge = 1.0 - (g.abs() * 2.5).min(1.0);
    let gate = smoothstep(
        -0.12,
        0.22,
        cirrus_noise2(
            a * freq / (CIRRUS_FIBER_ANISO * 2.5) + 3.7 * octave + 40.0,
            b * freq * 0.3 + 9.2 * octave,
        ),
    );
    ridge * ridge * ridge * gate
}

/// The sheet fibers at `a` fiber units along the shear and `b` across it, three octaves, area
/// mean 1; each octave fades to its mean before a footprint of `footprint` fiber units covers a
/// third of it. Mirrors `cirrus_sheet_fibers`.
fn cirrus_sheet_fibers(a: f32, b: f32, footprint: f32) -> f32 {
    let m = CIRRUS_FIBER_OCTAVE_MEAN;
    let (mut total, mut norm) = (0.0f32, 0.0f32);
    for (octave, (freq, amp)) in [(1.0f32, 1.0f32), (2.03, 0.8), (4.1, 0.6)]
        .into_iter()
        .enumerate()
    {
        let keep = 1.0 - smoothstep(0.12, 0.35, footprint * freq);
        let v = if keep > 0.0 {
            cirrus_fiber_octave(a, b, octave as f32, freq)
        } else {
            m
        };
        total += amp * (m + (v - m) * keep);
        norm += amp * m;
    }
    total / norm
}

/// Normalised ice density of a fibrous cirrus sheet (ADR-1509) at `x` streak tiles
/// along the wind and `y` across it (after the shader's curl warp), inside a band of strength
/// `band`, for a pixel footprint of `footprint` tiles across the shear. The return is `0..1`
/// for the ADR-1509 sheet; with `crisp` above 0 the strand peaks reach
/// `1 + (CIRRUS_CRISP_CAP − 1) · crisp` (ADR-1516). `slant` is the view
/// ray's horizontal direction over its vertical part at the sheet, in the wind frame (along,
/// across): how far a point one tile below the sheet appears displaced on it. Mirrors
/// `cirrus_density_fibrous` in `clouds.wgsl`; the sheet uses it when
/// [`CirrusSheet::is_fibrous`], and [`cirrus_density`] otherwise.
///
/// Fallstreaks (Marshall 1953; Heymsfield 1975): crystals grow in generating cells and fall
/// through layers whose wind differs from the cell's. With a wind that grows linearly with
/// depth a falling crystal's horizontal offset from its head grows as the square of the depth;
/// a wind that also veers with depth adds a cubic sideways bend; and the tail lies below the
/// sheet, so seen at a slant it is displaced on the sheet by `slant` times its depth. Each
/// trail is that curve: `L (h₀ u + (1 − h₀) u²)` along the shear plus `slant · D · u`, with the
/// veer bend across, where `u` is the fall fraction, `L` the length (longer with more shear),
/// `D` the fall depth ([`CIRRUS_FALL_DEPTH_M`] × shear) and `h₀` going from straight (no hook)
/// to quadratic (the uncinus comma: the tail leaves the head along the slant and turns
/// downwind). The head is a dense tuft. Strands combed along each trail spread as it widens
/// (crystals of different sizes fall at different speeds), each broken along its length, and
/// fade to their mean where a footprint covers them. Faint patches of sheet fibers lie along
/// the shear between the trails, and a patch noise breaks the band into separate patches.
#[must_use]
pub fn cirrus_density_fibrous(
    x: f32,
    y: f32,
    band: f32,
    footprint: f32,
    slant: [f32; 2],
    sheet: &CirrusSheet,
) -> f32 {
    let finite = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
    let (x, y) = (finite(x, 0.0), finite(y, 0.0));
    let band = finite(band, 0.0).clamp(0.0, 1.0);
    if band <= 0.0 {
        return 0.0;
    }
    let footprint = finite(footprint, 0.0).max(0.0);
    let sheet = sheet.sanitized();
    let (sin, cos) = sheet.shear_deg.to_radians().sin_cos();
    let (fiber, shear, hook, breakup) = (sheet.fiber, sheet.shear, sheet.hook, sheet.breakup);
    // The shear frame: `s` along the shear, `n` across it.
    let s = x * cos + y * sin;
    let n = -x * sin + y * cos;
    let (sx, sy) = (finite(slant[0], 0.0), finite(slant[1], 0.0));
    let ps = sx * cos + sy * sin;
    let pn = -sx * sin + sy * cos;
    let cx = CIRRUS_CELL_ALONG * sheet.stretch;
    // Patches: the band broken by a noise stretched along the shear.
    let pz = cirrus_noise2(s / (cx * 0.45) + 71.0, n / 0.7 + 13.0)
        + 0.5 * cirrus_noise2(s / (cx * 0.2) + 5.0, n / 0.33 + 29.0);
    let thr = breakup * 0.3 - 0.3;
    let b = band * (1.0 + (smoothstep(thr, thr + 0.25, pz) - 1.0) * breakup);
    if b <= 0.0 {
        return 0.0;
    }
    // Crisp (ADR-1516): less veil between the trails, and strand peaks up to `cap`.
    let (thin, cap) = if sheet.crisp > 0.0 {
        (
            1.0 + (CIRRUS_CRISP_VEIL - 1.0) * sheet.crisp,
            1.0 + (CIRRUS_CRISP_CAP - 1.0) * sheet.crisp,
        )
    } else {
        (1.0, 1.0)
    };
    let veil_far =
        (CIRRUS_VEIL * (1.0 - fiber) + fiber * CIRRUS_SHEET_FIBERS * CIRRUS_FIBER_PATCH_MEAN) * thin;
    let trail_far = CIRRUS_TRAIL_MEAN * (1.0 + 1.3 * shear) * (1.0 + 0.1 * hook) * (1.0 - 0.2 * fiber);
    let far = b * (veil_far + trail_far);
    let blur = smoothstep(0.2, 0.5, footprint);
    if blur >= 1.0 {
        return far.min(cap);
    }
    let fs = sheet.fiber_scale_m / sheet.scale_m;
    // The tail's apparent slant on the sheet per unit of fall fraction, clamped.
    let fall = shear * CIRRUS_FALL_DEPTH_M / sheet.scale_m;
    let (mut pvs, mut pvn) = (ps * fall, pn * fall);
    let mag = (pvs * pvs + pvn * pvn).sqrt();
    if mag > CIRRUS_SLANT_MAX {
        let k = CIRRUS_SLANT_MAX / mag;
        pvs *= k;
        pvn *= k;
    }
    let h0 = 1.0 - 0.75 * hook;
    let lmax = cx * 0.95 * (1.0 + shear);
    // Rows and cells whose trail can reach this point (heads sit 0.25..0.75 into a row and
    // 0.1..0.9 into a cell; a trail's cross offset spans `min(pvn, 0) − 0.49` to
    // `max(pvn, 0) + 1.19`, its half width at most 1.05).
    let j0 = (n - pvn.max(0.0) - 3.0).floor() as i32;
    let j1 = ((n - pvn.min(0.0) + 1.3).floor() as i32).min(j0.saturating_add(9));
    let mut total = 0.0f32;
    for j in j0..=j1 {
        // Rows staggered by the golden ratio so heads never line up across the shear.
        let stagger = j as f32 * 0.618_034;
        let sr = s + (stagger - stagger.floor()) * cx;
        let c0 = ((sr - lmax - pvs.max(0.0) - 1.05) / cx - 0.9).floor() as i32;
        let c1 = (((sr - pvs.min(0.0) + 1.05) / cx).floor() as i32).min(c0.saturating_add(8));
        for c in c0..=c1 {
            total += cirrus_fibrous_trail([c, j], [sr, n], [pvs, pvn], h0, footprint, fs, &sheet);
        }
    }
    let mut veil = CIRRUS_VEIL * (1.0 - fiber);
    if fiber > 0.0 {
        let env = smoothstep(0.0, 0.35, cirrus_noise2(s / (cx * 0.3) + 21.0, n / 0.35 + 3.0));
        veil += fiber * CIRRUS_SHEET_FIBERS * env * cirrus_sheet_fibers(s / fs, n / fs, footprint / fs);
    }
    let near = b * (veil * thin + total.min(cap));
    (near + (far - near) * blur).clamp(0.0, cap)
}

/// One fibrous trail's density at `p` (shear frame, staggered: `[sr, n]`) from the generating
/// cell `[c, j]`, with the apparent slant `[pvs, pvn]` (tiles per unit of fall fraction). Part
/// of [`cirrus_density_fibrous`]; mirrors `cirrus_fibrous_trail`.
fn cirrus_fibrous_trail(
    cell: [i32; 2],
    p: [f32; 2],
    slant: [f32; 2],
    h0: f32,
    footprint: f32,
    fs: f32,
    sheet: &CirrusSheet,
) -> f32 {
    let [c, j] = cell;
    let [hx_bits, hy_bits] = cirrus_pcg2d(c, j);
    if cirrus_byte(hx_bits, 0) >= CIRRUS_CELL_ACTIVE {
        return 0.0;
    }
    let (shear, hook, fiber) = (sheet.shear, sheet.hook, sheet.fiber);
    let [pvs, pvn] = slant;
    let cx = CIRRUS_CELL_ALONG * sheet.stretch;
    let w0 = 0.12 + 0.28 * cirrus_byte(hy_bits, 0);
    let hs = (c as f32 + 0.1 + 0.8 * cirrus_byte(hx_bits, 1)) * cx;
    let hn = j as f32 + 0.25 + 0.5 * cirrus_byte(hx_bits, 2);
    let len = cx * (0.4 + 0.55 * cirrus_byte(hx_bits, 3)) * (1.0 + shear);
    let drift = (cirrus_byte(hy_bits, 1) - 0.5) * 0.5;
    let bend = (cirrus_byte(hy_bits, 2) - 0.5) * 1.2 * (1.0 - hook) * w0;
    let veer = hook * (0.35 + 0.35 * cirrus_byte(hy_bits, 2));
    let seed = cirrus_byte(hy_bits, 3);
    // The chord from the head to the tail's end.
    let a_s = len + pvs;
    let a_n = pvn + drift + veer + bend;
    let a2 = (a_s * a_s + a_n * a_n).max(1e-6);
    let alen = a2.sqrt();
    let (rs, rn) = (p[0] - hs, p[1] - hn);
    let along = (rs * a_s + rn * a_n) / alen;
    let t0 = along / alen;
    let front = (0.8 * w0).min(0.1 * cx);
    if along <= -front || t0 >= 1.0 {
        return 0.0;
    }
    // The trail's point at fall fraction `u` and its derivative.
    let curve = |u: f32| {
        let os = len * (h0 * u + (1.0 - h0) * u * u) + pvs * u;
        let on = (pvn + drift) * u + veer * u * u * u + bend * u * u;
        let ds = len * (h0 + 2.0 * (1.0 - h0) * u) + pvs;
        let dn = pvn + drift + 3.0 * veer * u * u + 2.0 * bend * u;
        (os, on, ds, dn)
    };
    // The fall fraction whose point projects onto the chord where `p` does (two Newton steps).
    let mut u = t0.clamp(0.0, 1.0);
    for _ in 0..2 {
        let (os, on, ds, dn) = curve(u);
        let cu = (os * a_s + on * a_n) / a2;
        let dc = ((ds * a_s + dn * a_n) / a2).max(0.05);
        u = (u - (cu - t0) / dc).clamp(0.0, 1.0);
    }
    let (os, on, _, _) = curve(u);
    let dist = ((rs - os) * (-a_n) + (rn - on) * a_s) / alen;
    let head = smoothstep(-front, front, along);
    let tail = (1.0 - smoothstep(0.0, 1.0, t0)) * (1.0 - 0.5 * u);
    let near_head = (-(u / 0.1) * (u / 0.1)).exp();
    let tuft = 1.0 + hook * CIRRUS_HOOK_TUFT * near_head;
    let w = w0 * (0.6 + (1.2 + 0.8 * shear) * u) * (1.0 - 0.35 * hook * near_head);
    // The profile convolved with the footprint keeps its area: wider and fainter.
    let crisp = sheet.crisp;
    let filter = if crisp > 0.0 {
        CIRRUS_FIBROUS_FILTER + (CIRRUS_CRISP_FILTER - CIRRUS_FIBROUS_FILTER) * crisp
    } else {
        CIRRUS_FIBROUS_FILTER
    };
    let fp = filter * footprint;
    let we = (w * w + fp * fp).sqrt().min(w.max(0.72));
    let a = dist / we;
    let across = (1.0 - a * a).clamp(0.0, 1.0);
    // Strands combed along the trail, spreading as it widens; each fades to its mean before a
    // footprint covers a sixth of its spacing.
    let q = dist / w * (w0 / fs);
    let spacing = fs * w / w0;
    let pre = 1.0 - smoothstep(0.15, 0.4, footprint / spacing);
    let mut strands = 1.0f32;
    if fiber * pre > 0.0 {
        let fine = 1.0 - smoothstep(0.15, 0.4, 2.3 * footprint / spacing);
        let f = 0.65 * cirrus_noise1(q + seed * 37.0 + u * 0.7)
            + 0.35 * fine * cirrus_noise1(q * 2.3 + 11.0 + seed * 13.0);
        // Crisp (ADR-1516): the ridge as sharp as the footprint allows, up to
        // `CIRRUS_CRISP_RIDGE`, over less ice between the strands; the mean follows.
        let (k, base, mean) = if crisp > 0.0 {
            let kmax = CIRRUS_STRAND_RIDGE + (CIRRUS_CRISP_RIDGE - CIRRUS_STRAND_RIDGE) * crisp;
            let k =
                (CIRRUS_CRISP_RIDGE_WIDTH * spacing / footprint.max(1e-9)).clamp(CIRRUS_STRAND_RIDGE, kmax);
            let base = CIRRUS_STRAND_BASE + (CIRRUS_CRISP_BASE - CIRRUS_STRAND_BASE) * crisp;
            (k, base, base + CIRRUS_CRISP_MEAN_A / (k + CIRRUS_CRISP_MEAN_B))
        } else {
            (CIRRUS_STRAND_RIDGE, CIRRUS_STRAND_BASE, CIRRUS_STRAND_MEAN)
        };
        let r = (1.0 - f.abs() * k).clamp(0.0, 1.0);
        // Breaks along the strands, different from strand to strand.
        let brk = smoothstep(
            -0.25,
            0.3,
            cirrus_noise2(u * CIRRUS_STRAND_BREAK + seed * 53.0, q),
        );
        let v = (base + r * r * brk) / mean;
        strands = 1.0 + (v - 1.0) * fiber * pre;
    }
    (0.45 + 0.55 * seed) * CIRRUS_TRAIL_GAIN * head * tail * tuft * across * across * (w / we) * strands
}

/// The light a cirrus sheet sees and the view through it, for [`cirrus_composite`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CirrusView {
    /// Sun at the sheet's height, colour × lux (the `cirrus_sun` row).
    pub sun: [f32; 3],
    /// Cosine of the angle between the view ray and the direction toward the sun.
    pub cos_sun: f32,
    /// Absolute cosine of the view ray against the local vertical at the sheet.
    pub mu: f32,
    /// Cosine-weighted sky radiance seen from the ground (the `ambient_top` row, cd/m²).
    pub sky_ambient: [f32; 3],
    /// Light coming up from below the sheet (the `ambient_bottom` row: half sky, half ground).
    pub ground_ambient: [f32; 3],
    /// Transmittance of the air between the eye and the sheet.
    pub air_transmittance: f32,
}

/// The cirrus sheet over a sky pixel `bg` as `bg · a + rgb`: returns `(rgb, a)` for a streak of
/// normalised density `density`. This is the shading of `cirrus_sheet` in `clouds.wgsl`
/// (ADR-1425) inside the aerial table's range: single scattering of the sun through
/// [`cirrus_phase`] plus the sky light above and below the sheet, and the sheet hides only the
/// air behind it (`exp(−h / H)` of the sky, [`CIRRUS_AIR_SCALE_HEIGHT_M`]). `density` is clamped
/// to `0..=1 + (CIRRUS_CRISP_CAP − 1) · crisp` (ADR-1516: a crisp sheet's strand peaks are
/// shaded unclipped), not to `0..1`.
#[must_use]
pub fn cirrus_composite(sheet: &CirrusSheet, density: f32, view: &CirrusView) -> ([f32; 3], f32) {
    let sheet = sheet.sanitized();
    let cap = 1.0 + (CIRRUS_CRISP_CAP - 1.0) * sheet.crisp;
    let d = if density.is_finite() {
        density.clamp(0.0, cap)
    } else {
        0.0
    };
    let mu = view.mu.abs().max(0.03);
    let trans = (-sheet.optical_depth * d / mu).exp();
    let behind = (-sheet.altitude_m / CIRRUS_AIR_SCALE_HEIGHT_M).exp();
    let phase = cirrus_phase(view.cos_sun.clamp(-1.0, 1.0));
    let air = view.air_transmittance.clamp(0.0, 1.0);
    let rgb = std::array::from_fn(|c| {
        let ambient = 0.5 * (behind * view.sky_ambient[c] + view.ground_ambient[c]);
        air * (1.0 - trans) * (view.sun[c] * phase + ambient)
    });
    (rgb, 1.0 - (1.0 - trans) * behind)
}

/// A kind of cloud (WMO International Cloud Atlas 2017 genera and species).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloudGenus {
    /// Cumulus humilis: small, flat, well separated fair-weather cumulus.
    FairCumulus,
    /// Cumulus mediocris to congestus: taller, denser, sharper-edged heaps.
    Cumulus,
    /// Broken stratocumulus (perlucidus / translucidus): a low sheet of rounded cells with gaps.
    Stratocumulus,
    /// Cirrus fibratus / uncinus: thin high ice streaks, with a few small cumulus below.
    Cirrus,
}

/// Coverage or cloud type in `0..=1`. A non-finite value is 0: `f32::clamp` leaves NaN unchanged.
fn finite_unit(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }
}

/// The layer a [`CloudGenus`] draws: what [`CloudGenusLayer::apply`] writes into
/// [`CloudSettings`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CloudGenusLayer {
    /// Coverage of the volumetric layer, `0..1`.
    pub coverage: f32,
    /// Cloud type (0 stratus, 0.5 cumulus, 1 towering).
    pub cloud_type: f32,
    /// Extinction at full density (1/m).
    pub density: f32,
    /// Cloud base above the ground (metres).
    pub altitude_bottom: f32,
    /// Cloud top (metres).
    pub altitude_top: f32,
    /// Base-shape noise tile (metres).
    pub shape_scale_m: f32,
    /// Detail noise tile (metres).
    pub detail_scale_m: f32,
    /// Lighting, erosion, breakup and cirrus.
    pub look: CloudLook,
}

impl CloudGenusLayer {
    /// Writes the layer into `settings`: coverage, type, density, heights, noise tiles and look.
    /// Wind, time, map, steps, seed and the shadow switches are kept.
    pub fn apply(&self, settings: &mut CloudSettings) {
        self.apply_shape(settings);
        // A non-finite weather value would be uploaded into the march uniform.
        settings.coverage = finite_unit(self.coverage);
        settings.cloud_type = finite_unit(self.cloud_type);
        settings.density = if self.density.is_finite() {
            self.density.max(0.0)
        } else {
            0.0
        };
    }

    /// Writes only the heights, noise tiles and look (the weather owns coverage, type and
    /// density).
    pub fn apply_shape(&self, settings: &mut CloudSettings) {
        let bottom = self.altitude_bottom.max(0.0);
        settings.altitude_bottom = bottom;
        settings.altitude_top = self.altitude_top.max(bottom + 1.0);
        settings.shape_scale_m = self.shape_scale_m.max(1.0);
        settings.detail_scale_m = self.detail_scale_m.max(1.0);
        settings.look = self.look.sanitized();
    }

    /// The shape part of `settings` (heights, tiles, look) as a layer, with its weather values.
    #[must_use]
    pub fn of(settings: &CloudSettings) -> Self {
        Self {
            coverage: settings.coverage,
            cloud_type: settings.cloud_type,
            density: settings.density,
            altitude_bottom: settings.altitude_bottom,
            altitude_top: settings.altitude_top,
            shape_scale_m: settings.shape_scale_m,
            detail_scale_m: settings.detail_scale_m,
            look: settings.look,
        }
    }

    /// `a` blended toward `b` by `t` (`0..1`), every field linearly.
    #[must_use]
    pub fn lerp(a: Self, b: Self, t: f32) -> Self {
        if !t.is_finite() {
            return a;
        }
        let t = t.clamp(0.0, 1.0);
        let mix = |x: f32, y: f32| x + (y - x) * t;
        Self {
            coverage: mix(a.coverage, b.coverage),
            cloud_type: mix(a.cloud_type, b.cloud_type),
            density: mix(a.density, b.density),
            altitude_bottom: mix(a.altitude_bottom, b.altitude_bottom),
            altitude_top: mix(a.altitude_top, b.altitude_top),
            shape_scale_m: mix(a.shape_scale_m, b.shape_scale_m),
            detail_scale_m: mix(a.detail_scale_m, b.detail_scale_m),
            look: CloudLook::lerp(a.look, b.look, t),
        }
    }
}

/// Physical diffusion strength the genus presets use (see [`CloudLook::diffusion`]).
pub const CLOUD_DIFFUSION_PHYSICAL: f32 = 2.0;

impl CloudGenus {
    /// Every genus, in the order of [`Self::NAMES`].
    pub const ALL: [Self; 4] = [
        Self::FairCumulus,
        Self::Cumulus,
        Self::Stratocumulus,
        Self::Cirrus,
    ];

    /// The names [`Self::from_name`] accepts (also weather preset names).
    pub const NAMES: [&'static str; 4] = ["fair_cumulus", "cumulus", "stratocumulus", "cirrus"];

    /// The genus's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::FairCumulus => "fair_cumulus",
            Self::Cumulus => "cumulus",
            Self::Stratocumulus => "stratocumulus",
            Self::Cirrus => "cirrus",
        }
    }

    /// The genus called `name`, or `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.name() == name)
    }

    /// The layer this genus draws.
    ///
    /// Extinction comes from the liquid water content and droplet radius,
    /// `β = 3 LWC / (2 ρ_w r_e)` (Stephens 1978; Miles, Verlinde & Clothiaux 2000 for the
    /// measured LWC and `r_e` of each genus). Heights are the WMO cloud atlas's temperate
    /// values for a cool spring day.
    #[must_use]
    pub fn layer(self) -> CloudGenusLayer {
        let look = CloudLook {
            diffusion: CLOUD_DIFFUSION_PHYSICAL,
            ..CloudLook::LEGACY
        };
        match self {
            // Humilis: base 1.0–1.5 km, a few hundred metres deep, LWC ≈ 0.2 g/m³, r_e ≈ 8 µm
            // → β ≈ 0.04 /m. Elements 0.3–1 km across, about a fifth of the sky.
            Self::FairCumulus => CloudGenusLayer {
                coverage: 0.22,
                cloud_type: 0.45,
                density: 0.04,
                altitude_bottom: 1_200.0,
                altitude_top: 2_300.0,
                shape_scale_m: 1_600.0,
                detail_scale_m: 360.0,
                look: CloudLook {
                    erosion: 0.6,
                    breakup: 0.45,
                    breakup_scale_m: 2_400.0,
                    ..look
                },
            },
            // Mediocris to congestus: base about 1.2 km, tops 3.5–5 km, LWC ≈ 0.5 g/m³,
            // r_e ≈ 10 µm → β ≈ 0.075 /m. Sharper, deeper cauliflower erosion.
            Self::Cumulus => CloudGenusLayer {
                coverage: 0.38,
                cloud_type: 0.78,
                density: 0.075,
                altitude_bottom: 1_200.0,
                altitude_top: 4_800.0,
                shape_scale_m: 2_400.0,
                detail_scale_m: 420.0,
                look: CloudLook {
                    erosion: 0.68,
                    breakup: 0.35,
                    breakup_scale_m: 3_600.0,
                    ..look
                },
            },
            // Stratocumulus: base 0.6–1.5 km, 200–700 m deep, LWC ≈ 0.3 g/m³, r_e ≈ 10 µm
            // → β ≈ 0.045 /m. Cells 0.5–2 km in a broken sheet (five to seven eighths).
            Self::Stratocumulus => CloudGenusLayer {
                coverage: 0.66,
                cloud_type: 0.2,
                density: 0.045,
                altitude_bottom: 900.0,
                altitude_top: 1_900.0,
                shape_scale_m: 1_400.0,
                detail_scale_m: 300.0,
                look: CloudLook {
                    erosion: 0.55,
                    breakup: 0.6,
                    breakup_scale_m: 1_800.0,
                    ..look
                },
            },
            // Cirrus at 9 km, vertical optical depth 0.2 (thin cirrus, Sassen & Cho 1992),
            // streaks about 2.5 km wide and six times longer along the wind; a few humilis
            // under it.
            Self::Cirrus => CloudGenusLayer {
                coverage: 0.08,
                cloud_type: 0.42,
                density: 0.04,
                altitude_bottom: 1_300.0,
                altitude_top: 2_200.0,
                shape_scale_m: 1_500.0,
                detail_scale_m: 340.0,
                look: CloudLook {
                    erosion: 0.6,
                    breakup: 0.5,
                    breakup_scale_m: 2_200.0,
                    cirrus: CirrusSheet {
                        coverage: 0.5,
                        altitude_m: 9_000.0,
                        optical_depth: 0.2,
                        scale_m: 2_500.0,
                        stretch: 6.0,
                        ..CirrusSheet::NONE
                    },
                    ..look
                },
            },
        }
    }
}

impl CloudSettings {
    /// These settings with `genus`'s layer applied ([`CloudGenusLayer::apply`]).
    #[must_use]
    pub fn with_genus(mut self, genus: CloudGenus) -> Self {
        genus.layer().apply(&mut self);
        self
    }

    /// Whether the layer draws anything: volumetric coverage or a cirrus sheet.
    #[must_use]
    pub fn draws_anything(&self) -> bool {
        self.coverage > 0.0 || self.look.cirrus.sanitized().coverage > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cirrus sheet blend ends exactly on its two sheets, sheared ones included.
    #[test]
    fn cirrus_lerp_ends_exactly_on_sheared_sheets() {
        let a = CirrusSheet {
            shear_deg: -1.643_368_6,
            ..CirrusSheet::FIBROUS
        };
        let b = CirrusSheet {
            shear_deg: -18.183_216,
            coverage: 0.7,
            ..CirrusSheet::FIBROUS
        };
        assert_eq!(CirrusSheet::lerp(a, b, 0.0), a);
        assert_eq!(CirrusSheet::lerp(a, b, 1.0), b);
        let edge = CirrusSheet {
            shear_deg: 180.0,
            ..CirrusSheet::FIBROUS
        };
        assert_eq!(CirrusSheet::lerp(a, edge, 1.0).shear_deg, 180.0);
        assert_eq!(CirrusSheet::lerp(a, b, 2.0), b);
        assert_eq!(CirrusSheet::lerp(a, b, -1.0), a);
    }

    #[test]
    fn diffuse_transmission_is_one_at_zero_and_falls_as_one_over_tau() {
        let g = CLOUD_DROPLET_ASYMMETRY;
        assert!((diffuse_transmission(0.0, g) - 1.0).abs() < 1e-6);
        let mut last = 1.0;
        for i in 1..200 {
            let tau = i as f32 * 0.5;
            let t = diffuse_transmission(tau, g);
            assert!(t < last, "monotone at {tau}");
            // Multiple scattering carries far more light than the direct beam.
            assert!(t > (-tau).exp(), "above Beer at {tau}");
            last = t;
        }
        // Thick limit: T · τ → 1 / (¾ (1 − g)).
        let tau = 1.0e4;
        let want = 1.0 / (0.75 * (1.0 - g));
        assert!((diffuse_transmission(tau, g) * tau - want).abs() / want < 0.01);
        // A cumulus core seen against the sun (τ ≈ 50) still passes about 15 %.
        let core = diffuse_transmission(50.0, g);
        assert!((0.1..0.2).contains(&core), "{core}");
        assert!((diffuse_transmission(f32::NAN, g) - 1.0).abs() < 1e-6);
        assert!((diffuse_transmission(-3.0, g) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_default_look_is_the_legacy_march() {
        assert_eq!(CloudLook::default(), CloudLook::LEGACY);
        assert_eq!(CloudSettings::default().look, CloudLook::LEGACY);
        // The rows the shader reads for the legacy look: no diffusion, erosion 0.5 (the old
        // literal), no breakup, no cirrus.
        assert_eq!(CloudLook::LEGACY.look_row(), [0.0, 0.5, 0.0, 3_000.0]);
        assert_eq!(CloudLook::LEGACY.cirrus_row()[0], 0.0);
        assert!(!CloudSettings::default().draws_anything());
        assert!(CloudSettings::coverage_layer(0.3).draws_anything());
    }

    #[test]
    fn genus_names_round_trip() {
        for (genus, name) in CloudGenus::ALL.into_iter().zip(CloudGenus::NAMES) {
            assert_eq!(genus.name(), name);
            assert_eq!(CloudGenus::from_name(name), Some(genus));
        }
        assert_eq!(CloudGenus::from_name("nimbus"), None);
    }

    /// Each preset sits in the ranges its sources give: low-étage bases under 2 km, extinction
    /// from a measured LWC and droplet radius, thin cirrus in the high étage.
    #[test]
    fn genus_layers_are_physically_sane() {
        for genus in CloudGenus::ALL {
            let l = genus.layer();
            assert_eq!(l.look, l.look.sanitized(), "{genus:?} in range");
            assert!(l.altitude_bottom < 2_000.0, "{genus:?} base in the low étage");
            assert!(l.altitude_top > l.altitude_bottom + 300.0);
            assert!((0.01..=0.12).contains(&l.density), "{genus:?} extinction");
            assert!(l.look.diffusion > 0.0, "{genus:?} uses the diffuse light");
            assert!(l.detail_scale_m < l.shape_scale_m);
        }
        let sc = CloudGenus::Stratocumulus.layer();
        assert!(
            sc.altitude_top - sc.altitude_bottom <= 1_200.0,
            "stratocumulus is a sheet"
        );
        assert!(sc.coverage > 0.5, "broken: more than half the sky");
        let cu = CloudGenus::Cumulus.layer();
        let fair = CloudGenus::FairCumulus.layer();
        assert!(cu.altitude_top > fair.altitude_top && cu.density > fair.density);
        let ci = CloudGenus::Cirrus.layer().look.cirrus;
        assert!((5_000.0..=13_000.0).contains(&ci.altitude_m), "high étage");
        assert!((0.03..=0.3).contains(&ci.optical_depth), "thin cirrus");
        assert!(ci.coverage > 0.0 && ci.stretch > 1.0);
        for genus in [
            CloudGenus::FairCumulus,
            CloudGenus::Cumulus,
            CloudGenus::Stratocumulus,
        ] {
            assert_eq!(genus.layer().look.cirrus.coverage, 0.0);
        }
    }

    #[test]
    fn apply_keeps_wind_time_and_switches() {
        let start = CloudSettings {
            wind_m_per_s: [3.0, -2.0],
            time_s: 40.0,
            seed: 99,
            shadow_map: true,
            ..CloudSettings::default()
        };
        let s = start.with_genus(CloudGenus::Stratocumulus);
        assert_eq!(s.wind_m_per_s, start.wind_m_per_s);
        assert_eq!(s.time_s, 40.0);
        assert_eq!(s.seed, 99);
        assert!(s.shadow_map);
        assert_eq!(s.altitude_bottom, 900.0);
        assert_eq!(CloudGenusLayer::of(&s), CloudGenus::Stratocumulus.layer());
        // Cirrus alone draws even with almost no volumetric cover.
        let ci = CloudSettings {
            coverage: 0.0,
            ..CloudSettings::default().with_genus(CloudGenus::Cirrus)
        };
        assert!(ci.draws_anything());
    }

    #[test]
    fn lerp_blends_shear_deg_across_the_wrap() {
        // Directions straddling the ±180° wrap blend through ±180, not through 0.
        let a = CirrusSheet {
            shear_deg: 170.0,
            ..CirrusSheet::NONE
        };
        let b = CirrusSheet {
            shear_deg: -170.0,
            ..CirrusSheet::NONE
        };
        let mid = CirrusSheet::lerp(a, b, 0.5);
        assert!(
            mid.shear_deg.abs() > 179.0,
            "mid-blend stays near the wrap: {}",
            mid.shear_deg
        );
        let quarter = CirrusSheet::lerp(a, b, 0.25);
        assert!(
            quarter.shear_deg > 174.0 && quarter.shear_deg <= 180.0,
            "quarter-blend moves toward the wrap: {}",
            quarter.shear_deg
        );
        assert_eq!(CirrusSheet::lerp(a, b, 1.0).shear_deg, -170.0);
        // A plain mid-range blend is unchanged by the wrapping.
        let c = CirrusSheet {
            shear_deg: -30.0,
            ..CirrusSheet::NONE
        };
        let d = CirrusSheet {
            shear_deg: 30.0,
            ..CirrusSheet::NONE
        };
        assert_eq!(CirrusSheet::lerp(c, d, 0.5).shear_deg, 0.0);
    }

    #[test]
    fn lerp_ends_exactly_and_sanitize_clamps() {
        let a = CloudGenus::FairCumulus.layer();
        let b = CloudGenus::Cirrus.layer();
        assert_eq!(CloudGenusLayer::lerp(a, b, 0.0), a);
        assert_eq!(CloudGenusLayer::lerp(a, b, 1.0), b);
        let mid = CloudGenusLayer::lerp(a, b, 0.5);
        assert!((mid.look.cirrus.coverage - 0.25).abs() < 1e-6);
        let wild = CloudLook {
            diffusion: f32::NAN,
            erosion: 7.0,
            breakup: -1.0,
            breakup_scale_m: 0.0,
            cirrus: CirrusSheet {
                coverage: 3.0,
                altitude_m: 1.0e9,
                optical_depth: f32::INFINITY,
                scale_m: -5.0,
                stretch: 0.0,
                fiber: 9.0,
                fiber_scale_m: 0.0,
                shear_deg: 720.0,
                shear: f32::NAN,
                breakup: -3.0,
                hook: f32::INFINITY,
                crisp: f32::NAN,
            },
        }
        .sanitized();
        assert_eq!(wild.diffusion, 0.0);
        assert_eq!(wild.erosion, 1.0);
        assert_eq!(wild.breakup, 0.0);
        assert_eq!(wild.breakup_scale_m, 100.0);
        assert_eq!(wild.cirrus.coverage, 1.0);
        assert_eq!(wild.cirrus.altitude_m, 20_000.0);
        assert_eq!(wild.cirrus.optical_depth, CirrusSheet::NONE.optical_depth);
        assert_eq!(wild.cirrus.scale_m, 100.0);
        assert_eq!(wild.cirrus.stretch, 1.0);
        assert_eq!(wild.cirrus.fiber, 1.0);
        assert_eq!(wild.cirrus.fiber_scale_m, 10.0);
        assert_eq!(wild.cirrus.shear_deg, 180.0);
        assert_eq!(wild.cirrus.shear, 0.0);
        assert_eq!(wild.cirrus.breakup, 0.0);
        assert_eq!(wild.cirrus.hook, 0.0);
        assert_eq!(wild.cirrus.crisp, 0.0);
    }

    #[test]
    fn cirrus_streaks_follow_the_wind() {
        let s = CloudSettings {
            wind_m_per_s: [0.0, 8.0],
            time_s: 10.0,
            ..CloudSettings::default()
        };
        let row = cirrus_direction_row(&s);
        assert!((row[0]).abs() < 1e-6 && (row[1] - 1.0).abs() < 1e-6);
        assert!((row[3] - 80.0 * CIRRUS_WIND_RATIO).abs() < 1e-3);
        let calm = CloudSettings {
            wind_m_per_s: [0.0, 0.0],
            ..CloudSettings::default()
        };
        assert_eq!(cirrus_direction_row(&calm)[..2], [1.0, 0.0]);
    }

    fn luminance(c: [f32; 3]) -> f32 {
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }

    #[test]
    fn cirrus_phase_is_normalised_forward_with_ice_asymmetry() {
        // ∫ p dΩ = 1 and ∫ p cos θ dΩ = g over the sphere (midpoint rule in cos θ).
        let n = 20_000;
        let (mut norm, mut mean_cos) = (0.0f64, 0.0f64);
        for i in 0..n {
            let c = -1.0 + (i as f64 + 0.5) * 2.0 / n as f64;
            let p = f64::from(cirrus_phase(c as f32)) * 2.0 * std::f64::consts::PI * 2.0 / n as f64;
            norm += p;
            mean_cos += p * c;
        }
        assert!((norm - 1.0).abs() < 0.01, "normalised: {norm}");
        assert!(
            (mean_cos - f64::from(CIRRUS_ICE_ASYMMETRY)).abs() < 0.01,
            "g = {mean_cos}"
        );
        // Far brighter toward the sun than to the side, and the side above the back.
        assert!(cirrus_phase(5f32.to_radians().cos()) > 20.0 * cirrus_phase(0.0));
        assert!(cirrus_phase(0.0) > cirrus_phase(-1.0));
    }

    /// Filaments, not a veil: over a uniform noise only a small part of the sky carries a
    /// filament, and the ridge is 1 on the crossing and 0 away from it.
    #[test]
    fn cirrus_ridges_are_thin() {
        assert!((cirrus_ridge(0.5) - 1.0).abs() < 1e-6);
        assert_eq!(cirrus_ridge(0.2), 0.0);
        assert_eq!(cirrus_ridge(0.9), 0.0);
        let n = 1_000;
        let dense = (0..n)
            .filter(|i| cirrus_ridge((*i as f32 + 0.5) / n as f32) > 0.3)
            .count();
        assert!(dense * 5 < n, "{dense} of {n} samples carry a dense filament");
    }

    /// The sheet hides only the air behind it, and its own light is the sun's single scattering
    /// plus the sky: a streak is brighter than the clear sky around it at every angle from the
    /// sun, strongly near the sun and only slightly away from it (forward-scattering ice). The
    /// look round 1 sheet drew streaks about 40 % darker than the sky.
    #[test]
    fn cirrus_is_brighter_than_the_local_sky_strongest_near_the_sun() {
        let el = 21f32.to_radians();
        let to_sun = [0.0, el.sin(), el.cos()];
        let luts = crate::build_atmosphere_luts(to_sun, 2.0);
        let sheet = CloudGenus::Cirrus.layer().look.cirrus;
        let (color, lux) = crate::transmitted_sun_at(&luts, sheet.altitude_m);
        let sky = crate::ambient_irradiance_table(&luts);
        let ground = crate::atmosphere::ground_radiance(&luts);
        let bottom = std::array::from_fn(|c| 0.5 * sky[c] + 0.5 * ground[c]);
        let mut gains = Vec::new();
        // View 30° above the horizon at azimuths 0..180° from the sun's.
        for az_deg in [0.0f32, 15.0, 30.0, 60.0, 90.0, 120.0, 150.0, 180.0] {
            let (az, vel) = (az_deg.to_radians(), 30f32.to_radians());
            let dir = [vel.cos() * az.sin(), vel.sin(), vel.cos() * az.cos()];
            let cos_sun = dir[0] * to_sun[0] + dir[1] * to_sun[1] + dir[2] * to_sun[2];
            let t = sheet.altitude_m / vel.sin();
            let (_, air) = crate::sample_aerial(&luts, dir, t);
            let view = CirrusView {
                sun: color.map(|c| c * lux),
                cos_sun,
                mu: vel.sin(),
                sky_ambient: sky,
                ground_ambient: bottom,
                air_transmittance: air,
            };
            let bg = crate::sample_sky_table(&luts, dir);
            let (rgb, a) = cirrus_composite(&sheet, 1.0, &view);
            let out = std::array::from_fn(|c| bg[c] * a + rgb[c]);
            let gain = luminance(out) / luminance(bg);
            let angle = cos_sun.acos().to_degrees();
            println!("cirrus at {angle:5.1}° from the sun: streak / sky = {gain:.2}");
            assert!(
                gain > 1.0,
                "a streak {angle:.0}° from the sun is darker than the sky: {gain}"
            );
            gains.push((angle, gain));
            // Nothing over a clear texel.
            let (rgb0, a0) = cirrus_composite(&sheet, 0.0, &view);
            assert_eq!((luminance(rgb0), a0), (0.0, 1.0));
        }
        let near = gains
            .iter()
            .filter(|g| g.0 < 35.0)
            .map(|g| g.1)
            .fold(f32::MAX, f32::min);
        let far = gains
            .iter()
            .filter(|g| g.0 > 80.0)
            .map(|g| g.1)
            .fold(0.0, f32::max);
        assert!(near > 1.8, "near the sun the streak glows: {near}");
        assert!(far < 1.6, "away from the sun the streak is subtle: {far}");
        assert!(near > far + 0.5);
    }

    /// The CPU mirror shades a crisp sheet's strand peaks like the GPU: densities above 1 are
    /// not clipped at 1 (the pre-ADR-1516 clamp lost exactly the contrast crisp adds).
    #[test]
    fn cirrus_composite_keeps_crisp_density_peaks() {
        let sheet = CirrusSheet::FIBROUS.with_crisp(1.0);
        let view = CirrusView {
            sun: [3.0e6, 2.9e6, 2.7e6],
            cos_sun: 0.5,
            mu: 0.5,
            sky_ambient: [2_000.0; 3],
            ground_ambient: [1_000.0; 3],
            air_transmittance: 0.9,
        };
        let cap = CIRRUS_CRISP_CAP;
        let (rgb_peak, a_peak) = cirrus_composite(&sheet, cap, &view);
        let (rgb_clip, _) = cirrus_composite(&sheet, 1.0, &view);
        let (rgb_over, a_over) = cirrus_composite(&sheet, cap * 2.0, &view);
        let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        assert!(
            lum(rgb_peak) > lum(rgb_clip),
            "a crisp strand peak at the cap shades brighter than clipped at 1: {} vs {}",
            lum(rgb_peak),
            lum(rgb_clip)
        );
        assert_eq!(
            (rgb_peak, a_peak),
            (rgb_over, a_over),
            "densities past the cap clamp to the cap"
        );
        // The off path is unchanged: a non-crisp sheet still clamps at 1.
        let plain = CirrusSheet::FIBROUS;
        let (_, a_plain) = cirrus_composite(&plain, cap, &view);
        let (_, a_plain_clip) = cirrus_composite(&plain, 1.0, &view);
        assert_eq!(a_plain, a_plain_clip);
    }

    /// `cirrus_density` over `w × h` streak tiles every `step` tiles, full band, stretch 6.
    fn cirrus_grid(w: f32, h: f32, step: f32, footprint: f32) -> Vec<f32> {
        let (nx, ny) = ((w / step) as usize, (h / step) as usize);
        let mut out = Vec::with_capacity(nx * ny);
        for j in 0..ny {
            for i in 0..nx {
                out.push(cirrus_density(
                    i as f32 * step,
                    j as f32 * step,
                    1.0,
                    6.0,
                    footprint,
                ));
            }
        }
        out
    }

    fn mean(v: &[f32]) -> f32 {
        v.iter().sum::<f32>() / v.len().max(1) as f32
    }

    /// Normalised autocorrelation of `v` at `lag` samples.
    fn autocorrelation(v: &[f32], lag: usize) -> f32 {
        let m = mean(v);
        let var: f32 = v.iter().map(|x| (x - m) * (x - m)).sum();
        let cov: f32 = v.iter().zip(&v[lag..]).map(|(a, b)| (a - m) * (b - m)).sum();
        cov / var.max(1e-12)
    }

    /// First lag (in samples) where the autocorrelation of `v` falls below one half.
    fn correlation_length(v: &[f32]) -> usize {
        (1..v.len() / 2)
            .find(|lag| autocorrelation(v, *lag) < 0.5)
            .unwrap_or(v.len() / 2)
    }

    #[test]
    fn cirrus_density_is_bounded_deterministic_and_empty_outside_a_band() {
        let points = [
            (0.0, 0.0),
            (3.7, -12.2),
            (1.0e6, -4.0e5),
            (f32::NAN, 1.0),
            (2.0, f32::INFINITY),
        ];
        for (x, y) in points {
            assert_eq!(cirrus_density(x, y, 0.0, 6.0, 0.0), 0.0, "no band, no cirrus");
            assert_eq!(cirrus_density(x, y, f32::NAN, 6.0, 0.0), 0.0);
            for footprint in [0.0, 0.01, 0.3, 10.0, f32::NAN, -1.0] {
                for stretch in [1.0, 6.0, 32.0, f32::NAN, 0.0] {
                    let d = cirrus_density(x, y, 1.0, stretch, footprint);
                    assert!(
                        (0.0..=1.0).contains(&d),
                        "{d} at ({x}, {y}), footprint {footprint}, stretch {stretch}"
                    );
                }
            }
        }
        let a = cirrus_density(12.3, 4.5, 0.7, 6.0, 0.01);
        assert_eq!(a.to_bits(), cirrus_density(12.3, 4.5, 0.7, 6.0, 0.01).to_bits());
    }

    #[test]
    fn cirrus_trails_leave_clear_sky_and_a_few_dense_heads() {
        // ADR-1470: the old sheet laid evenly spaced strands over its whole band. Fallstreaks
        // leave most of a full band at the faint veil, with a few dense heads.
        let d = cirrus_grid(240.0, 60.0, 0.1, 0.001);
        let n = d.len() as f32;
        let clear = d.iter().filter(|v| **v < CIRRUS_VEIL + 0.01).count() as f32 / n;
        let dense = d.iter().filter(|v| **v > 0.3).count() as f32 / n;
        println!("cirrus: clear {clear:.3}, dense {dense:.3}");
        assert!(
            (0.6..0.92).contains(&clear),
            "clear sky between the trails: {clear}"
        );
        assert!(dense > 0.01, "dense heads: {dense}");
    }

    #[test]
    fn cirrus_trails_are_irregularly_spaced_across_the_wind() {
        // Along lines across the wind, the gaps between trail crossings: evenly spaced strands
        // give one gap length (coefficient of variation near 0), fallstreaks from randomly
        // placed generating cells give a cv near 1.
        let step = 1.0 / 32.0;
        let mut gaps = Vec::new();
        for line in 0..33 {
            let x = 0.37 + line as f32 * 7.3;
            let (mut last, mut was_on) = (None, false);
            for i in 0..(400.0 / step) as usize {
                let on = cirrus_density(x, i as f32 * step, 1.0, 6.0, 0.001) > CIRRUS_VEIL + 0.05;
                if on && !was_on {
                    if let Some(start) = last {
                        gaps.push((i - start) as f32 * step);
                    }
                    last = Some(i);
                }
                was_on = on;
            }
        }
        let m = mean(&gaps);
        let sd = (gaps.iter().map(|g| (g - m) * (g - m)).sum::<f32>() / gaps.len().max(1) as f32).sqrt();
        println!("cirrus: {} gaps, mean {m:.2} tiles, cv {:.2}", gaps.len(), sd / m);
        assert!(gaps.len() > 1000, "trails cross the lines: {}", gaps.len());
        assert!(sd / m > 0.6, "irregular spacing: cv {}", sd / m);
        // No period across the wind: the autocorrelation stays low past one tile.
        let line: Vec<f32> = (0..32_000)
            .map(|i| cirrus_density(3.3, i as f32 / 16.0, 1.0, 6.0, 0.001))
            .collect();
        let peak = (16..640)
            .map(|lag| autocorrelation(&line, lag))
            .fold(f32::MIN, f32::max);
        assert!(peak < 0.3, "periodic across the wind: {peak}");
    }

    #[test]
    fn cirrus_trails_run_along_the_wind() {
        let along: Vec<f32> = (0..24_000)
            .map(|i| cirrus_density(i as f32 / 8.0, 7.6, 1.0, 6.0, 0.001))
            .collect();
        let across: Vec<f32> = (0..24_000)
            .map(|i| cirrus_density(3.3, i as f32 / 64.0, 1.0, 6.0, 0.001))
            .collect();
        let l_along = correlation_length(&along) as f32 / 8.0;
        let l_across = correlation_length(&across) as f32 / 64.0;
        println!("cirrus correlation length: along {l_along} tiles, across {l_across} tiles");
        assert!(
            l_along > 4.0 * l_across,
            "streaks along the wind: {l_along} vs {l_across}"
        );
    }

    #[test]
    fn cirrus_prefilter_keeps_the_mean_and_nothing_is_finer_than_the_footprint() {
        // The far field is the area mean of the near one, so the sky does not brighten or dim
        // toward the horizon where the footprint grows.
        let near = mean(&cirrus_grid(240.0, 60.0, 0.15, 0.001));
        let far = cirrus_density(1.0, 2.0, 1.0, 6.0, 0.7);
        assert!((far - (CIRRUS_VEIL + CIRRUS_TRAIL_MEAN)).abs() < 1e-6);
        for footprint in [0.05, 0.14, 0.3, 0.45] {
            let m = mean(&cirrus_grid(240.0, 60.0, 0.15, footprint));
            assert!(
                (m / near - 1.0).abs() < 0.1,
                "mean at footprint {footprint}: {m} vs {near}"
            );
        }
        assert!(
            (far / near - 1.0).abs() < 0.1,
            "far field {far} vs near mean {near}"
        );
        // Sampled once per footprint across the wind, the field never jumps by more than a
        // fraction of its range: no hairline detail left to alias.
        for footprint in [0.002f32, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.3, 0.4, 0.5] {
            let mut worst = 0.0f32;
            for line in 0..40 {
                let x = 0.37 + line as f32 * 6.1;
                let n = (60.0 / footprint) as usize;
                let mut prev = cirrus_density(x, 0.0, 1.0, 6.0, footprint);
                for i in 1..n {
                    let d = cirrus_density(x, i as f32 * footprint, 1.0, 6.0, footprint);
                    worst = worst.max((d - prev).abs());
                    prev = d;
                }
            }
            println!("cirrus: footprint {footprint}, largest step {worst:.3}");
            assert!(worst < 0.4, "aliasing at footprint {footprint}: step {worst}");
        }
    }

    /// The sheet as the look round sets it (ADR-1509 tests): the genus sheet with fibrous
    /// values, a 1500 m tile.
    fn fibrous(fiber: f32, shear: f32, hook: f32, breakup: f32, shear_deg: f32) -> CirrusSheet {
        CirrusSheet {
            scale_m: 1_500.0,
            fiber,
            shear,
            hook,
            breakup,
            shear_deg,
            ..CloudGenus::Cirrus.layer().look.cirrus
        }
    }

    /// `cirrus_density_fibrous` along `n` points from `(x0, y0)` every `step` tiles in direction
    /// `deg` (from the wind), full band.
    fn fibrous_line(
        sheet: &CirrusSheet,
        (x0, y0): (f32, f32),
        deg: f32,
        step: f32,
        n: usize,
        footprint: f32,
        slant: [f32; 2],
    ) -> Vec<f32> {
        let (sin, cos) = deg.to_radians().sin_cos();
        (0..n)
            .map(|i| {
                let t = i as f32 * step;
                cirrus_density_fibrous(x0 + t * cos, y0 + t * sin, 1.0, footprint, slant, sheet)
            })
            .collect()
    }

    /// ADR-1509: every fibrous value defaults to off, so the genus sheet and every existing
    /// scene draw the ADR-1470 trails (the shader takes that path when the flag is 0).
    #[test]
    fn fibrous_values_default_to_off() {
        for sheet in [CirrusSheet::NONE, CloudGenus::Cirrus.layer().look.cirrus] {
            assert!(!sheet.is_fibrous());
            let look = CloudLook {
                cirrus: sheet,
                ..CloudLook::LEGACY
            };
            assert_eq!(look.cirrus_shear_row(), [1.0, 0.0, 0.0, 0.0]);
            assert_eq!(look.cirrus_fiber_row(), [0.0, 150.0, 0.0, 0.0]);
        }
        assert!(CirrusSheet::FIBROUS.is_fibrous());
        // Any one value switches the fibrous field on.
        for one in [
            fibrous(0.1, 0.0, 0.0, 0.0, 0.0),
            fibrous(0.0, 0.1, 0.0, 0.0, 0.0),
            fibrous(0.0, 0.0, 0.1, 0.0, 0.0),
            fibrous(0.0, 0.0, 0.0, 0.1, 0.0),
        ] {
            assert!(one.is_fibrous());
        }
        // The shear direction and fiber scale alone do not.
        assert!(!fibrous(0.0, 0.0, 0.0, 0.0, 30.0).is_fibrous());
        let base = CloudGenus::Cirrus.layer().look.cirrus;
        let f = base.with_fibrous_look();
        assert_eq!(
            (f.coverage, f.altitude_m, f.optical_depth, f.scale_m, f.stretch),
            (
                base.coverage,
                base.altitude_m,
                base.optical_depth,
                base.scale_m,
                base.stretch
            )
        );
        assert_eq!(
            (f.fiber, f.shear, f.hook, f.breakup),
            (
                CirrusSheet::FIBROUS.fiber,
                CirrusSheet::FIBROUS.shear,
                CirrusSheet::FIBROUS.hook,
                CirrusSheet::FIBROUS.breakup
            )
        );
        let (s, c) = 30f32.to_radians().sin_cos();
        let row = CloudLook {
            cirrus: fibrous(0.5, 0.0, 0.0, 0.25, 30.0),
            ..CloudLook::LEGACY
        }
        .cirrus_shear_row();
        assert!((row[0] - c).abs() < 1e-6 && (row[1] - s).abs() < 1e-6);
        assert_eq!(row[2..], [0.25, 1.0]);
    }

    #[test]
    fn fibrous_density_is_bounded_deterministic_and_empty_outside_a_band() {
        let sheets = [
            CirrusSheet::FIBROUS,
            fibrous(1.0, 1.0, 1.0, 1.0, 180.0),
            fibrous(1.0, 0.0, 0.0, 0.0, -90.0),
            CirrusSheet {
                fiber: f32::NAN,
                shear: f32::INFINITY,
                stretch: 0.0,
                scale_m: 0.0,
                fiber_scale_m: -1.0,
                ..CirrusSheet::FIBROUS
            },
        ];
        let points = [
            (0.0, 0.0),
            (3.7, -12.2),
            (1.0e5, -4.0e4),
            (f32::NAN, 1.0),
            (2.0, f32::INFINITY),
        ];
        for sheet in &sheets {
            for (x, y) in points {
                assert_eq!(cirrus_density_fibrous(x, y, 0.0, 0.0, [0.0, 0.0], sheet), 0.0);
                assert_eq!(
                    cirrus_density_fibrous(x, y, f32::NAN, 0.0, [0.0, 0.0], sheet),
                    0.0
                );
                for footprint in [0.0, 0.01, 0.3, 10.0, f32::NAN, -1.0] {
                    for slant in [[0.0, 0.0], [3.0, -2.0], [1.0e4, 1.0e4], [f32::NAN, 0.0]] {
                        let d = cirrus_density_fibrous(x, y, 1.0, footprint, slant, sheet);
                        assert!(
                            (0.0..=1.0).contains(&d),
                            "{d} at ({x}, {y}), footprint {footprint}, slant {slant:?}"
                        );
                    }
                }
            }
        }
        let a = cirrus_density_fibrous(12.3, 4.5, 0.7, 0.01, [0.4, 1.2], &CirrusSheet::FIBROUS);
        let b = cirrus_density_fibrous(12.3, 4.5, 0.7, 0.01, [0.4, 1.2], &CirrusSheet::FIBROUS);
        assert_eq!(a.to_bits(), b.to_bits());
    }

    /// The fibers are fine structure across the shear: sampled every 1/16 of the fiber
    /// spacing across the trails, the field bends far more with fibers than without (the
    /// ADR-1470 trails are smooth there).
    #[test]
    fn fibers_add_fine_structure_across_the_shear() {
        let roughness = |fiber: f32| {
            let sheet = fibrous(fiber, 0.6, 0.6, 0.0, 0.0);
            let step = sheet.fiber_scale_m / sheet.scale_m / 16.0;
            let mut sum = 0.0f32;
            let mut count = 0usize;
            for line in 0..40 {
                let x = 0.37 + line as f32 * 7.3;
                let v = fibrous_line(
                    &sheet,
                    (x, 0.0),
                    90.0,
                    step,
                    (60.0 / step) as usize,
                    0.001,
                    [0.0, 0.0],
                );
                for w in v.windows(3) {
                    sum += (w[0] - 2.0 * w[1] + w[2]).abs();
                    count += 1;
                }
            }
            sum / count.max(1) as f32
        };
        let (smooth, combed) = (roughness(0.0), roughness(1.0));
        println!("cirrus fibers: roughness {smooth:.4} without, {combed:.4} with");
        assert!(combed > 5.0 * smooth, "fibers: {combed} vs {smooth}");
    }

    /// Trails and fibers lie along the shear direction, not the wind's, when they differ.
    #[test]
    fn fibers_lie_along_the_shear_direction() {
        let sheet = fibrous(1.0, 0.6, 0.6, 0.0, 40.0);
        let length = |deg: f32, step: f32, n: usize| {
            let mut total = 0.0;
            for line in 0..12 {
                let o = (3.1 * line as f32, 1.7 * line as f32);
                let v = fibrous_line(&sheet, o, deg, step, n, 0.001, [0.0, 0.0]);
                total += correlation_length(&v) as f32 * step;
            }
            total / 12.0
        };
        let along = length(40.0, 0.02, 20_000);
        let across = length(-50.0, 0.005, 20_000);
        let wind = length(0.0, 0.02, 20_000);
        println!("cirrus shear 40°: correlation along {along}, across {across}, wind {wind}");
        assert!(along > 4.0 * across, "along the shear: {along} vs {across}");
        assert!(along > 2.5 * wind, "not along the wind: {along} vs {wind}");
    }

    /// Fall-streak tails hang below the sheet: seen at a slant (the view's horizontal
    /// direction over its vertical part) a trail leans toward that direction on the sheet, and
    /// the other way for the opposite slant; seen from straight below it does not lean.
    #[test]
    fn sheared_tails_lean_with_the_view_slant() {
        let sheet = fibrous(0.0, 0.8, 0.0, 0.0, 0.0);
        let lean = |slant: [f32; 2]| {
            let length = |deg: f32| {
                let mut total = 0.0;
                for line in 0..30 {
                    let o = (3.1 * line as f32, 1.3 * line as f32);
                    let v = fibrous_line(&sheet, o, deg, 0.02, 6_000, 0.001, slant);
                    total += correlation_length(&v) as f32 * 0.02;
                }
                total / 30.0
            };
            (length(24.0), length(-24.0))
        };
        let (up, down) = lean([0.0, 2.0]);
        println!("cirrus slant +2: l(+24°) {up}, l(−24°) {down}");
        assert!(up > 2.5 * down, "leans with the slant: {up} vs {down}");
        let (up, down) = lean([0.0, -2.0]);
        assert!(down > 2.5 * up, "and the other way: {up} vs {down}");
        let (up, down) = lean([0.0, 0.0]);
        assert!(
            (up / down - 1.0).abs() < 0.35,
            "no lean from below: {up} vs {down}"
        );
    }

    /// Breakup leaves clear sky between patches at full strength and none at 0, where the
    /// faint veil covers the whole band.
    #[test]
    fn breakup_leaves_clear_sky_between_patches() {
        let clear = |breakup: f32| {
            let sheet = fibrous(0.8, 0.6, 0.6, breakup, 0.0);
            let mut n = 0usize;
            let total = 200 * 200;
            for j in 0..200 {
                for i in 0..200 {
                    let d = cirrus_density_fibrous(
                        i as f32 * 2.43 + 0.1,
                        j as f32 * 2.37 + 0.2,
                        1.0,
                        0.001,
                        [0.0, 0.0],
                        &sheet,
                    );
                    if d <= 0.0 {
                        n += 1;
                    }
                }
            }
            n as f32 / total as f32
        };
        let (none, full) = (clear(0.0), clear(1.0));
        println!("cirrus breakup: clear {none} at 0, {full} at 1");
        assert_eq!(none, 0.0);
        assert!((0.3..0.75).contains(&full), "patches: {full}");
    }

    /// The prefilter keeps the area mean as the footprint grows, and the far field matches it
    /// (so the fibrous sheet does not brighten or dim toward the horizon); strands are never
    /// resolved by fewer than about two footprints.
    #[test]
    fn fibrous_prefilter_keeps_the_mean() {
        let grid = |sheet: &CirrusSheet, footprint: f32| {
            let mut v = Vec::with_capacity(300 * 300);
            for j in 0..300 {
                for i in 0..300 {
                    v.push(cirrus_density_fibrous(
                        i as f32 * 1.13 + 0.05,
                        j as f32 * 0.97 + 0.11,
                        1.0,
                        footprint,
                        [0.0, 0.0],
                        sheet,
                    ));
                }
            }
            mean(&v)
        };
        for sheet in [
            CirrusSheet::FIBROUS,
            fibrous(1.0, 0.0, 0.0, 0.0, 0.0),
            fibrous(0.0, 1.0, 1.0, 0.0, 0.0),
            fibrous(0.3, 0.3, 0.0, 0.0, 0.0),
        ] {
            let sheet = CirrusSheet {
                scale_m: 1_500.0,
                breakup: 0.0,
                ..sheet
            };
            let near = grid(&sheet, 0.001);
            for footprint in [0.02, 0.05, 0.14, 0.3] {
                let m = grid(&sheet, footprint);
                assert!(
                    (m / near - 1.0).abs() < 0.15,
                    "mean at footprint {footprint}: {m} vs {near} ({sheet:?})"
                );
            }
            let far = cirrus_density_fibrous(1.0, 2.0, 1.0, 0.7, [0.0, 0.0], &sheet);
            assert!(
                (far / near - 1.0).abs() < 0.25,
                "far {far} vs near {near} ({sheet:?})"
            );
        }
        let sheet = CirrusSheet {
            scale_m: 1_500.0,
            ..fibrous(1.0, 0.6, 0.6, 0.0, 0.0)
        };
        for footprint in [0.002f32, 0.01, 0.05, 0.1, 0.2, 0.3, 0.4, 0.5] {
            let mut worst = 0.0f32;
            for line in 0..40 {
                let x = 0.37 + line as f32 * 6.1;
                let n = (60.0 / footprint) as usize;
                let v = fibrous_line(&sheet, (x, 0.0), 90.0, footprint, n, footprint, [0.0, 0.0]);
                for w in v.windows(2) {
                    worst = worst.max((w[1] - w[0]).abs());
                }
            }
            println!("cirrus fibrous: footprint {footprint}, largest step {worst:.3}");
            assert!(worst < 0.85, "aliasing at footprint {footprint}: step {worst}");
            if footprint >= 0.4 {
                assert!(worst < 0.4, "smooth where the fibers are gone: {worst}");
            }
        }
    }

    #[test]
    fn cirrus_constants_match_the_shader() {
        let wgsl = include_str!("../shaders/clouds.wgsl");
        let value = |name: &str| -> f32 {
            let key = format!("const {name}: f32 = ");
            let Some(start) = wgsl.find(&key).map(|i| i + key.len()) else {
                panic!("{name} is missing from clouds.wgsl");
            };
            let end = wgsl[start..].find(';').map_or(start, |e| start + e);
            wgsl[start..end].trim().parse().unwrap_or(f32::NAN)
        };
        for (name, v) in [
            ("CIRRUS_CELL_ALONG", CIRRUS_CELL_ALONG),
            ("CIRRUS_CELL_ACTIVE", CIRRUS_CELL_ACTIVE),
            ("CIRRUS_VEIL", CIRRUS_VEIL),
            ("CIRRUS_TRAIL_MEAN", CIRRUS_TRAIL_MEAN),
            ("CIRRUS_FIBRE_CONTRAST", CIRRUS_FIBRE_CONTRAST),
            ("CIRRUS_FIBRE_GAIN", CIRRUS_FIBRE_GAIN),
            ("CIRRUS_FIBRE_CELLS", CIRRUS_FIBRE_CELLS),
            ("CIRRUS_TRAIL_FILTER", CIRRUS_TRAIL_FILTER),
            ("CIRRUS_TRAIL_GAIN", CIRRUS_TRAIL_GAIN),
            ("CIRRUS_FALL_DEPTH_M", CIRRUS_FALL_DEPTH_M),
            ("CIRRUS_SLANT_MAX", CIRRUS_SLANT_MAX),
            ("CIRRUS_HOOK_TUFT", CIRRUS_HOOK_TUFT),
            ("CIRRUS_FIBROUS_FILTER", CIRRUS_FIBROUS_FILTER),
            ("CIRRUS_FIBROUS_WARP", CIRRUS_FIBROUS_WARP),
            ("CIRRUS_STRAND_RIDGE", CIRRUS_STRAND_RIDGE),
            ("CIRRUS_STRAND_BASE", CIRRUS_STRAND_BASE),
            ("CIRRUS_STRAND_MEAN", CIRRUS_STRAND_MEAN),
            ("CIRRUS_STRAND_BREAK", CIRRUS_STRAND_BREAK),
            ("CIRRUS_SHEET_FIBERS", CIRRUS_SHEET_FIBERS),
            ("CIRRUS_FIBER_PATCH_MEAN", CIRRUS_FIBER_PATCH_MEAN),
            ("CIRRUS_FIBER_ANISO", CIRRUS_FIBER_ANISO),
            ("CIRRUS_FIBER_OCTAVE_MEAN", CIRRUS_FIBER_OCTAVE_MEAN),
            ("CIRRUS_CRISP_FILTER", CIRRUS_CRISP_FILTER),
            ("CIRRUS_CRISP_RIDGE", CIRRUS_CRISP_RIDGE),
            ("CIRRUS_CRISP_RIDGE_WIDTH", CIRRUS_CRISP_RIDGE_WIDTH),
            ("CIRRUS_CRISP_BASE", CIRRUS_CRISP_BASE),
            ("CIRRUS_CRISP_MEAN_A", CIRRUS_CRISP_MEAN_A),
            ("CIRRUS_CRISP_MEAN_B", CIRRUS_CRISP_MEAN_B),
            ("CIRRUS_CRISP_VEIL", CIRRUS_CRISP_VEIL),
            ("CIRRUS_CRISP_CAP", CIRRUS_CRISP_CAP),
        ] {
            assert_eq!(value(name), v, "{name}");
        }
    }

    /// ADR-1516: crisp is off everywhere by default, and an off sheet's uniform row asks for one
    /// centred sheet sample (the shader then runs the ADR-1509 path unchanged).
    #[test]
    fn crisp_defaults_to_off() {
        for sheet in [
            CirrusSheet::NONE,
            CirrusSheet::FIBROUS,
            CloudGenus::Cirrus.layer().look.cirrus,
            CloudGenus::Cirrus.layer().look.cirrus.with_fibrous_look(),
        ] {
            assert_eq!(sheet.crisp, 0.0);
            let look = CloudLook {
                cirrus: sheet,
                ..CloudLook::LEGACY
            };
            assert_eq!(look.cirrus_crisp_row(), [0.0, 1.0, 0.0, 0.0]);
        }
        assert_eq!(CloudLook::default().cirrus_crisp_row(), [0.0, 1.0, 0.0, 0.0]);
        let crisp = CloudGenus::Cirrus.layer().look.cirrus.with_crisp(0.7);
        assert!(crisp.is_fibrous(), "crisp alone switches the fibrous sheet on");
        let look = CloudLook {
            cirrus: crisp,
            ..CloudLook::LEGACY
        };
        assert_eq!(
            look.cirrus_crisp_row(),
            [0.7, CIRRUS_CRISP_SAMPLES as f32, 0.0, 0.0]
        );
        assert_eq!(look.cirrus_shear_row()[3], 1.0);
        for (raw, clean) in [(-1.0, 0.0), (5.0, 1.0), (f32::NAN, 0.0), (f32::INFINITY, 0.0)] {
            assert_eq!(
                CirrusSheet::NONE.with_crisp(raw).sanitized().crisp,
                clean,
                "{raw}"
            );
        }
        let half = CirrusSheet::lerp(CirrusSheet::NONE, CirrusSheet::NONE.with_crisp(1.0), 0.5);
        assert_eq!(half.crisp, 0.5);
    }

    /// ADR-1516: the two linear taps of `cirrus_bspline_axis` reproduce the cubic B-spline of
    /// the four texels around the point (Sigg & Hadwiger 2005), and the taps lie between the
    /// texels they blend.
    #[test]
    fn bspline_taps_reproduce_the_cubic_bspline() {
        let size = 16.0f32;
        let texel = |i: i32| ((i.rem_euclid(16) as f32) * 2.399).sin();
        // Linear interpolation of the texels at texel-space position `x` (centres at k + 0.5).
        let linear = |x: f32| {
            let t = x - 0.5;
            let i = t.floor();
            let f = t - i;
            texel(i as i32) * (1.0 - f) + texel(i as i32 + 1) * f
        };
        let mut worst = 0.0f32;
        for k in 0..400 {
            let uv = k as f32 * 0.003_7 + 0.001;
            let [h0, h1, g0] = cirrus_bspline_axis(uv, size);
            let two = linear(h0 * size) * g0 + linear(h1 * size) * (1.0 - g0);
            let x = uv * size - 0.5;
            let i = x.floor() as i32;
            let f = x - x.floor();
            let w = [
                (1.0 - f).powi(3) / 6.0,
                (4.0 - 6.0 * f * f + 3.0 * f.powi(3)) / 6.0,
                (1.0 + 3.0 * f + 3.0 * f * f - 3.0 * f.powi(3)) / 6.0,
                f.powi(3) / 6.0,
            ];
            let direct: f32 = w.iter().zip(i - 1..).map(|(wj, k)| wj * texel(k)).sum();
            worst = worst.max((two - direct).abs());
            assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-5);
            let (c0, c1) = (h0 * size - 0.5, h1 * size - 0.5);
            assert!(
                c0 >= (i - 1) as f32 - 1e-4 && c0 <= i as f32 + 1e-4,
                "tap 0 at {c0}, texel {i}"
            );
            assert!(
                c1 >= (i + 1) as f32 - 1e-4 && c1 <= (i + 2) as f32 + 1e-4,
                "tap 1 at {c1}"
            );
        }
        assert!(worst < 1e-4, "two taps vs the cubic B-spline: {worst}");
        for v in cirrus_bspline_axis(f32::NAN, f32::INFINITY) {
            assert!(v.is_finite());
        }
    }

    /// ADR-1516, the saw-tooth: the curl texture read bilinearly is piecewise linear, so its
    /// slope jumps at every texel edge and the warped trails run as polygons. Read through the
    /// B-spline taps the slope is continuous: along a texel row of the real curl texture the
    /// largest second difference falls more than fourfold (measured about 13-fold).
    #[test]
    fn the_bspline_warp_has_no_kinks() {
        let curl = crate::clouds::cloud_curl();
        let n = crate::clouds::CURL_SIZE as i32;
        let row = 37;
        let texel = |i: i32| f32::from(curl[((row * n + i.rem_euclid(n)) * 4) as usize]) / 255.0 * 2.0 - 1.0;
        let linear = |x: f32| {
            let t = x - 0.5;
            let i = t.floor();
            let f = t - i;
            texel(i as i32) * (1.0 - f) + texel(i as i32 + 1) * f
        };
        let size = n as f32;
        let smooth = |x: f32| {
            let [h0, h1, g0] = cirrus_bspline_axis(x / size, size);
            linear(h0 * size) * g0 + linear(h1 * size) * (1.0 - g0)
        };
        let step = 1.0 / 16.0;
        let largest_d2 = |f: &dyn Fn(f32) -> f32| {
            let v: Vec<f32> = (0..(n * 16)).map(|k| f(k as f32 * step + 0.013)).collect();
            v.windows(3)
                .map(|w| (w[2] - 2.0 * w[1] + w[0]).abs() / (step * step))
                .fold(0.0f32, f32::max)
        };
        let kinked = largest_d2(&linear);
        let smoothed = largest_d2(&smooth);
        println!("curl row {row}: largest second difference bilinear {kinked:.3}, B-spline {smoothed:.3}");
        assert!(kinked > 0.1, "the bilinear read should kink: {kinked}");
        assert!(
            smoothed < 0.25 * kinked,
            "B-spline {smoothed} vs bilinear {kinked}"
        );
    }

    /// ADR-1516: a crisp sheet keeps about the same area mean (so the sky does not brighten or
    /// darken overall) while its strands get sharper, their peaks denser and the gaps clearer;
    /// steps between samples one footprint apart stay inside the raised cap.
    #[test]
    fn crisp_keeps_the_mean_and_sharpens_the_strands() {
        let base = CirrusSheet {
            fiber_scale_m: 120.0,
            ..fibrous(1.0, 0.6, 0.45, 0.0, 0.0)
        };
        let crisp = base.with_crisp(1.0);
        let grid = |sheet: &CirrusSheet, footprint: f32| {
            let mut v = Vec::with_capacity(160 * 160);
            for j in 0..160 {
                for i in 0..160 {
                    v.push(cirrus_density_fibrous(
                        i as f32 * 0.37 + 0.11,
                        j as f32 * 0.37 + 0.07,
                        1.0,
                        footprint,
                        [0.0, 0.0],
                        sheet,
                    ));
                }
            }
            v
        };
        let percentile = |v: &[f32], p: f32| {
            let mut s = v.to_vec();
            s.sort_by(f32::total_cmp);
            s[((s.len() - 1) as f32 * p) as usize]
        };
        let clear = |v: &[f32]| v.iter().filter(|d| **d < 0.02).count() as f32 / v.len() as f32;
        for footprint in [0.002f32, 0.01, 0.04, 0.1] {
            let (a, b) = (grid(&base, footprint), grid(&crisp, footprint));
            let ratio = mean(&b) / mean(&a);
            println!(
                "crisp footprint {footprint}: mean ratio {ratio:.3}, p99 {:.2} -> {:.2}, clear {:.2} -> {:.2}",
                percentile(&a, 0.99),
                percentile(&b, 0.99),
                clear(&a),
                clear(&b)
            );
            assert!((0.85..1.15).contains(&ratio), "mean ratio {ratio} at {footprint}");
            assert!(b.iter().all(|d| (0.0..=CIRRUS_CRISP_CAP).contains(d)));
            assert!(clear(&b) > clear(&a), "gaps clearer at {footprint}");
            if footprint <= 0.01 {
                assert!(
                    percentile(&b, 0.99) > 1.5 * percentile(&a, 0.99),
                    "denser strand peaks at {footprint}"
                );
            }
        }
        for footprint in [0.004f32, 0.01] {
            let roughness = |sheet: &CirrusSheet| {
                let (mut d2, mut count, mut worst) = (0.0f32, 0usize, 0.0f32);
                for line in 0..40 {
                    let x = 0.37 + line as f32 * 6.1;
                    let n = (60.0 / footprint) as usize;
                    let v = fibrous_line(sheet, (x, 0.0), 90.0, footprint, n, footprint, [0.0, 0.0]);
                    for w in v.windows(3) {
                        d2 += (w[2] - 2.0 * w[1] + w[0]).abs();
                        count += 1;
                        worst = worst.max((w[1] - w[0]).abs());
                    }
                }
                (d2 / count as f32, worst)
            };
            let ((ra, _), (rb, worst)) = (roughness(&base), roughness(&crisp));
            println!("crisp footprint {footprint}: mean |d2| {ra:.4} -> {rb:.4}, largest step {worst:.2}");
            assert!(rb > 1.6 * ra, "sharper strands at {footprint}: {rb} vs {ra}");
            assert!(worst < 0.95 * CIRRUS_CRISP_CAP, "step {worst} at {footprint}");
        }
    }

    /// A non-finite coverage, type, density or blend weight must not become NaN in the settings
    /// the march reads. `f32::clamp` leaves NaN unchanged.
    #[test]
    fn audfix_non_finite_cloud_inputs_stay_finite() {
        let mut layer = CloudGenus::FairCumulus.layer();
        layer.coverage = f32::NAN;
        layer.cloud_type = f32::NAN;
        layer.density = f32::INFINITY;
        let mut settings = CloudSettings::default();
        layer.apply(&mut settings);
        assert_eq!(settings.coverage, 0.0);
        assert_eq!(settings.cloud_type, 0.0);
        assert_eq!(settings.density, 0.0);
        assert!(settings.coverage.is_finite() && settings.cloud_type.is_finite());

        let a = CloudGenus::FairCumulus.layer();
        let b = CloudGenus::Cirrus.layer();
        assert_ne!(a.coverage, b.coverage);
        assert_eq!(CloudGenusLayer::lerp(a, b, f32::NAN), a);
        assert_eq!(CloudLook::lerp(a.look, b.look, f32::NAN), a.look);
        assert_eq!(
            CirrusSheet::lerp(a.look.cirrus, b.look.cirrus, f32::NAN),
            a.look.cirrus
        );
    }
}
