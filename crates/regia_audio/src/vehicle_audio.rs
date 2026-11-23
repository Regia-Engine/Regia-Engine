// SPDX-License-Identifier: MPL-2.0
//! Vehicle audio (ADR-3271, `docs/specs/16-audio.md` § 3f): cars that sound like cars.
//!
//! Three parts, all in this file:
//!
//! - **The format**, `.vehicleaudio.ron` v1 ([`VehicleAudioDef`], `docs/formats/vehicle-audio.md`):
//!   engine loops each recorded at an rpm, in an on-load and an off-load set, an optional whine
//!   layer, tire squeal, rolling noise per ground surface, suspension bumps, wind and the shift
//!   one-shot.
//! - **The model** ([`VehicleAudioModel`]): a pure, fixed-step function of the drivetrain and wheel
//!   readout ([`VehicleAudioInputs`]) to per-voice amplitudes and pitches ([`VehicleVoiceOut`]) and
//!   one-shot triggers ([`VehicleOneShot`]). The two engine layers nearest the smoothed rpm crossfade
//!   with equal power ([`engine_crossfade`]); each layer plays at `rpm / recorded rpm` (clamped); the
//!   load blend follows throttle and engine torque; the rev limiter cuts in a fixed rhythm; a shift
//!   fires one one-shot per gear change and dips the engine briefly. Every parameter goes through a
//!   fixed-step one-pole smoother ([`one_pole_coefficient`]), so nothing zippers. Steady-state steps
//!   allocate nothing and hash to [`VehicleAudioModel::parameter_digest`].
//! - **The voices** ([`VehicleVoiceSet`]): starts, shapes and stops attached-emitter voices on the
//!   [`Audio`] facade from the model's outputs, under a voice budget mode ([`VehicleVoiceMode`]).
//!   Distance, Doppler, occlusion and the listener's own vehicle (`ListenerCoupling::vehicle`) come
//!   from the existing emitter pipeline: [`Audio::set_emitter_voice_layer`] only sets the voice's own
//!   level and pitch, which the sync composes with them.
//!
//! The ECS bridge (reading `regia_physics` vehicles, placing the emitters, ranking vehicles for the
//! budget) is `regia_framework::vehicle_audio`. [`placeholder_sound`] generates every sound the
//! [`VehicleAudioDef::placeholder`] definition names, in code: they are labeled placeholders, not
//! recordings.

use std::time::Duration;

use regia_core::math::Rng;
use serde::{Deserialize, Serialize};

use crate::audio::Audio;
use crate::bus::MUTE_DB;
use crate::clip::SoundClip;
use crate::error::AudioError;
use crate::loopgen::{FilterMode, LoopGenDef, LoopMixInput, LoopNode, LoopNodeDef, NoiseColor, OscShape};
use crate::sound::{PlaybackParams, SoundAsset};
use crate::spatial::{AudioEmitter, db_to_gain, gain_to_db, polyline};
use crate::voices::SoundInstanceId;

/// The `.vehicleaudio.ron` format version this engine reads.
pub const VEHICLE_AUDIO_VERSION: u32 = 1;

/// The file suffix of a vehicle audio definition.
pub const VEHICLE_AUDIO_SUFFIX: &str = ".vehicleaudio.ron";

/// Most engine layers per set (on-load or off-load).
pub const MAX_ENGINE_LAYERS: usize = 8;

/// Most wheels the model hears; further wheels are ignored.
pub const MAX_AUDIO_WHEELS: usize = 8;

/// Most one-shots one step can queue (shifts and bumps); more are dropped and counted.
pub const ONE_SHOT_CAPACITY: usize = 16;

/// Sound names with this prefix are generated in code by [`placeholder_sound`].
pub const PLACEHOLDER_PREFIX: &str = "placeholder:";

/// Time constant of the rev-limiter cut's own smoother, seconds (fast, but never a step).
pub const LIMITER_SMOOTHING_SECS: f32 = 0.004;

/// A voice starts when its amplitude rises above this (−60 dB) ...
pub const VOICE_START_AMPLITUDE: f32 = 1.0e-3;

/// ... and stops when it falls below this (−80 dB); the gap is the hysteresis.
pub const VOICE_STOP_AMPLITUDE: f32 = 1.0e-4;

/// Fade of a voice the budget or the model stops.
pub const VOICE_STOP_FADE: Duration = Duration::from_millis(60);

/// In reduced mode the kept engine voice changes only when another is this much louder.
pub const REDUCED_SWITCH_RATIO: f32 = 1.25;

// ------------------------------------------------------------------------------------------ format

/// Where on the vehicle a sound plays (emitter slot).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VehicleEmitterSlot {
    /// The engine bay (`emitters.engine`).
    Engine,
    /// The tailpipe (`emitters.exhaust`).
    #[default]
    Exhaust,
    /// A wheel's contact (tires and bumps; index into the vehicle's wheels).
    Wheel(u8),
}

/// One engine loop, recorded at `rpm`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineLayerDef {
    /// The engine speed the loop was recorded at (pitch 1 there).
    pub rpm: f32,
    /// The loop sound (a `SoundAssets` name, or `placeholder:...`).
    pub sound: String,
    /// Gain of this layer, dB.
    #[serde(default)]
    pub gain_db: f32,
}

/// The rev limiter's sound: a rhythmic cut while the engine sits on the redline under throttle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RevLimiterDef {
    /// The cut starts this far below `redline_rpm`.
    #[serde(default = "default_limiter_margin")]
    pub margin_rpm: f32,
    /// Engine level during a cut, dB (≤ 0).
    #[serde(default = "default_limiter_cut")]
    pub cut_db: f32,
    /// Cuts per second.
    #[serde(default = "default_limiter_rate")]
    pub rate_hz: f32,
    /// The cut needs at least this much throttle, 0..1.
    #[serde(default = "default_limiter_throttle")]
    pub min_throttle: f32,
}

impl Default for RevLimiterDef {
    fn default() -> Self {
        Self {
            margin_rpm: default_limiter_margin(),
            cut_db: default_limiter_cut(),
            rate_hz: default_limiter_rate(),
            min_throttle: default_limiter_throttle(),
        }
    }
}

fn default_limiter_margin() -> f32 {
    120.0
}
fn default_limiter_cut() -> f32 {
    -14.0
}
fn default_limiter_rate() -> f32 {
    16.0
}
fn default_limiter_throttle() -> f32 {
    0.5
}

/// The gear-change one-shot and the engine dip that goes with it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShiftSoundDef {
    /// The one-shot sound.
    pub sound: String,
    /// Its gain, dB.
    #[serde(default)]
    pub gain_db: f32,
    /// The engine layers dip to this level during a shift, dB (≤ 0).
    #[serde(default = "default_dip_db")]
    pub dip_db: f32,
    /// How long the dip holds, seconds.
    #[serde(default = "default_dip_secs")]
    pub dip_secs: f32,
    /// Downshifts play the one-shot too (upshifts always do).
    #[serde(default)]
    pub on_downshift: bool,
}

fn default_dip_db() -> f32 {
    -8.0
}
fn default_dip_secs() -> f32 {
    0.12
}

/// The engine: two crossfaded loop sets and the load blend between them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineSoundDef {
    /// Loops recorded under load (throttle open), at rising rpm. At least one.
    pub on_load: Vec<EngineLayerDef>,
    /// Loops recorded off load (overrun, throttle shut). Empty: the on-load set plays alone.
    #[serde(default)]
    pub off_load: Vec<EngineLayerDef>,
    /// Engine level, dB.
    #[serde(default)]
    pub volume_db: f32,
    /// Extra level of the off-load set, dB.
    #[serde(default = "default_off_load_db")]
    pub off_load_db: f32,
    /// Lowest playback pitch of a layer.
    #[serde(default = "default_min_pitch")]
    pub min_pitch: f32,
    /// Highest playback pitch of a layer.
    #[serde(default = "default_max_pitch")]
    pub max_pitch: f32,
    /// Share of the load target that comes from the throttle (the rest from engine torque), 0..1.
    #[serde(default = "default_throttle_weight")]
    pub throttle_weight: f32,
    /// Engine torque that counts as full load, newton-meters (> 0).
    #[serde(default = "default_peak_torque")]
    pub peak_torque_nm: f32,
    /// The redline, rpm (the rev limiter sits just below it).
    #[serde(default = "default_redline")]
    pub redline_rpm: f32,
    /// The rev limiter's cut.
    #[serde(default)]
    pub limiter: RevLimiterDef,
    /// The shift one-shot and dip (none: shifts are silent).
    #[serde(default)]
    pub shift: Option<ShiftSoundDef>,
    /// Where the on-load set plays.
    #[serde(default = "exhaust_slot")]
    pub on_load_slot: VehicleEmitterSlot,
    /// Where the off-load set plays.
    #[serde(default = "engine_slot")]
    pub off_load_slot: VehicleEmitterSlot,
}

fn default_off_load_db() -> f32 {
    -4.0
}
fn default_min_pitch() -> f32 {
    0.5
}
fn default_max_pitch() -> f32 {
    2.0
}
fn default_throttle_weight() -> f32 {
    0.6
}
fn default_peak_torque() -> f32 {
    300.0
}
fn default_redline() -> f32 {
    7000.0
}
fn exhaust_slot() -> VehicleEmitterSlot {
    VehicleEmitterSlot::Exhaust
}
fn engine_slot() -> VehicleEmitterSlot {
    VehicleEmitterSlot::Engine
}

/// A whine or turbo layer: one loop whose level follows rpm.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WhineSoundDef {
    /// The loop sound.
    pub sound: String,
    /// The rpm it was recorded at.
    pub recorded_rpm: f32,
    /// Level by rpm, `(rpm, dB)` points.
    pub gain_curve: Vec<(f32, f32)>,
    /// Level off load as a fraction of the level on load, 0..1.
    #[serde(default = "default_whine_off_load")]
    pub off_load_share: f32,
}

fn default_whine_off_load() -> f32 {
    0.4
}

/// Tire squeal from combined slip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SquealSoundDef {
    /// The loop sound.
    pub sound: String,
    /// Level at full squeal, dB.
    #[serde(default)]
    pub gain_db: f32,
    /// Slip angle that counts as one unit of slip, degrees (> 0).
    #[serde(default = "default_slip_angle_peak")]
    pub slip_angle_peak_deg: f32,
    /// Slip ratio that counts as one unit of slip (> 0).
    #[serde(default = "default_slip_ratio_peak")]
    pub slip_ratio_peak: f32,
    /// Squeal starts above this combined slip.
    #[serde(default = "default_squeal_threshold")]
    pub threshold: f32,
    /// Squeal is full at this combined slip (> `threshold`).
    #[serde(default = "default_squeal_full")]
    pub full: f32,
    /// Pitch rises by this much from the first squeal to full squeal.
    #[serde(default = "default_squeal_pitch")]
    pub pitch_spread: f32,
}

fn default_slip_angle_peak() -> f32 {
    8.0
}
fn default_slip_ratio_peak() -> f32 {
    0.12
}
fn default_squeal_threshold() -> f32 {
    0.6
}
fn default_squeal_full() -> f32 {
    1.6
}
fn default_squeal_pitch() -> f32 {
    0.15
}

/// Rolling noise on one ground surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RollingSoundDef {
    /// The surface name (`"default"` is the fallback for every unnamed surface).
    pub surface: String,
    /// The loop sound.
    pub sound: String,
    /// Level by contact speed, `(m/s, dB)` points.
    pub gain_curve: Vec<(f32, f32)>,
    /// Pitch rise per m/s of contact speed (pitch 1 at rest).
    #[serde(default = "default_roll_pitch")]
    pub pitch_per_mps: f32,
}

fn default_roll_pitch() -> f32 {
    0.01
}

/// Suspension bump one-shots from the compression rate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BumpSoundDef {
    /// The one-shot sound.
    pub sound: String,
    /// Level of the hardest bump, dB.
    #[serde(default)]
    pub gain_db: f32,
    /// A bump needs the suspension to compress faster than this, m/s (> 0).
    #[serde(default = "default_bump_threshold")]
    pub rate_threshold_mps: f32,
    /// Compression rate of the hardest bump, m/s (> threshold).
    #[serde(default = "default_bump_full")]
    pub full_rate_mps: f32,
    /// Shortest time between two bumps of one wheel, seconds.
    #[serde(default = "default_bump_cooldown")]
    pub cooldown_secs: f32,
}

fn default_bump_threshold() -> f32 {
    0.6
}
fn default_bump_full() -> f32 {
    3.0
}
fn default_bump_cooldown() -> f32 {
    0.15
}

