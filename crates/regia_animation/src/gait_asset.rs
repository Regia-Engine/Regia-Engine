// SPDX-License-Identifier: MPL-2.0
//! `.gait.ron` gait settings files (format `gait` v1, gap G7): a walker authored and tuned as data.
//!
//! A [`GaitFile`] names everything the stepper in [`crate::gait`] needs: the gait patterns (a preset
//! or explicit phase offsets and duty factor) with the speeds that switch between them, the speed
//! limits, step length, lift and clearance, slope and reach limits, settle and turn thresholds, the
//! foothold search, the ground band, the optional body posture (height smoothing and tilt limits),
//! and the legs by joint name with optional hip offset and reach overrides. See
//! `docs/formats/gait-ron.md`.
//!
//! [`GaitAsset::from_ron`] reads `format_version` first (a newer file is refused with
//! `gait_version`), parses the typed document (unknown fields refused), builds the
//! [`GaitParams`] and checks them with the stepper's own [`GaitParams::validate`], so a bad file
//! fails with the stepper's codes (`gait_settings`, `gait_pattern`). Only fields whose absence
//! means "off" have defaults (no game tuning defaults): no pattern switching, no reach limit, any
//! ground height, the upright body, no body joint, no hip offset or reach override.
//!
//! [`GaitAssets`] is the runtime store: it loads files (or text) into a
//! [`regia_asset::Assets`] slab and hands out [`Handle`]s. A [`crate::gait_rig::GaitRig`] built
//! with [`crate::gait_rig::GaitRig::from_asset`] keeps its handle; when the file changes on disk
//! (a [`SourceAssetChanged`] message, forwarded by the dev watcher), [`GaitAssets`] reads it
//! again and validates it, and every rig on that handle takes the new settings on its next
//! frame. A reload that fails keeps the settings in use and records the error
//! ([`GaitAssets::error`]).
//!
//! Version 1 is the first published shape, so the migrator registry is empty
//! ([`gait_migration_sources`]); [`migrate_gait_ron`] passes v1 through and refuses newer files.
//!
//! Presentation data: nothing here is read by a simulation checksum.

use std::path::{Path, PathBuf};

use regia_asset::{AssetId, Assets, Handle, SourceAssetChanged};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::gait::{GaitError, GaitParams, GaitPattern, GaitPatternSet, GaitPosture, MAX_LEGS, MAX_PATTERNS};
use crate::gait_rig::GaitLeg;

/// The format name in `docs/formats/format-hashes.ron`, `cargo xtask check-formats` and
/// `regia upgrade-project`.
pub const GAIT_FORMAT: &str = "gait";

/// The `.gait.ron` format version this build writes and the newest it reads.
pub const GAIT_FORMAT_VERSION: u32 = 1;

/// The file name suffix of a gait settings file.
pub const GAIT_SUFFIX: &str = ".gait.ron";

/// One gait pattern of a [`GaitFile`]: a preset by name or explicit offsets and duty. An unknown
/// field inside `Explicit(..)` is refused (`gait_parse`) like on every other gait struct.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum GaitPatternSpec {
    /// [`GaitPattern::tripod`]: six legs, duty 0.5.
    Tripod,
    /// [`GaitPattern::ripple`] for this many legs (even, 4 to 16).
    Ripple(u8),
    /// [`GaitPattern::wave`] for this many legs (even, 2 to 16).
    Wave(u8),
    /// [`GaitPattern::quadruped_walk`]: four legs, duty 0.75.
    QuadrupedWalk,
    /// [`GaitPattern::trot`]: four legs, duty 0.5.
    Trot,
    /// [`GaitPattern::pace`]: four legs, duty 0.5.
    Pace,
    /// [`GaitPattern::bound`]: four legs, duty 0.4.
    Bound,
    /// [`GaitPattern::biped_walk`]: two legs, duty 0.6.
    BipedWalk,
    /// Explicit per-foot phase offsets (one per leg, each in `[0, 1)`) and a duty factor in
    /// `(0, 1)`.
    Explicit {
        /// Phase offset of each foot in cycles, in leg order.
        offsets: Vec<f32>,
        /// Fraction of the cycle each foot is in stance.
        duty: f32,
    },
}

