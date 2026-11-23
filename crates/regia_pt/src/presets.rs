// SPDX-License-Identifier: MPL-2.0
//! Named quality presets for the frame path tracer (ADR-2433, `book/agents/pt-presets.md`).
//!
//! A preset is a bundle of the knobs that trade bias for speed: samples per pixel, the bounce cap,
//! the Russian-roulette start, an indirect clamp, the caustics mode, firefly suppression, adaptive
//! sampling and a preview denoiser. [`PtPreset::Reference`] is the base tracer: applying it
//! changes nothing, so its output is bit-identical to a trace made without presets. Every other
//! preset is **biased** (the clamp, the caustics cut and the firefly filter remove energy) and its
//! output is a preview: a denoised image carries [`crate::output::DENOISER_KEY`] and
//! [`crate::compare`] refuses it as a reference.
//!
//! Everything here is plain data plus validation, so it needs no GPU; [`crate::render_preset`]
//! runs a preset on a [`crate::PathTracer`].

use crate::PtError;
use crate::tracer::{MAX_PATH_VERTICES, PtSettings};

/// What `metadata.denoiser` starts with on a trace whose light transport was biased by a preset
/// (clamps, caustics cut, firefly filter) but not denoised: [`crate::compare`] refuses it as a
/// reference like a denoised one.
pub const BIASED_MARK: &str = "regia preset (biased)";

/// The most samples per pixel a settings bundle may ask for (larger values are almost always a
/// typo and would hold the GPU for hours).
pub const MAX_SPP: u32 = 1 << 20;

/// How light that reaches a source through a specular chain after a diffuse bounce is treated.
///
/// A unidirectional tracer finds such a path only when a BSDF-sampled ray happens to hit the
/// source, so caustics converge slowly and show up as fireflies at low spp. [`Self::Full`] is the
/// base behavior and needs Final-level spp (or more) to be smooth; light tracing and photon
/// mapping would converge faster and are out of scope (listed in `book/agents/pt-presets.md`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CausticsMode {
    /// Drop these paths (the standard biased noise cut): no caustics, no fireflies from them.
    Suppress,
    /// Keep them, luminance-capped by [`PtQuality::clamp_caustics`].
    Clamped,
    /// Keep them unchanged: the base behavior.
    #[default]
    Full,
}

impl CausticsMode {
    /// Lowercase name (`suppress`, `clamped`, `full`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Suppress => "suppress",
            Self::Clamped => "clamped",
            Self::Full => "full",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "suppress" => Some(Self::Suppress),
            "clamped" => Some(Self::Clamped),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The value the shader reads (`globals.misc.w`): 0 full, 1 clamped, 2 suppress.
    pub(crate) fn shader_code(self) -> f32 {
        match self {
            Self::Full => 0.0,
            Self::Clamped => 1.0,
            Self::Suppress => 2.0,
        }
    }
}

/// The biased light-transport limits of a trace. The default is the base tracer (everything off).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtQuality {
    /// Caustics treatment.
    pub caustics: CausticsMode,
    /// Luminance cap (the scene's radiance units) on every contribution credited after the first
    /// path vertex, keeping its color; 0 = off.
    pub clamp_indirect: f32,
    /// Luminance cap on caustic contributions in [`CausticsMode::Clamped`]; 0 = use
    /// `clamp_indirect` (and no cap when that is 0 too).
    pub clamp_caustics: f32,
}

impl Default for PtQuality {
    fn default() -> Self {
        Self {
            caustics: CausticsMode::Full,
            clamp_indirect: 0.0,
            clamp_caustics: 0.0,
        }
    }
}

impl PtQuality {
    /// True for the base tracer (nothing biased).
    #[must_use]
    pub fn is_base(&self) -> bool {
        *self == Self::default()
    }

    /// The caustic cap the shader uses (`media_f.z`): the explicit one, else the indirect clamp.
    pub(crate) fn caustic_cap(&self) -> f32 {
        if self.clamp_caustics > 0.0 {
            self.clamp_caustics
        } else {
            self.clamp_indirect
        }
    }

    /// Checks that every number is finite and non-negative.
    ///
    /// # Errors
    ///
    /// A clamp is NaN, infinite or negative.
    pub fn validate(&self) -> Result<(), PtError> {
        finite_non_negative("clamp_indirect", self.clamp_indirect)?;
        finite_non_negative("clamp_caustics", self.clamp_caustics)
    }
}