/// The tires: squeal, rolling noise, bumps.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TireSoundDef {
    /// Squeal (none: silent tires).
    #[serde(default)]
    pub squeal: Option<SquealSoundDef>,
    /// Rolling noise per surface; the first entry (or the one named `default`) is the fallback.
    #[serde(default)]
    pub rolling: Vec<RollingSoundDef>,
    /// Bumps (none: no bump one-shots).
    #[serde(default)]
    pub bump: Option<BumpSoundDef>,
}

/// Wind noise by speed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindSoundDef {
    /// The loop sound.
    pub sound: String,
    /// Level by chassis speed, `(m/s, dB)` points.
    pub gain_curve: Vec<(f32, f32)>,
    /// Pitch rise per m/s (pitch 1 at rest).
    #[serde(default = "default_wind_pitch")]
    pub pitch_per_mps: f32,
}

fn default_wind_pitch() -> f32 {
    0.005
}

/// Time constants of the one-pole smoothers, seconds (0 = no smoothing).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SmoothingDef {
    /// Engine speed (drives the crossfade and every pitch).
    #[serde(default = "default_rpm_secs")]
    pub rpm_secs: f32,
    /// The on-load/off-load blend.
    #[serde(default = "default_load_secs")]
    pub load_secs: f32,
    /// Shift dip, whine and wind levels.
    #[serde(default = "default_gain_secs")]
    pub gain_secs: f32,
    /// Tire squeal and rolling levels and pitches.
    #[serde(default = "default_tire_secs")]
    pub tire_secs: f32,
}

impl Default for SmoothingDef {
    fn default() -> Self {
        Self {
            rpm_secs: default_rpm_secs(),
            load_secs: default_load_secs(),
            gain_secs: default_gain_secs(),
            tire_secs: default_tire_secs(),
        }
    }
}

fn default_rpm_secs() -> f32 {
    0.03
}
fn default_load_secs() -> f32 {
    0.08
}
fn default_gain_secs() -> f32 {
    0.05
}
fn default_tire_secs() -> f32 {
    0.06
}

/// Emitter offsets in chassis space (meters; forward is −Z, up is +Y).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmitterPlacementDef {
    /// The engine bay.
    #[serde(default = "default_engine_pos")]
    pub engine: [f32; 3],
    /// The tailpipe.
    #[serde(default = "default_exhaust_pos")]
    pub exhaust: [f32; 3],
}

impl Default for EmitterPlacementDef {
    fn default() -> Self {
        Self {
            engine: default_engine_pos(),
            exhaust: default_exhaust_pos(),
        }
    }
}

fn default_engine_pos() -> [f32; 3] {
    [0.0, 0.3, -1.4]
}
fn default_exhaust_pos() -> [f32; 3] {
    [0.4, -0.25, 2.1]
}

/// The RON form of a vehicle's sound (`.vehicleaudio.ron`), version 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VehicleAudioDef {
    /// Format version; must be [`VEHICLE_AUDIO_VERSION`].
    pub format_version: u32,
    /// Name (logs, the agent protocol).
    pub name: String,
    /// Bus the voices play on (default `World`).
    #[serde(default)]
    pub bus: Option<String>,
    /// The engine.
    pub engine: EngineSoundDef,
    /// A whine or turbo layer.
    #[serde(default)]
    pub whine: Option<WhineSoundDef>,
    /// The tires.
    #[serde(default)]
    pub tires: TireSoundDef,
    /// Wind noise.
    #[serde(default)]
    pub wind: Option<WindSoundDef>,
    /// Smoothing time constants.
    #[serde(default)]
    pub smoothing: SmoothingDef,
    /// Emitter placement.
    #[serde(default)]
    pub emitters: EmitterPlacementDef,
}

/// Why a `.vehicleaudio.ron` did not load.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum VehicleAudioError {
    /// The text is not valid RON for the format.
    #[error("vehicle audio does not parse: {0}")]
    Parse(String),
    /// The file's `format_version` is not one this build reads.
    #[error("vehicle audio format version {found}; this build reads {expected}")]
    Version {
        /// The version in the file.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// A value is out of range, not finite, or a list is empty or too long.
    #[error("vehicle audio field `{field}`: {message}")]
    Value {
        /// Key path of the field (`engine.on_load[1].rpm`).
        field: String,
        /// What is wrong.
        message: String,
    },
}

impl VehicleAudioError {
    /// A stable identifier: lowercase ASCII, digits and `_` (agent protocol, logs).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "vehicle_audio_parse",
            Self::Version { .. } => "vehicle_audio_version",
            Self::Value { .. } => "vehicle_audio_value",
        }
    }
}

fn bad(field: impl Into<String>, message: impl Into<String>) -> VehicleAudioError {
    VehicleAudioError::Value {
        field: field.into(),
        message: message.into(),
    }
}

impl VehicleAudioDef {
    /// Parses and validates a `.vehicleaudio.ron` text. The version is peeked first, so a newer
    /// file is rejected even when its body no longer matches.
    ///
    /// # Errors
    /// [`VehicleAudioError`]: a parse failure, another version, or a bad value.
    pub fn from_ron(text: &str) -> Result<Self, VehicleAudioError> {
        let found = regia_core::version::peek_ron_version(text).map_err(VehicleAudioError::Parse)?;
        if found > VEHICLE_AUDIO_VERSION || found == 0 {
            return Err(VehicleAudioError::Version {
                found,
                expected: VEHICLE_AUDIO_VERSION,
            });
        }
        let mut def: Self = ron::from_str(text).map_err(|e| VehicleAudioError::Parse(e.to_string()))?;
        def.migrate_to_latest()?;
        def.validate()?;
        Ok(def)
    }

    /// The definition as pretty RON text.
    ///
    /// # Errors
    /// [`VehicleAudioError::Parse`] when RON cannot write the value (never for a valid one).
    pub fn to_ron(&self) -> Result<String, VehicleAudioError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| VehicleAudioError::Parse(e.to_string()))
    }

    /// Runs the migrator chain up to [`VEHICLE_AUDIO_VERSION`] (none at v1; `docs/specs/12-assets.md`
    /// § 4a). Returns the version reached.
    ///
    /// # Errors
    /// [`VehicleAudioError::Version`] for a newer or zero version.
    pub fn migrate_to_latest(&mut self) -> Result<u32, VehicleAudioError> {
        if self.format_version > VEHICLE_AUDIO_VERSION || self.format_version == 0 {
            return Err(VehicleAudioError::Version {
                found: self.format_version,
                expected: VEHICLE_AUDIO_VERSION,
            });
        }
        // v1 is the first version: nothing older exists to migrate from.
        Ok(self.format_version)
    }

    /// Checks every range and list length.
    ///
    /// # Errors
    /// [`VehicleAudioError::Value`] naming the first bad field.
    pub fn validate(&self) -> Result<(), VehicleAudioError> {
        if self.name.trim().is_empty() {
            return Err(bad("name", "is empty"));
        }
        let engine = &self.engine;
        check_layers("engine.on_load", &engine.on_load, false)?;
        check_layers("engine.off_load", &engine.off_load, true)?;
        let finite = |field: &str, v: f32| {
            if v.is_finite() {
                Ok(())
            } else {
                Err(bad(field, format!("{v} is not finite")))
            }
        };
        let positive = |field: &str, v: f32| {
            if v.is_finite() && v > 0.0 {
                Ok(())
            } else {
                Err(bad(field, format!("{v} must be finite and above 0")))
            }
        };
        let within = |field: &str, v: f32, lo: f32, hi: f32| {
            if v.is_finite() && (lo..=hi).contains(&v) {
                Ok(())
            } else {
                Err(bad(field, format!("{v} is outside {lo}..={hi}")))
            }
        };
        within("engine.volume_db", engine.volume_db, -100.0, 24.0)?;
        within("engine.off_load_db", engine.off_load_db, -100.0, 24.0)?;
        within("engine.min_pitch", engine.min_pitch, 0.05, 1.0)?;
        within("engine.max_pitch", engine.max_pitch, 1.0, 8.0)?;
        within("engine.throttle_weight", engine.throttle_weight, 0.0, 1.0)?;
        positive("engine.peak_torque_nm", engine.peak_torque_nm)?;
        positive("engine.redline_rpm", engine.redline_rpm)?;
        within(
            "engine.limiter.margin_rpm",
            engine.limiter.margin_rpm,
            0.0,
            5000.0,
        )?;
        if engine.limiter.margin_rpm >= engine.redline_rpm {
            return Err(bad(
                "engine.limiter.margin_rpm",
                format!(
                    "{} is at or past engine.redline_rpm ({}): the cut would sit at or below 0 rpm",
                    engine.limiter.margin_rpm, engine.redline_rpm
                ),
            ));
        }
        within("engine.limiter.cut_db", engine.limiter.cut_db, -100.0, 0.0)?;
        within("engine.limiter.rate_hz", engine.limiter.rate_hz, 0.0, 100.0)?;
        within(
            "engine.limiter.min_throttle",
            engine.limiter.min_throttle,
            0.0,
            1.0,
        )?;
        if let Some(shift) = &engine.shift {
            check_sound("engine.shift.sound", &shift.sound)?;
            within("engine.shift.gain_db", shift.gain_db, -100.0, 24.0)?;
            within("engine.shift.dip_db", shift.dip_db, -100.0, 0.0)?;
            within("engine.shift.dip_secs", shift.dip_secs, 0.0, 5.0)?;
        }
        if let Some(whine) = &self.whine {
            check_sound("whine.sound", &whine.sound)?;
            positive("whine.recorded_rpm", whine.recorded_rpm)?;
            check_curve("whine.gain_curve", &whine.gain_curve)?;
            within("whine.off_load_share", whine.off_load_share, 0.0, 1.0)?;
        }
        if let Some(squeal) = &self.tires.squeal {
            check_sound("tires.squeal.sound", &squeal.sound)?;
            within("tires.squeal.gain_db", squeal.gain_db, -100.0, 24.0)?;
            positive("tires.squeal.slip_angle_peak_deg", squeal.slip_angle_peak_deg)?;
            positive("tires.squeal.slip_ratio_peak", squeal.slip_ratio_peak)?;
            within("tires.squeal.threshold", squeal.threshold, 0.0, 100.0)?;
            if !(squeal.full.is_finite() && squeal.full > squeal.threshold) {
                return Err(bad("tires.squeal.full", "must be finite and above `threshold`"));
            }
            within("tires.squeal.pitch_spread", squeal.pitch_spread, 0.0, 2.0)?;
        }
        if self.tires.rolling.len() > u16::MAX as usize / 2 {
            return Err(bad("tires.rolling", "too many surfaces"));
        }
        for (i, rolling) in self.tires.rolling.iter().enumerate() {
            if rolling.surface.trim().is_empty() {
                return Err(bad(format!("tires.rolling[{i}].surface"), "is empty"));
            }
            check_sound(&format!("tires.rolling[{i}].sound"), &rolling.sound)?;
            check_curve(&format!("tires.rolling[{i}].gain_curve"), &rolling.gain_curve)?;
            within(
                &format!("tires.rolling[{i}].pitch_per_mps"),
                rolling.pitch_per_mps,
                0.0,
                1.0,
            )?;
        }
        if let Some(bump) = &self.tires.bump {
            check_sound("tires.bump.sound", &bump.sound)?;
            within("tires.bump.gain_db", bump.gain_db, -100.0, 24.0)?;
            positive("tires.bump.rate_threshold_mps", bump.rate_threshold_mps)?;
            if !(bump.full_rate_mps.is_finite() && bump.full_rate_mps > bump.rate_threshold_mps) {
                return Err(bad(
                    "tires.bump.full_rate_mps",
                    "must be finite and above `rate_threshold_mps`",
                ));
            }
            within("tires.bump.cooldown_secs", bump.cooldown_secs, 0.0, 10.0)?;
        }
        if let Some(wind) = &self.wind {
            check_sound("wind.sound", &wind.sound)?;
            check_curve("wind.gain_curve", &wind.gain_curve)?;
            within("wind.pitch_per_mps", wind.pitch_per_mps, 0.0, 1.0)?;
        }
        let s = &self.smoothing;
        for (field, v) in [
            ("smoothing.rpm_secs", s.rpm_secs),
            ("smoothing.load_secs", s.load_secs),
            ("smoothing.gain_secs", s.gain_secs),
            ("smoothing.tire_secs", s.tire_secs),
        ] {
            within(field, v, 0.0, 10.0)?;
        }
        for (field, p) in [
            ("emitters.engine", self.emitters.engine),
            ("emitters.exhaust", self.emitters.exhaust),
        ] {
            for v in p {
                finite(field, v)?;
            }
        }
        for (field, slot) in [
            ("engine.on_load_slot", engine.on_load_slot),
            ("engine.off_load_slot", engine.off_load_slot),
        ] {
            if matches!(slot, VehicleEmitterSlot::Wheel(_)) {
                return Err(bad(field, "the engine plays at Engine or Exhaust"));
            }
        }
        Ok(())
    }

    /// The placeholder car: four on-load and three off-load loops, a whine, squeal, rolling noise
    /// on `default` and `gravel`, bumps, wind and the shift. Every sound is `placeholder:...`,
    /// generated in code by [`placeholder_sound`] (no recordings).
    #[must_use]
    pub fn placeholder() -> Self {
        let layer = |set: &str, rpm: f32| EngineLayerDef {
            rpm,
            sound: format!("{PLACEHOLDER_PREFIX}engine_{set}_{rpm}"),
            gain_db: 0.0,
        };
        Self {
            format_version: VEHICLE_AUDIO_VERSION,
            name: "placeholder_car".into(),
            bus: None,
            engine: EngineSoundDef {
                on_load: [1000.0, 2500.0, 4500.0, 6500.0].map(|r| layer("on", r)).to_vec(),
                off_load: [1000.0, 3000.0, 6000.0].map(|r| layer("off", r)).to_vec(),
                volume_db: -6.0,
                off_load_db: default_off_load_db(),
                min_pitch: default_min_pitch(),
                max_pitch: default_max_pitch(),
                throttle_weight: default_throttle_weight(),
                peak_torque_nm: default_peak_torque(),
                redline_rpm: default_redline(),
                limiter: RevLimiterDef::default(),
                shift: Some(ShiftSoundDef {
                    sound: format!("{PLACEHOLDER_PREFIX}shift"),
                    gain_db: -6.0,
                    dip_db: default_dip_db(),
                    dip_secs: default_dip_secs(),
                    on_downshift: false,
                }),
                on_load_slot: VehicleEmitterSlot::Exhaust,
                off_load_slot: VehicleEmitterSlot::Engine,
            },
            whine: Some(WhineSoundDef {
                sound: format!("{PLACEHOLDER_PREFIX}whine_5000"),
                recorded_rpm: 5000.0,
                gain_curve: vec![(1000.0, -40.0), (4000.0, -24.0), (7000.0, -16.0)],
                off_load_share: default_whine_off_load(),
            }),
            tires: TireSoundDef {
                squeal: Some(SquealSoundDef {
                    sound: format!("{PLACEHOLDER_PREFIX}squeal"),
                    gain_db: -8.0,
                    slip_angle_peak_deg: default_slip_angle_peak(),
                    slip_ratio_peak: default_slip_ratio_peak(),
                    threshold: default_squeal_threshold(),
                    full: default_squeal_full(),
                    pitch_spread: default_squeal_pitch(),
                }),
                rolling: vec![
                    RollingSoundDef {
                        surface: "default".into(),
                        sound: format!("{PLACEHOLDER_PREFIX}roll_asphalt"),
                        gain_curve: vec![(0.0, -100.0), (1.0, -40.0), (30.0, -18.0)],
                        pitch_per_mps: default_roll_pitch(),
                    },
                    RollingSoundDef {
                        surface: "gravel".into(),
                        sound: format!("{PLACEHOLDER_PREFIX}roll_gravel"),
                        gain_curve: vec![(0.0, -100.0), (1.0, -30.0), (30.0, -12.0)],
                        pitch_per_mps: default_roll_pitch(),
                    },
                ],
                bump: Some(BumpSoundDef {
                    sound: format!("{PLACEHOLDER_PREFIX}bump"),
                    gain_db: -6.0,
                    rate_threshold_mps: default_bump_threshold(),
                    full_rate_mps: default_bump_full(),
                    cooldown_secs: default_bump_cooldown(),
                }),
            },
            wind: Some(WindSoundDef {
                sound: format!("{PLACEHOLDER_PREFIX}wind"),
                gain_curve: vec![(0.0, -100.0), (5.0, -50.0), (40.0, -14.0)],
                pitch_per_mps: default_wind_pitch(),
            }),
            smoothing: SmoothingDef::default(),
            emitters: EmitterPlacementDef::default(),
        }
    }
}