impl GaitPatternSpec {
    /// The pattern this spec names, validated.
    ///
    /// # Errors
    ///
    /// [`GaitError::Pattern`] for a leg count the preset does not support, or offsets or a duty
    /// factor out of range.
    pub fn build(&self) -> Result<GaitPattern, GaitError> {
        let pattern = match self {
            Self::Tripod => GaitPattern::tripod(),
            Self::Ripple(legs) => GaitPattern::ripple(usize::from(*legs))?,
            Self::Wave(legs) => GaitPattern::wave(usize::from(*legs))?,
            Self::QuadrupedWalk => GaitPattern::quadruped_walk(),
            Self::Trot => GaitPattern::trot(),
            Self::Pace => GaitPattern::pace(),
            Self::Bound => GaitPattern::bound(),
            Self::BipedWalk => GaitPattern::biped_walk(),
            Self::Explicit { offsets, duty } => GaitPattern::new(offsets, *duty)?,
        };
        pattern.validate()?;
        Ok(pattern)
    }

    /// The explicit spec of `pattern` (its used offsets and duty factor).
    #[must_use]
    pub fn explicit(pattern: &GaitPattern) -> Self {
        let legs = pattern.leg_count().min(MAX_LEGS);
        Self::Explicit {
            offsets: pattern.phase_offset[..legs].to_vec(),
            duty: pattern.duty_factor,
        }
    }

    /// Every preset name with an example leg count, in the order of the format page.
    #[must_use]
    pub fn presets() -> Vec<Self> {
        vec![
            Self::Tripod,
            Self::Ripple(6),
            Self::Wave(6),
            Self::QuadrupedWalk,
            Self::Trot,
            Self::Pace,
            Self::Bound,
            Self::BipedWalk,
        ]
    }
}

/// Speed limits of a [`GaitFile`] (meters per second, radians per second).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GaitSpeeds {
    /// [`GaitParams::forward_speed`].
    pub forward: f32,
    /// [`GaitParams::reverse_speed`].
    pub reverse: f32,
    /// [`GaitParams::strafe_speed`].
    pub strafe: f32,
    /// [`GaitParams::acceleration`].
    pub acceleration: f32,
    /// [`GaitParams::idle_speed`]: at or below it the gait settles and turns in place.
    pub idle: f32,
    /// [`GaitParams::max_yaw_rate`].
    pub yaw_rate: f32,
}

/// Idle settle thresholds of a [`GaitFile`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GaitSettle {
    /// [`GaitParams::settle_distance`], meters.
    pub distance: f32,
    /// [`GaitParams::settle_period`], seconds.
    pub period: f32,
}

/// Turn-in-place thresholds of a [`GaitFile`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GaitTurn {
    /// [`GaitParams::turn_step_distance`], meters.
    pub step_distance: f32,
    /// [`GaitParams::turn_step_angle`], radians in `[0, pi]`.
    pub step_angle: f32,
}

/// Foothold search of a [`GaitFile`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GaitFoothold {
    /// [`GaitParams::foothold_candidates`]: ring points tried around a refused landing; 0 turns
    /// the search off.
    pub candidates: u32,
    /// [`GaitParams::foothold_spacing`], meters between rings.
    pub spacing: f32,
}

