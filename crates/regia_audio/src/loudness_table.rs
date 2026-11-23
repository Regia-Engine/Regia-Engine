// SPDX-License-Identifier: MPL-2.0
//! Loudness at import (`docs/specs/95-loudness-importers.md` § 2, ADR-1855): the per-sound loudness
//! record the cook writes, the gain policy, and the `loudness.ron` table a host hands to
//! [`SoundAssets::set_loudness_normalization`](crate::SoundAssets::set_loudness_normalization).
//!
//! The measurement is the [`LoudnessMeter`](crate::LoudnessMeter) (ITU-R BS.1770-4 / EBU R128)
//! run over the frames the runtime decoder delivers: always stereo, so a mono file is measured as
//! the dual-mono signal the mixer plays (`dual_mono`). A clip shorter than one 400 ms gating
//! block has no gated loudness; its record holds the ungated loudness of the whole clip
//! (`short_clip`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::AudioError;
use crate::loudness::{LoudnessMeter, MAX_NORMALIZE_GAIN_DB, REFERENCE_LUFS};
use crate::sound::SoundAsset;

/// Current `loudness.ron` format version.
pub const LOUDNESS_TABLE_VERSION: u32 = 1;

/// File name of the loudness table in a cooked cache directory.
pub const LOUDNESS_TABLE_FILE: &str = "loudness.ron";

/// Default true-peak ceiling after normalization, dBTP (EBU R128's maximum true-peak level).
pub const DEFAULT_TRUE_PEAK_CEILING_DB: f32 = -1.0;

/// The project's normalization policy: the target loudness and the true-peak ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoudnessPolicy {
    /// Integrated loudness every normalized sound is brought to, LUFS.
    pub target_lufs: f32,
    /// Highest true peak a normalized sound may reach, dBTP: the gain is lowered to keep it.
    pub true_peak_ceiling_db: f32,
}

impl Default for LoudnessPolicy {
    fn default() -> Self {
        Self {
            target_lufs: REFERENCE_LUFS,
            true_peak_ceiling_db: DEFAULT_TRUE_PEAK_CEILING_DB,
        }
    }
}

impl LoudnessPolicy {
    /// The accepted target range, LUFS.
    pub const TARGET_RANGE: (f32, f32) = (-70.0, 0.0);
    /// The accepted ceiling range, dBTP.
    pub const CEILING_RANGE: (f32, f32) = (-30.0, 0.0);

    /// `None` when both values are finite and inside [`Self::TARGET_RANGE`] and
    /// [`Self::CEILING_RANGE`]; otherwise what is wrong.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        let inside = |v: f32, (lo, hi): (f32, f32)| v.is_finite() && (lo..=hi).contains(&v);
        if !inside(self.target_lufs, Self::TARGET_RANGE) {
            return Some(format!(
                "target_lufs {} is outside {}..={}",
                self.target_lufs,
                Self::TARGET_RANGE.0,
                Self::TARGET_RANGE.1
            ));
        }
        if !inside(self.true_peak_ceiling_db, Self::CEILING_RANGE) {
            return Some(format!(
                "true_peak_ceiling_db {} is outside {}..={}",
                self.true_peak_ceiling_db,
                Self::CEILING_RANGE.0,
                Self::CEILING_RANGE.1
            ));
        }
        None
    }
}

/// One sound's loudness record: the measurement and the normalization gain the policy gives it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SoundLoudness {
    /// Gated integrated loudness, LUFS (the ungated whole-clip loudness for a `short_clip`);
    /// `None` for silence.
    pub integrated_lufs: Option<f32>,
    /// Loudness range (EBU Tech 3342), LU; 0 for clips shorter than two short-term windows.
    pub loudness_range_lu: f32,
    /// True peak (4× oversampled), dBTP; `None` for an all-zero clip.
    pub true_peak_db: Option<f32>,
    /// Shorter than one 400 ms gating block: `integrated_lufs` is ungated.
    pub short_clip: bool,
    /// Every frame has identical left and right samples (a mono source as the decoder plays it):
    /// the single-channel BS.1770 loudness is `integrated_lufs − 3.0103`.
    pub dual_mono: bool,
    /// Sample rate of the decoded frames, Hz.
    pub sample_rate: u32,
    /// Decoded stereo frames.
    pub frames: u64,
    /// Whether normalization applies to this sound (its `.meta` may opt out).
    pub normalize: bool,
    /// The target the gain was computed for, LUFS.
    pub target_lufs: f32,
    /// The normalization gain, dB (0 when `normalize` is off or the sound is silent).
    pub gain_db: f32,
    /// The true-peak ceiling lowered the gain below `target − integrated`.
    pub peak_limited: bool,
}

