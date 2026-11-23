// SPDX-License-Identifier: MPL-2.0
//! Music: streaming playback with crossfades and intensity-driven stems switching on bar
//! boundaries (`docs/specs/16-audio.md` § 3a).
//!
//! A [`MusicAsset`] is a set of stems that play in sync (all started together on the Music bus)
//! with per-stem volume driven by a game-state "intensity" through per-stem polylines. Level
//! changes requested mid-bar are deferred to the next bar boundary (from the BPM/meter
//! metadata), so the arrangement stays musical.
//!
//! # The audio clock (ADR-2517)
//!
//! Everything musical is timed on one clock inside the mixer, never on the frame clock: one kira
//! clock ticking once per second, read as `ticks + fraction` seconds. A [`BeatGrid`] turns those
//! seconds into beats, bars and segments. A scheduled sound (a stem, a [`MusicStinger`], a
//! transition segment) is handed to the mixer ahead of time in a `MusicVoice`, which stays silent
//! until the frame nearest its start time and splits the block there, so it starts on an exact
//! sample whatever size the mixer's blocks and the frame loop's steps are. The game thread only
//! decides and reads the clock, so [`MusicBeat`] messages arrive up to a frame late, each carrying
//! the exact time of its beat.
//!
//! # Format
//!
//! `.music.ron` is a frozen format (`docs/specs/12-assets.md` § 4a, [`MUSIC_VERSION`],
//! `docs/formats/music-ron.md`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kira::Frame;
use kira::clock::{ClockHandle, ClockId};
use kira::info::Info;
use kira::sound::{Sound, SoundData};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::AudioError;
use crate::instance::{DECLICK_FADE, SoundInstance};
use crate::sound::{LoopRegion, PlaybackParams, SoundAsset};
use crate::spatial::polyline;

/// The `.music.ron` format version. v1 has no `version` field and only stems; v2 adds
/// `segment_bars`, `stingers` and `transitions`, all optional, so v1 files read unchanged.
pub const MUSIC_VERSION: u32 = 2;

/// How far ahead of the clock a scheduled sound is handed to the mixer. It covers the output's
/// callback (the mixer reads new sounds once per callback, and the clock this thread reads lags by
/// one) on every platform; a boundary nearer than this goes to the next beat or bar.
pub const MUSIC_SCHEDULE_LEAD_SECS: f64 = 0.05;

/// The `to` of a transition rule that applies to every track without a rule of its own.
pub const MUSIC_ANY_TRACK: &str = "*";

/// When a stinger or a transition takes effect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MusicSync {
    /// As soon as the mixer can start it ([`MUSIC_SCHEDULE_LEAD_SECS`]).
    Immediate,
    /// On the next beat of the playing track.
    NextBeat,
    /// On the next bar of the playing track.
    #[default]
    NextBar,
    /// At the end of the playing track's segment: every [`MusicDef::segment_bars`] bars, or the
    /// length of its longest stem when that is not set.
    EndOfSegment,
}

impl MusicSync {
    /// The name used by the agent protocol (`"next_bar"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::NextBeat => "next_beat",
            Self::NextBar => "next_bar",
            Self::EndOfSegment => "end_of_segment",
        }
    }

    /// The value of a name from [`Self::as_str`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Immediate, Self::NextBeat, Self::NextBar, Self::EndOfSegment]
            .into_iter()
            .find(|sync| sync.as_str() == name)
    }
}

/// One stem of a music track (RON form).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StemDef {
    /// Stem name (`"drums"`, `"strings"`).
    pub name: String,
    /// Path of the sound file.
    pub sound: String,
    /// Base volume in dB.
    #[serde(default)]
    pub volume_db: f32,
    /// Intensity curve: polyline of `(intensity 0..1, dB offset)`.
    #[serde(default)]
    pub intensity: Vec<(f32, f32)>,
}

/// A stinger of a music track (RON form): a one-shot quantized to the track's grid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MusicStingerDef {
    /// Stinger name (`"victory"`).
    pub name: String,
    /// Path of the sound file.
    pub sound: String,
    /// Volume in dB.
    #[serde(default)]
    pub volume_db: f32,
    /// When it plays.
    #[serde(default)]
    pub sync: MusicSync,
}

/// A sound played once between two tracks (RON form); the next track starts when it ends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MusicSegmentDef {
    /// Path of the sound file.
    pub sound: String,
    /// Volume in dB.
    #[serde(default)]
    pub volume_db: f32,
}

/// How a track hands over to another (RON form).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MusicTransitionDef {
    /// The track this rule leads to, or [`MUSIC_ANY_TRACK`].
    pub to: String,
    /// When the switch happens.
    #[serde(default)]
    pub sync: MusicSync,
    /// Equal-power crossfade length, seconds, starting at the switch. With a `segment` it is the
    /// fade from this track into the segment; the next track then starts at full level. If the
    /// segment is shorter than this fade, the outgoing track reaches silence when the segment
    /// ends (the segment's rise still uses this length, so a short segment may not reach full).
    #[serde(default)]
    pub crossfade_secs: f64,
    /// A sound to play between the two tracks.
    #[serde(default)]
    pub segment: Option<MusicSegmentDef>,
}

fn v1() -> u32 {
    1
}

/// A music track (RON form): BPM, meter, stems, stingers and transition rules.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MusicDef {
    /// Format version; absent in v1 files (read as 1). Newer than [`MUSIC_VERSION`] is a load
    /// error.
    #[serde(default = "v1")]
    pub version: u32,
    /// Track name.
    pub name: String,
    /// Tempo in beats per minute. The music clock ticks once per second; one beat is `60/bpm`
    /// seconds ([`BeatGrid`]).
    pub bpm: f64,
    /// Beats per bar (meter numerator; 4/4 → 4).
    #[serde(default = "four")]
    pub beats_per_bar: u32,
    /// The stems.
    pub stems: Vec<StemDef>,
    /// The track's segment length in bars, for [`MusicSync::EndOfSegment`]. `None`: the length of
    /// its longest stem (where the stems loop), rounded to whole beats.
    #[serde(default)]
    pub segment_bars: Option<u32>,
    /// One-shots quantized to this track's grid.
    #[serde(default)]
    pub stingers: Vec<MusicStingerDef>,
    /// How this track hands over to others. A track without a rule switches on the next bar with
    /// no crossfade.
    #[serde(default)]
    pub transitions: Vec<MusicTransitionDef>,
}

fn four() -> u32 {
    4
}

impl MusicDef {
    /// Parses a music track definition from RON text (`.music.ron`); sounds stay paths until
    /// [`MusicAsset::from_def`] resolves them.
    pub fn from_ron(text: &str) -> Result<Self, AudioError> {
        // Version first: a newer file may not parse as this engine's struct, and the useful
        // error is "upgrade the engine", not a RON field error.
        let version = Self::peek_version(text)?;
        if version > MUSIC_VERSION {
            return Err(AudioError::load(
                "music",
                format!(
                    "music format version {version} is newer than this engine ({MUSIC_VERSION}); upgrade the engine"
                ),
            ));
        }
        ron::from_str(text).map_err(|e| AudioError::load("music", e))
    }

    /// The version of music text without a full parse (1 when the file has no `version`).
    pub fn peek_version(text: &str) -> Result<u32, AudioError> {
        #[derive(Deserialize)]
        struct Peek {
            #[serde(default = "v1")]
            version: u32,
        }
        ron::from_str::<Peek>(text)
            .map(|p| p.version)
            .map_err(|e| AudioError::load("music", e))
    }

    /// Serializes the definition to pretty RON (`.music.ron`).
    pub fn to_ron(&self) -> Result<String, AudioError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| AudioError::load("music", e))
    }

    /// Migrates older music text to [`MUSIC_VERSION`]: every newer field is optional, so this is
    /// load, stamp the version, re-save.
    pub fn migrate_ron(text: &str) -> Result<String, AudioError> {
        let mut def = Self::from_ron(text)?;
        def.version = MUSIC_VERSION;
        def.to_ron()
    }
}