/// Body height smoothing and tilt limits of a [`GaitFile`] ([`GaitPosture`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GaitPostureSpec {
    /// [`GaitPosture::swing_weight`].
    pub swing_weight: f32,
    /// [`GaitPosture::height_tau`], seconds.
    pub height_tau: f32,
    /// [`GaitPosture::height_slew`], meters per second; `None` for no limit.
    #[serde(default)]
    pub height_slew: Option<f32>,
    /// [`GaitPosture::attitude_weight`].
    pub attitude_weight: f32,
    /// [`GaitPosture::max_pitch`], radians.
    pub max_pitch: f32,
    /// [`GaitPosture::max_roll`], radians.
    pub max_roll: f32,
    /// [`GaitPosture::attitude_tau`], seconds.
    pub attitude_tau: f32,
}

impl GaitPostureSpec {
    /// The posture this spec names.
    #[must_use]
    pub fn posture(&self) -> GaitPosture {
        GaitPosture {
            swing_weight: self.swing_weight,
            height_tau: self.height_tau,
            height_slew: self.height_slew.unwrap_or(f32::INFINITY),
            attitude_weight: self.attitude_weight,
            max_pitch: self.max_pitch,
            max_roll: self.max_roll,
            attitude_tau: self.attitude_tau,
        }
    }

    /// The spec of `posture`, or `None` for [`GaitPosture::UPRIGHT`].
    #[must_use]
    pub fn of(posture: &GaitPosture) -> Option<Self> {
        if *posture == GaitPosture::UPRIGHT {
            return None;
        }
        Some(Self {
            swing_weight: posture.swing_weight,
            height_tau: posture.height_tau,
            height_slew: finite(posture.height_slew),
            attitude_weight: posture.attitude_weight,
            max_pitch: posture.max_pitch,
            max_roll: posture.max_roll,
            attitude_tau: posture.attitude_tau,
        })
    }
}

/// One leg of a [`GaitFile`], in pattern order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GaitLegSpec {
    /// Chain root joint (the hip or coxa).
    pub root: String,
    /// Chain tip joint (the foot).
    pub tip: String,
    /// The stepper's `hip_offsets` entry for this leg (see the frame table in [`crate::gait`]);
    /// `None` lets a [`crate::gait_rig::GaitRig`] measure it from the bind pose (0 for a host
    /// that calls the stepper itself).
    #[serde(default)]
    pub hip_offset: Option<[f32; 2]>,
    /// This leg's reach limit in meters, overriding [`GaitFile::max_reach`].
    #[serde(default)]
    pub max_reach: Option<f32>,
}

/// The `.gait.ron` document (format `gait` v1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GaitFile {
    /// Format version of this file.
    pub format_version: u32,
    /// Patterns slowest first, 1 to 4, every one for the same number of legs.
    pub patterns: Vec<GaitPatternSpec>,
    /// Ground speeds between neighboring patterns (one fewer than `patterns`); empty for one
    /// pattern.
    #[serde(default)]
    pub switch_speeds: Vec<f32>,
    /// Half-width of the band around each switch speed in which the current pattern holds;
    /// 0 with one pattern.
    #[serde(default)]
    pub hysteresis: f32,
    /// Speed limits.
    pub speeds: GaitSpeeds,
    /// [`GaitParams::step_length`], meters of body travel per stance.
    pub step_length: f32,
    /// [`GaitParams::swing_lift`], meters.
    pub swing_lift: f32,
    /// [`GaitParams::clearance`], meters of body height above the stance floor.
    pub clearance: f32,
    /// [`GaitParams::max_slope`], rise over run.
    pub max_slope: f32,
    /// Reach limit of every leg in meters ([`GaitParams::max_reach`]); `None` for no limit.
    #[serde(default)]
    pub max_reach: Option<f32>,
    /// Idle settle thresholds.
    pub settle: GaitSettle,
    /// Turn-in-place thresholds.
    pub turn: GaitTurn,
    /// [`GaitParams::ground_band`], meters; `None` accepts any height (single-layer worlds).
    #[serde(default)]
    pub ground_band: Option<f32>,
    /// Foothold search.
    pub foothold: GaitFoothold,
    /// [`GaitParams::hang_drop`], meters.
    pub hang_drop: f32,
    /// Body height smoothing and tilt; `None` keeps the upright body ([`GaitPosture::UPRIGHT`]).
    #[serde(default)]
    pub posture: Option<GaitPostureSpec>,
    /// Joint a [`crate::gait_rig::GaitRig`] lifts to the stance height; `None` leaves the body.
    #[serde(default)]
    pub body: Option<String>,
    /// The legs in pattern order, as many as the patterns' leg count.
    pub legs: Vec<GaitLegSpec>,
}