/// 10·log₁₀(2): a dual-mono signal measured as stereo reads this much above its single channel.
pub const DUAL_MONO_OFFSET_LU: f32 = 3.010_3;

impl SoundLoudness {
    /// Measures interleaved stereo `samples` (left, right, …) at `sample_rate`. The gain fields
    /// are left at "not normalized"; call [`Self::apply_policy`].
    ///
    /// # Errors
    ///
    /// A zero sample rate or no complete frame.
    pub fn measure_interleaved_stereo(samples: &[f32], sample_rate: u32) -> Result<Self, AudioError> {
        let _span = tracing::debug_span!(target: "regia_audio", "measure_sound_loudness").entered();
        let frames = samples.len() / 2;
        if frames == 0 {
            return Err(AudioError::Invalid("no audio frames to measure".into()));
        }
        let mut meter = LoudnessMeter::new(2, sample_rate).map_err(|e| AudioError::Invalid(e.to_string()))?;
        let samples = &samples[..frames * 2];
        meter.push(samples);
        let result = meter.finish();
        let short_clip = (frames as f64) < crate::loudness::GATING_BLOCK_SECS * f64::from(sample_rate);
        let loudness = if short_clip {
            result.momentary_lufs
        } else {
            result.integrated_lufs
        };
        // Below the absolute gate a short clip is silence too, as a long one is.
        let integrated_lufs =
            (loudness.is_finite() && loudness >= crate::loudness::ABSOLUTE_GATE_LUFS).then_some(loudness);
        let dual_mono = samples.as_chunks::<2>().0.iter().all(|[l, r]| l == r);
        Ok(Self {
            integrated_lufs,
            loudness_range_lu: if result.loudness_range_lu.is_finite() {
                result.loudness_range_lu
            } else {
                0.0
            },
            // `gain_to_db` floors silence at `MUTE_DB`.
            true_peak_db: (result.true_peak_db.is_finite() && result.true_peak_db > crate::MUTE_DB)
                .then_some(result.true_peak_db),
            short_clip,
            dual_mono,
            sample_rate,
            frames: frames as u64,
            normalize: false,
            target_lufs: REFERENCE_LUFS,
            gain_db: 0.0,
            peak_limited: false,
        })
    }

    /// Fills the gain fields from `policy` (`target_override` replaces its target for this
    /// sound): `target − integrated`, lowered so the true peak stays under the ceiling, clamped to
    /// ±[`MAX_NORMALIZE_GAIN_DB`]. 0 dB when `normalize` is off or the sound is silent.
    pub fn apply_policy(&mut self, policy: &LoudnessPolicy, normalize: bool, target_override: Option<f32>) {
        let target = target_override
            .filter(|t| t.is_finite())
            .unwrap_or(policy.target_lufs);
        self.normalize = normalize;
        self.target_lufs = target;
        self.gain_db = 0.0;
        self.peak_limited = false;
        let Some(measured) = self.integrated_lufs.filter(|_| normalize) else {
            return;
        };
        let mut gain = target - measured;
        if let Some(peak) = self.true_peak_db {
            let headroom = policy.true_peak_ceiling_db - peak;
            if headroom.is_finite() && gain > headroom {
                gain = headroom;
                self.peak_limited = true;
            }
        }
        if gain.is_finite() {
            self.gain_db = gain.clamp(-MAX_NORMALIZE_GAIN_DB, MAX_NORMALIZE_GAIN_DB);
        }
    }

    /// The decoded length, seconds.
    #[must_use]
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames as f64 / f64::from(self.sample_rate)
        }
    }
}

/// Decodes a sound file's bytes with the runtime decoder (WAV, Ogg Vorbis, FLAC, MP3) and
/// measures it. The gain fields are left at "not normalized".
///
/// # Errors
///
/// The bytes do not decode, or hold no frame; [`AudioError::DecodeLimit`] when they decode past the
/// default [`crate::SoundDecodeLimits`] (ADR-4922).
pub fn measure_sound_bytes(bytes: Vec<u8>, label: &str) -> Result<SoundLoudness, AudioError> {
    measure_sound_bytes_with_limits(bytes, label, &crate::SoundDecodeLimits::default())
}