/// The serde field-list fingerprint of the `.music.ron` format (hashed by
/// `cargo xtask check-formats`): the root struct, every nested struct and the sync names.
pub fn music_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<MusicDef>(),
        regia_core::schema::serde_field_names::<StemDef>(),
        regia_core::schema::serde_field_names::<MusicStingerDef>(),
        regia_core::schema::serde_field_names::<MusicTransitionDef>(),
        regia_core::schema::serde_field_names::<MusicSegmentDef>(),
        regia_core::schema::serde_variant_names::<MusicSync>(),
    ])
}

/// The versions a built-in migration step leaves (`from` side): v1 → v2
/// ([`MusicDef::migrate_ron`]).
pub fn music_migration_sources() -> Vec<u32> {
    vec![1]
}

/// A resolved stem: its loaded sound and intensity curve.
#[derive(Clone)]
pub struct MusicStem {
    /// Stem name.
    pub name: String,
    /// The loaded sound.
    pub asset: SoundAsset,
    /// Base volume in dB.
    pub base_db: f32,
    /// Intensity → dB-offset polyline.
    pub intensity: Vec<(f32, f32)>,
}

impl MusicStem {
    /// The dB offset for an intensity value.
    pub fn offset_db_at(&self, intensity: f32) -> f32 {
        // NaN reads as the lowest intensity (`clamp` would pass it on and `polyline` would
        // answer with the loudest point).
        let intensity = if intensity.is_nan() {
            0.0
        } else {
            intensity.clamp(0.0, 1.0)
        };
        polyline(&self.intensity, intensity)
    }
}

/// A resolved stinger: its loaded sound and when it plays.
#[derive(Clone)]
pub struct MusicStinger {
    /// Stinger name.
    pub name: String,
    /// The loaded sound.
    pub asset: SoundAsset,
    /// Volume in dB.
    pub volume_db: f32,
    /// When it plays.
    pub sync: MusicSync,
}

/// A resolved transition segment.
#[derive(Clone)]
pub struct MusicSegment {
    /// The loaded sound.
    pub asset: SoundAsset,
    /// Volume in dB.
    pub volume_db: f32,
}

/// A resolved transition rule.
#[derive(Clone)]
pub struct MusicTransitionRule {
    /// The track this rule leads to, or [`MUSIC_ANY_TRACK`].
    pub to: String,
    /// When the switch happens.
    pub sync: MusicSync,
    /// Equal-power crossfade length, seconds.
    pub crossfade_secs: f64,
    /// A sound played between the two tracks.
    pub segment: Option<MusicSegment>,
}

/// A playable music track.
#[derive(Clone)]
pub struct MusicAsset {
    /// Track name.
    pub name: String,
    /// Tempo in beats per minute.
    pub bpm: f64,
    /// Beats per bar.
    pub beats_per_bar: u32,
    /// The stems.
    pub stems: Vec<MusicStem>,
    /// The segment length in bars; `None`: the longest stem.
    pub segment_bars: Option<u32>,
    /// One-shots quantized to this track's grid.
    pub stingers: Vec<MusicStinger>,
    /// How this track hands over to others.
    pub transitions: Vec<MusicTransitionRule>,
}

impl MusicAsset {
    /// Builds a track from its definition, resolving each sound path with `resolve`.
    pub fn from_def(
        def: &MusicDef,
        resolve: impl Fn(&str) -> Result<SoundAsset, AudioError>,
    ) -> Result<Self, AudioError> {
        if !(def.bpm > 0.0 && def.bpm <= MAX_BPM) {
            return Err(invalid_bpm(&def.name, def.bpm));
        }
        let mut stems = Vec::with_capacity(def.stems.len());
        for stem in &def.stems {
            stems.push(MusicStem {
                name: stem.name.clone(),
                asset: resolve(&stem.sound)?,
                base_db: stem.volume_db,
                intensity: stem.intensity.clone(),
            });
        }
        if stems.is_empty() {
            return Err(AudioError::Invalid(format!(
                "music track `{}` has no stems",
                def.name
            )));
        }
        if def.segment_bars == Some(0) {
            return Err(AudioError::Invalid(format!(
                "music track `{}` has a segment of 0 bars",
                def.name
            )));
        }
        let mut stingers: Vec<MusicStinger> = Vec::with_capacity(def.stingers.len());
        for stinger in &def.stingers {
            if stingers.iter().any(|known| known.name == stinger.name) {
                return Err(AudioError::Invalid(format!(
                    "music track `{}` names the stinger `{}` twice",
                    def.name, stinger.name
                )));
            }
            stingers.push(MusicStinger {
                name: stinger.name.clone(),
                asset: resolve(&stinger.sound)?,
                volume_db: stinger.volume_db,
                sync: stinger.sync,
            });
        }
        let mut transitions = Vec::with_capacity(def.transitions.len());
        for rule in &def.transitions {
            if !rule.crossfade_secs.is_finite() || rule.crossfade_secs < 0.0 {
                return Err(AudioError::Invalid(format!(
                    "music track `{}`: the transition to `{}` has a negative or non-finite crossfade",
                    def.name, rule.to
                )));
            }
            let segment = match &rule.segment {
                Some(segment) => Some(MusicSegment {
                    asset: resolve(&segment.sound)?,
                    volume_db: segment.volume_db,
                }),
                None => None,
            };
            transitions.push(MusicTransitionRule {
                to: rule.to.clone(),
                sync: rule.sync,
                crossfade_secs: rule.crossfade_secs,
                segment,
            });
        }
        Ok(Self {
            name: def.name.clone(),
            bpm: def.bpm,
            beats_per_bar: def.beats_per_bar.max(1),
            stems,
            segment_bars: def.segment_bars,
            stingers,
            transitions,
        })
    }

    /// A track from already-loaded stems, with no stingers or rules.
    pub fn from_stems(name: impl Into<String>, bpm: f64, beats_per_bar: u32, stems: Vec<MusicStem>) -> Self {
        Self {
            name: name.into(),
            bpm,
            beats_per_bar: beats_per_bar.max(1),
            stems,
            segment_bars: None,
            stingers: Vec::new(),
            transitions: Vec::new(),
        }
    }

    /// The segment length in beats: [`Self::segment_bars`], else the longest stem rounded to whole
    /// beats. `None` when neither says (a stem of unknown length).
    #[must_use]
    pub fn segment_beats(&self) -> Option<u64> {
        if let Some(bars) = self.segment_bars {
            return Some(u64::from(bars.max(1)) * u64::from(self.beats_per_bar.max(1)));
        }
        let longest = self
            .stems
            .iter()
            .map(|stem| stem.asset.duration_secs())
            .fold(0.0f64, f64::max);
        let beats = (longest * self.bpm / 60.0).round();
        (beats.is_finite() && beats >= 1.0).then_some(beats as u64)
    }

    /// The rule for a transition to `to`: its own, else the [`MUSIC_ANY_TRACK`] one, else the
    /// default (next bar, no crossfade, no segment).
    #[must_use]
    pub fn rule_to(&self, to: &str) -> MusicTransitionRule {
        self.transitions
            .iter()
            .find(|rule| rule.to == to)
            .or_else(|| self.transitions.iter().find(|rule| rule.to == MUSIC_ANY_TRACK))
            .cloned()
            .unwrap_or_else(|| MusicTransitionRule {
                to: to.to_string(),
                sync: MusicSync::NextBar,
                crossfade_secs: 0.0,
                segment: None,
            })
    }

    fn grid(&self, origin_secs: f64) -> BeatGrid {
        BeatGrid::new(origin_secs, self.bpm, self.beats_per_bar, self.segment_beats())
    }
}

/// Beats, bars and segments on the audio clock (pure; unit-tested without a mixer).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatGrid {
    /// The clock time of beat 0, seconds.
    pub origin_secs: f64,
    /// Seconds per beat.
    pub beat_secs: f64,
    /// Beats per bar.
    pub beats_per_bar: u32,
    /// Beats per segment; `None` falls back to bars.
    pub segment_beats: Option<u64>,
}

impl BeatGrid {
    /// A clock reading this close before a boundary (in beats) counts as on it: far under a sample
    /// at any tempo, and more than the clock's accumulated rounding.
    const ON_BOUNDARY_BEATS: f64 = 1e-6;

