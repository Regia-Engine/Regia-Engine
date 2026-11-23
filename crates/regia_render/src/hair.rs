// SPDX-License-Identifier: MPL-2.0
//! Hair card shading and alpha strategy (Tincal engine request B5, `book/agents/hair-cards.md`).
//!
//! A hair material feature on [`crate::LabMaterial`]: [`HairFactors`] switch the forward shader
//! (`lab_forward.wgsl`) to a strand model for cards — flat strips of hair, eyebrows, lashes, fur
//! or foliage whose mesh tangent runs along the strand, root to tip:
//!
//! - **Specular**: two Kajiya–Kay lobes with Scheuermann's tangent shifts. The primary lobe takes
//!   the light's colour; the secondary lobe is shifted the other way, is broader, and takes the
//!   hair colour, which gives the two-tone highlight of real hair. A per-strand shift jitter
//!   breaks the highlight into streaks.
//! - **Transmission**: light through the strands, as a wrapped diffuse term.
//! - **Root-to-tip ramp**: a colour and alpha ramp along the card's `v` coordinate.
//! - **Alpha**: [`HairAlpha`] picks the strategy. `Hashed` (the default) alpha-tests with a
//!   per-pixel hashed threshold that TAA/TSR resolves, in the depth prepass and the colour pass
//!   alike. `Sorted` draws the card through the sorted alpha-blend pass (back to front).
//!   The order-independent pass excludes every hair card (`takes_oit` requires `!draw.hair`).
//!   `Coverage` asks for alpha-to-coverage and resolves to `Hashed` on this build
//!   (`docs/decisions.md` ADR-245).
//! - **Shadows**: a hair card casts a shadow through its alpha test
//!   (`LabMaterial::alpha_cutoff`), not the card's square outline.
//!
//! The feature is a branch of the one forward shader: it adds no pipeline, so the warmed
//! pipelines cover it. A material with [`HairFactors::OFF`] shades exactly as before. The CPU
//! functions below are the reference the shader mirrors; they are what the unit tests check.
//!
//! Sources are in `docs/provenance.md`: Kajiya & Kay 1989, Scheuermann 2004, Marschner et al.
//! 2003 (the lobe layout), Wyman & McGuire 2017 (hashed alpha), Jimenez 2014 (interleaved
//! gradient noise).

use crate::lab_frame::{ALPHA_BLEND, ALPHA_MASK, LabMaterial};

/// How a hair card resolves its alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HairAlpha {
    /// Alpha test against a hashed per-pixel threshold on TSR/TAA frames (the cutoff elsewhere),
    /// in the depth prepass and the colour pass. Writes depth, so cards need no sorting.
    #[default]
    Hashed,
    /// The sorted alpha-blend pass: smooth tips, drawn back to front, no depth writes. Crossing
    /// cards still need a draw order. The order-independent pass does not take hair cards.
    Sorted,
    /// Alpha-to-coverage on multisampled frames. This build has no coverage pipeline, so it
    /// shades as [`HairAlpha::Hashed`].
    Coverage,
}

impl HairAlpha {
    /// The shader's strategy code (`hair_b.w`, 1-based; 0 is "hair off").
    #[must_use]
    pub const fn code(self) -> f32 {
        match self {
            Self::Hashed => 1.0,
            Self::Sorted => 2.0,
            Self::Coverage => 3.0,
        }
    }

    /// The strategy that actually draws: [`Self::Coverage`] resolves to [`Self::Hashed`].
    #[must_use]
    pub const fn resolved(self) -> Self {
        match self {
            Self::Coverage => Self::Hashed,
            other => other,
        }
    }

    /// Strategy name as the agent protocol spells it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hashed => "hashed",
            Self::Sorted => "sorted",
            Self::Coverage => "coverage",
        }
    }

    /// Parse [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "hashed" => Some(Self::Hashed),
            "sorted" => Some(Self::Sorted),
            "coverage" => Some(Self::Coverage),
            _ => None,
        }
    }
}

