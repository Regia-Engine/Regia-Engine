// SPDX-License-Identifier: MPL-2.0
//! The sequencer asset (`.seq.ron`): the data model, its RON form, validation, and the format
//! version contract (`docs/specs/12-assets.md` § 4a, `docs/formats/seq-ron.md`).
//!
//! A [`Sequence`] is one master clock: named [`Track`]s that animate a single property of a
//! named target each (a float, a vec3 or a bool, as plain keys in seconds), plus named
//! [`Marker`]s (instants that fire when playback crosses them). Everything is plain keyed data,
//! so a hand-written file, an agent and the editor's sequence curve panel all edit the same
//! thing.
//!
//! Version 2 (ADR-1880) adds Bezier keys with tangents ([`Ease::Bezier`], [`Tangent`],
//! optional handle weights) and three section channels: animation clips with blending
//! ([`Channel::Clip`]), audio ([`Channel::Audio`]) and sub-sequences ([`Channel::Sub`]). A v1
//! file loads unchanged: every v2 addition is a new variant or an optional field.
//!
//! Version 3 (ADR-2405) adds spatial (3D) sound sections ([`AudioSection::spatial`], a bound
//! entity or a fixed position with the section's own attenuation), a time scale on sound
//! sections ([`AudioSection::time_scale`]) and looping sub-sequence sections
//! ([`SubSection::repeat`], [`SubRepeat`], [`SubSection::loop_length`]). A v2 file loads
//! unchanged: every v3 addition is an optional field left out of a saved file at its default.
//!
//! Version 4 (ADR-2862) adds video sections ([`Channel::Video`], [`VideoSection`], [`VideoTarget`]):
//! a clip of a video asset placed on the clock and shown fullscreen, through a bound entity's video
//! player, on a bound mesh's material or in a bound UI node. A v3 file loads unchanged: the only
//! addition is a new variant.
//!
//! Version 5 adds spawnable bindings and `restore_on_stop` ([`Sequence::spawnables`],
//! [`Spawnable`], [`Sequence::restore_on_stop`], ADR-2948), and Doppler and occlusion on spatial
//! sound sections ([`AudioSpatial::doppler`], [`AudioSpatial::occlusion`]) with a pitch-preserving
//! time scale ([`AudioSection::time_scale_mode`], [`TimeScaleMode`], ADR-3197). A v4 file loads
//! with Doppler off on every spatial section (the migration step writes `doppler: 0.0`, so it
//! sounds as it did); every other v5 field is left out of a saved file at its default.
//!
//! The model is target-agnostic: `target` is a binding name the host resolves (an entity name, a
//! camera name, "ui", ...), `property` a path on that target. Camera moves stay in `.shots.ron`
//! (ADR-268); the sequencer is the timeline around them (ADR-1507).
//!
//! ```
//! use regia_framework::sequencer::{Channel, Sequence};
//!
//! let sequence = Sequence::from_ron(r#"(
//!     version: 1,
//!     name: "intro",
//!     tracks: [
//!         (name: "fade-in", target: "ui", property: "opacity",
//!             keys: Float([(time: 0.0, value: 0.0), (time: 2.0, value: 1.0, ease: Smooth)])),
//!     ],
//!     markers: [(name: "title", time: 0.5)],
//! )"#).expect("valid sequence");
//! assert_eq!(sequence.duration(), 2.0);
//! assert_eq!(sequence.tracks[0].target, "ui");
//! ```

use std::sync::atomic::{AtomicU64, Ordering};

use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};

/// Current `.seq.ron` version (v2: Bezier tangents and the clip, audio and sub-sequence
/// channels, ADR-1880; v3: spatial sound sections, voice time scale and looping sub-sequences,
/// ADR-2405; v4: video sections, ADR-2862; v5: spawnable bindings and `restore_on_stop`,
/// ADR-2948, and Doppler, occlusion and pitch-preserving sound sections, ADR-3197; v6: event
/// tracks ([`Channel::Event`]) and the track lock ([`Track::locked`]), ADR-8105 row (c)).
pub const SEQUENCE_FORMAT_VERSION: u32 = 6;

/// Most spawnable bindings one sequence may declare (v5).
pub const MAX_SPAWNABLES: usize = 256;

/// Largest [`AudioSpatial::doppler`] factor (1 is physical; more exaggerates the shift).
pub const MAX_DOPPLER_FACTOR: f32 = 4.0;

/// The Doppler pitch ratio of a sequence voice is clamped to this range (one octave each way),
/// whatever the speeds: a fast cut or a teleport cannot throw a voice further than that.
pub const DOPPLER_RATIO_RANGE: (f32, f32) = (0.5, 2.0);

/// The rates a [`TimeScaleMode::PreservePitch`] section stretches at (`regia_audio`'s WSOLA
/// range): a voice rate outside it (a sub-sequence speed on top of the section's time scale)
/// plays by varispeed instead and is reported (`stretch_rate`).
pub const PRESERVE_PITCH_RANGE: (f32, f32) = (0.5, 2.0);

/// Deepest nesting of looping sub-sequence sections (a looping section inside a looping section
/// is depth 2). Each level multiplies the markers a tree fires, so
/// [`crate::sequencer::SequenceLibrary::check`] refuses a deeper tree with
/// [`SequenceError::LoopTooDeep`] (agent code `seq_loop_too_deep`), and sampling plays a looping
/// section past the limit as if it did not loop.
pub const MAX_LOOP_DEPTH: usize = 4;

/// Shortest loop pass, seconds of sub-sequence time: shorter `loop_length`s are refused, and a
/// default pass (the sub-sequence's length after `offset`) shorter than this does not loop.
pub const MIN_LOOP_LENGTH: f32 = 1.0e-3;

/// Most loop passes [`SubRepeat::Count`] may ask for.
pub const MAX_LOOP_COUNT: u32 = 1_000_000;

/// Deepest nesting of sub-sequences a player follows (a sequence in a sequence is depth 1).
pub const MAX_SUB_DEPTH: usize = 8;

/// Most distinct sequences one sub-sequence tree may read ([`crate::sequencer::SequenceLibrary::read_tree`]).
pub const MAX_TREE_SEQUENCES: usize = 1024;

/// Most sub-sequence sections one tree may expand to, counting a sequence again wherever it is
/// placed. Sections fan out level by level, so a small file can name an astronomical tree:
/// [`crate::sequencer::SequenceLibrary::check`] refuses one, and every walk stops here.
pub const MAX_TREE_SECTIONS: usize = 16_384;

/// Most sections one clip, audio or sub-sequence track may hold.
pub const MAX_SECTIONS: usize = 1024;

/// Handle weight of an unweighted Bezier key: a third of the segment, which makes the segment
/// the cubic Hermite curve of its two slopes.
pub const DEFAULT_WEIGHT: f32 = 1.0 / 3.0;

/// Authoring frame rate, frames per second, of a sequence that omits `fps`.
fn default_fps() -> f32 {
    30.0
}

/// `enabled` of a track that omits it.
fn default_true() -> bool {
    true
}

/// Sequencer errors.
#[derive(Debug, thiserror::Error)]
pub enum SequenceError {
    /// Looping sub-sequence sections nest deeper than [`MAX_LOOP_DEPTH`] (v3,
    /// [`crate::sequencer::SequenceLibrary::check`]; agent code `seq_loop_too_deep`).
    #[error("track `{track}`: looping sub-sequences nest {depth} deep; the limit is {MAX_LOOP_DEPTH}")]
    LoopTooDeep {
        /// The track whose section goes past the limit.
        track: String,
        /// The loop depth that section would reach.
        depth: usize,
    },
    /// The file is a newer version than this build reads.
    #[error("sequence version {found} is newer than supported version {expected}; upgrade the engine")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// RON syntax or shape error.
    #[error("sequence parse error: {0}")]
    Parse(String),
    /// Semantic error (unsorted keys, empty name, out-of-range value).
    #[error("invalid sequence: {0}")]
    Invalid(String),
    /// A migration step failed.
    #[error("sequence migration failed: {0}")]
    Migration(String),
}

impl SequenceError {
    /// The stable lowercase code agents see for this error (`seq_version`, `seq_parse`,
    /// `seq_invalid`, `seq_migration`, `seq_loop_too_deep`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::LoopTooDeep { .. } => "seq_loop_too_deep",
            Self::Version { .. } => "seq_version",
            Self::Parse(_) => "seq_parse",
            Self::Invalid(_) => "seq_invalid",
            Self::Migration(_) => "seq_migration",
        }
    }
}

/// How a keyed channel eases from one key to the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Ease {
    /// Straight line between the two keys.
    #[default]
    Linear,
    /// Holds this key's value until the next key (a cut in the value).
    Step,
    /// Smoothstep (`3u² - 2u³`): zero slope at both keys.
    Smooth,
    /// A cubic Bezier from this key's outgoing tangent to the next key's incoming tangent
    /// ([`Tangent`], v2). Unweighted handles (a third of the segment each) give the cubic
    /// Hermite curve of the two slopes.
    Bezier,
}

/// The tangent of a key: the slopes (value units per second) of the curve arriving at and
/// leaving the key, used by [`Ease::Bezier`] segments (v2). `S` is `f32` for a float key and
/// `[f32; 3]` (one slope per component) for a vec3 key.
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub enum Tangent<S> {
    /// Computed from the neighboring keys: the slope through the two neighbors, set to zero at
    /// a local extreme and limited so the curve does not overshoot between monotone keys
    /// (Fritsch and Carlson's condition). The first and the last key are flat.
    #[default]
    Auto,
    /// Zero slope on both sides.
    Flat,
    /// Each side points at its neighboring key (the incoming slope is the secant from the
    /// previous key, the outgoing one the secant to the next).
    Linear,
    /// One slope on both sides (a smooth key).
    Aligned(S),
    /// Separate incoming and outgoing slopes (a broken key).
    Broken(S, S),
}

impl<S> Tangent<S> {
    /// `true` for [`Tangent::Auto`] (the default, left out of a saved file).
    pub fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }

    /// The name agents and the editor use (`auto`, `flat`, `linear`, `aligned`, `broken`).
    #[must_use]
    pub fn mode_name(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Flat => "flat",
            Self::Linear => "linear",
            Self::Aligned(_) => "aligned",
            Self::Broken(..) => "broken",
        }
    }
}

/// One keyed scalar of a [`Channel::Float`] track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScalarKey {
    /// Seconds on the sequence's master clock.
    pub time: f32,
    /// The value at `time`.
    pub value: f32,
    /// How the value eases from this key to the next.
    #[serde(default)]
    pub ease: Ease,
    /// The key's tangent, used by the [`Ease::Bezier`] segments on either side of it (v2; left
    /// out of a saved file when [`Tangent::Auto`]).
    #[serde(default, skip_serializing_if = "Tangent::is_auto")]
    pub tangent: Tangent<f32>,
    /// Bezier handle lengths `(incoming, outgoing)` as fractions of the segment on that side, each
    /// in `(0, 1]` (v2; `None` is [`DEFAULT_WEIGHT`] on both sides).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<(f32, f32)>,
}

/// One keyed vec3 of a [`Channel::Vec3`] track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vec3Key {
    /// Seconds on the sequence's master clock.
    pub time: f32,
    /// The value at `time` (a position, a color, an Euler triple — whatever the property means).
    pub value: [f32; 3],
    /// How every component eases from this key to the next.
    #[serde(default)]
    pub ease: Ease,
    /// The key's tangent, one slope per component (v2; left out when [`Tangent::Auto`]).
    #[serde(default, skip_serializing_if = "Tangent::is_auto")]
    pub tangent: Tangent<[f32; 3]>,
    /// Bezier handle lengths `(incoming, outgoing)`, shared by the components (v2; `None` is
    /// [`DEFAULT_WEIGHT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<(f32, f32)>,
}