fn check_sound(field: &str, sound: &str) -> Result<(), VehicleAudioError> {
    if sound.trim().is_empty() {
        return Err(bad(field, "is empty"));
    }
    Ok(())
}

fn check_curve(field: &str, curve: &[(f32, f32)]) -> Result<(), VehicleAudioError> {
    if curve.is_empty() {
        return Err(bad(field, "needs at least one point"));
    }
    for (i, &(x, y)) in curve.iter().enumerate() {
        if !x.is_finite() || !y.is_finite() || y > 24.0 {
            return Err(bad(
                format!("{field}[{i}]"),
                "points must be finite, levels at most 24 dB",
            ));
        }
        if i > 0 && x <= curve[i - 1].0 {
            return Err(bad(format!("{field}[{i}]"), "x must rise strictly"));
        }
    }
    Ok(())
}

fn check_layers(field: &str, layers: &[EngineLayerDef], may_be_empty: bool) -> Result<(), VehicleAudioError> {
    if layers.is_empty() && !may_be_empty {
        return Err(bad(field, "needs at least one layer"));
    }
    if layers.len() > MAX_ENGINE_LAYERS {
        return Err(bad(field, format!("at most {MAX_ENGINE_LAYERS} layers")));
    }
    for (i, layer) in layers.iter().enumerate() {
        if !(layer.rpm.is_finite() && layer.rpm > 0.0) {
            return Err(bad(format!("{field}[{i}].rpm"), "must be finite and above 0"));
        }
        if i > 0 && layer.rpm <= layers[i - 1].rpm {
            return Err(bad(
                format!("{field}[{i}].rpm"),
                "layers must rise strictly in rpm",
            ));
        }
        check_sound(&format!("{field}[{i}].sound"), &layer.sound)?;
        if !(layer.gain_db.is_finite() && layer.gain_db <= 24.0) {
            return Err(bad(
                format!("{field}[{i}].gain_db"),
                "must be finite and at most 24 dB",
            ));
        }
    }
    Ok(())
}

/// Versions a migrator accepts below the current one (none at v1), for `cargo xtask check-formats`.
#[must_use]
pub fn vehicle_audio_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of `.vehicleaudio.ron` (hashed by `cargo xtask check-formats`):
/// the root struct and every nested struct, plus the emitter slot's variants.
#[must_use]
pub fn vehicle_audio_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<VehicleAudioDef>(),
        serde_field_names::<EngineSoundDef>(),
        serde_field_names::<EngineLayerDef>(),
        serde_field_names::<RevLimiterDef>(),
        serde_field_names::<ShiftSoundDef>(),
        serde_field_names::<WhineSoundDef>(),
        serde_field_names::<TireSoundDef>(),
        serde_field_names::<SquealSoundDef>(),
        serde_field_names::<RollingSoundDef>(),
        serde_field_names::<BumpSoundDef>(),
        serde_field_names::<WindSoundDef>(),
        serde_field_names::<SmoothingDef>(),
        serde_field_names::<EmitterPlacementDef>(),
        serde_variant_names::<VehicleEmitterSlot>(),
    ])
}

// ------------------------------------------------------------------------------------- pure math

/// The one-pole smoothing coefficient for a step of `dt` seconds and time constant `tau`:
/// `1 − e^(−dt/τ)`; 1 (no smoothing) for `tau ≤ 0`, 0 for a non-finite or non-positive step.
/// Each step moves a smoothed value by exactly `coefficient × (target − value)`, so its change per
/// step is bounded by `coefficient × |target − value|`.
#[must_use]
pub fn one_pole_coefficient(tau: f32, dt: f32) -> f32 {
    if !dt.is_finite() || dt <= 0.0 {
        return 0.0;
    }
    if !tau.is_finite() || tau <= 0.0 {
        return 1.0;
    }
    (1.0 - (-f64::from(dt) / f64::from(tau)).exp()) as f32
}

/// One step of a one-pole smoother toward `target` (a NaN target holds the value).
#[inline]
fn smooth(value: f32, target: f32, coefficient: f32) -> f32 {
    if target.is_nan() {
        return value;
    }
    value + coefficient * (target - value)
}

/// The equal-power crossfade over engine layers recorded at the rising `rpms`: the (at most two)
/// layers nearest `rpm` as `[(index, weight); 2]` with `w0² + w1² = 1`. On a recorded rpm the
/// weights are exactly `(1, 0)` (that layer and its upper neighbor); below the first or above
/// the last layer the end layer plays alone (the second entry has weight 0). Empty `rpms` give
/// zero weights. No allocation.
#[must_use]
pub fn engine_crossfade(rpms: &[f32], rpm: f32) -> [(usize, f32); 2] {
    let Some(&first) = rpms.first() else {
        return [(0, 0.0), (0, 0.0)];
    };
    let last_index = rpms.len() - 1;
    if rpms.len() == 1 || rpm.is_nan() || rpm <= first {
        return [(0, 1.0), (0, 0.0)];
    }
    if rpm >= rpms[last_index] {
        return [(last_index, 1.0), (last_index, 0.0)];
    }
    // The bracketing pair: the last layer at or below `rpm` and the next one.
    let mut low = 0;
    while low + 1 < last_index && rpms[low + 1] <= rpm {
        low += 1;
    }
    let span = rpms[low + 1] - rpms[low];
    let t = ((rpm - rpms[low]) / span).clamp(0.0, 1.0);
    let angle = t * std::f32::consts::FRAC_PI_2;
    [(low, angle.cos()), (low + 1, angle.sin())]
}

/// The squeal amount 0..1 of a contact slipping at `slip_angle` (radians) and `slip_ratio`: the
/// combined slip `x = √((|α|/α_peak)² + (|κ|/κ_peak)²)` mapped linearly from `threshold` (0) to
/// `full` (1). Exactly 0 at or below the threshold, monotonic in `|α|` and `|κ|` above it.
#[must_use]
pub fn squeal_amount(def: &SquealSoundDef, slip_angle: f32, slip_ratio: f32) -> f32 {
    let a = slip_angle.abs().to_degrees() / def.slip_angle_peak_deg.max(1e-6);
    let k = slip_ratio.abs() / def.slip_ratio_peak.max(1e-6);
    // `hypot`, not `(a² + k²).sqrt()`: the squares overflow to infinity at slips past ~2e18 and
    // the non-finite guard below would read full slip as no slip.
    let x = a.hypot(k);
    if !x.is_finite() || x <= def.threshold {
        return 0.0;
    }
    ((x - def.threshold) / (def.full - def.threshold).max(1e-6)).clamp(0.0, 1.0)
}

/// The engine load target 0..1 from the throttle and the engine's net torque.
#[must_use]
pub fn load_target(engine: &EngineSoundDef, throttle: f32, engine_torque: f32) -> f32 {
    let throttle = if throttle.is_finite() {
        throttle.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let torque = if engine_torque.is_finite() {
        (engine_torque / engine.peak_torque_nm.max(1e-3)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let w = engine.throttle_weight.clamp(0.0, 1.0);
    w * throttle + (1.0 - w) * torque
}

// ----------------------------------------------------------------------------------------- model

/// What one wheel tells the model each step.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WheelAudioInput {
    /// The wheel touches the ground.
    pub grounded: bool,
    /// Lateral slip angle, radians.
    pub slip_angle: f32,
    /// Longitudinal slip ratio.
    pub slip_ratio: f32,
    /// Speed of the contact over the ground, m/s.
    pub contact_speed: f32,
    /// Suspension compression, meters (positive compressed).
    pub compression: f32,
    /// Index into `tires.rolling` of the surface under the wheel ([`VehicleAudioModel::surface_index`]).
    pub surface: u16,
}

/// What the vehicle tells the model each step (drivetrain and wheel readout).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VehicleAudioInputs {
    /// Engine speed, rpm.
    pub rpm: f32,
    /// Throttle that reached the engine, 0..1.
    pub throttle: f32,
    /// The engine's net torque, newton-meters.
    pub engine_torque: f32,
    /// Upshifts since the start (a counter: each increase fires the shift one-shot).
    pub upshifts: u32,
    /// Downshifts since the start.
    pub downshifts: u32,
    /// A shift is under way.
    pub shifting: bool,
    /// Chassis speed, m/s.
    pub speed: f32,
    /// The wheels (the first `wheel_count`).
    pub wheels: [WheelAudioInput; MAX_AUDIO_WHEELS],
    /// How many entries of `wheels` are real.
    pub wheel_count: usize,
}

/// What a voice of the model is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VehicleVoiceRole {
    /// On-load engine layer (index into `engine.on_load`).
    EngineOn(u8),
    /// Off-load engine layer (index into `engine.off_load`).
    EngineOff(u8),
    /// The whine layer.
    Whine,
    /// Wind.
    Wind,
    /// Squeal of a wheel.
    Squeal(u8),
    /// Rolling noise of a wheel.
    Rolling(u8),
}