/// Hair card shading parameters. [`HairFactors::OFF`] (the default) leaves a material alone.
///
/// # Example
///
/// ```
/// use regia_render::{HairFactors, LabMaterial};
///
/// let mut material = LabMaterial::EMPTY;
/// assert!(!material.hair.enabled);
/// material.hair = HairFactors::strands();
/// material.hair.apply_alpha_mode(&mut material.alpha_mode, &mut material.alpha_cutoff);
/// assert!(material.hair.enabled);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HairFactors {
    /// Shade this material as hair. `false` skips every term below.
    pub enabled: bool,
    /// Primary lobe tangent shift along the normal (Scheuermann). Negative pushes the highlight
    /// toward the root.
    pub primary_shift: f32,
    /// Secondary lobe tangent shift (opposite sign of the primary, larger).
    pub secondary_shift: f32,
    /// Primary lobe exponent (higher is a tighter, brighter line).
    pub primary_exponent: f32,
    /// Secondary lobe exponent (lower: broader and softer).
    pub secondary_exponent: f32,
    /// Primary lobe strength, scaling the light colour.
    pub primary_strength: f32,
    /// Secondary lobe strength, scaling the hair colour.
    pub secondary_strength: f32,
    /// Light through the strands, 0..1: widens the diffuse wrap so a card lit from behind
    /// glows instead of going black.
    pub transmission: f32,
    /// Per-strand shift jitter amplitude, 0..1, applied to both lobes. 0 draws a smooth sheet.
    pub shift_noise: f32,
    /// Strands across the card's `u`: the shift jitter's cell count.
    pub streak_density: f32,
    /// Colour multiplier at the root (linear RGB, 1 leaves the base colour).
    pub root_color: [f32; 3],
    /// Colour multiplier at the tip.
    pub tip_color: [f32; 3],
    /// Alpha multiplier at the root.
    pub root_alpha: f32,
    /// Alpha multiplier at the tip.
    pub tip_alpha: f32,
    /// Card `v` where the root ramp value applies. Equal to [`Self::ramp_end`] disables the ramp.
    pub ramp_start: f32,
    /// Card `v` where the tip ramp value applies. Start above end flips the ramp.
    pub ramp_end: f32,
    /// How alpha resolves.
    pub alpha: HairAlpha,
}

impl HairFactors {
    /// Hair off: the material shades as it did before this feature.
    pub const OFF: Self = Self {
        enabled: false,
        primary_shift: -0.1,
        secondary_shift: 0.15,
        primary_exponent: 120.0,
        secondary_exponent: 24.0,
        primary_strength: 0.35,
        secondary_strength: 0.25,
        transmission: 0.0,
        shift_noise: 0.0,
        streak_density: 48.0,
        root_color: [1.0; 3],
        tip_color: [1.0; 3],
        root_alpha: 1.0,
        tip_alpha: 1.0,
        ramp_start: 0.0,
        ramp_end: 1.0,
        alpha: HairAlpha::Hashed,
    };

    /// Hair on with usable defaults: two lobes, mild streaking and transmission, a flat ramp,
    /// hashed alpha.
    #[must_use]
    pub const fn strands() -> Self {
        Self {
            enabled: true,
            transmission: 0.35,
            shift_noise: 0.35,
            ..Self::OFF
        }
    }

    /// Sets the draw's alpha mode for the chosen strategy: `Sorted` blends, the others test
    /// against `cutoff` (`0.5` when it was a non-mask material). Hosts call this after choosing
    /// the strategy; the renderer reads the mode from the material.
    pub fn apply_alpha_mode(&self, alpha_mode: &mut u32, alpha_cutoff: &mut f32) {
        if !self.enabled {
            return;
        }
        match self.alpha.resolved() {
            HairAlpha::Sorted => *alpha_mode = ALPHA_BLEND,
            _ => {
                if *alpha_mode != ALPHA_MASK {
                    *alpha_cutoff = 0.5;
                }
                *alpha_mode = ALPHA_MASK;
            }
        }
    }