impl ScalarKey {
    /// A key with the default tangent and weights.
    #[must_use]
    pub fn new(time: f32, value: f32, ease: Ease) -> Self {
        Self {
            time,
            value,
            ease,
            tangent: Tangent::Auto,
            weights: None,
        }
    }
}

impl Vec3Key {
    /// A key with the default tangent and weights.
    #[must_use]
    pub fn new(time: f32, value: [f32; 3], ease: Ease) -> Self {
        Self {
            time,
            value,
            ease,
            tangent: Tangent::Auto,
            weights: None,
        }
    }
}

/// One keyed bool of a [`Channel::Bool`] track. Bool keys always step: the value holds until the
/// next key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoolKey {
    /// Seconds on the sequence's master clock.
    pub time: f32,
    /// The value from `time` until the next key.
    pub value: bool,
}

fn default_one() -> f32 {
    1.0
}

fn is_one(x: &f32) -> bool {
    *x == 1.0
}

/// How a sound section follows its time scale (v5, ADR-3197).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeScaleMode {
    /// Like tape: the sound plays faster and higher, or slower and lower, by the same ratio (the
    /// v3 behavior).
    #[default]
    Varispeed,
    /// The speed changes and the pitch does not: the player renders the section ahead through
    /// `regia_audio`'s WSOLA stretch (`stretch_asset`) into a bounded cache and plays that. Voice
    /// rates from 0.5 to 2 ([`PRESERVE_PITCH_RANGE`]); a rate of exactly 1 plays the sound
    /// itself.
    PreservePitch,
}

impl TimeScaleMode {
    /// `true` for [`TimeScaleMode::Varispeed`] (left out of a saved file).
    #[must_use]
    pub fn is_varispeed(&self) -> bool {
        *self == Self::Varispeed
    }

    /// `"varispeed"` or `"preserve_pitch"` (the agent spelling).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Varispeed => "varispeed",
            Self::PreservePitch => "preserve_pitch",
        }
    }

    /// The mode an agent spelling names.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "varispeed" => Some(Self::Varispeed),
            "preserve_pitch" => Some(Self::PreservePitch),
            _ => None,
        }
    }
}

fn default_min_distance() -> f32 {
    1.0
}

fn default_max_distance() -> f32 {
    50.0
}

fn is_default_min_distance(x: &f32) -> bool {
    *x == default_min_distance()
}

fn is_default_max_distance(x: &f32) -> bool {
    *x == default_max_distance()
}

/// Where a spatial sound section emits from (v3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SoundSource {
    /// From the entity a binding name resolves to, like a track `target` (renamed through the
    /// sub-sequence sections above it): the sound follows the entity's world position and facing
    /// every frame. An unbound name plays from the listener (no pan, no distance gain) and is
    /// reported as an unbound target.
    Binding(String),
    /// From a fixed world position, metres.
    Position([f32; 3]),
}

/// How a spatial section's gain falls with distance (v3): the rolloff curves of
/// `regia_audio`'s emitters (`RolloffCurve`), as a frozen part of this format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum SoundRolloff {
    /// Inverse distance below `max_distance`, silent past it.
    #[default]
    Inverse,
    /// Linear from full at `min_distance` to silent at `max_distance`.
    Linear,
    /// `-db_per_doubling` dB per doubling of distance past `min_distance`, silent past
    /// `max_distance`.
    Log(f32),
}

impl SoundRolloff {
    fn is_inverse(&self) -> bool {
        *self == Self::Inverse
    }
}

/// Places a sound section in the world (v3): where it emits from and how it attenuates. The
/// player plays it through `regia_audio`'s emitter spatialization (distance gain, cone-free,
/// equal-power pan against the nearest listener), with this attenuation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSpatial {
    /// Where the sound emits from.
    pub source: SoundSource,
    /// Full volume inside this distance, metres (more than zero).
    #[serde(
        default = "default_min_distance",
        skip_serializing_if = "is_default_min_distance"
    )]
    pub min_distance: f32,
    /// Silent beyond this distance, metres (more than `min_distance`).
    #[serde(
        default = "default_max_distance",
        skip_serializing_if = "is_default_max_distance"
    )]
    pub max_distance: f32,
    /// How the gain falls between the two.
    #[serde(default, skip_serializing_if = "SoundRolloff::is_inverse")]
    pub rolloff: SoundRolloff,
    /// 0 plays the sound flat (no distance gain, no pan), 1 fully in 3D; in `0..=1`.
    #[serde(default = "default_one", skip_serializing_if = "is_one")]
    pub spatial_blend: f32,
    /// Doppler factor (v5, ADR-3197): 0 is off, 1 physical, up to [`MAX_DOPPLER_FACTOR`]. The
    /// emitter's velocity comes from the section's sampled motion on the sequence clock (the
    /// bound entity's position track, or its sampled positions), never from a seek or a cut, and
    /// the pitch ratio is clamped to [`DOPPLER_RATIO_RANGE`]. 1 when left out of a v5 file; the
    /// v4 → v5 migration writes 0, so an older file sounds as it did.
    #[serde(default = "default_one", skip_serializing_if = "is_one")]
    pub doppler: f32,
    /// Occlusion (v5, ADR-3197): the section asks `regia_audio`'s occlusion query (the
    /// `AudioOcclusion` provider) whether geometry stands between the listener and the source,
    /// and plays muffled (its low-pass) and quieter by the occlusion tuning. Off by default.
    #[serde(default, skip_serializing_if = "is_false")]
    pub occlusion: bool,
}

impl AudioSpatial {
    /// A fully 3D source with the default attenuation (1 m to 50 m, inverse rolloff).
    #[must_use]
    pub fn new(source: SoundSource) -> Self {
        Self {
            source,
            min_distance: default_min_distance(),
            max_distance: default_max_distance(),
            rolloff: SoundRolloff::Inverse,
            spatial_blend: 1.0,
            doppler: 1.0,
            occlusion: false,
        }
    }
}

/// How many times a sub-sequence section plays its sub-sequence (v3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubRepeat {
    /// Once, as in v2: the sub-sequence's keys hold past its end.
    #[default]
    Once,
    /// This many passes (1..=[`MAX_LOOP_COUNT`]), then the end of the last pass holds until the
    /// section ends.
    Count(u32),
    /// Pass after pass until the section ends or the player stops.
    UntilStop,
}

impl SubRepeat {
    /// `true` for [`SubRepeat::Once`] (the default, left out of a saved file).
    #[must_use]
    pub fn is_once(&self) -> bool {
        *self == Self::Once
    }

    /// `true` when the section loops ([`Count`](Self::Count) of 2 or more, or
    /// [`UntilStop`](Self::UntilStop)).
    #[must_use]
    pub fn loops(&self) -> bool {
        match self {
            Self::Once => false,
            Self::Count(n) => *n > 1,
            Self::UntilStop => true,
        }
    }

    /// The name agents and the editor use (`once`, `count`, `until_stop`).
    #[must_use]
    pub fn mode_name(&self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Count(_) => "count",
            Self::UntilStop => "until_stop",
        }
    }
}

/// One animation clip placed on a [`Channel::Clip`] track (v2). Sections may overlap: where
/// they do, their poses blend by weight, and where the summed weight is below one the rig's own
/// pose shows through.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipSection {
    /// The clip's name in the player's clip library (`SequenceClips`).
    pub clip: String,
    /// Sequence time the section starts, seconds.
    pub start: f32,
    /// Sequence time the section ends, seconds (after `start`).
    pub end: f32,
    /// Clip time at `start`, seconds.
    #[serde(default)]
    pub offset: f32,
    /// Clip seconds per sequence second (more than zero).
    #[serde(default = "default_one")]
    pub speed: f32,
    /// `true` wraps the clip time over the clip's length; `false` holds the last pose.
    #[serde(default)]
    pub looping: bool,
    /// Seconds after `start` over which the weight rises from zero (zero: full at once).
    #[serde(default)]
    pub blend_in: f32,
    /// Seconds before `end` over which the weight falls to zero (zero: full to the end).
    #[serde(default)]
    pub blend_out: f32,
    /// The section's full weight, in `(0, 1]`.
    #[serde(default = "default_one")]
    pub weight: f32,
}

/// One sound placed on a [`Channel::Audio`] track (v2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSection {
    /// The sound's name in the audio library (`regia_audio::SoundAssets`).
    pub sound: String,
    /// Sequence time the sound starts, seconds.
    pub start: f32,
    /// Sequence time the sound stops, seconds (after `start`).
    pub end: f32,
    /// Seconds into the sound at `start`.
    #[serde(default)]
    pub offset: f32,
    /// Volume, dB.
    #[serde(default)]
    pub volume_db: f32,
    /// Seconds after `start` over which the sound fades in from silence.
    #[serde(default)]
    pub fade_in: f32,
    /// Seconds before `end` over which the sound fades out.
    #[serde(default)]
    pub fade_out: f32,
    /// Bus to play on (`None`: `Master`).
    #[serde(default)]
    pub bus: Option<String>,
    /// Sound seconds per sequence second, in `(0, 16]` (v3; 1 when left out). The voice's time
    /// scale is this times the speeds of the sub-sequence sections above it, and it plays by
    /// varispeed: faster is higher in pitch by the same ratio (no pitch-preserving stretch,
    /// ADR-2405).
    #[serde(default = "default_one", skip_serializing_if = "is_one")]
    pub time_scale: f32,
    /// Plays the sound in 3D from an entity or a position (v3; `None`: flat, as in v2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spatial: Option<AudioSpatial>,
    /// How the voice follows its time scale (v5): varispeed (pitch moves with speed, the
    /// default) or pitch-preserving ([`TimeScaleMode::PreservePitch`], ADR-3197).
    #[serde(default, skip_serializing_if = "TimeScaleMode::is_varispeed")]
    pub time_scale_mode: TimeScaleMode,
}

impl AudioSection {
    /// A flat sound section of `sound` from `start` to `end` with every other field at its
    /// default.
    #[must_use]
    pub fn new(sound: impl Into<String>, start: f32, end: f32) -> Self {
        Self {
            sound: sound.into(),
            start,
            end,
            offset: 0.0,
            volume_db: 0.0,
            fade_in: 0.0,
            fade_out: 0.0,
            bus: None,
            time_scale: 1.0,
            spatial: None,
            time_scale_mode: TimeScaleMode::Varispeed,
        }
    }
}

/// Another sequence placed on a [`Channel::Sub`] track (v2): its tracks apply and its markers
/// fire on this sequence's clock.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubSection {
    /// The sub-sequence's name in the sequence library (`SequenceLibrary`), usually its
    /// project path.
    pub sequence: String,
    /// Sequence time the section starts, seconds.
    pub start: f32,
    /// Sequence time the section ends, seconds (after `start`).
    pub end: f32,
    /// Sub-sequence time at `start`, seconds.
    #[serde(default)]
    pub offset: f32,
    /// Sub-sequence seconds per sequence second (more than zero).
    #[serde(default = "default_one")]
    pub speed: f32,
    /// Target renames: a sub-sequence target name to the name it binds as here. Unlisted
    /// targets keep their name.
    #[serde(default)]
    pub bindings: std::collections::BTreeMap<String, String>,
    /// How many passes the section plays (v3; [`SubRepeat::Once`] when left out).
    #[serde(default, skip_serializing_if = "SubRepeat::is_once")]
    pub repeat: SubRepeat,
    /// Sub-sequence seconds in one pass, from `offset` (v3; at least [`MIN_LOOP_LENGTH`]).
    /// `None`: the sub-sequence's length after `offset`. Only used when the section loops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_length: Option<f32>,
}

impl SubSection {
    /// A section playing `sequence` once from `start` to `end`, every other field at its
    /// default.
    #[must_use]
    pub fn new(sequence: impl Into<String>, start: f32, end: f32) -> Self {
        Self {
            sequence: sequence.into(),
            start,
            end,
            offset: 0.0,
            speed: 1.0,
            bindings: std::collections::BTreeMap::new(),
            repeat: SubRepeat::Once,
            loop_length: None,
        }
    }
}