/// [`measure_sound_bytes`] with explicit decode limits.
///
/// # Errors
///
/// As [`measure_sound_bytes`], past `limits`.
pub fn measure_sound_bytes_with_limits(
    bytes: Vec<u8>,
    label: &str,
    limits: &crate::SoundDecodeLimits,
) -> Result<SoundLoudness, AudioError> {
    let data = crate::decode::decode_static_bytes(bytes, label, limits)?;
    measure_frames(&data.frames, data.sample_rate)
}

/// Measures a sound asset (a streaming sound is decoded to memory first). The gain fields are
/// left at "not normalized".
///
/// # Errors
///
/// The sound cannot be decoded to memory, or holds no frame.
pub fn measure_sound(asset: &SoundAsset) -> Result<SoundLoudness, AudioError> {
    let decoded = asset.decode_to_memory()?;
    let (frames, rate) = decoded
        .static_frames()
        .ok_or_else(|| AudioError::load(asset.label(), "no in-memory frames"))?;
    measure_frames(frames, rate)
}

fn measure_frames(frames: &[kira::Frame], sample_rate: u32) -> Result<SoundLoudness, AudioError> {
    let interleaved: Vec<f32> = frames.iter().flat_map(|f| [f.left, f.right]).collect();
    SoundLoudness::measure_interleaved_stereo(&interleaved, sample_rate)
}

/// `loudness.ron`: every cooked sound's [`SoundLoudness`], keyed by its `assets/…` path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoudnessTable {
    /// Format version ([`LOUDNESS_TABLE_VERSION`]).
    #[serde(default = "first_version")]
    pub format_version: u32,
    /// The policy every entry's gain was computed with.
    #[serde(default)]
    pub policy: LoudnessPolicy,
    /// Sound path (`assets/…`, forward slashes) → record.
    #[serde(default)]
    pub entries: BTreeMap<String, SoundLoudness>,
}

fn first_version() -> u32 {
    1
}

impl Default for LoudnessTable {
    fn default() -> Self {
        Self {
            format_version: LOUDNESS_TABLE_VERSION,
            policy: LoudnessPolicy::default(),
            entries: BTreeMap::new(),
        }
    }
}

/// Errors reading a loudness table.
#[derive(Debug, thiserror::Error)]
pub enum LoudnessTableError {
    /// The file could not be read.
    #[error("loudness table `{path}` could not be read: {message}")]
    Unreadable {
        /// The path.
        path: String,
        /// The IO error.
        message: String,
    },
    /// The text is not a loudness table.
    #[error("loudness table is invalid: {0}")]
    Invalid(String),
    /// Written by a newer engine.
    #[error("loudness table format version {0} is not supported (this engine reads up to version 1)")]
    UnsupportedVersion(u32),
}

impl LoudnessTable {
    /// Parses `loudness.ron` text.
    ///
    /// # Errors
    ///
    /// Not RON of this shape, or a `format_version` newer than [`LOUDNESS_TABLE_VERSION`].
    pub fn from_ron(bytes: &[u8]) -> Result<Self, LoudnessTableError> {
        let text = std::str::from_utf8(bytes).map_err(|e| LoudnessTableError::Invalid(e.to_string()))?;
        let table: Self = ron::from_str(text).map_err(|e| LoudnessTableError::Invalid(e.to_string()))?;
        if table.format_version > LOUDNESS_TABLE_VERSION {
            return Err(LoudnessTableError::UnsupportedVersion(table.format_version));
        }
        Ok(table)
    }

    /// Reads and parses a `loudness.ron` file.
    ///
    /// # Errors
    ///
    /// The file cannot be read, or [`Self::from_ron`] fails.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, LoudnessTableError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| LoudnessTableError::Unreadable {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        Self::from_ron(&bytes)
    }

    /// The table as pretty RON (stable: entries are sorted by path).
    ///
    /// # Errors
    ///
    /// Serialization failed (a non-finite float in a record).
    pub fn to_ron_string(&self) -> Result<String, LoudnessTableError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| LoudnessTableError::Invalid(e.to_string()))
    }

    /// The record for `path`: the entry whose key equals it, or the longest key it ends with at
    /// a path-component boundary (`/home/me/game/assets/sfx/a.wav` finds `assets/sfx/a.wav`, also
    /// when the table has `assets/a.wav` under a project inside another `assets` folder).
    /// Backslashes are read as separators.
    #[must_use]
    pub fn entry_for(&self, path: &str) -> Option<&SoundLoudness> {
        let path = path.replace('\\', "/");
        if let Some(entry) = self.entries.get(&path) {
            return Some(entry);
        }
        self.entries
            .iter()
            .filter(|(key, _)| {
                path.strip_suffix(key.as_str())
                    .is_some_and(|head| head.ends_with('/'))
            })
            .max_by_key(|(key, _)| key.len())
            .map(|(_, entry)| entry)
    }

    /// The normalization gain for `path`, dB ([`Self::entry_for`]).
    #[must_use]
    pub fn gain_db_for(&self, path: &str) -> Option<f32> {
        self.entry_for(path).map(|e| e.gain_db)
    }
}