fn finite(value: f32) -> Option<f32> {
    value.is_finite().then_some(value)
}

/// Why a gait file is refused. [`Self::code`] is the agent's lowercase error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum GaitAssetError {
    /// The text is not a `.gait.ron` document (RON syntax, a missing or unknown field, a wrong
    /// type).
    #[error("gait_parse: {0}")]
    Parse(String),
    /// The file's `format_version` is newer than this build reads (or 0).
    #[error("gait_version: format_version {found} is not supported (this build reads 1 to {supported})")]
    Version {
        /// The version in the file.
        found: u32,
        /// The newest this build reads.
        supported: u32,
    },
    /// The settings fail the stepper's own checks: [`GaitError::Settings`] (`gait_settings`) or
    /// [`GaitError::Pattern`] (`gait_pattern`).
    #[error("{0}")]
    Gait(GaitError),
    /// The leg list does not match the patterns: a different count, or an empty joint name.
    #[error("gait_legs: {0}")]
    Legs(String),
    /// The file could not be read or written.
    #[error("gait_io: {0}")]
    Io(String),
}

impl GaitAssetError {
    /// The lowercase error code: `gait_parse`, `gait_version`, `gait_settings`, `gait_pattern`,
    /// `gait_legs` or `gait_io` (and, never from a file, the stepper's other codes).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "gait_parse",
            Self::Version { .. } => "gait_version",
            Self::Gait(error) => gait_error_code(*error),
            Self::Legs(_) => "gait_legs",
            Self::Io(_) => "gait_io",
        }
    }
}

/// The lowercase code of a stepper error (the prefix of its message).
#[must_use]
pub fn gait_error_code(error: GaitError) -> &'static str {
    match error {
        GaitError::Settings => "gait_settings",
        GaitError::Input => "gait_input",
        GaitError::State => "gait_state",
        GaitError::Floor => "gait_floor",
        GaitError::StepTooLarge => "gait_step_too_large",
        GaitError::Pattern => "gait_pattern",
    }
}

impl From<GaitError> for GaitAssetError {
    fn from(error: GaitError) -> Self {
        Self::Gait(error)
    }
}

/// The serde field-list fingerprint of the format (for `cargo xtask check-formats`).
#[must_use]
pub fn gait_schema_fingerprint() -> String {
    use regia_core::schema::{
        join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes,
    };
    join_field_lists(&[
        serde_field_names::<GaitFile>(),
        serde_field_names::<GaitSpeeds>(),
        serde_field_names::<GaitSettle>(),
        serde_field_names::<GaitTurn>(),
        serde_field_names::<GaitFoothold>(),
        serde_field_names::<GaitPostureSpec>(),
        serde_field_names::<GaitLegSpec>(),
        serde_variant_names::<GaitPatternSpec>(),
        serde_variant_shapes::<GaitPatternSpec>(),
    ])
}

/// `from` versions of the registered migrations (none: v1 is the first version).
#[must_use]
pub fn gait_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Bring `.gait.ron` text to the current version: v1 passes through unchanged.
///
/// # Errors
///
/// `gait_parse` when `format_version` cannot be read, `gait_version` for a version this build
/// does not read.
pub fn migrate_gait_ron(text: &str) -> Result<String, GaitAssetError> {
    let version = read_version(text)?;
    if version == GAIT_FORMAT_VERSION {
        return Ok(text.to_string());
    }
    Err(GaitAssetError::Version {
        found: version,
        supported: GAIT_FORMAT_VERSION,
    })
}