/// How a fullscreen video section's picture fills the screen (v4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoFit {
    /// The whole picture shows, with bars on two sides.
    #[default]
    Letterbox,
    /// The screen is covered; the picture is cropped.
    Fill,
    /// The picture is stretched to the screen.
    Stretch,
}

/// Which material slot of a bound mesh shows a video section (v4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoScreenSlot {
    /// Base color: lit like any texture.
    Albedo,
    /// Emissive: a self-lit screen.
    #[default]
    Emissive,
}

/// Where a video section's picture goes (v4). Binding names are resolved by the player like a
/// track's `target`, renamed through the sub-sequence sections above the section and listed by
/// [`Sequence::target_names`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VideoTarget {
    /// A fullscreen layer. A negative `layer` draws under the UI, any other above it.
    Fullscreen {
        /// Order among fullscreen videos.
        #[serde(default)]
        layer: i32,
        /// How the picture fills the screen.
        #[serde(default)]
        fit: VideoFit,
    },
    /// The video player of the bound entity. The section takes the player over and restores it
    /// when it ends. The host decides what shows the player's picture.
    Player(String),
    /// A material slot of the bound entity's mesh (a screen in the world).
    Screen {
        /// The binding name of the mesh entity.
        binding: String,
        /// The material slot that shows the picture.
        #[serde(default)]
        slot: VideoScreenSlot,
    },
    /// The bound UI image node.
    UiImage(String),
}

impl Default for VideoTarget {
    fn default() -> Self {
        Self::Fullscreen {
            layer: 0,
            fit: VideoFit::Letterbox,
        }
    }
}

impl VideoTarget {
    /// The binding name this target needs (`None` for a fullscreen layer).
    #[must_use]
    pub fn binding(&self) -> Option<&str> {
        match self {
            Self::Fullscreen { .. } => None,
            Self::Player(name) | Self::UiImage(name) | Self::Screen { binding: name, .. } => Some(name),
        }
    }

    /// The binding name, mutably (`None` for a fullscreen layer).
    pub fn binding_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Fullscreen { .. } => None,
            Self::Player(name) | Self::UiImage(name) | Self::Screen { binding: name, .. } => Some(name),
        }
    }

    /// The target kind as lowercase text (the agent surface): `fullscreen`, `player`, `screen` or
    /// `ui_image`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Fullscreen { .. } => "fullscreen",
            Self::Player(_) => "player",
            Self::Screen { .. } => "screen",
            Self::UiImage(_) => "ui_image",
        }
    }
}

/// A clip of a video asset placed on a [`Channel::Video`] track (v4): it shows from `start` to
/// `end`, `offset` seconds into the source at `start`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoSection {
    /// The source: a project path of a `.video.ron`, `.webm` or `.mkv`.
    pub video: String,
    /// Sequence time the section starts, seconds.
    pub start: f32,
    /// Sequence time the section ends, seconds (after `start`).
    pub end: f32,
    /// Seconds into the source at `start`.
    #[serde(default)]
    pub offset: f32,
    /// Where the picture goes (default: a fullscreen layer).
    #[serde(default)]
    pub target: VideoTarget,
    /// Volume of the source's own audio track, dB (at most 24; -80 or less is silent).
    #[serde(default)]
    pub volume_db: f32,
    /// Wrap over the source's length instead of holding its last frame.
    #[serde(default)]
    pub looping: bool,
}

impl VideoSection {
    /// A fullscreen section of `video` from `start` to `end`, every other field at its default.
    #[must_use]
    pub fn new(video: impl Into<String>, start: f32, end: f32) -> Self {
        Self {
            video: video.into(),
            start,
            end,
            offset: 0.0,
            target: VideoTarget::default(),
            volume_db: 0.0,
            looping: false,
        }
    }

    /// The source time at sequence time `time`, seconds (not wrapped, not clamped to the source).
    #[must_use]
    pub fn source_time(&self, time: f64) -> f64 {
        f64::from(self.offset) + (time - f64::from(self.start))
    }
}

/// The keys of a [`Track`]: one kind of value, keyed over the master clock. A track cannot mix
/// kinds; two properties need two tracks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Channel {
    /// Scalar keys (`Float([(time: 0.0, value: 0.0)])`).
    Float(Vec<ScalarKey>),
    /// Vec3 keys (`Vec3([(time: 0.0, value: (0.0, 1.0, 0.0))])`).
    Vec3(Vec<Vec3Key>),
    /// Bool keys (`Bool([(time: 0.0, value: true)])`).
    Bool(Vec<BoolKey>),
    /// Animation clip sections on the rig the track's `target` binds to (v2; `property` is
    /// unused).
    Clip(Vec<ClipSection>),
    /// Sound sections (v2; `target` and `property` are unused).
    Audio(Vec<AudioSection>),
    /// Sub-sequence sections (v2; `target` and `property` are unused).
    Sub(Vec<SubSection>),
    /// Video sections (v4; `target` and `property` are unused: each section names its own target).
    Video(Vec<VideoSection>),
    /// Event keys (v6, ADR-8105): each fires a named event with a payload when playback crosses
    /// its time (`SequenceEventFired`, like markers). `target` is optional (the binding the event
    /// is about; empty for a sequence-wide event); `property` is unused.
    Event(Vec<EventKey>),
}

/// One key of an event track (v6, ADR-8105): playback crossing `time` fires `event` with
/// `payload`. Keys are in time order; keys at the same time fire in file order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventKey {
    /// Seconds on the master clock, zero or more.
    pub time: f32,
    /// The event name game code matches on (non-empty).
    pub event: String,
    /// Free-form payload the host interprets. Left out of the file when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub payload: String,
}

impl EventKey {
    /// A key firing `event` at `time` with no payload.
    #[must_use]
    pub fn new(time: f32, event: impl Into<String>) -> Self {
        Self {
            time,
            event: event.into(),
            payload: String::new(),
        }
    }
}

impl Channel {
    /// The channel's kind as written in the file (`Float`, `Vec3`, `Bool`, `Clip`, `Audio`,
    /// `Sub`, `Video`, `Event`).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Float(_) => "Float",
            Self::Vec3(_) => "Vec3",
            Self::Bool(_) => "Bool",
            Self::Clip(_) => "Clip",
            Self::Audio(_) => "Audio",
            Self::Sub(_) => "Sub",
            Self::Video(_) => "Video",
            Self::Event(_) => "Event",
        }
    }

    /// `true` for the keyed channels (`Float`, `Vec3`, `Bool`), which write a property.
    #[must_use]
    pub fn is_keyed(&self) -> bool {
        matches!(self, Self::Float(_) | Self::Vec3(_) | Self::Bool(_))
    }

    /// The number of keys or sections.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Float(k) => k.len(),
            Self::Vec3(k) => k.len(),
            Self::Bool(k) => k.len(),
            Self::Clip(s) => s.len(),
            Self::Audio(s) => s.len(),
            Self::Sub(s) => s.len(),
            Self::Video(s) => s.len(),
            Self::Event(k) => k.len(),
        }
    }

    /// `true` with no keys or sections (validation rejects that).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A named animation of one property of one target, on the master clock.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Unique, non-empty name (what agents, markers and the editor refer to the track by).
    pub name: String,
    /// Binding name the host resolves to whatever it drives (an entity name, a camera name,
    /// "ui", ...). Not resolved by the engine. Required by keyed and clip tracks; unused (may be
    /// empty) on audio and sub-sequence tracks.
    #[serde(default)]
    pub target: String,
    /// Property path on the target (`"transform.position"`, `"opacity"`, `"intensity"`). Its
    /// meaning, unit and range belong to the host. Required by keyed tracks only.
    #[serde(default)]
    pub property: String,
    /// The keyed values.
    pub keys: Channel,
    /// `false` mutes the track: sampling skips it entirely (a muted event track fires nothing).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `true` locks the track in the editor (v6, ADR-8105): the timeline and the `editor.seq_*`
    /// edits refuse to change its keys, sections, name, binding or position (agent code
    /// `seq_track_locked`). Playback ignores it. Left out of the file when `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub locked: bool,
}

/// A named instant on the master clock, fired when playback crosses its time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    /// Unique, non-empty name.
    pub name: String,
    /// Seconds on the master clock, zero or more.
    pub time: f32,
    /// Free-form payload the host interprets (a cue to play, a message to send, a note).
    #[serde(default)]
    pub payload: String,
}

/// Remembers [`Sequence::duration`] so a later sample does not fold every key again.
///
/// Equality is always true: the cache is not part of the sequence. `.seq.ron` skips it
/// (`serde(skip)`), so a round trip stays byte-identical and the format version does not change.
/// The atomics keep [`Sequence`] `Sync` inside `Arc<Sequence>`. The type is public so another
/// crate can build a [`Sequence`] with `..Sequence::default()`; a private field rejects that
/// syntax. Hosts leave the cache at [`DurationCache::default`].
#[derive(Debug)]
pub struct DurationCache {
    stamp: AtomicU64,
    bits: AtomicU64,
}

impl DurationCache {
    pub(crate) fn load(&self, stamp: u64) -> Option<f64> {
        if self.stamp.load(Ordering::Acquire) != stamp {
            return None;
        }
        Some(f64::from_bits(self.bits.load(Ordering::Acquire)))
    }

    pub(crate) fn store(&self, stamp: u64, seconds: f64) {
        // Bits first, then the stamp: a reader that sees `stamp` also sees these bits.
        self.bits.store(seconds.to_bits(), Ordering::Release);
        self.stamp.store(stamp, Ordering::Release);
    }
}

impl Default for DurationCache {
    fn default() -> Self {
        Self {
            stamp: AtomicU64::new(0),
            bits: AtomicU64::new(0),
        }
    }
}

impl Clone for DurationCache {
    fn clone(&self) -> Self {
        Self {
            stamp: AtomicU64::new(self.stamp.load(Ordering::Relaxed)),
            bits: AtomicU64::new(self.bits.load(Ordering::Relaxed)),
        }
    }
}

impl PartialEq for DurationCache {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// The `.seq.ron` document: one master clock of tracks and markers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    /// Format version ([`SEQUENCE_FORMAT_VERSION`]).
    pub version: u32,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Authoring frame rate, frames per second. Only converts frame numbers to seconds;
    /// playback is by time.
    #[serde(default = "default_fps")]
    pub fps: f32,
    /// The tracks. Two tracks may name the same `target` and `property`; the later one wins.
    #[serde(default)]
    pub tracks: Vec<Track>,
    /// The markers.
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// Not part of `.seq.ron`. Leave this at [`DurationCache::default`] (`..Sequence::default()`
    /// does that). See [`DurationCache`].
    #[serde(skip)]
    pub duration_cache: DurationCache,
    /// Bindings the player creates itself from a prefab for a time range, instead of finding an
    /// existing entity (v5, ADR-2948). Left out of a saved file when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawnables: Vec<Spawnable>,
    /// `true` writes back, when the player stops (or lets go at the end), the value every keyed
    /// track's property had before the player first wrote it (v5, ADR-2948). Left out when `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub restore_on_stop: bool,
}

/// `true` for `false` (a serde skip helper).
fn is_false(value: &bool) -> bool {
    !*value
}

/// A binding the player spawns from a prefab (v5, ADR-2948): the entity exists while the clock is
/// in `[start, end)` (and at `end` when that is the player's end and the player holds it), so
/// tracks targeting `binding` animate it; it is despawned when the clock leaves the range, when
/// the player stops, and when it lets go at the end, unless `keep_on_stop`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Spawnable {
    /// The binding name tracks target (unique among the spawnables; it also may not be bound by
    /// hand while the spawnable exists: the spawned entity wins).
    pub binding: String,
    /// The prefab: a project-relative `.prefab.ron` path (`/`-separated, no `..`), or the guid of
    /// a template already in the scene's prefab library (`guid:<32 hex digits>`).
    pub prefab: String,
    /// Seconds on the master clock the entity appears at.
    #[serde(default)]
    pub start: f32,
    /// Seconds it disappears at; `None` keeps it to the end of the sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<f32>,
    /// `true` leaves the entity in the world when the player stops or lets go (it is then the
    /// game's); leaving the range by playback or a seek still despawns it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub keep_on_stop: bool,
}