    /// Clamp every field to the range the shader expects. Non-finite values fall back to the
    /// [`Self::OFF`] default.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let d = Self::OFF;
        let f = |v: f32, lo: f32, hi: f32, fallback: f32| {
            if v.is_finite() { v.clamp(lo, hi) } else { fallback }
        };
        let c = |v: [f32; 3], fallback: [f32; 3]| {
            [
                f(v[0], 0.0, 4.0, fallback[0]),
                f(v[1], 0.0, 4.0, fallback[1]),
                f(v[2], 0.0, 4.0, fallback[2]),
            ]
        };
        Self {
            enabled: self.enabled,
            primary_shift: f(self.primary_shift, -1.0, 1.0, d.primary_shift),
            secondary_shift: f(self.secondary_shift, -1.0, 1.0, d.secondary_shift),
            primary_exponent: f(self.primary_exponent, 1.0, 2048.0, d.primary_exponent),
            secondary_exponent: f(self.secondary_exponent, 1.0, 2048.0, d.secondary_exponent),
            primary_strength: f(self.primary_strength, 0.0, 8.0, d.primary_strength),
            secondary_strength: f(self.secondary_strength, 0.0, 8.0, d.secondary_strength),
            transmission: f(self.transmission, 0.0, 1.0, 0.0),
            shift_noise: f(self.shift_noise, 0.0, 1.0, 0.0),
            streak_density: f(self.streak_density, 1.0, 1024.0, d.streak_density),
            root_color: c(self.root_color, d.root_color),
            tip_color: c(self.tip_color, d.tip_color),
            root_alpha: f(self.root_alpha, 0.0, 1.0, 1.0),
            tip_alpha: f(self.tip_alpha, 0.0, 1.0, 1.0),
            ramp_start: f(self.ramp_start, -8.0, 8.0, 0.0),
            ramp_end: f(self.ramp_end, -8.0, 8.0, 1.0),
            alpha: self.alpha,
        }
    }
}

impl Default for HairFactors {
    fn default() -> Self {
        Self::OFF
    }
}

/// The five `vec4`s the material uniform carries for hair (`hair_a`..`hair_e` in
/// `lab_forward.wgsl`). All zero when hair is off.
pub(crate) type HairUniform = [[f32; 4]; 5];

/// Pack [`HairFactors`] for the material uniform. An off material packs all zeros, so the
/// uniform of a plain material is byte-identical to before the feature.
pub(crate) fn pack_hair(h: &HairFactors) -> HairUniform {
    if !h.enabled {
        return [[0.0; 4]; 5];
    }
    let h = h.sanitized();
    [
        [
            h.primary_shift,
            h.secondary_shift,
            h.primary_exponent,
            h.secondary_exponent,
        ],
        [
            h.primary_strength,
            h.secondary_strength,
            h.transmission,
            h.alpha.code(),
        ],
        [h.tip_color[0], h.tip_color[1], h.tip_color[2], h.tip_alpha],
        [h.ramp_start, h.ramp_end, h.root_alpha, h.shift_noise],
        [
            h.root_color[0],
            h.root_color[1],
            h.root_color[2],
            h.streak_density,
        ],
    ]
}

/// Root-to-tip ramp parameter at card coordinate `v`: 0 at [`HairFactors::ramp_start`], 1 at
/// [`HairFactors::ramp_end`], clamped. A flat ramp is 0.
#[must_use]
pub fn ramp_t(h: &HairFactors, v: f32) -> f32 {
    let span = h.ramp_end - h.ramp_start;
    if span.abs() < 1.0e-6 {
        return 0.0;
    }
    ((v - h.ramp_start) / span).clamp(0.0, 1.0)
}

/// Colour multiplier at card coordinate `v` (white when hair is off).
#[must_use]
pub fn ramp_color(h: &HairFactors, v: f32) -> [f32; 3] {
    if !h.enabled {
        return [1.0; 3];
    }
    let t = ramp_t(h, v);
    [0, 1, 2].map(|i| h.root_color[i] + (h.tip_color[i] - h.root_color[i]) * t)
}

/// Alpha multiplier at card coordinate `v` (1 when hair is off).
#[must_use]
pub fn ramp_alpha(h: &HairFactors, v: f32) -> f32 {
    if !h.enabled {
        return 1.0;
    }
    let t = ramp_t(h, v);
    h.root_alpha + (h.tip_alpha - h.root_alpha) * t
}

/// Hash of a strand cell to 0..1 (the shader's `hair_hash`).
#[must_use]
pub fn strand_hash(cell: f32) -> f32 {
    let x = (cell * 12.9898 + 78.233).sin() * 43_758.545;
    x - x.floor()
}