fn read_version(text: &str) -> Result<u32, GaitAssetError> {
    regia_core::version::peek_ron_version(text)
        .map_err(|error| GaitAssetError::Parse(format!("cannot read format_version: {error}")))
}

impl GaitFile {
    /// Parse `.gait.ron` text without checking the settings. `format_version` is read first:
    /// a version this build does not read is `gait_version` before any other error.
    ///
    /// # Errors
    ///
    /// `gait_parse` or `gait_version`.
    pub fn from_ron(text: &str) -> Result<Self, GaitAssetError> {
        let version = read_version(text)?;
        if version != GAIT_FORMAT_VERSION && !gait_migration_sources().contains(&version) {
            return Err(GaitAssetError::Version {
                found: version,
                supported: GAIT_FORMAT_VERSION,
            });
        }
        ron::from_str(text).map_err(|error| GaitAssetError::Parse(error.to_string()))
    }

    /// Canonical RON at the current version.
    ///
    /// # Errors
    ///
    /// `gait_parse` when serialization fails. RON writes `inf` and `NaN`, so a nonfinite number
    /// that the stepper allows (an unbounded `ground_band`, say) round-trips.
    pub fn to_ron(&self) -> Result<String, GaitAssetError> {
        let mut file = self.clone();
        file.format_version = GAIT_FORMAT_VERSION;
        ron::ser::to_string_pretty(&file, regia_core::ron_pretty())
            .map_err(|error| GaitAssetError::Parse(error.to_string()))
    }

    /// The file that loads to `params` (every used entry bit for bit), with `legs` as the leg
    /// list: every pattern explicit, every hip offset written, the shared reach limit taken from
    /// the first unused entry and a per-leg reach written where a leg differs from it.
    #[must_use]
    pub fn from_params(params: &GaitParams, legs: &[GaitLeg], body: Option<&str>) -> Self {
        let set = &params.patterns;
        let count = usize::from(set.count).clamp(1, MAX_PATTERNS);
        let leg_count = params.legs();
        let shared = params.max_reach.get(leg_count).copied().and_then(finite);
        let shared_bits = shared.unwrap_or(f32::INFINITY).to_bits();
        Self {
            format_version: GAIT_FORMAT_VERSION,
            patterns: set.patterns[..count]
                .iter()
                .map(GaitPatternSpec::explicit)
                .collect(),
            switch_speeds: set.switch_speeds[..count - 1].to_vec(),
            hysteresis: set.hysteresis,
            speeds: GaitSpeeds {
                forward: params.forward_speed,
                reverse: params.reverse_speed,
                strafe: params.strafe_speed,
                acceleration: params.acceleration,
                idle: params.idle_speed,
                yaw_rate: params.max_yaw_rate,
            },
            step_length: params.step_length,
            swing_lift: params.swing_lift,
            clearance: params.clearance,
            max_slope: params.max_slope,
            max_reach: shared,
            settle: GaitSettle {
                distance: params.settle_distance,
                period: params.settle_period,
            },
            turn: GaitTurn {
                step_distance: params.turn_step_distance,
                step_angle: params.turn_step_angle,
            },
            ground_band: finite(params.ground_band),
            foothold: GaitFoothold {
                candidates: params.foothold_candidates,
                spacing: params.foothold_spacing,
            },
            hang_drop: params.hang_drop,
            posture: GaitPostureSpec::of(&params.posture),
            body: body.map(str::to_string),
            legs: (0..leg_count)
                .map(|i| {
                    let leg = legs.get(i);
                    GaitLegSpec {
                        root: leg.map(|l| l.root.clone()).unwrap_or_default(),
                        tip: leg.map(|l| l.tip.clone()).unwrap_or_default(),
                        hip_offset: Some(params.hip_offsets[i]),
                        max_reach: (params.max_reach[i].to_bits() != shared_bits)
                            .then_some(params.max_reach[i]),
                    }
                })
                .collect(),
        }
    }