    /// A grid with beat 0 at `origin_secs`. A non-positive or non-finite `bpm` reads as 120.
    #[must_use]
    pub fn new(origin_secs: f64, bpm: f64, beats_per_bar: u32, segment_beats: Option<u64>) -> Self {
        let bpm = if bpm.is_finite() && bpm > 0.0 { bpm } else { 120.0 };
        Self {
            origin_secs,
            beat_secs: 60.0 / bpm,
            beats_per_bar: beats_per_bar.max(1),
            segment_beats: segment_beats.filter(|beats| *beats > 0),
        }
    }

    /// The clock time of beat `beat`, seconds.
    #[must_use]
    pub fn beat_time(&self, beat: u64) -> f64 {
        self.origin_secs + beat as f64 * self.beat_secs
    }

    /// The last beat at or before `secs`; `None` before beat 0.
    #[must_use]
    pub fn beat_at(&self, secs: f64) -> Option<u64> {
        let beats = (secs - self.origin_secs) / self.beat_secs + Self::ON_BOUNDARY_BEATS;
        (beats >= 0.0).then(|| beats.floor() as u64)
    }

    /// `(bar, beat within the bar)` of a beat index, both from 0.
    #[must_use]
    pub fn bar_and_beat(&self, beat: u64) -> (u64, u32) {
        let per_bar = u64::from(self.beats_per_bar);
        (beat / per_bar, (beat % per_bar) as u32)
    }

    /// The first time at or after `not_before` that `sync` allows, seconds.
    #[must_use]
    pub fn next(&self, sync: MusicSync, not_before: f64) -> f64 {
        let per_bar = u64::from(self.beats_per_bar);
        let unit = match sync {
            MusicSync::Immediate => return not_before,
            MusicSync::NextBeat => 1,
            MusicSync::NextBar => per_bar,
            MusicSync::EndOfSegment => self.segment_beats.unwrap_or(per_bar),
        };
        let unit_secs = unit as f64 * self.beat_secs;
        let units = ((not_before - self.origin_secs) / unit_secs - Self::ON_BOUNDARY_BEATS)
            .ceil()
            .max(0.0);
        self.origin_secs + units * unit_secs
    }
}

/// Sent to gameplay on every beat of the playing track (`PostUpdate`/`AudioSync`). It is read off
/// the audio clock, so it arrives up to a frame after the beat was heard; `clock_secs` is exact.
/// A frame that crosses several beats sends them all, in order.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct MusicBeat {
    /// The track the beat belongs to.
    pub track: String,
    /// Bar index since the track started, from 0.
    pub bar: u64,
    /// Beat within the bar, from 0 (0 = the downbeat).
    pub beat: u32,
    /// Beat index since the track started, from 0.
    pub beat_index: u64,
    /// The beat's time on the music clock, seconds.
    pub clock_secs: f64,
}

/// Overrides of a transition rule, for one request.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MusicTransitionRequest {
    /// Replaces the rule's `sync`.
    pub sync: Option<MusicSync>,
    /// Replaces the rule's `crossfade_secs`. Negative and non-finite values are ignored.
    pub crossfade_secs: Option<f64>,
}

/// When a requested stinger or transition will take effect.
#[derive(Clone, Debug, PartialEq)]
pub struct MusicSchedule {
    /// The stinger or track name.
    pub name: String,
    /// The sync that was applied.
    pub sync: MusicSync,
    /// When it takes effect, seconds on the music clock.
    pub at_secs: f64,
    /// The same moment as a sample index on the music clock (frame 0 is where the clock started,
    /// the first frame of the first track that started it).
    pub at_frame: u64,
    /// The bar it lands in, on the grid of the track playing now.
    pub bar: u64,
    /// The beat within that bar.
    pub beat: u32,
    /// True when nothing had to change (the track is already playing or already on its way).
    pub noop: bool,
}

/// A transition waiting for its switch, or in its segment.
#[derive(Clone, Debug, PartialEq)]
pub struct MusicPending {
    /// The track on its way.
    pub to: String,
    /// The sync that was applied.
    pub sync: MusicSync,
    /// When the current track starts fading (and the segment starts), clock seconds.
    pub switch_secs: f64,
    /// When the next track starts, clock seconds.
    pub start_secs: f64,
    /// The crossfade length, seconds.
    pub crossfade_secs: f64,
    /// True when a segment plays between the two tracks.
    pub segment: bool,
}

/// A snapshot of the music system (`Audio::music_state`, `audio.music_state`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MusicState {
    /// The playing track.
    pub track: Option<String>,
    /// Its tempo.
    pub bpm: Option<f64>,
    /// Its beats per bar.
    pub beats_per_bar: Option<u32>,
    /// Its segment length in beats.
    pub segment_beats: Option<u64>,
    /// The music clock, seconds. `None` when no clock runs.
    pub clock_secs: Option<f64>,
    /// The current beat index, from 0.
    pub beat_index: Option<u64>,
    /// The current bar, from 0.
    pub bar: Option<u64>,
    /// The current beat within the bar, from 0.
    pub beat: Option<u32>,
    /// The applied intensity.
    pub intensity: f32,
    /// Stem level targets `(name, dB)`.
    pub stems: Vec<(String, f32)>,
    /// The transition on its way.
    pub pending: Option<MusicPending>,
    /// The stingers not finished yet: `(name, start in clock seconds)`.
    pub stingers_playing: Vec<(String, f64)>,
    /// True while a crossfade tail runs.
    pub crossfading: bool,
    /// The playing track's stinger names.
    pub stingers: Vec<String>,
    /// The registered tracks.
    pub tracks: Vec<String>,
    /// How many scheduled sounds reached the mixer after their start time and started late. Not
    /// 0 means the schedule lead is too short for this output.
    pub late_starts: u64,
}

/// The schedule of one music voice, shared with the mixer. Times are clock seconds.
#[derive(Debug)]
pub(crate) struct VoiceSchedule {
    start_secs: f64,
    fade_in_secs: f64,
    /// `f64` bits. Infinity: no stop.
    stop_at: AtomicU64,
    /// `f64` bits, the fade before the stop is final.
    stop_fade: AtomicU64,
    /// The system's late-start counter.
    late: Arc<AtomicU64>,
}

impl VoiceSchedule {
    fn new(start_secs: f64, fade_in_secs: f64, late: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            start_secs,
            fade_in_secs,
            stop_at: AtomicU64::new(f64::INFINITY.to_bits()),
            stop_fade: AtomicU64::new(0f64.to_bits()),
            late,
        })
    }

    /// Fades the voice out from clock time `at` over `fade`, then ends it. A voice whose start is
    /// not before `at` never starts.
    fn stop_at(&self, at: f64, fade: f64) {
        self.stop_fade.store(fade.to_bits(), Ordering::Release);
        self.stop_at.store(at.to_bits(), Ordering::Release);
    }

    /// Takes back a stop that has not happened yet.
    fn clear_stop(&self) {
        self.stop_at.store(f64::INFINITY.to_bits(), Ordering::Release);
    }
}

/// What ties a sound to the music clock.
#[derive(Clone)]
pub(crate) struct VoiceLink {
    pub(crate) clock: ClockId,
    pub(crate) schedule: Arc<VoiceSchedule>,
}

/// Wraps a sound's data into a [`MusicVoice`]; the handle stays the inner sound's.
pub(crate) struct MusicVoiceData<D> {
    pub(crate) inner: D,
    pub(crate) link: VoiceLink,
}

impl<D: SoundData> SoundData for MusicVoiceData<D> {
    type Error = D::Error;
    type Handle = D::Handle;

    fn into_sound(self) -> Result<(Box<dyn Sound>, Self::Handle), Self::Error> {
        let (inner, handle) = self.inner.into_sound()?;
        Ok((
            Box::new(MusicVoice {
                inner,
                link: self.link,
                started_at: None,
                next_frame: 0,
                finished: false,
            }),
            handle,
        ))
    }
}

/// A sound on the music clock: silent until its start frame, then the inner sound under the
/// schedule's gain envelope. It runs in the mixer, so it allocates nothing and takes no lock.
struct MusicVoice {
    inner: Box<dyn Sound>,
    link: VoiceLink,
    /// The clock frame of the voice's first sample, once it started.
    started_at: Option<i64>,
    /// The clock frame of the next block's first sample, once it started.
    next_frame: i64,
    finished: bool,
}

