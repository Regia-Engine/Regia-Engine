// SPDX-License-Identifier: MPL-2.0
//! The planet's air as an acoustic medium (`docs/specs/16-audio.md` § 3c "Propagation media").
//!
//! An [`AudioMedium`] says whether sound travels through air on this level at all, how fast, how
//! loud it arrives for a given source and how quickly the high frequencies die with range. It is
//! derived from the level's atmosphere preset ([`regia_scene::level::AtmospherePreset`]) with an
//! optional per-level override ([`regia_scene::level::LevelAudioMedium`]).
//!
//! Physics, all public:
//! - **Earth**: c = 343 m/s at 20 °C; air absorption from ISO 9613-1 (at 20 °C / 50 % RH roughly
//!   0.1 dB/m at 8 kHz, falling with f²), modelled as `α(f) = k · f²` with
//!   `k = 0.0016 dB/m/kHz²`.
//! - **Mars**: Maurice et al. 2022 (Nature 605, "In situ recording of Mars soundscape",
//!   Perseverance SuperCam microphone): two speeds of sound, ~240 m/s below ~240 Hz and ~250 m/s
//!   above; a source of equal power is ~20 dB quieter than on Earth (low density, low acoustic
//!   impedance); strong CO₂ vibrational-relaxation absorption so high frequencies vanish within
//!   tens of metres (also Bass & Chambers 2001, JASA 109, "Absorption of sound in the Martian
//!   atmosphere"; Petculescu & Lueptow 2007, Icarus 186). Modelled with the same `k · f²` law
//!   at `k = 0.08 dB/m/kHz²` (≈ 50× Earth), which puts the −3 dB point at ~430 Hz at 200 m.
//! - **Airless** (Europa, the Moon): no airborne path at all. Sound only reaches the listener
//!   through the ground ([`crate::ground`]) or through a pressurised interior's own air.
//!
//! The absorption cutoff is the frequency at which the accumulated absorption reaches 3 dB:
//! `k · f_c² · r = 3` ⇒ `f_c = √(3 / (k r))` (kHz), applied as a 2-pole low-pass whose cutoff
//! drops with range.

use regia_ecs::prelude::*;
use regia_scene::level::{AtmospherePreset, LevelAudioMedium};
use serde::{Deserialize, Serialize};

use crate::spatial::SPEED_OF_SOUND;

/// Air absorption coefficient on Earth, dB per metre per kHz² (ISO 9613-1 fit at 8 kHz).
pub const EARTH_ABSORPTION_DB_PER_M_KHZ2: f32 = 0.0016;
/// Air absorption coefficient on Mars, dB per metre per kHz² (≈ 50× Earth; Bass & Chambers 2001).
pub const MARS_ABSORPTION_DB_PER_M_KHZ2: f32 = 0.08;
/// Speed of sound on Mars near the surface, m/s (Maurice et al. 2022, the low-frequency branch).
pub const MARS_SPEED_OF_SOUND: f32 = 240.0;
/// Level of a source on Mars relative to the same source power on Earth, dB (Maurice et al. 2022).
pub const MARS_LEVEL_OFFSET_DB: f32 = -20.0;
/// Highest cutoff the absorption filter reports; above this the path is left unfiltered.
pub const MAX_CUTOFF_HZ: f32 = 20_000.0;
/// Lowest cutoff the absorption filter reports (keeps the biquad well-conditioned).
pub const MIN_CUTOFF_HZ: f32 = 20.0;

/// Which planet's air a medium was derived from (for the agent protocol and logs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
#[reflect(Default)]
pub enum MediumKind {
    /// Earth-like air.
    #[default]
    Earth,
    /// Thin, cold CO₂ (Mars).
    Mars,
    /// No air (Europa, the Moon).
    Airless,
    /// Air inside a pressurised interior volume (always Earth-like, whatever the planet).
    Interior,
}

impl MediumKind {
    /// Lower-case name as reported by `audio.medium`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Earth => "earth",
            Self::Mars => "mars",
            Self::Airless => "airless",
            Self::Interior => "interior",
        }
    }
}