    /// The stepper settings this file names, checked with [`GaitParams::validate`].
    ///
    /// # Errors
    ///
    /// `gait_version` for a file of another version, `gait_pattern` for a bad pattern list or
    /// switch speeds, `gait_legs` for a leg list that does not match, `gait_settings` for any
    /// other setting out of its range.
    pub fn params(&self) -> Result<GaitParams, GaitAssetError> {
        if self.format_version != GAIT_FORMAT_VERSION {
            return Err(GaitAssetError::Version {
                found: self.format_version,
                supported: GAIT_FORMAT_VERSION,
            });
        }
        if self.patterns.is_empty() || self.patterns.len() > MAX_PATTERNS {
            return Err(GaitError::Pattern.into());
        }
        let mut patterns = [GaitPattern::tripod(); MAX_PATTERNS];
        for (slot, spec) in patterns.iter_mut().zip(&self.patterns) {
            *slot = spec.build()?;
        }
        let set = GaitPatternSet::new(
            &patterns[..self.patterns.len()],
            &self.switch_speeds,
            self.hysteresis,
        )?;
        let legs = set.leg_count();
        if self.legs.len() != legs {
            return Err(GaitAssetError::Legs(format!(
                "{} legs listed, the patterns drive {legs}",
                self.legs.len()
            )));
        }
        if let Some(i) = self
            .legs
            .iter()
            .position(|leg| leg.root.is_empty() || leg.tip.is_empty())
        {
            return Err(GaitAssetError::Legs(format!("leg {i} has an empty joint name")));
        }
        let mut hip_offsets = [[0.0; 2]; MAX_LEGS];
        // Entries past the leg count are ignored by the stepper; they hold the shared limit.
        let mut max_reach = [self.max_reach.unwrap_or(f32::INFINITY); MAX_LEGS];
        for (i, leg) in self.legs.iter().enumerate() {
            hip_offsets[i] = leg.hip_offset.unwrap_or([0.0; 2]);
            if let Some(reach) = leg.max_reach {
                max_reach[i] = reach;
            }
        }
        let params = GaitParams {
            forward_speed: self.speeds.forward,
            reverse_speed: self.speeds.reverse,
            strafe_speed: self.speeds.strafe,
            acceleration: self.speeds.acceleration,
            step_length: self.step_length,
            clearance: self.clearance,
            swing_lift: self.swing_lift,
            max_slope: self.max_slope,
            idle_speed: self.speeds.idle,
            max_yaw_rate: self.speeds.yaw_rate,
            hip_offsets,
            settle_distance: self.settle.distance,
            settle_period: self.settle.period,
            turn_step_distance: self.turn.step_distance,
            turn_step_angle: self.turn.step_angle,
            ground_band: self.ground_band.unwrap_or(f32::INFINITY),
            foothold_candidates: self.foothold.candidates,
            foothold_spacing: self.foothold.spacing,
            max_reach,
            hang_drop: self.hang_drop,
            patterns: set,
            posture: self
                .posture
                .as_ref()
                .map_or(GaitPosture::UPRIGHT, GaitPostureSpec::posture),
        };
        params.validate()?;
        Ok(params)
    }
}

/// A loaded, validated `.gait.ron`: the document as written and the stepper settings it names.
#[derive(Clone, Debug, PartialEq)]
pub struct GaitAsset {
    /// The document as written.
    pub file: GaitFile,
    /// The validated stepper settings.
    pub params: GaitParams,
}

impl GaitAsset {
    /// Check `file` and build its settings.
    ///
    /// # Errors
    ///
    /// See [`GaitFile::params`].
    pub fn from_file(file: GaitFile) -> Result<Self, GaitAssetError> {
        let params = file.params()?;
        Ok(Self { file, params })
    }