/// Quarter-sine equal-power fade: 0 at `t = 0`, 1 at `t = 1`.
#[inline]
fn equal_power(t: f32) -> f32 {
    (t.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2).sin()
}

impl MusicVoice {
    /// The clock frame of `out[0]` before the voice started (kira advances its clocks before the
    /// mixer, so the clock reads the end of this block). `None` while the clock does not run.
    fn block_start(&mut self, len: usize, dt: f64, info: &Info) -> Option<i64> {
        let Some(clock) = info.clock_info(self.link.clock) else {
            // The clock is gone and with it the time this voice was waiting for.
            self.finished = true;
            return None;
        };
        if !clock.ticking {
            return None;
        }
        let end = (clock.time.ticks as f64 + clock.time.fraction) / dt;
        Some(end.round() as i64 - len as i64)
    }

    /// Applies the fade-in and the stop to the block from `first` (`out[0]`'s clock frame).
    fn apply_envelope(&mut self, out: &mut [Frame], first: i64, start: i64, dt: f64) {
        let schedule = &self.link.schedule;
        let stop_secs = f64::from_bits(schedule.stop_at.load(Ordering::Acquire));
        let fade_in = (schedule.fade_in_secs / dt).round() as i64;
        let in_fade = fade_in > 0 && first < start + fade_in;
        if !in_fade && stop_secs == f64::INFINITY {
            return;
        }
        let (stop, fade_out) = if stop_secs == f64::INFINITY {
            (i64::MAX, 0)
        } else {
            let fade = f64::from_bits(schedule.stop_fade.load(Ordering::Acquire));
            ((stop_secs / dt).round() as i64, (fade / dt).round() as i64)
        };
        for (frame, at) in out.iter_mut().zip(first..) {
            let mut gain = 1.0f32;
            if at < start + fade_in {
                gain *= equal_power((at - start) as f32 / fade_in as f32);
            }
            if at >= stop {
                let done = at.saturating_sub(stop);
                if done >= fade_out {
                    gain = 0.0;
                    self.finished = true;
                } else {
                    gain *= equal_power(1.0 - done as f32 / fade_out as f32);
                }
            }
            *frame *= gain;
        }
    }
}

impl Sound for MusicVoice {
    fn on_start_processing(&mut self) {
        self.inner.on_start_processing();
    }

    fn process(&mut self, out: &mut [Frame], dt: f64, info: &Info) {
        let (first, start) = match self.started_at {
            Some(start) => (self.next_frame, start),
            None => {
                let start_secs = self.link.schedule.start_secs;
                let stop_secs = f64::from_bits(self.link.schedule.stop_at.load(Ordering::Acquire));
                if stop_secs <= start_secs {
                    // Stopped before it began: nothing was or will be heard.
                    self.finished = true;
                    out.fill(Frame::ZERO);
                    return;
                }
                let Some(first) = self.block_start(out.len(), dt, info) else {
                    out.fill(Frame::ZERO);
                    return;
                };
                let start = (start_secs / dt).round() as i64;
                if start >= first + out.len() as i64 {
                    out.fill(Frame::ZERO);
                    return;
                }
                if start < first {
                    self.link.schedule.late.fetch_add(1, Ordering::Relaxed);
                }
                let start = start.max(first);
                self.started_at = Some(start);
                (first, start)
            }
        };
        self.next_frame = first + out.len() as i64;
        let lead = (start - first).max(0) as usize;
        out[..lead].fill(Frame::ZERO);
        let tail = &mut out[lead..];
        self.inner.process(tail, dt, info);
        self.apply_envelope(tail, first + lead as i64, start, dt);
    }

    fn finished(&self) -> bool {
        self.finished || self.inner.finished()
    }
}

/// The fastest tempo a track may play at, beats per minute.
const MAX_BPM: f64 = 1_000.0;

/// The load and play rejection for a tempo the music clock cannot run.
fn invalid_bpm(name: &str, bpm: f64) -> AudioError {
    AudioError::Invalid(format!(
        "music track `{name}` has bpm {bpm} (must be above 0 and at most {MAX_BPM})"
    ))
}

/// How long the outgoing track fades. `segment_secs` is `Some` when a transition segment plays.
/// A segment shorter than the crossfade ends the fade when the segment ends. A hard cut (no
/// segment, or a zero crossfade) still lasts at least the declick time (ADR-544), unless that
/// would keep the old track alive past a short segment.
fn outgoing_fade_secs(crossfade_secs: f64, segment_secs: Option<f64>) -> f64 {
    let declick = DECLICK_FADE.as_secs_f64();
    match segment_secs {
        Some(segment) if segment > 0.0 => {
            let into = crossfade_secs.min(segment);
            if segment < declick {
                into
            } else {
                into.max(declick)
            }
        }
        _ => crossfade_secs.max(declick),
    }
}

/// Rejects a tempo the music clock cannot run: NaN or non-positive never ticks, and an infinite or
/// huge one makes the mixer's clock count ticks one by one forever (a hung render).
/// Regression: tests/tqs_sweep.rs::music_with_an_unplayable_bpm_is_rejected_regression
pub(crate) fn check_bpm(asset: &MusicAsset) -> Result<(), AudioError> {
    if asset.bpm > 0.0 && asset.bpm <= MAX_BPM {
        Ok(())
    } else {
        Err(invalid_bpm(&asset.name, asset.bpm))
    }
}

/// Pure bar-boundary decision logic (unit-tested without a clock).
#[derive(Debug, Default)]
pub struct BarGate {
    last_seen_tick: u64,
}

impl BarGate {
    /// Feed the music clock's current tick count; returns `true` exactly once per bar boundary
    /// crossing (tick multiples of `beats_per_bar`).
    pub fn crossed_boundary(&mut self, tick: u64, beats_per_bar: u32) -> bool {
        let beats = beats_per_bar.max(1) as u64;
        // A boundary lies in (last_seen, tick] exactly when the bar index grew; no walk over the
        // ticks in between (a long frame, or a clock that reads far ahead, costs the same).
        let crossed = tick > self.last_seen_tick && tick / beats > self.last_seen_tick / beats;
        self.last_seen_tick = self.last_seen_tick.max(tick);
        crossed
    }
}

/// One live stem voice.
pub struct StemVoice {
    /// Stem name.
    pub name: String,
    /// The playing instance.
    pub instance: SoundInstance,
    /// The volume target currently applied (dB).
    pub target_db: f32,
    /// The stem's base volume (dB).
    pub base_db: f32,
    /// The stem's intensity → dB-offset polyline.
    pub intensity_curve: Vec<(f32, f32)>,
    /// Its link to the clock; `None` when no clock could be had.
    schedule: Option<Arc<VoiceSchedule>>,
}

/// A track currently playing.
pub struct PlayingMusic {
    /// Track name.
    pub name: String,
    /// Beats per bar.
    pub beats_per_bar: u32,
    /// Stem voices.
    pub stems: Vec<StemVoice>,
    bpm: f64,
    /// The asset, for its stingers and rules.
    asset: MusicAsset,
}

/// A track fading out (crossfade tail).
struct FadingMusic {
    stems: Vec<StemVoice>,
    remaining_secs: f64,
    /// A clock this tail was started on, when a new one replaced it: its voices run on it.
    _clock: Option<ClockHandle>,
}

/// A transition on its way: the next track's stems are in the mixer, silent until their start.
struct PendingTransition {
    info: MusicPending,
    asset: MusicAsset,
    stems: Vec<StemVoice>,
    segment: Option<(SoundInstance, Arc<VoiceSchedule>)>,
    /// A request that arrived after the switch: applied once the next track is playing.
    queued: Option<(String, MusicTransitionRequest)>,
}

/// A stinger on its way or playing.
struct ScheduledStinger {
    name: String,
    at_secs: f64,
    end_secs: f64,
    _instance: SoundInstance,
}

/// Starts one sound on the Music bus. `Audio` passes this in: the buses are its own.
pub(crate) type StartVoice<'a> =
    dyn FnMut(&SoundAsset, &PlaybackParams, Option<VoiceLink>) -> Result<SoundInstance, AudioError> + 'a;