/// The level's acoustic medium: the airborne path's speed, level and absorption, and the ground
/// material sounds travel through when nothing more specific is known.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Reflect)]
#[reflect(Default)]
pub struct AudioMedium {
    /// Which preset this medium came from.
    pub kind: MediumKind,
    /// Whether sound travels through the air at all (false on airless worlds).
    pub airborne: bool,
    /// Speed of sound in the air, m/s (delay and Doppler).
    pub speed_of_sound: f32,
    /// Level of every airborne arrival relative to Earth for the same source power, dB.
    pub level_offset_db: f32,
    /// Air absorption `k` in `α(f) = k · f²`, dB per metre per kHz².
    pub absorption_db_per_m_khz2: f32,
    /// Ground material (name from [`crate::GroundMaterial::presets`]) used when an emitter or
    /// listener does not name one.
    pub ground: String,
}

impl Default for AudioMedium {
    fn default() -> Self {
        Self::earth()
    }
}

impl AudioMedium {
    /// Earth's air at 20 °C over dry compacted soil.
    #[must_use]
    pub fn earth() -> Self {
        Self {
            kind: MediumKind::Earth,
            airborne: true,
            speed_of_sound: SPEED_OF_SOUND,
            level_offset_db: 0.0,
            absorption_db_per_m_khz2: EARTH_ABSORPTION_DB_PER_M_KHZ2,
            ground: "soil".into(),
        }
    }

    /// Mars' CO₂ air over loose Martian regolith.
    #[must_use]
    pub fn mars() -> Self {
        Self {
            kind: MediumKind::Mars,
            airborne: true,
            speed_of_sound: MARS_SPEED_OF_SOUND,
            level_offset_db: MARS_LEVEL_OFFSET_DB,
            absorption_db_per_m_khz2: MARS_ABSORPTION_DB_PER_M_KHZ2,
            ground: "mars_regolith".into(),
        }
    }

    /// No air (Europa's ice shell by default; override the ground for the Moon).
    #[must_use]
    pub fn airless() -> Self {
        Self {
            kind: MediumKind::Airless,
            airborne: false,
            speed_of_sound: 0.0,
            level_offset_db: -100.0,
            absorption_db_per_m_khz2: 0.0,
            ground: "ice".into(),
        }
    }

    /// The air inside a pressurised interior: Earth air, whatever the planet outside.
    #[must_use]
    pub fn interior() -> Self {
        Self {
            kind: MediumKind::Interior,
            ..Self::earth()
        }
    }

    /// [`Self::interior`] built once. A frame solve borrows this instead of allocating a ground
    /// name for every emitter that is not inside a pressurised volume.
    pub(crate) fn interior_shared() -> &'static Self {
        static INTERIOR: std::sync::LazyLock<AudioMedium> = std::sync::LazyLock::new(AudioMedium::interior);
        &INTERIOR
    }

    /// The medium of an atmosphere preset.
    #[must_use]
    pub fn from_preset(preset: AtmospherePreset) -> Self {
        match preset {
            AtmospherePreset::Earth => Self::earth(),
            AtmospherePreset::Mars => Self::mars(),
            AtmospherePreset::Airless => Self::airless(),
        }
    }

    /// The medium of a level: its preset with the level's overrides applied on top.
    #[must_use]
    pub fn from_level(preset: AtmospherePreset, overrides: &LevelAudioMedium) -> Self {
        let mut medium = Self::from_preset(preset);
        if let Some(airborne) = overrides.airborne {
            medium.airborne = airborne;
            if airborne && medium.speed_of_sound <= 0.0 {
                // Turning air on over an airless preset: start from Earth air.
                let earth = Self::earth();
                medium.speed_of_sound = earth.speed_of_sound;
                medium.level_offset_db = earth.level_offset_db;
                medium.absorption_db_per_m_khz2 = earth.absorption_db_per_m_khz2;
            }
        }
        if let Some(c) = overrides.speed_of_sound_mps.filter(|c| c.is_finite() && *c > 1.0) {
            medium.speed_of_sound = c;
        }
        if let Some(db) = overrides.level_offset_db.filter(|d| d.is_finite()) {
            medium.level_offset_db = db;
        }
        if let Some(k) = overrides
            .absorption_db_per_m_khz2
            .filter(|k| k.is_finite() && *k >= 0.0)
        {
            medium.absorption_db_per_m_khz2 = k;
        }
        if let Some(ground) = overrides.ground.as_ref().filter(|g| !g.is_empty()) {
            medium.ground.clone_from(ground);
        }
        medium
    }

    /// Whether an airborne path exists in this medium.
    #[must_use]
    pub fn carries_sound(&self) -> bool {
        self.airborne && self.speed_of_sound > 1.0
    }

    /// Arrival delay of the airborne path over `distance` metres, seconds (`r / c`).
    #[must_use]
    pub fn delay_secs(&self, distance: f32) -> f32 {
        if self.carries_sound() {
            distance.max(0.0) / self.speed_of_sound
        } else {
            0.0
        }
    }

    /// Absorption low-pass cutoff after `distance` metres, Hz: where `k · f² · r = 3 dB`.
    #[must_use]
    pub fn absorption_cutoff_hz(&self, distance: f32) -> f32 {
        absorption_cutoff_hz(self.absorption_db_per_m_khz2, distance)
    }

    /// Absorption in dB at `hz` after `distance` metres (`k · f² · r`).
    #[must_use]
    pub fn absorption_db(&self, hz: f32, distance: f32) -> f32 {
        let khz = hz / 1000.0;
        self.absorption_db_per_m_khz2 * khz * khz * distance.max(0.0)
    }

    /// Doppler playback-rate ratio in this medium for an emitter closing at `closing_speed` m/s,
    /// scaled by `factor` (OpenAL 1.1 § "Doppler Shift", static listener, the medium's `c`).
    #[must_use]
    pub fn doppler_ratio(&self, closing_speed: f32, factor: f32) -> f32 {
        doppler_ratio_in(self.speed_of_sound, closing_speed, factor)
    }

    /// Doppler ratio with a moving source and listener in this medium; see
    /// [`doppler_ratio_moving_in`].
    pub fn doppler_ratio_moving(&self, source_approach: f32, listener_recede: f32, factor: f32) -> f32 {
        doppler_ratio_moving_in(self.speed_of_sound, source_approach, listener_recede, factor)
    }
}