impl VehicleVoiceRole {
    /// The role as a short label (`engine_on_2`, `squeal_0`) for tools and the agent protocol.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::EngineOn(i) => format!("engine_on_{i}"),
            Self::EngineOff(i) => format!("engine_off_{i}"),
            Self::Whine => "whine".into(),
            Self::Wind => "wind".into(),
            Self::Squeal(i) => format!("squeal_{i}"),
            Self::Rolling(i) => format!("rolling_{i}"),
        }
    }

    /// An engine layer (the voices the reduced budget mode keeps one of).
    #[must_use]
    pub fn is_engine(self) -> bool {
        matches!(self, Self::EngineOn(_) | Self::EngineOff(_))
    }

    fn code(self) -> u32 {
        match self {
            Self::EngineOn(i) => u32::from(i),
            Self::EngineOff(i) => 0x100 | u32::from(i),
            Self::Whine => 0x200,
            Self::Wind => 0x300,
            Self::Squeal(i) => 0x400 | u32::from(i),
            Self::Rolling(i) => 0x500 | u32::from(i),
        }
    }
}

/// One looping voice's state after a step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VehicleVoiceOut {
    /// What it is.
    pub role: VehicleVoiceRole,
    /// Where it plays.
    pub slot: VehicleEmitterSlot,
    /// Index into [`VehicleAudioModel::sounds`].
    pub sound: u16,
    /// For an engine layer, its crossfade weight within its set (else 1).
    pub weight: f32,
    /// Linear amplitude (every gain applied; 0 silent).
    pub amplitude: f32,
    /// Playback pitch ratio.
    pub pitch: f32,
}

/// What fired a one-shot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VehicleOneShotKind {
    /// A gear change.
    Shift,
    /// A suspension bump.
    Bump,
}

/// A one-shot the model asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VehicleOneShot {
    /// Shift or bump.
    pub kind: VehicleOneShotKind,
    /// Where it plays.
    pub slot: VehicleEmitterSlot,
    /// Index into [`VehicleAudioModel::sounds`].
    pub sound: u16,
    /// Linear amplitude.
    pub amplitude: f32,
}

/// The smoothed parameters after a step.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VehicleAudioParams {
    /// Smoothed engine speed, rpm.
    pub rpm: f32,
    /// Smoothed load 0..1 (1 = the on-load set alone).
    pub load: f32,
    /// The rev limiter's smoothed level multiplier 0..1.
    pub limiter: f32,
    /// The shift dip's smoothed level multiplier 0..1.
    pub dip: f32,
    /// The rev limiter is cutting this step.
    pub limiter_cutting: bool,
}

/// The fixed-step vehicle sound model (see the module docs). Build it once per vehicle with
/// [`VehicleAudioModel::new`]; [`VehicleAudioModel::step`] then allocates nothing.
#[derive(Clone, Debug)]
pub struct VehicleAudioModel {
    def: VehicleAudioDef,
    sounds: Vec<String>,
    rolling_sounds: Vec<u16>,
    on_rpms: Vec<f32>,
    off_rpms: Vec<f32>,
    voices: Vec<VehicleVoiceOut>,
    one_shots: [Option<VehicleOneShot>; ONE_SHOT_CAPACITY],
    one_shot_count: usize,
    dropped_one_shots: u64,
    params: VehicleAudioParams,
    started: bool,
    steps: u64,
    last_upshifts: u32,
    last_downshifts: u32,
    dip_left: f32,
    last_compression: [f32; MAX_AUDIO_WHEELS],
    bump_cooldown: [f32; MAX_AUDIO_WHEELS],
    shift_sound: Option<u16>,
    bump_sound: Option<u16>,
    shifts_fired: u64,
    bumps_fired: u64,
    cached_dt: f32,
    coefficients: [f32; 5],
}

impl VehicleAudioModel {
    /// A model for `def` with room for `wheels` wheels (at most [`MAX_AUDIO_WHEELS`]).
    ///
    /// # Errors
    /// [`VehicleAudioError::Value`] when `def` does not validate.
    pub fn new(def: VehicleAudioDef, wheels: usize) -> Result<Self, VehicleAudioError> {
        def.validate()?;
        let wheels = wheels.min(MAX_AUDIO_WHEELS);
        let mut sounds: Vec<String> = Vec::new();
        let mut intern = |name: &str| -> u16 {
            if let Some(i) = sounds.iter().position(|s| s == name) {
                return i as u16;
            }
            sounds.push(name.to_string());
            (sounds.len() - 1) as u16
        };
        let engine = &def.engine;
        let mut voices = Vec::new();
        let voice = |role, slot, sound| VehicleVoiceOut {
            role,
            slot,
            sound,
            weight: 0.0,
            amplitude: 0.0,
            pitch: 1.0,
        };
        for (i, layer) in engine.on_load.iter().enumerate() {
            voices.push(voice(
                VehicleVoiceRole::EngineOn(i as u8),
                engine.on_load_slot,
                intern(&layer.sound),
            ));
        }
        for (i, layer) in engine.off_load.iter().enumerate() {
            voices.push(voice(
                VehicleVoiceRole::EngineOff(i as u8),
                engine.off_load_slot,
                intern(&layer.sound),
            ));
        }
        if let Some(whine) = &def.whine {
            voices.push(voice(
                VehicleVoiceRole::Whine,
                VehicleEmitterSlot::Engine,
                intern(&whine.sound),
            ));
        }
        if let Some(wind) = &def.wind {
            voices.push(voice(
                VehicleVoiceRole::Wind,
                VehicleEmitterSlot::Engine,
                intern(&wind.sound),
            ));
        }
        let rolling_sounds: Vec<u16> = def.tires.rolling.iter().map(|r| intern(&r.sound)).collect();
        let squeal = def.tires.squeal.as_ref().map(|s| intern(&s.sound));
        for wheel in 0..wheels as u8 {
            if let Some(sound) = squeal {
                voices.push(voice(
                    VehicleVoiceRole::Squeal(wheel),
                    VehicleEmitterSlot::Wheel(wheel),
                    sound,
                ));
            }
            if let Some(&sound) = rolling_sounds.first() {
                voices.push(voice(
                    VehicleVoiceRole::Rolling(wheel),
                    VehicleEmitterSlot::Wheel(wheel),
                    sound,
                ));
            }
        }
        let shift_sound = engine.shift.as_ref().map(|s| intern(&s.sound));
        let bump_sound = def.tires.bump.as_ref().map(|b| intern(&b.sound));
        let on_rpms = engine.on_load.iter().map(|l| l.rpm).collect();
        let off_rpms = engine.off_load.iter().map(|l| l.rpm).collect();
        Ok(Self {
            def,
            sounds,
            rolling_sounds,
            on_rpms,
            off_rpms,
            voices,
            one_shots: [None; ONE_SHOT_CAPACITY],
            one_shot_count: 0,
            dropped_one_shots: 0,
            params: VehicleAudioParams {
                limiter: 1.0,
                dip: 1.0,
                ..VehicleAudioParams::default()
            },
            started: false,
            steps: 0,
            last_upshifts: 0,
            last_downshifts: 0,
            dip_left: 0.0,
            last_compression: [0.0; MAX_AUDIO_WHEELS],
            bump_cooldown: [0.0; MAX_AUDIO_WHEELS],
            shift_sound,
            bump_sound,
            shifts_fired: 0,
            bumps_fired: 0,
            cached_dt: f32::NAN,
            coefficients: [1.0; 5],
        })
    }

    /// The definition.
    #[must_use]
    pub fn def(&self) -> &VehicleAudioDef {
        &self.def
    }

    /// Every sound the model names, in first-use order (voices and one-shots index this).
    #[must_use]
    pub fn sounds(&self) -> &[String] {
        &self.sounds
    }

    /// The looping voices after the last step.
    #[must_use]
    pub fn voices(&self) -> &[VehicleVoiceOut] {
        &self.voices
    }

    /// The smoothed parameters after the last step.
    #[must_use]
    pub fn params(&self) -> VehicleAudioParams {
        self.params
    }

    /// Steps taken.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Shift one-shots fired since the model was built.
    #[must_use]
    pub fn shifts_fired(&self) -> u64 {
        self.shifts_fired
    }

    /// Bump one-shots fired since the model was built.
    #[must_use]
    pub fn bumps_fired(&self) -> u64 {
        self.bumps_fired
    }

    /// One-shots dropped because a step queued more than [`ONE_SHOT_CAPACITY`] before a drain.
    #[must_use]
    pub fn dropped_one_shots(&self) -> u64 {
        self.dropped_one_shots
    }

    /// The `tires.rolling` index for a surface name: the entry named so, else the one named
    /// `default`, else the first.
    #[must_use]
    pub fn surface_index(&self, surface: Option<&str>) -> u16 {
        let rolling = &self.def.tires.rolling;
        let find = |name: &str| rolling.iter().position(|r| r.surface == name);
        surface.and_then(find).or_else(|| find("default")).unwrap_or(0) as u16
    }

    /// Takes the one-shots queued since the last call (oldest first) into `out`, which is
    /// cleared first; no allocation when `out` has room for [`ONE_SHOT_CAPACITY`].
    pub fn drain_one_shots(&mut self, out: &mut Vec<VehicleOneShot>) {
        out.clear();
        for slot in self.one_shots.iter_mut().take(self.one_shot_count) {
            if let Some(shot) = slot.take() {
                out.push(shot);
            }
        }
        self.one_shot_count = 0;
    }

    fn push_one_shot(&mut self, shot: VehicleOneShot) {
        match self.one_shots.get_mut(self.one_shot_count) {
            Some(slot) => {
                *slot = Some(shot);
                self.one_shot_count += 1;
            }
            None => self.dropped_one_shots += 1,
        }
    }

    fn refresh_coefficients(&mut self, dt: f32) {
        if self.cached_dt.to_bits() == dt.to_bits() {
            return;
        }
        let s = &self.def.smoothing;
        self.coefficients = [
            one_pole_coefficient(s.rpm_secs, dt),
            one_pole_coefficient(s.load_secs, dt),
            one_pole_coefficient(s.gain_secs, dt),
            one_pole_coefficient(s.tire_secs, dt),
            one_pole_coefficient(LIMITER_SMOOTHING_SECS, dt),
        ];
        self.cached_dt = dt;
    }

    /// The per-step smoothing coefficients for `dt`: `[rpm, load, gain, tire, limiter]`.
    #[must_use]
    pub fn coefficients_for(&self, dt: f32) -> [f32; 5] {
        let s = &self.def.smoothing;
        [
            one_pole_coefficient(s.rpm_secs, dt),
            one_pole_coefficient(s.load_secs, dt),
            one_pole_coefficient(s.gain_secs, dt),
            one_pole_coefficient(s.tire_secs, dt),
            one_pole_coefficient(LIMITER_SMOOTHING_SECS, dt),
        ]
    }

    /// Advances the model one fixed step of `dt` seconds. The first step snaps rpm and load to
    /// their targets (a vehicle heard for the first time does not sweep up from 0 rpm) and only
    /// records the shift counters and suspension (no one-shots).
    pub fn step(&mut self, inputs: &VehicleAudioInputs, dt: f32) {
        self.advance(inputs, dt, false);
    }

    /// Like [`Self::step`] with every smoother snapped to its target: the steady state for
    /// `inputs` (the agent's offline probe). One-shots are not fired.
    pub fn settle(&mut self, inputs: &VehicleAudioInputs) {
        self.advance(inputs, 1.0 / 60.0, true);
    }