/// Adaptive sampling: the image is cut into horizontal bands of `tile_rows` rows (full width),
/// and a band stops taking samples once its relative standard error is under `threshold`.
///
/// Bands, not squares, because the kernels dispatch runs of 64 consecutive pixels: a band is one
/// contiguous range, so stopping it costs no extra dispatches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdaptiveSettings {
    /// Off: every pixel takes the same number of samples.
    pub enabled: bool,
    /// Rows per band (at least 1).
    pub tile_rows: u32,
    /// Relative standard error of the band's mean luminance below which it stops (> 0).
    pub threshold: f32,
    /// Samples every band takes before it may stop (at least 2: a variance needs two).
    pub min_spp: u32,
    /// Samples per round: the bands are re-judged after each (at least 1).
    pub round_spp: u32,
}

impl Default for AdaptiveSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            tile_rows: 16,
            threshold: 0.05,
            min_spp: 8,
            round_spp: 4,
        }
    }
}

impl AdaptiveSettings {
    /// Checks the numbers.
    ///
    /// # Errors
    ///
    /// A zero count, or a threshold that is NaN, infinite or not positive.
    pub fn validate(&self) -> Result<(), PtError> {
        if self.tile_rows == 0 {
            return Err(bad("adaptive tile_rows must be at least 1"));
        }
        if self.min_spp < 2 {
            return Err(bad("adaptive min_spp must be at least 2"));
        }
        if self.round_spp == 0 {
            return Err(bad("adaptive round_spp must be at least 1"));
        }
        if !(self.threshold.is_finite() && self.threshold > 0.0) {
            return Err(bad(format!(
                "adaptive threshold must be finite and above 0, got {}",
                self.threshold
            )));
        }
        Ok(())
    }
}

/// Which denoiser filters the preview's beauty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DenoiserKind {
    /// None: the raw trace.
    #[default]
    None,
    /// The built-in edge-avoiding a-trous filter ([`crate::denoise_atrous`]): no native library.
    BuiltIn,
    /// Open Image Denoise (the `denoise` feature; absent in CI and on Windows/Android builds).
    Oidn,
}

impl DenoiserKind {
    /// Lowercase name (`none`, `builtin`, `oidn`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BuiltIn => "builtin",
            Self::Oidn => "oidn",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "builtin" => Some(Self::BuiltIn),
            "oidn" => Some(Self::Oidn),
            _ => None,
        }
    }
}

/// A named quality level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PtPreset {
    /// Seconds-scale look iteration: very few samples, short paths, hard clamps, caustics cut,
    /// firefly filter, the built-in denoiser.
    Draft,
    /// A readable preview: tens of samples, soft clamps, caustics clamped, the built-in denoiser.
    Preview,
    /// A production-quality trace without a denoiser: base light transport (caustics full, no
    /// clamp) at the base sample count, with adaptive sampling.
    Final,
    /// The base tracer, unchanged: the caller's settings, bit for bit.
    Reference,
}

impl PtPreset {
    /// Every preset, cheapest first.
    pub const ALL: [Self; 4] = [Self::Draft, Self::Preview, Self::Final, Self::Reference];

    /// Lowercase name (`draft`, `preview`, `final`, `reference`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Preview => "preview",
            Self::Final => "final",
            Self::Reference => "reference",
        }
    }

    /// Parses [`Self::name`] (case-insensitive).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "draft" => Some(Self::Draft),
            "preview" => Some(Self::Preview),
            "final" => Some(Self::Final),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }

    /// The preset's settings table.
    ///
    /// | preset | spp | bounces | rr start | indirect clamp | caustics | firefly | adaptive | denoiser |
    /// |---|---|---|---|---|---|---|---|---|
    /// | Draft | 4 | 3 | 2 | 1e4 | suppress | 8x median | yes, 12 % | built-in |
    /// | Preview | 32 | 6 | 3 | 5e4 | clamped (1e5) | 16x median | yes, 5 % | built-in |
    /// | Final | 256 | 12 | 3 | off | full | off | yes, 1 % | none |
    /// | Reference | 256 | 12 | 3 | off | full | off | no | none |
    ///
    /// Clamps are luminance caps in the scene's radiance units (cd/m²); they suit sun-lit
    /// outdoor scenes and should be scaled down for dim interiors through the overrides.
    #[must_use]
    pub fn settings(self) -> PresetSettings {
        match self {
            Self::Draft => PresetSettings {
                preset: self,
                spp: 4,
                max_bounces: 3,
                rr_start: 2,
                quality: PtQuality {
                    caustics: CausticsMode::Suppress,
                    clamp_indirect: 1.0e4,
                    clamp_caustics: 0.0,
                },
                firefly_threshold: 8.0,
                adaptive: AdaptiveSettings {
                    enabled: true,
                    threshold: 0.12,
                    min_spp: 2,
                    round_spp: 1,
                    ..AdaptiveSettings::default()
                },
                denoiser: DenoiserKind::BuiltIn,
            },
            Self::Preview => PresetSettings {
                preset: self,
                spp: 32,
                max_bounces: 6,
                rr_start: 3,
                quality: PtQuality {
                    caustics: CausticsMode::Clamped,
                    clamp_indirect: 5.0e4,
                    clamp_caustics: 1.0e5,
                },
                firefly_threshold: 16.0,
                adaptive: AdaptiveSettings {
                    enabled: true,
                    threshold: 0.05,
                    min_spp: 8,
                    round_spp: 4,
                    ..AdaptiveSettings::default()
                },
                denoiser: DenoiserKind::BuiltIn,
            },
            Self::Final => PresetSettings {
                preset: self,
                spp: 256,
                max_bounces: 12,
                rr_start: 3,
                quality: PtQuality::default(),
                firefly_threshold: 0.0,
                adaptive: AdaptiveSettings {
                    enabled: true,
                    threshold: 0.01,
                    min_spp: 64,
                    round_spp: 16,
                    ..AdaptiveSettings::default()
                },
                denoiser: DenoiserKind::None,
            },
            Self::Reference => {
                let base = PtSettings::default();
                PresetSettings {
                    preset: self,
                    spp: base.spp,
                    max_bounces: base.max_bounces,
                    rr_start: base.rr_start,
                    quality: PtQuality::default(),
                    firefly_threshold: 0.0,
                    adaptive: AdaptiveSettings::default(),
                    denoiser: DenoiserKind::None,
                }
            }
        }
    }

    /// `base` with this preset applied. [`Self::Reference`] returns `base` untouched (including its
    /// seed, jitter and submit size); the others replace `spp`, `max_bounces` and `rr_start` and
    /// keep the rest.
    #[must_use]
    pub fn apply(self, base: &PtSettings) -> PtSettings {
        if self == Self::Reference {
            return *base;
        }
        let s = self.settings();
        PtSettings {
            spp: s.spp,
            max_bounces: s.max_bounces,
            rr_start: s.rr_start,
            ..*base
        }
    }
}