/// The `k · f² · r = 3 dB` cutoff (Hz), clamped to [`MIN_CUTOFF_HZ`]..[`MAX_CUTOFF_HZ`].
#[must_use]
pub fn absorption_cutoff_hz(k_db_per_m_khz2: f32, distance: f32) -> f32 {
    let kr = k_db_per_m_khz2.max(0.0) * distance.max(0.0);
    if kr <= 1e-9 {
        return MAX_CUTOFF_HZ;
    }
    ((3.0 / kr).sqrt() * 1000.0).clamp(MIN_CUTOFF_HZ, MAX_CUTOFF_HZ)
}

/// Doppler ratio for a medium whose speed of sound is `c` (`c / (c − v·factor)`, clamped to
/// 0.5..4). A medium without sound (`c ≤ 1`) or `factor ≤ 0` gives 1.
#[must_use]
pub fn doppler_ratio_in(c: f32, closing_speed: f32, factor: f32) -> f32 {
    if factor.is_nan() || factor <= 0.0 || c.is_nan() || c <= 1.0 {
        return 1.0;
    }
    let denominator = c - closing_speed * factor;
    if denominator <= 1e-3 {
        return 4.0;
    }
    // A NaN speed (a NaN velocity) is no shift, not a NaN pitch.
    let ratio = c / denominator;
    if ratio.is_nan() {
        1.0
    } else {
        ratio.clamp(0.5, 4.0)
    }
}