    fn advance(&mut self, inputs: &VehicleAudioInputs, dt: f32, snap: bool) {
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        self.refresh_coefficients(dt);
        let first = !self.started || snap;
        let [c_rpm, c_load, c_gain, c_tire, c_limiter] = if first { [1.0; 5] } else { self.coefficients };
        let engine = &self.def.engine;

        // Engine speed and load.
        let rpm_target = if inputs.rpm.is_finite() {
            inputs.rpm.max(0.0)
        } else {
            self.params.rpm
        };
        self.params.rpm = smooth(self.params.rpm, rpm_target, c_rpm);
        let load = if engine.off_load.is_empty() {
            1.0
        } else {
            load_target(engine, inputs.throttle, inputs.engine_torque)
        };
        self.params.load = smooth(self.params.load, load, c_load).clamp(0.0, 1.0);

        // Rev limiter: a fixed rhythm counted in steps, never a clock. The fastest rhythm a step
        // can show is one cut every other step (half the step rate); a faster `rate_hz` would
        // alias (at exactly the step rate every step lands at the same phase and the cut never
        // lifts), so it is held there.
        let limiter_rate = f64::from(engine.limiter.rate_hz).min(0.5 / f64::from(dt));
        let near_redline = rpm_target >= engine.redline_rpm - engine.limiter.margin_rpm;
        let throttle = if inputs.throttle.is_finite() {
            inputs.throttle
        } else {
            0.0
        };
        let cutting = !snap
            && near_redline
            && throttle >= engine.limiter.min_throttle
            && engine.limiter.rate_hz > 0.0
            && ((self.steps as f64 * f64::from(dt) * limiter_rate).fract() < 0.5);
        self.params.limiter_cutting = cutting;
        let limiter_target = if cutting {
            db_to_gain(engine.limiter.cut_db)
        } else {
            1.0
        };
        self.params.limiter = smooth(self.params.limiter, limiter_target, c_limiter);

        // Shifts: one one-shot per counted gear change, and the dip.
        let shift = engine
            .shift
            .as_ref()
            .map(|s| (s.on_downshift, s.dip_secs, db_to_gain(s.gain_db)));
        if self.started
            && !snap
            && let Some((on_downshift, dip_secs, amplitude)) = shift
        {
            // Each counter is a `u32`. Adding the two deltas in `u32` overflows (a debug panic,
            // or a release wrap to 0 so the dip never engages) and the old loop then walked every
            // counted shift. Count in `u64`, queue only what fits, and record the rest as drops.
            let ups = u64::from(inputs.upshifts.saturating_sub(self.last_upshifts));
            let downs = u64::from(inputs.downshifts.saturating_sub(self.last_downshifts));
            let counted = ups.saturating_add(downs);
            let fired = if on_downshift { counted } else { ups };
            if counted > 0 {
                self.dip_left = dip_secs;
            }
            if let Some(sound) = self.shift_sound {
                let room = ONE_SHOT_CAPACITY.saturating_sub(self.one_shot_count) as u64;
                let queued = fired.min(room);
                for _ in 0..queued {
                    self.push_one_shot(VehicleOneShot {
                        kind: VehicleOneShotKind::Shift,
                        slot: VehicleEmitterSlot::Engine,
                        sound,
                        amplitude,
                    });
                }
                self.dropped_one_shots += fired - queued;
                self.shifts_fired += fired;
            }
        }
        self.last_upshifts = inputs.upshifts;
        self.last_downshifts = inputs.downshifts;
        let engine = &self.def.engine;
        let dip_target = match &engine.shift {
            Some(shift) if self.dip_left > 0.0 => db_to_gain(shift.dip_db),
            _ => 1.0,
        };
        self.dip_left = (self.dip_left - dt).max(0.0);
        self.params.dip = smooth(self.params.dip, dip_target, c_gain);

        // Engine layers.
        let rpm = self.params.rpm;
        let master = db_to_gain(engine.volume_db) * self.params.limiter * self.params.dip;
        let on_gain = master * (self.params.load * std::f32::consts::FRAC_PI_2).sin();
        let off_gain =
            master * db_to_gain(engine.off_load_db) * (self.params.load * std::f32::consts::FRAC_PI_2).cos();
        let on = engine_crossfade(&self.on_rpms, rpm);
        let off = engine_crossfade(&self.off_rpms, rpm);
        let pitch_of = |recorded: f32| (rpm / recorded).clamp(engine.min_pitch, engine.max_pitch);
        let weight_of = |pair: &[(usize, f32); 2], index: usize| {
            pair.iter()
                .filter(|(i, _)| *i == index)
                .map(|(_, w)| *w)
                .fold(0.0f32, f32::max)
        };
        let whine_level = self.def.whine.as_ref().map(|whine| {
            let share = whine.off_load_share + (1.0 - whine.off_load_share) * self.params.load;
            (
                db_to_gain(polyline(&whine.gain_curve, rpm)) * share * self.params.limiter,
                (rpm / whine.recorded_rpm).clamp(engine.min_pitch, engine.max_pitch),
            )
        });
        let speed = if inputs.speed.is_finite() {
            inputs.speed.abs()
        } else {
            0.0
        };
        let wind_level = self.def.wind.as_ref().map(|wind| {
            (
                db_to_gain(polyline(&wind.gain_curve, speed)),
                (1.0 + speed * wind.pitch_per_mps).clamp(0.5, 2.0),
            )
        });
        let wheel_count = inputs.wheel_count.min(MAX_AUDIO_WHEELS);
        for voice in &mut self.voices {
            match voice.role {
                VehicleVoiceRole::EngineOn(i) => {
                    let i = usize::from(i);
                    voice.weight = weight_of(&on, i);
                    voice.amplitude =
                        on_gain * voice.weight * db_to_gain(engine.on_load.get(i).map_or(0.0, |l| l.gain_db));
                    voice.pitch = pitch_of(self.on_rpms.get(i).copied().unwrap_or(1.0));
                }
                VehicleVoiceRole::EngineOff(i) => {
                    let i = usize::from(i);
                    voice.weight = weight_of(&off, i);
                    voice.amplitude = off_gain
                        * voice.weight
                        * db_to_gain(engine.off_load.get(i).map_or(0.0, |l| l.gain_db));
                    voice.pitch = pitch_of(self.off_rpms.get(i).copied().unwrap_or(1.0));
                }
                VehicleVoiceRole::Whine => {
                    if let Some((amplitude, pitch)) = whine_level {
                        voice.weight = 1.0;
                        voice.amplitude = smooth(voice.amplitude, amplitude, c_gain);
                        voice.pitch = pitch;
                    }
                }
                VehicleVoiceRole::Wind => {
                    if let Some((amplitude, pitch)) = wind_level {
                        voice.weight = 1.0;
                        voice.amplitude = smooth(voice.amplitude, amplitude, c_gain);
                        voice.pitch = smooth(voice.pitch, pitch, c_gain);
                    }
                }
                VehicleVoiceRole::Squeal(w) => {
                    let wheel = usize::from(w);
                    let (target, pitch) = match (&self.def.tires.squeal, inputs.wheels.get(wheel)) {
                        (Some(def), Some(input)) if wheel < wheel_count && input.grounded => {
                            let amount = squeal_amount(def, input.slip_angle, input.slip_ratio);
                            (
                                db_to_gain(def.gain_db) * amount,
                                1.0 - def.pitch_spread * 0.5 + def.pitch_spread * amount,
                            )
                        }
                        _ => (0.0, voice.pitch),
                    };
                    voice.weight = 1.0;
                    voice.amplitude = smooth(voice.amplitude, target, c_tire);
                    voice.pitch = smooth(voice.pitch, pitch, c_tire);
                }
                VehicleVoiceRole::Rolling(w) => {
                    let wheel = usize::from(w);
                    let input = inputs.wheels.get(wheel).filter(|_| wheel < wheel_count);
                    let surface = input.map_or(0, |i| usize::from(i.surface));
                    let rolling = &self.def.tires.rolling;
                    let def = rolling.get(surface).or_else(|| rolling.first());
                    let surface = if surface < rolling.len() { surface } else { 0 };
                    let (target, pitch) = match (def, input) {
                        (Some(def), Some(input)) if input.grounded => {
                            let v = if input.contact_speed.is_finite() {
                                input.contact_speed.abs()
                            } else {
                                0.0
                            };
                            (
                                db_to_gain(polyline(&def.gain_curve, v)),
                                (1.0 + v * def.pitch_per_mps).clamp(0.5, 2.0),
                            )
                        }
                        _ => (0.0, voice.pitch),
                    };
                    // The surface's loop; while silent the voice keeps its sound (no swap churn).
                    if target > 0.0
                        && let Some(&sound) = self.rolling_sounds.get(surface)
                    {
                        voice.sound = sound;
                    }
                    voice.weight = 1.0;
                    voice.amplitude = smooth(voice.amplitude, target, c_tire);
                    voice.pitch = smooth(voice.pitch, pitch, c_tire);
                }
            }
            if !voice.amplitude.is_finite() {
                voice.amplitude = 0.0;
            }
            if !voice.pitch.is_finite() || voice.pitch <= 0.0 {
                voice.pitch = 1.0;
            }
        }

        // Suspension bumps from the compression rate.
        let bump = self
            .def
            .tires
            .bump
            .as_ref()
            .map(|b| (b.rate_threshold_mps, b.full_rate_mps, b.gain_db, b.cooldown_secs));
        if let Some((threshold, full, gain_db, cooldown)) = bump {
            for (wheel, input) in inputs.wheels.iter().enumerate().take(wheel_count) {
                let compression = if input.compression.is_finite() {
                    input.compression
                } else {
                    0.0
                };
                self.bump_cooldown[wheel] = (self.bump_cooldown[wheel] - dt).max(0.0);
                if self.started && !snap && dt > 0.0 && input.grounded {
                    let rate = (compression - self.last_compression[wheel]) / dt;
                    if rate > threshold && self.bump_cooldown[wheel] <= 0.0 {
                        let strength = ((rate - threshold) / (full - threshold)).clamp(0.0, 1.0);
                        if let Some(sound) = self.bump_sound {
                            self.push_one_shot(VehicleOneShot {
                                kind: VehicleOneShotKind::Bump,
                                slot: VehicleEmitterSlot::Wheel(wheel as u8),
                                sound,
                                amplitude: db_to_gain(gain_db) * (0.25 + 0.75 * strength),
                            });
                            self.bumps_fired += 1;
                        }
                        self.bump_cooldown[wheel] = cooldown;
                    }
                }
                self.last_compression[wheel] = compression;
            }
        }

        if !snap {
            self.started = true;
            self.steps += 1;
        }
    }

    /// FNV-1a over every smoothed parameter, voice (role, sound, amplitude and pitch bits) and
    /// queued one-shot: equal inputs stepped equally give equal digests.
    #[must_use]
    pub fn parameter_digest(&self) -> u64 {
        let mut h = 0xCBF2_9CE4_8422_2325_u64;
        let mut eat = |v: u32| {
            for byte in v.to_le_bytes() {
                h = (h ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3);
            }
        };
        let p = self.params;
        for v in [p.rpm, p.load, p.limiter, p.dip] {
            eat(v.to_bits());
        }
        eat(u32::from(p.limiter_cutting));
        for voice in &self.voices {
            eat(voice.role.code());
            eat(u32::from(voice.sound));
            eat(voice.weight.to_bits());
            eat(voice.amplitude.to_bits());
            eat(voice.pitch.to_bits());
        }
        for shot in self.one_shots.iter().take(self.one_shot_count).flatten() {
            eat(shot.kind as u32);
            eat(u32::from(shot.sound));
            eat(shot.amplitude.to_bits());
        }
        h
    }
}

// ---------------------------------------------------------------------------------------- voices

/// How many voices a vehicle may use (the voice budget, ADR-3271).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VehicleVoiceMode {
    /// Every layer the model makes audible.
    Full,
    /// One engine layer: the loudest (with hysteresis).
    Reduced,
    /// Nothing.
    #[default]
    Silent,
}

impl VehicleVoiceMode {
    /// The mode as a label (`full`, `reduced`, `silent`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Reduced => "reduced",
            Self::Silent => "silent",
        }
    }
}

/// The sound assets of a model, in [`VehicleAudioModel::sounds`] order (`None` = missing; its
/// voices stay silent).
#[derive(Clone, Default)]
pub struct VehicleSoundBank {
    /// One entry per model sound.
    pub assets: Vec<Option<SoundAsset>>,
    /// Names that did not resolve.
    pub missing: Vec<String>,
}

impl VehicleSoundBank {
    /// Resolves every sound of `model` with `resolve` (`None` = missing; logged once by the caller).
    pub fn resolve(model: &VehicleAudioModel, mut resolve: impl FnMut(&str) -> Option<SoundAsset>) -> Self {
        let mut missing = Vec::new();
        let assets = model
            .sounds()
            .iter()
            .map(|name| {
                let found = resolve(name);
                if found.is_none() {
                    missing.push(name.clone());
                }
                found
            })
            .collect();
        Self { assets, missing }
    }

    fn get(&self, index: u16) -> Option<&SoundAsset> {
        self.assets.get(usize::from(index)).and_then(Option::as_ref)
    }
}