impl Spawnable {
    /// A spawnable of `prefab` bound as `binding` from `start` to the end of the sequence.
    #[must_use]
    pub fn new(binding: impl Into<String>, prefab: impl Into<String>, start: f32) -> Self {
        Self {
            binding: binding.into(),
            prefab: prefab.into(),
            start,
            end: None,
            keep_on_stop: false,
        }
    }

    /// The guid a `guid:<hex>` prefab reference names; `None` for a path.
    #[must_use]
    pub fn prefab_guid(&self) -> Option<regia_core::id::Guid> {
        let hex = self.prefab.strip_prefix("guid:")?;
        if hex.len() != 32 {
            return None;
        }
        u128::from_str_radix(hex, 16)
            .ok()
            .filter(|&v| v != 0)
            .map(regia_core::id::Guid::from_u128)
    }

    /// `true` when the entity exists at `t` (seconds) on a player of length `duration`: inside
    /// `[start, end)`, or at `end` when that is the player's end (the last frame holds it).
    #[must_use]
    pub fn live_at(&self, t: f64, duration: f64) -> bool {
        // Compared in whole nanoseconds, the player clock's unit: a player's length is a rounded
        // f32 end, and comparing the unrounded value would drop an actor on its held last frame.
        let ns = |s: f64| {
            if s.is_finite() && s > 0.0 {
                (s * 1.0e9).round() as u64
            } else {
                0
            }
        };
        let (t, duration) = (ns(t), ns(duration));
        let start = ns(f64::from(self.start));
        let end = self.end.map_or(duration, |e| ns(f64::from(e)));
        t >= start && (t < end || (t >= duration && end >= duration))
    }
}

impl Default for Sequence {
    fn default() -> Self {
        Self {
            version: SEQUENCE_FORMAT_VERSION,
            name: String::new(),
            fps: default_fps(),
            tracks: Vec::new(),
            markers: Vec::new(),
            duration_cache: DurationCache::default(),
            spawnables: Vec::new(),
            restore_on_stop: false,
        }
    }
}

impl Sequence {
    /// Parses a `.seq.ron`: version check, migration, validation.
    pub fn from_ron(text: &str) -> Result<Self, SequenceError> {
        let found = version::peek_ron_version(text).map_err(SequenceError::Parse)?;
        if found > SEQUENCE_FORMAT_VERSION {
            return Err(SequenceError::Version {
                found,
                expected: SEQUENCE_FORMAT_VERSION,
            });
        }
        let mut sequence: Self = ron::from_str(text).map_err(|e| SequenceError::Parse(e.to_string()))?;
        sequence.migrate_to_latest()?;
        sequence.validate()?;
        Ok(sequence)
    }

    /// Serializes to RON with a trailing newline. Re-saving an unchanged sequence is
    /// byte-identical.
    pub fn to_ron(&self) -> Result<String, SequenceError> {
        let mut text = ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| SequenceError::Parse(e.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// Rejects a newer version and runs the migration chain on an older one
    /// (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, SequenceError> {
        if self.version > SEQUENCE_FORMAT_VERSION {
            return Err(SequenceError::Version {
                found: self.version,
                expected: SEQUENCE_FORMAT_VERSION,
            });
        }
        if self.version < SEQUENCE_FORMAT_VERSION {
            self.version =
                version::migrate(self, self.version, sequence_migrations(), SEQUENCE_FORMAT_VERSION)
                    .map_err(|e| SequenceError::Migration(e.to_string()))?;
        }
        Ok(self.version)
    }

    /// Checks everything sampling relies on: a sane `fps`; tracks with unique non-empty names,
    /// non-empty targets and properties, and channels with at least one key whose times are
    /// finite, zero or more and strictly increasing, with finite values; markers with unique
    /// non-empty names and finite, non-negative times.
    pub fn validate(&self) -> Result<(), SequenceError> {
        let invalid = |m: String| Err(SequenceError::Invalid(m));
        if !(self.fps.is_finite() && self.fps > 0.0 && self.fps <= 1000.0) {
            return invalid(format!("fps {} must be in (0, 1000]", self.fps));
        }
        let mut names = std::collections::BTreeSet::new();
        for track in &self.tracks {
            if track.name.is_empty() {
                return invalid("a track has an empty name".into());
            }
            if !names.insert(track.name.as_str()) {
                return invalid(format!("duplicate track name `{}`", track.name));
            }
            track.validate()?;
        }
        let mut markers = std::collections::BTreeSet::new();
        for marker in &self.markers {
            if marker.name.is_empty() {
                return invalid("a marker has an empty name".into());
            }
            if !markers.insert(marker.name.as_str()) {
                return invalid(format!("duplicate marker name `{}`", marker.name));
            }
            if !(marker.time.is_finite() && marker.time >= 0.0) {
                return invalid(format!(
                    "marker `{}`: time {} must be zero or more",
                    marker.name, marker.time
                ));
            }
        }
        if self.spawnables.len() > MAX_SPAWNABLES {
            return invalid(format!(
                "{} spawnables; the limit is {MAX_SPAWNABLES}",
                self.spawnables.len()
            ));
        }
        let mut bindings = std::collections::BTreeSet::new();
        for spawnable in &self.spawnables {
            spawnable.validate()?;
            if !bindings.insert(spawnable.binding.as_str()) {
                return invalid(format!("duplicate spawnable binding `{}`", spawnable.binding));
            }
        }
        Ok(())
    }
}

impl Spawnable {
    /// A non-empty binding, a prefab that is a `guid:` reference or a relative path with no `..`,
    /// backslash or empty component, a finite `start` of zero or more and an `end` after it.
    pub fn validate(&self) -> Result<(), SequenceError> {
        let at = format!("spawnable `{}`", self.binding);
        let invalid = |m: String| Err(SequenceError::Invalid(format!("{at}: {m}")));
        if self.binding.is_empty() {
            return Err(SequenceError::Invalid("a spawnable has an empty binding".into()));
        }
        if self.prefab.starts_with("guid:") {
            if self.prefab_guid().is_none() {
                return invalid(format!(
                    "prefab `{}` is not `guid:` and 32 hex digits",
                    self.prefab
                ));
            }
        } else if self.prefab.is_empty()
            || self.prefab.starts_with('/')
            || self.prefab.contains('\\')
            || self.prefab.contains(':')
            || self.prefab.split('/').any(|c| c.is_empty() || c == "..")
        {
            return invalid(format!(
                "prefab `{}` must be a relative project path without `..`",
                self.prefab
            ));
        }
        if !(self.start.is_finite() && self.start >= 0.0) {
            return invalid(format!("start {} must be zero or more", self.start));
        }
        if let Some(end) = self.end
            && !(end.is_finite() && end > self.start)
        {
            return invalid(format!("end {end} must come after start {}", self.start));
        }
        Ok(())
    }
}

impl Track {
    /// Validates one track (see [`Sequence::validate`]).
    pub fn validate(&self) -> Result<(), SequenceError> {
        let name = &self.name;
        let needs_target = matches!(
            self.keys,
            Channel::Float(_) | Channel::Vec3(_) | Channel::Bool(_) | Channel::Clip(_)
        );
        if needs_target && self.target.is_empty() {
            return Err(SequenceError::Invalid(format!("track `{name}`: target is empty")));
        }
        if self.keys.is_keyed() && self.property.is_empty() {
            return Err(SequenceError::Invalid(format!(
                "track `{name}`: property is empty"
            )));
        }
        match &self.keys {
            Channel::Float(keys) => {
                check_keys(
                    name,
                    "Float",
                    &mut keys.iter().map(|k| (k.time, k.value.is_finite())),
                )?;
                for k in keys {
                    check_tangent(name, k.time, &tangent_slopes(&k.tangent), k.weights)?;
                }
                Ok(())
            }
            Channel::Vec3(keys) => {
                check_keys(
                    name,
                    "Vec3",
                    &mut keys
                        .iter()
                        .map(|k| (k.time, k.value.iter().all(|v| v.is_finite()))),
                )?;
                for k in keys {
                    let slopes: Vec<f32> = tangent_slopes(&k.tangent)
                        .into_iter()
                        .flat_map(|s| s.into_iter())
                        .collect();
                    check_tangent(name, k.time, &slopes, k.weights)?;
                }
                Ok(())
            }
            Channel::Bool(keys) => check_keys(name, "Bool", &mut keys.iter().map(|k| (k.time, true))),
            Channel::Clip(sections) => {
                check_sections(name, "Clip", sections.len())?;
                for s in sections {
                    let at = format!("track `{name}`: Clip section `{}` at {}", s.clip, s.start);
                    check_span(&at, s.start, s.end)?;
                    if s.clip.is_empty() {
                        return Err(SequenceError::Invalid(format!("{at}: clip name is empty")));
                    }
                    check_rate(&at, "offset", s.offset, s.speed)?;
                    check_ramps(&at, "blend", s.blend_in, s.blend_out, s.end - s.start)?;
                    if !(s.weight.is_finite() && s.weight > 0.0 && s.weight <= 1.0) {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: weight {} must be in (0, 1]",
                            s.weight
                        )));
                    }
                }
                Ok(())
            }
            Channel::Audio(sections) => {
                check_sections(name, "Audio", sections.len())?;
                for s in sections {
                    let at = format!("track `{name}`: Audio section `{}` at {}", s.sound, s.start);
                    check_span(&at, s.start, s.end)?;
                    if s.sound.is_empty() {
                        return Err(SequenceError::Invalid(format!("{at}: sound name is empty")));
                    }
                    if !(s.offset.is_finite() && s.offset >= 0.0) {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: offset {} must be zero or more",
                            s.offset
                        )));
                    }
                    if !(s.volume_db.is_finite() && s.volume_db <= 24.0) {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: volume_db {} must be finite and at most 24",
                            s.volume_db
                        )));
                    }
                    check_ramps(&at, "fade", s.fade_in, s.fade_out, s.end - s.start)?;
                    if s.bus.as_ref().is_some_and(String::is_empty) {
                        return Err(SequenceError::Invalid(format!("{at}: bus name is empty")));
                    }
                    if !(s.time_scale.is_finite() && s.time_scale > 0.0 && s.time_scale <= 16.0) {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: time_scale {} must be in (0, 16]",
                            s.time_scale
                        )));
                    }
                    if let Some(spatial) = &s.spatial {
                        check_spatial(&at, spatial)?;
                    }
                    let (lo, hi) = PRESERVE_PITCH_RANGE;
                    if s.time_scale_mode == TimeScaleMode::PreservePitch && !(lo..=hi).contains(&s.time_scale)
                    {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: a PreservePitch time_scale {} must be in {lo}..={hi}",
                            s.time_scale
                        )));
                    }
                }
                Ok(())
            }
            Channel::Sub(sections) => {
                check_sections(name, "Sub", sections.len())?;
                for s in sections {
                    let at = format!("track `{name}`: Sub section `{}` at {}", s.sequence, s.start);
                    check_span(&at, s.start, s.end)?;
                    if s.sequence.is_empty() {
                        return Err(SequenceError::Invalid(format!("{at}: sequence name is empty")));
                    }
                    check_rate(&at, "offset", s.offset, s.speed)?;
                    for (from, to) in &s.bindings {
                        if from.is_empty() || to.is_empty() {
                            return Err(SequenceError::Invalid(format!(
                                "{at}: a binding rename has an empty name"
                            )));
                        }
                    }
                    if let SubRepeat::Count(n) = s.repeat
                        && !(1..=MAX_LOOP_COUNT).contains(&n)
                    {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: repeat Count({n}) must be in 1..={MAX_LOOP_COUNT}"
                        )));
                    }
                    if let Some(length) = s.loop_length
                        && !(length.is_finite() && length >= MIN_LOOP_LENGTH)
                    {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: loop_length {length} must be at least {MIN_LOOP_LENGTH} s"
                        )));
                    }
                }
                Ok(())
            }
            Channel::Video(sections) => {
                check_sections(name, "Video", sections.len())?;
                for s in sections {
                    let at = format!("track `{name}`: Video section `{}` at {}", s.video, s.start);
                    check_span(&at, s.start, s.end)?;
                    if s.video.is_empty() {
                        return Err(SequenceError::Invalid(format!("{at}: video path is empty")));
                    }
                    if !(s.offset.is_finite() && s.offset >= 0.0) {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: offset {} must be zero or more",
                            s.offset
                        )));
                    }
                    if !(s.volume_db.is_finite() && s.volume_db <= 24.0) {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: volume_db {} must be finite and at most 24",
                            s.volume_db
                        )));
                    }
                    if s.target.binding().is_some_and(str::is_empty) {
                        return Err(SequenceError::Invalid(format!(
                            "{at}: target binding name is empty"
                        )));
                    }
                }
                Ok(())
            }
            Channel::Event(keys) => check_event_keys(name, keys),
        }
    }
}