/// The music system: current track, crossfade tails, intensity and bar gating.
pub struct MusicSystem {
    current: Option<PlayingMusic>,
    fading: Vec<FadingMusic>,
    clock: Option<ClockHandle>,
    grid: Option<BeatGrid>,
    /// The next beat to announce.
    next_beat: u64,
    beats: Vec<MusicBeat>,
    intensity: f32,
    pending_intensity: Option<f32>,
    gate: BarGate,
    bpm_hint: f64,
    tracks: BTreeMap<String, MusicAsset>,
    pending: Option<PendingTransition>,
    stingers: Vec<ScheduledStinger>,
    queued_request: Option<(String, MusicTransitionRequest)>,
    late: Arc<AtomicU64>,
}

/// When and how a track's stems start.
struct StemPlan {
    /// The clock they run on; `None`: untimed.
    clock: Option<ClockId>,
    /// Their start, clock seconds.
    start_secs: f64,
    /// Their equal-power fade-in, seconds.
    fade_in_secs: f64,
    /// A fade-in by the mixer instead, for stems with no clock.
    kira_fade: Option<Duration>,
}

/// The parameters of a stem at `target_db`.
fn stem_params(target_db: f32, fade_in: Option<Duration>) -> PlaybackParams {
    PlaybackParams {
        bus: Some("Music".into()),
        volume_db: target_db,
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        // No fade is not a zero-length fade: the mixer ramps one of those over its first block.
        fade_in: fade_in.filter(|fade| !fade.is_zero()),
        ..PlaybackParams::default()
    }
}

/// The frame nearest a clock time.
fn frame_of(secs: f64, sample_rate: u32) -> u64 {
    (secs.max(0.0) * f64::from(sample_rate)).round() as u64
}

impl MusicSystem {
    /// An idle music system (no track).
    pub fn new() -> Self {
        Self {
            current: None,
            fading: Vec::new(),
            clock: None,
            grid: None,
            next_beat: 0,
            beats: Vec::new(),
            intensity: 0.0,
            pending_intensity: None,
            gate: BarGate::default(),
            bpm_hint: 120.0,
            tracks: BTreeMap::new(),
            pending: None,
            stingers: Vec::new(),
            queued_request: None,
            late: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Registers a track by name, for transitions; a track of the same name is replaced.
    pub fn register_track(&mut self, asset: MusicAsset) {
        self.tracks.insert(asset.name.clone(), asset);
    }

    /// The registered track `name`.
    #[must_use]
    pub fn track(&self, name: &str) -> Option<&MusicAsset> {
        self.tracks.get(name)
    }

    /// Starts every stem of `asset` as `plan` says. A failure stops what it started and leaves
    /// everything else as it was.
    fn start_stems(
        &self,
        asset: &MusicAsset,
        intensity: f32,
        plan: StemPlan,
        start_voice: &mut StartVoice<'_>,
    ) -> Result<Vec<StemVoice>, AudioError> {
        let mut stems: Vec<StemVoice> = Vec::with_capacity(asset.stems.len());
        for stem in &asset.stems {
            let target_db = stem.base_db + stem.offset_db_at(intensity);
            let link = plan.clock.map(|clock| VoiceLink {
                clock,
                schedule: VoiceSchedule::new(plan.start_secs, plan.fade_in_secs, Arc::clone(&self.late)),
            });
            let schedule = link.as_ref().map(|link| Arc::clone(&link.schedule));
            match start_voice(&stem.asset, &stem_params(target_db, plan.kira_fade), link) {
                Ok(instance) => stems.push(StemVoice {
                    name: stem.name.clone(),
                    instance,
                    target_db,
                    base_db: stem.base_db,
                    intensity_curve: stem.intensity.clone(),
                    schedule,
                }),
                Err(err) => {
                    for started in &mut stems {
                        Self::end_voice(started);
                    }
                    return Err(err);
                }
            }
        }
        Ok(stems)
    }

    /// Ends a voice that may not have started.
    fn end_voice(stem: &mut StemVoice) {
        if let Some(schedule) = &stem.schedule {
            schedule.stop_at(f64::NEG_INFINITY, 0.0);
        }
        stem.instance.stop(Duration::ZERO);
    }

    /// Starts (or crossfades to) a track at once, on a new clock. `clock` is that new clock:
    /// `None` (a suspended output, no clock slot) plays the stems untimed.
    /// [`AudioError::Invalid`] when the track's bpm is not above 0 and at most 1000.
    pub(crate) fn play(
        &mut self,
        asset: &MusicAsset,
        fade: Duration,
        mut clock: Option<ClockHandle>,
        start_voice: &mut StartVoice<'_>,
    ) -> Result<(), AudioError> {
        check_bpm(asset)?;
        let intensity = self.pending_intensity.unwrap_or(self.intensity);
        let clock_id = clock.as_ref().map(ClockHandle::id);

        // Start every stem before touching the current track, so a failure leaves the previous
        // piece playing and no looping stem without a handle.
        // On a clock the crossfade is the voices' own equal-power one. Without one, the mixer's fade.
        let plan = StemPlan {
            clock: clock_id,
            start_secs: 0.0,
            fade_in_secs: fade.as_secs_f64(),
            kira_fade: clock_id.is_none().then_some(fade),
        };
        let stems = self.start_stems(asset, intensity, plan, start_voice)?;

        // A transition on its way is dropped: this call replaces whatever it would have played.
        self.abort_pending(fade);

        // Committed: move the current track to the fading list. Its voices keep running on the
        // old clock until the tail ends.
        let old_now = self.position_secs();
        let old_clock = self.clock.take();
        if let Some(current) = self.current.take() {
            let mut fading_music = FadingMusic {
                remaining_secs: fade.as_secs_f64(),
                stems: current.stems,
                _clock: old_clock,
            };
            // Equal power with the new stems' fade-in (`Audio::play_music`): the level holds.
            for stem in &mut fading_music.stems {
                match (&stem.schedule, old_now) {
                    (Some(schedule), Some(now)) => {
                        schedule.stop_at(now, fade.max(DECLICK_FADE).as_secs_f64());
                    }
                    _ => stem.instance.stop_crossfading(fade),
                }
            }
            self.fading.push(fading_music);
        }
        if let Some(clock) = clock.as_mut() {
            // After the stems, so every stem is in the mixer when the clock starts: they begin
            // on the same sample.
            clock.start();
        }
        self.clock = clock;
        self.grid = self.clock.is_some().then(|| asset.grid(0.0));
        self.next_beat = 0;
        self.gate = BarGate::default();
        self.bpm_hint = asset.bpm;
        self.pending_intensity = None;
        self.intensity = intensity;
        self.current = Some(PlayingMusic {
            name: asset.name.clone(),
            beats_per_bar: asset.beats_per_bar,
            stems,
            bpm: asset.bpm,
            asset: asset.clone(),
        });
        Ok(())
    }

    /// True when a transition can be scheduled on the running clock.
    pub(crate) fn can_schedule(&self) -> bool {
        self.clock.is_some() && self.grid.is_some() && self.current.is_some()
    }

    /// A schedule report for `at_secs` on the current grid.
    fn report(
        &self,
        name: &str,
        sync: MusicSync,
        at_secs: f64,
        noop: bool,
        sample_rate: u32,
    ) -> MusicSchedule {
        let (bar, beat) = self
            .grid
            .and_then(|grid| grid.beat_at(at_secs).map(|index| grid.bar_and_beat(index)))
            .unwrap_or((0, 0));
        MusicSchedule {
            name: name.to_string(),
            sync,
            at_secs,
            at_frame: frame_of(at_secs, sample_rate),
            bar,
            beat,
            noop,
        }
    }

    /// Requests a transition to the registered track `to`, with `request` over the current
    /// track's rule. The stems go into the mixer now and start on their exact sample; the current
    /// track stops on its own. Asking again for what is already playing or on its way changes
    /// nothing, so a game can state the music it wants every frame.
    pub(crate) fn transition(
        &mut self,
        to: &str,
        request: MusicTransitionRequest,
        sample_rate: u32,
        start_voice: &mut StartVoice<'_>,
    ) -> Result<MusicSchedule, AudioError> {
        let asset = self
            .tracks
            .get(to)
            .cloned()
            .ok_or_else(|| AudioError::UnknownEntry(to.to_string()))?;
        check_bpm(&asset)?;
        let (Some(now), Some(grid), Some(clock), Some(current)) = (
            self.position_secs(),
            self.grid,
            self.clock.as_ref().map(ClockHandle::id),
            self.current.as_ref(),
        ) else {
            return Err(AudioError::Invalid(
                "no music clock is running: play a track first".into(),
            ));
        };
        let rule = current.asset.rule_to(to);
        let earliest = now + MUSIC_SCHEDULE_LEAD_SECS;
        if let Some(pending) = &mut self.pending {
            if pending.info.to == to {
                let (sync, at) = (pending.info.sync, pending.info.switch_secs);
                pending.queued = None;
                return Ok(self.report(to, sync, at, true, sample_rate));
            }
            if pending.info.switch_secs < earliest {
                // Too late to take back: this request waits for the next track.
                pending.queued = Some((to.to_string(), request));
                let at = pending.info.start_secs;
                let sync = request.sync.unwrap_or(rule.sync);
                return Ok(self.report(to, sync, at, false, sample_rate));
            }
        }
        if current.name == to {
            self.cancel_pending();
            return Ok(self.report(to, rule.sync, now, true, sample_rate));
        }

        let sync = request.sync.unwrap_or(rule.sync);
        let crossfade_secs = request
            .crossfade_secs
            .filter(|secs| secs.is_finite() && *secs >= 0.0)
            .unwrap_or(rule.crossfade_secs);
        let switch_secs = grid.next(sync, earliest);
        let segment_secs = rule.segment.as_ref().map_or(0.0, |s| s.asset.duration_secs());
        let start_secs = switch_secs + segment_secs;
        let intensity = self.pending_intensity.unwrap_or(self.intensity);
        // A segment leads into the next track, which starts at full level. The crossfade
        // is from this track into the segment (equal-power), not into the next track.
        let fade_in_secs = if rule.segment.is_some() {
            0.0
        } else {
            crossfade_secs
        };
        let plan = StemPlan {
            clock: Some(clock),
            start_secs,
            fade_in_secs,
            kira_fade: None,
        };
        let mut stems = self.start_stems(&asset, intensity, plan, start_voice)?;
        let segment = match &rule.segment {
            None => None,
            Some(segment) => {
                let schedule = VoiceSchedule::new(switch_secs, crossfade_secs, Arc::clone(&self.late));
                let link = VoiceLink {
                    clock,
                    schedule: Arc::clone(&schedule),
                };
                let params = PlaybackParams {
                    bus: Some("Music".into()),
                    volume_db: segment.volume_db,
                    ..PlaybackParams::default()
                };
                match start_voice(&segment.asset, &params, Some(link)) {
                    Ok(instance) => Some((instance, schedule)),
                    Err(err) => {
                        for stem in &mut stems {
                            Self::end_voice(stem);
                        }
                        return Err(err);
                    }
                }
            }
        };
        // Committed. A pending transition not yet at its switch is replaced.
        self.cancel_pending();
        // A hard cut still fades over the declick time, like every stop of a sound that may have
        // been heard (ADR-544). With a segment shorter than the crossfade, the outgoing fade
        // ends when the segment ends, so the next track (full level) does not overlap a tail.
        let stop_fade = outgoing_fade_secs(crossfade_secs, rule.segment.is_some().then_some(segment_secs));
        if let Some(current) = self.current.as_ref() {
            for stem in &current.stems {
                if let Some(schedule) = &stem.schedule {
                    schedule.stop_at(switch_secs, stop_fade);
                }
            }
        }
        self.pending = Some(PendingTransition {
            info: MusicPending {
                to: to.to_string(),
                sync,
                switch_secs,
                start_secs,
                crossfade_secs,
                segment: segment.is_some(),
            },
            asset,
            stems,
            segment,
            queued: None,
        });
        Ok(self.report(to, sync, switch_secs, false, sample_rate))
    }

    /// Takes back a transition that has not reached its switch: the next track's voices end
    /// unheard and the current track's stop is withdrawn.
    fn cancel_pending(&mut self) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        for stem in &mut pending.stems {
            Self::end_voice(stem);
        }
        if let Some((mut instance, schedule)) = pending.segment.take() {
            schedule.stop_at(f64::NEG_INFINITY, 0.0);
            instance.stop(Duration::ZERO);
        }
        if let Some(current) = self.current.as_ref() {
            for stem in &current.stems {
                if let Some(schedule) = &stem.schedule {
                    schedule.clear_stop();
                }
            }
        }
    }

    /// Ends a transition whatever its stage, fading what is already heard over `fade`.
    fn abort_pending(&mut self, fade: Duration) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let now = self.position_secs().unwrap_or(f64::NEG_INFINITY);
        let fade = fade.max(DECLICK_FADE).as_secs_f64();
        let stems = pending.stems.iter().filter_map(|stem| stem.schedule.as_ref());
        let segment = pending.segment.iter().map(|(_, schedule)| schedule);
        for schedule in stems.chain(segment) {
            schedule.stop_at(now, fade);
        }
    }