/// The emitter entities of one vehicle, by slot.
#[derive(Clone, Debug, PartialEq)]
pub struct VehicleEmitterEntities {
    /// The engine-bay emitter.
    pub engine: regia_ecs::prelude::Entity,
    /// The tailpipe emitter.
    pub exhaust: regia_ecs::prelude::Entity,
    /// One emitter per wheel.
    pub wheels: Vec<regia_ecs::prelude::Entity>,
}

impl VehicleEmitterEntities {
    /// The entity of `slot` (`None` for a wheel the vehicle does not have).
    #[must_use]
    pub fn entity(&self, slot: VehicleEmitterSlot) -> Option<regia_ecs::prelude::Entity> {
        match slot {
            VehicleEmitterSlot::Engine => Some(self.engine),
            VehicleEmitterSlot::Exhaust => Some(self.exhaust),
            VehicleEmitterSlot::Wheel(i) => self.wheels.get(usize::from(i)).copied(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct LiveVoice {
    id: Option<SoundInstanceId>,
    sound: u16,
}

/// The live voices of one vehicle on the [`Audio`] facade (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct VehicleVoiceSet {
    live: Vec<LiveVoice>,
    reduced_pick: Option<usize>,
    started: u64,
    stopped: u64,
}

impl VehicleVoiceSet {
    /// Live (started, not yet stopped) voices.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.live.iter().filter(|v| v.id.is_some()).count()
    }

    /// The live voice of model voice `index`, if any.
    #[must_use]
    pub fn voice_id(&self, index: usize) -> Option<SoundInstanceId> {
        self.live.get(index).and_then(|v| v.id)
    }

    /// Voices started and stopped since creation.
    #[must_use]
    pub fn counters(&self) -> (u64, u64) {
        (self.started, self.stopped)
    }

    /// Brings the voices in line with `model` under `mode`: starts voices that became audible
    /// (on their slot's emitter entity, at mute, so the sync fades them in), stops the ones that
    /// went silent or that the mode no longer allows, and pushes each live voice's level and pitch
    /// through [`Audio::set_emitter_voice_layer`]. Starts and stops allocate in the mixer;
    /// a steady state allocates nothing.
    pub fn apply(
        &mut self,
        audio: &mut Audio,
        model: &VehicleAudioModel,
        bank: &VehicleSoundBank,
        entities: &VehicleEmitterEntities,
        emitter: &AudioEmitter,
        mode: VehicleVoiceMode,
    ) {
        let voices = model.voices();
        if self.live.len() != voices.len() {
            self.stop_all(audio);
            self.live = vec![LiveVoice::default(); voices.len()];
        }
        // Reduced: the loudest engine layer, changed only for a clearly louder one.
        self.reduced_pick = if mode == VehicleVoiceMode::Reduced {
            let loudest = voices
                .iter()
                .enumerate()
                .filter(|(_, v)| v.role.is_engine())
                .max_by(|a, b| a.1.amplitude.total_cmp(&b.1.amplitude).then(b.0.cmp(&a.0)))
                .map(|(i, _)| i);
            match (self.reduced_pick, loudest) {
                (Some(current), Some(best))
                    if voices.get(current).is_some_and(|v| {
                        v.role.is_engine() && voices[best].amplitude <= v.amplitude * REDUCED_SWITCH_RATIO
                    }) =>
                {
                    Some(current)
                }
                (_, best) => best,
            }
        } else {
            None
        };
        for (index, voice) in voices.iter().enumerate() {
            let live = self.live[index];
            if let Some(id) = live.id
                && !audio.voice_alive(id)
            {
                self.live[index].id = None;
            }
            let allowed = match mode {
                VehicleVoiceMode::Full => true,
                VehicleVoiceMode::Reduced => self.reduced_pick == Some(index),
                VehicleVoiceMode::Silent => false,
            };
            let running = self.live[index].id.is_some();
            let audible = if running {
                voice.amplitude > VOICE_STOP_AMPLITUDE
            } else {
                voice.amplitude > VOICE_START_AMPLITUDE
            };
            let wanted = allowed && audible && bank.get(voice.sound).is_some();
            if let Some(id) = self.live[index].id
                && (!wanted || self.live[index].sound != voice.sound)
            {
                audio.stop_voice(id, VOICE_STOP_FADE);
                self.live[index].id = None;
                self.stopped += 1;
            }
            if wanted && self.live[index].id.is_none() {
                let (Some(asset), Some(entity)) = (bank.get(voice.sound), entities.entity(voice.slot)) else {
                    continue;
                };
                match audio.start_emitter(entity, asset.clone(), emitter, MUTE_DB) {
                    Ok(id) => {
                        self.live[index] = LiveVoice {
                            id: Some(id),
                            sound: voice.sound,
                        };
                        self.started += 1;
                    }
                    Err(error) => {
                        tracing::debug!(target: "regia_audio", "vehicle voice did not start: {error}");
                    }
                }
            }
            if let Some(id) = self.live[index].id {
                audio.set_emitter_voice_layer(id, gain_to_db(voice.amplitude), voice.pitch);
            }
        }
    }

    /// Plays `shots` as positional one-shots at their slots' positions (`position_of`) with the
    /// vehicle's emitter settings.
    pub fn play_one_shots(
        audio: &mut Audio,
        shots: &[VehicleOneShot],
        bank: &VehicleSoundBank,
        emitter: &AudioEmitter,
        mut position_of: impl FnMut(VehicleEmitterSlot) -> Option<regia_core::math::Vec3>,
    ) -> usize {
        let mut played = 0;
        for shot in shots {
            let (Some(asset), Some(position)) = (bank.get(shot.sound), position_of(shot.slot)) else {
                continue;
            };
            let params = PlaybackParams {
                bus: Some(emitter.bus.clone().unwrap_or_else(|| "World".to_string())),
                volume_db: gain_to_db(shot.amplitude),
                ..PlaybackParams::default()
            };
            match audio.play_asset_at(asset, params, position, emitter) {
                Ok(()) => played += 1,
                Err(error) => tracing::debug!(target: "regia_audio", "vehicle one-shot not queued: {error}"),
            }
        }
        played
    }

    /// Stops every live voice (despawn, budget drop to nothing).
    pub fn stop_all(&mut self, audio: &mut Audio) {
        for live in &mut self.live {
            if let Some(id) = live.id.take() {
                audio.stop_voice(id, VOICE_STOP_FADE);
                self.stopped += 1;
            }
        }
        self.reduced_pick = None;
    }
}

impl Audio {
    /// Sets an attached emitter voice's own level (dB) and pitch ratio. The `AudioSync` emitter
    /// pass composes them with the voice's distance gain, occlusion and Doppler each frame (the
    /// level is the voice's base volume, the pitch multiplies the Doppler ratio), so a layered
    /// sound keeps every spatial effect. Non-finite values are ignored; the pitch is floored at
    /// 0.01. Allocation-free.
    pub fn set_emitter_voice_layer(&mut self, id: SoundInstanceId, volume_db: f32, pitch: f32) {
        if let Some(entry) = self.voices.get_mut(id) {
            if volume_db.is_finite() {
                entry.base_volume_db = volume_db.max(MUTE_DB);
            }
            if pitch.is_finite() {
                entry.params.pitch = pitch.max(0.01);
            }
        }
    }
}

// ----------------------------------------------------------------------------------- placeholders

/// Generates a placeholder sound named `name` (`placeholder:engine_on_<rpm>`,
/// `placeholder:engine_off_<rpm>`, `placeholder:whine_<rpm>`, `placeholder:squeal`,
/// `placeholder:roll_asphalt`, `placeholder:roll_gravel`, `placeholder:wind`, `placeholder:shift`,
/// `placeholder:bump`). Returns `None` for any other name. These are generated test signals, not
/// recordings: an engine loop is a harmonic stack at the firing frequency of a four-cylinder
/// four-stroke (`rpm / 60 × 4 / 2`) with a half-order rumble, rendered by `loopgen` (seamless,
/// deterministic); the others are filtered noise, and the one-shots are decaying noise bursts.
///
/// # Errors
/// An [`AudioError`] when the generator rejects its own graph (a bug).
pub fn placeholder_sound(name: &str) -> Option<Result<SoundAsset, AudioError>> {
    let key = name.strip_prefix(PLACEHOLDER_PREFIX)?;
    let rpm_of = |prefix: &str| {
        key.strip_prefix(prefix)
            .and_then(|r| r.parse::<f32>().ok())
            .filter(|r| *r > 0.0)
    };
    let def = if let Some(rpm) = rpm_of("engine_on_") {
        engine_loop(key, rpm, true)
    } else if let Some(rpm) = rpm_of("engine_off_") {
        engine_loop(key, rpm, false)
    } else if let Some(rpm) = rpm_of("whine_") {
        // A gear whine at 12× the crank rate, a little detuned against its octave.
        let hz = rpm / 60.0 * 12.0;
        let (length, _) = whole_cycles(hz);
        loop_def(
            key,
            length,
            vec![
                node(
                    "a",
                    LoopNode::Osc {
                        shape: OscShape::Sine,
                        hz,
                        detune_cents: 0.0,
                        gain: 0.6,
                        phase: 0.0,
                    },
                ),
                node(
                    "b",
                    LoopNode::Osc {
                        shape: OscShape::Sine,
                        hz: hz * 2.0,
                        detune_cents: 0.0,
                        gain: 0.2,
                        phase: 0.25,
                    },
                ),
                mix("out", &[("a", 1.0), ("b", 1.0)]),
            ],
        )
    } else {
        match key {
            "squeal" => loop_def(
                key,
                1.0,
                vec![
                    node(
                        "n",
                        LoopNode::Noise {
                            color: NoiseColor::White,
                            gain: 1.0,
                        },
                    ),
                    node(
                        "bp",
                        LoopNode::Filter {
                            input: "n".into(),
                            mode: FilterMode::BandPass,
                            cutoff_hz: 1400.0,
                            q: 12.0,
                        },
                    ),
                    node(
                        "out",
                        LoopNode::Am {
                            input: "bp".into(),
                            rate_hz: 9.0,
                            depth: 0.3,
                        },
                    ),
                ],
            ),
            "roll_asphalt" | "roll_gravel" => {
                let gravel = key == "roll_gravel";
                loop_def(
                    key,
                    1.0,
                    vec![
                        node(
                            "n",
                            LoopNode::Noise {
                                color: if gravel {
                                    NoiseColor::Pink
                                } else {
                                    NoiseColor::Brown
                                },
                                gain: 1.0,
                            },
                        ),
                        node(
                            "out",
                            LoopNode::Filter {
                                input: "n".into(),
                                mode: FilterMode::LowPass,
                                cutoff_hz: if gravel { 3500.0 } else { 600.0 },
                                q: std::f32::consts::FRAC_1_SQRT_2,
                            },
                        ),
                    ],
                )
            }
            "wind" => loop_def(
                key,
                2.0,
                vec![
                    node(
                        "n",
                        LoopNode::Noise {
                            color: NoiseColor::Pink,
                            gain: 0.8,
                        },
                    ),
                    node(
                        "bp",
                        LoopNode::Filter {
                            input: "n".into(),
                            mode: FilterMode::BandPass,
                            cutoff_hz: 700.0,
                            q: 0.7,
                        },
                    ),
                    node(
                        "out",
                        LoopNode::Am {
                            input: "bp".into(),
                            rate_hz: 0.5,
                            depth: 0.4,
                        },
                    ),
                ],
            ),
            "shift" => return Some(burst(0x5_41F7, 0.09, 0.025, 0.5, 0.6)),
            "bump" => return Some(burst(0xB0_4B, 0.18, 0.05, 0.08, 0.8)),
            _ => return None,
        }
    };
    Some(def.render().map(|r| r.to_sound_asset()).map_err(AudioError::from))
}

/// A loop length near 1 s holding an even number of whole cycles of `hz` (so `hz / 2` loops too).
fn whole_cycles(hz: f32) -> (f32, f32) {
    let cycles = (2.0 * (hz / 2.0).round()).max(2.0);
    (cycles / hz, cycles)
}

fn engine_loop(name: &str, rpm: f32, on_load: bool) -> LoopGenDef {
    let firing = rpm / 60.0 * 2.0; // four cylinders, four strokes: two firings per turn
    let (length, _) = whole_cycles(firing);
    let osc = |id: &str, shape, hz: f32, gain: f32| {
        node(
            id,
            LoopNode::Osc {
                shape,
                hz,
                detune_cents: 0.0,
                gain,
                phase: 0.0,
            },
        )
    };
    let mut nodes = vec![
        osc(
            "f",
            if on_load { OscShape::Saw } else { OscShape::Sine },
            firing,
            0.35,
        ),
        osc(
            "h2",
            OscShape::Sine,
            firing * 2.0,
            if on_load { 0.22 } else { 0.08 },
        ),
        osc(
            "h3",
            OscShape::Sine,
            firing * 3.0,
            if on_load { 0.12 } else { 0.03 },
        ),
        osc("half", OscShape::Sine, firing * 0.5, 0.25),
        node(
            "r",
            LoopNode::Rumble {
                cutoff_hz: firing * 4.0,
                q: std::f32::consts::FRAC_1_SQRT_2,
                gain: if on_load { 0.25 } else { 0.12 },
            },
        ),
        node(
            "pulse",
            LoopNode::Am {
                input: "r".into(),
                // Loopgen's AM rate stops at 200 Hz: the half order (the loop holds whole cycles of it).
                rate_hz: (firing * 0.5).min(200.0),
                depth: 0.7,
            },
        ),
        mix(
            "sum",
            &[
                ("f", 1.0),
                ("h2", 1.0),
                ("h3", 1.0),
                ("half", 1.0),
                ("pulse", 1.0),
            ],
        ),
    ];
    nodes.push(node(
        "out",
        LoopNode::Filter {
            input: "sum".into(),
            mode: FilterMode::LowPass,
            cutoff_hz: if on_load { 3000.0 } else { 1000.0 },
            q: std::f32::consts::FRAC_1_SQRT_2,
        },
    ));
    loop_def(name, length, nodes)
}

fn node(id: &str, node: LoopNode) -> LoopNodeDef {
    LoopNodeDef { id: id.into(), node }
}

fn mix(id: &str, inputs: &[(&str, f32)]) -> LoopNodeDef {
    node(
        id,
        LoopNode::Mix {
            inputs: inputs
                .iter()
                .map(|(input, gain)| LoopMixInput {
                    input: (*input).into(),
                    gain: *gain,
                })
                .collect(),
        },
    )
}

fn loop_def(name: &str, length_secs: f32, nodes: Vec<LoopNodeDef>) -> LoopGenDef {
    LoopGenDef {
        version: crate::loopgen::LOOPGEN_VERSION,
        name: format!("placeholder {name}"),
        sample_rate: 48_000,
        length_secs: length_secs.clamp(0.05, 4.0),
        channels: 1,
        seed: 0x07E4_1C1E,
        crossfade_secs: 0.03,
        output_gain_db: -3.0,
        nodes,
        output: "out".into(),
    }
}

/// A decaying noise burst (one-shot placeholder): `seconds` long, decaying by 60 dB over `decay`
/// seconds after a short attack, low-passed by a one-pole with coefficient `smooth`.
fn burst(seed: u64, seconds: f32, decay: f32, smooth: f32, peak: f32) -> Result<SoundAsset, AudioError> {
    let rate = 48_000u32;
    let len = (seconds * rate as f32) as usize;
    let mut rng = Rng::new(seed);
    // −60 dB over `decay` seconds: a per-sample factor of 10^(−3 / (decay × rate)).
    let factor = 10.0f32.powf(-3.0 / (decay * rate as f32));
    let attack = (0.002 * rate as f32) as usize;
    let mut envelope = peak;
    let mut low = 0.0f32;
    let samples: Vec<f32> = (0..len)
        .map(|i| {
            let white = rng.f32() * 2.0 - 1.0;
            low += smooth * (white - low);
            let ramp = if i < attack {
                i as f32 / attack.max(1) as f32
            } else {
                1.0
            };
            let s = low * envelope * ramp;
            if i >= attack {
                envelope *= factor;
            }
            s.clamp(-1.0, 1.0)
        })
        .collect();
    SoundClip::new(samples, rate)
        .map(|clip| SoundAsset::from_clip(&clip))
        .map_err(|e| AudioError::Invalid(format!("placeholder burst: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 120.0;

    fn model() -> VehicleAudioModel {
        VehicleAudioModel::new(VehicleAudioDef::placeholder(), 4).expect("the placeholder validates")
    }

    fn inputs(rpm: f32, throttle: f32, torque: f32) -> VehicleAudioInputs {
        let mut i = VehicleAudioInputs {
            rpm,
            throttle,
            engine_torque: torque,
            speed: 10.0,
            wheel_count: 4,
            ..VehicleAudioInputs::default()
        };
        for w in &mut i.wheels[..4] {
            w.grounded = true;
            w.contact_speed = 10.0;
        }
        i
    }

    #[test]
    fn crossfade_is_equal_power_over_the_sweep_and_exact_on_recorded_rpms() {
        let def = VehicleAudioDef::placeholder();
        let rpms: Vec<f32> = def.engine.on_load.iter().map(|l| l.rpm).collect();
        let mut rpm = 800.0f32;
        while rpm <= 7000.0 {
            let [(_, w0), (_, w1)] = engine_crossfade(&rpms, rpm);
            assert!((w0 * w0 + w1 * w1 - 1.0).abs() < 1e-5, "{rpm}: {w0} {w1}");
            assert!(w0 >= 0.0 && w1 >= 0.0);
            rpm += 0.5;
        }
        for (index, &recorded) in rpms.iter().enumerate() {
            let [(i0, w0), (_, w1)] = engine_crossfade(&rpms, recorded);
            assert_eq!(
                (i0, w0, w1),
                (index, 1.0, 0.0),
                "exactly (1, 0) at {recorded} rpm"
            );
        }
        assert_eq!(engine_crossfade(&[], 3000.0), [(0, 0.0), (0, 0.0)]);
        assert_eq!(engine_crossfade(&rpms, f32::NAN)[0], (0, 1.0));
    }

    #[test]
    fn the_model_layers_satisfy_equal_power_within_each_set() {
        let mut m = model();
        let mut rpm = 800.0f32;
        while rpm <= 7000.0 {
            m.settle(&inputs(rpm, 1.0, 300.0));
            let on: f32 = m
                .voices()
                .iter()
                .filter(|v| matches!(v.role, VehicleVoiceRole::EngineOn(_)))
                .map(|v| v.weight * v.weight)
                .sum();
            assert!((on - 1.0).abs() < 1e-5, "{rpm}: {on}");
            let active = m
                .voices()
                .iter()
                .filter(|v| matches!(v.role, VehicleVoiceRole::EngineOn(_)) && v.weight > 0.0)
                .count();
            assert!((1..=2).contains(&active));
            rpm += 25.0;
        }
    }

    #[test]
    fn pitch_follows_a_linear_ramp_continuously() {
        let mut m = model();
        let [c_rpm, ..] = m.coefficients_for(DT);
        let steps = 360; // 3 s
        let mut last_rpm = 800.0f32;
        let mut last: Vec<f32> = Vec::new();
        m.step(&inputs(800.0, 1.0, 300.0), DT);
        for step in 1..=steps {
            let target = 800.0 + 6200.0 * step as f32 / steps as f32;
            last.clear();
            last.extend(m.voices().iter().map(|v| v.pitch));
            m.step(&inputs(target, 1.0, 300.0), DT);
            let rpm = m.params().rpm;
            let bound = c_rpm * (target - last_rpm).abs() + 1e-3;
            assert!(
                (rpm - last_rpm).abs() <= bound,
                "step {step}: rpm moved {}",
                rpm - last_rpm
            );
            for (voice, before) in m.voices().iter().zip(&last) {
                let recorded = match voice.role {
                    VehicleVoiceRole::EngineOn(i) => m.def().engine.on_load[usize::from(i)].rpm,
                    VehicleVoiceRole::EngineOff(i) => m.def().engine.off_load[usize::from(i)].rpm,
                    _ => continue,
                };
                let change = (voice.pitch - before).abs();
                assert!(
                    change <= (rpm - last_rpm).abs() / recorded + 1e-5,
                    "step {step} {:?}: pitch jumped by {change}",
                    voice.role
                );
                assert!(
                    change < 0.02,
                    "a 2,067 rpm/s ramp moves no pitch more than 2 % per step"
                );
            }
            last_rpm = rpm;
        }
        assert!(m.params().rpm > 6900.0, "the smoothed rpm tracks the ramp");
    }

    #[test]
    fn the_load_switch_is_smoothed() {
        let mut m = model();
        let [_, c_load, ..] = m.coefficients_for(DT);
        m.step(&inputs(3000.0, 1.0, 300.0), DT);
        assert_eq!(m.params().load, 1.0, "the first step snaps");
        let mut previous = m.params().load;
        for step in 0..120 {
            m.step(&inputs(3000.0, 0.0, -40.0), DT);
            let load = m.params().load;
            assert!(
                (load - previous).abs() <= c_load * previous + 1e-6,
                "step {step}: {previous} -> {load}"
            );
            assert!(load <= previous + 1e-7, "monotonic toward off load");
            previous = load;
        }
        assert!(previous < 0.01, "settles off load, got {previous}");
        let off: f32 = m
            .voices()
            .iter()
            .filter(|v| matches!(v.role, VehicleVoiceRole::EngineOff(_)))
            .map(|v| v.amplitude)
            .sum();
        let on: f32 = m
            .voices()
            .iter()
            .filter(|v| matches!(v.role, VehicleVoiceRole::EngineOn(_)))
            .map(|v| v.amplitude)
            .sum();
        assert!(off > 10.0 * on, "off-load set dominates: off {off}, on {on}");
    }

    #[test]
    fn load_target_mixes_throttle_and_torque() {
        let engine = VehicleAudioDef::placeholder().engine;
        assert_eq!(load_target(&engine, 0.0, -100.0), 0.0);
        assert!((load_target(&engine, 1.0, 300.0) - 1.0).abs() < 1e-6);
        assert!((load_target(&engine, 1.0, 0.0) - 0.6).abs() < 1e-6);
        assert_eq!(load_target(&engine, f32::NAN, f32::NAN), 0.0);
    }

    #[test]
    fn squeal_is_zero_below_threshold_and_monotonic_above() {
        let def = VehicleAudioDef::placeholder()
            .tires
            .squeal
            .expect("placeholder has squeal");
        let mut previous = 0.0f32;
        let mut first_audible = None;
        for i in 0..=400 {
            let angle = (i as f32 * 0.05).to_radians(); // 0 to 20 degrees
            let s = squeal_amount(&def, angle, 0.0);
            let x = angle.to_degrees() / def.slip_angle_peak_deg;
            if x <= def.threshold {
                assert_eq!(s, 0.0, "below threshold at {} deg", angle.to_degrees());
            } else if first_audible.is_none() {
                first_audible = Some(i);
            }
            assert!(s >= previous, "monotonic in slip angle");
            previous = s;
        }
        assert!(first_audible.is_some() && previous == 1.0);
        let mut previous = 0.0f32;
        for i in 0..=100 {
            let s = squeal_amount(&def, 0.0, i as f32 * 0.005);
            assert!(s >= previous, "monotonic in slip ratio");
            previous = s;
        }
        assert!(
            squeal_amount(&def, 0.05, 0.1) >= squeal_amount(&def, 0.05, 0.0),
            "combined slip adds"
        );
        assert_eq!(squeal_amount(&def, f32::NAN, 0.0), 0.0);
        assert_eq!(
            squeal_amount(&def, -0.3, -0.5),
            squeal_amount(&def, 0.3, 0.5),
            "symmetric"
        );
    }

    #[test]
    fn upshifts_fire_one_shot_each_and_dip_the_engine() {
        let mut m = model();
        let mut shots = Vec::with_capacity(ONE_SHOT_CAPACITY);
        let mut i = inputs(5000.0, 1.0, 300.0);
        m.step(&i, DT);
        let before = m.voices().iter().map(|v| v.amplitude).sum::<f32>();
        i.upshifts = 1;
        m.step(&i, DT);
        m.drain_one_shots(&mut shots);
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].kind, VehicleOneShotKind::Shift);
        for _ in 0..6 {
            m.step(&i, DT);
        }
        assert!(
            m.params().dip < 0.7,
            "the dip lowers the engine: {}",
            m.params().dip
        );
        let during: f32 = m
            .voices()
            .iter()
            .filter(|v| v.role.is_engine())
            .map(|v| v.amplitude)
            .sum();
        assert!(during < before);
        // Two more upshifts in one step, a downshift: two shots (downshifts are off by default).
        i.upshifts = 3;
        i.downshifts = 1;
        m.step(&i, DT);
        m.drain_one_shots(&mut shots);
        assert_eq!(shots.len(), 2);
        assert_eq!(m.shifts_fired(), 3);
        for _ in 0..60 {
            m.step(&i, DT);
        }
        assert!((m.params().dip - 1.0).abs() < 1e-3, "the dip recovers");
    }

    #[test]
    fn the_first_step_fires_nothing_for_counters_already_raised() {
        let mut m = model();
        let mut i = inputs(3000.0, 0.5, 100.0);
        i.upshifts = 4;
        m.step(&i, DT);
        let mut shots = Vec::new();
        m.drain_one_shots(&mut shots);
        assert!(
            shots.is_empty(),
            "a car heard for the first time replays no old shifts"
        );
    }

    /// A counter jump of `u32::MAX` upshifts plus one downshift must not overflow the `u32` sum
    /// (debug panic) and must not walk the delta. The queue keeps [`ONE_SHOT_CAPACITY`] shots;
    /// the rest count as dropped, and `shifts_fired` counts every one.
    #[test]
    fn a_shift_counter_jump_does_not_overflow_and_drops_the_excess() {
        let mut m = model();
        let mut i = inputs(2000.0, 0.4, 40.0);
        m.step(&i, DT);
        i.upshifts = u32::MAX;
        i.downshifts = 1;
        m.step(&i, DT);
        let mut shots = Vec::new();
        m.drain_one_shots(&mut shots);
        assert_eq!(shots.len(), ONE_SHOT_CAPACITY);
        assert!(shots.iter().all(|s| s.kind == VehicleOneShotKind::Shift));
        let fired = u64::from(u32::MAX);
        assert_eq!(m.shifts_fired(), fired);
        assert_eq!(m.dropped_one_shots(), fired - ONE_SHOT_CAPACITY as u64);
        for _ in 0..8 {
            m.step(&i, DT);
        }
        assert!(m.params().dip < 0.7, "the dip still engages: {}", m.params().dip);
        assert_eq!(m.shifts_fired(), fired, "later steps do not count the jump again");
    }

    #[test]
    fn the_rev_limiter_cuts_in_rhythm_without_steps() {
        let mut m = model();
        let [.., c_limiter] = m.coefficients_for(DT);
        // The first step snaps every smoother; the bound applies from the second.
        m.step(&inputs(6990.0, 1.0, 200.0), DT);
        let mut cuts = usize::from(m.params().limiter_cutting);
        let mut previous = m.params().limiter;
        for _ in 1..240 {
            m.step(&inputs(6990.0, 1.0, 200.0), DT);
            let p = m.params();
            cuts += usize::from(p.limiter_cutting);
            let bound = c_limiter * (previous - db_to_gain(-14.0)).abs().max((1.0 - previous).abs()) + 1e-6;
            assert!((p.limiter - previous).abs() <= bound);
            previous = p.limiter;
        }
        assert!((100..=140).contains(&cuts), "half the steps cut at 16 Hz: {cuts}");
        let mut m = model();
        for _ in 0..240 {
            m.step(&inputs(6990.0, 0.2, 0.0), DT);
            assert!(!m.params().limiter_cutting, "no cut without throttle");
        }
    }

    /// A limiter rate at or above the step rate (valid: up to 100 Hz) used to alias: at exactly
    /// the step rate every step landed at the same phase, so the cut never lifted. It is held at
    /// one cut every other step.
    #[test]
    fn a_limiter_rate_past_half_the_step_rate_still_cuts_in_rhythm() {
        for rate_hz in [60.0f32, 100.0] {
            let mut def = VehicleAudioDef::placeholder();
            def.engine.limiter.rate_hz = rate_hz;
            let mut m = VehicleAudioModel::new(def, 4).expect("valid");
            let dt = 1.0 / 60.0;
            let mut cuts = 0usize;
            for _ in 0..240 {
                m.step(&inputs(6990.0, 1.0, 200.0), dt);
                cuts += usize::from(m.params().limiter_cutting);
            }
            assert!(
                (100..=140).contains(&cuts),
                "{rate_hz} Hz at a 60 Hz step: about half the steps cut, got {cuts} of 240"
            );
        }
        // A rate under half the step rate is unchanged.
        let mut m = model();
        let mut cuts = 0usize;
        for _ in 0..240 {
            m.step(&inputs(6990.0, 1.0, 200.0), DT);
            cuts += usize::from(m.params().limiter_cutting);
        }
        assert!((100..=140).contains(&cuts), "16 Hz at 120 Hz: {cuts}");
    }

    #[test]
    fn bumps_fire_on_fast_compression_with_a_cooldown() {
        let mut m = model();
        let mut i = inputs(2000.0, 0.3, 50.0);
        m.step(&i, DT);
        let mut shots = Vec::new();
        // 0.05 m in one 1/120 s step = 6 m/s: a hard bump on wheel 2.
        i.wheels[2].compression = 0.05;
        m.step(&i, DT);
        m.drain_one_shots(&mut shots);
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].slot, VehicleEmitterSlot::Wheel(2));
        assert_eq!(shots[0].kind, VehicleOneShotKind::Bump);
        i.wheels[2].compression = 0.10;
        m.step(&i, DT);
        m.drain_one_shots(&mut shots);
        assert!(shots.is_empty(), "within the cooldown");
        // Slow compression never bumps.
        for k in 0..100 {
            i.wheels[1].compression = k as f32 * 0.001;
            m.step(&i, DT);
        }
        m.drain_one_shots(&mut shots);
        assert!(shots.is_empty());
    }

    #[test]
    fn rolling_follows_the_surface_and_speed() {
        let mut m = model();
        let gravel = m.surface_index(Some("gravel"));
        assert_eq!(
            m.surface_index(Some("ice")),
            m.surface_index(None),
            "unknown surfaces fall back"
        );
        let mut i = inputs(2000.0, 0.3, 50.0);
        i.wheels[0].surface = gravel;
        m.settle(&i);
        let roll = |m: &VehicleAudioModel, wheel: u8| {
            *m.voices()
                .iter()
                .find(|v| v.role == VehicleVoiceRole::Rolling(wheel))
                .expect("a rolling voice per wheel")
        };
        let gravel_sound = m
            .sounds()
            .iter()
            .position(|s| s.ends_with("roll_gravel"))
            .expect("named");
        assert_eq!(usize::from(roll(&m, 0).sound), gravel_sound);
        assert_ne!(usize::from(roll(&m, 1).sound), gravel_sound);
        assert!(roll(&m, 0).amplitude > roll(&m, 1).amplitude, "gravel is louder");
        i.wheels[1].grounded = false;
        m.settle(&i);
        assert_eq!(roll(&m, 1).amplitude, 0.0, "a wheel in the air does not roll");
    }

    #[test]
    fn equal_inputs_give_equal_digests() {
        let run = |throttle: f32| {
            let mut m = model();
            for step in 0..600 {
                let rpm = 900.0 + step as f32 * 10.0;
                let mut i = inputs(rpm.min(7000.0), throttle, 250.0);
                i.upshifts = step / 150;
                i.wheels[0].slip_angle = (step as f32 * 0.001).sin() * 0.2;
                m.step(&i, DT);
            }
            m.parameter_digest()
        };
        assert_eq!(run(1.0), run(1.0));
        assert_ne!(run(1.0), run(0.5));
    }

    #[test]
    fn non_finite_inputs_never_reach_the_outputs() {
        let mut m = model();
        m.step(&inputs(3000.0, 1.0, 300.0), DT);
        let mut bad = inputs(f32::NAN, f32::INFINITY, f32::NAN);
        bad.speed = f32::NAN;
        bad.wheels[0].slip_angle = f32::NAN;
        bad.wheels[0].compression = f32::INFINITY;
        bad.wheels[0].contact_speed = f32::NEG_INFINITY;
        for _ in 0..10 {
            m.step(&bad, f32::NAN);
            m.step(&bad, DT);
        }
        for v in m.voices() {
            assert!(
                v.amplitude.is_finite() && v.pitch.is_finite() && v.pitch > 0.0,
                "{v:?}"
            );
        }
        assert!(m.params().rpm.is_finite());
    }

    #[test]
    fn the_format_round_trips_and_rejects_bad_files() {
        let def = VehicleAudioDef::placeholder();
        let text = def.to_ron().expect("writes");
        assert_eq!(VehicleAudioDef::from_ron(&text).expect("reads back"), def);
        let newer = text.replacen("format_version: 1", "format_version: 2", 1);
        assert_eq!(
            VehicleAudioDef::from_ron(&newer).expect_err("newer").code(),
            "vehicle_audio_version"
        );
        assert_eq!(
            VehicleAudioDef::from_ron("(nonsense").expect_err("parse").code(),
            "vehicle_audio_parse"
        );
        let mut unordered = def.clone();
        unordered.engine.on_load.swap(0, 1);
        let err = unordered.validate().expect_err("rpm order");
        assert_eq!(err.code(), "vehicle_audio_value");
        assert!(err.to_string().contains("engine.on_load[1].rpm"), "{err}");
        let mut empty = def.clone();
        empty.engine.on_load.clear();
        assert!(VehicleAudioModel::new(empty, 4).is_err());
        let mut wheel_engine = def;
        wheel_engine.engine.on_load_slot = VehicleEmitterSlot::Wheel(0);
        assert!(wheel_engine.validate().is_err());
        let minimal = r#"(format_version: 1, name: "m", engine: (on_load: [(rpm: 3000.0, sound: "e")]))"#;
        let parsed = VehicleAudioDef::from_ron(minimal).expect("defaults fill the rest");
        assert_eq!(parsed.engine.redline_rpm, 7000.0);
    }

    #[test]
    fn the_fingerprint_names_nested_fields() {
        let f = vehicle_audio_schema_fingerprint();
        for field in [
            "format_version",
            "on_load",
            "off_load",
            "slip_angle_peak_deg",
            "rolling",
            "Exhaust",
        ] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
        assert!(vehicle_audio_migration_sources().is_empty());
    }

    #[test]
    fn every_placeholder_sound_is_generated() {
        let m = model();
        for name in m.sounds() {
            let asset = placeholder_sound(name)
                .expect("a placeholder name")
                .expect("renders");
            assert!(asset.duration_secs() > 0.01, "{name}");
        }
        assert!(placeholder_sound("engine.ogg").is_none());
        assert!(placeholder_sound("placeholder:unknown").is_none());
    }

    #[test]
    fn one_pole_coefficient_edges() {
        assert_eq!(one_pole_coefficient(0.0, DT), 1.0);
        assert_eq!(one_pole_coefficient(0.1, 0.0), 0.0);
        assert_eq!(one_pole_coefficient(0.1, f32::NAN), 0.0);
        let c = one_pole_coefficient(0.03, DT);
        assert!(c > 0.0 && c < 1.0);
        // After τ (ten steps of τ/10) the smoother covers 1 − 1/e of a step.
        let c = one_pole_coefficient(0.03, 0.003);
        let mut v = 0.0f32;
        for _ in 0..10 {
            v = smooth(v, 1.0, c);
        }
        assert!((v - (1.0 - (-1.0f32).exp())).abs() < 1e-4, "{v}");
    }

    /// A limiter margin at or past the redline puts the cut at or below 0 rpm, so the engine cuts
    /// at idle whenever the throttle is open. Validation must refuse it (it accepted any margin
    /// up to 5000 rpm whatever the redline).
    #[test]
    fn a_limiter_margin_at_or_past_the_redline_is_refused() {
        for margin in [3_000.0f32, 4_000.0] {
            let mut def = VehicleAudioDef::placeholder();
            def.engine.redline_rpm = 3_000.0;
            def.engine.limiter.margin_rpm = margin;
            let err = def.validate().expect_err("a margin at or past the redline");
            assert_eq!(err.code(), "vehicle_audio_value");
            assert!(err.to_string().contains("engine.limiter.margin_rpm"), "{err}");
        }
        let mut def = VehicleAudioDef::placeholder();
        def.engine.redline_rpm = 3_000.0;
        def.engine.limiter.margin_rpm = 2_999.0;
        def.validate().expect("a margin below the redline is valid");
    }

    /// `squeal_amount` is monotonic in the slip. A huge but finite slip overflowed `a² + k²` to
    /// infinity in f32 and the non-finite guard returned 0: full slip went silent.
    #[test]
    fn a_huge_finite_slip_is_full_squeal_not_silence() {
        let def = VehicleAudioDef::placeholder()
            .tires
            .squeal
            .expect("placeholder has squeal");
        assert_eq!(squeal_amount(&def, 0.0, 1.0e18), 1.0, "no overflow yet");
        assert_eq!(squeal_amount(&def, 0.0, 1.0e20), 1.0, "slip ratio");
        assert_eq!(squeal_amount(&def, 1.0e19, 0.0), 1.0, "slip angle");
        assert_eq!(squeal_amount(&def, 1.0e19, 1.0e20), 1.0, "both");
        assert_eq!(squeal_amount(&def, f32::NAN, 0.0), 0.0, "NaN is still no slip");
    }
}