/// Event keys (v6): at least one, times finite, zero or more and in time order (equal times
/// allowed: they fire in file order), and non-empty event names.
fn check_event_keys(track: &str, keys: &[EventKey]) -> Result<(), SequenceError> {
    let invalid = |m: String| Err(SequenceError::Invalid(format!("track `{track}`: Event {m}")));
    if keys.is_empty() {
        return invalid("needs at least one key".into());
    }
    let mut last = 0.0f32;
    for key in keys {
        if !(key.time.is_finite() && key.time >= 0.0) {
            return invalid(format!("key time {} must be zero or more", key.time));
        }
        if key.time < last {
            return invalid(format!(
                "key times must not decrease (found {} after {last})",
                key.time
            ));
        }
        if key.event.is_empty() {
            return invalid(format!("key at {} has an empty event name", key.time));
        }
        last = key.time;
    }
    Ok(())
}

/// A spatial placement: a non-empty binding or a finite position, distances with
/// `0 < min < max`, a finite non-negative log rolloff and a blend in `0..=1`.
fn check_spatial(at: &str, spatial: &AudioSpatial) -> Result<(), SequenceError> {
    let invalid = |m: String| Err(SequenceError::Invalid(format!("{at}: spatial {m}")));
    match &spatial.source {
        SoundSource::Binding(name) if name.is_empty() => return invalid("binding name is empty".into()),
        SoundSource::Position(p) if !p.iter().all(|v| v.is_finite()) => {
            return invalid(format!("position {p:?} must be finite"));
        }
        _ => {}
    }
    let (min, max) = (spatial.min_distance, spatial.max_distance);
    if !(min.is_finite() && max.is_finite() && min > 0.0 && max > min) {
        return invalid(format!(
            "min_distance {min} and max_distance {max} must be finite with 0 < min < max"
        ));
    }
    if let SoundRolloff::Log(db) = spatial.rolloff
        && !(db.is_finite() && (0.0..=96.0).contains(&db))
    {
        return invalid(format!("Log rolloff {db} dB per doubling must be in 0..=96"));
    }
    if !(spatial.spatial_blend.is_finite() && (0.0..=1.0).contains(&spatial.spatial_blend)) {
        return invalid(format!(
            "spatial_blend {} must be in 0..=1",
            spatial.spatial_blend
        ));
    }
    if !(spatial.doppler.is_finite() && (0.0..=MAX_DOPPLER_FACTOR).contains(&spatial.doppler)) {
        return invalid(format!(
            "doppler {} must be in 0..={MAX_DOPPLER_FACTOR}",
            spatial.doppler
        ));
    }
    Ok(())
}

/// The explicit slopes of a tangent (none for the computed modes).
fn tangent_slopes<S: Copy>(tangent: &Tangent<S>) -> Vec<S> {
    match *tangent {
        Tangent::Auto | Tangent::Flat | Tangent::Linear => Vec::new(),
        Tangent::Aligned(s) => vec![s],
        Tangent::Broken(a, b) => vec![a, b],
    }
}

/// Explicit slopes finite, weights in `(0, 1]`.
fn check_tangent(
    track: &str,
    time: f32,
    slopes: &[f32],
    weights: Option<(f32, f32)>,
) -> Result<(), SequenceError> {
    if slopes.iter().any(|s| !s.is_finite()) {
        return Err(SequenceError::Invalid(format!(
            "track `{track}`: key at {time} has a non-finite tangent slope"
        )));
    }
    if let Some((a, b)) = weights
        && !(a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0 && a <= 1.0 && b <= 1.0)
    {
        return Err(SequenceError::Invalid(format!(
            "track `{track}`: key at {time}: weights ({a}, {b}) must each be in (0, 1]"
        )));
    }
    Ok(())
}

/// A section track holds 1..=[`MAX_SECTIONS`] sections.
fn check_sections(track: &str, what: &str, count: usize) -> Result<(), SequenceError> {
    if count == 0 {
        return Err(SequenceError::Invalid(format!(
            "track `{track}`: {what} needs at least one section"
        )));
    }
    if count > MAX_SECTIONS {
        return Err(SequenceError::Invalid(format!(
            "track `{track}`: {what} has {count} sections; the limit is {MAX_SECTIONS}"
        )));
    }
    Ok(())
}

/// A finite `start` of zero or more and an `end` after it.
fn check_span(at: &str, start: f32, end: f32) -> Result<(), SequenceError> {
    if !(start.is_finite() && start >= 0.0) {
        return Err(SequenceError::Invalid(format!(
            "{at}: start {start} must be zero or more"
        )));
    }
    if !(end.is_finite() && end > start) {
        return Err(SequenceError::Invalid(format!(
            "{at}: end {end} must come after start {start}"
        )));
    }
    Ok(())
}

/// A finite offset and a positive finite speed.
fn check_rate(at: &str, what: &str, offset: f32, speed: f32) -> Result<(), SequenceError> {
    if !offset.is_finite() {
        return Err(SequenceError::Invalid(format!(
            "{at}: {what} {offset} must be finite"
        )));
    }
    if !(speed.is_finite() && speed > 0.0 && speed <= 1000.0) {
        return Err(SequenceError::Invalid(format!(
            "{at}: speed {speed} must be in (0, 1000]"
        )));
    }
    Ok(())
}

/// Ramp lengths zero or more whose sum fits the section.
fn check_ramps(at: &str, what: &str, rise: f32, fall: f32, length: f32) -> Result<(), SequenceError> {
    if !(rise.is_finite() && fall.is_finite() && rise >= 0.0 && fall >= 0.0) {
        return Err(SequenceError::Invalid(format!(
            "{at}: {what}_in {rise} and {what}_out {fall} must be zero or more"
        )));
    }
    if rise + fall > length * (1.0 + 1.0e-6) {
        return Err(SequenceError::Invalid(format!(
            "{at}: {what}_in {rise} + {what}_out {fall} is longer than the section ({length} s)"
        )));
    }
    Ok(())
}

/// Checks one channel's keys: at least one, times finite, zero or more and strictly increasing,
/// values finite (`false` for a channel that cannot be non-finite).
fn check_keys(
    track: &str,
    what: &str,
    keys: &mut dyn Iterator<Item = (f32, bool)>,
) -> Result<(), SequenceError> {
    let invalid = |m: String| Err(SequenceError::Invalid(format!("track `{track}`: {what} {m}")));
    let mut last = f32::NEG_INFINITY;
    let mut count = 0usize;
    for (time, finite) in keys {
        count += 1;
        if !time.is_finite() || time < 0.0 {
            return invalid(format!("key time {time} must be zero or more"));
        }
        if time <= last {
            return invalid(format!(
                "key times must strictly increase (found {time} after {last})"
            ));
        }
        if !finite {
            return invalid(format!("key at {time} has a non-finite value"));
        }
        last = time;
    }
    if count == 0 {
        return invalid("needs at least one key".into());
    }
    Ok(())
}

/// The `.seq.ron` migration chain. v1 → v2 (ADR-1880) only added variants and optional fields
/// (Bezier keys, tangents, weights, the clip, audio and sub-sequence channels), so a v1 document
/// parses as v2 unchanged and the step only bumps the version; `regia upgrade-project` re-saves it.
/// v2 → v3 (ADR-2405) likewise only adds optional fields (spatial sound sections, the voice time
/// scale, looping sub-sequences) whose defaults are the v2 behavior, so it too only bumps the
/// version: a v2 file samples bit for bit as it did.
/// v3 → v4 (ADR-2862) only adds a variant ([`Channel::Video`]), so a v3 document parses unchanged.
/// v4 → v5 adds the optional `spawnables` and `restore_on_stop` (ADR-2948), whose defaults (none,
/// `false`) are the v4 behavior, and Doppler (on by default in v5), occlusion and the
/// pitch-preserving time scale (ADR-3197); the step turns Doppler off on every spatial section the
/// file holds (sub-sequences are their own files and migrate on their own), so an older file sounds
/// as it did.
/// v5 → v6 (ADR-8105) only adds a variant ([`Channel::Event`]) and the optional `locked` (default
/// `false`), so a v5 document parses unchanged and the step only bumps the version.
fn sequence_migrations() -> &'static [VersionStep<Sequence>] {
    &[
        VersionStep {
            from: 1,
            to: 2,
            migrate: |_| Ok(()),
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: |_| Ok(()),
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: |_| Ok(()),
        },
        VersionStep {
            from: 4,
            to: 5,
            migrate: doppler_off,
        },
        VersionStep {
            from: 5,
            to: 6,
            migrate: |_| Ok(()),
        },
    ]
}

/// The v4 → v5 step: Doppler off on every spatial sound section (v4 had none).
fn doppler_off(sequence: &mut Sequence) -> Result<(), String> {
    for track in &mut sequence.tracks {
        if let Channel::Audio(sections) = &mut track.keys {
            for spatial in sections.iter_mut().filter_map(|s| s.spatial.as_mut()) {
                spatial.doppler = 0.0;
            }
        }
    }
    Ok(())
}

/// The `from` versions of the `.seq.ron` migration steps (for `cargo xtask check-formats`).
#[must_use]
pub fn sequence_migration_sources() -> Vec<u32> {
    sequence_migrations().iter().map(|s| s.from).collect()
}

/// The serde shape fingerprint of `.seq.ron` (hashed by `cargo xtask check-formats`): the field
/// list of every struct in the tree, then the variant spellings of every enum.
#[must_use]
pub fn sequence_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<Sequence>(),
        serde_field_names::<Track>(),
        serde_field_names::<Marker>(),
        serde_field_names::<ScalarKey>(),
        serde_field_names::<Vec3Key>(),
        serde_field_names::<BoolKey>(),
        serde_field_names::<ClipSection>(),
        serde_field_names::<AudioSection>(),
        serde_field_names::<SubSection>(),
        serde_field_names::<EventKey>(),
        serde_variant_names::<Channel>(),
        serde_variant_names::<Ease>(),
        serde_variant_names::<Tangent<f32>>(),
        serde_field_names::<AudioSpatial>(),
        serde_variant_names::<SoundSource>(),
        serde_variant_names::<SoundRolloff>(),
        serde_variant_names::<SubRepeat>(),
        serde_field_names::<VideoSection>(),
        serde_variant_names::<VideoTarget>(),
        serde_variant_names::<VideoFit>(),
        serde_variant_names::<VideoScreenSlot>(),
        serde_field_names::<Spawnable>(),
        serde_variant_names::<TimeScaleMode>(),
    ])
}