    /// Parse and check `.gait.ron` text.
    ///
    /// # Errors
    ///
    /// `gait_parse`, `gait_version`, `gait_pattern`, `gait_legs` or `gait_settings`.
    pub fn from_ron(text: &str) -> Result<Self, GaitAssetError> {
        Self::from_file(GaitFile::from_ron(text)?)
    }

    /// Canonical RON of the document.
    ///
    /// # Errors
    ///
    /// See [`GaitFile::to_ron`].
    pub fn to_ron(&self) -> Result<String, GaitAssetError> {
        self.file.to_ron()
    }

    /// The legs as a [`crate::gait_rig::GaitRig`] takes them (allocates; call on load).
    #[must_use]
    pub fn legs(&self) -> Vec<GaitLeg> {
        self.file
            .legs
            .iter()
            .map(|leg| GaitLeg::new(leg.root.clone(), leg.tip.clone()))
            .collect()
    }

    /// Bit `i` is set when leg `i` has a hip offset in the file: a [`crate::gait_rig::GaitRig`]
    /// keeps that offset instead of measuring it from the bind pose.
    #[must_use]
    pub fn hip_override_mask(&self) -> u16 {
        self.file
            .legs
            .iter()
            .take(MAX_LEGS)
            .enumerate()
            .filter(|(_, leg)| leg.hip_offset.is_some())
            .fold(0, |mask, (i, _)| mask | (1 << i))
    }
}

/// Read and check a `.gait.ron` file.
///
/// # Errors
///
/// `gait_io` when the file cannot be read, else see [`GaitAsset::from_ron`].
pub fn load_gait_file(path: &Path) -> Result<GaitAsset, GaitAssetError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| GaitAssetError::Io(format!("{}: {error}", path.display())))?;
    GaitAsset::from_ron(&text)
}

/// One stored gait: where it came from and its reload history.
#[derive(Debug)]
struct GaitEntry {
    handle: Handle<GaitAsset>,
    key: String,
    file: Option<PathBuf>,
    generation: u64,
    error: Option<GaitAssetError>,
}

/// The gait settings the app has loaded, by key (a path), with [`Handle`]s for
/// [`crate::gait_rig::GaitRig::from_asset`]. Installed by [`crate::AnimationPlugin`].
#[derive(Resource, Default)]
pub struct GaitAssets {
    assets: Assets<GaitAsset>,
    entries: Vec<GaitEntry>,
}

impl std::fmt::Debug for GaitAssets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GaitAssets")
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

impl GaitAssets {
    /// Parse, check and store `text` under `key` (a path or any name). Storing the same key
    /// again replaces the settings in place: every handle to it reads the new value and its
    /// generation goes up. A refused text keeps what was stored and records the error.
    ///
    /// # Errors
    ///
    /// See [`GaitAsset::from_ron`].
    pub fn load_text(&mut self, key: &str, text: &str) -> Result<Handle<GaitAsset>, GaitAssetError> {
        self.store(key, None, GaitAsset::from_ron(text))
    }

    /// Read, check and store the file at `path` (keyed by the path as given). A later
    /// [`SourceAssetChanged`] for that file reads it again.
    ///
    /// # Errors
    ///
    /// See [`load_gait_file`].
    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<Handle<GaitAsset>, GaitAssetError> {
        let path = path.as_ref();
        let key = path.to_string_lossy().into_owned();
        self.store(&key, Some(path.to_path_buf()), load_gait_file(path))
    }

    /// Store an asset built in code under `key`.
    pub fn insert(&mut self, key: &str, asset: GaitAsset) -> Handle<GaitAsset> {
        match self.store(key, None, Ok(asset)) {
            Ok(handle) => handle,
            // `store` only fails when given an error.
            Err(_) => Handle::INVALID,
        }
    }