/// Doppler ratio with a moving source and listener in a medium whose speed of sound is `c`:
/// `(c − f·v_listener) / (c − f·v_source)` along the line from the source to the listener —
/// `source_approach` is positive when the source moves toward the listener, `listener_recede`
/// positive when the listener moves away from the source. Speeds are clamped below `c / f` (no
/// sonic boom) and the ratio to 0.5..4. A medium without sound (`c ≤ 1`) or `factor ≤ 0` gives 1.
#[must_use]
pub fn doppler_ratio_moving_in(c: f32, source_approach: f32, listener_recede: f32, factor: f32) -> f32 {
    if factor.is_nan() || factor <= 0.0 || c.is_nan() || c <= 1.0 {
        return 1.0;
    }
    let limit = c / factor * 0.95;
    // `f32::min` would turn a NaN speed into the near-sonic limit (a 4× pitch): NaN is still.
    let still = |v: f32| if v.is_nan() { 0.0 } else { v.min(limit) };
    let vs = still(source_approach);
    let vl = still(listener_recede);
    let ratio = (c - factor * vl) / (c - factor * vs);
    if ratio.is_nan() {
        1.0
    } else {
        ratio.clamp(0.5, 4.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doppler_never_returns_nan() {
        for (c, v, f) in [
            (343.0, f32::NAN, 1.0),
            (343.0, 10.0, f32::NAN),
            (f32::NAN, 10.0, 1.0),
            (343.0, f32::INFINITY, 1.0),
            (343.0, f32::NEG_INFINITY, 1.0),
        ] {
            let r = doppler_ratio_in(c, v, f);
            assert!(
                r.is_finite() && (0.5..=4.0).contains(&r),
                "in({c}, {v}, {f}) = {r}"
            );
            let m = doppler_ratio_moving_in(c, v, v, f);
            assert!(
                m.is_finite() && (0.5..=4.0).contains(&m),
                "moving({c}, {v}, {f}) = {m}"
            );
        }
        assert_eq!(doppler_ratio_moving_in(343.0, f32::NAN, 0.0, 1.0), 1.0);
    }

    #[test]
    fn presets_follow_the_atmosphere() {
        assert_eq!(
            AudioMedium::from_preset(AtmospherePreset::Earth).kind,
            MediumKind::Earth
        );
        let mars = AudioMedium::from_preset(AtmospherePreset::Mars);
        assert!((mars.speed_of_sound - 240.0).abs() < 1e-3);
        assert!((mars.level_offset_db + 20.0).abs() < 1e-3);
        let airless = AudioMedium::from_preset(AtmospherePreset::Airless);
        assert!(!airless.carries_sound());
        assert_eq!(airless.ground, "ice");
    }

    #[test]
    fn level_overrides_apply_on_top_of_the_preset() {
        let overrides = LevelAudioMedium {
            speed_of_sound_mps: Some(300.0),
            ground: Some("bedrock".into()),
            ..LevelAudioMedium::default()
        };
        let medium = AudioMedium::from_level(AtmospherePreset::Earth, &overrides);
        assert!((medium.speed_of_sound - 300.0).abs() < 1e-3);
        assert_eq!(medium.ground, "bedrock");
        // Air switched on over an airless preset starts from Earth air.
        let on = LevelAudioMedium {
            airborne: Some(true),
            ..LevelAudioMedium::default()
        };
        let medium = AudioMedium::from_level(AtmospherePreset::Airless, &on);
        assert!(medium.carries_sound());
        assert!((medium.speed_of_sound - SPEED_OF_SOUND).abs() < 1e-3);
        // Garbage overrides are ignored.
        let bad = LevelAudioMedium {
            speed_of_sound_mps: Some(f32::NAN),
            absorption_db_per_m_khz2: Some(-1.0),
            ..LevelAudioMedium::default()
        };
        assert_eq!(
            AudioMedium::from_level(AtmospherePreset::Earth, &bad),
            AudioMedium::earth()
        );
    }

    #[test]
    fn absorption_cutoff_falls_with_range_and_mars_is_darker() {
        let earth = AudioMedium::earth();
        let mars = AudioMedium::mars();
        assert_eq!(earth.absorption_cutoff_hz(0.0), MAX_CUTOFF_HZ);
        let e200 = earth.absorption_cutoff_hz(200.0);
        let m200 = mars.absorption_cutoff_hz(200.0);
        assert!((2_800.0..3_300.0).contains(&e200), "earth 200 m cutoff {e200}");
        assert!((380.0..480.0).contains(&m200), "mars 200 m cutoff {m200}");
        assert!(earth.absorption_cutoff_hz(1000.0) < e200);
        // The cutoff is where the absorption reaches 3 dB.
        assert!((earth.absorption_db(e200, 200.0) - 3.0).abs() < 0.01);
    }

    #[test]
    fn doppler_uses_the_medium_speed() {
        let earth = AudioMedium::earth();
        let mars = AudioMedium::mars();
        let e = earth.doppler_ratio(30.0, 1.0);
        let m = mars.doppler_ratio(30.0, 1.0);
        assert!((e - 343.3 / 313.3).abs() < 1e-4);
        assert!((m - 240.0 / 210.0).abs() < 1e-4);
        assert!(m > e, "the same closing speed shifts more in slower air");
        assert_eq!(AudioMedium::airless().doppler_ratio(30.0, 1.0), 1.0);
        assert_eq!(doppler_ratio_in(100.0, 200.0, 1.0), 4.0, "supersonic clamps");
    }

    #[test]
    fn delay_is_distance_over_c() {
        let mars = AudioMedium::mars();
        assert!((mars.delay_secs(240.0) - 1.0).abs() < 1e-5);
        assert_eq!(AudioMedium::airless().delay_secs(500.0), 0.0);
    }
}