/// The knobs of one preset (also the unit callers override).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PresetSettings {
    /// The preset these came from.
    pub preset: PtPreset,
    /// Samples per pixel (the cap with adaptive sampling).
    pub spp: u32,
    /// Maximum path vertices.
    pub max_bounces: u32,
    /// Russian-roulette start vertex.
    pub rr_start: u32,
    /// Biased light-transport limits.
    pub quality: PtQuality,
    /// A pixel brighter than this multiple of its 3x3 neighbors' median luminance is pulled down
    /// to it (0 = off; otherwise above 1).
    pub firefly_threshold: f32,
    /// Adaptive sampling.
    pub adaptive: AdaptiveSettings,
    /// Preview denoiser.
    pub denoiser: DenoiserKind,
}

impl PresetSettings {
    /// Checks every number: counts in range, thresholds finite.
    ///
    /// # Errors
    ///
    /// Zero or more than [`MAX_SPP`] samples, a bounce cap of 0 or above
    /// [`MAX_PATH_VERTICES`], a zero Russian-roulette start, a clamp or threshold that is NaN,
    /// infinite or negative, or a firefly threshold in `(0, 1]`.
    pub fn validate(&self) -> Result<(), PtError> {
        if self.spp == 0 || self.spp > MAX_SPP {
            return Err(bad(format!("spp must be 1..={MAX_SPP}, got {}", self.spp)));
        }
        if self.max_bounces == 0 || self.max_bounces > MAX_PATH_VERTICES {
            return Err(bad(format!(
                "max_bounces must be 1..={MAX_PATH_VERTICES}, got {}",
                self.max_bounces
            )));
        }
        if self.rr_start == 0 {
            return Err(bad("rr_start must be at least 1"));
        }
        self.quality.validate()?;
        finite_non_negative("firefly_threshold", self.firefly_threshold)?;
        if self.firefly_threshold > 0.0 && self.firefly_threshold <= 1.0 {
            return Err(bad(format!(
                "firefly_threshold must be 0 (off) or above 1, got {}",
                self.firefly_threshold
            )));
        }
        self.adaptive.validate()?;
        if self.adaptive.enabled && self.adaptive.min_spp > self.spp {
            return Err(bad(format!(
                "adaptive min_spp {} exceeds spp {}",
                self.adaptive.min_spp, self.spp
            )));
        }
        Ok(())
    }

    /// The tracer settings these numbers describe on top of `base` (seed, jitter and submit size
    /// stay `base`'s).
    #[must_use]
    pub fn trace_settings(&self, base: &PtSettings) -> PtSettings {
        PtSettings {
            spp: self.spp,
            max_bounces: self.max_bounces,
            rr_start: self.rr_start,
            ..*base
        }
    }

    /// Whether the settings are the base tracer's (no bias, no adaptive sampling, no denoiser,
    /// no firefly filter).
    #[must_use]
    pub fn is_base(&self) -> bool {
        self.quality.is_base()
            && self.firefly_threshold == 0.0
            && !self.adaptive.enabled
            && self.denoiser == DenoiserKind::None
    }
}