/// Shift jitter of the strand under card coordinate `u`, in −`noise`/2..`noise`/2 around zero.
#[must_use]
pub fn strand_shift(h: &HairFactors, u: f32) -> f32 {
    (strand_hash((u * h.streak_density).floor()) - 0.5) * h.shift_noise
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = dot(v, v).sqrt().max(1.0e-12);
    [v[0] / l, v[1] / l, v[2] / l]
}

/// Scheuermann's shifted tangent, `normalize(T + shift · N)`.
#[must_use]
pub fn shifted_tangent(tangent: [f32; 3], normal: [f32; 3], shift: f32) -> [f32; 3] {
    normalize([
        tangent[0] + normal[0] * shift,
        tangent[1] + normal[1] * shift,
        tangent[2] + normal[2] * shift,
    ])
}

/// One Kajiya–Kay lobe: `(e + 2)/(2π) · sin^e(T, H)`, faded to 0 where the half vector is behind
/// the strand's mirror direction (`smoothstep(−1, 0, T·H)`, Scheuermann's attenuation). `t` and
/// `h` are unit vectors.
#[must_use]
pub fn kajiya_kay(t: [f32; 3], h: [f32; 3], exponent: f32) -> f32 {
    let th = dot(t, h).clamp(-1.0, 1.0);
    let sin_th = (1.0 - th * th).max(0.0).sqrt();
    let x = (th + 1.0).clamp(0.0, 1.0);
    let atten = x * x * (3.0 - 2.0 * x);
    atten * (exponent + 2.0) / (2.0 * std::f32::consts::PI) * sin_th.powf(exponent)
}

/// The two-lobe hair specular for tangent `t`, normal `n`, view `v` and light `l` (unit
/// vectors, pointing away from the surface). Returns `(primary, secondary)` already weighted by
/// the strengths; the primary takes the light colour and the secondary the hair colour.
#[must_use]
pub fn hair_specular(
    h: &HairFactors,
    t: [f32; 3],
    n: [f32; 3],
    v: [f32; 3],
    l: [f32; 3],
    strand_shift: f32,
) -> (f32, f32) {
    let half = normalize([l[0] + v[0], l[1] + v[1], l[2] + v[2]]);
    let t1 = shifted_tangent(t, n, h.primary_shift + strand_shift);
    let t2 = shifted_tangent(t, n, h.secondary_shift + strand_shift);
    (
        h.primary_strength * kajiya_kay(t1, half, h.primary_exponent),
        h.secondary_strength * kajiya_kay(t2, half, h.secondary_exponent),
    )
}

/// The wrapped diffuse factor `saturate((n·l + w)/(1 + w))` with `w` the transmission, which
/// replaces `max(n·l, 0)` for hair. Transmission 0 is plain Lambert.
#[must_use]
pub fn wrapped_n_dot_l(n_dot_l: f32, transmission: f32) -> f32 {
    ((n_dot_l + transmission) / (1.0 + transmission)).clamp(0.0, 1.0)
}

/// Interleaved gradient noise (Jimenez 2014) advanced by the golden ratio per frame: the
/// threshold the hashed alpha test uses on TSR frames (the shader's `alpha_rejects`).
#[must_use]
pub fn hashed_threshold(frag: [f32; 2], frame: f32) -> f32 {
    let ign = (52.982_918 * ((frag[0] * 0.067_110_56 + frag[1] * 0.005_837_15).fract())).fract();
    (ign + 0.618_034 * frame).fract()
}