/// The serde field-list fingerprint of `loudness.ron` (hashed by `cargo xtask check-formats`).
pub fn loudness_table_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<LoudnessTable>(),
        regia_core::schema::serde_field_names::<LoudnessPolicy>(),
        regia_core::schema::serde_field_names::<SoundLoudness>(),
    ])
}

/// The versions a built-in migration step leaves (`from` side): none, v1 is the first version.
pub fn loudness_table_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo_sine(hz: f32, secs: f64, amp: f32) -> Vec<f32> {
        let n = (secs * 48_000.0) as usize;
        (0..n)
            .flat_map(|i| {
                let s = (i as f32 / 48_000.0 * hz * std::f32::consts::TAU).sin() * amp;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn a_record_matches_the_meter_and_its_gain_lands_on_the_target() {
        let samples = stereo_sine(1000.0, 3.0, 0.1);
        let meter = crate::measure_loudness(&samples, 2, 48_000).expect("valid");
        let mut record = SoundLoudness::measure_interleaved_stereo(&samples, 48_000).expect("valid");
        let measured = record.integrated_lufs.expect("loud enough");
        assert!(
            (measured - meter.integrated_lufs).abs() < 0.1,
            "{measured} vs {}",
            meter.integrated_lufs
        );
        assert!(!record.short_clip && record.dual_mono);
        assert_eq!(record.frames, 144_000);
        record.apply_policy(&LoudnessPolicy::default(), true, None);
        assert!(!record.peak_limited);
        let gain = crate::db_to_gain(record.gain_db);
        let scaled: Vec<f32> = samples.iter().map(|s| s * gain).collect();
        let again = SoundLoudness::measure_interleaved_stereo(&scaled, 48_000).expect("valid");
        let after = again.integrated_lufs.expect("loud");
        assert!((after - REFERENCE_LUFS).abs() < 0.05, "{after}");
    }

    #[test]
    fn the_true_peak_ceiling_limits_a_hot_transient() {
        // A quiet bed with one full-scale click: raising the bed to −16 would push the click
        // past −1 dBTP.
        let mut samples = stereo_sine(500.0, 2.0, 0.01);
        samples[48_000] = 0.9;
        samples[48_001] = 0.9;
        let mut record = SoundLoudness::measure_interleaved_stereo(&samples, 48_000).expect("valid");
        let policy = LoudnessPolicy {
            target_lufs: -16.0,
            ..LoudnessPolicy::default()
        };
        record.apply_policy(&policy, true, None);
        assert!(record.peak_limited, "{record:?}");
        let peak = record.true_peak_db.expect("peak");
        assert!(
            (peak + record.gain_db - policy.true_peak_ceiling_db).abs() < 1e-3,
            "{record:?}"
        );
    }

    #[test]
    fn silence_has_no_loudness_and_no_gain() {
        let mut record = SoundLoudness::measure_interleaved_stereo(&[0.0; 96_000], 48_000).expect("valid");
        assert_eq!(record.integrated_lufs, None);
        assert_eq!(record.true_peak_db, None);
        record.apply_policy(&LoudnessPolicy::default(), true, None);
        assert_eq!(record.gain_db, 0.0);
    }

    #[test]
    fn a_short_clip_is_measured_ungated_and_close_to_the_long_tone() {
        let short =
            SoundLoudness::measure_interleaved_stereo(&stereo_sine(1000.0, 0.1, 0.2), 48_000).expect("valid");
        let long =
            SoundLoudness::measure_interleaved_stereo(&stereo_sine(1000.0, 2.0, 0.2), 48_000).expect("valid");
        assert!(short.short_clip && !long.short_clip);
        let (s, l) = (
            short.integrated_lufs.expect("finite"),
            long.integrated_lufs.expect("finite"),
        );
        assert!((s - l).abs() < 0.5, "short {s} long {l}");
    }

    #[test]
    fn opt_out_and_target_override() {
        let samples = stereo_sine(1000.0, 1.0, 0.1);
        let mut record = SoundLoudness::measure_interleaved_stereo(&samples, 48_000).expect("valid");
        record.apply_policy(&LoudnessPolicy::default(), false, None);
        assert_eq!(record.gain_db, 0.0);
        assert!(!record.normalize);
        record.apply_policy(&LoudnessPolicy::default(), true, Some(-30.0));
        let measured = record.integrated_lufs.expect("finite");
        assert!((record.gain_db - (-30.0 - measured)).abs() < 1e-4);
        assert_eq!(record.target_lufs, -30.0);
    }

    #[test]
    fn a_stereo_source_is_not_dual_mono() {
        let mut samples = stereo_sine(1000.0, 0.5, 0.1);
        samples[11] = -samples[11] + 0.01;
        let record = SoundLoudness::measure_interleaved_stereo(&samples, 48_000).expect("valid");
        assert!(!record.dual_mono);
    }

    #[test]
    fn bad_inputs_are_errors() {
        assert!(SoundLoudness::measure_interleaved_stereo(&[0.1], 48_000).is_err());
        assert!(SoundLoudness::measure_interleaved_stereo(&[0.1, 0.1], 0).is_err());
        assert!(measure_sound_bytes(b"not audio".to_vec(), "x").is_err());
    }

    #[test]
    fn the_table_round_trips_and_refuses_a_newer_version() {
        let mut table = LoudnessTable::default();
        let mut record =
            SoundLoudness::measure_interleaved_stereo(&stereo_sine(440.0, 1.0, 0.3), 48_000).expect("valid");
        record.apply_policy(&table.policy, true, None);
        table.entries.insert("assets/sfx/a.wav".into(), record);
        let mut silent = SoundLoudness::measure_interleaved_stereo(&[0.0; 200], 48_000).expect("valid");
        silent.apply_policy(&table.policy, true, None);
        table.entries.insert("assets/sfx/quiet.wav".into(), silent);
        let text = table.to_ron_string().expect("ron");
        assert_eq!(LoudnessTable::from_ron(text.as_bytes()).expect("parse"), table);
        let newer = text.replacen("format_version: 1", "format_version: 2", 1);
        assert!(matches!(
            LoudnessTable::from_ron(newer.as_bytes()),
            Err(LoudnessTableError::UnsupportedVersion(2))
        ));
        assert!(matches!(
            LoudnessTable::from_ron(b"(entries: 3)"),
            Err(LoudnessTableError::Invalid(_))
        ));
    }

    #[test]
    fn lookups_match_the_key_or_a_path_suffix_at_a_separator() {
        let mut table = LoudnessTable::default();
        let mut record =
            SoundLoudness::measure_interleaved_stereo(&stereo_sine(440.0, 1.0, 0.3), 48_000).expect("valid");
        record.gain_db = -4.0;
        table.entries.insert("assets/sfx/a.wav".into(), record);
        assert_eq!(table.gain_db_for("assets/sfx/a.wav"), Some(-4.0));
        assert_eq!(table.gain_db_for("/home/me/game/assets/sfx/a.wav"), Some(-4.0));
        assert_eq!(table.gain_db_for("C:\\game\\assets\\sfx\\a.wav"), Some(-4.0));
        assert_eq!(table.gain_db_for("/home/me/game/myassets/sfx/a.wav"), None);
        assert_eq!(table.gain_db_for("assets/sfx/b.wav"), None);
        // A project under a folder named `assets`: the longest key wins, whatever the sort order.
        let mut nested = table.clone();
        let mut other = nested.entries["assets/sfx/a.wav"];
        other.gain_db = 7.0;
        nested.entries.insert("assets/a.wav".into(), other);
        nested.entries.insert("assets/mods/assets/a.wav".into(), {
            other.gain_db = -2.0;
            other
        });
        assert_eq!(nested.gain_db_for("/srv/assets/mods/assets/a.wav"), Some(-2.0));
    }

    #[test]
    fn the_policy_range_check() {
        assert_eq!(LoudnessPolicy::default().problem(), None);
        let bad = LoudnessPolicy {
            target_lufs: 3.0,
            ..LoudnessPolicy::default()
        };
        assert!(bad.problem().is_some());
        let bad = LoudnessPolicy {
            true_peak_ceiling_db: f32::NAN,
            ..LoudnessPolicy::default()
        };
        assert!(bad.problem().is_some());
    }

    #[test]
    fn the_fingerprint_names_every_field() {
        let f = loudness_table_schema_fingerprint();
        for field in [
            "format_version",
            "policy",
            "entries",
            "true_peak_ceiling_db",
            "gain_db",
            "dual_mono",
        ] {
            assert!(f.contains(field), "{field}: {f}");
        }
    }
}