/// Builders shared by this crate's sequencer tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// A float track animating `target`.`property` from 0 to 1 over `duration` seconds.
    pub(crate) fn float_track(name: &str, target: &str, property: &str, duration: f32) -> Track {
        Track {
            name: name.into(),
            target: target.into(),
            property: property.into(),
            keys: Channel::Float(vec![
                ScalarKey::new(0.0, 0.0, Ease::Linear),
                ScalarKey::new(duration, 1.0, Ease::Linear),
            ]),
            enabled: true,
            locked: false,
        }
    }

    /// A marker named `name` at `time` with no payload.
    pub(crate) fn marker(name: &str, time: f32) -> Marker {
        Marker {
            name: name.into(),
            time,
            payload: String::new(),
        }
    }

    /// A sequence of the given tracks and markers with default metadata.
    pub(crate) fn sequence(tracks: Vec<Track>, markers: Vec<Marker>) -> Sequence {
        Sequence {
            tracks,
            markers,
            ..Sequence::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{float_track, marker, sequence};
    use super::*;

    #[test]
    fn a_v5_file_upgrades_to_v6_unchanged_and_v6_holds_event_tracks_and_locks() {
        let v5 = r#"(version: 5, tracks: [(name: "f", target: "ui", property: "opacity", keys: Float([(time: 0.0, value: 0.0)]))])"#;
        let upgraded = Sequence::from_ron(v5).expect("v5");
        assert_eq!(upgraded.version, 6);
        assert!(!upgraded.tracks[0].locked);
        assert!(
            !upgraded.to_ron().expect("ron").contains("locked"),
            "an unlocked track writes no lock"
        );
        let v6 = r#"(version: 6, tracks: [
            (name: "cues", keys: Event([(time: 1.0, event: "door", payload: "open"), (time: 1.0, event: "bell"), (time: 3.0, event: "end")])),
            (name: "f", target: "ui", property: "opacity", keys: Float([(time: 0.0, value: 0.0)]), locked: true),
            (name: "off", target: "door", keys: Event([(time: 2.0, event: "x")]), enabled: false),
        ])"#;
        let seq = Sequence::from_ron(v6).expect("v6");
        assert_eq!(seq.tracks[0].keys.kind(), "Event");
        assert!(!seq.tracks[0].keys.is_keyed());
        assert_eq!(seq.tracks[0].keys.len(), 3);
        assert!(seq.tracks[1].locked);
        assert_eq!(seq.duration(), 3.0, "event keys count toward the length");
        let text = seq.to_ron().expect("ron");
        assert!(text.contains("locked: true"), "{text}");
        let again = Sequence::from_ron(&text).expect("round trip");
        assert_eq!(again, seq);
        assert_eq!(again.to_ron().expect("ron"), text, "re-saving is byte-identical");
        for (bad, why) in [
            (
                r#"(version: 6, tracks: [(name: "c", keys: Event([(time: 2.0, event: "a"), (time: 1.0, event: "b")]))])"#,
                "decreasing",
            ),
            (
                r#"(version: 6, tracks: [(name: "c", keys: Event([(time: 1.0, event: "")]))])"#,
                "empty name",
            ),
            (
                r#"(version: 6, tracks: [(name: "c", keys: Event([(time: -1.0, event: "a")]))])"#,
                "negative",
            ),
            (
                r#"(version: 6, tracks: [(name: "c", keys: Event([]))])"#,
                "no key",
            ),
        ] {
            assert_eq!(
                Sequence::from_ron(bad).expect_err(why).code(),
                "seq_invalid",
                "{why}"
            );
        }
        assert_eq!(
            Sequence::from_ron("(version: 7)").expect_err("newer").code(),
            "seq_version"
        );
        assert_eq!(sequence_migration_sources(), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_valid_sequence_round_trips_byte_identically() {
        let mut seq = sequence(
            vec![float_track("fade", "ui", "opacity", 2.0)],
            vec![marker("title", 0.5)],
        );
        seq.name = "demo".into();
        if let Channel::Float(keys) = &mut seq.tracks[0].keys {
            keys[0].ease = Ease::Smooth;
        }
        seq.tracks.push(Track {
            name: "move".into(),
            target: "hero".into(),
            property: "transform.position".into(),
            keys: Channel::Vec3(vec![
                Vec3Key::new(0.0, [0.0, 0.0, 0.0], Ease::Linear),
                Vec3Key::new(2.0, [1.0, 2.0, 3.0], Ease::Linear),
            ]),
            enabled: true,
            locked: false,
        });
        seq.markers[0].payload = "cue:ambient".into();
        seq.validate().expect("valid");
        let text = seq.to_ron().expect("serializes");
        let back = Sequence::from_ron(&text).expect("parses");
        assert_eq!(back, seq);
        assert_eq!(back.to_ron().expect("serializes again"), text, "canonical text");
    }

    #[test]
    fn defaults_fill_in_for_a_minimal_hand_written_file() {
        let seq = Sequence::from_ron(
            r#"(version: 1, tracks: [(name: "t", target: "ui", property: "opacity",
                keys: Bool([(time: 0.0, value: false)]))])"#,
        )
        .expect("minimal file");
        assert_eq!(seq.fps, 30.0);
        assert!(seq.name.is_empty() && seq.markers.is_empty());
        assert!(seq.tracks[0].enabled, "enabled defaults to true");
    }

    #[test]
    fn an_empty_sequence_is_valid_and_finishes_at_zero() {
        let seq = Sequence::from_ron("(version: 1)").expect("bare document");
        seq.validate().expect("valid");
        assert_eq!(seq.duration(), 0.0);
    }

    #[test]
    fn a_newer_version_is_rejected_before_the_body_is_read() {
        let err = Sequence::from_ron("(version: 99, something_new: true)").expect_err("too new");
        assert!(matches!(err, SequenceError::Version { found: 99, .. }), "{err}");
    }

    /// v5 (ADR-3197): a v4 file's spatial sections migrate with Doppler off, a v5 file that leaves
    /// `doppler` out gets 1, the new fields are left out of a saved file at their defaults and
    /// round-trip when set, and the mode names parse.
    #[test]
    fn v5_doppler_occlusion_and_time_scale_mode_migrate_and_round_trip() {
        let body = r#"tracks: [(name: "steps", keys: Audio([
            (sound: "step", start: 0.0, end: 2.0, spatial: Some((source: Binding("hero")))),
            (sound: "line", start: 0.0, end: 2.0, time_scale: 0.75),
        ]))])"#;
        let sounds = |seq: &Sequence| -> Vec<AudioSection> {
            match &seq.tracks[0].keys {
                Channel::Audio(list) => list.clone(),
                _ => panic!("audio"),
            }
        };
        let v4 = Sequence::from_ron(&format!("(version: 4, {body}")).expect("v4");
        assert_eq!(v4.version, SEQUENCE_FORMAT_VERSION);
        let migrated = sounds(&v4);
        assert_eq!(migrated[0].spatial.as_ref().map(|s| s.doppler), Some(0.0));
        assert_eq!(migrated[1].time_scale_mode, TimeScaleMode::Varispeed);
        let v5 = Sequence::from_ron(&format!("(version: 5, {body}")).expect("v5");
        let fresh = sounds(&v5);
        let spatial = fresh[0].spatial.clone().expect("spatial");
        assert_eq!((spatial.doppler, spatial.occlusion), (1.0, false));
        let text = v5.to_ron().expect("save");
        assert!(
            !text.contains("doppler") && !text.contains("occlusion") && !text.contains("time_scale_mode"),
            "defaults stay out: {text}"
        );
        assert!(v4.to_ron().expect("save").contains("doppler: 0.0"));
        let mut set = v5.clone();
        if let Channel::Audio(list) = &mut set.tracks[0].keys {
            if let Some(spatial) = list[0].spatial.as_mut() {
                spatial.doppler = 0.5;
                spatial.occlusion = true;
            }
            list[1].time_scale_mode = TimeScaleMode::PreservePitch;
        }
        set.validate().expect("valid");
        let text = set.to_ron().expect("save");
        assert!(
            text.contains("doppler: 0.5") && text.contains("occlusion: true"),
            "{text}"
        );
        assert!(text.contains("time_scale_mode: PreservePitch"), "{text}");
        assert_eq!(Sequence::from_ron(&text).expect("re-parse"), set);
        for mode in [TimeScaleMode::Varispeed, TimeScaleMode::PreservePitch] {
            assert_eq!(TimeScaleMode::parse(mode.name()), Some(mode));
        }
        assert_eq!(TimeScaleMode::parse("tape"), None);
        assert_eq!(sequence_migration_sources(), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn the_published_example_parses_and_validates() {
        let text = include_str!("../../../../docs/formats/seq.example.ron");
        let seq = Sequence::from_ron(text).expect("docs/formats/seq.example.ron parses");
        assert_eq!(seq.version, SEQUENCE_FORMAT_VERSION);
        assert_eq!(seq.tracks.len(), 7);
        assert_eq!(seq.markers.len(), 3);
        let kinds: Vec<&str> = seq.tracks.iter().map(|t| t.keys.kind()).collect();
        assert_eq!(kinds, ["Float", "Vec3", "Clip", "Audio", "Sub", "Bool", "Video"]);
        let again = Sequence::from_ron(&seq.to_ron().expect("serializes")).expect("re-parses");
        assert_eq!(again, seq);
    }

    #[test]
    fn validation_names_the_track_and_the_problem() {
        let bad = |edit: fn(&mut Sequence)| {
            let mut seq = sequence(
                vec![
                    float_track("a", "ui", "opacity", 2.0),
                    float_track("b", "ui", "scale", 2.0),
                ],
                vec![marker("m", 1.0)],
            );
            edit(&mut seq);
            seq.validate().expect_err("invalid").to_string()
        };
        assert!(bad(|s| s.tracks[1].name = "a".into()).contains("duplicate track name `a`"));
        assert!(bad(|s| s.tracks[0].name.clear()).contains("empty name"));
        assert!(bad(|s| s.tracks[0].target.clear()).contains("track `a`: target"));
        assert!(bad(|s| s.tracks[0].property.clear()).contains("track `a`: property"));
        assert!(
            bad(|s| {
                if let Channel::Float(keys) = &mut s.tracks[0].keys {
                    keys.clear();
                }
            })
            .contains("at least one key")
        );
        assert!(
            bad(|s| {
                if let Channel::Float(keys) = &mut s.tracks[0].keys {
                    keys[1].time = 0.0;
                }
            })
            .contains("strictly increase")
        );
        assert!(
            bad(|s| {
                if let Channel::Float(keys) = &mut s.tracks[0].keys {
                    keys[1].time = f32::NAN;
                }
            })
            .contains("zero or more")
        );
        assert!(
            bad(|s| {
                if let Channel::Float(keys) = &mut s.tracks[0].keys {
                    keys[0].value = f32::INFINITY;
                }
            })
            .contains("non-finite value")
        );
        assert!(
            bad(|s| {
                // The fixture's track 0 is a Float channel: swap in a Vec3 channel with a NaN.
                s.tracks[0].keys = Channel::Vec3(vec![Vec3Key::new(0.0, [0.0, f32::NAN, 0.0], Ease::Linear)]);
            })
            .contains("Vec3")
        );
        assert!(bad(|s| s.fps = 0.0).contains("fps"));
        assert!(bad(|s| s.markers[0].name.clear()).contains("marker"));
        assert!(
            bad(|s| {
                s.markers.push(marker("m", 0.0));
            })
            .contains("duplicate marker name `m`")
        );
        assert!(bad(|s| s.markers[0].time = -1.0).contains("marker `m`: time"));
    }

    #[test]
    fn the_fingerprint_lists_every_struct_and_enum() {
        let fp = sequence_schema_fingerprint();
        for needle in [
            "tracks,markers",
            "target,property,keys,enabled",
            "payload",
            "Float,Vec3,Bool",
            "Linear,Step,Smooth",
        ] {
            assert!(fp.contains(needle), "{needle} missing from {fp}");
        }
        assert!(fp.contains("Float,Vec3,Bool,Clip,Audio,Sub"), "{fp}");
        assert!(fp.contains("Linear,Step,Smooth,Bezier"), "{fp}");
        assert!(fp.contains("Auto,Flat,Linear,Aligned,Broken"), "{fp}");
        assert!(
            fp.contains("clip,start,end,offset,speed,looping,blend_in,blend_out,weight"),
            "{fp}"
        );
        assert!(fp.contains("sequence,start,end,offset,speed,bindings"), "{fp}");
        assert!(fp.contains("time,value,ease,tangent,weights"), "{fp}");
        assert_eq!(
            sequence_migration_sources(),
            vec![1, 2, 3, 4, 5],
            "v1 -> v2 -> v3 -> v4 -> v5 -> v6"
        );
        assert!(
            fp.contains("source,min_distance,max_distance,rolloff,spatial_blend"),
            "{fp}"
        );
        assert!(fp.contains("Binding,Position"), "{fp}");
        assert!(fp.contains("Inverse,Linear,Log"), "{fp}");
        assert!(fp.contains("Once,Count,UntilStop"), "{fp}");
        assert!(fp.contains("bus,time_scale,spatial"), "{fp}");
        assert!(fp.contains("bindings,repeat,loop_length"), "{fp}");
    }

    /// A v1 document of every v1 feature, as v0.2.x wrote it.
    const V1_FIXTURE: &str = r#"(
    version: 1,
    name: "old",
    fps: 24.0,
    tracks: [
        (name: "fade", target: "ui", property: "opacity",
            keys: Float([(time: 0.0, value: 0.0, ease: Smooth), (time: 2.0, value: 1.0)])),
        (name: "move", target: "hero", property: "transform.position",
            keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 1.0, value: (1.0, 2.0, 3.0), ease: Step)])),
        (name: "show", target: "hero", property: "visible", keys: Bool([(time: 0.5, value: true)]), enabled: false),
    ],
    markers: [(name: "go", time: 1.0, payload: "cue")],
)"#;

    #[test]
    fn a_v1_file_migrates_to_the_current_version_with_the_same_meaning() {
        let seq = Sequence::from_ron(V1_FIXTURE).expect("v1 loads");
        assert_eq!(seq.version, SEQUENCE_FORMAT_VERSION);
        assert_eq!(seq.version, 6);
        assert_eq!(seq.fps, 24.0);
        assert_eq!(seq.tracks.len(), 3);
        let Channel::Float(keys) = &seq.tracks[0].keys else {
            panic!("float")
        };
        assert_eq!(keys[0].ease, Ease::Smooth);
        assert!(
            keys.iter()
                .all(|k| k.tangent == Tangent::Auto && k.weights.is_none())
        );
        assert!(!seq.tracks[2].enabled);
        // Sampling means the same: smoothstep midpoint, the stepped vec3, the marker.
        assert_eq!(
            seq.sample(1.0).values[0].value,
            crate::sequencer::ChannelValue::Float(0.5)
        );
        assert_eq!(seq.markers[0].payload, "cue");
        // The re-save is current and canonical: no tangent or weight noise on v1 keys.
        let text = seq.to_ron().expect("serializes");
        assert!(text.contains(&format!("version: {SEQUENCE_FORMAT_VERSION}")));
        assert!(!text.contains("tangent") && !text.contains("weights"), "{text}");
        assert_eq!(Sequence::from_ron(&text).expect("re-parses"), seq);
        assert_eq!(
            Sequence::from_ron(&text).expect("again").to_ron().expect("ser"),
            text
        );
        // A newer file is refused before its body is read.
        let newer = SEQUENCE_FORMAT_VERSION + 1;
        assert!(matches!(
            Sequence::from_ron(&format!("(version: {newer}, anything: 1)")),
            Err(SequenceError::Version {
                found,
                expected: SEQUENCE_FORMAT_VERSION
            }) if found == newer
        ));
    }

    #[test]
    fn v2_tracks_round_trip_byte_identically() {
        let text = r#"(
    version: 2,
    tracks: [
        (name: "curve", target: "ui", property: "opacity", keys: Float([
            (time: 0.0, value: 0.0, ease: Bezier, tangent: Aligned(2.0), weights: Some((0.2, 0.5))),
            (time: 1.0, value: 1.0, ease: Bezier, tangent: Broken(0.0, -1.0)),
            (time: 2.0, value: 0.5, tangent: Flat),
        ])),
        (name: "arm", target: "hero", property: "transform.position", keys: Vec3([
            (time: 0.0, value: (0.0, 0.0, 0.0), ease: Bezier, tangent: Aligned((1.0, 0.0, 0.0))),
            (time: 1.0, value: (1.0, 0.0, 0.0), tangent: Linear),
        ])),
        (name: "body", target: "hero", keys: Clip([
            (clip: "walk", start: 0.0, end: 2.0, looping: true, blend_out: 0.5),
            (clip: "wave", start: 1.5, end: 3.0, offset: 0.2, speed: 1.5, blend_in: 0.5, weight: 0.8),
        ])),
        (name: "music", keys: Audio([(sound: "theme", start: 0.0, end: 3.0, volume_db: -3.0, fade_out: 1.0, bus: Some("Music"))])),
        (name: "fx", keys: Sub([(sequence: "fx/spark.seq.ron", start: 1.0, end: 2.0, bindings: {"lamp": "porch"})])),
    ],
)"#;
        let seq = Sequence::from_ron(text).expect("v2 parses");
        assert_eq!(seq.duration(), 3.0);
        let saved = seq.to_ron().expect("serializes");
        let back = Sequence::from_ron(&saved).expect("re-parses");
        assert_eq!(back, seq);
        assert_eq!(back.to_ron().expect("ser"), saved, "canonical");
        let Channel::Clip(sections) = &seq.tracks[2].keys else {
            panic!("clip")
        };
        assert_eq!((sections[0].speed, sections[0].weight), (1.0, 1.0), "defaults");
        assert!(seq.tracks[3].target.is_empty() && seq.tracks[3].property.is_empty());
    }

    #[test]
    fn v2_validation_names_the_section_and_the_problem() {
        let base = |keys: Channel| {
            sequence(
                vec![Track {
                    name: "t".into(),
                    target: "hero".into(),
                    property: String::new(),
                    keys,
                    enabled: true,
                    locked: false,
                }],
                vec![],
            )
        };
        let clip = |edit: fn(&mut ClipSection)| {
            let mut s = ClipSection {
                clip: "walk".into(),
                start: 0.0,
                end: 1.0,
                offset: 0.0,
                speed: 1.0,
                looping: false,
                blend_in: 0.0,
                blend_out: 0.0,
                weight: 1.0,
            };
            edit(&mut s);
            base(Channel::Clip(vec![s]))
                .validate()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        assert_eq!(clip(|_| {}), "");
        assert!(clip(|s| s.end = 0.0).contains("end 0 must come after start 0"));
        assert!(clip(|s| s.start = -1.0).contains("start -1 must be zero or more"));
        assert!(clip(|s| s.clip.clear()).contains("clip name is empty"));
        assert!(clip(|s| s.speed = 0.0).contains("speed 0"));
        assert!(clip(|s| s.weight = 1.5).contains("weight 1.5"));
        assert!(
            clip(|s| {
                s.blend_in = 0.6;
                s.blend_out = 0.6;
            })
            .contains("longer than the section")
        );
        assert!(
            base(Channel::Clip(vec![]))
                .validate()
                .expect_err("empty")
                .to_string()
                .contains("at least one section")
        );
        let mut no_target = base(Channel::Clip(vec![]));
        no_target.tracks[0].target.clear();
        assert!(
            no_target
                .validate()
                .expect_err("target")
                .to_string()
                .contains("target is empty")
        );

        let audio = |edit: fn(&mut AudioSection)| {
            let mut s = AudioSection::new("theme", 0.0, 1.0);
            edit(&mut s);
            let mut seq = base(Channel::Audio(vec![s]));
            seq.tracks[0].target.clear();
            seq.validate().err().map(|e| e.to_string()).unwrap_or_default()
        };
        assert_eq!(audio(|_| {}), "", "audio needs no target");
        assert!(audio(|s| s.sound.clear()).contains("sound name is empty"));
        assert!(audio(|s| s.offset = -1.0).contains("offset -1"));
        assert!(audio(|s| s.volume_db = 40.0).contains("volume_db 40"));
        assert!(audio(|s| s.fade_in = 2.0).contains("longer than the section"));
        assert!(audio(|s| s.bus = Some(String::new())).contains("bus name is empty"));

        let sub = |edit: fn(&mut SubSection)| {
            let mut s = SubSection::new("fx", 0.0, 1.0);
            edit(&mut s);
            base(Channel::Sub(vec![s]))
                .validate()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        assert_eq!(sub(|_| {}), "");
        assert!(sub(|s| s.sequence.clear()).contains("sequence name is empty"));
        assert!(sub(|s| s.offset = f32::NAN).contains("offset NaN must be finite"));
        assert!(
            sub(|s| {
                s.bindings.insert(String::new(), "x".into());
            })
            .contains("empty name")
        );

        let key = |tangent: Tangent<f32>, weights: Option<(f32, f32)>| {
            let mut k = ScalarKey::new(0.0, 0.0, Ease::Bezier);
            k.tangent = tangent;
            k.weights = weights;
            let mut seq = base(Channel::Float(vec![k]));
            seq.tracks[0].property = "opacity".into();
            seq.validate().err().map(|e| e.to_string()).unwrap_or_default()
        };
        assert_eq!(key(Tangent::Broken(1.0, -2.0), Some((0.1, 1.0))), "");
        assert!(key(Tangent::Aligned(f32::INFINITY), None).contains("non-finite tangent"));
        assert!(key(Tangent::Auto, Some((0.0, 0.5))).contains("weights (0, 0.5)"));
        assert!(key(Tangent::Auto, Some((0.5, 1.5))).contains("must each be in (0, 1]"));
    }

    /// A v2 document using every v2 section kind, as v0.2.x wrote it.
    const V2_FIXTURE: &str = r#"(
    version: 2,
    name: "v2",
    tracks: [
        (name: "curve", target: "ui", property: "opacity", keys: Float([
            (time: 0.0, value: 0.0, ease: Bezier, tangent: Aligned(2.0), weights: Some((0.2, 0.5))),
            (time: 1.0, value: 1.0, ease: Smooth),
            (time: 2.0, value: 0.5),
        ])),
        (name: "music", keys: Audio([(sound: "theme", start: 0.0, end: 3.0, offset: 0.25, volume_db: -3.0, fade_out: 1.0)])),
        (name: "fx", keys: Sub([(sequence: "fx.seq.ron", start: 1.0, end: 2.5, offset: 0.5, speed: 2.0, bindings: {"lamp": "porch"})])),
    ],
    markers: [(name: "m", time: 0.5)],
)"#;

    #[test]
    fn a_v2_file_loads_as_the_current_version_with_only_its_version_changed() {
        let seq = Sequence::from_ron(V2_FIXTURE).expect("v2 loads");
        assert_eq!(seq.version, SEQUENCE_FORMAT_VERSION);
        let Channel::Audio(sounds) = &seq.tracks[1].keys else {
            panic!("audio")
        };
        assert_eq!(sounds[0].time_scale, 1.0);
        assert!(sounds[0].spatial.is_none());
        let Channel::Sub(subs) = &seq.tracks[2].keys else {
            panic!("sub")
        };
        assert_eq!(subs[0].repeat, SubRepeat::Once);
        assert!(subs[0].loop_length.is_none());
        // The re-save names no v3 field: only the version line differs from a v2 re-save.
        let text = seq.to_ron().expect("serializes");
        for v3 in ["time_scale", "spatial", "repeat", "loop_length"] {
            assert!(!text.contains(v3), "{v3} in {text}");
        }
        let mut as_v2 = seq.clone();
        as_v2.version = 2;
        let v2_text = as_v2.to_ron().expect("serializes");
        assert_eq!(
            text.replacen(&format!("version: {SEQUENCE_FORMAT_VERSION}"), "version: 2", 1),
            v2_text
        );
    }

    #[test]
    fn v3_fields_round_trip_byte_identically_and_validate() {
        let text = r#"(
    version: 3,
    tracks: [
        (name: "steps", keys: Audio([
            (sound: "step", start: 0.0, end: 2.0, time_scale: 1.5,
                spatial: Some((source: Binding("hero"), min_distance: 2.0, max_distance: 30.0, rolloff: Log(6.0), spatial_blend: 0.5))),
            (sound: "bell", start: 1.0, end: 3.0, spatial: Some((source: Position((1.0, 2.0, -3.0))))),
        ])),
        (name: "loop", keys: Sub([
            (sequence: "a", start: 0.0, end: 4.0, repeat: Count(3), loop_length: Some(0.5)),
            (sequence: "b", start: 4.0, end: 9.0, repeat: UntilStop),
        ])),
    ],
)"#;
        let seq = Sequence::from_ron(text).expect("v3 parses");
        let saved = seq.to_ron().expect("serializes");
        let back = Sequence::from_ron(&saved).expect("re-parses");
        assert_eq!(back, seq);
        assert_eq!(back.to_ron().expect("ser"), saved, "canonical");
        let Channel::Audio(sounds) = &seq.tracks[0].keys else {
            panic!("audio")
        };
        let bell = sounds[1].spatial.as_ref().expect("spatial");
        assert_eq!(bell.source, SoundSource::Position([1.0, 2.0, -3.0]));
        assert_eq!(
            (bell.min_distance, bell.max_distance, bell.spatial_blend),
            (1.0, 50.0, 1.0)
        );
        assert_eq!(bell.rolloff, SoundRolloff::Inverse);

        let audio = |edit: fn(&mut AudioSection)| {
            let mut s = AudioSection::new("theme", 0.0, 1.0);
            s.spatial = Some(AudioSpatial::new(SoundSource::Binding("hero".into())));
            edit(&mut s);
            sequence(
                vec![Track {
                    name: "t".into(),
                    target: String::new(),
                    property: String::new(),
                    keys: Channel::Audio(vec![s]),
                    enabled: true,
                    locked: false,
                }],
                vec![],
            )
            .validate()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
        };
        assert_eq!(audio(|_| {}), "");
        assert!(audio(|s| s.time_scale = 0.0).contains("time_scale 0"));
        assert!(audio(|s| s.time_scale = 17.0).contains("time_scale 17"));
        fn spatial(s: &mut AudioSection) -> &mut AudioSpatial {
            s.spatial.as_mut().expect("spatial")
        }
        assert!(
            audio(|s| spatial(s).source = SoundSource::Binding(String::new()))
                .contains("binding name is empty")
        );
        assert!(
            audio(|s| spatial(s).source = SoundSource::Position([f32::NAN, 0.0, 0.0]))
                .contains("must be finite")
        );
        assert!(audio(|s| spatial(s).min_distance = 0.0).contains("0 < min < max"));
        assert!(audio(|s| spatial(s).max_distance = 0.5).contains("0 < min < max"));
        assert!(audio(|s| spatial(s).rolloff = SoundRolloff::Log(-1.0)).contains("Log rolloff"));
        assert!(audio(|s| spatial(s).spatial_blend = 1.5).contains("spatial_blend 1.5"));
        // v5 (ADR-3197): Doppler factor 0..=4, a pitch-preserving time scale in 0.5..=2.
        assert_eq!(audio(|s| spatial(s).doppler = MAX_DOPPLER_FACTOR), "");
        assert!(audio(|s| spatial(s).doppler = 4.5).contains("doppler 4.5"));
        assert!(audio(|s| spatial(s).doppler = -0.1).contains("doppler -0.1"));
        assert!(audio(|s| spatial(s).doppler = f32::NAN).contains("doppler NaN"));
        assert_eq!(
            audio(|s| {
                s.time_scale = 2.0;
                s.time_scale_mode = TimeScaleMode::PreservePitch;
            }),
            ""
        );
        assert!(
            audio(|s| {
                s.time_scale = 2.5;
                s.time_scale_mode = TimeScaleMode::PreservePitch;
            })
            .contains("PreservePitch time_scale 2.5")
        );
        assert_eq!(audio(|s| s.time_scale = 2.5), "", "varispeed keeps (0, 16]");

        let sub = |edit: fn(&mut SubSection)| {
            let mut s = SubSection::new("fx", 0.0, 1.0);
            edit(&mut s);
            sequence(
                vec![Track {
                    name: "t".into(),
                    target: String::new(),
                    property: String::new(),
                    keys: Channel::Sub(vec![s]),
                    enabled: true,
                    locked: false,
                }],
                vec![],
            )
            .validate()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
        };
        assert_eq!(sub(|s| s.repeat = SubRepeat::UntilStop), "");
        assert!(sub(|s| s.repeat = SubRepeat::Count(0)).contains("Count(0)"));
        assert!(sub(|s| s.repeat = SubRepeat::Count(MAX_LOOP_COUNT + 1)).contains("must be in 1..="));
        assert!(sub(|s| s.loop_length = Some(0.0)).contains("loop_length 0"));
        assert!(sub(|s| s.loop_length = Some(f32::INFINITY)).contains("loop_length inf"));
    }

    /// A v3 fixture of every section kind (ADR-2862): it must load as v4 with only its version
    /// changed, and sample the same.
    const V3_FILE: &str = r#"(version: 3, name: "old", tracks: [
        (name: "move", target: "hero", property: "transform.position",
            keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 2.0, value: (1.0, 0.0, 0.0))])),
        (name: "music", keys: Audio([(sound: "theme", start: 0.0, end: 2.0, time_scale: 2.0)])),
        (name: "fx", keys: Sub([(sequence: "fx.seq.ron", start: 1.0, end: 2.5, repeat: Count(2))])),
    ], markers: [(name: "cue", time: 1.0)])"#;

    #[test]
    fn a_v3_file_loads_as_v4_with_only_its_version_changed() {
        let seq = Sequence::from_ron(V3_FILE).expect("v3 parses");
        assert_eq!(seq.version, SEQUENCE_FORMAT_VERSION);
        assert_eq!(seq.tracks.len(), 3);
        assert!(!seq.tracks.iter().any(|t| matches!(t.keys, Channel::Video(_))));
        let mut as_written: Sequence = ron::from_str(V3_FILE).expect("the same body");
        assert_eq!(as_written.version, 3);
        as_written.version = SEQUENCE_FORMAT_VERSION;
        assert_eq!(seq, as_written, "the migration changes nothing but the version");
        assert_eq!(sequence_migration_sources(), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn v4_video_sections_round_trip_byte_identically_and_keep_their_fields() {
        let text = r#"(version: 4, name: "tv", tracks: [
            (name: "wall", keys: Video([
                (video: "movies/a.webm", start: 1.0, end: 3.0, offset: 0.5, volume_db: -6.0, looping: true,
                    target: Screen(binding: "tv", slot: Albedo)),
                (video: "movies/b.video.ron", start: 3.0, end: 4.0, target: Player("kiosk")),
                (video: "movies/c.webm", start: 4.0, end: 5.0, target: UiImage("hud/slate")),
                (video: "movies/d.webm", start: 5.0, end: 6.0, target: Fullscreen(layer: -1, fit: Fill)),
                (video: "movies/e.webm", start: 6.0, end: 7.0),
                (video: "movies/f.webm", start: 7.0, end: 8.0, target: Fullscreen()),
            ])),
        ])"#;
        let seq = Sequence::from_ron(text).expect("valid");
        let Channel::Video(sections) = &seq.tracks[0].keys else {
            panic!("a video track");
        };
        assert_eq!(
            sections[0].target,
            VideoTarget::Screen {
                binding: "tv".into(),
                slot: VideoScreenSlot::Albedo
            }
        );
        assert_eq!(
            (sections[0].offset, sections[0].volume_db, sections[0].looping),
            (0.5, -6.0, true)
        );
        assert_eq!(sections[1].target.binding(), Some("kiosk"));
        assert_eq!(sections[2].target.kind(), "ui_image");
        assert_eq!(
            sections[3].target,
            VideoTarget::Fullscreen {
                layer: -1,
                fit: VideoFit::Fill
            }
        );
        assert_eq!(
            sections[4].target,
            VideoTarget::default(),
            "a left-out target is a fullscreen layer"
        );
        assert_eq!(
            sections[5].target,
            VideoTarget::default(),
            "fullscreen fields default"
        );
        assert_eq!(sections[0].source_time(2.0), 1.5);
        let saved = seq.to_ron().expect("saves");
        let again = Sequence::from_ron(&saved).expect("reloads");
        assert_eq!(again, seq);
        assert_eq!(
            again.to_ron().expect("saves"),
            saved,
            "re-saving is byte-identical"
        );
    }

    #[test]
    fn v4_validation_names_the_video_section_and_the_problem() {
        let base = |s: VideoSection| Sequence {
            tracks: vec![Track {
                name: "wall".into(),
                target: String::new(),
                property: String::new(),
                keys: Channel::Video(vec![s]),
                enabled: true,
                locked: false,
            }],
            ..Sequence::default()
        };
        let bad = |s: VideoSection, needle: &str| {
            let text = base(s).validate().expect_err("invalid").to_string();
            assert!(text.contains("wall") && text.contains(needle), "{text}");
        };
        let good = VideoSection::new("a.webm", 0.0, 1.0);
        assert!(base(good.clone()).validate().is_ok());
        bad(
            VideoSection {
                video: String::new(),
                ..good.clone()
            },
            "video path is empty",
        );
        bad(
            VideoSection {
                end: 0.0,
                ..good.clone()
            },
            "a.webm",
        );
        bad(
            VideoSection {
                start: -1.0,
                ..good.clone()
            },
            "a.webm",
        );
        bad(
            VideoSection {
                offset: -0.5,
                ..good.clone()
            },
            "offset",
        );
        bad(
            VideoSection {
                offset: f32::NAN,
                ..good.clone()
            },
            "offset",
        );
        bad(
            VideoSection {
                volume_db: 25.0,
                ..good.clone()
            },
            "volume_db",
        );
        bad(
            VideoSection {
                volume_db: f32::INFINITY,
                ..good.clone()
            },
            "volume_db",
        );
        bad(
            VideoSection {
                target: VideoTarget::Player(String::new()),
                ..good.clone()
            },
            "binding",
        );
        bad(
            VideoSection {
                target: VideoTarget::UiImage(String::new()),
                ..good.clone()
            },
            "binding",
        );
        bad(
            VideoSection {
                target: VideoTarget::Screen {
                    binding: String::new(),
                    slot: VideoScreenSlot::Emissive,
                },
                ..good
            },
            "binding",
        );
    }

    #[test]
    fn every_error_has_a_lowercase_code() {
        let errors = [
            SequenceError::LoopTooDeep {
                track: "t".into(),
                depth: 5,
            },
            SequenceError::Version {
                found: 9,
                expected: 3,
            },
            SequenceError::Parse(String::new()),
            SequenceError::Invalid(String::new()),
            SequenceError::Migration(String::new()),
        ];
        let codes: Vec<&str> = errors.iter().map(SequenceError::code).collect();
        assert_eq!(
            codes,
            [
                "seq_loop_too_deep",
                "seq_version",
                "seq_parse",
                "seq_invalid",
                "seq_migration"
            ]
        );
        assert!(
            codes
                .iter()
                .all(|c| c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        );
    }
}