    /// Requests the playing track's stinger `name` on its `sync`, or on `sync` when given.
    pub(crate) fn stinger(
        &mut self,
        name: &str,
        sync: Option<MusicSync>,
        sample_rate: u32,
        start_voice: &mut StartVoice<'_>,
    ) -> Result<MusicSchedule, AudioError> {
        let (Some(now), Some(grid), Some(clock), Some(current)) = (
            self.position_secs(),
            self.grid,
            self.clock.as_ref().map(ClockHandle::id),
            self.current.as_ref(),
        ) else {
            return Err(AudioError::Invalid(
                "no music clock is running: play a track first".into(),
            ));
        };
        let stinger = current
            .asset
            .stingers
            .iter()
            .find(|stinger| stinger.name == name)
            .cloned()
            .ok_or_else(|| AudioError::UnknownEntry(name.to_string()))?;
        let sync = sync.unwrap_or(stinger.sync);
        let at_secs = grid.next(sync, now + MUSIC_SCHEDULE_LEAD_SECS);
        let link = VoiceLink {
            clock,
            schedule: VoiceSchedule::new(at_secs, 0.0, Arc::clone(&self.late)),
        };
        let params = PlaybackParams {
            bus: Some("Music".into()),
            volume_db: stinger.volume_db,
            ..PlaybackParams::default()
        };
        let instance = start_voice(&stinger.asset, &params, Some(link))?;
        self.stingers.push(ScheduledStinger {
            name: name.to_string(),
            at_secs,
            end_secs: at_secs + stinger.asset.duration_secs(),
            _instance: instance,
        });
        Ok(self.report(name, sync, at_secs, false, sample_rate))
    }

    /// Requests a new intensity; applied at the next bar boundary.
    pub fn set_intensity(&mut self, intensity: f32) {
        if intensity.is_nan() {
            return;
        }
        self.pending_intensity = Some(intensity.clamp(0.0, 1.0));
    }

    /// The currently applied intensity.
    pub fn intensity(&self) -> f32 {
        self.intensity
    }

    /// Announces the beats up to `until` (exclusive when `exclusive`) on the current grid.
    fn announce_beats(&mut self, until: f64, exclusive: bool) {
        let (Some(grid), Some(current)) = (self.grid, self.current.as_ref()) else {
            return;
        };
        let Some(last) = grid.beat_at(until) else {
            return;
        };
        while self.next_beat <= last {
            let clock_secs = grid.beat_time(self.next_beat);
            if exclusive && clock_secs >= until {
                break;
            }
            let (bar, beat) = grid.bar_and_beat(self.next_beat);
            self.beats.push(MusicBeat {
                track: current.name.clone(),
                bar,
                beat,
                beat_index: self.next_beat,
                clock_secs,
            });
            self.next_beat += 1;
        }
    }

    /// Makes the next track current once the clock reached its start.
    fn finish_transition(&mut self, now: f64) {
        let Some(pending) = self.pending.take_if(|pending| now >= pending.info.start_secs) else {
            return;
        };
        let start = pending.info.start_secs;
        self.announce_beats(start, true);
        if let Some(current) = self.current.take() {
            // The stems end themselves on their schedule; these handles only keep them
            // reachable until then.
            let left = pending.info.switch_secs + pending.info.crossfade_secs - now;
            self.fading.push(FadingMusic {
                stems: current.stems,
                remaining_secs: left.max(0.0),
                _clock: None,
            });
        }
        self.grid = Some(pending.asset.grid(start));
        self.next_beat = 0;
        self.gate = BarGate::default();
        self.bpm_hint = pending.asset.bpm;
        self.current = Some(PlayingMusic {
            name: pending.asset.name.clone(),
            beats_per_bar: pending.asset.beats_per_bar,
            stems: pending.stems,
            bpm: pending.asset.bpm,
            asset: pending.asset,
        });
        self.queued_request = pending.queued;
    }