    fn store(
        &mut self,
        key: &str,
        file: Option<PathBuf>,
        loaded: Result<GaitAsset, GaitAssetError>,
    ) -> Result<Handle<GaitAsset>, GaitAssetError> {
        let index = self.entries.iter().position(|entry| entry.key == key);
        match loaded {
            Ok(asset) => {
                let handle = self
                    .assets
                    .insert(AssetId::sub_asset(AssetId::default(), key), asset);
                match index {
                    Some(i) => {
                        let entry = &mut self.entries[i];
                        entry.generation = entry.generation.saturating_add(1);
                        entry.error = None;
                        if file.is_some() {
                            entry.file = file;
                        }
                    }
                    None => self.entries.push(GaitEntry {
                        handle: handle.clone(),
                        key: key.to_string(),
                        file,
                        generation: 1,
                        error: None,
                    }),
                }
                Ok(handle)
            }
            Err(error) => {
                tracing::warn!(target: "regia_animation", key, code = error.code(), "gait settings refused: {error}");
                if let Some(i) = index {
                    self.entries[i].error = Some(error.clone());
                }
                Err(error)
            }
        }
    }

    /// The settings behind `handle`.
    #[must_use]
    pub fn get(&self, handle: &Handle<GaitAsset>) -> Option<&GaitAsset> {
        self.assets.get(handle)
    }

    /// The handle stored under `key`.
    #[must_use]
    pub fn handle(&self, key: &str) -> Option<Handle<GaitAsset>> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.handle.clone())
    }

    /// How many times `handle`'s settings were stored (1 after the first load, 0 for an unknown
    /// handle). A rig on the handle takes the settings again when this changes.
    #[must_use]
    pub fn generation(&self, handle: &Handle<GaitAsset>) -> u64 {
        self.entry(handle).map_or(0, |entry| entry.generation)
    }

    /// The error of the last refused reload of `handle`, cleared by the next good one.
    #[must_use]
    pub fn error(&self, handle: &Handle<GaitAsset>) -> Option<&GaitAssetError> {
        self.entry(handle).and_then(|entry| entry.error.as_ref())
    }

    /// Stored keys in load order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.key.as_str())
    }

    fn entry(&self, handle: &Handle<GaitAsset>) -> Option<&GaitEntry> {
        self.entries.iter().find(|entry| entry.handle == *handle)
    }

    /// Read again every stored file `changed` names (exact path, or the stored path ends with
    /// it). Returns how many were read.
    pub fn reload_changed(&mut self, changed: &Path) -> usize {
        let mut read = 0;
        for i in 0..self.entries.len() {
            let Some(file) = self.entries[i].file.clone() else {
                continue;
            };
            if file == changed || changed.ends_with(&file) || file.ends_with(changed) {
                let key = self.entries[i].key.clone();
                let loaded = load_gait_file(&file);
                let _ = self.store(&key, Some(file), loaded);
                read += 1;
            }
        }
        read
    }
}

/// `PreUpdate`: reads a changed `.gait.ron` again (forwarded by the dev watcher as
/// [`SourceAssetChanged`]). Allocates only when a gait file changed.
pub fn reload_gait_assets(
    messages: Option<Res<Messages<SourceAssetChanged>>>,
    mut cursor: Local<MessageCursor<SourceAssetChanged>>,
    gaits: Option<ResMut<GaitAssets>>,
) {
    let (Some(messages), Some(mut gaits)) = (messages, gaits) else {
        return;
    };
    for change in cursor.read(&messages) {
        let is_gait = change
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.to_ascii_lowercase().ends_with(GAIT_SUFFIX));
        if is_gait {
            let _span = tracing::debug_span!("gait_asset_reload").entered();
            gaits.reload_changed(&change.path);
        }
    }
}

pub(crate) fn register(app: &mut regia_app::App) {
    use regia_app::PreUpdate;
    app.init_resource::<GaitAssets>()
        .add_systems(PreUpdate, reload_gait_assets);
}