/// The alpha mode the renderer draws a material with: `Sorted` hair is blended and `Hashed`
/// hair is at least alpha-tested, whatever mode the host wrote, so a host that forgot
/// [`HairFactors::apply_alpha_mode`] still draws hair as its strategy says. Any other material
/// keeps its own mode.
#[must_use]
pub fn effective_alpha_mode(material: &LabMaterial) -> u32 {
    if !material.hair.enabled {
        return material.alpha_mode;
    }
    match material.hair.alpha.resolved() {
        HairAlpha::Sorted => material.alpha_mode.max(ALPHA_BLEND),
        _ if material.alpha_mode == crate::lab_frame::ALPHA_OPAQUE => ALPHA_MASK,
        _ => material.alpha_mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_packs_zeros_and_is_identity() {
        let off = HairFactors::OFF;
        assert_eq!(pack_hair(&off), [[0.0; 4]; 5]);
        assert_eq!(ramp_color(&off, 0.7), [1.0; 3]);
        assert_eq!(ramp_alpha(&off, 0.7), 1.0);
        assert_eq!(HairFactors::default(), HairFactors::OFF);
    }

    #[test]
    fn packing_carries_every_field() {
        let h = HairFactors {
            tip_color: [0.5, 0.25, 0.125],
            tip_alpha: 0.2,
            root_color: [0.9, 0.8, 0.7],
            alpha: HairAlpha::Sorted,
            ..HairFactors::strands()
        };
        let u = pack_hair(&h);
        assert_eq!(u[0], [-0.1, 0.15, 120.0, 24.0]);
        assert_eq!(u[1][3], 2.0);
        assert_eq!(u[2], [0.5, 0.25, 0.125, 0.2]);
        assert_eq!(u[3][..2], [0.0, 1.0]);
        assert_eq!(u[4][..3], [0.9, 0.8, 0.7]);
    }

    #[test]
    fn ramp_runs_root_to_tip_and_flips() {
        let h = HairFactors {
            tip_color: [0.0; 3],
            tip_alpha: 0.0,
            ..HairFactors::strands()
        };
        assert_eq!(ramp_color(&h, 0.0), [1.0; 3]);
        assert_eq!(ramp_color(&h, 1.0), [0.0; 3]);
        assert!((ramp_alpha(&h, 0.25) - 0.75).abs() < 1.0e-6);
        let flipped = HairFactors {
            ramp_start: 1.0,
            ramp_end: 0.0,
            ..h
        };
        assert_eq!(ramp_alpha(&flipped, 1.0), 1.0);
        assert_eq!(ramp_alpha(&flipped, 0.0), 0.0);
        let flat = HairFactors {
            ramp_end: h.ramp_start,
            ..h
        };
        assert_eq!(ramp_alpha(&flat, 0.9), 1.0);
    }

    #[test]
    fn highlight_peaks_where_the_half_vector_is_perpendicular_to_the_strand() {
        let h = HairFactors::strands();
        let t = [1.0, 0.0, 0.0];
        let n = [0.0, 0.0, 1.0];
        // Half vector along the normal: perpendicular to T, sin = 1.
        let peak = kajiya_kay(t, n, 120.0);
        let tilted = kajiya_kay(t, normalize([0.4, 0.0, 1.0]), 120.0);
        let along = kajiya_kay(t, t, 120.0);
        assert!(peak > tilted && tilted > along);
        assert_eq!(along, 0.0);
        // Shifting the tangent moves the highlight: the shifted lobe peaks off the normal.
        let (p0, _) = hair_specular(
            &HairFactors {
                primary_shift: 0.0,
                ..h
            },
            t,
            n,
            n,
            n,
            0.0,
        );
        let (p1, _) = hair_specular(
            &HairFactors {
                primary_shift: 0.3,
                ..h
            },
            t,
            n,
            n,
            n,
            0.0,
        );
        assert!(p0 > p1);
    }

    #[test]
    fn the_secondary_lobe_is_broader() {
        let t = [1.0, 0.0, 0.0];
        let off_peak = normalize([0.5, 0.0, 1.0]);
        let tight = kajiya_kay(t, off_peak, 120.0) / kajiya_kay(t, [0.0, 0.0, 1.0], 120.0);
        let broad = kajiya_kay(t, off_peak, 24.0) / kajiya_kay(t, [0.0, 0.0, 1.0], 24.0);
        assert!(broad > tight * 4.0);
    }

    #[test]
    fn wrap_is_lambert_without_transmission_and_lights_the_back_with_it() {
        assert_eq!(wrapped_n_dot_l(-0.2, 0.0), 0.0);
        assert!((wrapped_n_dot_l(0.6, 0.0) - 0.6).abs() < 1.0e-6);
        assert!(wrapped_n_dot_l(-0.2, 0.5) > 0.0);
        assert_eq!(wrapped_n_dot_l(1.0, 0.7), 1.0);
    }

    #[test]
    fn strand_jitter_is_per_strand_and_bounded() {
        let h = HairFactors {
            shift_noise: 0.4,
            streak_density: 10.0,
            ..HairFactors::strands()
        };
        let a = strand_shift(&h, 0.01);
        assert_eq!(a, strand_shift(&h, 0.09), "one strand across its width");
        assert_ne!(a, strand_shift(&h, 0.15));
        for i in 0..200 {
            assert!(strand_shift(&h, i as f32 / 200.0).abs() <= 0.2 + 1.0e-6);
        }
        assert_eq!(
            strand_shift(
                &HairFactors {
                    shift_noise: 0.0,
                    ..h
                },
                0.3
            ),
            0.0
        );
    }

    #[test]
    fn hashed_threshold_covers_the_unit_interval() {
        let mut passed = 0;
        let n = 4096;
        for i in 0..n {
            let t = hashed_threshold([(i % 64) as f32, (i / 64) as f32], 3.0);
            assert!((0.0..1.0).contains(&t));
            if t < 0.3 {
                passed += 1;
            }
        }
        let share = passed as f32 / n as f32;
        assert!(
            (share - 0.3).abs() < 0.08,
            "a 0.3 alpha keeps about 30 % of pixels, got {share}"
        );
    }

    #[test]
    fn alpha_mode_follows_the_strategy() {
        let mut mode = crate::ALPHA_OPAQUE;
        let mut cutoff = 0.1;
        HairFactors::strands().apply_alpha_mode(&mut mode, &mut cutoff);
        assert_eq!((mode, cutoff), (ALPHA_MASK, 0.5));
        let mut cutoff = 0.3;
        HairFactors::strands().apply_alpha_mode(&mut mode, &mut cutoff);
        assert_eq!(cutoff, 0.3, "an authored mask cutoff is kept");
        let sorted = HairFactors {
            alpha: HairAlpha::Sorted,
            ..HairFactors::strands()
        };
        sorted.apply_alpha_mode(&mut mode, &mut cutoff);
        assert_eq!(mode, ALPHA_BLEND);
        let mut untouched = crate::ALPHA_OPAQUE;
        HairFactors::OFF.apply_alpha_mode(&mut untouched, &mut cutoff);
        assert_eq!(untouched, crate::ALPHA_OPAQUE);
        assert_eq!(HairAlpha::Coverage.resolved(), HairAlpha::Hashed);
    }

    #[test]
    fn effective_mode_blends_sorted_hair_only() {
        let mut m = LabMaterial::EMPTY;
        m.hair = HairFactors {
            alpha: HairAlpha::Sorted,
            ..HairFactors::strands()
        };
        assert_eq!(effective_alpha_mode(&m), ALPHA_BLEND);
        m.hair.alpha = HairAlpha::Hashed;
        assert_eq!(
            effective_alpha_mode(&m),
            ALPHA_MASK,
            "hashed hair is alpha tested"
        );
        m.alpha_mode = ALPHA_BLEND;
        assert_eq!(effective_alpha_mode(&m), ALPHA_BLEND);
        m.alpha_mode = crate::lab_frame::ALPHA_OPAQUE;
        m.hair.enabled = false;
        m.hair.alpha = HairAlpha::Sorted;
        assert_eq!(effective_alpha_mode(&m), m.alpha_mode);
    }

    #[test]
    fn sanitize_clamps_and_replaces_non_finite() {
        let h = HairFactors {
            primary_exponent: f32::NAN,
            transmission: 9.0,
            tip_alpha: -1.0,
            ..HairFactors::strands()
        }
        .sanitized();
        assert_eq!(h.primary_exponent, HairFactors::OFF.primary_exponent);
        assert_eq!(h.transmission, 1.0);
        assert_eq!(h.tip_alpha, 0.0);
    }

    #[test]
    fn alpha_names_round_trip() {
        for a in [HairAlpha::Hashed, HairAlpha::Sorted, HairAlpha::Coverage] {
            assert_eq!(HairAlpha::from_name(a.name()), Some(a));
        }
        assert_eq!(HairAlpha::from_name("dither"), None);
    }
}