pub(crate) fn bad(msg: impl Into<String>) -> PtError {
    PtError::Scene(format!("preset: {}", msg.into()))
}

pub(crate) fn finite_non_negative(name: &str, v: f32) -> Result<(), PtError> {
    if v.is_finite() && v >= 0.0 {
        Ok(())
    } else {
        Err(bad(format!("{name} must be finite and not negative, got {v}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_monotone_in_cost() {
        let table: Vec<_> = [PtPreset::Draft, PtPreset::Preview, PtPreset::Final]
            .iter()
            .map(|p| p.settings())
            .collect();
        for pair in table.windows(2) {
            assert!(pair[0].spp <= pair[1].spp, "{pair:?}");
            assert!(pair[0].max_bounces <= pair[1].max_bounces, "{pair:?}");
            assert!(pair[0].rr_start <= pair[1].rr_start, "{pair:?}");
        }
        // Bias also falls as cost rises: the clamps relax and the firefly filter loosens.
        assert!(table[0].firefly_threshold <= table[1].firefly_threshold);
        assert_eq!(table[2].firefly_threshold, 0.0);
    }

    #[test]
    fn every_preset_validates_and_names_round_trip() {
        for p in PtPreset::ALL {
            p.settings().validate().expect("preset table is valid");
            assert_eq!(PtPreset::from_name(p.name()), Some(p));
        }
        assert_eq!(PtPreset::from_name("DRAFT"), Some(PtPreset::Draft));
        assert_eq!(PtPreset::from_name("nope"), None);
        for m in [CausticsMode::Suppress, CausticsMode::Clamped, CausticsMode::Full] {
            assert_eq!(CausticsMode::from_name(m.name()), Some(m));
        }
        for d in [DenoiserKind::None, DenoiserKind::BuiltIn, DenoiserKind::Oidn] {
            assert_eq!(DenoiserKind::from_name(d.name()), Some(d));
        }
    }

    #[test]
    fn reference_changes_nothing() {
        let base = PtSettings {
            spp: 7,
            max_bounces: 5,
            seed: 99,
            rr_start: 2,
            jitter: false,
            samples_per_submit: 3,
        };
        assert_eq!(PtPreset::Reference.apply(&base), base);
        assert!(PtPreset::Reference.settings().is_base());
        assert_eq!(
            PtPreset::Reference
                .settings()
                .trace_settings(&PtSettings::default()),
            PtSettings::default()
        );
        // The others keep seed, jitter and submit size.
        let draft = PtPreset::Draft.apply(&base);
        assert_eq!(
            (draft.seed, draft.jitter, draft.samples_per_submit),
            (99, false, 3)
        );
        assert_eq!(draft.spp, 4);
        assert!(!PtPreset::Draft.settings().is_base());
    }

    #[test]
    fn hostile_settings_are_errors_not_panics() {
        let ok = PtPreset::Preview.settings();
        let mut s = ok;
        s.spp = 0;
        assert!(s.validate().is_err());
        s.spp = u32::MAX;
        assert!(s.validate().is_err());
        s = ok;
        s.max_bounces = 0;
        assert!(s.validate().is_err());
        s.max_bounces = u32::MAX;
        assert!(s.validate().is_err());
        s = ok;
        s.rr_start = 0;
        assert!(s.validate().is_err());
        for bad_value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
            s = ok;
            s.quality.clamp_indirect = bad_value;
            assert!(s.validate().is_err(), "clamp_indirect {bad_value}");
            s = ok;
            s.quality.clamp_caustics = bad_value;
            assert!(s.validate().is_err(), "clamp_caustics {bad_value}");
            s = ok;
            s.firefly_threshold = bad_value;
            assert!(s.validate().is_err(), "firefly {bad_value}");
            s = ok;
            s.adaptive.threshold = bad_value;
            assert!(s.validate().is_err(), "adaptive threshold {bad_value}");
        }
        s = ok;
        s.firefly_threshold = 0.5;
        assert!(s.validate().is_err());
        s = ok;
        s.adaptive.tile_rows = 0;
        assert!(s.validate().is_err());
        s.adaptive.tile_rows = 8;
        s.adaptive.min_spp = 1;
        assert!(s.validate().is_err());
        s.adaptive.min_spp = 64;
        assert!(s.validate().is_err(), "min_spp above spp");
        s = ok;
        s.adaptive.round_spp = 0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn caustic_cap_falls_back_to_the_indirect_clamp() {
        let q = PtQuality {
            clamp_indirect: 10.0,
            ..PtQuality::default()
        };
        assert_eq!(q.caustic_cap(), 10.0);
        let q = PtQuality {
            clamp_caustics: 3.0,
            ..q
        };
        assert_eq!(q.caustic_cap(), 3.0);
    }
}