    /// Advances crossfade tails, moves a finished transition's track in, announces beats and
    /// applies pending intensity changes on bar boundaries. Reads the music clock.
    pub(crate) fn tick(&mut self, dt: f64) {
        for fading in &mut self.fading {
            fading.remaining_secs -= dt;
        }
        self.fading.retain(|f| f.remaining_secs > 0.0);

        let Some(now) = self.position_secs() else {
            return;
        };
        self.stingers.retain(|stinger| stinger.end_secs > now);
        // Time 0 is a clock the mixer has not run yet: nothing has been heard.
        if now <= 0.0 {
            return;
        }
        self.finish_transition(now);
        self.announce_beats(now, false);
        let Some(tick) = self.clock_tick() else {
            return;
        };
        let Some(current) = self.current.as_mut() else {
            return;
        };
        if !self.gate.crossed_boundary(tick, current.beats_per_bar) {
            return;
        }
        if let Some(intensity) = self.pending_intensity.take() {
            self.intensity = intensity;
            // Levels glide in over half a bar so the boundary switch is not a jump.
            let beat_secs = 60.0 / self.bpm_hint.max(1.0);
            let fade = Duration::from_secs_f64((beat_secs * 0.5).max(0.05));
            for stem in &mut current.stems {
                stem.target_db = stem.base_db + polyline(&stem.intensity_curve, intensity);
                stem.instance.set_volume_db(stem.target_db, fade);
            }
        }
    }

    /// The beats announced since the last call, oldest first.
    pub(crate) fn take_beats(&mut self) -> Vec<MusicBeat> {
        std::mem::take(&mut self.beats)
    }

    /// A transition requested after the last one's switch, once its track is playing.
    pub(crate) fn take_queued_request(&mut self) -> Option<(String, MusicTransitionRequest)> {
        self.queued_request.take()
    }

    /// The voices are gone with the output (a suspend, a mixdown swap): nothing on its way can
    /// happen, and the clock went with the mixer. The current track's name stays (spec § 4).
    pub(crate) fn output_lost(&mut self) {
        self.pending = None;
        self.queued_request = None;
        self.stingers.clear();
        self.fading.clear();
        self.clock = None;
        self.grid = None;
    }

    /// The music clock in seconds. `None` when no clock runs.
    fn position_secs(&self) -> Option<f64> {
        self.clock.as_ref().map(|clock| {
            let time = clock.time();
            time.ticks as f64 + time.fraction
        })
    }

    /// The current beat index on the playing track's grid (beat 0 at the grid origin).
    ///
    /// This is not the kira clock tick. That clock ticks once per second; the beat index is
    /// `clock seconds / (60/bpm)`.
    pub fn clock_tick(&self) -> Option<u64> {
        self.grid?.beat_at(self.position_secs()?)
    }

    /// Current stem level targets `(name, dB)` of the playing track.
    pub fn stem_levels(&self) -> Vec<(String, f32)> {
        self.current
            .as_ref()
            .map(|m| m.stems.iter().map(|s| (s.name.clone(), s.target_db)).collect())
            .unwrap_or_default()
    }

    /// The name of the playing track.
    pub fn current_track(&self) -> Option<&str> {
        self.current.as_ref().map(|m| m.name.as_str())
    }

    /// True while a crossfade tail is still running.
    pub fn is_crossfading(&self) -> bool {
        !self.fading.is_empty()
    }

    /// A snapshot for tools.
    #[must_use]
    pub fn state(&self) -> MusicState {
        let clock_secs = self.position_secs();
        let beat_index = self.clock_tick();
        let bar_beat = self
            .grid
            .zip(beat_index)
            .map(|(grid, index)| grid.bar_and_beat(index));
        let current = self.current.as_ref();
        MusicState {
            track: current.map(|m| m.name.clone()),
            bpm: current.map(|m| m.bpm),
            beats_per_bar: current.map(|m| m.beats_per_bar),
            segment_beats: current.and_then(|m| m.asset.segment_beats()),
            clock_secs,
            beat_index,
            bar: bar_beat.map(|(bar, _)| bar),
            beat: bar_beat.map(|(_, beat)| beat),
            intensity: self.intensity,
            stems: self.stem_levels(),
            pending: self.pending.as_ref().map(|pending| pending.info.clone()),
            stingers_playing: self
                .stingers
                .iter()
                .map(|stinger| (stinger.name.clone(), stinger.at_secs))
                .collect(),
            crossfading: self.is_crossfading(),
            stingers: current
                .map(|m| m.asset.stingers.iter().map(|s| s.name.clone()).collect())
                .unwrap_or_default(),
            tracks: self.tracks.keys().cloned().collect(),
            late_starts: self.late.load(Ordering::Relaxed),
        }
    }

    /// The playing track's stems as `(track/stem, instance)` (the stream watch; crossfade tails
    /// are stopping and not listed).
    pub(crate) fn current_stems(&self) -> impl Iterator<Item = (String, &SoundInstance)> {
        self.current.iter().flat_map(|track| {
            track
                .stems
                .iter()
                .map(move |stem| (format!("{}/{}", track.name, stem.name), &stem.instance))
        })
    }
}

impl Default for MusicSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AudioError;

    /// Moved from tests/tqs_sweep.rs when `tick` became crate-private (ADR-2517): a NaN `dt` on an
    /// idle system changes nothing, and a pending intensity waits for a track.
    #[test]
    fn an_idle_tick_with_a_nan_dt_changes_nothing() {
        let mut music = MusicSystem::new();
        music.set_intensity(0.7);
        music.tick(f64::NAN);
        assert_eq!(music.intensity(), 0.0);
        assert!(!music.is_crossfading());
        assert_eq!(music.current_track(), None);
    }

    fn one_stem_def(name: &str) -> MusicDef {
        MusicDef {
            version: MUSIC_VERSION,
            name: name.into(),
            bpm: 120.0,
            beats_per_bar: 4,
            stems: vec![StemDef {
                name: "a".into(),
                sound: "a.wav".into(),
                volume_db: 0.0,
                intensity: Vec::new(),
            }],
            segment_bars: None,
            stingers: Vec::new(),
            transitions: Vec::new(),
        }
    }

    #[test]
    fn the_grid_finds_beats_bars_and_segments() {
        let grid = BeatGrid::new(1.0, 120.0, 4, Some(16));
        assert_eq!(grid.beat_at(0.5), None, "before beat 0");
        assert_eq!(grid.beat_at(1.0), Some(0));
        assert_eq!(grid.beat_at(1.49), Some(0));
        assert_eq!(grid.beat_at(1.5), Some(1));
        assert_eq!(grid.bar_and_beat(9), (2, 1));
        assert_eq!(grid.next(MusicSync::Immediate, 1.7), 1.7);
        assert_eq!(grid.next(MusicSync::NextBeat, 1.7), 2.0);
        assert_eq!(grid.next(MusicSync::NextBar, 1.7), 3.0);
        assert_eq!(grid.next(MusicSync::EndOfSegment, 1.7), 9.0);
        // Exactly on a boundary, that boundary is still the next one.
        assert_eq!(grid.next(MusicSync::NextBeat, 2.0), 2.0);
        assert_eq!(grid.next(MusicSync::NextBar, 3.0), 3.0);
        // Before the grid starts: its first boundary.
        assert_eq!(grid.next(MusicSync::NextBar, 0.0), 1.0);
    }

    #[test]
    fn the_grid_stays_exact_far_into_a_track() {
        // 132 bpm: no whole number of samples per beat. A beat index an hour in reads back.
        let grid = BeatGrid::new(0.0, 132.0, 4, None);
        for beat in [1u64, 7, 1_000, 7_920, 100_003] {
            let time = grid.beat_time(beat);
            assert_eq!(grid.beat_at(time), Some(beat), "beat {beat}");
            assert_eq!(grid.next(MusicSync::NextBeat, time), time, "beat {beat}");
        }
    }

    #[test]
    fn a_grid_without_a_segment_uses_bars() {
        let grid = BeatGrid::new(0.0, 60.0, 3, None);
        assert_eq!(grid.next(MusicSync::EndOfSegment, 0.5), 3.0);
    }

    #[test]
    fn a_bad_tempo_or_meter_cannot_break_the_grid() {
        for bpm in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let grid = BeatGrid::new(0.0, bpm, 0, Some(0));
            assert!(grid.beat_secs.is_finite() && grid.beat_secs > 0.0, "{bpm}");
            assert_eq!(grid.beats_per_bar, 1);
            assert_eq!(grid.segment_beats, None);
        }
    }

    #[test]
    fn the_rule_lookup_prefers_the_named_track() {
        let any = MusicTransitionDef {
            to: MUSIC_ANY_TRACK.into(),
            sync: MusicSync::NextBeat,
            crossfade_secs: 1.0,
            segment: None,
        };
        let named = MusicTransitionDef {
            to: "calm".into(),
            sync: MusicSync::EndOfSegment,
            crossfade_secs: 2.0,
            segment: None,
        };
        let def = MusicDef {
            transitions: vec![any, named],
            ..one_stem_def("t")
        };
        let asset = MusicAsset::from_def(&def, |_| Ok(crate::test_support::asset())).expect("valid");
        assert_eq!(asset.rule_to("calm").sync, MusicSync::EndOfSegment);
        assert_eq!(asset.rule_to("other").sync, MusicSync::NextBeat);
        let bare =
            MusicAsset::from_def(&one_stem_def("b"), |_| Ok(crate::test_support::asset())).expect("valid");
        let default_rule = bare.rule_to("other");
        assert_eq!(default_rule.sync, MusicSync::NextBar);
        assert_eq!(default_rule.crossfade_secs, 0.0);
    }

    #[test]
    fn from_def_refuses_what_cannot_play() {
        let resolve = |_: &str| Ok(crate::test_support::asset());
        let zero_bars = MusicDef {
            segment_bars: Some(0),
            ..one_stem_def("t")
        };
        assert!(MusicAsset::from_def(&zero_bars, resolve).is_err());
        let stinger = MusicStingerDef {
            name: "hit".into(),
            sound: "hit.wav".into(),
            volume_db: 0.0,
            sync: MusicSync::NextBar,
        };
        let twice = MusicDef {
            stingers: vec![stinger.clone(), stinger],
            ..one_stem_def("t")
        };
        assert!(MusicAsset::from_def(&twice, resolve).is_err());
        for crossfade_secs in [-1.0, f64::NAN, f64::INFINITY] {
            let bad = MusicDef {
                transitions: vec![MusicTransitionDef {
                    to: "x".into(),
                    sync: MusicSync::NextBar,
                    crossfade_secs,
                    segment: None,
                }],
                ..one_stem_def("t")
            };
            assert!(MusicAsset::from_def(&bad, resolve).is_err(), "{crossfade_secs}");
        }
    }

    #[test]
    fn the_segment_comes_from_the_def_or_the_longest_stem() {
        let resolve = |_: &str| Ok(crate::test_support::tone_asset(440.0, 4.0, 0.1));
        let by_stem = MusicAsset::from_def(&one_stem_def("t"), resolve).expect("valid");
        assert_eq!(by_stem.segment_beats(), Some(8), "4 s at 120 bpm");
        let def = MusicDef {
            segment_bars: Some(3),
            ..one_stem_def("t")
        };
        let by_def = MusicAsset::from_def(&def, resolve).expect("valid");
        assert_eq!(by_def.segment_beats(), Some(12));
    }

    #[test]
    fn v1_text_reads_and_migrates_and_a_newer_version_is_refused() {
        let v1 = r#"( name: "a", bpm: 120.0, stems: [(name: "s", sound: "s.ogg")] )"#;
        assert_eq!(MusicDef::peek_version(v1).expect("peek"), 1);
        let read = MusicDef::from_ron(v1).expect("v1 reads");
        assert!(read.stingers.is_empty() && read.transitions.is_empty());
        assert_eq!(read.segment_bars, None);
        let migrated = MusicDef::migrate_ron(v1).expect("migrates");
        assert_eq!(MusicDef::peek_version(&migrated).expect("peek"), MUSIC_VERSION);
        let again = MusicDef::from_ron(&migrated).expect("v2 reads");
        assert_eq!(again.name, "a");
        assert_eq!(music_migration_sources(), vec![1]);
        let newer = format!(
            r#"( version: {}, name: "a", bpm: 120.0, stems: [] )"#,
            MUSIC_VERSION + 1
        );
        let error = MusicDef::from_ron(&newer)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("upgrade the engine"), "{error}");
    }

    #[test]
    fn the_sync_names_round_trip() {
        for sync in [
            MusicSync::Immediate,
            MusicSync::NextBeat,
            MusicSync::NextBar,
            MusicSync::EndOfSegment,
        ] {
            assert_eq!(MusicSync::from_name(sync.as_str()), Some(sync));
        }
        assert_eq!(MusicSync::from_name("sometime"), None);
    }

    #[test]
    fn bar_gate_fires_once_per_bar() {
        let mut gate = BarGate::default();
        // 4/4: boundaries at ticks 4, 8, 12...
        assert!(!gate.crossed_boundary(3, 4));
        assert!(gate.crossed_boundary(4, 4));
        assert!(!gate.crossed_boundary(5, 4));
        assert!(!gate.crossed_boundary(7, 4));
        assert!(gate.crossed_boundary(8, 4));
        // A long frame that jumps a whole bar still reports exactly one crossing call.
        assert!(gate.crossed_boundary(12, 4));
        assert!(!gate.crossed_boundary(12, 4), "same tick does not re-fire");
    }

    #[test]
    fn stem_offset_curve_interpolates() {
        let stem = MusicStem {
            name: "drums".into(),
            asset: crate::test_support::asset(),
            base_db: -3.0,
            intensity: vec![(0.0, -18.0), (1.0, 0.0)],
        };
        assert!((stem.offset_db_at(0.0) + 18.0).abs() < 1e-6);
        assert!((stem.offset_db_at(0.5) + 9.0).abs() < 1e-6);
        assert!(stem.offset_db_at(1.0).abs() < 1e-6);
        assert!(stem.offset_db_at(2.0).abs() < 1e-6, "clamped above");
    }

    #[test]
    fn from_def_rejects_bad_bpm() {
        for bpm in [0.0, -120.0, f64::NAN, f64::INFINITY, 1001.0] {
            let def = MusicDef {
                bpm,
                ..one_stem_def("t")
            };
            let Err(error) = MusicAsset::from_def(&def, |_| Ok(crate::test_support::asset())) else {
                panic!("bpm {bpm} must be rejected");
            };
            assert!(
                matches!(error, AudioError::Invalid(_)),
                "bpm {bpm} is a bad file, not a missing track: {error:?}"
            );
            let text = error.to_string();
            assert!(text.contains("must be above 0 and at most 1000"), "{text}");
            assert!(!text.contains("is not registered"), "{text}");
        }
        let ok = MusicDef {
            bpm: 1000.0,
            ..one_stem_def("t")
        };
        assert!(MusicAsset::from_def(&ok, |_| Ok(crate::test_support::asset())).is_ok());
    }

    #[test]
    fn a_120_bpm_grid_is_two_beats_after_one_second() {
        // The kira clock's tick at t = 1.0 is 1. The beat index is 2.
        let grid = BeatGrid::new(0.0, 120.0, 4, None);
        assert_eq!(grid.beat_secs, 0.5);
        assert_eq!(grid.beat_at(1.0), Some(2));
    }

    #[test]
    fn track_without_stems_reports_a_readable_error() {
        let def = MusicDef {
            stems: Vec::new(),
            ..one_stem_def("empty")
        };
        let error = MusicAsset::from_def(&def, |_| Ok(crate::test_support::asset()))
            .err()
            .map(|e| e.to_string());
        assert_eq!(error.as_deref(), Some("music track `empty` has no stems"));
    }
}
